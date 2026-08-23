import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import test from "node:test";

import {
  StalePullRequestIdentityError,
  assertAuthoritativePullRequestIdentity,
  assertNoPromotionAuthorities,
  assertQuietProcessSuccess,
  cargoPrepHelperPath,
  closePullRequestLane,
  collectOpenPullRequests,
  controllerConfiguration,
  GitHubRateLimitError,
  newCloseId,
  parseArguments,
  parseCiRunState,
  parseControllerState,
  parseGitHubNextPage,
  parseMasterSourceState,
  parseLsRemote,
  parseOpenPullRequestPage,
  parsePullRequestLaneState,
  parsePrepProbe,
  parsePrepUsername,
  prepHelperArguments,
  publicationEnvironment,
  publishPreparedPullRequestCargoVendor,
  pullRequestStatusPayload,
  runProcess,
  runPullRequestControllerCycle,
  runWhilePullRequestIsCurrent,
  trustedPublisherPath,
  throwGitHubResponseFailure,
} from "./ci-pr-controller.mjs";
import { probeDescriptor } from "./ci-pr-cargo-builder.mjs";

const headOne = "1".repeat(40);
const headTwo = "2".repeat(40);
const headThree = "3".repeat(40);
const headFour = "4".repeat(40);
const rustSecRevision = "5".repeat(40);
const cargoVendorSha256 = "6".repeat(64);

test("GitHub pagination covers the full bounded open master set", async () => {
  const first = Array.from({ length: 100 }, (_, index) =>
    githubPullRequest(index + 1, shaFor(index + 1), shaFor(index + 101))
  );
  const second = [
    githubPullRequest(102, headTwo, headThree),
    githubPullRequest(101, headOne, headFour),
  ];
  const pages = [];
  const pullRequests = await collectOpenPullRequests(async (page) => {
    pages.push(page);
    return {
      items: page === 1 ? first : second,
      nextPage: page === 1 ? 2 : null,
    };
  });
  assert.deepEqual(pages, [1, 2]);
  assert.equal(pullRequests.length, 102);
  assert.deepEqual(
    pullRequests.slice(-2).map(({ number }) => number),
    [101, 102],
  );
  assert.throws(
    () =>
      parseOpenPullRequestPage([
        {
          ...githubPullRequest(7, headOne, headTwo),
          base: {
            ref: "release",
            repo: { full_name: "gakonst/nanocodex" },
          },
        },
      ]),
    /outside the master lane/,
  );
  await assert.rejects(
    collectOpenPullRequests(
      async () => ({ items: first, nextPage: 2 }),
      { maximumPages: 1 },
    ),
    /pagination exceeded 1 pages/,
  );
  await assert.rejects(
    collectOpenPullRequests(async (page) => ({
      items: page === 1
        ? first
        : [githubPullRequest(1, headOne, headTwo)],
      nextPage: page === 1 ? 2 : null,
    })),
    /duplicate pull request/,
  );
  const nextUrl =
    "https://api.github.com/repos/gakonst/nanocodex/pulls" +
    "?state=open&base=master&sort=created&direction=asc&per_page=100&page=2";
  assert.equal(
    parseGitHubNextPage(
      `<${nextUrl}>; rel="next", <${nextUrl.replace("page=2", "page=9")}>; rel="last"`,
      1,
    ),
    2,
  );
  assert.throws(
    () =>
      parseGitHubNextPage(
        `<${nextUrl.replace("api.github.com", "evil.example")}>; rel="next"`,
        1,
      ),
    /unexpected next-page URL/,
  );
  assert.equal(
    parseOpenPullRequestPage([
      githubPullRequest(103, headThree, null),
    ])[0].mergeHead,
    null,
  );
});

test("lane and global run parsers bind exact PR, merge, and workflow identities", () => {
  const open = parsePullRequestLaneState(
    laneState(7, headOne, headTwo),
    7,
  );
  assert.equal(open.type, "open");
  assert.equal(open.mergeHead, headTwo);
  assert.equal(open.pullRequestHead, headOne);
  assert.deepEqual(
    parsePullRequestLaneState({ error: "not_published" }, 8),
    { type: "not_published", number: 8 },
  );
  const closed = parsePullRequestLaneState(
    {
      error: "pull_request_closed",
      number: 7,
      closeId: "123e4567-e89b-42d3-a456-426614174000",
      mergeHead: headTwo,
      pullRequestHead: headOne,
      closedAt: "2026-08-22T00:00:00.000Z",
    },
    7,
  );
  assert.equal(closed.type, "closed");
  assert.throws(
    () =>
      parsePullRequestLaneState(
        {
          ...laneState(7, headOne, headTwo),
          publication: {
            ...laneState(7, headOne, headTwo).publication,
            branch: "master",
          },
        },
        7,
      ),
    /invalid pull-request lane/,
  );
  assert.deepEqual(parseMasterSourceState(masterSourceState()), {
    head: headFour,
    rustSecRevision,
  });

  assert.equal(
    parseCiRunState(runState(headTwo, "complete", "success"), headTwo)
      .outcome,
    "success",
  );
  assert.equal(
    parseCiRunState(runState(headTwo, "errored", "failure"), headTwo)
      .outcome,
    "failure",
  );
  assert.equal(
    parseCiRunState(runState(headTwo, "running", "running"), headTwo)
      .outcome,
    "pending",
  );
  assert.throws(
    () => parseCiRunState(runState(headTwo, "complete", "failure"), headTwo),
    /no successful result/,
  );
});

