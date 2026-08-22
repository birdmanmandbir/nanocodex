import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  GitHubRepository,
  GitHubRequestError,
  GitHubTransportError,
  MirrorConflictError,
  MirrorValidationError,
  assertStableLatestDoesNotRollback,
  assertTargetIsHighestPublishedStable,
  canonicalJson,
  captureStableTagAuthority,
  compatibilityBody,
  createGitHubChildClient,
  downloadPublicGitHubAsset,
  fetchCanonicalCompatibilityRelease,
  githubChildEnvironment,
  mirrorCompatibilityRelease,
  parseGitHubRelease,
  parseImmutableIdentity,
  planCompatibilityMirror,
  runMirrorCommand,
  sha256Hex,
  validateChecksumManifest,
  validatePublicManifest,
  withLocalMirrorLock,
  withWorkerPublicationLease,
} from "./mirror-cloudflare-release-to-github.mjs";

const origin = "https://release.test";
const bootstrapCommit = "e4eea49fc6fab06a98ff01ec8c3da8d9a729eee1";
const encoder = new TextEncoder();

function releaseInventory(channel, id, commit) {
  if (channel === "stable" && id === "v0.5.0") {
    return [
      ["PROVENANCE.json", "linux", "application/json"],
      ["SHA256SUMS", "linux", "text/plain"],
      ["nanocodex-aarch64-apple-darwin", "aarch64-apple-darwin", "application/octet-stream"],
      ["nanocodex-x86_64-unknown-linux-gnu", "x86_64-unknown-linux-gnu", "application/octet-stream"],
      ["nanocodex-x86_64-unknown-linux-gnu.gz", "x86_64-unknown-linux-gnu", "application/gzip"],
    ].sort(([left], [right]) => left.localeCompare(right));
  }
  const npm = channel === "stable"
    ? `nanocodex-${id.slice(1)}.tgz`
    : `nanocodex-${commit.slice(0, 10)}.tgz`;
  return [
    ["PROVENANCE.json", "linux", "application/json"],
    ["SHA256SUMS", "linux", "text/plain; charset=utf-8"],
    [npm, "npm", "application/gzip"],
    ["nanocodex-aarch64-apple-darwin", "aarch64-apple-darwin", "application/octet-stream"],
    ["nanocodex-vm-guest-x86_64-unknown-linux-musl", "x86_64-unknown-linux-musl", "application/octet-stream"],
    ["nanocodex-vm-guest-x86_64-unknown-linux-musl.gz", "x86_64-unknown-linux-musl", "application/gzip"],
    ["nanocodex-x86_64-unknown-linux-gnu", "x86_64-unknown-linux-gnu", "application/octet-stream"],
    ["nanocodex-x86_64-unknown-linux-gnu.gz", "x86_64-unknown-linux-gnu", "application/gzip"],
  ].sort(([left], [right]) => left.localeCompare(right));
}

function fixture({
  channel = "stable",
  id = "v0.5.0",
  commit = channel === "stable" ? bootstrapCommit : id,
  payload = "exact bytes",
} = {}) {
  const kind = channel === "stable" ? "stable" : "commit";
  const tag = channel === "stable" ? id : `nightly-${id}`;
  const bytes = new Map();
  const inventory = releaseInventory(channel, id, commit);
  for (const [name] of inventory) {
    if (name !== "SHA256SUMS") bytes.set(name, encoder.encode(`${payload} for ${name} at ${commit}`));
  }
  const sums = inventory
    .filter(([name]) => name !== "SHA256SUMS")
    .map(([name]) => `${sha256Hex(bytes.get(name))}  ${name}\n`)
    .join("");
  bytes.set("SHA256SUMS", encoder.encode(sums));
  const assets = inventory.map(([name, platform, contentType]) => ({
    name,
    platform,
    size: bytes.get(name).byteLength,
    sha256: sha256Hex(bytes.get(name)),
    contentType,
    downloadPath:
      `/api/releases/releases/${kind}/${encodeURIComponent(id)}/assets/${encodeURIComponent(name)}`,
  }));
  const unsigned = {
    version: 1,
    kind,
    id,
    tag,
    commit,
    channel: channel === "stable" ? "latest" : "nightly",
    finalizedAt: "2026-08-22T10:00:00.000Z",
    assets,
  };
  const manifest = { ...unsigned, manifestSha256: sha256Hex(canonicalJson(unsigned)) };
  const pointer = {
    version: 1,
    channel: unsigned.channel,
    kind,
    id,
    tag,
    commit,
    generation: 1,
    updatedAt: "2026-08-22T10:00:01.000Z",
  };
  const identity = { channel, kind, id };
  const selected = [
    "SHA256SUMS",
    "nanocodex-aarch64-apple-darwin",
    "nanocodex-x86_64-unknown-linux-gnu",
  ].map((name) => ({
    ...assets.find((asset) => asset.name === name),
    bytes: bytes.get(name),
  }));
  const manifestPath = `/api/releases/releases/${kind}/${encodeURIComponent(id)}`;
  return {
    origin,
    identity,
    manifest,
    pointer,
    assets: selected,
    manifestUrl: `${origin}${manifestPath}`,
    bytes,
    manifestPath,
  };
}

