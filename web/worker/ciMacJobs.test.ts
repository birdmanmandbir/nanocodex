import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { test } from "node:test";

import {
  CI_MAC_CLAIM_LEASE_MS,
  CI_MAC_RETENTION_MS,
  type CiMacAsset,
  type CiMacJobRecord,
  CiMacJobs,
  type CiMacLog,
} from "./ciMacJobs.ts";

const headA = "a".repeat(40);
const headB = "b".repeat(40);
const headC = "c".repeat(40);
const workflowA = `ci-${headA}`;
const workflowB = `ci-${headB}`;
const workflowC = `ci-${headC}`;
const host = {
  hostname: "mac-builder-1",
  platform: "darwin",
  arch: "arm64",
} as const;
const publishedAt = "2026-08-22T00:00:00.000Z";

test("accepts only the exact lane-neutral native-build input", async () => {
  const memory = broker();
  const accepted = await queue(memory.durable, nativeJob(headA, workflowA));
  assert.equal(accepted.status, 201);
  const job = (await accepted.json() as { job: CiMacJobRecord }).job;
  assert.equal(job.id, `macos-native-build-${headA}`);
  assert.equal(job.head, headA);
  assert.equal(job.workflowId, workflowA);
  assert.equal(job.publishedAt, publishedAt);
  assert.deepEqual(job.source, source(headA));
  assert.deepEqual(job.cargoVendor, cargoVendor(headA));
  assert.equal(job.release, undefined);

  assert.equal(
    (await queue(memory.durable, nativeJob(headA, workflowA))).status,
    200,
  );
  assert.equal(
    (await queue(memory.durable, {
      cargoVendor: {
        sha256: cargoVendor(headA).sha256,
        url: cargoVendor(headA).url,
        size: cargoVendor(headA).size,
      },
      publishedAt,
      source: {
        sha256: source(headA).sha256,
        url: source(headA).url,
        size: source(headA).size,
      },
      task: "native-build",
      workflowId: workflowA,
      head: headA,
    })).status,
    200,
  );
  assert.equal(
    (await queue(memory.durable, {
      ...nativeJob(headA, workflowA),
      source: { ...source(headA), sha256: "9".repeat(64) },
    })).status,
    409,
  );
  const fullVendor = broker();
  assert.equal(
    (await queue(fullVendor.durable, {
      ...nativeJob(headB, workflowB),
      cargoVendor: { ...cargoVendor(headB), size: 189_626_590 },
    })).status,
    201,
  );

  const invalid: unknown[] = [
    { ...nativeJob(headB, workflowB), publishedAt: "2026-08-22T00:00:00Z" },
    {
      ...nativeJob(headB, workflowB),
      publishedAt: "2026-08-22T03:00:00.000+03:00",
    },
    { ...nativeJob(headB, workflowB), publishedAt: "not-a-timestamp" },
    { ...nativeJob(headB, workflowB), workflowId: "ci-unrelated" },
    {
      ...nativeJob(headB, workflowB),
      source: { ...source(headB), url: source(headA).url },
    },
    {
      ...nativeJob(headB, workflowB),
      cargoVendor: {
        ...cargoVendor(headB),
        url: `https://other.test/api/ci/cargo-vendor/${headB}/${cargoVendor(headB).sha256}/bundle.tar.gz`,
      },
    },
    { ...nativeJob(headB, workflowB), command: ["cargo", "build"] },
    {
      ...nativeJob(headB, workflowB),
      environment: { RUSTFLAGS: "-C target-cpu=native" },
    },
    { ...nativeJob(headB, workflowB), buildTimestamp: publishedAt },
    {
      ...nativeJob(headB, workflowB),
      release: releaseJob(headB, workflowB).release,
    },
    { ...nativeJob(headB, workflowB), testedSha: headB },
    {
      ...nativeJob(headB, workflowB),
      lane: { type: "pull_request", number: 1, pullRequestHead: headA },
    },
  ];
  const { publishedAt: _omitted, ...withoutPublishedAt } = nativeJob(
    headB,
    workflowB,
  );
  invalid.push(withoutPublishedAt);
  for (const input of invalid) {
    assert.equal((await queue(memory.durable, input)).status, 400);
  }

  const claimed = await claimBody(memory.durable);
  assert.deepEqual(Object.keys(claimed.job).sort(), [
    "cargoVendor",
    "head",
    "id",
    "publishedAt",
    "source",
    "task",
    "workflowId",
  ]);
  assert.equal(claimed.job.task, "native-build");
  assert.equal(claimed.job.publishedAt, publishedAt);
  assert.equal("command" in claimed.job, false);
  assert.equal("environment" in claimed.job, false);
});

