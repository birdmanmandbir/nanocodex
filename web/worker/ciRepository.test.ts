import assert from "node:assert/strict";
import { test } from "node:test";

import {
  ciWorkflowParams,
  CiRepository,
  isCiTerminalResult,
  type CiReleaseCommitReservation,
  type CiReleaseCommitReservationProof,
  type CiRunRecord,
} from "./ciRepository.ts";
import type { CiSourcePublication, NanocodexCiParams } from "./ciSource.ts";

const headA = "a".repeat(40);
const headB = "b".repeat(40);
const headC = "e".repeat(40);
const headD = "f".repeat(40);
const closeIdA = "00000000-0000-4000-8000-000000000001";
const closeIdB = "00000000-0000-4000-8000-000000000002";

test("CI publication CAS commits source and one durable dispatch outbox", async () => {
  const memory = repository();
  const accepted = await publish(memory.durable, null, publication(headA));
  assert.equal(accepted.status, 202);
  assert.ok(memory.alarm > 0);

  const stale = await publish(memory.durable, null, publication(headB));
  assert.equal(stale.status, 409);
  assert.deepEqual(await stale.json(), { error: "publication_conflict", currentHead: headA });

  const replay = await publish(memory.durable, null, publication(headA));
  assert.equal(replay.status, 200);
  assert.equal((await replay.json() as { run: CiRunRecord }).run.workflowId, `ci-${headA}`);

  await memory.durable.alarm();
  assert.equal(memory.created, 1);
  assert.deepEqual(memory.createdParams?.providerData, {
    lane: {
      type: "master",
      deployable: true,
      branch: "master",
      ref: "refs/heads/master",
      head: headA,
    },
    archiveKey: `sources/${headA}/source.tar.gz`,
    archiveSha256: "1".repeat(64),
    archiveSize: 123,
    treeKey: `sources/${headA}/tree.json`,
    treeSha256: "2".repeat(64),
    cargoLockBlob: "c".repeat(40),
    cargoVendorKey: `cargo-vendor/${"c".repeat(40)}/${"3".repeat(64)}/bundle.tar.gz`,
    cargoVendorSha256: "3".repeat(64),
    cargoVendorSize: 4_000_000,
    rustSecRevision: "d".repeat(40),
    rustSecKey: `rustsec-advisory-db/${"d".repeat(40)}/bundle.tar.gz`,
    rustSecSha256: "4".repeat(64),
    rustSecSize: 1_400_000,
    publishedAt: "2026-08-21T00:00:00.000Z",
  });
  const state = await memory.durable.fetch(new Request("https://ci.test/state"));
  assert.equal(state.status, 200);
  const body = await state.json() as { publication: CiSourcePublication; run: CiRunRecord };
  assert.equal(body.publication.head, headA);
  assert.equal(body.run.state, "dispatched");
  assert.equal(body.run.attempts, 1);
});

test("release-commit reservation linearizes master publication without blocking PRs", async () => {
  const racedBefore = repository();
  assert.equal((await publish(racedBefore.durable, null, publication(headA))).status, 202);
  await racedBefore.durable.alarm();
  assert.equal((await publish(racedBefore.durable, headA, publication(headB))).status, 202);
  await racedBefore.durable.alarm();
  const stale = await acquireReleaseCommitReservation(
    racedBefore.durable,
    releaseReservationRequest(headA),
  );
  assert.equal(stale.status, 409);
  assert.deepEqual(await stale.json(), { error: "release_head_stale" });

  const memory = repository();
  assert.equal((await publish(memory.durable, null, publication(headA))).status, 202);
  await memory.durable.alarm();
  const acquired = await acquireReleaseCommitReservation(
    memory.durable,
    releaseReservationRequest(headA),
  );
  assert.equal(acquired.status, 201);
  const proof = await acquired.json() as CiReleaseCommitReservationProof;
  assertReleaseCommitReservation(proof.reservation, headA, 1);
  assert.equal(proof.publication.head, headA);
  assert.equal(proof.run.head, headA);
  assert.equal(proof.run.state, "dispatched");

  const blocked = await publish(memory.durable, headA, publication(headB));
  assert.equal(blocked.status, 409);
  assert.deepEqual(await blocked.json(), {
    error: "release_commit_reserved",
    commit: headA,
    expiresAt: proof.reservation.expiresAt,
  });
  assert.equal(
    (await (await memory.durable.fetch(new Request("https://ci.test/state"))).json() as {
      publication: CiSourcePublication;
    }).publication.head,
    headA,
  );

  const pr = await publish(
    memory.durable,
    null,
    pullRequestPublication(headB, headC, 41),
  );
  assert.equal(pr.status, 202, "PR publication does not mutate the master pointer");
  assert.equal(
    (await (await memory.durable.fetch(
      new Request("https://ci.test/pull-requests/41/state"),
    )).json() as { publication: CiSourcePublication }).publication.head,
    headB,
  );

  assert.equal(
    (await releaseReleaseCommitReservation(memory.durable, proof.reservation)).status,
    204,
  );
  assert.equal((await publish(memory.durable, headA, publication(headB))).status, 202);
});

test("release-commit reservations replay, heartbeat, expire, and reclaim monotonically", async () => {
  const memory = repository();
  assert.equal((await publish(memory.durable, null, publication(headA))).status, 202);
  await memory.durable.alarm();
  const request = releaseReservationRequest(headA);
  const firstResponse = await acquireReleaseCommitReservation(memory.durable, request);
  assert.equal(firstResponse.status, 201);
  const first = (await firstResponse.json() as CiReleaseCommitReservationProof).reservation;
  assertReleaseCommitReservation(first, headA, 1);

  memory.setState("reservation:release-commit:active", {
    ...first,
    expiresAt: new Date(Date.now() + 1_000).toISOString(),
  });
  const replayResponse = await acquireReleaseCommitReservation(memory.durable, request);
  assert.equal(replayResponse.status, 200, "a lost acquire acknowledgement replays");
  const replay = (await replayResponse.json() as CiReleaseCommitReservationProof).reservation;
  assert.equal(replay.reservationId, first.reservationId);
  assert.ok(Date.parse(replay.expiresAt) > Date.now() + 118_000);
  assert.equal(
    (await heartbeatReleaseCommitReservation(memory.durable, replay)).status,
    200,
  );

  memory.setState("reservation:release-commit:active", {
    ...replay,
    acquiredAt: new Date(Date.now() - 1_000).toISOString(),
    renewedAt: new Date(Date.now() - 1_000).toISOString(),
    expiresAt: new Date(Date.now() - 1).toISOString(),
  });
  const successorRequest = releaseReservationRequest(headA, {
    owner: "release-controller:successor",
    releaseId: "v1.2.4",
    publicationLeaseId: `2.${closeIdB}`,
    publicationLeaseGeneration: 2,
  });
  const successorResponse = await acquireReleaseCommitReservation(
    memory.durable,
    successorRequest,
  );
  assert.equal(successorResponse.status, 201);
  const successor = (await successorResponse.json() as CiReleaseCommitReservationProof)
    .reservation;
  assertReleaseCommitReservation(successor, headA, 2);
  assert.equal(
    (await heartbeatReleaseCommitReservation(memory.durable, replay)).status,
    409,
  );
  assert.equal(
    (await releaseReleaseCommitReservation(memory.durable, replay)).status,
    204,
    "a stale release cannot clear its successor",
  );
  assert.equal(
    (await publish(memory.durable, headA, publication(headB))).status,
    409,
  );
  assert.equal(
    (await releaseReleaseCommitReservation(memory.durable, successor)).status,
    204,
  );
  assert.equal(
    (await releaseReleaseCommitReservation(memory.durable, successor)).status,
    204,
    "a lost release acknowledgement is replayable",
  );
  assert.equal((await publish(memory.durable, headA, publication(headB))).status, 202);
});

test("terminal CI evidence is complete and bound to one exact run identity", () => {
  const valid = {
    version: 1,
    head: headA,
    workflowId: `ci-${headA}`,
    status: "success",
    completedAt: "2026-08-22T00:00:00.000Z",
  };
  assert.equal(isCiTerminalResult(valid, headA, `ci-${headA}`), true);
  for (const malformed of [
    { status: "success" },
    { ...valid, version: 2 },
    { ...valid, head: headB },
    { ...valid, workflowId: `ci-${headB}` },
    { ...valid, status: "running" },
    { ...valid, completedAt: "not-a-timestamp" },
  ]) {
    assert.equal(isCiTerminalResult(malformed, headA, `ci-${headA}`), false);
  }
});

