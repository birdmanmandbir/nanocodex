const SHA1_PATTERN = /^[a-f0-9]{40}$/;
const SHA256_PATTERN = /^[a-f0-9]{64}$/;
const SOURCE_PREFIX_PATTERN = /^sources\/([a-f0-9]{40})\/(source\.tar\.gz|tree\.json)$/;
export const MAX_CARGO_VENDOR_BYTES = 256 * 1024 * 1024;
export const MASTER_SOURCE_REF = "refs/heads/master";
export const MASTER_SOURCE_BRANCH = "master";
export const EXACT_SOURCE_TREE_PATH = "/.nanocodex-ci/source-tree";

export type CiSourceObject = {
  key: string;
  size: number;
  sha256: string;
};

type CiSourcePublicationFields = {
  version: 1;
  head: string;
  archive: CiSourceObject;
  tree: CiSourceObject;
  cargoLockBlob: string;
  cargoVendor: CiSourceObject;
  rustSecRevision: string;
  rustSec: CiSourceObject;
  publishedAt: string;
};

export type CiSourcePublicationLane =
  | { type: "master" }
  | {
    type: "pull_request";
    number: number;
    pullRequestHead: string;
  };

export type CiSourcePublication = CiSourcePublicationFields & (
  | {
    branch: typeof MASTER_SOURCE_BRANCH;
    ref: typeof MASTER_SOURCE_REF;
    // Optional on input so the existing master publisher remains compatible.
    // The repository normalizes every stored master publication to this lane.
    lane?: { type: "master" };
  }
  | {
    branch: `pull/${number}/merge`;
    ref: `refs/pull/${number}/merge`;
    lane: Extract<CiSourcePublicationLane, { type: "pull_request" }>;
  }
);

export type CiSourceLane =
  | {
    type: "master";
    deployable: true;
    branch: typeof MASTER_SOURCE_BRANCH;
    ref: typeof MASTER_SOURCE_REF;
    head: string;
  }
  | {
    type: "pull_request";
    deployable: false;
    number: number;
    branch: `pull/${number}/merge`;
    ref: `refs/pull/${number}/merge`;
    mergeHead: string;
    pullRequestHead: string;
  };

export type CiSourceTreeFile = {
  path: string;
  sha: string;
  mode: "100644" | "100755" | "120000";
};

export type CiSourceTree = {
  version: 1;
  head: string;
  archive: { size: number; sha256: string };
  files: CiSourceTreeFile[];
};

type NanocodexCiSourceData = {
  archiveKey: string;
  archiveSha256: string;
  archiveSize: number;
  treeKey: string;
  treeSha256: string;
  cargoLockBlob: string;
  cargoVendorKey: string;
  cargoVendorSha256: string;
  cargoVendorSize: number;
  rustSecRevision: string;
  rustSecKey: string;
  rustSecSha256: string;
  rustSecSize: number;
  publishedAt: string;
};

export type NanocodexCiProviderData = NanocodexCiSourceData & (
  | {
    lane: Extract<CiSourceLane, { type: "master" }>;
    distribution?: CiDistributionRequest;
  }
  | {
    lane: Extract<CiSourceLane, { type: "pull_request" }>;
    distribution?: never;
  }
);

export type CiDistributionRequest = {
  version: 1;
  channel: "nightly" | "stable";
  tagName: string;
  buildTimestamp: string;
};

export type NanocodexCiParams = {
  provider: "nanocodex-source";
  providerData: NanocodexCiProviderData;
  event: { type: "push" };
  owner: "gakonst";
  repo: "nanocodex";
  sha: string;
  remote: "cloudflare";
  trigger: "push";
  ref: CiSourceLane["ref"];
  branch: CiSourceLane["branch"];
  beforeSha?: string;
};

export function sourceArchiveKey(head: string): string {
  return `sources/${head}/source.tar.gz`;
}

export function sourceTreeKey(head: string): string {
  return `sources/${head}/tree.json`;
}

export function cargoVendorBundleKey(cargoLockBlob: string, bundleSha256: string): string {
  return `cargo-vendor/${cargoLockBlob}/${bundleSha256}/bundle.tar.gz`;
}

export function rustSecAdvisoryBundleKey(revision: string): string {
  return `rustsec-advisory-db/${revision}/bundle.tar.gz`;
}

