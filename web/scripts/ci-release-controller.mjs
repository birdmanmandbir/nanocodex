#!/usr/bin/env node

import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { constants as fsConstants } from "node:fs";
import {
  lstat,
  link,
  mkdir,
  mkdtemp,
  open,
  rename,
  rm,
  stat,
  unlink,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";

import {
  acquireExclusiveLock,
  authoritativeRepositoryUrl,
  parseCiRunState,
  parseMasterSourceState,
  redactSecrets,
  runtimeEnvironment,
} from "./ci-controller.mjs";

const scriptPath = fileURLToPath(import.meta.url);
const webDirectory = resolve(dirname(scriptPath), "..");
const defaultRepository = resolve(webDirectory, "..");

const SHA1 = /^[a-f0-9]{40}$/;
const SHA256 = /^[a-f0-9]{64}$/;
const stableTagPattern = /^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/;
const uuidV4Pattern = /^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;
const maximumJsonBytes = 1024 * 1024;
const maximumErrorBytes = 4 * 1024;
const maximumStructuredErrorBytes = 16 * 1024;
const maximumStructuredErrorNodes = 32;
const maximumProcessOutputBytes = 16 * 1024 * 1024;
const maximumNpmBytes = 16 * 1024 * 1024;
const maximumNpmExpandedBytes = 256 * 1024 * 1024;
const maximumTarEntries = 50_000;
const maximumCrateBytes = 64 * 1024 * 1024;
const crateReadChunkBytes = 64 * 1024;
const npmRegistryOrigin = "https://registry.npmjs.org";
const cratesRegistryOrigin = "https://crates.io";
const retryableHttpStatuses = new Set([408, 425, 429]);
const responseAttemptCounts = new WeakMap();
const maximumHttpAttempts = 5;
const maximumRetryDelayMs = 30_000;
const maximumToolchainVersionBytes = 16 * 1024;

export const releaseCrateNames = Object.freeze([
  "nanocodex-oai-api",
  "nanocodex-tools-macros",
  "nanocodex-observability",
  "nanocodex-tools",
  "nanocodex-agent",
  "nanocodex-durability",
  "nanocodex-subagents",
  "nanocodex",
]);

export const releaseProvenanceNotice = Object.freeze({
  npmRegistryProvenance: {
    status: "unavailable",
    reason:
      "This Cloudflare release controller has no supported npm OIDC trusted-publisher provenance path; token publication is not registry-attested.",
  },
  r2Provenance: {
    asset: "PROVENANCE.json",
    meaning: "Cloudflare builder metadata only; it is not an npm registry attestation.",
  },
});

export class TagMovedError extends Error {
  constructor(tag, phase, expected, observed) {
    super(`authoritative tag ${tag} moved during ${phase}`);
    this.name = "TagMovedError";
    this.tag = tag;
    this.phase = phase;
    this.expected = expected;
    this.observed = observed;
  }
}

export class ReleaseHttpError extends Error {
  constructor(operation, {
    status,
    attempts = 1,
    retryable = false,
    retryAfterMs,
    detail,
    cause,
  } = {}) {
    super(
      `${operation} failed${status == null ? "" : ` with HTTP ${status}`}` +
        `${detail ? `: ${detail}` : ""}`,
      cause === undefined ? undefined : { cause },
    );
    this.name = "ReleaseHttpError";
    this.operation = operation;
    this.status = status;
    this.attempts = attempts;
    this.retryable = retryable;
    this.retryAfterMs = retryAfterMs;
  }
}

export class PublicationLeaseLostError extends Error {
  constructor(phase, options) {
    super(`stable publication authority was lost during ${phase}`, options);
    this.name = "PublicationLeaseLostError";
    this.phase = phase;
  }
}

export class ReviewedReleasePlanMismatchError extends Error {
  constructor(message, options) {
    super(message, options);
    this.name = "ReviewedReleasePlanMismatchError";
  }
}

export class ReleaseValidationError extends Error {
  constructor(message, options) {
    super(message, options);
    this.name = "ReleaseValidationError";
  }
}

class CrateRepackageMismatchError extends Error {
  constructor(crate, version, phase, expected, observed, options) {
    super(
      `Cargo ${phase} bytes changed for ${crate}@${version}: ` +
        `expected ${expected}, observed ${observed}`,
      options,
    );
    this.name = "CrateRepackageMismatchError";
  }
}

export function parseReleaseArguments(args) {
  if (!Array.isArray(args)) throw new TypeError("release arguments must be an array");
  if (args.length === 1 && (args[0] === "--help" || args[0] === "-h")) {
    return { help: true };
  }
  if (args.length !== 2 || !["stage", "publish"].includes(args[0])) {
    throw new Error(
      "usage: ci-release-controller.mjs <stage|publish> vMAJOR.MINOR.PATCH",
    );
  }
  return { help: false, command: args[0], ...parseStableTag(args[1]) };
}

export function parseStableTag(value) {
  if (typeof value !== "string" || value.length > 64) {
    throw new Error("stable release tag must be canonical vMAJOR.MINOR.PATCH");
  }
  const match = stableTagPattern.exec(value);
  if (!match) throw new Error("stable release tag must be canonical vMAJOR.MINOR.PATCH");
  const numbers = match.slice(1).map(Number);
  if (!numbers.every(Number.isSafeInteger)) {
    throw new Error("stable release version components must be safe integers");
  }
  return {
    tag: value,
    version: value.slice(1),
    major: numbers[0],
    minor: numbers[1],
    patch: numbers[2],
  };
}

/** Parse `git ls-remote <url> refs/tags/TAG refs/tags/TAG^{}` exactly. */
export function parseRemoteTagRefs(output, tag) {
  parseStableTag(tag);
  if (typeof output !== "string") throw new TypeError("remote tag refs must be text");
  const directRef = `refs/tags/${tag}`;
  const peeledRef = `${directRef}^{}`;
  const refs = new Map();
  for (const line of output.trim().split("\n").filter(Boolean)) {
    const match = /^([a-f0-9]{40})\t([^\t\r\n]+)$/.exec(line);
    if (
      !match || ![directRef, peeledRef].includes(match[2]) || refs.has(match[2])
    ) throw new Error(`authoritative remote returned unexpected refs for ${tag}`);
    refs.set(match[2], match[1]);
  }
  const object = refs.get(directRef);
  if (!object) throw new Error(`authoritative remote has no tag ${tag}`);
  const peeled = refs.get(peeledRef);
  return Object.freeze({
    tag,
    ref: directRef,
    object,
    commit: peeled ?? object,
    annotated: peeled != null,
  });
}

export function assertTagBinding(expected, observed, phase = "tag revalidation") {
  if (!isTagBinding(expected) || !isTagBinding(observed) || expected.tag !== observed.tag) {
    throw new Error("tag bindings are invalid or refer to different tags");
  }
  if (
    expected.ref !== observed.ref || expected.object !== observed.object ||
    expected.commit !== observed.commit || expected.annotated !== observed.annotated
  ) throw new TagMovedError(expected.tag, phase, expected, observed);
  return expected;
}

export function assertReleaseCheckoutState(state, binding) {
  if (!isRecord(state) || !isTagBinding(binding)) {
    throw new Error("release checkout state is invalid");
  }
  if (state.ref !== "HEAD") {
    throw new Error("stable release requires a detached checkout of the exact tag");
  }
  if (state.status !== "") {
    throw new Error("stable release requires a clean tracked and untracked checkout");
  }
  for (const [name, value] of [
    ["checkout HEAD", state.head],
    ["local tag object", state.tagObject],
    ["local tag commit", state.tagCommit],
  ]) assertSha1(value, name);
  if (
    state.head !== binding.commit || state.tagObject !== binding.object ||
    state.tagCommit !== binding.commit
  ) throw new Error("local checkout is not the exact authoritative remote tag");
  return binding.commit;
}

export function parseGreenMasterEvidence(masterValue, runValue, head) {
  assertSha1(head, "release head");
  const master = parseMasterSourceState(masterValue, head);
  const run = parseCiRunState(runValue, head);
  if (run.outcome !== "success") {
    throw new Error(`tag commit ${head} does not have a retained green Cloudflare run`);
  }
  if (
    master.run.workflowId !== run.value.workflowId ||
    master.run.state !== run.value.state ||
    master.run.version !== run.value.version ||
    master.run.head !== run.value.head
  ) throw new Error("retained master publication and green run records do not agree");
  return Object.freeze({ master, run });
}

export function selectTestedNpmArtifact(runState, tag) {
  const { version } = parseStableTag(tag);
  if (
    !isRecord(runState) || runState.outcome !== "success" ||
    !isRecord(runState.result) || !Array.isArray(runState.result.artifacts)
  ) throw new Error("green Cloudflare run has no tested npm artifact declaration");
  assertSha1(runState.head, "tested npm artifact head");
  const key = `runs/${runState.head}/artifacts/npm-package.tgz`;
  const matches = runState.result.artifacts.filter(
    (asset) => isRecord(asset) && asset.key === key,
  );
  if (matches.length !== 1) {
    throw new Error("green Cloudflare run must declare exactly one tested npm artifact");
  }
  const asset = matches[0];
  if (
    !Number.isSafeInteger(asset.size) || asset.size <= 0 || asset.size > maximumNpmBytes ||
    typeof asset.sha256 !== "string" || !SHA256.test(asset.sha256) ||
    asset.contentType !== "application/gzip"
  ) throw new Error("tested npm artifact declaration is invalid");
  return Object.freeze({
    name: `nanocodex-${version}.tgz`,
    platform: "npm",
    key,
    size: asset.size,
    sha256: asset.sha256,
    contentType: asset.contentType,
  });
}

export function validateNpmArtifactHeaders(headers, artifact, head) {
  if (!(headers instanceof Headers)) throw new TypeError("npm artifact headers must be Headers");
  assertSha1(head, "npm artifact head");
  const size = parseCanonicalInteger(headers.get("content-length"));
  if (
    size !== artifact.size || headers.get("content-type") !== artifact.contentType ||
    headers.get("x-nanocodex-sha256") !== artifact.sha256 ||
    headers.get("content-disposition") !==
      `attachment; filename="nanocodex-${head}-npm-package.tgz"` ||
    headers.get("x-content-type-options") !== "nosniff" ||
    !/(?:^|,)\s*immutable(?:,|$)/.test(headers.get("cache-control") ?? "") ||
    headers.has("content-encoding") || headers.has("content-range")
  ) throw new Error("tested npm artifact response headers do not match its declaration");
  return artifact;
}

export function verifyNpmPackageBytes(bytes, artifact, tag) {
  const { version } = parseStableTag(tag);
  if (!Buffer.isBuffer(bytes) || bytes.byteLength !== artifact.size) {
    throw new Error(`tested npm artifact body must be exactly ${artifact.size} bytes`);
  }
  const sha256 = createHash("sha256").update(bytes).digest("hex");
  if (sha256 !== artifact.sha256) {
    throw new Error("tested npm artifact SHA-256 does not match its declaration");
  }
  const packageJson = inspectNpmPackage(bytes);
  if (
    packageJson.name !== "nanocodex" || packageJson.version !== version ||
    packageJson.private === true ||
    !isRecord(packageJson.publishConfig) ||
    packageJson.publishConfig.access !== "public" ||
    packageJson.publishConfig.provenance !== false
  ) throw new Error(`tested npm artifact is not publishable nanocodex@${version}`);
  return Object.freeze({
    name: packageJson.name,
    version: packageJson.version,
    size: bytes.byteLength,
    sha256,
    sha1: createHash("sha1").update(bytes).digest("hex"),
    integrity: `sha512-${createHash("sha512").update(bytes).digest("base64")}`,
    packageJsonSha256: packageJson.sha256,
  });
}

export function inspectNpmPackage(bytes) {
  if (!Buffer.isBuffer(bytes) || bytes.byteLength === 0 || bytes.byteLength > maximumNpmBytes) {
    throw new Error("npm package has an invalid compressed size");
  }
  if (bytes[0] !== 0x1f || bytes[1] !== 0x8b) throw new Error("npm package is not gzip data");
  let archive;
  try {
    archive = gunzipSync(bytes, { maxOutputLength: maximumNpmExpandedBytes });
  } catch (cause) {
    throw new Error("npm package gzip body is invalid or expands beyond its limit", { cause });
  }
  const packageJsonBodies = [];
  inspectNpmTar(archive, (path, body) => {
    if (path === "package/package.json") packageJsonBodies.push(Buffer.from(body));
  });
  if (packageJsonBodies.length !== 1) {
    throw new Error("npm package must contain exactly one package/package.json");
  }
  const body = packageJsonBodies[0];
  if (body.byteLength === 0 || body.byteLength > maximumJsonBytes) {
    throw new Error("npm package package.json has an invalid size");
  }
  let value;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(body));
  } catch (cause) {
    throw new Error("npm package package.json is invalid", { cause });
  }
  if (!isRecord(value) || typeof value.name !== "string" || typeof value.version !== "string") {
    throw new Error("npm package package.json has no package identity");
  }
  return {
    ...value,
    sha256: createHash("sha256").update(body).digest("hex"),
  };
}

export function parseReadyDistribution(value, tag, head, testedNpm) {
  parseStableTag(tag);
  assertSha1(head, "stable distribution head");
  if (
    !isRecord(value) || value.version !== 1 ||
    !["ready", "success"].includes(value.status) || value.channel !== "stable" ||
    value.tagName !== tag || value.head !== head ||
    value.workflowId !== `release-${tag}-${head}` ||
    !Number.isSafeInteger(value.durationMs) || value.durationMs < 0 ||
    typeof value.completedAt !== "string" || !Number.isFinite(Date.parse(value.completedAt)) ||
    !isRecord(value.staged) || !isRecord(value.staged.draft)
  ) throw new Error("stable distribution evidence is not a ready exact-tag draft");
  const draft = value.staged.draft;
  if (
    draft.version !== 1 || draft.kind !== "stable" || draft.id !== tag ||
    draft.tag !== tag || draft.commit !== head || draft.channel !== "latest" ||
    !(draft.expectedChannel === null || isCanonicalPriorTag(draft.expectedChannel, tag)) ||
    typeof draft.createdAt !== "string" || !Number.isFinite(Date.parse(draft.createdAt)) ||
    !Array.isArray(draft.assets)
  ) throw new Error("staged stable release draft has the wrong immutable identity");
  const assets = validateStableAssets(draft.assets, tag, head, testedNpm);
  return Object.freeze({
    status: value.status,
    tag,
    head,
    workflowId: value.workflowId,
    completedAt: value.completedAt,
    draft: Object.freeze({ ...draft, assets }),
    evidence: value,
    npm: assets.find((asset) => asset.platform === "npm"),
  });
}

export function parseAcceptedStableRelease(value, tag, head) {
  parseStableTag(tag);
  assertSha1(head, "accepted stable release head");
  if (
    !isRecord(value) || value.status !== "accepted" || value.channel !== "stable" ||
    value.tagName !== tag || value.head !== head ||
    value.workflowId !== `release-${tag}-${head}` ||
    typeof value.requestId !== "string" || !uuidV4Pattern.test(value.requestId)
  ) throw new Error("stage stable release accepted an invalid attempt identity");
  return Object.freeze({
    status: value.status,
    channel: value.channel,
    tagName: value.tagName,
    head: value.head,
    workflowId: value.workflowId,
    requestId: value.requestId,
  });
}

export function parseRunningStableRelease(value, tag, head, expectedRequestId) {
  parseStableTag(tag);
  assertSha1(head, "running stable release head");
  if (
    !isRecord(value) || value.version !== 1 || value.status !== "running" ||
    value.channel !== "stable" || value.tagName !== tag || value.head !== head ||
    value.workflowId !== `release-${tag}-${head}`
  ) throw new Error("stable distribution running evidence targets the wrong release");
  if (
    value.requestId !== undefined &&
    (typeof value.requestId !== "string" || !uuidV4Pattern.test(value.requestId) ||
      (expectedRequestId !== undefined && value.requestId !== expectedRequestId))
  ) throw new Error("stable distribution running evidence has the wrong attempt identity");
  return Object.freeze({ status: value.status, requestId: value.requestId });
}

export function validateStableAssets(assets, tag, head, testedNpm) {
  parseStableTag(tag);
  assertSha1(head, "stable asset head");
  const specifications = stableAssetSpecifications(tag);
  if (assets.length !== specifications.length) {
    throw new Error(`stable release must stage exactly ${specifications.length} assets`);
  }
  const normalized = [];
  for (let index = 0; index < specifications.length; index += 1) {
    const expected = specifications[index];
    const asset = assets[index];
    if (
      !isRecord(asset) || asset.name !== expected.name || asset.platform !== expected.platform ||
      asset.key !== expected.key || asset.contentType !== expected.contentType ||
      !Number.isSafeInteger(asset.size) || asset.size <= 0 || asset.size > expected.maxBytes ||
      typeof asset.sha256 !== "string" || !SHA256.test(asset.sha256)
    ) throw new Error(`staged release asset ${expected.name} has the wrong identity`);
    normalized.push(Object.freeze({
      name: asset.name,
      platform: asset.platform,
      key: asset.key,
      size: asset.size,
      sha256: asset.sha256,
      contentType: asset.contentType,
    }));
  }
  const npm = normalized.find((asset) => asset.platform === "npm");
  if (
    !npm || !testedNpm || testedNpm.platform !== "npm" ||
    testedNpm.key !== `runs/${head}/artifacts/npm-package.tgz` ||
    npm.name !== testedNpm.name ||
    npm.size !== testedNpm.size || npm.sha256 !== testedNpm.sha256 ||
    npm.contentType !== testedNpm.contentType
  ) throw new Error("staged npm tarball is not the exact artifact from the green master run");
  return Object.freeze(normalized);
}

function stableAssetSpecifications(tag) {
  const { version } = parseStableTag(tag);
  const releasePrefix = `distribution/stable/${tag}`;
  const linuxPrefix = `distribution/stable/${tag}/components/linux`;
  const npmName = `nanocodex-${version}.tgz`;
  return [
    assetSpec("PROVENANCE.json", "linux", `${releasePrefix}/PROVENANCE.json`, 64 * 1024, "application/json"),
    assetSpec("SHA256SUMS", "linux", `${releasePrefix}/SHA256SUMS`, 64 * 1024, "text/plain; charset=utf-8"),
    assetSpec(npmName, "npm", `${releasePrefix}/components/npm/${npmName}`, maximumNpmBytes, "application/gzip"),
    assetSpec("nanocodex-aarch64-apple-darwin", "aarch64-apple-darwin", `${releasePrefix}/components/macos/nanocodex-aarch64-apple-darwin`, 128 * 1024 * 1024, "application/octet-stream"),
    assetSpec("nanocodex-vm-guest-x86_64-unknown-linux-musl", "x86_64-unknown-linux-musl", `${linuxPrefix}/nanocodex-vm-guest-x86_64-unknown-linux-musl`, 64 * 1024 * 1024, "application/octet-stream"),
    assetSpec("nanocodex-vm-guest-x86_64-unknown-linux-musl.gz", "x86_64-unknown-linux-musl", `${linuxPrefix}/nanocodex-vm-guest-x86_64-unknown-linux-musl.gz`, 64 * 1024 * 1024, "application/gzip"),
    assetSpec("nanocodex-x86_64-unknown-linux-gnu", "x86_64-unknown-linux-gnu", `${linuxPrefix}/nanocodex-x86_64-unknown-linux-gnu`, 128 * 1024 * 1024, "application/octet-stream"),
    assetSpec("nanocodex-x86_64-unknown-linux-gnu.gz", "x86_64-unknown-linux-gnu", `${linuxPrefix}/nanocodex-x86_64-unknown-linux-gnu.gz`, 128 * 1024 * 1024, "application/gzip"),
  ].sort((left, right) => left.name.localeCompare(right.name));
}

