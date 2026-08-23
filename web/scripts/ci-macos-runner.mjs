#!/usr/bin/env node

import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { constants as fsConstants, createReadStream } from "node:fs";
import {
  access,
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  open,
  readFile,
  realpath,
  rm,
  stat,
  unlink,
} from "node:fs/promises";
import {
  arch,
  hostname,
  platform,
  tmpdir,
  userInfo,
} from "node:os";
import { basename, dirname, isAbsolute, join, posix, resolve } from "node:path";
import { Readable } from "node:stream";
import { fileURLToPath } from "node:url";
import { createGunzip } from "node:zlib";

const scriptPath = fileURLToPath(import.meta.url);
const SHA1 = /^[a-f0-9]{40}$/;
const SHA256 = /^[a-f0-9]{64}$/;
const STABLE_TAG = /^v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$/;
const MACOS_TARGET = "aarch64-apple-darwin";
const PINNED_TOOLCHAIN = "1.98.0-aarch64-apple-darwin";
const SANDBOX_EXEC = "/usr/bin/sandbox-exec";
const SYSTEM_PATH = "/usr/bin:/bin:/usr/sbin:/sbin";
const IDENTIFIER = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,199}$/;
const DEFAULT_POLL_MS = 15_000;
const DEFAULT_HEARTBEAT_MS = 30_000;
// Leave room beneath the broker's 64 MiB object cap for the truncation marker.
const DEFAULT_HEAD_BYTES = 31 * 1024 * 1024;
const DEFAULT_TAIL_BYTES = 31 * 1024 * 1024;
const MAX_LOG_PART_BYTES = 32 * 1024 * 1024;
const MAX_STORED_LOG_BYTES = 64 * 1024 * 1024;
const LOG_MARKER_RESERVE_BYTES = 1024;
const MAX_SOURCE_ARCHIVE_BYTES = 128 * 1024 * 1024;
const MAX_CARGO_VENDOR_ARCHIVE_BYTES = 256 * 1024 * 1024;
const MAX_SOURCE_TAR_BYTES = 512 * 1024 * 1024;
const MAX_CARGO_VENDOR_TAR_BYTES = 2 * 1024 * 1024 * 1024;
const MAX_TAR_ENTRIES = 100_000;
const MAX_TAR_HEADERS = 200_000;
const MAX_TAR_PATH_COMPONENTS = 1_000_000;
const MAX_TAR_REQUIRED_DIRECTORIES = 200_000;
const MAX_TAR_METADATA_BYTES = 128 * 1024 * 1024;
const MAX_TAR_PATH_BYTES = 4_096;
const MAX_PAX_BYTES = 1024 * 1024;
const MAX_CARGO_CONFIG_BYTES = 1024 * 1024;
const MAX_RELEASE_ASSET_BYTES = 128 * 1024 * 1024;
const REQUEST_TIMEOUT_MS = 60_000;
const LEASE_SAFETY_MARGIN_MS = 10_000;
const LEASE_PROCESS_KILL_GRACE_MS = 1_000;
const ERROR_RESPONSE_BYTES = 16 * 1024;
const CARGO_VENDOR_DIRECTORY = "/workspace/.cargo-home/vendor";
const MACH_HEADER_64_BYTES = 32;
const MAX_MACH_LOAD_COMMAND_BYTES = 16 * 1024 * 1024;
const MACHO_64_MAGIC = 0xfeedfacf;
const MACHO_ARM64_CPU = 0x0100000c;
const MACHO_EXECUTE_FILE_TYPE = 2;
const MACHO_SEGMENT_64_COMMAND = 0x19;
const MACHO_ZERO_FILL_SECTION_TYPES = new Set([0x1, 0xc, 0x12]);

// The server chooses only a symbolic task. It never supplies an executable,
// arguments, a shell fragment, a working directory, or environment variables.
const TASKS = Object.freeze({
  "workspace-test": Object.freeze({
    name: "workspace-test",
    arguments: Object.freeze(["test", "--workspace", "--locked"]),
    timeoutMs: 45 * 60 * 1_000,
    asset: null,
  }),
  "release-build": Object.freeze({
    name: "release-build",
    timeoutMs: 60 * 60 * 1_000,
  }),
  "native-build": Object.freeze({
    name: "native-build",
    testArguments: Object.freeze([
      "test",
      "--workspace",
      "--locked",
      "--offline",
    ]),
    testTimeoutMs: 45 * 60 * 1_000,
    arguments: Object.freeze([
      "build",
      "--locked",
      "--offline",
      "--profile",
      "nightly",
      "--package",
      "nanocodex-bin",
      "--bin",
      "nanocodex",
      "--features",
      "tempo",
      "--target",
      MACOS_TARGET,
    ]),
    timeoutMs: 60 * 60 * 1_000,
    networkAccess: false,
    cargoOffline: true,
    asset: Object.freeze({
      path: join("target", MACOS_TARGET, "nightly", "nanocodex"),
    }),
  }),
});

export class RunnerProtocolError extends Error {
  constructor(message, options) {
    super(message, options);
    this.name = "RunnerProtocolError";
  }
}

export class RunnerHttpError extends Error {
  constructor(operation, status, detail = "") {
    super(`${operation} failed with HTTP ${status}${detail ? `: ${detail}` : ""}`);
    this.name = "RunnerHttpError";
    this.status = status;
  }
}

export class BoundedLogCapture {
  #head = [];
  #headBytes = 0;
  #tail = [];
  #tailBytes = 0;
  #bytesObserved = 0;

  constructor({ headBytes = DEFAULT_HEAD_BYTES, tailBytes = DEFAULT_TAIL_BYTES } = {}) {
    this.headLimit = logLimit(headBytes, "headBytes");
    this.tailLimit = logLimit(tailBytes, "tailBytes");
    if (this.headLimit + this.tailLimit > MAX_STORED_LOG_BYTES - LOG_MARKER_RESERVE_BYTES) {
      throw new TypeError("combined log retention leaves no room for the truncation marker");
    }
  }

