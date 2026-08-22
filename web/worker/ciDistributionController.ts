import {
  ciWorkflowParams,
  type CiDistributionLease,
  type CiLaneState,
  type CiRunRecord,
} from "./ciRepository.ts";
import type {
  CiDistributionRequest,
  CiSourcePublication,
  NanocodexCiParams,
} from "./ciSource.ts";
import { ciSourceLane, isCiSourcePublication } from "./ciSource.ts";

const STABLE_TAG = /^v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$/;
const SHA1 = /^[a-f0-9]{40}$/;
const UUID_V4 = /^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;
const MAX_DISTRIBUTION_LEASE_MS = 6 * 60 * 60 * 1_000;
const MAX_DISTRIBUTION_RESULT_BYTES = 1024 * 1024;
const MAX_CI_RESULT_BYTES = 8 * 1024 * 1024;
const DISTRIBUTION_ATTEMPT_CLAIM_MS = 30_000;
const DISTRIBUTION_EXECUTION_CLAIM_MS = MAX_DISTRIBUTION_LEASE_MS;
const PUBLICATION_LEASE_ID =
  /^(0|[1-9][0-9]{0,15})\.[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;
const PUBLICATION_LEASE_OWNER = /^[A-Za-z0-9][A-Za-z0-9._:@/-]{0,191}$/;
const RELEASE_ASSET_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,159}$/;
const RELEASE_CONTENT_TYPE =
  /^[A-Za-z0-9][A-Za-z0-9!#$&^_.+-]*\/[A-Za-z0-9][A-Za-z0-9!#$&^_.+-]*(?:;[\t ]*[A-Za-z0-9_-]+=[A-Za-z0-9._-]+)*$/;
const SHA256 = /^[a-f0-9]{64}$/;

export type CiDistributionControlEnv = {
  BACKUP_BUCKET?: R2Bucket;
  CI_REPOSITORY?: DurableObjectNamespace;
  CI_RELEASES?: DurableObjectNamespace;
  CI_WORKFLOW?: Workflow<NanocodexCiParams>;
  CI_RELEASE_TOKEN?: string;
};

export async function routeCiDistributionControl(
  request: Request,
  env: CiDistributionControlEnv,
  url: URL,
): Promise<Response | undefined> {
  if (url.pathname === "/api/ci/releases/nightly") {
    if (request.method !== "POST") return error("method_not_allowed", 405);
    if (!authenticate(request, env.CI_RELEASE_TOKEN)) return unauthorized();
    const configured = configuredEnv(env);
    if (configured instanceof Response) return configured;
    const requested = await optionalHead(request);
    if (requested instanceof Response) return requested;
    const dispatched = await dispatchNightly(configured, requested ?? undefined);
    return dispatched instanceof Response
      ? dispatched
      : Response.json(dispatched, {
        status: dispatched.status === "current" ? 200 : 202,
        headers: noStoreHeaders(),
      });
  }
  const match = url.pathname.match(
    /^\/api\/ci\/releases\/stable\/(v[^/]+)(\/finalize)?$/,
  );
  if (!match) return undefined;
  if (request.method !== "POST") return error("method_not_allowed", 405);
  if (!authenticate(request, env.CI_RELEASE_TOKEN)) return unauthorized();
  const tagName = match[1]!;
  if (!STABLE_TAG.test(tagName)) return error("invalid_release_tag", 400);
  const configured = configuredEnv(env);
  if (configured instanceof Response) return configured;
  const publicationFence = match[2] ? stablePublicationFence(request) : undefined;
  if (publicationFence instanceof Response) return publicationFence;

  const requested = await optionalHead(request);
  if (requested instanceof Response) return requested;
  const state = await greenMasterState(configured, requested ?? undefined);
  if (state instanceof Response) return state;
  const existing = await releaseManifest(configured.CI_RELEASES, "stable", tagName);
  const prepared = await stableDistributionState(configured.BACKUP_BUCKET, tagName);
  if (existing) {
    if (existing.commit !== state.publication.head) {
      return error("immutable_release_conflict", 409);
    }
    const publication = await exactStablePublication(
      configured.CI_RELEASES,
      tagName,
      state.publication.head,
      existing,
    );
    if (!publication) return error("stable_release_state_invalid", 503);
    if (!exactStablePrepared(prepared, tagName, state.publication.head)) {
      return error("stable_distribution_not_ready", 409);
    }
    await persistStableReleaseSuccess(
      configured.BACKUP_BUCKET,
      tagName,
      prepared,
      publication,
    );
    return Response.json({ status: "released", publication }, {
      headers: noStoreHeaders(),
    });
  }
  if (match[2]) {
    if (
      prepared?.status !== "ready" || prepared.head !== state.publication.head ||
      prepared.tagName !== tagName
    ) return error("stable_distribution_not_ready", 409);
    const finalized = await releaseStub(configured.CI_RELEASES).fetch(
      `https://ci-releases/drafts/stable/${tagName}/finalize`,
      {
        method: "POST",
        headers: {
          authorization: `Bearer ${configured.CI_RELEASE_TOKEN}`,
          ...publicationFence,
        },
      },
    );
    if (!finalized.ok) return new Response(finalized.body, finalized);
    await finalized.body?.cancel().catch(() => undefined);
    const publication = await exactStablePublication(
      configured.CI_RELEASES,
      tagName,
      state.publication.head,
    );
    if (!publication) return error("stable_release_state_invalid", 503);
    await persistStableReleaseSuccess(configured.BACKUP_BUCKET, tagName, prepared, publication);
    return Response.json({ status: "released", publication }, {
      headers: noStoreHeaders(),
    });
  }
  if (prepared?.status === "ready" && prepared.head === state.publication.head) {
    return Response.json({ status: "ready", distribution: prepared }, {
      headers: noStoreHeaders(),
    });
  }

  const distribution = {
    version: 1,
    channel: "stable",
    tagName,
    buildTimestamp: state.publication.publishedAt,
  } satisfies CiDistributionRequest;
  const workflowId = `release-${tagName}-${state.publication.head}`;
  const lease = await acquireDistributionLease(
    configured.CI_REPOSITORY,
    state.publication.head,
    workflowId,
  );
  if (lease instanceof Response) return lease;
  const retained = await greenMasterState(configured, state.publication.head);
  if (retained instanceof Response) return error("ci_head_changed", 409);
  const attempt = await ensureDistributionWorkflow(
    configured.CI_WORKFLOW,
    workflowId,
    // A SHA-global correctness run may first have been created by a PR lane,
    // so its predecessor is not authoritative master history. Distribution
    // source is exact and needs no synthetic beforeSha.
    ciWorkflowParams(state.publication, null, distribution),
    configured.BACKUP_BUCKET,
    { ...distribution, head: state.publication.head },
  );
  return Response.json({
    status: "accepted",
    channel: distribution.channel,
    tagName,
    head: state.publication.head,
    workflowId,
    workflow: attempt.workflow,
    requestId: attempt.requestId,
  }, { status: 202, headers: noStoreHeaders() });
}

export async function dispatchNightlyDistribution(
  env: CiDistributionControlEnv,
): Promise<NightlyDispatchResult> {
  const configured = configuredEnv(env);
  if (configured instanceof Response) throw new Error("CI distribution is not configured");
  const dispatched = await dispatchNightly(configured);
  if (!(dispatched instanceof Response)) return dispatched;
  const detail = await dispatched.text();
  throw new Error(`Nightly distribution requires a green current CI head: ${detail}`);
}

type NightlyDispatchResult = {
  status: "current" | "accepted" | "restarted";
  head: string;
  workflowId?: string;
  requestId?: string;
};

type DistributionAttemptIdentity = CiDistributionRequest & {
  head: string;
};

type DistributionAttemptRecord = DistributionAttemptIdentity & {
  version: 1;
  state:
    | "creating"
    | "create_executing"
    | "active"
    | "restarting"
    | "restart_executing"
    | "create_failed"
    | "restart_failed";
  requestId: string;
  workflowId: string;
  requestedAt: string;
  claimExpiresAt: string;
};

type StoredDistributionAttempt = {
  record: DistributionAttemptRecord;
  etag: string;
};

type DistributionWorkflowDispatch = {
  workflow: string;
  requestId: string;
};

async function dispatchNightly(
  configured: ConfiguredEnv,
  requestedHead?: string,
): Promise<NightlyDispatchResult | Response> {
  const state = await greenMasterState(configured);
  if (state instanceof Response) return state;
  if (requestedHead != null && requestedHead !== state.publication.head) {
    return error("ci_head_changed", 409);
  }
  const current = await releaseChannel(configured.CI_RELEASES, "nightly");
  if (releaseChannelCommit(current) === state.publication.head) {
    const repaired = await reconcileNightlyReleaseSuccess(
      configured,
      state.publication.head,
      current,
    );
    if (repaired) return repaired;
    return { status: "current", head: state.publication.head };
  }
  const distribution = {
    version: 1,
    channel: "nightly",
    tagName: "nightly",
    buildTimestamp: state.publication.publishedAt,
  } satisfies CiDistributionRequest;
  const workflowId = `nightly-${state.publication.head}`;
  const lease = await acquireDistributionLease(
    configured.CI_REPOSITORY,
    state.publication.head,
    workflowId,
  );
  if (lease instanceof Response) return lease;
  const retained = await greenMasterState(configured, state.publication.head);
  if (retained instanceof Response) return error("ci_head_changed", 409);
  const latest = await greenMasterState(configured);
  if (latest instanceof Response || latest.publication.head !== state.publication.head) {
    return error("ci_head_changed", 409);
  }
  const rechecked = await releaseChannel(configured.CI_RELEASES, "nightly");
  if (releaseChannelCommit(rechecked) === state.publication.head) {
    const repaired = await reconcileNightlyReleaseSuccess(
      configured,
      state.publication.head,
      rechecked,
    );
    if (repaired) return repaired;
    return { status: "current", head: state.publication.head };
  }
  const attempt = await ensureDistributionWorkflow(
    configured.CI_WORKFLOW,
    workflowId,
    ciWorkflowParams(state.publication, null, distribution),
    configured.BACKUP_BUCKET,
    { ...distribution, head: state.publication.head },
  );
  return {
    status: attempt.workflow === "restarted" ? "restarted" : "accepted",
    head: state.publication.head,
    workflowId,
    requestId: attempt.requestId,
  };
}

type ConfiguredEnv = Required<
  Pick<
    CiDistributionControlEnv,
    "BACKUP_BUCKET" | "CI_REPOSITORY" | "CI_RELEASES" | "CI_WORKFLOW" |
      "CI_RELEASE_TOKEN"
  >
>;

type StableDistributionState = {
  status?: unknown;
  head?: unknown;
  tagName?: unknown;
  [key: string]: unknown;
};

type PublicReleaseAsset = {
  name: string;
  platform: string;
  size: number;
  sha256: string;
  contentType: string;
  downloadPath: string;
};

type PublicStableManifest = {
  version: 1;
  kind: "stable";
  id: string;
  tag: string;
  commit: string;
  channel: "latest";
  finalizedAt: string;
  assets: PublicReleaseAsset[];
  manifestSha256: string;
};

type PublicStablePointer = {
  version: 1;
  channel: "latest";
  kind: "stable";
  id: string;
  tag: string;
  commit: string;
  generation: number;
  updatedAt: string;
};

type PublicStablePublication = {
  manifest: PublicStableManifest;
  pointer: PublicStablePointer;
};

type PublicCommitManifest = {
  version: 1;
  kind: "commit";
  id: string;
  tag: string;
  commit: string;
  channel: "nightly";
  finalizedAt: string;
  assets: PublicReleaseAsset[];
  manifestSha256: string;
};

type PublicCommitPointer = {
  version: 1;
  channel: "nightly";
  kind: "commit";
  id: string;
  tag: string;
  commit: string;
  generation: number;
  updatedAt: string;
};

type PublicCommitPublication = {
  manifest: PublicCommitManifest;
  pointer: PublicCommitPointer;
};

function configuredEnv(env: CiDistributionControlEnv): ConfiguredEnv | Response {
  return env.BACKUP_BUCKET && env.CI_REPOSITORY && env.CI_RELEASES && env.CI_WORKFLOW &&
      env.CI_RELEASE_TOKEN
    ? env as ConfiguredEnv
    : error("ci_distribution_not_configured", 503);
}

async function greenMasterState(env: ConfiguredEnv, head?: string): Promise<{
  publication: CiSourcePublication;
  run: CiRunRecord;
} | Response> {
  const repository = env.CI_REPOSITORY.get(env.CI_REPOSITORY.idFromName("nanocodex"));
  const response = await repository.fetch(
    head == null
      ? "https://ci-repository/state"
      : `https://ci-repository/master/publications/${head}`,
  );
  if (response.status === 404) {
    return error(head == null ? "ci_source_not_published" : "release_head_not_retained", 409);
  }
  if (!response.ok) return error("ci_repository_state_invalid", 503);
  const state = await response.json() as Partial<CiLaneState>;
  const publication = state.publication;
  const run = state.run;
  const publicationHead = publication?.head;
  if (
    !publication || !isCiSourcePublication(publication) ||
    ciSourceLane(publication).type !== "master" ||
    !run || !publicationHead || !SHA1.test(publicationHead) ||
    run.version !== 1 || run.head !== publicationHead ||
    run.workflowId !== `ci-${publicationHead}` || run.state !== "dispatched" ||
    !Number.isSafeInteger(run.attempts) || run.attempts < 1 ||
    !validTimestamp(run.publishedAt) ||
    (head != null && publicationHead !== head)
  ) {
    return error("ci_repository_state_invalid", 503);
  }
  const result = await env.BACKUP_BUCKET.get(`runs/${publicationHead}/result.json`);
  const evidence = result && result.size > 0 && result.size <= MAX_CI_RESULT_BYTES
    ? await result.json<Record<string, unknown>>().catch(() => undefined)
    : undefined;
  if (result && !evidence) await result.body.cancel().catch(() => undefined);
  if (
    !record(evidence) || evidence.version !== 1 || evidence.status !== "success" ||
    evidence.head !== publicationHead || evidence.workflowId !== run.workflowId ||
    !validTimestamp(evidence.completedAt)
  ) return error("ci_head_not_green", 409);
  return { publication, run };
}

async function ensureDistributionWorkflow(
  workflow: Workflow<NanocodexCiParams>,
  id: string,
  params: NanocodexCiParams,
  bucket: R2Bucket,
  identity: DistributionAttemptIdentity,
): Promise<DistributionWorkflowDispatch> {
  const selected = await currentDistributionAttempt(bucket, identity, id);
  let attempt = selected.attempt;
  let createOwner = selected.createOwner;
  if (
    !createOwner && attempt.record.state === "create_executing" &&
    !distributionClaimActive(attempt.record)
  ) {
    const instance = await workflow.get(id);
    const status = await instance.status().catch(() => ({ status: "unknown" as const }));
    if (status.status === "unknown") {
      await failDistributionAttempt(bucket, identity, id, attempt, "create_failed");
      return ensureDistributionWorkflow(workflow, id, params, bucket, identity);
    }
    attempt = await activateDistributionAttempt(bucket, identity, id, attempt);
    if (terminalWorkflowStatus(status.status)) {
      return restartDistributionWorkflow(instance, bucket, identity, id);
    }
    return { workflow: status.status, requestId: attempt.record.requestId };
  }
  if (
    !createOwner &&
    (attempt.record.state === "restart_failed" ||
      ((attempt.record.state === "restarting" ||
        attempt.record.state === "restart_executing") &&
        !distributionClaimActive(attempt.record)))
  ) {
    const instance = await workflow.get(id);
    const status = await instance.status().catch(() => ({ status: "unknown" as const }));
    if (terminalWorkflowStatus(status.status)) {
      if (attempt.record.state === "restart_executing") {
        attempt = await failDistributionAttempt(
          bucket,
          identity,
          id,
          attempt,
          "restart_failed",
        );
      }
      return restartDistributionWorkflow(instance, bucket, identity, id);
    }
    if (status.status !== "unknown") {
      attempt = await activateDistributionAttempt(bucket, identity, id, attempt);
      return { workflow: status.status, requestId: attempt.record.requestId };
    }
    throw new Error(`Distribution restart state is unknown for ${id}`);
  }
  if (!createOwner && attempt.record.state === "active") {
    const instance = await workflow.get(id);
    const status = await instance.status().catch(() => ({ status: "unknown" as const }));
    if (terminalWorkflowStatus(status.status)) {
      return restartDistributionWorkflow(instance, bucket, identity, id);
    }
    if (status.status !== "unknown") {
      return { workflow: status.status, requestId: attempt.record.requestId };
    }
    const replacement = newDistributionAttempt(identity, id, "creating");
    const claimed = await putDistributionAttempt(
      bucket,
      identity,
      id,
      replacement,
      { etagMatches: attempt.etag },
    );
    if (!claimed) return ensureDistributionWorkflow(workflow, id, params, bucket, identity);
    attempt = claimed;
    createOwner = true;
  }
  if (!createOwner && attempt.record.state !== "active") {
    attempt = await waitForDistributionAttempt(bucket, identity, id);
    if (attempt.record.state === "create_failed" || attempt.record.state === "restart_failed") {
      throw new Error(`Distribution attempt failed for ${id}`);
    }
    const instance = await workflow.get(id);
    const status = await instance.status().catch(() => ({ status: "unknown" as const }));
    if (terminalWorkflowStatus(status.status)) {
      return restartDistributionWorkflow(instance, bucket, identity, id);
    }
    if (status.status !== "unknown") {
      return { workflow: status.status, requestId: attempt.record.requestId };
    }
    return ensureDistributionWorkflow(workflow, id, params, bucket, identity);
  }
  if (createOwner) {
    const executing = await beginDistributionCreateExecution(
      bucket,
      identity,
      id,
      attempt,
    );
    if (!executing) {
      attempt = await waitForDistributionAttempt(bucket, identity, id);
      if (attempt.record.state !== "active") {
        throw new Error(`Distribution create failed for ${id}`);
      }
      const instance = await workflow.get(id);
      const status = await instance.status().catch(() => ({ status: "unknown" as const }));
      if (terminalWorkflowStatus(status.status)) {
        return restartDistributionWorkflow(instance, bucket, identity, id);
      }
      if (status.status === "unknown") {
        throw new Error(`Distribution create state is unknown for ${id}`);
      }
      return { workflow: status.status, requestId: attempt.record.requestId };
    }
    attempt = executing;
  }
  try {
    if (attempt.record.state === "create_executing") {
      await requireDistributionAttemptFence(
        bucket,
        identity,
        id,
        attempt,
        "create_executing",
      );
    }
    await workflow.create({
      id,
      params,
      retention: { successRetention: "30 days", errorRetention: "30 days" },
    });
    if (attempt.record.state !== "active") {
      attempt = await activateDistributionAttempt(bucket, identity, id, attempt);
    }
    return { workflow: "queued", requestId: attempt.record.requestId };
  } catch (cause) {
    const instance = await workflow.get(id);
    const status = await instance.status().catch(() => ({ status: "unknown" as const }));
    if (
      status.status === "complete" || status.status === "errored" ||
      status.status === "terminated"
    ) {
      return restartDistributionWorkflow(instance, bucket, identity, id);
    }
    if (status.status !== "unknown") {
      if (attempt.record.state !== "active") {
        attempt = await activateDistributionAttempt(bucket, identity, id, attempt);
      }
      return { workflow: status.status, requestId: attempt.record.requestId };
    }
    if (attempt.record.state === "creating" || attempt.record.state === "create_executing") {
      await failDistributionAttempt(bucket, identity, id, attempt, "create_failed")
        .catch(() => undefined);
    }
    throw cause;
  }
}

async function restartDistributionWorkflow(
  instance: {
    status(): Promise<{ status: string }>;
    restart(): Promise<unknown>;
  },
  bucket: R2Bucket,
  identity: DistributionAttemptIdentity,
  workflowId: string,
): Promise<DistributionWorkflowDispatch> {
  let attempt: StoredDistributionAttempt;
  const claim = await claimDistributionRestart(bucket, identity, workflowId);
  if (!claim.owner) {
    return awaitDistributionRestart(instance, bucket, identity, workflowId);
  }

  const rechecked = await instance.status().catch(() => ({ status: "unknown" as const }));
  if (rechecked.status !== "unknown" && !terminalWorkflowStatus(rechecked.status)) {
    attempt = await restoreDistributionAttempt(
      bucket,
      identity,
      workflowId,
      claim.claimed,
      claim.previous,
    );
    return { workflow: rechecked.status, requestId: attempt.record.requestId };
  }

  const executing = await beginDistributionRestartExecution(
    bucket,
    identity,
    workflowId,
    claim.claimed,
  );
  if (!executing) {
    return awaitDistributionRestart(instance, bucket, identity, workflowId);
  }

  try {
    await supersedeTerminalDistributionResult(
      bucket,
      identity,
      workflowId,
      executing.record,
    );
  } catch (supersedeCause) {
    await failDistributionAttempt(
      bucket,
      identity,
      workflowId,
      executing,
      "restart_failed",
    )
      .catch(() => undefined);
    throw supersedeCause;
  }
  try {
    await requireDistributionAttemptFence(bucket, identity, workflowId, executing);
    await instance.restart();
  } catch (restartCause) {
    const reconciled = await instance.status().catch(() => ({ status: "unknown" as const }));
    if (reconciled.status !== "unknown" && !terminalWorkflowStatus(reconciled.status)) {
      attempt = await activateDistributionAttempt(bucket, identity, workflowId, executing);
      return { workflow: "restarted", requestId: attempt.record.requestId };
    }
    if (reconciled.status !== "unknown") {
      await failDistributionAttempt(
        bucket,
        identity,
        workflowId,
        executing,
        "restart_failed",
      )
        .catch(() => undefined);
    }
    throw restartCause;
  }
  attempt = await activateDistributionAttempt(bucket, identity, workflowId, executing);
  return { workflow: "restarted", requestId: attempt.record.requestId };
}

async function awaitDistributionRestart(
  instance: { status(): Promise<{ status: string }> },
  bucket: R2Bucket,
  identity: DistributionAttemptIdentity,
  workflowId: string,
): Promise<DistributionWorkflowDispatch> {
  const attempt = await waitForDistributionAttempt(bucket, identity, workflowId);
  if (attempt.record.state !== "active") {
    throw new Error(`Distribution restart failed for ${workflowId}`);
  }
  const reconciled = await instance.status().catch(() => ({ status: "unknown" as const }));
  if (reconciled.status === "unknown") {
    throw new Error(`Distribution restart state is unknown for ${workflowId}`);
  }
  return {
    workflow: terminalWorkflowStatus(reconciled.status) ? "restarted" : reconciled.status,
    requestId: attempt.record.requestId,
  };
}

async function currentDistributionAttempt(
  bucket: R2Bucket,
  identity: DistributionAttemptIdentity,
  workflowId: string,
): Promise<{ attempt: StoredDistributionAttempt; createOwner: boolean }> {
  const existing = await readDistributionAttempt(bucket, identity, workflowId);
  if (existing) {
    const retryCreate = existing.record.state === "create_failed" ||
      (existing.record.state === "creating" && !distributionClaimActive(existing.record));
    if (!retryCreate) return { attempt: existing, createOwner: false };
    const replacement = newDistributionAttempt(identity, workflowId, "creating");
    const claimed = await putDistributionAttempt(
      bucket,
      identity,
      workflowId,
      replacement,
      { etagMatches: existing.etag },
    );
    if (claimed) return { attempt: claimed, createOwner: true };
    const winner = await readDistributionAttempt(bucket, identity, workflowId);
    if (!winner) throw new Error(`Distribution create takeover lost its winner for ${workflowId}`);
    return { attempt: winner, createOwner: false };
  }
  const candidate = newDistributionAttempt(identity, workflowId, "creating");
  const created = await putDistributionAttempt(
    bucket,
    identity,
    workflowId,
    candidate,
    { etagDoesNotMatch: "*" },
  );
  if (created) return { attempt: created, createOwner: true };
  const winner = await readDistributionAttempt(bucket, identity, workflowId);
  if (!winner) throw new Error(`Distribution request race lost its winner for ${workflowId}`);
  return { attempt: winner, createOwner: false };
}

async function supersedeTerminalDistributionResult(
  bucket: R2Bucket,
  identity: DistributionAttemptIdentity,
  workflowId: string,
  attempt: DistributionAttemptRecord,
): Promise<void> {
  const prefix = distributionPrefix(identity);
  const result = await boundedBucketJson(bucket, `${prefix}/result.json`);
  if (result !== undefined && !validDistributionResultIdentity(result, identity, workflowId)) {
    throw new Error(`Distribution result identity is invalid for ${workflowId}`);
  }

  const restartedAt = new Date().toISOString();
  await bucket.put(
    `${prefix}/result.json`,
    JSON.stringify({
      version: 1,
      status: "running",
      channel: identity.channel,
      tagName: identity.tagName,
      head: identity.head,
      workflowId,
      requestId: attempt.requestId,
      requestedAt: attempt.requestedAt,
      startedAt: restartedAt,
      restartedAt,
    }),
    { httpMetadata: { contentType: "application/json" } },
  );
}

function newDistributionAttempt(
  identity: DistributionAttemptIdentity,
  workflowId: string,
  state: DistributionAttemptRecord["state"],
): DistributionAttemptRecord {
  const requested = Date.now();
  return {
    ...identity,
    version: 1,
    state,
    requestId: crypto.randomUUID(),
    workflowId,
    requestedAt: new Date(requested).toISOString(),
    claimExpiresAt: new Date(requested + DISTRIBUTION_ATTEMPT_CLAIM_MS).toISOString(),
  };
}

async function beginDistributionRestartExecution(
  bucket: R2Bucket,
  identity: DistributionAttemptIdentity,
  workflowId: string,
  claimed: StoredDistributionAttempt,
): Promise<StoredDistributionAttempt | undefined> {
  const executing = {
    ...claimed.record,
    state: "restart_executing" as const,
    claimExpiresAt: new Date(Date.now() + DISTRIBUTION_EXECUTION_CLAIM_MS).toISOString(),
  };
  return putDistributionAttempt(
    bucket,
    identity,
    workflowId,
    executing,
    { etagMatches: claimed.etag },
  );
}

async function beginDistributionCreateExecution(
  bucket: R2Bucket,
  identity: DistributionAttemptIdentity,
  workflowId: string,
  claimed: StoredDistributionAttempt,
): Promise<StoredDistributionAttempt | undefined> {
  const executing = {
    ...claimed.record,
    state: "create_executing" as const,
    claimExpiresAt: new Date(Date.now() + DISTRIBUTION_EXECUTION_CLAIM_MS).toISOString(),
  };
  return putDistributionAttempt(
    bucket,
    identity,
    workflowId,
    executing,
    { etagMatches: claimed.etag },
  );
}

async function requireDistributionAttemptFence(
  bucket: R2Bucket,
  identity: DistributionAttemptIdentity,
  workflowId: string,
  expected: StoredDistributionAttempt,
  state: "create_executing" | "restart_executing" = "restart_executing",
): Promise<void> {
  const current = await readDistributionAttempt(bucket, identity, workflowId);
  if (
    !current || current.etag !== expected.etag ||
    current.record.requestId !== expected.record.requestId ||
    current.record.state !== state ||
    !distributionClaimActive(current.record)
  ) throw new Error(`Distribution restart lost its execution fence for ${workflowId}`);
}

async function claimDistributionRestart(
  bucket: R2Bucket,
  identity: DistributionAttemptIdentity,
  workflowId: string,
): Promise<{
  owner: true;
  previous: StoredDistributionAttempt;
  claimed: StoredDistributionAttempt;
} | { owner: false; claimed: StoredDistributionAttempt }> {
  const current = (await currentDistributionAttempt(bucket, identity, workflowId)).attempt;
  if (
    (current.record.state === "restarting" && distributionClaimActive(current.record)) ||
    current.record.state === "restart_executing"
  ) {
    return { owner: false, claimed: current };
  }
  const candidate = newDistributionAttempt(identity, workflowId, "restarting");
  const claimed = await putDistributionAttempt(
    bucket,
    identity,
    workflowId,
    candidate,
    { etagMatches: current.etag },
  );
  if (claimed) return { owner: true, previous: current, claimed };
  const winner = await readDistributionAttempt(bucket, identity, workflowId);
  if (!winner) throw new Error(`Distribution restart race lost its winner for ${workflowId}`);
  return { owner: false, claimed: winner };
}

async function activateDistributionAttempt(
  bucket: R2Bucket,
  identity: DistributionAttemptIdentity,
  workflowId: string,
  current: StoredDistributionAttempt,
): Promise<StoredDistributionAttempt> {
  if (current.record.state === "active") return current;
  const active = { ...current.record, state: "active" as const };
  const stored = await putDistributionAttempt(
    bucket,
    identity,
    workflowId,
    active,
    { etagMatches: current.etag },
  );
  if (stored) return stored;
  const winner = await readDistributionAttempt(bucket, identity, workflowId);
  if (winner?.record.requestId === active.requestId && winner.record.state === "active") {
    return winner;
  }
  throw new Error(`Distribution restart completion lost its fence for ${workflowId}`);
}

async function failDistributionAttempt(
  bucket: R2Bucket,
  identity: DistributionAttemptIdentity,
  workflowId: string,
  current: StoredDistributionAttempt,
  state: "create_failed" | "restart_failed",
): Promise<StoredDistributionAttempt> {
  const failed = { ...current.record, state };
  const stored = await putDistributionAttempt(
    bucket,
    identity,
    workflowId,
    failed,
    { etagMatches: current.etag },
  );
  if (stored) return stored;
  const winner = await readDistributionAttempt(bucket, identity, workflowId);
  if (winner?.record.requestId === failed.requestId && winner.record.state === state) {
    return winner;
  }
  throw new Error(`Distribution attempt failure lost its fence for ${workflowId}`);
}

async function restoreDistributionAttempt(
  bucket: R2Bucket,
  identity: DistributionAttemptIdentity,
  workflowId: string,
  claimed: StoredDistributionAttempt,
  previous: StoredDistributionAttempt,
): Promise<StoredDistributionAttempt> {
  const restoredRecord = { ...previous.record, state: "active" as const };
  const restored = await putDistributionAttempt(
    bucket,
    identity,
    workflowId,
    restoredRecord,
    { etagMatches: claimed.etag },
  );
  if (restored) return restored;
  const winner = await readDistributionAttempt(bucket, identity, workflowId);
  if (!winner) throw new Error(`Distribution restart restore lost its fence for ${workflowId}`);
  return winner;
}

async function waitForDistributionAttempt(
  bucket: R2Bucket,
  identity: DistributionAttemptIdentity,
  workflowId: string,
): Promise<StoredDistributionAttempt> {
  for (let attempt = 0; attempt < 40; attempt += 1) {
    const current = await readDistributionAttempt(bucket, identity, workflowId);
    if (
      current && (current.record.state === "active" ||
        current.record.state === "create_failed" ||
        current.record.state === "restart_failed")
    ) return current;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`Distribution restart remains in progress for ${workflowId}`);
}

async function readDistributionAttempt(
  bucket: R2Bucket,
  identity: DistributionAttemptIdentity,
  workflowId: string,
): Promise<StoredDistributionAttempt | undefined> {
  const key = `${distributionPrefix(identity)}/request.json`;
  const object = await bucket.get(key);
  if (!object) return undefined;
  if (object.size <= 0 || object.size > MAX_DISTRIBUTION_RESULT_BYTES) {
    await object.body.cancel().catch(() => undefined);
    throw new Error(`Distribution request at ${key} is not bounded`);
  }
  const value = await object.json().catch(() => undefined);
  if (!validDistributionAttempt(value, identity, workflowId) || !object.etag) {
    throw new Error(`Distribution request identity is invalid for ${workflowId}`);
  }
  return { record: value, etag: object.etag };
}

async function putDistributionAttempt(
  bucket: R2Bucket,
  identity: DistributionAttemptIdentity,
  workflowId: string,
  attempt: DistributionAttemptRecord,
  onlyIf: R2Conditional,
): Promise<StoredDistributionAttempt | undefined> {
  const key = `${distributionPrefix(identity)}/request.json`;
  let stored: R2Object | null;
  try {
    stored = await bucket.put(key, JSON.stringify(attempt), {
      onlyIf,
      httpMetadata: { contentType: "application/json" },
    });
  } catch (cause) {
    // R2 conditional PUT is the execution fence. A committed write whose
    // acknowledgement was lost must be treated exactly like its successful
    // response, otherwise the six-hour execution claim strands the workflow
    // side effect that this record authorizes.
    const reconciled = await readDistributionAttempt(bucket, identity, workflowId)
      .catch(() => undefined);
    if (reconciled && canonicalJson(reconciled.record) === canonicalJson(attempt)) {
      return reconciled;
    }
    throw cause;
  }
  if (!stored) return undefined;
  if (!stored.etag) throw new Error(`Distribution request write has no fence for ${workflowId}`);
  return { record: attempt, etag: stored.etag };
}

async function boundedBucketJson(bucket: R2Bucket, key: string): Promise<unknown> {
  const object = await bucket.get(key);
  if (!object) return undefined;
  if (object.size <= 0 || object.size > MAX_DISTRIBUTION_RESULT_BYTES) {
    await object.body.cancel().catch(() => undefined);
    throw new Error(`Distribution evidence at ${key} is not bounded`);
  }
  const value = await object.json().catch(() => undefined);
  if (value === undefined) throw new Error(`Distribution evidence at ${key} is invalid`);
  return value;
}

function distributionPrefix(identity: DistributionAttemptIdentity): string {
  return identity.channel === "stable"
    ? `distribution/stable/${identity.tagName}`
    : `distribution/commit/${identity.head}`;
}

function validDistributionAttempt(
  value: unknown,
  identity: DistributionAttemptIdentity,
  workflowId: string,
): value is DistributionAttemptRecord {
  return record(value) && hasExactKeys(value, [
    "buildTimestamp",
    "channel",
    "claimExpiresAt",
    "head",
    "requestId",
    "requestedAt",
    "state",
    "tagName",
    "version",
    "workflowId",
  ]) && value.version === 1 && value.channel === identity.channel &&
    value.tagName === identity.tagName && value.head === identity.head &&
    value.buildTimestamp === identity.buildTimestamp && value.workflowId === workflowId &&
    (value.state === "creating" || value.state === "create_executing" ||
      value.state === "active" ||
      value.state === "restarting" || value.state === "restart_executing" ||
      value.state === "create_failed" ||
      value.state === "restart_failed") &&
    typeof value.requestId === "string" && UUID_V4.test(value.requestId) &&
    validTimestamp(value.requestedAt) && validTimestamp(value.claimExpiresAt) &&
    Date.parse(value.claimExpiresAt) > Date.parse(value.requestedAt) &&
    Date.parse(value.claimExpiresAt) - Date.parse(value.requestedAt) <=
      DISTRIBUTION_EXECUTION_CLAIM_MS + DISTRIBUTION_ATTEMPT_CLAIM_MS;
}

function validDistributionResultIdentity(
  value: unknown,
  identity: DistributionAttemptIdentity,
  workflowId: string,
): value is Record<string, unknown> {
  return record(value) && value.version === 1 && value.channel === identity.channel &&
    value.tagName === identity.tagName && value.head === identity.head &&
    value.workflowId === workflowId && typeof value.status === "string";
}

function terminalWorkflowStatus(status: string): boolean {
  return status === "complete" || status === "errored" || status === "terminated";
}

function distributionClaimActive(attempt: DistributionAttemptRecord): boolean {
  return Date.parse(attempt.claimExpiresAt) > Date.now();
}

async function acquireDistributionLease(
  repository: DurableObjectNamespace,
  head: string,
  workflowId: string,
): Promise<CiDistributionLease | Response> {
  const response = await repository.get(repository.idFromName("nanocodex")).fetch(
    `https://ci-repository/leases/distribution/${head}`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ workflowId }),
    },
  );
  if (response.status !== 200 && response.status !== 201) {
    const headers = new Headers(response.headers);
    for (const [name, value] of Object.entries(noStoreHeaders())) headers.set(name, value);
    return new Response(response.body, { status: response.status, headers });
  }
  const value = await response.json().catch(() => undefined);
  const leaseValue = record(value) && hasExactKeys(value, ["lease"])
    ? value.lease
    : undefined;
  const lease = record(leaseValue) && hasExactKeys(leaseValue, [
      "acquiredAt",
      "expiresAt",
      "head",
      "kind",
      "leaseId",
      "version",
      "workflowId",
    ])
    ? leaseValue as CiDistributionLease
    : undefined;
  const acquiredAt = lease ? Date.parse(lease.acquiredAt) : Number.NaN;
  const expiresAt = lease ? Date.parse(lease.expiresAt) : Number.NaN;
  if (
    !lease || lease.version !== 1 || lease.kind !== "distribution" ||
    !UUID_V4.test(lease.leaseId) || lease.head !== head ||
    lease.workflowId !== workflowId || !Number.isFinite(acquiredAt) ||
    !Number.isFinite(expiresAt) || acquiredAt > Date.now() || expiresAt <= Date.now() ||
    expiresAt <= acquiredAt ||
    expiresAt - acquiredAt > MAX_DISTRIBUTION_LEASE_MS
  ) return error("ci_distribution_lease_invalid", 503);
  return lease;
}

