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
import {
  isCiTerminalResult,
  type CiRunRecord,
} from "./ciRepository.ts";
import {
  failureMarkerKey,
  terminationMarkerKey,
} from "./ciSandboxes.ts";
import {
  routeCiDistributionControl,
  type CiDistributionControlEnv,
} from "./ciDistributionController.ts";
import { npmPreviewVersion } from "./ciWorkflowPlan.ts";

const IMMUTABLE_CACHE = "public, max-age=31536000, immutable";
const MAX_ARCHIVE_BYTES = 128 * 1024 * 1024;
const MAX_TREE_BYTES = 16 * 1024 * 1024;
const MAX_CARGO_VENDOR_BYTES = 256 * 1024 * 1024;
const CARGO_VENDOR_PART_BYTES = 32 * 1024 * 1024;
const MAX_CARGO_VENDOR_PARTS = MAX_CARGO_VENDOR_BYTES / CARGO_VENDOR_PART_BYTES;
const MAX_RUSTSEC_ADVISORY_BYTES = 16 * 1024 * 1024;
const MAX_PUBLICATION_LEASE_MS = 5 * 60 * 1_000;
const CLOSE_ID_PATTERN =
  /^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;

type SourceCapability = "master" | "pull_request";

type SourceAuthentication =
  | { kind: "none" }
  | { kind: "ambiguous" }
  | { kind: SourceCapability };

type PublishSourceInput = {
  expectedHead: string | null;
  publication: CiSourcePublication;
  reopen?: unknown;
};

type PublicationLease = {
  version: 1;
  kind: "publication";
  leaseId: string;
  head: string;
  acquiredAt: string;
  expiresAt: string;
};

export type CiStorageEnv = CiDistributionControlEnv & {
  ENVIRONMENT?: string;
  CI_SOURCE?: R2Bucket;
  BACKUP_BUCKET?: R2Bucket;
  CI_REPOSITORY?: DurableObjectNamespace;
  CI_MACOS_JOBS?: DurableObjectNamespace;
  CI_RELEASES?: DurableObjectNamespace;
  CI_WORKFLOW?: Workflow;
  SANDBOX?: DurableObjectNamespace<import("@cloudflare/ci/worker").CiSandbox>;
  CI_MASTER_SOURCE_WRITE_TOKEN?: string;
  CI_PR_SOURCE_WRITE_TOKEN?: string;
  CI_CONTROL_TOKEN?: string;
  CI_MACOS_RUNNER_TOKEN?: string;
  CI_RELEASE_TOKEN?: string;
};

