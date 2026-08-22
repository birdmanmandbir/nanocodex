import { createHash } from "node:crypto";
import { execFile, spawn } from "node:child_process";
import { createReadStream } from "node:fs";
import {
  mkdtemp,
  open,
  readFile,
  rm,
  stat,
  unlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { promisify } from "node:util";

const TAG = "v0.5.0";
const TAG_OBJECT = "9d8e097cd3eeb87e50809e14d54c46978f72a229";
const COMMIT = "e4eea49fc6fab06a98ff01ec8c3da8d9a729eee1";
const TAG_REF = `refs/tags/${TAG}`;
const REPOSITORY_URL = "https://github.com/gakonst/nanocodex.git";
const RELEASE_URL = `https://github.com/gakonst/nanocodex/releases/tag/${TAG}`;
const DOWNLOAD_ROOT = `https://github.com/gakonst/nanocodex/releases/download/${TAG}`;
const IMPORT_PREFIX = `release-import/stable/${TAG}`;
const OWNER = `legacy-import:${TAG}`;
const PUBLIC_RELEASE_ROOT = "/api/releases";
const IMMUTABLE_MANIFEST_PATH = `${PUBLIC_RELEASE_ROOT}/releases/stable/${TAG}`;
const LATEST_PATH = `${PUBLIC_RELEASE_ROOT}/channels/latest`;
const NIGHTLY_PATH = `${PUBLIC_RELEASE_ROOT}/channels/nightly`;
const DRAFT_PATH = `${PUBLIC_RELEASE_ROOT}/drafts/stable/${TAG}`;
const IMMUTABLE_CACHE = "public, max-age=31536000, immutable";
const NO_STORE = "no-store";
const SOURCE_TIMEOUT_MS = 2 * 60_000;
const REQUEST_TIMEOUT_MS = 2 * 60_000;
const TRANSFER_TIMEOUT_MS = 10 * 60_000;
const MAX_JSON_BYTES = 256 * 1024;
const MAX_ERROR_BYTES = 64 * 1024;
const MAX_COMMAND_OUTPUT_BYTES = 64 * 1024;
const MAX_UPLOAD_BYTES = 90 * 1024 * 1024;
const RETRYABLE_STATUS = new Set([408, 425, 429, 500, 502, 503, 504]);
const GITHUB_ASSET_HOST = "release-assets.githubusercontent.com";
const PUBLICATION_LEASE_ID =
  /^(0|[1-9][0-9]{0,15})\.[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;
const execFileAsync = promisify(execFile);

const NAMES = Object.freeze({
  mac: "nanocodex-aarch64-apple-darwin",
  linux: "nanocodex-x86_64-unknown-linux-gnu",
  gzip: "nanocodex-x86_64-unknown-linux-gnu.gz",
  provenance: "PROVENANCE.json",
  checksums: "SHA256SUMS",
});

const PRODUCTION_BINARY_EVIDENCE = deepFreeze({
  mac: {
    size: 79_685_840,
    sha256: "3707e2acbabdf7911eb37ad4c258af1a15ce6c473d5ababdb26841e883f259df",
  },
  linux: {
    size: 88_726_136,
    sha256: "a6920ad69a204e06e2d2fb87af7eacf4b4c84b8b30f054d31f59c7edf40850ae",
  },
  gzip: {
    size: 34_510_818,
    sha256: "89d5232db3d77cc67d0202bb2dede32e4f715af45d7646c6e896745c44ecbf4f",
  },
});
const PRODUCTION_PROFILE = createImportProfile(PRODUCTION_BINARY_EVIDENCE);

assertProductionPins(PRODUCTION_PROFILE);

/**
 * Imports the sole legacy release accepted by the Cloudflare release ledger.
 * There is deliberately no tag, commit, source URL, or asset override here.
 */
export async function importV05Release(options = {}) {
  return executeImport(PRODUCTION_PROFILE, options);
}

async function executeImport(profile, options) {
  assertPinnedProfileIdentity(profile);
  const environment = options.environment ?? process.env;
  const origin = requiredOrigin(environment.CI_PUBLIC_ORIGIN);
  const token = requiredSecret(environment.CI_RELEASE_TOKEN);
  const repository = resolve(options.repository ?? process.cwd());
  const fetchImpl = options.fetch ?? globalThis.fetch;
  if (typeof fetchImpl !== "function") throw new Error("global fetch is unavailable");
  const services = {
    fetch: fetchImpl,
    runGit: options.runGit ?? defaultRunGit,
    runGzip: options.runGzip ?? defaultRunGzip,
    sleep: options.sleep ?? defaultSleep,
  };

  const directory = await mkdtemp(join(tmpdir(), "nanocodex-v0.5-import-"));
  let operationError;
  try {
    // This complete boundary intentionally precedes every request to the release
    // service. A source, tag, derivation, or provenance mismatch cannot create a
    // draft, upload bytes, or acquire the publication lease.
    const paths = await prepareAssets(profile, {
      directory,
      repository,
      services,
    });
    const client = releaseClient({ origin, token, services });
    const state = await inspectLedger(client, profile);
    if (state === "published") {
      await verifyPublicPublication(client, profile);
      return Object.freeze({ tag: TAG, commit: COMMIT, replayed: true });
    }

    const created = await putDraft(client, profile);
    if (created === "published") {
      await verifyPublicPublication(client, profile);
      return Object.freeze({ tag: TAG, commit: COMMIT, replayed: true });
    }
    for (const asset of profile.assets) {
      await uploadAsset(client, profile, asset, paths.get(asset.name));
    }

    const lease = await acquireLease(client);
    let publicationError;
    try {
      const renewed = await heartbeatLease(client, lease);
      await finalizeDraft(client, profile, renewed);
    } catch (error) {
      publicationError = error;
    }

    let releaseError;
    try {
      await releaseLease(client, lease);
    } catch (error) {
      releaseError = error;
    }
    if (publicationError && releaseError) {
      throw new AggregateError(
        [publicationError, releaseError],
        "v0.5.0 finalization and publication lease release both failed",
      );
    }
    if (publicationError) throw publicationError;
    if (releaseError) throw releaseError;

    await verifyPublicPublication(client, profile);
    return Object.freeze({ tag: TAG, commit: COMMIT, replayed: state === "draft" });
  } catch (error) {
    operationError = error;
    throw error;
  } finally {
    try {
      await rm(directory, { recursive: true, force: true });
    } catch (cleanupError) {
      if (!operationError) throw cleanupError;
    }
  }
}

async function prepareAssets(profile, context) {
  const { directory, repository, services } = context;
  await verifyTag(repository, directory, services.runGit);

  const paths = new Map(profile.assets.map((asset) => [
    asset.name,
    join(directory, asset.name),
  ]));
  const cancellation = new AbortController();
  let primaryDownloadError;
  const downloads = await Promise.allSettled(profile.sources.map(async (source) => {
    try {
      await downloadGithubAsset(
        source,
        paths.get(source.name),
        services.fetch,
        cancellation.signal,
      );
    } catch (error) {
      if (!primaryDownloadError) {
        primaryDownloadError = error;
        cancellation.abort(error);
      }
      throw error;
    }
  }));
  if (downloads.some(({ status }) => status === "rejected")) throw primaryDownloadError;

  const checksumBytes = await readFile(paths.get(NAMES.checksums));
  if (!checksumBytes.equals(profile.checksumBytes)) {
    throw new Error("public v0.5.0 SHA256SUMS content changed");
  }

  await services.runGzip(
    ["-n", "-9", "-c", paths.get(NAMES.linux)],
    {
      cwd: directory,
      home: directory,
      outputPath: paths.get(NAMES.gzip),
    },
  );
  await writeFile(paths.get(NAMES.provenance), profile.provenanceBytes, {
    flag: "wx",
    mode: 0o600,
  });

  for (const asset of profile.assets) {
    await verifyFile(paths.get(asset.name), asset);
  }
  return paths;
}

async function verifyTag(repository, cleanDirectory, runGit) {
  const object = trimCommandOutput(await runGit(
    ["rev-parse", "--verify", TAG_REF],
    { cwd: repository, home: cleanDirectory },
  ));
  if (object !== TAG_OBJECT) {
    throw new Error("local v0.5.0 annotated tag object changed");
  }

  const [type, peeled, contents, remote] = await Promise.all([
    runGit(["cat-file", "-t", TAG_OBJECT], { cwd: repository, home: cleanDirectory }),
    runGit(["rev-parse", "--verify", `${TAG_OBJECT}^{}`], {
      cwd: repository,
      home: cleanDirectory,
    }),
    runGit(["cat-file", "-p", TAG_OBJECT], { cwd: repository, home: cleanDirectory }),
    runGit([
      "ls-remote",
      "--tags",
      REPOSITORY_URL,
      TAG_REF,
      `${TAG_REF}^{}`,
    ], { cwd: cleanDirectory, home: cleanDirectory }),
  ]);
  const tagHeader = trimCommandOutput(contents).split("\n\n", 1)[0].split("\n");
  if (
    trimCommandOutput(type) !== "tag" ||
    trimCommandOutput(peeled) !== COMMIT ||
    tagHeader[0] !== `object ${COMMIT}` ||
    tagHeader[1] !== "type commit" ||
    tagHeader[2] !== `tag ${TAG}`
  ) {
    throw new Error("local v0.5.0 tag is not the pinned annotated commit tag");
  }

  const expectedRemote = [
    `${TAG_OBJECT}\t${TAG_REF}`,
    `${COMMIT}\t${TAG_REF}^{}`,
  ].join("\n");
  if (trimCommandOutput(remote) !== expectedRemote) {
    throw new Error("authoritative public v0.5.0 tag identity changed");
  }
  const rechecked = trimCommandOutput(await runGit(
    ["rev-parse", "--verify", TAG_REF],
    { cwd: repository, home: cleanDirectory },
  ));
  if (rechecked !== TAG_OBJECT) throw new Error("local v0.5.0 tag moved during verification");
}

async function downloadGithubAsset(source, destination, fetchImpl, cancellationSignal) {
  const sourceUrl = new URL(source.url);
  if (
    sourceUrl.origin !== "https://github.com" ||
    sourceUrl.pathname !== `/gakonst/nanocodex/releases/download/${TAG}/${source.name}` ||
    sourceUrl.search || sourceUrl.hash || sourceUrl.username || sourceUrl.password
  ) throw new Error(`invalid pinned GitHub source URL for ${source.name}`);

  const headers = new Headers({
    accept: "application/octet-stream",
    "accept-encoding": "identity",
    "user-agent": "nanocodex-v0.5-release-import/1",
  });
  const redirect = await fetchImpl(sourceUrl, {
    method: "GET",
    headers,
    redirect: "manual",
    signal: AbortSignal.any([cancellationSignal, AbortSignal.timeout(SOURCE_TIMEOUT_MS)]),
  });
  if (redirect.status !== 302) {
    throw await responseError(redirect, `resolve public GitHub asset ${source.name}`);
  }
  const location = redirect.headers.get("location");
  await redirect.body?.cancel().catch(() => undefined);
  if (!location) throw new Error(`GitHub omitted the asset redirect for ${source.name}`);
  const target = new URL(location, sourceUrl);
  if (
    target.origin !== `https://${GITHUB_ASSET_HOST}` ||
    target.username || target.password || target.hash ||
    !target.pathname.startsWith("/github-production-release-asset/")
  ) throw new Error(`GitHub returned an untrusted asset redirect for ${source.name}`);

  const response = await fetchImpl(target, {
    method: "GET",
    headers,
    redirect: "error",
    signal: AbortSignal.any([cancellationSignal, AbortSignal.timeout(SOURCE_TIMEOUT_MS)]),
  });
  if (response.status !== 200) {
    throw await responseError(response, `download public GitHub asset ${source.name}`);
  }
  if (
    strictContentLength(response.headers.get("content-length")) !== source.size ||
    response.headers.has("content-encoding") || response.headers.has("content-range")
  ) {
    await response.body?.cancel().catch(() => undefined);
    throw new Error(`public GitHub asset headers changed for ${source.name}`);
  }
  const proof = await streamToFile(response, destination, source.size, source.name);
  if (proof.sha256 !== source.sha256) {
    throw new Error(`public GitHub source hash changed for ${source.name}`);
  }
}

async function streamToFile(response, destination, expectedSize, label) {
  if (!response.body) throw new Error(`${label} returned no body`);
  const file = await open(destination, "wx", 0o600);
  const reader = response.body.getReader();
  const digest = createHash("sha256");
  let size = 0;
  try {
    while (true) {
      const chunk = await reader.read();
      if (chunk.done) break;
      size += chunk.value.byteLength;
      if (size > expectedSize) {
        await reader.cancel().catch(() => undefined);
        throw new Error(`${label} exceeded its pinned size`);
      }
      digest.update(chunk.value);
      let offset = 0;
      while (offset < chunk.value.byteLength) {
        const written = await file.write(
          chunk.value,
          offset,
          chunk.value.byteLength - offset,
        );
        if (written.bytesWritten <= 0) throw new Error(`failed to write ${label}`);
        offset += written.bytesWritten;
      }
    }
  } catch (error) {
    await reader.cancel().catch(() => undefined);
    throw error;
  } finally {
    reader.releaseLock();
    await file.close();
  }
  if (size !== expectedSize) {
    await unlink(destination).catch(() => undefined);
    throw new Error(`${label} did not match its pinned size`);
  }
  return { size, sha256: digest.digest("hex") };
}

async function verifyFile(path, asset) {
  const metadata = await stat(path);
  if (!metadata.isFile() || metadata.size !== asset.size) {
    throw new Error(`prepared v0.5.0 asset size mismatch for ${asset.name}`);
  }
  const digest = createHash("sha256");
  let size = 0;
  for await (const chunk of createReadStream(path)) {
    size += chunk.byteLength;
    if (size > asset.size) throw new Error(`prepared asset exceeded its bound: ${asset.name}`);
    digest.update(chunk);
  }
  if (size !== asset.size || digest.digest("hex") !== asset.sha256) {
    throw new Error(`prepared v0.5.0 asset hash mismatch for ${asset.name}`);
  }
}

function releaseClient({ origin, token, services }) {
  const authorization = `Bearer ${token}`;
  return Object.freeze({
    authorization,
    async fetch(path, initFactory, label, timeout = REQUEST_TIMEOUT_MS) {
      const url = new URL(path, origin);
      if (url.origin !== origin || url.protocol !== "https:") {
        throw new Error("release request escaped the configured HTTPS origin");
      }
      return fetchWithReplay(async () => {
        const init = typeof initFactory === "function" ? initFactory() : initFactory;
        return services.fetch(url, {
          ...init,
          redirect: "error",
          signal: AbortSignal.timeout(timeout),
        });
      }, label, services.sleep);
    },
  });
}

async function inspectLedger(client, profile) {
  const [immutable, latest, nightly, draft] = await Promise.all([
    optionalJson(client, IMMUTABLE_MANIFEST_PATH, { method: "GET" }, "inspect immutable v0.5.0"),
    optionalJson(client, LATEST_PATH, { method: "GET" }, "inspect latest release"),
    optionalJson(client, NIGHTLY_PATH, { method: "GET" }, "inspect nightly release"),
    optionalJson(client, DRAFT_PATH, {
      method: "GET",
      headers: { authorization: client.authorization },
    }, "inspect v0.5.0 draft"),
  ]);

  if (nightly.status !== 404) {
    throw new Error("release ledger is nonempty: nightly already exists");
  }
  const immutableExists = immutable.status === 200;
  const latestExists = latest.status === 200;
  if (immutableExists || latestExists) {
    if (!immutableExists || !latestExists || draft.status !== 404) {
      throw new Error("release ledger contains a partial or conflicting v0.5.0 cutover");
    }
    const manifest = await verifyManifest(immutable.value, profile);
    await verifyLatest(latest.value, manifest, profile);
    return "published";
  }
  if (draft.status === 200) {
    verifyDraftResponse(draft.value, profile);
    return "draft";
  }
  if (draft.status !== 404) throw new Error("release draft inspection failed closed");
  return "empty";
}

async function putDraft(client, profile) {
  const value = await requestJson(
    client,
    DRAFT_PATH,
    () => ({
      method: "PUT",
      headers: {
        authorization: client.authorization,
        "content-length": String(profile.draftBytes.byteLength),
        "content-type": "application/json",
      },
      body: profile.draftBytes,
    }),
    [200, 201],
    "create exact v0.5.0 draft",
  );
  if (record(value) && Object.hasOwn(value, "draft")) {
    verifyDraftResponse(value, profile);
    return "draft";
  }
  await verifyPublicationResponse(value, profile);
  return "published";
}

async function uploadAsset(client, profile, asset, path) {
  if (!path) throw new Error(`prepared asset path missing for ${asset.name}`);
  if (asset.size > MAX_UPLOAD_BYTES) throw new Error(`asset exceeds upload bound: ${asset.name}`);
  const uploadPath = `${DRAFT_PATH}/assets/${encodeURIComponent(asset.name)}`;
  const value = await requestJson(
    client,
    uploadPath,
    () => ({
      method: "PUT",
      headers: {
        authorization: client.authorization,
        "content-length": String(asset.size),
        "content-type": asset.contentType,
        "x-nanocodex-sha256": asset.sha256,
      },
      body: createReadStream(path),
      duplex: "half",
    }),
    [200, 201],
    `upload ${asset.name}`,
    TRANSFER_TIMEOUT_MS,
  );
  if (!exactRecord(value, ["asset", "uploaded"]) || typeof value.uploaded !== "boolean") {
    throw new Error(`upload acknowledgement is invalid for ${asset.name}`);
  }
  verifyPublicAsset(value.asset, asset, `${DRAFT_PATH}/assets`);
}

async function acquireLease(client) {
  const value = await requestJson(
    client,
    `${PUBLIC_RELEASE_ROOT}/publication-lease/acquire`,
    {
      method: "POST",
      headers: {
        authorization: client.authorization,
        "content-type": "application/json",
      },
      body: JSON.stringify({ owner: OWNER, kind: "stable", id: TAG, commit: COMMIT }),
    },
    [200, 201],
    "acquire v0.5.0 publication lease",
  );
  return verifyLease(value);
}

async function heartbeatLease(client, lease) {
  const value = await requestJson(
    client,
    `${PUBLIC_RELEASE_ROOT}/publication-lease/${encodeURIComponent(lease.leaseId)}/heartbeat`,
    {
      method: "POST",
      headers: {
        authorization: client.authorization,
        "content-type": "application/json",
      },
      body: JSON.stringify({ owner: OWNER }),
    },
    [200],
    "renew v0.5.0 publication lease",
  );
  const renewed = verifyLease(value);
  if (renewed.leaseId !== lease.leaseId || renewed.generation !== lease.generation) {
    throw new Error("v0.5.0 publication lease fence changed");
  }
  return renewed;
}

async function finalizeDraft(client, profile, lease) {
  const value = await requestJson(
    client,
    `${DRAFT_PATH}/finalize`,
    {
      method: "POST",
      headers: {
        authorization: client.authorization,
        "x-nanocodex-publication-lease-generation": String(lease.generation),
        "x-nanocodex-publication-lease-id": lease.leaseId,
        "x-nanocodex-publication-lease-owner": lease.owner,
      },
    },
    [200, 201],
    "finalize exact v0.5.0 draft",
  );
  await verifyPublicationResponse(value, profile);
}

async function releaseLease(client, lease) {
  const response = await client.fetch(
    `${PUBLIC_RELEASE_ROOT}/publication-lease/${encodeURIComponent(lease.leaseId)}`,
    {
      method: "DELETE",
      headers: {
        authorization: client.authorization,
        "content-type": "application/json",
      },
      body: JSON.stringify({ owner: OWNER }),
    },
    "release v0.5.0 publication lease",
  );
  if (response.status !== 204) {
    throw await responseError(response, "release v0.5.0 publication lease");
  }
  const body = await boundedBody(response, 1, "release v0.5.0 publication lease");
  if (body.byteLength !== 0) throw new Error("publication lease release returned a body");
}

function verifyLease(value) {
  if (!exactRecord(value, [
    "version",
    "leaseId",
    "owner",
    "kind",
    "id",
    "commit",
    "generation",
    "expiresAt",
  ])) throw new Error("v0.5.0 publication lease response is malformed");
  if (
    value.version !== 1 || value.owner !== OWNER || value.kind !== "stable" ||
    value.id !== TAG || value.commit !== COMMIT ||
    !Number.isSafeInteger(value.generation) || value.generation < 1 ||
    typeof value.leaseId !== "string" || !PUBLICATION_LEASE_ID.test(value.leaseId) ||
    !value.leaseId.startsWith(`${value.generation}.`) ||
    typeof value.expiresAt !== "string" || !canonicalTimestamp(value.expiresAt)
  ) throw new Error("v0.5.0 publication lease identity changed");
  return value;
}

async function verifyPublicPublication(client, profile) {
  const immutableResponse = await requiredJson(
    client,
    IMMUTABLE_MANIFEST_PATH,
    { method: "GET" },
    "refetch immutable v0.5.0 manifest",
  );
  const manifest = await verifyManifest(immutableResponse.value, profile);
  if (
    immutableResponse.headers.get("cache-control") !== IMMUTABLE_CACHE ||
    immutableResponse.headers.get("etag") !== `"${manifest.manifestSha256}"` ||
    immutableResponse.headers.has("content-location")
  ) throw new Error("immutable v0.5.0 manifest headers are invalid");

  const latestResponse = await requiredJson(
    client,
    LATEST_PATH,
    { method: "GET" },
    "refetch latest release",
  );
  if (
    latestResponse.headers.get("cache-control") !== NO_STORE ||
    latestResponse.headers.get("content-location") !== IMMUTABLE_MANIFEST_PATH ||
    latestResponse.headers.has("etag")
  ) throw new Error("latest v0.5.0 response headers are invalid");
  await verifyLatest(latestResponse.value, manifest, profile);

  for (const asset of profile.assets) {
    const immutablePath = `${IMMUTABLE_MANIFEST_PATH}/assets/${encodeURIComponent(asset.name)}`;
    const rollingPath = `${LATEST_PATH}/assets/${encodeURIComponent(asset.name)}`;
    const immutableProof = await verifyPublicAssetBytes(
      client,
      immutablePath,
      asset,
      { cacheControl: IMMUTABLE_CACHE, contentLocation: null },
    );
    const rollingProof = await verifyPublicAssetBytes(
      client,
      rollingPath,
      asset,
      { cacheControl: NO_STORE, contentLocation: immutablePath },
    );
    if (
      immutableProof.size !== rollingProof.size ||
      immutableProof.sha256 !== rollingProof.sha256
    ) throw new Error(`latest bytes differ from immutable bytes for ${asset.name}`);
  }
}

async function verifyPublicAssetBytes(client, path, asset, expectedHeaders) {
  const response = await client.fetch(
    path,
    { method: "GET" },
    `refetch public asset ${asset.name}`,
    TRANSFER_TIMEOUT_MS,
  );
  if (response.status !== 200) {
    throw await responseError(response, `refetch public asset ${asset.name}`);
  }
  const contentLocation = response.headers.get("content-location");
  if (
    strictContentLength(response.headers.get("content-length")) !== asset.size ||
    response.headers.get("content-type") !== asset.contentType ||
    response.headers.get("x-nanocodex-sha256") !== asset.sha256 ||
    response.headers.get("etag") !== `"${asset.sha256}"` ||
    response.headers.get("cache-control") !== expectedHeaders.cacheControl ||
    response.headers.get("content-disposition") !== `attachment; filename="${asset.name}"` ||
    response.headers.get("x-content-type-options") !== "nosniff" ||
    response.headers.get("x-nanocodex-release") !== TAG ||
    contentLocation !== expectedHeaders.contentLocation ||
    response.headers.has("content-encoding") || response.headers.has("content-range")
  ) {
    await response.body?.cancel().catch(() => undefined);
    throw new Error(`public asset headers are invalid for ${asset.name}`);
  }
  const proof = await digestResponse(response, asset.size, asset.name);
  if (proof.sha256 !== asset.sha256) {
    throw new Error(`public asset hash is invalid for ${asset.name}`);
  }
  return proof;
}

async function digestResponse(response, expectedSize, label) {
  if (!response.body) throw new Error(`public asset returned no body: ${label}`);
  const reader = response.body.getReader();
  const digest = createHash("sha256");
  let size = 0;
  try {
    while (true) {
      const chunk = await reader.read();
      if (chunk.done) break;
      size += chunk.value.byteLength;
      if (size > expectedSize) {
        await reader.cancel().catch(() => undefined);
        throw new Error(`public asset exceeded its bound: ${label}`);
      }
      digest.update(chunk.value);
    }
  } finally {
    reader.releaseLock();
  }
  if (size !== expectedSize) throw new Error(`public asset size is invalid for ${label}`);
  return { size, sha256: digest.digest("hex") };
}

async function verifyPublicationResponse(value, profile) {
  if (!exactRecord(value, ["manifest", "pointer"]) || value.pointer == null) {
    throw new Error("v0.5.0 publication response is malformed");
  }
  const manifest = await verifyManifest(value.manifest, profile);
  verifyPointer(value.pointer, manifest);
  return manifest;
}

async function verifyLatest(value, immutable, profile) {
  if (!exactRecord(value, ["pointer", "manifest"])) {
    throw new Error("latest release response is malformed");
  }
  const embedded = await verifyManifest(value.manifest, profile);
  verifyPointer(value.pointer, embedded);
  if (canonicalJson(embedded) !== canonicalJson(immutable)) {
    throw new Error("latest does not embed the immutable v0.5.0 manifest");
  }
}

async function verifyManifest(value, profile) {
  if (!exactRecord(value, [
    "version",
    "kind",
    "id",
    "tag",
    "commit",
    "channel",
    "finalizedAt",
    "assets",
    "manifestSha256",
  ])) throw new Error("immutable v0.5.0 manifest is malformed");
  if (
    value.version !== 1 || value.kind !== "stable" || value.id !== TAG ||
    value.tag !== TAG || value.commit !== COMMIT || value.channel !== "latest" ||
    typeof value.finalizedAt !== "string" || !canonicalTimestamp(value.finalizedAt) ||
    !Array.isArray(value.assets) || value.assets.length !== profile.assets.length ||
    typeof value.manifestSha256 !== "string"
  ) throw new Error("immutable v0.5.0 manifest identity changed");

  for (let index = 0; index < profile.assets.length; index += 1) {
    verifyPublicAsset(value.assets[index], profile.assets[index], `${IMMUTABLE_MANIFEST_PATH}/assets`);
  }
  const { manifestSha256, ...unsigned } = value;
  if (sha256Bytes(Buffer.from(canonicalJson(unsigned))) !== manifestSha256) {
    throw new Error("immutable v0.5.0 manifest digest is invalid");
  }
  return value;
}

function verifyPointer(value, manifest) {
  if (!exactRecord(value, [
    "version",
    "channel",
    "kind",
    "id",
    "tag",
    "commit",
    "generation",
    "updatedAt",
  ])) throw new Error("latest v0.5.0 pointer is malformed");
  if (
    value.version !== 1 || value.channel !== "latest" || value.kind !== "stable" ||
    value.id !== TAG || value.tag !== TAG || value.commit !== COMMIT ||
    value.generation !== 1 || value.updatedAt !== manifest.finalizedAt
  ) throw new Error("latest is not the first exact v0.5.0 publication");
}

function verifyDraftResponse(value, profile) {
  if (!exactRecord(value, ["draft"]) || !exactRecord(value.draft, [
    "version",
    "kind",
    "id",
    "tag",
    "commit",
    "channel",
    "expectedChannel",
    "createdAt",
    "assets",
  ])) throw new Error("v0.5.0 draft response is malformed");
  const draft = value.draft;
  if (
    draft.version !== 1 || draft.kind !== "stable" || draft.id !== TAG ||
    draft.tag !== TAG || draft.commit !== COMMIT || draft.channel !== "latest" ||
    draft.expectedChannel !== null || !canonicalTimestamp(draft.createdAt) ||
    !Array.isArray(draft.assets) || draft.assets.length !== profile.assets.length
  ) throw new Error("v0.5.0 draft identity changed");
  for (let index = 0; index < profile.assets.length; index += 1) {
    verifyPublicAsset(draft.assets[index], profile.assets[index], `${DRAFT_PATH}/assets`);
  }
}

function verifyPublicAsset(value, expected, pathPrefix) {
  if (!exactRecord(value, [
    "name",
    "platform",
    "size",
    "sha256",
    "contentType",
    "downloadPath",
  ])) throw new Error(`release asset descriptor is malformed for ${expected.name}`);
  if (
    value.name !== expected.name || value.platform !== expected.platform ||
    value.size !== expected.size || value.sha256 !== expected.sha256 ||
    value.contentType !== expected.contentType ||
    value.downloadPath !== `${pathPrefix}/${encodeURIComponent(expected.name)}`
  ) throw new Error(`release asset descriptor changed for ${expected.name}`);
}

async function optionalJson(client, path, init, label) {
  const response = await client.fetch(path, init, label);
  if (response.status === 404) {
    await boundedBody(response, MAX_ERROR_BYTES, label);
    return { status: 404 };
  }
  if (response.status !== 200) throw await responseError(response, label);
  return { status: 200, value: await parseJson(response, label) };
}

async function requiredJson(client, path, init, label) {
  const response = await client.fetch(path, init, label);
  if (response.status !== 200) throw await responseError(response, label);
  return { value: await parseJson(response, label), headers: response.headers };
}

async function requestJson(client, path, init, statuses, label, timeout) {
  const response = await client.fetch(path, init, label, timeout);
  if (!statuses.includes(response.status)) throw await responseError(response, label);
  return parseJson(response, label);
}

async function parseJson(response, label) {
  const contentType = response.headers.get("content-type")?.split(";", 1)[0].trim();
  if (contentType !== "application/json") {
    await response.body?.cancel().catch(() => undefined);
    throw new Error(`${label} returned a non-JSON response`);
  }
  const bytes = await boundedBody(response, MAX_JSON_BYTES, label);
  try {
    return JSON.parse(new TextDecoder().decode(bytes));
  } catch {
    throw new Error(`${label} returned invalid JSON`);
  }
}

async function fetchWithReplay(makeRequest, label, sleep) {
  let lastError;
  for (let attempt = 1; attempt <= 3; attempt += 1) {
    let response;
    try {
      response = await makeRequest();
    } catch (error) {
      lastError = error;
      if (attempt === 3) break;
      await sleep(attempt * 100);
      continue;
    }
    if (!RETRYABLE_STATUS.has(response.status) || attempt === 3) return response;
    await response.body?.cancel().catch(() => undefined);
    await sleep(attempt * 100);
  }
  throw new Error(`${label} failed after replay-safe retries`, { cause: lastError });
}

async function responseError(response, label) {
  const bytes = await boundedBody(response, MAX_ERROR_BYTES, label);
  const detail = new TextDecoder().decode(bytes).replace(/[\r\n\t]+/g, " ").slice(0, 512);
  return new Error(`${label} failed with HTTP ${response.status}${detail ? `: ${detail}` : ""}`);
}

async function boundedBody(response, limit, label) {
  const declared = strictContentLength(response.headers.get("content-length"));
  if (declared != null && declared > limit) {
    await response.body?.cancel().catch(() => undefined);
    throw new Error(`${label} returned an oversized body`);
  }
  if (!response.body) return new Uint8Array();
  const reader = response.body.getReader();
  const chunks = [];
  let size = 0;
  try {
    while (true) {
      const chunk = await reader.read();
      if (chunk.done) break;
      size += chunk.value.byteLength;
      if (size > limit) {
        await reader.cancel().catch(() => undefined);
        throw new Error(`${label} returned an oversized body`);
      }
      chunks.push(chunk.value);
    }
  } finally {
    reader.releaseLock();
  }
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return bytes;
}

function createImportProfile({ mac, linux, gzip }) {
  for (const [label, value] of Object.entries({ mac, linux, gzip })) {
    if (
      !Number.isSafeInteger(value.size) || value.size <= 0 ||
      !/^[a-f0-9]{64}$/.test(value.sha256)
    ) throw new Error(`invalid ${label} import evidence`);
  }
  const checksumBytes = Buffer.from(
    `${mac.sha256}  ${NAMES.mac}\n${linux.sha256}  ${NAMES.linux}\n`,
  );
  const checksumSha256 = sha256Bytes(checksumBytes);
  const provenance = {
    version: 1,
    kind: "legacy-release-import",
    release: { tag: TAG, commit: COMMIT, source: RELEASE_URL },
    verification: {
      sourceChecksumFile: NAMES.checksums,
      sourceChecksumFileSha256: checksumSha256,
      sourceAssetsVerified: true,
    },
    derivations: [{
      name: NAMES.gzip,
      source: NAMES.linux,
      sourceSha256: linux.sha256,
      method: "gzip -n -9",
      sha256: gzip.sha256,
    }],
    limitations: [
      "This migration record is not registry-native build provenance.",
      "The v0.5.0 release predates the Linux VM guest artifact and Cloudflare CI evidence bundle.",
    ],
  };
  const provenanceBytes = Buffer.from(`${JSON.stringify(provenance)}\n`);
  const assets = [
    releaseAsset(NAMES.mac, "aarch64-apple-darwin", mac, "application/octet-stream"),
    releaseAsset(NAMES.linux, "x86_64-unknown-linux-gnu", linux, "application/octet-stream"),
    releaseAsset(NAMES.gzip, "x86_64-unknown-linux-gnu", gzip, "application/gzip"),
    releaseAsset(
      NAMES.provenance,
      "linux",
      { size: provenanceBytes.byteLength, sha256: sha256Bytes(provenanceBytes) },
      "application/json",
    ),
    releaseAsset(
      NAMES.checksums,
      "linux",
      { size: checksumBytes.byteLength, sha256: checksumSha256 },
      "text/plain",
    ),
  ];
  const draft = {
    version: 1,
    kind: "stable",
    tag: TAG,
    commit: COMMIT,
    channel: "latest",
    expectedChannel: null,
    assets,
  };
  const draftBytes = Buffer.from(`${JSON.stringify(draft, null, 2)}\n`);
  return deepFreeze({
    tag: TAG,
    tagObject: TAG_OBJECT,
    commit: COMMIT,
    sources: [
      sourceAsset(NAMES.checksums, checksumBytes.byteLength, checksumSha256),
      sourceAsset(NAMES.mac, mac.size, mac.sha256),
      sourceAsset(NAMES.linux, linux.size, linux.sha256),
    ],
    checksumBytes,
    provenance,
    provenanceBytes,
    assets,
    draft,
    draftBytes,
  });
}

function sourceAsset(name, size, sha256) {
  return { name, size, sha256, url: `${DOWNLOAD_ROOT}/${name}` };
}

function releaseAsset(name, platform, proof, contentType) {
  return {
    name,
    platform,
    key: `${IMPORT_PREFIX}/${name}`,
    size: proof.size,
    sha256: proof.sha256,
    contentType,
  };
}

function assertPinnedProfileIdentity(profile) {
  if (
    profile.tag !== TAG || profile.tagObject !== TAG_OBJECT || profile.commit !== COMMIT ||
    !exactRecord(profile.draft, [
      "version",
      "kind",
      "tag",
      "commit",
      "channel",
      "expectedChannel",
      "assets",
    ]) ||
    profile.draft.version !== 1 || profile.draft.kind !== "stable" ||
    profile.draft.tag !== TAG || profile.draft.commit !== COMMIT ||
    profile.draft.channel !== "latest" || profile.draft.expectedChannel !== null
  ) throw new Error("release importer accepts only the pinned annotated v0.5.0 tag");
  const expectedShape = [
    [NAMES.mac, "aarch64-apple-darwin", "application/octet-stream"],
    [NAMES.linux, "x86_64-unknown-linux-gnu", "application/octet-stream"],
    [NAMES.gzip, "x86_64-unknown-linux-gnu", "application/gzip"],
    [NAMES.provenance, "linux", "application/json"],
    [NAMES.checksums, "linux", "text/plain"],
  ];
  if (profile.assets.length !== expectedShape.length) {
    throw new Error("v0.5.0 import must use the exact reduced asset set");
  }
  for (let index = 0; index < expectedShape.length; index += 1) {
    const asset = profile.assets[index];
    const [name, platform, contentType] = expectedShape[index];
    if (
      asset.name !== name || asset.platform !== platform || asset.contentType !== contentType ||
      asset.key !== `${IMPORT_PREFIX}/${name}` || profile.draft.assets[index] !== asset ||
      asset.size > MAX_UPLOAD_BYTES
    ) throw new Error("v0.5.0 import asset shape changed");
  }
}

function assertProductionPins(profile) {
  const expected = {
    checksums: { size: 198, sha256: "aabb9714794b04352d7f20bf47eb0758173ff39a2687924d0cd8beb71dcf3a76" },
    provenance: { size: 830, sha256: "2be2c55774a2072e3ffd42c5a07ceae60011ee23fd6192270bbaca99dce6ad66" },
    draft: { size: 1_747, sha256: "cfb90a5e31a4b3c9ab3943544ca705fe973e4187dbbf3ba6a68d01f86f3613db" },
  };
  const checksums = profile.assets.find((asset) => asset.name === NAMES.checksums);
  const provenance = profile.assets.find((asset) => asset.name === NAMES.provenance);
  if (
    checksums.size !== expected.checksums.size || checksums.sha256 !== expected.checksums.sha256 ||
    provenance.size !== expected.provenance.size || provenance.sha256 !== expected.provenance.sha256 ||
    profile.draftBytes.byteLength !== expected.draft.size ||
    sha256Bytes(profile.draftBytes) !== expected.draft.sha256
  ) throw new Error("source-controlled v0.5.0 migration evidence changed");
}

async function defaultRunGit(args, options) {
  try {
    const result = await execFileAsync("git", args, {
      cwd: options.cwd,
      env: commandEnvironment(options.home),
      encoding: "utf8",
      maxBuffer: MAX_COMMAND_OUTPUT_BYTES,
      timeout: SOURCE_TIMEOUT_MS,
    });
    return result.stdout;
  } catch (cause) {
    throw new Error(`git ${args[0]} failed during v0.5.0 source proof`, { cause });
  }
}

async function defaultRunGzip(args, options) {
  const output = await open(options.outputPath, "wx", 0o600);
  try {
    await new Promise((resolvePromise, rejectPromise) => {
      const child = spawn("gzip", args, {
        cwd: options.cwd,
        env: commandEnvironment(options.home),
        stdio: ["ignore", output.fd, "pipe"],
      });
      const stderr = [];
      let stderrSize = 0;
      let settled = false;
      let timeout;
      const rejectOnce = (error) => {
        if (settled) return;
        settled = true;
        clearTimeout(timeout);
        rejectPromise(error);
      };
      timeout = setTimeout(() => {
        child.kill("SIGKILL");
        rejectOnce(new Error("gzip -n -9 exceeded its time bound"));
      }, TRANSFER_TIMEOUT_MS);
      child.stderr.on("data", (chunk) => {
        stderrSize += chunk.byteLength;
        if (stderrSize > MAX_COMMAND_OUTPUT_BYTES) {
          child.kill("SIGKILL");
          rejectOnce(new Error("gzip emitted oversized diagnostics"));
          return;
        }
        stderr.push(chunk);
      });
      child.once("error", (cause) => rejectOnce(new Error("failed to start gzip", { cause })));
      child.once("close", (code, signal) => {
        if (settled) return;
        settled = true;
        clearTimeout(timeout);
        if (code === 0) {
          resolvePromise();
          return;
        }
        const detail = Buffer.concat(stderr).toString("utf8").trim().slice(0, 512);
        rejectPromise(new Error(
          `gzip -n -9 failed${signal ? ` with ${signal}` : ` with exit ${code}`}${detail ? `: ${detail}` : ""}`,
        ));
      });
    });
  } finally {
    await output.close();
  }
}

function commandEnvironment(home) {
  return {
    PATH: process.env.PATH ?? "/usr/bin:/bin",
    HOME: home,
    LC_ALL: "C",
    TZ: "UTC",
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_NO_REPLACE_OBJECTS: "1",
    GIT_OPTIONAL_LOCKS: "0",
    GIT_TERMINAL_PROMPT: "0",
  };
}

function requiredOrigin(value) {
  if (!value) throw new Error("CI_PUBLIC_ORIGIN is required");
  const url = new URL(value);
  if (
    url.protocol !== "https:" || url.username || url.password || url.search || url.hash ||
    url.pathname !== "/"
  ) throw new Error("CI_PUBLIC_ORIGIN must be an HTTPS origin without credentials or a path");
  return url.origin;
}

function requiredSecret(value) {
  if (!value || /[\r\n]/.test(value)) throw new Error("CI_RELEASE_TOKEN is required");
  return value;
}

function trimCommandOutput(value) {
  if (typeof value !== "string" || Buffer.byteLength(value) > MAX_COMMAND_OUTPUT_BYTES) {
    throw new Error("source proof command returned invalid output");
  }
  return value.replace(/\n$/, "");
}

function strictContentLength(value) {
  if (value == null || !/^(0|[1-9][0-9]*)$/.test(value)) return undefined;
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) ? parsed : undefined;
}

function canonicalTimestamp(value) {
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) && new Date(parsed).toISOString() === value;
}