async function releaseManifest(
  namespace: DurableObjectNamespace,
  kind: "stable" | "commit",
  id: string,
): Promise<Record<string, unknown> | undefined> {
  const response = await releaseStub(namespace).fetch(
    `https://ci-releases/releases/${kind}/${id}`,
  );
  if (response.status === 404) return undefined;
  if (!response.ok) throw new Error(`Failed to read release ${kind}/${id}`);
  return response.json<Record<string, unknown>>();
}

async function exactStablePublication(
  namespace: DurableObjectNamespace,
  tagName: string,
  head: string,
  observedManifest?: Record<string, unknown>,
): Promise<PublicStablePublication | undefined> {
  const manifestValue = observedManifest ??
    await releaseManifest(namespace, "stable", tagName);
  const latest = await releaseChannel(namespace, "latest");
  if (
    !validPublicStableManifest(manifestValue, tagName, head) ||
    !record(latest) || !hasExactKeys(latest, ["manifest", "pointer"]) ||
    !validPublicStablePointer(latest.pointer, tagName, head) ||
    canonicalJson(latest.manifest) !== canonicalJson(manifestValue)
  ) return undefined;
  const { manifestSha256, ...unsigned } = manifestValue;
  if (await jsonSha256(unsigned) !== manifestSha256) return undefined;
  return {
    manifest: manifestValue,
    pointer: latest.pointer,
  };
}

