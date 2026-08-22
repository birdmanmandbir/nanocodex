import assert from "node:assert/strict";
import { test } from "node:test";

import {
  EXACT_SOURCE_TREE_PATH,
  isCiSourceTree,
  sourceTreeFingerprint,
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
