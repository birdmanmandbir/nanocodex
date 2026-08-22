import { sha256 } from "@noble/hashes/sha2";

import { ciSourceLane, isCiSourcePublication } from "./ciSource.ts";
import type {
  CiReleaseCommitReservation,
  CiReleaseCommitReservationProof,
  CiRunRecord,
} from "./ciRepository.ts";

const SHA1 = /^[a-f0-9]{40}$(?![\s\S])/;
const SHA256 = /^[a-f0-9]{64}$(?![\s\S])/;
const ASSET_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,159}$(?![\s\S])/;
const CONTENT_TYPE = /^[A-Za-z0-9][A-Za-z0-9!#$&^_.+-]*\/[A-Za-z0-9][A-Za-z0-9!#$&^_.+-]*(?:;[\t ]*[A-Za-z0-9_-]+=[A-Za-z0-9._-]+)*$(?![\s\S])/;
const MAX_ASSETS = 64;
const MAX_ASSET_BYTES = 256 * 1024 * 1024;
const MAX_DRAFT_ASSET_UPLOAD_BYTES = 90 * 1024 * 1024;
const PUBLICATION_LEASE_TTL_MS = 120_000;
const INVALID_PUBLICATION_LEASE_OWNER_CHARACTER = /[^A-Za-z0-9._:@/-]/;
const PUBLICATION_LEASE_ID =
  /^(0|[1-9][0-9]{0,15})\.[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$(?![\s\S])/;
const PUBLICATION_LEASE_KEY = "publication-lease:active";
const PUBLICATION_LEASE_GENERATION_KEY = "publication-lease:generation";
const RELEASE_AUTHORITY_ATTEMPTS = 2;
const STABLE_BOOTSTRAP_ID = "v0.5.0";
const STABLE_BOOTSTRAP_COMMIT = "e4eea49fc6fab06a98ff01ec8c3da8d9a729eee1";
const DRAFT_RETENTION_MS = 30 * 24 * 60 * 60 * 1_000;
const DRAFT_GC_RETRY_MS = 5 * 60 * 1_000;
const DRAFT_GC_PREFIX = "draft-gc:";
const RELEASE_STAGING_RETENTION_MS = 7 * 24 * 60 * 60 * 1_000;
const RELEASE_STAGING_RETRY_MS = 5 * 60 * 1_000;
const RELEASE_STAGING_PREFIX = "release-staging:";
const RELEASE_STAGING_ID = /^[a-f0-9]{64}$(?![\s\S])/;
const RELEASE_STAGING_FENCE_ID =
  /^(0|[1-9][0-9]{0,15})\.[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$(?![\s\S])/;
const MAX_RELEASE_STAGING_KEYS = 16;
const PUBLIC_RELEASES_PATH = "/api/releases";
const IMMUTABLE_CACHE = "public, max-age=31536000, immutable";
const NO_STORE = "no-store";

export type CiReleaseKind = "stable" | "commit";
export type CiReleaseChannel = "latest" | "nightly";
export type CiReleasePlatform =
  | "x86_64-unknown-linux-gnu"
  | "aarch64-apple-darwin"
  | "x86_64-unknown-linux-musl"
  | "linux"
  | "npm";

export type CiReleaseAsset = {
  name: string;
  platform: CiReleasePlatform;
  key: string;
  size: number;
  sha256: string;
  contentType: string;
};

export type CiReleaseAssetPromotion = {
  kind: CiReleaseKind;
  id: string;
  commit: string;
  component: "linux" | "npm" | "macos";
  source: {
    key: string;
    size: number;
    sha256: string;
    contentType: string;
    customMetadata: Readonly<Record<string, string>>;
  };
  asset: CiReleaseAsset;
};

export type CiReleaseDraft = {
  version: 1;
  kind: CiReleaseKind;
  tag: string;
  commit: string;
  channel: CiReleaseChannel;
  expectedChannel: string | null;
  assets: CiReleaseAsset[];
};

type CiReleaseStagingBase = {
  version: 1;
  kind: CiReleaseKind;
  id: string;
  commit: string;
  stageId: string;
  fenceId: string;
  generation: number;
  keys: string[];
  createdAt: string;
  expiresAt: string;
  nextAttemptAt: string;
  attempts: number;
};

export type CiReleaseStaging = CiReleaseStagingBase & (
  | { state: "active" }
  | { state: "collecting"; claimId: string; claimedAt: string }
);

export type CiReleaseStagingFence = Pick<
  CiReleaseStaging,
  "fenceId" | "generation"
>;

export type CiReleaseManifest = {
  version: 1;
  kind: CiReleaseKind;
  id: string;
  tag: string;
  commit: string;
  channel: CiReleaseChannel;
  finalizedAt: string;
  manifestSha256: string;
  assets: CiReleaseAsset[];
};

export type CiReleasePointer = {
  version: 1;
  channel: CiReleaseChannel;
  kind: CiReleaseKind;
  id: string;
  tag: string;
  commit: string;
  generation: number;
  updatedAt: string;
};

export type CiPublicationLease = {
  version: 1;
  leaseId: string;
  owner: string;
  kind: CiReleaseKind;
  id: string;
  commit: string;
  generation: number;
  expiresAt: string;
};

export type CiReleasesEnv = {
  BACKUP_BUCKET: R2Bucket;
  CI_REPOSITORY?: DurableObjectNamespace;
  CI_RELEASE_TOKEN?: string;
};

type StoredDraft = CiReleaseDraft & {
  id: string;
  createdAt: string;
  expiresAt: string;
};

type DraftGcRecord = {
  version: 1;
  kind: CiReleaseKind;
  id: string;
  assets: CiReleaseAsset[];
  claimedAt: string;
  nextAttemptAt: string;
  attempts: number;
};

type ReleaseIdentity = {
  kind: CiReleaseKind;
  id: string;
};

type ReleaseStagingInput = Pick<
  CiReleaseStaging,
  "version" | "kind" | "id" | "commit" | "stageId" | "keys"
>;

type PublicationLeaseIdentity = Pick<
  CiPublicationLease,
  "owner" | "kind" | "id" | "commit"
>;

type PublicationLeaseFence = Pick<
  CiPublicationLease,
  "leaseId" | "owner" | "generation"
>;

type ReleaseCommitReservationRequest = {
  version: 1;
  owner: string;
  releaseKind: CiReleaseKind;
  releaseId: string;
  commit: string;
  publicationLeaseId: string;
  publicationLeaseGeneration: number;
};

type CreateOutcome =
  | { status: "created" | "replayed" | "rebased"; draft: StoredDraft }
  | { status: "released"; manifest: CiReleaseManifest }
  | { status: "conflict"; error: string };

type FinalizeOutcome =
  | { status: "published" | "replayed"; manifest: CiReleaseManifest; pointer?: CiReleasePointer }
  | { status: "conflict"; error: string; current?: CiReleasePointer };

type AcquirePublicationLeaseOutcome =
  | { status: "created" | "replayed"; lease: CiPublicationLease }
  | { status: "conflict" };

/** Copies a verified transient build output into its immutable release namespace. */
export async function promoteCiReleaseAsset(
  bucket: R2Bucket,
  promotion: CiReleaseAssetPromotion,
): Promise<CiReleaseAsset> {
  const identity = releaseIdentity(promotion.kind, promotion.id);
  const { source, asset, component } = promotion;
  if (
    !identity || !SHA1.test(promotion.commit) ||
    (identity.kind === "commit" && identity.id !== promotion.commit) ||
    !normalizeAsset(asset) ||
    asset.key !== `${releaseObjectPrefix(identity)}/components/${component}/${asset.name}` ||
    (component === "npm"
      ? asset.platform !== "npm"
      : component === "macos"
      ? asset.platform !== "aarch64-apple-darwin"
      : asset.platform !== "x86_64-unknown-linux-gnu" &&
        asset.platform !== "x86_64-unknown-linux-musl") ||
    !safeObjectKey(source.key) || source.key === asset.key ||
    source.size !== asset.size || source.sha256 !== asset.sha256 ||
    source.contentType !== asset.contentType ||
    Object.keys(source.customMetadata).some((key) =>
      key.length === 0 || typeof source.customMetadata[key] !== "string"
    )
  ) throw new Error(`Invalid ${component} release promotion for ${promotion.id}`);

  const retainedMetadata = promotionMetadata(promotion);
  const existing = await bucket.head(asset.key);
  if (existing) {
    if (matchesPromotedAsset(existing, asset, retainedMetadata)) return asset;
    throw new Error(`Immutable release asset conflicts at ${asset.key}`);
  }

  const transient = await bucket.get(source.key);
  if (!transient) {
    throw new Error(`Transient release asset is missing or invalid at ${source.key}`);
  }
  if (!matchesSourceAsset(transient, source)) {
    await transient.body.cancel().catch(() => undefined);
    throw new Error(`Transient release asset is missing or invalid at ${source.key}`);
  }

  let created: R2Object | null;
  try {
    created = await bucket.put(asset.key, transient.body, {
      onlyIf: { etagDoesNotMatch: "*" },
      sha256: asset.sha256,
      httpMetadata: { contentType: asset.contentType },
      customMetadata: retainedMetadata,
    });
  } catch (cause) {
    throw new Error(`Failed to promote immutable release asset at ${asset.key}`, { cause });
  }
  const retained = created ?? await bucket.head(asset.key);
  if (!matchesPromotedAsset(retained, asset, retainedMetadata)) {
    throw new Error(`Failed to retain immutable release asset at ${asset.key}`);
  }
  return asset;
}

/** Owns release publication state. Integration code may forward these HTTP routes unchanged. */
export class CiReleases {
  readonly #state: DurableObjectState;
  readonly #env: CiReleasesEnv;

  constructor(state: DurableObjectState, env: CiReleasesEnv) {
    this.#state = state;
    this.#env = env;
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/publication-lease/acquire" && request.method === "POST") {
      if (!authenticated(request, this.#env.CI_RELEASE_TOKEN)) return unauthorized();
      const value: unknown = await request.json().catch(() => undefined);
      const identity = publicationLeaseIdentity(value);
      return identity
        ? this.#acquirePublicationLease(identity)
        : error("invalid_publication_lease", 400);
    }

    const heartbeatLease = route(
      url.pathname,
      /^\/publication-lease\/([^/]+)\/heartbeat$/,
    );
    if (heartbeatLease && request.method === "POST") {
      if (!authenticated(request, this.#env.CI_RELEASE_TOKEN)) return unauthorized();
      const leaseId = publicationLeaseId(heartbeatLease[1]);
      const value: unknown = await request.json().catch(() => undefined);
      const owner = publicationLeaseOwner(value);
      return leaseId && owner
        ? this.#heartbeatPublicationLease(leaseId, owner)
        : error("invalid_publication_lease", 400);
    }

    const releaseLease = route(url.pathname, /^\/publication-lease\/([^/]+)$/);
    if (releaseLease && request.method === "DELETE") {
      if (!authenticated(request, this.#env.CI_RELEASE_TOKEN)) return unauthorized();
      const leaseId = publicationLeaseId(releaseLease[1]);
      const value: unknown = await request.json().catch(() => undefined);
      const owner = publicationLeaseOwner(value);
      return leaseId && owner
        ? this.#releasePublicationLease(leaseId, owner)
        : error("invalid_publication_lease", 400);
    }

    const staging = route(
      url.pathname,
      /^\/staging\/(stable|commit)\/([^/]+)\/([a-f0-9]{64})$/,
    );
    if (staging && (request.method === "PUT" || request.method === "DELETE")) {
      if (!authenticated(request, this.#env.CI_RELEASE_TOKEN)) return unauthorized();
      const identity = releaseIdentity(staging[1], staging[2]);
      const stageId = staging[3]!;
      if (!identity || !RELEASE_STAGING_ID.test(stageId)) {
        return error("invalid_release_staging", 400);
      }
      const value: unknown = await request.json().catch(() => undefined);
      if (request.method === "PUT") {
        const input = normalizeReleaseStaging(value, identity, stageId);
        return input
          ? this.#registerReleaseStaging(input)
          : error("invalid_release_staging", 400);
      }
      return exactRecord(value, ["commit", "fenceId", "generation"]) &&
          typeof value.commit === "string" && SHA1.test(value.commit) &&
          typeof value.fenceId === "string" &&
          RELEASE_STAGING_FENCE_ID.test(value.fenceId) &&
          typeof value.generation === "number" &&
          Number.isSafeInteger(value.generation) && value.generation > 0 &&
          value.fenceId.split(".", 1)[0] === String(value.generation)
        ? this.#deleteReleaseStaging(identity, stageId, value.commit, {
          fenceId: value.fenceId,
          generation: value.generation,
        })
        : error("invalid_release_staging", 400);
    }

    const draftAsset = route(
      url.pathname,
      /^\/drafts\/(stable|commit)\/([^/]+)\/assets\/([^/]+)$/,
    );
    if (
      draftAsset &&
      (request.method === "GET" || request.method === "HEAD" || request.method === "PUT")
    ) {
      if (!authenticated(request, this.#env.CI_RELEASE_TOKEN)) return unauthorized();
      const identity = releaseIdentity(draftAsset[1], draftAsset[2]);
      const name = decoded(draftAsset[3]);
      if (!identity || !name || !ASSET_NAME.test(name)) {
        if (request.method === "PUT") await cancelBody(request.body);
        return error("invalid_release_asset", 400);
      }
      const stored = await this.#state.storage.get<StoredDraft>(draftKey(identity));
      if (!stored) {
        if (request.method === "PUT") await cancelBody(request.body);
        return error("release_draft_not_found", 404);
      }
      if (request.method === "PUT") return this.#uploadDraftAsset(request, stored, name);
      return this.#serveDraftAsset(request.method === "HEAD", stored, name);
    }

    const finalize = route(
      url.pathname,
      /^\/drafts\/(stable|commit)\/([^/]+)\/finalize$/,
    );
    if (finalize && request.method === "POST") {
      if (!authenticated(request, this.#env.CI_RELEASE_TOKEN)) return unauthorized();
      const identity = releaseIdentity(finalize[1], finalize[2]);
      if (!identity) return error("invalid_release_identity", 400);
      const fence = publicationLeaseFence(request);
      if (!fence) return error("invalid_publication_lease", 400);
      return this.#finalize(identity, fence);
    }

    const readDraft = route(url.pathname, /^\/drafts\/(stable|commit)\/([^/]+)$/);
    if (readDraft && (request.method === "GET" || request.method === "HEAD")) {
      if (!authenticated(request, this.#env.CI_RELEASE_TOKEN)) return unauthorized();
      const identity = releaseIdentity(readDraft[1], readDraft[2]);
      if (!identity) return error("invalid_release_identity", 400);
      const stored = await this.#state.storage.get<StoredDraft>(draftKey(identity));
      return stored
        ? draftResponse(stored, request.method === "HEAD")
        : error("release_draft_not_found", 404);
    }

    const draft = route(url.pathname, /^\/drafts\/(stable|commit)\/([^/]+)$/);
    if (draft && request.method === "PUT") {
      if (!authenticated(request, this.#env.CI_RELEASE_TOKEN)) return unauthorized();
      const identity = releaseIdentity(draft[1], draft[2]);
      if (!identity) return error("invalid_release_identity", 400);
      const value: unknown = await request.json().catch(() => undefined);
      const normalized = normalizeDraft(value, identity);
      return normalized
        ? this.#createDraft(identity, normalized)
        : error("invalid_release_draft", 400);
    }

    const channelAsset = route(
      url.pathname,
      /^\/channels\/(latest|nightly)\/assets\/([^/]+)$/,
    );
    if (channelAsset && (request.method === "GET" || request.method === "HEAD")) {
      const channel = channelAsset[1] as CiReleaseChannel;
      const name = decoded(channelAsset[2]);
      if (!name || !ASSET_NAME.test(name)) return error("invalid_release_asset", 400);
      const resolved = await this.#resolveChannel(channel);
      if (resolved instanceof Response) return resolved;
      return this.#serveAsset(
        request.method === "HEAD",
        resolved.manifest,
        name,
        channel,
      );
    }

    const immutableAsset = route(
      url.pathname,
      /^\/releases\/(stable|commit)\/([^/]+)\/assets\/([^/]+)$/,
    );
    if (immutableAsset && (request.method === "GET" || request.method === "HEAD")) {
      const identity = releaseIdentity(immutableAsset[1], immutableAsset[2]);
      const name = decoded(immutableAsset[3]);
      if (!identity || !name || !ASSET_NAME.test(name)) {
        return error("invalid_release_asset", 400);
      }
      const manifest = await this.#state.storage.get<CiReleaseManifest>(releaseKey(identity));
      if (!manifest) return error("release_not_found", 404);
      return this.#serveAsset(request.method === "HEAD", manifest, name);
    }

    const immutableRelease = route(url.pathname, /^\/releases\/(stable|commit)\/([^/]+)$/);
    if (
      immutableRelease &&
      (request.method === "GET" || request.method === "HEAD")
    ) {
      const identity = releaseIdentity(immutableRelease[1], immutableRelease[2]);
      if (!identity) return error("invalid_release_identity", 400);
      const manifest = await this.#state.storage.get<CiReleaseManifest>(releaseKey(identity));
      return manifest
        ? manifestResponse(manifest, request.method === "HEAD")
        : error("release_not_found", 404);
    }

    const channel = route(url.pathname, /^\/channels\/(latest|nightly)$/);
    if (channel && (request.method === "GET" || request.method === "HEAD")) {
      const resolved = await this.#resolveChannel(channel[1] as CiReleaseChannel);
      return resolved instanceof Response
        ? resolved
        : channelResponse(resolved.pointer, resolved.manifest, request.method === "HEAD");
    }

    return error("not_found", 404);
  }

  async alarm(): Promise<void> {
    const now = Date.now();
    const staging = await this.#state.storage.list<CiReleaseStaging>({
      prefix: RELEASE_STAGING_PREFIX,
    });
    for (const [key, record] of staging) {
      if (!validReleaseStagingRecord(record, key)) {
        throw new Error("Release staging contains invalid durable state");
      }
    }
    const dueStaging = [...staging.entries()]
      .filter(([, record]) => Date.parse(record.nextAttemptAt) <= now)
      .sort(([, left], [, right]) =>
        left.nextAttemptAt.localeCompare(right.nextAttemptAt) ||
        left.stageId.localeCompare(right.stageId)
      )[0];
    if (dueStaging) {
      await this.#collectReleaseStaging(dueStaging[0], dueStaging[1], now);
      await this.#scheduleNextAlarm();
      return;
    }
    const retained = await this.#state.storage.list<DraftGcRecord>({
      prefix: DRAFT_GC_PREFIX,
    });
    const dueRetention = [...retained.entries()]
      .filter(([, record]) => Date.parse(record.nextAttemptAt) <= now)
      .sort(([, left], [, right]) =>
        left.nextAttemptAt.localeCompare(right.nextAttemptAt) ||
        left.id.localeCompare(right.id)
      )[0];
    if (dueRetention) {
      await this.#collectDraftAssets(dueRetention[0], dueRetention[1], now);
      await this.#scheduleNextAlarm();
      return;
    }

    const drafts = await this.#state.storage.list<StoredDraft>({ prefix: "draft:" });
    const dueDraft = [...drafts.entries()]
      .filter(([, draft]) => Date.parse(draft.expiresAt) <= now)
      .sort(([, left], [, right]) =>
        left.expiresAt.localeCompare(right.expiresAt) || left.id.localeCompare(right.id)
      )[0];
    if (dueDraft) await this.#claimExpiredDraft(dueDraft[0], dueDraft[1], now);
    await this.#scheduleNextAlarm();
  }

  async #acquirePublicationLease(identity: PublicationLeaseIdentity): Promise<Response> {
    if (identity.kind === "commit") {
      const active = await this.#state.storage.get<CiPublicationLease>(PUBLICATION_LEASE_KEY);
      if (!active || !publicationLeaseActive(active, Date.now()) ||
        !samePublicationLeaseIdentity(active, identity)) {
        const authoritative = await this.#requireAuthoritativeCommit(identity.commit);
        if (authoritative) return authoritative;
      }
    }
    const nonce = crypto.randomUUID();
    const outcome = await this.#state.storage.transaction<AcquirePublicationLeaseOutcome>(
      async (transaction) => {
        const now = Date.now();
        const expiresAt = publicationLeaseExpiry(now);
        const active = await transaction.get<CiPublicationLease>(PUBLICATION_LEASE_KEY);
        if (active && publicationLeaseActive(active, now)) {
          if (!samePublicationLeaseIdentity(active, identity)) return { status: "conflict" };
          const renewed = { ...active, expiresAt };
          await transaction.put(PUBLICATION_LEASE_KEY, renewed);
          return { status: "replayed", lease: renewed };
        }

        const storedGeneration = await transaction.get<number>(
          PUBLICATION_LEASE_GENERATION_KEY,
        );
        const generation = Math.max(storedGeneration ?? 0, active?.generation ?? 0) + 1;
        if (!Number.isSafeInteger(generation)) return { status: "conflict" };
        const lease: CiPublicationLease = {
          version: 1,
          leaseId: `${generation}.${nonce}`,
          ...identity,
          generation,
          expiresAt,
        };
        await transaction.put(PUBLICATION_LEASE_KEY, lease);
        await transaction.put(PUBLICATION_LEASE_GENERATION_KEY, generation);
        return { status: "created", lease };
      },
    );

    return outcome.status === "conflict"
      ? error("publication_lease_conflict", 409)
      : json(outcome.lease, outcome.status === "created" ? 201 : 200);
  }

  async #heartbeatPublicationLease(leaseId: string, owner: string): Promise<Response> {
    const renewed = await this.#state.storage.transaction<CiPublicationLease | undefined>(
      async (transaction) => {
        const now = Date.now();
        const expiresAt = publicationLeaseExpiry(now);
        const active = await transaction.get<CiPublicationLease>(PUBLICATION_LEASE_KEY);
        if (
          !active || !publicationLeaseActive(active, now) || active.leaseId !== leaseId ||
          active.owner !== owner
        ) return undefined;
        const lease = { ...active, expiresAt };
        await transaction.put(PUBLICATION_LEASE_KEY, lease);
        return lease;
      },
    );
    return renewed ? json(renewed) : error("publication_lease_not_held", 409);
  }

  async #releasePublicationLease(leaseId: string, owner: string): Promise<Response> {
    const outcome = await this.#state.storage.transaction<"released" | "mismatch">(
      async (transaction) => {
        const active = await transaction.get<CiPublicationLease>(PUBLICATION_LEASE_KEY);
        if (!active) return "released";
        if (active.leaseId !== leaseId) return "released";
        if (active.owner !== owner) return "mismatch";
        await transaction.delete(PUBLICATION_LEASE_KEY);
        return "released";
      },
    );
    return outcome === "mismatch"
      ? error("publication_lease_not_held", 409)
      : empty(204);
  }

  async #registerReleaseStaging(input: ReleaseStagingInput): Promise<Response> {
    const now = Date.now();
    const createdAt = new Date(now).toISOString();
    const expiresAt = new Date(now + RELEASE_STAGING_RETENTION_MS).toISOString();
    const nonce = crypto.randomUUID();
    const candidate = {
      ...input,
      fenceId: `1.${nonce}`,
      generation: 1,
      createdAt,
      expiresAt,
      nextAttemptAt: expiresAt,
      attempts: 0,
      state: "active",
    } satisfies CiReleaseStaging;
    const key = releaseStagingKey(input);
    const outcome = await this.#state.storage.transaction(async (transaction) => {
      const existing = await transaction.get<CiReleaseStaging>(key);
      if (existing) {
        if (!validReleaseStagingRecord(existing, key)) return { type: "invalid" as const };
        if (!sameReleaseStaging(existing, input)) return { type: "conflict" as const };
        if (existing.state === "collecting") return { type: "collecting" as const };
        const generation = existing.generation + 1;
        if (!Number.isSafeInteger(generation)) return { type: "invalid" as const };
        // A replay may be a new Workflow attempt following a lost response.
        // Supersede its cleanup fence before that attempt can write any bytes.
        const renewed = {
          ...existing,
          fenceId: `${generation}.${nonce}`,
          generation,
          createdAt,
          expiresAt,
          nextAttemptAt: expiresAt,
          attempts: 0,
        } satisfies CiReleaseStaging;
        await transaction.put(key, renewed);
        return { type: "replayed" as const, record: renewed };
      }
      await transaction.put(key, candidate);
      return { type: "created" as const, record: candidate };
    });
    if (outcome.type === "invalid") return error("release_staging_state_invalid", 503);
    if (outcome.type === "conflict") return error("release_staging_conflict", 409);
    if (outcome.type === "collecting") return error("release_staging_collecting", 409);
    await this.#scheduleNextAlarm();
    return json({
      staging: outcome.record,
      fence: releaseStagingFence(outcome.record),
    }, outcome.type === "created" ? 201 : 200);
  }

  async #deleteReleaseStaging(
    identity: ReleaseIdentity,
    stageId: string,
    commit: string,
    fence: CiReleaseStagingFence,
  ): Promise<Response> {
    const key = releaseStagingKey({ ...identity, stageId });
    const observed = await this.#state.storage.get<CiReleaseStaging>(key);
    if (!observed) return empty(204);
    if (!validReleaseStagingRecord(observed, key)) {
      return error("release_staging_state_invalid", 503);
    }
    if (observed.commit !== commit) return error("release_staging_conflict", 409);
    if (!sameReleaseStagingFence(observed, fence)) return empty(204);
    const collected = await this.#collectReleaseStaging(key, observed, Date.now(), true);
    await this.#scheduleNextAlarm();
    if (collected) return empty(204);
    const current = await this.#state.storage.get<CiReleaseStaging>(key);
    if (!current) return empty(204);
    if (!validReleaseStagingRecord(current, key)) {
      return error("release_staging_state_invalid", 503);
    }
    return sameReleaseStagingFence(current, fence)
      ? error("release_staging_cleanup_failed", 503)
      : empty(204);
  }

  async #collectReleaseStaging(
    key: string,
    observed: CiReleaseStaging,
    now: number,
    force = false,
  ): Promise<boolean> {
    const claimId = crypto.randomUUID();
    const claimed = await this.#state.storage.transaction<CiReleaseStaging | undefined>(
      async (transaction) => {
        const current = await transaction.get<CiReleaseStaging>(key);
        if (!current || !validReleaseStagingRecord(current, key)) return undefined;
        if (!sameReleaseStaging(current, observed)) return undefined;
        if (force && !sameReleaseStagingFence(current, observed)) return undefined;
        if (!force && JSON.stringify(current) !== JSON.stringify(observed)) return undefined;
        if (!force && Date.parse(current.nextAttemptAt) > now) return undefined;
        const collecting = {
          ...current,
          state: "collecting",
          claimId,
          claimedAt: new Date(now).toISOString(),
          nextAttemptAt: new Date(now + RELEASE_STAGING_RETRY_MS).toISOString(),
        } satisfies CiReleaseStaging;
        await transaction.put(key, collecting);
        return collecting;
      },
    );
    if (!claimed || claimed.state !== "collecting") return false;
    try {
      await this.#env.BACKUP_BUCKET.delete(claimed.keys);
      await this.#state.storage.transaction(async (transaction) => {
        const current = await transaction.get<CiReleaseStaging>(key);
        if (current?.state === "collecting" && current.claimId === claimId) {
          await transaction.delete(key);
        }
      });
      return true;
    } catch {
      await this.#state.storage.transaction(async (transaction) => {
        const current = await transaction.get<CiReleaseStaging>(key);
        if (current?.state !== "collecting" || current.claimId !== claimId) return;
        await transaction.put(key, {
          ...current,
          attempts: current.attempts + 1,
          nextAttemptAt: new Date(now + RELEASE_STAGING_RETRY_MS).toISOString(),
        } satisfies CiReleaseStaging);
      });
      return false;
    }
  }

  async #createDraft(
    identity: ReleaseIdentity,
    input: CiReleaseDraft,
  ): Promise<Response> {
    const now = Date.now();
    const candidate: StoredDraft = {
      ...input,
      id: identity.id,
      createdAt: new Date(now).toISOString(),
      expiresAt: new Date(now + DRAFT_RETENTION_MS).toISOString(),
    };
    const outcome = await this.#state.storage.transaction<CreateOutcome>(async (transaction) => {
      const [existingRelease, collecting] = await Promise.all([
        transaction.get<CiReleaseManifest>(releaseKey(identity)),
        transaction.get<DraftGcRecord>(draftGcKey(identity)),
      ]);
      if (existingRelease) {
        return sameReleaseInput(existingRelease, input)
          ? { status: "released", manifest: existingRelease }
          : { status: "conflict", error: "immutable_release_conflict" };
      }
      if (collecting) return { status: "conflict", error: "release_draft_collecting" };
      const key = draftKey(identity);
      const existingDraft = await transaction.get<StoredDraft>(key);
      if (existingDraft) {
        if (sameDraft(existingDraft, input)) {
          const replayed = { ...existingDraft, expiresAt: candidate.expiresAt };
          await transaction.put(key, replayed);
          return { status: "replayed", draft: replayed };
        }
        if (sameDraftIdentity(existingDraft, input)) {
          const rebased = {
            ...existingDraft,
            expectedChannel: input.expectedChannel,
            expiresAt: candidate.expiresAt,
          };
          await transaction.put(key, rebased);
          return { status: "rebased", draft: rebased };
        }
        return { status: "conflict", error: "immutable_draft_conflict" };
      }
      await transaction.put(key, candidate);
      return { status: "created", draft: candidate };
    });

    if (outcome.status === "conflict") return error(outcome.error, 409);
    if (outcome.status === "released") {
      const pointer = await this.#state.storage.get<CiReleasePointer>(
        channelKey(outcome.manifest.channel),
      );
      return publicationResponse(outcome.manifest, matchingPointer(pointer, outcome.manifest), 200);
    }
    await this.#scheduleNextAlarm();
    return draftResponse(
      outcome.draft,
      false,
      outcome.status === "created" ? 201 : 200,
    );
  }

  async #finalize(
    identity: ReleaseIdentity,
    fence: PublicationLeaseFence,
  ): Promise<Response> {
    const existing = await this.#state.storage.get<CiReleaseManifest>(releaseKey(identity));
    if (existing) {
      const pointer = await this.#state.storage.get<CiReleasePointer>(channelKey(existing.channel));
      return publicationResponse(existing, matchingPointer(pointer, existing), 200);
    }
    const observed = await this.#state.storage.get<StoredDraft>(draftKey(identity));
    if (!observed) return error("release_draft_not_found", 404);

    if (identity.kind === "commit") {
      const authoritative = await this.#requireAuthoritativeCommit(identity.id);
      if (authoritative) return authoritative;
    }

    const invalidAssets = await invalidReleaseAssets(this.#env.BACKUP_BUCKET, observed.assets);
    if (invalidAssets.length > 0) {
      return json({ error: "release_assets_invalid", assets: invalidAssets }, 409);
    }
    if (identity.kind === "commit") {
      const authoritative = await this.#requireAuthoritativeCommit(identity.id);
      if (authoritative) return authoritative;
    }

    const finalizedAt = new Date().toISOString();
    const unsigned: Omit<CiReleaseManifest, "manifestSha256"> = {
      version: 1 as const,
      kind: observed.kind,
      id: observed.id,
      tag: observed.tag,
      commit: observed.commit,
      channel: observed.channel,
      finalizedAt,
      assets: observed.assets,
    };
    const candidate: CiReleaseManifest = {
      ...unsigned,
      manifestSha256: await jsonSha256(publicUnsignedManifest(unsigned)),
    };
    // Distributed lock order is fixed: hold and renew the one global
    // CI_RELEASES publication lease before acquiring CI_REPOSITORY authority.
    const leaseHeld = await this.#renewPublicationLeaseFence(
      identity,
      observed.commit,
      fence,
    );
    if (!leaseHeld) return error("publication_lease_not_held", 409);
    const preflight = await this.#preflightFinalization(identity, observed, fence);
    if (preflight) {
      if (preflight.status === "conflict") {
        return json({
          error: preflight.error,
          ...(preflight.current ? { current: preflight.current } : {}),
        }, 409);
      }
      return publicationResponse(preflight.manifest, preflight.pointer, 200);
    }

    let outcome: FinalizeOutcome;
    if (requiresReleaseCommitReservation(identity, observed.commit)) {
      const acquired = await this.#acquireReleaseCommitReservation(
        identity,
        observed.commit,
        fence,
      );
      if (acquired instanceof Response) return acquired;
      let reservation = acquired;
      try {
        const heartbeated = await this.#heartbeatReleaseCommitReservation(reservation);
        if (heartbeated instanceof Response) return heartbeated;
        reservation = heartbeated;
        outcome = await this.#commitFinalization(
          identity,
          observed,
          candidate,
          finalizedAt,
          fence,
        );
      } finally {
        await this.#releaseReleaseCommitReservation(reservation).catch((cause) => {
          // The bounded repository reservation remains reclaimable. A release
          // already committed in this Durable Object stays authoritative.
          console.error("Failed to release CI repository release-commit reservation", cause);
        });
      }
    } else {
      outcome = await this.#commitFinalization(
        identity,
        observed,
        candidate,
        finalizedAt,
        fence,
      );
    }

    if (outcome.status === "conflict") {
      return json({ error: outcome.error, ...(outcome.current ? { current: outcome.current } : {}) }, 409);
    }
    return publicationResponse(
      outcome.manifest,
      outcome.pointer,
      outcome.status === "published" ? 201 : 200,
    );
  }

  async #renewPublicationLeaseFence(
    identity: ReleaseIdentity,
    commit: string,
    fence: PublicationLeaseFence,
  ): Promise<boolean> {
    return this.#state.storage.transaction(async (transaction) => {
      const now = Date.now();
      const active = await transaction.get<CiPublicationLease>(PUBLICATION_LEASE_KEY);
      if (
        !active || !publicationLeaseActive(active, now) ||
        active.leaseId !== fence.leaseId || active.owner !== fence.owner ||
        active.generation !== fence.generation || active.kind !== identity.kind ||
        active.id !== identity.id || active.commit !== commit
      ) return false;
      await transaction.put(PUBLICATION_LEASE_KEY, {
        ...active,
        expiresAt: publicationLeaseExpiry(now),
      } satisfies CiPublicationLease);
      return true;
    });
  }

  async #commitFinalization(
    identity: ReleaseIdentity,
    observed: StoredDraft,
    candidate: CiReleaseManifest,
    finalizedAt: string,
    fence: PublicationLeaseFence,
  ): Promise<FinalizeOutcome> {
    return this.#state.storage.transaction<FinalizeOutcome>(async (transaction) => {
      const published = await transaction.get<CiReleaseManifest>(releaseKey(identity));
      if (published) {
        const pointer = await transaction.get<CiReleasePointer>(channelKey(published.channel));
        return {
          status: "replayed",
          manifest: published,
          pointer: matchingPointer(pointer, published),
        };
      }
      const active = await transaction.get<CiPublicationLease>(PUBLICATION_LEASE_KEY);
      if (
        !active || !publicationLeaseActive(active, Date.now()) ||
        active.leaseId !== fence.leaseId || active.owner !== fence.owner ||
        active.generation !== fence.generation || active.kind !== identity.kind ||
        active.id !== identity.id || active.commit !== observed.commit
      ) return { status: "conflict", error: "publication_lease_not_held" };
      const currentDraft = await transaction.get<StoredDraft>(draftKey(identity));
      if (!currentDraft || JSON.stringify(currentDraft) !== JSON.stringify(observed)) {
        return { status: "conflict", error: "release_draft_changed" };
      }
      const pointerKey = channelKey(observed.channel);
      const current = await transaction.get<CiReleasePointer>(pointerKey);
      const conflict = finalizationPolicyConflict(observed, current);
      if (conflict) return conflict;
      const pointer: CiReleasePointer = {
        version: 1,
        channel: observed.channel,
        kind: observed.kind,
        id: observed.id,
        tag: observed.tag,
        commit: observed.commit,
        generation: (current?.generation ?? 0) + 1,
        updatedAt: finalizedAt,
      };
      await transaction.put(releaseKey(identity), candidate);
      await transaction.put(pointerKey, pointer);
      await transaction.delete(draftKey(identity));
      return { status: "published", manifest: candidate, pointer };
    });
  }

  async #preflightFinalization(
    identity: ReleaseIdentity,
    observed: StoredDraft,
    fence: PublicationLeaseFence,
  ): Promise<FinalizeOutcome | undefined> {
    return this.#state.storage.transaction(async (transaction) => {
      const published = await transaction.get<CiReleaseManifest>(releaseKey(identity));
      if (published) {
        const pointer = await transaction.get<CiReleasePointer>(channelKey(published.channel));
        return {
          status: "replayed",
          manifest: published,
          pointer: matchingPointer(pointer, published),
        };
      }
      const active = await transaction.get<CiPublicationLease>(PUBLICATION_LEASE_KEY);
      if (
        !active || !publicationLeaseActive(active, Date.now()) ||
        active.leaseId !== fence.leaseId || active.owner !== fence.owner ||
        active.generation !== fence.generation || active.kind !== identity.kind ||
        active.id !== identity.id || active.commit !== observed.commit
      ) return { status: "conflict", error: "publication_lease_not_held" };
      const currentDraft = await transaction.get<StoredDraft>(draftKey(identity));
      if (!currentDraft || JSON.stringify(currentDraft) !== JSON.stringify(observed)) {
        return { status: "conflict", error: "release_draft_changed" };
      }
      const current = await transaction.get<CiReleasePointer>(channelKey(observed.channel));
      return finalizationPolicyConflict(observed, current);
    });
  }

  async #acquireReleaseCommitReservation(
    identity: ReleaseIdentity,
    commit: string,
    fence: PublicationLeaseFence,
  ): Promise<CiReleaseCommitReservation | Response> {
    const request = {
      version: 1,
      owner: fence.owner,
      releaseKind: identity.kind,
      releaseId: identity.id,
      commit,
      publicationLeaseId: fence.leaseId,
      publicationLeaseGeneration: fence.generation,
    } satisfies ReleaseCommitReservationRequest;
    const response = await this.#repositoryRequest(
      "/reservations/release-commit/acquire",
      "POST",
      request,
    );
    if (!response) return error("release_authority_unavailable", 503);
    if (!response.ok) return repositoryReservationError(response);
    const value: unknown = await response.json().catch(() => undefined);
    return validReleaseCommitReservationProof(value, request)
      ? value.reservation
      : error("release_authority_invalid", 503);
  }

  async #heartbeatReleaseCommitReservation(
    reservation: CiReleaseCommitReservation,
  ): Promise<CiReleaseCommitReservation | Response> {
    const response = await this.#repositoryRequest(
      `/reservations/release-commit/${encodeURIComponent(reservation.reservationId)}/heartbeat`,
      "POST",
      { owner: reservation.owner, generation: reservation.generation },
    );
    if (!response) return error("release_authority_unavailable", 503);
    if (!response.ok) return repositoryReservationError(response);
    const value: unknown = await response.json().catch(() => undefined);
    if (
      !exactRecord(value, ["reservation"]) ||
      !validReleaseCommitReservation(value.reservation, reservation)
    ) return error("release_authority_invalid", 503);
    return value.reservation;
  }

  async #releaseReleaseCommitReservation(
    reservation: CiReleaseCommitReservation,
  ): Promise<void> {
    const response = await this.#repositoryRequest(
      `/reservations/release-commit/${encodeURIComponent(reservation.reservationId)}`,
      "DELETE",
      { owner: reservation.owner, generation: reservation.generation },
    );
    if (!response) throw new Error("CI repository release-commit reservation is unavailable");
    if (!response.ok) {
      const detail = await response.text().catch(() => "");
      throw new Error(
        `CI repository release-commit reservation release failed (${response.status}): ${detail}`,
      );
    }
    await response.body?.cancel().catch(() => undefined);
  }

  async #repositoryRequest(
    path: string,
    method: "POST" | "DELETE",
    body: unknown,
  ): Promise<Response | undefined> {
    const repository = this.#env.CI_REPOSITORY;
    if (!repository) return undefined;
    const stub = repository.get(repository.idFromName("nanocodex"));
    const encoded = JSON.stringify(body);
    for (let attempt = 0; attempt < RELEASE_AUTHORITY_ATTEMPTS; attempt += 1) {
      try {
        return await stub.fetch(`https://ci-repository${path}`, {
          method,
          headers: { "content-type": "application/json" },
          body: encoded,
        });
      } catch {
        if (attempt + 1 === RELEASE_AUTHORITY_ATTEMPTS) return undefined;
      }
    }
    return undefined;
  }

  async #requireAuthoritativeCommit(commit: string): Promise<Response | undefined> {
    const repository = this.#env.CI_REPOSITORY;
    if (!repository) return error("release_authority_not_configured", 503);
    let response: Response;
    try {
      response = await repository.get(repository.idFromName("nanocodex")).fetch(
        "https://ci-repository/state",
      );
    } catch {
      return error("release_authority_unavailable", 503);
    }
    if (!response.ok) {
      await response.body?.cancel().catch(() => undefined);
      return error("release_authority_unavailable", 503);
    }
    const value: unknown = await response.json().catch(() => undefined);
    if (!record(value)) return error("release_authority_invalid", 503);
    const publication = value.publication;
    const run = value.run;
    if (
      !isCiSourcePublication(publication) || ciSourceLane(publication).type !== "master" ||
      !record(run) || run.version !== 1 || run.head !== publication.head ||
      run.workflowId !== `ci-${publication.head}` || run.state !== "dispatched"
    ) return error("release_authority_invalid", 503);
    return publication.head === commit ? undefined : error("release_head_stale", 409);
  }

  async #claimExpiredDraft(
    key: string,
    observed: StoredDraft,
    now: number,
  ): Promise<void> {
    const identity = { kind: observed.kind, id: observed.id } satisfies ReleaseIdentity;
    const claimed = await this.#state.storage.transaction<DraftGcRecord | undefined>(
      async (transaction) => {
        const [current, release, active] = await Promise.all([
          transaction.get<StoredDraft>(key),
          transaction.get<CiReleaseManifest>(releaseKey(identity)),
          transaction.get<CiPublicationLease>(PUBLICATION_LEASE_KEY),
        ]);
        if (!current || JSON.stringify(current) !== JSON.stringify(observed)) return undefined;
        if (release) {
          await transaction.delete(key);
          return undefined;
        }
        if (Date.parse(current.expiresAt) > now) return undefined;
        if (
          active && publicationLeaseActive(active, now) && active.kind === identity.kind &&
          active.id === identity.id && active.commit === current.commit
        ) {
          await transaction.put(key, { ...current, expiresAt: active.expiresAt });
          return undefined;
        }
        const claimedAt = new Date(now).toISOString();
        const retention: DraftGcRecord = {
          version: 1,
          kind: identity.kind,
          id: identity.id,
          assets: current.assets,
          claimedAt,
          nextAttemptAt: claimedAt,
          attempts: 0,
        };
        await transaction.put(draftGcKey(identity), retention);
        await transaction.delete(key);
        return retention;
      },
    );
    if (claimed) await this.#collectDraftAssets(draftGcKey(identity), claimed, now);
  }

  async #collectDraftAssets(
    key: string,
    observed: DraftGcRecord,
    now: number,
  ): Promise<void> {
    const identity = { kind: observed.kind, id: observed.id } satisfies ReleaseIdentity;
    const release = await this.#state.storage.get<CiReleaseManifest>(releaseKey(identity));
    if (release) {
      await this.#state.storage.delete(key);
      return;
    }
    try {
      await this.#env.BACKUP_BUCKET.delete(observed.assets.map(({ key }) => key));
      await this.#state.storage.transaction(async (transaction) => {
        const current = await transaction.get<DraftGcRecord>(key);
        if (current && JSON.stringify(current) === JSON.stringify(observed)) {
          await transaction.delete(key);
        }
      });
    } catch {
      await this.#state.storage.transaction(async (transaction) => {
        const current = await transaction.get<DraftGcRecord>(key);
        if (!current || JSON.stringify(current) !== JSON.stringify(observed)) return;
        await transaction.put(key, {
          ...current,
          attempts: current.attempts + 1,
          nextAttemptAt: new Date(now + DRAFT_GC_RETRY_MS).toISOString(),
        });
      });
    }
  }

  async #scheduleNextAlarm(): Promise<void> {
    const [drafts, retained, staging] = await Promise.all([
      this.#state.storage.list<StoredDraft>({ prefix: "draft:" }),
      this.#state.storage.list<DraftGcRecord>({ prefix: DRAFT_GC_PREFIX }),
      this.#state.storage.list<CiReleaseStaging>({ prefix: RELEASE_STAGING_PREFIX }),
    ]);
    for (const [key, record] of staging) {
      if (!validReleaseStagingRecord(record, key)) {
        throw new Error("Release staging contains invalid durable state");
      }
    }
    const candidates = [
      ...[...drafts.values()].map(({ expiresAt }) => Date.parse(expiresAt)),
      ...[...retained.values()].map(({ nextAttemptAt }) => Date.parse(nextAttemptAt)),
      ...[...staging.values()].map(({ nextAttemptAt }) => Date.parse(nextAttemptAt)),
    ].filter(Number.isFinite);
    if (candidates.length > 0) {
      await this.#state.storage.setAlarm(Math.max(Date.now(), Math.min(...candidates)));
    }
  }

  async #resolveChannel(channel: CiReleaseChannel): Promise<
    { pointer: CiReleasePointer; manifest: CiReleaseManifest } | Response
  > {
    const pointer = await this.#state.storage.get<CiReleasePointer>(channelKey(channel));
    if (!pointer) return error("release_channel_not_found", 404);
    const identity = { kind: pointer.kind, id: pointer.id } satisfies ReleaseIdentity;
    const manifest = await this.#state.storage.get<CiReleaseManifest>(releaseKey(identity));
    if (
      !manifest || manifest.channel !== channel || manifest.tag !== pointer.tag ||
      manifest.commit !== pointer.commit
    ) return error("release_channel_invalid", 503);
    return { pointer, manifest };
  }

  async #serveAsset(
    headOnly: boolean,
    manifest: CiReleaseManifest,
    name: string,
    rollingChannel?: CiReleaseChannel,
  ): Promise<Response> {
    const asset = manifest.assets.find((candidate) => candidate.name === name);
    if (!asset) return error("release_asset_not_found", 404);
    const immutablePath = assetPath(manifest, asset.name);
    return this.#serveStoredAsset(
      headOnly,
      asset,
      manifest.id,
      rollingChannel ? NO_STORE : IMMUTABLE_CACHE,
      rollingChannel ? immutablePath : undefined,
    );
  }

  async #serveDraftAsset(
    headOnly: boolean,
    draft: StoredDraft,
    name: string,
  ): Promise<Response> {
    const asset = draft.assets.find((candidate) => candidate.name === name);
    if (!asset) return error("release_asset_not_found", 404);
    return this.#serveStoredAsset(
      headOnly,
      asset,
      draft.id,
      NO_STORE,
      draftAssetPath(draft, asset.name),
    );
  }

  async #uploadDraftAsset(
    request: Request,
    observed: StoredDraft,
    name: string,
  ): Promise<Response> {
    const asset = observed.assets.find((candidate) => candidate.name === name);
    if (!asset) {
      await cancelBody(request.body);
      return error("release_asset_not_found", 404);
    }
    if (request.body == null) return error("invalid_release_asset_upload", 400);
    const contentLength = strictContentLength(request.headers.get("content-length"));
    if (
      contentLength !== asset.size ||
      request.headers.get("content-type") !== asset.contentType ||
      request.headers.get("x-nanocodex-sha256") !== asset.sha256 ||
      request.headers.has("content-encoding") || request.headers.has("content-range")
    ) {
      await cancelBody(request.body);
      return error("invalid_release_asset_upload", 400);
    }
    if (asset.size > MAX_DRAFT_ASSET_UPLOAD_BYTES) {
      await cancelBody(request.body);
      return error("release_asset_upload_too_large", 413);
    }

    const draft = await this.#state.storage.transaction<StoredDraft | "collecting" | undefined>(
      async (transaction) => {
        const identity = { kind: observed.kind, id: observed.id } satisfies ReleaseIdentity;
        const [current, collecting] = await Promise.all([
          transaction.get<StoredDraft>(draftKey(identity)),
          transaction.get<DraftGcRecord>(draftGcKey(identity)),
        ]);
        if (collecting) return "collecting";
        if (!current || JSON.stringify(current) !== JSON.stringify(observed)) return undefined;
        const renewed = {
          ...current,
          expiresAt: new Date(Date.now() + DRAFT_RETENTION_MS).toISOString(),
        };
        await transaction.put(draftKey(identity), renewed);
        return renewed;
      },
    );
    if (draft === "collecting") {
      await cancelBody(request.body);
      return error("release_draft_collecting", 409);
    }
    if (!draft) {
      await cancelBody(request.body);
      return error("release_draft_changed", 409);
    }

    const existing = await this.#env.BACKUP_BUCKET.head(asset.key);
    if (existing) {
      if (!matchesAsset(existing, asset)) {
        await cancelBody(request.body);
        return error("immutable_release_asset_conflict", 409);
      }
      if (!await bodyMatchesSha256(request.body, asset.size, asset.sha256)) {
        return error("immutable_release_asset_conflict", 409);
      }
      const verified = await this.#env.BACKUP_BUCKET.head(asset.key);
      return matchesAsset(verified, asset)
        ? json({ asset: publicDraftAsset(draft, asset), uploaded: false })
        : error("immutable_release_asset_conflict", 409);
    }

    let uploaded: R2Object | null;
    try {
      uploaded = await this.#env.BACKUP_BUCKET.put(asset.key, request.body, {
        onlyIf: { etagDoesNotMatch: "*" },
        sha256: asset.sha256,
        httpMetadata: { contentType: asset.contentType },
      });
    } catch {
      return error("immutable_release_asset_conflict", 409);
    }
    if (!uploaded) return error("immutable_release_asset_conflict", 409);
    const verified = await this.#env.BACKUP_BUCKET.head(asset.key);
    if (!matchesAsset(verified, asset)) {
      return error("immutable_release_asset_conflict", 409);
    }
    return json({ asset: publicDraftAsset(draft, asset), uploaded: true }, 201);
  }

  async #serveStoredAsset(
    headOnly: boolean,
    asset: CiReleaseAsset,
    releaseId: string,
    cacheControl: string,
    contentLocation?: string,
  ): Promise<Response> {
    const object = headOnly
      ? await this.#env.BACKUP_BUCKET.head(asset.key)
      : await this.#env.BACKUP_BUCKET.get(asset.key);
    if (!matchesAsset(object, asset)) {
      if (object && "body" in object) await (object as R2ObjectBody).body.cancel();
      return error("release_asset_unavailable", 503);
    }
    const headers = new Headers({
      "cache-control": cacheControl,
      "content-disposition": `attachment; filename="${asset.name}"`,
      "content-length": String(asset.size),
      "content-type": asset.contentType,
      "etag": `"${asset.sha256}"`,
      "x-content-type-options": "nosniff",
      "x-nanocodex-release": releaseId,
      "x-nanocodex-sha256": asset.sha256,
    });
    if (contentLocation) headers.set("content-location", contentLocation);
    return new Response(headOnly ? null : (object as R2ObjectBody).body, { headers });
  }
}