export function isCiSourcePublication(value: unknown): value is CiSourcePublication {
  if (value == null || typeof value !== "object") return false;
  const publication = value as Partial<CiSourcePublication>;
  if (
    publication.version !== 1 ||
    typeof publication.head !== "string" || !SHA1_PATTERN.test(publication.head) ||
    !isSourceObject(publication.archive, sourceArchiveKey(publication.head), 128 * 1024 * 1024) ||
    !isSourceObject(publication.tree, sourceTreeKey(publication.head), 16 * 1024 * 1024) ||
    typeof publication.cargoLockBlob !== "string" ||
    !SHA1_PATTERN.test(publication.cargoLockBlob) ||
    !isCargoVendorObject(publication.cargoVendor, publication.cargoLockBlob) ||
    typeof publication.rustSecRevision !== "string" ||
    !SHA1_PATTERN.test(publication.rustSecRevision) ||
    !isSourceObject(
      publication.rustSec,
      rustSecAdvisoryBundleKey(publication.rustSecRevision),
      16 * 1024 * 1024,
    ) ||
    typeof publication.publishedAt !== "string" ||
    !Number.isFinite(Date.parse(publication.publishedAt))
  ) return false;
  return isPublicationLane(publication);
}

export function normalizeCiSourcePublication(
  publication: CiSourcePublication,
): CiSourcePublication {
  const common = {
    version: publication.version,
    head: publication.head,
    archive: {
      key: publication.archive.key,
      size: publication.archive.size,
      sha256: publication.archive.sha256,
    },
    tree: {
      key: publication.tree.key,
      size: publication.tree.size,
      sha256: publication.tree.sha256,
    },
    cargoLockBlob: publication.cargoLockBlob,
    cargoVendor: {
      key: cargoVendorBundleKey(
        publication.cargoLockBlob,
        publication.cargoVendor.sha256,
      ),
      size: publication.cargoVendor.size,
      sha256: publication.cargoVendor.sha256,
    },
    rustSecRevision: publication.rustSecRevision,
    rustSec: {
      key: publication.rustSec.key,
      size: publication.rustSec.size,
      sha256: publication.rustSec.sha256,
    },
    publishedAt: publication.publishedAt,
  };
  if (publication.lane?.type === "pull_request") {
    const branch = `pull/${publication.lane.number}/merge` as const;
    const ref = `refs/pull/${publication.lane.number}/merge` as const;
    return {
      ...common,
      branch,
      ref,
      lane: { ...publication.lane },
    };
  }
  return {
    ...common,
    branch: MASTER_SOURCE_BRANCH,
    ref: MASTER_SOURCE_REF,
    lane: { type: "master" },
  };
}

export function ciSourceLane(publication: CiSourcePublication): CiSourceLane {
  if (publication.lane?.type === "pull_request") {
    const branch = `pull/${publication.lane.number}/merge` as const;
    const ref = `refs/pull/${publication.lane.number}/merge` as const;
    return {
      type: "pull_request",
      deployable: false,
      number: publication.lane.number,
      branch,
      ref,
      mergeHead: publication.head,
      pullRequestHead: publication.lane.pullRequestHead,
    };
  }
  return {
    type: "master",
    deployable: true,
    branch: MASTER_SOURCE_BRANCH,
    ref: MASTER_SOURCE_REF,
    head: publication.head,
  };
}

export function isCiSourceLane(value: unknown, expectedHead?: string): value is CiSourceLane {
  if (value == null || typeof value !== "object") return false;
  const lane = value as Partial<CiSourceLane>;
  if (lane.type === "master") {
    return lane.deployable === true &&
      lane.branch === MASTER_SOURCE_BRANCH && lane.ref === MASTER_SOURCE_REF &&
      typeof lane.head === "string" && SHA1_PATTERN.test(lane.head) &&
      (expectedHead === undefined || lane.head === expectedHead) &&
      hasExactKeys(value, ["branch", "deployable", "head", "ref", "type"]);
  }
  if (lane.type !== "pull_request") return false;
  const pullRequest = lane as Partial<Extract<CiSourceLane, { type: "pull_request" }>>;
  return pullRequest.deployable === false &&
    typeof pullRequest.number === "number" &&
    Number.isSafeInteger(pullRequest.number) && pullRequest.number > 0 &&
    pullRequest.branch === `pull/${pullRequest.number}/merge` &&
    pullRequest.ref === `refs/pull/${pullRequest.number}/merge` &&
    typeof pullRequest.mergeHead === "string" && SHA1_PATTERN.test(pullRequest.mergeHead) &&
    (expectedHead === undefined || pullRequest.mergeHead === expectedHead) &&
    typeof pullRequest.pullRequestHead === "string" &&
    SHA1_PATTERN.test(pullRequest.pullRequestHead) &&
    hasExactKeys(value, [
      "branch",
      "deployable",
      "mergeHead",
      "number",
      "pullRequestHead",
      "ref",
      "type",
    ]);
}

