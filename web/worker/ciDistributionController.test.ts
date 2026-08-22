import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { test } from "node:test";

import {
  dispatchNightlyDistribution,
  routeCiDistributionControl,
  type CiDistributionControlEnv,
} from "./ciDistributionController.ts";
import type { CiRunRecord } from "./ciRepository.ts";
import type { CiSourcePublication, NanocodexCiParams } from "./ciSource.ts";

const HEAD = "a".repeat(40);
const TOKEN = "release-control-token";
const PUBLICATION_FENCE_HEADERS = {
  "x-nanocodex-publication-lease-id": "7.00000000-0000-4000-8000-000000000007",
  "x-nanocodex-publication-lease-owner": "release-controller:test",
  "x-nanocodex-publication-lease-generation": "7",
};

test("stable release control authenticates before reading CI state", async () => {
  const response = await routeCiDistributionControl(
    new Request("https://ci.example/api/ci/releases/stable/v1.2.3", {
      method: "POST",
    }),
    {},
    new URL("https://ci.example/api/ci/releases/stable/v1.2.3"),
  );
  assert.equal(response?.status, 401);
  assert.deepEqual(await response?.json(), { error: "unauthorized" });
});

test("manual nightly control authenticates and requires an exact optional head body", async () => {
  const memory = controlEnvironment();
  const unauthorized = await nightlyRequest(memory.env, undefined, false);
  assert.equal(unauthorized.status, 401);
  assert.equal(memory.leaseRequests.length, 0);

  for (const [body, error] of [
    [{}, "invalid_release_request"],
    [{ head: "not-a-sha" }, "invalid_release_head"],
    [{ head: HEAD, extra: true }, "invalid_release_request"],
  ] as const) {
    const malformed = await nightlyRequest(memory.env, body);
    assert.equal(malformed.status, 400);
    assert.deepEqual(await malformed.json(), { error });
  }
  assert.equal(memory.created.length, 0);
  assert.equal(memory.leaseRequests.length, 0);
});

test("stable release queues an exact retained green master publication", async () => {
  const memory = controlEnvironment();
  const staleHead = "b".repeat(40);
  const stale = await stableRequest(memory.env, { head: staleHead });
  assert.equal(stale.status, 202);
  assert.equal(memory.created[0]!.id, `release-v1.2.3-${staleHead}`);
  assert.equal(memory.created[0]!.params.sha, staleHead);

  const missing = controlEnvironment({ missingRetainedHead: true });
  const notRetained = await stableRequest(missing.env, { head: staleHead });
  assert.equal(notRetained.status, 409);
  assert.deepEqual(await notRetained.json(), { error: "release_head_not_retained" });
  assert.equal(missing.created.length, 0);

  const current = controlEnvironment();
  const accepted = await stableRequest(current.env, { head: HEAD });
  assert.equal(accepted.status, 202);
  assert.equal(current.created.length, 1);
  assert.equal(current.created[0]!.id, `release-v1.2.3-${HEAD}`);
  assert.deepEqual(current.created[0]!.params.providerData.distribution, {
    version: 1,
    channel: "stable",
    tagName: "v1.2.3",
    buildTimestamp: "2026-08-22T01:02:03.000Z",
  });
  assert.equal(current.created[0]!.params.sha, HEAD);
  assert.equal(current.created[0]!.params.beforeSha, undefined);
  assert.deepEqual(current.created[0]!.retention, {
    successRetention: "30 days",
    errorRetention: "30 days",
  });
  assert.deepEqual(current.events.slice(-2), ["lease", "workflow"]);
});

test("stable release rejects a retained pull-request publication", async () => {
  const memory = controlEnvironment({ retainedLane: "pull_request" });
  const response = await stableRequest(memory.env, { head: "b".repeat(40) });
  assert.equal(response.status, 503);
  assert.deepEqual(await response.json(), { error: "ci_repository_state_invalid" });
  assert.equal(memory.created.length, 0);
});

test("a completed distribution with no immutable release is safely restarted", async () => {
  const memory = controlEnvironment({
    duplicateStatus: "complete",
    failedDistribution: "stable",
  });
  const response = await stableRequest(memory.env);
  assert.equal(response.status, 202);
  assert.equal(memory.restarts, 1);
  const body = await response.json() as { workflow: string; requestId: string };
  assert.equal(body.workflow, "restarted");
  assert.match(body.requestId, /^[a-f0-9-]{36}$/);
  assert.deepEqual(memory.restartSnapshots, [{
    status: "running",
    head: HEAD,
    workflowId: `release-v1.2.3-${HEAD}`,
    requestId: body.requestId,
  }]);
  assert.equal(
    memory.bucketValue("distribution/stable/v1.2.3/request.json")?.requestId,
    body.requestId,
  );
});