test("Workflow dispatch reconstructs poisoned outbox params and publication replay stays safe", async () => {
  const memory = repository();
  const next = pullRequestPublication(headA, headB, 42);
  assert.equal((await publish(memory.durable, null, next)).status, 202);
  const key = `outbox:${headA}`;
  const outbox = memory.stateValue(key) as {
    version: 1;
    run: CiRunRecord;
    params: NanocodexCiParams;
  };
  memory.setState(key, {
    ...outbox,
    params: {
      sha: headA,
      branch: "master",
      ref: "refs/heads/master",
      providerData: {
        lane: { type: "master", head: headA },
        archiveKey: `sources/${headB}/source.tar.gz`,
      },
    },
  });

  const replay = await publish(memory.durable, null, next);
  assert.equal(replay.status, 200);
  assert.equal((await replay.json() as { reusedWorkflow: boolean }).reusedWorkflow, true);
  await memory.durable.alarm();
  assert.equal(memory.created, 1);
  assert.deepEqual(memory.createdParams, ciWorkflowParams(next, null));
});

test("Workflow dispatch and publication replay reject mismatched retained identity", async (t) => {
  const cases = ["workflow", "head", "lane", "source", "outbox"] as const;
  for (const kind of cases) {
    await t.test(kind, async () => {
      const memory = repository();
      const next = publication(headA);
      assert.equal((await publish(memory.durable, null, next)).status, 202);
      const outboxKey = `outbox:${headA}`;
      const outbox = memory.stateValue(outboxKey) as {
        version: 1;
        run: CiRunRecord;
        params: NanocodexCiParams;
      };
      if (kind === "workflow") {
        memory.setState(outboxKey, {
          ...outbox,
          run: { ...outbox.run, workflowId: `ci-${headB}` },
        });
      } else if (kind === "head") {
        memory.setState(outboxKey, {
          ...outbox,
          run: { ...outbox.run, head: headB, workflowId: `ci-${headB}` },
        });
      } else if (kind === "outbox") {
        memory.setState(outboxKey, {
          ...outbox,
          run: { ...outbox.run, attempts: outbox.run.attempts + 1 },
        });
      } else {
        const sourceKey = `source:${headA}`;
        const source = memory.stateValue(sourceKey) as {
          version: 1;
          head: string;
          firstPublication: CiSourcePublication;
        };
        const firstPublication = kind === "lane"
          ? pullRequestPublication(headA, headB, 9)
          : {
            ...source.firstPublication,
            archive: { ...source.firstPublication.archive, sha256: "9".repeat(64) },
          };
        memory.setState(sourceKey, { ...source, firstPublication });
      }

      const replay = await publishWithLease(memory.durable, null, next, closeIdA);
      assert.equal(replay.status, 503);
      assert.deepEqual(await replay.json(), { error: "repository_state_invalid" });
      await assert.rejects(
        memory.durable.alarm(),
        /CI Workflow outbox contains invalid repository state/,
      );
      assert.equal(memory.created, 0);
    });
  }
});

test("pull request merge publications have isolated CAS state and typed params", async () => {
  const memory = repository();
  assert.equal((await publish(memory.durable, null, publication(headA))).status, 202);
  await memory.durable.alarm();

  const pullRequest = pullRequestPublication(headB, headC, 42);
  assert.equal((await publish(memory.durable, null, pullRequest)).status, 202);
  await memory.durable.alarm();

  const masterState = await memory.durable.fetch(new Request("https://ci.test/state"));
  assert.equal(
    (await masterState.json() as { publication: CiSourcePublication }).publication.head,
    headA,
  );
  const pullRequestState = await memory.durable.fetch(
    new Request("https://ci.test/pull-requests/42/state"),
  );
  assert.equal(pullRequestState.status, 200);
  const pullRequestBody = await pullRequestState.json() as {
    publication: CiSourcePublication;
    run: CiRunRecord;
  };
  assert.deepEqual(pullRequestBody.publication.lane, {
    type: "pull_request",
    number: 42,
    pullRequestHead: headC,
  });
  assert.equal(pullRequestBody.publication.head, headB);
  assert.equal(pullRequestBody.run.workflowId, `ci-${headB}`);
  assert.deepEqual(memory.paramsFor(headB)?.providerData.lane, {
    type: "pull_request",
    deployable: false,
    number: 42,
    branch: "pull/42/merge",
    ref: "refs/pull/42/merge",
    mergeHead: headB,
    pullRequestHead: headC,
  });
  assert.deepEqual(
    {
      event: memory.paramsFor(headB)?.event,
      trigger: memory.paramsFor(headB)?.trigger,
      branch: memory.paramsFor(headB)?.branch,
      ref: memory.paramsFor(headB)?.ref,
      sha: memory.paramsFor(headB)?.sha,
    },
    {
      event: { type: "push" },
      trigger: "push",
      branch: "pull/42/merge",
      ref: "refs/pull/42/merge",
      sha: headB,
    },
  );
  assert.throws(
    () => ciWorkflowParams(pullRequest, null, {
      version: 1,
      channel: "nightly",
      tagName: "nightly",
      buildTimestamp: "2026-08-22T00:00:00.000Z",
    }),
    /deployable master source/,
  );

  const stale = await publish(
    memory.durable,
    null,
    pullRequestPublication(headD, headA, 42),
  );
  assert.equal(stale.status, 409);
  assert.deepEqual(await stale.json(), {
    error: "publication_conflict",
    currentHead: headB,
  });
  assert.equal(
    (await (await memory.durable.fetch(new Request("https://ci.test/state"))).json() as {
      publication: CiSourcePublication;
    }).publication.head,
    headA,
  );

  const staleClose = await closePullRequest(
    memory.durable,
    42,
    closeIdA,
    headB,
    headA,
  );
  assert.equal(staleClose.status, 409);
  assert.deepEqual(await staleClose.json(), {
    error: "publication_conflict",
    currentMergeHead: headB,
    currentPullRequestHead: headC,
  });
  const closed = await closePullRequest(memory.durable, 42, closeIdA, headB, headC);
  assert.equal(closed.status, 200);
  const closedBody = await closed.json() as {
    closed: boolean;
    closeId: string;
    mergeHead: string;
    pullRequestHead: string;
    replay: boolean;
  };
  assert.deepEqual(
    {
      closed: closedBody.closed,
      mergeHead: closedBody.mergeHead,
      pullRequestHead: closedBody.pullRequestHead,
      replay: closedBody.replay,
    },
    {
      closed: true,
      mergeHead: headB,
      pullRequestHead: headC,
      replay: false,
    },
  );
  const closedState = await memory.durable.fetch(
    new Request("https://ci.test/pull-requests/42/state"),
  );
  assert.equal(closedState.status, 404);
  assert.equal(
    (await closedState.json() as { error: string }).error,
    "pull_request_closed",
  );
  assert.equal(
    (await memory.durable.fetch(
      new Request(`https://ci.test/pull-requests/42/publications/${headB}`),
    )).status,
    200,
  );
  assert.equal(
    (await closePullRequest(memory.durable, 42, closeIdA, headB, headC)).status,
    200,
  );
  const stalePublication = await publish(memory.durable, null, pullRequest);
  assert.equal(stalePublication.status, 409);
  assert.equal(
    (await stalePublication.json() as { error: string }).error,
    "pull_request_closed",
  );
  const reopened = await reopenPullRequest(memory.durable, null, pullRequest, {
    closeId: closedBody.closeId,
    mergeHead: headB,
    pullRequestHead: headC,
  });
  assert.equal(reopened.status, 202);
  const reopenReplay = await reopenPullRequest(memory.durable, null, pullRequest, {
    closeId: closedBody.closeId,
    mergeHead: headB,
    pullRequestHead: headC,
  });
  assert.equal(reopenReplay.status, 200, "a lost reopen acknowledgement is replayable");
  const delayedClose = await closePullRequest(
    memory.durable,
    42,
    closeIdA,
    headB,
    headC,
  );
  assert.equal(delayedClose.status, 200);
  assert.equal(
    (await delayedClose.json() as { replay: boolean }).replay,
    true,
    "a delayed close operation cannot close the reopened generation",
  );
  assert.equal(
    (await memory.durable.fetch(
      new Request("https://ci.test/pull-requests/42/state"),
    )).status,
    200,
  );
});

