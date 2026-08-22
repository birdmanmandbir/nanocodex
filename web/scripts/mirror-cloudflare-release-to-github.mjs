#!/usr/bin/env node

import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { open, unlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptPath = fileURLToPath(import.meta.url);

const repository = "gakonst/nanocodex";
const defaultPublicOrigin = "https://nanocodex.me-7fb.workers.dev";
const githubApiOrigin = "https://api.github.com";
const githubUploadsOrigin = "https://uploads.github.com";
const githubWebOrigin = "https://github.com";
const githubApiVersion = "2026-03-10";
const immutableCacheControl = "public, max-age=31536000, immutable";
const noStore = "no-store";
const compatibilityPurpose = "nanocodex-cloudflare-github-updater-compatibility";
const bootstrapStableCommit = "e4eea49fc6fab06a98ff01ec8c3da8d9a729eee1";
const compatibilityAssetNames = Object.freeze([
  "SHA256SUMS",
  "nanocodex-aarch64-apple-darwin",
  "nanocodex-x86_64-unknown-linux-gnu",
]);
const compatibilityAssetSpecifications = Object.freeze(new Map([
  [
    "SHA256SUMS",
    Object.freeze({ platform: "linux", maximumBytes: 64 * 1024 }),
  ],
  [
    "nanocodex-aarch64-apple-darwin",
    Object.freeze({ platform: "aarch64-apple-darwin", maximumBytes: 128 * 1024 * 1024 }),
  ],
  [
    "nanocodex-x86_64-unknown-linux-gnu",
    Object.freeze({ platform: "x86_64-unknown-linux-gnu", maximumBytes: 128 * 1024 * 1024 }),
  ],
]));
const legacyRollingAssetSpecifications = Object.freeze(new Map([
  ...compatibilityAssetSpecifications,
  ["nanocodex-aarch64-apple-darwin.gz", Object.freeze({ maximumBytes: 128 * 1024 * 1024 })],
  ["nanocodex-x86_64-unknown-linux-gnu.gz", Object.freeze({ maximumBytes: 128 * 1024 * 1024 })],
  [
    "nanocodex-vm-guest-x86_64-unknown-linux-musl",
    Object.freeze({ maximumBytes: 64 * 1024 * 1024 }),
  ],
  [
    "nanocodex-vm-guest-x86_64-unknown-linux-musl.gz",
    Object.freeze({ maximumBytes: 64 * 1024 * 1024 }),
  ],
]));
const legacyRollingAssetSets = Object.freeze([
  Object.freeze([...compatibilityAssetNames]),
  Object.freeze([
    "SHA256SUMS",
    "nanocodex-aarch64-apple-darwin",
    "nanocodex-aarch64-apple-darwin.gz",
    "nanocodex-vm-guest-x86_64-unknown-linux-musl",
    "nanocodex-vm-guest-x86_64-unknown-linux-musl.gz",
    "nanocodex-x86_64-unknown-linux-gnu",
    "nanocodex-x86_64-unknown-linux-gnu.gz",
  ]),
]);
const releasePlatforms = new Set([
  "x86_64-unknown-linux-gnu",
  "aarch64-apple-darwin",
  "x86_64-unknown-linux-musl",
  "linux",
  "npm",
]);
const SHA1 = /^[a-f0-9]{40}$/;
const SHA256 = /^[a-f0-9]{64}$/;
const stableTagPattern = /^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/;
const assetNamePattern = /^[A-Za-z0-9][A-Za-z0-9._-]{0,159}$/;
const contentTypePattern =
  /^[A-Za-z0-9][A-Za-z0-9!#$&^_.+-]*\/[A-Za-z0-9][A-Za-z0-9!#$&^_.+-]*(?:;[\t ]*[A-Za-z0-9_-]+=[A-Za-z0-9._-]+)*$/;
const maximumManifestBytes = 256 * 1024;
const maximumChannelBytes = 512 * 1024;
const maximumGitHubJsonBytes = 1024 * 1024;
const maximumChildOutputBytes = 2 * 1024 * 1024;
const maximumChildErrorBytes = 64 * 1024;
const sourceTimeoutMs = 120_000;
const githubMetadataTimeoutMs = 120_000;
const githubMutationTimeoutMs = 30_000;
const githubUploadTimeoutMs = 10 * 60_000;
const publicVerificationTimeoutMs = 10 * 60_000;
const mutationAttempts = 4;
const publicMetadataAttempts = 12;
const publicDownloadRedirects = 5;
const maximumReleasePages = 1_000;
const maximumReleaseAssets = 1_000;

export class MirrorValidationError extends Error {
  constructor(message, options) {
    super(message, options);
    this.name = "MirrorValidationError";
  }
}

export class MirrorConflictError extends Error {
  constructor(message, options) {
    super(message, options);
    this.name = "MirrorConflictError";
  }
}

export class GitHubRequestError extends Error {
  constructor(operation, { status, retryable = false, detail, cause } = {}) {
    super(
      `${operation} failed${status == null ? "" : ` with HTTP ${status}`}` +
        `${detail ? `: ${detail}` : ""}`,
      cause === undefined ? undefined : { cause },
    );
    this.name = "GitHubRequestError";
    this.operation = operation;
    this.status = status;
    this.retryable = retryable;
  }
}

export class GitHubTransportError extends Error {
  constructor(message, options) {
    super(message, options);
    this.name = "GitHubTransportError";
  }
}

export class PublicationLeaseLostError extends Error {
  constructor(message, options) {
    super(message, options);
    this.name = "PublicationLeaseLostError";
  }
}

export function parseMirrorArguments(args) {
  if (!Array.isArray(args)) throw new TypeError("mirror arguments must be an array");
  const values = [...args];
  if (values.length === 1 && (values[0] === "--help" || values[0] === "-h")) {
    return { help: true };
  }
  let dryRun = false;
  let origin;
  const positional = [];
  for (let index = 0; index < values.length; index += 1) {
    const value = values[index];
    if (value === "--dry-run") {
      if (dryRun) throw new Error("--dry-run may be supplied only once");
      dryRun = true;
    } else if (value === "--origin") {
      if (origin != null || index + 1 >= values.length) {
        throw new Error("--origin requires one value and may be supplied only once");
      }
      origin = values[++index];
    } else if (value?.startsWith("-")) {
      throw new Error(`unknown mirror option ${JSON.stringify(value)}`);
    } else {
      positional.push(value);
    }
  }
  if (positional.length !== 2) {
    throw new Error(
      "usage: mirror-cloudflare-release-to-github.mjs [--dry-run] [--origin ORIGIN] <stable|nightly> <immutable-id>",
    );
  }
  const identity = parseImmutableIdentity(positional[0], positional[1]);
  return { help: false, dryRun, origin, ...identity };
}

export function parseImmutableIdentity(channel, id) {
  if (channel === "stable") {
    if (typeof id !== "string" || id.length > 64 || !stableTagPattern.test(id)) {
      throw new MirrorValidationError(
        "stable compatibility identity must be canonical vMAJOR.MINOR.PATCH",
      );
    }
    const components = id.slice(1).split(".").map(Number);
    if (!components.every(Number.isSafeInteger)) {
      throw new MirrorValidationError("stable compatibility version components are too large");
    }
    return Object.freeze({ channel, kind: "stable", id });
  }
  if (channel === "nightly") {
    if (typeof id !== "string" || !SHA1.test(id)) {
      throw new MirrorValidationError(
        "nightly compatibility identity must be a full lowercase 40-hex commit",
      );
    }
    return Object.freeze({ channel, kind: "commit", id });
  }
  throw new MirrorValidationError("compatibility channel must be stable or nightly");
}

export function parsePublicOrigin(value) {
  if (typeof value !== "string" || value.length === 0 || value.length > 2048) {
    throw new MirrorValidationError("release origin must be an absolute public origin");
  }
  let url;
  try {
    url = new URL(value);
  } catch (cause) {
    throw new MirrorValidationError("release origin must be an absolute public origin", { cause });
  }
  const loopback = ["127.0.0.1", "[::1]", "localhost"].includes(url.hostname);
  if (
    (url.protocol !== "https:" && !(loopback && url.protocol === "http:")) ||
    url.username !== "" || url.password !== "" || url.pathname !== "/" ||
    url.search !== "" || url.hash !== ""
  ) {
    throw new MirrorValidationError(
      "release origin must be HTTPS without credentials, path, query, or fragment (HTTP is allowed only for loopback fixtures)",
    );
  }
  return url.origin;
}

export function canonicalJson(value) {
  if (value === null || typeof value === "boolean" || typeof value === "string") {
    return JSON.stringify(value);
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value)) {
      throw new MirrorValidationError("canonical JSON requires finite numbers");
    }
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (isRecord(value)) {
    return `{${Object.keys(value).sort().map((key) =>
      `${JSON.stringify(key)}:${canonicalJson(value[key])}`
    ).join(",")}}`;
  }
  throw new MirrorValidationError("canonical JSON requires JSON values");
}

export function sha256Hex(value) {
  return createHash("sha256").update(value).digest("hex");
}

export function validatePublicManifest(value, identity) {
  const expected = parseImmutableIdentity(identity.channel, identity.id);
  if (!exactRecord(value, [
    "version",
    "kind",
    "id",
    "tag",
    "commit",
    "channel",
    "finalizedAt",
    "assets",
    "manifestSha256",
  ])) throw new MirrorValidationError("public release manifest has unexpected fields");
  const expectedTag = expected.channel === "stable" ? expected.id : `nightly-${expected.id}`;
  const expectedCommit = expected.channel === "nightly" ? expected.id : value.commit;
  if (
    value.version !== 1 || value.kind !== expected.kind || value.id !== expected.id ||
    value.tag !== expectedTag || value.channel !== (expected.channel === "stable" ? "latest" : "nightly") ||
    typeof value.commit !== "string" || !SHA1.test(value.commit) ||
    value.commit !== expectedCommit || !canonicalTimestamp(value.finalizedAt) ||
    typeof value.manifestSha256 !== "string" || !SHA256.test(value.manifestSha256) ||
    !Array.isArray(value.assets) || value.assets.length === 0 || value.assets.length > 64
  ) throw new MirrorValidationError("public release manifest has the wrong immutable identity");
  if (expected.channel === "stable" && expected.id === "v0.5.0" &&
    value.commit !== bootstrapStableCommit) {
    throw new MirrorValidationError("v0.5.0 public release has the wrong pinned commit");
  }

  const names = new Set();
  const assets = value.assets.map((asset) => {
    if (!exactRecord(asset, [
      "name",
      "platform",
      "size",
      "sha256",
      "contentType",
      "downloadPath",
    ])) throw new MirrorValidationError("public release asset has unexpected fields");
    if (
      typeof asset.name !== "string" || !assetNamePattern.test(asset.name) || names.has(asset.name) ||
      !releasePlatforms.has(asset.platform) ||
      !Number.isSafeInteger(asset.size) || asset.size <= 0 || asset.size > 256 * 1024 * 1024 ||
      typeof asset.sha256 !== "string" || !SHA256.test(asset.sha256) ||
      typeof asset.contentType !== "string" || asset.contentType.length > 160 ||
      !contentTypePattern.test(asset.contentType)
    ) throw new MirrorValidationError(`public release asset is invalid: ${String(asset.name)}`);
    names.add(asset.name);
    const expectedPath = immutableAssetPath(expected, asset.name);
    if (asset.downloadPath !== expectedPath) {
      throw new MirrorValidationError(`public release asset has a noncanonical path: ${asset.name}`);
    }
    return Object.freeze({ ...asset });
  });

  const inventory = publicReleaseAssetInventory(expected, value.commit);
  if (assets.length !== inventory.length || assets.some((asset, index) => {
    const specification = inventory[index];
    return asset.name !== specification.name || asset.platform !== specification.platform ||
      asset.contentType !== specification.contentType || asset.size > specification.maximumBytes;
  })) {
    throw new MirrorValidationError("public release manifest has a noncanonical asset inventory");
  }

  for (const [name, specification] of compatibilityAssetSpecifications) {
    const asset = assets.find((candidate) => candidate.name === name);
    if (!asset) throw new MirrorValidationError(`public release manifest is missing ${name}`);
    if (asset.platform !== specification.platform || asset.size > specification.maximumBytes) {
      throw new MirrorValidationError(`public release compatibility asset has the wrong identity: ${name}`);
    }
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
  const actualManifestSha256 = sha256Hex(canonicalJson(unsigned));
  if (actualManifestSha256 !== value.manifestSha256) {
    throw new MirrorValidationError(
      `public release manifest SHA-256 mismatch: expected ${value.manifestSha256}, calculated ${actualManifestSha256}`,
    );
  }
  return deepFreeze({ ...value, assets });
}

function publicReleaseAssetInventory(identity, commit) {
  if (identity.channel === "stable" && identity.id === "v0.5.0") {
    return [
      { name: "PROVENANCE.json", platform: "linux", contentType: "application/json", maximumBytes: 64 * 1024 },
      { name: "SHA256SUMS", platform: "linux", contentType: "text/plain", maximumBytes: 64 * 1024 },
      { name: "nanocodex-aarch64-apple-darwin", platform: "aarch64-apple-darwin", contentType: "application/octet-stream", maximumBytes: 128 * 1024 * 1024 },
      { name: "nanocodex-x86_64-unknown-linux-gnu", platform: "x86_64-unknown-linux-gnu", contentType: "application/octet-stream", maximumBytes: 128 * 1024 * 1024 },
      { name: "nanocodex-x86_64-unknown-linux-gnu.gz", platform: "x86_64-unknown-linux-gnu", contentType: "application/gzip", maximumBytes: 128 * 1024 * 1024 },
    ].sort((left, right) => left.name.localeCompare(right.name));
  }
  const npmName = identity.channel === "stable"
    ? `nanocodex-${identity.id.slice(1)}.tgz`
    : `nanocodex-${commit.slice(0, 10)}.tgz`;
  return [
    { name: "PROVENANCE.json", platform: "linux", contentType: "application/json", maximumBytes: 64 * 1024 },
    { name: "SHA256SUMS", platform: "linux", contentType: "text/plain; charset=utf-8", maximumBytes: 64 * 1024 },
    { name: npmName, platform: "npm", contentType: "application/gzip", maximumBytes: 16 * 1024 * 1024 },
    { name: "nanocodex-aarch64-apple-darwin", platform: "aarch64-apple-darwin", contentType: "application/octet-stream", maximumBytes: 128 * 1024 * 1024 },
    { name: "nanocodex-vm-guest-x86_64-unknown-linux-musl", platform: "x86_64-unknown-linux-musl", contentType: "application/octet-stream", maximumBytes: 64 * 1024 * 1024 },
    { name: "nanocodex-vm-guest-x86_64-unknown-linux-musl.gz", platform: "x86_64-unknown-linux-musl", contentType: "application/gzip", maximumBytes: 64 * 1024 * 1024 },
    { name: "nanocodex-x86_64-unknown-linux-gnu", platform: "x86_64-unknown-linux-gnu", contentType: "application/octet-stream", maximumBytes: 128 * 1024 * 1024 },
    { name: "nanocodex-x86_64-unknown-linux-gnu.gz", platform: "x86_64-unknown-linux-gnu", contentType: "application/gzip", maximumBytes: 128 * 1024 * 1024 },
  ].sort((left, right) => left.name.localeCompare(right.name));
}

export function validateReleaseChannel(value, identity, manifest) {
  const expected = parseImmutableIdentity(identity.channel, identity.id);
  if (!exactRecord(value, ["pointer", "manifest"]) || !exactRecord(value.pointer, [
    "version",
    "channel",
    "kind",
    "id",
    "tag",
    "commit",
    "generation",
    "updatedAt",
  ])) throw new MirrorValidationError("public release channel has unexpected fields");
  const pointer = value.pointer;
  if (
    pointer.version !== 1 ||
    pointer.channel !== (expected.channel === "stable" ? "latest" : "nightly") ||
    pointer.kind !== expected.kind || pointer.id !== manifest.id || pointer.tag !== manifest.tag ||
    pointer.commit !== manifest.commit || !Number.isSafeInteger(pointer.generation) ||
    pointer.generation <= 0 || !canonicalTimestamp(pointer.updatedAt) ||
    canonicalJson(value.manifest) !== canonicalJson(manifest)
  ) {
    throw new MirrorValidationError(
      "public release channel does not resolve to the exact immutable manifest",
    );
  }
  return deepFreeze({ pointer: { ...pointer }, manifest });
}

export async function fetchCanonicalCompatibilityRelease({
  origin,
  channel,
  id,
  fetchImpl = fetch,
  timeoutMs = sourceTimeoutMs,
} = {}) {
  const immutable = await fetchImmutableCompatibilityRelease({
    origin,
    channel,
    id,
    fetchImpl,
    timeoutMs,
  });
  const { identity, manifest, origin: publicOrigin } = immutable;
  const manifestPath = immutableManifestPath(identity);
  const channelPath = `/api/releases/channels/${identity.channel === "stable" ? "latest" : "nightly"}`;
  const channelUrl = new URL(channelPath, `${publicOrigin}/`);
  const channelResponse = await publicWorkerFetch(channelUrl, {
    fetchImpl,
    timeoutMs,
    operation: "release channel",
  });
  assertResponseStatus(channelResponse, 200, "release channel");
  assertExactHeader(channelResponse, "cache-control", noStore, "release channel");
  assertExactHeader(
    channelResponse,
    "content-type",
    "application/json; charset=utf-8",
    "release channel",
  );
  assertExactHeader(channelResponse, "content-location", manifestPath, "release channel");
  if (channelResponse.headers.has("etag")) {
    throw new MirrorValidationError("rolling release channel unexpectedly has an etag");
  }
  const channelBytes = await readBoundedResponse(
    channelResponse,
    maximumChannelBytes,
    "release channel",
  );
  const resolved = validateReleaseChannel(
    parseJsonBytes(channelBytes, "release channel"),
    identity,
    manifest,
  );
  return deepFreeze({ ...immutable, pointer: resolved.pointer });
}

export async function fetchImmutableCompatibilityRelease({
  origin,
  channel,
  id,
  fetchImpl = fetch,
  timeoutMs = sourceTimeoutMs,
} = {}) {
  const identity = parseImmutableIdentity(channel, id);
  const publicOrigin = parsePublicOrigin(origin);
  if (typeof fetchImpl !== "function") throw new TypeError("fetchImpl must be a function");
  const manifestPath = immutableManifestPath(identity);
  const manifestUrl = new URL(manifestPath, `${publicOrigin}/`);

  const immutableResponse = await publicWorkerFetch(manifestUrl, {
    fetchImpl,
    timeoutMs,
    operation: "immutable release manifest",
  });
  assertResponseStatus(immutableResponse, 200, "immutable release manifest");
  assertExactHeader(
    immutableResponse,
    "cache-control",
    immutableCacheControl,
    "immutable release manifest",
  );
  assertExactHeader(
    immutableResponse,
    "content-type",
    "application/json; charset=utf-8",
    "immutable release manifest",
  );
  if (immutableResponse.headers.has("content-location")) {
    throw new MirrorValidationError("immutable release manifest unexpectedly has content-location");
  }
  const immutableBytes = await readBoundedResponse(
    immutableResponse,
    maximumManifestBytes,
    "immutable release manifest",
  );
  const manifest = validatePublicManifest(
    parseJsonBytes(immutableBytes, "immutable release manifest"),
    identity,
  );
  assertExactHeader(
    immutableResponse,
    "etag",
    `"${manifest.manifestSha256}"`,
    "immutable release manifest",
  );

  const selectedAssets = [];
  for (const name of compatibilityAssetNames) {
    const asset = manifest.assets.find((candidate) => candidate.name === name);
    const url = new URL(asset.downloadPath, `${publicOrigin}/`);
    if (url.origin !== publicOrigin || url.pathname !== asset.downloadPath || url.search || url.hash) {
      throw new MirrorValidationError(`release asset URL is not canonical: ${name}`);
    }
    const response = await publicWorkerFetch(url, {
      fetchImpl,
      timeoutMs,
      operation: `immutable release asset ${name}`,
    });
    validateWorkerAssetResponse(response, asset, manifest.id);
    const bytes = await readBoundedResponse(
      response,
      compatibilityAssetSpecifications.get(name).maximumBytes,
      `immutable release asset ${name}`,
      asset.size,
    );
    const actual = sha256Hex(bytes);
    if (actual !== asset.sha256) {
      throw new MirrorValidationError(
        `immutable release asset ${name} SHA-256 mismatch: expected ${asset.sha256}, calculated ${actual}`,
      );
    }
    selectedAssets.push(deepFreeze({ ...asset, bytes }));
  }
  validateChecksumManifest(
    selectedAssets.find((asset) => asset.name === "SHA256SUMS").bytes,
    manifest.assets,
  );
  return deepFreeze({
    origin: publicOrigin,
    identity,
    manifest,
    assets: selectedAssets,
    manifestUrl: manifestUrl.href,
  });
}

export async function withWorkerPublicationLease({
  origin,
  token,
  channel,
  id,
  commit,
  fetchImpl = fetch,
  owner = `github-compat:${process.pid}:${randomUUID()}`,
  heartbeatIntervalMs = 30_000,
  allowFixtureOrigin = false,
} = {}, operation) {
  const identity = parseImmutableIdentity(channel, id);
  assertSha1(commit, "publication lease commit");
  if (identity.kind === "commit" && identity.id !== commit) {
    throw new MirrorValidationError("nightly publication lease id must equal its commit");
  }
  const publicOrigin = parsePublicOrigin(origin);
  if (!allowFixtureOrigin && publicOrigin !== defaultPublicOrigin) {
    throw new MirrorValidationError(
      `authenticated release authority is pinned to ${defaultPublicOrigin}`,
    );
  }
  validateWorkerToken(token);
  if (typeof fetchImpl !== "function" || typeof operation !== "function") {
    throw new TypeError("publication lease requires fetch and operation functions");
  }
  if (!validLeaseOwner(owner) || !Number.isSafeInteger(heartbeatIntervalMs) ||
    heartbeatIntervalMs < 1_000 || heartbeatIntervalMs > 60_000) {
    throw new MirrorValidationError("publication lease owner or heartbeat interval is invalid");
  }
  let lease = await workerLeaseRequest({
    origin: publicOrigin,
    token,
    fetchImpl,
    method: "POST",
    path: "/api/releases/publication-lease/acquire",
    body: { owner, kind: identity.kind, id: identity.id, commit },
    expectedStatuses: new Set([200, 201]),
    identity,
    owner,
    commit,
  });
  let lost;
  let heartbeatPromise;
  let stopped = false;
  const authorityController = new AbortController();
  const heartbeat = async () => {
    if (lost) throw lost;
    if (stopped) throw new PublicationLeaseLostError("publication lease is already closed");
    if (heartbeatPromise) return heartbeatPromise;
    heartbeatPromise = (async () => {
      try {
        const renewed = await workerLeaseRequest({
          origin: publicOrigin,
          token,
          fetchImpl,
          method: "POST",
          path:
            `/api/releases/publication-lease/${encodeURIComponent(lease.leaseId)}/heartbeat`,
          body: { owner },
          expectedStatuses: new Set([200]),
          identity,
          owner,
          commit,
        });
        if (renewed.leaseId !== lease.leaseId || renewed.generation !== lease.generation) {
          throw new PublicationLeaseLostError("publication lease fence changed during heartbeat");
        }
        lease = renewed;
        return lease;
      } catch (cause) {
        lost = cause instanceof PublicationLeaseLostError
          ? cause
          : new PublicationLeaseLostError("publication lease heartbeat failed", { cause });
        authorityController.abort(lost);
        throw lost;
      } finally {
        heartbeatPromise = undefined;
      }
    })();
    return heartbeatPromise;
  };
  const interval = setInterval(() => {
    heartbeat().catch(() => undefined);
  }, heartbeatIntervalMs);
  interval.unref?.();
  const authority = Object.freeze({
    get lease() {
      return lease;
    },
    get signal() {
      return authorityController.signal;
    },
    async assertHeld() {
      return heartbeat();
    },
  });
  let result;
  let operationError;
  try {
    result = await operation(authority);
    await heartbeat();
  } catch (cause) {
    operationError = cause;
  } finally {
    stopped = true;
    authorityController.abort(
      operationError ?? new PublicationLeaseLostError("publication lease operation ended"),
    );
    clearInterval(interval);
    if (heartbeatPromise) await heartbeatPromise.catch(() => undefined);
    try {
      await workerLeaseRequest({
        origin: publicOrigin,
        token,
        fetchImpl,
        method: "DELETE",
        path: `/api/releases/publication-lease/${encodeURIComponent(lease.leaseId)}`,
        body: { owner },
        expectedStatuses: new Set([204]),
        identity,
        owner,
        commit,
        noResponseBody: true,
      });
    } catch (cause) {
      if (!operationError) operationError = cause;
    }
  }
  if (operationError) throw operationError;
  return result;
}

async function workerLeaseRequest({
  origin,
  token,
  fetchImpl,
  method,
  path,
  body,
  expectedStatuses,
  identity,
  owner,
  commit,
  noResponseBody = false,
}) {
  const url = new URL(path, `${origin}/`);
  if (url.origin !== origin || !url.pathname.startsWith("/api/releases/publication-lease/") ||
    url.search || url.hash) {
    throw new MirrorValidationError("publication lease URL is invalid");
  }
  const controller = new AbortController();
  const timeout = setTimeout(
    () => controller.abort(new Error("publication lease request timed out")),
    githubMutationTimeoutMs,
  );
  timeout.unref?.();
  let response;
  try {
    response = await fetchImpl(url, {
      method,
      redirect: "manual",
      signal: controller.signal,
      headers: {
        accept: "application/json",
        authorization: `Bearer ${token}`,
        "content-type": "application/json",
        "user-agent": "nanocodex-cloudflare-github-compatibility-mirror/1",
      },
      body: JSON.stringify(body),
    });
  } catch (cause) {
    throw new PublicationLeaseLostError("publication lease request failed", { cause });
  } finally {
    clearTimeout(timeout);
  }
  if (!(response instanceof Response) || response.redirected || isRedirectStatus(response.status) ||
    response.type === "opaqueredirect") {
    await cancelResponse(response).catch(() => undefined);
    throw new PublicationLeaseLostError("publication lease request redirected or was invalid");
  }
  if (!expectedStatuses.has(response.status)) {
    const detail = response.body == null
      ? ""
      : new TextDecoder().decode(await readBoundedResponse(
          response,
          64 * 1024,
          "publication lease error",
        )).slice(0, 512);
    throw new PublicationLeaseLostError(
      `publication lease request returned HTTP ${response.status}${detail ? `: ${detail}` : ""}`,
    );
  }
  if (noResponseBody) {
    if (response.body != null) {
      const bytes = await readBoundedResponse(response, 1, "publication lease release");
      if (bytes.byteLength !== 0) {
        throw new PublicationLeaseLostError("publication lease release returned a body");
      }
    }
    return null;
  }
  const bytes = await readBoundedResponse(response, 64 * 1024, "publication lease response");
  return validateWorkerLease(
    parseJsonBytes(bytes, "publication lease response"),
    identity,
    owner,
    commit,
  );
}

export function validateWorkerLease(value, identity, owner, commit) {
  if (!exactRecord(value, [
    "version",
    "leaseId",
    "owner",
    "kind",
    "id",
    "commit",
    "generation",
    "expiresAt",
  ]) || value.version !== 1 || value.owner !== owner || value.kind !== identity.kind ||
    value.id !== identity.id || value.commit !== commit ||
    !Number.isSafeInteger(value.generation) || value.generation <= 0 ||
    typeof value.leaseId !== "string" ||
    !new RegExp(`^${value.generation}\\.[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$`).test(value.leaseId) ||
    !canonicalTimestamp(value.expiresAt) || Date.parse(value.expiresAt) <= Date.now()
  ) throw new PublicationLeaseLostError("publication lease response has the wrong identity");
  return deepFreeze({ ...value });
}

function validateWorkerToken(token) {
  if (typeof token !== "string" || token.length === 0 || token.length > 4096 || /[\0\r\n]/.test(token)) {
    throw new MirrorValidationError("CI_RELEASE_TOKEN is missing or invalid");
  }
}

function validLeaseOwner(value) {
  return typeof value === "string" && value.length >= 1 && value.length <= 192 &&
    /^[A-Za-z0-9][A-Za-z0-9._:@/-]*$/.test(value);
}

export function validateChecksumManifest(bytes, manifestAssets) {
  if (!(bytes instanceof Uint8Array) || bytes.byteLength === 0 || bytes.byteLength > 64 * 1024) {
    throw new MirrorValidationError("SHA256SUMS has an invalid size");
  }
  if (bytes.byteLength >= 3 && bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf) {
    throw new MirrorValidationError("SHA256SUMS must not contain a UTF-8 BOM");
  }
  let text;
  try {
    text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch (cause) {
    throw new MirrorValidationError("SHA256SUMS is not UTF-8", { cause });
  }
  const manifestByName = new Map(manifestAssets.map((asset) => [asset.name, asset]));
  const checksums = new Map();
  for (const line of text.split("\n")) {
    if (line === "") continue;
    const match = /^([a-fA-F0-9]{64})[ \t]+\*?([A-Za-z0-9][A-Za-z0-9._-]{0,159})$/.exec(line);
    if (!match || checksums.has(match[2])) {
      throw new MirrorValidationError("SHA256SUMS contains an invalid or duplicate entry");
    }
    const asset = manifestByName.get(match[2]);
    const digest = match[1].toLowerCase();
    if (!asset || asset.name === "SHA256SUMS" || asset.sha256 !== digest) {
      throw new MirrorValidationError(`SHA256SUMS conflicts with the manifest for ${match[2]}`);
    }
    checksums.set(match[2], digest);
  }
  for (const name of compatibilityAssetNames.filter((value) => value !== "SHA256SUMS")) {
    if (checksums.get(name) !== manifestByName.get(name)?.sha256) {
      throw new MirrorValidationError(`SHA256SUMS does not contain the exact ${name} digest`);
    }
  }
  return checksums;
}

export function planCompatibilityMirror(source) {
  validateSourceObject(source);
  const nightly = source.identity.channel === "nightly";
  const githubTag = nightly ? "nightly" : source.identity.id;
  const releaseName = nightly ? "Nanocodex Nightly" : `Nanocodex ${source.identity.id}`;
  return deepFreeze({
    version: 1,
    repository,
    channel: source.identity.channel,
    immutableId: source.identity.id,
    commit: source.manifest.commit,
    manifestUrl: source.manifestUrl,
    manifestSha256: source.manifest.manifestSha256,
    githubTag,
    releaseName,
    prerelease: nightly,
    makeLatest: !nightly,
    assets: source.assets.map(({ bytes: _bytes, ...asset }) => ({
      name: asset.name,
      platform: asset.platform,
      size: asset.size,
      sha256: asset.sha256,
      contentType: asset.contentType,
      sourceUrl: new URL(asset.downloadPath, `${source.origin}/`).href,
    })),
  });
}

export async function runMirrorCommand({
  args = process.argv.slice(2),
  env = process.env,
  fetchImpl = fetch,
  clientFactory = createGitHubChildClient,
  sleep = defaultSleep,
  lockPath = resolve(tmpdir(), "nanocodex-cloudflare-github-compatibility-mirror.lock"),
} = {}) {
  const options = parseMirrorArguments(args);
  if (options.help) return Object.freeze({ help: true, text: mirrorHelpText() });
  if (!isRecord(env) || typeof fetchImpl !== "function" ||
    typeof clientFactory !== "function" || typeof sleep !== "function") {
    throw new TypeError("mirror command dependencies are invalid");
  }
  const origin = parsePublicOrigin(options.origin ?? defaultPublicOrigin);
  if (options.dryRun) {
    const source = await fetchCanonicalCompatibilityRelease({
      origin,
      channel: options.channel,
      id: options.id,
      fetchImpl,
    });
    return deepFreeze({ help: false, mode: "dry-run", plan: planCompatibilityMirror(source) });
  }
  if (origin !== defaultPublicOrigin) {
    throw new MirrorValidationError(
      `live mirroring is pinned to the authenticated release origin ${defaultPublicOrigin}`,
    );
  }
  validateGitHubToken(env.NANOCODEX_GITHUB_RELEASE_TOKEN);
  validateWorkerToken(env.CI_RELEASE_TOKEN);

  return withLocalMirrorLock({ path: lockPath }, async () => {
    const preflightClient = clientFactory({
      token: env.NANOCODEX_GITHUB_RELEASE_TOKEN,
    });
    const preflight = new GitHubRepository({
      client: preflightClient,
      publicFetch: fetchImpl,
      sleep,
    });
    await assertGitHubMutationPrerequisites(preflight, options.channel);
    const stableTagAuthority = options.channel === "stable"
      ? captureStableTagAuthority(await preflight.getRef(options.id), options.id)
      : null;
    const commit = stableTagAuthority?.commit ?? options.id;

    return withWorkerPublicationLease({
      origin,
      token: env.CI_RELEASE_TOKEN,
      channel: options.channel,
      id: options.id,
      commit,
      fetchImpl,
    }, async (authority) => {
      const liveClient = clientFactory({
        token: env.NANOCODEX_GITHUB_RELEASE_TOKEN,
        signal: authority.signal,
      });
      const assertAuthority = async () => {
        await authority.assertHeld();
        await assertGitHubMutationPrerequisites(preflight, options.channel);
        if (stableTagAuthority != null) {
          assertStableTagAuthorityUnchanged(
            await preflight.getRef(stableTagAuthority.tag),
            stableTagAuthority,
          );
        }
        await authority.assertHeld();
      };
      const github = new GitHubRepository({
        client: liveClient,
        publicFetch: fetchImpl,
        sleep,
        assertAuthority,
        intendedTag: options.channel === "nightly" ? "nightly" : options.id,
      });
      const fetchCurrentSource = () => fetchCanonicalCompatibilityRelease({
        origin,
        channel: options.channel,
        id: options.id,
        fetchImpl,
      });
      const source = await fetchCurrentSource();
      if (source.manifest.commit !== commit) {
        throw new MirrorConflictError(
          `canonical Worker commit ${source.manifest.commit} conflicts with GitHub tag commit ${commit}`,
        );
      }
      await assertAuthority();
      const result = await mirrorCompatibilityRelease({
        source,
        github,
        refreshSource: fetchCurrentSource,
        fetchImmutableSource: ({ channel, id }) => fetchImmutableCompatibilityRelease({
          origin,
          channel,
          id,
          fetchImpl,
        }),
      });
      await assertAuthority();
      return deepFreeze({ help: false, mode: "live", ...result });
    });
  });
}

export async function withLocalMirrorLock({ path }, operation) {
  if (typeof path !== "string" || path.length === 0 || path.length > 4096 ||
    typeof operation !== "function") {
    throw new TypeError("local mirror lock input is invalid");
  }
  let handle;
  try {
    handle = await open(path, "wx", 0o600);
  } catch (cause) {
    if (cause?.code === "EEXIST") {
      throw new MirrorConflictError(
        `another compatibility mirror invocation owns ${path}; remove it only after proving that process is gone`,
        { cause },
      );
    }
    throw cause;
  }
  let result;
  let operationError;
  try {
    await handle.writeFile(`${process.pid} ${randomUUID()}\n`, { encoding: "utf8" });
    result = await operation();
  } catch (cause) {
    operationError = cause;
  } finally {
    await handle.close().catch((cause) => {
      operationError ??= cause;
    });
    await unlink(path).catch((cause) => {
      if (cause?.code !== "ENOENT") operationError ??= cause;
    });
  }
  if (operationError) throw operationError;
  return result;
}

async function assertGitHubMutationPrerequisites(github, channel) {
  await assertLegacyPublisherDisabled(github, channel);
  if (channel === "nightly") await github.assertRollingReleasePolicyMutable();
}

function mirrorHelpText() {
  return [
    "Usage: mirror-cloudflare-release-to-github.mjs [--dry-run] [--origin ORIGIN] <stable|nightly> <immutable-id>",
    "",
    "Live mode requires NANOCODEX_GITHUB_RELEASE_TOKEN and CI_RELEASE_TOKEN.",
    "The GitHub token needs Actions(read) and Contents(write); nightly also needs Administration(read).",
    "Disable the relevant legacy GitHub Actions workflow and prove it has no active runs first.",
    "For stable releases, disable release.yml before pushing the release tag; an unowned draft is refused.",
    "Stable mirroring also requires protected v* tags and exclusive GitHub release/tag administrators for the invocation.",
    "Live authenticated Worker requests are pinned to the production origin. --origin is fixture/dry-run only.",
  ].join("\n");
}

function validateWorkerAssetResponse(response, asset, releaseId) {
  assertResponseStatus(response, 200, `immutable release asset ${asset.name}`);
  for (const [name, expected] of [
    ["cache-control", immutableCacheControl],
    ["content-disposition", `attachment; filename="${asset.name}"`],
    ["content-length", String(asset.size)],
    ["content-type", asset.contentType],
    ["etag", `"${asset.sha256}"`],
    ["x-nanocodex-release", releaseId],
    ["x-nanocodex-sha256", asset.sha256],
  ]) assertExactHeader(response, name, expected, `immutable release asset ${asset.name}`);
  for (const forbidden of ["content-encoding", "content-location", "content-range"]) {
    if (response.headers.has(forbidden)) {
      throw new MirrorValidationError(
        `immutable release asset ${asset.name} unexpectedly has ${forbidden}`,
      );
    }
  }
}

async function publicWorkerFetch(url, { fetchImpl, timeoutMs, operation }) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(new Error(`${operation} timed out`)), timeoutMs);
  timeout.unref?.();
  let response;
  try {
    response = await fetchImpl(url, {
      method: "GET",
      redirect: "manual",
      signal: controller.signal,
      headers: {
        accept: "application/json, application/octet-stream;q=0.9, text/plain;q=0.8",
        "accept-encoding": "identity",
        "user-agent": "nanocodex-cloudflare-github-compatibility-mirror/1",
      },
    });
  } catch (cause) {
    throw new MirrorValidationError(`failed to fetch ${operation}`, { cause });
  } finally {
    clearTimeout(timeout);
  }
  if (!(response instanceof Response)) {
    throw new MirrorValidationError(`${operation} fetch returned a non-Response value`);
  }
  if (response.redirected || isRedirectStatus(response.status) || response.type === "opaqueredirect") {
    await cancelResponse(response);
    throw new MirrorValidationError(`${operation} redirected; canonical Worker URLs must not redirect`);
  }
  return response;
}

async function readBoundedResponse(response, maximumBytes, operation, expectedBytes) {
  const declared = strictContentLength(response.headers.get("content-length"));
  if (declared != null && declared > maximumBytes) {
    await cancelResponse(response);
    throw new MirrorValidationError(`${operation} exceeds its ${maximumBytes}-byte limit`);
  }
  if (expectedBytes != null && declared != null && declared !== expectedBytes) {
    await cancelResponse(response);
    throw new MirrorValidationError(
      `${operation} content-length is ${String(declared)}, expected ${expectedBytes}`,
    );
  }
  if (response.body == null) {
    throw new MirrorValidationError(`${operation} response has no body`);
  }
  const reader = response.body.getReader();
  const exactBytes = expectedBytes ?? (Number.isSafeInteger(declared) ? declared : null);
  const bytes = exactBytes == null ? null : new Uint8Array(exactBytes);
  const chunks = bytes == null ? [] : null;
  let size = 0;
  try {
    while (true) {
      const chunk = await reader.read();
      if (chunk.done) break;
      size += chunk.value.byteLength;
      if (size > maximumBytes || (expectedBytes != null && size > expectedBytes) ||
        (Number.isSafeInteger(declared) && size > declared)) {
        await reader.cancel().catch(() => undefined);
        throw new MirrorValidationError(`${operation} body exceeds its declared bound`);
      }
      if (bytes == null) chunks.push(chunk.value);
      else bytes.set(chunk.value, size - chunk.value.byteLength);
    }
  } finally {
    reader.releaseLock();
  }
  if (declared != null && size !== declared) {
    throw new MirrorValidationError(`${operation} body length ${size} does not match ${declared}`);
  }
  if (expectedBytes != null && size !== expectedBytes) {
    throw new MirrorValidationError(`${operation} body length ${size} does not match ${expectedBytes}`);
  }
  if (bytes != null) return bytes;
  const collected = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    collected.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return collected;
}

function parseJsonBytes(bytes, operation) {
  let text;
  try {
    text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch (cause) {
    throw new MirrorValidationError(`${operation} is not UTF-8`, { cause });
  }
  try {
    return JSON.parse(text);
  } catch (cause) {
    throw new MirrorValidationError(`${operation} is not valid JSON`, { cause });
  }
}

function immutableManifestPath(identity) {
  return `/api/releases/releases/${identity.kind}/${encodeURIComponent(identity.id)}`;
}

function immutableAssetPath(identity, name) {
  return `${immutableManifestPath(identity)}/assets/${encodeURIComponent(name)}`;
}

export function githubChildEnvironment(token) {
  validateGitHubToken(token);
  return Object.freeze({ NANOCODEX_GITHUB_RELEASE_TOKEN: token });
}

export function createGitHubChildClient({
  token,
  runChild = runGitHubChildRequest,
  signal,
} = {}) {
  validateGitHubToken(token);
  if (typeof runChild !== "function") throw new TypeError("runChild must be a function");
  if (signal != null && !(signal instanceof AbortSignal)) {
    throw new TypeError("GitHub child signal must be an AbortSignal");
  }
  return Object.freeze({
    async request({ method, url, json, bytes, contentType, timeoutMs } = {}) {
      if (!new Set(["GET", "POST", "PATCH", "DELETE"]).has(method)) {
        throw new TypeError("GitHub child request method is invalid");
      }
      const requestUrl = validateGitHubApiUrl(url, method);
      if (json !== undefined && bytes !== undefined) {
        throw new TypeError("GitHub child request may have only one body");
      }
      let body = new Uint8Array();
      let requestContentType;
      if (json !== undefined) {
        body = new TextEncoder().encode(JSON.stringify(json));
        requestContentType = "application/json";
      } else if (bytes !== undefined) {
        if (!(bytes instanceof Uint8Array)) throw new TypeError("GitHub upload body must be bytes");
        body = bytes;
        requestContentType = contentType;
      }
      if (body.byteLength > 256 * 1024 * 1024) {
        throw new MirrorValidationError("GitHub request body exceeds 256 MiB");
      }
      if (
        requestContentType != null &&
        (typeof requestContentType !== "string" || requestContentType.length > 160 ||
          !contentTypePattern.test(requestContentType))
      ) throw new MirrorValidationError("GitHub request content type is invalid");
      const requestTimeout = timeoutMs ?? (body.byteLength > 0
        ? githubUploadTimeoutMs
        : githubMetadataTimeoutMs);
      if (!Number.isSafeInteger(requestTimeout) || requestTimeout <= 0 || requestTimeout > 15 * 60_000) {
        throw new TypeError("GitHub child timeout is invalid");
      }
      const descriptor = Object.freeze({
        version: 1,
        method,
        url: requestUrl.href,
        contentType: requestContentType ?? null,
        bodyLength: body.byteLength,
        timeoutMs: requestTimeout,
      });
      validateGitHubRequestBody(descriptor, body);
      const result = await runChild({ descriptor, body, token, signal });
      return validateGitHubChildResult(result);
    },
  });
}

export async function runGitHubChildRequest({ descriptor, body, token, signal } = {}) {
  validateGitHubChildDescriptor(descriptor);
  if (!(body instanceof Uint8Array) || body.byteLength !== descriptor.bodyLength) {
    throw new TypeError("GitHub child body does not match its descriptor");
  }
  if (signal != null && !(signal instanceof AbortSignal)) {
    throw new TypeError("GitHub child signal must be an AbortSignal");
  }
  if (signal?.aborted) {
    throw new PublicationLeaseLostError("GitHub child was fenced before it started", {
      cause: signal.reason,
    });
  }
  const encoded = Buffer.from(JSON.stringify(descriptor), "utf8").toString("base64url");
  const child = spawn(process.execPath, [scriptPath, "--github-api-child", encoded], {
    env: githubChildEnvironment(token),
    stdio: ["pipe", "pipe", "pipe"],
  });
  const stdout = [];
  const stderr = [];
  let stdoutBytes = 0;
  let stderrBytes = 0;
  let overflow;
  child.stdout.on("data", (chunk) => {
    stdoutBytes += chunk.byteLength;
    if (stdoutBytes > maximumChildOutputBytes) {
      overflow = new GitHubTransportError("GitHub child exceeded its stdout limit");
      child.kill("SIGKILL");
      return;
    }
    stdout.push(chunk);
  });
  child.stderr.on("data", (chunk) => {
    stderrBytes += chunk.byteLength;
    if (stderrBytes <= maximumChildErrorBytes) stderr.push(chunk);
  });
  const timeout = setTimeout(() => {
    overflow = new GitHubTransportError("GitHub child timed out");
    child.kill("SIGKILL");
  }, descriptor.timeoutMs + 5_000);
  timeout.unref?.();
  const abortChild = () => {
    overflow ??= new PublicationLeaseLostError("GitHub child was fenced by authority loss", {
      cause: signal?.reason,
    });
    child.kill("SIGKILL");
  };
  signal?.addEventListener("abort", abortChild, { once: true });
  const completion = new Promise((resolveCompletion, rejectCompletion) => {
    child.once("error", rejectCompletion);
    child.once("close", (code, signal) => resolveCompletion({ code, signal }));
  });
  child.stdin.on("error", (cause) => {
    if (cause?.code !== "EPIPE") overflow ??= new GitHubTransportError(
      "failed to send bytes to GitHub child",
      { cause },
    );
  });
  child.stdin.end(body);
  let status;
  try {
    status = await completion;
  } catch (cause) {
    throw new GitHubTransportError("failed to start GitHub child", { cause });
  } finally {
    clearTimeout(timeout);
    signal?.removeEventListener("abort", abortChild);
  }
  if (overflow) throw overflow;
  if (status.code !== 0) {
    const detail = Buffer.concat(stderr).toString("utf8").trim().slice(0, 1024);
    throw new GitHubTransportError(
      `GitHub child exited ${status.code ?? `on ${status.signal}`}${detail ? `: ${detail}` : ""}`,
    );
  }
  let value;
  try {
    value = JSON.parse(Buffer.concat(stdout).toString("utf8"));
  } catch (cause) {
    throw new GitHubTransportError("GitHub child returned invalid JSON", { cause });
  }
  return value;
}

export async function githubApiChildMain(encodedDescriptor, {
  stdin = process.stdin,
  stdout = process.stdout,
  fetchImpl = fetch,
  env = process.env,
} = {}) {
  let descriptor;
  try {
    descriptor = JSON.parse(Buffer.from(encodedDescriptor, "base64url").toString("utf8"));
  } catch (cause) {
    throw new GitHubTransportError("GitHub child descriptor is invalid", { cause });
  }
  validateGitHubChildDescriptor(descriptor);
  validateGitHubToken(env.NANOCODEX_GITHUB_RELEASE_TOKEN);
  const body = await readNodeStreamBounded(
    stdin,
    descriptor.bodyLength,
    "GitHub child request body",
  );
  validateGitHubRequestBody(descriptor, body);
  const controller = new AbortController();
  const timeout = setTimeout(
    () => controller.abort(new Error("GitHub API request timed out")),
    descriptor.timeoutMs,
  );
  timeout.unref?.();
  let response;
  try {
    response = await fetchImpl(descriptor.url, {
      method: descriptor.method,
      redirect: "manual",
      signal: controller.signal,
      headers: {
        accept: "application/vnd.github+json",
        authorization: `Bearer ${env.NANOCODEX_GITHUB_RELEASE_TOKEN}`,
        "user-agent": "nanocodex-cloudflare-github-compatibility-mirror/1",
        "x-github-api-version": githubApiVersion,
        ...(descriptor.contentType == null
          ? {}
          : {
              "content-type": descriptor.contentType,
              "content-length": String(body.byteLength),
            }),
      },
      ...(body.byteLength === 0 ? {} : { body }),
    });
  } catch (cause) {
    throw new GitHubTransportError("GitHub API request failed", { cause });
  } finally {
    clearTimeout(timeout);
  }
  if (!(response instanceof Response)) {
    throw new GitHubTransportError("GitHub API child fetch returned a non-Response value");
  }
  if (response.redirected || isRedirectStatus(response.status) || response.type === "opaqueredirect") {
    await cancelResponse(response);
    throw new GitHubTransportError("GitHub API redirected an authenticated request");
  }
  const responseBytes = response.body == null
    ? new Uint8Array()
    : await readBoundedResponse(
        response,
        maximumGitHubJsonBytes,
        "GitHub API response",
      );
  const result = {
    version: 1,
    status: response.status,
    headers: Object.fromEntries(
      ["content-type", "retry-after", "x-github-request-id"]
        .map((name) => [name, response.headers.get(name)])
        .filter(([, value]) => value != null),
    ),
    body: bufferView(responseBytes).toString("base64"),
  };
  stdout.write(JSON.stringify(result));
}

function validateGitHubChildDescriptor(value) {
  if (!exactRecord(value, [
    "version",
    "method",
    "url",
    "contentType",
    "bodyLength",
    "timeoutMs",
  ])) throw new MirrorValidationError("GitHub child descriptor has unexpected fields");
  if (
    value.version !== 1 || !new Set(["GET", "POST", "PATCH", "DELETE"]).has(value.method) ||
    !Number.isSafeInteger(value.bodyLength) || value.bodyLength < 0 ||
    value.bodyLength > 256 * 1024 * 1024 || !Number.isSafeInteger(value.timeoutMs) ||
    value.timeoutMs <= 0 || value.timeoutMs > 15 * 60_000 ||
    (value.contentType !== null &&
      (typeof value.contentType !== "string" || value.contentType.length > 160 ||
        !contentTypePattern.test(value.contentType))) ||
    (value.bodyLength === 0) !== (value.contentType === null)
  ) throw new MirrorValidationError("GitHub child descriptor is invalid");
  validateGitHubApiUrl(value.url, value.method);
  return value;
}

function validateGitHubChildResult(value) {
  if (!exactRecord(value, ["version", "status", "headers", "body"]) || value.version !== 1 ||
    !Number.isSafeInteger(value.status) || value.status < 100 || value.status > 599 ||
    !isRecord(value.headers) || typeof value.body !== "string" ||
    !Object.keys(value.headers).every((name) =>
      ["content-type", "retry-after", "x-github-request-id"].includes(name) &&
      typeof value.headers[name] === "string" && value.headers[name].length <= 1024
    )) throw new GitHubTransportError("GitHub child result is invalid");
  let body;
  try {
    body = Buffer.from(value.body, "base64");
  } catch (cause) {
    throw new GitHubTransportError("GitHub child result body is invalid", { cause });
  }
  if (body.byteLength > maximumGitHubJsonBytes) {
    throw new GitHubTransportError("GitHub child result body is oversized");
  }
  return Object.freeze({ status: value.status, headers: Object.freeze({ ...value.headers }), body });
}

function validateGitHubApiUrl(value, method) {
  let url;
  try {
    url = new URL(value);
  } catch (cause) {
    throw new MirrorValidationError("GitHub API URL is invalid", { cause });
  }
  if (url.username || url.password || url.hash) {
    throw new MirrorValidationError("GitHub API URL contains credentials or a fragment");
  }
  const repositoryPath = `/repos/${repository}`;
  const api = url.origin === githubApiOrigin && allowedGitHubApiEndpoint(url, method);
  const upload = url.origin === githubUploadsOrigin &&
    /^\/repos\/gakonst\/nanocodex\/releases\/[1-9][0-9]*\/assets$/.test(url.pathname) &&
    method === "POST";
  if (!api && !upload) throw new MirrorValidationError("GitHub API URL escaped the fixed operation allowlist");
  if (upload) {
    const entries = [...url.searchParams.entries()];
    if (
      entries.length !== 1 || entries[0][0] !== "name" ||
      !compatibilityAssetSpecifications.has(entries[0][1])
    ) {
      throw new MirrorValidationError("GitHub upload URL has invalid parameters");
    }
  }
  return url;
}

function allowedGitHubApiEndpoint(url, method) {
  const base = `/repos/${repository}`;
  const noQuery = url.search === "";
  if (method === "GET") {
    if (url.pathname === `${base}/releases` && exactSearch(url, [
      ["per_page", "30"],
      ["page", /^[1-9][0-9]*$/],
    ])) return true;
    if (/^\/repos\/gakonst\/nanocodex\/releases\/[1-9][0-9]*$/.test(url.pathname) && noQuery) {
      return true;
    }
    if (url.pathname === `${base}/releases/latest` && noQuery) return true;
    if (/^\/repos\/gakonst\/nanocodex\/releases\/[1-9][0-9]*\/assets$/.test(url.pathname) &&
      exactSearch(url, [
        ["per_page", "100"],
        ["page", /^(?:[1-9]|10)$/],
      ])) return true;
    if (/^\/repos\/gakonst\/nanocodex\/git\/ref\/tags\/(?:nightly|v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))$/.test(url.pathname) && noQuery) {
      return true;
    }
    if (/^\/repos\/gakonst\/nanocodex\/git\/tags\/[a-f0-9]{40}$/.test(url.pathname) && noQuery) {
      return true;
    }
    if (/^\/repos\/gakonst\/nanocodex\/actions\/workflows\/(?:release\.yml|nightly\.yml)$/.test(url.pathname) && noQuery) {
      return true;
    }
    if (url.pathname === `${base}/actions/permissions` && noQuery) return true;
    if (url.pathname === `${base}/immutable-releases` && noQuery) return true;
    if (/^\/repos\/gakonst\/nanocodex\/actions\/workflows\/(?:release\.yml|nightly\.yml)\/runs$/.test(url.pathname) &&
      exactSearch(url, [
        ["status", new Set(["requested", "waiting", "pending", "queued", "in_progress"])],
        ["per_page", "1"],
      ])) return true;
    return false;
  }
  if (method === "POST") {
    return noQuery && (url.pathname === `${base}/releases` || url.pathname === `${base}/git/refs`);
  }
  if (method === "PATCH") {
    return noQuery && (
      /^\/repos\/gakonst\/nanocodex\/releases\/[1-9][0-9]*$/.test(url.pathname) ||
      url.pathname === `${base}/git/refs/tags/nightly`
    );
  }
  if (method === "DELETE") {
    return noQuery && /^\/repos\/gakonst\/nanocodex\/releases\/assets\/[1-9][0-9]*$/.test(
      url.pathname,
    );
  }
  return false;
}

function exactSearch(url, specifications) {
  const entries = [...url.searchParams.entries()];
  if (entries.length !== specifications.length) return false;
  const values = new Map(entries);
  if (values.size !== entries.length) return false;
  return specifications.every(([name, expected]) => {
    const value = values.get(name);
    return typeof expected === "string"
      ? value === expected
      : expected instanceof RegExp
      ? expected.test(value ?? "")
      : expected.has(value);
  });
}

function validateGitHubRequestBody(descriptor, body) {
  const url = new URL(descriptor.url);
  const upload = url.origin === githubUploadsOrigin;
  if (upload) {
    if (descriptor.method !== "POST" || body.byteLength === 0 || descriptor.contentType == null) {
      throw new MirrorValidationError("GitHub upload request is invalid");
    }
    return;
  }
  if (descriptor.method === "GET" || descriptor.method === "DELETE") {
    if (body.byteLength !== 0 || descriptor.contentType !== null) {
      throw new MirrorValidationError("GitHub read/delete request unexpectedly has a body");
    }
    return;
  }
  if (descriptor.contentType !== "application/json" || body.byteLength === 0 || body.byteLength > 64 * 1024) {
    throw new MirrorValidationError("GitHub mutation JSON body is invalid");
  }
  const value = parseJsonBytes(body, "GitHub mutation request");
  if (url.pathname.endsWith("/git/refs")) {
    if (!exactRecord(value, ["ref", "sha"]) || value.ref !== "refs/tags/nightly") {
      throw new MirrorValidationError("GitHub ref creation is limited to refs/tags/nightly");
    }
    assertSha1(value.sha, "GitHub nightly ref commit");
  } else if (url.pathname.endsWith("/git/refs/tags/nightly")) {
    if (!exactRecord(value, ["sha", "force"]) || value.force !== true) {
      throw new MirrorValidationError("GitHub nightly ref update is invalid");
    }
    assertSha1(value.sha, "GitHub nightly ref commit");
  } else {
    validateReleaseMutation(value, descriptor.method === "POST");
  }
}

function validateGitHubToken(token) {
  if (
    typeof token !== "string" || token.length < 1 || token.length > 4096 ||
    /[\0\r\n]/.test(token)
  ) throw new MirrorValidationError("NANOCODEX_GITHUB_RELEASE_TOKEN is missing or invalid");
}

async function readNodeStreamBounded(stream, expectedBytes, operation) {
  const bytes = new Uint8Array(expectedBytes);
  let size = 0;
  for await (const chunk of stream) {
    size += chunk.byteLength;
    if (size > expectedBytes) throw new GitHubTransportError(`${operation} exceeds ${expectedBytes} bytes`);
    bytes.set(chunk, size - chunk.byteLength);
  }
  if (size !== expectedBytes) {
    throw new GitHubTransportError(`${operation} has ${size} bytes, expected ${expectedBytes}`);
  }
  return bytes;
}

export class GitHubRepository {
  constructor({
    client,
    publicFetch = fetch,
    sleep = defaultSleep,
    assertAuthority = async () => undefined,
    intendedTag,
  } = {}) {
    if (!client || typeof client.request !== "function") {
      throw new TypeError("GitHubRepository requires a request client");
    }
    if (
      typeof publicFetch !== "function" || typeof sleep !== "function" ||
      typeof assertAuthority !== "function"
    ) {
      throw new TypeError("GitHubRepository fetch, sleep, and authority boundaries must be functions");
    }
    this.client = client;
    this.publicFetch = publicFetch;
    this.sleep = sleep;
    this.assertAuthority = assertAuthority;
    this.intendedTag = intendedTag == null ? null : validateGitHubTag(intendedTag);
    this.mutableReleaseIds = new Map();
    this.mutableAssetIds = new Map();
  }

  async getReleaseByTag(tag) {
    validateGitHubTag(tag);
    const matches = (await this.#enumerateReleaseValues())
      .filter((value) => value.tag_name === tag)
      .map((value) => parseGitHubReleaseMetadata(value, tag));
    if (matches.length > 1) {
      throw new MirrorConflictError(`GitHub contains duplicate releases for tag ${tag}`);
    }
    if (matches.length === 0) return null;
    const release = await this.#hydrateRelease(matches[0], tag);
    this.#authorizeIntendedRelease(release);
    return release;
  }

  async getPublishedStableReleases() {
    const releases = (await this.#enumerateReleaseValues())
      .filter((value) => stableTagPattern.test(value.tag_name))
      .map((value) => parseGitHubReleaseMetadata(value, value.tag_name))
      .filter((release) => !release.draft && !release.prerelease);
    const tags = new Set();
    for (const release of releases) {
      if (tags.has(release.tag)) {
        throw new MirrorConflictError(
          `GitHub contains duplicate published stable releases for ${release.tag}`,
        );
      }
      tags.add(release.tag);
    }
    return deepFreeze(releases);
  }

  async getRelease(id, expectedTag) {
    validatePositiveId(id, "GitHub release id");
    const response = await this.client.request({
      method: "GET",
      url: apiUrl(`/repos/${repository}/releases/${id}`),
      timeoutMs: githubMetadataTimeoutMs,
    });
    if (response.status === 404) return null;
    assertGitHubStatus(response, 200, `read GitHub release ${id}`);
    const metadata = parseGitHubReleaseMetadata(
      responseJson(response, `GitHub release ${id}`),
      expectedTag,
    );
    const release = await this.#hydrateRelease(metadata, expectedTag);
    if (this.mutableReleaseIds.get(id) === expectedTag) {
      for (const asset of release.assets) this.mutableAssetIds.set(asset.id, id);
    }
    return release;
  }

  async getLatestRelease() {
    const response = await this.client.request({
      method: "GET",
      url: apiUrl(`/repos/${repository}/releases/latest`),
      timeoutMs: githubMetadataTimeoutMs,
    });
    if (response.status === 404) return null;
    assertGitHubStatus(response, 200, "read latest GitHub release");
    const metadata = parseGitHubReleaseMetadata(responseJson(response, "latest GitHub release"));
    return this.#hydrateRelease(metadata, metadata.tag);
  }

  async createRelease(input) {
    validateReleaseMutation(input, true);
    this.#assertIntendedTag(input.tag_name);
    const response = await this.#authorizedMutation(() => this.client.request({
      method: "POST",
      url: apiUrl(`/repos/${repository}/releases`),
      json: input,
      timeoutMs: githubMutationTimeoutMs,
    }));
    assertGitHubStatus(response, 201, `create GitHub release ${input.tag_name}`);
    const release = parseGitHubReleaseMetadata(
      responseJson(response, `created GitHub release ${input.tag_name}`),
      input.tag_name,
    );
    this.#authorizeIntendedRelease({ ...release, assets: [] });
    return release;
  }

  async updateRelease(id, expectedTag, input) {
    validatePositiveId(id, "GitHub release id");
    validateReleaseMutation(input, false);
    this.#assertAuthorizedRelease(id, expectedTag);
    const response = await this.#authorizedMutation(() => this.client.request({
      method: "PATCH",
      url: apiUrl(`/repos/${repository}/releases/${id}`),
      json: input,
      timeoutMs: githubMutationTimeoutMs,
    }));
    assertGitHubStatus(response, 200, `update GitHub release ${id}`);
    return parseGitHubReleaseMetadata(
      responseJson(response, `updated GitHub release ${id}`),
      expectedTag,
    );
  }

  async getRef(tag) {
    validateGitHubTag(tag);
    const response = await this.client.request({
      method: "GET",
      url: apiUrl(`/repos/${repository}/git/ref/tags/${encodeURIComponent(tag)}`),
      timeoutMs: githubMetadataTimeoutMs,
    });
    if (response.status === 404) return null;
    assertGitHubStatus(response, 200, `read GitHub tag ${tag}`);
    const ref = parseGitHubReference(responseJson(response, `GitHub tag ${tag}`), tag);
    const seen = new Set();
    const tagObjects = [];
    let object = ref.object;
    for (let depth = 0; object.type === "tag"; depth += 1) {
      if (depth >= 8 || seen.has(object.sha)) {
        throw new MirrorConflictError(`GitHub tag ${tag} has a cyclic or overly deep tag chain`);
      }
      seen.add(object.sha);
      const tagResponse = await this.client.request({
        method: "GET",
        url: apiUrl(`/repos/${repository}/git/tags/${object.sha}`),
        timeoutMs: githubMetadataTimeoutMs,
      });
      assertGitHubStatus(tagResponse, 200, `peel GitHub tag ${tag}`);
      const annotated = parseGitHubTagObject(
        responseJson(tagResponse, `GitHub annotated tag ${tag}`),
        object.sha,
      );
      tagObjects.push(annotated);
      object = annotated.object;
    }
    if (object.type !== "commit") {
      throw new MirrorConflictError(`GitHub tag ${tag} resolves to ${object.type}, not a commit`);
    }
    return deepFreeze({ ...ref, tagObjects, commit: object.sha });
  }

  async createRef(tag, commit) {
    if (tag !== "nightly") {
      throw new MirrorValidationError("the compatibility bridge may create only refs/tags/nightly");
    }
    assertSha1(commit, "GitHub tag commit");
    const response = await this.#authorizedMutation(() => this.client.request({
      method: "POST",
      url: apiUrl(`/repos/${repository}/git/refs`),
      json: { ref: `refs/tags/${tag}`, sha: commit },
      timeoutMs: githubMutationTimeoutMs,
    }));
    assertGitHubStatus(response, 201, `create GitHub tag ${tag}`);
    return parseGitHubReference(responseJson(response, `created GitHub tag ${tag}`), tag);
  }

  async updateRef(tag, commit) {
    if (tag !== "nightly") {
      throw new MirrorValidationError("the compatibility bridge may update only refs/tags/nightly");
    }
    assertSha1(commit, "GitHub tag commit");
    const response = await this.#authorizedMutation(() => this.client.request({
      method: "PATCH",
      url: apiUrl(`/repos/${repository}/git/refs/tags/${encodeURIComponent(tag)}`),
      json: { sha: commit, force: true },
      timeoutMs: githubMutationTimeoutMs,
    }));
    assertGitHubStatus(response, 200, `update GitHub tag ${tag}`);
    return parseGitHubReference(responseJson(response, `updated GitHub tag ${tag}`), tag);
  }

  async uploadAsset(releaseId, asset) {
    validatePositiveId(releaseId, "GitHub release id");
    validateCompatibilityAsset(asset);
    this.#assertAuthorizedRelease(releaseId, this.intendedTag);
    const url = new URL(
      `/repos/${repository}/releases/${releaseId}/assets`,
      `${githubUploadsOrigin}/`,
    );
    url.searchParams.set("name", asset.name);
    const response = await this.#authorizedMutation(() => this.client.request({
      method: "POST",
      url: url.href,
      bytes: asset.bytes,
      contentType: asset.contentType,
      timeoutMs: githubUploadTimeoutMs,
    }));
    assertGitHubStatus(response, 201, `upload GitHub asset ${asset.name}`);
    const uploaded = parseGitHubAsset(
      responseJson(response, `uploaded GitHub asset ${asset.name}`),
      undefined,
    );
    this.mutableAssetIds.set(uploaded.id, releaseId);
    return uploaded;
  }

  async deleteAsset(releaseId, assetId) {
    validatePositiveId(releaseId, "GitHub release id");
    validatePositiveId(assetId, "GitHub asset id");
    this.#assertAuthorizedRelease(releaseId, this.intendedTag);
    if (this.mutableAssetIds.get(assetId) !== releaseId) {
      throw new MirrorConflictError("GitHub asset was not observed on the intended release");
    }
    const response = await this.#authorizedMutation(() => this.client.request({
      method: "DELETE",
      url: apiUrl(`/repos/${repository}/releases/assets/${assetId}`),
      timeoutMs: githubMutationTimeoutMs,
    }));
    assertGitHubStatus(response, 204, `delete GitHub asset ${assetId}`);
    this.mutableAssetIds.delete(assetId);
  }

  async verifyPublicAsset(githubAsset, expected) {
    return verifyPublicGitHubAsset({
      githubAsset,
      expected,
      fetchImpl: this.publicFetch,
      timeoutMs: publicVerificationTimeoutMs,
    });
  }

  async getPublicUpdaterRelease(channel) {
    if (channel !== "stable" && channel !== "nightly") {
      throw new MirrorValidationError("public updater channel is invalid");
    }
    const path = channel === "stable"
      ? `/repos/${repository}/releases/latest`
      : `/repos/${repository}/releases/tags/nightly`;
    const controller = new AbortController();
    const timeout = setTimeout(
      () => controller.abort(new Error("public GitHub release metadata timed out")),
      githubMetadataTimeoutMs,
    );
    timeout.unref?.();
    let response;
    try {
      response = await this.publicFetch(new URL(path, `${githubApiOrigin}/`), {
        method: "GET",
        redirect: "manual",
        signal: controller.signal,
        headers: {
          accept: "application/vnd.github+json",
          "user-agent": "nanocodex-cloudflare-github-compatibility-mirror/1",
          "x-github-api-version": githubApiVersion,
        },
      });
    } catch (cause) {
      throw new GitHubRequestError(`read public GitHub ${channel} release metadata`, {
        retryable: true,
        cause,
      });
    } finally {
      clearTimeout(timeout);
    }
    if (!(response instanceof Response) || response.redirected || isRedirectStatus(response.status) ||
      response.type === "opaqueredirect") {
      await cancelResponse(response).catch(() => undefined);
      throw new MirrorConflictError(`public GitHub ${channel} release metadata redirected`);
    }
    if (response.status === 404) {
      await cancelResponse(response);
      return null;
    }
    if (response.status !== 200) {
      await cancelResponse(response);
      throw new GitHubRequestError(`read public GitHub ${channel} release metadata`, {
        status: response.status,
        retryable: retryableStatus(response.status),
      });
    }
    const bytes = await readBoundedResponse(
      response,
      maximumGitHubJsonBytes,
      `public GitHub ${channel} release metadata`,
    );
    return parseGitHubRelease(
      parseJsonBytes(bytes, `public GitHub ${channel} release metadata`),
      channel === "nightly" ? "nightly" : undefined,
    );
  }

  async getWorkflowPublisherState(file) {
    if (!new Set(["release.yml", "nightly.yml"]).has(file)) {
      throw new MirrorValidationError("workflow publisher is outside the compatibility scope");
    }
    const response = await this.client.request({
      method: "GET",
      url: apiUrl(`/repos/${repository}/actions/workflows/${file}`),
      timeoutMs: githubMetadataTimeoutMs,
    });
    assertGitHubStatus(response, 200, `read GitHub workflow ${file}`);
    const value = responseJson(response, `GitHub workflow ${file}`);
    if (!isRecord(value) || !Number.isSafeInteger(value.id) || value.id <= 0 ||
      value.path !== `.github/workflows/${file}` || typeof value.state !== "string") {
      throw new MirrorValidationError(`GitHub workflow ${file} response is invalid`);
    }
    const active = [];
    for (const status of ["requested", "waiting", "pending", "queued", "in_progress"]) {
      const url = new URL(
        `/repos/${repository}/actions/workflows/${file}/runs`,
        `${githubApiOrigin}/`,
      );
      url.searchParams.set("status", status);
      url.searchParams.set("per_page", "1");
      const runsResponse = await this.client.request({
        method: "GET",
        url: url.href,
        timeoutMs: githubMetadataTimeoutMs,
      });
      assertGitHubStatus(runsResponse, 200, `read ${status} runs for ${file}`);
      const runs = responseJson(runsResponse, `${status} runs for ${file}`);
      if (!isRecord(runs) || !Number.isSafeInteger(runs.total_count) || runs.total_count < 0 ||
        !Array.isArray(runs.workflow_runs)) {
        throw new MirrorValidationError(`GitHub workflow runs response is invalid for ${file}`);
      }
      if (runs.total_count > 0) active.push({ status, count: runs.total_count });
    }
    return deepFreeze({ id: value.id, path: value.path, state: value.state, active });
  }

  async assertRollingReleasePolicyMutable() {
    const permissionResponse = await this.client.request({
      method: "GET",
      url: apiUrl(`/repos/${repository}/actions/permissions`),
      timeoutMs: githubMetadataTimeoutMs,
    });
    assertGitHubStatus(
      permissionResponse,
      200,
      "prove GitHub Administration(read) capability",
    );
    const permissions = responseJson(
      permissionResponse,
      "GitHub Actions repository permissions",
    );
    if (!isRecord(permissions) || typeof permissions.enabled !== "boolean" ||
      !new Set(["all", "local_only", "selected"]).has(permissions.allowed_actions)) {
      throw new MirrorValidationError("GitHub Administration(read) capability proof is invalid");
    }
    const response = await this.client.request({
      method: "GET",
      url: apiUrl(`/repos/${repository}/immutable-releases`),
      timeoutMs: githubMetadataTimeoutMs,
    });
    if (response.status === 404) return;
    if (response.status === 200) {
      const value = responseJson(response, "GitHub immutable releases policy");
      if (!exactRecord(value, ["enabled"]) || value.enabled !== true) {
        throw new MirrorValidationError("GitHub immutable releases policy response is invalid");
      }
      throw new MirrorConflictError(
        "GitHub immutable releases policy must be disabled for the rolling nightly bridge",
      );
    }
    assertGitHubStatus(response, 404, "read GitHub immutable releases policy");
  }

  async #enumerateReleaseValues() {
    const releases = [];
    for (let page = 1; page <= maximumReleasePages; page += 1) {
      const url = new URL(`/repos/${repository}/releases`, `${githubApiOrigin}/`);
      url.searchParams.set("per_page", "30");
      url.searchParams.set("page", String(page));
      const response = await this.client.request({
        method: "GET",
        url: url.href,
        timeoutMs: githubMetadataTimeoutMs,
      });
      assertGitHubStatus(response, 200, `enumerate GitHub releases page ${page}`);
      const values = responseJson(response, `GitHub releases page ${page}`);
      if (!Array.isArray(values) || values.length > 30) {
        throw new MirrorValidationError(`GitHub releases page ${page} is invalid`);
      }
      for (const value of values) {
        if (!isRecord(value) || typeof value.tag_name !== "string" ||
          value.tag_name.length === 0 || value.tag_name.length > 200) {
          throw new MirrorValidationError(`GitHub releases page ${page} contains invalid metadata`);
        }
        releases.push(value);
      }
      if (values.length < 30) return releases;
    }
    throw new MirrorConflictError("GitHub release enumeration exceeded its bounded page limit");
  }

  async #hydrateRelease(metadata, expectedTag) {
    const assets = [];
    const pages = Math.ceil(maximumReleaseAssets / 100);
    for (let page = 1; page <= pages; page += 1) {
      const url = new URL(
        `/repos/${repository}/releases/${metadata.id}/assets`,
        `${githubApiOrigin}/`,
      );
      url.searchParams.set("per_page", "100");
      url.searchParams.set("page", String(page));
      const response = await this.client.request({
        method: "GET",
        url: url.href,
        timeoutMs: githubMetadataTimeoutMs,
      });
      assertGitHubStatus(response, 200, `enumerate assets for GitHub release ${metadata.id}`);
      const values = responseJson(response, `GitHub release ${metadata.id} assets page ${page}`);
      if (!Array.isArray(values) || values.length > 100) {
        throw new MirrorValidationError(`GitHub release ${metadata.id} asset page is invalid`);
      }
      assets.push(...values.map((value) => parseGitHubAsset(value, expectedTag)));
      if (values.length < 100) break;
      if (page === pages) {
        throw new MirrorConflictError(
          `GitHub release ${metadata.id} has ${maximumReleaseAssets} or more assets`,
        );
      }
    }
    const names = new Set();
    if (assets.some((asset) => names.has(asset.name) || !names.add(asset.name))) {
      throw new MirrorConflictError(`GitHub release ${metadata.id} has duplicate asset names`);
    }
    return deepFreeze({ ...metadata, assets });
  }

  #authorizeIntendedRelease(release) {
    if (this.intendedTag == null || release.tag !== this.intendedTag) return;
    this.mutableReleaseIds.set(release.id, release.tag);
    for (const asset of release.assets) this.mutableAssetIds.set(asset.id, release.id);
  }

  #assertIntendedTag(tag) {
    if (this.intendedTag == null || tag !== this.intendedTag) {
      throw new MirrorConflictError("GitHub mutation escaped the intended compatibility tag");
    }
  }

  #assertAuthorizedRelease(id, tag) {
    this.#assertIntendedTag(tag);
    if (this.mutableReleaseIds.get(id) !== tag) {
      throw new MirrorConflictError("GitHub release was not classified for compatibility mutation");
    }
  }

  async #authorizedMutation(operation) {
    await this.#proveMutationAuthority("before");
    let result;
    let mutationError;
    try {
      result = await operation();
    } catch (cause) {
      mutationError = cause;
    }
    await this.#proveMutationAuthority("after");
    if (mutationError) throw mutationError;
    return result;
  }

  async #proveMutationAuthority(phase) {
    try {
      await this.assertAuthority();
    } catch (cause) {
      throw new MirrorConflictError(
        `GitHub mutation authority could not be proven ${phase} the request`,
        { cause },
      );
    }
  }
}

