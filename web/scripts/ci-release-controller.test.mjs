import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, rm, stat, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { gzipSync } from "node:zlib";
import test from "node:test";

import {
  PublicationLeaseLostError,
  ReleaseHttpError,
  ReleaseValidationError,
  ReviewedReleasePlanMismatchError,
  TagMovedError,
  assertReviewedReleasePlan,
  assertReleaseCheckoutState,
  assertTagBinding,
  boundedStructuredError,
  cargoPublicationEnvironment,
  cargoPublishArguments,
  cargoRepackagePath,
  createReviewedReleasePlan,
  inspectLocalCratePackage,
  npmPublicationEnvironment,
  parseAcceptedStableRelease,
  parseDraftStableDescriptor,
  parseGreenMasterEvidence,
  parseCrateRegistryVersion,
  parseLatestStable,
  parseNpmRegistryVersion,
  parsePublicationLease,
  parsePublicStableManifest,
  parseReadyDistribution,
  parseReleaseArguments,
  parseRemoteTagRefs,
  parseRetryAfter,
  parseReviewedReleasePlan,
  parseRunningStableRelease,
  parseStableTag,
  publicationLeaseFinalizeHeaders,
  publishCratePackages,
  persistReviewedReleasePlan,
  readReviewedReleasePlan,
  reconcileCrateRegistryVersions,
  registryCredentialRequirements,
  releaseCrateNames,
  releaseProvenanceNotice,
  releaseRequestHeaders,
  retryReleaseOperation,
  runReleaseSequence,
  runWithPublicationLease,
  selectTestedNpmArtifact,
  validateNpmArtifactHeaders,
  validateLocalCratePackages,
  verifyCargoRepackage,
  verifyNpmPackageBytes,
  verifyNpmRegistryTarballResponse,
} from "./ci-release-controller.mjs";

const tag = "v0.5.0";
const version = "0.5.0";
const head = "1".repeat(40);
const otherHead = "2".repeat(40);
const tagObject = "3".repeat(40);
const workflowId = `ci-${head}`;

test("CLI and remote refs bind canonical lightweight and annotated stable tags", () => {
  assert.deepEqual(parseReleaseArguments(["stage", tag]), {
    help: false,
    command: "stage",
    tag,
    version,
    major: 0,
    minor: 5,
    patch: 0,
  });
  assert.deepEqual(parseReleaseArguments(["--help"]), { help: true });
  assert.equal(parseStableTag("v12.34.56").version, "12.34.56");
  for (const invalid of ["0.5.0", "v00.5.0", "v0.05.0", "v0.5", "v0.5.0-rc.1"]) {
    assert.throws(() => parseStableTag(invalid), /canonical/);
  }
  assert.throws(() => parseReleaseArguments(["release", tag]), /usage/);
  assert.throws(() => parseReleaseArguments(["publish", tag, "extra"]), /usage/);

  const lightweight = parseRemoteTagRefs(`${head}\trefs/tags/${tag}\n`, tag);
  assert.deepEqual(lightweight, {
    tag,
    ref: `refs/tags/${tag}`,
    object: head,
    commit: head,
    annotated: false,
  });
  const annotated = parseRemoteTagRefs(
    `${tagObject}\trefs/tags/${tag}\n${head}\trefs/tags/${tag}^{}\n`,
    tag,
  );
  assert.equal(annotated.annotated, true);
  assert.equal(annotated.object, tagObject);
  assert.equal(annotated.commit, head);
  assert.equal(assertTagBinding(annotated, { ...annotated }, "before publish"), annotated);
  assert.throws(
    () => assertTagBinding(annotated, { ...annotated, object: otherHead }, "before publish"),
    (error) => error instanceof TagMovedError && error.phase === "before publish",
  );
  assert.throws(
    () => parseRemoteTagRefs(`${head}\trefs/heads/master\n`, tag),
    /unexpected refs/,
  );
});

test("release checkout must be detached, clean, and resolve the exact remote tag object and commit", () => {
  const binding = parseRemoteTagRefs(
    `${tagObject}\trefs/tags/${tag}\n${head}\trefs/tags/${tag}^{}\n`,
    tag,
  );
  assert.equal(assertReleaseCheckoutState({
    ref: "HEAD",
    head,
    tagObject,
    tagCommit: head,
    status: "",
  }, binding), head);
  assert.throws(() => assertReleaseCheckoutState({
    ref: "refs/heads/master",
    head,
    tagObject,
    tagCommit: head,
    status: "",
  }, binding), /detached checkout/);
  assert.throws(() => assertReleaseCheckoutState({
    ref: "HEAD",
    head,
    tagObject,
    tagCommit: head,
    status: "?? unexpected\0",
  }, binding), /clean tracked and untracked/);
  assert.throws(() => assertReleaseCheckoutState({
    ref: "HEAD",
    head,
    tagObject: otherHead,
    tagCommit: head,
    status: "",
  }, binding), /exact authoritative/);
});

test("green release evidence must be retained on the exact master lane, never only a PR", () => {
  const master = masterState();
  const run = greenRunValue([]);
  const parsed = parseGreenMasterEvidence(master, run, head);
  assert.equal(parsed.master.publication.lane.type, "master");
  assert.equal(parsed.run.outcome, "success");

  assert.throws(() => parseGreenMasterEvidence({
    ...master,
    publication: {
      ...master.publication,
      branch: "pull/7/merge",
      ref: "refs/pull/7/merge",
      lane: { type: "pull_request", number: 7, pullRequestHead: otherHead },
    },
  }, run, head), /not the exact master lane/);
  assert.throws(() => parseGreenMasterEvidence(master, {
    ...run,
    workflow: { status: "errored" },
    result: { ...run.result, status: "failure" },
  }, head), /does not have a retained green/);
  assert.throws(() => parseGreenMasterEvidence({
    ...master,
    run: { ...master.run, state: "pending" },
  }, run, head), /do not agree/);
});

