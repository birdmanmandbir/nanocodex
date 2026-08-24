import { sha1 } from "@noble/hashes/legacy";
import { constants as pakoConstants, Inflate } from "pako";
// Pako's public Inflate wrapper deliberately accepts concatenated zlib streams. Git pack
// objects need the exact first-stream boundary, so use Pako's exported low-level inflater.
// @ts-ignore -- Pako does not publish declarations for its exported low-level module.
import lowLevelInflate from "pako/lib/zlib/inflate.js";

const PACK_HEADER_BYTES = 12;
const PACK_TRAILER_BYTES = 20;
const ZERO_OID = "0000000000000000000000000000000000000000";
const OID_PATTERN = /^[0-9a-f]{40}$/;
const encoder = new TextEncoder();
const { Z_NO_FLUSH, Z_OK, Z_STREAM_END } = pakoConstants;

export interface AppGitValidationLimits {
  maxPacks: number;
  maxCompressedBytes: number;
  maxObjects: number;
  maxObjectBytes: number;
  maxDecompressedBytes: number;
  maxDeltaDepth: number;
  maxTreeEntries: number;
}

export interface AppGitPushCandidate {
  /** Complete packs in repository order, including the newly staged pack. */
  packs: readonly Uint8Array[];
  oldOid: string;
  newOid: string;
  limits?: Partial<AppGitValidationLimits>;
}

export interface ValidatedAppGitPush {
  oldOid: string;
  newOid: string;
  packCount: number;
  packedObjectCount: number;
  uniqueObjectCount: number;
  compressedBytes: number;
  decompressedBytes: number;
  reachableTreeEntries: number;
}

export const DEFAULT_APP_GIT_VALIDATION_LIMITS: Readonly<AppGitValidationLimits> = Object.freeze({
  maxPacks: 128,
  maxCompressedBytes: 64 * 1024 * 1024,
  maxObjects: 100_000,
  maxObjectBytes: 16 * 1024 * 1024,
  maxDecompressedBytes: 128 * 1024 * 1024,
  maxDeltaDepth: 64,
  maxTreeEntries: 100_000,
});

export class AppGitValidationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "AppGitValidationError";
  }
}

type GitObjectType = "commit" | "tree" | "blob" | "tag";

interface GitObject {
  type: GitObjectType;
  content: Uint8Array;
  oid: string;
  depth: number;
}

interface PackEntry {
  packIndex: number;
  offset: number;
  delta: Uint8Array | null;
  directType: GitObjectType | null;
  ofsBase: PackEntry | null;
  refBaseOid: string | null;
  object: GitObject | null;
}

interface PakoInflateInstance {
  strm: {
    input: Uint8Array;
    next_in: number;
    avail_in: number;
    output: Uint8Array;
    next_out: number;
    avail_out: number;
    msg: string;
  };
}

interface LowLevelInflate {
  inflate(stream: PakoInflateInstance["strm"], flush: number): number;
  inflateEnd(stream: PakoInflateInstance["strm"]): number;
}

interface Budget {
  decompressedBytes: number;
}

/**
 * Fully validates the object database represented by `packs` and the proposed ref update.
 * Throws AppGitValidationError before the caller may make the staged pack/ref durable.
 */
