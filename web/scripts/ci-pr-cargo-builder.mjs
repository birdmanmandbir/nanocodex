import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { constants, createReadStream, createWriteStream } from "node:fs";
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  open,
  readFile,
  readdir,
  readlink,
  realpath,
  rm,
  stat,
} from "node:fs/promises";
import { dirname, isAbsolute, resolve } from "node:path";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { fileURLToPath } from "node:url";
import { createGzip } from "node:zlib";

const scriptPath = fileURLToPath(import.meta.url);
const authoritativeRepositoryUrl = "https://github.com/gakonst/nanocodex.git";
const sha1Pattern = /^[a-f0-9]{40}$/;
const sha256Pattern = /^[a-f0-9]{64}$/;
const maximumBuildRequestBytes = 16 * 1024;
const maximumDescriptorBytes = 16 * 1024;
const maximumBundleBytes = 256 * 1024 * 1024;
const maximumTarBytes = 2 * 1024 * 1024 * 1024;
const maximumTarEntries = 100_000;
const maximumCommandOutputBytes = 16 * 1024 * 1024;
const maximumDiagnosticBytes = 2_000;
const processTerminationGraceMs = 500;
const processKillWaitMs = 5_000;
const processGroupPollMs = 25;
const cargoHomeVendor = "/workspace/.cargo-home/vendor";
const frameMagic = Buffer.from("NANOCODEX-CI-CARGO-VENDOR\0", "ascii");
const frameVersion = 1;
const helperVersion = "2026-08-22.1";
const freshHomePolicy = "per-build-private-temporary";
const buildRequestExitCode = 65;
const fixedCargoPath = "/Library/PrivilegedHelperTools/dev.nanocodex.ci-cargo";
const dangerousEnvironmentPattern =
  /(?:^|_)(?:AUTH|CREDENTIAL|COOKIE|KEYCHAIN|PASSWORD|PASSWD|SECRET|SESSION|TOKEN)(?:_|$)/i;
const dangerousEnvironmentNames = new Set([
  "AWS_ACCESS_KEY_ID",
  "AWS_SECRET_ACCESS_KEY",
  "AWS_SESSION_TOKEN",
  "BASH_ENV",
  "CARGO_HOME",
  "CARGO_REGISTRY_TOKEN",
  "CARGO_REGISTRIES_CRATES_IO_TOKEN",
  "CLOUDFLARE_API_KEY",
  "CLOUDFLARE_API_TOKEN",
  "DYLD_INSERT_LIBRARIES",
  "DYLD_LIBRARY_PATH",
  "ENV",
  "GIT_ASKPASS",
  "GIT_CONFIG_GLOBAL",
  "GIT_CONFIG_SYSTEM",
  "GH_ENTERPRISE_TOKEN",
  "GH_TOKEN",
  "GITHUB_ENTERPRISE_TOKEN",
  "GITHUB_TOKEN",
  "LD_LIBRARY_PATH",
  "LD_PRELOAD",
  "NODE_AUTH_TOKEN",
  "NODE_OPTIONS",
  "NODE_PATH",
  "NPM_TOKEN",
  "PERL5OPT",
  "PYTHONPATH",
  "RUBYOPT",
  "RUSTC_WRAPPER",
  "RUSTC_WORKSPACE_WRAPPER",
  "RUSTUP_HOME",
  "SSH_ASKPASS",
  "SSH_AUTH_SOCK",
]);
const activeProcessGroups = new Set();

export class BuildRequestError extends Error {
  constructor(message) {
    super(message);
    this.name = "BuildRequestError";
  }
}

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

function canonicalValue(value) {
  if (Array.isArray(value)) return value.map(canonicalValue);
  if (value && typeof value === "object") {
    const result = {};
    for (const key of Object.keys(value).sort()) {
      result[key] = canonicalValue(value[key]);
    }
    return result;
  }
  return value;
}

export function parseBuildRequest(bytes) {
  if (!Buffer.isBuffer(bytes)) bytes = Buffer.from(bytes ?? "");
  if (bytes.length === 0 || bytes.length > maximumBuildRequestBytes) {
    throw new BuildRequestError("build request must be nonempty and at most 16384 bytes");
  }
  let value;
  try {
    value = JSON.parse(bytes.toString("utf8"));
  } catch {
    throw new BuildRequestError("build request must be canonical JSON");
  }
  if (!record(value) || canonicalJson(value) !== bytes.toString("utf8")) {
    throw new BuildRequestError("build request must be canonical JSON");
  }
  const keys = Object.keys(value).sort();
  if (sameKeys(keys, ["baseHead", "mergeHead", "number", "pullRequestHead", "version"])) {
    if (
      value.version !== 1 || !Number.isSafeInteger(value.number) || value.number <= 0 ||
      !sha1Pattern.test(value.baseHead) || !sha1Pattern.test(value.pullRequestHead) ||
      !sha1Pattern.test(value.mergeHead)
    ) throw new BuildRequestError("invalid pull-request build identity");
    return {
      type: "pull_request",
      version: 1,
      number: value.number,
      baseHead: value.baseHead,
      pullRequestHead: value.pullRequestHead,
      mergeHead: value.mergeHead,
    };
  }
  if (sameKeys(keys, ["head", "kind", "version"])) {
    if (value.version !== 1 || value.kind !== "master" || !sha1Pattern.test(value.head)) {
      throw new BuildRequestError("invalid master build identity");
    }
    return { type: "master", version: 1, head: value.head };
  }
  throw new BuildRequestError("build request has unsupported fields");
}

export function dangerousCredentialEnvironment(env = process.env) {
  return Object.keys(env).filter((name) =>
    name.startsWith("NANOCODEX_") ||
    dangerousEnvironmentNames.has(name) ||
    dangerousEnvironmentPattern.test(name)
  ).sort();
}

export function probeDescriptor({ env = process.env, uid, gid } = {}) {
  const credentialEnvironmentNames = dangerousCredentialEnvironment(env);
  if (credentialEnvironmentNames.length > 0) {
    throw new Error(
      "credential-dangerous environment is present: " +
      credentialEnvironmentNames.join(", "),
    );
  }
  const observedUid = uid ?? process.getuid?.();
  const observedGid = gid ?? process.getgid?.();
  if (
    !Number.isSafeInteger(observedUid) || observedUid <= 0 ||
    !Number.isSafeInteger(observedGid) || observedGid <= 0
  ) throw new Error("helper requires a POSIX uid and gid");
  return {
    credentialEnvironmentNames: [],
    freshHomePolicy,
    gid: observedGid,
    helperVersion,
    uid: observedUid,
    version: 1,
  };
}

