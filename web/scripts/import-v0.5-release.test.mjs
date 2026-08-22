import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { writeFile } from "node:fs/promises";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";

import { __test } from "./import-v0.5-release.mjs";

const TAG = "v0.5.0";
const TAG_OBJECT = "9d8e097cd3eeb87e50809e14d54c46978f72a229";
const COMMIT = "e4eea49fc6fab06a98ff01ec8c3da8d9a729eee1";
const TOKEN = "test-release-token";
const ORIGIN = "https://release.test";
const RELEASE_ROOT = "/api/releases";
const DRAFT_PATH = `${RELEASE_ROOT}/drafts/stable/${TAG}`;
const IMMUTABLE_PATH = `${RELEASE_ROOT}/releases/stable/${TAG}`;
const LATEST_PATH = `${RELEASE_ROOT}/channels/latest`;
const NIGHTLY_PATH = `${RELEASE_ROOT}/channels/nightly`;
const NAMES = Object.freeze({
  mac: "nanocodex-aarch64-apple-darwin",
  linux: "nanocodex-x86_64-unknown-linux-gnu",
  gzip: "nanocodex-x86_64-unknown-linux-gnu.gz",
  provenance: "PROVENANCE.json",
  checksums: "SHA256SUMS",
});

test("production import pins the reviewed evidence and accepts no release argument", async () => {
  const profile = __test.productionProfile;
  assert.equal(profile.tag, TAG);
  assert.equal(profile.tagObject, TAG_OBJECT);
  assert.equal(profile.commit, COMMIT);
  assert.deepEqual(profile.assets, [
    {
      name: NAMES.mac,
      platform: "aarch64-apple-darwin",
      key: `release-import/stable/${TAG}/${NAMES.mac}`,
      size: 79_685_840,
      sha256: "3707e2acbabdf7911eb37ad4c258af1a15ce6c473d5ababdb26841e883f259df",
      contentType: "application/octet-stream",
    },
    {
      name: NAMES.linux,
      platform: "x86_64-unknown-linux-gnu",
      key: `release-import/stable/${TAG}/${NAMES.linux}`,
      size: 88_726_136,
      sha256: "a6920ad69a204e06e2d2fb87af7eacf4b4c84b8b30f054d31f59c7edf40850ae",
      contentType: "application/octet-stream",
    },
    {
      name: NAMES.gzip,
      platform: "x86_64-unknown-linux-gnu",
      key: `release-import/stable/${TAG}/${NAMES.gzip}`,
      size: 34_510_818,
      sha256: "89d5232db3d77cc67d0202bb2dede32e4f715af45d7646c6e896745c44ecbf4f",
      contentType: "application/gzip",
    },
    {
      name: NAMES.provenance,
      platform: "linux",
      key: `release-import/stable/${TAG}/${NAMES.provenance}`,
      size: 830,
      sha256: "2be2c55774a2072e3ffd42c5a07ceae60011ee23fd6192270bbaca99dce6ad66",
      contentType: "application/json",
    },
    {
      name: NAMES.checksums,
      platform: "linux",
      key: `release-import/stable/${TAG}/${NAMES.checksums}`,
      size: 198,
      sha256: "aabb9714794b04352d7f20bf47eb0758173ff39a2687924d0cd8beb71dcf3a76",
      contentType: "text/plain",
    },
  ]);
  assert.equal(profile.provenanceBytes.byteLength, 830);
  assert.equal(
    Buffer.from(profile.checksumBytes).toString("utf8"),
    [
      "3707e2acbabdf7911eb37ad4c258af1a15ce6c473d5ababdb26841e883f259df  nanocodex-aarch64-apple-darwin",
      "a6920ad69a204e06e2d2fb87af7eacf4b4c84b8b30f054d31f59c7edf40850ae  nanocodex-x86_64-unknown-linux-gnu",
      "",
    ].join("\n"),
  );
  assert.deepEqual(profile.provenance.derivations, [{
    name: NAMES.gzip,
    source: NAMES.linux,
    sourceSha256: "a6920ad69a204e06e2d2fb87af7eacf4b4c84b8b30f054d31f59c7edf40850ae",
    method: "gzip -n -9",
    sha256: "89d5232db3d77cc67d0202bb2dede32e4f715af45d7646c6e896745c44ecbf4f",
  }]);
  assert.equal(
    __test.sha256Bytes(profile.provenanceBytes),
    "2be2c55774a2072e3ffd42c5a07ceae60011ee23fd6192270bbaca99dce6ad66",
  );
  assert.equal(profile.draftBytes.byteLength, 1_747);
  assert.equal(
    __test.sha256Bytes(profile.draftBytes),
    "cfb90a5e31a4b3c9ab3943544ca705fe973e4187dbbf3ba6a68d01f86f3613db",
  );
  assert.deepEqual(
    profile.sources.map(({ name, url }) => ({ name, url })),
    [NAMES.checksums, NAMES.mac, NAMES.linux].map((name) => ({
      name,
      url: `https://github.com/gakonst/nanocodex/releases/download/${TAG}/${name}`,
    })),
  );

  const invocation = spawnSync(
    process.execPath,
    [fileURLToPath(new URL("./import-v0.5-release.mjs", import.meta.url)), "v0.6.0"],
    { encoding: "utf8" },
  );
  assert.equal(invocation.status, 1);
  assert.equal(invocation.stdout, "");
  assert.match(invocation.stderr, /accepts no arguments/);
});