function canonicalJson(value) {
  if (value === null || typeof value === "boolean" || typeof value === "string") {
    return JSON.stringify(value);
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new TypeError("canonical JSON requires finite numbers");
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (record(value)) {
    return `{${Object.keys(value).sort().map((key) =>
      `${JSON.stringify(key)}:${canonicalJson(value[key])}`
    ).join(",")}}`;
  }
  throw new TypeError("canonical JSON cannot encode this value");
}

function sha256Bytes(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function record(value) {
  return value != null && typeof value === "object" && !Array.isArray(value);
}

function exactRecord(value, keys) {
  if (!record(value)) return false;
  const actual = Object.keys(value);
  return actual.length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}

function deepFreeze(value) {
  if (!value || typeof value !== "object" || Object.isFrozen(value)) return value;
  // Buffers are immutable by convention in this profile and cannot be frozen
  // by V8 while they contain elements.
  if (ArrayBuffer.isView(value)) return value;
  for (const child of Object.values(value)) deepFreeze(child);
  return Object.freeze(value);
}

function defaultSleep(milliseconds) {
  return new Promise((resolvePromise) => setTimeout(resolvePromise, milliseconds));
}

export const __test = Object.freeze({
  canonicalJson,
  createImportProfile,
  executeImport,
  get productionProfile() {
    return createImportProfile(PRODUCTION_BINARY_EVIDENCE);
  },
  sha256Bytes,
});

const main = process.argv[1] &&
  import.meta.url === pathToFileURL(resolve(process.argv[1])).href;
if (main) {
  try {
    if (process.argv.length !== 2) {
      throw new Error("the v0.5.0 release importer accepts no arguments");
    }
    const result = await importV05Release();
    process.stdout.write(
      `Cloudflare latest serves ${result.tag} at ${result.commit}; all public bytes verified\n`,
    );
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    process.stderr.write(`v0.5.0 release import failed: ${message}\n`);
    process.exitCode = 1;
  }
}