export function cargoConfigurationDifference(base, merge) {
  const paths = [".cargo", ".cargo/config", ".cargo/config.toml"];
  for (const path of paths) {
    const left = base.get(path) ?? null;
    const right = merge.get(path) ?? null;
    if (
      (typeof left === "string" && left.startsWith("120000 ")) ||
      (typeof right === "string" && right.startsWith("120000 "))
    ) return path;
    if (path !== ".cargo" && left !== right) return path;
  }
  return null;
}

export function encodeFrameHeader(descriptor) {
  const normalized = validateDescriptor(descriptor);
  const json = Buffer.from(canonicalJson(normalized));
  if (json.length === 0 || json.length > maximumDescriptorBytes) {
    throw new Error("Cargo vendor descriptor exceeds its bound");
  }
  const header = Buffer.alloc(frameMagic.length + 8);
  frameMagic.copy(header);
  header.writeUInt32BE(frameVersion, frameMagic.length);
  header.writeUInt32BE(json.length, frameMagic.length + 4);
  return Buffer.concat([header, json]);
}

export function validateDescriptor(value) {
  if (!record(value) || value.version !== 1) {
    throw new Error("invalid Cargo vendor descriptor");
  }
  const common =
    sha1Pattern.test(value.cargoLockBlob) &&
    sha256Pattern.test(value.sha256) &&
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
    if (!sha1Pattern.test(value.head)) {
      throw new Error("invalid master Cargo vendor descriptor");
    }
    return value;
  }
  throw new Error("invalid Cargo vendor descriptor fields");
}

export async function buildCargoVendor(request, {
  gitPath,
  signal,
} = {}) {
  const identity = validateParsedRequest(request);
  const uid = process.getuid?.();
  const gid = process.getgid?.();
  if (
    !Number.isSafeInteger(uid) || uid <= 0 ||
    !Number.isSafeInteger(gid) || gid <= 0
  ) throw new Error("helper requires a non-root POSIX uid and nonzero gid");
  const tools = {
    git: gitPath ?? await resolveTrustedExecutable(
      process.platform === "win32" ? [] : ["/usr/bin/git", "/bin/git"],
      "Git",
      uid,
    ),
    cargo: await resolveFixedCargoExecutable(),
  };
  const state = await createFreshState(uid);
  const buildAbort = new AbortController();
  const combinedSignal = signal
    ? AbortSignal.any([signal, buildAbort.signal])
    : buildAbort.signal;
  const onSignal = () => buildAbort.abort(new DOMException("helper stopped", "AbortError"));
  let bundle;
  process.once("SIGINT", onSignal);
  process.once("SIGTERM", onSignal);
  try {
    const context = {
      ...state,
      ...tools,
      signal: combinedSignal,
      env: buildEnvironment(state),
    };
    await checkoutIdentity(context, identity);
    await assertCheckout(context, identity);
    await assertCargoConfigurationBoundary(context, identity);
    const cargoLockBlob = await gitText(context, [
      "rev-parse", "--verify", `${identityHead(identity)}:Cargo.lock`,
    ]);
    if (!sha1Pattern.test(cargoLockBlob)) {
      throw new Error("Git resolved an invalid Cargo.lock blob ID");
    }
    const lockPath = resolve(state.repository, "Cargo.lock");
    const lockIdentity = await lstat(lockPath);
    if (
      !lockIdentity.isFile() || lockIdentity.isSymbolicLink() ||
      await realpath(lockPath) !== lockPath
    ) throw new Error("Cargo.lock must be a real root repository file");
    const cargoLock = await readFile(lockPath, "utf8");
    const observedLock = await gitText(context, ["hash-object", "--no-filters", "--", "Cargo.lock"]);
    if (observedLock !== cargoLockBlob) throw new Error("worktree Cargo.lock differs from tested merge");

    await runTool(tools.cargo, ["fetch", "--locked"], {
      cwd: state.repository,
      env: context.env,
      signal: combinedSignal,
      operation: "cargo fetch --locked",
    });
    await assertCheckout(context, identity);
    await assertCargoConfigurationBoundary(context, identity);

    const vendorDirectory = resolve(state.root, "vendor");
    const vendorResult = await runTool(tools.cargo, [
      "vendor", "--offline", "--locked", "--versioned-dirs", vendorDirectory,
    ], {
      cwd: state.repository,
      env: { ...context.env, CARGO_NET_OFFLINE: "true" },
      signal: combinedSignal,
      operation: "cargo vendor",
    });
    const config = cargoSourceConfig(vendorResult.stdout.toString("utf8"), cargoLock);
    const directories = await selectVendorDirectories(vendorDirectory, cargoLock);
    const checkouts = await readCleanCargoGitCheckouts(context, cargoLock);
    const extraDirectories = await cargoVendorExtraDirectories(cargoLock, checkouts);
    const bundlePath = resolve(state.root, "bundle.tar.gz");
    await buildCargoVendorBundle({
      bundlePath,
      config,
      directories,
      extraDirectories,
      vendorDirectory,
    });
    bundle = await openGeneratedBundle(bundlePath, { expectedUid: uid, expectedGid: gid });
    if (bundle.size > maximumBundleBytes) {
      throw new Error(`CI Cargo vendor bundle exceeds ${maximumBundleBytes} bytes`);
    }
    await assertCheckout(context, identity);
    await assertCargoConfigurationBoundary(context, identity);
    await assertCleanCargoGitCheckouts(context, checkouts);
    const descriptor = identity.type === "pull_request"
      ? {
          version: 1,
          number: identity.number,
          baseHead: identity.baseHead,
          pullRequestHead: identity.pullRequestHead,
          mergeHead: identity.mergeHead,
          cargoLockBlob,
          key: `cargo-vendor/${cargoLockBlob}/${bundle.sha256}/bundle.tar.gz`,
          size: bundle.size,
          sha256: bundle.sha256,
        }
      : {
          version: 1,
          head: identity.head,
          cargoLockBlob,
          key: `cargo-vendor/${cargoLockBlob}/${bundle.sha256}/bundle.tar.gz`,
          size: bundle.size,
          sha256: bundle.sha256,
        };
    validateDescriptor(descriptor);
    let cleaned = false;
    return {
      descriptor,
      bundle,
      bundlePath,
      cleanup: async () => {
        if (cleaned) return;
        cleaned = true;
        await bundle.handle.close().catch(() => undefined);
        await cleanupState(state.root);
      },
    };
  } catch (cause) {
    await bundle?.handle.close().catch(() => undefined);
    try {
      await cleanupState(state.root);
    } catch (cleanupCause) {
      if (cause instanceof Error) cause.cleanupCause = cleanupCause;
      else throw new AggregateError([cause, cleanupCause], "Cargo build and cleanup failed");
    }
    throw cause;
  } finally {
    process.removeListener("SIGINT", onSignal);
    process.removeListener("SIGTERM", onSignal);
  }
}