export function validateAppGitPush(candidate: AppGitPushCandidate): ValidatedAppGitPush {
  const limits = validationLimits(candidate.limits);
  const oldOid = validateOid(candidate.oldOid, "old OID");
  const newOid = validateOid(candidate.newOid, "new OID");
  if (newOid === ZERO_OID) fail("deleting an app ref is not a valid app push");
  if (candidate.packs.length === 0) fail("app push has no packs");
  if (candidate.packs.length > limits.maxPacks) fail("app push has too many packs");

  let compressedBytes = 0;
  for (const pack of candidate.packs) {
    if (!(pack instanceof Uint8Array)) fail("app push contains a non-byte pack");
    compressedBytes = checkedAdd(compressedBytes, pack.byteLength, "compressed pack bytes");
    if (compressedBytes > limits.maxCompressedBytes) fail("app push exceeds compressed byte limit");
  }

  const budget: Budget = { decompressedBytes: 0 };
  const entries: PackEntry[] = [];
  const objects = new Map<string, GitObject>();
  let packedObjectCount = 0;
  for (let packIndex = 0; packIndex < candidate.packs.length; packIndex++) {
    const parsed = parsePack(
      candidate.packs[packIndex]!,
      packIndex,
      limits.maxObjects - packedObjectCount,
      limits,
      budget,
      objects,
    );
    packedObjectCount = checkedAdd(packedObjectCount, parsed.length, "packed object count");
    if (packedObjectCount > limits.maxObjects) fail("app push exceeds object count limit");
    entries.push(...parsed);
  }

  resolveDeltas(entries, objects, limits, budget);

  const newCommit = objects.get(newOid);
  if (newCommit == null) fail("new OID is absent from the supplied packs");
  if (newCommit.type !== "commit") fail("new OID does not name a commit");
  const commit = parseCommit(newCommit.content);
  if (oldOid === ZERO_OID) {
    if (commit.parents.length !== 0) fail("initial app commit must have zero parents");
  } else {
    const oldCommit = objects.get(oldOid);
    if (oldCommit == null || oldCommit.type !== "commit") {
      fail("old OID does not name a commit in the supplied packs");
    }
    if (commit.parents.length !== 1 || commit.parents[0] !== oldOid) {
      fail("app commit is not a direct fast-forward from old OID");
    }
  }

  const reachableTreeEntries = validateTreeClosure(commit.tree, objects, limits);
  return {
    oldOid,
    newOid,
    packCount: candidate.packs.length,
    packedObjectCount,
    uniqueObjectCount: objects.size,
    compressedBytes,
    decompressedBytes: budget.decompressedBytes,
    reachableTreeEntries,
  };
}

function parsePack(
  pack: Uint8Array,
  packIndex: number,
  remainingObjectCount: number,
  limits: AppGitValidationLimits,
  budget: Budget,
  objects: Map<string, GitObject>,
): PackEntry[] {
  if (pack.byteLength < PACK_HEADER_BYTES + PACK_TRAILER_BYTES) fail(`pack ${packIndex} is truncated`);
  if (pack[0] !== 0x50 || pack[1] !== 0x41 || pack[2] !== 0x43 || pack[3] !== 0x4b) {
    fail(`pack ${packIndex} has an invalid signature`);
  }
  const view = new DataView(pack.buffer, pack.byteOffset, pack.byteLength);
  if (view.getUint32(4) !== 2) fail(`pack ${packIndex} is not pack version 2`);
  const count = view.getUint32(8);
  if (count > remainingObjectCount) fail(`pack ${packIndex} exceeds object count limit`);
  const trailerOffset = pack.byteLength - PACK_TRAILER_BYTES;
  const expectedChecksum = pack.subarray(trailerOffset);
  const actualChecksum = sha1(pack.subarray(0, trailerOffset));
  if (!equalBytes(expectedChecksum, actualChecksum)) fail(`pack ${packIndex} checksum is invalid`);

  const entries: PackEntry[] = [];
  const entriesByOffset = new Map<number, PackEntry>();
  let offset = PACK_HEADER_BYTES;
  for (let objectIndex = 0; objectIndex < count; objectIndex++) {
    const entryOffset = offset;
    const header = parseObjectHeader(pack, offset, trailerOffset, limits.maxObjectBytes);
    offset = header.offset;

    let directType: GitObjectType | null = null;
    let ofsBase: PackEntry | null = null;
    let refBaseOid: string | null = null;
    if (header.type >= 1 && header.type <= 4) {
      directType = (["commit", "tree", "blob", "tag"] as const)[header.type - 1]!;
    } else if (header.type === 6) {
      const decoded = parseOfsDeltaBase(pack, offset, trailerOffset, entryOffset);
      offset = decoded.offset;
      ofsBase = entriesByOffset.get(decoded.baseOffset) ?? null;
      if (ofsBase == null) fail(`pack ${packIndex} OFS_DELTA base is not an object boundary`);
    } else if (header.type === 7) {
      if (offset + 20 > trailerOffset) fail(`pack ${packIndex} REF_DELTA base is truncated`);
      refBaseOid = bytesToHex(pack.subarray(offset, offset + 20));
      offset += 20;
    } else {
      fail(`pack ${packIndex} has unsupported object type ${header.type}`);
    }

    const inflated = inflateOne(pack.subarray(offset, trailerOffset), header.size, limits, budget);
    offset += inflated.consumed;
    const entry: PackEntry = {
      packIndex,
      offset: entryOffset,
      delta: directType == null ? inflated.bytes : null,
      directType,
      ofsBase,
      refBaseOid,
      object: null,
    };
    if (directType != null) {
      entry.object = makeObject(directType, inflated.bytes, 0);
      registerObject(objects, entry.object);
    }
    entries.push(entry);
    entriesByOffset.set(entryOffset, entry);
  }
  if (offset !== trailerOffset) fail(`pack ${packIndex} has bytes outside its declared objects`);
  return entries;
}