test("a failed public source proof performs no release-service mutation", async () => {
  const fixture = fixtureImport();
  const release = new FakeReleaseServer(fixture);
  const github = githubFetcher(fixture, { corrupt: NAMES.mac });
  const tools = fakeTools(fixture);

  await assert.rejects(
    runFixture(fixture, release, github.fetch, tools),
    /public GitHub source hash changed/,
  );
  assert.equal(release.calls.length, 0);
  assert.equal(release.mutations.length, 0);
  assert.equal(tools.gzipCalls.length, 0);
  assert.ok(github.calls.every(({ headers }) =>
    !headers.has("authorization") && !headers.has("cookie")
  ));
});

test("a nonempty release ledger fails closed after proofs and before mutation", async () => {
  const fixture = fixtureImport();
  const release = new FakeReleaseServer(fixture, { conflictingLatest: true });
  const github = githubFetcher(fixture);
  const tools = fakeTools(fixture);

  await assert.rejects(
    runFixture(fixture, release, github.fetch, tools),
    /partial or conflicting v0\.5\.0 cutover/,
  );
  assert.equal(release.mutations.length, 0);
  assert.ok(release.calls.some(({ method, path }) => method === "GET" && path === LATEST_PATH));
  assert.equal(tools.gzipCalls.length, 1);
});

test("lost acknowledgements replay one exact import and a completed rerun is read-only", async () => {
  const fixture = fixtureImport();
  const release = new FakeReleaseServer(fixture, {
    loseDraftAcknowledgement: true,
    loseUploadAcknowledgement: NAMES.linux,
    loseLeaseAcknowledgement: true,
    loseFinalizeAcknowledgement: true,
  });
  const github = githubFetcher(fixture);
  const tools = fakeTools(fixture);

  const first = await runFixture(fixture, release, github.fetch, tools);
  assert.deepEqual(first, { tag: TAG, commit: COMMIT, replayed: false });
  assert.equal(release.count("PUT", DRAFT_PATH), 2);
  assert.equal(
    release.count("PUT", `${DRAFT_PATH}/assets/${encodeURIComponent(NAMES.linux)}`),
    2,
  );
  assert.equal(release.count("POST", `${DRAFT_PATH}/finalize`), 2);
  assert.equal(release.count("POST", `${RELEASE_ROOT}/publication-lease/acquire`), 2);
  assert.equal(release.pointer.generation, 1);
  assert.equal(release.pointer.id, TAG);
  assert.equal(release.lease, null);
  for (const asset of fixture.profile.assets) {
    assert.deepEqual(release.uploads.get(asset.name), fixture.publicBytes.get(asset.name));
    assert.ok(release.count(
      "GET",
      `${IMMUTABLE_PATH}/assets/${encodeURIComponent(asset.name)}`,
    ) >= 1);
    assert.ok(release.count(
      "GET",
      `${LATEST_PATH}/assets/${encodeURIComponent(asset.name)}`,
    ) >= 1);
  }

  const mutationCount = release.mutations.length;
  const second = await runFixture(fixture, release, github.fetch, tools);
  assert.deepEqual(second, { tag: TAG, commit: COMMIT, replayed: true });
  assert.equal(release.mutations.length, mutationCount);
  assert.equal(release.pointer.generation, 1);
  assert.equal(tools.gzipCalls.length, 2);
  assert.equal(
    tools.gitCalls.filter(({ args }) => args[0] === "ls-remote").length,
    2,
  );
});