export async function writeCargoVendorFrame(output, descriptor, bundle) {
  validateDescriptor(descriptor);
  if (!bundle?.handle || !Number.isSafeInteger(bundle.handle.fd) || bundle.handle.fd < 0) {
    throw new Error("Cargo vendor bundle must be an already-open generated artifact");
  }
  const identityOptions = {
    expectedUid: bundle.expectedUid ?? process.getuid?.(),
    expectedGid: bundle.expectedGid ?? process.getgid?.(),
  };
  const before = generatedBundleIdentity(await bundle.handle.stat(), identityOptions);
  if (!sameFileIdentity(before, bundle.identity)) {
    throw new Error("Cargo vendor bundle changed before framing");
  }
  await assertGeneratedBundlePath(bundle.path, before, identityOptions);
  const header = encodeFrameHeader(descriptor);
  await writeOutput(output, header);
  let bytes = 0;
  const hash = createHash("sha256");
  for await (const chunk of createReadStream("", {
    fd: bundle.handle.fd,
    autoClose: false,
    start: 0,
    end: descriptor.size - 1,
  })) {
    bytes += chunk.length;
    if (bytes > descriptor.size || bytes > maximumBundleBytes) {
      throw new Error("Cargo vendor bundle changed while framing");
    }
    hash.update(chunk);
    await writeOutput(output, chunk);
  }
  if (bytes !== descriptor.size) throw new Error("Cargo vendor bundle was truncated while framing");
  if (hash.digest("hex") !== descriptor.sha256) {
    throw new Error("Cargo vendor bundle hash changed while framing");
  }
  const after = generatedBundleIdentity(await bundle.handle.stat(), identityOptions);
  if (!sameFileIdentity(before, after)) {
    throw new Error("Cargo vendor bundle changed while framing");
  }
  await assertGeneratedBundlePath(bundle.path, after, identityOptions);
}

export async function main({
  args = process.argv.slice(2),
  env = process.env,
  input = process.stdin,
  output = process.stdout,
  signal,
} = {}) {
  if (args.length !== 1 || !["--probe", "--build"].includes(args[0])) {
    throw new BuildRequestError("helper accepts exactly one of --probe or --build");
  }
  const probe = probeDescriptor({ env });
  if (args[0] === "--probe") {
    await probeToolchain(probe.uid, signal);
    await writeOutput(output, Buffer.from(canonicalJson(probe) + "\n"));
    return probe;
  }
  const requestBytes = await readBoundedInput(input, maximumBuildRequestBytes);
  const request = parseBuildRequest(requestBytes);
  const build = await buildCargoVendor(request, { signal });
  try {
    await writeCargoVendorFrame(output, build.descriptor, build.bundle);
    return build.descriptor;
  } finally {
    await build.cleanup();
  }
}

async function checkoutIdentity(context, identity) {
  await runGit(context, ["init", "--quiet", context.repository], context.root);
  await assertPrivateBuildDirectories(context);
  await runGit(context, ["remote", "add", "origin", authoritativeRepositoryUrl]);
  const refspecs = identity.type === "pull_request"
    ? [
        "+refs/heads/master:refs/remotes/origin/master",
        `+refs/pull/${identity.number}/head:refs/pull/${identity.number}/head`,
        `+refs/pull/${identity.number}/merge:refs/pull/${identity.number}/merge`,
      ]
    : ["+refs/heads/master:refs/remotes/origin/master"];
  await runGit(context, [
    "fetch", "--quiet", "--force", "--atomic", "--no-tags", "--no-recurse-submodules",
    "origin", ...refspecs,
  ]);
  const target = identity.type === "pull_request"
    ? `refs/pull/${identity.number}/merge`
    : "refs/remotes/origin/master";
  await runGit(context, ["checkout", "--quiet", "--detach", "--force", target]);
}

async function assertPrivateBuildDirectories(context) {
  const uid = process.getuid?.();
  for (const path of [context.root, context.home, context.cargoHome, context.repository]) {
    const identity = await lstat(path);
    if (
      !identity.isDirectory() || identity.isSymbolicLink() || identity.uid !== uid ||
      (identity.mode & 0o077) !== 0 || await realpath(path) !== path
    ) throw new Error("Cargo preparation directories must remain private to the prep uid");
  }
}

async function assertCheckout(context, identity) {
  await assertPrivateBuildDirectories(context);
  const head = await gitText(context, ["rev-parse", "--verify", "HEAD^{commit}"]);
  if (head !== identityHead(identity)) throw new Error("detached checkout identity changed");
  const status = await runGit(context, [
    "status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignore-submodules=none",
  ]);
  if (status.stdout.length !== 0) throw new Error("detached checkout is not clean");
  const origin = await gitText(context, ["remote", "get-url", "origin"]);
  if (origin !== authoritativeRepositoryUrl) throw new Error("repository origin changed");
  const replacements = await gitText(context, [
    "for-each-ref", "--format=%(refname)", "refs/replace/",
  ]);
  if (replacements !== "") throw new Error("replacement refs are forbidden");
  if (identity.type === "pull_request") {
    const [base, pullHead, merge, parents] = await Promise.all([
      gitText(context, ["rev-parse", "--verify", "refs/remotes/origin/master^{commit}"]),
      gitText(context, ["rev-parse", "--verify", `refs/pull/${identity.number}/head^{commit}`]),
      gitText(context, ["rev-parse", "--verify", `refs/pull/${identity.number}/merge^{commit}`]),
      gitText(context, ["show", "--no-patch", "--format=%P", identity.mergeHead]),
    ]);
    if (
      base !== identity.baseHead || pullHead !== identity.pullRequestHead ||
      merge !== identity.mergeHead || parents !== `${identity.baseHead} ${identity.pullRequestHead}`
    ) throw new Error("fetched refs do not match the exact authoritative PR merge identity");
  } else {
    const master = await gitText(context, [
      "rev-parse", "--verify", "refs/remotes/origin/master^{commit}",
    ]);
    if (master !== identity.head) throw new Error("fetched master differs from requested head");
  }
}

