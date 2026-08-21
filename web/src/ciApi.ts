export type CiDispatchState = "pending" | "dispatched";

export type CiRun = {
  version: 1;
  head: string;
  beforeHead: string | null;
  workflowId: string;
  state: CiDispatchState;
  attempts: number;
  publishedAt: string;
  dispatchedAt?: string;
  lastDispatchError?: string;
  nextDispatchAt?: string;
};

export type CiStepSummary = {
  name: string;
  exitCode: number;
  cacheHit: boolean;
  durationMs: number;
};

export type CiProgressStep = {
  name: string;
  slug: string;
  status: "pending" | "running" | "success" | "failure";
  startedAt?: string;
  completedAt?: string;
  durationMs?: number;
  exitCode?: number;
  cacheHit?: boolean;
  message?: string;
};

export type CiProgress = {
  version: 1;
  head: string;
  startedAt: string;
  updatedAt: string;
  steps: CiProgressStep[];
};

export type CiArtifact = {
  key: string;
  size: number;
  sha256: string;
  contentType: string;
};

export type CiResult =
  | {
      version: 1;
      head: string;
      workflowId: string;
      status: "running";
      rustSecRevision: string;
      rustSecSha256: string;
      startedAt: string;
      steps: [];
    }
  | {
      version: 1;
      head: string;
      workflowId: string;
      status: "success";
      rustSecRevision: string;
      rustSecSha256: string;
      completedAt: string;
      durationMs: number;
      steps: CiStepSummary[];
      artifacts: CiArtifact[];
    }
  | {
      version: 1;
      head: string;
      workflowId: string;
      status: "failure";
      rustSecRevision: string;
      rustSecSha256: string;
      completedAt: string;
      durationMs: number;
      steps: CiStepSummary[];
      failure: { name: string; message: string };
    };

export type CiRunDetail = CiRun & {
  workflow: { status?: string; error?: unknown };
  result: CiResult | { error: "invalid_result" } | null;
  progress: CiProgress | { error: "invalid_progress" } | null;
};

export type CiRuns = {
  runs: CiRun[];
  retainedCount: number;
  retentionLimit: number;
};

export class CiApiError extends Error {
  readonly status: number;

  constructor(message: string, status: number) {
    super(message);
    this.name = "CiApiError";
    this.status = status;
  }
}

async function request<T>(path: string, signal?: AbortSignal): Promise<T> {
  const response = await fetch(path, { cache: "no-store", signal });
  if (response.ok) return response.json() as Promise<T>;
  const body = await response.json().catch(() => undefined) as { error?: string } | undefined;
  const reason = body?.error ? body.error.replaceAll("_", " ") : response.statusText;
  throw new CiApiError(`CI request failed: ${reason || `HTTP ${response.status}`}`, response.status);
}

export const ciApi = {
  runs(signal?: AbortSignal) {
    return request<CiRuns>("/api/ci/runs", signal);
  },
  run(head: string, signal?: AbortSignal) {
    return request<CiRunDetail>(`/api/ci/runs/${encodeURIComponent(head)}`, signal);
  },
};