function fixtureImport() {
  const macBytes = Buffer.from("fixture exact Apple Silicon v0.5.0 bytes\n");
  const linuxBytes = Buffer.from("fixture exact Linux v0.5.0 bytes\n");
  const gzipBytes = gzipSync(linuxBytes, { level: 9, mtime: 0 });
  const profile = __test.createImportProfile({
    mac: { size: macBytes.byteLength, sha256: __test.sha256Bytes(macBytes) },
    linux: { size: linuxBytes.byteLength, sha256: __test.sha256Bytes(linuxBytes) },
    gzip: { size: gzipBytes.byteLength, sha256: __test.sha256Bytes(gzipBytes) },
  });
  const sourceBytes = new Map([
    [NAMES.checksums, profile.checksumBytes],
    [NAMES.mac, macBytes],
    [NAMES.linux, linuxBytes],
  ]);
  const publicBytes = new Map([
    [NAMES.mac, macBytes],
    [NAMES.linux, linuxBytes],
    [NAMES.gzip, gzipBytes],
    [NAMES.provenance, profile.provenanceBytes],
    [NAMES.checksums, profile.checksumBytes],
  ]);
  return { profile, sourceBytes, publicBytes, gzipBytes };
}

async function runFixture(fixture, release, githubFetch, tools) {
  return __test.executeImport(fixture.profile, {
    environment: { CI_PUBLIC_ORIGIN: ORIGIN, CI_RELEASE_TOKEN: TOKEN },
    repository: "/fixture/nanocodex",
    fetch: async (input, init) => {
      const url = new URL(input);
      return url.origin === ORIGIN
        ? release.fetch(url, init)
        : githubFetch(url, init);
    },
    runGit: tools.runGit,
    runGzip: tools.runGzip,
    sleep: async () => {},
  });
}

function githubFetcher(fixture, options = {}) {
  const calls = [];
  return {
    calls,
    async fetch(input, init = {}) {
      const url = new URL(input);
      const headers = new Headers(init.headers);
      calls.push({ url: url.href, method: init.method ?? "GET", headers });
      assert.equal(init.method, "GET");
      assert.equal(headers.get("accept"), "application/octet-stream");
      assert.equal(headers.get("accept-encoding"), "identity");
      assert.equal(headers.has("authorization"), false);
      assert.equal(headers.has("cookie"), false);
      if (url.origin === "https://github.com") {
        assert.equal(init.redirect, "manual");
        const name = decodeURIComponent(url.pathname.split("/").at(-1));
        assert.ok(fixture.sourceBytes.has(name), name);
        return new Response(null, {
          status: 302,
          headers: {
            location:
              `https://release-assets.githubusercontent.com/github-production-release-asset/fixture/${encodeURIComponent(name)}?signature=fixture`,
          },
        });
      }
      assert.equal(url.origin, "https://release-assets.githubusercontent.com");
      assert.equal(init.redirect, "error");
      const name = decodeURIComponent(url.pathname.split("/").at(-1));
      let bytes = fixture.sourceBytes.get(name);
      assert.ok(bytes, name);
      if (options.corrupt === name) {
        bytes = Buffer.from(bytes);
        bytes[0] ^= 1;
      }
      return new Response(bytes, {
        status: 200,
        headers: {
          "content-length": String(bytes.byteLength),
          "content-type": "application/octet-stream",
        },
      });
    },
  };
}