test("Cargo, npm, and Worker authority remain isolated from ambient secrets and each other", () => {
  const env = {
    PATH: "/safe/bin",
    HOME: "/safe/home",
    CARGO_HOME: "/ambient/cargo-home",
    CARGO_REGISTRY_TOKEN: "cargo-secret",
    NPM_TOKEN: "npm-secret",
    CI_RELEASE_TOKEN: "release-secret",
    NANOCODEX_CI_TOKEN: "ci-secret",
    CLOUDFLARE_API_TOKEN: "cloudflare-secret",
    GITHUB_TOKEN: "github-secret",
    OPENAI_API_KEY: "openai-secret",
  };
  const cargo = cargoPublicationEnvironment(env, {
    cargoHome: "/tmp/release-cargo-home",
  });
  const npm = npmPublicationEnvironment(env, {
    userConfig: "/tmp/release-npmrc",
    cache: "/tmp/release-npm-cache",
  });
  const worker = releaseRequestHeaders(env.CI_RELEASE_TOKEN, { json: true });

  assert.deepEqual(authorityKeys(cargo), ["CARGO_REGISTRY_TOKEN"]);
  assert.equal(cargo.CARGO_HOME, "/tmp/release-cargo-home");
  assert.deepEqual(
    cargoPublishArguments(releaseCrateNames[0], "/tmp/release-cargo-target"),
    [
      "publish",
      "--locked",
      "--no-verify",
      "--config",
      ".cargo/release.toml",
      "--registry",
      "crates-io",
      "--package",
      releaseCrateNames[0],
      "--target-dir",
      "/tmp/release-cargo-target",
    ],
  );
  assert.deepEqual(
    cargoPublishArguments(
      releaseCrateNames[0],
      "/tmp/release-cargo-target",
      { dryRun: true },
    ),
    [
      "publish",
      "--dry-run",
      "--locked",
      "--no-verify",
      "--config",
      ".cargo/release.toml",
      "--registry",
      "crates-io",
      "--package",
      releaseCrateNames[0],
      "--target-dir",
      "/tmp/release-cargo-target",
    ],
  );
  assert.deepEqual(authorityKeys(npm), ["NODE_AUTH_TOKEN"]);
  assert.equal(npm.NODE_AUTH_TOKEN, env.NPM_TOKEN);
  assert.equal(cargo.NPM_TOKEN, undefined);
  assert.equal(cargo.NODE_AUTH_TOKEN, undefined);
  assert.equal(cargo.CI_RELEASE_TOKEN, undefined);
  assert.equal(npm.CARGO_REGISTRY_TOKEN, undefined);
  assert.equal(npm.CI_RELEASE_TOKEN, undefined);
  assert.equal(worker.get("authorization"), "Bearer release-secret");
  assert.equal([...worker.values()].join(" ").includes("cargo-secret"), false);
  assert.equal([...worker.values()].join(" ").includes("npm-secret"), false);
  assert.equal(releaseProvenanceNotice.npmRegistryProvenance.status, "unavailable");
  assert.match(releaseProvenanceNotice.r2Provenance.meaning, /not an npm registry attestation/);

  assert.throws(
    () => cargoPublicationEnvironment(env),
    /isolated Cargo home/,
  );
  assert.throws(() => npmPublicationEnvironment({
    ...env,
    NODE_AUTH_TOKEN: "another-npm-secret",
  }, { userConfig: "/tmp/npmrc" }), /disagree/);
});

test("local Cargo packages are regular bounded non-symlinks with recorded SHA-256", async (t) => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-crate-test-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const crate = releaseCrateNames[0];
  const bytes = Buffer.from("locally packaged crate bytes");
  const packageDirectory = resolve(directory, "target", "package");
  await mkdir(packageDirectory, { recursive: true, mode: 0o700 });
  const path = resolve(packageDirectory, `${crate}-${version}.crate`);
  await writeFile(path, bytes, { mode: 0o600 });

  const localPackage = await inspectLocalCratePackage(path, crate, version);
  assert.deepEqual(localPackage, {
    crate,
    version,
    path,
    size: bytes.byteLength,
    sha256: createHash("sha256").update(bytes).digest("hex"),
  });
  assert.equal(
    validateLocalCratePackages(
      localCratePackages().map((entry, index) => index === 0 ? localPackage : entry),
      version,
    )[0].sha256,
    localPackage.sha256,
  );

  await assert.rejects(
    inspectLocalCratePackage(path, crate, version, { maximumBytes: bytes.byteLength - 1 }),
    /size is invalid/,
  );
  const repackagedPath = cargoRepackagePath(localPackage);
  await mkdir(resolve(packageDirectory, "tmp-crate"), { mode: 0o700 });
  await writeFile(repackagedPath, bytes, { mode: 0o600 });
  assert.equal(
    (await verifyCargoRepackage(localPackage, "test dry-run")).sha256,
    localPackage.sha256,
  );
  await writeFile(repackagedPath, Buffer.from("different Cargo repackage bytes"));
  await assert.rejects(
    verifyCargoRepackage(localPackage, "test dry-run"),
    /Cargo test dry-run bytes changed/,
  );
  const linkedCrate = releaseCrateNames[1];
  const linkedPath = resolve(packageDirectory, `${linkedCrate}-${version}.crate`);
  await symlink(path, linkedPath);
  await assert.rejects(
    inspectLocalCratePackage(linkedPath, linkedCrate, version),
    /regular non-symlink/,
  );
});

test("crate publication matches local hashes and skips a fully existing release", async () => {
  const packages = localCratePackages();
  const parsed = parseCrateRegistryVersion({
    version: {
      crate: packages[0].crate,
      num: version,
      yanked: false,
      checksum: packages[0].sha256,
      dl_path: `/api/v1/crates/${packages[0].crate}/${version}/download`,
    },
  }, packages[0].crate, version);
  assert.deepEqual(parsed, crateRegistryRecord(packages[0]));
  assert.throws(
    () => parseCrateRegistryVersion({
      version: {
        crate: packages[0].crate,
        num: version,
        yanked: false,
        checksum: packages[0].sha256,
        dl_path: `/api/v1/crates/${packages[1].crate}/${version}/download`,
      },
    }, packages[0].crate, version),
    /untrusted download path/,
  );
  const records = new Map(packages.map((localPackage) => [
    localPackage.crate,
    localPackage.crate === parsed.crate ? parsed : crateRegistryRecord(localPackage),
  ]));
  assert.equal(
    reconcileCrateRegistryVersions(packages, [...records.values()], { requireAll: true }).length,
    releaseCrateNames.length,
  );
  const mismatchedRecords = [...records.values()];
  mismatchedRecords[4] = {
    ...mismatchedRecords[4],
    checksum: "e".repeat(64),
  };
  assert.throws(
    () => reconcileCrateRegistryVersions(packages, mismatchedRecords),
    new RegExp(`API checksum mismatch for ${packages[4].crate}`),
  );
  const published = [];
  const result = await publishCratePackages(packages, {
    async readVersion(localPackage) {
      return records.get(localPackage.crate);
    },
    async publishVersion(localPackage) {
      published.push(localPackage.crate);
    },
    async waitVersion(localPackage) {
      return records.get(localPackage.crate);
    },
  });
  assert.equal(result.action, "already-published");
  assert.deepEqual(published, []);
  assert.deepEqual(
    result.crates.map(({ crate, action }) => ({ crate, action })),
    releaseCrateNames.map((crate) => ({ crate, action: "already-published" })),
  );
});

test("partial crates.io state publishes only missing crates in dependency order", async () => {
  const packages = localCratePackages();
  const initiallyExisting = new Set([
    releaseCrateNames[0],
    releaseCrateNames[3],
    releaseCrateNames[6],
  ]);
  const records = new Map(packages.flatMap((localPackage) =>
    initiallyExisting.has(localPackage.crate)
      ? [[localPackage.crate, crateRegistryRecord(localPackage)]]
      : []
  ));
  const published = [];
  const result = await publishCratePackages(packages, {
    async readVersion(localPackage) {
      return records.get(localPackage.crate);
    },
    async publishVersion(localPackage) {
      published.push(localPackage.crate);
    },
    async waitVersion(localPackage) {
      const record = crateRegistryRecord(localPackage);
      records.set(localPackage.crate, record);
      return record;
    },
  });
  assert.equal(result.action, "published");
  assert.deepEqual(
    published,
    releaseCrateNames.filter((crate) => !initiallyExisting.has(crate)),
  );
  assert.deepEqual(
    result.crates.filter(({ action }) => action === "already-published").map(({ crate }) => crate),
    releaseCrateNames.filter((crate) => initiallyExisting.has(crate)),
  );
});