test("historical publications require CAS before either lane can reuse them", async () => {
  const memory = repository();
  const masterA = publication(headA);
  const masterB = publication(headB);
  assert.equal((await publish(memory.durable, null, masterA)).status, 202);
  assert.equal((await publish(memory.durable, headA, masterB)).status, 202);
  assert.equal((await publish(memory.durable, null, masterA)).status, 409);

  const pullRequestA = pullRequestPublication(headC, headA, 17);
  const pullRequestB = pullRequestPublication(headD, headB, 17);
  assert.equal((await publish(memory.durable, null, pullRequestA)).status, 202);
  assert.equal((await publish(memory.durable, headC, pullRequestB)).status, 202);
  assert.equal((await publish(memory.durable, null, pullRequestA)).status, 409);

  const [
    masterState,
    retainedMasterA,
    pullRequestState,
    retainedPullRequestA,
  ] = await Promise.all([
    memory.durable.fetch(new Request("https://ci.test/state")),
    memory.durable.fetch(new Request(`https://ci.test/master/publications/${headA}`)),
    memory.durable.fetch(new Request("https://ci.test/pull-requests/17/state")),
    memory.durable.fetch(
      new Request(`https://ci.test/pull-requests/17/publications/${headC}`),
    ),
  ]);
  assert.equal(
    (await masterState.json() as { publication: CiSourcePublication }).publication.head,
    headB,
  );
  assert.equal(retainedMasterA.status, 200);
  const retained = await retainedMasterA.json() as {
    publication: CiSourcePublication;
    run: CiRunRecord;
  };
  assert.equal(retained.publication.head, headA);
  assert.equal(retained.publication.lane?.type, "master");
  assert.equal(retained.run.workflowId, `ci-${headA}`);
  assert.equal(retainedPullRequestA.status, 200);
  assert.equal(
    (await retainedPullRequestA.json() as {
      publication: CiSourcePublication;
    }).publication.head,
    headC,
  );
  assert.equal(
    (await pullRequestState.json() as { publication: CiSourcePublication }).publication.head,
    headD,
  );

  const reused = await publish(memory.durable, headD, pullRequestA);
  assert.equal(reused.status, 202);
  assert.equal((await reused.json() as { reusedWorkflow: boolean }).reusedWorkflow, true);
  assert.equal(
    (await (await memory.durable.fetch(
      new Request("https://ci.test/pull-requests/17/state"),
    )).json() as { publication: CiSourcePublication }).publication.head,
    headC,
  );
});

test("one source SHA reuses its global workflow when a tested merge becomes master", async () => {
  const memory = repository();
  const pullRequest = pullRequestPublication(
    headA,
    headB,
    7,
    "2026-08-21T00:00:00.000Z",
  );
  assert.equal((await publish(memory.durable, null, pullRequest)).status, 202);
  await memory.durable.alarm();
  assert.equal(memory.created, 1);
  assert.equal(
    (await memory.durable.fetch(
      new Request(`https://ci.test/master/publications/${headA}`),
    )).status,
    404,
    "a PR-only global run is not a master publication",
  );

  const master = publication(headA, "2026-08-22T00:00:00.000Z");
  const promoted = await publish(memory.durable, null, master);
  assert.equal(promoted.status, 202);
  assert.equal((await promoted.json() as { reusedWorkflow: boolean }).reusedWorkflow, true);
  await memory.durable.alarm();
  assert.equal(memory.created, 1);

  const [masterState, retainedMaster, pullRequestState, run] = await Promise.all([
    memory.durable.fetch(new Request("https://ci.test/state")),
    memory.durable.fetch(new Request(`https://ci.test/master/publications/${headA}`)),
    memory.durable.fetch(new Request("https://ci.test/pull-requests/7/state")),
    memory.durable.fetch(new Request(`https://ci.test/runs/${headA}`)),
  ]);
  const masterBody = await masterState.json() as {
    publication: CiSourcePublication;
    run: CiRunRecord;
  };
  const pullRequestBody = await pullRequestState.json() as {
    publication: CiSourcePublication;
    run: CiRunRecord;
  };
  assert.deepEqual(masterBody.publication.lane, { type: "master" });
  assert.equal(pullRequestBody.publication.lane?.type, "pull_request");
  assert.equal(masterBody.run.workflowId, pullRequestBody.run.workflowId);
  assert.deepEqual(await retainedMaster.json(), masterBody);
  assert.deepEqual(await run.json(), masterBody.run);

  const replay = await publish(memory.durable, null, master);
  assert.equal(replay.status, 200);

  const conflictingSource = publication(headA, "2026-08-23T00:00:00.000Z");
  conflictingSource.archive.sha256 = "9".repeat(64);
  const conflict = await publish(
    memory.durable,
    null,
    pullRequestPublication(headA, headC, 8),
  );
  assert.equal(conflict.status, 202, "another PR may reuse identical content");

  const conflictMemory = repository();
  await publish(conflictMemory.durable, null, pullRequest);
  const rejected = await publish(conflictMemory.durable, null, conflictingSource);
  assert.equal(rejected.status, 409);
  assert.deepEqual(await rejected.json(), { error: "source_conflict" });
  assert.equal(
    (await conflictMemory.durable.fetch(new Request("https://ci.test/state"))).status,
    404,
  );
});

test("a lost Workflow create acknowledgement retries the deterministic ID", async () => {
  const memory = repository({ failAfterCreate: true });
  await publish(memory.durable, null, publication(headA));
  await memory.durable.alarm();
  assert.equal(memory.created, 1);
  const run = await memory.durable.fetch(new Request(`https://ci.test/runs/${headA}`));
  assert.equal((await run.json() as CiRunRecord).state, "dispatched");
});

test("a failing dispatch does not block a later publication", async () => {
  const memory = repository({ failHead: headA });
  await publish(memory.durable, null, publication(headA));
  await publish(memory.durable, headA, publication(headB));

  await memory.durable.alarm();
  const failed = await memory.durable.fetch(new Request(`https://ci.test/runs/${headA}`));
  assert.equal((await failed.json() as CiRunRecord).state, "pending");

  await memory.durable.alarm();
  const dispatched = await memory.durable.fetch(new Request(`https://ci.test/runs/${headB}`));
  assert.equal((await dispatched.json() as CiRunRecord).state, "dispatched");
});

test("retention never deletes active or current publications", async () => {
  const memory = repository();
  let expected: string | null = null;
  for (let index = 1; index <= 105; index++) {
    const next = index.toString(16).padStart(40, "0");
    assert.equal((await publish(memory.durable, expected, publication(next))).status, 202);
    await memory.durable.alarm();
    expected = next;
  }
  const state = await memory.durable.fetch(new Request("https://ci.test/state"));
  assert.equal((await state.json() as { publication: CiSourcePublication }).publication.head, expected);
  const runs = await memory.durable.fetch(new Request("https://ci.test/runs"));
  const history = await runs.json() as {
    runs: CiRunRecord[];
    retainedCount: number;
    retentionLimit: number;
  };
  assert.equal(history.runs.length, 50);
  assert.equal(history.retainedCount, 105);
  assert.equal(history.retentionLimit, 100);
  assert.equal(
    (await memory.durable.fetch(new Request(`https://ci.test/runs/${"1".padStart(40, "0")}`))).status,
    200,
  );
});

