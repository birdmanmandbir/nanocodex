import { authorized } from "./evalCoordinator.ts";
import {
  cargoVendorBundleKey,
  isCiSourcePublication,
  isCiSourceTree,
  isSha1,
  isSha256,
  rustSecAdvisoryBundleKey,
  sourceArchiveKey,
  sourceTreeKey,
  type CiSourcePublication,
} from "./ciSource.ts";
import type { CiRunRecord } from "./ciRepository.ts";
import {
  failureMarkerKey,
  terminateActiveSandboxes,
  terminationMarkerKey,
} from "./ciSandboxes.ts";

const IMMUTABLE_CACHE = "public, max-age=31536000, immutable";
const MAX_ARCHIVE_BYTES = 128 * 1024 * 1024;
const MAX_TREE_BYTES = 16 * 1024 * 1024;
const MAX_CARGO_VENDOR_BYTES = 16 * 1024 * 1024;
const MAX_RUSTSEC_ADVISORY_BYTES = 16 * 1024 * 1024;

export type CiStorageEnv = {
  ENVIRONMENT?: string;
  CI_SOURCE?: R2Bucket;
  BACKUP_BUCKET?: R2Bucket;
  CI_REPOSITORY?: DurableObjectNamespace;
  CI_WORKFLOW?: Workflow;
  SANDBOX?: DurableObjectNamespace<import("@cloudflare/ci/worker").CiSandbox>;
  CI_SOURCE_WRITE_TOKEN?: string;
  CI_CONTROL_TOKEN?: string;
};