function workerFetch(source, override = () => undefined) {
  return async (input, init) => {
    const url = new URL(input);
    assert.equal(init.redirect, "manual");
    const replacement = override(url, init);
    if (replacement) return replacement;
    if (url.pathname === source.manifestPath) {
      return new Response(JSON.stringify(source.manifest), {
        headers: {
          "cache-control": "public, max-age=31536000, immutable",
          "content-type": "application/json; charset=utf-8",
          etag: `"${source.manifest.manifestSha256}"`,
        },
      });
    }
    if (url.pathname === `/api/releases/channels/${source.pointer.channel}`) {
      return new Response(JSON.stringify({ pointer: source.pointer, manifest: source.manifest }), {
        headers: {
          "cache-control": "no-store",
          "content-location": source.manifestPath,
          "content-type": "application/json; charset=utf-8",
        },
      });
    }
    const asset = source.manifest.assets.find((candidate) => candidate.downloadPath === url.pathname);
    if (asset) {
      return new Response(source.bytes.get(asset.name), {
        headers: {
          "cache-control": "public, max-age=31536000, immutable",
          "content-disposition": `attachment; filename="${asset.name}"`,
          "content-length": String(asset.size),
          "content-type": asset.contentType,
          etag: `"${asset.sha256}"`,
          "x-nanocodex-release": source.manifest.id,
          "x-nanocodex-sha256": asset.sha256,
        },
      });
    }
    return new Response("missing", { status: 404 });
  };
}

function githubAsset(sourceAsset, id, tag) {
  return {
    id,
    name: sourceAsset.name,
    state: "uploaded",
    size: sourceAsset.size,
    digest: sourceAsset.sha256,
    contentType: sourceAsset.contentType,
    browserDownloadUrl:
      `https://github.com/gakonst/nanocodex/releases/download/${encodeURIComponent(tag)}/${encodeURIComponent(sourceAsset.name)}`,
    uploaderLogin: "github-actions[bot]",
  };
}

function annotatedTagRef(tag, commit, objectSha = "9".repeat(40)) {
  return {
    tag,
    ref: `refs/tags/${tag}`,
    object: { type: "tag", sha: objectSha },
    tagObjects: [{
      sha: objectSha,
      tag,
      object: { type: "commit", sha: commit },
    }],
    commit,
  };
}

function lightweightTagRef(tag, commit) {
  return {
    tag,
    ref: `refs/tags/${tag}`,
    object: { type: "commit", sha: commit },
    tagObjects: [],
    commit,
  };
}

function publishedCompatibilityRelease(source, {
  id = 500,
  body = compatibilityBody(source, {
    phase: "published",
    previousCommit: null,
    previousAssets: [],
  }),
} = {}) {
  const nightly = source.identity.channel === "nightly";
  const tag = nightly ? "nightly" : source.identity.id;
  return {
    id,
    tag,
    name: nightly ? "Nanocodex Nightly" : `Nanocodex ${tag}`,
    body,
    draft: false,
    prerelease: nightly,
    immutable: false,
    authorLogin: "github-actions[bot]",
    assets: source.assets.map((asset, index) => githubAsset(asset, id * 10 + index, tag)),
  };
}

class FakeGitHub {
  constructor(source, {
    release = null,
    refCommit = source.manifest.commit,
    latest = null,
    tagObjectSha = "9".repeat(40),
    publishedStable = [],
  } = {}) {
    this.source = source;
    this.release = release;
    this.refCommit = refCommit;
    this.tagObjectSha = tagObjectSha;
    this.latest = latest;
    this.publishedStable = publishedStable;
    this.publicFetch = async () => new Response("unused", { status: 500 });
    this.sleep = async () => undefined;
    this.createdAmbiguously = false;
    this.publishedAmbiguously = false;
    this.uploadedAmbiguously = new Set();
    this.mutations = [];
  }

  async getWorkflowPublisherState(file) {
    return { id: 1, path: `.github/workflows/${file}`, state: "disabled_manually", active: [] };
  }

  async assertRollingReleasePolicyMutable() {}
  async getRef(tag) {
    if (this.refCommit == null) return null;
    return tag === "nightly"
      ? lightweightTagRef(tag, this.refCommit)
      : annotatedTagRef(tag, this.refCommit, this.tagObjectSha);
  }
  async getReleaseByTag() { return this.release; }
  async getRelease() { return this.release; }
  async getLatestRelease() { return this.latest; }
  async getPublicUpdaterRelease() { return this.release; }
  async getPublishedStableReleases() {
    const releases = [...this.publishedStable];
    for (const release of [this.release, this.latest]) {
      if (release != null && /^v[0-9]+\.[0-9]+\.[0-9]+$/.test(release.tag) &&
        !release.draft && !release.prerelease &&
        !releases.some((candidate) => candidate.id === release.id)) {
        releases.push(release);
      }
    }
    return releases;
  }