function publicationLeaseIdentity(value: unknown): PublicationLeaseIdentity | undefined {
  if (!exactRecord(value, ["owner", "kind", "id", "commit"])) return undefined;
  const { owner, kind, id, commit } = value;
  const identity = typeof kind === "string" && typeof id === "string"
    ? releaseIdentity(kind, id)
    : undefined;
  if (
    !validPublicationLeaseOwner(owner) || !identity ||
    typeof commit !== "string" || commit.length !== 40 || !SHA1.test(commit) ||
    (identity.kind === "commit" && identity.id !== commit)
  ) return undefined;
  return { owner, kind: identity.kind, id: identity.id, commit };
}

function publicationLeaseOwner(value: unknown): string | undefined {
  if (!exactRecord(value, ["owner"])) return undefined;
  return validPublicationLeaseOwner(value.owner) ? value.owner : undefined;
}

function publicationLeaseFence(request: Request): PublicationLeaseFence | undefined {
  const leaseId = request.headers.get("x-nanocodex-publication-lease-id");
  const owner = request.headers.get("x-nanocodex-publication-lease-owner");
  const encodedGeneration = request.headers.get(
    "x-nanocodex-publication-lease-generation",
  );
  if (
    !leaseId || !PUBLICATION_LEASE_ID.test(leaseId) ||
    !validPublicationLeaseOwner(owner) || encodedGeneration == null ||
    !/^[1-9][0-9]{0,15}$(?![\s\S])/.test(encodedGeneration)
  ) return undefined;
  const generation = Number(encodedGeneration);
  if (
    !Number.isSafeInteger(generation) ||
    !leaseId.startsWith(`${generation}.`)
  ) return undefined;
  return { leaseId, owner, generation };
}

