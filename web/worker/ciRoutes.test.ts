import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { test } from "node:test";

import { routeCiRequest, type CiStorageEnv } from "./ciRoutes.ts";
import type { CiSourcePublication } from "./ciSource.ts";

const head = "a".repeat(40);
const cargoLockBlob = "c".repeat(40);
const rustSecRevision = "d".repeat(40);
const archiveBody = new TextEncoder().encode("archive");
const archiveSha = "1".repeat(64);
const cargoVendorBody = new TextEncoder().encode("cargo vendor bundle");
const cargoVendorSha256 = createHash("sha256").update(cargoVendorBody).digest("hex");
const rustSecBody = new TextEncoder().encode("RustSec advisory database bundle");
const snapshotId = "123e4567-e89b-42d3-a456-426614174000";
const snapshotBody = new TextEncoder().encode("0123456789abcdef");
const treeBody = new TextEncoder().encode(JSON.stringify({
  version: 1,
  head,
  archive: { size: archiveBody.byteLength, sha256: archiveSha },
  files: [
    { path: "Cargo.lock", sha: cargoLockBlob, mode: "100644" },
    { path: "Cargo.toml", sha: "b".repeat(40), mode: "100644" },
  ],
}));

test("CI source mutations authenticate before touching storage", async () => {
  const response = await route(new Request("https://ci.test/api/ci/source/publish", {
    method: "PUT",
    body: "{}",
  }), {});
  assert.equal(response.status, 401);
  assert.equal(response.headers.get("www-authenticate"), "Bearer");
  const cargo = await route(new Request(
    `https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/${cargoVendorSha256}/bundle.tar.gz`,
    { method: "PUT", body: cargoVendorBody },
  ), {});
  assert.equal(cargo.status, 401);
  const rustSec = await route(new Request(
    `https://ci.test/api/ci/rustsec-advisory-db/${rustSecRevision}/bundle.tar.gz`,
    { method: "PUT", body: rustSecBody },
  ), {});
  assert.equal(rustSec.status, 401);
  const close = await route(new Request(
    "https://ci.test/api/ci/source/pull-requests/7/state",
    { method: "DELETE", body: "{}" },
  ), {});
  assert.equal(close.status, 401);
  const sourceHead = await route(new Request(
    `https://ci.test/api/ci/source/objects/${head}/source.tar.gz`,
    { method: "HEAD" },
  ), {});
  assert.equal(sourceHead.status, 401);
});

test("source capabilities reject cross-lane authority before every storage binding", async () => {
  const storageAccesses: string[] = [];
  const storageTripwire = new Proxy({}, {
    get(_target, property) {
      return () => {
        storageAccesses.push(String(property));
        throw new Error(`unexpected storage access through ${String(property)}`);
      };
    },
  });
  const env = configured(memoryBucket(), memoryNamespace());
  env.CI_SOURCE = storageTripwire as unknown as R2Bucket;
  env.BACKUP_BUCKET = storageTripwire as unknown as R2Bucket;
  env.CI_REPOSITORY = storageTripwire as unknown as DurableObjectNamespace;
  env.CI_WORKFLOW = storageTripwire as unknown as Workflow;
  env.SANDBOX = storageTripwire as unknown as DurableObjectNamespace<
    import("@cloudflare/ci/worker").CiSandbox
  >;

  const masterPublication = sourcePublication(
    archiveSha,
    "2".repeat(64),
    "4".repeat(64),
  );
  const pullRequestPublication: CiSourcePublication = {
    ...masterPublication,
    branch: "pull/7/merge",
    ref: "refs/pull/7/merge",
    lane: { type: "pull_request", number: 7, pullRequestHead: "b".repeat(40) },
  };
  const closeBody = JSON.stringify({
    closeId: "123e4567-e89b-42d3-a456-426614174000",
    expectedMergeHead: head,
    expectedPullRequestHead: "b".repeat(40),
  });
  const forbiddenRequests = [
    new Request("https://ci.test/api/ci/source/publish", {
      method: "PUT",
      headers: masterAuth({ "content-type": "application/json" }),
      body: JSON.stringify({ expectedHead: null, publication: pullRequestPublication }),
    }),
    new Request("https://ci.test/api/ci/source/publish", {
      method: "PUT",
      headers: prAuth({ "content-type": "application/json" }),
      body: JSON.stringify({ expectedHead: null, publication: masterPublication }),
    }),
    new Request("https://ci.test/api/ci/source/pull-requests/7/state", {
      headers: masterAuth(),
    }),
    new Request("https://ci.test/api/ci/source/pull-requests/7/state", {
      method: "DELETE",
      headers: masterAuth({ "content-type": "application/json" }),
      body: closeBody,
    }),
    new Request(
      `https://ci.test/api/ci/source/pull-requests/7/publications/${head}`,
      { headers: masterAuth() },
    ),
    new Request(
      `https://ci.test/api/ci/source/master/publications/${head}`,
      { headers: prAuth() },
    ),
    new Request(
      `https://ci.test/api/ci/rustsec-advisory-db/${rustSecRevision}/bundle.tar.gz`,
      {
        method: "PUT",
        headers: prAuth({
          "content-length": String(rustSecBody.byteLength),
          "content-type": "application/gzip",
          "x-nanocodex-sha256": "6".repeat(64),
        }),
        body: byteBuffer(rustSecBody),
      },
    ),
    new Request("https://ci.test/api/ci/source/state", {
      method: "DELETE",
      headers: prAuth(),
    }),
  ];
  for (const request of forbiddenRequests) {
    const response = await route(request, env);
    assert.equal(response.status, 403, `${request.method} ${new URL(request.url).pathname}`);
  }
  const malformed = await route(new Request("https://ci.test/api/ci/source/publish", {
    method: "PUT",
    headers: masterAuth({ "content-type": "application/json" }),
    body: JSON.stringify({ expectedHead: null, publication: {} }),
  }), env);
  assert.equal(malformed.status, 400);

  const releaseHeaders = { authorization: "Bearer release-token" };
  for (const request of [
    new Request("https://ci.test/api/ci/source/state", { headers: releaseHeaders }),
    new Request(`https://ci.test/api/ci/source/master/publications/${head}`, {
      method: "HEAD",
      headers: releaseHeaders,
    }),
    new Request(`https://ci.test/api/ci/source/objects/${head}/source.tar.gz`, {
      method: "HEAD",
      headers: releaseHeaders,
    }),
    new Request(`https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/${cargoVendorSha256}/bundle.tar.gz`, {
      method: "HEAD",
      headers: releaseHeaders,
    }),
    new Request(
      `https://ci.test/api/ci/rustsec-advisory-db/${rustSecRevision}/bundle.tar.gz`,
      { method: "HEAD", headers: releaseHeaders },
    ),
  ]) {
    assert.equal((await route(request, env)).status, 401);
  }

  for (const authorization of [masterAuth(), prAuth()]) {
    for (const request of [
      new Request(`https://ci.test/api/ci/runs/${head}/terminate`, {
        method: "POST",
        headers: authorization,
      }),
      new Request("https://ci.test/api/ci/macos/claims", {
        method: "POST",
        headers: authorization,
      }),
      new Request("https://ci.test/api/ci/releases/nightly", {
        method: "POST",
        headers: authorization,
      }),
    ]) {
      assert.equal((await route(request, env)).status, 401);
    }
  }

  const ambiguousEnv = {
    ...env,
    CI_MASTER_SOURCE_WRITE_TOKEN: "shared-source-token",
    CI_PR_SOURCE_WRITE_TOKEN: "shared-source-token",
  };
  assert.equal((await route(new Request(
    `https://ci.test/api/ci/source/objects/${head}/source.tar.gz`,
    {
      method: "HEAD",
      headers: { authorization: "Bearer shared-source-token" },
    },
  ), ambiguousEnv)).status, 403);
  assert.deepEqual(storageAccesses, []);
});

test("authenticated PR source routes preserve close CAS, reopen proof, and retained identity", async () => {
  const calls: Array<{ path: string; method: string; body?: unknown }> = [];
  const namespace = forwardingNamespace(async (request) => {
    calls.push({
      path: new URL(request.url).pathname,
      method: request.method,
      ...(request.method === "DELETE" ? { body: await request.json() } : {}),
    });
    return Response.json({ forwarded: true });
  });
  const env = configured(memoryBucket(), memoryNamespace());
  env.CI_REPOSITORY = namespace;
  const closeBody = {
    closeId: "123e4567-e89b-42d3-a456-426614174000",
    expectedMergeHead: head,
    expectedPullRequestHead: "b".repeat(40),
  };
  assert.equal((await route(new Request(
    "https://ci.test/api/ci/source/state",
    { headers: prAuth() },
  ), env)).status, 200);
  assert.equal((await route(new Request(
    "https://ci.test/api/ci/source/state",
    { headers: masterAuth() },
  ), env)).status, 200);
  assert.equal((await route(new Request(
    "https://ci.test/api/ci/source/pull-requests/7/state",
    { headers: prAuth() },
  ), env)).status, 200);
  assert.equal((await route(new Request(
    "https://ci.test/api/ci/source/pull-requests/7/state",
    {
      method: "DELETE",
      headers: prAuth({ "content-type": "application/json" }),
      body: JSON.stringify(closeBody),
    },
  ), env)).status, 200);
  assert.equal((await route(new Request(
    `https://ci.test/api/ci/source/pull-requests/7/publications/${head}`,
    { headers: prAuth() },
  ), env)).status, 200);
  assert.equal((await route(new Request(
    `https://ci.test/api/ci/source/master/publications/${head}`,
    { headers: masterAuth() },
  ), env)).status, 200);
  const releaseHeaders = { authorization: "Bearer release-token" };
  assert.equal((await route(new Request(
    `https://ci.test/api/ci/source/master/publications/${head}`,
    { headers: releaseHeaders },
  ), env)).status, 200);
  assert.equal((await route(new Request(
    `https://ci.test/api/ci/source/pull-requests/7/publications/${head}`,
    { headers: releaseHeaders },
  ), env)).status, 401);
  assert.equal((await route(new Request(
    "https://ci.test/api/ci/source/publish",
    { method: "PUT", headers: releaseHeaders, body: "{}" },
  ), env)).status, 401);
  assert.deepEqual(calls, [
    { path: "/state", method: "GET" },
    { path: "/state", method: "GET" },
    { path: "/pull-requests/7/state", method: "GET" },
    { path: "/pull-requests/7/state", method: "DELETE", body: closeBody },
    { path: `/pull-requests/7/publications/${head}`, method: "GET" },
    { path: `/master/publications/${head}`, method: "GET" },
    { path: `/master/publications/${head}`, method: "GET" },
  ]);
});

test("release reads are public while macOS runner routes require their dedicated token", async () => {
  const releasePaths: string[] = [];
  const macPaths: string[] = [];
  const releases = forwardingNamespace(async (request) => {
    releasePaths.push(new URL(request.url).pathname);
    return Response.json({ channel: "latest" });
  });
  const mac = forwardingNamespace(async (request) => {
    macPaths.push(new URL(request.url).pathname);
    return Response.json({ job: null });
  });
  const env: CiStorageEnv = {
    CI_RELEASES: releases,
    CI_MACOS_JOBS: mac,
    CI_MACOS_RUNNER_TOKEN: "mac-runner-token",
  };

  const release = await route(
    new Request("https://ci.test/api/releases/channels/latest"),
    env,
  );
  assert.equal(release.status, 200);
  assert.deepEqual(releasePaths, ["/channels/latest"]);

  const unauthorized = await route(
    new Request("https://ci.test/api/ci/macos/claims", { method: "POST" }),
    env,
  );
  assert.equal(unauthorized.status, 401);
  assert.deepEqual(macPaths, []);

  const authorized = await route(
    new Request("https://ci.test/api/ci/macos/claims", {
      method: "POST",
      headers: { authorization: "Bearer mac-runner-token" },
    }),
    env,
  );
  assert.equal(authorized.status, 200);
  assert.deepEqual(macPaths, ["/api/ci/macos/claims"]);
});