test("a local/registry checksum mismatch prevents every later crate publication", async () => {
  const packages = localCratePackages();
  const records = new Map([[packages[0].crate, crateRegistryRecord(packages[0])]]);
  const published = [];
  await assert.rejects(
    publishCratePackages(packages, {
      async readVersion(localPackage) {
        return records.get(localPackage.crate);
      },
      async publishVersion(localPackage) {
        published.push(localPackage.crate);
      },
      async waitVersion(localPackage) {
        const record = crateRegistryRecord(localPackage, {
          checksum: localPackage.crate === packages[2].crate
            ? "f".repeat(64)
            : localPackage.sha256,
        });
        records.set(localPackage.crate, record);
        return record;
      },
    }),
    new RegExp(`API checksum mismatch for ${packages[2].crate}`),
  );
  assert.deepEqual(published, [packages[1].crate, packages[2].crate]);
  assert.equal(published.includes(packages[3].crate), false);
});

test("ready draft selects and verifies the exact tested npm tgz and all eight release identities", () => {
  const tgz = npmTarball();
  const tested = npmArtifact(tgz);
  const run = {
    head,
    outcome: "success",
    result: {
      artifacts: [
        { key: `runs/${head}/artifacts/web-dist.tar`, size: 10, sha256: "9".repeat(64), contentType: "application/x-tar" },
        tested,
      ],
    },
  };
  const selected = selectTestedNpmArtifact(run, tag);
  assert.equal(selected.name, `nanocodex-${version}.tgz`);
  assert.equal(selected.sha256, tested.sha256);
  const headers = npmHeaders(selected);
  assert.equal(validateNpmArtifactHeaders(headers, selected, head), selected);
  const metadata = verifyNpmPackageBytes(tgz, selected, tag);
  assert.equal(metadata.name, "nanocodex");
  assert.equal(metadata.version, version);
  assert.match(metadata.integrity, /^sha512-/);

  const ready = parseReadyDistribution(
    readyEvidence(selected),
    tag,
    head,
    selected,
  );
  assert.equal(ready.draft.assets.length, 8);
  assert.deepEqual(
    ready.draft.assets.map(({ name }) => name),
    [...ready.draft.assets.map(({ name }) => name)].sort((a, b) => a.localeCompare(b)),
  );
  assert.equal(ready.npm.sha256, selected.sha256);
  assert.equal(
    ready.npm.key,
    `distribution/stable/${tag}/components/npm/nanocodex-${version}.tgz`,
  );
  assert.notEqual(ready.npm.key, selected.key);
  assert.equal(
    ready.draft.assets.find((asset) => asset.platform === "aarch64-apple-darwin").key,
    `distribution/stable/${tag}/components/macos/nanocodex-aarch64-apple-darwin`,
  );

  const wrongRun = structuredClone(run);
  wrongRun.result.artifacts.push({ ...tested });
  assert.throws(() => selectTestedNpmArtifact(wrongRun, tag), /exactly one/);
  assert.throws(
    () => verifyNpmPackageBytes(Buffer.concat([tgz, Buffer.from("x")]), selected, tag),
    /exactly/,
  );
  const provenanceTgz = npmTarball("export {};\n", true);
  assert.throws(
    () => verifyNpmPackageBytes(provenanceTgz, npmArtifact(provenanceTgz), tag),
    /not publishable/,
  );
  const wrongReady = readyEvidence(selected);
  wrongReady.staged.draft.assets.find((asset) => asset.platform === "npm").sha256 = "f".repeat(64);
  assert.throws(
    () => parseReadyDistribution(wrongReady, tag, head, selected),
    /exact artifact/,
  );
  const transientReady = readyEvidence(selected);
  transientReady.staged.draft.assets.find((asset) => asset.platform === "npm").key = selected.key;
  assert.throws(
    () => parseReadyDistribution(transientReady, tag, head, selected),
    /wrong identity/,
  );
  assert.throws(
    () => parseReadyDistribution(
      readyEvidence(selected),
      tag,
      head,
      { ...selected, key: `runs/${"f".repeat(40)}/artifacts/npm-package.tgz` },
    ),
    /exact artifact/,
  );
});

test("accepted and restarted stable attempts preserve exact request identity without requiring it on normal evidence", () => {
  const requestId = "123e4567-e89b-42d3-a456-426614174000";
  const accepted = parseAcceptedStableRelease({
    status: "accepted",
    channel: "stable",
    tagName: tag,
    head,
    workflowId: `release-${tag}-${head}`,
    requestId,
  }, tag, head);
  assert.equal(accepted.requestId, requestId);

  const running = {
    version: 1,
    status: "running",
    channel: "stable",
    tagName: tag,
    head,
    workflowId: `release-${tag}-${head}`,
  };
  assert.equal(parseRunningStableRelease(running, tag, head, requestId).requestId, undefined);
  assert.equal(
    parseRunningStableRelease({ ...running, requestId }, tag, head, requestId).requestId,
    requestId,
  );
  assert.throws(
    () => parseAcceptedStableRelease({ ...accepted, requestId: undefined }, tag, head),
    /attempt identity/,
  );
  assert.throws(
    () => parseRunningStableRelease({
      ...running,
      requestId: "223e4567-e89b-42d3-a456-426614174000",
    }, tag, head, requestId),
    /attempt identity/,
  );
});

test("public latest and immutable manifests reproduce the reviewed asset identities and canonical hash", () => {
  const tgz = npmTarball();
  const selected = {
    ...npmArtifact(tgz),
    name: `nanocodex-${version}.tgz`,
    platform: "npm",
  };
  const ready = parseReadyDistribution(readyEvidence(selected), tag, head, selected);
  const unsigned = {
    version: 1,
    kind: "stable",
    id: tag,
    tag,
    commit: head,
    channel: "latest",
    finalizedAt: "2026-08-22T12:00:00.000Z",
    assets: ready.draft.assets.map(({ key: _key, ...asset }) => ({
      ...asset,
      downloadPath: `/api/releases/releases/stable/${tag}/assets/${encodeURIComponent(asset.name)}`,
    })),
  };
  const manifest = {
    ...unsigned,
    manifestSha256: createHash("sha256").update(canonicalJson(unsigned)).digest("hex"),
  };
  const parsed = parsePublicStableManifest(manifest, ready);
  const latest = parseLatestStable({
    pointer: {
      version: 1,
      channel: "latest",
      kind: "stable",
      id: tag,
      tag,
      commit: head,
      generation: 4,
      updatedAt: "2026-08-22T12:00:00.000Z",
    },
    manifest,
  }, parsed);
  assert.equal(latest.pointer.generation, 4);
  assert.throws(
    () => parsePublicStableManifest({ ...manifest, manifestSha256: "0".repeat(64) }, ready),
    /canonical SHA-256/,
  );
});