async function assertCargoConfigurationBoundary(context, identity) {
  if (identity.type !== "pull_request") return;
  const base = await cargoTreeEntries(context, identity.baseHead);
  const merge = await cargoTreeEntries(context, identity.mergeHead);
  const difference = cargoConfigurationDifference(base, merge);
  if (difference) {
    throw new Error(`pull request changes forbidden Cargo configuration: ${difference}`);
  }
}

async function cargoTreeEntries(context, commit) {
  const result = new Map();
  for (const path of [".cargo", ".cargo/config", ".cargo/config.toml"]) {
    const output = await runGit(context, ["ls-tree", "-z", commit, "--", path]);
    if (output.stdout.length === 0) continue;
    const records = output.stdout.toString("utf8").split("\0").filter(Boolean);
    const exact = records.find((record) => record.endsWith(`\t${path}`));
    if (exact) result.set(path, exact.slice(0, exact.indexOf("\t")));
  }
  return result;
}

async function runGit(context, args, cwd = context.repository) {
  return runTool(context.git, [
    "-c", "core.fsmonitor=false",
    "-c", "core.hooksPath=/dev/null",
    "-c", "core.attributesFile=/dev/null",
    "-c", "core.autocrlf=false",
    "-c", "credential.helper=",
    "-c", "core.askPass=/usr/bin/false",
    "-c", "credential.interactive=never",
    "-c", "protocol.file.allow=never",
    ...args,
  ], {
    cwd,
    env: context.env,
    signal: context.signal,
    operation: `git ${args[0]}`,
  });
}

async function gitText(context, args, cwd) {
  return (await runGit(context, args, cwd)).stdout.toString("utf8").trimEnd();
}

async function createFreshState(uid) {
  const temporaryBase = await fixedTemporaryBase();
  const root = await mkdtemp(resolve(temporaryBase, "nanocodex-ci-cargo-prep-"));
  await chmod(root, 0o700);
  const identity = await lstat(root);
  if (
    !identity.isDirectory() || identity.isSymbolicLink() || identity.uid !== uid ||
    (identity.mode & 0o777) !== 0o700
  ) {
    await cleanupState(root);
    throw new Error("fresh preparation root has an unsafe identity");
  }
  const home = resolve(root, "home");
  const cargoHome = resolve(root, "cargo-home");
  const repository = resolve(root, "repository");
  await Promise.all([
    mkdir(home, { mode: 0o700 }),
    mkdir(cargoHome, { mode: 0o700 }),
    mkdir(repository, { mode: 0o700 }),
  ]);
  return { root, home, cargoHome, repository };
}

export function buildEnvironment(state) {
  return {
    PATH: "/usr/bin:/bin",
    HOME: state.home,
    USERPROFILE: state.home,
    CARGO_HOME: state.cargoHome,
    RUSTUP_HOME: resolve(state.root, "rustup-home"),
    LANG: "C.UTF-8",
    LC_ALL: "C.UTF-8",
    TMPDIR: state.root,
    XDG_CACHE_HOME: resolve(state.root, "xdg-cache"),
    XDG_CONFIG_HOME: resolve(state.root, "xdg-config"),
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_CONFIG_SYSTEM: "/dev/null",
    GIT_NO_REPLACE_OBJECTS: "1",
    GIT_OPTIONAL_LOCKS: "0",
    GIT_TERMINAL_PROMPT: "0",
    CARGO_NET_GIT_FETCH_WITH_CLI: "false",
    CARGO_REGISTRIES_CRATES_IO_PROTOCOL: "sparse",
    CARGO_TERM_COLOR: "never",
  };
}

async function fixedTemporaryBase() {
  for (const candidate of process.platform === "darwin" ? ["/private/tmp", "/tmp"] : ["/tmp"]) {
    try {
      const canonical = await realpath(candidate);
      const identity = await lstat(canonical);
      if (identity.isDirectory() && !identity.isSymbolicLink()) return canonical;
    } catch {}
  }
  throw new Error("fixed system temporary directory is unavailable");
}

export async function resolveTrustedExecutable(candidates, description, uid = process.getuid?.()) {
  for (const candidate of candidates) {
    try {
      if (!isAbsolute(candidate)) continue;
      const canonical = await realpath(candidate);
      const identity = await lstat(canonical);
      if (
        !identity.isFile() || identity.isSymbolicLink() ||
        (identity.mode & 0o111) === 0 || (identity.mode & 0o022) !== 0
      ) continue;
      let path = canonical;
      let safe = true;
      while (path !== "/") {
        const part = await lstat(path);
        if (
          part.isSymbolicLink() || (part.mode & 0o022) !== 0 ||
          (Number.isSafeInteger(uid) && part.uid === uid && (part.mode & 0o200) !== 0)
        ) {
          safe = false;
          break;
        }
        path = dirname(path);
      }
      if (safe) return canonical;
    } catch {}
  }
  throw new Error(`${description} must be an absolute non-prep-writable trusted executable`);
}

export async function resolveFixedCargoExecutable({
  lstatPath = lstat,
  realpathPath = realpath,
} = {}) {
  let canonical;
  try {
    canonical = await realpathPath(fixedCargoPath);
  } catch (cause) {
    throw new Error(`Cargo must be installed at ${fixedCargoPath}`, { cause });
  }
  if (canonical !== fixedCargoPath) {
    throw new Error(`Cargo at ${fixedCargoPath} must not traverse symbolic links`);
  }
  let path = fixedCargoPath;
  while (true) {
    let identity;
    try {
      identity = await lstatPath(path);
    } catch (cause) {
      throw new Error(`Cargo trust path is unavailable: ${path}`, { cause });
    }
    const common =
      !identity.isSymbolicLink() && identity.uid === 0 && identity.gid === 0 &&
      (identity.mode & 0o022) === 0;
    const valid = path === fixedCargoPath
      ? common && identity.isFile() && identity.nlink === 1 && (identity.mode & 0o111) !== 0
      : common && identity.isDirectory();
    if (!valid) {
      throw new Error(
        path === fixedCargoPath
          ? "fixed Cargo must be one root-owned, singly-linked, non-root-writable executable"
          : `fixed Cargo parent must be a real root-owned non-root-writable directory: ${path}`,
      );
    }
    if (path === "/") break;
    path = dirname(path);
  }
  return fixedCargoPath;
}