function publicationLeaseId(encoded: string | undefined): string | undefined {
  const leaseId = decoded(encoded);
  return leaseId && PUBLICATION_LEASE_ID.test(leaseId) ? leaseId : undefined;
}

function validPublicationLeaseOwner(value: unknown): value is string {
  return typeof value === "string" && value.length > 0 && value.length <= 192 &&
    /^[A-Za-z0-9]/.test(value) && !INVALID_PUBLICATION_LEASE_OWNER_CHARACTER.test(value);
}

function publicationLeaseExpiry(now: number): string {
  return new Date(now + PUBLICATION_LEASE_TTL_MS).toISOString();
}

function publicationLeaseActive(lease: CiPublicationLease, now: number): boolean {
  const expiresAt = Date.parse(lease.expiresAt);
  return Number.isFinite(expiresAt) && expiresAt > now;
}

function samePublicationLeaseIdentity(
  lease: CiPublicationLease,
  identity: PublicationLeaseIdentity,
): boolean {
  return lease.owner === identity.owner && lease.kind === identity.kind &&
    lease.id === identity.id && lease.commit === identity.commit;
}

function requiresReleaseCommitReservation(
  identity: ReleaseIdentity,
  commit: string,
): boolean {
  return identity.kind !== "stable" || identity.id !== STABLE_BOOTSTRAP_ID ||
    commit !== STABLE_BOOTSTRAP_COMMIT;
}