test("stable distribution stops ready and finalizes only through authenticated control", async () => {
  const memory = controlEnvironment({ distributionReady: true });
  const ready = await stableRequest(memory.env);
  assert.equal(ready.status, 200);
  assert.equal((await ready.json() as { status: string }).status, "ready");
  assert.equal(memory.created.length, 0);

  const missingFence = await stableRequest(memory.env, undefined, true, {});
  assert.equal(missingFence.status, 400);
  assert.deepEqual(await missingFence.json(), { error: "invalid_publication_lease" });
  assert.equal(memory.finalizations, 0);

  const malformedFence = await stableRequest(memory.env, undefined, true, {
    ...PUBLICATION_FENCE_HEADERS,
    "x-nanocodex-publication-lease-generation": "07",
  });
  assert.equal(malformedFence.status, 400);
  assert.equal(memory.finalizations, 0);

  const finalized = await stableRequest(memory.env, undefined, true);
  assert.equal(finalized.status, 200);
  assert.equal((await finalized.json() as { status: string }).status, "released");
  assert.equal(memory.finalizations, 1);
  assert.equal(memory.persistedDistribution?.status, "success");
  assert.deepEqual(memory.finalizeHeaders, PUBLICATION_FENCE_HEADERS);
});

test("stable finalize replay repairs success evidence after a lost R2 acknowledgement", async () => {
  const memory = controlEnvironment({
    distributionReady: true,
    failStableResultPutAfterFinalize: true,
  });
  await assert.rejects(
    stableRequest(memory.env, undefined, true),
    /injected stable result write failure/,
  );
  assert.equal(memory.finalizations, 1);
  assert.equal(
    memory.bucketValue("distribution/stable/v1.2.3/result.json")?.status,
    "ready",
  );

  const replay = await stableRequest(memory.env, undefined, true);
  assert.equal(replay.status, 200);
  const replayed = await replay.json() as {
    status: string;
    publication: { manifest: { commit: string }; pointer: { commit: string } };
  };
  assert.equal(replayed.status, "released");
  assert.equal(replayed.publication.manifest.commit, HEAD);
  assert.equal(replayed.publication.pointer.commit, HEAD);
  assert.equal(memory.finalizations, 1, "the immutable release is not finalized twice");
  assert.equal(memory.persistedDistribution?.status, "success");
  assert.equal(memory.persistedDistribution?.finalizedAt, "2026-08-22T03:00:00.000Z");
});

test("nightly dispatch is idempotent by the rolling pointer and green head", async () => {
  const current = controlEnvironment({ nightlyCommit: HEAD });
  assert.deepEqual(await dispatchNightlyDistribution(current.env), {
    status: "current",
    head: HEAD,
  });
  assert.equal(current.created.length, 0);

  const pending = controlEnvironment();
  const accepted = await dispatchNightlyDistribution(pending.env);
  assert.equal(accepted.status, "accepted");
  assert.equal(accepted.head, HEAD);
  assert.equal(accepted.workflowId, `nightly-${HEAD}`);
  assert.match(accepted.requestId ?? "", /^[a-f0-9-]{36}$/);
  assert.deepEqual(pending.leaseRequests, [{
    head: HEAD,
    workflowId: `nightly-${HEAD}`,
  }]);
  assert.equal(pending.created[0]!.params.providerData.distribution?.channel, "nightly");
  assert.equal(pending.created[0]!.params.providerData.distribution?.tagName, "nightly");
  assert.deepEqual(pending.events, ["lease", "workflow"]);
});

test("current nightly publication repairs failure evidence after a lost acknowledgement", async () => {
  const memory = controlEnvironment({
    nightlyCommit: HEAD,
    failedDistribution: "nightly",
  });
  const response = await nightlyRequest(memory.env);
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { status: "current", head: HEAD });
  assert.equal(memory.created.length, 0);
  assert.equal(memory.restarts, 0);
  assert.deepEqual(
    pick(
      memory.bucketValue(`distribution/commit/${HEAD}/result.json`) ?? {},
      ["status", "channel", "tagName", "head", "workflowId", "completedAt"],
    ),
    {
      status: "success",
      channel: "nightly",
      tagName: "nightly",
      head: HEAD,
      workflowId: `nightly-${HEAD}`,
      completedAt: "2026-08-22T03:00:00.000Z",
    },
  );
  assert.deepEqual(
    memory.bucketValue(`distribution/commit/${HEAD}/result.json`)?.publication,
    commitPublication(HEAD),
  );
});

