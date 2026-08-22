import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  type CiPublicationLease,
  CiReleases,
  promoteCiReleaseAsset,
  type CiReleaseAsset,
  type CiReleaseDraft,
  type CiReleaseManifest,
  type CiReleasePointer,
} from "./ciReleases.ts";
import type { CiReleaseCommitReservation } from "./ciRepository.ts";

const TOKEN = "release-token";
const COMMIT_V050 = "e4eea49fc6fab06a98ff01ec8c3da8d9a729eee1";
const COMMIT_A = "a".repeat(40);
const COMMIT_B = "b".repeat(40);
const COMMIT_C = "c".repeat(40);
const OWNER_A = "release-controller:a";
const OWNER_B = "release-controller:b";

type PublicationLeaseIdentity = Pick<
  CiPublicationLease,
  "owner" | "kind" | "id" | "commit"
>;

type PublicManifest = Omit<CiReleaseManifest, "assets"> & {
  assets: Array<Omit<CiReleaseAsset, "key"> & { downloadPath: string }>;
};

type PublicDraft = {
  version: 1;
  kind: "stable" | "commit";
  id: string;
  tag: string;
  commit: string;
  channel: "latest" | "nightly";
  expectedChannel: string | null;
  createdAt: string;
  assets: Array<Omit<CiReleaseAsset, "key"> & { downloadPath: string }>;
};

test("publication lease authority is exclusively the constant-time release bearer", async () => {
  const memory = releases();
  const identity = publicationLeaseIdentity();

  for (const token of [null, "wrong", `${TOKEN}-too-long`]) {
    const response = await acquirePublicationLease(memory.durable, identity, token);
    assert.equal(response.status, 401);
    assert.equal(response.headers.get("www-authenticate"), "Bearer");
    assert.deepEqual(await response.json(), { error: "unauthorized" });
  }
  const alternativeHeader = await memory.durable.fetch(new Request(
    "https://release.test/publication-lease/acquire",
    {
      method: "POST",
      headers: { "content-type": "application/json", "x-ci-release-token": TOKEN },
      body: JSON.stringify(identity),
    },
  ));
  assert.equal(alternativeHeader.status, 401);

  const unconfigured = releases({ token: null });
  assert.equal(
    (await acquirePublicationLease(unconfigured.durable, identity)).status,
    401,
  );

  const acquired = await acquirePublicationLease(memory.durable, identity);
  assert.equal(acquired.status, 201);
  const lease = await acquired.json() as CiPublicationLease;
  assertPublicationLease(lease, identity, 1);
  assert.ok(Date.parse(lease.expiresAt) - Date.now() <= 120_000);
  assert.ok(Date.parse(lease.expiresAt) - Date.now() > 118_000);
  assert.equal(JSON.stringify(lease).includes(TOKEN), false);

  assert.equal(
    (await heartbeatPublicationLease(memory.durable, lease, null)).status,
    401,
  );
  assert.equal(
    (await releasePublicationLease(memory.durable, lease, "wrong")).status,
    401,
  );
  assert.equal(
    (await heartbeatPublicationLease(memory.durable, lease)).status,
    200,
  );
});

test("publication lease identities and mutation bodies are strict", async () => {
  const memory = releases();
  const identity = publicationLeaseIdentity();
  const invalidBodies: unknown[] = [
    null,
    [],
    {},
    { ...identity, ttl: 1 },
    { ...identity, owner: "" },
    { ...identity, owner: "release controller" },
    { ...identity, owner: `${OWNER_A}\n` },
    { ...identity, owner: "x".repeat(193) },
    { ...identity, kind: "commit" },
    { ...identity, id: "1.2.3" },
    { ...identity, id: "v01.2.3" },
    { ...identity, id: "v1.2" },
    { ...identity, id: "v1.2.3\n" },
    { ...identity, commit: "A".repeat(40) },
    { ...identity, commit: "a".repeat(39) },
    { ...identity, commit: `${"a".repeat(40)}\n` },
  ];
  for (const body of invalidBodies) {
    const response = await publicationLeaseRequest(
      memory.durable,
      "POST",
      "/publication-lease/acquire",
      body,
    );
    assert.equal(response.status, 400, JSON.stringify(body));
    assert.deepEqual(await response.json(), { error: "invalid_publication_lease" });
  }
  const invalidJson = await rawPublicationLeaseRequest(
    memory.durable,
    "POST",
    "/publication-lease/acquire",
    "{",
  );
  assert.equal(invalidJson.status, 400);

  const lease = await acquireLease(memory.durable, identity);
  for (const [method, path, body] of [
    ["POST", `/publication-lease/${lease.leaseId}/heartbeat`, {}],
    ["POST", `/publication-lease/${lease.leaseId}/heartbeat`, { owner: lease.owner, extra: 1 }],
    ["POST", "/publication-lease/not-a-lease/heartbeat", { owner: lease.owner }],
    ["POST", `/publication-lease/${lease.leaseId}%0A/heartbeat`, { owner: lease.owner }],
    ["DELETE", `/publication-lease/${lease.leaseId}`, {}],
    ["DELETE", `/publication-lease/${lease.leaseId}`, { owner: lease.owner, extra: true }],
    ["DELETE", "/publication-lease/not-a-lease", { owner: lease.owner }],
  ] as const) {
    const response = await publicationLeaseRequest(memory.durable, method, path, body);
    assert.equal(response.status, 400, `${method} ${path}`);
    assert.deepEqual(await response.json(), { error: "invalid_publication_lease" });
  }
  assert.equal((await heartbeatPublicationLease(memory.durable, lease)).status, 200);
});

test("one global publication lease serializes different releases and owners", async () => {
  const memory = releases();
  const left = publicationLeaseIdentity();
  const right = publicationLeaseIdentity({
    owner: OWNER_B,
    id: "v1.3.0",
    commit: COMMIT_B,
  });
  const responses = await Promise.all([
    acquirePublicationLease(memory.durable, left),
    acquirePublicationLease(memory.peer, right),
  ]);
  assert.deepEqual(responses.map((response) => response.status).sort(), [201, 409]);
  const winnerIndex = responses.findIndex((response) => response.status === 201);
  const winner = winnerIndex === 0 ? left : right;
  const lease = await responses[winnerIndex]!.json() as CiPublicationLease;
  assertPublicationLease(lease, winner, 1);
  assert.deepEqual(
    await responses[1 - winnerIndex]!.json(),
    { error: "publication_lease_conflict" },
  );

  const changedOwner = { ...winner, owner: winner.owner === OWNER_A ? OWNER_B : OWNER_A };
  assert.equal(
    (await acquirePublicationLease(memory.peer, changedOwner)).status,
    409,
  );
  const changedRelease = {
    ...winner,
    id: winner.id === "v1.2.3" ? "v1.3.0" : "v1.2.3",
    commit: winner.commit === COMMIT_A ? COMMIT_B : COMMIT_A,
  };
  assert.equal(
    (await acquirePublicationLease(memory.peer, changedRelease)).status,
    409,
  );
});

test("lost acquire acknowledgement replay preserves identity and renews the lease", async () => {
  const memory = releases();
  const identity = publicationLeaseIdentity();
  const firstResponse = await acquirePublicationLease(memory.durable, identity);
  assert.equal(firstResponse.status, 201);
  const first = await firstResponse.json() as CiPublicationLease;
  memory.setLeaseExpiry(first.leaseId, Date.now() + 1_000);

  const replayedResponse = await acquirePublicationLease(memory.peer, identity);
  assert.equal(replayedResponse.status, 200);
  const replayed = await replayedResponse.json() as CiPublicationLease;
  assert.deepEqual(
    { ...replayed, expiresAt: first.expiresAt },
    first,
  );
  assert.ok(Date.parse(replayed.expiresAt) > Date.now() + 118_000);
  assert.deepEqual(memory.activeLease(), replayed);
});

test("expired publication leases are reclaimable with a new monotonic generation", async () => {
  const memory = releases();
  const firstIdentity = publicationLeaseIdentity();
  const first = await acquireLease(memory.durable, firstIdentity);
  memory.setLeaseExpiry(first.leaseId, Date.now() - 1);

  const nextIdentity = publicationLeaseIdentity({
    owner: OWNER_B,
    id: "v2.0.0",
    commit: COMMIT_B,
  });
  const reclaimedResponse = await acquirePublicationLease(memory.peer, nextIdentity);
  assert.equal(reclaimedResponse.status, 201);
  const reclaimed = await reclaimedResponse.json() as CiPublicationLease;
  assertPublicationLease(reclaimed, nextIdentity, 2);
  assert.notEqual(reclaimed.leaseId, first.leaseId);
  assert.equal(
    (await acquirePublicationLease(memory.durable, firstIdentity)).status,
    409,
  );
  assert.equal(
    (await heartbeatPublicationLease(memory.durable, first)).status,
    409,
  );
});

test("heartbeats renew only the live matching publication lease", async () => {
  const memory = releases();
  const lease = await acquireLease(memory.durable, publicationLeaseIdentity());
  const shortened = new Date(Date.now() + 1_000).toISOString();
  memory.setLeaseExpiry(lease.leaseId, Date.parse(shortened));

  const wrongOwner = await heartbeatPublicationLease(
    memory.peer,
    { ...lease, owner: OWNER_B },
  );
  assert.equal(wrongOwner.status, 409);
  assert.deepEqual(await wrongOwner.json(), { error: "publication_lease_not_held" });
  assert.equal(memory.activeLease().expiresAt, shortened);

  const staleId = await heartbeatPublicationLease(
    memory.peer,
    { ...lease, leaseId: `999.${crypto.randomUUID()}` },
  );
  assert.equal(staleId.status, 409);
  assert.equal(memory.activeLease().expiresAt, shortened);

  const heartbeat = await heartbeatPublicationLease(memory.peer, lease);
  assert.equal(heartbeat.status, 200);
  const renewed = await heartbeat.json() as CiPublicationLease;
  assert.equal(renewed.leaseId, lease.leaseId);
  assert.equal(renewed.generation, lease.generation);
  assert.ok(Date.parse(renewed.expiresAt) > Date.now() + 118_000);

  memory.setLeaseExpiry(lease.leaseId, Date.now() - 1);
  assert.equal(
    (await heartbeatPublicationLease(memory.durable, lease)).status,
    409,
  );
});

test("release is acknowledgement-safe and stale lease IDs cannot release a successor", async () => {
  const memory = releases();
  const first = await acquireLease(memory.durable, publicationLeaseIdentity());
  memory.setLeaseExpiry(first.leaseId, Date.now() - 1);
  const nextIdentity = publicationLeaseIdentity({
    owner: OWNER_B,
    id: "v1.3.0",
    commit: COMMIT_B,
  });
  const next = await acquireLease(memory.peer, nextIdentity, 2);

  assert.equal((await releasePublicationLease(memory.durable, first)).status, 204);
  assert.equal((await heartbeatPublicationLease(memory.peer, next)).status, 200);
  memory.setLeaseExpiry(next.leaseId, Date.now() - 1);
  const expiredSuccessor = memory.activeLease();
  assert.equal((await releasePublicationLease(memory.durable, first)).status, 204);
  assert.deepEqual(memory.activeLease(), expiredSuccessor);
  memory.setLeaseExpiry(next.leaseId, Date.now() + 120_000);

  const wrongOwner = await releasePublicationLease(
    memory.durable,
    { ...next, owner: OWNER_A },
  );
  assert.equal(wrongOwner.status, 409);
  assert.deepEqual(await wrongOwner.json(), { error: "publication_lease_not_held" });
  assert.equal((await heartbeatPublicationLease(memory.peer, next)).status, 200);

  assert.equal((await releasePublicationLease(memory.peer, next)).status, 204);
  assert.equal((await releasePublicationLease(memory.peer, next)).status, 204);
  const third = await acquireLease(
    memory.durable,
    publicationLeaseIdentity({ id: "v1.4.0", commit: COMMIT_C }),
    3,
  );
  assert.equal(third.generation, 3);
});

