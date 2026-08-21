import assert from "node:assert/strict";
import { test } from "node:test";

import { routeCiRequest, type CiStorageEnv } from "./ciRoutes.ts";
import type { CiSourcePublication } from "./ciSource.ts";

const head = "a".repeat(40);
const cargoLockBlob = "c".repeat(40);
const rustSecRevision = "d".repeat(40);
const archiveBody = new TextEncoder().encode("archive");
const archiveSha = "1".repeat(64);
const cargoVendorBody = new TextEncoder().encode("cargo vendor bundle");
const rustSecBody = new TextEncoder().encode("RustSec advisory database bundle");
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
    `https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/bundle.tar.gz`,
    { method: "PUT", body: cargoVendorBody },
  ), {});
  assert.equal(cargo.status, 401);
  const rustSec = await route(new Request(
    `https://ci.test/api/ci/rustsec-advisory-db/${rustSecRevision}/bundle.tar.gz`,
    { method: "PUT", body: rustSecBody },
  ), {});
  assert.equal(rustSec.status, 401);
});

test("source objects are immutable and publication verifies both R2 objects", async () => {
  const bucket = memoryBucket();
  const repository = memoryNamespace();
  const env = configured(bucket, repository);
  const treeSha = "2".repeat(64);
  const cargoVendorSha = "4".repeat(64);
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
  assert.equal((await route(cargoVendorUpload("5".repeat(64)), env)).status, 409);
  const cargoHead = await route(new Request(
    `https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/bundle.tar.gz`,
    { method: "HEAD", headers: auth() },
  ), env);
  assert.equal(cargoHead.status, 200);
  assert.equal(cargoHead.headers.get("x-nanocodex-sha256"), cargoVendorSha);
  assert.equal(cargoHead.headers.get("x-nanocodex-cargo-lock"), cargoLockBlob);
  const cargoDownload = await route(new Request(
    `https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/bundle.tar.gz`,
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
    headers: auth({ "content-type": "application/json" }),
    body: JSON.stringify({ expectedHead: null, publication }),
  }), env);
  assert.equal(published.status, 202);
  assert.deepEqual(repository.published, { expectedHead: null, publication });
});