function fakeTools(fixture) {
  const gitCalls = [];
  const gzipCalls = [];
  return {
    gitCalls,
    gzipCalls,
    runGit: async (args, options) => {
      gitCalls.push({ args: [...args], options: { ...options } });
      const command = args.join(" ");
      if (command === "rev-parse --verify refs/tags/v0.5.0") return `${TAG_OBJECT}\n`;
      if (command === `cat-file -t ${TAG_OBJECT}`) return "tag\n";
      if (command === `rev-parse --verify ${TAG_OBJECT}^{}`) return `${COMMIT}\n`;
      if (command === `cat-file -p ${TAG_OBJECT}`) {
        return [
          `object ${COMMIT}`,
          "type commit",
          `tag ${TAG}`,
          "tagger Fixture <fixture@example.invalid> 0 +0000",
          "",
          "fixture annotation",
          "",
        ].join("\n");
      }
      if (args[0] === "ls-remote") {
        assert.deepEqual(args, [
          "ls-remote",
          "--tags",
          "https://github.com/gakonst/nanocodex.git",
          `refs/tags/${TAG}`,
          `refs/tags/${TAG}^{}`,
        ]);
        return `${TAG_OBJECT}\trefs/tags/${TAG}\n${COMMIT}\trefs/tags/${TAG}^{}\n`;
      }
      throw new Error(`unexpected fake git invocation: ${command}`);
    },
    runGzip: async (args, options) => {
      gzipCalls.push({ args: [...args], options: { ...options } });
      assert.deepEqual(args.slice(0, 3), ["-n", "-9", "-c"]);
      assert.equal(args[3].endsWith(`/${NAMES.linux}`), true);
      assert.equal(options.outputPath.endsWith(`/${NAMES.gzip}`), true);
      await writeFile(options.outputPath, fixture.gzipBytes, { flag: "wx", mode: 0o600 });
    },
  };
}

class FakeReleaseServer {
  constructor(fixture, options = {}) {
    this.fixture = fixture;
    this.options = { ...options };
    this.calls = [];
    this.mutations = [];
    this.uploads = new Map();
    this.draft = null;
    this.manifest = null;
    this.pointer = null;
    this.lease = null;
    this.createdAt = "2026-08-22T09:00:00.000Z";
    this.finalizedAt = "2026-08-22T09:01:00.000Z";
  }

  count(method, path) {
    return this.calls.filter((call) => call.method === method && call.path === path).length;
  }