async function probeToolchain(uid, signal) {
  const git = await resolveTrustedExecutable(
    process.platform === "win32" ? [] : ["/usr/bin/git", "/bin/git"],
    "Git",
    uid,
  );
  const cargo = await resolveFixedCargoExecutable();
  const environment = {
    PATH: "/usr/bin:/bin",
    HOME: "/var/empty",
    CARGO_HOME: "/var/empty",
    RUSTUP_HOME: "/var/empty",
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_TERMINAL_PROMPT: "0",
    LANG: "C.UTF-8",
  };
  await runTool(git, ["--version"], {
    cwd: "/",
    env: environment,
    signal,
    operation: "Git readiness probe",
  });
  await runTool(cargo, ["--version", "--verbose"], {
    cwd: "/",
    env: environment,
    signal,
    operation: "Cargo readiness probe",
  });
}

export async function runTool(command, args, { cwd, env, signal, operation }) {
  throwIfAborted(signal);
  const child = spawn(command, args, {
    cwd,
    env,
    shell: false,
    detached: process.platform !== "win32",
    stdio: ["ignore", "pipe", "pipe"],
  });
  const processGroupId = process.platform === "win32" ? undefined : child.pid;
  if (processGroupId != null) activeProcessGroups.add(processGroupId);
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
  const consume = (chunk, chunks, stream) => {
    if (failure != null) return;
    if (stream === "stdout") stdoutBytes += chunk.length;
    else stderrBytes += chunk.length;
    if (stdoutBytes > maximumCommandOutputBytes || stderrBytes > maximumCommandOutputBytes) {
      fail(new Error(`${operation} exceeded its output bound`));
      return;
    }
    chunks.push(chunk);
  };
  child.stdout.on("data", (chunk) => consume(chunk, stdout, "stdout"));
  child.stderr.on("data", (chunk) => consume(chunk, stderr, "stderr"));
  const onAbort = () => {
    fail(signal.reason ?? new DOMException("Aborted", "AbortError"));
  };
  signal?.addEventListener("abort", onAbort, { once: true });
  if (signal?.aborted) onAbort();
  const close = new Promise((resolvePromise) => {
    child.once("close", (code, signalName) =>
      resolvePromise({ code, signalName }));
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
      result = await waitForChildClose(close, operation);
    } catch (cause) {
      closeFailure = cause;
    }
  } finally {
    signal?.removeEventListener("abort", onAbort);
  }
  if (cleanupOutcome?.error == null && processGroupId != null) {
    activeProcessGroups.delete(processGroupId);
  }
  const cleanupFailures = [cleanupOutcome?.error, closeFailure].filter(Boolean);
  const cleanupFailure = cleanupFailures.length > 1
    ? new AggregateError(cleanupFailures, "detached tool cleanup failed")
    : cleanupFailures[0];
  const abortFailure = signal?.aborted
    ? signal.reason ?? new DOMException("Aborted", "AbortError")
    : undefined;
  const primaryFailure = abortFailure ?? failure;
  if (primaryFailure) {
    if (cleanupFailure != null) {
      if (primaryFailure instanceof Error) primaryFailure.cleanupCause = cleanupFailure;
      else {
        throw new AggregateError(
          [primaryFailure, cleanupFailure],
          `${operation} abort and cleanup failed`,
        );
      }
    }
    throw primaryFailure;
  }
  if (cleanupFailure != null) throw cleanupFailure;
  if (result.code === 0 && cleanupOutcome.value.observedLiveGroup) {
    throw new Error(
      `${operation} exited successfully but left a live detached process-group descendant`,
    );
  }
  const output = {
    stdout: Buffer.concat(stdout, stdoutBytes),
    stderr: Buffer.concat(stderr, stderrBytes),
  };
  if (result.code === 0) return output;
  const detail = (output.stderr.length ? output.stderr : output.stdout)
    .toString("utf8").trim().slice(0, maximumDiagnosticBytes);
  throw new Error(
    `${operation} failed with exit ${String(result.code)}` +
      `${result.signalName ? ` (${result.signalName})` : ""}` +
      `${detail ? `: ${detail}` : ""}`,
  );
}