export async function mirrorCompatibilityRelease({
  source,
  github,
  refreshSource = async () => source,
  fetchImmutableSource = ({ channel, id }) => fetchImmutableCompatibilityRelease({
    origin: source.origin,
    channel,
    id,
  }),
} = {}) {
  validateSourceObject(source);
  if (!github || typeof github.getReleaseByTag !== "function") {
    throw new TypeError("mirrorCompatibilityRelease requires a GitHub repository boundary");
  }
  if (typeof refreshSource !== "function" || typeof fetchImmutableSource !== "function") {
    throw new TypeError("mirror source refresh boundaries must be functions");
  }
  await assertLegacyPublisherDisabled(github, source.identity.channel);
  if (source.identity.channel === "nightly") {
    if (typeof github.assertRollingReleasePolicyMutable !== "function") {
      throw new TypeError("GitHub boundary cannot prove rolling releases remain mutable");
    }
    await github.assertRollingReleasePolicyMutable();
  }
  const plan = planCompatibilityMirror(source);
  const result = source.identity.channel === "stable"
    ? await reconcileStableCompatibility({ source, github, refreshSource })
    : await reconcileNightlyCompatibility({
        source,
        github,
        refreshSource,
        fetchImmutableSource,
      });
  return deepFreeze({ plan, ...result });
}