test("queues an exact immutable job and atomically reclaims an expired lease", async () => {
  const clock = useClock();
  try {
    const memory = broker();
    const accepted = await queue(
      memory.durable,
      workspaceJob(headA, workflowA),
    );
    assert.equal(accepted.status, 201);
    const acceptedJob = (await accepted.json() as { job: CiMacJobRecord }).job;
    assert.equal(acceptedJob.id, `macos-workspace-test-${headA}`);
    assert.equal(acceptedJob.state, "queued");
    assert.deepEqual(acceptedJob.cargoVendor, cargoVendor(headA));

    assert.equal(
      (await queue(memory.durable, workspaceJob(headA, workflowA))).status,
      200,
    );
    assert.equal(
      (await queue(memory.durable, workspaceJob(headA, "ci-other"))).status,
      409,
    );
    assert.equal(
      (await queue(memory.durable, {
        ...workspaceJob(headA, workflowA),
        cargoVendor: {
          ...cargoVendor(headA),
          url: `https://ci.test/api/ci/cargo-vendor/${headA}/${"3".repeat(64)}/bundle.tar.gz`,
          sha256: "3".repeat(64),
        },
      })).status,
      409,
    );
    assert.equal(
      (await queue(memory.durable, {
        ...workspaceJob(headB, workflowB),
        cargoVendor: { ...cargoVendor(headB), sha256: "3".repeat(64) },
      })).status,
      400,
    );
    assert.equal(
      (await queue(memory.durable, {
        ...workspaceJob(headB, workflowB),
        task: "arbitrary-shell",
      })).status,
      400,
    );
    assert.equal(
      (await queue(memory.durable, {
        ...workspaceJob(headB, workflowB),
        source: {
          ...source(headB),
          url: `https://ci.test/api/ci/source/${headA}/archive`,
        },
      })).status,
      400,
    );
    assert.equal(
      (await queue(memory.durable, {
        ...workspaceJob(headB, workflowB),
        cargoVendor: {
          ...cargoVendor(headB),
          url: `https://other.test/api/ci/cargo-vendor/${headB}/${cargoVendor(headB).sha256}/bundle.tar.gz`,
        },
      })).status,
      400,
    );
    assert.equal(
      (await queue(memory.durable, {
        ...workspaceJob(headB, workflowB),
        cargoVendor: { ...cargoVendor(headB), size: 256 * 1024 * 1024 + 1 },
      })).status,
      400,
    );

    const claims = await Promise.all([
      claim(memory.durable, "worker-a"),
      claim(memory.durable, "worker-b"),
    ]);
    const bodies = await Promise.all(
      claims.map((response) => response.json()),
    ) as Array<{
      action: string;
      claim?: string;
      job?: { head: string; cargoVendor: ReturnType<typeof cargoVendor> };
    }>;
    const running = bodies.filter(({ action }) => action === "run");
    assert.equal(running.length, 1);
    assert.equal(bodies.filter(({ action }) => action === "idle").length, 1);
    assert.equal(running[0]!.job!.head, headA);
    assert.deepEqual(running[0]!.job!.cargoVendor, cargoVendor(headA));
    const firstClaim = running[0]!.claim!;

    clock.advance(CI_MAC_CLAIM_LEASE_MS + 1);
    const reclaimed = await claim(memory.durable, "worker-c");
    const reclaimedBody = await reclaimed.json() as {
      action: "run";
      claim: string;
      job: { head: string; cargoVendor: ReturnType<typeof cargoVendor> };
    };
    assert.equal(reclaimedBody.action, "run");
    assert.equal(reclaimedBody.job.head, headA);
    assert.notEqual(reclaimedBody.claim, firstClaim);
    assert.equal((await heartbeat(memory.durable, firstClaim)).status, 404);
    const stored = await readJob(
      memory.durable,
      `macos-workspace-test-${headA}`,
    );
    assert.equal(stored.attempts, 2);
    assert.equal(stored.claim?.worker, "worker-c");
    assert.deepEqual(stored.claim?.host, host);
    const workers = await memory.durable.fetch(
      new Request("https://ci.test/workers"),
    );
    assert.deepEqual(await workers.json(), {
      workers: [
        {
          version: 1,
          id: "worker-c",
          host,
          firstSeenAt: new Date(clock.now).toISOString(),
          lastSeenAt: new Date(clock.now).toISOString(),
        },
        {
          version: 1,
          id: "worker-a",
          host,
          firstSeenAt: new Date(clock.now - CI_MAC_CLAIM_LEASE_MS - 1)
            .toISOString(),
          lastSeenAt: new Date(clock.now - CI_MAC_CLAIM_LEASE_MS - 1)
            .toISOString(),
        },
        {
          version: 1,
          id: "worker-b",
          host,
          firstSeenAt: new Date(clock.now - CI_MAC_CLAIM_LEASE_MS - 1)
            .toISOString(),
          lastSeenAt: new Date(clock.now - CI_MAC_CLAIM_LEASE_MS - 1)
            .toISOString(),
        },
      ],
    });
  } finally {
    clock.restore();
  }
});

test("claim leases start after control-plane lookup completes", async () => {
  const clock = useClock();
  try {
    const memory = broker({
      onWorkflowStatus: () => clock.advance(5 * 60 * 1_000),
    });
    await queue(memory.durable, nativeJob(headA, workflowA));
    const claimed = await claimBody(memory.durable);
    assert.equal(
      claimed.leaseExpiresAt,
      new Date(clock.now + CI_MAC_CLAIM_LEASE_MS).toISOString(),
    );
  } finally {
    clock.restore();
  }
});

test("heartbeats renew live claims and report terminal Workflow cancellation", async () => {
  const clock = useClock();
  try {
    const memory = broker();
    await queue(memory.durable, workspaceJob(headA, workflowA));
    const claimed = await claimBody(memory.durable);
    clock.advance(30_000);
    const renewed = await heartbeat(memory.durable, claimed.claim);
    assert.equal(renewed.status, 204);
    assert.equal(
      renewed.headers.get("x-nanocodex-lease-expires-at"),
      new Date(clock.now + CI_MAC_CLAIM_LEASE_MS).toISOString(),
    );
    const workers = await memory.durable.fetch(
      new Request("https://ci.test/workers"),
    );
    const worker = (await workers.json() as {
      workers: Array<{ id: string; lastSeenAt: string }>;
    }).workers.find(({ id }) => id === "mac-worker");
    assert.equal(worker?.lastSeenAt, new Date(clock.now).toISOString());

    const acknowledgedExpiry =
      (await readJob(memory.durable, claimed.job.id)).claim!
        .leaseExpiresAt;
    const stdout = await uploadedLog(
      memory.durable,
      claimed.claim,
      "stdout",
      "stdout\n",
    );
    const stderr = await uploadedLog(
      memory.durable,
      claimed.claim,
      "stderr",
      "",
    );
    clock.advance(1_000);
    memory.setWorkflow(workflowA, "unknown");
    const uncertain = await heartbeat(memory.durable, claimed.claim);
    assert.equal(uncertain.status, 503);
    assert.equal(uncertain.headers.get("x-nanocodex-lease-expires-at"), null);
    assert.deepEqual(await uncertain.json(), {
      error: "workflow_status_unavailable",
    });
    assert.equal(
      (await readJob(memory.durable, claimed.job.id)).claim?.leaseExpiresAt,
      acknowledgedExpiry,
    );
    assert.equal(
      (await complete(memory.durable, claimed.claim, success(stdout, stderr)))
        .status,
      503,
    );
    assert.equal(
      (await uploadLog(
        memory.durable,
        claimed.claim,
        "stdout",
        new TextEncoder().encode("replacement\n"),
      )).status,
      503,
    );

    memory.setWorkflow(workflowA, "terminated");
    const cancelled = await heartbeat(memory.durable, claimed.claim);
    assert.equal(cancelled.status, 200);
    assert.deepEqual(await cancelled.json(), {
      action: "cancel",
      reason: "workflow_terminal",
      workflowStatus: "terminated",
    });
    const stored = await readJob(memory.durable, claimed.job.id);
    assert.equal(stored.state, "cancelled");
    assert.deepEqual(stored.cancellation, {
      reason: "workflow_terminal",
      workflowStatus: "terminated",
    });

    await queue(memory.durable, workspaceJob(headB, workflowB));
    memory.setWorkflow(workflowB, "unknown");
    const unavailable = await claim(memory.durable, "worker-d");
    assert.equal(unavailable.status, 503);
    assert.equal(
      (await readJob(memory.durable, `macos-workspace-test-${headB}`)).state,
      "queued",
    );
    memory.setWorkflow(workflowB, "complete");
    const idle = await claim(memory.durable, "worker-d");
    assert.equal((await idle.json() as { action: string }).action, "idle");
    assert.equal(
      (await readJob(memory.durable, `macos-workspace-test-${headB}`)).state,
      "cancelled",
    );
  } finally {
    clock.restore();
  }
});