function stableReleaseObjects(tag) {
  return {
    prefix: `distribution/stable/${tag}/`,
    assets: stableAssetSpecifications(tag).map(({ name, key }) => ({ name, key })),
  };
}

export function parsePublicStableManifest(value, ready) {
  if (!isRecord(value) || !ready || !isRecord(ready.draft)) {
    throw new Error("public stable manifest is invalid");
  }
  const draft = ready.draft;
  if (
    value.version !== 1 || value.kind !== "stable" || value.id !== ready.tag ||
    value.tag !== ready.tag || value.commit !== ready.head || value.channel !== "latest" ||
    typeof value.finalizedAt !== "string" || !Number.isFinite(Date.parse(value.finalizedAt)) ||
    typeof value.manifestSha256 !== "string" || !SHA256.test(value.manifestSha256) ||
    !Array.isArray(value.assets) || value.assets.length !== draft.assets.length
  ) throw new Error("public stable manifest has the wrong immutable identity");
  const expectedAssets = draft.assets.map(({ key: _key, ...asset }) => ({
    ...asset,
    downloadPath: `/api/releases/releases/stable/${ready.tag}/assets/${encodeURIComponent(asset.name)}`,
  }));
  if (canonicalJson(value.assets) !== canonicalJson(expectedAssets)) {
    throw new Error("public stable manifest assets do not match the reviewed draft");
  }
  const unsigned = {
    version: value.version,
    kind: value.kind,
    id: value.id,
    tag: value.tag,
    commit: value.commit,
    channel: value.channel,
    finalizedAt: value.finalizedAt,
    assets: value.assets,
  };
  const digest = createHash("sha256").update(canonicalJson(unsigned)).digest("hex");
  if (digest !== value.manifestSha256) {
    throw new Error("public stable manifest canonical SHA-256 is invalid");
  }
  return Object.freeze({ ...value, assets: Object.freeze(value.assets.map(Object.freeze)) });
}

export function parseLatestStable(value, manifest) {
  if (!isRecord(value) || !isRecord(value.pointer) || !isRecord(value.manifest)) {
    throw new Error("latest release channel is invalid");
  }
  const pointer = value.pointer;
  if (
    pointer.version !== 1 || pointer.channel !== "latest" || pointer.kind !== "stable" ||
    pointer.id !== manifest.id || pointer.tag !== manifest.tag || pointer.commit !== manifest.commit ||
    !Number.isSafeInteger(pointer.generation) || pointer.generation <= 0 ||
    typeof pointer.updatedAt !== "string" || !Number.isFinite(Date.parse(pointer.updatedAt)) ||
    canonicalJson(value.manifest) !== canonicalJson(manifest)
  ) throw new Error("latest pointer does not resolve to the exact immutable stable manifest");
  return Object.freeze({ pointer: Object.freeze({ ...pointer }), manifest });
}

export function parseDraftStableDescriptor(value, ready) {
  if (!isRecord(value) || !isRecord(value.draft) || !ready || !isRecord(ready.draft)) {
    throw new Error("authenticated stable draft descriptor is invalid");
  }
  const expected = {
    version: 1,
    kind: "stable",
    id: ready.tag,
    tag: ready.tag,
    commit: ready.head,
    channel: "latest",
    expectedChannel: ready.draft.expectedChannel,
    createdAt: ready.draft.createdAt,
    assets: ready.draft.assets.map(({ key: _key, ...asset }) => ({
      ...asset,
      downloadPath:
        `/api/releases/drafts/stable/${ready.tag}/assets/${encodeURIComponent(asset.name)}`,
    })),
  };
  if (canonicalJson(value.draft) !== canonicalJson(expected)) {
    throw new Error("authenticated stable draft descriptor changed reviewed identity");
  }
  return Object.freeze({
    ...expected,
    assets: Object.freeze(expected.assets.map((asset) => Object.freeze(asset))),
  });
}

export function createReviewedReleasePlan({
  ready,
  draft,
  crates,
  npm,
  assets,
  toolchain,
}) {
  if (
    !ready || !isRecord(ready.draft) || !isRecord(draft) ||
    draft.id !== ready.tag || draft.commit !== ready.head ||
    canonicalJson(draft.assets) !== canonicalJson(assets)
  ) throw new Error("reviewed plan requires exact downloaded draft assets");
  const localCrates = validateLocalCratePackages(crates, parseStableTag(ready.tag).version);
  const normalizedCrates = localCrates.map(({ path: _path, ...localPackage }) => localPackage);
  const normalizedToolchain = normalizeToolchain(toolchain);
  const normalizedNpm = normalizeNpmMetadata(npm, parseStableTag(ready.tag).version);
  const npmAsset = assets.find((asset) => asset.platform === "npm");
  if (
    !npmAsset || npmAsset.size !== normalizedNpm.size ||
    npmAsset.sha256 !== normalizedNpm.sha256 || npmAsset.name !== `${normalizedNpm.name}-${normalizedNpm.version}.tgz`
  ) throw new Error("reviewed plan npm metadata does not match the downloaded draft asset");

  const releaseObjects = stableReleaseObjects(ready.tag);
  if (canonicalJson(releaseObjects.assets) !== canonicalJson(
    ready.draft.assets.map(({ name, key }) => ({ name, key })),
  )) throw new Error("reviewed plan requires exact immutable release object keys");

  const draftIdentity = structuredClone(draft);
  const manifestIdentity = {
    version: 1,
    kind: "stable",
    id: ready.tag,
    tag: ready.tag,
    commit: ready.head,
    channel: "latest",
    assets: draft.assets.map(({ downloadPath: _draftPath, ...asset }) => ({
      ...asset,
      downloadPath:
        `/api/releases/releases/stable/${ready.tag}/assets/${encodeURIComponent(asset.name)}`,
    })),
  };
  const payload = {
    version: 1,
    format: "nanocodex-native-stable-release/v1",
    tag: ready.tag,
    head: ready.head,
    draft: draftIdentity,
    draftSha256: sha256CanonicalJson(draftIdentity),
    manifest: manifestIdentity,
    manifestIdentitySha256: sha256CanonicalJson(manifestIdentity),
    crates: normalizedCrates,
    npm: normalizedNpm,
    assets: structuredClone(assets),
    releaseObjects,
    toolchain: normalizedToolchain,
    provenance: releaseProvenanceNotice,
  };
  return deepFreeze({
    ...payload,
    planSha256: sha256CanonicalJson(payload),
  });
}

export function parseReviewedReleasePlan(value) {
  if (
    !isRecord(value) || value.version !== 1 ||
    value.format !== "nanocodex-native-stable-release/v1" ||
    typeof value.tag !== "string" || typeof value.head !== "string" ||
    !isRecord(value.draft) || !isRecord(value.manifest) ||
    !Array.isArray(value.crates) || !isRecord(value.npm) ||
    !Array.isArray(value.assets) || !isRecord(value.releaseObjects) ||
    !isRecord(value.toolchain) ||
    typeof value.draftSha256 !== "string" || !SHA256.test(value.draftSha256) ||
    typeof value.manifestIdentitySha256 !== "string" ||
    !SHA256.test(value.manifestIdentitySha256) ||
    typeof value.planSha256 !== "string" || !SHA256.test(value.planSha256)
  ) throw new Error("reviewed stable release plan has an invalid shape");
  parseStableTag(value.tag);
  assertSha1(value.head, "reviewed release plan head");
  if (
    value.draft.id !== value.tag || value.draft.tag !== value.tag ||
    value.draft.commit !== value.head || value.manifest.id !== value.tag ||
    value.manifest.tag !== value.tag || value.manifest.commit !== value.head ||
    value.draftSha256 !== sha256CanonicalJson(value.draft) ||
    value.manifestIdentitySha256 !== sha256CanonicalJson(value.manifest)
  ) throw new Error("reviewed stable release plan identity or digest is invalid");
  const { planSha256, ...payload } = value;
  if (planSha256 !== sha256CanonicalJson(payload)) {
    throw new Error("reviewed stable release plan canonical SHA-256 is invalid");
  }
  if (value.crates.length !== releaseCrateNames.length) {
    throw new Error("reviewed stable release plan does not contain all eight crates");
  }
  for (let index = 0; index < releaseCrateNames.length; index += 1) {
    const localPackage = value.crates[index];
    if (
      !isRecord(localPackage) || localPackage.crate !== releaseCrateNames[index] ||
      localPackage.version !== value.tag.slice(1) ||
      !Number.isSafeInteger(localPackage.size) || localPackage.size <= 0 ||
      typeof localPackage.sha256 !== "string" || !SHA256.test(localPackage.sha256)
    ) throw new Error("reviewed stable release plan crate identities are invalid");
  }
  normalizeNpmMetadata(value.npm, value.tag.slice(1));
  normalizeToolchain(value.toolchain);
  if (
    canonicalJson(value.assets) !== canonicalJson(value.draft.assets) ||
    canonicalJson(value.releaseObjects) !== canonicalJson(stableReleaseObjects(value.tag)) ||
    canonicalJson(value.provenance) !== canonicalJson(releaseProvenanceNotice)
  ) throw new Error("reviewed stable release plan asset or provenance identity is invalid");
  return deepFreeze(structuredClone(value));
}

export function assertReviewedReleasePlan(expected, observed, phase = "publication") {
  const reviewed = parseReviewedReleasePlan(expected);
  const reproduced = parseReviewedReleasePlan(observed);
  if (canonicalJson(reviewed) !== canonicalJson(reproduced)) {
    throw new ReviewedReleasePlanMismatchError(
      `reviewed stable release plan did not reproduce during ${phase}: ` +
        `expected ${reviewed.planSha256}, observed ${reproduced.planSha256}`,
    );
  }
  return reviewed;
}

export function cargoPublicationEnvironment(env = process.env, { cargoHome } = {}) {
  if (
    typeof cargoHome !== "string" || cargoHome === "" || cargoHome !== resolve(cargoHome)
  ) throw new Error("an absolute isolated Cargo home is required");
  return {
    ...runtimeEnvironment(env),
    CARGO_HOME: cargoHome,
    CARGO_REGISTRY_TOKEN: requiredEnvironment(env, "CARGO_REGISTRY_TOKEN"),
  };
}

export function cargoPublishArguments(crate, targetDirectory, { dryRun = false } = {}) {
  if (!releaseCrateNames.includes(crate)) throw new Error(`unexpected release crate ${crate}`);
  if (
    typeof targetDirectory !== "string" || targetDirectory === "" ||
    targetDirectory !== resolve(targetDirectory)
  ) throw new Error("an absolute Cargo publication target is required");
  return Object.freeze([
    "publish",
    ...(dryRun ? ["--dry-run"] : []),
    "--locked",
    "--no-verify",
    "--config",
    ".cargo/release.toml",
    "--registry",
    "crates-io",
    "--package",
    crate,
    "--target-dir",
    targetDirectory,
  ]);
}

export function npmPublicationEnvironment(env = process.env, { userConfig, cache } = {}) {
  const npmToken = optionalEnvironment(env, "NPM_TOKEN");
  const nodeToken = optionalEnvironment(env, "NODE_AUTH_TOKEN");
  if (npmToken && nodeToken && npmToken !== nodeToken) {
    throw new Error("NPM_TOKEN and NODE_AUTH_TOKEN disagree");
  }
  const token = nodeToken ?? npmToken;
  if (!token) throw new Error("NPM_TOKEN or NODE_AUTH_TOKEN is required");
  if (typeof userConfig !== "string" || userConfig === "") {
    throw new Error("an isolated npm user configuration path is required");
  }
  return {
    ...runtimeEnvironment(env),
    NODE_AUTH_TOKEN: token,
    NPM_CONFIG_USERCONFIG: userConfig,
    NPM_CONFIG_PROVENANCE: "false",
    NPM_CONFIG_REGISTRY: npmRegistryOrigin,
    ...(cache ? { NPM_CONFIG_CACHE: cache } : {}),
  };
}

export function releaseRequestHeaders(token, { json = false } = {}) {
  if (typeof token !== "string" || token.trim() === "") {
    throw new Error("CI_RELEASE_TOKEN is required");
  }
  return new Headers({
    accept: "application/json",
    authorization: `Bearer ${token.trim()}`,
    ...(json ? { "content-type": "application/json" } : {}),
  });
}

export function parseNpmRegistryVersion(value, expected) {
  if (
    !isRecord(value) || value.name !== "nanocodex" || value.version !== expected.version ||
    !isRecord(value.dist) || value.dist.integrity !== expected.integrity ||
    value.dist.shasum !== expected.sha1 || typeof value.dist.tarball !== "string"
  ) throw new Error(`npm registry nanocodex@${expected.version} has the wrong integrity`);
  const tarball = new URL(value.dist.tarball);
  const expectedTarball = `${npmRegistryOrigin}/nanocodex/-/nanocodex-${expected.version}.tgz`;
  if (tarball.href !== expectedTarball) {
    throw new Error("npm registry returned an untrusted tarball URL");
  }
  return Object.freeze({
    name: value.name,
    version: value.version,
    integrity: value.dist.integrity,
    shasum: value.dist.shasum,
    tarball: tarball.href,
  });
}

export async function verifyNpmRegistryTarballResponse(response, record, expected) {
  if (!(response instanceof Response) || !isRecord(record) || !isRecord(expected)) {
    throw new TypeError("npm registry tarball response, record, and expectation are required");
  }
  if (response.url && response.url !== record.tarball) {
    throw new ReleaseValidationError("npm registry tarball response changed canonical URL");
  }
  const declaredLength = parseCanonicalInteger(response.headers.get("content-length"));
  if (declaredLength != null && declaredLength !== expected.size) {
    throw new ReleaseValidationError(
      `npm registry tarball declared ${declaredLength} bytes; expected ${expected.size}`,
    );
  }
  if (response.headers.has("content-encoding") || response.headers.has("content-range")) {
    throw new ReleaseValidationError(
      "npm registry tarball response must contain exact unencoded bytes",
    );
  }
  const body = await readResponseBuffer(
    response,
    expected.size,
    "npm registry tarball",
    true,
  );
  let observed;
  try {
    observed = verifyNpmPackageBytes(body, {
      size: expected.size,
      sha256: expected.sha256,
    }, `v${expected.version}`);
  } catch (cause) {
    throw new ReleaseValidationError(
      `npm registry tarball bytes are invalid for nanocodex@${expected.version}`,
      { cause },
    );
  }
  if (
    observed.sha1 !== expected.sha1 || observed.sha1 !== record.shasum ||
    observed.integrity !== expected.integrity || observed.integrity !== record.integrity ||
    observed.sha256 !== expected.sha256 || observed.size !== expected.size ||
    observed.name !== expected.name || observed.version !== expected.version ||
    observed.packageJsonSha256 !== expected.packageJsonSha256
  ) throw new ReleaseValidationError(
    `npm registry tarball bytes differ from nanocodex@${expected.version}`,
  );
  return Object.freeze({
    ...record,
    size: observed.size,
    sha1: observed.sha1,
    sha256: observed.sha256,
    integrity: observed.integrity,
    packageJsonSha256: observed.packageJsonSha256,
  });
}

export function parseCrateRegistryVersion(value, crate, version) {
  if (!releaseCrateNames.includes(crate) || !isRecord(value) || !isRecord(value.version)) {
    throw new Error(`crates.io returned invalid metadata for ${crate}@${version}`);
  }
  const record = value.version;
  if (
    record.crate !== crate || record.num !== version || record.yanked !== false ||
    typeof record.checksum !== "string" || !SHA256.test(record.checksum) ||
    typeof record.dl_path !== "string"
  ) throw new Error(`crates.io returned the wrong version or integrity for ${crate}@${version}`);
  const expectedPath = `/api/v1/crates/${crate}/${version}/download`;
  const url = new URL(record.dl_path, cratesRegistryOrigin);
  if (url.origin !== cratesRegistryOrigin || url.pathname !== expectedPath) {
    throw new Error(`crates.io returned an untrusted download path for ${crate}@${version}`);
  }
  return Object.freeze({ crate, version, checksum: record.checksum, downloadUrl: url.href });
}

export async function inspectLocalCratePackage(
  path,
  crate,
  version,
  { maximumBytes = maximumCrateBytes } = {},
) {
  if (!releaseCrateNames.includes(crate)) throw new Error(`unexpected release crate ${crate}`);
  parseStableTag(`v${version}`);
  if (
    !Number.isSafeInteger(maximumBytes) || maximumBytes <= 0 ||
    maximumBytes > maximumCrateBytes
  ) throw new Error(`Cargo package bound must be at most ${maximumCrateBytes} bytes`);
  if (typeof path !== "string" || path === "") throw new Error("Cargo package path is required");
  const absolutePath = resolve(path);
  if (basename(absolutePath) !== `${crate}-${version}.crate`) {
    throw new Error(`Cargo package path has the wrong identity for ${crate}@${version}`);
  }

  let pathInfo;
  try {
    pathInfo = await lstat(absolutePath, { bigint: true });
  } catch (cause) {
    throw new Error(`Cargo package is unavailable for ${crate}@${version}`, { cause });
  }
  if (pathInfo.isSymbolicLink() || !pathInfo.isFile()) {
    throw new Error(`Cargo package must be a regular non-symlink for ${crate}@${version}`);
  }
  if (pathInfo.size <= 0n || pathInfo.size > BigInt(maximumBytes)) {
    throw new Error(`Cargo package size is invalid for ${crate}@${version}`);
  }

  let handle;
  try {
    handle = await open(
      absolutePath,
      fsConstants.O_RDONLY | (fsConstants.O_NOFOLLOW ?? 0),
    );
  } catch (cause) {
    throw new Error(`Cargo package could not be opened safely for ${crate}@${version}`, { cause });
  }
  try {
    const opened = await handle.stat({ bigint: true });
    if (!opened.isFile() || !sameFileIdentity(pathInfo, opened)) {
      throw new Error(`Cargo package changed while opening ${crate}@${version}`);
    }
    const hash = createHash("sha256");
    const buffer = Buffer.allocUnsafe(crateReadChunkBytes);
    let size = 0;
    while (true) {
      const remaining = maximumBytes - size;
      const length = Math.min(buffer.byteLength, remaining + 1);
      const { bytesRead } = await handle.read(buffer, 0, length, null);
      if (bytesRead === 0) break;
      size += bytesRead;
      if (size > maximumBytes) {
        throw new Error(`Cargo package exceeded ${maximumBytes} bytes for ${crate}@${version}`);
      }
      hash.update(buffer.subarray(0, bytesRead));
    }
    const closedSnapshot = await handle.stat({ bigint: true });
    if (
      BigInt(size) !== opened.size ||
      !sameFileSnapshot(opened, closedSnapshot)
    ) throw new Error(`Cargo package changed while hashing ${crate}@${version}`);
    return Object.freeze({
      crate,
      version,
      path: absolutePath,
      size,
      sha256: hash.digest("hex"),
    });
  } finally {
    await handle.close();
  }
}