export async function routeCiRequest(
  request: Request,
  env: CiStorageEnv,
  url: URL,
): Promise<Response | undefined> {
  const distributionControl = await routeCiDistributionControl(request, env, url);
  if (distributionControl) return distributionControl;
  if (url.pathname === "/api/releases" || url.pathname.startsWith("/api/releases/")) {
    if (!env.CI_RELEASES) return error("releases_not_configured", 503);
    const internal = new URL(request.url);
    internal.pathname = internal.pathname.slice("/api/releases".length) || "/";
    return releases(env.CI_RELEASES).fetch(new Request(internal, request));
  }
  if (!url.pathname.startsWith("/api/ci/")) return undefined;
  if (url.pathname === "/api/ci/macos" || url.pathname.startsWith("/api/ci/macos/")) {
    if (!authenticate(request, env.CI_MACOS_RUNNER_TOKEN)) return unauthorized();
    if (!env.CI_MACOS_JOBS) return error("ci_macos_not_configured", 503);
    return macJobs(env.CI_MACOS_JOBS).fetch(request);
  }
  const sourceAuthentication = authenticateSource(request, env);
  const sourceStateRoute = url.pathname === "/api/ci/source/state";
  const pullRequestLaneRoute = /^\/api\/ci\/source\/pull-requests\/[1-9][0-9]*(?:\/state|\/publications\/[a-f0-9]{40})$/.test(
    url.pathname,
  );
  const masterPublicationRoute =
    /^\/api\/ci\/source\/master\/publications\/[a-f0-9]{40}$/.test(url.pathname);
  const sourceObjectRoute = url.pathname.startsWith("/api/ci/source/objects/");
  const publishSourceRoute = url.pathname === "/api/ci/source/publish" &&
    request.method === "PUT";
  const cargoVendorProtectedRoute = request.method !== "GET" &&
    url.pathname.startsWith("/api/ci/cargo-vendor/");
  const rustSecProtectedRoute = request.method !== "GET" &&
    url.pathname.startsWith("/api/ci/rustsec-advisory-db/");
  const controlRoute = /^\/api\/ci\/runs\/[a-f0-9]{40}\/terminate$/.test(
    url.pathname,
  );

  if (sourceStateRoute) {
    const denied = requireSourceCapability(
      sourceAuthentication,
      request.method === "GET" ? ["master", "pull_request"] : ["master"],
    );
    if (denied) return denied;
  }
  if (pullRequestLaneRoute) {
    const denied = requireSourceCapability(sourceAuthentication, ["pull_request"]);
    if (denied) return denied;
  }
  if (masterPublicationRoute) {
    const denied = requireSourceCapability(sourceAuthentication, ["master"]);
    if (
      denied &&
      !(
        request.method === "GET" && sourceAuthentication.kind === "none" &&
        authenticate(request, env.CI_RELEASE_TOKEN)
      )
    ) return denied;
  }
  if (sourceObjectRoute || cargoVendorProtectedRoute) {
    const denied = requireSourceCapability(sourceAuthentication, ["master", "pull_request"]);
    if (denied) return denied;
  }
  if (rustSecProtectedRoute) {
    const denied = requireSourceCapability(
      sourceAuthentication,
      request.method === "HEAD" ? ["master", "pull_request"] : ["master"],
    );
    if (denied) return denied;
  }

  let publicationInput: PublishSourceInput | undefined;
  if (publishSourceRoute) {
    const denied = requireSourceCapability(sourceAuthentication, ["master", "pull_request"]);
    if (denied) return denied;
    const parsed = await parsePublishSourceInput(request);
    if (parsed instanceof Response) return parsed;
    const publicationCapability: SourceCapability = parsed.publication.lane?.type === "pull_request"
      ? "pull_request"
      : "master";
    if (sourceAuthentication.kind !== publicationCapability) return forbidden();
    publicationInput = parsed;
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

  const publicPullRequest = url.pathname.match(
    /^\/api\/ci\/pull-requests\/([1-9][0-9]*)$/,
  );
  if (publicPullRequest && request.method === "GET") {
    const number = Number(publicPullRequest[1]);
    if (!Number.isSafeInteger(number)) return error("pull_request_not_found", 404);
    return servePublicPullRequest(configured, number);
  }

  const exactPullRequestManifest = url.pathname.match(
    /^\/api\/ci\/pull-requests\/([1-9][0-9]*)\/builds\/([a-f0-9]{40})\/([a-f0-9]{40})\/manifests\/([a-f0-9]{64})$/,
  );
  if (exactPullRequestManifest && request.method === "GET") {
    return serveExactPullRequestManifest(
      configured,
      Number(exactPullRequestManifest[1]),
      exactPullRequestManifest[2]!,
      exactPullRequestManifest[3]!,
      exactPullRequestManifest[4]!,
    );
  }

  const exactPullRequestNpmPreview = url.pathname.match(
    /^\/api\/ci\/pull-requests\/([1-9][0-9]*)\/builds\/([a-f0-9]{40})\/([a-f0-9]{40})\/artifacts\/npm-preview\/([a-f0-9]{64})\.tgz$/,
  );
  if (
    exactPullRequestNpmPreview &&
    (request.method === "GET" || request.method === "HEAD")
  ) {
    return serveExactPullRequestNpmPreview(
      configured,
      Number(exactPullRequestNpmPreview[1]),
      exactPullRequestNpmPreview[2]!,
      exactPullRequestNpmPreview[3]!,
      exactPullRequestNpmPreview[4]!,
      request.method === "HEAD",
    );
  }

  const exactPullRequestAsset = url.pathname.match(
    /^\/api\/ci\/pull-requests\/([1-9][0-9]*)\/builds\/([a-f0-9]{40})\/([a-f0-9]{40})\/artifacts\/(nanocodex-(?:x86_64-unknown-linux-gnu|aarch64-apple-darwin))$/,
  );
  if (
    exactPullRequestAsset &&
    (request.method === "GET" || request.method === "HEAD")
  ) {
    return serveExactPullRequestNativeAsset(
      configured,
      Number(exactPullRequestAsset[1]),
      exactPullRequestAsset[2]!,
      exactPullRequestAsset[3]!,
      exactPullRequestAsset[4]!,
      request.method === "HEAD",
    );
  }

  if (url.pathname === "/api/ci/source/state" && request.method === "GET") {
    return repository(configured).fetch("https://ci-repository/state");
  }

  const pullRequestState = url.pathname.match(
    /^\/api\/ci\/source\/pull-requests\/([1-9][0-9]*)\/state$/,
  );
  if (pullRequestState && (request.method === "GET" || request.method === "DELETE")) {
    return repository(configured).fetch(new Request(
      `https://ci-repository/pull-requests/${pullRequestState[1]}/state`,
      request,
    ));
  }

  const pullRequestPublication = url.pathname.match(
    /^\/api\/ci\/source\/pull-requests\/([1-9][0-9]*)\/publications\/([a-f0-9]{40})$/,
  );
  if (pullRequestPublication && request.method === "GET") {
    return repository(configured).fetch(
      `https://ci-repository/pull-requests/${pullRequestPublication[1]}/publications/${pullRequestPublication[2]}`,
    );
  }

  const masterPublication = url.pathname.match(
    /^\/api\/ci\/source\/master\/publications\/([a-f0-9]{40})$/,
  );
  if (masterPublication && request.method === "GET") {
    return repository(configured).fetch(
      `https://ci-repository/master/publications/${masterPublication[1]}`,
    );
  }

  const upload = url.pathname.match(
    /^\/api\/ci\/source\/objects\/([a-f0-9]{40})\/(source\.tar\.gz|tree\.json)$/,
  );
  if (upload && request.method === "PUT") {
    return uploadSourceObject(request, configured.CI_SOURCE, upload[1]!, upload[2]!);
  }
  if (upload && request.method === "HEAD") {
    return headSourceObject(configured.CI_SOURCE, upload[1]!, upload[2]!);
  }

  if (publishSourceRoute && publicationInput) {
    return publishSource(publicationInput, configured);
  }

  const cargoVendor = url.pathname.match(
    /^\/api\/ci\/cargo-vendor\/([a-f0-9]{40})\/([a-f0-9]{64})\/bundle\.tar\.gz$/,
  );
  if (cargoVendor && request.method === "PUT") {
    return uploadCargoVendor(
      request,
      configured.CI_SOURCE,
      cargoVendor[1]!,
      cargoVendor[2]!,
    );
  }
  if (cargoVendor && (request.method === "GET" || request.method === "HEAD")) {
    return serveCargoVendor(
      configured.CI_SOURCE,
      cargoVendor[1]!,
      cargoVendor[2]!,
      request.method === "HEAD",
    );
  }

  const cargoVendorMultipart = url.pathname.match(
    /^\/api\/ci\/cargo-vendor\/([a-f0-9]{40})\/([a-f0-9]{64})\/multipart(?:\/(complete|parts\/([1-9][0-9]*)))?$/,
  );
  if (cargoVendorMultipart) {
    const cargoLockBlob = cargoVendorMultipart[1]!;
    const bundleSha256 = cargoVendorMultipart[2]!;
    const operation = cargoVendorMultipart[3];
    if (!operation && request.method === "POST") {
      return createCargoVendorMultipart(
        request,
        repository(configured),
        cargoLockBlob,
        bundleSha256,
      );
    }
    if (!operation && request.method === "DELETE") {
      return abortCargoVendorMultipart(
        request,
        configured.CI_SOURCE,
        cargoLockBlob,
        bundleSha256,
      );
    }
    if (operation?.startsWith("parts/") && request.method === "PUT") {
      return uploadCargoVendorPart(
        request,
        configured.CI_SOURCE,
        cargoLockBlob,
        bundleSha256,
        Number(cargoVendorMultipart[4]),
      );
    }
    if (operation === "complete" && request.method === "POST") {
      return completeCargoVendorMultipart(
        request,
        configured.CI_SOURCE,
        cargoLockBlob,
        bundleSha256,
      );
    }
    return error("method_not_allowed", 405);
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

  const distribution = url.pathname.match(
    /^\/api\/ci\/distributions\/(stable|nightly)\/(v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)|[a-f0-9]{40})$/,
  );
  if (distribution && request.method === "GET") {
    const channel = distribution[1]! as "stable" | "nightly";
    const id = distribution[2]!;
    if ((channel === "stable") !== id.startsWith("v")) {
      return error("ci_distribution_missing", 404);
    }
    return serveDistributionEvidence(configured.BACKUP_BUCKET, channel, id);
  }

  const distributionStep = url.pathname.match(
    /^\/api\/ci\/distributions\/(stable|nightly)\/(v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)|[a-f0-9]{40})\/steps\/([a-z0-9-]{1,80})\/(result\.json|stdout\.log|stderr\.log)$/,
  );
  if (distributionStep && request.method === "GET") {
    const channel = distributionStep[1]! as "stable" | "nightly";
    const id = distributionStep[2]!;
    if ((channel === "stable") !== id.startsWith("v")) {
      return error("ci_distribution_step_missing", 404);
    }
    return serveDistributionStep(
      configured.BACKUP_BUCKET,
      channel,
      id,
      distributionStep[3]!,
      distributionStep[4]!,
    );
  }

  const contentArtifact = url.pathname.match(
    /^\/api\/ci\/artifacts\/(web-wasm|npm-package)\/([a-f0-9]{64})\.(tar|tgz)$/,
  );
  if (contentArtifact && (request.method === "GET" || request.method === "HEAD")) {
    const kind = contentArtifact[1]! as "web-wasm" | "npm-package";
    const expectedExtension = kind === "npm-package" ? "tgz" : "tar";
    if (contentArtifact[3] !== expectedExtension) return error("ci_artifact_missing", 404);
    return serveContentAddressedCiArtifact(
      configured.BACKUP_BUCKET,
      kind,
      contentArtifact[2]!,
      request.method === "HEAD",
    );
  }

  const artifact = url.pathname.match(
    /^\/api\/ci\/runs\/([a-f0-9]{40})\/artifacts\/(web-dist|web-wasm|npm-package)\.(tar|tgz)$/,
  );
  if (artifact && (request.method === "GET" || request.method === "HEAD")) {
    const kind = artifact[2]! as "web-dist" | "web-wasm" | "npm-package";
    const expectedExtension = kind === "npm-package" ? "tgz" : "tar";
    if (artifact[3] !== expectedExtension) return error("ci_artifact_missing", 404);
    return serveCiArtifact(
      configured,
      artifact[1]!,
      kind,
      request.method === "HEAD",
    );
  }

  const stepFile = url.pathname.match(
    /^\/api\/ci\/runs\/([a-f0-9]{40})\/steps\/([a-z0-9-]{1,80})\/(result\.json|stdout\.log|stderr\.log)$/,
  );
  if (stepFile && request.method === "GET") {
    return serveRunFile(configured, stepFile[1]!, stepFile[2]!, stepFile[3]!);
  }

  const macEvidence = url.pathname.match(
    /^\/api\/ci\/runs\/([a-f0-9]{40})\/macos(?:\/(stdout\.log|stderr\.log))?$/,
  );
  if (macEvidence && request.method === "GET") {
    if (!configured.CI_MACOS_JOBS) return error("ci_macos_not_configured", 503);
    return serveMacRunEvidence(
      configured.BACKUP_BUCKET,
      configured.CI_MACOS_JOBS,
      macEvidence[1]!,
      macEvidence[2],
    );
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
    const head = control[1]!;
    return repository(configured).fetch(
      `https://ci-repository/cancellations/${head}`,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ workflowId: `ci-${head}` }),
      },
    );
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