export async function routeCiRequest(
  request: Request,
  env: CiStorageEnv,
  url: URL,
): Promise<Response | undefined> {
  if (!url.pathname.startsWith("/api/ci/")) return undefined;
  const sourceWriteRoute = url.pathname === "/api/ci/source/state" ||
    url.pathname === "/api/ci/source/publish" ||
    url.pathname.startsWith("/api/ci/source/objects/") ||
    (request.method !== "GET" && url.pathname.startsWith("/api/ci/cargo-vendor/")) ||
    (request.method !== "GET" && url.pathname.startsWith("/api/ci/rustsec-advisory-db/"));
  const controlRoute = /^\/api\/ci\/runs\/[a-f0-9]{40}\/terminate$/.test(
    url.pathname,
  );
  if (sourceWriteRoute && !authenticate(request, env.CI_SOURCE_WRITE_TOKEN)) {
    return unauthorized();
  }
  if (controlRoute && !authenticate(request, env.CI_CONTROL_TOKEN)) return unauthorized();
  if (
    !env.CI_SOURCE ||
    !env.BACKUP_BUCKET ||
    !env.CI_REPOSITORY ||
    !env.CI_WORKFLOW ||
    !env.SANDBOX
  ) {
    return error("ci_not_configured", 503);
  }
  const configured = env as RequiredCiEnv;

  const localSnapshot = url.pathname.match(
    /^\/api\/ci\/local-backups\/([0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12})\/data\.sqsh$/i,
  );
  if (
    localSnapshot &&
    (request.method === "GET" || request.method === "HEAD")
  ) {
    if (configured.ENVIRONMENT !== "development") return error("not_found", 404);
    return serveLocalSnapshot(
      configured.BACKUP_BUCKET,
      localSnapshot[1]!,
      request,
      url.searchParams.get("run"),
    );
  }

  if (
    url.pathname === "/api/ci/badge.svg" &&
    (request.method === "GET" || request.method === "HEAD")
  ) {
    return serveCiBadge(configured, request.method === "HEAD");
  }

  if (url.pathname === "/api/ci/source/state" && request.method === "GET") {
    return repository(configured).fetch("https://ci-repository/state");
  }

  const upload = url.pathname.match(
    /^\/api\/ci\/source\/objects\/([a-f0-9]{40})\/(source\.tar\.gz|tree\.json)$/,
  );
  if (upload && request.method === "PUT") {
    return uploadSourceObject(request, configured.CI_SOURCE, upload[1]!, upload[2]!);
  }

  if (url.pathname === "/api/ci/source/publish" && request.method === "PUT") {
    return publishSource(request, configured);
  }

  const cargoVendor = url.pathname.match(
    /^\/api\/ci\/cargo-vendor\/([a-f0-9]{40})\/bundle\.tar\.gz$/,
  );
  if (cargoVendor && request.method === "PUT") {
    return uploadCargoVendor(request, configured.CI_SOURCE, cargoVendor[1]!);
  }
  if (cargoVendor && (request.method === "GET" || request.method === "HEAD")) {
    return serveCargoVendor(configured.CI_SOURCE, cargoVendor[1]!, request.method === "HEAD");
  }

  const rustSec = url.pathname.match(
    /^\/api\/ci\/rustsec-advisory-db\/([a-f0-9]{40})\/bundle\.tar\.gz$/,
  );
  if (rustSec && request.method === "PUT") {
    return uploadRustSecAdvisory(request, configured.CI_SOURCE, rustSec[1]!);
  }
  if (rustSec && (request.method === "GET" || request.method === "HEAD")) {
    return serveRustSecAdvisory(
      configured.CI_SOURCE,
      rustSec[1]!,
      request.method === "HEAD",
    );
  }

  const archive = url.pathname.match(/^\/api\/ci\/source\/([a-f0-9]{40})\/archive$/);
  if (archive && request.method === "GET") return serveArchive(configured, archive[1]!);

  if (url.pathname === "/api/ci/runs" && request.method === "GET") {
    const response = await repository(configured).fetch("https://ci-repository/runs");
    if (!response.ok) return response;
    const body = await response.json() as { runs: CiRunRecord[] };
    return Response.json(body, noStore());
  }

  const contentArtifact = url.pathname.match(
    /^\/api\/ci\/artifacts\/(web-wasm)\/([a-f0-9]{64})\.tar$/,
  );
  if (contentArtifact && (request.method === "GET" || request.method === "HEAD")) {
    return serveContentAddressedCiArtifact(
      configured.BACKUP_BUCKET,
      contentArtifact[1]! as "web-wasm",
      contentArtifact[2]!,
      request.method === "HEAD",
    );
  }

  const artifact = url.pathname.match(
    /^\/api\/ci\/runs\/([a-f0-9]{40})\/artifacts\/(web-dist|web-wasm)\.tar$/,
  );
  if (artifact && (request.method === "GET" || request.method === "HEAD")) {
    return serveCiArtifact(
      configured,
      artifact[1]!,
      artifact[2]! as "web-dist" | "web-wasm",
      request.method === "HEAD",
    );
  }

  const stepFile = url.pathname.match(
    /^\/api\/ci\/runs\/([a-f0-9]{40})\/steps\/([a-z0-9-]{1,80})\/(result\.json|stdout\.log|stderr\.log)$/,
  );
  if (stepFile && request.method === "GET") {
    return serveRunFile(configured, stepFile[1]!, stepFile[2]!, stepFile[3]!);
  }

  const run = url.pathname.match(/^\/api\/ci\/runs\/([a-f0-9]{40})$/);
  if (run && request.method === "GET") {
    const response = await repository(configured).fetch(`https://ci-repository/runs/${run[1]}`);
    if (!response.ok) return response;
    return Response.json(await runStatus(configured, await response.json<CiRunRecord>()), noStore());
  }

  const control = url.pathname.match(
    /^\/api\/ci\/runs\/([a-f0-9]{40})\/(terminate)$/,
  );
  if (control && request.method === "POST") {
    const response = await repository(configured).fetch(`https://ci-repository/runs/${control[1]}`);
    if (!response.ok) return response;
    const record = await response.json<CiRunRecord>();
    await configured.BACKUP_BUCKET.put(
      terminationMarkerKey(record.head),
      JSON.stringify({
        version: 1,
        head: record.head,
        workflowId: record.workflowId,
        terminatedAt: new Date().toISOString(),
      }),
      {
        httpMetadata: { contentType: "application/json" },
        customMetadata: { kind: "ci-run-termination", head: record.head },
      },
    );
    let workflowFailure: string | undefined;
    try {
      const instance = await configured.CI_WORKFLOW.get(record.workflowId);
      await instance.terminate();
    } catch (cause) {
      workflowFailure = boundedError(cause);
    }
    const sandboxCleanup = await terminateActiveSandboxes(configured, record.head);
    if (workflowFailure || sandboxCleanup.failed.length > 0) {
      return Response.json({
        error: "ci_termination_incomplete",
        ...(workflowFailure ? { workflowFailure } : {}),
        sandboxCleanup,
      }, { status: 409, ...noStore() });
    }
    return Response.json({
      ...await runStatus(configured, record),
      sandboxCleanup,
    }, noStore());
  }

  return error("not_found", 404);
}