export function cargoRepackagePath(localPackage) {
  if (
    !isRecord(localPackage) || !releaseCrateNames.includes(localPackage.crate) ||
    typeof localPackage.version !== "string" || typeof localPackage.path !== "string" ||
    localPackage.path !== resolve(localPackage.path) ||
    basename(localPackage.path) !== `${localPackage.crate}-${localPackage.version}.crate` ||
    basename(dirname(localPackage.path)) !== "package"
  ) throw new Error("local Cargo package path cannot identify its repackage output");
  return resolve(dirname(localPackage.path), "tmp-crate", basename(localPackage.path));
}

export async function verifyCargoRepackage(localPackage, phase) {
  if (typeof phase !== "string" || phase === "") {
    throw new Error("Cargo repackage verification phase is required");
  }
  return inspectMatchingCratePackage(cargoRepackagePath(localPackage), localPackage, phase);
}

export function validateLocalCratePackages(packages, version) {
  parseStableTag(`v${version}`);
  if (!Array.isArray(packages) || packages.length !== releaseCrateNames.length) {
    throw new Error(`release requires exactly ${releaseCrateNames.length} local Cargo packages`);
  }
  return Object.freeze(Array.from(
    { length: releaseCrateNames.length },
    (_, index) => {
      const localPackage = packages[index];
      const crate = releaseCrateNames[index];
      if (
        !isRecord(localPackage) || localPackage.crate !== crate ||
        localPackage.version !== version || typeof localPackage.path !== "string" ||
        localPackage.path !== resolve(localPackage.path) ||
        basename(localPackage.path) !== `${crate}-${version}.crate` ||
        !Number.isSafeInteger(localPackage.size) || localPackage.size <= 0 ||
        localPackage.size > maximumCrateBytes ||
        typeof localPackage.sha256 !== "string" || !SHA256.test(localPackage.sha256)
      ) throw new Error(`local Cargo package has the wrong identity for ${crate}@${version}`);
      return Object.freeze({
        crate,
        version,
        path: localPackage.path,
        size: localPackage.size,
        sha256: localPackage.sha256,
      });
    },
  ));
}

export function reconcileCrateRegistryVersions(
  packages,
  records,
  { requireAll = false } = {},
) {
  if (!Array.isArray(packages) || packages.length !== releaseCrateNames.length) {
    throw new Error("local Cargo package set is invalid");
  }
  const version = packages[0]?.version;
  const localPackages = validateLocalCratePackages(packages, version);
  if (!Array.isArray(records) || records.length !== localPackages.length) {
    throw new Error("crates.io record set does not match the local Cargo package set");
  }
  const reconciled = Array.from({ length: localPackages.length }, (_, index) => {
    const record = records[index];
    return record == null
      ? undefined
      : assertCrateRegistryChecksum(localPackages[index], record);
  });
  if (requireAll && reconciled.some((record) => record == null)) {
    const missing = localPackages
      .filter((_, index) => reconciled[index] == null)
      .map(({ crate }) => crate);
    throw new Error(`crates.io did not publish: ${missing.join(", ")}`);
  }
  return Object.freeze(reconciled);
}

export function registryCredentialRequirements(crateRecords, npmRecord) {
  if (!Array.isArray(crateRecords) || crateRecords.length !== releaseCrateNames.length) {
    throw new Error("credential preflight requires all eight crates.io observations");
  }
  for (let index = 0; index < crateRecords.length; index += 1) {
    const record = crateRecords[index];
    if (
      record != null &&
      (!isRecord(record) || record.crate !== releaseCrateNames[index] ||
        typeof record.version !== "string" || typeof record.checksum !== "string" ||
        !SHA256.test(record.checksum))
    ) throw new Error("credential preflight received invalid crates.io observations");
  }
  if (npmRecord != null && (!isRecord(npmRecord) || npmRecord.name !== "nanocodex")) {
    throw new Error("credential preflight received an invalid npm observation");
  }
  return Object.freeze({
    cargo: crateRecords.some((record) => record == null),
    npm: npmRecord == null,
  });
}

export async function publishCratePackages(
  packages,
  operations,
  { signal, maximumAttempts = 6 } = {},
) {
  if (!isRecord(operations)) throw new TypeError("crate publication operations are required");
  for (const name of ["readVersion", "publishVersion", "waitVersion"]) {
    if (typeof operations[name] !== "function") {
      throw new TypeError(`crate publication operation ${name} is required`);
    }
  }
  if (!Number.isSafeInteger(maximumAttempts) || maximumAttempts <= 0 || maximumAttempts > 10) {
    throw new Error("crate publication attempts must be between one and ten");
  }
  const version = packages?.[0]?.version;
  const localPackages = validateLocalCratePackages(packages, version);

  // Reconcile the complete registry view before the first mutation. A mismatch
  // in a later dependency must not be discovered after publishing an earlier one.
  const initialRecords = await Promise.all(localPackages.map((localPackage) =>
    operations.readVersion(localPackage, signal)
  ));
  const reconciled = reconcileCrateRegistryVersions(localPackages, initialRecords);
  const publications = [];
  for (let index = 0; index < localPackages.length; index += 1) {
    const localPackage = localPackages[index];
    if (reconciled[index]) {
      publications.push(Object.freeze({
        crate: localPackage.crate,
        action: "already-published",
        sha256: localPackage.sha256,
      }));
      continue;
    }

    let completed;
    for (let attempt = 1; attempt <= maximumAttempts; attempt += 1) {
      // Close the race between the all-crate preflight and this exact upload.
      const raced = await operations.readVersion(localPackage, signal);
      if (raced) {
        assertCrateRegistryChecksum(localPackage, raced);
        completed = Object.freeze({
          crate: localPackage.crate,
          action: "concurrently-published",
          sha256: localPackage.sha256,
        });
        break;
      }

      try {
        if (typeof operations.beforePublish === "function") {
          await operations.beforePublish(localPackage, attempt, signal);
        }
        await operations.publishVersion(localPackage, attempt, signal);
      } catch (cause) {
        if (
          cause instanceof CrateRepackageMismatchError ||
          cause instanceof PublicationLeaseLostError || signal?.aborted
        ) throw signal?.aborted ? (signal.reason ?? cause) : cause;
        const observed = await operations.readVersion(localPackage, signal);
        if (observed) {
          assertCrateRegistryChecksum(localPackage, observed);
          completed = Object.freeze({
            crate: localPackage.crate,
            action: "published-despite-client-error",
            sha256: localPackage.sha256,
          });
          break;
        }
        if (attempt === maximumAttempts) {
          throw new Error(
            `failed to publish ${localPackage.crate}@${localPackage.version} after ${attempt} attempts`,
            { cause },
          );
        }
        if (typeof operations.retryDelay === "function") {
          await operations.retryDelay(attempt, signal);
        } else {
          await abortableDelay(attempt * 10_000, signal);
        }
        continue;
      }

      const observed = await operations.waitVersion(localPackage, signal);
      assertCrateRegistryChecksum(localPackage, observed);
      completed = Object.freeze({
        crate: localPackage.crate,
        action: "published",
        sha256: localPackage.sha256,
      });
      break;
    }
    publications.push(completed);
  }
  return Object.freeze({
    action: publications.every(({ action }) => action === "already-published")
      ? "already-published"
      : "published",
    crates: Object.freeze(publications),
  });
}

export function parsePublicationLease(value, identity, previous, { now = Date.now() } = {}) {
  const keys = [
    "commit",
    "expiresAt",
    "generation",
    "id",
    "kind",
    "leaseId",
    "owner",
    "version",
  ];
  if (
    !isRecord(value) || !isRecord(identity) ||
    canonicalJson(Object.keys(value).sort()) !== canonicalJson(keys) ||
    value.version !== 1 || value.owner !== identity.owner || value.kind !== "stable" ||
    value.kind !== identity.kind || value.id !== identity.id || value.commit !== identity.commit ||
    typeof value.leaseId !== "string" || value.leaseId.length < 8 || value.leaseId.length > 128 ||
    !/^[A-Za-z0-9._:-]+$/.test(value.leaseId) ||
    !Number.isSafeInteger(value.generation) || value.generation <= 0 ||
    !value.leaseId.startsWith(`${value.generation}.`) ||
    typeof value.expiresAt !== "string" || !Number.isFinite(Date.parse(value.expiresAt)) ||
    Date.parse(value.expiresAt) <= now
  ) throw new Error("publication lease response has the wrong identity or expiry");
  if (
    previous &&
    (previous.leaseId !== value.leaseId || previous.generation !== value.generation)
  ) throw new PublicationLeaseLostError("heartbeat fencing validation");
  return Object.freeze({ ...value });
}

export function publicationLeaseFinalizeHeaders(lease) {
  if (
    !isRecord(lease) || typeof lease.leaseId !== "string" ||
    !/^[A-Za-z0-9._:-]{8,128}$/.test(lease.leaseId) ||
    typeof lease.owner !== "string" || !uuidV4Pattern.test(lease.owner) ||
    !Number.isSafeInteger(lease.generation) || lease.generation <= 0 ||
    !lease.leaseId.startsWith(`${lease.generation}.`)
  ) throw new Error("stable finalization requires an exact publication lease fence");
  return Object.freeze({
    "x-nanocodex-publication-lease-id": lease.leaseId,
    "x-nanocodex-publication-lease-owner": lease.owner,
    "x-nanocodex-publication-lease-generation": String(lease.generation),
  });
}

export async function runWithPublicationLease(identity, operations, body, {
  signal,
  heartbeatMs = 30_000,
} = {}) {
  if (
    !isRecord(identity) || typeof operations?.acquire !== "function" ||
    typeof operations?.heartbeat !== "function" || typeof operations?.release !== "function" ||
    typeof body !== "function" || !Number.isSafeInteger(heartbeatMs) || heartbeatMs <= 0
  ) throw new TypeError("publication lease lifecycle operations are invalid");
  const delay = typeof operations.delay === "function" ? operations.delay : abortableDelay;
  let lease = await operations.acquire(identity, signal);
  lease = parsePublicationLease(lease, identity);

  const owned = new AbortController();
  const parentAbort = () => owned.abort(
    signal.reason ?? new DOMException("publication operation aborted", "AbortError"),
  );
  signal?.addEventListener("abort", parentAbort, { once: true });
  if (signal?.aborted) parentAbort();

  let stopped = false;
  let authorityFailure;
  let renewal = Promise.resolve();
  const renew = (phase) => {
    const next = renewal.then(async () => {
      if (authorityFailure) throw authorityFailure;
      throwIfAborted(owned.signal);
      try {
        const observed = await operations.heartbeat(lease, owned.signal, phase);
        lease = parsePublicationLease(observed, identity, lease);
        return lease;
      } catch (cause) {
        if (stopped && cause?.name === "AbortError") throw cause;
        authorityFailure = cause instanceof PublicationLeaseLostError
          ? cause
          : new PublicationLeaseLostError(phase, { cause });
        owned.abort(authorityFailure);
        throw authorityFailure;
      }
    });
    renewal = next.catch(() => undefined);
    return next;
  };

  const heartbeatLoop = (async () => {
    while (!stopped) {
      try {
        await delay(heartbeatMs, owned.signal);
      } catch (cause) {
        if (stopped || owned.signal.aborted) return;
        authorityFailure = new PublicationLeaseLostError("heartbeat delay", { cause });
        owned.abort(authorityFailure);
        return;
      }
      if (stopped) return;
      try {
        await renew("background heartbeat");
      } catch {
        return;
      }
    }
  })();

  const authority = Object.freeze({
    get lease() {
      return lease;
    },
    signal: owned.signal,
    async checkpoint(phase) {
      if (typeof phase !== "string" || phase === "") {
        throw new TypeError("publication authority checkpoint requires a phase");
      }
      if (authorityFailure) throw authorityFailure;
      await renew(phase);
      if (authorityFailure) throw authorityFailure;
      return lease;
    },
  });

  let result;
  let primaryFailure;
  try {
    result = await body(authority);
    if (authorityFailure) throw authorityFailure;
  } catch (cause) {
    primaryFailure = cause;
  } finally {
    stopped = true;
    if (!owned.signal.aborted) {
      owned.abort(new DOMException("publication lease lifecycle complete", "AbortError"));
    }
    await heartbeatLoop.catch((cause) => {
      if (!primaryFailure && !stopped) primaryFailure = cause;
    });
    signal?.removeEventListener("abort", parentAbort);
    try {
      await operations.release(lease);
    } catch (cause) {
      primaryFailure = primaryFailure
        ? new AggregateError(
          [primaryFailure, cause],
          "publication operation and lease release failed",
        )
        : cause;
    }
  }
  if (primaryFailure) throw primaryFailure;
  return result;
}

/** Pure ordering boundary used by the CLI and focused tests. */
export async function runReleaseSequence(command, operations, { signal } = {}) {
  if (!["stage", "publish"].includes(command) || !isRecord(operations)) {
    throw new TypeError("release sequence requires a stage/publish command and operations");
  }
  const trust = await operations.verifyTrust("initial", signal);
  const cratePackages = await operations.packageCrates(trust, signal);
  const existingCrates = await operations.verifyExistingCrates(cratePackages, signal);
  const ready = command === "stage"
    ? await operations.stage(trust, signal)
    : await operations.readReady(trust, signal);

  if (command === "stage") {
    let reviewed;
    if (ready.status === "success") {
      const stored = await operations.readPlan(signal);
      const published = await operations.readPublished(ready, signal);
      if (!published) throw new Error("released distribution has no immutable public manifest");
      reviewed = await operations.reviewPublishedPlan(
        ready,
        cratePackages,
        published,
        stored,
        signal,
      );
      assertReviewedReleasePlan(stored, reviewed.plan, "stage replay");
    } else {
      reviewed = await operations.reviewPlan(ready, cratePackages, signal);
    }
    await operations.assertTrust(trust, "after staging", signal);
    await operations.assertReady(ready, "after staging", signal);
    const persisted = await operations.persistPlan(reviewed.plan, signal);
    return {
      action: ready.status === "success" ? "already-published" : "staged",
      head: ready.head,
      tag: ready.tag,
      ...(ready.requestId === undefined ? {} : { requestId: ready.requestId }),
      npm: reviewed.npmPackage.metadata,
      plan: {
        path: persisted.path,
        sha256: reviewed.plan.planSha256,
        action: persisted.action,
      },
    };
  }

  const storedPlan = await operations.readPlan(signal);
  const existing = await operations.readPublished(ready, signal);
  if (existing) {
    const reviewed = await operations.reviewPublishedPlan(
      ready,
      cratePackages,
      existing,
      storedPlan,
      signal,
    );
    assertReviewedReleasePlan(storedPlan, reviewed.plan, "published replay");
    const crates = await operations.verifyCrates(cratePackages, signal);
    const npm = await operations.verifyNpmRegistry(reviewed.npmPackage, signal);
    const publication = await operations.verifyPublic(ready, existing, signal);
    await operations.assertTrust(trust, "after replay verification", signal);
    await operations.assertReady(ready, "after replay verification", signal);
    return {
      action: "already-published",
      head: ready.head,
      tag: ready.tag,
      npm: reviewed.npmPackage.metadata,
      registries: { crates, npm },
      publication,
      plan: { path: operations.planPath, sha256: storedPlan.planSha256 },
    };
  }

  const reviewed = await operations.reviewPlan(ready, cratePackages, signal);
  assertReviewedReleasePlan(storedPlan, reviewed.plan, "publish startup");

  const npmRecord = await operations.inspectNpmRegistry(reviewed.npmPackage, signal);
  const registryState = Object.freeze({ crates: existingCrates, npm: npmRecord });
  await operations.preflightCredentials(
    registryState,
    cratePackages,
    reviewed.npmPackage,
    signal,
  );

  const identity = {
    owner: operations.leaseOwner,
    kind: "stable",
    id: ready.tag,
    commit: ready.head,
  };
  return runWithPublicationLease(
    identity,
    operations.publicationLease,
    async (authority) => {
      const racedPublication = await operations.readPublished(ready, authority.signal);
      if (racedPublication) {
        const crates = await operations.verifyCrates(cratePackages, authority.signal);
        const npm = await operations.verifyNpmRegistry(
          reviewed.npmPackage,
          authority.signal,
        );
        const publication = await operations.verifyPublic(
          ready,
          racedPublication,
          authority.signal,
        );
        await operations.assertTrust(
          trust,
          "after raced publication verification",
          authority.signal,
        );
        await authority.checkpoint("after raced public byte verification");
        return {
          action: "already-published",
          head: ready.head,
          tag: ready.tag,
          npm: reviewed.npmPackage.metadata,
          registries: { crates, npm },
          publication,
          plan: { path: operations.planPath, sha256: storedPlan.planSha256 },
        };
      }

      await operations.assertTrust(trust, "before plan reproduction", authority.signal);
      await operations.assertReady(ready, "before plan reproduction", authority.signal);
      const reproduced = await operations.reviewPlan(ready, cratePackages, authority.signal);
      assertReviewedReleasePlan(storedPlan, reproduced.plan, "lease-owned publication");

      // Registry state can only skip a credential while its exact bytes remain
      // present. Re-observe both registries after any lease wait and re-run the
      // complete credential preflight before the first possible upload.
      const leaseCrates = await operations.verifyExistingCrates(
        cratePackages,
        authority.signal,
      );
      const leaseNpm = await operations.inspectNpmRegistry(
        reproduced.npmPackage,
        authority.signal,
      );
      await operations.preflightCredentials(
        Object.freeze({ crates: leaseCrates, npm: leaseNpm }),
        cratePackages,
        reproduced.npmPackage,
        authority.signal,
      );

      await authority.checkpoint("before crate publication");
      await operations.assertTrust(trust, "before crate publication", authority.signal);
      await operations.assertReady(ready, "before crate publication", authority.signal);
      const cratePublication = await operations.publishCrates(
        ready,
        cratePackages,
        authority,
        authority.signal,
      );
      const crates = await operations.verifyCrates(cratePackages, authority.signal);

      await authority.checkpoint("before npm publication");
      await operations.assertTrust(trust, "before npm publication", authority.signal);
      await operations.assertReady(ready, "before npm publication", authority.signal);
      const npmPublication = await operations.publishNpm(
        reproduced.npmPackage,
        authority,
        authority.signal,
      );
      const npm = await operations.verifyNpmRegistry(
        reproduced.npmPackage,
        authority.signal,
      );

      await authority.checkpoint("before finalization");
      await operations.assertTrust(trust, "before finalization", authority.signal);
      await operations.assertReady(ready, "before finalization", authority.signal);
      const finalized = await operations.finalize(ready, authority, authority.signal);
      const publication = await operations.verifyPublic(
        ready,
        finalized,
        authority.signal,
      );
      await operations.assertTrust(
        trust,
        "after public verification",
        authority.signal,
      );
      await authority.checkpoint("after public byte verification");
      return {
        action: "published",
        head: ready.head,
        tag: ready.tag,
        npm: reproduced.npmPackage.metadata,
        registries: { cratePublication, crates, npmPublication, npm },
        publication,
        lease: {
          leaseId: authority.lease.leaseId,
          generation: authority.lease.generation,
        },
        plan: { path: operations.planPath, sha256: storedPlan.planSha256 },
      };
    },
    { signal, heartbeatMs: operations.leaseHeartbeatMs },
  );
}