  async fetch(url, init = {}) {
    const method = init.method ?? "GET";
    const path = url.pathname;
    const headers = new Headers(init.headers);
    this.calls.push({ method, path });
    if (method !== "GET" && method !== "HEAD") this.mutations.push({ method, path });
    if (method === "GET" && path !== DRAFT_PATH) {
      assert.equal(headers.has("authorization"), false);
    }

    if (path === NIGHTLY_PATH && method === "GET") return jsonError("release_channel_not_found", 404);
    if (path === IMMUTABLE_PATH && method === "GET") {
      return this.manifest
        ? Response.json(this.manifest, { headers: {
          "cache-control": "public, max-age=31536000, immutable",
          etag: `"${this.manifest.manifestSha256}"`,
        } })
        : jsonError("release_not_found", 404);
    }
    if (path === LATEST_PATH && method === "GET") {
      if (this.options.conflictingLatest) {
        return Response.json({ pointer: { id: "v0.6.0" }, manifest: { id: "v0.6.0" } });
      }
      return this.manifest
        ? Response.json({ pointer: this.pointer, manifest: this.manifest }, { headers: {
          "cache-control": "no-store",
          "content-location": IMMUTABLE_PATH,
        } })
        : jsonError("release_channel_not_found", 404);
    }
    if (path === DRAFT_PATH && method === "GET") {
      this.#assertAuth(headers);
      return this.draft
        ? Response.json({ draft: this.#publicDraft() })
        : jsonError("release_draft_not_found", 404);
    }
    if (path === DRAFT_PATH && method === "PUT") {
      this.#assertAuth(headers);
      assert.equal(headers.get("content-type"), "application/json");
      const bytes = await requestBytes(init.body);
      assert.equal(headers.get("content-length"), String(bytes.byteLength));
      assert.deepEqual(JSON.parse(bytes.toString("utf8")), this.fixture.profile.draft);
      const created = !this.draft;
      if (created) this.draft = this.fixture.profile.draft;
      const response = Response.json({ draft: this.#publicDraft() }, {
        status: created ? 201 : 200,
      });
      if (this.options.loseDraftAcknowledgement) {
        this.options.loseDraftAcknowledgement = false;
        throw new TypeError("fixture lost draft acknowledgement");
      }
      return response;
    }

    const upload = path.match(/^\/api\/releases\/drafts\/stable\/v0\.5\.0\/assets\/([^/]+)$/);
    if (upload && method === "PUT") {
      this.#assertAuth(headers);
      assert.ok(this.draft);
      const name = decodeURIComponent(upload[1]);
      const asset = this.fixture.profile.assets.find((candidate) => candidate.name === name);
      assert.ok(asset, name);
      const bytes = await requestBytes(init.body);
      assert.equal(headers.get("content-length"), String(asset.size));
      assert.equal(headers.get("content-type"), asset.contentType);
      assert.equal(headers.get("x-nanocodex-sha256"), asset.sha256);
      assert.deepEqual(bytes, this.fixture.publicBytes.get(name));
      const uploaded = !this.uploads.has(name);
      this.uploads.set(name, bytes);
      if (this.options.loseUploadAcknowledgement === name) {
        this.options.loseUploadAcknowledgement = null;
        throw new TypeError("fixture lost upload acknowledgement");
      }
      return Response.json({
        asset: publicAsset(asset, `${DRAFT_PATH}/assets`),
        uploaded,
      }, { status: uploaded ? 201 : 200 });
    }

    if (path === `${RELEASE_ROOT}/publication-lease/acquire` && method === "POST") {
      this.#assertAuth(headers);
      assert.deepEqual(JSON.parse((await requestBytes(init.body)).toString("utf8")), {
        owner: `legacy-import:${TAG}`,
        kind: "stable",
        id: TAG,
        commit: COMMIT,
      });
      const created = !this.lease;
      this.lease ??= {
        version: 1,
        leaseId: "1.00000000-0000-4000-8000-000000000001",
        owner: `legacy-import:${TAG}`,
        kind: "stable",
        id: TAG,
        commit: COMMIT,
        generation: 1,
        expiresAt: "2099-01-01T00:00:00.000Z",
      };
      const response = Response.json(this.lease, { status: created ? 201 : 200 });
      if (this.options.loseLeaseAcknowledgement) {
        this.options.loseLeaseAcknowledgement = false;
        throw new TypeError("fixture lost lease acknowledgement");
      }
      return response;
    }

    const heartbeat = path.match(/^\/api\/releases\/publication-lease\/([^/]+)\/heartbeat$/);
    if (heartbeat && method === "POST") {
      this.#assertAuth(headers);
      assert.equal(decodeURIComponent(heartbeat[1]), this.lease?.leaseId);
      assert.deepEqual(JSON.parse((await requestBytes(init.body)).toString("utf8")), {
        owner: `legacy-import:${TAG}`,
      });
      return Response.json(this.lease);
    }

    if (path === `${DRAFT_PATH}/finalize` && method === "POST") {
      this.#assertAuth(headers);
      assert.equal(headers.get("x-nanocodex-publication-lease-id"), this.lease?.leaseId);
      assert.equal(headers.get("x-nanocodex-publication-lease-owner"), this.lease?.owner);
      assert.equal(headers.get("x-nanocodex-publication-lease-generation"), "1");
      for (const asset of this.fixture.profile.assets) assert.ok(this.uploads.has(asset.name));
      const created = !this.manifest;
      if (created) this.#publish();
      const response = Response.json({ manifest: this.manifest, pointer: this.pointer }, {
        status: created ? 201 : 200,
      });
      if (this.options.loseFinalizeAcknowledgement) {
        this.options.loseFinalizeAcknowledgement = false;
        throw new TypeError("fixture lost finalize acknowledgement");
      }
      return response;
    }

    const releaseLease = path.match(/^\/api\/releases\/publication-lease\/([^/]+)$/);
    if (releaseLease && method === "DELETE") {
      this.#assertAuth(headers);
      assert.equal(decodeURIComponent(releaseLease[1]), this.lease?.leaseId);
      assert.deepEqual(JSON.parse((await requestBytes(init.body)).toString("utf8")), {
        owner: `legacy-import:${TAG}`,
      });
      this.lease = null;
      return new Response(null, { status: 204 });
    }

    const immutableAsset = path.match(/^\/api\/releases\/releases\/stable\/v0\.5\.0\/assets\/([^/]+)$/);
    if (immutableAsset && method === "GET") {
      return this.#assetResponse(decodeURIComponent(immutableAsset[1]), null);
    }
    const rollingAsset = path.match(/^\/api\/releases\/channels\/latest\/assets\/([^/]+)$/);
    if (rollingAsset && method === "GET") {
      const name = decodeURIComponent(rollingAsset[1]);
      return this.#assetResponse(
        name,
        `${IMMUTABLE_PATH}/assets/${encodeURIComponent(name)}`,
      );
    }
    throw new Error(`unexpected fake release request: ${method} ${path}`);
  }

  #assertAuth(headers) {
    assert.equal(headers.get("authorization"), `Bearer ${TOKEN}`);
  }