async function uploadSourceObject(
  request: Request,
  bucket: R2Bucket,
  head: string,
  file: string,
): Promise<Response> {
  if (request.body == null) return error("missing_body", 400);
  const size = Number(request.headers.get("content-length"));
  const sha256 = request.headers.get("x-nanocodex-sha256");
  const maxSize = file === "source.tar.gz" ? MAX_ARCHIVE_BYTES : MAX_TREE_BYTES;
  if (!Number.isSafeInteger(size) || size <= 0 || size > maxSize || !isSha256(sha256)) {
    await request.body.cancel();
    return error("invalid_source_object", 400);
  }
  const key = file === "source.tar.gz" ? sourceArchiveKey(head) : sourceTreeKey(head);
  const existing = await bucket.head(key);
  if (existing) {
    await request.body.cancel();
    return matchesObject(existing, size, sha256)
      ? Response.json({ key, size, sha256, uploaded: false }, noStore())
      : error("immutable_object_conflict", 409);
  }
  const object = await bucket.put(key, request.body, {
    onlyIf: { etagDoesNotMatch: "*" },
    sha256,
    httpMetadata: {
      contentType: file === "source.tar.gz" ? "application/gzip" : "application/json",
      cacheControl: IMMUTABLE_CACHE,
    },
    customMetadata: { sha256, size: String(size), kind: file },
  });
  const resolved = object ?? await bucket.head(key);
  if (!resolved || !matchesObject(resolved, size, sha256)) {
    return error("immutable_object_conflict", 409);
  }
  return Response.json({ key, size, sha256, uploaded: object != null }, noStore());
}

async function uploadCargoVendor(
  request: Request,
  bucket: R2Bucket,
  cargoLockBlob: string,
): Promise<Response> {
  if (request.body == null) return error("missing_body", 400);
  const size = Number(request.headers.get("content-length"));
  const sha256 = request.headers.get("x-nanocodex-sha256");
  if (
    request.headers.get("content-type") !== "application/gzip" ||
    !Number.isSafeInteger(size) || size <= 0 || size > MAX_CARGO_VENDOR_BYTES ||
    !isSha256(sha256)
  ) {
    await request.body.cancel();
    return error("invalid_cargo_vendor_bundle", 400);
  }
  const key = cargoVendorBundleKey(cargoLockBlob);
  const existing = await bucket.head(key);
  if (existing) {
    await request.body.cancel();
    return matchesCargoVendor(existing, cargoLockBlob, size, sha256)
      ? Response.json({ key, size, sha256, cargoLockBlob, uploaded: false }, noStore())
      : error("immutable_object_conflict", 409);
  }
  const object = await bucket.put(key, request.body, {
    onlyIf: { etagDoesNotMatch: "*" },
    sha256,
    httpMetadata: { contentType: "application/gzip", cacheControl: IMMUTABLE_CACHE },
    customMetadata: {
      sha256,
      size: String(size),
      kind: "cargo-git-vendor",
      cargoLockBlob,
    },
  });
  const resolved = object ?? await bucket.head(key);
  if (!resolved || !matchesCargoVendor(resolved, cargoLockBlob, size, sha256)) {
    return error("immutable_object_conflict", 409);
  }
  return Response.json({ key, size, sha256, cargoLockBlob, uploaded: object != null }, noStore());
}