  append(value) {
    let chunk = Buffer.isBuffer(value) ? value : Buffer.from(value);
    this.#bytesObserved += chunk.byteLength;
    if (this.#headBytes < this.headLimit) {
      const retained = chunk.subarray(0, this.headLimit - this.#headBytes);
      if (retained.byteLength > 0) {
        this.#head.push(Buffer.from(retained));
        this.#headBytes += retained.byteLength;
        chunk = chunk.subarray(retained.byteLength);
      }
    }
    if (chunk.byteLength === 0 || this.tailLimit === 0) return;
    this.#tail.push(Buffer.from(chunk));
    this.#tailBytes += chunk.byteLength;
    while (this.#tailBytes > this.tailLimit) {
      const overflow = this.#tailBytes - this.tailLimit;
      const first = this.#tail[0];
      if (first.byteLength <= overflow) {
        this.#tail.shift();
        this.#tailBytes -= first.byteLength;
      } else {
        this.#tail[0] = first.subarray(overflow);
        this.#tailBytes -= overflow;
      }
    }
  }

  result() {
    const omitted = this.#bytesObserved - this.#headBytes - this.#tailBytes;
    const truncated = omitted > 0;
    const marker = truncated
      ? Buffer.from(`\n[... nanocodex macOS CI omitted ${omitted} log bytes ...]\n`)
      : Buffer.alloc(0);
    const body = Buffer.concat([...this.#head, marker, ...this.#tail]);
    return Object.freeze({
      body,
      bytesObserved: this.#bytesObserved,
      bytesStored: body.byteLength,
      truncated,
      sha256: sha256(body),
    });
  }
}

export function resolveTask(value, release, head, publishedAt) {
  const task = typeof value === "string" ? TASKS[value] : undefined;
  if (!task) {
    throw new RunnerProtocolError(`unsupported macOS CI task: ${String(value)}`);
  }
  if (value === "workspace-test") return task;
  if (value === "native-build") {
    if (
      typeof head !== "string" || !SHA1.test(head) || release !== undefined ||
      !isCanonicalTimestamp(publishedAt)
    ) throw new RunnerProtocolError("native-build requires a canonical source identity");
    return Object.freeze({
      ...task,
      environment: Object.freeze({
        TAG_NAME: "pr",
        VERGEN_GIT_SHA: head,
        VERGEN_BUILD_TIMESTAMP: publishedAt,
      }),
    });
  }
  if (
    typeof head !== "string" || !SHA1.test(head) || !isRecord(release) ||
    !(
      (release.channel === "nightly" && release.tagName === "nightly") ||
      (release.channel === "stable" && typeof release.tagName === "string" &&
        STABLE_TAG.test(release.tagName))
    ) ||
    typeof release.buildTimestamp !== "string" ||
    !Number.isFinite(Date.parse(release.buildTimestamp)) ||
    Object.keys(release).some((key) =>
      key !== "channel" && key !== "tagName" && key !== "buildTimestamp"
    )
  ) throw new RunnerProtocolError("release-build requires a canonical release identity");
  const profile = release.channel === "nightly" ? "nightly" : "release";
  return Object.freeze({
    name: "release-build",
    arguments: Object.freeze([
      "build",
      "--locked",
      ...(profile === "nightly" ? ["--profile", "nightly"] : ["--release"]),
      "--package",
      "nanocodex-bin",
      "--bin",
      "nanocodex",
      "--features",
      "tempo",
      "--target",
      MACOS_TARGET,
    ]),
    timeoutMs: task.timeoutMs,
    environment: Object.freeze({
      TAG_NAME: release.tagName,
      VERGEN_GIT_SHA: head,
      VERGEN_BUILD_TIMESTAMP: release.buildTimestamp,
    }),
    asset: Object.freeze({
      path: join("target", MACOS_TARGET, profile, "nanocodex"),
    }),
  });
}

export function parseOrigin(value) {
  try {
    const url = new URL(value);
    if (!["http:", "https:"].includes(url.protocol) || url.username || url.password) {
      throw new Error("unsupported URL");
    }
    const loopback = url.hostname === "localhost" ||
      url.hostname.endsWith(".localhost") ||
      url.hostname === "127.0.0.1" ||
      url.hostname === "[::1]";
    if (url.protocol !== "https:" && !loopback) throw new Error("HTTPS is required");
    url.pathname = url.pathname.replace(/\/+$/, "");
    url.search = "";
    url.hash = "";
    return url.toString().replace(/\/$/, "");
  } catch (error) {
    throw new RunnerProtocolError(
      "NANOCODEX_CI_ORIGIN must use HTTPS (HTTP is allowed only for loopback development)",
      { cause: error },
    );
  }
}

export function hostMetadata() {
  return Object.freeze({
    hostname: hostname(),
    platform: platform(),
    arch: arch(),
  });
}

export async function resolveSandboxRuntime({ env = process.env } = {}) {
  const host = hostMetadata();
  if (host.platform !== "darwin" || host.arch !== "arm64") {
    throw new RunnerProtocolError(
      `macOS CI sandbox requires darwin/arm64, received ${host.platform}/${host.arch}`,
    );
  }
  // os.homedir() trusts ambient HOME; passwd-backed userInfo() identifies the
  // actual account home that the profile must hide from an untrusted checkout.
  const realHome = await canonicalDirectory(userInfo().homedir, "runner home");
  const configuredRustupHome = env.RUSTUP_HOME?.trim();
  const rustupHomePath = configuredRustupHome || join(realHome, ".rustup");
  const rustupHome = await canonicalDirectory(rustupHomePath, "RUSTUP_HOME");
  const toolchainDirectory = await canonicalDirectory(
    join(rustupHome, "toolchains", PINNED_TOOLCHAIN),
    `Rust ${PINNED_TOOLCHAIN} toolchain`,
  );
  const [sandboxExec, cargo, rustc, rustdoc] = await Promise.all([
    canonicalExecutable(SANDBOX_EXEC, "sandbox-exec"),
    canonicalExecutable(join(toolchainDirectory, "bin", "cargo"), "pinned cargo"),
    canonicalExecutable(join(toolchainDirectory, "bin", "rustc"), "pinned rustc"),
    canonicalExecutable(join(toolchainDirectory, "bin", "rustdoc"), "pinned rustdoc"),
  ]);
  if (sandboxExec !== SANDBOX_EXEC) {
    throw new RunnerProtocolError(`${SANDBOX_EXEC} did not resolve to its canonical path`);
  }
  for (const [name, path] of Object.entries({ cargo, rustc, rustdoc })) {
    if (!isWithin(toolchainDirectory, path)) {
      throw new RunnerProtocolError(`${name} escaped the pinned Rust toolchain`);
    }
  }
  return Object.freeze({
    sandboxExec,
    realHome,
    rustupHome,
    toolchainDirectory,
    cargo,
    rustc,
    rustdoc,
  });
}

// sandbox-exec is defense in depth, not VM isolation. Long-lived operation is
// safe only under install-ci-macos-service.mjs's operator contract: a dedicated
// arm64 login account with no GitHub, deploy, registry, cloud, SSH-agent, or
// other credentials, and only the scoped macOS runner token in its Keychain.
export function createSandboxProfile({ jobDirectory, runtime, networkAccess = true }) {
  assertCanonicalPath(jobDirectory, "job directory");
  assertCanonicalPath(runtime?.realHome, "runner home");
  assertCanonicalPath(runtime?.toolchainDirectory, "Rust toolchain directory");
  if (typeof networkAccess !== "boolean") {
    throw new RunnerProtocolError("sandbox network policy must be boolean");
  }
  if (isWithin(runtime.realHome, jobDirectory)) {
    throw new RunnerProtocolError("macOS CI job directory must be outside the runner home");
  }
  const job = sbplString(jobDirectory);
  const home = sbplString(runtime.realHome);
  const toolchain = sbplString(runtime.toolchainDirectory);
  return [
    "(version 1)",
    "(deny default)",
    "",
    "; Cargo may create compiler/build-script children, but may not inspect other processes",
    "; or create a new process group/session that escapes parent-owned cleanup.",
    "(allow process-exec)",
    "(allow process-fork)",
    "(allow signal (target same-sandbox))",
    "(deny process-info*)",
    "(allow process-info* (target self))",
    "(deny process-info-setcontrol)",
    "",
    "; System and SDK reads are needed by rustc/clang. Hide the operator home, then",
    "; narrowly restore only the pinned rustup toolchain beneath it.",
    "(allow file-read*)",
    `(deny file-read* (subpath ${home}))`,
    `(allow file-read* (subpath ${toolchain}))`,
    `(allow file-write* (subpath ${job}))`,
    "; Apple's clang driver probes Xcode with stderr redirected to this one discard device.",
    '(allow file-write-data (literal "/dev/null"))',
    "",
    ...(networkAccess
      ? [
        "; Tests and legacy release builds may exercise network clients.",
        "(allow network*)",
      ]
      : [
        "; The fixed native build uses only the verified full Cargo.lock bundle.",
        "(deny network*)",
      ]),
    "(allow sysctl-read)",
    '(deny sysctl-read (sysctl-name-regex #"^kern\\.proc"))',
    "",
    "; Keychain/security agents and all other Mach services remain unavailable.",
    '(deny mach-lookup (global-name-regex #"^com\\.apple\\.(SecurityServer|security.*|trustd.*)"))',
    "",
  ].join("\n");
}

export async function writeSandboxProfile(path, profile) {
  assertCanonicalPath(path, "sandbox profile path");
  if (typeof profile !== "string" || profile.includes("\0") || !profile.startsWith("(version 1)\n")) {
    throw new RunnerProtocolError("invalid macOS sandbox profile");
  }
  const target = await open(path, "wx", 0o600);
  try {
    await writeAll(target, Buffer.from(profile, "utf8"));
    await target.sync();
  } finally {
    await target.close();
  }
}

export function sandboxEnvironment({
  jobDirectory,
  homeDirectory,
  cargoHome,
  temporaryDirectory,
  workspace,
  runtime,
  taskEnvironment = {},
  cargoOffline = false,
}) {
  assertCanonicalPath(jobDirectory, "job directory");
  for (const [name, path] of Object.entries({
    toolchainDirectory: runtime?.toolchainDirectory,
    rustc: runtime?.rustc,
    rustdoc: runtime?.rustdoc,
  })) assertCanonicalPath(path, name);
  for (const [name, path] of Object.entries({
    homeDirectory,
    cargoHome,
    temporaryDirectory,
    workspace,
  })) {
    assertCanonicalPath(path, name);
    if (!isWithin(jobDirectory, path)) {
      throw new RunnerProtocolError(`${name} escaped the macOS CI job directory`);
    }
  }
  const allowedTaskKeys = new Set(["TAG_NAME", "VERGEN_GIT_SHA", "VERGEN_BUILD_TIMESTAMP"]);
  for (const [name, value] of Object.entries(taskEnvironment)) {
    if (!allowedTaskKeys.has(name) || typeof value !== "string" || /[\0\r\n]/.test(value)) {
      throw new RunnerProtocolError(`invalid sandbox task environment: ${name}`);
    }
  }
  if (typeof cargoOffline !== "boolean") {
    throw new RunnerProtocolError("sandbox Cargo offline policy must be boolean");
  }
  return Object.freeze({
    PATH: `${join(runtime.toolchainDirectory, "bin")}:${SYSTEM_PATH}`,
    HOME: homeDirectory,
    CARGO_HOME: cargoHome,
    CARGO_TARGET_DIR: join(workspace, "target"),
    TMPDIR: temporaryDirectory,
    LANG: "en_US.UTF-8",
    LC_ALL: "en_US.UTF-8",
    SHELL: "/bin/sh",
    CI: "true",
    // The immutable full Cargo.lock bundle installs registry and Git source
    // replacements into this disposable, credential-free CARGO_HOME.
    CARGO_REGISTRIES_CRATES_IO_PROTOCOL: "sparse",
    CARGO_INCREMENTAL: "0",
    CARGO_TERM_COLOR: "always",
    ...(cargoOffline ? { CARGO_NET_OFFLINE: "true" } : {}),
    RUSTC: runtime.rustc,
    RUSTDOC: runtime.rustdoc,
    ...taskEnvironment,
  });
}

export async function downloadVerifiedArchive({
  url,
  destination,
  size,
  sha256: expectedSha256,
  label = "source archive",
  maximumBytes = MAX_SOURCE_ARCHIVE_BYTES,
  signal,
  fetchImpl = fetch,
}) {
  assertArchiveDescriptor({ url, size, sha256: expectedSha256 }, maximumBytes, label);
  const response = await fetchImpl(url, {
    method: "GET",
    headers: { accept: "application/gzip" },
    redirect: "error",
    signal: requestSignal(signal, 5 * 60 * 1_000),
  });
  if (!response.ok || response.body == null) {
    await response.body?.cancel().catch(() => undefined);
    throw new RunnerHttpError(`download ${label}`, response.status);
  }
  const declaredSize = response.headers.get("content-length");
  if (declaredSize != null && Number(declaredSize) !== size) {
    await response.body.cancel().catch(() => undefined);
    throw new RunnerProtocolError(
      `${label} Content-Length mismatch: expected ${size}, received ${declaredSize}`,
    );
  }

  const digest = createHash("sha256");
  const reader = response.body.getReader();
  const target = await open(destination, "wx", 0o600);
  let observed = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      observed += value.byteLength;
      if (observed > size) {
        throw new RunnerProtocolError(`${label} exceeds declared size ${size}`);
      }
      const chunk = Buffer.from(value);
      digest.update(chunk);
      await writeAll(target, chunk);
    }
    if (observed !== size) {
      throw new RunnerProtocolError(
        `${label} size mismatch: expected ${size}, received ${observed}`,
      );
    }
    const actualSha256 = digest.digest("hex");
    if (actualSha256 !== expectedSha256) {
      throw new RunnerProtocolError(
        `${label} SHA-256 mismatch: expected ${expectedSha256}, received ${actualSha256}`,
      );
    }
    await target.sync();
    return { size: observed, sha256: actualSha256 };
  } catch (error) {
    await reader.cancel(error).catch(() => undefined);
    await rm(destination, { force: true }).catch(() => undefined);
    throw error;
  } finally {
    reader.releaseLock();
    await target.close();
  }
}

export async function runCapturedProcess({
  executable,
  arguments: args = [],
  cwd,
  env,
  signal,
  timeoutMs,
  headBytes = DEFAULT_HEAD_BYTES,
  tailBytes = DEFAULT_TAIL_BYTES,
  killGraceMs = 5_000,
  capture,
}) {
  if (typeof executable !== "string" || executable.length === 0 || !Array.isArray(args)) {
    throw new TypeError("a process executable and argument array are required");
  }
  if (!Number.isSafeInteger(timeoutMs) || timeoutMs <= 0) {
    throw new TypeError("timeoutMs must be a positive integer");
  }
  if (!Number.isSafeInteger(killGraceMs) || killGraceMs < 0) {
    throw new TypeError("killGraceMs must be a non-negative integer");
  }
  signal?.throwIfAborted();

  if (
    capture !== undefined &&
    (!isRecord(capture) || !(capture.stdout instanceof BoundedLogCapture) ||
      !(capture.stderr instanceof BoundedLogCapture) ||
      !hasOnlyKeys(capture, ["stdout", "stderr"]))
  ) throw new TypeError("capture must contain bounded stdout and stderr captures");
  const stdout = capture?.stdout ?? new BoundedLogCapture({ headBytes, tailBytes });
  const stderr = capture?.stderr ?? new BoundedLogCapture({ headBytes, tailBytes });
  const child = spawn(executable, args, {
    cwd,
    env,
    detached: process.platform !== "win32",
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout.on("data", (chunk) => stdout.append(chunk));
  child.stderr.on("data", (chunk) => stderr.append(chunk));

  let timedOut = false;
  let cancelled = false;
  let termination;
  const terminate = (reason) => {
    if (termination) return;
    timedOut ||= reason === "timeout";
    cancelled ||= reason === "cancelled";
    termination = terminateProcessGroup(child, killGraceMs);
  };
  const onAbort = () => terminate("cancelled");
  signal?.addEventListener("abort", onAbort, { once: true });
  // Close the throwIfAborted()/listener-registration race after spawning. An
  // already-aborted signal will not dispatch a second abort event.
  if (signal?.aborted) onAbort();
  const timer = setTimeout(() => terminate("timeout"), timeoutMs);
  timer.unref?.();

  try {
    const completed = await new Promise((resolvePromise, rejectPromise) => {
      child.once("error", rejectPromise);
      child.once("close", (code, processSignal) => {
        resolvePromise({ code, signal: processSignal });
      });
    });
    if (termination) await termination;
    else await terminateProcessGroup(child, killGraceMs);
    return Object.freeze({
      exitCode: timedOut ? 124 : cancelled ? 130 : completed.code ?? 1,
      signal: completed.signal,
      timedOut,
      cancelled,
      stdout: stdout.result(),
      stderr: stderr.result(),
    });
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener("abort", onAbort);
  }
}

export class MacCiApi {
  constructor({ origin, token, fetchImpl = fetch }) {
    this.origin = parseOrigin(origin);
    if (typeof token !== "string" || token.trim() === "") {
      throw new RunnerProtocolError("NANOCODEX_CI_MACOS_TOKEN is required");
    }
    this.token = token;
    this.fetchImpl = fetchImpl;
  }

  async claim(worker, host, signal) {
    const value = await this.#json("claim macOS CI job", "/api/ci/macos/claims", {
      method: "POST",
      body: { worker, host },
      signal,
    });
    return parseClaimResponse(value, this.origin);
  }

  async heartbeat(claim, worker, {
    leaseDeadlineMs,
    safetyMarginMs = LEASE_SAFETY_MARGIN_MS,
    signal,
  }) {
    const requestTimeoutMs = leaseRequestTimeout(
      leaseDeadlineMs,
      safetyMarginMs,
      Date.now(),
    );
    const response = await this.#fetch(
      "heartbeat macOS CI claim",
      `/api/ci/macos/claims/${encodeURIComponent(claim)}/heartbeat`,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ worker }),
        signal,
        requestTimeoutMs,
      },
    );
    if (response.status !== 204 && response.headers.get("content-length") !== "0") {
      let value;
      try {
        value = await response.json();
      } catch (error) {
        throw new RunnerProtocolError("macOS CI heartbeat returned invalid JSON", {
          cause: error,
        });
      }
      if (isRecord(value) && value.action === "cancel" && typeof value.reason === "string") {
        return { action: "cancel", reason: value.reason };
      }
      if (!isRecord(value) || value.action !== "continue") {
        throw new RunnerProtocolError("macOS CI heartbeat returned an invalid response");
      }
    } else {
      await response.body?.cancel().catch(() => undefined);
    }
    const leaseExpiresAt = response.headers.get("x-nanocodex-lease-expires-at");
    const renewedDeadlineMs = canonicalTimestamp(
      leaseExpiresAt,
      "macOS CI heartbeat lease deadline",
    );
    if (renewedDeadlineMs <= leaseDeadlineMs) {
      throw new RunnerProtocolError("macOS CI heartbeat did not renew the acknowledged lease");
    }
    return Object.freeze({
      action: "continue",
      leaseExpiresAt,
      leaseDeadlineMs: renewedDeadlineMs,
    });
  }

  async uploadLog(claim, stream, log, signal) {
    if (stream !== "stdout" && stream !== "stderr") {
      throw new RunnerProtocolError(`invalid macOS CI log stream: ${stream}`);
    }
    const response = await this.#fetch(
      `upload macOS CI ${stream}`,
      `/api/ci/macos/claims/${encodeURIComponent(claim)}/logs/${stream}`,
      {
        method: "PUT",
        headers: {
          "content-length": String(log.body.byteLength),
          "content-type": "text/plain; charset=utf-8",
          "x-nanocodex-sha256": log.sha256,
          "x-nanocodex-bytes-observed": String(log.bytesObserved),
          "x-nanocodex-log-truncated": String(log.truncated),
        },
        body: log.body,
        signal,
      },
    );
    const descriptor = await response.json().catch((error) => {
      throw new RunnerProtocolError(`upload macOS CI ${stream} returned invalid JSON`, {
        cause: error,
      });
    });
    return parseUploadedLog(descriptor, log);
  }

  async uploadAsset(claim, asset, signal) {
    const response = await this.#fetch(
      "upload macOS CI release asset",
      `/api/ci/macos/claims/${encodeURIComponent(claim)}/asset`,
      {
        method: "PUT",
        headers: {
          "content-length": String(asset.size),
          "content-type": "application/octet-stream",
          "x-nanocodex-name": asset.name,
          "x-nanocodex-sha256": asset.sha256,
        },
        body: Readable.toWeb(asset.file.createReadStream({ autoClose: false, start: 0 })),
        duplex: "half",
        signal,
      },
    );
    const descriptor = await response.json().catch((error) => {
      throw new RunnerProtocolError("upload macOS CI release asset returned invalid JSON", {
        cause: error,
      });
    });
    return parseUploadedAsset(descriptor, asset);
  }

  async complete(claim, result, signal) {
    return this.#json(
      "complete macOS CI claim",
      `/api/ci/macos/claims/${encodeURIComponent(claim)}/complete`,
      { method: "POST", body: result, signal, allowEmpty: true },
    );
  }

  async #json(operation, path, { body, allowEmpty = false, ...init }) {
    const response = await this.#fetch(operation, path, {
      ...init,
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    if (allowEmpty && (response.status === 204 || response.headers.get("content-length") === "0")) {
      await response.body?.cancel().catch(() => undefined);
      return null;
    }
    try {
      return await response.json();
    } catch (error) {
      throw new RunnerProtocolError(`${operation} returned invalid JSON`, { cause: error });
    }
  }

  async #fetch(operation, path, init) {
    const { requestTimeoutMs = REQUEST_TIMEOUT_MS, ...fetchInit } = init;
    if (!Number.isSafeInteger(requestTimeoutMs) || requestTimeoutMs <= 0) {
      throw new RunnerProtocolError(`${operation} has no safe request-time budget`);
    }
    const headers = new Headers(fetchInit.headers);
    headers.set("authorization", `Bearer ${this.token}`);
    headers.set("accept", "application/json");
    const signal = requestSignal(fetchInit.signal, requestTimeoutMs);
    let response;
    try {
      response = await this.fetchImpl(new URL(path, `${this.origin}/`), {
        ...fetchInit,
        headers,
        redirect: "error",
        signal,
      });
    } catch (error) {
      throw new Error(`${operation} failed: ${error instanceof Error ? error.message : error}`, {
        cause: error,
      });
    }
    if (!response.ok) {
      const detail = await boundedResponseText(response, ERROR_RESPONSE_BYTES);
      throw new RunnerHttpError(operation, response.status, detail);
    }
    return response;
  }
}