test("distribution state and bounded step evidence are served from their immutable identity", async () => {
  const env = configured(memoryBucket(), memoryNamespace());
  const prefix = "distribution/stable/v1.2.3";
  const stdout = new TextEncoder().encode("release build passed\n");
  await env.backup.put(`${prefix}/result.json`, JSON.stringify({
    version: 1,
    status: "ready",
    channel: "stable",
    tagName: "v1.2.3",
    head,
  }));
  await env.backup.put(`${prefix}/steps/linux/stdout.log`, stdout);
  await env.backup.put(`${prefix}/steps/linux/result.json`, JSON.stringify({
    version: 1,
    exitCode: 0,
    stdout: { key: `${prefix}/steps/linux/stdout.log`, size: stdout.byteLength },
    stderr: { key: `${prefix}/steps/linux/stderr.log`, size: 0 },
  }));

  const result = await route(
    new Request("https://ci.test/api/ci/distributions/stable/v1.2.3"),
    env,
  );
  assert.equal(result.status, 200);
  assert.equal((await result.json() as { status: string }).status, "ready");
  const log = await route(
    new Request(
      "https://ci.test/api/ci/distributions/stable/v1.2.3/steps/linux/stdout.log",
    ),
    env,
  );
  assert.equal(log.status, 200);
  assert.equal(await log.text(), "release build passed\n");
  assert.equal((await route(
    new Request(`https://ci.test/api/ci/distributions/stable/${head}`),
    env,
  )).status, 404);
});

test("macOS run evidence exposes bounded logs without leaking runner identity", async () => {
  const env = configured(memoryBucket(), memoryNamespace());
  const claim = "123e4567-e89b-42d3-a456-426614174000";
  const key = `macos/jobs/macos-native-build-${head}/attempts/${claim}/stdout.log`;
  const body = new TextEncoder().encode("macOS passed\n");
  const sha256 = [...new Uint8Array(await crypto.subtle.digest("SHA-256", body))]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
  await env.backup.put(key, body, { customMetadata: { sha256 } });
  const job = {
    state: "completed",
    queuedAt: "2026-08-22T00:00:00.000Z",
    attempts: 1,
    completedAt: "2026-08-22T00:01:00.000Z",
    result: {
      outcome: "success",
      exitCode: 0,
      durationMs: 60_000,
      worker: "secret-runner-name",
      host: { hostname: "secret-hostname", platform: "darwin", arch: "arm64" },
      logs: {
        stdout: { key, size: body.byteLength, sha256 },
        stderr: {
          key: `macos/jobs/macos-native-build-${head}/attempts/${claim}/stderr.log`,
          size: 0,
          sha256: "9".repeat(64),
        },
      },
    },
  };
  const mac = forwardingNamespace(async () => Response.json({ job }));
  const configuredMac: CiStorageEnv = {
    ...env,
    CI_MACOS_JOBS: mac,
  };

  const evidence = await route(
    new Request(`https://ci.test/api/ci/runs/${head}/macos`),
    configuredMac,
  );
  assert.equal(evidence.status, 200);
  const evidenceText = await evidence.text();
  assert.doesNotMatch(evidenceText, /secret-runner-name|secret-hostname/);
  assert.match(evidenceText, new RegExp(`/api/ci/runs/${head}/macos/stdout\\.log`));
  const log = await route(
    new Request(`https://ci.test/api/ci/runs/${head}/macos/stdout.log`),
    configuredMac,
  );
  assert.equal(log.status, 200);
  assert.equal(log.headers.get("x-nanocodex-sha256"), sha256);
  assert.equal(await log.text(), "macOS passed\n");
});

test("master and PR capabilities share only immutable source and Cargo object surfaces", async () => {
  for (const capability of ["master", "pull_request"] as const) {
    const bucket = memoryBucket();
    const env = configured(bucket, memoryNamespace());
    const authorization = capability === "master" ? masterAuth : prAuth;

    const archive = await route(
      uploadRequest("source.tar.gz", archiveBody, archiveSha, capability),
      env,
    );
    assert.equal(archive.status, 200, capability);
    const archiveHead = await route(new Request(
      `https://ci.test/api/ci/source/objects/${head}/source.tar.gz`,
      { method: "HEAD", headers: authorization() },
    ), env);
    assert.equal(archiveHead.status, 200, capability);
    assert.equal(archiveHead.headers.get("x-nanocodex-key"),
      `sources/${head}/source.tar.gz`);
    assert.equal(archiveHead.headers.get("x-nanocodex-sha256"), archiveSha);

    const cargoVendorSha = cargoVendorSha256;
    const cargo = await route(cargoVendorUpload(
      cargoVendorSha,
      cargoLockBlob,
      capability,
    ), env);
    assert.equal(cargo.status, 200, capability);
    const cargoHead = await route(new Request(
      `https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/${cargoVendorSha}/bundle.tar.gz`,
      { method: "HEAD", headers: authorization() },
    ), env);
    assert.equal(cargoHead.status, 200, capability);
    assert.equal(cargoHead.headers.get("x-nanocodex-cargo-lock"), cargoLockBlob);
    assert.equal(cargoHead.headers.get("x-nanocodex-sha256"), cargoVendorSha);

    assert.equal((await route(rustSecUploadRequest("6".repeat(64)), env)).status, 200);
    const rustSecHead = await route(new Request(
      `https://ci.test/api/ci/rustsec-advisory-db/${rustSecRevision}/bundle.tar.gz`,
      { method: "HEAD", headers: authorization() },
    ), env);
    assert.equal(rustSecHead.status, 200, capability);
    assert.equal(rustSecHead.headers.get("x-nanocodex-revision"), rustSecRevision);
  }
});

test("local snapshot transport is development-only, ranged, and run-scoped", async () => {
  const bucket = memoryBucket();
  const repository = memoryNamespace();
  const env = configured(bucket, repository);
  await env.backup.put(`backups/${snapshotId}/data.sqsh`, snapshotBody);
  const url = `https://ci.test/api/ci/local-backups/${snapshotId}/data.sqsh?run=${head}`;

  const range = await route(new Request(url, {
    headers: { range: "bytes=3-8" },
  }), env);
  assert.equal(range.status, 206);
  assert.equal(range.headers.get("accept-ranges"), "bytes");
  assert.equal(range.headers.get("content-range"), `bytes 3-8/${snapshotBody.byteLength}`);
  assert.equal(range.headers.get("content-length"), "6");
  assert.equal(await range.text(), "345678");

  const headResponse = await route(new Request(url, { method: "HEAD" }), env);
  assert.equal(headResponse.status, 200);
  assert.equal(headResponse.headers.get("content-length"), String(snapshotBody.byteLength));
  assert.equal(await headResponse.text(), "");

  const invalid = await route(new Request(url, {
    headers: { range: "bytes=999-1000" },
  }), env);
  assert.equal(invalid.status, 416);
  assert.equal(invalid.headers.get("content-range"), `bytes */${snapshotBody.byteLength}`);

  await env.backup.put(`runs/${head}/control/terminated.json`, "{}");
  assert.equal((await route(new Request(url, { headers: { range: "bytes=0-1" } }), env)).status, 409);
  assert.equal((await route(new Request(url.replace(head, "bad")), env)).status, 400);
  assert.equal((await route(new Request(url), { ...env, ENVIRONMENT: "production" })).status, 404);
});