async function uploadRustSecAdvisory(
  request: Request,
  bucket: R2Bucket,
  revision: string,
): Promise<Response> {
  if (request.body == null) return error("missing_body", 400);
  const size = Number(request.headers.get("content-length"));
  const sha256 = request.headers.get("x-nanocodex-sha256");
  if (
    request.headers.get("content-type") !== "application/gzip" ||
    !Number.isSafeInteger(size) || size <= 0 || size > MAX_RUSTSEC_ADVISORY_BYTES ||
    !isSha256(sha256)
  ) {
    await request.body.cancel();
    return error("invalid_rustsec_advisory_bundle", 400);
  }
  const key = rustSecAdvisoryBundleKey(revision);
  const existing = await bucket.head(key);
  if (existing) {
    await request.body.cancel();
    return matchesRustSecAdvisory(existing, revision, size, sha256)
      ? Response.json({ key, size, sha256, revision, uploaded: false }, noStore())
      : error("immutable_object_conflict", 409);
  }
  const object = await bucket.put(key, request.body, {
    onlyIf: { etagDoesNotMatch: "*" },
    sha256,
    httpMetadata: { contentType: "application/gzip", cacheControl: IMMUTABLE_CACHE },
    customMetadata: {
      sha256,
      size: String(size),
      kind: "rustsec-advisory-db",
      revision,
    },
  });
  const resolved = object ?? await bucket.head(key);
  if (!resolved || !matchesRustSecAdvisory(resolved, revision, size, sha256)) {
    return error("immutable_object_conflict", 409);
  }
  return Response.json({ key, size, sha256, revision, uploaded: object != null }, noStore());
}

async function publishSource(request: Request, env: RequiredCiEnv): Promise<Response> {
  const body = await request.json().catch(() => undefined) as {
    expectedHead?: unknown;
    publication?: unknown;
  } | undefined;
  if (
    body == null ||
    !(body.expectedHead === null || isSha1(body.expectedHead)) ||
    !isCiSourcePublication(body.publication)
  ) return error("invalid_publication", 400);
  const publication = body.publication;
  const [archive, treeObject, cargoVendor, rustSec] = await Promise.all([
    env.CI_SOURCE.head(publication.archive.key),
    env.CI_SOURCE.head(publication.tree.key),
    env.CI_SOURCE.head(publication.cargoVendor.key),
    env.CI_SOURCE.head(publication.rustSec.key),
  ]);
  const invalid = [
    !archive || !matchesObject(archive, publication.archive.size, publication.archive.sha256)
      ? publication.archive.key : null,
    !treeObject || !matchesObject(treeObject, publication.tree.size, publication.tree.sha256)
      ? publication.tree.key : null,
    !cargoVendor || !matchesCargoVendor(
      cargoVendor,
      publication.cargoLockBlob,
      publication.cargoVendor.size,
      publication.cargoVendor.sha256,
    ) ? publication.cargoVendor.key : null,
    !rustSec || !matchesRustSecAdvisory(
      rustSec,
      publication.rustSecRevision,
      publication.rustSec.size,
      publication.rustSec.sha256,
    ) ? publication.rustSec.key : null,
  ].filter((key): key is string => key != null);
  if (invalid.length > 0) return Response.json({ error: "source_objects_invalid", invalid }, {
    status: 409,
    headers: { "cache-control": "no-store" },
  });
  const tree = await env.CI_SOURCE.get(publication.tree.key);
  let parsedTree: unknown;
  try {
    parsedTree = await tree?.json();
  } catch {
    return error("source_tree_invalid", 409);
  }
  if (!isCiSourceTree(parsedTree, publication.head)) return error("source_tree_invalid", 409);
  if (
    parsedTree.archive.size !== publication.archive.size ||
    parsedTree.archive.sha256 !== publication.archive.sha256
  ) return error("source_archive_tree_mismatch", 409);
  const cargoLock = parsedTree.files.find(({ path }) => path === "Cargo.lock");
  if (cargoLock?.sha !== publication.cargoLockBlob) {
    return error("cargo_lock_bundle_mismatch", 409);
  }
  return repository(env).fetch("https://ci-repository/publications", {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ expectedHead: body.expectedHead, publication }),
  });
}