test("stage stops after review while publish orders registries before finalization", async () => {
  const stageCalls = [];
  const stageResult = await runReleaseSequence("stage", orderedOperations(stageCalls));
  assert.equal(stageResult.action, "staged");
  assert.deepEqual(stageCalls, [
    "trust:initial",
    "package-crates",
    "verify-existing-crates",
    "stage",
    "review-plan",
    "trust:after staging",
    "ready:after staging",
    "persist-plan",
  ]);
  assert.equal(stageCalls.includes("publish-crates"), false);
  assert.equal(stageCalls.includes("publish-npm"), false);
  assert.equal(stageCalls.includes("finalize"), false);

  const publishCalls = [];
  const publishResult = await runReleaseSequence("publish", orderedOperations(publishCalls));
  assert.equal(publishResult.action, "published");
  assert.deepEqual(publishCalls, [
    "trust:initial",
    "package-crates",
    "verify-existing-crates",
    "read-ready",
    "read-plan",
    "read-published",
    "review-plan",
    "inspect-npm",
    "preflight-credentials",
    "lease:acquire",
    "read-published",
    "trust:before plan reproduction",
    "ready:before plan reproduction",
    "review-plan",
    "verify-existing-crates",
    "inspect-npm",
    "preflight-credentials",
    "lease:heartbeat",
    "trust:before crate publication",
    "ready:before crate publication",
    "publish-crates",
    "verify-crates",
    "lease:heartbeat",
    "trust:before npm publication",
    "ready:before npm publication",
    "publish-npm",
    "verify-npm",
    "lease:heartbeat",
    "trust:before finalization",
    "ready:before finalization",
    "finalize",
    "verify-public",
    "trust:after public verification",
    "lease:heartbeat",
    "lease:release",
  ]);
  assert.ok(publishCalls.indexOf("verify-crates") < publishCalls.indexOf("finalize"));
  assert.ok(publishCalls.indexOf("verify-npm") < publishCalls.indexOf("finalize"));
});

test("publish is idempotent and never finalizes after registry verification failure", async () => {
  const replayCalls = [];
  const replay = await runReleaseSequence(
    "publish",
    orderedOperations(replayCalls, { existing: true }),
  );
  assert.equal(replay.action, "already-published");
  assert.deepEqual(replayCalls, [
    "trust:initial",
    "package-crates",
    "verify-existing-crates",
    "read-ready",
    "read-plan",
    "read-published",
    "review-public-plan",
    "verify-crates",
    "verify-npm",
    "verify-public",
    "trust:after replay verification",
    "ready:after replay verification",
  ]);
  assert.equal(replayCalls.includes("publish-crates"), false);
  assert.equal(replayCalls.includes("publish-npm"), false);
  assert.equal(replayCalls.includes("finalize"), false);

  const movedReplayCalls = [];
  await assert.rejects(
    runReleaseSequence(
      "publish",
      orderedOperations(movedReplayCalls, { existing: true, replayTrustFailure: true }),
    ),
    (error) => error instanceof TagMovedError &&
      error.phase === "after replay verification",
  );
  assert.equal(movedReplayCalls.includes("trust:after replay verification"), true);
  assert.equal(movedReplayCalls.includes("publish-crates"), false);
  assert.equal(movedReplayCalls.includes("publish-npm"), false);
  assert.equal(movedReplayCalls.includes("finalize"), false);

  const failureCalls = [];
  await assert.rejects(
    runReleaseSequence(
      "publish",
      orderedOperations(failureCalls, { npmRegistryFailure: true }),
    ),
    /npm registry unavailable/,
  );
  assert.equal(failureCalls.includes("publish-crates"), true);
  assert.equal(failureCalls.includes("publish-npm"), true);
  assert.equal(failureCalls.includes("finalize"), false);
  assert.equal(failureCalls.includes("verify-public"), false);
});

test("repackage or checksum failure blocks staging and all later finalization", async () => {
  const repackageCalls = [];
  await assert.rejects(
    runReleaseSequence(
      "stage",
      orderedOperations(repackageCalls, { repackageFailure: true }),
    ),
    /simulated Cargo repackage failure/,
  );
  assert.deepEqual(repackageCalls, ["trust:initial", "package-crates"]);
  assert.equal(repackageCalls.includes("stage"), false);

  const stageMismatchCalls = [];
  await assert.rejects(
    runReleaseSequence(
      "stage",
      orderedOperations(stageMismatchCalls, { existingCrateMismatch: true }),
    ),
    /simulated existing crate checksum mismatch/,
  );
  assert.deepEqual(stageMismatchCalls, [
    "trust:initial",
    "package-crates",
    "verify-existing-crates",
  ]);
  assert.equal(stageMismatchCalls.includes("stage"), false);

  const publishMismatchCalls = [];
  await assert.rejects(
    runReleaseSequence(
      "publish",
      orderedOperations(publishMismatchCalls, { publishedCrateMismatch: true }),
    ),
    /simulated newly published crate checksum mismatch/,
  );
  assert.equal(publishMismatchCalls.includes("publish-crates"), true);
  assert.equal(publishMismatchCalls.includes("verify-crates"), false);
  assert.equal(publishMismatchCalls.includes("publish-npm"), false);
  assert.equal(publishMismatchCalls.includes("finalize"), false);
  assert.equal(publishMismatchCalls.includes("verify-public"), false);
});

test("a pre-finalization retry reconciles already-published crates before finalizing", async () => {
  const calls = [];
  const state = { cratesPublished: false, failNpmVerification: true };
  const operations = retryOperations(calls, state);
  await assert.rejects(
    runReleaseSequence("publish", operations),
    /simulated lost npm acknowledgement/,
  );
  assert.equal(state.cratesPublished, true);
  assert.equal(calls.filter((call) => call === "finalize").length, 0);

  const result = await runReleaseSequence("publish", operations);
  assert.equal(result.action, "published");
  assert.equal(calls.includes("publish-crates:published"), true);
  assert.equal(calls.includes("publish-crates:already-published"), true);
  assert.equal(calls.filter((call) => call === "finalize").length, 1);
});