export async function runClaim({
  api,
  response,
  worker,
  host,
  runtime,
  signal,
  heartbeatMs = DEFAULT_HEARTBEAT_MS,
  headBytes = DEFAULT_HEAD_BYTES,
  tailBytes = DEFAULT_TAIL_BYTES,
  leaseSafetyMarginMs = LEASE_SAFETY_MARGIN_MS,
  processKillGraceMs = LEASE_PROCESS_KILL_GRACE_MS,
}) {
  if (
    !Number.isSafeInteger(leaseSafetyMarginMs) || leaseSafetyMarginMs <= 0 ||
    !Number.isSafeInteger(processKillGraceMs) || processKillGraceMs < 0 ||
    processKillGraceMs >= leaseSafetyMarginMs
  ) {
    throw new RunnerProtocolError(
      "claim process kill grace must fit strictly inside the lease safety margin",
    );
  }
  const task = resolveTask(
    response.job.task,
    response.job.release,
    response.job.head,
    response.job.publishedAt,
  );
  const sandboxRuntime = runtime ?? await resolveSandboxRuntime();
  if (host.platform !== "darwin" || host.arch !== "arm64") {
    throw new RunnerProtocolError("macOS CI claims may run only on darwin/arm64");
  }
  const startedAt = Date.now();
  const directory = await realpath(await mkdtemp(join(tmpdir(), "nanocodex-ci-macos-")));
  const sourceArchive = join(directory, "source.tar.gz");
  const vendorArchive = join(directory, "cargo-vendor.tar.gz");
  const workspace = join(directory, "workspace");
  const homeDirectory = join(directory, "home");
  const cargoHome = join(directory, "cargo-home");
  const temporaryDirectory = join(directory, "tmp");
  // Keep the reusable profile outside the checkout-writable job subtree so a
  // test/build script cannot relax policy before the next native phase.
  const taskProfilePath = join(
    dirname(directory),
    `.nanocodex-ci-macos-sandbox-${randomUUID()}.sb`,
  );
  const jobAbort = new AbortController();
  const unlinkAbort = forwardAbort(signal, jobAbort);
  let cancellationReason;
  const stopHeartbeat = new AbortController();
  const heartbeat = heartbeatLoop({
    api,
    claim: response.claim,
    worker,
    leaseExpiresAt: response.leaseExpiresAt,
    intervalMs: heartbeatMs,
    safetyMarginMs: leaseSafetyMarginMs,
    signal: stopHeartbeat.signal,
    onCancel(reason) {
      cancellationReason = reason;
      jobAbort.abort(new Error(`macOS CI claim cancelled: ${reason}`));
    },
  });
  let execution;
  let uploadedLogs;
  let localAsset;
  let completionAttempted = false;
  const processCapture = Object.freeze({
    stdout: new BoundedLogCapture({ headBytes, tailBytes }),
    stderr: new BoundedLogCapture({ headBytes, tailBytes }),
  });

  try {
    await Promise.all([
      mkdir(workspace, { mode: 0o700 }),
      mkdir(homeDirectory, { mode: 0o700 }),
      mkdir(cargoHome, { mode: 0o700 }),
      mkdir(temporaryDirectory, { mode: 0o700 }),
    ]);
    await writeSandboxProfile(
      taskProfilePath,
      createSandboxProfile({
        jobDirectory: directory,
        runtime: sandboxRuntime,
        networkAccess: task.networkAccess !== false,
      }),
    );
    const downloads = await Promise.allSettled([
      downloadVerifiedArchive({
        ...response.job.source,
        destination: sourceArchive,
        label: "source archive",
        maximumBytes: MAX_SOURCE_ARCHIVE_BYTES,
        signal: jobAbort.signal,
      }),
      downloadVerifiedArchive({
        ...response.job.cargoVendor,
        destination: vendorArchive,
        label: "Cargo vendor archive",
        maximumBytes: MAX_CARGO_VENDOR_ARCHIVE_BYTES,
        signal: jobAbort.signal,
      }),
    ]);
    const failedDownload = downloads.find(({ status }) => status === "rejected");
    if (failedDownload?.status === "rejected") throw failedDownload.reason;
    await Promise.all([chmod(sourceArchive, 0o400), chmod(vendorArchive, 0o400)]);
    const extractionEnv = extractionEnvironment({ homeDirectory, temporaryDirectory });
    await extractVerifiedArchive({
      archive: sourceArchive,
      destination: workspace,
      kind: "source",
      sourcePrefix: `nanocodex-${response.job.head}`,
      maximumExpandedBytes: MAX_SOURCE_TAR_BYTES,
      stripComponents: 1,
      env: extractionEnv,
      signal: jobAbort.signal,
    });
    await extractVerifiedArchive({
      archive: vendorArchive,
      destination: cargoHome,
      kind: "cargo-vendor",
      maximumExpandedBytes: MAX_CARGO_VENDOR_TAR_BYTES,
      stripComponents: 0,
      env: extractionEnv,
      signal: jobAbort.signal,
    });
    const checkout = workspace;
    await verifyCargoLockBlob(checkout, response.job.cargoVendor.url);
    await installCargoVendor(cargoHome);
    const env = sandboxEnvironment({
      jobDirectory: directory,
      homeDirectory,
      cargoHome,
      temporaryDirectory,
      workspace,
      runtime: sandboxRuntime,
      taskEnvironment: task.environment,
      cargoOffline: task.cargoOffline === true,
    });
    if (task.testArguments) {
      execution = await runCapturedProcess({
        executable: sandboxRuntime.sandboxExec,
        arguments: [
          "-f",
          taskProfilePath,
          sandboxRuntime.cargo,
          ...task.testArguments,
        ],
        cwd: checkout,
        env,
        signal: jobAbort.signal,
        timeoutMs: task.testTimeoutMs,
        killGraceMs: processKillGraceMs,
        headBytes,
        tailBytes,
        capture: processCapture,
      });
    }
    if (execution == null || execution.exitCode === 0) {
      execution = await runCapturedProcess({
        executable: sandboxRuntime.sandboxExec,
        arguments: [
          "-f",
          taskProfilePath,
          sandboxRuntime.cargo,
          ...task.arguments,
        ],
        cwd: checkout,
        env,
        signal: jobAbort.signal,
        timeoutMs: task.timeoutMs,
        killGraceMs: processKillGraceMs,
        headBytes,
        tailBytes,
        capture: processCapture,
      });
    }

    jobAbort.signal.throwIfAborted();
    localAsset = execution.exitCode === 0 && task.asset
      ? await assetDescriptor(checkout, task.asset.path)
      : undefined;
    const [stdout, stderr] = await Promise.all([
      api.uploadLog(response.claim, "stdout", execution.stdout, jobAbort.signal),
      api.uploadLog(response.claim, "stderr", execution.stderr, jobAbort.signal),
    ]);
    uploadedLogs = { stdout, stderr };
    const asset = localAsset
      ? await api.uploadAsset(response.claim, localAsset, jobAbort.signal)
      : undefined;
    const result = {
      outcome: execution.exitCode === 0 ? "success" : "failure",
      exitCode: execution.exitCode,
      durationMs: Date.now() - startedAt,
      logs: uploadedLogs,
      ...(asset ? { asset } : {}),
      ...(execution.timedOut ? { error: "task timed out" } : {}),
      ...(execution.cancelled || cancellationReason ? {
        error: `task cancelled: ${cancellationReason ?? "runner shutdown"}`,
      } : {}),
    };
    completionAttempted = true;
    await api.complete(response.claim, result, jobAbort.signal);
    return result;
  } catch (error) {
    if (execution == null) {
      const stderr = new BoundedLogCapture({ headBytes, tailBytes });
      stderr.append(`${error instanceof Error ? error.stack ?? error.message : String(error)}\n`);
      execution = {
        exitCode: 1,
        timedOut: false,
        cancelled: false,
        stdout: new BoundedLogCapture({ headBytes, tailBytes }).result(),
        stderr: stderr.result(),
      };
    }
    if (!completionAttempted && !jobAbort.signal.aborted) {
      uploadedLogs ??= await Promise.all([
        api.uploadLog(response.claim, "stdout", execution.stdout, jobAbort.signal),
        api.uploadLog(response.claim, "stderr", execution.stderr, jobAbort.signal),
      ]).then(([stdout, stderr]) => ({ stdout, stderr })).catch(() => undefined);
      if (uploadedLogs) {
        completionAttempted = true;
        await api.complete(response.claim, {
        outcome: "failure",
        exitCode: execution.exitCode === 0 ? 1 : execution.exitCode,
        durationMs: Date.now() - startedAt,
        logs: uploadedLogs,
        error: error instanceof Error ? error.message.slice(0, 2_000) : String(error).slice(0, 2_000),
        }, jobAbort.signal).catch(() => undefined);
      }
    }
    throw error;
  } finally {
    stopHeartbeat.abort();
    await heartbeat.catch(() => undefined);
    unlinkAbort();
    await localAsset?.file.close().catch(() => undefined);
    await Promise.all([
      rm(taskProfilePath, { force: true }),
      rm(directory, { recursive: true, force: true }),
    ]);
  }
}