async function serveCargoVendor(
  bucket: R2Bucket,
  cargoLockBlob: string,
  headOnly: boolean,
): Promise<Response> {
  const key = cargoVendorBundleKey(cargoLockBlob);
  const object = headOnly ? await bucket.head(key) : await bucket.get(key);
  const size = object?.size;
  const sha256 = object?.customMetadata?.sha256;
  if (
    !object || typeof size !== "number" || !Number.isSafeInteger(size) ||
    size <= 0 || size > MAX_CARGO_VENDOR_BYTES ||
    !isSha256(sha256) || !matchesCargoVendor(object, cargoLockBlob, size, sha256)
  ) {
    if (object && "body" in object) await (object as R2ObjectBody).body.cancel();
    return error("cargo_vendor_bundle_missing", 404);
  }
  const headers = new Headers({
    "cache-control": IMMUTABLE_CACHE,
    "content-length": String(size),
    "content-type": "application/gzip",
    "etag": object.httpEtag,
    "x-content-type-options": "nosniff",
    "x-nanocodex-cargo-lock": cargoLockBlob,
    "x-nanocodex-key": key,
    "x-nanocodex-sha256": sha256,
  });
  return new Response(headOnly ? null : (object as R2ObjectBody).body, { headers });
}

async function serveRustSecAdvisory(
  bucket: R2Bucket,
  revision: string,
  headOnly: boolean,
): Promise<Response> {
  const key = rustSecAdvisoryBundleKey(revision);
  const object = headOnly ? await bucket.head(key) : await bucket.get(key);
  const size = object?.size;
  const sha256 = object?.customMetadata?.sha256;
  if (
    !object || typeof size !== "number" || !Number.isSafeInteger(size) ||
    size <= 0 || size > MAX_RUSTSEC_ADVISORY_BYTES ||
    !isSha256(sha256) || !matchesRustSecAdvisory(object, revision, size, sha256)
  ) {
    if (object && "body" in object) await (object as R2ObjectBody).body.cancel();
    return error("rustsec_advisory_bundle_missing", 404);
  }
  const headers = new Headers({
    "cache-control": IMMUTABLE_CACHE,
    "content-length": String(size),
    "content-type": "application/gzip",
    "etag": object.httpEtag,
    "x-content-type-options": "nosniff",
    "x-nanocodex-key": key,
    "x-nanocodex-revision": revision,
    "x-nanocodex-sha256": sha256,
  });
  return new Response(headOnly ? null : (object as R2ObjectBody).body, { headers });
}

async function serveLocalSnapshot(
  bucket: R2Bucket,
  id: string,
  request: Request,
  runHead: string | null,
): Promise<Response> {
  if (!isSha1(runHead)) return error("invalid_run", 400);
  const [terminated, failed] = await Promise.all([
    bucket.head(terminationMarkerKey(runHead)),
    bucket.head(failureMarkerKey(runHead)),
  ]);
  if (terminated || failed) return error("run_inactive", 409);

  const key = `backups/${id}/data.sqsh`;
  const metadata = await bucket.head(key);
  if (!metadata || metadata.size <= 0 || !Number.isSafeInteger(metadata.size)) {
    return error("local_snapshot_missing", 404);
  }
  const range = parseByteRange(request.headers.get("range"), metadata.size);
  if (range === "invalid") {
    return new Response(null, {
      status: 416,
      headers: {
        "accept-ranges": "bytes",
        "cache-control": "no-store",
        "content-range": `bytes */${metadata.size}`,
        "x-content-type-options": "nosniff",
      },
    });
  }
  const object = request.method === "HEAD"
    ? metadata
    : await bucket.get(key, range ? {
      range: { offset: range.start, length: range.length },
    } : undefined);
  if (!object) return error("local_snapshot_missing", 404);
  const headers = new Headers({
    "accept-ranges": "bytes",
    "cache-control": "no-store",
    "content-length": String(range?.length ?? metadata.size),
    "content-type": "application/octet-stream",
    "x-content-type-options": "nosniff",
  });
  if (range) headers.set("content-range", `bytes ${range.start}-${range.end}/${metadata.size}`);
  return new Response(
    request.method === "HEAD" ? null : (object as R2ObjectBody).body,
    { status: range ? 206 : 200, headers },
  );
}

function parseByteRange(
  value: string | null,
  size: number,
): { start: number; end: number; length: number } | "invalid" | undefined {
  if (value == null) return undefined;
  const match = /^bytes=(\d+)-(\d+)$/.exec(value);
  if (!match) return "invalid";
  const start = Number(match[1]);
  const requestedEnd = Number(match[2]);
  if (
    !Number.isSafeInteger(start) || !Number.isSafeInteger(requestedEnd) ||
    start < 0 || start >= size || requestedEnd < start
  ) return "invalid";
  const end = Math.min(requestedEnd, size - 1);
  return { start, end, length: end - start + 1 };
}

