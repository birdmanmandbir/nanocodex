const SHA1_PATTERN = /^[a-f0-9]{40}$/;
const SHA256_PATTERN = /^[a-f0-9]{64}$/;
const SOURCE_PREFIX_PATTERN = /^sources\/([a-f0-9]{40})\/(source\.tar\.gz|tree\.json)$/;
const SOURCE_REF = "refs/heads/master";
const SOURCE_BRANCH = "master";

export type CiSourceObject = {
  key: string;
  size: number;
  sha256: string;
};

export type CiSourcePublication = {
  version: 1;
  head: string;
  branch: typeof SOURCE_BRANCH;
  ref: typeof SOURCE_REF;
  archive: CiSourceObject;
  tree: CiSourceObject;
  cargoLockBlob: string;
  cargoVendor: CiSourceObject;
  rustSecRevision: string;
  rustSec: CiSourceObject;
  publishedAt: string;
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

export type NanocodexCiProviderData = {
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
  ref: typeof SOURCE_REF;
  branch: typeof SOURCE_BRANCH;
  beforeSha?: string;
};

export function sourceArchiveKey(head: string): string {
  return `sources/${head}/source.tar.gz`;
}

export function sourceTreeKey(head: string): string {
  return `sources/${head}/tree.json`;
}

export function cargoVendorBundleKey(cargoLockBlob: string): string {
  return `cargo-vendor/${cargoLockBlob}/bundle.tar.gz`;
}

export function rustSecAdvisoryBundleKey(revision: string): string {
  return `rustsec-advisory-db/${revision}/bundle.tar.gz`;
}

export function isCiSourcePublication(value: unknown): value is CiSourcePublication {
  if (value == null || typeof value !== "object") return false;
  const publication = value as Partial<CiSourcePublication>;
  return publication.version === 1 &&
    typeof publication.head === "string" && SHA1_PATTERN.test(publication.head) &&
    publication.branch === SOURCE_BRANCH &&
    publication.ref === SOURCE_REF &&
    isSourceObject(publication.archive, sourceArchiveKey(publication.head), 128 * 1024 * 1024) &&
    isSourceObject(publication.tree, sourceTreeKey(publication.head), 16 * 1024 * 1024) &&
    typeof publication.cargoLockBlob === "string" &&
    SHA1_PATTERN.test(publication.cargoLockBlob) &&
    isSourceObject(
      publication.cargoVendor,
      cargoVendorBundleKey(publication.cargoLockBlob),
      16 * 1024 * 1024,
    ) &&
    typeof publication.rustSecRevision === "string" &&
    SHA1_PATTERN.test(publication.rustSecRevision) &&
    isSourceObject(
      publication.rustSec,
      rustSecAdvisoryBundleKey(publication.rustSecRevision),
      16 * 1024 * 1024,
    ) &&
    typeof publication.publishedAt === "string" &&
    Number.isFinite(Date.parse(publication.publishedAt));
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

function isSourceObject(value: unknown, key: string, maxSize: number): value is CiSourceObject {
  if (value == null || typeof value !== "object") return false;
  const object = value as Partial<CiSourceObject>;
  return object.key === key &&
    typeof object.size === "number" && Number.isSafeInteger(object.size) &&
    object.size > 0 && object.size <= maxSize &&
    typeof object.sha256 === "string" && SHA256_PATTERN.test(object.sha256);
}

function isSafeRepositoryPath(path: string): boolean {
  if (
    path.length === 0 || path.length > 4_096 || path.startsWith("/") ||
    path.includes("\0") || path.includes("\\")
  ) return false;
  return path.split("/").every((part) => part !== "" && part !== "." && part !== "..");
}