test("authoritative PR evidence requires the published base, open identity, and exact refs", () => {
  const identity = {
    number: 7,
    baseHead: headFour,
    pullRequestHead: headOne,
    mergeHead: headTwo,
  };
  const evidence = {
    publishedMasterHead: headFour,
    pullRequest: {
      number: 7,
      state: "open",
      basedOnMaster: true,
      pullRequestHead: headOne,
      mergeHead: headTwo,
    },
    masterRef: headFour,
    pullRequestHeadRef: headOne,
    mergeRef: headTwo,
  };
  assert.deepEqual(
    assertAuthoritativePullRequestIdentity(identity, evidence),
    identity,
  );
  for (const stale of [
    { ...evidence, publishedMasterHead: headThree },
    {
      ...evidence,
      pullRequest: { ...evidence.pullRequest, state: "closed" },
    },
    {
      ...evidence,
      pullRequest: { ...evidence.pullRequest, pullRequestHead: headThree },
    },
    {
      ...evidence,
      pullRequest: { ...evidence.pullRequest, mergeHead: headThree },
    },
    { ...evidence, masterRef: headThree },
    { ...evidence, pullRequestHeadRef: headThree },
    { ...evidence, mergeRef: headThree },
  ]) {
    assert.throws(
      () => assertAuthoritativePullRequestIdentity(
        identity,
        stale,
        "test identity drift",
      ),
      (cause) =>
        cause instanceof StalePullRequestIdentityError &&
        cause.phase === "test identity drift",
    );
  }
});

test("GitHub status is written on the PR head with one immutable merge target", () => {
  for (const state of ["pending", "success", "failure", "error"]) {
    const payload = pullRequestStatusPayload(
      state,
      headOne,
      headTwo,
      "https://ci.example.test/base?ignored=1",
    );
    assert.deepEqual(Object.keys(payload).sort(), [
      "context",
      "description",
      "state",
      "target_url",
    ]);
    assert.equal(payload.context, "ci success");
    assert.equal(payload.state, state);
    assert.equal(
      payload.target_url,
      "https://ci.example.test/api/ci/runs/" + headTwo,
    );
    assert.doesNotMatch(payload.target_url, new RegExp(headOne));
    assert.ok(payload.description.length <= 140);
  }
  const closeId = newCloseId();
  assert.match(
    closeId,
    /^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/,
  );
  assert.notEqual(newCloseId(), closeId);
});

test("publisher children carry only source authority and fixed trusted code", () => {
  const env = {
    PATH: "/safe/bin",
    HOME: "/safe/home",
    NANOCODEX_CI_ORIGIN: "https://ci.example.test",
    NANOCODEX_CI_TOKEN: "source-secret",
    NANOCODEX_GITHUB_STATUS_TOKEN: "github-secret",
    NANOCODEX_RUSTSEC_REVISION: rustSecRevision,
    NANOCODEX_CI_PR_STATE_PATH: "/safe/state.json",
    AWS_SECRET_ACCESS_KEY: "ambient-aws",
    CI_MASTER_SOURCE_WRITE_TOKEN: "master-source-secret",
    CI_PR_SOURCE_WRITE_TOKEN: "pr-source-secret",
    CLOUDFLARE_API_TOKEN: "deploy-secret",
    GITHUB_TOKEN: "ambient-github",
    NANOCODEX_CI_CONTROL_TOKEN: "control-secret",
    NANOCODEX_GIT_TOKEN: "mirror-secret",
    NPM_TOKEN: "registry-secret",
  };
  assert.throws(
    () => assertNoPromotionAuthorities(env),
    /refuses promotion or ambient GitHub authorities/,
  );
  const allowed = {
    ...env,
    AWS_SECRET_ACCESS_KEY: undefined,
    CI_MASTER_SOURCE_WRITE_TOKEN: undefined,
    CI_PR_SOURCE_WRITE_TOKEN: undefined,
    CLOUDFLARE_API_TOKEN: undefined,
    GITHUB_TOKEN: undefined,
    NANOCODEX_CI_CONTROL_TOKEN: undefined,
    NANOCODEX_GIT_TOKEN: undefined,
    NPM_TOKEN: undefined,
  };
  assert.doesNotThrow(() => assertNoPromotionAuthorities(allowed));
  const cargo = publicationEnvironment(allowed, "cargo-vendor");
  const source = publicationEnvironment(allowed, "source", {
    cargoVendorSha256,
    repository: "/scratch/untrusted-checkout",
    rustSecRevision,
    pullRequest: { number: 7, pullRequestHead: headOne },
  });
  assert.deepEqual(authorityKeys(cargo), ["NANOCODEX_CI_TOKEN"]);
  assert.deepEqual(authorityKeys(source), ["NANOCODEX_CI_TOKEN"]);
  assert.equal(source.NANOCODEX_CI_PULL_REQUEST_NUMBER, "7");
  assert.equal(source.NANOCODEX_CI_PULL_REQUEST_HEAD, headOne);
  assert.equal(source.NANOCODEX_RUSTSEC_REVISION, rustSecRevision);
  assert.equal(source.NANOCODEX_CI_CARGO_VENDOR_SHA256, cargoVendorSha256);
  assert.equal(cargo.NANOCODEX_REPO, undefined);
  assert.equal(cargo.CARGO_HOME, undefined);
  for (const child of [cargo, source]) {
    assert.equal(child.NANOCODEX_GITHUB_STATUS_TOKEN, undefined);
    assert.equal(child.NANOCODEX_CI_CONTROL_TOKEN, undefined);
    assert.equal(child.CLOUDFLARE_API_TOKEN, undefined);
    assert.equal(child.NANOCODEX_GIT_TOKEN, undefined);
    assert.equal(child.NPM_TOKEN, undefined);
  }
  for (const stage of ["cargo-vendor", "source"]) {
    const path = trustedPublisherPath(stage);
    assert.ok(path.startsWith(resolve(import.meta.dirname, "..") + "/"));
    assert.ok(!path.startsWith("/scratch/untrusted-checkout/"));
  }
  assert.deepEqual(parseArguments(["--once"]), { once: true, help: false });
  assert.throws(() => parseArguments(["--deploy"]), /unknown argument/);

  assert.equal(parsePrepUsername("nanocodex_ci_prep"), "nanocodex_ci_prep");
  assert.throws(() => parsePrepUsername("-root"), /strict POSIX/);
  assert.deepEqual(
    prepHelperArguments("/opt/node/bin/node", "nanocodex_ci_prep", "--build"),
    [
      "-n", "-u", "nanocodex_ci_prep", "--", "/opt/node/bin/node",
      cargoPrepHelperPath, "--build",
    ],
  );
  const liveHelperProbe = probeDescriptor({ env: {}, uid: 502, gid: 20 });
  assert.deepEqual(parsePrepProbe(
    JSON.stringify(liveHelperProbe) + "\n",
    501,
  ), liveHelperProbe);
  assert.throws(
    () => parsePrepProbe(JSON.stringify({
      credentialEnvironmentNames: ["NANOCODEX_CI_TOKEN"],
      freshHomePolicy: "per-build-private-temporary",
      gid: 20,
      helperVersion: "2026-08-23.1",
      uid: 502,
      version: 1,
    }) + "\n", 501),
    /credential boundary probe/,
  );
  assert.deepEqual(
    assertQuietProcessSuccess(
      { stdout: "ok\n", stderr: "" },
      "PR preparation helper probe",
    ),
    { stdout: "ok\n", stderr: "" },
  );
  assert.throws(
    () => assertQuietProcessSuccess(
      { stdout: "ok\n", stderr: "unexpected diagnostic\n" },
      "PR source publisher",
    ),
    /successful PR source publisher emitted stderr/,
  );
});