  async createRelease(input) {
    this.mutations.push("create-release");
    this.lastCreateInput = input;
    this.release = {
      id: 101,
      tag: input.tag_name,
      name: input.name,
      body: input.body,
      draft: input.draft,
      prerelease: input.prerelease,
      immutable: false,
      authorLogin: "github-actions[bot]",
      assets: [],
    };
    if (!this.createdAmbiguously) {
      this.createdAmbiguously = true;
      throw new GitHubTransportError("lost create acknowledgement");
    }
    return this.release;
  }

  async updateRelease(_id, _tag, input) {
    this.mutations.push(`update-release:${String(input.draft)}`);
    this.release = {
      ...this.release,
      ...(input.tag_name == null ? {} : { tag: input.tag_name }),
      ...(input.name == null ? {} : { name: input.name }),
      ...(input.body == null ? {} : { body: input.body }),
      ...(input.draft == null ? {} : { draft: input.draft }),
      ...(input.prerelease == null ? {} : { prerelease: input.prerelease }),
    };
    if (input.make_latest === "true") this.latest = this.release;
    if (input.draft === false && !this.publishedAmbiguously) {
      this.publishedAmbiguously = true;
      throw new GitHubTransportError("lost publish acknowledgement");
    }
    return this.release;
  }

  async uploadAsset(_releaseId, asset) {
    this.mutations.push(`upload:${asset.name}`);
    const uploaded = githubAsset(asset, 200 + this.release.assets.length, this.release.tag);
    this.release = { ...this.release, assets: [...this.release.assets, uploaded] };
    if (!this.uploadedAmbiguously.has(asset.name)) {
      this.uploadedAmbiguously.add(asset.name);
      throw new GitHubTransportError("lost upload acknowledgement");
    }
    return uploaded;
  }

  async deleteAsset(_releaseId, id) {
    this.mutations.push(`delete:${id}`);
    this.release = { ...this.release, assets: this.release.assets.filter((asset) => asset.id !== id) };
  }

  async updateRef(_tag, commit) {
    this.mutations.push("update-ref");
    this.refCommit = commit;
    return { tag: "nightly", commit };
  }

  async createRef(_tag, commit) { return this.updateRef("nightly", commit); }

  async verifyPublicAsset(asset, expected) {
    assert.equal(asset.name, expected.name);
    assert.equal(asset.digest, expected.sha256);
    return { bytes: expected.bytes, sha256: expected.sha256, finalUrl: asset.browserDownloadUrl };
  }
}

test("immutable identity and canonical manifest reject alternate spellings and inventories", () => {
  assert.deepEqual(parseImmutableIdentity("stable", "v0.5.0"), {
    channel: "stable",
    kind: "stable",
    id: "v0.5.0",
  });
  for (const value of ["0.5.0", "v00.5.0", "v0.5", "v0.5.0-rc1"]) {
    assert.throws(() => parseImmutableIdentity("stable", value), MirrorValidationError);
  }
  const source = fixture();
  assert.equal(validatePublicManifest(source.manifest, source.identity).manifestSha256,
    source.manifest.manifestSha256);
  const expanded = structuredClone(source.manifest);
  expanded.assets.push({ ...expanded.assets[0], name: "unexpected", downloadPath: `${source.manifestPath}/assets/unexpected` });
  expanded.assets.sort((left, right) => left.name.localeCompare(right.name));
  const { manifestSha256: _old, ...unsigned } = expanded;
  expanded.manifestSha256 = sha256Hex(canonicalJson(unsigned));
  assert.throws(() => validatePublicManifest(expanded, source.identity), /noncanonical asset inventory/);
});

test("canonical Worker fetch proves headers, channel equality, exact bytes, and SHA256SUMS", async () => {
  const source = fixture();
  const fetched = await fetchCanonicalCompatibilityRelease({
    origin,
    channel: "stable",
    id: "v0.5.0",
    fetchImpl: workerFetch(source),
  });
  assert.deepEqual(fetched.assets.map((asset) => asset.name), source.assets.map((asset) => asset.name));
  assert.deepEqual(planCompatibilityMirror(fetched).assets.map((asset) => asset.sha256),
    source.assets.map((asset) => asset.sha256));

  await assert.rejects(fetchCanonicalCompatibilityRelease({
    origin,
    channel: "stable",
    id: "v0.5.0",
    fetchImpl: workerFetch(source, (url) => url.pathname === source.manifestPath
      ? new Response(null, { status: 302, headers: { location: `${origin}/elsewhere` } })
      : undefined),
  }), /redirected/);
});

test("checksum validation rejects a BOM that the Rust v0.5 parser would retain", () => {
  const source = fixture();
  const original = source.assets.find((asset) => asset.name === "SHA256SUMS").bytes;
  const bom = new Uint8Array([0xef, 0xbb, 0xbf, ...original]);
  assert.throws(() => validateChecksumManifest(bom, source.manifest.assets), /BOM/);
});

