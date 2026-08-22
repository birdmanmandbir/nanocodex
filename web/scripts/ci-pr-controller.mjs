import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import {
  lstat,
  mkdir,
  mkdtemp,
  open,
  readFile,
  realpath,
  rename,
  rm,
  unlink,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import {
  basename,
  dirname,
  isAbsolute,
  relative,
  resolve,
  sep,
} from "node:path";
import { fileURLToPath } from "node:url";

import {
  acquireExclusiveLock,
  assertUploaderDescriptor,
  captureCargoVendorArtifact,
  cargoBuilderEnvironment,
} from "./ci-controller.mjs";

const scriptPath = fileURLToPath(import.meta.url);
const webDirectory = resolve(dirname(scriptPath), "..");
const repositoryDirectory = resolve(webDirectory, "..");
const scriptsDirectory = resolve(webDirectory, "scripts");
const repositorySlug = "gakonst/nanocodex";
const authoritativeRepositoryUrl =
  "https://github.com/gakonst/nanocodex.git";
export const cargoPrepHelperPath =
  "/Library/PrivilegedHelperTools/dev.nanocodex.ci-pr-cargo-builder";
const sudoPath = "/usr/bin/sudo";
const cargoPrepHelperVersion = "2026-08-22.1";
const githubApiOrigin = "https://api.github.com";
const sha1Pattern = /^[a-f0-9]{40}$/;
const sha256Pattern = /^[a-f0-9]{64}$/;
const closeIdPattern =
  /^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;
const maximumJsonBytes = 1024 * 1024;
const maximumProcessOutputBytes = 16 * 1024 * 1024;
const maximumErrorBytes = 2_000;
const openPullRequestPageSize = 100;
const maximumOpenPullRequestPages = 100;
const githubMutationIntervalMs = 1_000;
const minimumGitHubRateLimitDelayMs = 60_000;
const processTerminationGraceMs = 1_250;
const processKillWaitMs = 5_000;
const processGroupPollMs = 25;
const githubStatusContext = "ci success";
const workflowStates = new Set([
  "queued",
  "running",
  "paused",
  "waiting",
  "unknown",
  "complete",
  "errored",
  "terminated",
]);
const controllerStatuses = new Set([
  "pending",
  "success",
  "failure",
  "error",
]);
const runtimeEnvironmentNames = [
  "PATH",
  "HOME",
  "TMPDIR",
  "TMP",
  "TEMP",
  "LANG",
  "LC_ALL",
  "LC_CTYPE",
  "TZ",
  "SSL_CERT_FILE",
  "SSL_CERT_DIR",
  "NODE_EXTRA_CA_CERTS",
  "CARGO_HOME",
  "RUSTUP_HOME",
];
const forbiddenPromotionAuthorities = [
  "AWS_ACCESS_KEY_ID",
  "AWS_SECRET_ACCESS_KEY",
  "AWS_SESSION_TOKEN",
  "CARGO_REGISTRY_TOKEN",
  "CF_API_TOKEN",
  "CI_CONTROL_TOKEN",
  "CI_MASTER_SOURCE_WRITE_TOKEN",
  "CI_MACOS_RUNNER_TOKEN",
  "CI_PR_SOURCE_WRITE_TOKEN",
  "CI_RELEASE_TOKEN",
  "NANOCODEX_SANDBOX_CONTROL_TOKEN",
  "CLOUDFLARE_API_TOKEN",
  "CLOUDFLARE_API_KEY",
  "GH_ENTERPRISE_TOKEN",
  "GH_TOKEN",
  "GITHUB_TOKEN",
  "GITHUB_ENTERPRISE_TOKEN",
  "GIT_MIRROR_TOKEN",
  "NANOCODEX_GIT_MIRROR_TOKEN",
  "NANOCODEX_GIT_TOKEN",
  "NANOCODEX_CI_CONTROL_TOKEN",
  "NANOCODEX_CI_MACOS_RUNNER_TOKEN",
  "NANOCODEX_RELEASE_TOKEN",
  "NODE_AUTH_TOKEN",
  "NPM_TOKEN",
  "R2_ACCESS_KEY_ID",
  "R2_SECRET_ACCESS_KEY",
];

export class GitHubRateLimitError extends Error {
  constructor(retryAt, operation) {
    super(operation + " reached the GitHub API rate limit");
    this.name = "GitHubRateLimitError";
    this.retryAt = retryAt;
  }
}

export class StalePullRequestIdentityError extends Error {
  constructor(identity, phase = "authoritative PR identity check") {
    const parsed = parseMonitoredPullRequestIdentity(identity);
    super(
      "pull request " +
        parsed.number +
        " is no longer the authoritative identity during " +
        phase,
    );
    this.name = "StalePullRequestIdentityError";
    this.phase = phase;
    this.identity = parsed;
  }
}

export function parseArguments(args) {
  let once = false;
  let help = false;
  for (const argument of args) {
    if (argument === "--once") {
      if (once) throw new Error("--once may be supplied only once");
      once = true;
    } else if (argument === "--help" || argument === "-h") {
      help = true;
    } else {
      throw new Error("unknown argument: " + argument);
    }
  }
  return { once, help };
}

export function assertNoPromotionAuthorities(env) {
  const present = forbiddenPromotionAuthorities.filter((name) =>
    typeof env[name] === "string" && env[name].trim() !== ""
  );
  if (present.length > 0) {
    throw new Error(
      "PR CI controller refuses promotion or ambient GitHub authorities: " +
        present.join(", "),
    );
  }
}

export function runtimeEnvironment(env = process.env) {
  const child = {};
  for (const name of runtimeEnvironmentNames) {
    if (typeof env[name] === "string" && env[name] !== "") {
      child[name] = env[name];
    }
  }
  child.PATH ??= "/usr/local/bin:/usr/bin:/bin";
  child.TMPDIR ??= tmpdir();
  child.LANG ??= "C.UTF-8";
  return {
    ...child,
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_NO_REPLACE_OBJECTS: "1",
    GIT_OPTIONAL_LOCKS: "0",
    GIT_TERMINAL_PROMPT: "0",
  };
}

export function publicationEnvironment(
  env,
  stage,
  {
    cargoVendorSha256,
    repository,
    rustSecRevision,
    pullRequest,
  } = {},
) {
  if (stage !== "cargo-vendor" && stage !== "source") {
    throw new Error("unsupported PR publication stage: " + stage);
  }
  const child = {
    ...runtimeEnvironment(env),
    NANOCODEX_CI_ORIGIN: requiredEnvironment(env, "NANOCODEX_CI_ORIGIN"),
    NANOCODEX_CI_TOKEN: requiredEnvironment(env, "NANOCODEX_CI_TOKEN"),
  };
  if (stage === "cargo-vendor") {
    delete child.CARGO_HOME;
    delete child.RUSTUP_HOME;
  }
  if (stage === "source") {
    if (typeof repository !== "string" || repository === "") {
      throw new Error("PR source publication repository is required");
    }
    if (typeof cargoVendorSha256 !== "string" || !sha256Pattern.test(cargoVendorSha256)) {
      throw new Error("PR Cargo vendor bundle must be a lowercase SHA-256");
    }
    assertSha1(rustSecRevision, "RustSec revision");
    const parsed = parsePullRequestIdentity(pullRequest);
    child.NANOCODEX_REPO = repository;
    child.NANOCODEX_RUSTSEC_REVISION = rustSecRevision;
    child.NANOCODEX_CI_PULL_REQUEST_NUMBER = String(parsed.number);
    child.NANOCODEX_CI_PULL_REQUEST_HEAD = parsed.pullRequestHead;
    child.NANOCODEX_CI_CARGO_VENDOR_SHA256 = cargoVendorSha256;
  }
  return child;
}

export function parsePrepUsername(value) {
  if (typeof value !== "string" || !/^[a-z_][a-z0-9_-]{0,30}$/.test(value)) {
    throw new Error("NANOCODEX_CI_PR_PREP_USER must be a strict POSIX username");
  }
  return value;
}

export function prepHelperArguments(node, prepUser, mode) {
  if (typeof node !== "string" || !isAbsolute(node)) {
    throw new Error("PR preparation Node must be canonical and absolute");
  }
  parsePrepUsername(prepUser);
  if (!["--probe", "--build"].includes(mode)) {
    throw new Error("unsupported PR preparation helper mode");
  }
  return ["-n", "-u", prepUser, "--", node, cargoPrepHelperPath, mode];
}

export function parsePrepProbe(stdout, controllerUid = process.getuid?.()) {
  if (
    typeof stdout !== "string" || !stdout.endsWith("\n") ||
    stdout.indexOf("\n") !== stdout.length - 1 || stdout.length > 4_096
  ) throw new Error("PR preparation helper returned invalid probe stdout");
  let value;
  try {
    value = JSON.parse(stdout.slice(0, -1));
  } catch {
    throw new Error("PR preparation helper returned invalid probe JSON");
  }
  const expectedKeys = [
    "credentialEnvironmentNames", "freshHomePolicy", "gid", "helperVersion", "uid", "version",
  ];
  if (
    !record(value) || JSON.stringify(value) !== stdout.slice(0, -1) ||
    Object.keys(value).sort().join("\0") !== expectedKeys.join("\0") ||
    value.version !== 1 || value.helperVersion !== cargoPrepHelperVersion ||
    value.freshHomePolicy !== "per-build-private-temporary" ||
    !Array.isArray(value.credentialEnvironmentNames) ||
    value.credentialEnvironmentNames.length !== 0 ||
    !Number.isSafeInteger(value.uid) || value.uid <= 0 ||
    !Number.isSafeInteger(value.gid) || value.gid <= 0 ||
    (Number.isSafeInteger(controllerUid) && value.uid === controllerUid)
  ) throw new Error("PR preparation helper returned an invalid credential boundary probe");
  return value;
}

export function assertQuietProcessSuccess(result, operation) {
  if (
    !record(result) ||
    typeof result.stdout !== "string" ||
    typeof result.stderr !== "string" ||
    typeof operation !== "string" ||
    operation === ""
  ) {
    throw new TypeError("successful helper output and operation are required");
  }
  if (result.stderr !== "") {
    throw new Error("successful " + operation + " emitted stderr");
  }
  return result;
}

export function trustedPublisherPath(stage) {
  if (stage === "cargo-vendor") {
    return resolve(scriptsDirectory, "publish-ci-cargo-vendor.mjs");
  }
  if (stage === "source") {
    return resolve(scriptsDirectory, "publish-ci-source.mjs");
  }
  throw new Error("unsupported PR publication stage: " + stage);
}

export function parseOpenPullRequestPage(value) {
  if (!Array.isArray(value) || value.length > openPullRequestPageSize) {
    throw new Error("GitHub returned an invalid open pull-request page");
  }
  return value.map((candidate) => {
    const pullRequest = parseGitHubPullRequest(candidate);
    if (pullRequest.state !== "open" || !pullRequest.basedOnMaster) {
      throw new Error("GitHub returned a pull request outside the master lane");
    }
    return {
      number: pullRequest.number,
      pullRequestHead: pullRequest.pullRequestHead,
      mergeHead: pullRequest.mergeHead,
    };
  });
}

export function parseGitHubPullRequest(value, expectedNumber) {
  if (!record(value)) {
    throw new Error("GitHub returned an invalid pull request");
  }
  const number = value.number;
  const head = value.head;
  const base = value.base;
  if (
    !Number.isSafeInteger(number) ||
    number <= 0 ||
    (expectedNumber != null && number !== expectedNumber) ||
    !["open", "closed"].includes(value.state) ||
    !record(head) ||
    !isSha1(head.sha) ||
    !record(base) ||
    typeof base.ref !== "string" ||
    !record(base.repo) ||
    typeof base.repo.full_name !== "string" ||
    (value.merge_commit_sha != null && !isSha1(value.merge_commit_sha))
  ) {
    throw new Error("GitHub returned an invalid pull request");
  }
  return {
    number,
    state: value.state,
    basedOnMaster:
      base.ref === "master" && base.repo.full_name === repositorySlug,
    pullRequestHead: head.sha,
    mergeHead: value.merge_commit_sha ?? null,
  };
}

export function parseGitHubNextPage(value, currentPage) {
  if (value == null || value === "") return null;
  if (
    typeof value !== "string" ||
    !Number.isSafeInteger(currentPage) ||
    currentPage <= 0
  ) {
    throw new Error("GitHub returned an invalid pagination link");
  }
  let next;
  for (const part of value.split(",")) {
    const target = /^\s*<([^<>]+)>/.exec(part)?.[1];
    const relations = /(?:^|;)\s*rel="([^"]+)"(?:;|$)/.exec(part)?.[1]
      ?.split(/\s+/);
    if (!target || !relations) {
      throw new Error("GitHub returned an invalid pagination link");
    }
    if (!relations.includes("next")) continue;
    if (next != null) {
      throw new Error("GitHub returned duplicate next-page links");
    }
    const url = new URL(target);
    const expected = {
      state: "open",
      base: "master",
      sort: "created",
      direction: "asc",
      per_page: String(openPullRequestPageSize),
      page: String(currentPage + 1),
    };
    if (
      url.origin !== githubApiOrigin ||
      url.pathname !== "/repos/" + repositorySlug + "/pulls" ||
      url.username !== "" ||
      url.password !== "" ||
      url.hash !== "" ||
      [...url.searchParams.keys()].length !== Object.keys(expected).length ||
      Object.entries(expected).some(
        ([name, expectedValue]) =>
          url.searchParams.getAll(name).length !== 1 ||
          url.searchParams.get(name) !== expectedValue,
      )
    ) {
      throw new Error("GitHub returned an unexpected next-page URL");
    }
    next = currentPage + 1;
  }
  return next ?? null;
}