test("stable finalization is transactionally fenced by the live publication lease", async () => {
  const memory = releases();
  const input = stableDraft("v0.5.0", COMMIT_V050, null);
  assert.equal((await putDraft(memory.durable, input)).status, 201);
  memory.bucket.seedAll(input.assets);

  const unauthorized = await finalizeRequest(memory.durable, input, { token: null });
  assert.equal(unauthorized.status, 401);
  assert.deepEqual(await unauthorized.json(), { error: "unauthorized" });

  const missing = await finalizeRequest(memory.durable, input);
  assert.equal(missing.status, 400);
  assert.deepEqual(await missing.json(), { error: "invalid_publication_lease" });

  const syntacticLease = {
    leaseId: `1.${crypto.randomUUID()}`,
    owner: OWNER_A,
    generation: 1,
  };
  const malformedHeaders: Array<Record<string, string>> = [
    { "x-nanocodex-publication-lease-id": "not-a-lease" },
    { "x-nanocodex-publication-lease-owner": "invalid owner" },
    { "x-nanocodex-publication-lease-generation": "01" },
    { "x-nanocodex-publication-lease-generation": "2" },
  ];
  for (const headers of malformedHeaders) {
    const malformed = await finalizeRequest(memory.durable, input, {
      fence: syntacticLease,
      headers,
    });
    assert.equal(malformed.status, 400, JSON.stringify(headers));
    assert.deepEqual(await malformed.json(), { error: "invalid_publication_lease" });
  }

  const identity = publicationLeaseIdentity({ id: input.tag, commit: input.commit });
  const first = await acquireLease(memory.durable, identity);
  for (const fence of [
    { ...first, owner: OWNER_B },
    { ...first, leaseId: `2.${crypto.randomUUID()}`, generation: 2 },
  ]) {
    const rejected = await finalizeRequest(memory.peer, input, { fence });
    assert.equal(rejected.status, 409);
    assert.deepEqual(await rejected.json(), { error: "publication_lease_not_held" });
  }
  assert.equal(
    (await draftFetch(memory.durable, "/api/releases/drafts/stable/v0.5.0")).status,
    200,
  );

  memory.setLeaseExpiry(first.leaseId, Date.now() - 1);
  const expired = await finalizeRequest(memory.durable, input, { fence: first });
  assert.equal(expired.status, 409);
  assert.deepEqual(await expired.json(), { error: "publication_lease_not_held" });

  const successor = await acquireLease(memory.peer, identity, 2);
  const stale = await finalizeRequest(memory.durable, input, { fence: first });
  assert.equal(stale.status, 409);
  assert.deepEqual(await stale.json(), { error: "publication_lease_not_held" });

  const published = await finalizeRequest(memory.peer, input, { fence: successor });
  assert.equal(published.status, 201);
  assert.equal((await releasePublicationLease(memory.peer, successor)).status, 204);

  const acknowledged = await finalizeRequest(memory.durable, input, { fence: first });
  assert.equal(acknowledged.status, 200);
  assert.equal(
    (await draftFetch(memory.durable, "/api/releases/drafts/stable/v0.5.0")).status,
    404,
  );
  assert.equal((await channelPointer(memory.durable, "latest")).id, input.tag);
});

test("the empty stable ledger is anchored to the v0.5.0 cutover release", async () => {
  const memory = releases();
  const unanchored = stableDraft("v0.6.0", COMMIT_A, null);
  assert.equal((await putDraft(memory.durable, unanchored)).status, 400);

  memory.seedStoredDraft(unanchored);
  memory.bucket.seedAll(unanchored.assets);
  const lease = await acquireLease(
    memory.durable,
    publicationLeaseIdentity({ id: unanchored.tag, commit: unanchored.commit }),
  );
  const rejected = await finalizeRequest(memory.durable, unanchored, { fence: lease });
  assert.equal(rejected.status, 409);
  assert.deepEqual(await rejected.json(), { error: "stable_bootstrap_required" });
  assert.equal((await releasePublicationLease(memory.durable, lease)).status, 204);
  assert.equal(
    (await draftFetch(memory.durable, "/api/releases/drafts/stable/v0.6.0")).status,
    200,
  );

  assert.equal(
    (await putDraft(memory.durable, stableDraft("v0.5.0", COMMIT_A, null))).status,
    400,
  );
  await publish(memory, stableDraft("v0.5.0", COMMIT_V050, null));
  assert.equal(memory.reservationAcquireCalls, 0, "the historical import is the sole exception");
  await publish(memory, stableDraft("v0.6.1", COMMIT_B, "v0.5.0"));
  assert.equal(memory.reservationAcquireCalls, 1, "normal stable publication is reserved");
  assert.equal(memory.reservationReleaseCalls, 1);
  assert.equal((await channelPointer(memory.durable, "latest")).id, "v0.6.1");
});

test("release drafts are authenticated, normalized, idempotent, and conflict-safe", async () => {
  const memory = releases();
  const input = stableDraft("v0.5.0", COMMIT_V050, null);

  const unauthorized = await putDraft(memory.durable, input, null);
  assert.equal(unauthorized.status, 401);
  assert.equal(unauthorized.headers.get("www-authenticate"), "Bearer");

  const invalid = structuredClone(input);
  invalid.assets[0]!.platform = "darwin" as never;
  assert.equal((await putDraft(memory.durable, invalid)).status, 400);
  assert.equal(
    (await putDraft(memory.durable, { ...input, tag: "v0.5" })).status,
    400,
  );

  const created = await putDraft(memory.durable, input);
  assert.equal(created.status, 201);
  assert.equal(created.headers.get("cache-control"), "no-store");
  const createdBody = await created.json() as { draft: PublicDraft };
  assert.deepEqual(Object.keys(createdBody), ["draft"]);
  assert.deepEqual(Object.keys(createdBody.draft).sort(), [
    "assets",
    "channel",
    "commit",
    "createdAt",
    "expectedChannel",
    "id",
    "kind",
    "tag",
    "version",
  ]);
  assert.equal(JSON.stringify(createdBody).includes('"key"'), false);
  assert.equal(JSON.stringify(createdBody).includes('"expiresAt"'), false);
  const reversed = { ...input, assets: [...input.assets].reverse() };
  const replayed = await putDraft(memory.durable, reversed);
  assert.equal(replayed.status, 200);
  assert.deepEqual(await replayed.json(), createdBody);

  const changed = structuredClone(input);
  changed.assets[0]!.sha256 = "f".repeat(64);
  const conflict = await putDraft(memory.durable, changed);
  assert.equal(conflict.status, 409);
  assert.deepEqual(await conflict.json(), { error: "immutable_draft_conflict" });
});

test("normal and bootstrap drafts require their exact canonical asset inventories", async () => {
  for (const input of [
    stableDraft("v0.6.0", COMMIT_A, "v0.5.0"),
    commitDraft(COMMIT_A, null),
  ]) {
    const missing = structuredClone(input);
    missing.assets.pop();
    assert.equal((await putDraft(releases().durable, missing)).status, 400);

    const extra = structuredClone(input);
    extra.assets.push({ ...extra.assets[0]!, name: "unexpected-release-asset" });
    assert.equal((await putDraft(releases().durable, extra)).status, 400);

    for (const mutation of [
      (asset: CiReleaseAsset) => asset.name = `${asset.name}.mislabeled`,
      (asset: CiReleaseAsset) => asset.platform = "linux",
      (asset: CiReleaseAsset) => asset.contentType = "application/json",
    ]) {
      const mislabeled = structuredClone(input);
      mutation(mislabeled.assets[0]!);
      assert.equal((await putDraft(releases().durable, mislabeled)).status, 400);
    }
    assert.equal((await putDraft(releases().durable, input)).status, 201);
  }

  const bootstrap = stableDraft("v0.5.0", COMMIT_V050, null);
  assert.deepEqual(
    [...bootstrap.assets].sort((left, right) => left.name.localeCompare(right.name)).map((asset) =>
      [asset.name, asset.platform, asset.contentType]
    ),
    [
      ["nanocodex-aarch64-apple-darwin", "aarch64-apple-darwin", "application/octet-stream"],
      ["nanocodex-x86_64-unknown-linux-gnu", "x86_64-unknown-linux-gnu", "application/octet-stream"],
      ["nanocodex-x86_64-unknown-linux-gnu.gz", "x86_64-unknown-linux-gnu", "application/gzip"],
      ["PROVENANCE.json", "linux", "application/json"],
      ["SHA256SUMS", "linux", "text/plain"],
    ],
  );
  assert.equal((await putDraft(releases().durable, bootstrap)).status, 201);
  const expandedBootstrap = structuredClone(bootstrap);
  expandedBootstrap.assets.push(normalAssets("stable", "v0.5.0").find((asset) =>
    asset.platform === "npm"
  )!);
  assert.equal((await putDraft(releases().durable, expandedBootstrap)).status, 400);
});

test("drafts accept only their exact release-owned object namespace", async () => {
  const stable = stableDraft("v0.6.0", COMMIT_A, "v0.5.0");
  const stablePrefix = "distribution/stable/v0.6.0/";
  for (const key of [
    `runs/${COMMIT_A}/artifacts/npm-package.tgz`,
    "macos/jobs/macos-release-build-release-v0.6.0/assets/nanocodex-aarch64-apple-darwin",
    "distribution/stable/v0.6.1/components/npm/nanocodex-0.6.0.tgz",
    `distribution/commit/${COMMIT_A}/components/npm/nanocodex-0.6.0.tgz`,
    "release-import/stable/v0.5.0/nanocodex-x86_64-unknown-linux-gnu",
    `${stablePrefix}../v0.6.1/asset`,
  ]) {
    const memory = releases();
    const input = structuredClone(stable);
    input.assets[0]!.key = key;
    const response = await putDraft(memory.durable, input);
    assert.equal(response.status, 400, key);
    assert.deepEqual(await response.json(), { error: "invalid_release_draft" });
  }
  assert.equal((await putDraft(releases().durable, stable)).status, 201);

  const commit = commitDraft(COMMIT_A, null);
  for (const key of [
    `runs/${COMMIT_A}/artifacts/npm-package.tgz`,
    `distribution/commit/${COMMIT_B}/components/npm/nightly.tgz`,
    `distribution/stable/v0.6.0/components/npm/nightly.tgz`,
    "release-import/stable/v0.5.0/nanocodex-x86_64-unknown-linux-gnu",
  ]) {
    const memory = releases();
    const input = structuredClone(commit);
    input.assets[0]!.key = key;
    assert.equal((await putDraft(memory.durable, input)).status, 400, key);
  }
  assert.equal((await putDraft(releases().durable, commit)).status, 201);

  const bootstrap = stableDraft("v0.5.0", COMMIT_V050, null);
  assert.ok(bootstrap.assets.every((asset) =>
    asset.key.startsWith("release-import/stable/v0.5.0/")
  ));
  assert.equal((await putDraft(releases().durable, bootstrap)).status, 201);
});