test("manual and cron nightly share current-head, stale, replay, restart, and race semantics", async () => {
  const alreadyCurrent = controlEnvironment({ nightlyCommit: HEAD });
  const current = await nightlyRequest(alreadyCurrent.env);
  assert.equal(current.status, 200);
  assert.deepEqual(await current.json(), { status: "current", head: HEAD });
  assert.equal(alreadyCurrent.leaseRequests.length, 0);

  const retainedHead = "b".repeat(40);
  const stale = controlEnvironment();
  const staleResponse = await nightlyRequest(stale.env, { head: retainedHead });
  assert.equal(staleResponse.status, 409);
  assert.deepEqual(await staleResponse.json(), { error: "ci_head_changed" });
  assert.equal(stale.leaseRequests.length, 0);
  assert.equal(stale.created.length, 0);

  const replay = controlEnvironment({ duplicateStatus: "running" });
  const replayed = await nightlyRequest(replay.env);
  assert.equal(replayed.status, 202);
  assert.deepEqual(
    pick(await replayed.json() as Record<string, unknown>, ["status", "workflowId"]),
    { status: "accepted", workflowId: `nightly-${HEAD}` },
  );
  assert.equal(replay.restarts, 0);

  const terminal = controlEnvironment({
    duplicateStatus: "complete",
    failedDistribution: "nightly",
  });
  const restarted = await nightlyRequest(terminal.env);
  assert.equal(restarted.status, 202);
  const restartedBody = await restarted.json() as Record<string, unknown>;
  assert.deepEqual(
    pick(restartedBody, ["status", "workflowId"]),
    { status: "restarted", workflowId: `nightly-${HEAD}` },
  );
  assert.match(String(restartedBody.requestId), /^[a-f0-9-]{36}$/);
  assert.equal(terminal.restarts, 1);
  assert.deepEqual(terminal.restartSnapshots, [{
    status: "running",
    head: HEAD,
    workflowId: `nightly-${HEAD}`,
    requestId: restartedBody.requestId,
  }]);

  const raced = controlEnvironment({ currentHeadChangesAfterLease: true });
  const changed = await nightlyRequest(raced.env);
  assert.equal(changed.status, 409);
  assert.deepEqual(await changed.json(), { error: "ci_head_changed" });
  assert.equal(raced.created.length, 0);
  assert.equal(raced.leaseRequests.length, 1);

  const pointerRace = controlEnvironment({ nightlyCommitAfterLease: HEAD });
  const becameCurrent = await nightlyRequest(pointerRace.env);
  assert.equal(becameCurrent.status, 200);
  assert.deepEqual(await becameCurrent.json(), { status: "current", head: HEAD });
  assert.equal(pointerRace.created.length, 0);
  assert.equal(pointerRace.leaseRequests.length, 1);

  const invalidLease = controlEnvironment({ invalidLease: true });
  const rejected = await nightlyRequest(invalidLease.env);
  assert.equal(rejected.status, 503);
  assert.deepEqual(await rejected.json(), { error: "ci_distribution_lease_invalid" });
  assert.equal(invalidLease.created.length, 0);
});

test("concurrent manual and cron creation share one conditional request identity", async () => {
  const requestBarrier = barrier(2);
  const memory = controlEnvironment({ beforeRequestRead: requestBarrier });
  const [manualResponse, cron] = await Promise.all([
    nightlyRequest(memory.env),
    dispatchNightlyDistribution(memory.env),
  ]);
  const manual = await manualResponse.json() as {
    status: string;
    requestId: string;
  };
  assert.equal(manual.status, "accepted");
  assert.equal(cron.status, "accepted");
  assert.equal(manual.requestId, cron.requestId);
  assert.match(manual.requestId, /^[a-f0-9-]{36}$/);
  assert.equal(memory.created.length, 1);
  assert.equal(memory.createCalls, 1);
  assert.equal(memory.restarts, 0);
  assert.equal(
    memory.bucketValue(`distribution/commit/${HEAD}/request.json`)?.requestId,
    manual.requestId,
  );
});

test("lost execution-fence acknowledgements reconcile before create and restart side effects", async () => {
  const create = controlEnvironment({
    failRequestPutAfterCommitState: "create_executing",
  });
  const createdResponse = await nightlyRequest(create.env);
  const created = await createdResponse.json() as { status: string; requestId: string };
  assert.equal(created.status, "accepted");
  assert.equal(create.createCalls, 1);
  assert.equal(create.created.length, 1);
  assert.equal(
    create.bucketValue(`distribution/commit/${HEAD}/request.json`)?.state,
    "active",
  );

  const restart = controlEnvironment({
    duplicateStatus: "complete",
    failedDistribution: "nightly",
    failRequestPutAfterCommitState: "restart_executing",
  });
  const restartedResponse = await nightlyRequest(restart.env);
  const restarted = await restartedResponse.json() as { status: string; requestId: string };
  assert.equal(restarted.status, "restarted");
  assert.equal(restart.restartCalls, 1);
  assert.equal(restart.restarts, 1);
  assert.equal(
    restart.bucketValue(`distribution/commit/${HEAD}/request.json`)?.state,
    "active",
  );
});

