import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  symlink,
  unlink,
  utimes,
  writeFile,
} from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { test } from "node:test";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";

import {
  buildRustSecBundle,
  collectRustSecArchiveEntries,
  maximumBundleBytes,
  readRustSecSnapshot,
  rustSecArchiveRoot,
  rustSecBundleKey,
  validateRustSecShape,
} from "./publish-ci-rustsec.mjs";

const execFileAsync = promisify(execFile);
const publisherPath = fileURLToPath(new URL("./publish-ci-rustsec.mjs", import.meta.url));
const commitTimestamp = 1_700_000_000;
const commitEnvironment = {
  GIT_AUTHOR_DATE: `${commitTimestamp} +0000`,
  GIT_COMMITTER_DATE: `${commitTimestamp} +0000`,
};

test("builds deterministic normalized bundle with a functional Git checkout", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-rustsec-bundle-"));
  const repository = resolve(directory, "advisory-db");
  try {
    await createRustSecFixture(repository);
    await git(["config", "credential.helper", "machine-local-secret"], repository);
    const snapshot = await readRustSecSnapshot(repository);
    assert.match(snapshot.revision, /^[a-f0-9]{40}$/);
    assert.equal(snapshot.commitTimestamp, commitTimestamp);
    assert.equal(validateRustSecShape(snapshot.trackedFiles).advisoryCount, 1);

    const first = await buildRustSecBundle({ repository, snapshot });
    await utimes(resolve(repository, "README.md"), new Date(1_000), new Date(2_000));
    await utimes(resolve(repository, "run.sh"), new Date(3_000), new Date(4_000));
    const second = await buildRustSecBundle({ repository, snapshot });
    assert.deepEqual(first, second);
    assert.ok(first.byteLength > 0 && first.byteLength <= maximumBundleBytes);
    assert.equal(first.readUInt32LE(4), commitTimestamp, "gzip mtime is normalized");

    const archivePath = resolve(directory, "bundle.tar.gz");
    await writeFile(archivePath, first);
    const { stdout: listingText } = await execFileAsync("tar", ["-tzf", archivePath], {
      encoding: "utf8",
    });
    const listing = listingText.trimEnd().split("\n");
    assert.equal(listing[0], `${rustSecArchiveRoot}/`);
    assert.ok(listing.includes(`${rustSecArchiveRoot}/.git/`));
    assert.ok(listing.includes(`${rustSecArchiveRoot}/.git/HEAD`));
    assert.ok(listing.includes(`${rustSecArchiveRoot}/support.toml`));
    assert.ok(listing.includes(
      `${rustSecArchiveRoot}/crates/example/RUSTSEC-2026-0001.md`,
    ));
    assert.deepEqual(listing, [...listing].sort(), "archive entries have byte-stable order");

    const headers = parseTarHeaders(gunzipSync(first));
    assert.ok(headers.length > 10);
    assert.ok(headers.every(({ uid, gid, mtime }) =>
      uid === 0 && gid === 0 && mtime === commitTimestamp
    ));
    assert.equal(headerFor(headers, `${rustSecArchiveRoot}/README.md`).mode, 0o644);
    assert.equal(headerFor(headers, `${rustSecArchiveRoot}/run.sh`).mode, 0o755);
    assert.equal(
      headerFor(headers, `${rustSecArchiveRoot}/crates/example/current.md`).mode,
      0o777,
    );
    assert.equal(headerFor(headers, `${rustSecArchiveRoot}/.git/HEAD`).mode, 0o644);
    const { stdout: archivedGitConfig } = await execFileAsync(
      "tar",
      ["-xOzf", archivePath, `${rustSecArchiveRoot}/.git/config`],
      { encoding: "utf8" },
    );
    assert.doesNotMatch(archivedGitConfig, /machine-local-secret/);
    assert.doesNotMatch(archivedGitConfig, new RegExp(repository.replaceAll("/", "\\/")));

    const extracted = resolve(directory, "extracted");
    await mkdir(extracted);
    await execFileAsync("tar", ["-xzf", archivePath, "-C", extracted]);
    const extractedRepository = resolve(extracted, rustSecArchiveRoot);
    assert.equal(await git(["rev-parse", "--verify", "HEAD^{commit}"], extractedRepository),
      snapshot.revision);
    assert.equal(await git(["status", "--porcelain=v1", "--untracked-files=all"],
      extractedRepository), "");

    await assert.rejects(
      collectRustSecArchiveEntries(repository, snapshot.trackedFiles, { rawByteLimit: 1 }),
      /raw content exceeds/,
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("requires exact clean RustSec shape and rejects unsafe checkout content", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-rustsec-validation-"));
  const repository = resolve(directory, "valid");
  const invalid = resolve(directory, "invalid");
  try {
    await createRustSecFixture(repository);
    await writeFile(resolve(repository, "scratch.txt"), "untracked\n");
    await assert.rejects(
      readRustSecSnapshot(repository),
      /clean tracked and untracked worktree/,
    );
    await unlink(resolve(repository, "scratch.txt"));

    await writeFile(resolve(repository, "support.toml"), "dirty tracked file\n");
    await assert.rejects(
      readRustSecSnapshot(repository),
      /clean tracked and untracked worktree/,
    );
    await writeFile(resolve(repository, "support.toml"), "[database]\nname = \"RustSec\"\n");
    assert.equal((await readRustSecSnapshot(repository)).commitTimestamp, commitTimestamp);

    await initializeRepository(invalid);
    await writeFile(resolve(invalid, "support.toml"), "[database]\n");
    await git(["add", "."], invalid);
    await git(["commit", "-qm", "invalid shape"], invalid, commitEnvironment);
    await assert.rejects(
      readRustSecSnapshot(invalid),
      /at least one crates\/\*\/RUSTSEC-YYYY-NNNN\.md advisory/,
    );
    assert.throws(() => rustSecBundleKey("A".repeat(40)), /invalid RustSec revision/);

    await symlink("../../../../outside", resolve(repository, "crates/example/escape.md"));
    await git(["add", "crates/example/escape.md"], repository);
    await git(["commit", "-qm", "unsafe symlink"], repository, {
      GIT_AUTHOR_DATE: `${commitTimestamp + 1} +0000`,
      GIT_COMMITTER_DATE: `${commitTimestamp + 1} +0000`,
    });
    const unsafeSnapshot = await readRustSecSnapshot(repository);
    await assert.rejects(
      buildRustSecBundle({ repository, snapshot: unsafeSnapshot }),
      /unsafe RustSec symlink/,
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("authenticates immutable PUT and reuses exact HEAD metadata without rebuilding", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-rustsec-cli-"));
  const repository = resolve(directory, "advisory-db");
  const requests = [];
  let published = null;
  let headRevision = null;
  const server = createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const body = Buffer.concat(chunks);
    requests.push({
      authorization: request.headers.authorization,
      contentType: request.headers["content-type"],
      method: request.method,
      sha256: request.headers["x-nanocodex-sha256"],
      url: request.url,
      body,
    });
    if (request.method === "HEAD" && request.url?.startsWith(
      "/api/ci/rustsec-advisory-db/",
    )) {
      if (!published) return response.writeHead(404).end();
      response.writeHead(200, {
        "content-length": String(published.size),
        "content-type": "application/gzip",
        "x-nanocodex-key": published.key,
        "x-nanocodex-revision": headRevision ?? published.revision,
        "x-nanocodex-sha256": published.sha256,
      }).end();
      return;
    }
    if (request.method === "PUT" && request.url?.startsWith(
      "/api/ci/rustsec-advisory-db/",
    )) {
      const revision = request.url.split("/")[4];
      published = {
        key: rustSecBundleKey(revision),
        revision,
        size: body.byteLength,
        sha256: sha256(body),
        uploaded: true,
      };
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify(published));
      return;
    }
    response.writeHead(418).end();
  });
  try {
    await createRustSecFixture(repository);
    await git(["remote", "add", "origin", "https://network-must-not-be-used.invalid/db"],
      repository);
    const revision = await git(["rev-parse", "HEAD"], repository);
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const address = server.address();
    assert.ok(address && typeof address === "object");
    const env = {
      ...process.env,
      NANOCODEX_CI_ORIGIN: `http://127.0.0.1:${address.port}`,
      NANOCODEX_CI_TOKEN: "rustsec-token",
      NANOCODEX_RUSTSEC_REPO: repository,
    };

    const first = await execFileAsync(process.execPath, [publisherPath], {
      env,
      encoding: "utf8",
    });
    assert.match(first.stdout, /Published CI RustSec advisory database/);
    assert.ok(published);
    assert.equal(published.revision, revision);
    assert.equal(published.key, rustSecBundleKey(revision));
    assert.ok(published.size > 0 && published.size <= maximumBundleBytes);
    assert.deepEqual(requests.map(({ method, url }) => ({ method, url })), [
      {
        method: "HEAD",
        url: `/api/ci/rustsec-advisory-db/${revision}/bundle.tar.gz`,
      },
      {
        method: "PUT",
        url: `/api/ci/rustsec-advisory-db/${revision}/bundle.tar.gz`,
      },
    ]);
    const upload = requests.at(-1);
    assert.equal(upload.authorization, "Bearer rustsec-token");
    assert.equal(upload.contentType, "application/gzip");
    assert.equal(upload.sha256, sha256(upload.body));
    assert.ok(requests.every(({ authorization }) => authorization === "Bearer rustsec-token"));

    await symlink("/outside", resolve(repository, ".git/unsafe-link"));
    const reuseStart = requests.length;
    const second = await execFileAsync(process.execPath, [publisherPath], {
      env,
      encoding: "utf8",
    });
    assert.match(second.stdout, /CI RustSec advisory database is current/);
    assert.deepEqual(
      requests.slice(reuseStart).map(({ method, url }) => ({ method, url })),
      [{
        method: "HEAD",
        url: `/api/ci/rustsec-advisory-db/${revision}/bundle.tar.gz`,
      }],
      "an exact immutable object must be reused before archive traversal",
    );

    headRevision = "b".repeat(40);
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], { env, encoding: "utf8" }),
      /returned invalid metadata/,
    );
  } finally {
    if (server.listening) await new Promise((done) => server.close(done));
    await rm(directory, { recursive: true, force: true });
  }
});