function validReleaseCommitReservationProof(
  value: unknown,
  expected: ReleaseCommitReservationRequest,
): value is CiReleaseCommitReservationProof {
  if (!exactRecord(value, ["reservation", "publication", "run"])) return false;
  const { reservation, publication, run } = value;
  return validReleaseCommitReservation(reservation, expected) &&
    isCiSourcePublication(publication) && publication.head === expected.commit &&
    ciSourceLane(publication).type === "master" &&
    validAuthoritativeRun(run, expected.commit);
}

function validReleaseCommitReservation(
  value: unknown,
  expected: ReleaseCommitReservationRequest | CiReleaseCommitReservation,
): value is CiReleaseCommitReservation {
  if (!exactRecord(value, [
    "version",
    "kind",
    "reservationId",
    "owner",
    "releaseKind",
    "releaseId",
    "commit",
    "publicationLeaseId",
    "publicationLeaseGeneration",
    "generation",
    "acquiredAt",
    "renewedAt",
    "expiresAt",
  ])) return false;
  const acquiredAt = Date.parse(String(value.acquiredAt));
  const renewedAt = Date.parse(String(value.renewedAt));
  const expiresAt = Date.parse(String(value.expiresAt));
  return value.version === 1 && value.kind === "release-commit" &&
    typeof value.reservationId === "string" &&
    PUBLICATION_LEASE_ID.test(value.reservationId) &&
    validPublicationLeaseOwner(value.owner) &&
    (value.releaseKind === "stable" || value.releaseKind === "commit") &&
    typeof value.releaseId === "string" &&
    (value.releaseKind === "stable"
      ? stableVersion(value.releaseId) != null
      : SHA1.test(value.releaseId)) &&
    typeof value.commit === "string" && SHA1.test(value.commit) &&
    (value.releaseKind !== "commit" || value.releaseId === value.commit) &&
    typeof value.publicationLeaseId === "string" &&
    PUBLICATION_LEASE_ID.test(value.publicationLeaseId) &&
    Number.isSafeInteger(value.publicationLeaseGeneration) &&
    (value.publicationLeaseGeneration as number) > 0 &&
    value.publicationLeaseId.split(".", 1)[0] ===
      String(value.publicationLeaseGeneration) &&
    Number.isSafeInteger(value.generation) && (value.generation as number) > 0 &&
    value.reservationId.split(".", 1)[0] === String(value.generation) &&
    Number.isFinite(acquiredAt) && acquiredAt <= Date.now() &&
    Number.isFinite(renewedAt) && renewedAt >= acquiredAt && renewedAt <= Date.now() &&
    Number.isFinite(expiresAt) && expiresAt > renewedAt &&
    expiresAt - renewedAt <= PUBLICATION_LEASE_TTL_MS &&
    value.owner === expected.owner && value.releaseKind === expected.releaseKind &&
    value.releaseId === expected.releaseId && value.commit === expected.commit &&
    value.publicationLeaseId === expected.publicationLeaseId &&
    value.publicationLeaseGeneration === expected.publicationLeaseGeneration &&
    (!("reservationId" in expected) ||
      (value.reservationId === expected.reservationId &&
        value.generation === expected.generation &&
        value.acquiredAt === expected.acquiredAt));
}