async function exactNightlyPublication(
  namespace: DurableObjectNamespace,
  head: string,
  observedChannel?: Record<string, unknown>,
): Promise<PublicCommitPublication | undefined> {
  const manifestValue = await releaseManifest(namespace, "commit", head);
  const nightly = observedChannel ?? await releaseChannel(namespace, "nightly");
  if (
    !validPublicCommitManifest(manifestValue, head) ||
    !record(nightly) || !hasExactKeys(nightly, ["manifest", "pointer"]) ||
    !validPublicCommitPointer(nightly.pointer, head) ||
    canonicalJson(nightly.manifest) !== canonicalJson(manifestValue)
  ) return undefined;
  const { manifestSha256, ...unsigned } = manifestValue;
  if (await jsonSha256(unsigned) !== manifestSha256) return undefined;
  return {
    manifest: manifestValue,
    pointer: nightly.pointer,
  };
}

function validPublicStableManifest(
  value: unknown,
  tagName: string,
  head: string,
): value is PublicStableManifest {
  if (
    !record(value) || !hasExactKeys(value, [
      "assets",
      "channel",
      "commit",
      "finalizedAt",
      "id",
      "kind",
      "manifestSha256",
      "tag",
      "version",
    ]) || value.version !== 1 || value.kind !== "stable" ||
    value.id !== tagName || value.tag !== tagName || value.commit !== head ||
    value.channel !== "latest" || !validTimestamp(value.finalizedAt) ||
    typeof value.manifestSha256 !== "string" || !SHA256.test(value.manifestSha256) ||
    !Array.isArray(value.assets) || value.assets.length > 64
  ) return false;
  return validPublicReleaseAssets(value.assets, "stable", tagName);
}

