import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { constants as fsConstants } from "node:fs";
import {
  lstat,
  mkdir,
  mkdtemp,
  open,
  readFile,
  readdir,
  readlink,
  realpath,
  rename,
  rm,
  stat,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, isAbsolute, posix, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

import {
  canonicalJson as canonicalCargoVendorJson,
  cargoVendorFrame,
  readFramedArtifact,
} from "./publish-ci-cargo-vendor.mjs";

const scriptPath = fileURLToPath(import.meta.url);
const webDirectory = resolve(dirname(scriptPath), "..");
const defaultRepository = resolve(webDirectory, "..");
const cargoBuilderPath = resolve(
  webDirectory,
  "scripts",
  "ci-pr-cargo-builder.mjs",
);
const cargoUploaderPath = resolve(
  webDirectory,
  "scripts",
  "publish-ci-cargo-vendor.mjs",
);

const sha1Pattern = /^[a-f0-9]{40}$/;
const sha256Pattern = /^[a-f0-9]{64}$/;
const maximumArtifactBytes = 64 * 1024 * 1024;
const maximumJsonBytes = 1024 * 1024;
const maximumProcessOutputBytes = 16 * 1024 * 1024;
const maximumTarEntries = 50_000;
const processTerminationGraceMs = 1_000;
const processKillWaitMs = 5_000;
const processGroupPollMs = 25;
const cargoHomeDirectoryName = "nanocodex-ci-cargo-home";
const cargoAuthorityFileNames = [
  "config",
  "config.toml",
  "credentials",
  "credentials.toml",
];
const cargoConfigurationFileNames = ["config", "config.toml"];
const webLockfileRelativePath = "web/package-lock.json";
const webPackageRelativePath = "web/package.json";
const webCloudflarePatchRelativePaths = Object.freeze([
  "web/patches/@cloudflare+ci+0.1.0.patch",
  "web/patches/@cloudflare+sandbox+0.12.1.patch",
]);
const maximumNpmBuiltinConfigBytes = 64 * 1024;
const terminalWorkflowStates = new Set(["complete", "errored", "terminated"]);
const activeWorkflowStates = new Set([
  "queued",
  "running",
  "paused",
  "waiting",
  "unknown",
]);
const runtimeEnvironmentNames = [
  "PATH",
  "HOME",
  "TMPDIR",
  "TMP",
  "TEMP",
  "LANG",
  "LC_ALL",
  "LC_CTYPE",
  "TZ",
  "SSL_CERT_FILE",
  "SSL_CERT_DIR",
  "NODE_EXTRA_CA_CERTS",
  "CARGO_HOME",
  "RUSTUP_HOME",
];

export const authoritativeRepositoryUrl =
  "https://github.com/gakonst/nanocodex.git";
export const authoritativeRustSecRepositoryUrl =
  "https://github.com/RustSec/advisory-db.git";
export const githubStatusContext = "ci success";

export class StaleHeadError extends Error {
  constructor(expected, observed, phase) {
    super(
      `${phase}: authoritative master is ${shortSha(observed)}, expected ${shortSha(expected)}`,
    );
    this.name = "StaleHeadError";
    this.expected = expected;
    this.observed = observed;
    this.phase = phase;
  }
}

export class CiRunTerminalError extends Error {
  constructor(head, outcome, detail) {
    super(`Cloudflare CI ${outcome} for ${shortSha(head)}${detail ? `: ${detail}` : ""}`);
    this.name = "CiRunTerminalError";
    this.head = head;
    this.outcome = outcome;
    this.githubState = outcome === "failure" ? "failure" : "error";
  }
}

export function parseArguments(args) {
  if (args[0] === "repair-repository") {
    if (args.length !== 2) {
      throw new Error("repair-repository requires exactly one full lowercase SHA-1");
    }
    assertSha1(args[1], "repository repair head");
    return {
      command: "repair-repository",
      head: args[1],
      once: true,
      help: false,
    };
  }
  let once = false;
  let help = false;
  for (const argument of args) {
    if (argument === "--once") {
      if (once) throw new Error("--once may be supplied only once");
      once = true;
    } else if (argument === "--help" || argument === "-h") {
      help = true;
    } else {
      throw new Error(`unknown argument: ${argument}`);
    }
  }
  return { command: "run", once, help };
}

export function runtimeEnvironment(env = process.env) {
  const isolated = {};
  for (const name of runtimeEnvironmentNames) {
    if (typeof env[name] === "string" && env[name] !== "") isolated[name] = env[name];
  }
  isolated.PATH ??= "/usr/local/bin:/usr/bin:/bin";
  isolated.TMPDIR ??= tmpdir();
  isolated.LANG ??= "C.UTF-8";
  return {
    ...isolated,
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_NO_REPLACE_OBJECTS: "1",
    GIT_OPTIONAL_LOCKS: "0",
    GIT_TERMINAL_PROMPT: "0",
  };
}

export function publicationEnvironment(env, stage, {
  cargoVendorSha256,
  repository,
  rustSecRepository,
  rustSecRevision,
} = {}) {
  const child = {
    ...runtimeEnvironment(env),
    NANOCODEX_CI_ORIGIN: requiredEnvironment(env, "NANOCODEX_CI_ORIGIN"),
    NANOCODEX_CI_TOKEN: requiredEnvironment(env, "NANOCODEX_CI_TOKEN"),
  };
  if (stage === "rustsec") {
    child.NANOCODEX_RUSTSEC_REPO = rustSecRepository ??
      requiredEnvironment(env, "NANOCODEX_RUSTSEC_REPO");
  } else if (stage === "cargo-vendor") {
    // Upload-only. The framed artifact is inherited as fd 3; no checkout or
    // Cargo configuration is accepted at this authority boundary.
    delete child.CARGO_HOME;
    delete child.RUSTUP_HOME;
  } else if (stage === "source") {
    assertSha256(cargoVendorSha256, "Cargo vendor bundle");
    child.NANOCODEX_REPO = repository ?? env.NANOCODEX_REPO ?? defaultRepository;
    child.NANOCODEX_RUSTSEC_REVISION = rustSecRevision ??
      requiredEnvironment(env, "NANOCODEX_RUSTSEC_REVISION");
    child.NANOCODEX_CI_CARGO_VENDOR_SHA256 = cargoVendorSha256;
  } else {
    throw new Error(`unknown publication stage: ${stage}`);
  }
  return child;
}

export function cargoEnvironment(env, cargoHome) {
  if (typeof cargoHome !== "string" || !isAbsolute(cargoHome)) {
    throw new Error("trusted CI Cargo home must be an absolute path");
  }
  return {
    ...runtimeEnvironment(env),
    CARGO_HOME: cargoHome,
    CARGO_NET_GIT_FETCH_WITH_CLI: "false",
    CARGO_REGISTRIES_CRATES_IO_PROTOCOL: "sparse",
    CARGO_TERM_COLOR: "never",
    GIT_ASKPASS: "/usr/bin/false",
    GIT_TERMINAL_PROMPT: "0",
    SSH_ASKPASS: "/usr/bin/false",
    SSH_ASKPASS_REQUIRE: "force",
  };
}

export function cargoBuilderEnvironment(env = process.env) {
  void env;
  return {
    PATH: "/usr/bin:/bin",
    LANG: "C.UTF-8",
    LC_ALL: "C.UTF-8",
  };
}

export async function captureCargoVendorArtifact({
  args,
  artifactDirectory,
  command,
  cwd,
  env,
  input,
  secrets = [],
  signal,
  timeoutMs,
}) {
  if (typeof command !== "string" || !isAbsolute(command)) {
    throw new Error("Cargo builder command must be an absolute path");
  }
  if (!Array.isArray(args) || args.some((value) => typeof value !== "string")) {
    throw new Error("Cargo builder arguments must be fixed strings");
  }
  if (!Buffer.isBuffer(input) || input.length === 0 || input.length > 16 * 1024) {
    throw new Error("Cargo builder input must be bounded canonical JSON");
  }
  if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 1) {
    throw new Error("Cargo builder timeout must be a positive integer");
  }
  throwIfAborted(signal);
  const parent = await realpath(artifactDirectory);
  const root = await mkdtemp(resolve(parent, "nanocodex-ci-cargo-artifact-"));
  const path = resolve(root, "artifact.frame");
  const flags = fsConstants.O_CREAT | fsConstants.O_EXCL | fsConstants.O_WRONLY |
    (fsConstants.O_NOFOLLOW ?? 0);
  let writer;
  let reader;
  try {
    writer = await open(path, flags, 0o600);
    await writer.chmod(0o600);
    const created = await writer.stat();
    if (
      !created.isFile() || created.nlink !== 1 ||
      created.uid !== process.getuid?.() || (created.mode & 0o777) !== 0o600
    ) throw new Error("Cargo builder artifact is not controller-owned mode 0600");

    const child = spawn(command, args, {
      cwd,
      env,
      shell: false,
      detached: process.platform !== "win32",
      stdio: ["pipe", "pipe", "pipe"],
    });
    const processGroupId = process.platform === "win32" ? undefined : child.pid;
    let failure;
    let stderrBytes = 0;
    const stderrChunks = [];
    let outputBytes = 0;
    let cleanupTask;
    let resolveCleanupStarted;
    const cleanupStarted = new Promise((resolvePromise) => {
      resolveCleanupStarted = resolvePromise;
    });
    const beginGroupCleanup = () => {
      if (cleanupTask != null) return;
      cleanupTask = cleanupDetachedProcessGroup(child, processGroupId).then(
        (value) => ({ value }),
        (error) => ({ error }),
      );
      resolveCleanupStarted();
    };
    const fail = (cause) => {
      failure ??= cause;
      beginGroupCleanup();
    };
    const onAbort = () => fail(
      signal.reason ?? new DOMException("Cargo builder aborted", "AbortError"),
    );
    signal?.addEventListener("abort", onAbort, { once: true });
    if (signal?.aborted) onAbort();
    const timeout = setTimeout(
      () => fail(new Error(`Cargo builder timed out after ${timeoutMs}ms`)),
      timeoutMs,
    );
    timeout.unref?.();

    const stdoutTask = (async () => {
      try {
        for await (const chunk of child.stdout) {
          outputBytes += chunk.length;
          if (outputBytes > cargoVendorFrame.maximumTotalBytes) {
            fail(new Error("Cargo builder exceeded the hard framed-output bound"));
            continue;
          }
          if (failure == null) await writer.write(chunk);
        }
      } catch (cause) {
        fail(cause);
      }
    })();
    const stderrTask = (async () => {
      try {
        for await (const chunk of child.stderr) {
          stderrBytes += chunk.length;
          if (stderrBytes > maximumProcessOutputBytes) {
            fail(new Error("Cargo builder exceeded the stderr bound"));
            continue;
          }
          if (failure == null) stderrChunks.push(chunk);
        }
      } catch (cause) {
        fail(cause);
      }
    })();
    const close = new Promise((resolvePromise) => {
      child.once("close", (code, signalName) =>
        resolvePromise({ code, signalName }));
    });
    child.once("error", fail);
    // Start cleanup when the leader exits. Waiting for `close` first would let
    // inherited pipes delay cleanup and detached stdio hide live descendants.
    child.once("exit", beginGroupCleanup);
    child.stdin.on("error", (cause) => {
      if (cause?.code !== "EPIPE") fail(cause);
    });
    child.stdin.end(input);

    let cleanupOutcome;
    let result;
    let closeFailure;
    try {
      await cleanupStarted;
      cleanupOutcome = await cleanupTask;
      try {
        result = await waitForChildClose(close, command);
      } catch (cause) {
        closeFailure = cause;
      }
      if (closeFailure == null) await Promise.all([stdoutTask, stderrTask]);
    } finally {
      clearTimeout(timeout);
      signal?.removeEventListener("abort", onAbort);
    }
    const cleanupFailures = [cleanupOutcome?.error, closeFailure].filter(Boolean);
    const cleanupFailure = cleanupFailures.length > 1
      ? new AggregateError(cleanupFailures, "Cargo builder process cleanup failed")
      : cleanupFailures[0];
    const abortFailure = signal?.aborted
      ? signal.reason ?? new DOMException("Cargo builder aborted", "AbortError")
      : undefined;
    const primaryFailure = abortFailure ?? failure;
    if (primaryFailure) {
      if (cleanupFailure != null) {
        if (primaryFailure instanceof Error) primaryFailure.cleanupCause = cleanupFailure;
        else {
          throw new AggregateError(
            [primaryFailure, cleanupFailure],
            "Cargo builder abort and cleanup failed",
          );
        }
      }
      throw primaryFailure;
    }
    if (cleanupFailure != null) throw cleanupFailure;
    const { code, signalName } = result;
    if (code === 0 && cleanupOutcome.value.observedLiveGroup) {
      throw new Error(
        "Cargo builder exited successfully but left a live detached process-group descendant",
      );
    }
    const stderrText = Buffer.concat(stderrChunks, stderrBytes).toString("utf8");
    if (code !== 0) {
      const detail = redactSecrets(
        stderrText,
        secrets,
      ).trim().slice(0, maximumErrorBytes);
      throw new Error(
        `Cargo builder failed with exit ${String(code)}` +
        `${signalName ? ` (${signalName})` : ""}${detail ? `: ${detail}` : ""}`,
      );
    }
    if (stderrText !== "") {
      throw new Error("successful Cargo builder emitted stderr");
    }
    await writer.sync();
    await writer.close();
    writer = undefined;
    reader = await open(
      path,
      fsConstants.O_RDONLY | (fsConstants.O_NOFOLLOW ?? 0),
    );
    const parsed = await readFramedArtifact(reader.fd);
    if (
      parsed.identity.dev !== created.dev || parsed.identity.ino !== created.ino ||
      parsed.identity.uid !== created.uid || parsed.identity.nlink !== created.nlink
    ) throw new Error("Cargo builder artifact was replaced before controller parsing");
    return {
      ...parsed,
      handle: reader,
      path,
      cleanup: async () => {
        await reader?.close().catch(() => {});
        reader = undefined;
        await rm(root, { recursive: true, force: true });
      },
    };
  } catch (cause) {
    await writer?.close().catch(() => {});
    await reader?.close().catch(() => {});
    await rm(root, { recursive: true, force: true });
    throw cause;
  }
}

