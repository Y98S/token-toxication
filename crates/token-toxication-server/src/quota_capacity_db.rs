//! Persistence for quota observations. Called with one SQLite transaction per
//! response so concurrent manual/background fetches cannot reorder the baseline.
use super::Db;
use crate::{
    models::{CodexAccountQuotaResponse, RequestSummary},
    quota_capacity::{self, CapacityReport, CapacityTokens, CapacityWorkload, Sample, Segment},
};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;

pub(super) fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS quota_capacity_samples (
        account_id TEXT NOT NULL, started_at INTEGER NOT NULL, finished_at INTEGER NOT NULL,
        payload TEXT NOT NULL, PRIMARY KEY(account_id, started_at));
        CREATE TABLE IF NOT EXISTS quota_capacity_state (
        account_id TEXT NOT NULL, limit_id TEXT NOT NULL, window TEXT NOT NULL, payload TEXT NOT NULL,
        PRIMARY KEY(account_id, limit_id, window));
        CREATE TABLE IF NOT EXISTS quota_capacity_history (
        id INTEGER PRIMARY KEY, account_id TEXT NOT NULL, payload TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS idx_capacity_history_account ON quota_capacity_history(account_id, id DESC);
        CREATE INDEX IF NOT EXISTS idx_request_logs_account_time ON request_logs(provider_account_id, created_at);")
}