test("controller serially publishes supersessions, reports on heads, and CAS-closes stale PRs", async () => {
  const calls = [];
  const lanes = new Map([
    [1, {
      type: "open",
      number: 1,
      pullRequestHead: headOne,
      mergeHead: headTwo,
    }],
    [2, {
      type: "closed",
      number: 2,
      closeId: "123e4567-e89b-42d3-a456-426614174000",
      pullRequestHead: headTwo,
      mergeHead: headThree,
    }],
  ]);
  const previous = [
    {
      number: 1,
      pullRequestHead: headOne,
      mergeHead: headTwo,
      status: "success",
    },
    {
      number: 3,
      pullRequestHead: headThree,
      mergeHead: headFour,
      status: "pending",
    },
  ];
  let saved;
  const savedSnapshots = [];
  const operations = {
    loadState: async () => previous,
    readMasterSource: async () => ({
      head: headFour,
      rustSecRevision,
    }),
    listOpenPullRequests: async () => [
      { number: 1, pullRequestHead: headOne, mergeHead: headThree },
      { number: 2, pullRequestHead: headTwo, mergeHead: headFour },
    ],
    readLane: async (number) => {
      calls.push("lane:" + number);
      return lanes.get(number) ?? { type: "not_published", number };
    },
    preparePullRequest: async (pullRequest) => {
      calls.push("prepare:" + pullRequest.number);
      return {
        ...pullRequest,
        baseHead: headFour,
        repository: "/scratch/pr-" + pullRequest.number,
        cleanup: async () => calls.push("cleanup:" + pullRequest.number),
      };
    },
    updateStatus: async (identity, status) => {
      calls.push(
        "status:" +
          status +
          ":head=" +
          identity.pullRequestHead +
          ":merge=" +
          identity.mergeHead,
      );
    },
    publishCargoVendor: async (checkout) => {
      calls.push("cargo:" + checkout.number);
      return { sha256: cargoVendorSha256 };
    },
    publishSource: async (checkout, _revision, observedCargoSha) => {
      assert.equal(observedCargoSha, cargoVendorSha256);
      calls.push("source:" + checkout.number);
      lanes.set(checkout.number, {
        type: "open",
        number: checkout.number,
        pullRequestHead: checkout.pullRequestHead,
        mergeHead: checkout.mergeHead,
      });
    },
    assertFresh: async (pullRequest) => {
      calls.push("fresh:" + pullRequest.number);
    },
    readRun: async (mergeHead) => {
      calls.push("run:" + mergeHead);
      return {
        outcome: mergeHead === headThree ? "success" : "failure",
      };
    },
    closeLane: async (entry) => {
      calls.push("close:" + entry.number);
      return true;
    },
    saveState: async (entries) => {
      calls.push("save");
      saved = entries;
      savedSnapshots.push(structuredClone(entries));
    },
  };

  const result = await runPullRequestControllerCycle(operations);
  assert.equal(result.errors.length, 0);
  assert.deepEqual(saved, [
    {
      number: 1,
      pullRequestHead: headOne,
      mergeHead: headThree,
      status: "success",
    },
    {
      number: 2,
      pullRequestHead: headTwo,
      mergeHead: headFour,
      status: "failure",
    },
  ]);
  assert.deepEqual(calls, [
    "prepare:1",
    "lane:1",
    "save",
    "status:pending:head=" + headOne + ":merge=" + headThree,
    "cargo:1",
    "fresh:1",
    "source:1",
    "fresh:1",
    "lane:1",
    "run:" + headThree,
    "fresh:1",
    "status:success:head=" + headOne + ":merge=" + headThree,
    "cleanup:1",
    "prepare:2",
    "lane:2",
    "save",
    "status:pending:head=" + headTwo + ":merge=" + headFour,
    "cargo:2",
    "fresh:2",
    "source:2",
    "fresh:2",
    "lane:2",
    "run:" + headFour,
    "fresh:2",
    "status:failure:head=" + headTwo + ":merge=" + headFour,
    "cleanup:2",
    "close:3",
    "save",
  ]);
  assert.ok(!calls.some((call) => call.startsWith("terminate:")));
  assert.equal(
    savedSnapshots[0].find(({ number }) => number === 1).status,
    null,
  );
});