test("GitHub child boundary isolates credentials and rejects out-of-scope mutations", async () => {
  assert.deepEqual(githubChildEnvironment("secret"), {
    NANOCODEX_GITHUB_RELEASE_TOKEN: "secret",
  });
  let calls = 0;
  const client = createGitHubChildClient({
    token: "secret",
    runChild: async () => {
      calls += 1;
      return { version: 1, status: 200, headers: {}, body: Buffer.from("[]").toString("base64") };
    },
  });
  await assert.rejects(client.request({
    method: "DELETE",
    url: "https://api.github.com/repos/gakonst/nanocodex/branches/master",
  }), /allowlist/);
  await assert.rejects(client.request({
    method: "POST",
    url: "https://api.github.com/repos/gakonst/nanocodex/git/refs",
    json: { ref: "refs/tags/v9.9.9", sha: "a".repeat(40) },
  }), /nightly/);
  await assert.rejects(client.request({
    method: "POST",
    url: "https://api.github.com/repos/gakonst/nanocodex/releases",
    json: {
      tag_name: "nightly",
      target_commitish: "a".repeat(40),
      name: "Nanocodex Nightly",
      body: "body",
      draft: true,
      prerelease: true,
      make_latest: "false",
    },
  }), /limited to one non-latest stable draft/);
  await assert.rejects(client.request({
    method: "PATCH",
    url: "https://api.github.com/repos/gakonst/nanocodex/releases/1",
    json: { target_commitish: "a".repeat(40) },
  }), /unexpected fields/);
  assert.equal(calls, 0);
});

test("GitHub release parser requires explicit immutability and fixed download URLs", () => {
  const source = fixture();
  const asset = source.assets[0];
  const raw = {
    id: 1,
    tag_name: "v0.5.0",
    name: "Nanocodex v0.5.0",
    body: "",
    draft: false,
    prerelease: false,
    author: { login: "github-actions[bot]" },
    assets: [],
  };
  assert.throws(() => parseGitHubRelease(raw, "v0.5.0"), /metadata/);
  raw.immutable = false;
  raw.assets = [{
    id: 2,
    name: asset.name,
    state: "uploaded",
    size: asset.size,
    digest: `sha256:${asset.sha256}`,
    content_type: asset.contentType,
    browser_download_url: `https://evil.test/${asset.name}`,
    uploader: { login: "github-actions[bot]" },
  }];
  assert.throws(() => parseGitHubRelease(raw, "v0.5.0"), /escaped/);
});

test("public GitHub proof uses the v0.5 asset_id cache-buster and rejects foreign redirects", async () => {
  const source = fixture();
  const expected = source.assets[1];
  const asset = githubAsset(expected, 42, "v0.5.0");
  let observed;
  const verified = await downloadPublicGitHubAsset({
    githubAsset: asset,
    maximumBytes: 1024 * 1024,
    expectedSize: expected.size,
    expectedSha256: expected.sha256,
    fetchImpl: async (url) => {
      observed = new URL(url);
      return new Response(expected.bytes, {
        headers: { "content-length": String(expected.size) },
      });
    },
  });
  assert.equal(observed.search, "?asset_id=42");
  assert.equal(verified.sha256, expected.sha256);
  await assert.rejects(downloadPublicGitHubAsset({
    githubAsset: asset,
    maximumBytes: 1024 * 1024,
    expectedSize: expected.size,
    expectedSha256: expected.sha256,
    fetchImpl: async () => new Response(null, {
      status: 302,
      headers: { location: "https://evil.test/asset" },
    }),
  }), /outside GitHub/);
});

test("stable reconciliation survives lost acknowledgements and verifies public bytes", async () => {
  const source = fixture();
  const github = new FakeGitHub(source);
  const result = await mirrorCompatibilityRelease({
    source,
    github,
    refreshSource: async () => source,
  });
  assert.equal(result.status, "published");
  assert.equal(github.release.draft, false);
  assert.equal(github.latest.id, github.release.id);
  assert.deepEqual(github.release.assets.map((asset) => asset.name).sort(),
    source.assets.map((asset) => asset.name).sort());
  assert.equal(github.mutations.filter((value) => value === "create-release").length, 1);
  assert.equal(github.lastCreateInput.target_commitish, source.manifest.commit);
  for (const asset of source.assets) {
    assert.equal(github.mutations.filter((value) => value === `upload:${asset.name}`).length, 1);
  }
});

test("an ambiguous mutation is never repeated until a successful observation", async () => {
  const source = fixture();
  class UnobservableGitHub extends FakeGitHub {
    async createRelease(input) {
      try {
        return await super.createRelease(input);
      } finally {
        this.observationFailed = true;
      }
    }

    async getReleaseByTag() {
      if (this.observationFailed) {
        throw new GitHubTransportError("observation transport failed");
      }
      return null;
    }
  }
  const github = new UnobservableGitHub(source);
  await assert.rejects(mirrorCompatibilityRelease({ source, github }), /fresh invocation/);
  assert.equal(github.mutations.filter((value) => value === "create-release").length, 1);
});