function assetSpec(name, platform, key, maxBytes, contentType) {
  return { name, platform, key, maxBytes, contentType };
}

function isCanonicalPriorTag(value, current) {
  try {
    const left = parseStableTag(value);
    const right = parseStableTag(current);
    return [left.major, left.minor, left.patch].some((part, index) => {
      const prior = [left.major, left.minor, left.patch];
      const next = [right.major, right.minor, right.patch];
      for (let cursor = 0; cursor < index; cursor += 1) {
        if (prior[cursor] !== next[cursor]) return false;
      }
      return part < next[index];
    });
  } catch {
    return false;
  }
}

function isTagBinding(value) {
  return isRecord(value) && typeof value.tag === "string" &&
    stableTagPattern.test(value.tag) && value.ref === `refs/tags/${value.tag}` &&
    typeof value.object === "string" && SHA1.test(value.object) &&
    typeof value.commit === "string" && SHA1.test(value.commit) &&
    typeof value.annotated === "boolean";
}

function assertSha1(value, description) {
  if (typeof value !== "string" || !SHA1.test(value)) {
    throw new Error(`${description} must be a full lowercase SHA-1`);
  }
}

function parseCanonicalInteger(value) {
  if (typeof value !== "string" || !/^(?:0|[1-9][0-9]*)$/.test(value)) return undefined;
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) ? parsed : undefined;
}

function isRecord(value) {
  return value != null && typeof value === "object" && !Array.isArray(value);
}

function sameFileIdentity(left, right) {
  return left.dev === right.dev && left.ino === right.ino && left.mode === right.mode &&
    left.size === right.size;
}

function sameFileSnapshot(left, right) {
  return sameFileIdentity(left, right) && left.mtimeNs === right.mtimeNs &&
    left.ctimeNs === right.ctimeNs;
}

function assertCrateRegistryChecksum(localPackage, record) {
  if (
    !isRecord(record) || record.crate !== localPackage.crate ||
    record.version !== localPackage.version || typeof record.checksum !== "string" ||
    !SHA256.test(record.checksum)
  ) {
    throw new Error(
      `crates.io returned invalid API identity for ${localPackage.crate}@${localPackage.version}`,
    );
  }
  if (record.checksum !== localPackage.sha256) {
    throw new Error(
      `crates.io API checksum mismatch for ${localPackage.crate}@${localPackage.version}: ` +
        `local ${localPackage.sha256}, registry ${record.checksum}`,
    );
  }
  return Object.freeze({
    crate: localPackage.crate,
    version: localPackage.version,
    checksum: record.checksum,
    ...(typeof record.downloadUrl === "string" ? { downloadUrl: record.downloadUrl } : {}),
  });
}

function assertSameCratePackage(expected, observed, phase) {
  if (
    observed.crate !== expected.crate || observed.version !== expected.version ||
    observed.size !== expected.size || observed.sha256 !== expected.sha256
  ) {
    throw new CrateRepackageMismatchError(
      expected.crate,
      expected.version,
      phase,
      `${expected.size} bytes/${expected.sha256}`,
      `${observed.size} bytes/${observed.sha256}`,
    );
  }
  return expected;
}

async function inspectMatchingCratePackage(path, expected, phase) {
  let observed;
  try {
    observed = await inspectLocalCratePackage(path, expected.crate, expected.version);
  } catch (cause) {
    throw new CrateRepackageMismatchError(
      expected.crate,
      expected.version,
      phase,
      `${expected.size} bytes/${expected.sha256}`,
      "an unavailable or unsafe archive",
      { cause },
    );
  }
  return assertSameCratePackage(expected, observed, phase);
}

function canonicalJson(value) {
  if (value === null || typeof value === "boolean" || typeof value === "string") {
    return JSON.stringify(value);
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new TypeError("canonical JSON requires finite numbers");
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (isRecord(value)) {
    return `{${Object.keys(value).sort().map((key) =>
      `${JSON.stringify(key)}:${canonicalJson(value[key])}`
    ).join(",")}}`;
  }
  throw new TypeError("canonical JSON requires JSON values");
}

function sha256CanonicalJson(value) {
  return createHash("sha256").update(canonicalJson(value)).digest("hex");
}

function normalizeNpmMetadata(value, version) {
  if (
    !isRecord(value) || value.name !== "nanocodex" || value.version !== version ||
    !Number.isSafeInteger(value.size) || value.size <= 0 || value.size > maximumNpmBytes ||
    typeof value.sha256 !== "string" || !SHA256.test(value.sha256) ||
    typeof value.sha1 !== "string" || !SHA1.test(value.sha1) ||
    typeof value.integrity !== "string" ||
    !/^sha512-[A-Za-z0-9+/]{86}==$/.test(value.integrity) ||
    typeof value.packageJsonSha256 !== "string" || !SHA256.test(value.packageJsonSha256)
  ) throw new Error(`reviewed npm metadata is invalid for nanocodex@${version}`);
  return Object.freeze({
    name: value.name,
    version: value.version,
    size: value.size,
    sha256: value.sha256,
    sha1: value.sha1,
    integrity: value.integrity,
    packageJsonSha256: value.packageJsonSha256,
  });
}

function normalizeToolchain(value) {
  const names = ["cargo", "node", "npm", "rustc"];
  if (!isRecord(value) || canonicalJson(Object.keys(value).sort()) !== canonicalJson(names)) {
    throw new Error("reviewed release toolchain must contain cargo, node, npm, and rustc");
  }
  const normalized = {};
  for (const name of names) {
    const version = value[name];
    if (
      typeof version !== "string" || version === "" ||
      Buffer.byteLength(version) > maximumToolchainVersionBytes || version.includes("\0")
    ) throw new Error(`reviewed release ${name} version is invalid`);
    normalized[name] = version;
  }
  return Object.freeze(normalized);
}

function deepFreeze(value) {
  if (isRecord(value) || Array.isArray(value)) {
    for (const child of Object.values(value)) deepFreeze(child);
    Object.freeze(value);
  }
  return value;
}

function inspectNpmTar(archive, onFile) {
  if (
    !Buffer.isBuffer(archive) || archive.byteLength === 0 ||
    archive.byteLength > maximumNpmExpandedBytes || archive.byteLength % 512 !== 0
  ) throw new Error("npm package tar has an invalid expanded size");
  let offset = 0;
  let entries = 0;
  let zeroBlocks = 0;
  let pendingPath;
  while (offset < archive.byteLength) {
    const header = archive.subarray(offset, offset + 512);
    if (header.every((byte) => byte === 0)) {
      zeroBlocks += 1;
      offset += 512;
      if (zeroBlocks >= 2) {
        if (!archive.subarray(offset).every((byte) => byte === 0)) {
          throw new Error("npm package tar has data after its end marker");
        }
        offset = archive.byteLength;
      }
      continue;
    }
    if (zeroBlocks !== 0) throw new Error("npm package tar has an incomplete end marker");
    if (++entries > maximumTarEntries) throw new Error("npm package tar has too many entries");
    validateTarChecksum(header);
    const name = tarText(header.subarray(0, 100), "name");
    const prefix = tarText(header.subarray(345, 500), "prefix");
    const size = tarOctal(header.subarray(124, 136), "size");
    const type = header[156] === 0 ? "0" : String.fromCharCode(header[156]);
    const bodyStart = offset + 512;
    const bodyEnd = bodyStart + size;
    const next = bodyStart + Math.ceil(size / 512) * 512;
    if (bodyEnd > archive.byteLength || next > archive.byteLength) {
      throw new Error("npm package tar entry is truncated");
    }
    if (!archive.subarray(bodyEnd, next).every((byte) => byte === 0)) {
      throw new Error("npm package tar entry padding is invalid");
    }
    const body = archive.subarray(bodyStart, bodyEnd);
    if (type === "x") {
      const pax = parsePax(body);
      if (pax.path) pendingPath = pax.path;
      offset = next;
      continue;
    }
    if (type === "g") {
      const pax = parsePax(body);
      if (pax.path) throw new Error("npm package global PAX header changes paths");
      offset = next;
      continue;
    }
    if (type === "L") {
      pendingPath = tarLongName(body);
      offset = next;
      continue;
    }
    const path = safeNpmPath(pendingPath ?? (prefix ? `${prefix}/${name}` : name));
    pendingPath = undefined;
    if (path !== "package" && !path.startsWith("package/")) {
      throw new Error(`npm package entry escapes package/: ${path}`);
    }
    if (type === "0") onFile(path, body);
    else if (type === "5") {
      if (size !== 0) throw new Error(`npm package directory has a body: ${path}`);
    } else {
      throw new Error(`npm package rejects tar entry type ${JSON.stringify(type)}`);
    }
    offset = next;
  }
  if (zeroBlocks < 2 || pendingPath !== undefined) {
    throw new Error("npm package tar has no complete end marker");
  }
}

function safeNpmPath(value) {
  if (
    typeof value !== "string" || value.length === 0 || value.includes("\0") ||
    value.includes("\\") || value.startsWith("/") || /[\u0000-\u001f\u007f]/.test(value)
  ) throw new Error("npm package tar has an unsafe path");
  const stripped = value.replace(/^\.\//, "").replace(/\/+$/, "");
  const parts = stripped.split("/");
  if (parts.some((part) => part === "" || part === "." || part === "..")) {
    throw new Error("npm package tar has an unsafe path");
  }
  return stripped;
}

function validateTarChecksum(header) {
  const expected = tarOctal(header.subarray(148, 156), "checksum");
  let observed = 0;
  for (let index = 0; index < header.length; index += 1) {
    observed += index >= 148 && index < 156 ? 0x20 : header[index];
  }
  if (expected !== observed) throw new Error("npm package tar header checksum is invalid");
}

function tarText(field, description) {
  const nul = field.indexOf(0);
  const body = nul < 0 ? field : field.subarray(0, nul);
  if (nul >= 0 && field.subarray(nul).some((byte) => byte !== 0)) {
    throw new Error(`npm package tar ${description} padding is invalid`);
  }
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(body);
  } catch (cause) {
    throw new Error(`npm package tar ${description} is not UTF-8`, { cause });
  }
}

function tarOctal(field, description) {
  const text = field.toString("ascii").replace(/[\0 ]+$/g, "").replace(/^ +/g, "");
  if (!/^[0-7]+$/.test(text)) throw new Error(`npm package tar ${description} is not octal`);
  const value = Number.parseInt(text, 8);
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new Error(`npm package tar ${description} is outside the supported range`);
  }
  return value;
}

function parsePax(body) {
  const values = {};
  let offset = 0;
  while (offset < body.byteLength) {
    const space = body.indexOf(0x20, offset);
    if (space < 0) throw new Error("npm package PAX header is invalid");
    const lengthText = body.subarray(offset, space).toString("ascii");
    if (!/^[1-9][0-9]*$/.test(lengthText)) throw new Error("npm package PAX length is invalid");
    const length = Number(lengthText);
    const end = offset + length;
    if (!Number.isSafeInteger(length) || end > body.byteLength || body[end - 1] !== 0x0a) {
      throw new Error("npm package PAX record is truncated");
    }
    const record = new TextDecoder("utf-8", { fatal: true }).decode(
      body.subarray(space + 1, end - 1),
    );
    const equals = record.indexOf("=");
    if (equals <= 0) throw new Error("npm package PAX record is invalid");
    const key = record.slice(0, equals);
    if (key === "path") values.path = record.slice(equals + 1);
    offset = end;
  }
  return values;
}

function tarLongName(body) {
  const nul = body.indexOf(0);
  const value = body.subarray(0, nul < 0 ? body.byteLength : nul);
  if (nul >= 0 && body.subarray(nul).some((byte) => byte !== 0)) {
    throw new Error("npm package GNU long name padding is invalid");
  }
  return new TextDecoder("utf-8", { fatal: true }).decode(value).replace(/\n$/, "");
}

class LocalReleaseReport {
  static async create(directory, command, release, secrets) {
    await mkdir(directory, { recursive: true, mode: 0o700 });
    const info = await lstat(directory);
    if (!info.isDirectory() || info.isSymbolicLink()) {
      throw new Error("NANOCODEX_RELEASE_REPORT_DIR must be a real directory");
    }
    const startedAt = new Date().toISOString();
    const identifier = randomUUID();
    const path = resolve(directory, `${release.tag}-${command}-${identifier}.json`);
    const report = new LocalReleaseReport(
      path,
      identifier,
      command,
      release,
      startedAt,
      secrets,
    );
    await report.#flush();
    return report;
  }

  #path;
  #attempt;
  #secrets;
  #counter = 0;
  #value;

  constructor(path, attempt, command, release, startedAt, secrets) {
    this.#path = path;
    this.#attempt = attempt;
    this.#secrets = secrets;
    this.#value = {
      version: 1,
      attempt,
      command,
      tag: release.tag,
      releaseVersion: release.version,
      status: "running",
      startedAt,
      provenance: releaseProvenanceNotice,
      steps: [],
    };
  }

  get path() {
    return this.#path;
  }

  get attempt() {
    return this.#attempt;
  }

  async run(name, operation, summarize = (value) => value) {
    const step = { name, status: "running", startedAt: new Date().toISOString() };
    this.#value.steps.push(step);
    await this.#flush();
    try {
      const result = await operation();
      step.status = "success";
      step.completedAt = new Date().toISOString();
      const summary = summarize(result);
      if (summary !== undefined) step.result = summary;
      await this.#flush();
      return result;
    } catch (cause) {
      step.status = "failed";
      step.completedAt = new Date().toISOString();
      step.error = boundedStructuredError(cause, this.#secrets);
      try {
        await this.#flush();
      } catch (reportCause) {
        throw new AggregateError(
          [cause, reportCause],
          "release operation failed and its step evidence could not be persisted",
        );
      }
      throw cause;
    }
  }

  async succeed(result) {
    this.#value.status = "success";
    this.#value.completedAt = new Date().toISOString();
    this.#value.result = result;
    await this.#flush();
  }

  async fail(cause) {
    this.#value.status = "failed";
    this.#value.completedAt = new Date().toISOString();
    this.#value.error = boundedStructuredError(cause, this.#secrets);
    await this.#flush();
  }

  async #flush() {
    const temporary = `${this.#path}.tmp-${process.pid}-${this.#counter++}`;
    let renamed = false;
    try {
      const handle = await open(
        temporary,
        fsConstants.O_CREAT | fsConstants.O_EXCL | fsConstants.O_WRONLY |
          (fsConstants.O_NOFOLLOW ?? 0),
        0o600,
      );
      try {
        await handle.writeFile(`${JSON.stringify(this.#value, null, 2)}\n`);
        await handle.sync();
      } finally {
        await handle.close();
      }
      await rename(temporary, this.#path);
      renamed = true;
    } finally {
      if (!renamed) await unlink(temporary).catch(() => undefined);
    }
  }
}

export async function readReviewedReleasePlan(path) {
  if (typeof path !== "string" || path === "" || path !== resolve(path)) {
    throw new Error("reviewed release plan path must be absolute");
  }
  let handle;
  try {
    handle = await open(path, fsConstants.O_RDONLY | (fsConstants.O_NOFOLLOW ?? 0));
  } catch (cause) {
    if (cause?.code === "ENOENT") {
      throw new Error(`reviewed release plan is missing at ${path}; run stage first`, { cause });
    }
    throw new Error(`reviewed release plan cannot be opened safely at ${path}`, { cause });
  }
  try {
    const info = await handle.stat();
    if (
      !info.isFile() || info.size <= 0 || info.size > maximumJsonBytes ||
      (info.mode & 0o077) !== 0
    ) throw new Error("reviewed release plan must be a private bounded regular file");
    const body = await handle.readFile();
    let value;
    try {
      value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(body));
    } catch (cause) {
      throw new Error("reviewed release plan contains invalid JSON", { cause });
    }
    return parseReviewedReleasePlan(value);
  } finally {
    await handle.close();
  }
}

export async function persistReviewedReleasePlan(path, plan) {
  const reviewed = parseReviewedReleasePlan(plan);
  if (typeof path !== "string" || path === "" || path !== resolve(path)) {
    throw new Error("reviewed release plan path must be absolute");
  }
  const directory = dirname(path);
  await mkdir(directory, { recursive: true, mode: 0o700 });
  const directoryInfo = await lstat(directory);
  if (
    !directoryInfo.isDirectory() || directoryInfo.isSymbolicLink() ||
    (directoryInfo.mode & 0o077) !== 0
  ) {
    throw new Error("reviewed release plan directory must be a private real directory");
  }
  const temporary = resolve(directory, `.${basename(path)}.${process.pid}.${randomUUID()}.tmp`);
  const handle = await open(
    temporary,
    fsConstants.O_CREAT | fsConstants.O_EXCL | fsConstants.O_WRONLY |
      (fsConstants.O_NOFOLLOW ?? 0),
    0o600,
  );
  try {
    await handle.writeFile(`${JSON.stringify(reviewed, null, 2)}\n`);
    await handle.sync();
  } finally {
    await handle.close();
  }
  let created = false;
  try {
    try {
      await link(temporary, path);
      created = true;
      const directoryHandle = await open(directory, fsConstants.O_RDONLY);
      try {
        await directoryHandle.sync();
      } finally {
        await directoryHandle.close();
      }
    } catch (cause) {
      if (cause?.code !== "EEXIST") throw cause;
      const existing = await readReviewedReleasePlan(path);
      assertReviewedReleasePlan(existing, reviewed, "stage replay");
    }
  } finally {
    await unlink(temporary).catch(() => undefined);
  }
  return Object.freeze({ action: created ? "created" : "replayed", plan: reviewed, path });
}