async function headSourceObject(
  bucket: R2Bucket,
  head: string,
  file: string,
): Promise<Response> {
  const key = file === "source.tar.gz" ? sourceArchiveKey(head) : sourceTreeKey(head);
  const object = await bucket.head(key);
  const size = object?.size;
  const sha256 = object?.customMetadata?.sha256;
  const maxSize = file === "source.tar.gz" ? MAX_ARCHIVE_BYTES : MAX_TREE_BYTES;
  if (
    !object || object.key !== key || object.customMetadata?.kind !== file ||
    typeof size !== "number" || !Number.isSafeInteger(size) || size <= 0 || size > maxSize ||
    !isSha256(sha256) || !matchesObject(object, size, sha256)
  ) return error("source_object_missing", 404);
  return new Response(null, {
    headers: {
      "cache-control": IMMUTABLE_CACHE,
      "content-length": String(size),
      "content-type": file === "source.tar.gz" ? "application/gzip" : "application/json",
      "etag": object.httpEtag,
      "x-content-type-options": "nosniff",
      "x-nanocodex-key": key,
      "x-nanocodex-sha256": sha256,
    },
  });
}

async function uploadCargoVendor(
  request: Request,
  bucket: R2Bucket,
  cargoLockBlob: string,
  bundleSha256: string,
): Promise<Response> {
  if (request.body == null) return error("missing_body", 400);
  const size = Number(request.headers.get("content-length"));
  const sha256 = request.headers.get("x-nanocodex-sha256");
  if (
    request.headers.get("content-type") !== "application/gzip" ||
    !Number.isSafeInteger(size) || size <= 0 || size > MAX_CARGO_VENDOR_BYTES ||
    sha256 !== bundleSha256
  ) {
    await request.body.cancel();
    return error("invalid_cargo_vendor_bundle", 400);
  }
  const key = cargoVendorBundleKey(cargoLockBlob, bundleSha256);
  const existing = await bucket.head(key);
  if (existing) {
    await request.body.cancel();
    return matchesCargoVendor(existing, cargoLockBlob, size, bundleSha256)
      ? Response.json({ key, size, sha256, cargoLockBlob, uploaded: false }, noStore())
      : error("immutable_object_conflict", 409);
  }
  let object: R2Object | null;
  try {
    object = await bucket.put(key, request.body, {
      onlyIf: { etagDoesNotMatch: "*" },
      sha256,
      httpMetadata: { contentType: "application/gzip", cacheControl: IMMUTABLE_CACHE },
      customMetadata: {
        sha256,
        size: String(size),
        kind: "cargo-vendor",
        cargoLockBlob,
      },
    });
  } catch (cause) {
    let resolved: R2Object | null;
    try {
      resolved = await bucket.head(key);
    } catch (headCause) {
      return Response.json({
        error: "cargo_vendor_upload_failed",
        detail: boundedError(headCause),
      }, { status: 503, ...noStore() });
    }
    if (resolved) {
      return matchesCargoVendor(resolved, cargoLockBlob, size, sha256)
        ? Response.json({ key, size, sha256, cargoLockBlob, uploaded: false }, noStore())
        : error("immutable_object_conflict", 409);
    }
    return isR2BadDigest(cause)
      ? error("invalid_cargo_vendor_bundle_checksum", 400)
      : Response.json({
        error: "cargo_vendor_upload_failed",
        detail: boundedError(cause),
      }, { status: 503, ...noStore() });
  }
  const resolved = object ?? await bucket.head(key);
  if (!resolved || !matchesCargoVendor(resolved, cargoLockBlob, size, sha256)) {
    return error("immutable_object_conflict", 409);
  }
  return Response.json({ key, size, sha256, cargoLockBlob, uploaded: object != null }, noStore());
}

async function createCargoVendorMultipart(
  request: Request,
  coordinator: DurableObjectStub,
  cargoLockBlob: string,
  bundleSha256: string,
): Promise<Response> {
  const input = await request.json().catch(() => undefined);
  if (
    !record(input) || !hasExactKeys(input, [
      "partCount",
      "partSize",
      "requestId",
      "sha256",
      "size",
      "version",
    ]) || input.version !== 1 || input.sha256 !== bundleSha256 ||
    typeof input.requestId !== "string" || !CLOSE_ID_PATTERN.test(input.requestId) ||
    typeof input.size !== "number" || !Number.isSafeInteger(input.size) ||
    input.size <= 0 || input.size > MAX_CARGO_VENDOR_BYTES ||
    input.partSize !== CARGO_VENDOR_PART_BYTES ||
    input.partCount !== Math.ceil(input.size / CARGO_VENDOR_PART_BYTES) ||
    typeof input.partCount !== "number" || !Number.isSafeInteger(input.partCount) ||
    input.partCount <= 0 || input.partCount > MAX_CARGO_VENDOR_PARTS
  ) return error("invalid_cargo_vendor_multipart", 400);
  return coordinator.fetch(new Request(
    `https://ci-repository/cargo-vendor/multipart/${input.requestId}`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        version: 1,
        requestId: input.requestId,
        cargoLockBlob,
        bundleSha256,
        size: input.size,
        partSize: input.partSize,
        partCount: input.partCount,
      }),
    },
  ));
}

async function uploadCargoVendorPart(
  request: Request,
  bucket: R2Bucket,
  cargoLockBlob: string,
  bundleSha256: string,
  partNumber: number,
): Promise<Response> {
  if (request.body == null) return error("missing_body", 400);
  const uploadId = request.headers.get("x-nanocodex-upload-id");
  const stagingId = request.headers.get("x-nanocodex-staging-id");
  const size = Number(request.headers.get("content-length"));
  const sha256 = request.headers.get("x-nanocodex-sha256");
  if (
    typeof uploadId !== "string" || uploadId.length === 0 || uploadId.length > 1_024 ||
    typeof stagingId !== "string" || !CLOSE_ID_PATTERN.test(stagingId) ||
    !Number.isSafeInteger(partNumber) || partNumber <= 0 ||
    partNumber > MAX_CARGO_VENDOR_PARTS ||
    !Number.isSafeInteger(size) || size <= 0 || size > CARGO_VENDOR_PART_BYTES ||
    !isSha256(sha256) || request.headers.get("content-type") !== "application/octet-stream"
  ) {
    await request.body.cancel();
    return error("invalid_cargo_vendor_part", 400);
  }
  try {
    const body = await request.arrayBuffer();
    if (body.byteLength !== size || await digestHex(body) !== sha256) {
      return error("invalid_cargo_vendor_part_checksum", 400);
    }
    const upload = bucket.resumeMultipartUpload(
      cargoVendorStagingKey(cargoLockBlob, bundleSha256, stagingId),
      uploadId,
    );
    const part = await upload.uploadPart(partNumber, body);
    const etag = part.etag.replace(/^"|"$/g, "").toLowerCase();
    if (part.partNumber !== partNumber || !/^[a-f0-9]{32}$/.test(etag)) {
      return error("cargo_vendor_part_invalid", 503);
    }
    return Response.json({ partNumber, etag, size, sha256 }, noStore());
  } catch (cause) {
    return Response.json({
      error: "cargo_vendor_part_failed",
      detail: boundedError(cause),
    }, { status: 409, ...noStore() });
  }
}