export async function assertLegacyPublisherDisabled(github, channel) {
  if (!github || typeof github.getWorkflowPublisherState !== "function") {
    throw new TypeError("GitHub boundary cannot prove the legacy publisher is disabled");
  }
  const file = channel === "stable" ? "release.yml" : "nightly.yml";
  const state = await github.getWorkflowPublisherState(file);
  if (state.state !== "disabled_manually" || state.active.length !== 0) {
    throw new MirrorConflictError(
      `legacy GitHub publisher ${file} must be disabled_manually with no active runs`,
    );
  }
  return state;
}

async function reconcileStableCompatibility({ source, github, refreshSource }) {
  const tag = source.identity.id;
  if (typeof github.getPublishedStableReleases !== "function") {
    throw new TypeError("GitHub boundary cannot enumerate every published stable release");
  }
  const tagAuthority = captureStableTagAuthority(
    await github.getRef(tag),
    tag,
    source.manifest.commit,
  );
  const assertTagAuthority = async () => assertStableTagAuthorityUnchanged(
    await github.getRef(tag),
    tagAuthority,
  );
  const finalBody = compatibilityBody(source, {
    phase: "published",
    previousCommit: null,
    previousAssets: [],
  });
  const stagingBody = compatibilityBody(source, {
    phase: "staging",
    previousCommit: null,
    previousAssets: [],
  });
  let release = await github.getReleaseByTag(tag);
  let owned = false;
  if (!release) {
    release = await createOwnedDraft({
      github,
      tag,
      commit: source.manifest.commit,
      name: `Nanocodex ${tag}`,
      body: stagingBody,
      prerelease: false,
      assertInvariant: assertTagAuthority,
    });
    owned = true;
  } else if (release.draft) {
    assertOwnedStagingRelease(release, {
      tag,
      name: `Nanocodex ${tag}`,
      body: stagingBody,
      prerelease: false,
    });
    owned = true;
  } else {
    if (release.prerelease || release.name !== `Nanocodex ${tag}`) {
      throw new MirrorConflictError(`stable GitHub release ${tag} has conflicting control metadata`);
    }
    await verifyExactPublishedAssets(release, source, github);
  }

  if (owned) {
    release = await ensureOwnedStagingAssets({
      github,
      release,
      source,
      staging: {
        tag,
        name: `Nanocodex ${tag}`,
        body: stagingBody,
        prerelease: false,
      },
      previousAssets: [],
    });
    await assertSourceStillCurrent(source, await refreshSource());
    await assertTagAuthority();
    const latestBefore = await github.getLatestRelease();
    assertStableLatestDoesNotRollback(latestBefore, release, tag);
    assertTargetIsHighestPublishedStable(
      await github.getPublishedStableReleases(),
      release,
      tag,
    );
    const before = release;
    release = await observedMutation({
      github,
      description: `publish stable compatibility release ${tag}`,
      assertInvariant: assertTagAuthority,
      mutate: async () => {
        const currentLatest = await github.getLatestRelease();
        if (currentLatest?.id === release.id) return currentLatest;
        if (currentLatest?.id !== latestBefore?.id) {
          throw new MirrorConflictError("GitHub latest release changed during stable publication");
        }
        assertStableLatestDoesNotRollback(currentLatest, release, tag);
        assertTargetIsHighestPublishedStable(
          await github.getPublishedStableReleases(),
          release,
          tag,
        );
        await assertTagAuthority();
        return github.updateRelease(release.id, tag, {
          tag_name: tag,
          name: `Nanocodex ${tag}`,
          body: finalBody,
          draft: false,
          prerelease: false,
          make_latest: "true",
        });
      },
      observe: async () => {
        const observedRelease = await github.getRelease(release.id, tag);
        const latest = await github.getLatestRelease();
        const publishedStable = await github.getPublishedStableReleases();
        return { release: observedRelease, latest, publishedStable };
      },
      classify: (observed) => {
        assertTargetIsHighestPublishedStable(observed.publishedStable, release, tag);
        if (releaseControlMatches(observed.release, {
          id: release.id,
          tag,
          name: `Nanocodex ${tag}`,
          body: finalBody,
          draft: false,
          prerelease: false,
        }) && observed.latest?.id === release.id) return done(observed.release);
        if (sameReleaseControl(observed.release, before) &&
          observed.latest?.id === latestBefore?.id) return pending();
        if (releaseControlMatches(observed.release, {
            id: release.id,
            tag,
            name: `Nanocodex ${tag}`,
            body: finalBody,
            draft: false,
            prerelease: false,
          }) && observed.latest?.id === latestBefore?.id) return pending();
        return conflict(`GitHub release ${tag} changed while it was being published`);
      },
    });
  } else {
    const latestBefore = await github.getLatestRelease();
    if (latestBefore?.id !== release.id) {
      assertStableLatestDoesNotRollback(latestBefore, release, tag);
      if (release.immutable) {
        throw new MirrorConflictError(
          `immutable stable GitHub release ${tag} is correct but cannot be made latest`,
        );
      }
      const before = release;
      release = await observedMutation({
        github,
        description: `mark stable compatibility release ${tag} latest`,
        assertInvariant: assertTagAuthority,
        mutate: async () => {
          const currentLatest = await github.getLatestRelease();
          if (currentLatest?.id === release.id) return currentLatest;
          if (currentLatest?.id !== latestBefore?.id) {
            throw new MirrorConflictError("GitHub latest release changed during stable publication");
          }
          assertStableLatestDoesNotRollback(currentLatest, release, tag);
          assertTargetIsHighestPublishedStable(
            await github.getPublishedStableReleases(),
            release,
            tag,
          );
          await assertTagAuthority();
          return github.updateRelease(release.id, tag, { make_latest: "true" });
        },
        observe: async () => {
          const observedRelease = await github.getRelease(release.id, tag);
          const latest = await github.getLatestRelease();
          const publishedStable = await github.getPublishedStableReleases();
          return { release: observedRelease, latest, publishedStable };
        },
        classify: (observed) => {
          assertTargetIsHighestPublishedStable(observed.publishedStable, release, tag);
          if (sameReleaseControl(observed.release, before) && observed.latest?.id === release.id) {
            return done(observed.release);
          }
          if (sameReleaseControl(observed.release, before) && observed.latest?.id === latestBefore?.id) {
            return pending();
          }
          return conflict(`published stable GitHub release ${tag} changed unexpectedly`);
        },
      });
    }
  }
  await assertSourceStillCurrent(source, await refreshSource());
  await assertTagAuthority();
  await verifyFinalGitHubRelease({
    source,
    github,
    release,
    finalBody,
    allowLegacyBody: !owned,
    stableTagAuthority: tagAuthority,
  });
  await assertTagAuthority();
  assertTargetIsHighestPublishedStable(
    await github.getPublishedStableReleases(),
    release,
    tag,
  );
  const latest = await github.getLatestRelease();
  await assertTagAuthority();
  if (!latest || latest.id !== release.id || latest.tag !== tag) {
    throw new MirrorConflictError(`GitHub latest release does not resolve to ${tag}`);
  }
  return { status: owned ? "published" : "verified", releaseId: release.id, tag };
}