test("retention waits for PR-close cancellation cleanup before retiring shared source", async () => {
  const originalNow = Date.now;
  let now = Date.UTC(2026, 7, 22, 9, 0, 0);
  Date.now = () => now;
  try {
    const memory = repository();
    const pullRequestHead = "1".padStart(40, "0");
    assert.equal(
      (await publish(
        memory.durable,
        null,
        pullRequestPublication(pullRequestHead, headA, 9),
      )).status,
      202,
    );
    await memory.durable.alarm();
    memory.complete(pullRequestHead);
    memory.seedSandbox(pullRequestHead, "linux-12345678-1234-4123-8123-123456789abc");

    let expectedMaster: string | null = null;
    const firstUnreferencedMaster = "2".padStart(40, "0");
    for (let index = 2; index <= 102; index++) {
      const next = index.toString(16).padStart(40, "0");
      assert.equal(
        (await publish(memory.durable, expectedMaster, publication(next))).status,
        202,
      );
      await memory.durable.alarm();
      if (index === 2) memory.complete(next);
      expectedMaster = next;
    }

    assert.equal(
      (await memory.durable.fetch(new Request(`https://ci.test/runs/${pullRequestHead}`))).status,
      200,
    );
    assert.equal(
      (await memory.durable.fetch(
        new Request(`https://ci.test/runs/${firstUnreferencedMaster}`),
      )).status,
      404,
    );
    assert.deepEqual(memory.sourceDeletes, [
      `sources/${firstUnreferencedMaster}/source.tar.gz`,
      `sources/${firstUnreferencedMaster}/tree.json`,
    ]);

    assert.ok(expectedMaster);
    const closed = await closePullRequest(
      memory.durable,
      9,
      closeIdB,
      pullRequestHead,
      headA,
    );
    assert.equal(closed.status, 200);
    assert.equal(
      (await memory.durable.fetch(new Request(`https://ci.test/runs/${pullRequestHead}`))).status,
      200,
      "pending cancellation keeps its run and source durable",
    );
    assert.deepEqual(memory.sourceDeletes, [
      `sources/${firstUnreferencedMaster}/source.tar.gz`,
      `sources/${firstUnreferencedMaster}/tree.json`,
    ]);

    now = memory.alarm + 1;
    await memory.durable.alarm();
    assert.deepEqual(memory.terminated, [], "terminal Workflow evidence avoids a redundant terminate");
    assert.deepEqual(memory.destroyedSandboxes, [
      "linux-12345678-1234-4123-8123-123456789abc",
    ]);
    assert.deepEqual(memory.terminationMarker(pullRequestHead), {
      status: "complete",
      head: pullRequestHead,
      workflowId: `ci-${pullRequestHead}`,
    });
    assert.equal(
      (await memory.durable.fetch(new Request(`https://ci.test/runs/${pullRequestHead}`))).status,
      404,
      "retention runs only after durable cancellation evidence and sandbox cleanup",
    );
    assert.deepEqual(memory.sourceDeletes, [
      `sources/${firstUnreferencedMaster}/source.tar.gz`,
      `sources/${firstUnreferencedMaster}/tree.json`,
      `sources/${pullRequestHead}/source.tar.gz`,
      `sources/${pullRequestHead}/tree.json`,
    ]);
    const [masterState, pullRequestState] = await Promise.all([
      memory.durable.fetch(new Request("https://ci.test/state")),
      memory.durable.fetch(new Request("https://ci.test/pull-requests/9/state")),
    ]);
    assert.equal(
      (await masterState.json() as { publication: CiSourcePublication }).publication.head,
      expectedMaster,
    );
    assert.equal(
      (await pullRequestState.json() as { error: string }).error,
      "pull_request_closed",
    );

    const laterHead = "7".repeat(40);
    assert.equal(
      (await publish(
        memory.durable,
        expectedMaster,
        publication(laterHead, new Date(now + 1_000).toISOString()),
      )).status,
      202,
    );
    await memory.durable.alarm();
    const later = await memory.durable.fetch(new Request(`https://ci.test/runs/${laterHead}`));
    assert.equal((await later.json() as CiRunRecord).state, "dispatched");
  } finally {
    Date.now = originalNow;
  }
});

test("retention accepts only exact terminal evidence after Workflow expiry and protects leases", async () => {
  const memory = repository();
  let expected: string | null = null;
  let leasedHead = "";
  let terminalHead = "";
  let mismatchedHead = "";
  let activeHead = "";
  for (let index = 1; index <= 102; index++) {
    const head = index.toString(16).padStart(40, "0");
    const publishedAt = new Date(Date.UTC(2026, 7, 1, 0, 0, index)).toISOString();
    assert.equal(
      (await publish(memory.durable, expected, publication(head, publishedAt))).status,
      202,
    );
    await memory.durable.alarm();
    if (index === 1) {
      leasedHead = head;
      memory.completeResult(head);
      memory.expireWorkflow(head);
      const first = await acquireDistributionLease(
        memory.durable,
        head,
        `nightly-${head}`,
      );
      assert.equal(first.status, 201);
      const lease = (await first.json() as { lease: { leaseId: string; expiresAt: string } }).lease;
      const replay = await acquireDistributionLease(memory.durable, head, `nightly-${head}`);
      assert.equal(replay.status, 200);
      assert.deepEqual((await replay.json() as { lease: unknown }).lease, lease);
      const stable = await acquireDistributionLease(
        memory.durable,
        head,
        `release-v1.2.3-${head}`,
      );
      assert.equal(stable.status, 201, "stable and nightly have independent exact leases");
    } else if (index === 2) {
      terminalHead = head;
      memory.completeResult(head);
      memory.expireWorkflow(head);
    } else if (index === 3) {
      mismatchedHead = head;
      memory.completeResult(head, { workflowId: `ci-${"9".repeat(40)}` });
      memory.expireWorkflow(head);
    } else if (index === 4) {
      activeHead = head;
      memory.completeResult(head);
    }
    expected = head;
  }

  assert.equal(
    (await memory.durable.fetch(new Request(`https://ci.test/runs/${leasedHead}`))).status,
    200,
    "an unexpired exact distribution lease protects retained source",
  );
  assert.equal(
    (await memory.durable.fetch(new Request(`https://ci.test/runs/${terminalHead}`))).status,
    404,
    "exact retained success evidence retires a run after Workflow metadata expires",
  );
  assert.equal(
    (await memory.durable.fetch(new Request(`https://ci.test/runs/${mismatchedHead}`))).status,
    200,
    "mismatched retained evidence is never terminal proof",
  );
  assert.equal(
    (await memory.durable.fetch(new Request(`https://ci.test/runs/${activeHead}`))).status,
    200,
    "live nonterminal Workflow metadata overrides stale terminal bytes",
  );
});

test("PR cancellation deadline commits with supersession and replay rearms it", async () => {
  const originalNow = Date.now;
  const now = Date.UTC(2026, 7, 22, 11, 0, 0);
  Date.now = () => now;
  try {
    const memory = repository();
    const first = pullRequestPublication(headA, headB, 7);
    const replacement = pullRequestPublication(headC, headD, 7);
    assert.equal((await publish(memory.durable, null, first)).status, 202);
    await memory.durable.alarm();
    assert.equal(
      (await publish(
        memory.durable,
        null,
        pullRequestPublication(headC, headB, 8),
      )).status,
      202,
    );
    await memory.durable.alarm();

    memory.failAlarmAfterWrites(1);
    await assert.rejects(
      publish(memory.durable, headA, replacement),
      /injected alarm acknowledgement loss/,
    );
    assert.equal(
      memory.alarm,
      now + 2 * 60 * 1_000,
      "the cancellation deadline is armed in the pointer transaction",
    );
    assert.equal(
      (await (await memory.durable.fetch(
        new Request("https://ci.test/pull-requests/7/state"),
      )).json() as { publication: CiSourcePublication }).publication.head,
      headC,
      "the pointer commit survives the lost response",
    );

    memory.simulateLostAlarm();
    const replay = await publish(memory.durable, headA, replacement);
    assert.equal(replay.status, 200);
    assert.equal(memory.alarm, now + 2 * 60 * 1_000);
  } finally {
    Date.now = originalNow;
  }
});

