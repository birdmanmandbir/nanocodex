import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import {
  lstat,
  mkdtemp,
  readFile,
  readdir,
  readlink,
  realpath,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, isAbsolute, resolve } from "node:path";
import { Readable } from "node:stream";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { deflateRawSync } from "node:zlib";

const execFileAsync = promisify(execFile);
const scriptPath = fileURLToPath(import.meta.url);
const projectRoot = resolve(dirname(scriptPath), "..");
const maximumBundleBytes = 16 * 1024 * 1024;
const maximumTarBytes = 128 * 1024 * 1024;
const maximumCommandOutputBytes = 16 * 1024 * 1024;
const maximumErrorBytes = 1_000;
const cargoHomeVendor = "/workspace/.cargo-home/vendor";
const sha1Pattern = /^[a-f0-9]{40}$/;
const sha256Pattern = /^[a-f0-9]{64}$/;

export async function main({ env = process.env, log = console.log } = {}) {
  const repository = resolve(env.NANOCODEX_REPO ?? resolve(projectRoot, ".."));
  const origin = parseOrigin(requiredEnvironment(env, "NANOCODEX_CI_ORIGIN"));
  const token = requiredEnvironment(env, "NANOCODEX_CI_TOKEN");
  const snapshot = await readCargoSnapshot(repository);
  const existing = await readPublishedBundle(origin, token, snapshot.cargoLockBlob);
  if (existing) {
    log(`CI Cargo vendor is current (${snapshot.cargoLockBlob.slice(0, 7)})`);
    return existing;
  }

  const temporaryDirectory = await mkdtemp(resolve(tmpdir(), "nanocodex-ci-cargo-vendor-"));
  try {
    const gitCheckouts = await readCleanCargoGitCheckouts(snapshot.cargoLock, env);
    const vendorDirectory = resolve(temporaryDirectory, "vendor");
    const { stdout } = await runCargo(repository, [
      "vendor",
      "--offline",
      "--locked",
      "--versioned-dirs",
      vendorDirectory,
    ]);
    const config = cargoGitSourceConfig(stdout, snapshot.cargoLock);
    const directories = await selectGitVendorDirectories(
      vendorDirectory,
      snapshot.cargoLock,
    );
    const extraDirectories = await cargoVendorExtraDirectories(
      snapshot.cargoLock,
      env,
      gitCheckouts,
    );
    const bundlePath = resolve(temporaryDirectory, "bundle.tar.gz");
    await buildCargoVendorBundle({
      bundlePath,
      config,
      directories,
      extraDirectories,
      vendorDirectory,
    });
    const bundle = await describeArtifact(bundlePath);
    if (bundle.size > maximumBundleBytes) {
      throw new Error(
        `CI Cargo vendor bundle is ${bundle.size} bytes; maximum is ${maximumBundleBytes}`,
      );
    }
    await assertCargoSnapshot(repository, snapshot);
    await assertCleanCargoGitCheckouts(gitCheckouts);
    const key = cargoVendorKey(snapshot.cargoLockBlob);
    const response = await authenticatedFetch(
      `${origin}/api/ci/cargo-vendor/${snapshot.cargoLockBlob}/bundle.tar.gz`,
      token,
      {
        method: "PUT",
        headers: {
          "content-length": String(bundle.size),
          "content-type": "application/gzip",
          "x-nanocodex-sha256": bundle.sha256,
        },
        body: Readable.toWeb(createReadStream(bundle.path)),
        duplex: "half",
      },
    );
    if (!response.ok) {
      throw new Error(await responseError("publish CI Cargo vendor", response));
    }
    const published = await response.json().catch(() => undefined);
    if (
      published?.key !== key || published?.cargoLockBlob !== snapshot.cargoLockBlob ||
      published?.size !== bundle.size || published?.sha256 !== bundle.sha256
    ) throw new Error("publish CI Cargo vendor returned invalid metadata");
    log(
      `Published CI Cargo vendor ${snapshot.cargoLockBlob.slice(0, 7)} ` +
      `(${directories.length} packages, ${bundle.size} bytes)`,
    );
    return published;
  } finally {
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
}

export async function readCargoSnapshot(repository) {
  const [head, cargoLockBlob, cargoLock, status] = await Promise.all([
    git(repository, ["rev-parse", "--verify", "HEAD^{commit}"]),
    git(repository, ["rev-parse", "--verify", "HEAD:Cargo.lock"]),
    git(repository, ["show", "HEAD:Cargo.lock"]),
    git(repository, ["status", "--porcelain=v1", "--untracked-files=no"]),
  ]);
  if (!sha1Pattern.test(head) || !sha1Pattern.test(cargoLockBlob)) {
    throw new Error("Git resolved an invalid commit or Cargo.lock blob ID");
  }
  if (status !== "") {
    throw new Error(
      `CI Cargo vendor publication requires a clean index and tracked worktree:\n` +
      status.slice(0, 1_000),
    );
  }
  return { head, cargoLockBlob, cargoLock };
}

export function cargoGitSourceConfig(cargoVendorOutput, cargoLock) {
  const expected = new Set(gitPackagesFromCargoLock(cargoLock).map(({ source }) =>
    source.slice(0, source.lastIndexOf("#"))
  ));
  if (expected.size === 0) throw new Error("Cargo.lock contains no git source packages");
  const blocks = parseSourceBlocks(cargoVendorOutput);
  const gitBlocks = blocks.filter(({ header }) => header.startsWith("[source.\"git+"));
  const observed = new Set(gitBlocks.map(({ header }) => header.slice(9, -2)));
  if (observed.size !== expected.size || [...expected].some((source) => !observed.has(source))) {
    throw new Error("cargo vendor source config does not match Cargo.lock git sources");
  }
  for (const block of gitBlocks) {
    if (!block.lines.includes('replace-with = "vendored-sources"')) {
      throw new Error(`cargo vendor omitted source replacement in ${block.header}`);
    }
  }
  gitBlocks.sort((left, right) => left.header.localeCompare(right.header));
  return `${gitBlocks.map(({ header, lines }) => `${header}\n${lines.join("\n")}`)
    .join("\n\n")}\n\n[source.vendored-sources]\n` +
    `directory = "${cargoHomeVendor}"\n`;
}

export async function selectGitVendorDirectories(vendorDirectory, cargoLock) {
  const expected = new Set(gitPackagesFromCargoLock(cargoLock).map(
    ({ name, version }) => `${name}-${version}`,
  ));
  const entries = await readdir(vendorDirectory, { withFileTypes: true });
  const selected = [];
  for (const entry of entries.sort((left, right) => left.name.localeCompare(right.name))) {
    if (!entry.isDirectory() || !isSafeArchivePart(entry.name)) {
      throw new Error(`cargo vendor produced unsupported entry: ${entry.name}`);
    }
    const checksumPath = resolve(vendorDirectory, entry.name, ".cargo-checksum.json");
    let checksum;
    try {
      checksum = JSON.parse(await readFile(checksumPath, "utf8"));
    } catch (cause) {
      throw new Error(`cargo vendor produced an invalid checksum for ${entry.name}`, { cause });
    }
    if (checksum?.package === null) selected.push(entry.name);
    else if (typeof checksum?.package !== "string" || !sha256Pattern.test(checksum.package)) {
      throw new Error(`cargo vendor produced an invalid package checksum for ${entry.name}`);
    }
  }
  const observed = new Set(selected);
  if (observed.size !== expected.size || [...expected].some((name) => !observed.has(name))) {
    throw new Error("vendored git package directories do not match Cargo.lock");
  }
  return selected;
}

export async function buildCargoVendorBundle({
  bundlePath,
  config,
  directories,
  extraDirectories = [],
  vendorDirectory,
}) {
  const entries = [{ path: "config.toml", type: "file", mode: 0o644, body: Buffer.from(config) }];
  entries.push({ path: "vendor/", type: "directory", mode: 0o755 });
  for (const directory of [...directories].sort()) {
    await collectArchiveEntries(
      resolve(vendorDirectory, directory),
      `vendor/${directory}`,
      entries,
    );
  }
  for (const { sourcePath, archivePath } of extraDirectories) {
    if (!isSafeArchivePart(archivePath)) {
      throw new Error(`unsafe Cargo vendor extra path: ${archivePath}`);
    }
    await collectArchiveEntries(sourcePath, archivePath, entries);
  }
  entries.sort((left, right) => left.path.localeCompare(right.path));
  const tar = createTar(entries);
  if (tar.byteLength > maximumTarBytes) {
    throw new Error(`CI Cargo vendor tar exceeds ${maximumTarBytes} bytes`);
  }
  await writeFile(bundlePath, deterministicGzip(tar));
}

export async function cargoVendorExtraDirectories(
  cargoLock,
  env = process.env,
  cleanCheckouts,
) {
  const dependency = gitPackagesFromCargoLock(cargoLock).find(
    ({ name }) => name === "krun-init-blob",
  );
  if (!dependency) return [];
  const commit = dependency.source.slice(dependency.source.lastIndexOf("#") + 1);
  if (!sha1Pattern.test(commit)) {
    throw new Error("krun-init-blob has an invalid Git commit in Cargo.lock");
  }
  const checkouts = cleanCheckouts ?? await readCleanCargoGitCheckouts(cargoLock, env);
  const checkout = checkouts.find((candidate) => candidate.revision === commit)?.path;
  if (!checkout) throw new Error(`missing verified krun-init checkout for ${commit}`);
  const init = resolve(checkout, "init");
  if (!(await stat(resolve(init, "Cargo.toml"))).isFile()) {
    throw new Error(`verified krun-init checkout ${commit} has no init/Cargo.toml`);
  }
  return [{ sourcePath: init, archivePath: "init" }];
}

export async function readCleanCargoGitCheckouts(cargoLock, env = process.env) {
  const cargoHome = env.CARGO_HOME?.trim() ||
    (env.HOME?.trim() ? resolve(env.HOME, ".cargo") : "");
  if (!cargoHome) throw new Error("CARGO_HOME or HOME is required to verify Cargo Git sources");
  const root = resolve(cargoHome, "git/checkouts");
  const repositories = await readdir(root, { withFileTypes: true }).catch(() => []);
  const revisions = [...new Set(gitPackagesFromCargoLock(cargoLock).map(({ source }) =>
    source.slice(source.lastIndexOf("#") + 1)
  ))].sort();
  const checkouts = [];
  for (const revision of revisions) {
    if (!sha1Pattern.test(revision)) {
      throw new Error(`Cargo.lock contains an invalid Git revision: ${revision}`);
    }
    const matches = [];
    for (const repository of repositories) {
      if (!repository.isDirectory()) continue;
      const candidate = resolve(root, repository.name, revision.slice(0, 7));
      try {
        if (!(await lstat(candidate)).isDirectory()) continue;
        if (await git(candidate, ["rev-parse", "--verify", "HEAD^{commit}"]) === revision) {
          matches.push(await realpath(candidate));
        }
      } catch {
        continue;
      }
    }
    if (matches.length !== 1) {
      throw new Error(`expected one cached Cargo Git checkout for ${revision}, found ${matches.length}`);
    }
    checkouts.push(await cleanCargoGitCheckout(matches[0], revision));
  }
  return checkouts;
}

async function cleanCargoGitCheckout(path, revision) {
  const gitDirectory = await lstat(resolve(path, ".git")).catch(() => undefined);
  if (!gitDirectory?.isDirectory() || gitDirectory.isSymbolicLink()) {
    throw new Error(`Cargo Git checkout ${revision} must own its .git directory`);
  }
  await assertSafeLocalGitConfig(path, revision);
  const [head, tree, topLevel, status] = await Promise.all([
    git(path, ["rev-parse", "--verify", "HEAD^{commit}"]),
    git(path, ["rev-parse", "--verify", "HEAD^{tree}"]),
    git(path, ["rev-parse", "--show-toplevel"]),
    git(path, [
      "status",
      "--porcelain=v1",
      "--untracked-files=all",
      "--ignored=matching",
      "--ignore-submodules=none",
    ]),
  ]);
  const unexpected = status.split("\n").filter((line) => line !== "" && line !== "?? .cargo-ok");
  if (
    head !== revision || !sha1Pattern.test(tree) ||
    await realpath(topLevel) !== path || unexpected.length > 0
  ) {
    throw new Error(
      `Cargo Git checkout ${revision} is dirty or mismatched:\n` +
      unexpected.join("\n").slice(0, maximumErrorBytes),
    );
  }
  return { path, revision, tree };
}

async function assertCleanCargoGitCheckouts(expected) {
  for (const checkout of expected) {
    const observed = await cleanCargoGitCheckout(checkout.path, checkout.revision);
    if (observed.tree !== checkout.tree) {
      throw new Error(`Cargo Git checkout ${checkout.revision} changed during publication`);
    }
  }
}

export async function readPublishedBundle(origin, token, cargoLockBlob) {
  const response = await authenticatedFetch(
    `${origin}/api/ci/cargo-vendor/${cargoLockBlob}/bundle.tar.gz`,
    token,
    { method: "HEAD" },
  );
  if (response.status === 404) return null;
  if (!response.ok) throw new Error(await responseError("read CI Cargo vendor", response));
  const key = response.headers.get("x-nanocodex-key");
  const sha256 = response.headers.get("x-nanocodex-sha256");
  const size = Number(response.headers.get("content-length"));
  if (
    key !== cargoVendorKey(cargoLockBlob) ||
    response.headers.get("x-nanocodex-cargo-lock") !== cargoLockBlob ||
    !sha256Pattern.test(sha256 ?? "") ||
    !Number.isSafeInteger(size) || size <= 0 || size > maximumBundleBytes
  ) throw new Error("read CI Cargo vendor returned invalid metadata");
  return { key, cargoLockBlob, sha256, size };
}

function parseSourceBlocks(output) {
  const lines = output.replaceAll("\r\n", "\n").split("\n");
  const blocks = [];
  let current;
  for (const line of lines) {
    if (/^\[source\.[^\]]+\]$/.test(line)) {
      if (current) blocks.push(current);
      current = { header: line, lines: [] };
    } else if (current && line.startsWith("[")) {
      blocks.push(current);
      current = undefined;
    } else if (current && line.trim() !== "") {
      current.lines.push(line.trimEnd());
    }
  }
  if (current) blocks.push(current);
  return blocks;
}