async function completeCargoVendorMultipart(
  request: Request,
  bucket: R2Bucket,
  cargoLockBlob: string,
  bundleSha256: string,
): Promise<Response> {
  const input = await request.json().catch(() => undefined);
  if (
    !record(input) || !hasExactKeys(input, [
      "parts",
      "sha256",
      "size",
      "stagingId",
      "uploadId",
      "version",
    ]) || input.version !== 1 || input.sha256 !== bundleSha256 ||
    typeof input.uploadId !== "string" || input.uploadId.length === 0 ||
    input.uploadId.length > 1_024 ||
    typeof input.stagingId !== "string" || !CLOSE_ID_PATTERN.test(input.stagingId) ||
    typeof input.size !== "number" || !Number.isSafeInteger(input.size) ||
    input.size <= 0 || input.size > MAX_CARGO_VENDOR_BYTES ||
    !Array.isArray(input.parts) ||
    input.parts.length !== Math.ceil(input.size / CARGO_VENDOR_PART_BYTES) ||
    input.parts.length <= 0 || input.parts.length > MAX_CARGO_VENDOR_PARTS ||
    !input.parts.every((part, index) =>
      record(part) && hasExactKeys(part, ["etag", "partNumber"]) &&
      part.partNumber === index + 1 && typeof part.etag === "string" &&
      /^[a-f0-9]{32}$/.test(part.etag)
    )
  ) return error("invalid_cargo_vendor_multipart", 400);
  const key = cargoVendorBundleKey(cargoLockBlob, bundleSha256);
  const stagingKey = cargoVendorStagingKey(
    cargoLockBlob,
    bundleSha256,
    input.stagingId,
  );
  const expectedParts = input.parts as Array<{ partNumber: number; etag: string }>;
  const existing = await bucket.head(key);
  if (existing && !matchesCargoVendor(existing, cargoLockBlob, input.size, bundleSha256)) {
    await cleanupCargoVendorMultipart(bucket, stagingKey, input.uploadId);
    return error("immutable_object_conflict", 409);
  }
  if (existing) {
    await cleanupCargoVendorMultipart(bucket, stagingKey, input.uploadId);
    return Response.json({
      key,
      cargoLockBlob,
      size: input.size,
      sha256: bundleSha256,
      uploaded: false,
    }, noStore());
  }
  let staged: R2Object | null = null;
  try {
    staged = await bucket.resumeMultipartUpload(stagingKey, input.uploadId).complete(expectedParts);
  } catch (cause) {
    staged = await bucket.head(stagingKey);
    if (!staged) {
      const resolved = await bucket.head(key);
      if (resolved && matchesCargoVendor(resolved, cargoLockBlob, input.size, bundleSha256)) {
        return Response.json({
          key,
          cargoLockBlob,
          size: input.size,
          sha256: bundleSha256,
          uploaded: false,
        }, noStore());
      }
      return Response.json({
        error: "cargo_vendor_multipart_failed",
        detail: boundedError(cause),
      }, { status: 409, ...noStore() });
    }
  }
  try {
    if (!matchesCargoVendorStaging(
      staged,
      stagingKey,
      key,
      cargoLockBlob,
      input.size,
      bundleSha256,
    )) return error("immutable_object_conflict", 409);
    const body = await bucket.get(stagingKey);
    if (!body || !matchesCargoVendorStaging(
      body,
      stagingKey,
      key,
      cargoLockBlob,
      input.size,
      bundleSha256,
    )) {
      await body?.body.cancel();
      return error("immutable_object_conflict", 409);
    }
    const published = await bucket.put(key, body.body, {
      onlyIf: { etagDoesNotMatch: "*" },
      sha256: bundleSha256,
      httpMetadata: { contentType: "application/gzip", cacheControl: IMMUTABLE_CACHE },
      customMetadata: {
        sha256: bundleSha256,
        size: String(input.size),
        kind: "cargo-vendor",
        cargoLockBlob,
      },
    });
    const resolved = published ?? await bucket.head(key);
    if (!resolved || !matchesCargoVendor(
      resolved,
      cargoLockBlob,
      input.size,
      bundleSha256,
    )) return error("immutable_object_conflict", 409);
    return Response.json({
      key,
      cargoLockBlob,
      size: input.size,
      sha256: bundleSha256,
      uploaded: published != null,
    }, noStore());
  } catch (cause) {
    const resolved = await bucket.head(key);
    if (resolved && matchesCargoVendor(resolved, cargoLockBlob, input.size, bundleSha256)) {
      return Response.json({
        key,
        cargoLockBlob,
        size: input.size,
        sha256: bundleSha256,
        uploaded: false,
      }, noStore());
    }
    return Response.json({
      error: "cargo_vendor_multipart_failed",
      detail: boundedError(cause),
    }, { status: 409, ...noStore() });
  } finally {
    await bucket.delete(stagingKey).catch(() => undefined);
  }
}

async function abortCargoVendorMultipart(
  request: Request,
  bucket: R2Bucket,
  cargoLockBlob: string,
  bundleSha256: string,
): Promise<Response> {
  const input = await request.json().catch(() => undefined);
  if (
    !record(input) || !hasExactKeys(input, ["stagingId", "uploadId", "version"]) ||
    input.version !== 1 || typeof input.uploadId !== "string" ||
    input.uploadId.length === 0 || input.uploadId.length > 1_024 ||
    typeof input.stagingId !== "string" || !CLOSE_ID_PATTERN.test(input.stagingId)
  ) return error("invalid_cargo_vendor_multipart", 400);
  const stagingKey = cargoVendorStagingKey(
    cargoLockBlob,
    bundleSha256,
    input.stagingId,
  );
  try {
    await bucket.resumeMultipartUpload(
      stagingKey,
      input.uploadId,
    ).abort();
  } catch (cause) {
    if (!await bucket.head(stagingKey)) {
      return new Response(null, { status: 204, headers: { "cache-control": "no-store" } });
    }
    return Response.json({
      error: "cargo_vendor_multipart_abort_failed",
      detail: boundedError(cause),
    }, { status: 409, ...noStore() });
  }
  await bucket.delete(stagingKey).catch(() => undefined);
  return new Response(null, { status: 204, headers: { "cache-control": "no-store" } });
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

async function parsePublishSourceInput(request: Request): Promise<PublishSourceInput | Response> {
  const body = await request.json().catch(() => undefined) as {
    expectedHead?: unknown;
    publication?: unknown;
    reopen?: unknown;
  } | undefined;
  if (
    body == null ||
    !(body.expectedHead === null || isSha1(body.expectedHead)) ||
    !isCiSourcePublication(body.publication) ||
    !validReopen(body.reopen) ||
    (body.reopen !== undefined && body.publication.lane?.type !== "pull_request")
  ) return error("invalid_publication", 400);
  return {
    expectedHead: body.expectedHead,
    publication: body.publication,
    ...(body.reopen === undefined ? {} : { reopen: body.reopen }),
  };
}

async function publishSource(body: PublishSourceInput, env: RequiredCiEnv): Promise<Response> {
  const publication = body.publication;
  const lease = await acquirePublicationLease(env.CI_REPOSITORY, publication);
  if (lease instanceof Response) return lease;
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
    body: JSON.stringify({
      expectedHead: body.expectedHead,
      leaseId: lease.leaseId,
      publication,
      ...(body.reopen === undefined ? {} : { reopen: body.reopen }),
    }),
  });
}

async function acquirePublicationLease(
  namespace: DurableObjectNamespace,
  publication: CiSourcePublication,
): Promise<PublicationLease | Response> {
  const response = await namespace.get(namespace.idFromName("nanocodex")).fetch(
    `https://ci-repository/leases/publication/${publication.head}`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ publication }),
    },
  );
  if (response.status !== 200 && response.status !== 201) return response;
  const value = await response.json().catch(() => undefined);
  const leaseValue = record(value) && hasExactKeys(value, ["lease"])
    ? value.lease
    : undefined;
  const lease = record(leaseValue) && hasExactKeys(leaseValue, [
      "acquiredAt",
      "expiresAt",
      "head",
      "kind",
      "leaseId",
      "version",
    ])
    ? leaseValue as PublicationLease
    : undefined;
  const acquiredAt = lease ? Date.parse(lease.acquiredAt) : Number.NaN;
  const expiresAt = lease ? Date.parse(lease.expiresAt) : Number.NaN;
  const now = Date.now();
  if (
    !lease || lease.version !== 1 || lease.kind !== "publication" ||
    !CLOSE_ID_PATTERN.test(lease.leaseId) || lease.head !== publication.head ||
    !Number.isFinite(acquiredAt) || !Number.isFinite(expiresAt) ||
    acquiredAt > now || expiresAt <= now || expiresAt <= acquiredAt ||
    expiresAt - acquiredAt > MAX_PUBLICATION_LEASE_MS
  ) return error("publication_lease_invalid", 503);
  return lease;
}

type PublicNativeArtifact = {
  name: "nanocodex-x86_64-unknown-linux-gnu" | "nanocodex-aarch64-apple-darwin";
  platform: "x86_64-unknown-linux-gnu" | "aarch64-apple-darwin";
  size: number;
  sha256: string;
  contentType: "application/octet-stream";
  downloadPath: string;
};