test("public PR resolution binds the current preview checksum and exact native artifacts", async () => {
  const source = memoryBucket();
  const env = configured(source, memoryNamespace());
  const number = 7;
  const pullRequestHead = "b".repeat(40);
  const releasePackageBody = new TextEncoder().encode("release-grade npm package");
  const releasePackageSha = "8".repeat(64);
  const previewBody = new TextEncoder().encode("PR npm preview");
  const previewSha = createHash("sha256").update(previewBody).digest("hex");
  const packageVersion = `0.0.0-preview-${head}`;
  const macName = "nanocodex-aarch64-apple-darwin";
  const linuxName = "nanocodex-x86_64-unknown-linux-gnu";
  const macBody = new TextEncoder().encode("arm64 Mach-O");
  const linuxBody = new TextEncoder().encode("x86-64 ELF");
  const macSha = createHash("sha256").update(macBody).digest("hex");
  const linuxSha = createHash("sha256").update(linuxBody).digest("hex");
  const publication: CiSourcePublication = {
    ...sourcePublication(archiveSha, "2".repeat(64), "4".repeat(64)),
    branch: `pull/${number}/merge`,
    ref: `refs/pull/${number}/merge`,
    lane: { type: "pull_request", number, pullRequestHead },
  };
  const run = {
    version: 1,
    head,
    beforeHead: null,
    workflowId: `ci-${head}`,
    state: "dispatched",
    attempts: 1,
    publishedAt: publication.publishedAt,
    dispatchedAt: publication.publishedAt,
    lastDispatchError: "source-token-must-not-leak",
  };
  let laneMode: "open" | "closed" | "stale" = "open";
  env.CI_REPOSITORY = forwardingNamespace(async (request) => {
    const path = new URL(request.url).pathname;
    if (path === `/pull-requests/${number}/state`) {
      if (laneMode === "closed") {
        return Response.json({
          error: "pull_request_closed",
          number,
          closeId: "123e4567-e89b-42d3-a456-426614174000",
          mergeHead: head,
          pullRequestHead,
          closedAt: "2026-08-22T02:00:00.000Z",
        }, { status: 404 });
      }
      if (laneMode === "stale") {
        return Response.json({
          publication: {
            ...publication,
            branch: "pull/8/merge",
            ref: "refs/pull/8/merge",
            lane: { type: "pull_request", number: 8, pullRequestHead },
          },
          run,
        });
      }
      return Response.json({ publication, run });
    }
    if (path === `/runs/${head}`) return Response.json(run);
    if (path === `/pull-requests/${number}/publications/${head}`) {
      return Response.json({ publication, run });
    }
    return Response.json({ error: "not_found" }, { status: 404 });
  });
  env.controls.status = "complete";
  await env.backup.put(`runs/${head}/result.json`, JSON.stringify({
    version: 1,
    head,
    workflowId: `ci-${head}`,
    status: "success",
    completedAt: "2026-08-22T01:00:00.000Z",
    durationMs: 10_000,
    secret: "github-token-must-not-leak",
    artifacts: [
      {
        key: `runs/${head}/artifacts/web-wasm.tar`,
        size: 1,
        sha256: "9".repeat(64),
        contentType: "application/x-tar",
      },
      {
        key: `runs/${head}/artifacts/npm-package.tgz`,
        size: releasePackageBody.byteLength,
        sha256: releasePackageSha,
        contentType: "application/gzip",
      },
      {
        key: `runs/${head}/artifacts/npm-preview.tgz`,
        size: previewBody.byteLength,
        sha256: previewSha,
        contentType: "application/gzip",
        kind: "npm-preview",
        packageVersion,
        pullRequest: number,
        pullRequestHead,
      },
      {
        key: `runs/${head}/artifacts/${macName}`,
        size: macBody.byteLength,
        sha256: macSha,
        contentType: "application/octet-stream",
        kind: "native-cli",
        name: macName,
        platform: "aarch64-apple-darwin",
      },
      {
        key: `runs/${head}/artifacts/${linuxName}`,
        size: linuxBody.byteLength,
        sha256: linuxSha,
        contentType: "application/octet-stream",
        kind: "native-cli",
        name: linuxName,
        platform: "x86_64-unknown-linux-gnu",
      },
    ],
  }));
  await env.backup.put(`runs/${head}/artifacts/npm-package.tgz`, releasePackageBody, {
    customMetadata: {
      head,
      kind: "npm-package",
      sha256: releasePackageSha,
      internalToken: "r2-token-must-not-leak",
    },
  });
  await env.backup.put(`runs/${head}/artifacts/npm-preview.tgz`, previewBody, {
    customMetadata: {
      head,
      kind: "npm-preview",
      packageVersion,
      pullRequest: String(number),
      pullRequestHead,
      sha256: previewSha,
    },
  });
  for (const [name, platform, body, sha256] of [
    [macName, "aarch64-apple-darwin", macBody, macSha],
    [linuxName, "x86_64-unknown-linux-gnu", linuxBody, linuxSha],
  ] as const) {
    await env.backup.put(`runs/${head}/artifacts/${name}`, body, {
      customMetadata: { head, kind: "native-cli", name, platform, sha256 },
    });
  }

  const response = await route(new Request(
    `https://ci.test/api/ci/pull-requests/${number}`,
  ), env);
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("cache-control"), "no-store");
  const payload = await response.json() as Record<string, unknown> & {
    native: {
      manifestSha256: string;
      manifestPath: string;
      manifest: Record<string, unknown> & { artifacts: Array<Record<string, unknown>> };
    };
  };
  const { native, ...publicPayload } = payload;
  assert.deepEqual(publicPayload, {
    version: 1,
    lane: {
      type: "pull_request",
      number,
      branch: `pull/${number}/merge`,
      ref: `refs/pull/${number}/merge`,
      mergeHead: head,
      pullRequestHead,
    },
    run: {
      version: 1,
      head,
      state: "dispatched",
      publishedAt: publication.publishedAt,
      workflow: { status: "complete" },
      result: {
        version: 1,
        head,
        status: "success",
        completedAt: "2026-08-22T01:00:00.000Z",
        durationMs: 10_000,
      },
    },
    preview: {
      kind: "npm-preview",
      packageVersion,
      size: previewBody.byteLength,
      sha256: previewSha,
      contentType: "application/gzip",
      downloadPath: `/api/ci/pull-requests/${number}/builds/${pullRequestHead}/${head}/artifacts/npm-preview/${previewSha}.tgz`,
    },
  });
  const previewPath = (publicPayload.preview as { downloadPath: string }).downloadPath;
  const previewDownload = await route(new Request(`https://ci.test${previewPath}`), env);
  assert.equal(previewDownload.status, 200);
  assert.equal(
    previewDownload.headers.get("cache-control"),
    "public, max-age=31536000, immutable",
  );
  assert.equal(previewDownload.headers.get("x-nanocodex-package-version"), packageVersion);
  assert.equal(previewDownload.headers.get("x-nanocodex-sha256"), previewSha);
  assert.deepEqual(new Uint8Array(await previewDownload.arrayBuffer()), previewBody);
  const previewHeadResponse = await route(new Request(`https://ci.test${previewPath}`, {
    method: "HEAD",
  }), env);
  assert.equal(previewHeadResponse.status, 200);
  assert.equal(previewHeadResponse.headers.get("content-length"), String(previewBody.byteLength));
  assert.equal(await previewHeadResponse.text(), "");
  assert.equal((await route(new Request(
    `https://ci.test${previewPath.replace(previewSha, "0".repeat(64))}`,
  ), env)).status, 404);
  assert.equal((await route(new Request(
    `https://ci.test${previewPath.replace(pullRequestHead, "c".repeat(40))}`,
  ), env)).status, 404);
  assert.equal((await route(new Request(
    `https://ci.test/api/ci/runs/${head}/artifacts/npm-preview.tgz`,
  ), env)).status, 404, "PR previews are not exposed as release-grade run artifacts");
  assert.match(native.manifestSha256, /^[a-f0-9]{64}$/);
  assert.equal(
    native.manifestSha256,
    "aaa3b9fb149a86032e2df83157f58c04ba7ddb7fffa79eea57cbdc2b4f3f13ff",
  );
  assert.equal(native.manifest.manifestSha256, native.manifestSha256);
  assert.equal(
    native.manifestPath,
    `/api/ci/pull-requests/${number}/builds/${pullRequestHead}/${head}/manifests/${native.manifestSha256}`,
  );
  assert.deepEqual(native.manifest.artifacts.map(({ name, platform, size, sha256 }) => ({
    name,
    platform,
    size,
    sha256,
  })), [
    { name: macName, platform: "aarch64-apple-darwin", size: macBody.byteLength, sha256: macSha },
    { name: linuxName, platform: "x86_64-unknown-linux-gnu", size: linuxBody.byteLength, sha256: linuxSha },
  ]);
  const exact = await route(new Request(`https://ci.test${native.manifestPath}`), env);
  assert.equal(exact.status, 200);
  assert.equal(exact.headers.get("cache-control"), "public, max-age=31536000, immutable");
  assert.deepEqual(await exact.json(), native.manifest);
  const macPath = native.manifest.artifacts[0]!.downloadPath as string;
  const macDownload = await route(new Request(`https://ci.test${macPath}`), env);
  assert.equal(macDownload.headers.get("x-nanocodex-manifest-sha256"), native.manifestSha256);
  assert.deepEqual(new Uint8Array(await macDownload.arrayBuffer()), macBody);
  env.controls.status = "unknown";
  const retainedAlias = await route(new Request(
    `https://ci.test/api/ci/pull-requests/${number}`,
  ), env);
  assert.equal(
    retainedAlias.status,
    200,
    "retained terminal evidence keeps an open PR usable after Workflow metadata expires",
  );
  const retainedPayload = await retainedAlias.json() as {
    run: { workflow: { status: string; evidence?: string } };
    preview?: { downloadPath: string };
  };
  assert.deepEqual(retainedPayload.run.workflow, { status: "complete" });
  assert.equal(retainedPayload.preview?.downloadPath, previewPath);
  const retainedRun = await route(new Request(`https://ci.test/api/ci/runs/${head}`), env);
  assert.deepEqual((await retainedRun.json() as {
    workflow: { status: string; evidence?: string };
  }).workflow, { status: "complete", evidence: "retained_result" });
  assert.equal((await route(new Request(`https://ci.test${native.manifestPath}`), env)).status, 200);
  env.controls.status = "complete";
  await env.backup.put(`runs/${head}/artifacts/npm-preview.tgz`, new TextEncoder().encode("tampered"), {
    customMetadata: {
      head,
      kind: "npm-preview",
      packageVersion,
      pullRequest: String(number),
      pullRequestHead,
      sha256: previewSha,
    },
  });
  assert.equal((await route(new Request(`https://ci.test${previewPath}`), env)).status, 404);
  await env.backup.put(`runs/${head}/artifacts/${macName}`, new TextEncoder().encode("tampered"), {
    customMetadata: {
      head,
      kind: "native-cli",
      name: macName,
      platform: "aarch64-apple-darwin",
      sha256: macSha,
    },
  });
  assert.equal((await route(new Request(`https://ci.test${macPath}`), env)).status, 404);

  laneMode = "closed";
  const closed = await route(new Request(
    `https://ci.test/api/ci/pull-requests/${number}`,
  ), env);
  assert.equal(closed.status, 404);
  assert.deepEqual(await closed.json(), { error: "pull_request_not_found" });

  laneMode = "stale";
  const stale = await route(new Request(
    `https://ci.test/api/ci/pull-requests/${number}`,
  ), env);
  assert.equal(stale.status, 404);
  assert.doesNotMatch(await stale.text(), /source-token|github-token|r2-token|closeId/);
});

test("source objects are immutable and publication verifies both R2 objects", async () => {
  const bucket = memoryBucket();
  const repository = memoryNamespace();
  const env = configured(bucket, repository);
  const treeSha = "2".repeat(64);
  const cargoVendorSha = cargoVendorSha256;
  const rustSecSha = "6".repeat(64);

  const archive = await route(uploadRequest("source.tar.gz", archiveBody, archiveSha), env);
  assert.equal(archive.status, 200);
  assert.equal((await archive.json() as { uploaded: boolean }).uploaded, true);
  const replay = await route(uploadRequest("source.tar.gz", archiveBody, archiveSha), env);
  assert.equal((await replay.json() as { uploaded: boolean }).uploaded, false);
  const conflict = await route(uploadRequest("source.tar.gz", archiveBody, "3".repeat(64)), env);
  assert.equal(conflict.status, 409);

  const cargoUpload = await route(cargoVendorUpload(cargoVendorSha), env);
  assert.equal(cargoUpload.status, 200);
  assert.equal((await cargoUpload.json() as { uploaded: boolean }).uploaded, true);
  assert.equal(
    (await (await route(cargoVendorUpload(cargoVendorSha), env)).json() as { uploaded: boolean })
      .uploaded,
    false,
  );
  assert.equal((await route(cargoVendorUpload("5".repeat(64)), env)).status, 400);
  const cargoHead = await route(new Request(
    `https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/${cargoVendorSha}/bundle.tar.gz`,
    { method: "HEAD", headers: masterAuth() },
  ), env);
  assert.equal(cargoHead.status, 200);
  assert.equal(cargoHead.headers.get("x-nanocodex-sha256"), cargoVendorSha);
  assert.equal(cargoHead.headers.get("x-nanocodex-cargo-lock"), cargoLockBlob);
  const cargoDownload = await route(new Request(
    `https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/${cargoVendorSha}/bundle.tar.gz`,
  ), env);
  assert.equal(cargoDownload.status, 200);
  assert.deepEqual(new Uint8Array(await cargoDownload.arrayBuffer()), cargoVendorBody);
  const rustSecUpload = await route(rustSecUploadRequest(rustSecSha), env);
  assert.equal(rustSecUpload.status, 200);
  assert.equal((await rustSecUpload.json() as { uploaded: boolean }).uploaded, true);
  assert.equal(
    (await (await route(rustSecUploadRequest(rustSecSha), env)).json() as { uploaded: boolean })
      .uploaded,
    false,
  );
  assert.equal((await route(rustSecUploadRequest("7".repeat(64)), env)).status, 409);
  const rustSecDownload = await route(new Request(
    `https://ci.test/api/ci/rustsec-advisory-db/${rustSecRevision}/bundle.tar.gz`,
  ), env);
  assert.equal(rustSecDownload.status, 200);
  assert.equal(rustSecDownload.headers.get("x-nanocodex-revision"), rustSecRevision);
  assert.deepEqual(new Uint8Array(await rustSecDownload.arrayBuffer()), rustSecBody);

  assert.equal((await route(uploadRequest("tree.json", treeBody, treeSha), env)).status, 200);
  const publication = sourcePublication(archiveSha, treeSha, cargoVendorSha);
  const published = await route(new Request("https://ci.test/api/ci/source/publish", {
    method: "PUT",
    headers: masterAuth({ "content-type": "application/json" }),
    body: JSON.stringify({ expectedHead: null, publication }),
  }), env);
  assert.equal(published.status, 202);
  assert.deepEqual(repository.published, {
    expectedHead: null,
    leaseId: repository.lastLeaseId,
    publication,
  });

  const pullRequestHead = "e".repeat(40);
  const reopen = {
    closeId: "123e4567-e89b-42d3-a456-426614174000",
    mergeHead: "f".repeat(40),
    pullRequestHead: "9".repeat(40),
  };
  const pullRequestPublication: CiSourcePublication = {
    ...publication,
    branch: "pull/7/merge",
    ref: "refs/pull/7/merge",
    lane: { type: "pull_request", number: 7, pullRequestHead },
  };
  const reopened = await route(new Request("https://ci.test/api/ci/source/publish", {
    method: "PUT",
    headers: prAuth({ "content-type": "application/json" }),
    body: JSON.stringify({ expectedHead: null, publication: pullRequestPublication, reopen }),
  }), env);
  assert.equal(reopened.status, 202);
  assert.deepEqual(repository.published, {
    expectedHead: null,
    leaseId: repository.lastLeaseId,
    publication: pullRequestPublication,
    reopen,
  });
});

