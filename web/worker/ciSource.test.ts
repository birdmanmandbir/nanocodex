import assert from "node:assert/strict";
import { test } from "node:test";

import {
  cargoVendorBundleKey,
  ciSourceLane,
  EXACT_SOURCE_TREE_PATH,
  isCiSourcePublication,
  isCiSourceLane,
  isCiSourceTree,
  isNanocodexCiProviderData,
  normalizeCiSourcePublication,
  sourceTreeFingerprint,
  type CiSourcePublication,
  type CiSourceTree,
} from "./ciSource.ts";

const head = "1".repeat(40);
const tree: CiSourceTree = {
  version: 1,
  head,
  archive: { size: 1, sha256: "2".repeat(64) },
  files: [
    { path: "Cargo.toml", sha: "3".repeat(40), mode: "100644" },
    { path: "src/lib.rs", sha: "4".repeat(40), mode: "100644" },
  ],
};

test("the exact-source cache fingerprint is the committed file tree, not the commit", async () => {
  const changedCommit = {
    ...tree,
    head: "5".repeat(40),
    archive: { size: 9, sha256: "6".repeat(64) },
  };
  assert.equal(
    await sourceTreeFingerprint(tree),
    await sourceTreeFingerprint(changedCommit),
  );

  for (const file of [
    { path: "Cargo.lock", sha: "3".repeat(40), mode: "100644" as const },
    { path: "Cargo.toml", sha: "7".repeat(40), mode: "100644" as const },
    { path: "Cargo.toml", sha: "3".repeat(40), mode: "100755" as const },
  ]) {
    assert.notEqual(
      await sourceTreeFingerprint(tree),
      await sourceTreeFingerprint({ ...tree, files: [file, tree.files[1]!] }),
    );
  }
});

test("the synthetic exact-source cache path cannot collide with published source", () => {
  assert.equal(isCiSourceTree({
    ...tree,
    files: [{ path: EXACT_SOURCE_TREE_PATH, sha: "3".repeat(40), mode: "100644" }],
  }), false);
});

test("source publications encode canonical master and pull request merge lanes", () => {
  const master = sourcePublication();
  assert.equal(isCiSourcePublication(master), true);
  assert.deepEqual(normalizeCiSourcePublication(master as CiSourcePublication).lane, {
    type: "master",
  });
  assert.deepEqual(ciSourceLane(master as CiSourcePublication), {
    type: "master",
    deployable: true,
    branch: "master",
    ref: "refs/heads/master",
    head,
  });
  assert.equal(isCiSourceLane(ciSourceLane(master as CiSourcePublication), head), true);

  const pullRequest = {
    ...master,
    branch: "pull/42/merge",
    ref: "refs/pull/42/merge",
    lane: {
      type: "pull_request",
      number: 42,
      pullRequestHead: "7".repeat(40),
    },
  };
  assert.equal(isCiSourcePublication(pullRequest), true);
  assert.deepEqual(ciSourceLane(pullRequest as CiSourcePublication), {
    type: "pull_request",
    deployable: false,
    number: 42,
    branch: "pull/42/merge",
    ref: "refs/pull/42/merge",
    mergeHead: head,
    pullRequestHead: "7".repeat(40),
  });
  const pullRequestLane = ciSourceLane(pullRequest as CiSourcePublication);
  assert.equal(isCiSourceLane(pullRequestLane, head), true);
  assert.equal(isCiSourceLane({ ...pullRequestLane, deployable: true }, head), false);

  for (const invalid of [
    { ...pullRequest, branch: "pull/41/merge" },
    { ...pullRequest, ref: "refs/pull/042/merge" },
    { ...pullRequest, lane: { ...pullRequest.lane, number: 0 } },
    { ...pullRequest, lane: { ...pullRequest.lane, pullRequestHead: "not-a-sha" } },
    { ...master, lane: { type: "master", number: 42 } },
    {
      ...master,
      lane: {
        type: "pull_request",
        number: 42,
        pullRequestHead: "7".repeat(40),
      },
    },
  ]) assert.equal(isCiSourcePublication(invalid), false);
});