export function assertUploaderDescriptor(stdout, expected) {
  if (typeof stdout !== "string" || !stdout.endsWith("\n") || stdout.indexOf("\n") !== stdout.length - 1) {
    throw new Error("Cargo vendor uploader returned invalid stdout");
  }
  let observed;
  try {
    observed = JSON.parse(stdout.slice(0, -1));
  } catch {
    throw new Error("Cargo vendor uploader returned invalid JSON");
  }
  if (
    canonicalCargoVendorJson(observed) !== stdout.slice(0, -1) ||
    canonicalCargoVendorJson(observed) !== canonicalCargoVendorJson(expected)
  ) throw new Error("Cargo vendor uploader returned a different descriptor");
  return observed;
}

export function npmInstallEnvironment(env, isolatedHome) {
  if (typeof isolatedHome !== "string" || !isAbsolute(isolatedHome)) {
    throw new Error("trusted CI npm home must be an absolute path");
  }
  const child = runtimeEnvironment(env);
  delete child.HOME;
  delete child.CARGO_HOME;
  delete child.RUSTUP_HOME;
  return {
    ...child,
    HOME: isolatedHome,
    USERPROFILE: isolatedHome,
    XDG_CACHE_HOME: resolve(isolatedHome, "xdg-cache"),
    XDG_CONFIG_HOME: resolve(isolatedHome, "xdg-config"),
    NPM_CONFIG_AUDIT: "false",
    NPM_CONFIG_CACHE: resolve(isolatedHome, "npm-cache"),
    NPM_CONFIG_FUND: "false",
    NPM_CONFIG_GLOBALCONFIG: resolve(isolatedHome, "global.npmrc"),
    NPM_CONFIG_IGNORE_SCRIPTS: "true",
    NPM_CONFIG_REGISTRY: "https://registry.npmjs.org/",
    NPM_CONFIG_UPDATE_NOTIFIER: "false",
    NPM_CONFIG_USERCONFIG: resolve(isolatedHome, "user.npmrc"),
  };
}

export async function resolveTrustedNpmCli(nodeExecutable = process.execPath) {
  if (typeof nodeExecutable !== "string" || !isAbsolute(nodeExecutable)) {
    throw new Error("trusted CI Node executable must be an absolute path");
  }
  const canonicalNode = await realpath(nodeExecutable);
  await validateTrustedRuntimeFile(canonicalNode, "Node executable");
  const npmDirectory = resolve(dirname(canonicalNode), "..", "lib", "node_modules", "npm");
  const npmCli = resolve(npmDirectory, "bin", "npm-cli.js");
  await validateTrustedRuntimeFile(npmCli, "npm CLI");
  for (const name of ["npmrc", ".npmrc"]) {
    const builtinConfig = resolve(npmDirectory, name);
    const identity = await lstat(builtinConfig).catch((cause) => {
      if (cause?.code === "ENOENT") return undefined;
      throw cause;
    });
    if (identity === undefined) continue;
    await validateTrustedRuntimeFile(builtinConfig, "npm builtin configuration");
    const npmrc = await readFile(builtinConfig);
    if (npmrc.length > maximumNpmBuiltinConfigBytes) {
      throw new Error("trusted CI npm builtin configuration is unexpectedly large");
    }
    const npmrcText = npmrc.toString("utf8");
    if (
      /(?:^|\n)\s*(?:(?:\/\/[^=\r\n]*:)?(?:_auth|_authToken|username|_password|certfile|keyfile)|@[^:=\s]+:registry)\s*=/i
        .test(npmrcText) ||
      /\$\{[^}\n]*(?:token|secret|password|auth)[^}\n]*\}/i.test(npmrcText)
    ) {
      throw new Error("trusted CI npm builtin configuration contains credential authority");
    }
  }
  return npmCli;
}

export async function validateNpmConfigurationIsolation(repository, webRoot) {
  if (
    typeof repository !== "string" ||
    typeof webRoot !== "string" ||
    !isAbsolute(repository) ||
    !isAbsolute(webRoot)
  ) {
    throw new Error("trusted CI repository and web root must be absolute paths");
  }
  const canonicalRepository = await realpath(repository);
  const canonicalWebRoot = await realpath(webRoot);
  if (
    canonicalWebRoot !== resolve(canonicalRepository, "web") ||
    !canonicalWebRoot.startsWith(`${canonicalRepository}${sep}`)
  ) {
    throw new Error("trusted CI web root must be the repository web directory");
  }
  for (const path of [
    resolve(canonicalRepository, ".npmrc"),
    resolve(canonicalWebRoot, ".npmrc"),
  ]) {
    const identity = await lstat(path).catch((cause) => {
      if (cause?.code === "ENOENT" || cause?.code === "ENOTDIR") return undefined;
      throw cause;
    });
    if (identity !== undefined) {
      throw new Error(`trusted CI npm rejects project configuration: ${path}`);
    }
  }
  return canonicalWebRoot;
}

export async function validateNpmInstallTarget(webRoot) {
  const nodeModules = resolve(webRoot, "node_modules");
  const identity = await lstat(nodeModules).catch((cause) => {
    if (cause?.code === "ENOENT") return undefined;
    throw cause;
  });
  if (identity === undefined) return nodeModules;
  if (!identity.isDirectory() || identity.isSymbolicLink()) {
    throw new Error("trusted CI refuses to let npm replace a non-directory or symlink node_modules");
  }
  if (await realpath(nodeModules) !== nodeModules) {
    throw new Error("trusted CI node_modules path must not traverse a symlink");
  }
  if (typeof process.geteuid === "function" && identity.uid !== process.geteuid()) {
    throw new Error("trusted CI node_modules must be owned by the controller user");
  }
  if ((identity.mode & 0o022) !== 0) {
    throw new Error("trusted CI node_modules must not be group- or world-writable");
  }
  return nodeModules;
}

export async function fingerprintNodeModules(nodeModules) {
  if (typeof nodeModules !== "string" || !isAbsolute(nodeModules)) {
    throw new Error("trusted CI node_modules path must be absolute");
  }
  const canonical = await realpath(nodeModules).catch((cause) => {
    throw new Error(`trusted CI node_modules is unavailable at ${nodeModules}`, { cause });
  });
  if (canonical !== nodeModules) {
    throw new Error("trusted CI node_modules path must not traverse a symlink");
  }
  const rootIdentity = await lstat(canonical);
  if (!rootIdentity.isDirectory() || rootIdentity.isSymbolicLink()) {
    throw new Error("trusted CI node_modules must be a real directory");
  }
  assertPrivateNodeModuleIdentity(rootIdentity, ".");
  const digest = createHash("sha256");
  digest.update("nanocodex-web-node-modules-v1\0");

  const visit = async (directory, relativeDirectory) => {
    const before = await lstat(directory);
    if (!before.isDirectory() || before.isSymbolicLink()) {
      throw new Error(`trusted CI node_modules contains an invalid directory: ${relativeDirectory}`);
    }
    const names = (await readdir(directory)).sort();
    for (const name of names) {
      if (name.includes("\0") || name.includes("/")) {
        throw new Error("trusted CI node_modules contains an invalid entry name");
      }
      const path = resolve(directory, name);
      const relativePath = relativeDirectory ? `${relativeDirectory}/${name}` : name;
      const identity = await lstat(path);
      assertPrivateNodeModuleIdentity(identity, relativePath);
      const mode = identity.mode & 0o7777;
      if (identity.isDirectory() && !identity.isSymbolicLink()) {
        digest.update(`${JSON.stringify(["directory", relativePath, mode])}\n`);
        await visit(path, relativePath);
      } else if (identity.isFile() && !identity.isSymbolicLink()) {
        const body = await readFile(path);
        const after = await lstat(path);
        if (!sameFileIdentity(identity, after)) {
          throw new Error(`trusted CI node_modules changed while hashing ${relativePath}`);
        }
        digest.update(`${JSON.stringify([
          "file",
          relativePath,
          mode,
          body.length,
          createHash("sha256").update(body).digest("hex"),
        ])}\n`);
      } else if (identity.isSymbolicLink()) {
        const target = await readlink(path);
        const after = await lstat(path);
        if (!sameFileIdentity(identity, after)) {
          throw new Error(`trusted CI node_modules changed while hashing ${relativePath}`);
        }
        digest.update(`${JSON.stringify(["symlink", relativePath, mode, target])}\n`);
      } else {
        throw new Error(`trusted CI node_modules rejects special entry: ${relativePath}`);
      }
    }
    const after = await lstat(directory);
    if (!sameFileIdentity(before, after)) {
      throw new Error(`trusted CI node_modules changed while hashing ${relativeDirectory || "."}`);
    }
  };
  await visit(canonical, "");
  return digest.digest("hex");
}

export async function prepareWebDeploymentToolchain({
  env,
  head,
  repository,
  runNpm,
  runPatchPackage,
  signal,
  verifyCheckout,
  verifyRepositoryFile,
  webRoot = resolve(repository, "web"),
}) {
  assertSha1(head, "web deployment toolchain head");
  if (
    typeof runNpm !== "function" ||
    typeof runPatchPackage !== "function" ||
    typeof verifyCheckout !== "function" ||
    typeof verifyRepositoryFile !== "function"
  ) {
    throw new TypeError(
      "trusted CI npm/patch-package runners and authoritative verifiers are required",
    );
  }
  const canonicalWebRoot = await validateNpmConfigurationIsolation(repository, webRoot);
  await validateNpmInstallTarget(canonicalWebRoot);
  await verifyCheckout(head, "before web deployment toolchain install", signal);
  const [lockfileBlob, packageBlob, ...cloudflarePatchBlobs] = await Promise.all([
    verifyRepositoryFile(head, webLockfileRelativePath, "before npm ci", signal),
    verifyRepositoryFile(head, webPackageRelativePath, "before npm ci", signal),
    ...webCloudflarePatchRelativePaths.map((relativePath) =>
      verifyRepositoryFile(head, relativePath, "before npm ci", signal)
    ),
  ]);
  assertSha1(lockfileBlob, "authoritative web package-lock.json blob");
  assertSha1(packageBlob, "authoritative web package.json blob");
  for (const [index, blob] of cloudflarePatchBlobs.entries()) {
    assertSha1(blob, `authoritative Cloudflare patch ${index + 1} blob`);
  }

  const isolatedHome = await mkdtemp(resolve(
    await realpath(tmpdir()),
    "nanocodex-ci-npm-home-",
  ));
  try {
    const homeIdentity = await lstat(isolatedHome);
    if (
      !homeIdentity.isDirectory() ||
      homeIdentity.isSymbolicLink() ||
      (homeIdentity.mode & 0o777) !== 0o700 ||
      await realpath(isolatedHome) !== isolatedHome
    ) {
      throw new Error("trusted CI npm home must be a private real directory");
    }
    const npmEnvironment = npmInstallEnvironment(env, isolatedHome);
    const configurationHandles = [];
    try {
      for (const path of [
        npmEnvironment.NPM_CONFIG_USERCONFIG,
        npmEnvironment.NPM_CONFIG_GLOBALCONFIG,
      ]) {
        configurationHandles.push(await open(
          path,
          fsConstants.O_CREAT | fsConstants.O_EXCL | fsConstants.O_WRONLY |
            fsConstants.O_NOFOLLOW,
          0o600,
        ));
      }
    } finally {
      await Promise.all(configurationHandles.map((handle) => handle.close()));
    }
    await runNpm(
      ["ci", "--prefix", "web", "--ignore-scripts"],
      npmEnvironment,
      signal,
    );
    await verifyCheckout(head, "after npm ci and before Cloudflare patch", signal);
    const [prePatchLockfileBlob, prePatchPackageBlob, ...prePatchBlobs] = await Promise.all([
      verifyRepositoryFile(head, webLockfileRelativePath, "after npm ci", signal),
      verifyRepositoryFile(head, webPackageRelativePath, "after npm ci", signal),
      ...webCloudflarePatchRelativePaths.map((relativePath) =>
        verifyRepositoryFile(head, relativePath, "before patch-package", signal)
      ),
    ]);
    if (
      prePatchLockfileBlob !== lockfileBlob ||
      prePatchPackageBlob !== packageBlob ||
      prePatchBlobs.some((blob, index) => blob !== cloudflarePatchBlobs[index])
    ) {
      throw new Error("web deployment manifests changed during npm ci");
    }
    await runPatchPackage(
      [
        resolve(canonicalWebRoot, "node_modules", "patch-package", "index.js"),
        "--error-on-fail",
      ],
      npmEnvironment,
      signal,
    );
  } finally {
    await rm(isolatedHome, { recursive: true, force: true });
  }

  await verifyCheckout(head, "after web deployment toolchain install", signal);
  await validateNpmConfigurationIsolation(repository, canonicalWebRoot);
  await validateNpmInstallTarget(canonicalWebRoot);
  const [installedLockfileBlob, installedPackageBlob, ...installedPatchBlobs] = await Promise.all([
    verifyRepositoryFile(head, webLockfileRelativePath, "after patch-package", signal),
    verifyRepositoryFile(head, webPackageRelativePath, "after patch-package", signal),
    ...webCloudflarePatchRelativePaths.map((relativePath) =>
      verifyRepositoryFile(head, relativePath, "after patch-package", signal)
    ),
  ]);
  if (
    installedLockfileBlob !== lockfileBlob ||
    installedPackageBlob !== packageBlob ||
    installedPatchBlobs.some((blob, index) => blob !== cloudflarePatchBlobs[index])
  ) {
    throw new Error("web deployment manifests changed during toolchain installation");
  }
  return Object.freeze({
    version: 2,
    head,
    lockfileBlob,
    packageBlob,
    cloudflarePatchBlobs: Object.freeze([...cloudflarePatchBlobs]),
    nodeModulesSha256: await fingerprintNodeModules(resolve(canonicalWebRoot, "node_modules")),
  });
}

