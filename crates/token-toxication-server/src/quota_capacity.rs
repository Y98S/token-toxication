//! Observed quota capacity, not a billing conversion. A recovery starts a new
//! observation segment even when the upstream quota cycle did not change.
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    AppState,
    codex_subscription::{fetch_codex_account_quota, is_codex_subscription_auth},
    models::CodexAccountQuotaResponse,
};

pub(crate) const MAX_GAP_SECONDS: i64 = 300;
const RESET_TOLERANCE_SECONDS: i64 = 120;
const RECOVERY_POINTS: f64 = 1.0;
const MIN_ESTIMATE_POINTS: f64 = 10.0;
const SETTLE_SECONDS: i64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Sample {
    pub observer_started_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub used: f64,
    pub reset_at: Option<DateTime<Utc>>,
    pub duration: Option<i64>,
    pub plan: Option<String>,
    pub settled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Segment {
    pub limit_id: String,
    pub window: String,
    pub baseline: Sample,
    pub latest: Sample,
    pub pending: Option<Sample>,
    pub event: String,
    #[serde(default)]
    pub metering_incomplete: bool,
}

impl Segment {
    pub fn new(limit_id: String, window: String, sample: Sample, event: &str) -> Self {
        Self {
            limit_id,
            window,
            baseline: sample.clone(),
            latest: sample,
            pending: None,
            event: event.into(),
            metering_incomplete: false,
        }
    }

    /// Returns the completed segment when a new baseline is required. Overlapping
    /// queries are ignored: an old in-flight UI fetch cannot undo a newer sample.
    pub fn observe(&mut self, next: Sample) -> Option<Self> {
        if next.started_at <= self.latest.finished_at {
            return None;
        }
        let gap = (next.started_at - self.latest.finished_at).num_seconds() > MAX_GAP_SECONDS;
        let changed_plan = next.plan != self.latest.plan || next.duration != self.latest.duration;
        let reset = match (self.latest.reset_at, next.reset_at) {
            (Some(old), Some(new)) => {
                next.started_at >= old && (new - old).num_seconds() > RESET_TOLERANCE_SECONDS
            }
            _ => false,
        };
        let unknown_boundary = match (self.latest.reset_at, next.reset_at) {
            (Some(old), Some(new)) => {
                (new - old).num_seconds().abs() > RESET_TOLERANCE_SECONDS && !reset
            }
            (None, Some(_)) | (Some(_), None) => true,
            _ => false,
        };
        let event = if next.observer_started_at != self.latest.observer_started_at {
            Some("observer_restart")
        } else if gap {
            Some("sampling_gap")
        } else if changed_plan {
            Some("plan_changed")
        } else if reset {
            Some("period_reset")
        } else if unknown_boundary {
            Some("boundary_changed")
        } else {
            None
        };
        if let Some(event) = event {
            return Some(self.restart(next, event));
        }

        if let Some(candidate) = self.pending.clone() {
            // Need a later, non-overlapping observation. A stale cache can still
            // exist upstream; this confirms a sustained observation, not its cause.
            if next.started_at - candidate.finished_at < Duration::seconds(SETTLE_SECONDS) {
                return None;
            }
            if self.latest.used - next.used >= RECOVERY_POINTS {
                // Never backdate the baseline to the confirmation of the event.
                // The ambiguous interval is excluded by starting at this fresh sample.
                self.pending = None;
                return Some(self.restart(next, "quota_recovery"));
            }
            self.pending = None; // transient correction; retain the original baseline
        } else if self.latest.used - next.used >= RECOVERY_POINTS {
            self.pending = Some(next);
            return None;
        } else if next.used < self.latest.used {
            // Even a small correction must not silently contaminate the ratio.
            return Some(self.restart(next, "quota_correction"));
        }
        if self.metering_incomplete && next.settled {
            self.latest = next.clone();
            return Some(self.restart(next, "metering_gap"));
        }
        if !self.baseline.settled && next.settled {
            return Some(self.restart(next, "settled_baseline"));
        }
        self.latest = next;
        None
    }

    fn restart(&mut self, next: Sample, event: &str) -> Self {
        let replacement = Self::new(self.limit_id.clone(), self.window.clone(), next, event);
        std::mem::replace(self, replacement)
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapacityTokens {
    pub uncached_input: u64,
    pub cached_input: u64,
    pub output: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapacityEstimate {
    pub uncached_input: f64,
    pub cached_input: f64,
    pub output: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapacityWorkload {
    pub model: String,
    pub service_tier: String,
    pub attempts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapacityReport {
    pub limit_id: String,
    pub window: String,
    pub started_at: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    pub remaining_start: f64,
    pub remaining_end: f64,
    pub consumed_points: f64,
    pub window_seconds: Option<i64>,
    pub reset_at: Option<DateTime<Utc>>,
    /// ready / collecting / incomplete / settling / pending_recovery / stale /
    /// unassigned_pool. These describe the observation, never official capacity.
    pub status: String,
    pub event: String,
    pub historical: bool,
    pub attempts: u64,
    pub complete_attempts: u64,
    pub tokens: CapacityTokens,
    pub estimate: Option<CapacityEstimate>,
    pub workload: Vec<CapacityWorkload>,
}

#[allow(clippy::too_many_arguments)] // Explicit observation inputs keep eligibility checks together.
pub(crate) fn report(
    segment: &Segment,
    tokens: CapacityTokens,
    attempts: u64,
    complete: u64,
    boundary_complete: bool,
    workload: Vec<CapacityWorkload>,
    historical: bool,
    now: DateTime<Utc>,
) -> CapacityReport {
    let points = (segment.latest.used - segment.baseline.used).max(0.0);
    let status = if segment.limit_id != "codex" {
        "unassigned_pool"
    } else if segment.pending.is_some() {
        "pending_recovery"
    } else if !historical && (now - segment.latest.finished_at).num_seconds() > MAX_GAP_SECONDS {
        "stale"
    } else if complete < attempts || !boundary_complete {
        "incomplete"
    } else if !segment.baseline.settled || !segment.latest.settled {
        "settling"
    } else if points < MIN_ESTIMATE_POINTS || attempts == 0 {
        "collecting"
    } else {
        "ready"
    };
    let estimate = (status == "ready").then(|| CapacityEstimate {
        uncached_input: tokens.uncached_input as f64 * 100.0 / points,
        cached_input: tokens.cached_input as f64 * 100.0 / points,
        output: tokens.output as f64 * 100.0 / points,
    });
    CapacityReport {
        limit_id: segment.limit_id.clone(),
        window: segment.window.clone(),
        started_at: segment.baseline.finished_at,
        observed_at: segment.latest.finished_at,
        remaining_start: 100.0 - segment.baseline.used,
        remaining_end: 100.0 - segment.latest.used,
        consumed_points: points,
        window_seconds: segment.latest.duration,
        reset_at: segment.latest.reset_at,
        status: status.into(),
        event: segment.event.clone(),
        historical,
        attempts,
        complete_attempts: complete,
        tokens,
        estimate,
        workload,
    }
}

/// Capture stats without changing provider health or route eligibility.
pub(crate) async fn capture(
    state: &AppState,
    quota: &mut CodexAccountQuotaResponse,
    started: DateTime<Utc>,
    initially_idle: bool,
) -> Result<(), crate::error::AppError> {
    record_sample(state, quota, started, initially_idle).await?;
    quota.capacity = state
        .db
        .capacity_reports(&quota.account_id, Utc::now())
        .await?;
    Ok(())
}

async fn record_sample(
    state: &AppState,
    quota: &CodexAccountQuotaResponse,
    started: DateTime<Utc>,
    initially_idle: bool,
) -> Result<(), crate::error::AppError> {
    let finished = Utc::now();
    let idle = initially_idle && state.account_concurrency.load(&quota.account_id).running == 0;
    state
        .db
        .record_capacity_sample(quota, started, finished, idle, state.started_at)
        .await?;
    Ok(())
}

pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut shutdown = state.shutdown.subscribe();
        loop {
            tokio::select! {
                changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow_and_update() { break; } }
                _ = interval.tick() => {
                    tokio::select! {
                        _ = shutdown.changed() => break,
                        _ = sample_accounts(&state) => {}
                    }
                }
            }
        }
    })
}

async fn sample_accounts(state: &AppState) {
    let accounts = match state.db.list_provider_accounts().await {
        Ok(accounts) => accounts,
        Err(error) => {
            tracing::warn!(%error, "capacity account scan failed");
            return;
        }
    };
    // Dropping the set on shutdown aborts outstanding queries. Bound concurrency
    // so slow accounts do not serialize the entire scan or flood the upstream.
    let mut pending = tokio::task::JoinSet::new();
    for account in accounts {
        if !account.is_active || !is_codex_subscription_auth(&account.auth_mode) {
            continue;
        }
        if pending.len() >= 4 {
            let _ = pending.join_next().await;
        }
        let state = state.clone();
        pending.spawn(async move {
            let load = state.account_concurrency.load(&account.id);
            let recent = account.last_used_at.is_some_and(|at| Utc::now() - at < Duration::minutes(10));
            let baseline_due = state.db.capacity_idle_sample_due(&account.id, Utc::now()).await.unwrap_or(false);
            if !recent && load.running == 0 && !baseline_due { return; }
            let started = Utc::now();
            let sample = async {
                match fetch_codex_account_quota(&state.db, &state.http, &state.codex_device_oauth, &account.id).await {
                    Ok(quota) => if let Err(error) = record_sample(&state, &quota, started, load.running == 0).await {
                        tracing::warn!(account_id = %account.id, %error, "capacity sample persistence failed");
                    },
                    Err(error) => tracing::debug!(account_id = %account.id, %error, "capacity sample unavailable"),
                }
            };
            let _ = tokio::time::timeout(std::time::Duration::from_secs(20), sample).await;
        });
    }
    while pending.join_next().await.is_some() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(seconds: i64, used: f64) -> Sample {
        let epoch = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        Sample {
            observer_started_at: epoch,
            started_at: epoch + Duration::seconds(seconds),
            finished_at: epoch + Duration::seconds(seconds + 1),
            used,
            reset_at: Some(epoch + Duration::hours(5)),
            duration: Some(18000),
            plan: Some("plus".into()),
            settled: true,
        }
    }
    fn segment() -> Segment {
        Segment::new("codex".into(), "primary".into(), sample(0, 4.0), "baseline")
    }
    #[test]
    fn mixed_workload_extrapolates_without_counting_cache_twice() {
        let mut state = segment();
        state.observe(sample(60, 44.0));
        let r = report(
            &state,
            CapacityTokens {
                uncached_input: 200_000,
                cached_input: 800_000,
                output: 50_000,
            },
            10,
            10,
            true,
            vec![],
            false,
            sample(61, 0.0).started_at,
        );
        assert_eq!(r.status, "ready");
        let e = r.estimate.unwrap();
        assert_eq!(e.uncached_input, 500_000.0);
        assert_eq!(e.cached_input, 2_000_000.0);
        assert_eq!(e.output, 125_000.0);
    }
    #[test]
    fn extra_recovery_needs_confirmation_even_without_cycle_change_or_credit() {
        let mut state = segment();
        state.observe(sample(60, 80.0));
        assert!(state.observe(sample(120, 5.0)).is_none());
        assert!(state.pending.is_some());
        assert_eq!(
            report(
                &state,
                CapacityTokens::default(),
                1,
                1,
                true,
                vec![],
                false,
                sample(121, 0.0).started_at
            )
            .status,
            "pending_recovery"
        );
        let old = state.observe(sample(180, 10.0)).unwrap();
        assert_eq!(old.latest.used, 80.0);
        assert!(old.pending.is_none());
        assert_eq!(state.event, "quota_recovery");
        assert_eq!(state.baseline.used, 10.0);
        assert_eq!(state.baseline.started_at, sample(180, 10.0).started_at);
    }
    #[test]
    fn temporary_recovery_does_not_create_a_segment() {
        let mut state = segment();
        state.observe(sample(60, 80.0));
        state.observe(sample(120, 5.0));
        assert!(state.observe(sample(180, 81.0)).is_none());
        assert!(state.pending.is_none());
        assert_eq!(state.baseline.used, 4.0);
    }
    #[test]
    fn reset_need_not_be_observed_at_one_hundred_percent() {
        let mut state = segment();
        state.baseline = sample(17900, 80.0);
        state.latest = state.baseline.clone();
        let mut next = sample(18060, 2.0);
        next.reset_at = Some(next.started_at + Duration::hours(5));
        assert!(state.observe(next).is_some());
        assert_eq!(state.event, "period_reset");
        assert_eq!(state.latest.used, 2.0);
    }
    #[test]
    fn drift_and_overlapping_results_do_not_reset() {
        let mut state = segment();
        let mut next = sample(60, 10.0);
        next.reset_at = next.reset_at.map(|v| v + Duration::seconds(10));
        assert!(state.observe(next).is_none());
        assert!(state.observe(sample(20, 0.0)).is_none());
        assert_eq!(state.latest.used, 10.0);
        assert!(state.pending.is_none());
    }
    #[test]
    fn gap_plan_change_and_process_restart_start_fresh_baselines() {
        for (mut next, event) in [
            (sample(600, 20.0), "sampling_gap"),
            (sample(60, 20.0), "plan_changed"),
            (sample(60, 20.0), "observer_restart"),
        ] {
            if event == "plan_changed" {
                next.plan = Some("pro".into());
            }
            if event == "observer_restart" {
                next.observer_started_at = next.started_at;
            }
            let mut state = segment();
            assert!(state.observe(next).is_some());
            assert_eq!(state.event, event);
        }
    }
    #[test]
    fn insufficient_or_ambiguous_data_never_produces_capacity() {
        let mut state = segment();
        state.observe(sample(60, 8.0));
        let result = |state: &Segment, n, c, b| {
            report(
                state,
                CapacityTokens::default(),
                n,
                c,
                b,
                vec![],
                false,
                sample(61, 0.0).started_at,
            )
        };
        assert_eq!(result(&state, 1, 1, true).status, "collecting");
        state.latest.used = 44.0;
        assert!(result(&state, 2, 1, true).estimate.is_none());
        assert!(result(&state, 2, 2, false).estimate.is_none());
        state.latest.settled = false;
        assert_eq!(result(&state, 2, 2, true).status, "settling");
        state.limit_id = "special-model-limit".into();
        assert_eq!(result(&state, 2, 2, true).status, "unassigned_pool");
    }
    #[test]
    fn no_change_is_not_free_usage_and_stale_data_is_not_live_capacity() {
        let state = segment();
        let r = report(
            &state,
            CapacityTokens {
                output: 100,
                ..Default::default()
            },
            1,
            1,
            true,
            vec![],
            false,
            sample(600, 0.0).started_at,
        );
        assert_eq!(r.status, "stale");
        assert_eq!(r.tokens.output, 100);
        assert!(r.estimate.is_none());
    }
}