fn encode<T: serde::Serialize>(value: &T) -> Result<String, rusqlite::Error> {
    serde_json::to_string(value).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, rusqlite::Error> {
    serde_json::from_str(value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

impl Db {
    pub async fn capacity_idle_sample_due(
        &self,
        id: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock().await;
        conn.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM quota_capacity_samples WHERE account_id=?1 AND finished_at>?2)",
            params![id, (now - Duration::minutes(3)).timestamp_millis()],
            |row| row.get(0),
        )
    }

    pub async fn record_capacity_sample(
        &self,
        quota: &CodexAccountQuotaResponse,
        started: DateTime<Utc>,
        finished: DateTime<Utc>,
        idle: bool,
        observer_started_at: DateTime<Utc>,
    ) -> Result<(), rusqlite::Error> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let last: Option<i64> = tx.query_row(
            "SELECT MAX(finished_at) FROM quota_capacity_samples WHERE account_id=?1",
            [&quota.account_id],
            |row| row.get(0),
        )?;
        if last.is_some_and(|last| started.timestamp_millis() <= last) {
            return Ok(());
        }
        let recent: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM request_logs WHERE provider_account_id=?1 AND created_at>=?2)",
            params![quota.account_id, (started - Duration::seconds(30)).to_rfc3339()], |row| row.get(0))?;
        let settled = idle && !recent;
        // Only the sanitized quota payload is retained; no credentials or bodies.
        let mut payload = quota.clone();
        payload.capacity.clear();
        tx.execute(
            "INSERT INTO quota_capacity_samples VALUES (?1,?2,?3,?4)",
            params![
                quota.account_id,
                started.timestamp_millis(),
                finished.timestamp_millis(),
                encode(&payload)?
            ],
        )?;
        for limit in &quota.limits {
            for (window, value) in [
                ("primary", &limit.primary_window),
                ("secondary", &limit.secondary_window),
            ] {
                let Some(value) = value else { continue };
                let Some(used) = value
                    .used_percent
                    .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
                else {
                    continue;
                };
                let sample = Sample {
                    observer_started_at,
                    started_at: started,
                    finished_at: finished,
                    used,
                    reset_at: value.reset_at,
                    duration: value.limit_window_seconds,
                    plan: quota.plan_type.clone(),
                    settled,
                };
                let raw: Option<String> = tx.query_row("SELECT payload FROM quota_capacity_state WHERE account_id=?1 AND limit_id=?2 AND window=?3",
                    params![quota.account_id, limit.limit_id, window], |row| row.get(0)).optional()?;
                let mut state = if let Some(raw) = raw {
                    let mut previous: Segment = decode(&raw)?;
                    if limit.limit_id == "codex" {
                        previous.metering_incomplete |=
                            has_incomplete_metering(&tx, &quota.account_id, &previous, started)?;
                    }
                    if let Some(archived) = previous.observe(sample) {
                        tx.execute(
                            "INSERT INTO quota_capacity_history(account_id,payload) VALUES (?1,?2)",
                            params![quota.account_id, encode(&archived)?],
                        )?;
                    }
                    previous
                } else {
                    Segment::new(limit.limit_id.clone(), window.into(), sample, "baseline")
                };
                // Keep the serialization stable and explicit; data is trusted only
                // after a later, settled snapshot, never based on a query count.
                state.limit_id.clone_from(&limit.limit_id);
                tx.execute("INSERT INTO quota_capacity_state VALUES (?1,?2,?3,?4) ON CONFLICT(account_id,limit_id,window) DO UPDATE SET payload=excluded.payload",
                    params![quota.account_id, limit.limit_id, window, encode(&state)?])?;
            }
        }
        tx.execute(
            "DELETE FROM quota_capacity_samples WHERE account_id=?1 AND finished_at<?2",
            params![
                quota.account_id,
                (finished - Duration::days(90)).timestamp_millis()
            ],
        )?;
        tx.execute("DELETE FROM quota_capacity_history WHERE account_id=?1 AND id NOT IN (SELECT id FROM quota_capacity_history WHERE account_id=?1 ORDER BY id DESC LIMIT 40)", [&quota.account_id])?;
        tx.commit()
    }

    pub async fn capacity_reports(
        &self,
        id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<CapacityReport>, rusqlite::Error> {
        let conn = self.conn.lock().await;
        let mut states = conn.prepare("SELECT payload,0 FROM quota_capacity_state WHERE account_id=?1 ORDER BY limit_id,window")?;
        let mut raw: Vec<(String, bool)> = states
            .query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut history = conn.prepare("SELECT payload,1 FROM quota_capacity_history WHERE account_id=?1 ORDER BY id DESC LIMIT 6")?;
        raw.extend(
            history
                .query_map([id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?,
        );
        raw.into_iter()
            .map(|(raw, historical)| {
                let segment: Segment = decode(&raw)?;
                summarize(&conn, id, &segment, historical, now)
            })
            .collect()
    }
}

// Only examine newly completed attempts; do not rescan the full segment on each
// background tick. Keep unknown intervals, then resume from a settled baseline.
fn has_incomplete_metering(
    conn: &Connection,
    id: &str,
    state: &Segment,
    until: DateTime<Utc>,
) -> Result<bool, rusqlite::Error> {
    let since = state.latest.started_at.max(state.baseline.finished_at);
    let mut stmt = conn.prepare(
        "SELECT request_summary,input_tokens,cached_input_tokens FROM request_logs
        WHERE provider_account_id=?1 AND created_at>?2 AND created_at<=?3",
    )?;
    let rows = stmt.query_map(params![id, since.to_rfc3339(), until.to_rfc3339()], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, u64>(1)?,
            row.get::<_, u64>(2)?,
        ))
    })?;
    for row in rows {
        let (raw, input, cached) = row?;
        let summary = raw.and_then(|raw| serde_json::from_str::<RequestSummary>(&raw).ok());
        if cached > input
            || !summary.is_some_and(|s| {
                s.usage_complete == Some(true)
                    && s.attempt_started_at
                        .is_some_and(|at| at >= state.baseline.finished_at)
            })
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn summarize(
    conn: &Connection,
    id: &str,
    state: &Segment,
    historical: bool,
    now: DateTime<Utc>,
) -> Result<CapacityReport, rusqlite::Error> {
    let mut tokens = CapacityTokens::default();
    let (mut attempts, mut complete) = (0, 0);
    let mut boundary_complete = true;
    let mut workloads = BTreeMap::<(String, String), u64>::new();
    if state.limit_id == "codex" {
        let mut stmt = conn.prepare("SELECT input_tokens,cached_input_tokens,output_tokens,request_summary,COALESCE(upstream_model,model,'unknown')
            FROM request_logs WHERE provider_account_id=?1 AND created_at>?2 AND created_at<=?3")?;
        let rows = stmt.query_map(
            params![
                id,
                state.baseline.finished_at.to_rfc3339(),
                state.latest.started_at.to_rfc3339()
            ],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )?;
        for row in rows {
            let (input, cached, output, raw, model) = row?;
            let summary = raw.and_then(|s| serde_json::from_str::<RequestSummary>(&s).ok());
            attempts += 1;
            tokens.uncached_input = tokens
                .uncached_input
                .saturating_add(input.saturating_sub(cached));
            tokens.cached_input = tokens.cached_input.saturating_add(cached);
            tokens.output = tokens.output.saturating_add(output);
            if summary
                .as_ref()
                .is_some_and(|s| s.usage_complete == Some(true))
                && cached <= input
            {
                complete += 1;
            }
            boundary_complete &= summary
                .as_ref()
                .and_then(|s| s.attempt_started_at)
                .is_some_and(|at| at >= state.baseline.finished_at);
            let tier = summary
                .and_then(|s| s.service_tier)
                .unwrap_or_else(|| "unknown".into());
            *workloads.entry((model, tier)).or_default() += 1;
        }
    }
    let workload = workloads
        .into_iter()
        .map(|((model, service_tier), attempts)| CapacityWorkload {
            model,
            service_tier,
            attempts,
        })
        .collect();
    Ok(quota_capacity::report(
        state,
        tokens,
        attempts,
        complete,
        boundary_complete,
        workload,
        historical,
        now,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).unwrap()
    }
    fn quota(used: f64) -> CodexAccountQuotaResponse {
        serde_json::from_value(json!({"accountId":"account-a","authMode":"codex-oauth","endpoint":"https://example.test/usage","planType":"plus",
            "limits":[{"limitId":"codex","displayName":"Codex","primaryWindow":{"usedPercent":used,"limitWindowSeconds":18000,"resetAt":at(18000)}}]})).unwrap()
    }
    async fn db() -> Db {
        Db::open(std::path::Path::new(":memory:")).await.unwrap()
    }
    async fn log(db: &Db, id: &str, complete: bool, start: i64, end: i64, cached: u64) {
        let summary = json!({"usageComplete":complete,"attemptStartedAt":at(start),"serviceTier":"priority","topLevelKeys":[],"bodyBytes":0,"stream":true,"strippedParams":[]});
        db.conn.lock().await.execute("INSERT INTO request_logs(id,api_key_id,provider_account_id,method,path,status_code,latency_ms,input_tokens,cached_input_tokens,output_tokens,created_at,request_summary,model,logical_request_id) VALUES (?1,'key','account-a','POST','/responses',500,1000,100,?2,20,?3,?4,'model-a','same-logical-request')",
            params![id,cached,at(end).to_rfc3339(),summary.to_string()]).unwrap();
    }
    async fn sample(db: &Db, seconds: i64, used: f64) {
        db.record_capacity_sample(&quota(used), at(seconds), at(seconds + 1), true, at(0))
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn stores_samples_and_sums_failed_and_retried_attempts_once_each() {
        let db = db().await;
        sample(&db, 0, 0.0).await;
        log(&db, "a", true, 10, 30, 80).await;
        log(&db, "b", true, 40, 60, 90).await;
        sample(&db, 180, 20.0).await;
        let r = db
            .capacity_reports("account-a", at(182))
            .await
            .unwrap()
            .remove(0);
        assert_eq!(r.attempts, 2);
        assert_eq!(r.tokens.uncached_input, 30);
        assert_eq!(r.tokens.cached_input, 170);
        assert_eq!(r.tokens.output, 40);
        assert_eq!(r.status, "ready");
        assert_eq!(r.estimate.unwrap().output, 200.0);
        assert_eq!(r.workload[0].service_tier, "priority");
        assert!(
            db.capacity_reports("different-account", at(182))
                .await
                .unwrap()
                .is_empty()
        );
    }
    #[tokio::test]
    async fn unknown_usage_bad_cache_and_cross_boundary_requests_disable_estimates() {
        for (complete, start, cached) in [(false, 10, 80), (true, -10, 80), (true, 10, 200)] {
            let db = db().await;
            sample(&db, 0, 0.0).await;
            log(&db, "a", complete, start, 30, cached).await;
            sample(&db, 180, 20.0).await;
            let reports = db.capacity_reports("account-a", at(182)).await.unwrap();
            assert_eq!(reports[0].event, "metering_gap");
            assert_eq!(reports[0].status, "collecting");
            let old = reports.iter().find(|r| r.historical).unwrap();
            assert_eq!(old.status, "incomplete");
            assert!(old.estimate.is_none());
            log(&db, "good", true, 200, 220, 80).await;
            sample(&db, 300, 40.0).await;
            let recovered = db
                .capacity_reports("account-a", at(302))
                .await
                .unwrap()
                .remove(0);
            assert_eq!(recovered.status, "ready");
            assert_eq!(recovered.attempts, 1);
        }
    }
    #[tokio::test]
    async fn rejects_late_responses_and_retains_recovery_history() {
        let db = db().await;
        sample(&db, 0, 0.0).await;
        sample(&db, 60, 80.0).await;
        sample(&db, 20, 0.0).await;
        let r = db
            .capacity_reports("account-a", at(62))
            .await
            .unwrap()
            .remove(0);
        assert_eq!(r.remaining_end, 20.0);
        sample(&db, 120, 5.0).await;
        sample(&db, 180, 10.0).await;
        let reports = db.capacity_reports("account-a", at(182)).await.unwrap();
        assert_eq!(reports[0].event, "quota_recovery");
        assert_eq!(reports[0].remaining_start, 90.0);
        assert!(reports.iter().any(|r| r.historical));
        assert!(
            reports
                .iter()
                .filter(|r| r.historical)
                .all(|r| r.status != "pending_recovery")
        );
        let count: i64 = db
            .conn
            .lock()
            .await
            .query_row("SELECT COUNT(*) FROM quota_capacity_samples", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 4);
    }
    #[tokio::test]
    async fn special_pool_does_not_claim_whole_account_tokens() {
        let db = db().await;
        let mut q = quota(0.0);
        q.limits[0].limit_id = "special".into();
        db.record_capacity_sample(&q, at(0), at(1), true, at(0))
            .await
            .unwrap();
        log(&db, "a", true, 10, 30, 80).await;
        q.limits[0].primary_window.as_mut().unwrap().used_percent = Some(20.0);
        db.record_capacity_sample(&q, at(180), at(181), true, at(0))
            .await
            .unwrap();
        let r = db
            .capacity_reports("account-a", at(182))
            .await
            .unwrap()
            .remove(0);
        assert_eq!(r.status, "unassigned_pool");
        assert_eq!(r.attempts, 0);
        assert!(r.estimate.is_none());
    }
    #[tokio::test]
    async fn idle_baselines_preserve_the_first_busy_session() {
        let db = db().await;
        assert!(
            db.capacity_idle_sample_due("account-a", at(0))
                .await
                .unwrap()
        );
        sample(&db, 0, 0.0).await;
        assert!(
            !db.capacity_idle_sample_due("account-a", at(120))
                .await
                .unwrap()
        );
        assert!(
            db.capacity_idle_sample_due("account-a", at(181))
                .await
                .unwrap()
        );
        for second in (180..=3600).step_by(180) {
            sample(&db, second, 0.0).await;
        }
        // An uninterrupted session after an idle hour retains its earlier settled baseline.
        log(&db, "busy", true, 3610, 3690, 80).await;
        db.record_capacity_sample(&quota(10.0), at(3660), at(3661), false, at(0))
            .await
            .unwrap();
        sample(&db, 3780, 20.0).await;
        let reports = db.capacity_reports("account-a", at(3782)).await.unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].status, "ready");
        assert_eq!(reports[0].attempts, 1);
        assert_eq!(reports[0].started_at, at(1));
    }
    #[tokio::test]
    async fn failure_after_the_archived_endpoint_does_not_invalidate_history() {
        let db = db().await;
        sample(&db, 0, 0.0).await;
        log(&db, "good", true, 10, 20, 80).await;
        sample(&db, 60, 20.0).await;
        log(&db, "unknown", false, 80, 90, 0).await;
        let mut reset = quota(0.0);
        reset.limits[0].primary_window.as_mut().unwrap().reset_at = Some(at(36000));
        db.record_capacity_sample(&reset, at(180), at(181), true, at(0))
            .await
            .unwrap();
        let reports = db.capacity_reports("account-a", at(182)).await.unwrap();
        let old = reports.iter().find(|r| r.historical).unwrap();
        assert_eq!(old.status, "ready");
        assert_eq!(old.attempts, 1);
        assert!(old.estimate.is_some());
    }
}