export async function collectOpenPullRequests(
  fetchPage,
  { maximumPages = maximumOpenPullRequestPages } = {},
) {
  if (
    typeof fetchPage !== "function" ||
    !Number.isSafeInteger(maximumPages) ||
    maximumPages <= 0 ||
    maximumPages > maximumOpenPullRequestPages
  ) {
    throw new TypeError("a bounded GitHub page fetcher is required");
  }
  const pullRequests = [];
  const numbers = new Set();
  for (let page = 1; page <= maximumPages; page++) {
    const pageResult = await fetchPage(page);
    if (
      !record(pageResult) ||
      !Array.isArray(pageResult.items) ||
      (pageResult.nextPage !== null &&
        pageResult.nextPage !== page + 1)
    ) {
      throw new Error("GitHub returned invalid pagination metadata");
    }
    const values = parseOpenPullRequestPage(pageResult.items);
    for (const pullRequest of values) {
      if (numbers.has(pullRequest.number)) {
        throw new Error("GitHub pagination returned a duplicate pull request");
      }
      numbers.add(pullRequest.number);
      pullRequests.push(pullRequest);
    }
    if (pageResult.nextPage === null) {
      pullRequests.sort((left, right) => left.number - right.number);
      return pullRequests;
    }
  }
  throw new Error(
    "GitHub open pull-request pagination exceeded " + maximumPages + " pages",
  );
}

export function parsePullRequestLaneState(value, number) {
  assertPullRequestNumber(number);
  if (!record(value)) {
    throw new Error("Cloudflare returned an invalid pull-request lane");
  }
  if (value.error === "not_published") {
    return { type: "not_published", number };
  }
  if (value.error === "pull_request_closed") {
    if (
      value.number !== number ||
      !closeIdPattern.test(String(value.closeId)) ||
      !isSha1(value.mergeHead) ||
      !isSha1(value.pullRequestHead) ||
      typeof value.closedAt !== "string" ||
      !Number.isFinite(Date.parse(value.closedAt))
    ) {
      throw new Error("Cloudflare returned an invalid PR close record");
    }
    return {
      type: "closed",
      number,
      closeId: value.closeId,
      mergeHead: value.mergeHead,
      pullRequestHead: value.pullRequestHead,
      closedAt: value.closedAt,
    };
  }
  const publication = value.publication;
  const run = value.run;
  if (
    !record(publication) ||
    publication.version !== 1 ||
    !isSha1(publication.head) ||
    publication.branch !== "pull/" + number + "/merge" ||
    publication.ref !== "refs/pull/" + number + "/merge" ||
    !record(publication.lane) ||
    publication.lane.type !== "pull_request" ||
    publication.lane.number !== number ||
    !isSha1(publication.lane.pullRequestHead) ||
    !record(run) ||
    run.version !== 1 ||
    run.head !== publication.head ||
    run.workflowId !== "ci-" + publication.head ||
    !["pending", "dispatched"].includes(run.state) ||
    typeof run.publishedAt !== "string" ||
    !Number.isFinite(Date.parse(run.publishedAt))
  ) {
    throw new Error("Cloudflare returned an invalid pull-request lane");
  }
  return {
    type: "open",
    number,
    mergeHead: publication.head,
    pullRequestHead: publication.lane.pullRequestHead,
    publication,
    run,
  };
}

export function parseMasterSourceState(value) {
  const publication = value?.publication;
  const run = value?.run;
  if (
    !record(value) ||
    !record(publication) ||
    publication.version !== 1 ||
    !isSha1(publication.head) ||
    publication.branch !== "master" ||
    publication.ref !== "refs/heads/master" ||
    (!record(publication.lane) || publication.lane.type !== "master") ||
    !isSha1(publication.rustSecRevision) ||
    !record(publication.rustSec) ||
    publication.rustSec.key !==
      "rustsec-advisory-db/" + publication.rustSecRevision + "/bundle.tar.gz" ||
    !Number.isSafeInteger(publication.rustSec.size) ||
    publication.rustSec.size <= 0 ||
    publication.rustSec.size > 16 * 1024 * 1024 ||
    !sha256Pattern.test(String(publication.rustSec.sha256)) ||
    typeof publication.publishedAt !== "string" ||
    !Number.isFinite(Date.parse(publication.publishedAt)) ||
    !record(run) ||
    run.version !== 1 ||
    run.head !== publication.head ||
    run.workflowId !== "ci-" + publication.head ||
    !["pending", "dispatched"].includes(run.state) ||
    typeof run.publishedAt !== "string" ||
    !Number.isFinite(Date.parse(run.publishedAt))
  ) {
    throw new Error("Cloudflare returned an invalid master source state");
  }
  return {
    head: publication.head,
    rustSecRevision: publication.rustSecRevision,
  };
}

