import assert from "node:assert/strict";
import { test } from "node:test";
import { deflateSync } from "node:zlib";

import { sha1 } from "@noble/hashes/legacy";

import { validateAppGitPush } from "./appGitValidation.ts";

const ZERO_OID = "0000000000000000000000000000000000000000";
const encoder = new TextEncoder();

test("validates initial and direct-fast-forward packs with OFS_DELTA and cross-pack REF_DELTA", () => {
  const fixture = repositoryFixture();

  const initial = validateAppGitPush({
    packs: [fixture.initialPack],
    oldOid: ZERO_OID,
    newOid: fixture.oldCommit.oid,
  });
  assert.equal(initial.packedObjectCount, 3);
  assert.equal(initial.reachableTreeEntries, 1);

  const update = validateAppGitPush({
    packs: [fixture.initialPack, fixture.updatePack],
    oldOid: fixture.oldCommit.oid,
    newOid: fixture.newCommit.oid,
  });
  assert.equal(update.packedObjectCount, 7);
  assert.equal(update.uniqueObjectCount, 6);
  assert.equal(update.reachableTreeEntries, 1);
  assert.ok(update.decompressedBytes > fixture.newCommit.content.byteLength);
});

test("rejects malformed pack checksums and mismatched inflated object sizes", () => {
  const fixture = repositoryFixture();
  const corrupt = fixture.initialPack.slice();
  corrupt[13] ^= 1;
  assert.throws(
    () => validateAppGitPush({ packs: [corrupt], oldOid: ZERO_OID, newOid: fixture.oldCommit.oid }),
    /checksum is invalid/,
  );

  const blob = gitObject("blob", encoder.encode("short"));
  const malformed = makePack([{ kind: "direct", type: "blob", content: blob.content, size: 6 }]);
  assert.throws(
    () => validateAppGitPush({ packs: [malformed], oldOid: ZERO_OID, newOid: blob.oid }),
    /size does not match pack header/,
  );
});

test("rejects arbitrary or non-commit new OIDs", () => {
  const fixture = repositoryFixture();
  assert.throws(
    () => validateAppGitPush({
      packs: [fixture.initialPack],
      oldOid: ZERO_OID,
      newOid: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    }),
    /new OID is absent/,
  );
  assert.throws(
    () => validateAppGitPush({
      packs: [fixture.initialPack],
      oldOid: ZERO_OID,
      newOid: fixture.oldBlob.oid,
    }),
    /does not name a commit/,
  );
});

test("rejects initial commits with parents and updates that are not direct fast-forwards", () => {
  const fixture = repositoryFixture();
  assert.throws(
    () => validateAppGitPush({
      packs: [fixture.initialPack, fixture.updatePack],
      oldOid: ZERO_OID,
      newOid: fixture.newCommit.oid,
    }),
    /initial app commit must have zero parents/,
  );

  const unrelated = gitObject("commit", commitBytes(fixture.newTree.oid, ZERO_OID.replaceAll("0", "f")));
  const pack = makePack([{ kind: "direct", type: "commit", content: unrelated.content }]);
  assert.throws(
    () => validateAppGitPush({
      packs: [fixture.initialPack, fixture.updatePack, pack],
      oldOid: fixture.oldCommit.oid,
      newOid: unrelated.oid,
    }),
    /not a direct fast-forward/,
  );
});

test("recursively requires typed trees and blobs", () => {
  const missingBlob = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
  const tree = gitObject("tree", treeBytes("index.js", missingBlob));
  const commit = gitObject("commit", commitBytes(tree.oid));
  const pack = makePack([
    { kind: "direct", type: "tree", content: tree.content },
    { kind: "direct", type: "commit", content: commit.content },
  ]);
  assert.throws(
    () => validateAppGitPush({ packs: [pack], oldOid: ZERO_OID, newOid: commit.oid }),
    /tree blob is missing/,
  );

  const nestedBlob = gitObject("blob", encoder.encode("export default {};\n"));
  const nestedTree = gitObject("tree", treeBytes("main.js", nestedBlob.oid));
  const rootTree = gitObject("tree", treeBytes("src", nestedTree.oid, "40000"));
  const nestedCommit = gitObject("commit", commitBytes(rootTree.oid));
  const nestedPack = makePack([
    { kind: "direct", type: "blob", content: nestedBlob.content },
    { kind: "direct", type: "tree", content: nestedTree.content },
    { kind: "direct", type: "tree", content: rootTree.content },
    { kind: "direct", type: "commit", content: nestedCommit.content },
  ]);
  assert.equal(validateAppGitPush({
    packs: [nestedPack],
    oldOid: ZERO_OID,
    newOid: nestedCommit.oid,
  }).reachableTreeEntries, 2);
});