test("direct Cargo vendor PUT resolves acknowledgement loss before classifying failures", async () => {
  const bucket = memoryBucket();
  const env = configured(bucket, memoryNamespace());

  const committedBody = new TextEncoder().encode("committed before acknowledgement loss");
  const committedSha = createHash("sha256").update(committedBody).digest("hex");
  bucket.failNextCargoVendorPut("commit_then_throw");
  const committed = await route(cargoVendorUpload(
    committedSha,
    cargoLockBlob,
    "master",
    committedBody,
  ), env);
  assert.equal(committed.status, 200);
  assert.equal((await committed.json() as { uploaded: boolean }).uploaded, false);

  const transientBody = new TextEncoder().encode("transient failure");
  const transientSha = createHash("sha256").update(transientBody).digest("hex");
  bucket.failNextCargoVendorPut("transient");
  const transient = await route(cargoVendorUpload(
    transientSha,
    cargoLockBlob,
    "master",
    transientBody,
  ), env);
  assert.equal(transient.status, 503);
  assert.equal((await transient.json() as { error: string }).error, "cargo_vendor_upload_failed");

  const conflictingBody = new TextEncoder().encode("present but malformed");
  const conflictingSha = createHash("sha256").update(conflictingBody).digest("hex");
  bucket.failNextCargoVendorPut("mismatch_then_throw");
  const conflict = await route(cargoVendorUpload(
    conflictingSha,
    cargoLockBlob,
    "master",
    conflictingBody,
  ), env);
  assert.equal(conflict.status, 409);
  assert.deepEqual(await conflict.json(), { error: "immutable_object_conflict" });

  const checksum = await route(cargoVendorUpload(
    "f".repeat(64),
    "e".repeat(40),
    "master",
    new TextEncoder().encode("definitive checksum mismatch"),
  ), env);
  assert.equal(checksum.status, 400);
  assert.deepEqual(await checksum.json(), {
    error: "invalid_cargo_vendor_bundle_checksum",
  });
});

test("same-lock Cargo bundles coexist and source binds only an exact content hash", async () => {
  const bucket = memoryBucket();
  const firstRepository = memoryNamespace();
  const firstEnv = configured(bucket, firstRepository);
  const alternateBody = cargoVendorBody.slice();
  alternateBody[alternateBody.byteLength - 1] ^= 1;
  const alternateSha = createHash("sha256").update(alternateBody).digest("hex");
  assert.notEqual(alternateSha, cargoVendorSha256);

  assert.equal((await route(cargoVendorUpload(cargoVendorSha256), firstEnv)).status, 200);
  assert.equal(
    (await route(cargoVendorUpload(
      alternateSha,
      cargoLockBlob,
      "pull_request",
      alternateBody,
    ), firstEnv)).status,
    200,
  );
  for (const [sha256, body] of [
    [cargoVendorSha256, cargoVendorBody],
    [alternateSha, alternateBody],
  ] as const) {
    const response = await route(new Request(
      `https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/${sha256}/bundle.tar.gz`,
    ), firstEnv);
    assert.equal(response.status, 200);
    assert.deepEqual(new Uint8Array(await response.arrayBuffer()), body);
  }
  assert.equal((await route(new Request(
    `https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/bundle.tar.gz`,
  ), firstEnv)).status, 404);
  assert.equal((await route(new Request(
    `https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/${"f".repeat(64)}/bundle.tar.gz`,
    {
      method: "PUT",
      headers: masterAuth({
        "content-length": String(cargoVendorBody.byteLength),
        "content-type": "application/gzip",
        "x-nanocodex-sha256": cargoVendorSha256,
      }),
      body: byteBuffer(cargoVendorBody),
    },
  ), firstEnv)).status, 400);

  await route(uploadRequest("source.tar.gz", archiveBody, archiveSha), firstEnv);
  await route(uploadRequest("tree.json", treeBody, "2".repeat(64)), firstEnv);
  await route(rustSecUploadRequest("6".repeat(64)), firstEnv);
  const first = sourcePublication(archiveSha, "2".repeat(64), cargoVendorSha256);
  const second: CiSourcePublication = {
    ...first,
    cargoVendor: {
      key: `cargo-vendor/${cargoLockBlob}/${alternateSha}/bundle.tar.gz`,
      size: alternateBody.byteLength,
      sha256: alternateSha,
    },
  };
  for (const [publication, repository] of [
    [first, firstRepository],
    [second, memoryNamespace()],
  ] as const) {
    const response = await route(new Request("https://ci.test/api/ci/source/publish", {
      method: "PUT",
      headers: masterAuth({ "content-type": "application/json" }),
      body: JSON.stringify({ expectedHead: null, publication }),
    }), configured(bucket, repository));
    assert.equal(response.status, 202, await response.clone().text());
  }
  const crossHash = {
    ...second,
    cargoVendor: { ...second.cargoVendor, key: first.cargoVendor.key },
  };
  assert.equal((await route(new Request("https://ci.test/api/ci/source/publish", {
    method: "PUT",
    headers: masterAuth({ "content-type": "application/json" }),
    body: JSON.stringify({ expectedHead: null, publication: crossHash }),
  }), configured(bucket, memoryNamespace()))).status, 400);
});