async function createRustSecFixture(repository) {
  await initializeRepository(repository);
  await mkdir(resolve(repository, "crates/example"), { recursive: true });
  await writeFile(resolve(repository, "support.toml"), "[database]\nname = \"RustSec\"\n");
  await writeFile(resolve(repository, "README.md"), "# RustSec fixture\n");
  await writeFile(
    resolve(repository, "crates/example/RUSTSEC-2026-0001.md"),
    "```toml\n[advisory]\nid = \"RUSTSEC-2026-0001\"\n```\n",
  );
  await symlink("RUSTSEC-2026-0001.md", resolve(repository, "crates/example/current.md"));
  await writeFile(resolve(repository, "run.sh"), "#!/bin/sh\nexit 0\n");
  await chmod(resolve(repository, "run.sh"), 0o755);
  await git(["add", "."], repository);
  await git(["commit", "-qm", "fixture"], repository, commitEnvironment);
}

async function initializeRepository(repository) {
  await execFileAsync("git", ["init", "-q", "-b", "master", repository]);
  await git(["config", "user.name", "Nanocodex Test"], repository);
  await git(["config", "user.email", "test@nanocodex.invalid"], repository);
  await git(["config", "core.filemode", "true"], repository);
}

async function git(args, cwd, extraEnvironment = {}) {
  const { stdout } = await execFileAsync("git", args, {
    cwd,
    env: { ...process.env, ...extraEnvironment },
    encoding: "utf8",
  });
  return stdout.trimEnd();
}