function gitPackagesFromCargoLock(cargoLock) {
  const packages = [];
  for (const block of cargoLock.replaceAll("\r\n", "\n").split("\n\n")) {
    const name = block.match(/^name = "([^"]+)"$/m)?.[1];
    const version = block.match(/^version = "([^"]+)"$/m)?.[1];
    const source = block.match(/^source = "(git\+[^"]+)"$/m)?.[1];
    if (name && version && source && source.includes("#")) packages.push({ name, version, source });
  }
  return packages;
}

async function collectArchiveEntries(sourcePath, archivePath, entries) {
  const metadata = await lstat(sourcePath);
  if (metadata.isDirectory()) {
    entries.push({ path: `${archivePath}/`, type: "directory", mode: 0o755 });
    for (const name of (await readdir(sourcePath)).sort()) {
      if (!isSafeArchivePart(name)) throw new Error(`unsafe vendored path component: ${name}`);
      await collectArchiveEntries(resolve(sourcePath, name), `${archivePath}/${name}`, entries);
    }
    return;
  }
  if (metadata.isFile()) {
    entries.push({
      path: archivePath,
      type: "file",
      mode: metadata.mode & 0o111 ? 0o755 : 0o644,
      body: await readFile(sourcePath),
    });
    return;
  }
  if (metadata.isSymbolicLink()) {
    const link = await readlink(sourcePath);
    if (isAbsolute(link) || link.split(/[\\/]/).includes("..") || link.includes("\0")) {
      throw new Error(`unsafe vendored symlink: ${archivePath}`);
    }
    entries.push({ path: archivePath, type: "symlink", mode: 0o777, link });
    return;
  }
  throw new Error(`unsupported vendored file type: ${archivePath}`);
}

