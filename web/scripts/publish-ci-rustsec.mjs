import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import {
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  readlink,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { isAbsolute, posix, resolve } from "node:path";
import { promisify } from "node:util";
import { fileURLToPath, pathToFileURL } from "node:url";
import { deflateRawSync } from "node:zlib";

const execFileAsync = promisify(execFile);
const scriptPath = fileURLToPath(import.meta.url);
const sha1Pattern = /^[a-f0-9]{40}$/;
const sha256Pattern = /^[a-f0-9]{64}$/;
const advisoryPattern = /^crates\/[^/]+\/RUSTSEC-[0-9]{4}-[0-9]{4}\.md$/;
const maximumCommandOutputBytes = 16 * 1024 * 1024;
const maximumErrorBytes = 1_000;
const maximumEntries = 50_000;

export const rustSecArchiveRoot =
  "advisory-dbs/advisory-db-3157b0e258782691";
export const maximumRawBytes = 64 * 1024 * 1024;
export const maximumTarBytes = 128 * 1024 * 1024;
export const maximumBundleBytes = 16 * 1024 * 1024;

export async function main({ env = process.env, log = console.log } = {}) {
  const repository = await resolveRepository(
    requiredEnvironment(env, "NANOCODEX_RUSTSEC_REPO"),
  );
  const origin = parseOrigin(requiredEnvironment(env, "NANOCODEX_CI_ORIGIN"));
  const token = requiredEnvironment(env, "NANOCODEX_CI_TOKEN");
  const snapshot = await readRustSecSnapshot(repository);
  const existing = await readPublishedBundle(origin, token, snapshot.revision);
  if (existing) {
    log(`CI RustSec advisory database is current (${snapshot.revision.slice(0, 7)})`);
    return existing;
  }

  const bundle = await buildRustSecBundle({ repository, snapshot });
  if (bundle.byteLength > maximumBundleBytes) {
    throw new Error(
      `CI RustSec bundle is ${bundle.byteLength} bytes; maximum is ${maximumBundleBytes}`,
    );
  }
  await assertRustSecSnapshot(repository, snapshot);
  const sha256 = createHash("sha256").update(bundle).digest("hex");
  const key = rustSecBundleKey(snapshot.revision);
  const response = await authenticatedFetch(
    `${origin}/api/ci/rustsec-advisory-db/${snapshot.revision}/bundle.tar.gz`,
    token,
    {
      method: "PUT",
      headers: {
        "content-length": String(bundle.byteLength),
        "content-type": "application/gzip",
        "x-nanocodex-sha256": sha256,
      },
      body: bundle,
    },
  );
  if (!response.ok) {
    throw new Error(await responseError("publish CI RustSec advisory database", response));
  }
  const published = await response.json().catch(() => undefined);
  if (
    published?.key !== key || published?.revision !== snapshot.revision ||
    published?.size !== bundle.byteLength || published?.sha256 !== sha256
  ) throw new Error("publish CI RustSec advisory database returned invalid metadata");
  log(
    `Published CI RustSec advisory database ${snapshot.revision.slice(0, 7)} ` +
      `(${bundle.byteLength} bytes)`,
  );
  return published;
}

export async function readRustSecSnapshot(repository) {
  const root = await resolveRepository(repository);
  const gitDirectory = resolve(root, ".git");
  const gitMetadata = await lstat(gitDirectory).catch(() => undefined);
  if (!gitMetadata?.isDirectory() || gitMetadata.isSymbolicLink()) {
    throw new Error("RustSec checkout must have its own .git directory");
  }
  const [topLevel, objectFormat, bare, revision, timestampText, status, tree] =
    await Promise.all([
      gitText(root, ["rev-parse", "--show-toplevel"]),
      gitText(root, ["rev-parse", "--show-object-format"]),
      gitText(root, ["rev-parse", "--is-bare-repository"]),
      gitText(root, ["rev-parse", "--verify", "HEAD^{commit}"]),
      gitText(root, ["show", "-s", "--format=%ct", "HEAD"]),
      gitBuffer(root, [
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--ignored=matching",
        "--ignore-submodules=all",
      ]),
      gitBuffer(root, ["ls-tree", "-rz", "--full-tree", "HEAD"]),
    ]);
  if (await realpath(topLevel) !== root || bare !== "false") {
    throw new Error("NANOCODEX_RUSTSEC_REPO must name the Git worktree root");
  }
  if (objectFormat !== "sha1" || !sha1Pattern.test(revision)) {
    throw new Error("RustSec HEAD must resolve to a full lowercase SHA-1 commit");
  }
  if (status.byteLength !== 0) {
    throw new Error(
      "CI RustSec publication requires a clean tracked and untracked worktree:\n" +
        status.toString("utf8").replaceAll("\0", "\n").slice(0, maximumErrorBytes),
    );
  }
  if (!/^(0|[1-9][0-9]*)$/.test(timestampText)) {
    throw new Error("RustSec HEAD has an invalid commit timestamp");
  }
  const commitTimestamp = Number(timestampText);
  if (!Number.isSafeInteger(commitTimestamp) || commitTimestamp > 0xffffffff) {
    throw new Error("RustSec HEAD commit timestamp is outside the supported range");
  }
  const trackedFiles = parseGitTree(tree);
  validateRustSecShape(trackedFiles);
  return { repository: root, revision, commitTimestamp, trackedFiles };
}

export function parseGitTree(output) {
  const body = Buffer.isBuffer(output) ? output : Buffer.from(output);
  const files = [];
  for (const record of splitNul(body)) {
    const tab = record.indexOf(0x09);
    if (tab < 0) throw new Error("Git returned a malformed RustSec tree entry");
    const metadata = record.subarray(0, tab).toString("ascii");
    const match = metadata.match(/^(100644|100755|120000|160000) (blob|commit) ([a-f0-9]{40})$/);
    if (!match) throw new Error("Git returned an unsupported RustSec tree entry");
    const pathBytes = record.subarray(tab + 1);
    const path = pathBytes.toString("utf8");
    if (!Buffer.from(path).equals(pathBytes)) {
      throw new Error("RustSec tree contains a non-UTF-8 path");
    }
    files.push({ mode: match[1], type: match[2], sha: match[3], path });
  }
  return files;
}

export function validateRustSecShape(files) {
  if (!Array.isArray(files) || files.length === 0 || files.length > maximumEntries) {
    throw new Error("RustSec checkout has an invalid tracked file count");
  }
  let previous = "";
  let hasSupportPolicy = false;
  let advisoryCount = 0;
  for (const file of files) {
    if (
      file == null || !["100644", "100755", "120000", "160000"].includes(file.mode) ||
      !["blob", "commit"].includes(file.type) || !sha1Pattern.test(file.sha ?? "") ||
      !isSafeRepositoryPath(file.path)
    ) throw new Error("RustSec checkout contains an invalid tracked entry");
    if (file.path <= previous) throw new Error("RustSec tracked paths are not uniquely ordered");
    previous = file.path;
    if (file.mode === "160000" || file.type === "commit") {
      throw new Error(`RustSec checkout must not contain gitlinks: ${file.path}`);
    }
    if (file.path === "support.toml" && file.type === "blob" && file.mode !== "120000") {
      hasSupportPolicy = true;
    }
    if (advisoryPattern.test(file.path) && file.type === "blob" && file.mode !== "120000") {
      advisoryCount++;
    }
  }
  if (!hasSupportPolicy || advisoryCount === 0) {
    throw new Error(
      "RustSec checkout must contain support.toml and at least one crates/*/RUSTSEC-YYYY-NNNN.md advisory",
    );
  }
  return { advisoryCount };
}

export async function buildRustSecBundle({ repository, snapshot }) {
  const temporaryDirectory = await mkdtemp(resolve(tmpdir(), "nanocodex-ci-rustsec-"));
  const materialized = resolve(temporaryDirectory, "advisory-db");
  try {
    await materializeRustSecRepository(repository, materialized, snapshot.revision);
    const entries = await collectRustSecArchiveEntries(materialized, snapshot.trackedFiles);
    const tar = createTar(entries, snapshot.commitTimestamp);
    if (tar.byteLength > maximumTarBytes) {
      throw new Error(`CI RustSec raw tar exceeds ${maximumTarBytes} bytes`);
    }
    return deterministicGzip(tar, snapshot.commitTimestamp);
  } finally {
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
}

async function materializeRustSecRepository(source, destination, revision) {
  await mkdir(destination, { recursive: true });
  await gitAt(destination, ["-c", "init.templateDir=", "init", "--quiet"]);
  await gitAt(destination, [
    "-c",
    "protocol.file.allow=always",
    "fetch",
    "--quiet",
    "--depth=1",
    "--no-tags",
    "--no-recurse-submodules",
    pathToFileURL(source).href,
    revision,
  ]);
  await writeFile(resolve(destination, ".git/HEAD"), `${revision}\n`);
  await gitAt(destination, ["read-tree", "--reset", revision]);
  await gitAt(destination, ["checkout-index", "--all"]);
  await Promise.all([
    rm(resolve(destination, ".git/FETCH_HEAD"), { force: true }),
    rm(resolve(destination, ".git/ORIG_HEAD"), { force: true }),
    rm(resolve(destination, ".git/logs"), { recursive: true, force: true }),
    rm(resolve(destination, ".git/hooks"), { recursive: true, force: true }),
    rm(resolve(destination, ".git/info/exclude"), { force: true }),
    rm(resolve(destination, ".git/description"), { force: true }),
  ]);
  await writeFile(
    resolve(destination, ".git/config"),
    "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = false\n",
  );
  const [head, status] = await Promise.all([
    gitText(destination, ["rev-parse", "--verify", "HEAD^{commit}"]),
    gitText(destination, ["status", "--porcelain=v1", "--untracked-files=all"]),
  ]);
  if (head !== revision || status !== "") {
    throw new Error("materialized RustSec repository is not the exact clean revision");
  }
}

export async function collectRustSecArchiveEntries(repository, trackedFiles, {
  rawByteLimit = maximumRawBytes,
} = {}) {
  const trackedModes = new Map(trackedFiles.map(({ path, mode }) => [path, mode]));
  const observedTracked = new Set();
  const entries = [];
  let rawBytes = 0;

  async function collect(sourcePath, archivePath, repositoryPath) {
    if (entries.length >= maximumEntries) {
      throw new Error(`CI RustSec archive exceeds ${maximumEntries} entries`);
    }
    if (!isSafeArchivePath(archivePath)) {
      throw new Error(`unsafe RustSec archive path: ${archivePath}`);
    }
    const metadata = await lstat(sourcePath);
    if (metadata.isDirectory()) {
      entries.push({ path: `${archivePath.replace(/\/$/, "")}/`, type: "directory", mode: 0o755 });
      const names = await readdir(sourcePath);
      names.sort();
      for (const name of names) {
        if (!isSafePathComponent(name)) {
          throw new Error(`unsafe RustSec path component: ${name}`);
        }
        const childRepositoryPath = repositoryPath === "" ? name : `${repositoryPath}/${name}`;
        await collect(
          resolve(sourcePath, name),
          `${archivePath.replace(/\/$/, "")}/${name}`,
          childRepositoryPath,
        );
      }
      return;
    }
    if (!repositoryPath.startsWith(".git/") && repositoryPath !== ".git") {
      const trackedMode = trackedModes.get(repositoryPath);
      if (!trackedMode) throw new Error(`untracked RustSec archive entry: ${repositoryPath}`);
      observedTracked.add(repositoryPath);
    }
    if (metadata.isFile()) {
      if (repositoryPath === ".git/objects/info/alternates") {
        throw new Error("RustSec checkout uses an external Git object alternate");
      }
      if (metadata.size > rawByteLimit - rawBytes) {
        throw new Error(`CI RustSec raw content exceeds ${rawByteLimit} bytes`);
      }
      const body = await readFile(sourcePath);
      rawBytes += body.byteLength;
      if (rawBytes > rawByteLimit) {
        throw new Error(`CI RustSec raw content exceeds ${rawByteLimit} bytes`);
      }
      entries.push({
        path: archivePath,
        type: "file",
        mode: trackedModes.get(repositoryPath) === "100755" ? 0o755 : 0o644,
        body,
      });
      return;
    }
    if (metadata.isSymbolicLink()) {
      const link = await readlink(sourcePath);
      rawBytes += Buffer.byteLength(link);
      if (rawBytes > rawByteLimit) {
        throw new Error(`CI RustSec raw content exceeds ${rawByteLimit} bytes`);
      }
      if (!isSafeSymlink(archivePath, link)) {
        throw new Error(`unsafe RustSec symlink: ${archivePath} -> ${link}`);
      }
      entries.push({ path: archivePath, type: "symlink", mode: 0o777, link });
      return;
    }
    throw new Error(`unsupported RustSec file type: ${repositoryPath || "."}`);
  }

  await collect(repository, rustSecArchiveRoot, "");
  for (const path of trackedModes.keys()) {
    if (!observedTracked.has(path)) throw new Error(`RustSec tracked file is missing: ${path}`);
  }
  entries.sort((left, right) => left.path < right.path ? -1 : left.path > right.path ? 1 : 0);
  return entries;
}

export function createTar(entries, mtime) {
  if (!Number.isSafeInteger(mtime) || mtime < 0 || mtime > 0xffffffff) {
    throw new Error("invalid tar mtime");
  }
  const chunks = [];
  let length = 0;
  let previous = "";
  let paxIndex = 0;
  const append = (...values) => {
    for (const value of values) length += value.byteLength;
    if (length > maximumTarBytes) throw new Error(`CI RustSec raw tar exceeds ${maximumTarBytes} bytes`);
    chunks.push(...values);
  };
  for (const entry of entries) {
    if (!isSafeArchivePath(entry.path) || entry.path <= previous) {
      throw new Error(`unsafe or unordered RustSec tar entry: ${entry.path}`);
    }
    previous = entry.path;
    if (!["file", "directory", "symlink"].includes(entry.type)) {
      throw new Error(`unsupported RustSec tar entry: ${entry.path}`);
    }
    if (
      (entry.type === "file" && !Buffer.isBuffer(entry.body)) ||
      (entry.type === "directory" && !entry.path.endsWith("/")) ||
      (entry.type === "symlink" && !isSafeSymlink(entry.path, entry.link))
    ) throw new Error(`invalid RustSec tar entry: ${entry.path}`);
    const pathParts = ustarPath(entry.path);
    const pax = [];
    if (!pathParts) pax.push(paxRecord("path", entry.path));
    if (entry.type === "symlink" && Buffer.byteLength(entry.link) > 100) {
      pax.push(paxRecord("linkpath", entry.link));
    }
    const entryIndex = paxIndex++;
    if (pax.length > 0) {
      const body = Buffer.from(pax.join(""));
      append(
        tarHeader({
          path: `PaxHeaders/${String(entryIndex).padStart(8, "0")}`,
          type: "pax",
          mode: 0o644,
          size: body.byteLength,
          mtime,
        }),
        body,
        padding(body.byteLength),
      );
    }
    const body = entry.type === "file" ? entry.body : Buffer.alloc(0);
    append(
      tarHeader({
        ...entry,
        path: pathParts ? entry.path : `PaxEntry/${String(entryIndex).padStart(8, "0")}`,
        link: entry.type === "symlink" && Buffer.byteLength(entry.link) <= 100
          ? entry.link
          : "",
        size: body.byteLength,
        mtime,
      }),
      body,
      padding(body.byteLength),
    );
  }
  append(Buffer.alloc(1024));
  return Buffer.concat(chunks, length);
}

export function deterministicGzip(body, mtime) {
  if (!Buffer.isBuffer(body) || !Number.isSafeInteger(mtime) || mtime < 0 || mtime > 0xffffffff) {
    throw new Error("invalid deterministic gzip input");
  }
  const header = Buffer.from([0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x02, 0xff]);
  header.writeUInt32LE(mtime, 4);
  const trailer = Buffer.alloc(8);
  trailer.writeUInt32LE(crc32(body), 0);
  trailer.writeUInt32LE(body.byteLength >>> 0, 4);
  return Buffer.concat([header, deflateRawSync(body, { level: 9 }), trailer]);
}

export async function readPublishedBundle(origin, token, revision) {
  if (!sha1Pattern.test(revision)) throw new Error("invalid RustSec revision");
  const response = await authenticatedFetch(
    `${origin}/api/ci/rustsec-advisory-db/${revision}/bundle.tar.gz`,
    token,
    { method: "HEAD" },
  );
  if (response.status === 404) return null;
  if (!response.ok) {
    throw new Error(await responseError("read CI RustSec advisory database", response));
  }
  const key = response.headers.get("x-nanocodex-key");
  const sha256 = response.headers.get("x-nanocodex-sha256");
  const size = Number(response.headers.get("content-length"));
  if (
    key !== rustSecBundleKey(revision) ||
    response.headers.get("x-nanocodex-revision") !== revision ||
    response.headers.get("content-type") !== "application/gzip" ||
    !sha256Pattern.test(sha256 ?? "") || !Number.isSafeInteger(size) ||
    size <= 0 || size > maximumBundleBytes
  ) throw new Error("read CI RustSec advisory database returned invalid metadata");
  return { key, revision, sha256, size };
}

export function rustSecBundleKey(revision) {
  if (!sha1Pattern.test(revision)) throw new Error("invalid RustSec revision");
  return `rustsec-advisory-db/${revision}/bundle.tar.gz`;
}

export function isSafeRepositoryPath(path) {
  return typeof path === "string" && path.length > 0 &&
    Buffer.byteLength(path) <= 4_096 && !path.startsWith("/") &&
    !/[\\\0\x00-\x1f\x7f]/.test(path) &&
    path.split("/").every(isSafePathComponent);
}

export function isSafeArchivePath(path) {
  if (typeof path !== "string" || Buffer.byteLength(path) > 4_096) return false;
  const normalized = path.endsWith("/") ? path.slice(0, -1) : path;
  return (normalized === rustSecArchiveRoot || normalized.startsWith(`${rustSecArchiveRoot}/`)) &&
    isSafeRepositoryPath(normalized);
}

export function isSafeSymlink(archivePath, link) {
  if (
    typeof link !== "string" || link.length === 0 || Buffer.byteLength(link) > 4_096 ||
    isAbsolute(link) || link.includes("\\") || /[\0\x00-\x1f\x7f]/.test(link)
  ) return false;
  const target = posix.normalize(posix.join(posix.dirname(archivePath), link));
  return target === rustSecArchiveRoot || target.startsWith(`${rustSecArchiveRoot}/`);
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

async function resolveRepository(path) {
  try {
    const root = await realpath(resolve(path));
    if (!(await lstat(root)).isDirectory()) throw new Error("not a directory");
    return root;
  } catch (cause) {
    throw new Error("NANOCODEX_RUSTSEC_REPO must name an existing local directory", { cause });
  }
}

async function assertRustSecSnapshot(repository, expected) {
  const observed = await readRustSecSnapshot(repository);
  if (
    observed.revision !== expected.revision ||
    observed.commitTimestamp !== expected.commitTimestamp ||
    !sameTrackedFiles(observed.trackedFiles, expected.trackedFiles)
  ) throw new Error("RustSec checkout changed while its CI bundle was being built");
}

function sameTrackedFiles(left, right) {
  return left.length === right.length && left.every((file, index) =>
    file.path === right[index]?.path && file.mode === right[index]?.mode &&
    file.type === right[index]?.type && file.sha === right[index]?.sha
  );
}

function splitNul(body) {
  const records = [];
  let start = 0;
  for (let index = 0; index < body.length; index++) {
    if (body[index] !== 0) continue;
    if (index > start) records.push(body.subarray(start, index));
    start = index + 1;
  }
  if (start !== body.length) throw new Error("Git returned unterminated NUL-delimited output");
  return records;
}

function isSafePathComponent(value) {
  return typeof value === "string" && value.length > 0 && value !== "." && value !== ".." &&
    !value.includes("/") && !/[\\\0\x00-\x1f\x7f]/.test(value);
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
  writeOctal(header, entry.mtime, 136, 12);
  header.fill(0x20, 148, 156);
  header[156] = { file: 0x30, symlink: 0x32, directory: 0x35, pax: 0x78 }[entry.type];
  writeText(header, entry.link ?? "", 157, 100);
  writeText(header, "ustar\0", 257, 6);
  writeText(header, "00", 263, 2);
  writeText(header, "root", 265, 32);
  writeText(header, "root", 297, 32);
  writeText(header, pathParts.prefix, 345, 155);
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

function crc32(body) {
  let crc = 0xffffffff;
  for (const byte of body) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1));
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function authenticatedFetch(url, token, init = {}) {
  const headers = new Headers(init.headers);
  headers.set("authorization", `Bearer ${token}`);
  return fetch(url, { ...init, headers, redirect: "error" });
}

async function responseError(operation, response) {
  const detail = (await response.text()).slice(0, maximumErrorBytes);
  return `${operation} failed with HTTP ${response.status}${detail ? `: ${detail}` : ""}`;
}

async function gitText(repository, args) {
  return (await gitBuffer(repository, args)).toString("utf8").trimEnd();
}

async function gitBuffer(repository, args) {
  try {
    const { stdout } = await execFileAsync("git", [
      "-c", "core.fsmonitor=false",
      "-c", "core.hooksPath=/dev/null",
      ...args,
    ], {
      cwd: repository,
      env: publisherEnvironment(),
      encoding: "buffer",
      maxBuffer: maximumCommandOutputBytes,
      env: {
        ...process.env,
        GIT_CONFIG_COUNT: "2",
        GIT_CONFIG_KEY_0: "fetch.recurseSubmodules",
        GIT_CONFIG_VALUE_0: "false",
        GIT_CONFIG_KEY_1: "core.fsmonitor",
        GIT_CONFIG_VALUE_1: "false",
        GIT_OPTIONAL_LOCKS: "0",
        GIT_TERMINAL_PROMPT: "0",
      },
    });
    return stdout;
  } catch (cause) {
    const detail = String(cause?.stderr ?? cause?.message ?? cause).trim().slice(0, maximumErrorBytes);
    throw new Error(`git ${args[0]} failed: ${detail}`, { cause });
  }
}

function publisherEnvironment() {
  return {
    PATH: process.env.PATH ?? "",
    TMPDIR: process.env.TMPDIR ?? tmpdir(),
    LANG: process.env.LANG ?? "C.UTF-8",
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_NO_REPLACE_OBJECTS: "1",
    GIT_OPTIONAL_LOCKS: "0",
    GIT_TERMINAL_PROMPT: "0",
  };
}

async function gitAt(repository, args) {
  await gitBuffer(repository, args);
}

function requiredEnvironment(env, name) {
  const value = env[name]?.trim();
  if (!value) throw new Error(`${name} is required`);
  return value;
}

if (resolve(process.argv[1] ?? "") === scriptPath) await main();