test("registered distribution staging is bounded, replayable, and durably collected", async () => {
  const memory = releases();
  const stageId = "7".repeat(64);
  const prefix = `distribution-staging/stable/v1.2.3/${stageId}/components/linux`;
  const keys = [
    `${prefix}/nanocodex-x86_64-unknown-linux-gnu`,
    `${prefix}/nanocodex-x86_64-unknown-linux-gnu.gz`,
  ];
  const input = { version: 1, commit: COMMIT_A, keys };
  assert.equal(
    (await releaseStagingRequest(
      memory.durable,
      "PUT",
      "stable",
      "v1.2.3",
      stageId,
      input,
      null,
    )).status,
    401,
  );
  const invalid = await releaseStagingRequest(
    memory.durable,
    "PUT",
    "stable",
    "v1.2.3",
    stageId,
    { ...input, keys: ["distribution/stable/v1.2.3/final-object"] },
  );
  assert.equal(invalid.status, 400);

  const created = await releaseStagingRequest(
    memory.durable,
    "PUT",
    "stable",
    "v1.2.3",
    stageId,
    input,
  );
  assert.equal(created.status, 201);
  const createdRecord = (await created.json() as {
    staging: {
      expiresAt: string;
      fenceId: string;
      generation: number;
      keys: string[];
      state: string;
    };
    fence: { fenceId: string; generation: number };
  });
  assert.deepEqual(createdRecord.staging.keys, [...keys].sort());
  assert.equal(createdRecord.staging.state, "active");
  assert.deepEqual(createdRecord.fence, {
    fenceId: createdRecord.staging.fenceId,
    generation: createdRecord.staging.generation,
  });
  assert.ok(
    Date.parse(createdRecord.staging.expiresAt) - Date.now() <=
      7 * 24 * 60 * 60 * 1_000,
  );
  const replayed = await releaseStagingRequest(
    memory.peer,
    "PUT",
    "stable",
    "v1.2.3",
    stageId,
    input,
  );
  assert.equal(replayed.status, 200, "registration acknowledgement loss is replayable");
  const replayedRecord = await replayed.json() as {
    fence: { fenceId: string; generation: number };
  };
  assert.equal(replayedRecord.fence.generation, createdRecord.fence.generation + 1);
  assert.notEqual(replayedRecord.fence.fenceId, createdRecord.fence.fenceId);
  assert.equal(
    (await releaseStagingRequest(
      memory.peer,
      "PUT",
      "stable",
      "v1.2.3",
      stageId,
      { ...input, commit: COMMIT_B },
    )).status,
    409,
  );

  const storageKey = `release-staging:stable:v1.2.3:${stageId}`;
  const delayedOldCleanup = await releaseStagingRequest(
    memory.durable,
    "DELETE",
    "stable",
    "v1.2.3",
    stageId,
    { commit: COMMIT_A, ...createdRecord.fence },
  );
  assert.equal(delayedOldCleanup.status, 204);
  assert.equal(
    memory.hasStoredValue(storageKey),
    true,
    "a delayed old cleanup cannot clear renewed staging ownership",
  );
  for (const key of keys) {
    memory.bucket.seedBytes(key, new Uint8Array([1, 2, 3]), "application/octet-stream", {});
  }
  const finalizedKey = "distribution/stable/v1.2.3/components/linux/finalized";
  memory.bucket.seedBytes(
    finalizedKey,
    new Uint8Array([9]),
    "application/octet-stream",
    {},
  );
  memory.bucket.failNextDelete();
  const failed = await releaseStagingRequest(
    memory.durable,
    "DELETE",
    "stable",
    "v1.2.3",
    stageId,
    { commit: COMMIT_A, ...replayedRecord.fence },
  );
  assert.equal(failed.status, 503);
  assert.ok(keys.every((key) => memory.bucket.has(key)));
  assert.equal(memory.hasStoredValue(storageKey), true);
  assert.equal(
    (await releaseStagingRequest(
      memory.peer,
      "PUT",
      "stable",
      "v1.2.3",
      stageId,
      input,
    )).status,
    409,
    "a collecting tombstone cannot be renewed over an in-flight delete",
  );
  memory.setGcDue(storageKey, Date.now() - 1);
  await memory.durable.alarm();
  assert.ok(keys.every((key) => !memory.bucket.has(key)));
  assert.equal(memory.bucket.has(finalizedKey), true, "GC cannot address final release keys");
  assert.equal(memory.hasStoredValue(storageKey), false);
  assert.equal(
    (await releaseStagingRequest(
      memory.peer,
      "DELETE",
      "stable",
      "v1.2.3",
      stageId,
      { commit: COMMIT_A, ...replayedRecord.fence },
    )).status,
    204,
    "cleanup acknowledgement loss is replayable",
  );
});

test("Workflow registers staging and a complete draft before immutable final writes", () => {
  const source = readFileSync(new URL("./ciWorkflow.ts", import.meta.url), "utf8");
  const pipelineStart = source.indexOf("private async distributionPipeline(");
  const pipelineEnd = source.indexOf("async function replayedNightlyPublication(");
  assert.ok(pipelineStart >= 0 && pipelineEnd > pipelineStart);
  const pipeline = source.slice(pipelineStart, pipelineEnd);
  const register = pipeline.indexOf("await registerReleaseStaging(");
  const runner = pipeline.indexOf("const linuxPromise = buildParent.runner(");
  const draft = pipeline.indexOf("const owningDraft = await stageDraft();");
  const immutable = pipeline.indexOf("publish ${distribution.channel} immutable distribution assets");
  const cleanup = pipeline.indexOf("await deleteReleaseStaging(");
  assert.ok(register >= 0 && register < runner, "staging ownership precedes runner uploads");
  assert.ok(draft > runner && draft < immutable, "the exact draft precedes final copies");
  assert.ok(cleanup > immutable, "staging is collected only after final reconciliation");
  assert.match(pipeline, /key: `\$\{stagingComponentPrefix\}\/\$\{output\.name\}`/);
  assert.doesNotMatch(
    pipeline,
    /key: `\$\{componentPrefix\}\/\$\{output\.name\}`/,
    "runner outputs never target immutable final keys",
  );
  assert.match(
    source,
    /body: JSON\.stringify\(\{ commit, \.\.\.fence \}\)/,
    "Workflow cleanup carries the exact returned staging fence",
  );
});

test("promoted npm and macOS bytes survive source collection and acknowledgement loss", async () => {
  const memory = releases();
  await publish(memory, stableDraft("v0.5.0", COMMIT_V050, null));

  const stable = stableDraft("v0.6.0", COMMIT_A, "v0.5.0");
  const stableNpm = promotion(
    memory.bucket,
    stable,
    "npm",
    `runs/${COMMIT_A}/artifacts/npm-package.tgz`,
    new TextEncoder().encode("exact tested stable npm bytes"),
    { head: COMMIT_A, kind: "npm-package" },
  );
  const stableMac = promotion(
    memory.bucket,
    stable,
    "macos",
    "macos/jobs/macos-release-build-stable/attempts/claim/assets/nanocodex-aarch64-apple-darwin",
    new TextEncoder().encode("exact tested stable macOS bytes"),
    { job: "macos-release-build-stable", platform: "aarch64-apple-darwin" },
  );
  memory.bucket.failNextPutAfterStore(stableNpm.asset.key);
  await assert.rejects(
    promoteCiReleaseAsset(memory.bucket as unknown as R2Bucket, stableNpm),
    /Failed to promote immutable release asset/,
  );
  await memory.bucket.delete(stableNpm.source.key);
  assert.deepEqual(
    await promoteCiReleaseAsset(memory.bucket as unknown as R2Bucket, stableNpm),
    stableNpm.asset,
  );
  assert.deepEqual(
    await promoteCiReleaseAsset(memory.bucket as unknown as R2Bucket, stableMac),
    stableMac.asset,
  );
  for (const asset of stable.assets) {
    if (asset.platform !== "npm" && asset.platform !== "aarch64-apple-darwin") {
      memory.bucket.seed(asset);
    }
  }
  assert.equal((await putDraft(memory.durable, stable)).status, 201);
  await memory.bucket.delete(stableMac.source.key);
  assert.equal((await finalize(memory.durable, stable)).status, 201);
  for (const asset of [stableNpm.asset, stableMac.asset]) {
    const immutable = await publicFetch(
      memory.durable,
      `/api/releases/releases/stable/v0.6.0/assets/${asset.name}`,
    );
    assert.equal(immutable.status, 200);
    assert.equal(await immutable.text(), asset.platform === "npm"
      ? "exact tested stable npm bytes"
      : "exact tested stable macOS bytes");
    const latest = await publicFetch(
      memory.durable,
      `/api/releases/channels/latest/assets/${asset.name}`,
    );
    assert.equal(latest.status, 200);
    assert.equal(await latest.text(), asset.platform === "npm"
      ? "exact tested stable npm bytes"
      : "exact tested stable macOS bytes");
  }

  const nightly = commitDraft(COMMIT_B, null);
  const nightlyNpm = promotion(
    memory.bucket,
    nightly,
    "npm",
    `runs/${COMMIT_B}/artifacts/npm-package.tgz`,
    new TextEncoder().encode("exact tested nightly npm bytes"),
    { head: COMMIT_B, kind: "npm-package" },
  );
  const nightlyMac = promotion(
    memory.bucket,
    nightly,
    "macos",
    "macos/jobs/macos-release-build-nightly/attempts/claim/assets/nanocodex-aarch64-apple-darwin",
    new TextEncoder().encode("exact tested nightly macOS bytes"),
    { job: "macos-release-build-nightly", platform: "aarch64-apple-darwin" },
  );
  await promoteCiReleaseAsset(memory.bucket as unknown as R2Bucket, nightlyNpm);
  await promoteCiReleaseAsset(memory.bucket as unknown as R2Bucket, nightlyMac);
  for (const asset of nightly.assets) {
    if (asset.platform !== "npm" && asset.platform !== "aarch64-apple-darwin") {
      memory.bucket.seed(asset);
    }
  }
  assert.equal((await putDraft(memory.durable, nightly)).status, 201);
  memory.setCurrentMaster(COMMIT_B);
  assert.equal((await finalize(memory.durable, nightly)).status, 201);
  await memory.bucket.delete(nightlyNpm.source.key);
  await memory.bucket.delete(nightlyMac.source.key);
  for (const asset of [nightlyNpm.asset, nightlyMac.asset]) {
    const expected = asset.platform === "npm"
      ? "exact tested nightly npm bytes"
      : "exact tested nightly macOS bytes";
    const immutable = await publicFetch(
      memory.durable,
      `/api/releases/releases/commit/${COMMIT_B}/assets/${asset.name}`,
    );
    assert.equal(immutable.status, 200);
    assert.equal(await immutable.text(), expected);
    const rolling = await publicFetch(
      memory.durable,
      `/api/releases/channels/nightly/assets/${asset.name}`,
    );
    assert.equal(rolling.status, 200);
    assert.equal(await rolling.text(), expected);
  }

  const invalid = releases();
  const invalidDraft = commitDraft(COMMIT_C, null);
  const invalidPromotion = promotion(
    invalid.bucket,
    invalidDraft,
    "npm",
    `runs/${COMMIT_C}/artifacts/npm-package.tgz`,
    new TextEncoder().encode("wrong native content type"),
    { head: COMMIT_C, kind: "npm-package" },
    "application/octet-stream",
  );
  await assert.rejects(
    promoteCiReleaseAsset(invalid.bucket as unknown as R2Bucket, invalidPromotion),
    /missing or invalid/,
  );
  assert.equal(await invalid.bucket.head(invalidPromotion.asset.key), null);
});