function createTar(entries) {
  const chunks = [];
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
      chunks.push(tarHeader({
        path: `PaxHeaders/${String(paxIndex++).padStart(8, "0")}`,
        type: "pax",
        mode: 0o644,
        size: body.byteLength,
      }), body, padding(body.byteLength));
    }
    const body = entry.type === "file" ? entry.body : Buffer.alloc(0);
    chunks.push(tarHeader({
      ...entry,
      path: pathParts ? entry.path : `PaxEntry/${String(paxIndex).padStart(8, "0")}`,
      link: entry.type === "symlink" && Buffer.byteLength(entry.link) <= 100 ? entry.link : "",
      size: body.byteLength,
    }), body, padding(body.byteLength));
  }
  chunks.push(Buffer.alloc(1024));
  return Buffer.concat(chunks);
}

function tarHeader(entry) {
  const header = Buffer.alloc(512);
  const pathParts = ustarPath(entry.path);
  if (!pathParts) throw new Error(`tar path cannot be represented: ${entry.path}`);
  writeText(header, pathParts.name, 0, 100);
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
  writeText(header, pathParts.prefix, 345, 155);
  const checksum = header.reduce((sum, byte) => sum + byte, 0);
  const encoded = checksum.toString(8).padStart(6, "0");
  header.write(encoded, 148, 6, "ascii");
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
    const actual = Buffer.byteLength(record);
    if (actual === length) return record;
    length = actual;
  }
}

