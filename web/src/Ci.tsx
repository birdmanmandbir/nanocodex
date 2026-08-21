import { Check, CircleDot, Cloud, Database, GitBranch, RefreshCw, X } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { ciApi, type CiRun, type CiRunDetail } from "./ciApi";
import "./ci.css";

const gates = [
  ["cargo-dependencies", "Cargo dependencies", "vendor + registry cache"],
  ["rust-build-cache", "Rust build cache", "shared compilation root"],
  ["stable-workspace-tests", "Stable workspace", "complete Rust test suite"],
  ["msrv-workspace-tests", "MSRV 1.97", "minimum supported Rust"],
  ["quality", "Quality", "fmt + warnings-denied Clippy"],
  ["dependency-policy", "Dependency policy", "boundaries + Cargo deny"],
  ["static-vm-guest", "Static VM guest", "cross-target guest build"],
  ["python-3-11", "Python 3.11", "bindings and consumer tests"],
  ["python-3-14", "Python 3.14", "forward-runtime coverage"],
  ["all-dependencies", "Dependency fan-in", "immutable dependency snapshot"],
  ["node-and-browser-bindings", "Node + browser", "WASM and package suites"],
  ["website", "Website", "typecheck, tests, production build"],
] as const;

type Overview = { runs: CiRun[]; latest: CiRunDetail | null };

export function Ci() {
  const [overview, setOverview] = useState<Overview | null>(null);
  const [error, setError] = useState<Error | null>(null);

  const refresh = useCallback(async (signal?: AbortSignal) => {
    try {
      const { runs } = await ciApi.runs(signal);
      const latest = runs[0] ? await ciApi.run(runs[0].head, signal) : null;
      setOverview({ runs, latest });
      setError(null);
    } catch (cause) {
      if (signal?.aborted) return;
      setError(cause instanceof Error ? cause : new Error(String(cause)));
    }
  }, []);

  useEffect(() => {
    const controller = new AbortController();
    void refresh(controller.signal);
    const interval = window.setInterval(() => void refresh(controller.signal), 5_000);
    return () => {
      controller.abort();
      window.clearInterval(interval);
    };
  }, [refresh]);

  const latest = overview?.latest ?? null;
  return (
    <div className="ci-dashboard">
      <section className="ci-page-head" aria-labelledby="ci-title">
        <div>
          <p className="eyebrow">Cloudflare-native continuous integration</p>
          <h1 id="ci-title">CI without GitHub.</h1>
          <p>
            Source publications enter one Worker-owned pipeline. Workflows coordinate
            isolated Linux containers; R2 retains source, logs, caches, and release artifacts.
          </p>
        </div>
        <div className="ci-runtime" aria-label="CI runtime architecture">
          <Runtime icon={<Cloud />} label="Control plane" value="Workers + Workflows" />
          <Runtime icon={<CircleDot />} label="Execution" value="Cloudflare Containers" />
          <Runtime icon={<Database />} label="Evidence" value="R2 + Durable Objects" />
        </div>
      </section>

      <section className="ci-summary" aria-label="CI summary">
        <Metric label="Retained runs" value={overview ? String(overview.runs.length) : "—"} />
        <Metric label="Latest result" value={resultLabel(latest?.result)} tone={resultTone(latest?.result)} />
        <Metric label="Workflow" value={latest?.workflow.status ?? (overview ? "idle" : "connecting")} />
        <Metric label="Validation gates" value={String(gates.length)} />
      </section>

      {error ? (
        <section className="ci-error" role="alert">
          <div>
            <p className="eyebrow">Worker request failed</p>
            <h2>CI state is unavailable.</h2>
            <p>{error.message}</p>
          </div>
          <button type="button" onClick={() => void refresh()}>
            <RefreshCw aria-hidden="true" /> Retry
          </button>
        </section>
      ) : null}

      <section className="ci-pipeline" aria-labelledby="pipeline-title">
        <header>
          <div>
            <p className="eyebrow">Execution graph</p>
            <h2 id="pipeline-title">Twelve gates, one immutable source.</h2>
          </div>
          <p>Independent branches run concurrently and converge only where artifacts are required.</p>
        </header>
        <div className="ci-gates">
          {gates.map(([slug, name, description], index) => {
            const step = completedStep(latest?.result, slug);
            return (
              <article className="ci-gate" key={slug}>
                <span className="ci-gate-index">{String(index + 1).padStart(2, "0")}</span>
                <div>
                  <h3>{name}</h3>
                  <p>{description}</p>
                </div>
                <GateState step={step} />
              </article>
            );
          })}
        </div>
      </section>

      <section className="ci-runs" aria-labelledby="runs-title">
        <header>
          <div>
            <p className="eyebrow">Publication history</p>
            <h2 id="runs-title">Runs</h2>
          </div>
          <button type="button" onClick={() => void refresh()} aria-label="Refresh CI runs">
            <RefreshCw aria-hidden="true" /> Refresh
          </button>
        </header>
        {overview?.runs.length === 0 ? (
          <div className="ci-empty">
            <GitBranch aria-hidden="true" />
            <div>
              <h3>Local control plane is ready.</h3>
              <p>No source publication has been submitted to this local Worker yet.</p>
            </div>
          </div>
        ) : null}
        {overview?.runs.map((run) => (
          <article className="ci-run" key={run.head}>
            <code title={run.head}>{run.head.slice(0, 12)}</code>
            <span>{run.state}</span>
            <span>{run.attempts} dispatch {run.attempts === 1 ? "attempt" : "attempts"}</span>
            <time dateTime={run.publishedAt}>{formatDate(run.publishedAt)}</time>
          </article>
        ))}
      </section>
    </div>
  );
}