async function serveArchive(env: RequiredCiEnv, head: string): Promise<Response> {
  const publicationResponse = await repository(env).fetch(
    `https://ci-repository/publications/${head}`,
  );
  if (!publicationResponse.ok) return publicationResponse;
  const publication = await publicationResponse.json<CiSourcePublication>();
  const object = await env.CI_SOURCE.get(publication.archive.key);
  if (!object) return error("source_archive_missing", 503);
  if (!matchesObject(object, publication.archive.size, publication.archive.sha256)) {
    await object.body.cancel();
    return error("source_archive_invalid", 503);
  }
  const headers = new Headers();
  object.writeHttpMetadata(headers);
  headers.set("cache-control", IMMUTABLE_CACHE);
  headers.set("etag", object.httpEtag);
  headers.set("x-content-type-options", "nosniff");
  headers.set("x-nanocodex-sha256", publication.archive.sha256);
  headers.set("content-length", String(object.size));
  return new Response(object.body, { headers });
}

async function runStatus(env: RequiredCiEnv, run: CiRunRecord) {
  let workflow: unknown = { status: run.state === "pending" ? "queued" : "unknown" };
  if (run.state === "dispatched") {
    try {
      workflow = await (await env.CI_WORKFLOW.get(run.workflowId)).status();
    } catch (cause) {
      workflow = { status: "unknown", error: boundedError(cause) };
    }
  }
  const [result, progress] = await Promise.all([
    env.BACKUP_BUCKET.get(`runs/${run.head}/result.json`),
    env.BACKUP_BUCKET.get(`runs/${run.head}/progress.json`),
  ]);
  const resultValue = result
    ? await result.json().catch(() => ({ error: "invalid_result" }))
    : null;
  const progressValue = progress
    ? await progress.json().catch(() => ({ error: "invalid_progress" }))
    : null;
  const terminated = workflowStatus(workflow) === "terminated";
  return {
    ...run,
    workflow,
    result: terminated ? terminalResult(resultValue) : resultValue,
    progress: terminated ? terminalProgress(progressValue) : progressValue,
  };
}

async function serveCiBadge(
  env: RequiredCiEnv,
  headOnly: boolean,
): Promise<Response> {
  const state = await repository(env).fetch("https://ci-repository/state");
  let badge: CiBadge = { message: "unknown", color: "#6b7280" };
  if (state.ok) {
    const value = await state.json().catch(() => undefined) as {
      run?: CiRunRecord;
    } | undefined;
    if (value?.run) badge = badgeForRun(await runStatus(env, value.run));
  }
  const svg = renderCiBadge(badge);
  return new Response(headOnly ? null : svg, {
    headers: {
      "cache-control": "public, max-age=30, stale-while-revalidate=60",
      "content-length": String(new TextEncoder().encode(svg).byteLength),
      "content-type": "image/svg+xml; charset=utf-8",
      "x-content-type-options": "nosniff",
    },
  });
}

type CiBadge = {
  message: "passing" | "failing" | "running" | "queued" | "terminated" | "unknown";
  color: string;
};

function badgeForRun(value: unknown): CiBadge {
  if (!record(value)) return { message: "unknown", color: "#6b7280" };
  const result = record(value.result) ? value.result : undefined;
  switch (result?.status) {
    case "success":
      return { message: "passing", color: "#168a5b" };
    case "failure":
      return { message: "failing", color: "#d14343" };
    case "running":
      return { message: "running", color: "#2563eb" };
    case "terminated":
      return { message: "terminated", color: "#6b7280" };
  }
  if (value.state === "pending") return { message: "queued", color: "#b7791f" };
  const workflow = record(value.workflow) ? value.workflow.status : undefined;
  if (workflow === "errored") return { message: "failing", color: "#d14343" };
  if (workflow === "running" || workflow === "queued") {
    return { message: workflow, color: workflow === "running" ? "#2563eb" : "#b7791f" };
  }
  if (workflow === "terminated") return { message: "terminated", color: "#6b7280" };
  return { message: "unknown", color: "#6b7280" };
}