async function reconcileNightlyCompatibility({
  source,
  github,
  refreshSource,
  fetchImmutableSource,
}) {
  const tag = "nightly";
  let ref = await github.getRef(tag);
  let release = await github.getReleaseByTag(tag);
  let stagingMarker;
  let staged = false;

  if (!release) {
    throw new MirrorConflictError(
      "GitHub rolling nightly release is missing; GitHub cannot stage assets before creating or moving refs/tags/nightly, so bootstrap requires explicit operator repair",
    );
  } else if (release.draft) {
    const marker = parseCompatibilityBody(release.body);
    assertOwnedStagingRelease(release, {
      tag,
      name: "Nanocodex Nightly",
      body: release.body,
      prerelease: true,
    });
    if (marker.immutableId !== source.identity.id) {
      ({ release, marker: stagingMarker } = await retargetStaleNightlyDraft({
        source,
        github,
        release,
        ref,
        marker,
        fetchImmutableSource,
      }));
    } else {
      validateCompatibilityMarker(marker, source, "staging");
      stagingMarker = {
        phase: "staging",
        previousCommit: marker.previousCommit,
        previousAssets: marker.previousAssets,
      };
    }
    staged = true;
  } else {
    if (!release.prerelease || release.name !== "Nanocodex Nightly" || release.immutable) {
      throw new MirrorConflictError("existing GitHub nightly release is not a mutable rolling nightly");
    }
    if (!ref) throw new MirrorConflictError("GitHub nightly release has no nightly tag");
    const previousAssets = await verifyReplaceablePublishedNightly(
      release,
      ref.commit,
      source,
      github,
      fetchImmutableSource,
    );
    let shouldStage = ref.commit !== source.manifest.commit;
    if (!shouldStage) {
      try {
        await verifyExactPublishedAssets(release, source, github);
      } catch (cause) {
        if (!(cause instanceof MirrorConflictError)) throw cause;
        shouldStage = true;
      }
    }
    if (shouldStage) {
      stagingMarker = {
        phase: "staging",
        previousCommit: ref.commit,
        previousAssets,
      };
      const stagingBody = compatibilityBody(source, stagingMarker);
      const before = release;
      release = await observedMutation({
        github,
        description: "take ownership of rolling GitHub nightly release",
        mutate: () => github.updateRelease(release.id, tag, {
          tag_name: tag,
          name: "Nanocodex Nightly",
          body: stagingBody,
          draft: true,
          prerelease: true,
          make_latest: "false",
        }),
        observe: () => github.getRelease(release.id, tag),
        classify: (observed) => {
          if (releaseControlMatches(observed, {
            id: release.id,
            tag,
            name: "Nanocodex Nightly",
            body: stagingBody,
            draft: true,
            prerelease: true,
          })) return done(observed);
          if (sameReleaseControl(observed, before)) return pending();
          return conflict("GitHub nightly release changed while entering staging");
        },
      });
      staged = true;
    }
  }

  const finalBody = compatibilityBody(source, {
    phase: "published",
    previousCommit: null,
    previousAssets: [],
  });
  if (staged) {
    const stagingBody = compatibilityBody(source, stagingMarker);
    release = await ensureOwnedStagingAssets({
      github,
      release,
      source,
      staging: {
        tag,
        name: "Nanocodex Nightly",
        body: stagingBody,
        prerelease: true,
      },
      previousAssets: stagingMarker.previousAssets,
    });
    await assertSourceStillCurrent(source, await refreshSource());
    await ensureGitHubRef({
      github,
      tag,
      desiredCommit: source.manifest.commit,
      previousCommit: stagingMarker.previousCommit,
    });
    const before = release;
    release = await observedMutation({
      github,
      description: "publish rolling GitHub nightly compatibility release",
      mutate: () => github.updateRelease(release.id, tag, {
        tag_name: tag,
        name: "Nanocodex Nightly",
        body: finalBody,
        draft: false,
        prerelease: true,
        make_latest: "false",
      }),
      observe: () => github.getRelease(release.id, tag),
      classify: (observed) => {
        if (releaseControlMatches(observed, {
          id: release.id,
          tag,
          name: "Nanocodex Nightly",
          body: finalBody,
          draft: false,
          prerelease: true,
        })) return done(observed);
        if (sameReleaseControl(observed, before)) return pending();
        return conflict("GitHub nightly release changed while leaving staging");
      },
    });
  }
  await verifyFinalGitHubRelease({
    source,
    github,
    release,
    finalBody,
    allowLegacyBody: !staged,
  });
  return { status: staged ? "published" : "verified", releaseId: release.id, tag };
}

