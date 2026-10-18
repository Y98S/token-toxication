import { t } from "@lingui/core/macro";
import { Trans } from "@lingui/react/macro";
import type { CapacityReport } from "../types";
import { Badge } from "@/components/ui/badge";
import { formatCodexWindow, formatDate, formatQuotaPercent } from "./helpers";

function statusText(status: string, historical: boolean) {
  if (historical && ["collecting", "settling", "pending_recovery", "stale"].includes(status)) {
    return t`This segment has ended without enough reliable data for an estimate.`;
  }
  switch (status) {
    case "ready":
      return t`Estimate available`;
    case "pending_recovery":
      return t`Confirming quota recovery`;
    case "incomplete":
      return t`Incomplete token metering or a request crossed the baseline. Estimate withheld.`;
    case "settling":
      return t`Waiting for requests to finish and quota to settle.`;
    case "stale":
      return t`Quota observations are out of date. Waiting for a successful refresh.`;
    case "unassigned_pool":
      return t`Tokens cannot be attributed to this quota pool.`;
    default:
      return t`Collecting data. At least 10 percentage points of consumption are needed.`;
  }
}

function eventText(event: string) {
  switch (event) {
    case "quota_recovery":
      return t`Quota recovery confirmed`;
    case "period_reset":
      return t`Quota window reset`;
    case "sampling_gap":
      return t`Sampling gap`;
    case "observer_restart":
      return t`Server restarted`;
    case "plan_changed":
      return t`Plan or window changed`;
    case "boundary_changed":
      return t`Reset boundary changed`;
    case "quota_correction":
      return t`Quota correction`;
    case "metering_gap":
      return t`New baseline after incomplete metering`;
    case "settled_baseline":
      return t`Settled baseline`;
    default:
      return t`First observation`;
  }
}

function Observation({ report }: { report: CapacityReport }) {
  const unassigned = report.status === "unassigned_pool";
  const consumedPoints = Number(report.consumedPoints.toFixed(2)).toLocaleString();
  const labels = [t`Uncached input`, t`Cached input`, t`Output`];
  const actual = [report.tokens.uncachedInput, report.tokens.cachedInput, report.tokens.output];
  const estimated = report.estimate && [
    report.estimate.uncachedInput,
    report.estimate.cachedInput,
    report.estimate.output,
  ];
  const number = (value: number) => Math.round(value).toLocaleString();
  return (
    <article className="min-w-0 rounded-lg border p-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h4 className="text-sm font-medium">
          {formatCodexWindow(report.windowSeconds)} ·{" "}
          {report.window === "primary" ? t`Primary` : t`Secondary`}
        </h4>
        <Badge variant="secondary">{report.limitId}</Badge>
      </div>
      <div className="mt-3 flex flex-wrap items-baseline gap-x-3 gap-y-1 tabular-nums">
        <span className="text-xl font-semibold">
          {formatQuotaPercent(report.remainingStart)} → {formatQuotaPercent(report.remainingEnd)}
        </span>
        <span className="text-xs text-muted-foreground">
          <Trans>Remaining · consumed {consumedPoints} percentage points</Trans>
        </span>
      </div>
      <p className="mt-1 text-xs text-muted-foreground">
        {formatDate(report.startedAt)} — {formatDate(report.observedAt)}
      </p>
      <p className="mt-4 text-xs text-muted-foreground">
        {report.status === "incomplete" || report.completeAttempts < report.attempts
          ? t`Recorded tokens (incomplete observation)`
          : t`Recorded tokens`}
      </p>
      <div className="mt-2 grid grid-cols-3 gap-2">
        {labels.map((label, index) => (
          <div key={label} className="min-w-0">
            <div className="text-xs text-muted-foreground">{label}</div>
            <div className="mt-1 break-all font-mono text-sm tabular-nums">
              {unassigned ? "—" : number(actual[index])}
            </div>
          </div>
        ))}
      </div>
      <div className="mt-4 rounded-md bg-muted/50 p-3">
        <p className="text-sm font-medium">
          <Trans>Estimated tokens for 100% quota</Trans>
        </p>
        {estimated ? (
          <>
            <p className="mt-1 text-xs text-muted-foreground">
              <Trans>At the same model, tier and token mix</Trans>
            </p>
            <div className="mt-2 space-y-2">
              {estimated.map((value, index) => (
                <div
                  key={labels[index]}
                  className="flex min-w-0 flex-wrap items-baseline justify-between gap-x-3 gap-y-1"
                >
                  <span className="text-xs text-muted-foreground">{labels[index]}</span>
                  <span className="break-all font-mono text-sm tabular-nums">
                    ≈ {number(value)}
                  </span>
                </div>
              ))}
            </div>
          </>
        ) : (
          <p className="mt-1 text-xs text-muted-foreground">
            {statusText(report.status, report.historical)}
          </p>
        )}
      </div>
      {!unassigned && (
        <p className="mt-3 text-xs text-muted-foreground">
          <Trans>
            Complete metering: {report.completeAttempts} / {report.attempts} attempts
          </Trans>
        </p>
      )}
      <details className="mt-2 text-xs text-muted-foreground">
        <summary className="cursor-pointer">
          <Trans>Observation details</Trans>
        </summary>
        <p className="mt-2">{eventText(report.event)}</p>
        {report.workload.length > 0 && (
          <ul className="mt-2 space-y-1">
            {report.workload.map((item) => (
              <li key={`${item.model}:${item.serviceTier}`} className="break-words">
                {item.model} · {item.serviceTier} · {item.attempts}
              </li>
            ))}
          </ul>
        )}
        <p className="mt-2">
          <Trans>Tier is the requested service tier. Retries count as separate attempts.</Trans>
        </p>
      </details>
    </article>
  );
}

export function QuotaCapacity({ reports }: { reports: readonly CapacityReport[] }) {
  const current = reports.filter((r) => !r.historical);
  const history = reports.filter((r) => r.historical);
  return (
    <section className="min-w-0 space-y-3" aria-label={t`Observed token capacity`}>
      <div>
        <h3 className="font-medium">
          <Trans>Observed token capacity</Trans>
        </h3>
        <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
          <Trans>
            Collected automatically from this proxy. Usage elsewhere and delayed upstream quota
            updates can affect estimates. This is an observed workload estimate, not an official
            token allowance.
          </Trans>
        </p>
      </div>
      <p className="text-xs leading-relaxed text-muted-foreground">
        <Trans>
          Quota windows are measured independently; do not add their token totals. Reopen this
          dialog to see the latest data.
        </Trans>
      </p>
      {current.length ? (
        <div className="grid gap-3 sm:grid-cols-2">
          {current.map((r) => (
            <Observation key={`${r.limitId}:${r.window}`} report={r} />
          ))}
        </div>
      ) : (
        <p className="text-sm text-muted-foreground">
          <Trans>Waiting for the first quota observation.</Trans>
        </p>
      )}
      {history.length > 0 && (
        <details>
          <summary className="cursor-pointer text-sm text-muted-foreground">
            <Trans>Recent observation segments</Trans>
          </summary>
          <div className="mt-3 grid gap-3 sm:grid-cols-2">
            {history.map((r, index) => (
              <Observation key={`${r.startedAt}:${index}`} report={r} />
            ))}
          </div>
        </details>
      )}
    </section>
  );
}