export function parseCiRunState(value, expectedHead) {
  assertSha1(expectedHead, "expected merge head");
  if (
    !record(value) ||
    value.version !== 1 ||
    value.head !== expectedHead ||
    value.workflowId !== "ci-" + expectedHead ||
    !["pending", "dispatched"].includes(value.state) ||
    !record(value.workflow) ||
    typeof value.workflow.status !== "string" ||
    !workflowStates.has(value.workflow.status)
  ) {
    throw new Error("Cloudflare returned an invalid merge run");
  }
  const workflowStatus = value.workflow.status;
  const result = value.result;
  let resultStatus = null;
  if (result != null) {
    if (
      !record(result) ||
      result.version !== 1 ||
      result.head !== expectedHead ||
      result.workflowId !== "ci-" + expectedHead ||
      !["running", "success", "failure", "terminated"].includes(result.status)
    ) {
      throw new Error("Cloudflare returned invalid merge-run evidence");
    }
    resultStatus = result.status;
  }
  if (workflowStatus === "complete") {
    if (resultStatus !== "success") {
      throw new Error("completed merge run has no successful result");
    }
    return { outcome: "success", workflowStatus, resultStatus };
  }
  if (workflowStatus === "errored") {
    return { outcome: "failure", workflowStatus, resultStatus };
  }
  if (workflowStatus === "terminated") {
    return { outcome: "error", workflowStatus, resultStatus };
  }
  return { outcome: "pending", workflowStatus, resultStatus };
}

export function pullRequestStatusPayload(
  state,
  pullRequestHead,
  mergeHead,
  origin,
) {
  if (!controllerStatuses.has(state)) {
    throw new Error("unsupported PR status: " + state);
  }
  assertSha1(pullRequestHead, "PR status head");
  assertSha1(mergeHead, "PR merge head");
  const descriptions = {
    pending: "Cloudflare CI is running for this PR",
    success: "Cloudflare CI passed for this PR",
    failure: "Cloudflare CI failed for this PR",
    error: "Trusted PR CI controller failed",
  };
  return {
    state,
    context: githubStatusContext,
    description: descriptions[state],
    target_url: new URL(
      "/api/ci/runs/" + mergeHead,
      parseOrigin(origin),
    ).href,
  };
}

export function newCloseId() {
  const closeId = randomUUID();
  if (!closeIdPattern.test(closeId)) {
    throw new Error("runtime returned an invalid close UUID");
  }
  return closeId;
}

export function parseControllerState(value) {
  if (
    !record(value) ||
    value.version !== 1 ||
    value.repository !== repositorySlug ||
    !Array.isArray(value.pullRequests)
  ) {
    throw new Error("PR controller state is invalid");
  }
  const entries = [];
  let previous = 0;
  for (const candidate of value.pullRequests) {
    if (
      !record(candidate) ||
      !Number.isSafeInteger(candidate.number) ||
      candidate.number <= previous ||
      !isSha1(candidate.pullRequestHead) ||
      !isSha1(candidate.mergeHead) ||
      (candidate.status !== null &&
        !controllerStatuses.has(candidate.status)) ||
      (candidate.previousLane != null &&
        (!record(candidate.previousLane) ||
          !isSha1(candidate.previousLane.pullRequestHead) ||
          !isSha1(candidate.previousLane.mergeHead)))
    ) {
      throw new Error("PR controller state contains an invalid lane");
    }
    previous = candidate.number;
    entries.push({
      number: candidate.number,
      pullRequestHead: candidate.pullRequestHead,
      mergeHead: candidate.mergeHead,
      status: candidate.status,
      ...(candidate.previousLane == null
        ? {}
        : {
            previousLane: {
              pullRequestHead: candidate.previousLane.pullRequestHead,
              mergeHead: candidate.previousLane.mergeHead,
            },
          }),
    });
  }
  return entries;
}

export async function runPullRequestControllerCycle(
  operations,
  { signal } = {},
) {
  const previousEntries = await operations.loadState(signal);
  await operations.readMasterSource(signal);
  const previous = new Map(
    previousEntries.map((entry) => [entry.number, entry]),
  );
  const pullRequests = await operations.listOpenPullRequests(signal);
  const openNumbers = new Set(pullRequests.map(({ number }) => number));
  const next = new Map(previous);
  const reports = [];
  const errors = [];

  for (const pullRequest of pullRequests) {
    throwIfAborted(signal);
    const prior = previous.get(pullRequest.number);
    let identity = {
      number: pullRequest.number,
      pullRequestHead: pullRequest.pullRequestHead,
      mergeHead: pullRequest.mergeHead,
    };
    let reportedStatus =
      sameIdentity(prior, identity) ? prior.status : undefined;
    let checkout;
    try {
      checkout = await operations.preparePullRequest(pullRequest, signal);
      identity = {
        number: pullRequest.number,
        pullRequestHead: checkout.pullRequestHead,
        mergeHead: checkout.mergeHead,
      };
      const masterSource = await operations.readMasterSource(signal);
      if (checkout.baseHead !== masterSource.head) {
        throw new StalePullRequestIdentityError(
          checkout,
          "matching the merge base to the current published master",
        );
      }
      reportedStatus = sameIdentity(prior, identity)
        ? prior.status
        : undefined;
      let lane = await operations.readLane(pullRequest.number, signal);
      const previousLane =
        lane.type === "open" && !laneMatchesIdentity(lane, identity)
          ? {
              pullRequestHead: lane.pullRequestHead,
              mergeHead: lane.mergeHead,
            }
          : undefined;
      next.set(pullRequest.number, {
        ...identity,
        status: reportedStatus ?? null,
        ...(previousLane == null ? {} : { previousLane }),
      });
      await operations.saveState(sortedControllerEntries(next));
      if (!laneMatchesIdentity(lane, identity)) {
        if (reportedStatus !== "pending" || !sameIdentity(prior, identity)) {
          await operations.updateStatus(identity, "pending", signal, checkout);
          reportedStatus = "pending";
        }
        const cargoVendor = await operations.publishCargoVendor(checkout, signal);
        if (!record(cargoVendor) || !sha256Pattern.test(cargoVendor.sha256)) {
          throw new Error("PR Cargo builder returned an invalid descriptor");
        }
        await operations.assertFresh(pullRequest, checkout, signal);
        await operations.publishSource(
          checkout,
          masterSource.rustSecRevision,
          cargoVendor.sha256,
          signal,
        );
        await operations.assertFresh(pullRequest, checkout, signal);
        lane = await operations.readLane(pullRequest.number, signal);
        if (!laneMatchesIdentity(lane, identity)) {
          throw new Error("published PR lane is not the authoritative merge");
        }
      } else {
        await operations.assertFresh(pullRequest, checkout, signal);
      }
      if (reportedStatus == null) {
        await operations.updateStatus(identity, "pending", signal, checkout);
        reportedStatus = "pending";
      }
      const run = await operations.readRun(identity.mergeHead, signal);
      const desiredStatus = run.outcome;
      if (!controllerStatuses.has(desiredStatus)) {
        throw new Error("PR run returned an unsupported status");
      }
      if (desiredStatus !== reportedStatus) {
        if (desiredStatus !== "pending") {
          await operations.assertFresh(pullRequest, checkout, signal);
        }
        await operations.updateStatus(
          identity,
          desiredStatus,
          signal,
          checkout,
        );
        reportedStatus = desiredStatus;
      }
      next.set(pullRequest.number, {
        ...identity,
        status: reportedStatus,
      });
      reports.push({
        number: pullRequest.number,
        pullRequestHead: identity.pullRequestHead,
        mergeHead: identity.mergeHead,
        status: reportedStatus,
      });
    } catch (cause) {
      if (isAbort(cause) || cause instanceof GitHubRateLimitError) throw cause;
      if (cause instanceof StalePullRequestIdentityError) {
        errors.push(cause);
        continue;
      }
      if (!isSha1(identity.mergeHead)) {
        errors.push(cause);
        continue;
      }
      const desiredStatus = cause?.githubState === "failure"
        ? "failure"
        : "error";
      let statusFailed = false;
      try {
        if (desiredStatus !== reportedStatus) {
          if (checkout == null) {
            throw new Error(
              "cannot publish a terminal PR status without a verified checkout",
            );
          }
          await operations.assertFresh(pullRequest, checkout, signal);
          await operations.updateStatus(
            identity,
            desiredStatus,
            signal,
            checkout,
          );
          reportedStatus = desiredStatus;
        }
      } catch (statusCause) {
        if (
          isAbort(statusCause) ||
          statusCause instanceof GitHubRateLimitError
        ) {
          throw statusCause;
        }
        statusFailed = true;
        errors.push(
          new AggregateError(
            [cause, statusCause],
            "PR " + pullRequest.number + " failed and its status update failed",
          ),
        );
      }
      if (!statusFailed) errors.push(cause);
      const tracked = next.get(pullRequest.number);
      next.set(pullRequest.number, {
        ...identity,
        status: reportedStatus ?? null,
        ...(tracked?.previousLane == null
          ? {}
          : { previousLane: tracked.previousLane }),
      });
      reports.push({
        number: pullRequest.number,
        pullRequestHead: identity.pullRequestHead,
        mergeHead: identity.mergeHead,
        status: reportedStatus ?? desiredStatus,
      });
    } finally {
      await checkout?.cleanup();
    }
  }

  for (const entry of [...previous.values()].sort(
    (left, right) => left.number - right.number,
  )) {
    if (openNumbers.has(entry.number)) continue;
    throwIfAborted(signal);
    try {
      const closed = await operations.closeLane(entry, signal);
      if (!closed) {
        continue;
      }
      next.delete(entry.number);
      reports.push({
        number: entry.number,
        pullRequestHead: entry.pullRequestHead,
        mergeHead: entry.mergeHead,
        status: "closed",
      });
    } catch (cause) {
      if (isAbort(cause) || cause instanceof GitHubRateLimitError) throw cause;
      next.set(entry.number, entry);
      errors.push(cause);
    }
  }

  const nextEntries = sortedControllerEntries(next);
  await operations.saveState(nextEntries, signal);
  return { reports, errors };
}