async function retargetStaleNightlyDraft({
  source,
  github,
  release,
  ref,
  marker,
  fetchImmutableSource,
}) {
  validateStaleNightlyMarkerEnvelope(marker, source.origin);
  const previousSource = await fetchImmutableSource({
    channel: "nightly",
    id: marker.immutableId,
  });
  validateImmutableSourceObject(previousSource);
  if (previousSource.origin !== source.origin) {
    throw new MirrorConflictError("stale GitHub nightly marker points at another release origin");
  }
  validateCompatibilityMarker(marker, previousSource, "staging");
  if (!ref || !new Set([marker.previousCommit, marker.commit]).has(ref.commit)) {
    throw new MirrorConflictError(
      "GitHub nightly tag changed outside the interrupted staging transition",
    );
  }

  const previousByName = new Map(marker.previousAssets.map((asset) => [asset.name, asset]));
  const previousSourceByName = new Map(
    previousSource.assets.map((asset) => [asset.name, asset]),
  );
  assertNoUnknownGitHubAssets(
    release.assets,
    new Set([...previousByName.keys(), ...previousSourceByName.keys()]),
  );
  const authorizedAssets = [];
  for (const asset of release.assets) {
    if (asset.state === "starter" && asset.size === 0 && asset.digest == null &&
      previousSourceByName.has(asset.name)) {
      continue;
    }
    const prior = previousByName.get(asset.name);
    const interruptedTarget = previousSourceByName.get(asset.name);
    const priorMetadataMatches = asset.state === "uploaded" && prior != null &&
      asset.id === prior.id && asset.size === prior.size &&
      asset.digest === prior.sha256;
    const targetMetadataMatches = asset.state === "uploaded" && interruptedTarget != null &&
      asset.size === interruptedTarget.size && asset.contentType === interruptedTarget.contentType &&
      asset.digest === interruptedTarget.sha256;
    let authorizedSha256;
    if (targetMetadataMatches) {
      authorizedSha256 = interruptedTarget.sha256;
    } else if (priorMetadataMatches) {
      authorizedSha256 = prior.sha256;
    } else {
      throw new MirrorConflictError(
        `interrupted GitHub nightly draft contains unauthorized ${asset.name}`,
      );
    }
    authorizedAssets.push({
      id: asset.id,
      name: asset.name,
      size: asset.size,
      sha256: authorizedSha256,
    });
  }
  authorizedAssets.sort((left, right) => left.name.localeCompare(right.name));
  const nextMarker = {
    phase: "staging",
    previousCommit: ref.commit,
    previousAssets: authorizedAssets,
  };
  const nextBody = compatibilityBody(source, nextMarker);
  const before = release;
  const retargeted = await observedMutation({
    github,
    description: "retarget interrupted rolling GitHub nightly draft",
    mutate: () => github.updateRelease(release.id, "nightly", {
      tag_name: "nightly",
      name: "Nanocodex Nightly",
      body: nextBody,
      draft: true,
      prerelease: true,
      make_latest: "false",
    }),
    observe: () => github.getRelease(release.id, "nightly"),
    classify: (observed) => {
      if (releaseControlMatches(observed, {
        id: release.id,
        tag: "nightly",
        name: "Nanocodex Nightly",
        body: nextBody,
        draft: true,
        prerelease: true,
      })) return done(observed);
      if (sameReleaseControl(observed, before)) return pending();
      return conflict("interrupted GitHub nightly draft changed while being retargeted");
    },
  });
  return { release: retargeted, marker: nextMarker };
}