test("a live create owner remains fenced after its original claim age passes", async () => {
  const originalNow = Date.now;
  let now = originalNow() + 1_000;
  Date.now = () => now;
  try {
    const createEntered = deferred();
    const followerSawExecution = deferred();
    const releaseCreate = deferred();
    let observeFollower = false;
    const memory = controlEnvironment({
      beforeCreate: async () => {
        createEntered.resolve();
        await releaseCreate.promise;
      },
      afterRequestRead: async (value) => {
        if (observeFollower && value?.state === "create_executing") {
          followerSawExecution.resolve();
        }
      },
      statusUnknownOnceAfterCreate: true,
    });
    const owner = nightlyRequest(memory.env);
    await createEntered.promise;
    const canonicalRequestId = String(
      memory.bucketValue(`distribution/commit/${HEAD}/request.json`)?.requestId,
    );
    now += 31_000;
    observeFollower = true;
    const follower = dispatchNightlyDistribution(memory.env);
    await followerSawExecution.promise;
    releaseCreate.resolve();

    const [ownerResponse, followerResult] = await Promise.all([owner, follower]);
    const ownerResult = await ownerResponse.json() as { status: string; requestId: string };
    assert.equal(ownerResult.status, "accepted");
    assert.equal(followerResult.status, "accepted");
    assert.equal(ownerResult.requestId, canonicalRequestId);
    assert.equal(followerResult.requestId, canonicalRequestId);
    assert.equal(memory.createCalls, 1);
    assert.equal(memory.created.length, 1);
  } finally {
    Date.now = originalNow;
  }
});

test("concurrent terminal dispatches fence one restart and return its canonical request", async () => {
  const entered = deferred();
  const release = deferred();
  const memory = controlEnvironment({
    duplicateStatus: "complete",
    failedDistribution: "nightly",
    beforeRestart: async () => {
      entered.resolve();
      await release.promise;
    },
  });
  const manualPromise = nightlyRequest(memory.env);
  const cronPromise = dispatchNightlyDistribution(memory.env);
  await entered.promise;
  release.resolve();
  const [manualResponse, cron] = await Promise.all([manualPromise, cronPromise]);
  const manual = await manualResponse.json() as {
    status: string;
    requestId: string;
  };
  assert.deepEqual(
    [manual.status, cron.status].sort(),
    ["accepted", "restarted"],
  );
  assert.equal(manual.requestId, cron.requestId);
  assert.equal(memory.restarts, 1);
  assert.equal(memory.restartSnapshots.length, 1);
  assert.equal(memory.restartSnapshots[0]?.requestId, manual.requestId);
  assert.equal(
    memory.bucketValue(`distribution/commit/${HEAD}/request.json`)?.state,
    "active",
  );
});

test("expired create and ambiguous restart claims are taken over with a new request", async () => {
  const staleRequestedAt = new Date(Date.now() - 60_000).toISOString();
  const staleClaimExpiresAt = new Date(Date.parse(staleRequestedAt) + 30_000).toISOString();
  const staleRequestId = "00000000-0000-4000-8000-000000000099";
  const staleCreating = controlEnvironment();
  staleCreating.seedBucket(`distribution/commit/${HEAD}/request.json`, {
    version: 1,
    state: "creating",
    requestId: staleRequestId,
    workflowId: `nightly-${HEAD}`,
    requestedAt: staleRequestedAt,
    claimExpiresAt: staleClaimExpiresAt,
    channel: "nightly",
    tagName: "nightly",
    head: HEAD,
    buildTimestamp: "2026-08-22T01:02:03.000Z",
  });

  const createdResponse = await nightlyRequest(staleCreating.env);
  const created = await createdResponse.json() as { status: string; requestId: string };
  assert.equal(created.status, "accepted");
  assert.notEqual(created.requestId, staleRequestId);
  assert.equal(staleCreating.createCalls, 1);
  assert.equal(
    staleCreating.bucketValue(`distribution/commit/${HEAD}/request.json`)?.state,
    "active",
  );

  const ambiguous = controlEnvironment({
    duplicateStatus: "complete",
    failedDistribution: "nightly",
    restartFailure: "unknown",
  });
  await assert.rejects(nightlyRequest(ambiguous.env), /injected restart failure/);
  const abandoned = ambiguous.bucketValue(`distribution/commit/${HEAD}/request.json`);
  assert.equal(abandoned?.state, "restart_executing");
  const abandonedRequestId = String(abandoned?.requestId);
  ambiguous.seedBucket(`distribution/commit/${HEAD}/request.json`, {
    ...abandoned,
    requestedAt: staleRequestedAt,
    claimExpiresAt: staleClaimExpiresAt,
  });
  ambiguous.setWorkflowState("complete");

  const restartedResponse = await nightlyRequest(ambiguous.env);
  const restarted = await restartedResponse.json() as { status: string; requestId: string };
  assert.equal(restarted.status, "restarted");
  assert.notEqual(restarted.requestId, abandonedRequestId);
  assert.equal(ambiguous.restartCalls, 2);
  assert.equal(ambiguous.restarts, 1);
  assert.equal(
    ambiguous.bucketValue(`distribution/commit/${HEAD}/request.json`)?.state,
    "active",
  );
});