function releaseConfiguration(env = process.env) {
  const repository = resolve(env.NANOCODEX_REPO ?? defaultRepository);
  const ciOrigin = parseOrigin(requiredEnvironment(env, "NANOCODEX_CI_ORIGIN"));
  const publicOrigin = parseOrigin(
    env.NANOCODEX_RELEASE_ORIGIN ?? env.NANOCODEX_WEB_ORIGIN ?? ciOrigin,
  );
  return {
    env,
    repository,
    ciOrigin,
    publicOrigin,
    releaseToken: requiredEnvironment(env, "CI_RELEASE_TOKEN"),
    requestTimeoutMs: boundedIntegerEnvironment(
      env,
      "NANOCODEX_RELEASE_REQUEST_TIMEOUT_MS",
      30_000,
      1_000,
      120_000,
    ),
    assetTimeoutMs: boundedIntegerEnvironment(
      env,
      "NANOCODEX_RELEASE_ASSET_TIMEOUT_MS",
      10 * 60 * 1_000,
      30_000,
      30 * 60 * 1_000,
    ),
    httpAttempts: boundedIntegerEnvironment(
      env,
      "NANOCODEX_RELEASE_HTTP_ATTEMPTS",
      maximumHttpAttempts,
      1,
      10,
    ),
    retryMaximumDelayMs: boundedIntegerEnvironment(
      env,
      "NANOCODEX_RELEASE_RETRY_MAXIMUM_DELAY_MS",
      maximumRetryDelayMs,
      250,
      120_000,
    ),
    leaseHeartbeatMs: boundedIntegerEnvironment(
      env,
      "NANOCODEX_RELEASE_LEASE_HEARTBEAT_MS",
      30_000,
      5_000,
      60_000,
    ),
    commandTimeoutMs: boundedIntegerEnvironment(
      env,
      "NANOCODEX_RELEASE_COMMAND_TIMEOUT_MS",
      45 * 60 * 1_000,
      60_000,
      2 * 60 * 60 * 1_000,
    ),
    distributionPollMs: boundedIntegerEnvironment(
      env,
      "NANOCODEX_RELEASE_DISTRIBUTION_POLL_MS",
      5_000,
      250,
      60_000,
    ),
    distributionTimeoutMs: boundedIntegerEnvironment(
      env,
      "NANOCODEX_RELEASE_DISTRIBUTION_TIMEOUT_MS",
      2 * 60 * 60 * 1_000,
      60_000,
      4 * 60 * 60 * 1_000,
    ),
    registryPollMs: boundedIntegerEnvironment(
      env,
      "NANOCODEX_RELEASE_REGISTRY_POLL_MS",
      10_000,
      250,
      60_000,
    ),
    registryTimeoutMs: boundedIntegerEnvironment(
      env,
      "NANOCODEX_RELEASE_REGISTRY_TIMEOUT_MS",
      5 * 60 * 1_000,
      10_000,
      30 * 60 * 1_000,
    ),
  };
}

async function releaseCommonDirectory(config, signal, secrets) {
  const { stdout } = await runProcess("git", [
    "rev-parse",
    "--path-format=absolute",
    "--git-common-dir",
  ], {
    cwd: config.repository,
    env: runtimeEnvironment(config.env),
    signal,
    timeoutMs: 10_000,
    secrets,
  });
  const directory = stdout.trimEnd();
  if (!directory.startsWith("/")) throw new Error("Git returned a non-absolute common directory");
  const info = await stat(directory);
  if (!info.isDirectory()) throw new Error("Git common directory is not a directory");
  return directory;
}

async function runProcess(command, args, {
  cwd,
  env,
  signal,
  timeoutMs = 60_000,
  secrets = [],
  acceptableExitCodes = [0],
} = {}) {
  throwIfAborted(signal);
  const child = spawn(command, args, {
    cwd,
    env,
    detached: process.platform !== "win32",
    stdio: ["ignore", "pipe", "pipe"],
  });
  const stdout = [];
  const stderr = [];
  let stdoutBytes = 0;
  let stderrBytes = 0;
  let outputExceeded = false;
  let aborted;
  const collect = (chunks, stream) => (chunk) => {
    if (stream === "stdout") stdoutBytes += chunk.length;
    else stderrBytes += chunk.length;
    if (stdoutBytes + stderrBytes > maximumProcessOutputBytes) {
      outputExceeded = true;
      terminateProcessGroup(child, "SIGKILL");
      return;
    }
    chunks.push(Buffer.from(chunk));
  };
  child.stdout.on("data", collect(stdout, "stdout"));
  child.stderr.on("data", collect(stderr, "stderr"));
  const abort = () => {
    aborted = signal?.reason ?? new DOMException("operation aborted", "AbortError");
    terminateProcessGroup(child, "SIGTERM");
  };
  signal?.addEventListener("abort", abort, { once: true });
  // EventTarget does not replay an abort that races with listener setup.
  // Register first, then re-check so every spawned process is cancellation-owned.
  if (signal?.aborted) abort();
  const timeout = setTimeout(() => {
    aborted = new DOMException(`process exceeded ${timeoutMs}ms`, "TimeoutError");
    terminateProcessGroup(child, "SIGTERM");
  }, timeoutMs);
  timeout.unref?.();
  const forceKill = setInterval(() => {
    if (aborted) terminateProcessGroup(child, "SIGKILL");
  }, 5_000);
  forceKill.unref?.();
  let result;
  try {
    result = await new Promise((resolveProcess, reject) => {
      child.once("error", reject);
      child.once("close", (code, closeSignal) => resolveProcess({ code, signal: closeSignal }));
    });
  } finally {
    clearTimeout(timeout);
    clearInterval(forceKill);
    signal?.removeEventListener("abort", abort);
  }
  if (aborted) throw aborted;
  if (outputExceeded) throw new Error(`${command} output exceeded ${maximumProcessOutputBytes} bytes`);
  const stdoutText = Buffer.concat(stdout, stdoutBytes).toString("utf8");
  const stderrText = Buffer.concat(stderr, stderrBytes).toString("utf8");
  if (!acceptableExitCodes.includes(result.code)) {
    const detail = redactSecrets(stderrText.trim() || stdoutText.trim(), secrets).slice(0, 4_000);
    const error = new Error(
      `${command} exited with ${result.code ?? result.signal}${detail ? `: ${detail}` : ""}`,
    );
    error.exitCode = result.code;
    throw error;
  }
  return { stdout: stdoutText, stderr: stderrText, exitCode: result.code };
}

function terminateProcessGroup(child, signal) {
  if (child.pid == null) return;
  try {
    if (process.platform === "win32") child.kill(signal);
    else process.kill(-child.pid, signal);
  } catch (cause) {
    if (cause?.code !== "ESRCH") throw cause;
  }
}

async function boundedFetch(url, init, parentSignal, timeoutMs) {
  throwIfAborted(parentSignal);
  const timeout = AbortSignal.timeout(timeoutMs);
  const signal = parentSignal ? AbortSignal.any([parentSignal, timeout]) : timeout;
  return fetch(url, { ...init, redirect: init?.redirect ?? "error", signal });
}

export function parseRetryAfter(value, {
  now = Date.now(),
  maximumMs = maximumRetryDelayMs,
} = {}) {
  if (!Number.isSafeInteger(now) || now < 0 || !Number.isSafeInteger(maximumMs) || maximumMs < 0) {
    throw new TypeError("Retry-After bounds are invalid");
  }
  if (typeof value !== "string" || value.trim() === "") return undefined;
  const text = value.trim();
  let milliseconds;
  if (/^(?:0|[1-9][0-9]*)$/.test(text)) {
    const seconds = Number(text);
    if (!Number.isSafeInteger(seconds)) return maximumMs;
    milliseconds = seconds * 1_000;
  } else {
    const date = Date.parse(text);
    if (!Number.isFinite(date)) return undefined;
    milliseconds = Math.max(0, date - now);
  }
  return Math.min(maximumMs, Math.max(0, milliseconds));
}

function retryableHttpStatus(status) {
  return retryableHttpStatuses.has(status) || status >= 500;
}

export async function retryReleaseOperation(operation, execute, {
  signal,
  maximumAttempts = maximumHttpAttempts,
  maximumDelayMs = maximumRetryDelayMs,
  delay = abortableDelay,
  now = Date.now,
  random = Math.random,
  onRetry,
} = {}) {
  if (typeof operation !== "string" || operation === "" || typeof execute !== "function") {
    throw new TypeError("retry operation requires a name and executable operation");
  }
  if (
    !Number.isSafeInteger(maximumAttempts) || maximumAttempts <= 0 ||
    maximumAttempts > 10 || !Number.isSafeInteger(maximumDelayMs) || maximumDelayMs < 0 ||
    typeof delay !== "function" || typeof now !== "function" || typeof random !== "function"
  ) throw new TypeError("retry operation policy is invalid");
  let lastCause;
  for (let attempt = 1; attempt <= maximumAttempts; attempt += 1) {
    throwIfAborted(signal);
    let outcome;
    try {
      outcome = await execute(attempt, signal);
    } catch (cause) {
      throwIfAborted(signal);
      if (
        cause instanceof ReleaseValidationError ||
        (cause instanceof ReleaseHttpError && !cause.retryable)
      ) throw cause;
      lastCause = cause;
      if (attempt === maximumAttempts) {
        throw new ReleaseHttpError(operation, {
          attempts: attempt,
          retryable: true,
          cause,
        });
      }
      const fallback = Math.min(maximumDelayMs, 250 * (2 ** (attempt - 1)));
      const wait = Math.round(fallback * (0.75 + Math.max(0, Math.min(1, random())) * 0.5));
      await onRetry?.({ attempt, wait, cause });
      await delay(wait, signal);
      continue;
    }
    if (!(outcome instanceof Response)) {
      return outcome;
    }
    responseAttemptCounts.set(outcome, attempt);
    if (!retryableHttpStatus(outcome.status)) return outcome;
    const retryAfterMs = parseRetryAfter(outcome.headers.get("retry-after"), {
      now: now(),
      maximumMs: maximumDelayMs,
    });
    if (attempt === maximumAttempts) return outcome;
    await outcome.body?.cancel().catch(() => undefined);
    const fallback = Math.min(maximumDelayMs, 250 * (2 ** (attempt - 1)));
    const wait = retryAfterMs ??
      Math.round(fallback * (0.75 + Math.max(0, Math.min(1, random())) * 0.5));
    await onRetry?.({ attempt, wait, status: outcome.status, retryAfterMs });
    await delay(wait, signal);
  }
  throw new ReleaseHttpError(operation, {
    attempts: maximumAttempts,
    retryable: true,
    cause: lastCause,
  });
}

async function retryingBufferedFetch(
  url,
  init,
  signal,
  config,
  operation,
  maximumBytes = maximumJsonBytes,
  timeoutMs = config.requestTimeoutMs,
) {
  const method = init?.method ?? "GET";
  if (!["GET", "HEAD"].includes(method)) {
    throw new TypeError(`automatic HTTP retry is not safe for ${method} ${operation}`);
  }
  if (!Number.isSafeInteger(maximumBytes) || maximumBytes <= 0) {
    throw new TypeError("buffered HTTP retry requires a positive byte bound");
  }
  return retryReleaseOperation(
    operation,
    async () => {
      const response = await boundedFetch(url, init, signal, timeoutMs);
      if (!response.ok) return response;
      const body = await readResponseBuffer(response, maximumBytes, operation);
      return new Response(body, {
        status: response.status,
        statusText: response.statusText,
        headers: response.headers,
      });
    },
    {
      signal,
      maximumAttempts: config.httpAttempts,
      maximumDelayMs: config.retryMaximumDelayMs,
    },
  );
}

async function retryingConsumedFetch(
  url,
  init,
  signal,
  config,
  operation,
  consume,
  timeoutMs = config.requestTimeoutMs,
) {
  const method = init?.method ?? "GET";
  if (!["GET", "HEAD"].includes(method)) {
    throw new TypeError(`automatic HTTP retry is not safe for ${method} ${operation}`);
  }
  if (typeof consume !== "function") {
    throw new TypeError("consumed HTTP retry requires a response consumer");
  }
  return retryReleaseOperation(
    operation,
    async () => {
      const response = await boundedFetch(url, init, signal, timeoutMs);
      if (!response.ok) return response;
      try {
        return await consume(response);
      } catch (cause) {
        await response.body?.cancel().catch(() => undefined);
        throw cause;
      }
    },
    {
      signal,
      maximumAttempts: config.httpAttempts,
      maximumDelayMs: config.retryMaximumDelayMs,
    },
  );
}

async function readResponseBuffer(response, maximumOrExpected, description, exact = false) {
  if (response.body == null) throw new Error(`${description} returned no body`);
  const reader = response.body.getReader();
  const chunks = [];
  let bytes = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      bytes += value.byteLength;
      if (bytes > maximumOrExpected) {
        await reader.cancel();
        throw new Error(`${description} exceeded ${maximumOrExpected} bytes`);
      }
      chunks.push(Buffer.from(value));
    }
  } finally {
    reader.releaseLock();
  }
  if (exact && bytes !== maximumOrExpected) {
    throw new Error(`${description} returned ${bytes} bytes; expected ${maximumOrExpected}`);
  }
  return Buffer.concat(chunks, bytes);
}

async function readJsonResponse(response, description) {
  const body = await readResponseBuffer(response, maximumJsonBytes, description);
  try {
    return JSON.parse(body.toString("utf8"));
  } catch (cause) {
    throw new Error(`${description} returned invalid JSON`, { cause });
  }
}

async function responseFailure(
  response,
  operation,
  secrets,
  attempts = responseAttemptCounts.get(response) ?? 1,
) {
  let detail = "";
  try {
    detail = (await readResponseBuffer(response, maximumErrorBytes, operation)).toString("utf8");
  } catch {
    await response.body?.cancel().catch(() => undefined);
  }
  return new ReleaseHttpError(operation, {
    status: response.status,
    attempts,
    retryable: retryableHttpStatus(response.status),
    retryAfterMs: parseRetryAfter(response.headers.get("retry-after")),
    detail: detail ? redactSecrets(detail, secrets) : undefined,
  });
}

function parseOrigin(value) {
  try {
    const url = new URL(value);
    if (url.username || url.password || !["http:", "https:"].includes(url.protocol)) {
      throw new Error("unsupported URL");
    }
    const loopback = url.hostname === "localhost" || url.hostname.endsWith(".localhost") ||
      url.hostname === "127.0.0.1" || url.hostname === "[::1]";
    if (url.protocol !== "https:" && !loopback) throw new Error("HTTPS is required");
    url.pathname = url.pathname.replace(/\/+$/, "");
    url.search = "";
    url.hash = "";
    return url.toString().replace(/\/$/, "");
  } catch (cause) {
    throw new Error("release origins must use HTTPS (HTTP is allowed only for loopback)", {
      cause,
    });
  }
}

function requiredEnvironment(env, name) {
  const value = env[name]?.trim();
  if (!value) throw new Error(`${name} is required`);
  return value;
}

function optionalEnvironment(env, name) {
  const value = env[name]?.trim();
  return value || undefined;
}

function boundedIntegerEnvironment(env, name, fallback, minimum, maximum) {
  const raw = env[name]?.trim();
  if (!raw) return fallback;
  if (!/^[1-9][0-9]*$/.test(raw)) throw new Error(`${name} must be a positive integer`);
  const value = Number(raw);
  if (!Number.isSafeInteger(value) || value < minimum || value > maximum) {
    throw new Error(`${name} must be between ${minimum} and ${maximum}`);
  }
  return value;
}

function environmentSecrets(env, extra = []) {
  const values = Object.entries(env)
    .filter(([name, value]) =>
      /(?:TOKEN|SECRET|PASSWORD|API_KEY|PRIVATE_KEY)/i.test(name) &&
      typeof value === "string" && value !== ""
    )
    .flatMap(([, value]) => [value, value.trim()]);
  return [...new Set([...values, ...extra].filter((value) =>
    typeof value === "string" && value !== ""
  ))];
}

export function boundedStructuredError(cause, secrets) {
  const budget = { nodes: maximumStructuredErrorNodes };
  const value = structuredErrorNode(cause, secrets, budget, 0);
  if (Buffer.byteLength(JSON.stringify(value)) <= maximumStructuredErrorBytes) return value;
  return {
    name: value.name,
    message: value.message,
    truncated: true,
  };
}

function structuredErrorNode(cause, secrets, budget, depth) {
  if (depth >= 5 || budget.nodes <= 0) {
    return { name: "Error", message: "nested error evidence truncated", truncated: true };
  }
  budget.nodes -= 1;
  const error = cause instanceof Error ? cause : new Error(String(cause));
  const errorName = typeof error.name === "string" && error.name ? error.name : "Error";
  const errorMessage = typeof error.message === "string"
    ? error.message
    : String(error.message ?? "");
  const value = {
    name: redactSecrets(errorName.slice(0, 640), secrets).slice(0, 160),
    message: redactSecrets(errorMessage.slice(0, 8_000), secrets).slice(0, 2_000),
  };
  for (const name of [
    "operation",
    "phase",
    "status",
    "attempts",
    "retryable",
    "retryAfterMs",
    "exitCode",
  ]) {
    const field = error[name];
    if (typeof field === "string") {
      value[name] = redactSecrets(field.slice(0, 2_000), secrets).slice(0, 500);
    }
    else if (typeof field === "number" || typeof field === "boolean") value[name] = field;
  }
  if (error instanceof AggregateError) {
    value.errors = [];
    let count = 0;
    for (const nested of error.errors) {
      if (count >= 8) break;
      count += 1;
      if (budget.nodes <= 0) {
        value.errors.push({
          name: "Error",
          message: "aggregate error evidence truncated",
          truncated: true,
        });
        break;
      }
      value.errors.push(structuredErrorNode(nested, secrets, budget, depth + 1));
    }
  }
  if (error.cause !== undefined && budget.nodes > 0) {
    value.cause = structuredErrorNode(error.cause, secrets, budget, depth + 1);
  } else if (error.cause !== undefined) {
    value.cause = {
      name: "Error",
      message: "causal error evidence truncated",
      truncated: true,
    };
  }
  return value;
}

function throwIfAborted(signal) {
  if (signal?.aborted) throw signal.reason ?? new DOMException("operation aborted", "AbortError");
}

