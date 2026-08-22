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
  assertMasterPublicationAuthority,
  buildSourceArtifacts,
  main,
  parseGitTree,
  parseOrigin,
  parsePublicationTarget,
  prepareMasterPublication,
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

test("requires an explicit trusted Cargo vendor bundle SHA", async () => {
  await assert.rejects(
    main({
      env: {
        NANOCODEX_CI_ORIGIN: "http://127.0.0.1:8787",
        NANOCODEX_CI_TOKEN: "source-token",
      },
    }),
    /NANOCODEX_CI_CARGO_VENDOR_SHA256 is required/,
  );
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

test("pull-request targets are canonical and all-or-none", () => {
  assert.deepEqual(parsePublicationTarget({}), {
    type: "master",
    branch: "master",
    ref: "refs/heads/master",
  });
  assert.throws(
    () => parsePublicationTarget({ NANOCODEX_CI_PULL_REQUEST_NUMBER: "7" }),
    /requires NANOCODEX_CI_PULL_REQUEST_NUMBER, NANOCODEX_CI_PULL_REQUEST_HEAD/,
  );
  assert.throws(
    () => parsePublicationTarget({
      NANOCODEX_CI_PULL_REQUEST_NUMBER: "07",
      NANOCODEX_CI_PULL_REQUEST_HEAD: "a".repeat(40),
    }),
    /positive canonical integer/,
  );
  assert.deepEqual(parsePublicationTarget({
    NANOCODEX_CI_PULL_REQUEST_NUMBER: "7",
    NANOCODEX_CI_PULL_REQUEST_HEAD: "a".repeat(40),
  }), {
    type: "pull_request",
    number: 7,
    pullRequestHead: "a".repeat(40),
    branch: "pull/7/merge",
    ref: "refs/pull/7/merge",
    lane: { type: "pull_request", number: 7, pullRequestHead: "a".repeat(40) },
  });
});

test("publishes an exact detached PR merge with CAS and exact reopen proof", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-ci-pr-source-test-"));
  const repository = resolve(directory, "repo");
  const requests = [];
  const number = 7;
  const cargoVendorSha = "4".repeat(64);
  const rustSecRevision = "5".repeat(40);
  const rustSecSha = "6".repeat(64);
  const closeRecord = {
    error: "pull_request_closed",
    number,
    closeId: "123e4567-e89b-42d3-a456-426614174000",
    mergeHead: "7".repeat(40),
    pullRequestHead: "8".repeat(40),
    closedAt: "2026-08-20T00:00:00.000Z",
  };
  let state = closeRecord;
  let cargoLockBlob;
  const server = createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const body = Buffer.concat(chunks);
    requests.push({
      authorization: request.headers.authorization,
      body,
      method: request.method,
      url: request.url,
    });
    if (
      request.method === "GET" &&
      request.url === `/api/ci/source/pull-requests/${number}/state`
    ) {
      response.writeHead(state.error ? 404 : 200, { "content-type": "application/json" });
      response.end(JSON.stringify(state));
      return;
    }
    if (
      request.method === "HEAD" &&
      request.url === `/api/ci/cargo-vendor/${cargoLockBlob}/${cargoVendorSha}/bundle.tar.gz`
    ) {
      response.writeHead(200, {
        "content-length": "1234",
        "x-nanocodex-cargo-lock": cargoLockBlob,
        "x-nanocodex-key": `cargo-vendor/${cargoLockBlob}/${cargoVendorSha}/bundle.tar.gz`,
        "x-nanocodex-sha256": cargoVendorSha,
      }).end();
      return;
    }
    if (
      request.method === "HEAD" &&
      request.url === `/api/ci/rustsec-advisory-db/${rustSecRevision}/bundle.tar.gz`
    ) {
      response.writeHead(200, {
        "content-length": "2345",
        "x-nanocodex-key": `rustsec-advisory-db/${rustSecRevision}/bundle.tar.gz`,
        "x-nanocodex-revision": rustSecRevision,
        "x-nanocodex-sha256": rustSecSha,
      }).end();
      return;
    }
    if (request.method === "PUT" && request.url?.startsWith("/api/ci/source/objects/")) {
      response.writeHead(201).end();
      return;
    }
    if (request.method === "PUT" && request.url === "/api/ci/source/publish") {
      response.writeHead(202).end();
      return;
    }
    response.writeHead(418).end("unexpected endpoint");
  });

  try {
    await git(["init", "-q", "-b", "master", repository], directory);
    await git(["config", "user.name", "Nanocodex Test"], repository);
    await git(["config", "user.email", "test@nanocodex.invalid"], repository);
    await git(["remote", "add", "origin", "https://github.com/gakonst/nanocodex.git"], repository);
    await writeFile(resolve(repository, "Cargo.lock"), "# lock\n");
    await writeFile(resolve(repository, "README.md"), "base\n");
    await git(["add", "."], repository);
    await git(["commit", "-qm", "base"], repository, {
      GIT_AUTHOR_DATE: "2026-08-20T01:00:00Z",
      GIT_COMMITTER_DATE: "2026-08-20T01:00:00Z",
    });
    const baseHead = await git(["rev-parse", "HEAD"], repository);
    await git(["checkout", "-qb", "pull-head"], repository);
    await writeFile(resolve(repository, "README.md"), "pull request\n");
    await git(["commit", "-qam", "pull request"], repository, {
      GIT_AUTHOR_DATE: "2026-08-20T02:00:00Z",
      GIT_COMMITTER_DATE: "2026-08-20T02:00:00Z",
    });
    const pullRequestHead = await git(["rev-parse", "HEAD"], repository);
    await git(["checkout", "-q", "master"], repository);
    await git(["merge", "--no-ff", "--no-edit", "pull-head"], repository, {
      GIT_AUTHOR_DATE: "2026-08-20T03:00:00Z",
      GIT_COMMITTER_DATE: "2026-08-20T03:00:00Z",
    });
    const mergeHead = await git(["rev-parse", "HEAD"], repository);
    cargoLockBlob = await git(["rev-parse", "HEAD:Cargo.lock"], repository);
    await git(["update-ref", "refs/remotes/origin/master", baseHead], repository);
    await git(["update-ref", `refs/pull/${number}/head`, pullRequestHead], repository);
    await git(["update-ref", `refs/pull/${number}/merge`, mergeHead], repository);

    const target = parsePublicationTarget({
      NANOCODEX_CI_PULL_REQUEST_NUMBER: String(number),
      NANOCODEX_CI_PULL_REQUEST_HEAD: pullRequestHead,
    });
    await assert.rejects(
      readRepositorySnapshot(repository, target),
      /requires a detached checkout/,
    );
    await git(["checkout", "-q", "--detach", mergeHead], repository);
    assert.deepEqual(await readRepositorySnapshot(repository, target), {
      head: mergeHead,
      ref: `refs/pull/${number}/merge`,
    });

    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const address = server.address();
    assert.ok(address && typeof address === "object");
    const env = {
      ...process.env,
      NANOCODEX_CI_ORIGIN: `http://127.0.0.1:${address.port}`,
      NANOCODEX_CI_TOKEN: "source-token",
      NANOCODEX_CI_CARGO_VENDOR_SHA256: cargoVendorSha,
      NANOCODEX_REPO: repository,
      NANOCODEX_RUSTSEC_REVISION: rustSecRevision,
      NANOCODEX_CI_PULL_REQUEST_NUMBER: String(number),
      NANOCODEX_CI_PULL_REQUEST_HEAD: pullRequestHead,
    };
    await main({ env, log() {} });
    const firstPublication = JSON.parse(
      requests.findLast(({ method, url }) =>
        method === "PUT" && url === "/api/ci/source/publish"
      ).body.toString("utf8"),
    );
    assert.equal(firstPublication.expectedHead, null);
    assert.deepEqual(firstPublication.reopen, {
      closeId: closeRecord.closeId,
      mergeHead: closeRecord.mergeHead,
      pullRequestHead: closeRecord.pullRequestHead,
    });
    assert.equal(firstPublication.publication.head, mergeHead);
    assert.equal(firstPublication.publication.branch, `pull/${number}/merge`);
    assert.equal(firstPublication.publication.ref, `refs/pull/${number}/merge`);
    assert.deepEqual(firstPublication.publication.lane, {
      type: "pull_request",
      number,
      pullRequestHead,
    });
    assert.ok(requests.every(({ authorization }) => authorization === "Bearer source-token"));

    const priorMerge = "9".repeat(40);
    state = {
      publication: {
        ...firstPublication.publication,
        head: priorMerge,
        lane: {
          type: "pull_request",
          number,
          pullRequestHead: "8".repeat(40),
        },
        archive: { ...firstPublication.publication.archive, key: `sources/${priorMerge}/source.tar.gz` },
        tree: { ...firstPublication.publication.tree, key: `sources/${priorMerge}/tree.json` },
      },
      run: {
        version: 1,
        head: priorMerge,
        workflowId: `ci-${priorMerge}`,
        state: "dispatched",
        publishedAt: "2026-08-20T04:00:00.000Z",
      },
    };
    const beforeReplay = requests.length;
    await main({ env, log() {} });
    const secondPublication = JSON.parse(
      requests.slice(beforeReplay).findLast(({ method, url }) =>
        method === "PUT" && url === "/api/ci/source/publish"
      ).body.toString("utf8"),
    );
    assert.equal(secondPublication.expectedHead, priorMerge);
    assert.equal(secondPublication.reopen, undefined);

    await writeFile(resolve(repository, "UNTRACKED"), "dirty\n");
    await assert.rejects(
      readRepositorySnapshot(repository, target),
      /requires a clean index and worktree/,
    );
  } finally {
    if (server.listening) {
      await new Promise((resolveClose) => server.close(resolveClose));
    }
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
  let advanceAuthorityOnState = false;
  let advanceAuthorityOnObjectUpload = false;
  let authoritativeRemote;
  let movedAuthorityHead;
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
      if (advanceAuthorityOnState) {
        advanceAuthorityOnState = false;
        await git(["update-ref", "refs/heads/master", movedAuthorityHead], authoritativeRemote);
        await git(["reset", "--hard", movedAuthorityHead], repository);
        await git(
          ["update-ref", "refs/remotes/origin/master", movedAuthorityHead],
          repository,
        );
      }
      if (remoteHead == null) {
        response.writeHead(404).end("missing");
      } else {
        response.writeHead(200, { "content-type": "application/json" });
        response.end(JSON.stringify({
          publication: { head: remoteHead, cargoVendor: { sha256: cargoVendorSha } },
        }));
      }
      return;
    }
    if (
      request.method === "HEAD" &&
      request.url === `/api/ci/cargo-vendor/${cargoLockBlob}/${cargoVendorSha}/bundle.tar.gz`
    ) {
      response.writeHead(200, {
        "content-length": String(cargoVendorSize),
        "x-nanocodex-cargo-lock": cargoLockBlob,
        "x-nanocodex-key": `cargo-vendor/${cargoLockBlob}/${cargoVendorSha}/bundle.tar.gz`,
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
      if (advanceAuthorityOnObjectUpload) {
        advanceAuthorityOnObjectUpload = false;
        await git(["update-ref", "refs/heads/master", movedAuthorityHead], authoritativeRemote);
        await git(["reset", "--hard", movedAuthorityHead], repository);
        await git(
          ["update-ref", "refs/remotes/origin/master", movedAuthorityHead],
          repository,
        );
      }
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
    await assert.rejects(
      readRepositorySnapshot(repository),
      /requires a clean tracked and untracked checkout/,
    );
    await rm(resolve(repository, "UNTRACKED-SECRET"));

    await git(["switch", "-qc", "future-authority"], repository);
    await writeFile(resolve(repository, "README.md"), "# future authoritative source\n");
    await git(["commit", "-qam", "future authority fixture"], repository);
    movedAuthorityHead = await git(["rev-parse", "HEAD"], repository);
    await git(["switch", "-q", "master"], repository);
    const authority = await installAuthoritativeGitShim({
      directory,
      repository,
      masterHead: head,
      retainedHeads: [movedAuthorityHead],
    });
    authoritativeRemote = authority.remote;

    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const address = server.address();
    assert.ok(address && typeof address === "object");
    const origin = `http://127.0.0.1:${address.port}`;
    const env = {
      ...process.env,
      NANOCODEX_CI_ORIGIN: origin,
      NANOCODEX_CI_TOKEN: "ci-source-token",
      NANOCODEX_CI_CARGO_VENDOR_SHA256: cargoVendorSha,
      NANOCODEX_REPO: repository,
      NANOCODEX_RUSTSEC_REVISION: rustSecRevision,
      PATH: `${authority.bin}:${process.env.PATH ?? ""}`,
      GIT_REPLACE_REF_BASE: "refs/replace-attacker/",
      GIT_OBJECT_DIRECTORY: resolve(directory, "attacker-objects"),
      GIT_ALTERNATE_OBJECT_DIRECTORIES: resolve(directory, "attacker-alternates"),
      GIT_INDEX_FILE: resolve(directory, "attacker-index"),
      GIT_SHALLOW_FILE: resolve(directory, "attacker-shallow"),
      GIT_CONFIG_GLOBAL: resolve(directory, "attacker-gitconfig"),
    };

    const firstStart = requests.length;
    const first = await execFileAsync(process.execPath, [publisherPath], {
      env,
      encoding: "utf8",
    });
    const firstGitCalls = await readGitShimCalls(authority.log);
    assert.equal(
      firstGitCalls.filter(([command]) => command === "ls-remote").length,
      3,
      "a mutating publication must prove fresh GitHub authority initially, before upload, and before publish",
    );
    assert.deepEqual(firstGitCalls.filter(([command]) => command === "fetch"), [[
      "fetch",
      "--no-tags",
      "--no-write-fetch-head",
      "--recurse-submodules=no",
      "origin",
      "+refs/heads/master:refs/remotes/origin/master",
    ]]);
    assert.ok(firstGitCalls.filter(([command]) => command === "ls-remote").every((call) =>
      call.includes("--exit-code") &&
      call.includes("--refs") &&
      call.includes("https://github.com/gakonst/nanocodex.git") &&
      call.includes("refs/heads/master")
    ));
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
      key: `cargo-vendor/${cargoLockBlob}/${cargoVendorSha}/bundle.tar.gz`,
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
      /local HEAD, fetched origin\/master, and fresh GitHub master to be identical/,
    );
    assert.equal(requests.length, rollbackStart);
    await git(["reset", "--hard", head], repository);

    await writeFile(resolve(repository, "README.md"), "dirty tracked source\n");
    const dirtyStart = requests.length;
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], { env, encoding: "utf8" }),
      /requires a clean tracked and untracked checkout/,
    );
    assert.equal(requests.length, dirtyStart);
    assert.equal(
      requests.some(({ url }) => url === "/api/health" || url?.includes("github")),
      false,
    );
    await git(["checkout", "--", "README.md"], repository);

    remoteHead = oldHead;
    advanceAuthorityOnState = true;
    const staleStart = requests.length;
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], { env, encoding: "utf8" }),
      /local HEAD, fetched origin\/master, and fresh GitHub master to be identical/,
    );
    assert.equal(
      requests.slice(staleStart).some(({ method }) => method === "PUT"),
      false,
      "an authoritative ref move before upload must prevent every remote mutation",
    );
    await git(["reset", "--hard", head], repository);
    await git(["update-ref", "refs/remotes/origin/master", head], repository);
    await git(["update-ref", "refs/heads/master", head], authoritativeRemote);

    advanceAuthorityOnObjectUpload = true;
    const publishRaceStart = requests.length;
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], { env, encoding: "utf8" }),
      /local HEAD, fetched origin\/master, and fresh GitHub master to be identical/,
    );
    const publishRaceRequests = requests.slice(publishRaceStart);
    assert.ok(publishRaceRequests.some(({ method, url }) =>
      method === "PUT" && url?.startsWith("/api/ci/source/objects/")
    ));
    assert.equal(publishRaceRequests.some(({ method, url }) =>
      method === "PUT" && url === "/api/ci/source/publish"
    ), false, "a coherent local and remote ref move after upload must block publication");
  } finally {
    if (server.listening) {
      await new Promise((resolveClose) => server.close(resolveClose));
    }
    await rm(directory, { recursive: true, force: true });
  }
});

