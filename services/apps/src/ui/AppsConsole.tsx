/// <reference lib="DOM" />
/// <reference lib="DOM.Iterable" />

import {
  type FormEvent,
  useCallback,
  useEffect,
  useMemo,
  useState,
} from "react";

type JobStatus = "building" | "completed" | "failed";

export type BuildJob = Readonly<{
  id: string;
  app_id: string;
  status: JobStatus;
  revision: string | null;
  error: string | null;
  created_at: string;
  completed_at: string | null;
}>;

type SourceFile = Readonly<{
  path: string;
  bytes?: number;
}>;

export type AppRevision = Readonly<{
  id: string;
  source_commit: string;
  created_at: string;
  artifact_bytes: number;
  generation_model: string;
  source_summary: Readonly<{
    entryPoint: string;
    files: readonly (SourceFile | string)[];
  }>;
}>;

export type GeneratedApp = Readonly<{
  id: string;
  display_name: string;
  slug: string;
  live_slug: string;
  active_revision: string;
  created_at: string;
  updated_at: string;
  revisions: readonly AppRevision[];
}>;

type AppsResponse = Readonly<{
  apps: readonly GeneratedApp[];
  tenant: Readonly<{ id: string; kind: "personal" | "team" }>;
}>;

type JobResponse = Readonly<{ job: BuildJob }>;
type ActivateResponse = Readonly<{ app: GeneratedApp }>;

type TrackedJob = BuildJob & Readonly<{
  requestedPrompt: string;
  updateAppId?: string;
}>;

type Notice = Readonly<{ kind: "error" | "success"; message: string }>;

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string | undefined,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

export function appLaunchPath(appId: string): string {
  return `/apps/api/apps/${encodeURIComponent(appId)}/launch`;
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "Unknown size";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"] as const;
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const precision = value >= 10 ? 0 : 1;
  return `${value.toFixed(precision)} ${units[unit]}`;
}

export function hasCompletedBuild(responses: readonly JobResponse[]): boolean {
  return responses.some(({ job }) => job.status === "completed");
}