export async function publishPreparedPullRequestCargoVendor(
  operations,
  checkout,
  { signal } = {},
) {
  const identity = parsePullRequestIdentity(checkout);
  assertSha1(checkout.baseHead, "PR Cargo base head");
  assertSha1(checkout.mergeHead, "PR Cargo merge head");
  if (typeof operations?.probe !== "function" || typeof operations.build !== "function" ||
      typeof operations.assertFresh !== "function" || typeof operations.upload !== "function") {
    throw new TypeError("PR Cargo publication requires probe, build, freshness, and upload phases");
  }
  await operations.probe(signal);
  const artifact = await operations.build(signal);
  try {
    const descriptor = artifact?.descriptor;
    if (
      !record(descriptor) || descriptor.number !== identity.number ||
      descriptor.baseHead !== checkout.baseHead ||
      descriptor.pullRequestHead !== identity.pullRequestHead ||
      descriptor.mergeHead !== checkout.mergeHead ||
      !sha1Pattern.test(descriptor.cargoLockBlob) ||
      !sha256Pattern.test(descriptor.sha256) ||
      !Number.isSafeInteger(descriptor.size) || descriptor.size <= 0 ||
      descriptor.key !==
        `cargo-vendor/${descriptor.cargoLockBlob}/${descriptor.sha256}/bundle.tar.gz`
    ) throw new Error("PR Cargo builder returned a different authoritative identity");
    await operations.assertFresh(signal);
    await operations.upload(artifact, signal);
    await operations.assertFresh(signal);
    return descriptor;
  } finally {
    await artifact?.cleanup?.();
  }
}

export function assertAuthoritativePullRequestIdentity(
  identity,
  evidence,
  phase = "authoritative PR identity check",
) {
  const expected = parseMonitoredPullRequestIdentity(identity);
  if (
    !record(evidence) ||
    !record(evidence.pullRequest) ||
    evidence.publishedMasterHead !== expected.baseHead ||
    evidence.pullRequest.number !== expected.number ||
    evidence.pullRequest.state !== "open" ||
    evidence.pullRequest.basedOnMaster !== true ||
    evidence.pullRequest.pullRequestHead !== expected.pullRequestHead ||
    evidence.pullRequest.mergeHead !== expected.mergeHead ||
    evidence.masterRef !== expected.baseHead ||
    evidence.pullRequestHeadRef !== expected.pullRequestHead ||
    evidence.mergeRef !== expected.mergeHead
  ) {
    throw new StalePullRequestIdentityError(expected, phase);
  }
  return expected;
}

export async function runWhilePullRequestIsCurrent(
  identity,
  operation,
  {
    assertCurrent,
    phase = "long-running pull-request operation",
    pollMs,
    signal,
  } = {},
) {
  const expected = parseMonitoredPullRequestIdentity(identity);
  if (typeof operation !== "function" || typeof assertCurrent !== "function") {
    throw new TypeError(
      "monitored operation and authoritative pull-request checker are required",
    );
  }
  if (!Number.isSafeInteger(pollMs) || pollMs < 1) {
    throw new Error(
      "authoritative pull-request monitor interval must be a positive integer",
    );
  }
  throwIfAborted(signal);
  await assertCurrent(expected, signal, phase);

  const monitorController = new AbortController();
  const combinedSignal = signal
    ? AbortSignal.any([signal, monitorController.signal])
    : monitorController.signal;
  const work = Promise.resolve().then(() => operation(combinedSignal));
  const monitor = (async () => {
    while (true) {
      await abortableDelay(pollMs, combinedSignal);
      await assertCurrent(expected, combinedSignal, phase);
    }
  })();
  try {
    return await Promise.race([work, monitor]);
  } finally {
    monitorController.abort(
      new DOMException("pull-request monitor completed", "AbortError"),
    );
    await Promise.allSettled([work, monitor]);
  }
}

export function controllerConfiguration(env = process.env) {
  assertNoPromotionAuthorities(env);
  const ciOrigin = parseOrigin(
    requiredEnvironment(env, "NANOCODEX_CI_ORIGIN"),
  );
  const statePath = resolve(
    requiredEnvironment(env, "NANOCODEX_CI_PR_STATE_PATH"),
  );
  const scratchRoot = resolve(env.NANOCODEX_CI_PR_TMPDIR ?? tmpdir());
  if (
    pathContains(repositoryDirectory, statePath) ||
    pathContains(repositoryDirectory, scratchRoot)
  ) {
    throw new Error(
      "PR controller state and scratch paths must be outside the trusted repository",
    );
  }
  const githubToken = requiredEnvironment(
    env,
    "NANOCODEX_GITHUB_STATUS_TOKEN",
  );
  const sourceToken = requiredEnvironment(env, "NANOCODEX_CI_TOKEN");
  const prepUser = parsePrepUsername(
    requiredEnvironment(env, "NANOCODEX_CI_PR_PREP_USER"),
  );
  if (githubToken === sourceToken) {
    throw new Error("GitHub and source-write tokens must be distinct");
  }
  return {
    env,
    ciOrigin,
    statePath,
    scratchRoot,
    githubToken,
    sourceToken,
    prepUser,
    intervalMs: boundedIntegerEnvironment(
      env,
      "NANOCODEX_CI_PR_INTERVAL_MS",
      60_000,
      5_000,
      60 * 60 * 1_000,
    ),
    requestTimeoutMs: boundedIntegerEnvironment(
      env,
      "NANOCODEX_CI_PR_REQUEST_TIMEOUT_MS",
      20_000,
      1_000,
      120_000,
    ),
    commandTimeoutMs: boundedIntegerEnvironment(
      env,
      "NANOCODEX_CI_PR_COMMAND_TIMEOUT_MS",
      45 * 60 * 1_000,
      60_000,
      2 * 60 * 60 * 1_000,
    ),
    identityPollMs: boundedIntegerEnvironment(
      env,
      "NANOCODEX_CI_PR_IDENTITY_POLL_MS",
      5_000,
      250,
      60_000,
    ),
    maximumPages: boundedIntegerEnvironment(
      env,
      "NANOCODEX_CI_PR_MAX_PAGES",
      maximumOpenPullRequestPages,
      1,
      maximumOpenPullRequestPages,
    ),
  };
}