test("master source authority rejects isolated, alternate, detached, shallow, grafted, and extra-master repositories", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-ci-source-authority-test-"));
  const repository = resolve(directory, "repo");
  const baseEnv = {
    ...process.env,
    NANOCODEX_CI_ORIGIN: "http://127.0.0.1:9",
    NANOCODEX_CI_TOKEN: "unused-source-token",
    NANOCODEX_CI_CARGO_VENDOR_SHA256: "a".repeat(64),
    NANOCODEX_REPO: repository,
    NANOCODEX_RUSTSEC_REVISION: "b".repeat(40),
  };
  try {
    await git(["init", "-q", "-b", "master", repository], directory);
    await git(["config", "user.name", "Nanocodex Test"], repository);
    await git(["config", "user.email", "test@nanocodex.invalid"], repository);
    await writeFile(resolve(repository, "Cargo.lock"), "# authority fixture\n");
    await git(["add", "Cargo.lock"], repository);
    await git(["commit", "-qm", "authority fixture"], repository);
    const head = await git(["rev-parse", "HEAD"], repository);

    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], { env: baseEnv, encoding: "utf8" }),
      /one canonical HTTPS origin/,
      "an isolated local repository must not acquire publication authority",
    );

    await git(["remote", "add", "origin", "git@github.com:gakonst/nanocodex.git"], repository);
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], { env: baseEnv, encoding: "utf8" }),
      /one canonical HTTPS origin/,
    );
    await git(["remote", "set-url", "origin", "https://github.com/gakonst/nanocodex.git"], repository);
    const authority = await installAuthoritativeGitShim({ directory, repository, masterHead: head });
    const env = { ...baseEnv, PATH: `${authority.bin}:${process.env.PATH ?? ""}` };

    await git(["checkout", "-q", "--detach", head], repository);
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], { env, encoding: "utf8" }),
      /attached refs\/heads\/master checkout|git symbolic-ref failed/,
    );
    await git(["checkout", "-q", "master"], repository);

    const shallowPath = resolve(repository, ".git", "shallow");
    await writeFile(shallowPath, `${head}\n`);
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], { env, encoding: "utf8" }),
      /complete non-bare repository/,
    );
    await rm(shallowPath);

    const graftPath = await git(["rev-parse", "--git-path", "info/grafts"], repository);
    await writeFile(resolve(repository, graftPath), `${head} ${head}\n`);
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], { env, encoding: "utf8" }),
      /rejects Git grafts/,
    );
    await rm(resolve(repository, graftPath));

    await git(["update-ref", "refs/remotes/backup/nested/master", head], repository);
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], { env, encoding: "utf8" }),
      /rejects extra remote-tracking master refs/,
    );
    await git(["update-ref", "-d", "refs/remotes/backup/nested/master"], repository);

    await git(["symbolic-ref", "refs/remotes/origin/master", "refs/heads/master"], repository);
    await assert.rejects(
      assertMasterPublicationAuthorityWithPath(repository, head, authority.bin),
      /rejects extra remote-tracking master refs/,
    );
    await git(["symbolic-ref", "--delete", "refs/remotes/origin/master"], repository);
    await git(["update-ref", "refs/remotes/origin/master", head], repository);

    assert.deepEqual(await prepareMasterPublicationWithPath(repository, authority.bin), {
      head,
      ref: "refs/heads/master",
    });
    assert.deepEqual(await assertMasterPublicationAuthorityWithPath(repository, head, authority.bin), {
      head,
      ref: "refs/heads/master",
    });
  } finally {
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

async function installAuthoritativeGitShim({
  directory,
  repository,
  masterHead,
  retainedHeads = [],
}) {
  const remote = resolve(directory, "authoritative.git");
  const bin = resolve(directory, "authority-bin");
  const log = resolve(directory, "authority-git.jsonl");
  const realGit = (await execFileAsync("which", ["git"], { encoding: "utf8" })).stdout.trim();
  await git(["init", "-q", "--bare", remote], directory);
  await git([
    "push",
    "-q",
    remote,
    `${masterHead}:refs/heads/master`,
    ...retainedHeads.map((head, index) => `${head}:refs/authority/${index}`),
  ], repository);
  const configured = await git(["remote"], repository);
  if (!configured.split("\n").includes("origin")) {
    await git([
      "remote",
      "add",
      "origin",
      "https://github.com/gakonst/nanocodex.git",
    ], repository);
  }
  await git(["config", "branch.master.remote", "origin"], repository);
  await git(["config", "branch.master.merge", "refs/heads/master"], repository);
  await mkdir(bin);
  await writeFile(log, "");
  const shim = resolve(bin, "git");
  await writeFile(shim, `#!/usr/bin/env node
const { spawnSync } = require("node:child_process");
const { appendFileSync } = require("node:fs");
const args = process.argv.slice(2);
let command = 0;
while (args[command] === "-c") command += 2;
appendFileSync(${JSON.stringify(log)}, JSON.stringify(args.slice(command)) + "\\n");
if (args[command] === "fetch") {
  const index = args.indexOf("origin", command + 1);
  if (index >= 0) args[index] = ${JSON.stringify(remote)};
}
if (args[command] === "ls-remote") {
  const index = args.indexOf("https://github.com/gakonst/nanocodex.git", command + 1);
  if (index >= 0) args[index] = ${JSON.stringify(remote)};
}
const result = spawnSync(${JSON.stringify(realGit)}, args, {
  cwd: process.cwd(),
  env: process.env,
  stdio: "inherit",
});
if (result.error) throw result.error;
process.exit(result.status ?? 1);
`);
  await chmod(shim, 0o755);
  return { bin, log, remote };
}

async function readGitShimCalls(path) {
  const contents = await readFile(path, "utf8");
  return contents.trim().split("\n").filter(Boolean).map((line) => JSON.parse(line));
}

async function withAuthorityPath(bin, operation) {
  const prior = process.env.PATH;
  process.env.PATH = `${bin}:${prior ?? ""}`;
  try {
    return await operation();
  } finally {
    if (prior == null) delete process.env.PATH;
    else process.env.PATH = prior;
  }
}

function prepareMasterPublicationWithPath(repository, bin) {
  return withAuthorityPath(bin, () => prepareMasterPublication(repository));
}

function assertMasterPublicationAuthorityWithPath(repository, head, bin) {
  return withAuthorityPath(bin, () => assertMasterPublicationAuthority(repository, head));
}