export async function assertWebDeploymentToolchain({
  attestation,
  head,
  phase,
  repository,
  signal,
  verifyCheckout,
  verifyRepositoryFile,
  webRoot = resolve(repository, "web"),
}) {
  assertSha1(head, "web deployment toolchain head");
  if (
    !isRecord(attestation) ||
    attestation.version !== 2 ||
    attestation.head !== head ||
    !sha1Pattern.test(attestation.lockfileBlob ?? "") ||
    !sha1Pattern.test(attestation.packageBlob ?? "") ||
    !Array.isArray(attestation.cloudflarePatchBlobs) ||
    attestation.cloudflarePatchBlobs.length !== webCloudflarePatchRelativePaths.length ||
    attestation.cloudflarePatchBlobs.some((blob) => !sha1Pattern.test(blob ?? "")) ||
    !sha256Pattern.test(attestation.nodeModulesSha256 ?? "")
  ) {
    throw new Error("trusted CI web deployment toolchain attestation is invalid");
  }
  const canonicalWebRoot = await validateNpmConfigurationIsolation(repository, webRoot);
  await validateNpmInstallTarget(canonicalWebRoot);
  await verifyCheckout(head, `${phase}: authoritative checkout`, signal);
  const [lockfileBlob, packageBlob, ...cloudflarePatchBlobs] = await Promise.all([
    verifyRepositoryFile(head, webLockfileRelativePath, `${phase}: package-lock.json`, signal),
    verifyRepositoryFile(head, webPackageRelativePath, `${phase}: package.json`, signal),
    ...webCloudflarePatchRelativePaths.map((relativePath) =>
      verifyRepositoryFile(head, relativePath, `${phase}: Cloudflare patch`, signal)
    ),
  ]);
  if (
    lockfileBlob !== attestation.lockfileBlob ||
    packageBlob !== attestation.packageBlob ||
    cloudflarePatchBlobs.some(
      (blob, index) => blob !== attestation.cloudflarePatchBlobs[index],
    ) ||
    await fingerprintNodeModules(resolve(canonicalWebRoot, "node_modules")) !==
      attestation.nodeModulesSha256
  ) {
    throw new Error(`trusted CI web deployment toolchain is tampered or stale during ${phase}`);
  }
  return attestation;
}

export async function prepareCargoHome(
  gitCommonDirectory,
  { directoryName = cargoHomeDirectoryName } = {},
) {
  if (typeof gitCommonDirectory !== "string" || !isAbsolute(gitCommonDirectory)) {
    throw new Error("Git common directory must be an absolute path");
  }
  if (!/^[a-z0-9][a-z0-9-]{0,79}$/.test(directoryName)) {
    throw new Error("trusted CI Cargo home directory name is invalid");
  }
  const canonicalCommonDirectory = await realpath(gitCommonDirectory);
  const commonIdentity = await lstat(canonicalCommonDirectory);
  if (!commonIdentity.isDirectory() || commonIdentity.isSymbolicLink()) {
    throw new Error("Git common directory is not a real directory");
  }
  const cargoHome = resolve(canonicalCommonDirectory, directoryName);
  if (dirname(cargoHome) !== canonicalCommonDirectory) {
    throw new Error("trusted CI Cargo home escaped the Git common directory");
  }
  try {
    await mkdir(cargoHome, { mode: 0o700 });
  } catch (cause) {
    if (cause?.code !== "EEXIST") {
      throw new Error(`could not create trusted CI Cargo home at ${cargoHome}`, { cause });
    }
  }
  return validateCargoHome(cargoHome);
}

export async function validateCargoHome(cargoHome) {
  if (typeof cargoHome !== "string" || !isAbsolute(cargoHome)) {
    throw new Error("trusted CI Cargo home must be an absolute path");
  }
  let identity;
  try {
    identity = await lstat(cargoHome);
  } catch (cause) {
    throw new Error(`trusted CI Cargo home is unavailable at ${cargoHome}`, { cause });
  }
  if (!identity.isDirectory() || identity.isSymbolicLink()) {
    throw new Error("trusted CI Cargo home must be a real directory, not a symlink");
  }
  if ((identity.mode & 0o777) !== 0o700) {
    throw new Error("trusted CI Cargo home must be private to its owner (mode 0700)");
  }
  if (typeof process.geteuid === "function" && identity.uid !== process.geteuid()) {
    throw new Error("trusted CI Cargo home must be owned by the controller user");
  }
  if (await realpath(cargoHome) !== cargoHome) {
    throw new Error("trusted CI Cargo home path must not traverse a symlink");
  }
  for (const name of cargoAuthorityFileNames) {
    const authority = await lstat(resolve(cargoHome, name)).catch((cause) => {
      if (cause?.code === "ENOENT") return undefined;
      throw cause;
    });
    if (authority !== undefined) {
      throw new Error(`trusted CI Cargo home rejects Cargo authority file: ${name}`);
    }
  }
  return cargoHome;
}

export async function validateCargoConfigurationDiscovery({
  cargoHome,
  repository,
  verifyRepositoryConfig,
}) {
  await validateCargoHome(cargoHome);
  if (typeof repository !== "string" || !isAbsolute(repository)) {
    throw new Error("trusted CI repository must be an absolute path");
  }
  if (typeof verifyRepositoryConfig !== "function") {
    throw new TypeError("authoritative repository Cargo config verifier is required");
  }
  const canonicalRepository = await realpath(repository);
  const repositoryIdentity = await lstat(canonicalRepository);
  if (!repositoryIdentity.isDirectory() || repositoryIdentity.isSymbolicLink()) {
    throw new Error("trusted CI repository must resolve to a real directory");
  }

  let directory = canonicalRepository;
  while (true) {
    for (const name of cargoConfigurationFileNames) {
      const path = resolve(directory, ".cargo", name);
      const identity = await lstat(path).catch((cause) => {
        if (cause?.code === "ENOENT" || cause?.code === "ENOTDIR") return undefined;
        throw cause;
      });
      if (identity === undefined) continue;
      if (directory !== canonicalRepository) {
        throw new Error(`trusted CI Cargo rejects ambient configuration: ${path}`);
      }
      if (!identity.isFile() || identity.isSymbolicLink() || await realpath(path) !== path) {
        throw new Error(`trusted CI Cargo config must be a real repository file: ${path}`);
      }
      await verifyRepositoryConfig(name, path);
    }
    const parent = dirname(directory);
    if (parent === directory) break;
    directory = parent;
  }
  return canonicalRepository;
}

export function deploymentEnvironment(env, head, { origin } = {}) {
  assertSha1(head, "deployment head");
  return {
    ...runtimeEnvironment(env),
    CLOUDFLARE_ACCOUNT_ID: requiredEnvironment(env, "CLOUDFLARE_ACCOUNT_ID"),
    CLOUDFLARE_API_TOKEN: requiredEnvironment(env, "CLOUDFLARE_API_TOKEN"),
    NANOCODEX_DEPLOYMENT_SHA: head,
    NANOCODEX_WEB_ORIGIN: origin ??
      env.NANOCODEX_WEB_ORIGIN ??
      requiredEnvironment(env, "NANOCODEX_CI_ORIGIN"),
  };
}

export function repositoryEnvironment(env, { repository, origin, repair = false } = {}) {
  const child = {
    ...runtimeEnvironment(env),
    NANOCODEX_GIT_ORIGIN: origin ??
      env.NANOCODEX_GIT_ORIGIN ??
      env.NANOCODEX_WEB_ORIGIN ??
      requiredEnvironment(env, "NANOCODEX_CI_ORIGIN"),
    NANOCODEX_GIT_TOKEN: requiredEnvironment(env, "NANOCODEX_GIT_TOKEN"),
    NANOCODEX_REPO: repository ?? env.NANOCODEX_REPO ?? defaultRepository,
    NANOCODEX_GIT_UPLOAD_TIMEOUT_MS: "60000",
  };
  if (repair) {
    child.NANOCODEX_REPAIR_INVALID_PUBLICATION = "1";
  }
  return child;
}

export function assertFreshHead(expected, observed, phase = "stale-head check") {
  assertSha1(expected, "expected master head");
  assertSha1(observed, "observed master head");
  if (expected !== observed) throw new StaleHeadError(expected, observed, phase);
  return expected;
}

export async function runWhileHeadIsCurrent(
  head,
  operation,
  {
    phase = "long-running master operation",
    pollMs,
    readAuthoritativeHead,
    signal,
  } = {},
) {
  assertSha1(head, "monitored master head");
  if (typeof operation !== "function" || typeof readAuthoritativeHead !== "function") {
    throw new TypeError("monitored operation and authoritative master reader are required");
  }
  if (!Number.isSafeInteger(pollMs) || pollMs < 1) {
    throw new Error("authoritative master monitor interval must be a positive integer");
  }
  const monitorController = new AbortController();
  const combinedSignal = signal
    ? AbortSignal.any([signal, monitorController.signal])
    : monitorController.signal;
  const work = Promise.resolve().then(() => operation(combinedSignal));
  const monitor = (async () => {
    while (true) {
      assertFreshHead(head, await readAuthoritativeHead(combinedSignal), phase);
      await abortableDelay(pollMs, combinedSignal);
    }
  })();
  try {
    return await Promise.race([work, monitor]);
  } finally {
    monitorController.abort(new DOMException("master monitor completed", "AbortError"));
    await Promise.allSettled([work, monitor]);
  }
}

export function assertCheckoutState(state, authoritativeHead) {
  if (state == null || typeof state !== "object") {
    throw new Error("Git checkout state is missing");
  }
  if (state.ref !== "refs/heads/master") {
    throw new Error(`trusted CI requires an attached master checkout; observed ${state.ref ?? "detached HEAD"}`);
  }
  if (state.status !== "") {
    throw new Error("trusted CI requires a clean tracked and untracked checkout");
  }
  assertFreshHead(authoritativeHead, state.trackingHead, "fetched origin/master");
  assertFreshHead(authoritativeHead, state.head, "local master checkout");
  return state.head;
}

export function parseLsRemote(output, requiredRefs) {
  if (typeof output !== "string" || !Array.isArray(requiredRefs) || requiredRefs.length === 0) {
    throw new TypeError("git ls-remote output and required refs are required");
  }
  const expected = new Set(requiredRefs);
  if (expected.size !== requiredRefs.length) throw new Error("required Git refs must be unique");
  const observed = new Map();
  for (const line of output.trim().split("\n").filter(Boolean)) {
    const match = /^([a-f0-9]{40})\t([^\t\r\n]+)$/.exec(line);
    if (!match || !expected.has(match[2]) || observed.has(match[2])) {
      throw new Error("git ls-remote returned unexpected output");
    }
    observed.set(match[2], match[1]);
  }
  for (const ref of requiredRefs) {
    if (!observed.has(ref)) throw new Error(`git ls-remote omitted ${ref}`);
  }
  return observed;
}

export function parseMasterSourceState(value, expectedHead) {
  assertSha1(expectedHead, "expected master source head");
  if (!isRecord(value) || !isRecord(value.publication) || !isRecord(value.run)) {
    throw new Error("Cloudflare CI returned invalid master source state");
  }
  const publication = value.publication;
  const run = value.run;
  if (
    publication.version !== 1 ||
    publication.head !== expectedHead ||
    publication.branch !== "master" ||
    publication.ref !== "refs/heads/master" ||
    !isRecord(publication.lane) ||
    publication.lane.type !== "master" ||
    Object.keys(publication.lane).length !== 1 ||
    run.version !== 1 ||
    run.head !== expectedHead ||
    run.workflowId !== `ci-${expectedHead}` ||
    !["pending", "dispatched"].includes(run.state)
  ) throw new Error("Cloudflare CI source state is not the exact master lane");
  return { publication, run };
}