function abortableDelay(milliseconds, signal) {
  throwIfAborted(signal);
  return new Promise((resolveDelay, reject) => {
    let timer;
    const abort = () => {
      clearTimeout(timer);
      reject(signal.reason ?? new DOMException("operation aborted", "AbortError"));
    };
    timer = setTimeout(() => {
      signal?.removeEventListener("abort", abort);
      resolveDelay();
    }, milliseconds);
    signal?.addEventListener("abort", abort, { once: true });
  });
}

async function createReleaseOperations(config, release, report, secrets) {
  const temporaryDirectories = new Set();
  const git = (args, signal, options = {}) => runProcess("git", [
    "-c", "core.fsmonitor=false",
    "-c", "core.hooksPath=/dev/null",
    "-c", "core.attributesFile=/dev/null",
    "-c", "core.autocrlf=false",
    ...args,
  ], {
    cwd: config.repository,
    env: runtimeEnvironment(config.env),
    timeoutMs: 30_000,
    secrets,
    signal,
    ...options,
  });
  const readRemoteTag = (phase, signal) => report.run(
    `${phase}: authoritative tag`,
    async () => {
      const { stdout } = await git([
        "ls-remote",
        authoritativeRepositoryUrl,
        `refs/tags/${release.tag}`,
        `refs/tags/${release.tag}^{}`,
      ], signal);
      return parseRemoteTagRefs(stdout, release.tag);
    },
    ({ object, commit, annotated }) => ({ object, commit, annotated }),
  );

  const inspectCheckout = (binding, phase, signal) => report.run(
    `${phase}: exact clean checkout`,
    async () => {
      const [head, tagObject, tagCommit, statusValue, symbolic] = await Promise.all([
        git(["rev-parse", "--verify", "HEAD^{commit}"], signal),
        git(["rev-parse", "--verify", `refs/tags/${release.tag}`], signal),
        git(["rev-parse", "--verify", `refs/tags/${release.tag}^{commit}`], signal),
        git(["status", "--porcelain=v1", "-z", "--untracked-files=all"], signal),
        git(["symbolic-ref", "--quiet", "HEAD"], signal, { acceptableExitCodes: [0, 1] }),
      ]);
      const state = {
        ref: symbolic.exitCode === 1 ? "HEAD" : symbolic.stdout.trimEnd(),
        head: head.stdout.trimEnd(),
        tagObject: tagObject.stdout.trimEnd(),
        tagCommit: tagCommit.stdout.trimEnd(),
        status: statusValue.stdout,
      };
      assertReleaseCheckoutState(state, binding);
      return state;
    },
    ({ head, ref }) => ({ head, ref, clean: true }),
  );

  const readGreenMaster = (head, phase, signal) => report.run(
    `${phase}: retained green master run`,
    async () => {
      const masterUrl = new URL(
        `/api/ci/source/master/publications/${head}`,
        config.ciOrigin,
      );
      const runUrl = new URL(`/api/ci/runs/${head}`, config.ciOrigin);
      const [masterResponse, runResponse] = await Promise.all([
        retryingBufferedFetch(masterUrl, {
          headers: releaseRequestHeaders(config.releaseToken),
        }, signal, config, "retained master publication"),
        retryingBufferedFetch(runUrl, {
          headers: { accept: "application/json" },
        }, signal, config, "retained Cloudflare run"),
      ]);
      if (!masterResponse.ok) {
        await runResponse.body?.cancel().catch(() => undefined);
        throw await responseFailure(masterResponse, "retained master publication", secrets);
      }
      if (!runResponse.ok) {
        await masterResponse.body?.cancel().catch(() => undefined);
        throw await responseFailure(runResponse, "retained Cloudflare run", secrets);
      }
      const [masterValue, runValue] = await Promise.all([
        readJsonResponse(masterResponse, "retained master publication"),
        readJsonResponse(runResponse, "retained Cloudflare run"),
      ]);
      const evidence = parseGreenMasterEvidence(masterValue, runValue, head);
      const npmArtifact = selectTestedNpmArtifact(evidence.run, release.tag);
      return { evidence, npmArtifact };
    },
    ({ evidence, npmArtifact }) => ({
      head,
      lane: evidence.master.publication.lane.type,
      workflowId: evidence.run.value.workflowId,
      outcome: evidence.run.outcome,
      testedNpm: {
        size: npmArtifact.size,
        sha256: npmArtifact.sha256,
        key: npmArtifact.key,
      },
    }),
  );

  const verifyTrust = async (phase, signal) => {
    const binding = await readRemoteTag(phase, signal);
    await inspectCheckout(binding, phase, signal);
    const green = await readGreenMaster(binding.commit, phase, signal);
    return { binding, ...green };
  };

  const assertTrust = async (expected, phase, signal) => {
    const observed = await verifyTrust(phase, signal);
    assertTagBinding(expected.binding, observed.binding, phase);
    if (
      canonicalJson(expected.npmArtifact) !== canonicalJson(observed.npmArtifact) ||
      canonicalJson(expected.evidence.master.publication) !==
        canonicalJson(observed.evidence.master.publication) ||
      expected.evidence.master.run.workflowId !== observed.evidence.master.run.workflowId
    ) throw new Error(`retained green master evidence changed during ${phase}`);
    return observed;
  };

  const postStage = (trust, signal) => report.run(
    "stage: authenticated exact-head request",
    async () => {
      let lastCause;
      for (let attempt = 1; attempt <= config.httpAttempts; attempt += 1) {
        let response;
        try {
          response = await boundedFetch(
            new URL(`/api/ci/releases/stable/${release.tag}`, config.ciOrigin),
            {
              method: "POST",
              headers: releaseRequestHeaders(config.releaseToken, { json: true }),
              body: JSON.stringify({ head: trust.binding.commit }),
            },
            signal,
            config.requestTimeoutMs,
          );
          if (response.ok) {
            try {
              const value = await readJsonResponse(response, "stage stable release");
              if (!isRecord(value) || !["accepted", "ready", "released"].includes(value.status)) {
                throw new Error("stage stable release returned an invalid status");
              }
              return value.status === "accepted"
                ? parseAcceptedStableRelease(value, release.tag, trust.binding.commit)
                : value;
            } catch (cause) {
              // A successful POST may commit before its response body is lost,
              // truncated, or otherwise unusable. Only authoritative exact
              // distribution state below may acknowledge that attempt.
              lastCause = cause;
            }
          } else if (!retryableHttpStatus(response.status)) {
            throw await responseFailure(response, "stage stable release", secrets, attempt);
          } else {
            lastCause = await responseFailure(
              response,
              "stage stable release",
              secrets,
              attempt,
            );
          }
        } catch (cause) {
          throwIfAborted(signal);
          if (cause instanceof ReleaseHttpError && !cause.retryable) throw cause;
          lastCause = cause;
        }

        // Reconcile the deterministic distribution identity before the exact
        // POST is ever resubmitted after an ambiguous acknowledgement.
        const acknowledged = await readDistribution(trust, signal, true, undefined, true);
        if (acknowledged) {
          return acknowledged.status === "running"
            ? acknowledged
            : { status: acknowledged.status, distribution: acknowledged };
        }
        if (attempt === config.httpAttempts) {
          throw new ReleaseHttpError("stage stable release", {
            attempts: attempt,
            retryable: true,
            cause: lastCause,
          });
        }
        const retryAfterMs = lastCause instanceof ReleaseHttpError
          ? lastCause.retryAfterMs
          : undefined;
        await abortableDelay(
          retryAfterMs ?? Math.min(
            config.retryMaximumDelayMs,
            250 * (2 ** (attempt - 1)),
          ),
          signal,
        );
      }
      throw lastCause;
    },
    (value) => ({
      status: value.status,
      head: trust.binding.commit,
      ...(value.requestId === undefined ? {} : { requestId: value.requestId }),
    }),
  );

  const readDistribution = async (
    trust,
    signal,
    allowMissing,
    expectedRequestId,
    returnRunning = false,
  ) => {
    const response = await retryingBufferedFetch(
      new URL(`/api/ci/distributions/stable/${release.tag}`, config.ciOrigin),
      { headers: { accept: "application/json" } },
      signal,
      config,
      "stable distribution evidence",
    );
    if (response.status === 404 && allowMissing) {
      await response.body?.cancel().catch(() => undefined);
      return undefined;
    }
    if (!response.ok) throw await responseFailure(response, "stable distribution evidence", secrets);
    const value = await readJsonResponse(response, "stable distribution evidence");
    if (isRecord(value) && ["ready", "success"].includes(value.status)) {
      return parseReadyDistribution(
        value,
        release.tag,
        trust.binding.commit,
        trust.npmArtifact,
      );
    }
    if (
      !isRecord(value) || value.version !== 1 || value.channel !== "stable" ||
      value.tagName !== release.tag || value.head !== trust.binding.commit ||
      value.workflowId !== `release-${release.tag}-${trust.binding.commit}`
    ) throw new Error("stable distribution evidence targets the wrong release");
    if (value.status === "failure") {
      const detail = isRecord(value.failure) && typeof value.failure.message === "string"
        ? `: ${value.failure.message.slice(0, 500)}`
        : "";
      throw new Error(`stable distribution failed${detail}`);
    }
    if (value.status !== "running") {
      throw new Error(`stable distribution returned unsupported status ${String(value.status)}`);
    }
    const running = parseRunningStableRelease(
      value,
      release.tag,
      trust.binding.commit,
      expectedRequestId,
    );
    return returnRunning ? running : undefined;
  };

  const latestPointerId = async (signal) => {
    const response = await retryingBufferedFetch(
      new URL("/api/releases/channels/latest", config.publicOrigin),
      { headers: { accept: "application/json" } },
      signal,
      config,
      "latest release channel",
    );
    if (response.status === 404) {
      await response.body?.cancel().catch(() => undefined);
      return null;
    }
    if (!response.ok) throw await responseFailure(response, "latest release channel", secrets);
    const value = await readJsonResponse(response, "latest release channel");
    if (!isRecord(value) || !isRecord(value.pointer) || typeof value.pointer.id !== "string") {
      throw new Error("latest release channel returned an invalid pointer");
    }
    parseStableTag(value.pointer.id);
    return value.pointer.id;
  };

  const verifyReadyChannel = async (ready, signal) => {
    const observed = await latestPointerId(signal);
    const expected = ready.status === "success" ? ready.tag : ready.draft.expectedChannel;
    if (observed !== expected) {
      throw new Error(
        `reviewed draft expected latest ${String(expected)}, observed ${String(observed)}`,
      );
    }
    return observed;
  };

  const waitReady = (trust, signal, requestId) => report.run(
    "stage: ready distribution draft",
    async () => {
      const deadline = Date.now() + config.distributionTimeoutMs;
      while (true) {
        const ready = await readDistribution(trust, signal, true, requestId);
        if (ready) {
          await verifyReadyChannel(ready, signal);
          return ready;
        }
        if (Date.now() >= deadline) throw new Error("stable distribution did not become ready in time");
        await abortableDelay(
          Math.min(config.distributionPollMs, Math.max(1, deadline - Date.now())),
          signal,
        );
      }
    },
    (ready) => readySummary(ready),
  );

  const readReady = (trust, signal) => report.run(
    "publish: reviewed ready distribution draft",
    async () => {
      const ready = await readDistribution(trust, signal, false);
      if (!ready) throw new Error("stable distribution is not ready; run stage first");
      await verifyReadyChannel(ready, signal);
      return ready;
    },
    (ready) => readySummary(ready),
  );

  const assertReady = (expected, phase, signal) => report.run(
    `${phase}: immutable reviewed draft`,
    async () => {
      const trust = {
        binding: { commit: expected.head },
        npmArtifact: expected.npm,
      };
      const observed = await readDistribution(trust, signal, false);
      if (
        !observed || observed.status !== expected.status ||
        canonicalJson(observed.draft) !== canonicalJson(expected.draft)
      ) throw new Error(`reviewed stable draft changed during ${phase}`);
      await verifyReadyChannel(observed, signal);
      return observed;
    },
    (ready) => ({ status: ready.status, head: ready.head, assetCount: ready.draft.assets.length }),
  );

  const stage = async (trust, signal) => {
    const accepted = await postStage(trust, signal);
    const ready = await waitReady(trust, signal, accepted.requestId);
    return accepted.requestId === undefined
      ? ready
      : Object.freeze({ ...ready, requestId: accepted.requestId });
  };

  const readDraftDescriptor = async (ready, signal) => {
    const response = await retryingBufferedFetch(
      new URL(`/api/releases/drafts/stable/${ready.tag}`, config.ciOrigin),
      { headers: releaseRequestHeaders(config.releaseToken) },
      signal,
      config,
      "authenticated stable draft descriptor",
    );
    if (!response.ok) {
      throw await responseFailure(response, "authenticated stable draft descriptor", secrets);
    }
    const declaredSize = parseCanonicalInteger(response.headers.get("content-length"));
    if (
      declaredSize == null || declaredSize <= 0 || declaredSize > maximumJsonBytes ||
      response.headers.get("cache-control") !== "no-store" ||
      response.headers.get("content-type") !== "application/json; charset=utf-8" ||
      response.headers.get("x-content-type-options") !== "nosniff" ||
      response.headers.has("content-encoding") || response.headers.has("content-range")
    ) throw new Error("authenticated stable draft descriptor headers are invalid");
    return parseDraftStableDescriptor(
      await readJsonResponse(response, "authenticated stable draft descriptor"),
      ready,
    );
  };

  const downloadAssetBytes = async (
    response,
    asset,
    description,
    { retain = false } = {},
  ) => {
    if (response.body == null) throw new Error(`${description} returned no body`);
    const reader = response.body.getReader();
    const hash = createHash("sha256");
    const chunks = retain ? [] : undefined;
    let bytes = 0;
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        bytes += value.byteLength;
        if (bytes > asset.size) {
          await reader.cancel();
          throw new ReleaseValidationError(`${description} exceeded ${asset.size} bytes`);
        }
        hash.update(value);
        if (chunks) chunks.push(Buffer.from(value));
      }
    } finally {
      reader.releaseLock();
    }
    if (bytes !== asset.size) {
      throw new Error(`${description} returned ${bytes} bytes; expected ${asset.size}`);
    }
    const sha256 = hash.digest("hex");
    if (sha256 !== asset.sha256) {
      throw new ReleaseValidationError(
        `${description} SHA-256 differs from the reviewed descriptor`,
      );
    }
    return {
      proof: Object.freeze({ ...asset }),
      ...(chunks ? { bytes: Buffer.concat(chunks, bytes) } : {}),
    };
  };

  const validateDraftAssetHeaders = (response, draft, asset) => {
    if (
      response.headers.get("cache-control") !== "no-store" ||
      response.headers.get("content-location") !== asset.downloadPath ||
      response.headers.get("content-disposition") !== `attachment; filename="${asset.name}"` ||
      parseCanonicalInteger(response.headers.get("content-length")) !== asset.size ||
      response.headers.get("content-type") !== asset.contentType ||
      response.headers.get("etag") !== `"${asset.sha256}"` ||
      response.headers.get("x-content-type-options") !== "nosniff" ||
      response.headers.get("x-nanocodex-release") !== draft.id ||
      response.headers.get("x-nanocodex-sha256") !== asset.sha256 ||
      response.headers.has("content-encoding") || response.headers.has("content-range")
    ) throw new ReleaseValidationError(
      `authenticated draft asset headers are invalid for ${asset.name}`,
    );
  };

  const writeNpmPackage = async (asset, bytes, tag) => {
    const metadata = verifyNpmPackageBytes(bytes, asset, tag);
    const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-stable-release-"));
    temporaryDirectories.add(directory);
    const path = resolve(directory, asset.name);
    const handle = await open(
      path,
      fsConstants.O_CREAT | fsConstants.O_EXCL | fsConstants.O_WRONLY |
        (fsConstants.O_NOFOLLOW ?? 0),
      0o600,
    );
    try {
      await handle.writeFile(bytes);
      await handle.sync();
    } finally {
      await handle.close();
    }
    return Object.freeze({ path, directory, metadata });
  };

  const downloadDraftAssets = async (draft, signal) => {
    const proofs = [];
    let npmPackage;
    for (const asset of draft.assets) {
      const operation = `authenticated draft asset ${asset.name}`;
      const downloaded = await retryingConsumedFetch(
        new URL(asset.downloadPath, config.ciOrigin),
        {
          headers: releaseRequestHeaders(config.releaseToken),
        },
        signal,
        config,
        operation,
        async (response) => {
          validateDraftAssetHeaders(response, draft, asset);
          return downloadAssetBytes(
            response,
            asset,
            operation,
            { retain: asset.platform === "npm" },
          );
        },
        config.assetTimeoutMs,
      );
      if (downloaded instanceof Response) {
        throw await responseFailure(
          downloaded,
          operation,
          secrets,
        );
      }
      proofs.push(downloaded.proof);
      if (asset.platform === "npm") {
        npmPackage = await writeNpmPackage(asset, downloaded.bytes, draft.tag);
      }
    }
    if (!npmPackage || proofs.length !== draft.assets.length) {
      throw new Error("authenticated stable draft did not yield every exact asset");
    }
    return Object.freeze({ assets: Object.freeze(proofs), npmPackage });
  };

  const inspectToolchain = async (signal) => {
    const specifications = [
      ["cargo", ["--version", "--verbose"]],
      ["rustc", ["--version", "--verbose"]],
      ["node", ["--version"]],
      ["npm", ["--version"]],
    ];
    const entries = await Promise.all(specifications.map(async ([command, args]) => {
      const { stdout } = await runProcess(command, args, {
        cwd: config.repository,
        env: runtimeEnvironment(config.env),
        signal,
        timeoutMs: 30_000,
        secrets,
      });
      const version = stdout.trimEnd();
      if (version === "" || Buffer.byteLength(version) > maximumToolchainVersionBytes) {
        throw new Error(`${command} returned an invalid version fingerprint`);
      }
      return [command, version];
    }));
    return normalizeToolchain(Object.fromEntries(entries));
  };

  const rehashCratePackages = async (packages, phase) => Promise.all(
    validateLocalCratePackages(packages, release.version).map((localPackage) =>
      inspectMatchingCratePackage(localPackage.path, localPackage, phase)
    ),
  );

  const reviewPlan = (ready, packages, signal) => report.run(
    `${ready.status === "ready" ? "review" : "replay"}: reproduce complete staged plan`,
    async () => {
      const [draft, toolchain, reviewedCrates] = await Promise.all([
        readDraftDescriptor(ready, signal),
        inspectToolchain(signal),
        rehashCratePackages(packages, "reviewed plan reproduction"),
      ]);
      const downloaded = await downloadDraftAssets(draft, signal);
      const plan = createReviewedReleasePlan({
        ready,
        draft,
        crates: reviewedCrates,
        npm: downloaded.npmPackage.metadata,
        assets: downloaded.assets,
        toolchain,
      });
      return { plan, npmPackage: downloaded.npmPackage };
    },
    ({ plan }) => ({
      planSha256: plan.planSha256,
      draftSha256: plan.draftSha256,
      manifestIdentitySha256: plan.manifestIdentitySha256,
      crateCount: plan.crates.length,
      assetCount: plan.assets.length,
      toolchain: plan.toolchain,
    }),
  );

  const readPublishedManifest = async (ready, signal, allowMissing) => {
    const response = await retryingBufferedFetch(
      new URL(`/api/releases/releases/stable/${ready.tag}`, config.publicOrigin),
      { headers: { accept: "application/json" } },
      signal,
      config,
      "immutable stable manifest",
    );
    if (response.status === 404 && allowMissing) {
      await response.body?.cancel().catch(() => undefined);
      return undefined;
    }
    if (!response.ok) throw await responseFailure(response, "immutable stable manifest", secrets);
    const declaredSize = parseCanonicalInteger(response.headers.get("content-length"));
    if (
      declaredSize == null || declaredSize <= 0 || declaredSize > maximumJsonBytes ||
      response.headers.get("content-type") !== "application/json; charset=utf-8" ||
      response.headers.get("x-content-type-options") !== "nosniff" ||
      !/(?:^|,)\s*immutable(?:,|$)/.test(response.headers.get("cache-control") ?? "") ||
      response.headers.has("content-encoding") || response.headers.has("content-range")
    ) throw new Error("immutable stable manifest response headers are invalid");
    const manifest = parsePublicStableManifest(
      await readJsonResponse(response, "immutable stable manifest"),
      ready,
    );
    if (response.headers.get("etag") !== `"${manifest.manifestSha256}"`) {
      throw new Error("immutable stable manifest ETag does not match its canonical SHA-256");
    }
    return manifest;
  };

  const readPublished = (ready, signal) => report.run(
    "publish: existing immutable release",
    () => readPublishedManifest(ready, signal, true),
    (manifest) => manifest
      ? { status: "published", manifestSha256: manifest.manifestSha256 }
      : { status: "absent" },
  );

  const readLocalCrateNames = async (signal) => {
    const { stdout } = await runProcess("./scripts/release-crates.sh", ["names"], {
      cwd: config.repository,
      env: runtimeEnvironment(config.env),
      signal,
      timeoutMs: 30_000,
      secrets,
    });
    const names = stdout.trim().split("\n").filter(Boolean);
    if (canonicalJson(names) !== canonicalJson(releaseCrateNames)) {
      throw new Error("tagged source does not declare the expected eight public crates");
    }
    return names;
  };

  const packageCrates = (trust, signal) => report.run(
    "prepare: package all eight crates from exact tagged source",
    async () => {
      if (!isRecord(trust) || !isTagBinding(trust.binding)) {
        throw new Error("trusted tag binding is required before Cargo packaging");
      }
      const names = await readLocalCrateNames(signal);
      const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-cargo-packages-"));
      temporaryDirectories.add(directory);
      const targetDirectory = resolve(directory, "target");
      const cargoHome = resolve(directory, "cargo-home");
      await mkdir(cargoHome, { mode: 0o700 });
      const packageEnvironment = {
        ...runtimeEnvironment(config.env),
        CARGO_HOME: cargoHome,
      };
      const packages = [];
      for (const crate of names) {
        await runProcess("cargo", [
          "package",
          "--locked",
          "--no-verify",
          "--config",
          ".cargo/release.toml",
          "--registry",
          "crates-io",
          "--package",
          crate,
          "--target-dir",
          targetDirectory,
        ], {
          cwd: config.repository,
          env: packageEnvironment,
          signal,
          timeoutMs: config.commandTimeoutMs,
          secrets,
        });
        const archiveName = `${crate}-${release.version}.crate`;
        const localPackage = await inspectLocalCratePackage(
          resolve(targetDirectory, "package", archiveName),
          crate,
          release.version,
        );
        // Exercise Cargo's exact live publication packaging path without a
        // credential or upload. This catches argument/config repackage drift
        // for every crate before any registry or release-ledger mutation.
        await runProcess("cargo", cargoPublishArguments(crate, targetDirectory, {
          dryRun: true,
        }), {
          cwd: config.repository,
          env: packageEnvironment,
          signal,
          timeoutMs: config.commandTimeoutMs,
          secrets,
        });
        await verifyCargoRepackage(localPackage, "dry-run repackage");
        packages.push(localPackage);
      }
      await inspectCheckout(trust.binding, "after local Cargo packaging", signal);
      return validateLocalCratePackages(packages, release.version);
    },
    (packages) => ({
      count: packages.length,
      crates: packages.map(({ crate, version, size, sha256 }) => ({
        crate,
        version,
        size,
        sha256,
      })),
    }),
  );

  const fetchCrateRecord = async (crate, signal) => {
    const response = await retryingBufferedFetch(
      `${cratesRegistryOrigin}/api/v1/crates/${crate}/${release.version}`,
      { headers: { accept: "application/json", "user-agent": `nanocodex-release/${release.version}` } },
      signal,
      config,
      `crates.io ${crate}`,
    );
    if (response.status === 404) {
      await response.body?.cancel().catch(() => undefined);
      return undefined;
    }
    if (!response.ok) throw await responseFailure(response, `crates.io ${crate}`, secrets);
    return parseCrateRegistryVersion(
      await readJsonResponse(response, `crates.io ${crate}`),
      crate,
      release.version,
    );
  };

  const verifyCrateRecords = async (packages, records, signal) => {
    const localPackages = validateLocalCratePackages(packages, release.version);
    const reconciled = reconcileCrateRegistryVersions(localPackages, records);
    const verified = [];
    for (let index = 0; index < reconciled.length; index += 1) {
      const record = reconciled[index];
      if (!record) continue;
      const localPackage = localPackages[index];
      const operation = `download ${record.crate}`;
      const downloaded = await retryingConsumedFetch(
        record.downloadUrl,
        {
          headers: {
            accept: "application/octet-stream",
            "user-agent": `nanocodex-release/${release.version}`,
          },
          redirect: "follow",
        },
        signal,
        config,
        operation,
        async (response) => {
          if (
            response.headers.has("content-encoding") ||
            response.headers.has("content-range") ||
            (response.headers.has("content-length") &&
              parseCanonicalInteger(response.headers.get("content-length")) !==
                localPackage.size)
          ) throw new ReleaseValidationError(
            `crates.io package headers are invalid for ${record.crate}`,
          );
          return downloadAssetBytes(
            response,
            { size: localPackage.size, sha256: localPackage.sha256 },
            `crates.io ${record.crate} package`,
          );
        },
        config.assetTimeoutMs,
      );
      if (downloaded instanceof Response) {
        throw await responseFailure(downloaded, operation, secrets);
      }
      verified.push(Object.freeze({
        crate: record.crate,
        version: release.version,
        size: localPackage.size,
        sha256: localPackage.sha256,
      }));
    }
    return Object.freeze(verified);
  };

  const verifyExistingCrates = (packages, signal) => report.run(
    "prepare: reconcile existing crates.io API and package bytes",
    async () => {
      const localPackages = validateLocalCratePackages(packages, release.version);
      const records = await Promise.all(localPackages.map(({ crate }) =>
        fetchCrateRecord(crate, signal)
      ));
      const reconciled = reconcileCrateRegistryVersions(localPackages, records);
      await verifyCrateRecords(localPackages, reconciled, signal);
      return reconciled;
    },
    (records) => ({
      existing: records.flatMap((record) => record
        ? [{ crate: record.crate, sha256: record.checksum }]
        : []),
    }),
  );

  const waitForCrate = async (localPackage, signal) => {
    const deadline = Date.now() + config.registryTimeoutMs;
    while (true) {
      const record = await fetchCrateRecord(localPackage.crate, signal);
      if (record) {
        assertCrateRegistryChecksum(localPackage, record);
        return record;
      }
      if (Date.now() >= deadline) {
        throw new Error(
          `crates.io did not publish ${localPackage.crate}@${localPackage.version}`,
        );
      }
      await abortableDelay(
        Math.min(config.registryPollMs, Math.max(1, deadline - Date.now())),
        signal,
      );
    }
  };

  const waitForCrates = async (packages, signal) => {
    const localPackages = validateLocalCratePackages(packages, release.version);
    const deadline = Date.now() + config.registryTimeoutMs;
    let records;
    while (true) {
      records = await Promise.all(localPackages.map(({ crate }) =>
        fetchCrateRecord(crate, signal)
      ));
      reconcileCrateRegistryVersions(localPackages, records);
      if (records.every(Boolean)) break;
      if (Date.now() >= deadline) {
        reconcileCrateRegistryVersions(localPackages, records, { requireAll: true });
      }
      await abortableDelay(
        Math.min(config.registryPollMs, Math.max(1, deadline - Date.now())),
        signal,
      );
    }
    const registryRecords = reconcileCrateRegistryVersions(
      localPackages,
      records,
      { requireAll: true },
    );
    return verifyCrateRecords(localPackages, registryRecords, signal);
  };

  const publishCrates = (ready, packages, authority, signal) => report.run(
    "publish: eight locally hashed crates in dependency order",
    async () => {
      if (ready.head !== (await git(["rev-parse", "--verify", "HEAD^{commit}"], signal)).stdout.trimEnd()) {
        throw new Error("checkout changed before crates.io publication");
      }
      return publishCratePackages(packages, {
        readVersion: ({ crate }, childSignal) => fetchCrateRecord(crate, childSignal),
        beforePublish: (localPackage, _attempt, childSignal) =>
          authority.checkpoint(`before publishing ${localPackage.crate}@${localPackage.version}`, childSignal),
        publishVersion: async (localPackage, _attempt, childSignal) => {
          const packageDirectory = dirname(localPackage.path);
          const targetDirectory = dirname(packageDirectory);
          const cargoHome = resolve(dirname(targetDirectory), "cargo-home");
          let result;
          let commandFailure;
          try {
            result = await runProcess(
              "cargo",
              cargoPublishArguments(localPackage.crate, targetDirectory),
              {
                cwd: config.repository,
                env: cargoPublicationEnvironment(config.env, { cargoHome }),
                signal: childSignal,
                timeoutMs: config.commandTimeoutMs,
                secrets,
              },
            );
          } catch (cause) {
            commandFailure = cause;
          }
          await verifyCargoRepackage(localPackage, "live publish repackage");
          if (commandFailure) throw commandFailure;
          return result;
        },
        waitVersion: (localPackage, childSignal) =>
          waitForCrate(localPackage, childSignal),
        retryDelay: (attempt, childSignal) => abortableDelay(attempt * 10_000, childSignal),
      }, {
        signal,
      });
    },
    (result) => result,
  );

  const verifyCrates = (packages, signal) => report.run(
    "verify: crates.io API and package bytes match local packages",
    () => waitForCrates(packages, signal),
    (crates) => ({ count: crates.length, crates }),
  );

  const readNpmVersion = async (metadata, signal) => {
    const response = await retryingBufferedFetch(
      `${npmRegistryOrigin}/nanocodex/${metadata.version}`,
      { headers: { accept: "application/json", "user-agent": `nanocodex-release/${metadata.version}` } },
      signal,
      config,
      "npm registry version",
    );
    if (response.status === 404) {
      await response.body?.cancel().catch(() => undefined);
      return undefined;
    }
    if (!response.ok) throw await responseFailure(response, "npm registry version", secrets);
    return parseNpmRegistryVersion(
      await readJsonResponse(response, "npm registry version"),
      metadata,
    );
  };

  const verifyNpmRecord = async (record, metadata, signal) => {
    const result = await retryingConsumedFetch(
      record.tarball,
      {
        headers: {
          accept: "application/octet-stream",
          "user-agent": `nanocodex-release/${metadata.version}`,
        },
      },
      signal,
      config,
      "npm registry tarball",
      (response) => verifyNpmRegistryTarballResponse(response, record, metadata),
      config.assetTimeoutMs,
    );
    if (result instanceof Response) {
      throw await responseFailure(result, "npm registry tarball", secrets);
    }
    return result;
  };

  const waitForNpm = async (metadata, signal) => {
    const deadline = Date.now() + config.registryTimeoutMs;
    while (true) {
      const record = await readNpmVersion(metadata, signal);
      if (record) {
        try {
          return await verifyNpmRecord(record, metadata, signal);
        } catch (cause) {
          if (
            !(cause instanceof ReleaseHttpError) ||
            (!cause.retryable && cause.status !== 404)
          ) throw cause;
        }
      }
      if (Date.now() >= deadline) {
        throw new Error(
          `npm did not publish byte-exact nanocodex@${metadata.version} in time`,
        );
      }
      await abortableDelay(
        Math.min(config.registryPollMs, Math.max(1, deadline - Date.now())),
        signal,
      );
    }
  };

  const inspectNpmRegistry = (npmPackage, signal) => report.run(
    "prepare: reconcile npm metadata and downloadable package bytes",
    async () => {
      const record = await readNpmVersion(npmPackage.metadata, signal);
      if (!record) return undefined;
      return verifyNpmRecord(record, npmPackage.metadata, signal);
    },
    (record) => record
      ? { status: "published", version: record.version, sha256: record.sha256 }
      : { status: "absent" },
  );

  const preflightCargoCredential = async (packages, records, signal) => {
    const first = validateLocalCratePackages(packages, release.version)[0];
    const targetDirectory = dirname(dirname(first.path));
    const cargoHome = resolve(dirname(targetDirectory), "cargo-home");
    const cargoEnvironment = cargoPublicationEnvironment(config.env, { cargoHome });
    const headers = {
      accept: "application/json",
      authorization: `token ${cargoEnvironment.CARGO_REGISTRY_TOKEN}`,
      "user-agent": `nanocodex-release/${release.version}`,
    };
    const meResponse = await retryingBufferedFetch(
      `${cratesRegistryOrigin}/api/v1/me`,
      { headers },
      signal,
      config,
      "crates.io credential identity",
    );
    if (!meResponse.ok) {
      throw await responseFailure(meResponse, "crates.io credential identity", secrets);
    }
    const me = await readJsonResponse(meResponse, "crates.io credential identity");
    if (
      !isRecord(me) || !isRecord(me.user) ||
      !Number.isSafeInteger(me.user.id) || me.user.id <= 0 ||
      typeof me.user.login !== "string" || me.user.login === ""
    ) throw new Error("crates.io credential returned an invalid publisher identity");

    const missing = records.flatMap((record, index) =>
      record ? [] : [releaseCrateNames[index]]
    );
    for (const crate of missing) {
      const ownersResponse = await retryingBufferedFetch(
        `${cratesRegistryOrigin}/api/v1/crates/${crate}/owners`,
        { headers },
        signal,
        config,
        `crates.io ${crate} publish ownership`,
      );
      if (!ownersResponse.ok) {
        throw await responseFailure(
          ownersResponse,
          `crates.io ${crate} publish ownership`,
          secrets,
        );
      }
      const owners = await readJsonResponse(
        ownersResponse,
        `crates.io ${crate} publish ownership`,
      );
      if (
        !isRecord(owners) || !Array.isArray(owners.users) ||
        !owners.users.some((owner) =>
          isRecord(owner) && owner.id === me.user.id && owner.login === me.user.login
        )
      ) throw new Error(`crates.io credential identity cannot publish ${crate}`);
    }
    return { login: me.user.login, crates: missing };
  };

  const preflightNpmCredential = async (signal) => {
    const npmEnvironment = npmPublicationEnvironment(config.env, {
      userConfig: resolve(tmpdir(), "nanocodex-release-preflight-npmrc"),
    });
    const headers = {
      accept: "application/json",
      authorization: `Bearer ${npmEnvironment.NODE_AUTH_TOKEN}`,
      "user-agent": `nanocodex-release/${release.version}`,
    };
    const whoResponse = await retryingBufferedFetch(
      `${npmRegistryOrigin}/-/whoami`,
      { headers },
      signal,
      config,
      "npm credential identity",
    );
    if (!whoResponse.ok) {
      throw await responseFailure(whoResponse, "npm credential identity", secrets);
    }
    const who = await readJsonResponse(whoResponse, "npm credential identity");
    if (!isRecord(who) || typeof who.username !== "string" || who.username === "") {
      throw new Error("npm credential returned an invalid publisher identity");
    }
    const accessResponse = await retryingBufferedFetch(
      `${npmRegistryOrigin}/-/package/nanocodex/collaborators?format=cli`,
      { headers },
      signal,
      config,
      "npm nanocodex publish access",
    );
    if (!accessResponse.ok) {
      throw await responseFailure(accessResponse, "npm nanocodex publish access", secrets);
    }
    const access = await readJsonResponse(accessResponse, "npm nanocodex publish access");
    if (!isRecord(access) || access[who.username] !== "read-write") {
      throw new Error("npm credential identity does not have nanocodex read-write access");
    }
    return { username: who.username, package: "nanocodex", access: "read-write" };
  };

  const preflightCredentials = (registryState, packages, _npmPackage, signal) => report.run(
    "prepare: validate every still-needed registry credential",
    async () => {
      const requirements = registryCredentialRequirements(
        registryState.crates,
        registryState.npm,
      );
      const [cargo, npm] = await Promise.all([
        requirements.cargo
          ? preflightCargoCredential(packages, registryState.crates, signal)
          : undefined,
        requirements.npm ? preflightNpmCredential(signal) : undefined,
      ]);
      return { requirements, cargo, npm };
    },
    ({ requirements, cargo, npm }) => ({
      requirements,
      cargo: cargo ? { login: cargo.login, crateCount: cargo.crates.length } : "not-needed",
      npm: npm ? { username: npm.username, package: npm.package } : "not-needed",
    }),
  );

  const publishNpm = (npmPackage, authority, signal) => report.run(
    "publish: exact R2 npm tarball",
    async () => {
      const metadataRecord = await readNpmVersion(npmPackage.metadata, signal);
      const existing = metadataRecord
        ? await verifyNpmRecord(metadataRecord, npmPackage.metadata, signal)
        : undefined;
      if (existing) return { action: "already-published", registry: existing };
      const configurationDirectory = await mkdtemp(resolve(tmpdir(), "nanocodex-npm-publish-"));
      temporaryDirectories.add(configurationDirectory);
      const npmrc = resolve(configurationDirectory, "npmrc");
      const cache = resolve(configurationDirectory, "cache");
      await mkdir(cache, { mode: 0o700 });
      const handle = await open(
        npmrc,
        fsConstants.O_CREAT | fsConstants.O_EXCL | fsConstants.O_WRONLY |
          (fsConstants.O_NOFOLLOW ?? 0),
        0o600,
      );
      try {
        await handle.writeFile(
          "registry=https://registry.npmjs.org/\n" +
            "//registry.npmjs.org/:_authToken=${NODE_AUTH_TOKEN}\n" +
            "provenance=false\n",
        );
      } finally {
        await handle.close();
      }
      try {
        await authority.checkpoint(
          `before publishing nanocodex@${npmPackage.metadata.version}`,
        );
        await runProcess("npm", [
          "publish",
          npmPackage.path,
          "--access=public",
          "--ignore-scripts",
          "--provenance=false",
          `--userconfig=${npmrc}`,
          `--registry=${npmRegistryOrigin}`,
        ], {
          cwd: config.repository,
          env: npmPublicationEnvironment(config.env, { userConfig: npmrc, cache }),
          signal,
          timeoutMs: config.commandTimeoutMs,
          secrets,
        });
        return { action: "published" };
      } catch (cause) {
        try {
          const registry = await waitForNpm(npmPackage.metadata, signal);
          return { action: "published-despite-client-error", registry };
        } catch {
          throw cause;
        }
      }
    },
    (result) => ({ action: result.action, version: npmPackage.metadata.version }),
  );

  const verifyNpmRegistry = (npmPackage, signal) => report.run(
    "verify: npm metadata and canonical downloadable tarball bytes",
    () => waitForNpm(npmPackage.metadata, signal),
    (record) => record,
  );

  const leaseOwner = report.attempt;
  const leaseIdentity = {
    owner: leaseOwner,
    kind: "stable",
    id: release.tag,
  };
  const sendLeaseMutation = async (
    operation,
    url,
    method,
    body,
    signal,
    {
      maximumAttempts = config.httpAttempts,
      timeoutMs = config.requestTimeoutMs,
      maximumDelayMs = config.retryMaximumDelayMs,
      jsonDescription,
    } = {},
  ) => {
    const serializedBody = JSON.stringify(body);
    return retryReleaseOperation(
      operation,
      async () => {
        const response = await boundedFetch(url, {
          method,
          headers: releaseRequestHeaders(config.releaseToken, { json: true }),
          body: serializedBody,
        }, signal, timeoutMs);
        if (!response.ok || jsonDescription === undefined) return response;
        try {
          return {
            response,
            value: await readJsonResponse(response, jsonDescription),
          };
        } catch (cause) {
          await response.body?.cancel().catch(() => undefined);
          throw cause;
        }
      },
      {
        signal,
        maximumAttempts,
        maximumDelayMs,
      },
    );
  };

  const publicationLease = {
    async acquire(identity, signal) {
      if (canonicalJson(identity) !== canonicalJson({
        ...leaseIdentity,
        commit: identity.commit,
      })) throw new Error("publication lease acquire received the wrong stable identity");
      const outcome = await sendLeaseMutation(
        "acquire stable publication lease",
        new URL("/api/releases/publication-lease/acquire", config.ciOrigin),
        "POST",
        identity,
        signal,
        { jsonDescription: "acquire stable publication lease" },
      );
      if (outcome instanceof Response) {
        throw await responseFailure(outcome, "acquire stable publication lease", secrets);
      }
      return parsePublicationLease(
        outcome.value,
        identity,
      );
    },
    async heartbeat(lease, signal) {
      const outcome = await sendLeaseMutation(
        "heartbeat stable publication lease",
        new URL(
          `/api/releases/publication-lease/${encodeURIComponent(lease.leaseId)}/heartbeat`,
          config.ciOrigin,
        ),
        "POST",
        { owner: lease.owner },
        signal,
        {
          maximumAttempts: 2,
          timeoutMs: Math.min(config.requestTimeoutMs, 10_000),
          maximumDelayMs: Math.min(config.retryMaximumDelayMs, 5_000),
          jsonDescription: "heartbeat stable publication lease",
        },
      );
      if (outcome instanceof Response) {
        throw new PublicationLeaseLostError("heartbeat acknowledgement", {
          cause: await responseFailure(
            outcome,
            "heartbeat stable publication lease",
            secrets,
          ),
        });
      }
      return parsePublicationLease(
        outcome.value,
        {
          owner: lease.owner,
          kind: lease.kind,
          id: lease.id,
          commit: lease.commit,
        },
        lease,
      );
    },
    async release(lease) {
      const response = await sendLeaseMutation(
        "release stable publication lease",
        new URL(
          `/api/releases/publication-lease/${encodeURIComponent(lease.leaseId)}`,
          config.ciOrigin,
        ),
        "DELETE",
        { owner: lease.owner },
        undefined,
      );
      if (response.status !== 204) {
        throw await responseFailure(response, "release stable publication lease", secrets);
      }
      await response.body?.cancel().catch(() => undefined);
    },
  };

  const finalize = (ready, authority, signal) => report.run(
    "finalize: publish immutable manifest and latest pointer",
    async () => {
      let lastCause;
      for (let attempt = 1; attempt <= config.httpAttempts; attempt += 1) {
        let response;
        try {
          await authority.checkpoint(
            `before finalization request ${attempt}`,
          );
          const headers = releaseRequestHeaders(config.releaseToken, { json: true });
          for (const [name, value] of Object.entries(
            publicationLeaseFinalizeHeaders(authority.lease),
          )) headers.set(name, value);
          response = await boundedFetch(
            new URL(`/api/ci/releases/stable/${ready.tag}/finalize`, config.ciOrigin),
            {
              method: "POST",
              headers,
              body: JSON.stringify({ head: ready.head }),
            },
            signal,
            config.requestTimeoutMs,
          );
          if (response.ok) {
            try {
              const value = await readJsonResponse(response, "finalize stable release");
              if (!isRecord(value) || value.status !== "released") {
                throw new Error("stable release finalization returned an invalid status");
              }
              return value;
            } catch (cause) {
              // Reconcile a committed finalization whose successful response
              // body could not be consumed or validated.
              lastCause = cause;
            }
          } else if (!retryableHttpStatus(response.status)) {
            throw await responseFailure(response, "finalize stable release", secrets, attempt);
          } else {
            lastCause = await responseFailure(
              response,
              "finalize stable release",
              secrets,
              attempt,
            );
          }
        } catch (cause) {
          throwIfAborted(signal);
          if (cause instanceof ReleaseHttpError && !cause.retryable) throw cause;
          lastCause = cause;
        }

        // A response can be lost after the Durable Object commits. Reconcile
        // the immutable release before ever resubmitting the finalization POST.
        const published = await readPublishedManifest(ready, signal, true);
        if (published) return published;
        await assertReady(ready, "before finalization retry", signal);
        if (attempt === config.httpAttempts) {
          throw new ReleaseHttpError("finalize stable release", {
            attempts: attempt,
            retryable: true,
            cause: lastCause,
          });
        }
        const retryAfterMs = lastCause instanceof ReleaseHttpError
          ? lastCause.retryAfterMs
          : undefined;
        await abortableDelay(
          retryAfterMs ?? Math.min(
            config.retryMaximumDelayMs,
            250 * (2 ** (attempt - 1)),
          ),
          signal,
        );
      }
      throw lastCause;
    },
    () => ({ status: "released", head: ready.head }),
  );

  const verifyPublicAsset = async (manifest, asset, signal, { retain = false } = {}) => {
    const operation = `public release asset ${asset.name}`;
    const downloaded = await retryingConsumedFetch(
      new URL(asset.downloadPath, config.publicOrigin),
      { headers: { accept: asset.contentType } },
      signal,
      config,
      operation,
      async (response) => {
        if (
          parseCanonicalInteger(response.headers.get("content-length")) !== asset.size ||
          response.headers.get("content-type") !== asset.contentType ||
          response.headers.get("content-disposition") !==
            `attachment; filename="${asset.name}"` ||
          response.headers.get("x-content-type-options") !== "nosniff" ||
          response.headers.get("x-nanocodex-release") !== manifest.id ||
          response.headers.get("x-nanocodex-sha256") !== asset.sha256 ||
          response.headers.get("etag") !== `"${asset.sha256}"` ||
          !/(?:^|,)\s*immutable(?:,|$)/.test(
            response.headers.get("cache-control") ?? "",
          ) ||
          response.headers.has("content-encoding") || response.headers.has("content-range")
        ) throw new ReleaseValidationError(
          `public release asset headers are invalid for ${asset.name}`,
        );
        return downloadAssetBytes(response, asset, operation, { retain });
      },
      config.assetTimeoutMs,
    );
    if (downloaded instanceof Response) {
      throw await responseFailure(downloaded, operation, secrets);
    }
    return {
      name: asset.name,
      size: asset.size,
      sha256: asset.sha256,
      ...(downloaded.bytes ? { bytes: downloaded.bytes } : {}),
    };
  };

  const verifyPublic = (ready, existing, signal) => report.run(
    "verify: latest immutable manifest and public assets",
    async () => {
      let manifest = existing && existing.manifestSha256 ? existing : undefined;
      const deadline = Date.now() + config.registryTimeoutMs;
      while (!manifest) {
        manifest = await readPublishedManifest(ready, signal, true);
        if (manifest) break;
        if (Date.now() >= deadline) throw new Error("public immutable manifest did not propagate");
        await abortableDelay(
          Math.min(config.registryPollMs, Math.max(1, deadline - Date.now())),
          signal,
        );
      }
      const latestResponse = await retryingBufferedFetch(
        new URL("/api/releases/channels/latest", config.publicOrigin),
        { headers: { accept: "application/json" } },
        signal,
        config,
        "latest public release",
      );
      if (!latestResponse.ok) {
        throw await responseFailure(latestResponse, "latest public release", secrets);
      }
      if (
        latestResponse.headers.get("cache-control") !== "no-store" ||
        latestResponse.headers.get("content-type") !== "application/json; charset=utf-8" ||
        latestResponse.headers.get("x-content-type-options") !== "nosniff" ||
        latestResponse.headers.get("content-location") !==
          `/api/releases/releases/stable/${ready.tag}` ||
        latestResponse.headers.has("content-encoding") ||
        latestResponse.headers.has("content-range")
      ) throw new Error("latest public release response headers are invalid");
      const latest = parseLatestStable(
        await readJsonResponse(latestResponse, "latest public release"),
        manifest,
      );
      const assets = [];
      for (const asset of manifest.assets) {
        assets.push(await verifyPublicAsset(manifest, asset, signal));
      }
      return {
        tag: manifest.tag,
        head: manifest.commit,
        manifestSha256: manifest.manifestSha256,
        generation: latest.pointer.generation,
        assets,
      };
    },
  );

  const reviewPublishedPlan = (
    ready,
    packages,
    manifest,
    storedPlan,
    signal,
  ) => report.run(
    "replay: reproduce reviewed plan from immutable public bytes",
    async () => {
      const manifestIdentity = {
        version: manifest.version,
        kind: manifest.kind,
        id: manifest.id,
        tag: manifest.tag,
        commit: manifest.commit,
        channel: manifest.channel,
        assets: manifest.assets,
      };
      if (canonicalJson(manifestIdentity) !== canonicalJson(storedPlan.manifest)) {
        throw new ReviewedReleasePlanMismatchError(
          "immutable public manifest does not reproduce the reviewed manifest identity",
        );
      }
      const [toolchain, reviewedCrates] = await Promise.all([
        inspectToolchain(signal),
        rehashCratePackages(packages, "published plan reproduction"),
      ]);
      const proofs = [];
      let npmPackage;
      for (const publicAsset of manifest.assets) {
        const draftAsset = storedPlan.draft.assets.find(
          (asset) => asset.name === publicAsset.name,
        );
        if (
          !draftAsset || draftAsset.platform !== publicAsset.platform ||
          draftAsset.size !== publicAsset.size || draftAsset.sha256 !== publicAsset.sha256 ||
          draftAsset.contentType !== publicAsset.contentType
        ) throw new ReviewedReleasePlanMismatchError(
          `public asset identity changed for ${publicAsset.name}`,
        );
        const downloaded = await verifyPublicAsset(
          manifest,
          publicAsset,
          signal,
          { retain: publicAsset.platform === "npm" },
        );
        proofs.push(draftAsset);
        if (publicAsset.platform === "npm") {
          npmPackage = await writeNpmPackage(draftAsset, downloaded.bytes, ready.tag);
        }
      }
      if (!npmPackage || proofs.length !== storedPlan.draft.assets.length) {
        throw new Error("public release did not reproduce every reviewed plan asset");
      }
      return {
        plan: createReviewedReleasePlan({
          ready,
          draft: storedPlan.draft,
          crates: reviewedCrates,
          npm: npmPackage.metadata,
          assets: proofs,
          toolchain,
        }),
        npmPackage,
      };
    },
    ({ plan }) => ({ planSha256: plan.planSha256, source: "immutable-public-release" }),
  );

  const planPath = resolve(config.planDirectory, `${release.tag}.json`);
  const persistPlan = (plan) => report.run(
    "stage: persist immutable reviewed release plan",
    () => persistReviewedReleasePlan(planPath, plan),
    ({ action, plan: reviewed }) => ({
      action,
      path: planPath,
      planSha256: reviewed.planSha256,
    }),
  );
  const readPlan = () => report.run(
    "publish: require immutable reviewed release plan",
    () => readReviewedReleasePlan(planPath),
    (plan) => ({ path: planPath, planSha256: plan.planSha256 }),
  );

  return {
    verifyTrust,
    assertTrust,
    packageCrates,
    verifyExistingCrates,
    stage,
    readReady,
    assertReady,
    reviewPlan,
    reviewPublishedPlan,
    persistPlan,
    readPlan,
    planPath,
    readPublished,
    inspectNpmRegistry,
    preflightCredentials,
    leaseOwner,
    publicationLease,
    leaseHeartbeatMs: config.leaseHeartbeatMs,
    publishCrates,
    verifyCrates,
    publishNpm,
    verifyNpmRegistry,
    finalize,
    verifyPublic,
    async cleanup() {
      for (const directory of temporaryDirectories) {
        await rm(directory, { recursive: true, force: true });
      }
      temporaryDirectories.clear();
    },
  };
}