function validAuthoritativeRun(value: unknown, commit: string): value is CiRunRecord {
  if (!record(value)) return false;
  return value.version === 1 && value.head === commit &&
    value.workflowId === `ci-${commit}` && value.state === "dispatched" &&
    (value.beforeHead === null || (typeof value.beforeHead === "string" &&
      SHA1.test(value.beforeHead))) &&
    Number.isSafeInteger(value.attempts) && (value.attempts as number) >= 0 &&
    typeof value.publishedAt === "string" &&
    Number.isFinite(Date.parse(value.publishedAt));
}

async function repositoryReservationError(response: Response): Promise<Response> {
  const value: unknown = await response.json().catch(() => undefined);
  const message = record(value) && typeof value.error === "string"
    ? value.error
    : undefined;
  if (
    response.status === 409 &&
    (message === "release_head_stale" ||
      message === "release_commit_reservation_conflict" ||
      message === "release_commit_reservation_not_held")
  ) return error(message, 409);
  return error(
    response.status >= 500 ? "release_authority_invalid" : "release_authority_rejected",
    response.status >= 500 ? 503 : 409,
  );
}

function normalizeReleaseStaging(
  value: unknown,
  identity: ReleaseIdentity,
  stageId: string,
): ReleaseStagingInput | undefined {
  if (
    !exactRecord(value, ["version", "commit", "keys"]) || value.version !== 1 ||
    typeof value.commit !== "string" || !SHA1.test(value.commit) ||
    (identity.kind === "commit" && identity.id !== value.commit) ||
    !Array.isArray(value.keys) || value.keys.length === 0 ||
    value.keys.length > MAX_RELEASE_STAGING_KEYS
  ) return undefined;
  const keys = [...value.keys];
  if (!keys.every((key): key is string => typeof key === "string")) return undefined;
  if (
    new Set(keys).size !== keys.length ||
    keys.some((key) => !releaseStagingOwnsKey(identity, stageId, key))
  ) return undefined;
  keys.sort();
  return {
    version: 1,
    ...identity,
    commit: value.commit,
    stageId,
    keys,
  };
}

