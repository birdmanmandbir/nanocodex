import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { lstat, mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { Readable } from "node:stream";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { deflateRawSync } from "node:zlib";

const execFileAsync = promisify(execFile);
const scriptPath = fileURLToPath(import.meta.url);
const projectRoot = resolve(dirname(scriptPath), "..");
const publicationBranch = "master";
const publicationRef = `refs/heads/${publicationBranch}`;
const maximumGitOutputBytes = 16 * 1024 * 1024;
const maximumStateBytes = 1024 * 1024;
const maximumErrorBytes = 1_000;

export async function main({ env = process.env, log = console.log } = {}) {
  const repository = resolve(env.NANOCODEX_REPO ?? resolve(projectRoot, ".."));
  const origin = parseOrigin(requiredEnvironment(env, "NANOCODEX_CI_ORIGIN"));
  const token = requiredEnvironment(env, "NANOCODEX_CI_TOKEN");
  const snapshot = await readRepositorySnapshot(repository);
  const expectedHead = await readSourceHead(origin, token);
  if (expectedHead === snapshot.head) {
    log(`CI source is current (${snapshot.head.slice(0, 7)})`);
    return;
  }
  await requireFastForward(repository, expectedHead, snapshot.head);
  const cargoLockBlob = await git(repository, [
    "rev-parse",
    "--verify",
    `${snapshot.head}:Cargo.lock`,
  ]);
  if (!/^[a-f0-9]{40}$/.test(cargoLockBlob)) {
    throw new Error("Git resolved an invalid Cargo.lock blob ID");
  }
  const cargoVendor = await readCargoVendor(origin, token, cargoLockBlob);
  const rustSecRevision = requiredEnvironment(env, "NANOCODEX_RUSTSEC_REVISION");
  if (!/^[a-f0-9]{40}$/.test(rustSecRevision)) {
    throw new Error("NANOCODEX_RUSTSEC_REVISION must be a full lowercase Git SHA-1");
  }
  const rustSec = await readRustSec(origin, token, rustSecRevision);
  const temporaryDirectory = await mkdtemp(resolve(tmpdir(), "nanocodex-ci-source-"));

  try {
    const artifacts = await buildSourceArtifacts({
      repository,
      temporaryDirectory,
      head: snapshot.head,
    });
    await assertRepositorySnapshot(repository, snapshot);

    await Promise.all([
      uploadFile(
        origin,
        token,
        `${snapshot.head}/source.tar.gz`,
        artifacts.archive,
        "application/gzip",
      ),
      uploadFile(
        origin,
        token,
        `${snapshot.head}/tree.json`,
        artifacts.tree,
        "application/json",
      ),
    ]);
    await assertRepositorySnapshot(repository, snapshot);

    const publication = {
      version: 1,
      head: snapshot.head,
      branch: publicationBranch,
      ref: publicationRef,
      archive: artifactRecord(
        `sources/${snapshot.head}/source.tar.gz`,
        artifacts.archive,
      ),
      tree: artifactRecord(`sources/${snapshot.head}/tree.json`, artifacts.tree),
      cargoLockBlob,
      cargoVendor,
      rustSecRevision,
      rustSec,
      publishedAt: new Date().toISOString(),
    };
    const body = Buffer.from(JSON.stringify({ expectedHead, publication }), "utf8");
    const response = await authenticatedFetch(
      `${origin}/api/ci/source/publish`,
      token,
      {
        method: "PUT",
        headers: {
          "content-length": String(body.byteLength),
          "content-type": "application/json",
        },
        body,
      },
    );
    if (!response.ok) throw new Error(await responseError("publish CI source", response));
    log(`Published CI source ${snapshot.head.slice(0, 7)}`);
  } finally {
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
}

async function requireFastForward(repository, expectedHead, head) {
  if (expectedHead == null) return;
  try {
    await git(repository, ["merge-base", "--is-ancestor", expectedHead, head]);
  } catch (error) {
    throw new Error(
      `CI source ${head.slice(0, 7)} must advance published head ${expectedHead.slice(0, 7)}`,
      { cause: error },
    );
  }
}

export async function readRepositorySnapshot(repository) {
  await assertSafeLocalGitConfig(repository);
  await rejectObjectSubstitution(repository);
  const head = await git(repository, ["rev-parse", "--verify", "HEAD^{commit}"]);
  if (!/^[a-f0-9]{40}$/.test(head)) {
    throw new Error(`Git resolved an invalid HEAD object ID: ${head.slice(0, 80)}`);
  }

  let ref;
  try {
    ref = await git(repository, ["symbolic-ref", "--quiet", "HEAD"]);
  } catch (error) {
    throw new Error("CI source publication requires an attached master branch", {
      cause: error,
    });
  }
  if (ref !== publicationRef) {
    throw new Error(
      `CI source publication requires ${publicationRef}; repository is on ${ref}`,
    );
  }

  await requireCleanTrackedState(repository);
  return { head, ref };
}

export async function buildSourceArtifacts({ repository, temporaryDirectory, head }) {
  const archivePath = resolve(temporaryDirectory, "source.tar.gz");
  const tarPath = resolve(temporaryDirectory, "source.tar");
  const treePath = resolve(temporaryDirectory, "tree.json");
  await git(repository, [
    "archive",
    "--format=tar",
    `--prefix=nanocodex-${head}/`,
    `--output=${tarPath}`,
    head,
  ]);
  const tar = await readFile(tarPath);
  await writeFile(archivePath, deterministicGzip(tar));
  await rm(tarPath);

  const rawTree = await gitBuffer(repository, [
    "ls-tree",
    "-r",
    "-z",
    "--full-tree",
    head,
  ]);
  const archive = await describeArtifact(archivePath);
  const treeDocument = {
    version: 1,
    head,
    archive: { size: archive.size, sha256: archive.sha256 },
    files: parseGitTree(rawTree),
  };
  await writeFile(treePath, `${JSON.stringify(treeDocument)}\n`);

  const tree = await describeArtifact(treePath);
  return { archive, tree, treeDocument };
}

export function parseGitTree(rawTree) {
  if (!Buffer.isBuffer(rawTree)) throw new TypeError("Git tree output must be a Buffer");
  const files = [];
  let offset = 0;
  const decoder = new TextDecoder("utf-8", { fatal: true });
  while (offset < rawTree.length) {
    const end = rawTree.indexOf(0, offset);
    if (end < 0) throw new Error("git ls-tree returned an unterminated entry");
    const record = decoder.decode(rawTree.subarray(offset, end));
    offset = end + 1;
    const tab = record.indexOf("\t");
    const metadata = tab < 0 ? [] : record.slice(0, tab).split(" ");
    const path = tab < 0 ? "" : record.slice(tab + 1);
    if (metadata.length !== 3 || path.length === 0) {
      throw new Error("git ls-tree returned a malformed entry");
    }
    const [mode, type, sha] = metadata;
    if (mode === "160000" || type === "commit") {
      throw new Error(`CI source archives do not support gitlinks: ${path}`);
    }
    if (
      type !== "blob" ||
      !["100644", "100755", "120000"].includes(mode) ||
      !/^[a-f0-9]{40}$/.test(sha)
    ) {
      throw new Error(`git ls-tree returned an unsupported entry: ${path}`);
    }
    files.push({ path, sha, mode });
  }
  return files;
}

export async function readSourceHead(origin, token) {
  const response = await authenticatedFetch(`${origin}/api/ci/source/state`, token, {
    headers: { accept: "application/json" },
  });
  if (response.status === 404) return null;
  if (!response.ok) throw new Error(await responseError("read CI source state", response));
  const text = await boundedResponseText(response, maximumStateBytes);
  let state;
  try {
    state = JSON.parse(text);
  } catch {
    throw new Error("read CI source state returned invalid JSON");
  }
  const head = state?.publication?.head;
  if (typeof head !== "string" || !/^[a-f0-9]{40}$/.test(head)) {
    throw new Error("read CI source state returned an invalid publication head");
  }
  return head;
}

export async function readCargoVendor(origin, token, cargoLockBlob) {
  const response = await authenticatedFetch(
    `${origin}/api/ci/cargo-vendor/${cargoLockBlob}/bundle.tar.gz`,
    token,
    { method: "HEAD" },
  );
  if (response.status === 404) {
    throw new Error(
      `CI Cargo vendor ${cargoLockBlob.slice(0, 7)} is not published; ` +
      "run npm run publish:ci-cargo-vendor --prefix web first",
    );
  }
  if (!response.ok) throw new Error(await responseError("read CI Cargo vendor", response));
  const key = response.headers.get("x-nanocodex-key");
  const sha256 = response.headers.get("x-nanocodex-sha256");
  const size = Number(response.headers.get("content-length"));
  if (
    key !== `cargo-vendor/${cargoLockBlob}/bundle.tar.gz` ||
    response.headers.get("x-nanocodex-cargo-lock") !== cargoLockBlob ||
    typeof sha256 !== "string" || !/^[a-f0-9]{64}$/.test(sha256) ||
    !Number.isSafeInteger(size) || size <= 0 || size > 16 * 1024 * 1024
  ) throw new Error("read CI Cargo vendor returned invalid metadata");
  return { key, size, sha256 };
}

export async function readRustSec(origin, token, revision) {
  const response = await authenticatedFetch(
    `${origin}/api/ci/rustsec-advisory-db/${revision}/bundle.tar.gz`,
    token,
    { method: "HEAD" },
  );
  if (response.status === 404) {
    throw new Error(
      `CI RustSec advisory database ${revision.slice(0, 7)} is not published; ` +
      "run npm run publish:ci-rustsec --prefix web first",
    );
  }
  if (!response.ok) throw new Error(await responseError("read CI RustSec advisory database", response));
  const key = response.headers.get("x-nanocodex-key");
  const sha256 = response.headers.get("x-nanocodex-sha256");
  const size = Number(response.headers.get("content-length"));
  if (
    key !== `rustsec-advisory-db/${revision}/bundle.tar.gz` ||
    response.headers.get("x-nanocodex-revision") !== revision ||
    typeof sha256 !== "string" || !/^[a-f0-9]{64}$/.test(sha256) ||
    !Number.isSafeInteger(size) || size <= 0 || size > 16 * 1024 * 1024
  ) throw new Error("read CI RustSec advisory database returned invalid metadata");
  return { key, size, sha256 };
}

async function assertRepositorySnapshot(repository, expected) {
  const observed = await readRepositorySnapshot(repository);
  if (observed.head !== expected.head || observed.ref !== expected.ref) {
    throw new Error("repository changed while its CI source publication was being built");
  }
}

async function requireCleanTrackedState(repository) {
  const status = await git(repository, [
    "status",
    "--porcelain=v1",
    "--untracked-files=no",
  ]);
  if (status !== "") {
    throw new Error(
      `CI source publication requires a clean index and tracked worktree:\n${status.slice(0, 1_000)}`,
    );
  }
}

async function describeArtifact(path) {
  const size = (await stat(path)).size;
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return { path, size, sha256: hash.digest("hex") };
}

function artifactRecord(key, artifact) {
  return { key, size: artifact.size, sha256: artifact.sha256 };
}

async function uploadFile(origin, token, remote, artifact, contentType) {
  const headers = {
    "content-length": String(artifact.size),
    "content-type": contentType,
    "x-nanocodex-sha256": artifact.sha256,
  };
  const body = Readable.toWeb(createReadStream(artifact.path));
  const response = await authenticatedFetch(
    `${origin}/api/ci/source/objects/${remote}`,
    token,
    { method: "PUT", headers, body, duplex: "half" },
  );
  if (!response.ok) {
    throw new Error(await responseError(`upload ${remote}`, response));
  }
}

function authenticatedFetch(url, token, init = {}) {
  const headers = new Headers(init.headers);
  headers.set("authorization", `Bearer ${token}`);
  return fetch(url, { ...init, headers });
}

async function responseError(operation, response) {
  const detail = await boundedResponseText(response, maximumErrorBytes);
  return `${operation} failed with HTTP ${response.status}${detail ? `: ${detail}` : ""}`;
}

async function boundedResponseText(response, maximumBytes) {
  if (response.body == null) return "";
  const reader = response.body.getReader();
  const chunks = [];
  let bytes = 0;
  let truncated = false;
  try {
    while (bytes <= maximumBytes) {
      const { done, value } = await reader.read();
      if (done) break;
      chunks.push(Buffer.from(value));
      bytes += value.byteLength;
      if (bytes > maximumBytes) {
        truncated = true;
        await reader.cancel();
        break;
      }
    }
  } finally {
    reader.releaseLock();
  }
  const text = Buffer.concat(chunks, bytes).subarray(0, maximumBytes).toString("utf8");
  return truncated ? `${text}…` : text;
}

async function git(repository, args) {
  return (await gitBuffer(repository, args)).toString("utf8").trimEnd();
}

async function gitBuffer(repository, args) {
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
      encoding: "buffer",
      maxBuffer: maximumGitOutputBytes,
    });
    return stdout;
  } catch (error) {
    const detail = Buffer.isBuffer(error?.stderr)
      ? error.stderr.toString("utf8")
      : String(error?.stderr ?? error?.message ?? error);
    throw new Error(`git ${args[0]} failed: ${detail.trim().slice(0, 1_000)}`, {
      cause: error,
    });
  }
}