function readySummary(ready) {
  return {
    status: ready.status,
    tag: ready.tag,
    head: ready.head,
    expectedLatest: ready.draft.expectedChannel,
    assetCount: ready.draft.assets.length,
    assets: ready.draft.assets.map(({ name, platform, size, sha256 }) => ({
      name,
      platform,
      size,
      sha256,
    })),
  };
}

export async function main(args = process.argv.slice(2), env = process.env) {
  const options = parseReleaseArguments(args);
  if (options.help) {
    process.stdout.write(
      "Usage: node web/scripts/ci-release-controller.mjs <stage|publish> vMAJOR.MINOR.PATCH\n" +
        "  stage    build and review exact tagged release artifacts without publishing registries or latest\n" +
        "  publish  publish reviewed crates/npm artifacts, prove registries, then finalize latest\n",
    );
    return;
  }

  const config = releaseConfiguration(env);
  const secrets = environmentSecrets(env, [config.releaseToken]);
  const controller = new AbortController();
  const stop = (name) => controller.abort(new DOMException(name, "AbortError"));
  const onInterrupt = () => stop("SIGINT");
  const onTerminate = () => stop("SIGTERM");
  process.once("SIGINT", onInterrupt);
  process.once("SIGTERM", onTerminate);

  let releaseLock;
  let operations;
  let report;
  let primaryFailure;
  let result;
  try {
    const commonDirectory = await releaseCommonDirectory(config, controller.signal, secrets);
    const reportDirectory = resolve(
      env.NANOCODEX_RELEASE_REPORT_DIR ??
        resolve(commonDirectory, "nanocodex-release-reports"),
    );
    config.planDirectory = resolve(
      env.NANOCODEX_RELEASE_PLAN_DIR ??
        resolve(commonDirectory, "nanocodex-release-plans"),
    );
    report = await LocalReleaseReport.create(
      reportDirectory,
      options.command,
      options,
      secrets,
    );
    process.stdout.write(`${JSON.stringify({
      status: "running",
      command: options.command,
      tag: options.tag,
      attempt: report.attempt,
      report: report.path,
    })}\n`);
    const lockPath = resolve(commonDirectory, "nanocodex-stable-release.lock");
    releaseLock = await acquireExclusiveLock(lockPath, {
      controller: "stable-release",
      attempt: report.attempt,
      command: options.command,
      tag: options.tag,
      repository: config.repository,
    });
    operations = await createReleaseOperations(config, options, report, secrets);
    result = await runReleaseSequence(options.command, operations, {
      signal: controller.signal,
    });
  } catch (cause) {
    primaryFailure = cause;
  } finally {
    process.removeListener("SIGINT", onInterrupt);
    process.removeListener("SIGTERM", onTerminate);
    const cleanupFailures = [];
    if (operations) {
      try {
        await operations.cleanup();
      } catch (cause) {
        cleanupFailures.push(cause);
      }
    }
    if (releaseLock) {
      try {
        await releaseLock();
      } catch (cause) {
        cleanupFailures.push(cause);
      }
    }
    if (cleanupFailures.length > 0) {
      primaryFailure = primaryFailure
        ? new AggregateError(
          [primaryFailure, ...cleanupFailures],
          "release controller operation and cleanup failed",
        )
        : new AggregateError(cleanupFailures, "release controller cleanup failed");
    }
  }
  if (primaryFailure) {
    if (report) {
      try {
        await report.fail(primaryFailure);
      } catch (cause) {
        primaryFailure = new AggregateError(
          [primaryFailure, cause],
          "release controller failed and could not persist its failure report",
        );
      }
    }
    primaryFailure.releaseEvidence = {
      status: "failure",
      command: options.command,
      tag: options.tag,
      ...(report ? { attempt: report.attempt, report: report.path } : {}),
      error: boundedStructuredError(primaryFailure, secrets),
    };
    throw primaryFailure;
  }
  try {
    await report.succeed(result);
  } catch (cause) {
    let failure = cause;
    try {
      await report.fail(cause);
    } catch (reportCause) {
      failure = new AggregateError(
        [cause, reportCause],
        "release succeeded but terminal report persistence failed",
      );
    }
    failure.releaseEvidence = {
      status: "failure",
      command: options.command,
      tag: options.tag,
      attempt: report.attempt,
      report: report.path,
      error: boundedStructuredError(failure, secrets),
    };
    throw failure;
  }
  process.stdout.write(`${JSON.stringify({
    status: "success",
    command: options.command,
    action: result.action,
    tag: result.tag,
    head: result.head,
    attempt: report.attempt,
    report: report.path,
  })}\n`);
  return result;
}

if (process.argv[1] && resolve(process.argv[1]) === scriptPath) {
  main().catch((cause) => {
    const secrets = environmentSecrets(process.env);
    const evidence = isRecord(cause?.releaseEvidence)
      ? cause.releaseEvidence
      : {
        status: "failure",
        error: boundedStructuredError(cause, secrets),
      };
    process.stderr.write(`${JSON.stringify(evidence)}\n`);
    process.exitCode = 1;
  });
}