function validPublicCommitManifest(
  value: unknown,
  head: string,
): value is PublicCommitManifest {
  if (
    !record(value) || !hasExactKeys(value, [
      "assets",
      "channel",
      "commit",
      "finalizedAt",
      "id",
      "kind",
      "manifestSha256",
      "tag",
      "version",
    ]) || value.version !== 1 || value.kind !== "commit" ||
    value.id !== head || value.tag !== `nightly-${head}` || value.commit !== head ||
    value.channel !== "nightly" || !validTimestamp(value.finalizedAt) ||
    typeof value.manifestSha256 !== "string" || !SHA256.test(value.manifestSha256) ||
    !Array.isArray(value.assets) || value.assets.length > 64
  ) return false;
  return validPublicReleaseAssets(value.assets, "commit", head);
}

function validPublicReleaseAssets(
  assets: unknown[],
  kind: "stable" | "commit",
  id: string,
): assets is PublicReleaseAsset[] {
  const names = new Set<string>();
  for (const asset of assets) {
    if (
      !record(asset) || !hasExactKeys(asset, [
        "contentType",
        "downloadPath",
        "name",
        "platform",
        "sha256",
        "size",
      ]) || typeof asset.name !== "string" || !RELEASE_ASSET_NAME.test(asset.name) ||
      names.has(asset.name) || typeof asset.platform !== "string" || asset.platform.length === 0 ||
      !Number.isSafeInteger(asset.size) || Number(asset.size) <= 0 ||
      typeof asset.sha256 !== "string" || !SHA256.test(asset.sha256) ||
      typeof asset.contentType !== "string" || !RELEASE_CONTENT_TYPE.test(asset.contentType) ||
      asset.downloadPath !==
        `/api/releases/releases/${kind}/${id}/assets/${encodeURIComponent(asset.name)}`
    ) return false;
    names.add(asset.name);
  }
  return true;
}