function sha256(body) {
  return createHash("sha256").update(body).digest("hex");
}

function parseTarHeaders(tar) {
  const entries = [];
  for (let offset = 0; offset + 512 <= tar.byteLength;) {
    const header = tar.subarray(offset, offset + 512);
    if (header.every((byte) => byte === 0)) break;
    const name = textField(header, 0, 100);
    const prefix = textField(header, 345, 155);
    const path = prefix ? `${prefix}/${name}` : name;
    const size = octalField(header, 124, 12);
    entries.push({
      path,
      mode: octalField(header, 100, 8),
      uid: octalField(header, 108, 8),
      gid: octalField(header, 116, 8),
      mtime: octalField(header, 136, 12),
      type: String.fromCharCode(header[156]),
    });
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  return entries;
}

function headerFor(headers, path) {
  const header = headers.find((candidate) => candidate.path === path);
  assert.ok(header, `missing tar header for ${path}`);
  return header;
}

function textField(body, offset, length) {
  const end = body.indexOf(0, offset);
  return body.subarray(offset, end < 0 || end > offset + length ? offset + length : end)
    .toString("utf8");
}

function octalField(body, offset, length) {
  const value = textField(body, offset, length).trim();
  return value === "" ? 0 : Number.parseInt(value, 8);
}