test("Workflow provider data validates exact lane identity and master-only distribution", () => {
  const common = providerData();
  const master = {
    ...common,
    lane: {
      type: "master",
      deployable: true,
      branch: "master",
      ref: "refs/heads/master",
      head,
    },
  };
  assert.equal(isNanocodexCiProviderData(master, head), true);
  assert.equal(isNanocodexCiProviderData({
    ...master,
    distribution: {
      version: 1,
      channel: "stable",
      tagName: "v1.2.3",
      buildTimestamp: "2026-08-22T01:02:03.000Z",
    },
  }, head), true);

  const pullRequest = {
    ...common,
    lane: {
      type: "pull_request",
      deployable: false,
      number: 42,
      branch: "pull/42/merge",
      ref: "refs/pull/42/merge",
      mergeHead: head,
      pullRequestHead: "7".repeat(40),
    },
  };
  assert.equal(isNanocodexCiProviderData(pullRequest, head), true);
  assert.equal(isNanocodexCiProviderData({
    ...pullRequest,
    distribution: {
      version: 1,
      channel: "nightly",
      tagName: "nightly",
      buildTimestamp: "2026-08-22T01:02:03.000Z",
    },
  }, head), false);
  assert.equal(isNanocodexCiProviderData({
    ...master,
    lane: { ...master.lane, head: "0".repeat(40) },
  }, head), false);
  assert.equal(isNanocodexCiProviderData({ ...master, unexpected: true }, head), false);
  assert.equal(isNanocodexCiProviderData({
    ...master,
    cargoVendorSha256: "e".repeat(64),
  }, head), false);
  assert.equal(isNanocodexCiProviderData({
    ...master,
    cargoVendorKey: cargoVendorBundleKey(master.cargoLockBlob, "e".repeat(64)),
  }, head), false);
});

test("Cargo vendor identity binds the lock blob and exact bundle hash", () => {
  const first = sourcePublication();
  const secondSha = "e".repeat(64);
  const second = {
    ...first,
    cargoVendor: {
      ...first.cargoVendor,
      key: cargoVendorBundleKey(first.cargoLockBlob, secondSha),
      sha256: secondSha,
    },
  };
  assert.equal(isCiSourcePublication(first), true);
  assert.equal(isCiSourcePublication(second), true);
  assert.notEqual(first.cargoVendor.key, second.cargoVendor.key);
  assert.equal(isCiSourcePublication({
    ...second,
    cargoVendor: { ...second.cargoVendor, key: first.cargoVendor.key },
  }), false);
  assert.equal(isCiSourcePublication({
    ...second,
    cargoVendor: {
      ...second.cargoVendor,
      key: `cargo-vendor/${first.cargoLockBlob}/bundle.tar.gz`,
    },
  }), false);
  for (const cargoVendor of [undefined, null, "bundle", {}]) {
    assert.equal(isCiSourcePublication({ ...first, cargoVendor }), false);
  }
});

function sourcePublication() {
  return {
    version: 1,
    head,
    branch: "master",
    ref: "refs/heads/master",
    archive: {
      key: `sources/${head}/source.tar.gz`,
      size: 123,
      sha256: "8".repeat(64),
    },
    tree: {
      key: `sources/${head}/tree.json`,
      size: 456,
      sha256: "9".repeat(64),
    },
    cargoLockBlob: "a".repeat(40),
    cargoVendor: {
      key: cargoVendorBundleKey("a".repeat(40), "b".repeat(64)),
      size: 789,
      sha256: "b".repeat(64),
    },
    rustSecRevision: "c".repeat(40),
    rustSec: {
      key: `rustsec-advisory-db/${"c".repeat(40)}/bundle.tar.gz`,
      size: 321,
      sha256: "d".repeat(64),
    },
    publishedAt: "2026-08-22T01:02:03.000Z",
  };
}

function providerData() {
  return {
    archiveKey: `sources/${head}/source.tar.gz`,
    archiveSize: 123,
    archiveSha256: "8".repeat(64),
    treeKey: `sources/${head}/tree.json`,
    treeSha256: "9".repeat(64),
    cargoLockBlob: "a".repeat(40),
    cargoVendorKey: cargoVendorBundleKey("a".repeat(40), "b".repeat(64)),
    cargoVendorSize: 789,
    cargoVendorSha256: "b".repeat(64),
    rustSecRevision: "c".repeat(40),
    rustSecKey: `rustsec-advisory-db/${"c".repeat(40)}/bundle.tar.gz`,
    rustSecSize: 321,
    rustSecSha256: "d".repeat(64),
    publishedAt: "2026-08-22T01:02:03.000Z",
  };
}