test("terminal Workflow rejects log and asset mutations without writing to R2", async () => {
  const clock = useClock();
  try {
    const logMemory = broker();
    await queue(logMemory.durable, workspaceJob(headA, workflowA));
    const logClaim = await claimBody(logMemory.durable);
    logMemory.setWorkflow(workflowA, "terminated");
    const logResponse = await uploadLog(
      logMemory.durable,
      logClaim.claim,
      "stdout",
      new TextEncoder().encode("late output\n"),
    );
    assert.equal(logResponse.status, 409);
    assert.deepEqual(await logResponse.json(), {
      action: "cancel",
      reason: "workflow_terminal",
      workflowStatus: "terminated",
    });
    const logJob = await readJob(logMemory.durable, logClaim.job.id);
    assert.equal(logJob.state, "cancelled");
    assert.deepEqual(logJob.cancellation, {
      reason: "workflow_terminal",
      workflowStatus: "terminated",
    });
    assert.deepEqual(logMemory.objectKeys(), []);

    const assetMemory = broker();
    await queue(assetMemory.durable, nativeJob(headB, workflowB));
    const assetClaim = await claimBody(assetMemory.durable);
    assetMemory.setWorkflow(workflowB, "errored");
    const assetResponse = await uploadAsset(
      assetMemory.durable,
      assetClaim.claim,
      new TextEncoder().encode("late asset"),
    );
    assert.equal(assetResponse.status, 409);
    assert.deepEqual(await assetResponse.json(), {
      action: "cancel",
      reason: "workflow_terminal",
      workflowStatus: "errored",
    });
    const assetJob = await readJob(assetMemory.durable, assetClaim.job.id);
    assert.equal(assetJob.state, "cancelled");
    assert.deepEqual(assetJob.cancellation, {
      reason: "workflow_terminal",
      workflowStatus: "errored",
    });
    assert.deepEqual(assetMemory.objectKeys(), []);
  } finally {
    clock.restore();
  }
});

test("terminal Workflow rejects completion with a cancellation conflict", async () => {
  const clock = useClock();
  try {
    const memory = broker();
    await queue(memory.durable, workspaceJob(headC, workflowC));
    const claimed = await claimBody(memory.durable);
    const stdout = await uploadedLog(
      memory.durable,
      claimed.claim,
      "stdout",
      "complete too late\n",
    );
    const stderr = await uploadedLog(
      memory.durable,
      claimed.claim,
      "stderr",
      "",
    );
    const objectsBefore = memory.objectKeys();

    memory.setWorkflow(workflowC, "complete");
    const response = await complete(
      memory.durable,
      claimed.claim,
      success(stdout, stderr),
    );
    assert.equal(response.status, 409);
    assert.deepEqual(await response.json(), {
      action: "cancel",
      reason: "workflow_terminal",
      workflowStatus: "complete",
    });
    const stored = await readJob(memory.durable, claimed.job.id);
    assert.equal(stored.state, "cancelled");
    assert.deepEqual(stored.cancellation, {
      reason: "workflow_terminal",
      workflowStatus: "complete",
    });
    assert.deepEqual(memory.objectKeys(), objectsBefore);
  } finally {
    clock.restore();
  }
});

test("queue creation arms reconciliation and terminal ownership is removed after seven days", async () => {
  const clock = useClock();
  try {
    const memory = broker();
    const accepted = await queue(
      memory.durable,
      workspaceJob(headA, workflowA),
    );
    assert.equal(accepted.status, 201);
    assert.equal(memory.alarm > clock.now, true);
    assert.equal(memory.alarm < clock.now + 24 * 60 * 60 * 1_000, true);

    memory.setWorkflow(workflowA, "complete");
    clock.advance(memory.alarm - clock.now);
    await memory.durable.alarm();
    const cancelled = await readJob(
      memory.durable,
      `macos-workspace-test-${headA}`,
    );
    assert.equal(cancelled.state, "cancelled");
    assert.deepEqual(cancelled.cancellation, {
      reason: "workflow_terminal",
      workflowStatus: "complete",
    });
    assert.equal(
      cancelled.retention?.deleteAfter,
      new Date(clock.now + CI_MAC_RETENTION_MS).toISOString(),
    );
    assert.equal(memory.retentionKeys().length, 1);

    clock.advance(CI_MAC_RETENTION_MS);
    await memory.durable.alarm();
    assert.equal(
      (await memory.durable.fetch(
        new Request(
          `https://ci.test/api/ci/macos/jobs/macos-workspace-test-${headA}`,
        ),
      )).status,
      404,
    );
    assert.equal(memory.retentionKeys().length, 0);
  } finally {
    clock.restore();
  }
});

test("an expired claim transactionally requeues and rearms autonomous reconciliation", async () => {
  const clock = useClock();
  try {
    const memory = broker();
    await queue(memory.durable, nativeJob(headA, workflowA));
    const claimed = await claimBody(memory.durable);
    assert.equal(memory.alarm, clock.now + CI_MAC_CLAIM_LEASE_MS);

    clock.advance(CI_MAC_CLAIM_LEASE_MS);
    await memory.durable.alarm();
    const requeued = await readJob(memory.durable, claimed.job.id);
    assert.equal(requeued.state, "queued");
    assert.equal(memory.retentionKeys().length, 1);
    assert.equal(memory.alarm > clock.now, true);
    assert.equal(memory.alarm < clock.now + 24 * 60 * 60 * 1_000, true);

    memory.setWorkflow(workflowA, "errored");
    clock.advance(memory.alarm - clock.now);
    await memory.durable.alarm();
    const cancelled = await readJob(memory.durable, claimed.job.id);
    assert.equal(cancelled.state, "cancelled");
    assert.equal(cancelled.cancellation?.workflowStatus, "errored");
  } finally {
    clock.restore();
  }
});

