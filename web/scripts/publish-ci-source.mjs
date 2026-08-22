import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { lstat, mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { Readable } from "node:stream";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { deflateRawSync } from "node:zlib";

const execFileAsync = promisify(execFile);
const scriptPath = fileURLToPath(import.meta.url);
const projectRoot = resolve(dirname(scriptPath), "..");
const publicationBranch = "master";
const publicationRef = `refs/heads/${publicationBranch}`;
const authoritativeRepositoryUrl = "https://github.com/gakonst/nanocodex.git";
const canonicalOriginFetch = "+refs/heads/*:refs/remotes/origin/*";
const sha1Pattern = /^[a-f0-9]{40}$/;
const closeIdPattern =
  /^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;
const maximumGitOutputBytes = 16 * 1024 * 1024;
const maximumCargoVendorBytes = 256 * 1024 * 1024;
const maximumStateBytes = 1024 * 1024;
const maximumErrorBytes = 1_000;
// Trusted producer-to-source handoff. This value must be the SHA-256 returned by
// publish-ci-cargo-vendor; source publication never discovers a lock-only winner.
const cargoVendorSha256Environment = "NANOCODEX_CI_CARGO_VENDOR_SHA256";

export async function main({ env = process.env, log = console.log } = {}) {
  const repository = resolve(env.NANOCODEX_REPO ?? resolve(projectRoot, ".."));
  const origin = parseOrigin(requiredEnvironment(env, "NANOCODEX_CI_ORIGIN"));
  const token = requiredEnvironment(env, "NANOCODEX_CI_TOKEN");
  const cargoVendorSha256 = requiredEnvironment(env, cargoVendorSha256Environment);
  if (!/^[a-f0-9]{64}$/.test(cargoVendorSha256)) {
    throw new Error(`${cargoVendorSha256Environment} must be a lowercase SHA-256`);
  }
  const target = parsePublicationTarget(env);
  const snapshot = target.type === "master"
    ? await prepareMasterPublication(repository)
    : await readRepositorySnapshot(repository, target);
  const laneState = await readSourceLaneState(origin, token, target);
  const { expectedHead, reopen } = laneState;
  const laneIsCurrent = target.type === "master" ||
    laneState.currentPullRequestHead === target.pullRequestHead;
  if (expectedHead === snapshot.head && reopen == null && laneIsCurrent) {
    if (laneState.cargoVendorSha256 !== cargoVendorSha256) {
      throw new Error("current CI source is bound to a different Cargo vendor hash");
    }
    log(`CI source is current (${snapshot.head.slice(0, 7)})`);
    return;
  }
  if (target.type === "master") {
    await requireFastForward(repository, expectedHead, snapshot.head);
  }
  const cargoLockBlob = await git(repository, [
    "rev-parse",
    "--verify",
    `${snapshot.head}:Cargo.lock`,
  ]);
  if (!/^[a-f0-9]{40}$/.test(cargoLockBlob)) {
    throw new Error("Git resolved an invalid Cargo.lock blob ID");
  }
  const cargoVendor = await readCargoVendor(
    origin,
    token,
    cargoLockBlob,
    cargoVendorSha256,
  );
  const rustSecRevision = requiredEnvironment(env, "NANOCODEX_RUSTSEC_REVISION");
  if (!/^[a-f0-9]{40}$/.test(rustSecRevision)) {
    throw new Error("NANOCODEX_RUSTSEC_REVISION must be a full lowercase Git SHA-1");
  }
  const rustSec = await readRustSec(origin, token, rustSecRevision);
  const temporaryDirectory = await mkdtemp(resolve(tmpdir(), "nanocodex-ci-source-"));

  try {
    const artifacts = await buildSourceArtifacts({
      repository,
      temporaryDirectory,
      head: snapshot.head,
    });
    await assertRepositorySnapshot(repository, snapshot, target);

    await Promise.all([
      uploadFile(
        origin,
        token,
        `${snapshot.head}/source.tar.gz`,
        artifacts.archive,
        "application/gzip",
      ),
      uploadFile(
        origin,
        token,
        `${snapshot.head}/tree.json`,
        artifacts.tree,
        "application/json",
      ),
    ]);

    const publication = {
      version: 1,
      head: snapshot.head,
      branch: target.branch,
      ref: target.ref,
      ...(target.type === "pull_request" ? { lane: target.lane } : {}),
      archive: artifactRecord(
        `sources/${snapshot.head}/source.tar.gz`,
        artifacts.archive,
      ),
      tree: artifactRecord(`sources/${snapshot.head}/tree.json`, artifacts.tree),
      cargoLockBlob,
      cargoVendor,
      rustSecRevision,
      rustSec,
      publishedAt: new Date().toISOString(),
    };
    const body = Buffer.from(JSON.stringify({
      expectedHead,
      publication,
      ...(reopen == null ? {} : { reopen }),
    }), "utf8");
    await assertRepositorySnapshot(repository, snapshot, target);
    const response = await authenticatedFetch(
      `${origin}/api/ci/source/publish`,
      token,
      {
        method: "PUT",
        headers: {
          "content-length": String(body.byteLength),
          "content-type": "application/json",
        },
        body,
      },
    );
    if (!response.ok) throw new Error(await responseError("publish CI source", response));
    log(`Published CI source ${snapshot.head.slice(0, 7)}`);
  } finally {
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
}

export async function prepareMasterPublication(repository) {
  await assertMasterRepositoryShape(repository);
  await git(repository, [
    "fetch",
    "--no-tags",
    "--no-write-fetch-head",
    "--recurse-submodules=no",
    "origin",
    `+${publicationRef}:refs/remotes/origin/${publicationBranch}`,
  ]);
  return assertMasterPublicationAuthority(repository);
}

export async function assertMasterPublicationAuthority(repository, expectedHead) {
  await assertMasterRepositoryShape(repository);
  const [
    head,
    localMaster,
    trackingHead,
    upstream,
    remoteHead,
    masterRefs,
  ] = await Promise.all([
    git(repository, ["rev-parse", "--verify", "HEAD^{commit}"]),
    git(repository, ["rev-parse", "--verify", `${publicationRef}^{commit}`]),
    git(repository, [
      "rev-parse",
      "--verify",
      `refs/remotes/origin/${publicationBranch}^{commit}`,
    ]),
    git(repository, ["rev-parse", "--symbolic-full-name", "@{upstream}"]),
    readAuthoritativeMaster(repository),
    readRemoteMasterRefs(repository),
  ]);
  assertRemoteMasterTopology(masterRefs, true);
  if (
    !sha1Pattern.test(head) ||
    localMaster !== head ||
    trackingHead !== head ||
    upstream !== `refs/remotes/origin/${publicationBranch}` ||
    remoteHead !== head ||
    (expectedHead != null && head !== expectedHead)
  ) {
    throw new Error(
      "master publication requires local HEAD, fetched origin/master, and fresh GitHub master to be identical",
    );
  }
  return { head, ref: publicationRef };
}

async function readAuthoritativeMaster(repository) {
  const output = await git(repository, [
    "ls-remote",
    "--exit-code",
    "--refs",
    authoritativeRepositoryUrl,
    publicationRef,
  ]);
  const match = /^([a-f0-9]{40})\trefs\/heads\/master$/.exec(output);
  if (match == null) {
    throw new Error("git ls-remote returned an invalid authoritative master ref");
  }
  return match[1];
}

async function assertMasterRepositoryShape(repository) {
  await assertSafeLocalGitConfig(repository, {
    type: "master",
    branch: publicationBranch,
    ref: publicationRef,
  });
  await rejectObjectSubstitution(repository);
  const [ref, status, bare, shallow, masterRefs] = await Promise.all([
    git(repository, ["symbolic-ref", "--quiet", "HEAD"]),
    git(repository, [
      "status",
      "--porcelain=v1",
      "-z",
      "--untracked-files=all",
      "--ignore-submodules=none",
    ]),
    git(repository, ["rev-parse", "--is-bare-repository"]),
    git(repository, ["rev-parse", "--is-shallow-repository"]),
    readRemoteMasterRefs(repository),
  ]);
  if (ref !== publicationRef) {
    throw new Error(
      `master publication requires an attached ${publicationRef} checkout; observed ${ref}`,
    );
  }
  if (status !== "") {
    throw new Error("master publication requires a clean tracked and untracked checkout");
  }
  if (bare !== "false" || shallow !== "false") {
    throw new Error("master publication requires a complete non-bare repository");
  }
  assertRemoteMasterTopology(masterRefs, false);
  await assertCanonicalOrigin(repository);
}

async function readRemoteMasterRefs(repository) {
  const output = await git(repository, [
    "for-each-ref",
    "--format=%(refname)%00%(symref)",
    "refs/remotes",
  ]);
  return output.split("\n").filter(Boolean).map((entry) => {
    const fields = entry.split("\0");
    if (fields.length !== 2) throw new Error("git returned invalid remote-tracking refs");
    return { ref: fields[0], symref: fields[1] };
  }).filter(({ ref }) => /^refs\/remotes\/.+\/master$/.test(ref));
}

function assertRemoteMasterTopology(masterRefs, requireOrigin) {
  if (
    masterRefs.length > 1 ||
    (masterRefs.length === 1 && (
      masterRefs[0].ref !== `refs/remotes/origin/${publicationBranch}` ||
      masterRefs[0].symref !== ""
    )) ||
    (requireOrigin && masterRefs.length !== 1)
  ) {
    throw new Error("master publication rejects extra remote-tracking master refs");
  }
}

async function assertCanonicalOrigin(repository) {
  const [remotes, urls, fetches, pushUrls, branchRemotes, branchMerges] = await Promise.all([
    git(repository, ["remote"]),
    readScopedConfigValues(repository, "remote.origin.url"),
    readScopedConfigValues(repository, "remote.origin.fetch"),
    readScopedConfigValues(repository, "remote.origin.pushurl"),
    readScopedConfigValues(repository, `branch.${publicationBranch}.remote`),
    readScopedConfigValues(repository, `branch.${publicationBranch}.merge`),
  ]);
  if (
    JSON.stringify(remotes.split("\n").filter(Boolean)) !== JSON.stringify(["origin"]) ||
    JSON.stringify(urls) !== JSON.stringify([authoritativeRepositoryUrl]) ||
    JSON.stringify(fetches) !== JSON.stringify([canonicalOriginFetch]) ||
    pushUrls.length !== 0 ||
    JSON.stringify(branchRemotes) !== JSON.stringify(["origin"]) ||
    JSON.stringify(branchMerges) !== JSON.stringify([publicationRef])
  ) {
    throw new Error(
      `master publication requires one canonical HTTPS origin at ${authoritativeRepositoryUrl}`,
    );
  }
}

export function parsePublicationTarget(env = process.env) {
  const names = [
    "NANOCODEX_CI_PULL_REQUEST_NUMBER",
    "NANOCODEX_CI_PULL_REQUEST_HEAD",
  ];
  const configured = names.filter((name) => env[name]?.trim());
  if (configured.length === 0) {
    return { type: "master", branch: publicationBranch, ref: publicationRef };
  }
  if (configured.length !== names.length) {
    throw new Error(`pull-request source publication requires ${names.join(", ")}`);
  }
  const rawNumber = requiredEnvironment(env, names[0]);
  if (!/^[1-9][0-9]*$/.test(rawNumber)) {
    throw new Error("NANOCODEX_CI_PULL_REQUEST_NUMBER must be a positive canonical integer");
  }
  const number = Number(rawNumber);
  if (!Number.isSafeInteger(number)) {
    throw new Error("NANOCODEX_CI_PULL_REQUEST_NUMBER is too large");
  }
  const pullRequestHead = requiredSha1(env, names[1]);
  const branch = `pull/${number}/merge`;
  const ref = `refs/pull/${number}/merge`;
  return {
    type: "pull_request",
    number,
    pullRequestHead,
    branch,
    ref,
    lane: { type: "pull_request", number, pullRequestHead },
  };
}

async function requireFastForward(repository, expectedHead, head) {
  if (expectedHead == null) return;
  try {
    await git(repository, ["merge-base", "--is-ancestor", expectedHead, head]);
  } catch (error) {
    throw new Error(
      `CI source ${head.slice(0, 7)} must advance published head ${expectedHead.slice(0, 7)}`,
      { cause: error },
    );
  }
}

export async function readRepositorySnapshot(
  repository,
  target = { type: "master", branch: publicationBranch, ref: publicationRef },
) {
  if (target.type === "master") return assertMasterPublicationAuthority(repository);
  if (target.type !== "pull_request") throw new Error("unsupported CI source target");
  await assertSafeLocalGitConfig(repository, target);
  await rejectObjectSubstitution(repository);
  const head = await git(repository, ["rev-parse", "--verify", "HEAD^{commit}"]);
  if (!sha1Pattern.test(head)) {
    throw new Error(`Git resolved an invalid HEAD object ID: ${head.slice(0, 80)}`);
  }

  if (target.type === "pull_request") {
    let attachedRef;
    try {
      attachedRef = await git(repository, ["symbolic-ref", "--quiet", "HEAD"]);
    } catch (error) {
      if (error?.exitCode !== 1) throw error;
      attachedRef = undefined;
    }
    if (attachedRef !== undefined) {
      throw new Error(
        `pull-request CI source publication requires a detached checkout; repository is on ${attachedRef}`,
      );
    }
    const [mergeRef, pullRequestRef, baseRef, originUrl, parents] = await Promise.all([
      git(repository, ["rev-parse", "--verify", `${target.ref}^{commit}`]),
      git(repository, [
        "rev-parse",
        "--verify",
        `refs/pull/${target.number}/head^{commit}`,
      ]),
      git(repository, [
        "rev-parse",
        "--verify",
        "refs/remotes/origin/master^{commit}",
      ]),
      git(repository, ["remote", "get-url", "origin"]),
      git(repository, ["show", "--no-patch", "--format=%P", head]),
    ]);
    if (
      head !== mergeRef || pullRequestRef !== target.pullRequestHead
    ) {
      throw new Error("pull-request CI source checkout does not match its authoritative refs");
    }
    if (originUrl !== authoritativeRepositoryUrl) {
      throw new Error("pull-request CI source checkout has a non-authoritative origin");
    }
    const mergeParents = parents.split(" ").filter(Boolean);
    if (
      mergeParents.length !== 2 || mergeParents[0] !== baseRef ||
      mergeParents[1] !== target.pullRequestHead
    ) {
      throw new Error(
        "pull-request CI source must be the exact two-parent merge of origin/master and the PR head",
      );
    }
    parseGitTree(await gitBuffer(repository, [
      "ls-tree",
      "-r",
      "-z",
      "--full-tree",
      head,
    ]));
    await requireCleanPullRequestState(repository);
    return { head, ref: target.ref };
  }

  throw new Error("unreachable CI source target");
}

export async function buildSourceArtifacts({ repository, temporaryDirectory, head }) {
  const archivePath = resolve(temporaryDirectory, "source.tar.gz");
  const tarPath = resolve(temporaryDirectory, "source.tar");
  const treePath = resolve(temporaryDirectory, "tree.json");
  await git(repository, [
    "archive",
    "--format=tar",
    `--prefix=nanocodex-${head}/`,
    `--output=${tarPath}`,
    head,
  ]);
  const tar = await readFile(tarPath);
  await writeFile(archivePath, deterministicGzip(tar));
  await rm(tarPath);

  const rawTree = await gitBuffer(repository, [
    "ls-tree",
    "-r",
    "-z",
    "--full-tree",
    head,
  ]);
  const archive = await describeArtifact(archivePath);
  const treeDocument = {
    version: 1,
    head,
    archive: { size: archive.size, sha256: archive.sha256 },
    files: parseGitTree(rawTree),
  };
  await writeFile(treePath, `${JSON.stringify(treeDocument)}\n`);

  const tree = await describeArtifact(treePath);
  return { archive, tree, treeDocument };
}

export function parseGitTree(rawTree) {
  if (!Buffer.isBuffer(rawTree)) throw new TypeError("Git tree output must be a Buffer");
  const files = [];
  let offset = 0;
  const decoder = new TextDecoder("utf-8", { fatal: true });
  while (offset < rawTree.length) {
    const end = rawTree.indexOf(0, offset);
    if (end < 0) throw new Error("git ls-tree returned an unterminated entry");
    const record = decoder.decode(rawTree.subarray(offset, end));
    offset = end + 1;
    const tab = record.indexOf("\t");
    const metadata = tab < 0 ? [] : record.slice(0, tab).split(" ");
    const path = tab < 0 ? "" : record.slice(tab + 1);
    if (metadata.length !== 3 || path.length === 0) {
      throw new Error("git ls-tree returned a malformed entry");
    }
    const [mode, type, sha] = metadata;
    if (mode === "160000" || type === "commit") {
      throw new Error(`CI source archives do not support gitlinks: ${path}`);
    }
    if (
      type !== "blob" ||
      !["100644", "100755", "120000"].includes(mode) ||
      !/^[a-f0-9]{40}$/.test(sha)
    ) {
      throw new Error(`git ls-tree returned an unsupported entry: ${path}`);
    }
    files.push({ path, sha, mode });
  }
  return files;
}

export async function readSourceHead(origin, token) {
  return (await readSourceLaneState(
    origin,
    token,
    { type: "master", branch: publicationBranch, ref: publicationRef },
  )).expectedHead;
}

export async function readSourceLaneState(origin, token, target) {
  const path = target.type === "pull_request"
    ? `/api/ci/source/pull-requests/${target.number}/state`
    : "/api/ci/source/state";
  const response = await authenticatedFetch(`${origin}${path}`, token, {
    headers: { accept: "application/json" },
  });
  if (response.status === 404) {
    if (target.type === "master") {
      await response.body?.cancel();
      return { expectedHead: null };
    }
    const value = await readJsonResponse(response, "read pull-request CI source state");
    if (value?.error === "not_published") return { expectedHead: null };
    if (
      value?.error === "pull_request_closed" && value.number === target.number &&
      typeof value.closeId === "string" && closeIdPattern.test(value.closeId) &&
      typeof value.mergeHead === "string" && sha1Pattern.test(value.mergeHead) &&
      typeof value.pullRequestHead === "string" && sha1Pattern.test(value.pullRequestHead) &&
      typeof value.closedAt === "string" && Number.isFinite(Date.parse(value.closedAt))
    ) {
      return {
        expectedHead: null,
        reopen: {
          closeId: value.closeId,
          mergeHead: value.mergeHead,
          pullRequestHead: value.pullRequestHead,
        },
      };
    }
    throw new Error("read pull-request CI source state returned an invalid closed lane");
  }
  if (!response.ok) throw new Error(await responseError("read CI source state", response));
  const state = await readJsonResponse(response, "read CI source state");
  const head = state?.publication?.head;
  const cargoVendorSha256 = state?.publication?.cargoVendor?.sha256;
  if (
    typeof head !== "string" || !sha1Pattern.test(head) ||
    typeof cargoVendorSha256 !== "string" || !/^[a-f0-9]{64}$/.test(cargoVendorSha256)
  ) {
    throw new Error("read CI source state returned an invalid publication identity");
  }
  if (target.type === "pull_request") {
    const publication = state?.publication;
    const run = state?.run;
    if (
      publication?.version !== 1 ||
      publication?.branch !== target.branch || publication?.ref !== target.ref ||
      publication?.lane?.type !== "pull_request" ||
      publication.lane.number !== target.number ||
      typeof publication.lane.pullRequestHead !== "string" ||
      !sha1Pattern.test(publication.lane.pullRequestHead) ||
      run?.version !== 1 || run.head !== head ||
      run.workflowId !== `ci-${head}` ||
      !["pending", "dispatched"].includes(run.state) ||
      typeof run.publishedAt !== "string" ||
      !Number.isFinite(Date.parse(run.publishedAt))
    ) {
      throw new Error("read CI source state returned a different pull-request lane");
    }
    return {
      expectedHead: head,
      currentPullRequestHead: publication.lane.pullRequestHead,
      cargoVendorSha256,
    };
  }
  return { expectedHead: head, cargoVendorSha256 };
}

export async function readCargoVendor(origin, token, cargoLockBlob, bundleSha256) {
  if (!sha1Pattern.test(cargoLockBlob) || !/^[a-f0-9]{64}$/.test(bundleSha256)) {
    throw new Error("invalid CI Cargo vendor identity");
  }
  const response = await authenticatedFetch(
    `${origin}/api/ci/cargo-vendor/${cargoLockBlob}/${bundleSha256}/bundle.tar.gz`,
    token,
    { method: "HEAD" },
  );
  if (response.status === 404) {
    throw new Error(
      `CI Cargo vendor ${cargoLockBlob.slice(0, 7)}/${bundleSha256.slice(0, 12)} ` +
      "is not published; " +
      "run npm run publish:ci-cargo-vendor --prefix web first",
    );
  }
  if (!response.ok) throw new Error(await responseError("read CI Cargo vendor", response));
  const key = response.headers.get("x-nanocodex-key");
  const sha256 = response.headers.get("x-nanocodex-sha256");
  const size = Number(response.headers.get("content-length"));
  if (
    key !== `cargo-vendor/${cargoLockBlob}/${bundleSha256}/bundle.tar.gz` ||
    response.headers.get("x-nanocodex-cargo-lock") !== cargoLockBlob ||
    sha256 !== bundleSha256 ||
    !Number.isSafeInteger(size) || size <= 0 || size > maximumCargoVendorBytes
  ) throw new Error("read CI Cargo vendor returned invalid metadata");
  return { key, size, sha256 };
}

export async function readRustSec(origin, token, revision) {
  const response = await authenticatedFetch(
    `${origin}/api/ci/rustsec-advisory-db/${revision}/bundle.tar.gz`,
    token,
    { method: "HEAD" },
  );
  if (response.status === 404) {
    throw new Error(
      `CI RustSec advisory database ${revision.slice(0, 7)} is not published; ` +
      "run npm run publish:ci-rustsec --prefix web first",
    );
  }
  if (!response.ok) throw new Error(await responseError("read CI RustSec advisory database", response));
  const key = response.headers.get("x-nanocodex-key");
  const sha256 = response.headers.get("x-nanocodex-sha256");
  const size = Number(response.headers.get("content-length"));
  if (
    key !== `rustsec-advisory-db/${revision}/bundle.tar.gz` ||
    response.headers.get("x-nanocodex-revision") !== revision ||
    typeof sha256 !== "string" || !/^[a-f0-9]{64}$/.test(sha256) ||
    !Number.isSafeInteger(size) || size <= 0 || size > 16 * 1024 * 1024
  ) throw new Error("read CI RustSec advisory database returned invalid metadata");
  return { key, size, sha256 };
}

async function assertRepositorySnapshot(repository, expected, target) {
  const observed = target.type === "master"
    ? await assertMasterPublicationAuthority(repository, expected.head)
    : await readRepositorySnapshot(repository, target);
  if (observed.head !== expected.head || observed.ref !== expected.ref) {
    throw new Error("repository changed while its CI source publication was being built");
  }
}

async function requireCleanPullRequestState(repository) {
  const status = await git(repository, [
    "status",
    "--porcelain=v1",
    "--untracked-files=all",
    "--ignore-submodules=none",
  ]);
  if (status !== "") {
    throw new Error(
      `pull-request CI source publication requires a clean index and worktree:\n${status.slice(0, 1_000)}`,
    );
  }
}

async function describeArtifact(path) {
  const size = (await stat(path)).size;
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return { path, size, sha256: hash.digest("hex") };
}

function artifactRecord(key, artifact) {
  return { key, size: artifact.size, sha256: artifact.sha256 };
}

async function uploadFile(origin, token, remote, artifact, contentType) {
  const headers = {
    "content-length": String(artifact.size),
    "content-type": contentType,
    "x-nanocodex-sha256": artifact.sha256,
  };
  const body = Readable.toWeb(createReadStream(artifact.path));
  const response = await authenticatedFetch(
    `${origin}/api/ci/source/objects/${remote}`,
    token,
    { method: "PUT", headers, body, duplex: "half" },
  );
  if (!response.ok) {
    throw new Error(await responseError(`upload ${remote}`, response));
  }
}

function authenticatedFetch(url, token, init = {}) {
  const headers = new Headers(init.headers);
  headers.set("authorization", `Bearer ${token}`);
  return fetch(url, { ...init, headers });
}

async function responseError(operation, response) {
  const detail = await boundedResponseText(response, maximumErrorBytes);
  return `${operation} failed with HTTP ${response.status}${detail ? `: ${detail}` : ""}`;
}

async function boundedResponseText(response, maximumBytes) {
  if (response.body == null) return "";
  const reader = response.body.getReader();
  const chunks = [];
  let bytes = 0;
  let truncated = false;
  try {
    while (bytes <= maximumBytes) {
      const { done, value } = await reader.read();
      if (done) break;
      chunks.push(Buffer.from(value));
      bytes += value.byteLength;
      if (bytes > maximumBytes) {
        truncated = true;
        await reader.cancel();
        break;
      }
    }
  } finally {
    reader.releaseLock();
  }
  const text = Buffer.concat(chunks, bytes).subarray(0, maximumBytes).toString("utf8");
  return truncated ? `${text}…` : text;
}

async function git(repository, args) {
  return (await gitBuffer(repository, args)).toString("utf8").trimEnd();
}

async function gitBuffer(repository, args) {
  try {
    const { stdout } = await execFileAsync("git", [
      "-c", "core.fsmonitor=false",
      "-c", "core.hooksPath=/dev/null",
      "-c", "core.attributesFile=/dev/null",
      "-c", "core.autocrlf=false",
      "-c", "credential.helper=",
      ...args,
    ], {
      cwd: repository,
      env: publisherEnvironment(repository),
      encoding: "buffer",
      maxBuffer: maximumGitOutputBytes,
    });
    return stdout;
  } catch (error) {
    const detail = Buffer.isBuffer(error?.stderr)
      ? error.stderr.toString("utf8")
      : String(error?.stderr ?? error?.message ?? error);
    const wrapped = new Error(`git ${args[0]} failed: ${detail.trim().slice(0, 1_000)}`, {
      cause: error,
    });
    wrapped.exitCode = error?.code;
    throw wrapped;
  }
}

async function rejectObjectSubstitution(repository) {
  const replacements = await git(repository, [
    "for-each-ref",
    "--format=%(refname)",
    "refs/replace/",
  ]);
  if (replacements !== "") {
    throw new Error("CI source publication rejects Git replacement objects");
  }
  const grafts = await git(repository, ["rev-parse", "--git-path", "info/grafts"]);
  const graftPath = resolve(repository, grafts);
  const graft = await lstat(graftPath).catch((error) => {
    if (error?.code === "ENOENT") return undefined;
    throw error;
  });
  if (graft != null) {
    throw new Error("CI source publication rejects Git grafts");
  }
}

async function assertSafeLocalGitConfig(repository, target) {
  const names = [
    ...(await readConfigScope(repository, "--local")),
    ...(await hasWorktreeConfig(repository)
      ? readConfigScope(repository, "--worktree")
      : []),
  ];
  const safe = /^(?:core\.(?:repositoryformatversion|filemode|bare|logallrefupdates|ignorecase|precomposeunicode|autocrlf)|user\.(?:name|email)|extensions\.worktreeconfig|remote\.[^.]+\.(?:url|fetch)|branch\.[^.]+\.(?:remote|merge)|submodule\..+\.url|worktrunk\..+)$/i;
  const unsafe = names.filter((name) => !safe.test(name));
  if (target.type === "pull_request") {
    unsafe.push(...names.filter((name) => /^submodule\./i.test(name)));
  }
  if (unsafe.length > 0) {
    throw new Error(`CI source publication rejects local Git configuration: ${unsafe.join(", ")}`);
  }
}

async function readConfigScope(repository, scope) {
  return (await git(repository, [
    "config",
    scope,
    "--no-includes",
    "--name-only",
    "--list",
  ])).split("\n").filter(Boolean);
}

async function hasWorktreeConfig(repository) {
  try {
    return await git(repository, [
      "config",
      "--local",
      "--no-includes",
      "--bool",
      "--get",
      "extensions.worktreeConfig",
    ]) === "true";
  } catch (error) {
    if (error?.exitCode === 1) return false;
    throw error;
  }
}

async function readScopedConfigValues(repository, name) {
  const scopes = ["--local", ...(await hasWorktreeConfig(repository) ? ["--worktree"] : [])];
  const values = [];
  for (const scope of scopes) {
    try {
      values.push(...(await git(repository, [
        "config",
        scope,
        "--no-includes",
        "--get-all",
        name,
      ])).split("\n"));
    } catch (error) {
      if (error?.exitCode !== 1) throw error;
    }
  }
  return values.filter((value) => value !== "");
}

async function readJsonResponse(response, operation) {
  const text = await boundedResponseText(response, maximumStateBytes);
  try {
    return JSON.parse(text);
  } catch {
    throw new Error(`${operation} returned invalid JSON`);
  }
}

function publisherEnvironment(repository) {
  return {
    PATH: process.env.PATH ?? "",
    TMPDIR: process.env.TMPDIR ?? tmpdir(),
    LANG: process.env.LANG ?? "C.UTF-8",
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_NO_REPLACE_OBJECTS: "1",
    GIT_OPTIONAL_LOCKS: "0",
    GIT_TERMINAL_PROMPT: "0",
    GIT_WORK_TREE: repository,
  };
}

function deterministicGzip(body) {
  const header = Buffer.from([0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x02, 0xff]);
  const trailer = Buffer.alloc(8);
  trailer.writeUInt32LE(crc32(body), 0);
  trailer.writeUInt32LE(body.byteLength >>> 0, 4);
  return Buffer.concat([header, deflateRawSync(body, { level: 9 }), trailer]);
}

function crc32(body) {
  let crc = 0xffffffff;
  for (const byte of body) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) {
      crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1));
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
}