function validPublicStablePointer(
  value: unknown,
  tagName: string,
  head: string,
): value is PublicStablePointer {
  return record(value) && hasExactKeys(value, [
    "channel",
    "commit",
    "generation",
    "id",
    "kind",
    "tag",
    "updatedAt",
    "version",
  ]) && value.version === 1 && value.channel === "latest" && value.kind === "stable" &&
    value.id === tagName && value.tag === tagName && value.commit === head &&
    Number.isSafeInteger(value.generation) && Number(value.generation) > 0 &&
    validTimestamp(value.updatedAt);
}

function validPublicCommitPointer(
  value: unknown,
  head: string,
): value is PublicCommitPointer {
  return record(value) && hasExactKeys(value, [
    "channel",
    "commit",
    "generation",
    "id",
    "kind",
    "tag",
    "updatedAt",
    "version",
  ]) && value.version === 1 && value.channel === "nightly" && value.kind === "commit" &&
    value.id === head && value.tag === `nightly-${head}` && value.commit === head &&
    Number.isSafeInteger(value.generation) && Number(value.generation) > 0 &&
    validTimestamp(value.updatedAt);
}

function exactStablePrepared(
  value: StableDistributionState | undefined,
  tagName: string,
  head: string,
): value is StableDistributionState {
  return value?.version === 1 && (value.status === "ready" || value.status === "success") &&
    value.channel === "stable" && value.tagName === tagName && value.head === head &&
    value.workflowId === `release-${tagName}-${head}`;
}