test("rejects unsafe deltas and all configured resource bounds", () => {
  const fixture = repositoryFixture();
  const invalidDelta = concat([
    deltaSize(fixture.oldBlob.content.byteLength),
    deltaSize(1),
    new Uint8Array([0x91, 0xff, 0x01]),
  ]);
  const invalidPack = makePack([
    { kind: "direct", type: "blob", content: fixture.oldBlob.content },
    { kind: "ofs", base: 0, delta: invalidDelta },
  ]);
  assert.throws(
    () => validateAppGitPush({ packs: [invalidPack], oldOid: ZERO_OID, newOid: fixture.oldCommit.oid }),
    /delta copy exceeds/,
  );

  assert.throws(
    () => validateAppGitPush({
      packs: [fixture.initialPack, fixture.updatePack],
      oldOid: fixture.oldCommit.oid,
      newOid: fixture.newCommit.oid,
      limits: { maxObjects: 6 },
    }),
    /object count limit/,
  );
  assert.throws(
    () => validateAppGitPush({
      packs: [fixture.initialPack],
      oldOid: ZERO_OID,
      newOid: fixture.oldCommit.oid,
      limits: { maxDecompressedBytes: 1 },
    }),
    /decompressed byte limit/,
  );
  assert.throws(
    () => validateAppGitPush({
      packs: [fixture.initialPack],
      oldOid: ZERO_OID,
      newOid: fixture.oldCommit.oid,
      limits: { maxCompressedBytes: fixture.initialPack.byteLength - 1 },
    }),
    /compressed byte limit/,
  );
});

interface ObjectFixture {
  type: "commit" | "tree" | "blob" | "tag";
  content: Uint8Array;
  oid: string;
}

type PackSpec =
  | { kind: "direct"; type: ObjectFixture["type"]; content: Uint8Array; size?: number }
  | { kind: "ofs"; base: number; delta: Uint8Array }
  | { kind: "ref"; baseOid: string; delta: Uint8Array };

function repositoryFixture() {
  const oldBlob = gitObject("blob", encoder.encode("export default 'old';\n"));
  const oldTree = gitObject("tree", treeBytes("index.js", oldBlob.oid));
  const oldCommit = gitObject("commit", commitBytes(oldTree.oid));
  const initialPack = makePack([
    { kind: "direct", type: "blob", content: oldBlob.content },
    { kind: "direct", type: "tree", content: oldTree.content },
    { kind: "direct", type: "commit", content: oldCommit.content },
  ]);

  const newBlob = gitObject("blob", encoder.encode("export default 'new dynamic app';\n"));
  const newTree = gitObject("tree", treeBytes("index.js", newBlob.oid));
  const newCommit = gitObject("commit", commitBytes(newTree.oid, oldCommit.oid));
  const updatePack = makePack([
    { kind: "direct", type: "blob", content: oldBlob.content },
    { kind: "ofs", base: 0, delta: insertDelta(oldBlob.content, newBlob.content) },
    { kind: "ref", baseOid: oldTree.oid, delta: insertDelta(oldTree.content, newTree.content) },
    { kind: "direct", type: "commit", content: newCommit.content },
  ]);
  return { oldBlob, oldTree, oldCommit, newBlob, newTree, newCommit, initialPack, updatePack };
}

function gitObject(type: ObjectFixture["type"], content: Uint8Array): ObjectFixture {
  const hash = sha1.create();
  hash.update(encoder.encode(`${type} ${content.byteLength}\0`));
  hash.update(content);
  return { type, content, oid: hex(hash.digest()) };
}