test("reviewed plan immutably binds toolchains, all crate bytes, and exact draft assets", async (t) => {
  const fixture = reviewedFixture();
  const parsed = parseReviewedReleasePlan(structuredClone(fixture.plan));
  assert.equal(parsed.planSha256, fixture.plan.planSha256);
  assert.equal(parsed.crates.length, 8);
  assert.equal(parsed.assets.length, 8);
  assert.equal(parsed.draftSha256, createHash("sha256").update(canonicalJson(parsed.draft)).digest("hex"));
  assert.equal(parsed.releaseObjects.prefix, `distribution/stable/${tag}/`);
  assert.equal(parsed.releaseObjects.assets.length, 8);
  assert.equal(
    parsed.releaseObjects.assets.every(({ key }) =>
      key.startsWith(parsed.releaseObjects.prefix) &&
      !key.startsWith(`runs/${head}/`) && !key.startsWith("macos/jobs/")
    ),
    true,
  );
  assert.equal(
    parseDraftStableDescriptor({ draft: structuredClone(parsed.draft) }, fixture.ready)
      .assets.length,
    8,
  );

  const changedToolchain = reviewedFixture({
    toolchain: { cargo: "cargo 1.90.0 (changed after stage)" },
  }).plan;
  assert.throws(
    () => assertReviewedReleasePlan(fixture.plan, changedToolchain, "toolchain replay"),
    (error) => error instanceof ReviewedReleasePlanMismatchError,
  );
  const changedCrates = localCratePackages();
  changedCrates[3] = { ...changedCrates[3], sha256: "f".repeat(64) };
  const changedBytes = reviewedFixture({ crates: changedCrates }).plan;
  assert.throws(
    () => assertReviewedReleasePlan(fixture.plan, changedBytes, "crate replay"),
    (error) => error instanceof ReviewedReleasePlanMismatchError,
  );

  const crossReleaseObjects = structuredClone(fixture.plan);
  crossReleaseObjects.releaseObjects.assets[0].key =
    `distribution/stable/v9.9.9/${crossReleaseObjects.releaseObjects.assets[0].name}`;
  const { planSha256: _planSha256, ...crossReleasePayload } = crossReleaseObjects;
  crossReleaseObjects.planSha256 = createHash("sha256")
    .update(canonicalJson(crossReleasePayload))
    .digest("hex");
  assert.throws(
    () => parseReviewedReleasePlan(crossReleaseObjects),
    /asset or provenance identity/,
  );

  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-reviewed-plan-test-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const path = resolve(directory, `${tag}.json`);
  const created = await persistReviewedReleasePlan(path, fixture.plan);
  assert.equal(created.action, "created");
  assert.equal((await stat(path)).mode & 0o077, 0);
  assert.equal((await readReviewedReleasePlan(path)).planSha256, fixture.plan.planSha256);
  assert.equal((await persistReviewedReleasePlan(path, fixture.plan)).action, "replayed");
  await assert.rejects(
    persistReviewedReleasePlan(path, changedToolchain),
    (error) => error instanceof ReviewedReleasePlanMismatchError,
  );
});

test("npm registry verification rejects metadata with a tampered canonical tarball body", async () => {
  const expectedBytes = npmTarball("export const value = 'a';\n");
  const tamperedBytes = Buffer.from(expectedBytes);
  tamperedBytes[Math.floor(tamperedBytes.byteLength / 2)] ^= 1;
  assert.equal(tamperedBytes.byteLength, expectedBytes.byteLength);
  const artifact = {
    ...npmArtifact(expectedBytes),
    name: `nanocodex-${version}.tgz`,
    platform: "npm",
  };
  const expected = verifyNpmPackageBytes(expectedBytes, artifact, tag);
  const record = parseCrateSafeNpmRecord(expected);
  const verified = await verifyNpmRegistryTarballResponse(
    new Response(expectedBytes, {
      headers: { "content-length": String(expectedBytes.byteLength) },
    }),
    record,
    expected,
  );
  assert.equal(verified.sha256, expected.sha256);
  let attempts = 0;
  await assert.rejects(
    retryReleaseOperation(
      "verify immutable npm tarball",
      async () => {
        attempts += 1;
        return verifyNpmRegistryTarballResponse(
          new Response(tamperedBytes, {
            headers: { "content-length": String(tamperedBytes.byteLength) },
          }),
          record,
          expected,
        );
      },
      { maximumAttempts: 5, delay: async () => undefined },
    ),
    (error) => error instanceof ReleaseValidationError,
  );
  assert.equal(attempts, 1);
});

test("idempotent retries are bounded, typed, and Retry-After-aware", async () => {
  assert.equal(parseRetryAfter("2", { now: 0, maximumMs: 5_000 }), 2_000);
  assert.equal(
    parseRetryAfter("Thu, 01 Jan 1970 00:00:09 GMT", { now: 1_000, maximumMs: 3_000 }),
    3_000,
  );
  assert.equal(parseRetryAfter("not-a-delay"), undefined);

  const delays = [];
  let attempts = 0;
  const response = await retryReleaseOperation(
    "GET release evidence",
    async () => {
      attempts += 1;
      return attempts === 1
        ? new Response("busy", { status: 503, headers: { "retry-after": "2" } })
        : new Response("ok", { status: 200 });
    },
    {
      maximumAttempts: 3,
      maximumDelayMs: 5_000,
      delay: async (milliseconds) => delays.push(milliseconds),
      now: () => 0,
      random: () => 0.5,
    },
  );
  assert.equal(response.status, 200);
  assert.equal(attempts, 2);
  assert.deepEqual(delays, [2_000]);

  let authAttempts = 0;
  const unauthorized = await retryReleaseOperation(
    "GET credential identity",
    async () => {
      authAttempts += 1;
      return new Response("unauthorized", { status: 401 });
    },
    { maximumAttempts: 5 },
  );
  assert.equal(unauthorized.status, 401);
  assert.equal(authAttempts, 1);

  await assert.rejects(
    retryReleaseOperation(
      "GET unavailable release",
      async () => {
        throw new TypeError("socket reset");
      },
      { maximumAttempts: 2, delay: async () => undefined, random: () => 0.5 },
    ),
    (error) => error instanceof ReleaseHttpError && error.attempts === 2 && error.retryable,
  );
});

test("structured failure evidence has one total redacted byte and node budget", () => {
  const nested = (depth) => depth === 0
    ? new Error(`secret-token ${"x".repeat(4_000)}`)
    : new AggregateError(
      Array.from({ length: 8 }, () => nested(depth - 1)),
      `aggregate secret-token ${depth}`,
      { cause: nested(depth - 1) },
    );
  const evidence = boundedStructuredError(nested(3), ["secret-token"]);
  const serialized = JSON.stringify(evidence);
  assert.ok(Buffer.byteLength(serialized) <= 16 * 1024);
  assert.equal(serialized.includes("secret-token"), false);
  assert.match(serialized, /\[redacted\]/i);
});