test("the PR capability uses authenticated checksum-bound Cargo multipart upload", async () => {
  const bucket = memoryBucket();
  const repository = memoryNamespace();
  const env = configured(bucket, repository);
  const endpoint = `https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/${cargoVendorSha256}/multipart`;
  const sha256 = [...new Uint8Array(await crypto.subtle.digest("SHA-256", cargoVendorBody))]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
  const createBody = {
    version: 1,
    requestId: "123e4567-e89b-42d3-a456-426614174001",
    size: cargoVendorBody.byteLength,
    sha256,
    partSize: 32 * 1024 * 1024,
    partCount: 1,
  };
  assert.equal((await route(new Request(endpoint, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(createBody),
  }), env)).status, 401);
  const { requestId: _legacyRequestId, ...legacyCreateBody } = createBody;
  assert.equal((await route(new Request(endpoint, {
    method: "POST",
    headers: prAuth({ "content-type": "application/json" }),
    body: JSON.stringify(legacyCreateBody),
  }), env)).status, 400, "the pre-idempotency create protocol has no fallback");

  repository.loseNextMultipartResponse();
  await assert.rejects(
    route(new Request(endpoint, {
      method: "POST",
      headers: prAuth({ "content-type": "application/json" }),
      body: JSON.stringify(createBody),
    }), env),
    /injected multipart response acknowledgement loss/,
  );
  const created = await route(new Request(endpoint, {
    method: "POST",
    headers: prAuth({ "content-type": "application/json" }),
    body: JSON.stringify(createBody),
  }), env);
  assert.equal(created.status, 200);
  const upload = await created.json() as {
    requestId: string;
    uploadId: string;
    stagingId: string;
  };
  assert.equal(upload.requestId, createBody.requestId);
  assert.equal(upload.stagingId, createBody.requestId);
  assert.equal(repository.multipartCreates, 1);
  const createConflict = await route(new Request(endpoint, {
    method: "POST",
    headers: prAuth({ "content-type": "application/json" }),
    body: JSON.stringify({ ...createBody, size: createBody.size + 1 }),
  }), env);
  assert.equal(createConflict.status, 409);
  assert.deepEqual(await createConflict.json(), {
    error: "cargo_vendor_multipart_request_conflict",
  });
  assert.equal(repository.multipartCreates, 1);
  const part = await route(new Request(`${endpoint}/parts/1`, {
    method: "PUT",
    headers: prAuth({
      "content-length": String(cargoVendorBody.byteLength),
      "content-type": "application/octet-stream",
      "x-nanocodex-staging-id": upload.stagingId,
      "x-nanocodex-upload-id": upload.uploadId,
      "x-nanocodex-sha256": sha256,
    }),
    body: byteBuffer(cargoVendorBody),
  }), env);
  assert.equal(part.status, 200, await part.clone().text());
  const descriptor = await part.json() as { partNumber: number; etag: string };
  assert.deepEqual(descriptor, {
    partNumber: 1,
    etag: "1".padStart(32, "0"),
    size: cargoVendorBody.byteLength,
    sha256,
  });
  const partReplay = await route(new Request(`${endpoint}/parts/1`, {
    method: "PUT",
    headers: prAuth({
      "content-length": String(cargoVendorBody.byteLength),
      "content-type": "application/octet-stream",
      "x-nanocodex-staging-id": upload.stagingId,
      "x-nanocodex-upload-id": upload.uploadId,
      "x-nanocodex-sha256": sha256,
    }),
    body: byteBuffer(cargoVendorBody),
  }), env);
  assert.equal(partReplay.status, 200);
  assert.deepEqual(await partReplay.json(), descriptor);
  const completionBody = {
    version: 1,
    uploadId: upload.uploadId,
    size: cargoVendorBody.byteLength,
    sha256,
    stagingId: upload.stagingId,
    parts: [{ partNumber: descriptor.partNumber, etag: descriptor.etag }],
  };
  bucket.returnIncompleteMultipartCompletionOnce();
  bucket.useCompositeMultipartChecksumOnce();
  const completed = await route(new Request(`${endpoint}/complete`, {
    method: "POST",
    headers: prAuth({ "content-type": "application/json" }),
    body: JSON.stringify(completionBody),
  }), env);
  assert.equal(completed.status, 200);
  assert.equal((await completed.json() as { uploaded: boolean }).uploaded, true);
  assert.equal(repository.multipartState(createBody.requestId), "complete");
  const completedReplay = await route(new Request(`${endpoint}/complete`, {
    method: "POST",
    headers: prAuth({ "content-type": "application/json" }),
    body: JSON.stringify(completionBody),
  }), env);
  assert.equal(completedReplay.status, 200);
  assert.equal((await completedReplay.json() as { uploaded: boolean }).uploaded, false);
  assert.equal((await bucket.list({ prefix: "cargo-vendor-staging/" })).objects.length, 0);

  const abortedSha = "e".repeat(64);
  const abortedEndpoint =
    `https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/${abortedSha}/multipart`;
  const abortCreated = await route(new Request(abortedEndpoint, {
    method: "POST",
    headers: prAuth({ "content-type": "application/json" }),
    body: JSON.stringify({
      ...createBody,
      requestId: "123e4567-e89b-42d3-a456-426614174002",
      sha256: abortedSha,
    }),
  }), env);
  const aborted = await abortCreated.json() as { uploadId: string; stagingId: string };
  const abortBody = JSON.stringify({
    version: 1,
    uploadId: aborted.uploadId,
    stagingId: aborted.stagingId,
  });
  for (let attempt = 0; attempt < 2; attempt += 1) {
    assert.equal((await route(new Request(abortedEndpoint, {
      method: "DELETE",
      headers: prAuth({ "content-type": "application/json" }),
      body: abortBody,
    }), env)).status, 204);
  }
  const recreatedAfterAbort = await route(new Request(abortedEndpoint, {
    method: "POST",
    headers: prAuth({ "content-type": "application/json" }),
    body: JSON.stringify({
      ...createBody,
      requestId: "123e4567-e89b-42d3-a456-426614174002",
      sha256: abortedSha,
    }),
  }), env);
  assert.equal(recreatedAfterAbort.status, 200);
  const freshAbortUpload = await recreatedAfterAbort.json() as {
    uploadId: string;
    stagingId: string;
  };
  assert.notEqual(freshAbortUpload.uploadId, aborted.uploadId);
  const freshPart = await route(new Request(`${abortedEndpoint}/parts/1`, {
    method: "PUT",
    headers: prAuth({
      "content-length": String(cargoVendorBody.byteLength),
      "content-type": "application/octet-stream",
      "x-nanocodex-staging-id": freshAbortUpload.stagingId,
      "x-nanocodex-upload-id": freshAbortUpload.uploadId,
      "x-nanocodex-sha256": sha256,
    }),
    body: byteBuffer(cargoVendorBody),
  }), env);
  const freshPartDescriptor = await freshPart.json() as { partNumber: number; etag: string };
  bucket.mismatchMultipartMetadataOnce();
  const stagedMismatch = await route(new Request(`${abortedEndpoint}/complete`, {
    method: "POST",
    headers: prAuth({ "content-type": "application/json" }),
    body: JSON.stringify({
      version: 1,
      uploadId: freshAbortUpload.uploadId,
      stagingId: freshAbortUpload.stagingId,
      size: cargoVendorBody.byteLength,
      sha256: abortedSha,
      parts: [{
        partNumber: freshPartDescriptor.partNumber,
        etag: freshPartDescriptor.etag,
      }],
    }),
  }), env);
  assert.equal(stagedMismatch.status, 409);
  assert.deepEqual(await stagedMismatch.json(), {
    error: "immutable_object_conflict",
    mismatch: {
      headPresent: true,
      keyMatches: true,
      sizeMatches: true,
      metadataPresent: true,
      metadataKeysMatch: true,
      kindMatches: false,
      canonicalKeyMatches: true,
      cargoLockBlobMatches: true,
      sha256MetadataMatches: true,
      sizeMetadataMatches: true,
      partSizeMatches: true,
      partCountMatches: true,
      requestIdMatches: true,
      checksumPresent: false,
    },
  });
  const recreatedAfterFailure = await route(new Request(abortedEndpoint, {
    method: "POST",
    headers: prAuth({ "content-type": "application/json" }),
    body: JSON.stringify({
      ...createBody,
      requestId: "123e4567-e89b-42d3-a456-426614174002",
      sha256: abortedSha,
    }),
  }), env);
  assert.equal(recreatedAfterFailure.status, 200);
  assert.notEqual(
    (await recreatedAfterFailure.json() as { uploadId: string }).uploadId,
    freshAbortUpload.uploadId,
  );
  const replay = await route(new Request(endpoint, {
    method: "POST",
    headers: prAuth({ "content-type": "application/json" }),
    body: JSON.stringify(createBody),
  }), env);
  assert.equal(replay.status, 200);
  assert.equal((await replay.json() as { uploaded: boolean }).uploaded, false);
  const downloaded = await route(new Request(
    `https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/${sha256}/bundle.tar.gz`,
  ), env);
  assert.equal(downloaded.headers.get("x-nanocodex-sha256"), sha256);
  assert.deepEqual(new Uint8Array(await downloaded.arrayBuffer()), cargoVendorBody);

  const wrongBundleSha = "f".repeat(64);
  const wrongEndpoint =
    `https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/${wrongBundleSha}/multipart`;
  const wrongCreated = await route(new Request(wrongEndpoint, {
    method: "POST",
    headers: prAuth({ "content-type": "application/json" }),
    body: JSON.stringify({
      ...createBody,
      requestId: "123e4567-e89b-42d3-a456-426614174003",
      sha256: wrongBundleSha,
    }),
  }), env);
  const wrongUpload = await wrongCreated.json() as { uploadId: string; stagingId: string };
  const wrongPart = await route(new Request(`${wrongEndpoint}/parts/1`, {
    method: "PUT",
    headers: prAuth({
      "content-length": String(cargoVendorBody.byteLength),
      "content-type": "application/octet-stream",
      "x-nanocodex-staging-id": wrongUpload.stagingId,
      "x-nanocodex-upload-id": wrongUpload.uploadId,
      "x-nanocodex-sha256": sha256,
    }),
    body: byteBuffer(cargoVendorBody),
  }), env);
  const wrongPartDescriptor = await wrongPart.json() as { partNumber: number; etag: string };
  const wrongComplete = await route(new Request(`${wrongEndpoint}/complete`, {
    method: "POST",
    headers: prAuth({ "content-type": "application/json" }),
    body: JSON.stringify({
      version: 1,
      uploadId: wrongUpload.uploadId,
      stagingId: wrongUpload.stagingId,
      size: cargoVendorBody.byteLength,
      sha256: wrongBundleSha,
      parts: [{
        partNumber: wrongPartDescriptor.partNumber,
        etag: wrongPartDescriptor.etag,
      }],
    }),
  }), env);
  assert.equal(wrongComplete.status, 409);
  assert.equal((await route(new Request(
    `https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/${wrongBundleSha}/bundle.tar.gz`,
  ), env)).status, 404);
  assert.equal((await bucket.list({ prefix: "cargo-vendor-staging/" })).objects.length, 0);
  const wrongRetry = await route(new Request(wrongEndpoint, {
    method: "POST",
    headers: prAuth({ "content-type": "application/json" }),
    body: JSON.stringify({
      ...createBody,
      requestId: "123e4567-e89b-42d3-a456-426614174003",
      sha256: wrongBundleSha,
    }),
  }), env);
  assert.equal(wrongRetry.status, 200);
  assert.notEqual(
    (await wrongRetry.json() as { uploadId: string }).uploadId,
    wrongUpload.uploadId,
  );
});

test("source publication rejects a missing or mismatched Cargo.lock bundle", async () => {
  const bucket = memoryBucket();
  const repository = memoryNamespace();
  const env = configured(bucket, repository);
  const treeSha = "2".repeat(64);
  const cargoVendorSha = cargoVendorSha256;
  const rustSecSha = "6".repeat(64);
  await route(uploadRequest("source.tar.gz", archiveBody, archiveSha), env);
  await route(uploadRequest("tree.json", treeBody, treeSha), env);
  await route(rustSecUploadRequest(rustSecSha), env);
  const publication = sourcePublication(archiveSha, treeSha, cargoVendorSha);
  const missing = await route(new Request("https://ci.test/api/ci/source/publish", {
    method: "PUT",
    headers: masterAuth({ "content-type": "application/json" }),
    body: JSON.stringify({ expectedHead: null, publication }),
  }), env);
  assert.equal(missing.status, 409);
  assert.deepEqual(await missing.json(), {
    error: "source_objects_invalid",
    invalid: [publication.cargoVendor.key],
  });

  await route(cargoVendorUpload(cargoVendorSha), env);
  const otherCargoLockBlob = "d".repeat(40);
  await route(cargoVendorUpload(cargoVendorSha, otherCargoLockBlob), env);
  const mismatch = {
    ...publication,
    cargoLockBlob: otherCargoLockBlob,
    cargoVendor: {
      ...publication.cargoVendor,
      key: `cargo-vendor/${otherCargoLockBlob}/${cargoVendorSha}/bundle.tar.gz`,
    },
  };
  const mismatched = await route(new Request("https://ci.test/api/ci/source/publish", {
    method: "PUT",
    headers: masterAuth({ "content-type": "application/json" }),
    body: JSON.stringify({ expectedHead: null, publication: mismatch }),
  }), env);
  assert.equal(mismatched.status, 409);
  assert.deepEqual(await mismatched.json(), { error: "cargo_lock_bundle_mismatch" });

  const { cargoVendor: _omitted, ...withoutCargoVendor } = publication;
  const invalidSchema = await route(new Request("https://ci.test/api/ci/source/publish", {
    method: "PUT",
    headers: masterAuth({ "content-type": "application/json" }),
    body: JSON.stringify({ expectedHead: null, publication: withoutCargoVendor }),
  }), env);
  assert.equal(invalidSchema.status, 400);
});

test("source publication acquires an exact token-free lease before reading R2", async () => {
  const publication = sourcePublication("1".repeat(64), "2".repeat(64), "4".repeat(64));
  const events: string[] = [];
  let mode: "valid" | "malformed" | "rejected" = "valid";
  const env = configured(memoryBucket(), memoryNamespace());
  env.CI_SOURCE = {
    async head(key: string) {
      events.push(`head:${key}`);
      return null;
    },
  } as unknown as R2Bucket;
  env.CI_REPOSITORY = forwardingNamespace(async (request) => {
    events.push(new URL(request.url).pathname);
    assert.equal(request.headers.get("authorization"), null);
    assert.deepEqual(await request.json(), { publication });
    if (mode === "rejected") {
      return Response.json({ error: "source_retiring" }, { status: 409 });
    }
    const lease = {
      version: 1,
      kind: "publication",
      leaseId: "123e4567-e89b-42d3-a456-426614174000",
      head,
      acquiredAt: new Date(Date.now() - 1_000).toISOString(),
      expiresAt: new Date(Date.now() + 4 * 60 * 1_000).toISOString(),
      ...(mode === "malformed" ? { unexpected: true } : {}),
    };
    return Response.json({ lease }, { status: 201 });
  });
  const publish = () => route(new Request("https://ci.test/api/ci/source/publish", {
    method: "PUT",
    headers: masterAuth({ "content-type": "application/json" }),
    body: JSON.stringify({ expectedHead: null, publication }),
  }), env);

  const missing = await publish();
  assert.equal(missing.status, 409);
  assert.equal(events[0], `/leases/publication/${head}`);
  assert.ok(events.slice(1).every((event) => event.startsWith("head:")));

  events.length = 0;
  mode = "malformed";
  const malformed = await publish();
  assert.equal(malformed.status, 503);
  assert.deepEqual(await malformed.json(), { error: "publication_lease_invalid" });
  assert.deepEqual(events, [`/leases/publication/${head}`]);

  events.length = 0;
  mode = "rejected";
  const rejected = await publish();
  assert.equal(rejected.status, 409);
  assert.deepEqual(await rejected.json(), { error: "source_retiring" });
  assert.deepEqual(events, [`/leases/publication/${head}`]);
});