  #publicDraft() {
    return {
      version: 1,
      kind: "stable",
      id: TAG,
      tag: TAG,
      commit: COMMIT,
      channel: "latest",
      expectedChannel: null,
      createdAt: this.createdAt,
      assets: this.fixture.profile.assets.map((asset) =>
        publicAsset(asset, `${DRAFT_PATH}/assets`)
      ),
    };
  }

  #publish() {
    const unsigned = {
      version: 1,
      kind: "stable",
      id: TAG,
      tag: TAG,
      commit: COMMIT,
      channel: "latest",
      finalizedAt: this.finalizedAt,
      assets: this.fixture.profile.assets.map((asset) =>
        publicAsset(asset, `${IMMUTABLE_PATH}/assets`)
      ),
    };
    this.manifest = {
      ...unsigned,
      manifestSha256: __test.sha256Bytes(Buffer.from(__test.canonicalJson(unsigned))),
    };
    this.pointer = {
      version: 1,
      channel: "latest",
      kind: "stable",
      id: TAG,
      tag: TAG,
      commit: COMMIT,
      generation: 1,
      updatedAt: this.finalizedAt,
    };
    this.draft = null;
  }

  #assetResponse(name, contentLocation) {
    assert.ok(this.manifest);
    const asset = this.fixture.profile.assets.find((candidate) => candidate.name === name);
    assert.ok(asset, name);
    const headers = new Headers({
      "cache-control": contentLocation ? "no-store" : "public, max-age=31536000, immutable",
      "content-disposition": `attachment; filename="${asset.name}"`,
      "content-length": String(asset.size),
      "content-type": asset.contentType,
      etag: `"${asset.sha256}"`,
      "x-content-type-options": "nosniff",
      "x-nanocodex-release": TAG,
      "x-nanocodex-sha256": asset.sha256,
    });
    if (contentLocation) headers.set("content-location", contentLocation);
    return new Response(this.uploads.get(name), { status: 200, headers });
  }
}

function publicAsset(asset, prefix) {
  return {
    name: asset.name,
    platform: asset.platform,
    size: asset.size,
    sha256: asset.sha256,
    contentType: asset.contentType,
    downloadPath: `${prefix}/${encodeURIComponent(asset.name)}`,
  };
}

function jsonError(error, status) {
  return Response.json({ error }, { status });
}

async function requestBytes(body) {
  if (body == null) return Buffer.alloc(0);
  if (typeof body === "string") return Buffer.from(body);
  if (ArrayBuffer.isView(body)) {
    return Buffer.from(body.buffer, body.byteOffset, body.byteLength);
  }
  if (body instanceof ArrayBuffer) return Buffer.from(body);
  const chunks = [];
  for await (const chunk of body) chunks.push(Buffer.from(chunk));
  return Buffer.concat(chunks);
}