test("publication lease contention, loss, and acknowledgement-safe release fence mutations", async () => {
  const identity = {
    owner: "11111111-1111-4111-8111-111111111111",
    kind: "stable",
    id: tag,
    commit: head,
  };
  const lease = {
    version: 1,
    leaseId: "7.77777777-7777-4777-8777-777777777777",
    ...identity,
    generation: 7,
    expiresAt: "2099-01-01T00:00:00.000Z",
  };
  assert.equal(parsePublicationLease(lease, identity).generation, 7);
  assert.deepEqual(publicationLeaseFinalizeHeaders(lease), {
    "x-nanocodex-publication-lease-id": lease.leaseId,
    "x-nanocodex-publication-lease-owner": lease.owner,
    "x-nanocodex-publication-lease-generation": "7",
  });
  assert.throws(
    () => publicationLeaseFinalizeHeaders({ ...lease, generation: 8 }),
    /exact publication lease fence/,
  );
  assert.throws(
    () => parsePublicationLease({ ...lease, leaseId: `6.${lease.leaseId.split(".")[1]}` }, identity),
    /wrong identity or expiry/,
  );

  const successCalls = [];
  const success = await runWithPublicationLease(
    identity,
    leaseOperations(successCalls, lease),
    async (authority) => {
      await authority.checkpoint("before test mutation");
      successCalls.push("mutation");
      return "ok";
    },
    { heartbeatMs: 60_000 },
  );
  assert.equal(success, "ok");
  assert.deepEqual(successCalls, ["acquire", "heartbeat", "mutation", "release"]);

  const lostCalls = [];
  await assert.rejects(
    runWithPublicationLease(
      identity,
      leaseOperations(lostCalls, lease, { loseHeartbeat: true }),
      async (authority) => {
        await authority.checkpoint("before lost mutation");
        lostCalls.push("mutation");
      },
      { heartbeatMs: 60_000 },
    ),
    (error) => error instanceof PublicationLeaseLostError,
  );
  assert.deepEqual(lostCalls, ["acquire", "heartbeat", "release"]);

  const contentionCalls = [];
  await assert.rejects(
    runWithPublicationLease(
      identity,
      leaseOperations(contentionCalls, lease, { contend: true }),
      async () => contentionCalls.push("mutation"),
    ),
    (error) => error instanceof ReleaseHttpError && error.status === 409,
  );
  assert.deepEqual(contentionCalls, ["acquire"]);

  const aggregateCalls = [];
  await assert.rejects(
    runWithPublicationLease(
      identity,
      leaseOperations(aggregateCalls, lease, { releaseFailure: true }),
      async () => {
        throw new Error("operation failed");
      },
    ),
    (error) => error instanceof AggregateError && error.errors.length === 2,
  );
  assert.deepEqual(aggregateCalls, ["acquire", "release"]);
});

test("missing or invalid needed credentials fail before lease acquisition or mutation", async () => {
  const exactCrates = localCratePackages().map(crateRegistryRecord);
  assert.deepEqual(
    registryCredentialRequirements(exactCrates, { name: "nanocodex" }),
    { cargo: false, npm: false },
  );
  assert.deepEqual(
    registryCredentialRequirements(
      exactCrates.map((record, index) => index === 4 ? undefined : record),
      { name: "nanocodex" },
    ),
    { cargo: true, npm: false },
  );
  assert.deepEqual(
    registryCredentialRequirements(exactCrates, undefined),
    { cargo: false, npm: true },
  );

  for (const message of [
    "CARGO_REGISTRY_TOKEN is required",
    "npm credential identity failed with HTTP 401",
  ]) {
    const calls = [];
    const operations = orderedOperations(calls);
    operations.preflightCredentials = async () => {
      calls.push("preflight-credentials");
      throw new Error(message);
    };
    await assert.rejects(runReleaseSequence("publish", operations), new RegExp(message));
    assert.equal(calls.includes("lease:acquire"), false);
    assert.equal(calls.includes("publish-crates"), false);
    assert.equal(calls.includes("publish-npm"), false);
    assert.equal(calls.includes("finalize"), false);
  }

  const changedCalls = [];
  const changedOperations = orderedOperations(changedCalls);
  let preflightCount = 0;
  changedOperations.preflightCredentials = async () => {
    changedCalls.push("preflight-credentials");
    preflightCount += 1;
    if (preflightCount === 2) {
      throw new Error("registry state newly requires an unavailable credential");
    }
  };
  await assert.rejects(
    runReleaseSequence("publish", changedOperations),
    /newly requires an unavailable credential/,
  );
  assert.equal(changedCalls.includes("lease:acquire"), true);
  assert.equal(changedCalls.includes("lease:release"), true);
  assert.equal(changedCalls.includes("publish-crates"), false);
  assert.equal(changedCalls.includes("publish-npm"), false);
  assert.equal(changedCalls.includes("finalize"), false);

  const stageCalls = [];
  await runReleaseSequence("stage", orderedOperations(stageCalls));
  assert.equal(stageCalls.includes("preflight-credentials"), false);
  assert.equal(stageCalls.includes("lease:acquire"), false);
});

test("lease loss and contention stop the publish sequence, while finalized replay skips the lease", async () => {
  const contentionCalls = [];
  await assert.rejects(
    runReleaseSequence(
      "publish",
      orderedOperations(contentionCalls, { leaseContention: true }),
    ),
    (error) => error instanceof ReleaseHttpError && error.status === 409,
  );
  assert.equal(contentionCalls.includes("publish-crates"), false);
  assert.equal(contentionCalls.includes("lease:release"), false);

  const lossCalls = [];
  await assert.rejects(
    runReleaseSequence("publish", orderedOperations(lossCalls, { leaseLossAt: 1 })),
    (error) => error instanceof PublicationLeaseLostError,
  );
  assert.equal(lossCalls.includes("publish-crates"), false);
  assert.equal(lossCalls.includes("publish-npm"), false);
  assert.equal(lossCalls.includes("finalize"), false);
  assert.equal(lossCalls.includes("lease:release"), true);

  const finalLossCalls = [];
  await assert.rejects(
    runReleaseSequence(
      "publish",
      orderedOperations(finalLossCalls, { leaseLossAt: 4 }),
    ),
    (error) => error instanceof PublicationLeaseLostError,
  );
  assert.equal(finalLossCalls.includes("finalize"), true);
  assert.equal(finalLossCalls.includes("verify-public"), true);
  assert.equal(finalLossCalls.includes("lease:release"), true);

  const replayCalls = [];
  const replay = await runReleaseSequence(
    "publish",
    orderedOperations(replayCalls, { existing: true }),
  );
  assert.equal(replay.action, "already-published");
  assert.equal(replayCalls.includes("lease:acquire"), false);
});

test("replay reconciles a lost finalization acknowledgement without a second mutation", async () => {
  const calls = [];
  const state = { finalized: false };
  const operations = orderedOperations(calls);
  operations.readPublished = async () => {
    calls.push("read-published");
    return state.finalized ? { manifestSha256: "c".repeat(64) } : undefined;
  };
  operations.finalize = async () => {
    calls.push("finalize");
    state.finalized = true;
    throw new Error("lost finalization acknowledgement");
  };
  await assert.rejects(
    runReleaseSequence("publish", operations),
    /lost finalization acknowledgement/,
  );
  assert.equal(calls.filter((call) => call === "finalize").length, 1);
  assert.equal(calls.filter((call) => call === "lease:release").length, 1);

  const replay = await runReleaseSequence("publish", operations);
  assert.equal(replay.action, "already-published");
  assert.equal(calls.filter((call) => call === "finalize").length, 1);
  assert.equal(calls.filter((call) => call === "lease:acquire").length, 1);
});

function masterState() {
  return {
    publication: {
      version: 1,
      head,
      branch: "master",
      ref: "refs/heads/master",
      lane: { type: "master" },
    },
    run: {
      version: 1,
      head,
      workflowId,
      state: "dispatched",
    },
  };
}

function greenRunValue(artifacts) {
  return {
    version: 1,
    head,
    workflowId,
    state: "dispatched",
    workflow: { status: "complete" },
    result: {
      version: 1,
      head,
      workflowId,
      status: "success",
      artifacts,
    },
  };
}