export function parseCiRunState(value, expectedHead) {
  assertSha1(expectedHead, "expected CI head");
  const expectedWorkflowId = `ci-${expectedHead}`;
  if (
    !isRecord(value) ||
    value.version !== 1 ||
    value.head !== expectedHead ||
    value.workflowId !== expectedWorkflowId ||
    !["pending", "dispatched"].includes(value.state)
  ) {
    throw new Error("Cloudflare CI returned a run for the wrong head");
  }
  if (!isRecord(value.workflow) || typeof value.workflow.status !== "string") {
    throw new Error("Cloudflare CI returned an invalid workflow state");
  }
  const workflowStatus = value.workflow.status;
  if (!terminalWorkflowStates.has(workflowStatus) && !activeWorkflowStates.has(workflowStatus)) {
    throw new Error(`Cloudflare CI returned an unknown workflow state: ${workflowStatus}`);
  }
  const result = value.result;
  let resultStatus = null;
  if (result != null) {
    if (
      !isRecord(result) ||
      result.version !== 1 ||
      result.head !== expectedHead ||
      result.workflowId !== expectedWorkflowId ||
      !["running", "success", "failure", "terminated"].includes(result.status)
    ) throw new Error("Cloudflare CI returned an invalid result state");
    resultStatus = result.status;
  }

  let outcome = "pending";
  if (workflowStatus === "complete") {
    if (resultStatus !== "success") {
      throw new Error("completed Cloudflare CI workflow has no successful result");
    }
    outcome = "success";
  } else if (workflowStatus === "errored") {
    outcome = "failure";
  } else if (workflowStatus === "terminated") {
    outcome = "terminated";
  }
  return { head: expectedHead, workflowStatus, resultStatus, outcome, result, value };
}

export function selectWebDistArtifact(runState) {
  const { head, outcome, result } = runState;
  if (outcome !== "success" || !isRecord(result) || !Array.isArray(result.artifacts)) {
    throw new Error("successful CI result has no artifact declaration");
  }
  const key = `runs/${head}/artifacts/web-dist.tar`;
  const matches = result.artifacts.filter((artifact) => isRecord(artifact) && artifact.key === key);
  if (matches.length !== 1) {
    throw new Error("successful CI result must declare exactly one web-dist artifact");
  }
  const artifact = matches[0];
  if (
    !Number.isSafeInteger(artifact.size) ||
    artifact.size <= 0 ||
    artifact.size > maximumArtifactBytes ||
    typeof artifact.sha256 !== "string" ||
    !sha256Pattern.test(artifact.sha256) ||
    artifact.contentType !== "application/x-tar"
  ) throw new Error("CI web-dist artifact declaration is invalid");
  return {
    key,
    size: artifact.size,
    sha256: artifact.sha256,
    contentType: artifact.contentType,
  };
}

export function validateArtifactHeaders(headers, artifact, head) {
  assertSha1(head, "artifact head");
  if (!(headers instanceof Headers)) throw new TypeError("artifact headers must be Headers");
  const expectedDisposition = `attachment; filename="nanocodex-${head}-web-dist.tar"`;
  const size = parseCanonicalInteger(headers.get("content-length"));
  if (
    size !== artifact.size ||
    headers.get("content-type") !== artifact.contentType ||
    headers.get("x-nanocodex-sha256") !== artifact.sha256 ||
    headers.get("content-disposition") !== expectedDisposition ||
    headers.get("x-content-type-options") !== "nosniff" ||
    !/(?:^|,)\s*immutable(?:,|$)/.test(headers.get("cache-control") ?? "") ||
    headers.has("content-encoding") ||
    headers.has("content-range")
  ) throw new Error("CI web-dist response headers do not match its declaration");
  return artifact;
}

export function verifyArtifactBytes(bytes, artifact) {
  if (!Buffer.isBuffer(bytes)) throw new TypeError("artifact body must be a Buffer");
  if (bytes.byteLength !== artifact.size) {
    throw new Error(`CI web-dist body is ${bytes.byteLength} bytes; expected ${artifact.size}`);
  }
  const sha256 = createHash("sha256").update(bytes).digest("hex");
  if (sha256 !== artifact.sha256) throw new Error("CI web-dist SHA-256 does not match its declaration");
  return bytes;
}

export function safeTarPath(value) {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    value.includes("\0") ||
    value.includes("\\") ||
    /[\u0000-\u001f\u007f]/.test(value) ||
    value.startsWith("/")
  ) throw new Error(`unsafe web-dist archive path: ${JSON.stringify(value)}`);

  let stripped = value;
  while (stripped.startsWith("./")) stripped = stripped.slice(2);
  stripped = stripped.replace(/\/+$/, "");
  if (stripped === "" || stripped === ".") return "";
  const parts = stripped.split("/");
  if (
    parts.some((part) => part === "" || part === "." || part === "..") ||
    /^[A-Za-z]:$/.test(parts[0]) ||
    posix.normalize(stripped) !== stripped
  ) throw new Error(`unsafe web-dist archive path: ${JSON.stringify(value)}`);
  return stripped;
}

export function inspectTarArchive(bytes) {
  if (!Buffer.isBuffer(bytes) || bytes.byteLength === 0 || bytes.byteLength > maximumArtifactBytes) {
    throw new Error("web-dist archive has an invalid size");
  }
  if (bytes.byteLength % 512 !== 0) throw new Error("web-dist tar is not block aligned");
  const entries = [];
  const paths = new Map();
  let offset = 0;
  let zeroBlocks = 0;
  let payloadBytes = 0;
  while (offset < bytes.byteLength) {
    const header = bytes.subarray(offset, offset + 512);
    if (header.every((byte) => byte === 0)) {
      zeroBlocks += 1;
      offset += 512;
      if (zeroBlocks >= 2) {
        if (!bytes.subarray(offset).every((byte) => byte === 0)) {
          throw new Error("web-dist tar has data after its end marker");
        }
        offset = bytes.byteLength;
      }
      continue;
    }
    if (zeroBlocks !== 0) throw new Error("web-dist tar has an incomplete end marker");
    if (entries.length >= maximumTarEntries) {
      throw new Error(`web-dist tar exceeds ${maximumTarEntries} entries`);
    }
    validateTarHeaderChecksum(header);
    const magic = header.subarray(257, 263).toString("latin1");
    if (magic !== "ustar\0" && magic !== "ustar ") {
      throw new Error("web-dist tar must use the ustar format");
    }
    const name = tarString(header.subarray(0, 100), "name");
    const prefix = tarString(header.subarray(345, 500), "prefix");
    const path = safeTarPath(prefix ? `${prefix}/${name}` : name);
    const size = tarOctal(header.subarray(124, 136), "size");
    const mode = tarOctal(header.subarray(100, 108), "mode");
    const typeByte = header[156];
    const type = typeByte === 0 || typeByte === 0x30
      ? "file"
      : typeByte === 0x35
        ? "directory"
        : undefined;
    if (!type) throw new Error(`web-dist tar rejects entry type ${String.fromCharCode(typeByte)}`);
    if (type === "directory" && size !== 0) {
      throw new Error(`web-dist directory has a body: ${path}`);
    }
    if (tarString(header.subarray(157, 257), "link name") !== "") {
      throw new Error(`web-dist tar entry has a link target: ${path}`);
    }
    const bodyStart = offset + 512;
    const bodyEnd = bodyStart + size;
    const next = bodyStart + Math.ceil(size / 512) * 512;
    if (bodyEnd > bytes.byteLength || next > bytes.byteLength) {
      throw new Error(`web-dist tar entry is truncated: ${path}`);
    }
    if (!bytes.subarray(bodyEnd, next).every((byte) => byte === 0)) {
      throw new Error(`web-dist tar entry has nonzero padding: ${path}`);
    }
    if (path === "") {
      if (type !== "directory") throw new Error("web-dist archive root must be a directory");
    } else {
      validateTarPathTopology(paths, path, type);
      const body = bytes.subarray(bodyStart, bodyEnd);
      entries.push({ path, type, mode, body });
      paths.set(path, type);
      payloadBytes += size;
      if (payloadBytes > maximumArtifactBytes) {
        throw new Error("web-dist tar expands beyond its artifact size limit");
      }
    }
    offset = next;
  }
  if (zeroBlocks < 2) throw new Error("web-dist tar has no complete end marker");
  if (!paths.has("nanocodex/wrangler.json") || paths.get("nanocodex/wrangler.json") !== "file") {
    throw new Error("web-dist tar has no nanocodex/wrangler.json deployment config");
  }
  if (!paths.has("client/index.html") || paths.get("client/index.html") !== "file") {
    throw new Error("web-dist tar has no website entry document");
  }
  return entries;
}

export async function extractTarArchive(bytes, destination) {
  const entries = inspectTarArchive(bytes);
  await mkdir(destination, { recursive: false, mode: 0o700 });
  const root = `${resolve(destination)}${sep}`;
  for (const entry of entries) {
    const target = resolve(destination, ...entry.path.split("/"));
    if (!target.startsWith(root)) throw new Error(`archive target escaped extraction root: ${entry.path}`);
    if (entry.type === "directory") {
      await mkdir(target, { recursive: true, mode: 0o755 });
      continue;
    }
    await mkdir(dirname(target), { recursive: true, mode: 0o755 });
    const handle = await open(
      target,
      fsConstants.O_CREAT | fsConstants.O_EXCL | fsConstants.O_WRONLY |
        (fsConstants.O_NOFOLLOW ?? 0),
      entry.mode & 0o111 ? 0o755 : 0o644,
    );
    try {
      await handle.writeFile(entry.body);
    } finally {
      await handle.close();
    }
  }
  return entries;
}

export function githubStatusPayload(state, head, origin) {
  assertSha1(head, "GitHub status head");
  if (!["pending", "success", "failure", "error"].includes(state)) {
    throw new Error(`unsupported GitHub status state: ${state}`);
  }
  const descriptions = {
    pending: "Cloudflare CI is running",
    success: "Cloudflare CI passed; production is verified",
    failure: "Cloudflare CI failed",
    error: "Trusted CI controller failed",
  };
  return {
    state,
    context: githubStatusContext,
    description: descriptions[state],
    target_url: new URL(`/api/ci/runs/${head}`, parseOrigin(origin)).href,
  };
}

export async function runControllerCycle(operations, { signal } = {}) {
  const head = await operations.synchronizeCheckout(signal);
  assertSha1(head, "synchronized master head");
  let pendingPublished = false;
  try {
    const toolchain = await operations.prepareToolchain(head, signal);
    await operations.updateStatus(head, "pending", signal);
    pendingPublished = true;
    const rustSecRevision = await operations.publishRustSec(head, toolchain, signal);
    assertSha1(rustSecRevision, "RustSec revision");
    const cargoVendor = await operations.publishCargoVendor(head, toolchain, signal);
    assertSha256(cargoVendor?.sha256, "Cargo vendor bundle");
    await operations.publishSource(
      head,
      rustSecRevision,
      cargoVendor.sha256,
      toolchain,
      signal,
    );
    const run = await operations.waitForRun(head, signal);
    if (run.outcome !== "success") {
      const detail = failureDetail(run);
      throw new CiRunTerminalError(head, run.outcome, detail);
    }
    const artifact = selectWebDistArtifact(run);
    await operations.assertFresh(head, "before promotion", signal);
    if (await operations.isPromoted(head, signal)) {
      await operations.assertFresh(head, "after promotion inspection", signal);
      await operations.updateStatus(head, "success", signal);
      return { head, action: "already-promoted", artifact };
    }
    const bytes = await operations.downloadArtifact(head, artifact, signal);
    await operations.installArtifact(bytes, artifact, signal);
    await operations.assertFresh(head, "before deploy", signal);
    await operations.deploy(head, toolchain, signal);
    await operations.assertFresh(head, "after deploy", signal);
    await operations.publishRepository(head, toolchain, signal);
    await operations.assertFresh(head, "after repository publication", signal);
    await operations.verifyPromotion(head, signal);
    await operations.assertFresh(head, "after promotion verification", signal);
    await operations.updateStatus(head, "success", signal);
    return { head, action: "promoted", artifact };
  } catch (cause) {
    if (!isAbort(cause) && !(cause instanceof StaleHeadError) && pendingPublished) {
      const status = cause?.githubState === "failure" ? "failure" : "error";
      try {
        await operations.assertFresh(head, "before terminal status", signal);
        await operations.updateStatus(head, status, signal);
      } catch (statusCause) {
        if (statusCause instanceof StaleHeadError) throw statusCause;
        throw new AggregateError(
          [cause, statusCause],
          "controller operation and final GitHub status both failed",
        );
      }
    }
    throw cause;
  }
}