async function persistStableReleaseSuccess(
  bucket: R2Bucket,
  tagName: string,
  prepared: StableDistributionState,
  publication: PublicStablePublication,
): Promise<void> {
  await bucket.put(
    `distribution/stable/${tagName}/result.json`,
    JSON.stringify({
      ...prepared,
      status: "success",
      finalizedAt: publication.manifest.finalizedAt,
      publication,
    }),
    { httpMetadata: { contentType: "application/json" } },
  );
}

async function reconcileNightlyReleaseSuccess(
  configured: ConfiguredEnv,
  head: string,
  observedChannel: Record<string, unknown> | undefined,
): Promise<Response | undefined> {
  const publication = await exactNightlyPublication(
    configured.CI_RELEASES,
    head,
    observedChannel,
  );
  if (!publication) return error("nightly_release_state_invalid", 503);
  await configured.BACKUP_BUCKET.put(
    `distribution/commit/${head}/result.json`,
    JSON.stringify({
      version: 1,
      status: "success",
      channel: "nightly",
      tagName: "nightly",
      head,
      workflowId: `nightly-${head}`,
      completedAt: publication.manifest.finalizedAt,
      publication,
    }),
    { httpMetadata: { contentType: "application/json" } },
  );
  return undefined;
}

async function releaseChannel(
  namespace: DurableObjectNamespace,
  channel: "latest" | "nightly",
): Promise<Record<string, unknown> | undefined> {
  const response = await releaseStub(namespace).fetch(
    `https://ci-releases/channels/${channel}`,
  );
  if (response.status === 404) return undefined;
  if (!response.ok) throw new Error(`Failed to read release channel ${channel}`);
  return response.json();
}