test("a release creation that becomes visible after one stale read is never reissued", async () => {
  const source = fixture();
  class LateCreateGitHub extends FakeGitHub {
    async createRelease(input) {
      this.mutations.push("create-release");
      this.lastCreateInput = input;
      this.pendingRelease = {
        id: 101,
        tag: input.tag_name,
        name: input.name,
        body: input.body,
        draft: input.draft,
        prerelease: input.prerelease,
        immutable: false,
        authorLogin: "github-actions[bot]",
        assets: [],
      };
      throw new GitHubTransportError("create committed after its response was lost");
    }

    async getReleaseByTag() {
      if (this.pendingRelease != null) {
        if (!this.returnedStaleCreateRead) {
          this.returnedStaleCreateRead = true;
          return null;
        }
        this.release = this.pendingRelease;
        this.pendingRelease = null;
      }
      return this.release;
    }
  }
  const github = new LateCreateGitHub(source);
  const result = await mirrorCompatibilityRelease({
    source,
    github,
    refreshSource: async () => source,
  });
  assert.equal(result.status, "published");
  assert.equal(github.mutations.filter((value) => value === "create-release").length, 1);
});

test("an asset upload that becomes visible after one stale read is never reissued", async () => {
  const source = fixture();
  class LateUploadGitHub extends FakeGitHub {
    async uploadAsset(_releaseId, asset) {
      this.mutations.push(`upload:${asset.name}`);
      this.pendingUpload = {
        asset: githubAsset(asset, 800 + this.mutations.length, this.release.tag),
        staleReadReturned: false,
      };
      throw new GitHubTransportError("upload committed after its response was lost");
    }

    async getRelease() {
      if (this.pendingUpload != null) {
        if (!this.pendingUpload.staleReadReturned) {
          this.pendingUpload.staleReadReturned = true;
          return this.release;
        }
        this.release = {
          ...this.release,
          assets: [...this.release.assets, this.pendingUpload.asset],
        };
        this.pendingUpload = null;
      }
      return this.release;
    }
  }
  const github = new LateUploadGitHub(source);
  const result = await mirrorCompatibilityRelease({
    source,
    github,
    refreshSource: async () => source,
  });
  assert.equal(result.status, "published");
  for (const asset of source.assets) {
    assert.equal(github.mutations.filter((value) => value === `upload:${asset.name}`).length, 1);
  }
});

test("stable make_latest refuses semver rollback", () => {
  assert.throws(() => assertStableLatestDoesNotRollback(
    { id: 2, tag: "v0.7.0", draft: false, prerelease: false },
    { id: 1 },
    "v0.6.0",
  ), /back/);
});

test("stable authority pins one direct annotated object and its exact commit target", () => {
  const source = fixture();
  const ref = annotatedTagRef(source.identity.id, source.manifest.commit);
  const authority = captureStableTagAuthority(
    ref,
    source.identity.id,
    source.manifest.commit,
  );
  assert.deepEqual(authority.object, { type: "tag", sha: "9".repeat(40) });
  assert.deepEqual(authority.target, { type: "commit", sha: source.manifest.commit });
  assert.throws(() => captureStableTagAuthority(
    lightweightTagRef(source.identity.id, source.manifest.commit),
    source.identity.id,
    source.manifest.commit,
  ), /direct annotated tag/);
  assert.throws(() => captureStableTagAuthority(
    annotatedTagRef(source.identity.id, "a".repeat(40)),
    source.identity.id,
    source.manifest.commit,
  ), /expected/);
});

test("stable tag deletion immediately before release creation is detected", async () => {
  const source = fixture();
  class DeletedTagGitHub extends FakeGitHub {
    async getRef(tag) {
      this.refReads = (this.refReads ?? 0) + 1;
      return this.refReads === 1 ? super.getRef(tag) : null;
    }
  }
  const github = new DeletedTagGitHub(source);
  await assert.rejects(mirrorCompatibilityRelease({ source, github }), /tag v0\.5\.0 is missing/);
  assert.deepEqual(github.mutations, []);
});

test("stable tag deletion during release creation can never produce success", async () => {
  const source = fixture();
  class DeletedDuringCreateGitHub extends FakeGitHub {
    async createRelease(input) {
      try {
        return await super.createRelease(input);
      } finally {
        this.refCommit = null;
      }
    }
  }
  const github = new DeletedDuringCreateGitHub(source);
  await assert.rejects(mirrorCompatibilityRelease({ source, github }), /tag v0\.5\.0 is missing/);
  assert.equal(github.mutations.filter((value) => value === "create-release").length, 1);
});

test("stable tag recreation during release creation can never produce success", async () => {
  const source = fixture();
  class RecreatedTagGitHub extends FakeGitHub {
    async createRelease(input) {
      try {
        return await super.createRelease(input);
      } finally {
        this.tagObjectSha = "8".repeat(40);
      }
    }
  }
  const github = new RecreatedTagGitHub(source);
  await assert.rejects(
    mirrorCompatibilityRelease({ source, github }),
    /deleted, recreated, or moved/,
  );
  assert.equal(github.mutations.filter((value) => value === "create-release").length, 1);
});

