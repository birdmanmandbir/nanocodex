import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { test } from "node:test";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";

import {
  buildSourceArtifacts,
  parseGitTree,
  parseOrigin,
  readRepositorySnapshot,
} from "./publish-ci-source.mjs";

const execFileAsync = promisify(execFile);
const publisherPath = fileURLToPath(new URL("./publish-ci-source.mjs", import.meta.url));

test("requires HTTPS except for loopback development", () => {
  assert.equal(parseOrigin("https://ci.example.test/"), "https://ci.example.test");
  assert.equal(parseOrigin("http://127.0.0.1:8787/"), "http://127.0.0.1:8787");
  assert.equal(parseOrigin("http://ci.localhost:8787/"), "http://ci.localhost:8787");
  assert.throws(() => parseOrigin("http://ci.example.test"), /must use HTTPS/);
});

test("rejects Git replacement objects before naming an immutable source", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-ci-replace-test-"));
  const repository = resolve(directory, "repo");
  try {
    await git(["init", "-q", "-b", "master", repository], directory);
    await git(["config", "user.name", "Nanocodex Test"], repository);
    await git(["config", "user.email", "test@nanocodex.invalid"], repository);
    await writeFile(resolve(repository, "Cargo.lock"), "original\n");
    await git(["add", "Cargo.lock"], repository);
    await git(["commit", "-qm", "original"], repository);
    const original = await git(["rev-parse", "HEAD"], repository);
    await writeFile(resolve(repository, "Cargo.lock"), "replacement\n");
    await git(["commit", "-qam", "replacement"], repository);
    const replacement = await git(["rev-parse", "HEAD"], repository);
    await git(["reset", "--hard", original], repository);
    await git(["replace", original, replacement], repository);
    await assert.rejects(
      readRepositorySnapshot(repository),
      /rejects Git replacement objects/,
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("publishes the exact committed master source with authenticated CAS", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-ci-source-test-"));
  const repository = resolve(directory, "repo");
  const artifactDirectoryOne = resolve(directory, "artifacts-one");
  const artifactDirectoryTwo = resolve(directory, "artifacts-two");
  const requests = [];
  let remoteHead = null;
  let cargoLockBlob;
  const cargoVendorSha = "9".repeat(64);
  const cargoVendorSize = 4_000_000;
  const rustSecRevision = "d".repeat(40);
  const rustSecSha = "8".repeat(64);
  const rustSecSize = 1_368_315;
  const server = createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const body = Buffer.concat(chunks);
    requests.push({
      authorization: request.headers.authorization,
      body,
      contentLength: request.headers["content-length"],
      contentType: request.headers["content-type"],
      method: request.method,
      sha256: request.headers["x-nanocodex-sha256"],
      url: request.url,
    });
    if (request.method === "GET" && request.url === "/api/ci/source/state") {
      if (remoteHead == null) {
        response.writeHead(404).end("missing");
      } else {
        response.writeHead(200, { "content-type": "application/json" });
        response.end(JSON.stringify({ publication: { head: remoteHead } }));
      }
      return;
    }
    if (
      request.method === "HEAD" &&
      request.url === `/api/ci/cargo-vendor/${cargoLockBlob}/bundle.tar.gz`
    ) {
      response.writeHead(200, {
        "content-length": String(cargoVendorSize),
        "x-nanocodex-cargo-lock": cargoLockBlob,
        "x-nanocodex-key": `cargo-vendor/${cargoLockBlob}/bundle.tar.gz`,
        "x-nanocodex-sha256": cargoVendorSha,
      }).end();
      return;
    }
    if (
      request.method === "HEAD" &&
      request.url === `/api/ci/rustsec-advisory-db/${rustSecRevision}/bundle.tar.gz`
    ) {
      response.writeHead(200, {
        "content-length": String(rustSecSize),
        "x-nanocodex-key": `rustsec-advisory-db/${rustSecRevision}/bundle.tar.gz`,
        "x-nanocodex-revision": rustSecRevision,
        "x-nanocodex-sha256": rustSecSha,
      }).end();
      return;
    }
    if (
      request.method === "PUT" &&
      request.url?.startsWith("/api/ci/source/objects/")
    ) {
      response.writeHead(201).end();
      return;
    }
    if (request.method === "PUT" && request.url === "/api/ci/source/publish") {
      response.writeHead(200).end();
      return;
    }
    response.writeHead(418).end("unexpected endpoint");
  });

  try {
    await git(["init", "-q", "-b", "master", repository], directory);
    await git(["config", "user.name", "Nanocodex Test"], repository);
    await git(["config", "user.email", "test@nanocodex.invalid"], repository);
    await writeFile(resolve(repository, "Cargo.lock"), "# committed lock fixture\n");
    await writeFile(resolve(repository, "README.md"), "# base source\n");
    await git(["add", "Cargo.lock", "README.md"], repository);
    await git(["commit", "-qm", "base fixture"], repository, {
      GIT_AUTHOR_DATE: "2026-01-01T03:04:05Z",
      GIT_COMMITTER_DATE: "2026-01-01T03:04:05Z",
    });
    const oldHead = await git(["rev-parse", "HEAD"], repository);
    await mkdir(resolve(repository, "src"));
    await writeFile(resolve(repository, "README.md"), "# committed source\n");
    await writeFile(resolve(repository, "src", "main.rs"), "fn main() {}\n");
    await writeFile(resolve(repository, "run.sh"), "#!/bin/sh\nexit 0\n");
    await chmod(resolve(repository, "run.sh"), 0o755);
    await git(["add", "."], repository);
    await git(["commit", "-qm", "source fixture"], repository, {
      GIT_AUTHOR_DATE: "2026-01-02T03:04:05Z",
      GIT_COMMITTER_DATE: "2026-01-02T03:04:05Z",
    });
    const head = await git(["rev-parse", "HEAD"], repository);
    cargoLockBlob = await git(["rev-parse", "HEAD:Cargo.lock"], repository);
    const cargoLockSha = cargoLockBlob;
    const readmeSha = await git(["rev-parse", "HEAD:README.md"], repository);
    const mainSha = await git(["rev-parse", "HEAD:src/main.rs"], repository);
    const runSha = await git(["rev-parse", "HEAD:run.sh"], repository);
    await writeFile(resolve(repository, "UNTRACKED-SECRET"), "must not publish\n");
    await git(["config", "core.attributesFile", resolve(directory, "external-attributes")], repository);
    await assert.rejects(
      readRepositorySnapshot(repository),
      /rejects local Git configuration: core.attributesfile/i,
    );
    await git(["config", "--unset", "core.attributesFile"], repository);
    await git(["config", "tar.tar.gz.command", "false"], repository);

    await Promise.all([mkdir(artifactDirectoryOne), mkdir(artifactDirectoryTwo)]);
    const [firstArtifacts, secondArtifacts] = await Promise.all([
      buildSourceArtifacts({ repository, temporaryDirectory: artifactDirectoryOne, head }),
      buildSourceArtifacts({ repository, temporaryDirectory: artifactDirectoryTwo, head }),
    ]);
    assert.deepEqual(
      await readFile(firstArtifacts.archive.path),
      await readFile(secondArtifacts.archive.path),
      "git archive should be byte-stable for the same commit",
    );
    assert.deepEqual(firstArtifacts.treeDocument, {
      version: 1,
      head,
      archive: {
        size: firstArtifacts.archive.size,
        sha256: firstArtifacts.archive.sha256,
      },
      files: [
        { path: "Cargo.lock", sha: cargoLockSha, mode: "100644" },
        { path: "README.md", sha: readmeSha, mode: "100644" },
        { path: "run.sh", sha: runSha, mode: "100755" },
        { path: "src/main.rs", sha: mainSha, mode: "100644" },
      ],
    });
    await git(["config", "--unset", "tar.tar.gz.command"], repository);

    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const address = server.address();
    assert.ok(address && typeof address === "object");
    const origin = `http://127.0.0.1:${address.port}`;
    const env = {
      ...process.env,
      NANOCODEX_CI_ORIGIN: origin,
      NANOCODEX_CI_TOKEN: "ci-source-token",
      NANOCODEX_REPO: repository,
      NANOCODEX_RUSTSEC_REVISION: rustSecRevision,
    };

    const firstStart = requests.length;
    const first = await execFileAsync(process.execPath, [publisherPath], {
      env,
      encoding: "utf8",
    });
    assert.match(first.stdout, new RegExp(`Published CI source ${head.slice(0, 7)}`));
    const firstRequests = requests.slice(firstStart);
    assert.deepEqual(
      firstRequests[0] && { method: firstRequests[0].method, url: firstRequests[0].url },
      { method: "GET", url: "/api/ci/source/state" },
    );
    assert.deepEqual(
      firstRequests.at(-1) && {
        method: firstRequests.at(-1).method,
        url: firstRequests.at(-1).url,
      },
      { method: "PUT", url: "/api/ci/source/publish" },
    );
    assert.ok(firstRequests.every(
      ({ authorization }) => authorization === "Bearer ci-source-token",
    ));
    const firstUploads = firstRequests.filter(({ url }) => url.includes("/objects/"));
    assert.deepEqual(
      firstUploads.map(({ url }) => url).sort(),
      [
        `/api/ci/source/objects/${head}/source.tar.gz`,
        `/api/ci/source/objects/${head}/tree.json`,
      ],
    );
    const archiveUpload = firstUploads.find(({ url }) => url.endsWith("source.tar.gz"));
    const treeUpload = firstUploads.find(({ url }) => url.endsWith("tree.json"));
    assert.ok(archiveUpload && treeUpload);
    for (const upload of firstUploads) {
      assert.equal(upload.contentLength, String(upload.body.byteLength));
      assert.equal(upload.sha256, sha256(upload.body));
    }
    const publication = JSON.parse(firstRequests.at(-1).body.toString("utf8"));
    assert.equal(publication.expectedHead, null);
    assert.equal(publication.publication.version, 1);
    assert.equal(publication.publication.head, head);
    assert.equal(publication.publication.branch, "master");
    assert.equal(publication.publication.ref, "refs/heads/master");
    assert.equal(publication.publication.cargoLockBlob, cargoLockBlob);
    assert.deepEqual(publication.publication.cargoVendor, {
      key: `cargo-vendor/${cargoLockBlob}/bundle.tar.gz`,
      size: cargoVendorSize,
      sha256: cargoVendorSha,
    });
    assert.equal(publication.publication.rustSecRevision, rustSecRevision);
    assert.deepEqual(publication.publication.rustSec, {
      key: `rustsec-advisory-db/${rustSecRevision}/bundle.tar.gz`,
      size: rustSecSize,
      sha256: rustSecSha,
    });
    assert.deepEqual(publication.publication.archive, {
      key: `sources/${head}/source.tar.gz`,
      size: archiveUpload.body.byteLength,
      sha256: sha256(archiveUpload.body),
    });
    assert.deepEqual(publication.publication.tree, {
      key: `sources/${head}/tree.json`,
      size: treeUpload.body.byteLength,
      sha256: sha256(treeUpload.body),
    });
    assert.equal(Number.isNaN(Date.parse(publication.publication.publishedAt)), false);
    assert.deepEqual(JSON.parse(treeUpload.body.toString("utf8")), {
      version: 1,
      head,
      archive: {
        size: archiveUpload.body.byteLength,
        sha256: sha256(archiveUpload.body),
      },
      files: [
        { path: "Cargo.lock", sha: cargoLockSha, mode: "100644" },
        { path: "README.md", sha: readmeSha, mode: "100644" },
        { path: "run.sh", sha: runSha, mode: "100755" },
        { path: "src/main.rs", sha: mainSha, mode: "100644" },
      ],
    });

    const archivePath = resolve(directory, "uploaded.tar.gz");
    await writeFile(archivePath, archiveUpload.body);
    const { stdout: archiveListing } = await execFileAsync(
      "tar",
      ["-tzf", archivePath],
      { encoding: "utf8" },
    );
    assert.deepEqual(archiveListing.trimEnd().split("\n"), [
      `nanocodex-${head}/`,
      `nanocodex-${head}/Cargo.lock`,
      `nanocodex-${head}/README.md`,
      `nanocodex-${head}/run.sh`,
      `nanocodex-${head}/src/`,
      `nanocodex-${head}/src/main.rs`,
    ]);
    assert.doesNotMatch(archiveListing, /UNTRACKED-SECRET/);
    const { stdout: archivedReadme } = await execFileAsync(
      "tar",
      ["-xOzf", archivePath, `nanocodex-${head}/README.md`],
      { encoding: "utf8" },
    );
    assert.equal(archivedReadme, "# committed source\n");

    remoteHead = oldHead;
    const secondStart = requests.length;
    await execFileAsync(process.execPath, [publisherPath], { env, encoding: "utf8" });
    const secondRequests = requests.slice(secondStart);
    assert.equal(
      JSON.parse(secondRequests.at(-1).body.toString("utf8")).expectedHead,
      oldHead,
    );
    assert.deepEqual(
      secondRequests.find(({ url }) => url?.endsWith("source.tar.gz"))?.body,
      archiveUpload.body,
    );

    remoteHead = head;
    const idempotentStart = requests.length;
    const current = await execFileAsync(process.execPath, [publisherPath], {
      env,
      encoding: "utf8",
    });
    assert.match(
      current.stdout,
      new RegExp(`CI source is current \\(${head.slice(0, 7)}\\)`),
    );
    assert.deepEqual(
      requests.slice(idempotentStart).map(({ method, url }) => ({ method, url })),
      [{ method: "GET", url: "/api/ci/source/state" }],
    );
    assert.equal(
      requests.filter(({ method, url }) =>
        method === "PUT" && url?.startsWith("/api/ci/cargo-vendor/")
      ).length,
      0,
      "source publication must reuse the published Cargo bundle",
    );

    await git(["reset", "--hard", oldHead], repository);
    const rollbackStart = requests.length;
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], { env, encoding: "utf8" }),
      /must advance published head/,
    );
    assert.deepEqual(
      requests.slice(rollbackStart).map(({ method, url }) => ({ method, url })),
      [{ method: "GET", url: "/api/ci/source/state" }],
    );
    await git(["reset", "--hard", head], repository);

    await writeFile(resolve(repository, "README.md"), "dirty tracked source\n");
    const dirtyStart = requests.length;
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], { env, encoding: "utf8" }),
      /requires a clean index and tracked worktree/,
    );
    assert.equal(requests.length, dirtyStart);
    assert.equal(
      requests.some(({ url }) => url === "/api/health" || url?.includes("github")),
      false,
    );
  } finally {
    if (server.listening) {
      await new Promise((resolveClose) => server.close(resolveClose));
    }
    await rm(directory, { recursive: true, force: true });
  }
});

test("rejects gitlinks from the committed source tree", () => {
  assert.throws(
    () => parseGitTree(Buffer.from(
      `160000 commit ${"a".repeat(40)}\tvendor/dependency\0`,
      "utf8",
    )),
    /do not support gitlinks: vendor\/dependency/,
  );
});

function sha256(body) {
  return createHash("sha256").update(body).digest("hex");
}

async function git(args, cwd, extraEnv = {}) {
  const { stdout } = await execFileAsync("git", args, {
    cwd,
    env: { ...process.env, ...extraEnv },
    encoding: "utf8",
  });
  return stdout.trimEnd();
}