function parseObjectHeader(
  pack: Uint8Array,
  start: number,
  end: number,
  maxObjectBytes: number,
): { type: number; size: number; offset: number } {
  if (start >= end) fail("pack object header is truncated");
  let byte = pack[start]!;
  const type = (byte >>> 4) & 7;
  let size = byte & 0x0f;
  let multiplier = 16;
  let offset = start + 1;
  let continuationBytes = 0;
  while ((byte & 0x80) !== 0) {
    if (offset >= end) fail("pack object size is truncated");
    if (++continuationBytes > 8) fail("pack object size is out of range");
    byte = pack[offset++]!;
    size += (byte & 0x7f) * multiplier;
    if (!Number.isSafeInteger(size) || size > maxObjectBytes) fail("pack object exceeds size limit");
    multiplier *= 128;
  }
  if (size > maxObjectBytes) fail("pack object exceeds size limit");
  return { type, size, offset };
}

function parseOfsDeltaBase(
  pack: Uint8Array,
  start: number,
  end: number,
  entryOffset: number,
): { baseOffset: number; offset: number } {
  if (start >= end) fail("OFS_DELTA base is truncated");
  let offset = start;
  let byte = pack[offset++]!;
  let distance = byte & 0x7f;
  let bytes = 1;
  while ((byte & 0x80) !== 0) {
    if (offset >= end) fail("OFS_DELTA base is truncated");
    if (++bytes > 10) fail("OFS_DELTA distance is out of range");
    byte = pack[offset++]!;
    distance = (distance + 1) * 128 + (byte & 0x7f);
    if (!Number.isSafeInteger(distance) || distance > entryOffset) {
      fail("OFS_DELTA distance is out of range");
    }
  }
  if (distance === 0 || distance > entryOffset - PACK_HEADER_BYTES) {
    fail("OFS_DELTA does not point backward into this pack");
  }
  return { baseOffset: entryOffset - distance, offset };
}