function releaseStagingOwnsKey(
  identity: ReleaseIdentity,
  stageId: string,
  key: string,
): boolean {
  const prefix = `distribution-staging/${identity.kind}/${identity.id}/${stageId}/components/linux/`;
  return safeObjectKey(key) && key.startsWith(prefix) &&
    ASSET_NAME.test(key.slice(prefix.length));
}

function releaseStagingKey(
  value: Pick<CiReleaseStaging, "kind" | "id" | "stageId">,
): string {
  return `${RELEASE_STAGING_PREFIX}${value.kind}:${value.id}:${value.stageId}`;
}

function sameReleaseStaging(
  record: CiReleaseStaging,
  input: ReleaseStagingInput | CiReleaseStaging,
): boolean {
  return record.version === input.version && record.kind === input.kind &&
    record.id === input.id && record.commit === input.commit &&
    record.stageId === input.stageId &&
    JSON.stringify(record.keys) === JSON.stringify(input.keys);
}

function releaseStagingFence(record: CiReleaseStaging): CiReleaseStagingFence {
  return { fenceId: record.fenceId, generation: record.generation };
}

function sameReleaseStagingFence(
  record: CiReleaseStaging,
  fence: CiReleaseStagingFence,
): boolean {
  return record.fenceId === fence.fenceId && record.generation === fence.generation;
}

function validReleaseStagingRecord(
  record: CiReleaseStaging,
  key: string,
): boolean {
  const identity = releaseIdentity(record.kind, record.id);
  const normalized = identity && RELEASE_STAGING_ID.test(record.stageId)
    ? normalizeReleaseStaging(
      { version: record.version, commit: record.commit, keys: record.keys },
      identity,
      record.stageId,
    )
    : undefined;
  const createdAt = Date.parse(record.createdAt);
  const expiresAt = Date.parse(record.expiresAt);
  const nextAttemptAt = Date.parse(record.nextAttemptAt);
  const exactShape = record.state === "active"
    ? exactRecord(record, [
      "version",
      "kind",
      "id",
      "commit",
      "stageId",
      "fenceId",
      "generation",
      "keys",
      "createdAt",
      "expiresAt",
      "nextAttemptAt",
      "attempts",
      "state",
    ])
    : record.state === "collecting" && exactRecord(record, [
      "version",
      "kind",
      "id",
      "commit",
      "stageId",
      "fenceId",
      "generation",
      "keys",
      "createdAt",
      "expiresAt",
      "nextAttemptAt",
      "attempts",
      "state",
      "claimId",
      "claimedAt",
    ]) &&
      /^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/.test(
        record.claimId,
      ) && Number.isFinite(Date.parse(record.claimedAt));
  return Boolean(normalized) && sameReleaseStaging(record, normalized!) && exactShape &&
    key === releaseStagingKey(record) &&
    RELEASE_STAGING_FENCE_ID.test(record.fenceId) &&
    Number.isSafeInteger(record.generation) && record.generation > 0 &&
    record.fenceId.split(".", 1)[0] === String(record.generation) &&
    Number.isFinite(createdAt) && createdAt <= Date.now() &&
    Number.isFinite(expiresAt) && expiresAt > createdAt &&
    expiresAt - createdAt <= RELEASE_STAGING_RETENTION_MS &&
    Number.isFinite(nextAttemptAt) && Number.isSafeInteger(record.attempts) &&
    record.attempts >= 0;
}