async function cleanupDetachedProcessGroup(child, processGroupId) {
  if (process.platform === "win32") {
    const observedLiveProcess = child?.exitCode == null && child?.signalCode == null;
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

async function waitForChildClose(close, operation) {
  let timeout;
  try {
    return await Promise.race([
      close,
      new Promise((_, rejectPromise) => {
        timeout = setTimeout(
          () => rejectPromise(new Error(`${operation} child was not reaped after group cleanup`)),
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

export function cargoSourceConfig(cargoVendorOutput, cargoLock) {
  const expected = new Set(gitPackagesFromCargoLock(cargoLock).map(({ source }) =>
    source.slice(0, source.lastIndexOf("#"))
  ));
  const blocks = parseSourceBlocks(cargoVendorOutput);
  const cratesIo = blocks.filter(({ header }) => header === "[source.crates-io]");
  const vendored = blocks.filter(({ header }) => header === "[source.vendored-sources]");
  const gitBlocks = blocks.filter(({ header }) => header.startsWith("[source.\"git+"));
  const observed = new Set(gitBlocks.map(({ header }) => header.slice(9, -2)));
  if (
    observed.size !== expected.size || [...expected].some((source) => !observed.has(source)) ||
    cratesIo.length !== 1 || vendored.length !== 1 ||
    !cratesIo[0].lines.includes('replace-with = "vendored-sources"') ||
    blocks.length !== gitBlocks.length + 2
  ) throw new Error("cargo vendor source config does not replace every locked external source");
  for (const block of gitBlocks) {
    if (!block.lines.includes('replace-with = "vendored-sources"')) {
      throw new Error(`cargo vendor omitted source replacement in ${block.header}`);
    }
  }
  gitBlocks.sort((left, right) => left.header.localeCompare(right.header));
  return `[source.crates-io]\nreplace-with = "vendored-sources"\n\n` +
    `${gitBlocks.map(({ header, lines }) => `${header}\n${lines.join("\n")}`).join("\n\n")}` +
    `${gitBlocks.length ? "\n\n" : "\n"}[source.vendored-sources]\n` +
    `directory = "${cargoHomeVendor}"\n`;
}

function parseSourceBlocks(output) {
  const blocks = [];
  let current;
  for (const raw of output.replaceAll("\r\n", "\n").split("\n")) {
    const line = raw.trim();
    if (line.startsWith("[source.") && line.endsWith("]")) {
      current = { header: line, lines: [] };
      blocks.push(current);
    } else if (current && line !== "") {
      if (!/^(?:replace-with|git|rev|directory) = "[^"\r\n]+"$/.test(line)) {
        throw new Error("cargo vendor emitted unsupported source configuration");
      }
      current.lines.push(line);
    }
  }
  return blocks;
}

export async function selectVendorDirectories(vendorDirectory, cargoLock) {
  const expected = externalPackagesFromCargoLock(cargoLock);
  const expectedByDirectory = new Map();
  for (const dependency of expected) {
    const directory = `${dependency.name}-${dependency.version}`;
    const identities = expectedByDirectory.get(directory) ?? [];
    identities.push(dependency.checksum ?? null);
    expectedByDirectory.set(directory, identities);
  }
  const entries = await readdir(vendorDirectory, { withFileTypes: true });
  const selected = [];
  for (const entry of entries.sort((left, right) => left.name.localeCompare(right.name))) {
    if (!entry.isDirectory() || !isSafeArchivePart(entry.name)) {
      throw new Error(`cargo vendor produced unsupported entry: ${entry.name}`);
    }
    let checksum;
    try {
      checksum = JSON.parse(await readFile(
        resolve(vendorDirectory, entry.name, ".cargo-checksum.json"), "utf8",
      ));
    } catch (cause) {
      throw new Error(`cargo vendor produced an invalid checksum for ${entry.name}`, { cause });
    }
    if (
      checksum?.package !== null &&
      (typeof checksum?.package !== "string" || !sha256Pattern.test(checksum.package))
    ) throw new Error(`cargo vendor produced an invalid package checksum for ${entry.name}`);
    const identities = expectedByDirectory.get(entry.name);
    const match = identities?.indexOf(checksum.package);
    if (match == null || match < 0) {
      throw new Error(`cargo vendor produced a package not bound by Cargo.lock: ${entry.name}`);
    }
    identities.splice(match, 1);
    selected.push(entry.name);
  }
  if (
    selected.length !== expected.length ||
    [...expectedByDirectory.values()].some((identities) => identities.length !== 0)
  ) throw new Error("vendored package directories do not match every Cargo.lock source");
  return selected;
}

export async function buildCargoVendorBundle({
  bundlePath,
  config,
  directories,
  extraDirectories = [],
  vendorDirectory,
}) {
  const configBody = Buffer.from(config);
  const entries = [{
    path: "config.toml",
    type: "file",
    mode: 0o644,
    size: configBody.length,
    body: configBody,
  }, { path: "vendor/", type: "directory", mode: 0o755, size: 0 }];
  for (const directory of [...directories].sort()) {
    await collectArchiveEntries(resolve(vendorDirectory, directory), `vendor/${directory}`, entries);
  }
  for (const { sourcePath, archivePath } of extraDirectories) {
    if (!isSafeArchivePart(archivePath)) throw new Error("unsafe Cargo vendor extra path");
    await collectArchiveEntries(sourcePath, archivePath, entries);
  }
  entries.sort((left, right) => left.path.localeCompare(right.path));
  if (entries.length > maximumTarEntries) throw new Error("CI Cargo vendor tar has too many entries");
  const tarBytes = entries.reduce(
    (total, entry) => total + 512 + entry.size + paddingLength(entry.size), 1024,
  );
  if (tarBytes > maximumTarBytes) throw new Error("CI Cargo vendor tar is too large");
  await pipeline(
    Readable.from(streamTar(entries)),
    createGzip({ level: 9, mtime: 0 }),
    createWriteStream(bundlePath, { flags: "wx", mode: 0o600 }),
  );
}

async function readCleanCargoGitCheckouts(context, cargoLock) {
  const root = resolve(context.cargoHome, "git/checkouts");
  const repositories = await readdir(root, { withFileTypes: true }).catch(() => []);
  const revisions = [...new Set(gitPackagesFromCargoLock(cargoLock).map(({ source }) =>
    source.slice(source.lastIndexOf("#") + 1)
  ))].sort();
  const checkouts = [];
  for (const revision of revisions) {
    if (!sha1Pattern.test(revision)) throw new Error("Cargo.lock has an invalid Git revision");
    const matches = [];
    for (const repository of repositories) {
      if (!repository.isDirectory()) continue;
      const parent = resolve(root, repository.name);
      for (const name of await readdir(parent).catch(() => [])) {
        const candidate = resolve(parent, name);
        try {
          if (!(await lstat(candidate)).isDirectory()) continue;
          if (await gitText(context, ["rev-parse", "--verify", "HEAD^{commit}"], candidate) === revision) {
            matches.push(await realpath(candidate));
          }
        } catch {}
      }
    }
    if (matches.length !== 1) {
      throw new Error(`expected one fresh Cargo Git checkout for ${revision}, found ${matches.length}`);
    }
    checkouts.push(await cleanCargoGitCheckout(context, matches[0], revision));
  }
  return checkouts;
}

async function cleanCargoGitCheckout(context, path, revision) {
  const [head, tree, topLevel, status] = await Promise.all([
    gitText(context, ["rev-parse", "--verify", "HEAD^{commit}"], path),
    gitText(context, ["rev-parse", "--verify", "HEAD^{tree}"], path),
    gitText(context, ["rev-parse", "--show-toplevel"], path),
    gitText(context, [
      "status", "--porcelain=v1", "--untracked-files=all", "--ignored=matching",
      "--ignore-submodules=none",
    ], path),
  ]);
  const unexpected = status.split("\n").filter((line) => line && line !== "?? .cargo-ok");
  if (
    head !== revision || !sha1Pattern.test(tree) || await realpath(topLevel) !== path ||
    unexpected.length > 0
  ) throw new Error(`Cargo Git checkout ${revision} is dirty or mismatched`);
  return { path, revision, tree };
}

async function assertCleanCargoGitCheckouts(context, expected) {
  for (const checkout of expected) {
    const observed = await cleanCargoGitCheckout(context, checkout.path, checkout.revision);
    if (observed.tree !== checkout.tree) throw new Error("Cargo Git checkout changed during build");
  }
}

async function cargoVendorExtraDirectories(cargoLock, checkouts) {
  const dependency = gitPackagesFromCargoLock(cargoLock).find(
    ({ name }) => name === "krun-init-blob",
  );
  if (!dependency) return [];
  const revision = dependency.source.slice(dependency.source.lastIndexOf("#") + 1);
  const checkout = checkouts.find((candidate) => candidate.revision === revision)?.path;
  if (!checkout) throw new Error("missing verified krun-init checkout");
  const init = resolve(checkout, "init");
  if (!(await stat(resolve(init, "Cargo.toml"))).isFile()) {
    throw new Error("verified krun-init checkout has no init/Cargo.toml");
  }
  return [{ sourcePath: init, archivePath: "init" }];
}

function gitPackagesFromCargoLock(cargoLock) {
  const packages = [];
  for (const block of cargoLock.replaceAll("\r\n", "\n").split("\n\n")) {
    const name = block.match(/^name = "([^"]+)"$/m)?.[1];
    const version = block.match(/^version = "([^"]+)"$/m)?.[1];
    const source = block.match(/^source = "(git\+https:\/\/[^\"]+)"$/m)?.[1];
    if (name && version && source && source.includes("#")) packages.push({ name, version, source });
  }
  return packages;
}

function externalPackagesFromCargoLock(cargoLock) {
  const packages = [];
  for (const block of cargoLock.replaceAll("\r\n", "\n").split("\n\n")) {
    const name = block.match(/^name = "([^"]+)"$/m)?.[1];
    const version = block.match(/^version = "([^"]+)"$/m)?.[1];
    const source = block.match(/^source = "((?:registry\+https:\/\/|git\+https:\/\/)[^\"]+)"$/m)?.[1];
    if (!name || !version || !source) continue;
    const checksum = block.match(/^checksum = "([a-f0-9]{64})"$/m)?.[1];
    if (source.startsWith("registry+") && !checksum) {
      throw new Error(`Cargo.lock registry package ${name} ${version} has no checksum`);
    }
    packages.push({ name, version, source, checksum: checksum ?? null });
  }
  if (packages.length === 0) throw new Error("Cargo.lock contains no supported external packages");
  return packages;
}

async function collectArchiveEntries(sourcePath, archivePath, entries) {
  const metadata = await lstat(sourcePath);
  if (metadata.isDirectory()) {
    entries.push({ path: `${archivePath}/`, type: "directory", mode: 0o755, size: 0 });
    for (const name of (await readdir(sourcePath)).sort()) {
      if (!isSafeArchivePart(name)) throw new Error("unsafe vendored path component");
      await collectArchiveEntries(resolve(sourcePath, name), `${archivePath}/${name}`, entries);
    }
    return;
  }
  if (metadata.isFile()) {
    entries.push({
      path: archivePath,
      type: "file",
      mode: metadata.mode & 0o111 ? 0o755 : 0o644,
      size: metadata.size,
      sourcePath,
      identity: fileIdentity(metadata),
    });
    return;
  }
  if (metadata.isSymbolicLink()) {
    const link = await readlink(sourcePath);
    if (isAbsolute(link) || link.split(/[\\/]/).includes("..") || link.includes("\0")) {
      throw new Error(`unsafe vendored symlink: ${archivePath}`);
    }
    entries.push({ path: archivePath, type: "symlink", mode: 0o777, size: 0, link });
    return;
  }
  throw new Error(`unsupported vendored file type: ${archivePath}`);
}

async function* streamTar(entries) {
  let paxIndex = 0;
  for (const entry of entries) {
    const pathParts = ustarPath(entry.path);
    const pax = [];
    if (!pathParts) pax.push(paxRecord("path", entry.path));
    if (entry.type === "symlink" && Buffer.byteLength(entry.link) > 100) {
      pax.push(paxRecord("linkpath", entry.link));
    }
    if (pax.length > 0) {
      const body = Buffer.from(pax.join(""));
      yield tarHeader({
        path: `PaxHeaders/${String(paxIndex++).padStart(8, "0")}`,
        type: "pax",
        mode: 0o644,
        size: body.length,
      });
      yield body;
      if (paddingLength(body.length)) yield padding(body.length);
    }
    yield tarHeader({
      ...entry,
      path: pathParts ? entry.path : `PaxEntry/${String(paxIndex).padStart(8, "0")}`,
      link: entry.type === "symlink" && Buffer.byteLength(entry.link) <= 100 ? entry.link : "",
    });
    if (entry.type !== "file") continue;
    let observed = 0;
    if (entry.body) {
      observed = entry.body.length;
      yield entry.body;
    } else {
      for await (const chunk of createReadStream(entry.sourcePath)) {
        observed += chunk.length;
        if (observed > entry.size) throw new Error("vendored file grew while archiving");
        yield chunk;
      }
      const after = await lstat(entry.sourcePath);
      if (!after.isFile() || !sameFileIdentity(entry.identity, fileIdentity(after))) {
        throw new Error("vendored file changed while archiving");
      }
    }
    if (observed !== entry.size) throw new Error("vendored file was truncated while archiving");
    if (paddingLength(entry.size)) yield padding(entry.size);
  }
  yield Buffer.alloc(1024);
}

function tarHeader(entry) {
  const header = Buffer.alloc(512);
  const path = ustarPath(entry.path);
  if (!path) throw new Error("tar path cannot be represented");
  writeText(header, path.name, 0, 100);
  writeOctal(header, entry.mode, 100, 8);
  writeOctal(header, 0, 108, 8);
  writeOctal(header, 0, 116, 8);
  writeOctal(header, entry.size, 124, 12);
  writeOctal(header, 0, 136, 12);
  header.fill(0x20, 148, 156);
  header[156] = { file: 0x30, symlink: 0x32, directory: 0x35, pax: 0x78 }[entry.type];
  writeText(header, entry.link ?? "", 157, 100);
  writeText(header, "ustar\0", 257, 6);
  writeText(header, "00", 263, 2);
  writeText(header, "root", 265, 32);
  writeText(header, "root", 297, 32);
  writeText(header, path.prefix, 345, 155);
  const checksum = header.reduce((sum, byte) => sum + byte, 0);
  header.write(checksum.toString(8).padStart(6, "0"), 148, 6, "ascii");
  header[154] = 0;
  header[155] = 0x20;
  return header;
}

function ustarPath(path) {
  if (Buffer.byteLength(path) <= 100) return { name: path, prefix: "" };
  const parts = path.split("/");
  for (let index = parts.length - 1; index > 0; index--) {
    const prefix = parts.slice(0, index).join("/");
    const name = parts.slice(index).join("/");
    if (Buffer.byteLength(prefix) <= 155 && Buffer.byteLength(name) <= 100) {
      return { name, prefix };
    }
  }
  return null;
}

function paxRecord(key, value) {
  const content = `${key}=${value}\n`;
  let length = Buffer.byteLength(content) + 2;
  while (true) {
    const record = `${length} ${content}`;
    if (Buffer.byteLength(record) === length) return record;
    length = Buffer.byteLength(record);
  }
}

function writeText(header, value, offset, length) {
  const bytes = Buffer.from(value);
  if (bytes.length > length) throw new Error("tar text field overflow");
  bytes.copy(header, offset);
}

function writeOctal(header, value, offset, length) {
  if (!Number.isSafeInteger(value) || value < 0) throw new Error("invalid tar integer");
  const encoded = value.toString(8).padStart(length - 1, "0");
  if (encoded.length >= length) throw new Error("tar integer overflow");
  header.write(encoded, offset, length - 1, "ascii");
}

function fileIdentity(metadata) {
  return {
    dev: metadata.dev,
    ino: metadata.ino,
    uid: metadata.uid,
    gid: metadata.gid,
    mode: metadata.mode,
    nlink: metadata.nlink,
    size: metadata.size,
    mtimeMs: metadata.mtimeMs,
    ctimeMs: metadata.ctimeMs,
  };
}

function sameFileIdentity(left, right) {
  return left.dev === right.dev && left.ino === right.ino && left.uid === right.uid &&
    left.gid === right.gid && left.mode === right.mode && left.nlink === right.nlink &&
    left.size === right.size && left.mtimeMs === right.mtimeMs &&
    left.ctimeMs === right.ctimeMs;
}

export async function openGeneratedBundle(path, {
  expectedUid = process.getuid?.(),
  expectedGid = process.getgid?.(),
} = {}) {
  let handle;
  try {
    handle = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW);
    const identityOptions = { expectedUid, expectedGid };
    const before = generatedBundleIdentity(await handle.stat(), identityOptions);
    await assertGeneratedBundlePath(path, before, identityOptions);
    const hash = createHash("sha256");
    let size = 0;
    for await (const chunk of createReadStream("", {
      fd: handle.fd,
      autoClose: false,
      start: 0,
      end: before.size - 1,
    })) {
      size += chunk.length;
      if (size > before.size || size > maximumBundleBytes) {
        throw new Error("generated Cargo vendor bundle changed while hashing");
      }
      hash.update(chunk);
    }
    if (size !== before.size) throw new Error("generated Cargo vendor bundle was truncated");
    const after = generatedBundleIdentity(await handle.stat(), identityOptions);
    if (!sameFileIdentity(before, after)) {
      throw new Error("generated Cargo vendor bundle changed while hashing");
    }
    await assertGeneratedBundlePath(path, after, identityOptions);
    return {
      path,
      handle,
      expectedUid,
      expectedGid,
      identity: after,
      size,
      sha256: hash.digest("hex"),
    };
  } catch (cause) {
    await handle?.close().catch(() => undefined);
    throw cause;
  }
}

function generatedBundleIdentity(metadata, { expectedUid, expectedGid }) {
  if (
    !metadata.isFile() || metadata.isSymbolicLink?.() || metadata.nlink !== 1 ||
    (metadata.mode & 0o777) !== 0o600 ||
    !Number.isSafeInteger(expectedUid) || expectedUid <= 0 || metadata.uid !== expectedUid ||
    !Number.isSafeInteger(expectedGid) || expectedGid <= 0 || metadata.gid !== expectedGid ||
    !Number.isSafeInteger(metadata.size) || metadata.size <= 0 ||
    metadata.size > maximumBundleBytes
  ) throw new Error("generated Cargo vendor bundle has an unsafe identity");
  return fileIdentity(metadata);
}

async function assertGeneratedBundlePath(path, identity, identityOptions) {
  const observed = generatedBundleIdentity(await lstat(path), identityOptions);
  if (!sameFileIdentity(identity, observed)) {
    throw new Error("generated Cargo vendor bundle path changed");
  }
}

async function readBoundedInput(input, maximum) {
  const chunks = [];
  let bytes = 0;
  for await (const chunk of input) {
    bytes += chunk.length;
    if (bytes > maximum) throw new BuildRequestError("build request exceeds 16384 bytes");
    chunks.push(chunk);
  }
  return Buffer.concat(chunks, bytes);
}

async function writeOutput(output, bytes) {
  if (output.write(bytes)) return;
  await new Promise((resolvePromise, rejectPromise) => {
    output.once("drain", resolvePromise);
    output.once("error", rejectPromise);
  });
}

async function cleanupState(root) {
  const failures = [];
  for (const processGroupId of [...activeProcessGroups]) {
    try {
      await cleanupDetachedProcessGroup(undefined, processGroupId);
      activeProcessGroups.delete(processGroupId);
    } catch (cause) {
      failures.push(cause);
    }
  }
  if (root) {
    try {
      await rm(root, { recursive: true, force: true });
    } catch (cause) {
      failures.push(cause);
    }
  }
  if (failures.length > 0) {
    throw new AggregateError(failures, "Cargo helper cleanup failed");
  }
}

function identityHead(identity) {
  return identity.type === "pull_request" ? identity.mergeHead : identity.head;
}

function validateParsedRequest(value) {
  if (!record(value) || value.version !== 1) throw new BuildRequestError("invalid build request");
  if (value.type === "pull_request") {
    return parseBuildRequest(Buffer.from(canonicalJson({
      baseHead: value.baseHead,
      mergeHead: value.mergeHead,
      number: value.number,
      pullRequestHead: value.pullRequestHead,
      version: value.version,
    })));
  }
  if (value.type === "master") {
    return parseBuildRequest(Buffer.from(canonicalJson({
      head: value.head,
      kind: "master",
      version: value.version,
    })));
  }
  throw new BuildRequestError("invalid build request type");
}

function padding(length) {
  return Buffer.alloc(paddingLength(length));
}

function paddingLength(length) {
  return (512 - (length % 512)) % 512;
}

function isSafeArchivePart(value) {
  return value.length > 0 && value !== "." && value !== ".." &&
    !value.includes("/") && !value.includes("\\") && !value.includes("\0");
}

function sameKeys(left, right) {
  return left.length === right.length && left.every((value, index) => value === right[index]);
}

function record(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function throwIfAborted(signal) {
  if (signal?.aborted) throw signal.reason ?? new DOMException("Aborted", "AbortError");
}

if (resolve(process.argv[1] ?? "") === scriptPath) {
  process.umask(0o077);
  const stop = new AbortController();
  const onSignal = () =>
    stop.abort(new DOMException("helper stopped", "AbortError"));
  process.once("SIGINT", onSignal);
  process.once("SIGTERM", onSignal);
  try {
    await main({ signal: stop.signal });
  } catch (cause) {
    const message = cause instanceof Error ? cause.message : String(cause);
    process.stderr.write(
      `Cargo preparation helper failed: ${message.replace(/[\r\n]+/g, " ").slice(0, maximumDiagnosticBytes)}\n`,
    );
    process.exitCode = cause instanceof BuildRequestError ? buildRequestExitCode : 1;
  } finally {
    process.removeListener("SIGINT", onSignal);
    process.removeListener("SIGTERM", onSignal);
  }
}