function npmArtifact(bytes) {
  return {
    key: `runs/${head}/artifacts/npm-package.tgz`,
    size: bytes.byteLength,
    sha256: createHash("sha256").update(bytes).digest("hex"),
    contentType: "application/gzip",
  };
}

function npmHeaders(artifact) {
  return new Headers({
    "cache-control": "public, max-age=31536000, immutable",
    "content-disposition": `attachment; filename="nanocodex-${head}-npm-package.tgz"`,
    "content-length": String(artifact.size),
    "content-type": artifact.contentType,
    "x-content-type-options": "nosniff",
    "x-nanocodex-sha256": artifact.sha256,
  });
}

function parseCrateSafeNpmRecord(expected) {
  return parseNpmRegistryVersion({
    name: "nanocodex",
    version: expected.version,
    dist: {
      integrity: expected.integrity,
      shasum: expected.sha1,
      tarball: `https://registry.npmjs.org/nanocodex/-/nanocodex-${expected.version}.tgz`,
    },
  }, expected);
}

function readyEvidence(tested) {
  const release = `distribution/stable/${tag}`;
  const linux = `${release}/components/linux`;
  const npmName = `nanocodex-${version}.tgz`;
  const assets = [
    releaseAsset("PROVENANCE.json", "linux", `${release}/PROVENANCE.json`, 11, "a", "application/json"),
    releaseAsset("SHA256SUMS", "linux", `${release}/SHA256SUMS`, 13, "b", "text/plain; charset=utf-8"),
    {
      name: npmName,
      platform: "npm",
      key: `${release}/components/npm/${npmName}`,
      size: tested.size,
      sha256: tested.sha256,
      contentType: tested.contentType,
    },
    releaseAsset("nanocodex-aarch64-apple-darwin", "aarch64-apple-darwin", `${release}/components/macos/nanocodex-aarch64-apple-darwin`, 17, "c", "application/octet-stream"),
    releaseAsset("nanocodex-vm-guest-x86_64-unknown-linux-musl", "x86_64-unknown-linux-musl", `${linux}/nanocodex-vm-guest-x86_64-unknown-linux-musl`, 19, "d", "application/octet-stream"),
    releaseAsset("nanocodex-vm-guest-x86_64-unknown-linux-musl.gz", "x86_64-unknown-linux-musl", `${linux}/nanocodex-vm-guest-x86_64-unknown-linux-musl.gz`, 23, "e", "application/gzip"),
    releaseAsset("nanocodex-x86_64-unknown-linux-gnu", "x86_64-unknown-linux-gnu", `${linux}/nanocodex-x86_64-unknown-linux-gnu`, 29, "f", "application/octet-stream"),
    releaseAsset("nanocodex-x86_64-unknown-linux-gnu.gz", "x86_64-unknown-linux-gnu", `${linux}/nanocodex-x86_64-unknown-linux-gnu.gz`, 31, "0", "application/gzip"),
  ].sort((left, right) => left.name.localeCompare(right.name));
  return {
    version: 1,
    status: "ready",
    channel: "stable",
    tagName: tag,
    head,
    workflowId: `release-${tag}-${head}`,
    durationMs: 1234,
    completedAt: "2026-08-22T11:00:00.000Z",
    staged: {
      draft: {
        version: 1,
        kind: "stable",
        id: tag,
        tag,
        commit: head,
        channel: "latest",
        expectedChannel: "v0.4.0",
        assets,
        createdAt: "2026-08-22T11:00:00.000Z",
      },
    },
  };
}

function reviewedFixture({ toolchain = {}, crates = localCratePackages() } = {}) {
  const tgz = npmTarball();
  const selected = {
    ...npmArtifact(tgz),
    name: `nanocodex-${version}.tgz`,
    platform: "npm",
  };
  const ready = parseReadyDistribution(readyEvidence(selected), tag, head, selected);
  const draft = parseDraftStableDescriptor({
    draft: {
      version: 1,
      kind: "stable",
      id: tag,
      tag,
      commit: head,
      channel: "latest",
      expectedChannel: ready.draft.expectedChannel,
      createdAt: ready.draft.createdAt,
      assets: ready.draft.assets.map(({ key: _key, ...asset }) => ({
        ...asset,
        downloadPath:
          `/api/releases/drafts/stable/${tag}/assets/${encodeURIComponent(asset.name)}`,
      })),
    },
  }, ready);
  const metadata = verifyNpmPackageBytes(tgz, selected, tag);
  const plan = createReviewedReleasePlan({
    ready,
    draft,
    crates,
    npm: metadata,
    assets: draft.assets,
    toolchain: {
      cargo: "cargo 1.89.0 (test)",
      rustc: "rustc 1.89.0 (test)",
      node: "v22.18.0",
      npm: "11.5.2",
      ...toolchain,
    },
  });
  return {
    ready,
    plan,
    npmPackage: { path: "/tmp/mock-nanocodex.tgz", metadata },
  };
}

function leaseOperations(calls, lease, {
  contend = false,
  loseHeartbeat = false,
  releaseFailure = false,
} = {}) {
  return {
    async acquire() {
      calls.push("acquire");
      if (contend) {
        throw new ReleaseHttpError("acquire stable publication lease", {
          status: 409,
          retryable: false,
        });
      }
      return lease;
    },
    async heartbeat() {
      calls.push("heartbeat");
      if (loseHeartbeat) throw new PublicationLeaseLostError("test heartbeat");
      return { ...lease, expiresAt: "2099-01-01T00:00:01.000Z" };
    },
    async release() {
      calls.push("release");
      if (releaseFailure) throw new Error("release acknowledgement failed");
    },
    delay(_milliseconds, signal) {
      return new Promise((resolveDelay, reject) => {
        const abort = () => reject(signal.reason);
        if (signal.aborted) abort();
        else signal.addEventListener("abort", abort, { once: true });
      });
    },
  };
}

function releaseAsset(name, platform, key, size, hashCharacter, contentType) {
  return {
    name,
    platform,
    key,
    size,
    sha256: hashCharacter.repeat(64),
    contentType,
  };
}

function npmTarball(indexSource = "export {};\n", provenance = false) {
  const packageJson = `${JSON.stringify({
    name: "nanocodex",
    version,
    type: "module",
    publishConfig: { access: "public", provenance },
  })}\n`;
  return gzipSync(tarArchive([
    { path: "package/", type: "5", body: Buffer.alloc(0) },
    { path: "package/package.json", type: "0", body: Buffer.from(packageJson) },
    { path: "package/index.mjs", type: "0", body: Buffer.from(indexSource) },
  ]), { level: 9, mtime: 0 });
}

function tarArchive(entries) {
  const chunks = [];
  for (const entry of entries) {
    const body = Buffer.from(entry.body);
    const header = Buffer.alloc(512);
    writeTarText(header, 0, 100, entry.path);
    writeTarOctal(header, 100, 8, entry.type === "5" ? 0o755 : 0o644);
    writeTarOctal(header, 108, 8, 0);
    writeTarOctal(header, 116, 8, 0);
    writeTarOctal(header, 124, 12, body.byteLength);
    writeTarOctal(header, 136, 12, 0);
    header.fill(0x20, 148, 156);
    header[156] = entry.type.charCodeAt(0);
    writeTarText(header, 257, 6, "ustar");
    writeTarText(header, 263, 2, "00");
    const checksum = [...header].reduce((sum, byte) => sum + byte, 0);
    const checksumText = checksum.toString(8).padStart(6, "0");
    header.write(checksumText, 148, 6, "ascii");
    header[154] = 0;
    header[155] = 0x20;
    chunks.push(header, body, Buffer.alloc((512 - (body.byteLength % 512)) % 512));
  }
  chunks.push(Buffer.alloc(1024));
  return Buffer.concat(chunks);
}