test("metadata expiry accepts only bounded exact result or completed termination evidence", async () => {
  const clock = useClock();
  try {
    const resultMemory = broker();
    const terminationMemory = broker();
    const malformedMemory = broker();
    for (const memory of [resultMemory, terminationMemory, malformedMemory]) {
      await queue(memory.durable, workspaceJob(headA, workflowA));
      memory.setWorkflow(workflowA, "unknown");
    }
    const completedAt = new Date(clock.now).toISOString();
    resultMemory.seedObject(
      `runs/${headA}/result.json`,
      JSON.stringify({
        version: 1,
        head: headA,
        workflowId: workflowA,
        status: "failure",
        completedAt,
      }),
    );
    terminationMemory.seedObject(
      `runs/${headA}/control/terminated.json`,
      JSON.stringify({
        version: 1,
        status: "complete",
        head: headA,
        workflowId: workflowA,
        claimId: crypto.randomUUID(),
        completedAt,
      }),
    );
    malformedMemory.seedObject(
      `runs/${headA}/result.json`,
      JSON.stringify({
        version: 1,
        head: headA,
        workflowId: workflowA,
        status: "success",
        completedAt,
      }),
      { reportedSize: 8 * 1024 * 1024 + 1 },
    );
    malformedMemory.seedObject(
      `runs/${headA}/control/terminated.json`,
      JSON.stringify({
        version: 1,
        status: "requested",
        head: headA,
        workflowId: workflowA,
        claimId: crypto.randomUUID(),
        completedAt: "2026-08-22T00:00:00Z",
      }),
    );

    clock.advance(31 * 24 * 60 * 60 * 1_000);
    await resultMemory.durable.alarm();
    await terminationMemory.durable.alarm();
    await malformedMemory.durable.alarm();

    const resultJob = await readJob(
      resultMemory.durable,
      `macos-workspace-test-${headA}`,
    );
    assert.equal(resultJob.state, "cancelled");
    assert.equal(resultJob.cancellation?.workflowStatus, "errored");
    const terminationJob = await readJob(
      terminationMemory.durable,
      `macos-workspace-test-${headA}`,
    );
    assert.equal(terminationJob.state, "cancelled");
    assert.equal(terminationJob.cancellation?.workflowStatus, "terminated");
    const malformedJob = await readJob(
      malformedMemory.durable,
      `macos-workspace-test-${headA}`,
    );
    assert.equal(malformedJob.state, "queued");
    assert.equal(malformedJob.retention, undefined);
    assert.equal(malformedMemory.alarm > clock.now, true);
    assert.equal(
      malformedMemory.alarm <= clock.now + 6 * 60 * 60 * 1_000,
      true,
    );
  } finally {
    clock.restore();
  }
});

test("live nonterminal Workflow state takes precedence over stale retained evidence", async () => {
  const clock = useClock();
  try {
    const memory = broker();
    await queue(memory.durable, workspaceJob(headA, workflowA));
    memory.seedObject(
      `runs/${headA}/result.json`,
      JSON.stringify({
        version: 1,
        head: headA,
        workflowId: workflowA,
        status: "success",
        completedAt: new Date(clock.now).toISOString(),
      }),
    );

    clock.advance(memory.alarm - clock.now);
    await memory.durable.alarm();
    assert.equal(
      (await readJob(memory.durable, `macos-workspace-test-${headA}`)).state,
      "queued",
    );
    assert.deepEqual(memory.r2Reads, []);

    memory.setWorkflow(workflowA, "unknown");
    clock.advance(memory.alarm - clock.now);
    await memory.durable.alarm();
    const cancelled = await readJob(
      memory.durable,
      `macos-workspace-test-${headA}`,
    );
    assert.equal(cancelled.state, "cancelled");
    assert.equal(cancelled.cancellation?.workflowStatus, "complete");
    assert.deepEqual(memory.r2Reads.sort(), [
      `runs/${headA}/control/terminated.json`,
      `runs/${headA}/result.json`,
    ]);
  } finally {
    clock.restore();
  }
});

test("the auth-forwarded maintenance route runs one bounded reconciliation sweep", async () => {
  const clock = useClock();
  try {
    const memory = broker();
    const heads = Array.from(
      { length: 17 },
      (_, index) => index.toString(16).padStart(40, "0"),
    );
    for (const head of heads) {
      const workflowId = `ci-${head}`;
      await queue(memory.durable, workspaceJob(head, workflowId));
      memory.setWorkflow(workflowId, "terminated");
    }
    const response = await memory.durable.fetch(
      new Request(
        "https://ci.test/api/ci/macos/maintenance/reconcile",
        { method: "POST" },
      ),
    );
    assert.equal(response.status, 200);
    assert.deepEqual(await response.json(), { reconciled: 16 });
    const firstPage = await Promise.all(
      heads.map((head) =>
        readJob(memory.durable, `macos-workspace-test-${head}`)
      ),
    );
    assert.equal(
      firstPage.filter(({ state }) => state === "cancelled").length,
      16,
    );
    assert.equal(firstPage.filter(({ state }) => state === "queued").length, 1);

    const final = await memory.durable.fetch(
      new Request(
        "https://ci.test/api/ci/macos/maintenance/reconcile",
        { method: "POST" },
      ),
    );
    assert.deepEqual(await final.json(), { reconciled: 1 });
    const swept = await Promise.all(
      heads.map((head) =>
        readJob(memory.durable, `macos-workspace-test-${head}`)
      ),
    );
    assert.equal(swept.every(({ state }) => state === "cancelled"), true);
  } finally {
    clock.restore();
  }
});

test("verifies bounded logs, completes once, and retries the Workflow event outbox", async () => {
  const clock = useClock();
  try {
    const memory = broker({ sendFailures: 1, loseAcknowledgement: true });
    await queue(memory.durable, workspaceJob(headA, workflowA));
    const claimed = await claimBody(memory.durable);

    assert.equal(
      (await uploadAsset(
        memory.durable,
        claimed.claim,
        new TextEncoder().encode("not-allowed"),
      )).status,
      409,
    );

    const tooLarge = await uploadLog(
      memory.durable,
      claimed.claim,
      "stdout",
      new Uint8Array([1]),
      { length: 64 * 1024 * 1024 + 1 },
    );
    assert.equal(tooLarge.status, 413);

    const badChecksum = await uploadLog(
      memory.durable,
      claimed.claim,
      "stdout",
      new TextEncoder().encode("stdout\n"),
      { sha256: sha256("different") },
    );
    assert.equal(badChecksum.status, 422);

    const stdout = await uploadedLog(
      memory.durable,
      claimed.claim,
      "stdout",
      "stdout\n",
    );
    const stderr = await uploadedLog(
      memory.durable,
      claimed.claim,
      "stderr",
      "",
    );
    assert.match(
      stdout.key,
      new RegExp(`^macos/jobs/${claimed.job.id}/attempts/`),
    );
    assert.equal(stderr.size, 0);

    const body = success(stdout, stderr);
    const concurrent = await Promise.all([
      complete(memory.durable, claimed.claim, body),
      complete(memory.durable, claimed.claim, body),
    ]);
    assert.deepEqual(concurrent.map(({ status }) => status).sort(), [200, 202]);
    assert.equal(memory.alarm, clock.now);
    let stored = await readJob(memory.durable, claimed.job.id);
    assert.equal(stored.state, "completed");
    assert.equal(stored.delivery?.state, "pending");
    assert.equal(stored.result?.worker, "mac-worker");
    assert.deepEqual(stored.result?.host, host);

    const replay = await complete(memory.durable, claimed.claim, body);
    assert.equal(replay.status, 200);
    assert.equal(
      (await complete(memory.durable, claimed.claim, {
        logs: { stderr, stdout },
        durationMs: body.durationMs,
        exitCode: body.exitCode,
        outcome: body.outcome,
      })).status,
      200,
    );
    assert.equal(
      (await complete(memory.durable, claimed.claim, {
        ...body,
        durationMs: 12_346,
      })).status,
      409,
    );

    await memory.durable.alarm();
    stored = await readJob(memory.durable, claimed.job.id);
    assert.equal(stored.delivery?.state, "pending");
    assert.equal(stored.delivery?.attempts, 1);
    assert.equal(memory.events.length, 1);
    assert.equal(memory.events[0]!.type, "macos-job-completed");
    assert.equal(memory.events[0]!.payload.jobId, claimed.job.id);

    clock.advance(2_001);
    await memory.durable.alarm();
    stored = await readJob(memory.durable, claimed.job.id);
    assert.equal(stored.delivery?.state, "delivered");
    assert.equal(stored.delivery?.attempts, 2);
    assert.equal(memory.events.length, 2);
    assert.deepEqual(memory.events[1], memory.events[0]);
  } finally {
    clock.restore();
  }
});