test("stable tag recreation during make_latest can never produce success", async () => {
  const source = fixture({
    id: "v0.6.0",
    commit: "6".repeat(40),
  });
  const release = publishedCompatibilityRelease(source);
  class RecreatedLatestTagGitHub extends FakeGitHub {
    async updateRelease(id, tag, input) {
      const result = await super.updateRelease(id, tag, input);
      this.tagObjectSha = "8".repeat(40);
      return result;
    }
  }
  const github = new RecreatedLatestTagGitHub(source, {
    release,
    latest: { id: 10, tag: "v0.5.0", draft: false, prerelease: false },
  });
  await assert.rejects(
    mirrorCompatibilityRelease({ source, github, refreshSource: async () => source }),
    /deleted, recreated, or moved/,
  );
  assert.equal(
    github.mutations.filter((value) => value === "update-release:undefined").length,
    1,
  );
});

test("stable latest compares the complete published semver set before mutation", async () => {
  const source = fixture({
    id: "v0.6.0",
    commit: "6".repeat(40),
  });
  const release = publishedCompatibilityRelease(source);
  const higher = { id: 700, tag: "v0.7.0", draft: false, prerelease: false };
  assert.throws(
    () => assertTargetIsHighestPublishedStable([higher], release, source.identity.id),
    /below published v0\.7\.0/,
  );
  const github = new FakeGitHub(source, {
    release,
    latest: { id: 10, tag: "v0.5.0", draft: false, prerelease: false },
    publishedStable: [higher],
  });
  await assert.rejects(
    mirrorCompatibilityRelease({ source, github, refreshSource: async () => source }),
    /below published v0\.7\.0/,
  );
  assert.deepEqual(github.mutations, []);
});

test("stable latest rechecks the complete published semver set after mutation", async () => {
  const source = fixture({
    id: "v0.6.0",
    commit: "6".repeat(40),
  });
  const release = publishedCompatibilityRelease(source);
  const higher = { id: 700, tag: "v0.7.0", draft: false, prerelease: false };
  class LateHigherStableGitHub extends FakeGitHub {
    async getPublishedStableReleases() {
      this.stableReads = (this.stableReads ?? 0) + 1;
      const releases = await super.getPublishedStableReleases();
      return this.stableReads === 1 ? releases : [...releases, higher];
    }
  }
  const github = new LateHigherStableGitHub(source, {
    release,
    latest: { id: 10, tag: "v0.5.0", draft: false, prerelease: false },
  });
  await assert.rejects(
    mirrorCompatibilityRelease({ source, github, refreshSource: async () => source }),
    /below published v0\.7\.0/,
  );
  assert.equal(
    github.mutations.filter((value) => value === "update-release:undefined").length,
    1,
  );
});

test("stable reconciliation fails if the final authenticated latest view moves", async () => {
  const source = fixture({
    id: "v0.6.0",
    commit: "6".repeat(40),
  });
  const release = publishedCompatibilityRelease(source);
  class MovedFinalLatestGitHub extends FakeGitHub {
    async getLatestRelease() {
      this.latestReads = (this.latestReads ?? 0) + 1;
      return this.latestReads === 1
        ? this.latest
        : { id: 700, tag: "v0.7.0", draft: false, prerelease: false };
    }
  }
  const github = new MovedFinalLatestGitHub(source, { release, latest: release });
  await assert.rejects(
    mirrorCompatibilityRelease({ source, github, refreshSource: async () => source }),
    /latest release does not resolve/,
  );
  assert.deepEqual(github.mutations, []);
});

test("stable reconciliation rechecks annotated-tag identity after final public bytes", async () => {
  const source = fixture();
  const release = publishedCompatibilityRelease(source);
  class FinalByteTagRaceGitHub extends FakeGitHub {
    async verifyPublicAsset(asset, expected) {
      const result = await super.verifyPublicAsset(asset, expected);
      this.publicByteProofs = (this.publicByteProofs ?? 0) + 1;
      if (this.publicByteProofs === source.assets.length * 3) {
        this.tagObjectSha = "8".repeat(40);
      }
      return result;
    }
  }
  const github = new FinalByteTagRaceGitHub(source, { release, latest: release });
  await assert.rejects(
    mirrorCompatibilityRelease({ source, github, refreshSource: async () => source }),
    /deleted, recreated, or moved/,
  );
  assert.deepEqual(github.mutations, []);
});

