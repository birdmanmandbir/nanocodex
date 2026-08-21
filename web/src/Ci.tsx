import { useCallback, useEffect, useRef, useState } from "react";
import {
  ciApi,
  type CiProgressStep,
  type CiRun,
  type CiRunDetail,
  type CiRuns,
  type CiStepSummary,
} from "./ciApi";
import "./ci.css";

const gates = [
  ["cargo-dependencies", "Cargo dependencies", "vendor and registry cache"],
  ["rust-build-cache", "Rust build cache", "shared compilation root"],
  ["stable-workspace-tests", "Stable workspace", "complete Rust test suite"],
  ["msrv-workspace-tests", "MSRV 1.97", "minimum supported Rust"],
  ["quality", "Quality", "rustfmt and warnings-denied Clippy"],
  ["dependency-policy", "Dependency policy", "crate boundaries and Cargo deny"],
  ["static-vm-guest", "Static VM guest", "cross-target guest build"],
  ["python-3-11", "Python 3.11", "bindings and consumer tests"],
  ["python-3-14", "Python 3.14", "forward-runtime coverage"],
  ["all-dependencies", "Dependency fan-in", "immutable JavaScript dependency snapshot"],
  ["node-and-browser-bindings", "Node and browser", "WASM and package suites"],
  ["website", "Website", "typecheck, tests, and production build"],
] as const;

type Overview = CiRuns & { latest: CiRunDetail | null };
type GateView = {
  status: "pending" | "running" | "success" | "failure" | "terminated";
  durationMs?: number;
  cacheHit?: boolean;
  exitCode?: number;
  message?: string;
  hasEvidence: boolean;
};

function RefreshIcon() {
  return (
    <svg
      aria-hidden="true"
      fill="none"
      viewBox="0 0 24 24"
      stroke="currentColor"
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth="2"
    >
      <path d="M21 12a9 9 0 0 0-15.17-6.55L3 8" />
      <path d="M3 3v5h5" />
      <path d="M3 12a9 9 0 0 0 15.17 6.55L21 16" />
      <path d="M16 16h5v5" />
    </svg>
  );
}