test("a confirmed restart failure is fenced from concurrent followers", async () => {
  const restartEntered = deferred();
  const followerSawClaim = deferred();
  const releaseRestart = deferred();
  const memory = controlEnvironment({
    duplicateStatus: "complete",
    failedDistribution: "nightly",
    restartFailure: "terminal",
    beforeRestart: async () => {
      restartEntered.resolve();
      await releaseRestart.promise;
    },
    afterRequestRead: async (value) => {
      if (value?.state === "restart_executing") followerSawClaim.resolve();
    },
  });
  const manual = nightlyRequest(memory.env);
  const cron = dispatchNightlyDistribution(memory.env);
  await restartEntered.promise;
  await followerSawClaim.promise;
  releaseRestart.resolve();

  const outcomes = await Promise.allSettled([manual, cron]);
  assert.deepEqual(outcomes.map(({ status }) => status), ["rejected", "rejected"]);
  for (const outcome of outcomes) {
    assert.equal(outcome.status, "rejected");
    if (outcome.status === "rejected") {
      assert.match(String(outcome.reason), /restart|attempt failed/i);
    }
  }
  assert.equal(memory.restartCalls, 1);
  assert.equal(memory.restarts, 0);
  assert.equal(
    memory.bucketValue(`distribution/commit/${HEAD}/request.json`)?.state,
    "restart_failed",
  );
});

test("an executing restart keeps one fence when its original claim age passes", async () => {
  const originalNow = Date.now;
  let now = originalNow() + 1_000;
  Date.now = () => now;
  try {
    const resultReadEntered = deferred();
    const releaseResultRead = deferred();
    const followerSawExecution = deferred();
    let blockResultRead = true;
    const memory = controlEnvironment({
      duplicateStatus: "complete",
      failedDistribution: "nightly",
      beforeResultRead: async (key) => {
        if (blockResultRead && key === `distribution/commit/${HEAD}/result.json`) {
          blockResultRead = false;
          resultReadEntered.resolve();
          await releaseResultRead.promise;
        }
      },
      afterRequestRead: async (value) => {
        if (value?.state === "restart_executing") followerSawExecution.resolve();
      },
    });
    const owner = nightlyRequest(memory.env);
    const first = await Promise.race([
      resultReadEntered.promise.then(() => "result_read" as const),
      owner.then(() => "owner_completed" as const),
    ]);
    assert.equal(first, "result_read");
    const canonicalRequestId = String(
      memory.bucketValue(`distribution/commit/${HEAD}/request.json`)?.requestId,
    );
    now += 31_000;
    const follower = dispatchNightlyDistribution(memory.env);
    await followerSawExecution.promise;
    releaseResultRead.resolve();

    const [ownerResponse, followerResult] = await Promise.all([owner, follower]);
    const ownerResult = await ownerResponse.json() as { status: string; requestId: string };
    assert.deepEqual(
      [ownerResult.status, followerResult.status].sort(),
      ["accepted", "restarted"],
    );
    assert.equal(ownerResult.requestId, canonicalRequestId);
    assert.equal(followerResult.requestId, canonicalRequestId);
    assert.equal(memory.restartCalls, 1);
    assert.equal(memory.restarts, 1);
    assert.equal(memory.restartSnapshots[0]?.requestId, canonicalRequestId);
  } finally {
    Date.now = originalNow;
  }
});