function inflateOne(
  compressed: Uint8Array,
  expectedSize: number,
  limits: AppGitValidationLimits,
  budget: Budget,
): { bytes: Uint8Array; consumed: number } {
  const inflater = new Inflate({ windowBits: 15 }) as unknown as PakoInflateInstance;
  const implementation = lowLevelInflate as LowLevelInflate;
  const stream = inflater.strm;
  stream.input = compressed;
  stream.next_in = 0;
  stream.avail_in = compressed.byteLength;
  const chunks: Uint8Array[] = [];
  let length = 0;
  let ended = false;
  try {
    for (;;) {
      const remaining = expectedSize - length;
      stream.output = new Uint8Array(Math.max(1, Math.min(64 * 1024, remaining + 1)));
      stream.next_out = 0;
      stream.avail_out = stream.output.byteLength;
      const status = implementation.inflate(stream, Z_NO_FLUSH);
      if (stream.next_out > 0) {
        length = checkedAdd(length, stream.next_out, "inflated object bytes");
        if (length > expectedSize || length > limits.maxObjectBytes) {
          fail("inflated object exceeds its declared size");
        }
        chunks.push(stream.output.subarray(0, stream.next_out));
      }
      if (status === Z_STREAM_END) {
        ended = true;
        break;
      }
      if (status !== Z_OK) fail(`pack object has invalid zlib data${stream.msg ? `: ${stream.msg}` : ""}`);
      if (stream.avail_in === 0 && stream.avail_out > 0) fail("pack object has truncated zlib data");
    }
  } catch (error) {
    if (error instanceof AppGitValidationError) throw error;
    fail("pack object has invalid zlib data");
  } finally {
    implementation.inflateEnd(stream);
  }
  if (!ended) fail("pack object has truncated zlib data");
  if (length !== expectedSize) fail("inflated object size does not match pack header");
  const consumed = stream.next_in;
  if (!Number.isSafeInteger(consumed) || consumed <= 0 || consumed > compressed.byteLength) {
    fail("pack object has an invalid compressed length");
  }
  chargeDecompressed(budget, length, limits);
  return { bytes: concatenate(chunks, length), consumed };
}

function resolveDeltas(
  entries: readonly PackEntry[],
  objects: Map<string, GitObject>,
  limits: AppGitValidationLimits,
  budget: Budget,
): void {
  let unresolved = entries.filter((entry) => entry.object == null);
  while (unresolved.length > 0) {
    const next: PackEntry[] = [];
    let progress = false;
    for (const entry of unresolved) {
      const base = entry.ofsBase?.object ??
        (entry.refBaseOid == null ? null : objects.get(entry.refBaseOid) ?? null);
      if (base == null) {
        next.push(entry);
        continue;
      }
      const depth = base.depth + 1;
      if (depth > limits.maxDeltaDepth) fail("pack delta chain exceeds depth limit");
      const content = applyGitDelta(base.content, entry.delta!, limits.maxObjectBytes);
      chargeDecompressed(budget, content.byteLength, limits);
      entry.object = makeObject(base.type, content, depth);
      registerObject(objects, entry.object);
      progress = true;
    }
    if (!progress) {
      const first = next[0]!;
      const base = first.refBaseOid ?? `pack ${first.packIndex} offset ${first.ofsBase?.offset ?? first.offset}`;
      fail(`pack delta base is missing or cyclic: ${base}`);
    }
    unresolved = next;
  }
}

function applyGitDelta(base: Uint8Array, delta: Uint8Array, maxObjectBytes: number): Uint8Array {
  let offset = 0;
  const sourceSize = readDeltaSize(delta, offset);
  offset = sourceSize.offset;
  if (sourceSize.value !== base.byteLength) fail("delta source size does not match its base");
  const targetSize = readDeltaSize(delta, offset);
  offset = targetSize.offset;
  if (targetSize.value > maxObjectBytes) fail("delta result exceeds object size limit");
  const result = new Uint8Array(targetSize.value);
  let written = 0;
  while (offset < delta.byteLength) {
    const opcode = delta[offset++]!;
    if ((opcode & 0x80) === 0) {
      if (opcode === 0) fail("delta contains reserved opcode zero");
      if (offset + opcode > delta.byteLength || written + opcode > result.byteLength) {
        fail("delta insert exceeds its input or result");
      }
      result.set(delta.subarray(offset, offset + opcode), written);
      offset += opcode;
      written += opcode;
      continue;
    }

    let copyOffset = 0;
    let copySize = 0;
    for (let byteIndex = 0; byteIndex < 4; byteIndex++) {
      if ((opcode & (1 << byteIndex)) !== 0) {
        if (offset >= delta.byteLength) fail("delta copy offset is truncated");
        copyOffset += delta[offset++]! * 2 ** (8 * byteIndex);
      }
    }
    for (let byteIndex = 0; byteIndex < 3; byteIndex++) {
      if ((opcode & (1 << (4 + byteIndex))) !== 0) {
        if (offset >= delta.byteLength) fail("delta copy size is truncated");
        copySize += delta[offset++]! * 2 ** (8 * byteIndex);
      }
    }
    if (copySize === 0) copySize = 0x10000;
    if (copyOffset + copySize > base.byteLength || written + copySize > result.byteLength) {
      fail("delta copy exceeds its base or result");
    }
    result.set(base.subarray(copyOffset, copyOffset + copySize), written);
    written += copySize;
  }
  if (written !== result.byteLength) fail("delta result size does not match its header");
  return result;
}