function releaseChannelCommit(value: unknown): unknown {
  return record(value) && record(value.pointer) ? value.pointer.commit : undefined;
}

async function jsonSha256(value: unknown): Promise<string> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(canonicalJson(value)),
  );
  return [...new Uint8Array(digest)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

function canonicalJson(value: unknown): string {
  if (value === null || typeof value === "boolean" || typeof value === "string") {
    return JSON.stringify(value);
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new TypeError("canonical JSON requires finite numbers");
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (record(value)) {
    return `{${Object.keys(value).sort().map((key) =>
      `${JSON.stringify(key)}:${canonicalJson(value[key])}`
    ).join(",")}}`;
  }
  throw new TypeError("canonical JSON requires JSON values");
}

async function stableDistributionState(
  bucket: R2Bucket,
  tagName: string,
): Promise<StableDistributionState | undefined> {
  const object = await bucket.get(`distribution/stable/${tagName}/result.json`);
  return object?.json<StableDistributionState>().catch(() => undefined);
}

function releaseStub(namespace: DurableObjectNamespace) {
  return namespace.get(namespace.idFromName("nanocodex"));
}

function stablePublicationFence(request: Request): Record<string, string> | Response {
  const leaseId = request.headers.get("x-nanocodex-publication-lease-id");
  const owner = request.headers.get("x-nanocodex-publication-lease-owner");
  const generation = request.headers.get("x-nanocodex-publication-lease-generation");
  const parsedGeneration = generation == null ? Number.NaN : Number(generation);
  if (
    !leaseId || !PUBLICATION_LEASE_ID.test(leaseId) ||
    !owner || !PUBLICATION_LEASE_OWNER.test(owner) ||
    !generation || !/^[1-9][0-9]{0,15}$/.test(generation) ||
    !Number.isSafeInteger(parsedGeneration) || String(parsedGeneration) !== generation ||
    !leaseId.startsWith(`${generation}.`)
  ) return error("invalid_publication_lease", 400);
  return {
    "x-nanocodex-publication-lease-id": leaseId,
    "x-nanocodex-publication-lease-owner": owner,
    "x-nanocodex-publication-lease-generation": generation,
  };
}

async function optionalHead(request: Request): Promise<string | null | Response> {
  if (request.body == null) return null;
  const value = await request.json().catch(() => undefined) as { head?: unknown } | undefined;
  if (!value || Object.keys(value).length !== 1 || !("head" in value)) {
    return error("invalid_release_request", 400);
  }
  return typeof value.head === "string" && SHA1.test(value.head)
    ? value.head
    : error("invalid_release_head", 400);
}

function authenticate(request: Request, expected: string | undefined): boolean {
  const header = request.headers.get("authorization");
  if (!expected || !header?.startsWith("Bearer ")) return false;
  const actual = header.slice("Bearer ".length);
  if (actual.length !== expected.length) return false;
  let mismatch = 0;
  for (let index = 0; index < actual.length; index += 1) {
    mismatch |= actual.charCodeAt(index) ^ expected.charCodeAt(index);
  }
  return mismatch === 0;
}

function unauthorized(): Response {
  return Response.json({ error: "unauthorized" }, {
    status: 401,
    headers: { ...noStoreHeaders(), "www-authenticate": "Bearer" },
  });
}

function error(message: string, status: number): Response {
  return Response.json({ error: message }, { status, headers: noStoreHeaders() });
}

function noStoreHeaders() {
  return { "cache-control": "no-store", "x-content-type-options": "nosniff" };
}

function record(value: unknown): value is Record<string, unknown> {
  return value != null && typeof value === "object" && !Array.isArray(value);
}

function hasExactKeys(value: Record<string, unknown>, expected: string[]): boolean {
  const keys = Object.keys(value).sort();
  return keys.length === expected.length &&
    keys.every((key, index) => key === expected[index]);
}

function validTimestamp(value: unknown): value is string {
  return typeof value === "string" && Number.isFinite(Date.parse(value));
}