function controlEnvironment(options: {
  duplicateStatus?: "complete" | "running";
  nightlyCommit?: string;
  nightlyCommitAfterLease?: string;
  releaseCommit?: string;
  distributionReady?: boolean;
  failedDistribution?: "stable" | "nightly";
  missingRetainedHead?: boolean;
  retainedLane?: "master" | "pull_request";
  currentHeadChangesAfterLease?: boolean;
  invalidLease?: boolean;
  beforeCreate?: () => Promise<void>;
  beforeRestart?: () => Promise<void>;
  beforeRequestRead?: () => Promise<void>;
  beforeResultRead?: (key: string) => Promise<void>;
  afterRequestRead?: (value: Record<string, unknown> | undefined) => Promise<void>;
  restartFailure?: "terminal" | "unknown";
  failStableResultPutAfterFinalize?: boolean;
  statusUnknownOnceAfterCreate?: boolean;
  failRequestPutAfterCommitState?: "create_executing" | "restart_executing";
} = {}) {
  const created: Array<{
    id: string;
    params: NanocodexCiParams;
    retention?: { successRetention: string; errorRetention: string };
  }> = [];
  let restarts = 0;
  let finalizations = 0;
  let finalizeHeaders: Record<string, string> | undefined;
  let persistedDistribution: Record<string, unknown> | undefined;
  const restartSnapshots: Array<Record<string, unknown>> = [];
  const bucketObjects = new Map<string, Record<string, unknown>>();
  const bucketEtags = new Map<string, string>();
  let nextBucketEtag = 1;
  let workflowState: "unknown" | "complete" | "running" =
    options.duplicateStatus ?? "unknown";
  let createCalls = 0;
  let restartCalls = 0;
  let restartFailure = options.restartFailure;
  let failStableResultPutAfterFinalize = options.failStableResultPutAfterFinalize === true;
  let releasedCommit = options.releaseCommit;
  let statusUnknownOnceAfterCreate = options.statusUnknownOnceAfterCreate === true;
  let failRequestPutAfterCommitState = options.failRequestPutAfterCommitState;
  const leaseRequests: Array<{ head: string; workflowId: string }> = [];
  const events: string[] = [];
  const publication = sourcePublication();
  const run: CiRunRecord = {
    version: 1,
    head: HEAD,
    beforeHead: null,
    workflowId: `ci-${HEAD}`,
    state: "dispatched",
    attempts: 1,
    publishedAt: publication.publishedAt,
  };
  if (options.distributionReady) {
    bucketObjects.set("distribution/stable/v1.2.3/result.json", {
      version: 1,
      status: "ready",
      channel: "stable",
      tagName: "v1.2.3",
      head: HEAD,
      workflowId: `release-v1.2.3-${HEAD}`,
    });
  }
  if (options.failedDistribution) {
    const stable = options.failedDistribution === "stable";
    const workflowId = stable ? `release-v1.2.3-${HEAD}` : `nightly-${HEAD}`;
    bucketObjects.set(
      stable
        ? "distribution/stable/v1.2.3/result.json"
        : `distribution/commit/${HEAD}/result.json`,
      {
        version: 1,
        status: "failure",
        channel: options.failedDistribution,
        tagName: stable ? "v1.2.3" : "nightly",
        head: HEAD,
        workflowId,
        completedAt: "2026-08-22T02:00:00.000Z",
        failure: { name: "Error", message: "old attempt failed" },
      },
    );
  }
  const repositoryStub = {
    fetch: async (request: Request | string, init?: RequestInit) => {
      const pathname = new URL(typeof request === "string" ? request : request.url).pathname;
      if (pathname.startsWith("/leases/distribution/")) {
        const head = pathname.slice(pathname.lastIndexOf("/") + 1);
        const body = JSON.parse(String(init?.body)) as { workflowId: string };
        leaseRequests.push({ head, workflowId: body.workflowId });
        events.push("lease");
        const acquiredAt = new Date().toISOString();
        return Response.json({
          lease: {
            version: 1,
            kind: "distribution",
            leaseId: options.invalidLease
              ? "not-a-lease-id"
              : "00000000-0000-4000-8000-000000000010",
            head,
            workflowId: body.workflowId,
            acquiredAt,
            expiresAt: new Date(Date.parse(acquiredAt) + 6 * 60 * 60 * 1_000).toISOString(),
          },
        }, { status: 201 });
      }
      if (pathname.startsWith("/master/publications/")) {
        if (options.missingRetainedHead) {
          return Response.json({ error: "not_found" }, { status: 404 });
        }
        const retainedHead = pathname.slice(pathname.lastIndexOf("/") + 1);
        const retainedPublication = sourcePublication(retainedHead);
        if (options.retainedLane === "pull_request") {
          return Response.json({
            publication: {
              ...retainedPublication,
              branch: "pull/7/merge",
              ref: "refs/pull/7/merge",
              lane: { type: "pull_request", number: 7, pullRequestHead: "d".repeat(40) },
            },
            run: { ...run, head: retainedHead, workflowId: `ci-${retainedHead}` },
          });
        }
        return Response.json({
          publication: retainedPublication,
          run: { ...run, head: retainedHead, workflowId: `ci-${retainedHead}` },
        });
      }
      if (options.currentHeadChangesAfterLease && leaseRequests.length > 0) {
        const changedHead = "f".repeat(40);
        return Response.json({
          publication: sourcePublication(changedHead),
          run: { ...run, head: changedHead, workflowId: `ci-${changedHead}` },
        });
      }
      return Response.json({ publication, run });
    },
  };
  const releaseStub = {
    fetch: async (request: Request | string, init?: RequestInit) => {
      const pathname = new URL(typeof request === "string" ? request : request.url).pathname;
      if (pathname === "/channels/nightly" && options.nightlyCommit) {
        return Response.json(commitPublication(options.nightlyCommit));
      }
      if (
        pathname === "/channels/nightly" && options.nightlyCommitAfterLease &&
        leaseRequests.length > 0
      ) {
        return Response.json(commitPublication(options.nightlyCommitAfterLease));
      }
      if (pathname.startsWith("/releases/commit/")) {
        const id = pathname.slice(pathname.lastIndexOf("/") + 1);
        const available = options.nightlyCommit ??
          (leaseRequests.length > 0 ? options.nightlyCommitAfterLease : undefined);
        if (id === available) return Response.json(commitManifest(id));
      }
      if (pathname === "/releases/stable/v1.2.3" && releasedCommit) {
        return Response.json(stableManifest(releasedCommit));
      }
      if (pathname === "/channels/latest" && releasedCommit) {
        return Response.json(stablePublication(releasedCommit));
      }
      if (pathname === "/drafts/stable/v1.2.3/finalize") {
        finalizations += 1;
        const headers = new Headers(init?.headers);
        finalizeHeaders = Object.fromEntries(
          Object.keys(PUBLICATION_FENCE_HEADERS).map((name) => [name, headers.get(name)!]),
        );
        releasedCommit = HEAD;
        return Response.json(stablePublication(HEAD));
      }
      return Response.json({ error: "not_found" }, { status: 404 });
    },
  };
  const workflow = {
    create: async (input: {
      id: string;
      params: NanocodexCiParams;
      retention?: { successRetention: string; errorRetention: string };
    }) => {
      createCalls += 1;
      await options.beforeCreate?.();
      if (workflowState !== "unknown") throw new Error("already exists");
      workflowState = "running";
      events.push("workflow");
      created.push(input);
    },
    get: async () => ({
      status: async () => {
        if (statusUnknownOnceAfterCreate && workflowState === "running") {
          statusUnknownOnceAfterCreate = false;
          return { status: "unknown" };
        }
        return { status: workflowState };
      },
      restart: async () => {
        const channel = options.failedDistribution ?? "stable";
        const key = channel === "stable"
          ? "distribution/stable/v1.2.3/result.json"
          : `distribution/commit/${HEAD}/result.json`;
        const result = bucketObjects.get(key);
        restartSnapshots.push(pick(result ?? {}, [
          "status",
          "head",
          "workflowId",
          "requestId",
        ]));
        await options.beforeRestart?.();
        restartCalls += 1;
        if (restartFailure) {
          workflowState = restartFailure === "terminal" ? "complete" : "unknown";
          restartFailure = undefined;
          throw new Error("injected restart failure");
        }
        workflowState = "running";
        events.push("workflow");
        restarts += 1;
      },
    }),
  };
  const env: CiDistributionControlEnv = {
    CI_RELEASE_TOKEN: TOKEN,
    BACKUP_BUCKET: {
      get: async (key: string) => {
        if (key.endsWith("/result.json")) await options.beforeResultRead?.(key);
        const stored = bucketObjects.get(key);
        if (stored) {
          await options.afterRequestRead?.(stored);
          return r2Object(stored, bucketEtags.get(key) ?? "seeded");
        }
        if (key.endsWith("/request.json")) await options.beforeRequestRead?.();
        await options.afterRequestRead?.(undefined);
        if (!key.startsWith("runs/")) return null;
        const head = key.split("/")[1]!;
        return r2Object({
          version: 1,
          status: "success",
          head,
          workflowId: `ci-${head}`,
          completedAt: "2026-08-22T01:30:00.000Z",
        }, `run-${head}`);
      },
      put: async (key: string, value: string, putOptions: R2PutOptions = {}) => {
        if (
          failStableResultPutAfterFinalize && finalizations > 0 &&
          key === "distribution/stable/v1.2.3/result.json"
        ) {
          failStableResultPutAfterFinalize = false;
          throw new Error("injected stable result write failure");
        }
        const existing = bucketObjects.has(key);
        const existingEtag = bucketEtags.get(key);
        const conditional = putOptions.onlyIf instanceof Headers
          ? undefined
          : putOptions.onlyIf;
        if (
          (conditional?.etagDoesNotMatch === "*" && existing) ||
          (conditional?.etagMatches != null && conditional.etagMatches !== existingEtag)
        ) return null;
        const parsed = JSON.parse(value) as Record<string, unknown>;
        bucketObjects.set(key, parsed);
        const etag = `etag-${nextBucketEtag++}`;
        bucketEtags.set(key, etag);
        if (key.endsWith("/result.json")) persistedDistribution = parsed;
        if (
          key.endsWith("/request.json") &&
          parsed.state === failRequestPutAfterCommitState
        ) {
          failRequestPutAfterCommitState = undefined;
          throw new Error("injected lost request fence acknowledgement");
        }
        return { etag };
      },
    } as unknown as R2Bucket,
    CI_REPOSITORY: namespace(repositoryStub),
    CI_RELEASES: namespace(releaseStub),
    CI_WORKFLOW: workflow as unknown as Workflow<NanocodexCiParams>,
  };
  return {
    env,
    created,
    events,
    leaseRequests,
    get restarts() {
      return restarts;
    },
    get createCalls() {
      return createCalls;
    },
    get restartCalls() {
      return restartCalls;
    },
    get finalizations() {
      return finalizations;
    },
    get finalizeHeaders() {
      return finalizeHeaders;
    },
    get persistedDistribution() {
      return persistedDistribution;
    },
    restartSnapshots,
    bucketValue(key: string) {
      return bucketObjects.get(key);
    },
    seedBucket(key: string, value: Record<string, unknown>) {
      bucketObjects.set(key, structuredClone(value));
      bucketEtags.set(key, `etag-${nextBucketEtag++}`);
    },
    setWorkflowState(state: "unknown" | "complete" | "running") {
      workflowState = state;
    },
  };
}