function validateStaleNightlyMarkerEnvelope(marker, origin) {
  if (!exactRecord(marker, [
    "version",
    "purpose",
    "phase",
    "channel",
    "immutableId",
    "commit",
    "manifestSha256",
    "manifestUrl",
    "previousCommit",
    "previousAssets",
  ]) || marker.version !== 1 || marker.purpose !== compatibilityPurpose ||
    marker.phase !== "staging" || marker.channel !== "nightly" ||
    marker.commit !== marker.immutableId || !SHA1.test(marker.commit) ||
    typeof marker.manifestSha256 !== "string" || !SHA256.test(marker.manifestSha256) ||
    marker.previousCommit == null || !SHA1.test(marker.previousCommit)
  ) throw new MirrorConflictError("stale GitHub nightly marker is invalid");
  const identity = parseImmutableIdentity("nightly", marker.immutableId);
  const expectedUrl = new URL(immutableManifestPath(identity), `${parsePublicOrigin(origin)}/`).href;
  if (marker.manifestUrl !== expectedUrl) {
    throw new MirrorConflictError("stale GitHub nightly marker has a noncanonical manifest URL");
  }
  marker.previousAssets = normalizePreviousAssets(marker.previousAssets);
}

async function createOwnedDraft({
  github,
  tag,
  commit,
  name,
  body,
  prerelease,
  assertInvariant = async () => undefined,
}) {
  return observedMutation({
    github,
    description: `create GitHub compatibility release ${tag}`,
    mutationConflictStatuses: new Set([422]),
    assertInvariant,
    mutate: () => github.createRelease({
      tag_name: tag,
      target_commitish: commit,
      name,
      body,
      draft: true,
      prerelease,
      make_latest: "false",
    }),
    observe: () => github.getReleaseByTag(tag),
    classify: (observed) => {
      if (releaseControlMatches(observed, {
        tag,
        name,
        body,
        draft: true,
        prerelease,
      })) return done(observed);
      if (observed == null) return pending();
      return conflict(`GitHub tag ${tag} already has a conflicting release`);
    },
  });
}

async function ensureOwnedStagingAssets({
  github,
  release,
  source,
  staging,
  previousAssets,
}) {
  const previousByName = new Map(previousAssets.map((asset) => [asset.name, asset]));
  const attemptedUploads = new Set();
  for (let cycle = 0; cycle < (previousAssets.length + compatibilityAssetNames.length) *
    (mutationAttempts + 2); cycle += 1) {
    const observed = await github.getRelease(release.id, staging.tag);
    assertOwnedStagingRelease(observed, { id: release.id, ...staging });
    assertNoUnknownGitHubAssets(
      observed.assets,
      new Set([...compatibilityAssetNames, ...previousByName.keys()]),
    );
    const obsolete = observed.assets.find((asset) =>
      !compatibilityAssetSpecifications.has(asset.name)
    );
    if (obsolete) {
      const previous = previousByName.get(obsolete.name);
      if (!previous || obsolete.id !== previous.id || obsolete.state !== "uploaded" ||
        obsolete.size !== previous.size ||
        (obsolete.digest != null && obsolete.digest !== previous.sha256)) {
        throw new MirrorConflictError(
          `GitHub release ${staging.tag} contains unauthorized prior asset ${obsolete.name}`,
        );
      }
      await deleteObservedAsset({ github, releaseId: release.id, staging, asset: obsolete });
      continue;
    }
    const target = source.assets.find((asset) => {
      const current = observed.assets.find((candidate) => candidate.name === asset.name);
      return !githubAssetMatchesSource(current, asset);
    });
    if (!target) return observed;
    const current = observed.assets.find((asset) => asset.name === target.name);
    if (current) {
      const previous = previousByName.get(current.name);
      const deletableStarter = current.state === "starter" && current.size === 0 && current.digest == null;
      if (!deletableStarter && !(current.state === "uploaded" && previous &&
        current.id === previous.id && current.size === previous.size &&
        (current.digest == null || current.digest === previous.sha256))) {
        throw new MirrorConflictError(
          `GitHub release ${staging.tag} contains conflicting bytes for ${current.name}`,
        );
      }
      await deleteObservedAsset({ github, releaseId: release.id, staging, asset: current });
      continue;
    }

    if (attemptedUploads.has(target.name)) {
      throw new MirrorConflictError(
        `GitHub asset ${target.name} disappeared after this invocation observed its upload; start a fresh invocation`,
      );
    }
    attemptedUploads.add(target.name);
    await observedMutation({
      github,
      description: `upload GitHub asset ${target.name}`,
      mutationConflictStatuses: new Set([422, 502]),
      mutate: () => github.uploadAsset(release.id, target),
      observe: () => github.getRelease(release.id, staging.tag),
      classify: (after) => {
        try {
          assertOwnedStagingRelease(after, { id: release.id, ...staging });
          assertNoUnknownGitHubAssets(
            after.assets,
            new Set([...compatibilityAssetNames, ...previousByName.keys()]),
          );
        } catch (cause) {
          return conflict(cause.message);
        }
        const uploaded = after.assets.find((asset) => asset.name === target.name);
        if (githubAssetMatchesSource(uploaded, target)) return done(after);
        if (uploaded == null ||
          (uploaded.state === "starter" && uploaded.size === 0 && uploaded.digest == null)) {
          return pending();
        }
        return conflict(`GitHub upload produced conflicting bytes for ${target.name}`);
      },
    });
  }
  throw new GitHubRequestError(`reconcile assets for ${staging.tag}`, { retryable: true });
}

async function deleteObservedAsset({ github, releaseId, staging, asset }) {
  return observedMutation({
    github,
    description: `delete recoverable GitHub asset ${asset.name}`,
    mutate: () => github.deleteAsset(releaseId, asset.id),
    observe: () => github.getRelease(releaseId, staging.tag),
    classify: (observed) => {
      try {
        assertOwnedStagingRelease(observed, { id: releaseId, ...staging });
      } catch (cause) {
        return conflict(cause.message);
      }
      const current = observed.assets.find((candidate) => candidate.name === asset.name);
      if (!current) return done(observed);
      if (sameGitHubAsset(current, asset)) return pending();
      return conflict(`GitHub asset ${asset.name} changed while it was being deleted`);
    },
  });
}

async function ensureGitHubRef({ github, tag, desiredCommit, previousCommit }) {
  assertSha1(desiredCommit, "desired GitHub tag commit");
  if (previousCommit != null) assertSha1(previousCommit, "previous GitHub tag commit");
  const initial = await github.getRef(tag);
  if (initial?.commit === desiredCommit) return initial;
  if ((initial?.commit ?? null) !== previousCommit) {
    throw new MirrorConflictError(
      `GitHub tag ${tag} changed from ${previousCommit ?? "missing"} to ${initial?.commit ?? "missing"}`,
    );
  }
  return observedMutation({
    github,
    description: `${initial ? "update" : "create"} GitHub tag ${tag}`,
    mutationConflictStatuses: new Set([409, 422]),
    mutate: () => initial
      ? github.updateRef(tag, desiredCommit)
      : github.createRef(tag, desiredCommit),
    observe: () => github.getRef(tag),
    classify: (observed) => {
      if (observed?.commit === desiredCommit) return done(observed);
      if ((observed?.commit ?? null) === previousCommit) return pending();
      return conflict(`GitHub tag ${tag} changed concurrently`);
    },
  });
}

async function observedMutation({
  github,
  description,
  mutate,
  observe,
  classify,
  mutationConflictStatuses = new Set(),
  assertInvariant = async () => undefined,
}) {
  await assertInvariant();
  let mutationError;
  try {
    await mutate();
  } catch (cause) {
    mutationError = cause;
  }
  await assertInvariant();
  if (mutationError && !ambiguousMutationError(mutationError, mutationConflictStatuses)) {
    throw mutationError;
  }

  let lastError = mutationError;
  for (let observation = 0; observation < mutationAttempts; observation += 1) {
    try {
      const observed = await observe();
      await assertInvariant();
      const verdict = classify(observed);
      if (verdict.status === "done") return verdict.value;
      if (verdict.status === "conflict") throw new MirrorConflictError(verdict.message);
    } catch (cause) {
      if (!ambiguousMutationError(cause)) throw cause;
      lastError = cause;
    }
    if (observation + 1 < mutationAttempts) {
      await github.sleep(retryDelay(observation));
    }
  }
  throw new GitHubRequestError(description, {
    retryable: true,
    ...(lastError == null ? {} : { cause: lastError }),
    detail: "mutation outcome remained unresolved after bounded observation; start a fresh invocation",
  });
}

async function verifyFinalGitHubRelease({
  source,
  github,
  release,
  finalBody,
  allowLegacyBody,
  stableTagAuthority = null,
}) {
  const tag = source.identity.channel === "nightly" ? "nightly" : source.identity.id;
  const expectedName = source.identity.channel === "nightly"
    ? "Nanocodex Nightly"
    : `Nanocodex ${tag}`;
  const observed = await github.getReleaseByTag(tag);
  if (!observed || observed.id !== release.id || observed.tag !== tag || observed.draft ||
    observed.name !== expectedName ||
    observed.prerelease !== (source.identity.channel === "nightly") ||
    (!allowLegacyBody && observed.body !== finalBody)) {
    throw new MirrorConflictError(`published GitHub release ${tag} has conflicting metadata`);
  }
  const ref = await github.getRef(tag);
  if (source.identity.channel === "stable") {
    assertStableTagAuthorityUnchanged(ref, stableTagAuthority);
  } else if (!ref || ref.commit !== source.manifest.commit) {
    throw new MirrorConflictError(
      `published GitHub tag ${tag} resolves to ${ref?.commit ?? "missing"}, expected ${source.manifest.commit}`,
    );
  }
  await verifyExactPublishedAssets(observed, source, github);

  let publicRelease;
  for (let attempt = 0; attempt < publicMetadataAttempts; attempt += 1) {
    const candidate = await github.getPublicUpdaterRelease(source.identity.channel);
    if (candidate?.id === observed.id && candidate.tag === tag) {
      publicRelease = candidate;
      break;
    }
    if (attempt + 1 < publicMetadataAttempts) await github.sleep(retryDelay(attempt));
  }
  if (!publicRelease) {
    throw new MirrorConflictError(
      `the public GitHub updater endpoint did not resolve to release ${observed.id} (${tag})`,
    );
  }
  if (publicRelease.draft ||
    publicRelease.prerelease !== (source.identity.channel === "nightly") ||
    publicRelease.assets.length !== compatibilityAssetNames.length) {
    throw new MirrorConflictError(`public GitHub release ${tag} has conflicting metadata`);
  }
  for (const expected of source.assets) {
    const authenticated = observed.assets.find((asset) => asset.name === expected.name);
    const publicAsset = publicRelease.assets.find((asset) => asset.name === expected.name);
    if (!authenticated || !publicAsset || publicAsset.id !== authenticated.id ||
      !sameGitHubAsset(publicAsset, authenticated)) {
      throw new MirrorConflictError(
        `public GitHub release ${tag} identifies conflicting bytes for ${expected.name}`,
      );
    }
    await github.verifyPublicAsset(publicAsset, expected);
  }
  const finalRef = await github.getRef(tag);
  if (source.identity.channel === "stable") {
    assertStableTagAuthorityUnchanged(finalRef, stableTagAuthority);
  } else if (!finalRef || finalRef.commit !== source.manifest.commit) {
    throw new MirrorConflictError(
      `published GitHub tag ${tag} changed during final public-byte verification`,
    );
  }
}

export function assertStableLatestDoesNotRollback(latest, target, targetTag) {
  parseImmutableIdentity("stable", targetTag);
  if (latest == null || latest.id === target.id) return;
  if (latest.draft || latest.prerelease) {
    throw new MirrorConflictError("GitHub latest release metadata is not a stable release");
  }
  const current = stableVersionTuple(latest.tag);
  const desired = stableVersionTuple(targetTag);
  if (compareVersionTuple(current, desired) >= 0) {
    throw new MirrorConflictError(
      `refusing to move GitHub latest from ${latest.tag} back to ${targetTag}`,
    );
  }
}

export function assertTargetIsHighestPublishedStable(releases, target, targetTag) {
  parseImmutableIdentity("stable", targetTag);
  validatePositiveId(target?.id, "target stable GitHub release id");
  if (target.tag !== targetTag) {
    throw new MirrorValidationError("target stable GitHub release has the wrong tag");
  }
  if (!Array.isArray(releases) || releases.length > maximumReleasePages * 30) {
    throw new MirrorValidationError("published stable GitHub release set is invalid");
  }
  const desired = stableVersionTuple(targetTag);
  const tags = new Set();
  for (const release of releases) {
    if (!isRecord(release) || !Number.isSafeInteger(release.id) || release.id <= 0 ||
      typeof release.tag !== "string" || !stableTagPattern.test(release.tag) ||
      release.draft !== false || release.prerelease !== false) {
      throw new MirrorValidationError("published stable GitHub release set is invalid");
    }
    if (tags.has(release.tag)) {
      throw new MirrorConflictError(
        `GitHub contains duplicate published stable releases for ${release.tag}`,
      );
    }
    tags.add(release.tag);
    if (release.tag === targetTag && release.id !== target.id) {
      throw new MirrorConflictError(
        `GitHub published stable tag ${targetTag} belongs to another release`,
      );
    }
    if (compareVersionTuple(stableVersionTuple(release.tag), desired) > 0) {
      throw new MirrorConflictError(
        `stable compatibility target ${targetTag} is below published ${release.tag}`,
      );
    }
  }
}

function stableVersionTuple(tag) {
  const identity = parseImmutableIdentity("stable", tag);
  return identity.id.slice(1).split(".").map((value) => Number(value));
}

function compareVersionTuple(left, right) {
  for (let index = 0; index < 3; index += 1) {
    if (left[index] !== right[index]) return left[index] < right[index] ? -1 : 1;
  }
  return 0;
}

