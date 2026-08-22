import assert from "node:assert/strict";
import { test } from "node:test";
import { execFile } from "node:child_process";
import { once } from "node:events";
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";

import {
  assertMasterPublicationAuthority,
  buildCommitPatchParts,
  buildGitArtifacts,
  buildUploadPlan,
  buildRepositoryPackParts,
  isRetriableUploadError,
  isRetriableUploadStatus,
  prepareMasterPublication,
  readRemoteState,
} from "./publish-repository.mjs";

const execFileAsync = promisify(execFile);
const publisherPath = fileURLToPath(new URL("./publish-repository.mjs", import.meta.url));

test("the publisher CLI initializes its module before building a generation", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-publisher-cli-test-"));
  const repository = resolve(directory, "repo");
  const requests = [];
  let deploymentSha;
  let stalledUpload = false;
  let advanceAuthorityOnObjectUpload = false;
  let authoritativeRemote;
  let movedAuthorityHead;
  const server = createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const rawBody = Buffer.concat(chunks);
    const body = rawBody.toString("utf8");
    requests.push({
      authorization: request.headers.authorization,
      body,
      method: request.method,
      url: request.url,
    });
    if (request.method === "GET" && request.url === "/api/health") {
      response.setHeader("content-type", "application/json");
      response.end(JSON.stringify({ deployment_sha: deploymentSha }));
      return;
    }
    if (request.method === "GET" && request.url === "/api/git/state") {
      response.writeHead(404).end();
      return;
    }
    if (request.method === "PUT" && request.url?.startsWith("/api/git/objects/")) {
      if (advanceAuthorityOnObjectUpload) {
        advanceAuthorityOnObjectUpload = false;
        await git(["update-ref", "refs/heads/master", movedAuthorityHead], authoritativeRemote);
        await git(["reset", "--hard", movedAuthorityHead], repository);
        await git(
          ["update-ref", "refs/remotes/origin/master", movedAuthorityHead],
          repository,
        );
      }
      if (!stalledUpload) {
        stalledUpload = true;
        return;
      }
      response.writeHead(201, { "content-type": "application/json" });
      response.end(JSON.stringify({ size: rawBody.byteLength }));
      return;
    }
    if (request.method === "PUT" && request.url === "/api/git/publish") {
      response.writeHead(200).end();
      return;
    }
    response.writeHead(404).end();
  });
  try {
    await git(["init", "-q", "-b", "master", repository], directory);
    await git(["config", "user.name", "Nanocodex Test"], repository);
    await git(["config", "user.email", "test@nanocodex.invalid"], repository);
    await writeFile(resolve(repository, "README.md"), "# publisher fixture\n");
    await git(["add", "README.md"], repository);
    await git(["commit", "-qm", "initial fixture"], repository);
    await writeFile(resolve(repository, "README.md"), "# publisher fixture\n\nsecond commit\n");
    await git(["add", "README.md"], repository);
    await git(["commit", "-qm", "second fixture"], repository);
    for (let index = 2; index < 33; index++) {
      await writeFile(
        resolve(repository, "README.md"),
        `# publisher fixture\n\ncommit ${index}\n`,
      );
      await git(["add", "README.md"], repository);
      const subject = index === 2
        ? "perf(web): page commit patches"
        : index === 3
        ? "fix(worker): gate publications"
        : `chore: fixture ${index}`;
      await git(["commit", "-qm", subject], repository);
    }
    const head = await git(["rev-parse", "HEAD"], repository);
    await git(["branch", "private-preview"], repository);
    await git(["tag", "v0-test"], repository);
    await git(["switch", "-qc", "future-authority"], repository);
    await writeFile(resolve(repository, "README.md"), "# future authority fixture\n");
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
    deploymentSha = head;

    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const address = server.address();
    assert.ok(address && typeof address === "object");
    const { stdout } = await execFileAsync(process.execPath, [publisherPath], {
      env: {
        ...process.env,
        NANOCODEX_GIT_ORIGIN: `http://127.0.0.1:${address.port}`,
        NANOCODEX_GIT_TOKEN: "publisher-test-token",
        NANOCODEX_REPO: repository,
        NANOCODEX_GIT_UPLOAD_TIMEOUT_MS: "100",
        PATH: `${authority.bin}:${process.env.PATH ?? ""}`,
        GIT_REPLACE_REF_BASE: "refs/replace-attacker/",
        GIT_OBJECT_DIRECTORY: resolve(directory, "attacker-objects"),
        GIT_ALTERNATE_OBJECT_DIRECTORIES: resolve(directory, "attacker-alternates"),
        GIT_INDEX_FILE: resolve(directory, "attacker-index"),
        GIT_SHALLOW_FILE: resolve(directory, "attacker-shallow"),
        GIT_CONFIG_GLOBAL: resolve(directory, "attacker-gitconfig"),
      },
      encoding: "utf8",
      maxBuffer: 16 * 1024 * 1024,
    });

    assert.match(stdout, new RegExp(`Published gakonst/nanocodex ${head.slice(0, 7)}`));
    const gitCalls = await readGitShimCalls(authority.log);
    assert.equal(
      gitCalls.filter(({ args: [command] }) => command === "ls-remote").length,
      3,
      "a mutating repository publication must recheck GitHub before upload and publish",
    );
    assert.deepEqual(
      gitCalls.filter(({ args: [command] }) => command === "fetch").map(({ args }) => args),
      [[
        "fetch",
        "--no-tags",
        "--no-write-fetch-head",
        "--recurse-submodules=no",
        "origin",
        "+refs/heads/master:refs/remotes/origin/master",
      ]],
    );
    assert.ok(gitCalls.filter(({ args: [command] }) => command === "ls-remote").every(({ args }) =>
      args.includes("--exit-code") &&
      args.includes("--refs") &&
      args.includes("https://github.com/gakonst/nanocodex.git") &&
      args.includes("refs/heads/master")
    ));
    for (const { gitEnvironment } of gitCalls) {
      assert.equal(gitEnvironment.GIT_CONFIG_GLOBAL, "/dev/null");
      assert.equal(gitEnvironment.GIT_NO_REPLACE_OBJECTS, "1");
      assert.equal(gitEnvironment.GIT_OBJECT_DIRECTORY, undefined);
      assert.equal(gitEnvironment.GIT_ALTERNATE_OBJECT_DIRECTORIES, undefined);
      assert.equal(gitEnvironment.GIT_INDEX_FILE, undefined);
      assert.equal(gitEnvironment.GIT_SHALLOW_FILE, undefined);
      assert.equal(gitEnvironment.GIT_REPLACE_REF_BASE, undefined);
    }
    assert.equal(stalledUpload, true);
    assert.ok(requests.length > 3);
    assert.equal(requests[0]?.url, "/api/health");
    assert.equal(requests[0]?.authorization, undefined);
    assert.ok(requests.slice(1).every(
      ({ authorization }) => authorization === "Bearer publisher-test-token"
    ));
    const publicationRequest = requests.find(({ url }) => url === "/api/git/publish");
    assert.ok(publicationRequest);
    const publication = JSON.parse(publicationRequest.body);
    assert.equal(publication.expectedHead, null);
    assert.equal(publication.publication.head, head);
    assert.equal(publication.publication.branch, "master");
    assert.deepEqual(publication.publication.refs, [
      { name: "refs/heads/master", oid: head },
    ]);
    assert.equal(publication.publication.commitPatchParts.length, 2);
    assert.equal(
      publication.publication.commitPatchParts.reduce(
        (total, part) => total + part.size,
        0,
      ),
      publication.publication.commitPatchSize,
    );
    assert.ok(publication.publication.commitPatchParts.every(({ key, size }) =>
      requests.some(({ url }) => url === `/api/git/objects/${key}`) &&
      Number.isSafeInteger(size) &&
      size > 0 &&
      size <= 4 * 1024 * 1024
    ));
    assert.equal(
      requests.some(({ url }) =>
        url === `/api/git/objects/generations/${head}/commits.diff`
      ),
      false,
    );
    const commitPatchManifestUpload = requests.find(({ url }) =>
      url === `/api/git/objects/generations/${head}/commit-patches.json`
    );
    assert.deepEqual(JSON.parse(commitPatchManifestUpload.body), {
      version: 1,
      head,
      parts: publication.publication.commitPatchParts,
      size: publication.publication.commitPatchSize,
    });
    assert.ok(publication.publication.packParts.length > 0);
    assert.equal("packKey" in publication.publication, false);
    assert.ok(publication.publication.packParts.every(({ key, size }) =>
      requests.some(({ url }) => url === `/api/git/objects/${key}`) &&
      Number.isSafeInteger(size) &&
      size > 0 &&
      size <= 4 * 1024 * 1024
    ));
    assert.equal(requests.some(({ url }) => url?.endsWith("/repository.pack")), false);
    const snapshotUpload = requests.find(({ url }) =>
      url === `/api/git/objects/generations/${head}/repository.json`
    );
    const commitsUpload = requests.find(({ url }) =>
      url === `/api/git/objects/generations/${head}/commits.json`
    );
    const commitIndexUpload = requests.find(({ url }) =>
      url === `/api/git/objects/generations/${head}/commit-index.json`
    );
    const commitPatchBody = publication.publication.commitPatchParts
      .map(({ key }) => requests.find(({ url }) => url === `/api/git/objects/${key}`)?.body)
      .join("");
    assert.equal(JSON.parse(snapshotUpload.body).repository.branch, "master");
    const publishedCommits = JSON.parse(commitsUpload.body);
    const publishedCommitIndex = JSON.parse(commitIndexUpload.body);
    assert.equal(publishedCommitIndex.hashes.length, 33);
    assert.equal(publishedCommitIndex.scopeCounts.all, 33);
    assert.equal(publishedCommitIndex.scopeCounts.perf, 1);
    assert.equal(publishedCommitIndex.scopeCounts.fix, 1);
    assert.deepEqual(publishedCommits[0].refs, ["HEAD -> master"]);
    assert.deepEqual(
      [...commitPatchBody.matchAll(/^From ([a-f0-9]{40}) Mon Sep 17 00:00:00 2001$/gm)]
        .map((match) => match[1]),
      publishedCommits.map(({ hash }) => hash),
    );
    assert.ok(requests.some(({ url }) =>
      url === `/api/git/objects/generations/${head}/commits/0000.json`
    ));
    assert.ok(requests.some(({ url }) =>
      url === `/api/git/objects/generations/${head}/commits/0001.json`
    ));
    assert.equal(requests.some(({ url }) =>
      url === `/api/git/objects/generations/${head}/commits/0000`
    ), false);

    deploymentSha = "0".repeat(40);
    const mismatchRequestIndex = requests.length;
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], {
        env: {
          ...process.env,
          NANOCODEX_GIT_ORIGIN: `http://127.0.0.1:${address.port}`,
          NANOCODEX_GIT_TOKEN: "publisher-test-token",
          NANOCODEX_REPO: repository,
          PATH: `${authority.bin}:${process.env.PATH ?? ""}`,
        },
        encoding: "utf8",
        maxBuffer: 16 * 1024 * 1024,
      }),
      new RegExp(`Cloudflare Worker revision ${"0".repeat(40)} does not match repository ${head}`),
    );
    assert.deepEqual(
      requests.slice(mismatchRequestIndex).map(({ authorization, method, url }) => ({
        authorization,
        method,
        url,
      })),
      [{ authorization: undefined, method: "GET", url: "/api/health" }],
    );

    deploymentSha = head;
    await writeFile(authority.control, JSON.stringify({
      head: movedAuthorityHead,
      skipLsRemote: 1,
    }));
    const staleStart = requests.length;
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], {
        env: {
          ...process.env,
          NANOCODEX_GIT_ORIGIN: `http://127.0.0.1:${address.port}`,
          NANOCODEX_GIT_TOKEN: "publisher-test-token",
          NANOCODEX_REPO: repository,
          PATH: `${authority.bin}:${process.env.PATH ?? ""}`,
        },
        encoding: "utf8",
        maxBuffer: 16 * 1024 * 1024,
      }),
      /local HEAD, fetched origin\/master, and fresh GitHub master to be identical/,
    );
    assert.equal(
      requests.slice(staleStart).some(({ method }) => method === "PUT"),
      false,
      "a coherent authority move at the pre-upload recheck must prevent mutation",
    );
    await git(["reset", "--hard", head], repository);
    await git(["update-ref", "refs/remotes/origin/master", head], repository);
    await git(["update-ref", "refs/heads/master", head], authoritativeRemote);

    advanceAuthorityOnObjectUpload = true;
    const publicationRaceStart = requests.length;
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], {
        env: {
          ...process.env,
          NANOCODEX_GIT_ORIGIN: `http://127.0.0.1:${address.port}`,
          NANOCODEX_GIT_TOKEN: "publisher-test-token",
          NANOCODEX_REPO: repository,
          PATH: `${authority.bin}:${process.env.PATH ?? ""}`,
        },
        encoding: "utf8",
        maxBuffer: 16 * 1024 * 1024,
      }),
      /local HEAD, fetched origin\/master, and fresh GitHub master to be identical/,
    );
    const publicationRaceRequests = requests.slice(publicationRaceStart);
    assert.ok(publicationRaceRequests.some(({ method, url }) =>
      method === "PUT" && url?.startsWith("/api/git/objects/")
    ));
    assert.equal(publicationRaceRequests.some(({ method, url }) =>
      method === "PUT" && url === "/api/git/publish"
    ), false, "a coherent authority move after uploads must prevent pointer publication");
  } finally {
    if (server.listening) await new Promise((resolveClose) => server.close(resolveClose));
    await rm(directory, { recursive: true, force: true });
  }
});