test("the completion outbox does not emit while Workflow status is uncertain", async () => {
  const clock = useClock();
  try {
    const memory = broker();
    await queue(memory.durable, workspaceJob(headA, workflowA));
    const claimed = await claimBody(memory.durable);
    const stdout = await uploadedLog(
      memory.durable,
      claimed.claim,
      "stdout",
      "done\n",
    );
    const stderr = await uploadedLog(
      memory.durable,
      claimed.claim,
      "stderr",
      "",
    );
    assert.equal(
      (await complete(memory.durable, claimed.claim, success(stdout, stderr)))
        .status,
      202,
    );

    memory.setWorkflow(workflowA, "unknown");
    await memory.durable.alarm();
    let stored = await readJob(memory.durable, claimed.job.id);
    assert.equal(stored.delivery?.state, "pending");
    assert.equal(stored.delivery?.attempts, 1);
    assert.equal(memory.events.length, 0);

    memory.setWorkflow(workflowA, "running");
    clock.advance(2_001);
    await memory.durable.alarm();
    stored = await readJob(memory.durable, claimed.job.id);
    assert.equal(stored.delivery?.state, "delivered");
    assert.equal(stored.delivery?.attempts, 2);
    assert.equal(memory.events.length, 1);
  } finally {
    clock.restore();
  }
});

test("requires one verified aarch64 asset for native and release builds", async () => {
  const clock = useClock();
  try {
    for (
      const queued of [
        nativeJob(headA, workflowA),
        releaseJob(headB, workflowB),
      ]
    ) {
      const memory = broker();
      await queue(memory.durable, queued);
      const claimed = await claimBody(memory.durable);
      const stdout = await uploadedLog(
        memory.durable,
        claimed.claim,
        "stdout",
        "built\n",
      );
      const stderr = await uploadedLog(
        memory.durable,
        claimed.claim,
        "stderr",
        "",
      );
      const withoutAsset = await complete(
        memory.durable,
        claimed.claim,
        success(stdout, stderr),
      );
      assert.equal(withoutAsset.status, 409);
      assert.deepEqual(await withoutAsset.json(), { error: "asset_required" });

      const assetBody = new TextEncoder().encode("mach-o-binary");
      const uploaded = await uploadAsset(
        memory.durable,
        claimed.claim,
        assetBody,
      );
      assert.equal(uploaded.status, 201);
      const asset = await uploaded.json() as CiMacAsset;
      assert.deepEqual(asset, {
        name: "nanocodex-aarch64-apple-darwin",
        platform: "aarch64-apple-darwin",
        key:
          `macos/jobs/${claimed.job.id}/attempts/${claimed.claim}/assets/nanocodex-aarch64-apple-darwin`,
        size: assetBody.byteLength,
        sha256: sha256(assetBody),
        contentType: "application/octet-stream",
      });
      const completed = await complete(
        memory.durable,
        claimed.claim,
        { ...success(stdout, stderr), asset },
      );
      assert.equal(completed.status, 202);
      const stored = await readJob(memory.durable, claimed.job.id);
      assert.deepEqual(stored.result?.asset, asset);
      assert.equal(
        stored.retention?.deleteAfter,
        new Date(clock.now + CI_MAC_RETENTION_MS).toISOString(),
      );
    }
  } finally {
    clock.restore();
  }
});

test("an expired asset attempt cannot poison retry and every stale mutation is rejected", async () => {
  const clock = useClock();
  try {
    const memory = broker();
    await queue(memory.durable, nativeJob(headA, workflowA));
    const first = await claimBody(memory.durable);
    const firstStdout = await uploadedLog(
      memory.durable,
      first.claim,
      "stdout",
      "first\n",
    );
    const firstStderr = await uploadedLog(
      memory.durable,
      first.claim,
      "stderr",
      "",
    );
    const firstBytes = new TextEncoder().encode("first-mach-o");
    const firstUpload = await uploadAsset(
      memory.durable,
      first.claim,
      firstBytes,
    );
    assert.equal(firstUpload.status, 201);
    const firstAsset = await firstUpload.json() as CiMacAsset;

    clock.advance(CI_MAC_CLAIM_LEASE_MS + 1);
    const second = await claimBody(memory.durable, "replacement-worker");
    assert.notEqual(second.claim, first.claim);
    assert.equal((await heartbeat(memory.durable, first.claim)).status, 404);
    assert.equal(
      (await uploadAsset(
        memory.durable,
        first.claim,
        new TextEncoder().encode("late-mach-o"),
      )).status,
      404,
    );
    assert.equal(
      (await complete(memory.durable, first.claim, {
        ...success(firstStdout, firstStderr),
        asset: firstAsset,
      })).status,
      404,
    );

    const secondBytes = new TextEncoder().encode("different-second-mach-o");
    const secondUpload = await uploadAsset(
      memory.durable,
      second.claim,
      secondBytes,
    );
    assert.equal(secondUpload.status, 201);
    const secondAsset = await secondUpload.json() as CiMacAsset;
    assert.notEqual(secondAsset.key, firstAsset.key);
    assert.match(
      firstAsset.key,
      new RegExp(`/attempts/${first.claim}/assets/`),
    );
    assert.match(
      secondAsset.key,
      new RegExp(`/attempts/${second.claim}/assets/`),
    );
    assert.equal(memory.hasObject(firstAsset.key), true);
    assert.equal(memory.hasObject(secondAsset.key), true);

    const secondStdout = await uploadedLog(
      memory.durable,
      second.claim,
      "stdout",
      "second\n",
    );
    const secondStderr = await uploadedLog(
      memory.durable,
      second.claim,
      "stderr",
      "",
    );
    assert.equal(
      (await complete(memory.durable, second.claim, {
        ...success(secondStdout, secondStderr),
        asset: secondAsset,
      })).status,
      202,
    );
  } finally {
    clock.restore();
  }
});