function writeTarText(header, offset, length, value) {
  const bytes = Buffer.from(value);
  if (bytes.byteLength > length) throw new Error(`test tar field is too long: ${value}`);
  bytes.copy(header, offset);
}

function writeTarOctal(header, offset, length, value) {
  const text = value.toString(8).padStart(length - 1, "0");
  header.write(text, offset, length - 1, "ascii");
  header[offset + length - 1] = 0;
}

function authorityKeys(env) {
  return Object.keys(env)
    .filter((name) => /(?:TOKEN|SECRET|PASSWORD|API_KEY|PRIVATE_KEY)/i.test(name))
    .sort();
}

function localCratePackages() {
  return releaseCrateNames.map((crate) => {
    const bytes = Buffer.from(`local ${crate}@${version}`);
    return {
      crate,
      version,
      path: resolve(tmpdir(), "nanocodex-mocked-crates", `${crate}-${version}.crate`),
      size: bytes.byteLength,
      sha256: createHash("sha256").update(bytes).digest("hex"),
    };
  });
}

function crateRegistryRecord(localPackage, overrides = {}) {
  return {
    crate: localPackage.crate,
    version: localPackage.version,
    checksum: localPackage.sha256,
    downloadUrl:
      `https://crates.io/api/v1/crates/${localPackage.crate}/${localPackage.version}/download`,
    ...overrides,
  };
}

function orderedOperations(calls, {
  existing = false,
  existingCrateMismatch = false,
  leaseContention = false,
  leaseLossAt,
  npmRegistryFailure = false,
  publishedCrateMismatch = false,
  repackageFailure = false,
  replayTrustFailure = false,
} = {}) {
  const trust = { binding: { commit: head } };
  const fixture = reviewedFixture();
  const ready = fixture.ready;
  const crates = localCratePackages();
  let heartbeatCount = 0;
  const lease = {
    version: 1,
    leaseId: "1.11111111-1111-4111-8111-111111111111",
    owner: "11111111-1111-4111-8111-111111111111",
    kind: "stable",
    id: tag,
    commit: head,
    generation: 1,
    expiresAt: "2099-01-01T00:00:00.000Z",
  };
  return {
    planPath: "/tmp/mock-reviewed-plan.json",
    leaseOwner: lease.owner,
    leaseHeartbeatMs: 60_000,
    publicationLease: {
      async acquire() {
        calls.push("lease:acquire");
        if (leaseContention) {
          throw new ReleaseHttpError("acquire stable publication lease", {
            status: 409,
            retryable: false,
          });
        }
        return lease;
      },
      async heartbeat() {
        calls.push("lease:heartbeat");
        heartbeatCount += 1;
        if (heartbeatCount === leaseLossAt) {
          throw new PublicationLeaseLostError("simulated heartbeat");
        }
        return { ...lease, expiresAt: "2099-01-01T00:00:01.000Z" };
      },
      async release() {
        calls.push("lease:release");
      },
      delay(_milliseconds, signal) {
        return new Promise((resolveDelay, reject) => {
          const abort = () => reject(signal.reason);
          if (signal.aborted) abort();
          else signal.addEventListener("abort", abort, { once: true });
        });
      },
    },
    async verifyTrust(phase) {
      calls.push(`trust:${phase}`);
      return trust;
    },
    async packageCrates() {
      calls.push("package-crates");
      if (repackageFailure) throw new Error("simulated Cargo repackage failure");
      return crates;
    },
    async verifyExistingCrates() {
      calls.push("verify-existing-crates");
      if (existingCrateMismatch) {
        throw new Error("simulated existing crate checksum mismatch");
      }
      return [];
    },
    async stage() {
      calls.push("stage");
      return ready;
    },
    async readReady() {
      calls.push("read-ready");
      return ready;
    },
    async reviewPlan() {
      calls.push("review-plan");
      return { plan: fixture.plan, npmPackage: fixture.npmPackage };
    },
    async reviewPublishedPlan() {
      calls.push("review-public-plan");
      return { plan: fixture.plan, npmPackage: fixture.npmPackage };
    },
    async persistPlan() {
      calls.push("persist-plan");
      return { action: "created", path: "/tmp/mock-reviewed-plan.json" };
    },
    async readPlan() {
      calls.push("read-plan");
      return fixture.plan;
    },
    async assertTrust(_trust, phase) {
      calls.push(`trust:${phase}`);
      if (replayTrustFailure && phase === "after replay verification") {
        throw new TagMovedError(tag, phase, {}, {});
      }
    },
    async assertReady(_ready, phase) {
      calls.push(`ready:${phase}`);
    },
    async readPublished() {
      calls.push("read-published");
      return existing ? { manifestSha256: "c".repeat(64) } : undefined;
    },
    async inspectNpmRegistry() {
      calls.push("inspect-npm");
      return undefined;
    },
    async preflightCredentials() {
      calls.push("preflight-credentials");
    },
    async publishCrates() {
      calls.push("publish-crates");
      if (publishedCrateMismatch) {
        throw new Error("simulated newly published crate checksum mismatch");
      }
      return { action: "published" };
    },
    async verifyCrates() {
      calls.push("verify-crates");
      return releaseCrateNames.map((crate) => ({ crate, version }));
    },
    async publishNpm() {
      calls.push("publish-npm");
      return { action: "published" };
    },
    async verifyNpmRegistry() {
      calls.push("verify-npm");
      if (npmRegistryFailure) throw new Error("npm registry unavailable");
      return { name: "nanocodex", version };
    },
    async finalize() {
      calls.push("finalize");
      return { status: "released" };
    },
    async verifyPublic() {
      calls.push("verify-public");
      return { manifestSha256: "c".repeat(64) };
    },
  };
}

function retryOperations(calls, state) {
  const base = orderedOperations(calls);
  return {
    ...base,
    async publishCrates() {
      const action = state.cratesPublished ? "already-published" : "published";
      state.cratesPublished = true;
      calls.push(`publish-crates:${action}`);
      return { action };
    },
    async verifyNpmRegistry() {
      calls.push("verify-npm");
      if (state.failNpmVerification) {
        state.failNpmVerification = false;
        throw new Error("simulated lost npm acknowledgement");
      }
      return { name: "nanocodex", version };
    },
  };
}

function canonicalJson(value) {
  if (value === null || typeof value === "boolean" || typeof value === "string") {
    return JSON.stringify(value);
  }
  if (typeof value === "number") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  return `{${Object.keys(value).sort().map((key) =>
    `${JSON.stringify(key)}:${canonicalJson(value[key])}`
  ).join(",")}}`;
}