test("repository publication uploads only content absent from the prior inventory", () => {
  assert.deepEqual(
    buildUploadPlan(
      { blobs: ["a", "b"], patches: ["1", "2"] },
      { blobs: ["a"], patches: ["1"] },
    ),
    { blobs: ["b"], patches: ["2"] },
  );
});

test("repository authority rejects isolated, alternate, detached, dirty, shallow, rewritten, and extra-master repositories", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-repository-authority-test-"));
  const repository = resolve(directory, "repo");
  const baseEnv = {
    ...process.env,
    NANOCODEX_GIT_ORIGIN: "http://127.0.0.1:9",
    NANOCODEX_GIT_TOKEN: "unused-repository-token",
    NANOCODEX_REPO: repository,
  };
  try {
    await git(["init", "-q", "-b", "master", repository], directory);
    await git(["config", "user.name", "Nanocodex Test"], repository);
    await git(["config", "user.email", "test@nanocodex.invalid"], repository);
    await writeFile(resolve(repository, "README.md"), "# authority fixture\n");
    await git(["add", "README.md"], repository);
    await git(["commit", "-qm", "authority fixture"], repository);
    const head = await git(["rev-parse", "HEAD"], repository);
    await git(["switch", "-qc", "future-authority"], repository);
    await writeFile(resolve(repository, "README.md"), "# future authority fixture\n");
    await git(["commit", "-qam", "future authority fixture"], repository);
    const futureHead = await git(["rev-parse", "HEAD"], repository);
    await git(["switch", "-q", "master"], repository);

    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], { env: baseEnv, encoding: "utf8" }),
      /one canonical HTTPS origin/,
      "an isolated local repository must not acquire mirror publication authority",
    );
    await git(["remote", "add", "origin", "git@github.com:gakonst/nanocodex.git"], repository);
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], { env: baseEnv, encoding: "utf8" }),
      /one canonical HTTPS origin/,
    );
    await git(["remote", "set-url", "origin", "https://github.com/gakonst/nanocodex.git"], repository);
    const authority = await installAuthoritativeGitShim({
      directory,
      repository,
      masterHead: head,
      retainedHeads: [futureHead],
    });
    const env = { ...baseEnv, PATH: `${authority.bin}:${process.env.PATH ?? ""}` };

    await git(["remote", "add", "backup", "https://github.com/gakonst/nanocodex.git"], repository);
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], { env, encoding: "utf8" }),
      /one canonical HTTPS origin/,
    );
    await git(["remote", "remove", "backup"], repository);

    await writeFile(resolve(repository, "UNTRACKED"), "dirty\n");
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], { env, encoding: "utf8" }),
      /clean tracked and untracked checkout/,
    );
    await rm(resolve(repository, "UNTRACKED"));

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

    await git(["replace", head, futureHead], repository);
    await assert.rejects(
      execFileAsync(process.execPath, [publisherPath], { env, encoding: "utf8" }),
      /rejects Git replacement objects/,
    );
    await git(["replace", "-d", head], repository);

    const graftPath = await git(["rev-parse", "--git-path", "info/grafts"], repository);
    await writeFile(resolve(repository, graftPath), `${head} ${futureHead}\n`);
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
      assertRepositoryAuthorityWithPath(repository, head, authority.bin),
      /rejects extra remote-tracking master refs/,
    );
    await git(["symbolic-ref", "--delete", "refs/remotes/origin/master"], repository);
    await git(["update-ref", "refs/remotes/origin/master", head], repository);

    assert.deepEqual(await prepareRepositoryAuthorityWithPath(repository, authority.bin), {
      head,
      ref: "refs/heads/master",
    });
    await git(["reset", "--hard", futureHead], repository);
    await git(["update-ref", "refs/remotes/origin/master", futureHead], repository);
    await git(["update-ref", "refs/heads/master", futureHead], authority.remote);
    await assert.rejects(
      assertRepositoryAuthorityWithPath(repository, head, authority.bin),
      /local HEAD, fetched origin\/master, and fresh GitHub master to be identical/,
      "a coherent new authority must not validate artifacts captured from the old head",
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("repository packs are divided into canonical bounded upload parts", () => {
  const head = "a".repeat(40);
  const packHash = "b".repeat(40);
  const partBytes = 4 * 1024 * 1024;
  const packSize = (partBytes * 4) + 1_100_465;
  const parts = buildRepositoryPackParts(head, packHash, packSize);

  assert.equal(parts.length, 5);
  assert.equal(parts.reduce((total, part) => total + part.size, 0), packSize);
  assert.deepEqual(parts.map(({ key, offset, size }) => ({ key, offset, size })), [
    { key: `generations/${head}/packs/${packHash}/0000.pack`, offset: 0, size: partBytes },
    { key: `generations/${head}/packs/${packHash}/0001.pack`, offset: partBytes, size: partBytes },
    { key: `generations/${head}/packs/${packHash}/0002.pack`, offset: partBytes * 2, size: partBytes },
    { key: `generations/${head}/packs/${packHash}/0003.pack`, offset: partBytes * 3, size: partBytes },
    { key: `generations/${head}/packs/${packHash}/0004.pack`, offset: partBytes * 4, size: 1_100_465 },
  ]);
});

test("commit-aligned patch pages retain contiguous bounded descriptors", () => {
  const head = "a".repeat(40);
  const partBytes = 4 * 1024 * 1024;
  const pages = [
    { name: "0000", path: "/tmp/0000.diff", size: 476_879 },
    { name: "0001", path: "/tmp/0001.diff", size: 2_105_727 },
  ];
  const parts = buildCommitPatchParts(head, pages);

  assert.deepEqual(parts, [
    {
      key: `generations/${head}/commit-patches/0000.diff`,
      path: "/tmp/0000.diff",
      size: 476_879,
    },
    {
      key: `generations/${head}/commit-patches/0001.diff`,
      path: "/tmp/0001.diff",
      size: 2_105_727,
    },
  ]);
  assert.throws(
    () => buildCommitPatchParts(head, [pages[0], { ...pages[1], name: "0002" }]),
    /page 0001 is invalid/,
  );
  assert.throws(
    () => buildCommitPatchParts(head, [{ ...pages[0], size: partBytes + 1 }]),
    /page 0000 is invalid/,
  );
  assert.throws(
    () => buildCommitPatchParts(head, Array.from({ length: 257 }, (_, index) => ({
      name: String(index).padStart(4, "0"),
      path: `/tmp/${index}.diff`,
      size: 1,
    }))),
    /page count is invalid/,
  );
});

test("repository uploads retry bounded transient responses and transport failures", () => {
  for (const status of [401, 408, 425, 429, 500, 503]) {
    assert.equal(isRetriableUploadStatus(status), true, `${status} should retry`);
  }
  for (const status of [400, 403, 404, 409, 422]) {
    assert.equal(isRetriableUploadStatus(status), false, `${status} should fail`);
  }
  assert.equal(isRetriableUploadError(new TypeError("fetch failed")), true);
  assert.equal(isRetriableUploadError(new DOMException("timed out", "TimeoutError")), true);
  assert.equal(isRetriableUploadError(new Error("invalid local input")), false);
});

test("repository state reads retry a transient secret-rollout response", async () => {
  let attempts = 0;
  const server = createServer((request, response) => {
    attempts += 1;
    assert.equal(request.headers.authorization, "Bearer mirror-token");
    if (attempts === 1) {
      response.writeHead(500, { "content-type": "text/html" });
      response.end("transient Worker rollout");
      return;
    }
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({ publication: { head: "current" } }));
  });
  try {
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const address = server.address();
    assert.ok(address && typeof address === "object");
    const state = await readRemoteState(
      `http://127.0.0.1:${address.port}`,
      "mirror-token",
    );
    assert.deepEqual(state, { publication: { head: "current" } });
    assert.equal(attempts, 2);
  } finally {
    if (server.listening) await new Promise((resolveClose) => server.close(resolveClose));
  }
});