function renderCiBadge(badge: CiBadge): string {
  const label = "cloudflare ci";
  const labelWidth = 82;
  const messageWidth = Math.max(54, badge.message.length * 7 + 14);
  const width = labelWidth + messageWidth;
  const aria = `${label}: ${badge.message}`;
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="20" role="img" aria-label="${aria}"><title>${aria}</title><linearGradient id="s" x2="0" y2="100%"><stop offset="0" stop-color="#fff" stop-opacity=".15"/><stop offset="1" stop-opacity=".1"/></linearGradient><clipPath id="r"><rect width="${width}" height="20" rx="3" fill="#fff"/></clipPath><g clip-path="url(#r)"><rect width="${labelWidth}" height="20" fill="#f38020"/><rect x="${labelWidth}" width="${messageWidth}" height="20" fill="${badge.color}"/><rect width="${width}" height="20" fill="url(#s)"/></g><g fill="#fff" text-anchor="middle" font-family="Verdana,DejaVu Sans,sans-serif" font-size="11"><text x="${labelWidth / 2}" y="15" fill="#010101" fill-opacity=".3">${label}</text><text x="${labelWidth / 2}" y="14">${label}</text><text x="${labelWidth + messageWidth / 2}" y="15" fill="#010101" fill-opacity=".3">${badge.message}</text><text x="${labelWidth + messageWidth / 2}" y="14">${badge.message}</text></g></svg>`;
}

function workflowStatus(value: unknown): string | undefined {
  return record(value) && typeof value.status === "string" ? value.status : undefined;
}

function terminalResult(value: unknown): unknown {
  return record(value) && value.status === "running"
    ? { ...value, status: "terminated" }
    : value;
}

function terminalProgress(value: unknown): unknown {
  if (!record(value) || !Array.isArray(value.steps)) return value;
  return {
    ...value,
    steps: value.steps.map((step) =>
      record(step) && step.status === "running"
        ? { ...step, status: "terminated", message: "terminated by operator" }
        : step
    ),
  };
}

function record(value: unknown): value is Record<string, unknown> {
  return value != null && typeof value === "object" && !Array.isArray(value);
}

async function serveCiArtifact(
  env: RequiredCiEnv,
  head: string,
  kind: "web-dist" | "web-wasm",
  headOnly: boolean,
): Promise<Response> {
  const run = await repository(env).fetch(`https://ci-repository/runs/${head}`);
  if (!run.ok) return run;
  const key = `runs/${head}/artifacts/${kind}.tar`;
  const object = headOnly
    ? await env.BACKUP_BUCKET.head(key)
    : await env.BACKUP_BUCKET.get(key);
  const sha256 = object?.customMetadata?.sha256;
  if (
    !object || object.key !== key || object.size <= 0 ||
    object.customMetadata?.head !== head || object.customMetadata?.kind !== kind ||
    !isSha256(sha256) || object.checksums.sha256 == null ||
    hex(object.checksums.sha256) !== sha256
  ) {
    if (object && "body" in object) await (object as R2ObjectBody).body.cancel();
    return error("ci_artifact_missing", 404);
  }
  return new Response(headOnly ? null : (object as R2ObjectBody).body, {
    headers: {
      "cache-control": IMMUTABLE_CACHE,
      "content-disposition": `attachment; filename="nanocodex-${head}-${kind}.tar"`,
      "content-length": String(object.size),
      "content-type": "application/x-tar",
      "etag": object.httpEtag,
      "x-content-type-options": "nosniff",
      "x-nanocodex-sha256": sha256,
    },
  });
}

async function serveContentAddressedCiArtifact(
  bucket: R2Bucket,
  kind: "web-wasm",
  sha256: string,
  headOnly: boolean,
): Promise<Response> {
  const key = `artifacts/${kind}/${sha256}.tar`;
  const object = headOnly ? await bucket.head(key) : await bucket.get(key);
  if (
    !object || object.key !== key || object.size <= 0 ||
    object.size > 16 * 1024 * 1024 ||
    object.customMetadata?.kind !== kind ||
    object.customMetadata?.sha256 !== sha256 ||
    object.checksums.sha256 == null || hex(object.checksums.sha256) !== sha256
  ) {
    if (object && "body" in object) await (object as R2ObjectBody).body.cancel();
    return error("ci_artifact_missing", 404);
  }
  return new Response(headOnly ? null : (object as R2ObjectBody).body, {
    headers: {
      "cache-control": IMMUTABLE_CACHE,
      "content-disposition": `attachment; filename="nanocodex-${kind}-${sha256}.tar"`,
      "content-length": String(object.size),
      "content-type": "application/x-tar",
      "etag": object.httpEtag,
      "x-content-type-options": "nosniff",
      "x-nanocodex-sha256": sha256,
    },
  });
}