async function verifyExactPublishedAssets(release, source, github) {
  assertNoUnknownGitHubAssets(release.assets);
  if (release.assets.length !== compatibilityAssetNames.length) {
    throw new MirrorConflictError(
      `GitHub release ${release.tag} does not contain the exact compatibility asset set`,
    );
  }
  for (const expected of source.assets) {
    const asset = release.assets.find((candidate) => candidate.name === expected.name);
    if (
      !asset || asset.state !== "uploaded" || asset.size !== expected.size ||
      (asset.digest != null && asset.digest !== expected.sha256)
    ) throw new MirrorConflictError(`GitHub release ${release.tag} conflicts at ${expected.name}`);
    await github.verifyPublicAsset(asset, expected);
  }
}

async function verifyReplaceablePublishedNightly(
  release,
  refCommit,
  source,
  github,
  fetchImmutableSource,
) {
  if (release.body.startsWith("Nanocodex updater compatibility mirror.\n")) {
    const marker = parseCompatibilityBody(release.body);
    validatePublishedNightlyMarkerEnvelope(marker, source.origin, refCommit);
    const previousSource = await fetchImmutableSource({
      channel: "nightly",
      id: marker.immutableId,
    });
    validateImmutableSourceObject(previousSource);
    if (previousSource.origin !== source.origin) {
      throw new MirrorConflictError(
        "published GitHub nightly marker points at another release origin",
      );
    }
    validateCompatibilityMarker(marker, previousSource, "published");
    await verifyExactPublishedAssets(release, previousSource, github);
    return previousSource.assets.map((expected) => {
      const asset = release.assets.find((candidate) => candidate.name === expected.name);
      return Object.freeze({
        id: asset.id,
        name: expected.name,
        size: expected.size,
        sha256: expected.sha256,
      });
    });
  }

  const short = refCommit.slice(0, 10);
  const expectedBody =
    `Automated nightly build from [\`${short}\`](https://github.com/${repository}/commit/${refCommit}).\n\n` +
    "Install it with `nanocodex update --nightly`.\n";
  if (release.body !== expectedBody || release.authorLogin !== "github-actions[bot]" ||
    release.assets.some((asset) => asset.uploaderLogin !== "github-actions[bot]")) {
    throw new MirrorConflictError(
      "published GitHub nightly is neither bridge-owned nor the exact legacy Actions release",
    );
  }
  return verifyPublishedNightlyAssets(release, github);
}

function validatePublishedNightlyMarkerEnvelope(marker, origin, refCommit) {
  if (!exactRecord(marker, [
    "version",
    "purpose",
    "phase",
    "channel",
    "immutableId",
    "commit",
    "manifestSha256",
    "manifestUrl",
    "previousCommit",
    "previousAssets",
  ]) || marker.version !== 1 || marker.purpose !== compatibilityPurpose ||
    marker.phase !== "published" || marker.channel !== "nightly" ||
    marker.immutableId !== refCommit || marker.commit !== refCommit ||
    !SHA1.test(marker.immutableId) ||
    typeof marker.manifestSha256 !== "string" || !SHA256.test(marker.manifestSha256) ||
    marker.previousCommit !== null || !Array.isArray(marker.previousAssets) ||
    marker.previousAssets.length !== 0) {
    throw new MirrorConflictError("published GitHub nightly ownership marker is invalid");
  }
  const identity = parseImmutableIdentity("nightly", marker.immutableId);
  const expectedUrl = new URL(immutableManifestPath(identity), `${parsePublicOrigin(origin)}/`).href;
  if (marker.manifestUrl !== expectedUrl) {
    throw new MirrorConflictError(
      "published GitHub nightly ownership marker has a noncanonical manifest URL",
    );
  }
}

async function verifyPublishedNightlyAssets(release, github) {
  const names = [...release.assets.map((asset) => asset.name)].sort();
  const inventory = legacyRollingAssetSets.find((candidate) =>
    canonicalJson([...candidate].sort()) === canonicalJson(names)
  );
  if (!inventory || release.assets.some((asset) =>
      asset.state !== "uploaded" || asset.size <= 0 ||
      asset.size > legacyRollingAssetSpecifications.get(asset.name).maximumBytes
    )) throw new MirrorConflictError("published GitHub nightly assets are incomplete or invalid");
  const checksumAsset = release.assets.find((asset) => asset.name === "SHA256SUMS");
  const checksumDownload = await downloadPublicGitHubAsset({
    githubAsset: checksumAsset,
    maximumBytes: 64 * 1024,
    expectedSize: checksumAsset.size,
    expectedSha256: checksumAsset.digest,
    fetchImpl: github.publicFetch,
    timeoutMs: publicVerificationTimeoutMs,
  });
  let checksumText;
  if (
    checksumDownload.bytes.byteLength >= 3 && checksumDownload.bytes[0] === 0xef &&
    checksumDownload.bytes[1] === 0xbb && checksumDownload.bytes[2] === 0xbf
  ) throw new MirrorConflictError("legacy GitHub SHA256SUMS contains a UTF-8 BOM");
  try {
    checksumText = new TextDecoder("utf-8", { fatal: true }).decode(checksumDownload.bytes);
  } catch (cause) {
    throw new MirrorConflictError("legacy GitHub SHA256SUMS is not UTF-8", { cause });
  }
  const checksums = new Map();
  for (const line of checksumText.split("\n")) {
    if (line === "") continue;
    const match = /^([a-fA-F0-9]{64})[ \t]+\*?([A-Za-z0-9][A-Za-z0-9._-]{0,159})$/.exec(line);
    if (!match || match[2] === "SHA256SUMS" || !inventory.includes(match[2]) ||
      checksums.has(match[2])) {
      throw new MirrorConflictError("legacy GitHub SHA256SUMS has an unexpected entry");
    }
    checksums.set(match[2], match[1].toLowerCase());
  }
  if (checksums.size !== inventory.length - 1 ||
    inventory.some((name) => name !== "SHA256SUMS" && !checksums.has(name))) {
    throw new MirrorConflictError("legacy GitHub SHA256SUMS is missing a release asset");
  }
  const descriptors = [{
    id: checksumAsset.id,
    name: checksumAsset.name,
    size: checksumDownload.bytes.byteLength,
    sha256: checksumDownload.sha256,
  }];
  for (const name of inventory.filter((value) => value !== "SHA256SUMS")) {
    const asset = release.assets.find((candidate) => candidate.name === name);
    const downloaded = await downloadPublicGitHubAsset({
      githubAsset: asset,
      maximumBytes: legacyRollingAssetSpecifications.get(name).maximumBytes,
      expectedSize: asset.size,
      expectedSha256: checksums.get(name),
      fetchImpl: github.publicFetch,
      timeoutMs: publicVerificationTimeoutMs,
    });
    if (asset.digest != null && asset.digest !== downloaded.sha256) {
      throw new MirrorConflictError(`legacy GitHub asset digest conflicts for ${name}`);
    }
    descriptors.push({
      id: asset.id,
      name,
      size: downloaded.bytes.byteLength,
      sha256: downloaded.sha256,
    });
  }
  descriptors.sort((left, right) => left.name.localeCompare(right.name));
  return descriptors.map(Object.freeze);
}

function assertOwnedStagingRelease(release, expected) {
  if (!releaseControlMatches(release, { ...expected, draft: true }) || release.immutable) {
    throw new MirrorConflictError(`GitHub release ${expected.tag} is not the owned staging release`);
  }
  return release;
}

function assertNoUnknownGitHubAssets(
  assets,
  allowedNames = new Set(compatibilityAssetNames),
) {
  const names = new Set();
  for (const asset of assets) {
    if (!allowedNames.has(asset.name) || names.has(asset.name)) {
      throw new MirrorConflictError(`GitHub release contains unexpected asset ${asset.name}`);
    }
    names.add(asset.name);
  }
}

function releaseControlMatches(release, expected) {
  return release != null && (expected.id == null || release.id === expected.id) &&
    release.tag === expected.tag && release.name === expected.name && release.body === expected.body &&
    release.draft === expected.draft && release.prerelease === expected.prerelease;
}

function sameReleaseControl(left, right) {
  return left != null && right != null && left.id === right.id && left.tag === right.tag &&
    left.name === right.name && left.body === right.body && left.draft === right.draft &&
    left.prerelease === right.prerelease && left.immutable === right.immutable;
}

function sameGitHubAsset(left, right) {
  return left.id === right.id && left.name === right.name && left.state === right.state &&
    left.size === right.size && left.digest === right.digest &&
    left.contentType === right.contentType &&
    left.browserDownloadUrl === right.browserDownloadUrl;
}

function githubAssetMatchesSource(asset, sourceAsset) {
  return asset != null && asset.name === sourceAsset.name && asset.state === "uploaded" &&
    asset.size === sourceAsset.size && asset.digest === sourceAsset.sha256 &&
    asset.contentType === sourceAsset.contentType;
}

function done(value) {
  return { status: "done", value };
}

function pending() {
  return { status: "pending" };
}

function conflict(message) {
  return { status: "conflict", message };
}

function ambiguousMutationError(error, extraStatuses = new Set()) {
  return error instanceof GitHubTransportError ||
    (error instanceof GitHubRequestError &&
      (error.retryable || extraStatuses.has(error.status)));
}

function retryDelay(attempt) {
  return Math.min(250 * (2 ** Math.min(attempt, 6)), 5_000);
}

async function assertSourceStillCurrent(expected, observed) {
  validateSourceObject(observed);
  if (
    observed.origin !== expected.origin ||
    canonicalJson(observed.identity) !== canonicalJson(expected.identity) ||
    canonicalJson(observed.manifest) !== canonicalJson(expected.manifest) ||
    canonicalJson(observed.pointer) !== canonicalJson(expected.pointer) ||
    observed.assets.length !== expected.assets.length || observed.assets.some((asset, index) =>
      asset.name !== expected.assets[index].name || asset.size !== expected.assets[index].size ||
      asset.sha256 !== expected.assets[index].sha256 ||
      !bufferView(asset.bytes).equals(bufferView(expected.assets[index].bytes))
    )
  ) throw new MirrorConflictError("canonical release channel changed during GitHub reconciliation");
}

export function compatibilityBody(source, {
  phase,
  previousCommit,
  previousAssets,
}) {
  validateSourceObject(source);
  if (!new Set(["staging", "published"]).has(phase)) {
    throw new MirrorValidationError("compatibility marker phase is invalid");
  }
  if (previousCommit != null) assertSha1(previousCommit, "compatibility marker previous commit");
  const normalizedAssets = normalizePreviousAssets(previousAssets);
  if (phase === "published" && (previousCommit != null || normalizedAssets.length !== 0)) {
    throw new MirrorValidationError("published compatibility marker cannot retain prior assets");
  }
  const marker = {
    version: 1,
    purpose: compatibilityPurpose,
    phase,
    channel: source.identity.channel,
    immutableId: source.identity.id,
    commit: source.manifest.commit,
    manifestSha256: source.manifest.manifestSha256,
    manifestUrl: source.manifestUrl,
    previousCommit,
    previousAssets: normalizedAssets,
  };
  return `Nanocodex updater compatibility mirror.\n${canonicalJson(marker)}\n`;
}

export function parseCompatibilityBody(body) {
  if (typeof body !== "string" || body.length > 64 * 1024) {
    throw new MirrorConflictError("GitHub compatibility release body is invalid");
  }
  const prefix = "Nanocodex updater compatibility mirror.\n";
  if (!body.startsWith(prefix) || !body.endsWith("\n")) {
    throw new MirrorConflictError("GitHub draft is not owned by the compatibility bridge");
  }
  const encoded = body.slice(prefix.length, -1);
  let marker;
  try {
    marker = JSON.parse(encoded);
  } catch (cause) {
    throw new MirrorConflictError("GitHub compatibility marker is invalid JSON", { cause });
  }
  if (canonicalJson(marker) !== encoded) {
    throw new MirrorConflictError("GitHub compatibility marker is not canonical");
  }
  return marker;
}

function validateCompatibilityMarker(marker, source, expectedPhase) {
  if (!exactRecord(marker, [
    "version",
    "purpose",
    "phase",
    "channel",
    "immutableId",
    "commit",
    "manifestSha256",
    "manifestUrl",
    "previousCommit",
    "previousAssets",
  ]) || marker.version !== 1 || marker.purpose !== compatibilityPurpose ||
    marker.phase !== expectedPhase || marker.channel !== source.identity.channel ||
    marker.immutableId !== source.identity.id || marker.commit !== source.manifest.commit ||
    marker.manifestSha256 !== source.manifest.manifestSha256 ||
    marker.manifestUrl !== source.manifestUrl ||
    (marker.previousCommit !== null && !SHA1.test(marker.previousCommit))
  ) throw new MirrorConflictError("GitHub compatibility marker targets another release");
  marker.previousAssets = normalizePreviousAssets(marker.previousAssets);
  return marker;
}

function normalizePreviousAssets(value) {
  if (!Array.isArray(value) || value.length > legacyRollingAssetSpecifications.size) {
    throw new MirrorValidationError("compatibility marker prior assets are invalid");
  }
  const names = new Set();
  const normalized = value.map((asset) => {
    if (!exactRecord(asset, ["id", "name", "size", "sha256"]) ||
      !legacyRollingAssetSpecifications.has(asset.name) || names.has(asset.name) ||
      !Number.isSafeInteger(asset.id) || asset.id <= 0 ||
      !Number.isSafeInteger(asset.size) || asset.size <= 0 ||
      asset.size > legacyRollingAssetSpecifications.get(asset.name).maximumBytes ||
      typeof asset.sha256 !== "string" || !SHA256.test(asset.sha256)
    ) throw new MirrorValidationError("compatibility marker prior asset is invalid");
    names.add(asset.name);
    return { id: asset.id, name: asset.name, size: asset.size, sha256: asset.sha256 };
  });
  normalized.sort((left, right) => left.name.localeCompare(right.name));
  return normalized;
}

export function parseGitHubRelease(value, expectedTag) {
  const metadata = parseGitHubReleaseMetadata(value, expectedTag);
  if (!Array.isArray(value.assets) || value.assets.length > 64) {
    throw new MirrorValidationError(`GitHub release ${metadata.tag} embedded assets are invalid`);
  }
  const names = new Set();
  const assets = value.assets.map((asset) => {
    const parsed = parseGitHubAsset(asset, metadata.tag);
    if (names.has(parsed.name)) {
      throw new MirrorConflictError(`GitHub release ${metadata.tag} has duplicate ${parsed.name}`);
    }
    names.add(parsed.name);
    return parsed;
  });
  return deepFreeze({ ...metadata, assets });
}

function parseGitHubReleaseMetadata(value, expectedTag) {
  if (!isRecord(value)) throw new MirrorValidationError("GitHub release response is not an object");
  validatePositiveId(value.id, "GitHub release id");
  if (typeof value.tag_name !== "string") {
    throw new MirrorValidationError("GitHub release tag is invalid");
  }
  validateGitHubTag(value.tag_name);
  if (expectedTag != null && value.tag_name !== expectedTag) {
    throw new MirrorConflictError(
      `GitHub returned release ${value.tag_name}, expected ${expectedTag}`,
    );
  }
  if (
    (value.name !== null && typeof value.name !== "string") ||
    (value.body !== null && typeof value.body !== "string") ||
    typeof value.draft !== "boolean" || typeof value.prerelease !== "boolean" ||
    typeof value.immutable !== "boolean" ||
    !isRecord(value.author) || typeof value.author.login !== "string" ||
    value.author.login.length === 0 || value.author.login.length > 160
  ) throw new MirrorValidationError(`GitHub release ${value.tag_name} metadata is invalid`);
  return deepFreeze({
    id: value.id,
    tag: value.tag_name,
    name: value.name ?? "",
    body: value.body ?? "",
    draft: value.draft,
    prerelease: value.prerelease,
    immutable: value.immutable,
    authorLogin: value.author.login,
  });
}

export function parseGitHubAsset(value, expectedTag) {
  if (!isRecord(value)) throw new MirrorValidationError("GitHub release asset is not an object");
  validatePositiveId(value.id, "GitHub asset id");
  if (
    typeof value.name !== "string" || !assetNamePattern.test(value.name) ||
    !new Set(["uploaded", "starter"]).has(value.state) ||
    !Number.isSafeInteger(value.size) || value.size < 0 || value.size > 256 * 1024 * 1024 ||
    typeof value.content_type !== "string" || value.content_type.length > 160 ||
    !contentTypePattern.test(value.content_type) ||
    typeof value.browser_download_url !== "string" || !isRecord(value.uploader) ||
    typeof value.uploader.login !== "string" || value.uploader.login.length === 0 ||
    value.uploader.login.length > 160
  ) throw new MirrorValidationError(`GitHub release asset is invalid: ${String(value.name)}`);
  let digest = null;
  if (value.digest != null) {
    if (typeof value.digest !== "string" || !/^sha256:[a-f0-9]{64}$/.test(value.digest)) {
      throw new MirrorValidationError(`GitHub release asset digest is invalid: ${value.name}`);
    }
    digest = value.digest.slice("sha256:".length);
  }
  const browserDownloadUrl = validateGitHubBrowserDownloadUrl(
    value.browser_download_url,
    expectedTag,
    value.name,
  );
  return Object.freeze({
    id: value.id,
    name: value.name,
    state: value.state,
    size: value.size,
    digest,
    contentType: value.content_type,
    browserDownloadUrl,
    uploaderLogin: value.uploader.login,
  });
}