export function CiStandalone() {
  return (
    <div className="site-shell surface-ci">
      <header className="site-header ci-standalone-header">
        <a className="wordmark" href="/">nanocodex</a>
        <div className="header-center">
          <nav className="surface-switch" aria-label="Product navigation">
            <a href="/agent">agent</a>
            <a href="/docs">docs</a>
            <a className="is-active" href="/ci" aria-current="page">ci</a>
          </nav>
        </div>
        <div className="ci-live"><span /> local</div>
      </header>
      <main id="top"><Ci /></main>
    </div>
  );
}

function Runtime({ icon, label, value }: { icon: React.ReactNode; label: string; value: string }) {
  return <div className="ci-runtime-row"><span>{icon}</span><div><small>{label}</small><strong>{value}</strong></div></div>;
}

function Metric({ label, value, tone }: { label: string; value: string; tone?: string }) {
  return <div className={tone ? `ci-metric is-${tone}` : "ci-metric"}><small>{label}</small><strong>{value}</strong></div>;
}

function GateState({ step }: { step: { exitCode: number; cacheHit: boolean; durationMs: number } | null }) {
  if (!step) return <span className="ci-gate-state">waiting</span>;
  return (
    <span className={step.exitCode === 0 ? "ci-gate-state is-success" : "ci-gate-state is-failure"}>
      {step.exitCode === 0 ? <Check aria-hidden="true" /> : <X aria-hidden="true" />}
      {step.cacheHit ? "cached" : formatDuration(step.durationMs)}
    </span>
  );
}

function completedStep(result: CiRunDetail["result"] | undefined, slug: string) {
  return result && "status" in result && (result.status === "success" || result.status === "failure")
    ? result.steps.find((step) => step.name.toLowerCase().replaceAll(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "") === slug) ?? null
    : null;
}

function resultLabel(result: CiRunDetail["result"] | undefined) {
  if (!result) return "none";
  return "status" in result ? result.status : "invalid";
}

function resultTone(result: CiRunDetail["result"] | undefined) {
  if (!result || !("status" in result)) return undefined;
  return result.status === "success" ? "success" : result.status === "failure" ? "failure" : "active";
}

function formatDate(value: string) {
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(new Date(value));
}

function formatDuration(milliseconds: number) {
  if (milliseconds < 1_000) return `${milliseconds}ms`;
  const seconds = Math.round(milliseconds / 1_000);
  return seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
}