export async function publishPreparedMasterCargoVendor(
  operations,
  head,
  { signal } = {},
) {
  assertSha1(head, "master Cargo vendor head");
  if (
    typeof operations?.build !== "function" ||
    typeof operations.assertFresh !== "function" ||
    typeof operations.upload !== "function"
  ) throw new TypeError("master Cargo publication requires build, freshness, and upload phases");
  const artifact = await operations.build(signal);
  try {
    const descriptor = artifact?.descriptor;
    if (
      !isRecord(descriptor) || descriptor.head !== head ||
      !sha1Pattern.test(descriptor.cargoLockBlob) ||
      !sha256Pattern.test(descriptor.sha256) ||
      !Number.isSafeInteger(descriptor.size) || descriptor.size <= 0 ||
      descriptor.key !==
        `cargo-vendor/${descriptor.cargoLockBlob}/${descriptor.sha256}/bundle.tar.gz`
    ) throw new Error("master Cargo builder returned the wrong exact descriptor");
    await operations.assertFresh(signal);
    await operations.upload(artifact, signal);
    await operations.assertFresh(signal);
    return descriptor;
  } finally {
    await artifact?.cleanup?.();
  }
}

export async function runRepositoryRepair(operations, requestedHead, { signal } = {}) {
  assertSha1(requestedHead, "repository repair head");
  const localHead = await operations.synchronizeCheckout(signal);
  assertFreshHead(requestedHead, localHead, "repository repair local HEAD");
  const toolchain = await operations.prepareToolchain(requestedHead, signal);
  await operations.assertFresh(requestedHead, "before repository repair deployment proof", signal);
  await operations.verifyDeployment(requestedHead, signal);
  await operations.assertFresh(requestedHead, "before repository repair publication", signal);
  await operations.publishRepository(requestedHead, toolchain, signal, { repair: true });
  await operations.assertFresh(requestedHead, "after repository repair publication", signal);
  await operations.verifyPromotion(requestedHead, signal);
  await operations.assertFresh(requestedHead, "after repository repair verification", signal);
  return { head: requestedHead, action: "repository-repaired" };
}

export function exclusiveLockCommand(platform = process.platform) {
  if (platform === "darwin") {
    return {
      command: "/usr/bin/lockf",
      args: ["-s", "-t", "0", "3"],
    };
  }
  if (platform === "linux") {
    return {
      command: "flock",
      args: ["--exclusive", "--nonblock", "--conflict-exit-code", "75", "3"],
    };
  }
  throw new Error(`trusted CI controller locking is unsupported on ${platform}`);
}

export async function acquireExclusiveLock(path, owner = {}, {
  env = process.env,
  platform = process.platform,
} = {}) {
  const noFollow = fsConstants.O_NOFOLLOW;
  if (typeof noFollow !== "number") {
    throw new Error("trusted CI controller requires O_NOFOLLOW lock-file support");
  }
  let handle;
  try {
    handle = await open(
      path,
      fsConstants.O_CREAT | fsConstants.O_RDWR | noFollow,
      0o600,
    );
  } catch (cause) {
    throw new Error(`trusted CI controller could not open its lock at ${path}`, { cause });
  }
  try {
    const identity = await handle.stat();
    if (!identity.isFile()) throw new Error("controller lock path is not a regular file");
    const command = exclusiveLockCommand(platform);
    const result = await runLockCommand(command, handle.fd, runtimeEnvironment(env));
    if (result.code === 75) {
      throw new Error(`trusted CI controller is already locked at ${path}`);
    }
    if (result.code !== 0) {
      const detail = result.stderr.trim().slice(0, 1_000);
      throw new Error(
        `${command.command} could not acquire the controller lock ` +
          `(exit ${result.code ?? result.signal})${detail ? `: ${detail}` : ""}`,
      );
    }
    await handle.truncate(0);
    await handle.writeFile(`${JSON.stringify({
      version: 1,
      pid: process.pid,
      startedAt: new Date().toISOString(),
      ...owner,
    })}\n`);
  } catch (cause) {
    await handle.close().catch(() => undefined);
    throw cause;
  }
  let released = false;
  return async () => {
    if (released) return;
    released = true;
    await handle.close();
  };
}

export function assertRustSecCheckoutState(state, expectedHead) {
  if (state == null || typeof state !== "object") {
    throw new Error("RustSec checkout state is missing");
  }
  if (
    !Array.isArray(state.remoteUrls) ||
    state.remoteUrls.length !== 1 ||
    state.remoteUrls[0] !== authoritativeRustSecRepositoryUrl
  ) {
    throw new Error(
      `RustSec origin must be the authoritative official HTTPS remote: ` +
        authoritativeRustSecRepositoryUrl,
    );
  }
  if (state.ref !== "refs/heads/main") {
    throw new Error(
      `RustSec publication requires an attached main checkout; observed ` +
        `${state.ref ?? "detached HEAD"}`,
    );
  }
  if (state.status !== "") {
    throw new Error("RustSec publication requires a clean tracked and untracked checkout");
  }
  assertSha1(state.head, "RustSec HEAD");
  if (expectedHead !== undefined) {
    assertSha1(expectedHead, "authoritative RustSec main head");
    assertFreshHead(expectedHead, state.head, "RustSec local main");
    assertFreshHead(expectedHead, state.trackingHead, "RustSec origin/main");
  }
  return state.head;
}

export async function refreshRustSecCheckout(git, { signal } = {}) {
  if (typeof git !== "function") throw new TypeError("RustSec Git runner is required");
  await assertSafeLocalGitConfig(git, "RustSec", signal);
  const initial = await readRustSecCheckoutState(git, signal);
  assertRustSecCheckoutState(initial);
  await git([
    "fetch",
    "--no-tags",
    "--no-recurse-submodules",
    authoritativeRustSecRepositoryUrl,
    "refs/heads/main:refs/remotes/origin/main",
  ], signal);
  const tracking = (await git([
    "rev-parse", "--verify", "refs/remotes/origin/main^{commit}",
  ], signal)).stdout.trimEnd();
  assertSha1(tracking, "fetched RustSec origin/main");
  if (initial.head !== tracking) {
    try {
      await git(["merge-base", "--is-ancestor", initial.head, tracking], signal);
    } catch (cause) {
      if (cause?.exitCode === 1) {
        throw new Error(
          "RustSec local main cannot fast-forward to authoritative origin/main",
          { cause },
        );
      }
      throw cause;
    }
    await git(["merge", "--ff-only", "--no-edit", tracking], signal);
  }
  return verifyRustSecCheckout(git, tracking, signal);
}

async function verifyRustSecCheckout(git, expectedHead, signal) {
  await assertSafeLocalGitConfig(git, "RustSec", signal);
  const state = await readRustSecCheckoutState(git, signal, true);
  return assertRustSecCheckoutState(state, expectedHead);
}

async function readRustSecCheckoutState(git, signal, includeTracking = false) {
  const reads = [
    git(["config", "--local", "--no-includes", "--get-all", "remote.origin.url"], signal),
    git(["symbolic-ref", "--quiet", "HEAD"], signal),
    git(["rev-parse", "--verify", "HEAD^{commit}"], signal),
    git([
      "status",
      "--porcelain=v1",
      "-z",
      "--untracked-files=all",
      "--ignored=matching",
      "--ignore-submodules=none",
    ], signal),
  ];
  if (includeTracking) {
    reads.push(git([
      "rev-parse", "--verify", "refs/remotes/origin/main^{commit}",
    ], signal));
  }
  const [remote, ref, head, status, tracking] = await Promise.all(reads);
  const remoteText = remote.stdout.endsWith("\n")
    ? remote.stdout.slice(0, -1)
    : remote.stdout;
  return {
    remoteUrls: remoteText === "" ? [] : remoteText.split("\n"),
    ref: ref.stdout.trimEnd(),
    head: head.stdout.trimEnd(),
    status: status.stdout,
    trackingHead: tracking?.stdout.trimEnd(),
  };
}

async function assertSafeLocalGitConfig(git, context, signal) {
  const local = await git([
    "config", "--local", "--no-includes", "--name-only", "--list",
  ], signal);
  const names = local.stdout.split("\n").filter(Boolean);
  if (names.some((name) => name.toLowerCase() === "extensions.worktreeconfig")) {
    const worktree = await git([
      "config", "--worktree", "--no-includes", "--name-only", "--list",
    ], signal);
    names.push(...worktree.stdout.split("\n").filter(Boolean));
  }
  const safe = /^(?:core\.(?:repositoryformatversion|filemode|bare|logallrefupdates|ignorecase|precomposeunicode|autocrlf)|user\.(?:name|email)|extensions\.worktreeconfig|remote\.[^.]+\.(?:url|fetch)|branch\.[^.]+\.(?:remote|merge)|submodule\..+\.url|worktrunk\..+)$/i;
  const unsafe = names.filter((name) => !safe.test(name));
  if (unsafe.length > 0) {
    throw new Error(`${context} rejects local Git configuration: ${unsafe.join(", ")}`);
  }
}

export function controllerConfiguration(
  env = process.env,
  { requireGithubStatus = true, requireRustSec = true } = {},
) {
  const repository = resolve(env.NANOCODEX_REPO ?? defaultRepository);
  const ciOrigin = parseOrigin(requiredEnvironment(env, "NANOCODEX_CI_ORIGIN"));
  const webOrigin = parseOrigin(env.NANOCODEX_WEB_ORIGIN ?? ciOrigin);
  const rustSecPath = requireRustSec
    ? requiredEnvironment(env, "NANOCODEX_RUSTSEC_REPO")
    : optionalEnvironment(env, "NANOCODEX_RUSTSEC_REPO");
  return {
    env,
    repository,
    rustSecRepository: rustSecPath ? resolve(rustSecPath) : undefined,
    ciOrigin,
    webOrigin,
    githubToken: requireGithubStatus
      ? requiredEnvironment(env, "NANOCODEX_GITHUB_STATUS_TOKEN")
      : undefined,
    intervalMs: boundedIntegerEnvironment(env, "NANOCODEX_CI_CONTROLLER_INTERVAL_MS", 60_000, 1_000, 3_600_000),
    runPollMs: boundedIntegerEnvironment(env, "NANOCODEX_CI_RUN_POLL_MS", 5_000, 250, 60_000),
    masterPollMs: boundedIntegerEnvironment(env, "NANOCODEX_CI_MASTER_POLL_MS", 5_000, 250, 60_000),
    runTimeoutMs: boundedIntegerEnvironment(env, "NANOCODEX_CI_RUN_TIMEOUT_MS", 4 * 60 * 60 * 1_000, 60_000, 8 * 60 * 60 * 1_000),
    requestTimeoutMs: boundedIntegerEnvironment(env, "NANOCODEX_CI_REQUEST_TIMEOUT_MS", 15_000, 1_000, 120_000),
    commandTimeoutMs: boundedIntegerEnvironment(env, "NANOCODEX_CI_COMMAND_TIMEOUT_MS", 45 * 60 * 1_000, 60_000, 2 * 60 * 60 * 1_000),
    verifyTimeoutMs: boundedIntegerEnvironment(env, "NANOCODEX_CI_VERIFY_TIMEOUT_MS", 60_000, 5_000, 10 * 60 * 1_000),
  };
}