async function rejectObjectSubstitution(repository) {
  const replacements = await git(repository, [
    "for-each-ref",
    "--format=%(refname)",
    "refs/replace/",
  ]);
  if (replacements !== "") {
    throw new Error("CI source publication rejects Git replacement objects");
  }
  const grafts = await git(repository, ["rev-parse", "--git-path", "info/grafts"]);
  const graftPath = resolve(repository, grafts);
  const graft = await lstat(graftPath).catch((error) => {
    if (error?.code === "ENOENT") return undefined;
    throw error;
  });
  if (graft != null) {
    throw new Error("CI source publication rejects Git grafts");
  }
}

async function assertSafeLocalGitConfig(repository) {
  const names = (await Promise.all(["--local", "--worktree"].map(async (scope) =>
    (await git(repository, [
      "config",
      scope,
      "--no-includes",
      "--name-only",
      "--list",
    ])).split("\n").filter(Boolean)
  ))).flat();
  const safe = /^(?:core\.(?:repositoryformatversion|filemode|bare|logallrefupdates|ignorecase|precomposeunicode|autocrlf)|user\.(?:name|email)|extensions\.worktreeconfig|remote\.[^.]+\.(?:url|fetch)|branch\.[^.]+\.(?:remote|merge)|submodule\..+\.url|worktrunk\..+)$/i;
  const unsafe = names.filter((name) => !safe.test(name));
  if (unsafe.length > 0) {
    throw new Error(`CI source publication rejects local Git configuration: ${unsafe.join(", ")}`);
  }
}

function publisherEnvironment(repository) {
  return {
    PATH: process.env.PATH ?? "",
    TMPDIR: process.env.TMPDIR ?? tmpdir(),
    LANG: process.env.LANG ?? "C.UTF-8",
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_NO_REPLACE_OBJECTS: "1",
    GIT_OPTIONAL_LOCKS: "0",
    GIT_TERMINAL_PROMPT: "0",
    GIT_WORK_TREE: repository,
  };
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
    for (let bit = 0; bit < 8; bit++) {
      crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1));
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
}

export function parseOrigin(value) {
  let origin;
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
    origin = url.toString().replace(/\/$/, "");
  } catch (error) {
    throw new Error(
      "NANOCODEX_CI_ORIGIN must use HTTPS (HTTP is allowed only for loopback development)",
      { cause: error },
    );
  }
  return origin;
}

function requiredEnvironment(env, name) {
  const value = env[name]?.trim();
  if (!value) throw new Error(`${name} is required`);
  return value;
}

if (resolve(process.argv[1] ?? "") === scriptPath) {
  await main();
}