function treeBytes(name: string, oid: string, mode = "100644"): Uint8Array {
  return concat([encoder.encode(`${mode} ${name}\0`), fromHex(oid)]);
}

function commitBytes(tree: string, parent?: string): Uint8Array {
  return encoder.encode(
    `tree ${tree}\n${parent == null ? "" : `parent ${parent}\n`}` +
      "author App Builder <app@nanocodex.test> 1700000000 +0000\n" +
      "committer App Builder <app@nanocodex.test> 1700000000 +0000\n\nBuild app\n",
  );
}

function makePack(specs: readonly PackSpec[]): Uint8Array {
  const header = new Uint8Array(12);
  header.set(encoder.encode("PACK"));
  const view = new DataView(header.buffer);
  view.setUint32(4, 2);
  view.setUint32(8, specs.length);
  const chunks: Uint8Array[] = [header];
  const offsets: number[] = [];
  let offset = header.byteLength;
  for (const spec of specs) {
    offsets.push(offset);
    let entry: Uint8Array;
    if (spec.kind === "direct") {
      const type = { commit: 1, tree: 2, blob: 3, tag: 4 }[spec.type];
      entry = concat([
        packObjectHeader(type, spec.size ?? spec.content.byteLength),
        new Uint8Array(deflateSync(spec.content)),
      ]);
    } else if (spec.kind === "ofs") {
      const distance = offset - offsets[spec.base]!;
      entry = concat([
        packObjectHeader(6, spec.delta.byteLength),
        ofsDistance(distance),
        new Uint8Array(deflateSync(spec.delta)),
      ]);
    } else {
      entry = concat([
        packObjectHeader(7, spec.delta.byteLength),
        fromHex(spec.baseOid),
        new Uint8Array(deflateSync(spec.delta)),
      ]);
    }
    chunks.push(entry);
    offset += entry.byteLength;
  }
  const withoutTrailer = concat(chunks);
  return concat([withoutTrailer, sha1(withoutTrailer)]);
}

function packObjectHeader(type: number, size: number): Uint8Array {
  const bytes: number[] = [(type << 4) | (size % 16)];
  let remaining = Math.floor(size / 16);
  while (remaining > 0) {
    bytes[bytes.length - 1]! |= 0x80;
    bytes.push(remaining % 128);
    remaining = Math.floor(remaining / 128);
  }
  return new Uint8Array(bytes);
}

function ofsDistance(distance: number): Uint8Array {
  const bytes = [distance % 128];
  let remaining = Math.floor(distance / 128);
  while (remaining > 0) {
    remaining--;
    bytes.push(0x80 | (remaining % 128));
    remaining = Math.floor(remaining / 128);
  }
  return new Uint8Array(bytes.reverse());
}

function insertDelta(base: Uint8Array, target: Uint8Array): Uint8Array {
  const chunks: Uint8Array[] = [deltaSize(base.byteLength), deltaSize(target.byteLength)];
  for (let offset = 0; offset < target.byteLength;) {
    const length = Math.min(127, target.byteLength - offset);
    chunks.push(new Uint8Array([length]), target.subarray(offset, offset + length));
    offset += length;
  }
  return concat(chunks);
}

function deltaSize(value: number): Uint8Array {
  const bytes: number[] = [];
  do {
    let byte = value % 128;
    value = Math.floor(value / 128);
    if (value > 0) byte |= 0x80;
    bytes.push(byte);
  } while (value > 0);
  return new Uint8Array(bytes);
}

function fromHex(value: string): Uint8Array {
  assert.equal(value.length % 2, 0);
  const result = new Uint8Array(value.length / 2);
  for (let index = 0; index < result.byteLength; index++) {
    result[index] = Number.parseInt(value.slice(index * 2, index * 2 + 2), 16);
  }
  return result;
}

function hex(bytes: Uint8Array): string {
  return [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

function concat(chunks: readonly Uint8Array[]): Uint8Array {
  const length = chunks.reduce((total, chunk) => total + chunk.byteLength, 0);
  const result = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    result.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return result;
}