test("PR supersession waits for grace and every shared reference before fenced cancellation", async () => {
  const originalNow = Date.now;
  let now = Date.UTC(2026, 7, 22, 12, 0, 0);
  Date.now = () => now;
  try {
    const memory = repository();
    assert.equal(
      (await publish(memory.durable, null, pullRequestPublication(headA, headB, 7))).status,
      202,
    );
    await memory.durable.alarm();
    assert.equal(
      (await publish(memory.durable, null, pullRequestPublication(headA, headC, 8))).status,
      202,
    );
    assert.equal((await publish(memory.durable, null, publication(headA))).status, 202);
    const lease = await acquireDistributionLease(
      memory.durable,
      headA,
      `nightly-${headA}`,
    );
    assert.equal(lease.status, 201);

    assert.equal(
      (await publish(memory.durable, headA, pullRequestPublication(headB, headC, 7))).status,
      202,
    );
    await memory.durable.alarm();
    now += 3 * 60 * 1_000;
    await memory.durable.alarm();
    assert.deepEqual(memory.terminated, [], "another PR and master still reference the SHA");

    assert.equal(
      (await publish(memory.durable, headA, pullRequestPublication(headC, headD, 8))).status,
      202,
    );
    await memory.durable.alarm();
    assert.equal((await publish(memory.durable, headA, publication(headD))).status, 202);
    await memory.durable.alarm();
    memory.seedSandbox(headA, "linux-12345678-1234-4123-8123-123456789abc");

    now += 3 * 60 * 1_000;
    await memory.durable.alarm();
    assert.deepEqual(memory.terminated, [], "the unexpired distribution lease is a hard fence");
    now = memory.alarm + 1;
    await memory.durable.alarm();
    assert.deepEqual(
      memory.terminated,
      [],
      "cancellation grace starts when the final lease reference expires",
    );
    now = memory.alarm + 1;
    await memory.durable.alarm();
    assert.deepEqual(memory.terminated, [`ci-${headA}`]);
    assert.deepEqual(memory.destroyedSandboxes, [
      "linux-12345678-1234-4123-8123-123456789abc",
    ]);
    assert.deepEqual(memory.terminationMarker(headA), {
      status: "complete",
      head: headA,
      workflowId: `ci-${headA}`,
    });
    assert.equal(
      (await publish(memory.durable, headD, publication(headA))).status,
      409,
      "the immutable cancellation fence rejects a late ref-add race",
    );

    let currentMaster = headD;
    for (let index = 0; index < 98; index += 1) {
      const next = (0x100 + index).toString(16).padStart(40, "0");
      assert.equal(
        (await publish(
          memory.durable,
          currentMaster,
          publication(next, new Date(now + index * 1_000).toISOString()),
        )).status,
        202,
      );
      currentMaster = next;
    }
    assert.equal(
      (await memory.durable.fetch(new Request(`https://ci.test/runs/${headA}`))).status,
      404,
      "the completed cancelled run is eventually retired",
    );
    assert.deepEqual(memory.terminationMarker(headA), {
      status: "complete",
      head: headA,
      workflowId: `ci-${headA}`,
    });
    assert.equal(
      (await publish(memory.durable, currentMaster, publication(headA))).status,
      409,
      "the permanent cancellation tombstone forbids stale Workflow/result reuse",
    );
  } finally {
    Date.now = originalNow;
  }
});

test("operator cancellation is immediate, retryable, and permanently tombstones the SHA", async () => {
  const memory = repository();
  assert.equal((await publish(memory.durable, null, publication(headA))).status, 202);
  await memory.durable.alarm();
  memory.seedSandbox(headA, "linux-12345678-1234-4123-8123-123456789abc");

  const accepted = await requestOperatorCancellation(memory.durable, headA);
  assert.equal(accepted.status, 202);
  assert.deepEqual(await accepted.json(), {
    status: "accepted",
    head: headA,
    workflowId: `ci-${headA}`,
    reason: "operator_terminated",
  });
  await memory.durable.alarm();
  assert.deepEqual(memory.terminated, [`ci-${headA}`]);
  assert.deepEqual(memory.destroyedSandboxes, [
    "linux-12345678-1234-4123-8123-123456789abc",
  ]);
  assert.deepEqual(memory.terminationMarker(headA), {
    status: "complete",
    head: headA,
    workflowId: `ci-${headA}`,
  });

  const replay = await requestOperatorCancellation(memory.durable, headA);
  assert.equal(replay.status, 200);
  const completed = await replay.json() as Record<string, unknown>;
  assert.equal(completed.status, "complete");
  assert.equal(completed.reason, "operator_terminated");
  assert.match(String(completed.claimId), /^[a-f0-9-]{36}$/);
  assert.ok(Number.isFinite(Date.parse(String(completed.completedAt))));
  assert.equal(
    (await acquireDistributionLease(memory.durable, headA, `nightly-${headA}`)).status,
    409,
  );
  assert.equal(
    (await publish(memory.durable, headA, publication(headA))).status,
    409,
    "a completed operator cancellation is a permanent same-SHA tombstone",
  );

  let currentMaster = headA;
  for (let index = 0; index < 100; index += 1) {
    const next = (0x1_000 + index).toString(16).padStart(40, "0");
    assert.equal(
      (await publish(
        memory.durable,
        currentMaster,
        publication(next, new Date(Date.now() + index * 1_000).toISOString()),
      )).status,
      202,
    );
    currentMaster = next;
  }
  assert.equal(
    (await memory.durable.fetch(new Request(`https://ci.test/runs/${headA}`))).status,
    404,
    "retention removes the cancelled run while retaining its tombstone",
  );
  const retiredReplay = await requestOperatorCancellation(memory.durable, headA);
  assert.equal(retiredReplay.status, 200);
  assert.deepEqual(await retiredReplay.json(), completed);

  const pending = repository();
  assert.equal((await publish(pending.durable, null, publication(headB))).status, 202);
  assert.equal((await requestOperatorCancellation(pending.durable, headB)).status, 202);
  await pending.durable.alarm();
  assert.deepEqual(pending.terminated, [], "a never-dispatched outbox needs no Workflow terminate");
  assert.equal((await requestOperatorCancellation(pending.durable, headB)).status, 200);
});

test("multipart create replays one durable request identity after acknowledgement loss", async () => {
  const requestId = "00000000-0000-4000-8000-000000000003";
  const input = {
    version: 1 as const,
    requestId,
    cargoLockBlob: "c".repeat(40),
    bundleSha256: "3".repeat(64),
    size: 4_000_000,
    partSize: 32 * 1024 * 1024,
    partCount: 1,
  };
  const memory = repository({ failMultipartReadyAfterWrite: true });
  await assert.rejects(
    createCargoVendorMultipart(memory.durable, input),
    /injected multipart state acknowledgement loss/,
  );

  const replay = await createCargoVendorMultipart(memory.durable, input);
  assert.equal(replay.status, 200);
  assert.deepEqual(await replay.json(), {
    requestId,
    key: `cargo-vendor/${input.cargoLockBlob}/${input.bundleSha256}/bundle.tar.gz`,
    cargoLockBlob: input.cargoLockBlob,
    size: input.size,
    sha256: input.bundleSha256,
    uploadId: "upload-1",
    stagingId: requestId,
    partSize: input.partSize,
    partCount: input.partCount,
  });
  assert.equal(memory.multipartCreates, 1);

  const conflict = await createCargoVendorMultipart(memory.durable, {
    ...input,
    bundleSha256: "8".repeat(64),
  });
  assert.equal(conflict.status, 409);
  assert.deepEqual(await conflict.json(), {
    error: "cargo_vendor_multipart_request_conflict",
  });
  assert.equal(memory.multipartCreates, 1);
});