function writeText(header, value, offset, length) {
  const bytes = Buffer.from(value);
  if (bytes.byteLength > length) throw new Error(`tar field exceeds ${length} bytes`);
  bytes.copy(header, offset);
}

function writeOctal(header, value, offset, length) {
  if (!Number.isSafeInteger(value) || value < 0) throw new Error("invalid tar integer");
  const encoded = value.toString(8).padStart(length - 1, "0");
  if (encoded.length >= length) throw new Error("tar integer overflow");
  header.write(encoded, offset, length - 1, "ascii");
}

function padding(length) {
  return Buffer.alloc((512 - (length % 512)) % 512);
}

function deterministicGzip(body) {
  const header = Buffer.from([0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x02, 0xff]);
  const trailer = Buffer.alloc(8);
  trailer.writeUInt32LE(crc32(body), 0);
  trailer.writeUInt32LE(body.byteLength >>> 0, 4);
  return Buffer.concat([header, deflateRawSync(body, { level: 9 }), trailer]);
}

function crc32(body) {
  let crc = 0xffffffff;
  for (const byte of body) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1));
  }
  return (crc ^ 0xffffffff) >>> 0;
}

async function assertCargoSnapshot(repository, expected) {
  const observed = await readCargoSnapshot(repository);
  if (observed.head !== expected.head || observed.cargoLockBlob !== expected.cargoLockBlob) {
    throw new Error("repository changed while its CI Cargo vendor bundle was being built");
  }
}