type PublicPullRequestManifest = {
  version: 1;
  repository: "gakonst/nanocodex";
  pullRequest: number;
  pullRequestHead: string;
  mergeHead: string;
  workflowId: string;
  completedAt: string;
  artifacts: PublicNativeArtifact[];
  manifestSha256: string;
};

type PublicNpmPreview = {
  kind: "npm-preview";
  packageVersion: string;
  size: number;
  sha256: string;
  contentType: "application/gzip";
  downloadPath: string;
};

async function servePublicPullRequest(
  env: RequiredCiEnv,
  number: number,
): Promise<Response> {
  const lane = await currentPullRequestLane(env, number);
  if (!lane) return error("pull_request_not_found", 404);
  const combined = await runStatus(env, lane.run);
  const workflow = record(combined.workflow) && typeof combined.workflow.status === "string"
    ? { status: combined.workflow.status }
    : { status: "unknown" };
  const result = publicRunResult(combined.result, lane.run);
  let preview: PublicNpmPreview | undefined;
  let native: {
    manifestSha256: string;
    manifestPath: string;
    manifest: PublicPullRequestManifest;
  } | undefined;
  if (workflow.status === "complete" && result?.status === "success") {
    preview = await verifiedNpmPreview(
      env.BACKUP_BUCKET,
      combined.result,
      number,
      lane.pullRequestHead,
      lane.run,
    );
    if (!preview) return error("pull_request_not_found", 404);
    const manifest = await pullRequestManifest(
      env,
      number,
      lane.pullRequestHead,
      lane.run,
      combined.result,
    );
    if (!manifest) return error("pull_request_not_found", 404);
    native = {
      manifestSha256: manifest.manifestSha256,
      manifestPath: pullRequestManifestPath(manifest),
      manifest,
    };
  }
  const current = await currentPullRequestLane(env, number);
  if (
    !current || current.run.head !== lane.run.head ||
    current.pullRequestHead !== lane.pullRequestHead
  ) return error("pull_request_not_found", 404);
  return Response.json({
    version: 1,
    lane: {
      type: "pull_request",
      number,
      branch: `pull/${number}/merge`,
      ref: `refs/pull/${number}/merge`,
      mergeHead: lane.run.head,
      pullRequestHead: lane.pullRequestHead,
    },
    run: {
      version: 1,
      head: lane.run.head,
      state: lane.run.state,
      publishedAt: lane.run.publishedAt,
      workflow,
      result,
    },
    ...(preview ? { preview } : {}),
    ...(native ? { native } : {}),
  }, noStore());
}

async function serveExactPullRequestManifest(
  env: RequiredCiEnv,
  number: number,
  pullRequestHead: string,
  mergeHead: string,
  manifestSha256: string,
): Promise<Response> {
  const manifest = await retainedPullRequestManifest(
    env,
    number,
    pullRequestHead,
    mergeHead,
  );
  if (!manifest || manifest.manifestSha256 !== manifestSha256) {
    return error("pull_request_manifest_not_found", 404);
  }
  return Response.json(manifest, {
    headers: {
      "cache-control": IMMUTABLE_CACHE,
      etag: `"${manifest.manifestSha256}"`,
      "x-content-type-options": "nosniff",
    },
  });
}

async function serveExactPullRequestNpmPreview(
  env: RequiredCiEnv,
  number: number,
  pullRequestHead: string,
  mergeHead: string,
  sha256: string,
  headOnly: boolean,
): Promise<Response> {
  const build = await retainedPullRequestBuild(
    env,
    number,
    pullRequestHead,
    mergeHead,
  );
  const preview = build
    ? await verifiedNpmPreview(
      env.BACKUP_BUCKET,
      build.result,
      number,
      pullRequestHead,
      build.run,
    )
    : undefined;
  if (
    !preview || preview.sha256 !== sha256 ||
    preview.downloadPath !== pullRequestNpmPreviewPath(
      number,
      pullRequestHead,
      mergeHead,
      sha256,
    )
  ) return error("pull_request_preview_not_found", 404);
  const key = `runs/${mergeHead}/artifacts/npm-preview.tgz`;
  const object = headOnly
    ? await env.BACKUP_BUCKET.head(key)
    : await env.BACKUP_BUCKET.get(key);
  if (!object || !matchesNpmPreviewObject(
    object,
    preview,
    number,
    pullRequestHead,
    mergeHead,
  )) {
    if (object && "body" in object) await (object as R2ObjectBody).body.cancel();
    return error("pull_request_preview_not_found", 404);
  }
  return new Response(headOnly ? null : (object as R2ObjectBody).body, {
    headers: {
      "cache-control": IMMUTABLE_CACHE,
      "content-disposition": `attachment; filename="nanocodex-${preview.packageVersion}.tgz"`,
      "content-length": String(preview.size),
      "content-type": preview.contentType,
      etag: object.httpEtag,
      "x-content-type-options": "nosniff",
      "x-nanocodex-package-version": preview.packageVersion,
      "x-nanocodex-sha256": preview.sha256,
    },
  });
}

async function serveExactPullRequestNativeAsset(
  env: RequiredCiEnv,
  number: number,
  pullRequestHead: string,
  mergeHead: string,
  name: string,
  headOnly: boolean,
): Promise<Response> {
  const manifest = await retainedPullRequestManifest(
    env,
    number,
    pullRequestHead,
    mergeHead,
  );
  const artifact = manifest?.artifacts.find((candidate) => candidate.name === name);
  if (!manifest || !artifact || artifact.downloadPath !== pullRequestAssetPath(
    number,
    pullRequestHead,
    mergeHead,
    artifact.name,
  )) return error("pull_request_artifact_not_found", 404);
  const key = `runs/${mergeHead}/artifacts/${artifact.name}`;
  const object = headOnly
    ? await env.BACKUP_BUCKET.head(key)
    : await env.BACKUP_BUCKET.get(key);
  if (!object || !matchesNativeArtifactObject(object, mergeHead, artifact)) {
    if (object && "body" in object) await (object as R2ObjectBody).body.cancel();
    return error("pull_request_artifact_not_found", 404);
  }
  return new Response(headOnly ? null : (object as R2ObjectBody).body, {
    headers: {
      "cache-control": IMMUTABLE_CACHE,
      "content-disposition": `attachment; filename="${artifact.name}"`,
      "content-length": String(artifact.size),
      "content-type": artifact.contentType,
      etag: object.httpEtag,
      "x-content-type-options": "nosniff",
      "x-nanocodex-manifest-sha256": manifest.manifestSha256,
      "x-nanocodex-sha256": artifact.sha256,
    },
  });
}

async function retainedPullRequestManifest(
  env: RequiredCiEnv,
  number: number,
  pullRequestHead: string,
  mergeHead: string,
): Promise<PublicPullRequestManifest | undefined> {
  const build = await retainedPullRequestBuild(
    env,
    number,
    pullRequestHead,
    mergeHead,
  );
  return build
    ? pullRequestManifest(env, number, pullRequestHead, build.run, build.result)
    : undefined;
}

async function retainedPullRequestBuild(
  env: RequiredCiEnv,
  number: number,
  pullRequestHead: string,
  mergeHead: string,
): Promise<{ run: CiRunRecord; result: unknown } | undefined> {
  if (!Number.isSafeInteger(number) || number <= 0 || !isSha1(pullRequestHead) || !isSha1(mergeHead)) {
    return undefined;
  }
  const response = await repository(env).fetch(
    `https://ci-repository/pull-requests/${number}/publications/${mergeHead}`,
  );
  if (!response.ok) {
    await response.body?.cancel();
    return undefined;
  }
  const state = await response.json().catch(() => undefined) as {
    publication?: unknown;
    run?: unknown;
  } | undefined;
  if (!state || !isCiSourcePublication(state.publication)) return undefined;
  const publication = state.publication;
  const lane = publication.lane;
  const run = state.run as CiRunRecord | undefined;
  if (
    lane?.type !== "pull_request" || lane.number !== number ||
    lane.pullRequestHead !== pullRequestHead || publication.head !== mergeHead ||
    !run || run.version !== 1 || run.head !== mergeHead ||
    run.workflowId !== `ci-${mergeHead}` || run.state !== "dispatched"
  ) return undefined;
  const combined = await runStatus(env, run);
  if (!record(combined.workflow) || combined.workflow.status !== "complete") return undefined;
  return { run, result: combined.result };
}