async function stableRequest(
  env: CiDistributionControlEnv,
  body?: Record<string, unknown>,
  finalize = false,
  publicationFence: Record<string, string> = finalize ? PUBLICATION_FENCE_HEADERS : {},
): Promise<Response> {
  const url = new URL(
    `https://ci.example/api/ci/releases/stable/v1.2.3${finalize ? "/finalize" : ""}`,
  );
  const response = await routeCiDistributionControl(
    new Request(url, {
      method: "POST",
      headers: {
        authorization: `Bearer ${TOKEN}`,
        ...publicationFence,
        ...(body ? { "content-type": "application/json" } : {}),
      },
      ...(body ? { body: JSON.stringify(body) } : {}),
    }),
    env,
    url,
  );
  assert.ok(response);
  return response;
}

async function nightlyRequest(
  env: CiDistributionControlEnv,
  body?: Record<string, unknown>,
  authenticated = true,
): Promise<Response> {
  const url = new URL("https://ci.example/api/ci/releases/nightly");
  const response = await routeCiDistributionControl(
    new Request(url, {
      method: "POST",
      headers: {
        ...(authenticated ? { authorization: `Bearer ${TOKEN}` } : {}),
        ...(body ? { "content-type": "application/json" } : {}),
      },
      ...(body ? { body: JSON.stringify(body) } : {}),
    }),
    env,
    url,
  );
  assert.ok(response);
  return response;
}