export function parseGitHubReference(value, expectedTag) {
  if (!isRecord(value) || !isRecord(value.object) ||
    value.ref !== `refs/tags/${expectedTag}` ||
    !new Set(["commit", "tag", "tree", "blob"]).has(value.object.type) ||
    typeof value.object.sha !== "string" || !SHA1.test(value.object.sha)
  ) throw new MirrorValidationError(`GitHub tag ${expectedTag} response is invalid`);
  return deepFreeze({
    tag: expectedTag,
    ref: value.ref,
    object: { type: value.object.type, sha: value.object.sha },
  });
}

function parseGitHubTagObject(value, expectedSha) {
  if (!isRecord(value) || value.sha !== expectedSha || typeof value.tag !== "string" ||
    !isRecord(value.object) || !new Set(["commit", "tag", "tree", "blob"]).has(value.object.type) ||
    typeof value.object.sha !== "string" || !SHA1.test(value.object.sha)
  ) throw new MirrorValidationError("GitHub annotated tag response is invalid");
  return deepFreeze({
    sha: value.sha,
    tag: value.tag,
    object: { type: value.object.type, sha: value.object.sha },
  });
}

export function captureStableTagAuthority(ref, tag, expectedCommit) {
  parseImmutableIdentity("stable", tag);
  if (expectedCommit != null) assertSha1(expectedCommit, "expected stable tag commit");
  if (ref == null) {
    throw new MirrorConflictError(`stable compatibility tag ${tag} is missing`);
  }
  if (ref.tag !== tag || ref.ref !== `refs/tags/${tag}` ||
    !isRecord(ref.object) || ref.object.type !== "tag" || !SHA1.test(ref.object.sha) ||
    !Array.isArray(ref.tagObjects) || ref.tagObjects.length !== 1) {
    throw new MirrorConflictError(
      `stable compatibility tag ${tag} must be one direct annotated tag`,
    );
  }
  const annotated = ref.tagObjects[0];
  if (!isRecord(annotated) || annotated.sha !== ref.object.sha || annotated.tag !== tag ||
    !isRecord(annotated.object) || annotated.object.type !== "commit" ||
    !SHA1.test(annotated.object.sha) || ref.commit !== annotated.object.sha) {
    throw new MirrorConflictError(
      `stable compatibility tag ${tag} has the wrong annotated object or target`,
    );
  }
  if (expectedCommit != null && ref.commit !== expectedCommit) {
    throw new MirrorConflictError(
      `stable compatibility tag ${tag} resolves to ${ref.commit}, expected ${expectedCommit}`,
    );
  }
  return deepFreeze({
    tag,
    ref: ref.ref,
    object: { type: ref.object.type, sha: ref.object.sha },
    target: { type: annotated.object.type, sha: annotated.object.sha },
    commit: ref.commit,
  });
}

export function assertStableTagAuthorityUnchanged(ref, expected) {
  if (!isRecord(expected) || typeof expected.tag !== "string") {
    throw new TypeError("expected stable tag authority is invalid");
  }
  const observed = captureStableTagAuthority(ref, expected.tag, expected.commit);
  if (canonicalJson(observed) !== canonicalJson(expected)) {
    throw new MirrorConflictError(
      `stable compatibility tag ${expected.tag} was deleted, recreated, or moved`,
    );
  }
  return observed;
}

export async function verifyPublicGitHubAsset({
  githubAsset,
  expected,
  fetchImpl = fetch,
  timeoutMs = publicVerificationTimeoutMs,
} = {}) {
  validateCompatibilityAsset(expected);
  if (!githubAsset || githubAsset.name !== expected.name || githubAsset.state !== "uploaded" ||
    githubAsset.size !== expected.size ||
    (githubAsset.digest != null && githubAsset.digest !== expected.sha256)
  ) throw new MirrorConflictError(`GitHub asset metadata conflicts for ${expected.name}`);
  return downloadPublicGitHubAsset({
    githubAsset,
    maximumBytes: compatibilityAssetSpecifications.get(expected.name).maximumBytes,
    expectedSize: expected.size,
    expectedSha256: expected.sha256,
    fetchImpl,
    timeoutMs,
  });
}

export async function downloadPublicGitHubAsset({
  githubAsset,
  maximumBytes,
  expectedSize,
  expectedSha256,
  fetchImpl = fetch,
  timeoutMs = publicVerificationTimeoutMs,
} = {}) {
  if (!githubAsset || typeof githubAsset.browserDownloadUrl !== "string" ||
    !Number.isSafeInteger(maximumBytes) || maximumBytes <= 0 ||
    !Number.isSafeInteger(expectedSize) || expectedSize <= 0 || expectedSize > maximumBytes ||
    (expectedSha256 != null && (typeof expectedSha256 !== "string" || !SHA256.test(expectedSha256))) ||
    typeof fetchImpl !== "function"
  ) throw new MirrorValidationError("public GitHub asset verification input is invalid");
  validatePositiveId(githubAsset.id, "public GitHub asset id");
  let url = new URL(githubAsset.browserDownloadUrl);
  url.searchParams.append("asset_id", String(githubAsset.id));
  let response;
  for (let redirect = 0; redirect <= publicDownloadRedirects; redirect += 1) {
    const controller = new AbortController();
    const timeout = setTimeout(
      () => controller.abort(new Error("public GitHub download timed out")),
      timeoutMs,
    );
    timeout.unref?.();
    try {
      response = await fetchImpl(url, {
        method: "GET",
        redirect: "manual",
        signal: controller.signal,
        headers: {
          accept: "application/octet-stream",
          "accept-encoding": "identity",
          "user-agent": "nanocodex-cloudflare-github-compatibility-mirror/1",
        },
      });
    } catch (cause) {
      throw new GitHubRequestError(`download public GitHub asset ${githubAsset.name}`, {
        retryable: true,
        cause,
      });
    } finally {
      clearTimeout(timeout);
    }
    if (!(response instanceof Response)) {
      throw new MirrorValidationError("public GitHub asset fetch returned a non-Response value");
    }
    if (!isRedirectStatus(response.status)) break;
    await cancelResponse(response);
    if (redirect === publicDownloadRedirects) {
      throw new MirrorConflictError(`public GitHub asset ${githubAsset.name} redirected too many times`);
    }
    const location = response.headers.get("location");
    if (!location) throw new MirrorConflictError("public GitHub download redirect has no location");
    const next = new URL(location, url);
    if (!allowedPublicGitHubRedirect(next)) {
      throw new MirrorConflictError(
        `public GitHub asset ${githubAsset.name} redirected outside GitHub's release CDN`,
      );
    }
    url = next;
  }
  assertResponseStatus(response, 200, `public GitHub asset ${githubAsset.name}`);
  if (response.headers.has("content-encoding") || response.headers.has("content-range")) {
    await cancelResponse(response);
    throw new MirrorConflictError(`public GitHub asset ${githubAsset.name} is encoded or partial`);
  }
  const declared = strictContentLength(response.headers.get("content-length"));
  if (declared != null && declared !== expectedSize) {
    await cancelResponse(response);
    throw new MirrorConflictError(
      `public GitHub asset ${githubAsset.name} declares ${declared} bytes, expected ${expectedSize}`,
    );
  }
  const bytes = await readBoundedResponse(
    response,
    maximumBytes,
    `public GitHub asset ${githubAsset.name}`,
    expectedSize,
  );
  if (bytes.byteLength !== expectedSize) {
    throw new MirrorConflictError(
      `public GitHub asset ${githubAsset.name} has ${bytes.byteLength} bytes, expected ${expectedSize}`,
    );
  }
  const sha256 = sha256Hex(bytes);
  if (expectedSha256 != null && sha256 !== expectedSha256) {
    throw new MirrorConflictError(
      `public GitHub asset ${githubAsset.name} SHA-256 mismatch: expected ${expectedSha256}, downloaded ${sha256}`,
    );
  }
  return Object.freeze({ bytes, sha256, finalUrl: url.href });
}

function validateGitHubBrowserDownloadUrl(value, expectedTag, name) {
  let url;
  try {
    url = new URL(value);
  } catch (cause) {
    throw new MirrorValidationError(`GitHub browser download URL is invalid for ${name}`, { cause });
  }
  if (url.origin !== githubWebOrigin || url.username || url.password || url.search || url.hash) {
    throw new MirrorConflictError(`GitHub browser download URL escaped the fixed repository for ${name}`);
  }
  if (expectedTag != null) {
    validateGitHubTag(expectedTag);
    const expectedPath = `/${repository}/releases/download/${encodeURIComponent(expectedTag)}/${encodeURIComponent(name)}`;
    if (url.pathname !== expectedPath) {
      throw new MirrorConflictError(`GitHub browser download URL has the wrong release path for ${name}`);
    }
  } else if (!url.pathname.endsWith(`/${encodeURIComponent(name)}`)) {
    throw new MirrorConflictError(`GitHub browser download URL has the wrong asset name for ${name}`);
  }
  return url.href;
}

function allowedPublicGitHubRedirect(url) {
  return url.protocol === "https:" && url.username === "" && url.password === "" && url.hash === "" &&
    (url.hostname === "release-assets.githubusercontent.com" ||
      url.hostname === "objects.githubusercontent.com" ||
      url.hostname.endsWith(".githubusercontent.com"));
}

function validateCompatibilityAsset(asset) {
  if (!asset || !compatibilityAssetSpecifications.has(asset.name) ||
    asset.platform !== compatibilityAssetSpecifications.get(asset.name).platform ||
    !Number.isSafeInteger(asset.size) || asset.size <= 0 ||
    asset.size > compatibilityAssetSpecifications.get(asset.name).maximumBytes ||
    typeof asset.sha256 !== "string" || !SHA256.test(asset.sha256) ||
    typeof asset.contentType !== "string" || asset.contentType.length > 160 ||
    !contentTypePattern.test(asset.contentType) || !(asset.bytes instanceof Uint8Array) ||
    asset.bytes.byteLength !== asset.size || sha256Hex(asset.bytes) !== asset.sha256
  ) throw new MirrorValidationError(`compatibility asset is invalid: ${String(asset?.name)}`);
  return asset;
}

function validateSourceObject(source) {
  validateImmutableSourceObject(source);
  const identity = parseImmutableIdentity(source.identity?.channel, source.identity?.id);
  const manifest = validatePublicManifest(source.manifest, identity);
  validateReleaseChannel({ pointer: source.pointer, manifest }, identity, manifest);
  return source;
}

function validateImmutableSourceObject(source) {
  if (!isRecord(source)) throw new MirrorValidationError("compatibility source is invalid");
  const identity = parseImmutableIdentity(source.identity?.channel, source.identity?.id);
  const origin = parsePublicOrigin(source.origin);
  const manifest = validatePublicManifest(source.manifest, identity);
  if (!Array.isArray(source.assets) || source.assets.length !== compatibilityAssetNames.length) {
    throw new MirrorValidationError("compatibility source has the wrong selected asset set");
  }
  for (let index = 0; index < compatibilityAssetNames.length; index += 1) {
    const asset = source.assets[index];
    if (asset?.name !== compatibilityAssetNames[index]) {
      throw new MirrorValidationError("compatibility source assets are not in canonical order");
    }
    validateCompatibilityAsset(asset);
    const manifestAsset = manifest.assets.find((candidate) => candidate.name === asset.name);
    if (
      !manifestAsset || asset.platform !== manifestAsset.platform || asset.size !== manifestAsset.size ||
      asset.sha256 !== manifestAsset.sha256 || asset.contentType !== manifestAsset.contentType ||
      asset.downloadPath !== manifestAsset.downloadPath
    ) throw new MirrorValidationError(`selected compatibility asset conflicts with manifest: ${asset.name}`);
  }
  const expectedManifestUrl = new URL(immutableManifestPath(identity), `${origin}/`).href;
  if (source.manifestUrl !== expectedManifestUrl) {
    throw new MirrorValidationError("compatibility source manifest URL is not canonical");
  }
  validateChecksumManifest(
    source.assets.find((asset) => asset.name === "SHA256SUMS").bytes,
    manifest.assets,
  );
  return source;
}

function validateReleaseMutation(value, create) {
  if (!isRecord(value)) throw new MirrorValidationError("GitHub release mutation is invalid");
  if (create) {
    if (!exactRecord(value, [
      "tag_name",
      "target_commitish",
      "name",
      "body",
      "draft",
      "prerelease",
      "make_latest",
    ]) || !stableTagPattern.test(value.tag_name) || value.draft !== true ||
      value.prerelease !== false || value.make_latest !== "false") {
      throw new MirrorValidationError(
        "GitHub release creation is limited to one non-latest stable draft",
      );
    }
  } else {
    const allowed = new Set([
      "tag_name",
      "name",
      "body",
      "draft",
      "prerelease",
      "make_latest",
    ]);
    if (!Object.keys(value).every((key) => allowed.has(key)) || Object.keys(value).length === 0) {
      throw new MirrorValidationError("GitHub release mutation has unexpected fields");
    }
  }
  if (value.tag_name != null) validateGitHubTag(value.tag_name);
  if (create) assertSha1(value.target_commitish, "release target commit");
  for (const key of ["name", "body"]) {
    if (value[key] != null && (typeof value[key] !== "string" || value[key].length > 64 * 1024)) {
      throw new MirrorValidationError(`GitHub release ${key} is invalid`);
    }
  }
  for (const key of ["draft", "prerelease"]) {
    if (value[key] != null && typeof value[key] !== "boolean") {
      throw new MirrorValidationError(`GitHub release ${key} is invalid`);
    }
  }
  if (value.make_latest != null && !new Set(["true", "false"]).has(value.make_latest)) {
    throw new MirrorValidationError("GitHub make_latest value is invalid");
  }
}

function validateGitHubTag(tag) {
  if (tag !== "nightly" && !(typeof tag === "string" && stableTagPattern.test(tag))) {
    throw new MirrorValidationError("GitHub compatibility tag is outside stable/nightly scope");
  }
  return tag;
}

function apiUrl(path) {
  const url = new URL(path, `${githubApiOrigin}/`);
  if (url.origin !== githubApiOrigin || !url.pathname.startsWith(`/repos/${repository}/`)) {
    throw new MirrorValidationError("internal GitHub API path escaped the fixed repository");
  }
  return url.href;
}

function responseJson(response, operation) {
  if (!(response.body instanceof Uint8Array) || response.body.byteLength > maximumGitHubJsonBytes) {
    throw new MirrorValidationError(`${operation} body is invalid`);
  }
  return parseJsonBytes(response.body, operation);
}

function assertGitHubStatus(response, expected, operation) {
  if (response.status === expected) return;
  let detail;
  try {
    const value = responseJson(response, operation);
    if (typeof value?.message === "string") detail = value.message.slice(0, 512);
  } catch {
    detail = undefined;
  }
  throw new GitHubRequestError(operation, {
    status: response.status,
    retryable: retryableStatus(response.status),
    detail,
  });
}

function retryableStatus(status) {
  return [408, 425, 429].includes(status) || (status >= 500 && status <= 599);
}

function assertResponseStatus(response, expected, operation) {
  if (response.status !== expected) {
    throw new MirrorValidationError(`${operation} returned HTTP ${response.status}, expected ${expected}`);
  }
}

function assertExactHeader(response, name, expected, operation) {
  const actual = response.headers.get(name);
  if (actual !== expected) {
    throw new MirrorValidationError(
      `${operation} ${name} is ${JSON.stringify(actual)}, expected ${JSON.stringify(expected)}`,
    );
  }
}

function strictContentLength(value) {
  if (value == null) return null;
  if (!/^(0|[1-9][0-9]*)$/.test(value)) return Number.NaN;
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) ? parsed : Number.NaN;
}

function canonicalTimestamp(value) {
  if (typeof value !== "string" || value.length > 64) return false;
  const milliseconds = Date.parse(value);
  return Number.isFinite(milliseconds) && new Date(milliseconds).toISOString() === value;
}

function assertSha1(value, name) {
  if (typeof value !== "string" || !SHA1.test(value)) {
    throw new MirrorValidationError(`${name} must be a full lowercase 40-hex commit`);
  }
  return value;
}

function validatePositiveId(value, name) {
  if (!Number.isSafeInteger(value) || value <= 0) {
    throw new MirrorValidationError(`${name} must be a positive safe integer`);
  }
  return value;
}

function isRecord(value) {
  return value != null && typeof value === "object" && !Array.isArray(value);
}

function exactRecord(value, keys) {
  if (!isRecord(value)) return false;
  const actual = Object.keys(value);
  return actual.length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}

function deepFreeze(value) {
  if (value == null || typeof value !== "object" || ArrayBuffer.isView(value)) return value;
  for (const child of Object.values(value)) deepFreeze(child);
  return Object.freeze(value);
}

function bufferView(bytes) {
  return Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength);
}

function isRedirectStatus(status) {
  return [301, 302, 303, 307, 308].includes(status);
}

async function cancelResponse(response) {
  await response.body?.cancel().catch(() => undefined);
}

function defaultSleep(milliseconds) {
  return new Promise((resolveSleep) => setTimeout(resolveSleep, milliseconds));
}

async function commandLineMain() {
  const args = process.argv.slice(2);
  if (args[0] === "--github-api-child") {
    if (args.length !== 2) throw new MirrorValidationError("GitHub child invocation is invalid");
    await githubApiChildMain(args[1]);
    return;
  }
  const result = await runMirrorCommand({ args });
  if (result.help) {
    process.stdout.write(`${result.text}\n`);
  } else {
    process.stdout.write(`${JSON.stringify(result)}\n`);
  }
}

if (process.argv[1] != null && resolve(process.argv[1]) === resolve(scriptPath)) {
  try {
    await commandLineMain();
  } catch (cause) {
    const name = typeof cause?.name === "string" ? cause.name : "Error";
    const message = typeof cause?.message === "string" ? cause.message : String(cause);
    process.stderr.write(`${name}: ${message}\n`);
    process.exitCode = 1;
  }
}