export async function runRunner({
  api,
  worker,
  runtime,
  once = false,
  signal,
  pollMs = DEFAULT_POLL_MS,
  heartbeatMs = DEFAULT_HEARTBEAT_MS,
  headBytes = DEFAULT_HEAD_BYTES,
  tailBytes = DEFAULT_TAIL_BYTES,
  host = hostMetadata(),
}) {
  assertIdentifier(worker, "runner ID");
  const sandboxRuntime = runtime ?? await resolveSandboxRuntime();
  for (;;) {
    signal?.throwIfAborted();
    const claimed = await api.claim(worker, host, signal);
    if (claimed.action === "idle") {
      if (once) return { action: "idle" };
      await delay(pollMs, signal);
      continue;
    }
    const result = await runClaim({
      api,
      response: claimed,
      worker,
      host,
      runtime: sandboxRuntime,
      signal,
      heartbeatMs,
      headBytes,
      tailBytes,
    });
    if (once) return { action: "complete", result };
  }
}

export async function main(argv = process.argv.slice(2), env = process.env) {
  const unknown = argv.filter((argument) => argument !== "--once");
  if (unknown.length > 0 || argv.filter((argument) => argument === "--once").length > 1) {
    throw new RunnerProtocolError("usage: ci-macos-runner.mjs [--once]");
  }
  const origin = requiredEnvironment(env, "NANOCODEX_CI_ORIGIN");
  const token = requiredEnvironment(env, "NANOCODEX_CI_MACOS_TOKEN");
  const worker = requiredEnvironment(env, "NANOCODEX_CI_MACOS_RUNNER_ID");
  assertIdentifier(worker, "NANOCODEX_CI_MACOS_RUNNER_ID");
  const host = hostMetadata();
  if (host.platform !== "darwin" || host.arch !== "arm64") {
    throw new RunnerProtocolError(
      `macOS CI runner requires darwin/arm64, received ${host.platform}/${host.arch}`,
    );
  }
  const runtime = await resolveSandboxRuntime({ env });
  const api = new MacCiApi({ origin, token });
  const shutdown = new AbortController();
  const stop = (name) => shutdown.abort(new Error(`received ${name}`));
  const onInterrupt = () => stop("SIGINT");
  const onTerminate = () => stop("SIGTERM");
  process.once("SIGINT", onInterrupt);
  process.once("SIGTERM", onTerminate);
  try {
    const outcome = await runRunner({
      api,
      worker,
      runtime,
      once: argv.includes("--once"),
      signal: shutdown.signal,
      pollMs: environmentInteger(env, "NANOCODEX_CI_MACOS_POLL_MS", DEFAULT_POLL_MS, 1_000, 5 * 60_000),
      heartbeatMs: environmentInteger(env, "NANOCODEX_CI_MACOS_HEARTBEAT_MS", DEFAULT_HEARTBEAT_MS, 5_000, 60_000),
      headBytes: environmentInteger(env, "NANOCODEX_CI_MACOS_LOG_HEAD_BYTES", DEFAULT_HEAD_BYTES, 0, MAX_LOG_PART_BYTES),
      tailBytes: environmentInteger(env, "NANOCODEX_CI_MACOS_LOG_TAIL_BYTES", DEFAULT_TAIL_BYTES, 0, MAX_LOG_PART_BYTES),
      host,
    });
    if (outcome.action === "complete" && outcome.result.outcome !== "success") {
      process.exitCode = 1;
    }
  } finally {
    process.removeListener("SIGINT", onInterrupt);
    process.removeListener("SIGTERM", onTerminate);
  }
}