async function createOperations(config) {
  const secrets = environmentSecrets(config.env, [config.githubToken]);
  const processOptions = {
    signal: undefined,
    timeoutMs: config.commandTimeoutMs,
    secrets,
  };
  const gitAt = (repository, args, signal, options = {}) => runProcess("git", [
    "-c", "core.fsmonitor=false",
    "-c", "core.hooksPath=/dev/null",
    "-c", "core.attributesFile=/dev/null",
    "-c", "core.autocrlf=false",
    ...args,
  ], {
    cwd: repository,
    env: runtimeEnvironment(config.env),
    ...processOptions,
    ...options,
    signal,
  });
  const git = (args, signal, options) =>
    gitAt(config.repository, args, signal, options);
  const rustSecGit = config.rustSecRepository
    ? (args, signal, options) => gitAt(config.rustSecRepository, args, signal, options)
    : undefined;
  const configuredWebDirectory = resolve(config.repository, "web");
  if (await realpath(configuredWebDirectory) !== await realpath(webDirectory)) {
    throw new Error("trusted CI controller must run from NANOCODEX_REPO/web");
  }
  const npmCli = await resolveTrustedNpmCli();
  const canonicalNode = await realpath(process.execPath);

  const readAuthoritativeHead = async (signal) => {
    const { stdout } = await git([
      "ls-remote",
      "--refs",
      authoritativeRepositoryUrl,
      "refs/heads/master",
    ], signal, { timeoutMs: config.requestTimeoutMs });
    return parseLsRemote(stdout, ["refs/heads/master"]).get("refs/heads/master");
  };

  const whileCurrent = (head, phase, operation, signal) => runWhileHeadIsCurrent(
    head,
    operation,
    {
      phase,
      pollMs: config.masterPollMs,
      readAuthoritativeHead,
      signal,
    },
  );

  const readCheckoutState = async (signal) => {
    const [ref, head, trackingHead, status] = await Promise.all([
      git(["symbolic-ref", "--quiet", "HEAD"], signal),
      git(["rev-parse", "--verify", "HEAD^{commit}"], signal),
      git(["rev-parse", "--verify", "refs/remotes/origin/master^{commit}"], signal),
      git([
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--ignore-submodules=none",
      ], signal),
    ]);
    return {
      ref: ref.stdout.trimEnd(),
      head: head.stdout.trimEnd(),
      trackingHead: trackingHead.stdout.trimEnd(),
      status: status.stdout,
    };
  };

  const synchronizeCheckout = async (signal) => {
    await assertSafeLocalGitConfig(git, "trusted CI", signal);
    const initialRef = await git(["symbolic-ref", "--quiet", "HEAD"], signal);
    if (initialRef.stdout.trimEnd() !== "refs/heads/master") {
      throw new Error("trusted CI requires an attached master checkout before fetching");
    }
    const initialStatus = await git([
      "status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignore-submodules=none",
    ], signal);
    if (initialStatus.stdout !== "") {
      throw new Error("trusted CI refuses to update a dirty checkout");
    }
    await git([
      "fetch",
      "--no-tags",
      "--no-recurse-submodules",
      authoritativeRepositoryUrl,
      "+refs/heads/master:refs/remotes/origin/master",
    ], signal);
    const authoritativeHead = await readAuthoritativeHead(signal);
    const state = await readCheckoutState(signal);
    assertFreshHead(authoritativeHead, state.trackingHead, "fetched origin/master");
    if (state.head !== state.trackingHead) {
      let ancestor = true;
      try {
        await git(["merge-base", "--is-ancestor", state.head, state.trackingHead], signal);
      } catch (cause) {
        if (cause?.exitCode === 1) ancestor = false;
        else throw cause;
      }
      if (!ancestor) throw new Error("local master cannot fast-forward to authoritative origin/master");
      await git(["merge", "--ff-only", "--no-edit", state.trackingHead], signal);
    }
    return assertCheckoutState(await readCheckoutState(signal), authoritativeHead);
  };

  const runPublisher = (name, childEnv, signal, extraFd) => runProcess(canonicalNode, [
    resolve(configuredWebDirectory, "scripts", name),
  ], {
    cwd: config.repository,
    env: childEnv,
    extraFd,
    ...processOptions,
    signal,
  });

  const publishRustSec = async (head, toolchain, signal) => {
    if (!rustSecGit || !config.rustSecRepository) {
      throw new Error("RustSec repository is required for a normal controller cycle");
    }
    const revision = await refreshRustSecCheckout(rustSecGit, { signal });
    await runAuthorityPublisher(
      "publish-ci-rustsec.mjs",
      publicationEnvironment(config.env, "rustsec", {
        rustSecRepository: config.rustSecRepository,
      }),
      head,
      toolchain,
      signal,
    );
    await verifyRustSecCheckout(rustSecGit, revision, signal);
    return revision;
  };

  const updateStatus = async (head, state, signal) => {
    if (!config.githubToken) return;
    const response = await boundedFetch(
      `https://api.github.com/repos/gakonst/nanocodex/statuses/${head}`,
      {
        method: "POST",
        redirect: "error",
        headers: {
          accept: "application/vnd.github+json",
          authorization: `Bearer ${config.githubToken}`,
          "content-type": "application/json",
          "user-agent": "nanocodex-trusted-ci-controller",
          "x-github-api-version": "2022-11-28",
        },
        body: JSON.stringify(githubStatusPayload(state, head, config.webOrigin)),
      },
      signal,
      config.requestTimeoutMs,
    );
    if (response.status !== 201) {
      throw new Error(await responseFailure("publish GitHub commit status", response));
    }
    await response.body?.cancel();
  };

  const assertFresh = async (head, phase, signal) => {
    assertFreshHead(head, await readAuthoritativeHead(signal), phase);
  };

  const verifyTrustedCheckout = async (head, phase, signal) => {
    await assertSafeLocalGitConfig(git, "trusted CI", signal);
    assertFreshHead(head, await readAuthoritativeHead(signal), phase);
    return assertCheckoutState(await readCheckoutState(signal), head);
  };

  const verifyRepositoryFile = async (head, relativePath, phase, signal) => {
    assertSha1(head, "authoritative repository file head");
    if (![
      webLockfileRelativePath,
      webPackageRelativePath,
      ...webCloudflarePatchRelativePaths,
    ].includes(relativePath)) {
      throw new Error(`trusted CI refuses to verify unexpected repository file: ${relativePath}`);
    }
    const path = resolve(config.repository, ...relativePath.split("/"));
    const identity = await lstat(path).catch((cause) => {
      throw new Error(`trusted CI repository file is unavailable: ${relativePath}`, { cause });
    });
    if (!identity.isFile() || identity.isSymbolicLink() || await realpath(path) !== path) {
      throw new Error(`trusted CI repository file must be real: ${relativePath}`);
    }
    let authoritative;
    let worktree;
    try {
      [authoritative, worktree] = await Promise.all([
        git(["rev-parse", "--verify", `${head}:${relativePath}`], signal),
        git(["hash-object", "--no-filters", "--", relativePath], signal),
      ]);
    } catch (cause) {
      throw new Error(`trusted CI could not verify ${relativePath} during ${phase}`, { cause });
    }
    const authoritativeBlob = authoritative.stdout.trimEnd();
    const worktreeBlob = worktree.stdout.trimEnd();
    assertSha1(authoritativeBlob, `authoritative ${relativePath} blob`);
    assertSha1(worktreeBlob, `worktree ${relativePath} blob`);
    if (authoritativeBlob !== worktreeBlob) {
      throw new Error(`trusted CI ${relativePath} differs from authoritative master during ${phase}`);
    }
    return authoritativeBlob;
  };

  const assertToolchain = (head, toolchain, phase, signal) =>
    assertWebDeploymentToolchain({
      attestation: toolchain,
      head,
      phase,
      repository: config.repository,
      signal,
      verifyCheckout: verifyTrustedCheckout,
      verifyRepositoryFile,
      webRoot: configuredWebDirectory,
    });

  const runAuthorityPublisher = async (
    name,
    childEnvironment,
    head,
    toolchain,
    signal,
    extraFd,
  ) => {
    await assertToolchain(head, toolchain, `before ${name}`, signal);
    return whileCurrent(
      head,
      `while ${name} holds external authority`,
      (monitoredSignal) => runPublisher(
        name,
        childEnvironment,
        monitoredSignal,
        extraFd,
      ),
      signal,
    );
  };

  const waitForRun = (head, signal) => whileCurrent(
    head,
    "while waiting for Cloudflare CI",
    async (monitoredSignal) => {
      const run = await pollCloudflareRun({
        origin: config.ciOrigin,
        head,
        signal: monitoredSignal,
        pollMs: config.runPollMs,
        timeoutMs: config.runTimeoutMs,
        requestTimeoutMs: config.requestTimeoutMs,
      });
      await assertFresh(head, "after Cloudflare CI wait", monitoredSignal);
      return run;
    },
    signal,
  );

  const verifyCargoCheckout = (head, phase, signal) =>
    verifyTrustedCheckout(head, `Cargo dependency prefetch: ${phase}`, signal);

  const verifyRepositoryCargoConfig = async (head, name, _path, signal) => {
    assertSha1(head, "authoritative Cargo config head");
    const relativePath = `.cargo/${name}`;
    let authoritative;
    let worktree;
    try {
      [authoritative, worktree] = await Promise.all([
        git(["rev-parse", "--verify", `${head}:${relativePath}`], signal),
        git(["hash-object", "--no-filters", "--", relativePath], signal),
      ]);
    } catch (cause) {
      throw new Error(
        `trusted CI Cargo config must be tracked by authoritative master: ${relativePath}`,
        { cause },
      );
    }
    const authoritativeBlob = authoritative.stdout.trimEnd();
    const worktreeBlob = worktree.stdout.trimEnd();
    assertSha1(authoritativeBlob, `authoritative ${relativePath} blob`);
    assertSha1(worktreeBlob, `worktree ${relativePath} blob`);
    if (authoritativeBlob !== worktreeBlob) {
      throw new Error(`trusted CI Cargo config differs from authoritative master: ${relativePath}`);
    }
  };

  const verifyOnce = (head, signal) => verifyLivePromotion({
    origin: config.webOrigin,
    head,
    repository: config.repository,
    env: config.env,
    signal,
    requestTimeoutMs: config.requestTimeoutMs,
    processOptions,
  });

  const verifyPromotion = (head, signal) => whileCurrent(
    head,
    "while verifying live promotion",
    async (monitoredSignal) => {
      const deadline = Date.now() + config.verifyTimeoutMs;
      let failure;
      do {
        try {
          await verifyOnce(head, monitoredSignal);
        } catch (cause) {
          if (isAbort(cause)) throw cause;
          failure = cause;
          await abortableDelay(
            Math.min(config.runPollMs, Math.max(0, deadline - Date.now())),
            monitoredSignal,
          );
          continue;
        }
        await assertFresh(head, "live promotion verification", monitoredSignal);
        return;
      } while (Date.now() < deadline);
      throw new Error(`live promotion verification timed out: ${errorMessage(failure)}`, {
        cause: failure,
      });
    },
    signal,
  );

  return {
    synchronizeCheckout,
    prepareToolchain: (head, signal) => prepareWebDeploymentToolchain({
      env: config.env,
      head,
      repository: config.repository,
      runNpm: (args, childEnvironment, childSignal) => whileCurrent(
        head,
        "while installing the web deployment toolchain",
        (monitoredSignal) => runProcess(process.execPath, [npmCli, ...args], {
          cwd: config.repository,
          env: childEnvironment,
          ...processOptions,
          signal: monitoredSignal,
        }),
        childSignal,
      ),
      runPatchPackage: (args, childEnvironment, childSignal) => whileCurrent(
        head,
        "while applying the authoritative Cloudflare CI patch",
        (monitoredSignal) => runProcess(process.execPath, args, {
          cwd: configuredWebDirectory,
          env: childEnvironment,
          ...processOptions,
          signal: monitoredSignal,
        }),
        childSignal,
      ),
      signal,
      verifyCheckout: verifyTrustedCheckout,
      verifyRepositoryFile,
      webRoot: configuredWebDirectory,
    }),
    updateStatus,
    publishRustSec,
    publishCargoVendor: (head, toolchain, signal) => publishPreparedMasterCargoVendor({
      build: async (childSignal) => {
        await verifyCargoCheckout(head, "before token-free Cargo build", childSignal);
        return whileCurrent(
          head,
          "while building the master Cargo vendor artifact",
          (monitoredSignal) => captureCargoVendorArtifact({
            args: [cargoBuilderPath, "--build"],
            artifactDirectory: tmpdir(),
            command: canonicalNode,
            cwd: config.repository,
            env: cargoBuilderEnvironment(config.env),
            input: Buffer.from(canonicalCargoVendorJson({
              head,
              kind: "master",
              version: 1,
            })),
            secrets,
            signal: monitoredSignal,
            timeoutMs: config.commandTimeoutMs,
          }),
          childSignal,
        );
      },
      assertFresh: (childSignal) => verifyCargoCheckout(
        head,
        "Cargo vendor phase boundary",
        childSignal,
      ),
      upload: async (artifact, childSignal) => {
        const uploaded = await runAuthorityPublisher(
          "publish-ci-cargo-vendor.mjs",
          publicationEnvironment(config.env, "cargo-vendor"),
          head,
          toolchain,
          childSignal,
          artifact.handle.fd,
        );
        assertUploaderDescriptor(uploaded.stdout, artifact.descriptor);
      },
    }, head, { signal }),
    publishSource: async (
      _head,
      rustSecRevision,
      cargoVendorSha256,
      toolchain,
      signal,
    ) => {
      await runAuthorityPublisher(
        "publish-ci-source.mjs",
        publicationEnvironment(config.env, "source", {
          cargoVendorSha256,
          repository: config.repository,
          rustSecRevision,
        }),
        _head,
        toolchain,
        signal,
      );
      const response = await boundedFetch(
        new URL("/api/ci/source/state", config.ciOrigin),
        {
          headers: {
            accept: "application/json",
            authorization: `Bearer ${requiredEnvironment(config.env, "NANOCODEX_CI_TOKEN")}`,
          },
          redirect: "error",
        },
        signal,
        config.requestTimeoutMs,
      );
      if (response.status !== 200) {
        throw new Error(await responseFailure("verify master CI source state", response));
      }
      parseMasterSourceState(
        await readJsonResponse(response, maximumJsonBytes, "master CI source state"),
        _head,
      );
      await assertFresh(_head, "after source publication verification", signal);
    },
    waitForRun,
    assertFresh,
    isPromoted: async (head, signal) => {
      try {
        await verifyOnce(head, signal);
        return true;
      } catch (cause) {
        if (isAbort(cause)) throw cause;
        return false;
      }
    },
    downloadArtifact: (head, artifact, signal) => downloadWebDist({
      origin: config.ciOrigin,
      head,
      artifact,
      signal,
      requestTimeoutMs: config.requestTimeoutMs,
    }),
    installArtifact: (bytes) => installWebDist(bytes, configuredWebDirectory),
    deploy: async (head, toolchain, signal) => {
      await runAuthorityPublisher(
        "deploy-worker.mjs",
        deploymentEnvironment(config.env, head, { origin: config.webOrigin }),
        head,
        toolchain,
        signal,
      );
    },
    publishRepository: async (_head, toolchain, signal, { repair = false } = {}) => {
      await runAuthorityPublisher(
        "publish-repository.mjs",
        repositoryEnvironment(config.env, {
          repository: config.repository,
          origin: config.webOrigin,
          repair,
        }),
        _head,
        toolchain,
        signal,
      );
    },
    verifyDeployment: (head, signal) => verifyLiveDeploymentHead({
      origin: config.webOrigin,
      head,
      signal,
      requestTimeoutMs: config.requestTimeoutMs,
    }),
    verifyPromotion,
  };
}