export function AppsConsole() {
  const [apps, setApps] = useState<readonly GeneratedApp[]>([]);
  const [hasAppSnapshot, setHasAppSnapshot] = useState(false);
  const [appsFailure, setAppsFailure] = useState<string>();
  const [notice, setNotice] = useState<Notice>();
  const [jobs, setJobs] = useState<readonly TrackedJob[]>([]);
  const [createPrompt, setCreatePrompt] = useState("");
  const [updatePrompts, setUpdatePrompts] = useState<Record<string, string>>({});
  const [creating, setCreating] = useState(false);
  const [updatingAppIds, setUpdatingAppIds] = useState<ReadonlySet<string>>(new Set());
  const [rollbackKeys, setRollbackKeys] = useState<ReadonlySet<string>>(new Set());

  const refreshApps = useCallback(async () => {
    try {
      const response = await requestJson<AppsResponse>("/apps/api/apps");
      if (!Array.isArray(response.apps) || typeof response.tenant?.id !== "string") {
        throw new Error("The app platform returned an unexpected app list.");
      }
      setApps(response.apps);
      setHasAppSnapshot(true);
      setAppsFailure(undefined);
      return true;
    } catch (error) {
      setAppsFailure(actionableError(error, "The app list could not be refreshed."));
      return false;
    }
  }, []);

  useEffect(() => {
    void refreshApps();
  }, [refreshApps]);

  const activeJobIds = useMemo(
    () => jobs.filter((job) => job.status === "building").map((job) => job.id),
    [jobs],
  );
  const activeJobKey = activeJobIds.join(",");

  useEffect(() => {
    if (!activeJobKey) return undefined;
    const ids = activeJobKey.split(",");
    let cancelled = false;
    let timer: number | undefined;

    const poll = async () => {
      try {
        const responses = await Promise.all(
          ids.map((id) => requestJson<JobResponse>(`/apps/api/builds/${encodeURIComponent(id)}`)),
        );
        if (cancelled) return;
        const published = hasCompletedBuild(responses);
        setJobs((current) => current.map((tracked) => {
          const response = responses.find((candidate) => candidate.job.id === tracked.id);
          if (!response) return tracked;
          return {
            ...response.job,
            requestedPrompt: tracked.requestedPrompt,
            ...(tracked.updateAppId ? { updateAppId: tracked.updateAppId } : {}),
          };
        }));
        if (published) await refreshApps();
        setNotice((current) => current?.message === "Build status could not be checked. Retry the page to reconnect."
          ? undefined
          : current);
      } catch {
        if (!cancelled) {
          setNotice({
            kind: "error",
            message: "Build status could not be checked. Retry the page to reconnect.",
          });
        }
      }
      if (!cancelled) timer = window.setTimeout(() => void poll(), 2_000);
    };

    timer = window.setTimeout(() => void poll(), 2_000);
    return () => {
      cancelled = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [activeJobKey, refreshApps]);

  const createApp = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const prompt = createPrompt.trim();
    if (!prompt || creating) return;
    setCreating(true);
    setNotice(undefined);
    try {
      const response = await startBuild(prompt);
      setJobs((current) => [trackJob(response.job, prompt), ...current]);
      setCreatePrompt("");
      setNotice({ kind: "success", message: "Build accepted. Its status will stay here while it runs." });
    } catch (error) {
      setNotice({ kind: "error", message: actionableError(error, "The build could not be started.") });
    } finally {
      setCreating(false);
    }
  };

  const updateApp = async (event: FormEvent<HTMLFormElement>, appId: string) => {
    event.preventDefault();
    const prompt = (updatePrompts[appId] ?? "").trim();
    if (!prompt || updatingAppIds.has(appId)) return;
    setUpdatingAppIds((current) => withSetValue(current, appId, true));
    setNotice(undefined);
    try {
      const response = await startBuild(prompt, appId);
      setJobs((current) => [trackJob(response.job, prompt, appId), ...current]);
      setUpdatePrompts((current) => ({ ...current, [appId]: "" }));
      setNotice({ kind: "success", message: "Update accepted. The current app remains live during the build." });
    } catch (error) {
      setNotice({ kind: "error", message: actionableError(error, "The update could not be started.") });
    } finally {
      setUpdatingAppIds((current) => withSetValue(current, appId, false));
    }
  };

  const rollback = async (app: GeneratedApp, revision: AppRevision) => {
    const key = `${app.id}:${revision.id}`;
    if (rollbackKeys.has(key)) return;
    setRollbackKeys((current) => withSetValue(current, key, true));
    setNotice(undefined);
    try {
      const response = await requestJson<ActivateResponse>(
        `/apps/api/apps/${encodeURIComponent(app.id)}/activate`,
        jsonPost({
          revision: revision.id,
          expected_revision: app.active_revision,
          reason: "rollback",
        }),
      );
      setApps((current) => current.map((candidate) => candidate.id === response.app.id
        ? response.app
        : candidate));
      setNotice({ kind: "success", message: `${response.app.display_name} now serves the selected revision.` });
    } catch (error) {
      if (error instanceof ApiError && error.code === "stale_active") {
        await refreshApps();
        setNotice({
          kind: "error",
          message: "The active revision changed before rollback. The app card was refreshed; review it and try again.",
        });
      } else {
        setNotice({ kind: "error", message: actionableError(error, "The revision could not be restored.") });
      }
    } finally {
      setRollbackKeys((current) => withSetValue(current, key, false));
    }
  };

  const restoreFailedPrompt = (job: TrackedJob) => {
    if (job.updateAppId) {
      setUpdatePrompts((current) => ({ ...current, [job.updateAppId!]: job.requestedPrompt }));
    } else {
      setCreatePrompt(job.requestedPrompt);
    }
    setNotice({ kind: "success", message: "The prompt is ready to edit and submit again." });
  };

  return (
    <div className="console-shell">
      <header className="site-header">
        <a className="brand" href="/" aria-label="Nanocodex Apps home">
          <span className="brand-mark" aria-hidden="true">N</span>
          <span>Nanocodex Apps</span>
        </a>
        <a className="quiet-button" href="/agent">Back to Nanocodex</a>
      </header>

      <main>
        <section className="hero" aria-labelledby="console-title">
          <p className="eyebrow">Private app studio</p>
          <h1 id="console-title">Describe it. Ship it.</h1>
          <p className="hero-copy">
            Turn an idea into a private, live Dynamic Worker. Nanocodex builds the first version and
            keeps every revision ready to restore.
          </p>
          <form className="prompt-composer" onSubmit={createApp}>
            <label htmlFor="create-prompt">What should we build?</label>
            <textarea
              id="create-prompt"
              name="prompt"
              value={createPrompt}
              onChange={(event) => setCreatePrompt(event.currentTarget.value)}
              placeholder="Build me a lightweight project tracker with a calm dashboard and a JSON API…"
              rows={6}
              maxLength={24_576}
              required
            />
            <div className="composer-footer">
              <span>Include the audience, workflow, and visual direction.</span>
              <button className="primary-button" type="submit" disabled={creating || !createPrompt.trim()}>
                Build app
              </button>
            </div>
          </form>
        </section>

        {notice ? (
          <div className={`notice notice-${notice.kind}`} role={notice.kind === "error" ? "alert" : "status"}>
            <span>{notice.message}</span>
            <button type="button" aria-label="Dismiss message" onClick={() => setNotice(undefined)}>×</button>
          </div>
        ) : null}

        {jobs.length ? (
          <section className="jobs-section" aria-labelledby="jobs-title">
            <div className="section-heading">
              <div>
                <p className="eyebrow">Build activity</p>
                <h2 id="jobs-title">Durable jobs</h2>
              </div>
              <span className="section-count">{jobs.length}</span>
            </div>
            <div className="job-grid">
              {jobs.map((job) => (
                <JobCard key={job.id} job={job} apps={apps} onRestore={restoreFailedPrompt} />
              ))}
            </div>
          </section>
        ) : null}

        {appsFailure ? (
          <div className="notice notice-error" role="alert">
            <span>{appsFailure}</span>
            <button type="button" onClick={() => void refreshApps()}>Try again</button>
          </div>
        ) : null}

        {hasAppSnapshot ? (
          <section className="apps-section" aria-labelledby="apps-title">
            <div className="section-heading">
              <div>
                <p className="eyebrow">Your workspace</p>
                <h2 id="apps-title">Live apps</h2>
              </div>
              <span className="section-count">{apps.length}</span>
            </div>
            {apps.length ? (
              <div className="apps-grid">
                {apps.map((app) => {
                  const appBuilding = jobs.some((job) => job.status === "building" && job.app_id === app.id);
                  return (
                    <AppCard
                      key={app.id}
                      app={app}
                      prompt={updatePrompts[app.id] ?? ""}
                      building={appBuilding}
                      submitting={updatingAppIds.has(app.id)}
                      rollbackKeys={rollbackKeys}
                      onPromptChange={(prompt) => setUpdatePrompts((current) => ({ ...current, [app.id]: prompt }))}
                      onSubmit={(event) => void updateApp(event, app.id)}
                      onRollback={(revision) => void rollback(app, revision)}
                    />
                  );
                })}
              </div>
            ) : (
              <div className="empty-state">
                <h3>Your first app starts with a sentence.</h3>
                <p>Describe the result above. The app and its revision history will appear here.</p>
              </div>
            )}
          </section>
        ) : null}
      </main>

      <footer>
        <span>Generated apps stay behind your Nanocodex account.</span>
        <span>Every source change is a Git commit; every deploy is immutable.</span>
      </footer>
    </div>
  );
}

function JobCard({
  job,
  apps,
  onRestore,
}: Readonly<{
  job: TrackedJob;
  apps: readonly GeneratedApp[];
  onRestore: (job: TrackedJob) => void;
}>) {
  const app = apps.find((candidate) => candidate.id === job.app_id);
  const title = app?.display_name ?? (job.updateAppId ? "App update" : "New app");
  return (
    <article className={`job-card job-${job.status}`}>
      <div className="job-card-head">
        <span className="status-dot" aria-hidden="true" />
        <div>
          <h3>{title}</h3>
          <p>{job.status === "building" ? "Build in progress" : job.status === "completed" ? "Build completed" : "Build failed"}</p>
        </div>
        <time dateTime={job.created_at}>{formatDate(job.created_at)}</time>
      </div>
      <p className="job-prompt">{job.requestedPrompt}</p>
      {job.status === "completed" && app ? (
        <LaunchApp app={app} className="inline-action" label="Open live app" />
      ) : null}
      {job.status === "failed" ? (
        <div className="job-failure" role="alert">
          <p>{job.error || "The builder did not complete this request. Adjust the prompt and try again."}</p>
          <button type="button" onClick={() => onRestore(job)}>Edit prompt and try again</button>
        </div>
      ) : null}
    </article>
  );
}

function AppCard({
  app,
  prompt,
  building,
  submitting,
  rollbackKeys,
  onPromptChange,
  onSubmit,
  onRollback,
}: Readonly<{
  app: GeneratedApp;
  prompt: string;
  building: boolean;
  submitting: boolean;
  rollbackKeys: ReadonlySet<string>;
  onPromptChange: (prompt: string) => void;
  onSubmit: (event: FormEvent<HTMLFormElement>) => void;
  onRollback: (revision: AppRevision) => void;
}>) {
  const active = app.revisions.find((revision) => revision.id === app.active_revision);
  const rollbackActive = [...rollbackKeys].some((key) => key.startsWith(`${app.id}:`));
  return (
    <article className="app-card">
      <div className="app-card-header">
        <div className="app-identity">
          <span className="app-monogram" aria-hidden="true">{app.display_name.slice(0, 1).toUpperCase()}</span>
          <div>
            <h3>{app.display_name}</h3>
            <p>/{app.live_slug}</p>
          </div>
        </div>
        <LaunchApp app={app} className="launch-button" label="Open app" />
      </div>

      <dl className="app-meta">
        <div>
          <dt>Active revision</dt>
          <dd><code title={app.active_revision}>{shortRevision(app.active_revision)}</code></dd>
        </div>
        <div>
          <dt>Published</dt>
          <dd><time dateTime={app.updated_at}>{formatDate(app.updated_at)}</time></dd>
        </div>
        <div>
          <dt>Artifact</dt>
          <dd>{active ? formatBytes(active.artifact_bytes) : "Unavailable"}</dd>
        </div>
      </dl>

      <form className="update-form" id={`update-${app.id}`} onSubmit={onSubmit}>
        <label htmlFor={`update-prompt-${app.id}`}>Describe an update</label>
        <textarea
          id={`update-prompt-${app.id}`}
          name="prompt"
          value={prompt}
          onChange={(event) => onPromptChange(event.currentTarget.value)}
          placeholder="Add filtering, simplify the navigation, change the color palette…"
          rows={3}
          maxLength={24_576}
          required
        />
        <div className="update-actions">
          {building ? <span className="building-note">An update is building</span> : <span />}
          <button type="submit" disabled={submitting || building || rollbackActive || !prompt.trim()}>
            Build update
          </button>
        </div>
      </form>

      <details className="revision-history">
        <summary>
          <span>Revision history</span>
          <span>{app.revisions.length}</span>
        </summary>
        <ol>
          {app.revisions.map((revision) => {
            const isActive = revision.id === app.active_revision;
            const rollbackKey = `${app.id}:${revision.id}`;
            const restoring = rollbackKeys.has(rollbackKey);
            return (
              <li key={revision.id}>
                <div className="revision-main">
                  <div className="revision-title">
                    <code title={revision.id}>{shortRevision(revision.id)}</code>
                    {isActive ? <span className="active-badge">Live</span> : null}
                  </div>
                  <p>
                    <time dateTime={revision.created_at}>{formatDate(revision.created_at)}</time>
                    <span aria-hidden="true"> · </span>
                    {formatBytes(revision.artifact_bytes)}
                    <span aria-hidden="true"> · </span>
                    {revision.generation_model}
                  </p>
                  <p className="source-summary">
                    Git {shortCommit(revision.source_commit)} · {revision.source_summary.entryPoint} · {revision.source_summary.files.length} {revision.source_summary.files.length === 1 ? "file" : "files"}
                  </p>
                </div>
                {!isActive ? (
                  <button
                    type="button"
                    disabled={restoring || rollbackActive || building || submitting}
                    onClick={() => onRollback(revision)}
                  >
                    Rollback
                  </button>
                ) : null}
              </li>
            );
          })}
        </ol>
      </details>
    </article>
  );
}

async function startBuild(prompt: string, appId?: string): Promise<JobResponse> {
  return requestJson<JobResponse>("/apps/api/builds", jsonPost({
    prompt,
    ...(appId ? { app_id: appId } : {}),
  }));
}

function LaunchApp({ app, className, label }: Readonly<{
  app: GeneratedApp;
  className: string;
  label: string;
}>) {
  return (
    <form action={appLaunchPath(app.id)} method="post" target="_blank">
      <button className={className} type="submit">
        {label} <span aria-hidden="true">↗</span>
      </button>
    </form>
  );
}

function trackJob(job: BuildJob, requestedPrompt: string, updateAppId?: string): TrackedJob {
  return { ...job, requestedPrompt, ...(updateAppId ? { updateAppId } : {}) };
}

function jsonPost(body: Record<string, unknown>): RequestInit {
  return {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  };
}

async function requestJson<T>(path: string, init?: RequestInit): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, {
      ...init,
      credentials: "same-origin",
      headers: {
        accept: "application/json",
        ...headersObject(init?.headers),
      },
    });
  } catch {
    throw new Error("The app platform could not be reached. Check your connection and try again.");
  }

  let payload: unknown;
  try {
    payload = await response.json();
  } catch {
    throw new ApiError(response.status, undefined, "The app platform returned an unreadable response.");
  }
  if (!response.ok) {
    const record = isRecord(payload) ? payload : {};
    const code = typeof record.error === "string" ? record.error : undefined;
    const detail = typeof record.message === "string" ? record.message : code;
    throw new ApiError(response.status, code, detail || `Request failed with status ${response.status}.`);
  }
  return payload as T;
}

function headersObject(headers: HeadersInit | undefined): Record<string, string> {
  return Object.fromEntries(new Headers(headers).entries());
}

function actionableError(error: unknown, fallback: string): string {
  if (error instanceof ApiError) {
    if (error.status === 401) return "Your owner session expired. Log in again, then retry this action.";
    if (error.status === 403) return "The platform rejected this change. Refresh the page and try again.";
    if (error.status === 409) return `${error.message} Refresh the app state and try again.`;
    return `${fallback} ${error.message}`;
  }
  return error instanceof Error && error.message ? error.message : fallback;
}

function withSetValue(current: ReadonlySet<string>, value: string, present: boolean): ReadonlySet<string> {
  const next = new Set(current);
  if (present) next.add(value);
  else next.delete(value);
  return next;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function shortRevision(revision: string): string {
  return revision ? revision.slice(0, 10) : "none";
}

function shortCommit(commit: string): string {
  return commit ? commit.slice(0, 8) : "unavailable";
}

function formatDate(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.valueOf())) return "Unknown time";
  return new Intl.DateTimeFormat(undefined, {
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
    month: "short",
    year: "numeric",
  }).format(date);
}