test("authenticated draft reads expose verified bytes without leaking R2 keys", async () => {
  const memory = releases();
  const input = stableDraft("v0.5.0", COMMIT_V050, null);
  assert.equal((await putDraft(memory.durable, input)).status, 201);
  const descriptorPath = "/api/releases/drafts/stable/v0.5.0";
  const assetPath = `${descriptorPath}/assets/${input.assets[0]!.name}`;

  for (const path of [descriptorPath, assetPath]) {
    const unauthorized = await draftFetch(memory.durable, path, {}, null);
    assert.equal(unauthorized.status, 401);
    assert.equal(unauthorized.headers.get("www-authenticate"), "Bearer");
  }
  assert.equal(
    (await draftFetch(
      memory.durable,
      "/api/releases/drafts/stable/v01.2.3",
    )).status,
    400,
  );
  assert.equal(
    (await draftFetch(
      memory.durable,
      `${descriptorPath}/assets/${encodeURIComponent("bad/name")}`,
    )).status,
    400,
  );
  assert.equal(
    (await draftFetch(
      memory.durable,
      `${descriptorPath}/assets/${encodeURIComponent(`${input.assets[0]!.name}\n`)}`,
    )).status,
    400,
  );

  const descriptorResponse = await draftFetch(memory.durable, descriptorPath);
  assert.equal(descriptorResponse.status, 200);
  assert.equal(descriptorResponse.headers.get("cache-control"), "no-store");
  const descriptorLength = descriptorResponse.headers.get("content-length");
  const { draft } = await descriptorResponse.json() as { draft: PublicDraft };
  assert.equal(draft.version, 1);
  assert.equal(draft.kind, input.kind);
  assert.equal(draft.id, input.tag);
  assert.equal(draft.tag, input.tag);
  assert.equal(draft.commit, input.commit);
  assert.equal(draft.channel, input.channel);
  assert.equal(draft.expectedChannel, input.expectedChannel);
  assert.match(draft.createdAt, /^\d{4}-\d\d-\d\dT/);
  assert.equal(JSON.stringify(draft).includes('"key"'), false);
  assert.equal(JSON.stringify(draft).includes(input.assets[0]!.key), false);
  assert.deepEqual(
    draft.assets.map((asset) => asset.name),
    [...input.assets].sort((left, right) => left.name.localeCompare(right.name)).map((asset) =>
      asset.name
    ),
  );
  for (const asset of draft.assets) {
    assert.equal(
      asset.downloadPath,
      `${descriptorPath}/assets/${encodeURIComponent(asset.name)}`,
    );
  }

  const descriptorHead = await draftFetch(memory.durable, descriptorPath, { method: "HEAD" });
  assert.equal(descriptorHead.status, 200);
  assert.equal(descriptorHead.headers.get("content-length"), descriptorLength);
  assert.equal(descriptorHead.headers.get("cache-control"), "no-store");
  assert.equal(await descriptorHead.text(), "");

  memory.bucket.seedAll(input.assets);
  const selected = input.assets[0]!;
  memory.bucket.setChecksum(selected.key, "0".repeat(64));
  const tampered = await draftFetch(memory.durable, assetPath);
  assert.equal(tampered.status, 503);
  assert.deepEqual(await tampered.json(), { error: "release_asset_unavailable" });

  memory.bucket.setChecksum(selected.key, selected.sha256);
  const asset = await draftFetch(memory.durable, assetPath);
  assert.equal(asset.status, 200);
  assert.equal(asset.headers.get("cache-control"), "no-store");
  assert.equal(asset.headers.get("content-location"), assetPath);
  assert.equal(asset.headers.get("content-disposition"), `attachment; filename="${selected.name}"`);
  assert.equal(asset.headers.get("content-length"), String(selected.size));
  assert.equal(asset.headers.get("content-type"), selected.contentType);
  assert.equal(asset.headers.get("etag"), `"${selected.sha256}"`);
  assert.equal(asset.headers.get("x-nanocodex-release"), input.tag);
  assert.equal(asset.headers.get("x-nanocodex-sha256"), selected.sha256);
  assert.equal(asset.headers.get("content-encoding"), null);
  assert.equal(asset.headers.get("accept-ranges"), null);
  assert.deepEqual(
    new Uint8Array(await asset.arrayBuffer()),
    new Uint8Array(selected.size).fill(selected.name.charCodeAt(0)),
  );

  const assetHead = await draftFetch(memory.durable, assetPath, { method: "HEAD" });
  assert.equal(assetHead.status, 200);
  assert.equal(assetHead.headers.get("content-length"), String(selected.size));
  assert.equal(assetHead.headers.get("x-nanocodex-sha256"), selected.sha256);
  assert.equal(await assetHead.text(), "");

  assert.equal((await finalize(memory.durable, input)).status, 201);
  for (const path of [descriptorPath, assetPath]) {
    const missing = await draftFetch(memory.durable, path);
    assert.equal(missing.status, 404);
    assert.deepEqual(await missing.json(), { error: "release_draft_not_found" });
  }
});

test("draft asset recovery uploads are bounded, native-hashed, and replay-safe", async () => {
  const memory = releases();
  const bytes = new TextEncoder().encode("legacy v0.5.0 release asset bytes");
  const input = stableDraft("v0.5.0", COMMIT_V050, null);
  const selected = input.assets.find((asset) =>
    asset.name === "nanocodex-x86_64-unknown-linux-gnu"
  )!;
  selected.size = bytes.byteLength;
  selected.sha256 = createHash("sha256").update(bytes).digest("hex");
  selected.contentType = "application/octet-stream";
  assert.equal((await putDraft(memory.durable, input)).status, 201);
  const descriptorPath = "/api/releases/drafts/stable/v0.5.0";
  const uploadPath = `${descriptorPath}/assets/${selected.name}`;
  const before = await (await draftFetch(memory.durable, descriptorPath)).json();

  assert.equal(
    (await uploadDraftAsset(memory.durable, uploadPath, bytes, selected, { token: null })).status,
    401,
  );
  assert.equal(
    (await uploadDraftAsset(
      memory.durable,
      `${descriptorPath}/assets/not-declared`,
      bytes,
      selected,
    )).status,
    404,
  );
  const missingBodyHeaders = new Headers({
    "content-length": String(selected.size),
    "content-type": selected.contentType,
    "x-nanocodex-sha256": selected.sha256,
  });
  const missingBody = await draftFetch(memory.durable, uploadPath, {
    method: "PUT",
    headers: missingBodyHeaders,
  });
  assert.equal(missingBody.status, 400);
  assert.deepEqual(await missingBody.json(), { error: "invalid_release_asset_upload" });
  const invalidHeaders: Array<Record<string, string | null>> = [
    { "content-length": null },
    { "content-length": `0${bytes.byteLength}` },
    { "content-length": String(bytes.byteLength + 1) },
    { "content-type": "application/gzip" },
    { "x-nanocodex-sha256": "0".repeat(64) },
    { "content-encoding": "gzip" },
    { "content-range": `bytes 0-${bytes.byteLength - 1}/${bytes.byteLength}` },
  ];
  for (const headers of invalidHeaders) {
    const invalid = await uploadDraftAsset(
      memory.durable,
      uploadPath,
      bytes,
      selected,
      { headers },
    );
    assert.equal(invalid.status, 400, JSON.stringify(headers));
    assert.deepEqual(await invalid.json(), { error: "invalid_release_asset_upload" });
  }
  assert.equal(memory.bucket.puts.length, 0);

  const oversizedMemory = releases();
  const oversizedInput = stableDraft("v0.5.0", COMMIT_V050, null);
  const oversizedAsset = oversizedInput.assets.find((asset) => asset.name === selected.name)!;
  oversizedAsset.size = 90 * 1024 * 1024 + 1;
  assert.equal((await putDraft(oversizedMemory.durable, oversizedInput)).status, 201);
  const oversized = await uploadDraftAsset(
    oversizedMemory.durable,
    uploadPath,
    bytes,
    oversizedAsset,
  );
  assert.equal(oversized.status, 413);
  assert.deepEqual(await oversized.json(), { error: "release_asset_upload_too_large" });
  assert.equal(oversizedMemory.bucket.puts.length, 0);

  const created = await uploadDraftAsset(memory.durable, uploadPath, bytes, selected);
  assert.equal(created.status, 201);
  assert.equal(created.headers.get("cache-control"), "no-store");
  const createdBody = await created.json() as {
    asset: PublicDraft["assets"][number];
    uploaded: boolean;
  };
  assert.equal(createdBody.uploaded, true);
  assert.equal("key" in createdBody.asset, false);
  assert.equal(createdBody.asset.name, selected.name);
  assert.equal(createdBody.asset.downloadPath, uploadPath);
  assert.deepEqual(memory.bucket.puts, [{
    key: selected.key,
    sha256: selected.sha256,
    contentType: selected.contentType,
    streamed: true,
  }]);
  assert.deepEqual(
    new Uint8Array(await (await draftFetch(memory.durable, uploadPath)).arrayBuffer()),
    bytes,
  );

  const replayed = await uploadDraftAsset(memory.peer, uploadPath, bytes, selected);
  assert.equal(replayed.status, 200);
  assert.equal((await replayed.json() as { uploaded: boolean }).uploaded, false);
  assert.equal(memory.bucket.puts.length, 1);

  memory.bucket.setChecksum(selected.key, "0".repeat(64));
  const existingConflict = await uploadDraftAsset(memory.peer, uploadPath, bytes, selected);
  assert.equal(existingConflict.status, 409);
  assert.deepEqual(
    await existingConflict.json(),
    { error: "immutable_release_asset_conflict" },
  );
  assert.equal(memory.bucket.puts.length, 1);
  assert.equal((await draftFetch(memory.durable, uploadPath)).status, 503);

  memory.bucket.setChecksum(selected.key, selected.sha256);
  const tampered = bytes.slice();
  tampered[0] ^= 0xff;
  const rejected = await uploadDraftAsset(memory.peer, uploadPath, tampered, selected);
  assert.equal(rejected.status, 409);
  assert.deepEqual(await rejected.json(), { error: "immutable_release_asset_conflict" });
  assert.deepEqual(
    new Uint8Array(await (await draftFetch(memory.durable, uploadPath)).arrayBuffer()),
    bytes,
  );
  assert.deepEqual(await (await draftFetch(memory.durable, descriptorPath)).json(), before);

  for (const asset of input.assets) {
    if (asset.key !== selected.key) memory.bucket.seed(asset);
  }
  assert.equal((await finalize(memory.durable, input)).status, 201);
  assert.equal(
    (await uploadDraftAsset(memory.durable, uploadPath, bytes, selected)).status,
    404,
  );
});