test("post-helper freshness and exact identity gate upload, then freshness is rechecked", async () => {
  const checkout = {
    number: 7,
    baseHead: headFour,
    pullRequestHead: headOne,
    mergeHead: headTwo,
  };
  const descriptor = {
    version: 1,
    number: checkout.number,
    baseHead: checkout.baseHead,
    pullRequestHead: checkout.pullRequestHead,
    mergeHead: checkout.mergeHead,
    cargoLockBlob: headThree,
    key: `cargo-vendor/${headThree}/${cargoVendorSha256}/bundle.tar.gz`,
    size: 123,
    sha256: cargoVendorSha256,
  };
  const staleCalls = [];
  await assert.rejects(
    publishPreparedPullRequestCargoVendor({
      probe: async () => staleCalls.push("probe"),
      build: async () => ({
        descriptor,
        cleanup: async () => staleCalls.push("cleanup"),
      }),
      assertFresh: async () => {
        staleCalls.push("fresh");
        throw new Error("PR changed after helper");
      },
      upload: async () => staleCalls.push("upload"),
    }, checkout),
    /changed after helper/,
  );
  assert.deepEqual(staleCalls, ["probe", "fresh", "cleanup"]);

  const calls = [];
  assert.equal((await publishPreparedPullRequestCargoVendor({
    probe: async () => calls.push("probe"),
    build: async () => {
      calls.push("build");
      return {
        descriptor,
        cleanup: async () => calls.push("cleanup"),
      };
    },
    assertFresh: async () => calls.push("fresh"),
    upload: async () => calls.push("upload"),
  }, checkout)).sha256, cargoVendorSha256);
  assert.deepEqual(calls, ["probe", "build", "fresh", "upload", "fresh", "cleanup"]);

  const wrong = [];
  await assert.rejects(
    publishPreparedPullRequestCargoVendor({
      probe: async () => wrong.push("probe"),
      build: async () => ({
        descriptor: { ...descriptor, mergeHead: headThree },
        cleanup: async () => wrong.push("cleanup"),
      }),
      assertFresh: async () => wrong.push("fresh"),
      upload: async () => wrong.push("upload"),
    }, checkout),
    /different authoritative identity/,
  );
  assert.deepEqual(wrong, ["probe", "cleanup"]);
});

test("authoritative identity drift aborts and awaits a mid-upload cleanup", async () => {
  const observed = await exerciseMonitoredCancellation(
    "the credential-bearing PR Cargo vendor upload",
  );
  assert.deepEqual(observed, {
    checks: 2,
    operationAborted: true,
    operationStarted: true,
    cleanupFinished: true,
  });
});

test("PR closure aborts and awaits a mid-source publication cleanup", async () => {
  const observed = await exerciseMonitoredCancellation(
    "the credential-bearing PR source publication",
  );
  assert.deepEqual(observed, {
    checks: 2,
    operationAborted: true,
    operationStarted: true,
    cleanupFinished: true,
  });
});

test("publisher cancellation kills pipe-detached and pipe-holding descendants", async () => {
  if (process.platform === "win32") return;
  const directory = await mkdtemp(
    resolve(tmpdir(), "nanocodex-pr-publisher-cancel-"),
  );
  const pidPath = resolve(directory, "processes.json");
  const controller = new AbortController();
  let running;
  let pids;
  try {
    const descendantProgram =
      "process.on('SIGTERM',()=>{});setInterval(()=>{},1000)";
    const publisherProgram = [
      "const {spawn}=require('node:child_process');",
      "const {writeFileSync}=require('node:fs');",
      "const ignored=spawn(process.execPath,['--eval'," +
        JSON.stringify(descendantProgram) +
        "],{stdio:'ignore'});",
      "const inherited=spawn(process.execPath,['--eval'," +
        JSON.stringify(descendantProgram) +
        "],{stdio:['ignore','inherit','inherit']});",
      "writeFileSync(process.env.PID_PATH,JSON.stringify({root:process.pid,ignored:ignored.pid,inherited:inherited.pid}));",
      "setInterval(()=>{},1000);",
    ].join("");
    running = runProcess(
      process.execPath,
      ["--eval", publisherProgram],
      {
        cwd: directory,
        env: {
          LANG: "C.UTF-8",
          PATH: process.env.PATH ?? "/usr/bin:/bin",
          PID_PATH: pidPath,
        },
        signal: controller.signal,
        timeoutMs: 10_000,
      },
    );
    for (let attempt = 0; attempt < 200; attempt += 1) {
      const value = await readFile(pidPath, "utf8").catch(() => undefined);
      if (value != null) {
        pids = JSON.parse(value);
        break;
      }
      await new Promise((resolvePromise) => setTimeout(resolvePromise, 10));
    }
    assert.ok(Number.isSafeInteger(pids?.root) && pids.root > 0);
    assert.ok(
      Number.isSafeInteger(pids?.ignored) && pids.ignored > 0,
    );
    assert.ok(
      Number.isSafeInteger(pids?.inherited) && pids.inherited > 0,
    );
    controller.abort(new DOMException("superseded PR", "AbortError"));
    await assert.rejects(
      running.catch((cause) => {
        for (const pid of [pids.root, pids.ignored, pids.inherited]) {
          assertProcessDead(pid);
        }
        throw cause;
      }),
      (cause) => cause?.name === "AbortError" && cause.message === "superseded PR",
    );
    await Promise.all(
      [pids.root, pids.ignored, pids.inherited].map(waitForProcessGone),
    );
  } finally {
    if (!controller.signal.aborted) {
      controller.abort(new DOMException("test cleanup", "AbortError"));
    }
    await running?.catch(() => undefined);
    for (const pid of [pids?.root, pids?.ignored, pids?.inherited]) {
      if (!Number.isSafeInteger(pid) || pid <= 0) continue;
      try {
        process.kill(pid, "SIGKILL");
      } catch (cause) {
        if (cause?.code !== "ESRCH") throw cause;
      }
    }
    await rm(directory, { recursive: true, force: true });
  }
});