async function pullRequestManifest(
  env: RequiredCiEnv,
  number: number,
  pullRequestHead: string,
  run: CiRunRecord,
  result: unknown,
): Promise<PublicPullRequestManifest | undefined> {
  if (
    !isCiTerminalResult(result, run.head, run.workflowId) ||
    result.status !== "success"
  ) return undefined;
  const artifacts = await verifiedNativeArtifacts(
    env.BACKUP_BUCKET,
    result,
    number,
    pullRequestHead,
    run.head,
  );
  if (!artifacts) return undefined;
  const unsigned = {
    artifacts,
    completedAt: result.completedAt,
    mergeHead: run.head,
    pullRequest: number,
    pullRequestHead,
    repository: "gakonst/nanocodex" as const,
    version: 1 as const,
    workflowId: run.workflowId,
  };
  const manifestSha256 = await digestHex(
    new TextEncoder().encode(canonicalJson(unsigned)).buffer,
  );
  return { ...unsigned, manifestSha256 };
}

async function verifiedNativeArtifacts(
  bucket: R2Bucket,
  result: Record<string, unknown>,
  number: number,
  pullRequestHead: string,
  mergeHead: string,
): Promise<PublicNativeArtifact[] | undefined> {
  if (!Array.isArray(result.artifacts)) return undefined;
  const identities = [
    ["nanocodex-aarch64-apple-darwin", "aarch64-apple-darwin"],
    ["nanocodex-x86_64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"],
  ] as const;
  const artifacts: PublicNativeArtifact[] = [];
  for (const [name, platform] of identities) {
    const key = `runs/${mergeHead}/artifacts/${name}`;
    const matches = result.artifacts.filter((candidate) =>
      record(candidate) && candidate.kind === "native-cli" &&
      candidate.name === name && candidate.platform === platform && candidate.key === key
    );
    if (matches.length !== 1) return undefined;
    const descriptor = matches[0] as Record<string, unknown>;
    if (
      typeof descriptor.size !== "number" || !Number.isSafeInteger(descriptor.size) ||
      descriptor.size <= 0 || descriptor.size > 128 * 1024 * 1024 ||
      !isSha256(descriptor.sha256) || descriptor.contentType !== "application/octet-stream"
    ) return undefined;
    const artifact: PublicNativeArtifact = {
      name,
      platform,
      size: descriptor.size,
      sha256: descriptor.sha256,
      contentType: "application/octet-stream",
      downloadPath: pullRequestAssetPath(number, pullRequestHead, mergeHead, name),
    };
    const object = await bucket.head(key);
    if (!object || !matchesNativeArtifactObject(object, mergeHead, artifact)) return undefined;
    artifacts.push(artifact);
  }
  return artifacts;
}

function matchesNativeArtifactObject(
  object: R2Object,
  mergeHead: string,
  artifact: PublicNativeArtifact,
): boolean {
  return object.key === `runs/${mergeHead}/artifacts/${artifact.name}` &&
    object.size === artifact.size && object.customMetadata?.head === mergeHead &&
    object.customMetadata?.kind === "native-cli" &&
    object.customMetadata?.name === artifact.name &&
    object.customMetadata?.platform === artifact.platform &&
    object.customMetadata?.sha256 === artifact.sha256 &&
    object.checksums.sha256 != null && hex(object.checksums.sha256) === artifact.sha256;
}

function pullRequestManifestPath(manifest: PublicPullRequestManifest): string {
  return `/api/ci/pull-requests/${manifest.pullRequest}/builds/${manifest.pullRequestHead}/${manifest.mergeHead}/manifests/${manifest.manifestSha256}`;
}

function pullRequestAssetPath(
  number: number,
  pullRequestHead: string,
  mergeHead: string,
  name: PublicNativeArtifact["name"],
): string {
  return `/api/ci/pull-requests/${number}/builds/${pullRequestHead}/${mergeHead}/artifacts/${name}`;
}

function pullRequestNpmPreviewPath(
  number: number,
  pullRequestHead: string,
  mergeHead: string,
  sha256: string,
): string {
  return `/api/ci/pull-requests/${number}/builds/${pullRequestHead}/${mergeHead}/artifacts/npm-preview/${sha256}.tgz`;
}

async function currentPullRequestLane(
  env: RequiredCiEnv,
  number: number,
): Promise<{ publication: CiSourcePublication; run: CiRunRecord; pullRequestHead: string } | undefined> {
  const response = await repository(env).fetch(
    `https://ci-repository/pull-requests/${number}/state`,
  );
  if (!response.ok) {
    await response.body?.cancel();
    return undefined;
  }
  const value = await response.json().catch(() => undefined) as {
    publication?: unknown;
    run?: unknown;
  } | undefined;
  if (!value || !isCiSourcePublication(value.publication)) return undefined;
  const publication = value.publication;
  const lane = publication.lane;
  const run = value.run as Partial<CiRunRecord> | undefined;
  if (
    lane?.type !== "pull_request" || lane.number !== number ||
    publication.branch !== `pull/${number}/merge` ||
    publication.ref !== `refs/pull/${number}/merge` ||
    !isSha1(lane.pullRequestHead) ||
    !run || run.version !== 1 || run.head !== publication.head ||
    run.workflowId !== `ci-${publication.head}` ||
    !["pending", "dispatched"].includes(run.state ?? "") ||
    typeof run.publishedAt !== "string" || !Number.isFinite(Date.parse(run.publishedAt))
  ) return undefined;
  return {
    publication,
    run: run as CiRunRecord,
    pullRequestHead: lane.pullRequestHead,
  };
}

function publicRunResult(value: unknown, run: CiRunRecord): {
  version: 1;
  head: string;
  status: "running" | "success" | "failure" | "terminated";
  startedAt?: string;
  completedAt?: string;
  durationMs?: number;
} | null {
  if (!record(value)) return null;
  const terminal = isCiTerminalResult(value, run.head, run.workflowId) ? value : undefined;
  if (!terminal && (
    value.version !== 1 || value.head !== run.head || value.workflowId !== run.workflowId ||
    value.status !== "running"
  )) return null;
  const source = terminal ?? value;
  const result: {
    version: 1;
    head: string;
    status: "running" | "success" | "failure" | "terminated";
    startedAt?: string;
    completedAt?: string;
    durationMs?: number;
  } = {
    version: 1,
    head: run.head,
    status: source.status as "running" | "success" | "failure" | "terminated",
  };
  for (const name of ["startedAt", "completedAt"] as const) {
    if (typeof source[name] === "string" && Number.isFinite(Date.parse(source[name]))) {
      result[name] = source[name];
    }
  }
  if (
    typeof source.durationMs === "number" && Number.isSafeInteger(source.durationMs) &&
    source.durationMs >= 0
  ) result.durationMs = source.durationMs;
  return result;
}

async function verifiedNpmPreview(
  bucket: R2Bucket,
  result: unknown,
  number: number,
  pullRequestHead: string,
  run: CiRunRecord,
): Promise<PublicNpmPreview | undefined> {
  if (
    !isCiTerminalResult(result, run.head, run.workflowId) || result.status !== "success" ||
    run.state !== "dispatched" ||
    !Array.isArray(result.artifacts) || !Number.isSafeInteger(number) || number <= 0 ||
    !isSha1(pullRequestHead)
  ) return undefined;
  const mergeHead = run.head;
  const packageVersion = npmPreviewVersion(mergeHead);
  const key = `runs/${mergeHead}/artifacts/npm-preview.tgz`;
  const matches = result.artifacts.filter((candidate) =>
    record(candidate) && candidate.key === key && candidate.kind === "npm-preview" &&
    candidate.packageVersion === packageVersion && candidate.pullRequest === number &&
    candidate.pullRequestHead === pullRequestHead
  );
  if (matches.length !== 1) return undefined;
  const descriptor = matches[0] as Record<string, unknown>;
  if (
    typeof descriptor.size !== "number" || !Number.isSafeInteger(descriptor.size) ||
    descriptor.size <= 0 || descriptor.size > 16 * 1024 * 1024 ||
    !isSha256(descriptor.sha256) || descriptor.contentType !== "application/gzip"
  ) return undefined;
  const preview: PublicNpmPreview = {
    kind: "npm-preview",
    packageVersion,
    size: descriptor.size,
    sha256: descriptor.sha256,
    contentType: "application/gzip",
    downloadPath: pullRequestNpmPreviewPath(
      number,
      pullRequestHead,
      mergeHead,
      descriptor.sha256,
    ),
  };
  const object = await bucket.head(key);
  if (!object || !matchesNpmPreviewObject(
    object,
    preview,
    number,
    pullRequestHead,
    mergeHead,
  )) return undefined;
  return preview;
}