test("expired attempts are collected after seven days without deleting the queued job or shared inputs", async () => {
  const clock = useClock();
  try {
    const memory = broker();
    const sourceKey = `sources/${headA}/source.tar.gz`;
    const vendorKey = `cargo-vendor/${headA}/${cargoVendor(headA).sha256}/bundle.tar.gz`;
    memory.seedObject(sourceKey, "shared source");
    memory.seedObject(vendorKey, "shared vendor");
    await queue(memory.durable, nativeJob(headA, workflowA));
    const claimed = await claimBody(memory.durable);
    const stdout = await uploadedLog(
      memory.durable,
      claimed.claim,
      "stdout",
      "orphan\n",
    );
    const stderr = await uploadedLog(
      memory.durable,
      claimed.claim,
      "stderr",
      "",
    );
    const uploaded = await uploadAsset(
      memory.durable,
      claimed.claim,
      new TextEncoder().encode("orphan-mach-o"),
    );
    const asset = await uploaded.json() as CiMacAsset;

    clock.advance(CI_MAC_CLAIM_LEASE_MS);
    await memory.durable.alarm();
    assert.equal(
      (await readJob(memory.durable, claimed.job.id)).state,
      "queued",
    );
    assert.equal(memory.retentionKeys().length, 1);
    assert.equal(memory.hasObject(stdout.key), true);
    assert.equal(memory.hasObject(stderr.key), true);
    assert.equal(memory.hasObject(asset.key), true);

    clock.advance(CI_MAC_RETENTION_MS);
    await memory.durable.alarm();
    assert.equal(
      (await readJob(memory.durable, claimed.job.id)).state,
      "queued",
    );
    assert.equal(memory.retentionKeys().length, 0);
    assert.equal(memory.hasObject(stdout.key), false);
    assert.equal(memory.hasObject(stderr.key), false);
    assert.equal(memory.hasObject(asset.key), false);
    assert.equal(memory.hasObject(sourceKey), true);
    assert.equal(memory.hasObject(vendorKey), true);
  } finally {
    clock.restore();
  }
});

test("terminal job retention is bounded, paged, and limited to broker-owned objects", async () => {
  const clock = useClock();
  try {
    const memory = broker();
    const sourceKey = `sources/${headC}/source.tar.gz`;
    const vendorKey = `cargo-vendor/${headC}/${cargoVendor(headC).sha256}/bundle.tar.gz`;
    memory.seedObject(sourceKey, "shared source");
    memory.seedObject(vendorKey, "shared vendor");
    await queue(memory.durable, workspaceJob(headC, workflowC));
    const claimed = await claimBody(memory.durable);
    const stdout = await uploadedLog(
      memory.durable,
      claimed.claim,
      "stdout",
      "done\n",
    );
    const stderr = await uploadedLog(
      memory.durable,
      claimed.claim,
      "stderr",
      "",
    );
    assert.equal(
      (await complete(memory.durable, claimed.claim, success(stdout, stderr)))
        .status,
      202,
    );
    await memory.durable.alarm();
    const retained = await readJob(memory.durable, claimed.job.id);
    assert.equal(retained.delivery?.state, "delivered");
    assert.equal(
      retained.retention?.deleteAfter,
      new Date(clock.now + CI_MAC_RETENTION_MS).toISOString(),
    );
    for (let index = 0; index < 1_001; index += 1) {
      memory.seedObject(
        `macos/jobs/${claimed.job.id}/attempts/orphan-${
          String(index).padStart(4, "0")
        }/data`,
        "x",
      );
    }

    clock.advance(CI_MAC_RETENTION_MS);
    await memory.durable.alarm();
    assert.equal(
      (await memory.durable.fetch(
        new Request(`https://ci.test/api/ci/macos/jobs/${claimed.job.id}`),
      )).status,
      200,
    );
    assert.equal(
      memory.objectKeys(`macos/jobs/${claimed.job.id}/`).length > 0,
      true,
    );
    await memory.durable.alarm();
    assert.equal(
      (await memory.durable.fetch(
        new Request(`https://ci.test/api/ci/macos/jobs/${claimed.job.id}`),
      )).status,
      404,
    );
    assert.deepEqual(memory.objectKeys(`macos/jobs/${claimed.job.id}/`), []);
    assert.equal(memory.hasObject(sourceKey), true);
    assert.equal(memory.hasObject(vendorKey), true);
    assert.equal(memory.retentionKeys().length, 0);
  } finally {
    clock.restore();
  }
});

test("release job cleanup retains the distribution asset and removes every job object", async () => {
  const clock = useClock();
  try {
    const memory = broker();
    await queue(memory.durable, releaseJob(headA, workflowA));
    const claimed = await claimBody(memory.durable);
    const stdout = await uploadedLog(
      memory.durable,
      claimed.claim,
      "stdout",
      "release\n",
    );
    const stderr = await uploadedLog(
      memory.durable,
      claimed.claim,
      "stderr",
      "",
    );
    const uploaded = await uploadAsset(
      memory.durable,
      claimed.claim,
      new TextEncoder().encode("published-release-mach-o"),
    );
    const asset = await uploaded.json() as CiMacAsset;
    assert.equal(
      (await complete(memory.durable, claimed.claim, {
        ...success(stdout, stderr),
        asset,
      })).status,
      202,
    );
    await memory.durable.alarm();
    const promotedKey =
      `distribution/commit/${headA}/components/macos/nanocodex-aarch64-apple-darwin`;
    memory.seedObject(promotedKey, "published-release-mach-o");

    clock.advance(CI_MAC_RETENTION_MS);
    await memory.durable.alarm();
    assert.equal(
      (await memory.durable.fetch(
        new Request(`https://ci.test/api/ci/macos/jobs/${claimed.job.id}`),
      )).status,
      404,
    );
    assert.equal(memory.hasObject(stdout.key), false);
    assert.equal(memory.hasObject(stderr.key), false);
    assert.equal(memory.hasObject(asset.key), false);
    assert.equal(memory.hasObject(promotedKey), true);
    assert.deepEqual(memory.objectKeys(`macos/jobs/${claimed.job.id}/`), []);
    assert.equal(memory.retentionKeys().length, 0);
  } finally {
    clock.restore();
  }
});