test("PR publisher rejects successful leader exit with a TERM-ignoring detached-stdio descendant", async () => {
  if (process.platform === "win32") return;
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-pr-success-group-"));
  const pidPath = resolve(directory, "processes.json");
  const controller = new AbortController();
  let processes;
  let running;
  try {
    running = runProcess(
      process.execPath,
      ["--eval", leaderWithTermIgnoringDescendant(pidPath)],
      {
        cwd: directory,
        env: { LANG: "C.UTF-8", PATH: process.env.PATH ?? "/usr/bin:/bin" },
        signal: controller.signal,
        timeoutMs: 10_000,
      },
    );
    processes = await waitForProcessFixture(pidPath);
    await waitForProcessGone(processes.leader);
    assertProcessAlive(processes.descendant);
    await assert.rejects(
      running.catch((cause) => {
        assertProcessDead(processes.descendant);
        throw cause;
      }),
      /left a live detached process-group descendant/,
    );
    await waitForProcessGone(processes.descendant);
  } finally {
    controller.abort(new DOMException("test cleanup", "AbortError"));
    await running?.catch(() => undefined);
    forceKillProcess(processes?.leader);
    forceKillProcess(processes?.descendant);
    await rm(directory, { recursive: true, force: true });
  }
});

test("PR supersession after leader exit wins and awaits descendant group cleanup", async () => {
  if (process.platform === "win32") return;
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-pr-exit-abort-"));
  const pidPath = resolve(directory, "processes.json");
  const controller = new AbortController();
  let processes;
  let running;
  try {
    running = runProcess(
      process.execPath,
      ["--eval", leaderWithTermIgnoringDescendant(pidPath)],
      {
        cwd: directory,
        env: { LANG: "C.UTF-8", PATH: process.env.PATH ?? "/usr/bin:/bin" },
        signal: controller.signal,
        timeoutMs: 10_000,
      },
    );
    processes = await waitForProcessFixture(pidPath);
    await waitForProcessGone(processes.leader);
    assertProcessAlive(processes.descendant);
    controller.abort(new DOMException("superseded PR after leader exit", "AbortError"));
    await assert.rejects(
      running.catch((cause) => {
        assertProcessDead(processes.descendant);
        throw cause;
      }),
      (cause) =>
        cause?.name === "AbortError" &&
        cause.message === "superseded PR after leader exit",
    );
    await waitForProcessGone(processes.descendant);
  } finally {
    controller.abort(new DOMException("test cleanup", "AbortError"));
    await running?.catch(() => undefined);
    forceKillProcess(processes?.leader);
    forceKillProcess(processes?.descendant);
    await rm(directory, { recursive: true, force: true });
  }
});

test("an unconfirmed write-ahead status never suppresses pending on a new merge", async () => {
  const calls = [];
  const identity = {
    number: 1,
    pullRequestHead: headOne,
    mergeHead: headTwo,
  };
  await runPullRequestControllerCycle({
    loadState: async () => [{ ...identity, status: null }],
    readMasterSource: async () => ({
      head: headFour,
      rustSecRevision,
    }),
    listOpenPullRequests: async () => [identity],
    preparePullRequest: async () => ({
      ...identity,
      baseHead: headFour,
      repository: "/scratch/pr-1",
      cleanup: async () => calls.push("cleanup"),
    }),
    readLane: async () => ({ type: "open", ...identity }),
    assertFresh: async () => calls.push("fresh"),
    updateStatus: async (_identity, status) => calls.push("status:" + status),
    readRun: async () => ({ outcome: "pending" }),
    closeLane: async () => true,
    saveState: async () => calls.push("save"),
  });
  assert.deepEqual(calls, ["save", "fresh", "status:pending", "cleanup", "save"]);
});

test("a stale terminal catch never publishes success, failure, or error", async () => {
  const identity = {
    number: 1,
    pullRequestHead: headOne,
    mergeHead: headTwo,
  };
  const statuses = [];
  let freshnessChecks = 0;
  let saved;
  const result = await runPullRequestControllerCycle({
    loadState: async () => [{ ...identity, status: "pending" }],
    readMasterSource: async () => ({
      head: headFour,
      rustSecRevision,
    }),
    listOpenPullRequests: async () => [identity],
    preparePullRequest: async () => ({
      ...identity,
      baseHead: headFour,
      repository: "/scratch/pr-1",
      cleanup: async () => undefined,
    }),
    readLane: async () => ({ type: "open", ...identity }),
    assertFresh: async (_pullRequest, checkout) => {
      freshnessChecks += 1;
      if (freshnessChecks === 2) {
        throw new StalePullRequestIdentityError(
          checkout,
          "the terminal GitHub status freshness gate",
        );
      }
    },
    updateStatus: async (_identity, status) => statuses.push(status),
    readRun: async () => ({ outcome: "success" }),
    closeLane: async () => true,
    saveState: async (entries) => {
      saved = structuredClone(entries);
    },
  });
  assert.equal(freshnessChecks, 2);
  assert.deepEqual(statuses, []);
  assert.deepEqual(result.reports, []);
  assert.equal(result.errors.length, 1);
  assert.ok(result.errors[0] instanceof StalePullRequestIdentityError);
  assert.deepEqual(saved, [{ ...identity, status: "pending" }]);
});