test("source publication rejects a missing or mismatched Cargo.lock bundle", async () => {
  const bucket = memoryBucket();
  const repository = memoryNamespace();
  const env = configured(bucket, repository);
  const treeSha = "2".repeat(64);
  const cargoVendorSha = "4".repeat(64);
  const rustSecSha = "6".repeat(64);
  await route(uploadRequest("source.tar.gz", archiveBody, archiveSha), env);
  await route(uploadRequest("tree.json", treeBody, treeSha), env);
  await route(rustSecUploadRequest(rustSecSha), env);
  const publication = sourcePublication(archiveSha, treeSha, cargoVendorSha);
  const missing = await route(new Request("https://ci.test/api/ci/source/publish", {
    method: "PUT",
    headers: auth({ "content-type": "application/json" }),
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
      key: `cargo-vendor/${otherCargoLockBlob}/bundle.tar.gz`,
    },
  };
  const mismatched = await route(new Request("https://ci.test/api/ci/source/publish", {
    method: "PUT",
    headers: auth({ "content-type": "application/json" }),
    body: JSON.stringify({ expectedHead: null, publication: mismatch }),
  }), env);
  assert.equal(mismatched.status, 409);
  assert.deepEqual(await mismatched.json(), { error: "cargo_lock_bundle_mismatch" });

  const { cargoVendor: _omitted, ...withoutCargoVendor } = publication;
  const invalidSchema = await route(new Request("https://ci.test/api/ci/source/publish", {
    method: "PUT",
    headers: auth({ "content-type": "application/json" }),
    body: JSON.stringify({ expectedHead: null, publication: withoutCargoVendor }),
  }), env);
  assert.equal(invalidSchema.status, 400);
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
  await env.backup.put(`runs/${head}/result.json`, JSON.stringify({ status: "success" }));
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
  const log = await route(new Request(
    `https://ci.test/api/ci/runs/${head}/steps/quality/stdout.log`,
  ), env);
  assert.equal(log.status, 200);
  assert.equal(await log.text(), "quality passed\n");
  const terminated = await route(new Request(`https://ci.test/api/ci/runs/${head}/terminate`, {
    method: "POST",
    headers: controlAuth(),
  }), env);
  assert.equal(terminated.status, 200);
  assert.equal(env.controls.terminations, 1);
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

test("termination tombstones the run and destroys every registered Sandbox", async () => {
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
  const active = [
    "quality-11111111-1111-4111-8111-111111111111",
    "python-3-11-22222222-2222-4222-8222-222222222222",
  ];
  for (const runnerId of active) {
    await env.backup.put(`runs/${head}/sandboxes/${runnerId}.json`, "{}");
  }
  const unrelated = "website-33333333-3333-4333-8333-333333333333";
  await env.backup.put(`runs/${"b".repeat(40)}/sandboxes/${unrelated}.json`, "{}");

  const response = await route(new Request(`https://ci.test/api/ci/runs/${head}/terminate`, {
    method: "POST",
    headers: controlAuth(),
  }), env);

  const destroyed = [...active].sort();
  assert.equal(response.status, 200);
  assert.deepEqual(env.controls.events, ["workflow", ...destroyed]);
  assert.equal(env.controls.status, "terminated");
  assert.deepEqual((await response.json() as {
    sandboxCleanup: { destroyed: string[]; failed: unknown[] };
  }).sandboxCleanup, { destroyed, failed: [] });
  assert.ok(await env.backup.head(`runs/${head}/control/terminated.json`));
  for (const runnerId of active) {
    assert.equal(await env.backup.head(`runs/${head}/sandboxes/${runnerId}.json`), null);
  }
  assert.ok(await env.backup.head(`runs/${"b".repeat(40)}/sandboxes/${unrelated}.json`));
});

test("incomplete Sandbox termination is retried and remains recoverable", async () => {
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
  const runnerId = "quality-44444444-4444-4444-8444-444444444444";
  const marker = `runs/${head}/sandboxes/${runnerId}.json`;
  await env.backup.put(marker, "{}");
  env.sandbox.failures.set(runnerId, 3);

  const response = await route(new Request(`https://ci.test/api/ci/runs/${head}/terminate`, {
    method: "POST",
    headers: controlAuth(),
  }), env);

  assert.equal(response.status, 409);
  assert.deepEqual(await response.json(), {
    error: "ci_termination_incomplete",
    sandboxCleanup: {
      destroyed: [],
      failed: [{
        runnerId,
        error: `failed to destroy active CI Sandbox ${runnerId}`,
      }],
    },
  });
  assert.equal(env.sandbox.attempts.get(runnerId), 3);
  assert.ok(await env.backup.head(marker), "failed teardown retains its registry marker");
});

function route(request: Request, env: CiStorageEnv): Promise<Response> {
  return routeCiRequest(request, env, new URL(request.url)) as Promise<Response>;
}

function uploadRequest(file: string, body: Uint8Array, sha256: string): Request {
  return new Request(`https://ci.test/api/ci/source/objects/${head}/${file}`, {
    method: "PUT",
    headers: auth({
      "content-length": String(body.byteLength),
      "x-nanocodex-sha256": sha256,
    }),
    body: byteBuffer(body),
  });
}

function cargoVendorUpload(sha256: string, lockBlob = cargoLockBlob): Request {
  return new Request(
    `https://ci.test/api/ci/cargo-vendor/${lockBlob}/bundle.tar.gz`,
    {
      method: "PUT",
      headers: auth({
        "content-length": String(cargoVendorBody.byteLength),
        "content-type": "application/gzip",
        "x-nanocodex-sha256": sha256,
      }),
      body: byteBuffer(cargoVendorBody),
    },
  );
}

function rustSecUploadRequest(sha256: string): Request {
  return new Request(
    `https://ci.test/api/ci/rustsec-advisory-db/${rustSecRevision}/bundle.tar.gz`,
    {
      method: "PUT",
      headers: auth({
        "content-length": String(rustSecBody.byteLength),
        "content-type": "application/gzip",
        "x-nanocodex-sha256": sha256,
      }),
      body: byteBuffer(rustSecBody),
    },
  );
}

function auth(extra: HeadersInit = {}): Headers {
  const headers = new Headers(extra);
  headers.set("authorization", "Bearer write-token");
  return headers;
}

function controlAuth(extra: HeadersInit = {}): Headers {
  const headers = new Headers(extra);
  headers.set("authorization", "Bearer control-token");
  return headers;
}

function configured(bucket: ReturnType<typeof memoryBucket>, repository: ReturnType<typeof memoryNamespace>) {
  const backup = memoryBucket();
  const controls = { terminations: 0, status: "running", events: [] as string[] };
  const sandbox = memorySandboxNamespace(controls);
  return {
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
    CI_SOURCE_WRITE_TOKEN: "write-token",
    CI_CONTROL_TOKEN: "control-token",
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
  }>();
  const object = (key: string, value: NonNullable<ReturnType<typeof objects.get>>) => ({
    key,
    size: value.body.byteLength,
    etag: "etag",
    httpEtag: '"etag"',
    uploaded: new Date(),
    customMetadata: value.customMetadata,
    httpMetadata: value.httpMetadata,
    checksums: {
      sha256: hexBuffer(value.customMetadata?.sha256 ?? ""),
      toJSON: () => ({ sha256: value.customMetadata?.sha256 }),
    },
    writeHttpMetadata(headers: Headers) {
      if (value.httpMetadata?.contentType) headers.set("content-type", value.httpMetadata.contentType);
      if (value.httpMetadata?.cacheControl) headers.set("cache-control", value.httpMetadata.cacheControl);
    },
  });
  return {
    async head(key: string) {
      const value = objects.get(key);
      return value ? object(key, value) : null;
    },
    async put(key: string, body: ReadableStream | string | Uint8Array, options: R2PutOptions = {}) {
      if (objects.has(key) && options.onlyIf) return null;
      const bytes = typeof body === "string" ? new TextEncoder().encode(body)
        : body instanceof Uint8Array ? body
        : new Uint8Array(await new Response(body).arrayBuffer());
      const value = {
        body: bytes,
        customMetadata: options.customMetadata,
        httpMetadata: options.httpMetadata as R2HTTPMetadata | undefined,
      };
      objects.set(key, value);
      return object(key, value);
    },
    async get(key: string) {
      const value = objects.get(key);
      if (!value) return null;
      return {
        ...object(key, value),
        body: new Response(byteBuffer(value.body)).body,
        bodyUsed: false,
        arrayBuffer: async () => value.body.buffer.slice(
          value.body.byteOffset,
          value.body.byteOffset + value.body.byteLength,
        ),
        text: async () => new TextDecoder().decode(value.body),
        json: async () => JSON.parse(new TextDecoder().decode(value.body)),
        blob: async () => new Blob([byteBuffer(value.body)]),
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

function memoryNamespace() {
  const state: {
    publication?: CiSourcePublication;
    run?: Record<string, unknown>;
    published?: unknown;
  } = {};
  const stub = {
    async fetch(input: string | URL | Request, init?: RequestInit) {
      const request = new Request(input, init);
      const path = new URL(request.url).pathname;
      if (path === "/publications" && request.method === "PUT") {
        state.published = await request.json();
        state.publication = (state.published as { publication: CiSourcePublication }).publication;
        return Response.json({ publication: state.publication }, { status: 202 });
      }
      if (path === `/publications/${head}` && state.publication) {
        return Response.json(state.publication);
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
  };
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
      key: `cargo-vendor/${cargoLockBlob}/bundle.tar.gz`,
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