async function pollCloudflareRun({
  origin,
  head,
  signal,
  pollMs,
  timeoutMs,
  requestTimeoutMs,
}) {
  const deadline = Date.now() + timeoutMs;
  let lastTransientFailure;
  while (Date.now() < deadline) {
    throwIfAborted(signal);
    let response;
    try {
      response = await boundedFetch(
        new URL(`/api/ci/runs/${head}`, origin),
        { headers: { accept: "application/json" }, redirect: "error" },
        signal,
        requestTimeoutMs,
      );
      if (response.status === 404 || response.status === 425 || response.status === 429 || response.status >= 500) {
        lastTransientFailure = new Error(`read Cloudflare CI run returned HTTP ${response.status}`);
        await response.body?.cancel();
      } else {
        if (!response.ok) throw new Error(await responseFailure("read Cloudflare CI run", response));
        const value = await readJsonResponse(response, maximumJsonBytes, "Cloudflare CI run");
        const state = parseCiRunState(value, head);
        if (terminalWorkflowStates.has(state.workflowStatus)) {
          if (state.outcome === "success") return state;
          throw new CiRunTerminalError(head, state.outcome, failureDetail(state));
        }
      }
    } catch (cause) {
      if (isAbort(cause) || cause instanceof CiRunTerminalError) throw cause;
      if (!(cause instanceof TypeError) && cause?.name !== "TimeoutError") throw cause;
      lastTransientFailure = cause;
    }
    await abortableDelay(Math.min(pollMs, Math.max(0, deadline - Date.now())), signal);
  }
  throw new Error(
    `Cloudflare CI did not reach a terminal state within ${timeoutMs}ms` +
      (lastTransientFailure ? `: ${errorMessage(lastTransientFailure)}` : ""),
    { cause: lastTransientFailure },
  );
}

async function downloadWebDist({ origin, head, artifact, signal, requestTimeoutMs }) {
  const url = new URL(`/api/ci/runs/${head}/artifacts/web-dist.tar`, origin);
  const headResponse = await boundedFetch(url, {
    method: "HEAD",
    headers: { accept: "application/x-tar" },
    redirect: "error",
  }, signal, requestTimeoutMs);
  if (headResponse.status !== 200) {
    throw new Error(await responseFailure("inspect CI web-dist artifact", headResponse));
  }
  validateArtifactHeaders(headResponse.headers, artifact, head);
  await headResponse.body?.cancel();

  const response = await boundedFetch(url, {
    headers: { accept: "application/x-tar" },
    redirect: "error",
  }, signal, requestTimeoutMs);
  if (response.status !== 200) {
    throw new Error(await responseFailure("download CI web-dist artifact", response));
  }
  validateArtifactHeaders(response.headers, artifact, head);
  const bytes = await readResponseBuffer(response, artifact.size, "CI web-dist artifact");
  verifyArtifactBytes(bytes, artifact);
  inspectTarArchive(bytes);
  return bytes;
}

async function installWebDist(bytes, webRoot) {
  const staging = await mkdtemp(resolve(webRoot, ".ci-controller-dist-"));
  const extracted = resolve(staging, "extracted");
  const previous = resolve(staging, "previous");
  const target = resolve(webRoot, "dist");
  let movedPrevious = false;
  let installed = false;
  try {
    await extractTarArchive(bytes, extracted);
    const [configuration, entry] = await Promise.all([
      stat(resolve(extracted, "nanocodex", "wrangler.json")),
      stat(resolve(extracted, "client", "index.html")),
    ]);
    if (!configuration.isFile() || !entry.isFile()) {
      throw new Error("extracted web-dist is missing deployment files");
    }
    try {
      await rename(target, previous);
      movedPrevious = true;
    } catch (cause) {
      if (cause?.code !== "ENOENT") throw cause;
    }
    try {
      await rename(extracted, target);
      installed = true;
    } catch (cause) {
      if (movedPrevious) await rename(previous, target).catch(() => undefined);
      throw cause;
    }
    if (movedPrevious) await rm(previous, { recursive: true, force: true });
  } finally {
    if (!installed && movedPrevious) {
      const current = await lstat(target).catch(() => undefined);
      if (!current) await rename(previous, target).catch(() => undefined);
    }
    await rm(staging, { recursive: true, force: true });
  }
}

async function verifyLiveDeploymentHead({ origin, head, signal, requestTimeoutMs }) {
  const response = await boundedFetch(
    new URL("/api/health", origin),
    {
      cache: "no-store",
      headers: { accept: "application/json" },
      redirect: "error",
    },
    signal,
    requestTimeoutMs,
  );
  if (response.status !== 200) {
    throw new Error(await responseFailure("verify deployment health", response));
  }
  const health = await readJsonResponse(response, maximumJsonBytes, "deployment health");
  if (!isRecord(health) || health.status !== "ok" || health.deployment_sha !== head) {
    throw new Error("live Worker does not attest the requested repository repair head");
  }
}

async function verifyLivePromotion({
  origin,
  head,
  repository,
  env,
  signal,
  requestTimeoutMs,
  processOptions,
}) {
  const healthUrl = new URL("/api/health", origin);
  healthUrl.searchParams.set("revision", head);
  const snapshotUrl = new URL("/api/repository/snapshot", origin);
  snapshotUrl.searchParams.set("revision", head);
  const [healthResponse, snapshotResponse] = await Promise.all([
    boundedFetch(healthUrl, {
      headers: { accept: "application/json" },
      redirect: "error",
    }, signal, requestTimeoutMs),
    boundedFetch(snapshotUrl, {
      headers: { accept: "application/json" },
      redirect: "error",
    }, signal, requestTimeoutMs),
  ]);
  if (healthResponse.status !== 200) {
    await snapshotResponse.body?.cancel();
    throw new Error(await responseFailure("verify deployment health", healthResponse));
  }
  if (snapshotResponse.status !== 200) {
    await healthResponse.body?.cancel();
    throw new Error(await responseFailure("verify repository snapshot", snapshotResponse));
  }
  const [health, snapshot] = await Promise.all([
    readJsonResponse(healthResponse, maximumJsonBytes, "deployment health"),
    readJsonResponse(snapshotResponse, maximumJsonBytes, "repository snapshot"),
  ]);
  if (!isRecord(health) || health.status !== "ok" || health.deployment_sha !== head) {
    throw new Error("live Worker does not attest the promoted head");
  }
  if (!isRecord(snapshot) || !isRecord(snapshot.repository) || snapshot.repository.head !== head) {
    throw new Error("live repository snapshot does not attest the promoted head");
  }
  const { stdout } = await runProcess("git", [
    "-c", "core.hooksPath=/dev/null",
    "-c", "credential.helper=",
    "ls-remote",
    new URL("/git", origin).href,
    "HEAD",
    "refs/heads/master",
  ], {
    cwd: repository,
    env: runtimeEnvironment(env),
    ...processOptions,
    signal,
    timeoutMs: requestTimeoutMs,
  });
  const refs = parseLsRemote(stdout, ["HEAD", "refs/heads/master"]);
  assertFreshHead(head, refs.get("HEAD"), "live Git HEAD");
  assertFreshHead(head, refs.get("refs/heads/master"), "live Git master");
}

async function controllerLockPath(repository, env, signal, secrets) {
  const { stdout } = await runProcess("git", [
    "rev-parse", "--path-format=absolute", "--git-common-dir",
  ], {
    cwd: repository,
    env: runtimeEnvironment(env),
    signal,
    timeoutMs: 10_000,
    secrets,
  });
  const gitDirectory = stdout.trimEnd();
  if (!gitDirectory.startsWith("/")) throw new Error("Git returned a non-absolute common directory");
  return resolve(gitDirectory, "nanocodex-ci-controller.lock");
}

export function redactSecrets(value, secrets) {
  let text = String(value);
  for (const secret of [...new Set(secrets.filter((entry) => typeof entry === "string" && entry.length > 0))]
    .sort((left, right) => right.length - left.length)) {
    text = text.replaceAll(secret, "[redacted]");
  }
  return text;
}

async function runLockCommand({ command, args }, descriptor, env) {
  const child = spawn(command, args, {
    env,
    detached: false,
    stdio: ["ignore", "ignore", "pipe", descriptor],
  });
  const stderr = [];
  let stderrBytes = 0;
  child.stderr.on("data", (chunk) => {
    stderrBytes += chunk.length;
    if (stderrBytes <= maximumProcessOutputBytes) stderr.push(Buffer.from(chunk));
  });
  const result = await new Promise((resolveProcess, reject) => {
    child.once("error", reject);
    child.once("close", (code, signal) => resolveProcess({ code, signal }));
  });
  return {
    ...result,
    stderr: Buffer.concat(stderr).toString("utf8"),
  };
}

async function runProcess(command, args, {
  cwd,
  env,
  extraFd,
  signal,
  timeoutMs = 60_000,
  secrets = [],
} = {}) {
  throwIfAborted(signal);
  if (extraFd != null && (!Number.isSafeInteger(extraFd) || extraFd < 0)) {
    throw new Error("inherited process fd must be a nonnegative integer");
  }
  const child = spawn(command, args, {
    cwd,
    env,
    detached: process.platform !== "win32",
    stdio: extraFd == null
      ? ["ignore", "pipe", "pipe"]
      : ["ignore", "pipe", "pipe", extraFd],
  });
  const processGroupId = process.platform === "win32" ? undefined : child.pid;
  const stdout = [];
  const stderr = [];
  let stdoutBytes = 0;
  let stderrBytes = 0;
  let failure;
  let cleanupTask;
  let resolveCleanupStarted;
  const cleanupStarted = new Promise((resolvePromise) => {
    resolveCleanupStarted = resolvePromise;
  });
  const beginGroupCleanup = () => {
    if (cleanupTask != null) return;
    cleanupTask = cleanupDetachedProcessGroup(child, processGroupId).then(
      (value) => ({ value }),
      (error) => ({ error }),
    );
    resolveCleanupStarted();
  };
  const fail = (cause) => {
    failure ??= cause;
    beginGroupCleanup();
  };
  const collect = (chunks, kind) => (chunk) => {
    if (kind === "stdout") stdoutBytes += chunk.length;
    else stderrBytes += chunk.length;
    if (stdoutBytes + stderrBytes > maximumProcessOutputBytes) {
      fail(new Error(`${command} output exceeded ${maximumProcessOutputBytes} bytes`));
      return;
    }
    if (failure != null) return;
    chunks.push(Buffer.from(chunk));
  };
  child.stdout.on("data", collect(stdout, "stdout"));
  child.stderr.on("data", collect(stderr, "stderr"));
  const abort = () => {
    fail(signal?.reason ?? new DOMException("operation aborted", "AbortError"));
  };
  signal?.addEventListener("abort", abort, { once: true });
  if (signal?.aborted) abort();
  const timeout = setTimeout(() => {
    fail(new DOMException(`process exceeded ${timeoutMs}ms`, "TimeoutError"));
  }, timeoutMs);
  timeout.unref?.();
  const close = new Promise((resolveProcess) => {
    child.once("close", (code, closeSignal) =>
      resolveProcess({ code, signal: closeSignal }));
  });
  child.once("error", fail);
  child.once("exit", beginGroupCleanup);

  let cleanupOutcome;
  let result;
  let closeFailure;
  try {
    await cleanupStarted;
    cleanupOutcome = await cleanupTask;
    try {
      result = await waitForChildClose(close, command);
    } catch (cause) {
      closeFailure = cause;
    }
  } finally {
    clearTimeout(timeout);
    signal?.removeEventListener("abort", abort);
  }
  const cleanupFailures = [cleanupOutcome?.error, closeFailure].filter(Boolean);
  const cleanupFailure = cleanupFailures.length > 1
    ? new AggregateError(cleanupFailures, "detached process cleanup failed")
    : cleanupFailures[0];
  const abortFailure = signal?.aborted
    ? signal.reason ?? new DOMException("operation aborted", "AbortError")
    : undefined;
  const primaryFailure = abortFailure ?? failure;
  if (primaryFailure) {
    if (cleanupFailure != null) {
      if (primaryFailure instanceof Error) primaryFailure.cleanupCause = cleanupFailure;
      else {
        throw new AggregateError(
          [primaryFailure, cleanupFailure],
          "trusted process abort and cleanup failed",
        );
      }
    }
    throw primaryFailure;
  }
  if (cleanupFailure != null) throw cleanupFailure;
  if (result.code === 0 && cleanupOutcome.value.observedLiveGroup) {
    throw new Error(
      `${command} exited successfully but left a live detached process-group descendant`,
    );
  }
  const stdoutText = Buffer.concat(stdout, stdoutBytes).toString("utf8");
  const stderrText = Buffer.concat(stderr, stderrBytes).toString("utf8");
  if (result.code !== 0) {
    const detail = redactSecrets(stderrText.trim() || stdoutText.trim(), secrets).slice(0, 4_000);
    const error = new Error(
      `${command} exited with ${result.code ?? result.signal}${detail ? `: ${detail}` : ""}`,
    );
    error.exitCode = result.code;
    throw error;
  }
  return { stdout: stdoutText, stderr: stderrText };
}