test("PR publication waits for the merge base to be the current published master", async () => {
  const calls = [];
  const result = await runPullRequestControllerCycle({
    loadState: async () => [],
    readMasterSource: async () => ({
      head: headFour,
      rustSecRevision,
    }),
    listOpenPullRequests: async () => [{
      number: 1,
      pullRequestHead: headOne,
      mergeHead: headTwo,
    }],
    preparePullRequest: async () => ({
      number: 1,
      pullRequestHead: headOne,
      mergeHead: headTwo,
      baseHead: headThree,
      repository: "/scratch/pr-1",
      cleanup: async () => calls.push("cleanup"),
    }),
    readLane: async () => {
      calls.push("lane");
      return { type: "not_published", number: 1 };
    },
    updateStatus: async (_identity, status) => calls.push("status:" + status),
    publishCargoVendor: async () => calls.push("cargo"),
    publishSource: async () => calls.push("source"),
    closeLane: async () => true,
    saveState: async () => calls.push("save"),
  });
  assert.equal(result.errors.length, 1);
  assert.ok(result.errors[0] instanceof StalePullRequestIdentityError);
  assert.match(result.errors[0].message, /current published master/);
  assert.deepEqual(calls, ["cleanup", "save"]);
});

test("controller propagates GitHub rate limits without publishing an error status", async () => {
  const calls = [];
  const retryAt = Date.now() + 60_000;
  const operations = {
    loadState: async () => [],
    readMasterSource: async () => ({
      head: headFour,
      rustSecRevision,
    }),
    listOpenPullRequests: async () => [
      { number: 1, pullRequestHead: headOne, mergeHead: headTwo },
    ],
    preparePullRequest: async () => {
      calls.push("prepare");
      throw new GitHubRateLimitError(retryAt, "refresh pull request");
    },
    updateStatus: async () => calls.push("status"),
    closeLane: async () => calls.push("close"),
    saveState: async () => calls.push("save"),
  };
  await assert.rejects(
    runPullRequestControllerCycle(operations),
    (cause) =>
      cause instanceof GitHubRateLimitError && cause.retryAt === retryAt,
  );
  assert.deepEqual(calls, ["prepare"]);

  const closeCalls = [];
  await assert.rejects(
    runPullRequestControllerCycle({
      loadState: async () => [{
        number: 3,
        pullRequestHead: headThree,
        mergeHead: headFour,
        status: "pending",
      }],
      readMasterSource: async () => ({
        head: headFour,
        rustSecRevision,
      }),
      listOpenPullRequests: async () => [],
      closeLane: async () => {
        closeCalls.push("close");
        throw new GitHubRateLimitError(retryAt, "confirm closed PR");
      },
      saveState: async () => closeCalls.push("save"),
    }),
    (cause) =>
      cause instanceof GitHubRateLimitError && cause.retryAt === retryAt,
  );
  assert.deepEqual(closeCalls, ["close"]);
});

test("GitHub rate-limit classification distinguishes permission failures and backs off", async () => {
  const before = Date.now();
  await assert.rejects(
    throwGitHubResponseFailure(
      "list pull requests",
      Response.json(
        { message: "secondary rate limit" },
        { status: 403 },
      ),
    ),
    (cause) =>
      cause instanceof GitHubRateLimitError &&
      cause.retryAt >= before + 60_000,
  );
  await assert.rejects(
    throwGitHubResponseFailure(
      "publish status",
      Response.json(
        { message: "Resource not accessible by integration" },
        {
          status: 403,
          headers: {
            "x-ratelimit-remaining": "42",
            "x-ratelimit-reset": String(
              Math.floor(Date.now() / 1_000) + 3_600,
            ),
          },
        },
      ),
    ),
    (cause) =>
      !(cause instanceof GitHubRateLimitError) &&
      /Resource not accessible/.test(cause.message),
  );
});

test("close reconciliation rechecks GitHub and CAS-closes only the persisted lane", async () => {
  const entry = {
    number: 7,
    pullRequestHead: headOne,
    mergeHead: headTwo,
    status: "success",
  };
  const config = { sourceToken: "source-secret" };
  let fetched = false;
  const stillOpen = await closePullRequestLane(
    config,
    async () => {
      throw new Error("lane must not be read while GitHub still reports open");
    },
    async () => ({ state: "open", basedOnMaster: true }),
    entry,
    undefined,
    async () => {
      fetched = true;
      throw new Error("close must not be sent");
    },
  );
  assert.equal(stillOpen, false);
  assert.equal(fetched, false);

  await assert.rejects(
    closePullRequestLane(
      config,
      async () => ({
        type: "open",
        number: 7,
        pullRequestHead: headThree,
        mergeHead: headFour,
      }),
      async () => ({ state: "closed", basedOnMaster: true }),
      entry,
      undefined,
      async () => {
        throw new Error("newer lane must not be closed");
      },
    ),
    /newer than the persisted generation/,
  );

  let closeBody;
  const closed = await closePullRequestLane(
    config,
    async () => ({
      type: "open",
      number: 7,
      pullRequestHead: headOne,
      mergeHead: headTwo,
    }),
    async () => ({ state: "closed", basedOnMaster: true }),
    {
      number: 7,
      pullRequestHead: headThree,
      mergeHead: headFour,
      status: null,
      previousLane: {
        pullRequestHead: headOne,
        mergeHead: headTwo,
      },
    },
    undefined,
    async (_config, path, init) => {
      assert.equal(path, "/api/ci/source/pull-requests/7/state");
      assert.equal(init.method, "DELETE");
      assert.equal(init.headers.authorization, "Bearer source-secret");
      closeBody = JSON.parse(init.body);
      return Response.json({
        closed: true,
        number: 7,
        closeId: closeBody.closeId,
        mergeHead: headTwo,
        pullRequestHead: headOne,
        closedAt: "2026-08-22T00:00:00.000Z",
        replay: false,
      });
    },
  );
  assert.equal(closed, true);
  assert.deepEqual(
    {
      expectedMergeHead: closeBody.expectedMergeHead,
      expectedPullRequestHead: closeBody.expectedPullRequestHead,
    },
    { expectedMergeHead: headTwo, expectedPullRequestHead: headOne },
  );
  assert.match(closeBody.closeId, /^[a-f0-9-]{36}$/);
});