test("exact legacy seven-asset nightly is destructively adopted, while altered provenance is refused", async () => {
  const previousCommit = "a".repeat(40);
  const current = fixture({ channel: "nightly", id: "b".repeat(40) });
  const names = [
    "SHA256SUMS",
    "nanocodex-aarch64-apple-darwin",
    "nanocodex-aarch64-apple-darwin.gz",
    "nanocodex-vm-guest-x86_64-unknown-linux-musl",
    "nanocodex-vm-guest-x86_64-unknown-linux-musl.gz",
    "nanocodex-x86_64-unknown-linux-gnu",
    "nanocodex-x86_64-unknown-linux-gnu.gz",
  ];
  const bytes = new Map(names
    .filter((name) => name !== "SHA256SUMS")
    .map((name) => [name, encoder.encode(`legacy ${name} ${previousCommit}`)]));
  bytes.set("SHA256SUMS", encoder.encode(names
    .filter((name) => name !== "SHA256SUMS")
    .map((name) => `${sha256Hex(bytes.get(name))}  ${name}\n`)
    .join("")));
  const assets = names.map((name, index) => ({
    id: 700 + index,
    name,
    state: "uploaded",
    size: bytes.get(name).byteLength,
    digest: sha256Hex(bytes.get(name)),
    contentType: name === "SHA256SUMS"
      ? "text/plain"
      : name.endsWith(".gz")
      ? "application/gzip"
      : "application/octet-stream",
    browserDownloadUrl:
      `https://github.com/gakonst/nanocodex/releases/download/nightly/${encodeURIComponent(name)}`,
    uploaderLogin: "github-actions[bot]",
  }));
  const body =
    `Automated nightly build from [\`${previousCommit.slice(0, 10)}\`](https://github.com/gakonst/nanocodex/commit/${previousCommit}).\n\n` +
    "Install it with `nanocodex update --nightly`.\n";
  const legacy = {
    id: 699,
    tag: "nightly",
    name: "Nanocodex Nightly",
    body,
    draft: false,
    prerelease: true,
    immutable: false,
    authorLogin: "github-actions[bot]",
    assets,
  };
  const makeGitHub = (release) => {
    const github = new FakeGitHub(current, {
      release,
      refCommit: previousCommit,
    });
    github.publicFetch = async (input) => {
      const name = decodeURIComponent(new URL(input).pathname.split("/").at(-1));
      const bodyBytes = bytes.get(name);
      return bodyBytes == null
        ? new Response("missing", { status: 404 })
        : new Response(bodyBytes, {
            headers: { "content-length": String(bodyBytes.byteLength) },
          });
    };
    return github;
  };

  const github = makeGitHub(legacy);
  const result = await mirrorCompatibilityRelease({
    source: current,
    github,
    refreshSource: async () => current,
  });
  assert.equal(result.status, "published");
  assert.equal(github.refCommit, current.manifest.commit);
  assert.equal(github.mutations.filter((value) => value.startsWith("delete:")).length, 7);
  const lastUpload = Math.max(...github.mutations.map((value, index) =>
    value.startsWith("upload:") ? index : -1));
  assert.ok(github.mutations.indexOf("update-ref") > lastUpload);

  const altered = {
    ...legacy,
    assets: legacy.assets.map((asset, index) => index === 0
      ? { ...asset, uploaderLogin: "untrusted-user" }
      : asset),
  };
  const rejected = makeGitHub(altered);
  await assert.rejects(mirrorCompatibilityRelease({
    source: current,
    github: rejected,
    refreshSource: async () => current,
  }), /neither bridge-owned nor the exact legacy Actions release/);
  assert.deepEqual(rejected.mutations, []);
});

test("published bridge-owned nightly is adopted only after its immutable source bytes match", async () => {
  const previous = fixture({ channel: "nightly", id: "a".repeat(40) });
  const current = fixture({ channel: "nightly", id: "b".repeat(40) });
  const release = publishedCompatibilityRelease(previous);
  const github = new FakeGitHub(current, {
    release,
    refCommit: previous.manifest.commit,
  });
  const fetched = [];
  const result = await mirrorCompatibilityRelease({
    source: current,
    github,
    refreshSource: async () => current,
    fetchImmutableSource: async ({ channel, id }) => {
      fetched.push({ channel, id });
      return previous;
    },
  });
  assert.equal(result.status, "published");
  assert.deepEqual(fetched, [{ channel: "nightly", id: previous.identity.id }]);
  assert.equal(github.refCommit, current.manifest.commit);
});

test("a forged self-consistent published nightly marker cannot authorize adoption", async () => {
  const previousId = "a".repeat(40);
  const canonicalPrevious = fixture({
    channel: "nightly",
    id: previousId,
    payload: "canonical immutable bytes",
  });
  const forgedPrevious = fixture({
    channel: "nightly",
    id: previousId,
    payload: "forged self-consistent bytes",
  });
  const current = fixture({ channel: "nightly", id: "b".repeat(40) });
  const github = new FakeGitHub(current, {
    release: publishedCompatibilityRelease(forgedPrevious),
    refCommit: previousId,
  });
  await assert.rejects(mirrorCompatibilityRelease({
    source: current,
    github,
    refreshSource: async () => current,
    fetchImmutableSource: async ({ channel, id }) => {
      assert.deepEqual({ channel, id }, { channel: "nightly", id: previousId });
      return canonicalPrevious;
    },
  }), /marker targets another release/);
  assert.deepEqual(github.mutations, []);
});