async function createOperations(config) {
  const canonicalNode = await realpath(process.execPath);
  const childEnvironment = {
    ...config.env,
    TMPDIR: config.scratchRoot,
    TMP: undefined,
    TEMP: undefined,
  };
  const secrets = [
    config.githubToken,
    config.sourceToken,
  ];
  const processOptions = {
    timeoutMs: config.commandTimeoutMs,
    secrets,
  };
  const runGit = (cwd, args, signal, options = {}) =>
    runProcess(
      "git",
      [
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.attributesFile=/dev/null",
        "-c",
        "core.autocrlf=false",
        ...args,
      ],
      {
        cwd,
        env: runtimeEnvironment(childEnvironment),
        ...processOptions,
        ...options,
        signal,
      },
    );

  const githubJson = async (path, init, signal, operation) => {
    const response = await boundedFetch(
      new URL(path, githubApiOrigin),
      {
        ...init,
        redirect: "error",
        headers: {
          accept: "application/vnd.github+json",
          authorization: "Bearer " + config.githubToken,
          "user-agent": "nanocodex-trusted-pr-ci-controller",
          "x-github-api-version": "2022-11-28",
          ...init?.headers,
        },
      },
      signal,
      config.requestTimeoutMs,
    );
    if (!response.ok) {
      await throwGitHubResponseFailure(operation, response);
    }
    const value = await readJsonResponse(response, maximumJsonBytes, operation);
    return {
      value,
      remaining: parseRateRemaining(response.headers.get("x-ratelimit-remaining")),
      retryAt: githubRateLimitRetryAt(response),
      link: response.headers.get("link"),
    };
  };

  const readGitHubPullRequest = async (number, signal) => {
    const { value } = await githubJson(
      "/repos/" + repositorySlug + "/pulls/" + number,
      undefined,
      signal,
      "read GitHub pull request " + number,
    );
    return parseGitHubPullRequest(value, number);
  };

  let nextGitHubMutationAt = 0;

  const preparePullRequest = async (pullRequest, signal) => {
    const root = await mkdtemp(
      resolve(
        config.scratchRoot,
        "nanocodex-ci-pr-" + pullRequest.number + "-",
      ),
    );
    const repository = resolve(root, "checkout");
    try {
      await runGit(root, ["init", "-q", repository], signal);
      await runGit(
        repository,
        ["remote", "add", "origin", authoritativeRepositoryUrl],
        signal,
      );
      await runGit(
        repository,
        [
          "fetch",
          "--quiet",
          "--force",
          "--atomic",
          "--depth=2",
          "--no-tags",
          "--no-recurse-submodules",
          "origin",
          "+refs/heads/master:refs/remotes/origin/master",
          "+refs/pull/" +
            pullRequest.number +
            "/head:refs/pull/" +
            pullRequest.number +
            "/head",
          "+refs/pull/" +
            pullRequest.number +
            "/merge:refs/pull/" +
            pullRequest.number +
            "/merge",
        ],
        signal,
      );
      const [headRef, mergeRef, baseRef, origin, parents] = await Promise.all([
        runGit(
          repository,
          [
            "rev-parse",
            "--verify",
            "refs/pull/" + pullRequest.number + "/head^{commit}",
          ],
          signal,
        ),
        runGit(
          repository,
          [
            "rev-parse",
            "--verify",
            "refs/pull/" + pullRequest.number + "/merge^{commit}",
          ],
          signal,
        ),
        runGit(
          repository,
          [
            "rev-parse",
            "--verify",
            "refs/remotes/origin/master^{commit}",
          ],
          signal,
        ),
        runGit(repository, ["remote", "get-url", "origin"], signal),
        runGit(
          repository,
          [
            "show",
            "--no-patch",
            "--format=%P",
            "refs/pull/" + pullRequest.number + "/merge",
          ],
          signal,
        ),
      ]);
      const pullRequestHead = headRef.stdout.trimEnd();
      const mergeHead = mergeRef.stdout.trimEnd();
      const baseHead = baseRef.stdout.trimEnd();
      const mergeParents = parents.stdout.trimEnd().split(" ").filter(Boolean);
      if (
        pullRequestHead !== pullRequest.pullRequestHead ||
        (pullRequest.mergeHead != null &&
          mergeHead !== pullRequest.mergeHead) ||
        origin.stdout.trimEnd() !== authoritativeRepositoryUrl ||
        mergeParents.length !== 2 ||
        mergeParents[0] !== baseHead ||
        mergeParents[1] !== pullRequestHead
      ) {
        throw new Error(
          "fetched pull-request refs do not describe the authoritative two-parent merge",
        );
      }
      await runGit(
        repository,
        [
          "checkout",
          "--quiet",
          "--detach",
          "--force",
          "refs/pull/" + pullRequest.number + "/merge",
        ],
        signal,
      );
      const [head, status, replacements, graftPath] = await Promise.all([
        runGit(repository, ["rev-parse", "--verify", "HEAD^{commit}"], signal),
        runGit(
          repository,
          [
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
          ],
          signal,
        ),
        runGit(
          repository,
          ["for-each-ref", "--format=%(refname)", "refs/replace/"],
          signal,
        ),
        runGit(
          repository,
          ["rev-parse", "--git-path", "info/grafts"],
          signal,
        ),
      ]);
      let attached = false;
      try {
        await runGit(repository, ["symbolic-ref", "--quiet", "HEAD"], signal);
        attached = true;
      } catch (cause) {
        if (cause?.exitCode !== 1) throw cause;
      }
      const graft = await lstat(
        resolve(repository, graftPath.stdout.trimEnd()),
      ).catch((cause) => {
        if (cause?.code === "ENOENT") return undefined;
        throw cause;
      });
      if (
        attached ||
        head.stdout.trimEnd() !== mergeHead ||
        status.stdout !== "" ||
        replacements.stdout !== "" ||
        graft != null
      ) {
        throw new Error("scratch pull-request checkout is not detached and clean");
      }
      return {
        number: pullRequest.number,
        pullRequestHead,
        mergeHead,
        baseHead,
        repository,
        cleanup: () => rm(root, { recursive: true, force: true }),
      };
    } catch (cause) {
      await rm(root, { recursive: true, force: true });
      throw cause;
    }
  };

  const readMasterSource = async (signal) => {
    const response = await ciFetch(
      config,
      "/api/ci/source/state",
      {
        headers: {
          accept: "application/json",
          authorization: "Bearer " + config.sourceToken,
        },
      },
      signal,
    );
    if (!response.ok) {
      throw new Error(
        await responseFailure("read current master source state", response),
      );
    }
    return parseMasterSourceState(
      await readJsonResponse(
        response,
        maximumJsonBytes,
        "read current master source state",
      ),
    );
  };

  const readLane = async (number, signal) => {
    const response = await ciFetch(
      config,
      "/api/ci/source/pull-requests/" + number + "/state",
      {
        headers: {
          accept: "application/json",
          authorization: "Bearer " + config.sourceToken,
        },
      },
      signal,
    );
    if (response.status === 200) {
      return parsePullRequestLaneState(
        await readJsonResponse(
          response,
          maximumJsonBytes,
          "read PR source lane",
        ),
        number,
      );
    }
    if (response.status === 404) {
      return parsePullRequestLaneState(
        await readJsonResponse(
          response,
          maximumJsonBytes,
          "read missing PR source lane",
        ),
        number,
      );
    }
    throw new Error(await responseFailure("read PR source lane", response));
  };

  const runPublisher = async (stage, childEnv, signal, extraFd) => {
    return assertQuietProcessSuccess(
      await runProcess(canonicalNode, [trustedPublisherPath(stage)], {
        cwd: webDirectory,
        env: childEnv,
        extraFd,
        signal,
        ...processOptions,
      }),
      "PR " + stage + " publisher",
    );
  };

  const probePrepHelper = async (signal) => {
    const result = await runProcess(
      sudoPath,
      prepHelperArguments(canonicalNode, config.prepUser, "--probe"),
      {
        cwd: "/",
        env: cargoBuilderEnvironment(config.env),
        signal,
        ...processOptions,
      },
    );
    return parsePrepProbe(
      assertQuietProcessSuccess(
        result,
        "PR preparation helper probe",
      ).stdout,
    );
  };

  const assertFresh = async (
    expected,
    checkout,
    signal,
    phase = "authoritative PR identity check",
  ) => {
    const identity = parseMonitoredPullRequestIdentity(checkout);
    if (
      expected.number !== identity.number ||
      expected.pullRequestHead !== identity.pullRequestHead ||
      (expected.mergeHead != null && expected.mergeHead !== identity.mergeHead)
    ) {
      throw new StalePullRequestIdentityError(identity, phase);
    }
    const refs = [
      "refs/heads/master",
      "refs/pull/" + identity.number + "/head",
      "refs/pull/" + identity.number + "/merge",
    ];
    const [observed, masterSource, remote] = await Promise.all([
      readGitHubPullRequest(identity.number, signal),
      readMasterSource(signal),
      runGit(
        webDirectory,
        ["ls-remote", "--refs", authoritativeRepositoryUrl, ...refs],
        signal,
      ),
    ]);
    const parsed = parseLsRemote(remote.stdout, refs);
    return assertAuthoritativePullRequestIdentity(
      identity,
      {
        publishedMasterHead: masterSource.head,
        pullRequest: observed,
        masterRef: parsed.get(refs[0]),
        pullRequestHeadRef: parsed.get(refs[1]),
        mergeRef: parsed.get(refs[2]),
      },
      phase,
    );
  };

  const whileCurrent = (checkout, phase, operation, signal) =>
    runWhilePullRequestIsCurrent(checkout, operation, {
      assertCurrent: (identity, childSignal, monitoredPhase) =>
        assertFresh(identity, identity, childSignal, monitoredPhase),
      phase,
      pollMs: config.identityPollMs,
      signal,
    });

  return {
    loadState: () => loadControllerState(config.statePath),
    saveState: (entries) => saveControllerState(config.statePath, entries),
    readMasterSource,
    listOpenPullRequests: (signal) =>
      collectOpenPullRequests(
        async (page) => {
          const path =
            "/repos/" +
            repositorySlug +
            "/pulls?state=open&base=master&sort=created&direction=asc" +
            "&per_page=" +
            openPullRequestPageSize +
            "&page=" +
            page;
          const response = await githubJson(
            path,
            undefined,
            signal,
            "list open GitHub pull requests",
          );
          const nextPage = parseGitHubNextPage(response.link, page);
          if (
            nextPage != null &&
            response.remaining === 0
          ) {
            throw new GitHubRateLimitError(
              response.retryAt ?? Date.now() + config.intervalMs,
              "list open GitHub pull requests",
            );
          }
          return { items: response.value, nextPage };
        },
        { maximumPages: config.maximumPages },
      ),
    readLane,
    probePrepHelper,
    preparePullRequest,
    publishCargoVendor: (checkout, signal) => publishPreparedPullRequestCargoVendor({
      probe: probePrepHelper,
      build: (childSignal) => captureCargoVendorArtifact({
        args: prepHelperArguments(canonicalNode, config.prepUser, "--build"),
        artifactDirectory: config.scratchRoot,
        command: sudoPath,
        cwd: "/",
        env: cargoBuilderEnvironment(config.env),
        input: Buffer.from(JSON.stringify({
          baseHead: checkout.baseHead,
          mergeHead: checkout.mergeHead,
          number: checkout.number,
          pullRequestHead: checkout.pullRequestHead,
          version: 1,
        })),
        secrets,
        signal: childSignal,
        timeoutMs: config.commandTimeoutMs,
      }),
      assertFresh: (childSignal) => assertFresh({
        number: checkout.number,
        pullRequestHead: checkout.pullRequestHead,
        mergeHead: checkout.mergeHead,
      }, checkout, childSignal),
      upload: async (artifact, childSignal) => {
        const uploaded = await whileCurrent(
          checkout,
          "the credential-bearing PR Cargo vendor upload",
          (monitoredSignal) => runPublisher(
            "cargo-vendor",
            publicationEnvironment(childEnvironment, "cargo-vendor"),
            monitoredSignal,
            artifact.handle.fd,
          ),
          childSignal,
        );
        assertUploaderDescriptor(uploaded.stdout, artifact.descriptor);
      },
    }, checkout, { signal }),
    publishSource: (checkout, rustSecRevision, cargoVendorSha256, signal) =>
      whileCurrent(
        checkout,
        "the credential-bearing PR source publication",
        (monitoredSignal) => runPublisher(
          "source",
          publicationEnvironment(childEnvironment, "source", {
            cargoVendorSha256,
            repository: checkout.repository,
            rustSecRevision,
            pullRequest: checkout,
          }),
          monitoredSignal,
        ),
        signal,
      ),
    assertFresh,
    readRun: async (head, signal) => {
      const response = await ciFetch(
        config,
        "/api/ci/runs/" + head,
        { headers: { accept: "application/json" } },
        signal,
      );
      if (
        response.status === 404 ||
        response.status === 425 ||
        response.status === 429 ||
        response.status >= 500
      ) {
        await response.body?.cancel();
        return { outcome: "pending", workflowStatus: "unknown" };
      }
      if (!response.ok) {
        throw new Error(await responseFailure("read PR merge run", response));
      }
      return parseCiRunState(
        await readJsonResponse(
          response,
          maximumJsonBytes,
          "read PR merge run",
        ),
        head,
      );
    },
    updateStatus: async (identity, state, signal, checkout) => {
      const payload = pullRequestStatusPayload(
        state,
        identity.pullRequestHead,
        identity.mergeHead,
        config.ciOrigin,
      );
      const publish = async (childSignal) => {
        await abortableDelay(
          Math.max(0, nextGitHubMutationAt - Date.now()),
          childSignal,
        );
        nextGitHubMutationAt = Date.now() + githubMutationIntervalMs;
        const response = await boundedFetch(
          new URL(
            "/repos/" +
              repositorySlug +
              "/statuses/" +
              identity.pullRequestHead,
            githubApiOrigin,
          ),
          {
            method: "POST",
            redirect: "error",
            headers: {
              accept: "application/vnd.github+json",
              authorization: "Bearer " + config.githubToken,
              "content-type": "application/json",
              "user-agent": "nanocodex-trusted-pr-ci-controller",
              "x-github-api-version": "2022-11-28",
            },
            body: JSON.stringify(payload),
          },
          childSignal,
          config.requestTimeoutMs,
        );
        if (response.status !== 201) {
          await throwGitHubResponseFailure(
            "publish GitHub PR status",
            response,
          );
        }
        await response.body?.cancel();
      };
      if (state === "pending") return publish(signal);
      const monitored = parseMonitoredPullRequestIdentity(checkout);
      if (
        monitored.number !== identity.number ||
        monitored.pullRequestHead !== identity.pullRequestHead ||
        monitored.mergeHead !== identity.mergeHead
      ) {
        throw new StalePullRequestIdentityError(
          monitored,
          "binding a terminal GitHub status to its checkout",
        );
      }
      return whileCurrent(
        monitored,
        "the terminal GitHub PR status publication",
        publish,
        signal,
      );
    },
    closeLane: (entry, signal) =>
      closePullRequestLane(
        config,
        readLane,
        readGitHubPullRequest,
        entry,
        signal,
      ),
  };
}

