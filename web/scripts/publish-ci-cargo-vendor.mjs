import { createHash } from "node:crypto";
import { createReadStream, fstatSync, readSync } from "node:fs";
import { Readable } from "node:stream";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const scriptPath = fileURLToPath(import.meta.url);
const sha1Pattern = /^[a-f0-9]{40}$/;
const sha256Pattern = /^[a-f0-9]{64}$/;
const maximumBundleBytes = 256 * 1024 * 1024;
const maximumDescriptorBytes = 16 * 1024;
const multipartPartBytes = 32 * 1024 * 1024;
const maximumErrorBytes = 1_000;
const frameMagic = Buffer.from("NANOCODEX-CI-CARGO-VENDOR\0", "ascii");
const frameVersion = 1;
const artifactFd = 3;
const uuidV4Pattern = /^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;
const defaultRequestTimeoutMs = 60_000;
const maximumRequestTimeoutMs = 120_000;
const defaultCleanupTimeoutMs = 750;
const maximumCleanupTimeoutMs = 5_000;
const defaultRetryDelayCapMs = 1_000;
const maximumRetryDelayCapMs = 5_000;
const multipartCreateAttempts = 3;
const completionSignalRecoveryMs = 350;

export const cargoVendorFrame = Object.freeze({
  magic: Buffer.from(frameMagic),
  version: frameVersion,
  maximumDescriptorBytes,
  maximumBundleBytes,
  maximumTotalBytes:
    frameMagic.length + 8 + maximumDescriptorBytes + maximumBundleBytes,
});

export function canonicalJson(value) {
  return JSON.stringify(canonicalValue(value));
}