test("controller state and exact ref parsing reject ambiguity", () => {
  const entries = parseControllerState({
    version: 1,
    repository: "gakonst/nanocodex",
    pullRequests: [
      {
        number: 7,
        pullRequestHead: headOne,
        mergeHead: headTwo,
        status: "pending",
      },
    ],
  });
  assert.equal(entries[0].number, 7);
  assert.throws(
    () =>
      parseControllerState({
        version: 1,
        repository: "gakonst/nanocodex",
        pullRequests: [entries[0], entries[0]],
      }),
    /invalid lane/,
  );
  const refs = parseLsRemote(
    headOne +
      "\trefs/pull/7/head\n" +
      headTwo +
      "\trefs/pull/7/merge\n",
    ["refs/pull/7/head", "refs/pull/7/merge"],
  );
  assert.equal(refs.get("refs/pull/7/head"), headOne);
  assert.throws(
    () =>
      parseLsRemote(
        headOne + "\trefs/pull/8/head\n",
        ["refs/pull/7/head"],
      ),
    /unexpected output/,
  );

  const configuration = controllerConfiguration({
    NANOCODEX_CI_ORIGIN: "https://ci.example.test",
    NANOCODEX_CI_TOKEN: "source",
    NANOCODEX_GITHUB_STATUS_TOKEN: "github",
    NANOCODEX_RUSTSEC_REVISION: rustSecRevision,
    NANOCODEX_CI_PR_STATE_PATH: "/var/lib/nanocodex/pr-state.json",
    NANOCODEX_CI_PR_PREP_USER: "nanocodex_ci_prep",
  });
  assert.equal(configuration.sourceToken, "source");
  assert.equal(configuration.githubToken, "github");
  assert.equal(configuration.identityPollMs, 5_000);
  assert.ok(!configuration.scratchRoot.startsWith(resolve(import.meta.dirname, "../..")));
  assert.equal(configuration.controlToken, undefined);
  assert.throws(
    () =>
      controllerConfiguration({
        NANOCODEX_CI_ORIGIN: "https://ci.example.test",
        NANOCODEX_CI_TOKEN: "same-secret",
        NANOCODEX_GITHUB_STATUS_TOKEN: "same-secret",
        NANOCODEX_RUSTSEC_REVISION: rustSecRevision,
        NANOCODEX_CI_PR_STATE_PATH: "/var/lib/nanocodex/pr-state.json",
        NANOCODEX_CI_PR_PREP_USER: "nanocodex_ci_prep",
      }),
    /must be distinct/,
  );
  for (const [name, value] of [
    [
      "NANOCODEX_CI_PR_STATE_PATH",
      resolve(import.meta.dirname, "../controller-state.json"),
    ],
    [
      "NANOCODEX_CI_PR_TMPDIR",
      resolve(import.meta.dirname, "../controller-scratch"),
    ],
  ]) {
    assert.throws(
      () =>
        controllerConfiguration({
          NANOCODEX_CI_ORIGIN: "https://ci.example.test",
          NANOCODEX_CI_TOKEN: "source",
          NANOCODEX_GITHUB_STATUS_TOKEN: "github",
          NANOCODEX_RUSTSEC_REVISION: rustSecRevision,
          NANOCODEX_CI_PR_STATE_PATH: "/var/lib/nanocodex/pr-state.json",
          NANOCODEX_CI_PR_PREP_USER: "nanocodex_ci_prep",
          [name]: value,
        }),
      /outside the trusted repository/,
    );
  }
  assert.throws(
    () =>
      controllerConfiguration({
        NANOCODEX_CI_ORIGIN: "https://ci.example.test",
        NANOCODEX_CI_TOKEN: "source",
        NANOCODEX_GITHUB_STATUS_TOKEN: "github",
        NANOCODEX_CI_PR_STATE_PATH: "/var/lib/nanocodex/pr-state.json",
        NANOCODEX_CI_PR_PREP_USER: "nanocodex_ci_prep",
        GH_TOKEN: "ambient-github",
      }),
    /ambient GitHub authorities/,
  );
  assert.throws(
    () =>
      controllerConfiguration({
        NANOCODEX_CI_ORIGIN: "https://ci.example.test",
        NANOCODEX_CI_TOKEN: "source",
        NANOCODEX_GITHUB_STATUS_TOKEN: "github",
        NANOCODEX_RUSTSEC_REVISION: rustSecRevision,
        NANOCODEX_CI_PR_STATE_PATH: "/var/lib/nanocodex/pr-state.json",
        NANOCODEX_CI_PR_PREP_USER: "nanocodex_ci_prep",
        NANOCODEX_CI_PR_IDENTITY_POLL_MS: "249",
      }),
    /between 250 and 60000/,
  );
});