function parseClaimResponse(value, origin) {
  if (isRecord(value) && value.action === "idle") return Object.freeze({ action: "idle" });
  if (
    !isRecord(value) || value.action !== "run" ||
    typeof value.claim !== "string" || !IDENTIFIER.test(value.claim) ||
    !isCanonicalTimestamp(value.leaseExpiresAt) ||
    !isRecord(value.job) || typeof value.job.id !== "string" || !IDENTIFIER.test(value.job.id) ||
    typeof value.job.head !== "string" || !SHA1.test(value.job.head) ||
    typeof value.job.workflowId !== "string" || value.job.workflowId.length === 0 || value.job.workflowId.length > 200 ||
    typeof value.job.task !== "string" || !isRecord(value.job.source) ||
    !isRecord(value.job.cargoVendor)
  ) {
    throw new RunnerProtocolError("macOS CI claim returned an invalid job");
  }
  resolveTask(
    value.job.task,
    value.job.release,
    value.job.head,
    value.job.publishedAt,
  );
  const jobKeys = ["id", "head", "workflowId", "task", "source", "cargoVendor"];
  if (value.job.task === "release-build") jobKeys.push("release");
  if (value.job.task === "native-build") jobKeys.push("publishedAt");
  if (!hasOnlyKeys(value.job, jobKeys)) {
    throw new RunnerProtocolError("macOS CI claim supplied unsupported job policy");
  }
  if (
    value.job.task === "native-build" &&
    value.job.id !== `macos-native-build-${value.job.head}`
  ) throw new RunnerProtocolError("macOS CI native-build job ID is not canonical");
  const sourceUrl = new URL(value.job.source.url, `${origin}/`);
  const expectedOrigin = new URL(origin);
  if (
    sourceUrl.origin !== expectedOrigin.origin ||
    sourceUrl.username || sourceUrl.password ||
    sourceUrl.pathname !== `/api/ci/source/${value.job.head}/archive` ||
    sourceUrl.search || sourceUrl.hash
  ) {
    throw new RunnerProtocolError("macOS CI source archive URL is not the canonical commit archive");
  }
  assertArchiveDescriptor({
    url: sourceUrl.href,
    size: value.job.source.size,
    sha256: value.job.source.sha256,
  }, MAX_SOURCE_ARCHIVE_BYTES, "source archive");
  if (!hasOnlyKeys(value.job.source, ["url", "size", "sha256"])) {
    throw new RunnerProtocolError("macOS CI source archive descriptor has unsupported fields");
  }
  const cargoVendorUrl = new URL(value.job.cargoVendor.url, `${origin}/`);
  const cargoVendorIdentity = cargoVendorUrl.pathname.match(
    /^\/api\/ci\/cargo-vendor\/([a-f0-9]{40})\/([a-f0-9]{64})\/bundle\.tar\.gz$/,
  );
  if (
    cargoVendorUrl.origin !== expectedOrigin.origin ||
    cargoVendorUrl.username || cargoVendorUrl.password ||
    cargoVendorIdentity?.[2] !== value.job.cargoVendor.sha256 ||
    cargoVendorUrl.search || cargoVendorUrl.hash
  ) {
    throw new RunnerProtocolError("macOS CI Cargo vendor URL is not canonical");
  }
  assertArchiveDescriptor({
    url: cargoVendorUrl.href,
    size: value.job.cargoVendor.size,
    sha256: value.job.cargoVendor.sha256,
  }, MAX_CARGO_VENDOR_ARCHIVE_BYTES, "Cargo vendor archive");
  if (!hasOnlyKeys(value.job.cargoVendor, ["url", "size", "sha256"])) {
    throw new RunnerProtocolError("macOS CI Cargo vendor descriptor has unsupported fields");
  }
  return Object.freeze({
    action: "run",
    claim: value.claim,
    leaseExpiresAt: value.leaseExpiresAt,
    job: Object.freeze({
      id: value.job.id,
      head: value.job.head,
      workflowId: value.job.workflowId,
      task: value.job.task,
      ...(value.job.release ? { release: Object.freeze({ ...value.job.release }) } : {}),
      ...(value.job.publishedAt ? { publishedAt: value.job.publishedAt } : {}),
      source: Object.freeze({
        url: sourceUrl.href,
        size: value.job.source.size,
        sha256: value.job.source.sha256,
      }),
      cargoVendor: Object.freeze({
        url: cargoVendorUrl.href,
        size: value.job.cargoVendor.size,
        sha256: value.job.cargoVendor.sha256,
      }),
    }),
  });
}

function assertArchiveDescriptor({ url, size, sha256: digest }, maximumBytes, label) {
  if (
    typeof url !== "string" || !/^https?:\/\//.test(url) ||
    !Number.isSafeInteger(maximumBytes) || maximumBytes <= 0 ||
    !Number.isSafeInteger(size) || size <= 0 || size > maximumBytes ||
    typeof digest !== "string" || !SHA256.test(digest)
  ) {
    throw new RunnerProtocolError(`invalid macOS CI ${label} descriptor`);
  }
}

export async function preflightTarArchive({
  archive,
  kind,
  sourcePrefix,
  maximumExpandedBytes,
  signal,
}) {
  if (kind !== "source" && kind !== "cargo-vendor") {
    throw new TypeError("archive kind must be source or cargo-vendor");
  }
  if (kind === "source" && !/^nanocodex-[a-f0-9]{40}$/.test(sourcePrefix ?? "")) {
    throw new RunnerProtocolError("source archive prefix is invalid");
  }
  if (!Number.isSafeInteger(maximumExpandedBytes) || maximumExpandedBytes <= 0) {
    throw new TypeError("maximumExpandedBytes must be a positive integer");
  }
  signal?.throwIfAborted();
  const inspection = new TarInspection({ kind, sourcePrefix, maximumExpandedBytes });
  const compressed = createReadStream(archive, { signal });
  const gunzip = createGunzip();
  compressed.once("error", (error) => gunzip.destroy(error));
  compressed.pipe(gunzip);
  try {
    for await (const chunk of gunzip) inspection.append(chunk);
    return inspection.finish();
  } catch (error) {
    compressed.destroy();
    gunzip.destroy();
    throw error;
  }
}

async function extractVerifiedArchive({
  archive,
  destination,
  kind,
  sourcePrefix,
  maximumExpandedBytes,
  stripComponents,
  env,
  signal,
}) {
  await preflightTarArchive({
    archive,
    kind,
    sourcePrefix,
    maximumExpandedBytes,
    signal,
  });
  const extraction = await runCapturedProcess({
    executable: "/usr/bin/tar",
    arguments: [
      "-xzf",
      archive,
      "-C",
      destination,
      `--strip-components=${stripComponents}`,
      "--no-same-owner",
      "--no-same-permissions",
      "-k",
    ],
    cwd: dirname(destination),
    env,
    signal,
    timeoutMs: 5 * 60 * 1_000,
    headBytes: 256 * 1024,
    tailBytes: 256 * 1024,
  });
  if (extraction.exitCode !== 0) {
    const detail = extraction.stderr.body.toString("utf8").slice(-2_000);
    throw new Error(
      `${kind} extraction failed with exit ${extraction.exitCode}${detail ? `: ${detail}` : ""}`,
    );
  }
}

class TarInspection {
  #header = Buffer.alloc(512);
  #headerBytes = 0;
  #dataRemaining = 0;
  #paddingRemaining = 0;
  #paxType;
  #paxChunks = [];
  #paxBytes = 0;
  #pendingPax;
  #zeroBlocks = 0;
  #ended = false;
  #expandedBytes = 0;
  #entries = 0;
  #headers = 0;
  #pathComponents = 0;
  #metadataBytes = 0;
  #paths = new Map();
  #requiredDirectories = new Set();
  #sawSourceRoot = false;
  #sawCargoConfig = false;
  #sawVendorRoot = false;

  constructor({ kind, sourcePrefix, maximumExpandedBytes }) {
    this.kind = kind;
    this.sourcePrefix = sourcePrefix;
    this.maximumExpandedBytes = maximumExpandedBytes;
  }