function readDeltaSize(bytes: Uint8Array, start: number): { value: number; offset: number } {
  let value = 0;
  let multiplier = 1;
  let offset = start;
  for (let count = 0; count < 8; count++) {
    if (offset >= bytes.byteLength) fail("delta size is truncated");
    const byte = bytes[offset++]!;
    value += (byte & 0x7f) * multiplier;
    if (!Number.isSafeInteger(value)) fail("delta size is out of range");
    if ((byte & 0x80) === 0) return { value, offset };
    multiplier *= 128;
  }
  fail("delta size is out of range");
}

function makeObject(type: GitObjectType, content: Uint8Array, depth: number): GitObject {
  const header = encoder.encode(`${type} ${content.byteLength}\0`);
  const hash = sha1.create();
  hash.update(header);
  hash.update(content);
  return { type, content, oid: bytesToHex(hash.digest()), depth };
}

function registerObject(objects: Map<string, GitObject>, object: GitObject): void {
  const existing = objects.get(object.oid);
  if (existing != null &&
    (existing.type !== object.type || !equalBytes(existing.content, object.content))) {
    fail(`conflicting objects have SHA-1 ${object.oid}`);
  }
  if (existing == null) objects.set(object.oid, object);
}

function parseCommit(content: Uint8Array): { tree: string; parents: string[] } {
  const separator = findSequence(content, 0x0a, 0x0a);
  if (separator < 0) fail("commit has no header terminator");
  const headers = ascii(content.subarray(0, separator)).split("\n");
  let tree: string | null = null;
  const parents: string[] = [];
  for (const header of headers) {
    const treeMatch = /^tree ([0-9a-f]{40})$/.exec(header);
    if (treeMatch != null) {
      if (tree != null) fail("commit has multiple tree headers");
      tree = treeMatch[1]!;
      continue;
    }
    if (header.startsWith("tree ")) fail("commit has an invalid tree header");
    const parentMatch = /^parent ([0-9a-f]{40})$/.exec(header);
    if (parentMatch != null) {
      parents.push(parentMatch[1]!);
      continue;
    }
    if (header.startsWith("parent ")) fail("commit has an invalid parent header");
  }
  if (tree == null) fail("commit has no valid tree header");
  return { tree, parents };
}