export function parseOrigin(value) {
  let origin;
  try {
    const url = new URL(value);
    if (!["http:", "https:"].includes(url.protocol) || url.username || url.password) {
      throw new Error("unsupported URL");
    }
    const loopback = url.hostname === "localhost" || url.hostname.endsWith(".localhost") ||
      url.hostname === "127.0.0.1" || url.hostname === "[::1]";
    if (url.protocol !== "https:" && !loopback) throw new Error("HTTPS is required");
    url.pathname = url.pathname.replace(/\/+$/, "");
    url.search = "";
    url.hash = "";
    origin = url.toString().replace(/\/$/, "");
  } catch (error) {
    throw new Error(
      "NANOCODEX_CI_ORIGIN must use HTTPS (HTTP is allowed only for loopback development)",
      { cause: error },
    );
  }
  return origin;
}

function requiredEnvironment(env, name) {
  const value = env[name]?.trim();
  if (!value) throw new Error(`${name} is required`);
  return value;
}

function requiredSha1(env, name) {
  const value = requiredEnvironment(env, name);
  if (!sha1Pattern.test(value)) {
    throw new Error(`${name} must be a full lowercase Git SHA-1`);
  }
  return value;
}

if (resolve(process.argv[1] ?? "") === scriptPath) {
  await main();
}