test("finalization verifies R2 and publishes an independently reproducible manifest", async () => {
  const memory = releases();
  const input = stableDraft("v0.5.0", COMMIT_V050, null);
  assert.equal((await putDraft(memory.durable, input)).status, 201);
  memory.bucket.seedAll(input.assets);
  memory.bucket.setChecksum(input.assets[0]!.key, "0".repeat(64));

  const rejected = await finalize(memory.durable, input);
  assert.equal(rejected.status, 409);
  assert.deepEqual(await rejected.json(), {
    error: "release_assets_invalid",
    assets: [{ name: input.assets[0]!.name, key: input.assets[0]!.key }],
  });

  memory.bucket.setChecksum(input.assets[0]!.key, input.assets[0]!.sha256);
  const published = await finalize(memory.durable, input);
  assert.equal(published.status, 201);
  assert.equal(published.headers.get("cache-control"), "no-store");
  const publication = await published.json() as {
    manifest: PublicManifest;
    pointer: CiReleasePointer;
  };
  assert.equal(publication.pointer.id, "v0.5.0");
  assert.equal(publication.pointer.generation, 1);
  assert.match(publication.manifest.manifestSha256, /^[a-f0-9]{64}$/);
  assert.equal(
    publication.manifest.manifestSha256,
    reproducibleManifestSha256(publication.manifest),
  );
  const name = input.assets[0]!.name;
  const publicAsset = publication.manifest.assets.find((asset) => asset.name === name)!;
  assert.equal("key" in publicAsset, false);
  assert.equal(
    publicAsset.downloadPath,
    `/api/releases/releases/stable/v0.5.0/assets/${name}`,
  );
  const tampered = structuredClone(publication.manifest);
  tampered.assets[0]!.downloadPath = "/api/releases/releases/stable/v0.5.0/assets/other";
  assert.notEqual(reproducibleManifestSha256(tampered), tampered.manifestSha256);

  const manifestLocation = "/api/releases/releases/stable/v0.5.0";
  const manifest = await publicFetch(
    memory.durable,
    manifestLocation,
  );
  assert.equal(manifest.status, 200);
  assert.equal(manifest.headers.get("cache-control"), "public, max-age=31536000, immutable");
  assert.equal(manifest.headers.get("etag"), `"${publication.manifest.manifestSha256}"`);
  assert.deepEqual(await manifest.json(), publication.manifest);

  const asset = await publicFetch(
    memory.durable,
    publicAsset.downloadPath,
  );
  assert.equal(asset.status, 200);
  assert.equal(asset.headers.get("cache-control"), "public, max-age=31536000, immutable");
  assert.equal(asset.headers.get("etag"), `"${input.assets[0]!.sha256}"`);
  assert.equal(asset.headers.get("x-nanocodex-sha256"), input.assets[0]!.sha256);
  assert.equal((await asset.arrayBuffer()).byteLength, input.assets[0]!.size);

  const rolling = await publicFetch(
    memory.durable,
    `/api/releases/channels/latest/assets/${name}`,
    { method: "HEAD" },
  );
  assert.equal(rolling.status, 200);
  assert.equal(rolling.headers.get("cache-control"), "no-store");
  assert.equal(
    rolling.headers.get("content-location"),
    `/api/releases/releases/stable/v0.5.0/assets/${name}`,
  );
  assert.equal(rolling.headers.get("x-nanocodex-release"), "v0.5.0");
  assert.equal(rolling.headers.get("x-nanocodex-sha256"), input.assets[0]!.sha256);
  assert.equal(await rolling.text(), "");
  assert.equal(
    (await publicFetch(memory.durable, rolling.headers.get("content-location")!, {
      method: "HEAD",
    })).status,
    200,
  );

  assert.equal((await finalize(memory.durable, input)).status, 200);
  assert.equal((await putDraft(memory.durable, input)).status, 200);
  const channel = await publicFetch(memory.durable, "/api/releases/channels/latest");
  assert.equal(channel.status, 200);
  assert.equal(channel.headers.get("cache-control"), "no-store");
  assert.equal(channel.headers.get("content-location"), manifestLocation);
  assert.equal(channel.headers.get("etag"), null);
  const channelBody = await channel.json() as {
    pointer: CiReleasePointer;
    manifest: PublicManifest;
  };
  assert.equal(channelBody.pointer.generation, 1);
  assert.deepEqual(channelBody.manifest, publication.manifest);
  assert.equal(
    (await publicFetch(memory.durable, channel.headers.get("content-location")!)).status,
    200,
  );
  for (const publicAsset of channelBody.manifest.assets) {
    assert.match(publicAsset.downloadPath, /^\/api\/releases\/releases\//);
    assert.equal(
      (await publicFetch(memory.durable, publicAsset.downloadPath, { method: "HEAD" })).status,
      200,
    );
  }

  memory.bucket.setChecksum(input.assets[0]!.key, "f".repeat(64));
  assert.equal(
    (await publicFetch(
      memory.durable,
      `/api/releases/releases/stable/v0.5.0/assets/${name}`,
    )).status,
    503,
  );
});

test("latest advances by CAS without mutating releases or accepting version rollback", async () => {
  const memory = releases();
  const first = stableDraft("v0.5.0", COMMIT_V050, null);
  await publish(memory, first);

  const stale = stableDraft("v0.6.0", COMMIT_B, "v0.4.0");
  assert.equal((await putDraft(memory.durable, stale)).status, 201);
  memory.bucket.seedAll(stale.assets);
  const staleFinalize = await finalize(memory.durable, stale);
  assert.equal(staleFinalize.status, 409);
  assert.equal(
    (await staleFinalize.json() as { current: CiReleasePointer }).current.id,
    "v0.5.0",
  );

  const next = stableDraft("v0.7.0", COMMIT_B, "v0.5.0");
  await publish(memory, next);
  const current = await channelPointer(memory.durable, "latest");
  assert.equal(current.id, "v0.7.0");
  assert.equal(current.generation, 2);

  const rollback = stableDraft("v0.6.1", COMMIT_C, "v0.7.0");
  assert.equal((await putDraft(memory.durable, rollback)).status, 201);
  memory.bucket.seedAll(rollback.assets);
  assert.deepEqual(await (await finalize(memory.durable, rollback)).json(), {
    error: "latest_must_advance",
    current,
  });
  assert.equal(
    (await memory.durable.fetch(
      new Request("https://release.test/releases/stable/v0.5.0"),
    )).status,
    200,
  );
  assert.equal((await channelPointer(memory.durable, "latest")).id, "v0.7.0");
});

test("commit releases atomically advance the rolling nightly pointer", async () => {
  const memory = releases();
  const first = commitDraft(COMMIT_A, null);
  await publish(memory, first);
  assert.equal((await channelPointer(memory.durable, "nightly")).id, COMMIT_A);

  const second = commitDraft(COMMIT_B, COMMIT_A);
  memory.setCurrentMaster(COMMIT_B);
  await publish(memory, second);
  const pointer = await channelPointer(memory.durable, "nightly");
  assert.equal(pointer.id, COMMIT_B);
  assert.equal(pointer.tag, `nightly-${COMMIT_B}`);
  assert.equal(pointer.generation, 2);

  const resolved = await memory.durable.fetch(
    new Request("https://release.test/channels/nightly"),
  );
  assert.equal(resolved.status, 200);
  assert.equal(resolved.headers.get("cache-control"), "no-store");
  assert.equal(
    resolved.headers.get("content-location"),
    `/api/releases/releases/commit/${COMMIT_B}`,
  );
  assert.equal(
    (await resolved.json() as { manifest: { commit: string } }).manifest.commit,
    COMMIT_B,
  );
});

test("CiReleases recovers lost repository reservation acquire and release acknowledgements", async () => {
  const memory = releases({
    currentMaster: COMMIT_B,
    loseReservationAcquireAck: true,
    loseReservationReleaseAck: true,
  });
  const input = commitDraft(COMMIT_B, null);
  assert.equal((await putDraft(memory.durable, input)).status, 201);
  memory.bucket.seedAll(input.assets);
  const lease = await acquireLease(
    memory.durable,
    publicationLeaseIdentity({
      owner: "nightly:reservation-lost-ack",
      kind: "commit",
      id: COMMIT_B,
      commit: COMMIT_B,
    }),
  );
  const finalized = await finalizeRequest(memory.durable, input, { fence: lease });
  assert.equal(finalized.status, 201);
  assert.equal(memory.reservationAcquireCalls, 2);
  assert.equal(memory.reservationReleaseCalls, 2);
  assert.equal(memory.activeReservation, undefined);
  assert.equal((await releasePublicationLease(memory.durable, lease)).status, 204);
});

test("a committed release survives failed repository reservation release", async () => {
  const memory = releases({
    currentMaster: COMMIT_B,
    failReservationRelease: true,
  });
  const input = commitDraft(COMMIT_B, null);
  assert.equal((await putDraft(memory.durable, input)).status, 201);
  memory.bucket.seedAll(input.assets);
  const lease = await acquireLease(
    memory.durable,
    publicationLeaseIdentity({
      owner: "nightly:release-failure",
      kind: "commit",
      id: COMMIT_B,
      commit: COMMIT_B,
    }),
  );
  const originalError = console.error;
  const errors: unknown[][] = [];
  console.error = (...values: unknown[]) => {
    errors.push(values);
  };
  let finalized: Response;
  try {
    finalized = await finalizeRequest(memory.durable, input, { fence: lease });
  } finally {
    console.error = originalError;
  }
  assert.equal(finalized!.status, 201);
  assert.equal(memory.reservationReleaseCalls, 2);
  assert.equal(errors.length, 1);
  assert.equal((await channelPointer(memory.durable, "nightly")).id, COMMIT_B);
  assert.equal(
    (await publicFetch(memory.durable, `/api/releases/releases/commit/${COMMIT_B}`)).status,
    200,
  );
  const stranded = memory.activeReservation;
  assert.ok(stranded);
  assert.ok(Date.parse(stranded.expiresAt) - Date.now() <= 120_000);
  assert.ok(Date.parse(stranded.expiresAt) > Date.now());
  assert.equal(
    (await finalizeRequest(memory.peer, input, { fence: lease })).status,
    200,
    "committed state is replayed without a second reservation",
  );
  assert.equal((await releasePublicationLease(memory.durable, lease)).status, 204);
});

test("A/B reversed completion and stale leases cannot roll nightly backward", async () => {
  const staleFirst = releases({ currentMaster: COMMIT_A });
  const draftA = commitDraft(COMMIT_A, null);
  const draftB = commitDraft(COMMIT_B, null);
  assert.equal((await putDraft(staleFirst.durable, draftA)).status, 201);
  assert.equal((await putDraft(staleFirst.durable, draftB)).status, 201);
  staleFirst.bucket.seedAll([...draftA.assets, ...draftB.assets]);
  const leaseA = await acquireLease(
    staleFirst.durable,
    publicationLeaseIdentity({
      owner: "nightly:a",
      kind: "commit",
      id: COMMIT_A,
      commit: COMMIT_A,
    }),
  );
  staleFirst.setCurrentMaster(COMMIT_B);
  const staleA = await finalizeRequest(staleFirst.durable, draftA, { fence: leaseA });
  assert.equal(staleA.status, 409);
  assert.deepEqual(await staleA.json(), { error: "release_head_stale" });
  assert.equal((await releasePublicationLease(staleFirst.durable, leaseA)).status, 204);
  const leaseB = await acquireLease(
    staleFirst.durable,
    publicationLeaseIdentity({
      owner: "nightly:b",
      kind: "commit",
      id: COMMIT_B,
      commit: COMMIT_B,
    }),
    2,
  );
  assert.equal((await finalizeRequest(staleFirst.durable, draftB, { fence: leaseB })).status, 201);
  assert.equal((await releasePublicationLease(staleFirst.durable, leaseB)).status, 204);
  assert.equal((await channelPointer(staleFirst.durable, "nightly")).id, COMMIT_B);

  const newestFirst = releases({ currentMaster: COMMIT_B });
  assert.equal((await putDraft(newestFirst.durable, draftA)).status, 201);
  assert.equal((await putDraft(newestFirst.durable, draftB)).status, 201);
  newestFirst.bucket.seedAll([...draftA.assets, ...draftB.assets]);
  const newestLease = await acquireLease(
    newestFirst.durable,
    publicationLeaseIdentity({
      owner: "nightly:b",
      kind: "commit",
      id: COMMIT_B,
      commit: COMMIT_B,
    }),
  );
  assert.equal((await finalizeRequest(newestFirst.durable, draftB, {
    fence: newestLease,
  })).status, 201);
  assert.equal((await releasePublicationLease(newestFirst.durable, newestLease)).status, 204);
  const rejectedOldLease = await acquirePublicationLease(
    newestFirst.durable,
    publicationLeaseIdentity({
      owner: "nightly:a",
      kind: "commit",
      id: COMMIT_A,
      commit: COMMIT_A,
    }),
  );
  assert.equal(rejectedOldLease.status, 409);
  assert.deepEqual(await rejectedOldLease.json(), { error: "release_head_stale" });
  assert.equal((await channelPointer(newestFirst.durable, "nightly")).id, COMMIT_B);
});

test("same-predecessor nightly drafts rebase only their unpublished CAS", async () => {
  const memory = releases({ currentMaster: COMMIT_A });
  const first = commitDraft(COMMIT_A, null);
  const staleSecond = commitDraft(COMMIT_B, null);
  assert.equal((await putDraft(memory.durable, staleSecond)).status, 201);
  await publish(memory, first);

  memory.setCurrentMaster(COMMIT_B);
  const rebasedSecond = commitDraft(COMMIT_B, COMMIT_A);
  const rebased = await putDraft(memory.durable, rebasedSecond);
  assert.equal(rebased.status, 200);
  const rebasedBody = await rebased.json() as { draft: PublicDraft };
  assert.equal(rebasedBody.draft.expectedChannel, COMMIT_A);
  assert.deepEqual(
    rebasedBody.draft.assets.map(({ name, sha256 }) => ({ name, sha256 })),
    [...staleSecond.assets]
      .sort((left, right) => left.name.localeCompare(right.name))
      .map(({ name, sha256 }) => ({ name, sha256 })),
  );
  memory.bucket.seedAll(rebasedSecond.assets);
  assert.equal((await finalize(memory.durable, rebasedSecond)).status, 201);
  assert.equal((await channelPointer(memory.durable, "nightly")).id, COMMIT_B);

  const conflicting = structuredClone(rebasedSecond);
  conflicting.assets[0]!.sha256 = "0".repeat(64);
  assert.equal((await putDraft(memory.durable, conflicting)).status, 409);
});

test("nightly finalize restart and lost acknowledgement replay never mutate a newer pointer", async () => {
  const memory = releases({ currentMaster: COMMIT_B });
  const lostAck = commitDraft(COMMIT_B, null);
  assert.equal((await putDraft(memory.durable, lostAck)).status, 201);
  memory.bucket.seedAll(lostAck.assets);
  const lease = await acquireLease(
    memory.durable,
    publicationLeaseIdentity({
      owner: "nightly:lost-ack",
      kind: "commit",
      id: COMMIT_B,
      commit: COMMIT_B,
    }),
  );
  assert.equal((await finalizeRequest(memory.durable, lostAck, { fence: lease })).status, 201);
  const sameAttemptReplay = await finalizeRequest(memory.peer, lostAck, { fence: lease });
  assert.equal(sameAttemptReplay.status, 200);
  assert.equal(
    (await sameAttemptReplay.json() as { pointer: CiReleasePointer }).pointer.id,
    COMMIT_B,
  );
  assert.equal((await releasePublicationLease(memory.durable, lease)).status, 204);

  memory.setCurrentMaster(COMMIT_C);
  const successor = commitDraft(COMMIT_C, COMMIT_B);
  await publish(memory, successor);
  const oldRestart = await finalizeRequest(memory.peer, lostAck, { fence: lease });
  assert.equal(oldRestart.status, 200);
  assert.equal((await oldRestart.json() as { pointer: null }).pointer, null);
  assert.equal((await channelPointer(memory.durable, "nightly")).id, COMMIT_C);
});

test("exact draft replay renews retention and fences a stale expiry alarm", async () => {
  const memory = releases();
  const input = stableDraft("v0.5.0", COMMIT_V050, null);
  assert.equal((await putDraft(memory.durable, input)).status, 201);
  memory.bucket.seedAll(input.assets);
  memory.setDraftExpiry(input, Date.now() - 1);

  const paused = memory.pauseNextDraftList();
  const staleAlarm = memory.durable.alarm();
  await paused.observed;
  try {
    assert.equal(
      (await putDraft(memory.peer, input)).status,
      200,
      "an exact recovery replay renews draft ownership before final writes",
    );
  } finally {
    paused.resume();
  }
  await staleAlarm;

  assert.equal(
    (await draftFetch(
      memory.durable,
      "/api/releases/drafts/stable/v0.5.0",
    )).status,
    200,
  );
  assert.equal(memory.hasStoredValue("draft-gc:stable:v0.5.0"), false);
  assert.ok(input.assets.every((asset) => memory.bucket.has(asset.key)));
  assert.equal((await finalize(memory.durable, input)).status, 201);
});

test("expired unpublished drafts are durably collected without deleting leased or published assets", async () => {
  const expired = releases();
  const abandoned = stableDraft("v0.6.0", COMMIT_A, "v0.5.0");
  assert.equal((await putDraft(expired.durable, abandoned)).status, 201);
  expired.bucket.seedAll(abandoned.assets);
  expired.bucket.failNextDelete();
  expired.setDraftExpiry(abandoned, Date.now() - 1);
  await expired.durable.alarm();
  assert.equal((await draftFetch(
    expired.durable,
    "/api/releases/drafts/stable/v0.6.0",
  )).status, 404);
  assert.ok(abandoned.assets.every((asset) => expired.bucket.has(asset.key)));
  assert.equal(expired.hasStoredValue("draft-gc:stable:v0.6.0"), true);
  const collecting = await putDraft(expired.durable, abandoned);
  assert.equal(collecting.status, 409);
  assert.deepEqual(await collecting.json(), {
    error: "release_draft_collecting",
  });
  expired.setGcDue("draft-gc:stable:v0.6.0", Date.now() - 1);
  await expired.durable.alarm();
  assert.ok(abandoned.assets.every((asset) => !expired.bucket.has(asset.key)));
  assert.equal(expired.hasStoredValue("draft-gc:stable:v0.6.0"), false);
  assert.equal((await putDraft(expired.durable, abandoned)).status, 201);

  const leased = releases();
  const protectedDraft = stableDraft("v0.6.0", COMMIT_A, "v0.5.0");
  assert.equal((await putDraft(leased.durable, protectedDraft)).status, 201);
  leased.bucket.seedAll(protectedDraft.assets);
  const protectedLease = await acquireLease(
    leased.durable,
    publicationLeaseIdentity({
      owner: "stable:review",
      id: "v0.6.0",
      commit: COMMIT_A,
    }),
  );
  leased.setDraftExpiry(protectedDraft, Date.now() - 1);
  await leased.durable.alarm();
  assert.equal((await draftFetch(
    leased.durable,
    "/api/releases/drafts/stable/v0.6.0",
  )).status, 200);
  assert.ok(protectedDraft.assets.every((asset) => leased.bucket.has(asset.key)));
  assert.equal((await releasePublicationLease(leased.durable, protectedLease)).status, 204);

  const published = releases();
  const bootstrap = stableDraft("v0.5.0", COMMIT_V050, null);
  await publish(published, bootstrap);
  published.seedStoredDraft(bootstrap);
  published.setDraftExpiry(bootstrap, Date.now() - 1);
  await published.durable.alarm();
  assert.equal((await publicFetch(
    published.durable,
    "/api/releases/releases/stable/v0.5.0",
  )).status, 200);
  assert.ok(bootstrap.assets.every((asset) => published.bucket.has(asset.key)));
});

function publicationLeaseIdentity(
  overrides: Partial<PublicationLeaseIdentity> = {},
): PublicationLeaseIdentity {
  return {
    owner: OWNER_A,
    kind: "stable",
    id: "v1.2.3",
    commit: COMMIT_A,
    ...overrides,
  };
}

function acquirePublicationLease(
  durable: CiReleases,
  identity: PublicationLeaseIdentity,
  token: string | null = TOKEN,
): Promise<Response> {
  return publicationLeaseRequest(
    durable,
    "POST",
    "/publication-lease/acquire",
    identity,
    token,
  );
}

async function acquireLease(
  durable: CiReleases,
  identity: PublicationLeaseIdentity,
  generation = 1,
): Promise<CiPublicationLease> {
  const response = await acquirePublicationLease(durable, identity);
  assert.equal(response.status, 201);
  const lease = await response.json() as CiPublicationLease;
  assertPublicationLease(lease, identity, generation);
  return lease;
}

function heartbeatPublicationLease(
  durable: CiReleases,
  lease: Pick<CiPublicationLease, "leaseId" | "owner">,
  token: string | null = TOKEN,
): Promise<Response> {
  return publicationLeaseRequest(
    durable,
    "POST",
    `/publication-lease/${encodeURIComponent(lease.leaseId)}/heartbeat`,
    { owner: lease.owner },
    token,
  );
}

function releasePublicationLease(
  durable: CiReleases,
  lease: Pick<CiPublicationLease, "leaseId" | "owner">,
  token: string | null = TOKEN,
): Promise<Response> {
  return publicationLeaseRequest(
    durable,
    "DELETE",
    `/publication-lease/${encodeURIComponent(lease.leaseId)}`,
    { owner: lease.owner },
    token,
  );
}

function publicationLeaseRequest(
  durable: CiReleases,
  method: string,
  path: string,
  body: unknown,
  token: string | null = TOKEN,
): Promise<Response> {
  return rawPublicationLeaseRequest(durable, method, path, JSON.stringify(body), token);
}

function rawPublicationLeaseRequest(
  durable: CiReleases,
  method: string,
  path: string,
  body: string,
  token: string | null = TOKEN,
): Promise<Response> {
  return durable.fetch(new Request(`https://release.test${path}`, {
    method,
    headers: {
      "content-type": "application/json",
      ...(token ? { authorization: `Bearer ${token}` } : {}),
    },
    body,
  }));
}

function assertPublicationLease(
  lease: CiPublicationLease,
  identity: PublicationLeaseIdentity,
  generation = 1,
): void {
  assert.deepEqual(Object.keys(lease).sort(), [
    "commit",
    "expiresAt",
    "generation",
    "id",
    "kind",
    "leaseId",
    "owner",
    "version",
  ]);
  assert.equal(lease.version, 1);
  assert.match(
    lease.leaseId,
    /^(0|[1-9][0-9]{0,15})\.[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/,
  );
  assert.equal(lease.owner, identity.owner);
  assert.equal(lease.kind, identity.kind);
  assert.equal(lease.id, identity.id);
  assert.equal(lease.commit, identity.commit);
  assert.equal(lease.generation, generation);
  assert.equal(lease.leaseId.split(".", 1)[0], String(generation));
  assert.match(lease.expiresAt, /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$/);
}

async function publish(
  memory: ReturnType<typeof releases>,
  input: CiReleaseDraft,
): Promise<void> {
  if (!(input.kind === "stable" && input.tag === "v0.5.0" && input.commit === COMMIT_V050)) {
    memory.setCurrentMaster(input.commit);
  }
  assert.equal((await putDraft(memory.durable, input)).status, 201);
  memory.bucket.seedAll(input.assets);
  assert.equal((await finalize(memory.durable, input)).status, 201);
}

async function channelPointer(
  durable: CiReleases,
  channel: "latest" | "nightly",
): Promise<CiReleasePointer> {
  const response = await durable.fetch(new Request(`https://release.test/channels/${channel}`));
  assert.equal(response.status, 200);
  return (await response.json() as { pointer: CiReleasePointer }).pointer;
}

function publicFetch(
  durable: CiReleases,
  path: string,
  init?: RequestInit,
): Promise<Response> {
  assert.match(path, /^\/api\/releases(?:\/|$)/);
  const internalPath = path.slice("/api/releases".length) || "/";
  return durable.fetch(new Request(`https://release.test${internalPath}`, init));
}

function draftFetch(
  durable: CiReleases,
  path: string,
  init: RequestInit = {},
  token: string | null = TOKEN,
): Promise<Response> {
  assert.match(path, /^\/api\/releases\/drafts\//);
  const internalPath = path.slice("/api/releases".length);
  const headers = new Headers(init.headers);
  if (token) headers.set("authorization", `Bearer ${token}`);
  return durable.fetch(new Request(`https://release.test${internalPath}`, {
    ...init,
    headers,
  }));
}

function releaseStagingRequest(
  durable: CiReleases,
  method: "PUT" | "DELETE",
  kind: "stable" | "commit",
  id: string,
  stageId: string,
  body: unknown,
  token: string | null = TOKEN,
): Promise<Response> {
  return durable.fetch(new Request(
    `https://release.test/staging/${kind}/${id}/${stageId}`,
    {
      method,
      headers: {
        "content-type": "application/json",
        ...(token ? { authorization: `Bearer ${token}` } : {}),
      },
      body: JSON.stringify(body),
    },
  ));
}

function uploadDraftAsset(
  durable: CiReleases,
  path: string,
  body: Uint8Array,
  asset: Pick<CiReleaseAsset, "size" | "sha256" | "contentType">,
  options: {
    token?: string | null;
    headers?: Record<string, string | null>;
  } = {},
): Promise<Response> {
  const headers = new Headers({
    "content-length": String(asset.size),
    "content-type": asset.contentType,
    "x-nanocodex-sha256": asset.sha256,
  });
  for (const [name, value] of Object.entries(options.headers ?? {})) {
    if (value == null) headers.delete(name);
    else headers.set(name, value);
  }
  const requestBody = new Uint8Array(body.byteLength);
  requestBody.set(body);
  return draftFetch(
    durable,
    path,
    { method: "PUT", headers, body: requestBody.buffer },
    options.token === undefined ? TOKEN : options.token,
  );
}

function reproducibleManifestSha256(manifest: PublicManifest): string {
  const { manifestSha256: _manifestSha256, ...unsigned } = manifest;
  return createHash("sha256").update(canonicalJson(unsigned), "utf8").digest("hex");
}

function canonicalJson(value: unknown): string {
  if (value === null || typeof value === "boolean" || typeof value === "string") {
    return JSON.stringify(value);
  }
  if (typeof value === "number") {
    assert.ok(Number.isFinite(value));
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  assert.equal(typeof value, "object");
  const record = value as Record<string, unknown>;
  return `{${Object.keys(record).sort().map((key) =>
    `${JSON.stringify(key)}:${canonicalJson(record[key])}`
  ).join(",")}}`;
}

function putDraft(
  durable: CiReleases,
  input: CiReleaseDraft,
  token: string | null = TOKEN,
): Promise<Response> {
  const id = input.kind === "stable" ? input.tag : input.commit;
  return durable.fetch(new Request(`https://release.test/drafts/${input.kind}/${id}`, {
    method: "PUT",
    headers: {
      "content-type": "application/json",
      ...(token ? { authorization: `Bearer ${token}` } : {}),
    },
    body: JSON.stringify(input),
  }));
}

async function finalize(durable: CiReleases, input: CiReleaseDraft): Promise<Response> {
  const acquired = await acquirePublicationLease(
    durable,
    publicationLeaseIdentity({
      owner: "test-finalizer",
      kind: input.kind,
      id: input.kind === "stable" ? input.tag : input.commit,
      commit: input.commit,
    }),
  );
  assert.ok(acquired.status === 200 || acquired.status === 201);
  const lease = await acquired.json() as CiPublicationLease;
  try {
    return await finalizeRequest(durable, input, { fence: lease });
  } finally {
    assert.equal((await releasePublicationLease(durable, lease)).status, 204);
  }
}

function finalizeRequest(
  durable: CiReleases,
  input: CiReleaseDraft,
  options: {
    token?: string | null;
    fence?: Pick<CiPublicationLease, "leaseId" | "owner" | "generation">;
    headers?: Record<string, string>;
  } = {},
): Promise<Response> {
  const id = input.kind === "stable" ? input.tag : input.commit;
  const headers = new Headers();
  const token = options.token === undefined ? TOKEN : options.token;
  if (token) headers.set("authorization", `Bearer ${token}`);
  if (options.fence) {
    headers.set("x-nanocodex-publication-lease-id", options.fence.leaseId);
    headers.set("x-nanocodex-publication-lease-owner", options.fence.owner);
    headers.set(
      "x-nanocodex-publication-lease-generation",
      String(options.fence.generation),
    );
  }
  for (const [name, value] of Object.entries(options.headers ?? {})) {
    headers.set(name, value);
  }
  return durable.fetch(new Request(
    `https://release.test/drafts/${input.kind}/${id}/finalize`,
    { method: "POST", headers },
  ));
}

function stableDraft(
  tag: string,
  commit: string,
  expectedChannel: string | null,
): CiReleaseDraft {
  return {
    version: 1,
    kind: "stable",
    tag,
    commit,
    channel: "latest",
    expectedChannel,
    assets: tag === "v0.5.0" && commit === COMMIT_V050
      ? bootstrapAssets()
      : normalAssets("stable", tag),
  };
}

function commitDraft(commit: string, expectedChannel: string | null): CiReleaseDraft {
  return {
    version: 1,
    kind: "commit",
    tag: `nightly-${commit}`,
    commit,
    channel: "nightly",
    expectedChannel,
    assets: normalAssets("commit", commit),
  };
}

function normalAssets(kind: "stable" | "commit", id: string): CiReleaseAsset[] {
  const prefix = `distribution/${kind}/${id}`;
  const npmName = kind === "stable"
    ? `nanocodex-${id.slice(1)}.tgz`
    : `nanocodex-${id.slice(0, 10)}.tgz`;
  return [
    {
      name: "nanocodex-x86_64-unknown-linux-gnu.gz",
      platform: "x86_64-unknown-linux-gnu",
      key: `${prefix}/components/linux/nanocodex-x86_64-unknown-linux-gnu.gz`,
      size: 11,
      sha256: "1".repeat(64),
      contentType: "application/gzip",
    },
    {
      name: "nanocodex-aarch64-apple-darwin",
      platform: "aarch64-apple-darwin",
      key: `${prefix}/components/macos/nanocodex-aarch64-apple-darwin`,
      size: 13,
      sha256: "2".repeat(64),
      contentType: "application/octet-stream",
    },
    {
      name: npmName,
      platform: "npm",
      key: `${prefix}/components/npm/${npmName}`,
      size: 15,
      sha256: "5".repeat(64),
      contentType: "application/gzip",
    },
    {
      name: "SHA256SUMS",
      platform: "linux",
      key: `${prefix}/SHA256SUMS`,
      size: 17,
      sha256: "3".repeat(64),
      contentType: "text/plain; charset=utf-8",
    },
    {
      name: "PROVENANCE.json",
      platform: "linux",
      key: `${prefix}/PROVENANCE.json`,
      size: 19,
      sha256: "4".repeat(64),
      contentType: "application/json",
    },
    {
      name: "nanocodex-x86_64-unknown-linux-gnu",
      platform: "x86_64-unknown-linux-gnu",
      key: `${prefix}/components/linux/nanocodex-x86_64-unknown-linux-gnu`,
      size: 21,
      sha256: "6".repeat(64),
      contentType: "application/octet-stream",
    },
    {
      name: "nanocodex-vm-guest-x86_64-unknown-linux-musl",
      platform: "x86_64-unknown-linux-musl",
      key: `${prefix}/components/linux/nanocodex-vm-guest-x86_64-unknown-linux-musl`,
      size: 23,
      sha256: "7".repeat(64),
      contentType: "application/octet-stream",
    },
    {
      name: "nanocodex-vm-guest-x86_64-unknown-linux-musl.gz",
      platform: "x86_64-unknown-linux-musl",
      key: `${prefix}/components/linux/nanocodex-vm-guest-x86_64-unknown-linux-musl.gz`,
      size: 25,
      sha256: "8".repeat(64),
      contentType: "application/gzip",
    },
  ];
}

function bootstrapAssets(): CiReleaseAsset[] {
  const prefix = "release-import/stable/v0.5.0";
  return [
    {
      name: "nanocodex-x86_64-unknown-linux-gnu.gz",
      platform: "x86_64-unknown-linux-gnu",
      key: `${prefix}/nanocodex-x86_64-unknown-linux-gnu.gz`,
      size: 11,
      sha256: "1".repeat(64),
      contentType: "application/gzip",
    },
    {
      name: "nanocodex-aarch64-apple-darwin",
      platform: "aarch64-apple-darwin",
      key: `${prefix}/nanocodex-aarch64-apple-darwin`,
      size: 13,
      sha256: "2".repeat(64),
      contentType: "application/octet-stream",
    },
    {
      name: "SHA256SUMS",
      platform: "linux",
      key: `${prefix}/SHA256SUMS`,
      size: 17,
      sha256: "3".repeat(64),
      contentType: "text/plain",
    },
    {
      name: "PROVENANCE.json",
      platform: "linux",
      key: `${prefix}/PROVENANCE.json`,
      size: 19,
      sha256: "4".repeat(64),
      contentType: "application/json",
    },
    {
      name: "nanocodex-x86_64-unknown-linux-gnu",
      platform: "x86_64-unknown-linux-gnu",
      key: `${prefix}/nanocodex-x86_64-unknown-linux-gnu`,
      size: 21,
      sha256: "6".repeat(64),
      contentType: "application/octet-stream",
    },
  ];
}

function promotion(
  bucket: MemoryBucket,
  draft: CiReleaseDraft,
  component: "npm" | "macos",
  sourceKey: string,
  bytes: Uint8Array,
  sourceMetadata: Record<string, string>,
  storedContentType?: string,
) {
  const asset = draft.assets.find((candidate) =>
    component === "npm"
      ? candidate.platform === "npm"
      : candidate.platform === "aarch64-apple-darwin"
  );
  if (!asset) throw new Error(`missing ${component} fixture asset`);
  const sha256 = bucket.seedBytes(
    sourceKey,
    bytes,
    storedContentType ?? asset.contentType,
    sourceMetadata,
  );
  asset.size = bytes.byteLength;
  asset.sha256 = sha256;
  return {
    kind: draft.kind,
    id: draft.kind === "stable" ? draft.tag : draft.commit,
    commit: draft.commit,
    component,
    source: {
      key: sourceKey,
      size: bytes.byteLength,
      sha256,
      contentType: asset.contentType,
      customMetadata: { ...sourceMetadata, sha256 },
    },
    asset: { ...asset },
  };
}

function sourcePublication(head: string) {
  return {
    version: 1,
    head,
    branch: "master",
    ref: "refs/heads/master",
    lane: { type: "master" },
    archive: {
      key: `sources/${head}/source.tar.gz`,
      size: 123,
      sha256: "a".repeat(64),
    },
    tree: {
      key: `sources/${head}/tree.json`,
      size: 456,
      sha256: "b".repeat(64),
    },
    cargoLockBlob: "c".repeat(40),
    cargoVendor: {
      key: `cargo-vendor/${"c".repeat(40)}/${"d".repeat(64)}/bundle.tar.gz`,
      size: 789,
      sha256: "d".repeat(64),
    },
    rustSecRevision: "e".repeat(40),
    rustSec: {
      key: `rustsec-advisory-db/${"e".repeat(40)}/bundle.tar.gz`,
      size: 321,
      sha256: "f".repeat(64),
    },
    publishedAt: "2026-08-22T01:02:03.000Z",
  };
}

function authoritativeState(head: string) {
  return {
    publication: sourcePublication(head),
    run: {
      version: 1 as const,
      head,
      beforeHead: null,
      workflowId: `ci-${head}`,
      state: "dispatched" as const,
      attempts: 1,
      publishedAt: "2026-08-22T01:02:03.000Z",
    },
  };
}

function releases({
  token = TOKEN,
  currentMaster = COMMIT_A,
  loseReservationAcquireAck = false,
  loseReservationReleaseAck = false,
  failReservationRelease = false,
}: {
  token?: string | null;
  currentMaster?: string;
  loseReservationAcquireAck?: boolean;
  loseReservationReleaseAck?: boolean;
  failReservationRelease?: boolean;
} = {}) {
  const values = new Map<string, unknown>();
  let alarmAt: number | null = null;
  let authoritativeHead = currentMaster;
  let reservation: CiReleaseCommitReservation | undefined;
  let reservationGeneration = 0;
  let reservationAcquireCalls = 0;
  let reservationReleaseCalls = 0;
  let draftListPause: {
    observed: () => void;
    resume: Promise<void>;
  } | undefined;
  const operations = {
    async get<T>(key: string): Promise<T | undefined> {
      const value = values.get(key);
      return value === undefined ? undefined : structuredClone(value) as T;
    },
    async put(key: string, value: unknown): Promise<void> {
      values.set(key, structuredClone(value));
    },
    async delete(key: string): Promise<boolean> {
      return values.delete(key);
    },
    async list<T>({ prefix }: { prefix: string }): Promise<Map<string, T>> {
      const listed = new Map(
        [...values.entries()]
          .filter(([key]) => key.startsWith(prefix))
          .map(([key, value]) => [key, structuredClone(value) as T]),
      );
      const pause = prefix === "draft:" ? draftListPause : undefined;
      if (pause) {
        draftListPause = undefined;
        pause.observed();
        await pause.resume;
      }
      return listed;
    },
    async setAlarm(timestamp: number): Promise<void> {
      alarmAt = timestamp;
    },
  };
  let transactionTail = Promise.resolve();
  const state = {
    storage: {
      ...operations,
      transaction: async <T>(
        callback: (transaction: typeof operations) => Promise<T>,
      ): Promise<T> => {
        let unlock!: () => void;
        const predecessor = transactionTail;
        transactionTail = new Promise<void>((resolve) => {
          unlock = resolve;
        });
        await predecessor;
        try {
          return await callback(operations);
        } finally {
          unlock();
        }
      },
    },
  } as unknown as DurableObjectState;
  const bucket = new MemoryBucket();
  const env = {
    BACKUP_BUCKET: bucket as unknown as R2Bucket,
    CI_REPOSITORY: {
      idFromName: () => ({ toString: () => "nanocodex" }),
      get: () => ({
        fetch: async (request: RequestInfo | URL, init?: RequestInit) => {
          const incoming = new Request(request, init);
          const pathname = new URL(incoming.url).pathname;
          if (pathname === "/state" && incoming.method === "GET") {
            return Response.json(authoritativeState(authoritativeHead));
          }
          if (
            pathname === "/reservations/release-commit/acquire" &&
            incoming.method === "POST"
          ) {
            reservationAcquireCalls += 1;
            const input = await incoming.json() as {
              version: 1;
              owner: string;
              releaseKind: "stable" | "commit";
              releaseId: string;
              commit: string;
              publicationLeaseId: string;
              publicationLeaseGeneration: number;
            };
            if (input.commit !== authoritativeHead) {
              return Response.json({ error: "release_head_stale" }, { status: 409 });
            }
            const now = Date.now();
            const same = reservation && Date.parse(reservation.expiresAt) > now &&
              reservation.owner === input.owner &&
              reservation.releaseKind === input.releaseKind &&
              reservation.releaseId === input.releaseId &&
              reservation.commit === input.commit &&
              reservation.publicationLeaseId === input.publicationLeaseId &&
              reservation.publicationLeaseGeneration === input.publicationLeaseGeneration;
            if (reservation && Date.parse(reservation.expiresAt) > now && !same) {
              return Response.json(
                { error: "release_commit_reservation_conflict" },
                { status: 409 },
              );
            }
            const renewedAt = new Date(now).toISOString();
            const status = same ? 200 : 201;
            if (same) {
              reservation = {
                ...reservation!,
                renewedAt,
                expiresAt: new Date(now + 120_000).toISOString(),
              };
            } else {
              reservationGeneration = Math.max(
                reservationGeneration,
                reservation?.generation ?? 0,
              ) + 1;
              reservation = {
                version: 1,
                kind: "release-commit",
                reservationId: `${reservationGeneration}.${crypto.randomUUID()}`,
                owner: input.owner,
                releaseKind: input.releaseKind,
                releaseId: input.releaseId,
                commit: input.commit,
                publicationLeaseId: input.publicationLeaseId,
                publicationLeaseGeneration: input.publicationLeaseGeneration,
                generation: reservationGeneration,
                acquiredAt: renewedAt,
                renewedAt,
                expiresAt: new Date(now + 120_000).toISOString(),
              };
            }
            const proof = {
              reservation: structuredClone(reservation),
              ...authoritativeState(authoritativeHead),
            };
            if (loseReservationAcquireAck) {
              loseReservationAcquireAck = false;
              throw new Error("injected reservation acquire acknowledgement loss");
            }
            return Response.json(proof, { status });
          }
          const heartbeat = pathname.match(
            /^\/reservations\/release-commit\/([^/]+)\/heartbeat$/,
          );
          if (heartbeat && incoming.method === "POST") {
            const mutation = await incoming.json() as { owner: string; generation: number };
            if (
              !reservation || Date.parse(reservation.expiresAt) <= Date.now() ||
              reservation.reservationId !== heartbeat[1] ||
              reservation.owner !== mutation.owner ||
              reservation.generation !== mutation.generation
            ) {
              return Response.json(
                { error: "release_commit_reservation_not_held" },
                { status: 409 },
              );
            }
            const renewedAt = new Date().toISOString();
            reservation = {
              ...reservation,
              renewedAt,
              expiresAt: new Date(Date.now() + 120_000).toISOString(),
            };
            return Response.json({ reservation: structuredClone(reservation) });
          }
          const release = pathname.match(/^\/reservations\/release-commit\/([^/]+)$/);
          if (release && incoming.method === "DELETE") {
            reservationReleaseCalls += 1;
            if (failReservationRelease) {
              throw new Error("injected reservation release failure");
            }
            const mutation = await incoming.json() as { owner: string; generation: number };
            if (
              reservation?.reservationId === release[1] &&
              reservation.owner === mutation.owner &&
              reservation.generation === mutation.generation
            ) reservation = undefined;
            if (loseReservationReleaseAck) {
              loseReservationReleaseAck = false;
              throw new Error("injected reservation release acknowledgement loss");
            }
            return new Response(null, { status: 204 });
          }
          return Response.json({ error: "not_found" }, { status: 404 });
        },
      }),
    } as unknown as DurableObjectNamespace,
    CI_RELEASE_TOKEN: token ?? undefined,
  };
  return {
    bucket,
    durable: new CiReleases(state, env),
    peer: new CiReleases(state, env),
    activeLease(): CiPublicationLease {
      const lease = values.get("publication-lease:active") as CiPublicationLease | undefined;
      if (!lease) throw new Error("missing active publication lease");
      return structuredClone(lease);
    },
    setLeaseExpiry(leaseId: string, expiresAt: number): void {
      const lease = values.get("publication-lease:active") as CiPublicationLease | undefined;
      if (!lease || lease.leaseId !== leaseId) {
        throw new Error(`missing test publication lease ${leaseId}`);
      }
      values.set("publication-lease:active", {
        ...lease,
        expiresAt: new Date(expiresAt).toISOString(),
      });
    },
    setCurrentMaster(head: string): void {
      authoritativeHead = head;
    },
    get activeReservation(): CiReleaseCommitReservation | undefined {
      return reservation ? structuredClone(reservation) : undefined;
    },
    get reservationAcquireCalls(): number {
      return reservationAcquireCalls;
    },
    get reservationReleaseCalls(): number {
      return reservationReleaseCalls;
    },
    get alarmAt(): number | null {
      return alarmAt;
    },
    pauseNextDraftList(): { observed: Promise<void>; resume: () => void } {
      if (draftListPause) throw new Error("draft-list pause already installed");
      let observed!: () => void;
      let resume!: () => void;
      const observedPromise = new Promise<void>((resolve) => {
        observed = resolve;
      });
      const resumePromise = new Promise<void>((resolve) => {
        resume = resolve;
      });
      draftListPause = { observed, resume: resumePromise };
      return { observed: observedPromise, resume };
    },
    setDraftExpiry(input: CiReleaseDraft, expiresAt: number): void {
      const id = input.kind === "stable" ? input.tag : input.commit;
      const key = `draft:${input.kind}:${id}`;
      const draft = values.get(key) as Record<string, unknown> | undefined;
      if (!draft) throw new Error(`missing test draft ${key}`);
      values.set(key, { ...draft, expiresAt: new Date(expiresAt).toISOString() });
    },
    hasStoredValue(key: string): boolean {
      return values.has(key);
    },
    setGcDue(key: string, nextAttemptAt: number): void {
      const record = values.get(key) as Record<string, unknown> | undefined;
      if (!record) throw new Error(`missing test retention record ${key}`);
      values.set(key, {
        ...record,
        nextAttemptAt: new Date(nextAttemptAt).toISOString(),
      });
    },
    seedStoredDraft(input: CiReleaseDraft): void {
      const id = input.kind === "stable" ? input.tag : input.commit;
      values.set(`draft:${input.kind}:${id}`, {
        ...structuredClone(input),
        id,
        createdAt: new Date().toISOString(),
        expiresAt: new Date(Date.now() + 30 * 24 * 60 * 60 * 1_000).toISOString(),
      });
    },
  };
}

class MemoryBucket {
  readonly puts: Array<{
    key: string;
    sha256: string;
    contentType: string;
    streamed: boolean;
  }> = [];
  readonly #objects = new Map<string, {
    bytes: Uint8Array;
    sha256: string;
    contentType: string;
    customMetadata: Record<string, string>;
  }>();
  #failAfterStoreKey: string | undefined;
  #failDelete = false;

  seedAll(assets: CiReleaseAsset[]): void {
    for (const asset of assets) this.seed(asset);
  }

  seed(asset: CiReleaseAsset): void {
    this.#objects.set(asset.key, {
      bytes: new Uint8Array(asset.size).fill(asset.name.charCodeAt(0)),
      sha256: asset.sha256,
      contentType: asset.contentType,
      customMetadata: { sha256: asset.sha256 },
    });
  }

  seedBytes(
    key: string,
    bytes: Uint8Array,
    contentType: string,
    customMetadata: Record<string, string>,
  ): string {
    const sha256 = createHash("sha256").update(bytes).digest("hex");
    this.#objects.set(key, {
      bytes: bytes.slice(),
      sha256,
      contentType,
      customMetadata: { ...customMetadata, sha256 },
    });
    return sha256;
  }

  failNextPutAfterStore(key: string): void {
    this.#failAfterStoreKey = key;
  }

  failNextDelete(): void {
    this.#failDelete = true;
  }

  async delete(key: string | string[]): Promise<void> {
    if (this.#failDelete) {
      this.#failDelete = false;
      throw new Error("injected delete failure");
    }
    for (const selected of Array.isArray(key) ? key : [key]) this.#objects.delete(selected);
  }

  has(key: string): boolean {
    return this.#objects.has(key);
  }

  setChecksum(key: string, sha256: string): void {
    const object = this.#objects.get(key);
    if (!object) throw new Error(`missing test object ${key}`);
    object.sha256 = sha256;
  }

  async put(
    key: string,
    value: ReadableStream | ArrayBuffer | ArrayBufferView | string | null | Blob,
    options: R2PutOptions = {},
  ): Promise<R2Object | null> {
    const existing = this.#objects.get(key);
    const condition = options.onlyIf;
    if (condition && !(condition instanceof Headers)) {
      if (condition.etagDoesNotMatch === "*" && existing) return null;
      if (condition.etagMatches != null && existing?.sha256 !== condition.etagMatches) {
        return null;
      }
    }
    const streamed = value instanceof ReadableStream;
    if (!streamed || typeof options.sha256 !== "string") {
      throw new Error("test release uploads must stream with a native SHA-256");
    }
    const contentType = options.httpMetadata instanceof Headers
      ? options.httpMetadata.get("content-type")
      : options.httpMetadata?.contentType;
    if (!contentType) throw new Error("test release upload content type is required");
    this.puts.push({ key, sha256: options.sha256, contentType, streamed });
    const bytes = new Uint8Array(await new Response(value).arrayBuffer());
    const actual = createHash("sha256").update(bytes).digest("hex");
    if (actual !== options.sha256) throw new Error("R2 native SHA-256 rejected upload");
    this.#objects.set(key, {
      bytes,
      sha256: actual,
      contentType,
      customMetadata: { ...(options.customMetadata ?? {}) },
    });
    if (this.#failAfterStoreKey === key) {
      this.#failAfterStoreKey = undefined;
      throw new Error("simulated lost R2 acknowledgement");
    }
    return this.#object(key, this.#objects.get(key)!);
  }

  async head(key: string): Promise<R2Object | null> {
    const value = this.#objects.get(key);
    return value ? this.#object(key, value) : null;
  }

  async get(key: string): Promise<R2ObjectBody | null> {
    const value = this.#objects.get(key);
    if (!value) return null;
    const body = new Response(value.bytes.slice()).body!;
    return {
      ...this.#object(key, value),
      body,
      bodyUsed: false,
      arrayBuffer: async () => value.bytes.slice().buffer,
      text: async () => new TextDecoder().decode(value.bytes),
      json: async <T>() => JSON.parse(new TextDecoder().decode(value.bytes)) as T,
      blob: async () => new Blob([value.bytes.slice()]),
    } as R2ObjectBody;
  }

  #object(
    key: string,
    value: {
      bytes: Uint8Array;
      sha256: string;
      contentType: string;
      customMetadata: Record<string, string>;
    },
  ): R2Object {
    return {
      key,
      size: value.bytes.byteLength,
      etag: value.sha256,
      httpEtag: `"${value.sha256}"`,
      uploaded: new Date("2026-08-22T00:00:00.000Z"),
      httpMetadata: { contentType: value.contentType },
      customMetadata: { ...value.customMetadata },
      range: undefined,
      storageClass: "Standard",
      ssecKeyMd5: undefined,
      checksums: {
        sha256: hexBuffer(value.sha256),
        toJSON: () => ({ sha256: value.sha256 }),
      },
      writeHttpMetadata(headers: Headers) {
        headers.set("content-type", value.contentType);
      },
    } as unknown as R2Object;
  }
}

function hexBuffer(value: string): ArrayBuffer {
  const bytes = new Uint8Array(value.length / 2);
  for (let index = 0; index < bytes.length; index += 1) {
    bytes[index] = Number.parseInt(value.slice(index * 2, index * 2 + 2), 16);
  }
  return bytes.buffer;
}