test("rolling nightly resumes an owned stale draft using explicit GitHub digests", async () => {
  const previous = fixture({ channel: "nightly", id: "a".repeat(40) });
  const current = fixture({ channel: "nightly", id: "b".repeat(40) });
  const oldBody = compatibilityBody(previous, {
    phase: "staging",
    previousCommit: "c".repeat(40),
    previousAssets: [],
  });
  const release = {
    id: 501,
    tag: "nightly",
    name: "Nanocodex Nightly",
    body: oldBody,
    draft: true,
    prerelease: true,
    immutable: false,
    authorLogin: "github-actions[bot]",
    assets: previous.assets.map((asset, index) => githubAsset(asset, 600 + index, "nightly")),
  };
  const github = new FakeGitHub(current, { release, refCommit: previous.manifest.commit });
  const result = await mirrorCompatibilityRelease({
    source: current,
    github,
    refreshSource: async () => current,
    fetchImmutableSource: async ({ id }) => {
      assert.equal(id, previous.identity.id);
      return previous;
    },
  });
  assert.equal(result.status, "published");
  assert.equal(github.refCommit, current.manifest.commit);
  assert.equal(github.release.draft, false);
  assert.ok(github.mutations.indexOf("update-ref") >
    github.mutations.findIndex((value) => value.startsWith("upload:")));
});

test("nightly bootstrap and unowned stable drafts fail closed", async () => {
  const nightly = fixture({ channel: "nightly", id: "d".repeat(40) });
  await assert.rejects(mirrorCompatibilityRelease({
    source: nightly,
    github: new FakeGitHub(nightly, { release: null, refCommit: null }),
  }), /bootstrap requires explicit operator repair/);

  const stable = fixture();
  const rogue = {
    id: 9,
    tag: "v0.5.0",
    name: "Nanocodex v0.5.0",
    body: "not owned",
    draft: true,
    prerelease: false,
    immutable: false,
    authorLogin: "attacker",
    assets: [],
  };
  await assert.rejects(mirrorCompatibilityRelease({
    source: stable,
    github: new FakeGitHub(stable, { release: rogue }),
  }), /not the owned staging release/);
});

test("nightly immutable-policy 404 is accepted only after Administration(read) proof", async () => {
  const json = (status, value) => ({
    status,
    headers: {},
    body: encoder.encode(JSON.stringify(value)),
  });
  let request = 0;
  const repository = new GitHubRepository({
    client: {
      async request() {
        request += 1;
        return request === 1
          ? json(200, { enabled: true, allowed_actions: "all" })
          : json(404, { message: "disabled" });
      },
    },
  });
  await repository.assertRollingReleasePolicyMutable();
  const underScoped = new GitHubRepository({
    client: { async request() { return json(404, { message: "not found" }); } },
  });
  await assert.rejects(underScoped.assertRollingReleasePolicyMutable(), GitHubRequestError);
});

test("publication lease pins credentials to the Worker and fences on heartbeat loss", async () => {
  const calls = [];
  const lease = {
    version: 1,
    leaseId: "1.12345678-1234-4123-8123-123456789abc",
    owner: "fixture-owner",
    kind: "commit",
    id: "e".repeat(40),
    commit: "e".repeat(40),
    generation: 1,
    expiresAt: new Date(Date.now() + 60_000).toISOString(),
  };
  await withWorkerPublicationLease({
    origin: "https://release.test",
    token: "worker-secret",
    channel: "nightly",
    id: lease.id,
    commit: lease.commit,
    owner: lease.owner,
    allowFixtureOrigin: true,
    fetchImpl: async (url, init) => {
      calls.push({ url: new URL(url), init });
      assert.equal(init.headers.authorization, "Bearer worker-secret");
      return init.method === "DELETE"
        ? new Response(null, { status: 204 })
        : new Response(JSON.stringify(lease), {
            status: calls.length === 1 ? 201 : 200,
            headers: { "content-type": "application/json" },
          });
    },
  }, async (authority) => {
    assert.equal(authority.signal.aborted, false);
    await authority.assertHeld();
  });
  assert.equal(calls.at(-1).init.method, "DELETE");
  assert.ok(calls.every(({ url }) => url.origin === "https://release.test"));
});

test("dry-run accepts fixture origins without credentials and local lock is exclusive", async () => {
  const source = fixture();
  const result = await runMirrorCommand({
    args: ["--dry-run", "--origin", origin, "stable", "v0.5.0"],
    env: {},
    fetchImpl: workerFetch(source),
  });
  assert.equal(result.mode, "dry-run");
  const directory = await mkdtemp(join(tmpdir(), "nanocodex-mirror-test-"));
  const path = join(directory, "lock");
  try {
    await withLocalMirrorLock({ path }, async () => {
      await assert.rejects(withLocalMirrorLock({ path }, async () => undefined), /another/);
    });
    await withLocalMirrorLock({ path }, async () => undefined);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