test("invalid publication repair requires an explicit operator opt-in", async () => {
  let authorization;
  const server = createServer(async (request, response) => {
    authorization = request.headers.authorization;
    response.writeHead(503, { "content-type": "application/json" });
    response.end(JSON.stringify({ error: "repository publication is invalid" }));
  });
  try {
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const address = server.address();
    assert.ok(address && typeof address === "object");
    const origin = `http://127.0.0.1:${address.port}`;

    await assert.rejects(
      readRemoteState(origin, "mirror-token"),
      /NANOCODEX_REPAIR_INVALID_PUBLICATION=1/,
    );
    assert.deepEqual(
      await readRemoteState(origin, "mirror-token", true),
      { replaceInvalid: true },
    );
    assert.equal(authorization, "Bearer mirror-token");
  } finally {
    if (server.listening) await new Promise((resolveClose) => server.close(resolveClose));
  }
});

test("Git artifacts contain only advertised refs and reuse immutable object shards", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-publisher-test-"));
  const repository = resolve(directory, "repo");
  const firstOutput = resolve(directory, "first");
  const secondOutput = resolve(directory, "second");
  try {
    await git(["init", "-q", "-b", "main", repository], directory);
    await git(["config", "user.name", "Nanocodex Test"], repository);
    await git(["config", "user.email", "test@nanocodex.invalid"], repository);
    await writeFile(resolve(repository, "public.txt"), "public\n");
    await git(["add", "public.txt"], repository);
    await git(["commit", "-qm", "public root"], repository);
    const firstHead = await git(["rev-parse", "HEAD"], repository);
    await git(["tag", "root"], repository);

    await git(["switch", "-qc", "hidden"], repository);
    await writeFile(resolve(repository, "secret.txt"), "not advertised\n");
    await git(["add", "secret.txt"], repository);
    await git(["commit", "-qm", "hidden work"], repository);
    const hiddenCommit = await git(["rev-parse", "HEAD"], repository);
    const hiddenBlob = await git(["rev-parse", "HEAD:secret.txt"], repository);
    await git(["switch", "-q", "main"], repository);
    await mkdir(firstOutput);

    const first = await buildGitArtifacts({
      repository,
      temporaryDirectory: firstOutput,
      head: firstHead,
      refs: [
        { name: "refs/heads/main", oid: firstHead },
        { name: "refs/tags/root", oid: firstHead },
      ],
      previousManifest: null,
    });
    assert.equal(first.manifest.objects[hiddenCommit], undefined);
    assert.equal(first.manifest.objects[hiddenBlob], undefined);
    assert.ok(first.manifest.objects[firstHead]);
    const firstPack = await readFile(first.packPath);
    assert.equal(first.packSize, firstPack.byteLength);
    assert.deepEqual(
      Buffer.concat(first.packParts.map(({ offset, size }) =>
        firstPack.subarray(offset, offset + size)
      )),
      firstPack,
    );
    const advertisedPack = await git(
      ["verify-pack", "-v", resolve(firstOutput, "repository.idx")],
      repository,
    );
    assert.equal(advertisedPack.includes(hiddenCommit), false);

    await writeFile(resolve(repository, "public.txt"), "public\nsmall update\n");
    await git(["add", "public.txt"], repository);
    await git(["commit", "-qm", "small update"], repository);
    const secondHead = await git(["rev-parse", "HEAD"], repository);
    await mkdir(secondOutput);
    const second = await buildGitArtifacts({
      repository,
      temporaryDirectory: secondOutput,
      head: secondHead,
      refs: [{ name: "refs/heads/main", oid: secondHead }],
      previousManifest: first.manifest,
    });
    assert.equal(second.manifest.objects[firstHead][1], first.manifest.objects[firstHead][1]);
    assert.equal(second.manifest.shards[0].key, first.manifest.shards[0].key);
    assert.ok(second.shards.length > 0);
    assert.ok(second.shards.every((shard) => shard.key.includes(secondHead)));
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

async function git(args, cwd) {
  const { stdout } = await execFileAsync("git", args, {
    cwd,
    encoding: "utf8",
    maxBuffer: 16 * 1024 * 1024,
  });
  return stdout.trim();
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
  const control = resolve(directory, "authority-control.json");
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
  await Promise.all([mkdir(bin), writeFile(log, "")]);
  const shim = resolve(bin, "git");
  await writeFile(shim, `#!/usr/bin/env node
const { spawnSync } = require("node:child_process");
const { appendFileSync, readFileSync, unlinkSync, writeFileSync } = require("node:fs");
const args = process.argv.slice(2);
let command = 0;
while (args[command] === "-c") command += 2;
const gitEnvironment = Object.fromEntries(Object.entries(process.env).filter(([name]) =>
  name.startsWith("GIT_")
));
appendFileSync(
  ${JSON.stringify(log)},
  JSON.stringify({ args: args.slice(command), gitEnvironment }) + "\\n",
);
if (args[command] === "ls-remote") {
  let control;
  try { control = JSON.parse(readFileSync(${JSON.stringify(control)}, "utf8")); } catch {}
  if (control != null && control.skipLsRemote > 0) {
    control.skipLsRemote -= 1;
    writeFileSync(${JSON.stringify(control)}, JSON.stringify(control));
  } else if (control != null) {
    const local = spawnSync(${JSON.stringify(realGit)}, [
      "-C", ${JSON.stringify(repository)}, "reset", "--hard", control.head,
    ], { env: process.env, stdio: "ignore" });
    const tracking = spawnSync(${JSON.stringify(realGit)}, [
      "-C", ${JSON.stringify(repository)}, "update-ref", "refs/remotes/origin/master", control.head,
    ], { env: process.env, stdio: "ignore" });
    const bareEnvironment = { ...process.env };
    delete bareEnvironment.GIT_WORK_TREE;
    const remote = spawnSync(${JSON.stringify(realGit)}, [
      "--git-dir", ${JSON.stringify(remote)}, "update-ref", "refs/heads/master", control.head,
    ], { env: bareEnvironment, stdio: "ignore" });
    if (local.status !== 0 || tracking.status !== 0 || remote.status !== 0) process.exit(97);
    unlinkSync(${JSON.stringify(control)});
  }
}
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
  return { bin, control, log, remote };
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

function prepareRepositoryAuthorityWithPath(repository, bin) {
  return withAuthorityPath(bin, () => prepareMasterPublication(repository));
}

function assertRepositoryAuthorityWithPath(repository, head, bin) {
  return withAuthorityPath(bin, () => assertMasterPublicationAuthority(repository, head));
}