function validateTreeClosure(
  rootOid: string,
  objects: ReadonlyMap<string, GitObject>,
  limits: AppGitValidationLimits,
): number {
  const visited = new Set<string>();
  const visiting = new Set<string>();
  let entryCount = 0;
  const visit = (oid: string): void => {
    if (visited.has(oid)) return;
    if (visiting.has(oid)) fail("commit tree contains a cycle");
    const tree = objects.get(oid);
    if (tree == null) fail(`commit tree object is missing: ${oid}`);
    if (tree.type !== "tree") fail(`tree entry does not name a tree: ${oid}`);
    visiting.add(oid);
    let offset = 0;
    while (offset < tree.content.byteLength) {
      const space = findByte(tree.content, 0x20, offset);
      const nul = space < 0 ? -1 : findByte(tree.content, 0, space + 1);
      if (space <= offset || nul <= space + 1 || nul + 21 > tree.content.byteLength) {
        fail(`tree object is malformed: ${oid}`);
      }
      const mode = ascii(tree.content.subarray(offset, space));
      const name = tree.content.subarray(space + 1, nul);
      if (hasByte(name, 0x2f) || isDotName(name)) fail(`tree object has an invalid name: ${oid}`);
      const childOid = bytesToHex(tree.content.subarray(nul + 1, nul + 21));
      entryCount++;
      if (entryCount > limits.maxTreeEntries) fail("commit tree exceeds entry count limit");
      const child = objects.get(childOid);
      if (mode === "40000") {
        if (child == null || child.type !== "tree") fail(`tree object is missing: ${childOid}`);
        visit(childOid);
      } else if (mode === "100644" || mode === "100755" || mode === "120000") {
        if (child == null || child.type !== "blob") fail(`tree blob is missing: ${childOid}`);
      } else {
        fail(`tree object has unsupported mode ${mode}`);
      }
      offset = nul + 21;
    }
    visiting.delete(oid);
    visited.add(oid);
  };
  visit(rootOid);
  return entryCount;
}

function validationLimits(overrides: Partial<AppGitValidationLimits> | undefined): AppGitValidationLimits {
  const limits = { ...DEFAULT_APP_GIT_VALIDATION_LIMITS, ...overrides };
  for (const [name, value] of Object.entries(limits)) {
    const permitsZero = name === "maxDeltaDepth" || name === "maxTreeEntries";
    if (!Number.isSafeInteger(value) || value < (permitsZero ? 0 : 1)) {
      fail(`invalid app Git validation limit: ${name}`);
    }
  }
  return limits;
}

function validateOid(value: string, label: string): string {
  if (!OID_PATTERN.test(value)) fail(`${label} is not a canonical SHA-1`);
  return value;
}

function chargeDecompressed(budget: Budget, bytes: number, limits: AppGitValidationLimits): void {
  budget.decompressedBytes = checkedAdd(budget.decompressedBytes, bytes, "decompressed bytes");
  if (budget.decompressedBytes > limits.maxDecompressedBytes) {
    fail("app push exceeds decompressed byte limit");
  }
}

function checkedAdd(left: number, right: number, label: string): number {
  const result = left + right;
  if (!Number.isSafeInteger(result)) fail(`${label} are out of range`);
  return result;
}

function concatenate(chunks: readonly Uint8Array[], length: number): Uint8Array {
  const result = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    result.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return result;
}

function ascii(bytes: Uint8Array): string {
  let result = "";
  for (const byte of bytes) {
    if (byte > 0x7f) fail("Git object header contains non-ASCII bytes");
    result += String.fromCharCode(byte);
  }
  return result;
}

function findByte(bytes: Uint8Array, wanted: number, start: number): number {
  for (let index = start; index < bytes.byteLength; index++) {
    if (bytes[index] === wanted) return index;
  }
  return -1;
}

function findSequence(bytes: Uint8Array, first: number, second: number): number {
  for (let index = 0; index + 1 < bytes.byteLength; index++) {
    if (bytes[index] === first && bytes[index + 1] === second) return index;
  }
  return -1;
}

function hasByte(bytes: Uint8Array, wanted: number): boolean {
  return findByte(bytes, wanted, 0) >= 0;
}

function isDotName(bytes: Uint8Array): boolean {
  return bytes.byteLength === 1 && bytes[0] === 0x2e ||
    bytes.byteLength === 2 && bytes[0] === 0x2e && bytes[1] === 0x2e;
}

function equalBytes(left: Uint8Array, right: Uint8Array): boolean {
  if (left.byteLength !== right.byteLength) return false;
  let difference = 0;
  for (let index = 0; index < left.byteLength; index++) difference |= left[index]! ^ right[index]!;
  return difference === 0;
}

function bytesToHex(bytes: Uint8Array): string {
  let result = "";
  for (const byte of bytes) result += byte.toString(16).padStart(2, "0");
  return result;
}

function fail(message: string): never {
  throw new AppGitValidationError(message);
}