export async function closePullRequestLane(
  config,
  readLane,
  readGitHubPullRequest,
  entry,
  signal,
  fetchCi = ciFetch,
) {
  const pullRequest = await readGitHubPullRequest(entry.number, signal);
  if (pullRequest.state === "open" && pullRequest.basedOnMaster) {
    return false;
  }
  const closeId = newCloseId();
  for (let attempt = 0; attempt < 3; attempt++) {
    const lane = await readLane(entry.number, signal);
    if (lane.type === "not_published" || lane.type === "closed") return true;
    const matchesPrevious =
      entry.previousLane != null &&
      lane.pullRequestHead === entry.previousLane.pullRequestHead &&
      lane.mergeHead === entry.previousLane.mergeHead;
    if (!laneMatchesIdentity(lane, entry) && !matchesPrevious) {
      throw new Error(
        "refusing to close a PR lane newer than the persisted generation",
      );
    }
    const confirmation = await readGitHubPullRequest(entry.number, signal);
    if (confirmation.state === "open" && confirmation.basedOnMaster) {
      return false;
    }
    const response = await fetchCi(
      config,
      "/api/ci/source/pull-requests/" + entry.number + "/state",
      {
        method: "DELETE",
        headers: {
          authorization: "Bearer " + config.sourceToken,
          "content-type": "application/json",
        },
        body: JSON.stringify({
          closeId,
          expectedMergeHead: lane.mergeHead,
          expectedPullRequestHead: lane.pullRequestHead,
        }),
      },
      signal,
    );
    if (response.status === 200) {
      const value = await readJsonResponse(
        response,
        maximumJsonBytes,
        "close PR source lane",
      );
      if (
        !record(value) ||
        value.closed !== true ||
        value.number !== entry.number ||
        value.closeId !== closeId ||
        value.mergeHead !== lane.mergeHead ||
        value.pullRequestHead !== lane.pullRequestHead
      ) {
        throw new Error("close PR source lane returned invalid proof");
      }
      return true;
    }
    if (response.status === 404 || response.status === 409) {
      await response.body?.cancel();
      continue;
    }
    throw new Error(await responseFailure("close PR source lane", response));
  }
  throw new Error(
    "pull-request lane changed repeatedly while it was being closed",
  );
}


export function parseLsRemote(output, requiredRefs) {
  if (
    typeof output !== "string" ||
    !Array.isArray(requiredRefs) ||
    requiredRefs.length === 0
  ) {
    throw new TypeError("Git ls-remote output and refs are required");
  }
  const expected = new Set(requiredRefs);
  if (expected.size !== requiredRefs.length) {
    throw new Error("Git refs must be unique");
  }
  const observed = new Map();
  for (const line of output.trim().split("\n").filter(Boolean)) {
    const match = /^([a-f0-9]{40})\t([^\t\r\n]+)$/.exec(line);
    if (!match || !expected.has(match[2]) || observed.has(match[2])) {
      throw new Error("git ls-remote returned unexpected output");
    }
    observed.set(match[2], match[1]);
  }
  for (const ref of requiredRefs) {
    if (!observed.has(ref)) {
      throw new Error("git ls-remote omitted " + ref);
    }
  }
  return observed;
}

async function loadControllerState(path) {
  const metadata = await lstat(path).catch((cause) => {
    if (cause?.code === "ENOENT") return undefined;
    throw cause;
  });
  if (!metadata) return [];
  if (
    !metadata.isFile() ||
    metadata.isSymbolicLink() ||
    metadata.size <= 0 ||
    metadata.size > maximumJsonBytes
  ) {
    throw new Error("PR controller state must be a bounded regular file");
  }
  let value;
  try {
    value = JSON.parse(await readFile(path, "utf8"));
  } catch (cause) {
    throw new Error("PR controller state is not valid JSON", { cause });
  }
  return parseControllerState(value);
}

async function saveControllerState(path, entries) {
  const normalized = parseControllerState({
    version: 1,
    repository: repositorySlug,
    pullRequests: entries,
  });
  const body = Buffer.from(
    JSON.stringify({
      version: 1,
      repository: repositorySlug,
      pullRequests: normalized,
    }) + "\n",
    "utf8",
  );
  if (body.byteLength > maximumJsonBytes) {
    throw new Error("PR controller state exceeds its size limit");
  }
  await mkdir(dirname(path), { recursive: true, mode: 0o700 });
  const temporary = path + ".tmp-" + randomUUID();
  let handle;
  try {
    handle = await open(temporary, "wx", 0o600);
    await handle.writeFile(body);
    await handle.sync();
    await handle.close();
    handle = undefined;
    await rename(temporary, path);
  } finally {
    await handle?.close().catch(() => undefined);
    await unlink(temporary).catch((cause) => {
      if (cause?.code !== "ENOENT") throw cause;
    });
  }
}

async function ciFetch(config, path, init, signal) {
  return boundedFetch(
    new URL(path, config.ciOrigin),
    { ...init, redirect: "error" },
    signal,
    config.requestTimeoutMs,
  );
}

async function boundedFetch(url, init, signal, timeoutMs) {
  throwIfAborted(signal);
  const controller = new AbortController();
  let timedOut = false;
  const timeout = setTimeout(() => {
    timedOut = true;
    controller.abort(new Error("request timed out"));
  }, timeoutMs);
  const abort = () => controller.abort(signal.reason);
  signal?.addEventListener("abort", abort, { once: true });
  try {
    return await fetch(url, { ...init, signal: controller.signal });
  } catch (cause) {
    if (timedOut) {
      throw new Error("request timed out after " + timeoutMs + "ms", {
        cause,
      });
    }
    throw cause;
  } finally {
    clearTimeout(timeout);
    signal?.removeEventListener("abort", abort);
  }
}

async function readJsonResponse(response, maximumBytes, operation) {
  const text = await boundedResponseText(response, maximumBytes);
  try {
    return JSON.parse(text);
  } catch (cause) {
    throw new Error(operation + " returned invalid JSON", { cause });
  }
}