export function isNanocodexCiProviderData(
  value: unknown,
  expectedHead: string,
): value is NanocodexCiProviderData {
  if (value == null || typeof value !== "object" || !isSha1(expectedHead)) return false;
  const data = value as Partial<NanocodexCiProviderData>;
  if (
    data.archiveKey !== sourceArchiveKey(expectedHead) ||
    !isSha256(data.archiveSha256) ||
    typeof data.archiveSize !== "number" ||
    !Number.isSafeInteger(data.archiveSize) || data.archiveSize <= 0 ||
    data.archiveSize > 128 * 1024 * 1024 ||
    data.treeKey !== sourceTreeKey(expectedHead) ||
    !isSha256(data.treeSha256) ||
    !isSha1(data.cargoLockBlob) ||
    !isSha256(data.cargoVendorSha256) ||
    data.cargoVendorKey !== cargoVendorBundleKey(
      data.cargoLockBlob,
      data.cargoVendorSha256,
    ) ||
    typeof data.cargoVendorSize !== "number" ||
    !Number.isSafeInteger(data.cargoVendorSize) || data.cargoVendorSize <= 0 ||
    data.cargoVendorSize > MAX_CARGO_VENDOR_BYTES ||
    typeof data.publishedAt !== "string" || !Number.isFinite(Date.parse(data.publishedAt)) ||
    !isSha1(data.rustSecRevision) ||
    data.rustSecKey !== rustSecAdvisoryBundleKey(data.rustSecRevision) ||
    !isSha256(data.rustSecSha256) ||
    typeof data.rustSecSize !== "number" ||
    !Number.isSafeInteger(data.rustSecSize) || data.rustSecSize <= 0 ||
    data.rustSecSize > 16 * 1024 * 1024 ||
    !isCiSourceLane(data.lane, expectedHead) ||
    (data.distribution !== undefined &&
      (data.lane.type !== "master" || !isCiDistributionRequest(data.distribution)))
  ) return false;
  const allowed = new Set([
    "archiveKey",
    "archiveSha256",
    "archiveSize",
    "cargoLockBlob",
    "cargoVendorKey",
    "cargoVendorSha256",
    "cargoVendorSize",
    "distribution",
    "lane",
    "publishedAt",
    "rustSecKey",
    "rustSecRevision",
    "rustSecSha256",
    "rustSecSize",
    "treeKey",
    "treeSha256",
  ]);
  return Object.keys(value).every((key) => allowed.has(key));
}

export function isCiDistributionRequest(value: unknown): value is CiDistributionRequest {
  if (value == null || typeof value !== "object") return false;
  const request = value as Partial<CiDistributionRequest>;
  const validIdentity = request.channel === "nightly"
    ? request.tagName === "nightly"
    : request.channel === "stable" && typeof request.tagName === "string" &&
      /^v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$/.test(request.tagName);
  return request.version === 1 && validIdentity &&
    typeof request.buildTimestamp === "string" &&
    Number.isFinite(Date.parse(request.buildTimestamp)) &&
    hasExactKeys(value, ["buildTimestamp", "channel", "tagName", "version"]);
}