test("multipart finalize and reset require the exact ready upload identity", async () => {
  const requestId = "00000000-0000-4000-8000-000000000005";
  const input = {
    version: 1 as const,
    requestId,
    cargoLockBlob: "c".repeat(40),
    bundleSha256: "3".repeat(64),
    size: 4_000_000,
    partSize: 32 * 1024 * 1024,
    partCount: 1,
  };
  const memory = repository();
  const created = await createCargoVendorMultipart(memory.durable, input);
  const upload = await created.json() as { uploadId: string; stagingId: string };
  const identity = {
    version: 1 as const,
    requestId,
    stagingId: upload.stagingId,
    uploadId: upload.uploadId,
    cargoLockBlob: input.cargoLockBlob,
    bundleSha256: input.bundleSha256,
  };
  for (const operation of ["finalize", "reset"] as const) {
    const spoofed = await transitionCargoVendorMultipart(memory.durable, operation, {
      ...identity,
      uploadId: "spoofed-upload",
    });
    assert.equal(spoofed.status, 409);
    assert.equal(
      (memory.stateValue(`cargo-vendor-multipart:${requestId}`) as { state: string }).state,
      "ready",
    );
  }

  assert.equal(
    (await transitionCargoVendorMultipart(memory.durable, "finalize", identity)).status,
    204,
  );
  assert.equal(
    (await transitionCargoVendorMultipart(memory.durable, "finalize", identity)).status,
    204,
  );
  assert.equal(
    (memory.stateValue(`cargo-vendor-multipart:${requestId}`) as {
      state: string;
      uploadId: string;
    }).state,
    "complete",
  );
  assert.equal(
    (await transitionCargoVendorMultipart(memory.durable, "reset", identity)).status,
    409,
  );

  const resetRequestId = "00000000-0000-4000-8000-000000000006";
  const resetInput = { ...input, requestId: resetRequestId };
  const resetCreated = await createCargoVendorMultipart(memory.durable, resetInput);
  const resetUpload = await resetCreated.json() as { uploadId: string; stagingId: string };
  const resetIdentity = {
    ...identity,
    requestId: resetRequestId,
    stagingId: resetUpload.stagingId,
    uploadId: resetUpload.uploadId,
  };
  assert.equal(
    (await transitionCargoVendorMultipart(memory.durable, "reset", resetIdentity)).status,
    204,
  );
  assert.equal(
    (await transitionCargoVendorMultipart(memory.durable, "reset", resetIdentity)).status,
    204,
  );
  const recreated = await createCargoVendorMultipart(memory.durable, resetInput);
  assert.notEqual(
    (await recreated.json() as { uploadId: string }).uploadId,
    resetUpload.uploadId,
  );
});

test("an uncertain multipart create waits for R2 incomplete-upload expiry", async () => {
  const originalNow = Date.now;
  let now = Date.UTC(2026, 7, 22, 15, 0, 0);
  Date.now = () => now;
  try {
    const requestId = "00000000-0000-4000-8000-000000000004";
    const input = {
      version: 1 as const,
      requestId,
      cargoLockBlob: "c".repeat(40),
      bundleSha256: "3".repeat(64),
      size: 4_000_000,
      partSize: 32 * 1024 * 1024,
      partCount: 1,
    };
    const memory = repository({ failMultipartCreate: true });
    const uncertain = await createCargoVendorMultipart(memory.durable, input);
    assert.equal(uncertain.status, 503);
    const body = await uncertain.json() as { requestId: string; retryAt: string };
    assert.equal(body.requestId, requestId);
    assert.equal(Date.parse(body.retryAt), now + 7 * 24 * 60 * 60 * 1_000);

    const earlyReplay = await createCargoVendorMultipart(memory.durable, input);
    assert.equal(earlyReplay.status, 503);
    assert.equal(memory.multipartCreates, 1);

    now = Date.parse(body.retryAt) + 1;
    const recovered = await createCargoVendorMultipart(memory.durable, input);
    assert.equal(recovered.status, 200);
    const upload = await recovered.json() as { stagingId: string; uploadId: string };
    assert.equal(upload.stagingId, requestId);
    assert.equal(upload.uploadId, "upload-2");
    assert.equal(memory.multipartCreates, 2);
  } finally {
    Date.now = originalNow;
  }
});

test("source maintenance collects only old unreferenced owned objects", async () => {
  const memory = repository();
  const current = publication("a".repeat(40));
  const old = new Date(Date.now() - 2 * 24 * 60 * 60 * 1_000);
  const young = new Date();
  for (const key of [
    current.archive.key,
    current.tree.key,
    current.cargoVendor.key,
    current.rustSec.key,
  ]) memory.seedSource(key, old);
  const orphan = `cargo-vendor/${current.cargoLockBlob}/${"9".repeat(64)}/bundle.tar.gz`;
  const youngOrphan = `rustsec-advisory-db/${"f".repeat(40)}/bundle.tar.gz`;
  memory.seedSource(orphan, old);
  memory.seedSource(youngOrphan, young);
  memory.seedSource("operator-owned/keep", old);
  assert.equal((await publish(memory.durable, null, current)).status, 202);

  const response = await memory.durable.fetch(new Request(
    "https://ci.test/maintenance/source-gc",
    { method: "POST" },
  ));
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), {
    scanned: 7,
    deleted: 1,
    continuation: false,
  });
  assert.equal(memory.hasSource(orphan), false);
  assert.equal(memory.hasSource(youngOrphan), true);
  assert.equal(memory.hasSource("operator-owned/keep"), true);
  for (const key of [
    current.archive.key,
    current.tree.key,
    current.cargoVendor.key,
    current.rustSec.key,
  ]) assert.equal(memory.hasSource(key), true);
});

test("publication pins and source-GC retirement claims close both race orderings", async () => {
  const old = new Date(Date.now() - 2 * 24 * 60 * 60 * 1_000);
  const next = publication(headA);
  const pinned = repository();
  for (const key of [
    next.archive.key,
    next.tree.key,
    next.cargoVendor.key,
    next.rustSec.key,
  ]) pinned.seedSource(key, old);
  const orphan = `cargo-vendor/${next.cargoLockBlob}/${"9".repeat(64)}/bundle.tar.gz`;
  pinned.seedSource(orphan, old);

  const acquired = await acquirePublicationLease(pinned.durable, next);
  assert.equal(acquired.status, 201);
  const lease = (await acquired.json() as { lease: { leaseId: string } }).lease;
  const collected = await pinned.durable.fetch(new Request(
    "https://ci.test/maintenance/source-gc",
    { method: "POST" },
  ));
  assert.deepEqual(await collected.json(), {
    scanned: 5,
    deleted: 1,
    continuation: false,
  });
  for (const key of [
    next.archive.key,
    next.tree.key,
    next.cargoVendor.key,
    next.rustSec.key,
  ]) assert.equal(pinned.hasSource(key), true);
  const committed = await publishWithLease(pinned.durable, null, next, lease.leaseId);
  assert.equal(committed.status, 202, "the exact pin is atomically converted to live source state");

  const deleteEntered = deferred();
  const releaseDelete = deferred();
  const claimed = repository({
    beforeSourceDelete: async () => {
      deleteEntered.resolve();
      await releaseDelete.promise;
    },
  });
  for (const key of [
    next.archive.key,
    next.tree.key,
    next.cargoVendor.key,
    next.rustSec.key,
  ]) claimed.seedSource(key, old);
  const collecting = claimed.durable.fetch(new Request(
    "https://ci.test/maintenance/source-gc",
    { method: "POST" },
  ));
  await deleteEntered.promise;
  const rejected = await acquirePublicationLease(claimed.durable, next);
  assert.equal(rejected.status, 409);
  assert.deepEqual(await rejected.json(), { error: "source_retiring" });
  const alternateSha = "8".repeat(64);
  const alternate = {
    ...publication(headB),
    cargoVendor: {
      ...publication(headB).cargoVendor,
      key: `cargo-vendor/${next.cargoLockBlob}/${alternateSha}/bundle.tar.gz`,
      sha256: alternateSha,
    },
    rustSecRevision: "b".repeat(40),
    rustSec: {
      ...publication(headB).rustSec,
      key: `rustsec-advisory-db/${"b".repeat(40)}/bundle.tar.gz`,
    },
  } as CiSourcePublication;
  assert.equal(
    (await acquirePublicationLease(claimed.durable, alternate)).status,
    201,
    "a retiring hash must not fence another hash for the same Cargo.lock",
  );
  releaseDelete.resolve();
  assert.equal((await collecting).status, 200);
});

test("a slow source delete keeps its publication fence past the former claim TTL", async () => {
  const originalNow = Date.now;
  let now = Date.UTC(2026, 7, 22, 14, 0, 0);
  Date.now = () => now;
  try {
    const next = publication(headA);
    const old = new Date(now - 2 * 24 * 60 * 60 * 1_000);
    const deleteEntered = deferred();
    const releaseDelete = deferred();
    const memory = repository({
      beforeSourceDelete: async () => {
        deleteEntered.resolve();
        await releaseDelete.promise;
      },
    });
    const keys = [
      next.archive.key,
      next.tree.key,
      next.cargoVendor.key,
      next.rustSec.key,
    ];
    for (const key of keys) memory.seedSource(key, old);

    const collecting = memory.durable.fetch(new Request(
      "https://ci.test/maintenance/source-gc",
      { method: "POST" },
    ));
    await deleteEntered.promise;
    now += 6 * 60 * 1_000;

    const reconciliation = await memory.durable.fetch(new Request(
      "https://ci.test/maintenance/source-gc",
      { method: "POST" },
    ));
    assert.deepEqual(await reconciliation.json(), {
      scanned: 4,
      deleted: 0,
      continuation: false,
    }, "a concurrent maintenance pass renews rather than steals active delete ownership");
    const blocked = await publish(memory.durable, null, next);
    assert.equal(blocked.status, 409);
    assert.deepEqual(await blocked.json(), { error: "source_retiring" });
    for (const key of keys) assert.equal(memory.hasSource(key), true);

    releaseDelete.resolve();
    assert.equal((await collecting).status, 200);
    for (const key of keys) assert.equal(memory.hasSource(key), false);

    for (const key of keys) memory.seedSource(key, new Date(now));
    assert.equal(
      (await publish(memory.durable, null, next)).status,
      202,
      "publication can commit only after the delete owner clears its reconciled fence",
    );
  } finally {
    Date.now = originalNow;
  }
});