function normalizeDraft(value: unknown, identity: ReleaseIdentity): CiReleaseDraft | undefined {
  if (!record(value)) return undefined;
  const kind = value.kind;
  const tag = value.tag;
  const commit = value.commit;
  const channel = value.channel;
  const expectedChannel = value.expectedChannel;
  if (
    value.version !== 1 || kind !== identity.kind || typeof tag !== "string" ||
    typeof commit !== "string" || !SHA1.test(commit) ||
    !(expectedChannel === null || typeof expectedChannel === "string") ||
    !Array.isArray(value.assets) || value.assets.length === 0 || value.assets.length > MAX_ASSETS
  ) return undefined;
  if (
    kind === "stable" &&
    (stableVersion(tag) == null || tag !== identity.id || channel !== "latest" ||
      (expectedChannel !== null && stableVersion(expectedChannel) == null) ||
      (tag === STABLE_BOOTSTRAP_ID
        ? expectedChannel !== null || commit !== STABLE_BOOTSTRAP_COMMIT
        : expectedChannel === null))
  ) return undefined;
  if (
    kind === "commit" &&
    (identity.id !== commit || tag !== `nightly-${commit}` || channel !== "nightly" ||
      (expectedChannel !== null && !SHA1.test(expectedChannel)))
  ) return undefined;
  if (expectedChannel === identity.id) return undefined;

  const names = new Set<string>();
  const assets: CiReleaseAsset[] = [];
  for (const valueAsset of value.assets) {
    const asset = normalizeAsset(valueAsset);
    if (
      !asset || names.has(asset.name) ||
      !releaseOwnsAsset(identity, commit, asset.key)
    ) return undefined;
    names.add(asset.name);
    assets.push(asset);
  }
  assets.sort((left, right) => left.name.localeCompare(right.name));
  if (!exactReleaseAssets(identity, commit, assets)) return undefined;
  return {
    version: 1,
    kind: kind as CiReleaseKind,
    tag,
    commit,
    channel: channel as CiReleaseChannel,
    expectedChannel,
    assets,
  };
}

function normalizeAsset(value: unknown): CiReleaseAsset | undefined {
  if (!record(value)) return undefined;
  const { name, platform, key, size, sha256, contentType } = value;
  if (
    typeof name !== "string" || !ASSET_NAME.test(name) ||
    !isReleasePlatform(platform) ||
    typeof key !== "string" || !safeObjectKey(key) ||
    typeof size !== "number" || !Number.isSafeInteger(size) || size <= 0 ||
    size > MAX_ASSET_BYTES ||
    typeof sha256 !== "string" || !SHA256.test(sha256) ||
    typeof contentType !== "string" || contentType.length > 160 || !CONTENT_TYPE.test(contentType)
  ) return undefined;
  return { name, platform, key, size, sha256, contentType };
}

type ReleaseAssetSpecification = Pick<
  CiReleaseAsset,
  "name" | "platform" | "key" | "contentType"
> & { maxBytes: number };

function exactReleaseAssets(
  identity: ReleaseIdentity,
  commit: string,
  assets: readonly CiReleaseAsset[],
): boolean {
  const expected = releaseAssetSpecifications(identity, commit);
  return assets.length === expected.length && expected.every((specification, index) => {
    const asset = assets[index];
    return asset?.name === specification.name &&
      asset.platform === specification.platform && asset.key === specification.key &&
      asset.contentType === specification.contentType && asset.size <= specification.maxBytes;
  });
}

function releaseAssetSpecifications(
  identity: ReleaseIdentity,
  commit: string,
): ReleaseAssetSpecification[] {
  if (
    identity.kind === "stable" && identity.id === STABLE_BOOTSTRAP_ID &&
    commit === STABLE_BOOTSTRAP_COMMIT
  ) {
    const prefix = `release-import/stable/${STABLE_BOOTSTRAP_ID}`;
    return [
      assetSpecification("PROVENANCE.json", "linux", `${prefix}/PROVENANCE.json`, 64 * 1024, "application/json"),
      assetSpecification("SHA256SUMS", "linux", `${prefix}/SHA256SUMS`, 64 * 1024, "text/plain"),
      assetSpecification("nanocodex-aarch64-apple-darwin", "aarch64-apple-darwin", `${prefix}/nanocodex-aarch64-apple-darwin`, 128 * 1024 * 1024, "application/octet-stream"),
      assetSpecification("nanocodex-x86_64-unknown-linux-gnu", "x86_64-unknown-linux-gnu", `${prefix}/nanocodex-x86_64-unknown-linux-gnu`, 128 * 1024 * 1024, "application/octet-stream"),
      assetSpecification("nanocodex-x86_64-unknown-linux-gnu.gz", "x86_64-unknown-linux-gnu", `${prefix}/nanocodex-x86_64-unknown-linux-gnu.gz`, 128 * 1024 * 1024, "application/gzip"),
    ].sort((left, right) => left.name.localeCompare(right.name));
  }

  const prefix = releaseObjectPrefix(identity);
  const linux = `${prefix}/components/linux`;
  const npmName = identity.kind === "stable"
    ? `nanocodex-${identity.id.slice(1)}.tgz`
    : `nanocodex-${commit.slice(0, 10)}.tgz`;
  return [
    assetSpecification("PROVENANCE.json", "linux", `${prefix}/PROVENANCE.json`, 64 * 1024, "application/json"),
    assetSpecification("SHA256SUMS", "linux", `${prefix}/SHA256SUMS`, 64 * 1024, "text/plain; charset=utf-8"),
    assetSpecification(npmName, "npm", `${prefix}/components/npm/${npmName}`, 16 * 1024 * 1024, "application/gzip"),
    assetSpecification("nanocodex-aarch64-apple-darwin", "aarch64-apple-darwin", `${prefix}/components/macos/nanocodex-aarch64-apple-darwin`, 128 * 1024 * 1024, "application/octet-stream"),
    assetSpecification("nanocodex-vm-guest-x86_64-unknown-linux-musl", "x86_64-unknown-linux-musl", `${linux}/nanocodex-vm-guest-x86_64-unknown-linux-musl`, 64 * 1024 * 1024, "application/octet-stream"),
    assetSpecification("nanocodex-vm-guest-x86_64-unknown-linux-musl.gz", "x86_64-unknown-linux-musl", `${linux}/nanocodex-vm-guest-x86_64-unknown-linux-musl.gz`, 64 * 1024 * 1024, "application/gzip"),
    assetSpecification("nanocodex-x86_64-unknown-linux-gnu", "x86_64-unknown-linux-gnu", `${linux}/nanocodex-x86_64-unknown-linux-gnu`, 128 * 1024 * 1024, "application/octet-stream"),
    assetSpecification("nanocodex-x86_64-unknown-linux-gnu.gz", "x86_64-unknown-linux-gnu", `${linux}/nanocodex-x86_64-unknown-linux-gnu.gz`, 128 * 1024 * 1024, "application/gzip"),
  ].sort((left, right) => left.name.localeCompare(right.name));
}

function assetSpecification(
  name: string,
  platform: CiReleasePlatform,
  key: string,
  maxBytes: number,
  contentType: string,
): ReleaseAssetSpecification {
  return { name, platform, key, maxBytes, contentType };
}

async function invalidReleaseAssets(bucket: R2Bucket, assets: CiReleaseAsset[]) {
  const checked = await Promise.all(assets.map(async (asset) => ({
    asset,
    object: await bucket.head(asset.key),
  })));
  return checked
    .filter(({ asset, object }) => !matchesAsset(object, asset))
    .map(({ asset }) => ({ name: asset.name, key: asset.key }));
}

function matchesAsset(object: R2Object | null, asset: CiReleaseAsset): object is R2Object {
  return object != null && object.key === asset.key && object.size === asset.size &&
    object.httpMetadata?.contentType === asset.contentType &&
    object.checksums.sha256 != null && hex(object.checksums.sha256) === asset.sha256;
}

function releaseOwnsAsset(
  identity: ReleaseIdentity,
  commit: string,
  key: string,
): boolean {
  if (key.startsWith(`${releaseObjectPrefix(identity)}/`)) return true;
  return identity.kind === "stable" && identity.id === STABLE_BOOTSTRAP_ID &&
    commit === STABLE_BOOTSTRAP_COMMIT &&
    key.startsWith(`release-import/stable/${STABLE_BOOTSTRAP_ID}/`);
}

function releaseObjectPrefix(identity: ReleaseIdentity): string {
  return `distribution/${identity.kind}/${identity.id}`;
}

function promotionMetadata(
  promotion: CiReleaseAssetPromotion,
): Record<string, string> {
  return {
    kind: "distribution-component",
    releaseKind: promotion.kind,
    releaseId: promotion.id,
    commit: promotion.commit,
    component: promotion.component,
    sourceKey: promotion.source.key,
    name: promotion.asset.name,
    platform: promotion.asset.platform,
    sha256: promotion.asset.sha256,
  };
}

function matchesSourceAsset(
  object: R2ObjectBody | null,
  source: CiReleaseAssetPromotion["source"],
): boolean {
  return object != null && object.key === source.key && object.size === source.size &&
    object.httpMetadata?.contentType === source.contentType &&
    object.checksums.sha256 != null && hex(object.checksums.sha256) === source.sha256 &&
    Object.entries(source.customMetadata).every(([key, value]) =>
      object.customMetadata?.[key] === value
    );
}

function matchesPromotedAsset(
  object: R2Object | null,
  asset: CiReleaseAsset,
  metadata: Readonly<Record<string, string>>,
): object is R2Object {
  return matchesAsset(object, asset) && Object.entries(metadata).every(([key, value]) =>
    object.customMetadata?.[key] === value
  );
}

function publicDraftAsset(draft: StoredDraft, asset: CiReleaseAsset) {
  const { key: _key, ...publicAsset } = asset;
  return {
    ...publicAsset,
    downloadPath: draftAssetPath(draft, asset.name),
  };
}