  append(value) {
    const chunk = Buffer.isBuffer(value) ? value : Buffer.from(value);
    this.#expandedBytes += chunk.byteLength;
    if (this.#expandedBytes > this.maximumExpandedBytes) {
      throw new RunnerProtocolError(
        `${this.kind} archive expands beyond ${this.maximumExpandedBytes} bytes`,
      );
    }
    let offset = 0;
    while (offset < chunk.byteLength) {
      if (this.#ended) {
        if (chunk.subarray(offset).some((byte) => byte !== 0)) {
          throw new RunnerProtocolError(`${this.kind} archive has data after its end marker`);
        }
        return;
      }
      if (this.#dataRemaining > 0) {
        const length = Math.min(this.#dataRemaining, chunk.byteLength - offset);
        if (this.#paxType) {
          this.#paxChunks.push(Buffer.from(chunk.subarray(offset, offset + length)));
          this.#paxBytes += length;
        }
        this.#dataRemaining -= length;
        offset += length;
        if (this.#dataRemaining === 0) this.#finishEntryData();
        continue;
      }
      if (this.#paddingRemaining > 0) {
        const length = Math.min(this.#paddingRemaining, chunk.byteLength - offset);
        this.#paddingRemaining -= length;
        offset += length;
        continue;
      }
      const length = Math.min(512 - this.#headerBytes, chunk.byteLength - offset);
      chunk.copy(this.#header, this.#headerBytes, offset, offset + length);
      this.#headerBytes += length;
      offset += length;
      if (this.#headerBytes === 512) {
        this.#consumeHeader(this.#header);
        this.#header = Buffer.alloc(512);
        this.#headerBytes = 0;
      }
    }
  }

  finish() {
    if (
      !this.#ended || this.#headerBytes !== 0 || this.#dataRemaining !== 0 ||
      this.#paddingRemaining !== 0 || this.#paxType
    ) throw new RunnerProtocolError(`${this.kind} archive is truncated`);
    if (this.kind === "source" && !this.#sawSourceRoot) {
      throw new RunnerProtocolError("source archive is missing its canonical root directory");
    }
    if (this.kind === "cargo-vendor" && (!this.#sawCargoConfig || !this.#sawVendorRoot)) {
      throw new RunnerProtocolError("Cargo vendor archive is missing config.toml or vendor/");
    }
    return Object.freeze({ entries: this.#entries, expandedBytes: this.#expandedBytes });
  }

  #consumeHeader(header) {
    if (header.every((byte) => byte === 0)) {
      this.#zeroBlocks += 1;
      if (this.#zeroBlocks === 2) this.#ended = true;
      return;
    }
    if (this.#zeroBlocks !== 0) {
      throw new RunnerProtocolError(`${this.kind} archive has a partial end marker`);
    }
    this.#headers += 1;
    if (this.#headers > MAX_TAR_HEADERS) {
      throw new RunnerProtocolError(`${this.kind} archive has too many metadata headers`);
    }
    verifyTarChecksum(header, this.kind);
    const name = tarHeaderPath(header);
    const linkPath = readTarString(header, 157, 100, "link path");
    const size = readTarOctal(header, 124, 12, "entry size");
    if (size > this.maximumExpandedBytes) {
      throw new RunnerProtocolError(`${this.kind} archive entry is too large`);
    }
    const typeByte = header[156];
    const type = typeByte === 0 ? "0" : String.fromCharCode(typeByte);
    if (type === "x" || type === "g") {
      assertSafeArchiveText(name, "PAX header path");
      if (size > MAX_PAX_BYTES) {
        throw new RunnerProtocolError(`${this.kind} archive PAX header is too large`);
      }
      if (this.#pendingPax && type === "x") {
        throw new RunnerProtocolError(`${this.kind} archive has consecutive local PAX headers`);
      }
      this.#paxType = type;
      this.#paxChunks = [];
      this.#paxBytes = 0;
      this.#startEntryData(size);
      if (size === 0) this.#finishEntryData();
      return;
    }
    if (!new Set(["0", "5", "2"]).has(type)) {
      throw new RunnerProtocolError(`${this.kind} archive contains unsupported entry type ${type}`);
    }
    const pax = this.#pendingPax;
    this.#pendingPax = undefined;
    const path = pax?.path ?? name;
    const resolvedLink = pax?.linkpath ?? linkPath;
    if (pax && Object.keys(pax).some((key) => key !== "path" && key !== "linkpath")) {
      throw new RunnerProtocolError(`${this.kind} archive has unsupported local PAX metadata`);
    }
    const entryType = type === "0" ? "file" : type === "5" ? "directory" : "symlink";
    if (entryType !== "file" && size !== 0) {
      throw new RunnerProtocolError(`${this.kind} archive ${entryType} has file data`);
    }
    this.#validateEntry(path, entryType, resolvedLink);
    this.#entries += 1;
    if (this.#entries > MAX_TAR_ENTRIES) {
      throw new RunnerProtocolError(`${this.kind} archive has too many entries`);
    }
    this.#startEntryData(size);
  }

  #startEntryData(size) {
    this.#dataRemaining = size;
    this.#paddingRemaining = (512 - (size % 512)) % 512;
  }

  #finishEntryData() {
    if (!this.#paxType) return;
    if (this.#paxBytes > MAX_PAX_BYTES) {
      throw new RunnerProtocolError(`${this.kind} archive PAX body is too large`);
    }
    const values = parsePax(Buffer.concat(this.#paxChunks, this.#paxBytes), this.kind);
    if (this.#paxType === "g") {
      if (Object.keys(values).some((key) => key !== "comment")) {
        throw new RunnerProtocolError(`${this.kind} archive has unsupported global PAX metadata`);
      }
    } else {
      this.#pendingPax = values;
    }
    this.#paxType = undefined;
    this.#paxChunks = [];
    this.#paxBytes = 0;
  }

  #validateEntry(rawPath, type, rawLink) {
    const path = safeArchivePath(rawPath, type);
    const parts = path === "" ? [] : path.split("/");
    let relative;
    if (this.kind === "source") {
      if (parts[0] !== this.sourcePrefix) {
        throw new RunnerProtocolError("source archive entry escaped its canonical prefix");
      }
      relative = parts.slice(1).join("/");
      if (relative === "") {
        if (type !== "directory") {
          throw new RunnerProtocolError("source archive root is not a directory");
        }
        this.#sawSourceRoot = true;
      }
    } else {
      relative = path;
      const root = parts[0];
      if (root === "config.toml") {
        if (parts.length !== 1 || type !== "file") {
          throw new RunnerProtocolError("Cargo vendor config.toml must be a regular root file");
        }
        this.#sawCargoConfig = true;
      } else if (root === "vendor") {
        if (parts.length === 1) {
          if (type !== "directory") {
            throw new RunnerProtocolError("Cargo vendor root is not a directory");
          }
          this.#sawVendorRoot = true;
        }
      } else if (root !== "init") {
        throw new RunnerProtocolError(`Cargo vendor archive has unexpected root ${root}`);
      }
    }
    if (this.#paths.has(relative)) {
      throw new RunnerProtocolError(`${this.kind} archive repeats path ${relative || "."}`);
    }
    if (type !== "directory" && this.#requiredDirectories.has(relative)) {
      throw new RunnerProtocolError(`${this.kind} archive replaces a parent directory`);
    }
    const relativeParts = relative === "" ? [] : relative.split("/");
    this.#pathComponents += relativeParts.length;
    if (this.#pathComponents > MAX_TAR_PATH_COMPONENTS) {
      throw new RunnerProtocolError(`${this.kind} archive has too many path components`);
    }
    this.#retainMetadata(relative);
    for (let index = 0; index < relativeParts.length; index++) {
      const parent = relativeParts.slice(0, index).join("/");
      const parentType = this.#paths.get(parent);
      if (parentType && parentType !== "directory") {
        throw new RunnerProtocolError(`${this.kind} archive traverses a non-directory entry`);
      }
      if (!this.#requiredDirectories.has(parent)) {
        this.#retainMetadata(parent);
        this.#requiredDirectories.add(parent);
        if (this.#requiredDirectories.size > MAX_TAR_REQUIRED_DIRECTORIES) {
          throw new RunnerProtocolError(`${this.kind} archive requires too many directories`);
        }
      }
    }
    this.#paths.set(relative, type);
    if (type === "symlink") validateArchiveSymlink(relative, rawLink, this.kind);
  }

  #retainMetadata(value) {
    this.#metadataBytes += Buffer.byteLength(value);
    if (this.#metadataBytes > MAX_TAR_METADATA_BYTES) {
      throw new RunnerProtocolError(`${this.kind} archive has too much path metadata`);
    }
  }
}

function verifyTarChecksum(header, kind) {
  const expected = readTarOctal(header, 148, 8, "header checksum");
  let actual = 0;
  for (let index = 0; index < header.byteLength; index++) {
    actual += index >= 148 && index < 156 ? 0x20 : header[index];
  }
  if (expected !== actual) {
    throw new RunnerProtocolError(`${kind} archive has an invalid tar checksum`);
  }
}

function tarHeaderPath(header) {
  const name = readTarString(header, 0, 100, "entry path");
  const prefix = readTarString(header, 345, 155, "entry prefix");
  return prefix ? `${prefix}/${name}` : name;
}

function readTarString(header, offset, length, label) {
  const field = header.subarray(offset, offset + length);
  const end = field.indexOf(0);
  const bytes = end === -1 ? field : field.subarray(0, end);
  if (end !== -1 && field.subarray(end).some((byte) => byte !== 0)) {
    throw new RunnerProtocolError(`tar ${label} has data after NUL`);
  }
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch (error) {
    throw new RunnerProtocolError(`tar ${label} is not UTF-8`, { cause: error });
  }
}

function readTarOctal(header, offset, length, label) {
  const field = header.subarray(offset, offset + length);
  if ((field[0] & 0x80) !== 0) {
    throw new RunnerProtocolError(`tar ${label} uses unsupported base-256 encoding`);
  }
  const value = field.toString("ascii").replaceAll("\0", "").trim();
  if (!/^[0-7]+$/.test(value)) throw new RunnerProtocolError(`tar ${label} is invalid`);
  const parsed = Number.parseInt(value, 8);
  if (!Number.isSafeInteger(parsed) || parsed < 0) {
    throw new RunnerProtocolError(`tar ${label} is out of range`);
  }
  return parsed;
}

function parsePax(body, kind) {
  const values = {};
  let offset = 0;
  while (offset < body.byteLength) {
    const separator = body.indexOf(0x20, offset);
    if (separator < 0) throw new RunnerProtocolError(`${kind} archive has invalid PAX data`);
    const lengthText = body.subarray(offset, separator).toString("ascii");
    if (!/^[1-9][0-9]*$/.test(lengthText)) {
      throw new RunnerProtocolError(`${kind} archive has invalid PAX record length`);
    }
    const length = Number(lengthText);
    const end = offset + length;
    if (!Number.isSafeInteger(length) || end > body.byteLength || body[end - 1] !== 0x0a) {
      throw new RunnerProtocolError(`${kind} archive has truncated PAX data`);
    }
    const record = body.subarray(separator + 1, end - 1);
    const equals = record.indexOf(0x3d);
    if (equals <= 0) throw new RunnerProtocolError(`${kind} archive has invalid PAX record`);
    let key;
    let value;
    try {
      const decoder = new TextDecoder("utf-8", { fatal: true });
      key = decoder.decode(record.subarray(0, equals));
      value = decoder.decode(record.subarray(equals + 1));
    } catch (error) {
      throw new RunnerProtocolError(`${kind} archive has non-UTF-8 PAX data`, { cause: error });
    }
    if (!/^[A-Za-z][A-Za-z0-9._-]*$/.test(key) || Object.hasOwn(values, key)) {
      throw new RunnerProtocolError(`${kind} archive has invalid PAX key`);
    }
    values[key] = value;
    offset = end;
  }
  return values;
}

function safeArchivePath(value, type) {
  assertSafeArchiveText(value, "entry path");
  const path = type === "directory" && value.endsWith("/") ? value.slice(0, -1) : value;
  if (path.startsWith("/") || path.endsWith("/") || path.includes("\\")) {
    throw new RunnerProtocolError("archive entry path is not canonical");
  }
  const parts = path.split("/");
  if (parts.some((part) => part === "" || part === "." || part === "..")) {
    throw new RunnerProtocolError("archive entry path escapes its destination");
  }
  return path;
}

function assertSafeArchiveText(value, label) {
  if (
    typeof value !== "string" || value.length === 0 ||
    Buffer.byteLength(value) > MAX_TAR_PATH_BYTES || /[\0\x00-\x1f\x7f]/.test(value)
  ) throw new RunnerProtocolError(`archive ${label} is unsafe`);
}

function validateArchiveSymlink(path, link, kind) {
  assertSafeArchiveText(link, "symlink target");
  if (link.startsWith("/") || link.includes("\\")) {
    throw new RunnerProtocolError(`${kind} archive symlink is absolute`);
  }
  const target = posix.normalize(posix.join(posix.dirname(path), link));
  if (target === ".." || target.startsWith("../") || posix.isAbsolute(target)) {
    throw new RunnerProtocolError(`${kind} archive symlink escapes its destination`);
  }
}

async function verifyCargoLockBlob(checkout, vendorUrl) {
  const match = new URL(vendorUrl).pathname.match(
    /^\/api\/ci\/cargo-vendor\/([a-f0-9]{40})\/[a-f0-9]{64}\/bundle\.tar\.gz$/,
  );
  if (!match) throw new RunnerProtocolError("Cargo vendor URL has no lockfile identity");
  const path = join(checkout, "Cargo.lock");
  const metadata = await lstat(path);
  if (!metadata.isFile() || metadata.isSymbolicLink() || metadata.size > 16 * 1024 * 1024) {
    throw new RunnerProtocolError("source archive has no safe Cargo.lock");
  }
  const digest = createHash("sha1");
  digest.update(`blob ${metadata.size}\0`);
  for await (const chunk of createReadStream(path)) digest.update(chunk);
  if (digest.digest("hex") !== match[1]) {
    throw new RunnerProtocolError("Cargo vendor descriptor does not match source Cargo.lock");
  }
}

async function installCargoVendor(cargoHome) {
  const configPath = join(cargoHome, "config.toml");
  const vendorPath = join(cargoHome, "vendor");
  const [configMetadata, vendorMetadata] = await Promise.all([
    lstat(configPath),
    lstat(vendorPath),
  ]);
  if (
    !configMetadata.isFile() || configMetadata.isSymbolicLink() ||
    configMetadata.size <= 0 || configMetadata.size > MAX_CARGO_CONFIG_BYTES ||
    !vendorMetadata.isDirectory() || vendorMetadata.isSymbolicLink()
  ) throw new RunnerProtocolError("Cargo vendor archive has an invalid installed shape");
  const config = await readFile(configPath, "utf8");
  const marker = `directory = ${JSON.stringify(CARGO_VENDOR_DIRECTORY)}`;
  if (config.split(marker).length !== 2) {
    throw new RunnerProtocolError("Cargo vendor config has an unexpected vendor directory");
  }
  const installed = config.replace(marker, `directory = ${JSON.stringify(vendorPath)}`);
  const file = await open(configPath, "r+");
  try {
    await file.truncate(0);
    await writeAll(file, Buffer.from(installed, "utf8"));
    await file.sync();
  } finally {
    await file.close();
  }
  await chmod(configPath, 0o600);
}

export async function heartbeatLoop({
  api,
  claim,
  worker,
  leaseExpiresAt,
  intervalMs,
  safetyMarginMs = LEASE_SAFETY_MARGIN_MS,
  signal,
  onCancel,
  now = Date.now,
  wait = delay,
  setTimer = setTimeout,
  clearTimer = clearTimeout,
}) {
  if (!Number.isSafeInteger(intervalMs) || intervalMs <= 0) {
    throw new TypeError("heartbeat interval must be a positive integer");
  }
  if (!Number.isSafeInteger(safetyMarginMs) || safetyMarginMs <= 0) {
    throw new TypeError("lease safety margin must be a positive integer");
  }
  if (typeof onCancel !== "function") throw new TypeError("onCancel must be a function");
  let acknowledgedDeadlineMs = canonicalTimestamp(
    leaseExpiresAt,
    "macOS CI acknowledged lease deadline",
  );
  let failures = 0;
  let deadlineTimer;
  let activeRequestAbort;
  let cancelled = false;
  const cancel = (reason) => {
    if (cancelled) return;
    cancelled = true;
    activeRequestAbort?.abort(new Error(reason));
    onCancel(reason);
  };
  const armDeadline = () => {
    if (deadlineTimer !== undefined) clearTimer(deadlineTimer);
    const remainingMs = acknowledgedDeadlineMs - safetyMarginMs - now();
    if (remainingMs <= 0) {
      cancel(`lease safety margin reached for ${leaseExpiresAt}`);
      return false;
    }
    deadlineTimer = setTimer(() => {
      cancel(`lease safety margin reached for ${leaseExpiresAt}`);
    }, remainingMs);
    deadlineTimer?.unref?.();
    return true;
  };
  if (!armDeadline()) return;
  try {
    for (;;) {
      try {
        const untilSafetyMargin = acknowledgedDeadlineMs - safetyMarginMs - now();
        await wait(Math.min(intervalMs, Math.max(1, untilSafetyMargin)), signal);
      } catch (error) {
        if (signal?.aborted) return;
        throw error;
      }
      if (cancelled) return;
      try {
        activeRequestAbort = new AbortController();
        const heartbeatSignal = signal
          ? AbortSignal.any([signal, activeRequestAbort.signal])
          : activeRequestAbort.signal;
        const result = await api.heartbeat(claim, worker, {
          leaseDeadlineMs: acknowledgedDeadlineMs,
          safetyMarginMs,
          signal: heartbeatSignal,
        });
        activeRequestAbort = undefined;
        if (cancelled || signal?.aborted) return;
        failures = 0;
        if (result.action === "cancel") {
          cancel(result.reason);
          return;
        }
        if (
          !isRecord(result) || result.action !== "continue" ||
          !isCanonicalTimestamp(result.leaseExpiresAt) ||
          result.leaseDeadlineMs !== Date.parse(result.leaseExpiresAt) ||
          result.leaseDeadlineMs <= acknowledgedDeadlineMs
        ) {
          cancel("heartbeat returned an invalid renewed lease deadline");
          return;
        }
        leaseExpiresAt = result.leaseExpiresAt;
        acknowledgedDeadlineMs = result.leaseDeadlineMs;
        if (!armDeadline()) return;
      } catch (error) {
        activeRequestAbort = undefined;
        if (signal?.aborted || cancelled) return;
        failures += 1;
        if (error instanceof RunnerHttpError) {
          cancel(`lease rejected with HTTP ${error.status}`);
          return;
        }
        if (error instanceof RunnerProtocolError) {
          cancel(error.message);
          return;
        }
        if (failures >= 3) {
          cancel(`heartbeat failed ${failures} times`);
          return;
        }
      }
    }
  } finally {
    if (deadlineTimer !== undefined) clearTimer(deadlineTimer);
    activeRequestAbort?.abort(new Error("heartbeat loop stopped"));
  }
}

async function terminateProcessGroup(child, graceMs) {
  if (!signalProcessGroup(child, "SIGTERM")) return;
  if (graceMs > 0) await new Promise((resolvePromise) => setTimeout(resolvePromise, graceMs));
  signalProcessGroup(child, "SIGKILL");
}

function signalProcessGroup(child, signal) {
  if (child.pid == null) return false;
  try {
    if (process.platform === "win32") child.kill(signal);
    else process.kill(-child.pid, signal);
    return true;
  } catch (error) {
    if (error?.code === "ESRCH") return false;
    throw error;
  }
}

function extractionEnvironment({ homeDirectory, temporaryDirectory }) {
  return Object.freeze({
    PATH: SYSTEM_PATH,
    HOME: homeDirectory,
    TMPDIR: temporaryDirectory,
    LANG: "en_US.UTF-8",
    LC_ALL: "en_US.UTF-8",
  });
}

async function canonicalDirectory(path, label) {
  assertSafeAbsolutePath(path, label);
  let canonical;
  try {
    canonical = await realpath(path);
  } catch (error) {
    throw new RunnerProtocolError(`${label} is unavailable`, { cause: error });
  }
  assertCanonicalPath(canonical, label);
  const metadata = await stat(canonical);
  if (!metadata.isDirectory()) throw new RunnerProtocolError(`${label} is not a directory`);
  return canonical;
}

async function canonicalExecutable(path, label) {
  assertSafeAbsolutePath(path, label);
  let canonical;
  try {
    canonical = await realpath(path);
    await access(canonical, fsConstants.X_OK);
  } catch (error) {
    throw new RunnerProtocolError(`${label} is unavailable or not executable`, { cause: error });
  }
  assertCanonicalPath(canonical, label);
  const metadata = await stat(canonical);
  if (!metadata.isFile()) throw new RunnerProtocolError(`${label} is not a regular file`);
  return canonical;
}

function assertSafeAbsolutePath(path, label) {
  if (
    typeof path !== "string" || !isAbsolute(path) || path.length > MAX_TAR_PATH_BYTES ||
    /[\0\r\n\x00-\x1f\x7f]/.test(path)
  ) throw new RunnerProtocolError(`${label} is not a safe absolute path`);
}

function assertCanonicalPath(path, label) {
  assertSafeAbsolutePath(path, label);
  if (resolve(path) !== path) throw new RunnerProtocolError(`${label} is not canonical`);
}

function isWithin(root, path) {
  return path === root || path.startsWith(`${root}/`);
}

function sbplString(value) {
  assertCanonicalPath(value, "sandbox path");
  return `"${value.replaceAll("\\", "\\\\").replaceAll('"', '\\"')}"`;
}

async function assetDescriptor(checkout, relativePath) {
  const checkoutRoot = await realpath(checkout);
  const path = resolve(checkoutRoot, relativePath);
  if (!isWithin(checkoutRoot, path)) {
    throw new RunnerProtocolError("release asset path escaped the checkout");
  }
  const parent = await realpath(dirname(path));
  if (!isWithin(checkoutRoot, parent)) {
    throw new RunnerProtocolError("release asset parent escaped the checkout");
  }
  const source = await open(
    join(parent, basename(path)),
    fsConstants.O_RDONLY | fsConstants.O_NOFOLLOW,
  );
  let snapshot;
  try {
    const metadata = await source.stat();
    if (!metadata.isFile() || metadata.nlink !== 1 || metadata.size <= 0) {
      throw new Error(`release asset is missing: ${relativePath}`);
    }
    const snapshotPath = join(dirname(checkoutRoot), `.release-asset-${randomUUID()}`);
    snapshot = await open(snapshotPath, "wx+", 0o600);
    await unlink(snapshotPath);
    const digest = createHash("sha256");
    const buffer = Buffer.alloc(1024 * 1024);
    let offset = 0;
    for (;;) {
      const { bytesRead } = await source.read(buffer, 0, buffer.byteLength, offset);
      if (bytesRead === 0) break;
      offset += bytesRead;
      if (offset > MAX_RELEASE_ASSET_BYTES) {
        throw new RunnerProtocolError("release asset exceeds the bounded upload limit");
      }
      const chunk = buffer.subarray(0, bytesRead);
      digest.update(chunk);
      await writeAll(snapshot, chunk);
    }
    if (offset === 0) throw new Error(`release asset is empty: ${relativePath}`);
    await snapshot.sync();
    await validateThinArm64MachO(snapshot, offset);
    return {
      file: snapshot,
      name: "nanocodex-aarch64-apple-darwin",
      size: offset,
      sha256: digest.digest("hex"),
    };
  } catch (error) {
    await snapshot?.close().catch(() => undefined);
    throw error;
  } finally {
    await source.close();
  }
}

export async function validateThinArm64MachO(file, size) {
  if (
    file == null || typeof file.read !== "function" ||
    !Number.isSafeInteger(size) || size < MACH_HEADER_64_BYTES
  ) throw new RunnerProtocolError("macOS release asset has a truncated Mach-O header");
  const header = Buffer.alloc(MACH_HEADER_64_BYTES);
  await readExactlyAt(file, header, 0, "Mach-O header");
  const rawMagic = header.subarray(0, 4).toString("hex");
  if (["cafebabe", "bebafeca", "cafebabf", "bfbafeca"].includes(rawMagic)) {
    throw new RunnerProtocolError("macOS release asset must not be a fat Mach-O binary");
  }
  if (header.readUInt32LE(0) !== MACHO_64_MAGIC) {
    throw new RunnerProtocolError("macOS release asset is not a thin 64-bit Mach-O binary");
  }
  if (header.readUInt32LE(4) !== MACHO_ARM64_CPU) {
    throw new RunnerProtocolError("macOS release asset is not arm64 Mach-O");
  }
  if (header.readUInt32LE(12) !== MACHO_EXECUTE_FILE_TYPE) {
    throw new RunnerProtocolError("macOS release asset is not a Mach-O executable");
  }
  const commandCount = header.readUInt32LE(16);
  const commandBytes = header.readUInt32LE(20);
  if (
    commandCount === 0 || commandBytes === 0 ||
    commandBytes > MAX_MACH_LOAD_COMMAND_BYTES ||
    commandCount > Math.floor(commandBytes / 8) ||
    commandBytes > size - MACH_HEADER_64_BYTES
  ) throw new RunnerProtocolError("macOS release asset has truncated Mach-O load commands");
  const commands = Buffer.alloc(commandBytes);
  await readExactlyAt(file, commands, MACH_HEADER_64_BYTES, "Mach-O load commands");
  let offset = 0;
  let sawSegment = false;
  for (let index = 0; index < commandCount; index += 1) {
    if (offset + 8 > commands.byteLength) {
      throw new RunnerProtocolError("macOS release asset has a truncated Mach-O load command");
    }
    const command = commands.readUInt32LE(offset);
    const commandSize = commands.readUInt32LE(offset + 4);
    if (
      commandSize < 8 || commandSize % 8 !== 0 ||
      commandSize > commands.byteLength - offset
    ) throw new RunnerProtocolError("macOS release asset has an invalid Mach-O load command");
    if (command === MACHO_SEGMENT_64_COMMAND) {
      if (commandSize < 72) {
        throw new RunnerProtocolError("macOS release asset has a truncated Mach-O segment");
      }
      const sectionCount = commands.readUInt32LE(offset + 64);
      if (sectionCount > Math.floor((commandSize - 72) / 80) || 72 + sectionCount * 80 !== commandSize) {
        throw new RunnerProtocolError("macOS release asset has an invalid Mach-O segment");
      }
      const fileOffset = commands.readBigUInt64LE(offset + 40);
      const fileSize = commands.readBigUInt64LE(offset + 48);
      const segmentEnd = fileOffset + fileSize;
      if (segmentEnd > BigInt(size)) {
        throw new RunnerProtocolError("macOS release asset has a truncated Mach-O segment payload");
      }
      for (let sectionIndex = 0; sectionIndex < sectionCount; sectionIndex += 1) {
        const section = offset + 72 + sectionIndex * 80;
        const sectionSize = commands.readBigUInt64LE(section + 40);
        const sectionOffset = BigInt(commands.readUInt32LE(section + 48));
        const relocationOffset = BigInt(commands.readUInt32LE(section + 56));
        const relocationCount = BigInt(commands.readUInt32LE(section + 60));
        const sectionType = commands.readUInt32LE(section + 64) & 0xff;
        if (
          sectionSize > 0n && !MACHO_ZERO_FILL_SECTION_TYPES.has(sectionType) &&
          (sectionOffset < fileOffset || sectionOffset + sectionSize > segmentEnd ||
            sectionOffset + sectionSize > BigInt(size))
        ) {
          throw new RunnerProtocolError(
            "macOS release asset has a truncated Mach-O section payload",
          );
        }
        if (relocationCount > 0n && relocationOffset + relocationCount * 8n > BigInt(size)) {
          throw new RunnerProtocolError(
            "macOS release asset has a truncated Mach-O relocation table",
          );
        }
      }
      sawSegment = true;
    }
    offset += commandSize;
  }
  if (offset !== commandBytes || !sawSegment) {
    throw new RunnerProtocolError("macOS release asset has an inconsistent Mach-O command table");
  }
  return Object.freeze({ architecture: "arm64", bits: 64, format: "Mach-O" });
}

async function readExactlyAt(file, buffer, position, label) {
  let offset = 0;
  while (offset < buffer.byteLength) {
    const { bytesRead } = await file.read(
      buffer,
      offset,
      buffer.byteLength - offset,
      position + offset,
    );
    if (bytesRead === 0) throw new RunnerProtocolError(`${label} is truncated`);
    offset += bytesRead;
  }
}

function parseUploadedLog(value, expected) {
  if (
    !isRecord(value) || typeof value.key !== "string" || value.key.length === 0 ||
    value.size !== expected.bytesStored || value.sha256 !== expected.sha256 ||
    value.contentType !== "text/plain; charset=utf-8"
  ) throw new RunnerProtocolError("macOS CI log upload returned an invalid descriptor");
  return Object.freeze({
    key: value.key,
    size: value.size,
    sha256: value.sha256,
    contentType: value.contentType,
  });
}

function parseUploadedAsset(value, expected) {
  if (
    !isRecord(value) || value.name !== expected.name ||
    value.platform !== "aarch64-apple-darwin" ||
    typeof value.key !== "string" || value.key.length === 0 ||
    value.size !== expected.size || value.sha256 !== expected.sha256 ||
    value.contentType !== "application/octet-stream"
  ) throw new RunnerProtocolError("macOS CI asset upload returned an invalid descriptor");
  return Object.freeze({
    name: value.name,
    platform: value.platform,
    key: value.key,
    size: value.size,
    sha256: value.sha256,
    contentType: value.contentType,
  });
}

async function writeAll(file, buffer) {
  let offset = 0;
  while (offset < buffer.byteLength) {
    const { bytesWritten } = await file.write(buffer, offset, buffer.byteLength - offset);
    if (bytesWritten <= 0) throw new Error("source archive write made no progress");
    offset += bytesWritten;
  }
}

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

function logLimit(value, name) {
  if (!Number.isSafeInteger(value) || value < 0 || value > MAX_LOG_PART_BYTES) {
    throw new TypeError(`${name} must be an integer between 0 and ${MAX_LOG_PART_BYTES}`);
  }
  return value;
}

function assertIdentifier(value, name) {
  if (typeof value !== "string" || !IDENTIFIER.test(value)) {
    throw new RunnerProtocolError(`${name} must be a stable identifier of at most 200 characters`);
  }
}

function isRecord(value) {
  return value != null && typeof value === "object" && !Array.isArray(value);
}

function hasOnlyKeys(value, expected) {
  const allowed = new Set(expected);
  const observed = Object.keys(value);
  return observed.length === allowed.size && observed.every((key) => allowed.has(key));
}

function isCanonicalTimestamp(value) {
  if (typeof value !== "string") return false;
  const timestamp = Date.parse(value);
  return Number.isFinite(timestamp) && new Date(timestamp).toISOString() === value;
}

function canonicalTimestamp(value, label) {
  if (!isCanonicalTimestamp(value)) {
    throw new RunnerProtocolError(`${label} is not canonical UTC RFC3339`);
  }
  return Date.parse(value);
}

export function leaseRequestTimeout(
  leaseDeadlineMs,
  safetyMarginMs = LEASE_SAFETY_MARGIN_MS,
  nowMs = Date.now(),
) {
  if (
    !Number.isSafeInteger(leaseDeadlineMs) ||
    !Number.isSafeInteger(safetyMarginMs) || safetyMarginMs <= 0 ||
    !Number.isSafeInteger(nowMs)
  ) throw new TypeError("lease deadline, safety margin, and current time must be integers");
  const available = leaseDeadlineMs - safetyMarginMs - nowMs;
  if (available <= 0) {
    throw new RunnerProtocolError("macOS CI lease has no safe heartbeat request budget");
  }
  return Math.min(REQUEST_TIMEOUT_MS, available);
}

function requiredEnvironment(env, name) {
  const value = env[name]?.trim();
  if (!value) throw new RunnerProtocolError(`${name} is required`);
  return value;
}

function environmentInteger(env, name, fallback, minimum, maximum) {
  if (env[name] == null || env[name] === "") return fallback;
  const value = Number(env[name]);
  if (!Number.isSafeInteger(value) || value < minimum || value > maximum) {
    throw new RunnerProtocolError(`${name} must be an integer between ${minimum} and ${maximum}`);
  }
  return value;
}

function requestSignal(parent, timeoutMs) {
  const timeout = AbortSignal.timeout(timeoutMs);
  return parent ? AbortSignal.any([parent, timeout]) : timeout;
}

function forwardAbort(source, destination) {
  if (!source) return () => {};
  const forward = () => destination.abort(source.reason);
  if (source.aborted) forward();
  else source.addEventListener("abort", forward, { once: true });
  return () => source.removeEventListener("abort", forward);
}

function delay(milliseconds, signal) {
  return new Promise((resolvePromise, rejectPromise) => {
    if (signal?.aborted) {
      rejectPromise(signal.reason);
      return;
    }
    const finished = () => {
      signal?.removeEventListener("abort", aborted);
      resolvePromise();
    };
    const timer = setTimeout(finished, milliseconds);
    const aborted = () => {
      clearTimeout(timer);
      rejectPromise(signal.reason);
    };
    signal?.addEventListener("abort", aborted, { once: true });
  });
}

async function boundedResponseText(response, maximumBytes) {
  if (response.body == null) return "";
  const reader = response.body.getReader();
  const chunks = [];
  let bytes = 0;
  try {
    while (bytes <= maximumBytes) {
      const { done, value } = await reader.read();
      if (done) break;
      chunks.push(Buffer.from(value));
      bytes += value.byteLength;
      if (bytes > maximumBytes) {
        await reader.cancel().catch(() => undefined);
        break;
      }
    }
  } finally {
    reader.releaseLock();
  }
  return Buffer.concat(chunks).subarray(0, maximumBytes).toString("utf8");
}

if (resolve(process.argv[1] ?? "") === scriptPath) {
  await main();
}