function repository(options: {
  failAfterCreate?: boolean;
  failHead?: string;
  failMultipartReadyAfterWrite?: boolean;
  failMultipartCreate?: boolean;
  beforeSourceDelete?: (keys: string[]) => Promise<void>;
} = {}) {
  const values = new Map<string, unknown>();
  const workflows = new Map<string, { status: string }>();
  let alarm = -1;
  let alarmFailureCountdown: number | undefined;
  let created = 0;
  let createdParams: NanocodexCiParams | undefined;
  const paramsByHead = new Map<string, NanocodexCiParams>();
  const sourceDeletes: string[] = [];
  const sourceObjects = new Map<string, Date>();
  const backupObjects = new Map<string, { value: unknown; uploaded: Date }>();
  const terminated: string[] = [];
  const destroyedSandboxes: string[] = [];
  let failAfterCreate = options.failAfterCreate === true;
  let failMultipartReadyAfterWrite = options.failMultipartReadyAfterWrite === true;
  let failMultipartCreate = options.failMultipartCreate === true;
  let multipartCreates = 0;
  const operations = {
    get: async <T>(key: string) => structuredClone(values.get(key)) as T | undefined,
    put: async (keyOrEntries: string | Record<string, unknown>, value?: unknown) => {
      if (typeof keyOrEntries === "string") {
        const stored = structuredClone(value);
        values.set(keyOrEntries, stored);
        if (
          failMultipartReadyAfterWrite &&
          keyOrEntries.startsWith("cargo-vendor-multipart:") &&
          typeof stored === "object" && stored != null &&
          (stored as { state?: unknown }).state === "ready"
        ) {
          failMultipartReadyAfterWrite = false;
          throw new Error("injected multipart state acknowledgement loss");
        }
      }
      else for (const [key, entry] of Object.entries(keyOrEntries)) {
        values.set(key, structuredClone(entry));
      }
    },
    delete: async (key: string | string[]) => {
      if (Array.isArray(key)) return key.reduce((deleted, entry) => values.delete(entry) || deleted, false);
      return values.delete(key);
    },
    list: async <T>({ prefix = "", limit }: { prefix?: string; limit?: number } = {}) => {
      const entries = [...values.entries()].filter(([key]) => key.startsWith(prefix));
      return new Map((limit === undefined ? entries : entries.slice(0, limit))
        .map(([key, value]) => [key, structuredClone(value) as T]));
    },
    getAlarm: async () => alarm > Date.now() ? alarm : null,
    setAlarm: async (timestamp: number) => {
      if (alarmFailureCountdown === 0) {
        alarmFailureCountdown = undefined;
        throw new Error("injected alarm acknowledgement loss");
      }
      if (alarmFailureCountdown != null) alarmFailureCountdown -= 1;
      alarm = timestamp;
    },
  };
  const state = {
    storage: {
      ...operations,
      transaction: async <T>(callback: (transaction: typeof operations) => Promise<T>) => callback(operations),
    },
  } as unknown as DurableObjectState;
  const env = {
    CI_SOURCE: {
      async head() {
        return null;
      },
      async createMultipartUpload(key: string) {
        multipartCreates += 1;
        if (failMultipartCreate) {
          failMultipartCreate = false;
          throw new Error("injected uncertain multipart create");
        }
        return { key, uploadId: `upload-${multipartCreates}` };
      },
      async delete(keys: string | string[]) {
        const entries = Array.isArray(keys) ? keys : [keys];
        await options.beforeSourceDelete?.(entries);
        sourceDeletes.push(...entries);
        for (const key of entries) sourceObjects.delete(key);
      },
      async list({
        limit = 1_000,
        cursor,
      }: { limit?: number; cursor?: string } = {}) {
        const entries = [...sourceObjects.entries()].sort(([left], [right]) =>
          left.localeCompare(right)
        );
        const start = cursor == null ? 0 : Number(cursor);
        const page = entries.slice(start, start + limit);
        const next = start + page.length;
        return {
          objects: page.map(([key, uploaded]) => ({ key, uploaded })),
          delimitedPrefixes: [],
          truncated: next < entries.length,
          ...(next < entries.length ? { cursor: String(next) } : {}),
        };
      },
    },
    CI_WORKFLOW: {
      async get(id: string) {
        return {
          id,
          status: async () => workflows.get(id) ?? { status: "unknown" },
          terminate: async () => {
            terminated.push(id);
            workflows.set(id, { status: "terminated" });
          },
        };
      },
      async createBatch([{ id, params }]: Array<{ id: string; params: NanocodexCiParams }>) {
        if (id === `ci-${options.failHead}`) throw new Error("permanent dispatch failure");
        if (workflows.has(id)) return [];
        created += 1;
        createdParams = structuredClone(params);
        paramsByHead.set(params.sha, structuredClone(params));
        workflows.set(id, { status: "queued" });
        if (failAfterCreate) {
          failAfterCreate = false;
          throw new Error("lost acknowledgement");
        }
        return [{ id }];
      },
    },
    BACKUP_BUCKET: {
      async put(key: string, value: string) {
        backupObjects.set(key, {
          value: JSON.parse(value),
          uploaded: new Date(Date.now()),
        });
        return {};
      },
      async get(key: string) {
        const object = backupObjects.get(key);
        return object
          ? { json: async () => structuredClone(object.value) }
          : null;
      },
      async head(key: string) {
        const object = backupObjects.get(key);
        return object ? { key, uploaded: object.uploaded } : null;
      },
      async delete(keys: string | string[]) {
        for (const key of Array.isArray(keys) ? keys : [keys]) backupObjects.delete(key);
      },
      async list({
        prefix = "",
        cursor,
        limit = 1_000,
      }: { prefix?: string; cursor?: string; limit?: number } = {}) {
        const entries = [...backupObjects.entries()]
          .filter(([key]) => key.startsWith(prefix))
          .sort(([left], [right]) => left.localeCompare(right));
        const start = cursor == null ? 0 : Number(cursor);
        const page = entries.slice(start, start + limit);
        const next = start + page.length;
        return {
          objects: page.map(([key, object]) => ({ key, uploaded: object.uploaded })),
          delimitedPrefixes: [],
          truncated: next < entries.length,
          ...(next < entries.length ? { cursor: String(next) } : {}),
        };
      },
    },
    SANDBOX: {
      idFromName: (id: string) => ({ id }),
      get: (id: { id: string }) => ({
        destroy: async () => {
          destroyedSandboxes.push(id.id);
          for (const key of backupObjects.keys()) {
            if (key.endsWith(`/sandboxes/${id.id}.json`)) backupObjects.delete(key);
          }
        },
      }),
    },
  };
  const durable = new CiRepository(state, env as never);
  return {
    durable,
    complete(head: string) { workflows.set(`ci-${head}`, { status: "complete" }); },
    completeResult(
      head: string,
      overrides: Record<string, unknown> = {},
    ) {
      backupObjects.set(`runs/${head}/result.json`, {
        value: {
          version: 1,
          head,
          workflowId: `ci-${head}`,
          status: "success",
          completedAt: new Date(Date.now()).toISOString(),
          ...overrides,
        },
        uploaded: new Date(Date.now()),
      });
    },
    expireWorkflow(head: string) { workflows.delete(`ci-${head}`); },
    seedSandbox(head: string, runnerId: string) {
      backupObjects.set(`runs/${head}/sandboxes/${runnerId}.json`, {
        value: { runnerId },
        uploaded: new Date(Date.now()),
      });
    },
    terminationMarker(head: string) {
      const value = backupObjects.get(`runs/${head}/control/terminated.json`)?.value as {
        status?: unknown;
        head?: unknown;
        workflowId?: unknown;
      } | undefined;
      return value
        ? { status: value.status, head: value.head, workflowId: value.workflowId }
        : undefined;
    },
    get terminated() { return terminated; },
    get destroyedSandboxes() { return destroyedSandboxes; },
    get sourceDeletes() { return sourceDeletes; },
    get alarm() { return alarm; },
    simulateLostAlarm() { alarm = -1; },
    failAlarmAfterWrites(writes: number) { alarmFailureCountdown = writes; },
    get created() { return created; },
    get multipartCreates() { return multipartCreates; },
    get createdParams() { return createdParams; },
    paramsFor(head: string) { return paramsByHead.get(head); },
    stateValue(key: string) { return structuredClone(values.get(key)); },
    setState(key: string, value: unknown) { values.set(key, structuredClone(value)); },
    seedSource(key: string, uploaded: Date) { sourceObjects.set(key, uploaded); },
    hasSource(key: string) { return sourceObjects.has(key); },
  };
}