function publicDraft(draft: StoredDraft) {
  return {
    version: draft.version,
    kind: draft.kind,
    id: draft.id,
    tag: draft.tag,
    commit: draft.commit,
    channel: draft.channel,
    expectedChannel: draft.expectedChannel,
    createdAt: draft.createdAt,
    assets: draft.assets.map((asset) => publicDraftAsset(draft, asset)),
  };
}

function draftResponse(draft: StoredDraft, headOnly: boolean, status = 200): Response {
  return jsonBody(
    { draft: publicDraft(draft) },
    status,
    { "cache-control": NO_STORE },
    headOnly,
  );
}

function publicUnsignedManifest(
  manifest: Omit<CiReleaseManifest, "manifestSha256"> | CiReleaseManifest,
) {
  return {
    version: manifest.version,
    kind: manifest.kind,
    id: manifest.id,
    tag: manifest.tag,
    commit: manifest.commit,
    channel: manifest.channel,
    finalizedAt: manifest.finalizedAt,
    assets: manifest.assets.map(({ key: _key, ...asset }) => ({
      ...asset,
      downloadPath: assetPath(manifest, asset.name),
    })),
  };
}

function publicManifest(manifest: CiReleaseManifest) {
  return {
    ...publicUnsignedManifest(manifest),
    manifestSha256: manifest.manifestSha256,
  };
}

function manifestResponse(manifest: CiReleaseManifest, headOnly: boolean): Response {
  return jsonBody(publicManifest(manifest), 200, {
    "cache-control": IMMUTABLE_CACHE,
    "etag": `"${manifest.manifestSha256}"`,
  }, headOnly);
}

function channelResponse(
  pointer: CiReleasePointer,
  manifest: CiReleaseManifest,
  headOnly: boolean,
): Response {
  return jsonBody({ pointer, manifest: publicManifest(manifest) }, 200, {
    "cache-control": NO_STORE,
    "content-location": manifestPath(manifest),
  }, headOnly);
}

function publicationResponse(
  manifest: CiReleaseManifest,
  pointer: CiReleasePointer | undefined,
  status: number,
): Response {
  return json({ manifest: publicManifest(manifest), pointer: pointer ?? null }, status);
}

function finalizationPolicyConflict(
  draft: StoredDraft,
  current: CiReleasePointer | undefined,
): Extract<FinalizeOutcome, { status: "conflict" }> | undefined {
  if (
    draft.kind === "stable" && !current &&
    (draft.id !== STABLE_BOOTSTRAP_ID || draft.commit !== STABLE_BOOTSTRAP_COMMIT)
  ) return { status: "conflict", error: "stable_bootstrap_required" };
  if ((current?.id ?? null) !== draft.expectedChannel) {
    return { status: "conflict", error: "release_channel_conflict", current };
  }
  if (
    draft.kind === "stable" && current &&
    compareStableTags(draft.id, current.id) <= 0
  ) return { status: "conflict", error: "latest_must_advance", current };
  return undefined;
}

function matchingPointer(
  pointer: CiReleasePointer | undefined,
  manifest: CiReleaseManifest,
): CiReleasePointer | undefined {
  return pointer?.kind === manifest.kind && pointer.id === manifest.id ? pointer : undefined;
}

function sameDraft(stored: StoredDraft, input: CiReleaseDraft): boolean {
  const { id: _id, createdAt: _createdAt, expiresAt: _expiresAt, ...draft } = stored;
  return JSON.stringify(draft) === JSON.stringify(input);
}

function sameDraftIdentity(stored: StoredDraft, input: CiReleaseDraft): boolean {
  const { expectedChannel: _storedExpected, ...storedIdentity } = stored;
  const {
    expectedChannel: _inputExpected,
    ...inputIdentity
  } = input;
  const {
    id: _id,
    createdAt: _createdAt,
    expiresAt: _expiresAt,
    ...storedInput
  } = storedIdentity;
  return JSON.stringify(storedInput) === JSON.stringify(inputIdentity);
}

function sameReleaseInput(manifest: CiReleaseManifest, input: CiReleaseDraft): boolean {
  return manifest.kind === input.kind && manifest.tag === input.tag &&
    manifest.commit === input.commit && manifest.channel === input.channel &&
    JSON.stringify(manifest.assets) === JSON.stringify(input.assets);
}

function releaseIdentity(kind: string | undefined, encodedId: string | undefined) {
  const id = decoded(encodedId);
  if (!id || (kind !== "stable" && kind !== "commit")) return undefined;
  if (kind === "stable" ? stableVersion(id) == null : !SHA1.test(id)) return undefined;
  return { kind, id } satisfies ReleaseIdentity;
}

function stableVersion(tag: string): [number, number, number] | undefined {
  const match = /^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$(?![\s\S])/.exec(tag);
  if (!match || match[0] !== tag) return undefined;
  const version = [Number(match[1]), Number(match[2]), Number(match[3])] as [number, number, number];
  return version.every(Number.isSafeInteger) ? version : undefined;
}

function compareStableTags(left: string, right: string): number {
  const leftVersion = stableVersion(left);
  const rightVersion = stableVersion(right);
  if (!leftVersion || !rightVersion) return 0;
  for (let index = 0; index < leftVersion.length; index += 1) {
    const order = leftVersion[index]! - rightVersion[index]!;
    if (order !== 0) return order;
  }
  return 0;
}

function isReleasePlatform(value: unknown): value is CiReleasePlatform {
  return value === "x86_64-unknown-linux-gnu" || value === "aarch64-apple-darwin" ||
    value === "x86_64-unknown-linux-musl" || value === "linux" || value === "npm";
}

function safeObjectKey(key: string): boolean {
  return key.length > 0 && key.length <= 1_024 && !key.startsWith("/") &&
    !key.includes("\\") && !key.includes("\0") &&
    key.split("/").every((part) => part !== "" && part !== "." && part !== "..");
}

function releaseKey(identity: ReleaseIdentity): string {
  return `release:${identity.kind}:${identity.id}`;
}

function draftKey(identity: ReleaseIdentity): string {
  return `draft:${identity.kind}:${identity.id}`;
}

function draftGcKey(identity: ReleaseIdentity): string {
  return `${DRAFT_GC_PREFIX}${identity.kind}:${identity.id}`;
}

function channelKey(channel: CiReleaseChannel): string {
  return `channel:${channel}`;
}

function manifestPath(manifest: Pick<CiReleaseManifest, "kind" | "id">): string {
  return `${PUBLIC_RELEASES_PATH}/releases/${manifest.kind}/${encodeURIComponent(manifest.id)}`;
}

function assetPath(
  manifest: Pick<CiReleaseManifest, "kind" | "id">,
  name: string,
): string {
  return `${manifestPath(manifest)}/assets/${encodeURIComponent(name)}`;
}

function draftAssetPath(
  draft: Pick<StoredDraft, "kind" | "id">,
  name: string,
): string {
  return `${PUBLIC_RELEASES_PATH}/drafts/${draft.kind}/${encodeURIComponent(draft.id)}/assets/${encodeURIComponent(name)}`;
}

function authenticated(request: Request, expected: string | undefined): boolean {
  const header = request.headers.get("authorization");
  if (!expected || !header?.startsWith("Bearer ")) return false;
  const actual = header.slice("Bearer ".length);
  let mismatch = actual.length ^ expected.length;
  for (let index = 0; index < expected.length; index += 1) {
    mismatch |= (actual.charCodeAt(index) | 0) ^ expected.charCodeAt(index);
  }
  return mismatch === 0;
}

function route(path: string, pattern: RegExp): RegExpExecArray | null {
  return pattern.exec(path);
}

function decoded(value: string | undefined): string | undefined {
  if (value == null) return undefined;
  try {
    return decodeURIComponent(value);
  } catch {
    return undefined;
  }
}

function record(value: unknown): value is Record<string, unknown> {
  return value != null && typeof value === "object" && !Array.isArray(value);
}

function exactRecord(value: unknown, keys: readonly string[]): value is Record<string, unknown> {
  if (!record(value)) return false;
  const actual = Object.keys(value);
  return actual.length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}

function strictContentLength(value: string | null): number | undefined {
  if (value == null || !/^(0|[1-9][0-9]*)$(?![\s\S])/.test(value)) return undefined;
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) ? parsed : undefined;
}

async function cancelBody(body: ReadableStream | null): Promise<void> {
  await body?.cancel().catch(() => undefined);
}

async function bodyMatchesSha256(
  body: ReadableStream<Uint8Array>,
  expectedSize: number,
  expectedSha256: string,
): Promise<boolean> {
  const reader = body.getReader();
  const digest = sha256.create();
  let size = 0;
  try {
    while (true) {
      const chunk = await reader.read();
      if (chunk.done) break;
      size += chunk.value.byteLength;
      if (size > expectedSize) {
        await reader.cancel().catch(() => undefined);
        return false;
      }
      digest.update(chunk.value);
    }
    return size === expectedSize && hexBytes(digest.digest()) === expectedSha256;
  } catch {
    await reader.cancel().catch(() => undefined);
    return false;
  } finally {
    reader.releaseLock();
  }
}

async function jsonSha256(value: unknown): Promise<string> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(canonicalJson(value)),
  );
  return hex(digest);
}

// The public manifest digest is defined over this canonical JSON form so a
// consumer needs neither property-order knowledge nor private R2 object keys.
function canonicalJson(value: unknown): string {
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
  throw new TypeError("canonical JSON requires JSON values");
}

function hex(value: ArrayBuffer): string {
  return hexBytes(new Uint8Array(value));
}

function hexBytes(value: Uint8Array): string {
  return [...value]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

function unauthorized(): Response {
  return jsonBody({ error: "unauthorized" }, 401, {
    "cache-control": NO_STORE,
    "www-authenticate": "Bearer",
  });
}

function error(message: string, status: number): Response {
  return json({ error: message }, status);
}

function json(value: unknown, status = 200): Response {
  return jsonBody(value, status, { "cache-control": NO_STORE });
}

function empty(status: number): Response {
  return new Response(null, { status, headers: { "cache-control": NO_STORE } });
}

function jsonBody(
  value: unknown,
  status: number,
  extraHeaders: Record<string, string>,
  headOnly = false,
): Response {
  const body = JSON.stringify(value);
  return new Response(headOnly ? null : body, {
    status,
    headers: {
      "content-length": String(new TextEncoder().encode(body).byteLength),
      "content-type": "application/json; charset=utf-8",
      "x-content-type-options": "nosniff",
      ...extraHeaders,
    },
  });
}