test("release job cleanup removes an asset cancelled before its first outbox send", async () => {
  const clock = useClock();
  try {
    const memory = broker();
    await queue(memory.durable, releaseJob(headA, workflowA));
    const claimed = await claimBody(memory.durable);
    const stdout = await uploadedLog(
      memory.durable,
      claimed.claim,
      "stdout",
      "release\n",
    );
    const stderr = await uploadedLog(
      memory.durable,
      claimed.claim,
      "stderr",
      "",
    );
    const uploaded = await uploadAsset(
      memory.durable,
      claimed.claim,
      new TextEncoder().encode("unpublished-release-mach-o"),
    );
    const asset = await uploaded.json() as CiMacAsset;
    assert.equal(
      (await complete(memory.durable, claimed.claim, {
        ...success(stdout, stderr),
        asset,
      })).status,
      202,
    );
    memory.setWorkflow(workflowA, "complete");
    await memory.durable.alarm();
    const retained = await readJob(memory.durable, claimed.job.id);
    assert.equal(retained.delivery?.state, "cancelled");
    assert.equal(retained.delivery?.attempts, 0);
    assert.equal(memory.events.length, 0);

    clock.advance(CI_MAC_RETENTION_MS);
    await memory.durable.alarm();
    assert.equal(memory.hasObject(asset.key), false);
    assert.equal(
      (await memory.durable.fetch(
        new Request(`https://ci.test/api/ci/macos/jobs/${claimed.job.id}`),
      )).status,
      404,
    );
  } finally {
    clock.restore();
  }
});

function broker(options: {
  sendFailures?: number;
  loseAcknowledgement?: boolean;
  onWorkflowStatus?: () => void;
} = {}) {
  const values = new Map<string, unknown>();
  const objects = new Map<string, StoredObject>();
  const objectBodies = new Map<string, Uint8Array>();
  const r2Reads: string[] = [];
  const workflows = new Map<string, string>([
    [workflowA, "running"],
    [workflowB, "running"],
    [workflowC, "running"],
  ]);
  const events: Array<{ type: string; payload: Record<string, unknown> }> = [];
  let alarm = -1;
  let transactionTail = Promise.resolve();
  let sendFailures = options.sendFailures ?? 0;

  const operations = {
    get: async <T>(key: string) => clone(values.get(key)) as T | undefined,
    put: async (
      keyOrEntries: string | Record<string, unknown>,
      value?: unknown,
    ) => {
      if (typeof keyOrEntries === "string") {
        values.set(keyOrEntries, clone(value));
      } else {for (const [key, entry] of Object.entries(keyOrEntries)) {
          values.set(key, clone(entry));
        }}
    },
    delete: async (key: string | string[]) => {
      if (Array.isArray(key)) {
        return key.reduce(
          (deleted, entry) => values.delete(entry) || deleted,
          false,
        );
      }
      return values.delete(key);
    },
    list: async <T>({ prefix = "" }: { prefix?: string } = {}) =>
      new Map(
        [...values.entries()]
          .filter(([key]) => key.startsWith(prefix))
          .sort(([left], [right]) => left.localeCompare(right))
          .map(([key, value]) => [key, clone(value) as T]),
      ),
    getAlarm: async () => alarm < 0 ? null : alarm,
    setAlarm: async (timestamp: number) => {
      alarm = timestamp;
    },
  };
  const storage = {
    ...operations,
    transaction: <T>(
      callback: (transaction: typeof operations) => Promise<T>,
    ) => {
      const result = transactionTail.then(() => callback(operations));
      transactionTail = result.then(() => undefined, () => undefined);
      return result;
    },
  };
  const state = { storage } as unknown as DurableObjectState;
  const bucket = {
    async put(
      key: string,
      body: ReadableStream | Uint8Array | string,
      putOptions: R2PutOptions = {},
    ) {
      if (objects.has(key) && putOptions.onlyIf) return null;
      const bytes = await readBody(body);
      const actualSha = sha256(bytes);
      const object = storedObject(
        key,
        bytes,
        actualSha,
        putOptions.customMetadata ?? {},
      );
      if (putOptions.sha256 === actualSha) {
        objects.set(key, object);
        objectBodies.set(key, bytes.slice());
      }
      return cloneObject(object);
    },
    async get(key: string) {
      r2Reads.push(key);
      const object = objects.get(key);
      const bytes = objectBodies.get(key);
      if (!object || !bytes) return null;
      const body = new Response(byteBuffer(bytes)).body!;
      return {
        ...cloneObject(object),
        body,
        bodyUsed: false,
        arrayBuffer: async () => byteBuffer(bytes),
        text: async () => new TextDecoder().decode(bytes),
        json: async <T>() => JSON.parse(new TextDecoder().decode(bytes)) as T,
        blob: async () => new Blob([byteBuffer(bytes)]),
        writeHttpMetadata: () => undefined,
      } as unknown as R2ObjectBody;
    },
    async head(key: string) {
      const object = objects.get(key);
      return object ? cloneObject(object) : null;
    },
    async delete(keyOrKeys: string | string[]) {
      for (const key of Array.isArray(keyOrKeys) ? keyOrKeys : [keyOrKeys]) {
        objects.delete(key);
        objectBodies.delete(key);
      }
    },
    async list({ prefix = "", limit = 1_000 }: R2ListOptions = {}) {
      const matching = [...objects.values()]
        .filter(({ key }) => key.startsWith(prefix))
        .sort((left, right) => left.key.localeCompare(right.key));
      const page = matching.slice(0, limit).map(cloneObject);
      return matching.length > page.length
        ? {
          objects: page,
          delimitedPrefixes: [],
          truncated: true,
          cursor: "next",
        }
        : { objects: page, delimitedPrefixes: [], truncated: false };
    },
  };
  const env = {
    BACKUP_BUCKET: bucket,
    CI_WORKFLOW: {
      async get(id: string) {
        return {
          id,
          status: async () => {
            options.onWorkflowStatus?.();
            return { status: workflows.get(id) ?? "unknown" };
          },
          async sendEvent(
            event: { type: string; payload: Record<string, unknown> },
          ) {
            if (options.loseAcknowledgement || sendFailures === 0) {
              events.push(clone(event) as typeof event);
            }
            if (sendFailures > 0) {
              sendFailures -= 1;
              throw new Error("lost Workflow event acknowledgement");
            }
          },
        };
      },
    },
  };
  return {
    durable: new CiMacJobs(state, env as never),
    events,
    setWorkflow(id: string, status: string) {
      workflows.set(id, status);
    },
    seedObject(
      key: string,
      value: string,
      options: { reportedSize?: number } = {},
    ) {
      const bytes = new TextEncoder().encode(value);
      const digest = sha256(bytes);
      const object = storedObject(key, bytes, digest, { sha256: digest });
      if (options.reportedSize !== undefined) {
        object.size = options.reportedSize;
      }
      objects.set(key, object);
      objectBodies.set(key, bytes);
    },
    hasObject(key: string) {
      return objects.has(key);
    },
    objectKeys(prefix = "") {
      return [...objects.keys()].filter((key) => key.startsWith(prefix)).sort();
    },
    retentionKeys() {
      return [...values.keys()].filter((key) => key.startsWith("retention:"))
        .sort();
    },
    r2Reads,
    get alarm() {
      return alarm;
    },
  };
}