function matchesNpmPreviewObject(
  object: R2Object,
  preview: PublicNpmPreview,
  number: number,
  pullRequestHead: string,
  mergeHead: string,
): boolean {
  return object.key === `runs/${mergeHead}/artifacts/npm-preview.tgz` &&
    object.size === preview.size && object.customMetadata?.head === mergeHead &&
    object.customMetadata?.kind === "npm-preview" &&
    object.customMetadata?.packageVersion === preview.packageVersion &&
    object.customMetadata?.pullRequest === String(number) &&
    object.customMetadata?.pullRequestHead === pullRequestHead &&
    object.customMetadata?.sha256 === preview.sha256 &&
    object.checksums.sha256 != null && hex(object.checksums.sha256) === preview.sha256;
}

function validReopen(value: unknown): boolean {
  if (value === undefined) return true;
  if (!record(value)) return false;
  return Object.keys(value).length === 3 &&
    typeof value.closeId === "string" && CLOSE_ID_PATTERN.test(value.closeId) &&
    isSha1(value.mergeHead) && isSha1(value.pullRequestHead);
}

async function serveCargoVendor(
  bucket: R2Bucket,
  cargoLockBlob: string,
  bundleSha256: string,
  headOnly: boolean,
): Promise<Response> {
  const key = cargoVendorBundleKey(cargoLockBlob, bundleSha256);
  const object = headOnly ? await bucket.head(key) : await bucket.get(key);
  const size = object?.size;
  const sha256 = object?.customMetadata?.sha256;
  if (
    !object || typeof size !== "number" || !Number.isSafeInteger(size) ||
    size <= 0 || size > MAX_CARGO_VENDOR_BYTES ||
    sha256 !== bundleSha256 ||
    !matchesCargoVendor(object, cargoLockBlob, size, bundleSha256)
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
  const visibleResult = visibleCiRunResult(resultValue, run);
  const progressValue = progress
    ? await progress.json().catch(() => ({ error: "invalid_progress" }))
    : null;
  if (workflowStatus(workflow) === "unknown") {
    const retainedStatus = retainedWorkflowStatus(run, visibleResult);
    if (retainedStatus) {
      workflow = { status: retainedStatus, evidence: "retained_result" };
    }
  }
  const terminated = workflowStatus(workflow) === "terminated";
  return {
    ...run,
    workflow,
    result: terminated ? terminalResult(visibleResult) : visibleResult,
    progress: terminated ? terminalProgress(progressValue) : progressValue,
  };
}

function visibleCiRunResult(value: unknown, run: CiRunRecord): unknown {
  if (value == null || isCiTerminalResult(value, run.head, run.workflowId)) return value;
  if (
    record(value) && value.version === 1 && value.head === run.head &&
    value.workflowId === run.workflowId && value.status === "running"
  ) return value;
  return { error: "invalid_result" };
}

function retainedWorkflowStatus(
  run: CiRunRecord,
  value: unknown,
): "complete" | "errored" | "terminated" | undefined {
  if (!isCiTerminalResult(value, run.head, run.workflowId)) return undefined;
  if (value.status === "success") return "complete";
  if (value.status === "failure") return "errored";
  if (value.status === "terminated") return "terminated";
  return undefined;
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
  const terminal = typeof value.head === "string" && typeof value.workflowId === "string" &&
      isCiTerminalResult(result, value.head, value.workflowId)
    ? result
    : undefined;
  switch (terminal?.status) {
    case "success":
      return { message: "passing", color: "#168a5b" };
    case "failure":
      return { message: "failing", color: "#d14343" };
    case "terminated":
      return { message: "terminated", color: "#6b7280" };
  }
  if (result?.status === "running") return { message: "running", color: "#2563eb" };
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

function hasExactKeys(value: Record<string, unknown>, expected: string[]): boolean {
  const actual = Object.keys(value).sort();
  const wanted = [...expected].sort();
  return actual.length === wanted.length &&
    actual.every((key, index) => key === wanted[index]);
}

function canonicalJson(value: unknown): string {
  if (value === null || typeof value === "string" || typeof value === "boolean") {
    return JSON.stringify(value);
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new Error("canonical JSON rejects non-finite numbers");
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (record(value)) {
    return `{${Object.keys(value).sort().map((key) =>
      `${JSON.stringify(key)}:${canonicalJson(value[key])}`
    ).join(",")}}`;
  }
  throw new Error("canonical JSON rejects unsupported values");
}

async function serveCiArtifact(
  env: RequiredCiEnv,
  head: string,
  kind: "web-dist" | "web-wasm" | "npm-package",
  headOnly: boolean,
): Promise<Response> {
  const run = await repository(env).fetch(`https://ci-repository/runs/${head}`);
  if (!run.ok) return run;
  const extension = kind === "npm-package" ? "tgz" : "tar";
  const contentType = kind === "npm-package" ? "application/gzip" : "application/x-tar";
  const key = `runs/${head}/artifacts/${kind}.${extension}`;
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
      "content-disposition": `attachment; filename="nanocodex-${head}-${kind}.${extension}"`,
      "content-length": String(object.size),
      "content-type": contentType,
      "etag": object.httpEtag,
      "x-content-type-options": "nosniff",
      "x-nanocodex-sha256": sha256,
    },
  });
}