export function Ci() {
  const [overview, setOverview] = useState<Overview | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const inFlight = useRef<Promise<void> | null>(null);

  const refresh = useCallback((signal?: AbortSignal): Promise<void> => {
    if (inFlight.current) return inFlight.current;
    const operation = (async () => {
      try {
        const runs = await ciApi.runs(signal);
        const latest = runs.runs[0]
          ? await ciApi.run(runs.runs[0].head, signal)
          : null;
        setOverview({ ...runs, latest });
        setError(null);
      } catch (cause) {
        if (signal?.aborted) return;
        setError(cause instanceof Error ? cause : new Error(String(cause)));
      }
    })();
    const tracked = operation.finally(() => {
      if (inFlight.current === tracked) inFlight.current = null;
    });
    inFlight.current = tracked;
    return tracked;
  }, []);

  useEffect(() => {
    const controller = new AbortController();
    let stopped = false;
    let timer: number | undefined;
    const poll = async () => {
      await refresh(controller.signal);
      if (!stopped) timer = window.setTimeout(() => void poll(), 5_000);
    };
    void poll();
    return () => {
      stopped = true;
      controller.abort();
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [refresh]);

  if (!overview && !error) return null;

  const latest = overview?.latest ?? null;
  const gateViews = gates.map(([slug]) => gateView(latest, slug));
  const completedGates = gateViews.filter(({ status }) =>
    status !== "pending" && status !== "running"
  ).length;
  const result = validResult(latest?.result);

  return (
    <div className="ci-dashboard">
      <header className="ci-page-head" aria-labelledby="ci-title">
        <p className="eyebrow">Cloudflare-native CI</p>
        <h1 id="ci-title">Continuous integration without GitHub.</h1>
        <p>
          One immutable source publication enters Workers and Workflows. Cloudflare
          Containers execute the gates; R2 retains caches, logs, and deployable artifacts.
        </p>
        <p className="ci-architecture">
          workers / workflows / containers / durable objects / r2
        </p>
      </header>

      {overview ? (
        <dl className="ci-summary" aria-label="CI summary">
          <Metric
            label={`Retained · target ${overview.retentionLimit}`}
            value={String(overview.retainedCount)}
          />
          <Metric
            label="Result"
            value={resultLabel(latest?.result)}
            tone={resultTone(latest?.result)}
          />
          <Metric label="Workflow" value={latest?.workflow.status ?? "idle"} />
          <Metric label="Gates" value={`${completedGates} / ${gates.length}`} />
        </dl>
      ) : null}

      {error ? (
        <section className="ci-error" role="alert">
          <div>
            <strong>{overview ? "CI refresh failed." : "CI state is unavailable."}</strong>
            <p>{error.message}</p>
          </div>
          <button type="button" onClick={() => void refresh()}>
            <RefreshIcon /> Retry
          </button>
        </section>
      ) : null}

      {overview ? (
        <>
          {result?.status === "failure" ? (
            <section className="ci-failure" role="alert" aria-labelledby="ci-failure-title">
              <p className="eyebrow">Failed gate</p>
              <h2 id="ci-failure-title">{result.failure.name}</h2>
              <p>{boundedMessage(result.failure.message)}</p>
            </section>
          ) : null}

          <section className="ci-pipeline" aria-labelledby="pipeline-title">
            <header>
              <div>
                <p className="eyebrow">Validation</p>
                <h2 id="pipeline-title">Twelve gates. One source.</h2>
              </div>
              {latest ? (
                <a
                  className="ci-head"
                  href={`/api/ci/runs/${encodeURIComponent(latest.head)}`}
                  target="_blank"
                  rel="noreferrer"
                  title={latest.head}
                >
                  {latest.head.slice(0, 12)}
                </a>
              ) : null}
            </header>
            <div className="ci-gates">
              {gates.map(([slug, name, description], index) => (
                <Gate
                  key={slug}
                  index={index}
                  slug={slug}
                  name={name}
                  description={description}
                  head={latest?.head}
                  view={gateViews[index]!}
                />
              ))}
            </div>
          </section>

          {result?.status === "success" && result.artifacts.length > 0 ? (
            <section className="ci-artifacts" aria-labelledby="artifacts-title">
              <header>
                <p className="eyebrow">Retained output</p>
                <h2 id="artifacts-title">Artifacts</h2>
              </header>
              {result.artifacts.map((artifact) => {
                const filename = artifact.key.split("/").at(-1) ?? "artifact";
                return (
                  <a
                    href={`/api/ci/runs/${encodeURIComponent(latest!.head)}/artifacts/${filename}`}
                    key={artifact.key}
                  >
                    <span>{filename}</span>
                    <span>{formatBytes(artifact.size)} · {artifact.sha256.slice(0, 12)}</span>
                  </a>
                );
              })}
            </section>
          ) : null}

          <section className="ci-runs" aria-labelledby="runs-title">
            <header>
              <div>
                <p className="eyebrow">Immutable publications</p>
                <h2 id="runs-title">Recent runs</h2>
              </div>
              <button type="button" onClick={() => void refresh()} aria-label="Refresh CI runs">
                <RefreshIcon /> Refresh
              </button>
            </header>
            {overview.runs.length === 0 ? (
              <div className="ci-empty">
                <strong>Control plane ready.</strong>
                <p>Publish a local source revision to start the first run.</p>
              </div>
            ) : null}
            {overview.runs.map((run, index) => (
              <RunRow
                key={run.head}
                run={run}
                result={index === 0 ? latest?.result : undefined}
              />
            ))}
          </section>
        </>
      ) : null}
    </div>
  );
}

function Gate({
  index,
  slug,
  name,
  description,
  head,
  view,
}: {
  index: number;
  slug: string;
  name: string;
  description: string;
  head?: string;
  view: GateView;
}) {
  return (
    <article className={`ci-gate is-${view.status}`}>
      <span className="ci-gate-index">{String(index + 1).padStart(2, "0")}</span>
      <div className="ci-gate-copy">
        <h3>{name}</h3>
        <p>{description}</p>
        {view.message ? <p className="ci-gate-message">{boundedMessage(view.message)}</p> : null}
      </div>
      <div className="ci-gate-evidence">
        <GateState view={view} />
        {head && view.hasEvidence ? (
          <span className="ci-log-links">
            <a
              href={`/api/ci/runs/${encodeURIComponent(head)}/steps/${slug}/stdout.log`}
              target="_blank"
              rel="noreferrer"
            >
              stdout
            </a>
            <a
              href={`/api/ci/runs/${encodeURIComponent(head)}/steps/${slug}/stderr.log`}
              target="_blank"
              rel="noreferrer"
            >
              stderr
            </a>
          </span>
        ) : null}
      </div>
    </article>
  );
}

function GateState({ view }: { view: GateView }) {
  const label = view.status === "pending"
    ? "not started"
    : view.status === "running"
    ? "running"
    : view.status === "terminated"
    ? "terminated"
    : view.status === "failure"
    ? "failed"
    : view.cacheHit
    ? "cached"
    : view.durationMs == null
    ? "passed"
    : formatDuration(view.durationMs);
  return (
    <span className={`ci-gate-state is-${view.status}`}>
      <i aria-hidden="true" />
      {label}
    </span>
  );
}

function RunRow({
  run,
  result,
}: {
  run: CiRun;
  result: CiRunDetail["result"] | undefined;
}) {
  return (
    <article className="ci-run">
      <a
        href={`/api/ci/runs/${encodeURIComponent(run.head)}`}
        target="_blank"
        rel="noreferrer"
        title={run.head}
      >
        {run.head.slice(0, 12)}
      </a>
      <span>{result ? resultLabel(result) : run.state}</span>
      <span>{run.attempts} {run.attempts === 1 ? "dispatch" : "dispatches"}</span>
      <time dateTime={run.publishedAt}>{formatDate(run.publishedAt)}</time>
    </article>
  );
}

function Metric({ label, value, tone }: { label: string; value: string; tone?: string }) {
  return (
    <div className={tone ? `ci-metric is-${tone}` : "ci-metric"}>
      <dt>{label}</dt>
      <dd>{value}</dd>
    </div>
  );
}

function gateView(detail: CiRunDetail | null, slug: string): GateView {
  const result = validResult(detail?.result);
  if (result && result.status !== "running") {
    const completed = result.steps.find((step) => stepSlug(step) === slug);
    if (completed) return summaryView(completed);
  }
  const progress = detail?.progress;
  if (progress && "steps" in progress) {
    const step = progress.steps.find((candidate) => candidate.slug === slug);
    if (step) return progressView(step);
  }
  return { status: "pending", hasEvidence: false };
}

function summaryView(step: CiStepSummary): GateView {
  return {
    status: step.exitCode === 0 ? "success" : "failure",
    durationMs: step.durationMs,
    cacheHit: step.cacheHit,
    exitCode: step.exitCode,
    hasEvidence: true,
  };
}

function progressView(step: CiProgressStep): GateView {
  return {
    status: step.status,
    durationMs: step.durationMs,
    cacheHit: step.cacheHit,
    exitCode: step.exitCode,
    message: step.message,
    hasEvidence: step.exitCode != null,
  };
}

function stepSlug(step: Pick<CiStepSummary, "name">) {
  return step.name
    .toLowerCase()
    .replaceAll(/[^a-z0-9]+/g, "-")
    .replace(/^-|-$/g, "");
}

function validResult(result: CiRunDetail["result"] | undefined) {
  return result && "status" in result ? result : null;
}

function resultLabel(result: CiRunDetail["result"] | undefined) {
  if (!result) return "no run";
  return "status" in result ? result.status : "invalid";
}

function resultTone(result: CiRunDetail["result"] | undefined) {
  if (!result || !("status" in result)) return undefined;
  return result.status === "success"
    ? "success"
    : result.status === "failure"
    ? "failure"
    : result.status === "terminated"
    ? "terminated"
    : "active";
}

function boundedMessage(message: string) {
  return message.length > 1_000 ? `${message.slice(0, 997)}…` : message;
}

function formatDate(value: string) {
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(value));
}

function formatDuration(milliseconds: number) {
  if (milliseconds < 1_000) return `${milliseconds}ms`;
  const seconds = Math.round(milliseconds / 1_000);
  return seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
}

function formatBytes(bytes: number) {
  if (bytes < 1_024) return `${bytes} B`;
  if (bytes < 1_024 * 1_024) return `${(bytes / 1_024).toFixed(1)} KiB`;
  return `${(bytes / (1_024 * 1_024)).toFixed(1)} MiB`;
}