export function deterministicMultipartRequestId(descriptor) {
  const immutable = canonicalJson(validateDescriptor(descriptor));
  const bytes = createHash("sha256")
    .update("nanocodex-ci-cargo-vendor-multipart-v1\0", "utf8")
    .update(immutable, "utf8")
    .digest()
    .subarray(0, 16);
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = bytes.toString("hex");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-` +
    `${hex.slice(16, 20)}-${hex.slice(20)}`;
}

function canonicalValue(value) {
  if (Array.isArray(value)) return value.map(canonicalValue);
  if (value && typeof value === "object") {
    const result = {};
    for (const key of Object.keys(value).sort()) result[key] = canonicalValue(value[key]);
    return result;
  }
  return value;
}

export function validateDescriptor(value) {
  if (!record(value) || value.version !== 1) throw new Error("invalid Cargo vendor descriptor");
  const common =
    sha1Pattern.test(value.cargoLockBlob) && sha256Pattern.test(value.sha256) &&
    Number.isSafeInteger(value.size) && value.size > 0 && value.size <= maximumBundleBytes &&
    value.key === `cargo-vendor/${value.cargoLockBlob}/${value.sha256}/bundle.tar.gz`;
  if (!common) throw new Error("invalid Cargo vendor descriptor");
  const keys = Object.keys(value).sort();
  if (sameKeys(keys, [
    "baseHead", "cargoLockBlob", "key", "mergeHead", "number",
    "pullRequestHead", "sha256", "size", "version",
  ])) {
    if (
      !Number.isSafeInteger(value.number) || value.number <= 0 ||
      !sha1Pattern.test(value.baseHead) || !sha1Pattern.test(value.pullRequestHead) ||
      !sha1Pattern.test(value.mergeHead)
    ) throw new Error("invalid PR Cargo vendor descriptor");
    return value;
  }
  if (sameKeys(keys, ["cargoLockBlob", "head", "key", "sha256", "size", "version"])) {
    if (!sha1Pattern.test(value.head)) throw new Error("invalid master Cargo vendor descriptor");
    return value;
  }
  throw new Error("invalid Cargo vendor descriptor fields");
}

export async function readFramedArtifact(fd = artifactFd, {
  expectedUid = process.getuid?.(),
  expectedGid = process.getgid?.(),
  signal,
} = {}) {
  if (!Number.isSafeInteger(fd) || fd < 0) throw new Error("invalid Cargo vendor artifact fd");
  throwIfAborted(signal);
  const before = validateArtifactIdentity(fstatSync(fd), expectedUid, expectedGid);
  const fixed = readExactly(fd, 0, frameMagic.length + 8);
  if (!fixed.subarray(0, frameMagic.length).equals(frameMagic)) {
    throw new Error("invalid Cargo vendor frame magic");
  }
  if (fixed.readUInt32BE(frameMagic.length) !== frameVersion) {
    throw new Error("unsupported Cargo vendor frame version");
  }
  const descriptorLength = fixed.readUInt32BE(frameMagic.length + 4);
  if (descriptorLength <= 0 || descriptorLength > maximumDescriptorBytes) {
    throw new Error("invalid Cargo vendor descriptor length");
  }
  const descriptorOffset = fixed.length;
  const descriptorBytes = readExactly(fd, descriptorOffset, descriptorLength);
  let descriptor;
  try {
    descriptor = JSON.parse(descriptorBytes.toString("utf8"));
  } catch {
    throw new Error("Cargo vendor descriptor is not JSON");
  }
  validateDescriptor(descriptor);
  if (canonicalJson(descriptor) !== descriptorBytes.toString("utf8")) {
    throw new Error("Cargo vendor descriptor is not canonical JSON");
  }
  const payloadOffset = descriptorOffset + descriptorLength;
  const exactSize = payloadOffset + descriptor.size;
  if (before.size !== exactSize) {
    throw new Error(
      before.size > exactSize
        ? "Cargo vendor frame has trailing bytes"
        : "Cargo vendor frame payload is truncated",
    );
  }
  const hash = createHash("sha256");
  let size = 0;
  for await (const chunk of createReadStream("", {
    fd,
    autoClose: false,
    start: payloadOffset,
    end: exactSize - 1,
    ...(signal ? { signal } : {}),
  })) {
    size += chunk.length;
    if (size > descriptor.size) throw new Error("Cargo vendor payload exceeds descriptor size");
    hash.update(chunk);
  }
  if (size !== descriptor.size || hash.digest("hex") !== descriptor.sha256) {
    throw new Error("Cargo vendor payload does not match its descriptor");
  }
  const after = validateArtifactIdentity(fstatSync(fd), expectedUid, expectedGid);
  if (!sameIdentity(before, after)) throw new Error("Cargo vendor artifact changed while parsing");
  return { descriptor, fd, payloadOffset, identity: after };
}

export async function main({
  args = process.argv.slice(2),
  env = process.env,
  fd = artifactFd,
  log = console.log,
  signal,
  requestTimeoutMs = defaultRequestTimeoutMs,
  cleanupTimeoutMs = defaultCleanupTimeoutMs,
  retryDelayCapMs = defaultRetryDelayCapMs,
} = {}) {
  if (args.length !== 0) throw new Error("Cargo vendor uploader accepts no arguments");
  for (const name of [
    "NANOCODEX_REPO",
    "NANOCODEX_CI_CARGO_VENDOR_PATH",
    "NANOCODEX_CI_CARGO_VENDOR_FD",
  ]) {
    if (typeof env[name] === "string" && env[name] !== "") {
      throw new Error(`Cargo vendor uploader rejects ${name}`);
    }
  }
  const ambientAuthorities = Object.keys(env).filter((name) => {
    if (["NANOCODEX_CI_ORIGIN", "NANOCODEX_CI_TOKEN"].includes(name)) return false;
    return name.startsWith("NANOCODEX_") ||
      /^(?:AWS|CF|CLOUDFLARE|GH|GITHUB|NPM|R2)_/.test(name) ||
      /(?:^|_)(?:AUTH|CREDENTIAL|PASSWORD|SECRET|SESSION|TOKEN)(?:_|$)/.test(name);
  });
  if (ambientAuthorities.length > 0) {
    throw new Error(
      "Cargo vendor uploader rejects ambient authorities: " +
      ambientAuthorities.sort().join(", "),
    );
  }
  const origin = parseOrigin(requiredEnvironment(env, "NANOCODEX_CI_ORIGIN"));
  const token = requiredEnvironment(env, "NANOCODEX_CI_TOKEN");
  validateBoundedInteger(requestTimeoutMs, 1, maximumRequestTimeoutMs, "request timeout");
  validateBoundedInteger(cleanupTimeoutMs, 1, maximumCleanupTimeoutMs, "cleanup timeout");
  validateBoundedInteger(retryDelayCapMs, 0, maximumRetryDelayCapMs, "retry delay cap");
  const shutdown = new AbortController();
  const operationSignal = signal
    ? AbortSignal.any([signal, shutdown.signal])
    : shutdown.signal;
  const stop = () => shutdown.abort(new DOMException("Cargo vendor uploader stopped", "AbortError"));
  process.once("SIGINT", stop);
  process.once("SIGTERM", stop);
  try {
    const artifact = await readFramedArtifact(fd, { signal: operationSignal });
    const { descriptor } = artifact;
    const requestOptions = { signal: operationSignal, timeoutMs: requestTimeoutMs };
    const existing = await readPublishedBundle(
      origin,
      token,
      descriptor.cargoLockBlob,
      descriptor.sha256,
      requestOptions,
    );
    let published;
    if (existing) {
      if (!matchesPublishedDescriptor(existing, descriptor)) {
        throw new Error("published Cargo vendor differs from framed artifact");
      }
      published = existing;
    } else {
      published = await publishMultipartBundle(origin, token, artifact, {
        signal: operationSignal,
        requestTimeoutMs,
        cleanupTimeoutMs,
        retryDelayCapMs,
      });
    }
    if (!matchesPublishedDescriptor(published, descriptor)) {
      throw new Error("Cargo vendor upload returned mismatched metadata");
    }
    const after = validateArtifactIdentity(
      fstatSync(fd),
      process.getuid?.(),
      process.getgid?.(),
    );
    if (!sameIdentity(artifact.identity, after)) {
      throw new Error("Cargo vendor artifact changed during upload");
    }
    log(canonicalJson(descriptor));
    return descriptor;
  } finally {
    process.removeListener("SIGINT", stop);
    process.removeListener("SIGTERM", stop);
  }
}

export async function readPublishedBundle(
  origin,
  token,
  cargoLockBlob,
  bundleSha256,
  { signal, timeoutMs = defaultRequestTimeoutMs } = {},
) {
  if (!sha1Pattern.test(cargoLockBlob) || !sha256Pattern.test(bundleSha256)) {
    throw new Error("invalid Cargo vendor identity");
  }
  const response = await authenticatedFetch(
    `${origin}/api/ci/cargo-vendor/${cargoLockBlob}/${bundleSha256}/bundle.tar.gz`,
    token,
    { method: "HEAD", redirect: "error", signal },
    { timeoutMs },
  );
  if (response.status === 404) {
    await response.body?.cancel();
    return null;
  }
  if (!response.ok) {
    throw new Error(await responseError("inspect CI Cargo vendor", response, token));
  }
  const size = parseCanonicalInteger(response.headers.get("content-length"));
  const observedLock = response.headers.get("x-nanocodex-cargo-lock");
  const key = response.headers.get("x-nanocodex-key");
  const sha256 = response.headers.get("x-nanocodex-sha256");
  await response.body?.cancel();
  if (
    size == null || size <= 0 || size > maximumBundleBytes || observedLock !== cargoLockBlob ||
    sha256 !== bundleSha256 || key !== cargoVendorKey(cargoLockBlob, bundleSha256)
  ) throw new Error("published CI Cargo vendor returned invalid exact-object metadata");
  return { key, cargoLockBlob, size, sha256, uploaded: false };
}

export async function publishMultipartBundle(origin, token, artifact, {
  signal,
  requestTimeoutMs = defaultRequestTimeoutMs,
  cleanupTimeoutMs = defaultCleanupTimeoutMs,
  retryDelayCapMs = defaultRetryDelayCapMs,
} = {}) {
  validateBoundedInteger(
    requestTimeoutMs,
    1,
    maximumRequestTimeoutMs,
    "request timeout",
  );
  validateBoundedInteger(cleanupTimeoutMs, 1, maximumCleanupTimeoutMs, "cleanup timeout");
  validateBoundedInteger(retryDelayCapMs, 0, maximumRetryDelayCapMs, "retry delay cap");
  const descriptor = validateDescriptor(artifact?.descriptor);
  const identity = validateArtifactIdentity(
    fstatSync(artifact.fd),
    process.getuid?.(),
    process.getgid?.(),
  );
  if (!sameIdentity(identity, artifact.identity)) {
    throw new Error("Cargo vendor artifact changed before upload");
  }
  const partCount = Math.ceil(descriptor.size / multipartPartBytes);
  if (partCount <= 0 || partCount > 10_000) throw new Error("invalid multipart part count");
  const endpoint = `${origin}/api/ci/cargo-vendor/${descriptor.cargoLockBlob}/${descriptor.sha256}/multipart`;
  const requestId = deterministicMultipartRequestId(descriptor);
  const createBody = canonicalJson({
    partCount,
    partSize: multipartPartBytes,
    requestId,
    sha256: descriptor.sha256,
    size: descriptor.size,
    version: 1,
  });
  const created = await createMultipartUpload(endpoint, token, descriptor, {
    createBody,
    partCount,
    requestId,
    retryDelayCapMs,
    signal,
    timeoutMs: requestTimeoutMs,
  });
  if (created.type === "published") return created.value;
  const upload = created.value;

  const parts = [];
  let canonicalProven = false;
  try {
    for (let index = 0; index < partCount; index += 1) {
      const payloadStart = index * multipartPartBytes;
      const payloadEnd = Math.min(payloadStart + multipartPartBytes, descriptor.size);
      const size = payloadEnd - payloadStart;
      const fileStart = artifact.payloadOffset + payloadStart;
      const fileEnd = artifact.payloadOffset + payloadEnd;
      const sha256 = await hashFileRange(artifact.fd, fileStart, fileEnd);
      const response = await authenticatedFetch(
        `${endpoint}/parts/${index + 1}`,
        token,
        {
          method: "PUT",
          redirect: "error",
          headers: {
            "content-length": String(size),
            "content-type": "application/octet-stream",
            "x-nanocodex-sha256": sha256,
            "x-nanocodex-staging-id": upload.stagingId,
            "x-nanocodex-upload-id": upload.uploadId,
          },
          body: Readable.toWeb(createReadStream("", {
            fd: artifact.fd,
            autoClose: false,
            start: fileStart,
            end: fileEnd - 1,
          })),
          duplex: "half",
          signal,
        },
        { timeoutMs: requestTimeoutMs },
      );
      if (!response.ok) {
        throw new Error(await responseError(
          `upload CI Cargo vendor part ${index + 1}`,
          response,
          token,
        ));
      }
      const value = await boundedJson(response, 64 * 1024, "multipart part");
      if (
        !hasExactKeys(value, ["etag", "partNumber", "sha256", "size"]) ||
        value?.partNumber !== index + 1 || value.size !== size || value.sha256 !== sha256 ||
        typeof value.etag !== "string" || !/^[a-f0-9]{32}$/.test(value.etag)
      ) throw new Error(`multipart part ${index + 1} returned invalid metadata`);
      parts.push({ partNumber: value.partNumber, etag: value.etag });
    }
    let completionFailure;
    try {
      const response = await authenticatedFetch(
        `${endpoint}/complete`,
        token,
        {
          method: "POST",
          redirect: "error",
          headers: { "content-type": "application/json" },
          body: canonicalJson({
            parts,
            sha256: descriptor.sha256,
            size: descriptor.size,
            stagingId: upload.stagingId,
            uploadId: upload.uploadId,
            version: 1,
          }),
          signal,
        },
        { timeoutMs: requestTimeoutMs },
      );
      if (!response.ok) {
        completionFailure = new Error(await responseError(
          "complete CI Cargo vendor multipart upload",
          response,
          token,
        ));
      } else {
        try {
          const published = validateCompletionResponse(
            await boundedJson(response, 64 * 1024, "multipart completion"),
            descriptor,
          );
          canonicalProven = true;
          return published;
        } catch (cause) {
          completionFailure = cause;
        }
      }
    } catch (cause) {
      completionFailure = cause;
    }
    const recoverySignal = signal?.aborted
      ? AbortSignal.timeout(Math.min(cleanupTimeoutMs, completionSignalRecoveryMs))
      : signal;
    let existing;
    try {
      existing = await readPublishedBundle(
        origin,
        token,
        descriptor.cargoLockBlob,
        descriptor.sha256,
        { signal: recoverySignal, timeoutMs: requestTimeoutMs },
      );
    } catch (recoveryCause) {
      throw new AggregateError(
        [completionFailure, recoveryCause],
        "multipart completion outcome and exact-object recovery both failed",
      );
    }
    if (existing && matchesPublishedDescriptor(existing, descriptor)) {
      canonicalProven = true;
      return existing;
    }
    throw completionFailure ?? new Error("multipart completion outcome was not canonical");
  } finally {
    if (!canonicalProven) {
      await abortMultipart(endpoint, token, upload.uploadId, upload.stagingId, {
        signal: AbortSignal.timeout(cleanupTimeoutMs),
        timeoutMs: cleanupTimeoutMs,
      });
    }
  }
}

async function createMultipartUpload(endpoint, token, descriptor, {
  createBody,
  partCount,
  requestId,
  retryDelayCapMs,
  signal,
  timeoutMs,
}) {
  let failure;
  for (let attempt = 0; attempt < multipartCreateAttempts; attempt += 1) {
    throwIfAborted(signal);
    let response;
    try {
      response = await authenticatedFetch(
        endpoint,
        token,
        {
          method: "POST",
          redirect: "error",
          headers: { "content-type": "application/json" },
          body: createBody,
          signal,
        },
        { timeoutMs },
      );
    } catch (cause) {
      throwIfAborted(signal);
      failure = new Error("create CI Cargo vendor multipart upload transport failed", {
        cause,
      });
      if (attempt + 1 === multipartCreateAttempts) throw failure;
      await waitForRetry(fallbackRetryDelay(attempt, retryDelayCapMs), signal);
      continue;
    }

    const retryDelay = boundedRetryAfter(response, attempt, retryDelayCapMs);
    if (!response.ok) {
      try {
        failure = new Error(await responseError(
          "create CI Cargo vendor multipart upload",
          response,
          token,
        ));
      } catch (cause) {
        failure = new Error("create CI Cargo vendor multipart upload returned an unreadable error", {
          cause,
        });
      }
      if (!retryableStatus(response.status) || attempt + 1 === multipartCreateAttempts) {
        throw failure;
      }
      await waitForRetry(retryDelay, signal);
      continue;
    }

    try {
      return validateCreateResponse(
        await boundedJson(response, 64 * 1024, "multipart creation"),
        descriptor,
        { partCount, requestId },
      );
    } catch (cause) {
      failure = cause;
      if (attempt + 1 === multipartCreateAttempts) throw failure;
      await waitForRetry(retryDelay, signal);
    }
  }
  throw failure ?? new Error("multipart creation failed");
}

function validateCreateResponse(value, descriptor, { partCount, requestId }) {
  if (
    hasExactKeys(value, [
      "cargoLockBlob", "key", "requestId", "sha256", "size", "uploaded",
    ]) && value.requestId === requestId && value.uploaded === false &&
    matchesPublishedDescriptor(value, descriptor)
  ) return { type: "published", value };
  if (
    !hasExactKeys(value, [
      "cargoLockBlob", "key", "partCount", "partSize", "requestId", "sha256", "size",
      "stagingId", "uploadId",
    ]) || !matchesPublishedDescriptor(value, descriptor) || value.requestId !== requestId ||
    value.stagingId !== requestId || !uuidV4Pattern.test(value.stagingId) ||
    typeof value.uploadId !== "string" || value.uploadId.length === 0 ||
    value.uploadId.length > 1_024 || value.partSize !== multipartPartBytes ||
    value.partCount !== partCount
  ) throw new Error("multipart creation returned invalid metadata");
  return { type: "upload", value };
}

function validateCompletionResponse(value, descriptor) {
  if (
    !hasExactKeys(value, ["cargoLockBlob", "key", "sha256", "size", "uploaded"]) ||
    typeof value.uploaded !== "boolean" || !matchesPublishedDescriptor(value, descriptor)
  ) throw new Error("multipart completion returned invalid metadata");
  return value;
}

function matchesPublishedDescriptor(value, descriptor) {
  return record(value) && value.key === descriptor.key &&
    value.cargoLockBlob === descriptor.cargoLockBlob && value.size === descriptor.size &&
    value.sha256 === descriptor.sha256;
}

function retryableStatus(status) {
  return status === 408 || status === 425 || status === 429 || status >= 500;
}

function boundedRetryAfter(response, attempt, capMs) {
  const value = response.headers.get("retry-after");
  let delay;
  if (value != null && /^[0-9]+$/.test(value)) {
    const seconds = Number(value);
    if (Number.isSafeInteger(seconds)) delay = seconds * 1_000;
  } else if (value != null) {
    const at = Date.parse(value);
    if (Number.isFinite(at)) delay = Math.max(0, at - Date.now());
  }
  if (!Number.isSafeInteger(delay) || delay < 0) {
    return fallbackRetryDelay(attempt, capMs);
  }
  return Math.min(delay, capMs);
}

function fallbackRetryDelay(attempt, capMs) {
  return Math.min(100 * (2 ** attempt), capMs);
}

async function waitForRetry(milliseconds, signal) {
  if (milliseconds <= 0) return;
  throwIfAborted(signal);
  await new Promise((resolvePromise, rejectPromise) => {
    const timer = setTimeout(done, milliseconds);
    const onAbort = () => done(signal.reason ?? new DOMException("Aborted", "AbortError"));
    function done(cause) {
      clearTimeout(timer);
      signal?.removeEventListener("abort", onAbort);
      if (cause) rejectPromise(cause);
      else resolvePromise();
    }
    signal?.addEventListener("abort", onAbort, { once: true });
    if (signal?.aborted) onAbort();
  });
}

async function abortMultipart(endpoint, token, uploadId, stagingId, {
  signal,
  timeoutMs = defaultCleanupTimeoutMs,
} = {}) {
  for (let attempt = 0; attempt < multipartCreateAttempts; attempt += 1) {
    try {
      const response = await authenticatedFetch(
        endpoint,
        token,
        {
          method: "DELETE",
          redirect: "error",
          headers: { "content-type": "application/json" },
          body: canonicalJson({ stagingId, uploadId, version: 1 }),
          signal,
        },
        { timeoutMs },
      );
      await response.body?.cancel();
      if (response.ok || response.status === 404) return;
    } catch {}
  }
}

async function hashFileRange(fd, start, endExclusive) {
  const hash = createHash("sha256");
  let size = 0;
  for await (const chunk of createReadStream("", {
    fd,
    autoClose: false,
    start,
    end: endExclusive - 1,
  })) {
    size += chunk.length;
    hash.update(chunk);
  }
  if (size !== endExclusive - start) throw new Error("Cargo vendor part was truncated");
  return hash.digest("hex");
}

function readExactly(fd, position, length) {
  const buffer = Buffer.alloc(length);
  let offset = 0;
  while (offset < length) {
    const read = readSync(fd, buffer, offset, length - offset, position + offset);
    if (read === 0) throw new Error("Cargo vendor frame is truncated");
    offset += read;
  }
  return buffer;
}

function validateArtifactIdentity(identity, expectedUid, expectedGid) {
  if (
    !identity.isFile() || identity.isSymbolicLink?.() || identity.nlink !== 1 ||
    (identity.mode & 0o777) !== 0o600 ||
    (Number.isSafeInteger(expectedUid) && identity.uid !== expectedUid) ||
    (Number.isSafeInteger(expectedGid) && identity.gid !== expectedGid) ||
    !Number.isSafeInteger(identity.size) || identity.size <= frameMagic.length + 8 ||
    identity.size > cargoVendorFrame.maximumTotalBytes
  ) throw new Error("Cargo vendor artifact fd must be one private controller-owned regular file");
  return {
    dev: identity.dev,
    ino: identity.ino,
    uid: identity.uid,
    gid: identity.gid,
    mode: identity.mode,
    nlink: identity.nlink,
    size: identity.size,
    mtimeMs: identity.mtimeMs,
    ctimeMs: identity.ctimeMs,
  };
}

function sameIdentity(left, right) {
  return left.dev === right.dev && left.ino === right.ino && left.uid === right.uid &&
    left.gid === right.gid && left.mode === right.mode && left.nlink === right.nlink &&
    left.size === right.size && left.mtimeMs === right.mtimeMs &&
    left.ctimeMs === right.ctimeMs;
}

function authenticatedFetch(url, token, init = {}, { timeoutMs = defaultRequestTimeoutMs } = {}) {
  validateBoundedInteger(timeoutMs, 1, maximumRequestTimeoutMs, "request timeout");
  const headers = new Headers(init.headers);
  headers.set("authorization", `Bearer ${token}`);
  const timeout = AbortSignal.timeout(timeoutMs);
  const signal = init.signal ? AbortSignal.any([init.signal, timeout]) : timeout;
  return fetch(url, { ...init, headers, signal });
}

async function boundedJson(response, maximum, description) {
  const body = await readResponseBounded(response, maximum, description);
  try {
    return JSON.parse(body.toString("utf8"));
  } catch (cause) {
    throw new Error(`${description} response is not JSON`, { cause });
  }
}

async function responseError(operation, response, token) {
  const detail = (await readResponseBounded(response, maximumErrorBytes, operation, true))
    .toString("utf8");
  const safe = redactCredential(detail, token);
  return `${operation} failed with HTTP ${response.status}${safe ? `: ${safe}` : ""}`;
}

async function readResponseBounded(response, maximum, description, truncate = false) {
  if (!response.body) return Buffer.alloc(0);
  const reader = response.body.getReader();
  const chunks = [];
  let bytes = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      if (bytes + value.byteLength > maximum) {
        if (truncate) {
          const remaining = maximum - bytes;
          if (remaining > 0) chunks.push(Buffer.from(value).subarray(0, remaining));
          bytes = maximum;
          await reader.cancel();
          break;
        }
        await reader.cancel();
        throw new Error(`${description} response exceeds ${maximum} bytes`);
      }
      chunks.push(Buffer.from(value));
      bytes += value.byteLength;
    }
  } finally {
    reader.releaseLock();
  }
  return Buffer.concat(chunks, bytes);
}

export function parseOrigin(value) {
  try {
    const url = new URL(value);
    if (!["http:", "https:"].includes(url.protocol) || url.username || url.password) {
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
    throw new Error(
      "NANOCODEX_CI_ORIGIN must use HTTPS (HTTP is allowed only for loopback development)",
      { cause },
    );
  }
}

function cargoVendorKey(cargoLockBlob, bundleSha256) {
  return `cargo-vendor/${cargoLockBlob}/${bundleSha256}/bundle.tar.gz`;
}

function parseCanonicalInteger(value) {
  if (typeof value !== "string" || !/^(?:0|[1-9][0-9]*)$/.test(value)) return undefined;
  const number = Number(value);
  return Number.isSafeInteger(number) ? number : undefined;
}

function requiredEnvironment(env, name) {
  const value = env[name]?.trim();
  if (!value) throw new Error(`${name} is required`);
  return value;
}

function validateBoundedInteger(value, minimum, maximum, description) {
  if (!Number.isSafeInteger(value) || value < minimum || value > maximum) {
    throw new Error(`${description} must be an integer from ${minimum} through ${maximum}`);
  }
  return value;
}

function redactCredential(value, token) {
  let result = value.replace(/Bearer\s+[^\s"']+/gi, "Bearer [redacted]");
  if (typeof token === "string" && token.length > 0) {
    result = result.split(token).join("[redacted]");
  }
  return result;
}

function throwIfAborted(signal) {
  if (signal?.aborted) {
    throw signal.reason ?? new DOMException("Aborted", "AbortError");
  }
}

function hasExactKeys(value, expected) {
  return record(value) && sameKeys(Object.keys(value).sort(), [...expected].sort());
}

function sameKeys(left, right) {
  return left.length === right.length && left.every((value, index) => value === right[index]);
}

function record(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

if (resolve(process.argv[1] ?? "") === scriptPath) {
  try {
    await main();
  } catch (cause) {
    process.stderr.write(
      `Publish CI Cargo vendor failed: ${cause instanceof Error ? cause.message : String(cause)}\n`,
    );
    process.exitCode = 1;
  }
}