async function cleanupDetachedProcessGroup(child, processGroupId) {
  if (process.platform === "win32") {
    const observedLiveProcess = child.exitCode == null && child.signalCode == null;
    if (!observedLiveProcess) return { observedLiveGroup: false };
    child.kill("SIGTERM");
    await delayForProcessCleanup(processTerminationGraceMs);
    if (child.exitCode == null && child.signalCode == null) child.kill("SIGKILL");
    return { observedLiveGroup: false };
  }
  if (!Number.isSafeInteger(processGroupId) || processGroupId <= 0) {
    return { observedLiveGroup: false };
  }
  const observedLiveGroup = signalProcessGroup(processGroupId, "SIGTERM");
  if (!observedLiveGroup) return { observedLiveGroup: false };
  if (await waitForProcessGroupExit(processGroupId, processTerminationGraceMs)) {
    return { observedLiveGroup: true };
  }
  signalProcessGroup(processGroupId, "SIGKILL");
  if (!await waitForProcessGroupExit(processGroupId, processKillWaitMs)) {
    throw new Error("detached process group did not exit after SIGKILL");
  }
  return { observedLiveGroup: true };
}

function signalProcessGroup(processGroupId, signalName) {
  try {
    process.kill(-processGroupId, signalName);
    return true;
  } catch (cause) {
    if (cause?.code === "ESRCH") return false;
    // A killed orphan can transiently remain as an unsignalable group member.
    // Keep owning and polling that group; only ESRCH proves it is gone.
    if (cause?.code === "EPERM") return true;
    throw cause;
  }
}

function processGroupExists(processGroupId) {
  try {
    process.kill(-processGroupId, 0);
    return true;
  } catch (cause) {
    if (cause?.code === "ESRCH") return false;
    if (cause?.code === "EPERM") return true;
    throw cause;
  }
}

async function waitForProcessGroupExit(processGroupId, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (processGroupExists(processGroupId)) {
    const remaining = deadline - Date.now();
    if (remaining <= 0) return false;
    await delayForProcessCleanup(Math.min(processGroupPollMs, remaining));
  }
  return true;
}

async function waitForChildClose(close, command) {
  let timeout;
  try {
    return await Promise.race([
      close,
      new Promise((_, rejectPromise) => {
        timeout = setTimeout(
          () => rejectPromise(new Error(`${command} child was not reaped after group cleanup`)),
          processKillWaitMs,
        );
      }),
    ]);
  } finally {
    clearTimeout(timeout);
  }
}

function delayForProcessCleanup(milliseconds) {
  return new Promise((resolvePromise) => setTimeout(resolvePromise, milliseconds));
}

async function boundedFetch(url, init, parentSignal, timeoutMs) {
  throwIfAborted(parentSignal);
  const timeoutSignal = AbortSignal.timeout(timeoutMs);
  const signal = parentSignal
    ? AbortSignal.any([parentSignal, timeoutSignal])
    : timeoutSignal;
  return fetch(url, { ...init, signal });
}

async function readJsonResponse(response, maximumBytes, description) {
  const body = await readResponseBuffer(response, maximumBytes, description, true);
  try {
    return JSON.parse(body.toString("utf8"));
  } catch (cause) {
    throw new Error(`${description} returned invalid JSON`, { cause });
  }
}

async function readResponseBuffer(response, expectedOrMaximum, description, maximumOnly = false) {
  if (response.body == null) throw new Error(`${description} returned no body`);
  const reader = response.body.getReader();
  const chunks = [];
  let bytes = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      bytes += value.byteLength;
      if (bytes > expectedOrMaximum) {
        await reader.cancel();
        throw new Error(`${description} exceeded ${expectedOrMaximum} bytes`);
      }
      chunks.push(Buffer.from(value));
    }
  } finally {
    reader.releaseLock();
  }
  if (!maximumOnly && bytes !== expectedOrMaximum) {
    throw new Error(`${description} returned ${bytes} bytes; expected ${expectedOrMaximum}`);
  }
  return Buffer.concat(chunks, bytes);
}

async function responseFailure(operation, response) {
  let detail = "";
  try {
    detail = (await readResponseBuffer(response, 1_000, operation, true)).toString("utf8");
  } catch {
    await response.body?.cancel().catch(() => undefined);
  }
  return `${operation} failed with HTTP ${response.status}${detail ? `: ${detail}` : ""}`;
}

function validateTarPathTopology(paths, path, type) {
  if (paths.has(path)) throw new Error(`web-dist tar repeats path: ${path}`);
  const parts = path.split("/");
  for (let index = 1; index < parts.length; index += 1) {
    const parent = parts.slice(0, index).join("/");
    if (paths.get(parent) === "file") {
      throw new Error(`web-dist tar places ${path} beneath file ${parent}`);
    }
  }
  if (type === "file") {
    for (const existing of paths.keys()) {
      if (existing.startsWith(`${path}/`)) {
        throw new Error(`web-dist tar replaces parent directory with file: ${path}`);
      }
    }
  }
}

function validateTarHeaderChecksum(header) {
  const expected = tarOctal(header.subarray(148, 156), "checksum");
  let sum = 0;
  for (let index = 0; index < header.length; index += 1) {
    sum += index >= 148 && index < 156 ? 0x20 : header[index];
  }
  if (sum !== expected) throw new Error("web-dist tar header checksum is invalid");
}

function tarString(field, description) {
  const nul = field.indexOf(0);
  const body = nul < 0 ? field : field.subarray(0, nul);
  const padding = nul < 0 ? Buffer.alloc(0) : field.subarray(nul);
  if (padding.some((byte) => byte !== 0)) {
    throw new Error(`web-dist tar ${description} has nonzero padding`);
  }
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(body);
  } catch (cause) {
    throw new Error(`web-dist tar ${description} is not UTF-8`, { cause });
  }
}

function tarOctal(field, description) {
  const text = field.toString("ascii").replace(/[\0 ]+$/g, "").replace(/^ +/g, "");
  if (!/^[0-7]+$/.test(text)) throw new Error(`web-dist tar ${description} is not octal`);
  const value = Number.parseInt(text, 8);
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new Error(`web-dist tar ${description} is outside the supported range`);
  }
  return value;
}

function failureDetail(run) {
  const failure = isRecord(run.result) && isRecord(run.result.failure)
    ? run.result.failure.message
    : undefined;
  return typeof failure === "string" ? failure.slice(0, 500) : undefined;
}

function parseCanonicalInteger(value) {
  if (typeof value !== "string" || !/^(?:0|[1-9][0-9]*)$/.test(value)) return undefined;
  const number = Number(value);
  return Number.isSafeInteger(number) ? number : undefined;
}

function parseOrigin(value) {
  try {
    const url = new URL(value);
    if (url.username || url.password || !["http:", "https:"].includes(url.protocol)) {
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
  } catch (cause) {
    throw new Error("controller origins must use HTTPS (HTTP is allowed only for loopback)", {
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

function assertSha1(value, description) {
  if (typeof value !== "string" || !sha1Pattern.test(value)) {
    throw new Error(`${description} must be a full lowercase SHA-1`);
  }
}

function assertSha256(value, description) {
  if (typeof value !== "string" || !sha256Pattern.test(value)) {
    throw new Error(`${description} must be a lowercase SHA-256`);
  }
}

function shortSha(value) {
  return typeof value === "string" ? value.slice(0, 12) : "invalid";
}

function isRecord(value) {
  return value != null && typeof value === "object" && !Array.isArray(value);
}

function errorMessage(value) {
  return value instanceof Error ? value.message : String(value);
}

function sameFileIdentity(left, right) {
  return left.dev === right.dev &&
    left.ino === right.ino &&
    left.mode === right.mode &&
    left.size === right.size &&
    left.mtimeMs === right.mtimeMs &&
    left.ctimeMs === right.ctimeMs;
}

function assertPrivateNodeModuleIdentity(identity, relativePath) {
  if (typeof process.geteuid === "function" && identity.uid !== process.geteuid()) {
    throw new Error(`trusted CI node_modules entry must be controller-owned: ${relativePath}`);
  }
  if (!identity.isSymbolicLink() && (identity.mode & 0o022) !== 0) {
    throw new Error(
      `trusted CI node_modules entry must not be group- or world-writable: ${relativePath}`,
    );
  }
}

async function validateTrustedRuntimeFile(path, description) {
  const identity = await lstat(path).catch((cause) => {
    throw new Error(`trusted CI ${description} is unavailable at ${path}`, { cause });
  });
  if (!identity.isFile() || identity.isSymbolicLink() || await realpath(path) !== path) {
    throw new Error(`trusted CI ${description} must be a real file`);
  }
  if (
    typeof process.geteuid === "function" &&
    identity.uid !== process.geteuid() &&
    identity.uid !== 0
  ) {
    throw new Error(`trusted CI ${description} must be owned by root or the controller user`);
  }
  if ((identity.mode & 0o022) !== 0) {
    throw new Error(`trusted CI ${description} must not be group- or world-writable`);
  }
  return path;
}

function environmentSecrets(env, extra = []) {
  const values = Object.entries(env)
    .filter(([name, value]) =>
      /(?:TOKEN|SECRET|PASSWORD|API_KEY|PRIVATE_KEY)/i.test(name) &&
      typeof value === "string" && value !== ""
    )
    .flatMap(([, value]) => [value, value.trim()]);
  return [...values, ...extra].filter((value) => typeof value === "string" && value !== "");
}

function throwIfAborted(signal) {
  if (signal?.aborted) throw signal.reason ?? new DOMException("operation aborted", "AbortError");
}

function isAbort(value) {
  return value?.name === "AbortError";
}

function abortableDelay(milliseconds, signal) {
  throwIfAborted(signal);
  if (milliseconds <= 0) return Promise.resolve();
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

async function main(args = process.argv.slice(2), env = process.env) {
  const options = parseArguments(args);
  if (options.help) {
    process.stdout.write(
      "Usage: node web/scripts/ci-controller.mjs [--once]\n" +
        "       node web/scripts/ci-controller.mjs repair-repository <exact-sha>\n" +
        "Polls authoritative master, runs native CI, and promotes only its exact green artifact.\n",
    );
    return;
  }
  const config = controllerConfiguration(env, {
    requireGithubStatus: options.command === "run",
    requireRustSec: options.command === "run",
  });
  // Validate each independent authority before taking the lock. The resulting
  // objects are discarded; child processes receive freshly scoped copies.
  if (options.command === "run") {
    publicationEnvironment(env, "rustsec", { rustSecRepository: config.rustSecRepository });
    publicationEnvironment(env, "source", {
      cargoVendorSha256: "0".repeat(64),
      repository: config.repository,
      rustSecRevision: "0".repeat(40),
    });
    deploymentEnvironment(env, "0".repeat(40), { origin: config.webOrigin });
  }
  repositoryEnvironment(env, { repository: config.repository, origin: config.webOrigin });
  const secrets = environmentSecrets(env, [config.githubToken]);
  const lockPath = await controllerLockPath(config.repository, env, undefined, secrets);
  const releaseLock = await acquireExclusiveLock(lockPath, { repository: config.repository });
  const controller = new AbortController();
  const abort = (signalName) => controller.abort(new DOMException(signalName, "AbortError"));
  const onInterrupt = () => abort("SIGINT");
  const onTerminate = () => abort("SIGTERM");
  process.once("SIGINT", onInterrupt);
  process.once("SIGTERM", onTerminate);
  try {
    const operations = await createOperations(config);
    if (options.command === "repair-repository") {
      const result = await runRepositoryRepair(operations, options.head, {
        signal: controller.signal,
      });
      process.stdout.write(`Trusted CI repaired repository ${shortSha(result.head)}\n`);
      return;
    }
    let settledHead;
    do {
      let retryImmediately = false;
      try {
        if (settledHead) {
          const currentHead = await operations.synchronizeCheckout(controller.signal);
          if (
            currentHead === settledHead &&
            await operations.isPromoted(currentHead, controller.signal)
          ) {
            await abortableDelay(config.intervalMs, controller.signal);
            continue;
          }
        }
        const result = await runControllerCycle(operations, { signal: controller.signal });
        settledHead = result.head;
        process.stdout.write(
          `Trusted CI ${result.action === "promoted" ? "promoted" : "verified"} ${shortSha(result.head)}\n`,
        );
      } catch (cause) {
        if (isAbort(cause)) break;
        if (options.once) throw cause;
        if (cause instanceof StaleHeadError) {
          retryImmediately = true;
          settledHead = undefined;
          process.stdout.write(
            `Trusted CI superseded ${shortSha(cause.expected)} with ` +
              `${shortSha(cause.observed)}; reconciling immediately\n`,
          );
        } else {
          process.stderr.write(`Trusted CI cycle failed: ${redactSecrets(errorMessage(cause), secrets)}\n`);
        }
      }
      if (!options.once && !retryImmediately) {
        await abortableDelay(config.intervalMs, controller.signal);
      }
    } while (!options.once && !controller.signal.aborted);
  } finally {
    process.removeListener("SIGINT", onInterrupt);
    process.removeListener("SIGTERM", onTerminate);
    await releaseLock();
  }
}

if (resolve(process.argv[1] ?? "") === scriptPath) {
  try {
    await main();
  } catch (cause) {
    const secrets = environmentSecrets(process.env);
    process.stderr.write(`Trusted CI controller failed: ${redactSecrets(errorMessage(cause), secrets)}\n`);
    process.exitCode = 1;
  }
}