async function publish(
  durable: CiRepository,
  expectedHead: string | null,
  next: CiSourcePublication,
) : Promise<Response> {
  const pinned = await acquirePublicationLease(durable, next);
  if (!pinned.ok) return pinned;
  const lease = (await pinned.json() as { lease: { leaseId: string } }).lease;
  return publishWithLease(durable, expectedHead, next, lease.leaseId);
}

function publishWithLease(
  durable: CiRepository,
  expectedHead: string | null,
  next: CiSourcePublication,
  leaseId: string,
): Promise<Response> {
  return durable.fetch(new Request("https://ci.test/publications", {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ expectedHead, publication: next, leaseId }),
  }));
}

function acquirePublicationLease(
  durable: CiRepository,
  publication: CiSourcePublication,
) {
  return durable.fetch(new Request(
    `https://ci.test/leases/publication/${publication.head}`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ publication }),
    },
  ));
}

function acquireDistributionLease(
  durable: CiRepository,
  head: string,
  workflowId: string,
) {
  return durable.fetch(new Request(`https://ci.test/leases/distribution/${head}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ workflowId }),
  }));
}

function releaseReservationRequest(
  commit: string,
  overrides: Partial<{
    owner: string;
    releaseKind: "stable" | "commit";
    releaseId: string;
    publicationLeaseId: string;
    publicationLeaseGeneration: number;
  }> = {},
) {
  return {
    version: 1 as const,
    owner: "release-controller:test",
    releaseKind: "stable" as const,
    releaseId: "v1.2.3",
    commit,
    publicationLeaseId: `1.${closeIdA}`,
    publicationLeaseGeneration: 1,
    ...overrides,
  };
}

function acquireReleaseCommitReservation(
  durable: CiRepository,
  input: ReturnType<typeof releaseReservationRequest>,
): Promise<Response> {
  return durable.fetch(new Request(
    "https://ci.test/reservations/release-commit/acquire",
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(input),
    },
  ));
}

function heartbeatReleaseCommitReservation(
  durable: CiRepository,
  reservation: Pick<CiReleaseCommitReservation, "reservationId" | "owner" | "generation">,
): Promise<Response> {
  return releaseCommitReservationMutation(durable, "POST", reservation, "/heartbeat");
}

function releaseReleaseCommitReservation(
  durable: CiRepository,
  reservation: Pick<CiReleaseCommitReservation, "reservationId" | "owner" | "generation">,
): Promise<Response> {
  return releaseCommitReservationMutation(durable, "DELETE", reservation);
}

function releaseCommitReservationMutation(
  durable: CiRepository,
  method: "POST" | "DELETE",
  reservation: Pick<CiReleaseCommitReservation, "reservationId" | "owner" | "generation">,
  suffix = "",
): Promise<Response> {
  return durable.fetch(new Request(
    `https://ci.test/reservations/release-commit/${reservation.reservationId}${suffix}`,
    {
      method,
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        owner: reservation.owner,
        generation: reservation.generation,
      }),
    },
  ));
}

function assertReleaseCommitReservation(
  reservation: CiReleaseCommitReservation,
  commit: string,
  generation: number,
): void {
  assert.equal(reservation.version, 1);
  assert.equal(reservation.kind, "release-commit");
  assert.equal(reservation.commit, commit);
  assert.equal(reservation.generation, generation);
  assert.equal(reservation.reservationId.split(".", 1)[0], String(generation));
  assert.equal(reservation.publicationLeaseId.split(".", 1)[0],
    String(reservation.publicationLeaseGeneration));
  assert.ok(Date.parse(reservation.expiresAt) > Date.now());
  assert.ok(Date.parse(reservation.expiresAt) - Date.parse(reservation.renewedAt) <= 120_000);
}

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

function closePullRequest(
  durable: CiRepository,
  number: number,
  closeId: string,
  expectedMergeHead: string,
  expectedPullRequestHead: string,
) {
  return durable.fetch(new Request(`https://ci.test/pull-requests/${number}/state`, {
    method: "DELETE",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ closeId, expectedMergeHead, expectedPullRequestHead }),
  }));
}

function requestOperatorCancellation(durable: CiRepository, head: string) {
  return durable.fetch(new Request(`https://ci.test/cancellations/${head}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ workflowId: `ci-${head}` }),
  }));
}

function createCargoVendorMultipart(
  durable: CiRepository,
  input: {
    version: 1;
    requestId: string;
    cargoLockBlob: string;
    bundleSha256: string;
    size: number;
    partSize: number;
    partCount: number;
  },
) {
  return durable.fetch(new Request(
    `https://ci.test/cargo-vendor/multipart/${input.requestId}`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(input),
    },
  ));
}

function transitionCargoVendorMultipart(
  durable: CiRepository,
  operation: "finalize" | "reset",
  identity: {
    version: 1;
    requestId: string;
    stagingId: string;
    uploadId: string;
    cargoLockBlob: string;
    bundleSha256: string;
  },
) {
  return durable.fetch(new Request(
    `https://ci.test/cargo-vendor/multipart/${identity.requestId}/${operation}`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(identity),
    },
  ));
}

async function reopenPullRequest(
  durable: CiRepository,
  expectedHead: string | null,
  next: CiSourcePublication,
  reopen: { closeId: string; mergeHead: string; pullRequestHead: string },
) : Promise<Response> {
  const pinned = await acquirePublicationLease(durable, next);
  if (!pinned.ok) return pinned;
  const lease = (await pinned.json() as { lease: { leaseId: string } }).lease;
  return durable.fetch(new Request("https://ci.test/publications", {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ expectedHead, publication: next, leaseId: lease.leaseId, reopen }),
  }));
}

function publicationFields(head: string, publishedAt: string) {
  return {
    version: 1 as const,
    head,
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
    cargoLockBlob: "c".repeat(40),
    cargoVendor: {
      key: `cargo-vendor/${"c".repeat(40)}/${"3".repeat(64)}/bundle.tar.gz`,
      size: 4_000_000,
      sha256: "3".repeat(64),
    },
    rustSecRevision: "d".repeat(40),
    rustSec: {
      key: `rustsec-advisory-db/${"d".repeat(40)}/bundle.tar.gz`,
      size: 1_400_000,
      sha256: "4".repeat(64),
    },
    publishedAt,
  };
}

function publication(
  head: string,
  publishedAt = "2026-08-21T00:00:00.000Z",
): CiSourcePublication {
  return {
    ...publicationFields(head, publishedAt),
    branch: "master",
    ref: "refs/heads/master",
  };
}

function pullRequestPublication(
  mergeHead: string,
  pullRequestHead: string,
  number: number,
  publishedAt = "2026-08-21T00:00:00.000Z",
): CiSourcePublication {
  const branch = `pull/${number}/merge` as const;
  const ref = `refs/pull/${number}/merge` as const;
  return {
    ...publicationFields(mergeHead, publishedAt),
    branch,
    ref,
    lane: { type: "pull_request", number, pullRequestHead },
  };
}