export function isCiSourceTree(value: unknown, expectedHead?: string): value is CiSourceTree {
  if (value == null || typeof value !== "object") return false;
  const tree = value as Partial<CiSourceTree>;
  if (
    tree.version !== 1 ||
    typeof tree.head !== "string" ||
    !SHA1_PATTERN.test(tree.head) ||
    (expectedHead !== undefined && tree.head !== expectedHead) ||
    tree.archive == null ||
    typeof tree.archive !== "object" ||
    typeof tree.archive.size !== "number" ||
    !Number.isSafeInteger(tree.archive.size) ||
    tree.archive.size <= 0 ||
    tree.archive.size > 128 * 1024 * 1024 ||
    typeof tree.archive.sha256 !== "string" ||
    !SHA256_PATTERN.test(tree.archive.sha256) ||
    !Array.isArray(tree.files) ||
    tree.files.length > 200_000
  ) return false;
  let previous = "";
  for (const file of tree.files) {
    if (
      file == null || typeof file !== "object" ||
      typeof file.path !== "string" || !isSafeRepositoryPath(file.path) ||
      file.path <= previous ||
      typeof file.sha !== "string" || !SHA1_PATTERN.test(file.sha) ||
      !["100644", "100755", "120000"].includes(file.mode)
    ) return false;
    previous = file.path;
  }
  return true;
}

export function isSha1(value: unknown): value is string {
  return typeof value === "string" && SHA1_PATTERN.test(value);
}

export function isSha256(value: unknown): value is string {
  return typeof value === "string" && SHA256_PATTERN.test(value);
}

export function isCanonicalSourceKey(value: string): boolean {
  return SOURCE_PREFIX_PATTERN.test(value);
}

export async function sourceTreeFingerprint(tree: CiSourceTree): Promise<string> {
  const canonical = JSON.stringify(
    tree.files.map(({ path, sha, mode }) => [path, sha, mode]),
  );
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(canonical),
  );
  return [...new Uint8Array(digest)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

function isSourceObject(value: unknown, key: string, maxSize: number): value is CiSourceObject {
  if (value == null || typeof value !== "object") return false;
  const object = value as Partial<CiSourceObject>;
  return object.key === key &&
    typeof object.size === "number" && Number.isSafeInteger(object.size) &&
    object.size > 0 && object.size <= maxSize &&
    typeof object.sha256 === "string" && SHA256_PATTERN.test(object.sha256);
}

function isCargoVendorObject(value: unknown, cargoLockBlob: string): value is CiSourceObject {
  if (value == null || typeof value !== "object") return false;
  const object = value as Partial<CiSourceObject>;
  return typeof object.sha256 === "string" && SHA256_PATTERN.test(object.sha256) &&
    isSourceObject(
      value,
      cargoVendorBundleKey(cargoLockBlob, object.sha256),
      MAX_CARGO_VENDOR_BYTES,
    );
}

function isPublicationLane(publication: Partial<CiSourcePublication>): boolean {
  const lane = (publication as { lane?: unknown }).lane;
  if (
    publication.branch === MASTER_SOURCE_BRANCH &&
    publication.ref === MASTER_SOURCE_REF
  ) {
    return lane === undefined || (
      lane != null && typeof lane === "object" &&
      (lane as { type?: unknown }).type === "master" &&
      hasExactKeys(lane, ["type"])
    );
  }
  if (lane == null || typeof lane !== "object") return false;
  const pullRequest = lane as {
    type?: unknown;
    number?: unknown;
    pullRequestHead?: unknown;
  };
  if (
    pullRequest.type !== "pull_request" ||
    typeof pullRequest.number !== "number" ||
    !Number.isSafeInteger(pullRequest.number) || pullRequest.number <= 0 ||
    typeof pullRequest.pullRequestHead !== "string" ||
    !SHA1_PATTERN.test(pullRequest.pullRequestHead) ||
    !hasExactKeys(lane, ["number", "pullRequestHead", "type"])
  ) return false;
  return publication.branch === `pull/${pullRequest.number}/merge` &&
    publication.ref === `refs/pull/${pullRequest.number}/merge`;
}

function hasExactKeys(value: object, expected: string[]): boolean {
  const keys = Object.keys(value).sort();
  return keys.length === expected.length &&
    keys.every((key, index) => key === expected[index]);
}

function isSafeRepositoryPath(path: string): boolean {
  if (
    path.length === 0 || path.length > 4_096 || path.startsWith("/") ||
    path.includes("\0") || path.includes("\\")
  ) return false;
  return path.split("/").every((part) => part !== "" && part !== "." && part !== "..");
}