function queue(durable: CiMacJobs, body: unknown) {
  return postJson(durable, "/api/ci/macos/jobs", body);
}

function claim(durable: CiMacJobs, worker: string) {
  return postJson(durable, "/api/ci/macos/claims", { worker, host });
}

async function claimBody(durable: CiMacJobs, worker = "mac-worker") {
  const response = await claim(durable, worker);
  assert.equal(response.status, 200);
  return response.json() as Promise<{
    action: "run";
    claim: string;
    leaseExpiresAt: string;
    job: {
      id: string;
      head: string;
      workflowId: string;
      task: string;
      cargoVendor: ReturnType<typeof cargoVendor>;
      publishedAt?: string;
    };
  }>;
}

function heartbeat(durable: CiMacJobs, claimId: string) {
  return durable.fetch(
    new Request(
      `https://ci.test/api/ci/macos/claims/${claimId}/heartbeat`,
      { method: "POST" },
    ),
  );
}

function complete(durable: CiMacJobs, claimId: string, body: unknown) {
  return postJson(durable, `/api/ci/macos/claims/${claimId}/complete`, body);
}

async function readJob(
  durable: CiMacJobs,
  id: string,
): Promise<CiMacJobRecord> {
  const response = await durable.fetch(
    new Request(`https://ci.test/api/ci/macos/jobs/${id}`),
  );
  assert.equal(response.status, 200);
  return (await response.json() as { job: CiMacJobRecord }).job;
}

function postJson(durable: CiMacJobs, path: string, body: unknown) {
  return durable.fetch(
    new Request(`https://ci.test${path}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    }),
  );
}

async function uploadedLog(
  durable: CiMacJobs,
  claimId: string,
  stream: "stdout" | "stderr",
  text: string,
): Promise<CiMacLog> {
  const response = await uploadLog(
    durable,
    claimId,
    stream,
    new TextEncoder().encode(text),
  );
  assert.equal(response.status, 201);
  return response.json() as Promise<CiMacLog>;
}

function uploadLog(
  durable: CiMacJobs,
  claimId: string,
  stream: "stdout" | "stderr",
  body: Uint8Array,
  overrides: { length?: number; sha256?: string } = {},
) {
  return durable.fetch(
    new Request(
      `https://ci.test/api/ci/macos/claims/${claimId}/logs/${stream}`,
      {
        method: "PUT",
        headers: {
          "content-length": String(overrides.length ?? body.byteLength),
          "content-type": "text/plain; charset=utf-8",
          "x-nanocodex-sha256": overrides.sha256 ?? sha256(body),
        },
        body: byteBuffer(body),
      },
    ),
  );
}

function uploadAsset(durable: CiMacJobs, claimId: string, body: Uint8Array) {
  return durable.fetch(
    new Request(
      `https://ci.test/api/ci/macos/claims/${claimId}/asset`,
      {
        method: "PUT",
        headers: {
          "content-length": String(body.byteLength),
          "content-type": "application/octet-stream",
          "x-nanocodex-name": "nanocodex-aarch64-apple-darwin",
          "x-nanocodex-sha256": sha256(body),
        },
        body: byteBuffer(body),
      },
    ),
  );
}

function success(stdout: CiMacLog, stderr: CiMacLog) {
  return {
    outcome: "success",
    exitCode: 0,
    durationMs: 12_345,
    logs: { stdout, stderr },
  } as const;
}

function byteBuffer(bytes: Uint8Array): ArrayBuffer {
  const copy = new Uint8Array(bytes.byteLength);
  copy.set(bytes);
  return copy.buffer;
}

function workspaceJob(head: string, workflowId: string) {
  return {
    head,
    workflowId,
    task: "workspace-test",
    source: source(head),
    cargoVendor: cargoVendor(head),
  } as const;
}

function nativeJob(head: string, workflowId: string) {
  return {
    head,
    workflowId,
    task: "native-build",
    source: source(head),
    cargoVendor: cargoVendor(head),
    publishedAt,
  } as const;
}

function releaseJob(head: string, workflowId: string) {
  return {
    head,
    workflowId,
    task: "release-build",
    source: source(head),
    cargoVendor: cargoVendor(head),
    release: {
      channel: "nightly",
      tagName: "nightly",
      buildTimestamp: "2026-08-22T00:00:00.000Z",
    },
  } as const;
}

function source(head: string) {
  return {
    url: `https://ci.test/api/ci/source/${head}/archive`,
    size: 123_456,
    sha256: "1".repeat(64),
  };
}

function cargoVendor(cargoLockBlob: string) {
  const sha256 = "2".repeat(64);
  return {
    url: `https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/${sha256}/bundle.tar.gz`,
    size: 7_654_321,
    sha256,
  };
}

function sha256(value: string | Uint8Array): string {
  return createHash("sha256").update(value).digest("hex");
}

type StoredObject = {
  key: string;
  size: number;
  customMetadata: Record<string, string>;
  checksums: { sha256: ArrayBuffer };
};

function storedObject(
  key: string,
  bytes: Uint8Array,
  digest: string,
  customMetadata: Record<string, string>,
): StoredObject {
  return {
    key,
    size: bytes.byteLength,
    customMetadata: { ...customMetadata },
    checksums: { sha256: Uint8Array.from(Buffer.from(digest, "hex")).buffer },
  };
}

function cloneObject(object: StoredObject): R2Object {
  return clone(object) as unknown as R2Object;
}

async function readBody(
  body: ReadableStream | Uint8Array | string,
): Promise<Uint8Array> {
  if (body instanceof Uint8Array) return body;
  if (typeof body === "string") return new TextEncoder().encode(body);
  return new Uint8Array(await new Response(body).arrayBuffer());
}

function clone<T>(value: T): T {
  return value === undefined ? value : structuredClone(value);
}

function useClock(start = Date.parse("2026-08-22T00:00:00.000Z")) {
  const original = Date.now;
  let now = start;
  Date.now = () => now;
  return {
    get now() {
      return now;
    },
    advance(milliseconds: number) {
      now += milliseconds;
    },
    restore() {
      Date.now = original;
    },
  };
}