async function runCargo(repository, args) {
  try {
    return await execFileAsync("cargo", args, {
      cwd: repository,
      env: publisherEnvironment(repository, false),
      encoding: "utf8",
      maxBuffer: maximumCommandOutputBytes,
    });
  } catch (cause) {
    const detail = String(cause?.stderr ?? cause?.message ?? cause).trim().slice(0, 2_000);
    throw new Error(`cargo vendor failed: ${detail}`, { cause });
  }
}

async function git(repository, args) {
  try {
    const { stdout } = await execFileAsync("git", [
      "-c", "core.fsmonitor=false",
      "-c", "core.hooksPath=/dev/null",
      "-c", "core.attributesFile=/dev/null",
      "-c", "core.autocrlf=false",
      ...args,
    ], {
      cwd: repository,
      env: publisherEnvironment(repository),
      encoding: "utf8",
      maxBuffer: maximumCommandOutputBytes,
    });
    return stdout.trimEnd();
  } catch (cause) {
    const detail = String(cause?.stderr ?? cause?.message ?? cause).trim().slice(0, 1_000);
    throw new Error(`git ${args[0]} failed: ${detail}`, { cause });
  }
}

async function assertSafeLocalGitConfig(repository, revision) {
  const names = (await Promise.all(["--local", "--worktree"].map(async (scope) =>
    (await git(repository, [
      "config",
      scope,
      "--no-includes",
      "--name-only",
      "--list",
    ])).split("\n").filter(Boolean)
  ))).flat();
  const safe = /^(?:core\.(?:repositoryformatversion|filemode|bare|logallrefupdates|ignorecase|precomposeunicode|autocrlf)|remote\.[^.]+\.(?:url|fetch)|branch\.[^.]+\.(?:remote|merge)|submodule\..+\.url)$/i;
  const unsafe = names.filter((name) => !safe.test(name));
  if (unsafe.length > 0) {
    throw new Error(
      `Cargo Git checkout ${revision} rejects local Git configuration: ${unsafe.join(", ")}`,
    );
  }
}