async function serveRunFile(
  env: RequiredCiEnv,
  head: string,
  step: string,
  file: string,
): Promise<Response> {
  const run = await repository(env).fetch(`https://ci-repository/runs/${head}`);
  if (!run.ok) return run;
  const stableKey = `runs/${head}/steps/${step}/${file}`;
  let object = await env.BACKUP_BUCKET.get(stableKey);
  if (!object && (file === "stdout.log" || file === "stderr.log")) {
    const result = await env.BACKUP_BUCKET.get(
      `runs/${head}/steps/${step}/result.json`,
    );
    const metadata = await result?.json<{
      stdout?: { key?: string };
      stderr?: { key?: string };
    }>().catch(() => undefined);
    const key = file === "stdout.log" ? metadata?.stdout?.key : metadata?.stderr?.key;
    const expected = new RegExp(
      `^runs/${head}/steps/${step}/attempts/[1-9][0-9]*/${file}$`,
    );
    if (key && expected.test(key)) object = await env.BACKUP_BUCKET.get(key);
  }
  if (!object) return error("ci_step_file_missing", 404);
  return new Response(object.body, {
    headers: {
      "cache-control": "no-store",
      "content-length": String(object.size),
      "content-type": file.endsWith(".json")
        ? "application/json"
        : "text/plain; charset=utf-8",
      "x-content-type-options": "nosniff",
    },
  });
}

type RequiredCiEnv = Required<
  Pick<
    CiStorageEnv,
    "CI_SOURCE" | "BACKUP_BUCKET" | "CI_REPOSITORY" | "CI_WORKFLOW" | "SANDBOX"
  >
> & CiStorageEnv;

function repository(env: RequiredCiEnv) {
  return env.CI_REPOSITORY.get(env.CI_REPOSITORY.idFromName("nanocodex"));
}

function authenticate(request: Request, token: string | undefined): boolean {
  return Boolean(token && authorized(request, token));
}

function matchesObject(object: R2Object, size: number, sha256: string): boolean {
  return object.size === size &&
    object.checksums.sha256 != null &&
    hex(object.checksums.sha256) === sha256 &&
    object.customMetadata?.sha256 === sha256 &&
    object.customMetadata?.size === String(size);
}

function matchesCargoVendor(
  object: R2Object,
  cargoLockBlob: string,
  size: number,
  sha256: string,
): boolean {
  return object.key === cargoVendorBundleKey(cargoLockBlob) &&
    object.customMetadata?.kind === "cargo-git-vendor" &&
    object.customMetadata?.cargoLockBlob === cargoLockBlob &&
    matchesObject(object, size, sha256);
}

function matchesRustSecAdvisory(
  object: R2Object,
  revision: string,
  size: number,
  sha256: string,
): boolean {
  return object.key === rustSecAdvisoryBundleKey(revision) &&
    object.customMetadata?.kind === "rustsec-advisory-db" &&
    object.customMetadata?.revision === revision &&
    matchesObject(object, size, sha256);
}

function hex(buffer: ArrayBuffer): string {
  return [...new Uint8Array(buffer)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

function unauthorized(): Response {
  return Response.json({ error: "unauthorized" }, {
    status: 401,
    headers: { "cache-control": "no-store", "www-authenticate": "Bearer" },
  });
}

function error(message: string, status: number): Response {
  return Response.json({ error: message }, { status, headers: { "cache-control": "no-store" } });
}

function noStore(): ResponseInit {
  return { headers: { "cache-control": "no-store", "x-content-type-options": "nosniff" } };
}

function boundedError(value: unknown): string {
  return (value instanceof Error ? value.message : String(value)).slice(0, 2_000);
}