async function responseFailure(operation, response) {
  const detail = await boundedResponseText(response, maximumErrorBytes);
  return (
    operation +
    " failed with HTTP " +
    response.status +
    (detail ? ": " + detail : "")
  );
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
  const text = Buffer.concat(chunks, bytes)
    .subarray(0, maximumBytes)
    .toString("utf8");
  return truncated ? text + "…" : text;
}

function githubRateLimitRetryAt(response) {
  const retryAfter = response.headers.get("retry-after");
  if (retryAfter != null && /^[0-9]+$/.test(retryAfter)) {
    const seconds = Number(retryAfter);
    if (Number.isSafeInteger(seconds) && seconds >= 0 && seconds <= 86_400) {
      return Date.now() + seconds * 1_000;
    }
  }
  const reset = response.headers.get("x-ratelimit-reset");
  if (reset != null && /^[0-9]+$/.test(reset)) {
    const seconds = Number(reset);
    if (
      Number.isSafeInteger(seconds) &&
      seconds > 0 &&
      seconds <= Math.floor(Date.now() / 1_000) + 86_400
    ) {
      return seconds * 1_000;
    }
  }
  return null;
}

export async function throwGitHubResponseFailure(operation, response) {
  const detail = await boundedResponseText(response, maximumErrorBytes);
  const remaining = parseRateRemaining(
    response.headers.get("x-ratelimit-remaining"),
  );
  const retryAfter = response.headers.get("retry-after");
  const secondary = /secondary rate limit|abuse detection/i.test(detail);
  const rateLimited =
    response.status === 429 ||
    (response.status === 403 &&
      (remaining === 0 || retryAfter != null || secondary));
  if (rateLimited) {
    throw new GitHubRateLimitError(
      Math.max(
        Date.now() + minimumGitHubRateLimitDelayMs,
        githubRateLimitRetryAt(response) ?? 0,
      ),
      operation,
    );
  }
  throw new Error(
    operation +
      " failed with HTTP " +
      response.status +
      (detail ? ": " + detail : ""),
  );
}

function parseRateRemaining(value) {
  if (value == null) return undefined;
  if (!/^(?:0|[1-9][0-9]*)$/.test(value)) {
    throw new Error("GitHub returned an invalid rate-limit header");
  }
  const remaining = Number(value);
  if (!Number.isSafeInteger(remaining)) {
    throw new Error("GitHub returned an invalid rate-limit header");
  }
  return remaining;
}

export async function runProcess(
  command,
  args,
  { cwd, env, extraFd, signal, timeoutMs, secrets = [] },
) {
  throwIfAborted(signal);
  if (extraFd != null && (!Number.isSafeInteger(extraFd) || extraFd < 0)) {
    throw new Error("inherited process fd must be a nonnegative integer");
  }
  if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 1) {
    throw new Error("process timeout must be a positive integer");
  }
  const child = spawn(command, args, {
    cwd,
    env,
    shell: false,
    detached: process.platform !== "win32",
    stdio: extraFd == null
      ? ["ignore", "pipe", "pipe"]
      : ["ignore", "pipe", "pipe", extraFd],
  });
  const processGroupId = process.platform === "win32" ? undefined : child.pid;
  const stdout = [];
  const stderr = [];
  let stdoutBytes = 0;
  let stderrBytes = 0;
  let failure;
  let cleanupTask;
  let resolveCleanupStarted;
  const cleanupStarted = new Promise((resolvePromise) => {
    resolveCleanupStarted = resolvePromise;
  });

  const beginGroupCleanup = () => {
    if (cleanupTask != null) return;
    cleanupTask = cleanupDetachedProcessGroup(child, processGroupId).then(
      (value) => ({ value }),
      (error) => ({ error }),
    );
    resolveCleanupStarted();
  };

  const requestTermination = (cause) => {
    failure ??= cause;
    beginGroupCleanup();
  };

  const failBound = (stream) => {
    requestTermination(
      new Error(
        command +
          " " +
          args[0] +
          " exceeded the " +
          maximumProcessOutputBytes +
          "-byte " +
          stream +
          " limit",
      ),
    );
  };
  child.stdout.on("data", (chunk) => {
    stdoutBytes += chunk.length;
    if (stdoutBytes > maximumProcessOutputBytes) {
      failBound("stdout");
      return;
    }
    stdout.push(chunk);
  });
  child.stderr.on("data", (chunk) => {
    stderrBytes += chunk.length;
    if (stderrBytes > maximumProcessOutputBytes) {
      failBound("stderr");
      return;
    }
    stderr.push(chunk);
  });

  const onAbort = () =>
    requestTermination(
      signal.reason ?? new DOMException("Aborted", "AbortError"),
    );
  signal?.addEventListener("abort", onAbort, { once: true });
  if (signal?.aborted) onAbort();
  const timeout = setTimeout(
    () =>
      requestTermination(
        new Error(
          command + " " + args[0] + " timed out after " + timeoutMs + "ms",
        ),
      ),
    timeoutMs,
  );
  timeout.unref?.();

  const close = new Promise((resolvePromise) => {
    child.once("close", (code, signalName) =>
      resolvePromise({ code, signalName }));
  });
  child.once("error", requestTermination);
  // `close` waits for inherited pipes. Cleanup begins at leader exit so a
  // same-group descendant cannot keep those pipes, or detached stdio, alive.
  child.once("exit", beginGroupCleanup);

  let cleanupOutcome;
  let result;
  let closeFailure;
  try {
    await cleanupStarted;
    cleanupOutcome = await cleanupTask;
    try {
      result = await waitForChildClose(close, command);
    } catch (cause) {
      closeFailure = cause;
    }
  } finally {
    clearTimeout(timeout);
    signal?.removeEventListener("abort", onAbort);
  }

  const cleanupFailures = [cleanupOutcome?.error, closeFailure].filter(Boolean);
  const cleanupFailure = cleanupFailures.length > 1
    ? new AggregateError(cleanupFailures, "detached process cleanup failed")
    : cleanupFailures[0];
  const abortFailure = signal?.aborted
    ? signal.reason ?? new DOMException("Aborted", "AbortError")
    : undefined;
  const primaryFailure = abortFailure ?? failure;
  if (primaryFailure) {
    if (cleanupFailure != null) {
      if (primaryFailure instanceof Error) primaryFailure.cleanupCause = cleanupFailure;
      else {
        throw new AggregateError(
          [primaryFailure, cleanupFailure],
          "PR publisher abort and cleanup failed",
        );
      }
    }
    throw primaryFailure;
  }
  if (cleanupFailure != null) throw cleanupFailure;
  if (result.code === 0 && cleanupOutcome.value.observedLiveGroup) {
    throw new Error(
      command + " " + args[0] +
        " exited successfully but left a live detached process-group descendant",
    );
  }

  const output = {
    stdout: Buffer.concat(stdout, stdoutBytes).toString("utf8"),
    stderr: Buffer.concat(stderr, stderrBytes).toString("utf8"),
  };
  if (result.code === 0) return output;
  const detail = redactSecrets(
    output.stderr || output.stdout || result.signalName || "unknown failure",
    secrets,
  )
    .trim()
    .slice(0, maximumErrorBytes);
  const error = new Error(
    command +
      " " +
      args[0] +
      " failed with exit " +
      String(result.code) +
      (detail ? ": " + detail : ""),
  );
  error.exitCode = result.code;
  throw error;
}

async function cleanupDetachedProcessGroup(child, processGroupId) {
  if (process.platform === "win32") {
    const observedLiveProcess = child.exitCode == null && child.signalCode == null;
    if (!observedLiveProcess) return { observedLiveGroup: false };
    child.kill("SIGTERM");
    await delayForProcessCleanup(processTerminationGraceMs);
    if (child.exitCode == null && child.signalCode == null) child.kill("SIGKILL");
    return { observedLiveGroup: false };
  }
  if (!Number.isSafeInteger(processGroupId) || processGroupId <= 0) {
    return { observedLiveGroup: false };
  }
  const observedLiveGroup = signalProcessGroup(processGroupId, "SIGTERM");
  if (!observedLiveGroup) return { observedLiveGroup: false };
  if (await waitForProcessGroupExit(processGroupId, processTerminationGraceMs)) {
    return { observedLiveGroup: true };
  }
  signalProcessGroup(processGroupId, "SIGKILL");
  if (!await waitForProcessGroupExit(processGroupId, processKillWaitMs)) {
    throw new Error("detached process group did not exit after SIGKILL");
  }
  return { observedLiveGroup: true };
}

function signalProcessGroup(processGroupId, signalName) {
  try {
    process.kill(-processGroupId, signalName);
    return true;
  } catch (cause) {
    if (cause?.code === "ESRCH") return false;
    // A killed orphan can transiently remain as an unsignalable group member.
    // Keep owning and polling that group; only ESRCH proves it is gone.
    if (cause?.code === "EPERM") return true;
    throw cause;
  }
}

function processGroupExists(processGroupId) {
  try {
    process.kill(-processGroupId, 0);
    return true;
  } catch (cause) {
    if (cause?.code === "ESRCH") return false;
    if (cause?.code === "EPERM") return true;
    throw cause;
  }
}

async function waitForProcessGroupExit(processGroupId, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (processGroupExists(processGroupId)) {
    const remaining = deadline - Date.now();
    if (remaining <= 0) return false;
    await delayForProcessCleanup(Math.min(processGroupPollMs, remaining));
  }
  return true;
}