function publisherEnvironment(repository, fixedWorktree = true) {
  return {
    PATH: process.env.PATH ?? "",
    HOME: process.env.HOME ?? "",
    TMPDIR: process.env.TMPDIR ?? tmpdir(),
    LANG: process.env.LANG ?? "C.UTF-8",
    CARGO_HOME: process.env.CARGO_HOME ?? resolve(process.env.HOME ?? "", ".cargo"),
    RUSTUP_HOME: process.env.RUSTUP_HOME ?? resolve(process.env.HOME ?? "", ".rustup"),
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_NO_REPLACE_OBJECTS: "1",
    GIT_OPTIONAL_LOCKS: "0",
    GIT_TERMINAL_PROMPT: "0",
    CARGO_NET_OFFLINE: "true",
    ...(fixedWorktree ? { GIT_WORK_TREE: repository } : {}),
  };
}

async function describeArtifact(path) {
  const size = (await stat(path)).size;
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return { path, size, sha256: hash.digest("hex") };
}

function cargoVendorKey(cargoLockBlob) {
  return `cargo-vendor/${cargoLockBlob}/bundle.tar.gz`;
}

function authenticatedFetch(url, token, init = {}) {
  const headers = new Headers(init.headers);
  headers.set("authorization", `Bearer ${token}`);
  return fetch(url, { ...init, headers });
}

async function responseError(operation, response) {
  const detail = (await response.text()).slice(0, maximumErrorBytes);
  return `${operation} failed with HTTP ${response.status}${detail ? `: ${detail}` : ""}`;
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

function isSafeArchivePart(value) {
  return value.length > 0 && value !== "." && value !== ".." &&
    !value.includes("/") && !value.includes("\\") && !value.includes("\0");
}

function requiredEnvironment(env, name) {
  const value = env[name]?.trim();
  if (!value) throw new Error(`${name} is required`);
  return value;
}

if (resolve(process.argv[1] ?? "") === scriptPath) await main();