function namespace(stub: {
  fetch(request: Request | string, init?: RequestInit): Promise<Response>;
}) {
  return {
    idFromName: () => ({ toString: () => "nanocodex" }),
    get: () => stub,
  } as unknown as DurableObjectNamespace;
}

function pick(value: Record<string, unknown>, keys: string[]) {
  return Object.fromEntries(keys.map((key) => [key, value[key]]));
}

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

function barrier(parties: number): () => Promise<void> {
  const ready = deferred();
  let arrivals = 0;
  return async () => {
    arrivals += 1;
    if (arrivals === parties) ready.resolve();
    await ready.promise;
  };
}

function r2Object(value: Record<string, unknown>, etag: string): R2ObjectBody {
  const encoded = JSON.stringify(value);
  return {
    size: new TextEncoder().encode(encoded).byteLength,
    etag,
    body: new Response(encoded).body,
    json: async () => structuredClone(value),
  } as unknown as R2ObjectBody;
}

function stableManifest(commit: string): Record<string, unknown> {
  const unsigned = {
    version: 1,
    kind: "stable",
    id: "v1.2.3",
    tag: "v1.2.3",
    commit,
    channel: "latest",
    finalizedAt: "2026-08-22T03:00:00.000Z",
    assets: [],
  };
  return {
    ...unsigned,
    manifestSha256: createHash("sha256").update(canonicalJson(unsigned)).digest("hex"),
  };
}

function stablePublication(commit: string): Record<string, unknown> {
  return {
    pointer: {
      version: 1,
      channel: "latest",
      kind: "stable",
      id: "v1.2.3",
      tag: "v1.2.3",
      commit,
      generation: 1,
      updatedAt: "2026-08-22T03:00:00.000Z",
    },
    manifest: stableManifest(commit),
  };
}

function commitManifest(head: string): Record<string, unknown> {
  const unsigned = {
    version: 1,
    kind: "commit",
    id: head,
    tag: `nightly-${head}`,
    commit: head,
    channel: "nightly",
    finalizedAt: "2026-08-22T03:00:00.000Z",
    assets: [],
  };
  return {
    ...unsigned,
    manifestSha256: createHash("sha256").update(canonicalJson(unsigned)).digest("hex"),
  };
}

function commitPublication(head: string): Record<string, unknown> {
  return {
    pointer: {
      version: 1,
      channel: "nightly",
      kind: "commit",
      id: head,
      tag: `nightly-${head}`,
      commit: head,
      generation: 1,
      updatedAt: "2026-08-22T03:00:00.000Z",
    },
    manifest: commitManifest(head),
  };
}

function canonicalJson(value: unknown): string {
  if (value === null || typeof value === "boolean" || typeof value === "string") {
    return JSON.stringify(value);
  }
  if (typeof value === "number") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  const entries = value as Record<string, unknown>;
  return `{${Object.keys(entries).sort().map((key) =>
    `${JSON.stringify(key)}:${canonicalJson(entries[key])}`
  ).join(",")}}`;
}

function sourcePublication(head = HEAD): CiSourcePublication {
  return {
    version: 1,
    head,
    branch: "master",
    ref: "refs/heads/master",
    archive: {
      key: `sources/${head}/source.tar.gz`,
      size: 123,
      sha256: "1".repeat(64),
    },
    tree: {
      key: `sources/${head}/tree.json`,
      size: 456,
      sha256: "2".repeat(64),
    },
    cargoLockBlob: "b".repeat(40),
    cargoVendor: {
      key: `cargo-vendor/${"b".repeat(40)}/${"3".repeat(64)}/bundle.tar.gz`,
      size: 789,
      sha256: "3".repeat(64),
    },
    rustSecRevision: "c".repeat(40),
    rustSec: {
      key: `rustsec-advisory-db/${"c".repeat(40)}/bundle.tar.gz`,
      size: 321,
      sha256: "4".repeat(64),
    },
    publishedAt: "2026-08-22T01:02:03.000Z",
  };
}