async function waitForChildClose(close, command) {
  let timeout;
  try {
    return await Promise.race([
      close,
      new Promise((_, rejectPromise) => {
        timeout = setTimeout(
          () => rejectPromise(new Error(command + " child was not reaped after group cleanup")),
          processKillWaitMs,
        );
      }),
    ]);
  } finally {
    clearTimeout(timeout);
  }
}

function delayForProcessCleanup(milliseconds) {
  return new Promise((resolvePromise) => setTimeout(resolvePromise, milliseconds));
}

export function redactSecrets(value, secrets) {
  let redacted = String(value);
  for (const secret of [...new Set(secrets.filter(Boolean))].sort(
    (left, right) => right.length - left.length,
  )) {
    redacted = redacted.split(secret).join("[redacted]");
  }
  return redacted;
}

function sortedControllerEntries(entries) {
  return [...entries.values()].sort(
    (left, right) => left.number - right.number,
  );
}

function sameIdentity(left, right) {
  return Boolean(
    left &&
      right &&
      left.number === right.number &&
      left.pullRequestHead === right.pullRequestHead &&
      left.mergeHead === right.mergeHead,
  );
}

function laneMatchesIdentity(lane, identity) {
  return (
    lane.type === "open" &&
    lane.number === identity.number &&
    lane.pullRequestHead === identity.pullRequestHead &&
    lane.mergeHead === identity.mergeHead
  );
}

function parsePullRequestIdentity(value) {
  if (
    !record(value) ||
    !Number.isSafeInteger(value.number) ||
    value.number <= 0 ||
    !isSha1(value.pullRequestHead)
  ) {
    throw new Error("canonical pull-request identity is required");
  }
  return {
    number: value.number,
    pullRequestHead: value.pullRequestHead,
  };
}

function parseMonitoredPullRequestIdentity(value) {
  const identity = parsePullRequestIdentity(value);
  assertSha1(value.baseHead, "canonical pull-request base head");
  assertSha1(value.mergeHead, "canonical pull-request merge head");
  return {
    number: identity.number,
    baseHead: value.baseHead,
    pullRequestHead: identity.pullRequestHead,
    mergeHead: value.mergeHead,
  };
}

function assertPullRequestNumber(value) {
  if (!Number.isSafeInteger(value) || value <= 0) {
    throw new Error("pull-request number must be a positive safe integer");
  }
}

function assertSha1(value, description) {
  if (!isSha1(value)) {
    throw new Error(description + " must be a full lowercase Git SHA-1");
  }
  return value;
}

function isSha1(value) {
  return typeof value === "string" && sha1Pattern.test(value);
}

function record(value) {
  return value != null && typeof value === "object" && !Array.isArray(value);
}

function pathContains(root, candidate) {
  const path = relative(root, candidate);
  return (
    path === "" ||
    (!isAbsolute(path) && path !== ".." && !path.startsWith(".." + sep))
  );
}

async function canonicalPath(candidate) {
  let cursor = candidate;
  const missing = [];
  for (;;) {
    try {
      return resolve(await realpath(cursor), ...missing);
    } catch (cause) {
      if (cause?.code !== "ENOENT") throw cause;
      const parent = dirname(cursor);
      if (parent === cursor) throw cause;
      missing.unshift(basename(cursor));
      cursor = parent;
    }
  }
}

async function assertExternalControllerPaths(config) {
  const trustedRepository = await realpath(repositoryDirectory);
  const candidates = [config.statePath, config.scratchRoot];
  for (const candidate of candidates) {
    if (pathContains(trustedRepository, await canonicalPath(candidate))) {
      throw new Error(
        "PR controller state and scratch paths must be outside the trusted repository",
      );
    }
  }
  await Promise.all([
    mkdir(dirname(config.statePath), { recursive: true, mode: 0o700 }),
    mkdir(config.scratchRoot, { recursive: true, mode: 0o700 }),
  ]);
  for (const candidate of candidates) {
    if (pathContains(trustedRepository, await canonicalPath(candidate))) {
      throw new Error(
        "PR controller path resolved inside the trusted repository",
      );
    }
  }
}

function parseOrigin(value) {
  try {
    const url = new URL(value);
    if (
      !["http:", "https:"].includes(url.protocol) ||
      url.username ||
      url.password
    ) {
      throw new Error("unsupported URL");
    }
    const loopback =
      url.hostname === "localhost" ||
      url.hostname.endsWith(".localhost") ||
      url.hostname === "127.0.0.1" ||
      url.hostname === "[::1]";
    if (url.protocol !== "https:" && !loopback) {
      throw new Error("HTTPS is required");
    }
    url.pathname = "/";
    url.search = "";
    url.hash = "";
    return url.origin;
  } catch (cause) {
    throw new Error(
      "controller origin must use HTTPS (HTTP is allowed only for loopback)",
      { cause },
    );
  }
}

function requiredEnvironment(env, name) {
  const value = env[name]?.trim();
  if (!value) throw new Error(name + " is required");
  return value;
}

function boundedIntegerEnvironment(
  env,
  name,
  fallback,
  minimum,
  maximum,
) {
  const raw = env[name]?.trim();
  if (!raw) return fallback;
  if (!/^[1-9][0-9]*$/.test(raw)) {
    throw new Error(name + " must be a positive integer");
  }
  const value = Number(raw);
  if (
    !Number.isSafeInteger(value) ||
    value < minimum ||
    value > maximum
  ) {
    throw new Error(
      name + " must be between " + minimum + " and " + maximum,
    );
  }
  return value;
}

function throwIfAborted(signal) {
  if (signal?.aborted) {
    throw signal.reason ?? new DOMException("Aborted", "AbortError");
  }
}

function isAbort(value) {
  return value?.name === "AbortError";
}

function abortableDelay(milliseconds, signal) {
  if (milliseconds <= 0) return Promise.resolve();
  throwIfAborted(signal);
  return new Promise((resolvePromise, rejectPromise) => {
    const timeout = setTimeout(() => {
      signal?.removeEventListener("abort", abort);
      resolvePromise();
    }, milliseconds);
    const abort = () => {
      clearTimeout(timeout);
      rejectPromise(
        signal.reason ?? new DOMException("Aborted", "AbortError"),
      );
    };
    signal?.addEventListener("abort", abort, { once: true });
  });
}

function usage() {
  return [
    "Usage: node web/scripts/ci-pr-controller.mjs [--once]",
    "",
    "Polls every open gakonst/nanocodex PR based on master, publishes its",
    "authoritative GitHub merge ref into an isolated Cloudflare CI lane,",
    "reports one ci success status on the PR head, and closes stale lanes.",
    "",
    "Required authority: NANOCODEX_GITHUB_STATUS_TOKEN and NANOCODEX_CI_TOKEN.",
    "Promotion, control, release, mirror, and registry credentials are rejected.",
  ].join("\n");
}

export async function main({
  env = process.env,
  args = process.argv.slice(2),
  log = console.log,
  logError = console.error,
} = {}) {
  const parsed = parseArguments(args);
  if (parsed.help) {
    log(usage());
    return;
  }
  const config = controllerConfiguration(env);
  await assertExternalControllerPaths(config);
  const release = await acquireExclusiveLock(
    config.statePath + ".lock",
    { controller: "pull-request" },
    { env: config.env },
  );
  const abort = new AbortController();
  const stop = () =>
    abort.abort(new DOMException("controller stopped", "AbortError"));
  process.once("SIGINT", stop);
  process.once("SIGTERM", stop);
  try {
    const operations = await createOperations(config);
    await operations.probePrepHelper(abort.signal);
    do {
      let retryAt;
      try {
        const result = await runPullRequestControllerCycle(operations, {
          signal: abort.signal,
        });
        for (const report of result.reports) {
          log(
            "PR " +
              report.number +
              " " +
              report.status +
              " (" +
              report.mergeHead.slice(0, 7) +
              ")",
          );
        }
        if (result.errors.length > 0) {
          throw new AggregateError(
            result.errors,
            result.errors.length + " PR reconciliation operation(s) failed",
          );
        }
        if (parsed.once) return result;
      } catch (cause) {
        if (isAbort(cause)) throw cause;
        if (parsed.once) throw cause;
        retryAt =
          cause instanceof GitHubRateLimitError ? cause.retryAt : undefined;
        logError(
          redactSecrets(
            cause instanceof Error ? cause.message : String(cause),
            [
              config.githubToken,
              config.sourceToken,
            ],
          ),
        );
      }
      const delay = Math.max(
        config.intervalMs,
        retryAt == null ? 0 : retryAt - Date.now(),
      );
      await abortableDelay(delay, abort.signal);
    } while (!abort.signal.aborted);
  } finally {
    process.removeListener("SIGINT", stop);
    process.removeListener("SIGTERM", stop);
    await release();
  }
}

if (resolve(process.argv[1] ?? "") === scriptPath) {
  try {
    await main();
  } catch (cause) {
    process.stderr.write(
      "Trusted PR CI controller failed: " +
        redactSecrets(
          cause instanceof Error ? cause.message : String(cause),
          [
            process.env.NANOCODEX_GITHUB_STATUS_TOKEN,
            process.env.NANOCODEX_CI_TOKEN,
          ],
        ) +
        "\n",
    );
    process.exitCode = 1;
  }
}