async function serveContentAddressedCiArtifact(
  bucket: R2Bucket,
  kind: "web-wasm" | "npm-package",
  sha256: string,
  headOnly: boolean,
): Promise<Response> {
  const extension = kind === "npm-package" ? "tgz" : "tar";
  const contentType = kind === "npm-package" ? "application/gzip" : "application/x-tar";
  const key = `artifacts/${kind}/${sha256}.${extension}`;
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
      "content-disposition": `attachment; filename="nanocodex-${kind}-${sha256}.${extension}"`,
      "content-length": String(object.size),
      "content-type": contentType,
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

async function serveDistributionEvidence(
  bucket: R2Bucket,
  channel: "stable" | "nightly",
  id: string,
): Promise<Response> {
  const key = distributionResultKey(channel, id);
  const object = await bucket.get(key);
  if (!object || object.size <= 0 || object.size > 1024 * 1024) {
    await object?.body.cancel();
    return error("ci_distribution_missing", 404);
  }
  return new Response(object.body, {
    headers: {
      "cache-control": "no-store",
      "content-length": String(object.size),
      "content-type": "application/json; charset=utf-8",
      "x-content-type-options": "nosniff",
    },
  });
}

async function serveMacRunEvidence(
  bucket: R2Bucket,
  namespace: DurableObjectNamespace,
  head: string,
  file?: string,
): Promise<Response> {
  const id = `macos-native-build-${head}`;
  const response = await macJobs(namespace).fetch(`https://ci-macos/jobs/${id}`);
  if (!response.ok) return error("ci_macos_evidence_missing", response.status === 404 ? 404 : 503);
  const value = await response.json() as {
    job?: {
      state?: unknown;
      queuedAt?: unknown;
      attempts?: unknown;
      completedAt?: unknown;
      result?: {
        outcome?: unknown;
        exitCode?: unknown;
        durationMs?: unknown;
        error?: unknown;
        host?: { platform?: unknown; arch?: unknown };
        logs?: Record<"stdout" | "stderr", {
          key?: unknown;
          size?: unknown;
          sha256?: unknown;
        }>;
      };
    };
  };
  const job = value.job;
  if (!job) return error("ci_macos_evidence_invalid", 503);
  if (!file) {
    const logs = job.result?.logs;
    return Response.json({
      version: 1,
      head,
      state: job.state,
      queuedAt: job.queuedAt,
      attempts: job.attempts,
      completedAt: job.completedAt,
      ...(job.result
        ? {
          result: {
            outcome: job.result.outcome,
            exitCode: job.result.exitCode,
            durationMs: job.result.durationMs,
            ...(job.result.error ? { error: job.result.error } : {}),
            host: job.result.host
              ? {
                platform: job.result.host.platform,
                arch: job.result.host.arch,
              }
              : undefined,
            ...(logs
              ? {
                logs: Object.fromEntries(
                  (["stdout", "stderr"] as const).map((stream) => [stream, {
                    size: logs[stream]?.size,
                    sha256: logs[stream]?.sha256,
                    downloadPath: `/api/ci/runs/${head}/macos/${stream}.log`,
                  }]),
                ),
              }
              : {}),
          },
        }
        : {}),
    }, noStore());
  }
  const stream = file === "stdout.log" ? "stdout" : "stderr";
  const descriptor = job.result?.logs?.[stream];
  if (
    typeof descriptor?.key !== "string" ||
    !new RegExp(
      `^macos/jobs/${id}/attempts/[0-9a-f-]{36}/${stream}\\.log$`,
    ).test(descriptor.key) ||
    typeof descriptor.size !== "number" || !Number.isSafeInteger(descriptor.size) ||
    descriptor.size < 0 || descriptor.size > 64 * 1024 * 1024 ||
    typeof descriptor.sha256 !== "string" || !isSha256(descriptor.sha256)
  ) return error("ci_macos_log_missing", 404);
  const object = await bucket.get(descriptor.key);
  if (
    !object || object.size !== descriptor.size || object.checksums.sha256 == null ||
    hex(object.checksums.sha256) !== descriptor.sha256
  ) {
    await object?.body.cancel();
    return error("ci_macos_log_missing", 404);
  }
  return new Response(object.body, {
    headers: {
      "cache-control": "no-store",
      "content-length": String(object.size),
      "content-type": "text/plain; charset=utf-8",
      "x-content-type-options": "nosniff",
      "x-nanocodex-sha256": descriptor.sha256,
    },
  });
}

async function serveDistributionStep(
  bucket: R2Bucket,
  channel: "stable" | "nightly",
  id: string,
  step: string,
  file: string,
): Promise<Response> {
  const kind = channel === "stable" ? "stable" : "commit";
  const prefix = `distribution/${kind}/${id}`;
  const resultKey = `${prefix}/steps/${step}/result.json`;
  const result = await bucket.get(resultKey);
  if (!result || result.size <= 0 || result.size > 1024 * 1024) {
    await result?.body.cancel();
    return error("ci_distribution_step_missing", 404);
  }
  if (file === "result.json") {
    return new Response(result.body, {
      headers: {
        "cache-control": "no-store",
        "content-length": String(result.size),
        "content-type": "application/json; charset=utf-8",
        "x-content-type-options": "nosniff",
      },
    });
  }
  const metadata = await result.json<{
    stdout?: { key?: string; size?: number };
    stderr?: { key?: string; size?: number };
    result?: {
      logs?: {
        stdout?: { key?: string; size?: number };
        stderr?: { key?: string; size?: number };
      };
    };
  }>().catch(() => undefined);
  const stream = file === "stdout.log" ? "stdout" : "stderr";
  const descriptor = metadata?.[stream] ?? metadata?.result?.logs?.[stream];
  if (
    typeof descriptor?.key !== "string" || !safeDistributionLogKey(descriptor.key, prefix) ||
    typeof descriptor.size !== "number" || !Number.isSafeInteger(descriptor.size) ||
    descriptor.size < 0 || descriptor.size > 64 * 1024 * 1024
  ) return error("ci_distribution_log_missing", 404);
  const log = await bucket.get(descriptor.key);
  if (!log || log.size !== descriptor.size) {
    await log?.body.cancel();
    return error("ci_distribution_log_missing", 404);
  }
  return new Response(log.body, {
    headers: {
      "cache-control": "no-store",
      "content-length": String(log.size),
      "content-type": "text/plain; charset=utf-8",
      "x-content-type-options": "nosniff",
    },
  });
}

function distributionResultKey(channel: "stable" | "nightly", id: string): string {
  return `distribution/${channel === "stable" ? "stable" : "commit"}/${id}/result.json`;
}

function safeDistributionLogKey(key: string, prefix: string): boolean {
  return (
    key.startsWith(`${prefix}/steps/`) ||
    /^runs\/[a-f0-9]{40}\/steps\/[a-z0-9-]{1,80}\/attempts\/[1-9][0-9]*\/(?:stdout|stderr)\.log$/.test(key) ||
    /^macos\/jobs\/macos-[A-Za-z0-9._-]+\/attempts\/[0-9a-f-]{36}\/(?:stdout|stderr)\.log$/.test(key)
  );
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

function macJobs(namespace: DurableObjectNamespace) {
  return namespace.get(namespace.idFromName("nanocodex"));
}

function releases(namespace: DurableObjectNamespace) {
  return namespace.get(namespace.idFromName("nanocodex"));
}

function authenticate(request: Request, token: string | undefined): boolean {
  return Boolean(token && authorized(request, token));
}

function authenticateSource(request: Request, env: CiStorageEnv): SourceAuthentication {
  const master = authenticate(request, env.CI_MASTER_SOURCE_WRITE_TOKEN);
  const pullRequest = authenticate(request, env.CI_PR_SOURCE_WRITE_TOKEN);
  if (master && pullRequest) return { kind: "ambiguous" };
  if (master) return { kind: "master" };
  if (pullRequest) return { kind: "pull_request" };
  return { kind: "none" };
}

function requireSourceCapability(
  authentication: SourceAuthentication,
  allowed: readonly SourceCapability[],
): Response | undefined {
  if (
    (authentication.kind === "master" || authentication.kind === "pull_request") &&
    allowed.includes(authentication.kind)
  ) return undefined;
  return authentication.kind === "none" ? unauthorized() : forbidden();
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
  return object.key === cargoVendorBundleKey(cargoLockBlob, sha256) &&
    object.customMetadata?.kind === "cargo-vendor" &&
    object.customMetadata?.cargoLockBlob === cargoLockBlob &&
    object.size === size && object.customMetadata?.sha256 === sha256 &&
    object.customMetadata?.size === String(size) &&
    object.checksums.sha256 != null && hex(object.checksums.sha256) === sha256;
}

function isR2BadDigest(value: unknown): value is R2Error {
  return value instanceof Error && "code" in value &&
    (value as Partial<R2Error>).code === 10037 && "action" in value &&
    typeof (value as Partial<R2Error>).action === "string";
}

function cargoVendorStagingKey(
  cargoLockBlob: string,
  bundleSha256: string,
  stagingId: string,
): string {
  return `cargo-vendor-staging/${cargoLockBlob}/${bundleSha256}/${stagingId}/bundle.tar.gz`;
}

function matchesCargoVendorStaging(
  object: R2Object,
  stagingKey: string,
  canonicalKey: string,
  cargoLockBlob: string,
  size: number,
  sha256: string,
): boolean {
  return object.key === stagingKey && object.size === size &&
    object.customMetadata?.kind === "cargo-vendor-staging" &&
    object.customMetadata?.canonicalKey === canonicalKey &&
    object.customMetadata?.cargoLockBlob === cargoLockBlob &&
    object.customMetadata?.sha256 === sha256 &&
    object.customMetadata?.size === String(size) &&
    (object.checksums.sha256 == null || hex(object.checksums.sha256) === sha256);
}

async function cleanupCargoVendorMultipart(
  bucket: R2Bucket,
  stagingKey: string,
  uploadId: string,
): Promise<void> {
  await bucket.resumeMultipartUpload(stagingKey, uploadId).abort().catch(() => undefined);
  await bucket.delete(stagingKey).catch(() => undefined);
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

async function digestHex(body: ArrayBuffer): Promise<string> {
  return hex(await crypto.subtle.digest("SHA-256", body));
}

function unauthorized(): Response {
  return Response.json({ error: "unauthorized" }, {
    status: 401,
    headers: { "cache-control": "no-store", "www-authenticate": "Bearer" },
  });
}

function forbidden(): Response {
  return Response.json({ error: "forbidden" }, {
    status: 403,
    headers: { "cache-control": "no-store" },
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