test("published historical archives and Workflow status do not resolve through current head", async () => {
  const bucket = memoryBucket();
  const repository = memoryNamespace();
  const env = configured(bucket, repository);
  const treeSha = "2".repeat(64);
  const cargoVendorSha = "4".repeat(64);
  const rustSecSha = "6".repeat(64);
  await route(uploadRequest("source.tar.gz", archiveBody, archiveSha), env);
  await route(uploadRequest("tree.json", treeBody, treeSha), env);
  await route(cargoVendorUpload(cargoVendorSha), env);
  await route(rustSecUploadRequest(rustSecSha), env);
  repository.publication = sourcePublication(archiveSha, treeSha, cargoVendorSha);
  repository.run = {
    version: 1,
    head,
    beforeHead: null,
    workflowId: `ci-${head}`,
    state: "dispatched",
    attempts: 1,
    publishedAt: repository.publication.publishedAt,
    dispatchedAt: repository.publication.publishedAt,
  };
  await env.backup.put(`runs/${head}/result.json`, JSON.stringify({
    version: 1,
    head,
    workflowId: `ci-${head}`,
    status: "success",
    completedAt: "2026-08-22T01:00:00.000Z",
  }));
  await env.backup.put(`runs/${head}/progress.json`, JSON.stringify({
    version: 1,
    head,
    steps: [{ name: "quality", slug: "quality", status: "success" }],
  }));
  const artifactBody = new TextEncoder().encode("web deployment");
  const artifactSha = "4".repeat(64);
  await env.backup.put(`runs/${head}/artifacts/web-dist.tar`, artifactBody, {
    customMetadata: { head, kind: "web-dist", sha256: artifactSha },
  });
  await env.backup.put(`runs/${head}/artifacts/web-wasm.tar`, artifactBody, {
    customMetadata: { head, kind: "web-wasm", sha256: artifactSha },
  });
  await env.backup.put(`runs/${head}/artifacts/npm-package.tgz`, artifactBody, {
    customMetadata: { head, kind: "npm-package", sha256: artifactSha },
  });
  await env.backup.put(`artifacts/web-wasm/${artifactSha}.tar`, artifactBody, {
    customMetadata: { kind: "web-wasm", sha256: artifactSha },
  });
  await env.backup.put(`artifacts/npm-package/${artifactSha}.tgz`, artifactBody, {
    customMetadata: { kind: "npm-package", sha256: artifactSha },
  });
  const qualityLogKey = `runs/${head}/steps/quality/attempts/1/stdout.log`;
  await env.backup.put(qualityLogKey, "quality passed\n");
  await env.backup.put(
    `runs/${head}/steps/quality/result.json`,
    JSON.stringify({ stdout: { key: qualityLogKey } }),
  );

  const archive = await route(new Request(`https://ci.test/api/ci/source/${head}/archive`), env);
  assert.equal(archive.status, 200);
  assert.deepEqual(new Uint8Array(await archive.arrayBuffer()), archiveBody);
  assert.equal(archive.headers.get("x-nanocodex-sha256"), archiveSha);

  const status = await route(new Request(`https://ci.test/api/ci/runs/${head}`), env);
  assert.equal(status.status, 200);
  const statusBody = await status.json() as {
    workflow: { status: string };
    result: { status: string };
    progress: { steps: Array<{ slug: string; status: string }> };
  };
  assert.equal(statusBody.workflow.status, "running");
  assert.equal(statusBody.result.status, "success");
  assert.deepEqual(statusBody.progress.steps, [
    { name: "quality", slug: "quality", status: "success" },
  ]);
  const badge = await route(new Request("https://ci.test/api/ci/badge.svg"), env);
  assert.equal(badge.status, 200);
  assert.equal(badge.headers.get("content-type"), "image/svg+xml; charset=utf-8");
  assert.equal(badge.headers.get("x-content-type-options"), "nosniff");
  assert.match(badge.headers.get("cache-control") ?? "", /max-age=30/);
  const badgeBody = await badge.text();
  assert.match(badgeBody, /cloudflare ci: passing/);
  assert.match(badgeBody, /#f38020/);
  const badgeHead = await route(new Request("https://ci.test/api/ci/badge.svg", {
    method: "HEAD",
  }), env);
  assert.equal(badgeHead.status, 200);
  assert.equal(badgeHead.headers.get("content-length"), String(badgeBody.length));
  assert.equal(await badgeHead.text(), "");
  const artifact = await route(new Request(
    `https://ci.test/api/ci/runs/${head}/artifacts/web-dist.tar`,
  ), env);
  assert.equal(artifact.status, 200);
  assert.equal(artifact.headers.get("x-nanocodex-sha256"), artifactSha);
  assert.deepEqual(new Uint8Array(await artifact.arrayBuffer()), artifactBody);
  const wasmArtifact = await route(new Request(
    `https://ci.test/api/ci/runs/${head}/artifacts/web-wasm.tar`,
  ), env);
  assert.equal(wasmArtifact.status, 200);
  assert.equal(wasmArtifact.headers.get("x-nanocodex-sha256"), artifactSha);
  const wasmArtifactHead = await route(new Request(
    `https://ci.test/api/ci/runs/${head}/artifacts/web-wasm.tar`,
    { method: "HEAD" },
  ), env);
  assert.equal(wasmArtifactHead.status, 200);
  assert.equal(wasmArtifactHead.headers.get("content-length"), String(artifactBody.length));
  const contentArtifact = await route(new Request(
    `https://ci.test/api/ci/artifacts/web-wasm/${artifactSha}.tar`,
  ), env);
  assert.equal(contentArtifact.status, 200);
  assert.equal(contentArtifact.headers.get("x-nanocodex-sha256"), artifactSha);
  assert.deepEqual(new Uint8Array(await contentArtifact.arrayBuffer()), artifactBody);
  const contentArtifactHead = await route(new Request(
    `https://ci.test/api/ci/artifacts/web-wasm/${artifactSha}.tar`,
    { method: "HEAD" },
  ), env);
  assert.equal(contentArtifactHead.status, 200);
  assert.equal(contentArtifactHead.headers.get("content-length"), String(artifactBody.length));
  const npmArtifact = await route(new Request(
    `https://ci.test/api/ci/runs/${head}/artifacts/npm-package.tgz`,
  ), env);
  assert.equal(npmArtifact.status, 200);
  assert.equal(npmArtifact.headers.get("content-type"), "application/gzip");
  assert.equal(npmArtifact.headers.get("x-nanocodex-sha256"), artifactSha);
  assert.deepEqual(new Uint8Array(await npmArtifact.arrayBuffer()), artifactBody);
  const contentNpmArtifact = await route(new Request(
    `https://ci.test/api/ci/artifacts/npm-package/${artifactSha}.tgz`,
  ), env);
  assert.equal(contentNpmArtifact.status, 200);
  assert.equal(contentNpmArtifact.headers.get("content-type"), "application/gzip");
  assert.equal(contentNpmArtifact.headers.get("x-nanocodex-sha256"), artifactSha);
  assert.equal((await route(new Request(
    `https://ci.test/api/ci/artifacts/npm-package/${artifactSha}.tar`,
  ), env)).status, 404);
  const log = await route(new Request(
    `https://ci.test/api/ci/runs/${head}/steps/quality/stdout.log`,
  ), env);
  assert.equal(log.status, 200);
  assert.equal(await log.text(), "quality passed\n");
  const terminated = await route(new Request(`https://ci.test/api/ci/runs/${head}/terminate`, {
    method: "POST",
    headers: controlAuth(),
  }), env);
  assert.equal(terminated.status, 202);
  assert.deepEqual(await terminated.json(), {
    status: "accepted",
    head,
    workflowId: `ci-${head}`,
    reason: "operator_terminated",
  });
  assert.equal(env.controls.terminations, 0);
});

test("run status and badge fail closed for malformed or mismatched success evidence", async () => {
  const bucket = memoryBucket();
  const repository = memoryNamespace();
  const env = configured(bucket, repository);
  repository.publication = sourcePublication("1".repeat(64), "2".repeat(64), "4".repeat(64));
  repository.run = {
    version: 1,
    head,
    beforeHead: null,
    workflowId: `ci-${head}`,
    state: "dispatched",
    attempts: 1,
    publishedAt: repository.publication.publishedAt,
    dispatchedAt: repository.publication.publishedAt,
  };
  env.controls.status = "unknown";
  const valid = {
    version: 1,
    head,
    workflowId: `ci-${head}`,
    status: "success",
    completedAt: "2026-08-22T01:00:00.000Z",
  };
  const malformed = [
    { status: "success" },
    { ...valid, head: "b".repeat(40) },
    { ...valid, workflowId: `ci-${"b".repeat(40)}` },
    { ...valid, completedAt: "invalid" },
  ];
  for (const value of malformed) {
    await env.backup.put(`runs/${head}/result.json`, JSON.stringify(value));
    const status = await route(new Request(`https://ci.test/api/ci/runs/${head}`), env);
    assert.equal(status.status, 200);
    const detail = await status.json() as {
      workflow: { status: string };
      result: unknown;
    };
    assert.equal(detail.workflow.status, "unknown");
    assert.deepEqual(detail.result, { error: "invalid_result" });
    const badge = await route(new Request("https://ci.test/api/ci/badge.svg"), env);
    assert.doesNotMatch(await badge.text(), /cloudflare ci: passing/);
  }

  await env.backup.put(`runs/${head}/result.json`, JSON.stringify(valid));
  const status = await route(new Request(`https://ci.test/api/ci/runs/${head}`), env);
  const detail = await status.json() as {
    workflow: { status: string; evidence?: string };
    result: { status: string };
  };
  assert.deepEqual(detail.workflow, { status: "complete", evidence: "retained_result" });
  assert.equal(detail.result.status, "success");
  const badge = await route(new Request("https://ci.test/api/ci/badge.svg"), env);
  assert.match(await badge.text(), /cloudflare ci: passing/);
});

test("terminated Workflows reconcile stale running evidence at the read boundary", async () => {
  const bucket = memoryBucket();
  const repository = memoryNamespace();
  const env = configured(bucket, repository);
  repository.run = {
    version: 1,
    head,
    beforeHead: null,
    workflowId: `ci-${head}`,
    state: "dispatched",
    attempts: 1,
    publishedAt: "2026-08-21T00:00:00.000Z",
  };
  env.controls.status = "terminated";
  await env.backup.put(`runs/${head}/result.json`, JSON.stringify({
    version: 1,
    head,
    workflowId: `ci-${head}`,
    status: "running",
    steps: [],
  }));
  await env.backup.put(`runs/${head}/progress.json`, JSON.stringify({
    version: 1,
    head,
    steps: [
      { name: "Cargo dependencies", slug: "cargo-dependencies", status: "success" },
      { name: "MSRV workspace tests", slug: "msrv-workspace-tests", status: "running" },
      { name: "website", slug: "website", status: "pending" },
    ],
  }));

  const response = await route(new Request(`https://ci.test/api/ci/runs/${head}`), env);
  assert.equal(response.status, 200);
  const detail = await response.json() as {
    result: { status: string };
    progress: { steps: Array<{ status: string; message?: string }> };
  };
  assert.equal(detail.result.status, "terminated");
  assert.deepEqual(detail.progress.steps, [
    { name: "Cargo dependencies", slug: "cargo-dependencies", status: "success" },
    {
      name: "MSRV workspace tests",
      slug: "msrv-workspace-tests",
      status: "terminated",
      message: "terminated by operator",
    },
    { name: "website", slug: "website", status: "pending" },
  ]);
});

test("operator termination delegates exact token-free cancellation to the repository", async () => {
  const bucket = memoryBucket();
  const env = configured(bucket, memoryNamespace());
  const calls: Array<{ path: string; authorization: string | null; body: unknown }> = [];
  let mode: "accepted" | "complete" | "conflict" = "accepted";
  env.CI_REPOSITORY = forwardingNamespace(async (request) => {
    calls.push({
      path: new URL(request.url).pathname,
      authorization: request.headers.get("authorization"),
      body: await request.json(),
    });
    if (mode === "conflict") {
      return Response.json({ error: "source_retiring" }, { status: 409 });
    }
    const proof = {
      status: mode === "complete" ? "complete" : "accepted",
      head,
      workflowId: `ci-${head}`,
      reason: "operator_terminated",
      ...(mode === "complete"
        ? {
          claimId: "123e4567-e89b-42d3-a456-426614174000",
          completedAt: "2026-08-22T02:00:00.000Z",
        }
        : {}),
    };
    return Response.json(proof, { status: mode === "complete" ? 200 : 202 });
  });

  const terminate = () => route(new Request(
    `https://ci.test/api/ci/runs/${head}/terminate`,
    { method: "POST", headers: controlAuth({ "x-operator-secret": "must-not-forward" }) },
  ), env);
  const accepted = await terminate();
  assert.equal(accepted.status, 202);
  assert.deepEqual(await accepted.json(), {
    status: "accepted",
    head,
    workflowId: `ci-${head}`,
    reason: "operator_terminated",
  });
  mode = "complete";
  const complete = await terminate();
  assert.equal(complete.status, 200);
  assert.equal((await complete.json() as { status: string }).status, "complete");
  mode = "conflict";
  const conflict = await terminate();
  assert.equal(conflict.status, 409);
  assert.deepEqual(await conflict.json(), { error: "source_retiring" });

  assert.deepEqual(calls, Array.from({ length: 3 }, () => ({
    path: `/cancellations/${head}`,
    authorization: null,
    body: { workflowId: `ci-${head}` },
  })));
  assert.equal(env.controls.terminations, 0);
  assert.deepEqual(env.controls.events, []);
  assert.equal(await env.backup.head(`runs/${head}/control/terminated.json`), null);
});

function route(request: Request, env: CiStorageEnv): Promise<Response> {
  return routeCiRequest(request, env, new URL(request.url)) as Promise<Response>;
}

function uploadRequest(
  file: string,
  body: Uint8Array,
  sha256: string,
  capability: "master" | "pull_request" = "master",
): Request {
  const authorization = capability === "master" ? masterAuth : prAuth;
  return new Request(`https://ci.test/api/ci/source/objects/${head}/${file}`, {
    method: "PUT",
    headers: authorization({
      "content-length": String(body.byteLength),
      "x-nanocodex-sha256": sha256,
    }),
    body: byteBuffer(body),
  });
}

function cargoVendorUpload(
  sha256: string,
  lockBlob = cargoLockBlob,
  capability: "master" | "pull_request" = "master",
  body = cargoVendorBody,
): Request {
  const authorization = capability === "master" ? masterAuth : prAuth;
  return new Request(
    `https://ci.test/api/ci/cargo-vendor/${lockBlob}/${sha256}/bundle.tar.gz`,
    {
      method: "PUT",
      headers: authorization({
        "content-length": String(body.byteLength),
        "content-type": "application/gzip",
        "x-nanocodex-sha256": sha256,
      }),
      body: byteBuffer(body),
    },
  );
}

function rustSecUploadRequest(sha256: string): Request {
  return new Request(
    `https://ci.test/api/ci/rustsec-advisory-db/${rustSecRevision}/bundle.tar.gz`,
    {
      method: "PUT",
      headers: masterAuth({
        "content-length": String(rustSecBody.byteLength),
        "content-type": "application/gzip",
        "x-nanocodex-sha256": sha256,
      }),
      body: byteBuffer(rustSecBody),
    },
  );
}

function masterAuth(extra: HeadersInit = {}): Headers {
  const headers = new Headers(extra);
  headers.set("authorization", "Bearer master-source-token");
  return headers;
}

function prAuth(extra: HeadersInit = {}): Headers {
  const headers = new Headers(extra);
  headers.set("authorization", "Bearer pr-source-token");
  return headers;
}

function controlAuth(extra: HeadersInit = {}): Headers {
  const headers = new Headers(extra);
  headers.set("authorization", "Bearer control-token");
  return headers;
}

function configured(bucket: ReturnType<typeof memoryBucket>, repository: ReturnType<typeof memoryNamespace>) {
  repository.bindBucket(bucket);
  const backup = memoryBucket();
  const controls = { terminations: 0, status: "running", events: [] as string[] };
  const sandbox = memorySandboxNamespace(controls);
  return {
    ENVIRONMENT: "development",
    CI_SOURCE: bucket as unknown as R2Bucket,
    BACKUP_BUCKET: backup as unknown as R2Bucket,
    CI_REPOSITORY: repository.namespace as unknown as DurableObjectNamespace,
    CI_WORKFLOW: {
      async get(id: string) {
        assert.equal(id, `ci-${head}`);
        return {
          id,
          status: async () => ({ status: controls.status }),
          terminate: async () => {
            assert.ok(await backup.head(`runs/${head}/control/terminated.json`));
            controls.terminations += 1;
            controls.status = "terminated";
            controls.events.push("workflow");
          },
        };
      },
    } as unknown as Workflow,
    SANDBOX: sandbox.namespace as unknown as DurableObjectNamespace<
      import("@cloudflare/ci/worker").CiSandbox
    >,
    CI_MASTER_SOURCE_WRITE_TOKEN: "master-source-token",
    CI_PR_SOURCE_WRITE_TOKEN: "pr-source-token",
    CI_CONTROL_TOKEN: "control-token",
    CI_RELEASE_TOKEN: "release-token",
    backup,
    controls,
    sandbox,
  };
}

function memorySandboxNamespace(controls: { terminations: number; events: string[] }) {
  const destroyed: string[] = [];
  const failures = new Map<string, number>();
  const attempts = new Map<string, number>();
  return {
    namespace: {
      idFromName(name: string) { return { name }; },
      get(id: { name: string }) {
        return {
          async destroy() {
            assert.equal(controls.terminations, 1);
            attempts.set(id.name, (attempts.get(id.name) ?? 0) + 1);
            const remaining = failures.get(id.name) ?? 0;
            if (remaining > 0) {
              failures.set(id.name, remaining - 1);
              throw new Error("injected Sandbox teardown failure");
            }
            destroyed.push(id.name);
            controls.events.push(id.name);
          },
        };
      },
    },
    destroyed,
    failures,
    attempts,
  };
}

function memoryBucket() {
  const objects = new Map<string, {
    body: Uint8Array;
    customMetadata?: Record<string, string>;
    httpMetadata?: R2HTTPMetadata;
    nativeSha256?: string;
  }>();
  const multipart = new Map<string, {
    key: string;
    options: R2MultipartOptions;
    parts: Map<number, Uint8Array>;
  }>();
  let uploadSequence = 0;
  let incompleteMultipartCompletion = false;
  let compositeMultipartChecksum = false;
  let mismatchedMultipartMetadata = false;
  let cargoVendorPutFault:
    | "commit_then_throw"
    | "mismatch_then_throw"
    | "transient"
    | undefined;
  const object = (key: string, value: NonNullable<ReturnType<typeof objects.get>>) => {
    const checksum = value.nativeSha256;
    return {
      key,
      size: value.body.byteLength,
      etag: "etag",
      httpEtag: '"etag"',
      uploaded: new Date(),
      customMetadata: value.customMetadata,
      httpMetadata: value.httpMetadata,
      checksums: {
        ...(checksum ? { sha256: hexBuffer(checksum) } : {}),
        toJSON: () => checksum ? { sha256: checksum } : {},
      },
      writeHttpMetadata(headers: Headers) {
        if (value.httpMetadata?.contentType) {
          headers.set("content-type", value.httpMetadata.contentType);
        }
        if (value.httpMetadata?.cacheControl) {
          headers.set("cache-control", value.httpMetadata.cacheControl);
        }
      },
    };
  };
  return {
    returnIncompleteMultipartCompletionOnce() {
      incompleteMultipartCompletion = true;
    },
    useCompositeMultipartChecksumOnce() {
      compositeMultipartChecksum = true;
    },
    mismatchMultipartMetadataOnce() {
      mismatchedMultipartMetadata = true;
    },
    failNextCargoVendorPut(
      fault: "commit_then_throw" | "mismatch_then_throw" | "transient",
    ) {
      cargoVendorPutFault = fault;
    },
    async head(key: string) {
      const value = objects.get(key);
      return value ? object(key, value) : null;
    },
    async put(key: string, body: ReadableStream | string | Uint8Array, options: R2PutOptions = {}) {
      if (objects.has(key) && options.onlyIf) return null;
      const bytes = typeof body === "string" ? new TextEncoder().encode(body)
        : body instanceof Uint8Array ? body
        : new Uint8Array(await new Response(body).arrayBuffer());
      const digest = createHash("sha256").update(bytes).digest("hex");
      const cargoVendor = options.customMetadata?.kind === "cargo-vendor";
      if (cargoVendor && typeof options.sha256 === "string" && options.sha256 !== digest) {
        throw r2Error(10037, "put: Provided checksum does not match uploaded content. (10037)");
      }
      if (cargoVendor && cargoVendorPutFault === "transient") {
        cargoVendorPutFault = undefined;
        throw new Error("injected transient R2 failure");
      }
      const nativeSha256 = cargoVendor
        ? digest
        : options.customMetadata?.kind === "native-cli" ||
            options.customMetadata?.kind === "npm-preview"
        ? digest
        : typeof options.sha256 === "string"
        ? options.sha256
        : options.customMetadata?.sha256;
      let value = {
        body: bytes,
        customMetadata: options.customMetadata,
        httpMetadata: options.httpMetadata as R2HTTPMetadata | undefined,
        nativeSha256,
      };
      if (cargoVendor && cargoVendorPutFault === "mismatch_then_throw") {
        value = {
          ...value,
          customMetadata: { ...value.customMetadata, kind: "tampered" },
        };
      }
      objects.set(key, value);
      if (
        cargoVendor &&
        (cargoVendorPutFault === "commit_then_throw" ||
          cargoVendorPutFault === "mismatch_then_throw")
      ) {
        cargoVendorPutFault = undefined;
        throw new Error("injected R2 acknowledgement loss");
      }
      return object(key, value);
    },
    async createMultipartUpload(key: string, options: R2MultipartOptions = {}) {
      const uploadId = `upload-${++uploadSequence}`;
      multipart.set(uploadId, { key, options, parts: new Map() });
      return { key, uploadId };
    },
    resumeMultipartUpload(key: string, uploadId: string) {
      return {
        key,
        uploadId,
        async uploadPart(partNumber: number, body: ReadableStream | Uint8Array) {
          const upload = multipart.get(uploadId);
          if (!upload || upload.key !== key) throw new Error("missing multipart upload");
          const bytes = body instanceof Uint8Array
            ? body
            : new Uint8Array(await new Response(body).arrayBuffer());
          upload.parts.set(partNumber, bytes);
          return { partNumber, etag: partNumber.toString(16).padStart(32, "0") };
        },
        async complete(parts: Array<{ partNumber: number; etag: string }>) {
          const upload = multipart.get(uploadId);
          if (!upload || upload.key !== key) throw new Error("missing multipart upload");
          const body = concatBytes(parts.map(({ partNumber }) => {
            const part = upload.parts.get(partNumber);
            if (!part) throw new Error("missing multipart part");
            return part;
          }));
          const metadata = mismatchedMultipartMetadata
            ? { ...upload.options.customMetadata, kind: "unexpected" }
            : upload.options.customMetadata;
          const value = {
            body,
            customMetadata: metadata,
            httpMetadata: upload.options.httpMetadata as R2HTTPMetadata | undefined,
            nativeSha256: compositeMultipartChecksum ? "f".repeat(64) : undefined,
          };
          mismatchedMultipartMetadata = false;
          compositeMultipartChecksum = false;
          objects.set(key, value);
          multipart.delete(uploadId);
          const completed = object(key, value);
          if (!incompleteMultipartCompletion) return completed;
          incompleteMultipartCompletion = false;
          return { ...completed, customMetadata: undefined };
        },
        async abort() {
          if (!multipart.delete(uploadId)) {
            throw r2Error(10024, "NoSuchUpload", "abortMultipartUpload");
          }
        },
      };
    },
    async get(key: string, options: { range?: { offset: number; length: number } } = {}) {
      const value = objects.get(key);
      if (!value) return null;
      const range = options.range;
      const body = range
        ? value.body.slice(range.offset, range.offset + range.length)
        : value.body;
      return {
        ...object(key, value),
        body: new Response(byteBuffer(body)).body,
        bodyUsed: false,
        arrayBuffer: async () => body.buffer.slice(
          body.byteOffset,
          body.byteOffset + body.byteLength,
        ),
        text: async () => new TextDecoder().decode(body),
        json: async () => JSON.parse(new TextDecoder().decode(body)),
        blob: async () => new Blob([byteBuffer(body)]),
      };
    },
    async list(options: R2ListOptions = {}) {
      const keys = [...objects.keys()]
        .filter((key) => !options.prefix || key.startsWith(options.prefix))
        .sort();
      return {
        objects: keys.map((key) => object(key, objects.get(key)!)),
        truncated: false as const,
        delimitedPrefixes: [],
      };
    },
    async delete(keys: string | string[]) {
      for (const key of typeof keys === "string" ? [keys] : keys) objects.delete(key);
    },
  };
}

function concatBytes(parts: Uint8Array[]): Uint8Array {
  const combined = new Uint8Array(parts.reduce((size, part) => size + part.byteLength, 0));
  let offset = 0;
  for (const part of parts) {
    combined.set(part, offset);
    offset += part.byteLength;
  }
  return combined;
}

function r2Error(
  code: number,
  message: string,
  action = "put",
): Error & { code: number; action: string } {
  return Object.assign(new Error(message), { name: "R2Error", code, action });
}

function memoryNamespace() {
  type MultipartRequest = {
    version: 1;
    requestId: string;
    cargoLockBlob: string;
    bundleSha256: string;
    size: number;
    partSize: number;
    partCount: number;
  };
  const state: {
    publication?: CiSourcePublication;
    run?: Record<string, unknown>;
    published?: unknown;
    lastLeaseId?: string;
    leaseSequence: number;
    leases: Map<string, string>;
    multipart: Map<string, {
      input: MultipartRequest;
      response: Record<string, unknown>;
      status: "ready" | "complete";
    }>;
    multipartCreates: number;
    loseMultipartResponse: boolean;
  } = {
    leaseSequence: 0,
    leases: new Map(),
    multipart: new Map(),
    multipartCreates: 0,
    loseMultipartResponse: false,
  };
  let sourceBucket: ReturnType<typeof memoryBucket> | undefined;
  const stub = {
    async fetch(input: string | URL | Request, init?: RequestInit) {
      const request = new Request(input, init);
      const path = new URL(request.url).pathname;
      const multipart = path.match(
        /^\/cargo-vendor\/multipart\/([a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12})$/,
      );
      if (multipart && request.method === "POST") {
        assert.ok(sourceBucket, "configured() must bind CI_SOURCE to the repository fixture");
        const value = await request.json() as MultipartRequest;
        assert.equal(value.requestId, multipart[1]);
        const previous = state.multipart.get(value.requestId);
        if (previous && JSON.stringify(previous.input) !== JSON.stringify(value)) {
          return Response.json({ error: "cargo_vendor_multipart_request_conflict" }, {
            status: 409,
          });
        }
        const key = `cargo-vendor/${value.cargoLockBlob}/${value.bundleSha256}/bundle.tar.gz`;
        if (await sourceBucket.head(key)) {
          return Response.json({
            requestId: value.requestId,
            key,
            cargoLockBlob: value.cargoLockBlob,
            size: value.size,
            sha256: value.bundleSha256,
            uploaded: false,
          });
        }
        if (previous) return Response.json(previous.response);
        const stagingId = value.requestId;
        const stagingKey = `cargo-vendor-staging/${value.cargoLockBlob}/${value.bundleSha256}/${stagingId}/bundle.tar.gz`;
        const upload = await sourceBucket.createMultipartUpload(stagingKey, {
          httpMetadata: { contentType: "application/gzip", cacheControl: "immutable" },
          customMetadata: {
            sha256: value.bundleSha256,
            size: String(value.size),
            kind: "cargo-vendor-staging",
            cargoLockBlob: value.cargoLockBlob,
            canonicalKey: key,
            partSize: String(value.partSize),
            partCount: String(value.partCount),
            requestId: value.requestId,
          },
        });
        state.multipartCreates += 1;
        const response = {
          requestId: value.requestId,
          key,
          cargoLockBlob: value.cargoLockBlob,
          size: value.size,
          sha256: value.bundleSha256,
          uploadId: upload.uploadId,
          stagingId,
          partSize: value.partSize,
          partCount: value.partCount,
        };
        state.multipart.set(value.requestId, {
          input: structuredClone(value),
          response,
          status: "ready",
        });
        if (state.loseMultipartResponse) {
          state.loseMultipartResponse = false;
          throw new Error("injected multipart response acknowledgement loss");
        }
        return Response.json(response);
      }
      const multipartTransition = path.match(
        /^\/cargo-vendor\/multipart\/([a-f0-9-]+)\/(finalize|reset)$/,
      );
      if (multipartTransition && request.method === "POST") {
        const requestId = multipartTransition[1]!;
        const operation = multipartTransition[2]!;
        const identity = await request.json() as {
          version: number;
          requestId: string;
          stagingId: string;
          uploadId: string;
          cargoLockBlob: string;
          bundleSha256: string;
        };
        const current = state.multipart.get(requestId);
        if (!current && operation === "reset") return new Response(null, { status: 204 });
        if (!current) return Response.json({ error: "cargo_vendor_multipart_not_found" }, {
          status: 404,
        });
        if (
          identity.version !== 1 || identity.requestId !== requestId ||
          identity.stagingId !== requestId ||
          identity.uploadId !== current.response.uploadId ||
          identity.cargoLockBlob !== current.input.cargoLockBlob ||
          identity.bundleSha256 !== current.input.bundleSha256
        ) {
          return Response.json({ error: "cargo_vendor_multipart_identity_conflict" }, {
            status: 409,
          });
        }
        if (operation === "reset") {
          if (current.status !== "ready") {
            return Response.json({ error: "cargo_vendor_multipart_identity_conflict" }, {
              status: 409,
            });
          }
          state.multipart.delete(requestId);
        } else {
          current.status = "complete";
        }
        return new Response(null, { status: 204 });
      }
      const publicationLease = path.match(/^\/leases\/publication\/([a-f0-9]{40})$/);
      if (publicationLease && request.method === "POST") {
        const value = await request.json() as { publication: CiSourcePublication };
        assert.deepEqual(Object.keys(value), ["publication"]);
        assert.equal(value.publication.head, publicationLease[1]);
        const leaseId = `123e4567-e89b-42d3-a456-${String(++state.leaseSequence).padStart(12, "0")}`;
        const acquiredAt = new Date(Date.now() - 1_000).toISOString();
        const expiresAt = new Date(Date.now() + 4 * 60 * 1_000).toISOString();
        state.lastLeaseId = leaseId;
        state.leases.set(publicationLease[1]!, leaseId);
        return Response.json({
          lease: {
            version: 1,
            kind: "publication",
            leaseId,
            head: publicationLease[1],
            acquiredAt,
            expiresAt,
          },
        }, { status: 201 });
      }
      if (path === "/publications" && request.method === "PUT") {
        state.published = await request.json();
        const publication = (state.published as { publication: CiSourcePublication }).publication;
        if (
          (state.published as { leaseId?: unknown }).leaseId !==
            state.leases.get(publication.head)
        ) return Response.json({ error: "publication_lease_invalid" }, { status: 409 });
        state.leases.delete(publication.head);
        state.publication = publication;
        return Response.json({ publication: state.publication }, { status: 202 });
      }
      const cancellation = path.match(/^\/cancellations\/([a-f0-9]{40})$/);
      if (cancellation && request.method === "POST") {
        const value = await request.json() as { workflowId?: unknown };
        if (
          value.workflowId !== `ci-${cancellation[1]}` ||
          state.run?.head !== cancellation[1]
        ) return Response.json({ error: "not_published" }, { status: 404 });
        return Response.json({
          status: "accepted",
          head: cancellation[1],
          workflowId: value.workflowId,
          reason: "operator_terminated",
        }, { status: 202 });
      }
      if (path === `/publications/${head}` && state.publication) {
        return Response.json(state.publication);
      }
      if (path === "/state" && state.publication && state.run) {
        return Response.json({ publication: state.publication, run: state.run });
      }
      if (path === `/runs/${head}` && state.run) return Response.json(state.run);
      return Response.json({ error: "not_found" }, { status: 404 });
    },
  };
  return {
    namespace: {
      idFromName(name: string) { return { name }; },
      get() { return stub; },
    },
    get publication() { return state.publication; },
    set publication(value) { state.publication = value; },
    get run() { return state.run; },
    set run(value) { state.run = value; },
    get published() { return state.published; },
    get lastLeaseId() { return state.lastLeaseId; },
    get multipartCreates() { return state.multipartCreates; },
    multipartState(requestId: string) { return state.multipart.get(requestId)?.status; },
    bindBucket(bucket: ReturnType<typeof memoryBucket>) { sourceBucket = bucket; },
    loseNextMultipartResponse() { state.loseMultipartResponse = true; },
  };
}

function forwardingNamespace(
  fetch: (request: Request) => Promise<Response>,
): DurableObjectNamespace {
  return {
    idFromName() { return { toString: () => "nanocodex" }; },
    get() {
      return {
        fetch(input: RequestInfo | URL, init?: RequestInit) {
          return fetch(new Request(input, init));
        },
      };
    },
  } as unknown as DurableObjectNamespace;
}

function sourcePublication(
  archiveSha: string,
  treeSha: string,
  cargoVendorSha: string,
): CiSourcePublication {
  return {
    version: 1,
    head,
    branch: "master",
    ref: "refs/heads/master",
    archive: { key: `sources/${head}/source.tar.gz`, size: archiveBody.byteLength, sha256: archiveSha },
    tree: { key: `sources/${head}/tree.json`, size: treeBody.byteLength, sha256: treeSha },
    cargoLockBlob,
    cargoVendor: {
      key: `cargo-vendor/${cargoLockBlob}/${cargoVendorSha}/bundle.tar.gz`,
      size: cargoVendorBody.byteLength,
      sha256: cargoVendorSha,
    },
    rustSecRevision,
    rustSec: {
      key: `rustsec-advisory-db/${rustSecRevision}/bundle.tar.gz`,
      size: rustSecBody.byteLength,
      sha256: "6".repeat(64),
    },
    publishedAt: "2026-08-21T00:00:00.000Z",
  };
}

function byteBuffer(bytes: Uint8Array): ArrayBuffer {
  const copy = new Uint8Array(bytes.byteLength);
  copy.set(bytes);
  return copy.buffer;
}

function hexBuffer(value: string): ArrayBuffer {
  const bytes = new Uint8Array(value.length / 2);
  for (let index = 0; index < bytes.length; index++) {
    bytes[index] = Number.parseInt(value.slice(index * 2, index * 2 + 2), 16);
  }
  return bytes.buffer;
}