async function exerciseMonitoredCancellation(phase) {
  const identity = {
    number: 7,
    baseHead: headFour,
    pullRequestHead: headOne,
    mergeHead: headTwo,
  };
  let checks = 0;
  let operationStarted = false;
  let operationAborted = false;
  let cleanupFinished = false;
  await assert.rejects(
    runWhilePullRequestIsCurrent(
      identity,
      async (signal) => {
        operationStarted = true;
        await new Promise((_, rejectPromise) => {
          const abort = () => {
            operationAborted = true;
            setTimeout(() => {
              cleanupFinished = true;
              rejectPromise(signal.reason);
            }, 10);
          };
          signal.addEventListener("abort", abort, { once: true });
          if (signal.aborted) abort();
        });
      },
      {
        assertCurrent: async (observed, _signal, observedPhase) => {
          assert.deepEqual(observed, identity);
          assert.equal(observedPhase, phase);
          checks += 1;
          if (checks === 2) {
            throw new StalePullRequestIdentityError(observed, phase);
          }
        },
        phase,
        pollMs: 1,
      },
    ),
    (cause) =>
      cause instanceof StalePullRequestIdentityError &&
      cause.phase === phase,
  );
  return { checks, operationAborted, operationStarted, cleanupFinished };
}

function leaderWithTermIgnoringDescendant(pidPath) {
  const descendant = [
    'const {writeFileSync}=require("node:fs");',
    'process.on("SIGTERM",()=>{});',
    `writeFileSync(${JSON.stringify(pidPath)},JSON.stringify({leader:process.ppid,descendant:process.pid}));`,
    "setInterval(()=>{},1000);",
  ].join("");
  return [
    'const {spawn}=require("node:child_process");',
    'const {existsSync}=require("node:fs");',
    `const descendant=spawn(process.execPath,["--eval",${JSON.stringify(descendant)}],{stdio:"ignore"});`,
    "descendant.unref();",
    "const deadline=Date.now()+5000;",
    "const ready=setInterval(()=>{",
    `if(existsSync(${JSON.stringify(pidPath)})){clearInterval(ready);}`,
    "else if(Date.now()>=deadline){clearInterval(ready);process.exitCode=97;}",
    "},5);",
  ].join("");
}

async function waitForProcessFixture(path) {
  for (let attempt = 0; attempt < 500; attempt += 1) {
    try {
      const value = JSON.parse(await readFile(path, "utf8"));
      if (
        Number.isSafeInteger(value?.leader) && value.leader > 0 &&
        Number.isSafeInteger(value?.descendant) && value.descendant > 0
      ) return value;
    } catch {}
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 10));
  }
  throw new Error("process-group fixture did not become ready");
}

async function waitForProcessGone(pid) {
  for (let attempt = 0; attempt < 500; attempt += 1) {
    try {
      process.kill(pid, 0);
    } catch (cause) {
      if (cause?.code === "ESRCH") return;
      throw cause;
    }
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 10));
  }
  throw new Error(`process ${pid} remained alive`);
}

function assertProcessAlive(pid) {
  assert.doesNotThrow(() => process.kill(pid, 0));
}

function assertProcessDead(pid) {
  try {
    process.kill(pid, 0);
  } catch (cause) {
    if (cause?.code === "ESRCH") return;
    throw cause;
  }
  let state;
  try {
    state = execFileSync("/bin/ps", ["-o", "state=", "-p", String(pid)], {
      encoding: "utf8",
    }).trim();
  } catch (cause) {
    if (cause?.status === 1) return;
    throw cause;
  }
  assert.match(state, /^Z/, `process ${pid} remained live in state ${state}`);
}

function forceKillProcess(pid) {
  if (!Number.isSafeInteger(pid) || pid <= 0) return;
  try {
    process.kill(pid, "SIGKILL");
  } catch (cause) {
    if (cause?.code !== "ESRCH") throw cause;
  }
}

function githubPullRequest(number, pullRequestHead, mergeHead) {
  return {
    number,
    state: "open",
    head: { sha: pullRequestHead },
    base: {
      ref: "master",
      repo: { full_name: "gakonst/nanocodex" },
    },
    merge_commit_sha: mergeHead,
  };
}

function laneState(number, pullRequestHead, mergeHead) {
  return {
    publication: {
      version: 1,
      head: mergeHead,
      branch: "pull/" + number + "/merge",
      ref: "refs/pull/" + number + "/merge",
      lane: { type: "pull_request", number, pullRequestHead },
    },
    run: {
      version: 1,
      head: mergeHead,
      workflowId: "ci-" + mergeHead,
      state: "dispatched",
      publishedAt: "2026-08-22T00:00:00.000Z",
    },
  };
}

function runState(head, workflowStatus, resultStatus) {
  return {
    version: 1,
    head,
    workflowId: "ci-" + head,
    state: "dispatched",
    workflow: { status: workflowStatus },
    result: {
      version: 1,
      head,
      workflowId: "ci-" + head,
      status: resultStatus,
    },
  };
}

function masterSourceState() {
  return {
    publication: {
      version: 1,
      head: headFour,
      branch: "master",
      ref: "refs/heads/master",
      lane: { type: "master" },
      rustSecRevision,
      rustSec: {
        key:
          "rustsec-advisory-db/" + rustSecRevision + "/bundle.tar.gz",
        size: 1024,
        sha256: "a".repeat(64),
      },
      publishedAt: "2026-08-22T00:00:00.000Z",
    },
    run: {
      version: 1,
      head: headFour,
      workflowId: "ci-" + headFour,
      state: "dispatched",
      publishedAt: "2026-08-22T00:00:00.000Z",
    },
  };
}

function authorityKeys(env) {
  return Object.keys(env)
    .filter((name) => /(?:TOKEN|SECRET|API_KEY)$/.test(name))
    .sort();
}

function shaFor(value) {
  return value.toString(16).padStart(40, "0");
}
