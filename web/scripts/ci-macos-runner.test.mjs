import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import { mkdir, mkdtemp, open, readFile, realpath, rm, stat, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { arch, platform, tmpdir, userInfo } from "node:os";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { gzipSync } from "node:zlib";

import {
  BoundedLogCapture,
  MacCiApi,
  createSandboxProfile,
  downloadVerifiedArchive,
  heartbeatLoop,
  leaseRequestTimeout,
  parseOrigin,
  preflightTarArchive,
  resolveSandboxRuntime,
  resolveTask,
  runCapturedProcess,
  sandboxEnvironment,
  validateThinArm64MachO,
  writeSandboxProfile,
} from "./ci-macos-runner.mjs";

test("bounded capture retains exact head and tail with observed-byte metadata", () => {
  const capture = new BoundedLogCapture({ headBytes: 4, tailBytes: 3 });
  capture.append(Buffer.from("01"));
  capture.append(Buffer.from("234567"));
  capture.append(Buffer.from("89"));
  const result = capture.result();
  assert.equal(
    result.body.toString("utf8"),
    "0123\n[... nanocodex macOS CI omitted 3 log bytes ...]\n789",
  );
  assert.equal(result.bytesObserved, 10);
  assert.equal(result.bytesStored, result.body.byteLength);
  assert.equal(result.truncated, true);
  assert.equal(result.sha256, sha256(result.body));

  const short = new BoundedLogCapture({ headBytes: 4, tailBytes: 3 });
  short.append("hello");
  assert.equal(short.result().body.toString("utf8"), "hello");
  assert.equal(short.result().truncated, false);
  assert.throws(
    () => new BoundedLogCapture({ headBytes: 32 * 1024 * 1024, tailBytes: 32 * 1024 * 1024 }),
    /truncation marker/,
  );
});

test("sequential native phases retain one ordered bounded output stream", async () => {
  const capture = Object.freeze({
    stdout: new BoundedLogCapture({ headBytes: 1_024, tailBytes: 1_024 }),
    stderr: new BoundedLogCapture({ headBytes: 1_024, tailBytes: 1_024 }),
  });
  const run = (text) => runCapturedProcess({
    executable: process.execPath,
    arguments: ["-e", `process.stdout.write(${JSON.stringify(`${text}\n`)})`],
    cwd: tmpdir(),
    env: { PATH: process.env.PATH ?? "" },
    timeoutMs: 5_000,
    killGraceMs: 25,
    capture,
  });
  assert.equal((await run("workspace-test")).exitCode, 0);
  const built = await run("nightly-build");
  assert.equal(built.exitCode, 0);
  assert.equal(built.stdout.body.toString("utf8"), "workspace-test\nnightly-build\n");
});

test("task resolution accepts only the hardcoded task allowlist", () => {
  assert.deepEqual(resolveTask("workspace-test").arguments, [
    "test",
    "--workspace",
    "--locked",
  ]);
  const release = resolveTask(
    "release-build",
    {
      channel: "stable",
      tagName: "v1.2.3",
      buildTimestamp: "2026-08-22T00:00:00.000Z",
    },
    "a".repeat(40),
  );
  assert.deepEqual(release.arguments, [
    "build",
    "--locked",
    "--release",
    "--package",
    "nanocodex-bin",
    "--bin",
    "nanocodex",
    "--features",
    "tempo",
    "--target",
    "aarch64-apple-darwin",
  ]);
  assert.deepEqual(release.environment, {
    TAG_NAME: "v1.2.3",
    VERGEN_GIT_SHA: "a".repeat(40),
    VERGEN_BUILD_TIMESTAMP: "2026-08-22T00:00:00.000Z",
  });
  const publishedAt = "2026-08-22T01:02:03.004Z";
  const native = resolveTask("native-build", undefined, "b".repeat(40), publishedAt);
  assert.equal(native.dependencyArguments, undefined);
  assert.deepEqual(native.testArguments, [
    "test",
    "--workspace",
    "--locked",
    "--offline",
  ]);
  assert.deepEqual(native.arguments, [
    "build",
    "--locked",
    "--offline",
    "--profile",
    "nightly",
    "--package",
    "nanocodex-bin",
    "--bin",
    "nanocodex",
    "--features",
    "tempo",
    "--target",
    "aarch64-apple-darwin",
  ]);
  assert.deepEqual(native.environment, {
    TAG_NAME: "pr",
    VERGEN_GIT_SHA: "b".repeat(40),
    VERGEN_BUILD_TIMESTAMP: publishedAt,
  });
  assert.equal(native.networkAccess, false);
  assert.equal(native.cargoOffline, true);
  assert.equal(native.asset.path, "target/aarch64-apple-darwin/nightly/nanocodex");
  assert.throws(
    () => resolveTask("native-build", undefined, "b".repeat(40), "2026-08-22T01:02:03Z"),
    /canonical source identity/,
  );
  assert.throws(
    () => resolveTask(
      "native-build",
      { channel: "nightly", tagName: "nightly", buildTimestamp: publishedAt },
      "b".repeat(40),
      publishedAt,
    ),
    /canonical source identity/,
  );
  assert.throws(
    () => resolveTask(
      "release-build",
      {
        channel: "stable",
        tagName: "unsafe",
        buildTimestamp: "2026-08-22T00:00:00.000Z",
      },
      "a".repeat(40),
    ),
    /canonical release identity/,
  );
  assert.throws(() => resolveTask("ci-test"), /unsupported macOS CI task/);
  assert.throws(() => resolveTask("cargo test --workspace"), /unsupported macOS CI task/);
  assert.throws(
    () => resolveTask({ executable: "sh", arguments: ["-c", "echo unsafe"] }),
    /unsupported macOS CI task/,
  );
});

test("native claim accepts only the immutable server-owned build identity", async () => {
  const head = "b".repeat(40);
  const leaseExpiresAt = new Date(Date.now() + 120_000).toISOString();
  const response = nativeClaim({ head, leaseExpiresAt });
  const api = new MacCiApi({
    origin: "https://ci.test",
    token: "runner-token",
    fetchImpl: async () => Response.json(response),
  });
  const claimed = await api.claim(
    "runner-1",
    { hostname: "mac.test", platform: "darwin", arch: "arm64" },
  );
  assert.equal(claimed.action, "run");
  assert.equal(claimed.job.task, "native-build");
  assert.equal(claimed.job.publishedAt, "2026-08-22T01:02:03.004Z");
  assert.equal(claimed.job.source.url, `https://ci.test/api/ci/source/${head}/archive`);
  assert.equal(claimed.job.cargoVendor.size, 189_626_590);

  const mismatchedVendor = new MacCiApi({
    origin: "https://ci.test",
    token: "runner-token",
    fetchImpl: async () => Response.json({
      ...response,
      job: {
        ...response.job,
        cargoVendor: { ...response.job.cargoVendor, sha256: "e".repeat(64) },
      },
    }),
  });
  await assert.rejects(
    mismatchedVendor.claim(
      "runner-1",
      { hostname: "mac.test", platform: "darwin", arch: "arm64" },
    ),
    /Cargo vendor URL is not canonical/,
  );

  for (const unsafeJob of [
    { ...response.job, command: "/bin/sh" },
    { ...response.job, environment: { TAG_NAME: "attacker" } },
    (() => {
      const { publishedAt: _publishedAt, ...job } = response.job;
      return { ...job, buildTimestamp: "2026-08-22T01:02:03.004Z" };
    })(),
  ]) {
    const unsafe = new MacCiApi({
      origin: "https://ci.test",
      token: "runner-token",
      fetchImpl: async () => Response.json({ ...response, job: unsafeJob }),
    });
    await assert.rejects(
      unsafe.claim(
        "runner-1",
        { hostname: "mac.test", platform: "darwin", arch: "arm64" },
      ),
      /unsupported job policy|canonical source identity/,
    );
  }
  const oversized = new MacCiApi({
    origin: "https://ci.test",
    token: "runner-token",
    fetchImpl: async () => Response.json({
      ...response,
      job: {
        ...response.job,
        cargoVendor: { ...response.job.cargoVendor, size: 256 * 1024 * 1024 + 1 },
      },
    }),
  });
  await assert.rejects(
    oversized.claim(
      "runner-1",
      { hostname: "mac.test", platform: "darwin", arch: "arm64" },
    ),
    /invalid macOS CI Cargo vendor archive descriptor/,
  );
});

test("heartbeat requires and carries a canonical renewed lease deadline", async () => {
  const now = Date.now();
  const acknowledged = new Date(now + 120_000).toISOString();
  const renewed = new Date(now + 240_000).toISOString();
  const api = new MacCiApi({
    origin: "https://ci.test",
    token: "runner-token",
    fetchImpl: async (_url, init) => {
      assert.equal(init.method, "POST");
      assert.equal(init.signal.aborted, false);
      return new Response(null, {
        status: 204,
        headers: { "x-nanocodex-lease-expires-at": renewed },
      });
    },
  });
  assert.deepEqual(
    await api.heartbeat("claim-1", "runner-1", {
      leaseDeadlineMs: Date.parse(acknowledged),
      safetyMarginMs: 10_000,
    }),
    {
      action: "continue",
      leaseExpiresAt: renewed,
      leaseDeadlineMs: Date.parse(renewed),
    },
  );
  assert.equal(leaseRequestTimeout(now + 20_000, 10_000, now), 10_000);
  assert.equal(leaseRequestTimeout(now + 120_000, 10_000, now), 60_000);
  assert.throws(
    () => leaseRequestTimeout(now + 10_000, 10_000, now),
    /no safe heartbeat request budget/,
  );

  for (const header of [null, "2026-08-22T01:02:03Z", acknowledged]) {
    const invalid = new MacCiApi({
      origin: "https://ci.test",
      token: "runner-token",
      fetchImpl: async () => new Response(null, {
        status: 204,
        ...(header == null
          ? {}
          : { headers: { "x-nanocodex-lease-expires-at": header } }),
      }),
    });
    await assert.rejects(
      invalid.heartbeat("claim-1", "runner-1", {
        leaseDeadlineMs: Date.parse(acknowledged),
        safetyMarginMs: 10_000,
      }),
      /canonical UTC RFC3339|did not renew/,
    );
  }
});

test("heartbeat loop passes the last acknowledged deadline to each renewal", async () => {
  const now = Date.parse("2026-08-22T00:00:00.000Z");
  const initial = "2026-08-22T00:02:00.000Z";
  const renewed = "2026-08-22T00:02:30.000Z";
  const observed = [];
  const cancellations = [];
  await heartbeatLoop({
    api: {
      async heartbeat(_claim, _worker, options) {
        observed.push(options.leaseDeadlineMs);
        if (observed.length === 1) {
          return {
            action: "continue",
            leaseExpiresAt: renewed,
            leaseDeadlineMs: Date.parse(renewed),
          };
        }
        return { action: "cancel", reason: "test complete" };
      },
    },
    claim: "claim-1",
    worker: "runner-1",
    leaseExpiresAt: initial,
    intervalMs: 30_000,
    safetyMarginMs: 10_000,
    now: () => now,
    wait: async () => {},
    setTimer: () => 1,
    clearTimer: () => {},
    onCancel: (reason) => cancellations.push(reason),
  });
  assert.deepEqual(observed, [Date.parse(initial), Date.parse(renewed)]);
  assert.deepEqual(cancellations, ["test complete"]);
});

test("source download enforces the declared byte count and SHA-256", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nanocodex-ci-macos-download-"));
  const archive = Buffer.from("verified source archive fixture");
  const digest = sha256(archive);
  const server = createServer((_request, response) => {
    response.writeHead(200, {
      "content-length": String(archive.byteLength),
      "content-type": "application/gzip",
    });
    response.end(archive);
  });
  await listen(server);
  const address = server.address();
  assert(address && typeof address === "object");
  const url = `${parseOrigin(`http://127.0.0.1:${address.port}`)}/source.tar.gz`;
  try {
    const destination = join(directory, "source.tar.gz");
    assert.deepEqual(
      await downloadVerifiedArchive({
        url,
        destination,
        size: archive.byteLength,
        sha256: digest,
      }),
      { size: archive.byteLength, sha256: digest },
    );
    assert.deepEqual(await readFile(destination), archive);

    const invalid = join(directory, "invalid.tar.gz");
    await assert.rejects(
      downloadVerifiedArchive({
        url,
        destination: invalid,
        size: archive.byteLength,
        sha256: "0".repeat(64),
      }),
      /SHA-256 mismatch/,
    );
    await assert.rejects(stat(invalid), { code: "ENOENT" });
  } finally {
    server.closeAllConnections();
    await new Promise((resolvePromise, rejectPromise) =>
      server.close((error) => error ? rejectPromise(error) : resolvePromise())
    );
    await rm(directory, { recursive: true, force: true });
  }
});

test("tar preflight accepts the Cargo-home shape and rejects path and symlink escape", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nanocodex-ci-macos-tar-"));
  try {
    const safe = join(directory, "safe.tar.gz");
    await writeFile(safe, gzipSync(tarArchive([
      { path: "config.toml", body: Buffer.from('[source.vendored-sources]\ndirectory = "/workspace/.cargo-home/vendor"\n') },
      { path: "vendor/", type: "directory" },
      { path: "vendor/example/", type: "directory" },
      { path: "vendor/example/Cargo.toml", body: Buffer.from("[package]\nname='example'\n") },
    ])));
    assert.deepEqual(
      await preflightTarArchive({
        archive: safe,
        kind: "cargo-vendor",
        maximumExpandedBytes: 1024 * 1024,
      }),
      { entries: 4, expandedBytes: 4_096 },
    );

    const escapedPath = join(directory, "escaped-path.tar.gz");
    await writeFile(escapedPath, gzipSync(tarArchive([
      { path: "config.toml", body: Buffer.from("config") },
      { path: "vendor/", type: "directory" },
      { path: "../outside", body: Buffer.from("escape") },
    ])));
    await assert.rejects(
      preflightTarArchive({
        archive: escapedPath,
        kind: "cargo-vendor",
        maximumExpandedBytes: 1024 * 1024,
      }),
      /path escapes|unsafe|not canonical/,
    );

    const escapedLink = join(directory, "escaped-link.tar.gz");
    await writeFile(escapedLink, gzipSync(tarArchive([
      { path: "config.toml", body: Buffer.from("config") },
      { path: "vendor/", type: "directory" },
      { path: "vendor/escape", type: "symlink", link: "../../../outside" },
    ])));
    await assert.rejects(
      preflightTarArchive({
        archive: escapedLink,
        kind: "cargo-vendor",
        maximumExpandedBytes: 1024 * 1024,
      }),
      /symlink escapes/,
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("release asset validation accepts only a complete thin 64-bit arm64 Mach-O", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nanocodex-ci-macos-macho-"));
  const validate = async (name, body) => {
    const path = join(directory, name);
    await writeFile(path, body);
    const file = await open(path, "r");
    try {
      return await validateThinArm64MachO(file, body.byteLength);
    } finally {
      await file.close();
    }
  };
  try {
    const valid = thinArm64MachO();
    assert.deepEqual(await validate("valid", valid), {
      architecture: "arm64",
      bits: 64,
      format: "Mach-O",
    });

    const fat = Buffer.from(valid);
    Buffer.from("cafebabe", "hex").copy(fat, 0);
    await assert.rejects(validate("fat", fat), /must not be a fat Mach-O/);

    const wrongCpu = Buffer.from(valid);
    wrongCpu.writeUInt32LE(0x01000007, 4);
    await assert.rejects(validate("wrong-cpu", wrongCpu), /not arm64 Mach-O/);

    await assert.rejects(
      validate("truncated-header", valid.subarray(0, 20)),
      /truncated Mach-O header/,
    );
    await assert.rejects(
      validate("truncated-commands", valid.subarray(0, 40)),
      /truncated Mach-O load commands/,
    );

    const truncatedPayload = Buffer.from(valid);
    truncatedPayload.writeBigUInt64LE(BigInt(valid.byteLength + 1), 32 + 48);
    await assert.rejects(
      validate("truncated-payload", truncatedPayload),
      /truncated Mach-O segment payload/,
    );

    const truncatedSection = Buffer.from(valid);
    truncatedSection.writeBigUInt64LE(2n, 32 + 72 + 40);
    await assert.rejects(
      validate("truncated-section", truncatedSection),
      /truncated Mach-O section payload/,
    );

    const truncatedRelocations = Buffer.from(valid);
    truncatedRelocations.writeUInt32LE(valid.byteLength - 4, 32 + 72 + 56);
    truncatedRelocations.writeUInt32LE(1, 32 + 72 + 60);
    await assert.rejects(
      validate("truncated-relocations", truncatedRelocations),
      /truncated Mach-O relocation table/,
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("darwin sandbox hides operator state and runs only the pinned temporary Cargo environment", {
  skip: platform() !== "darwin" || arch() !== "arm64",
  timeout: 30_000,
}, async () => {
  const ambientHome = process.env.HOME;
  process.env.HOME = "/tmp/untrusted-runner-home";
  const runtime = await resolveSandboxRuntime();
  if (ambientHome === undefined) delete process.env.HOME;
  else process.env.HOME = ambientHome;
  assert.equal(runtime.realHome, await realpath(userInfo().homedir));
  const directory = await realpath(await mkdtemp(join(tmpdir(), "nanocodex-ci-macos-sandbox-")));
  const workspace = join(directory, "workspace");
  const home = join(directory, "home");
  const cargoHome = join(directory, "cargo-home");
  const temporaryDirectory = join(directory, "tmp");
  const profilePath = join(dirname(directory), `nanocodex-sandbox-${randomUUID()}.sb`);
  const nativeProfilePath = join(
    dirname(directory),
    `nanocodex-native-sandbox-${randomUUID()}.sb`,
  );
  const outside = join(dirname(directory), `nanocodex-outside-${randomUUID()}`);
  const sentinel = join(runtime.realHome, `.nanocodex-sandbox-sentinel-${randomUUID()}`);
  const previousRunnerToken = process.env.NANOCODEX_CI_MACOS_TOKEN;
  const previousApiToken = process.env.OPENAI_API_KEY;
  try {
    await Promise.all([
      mkdir(join(workspace, "src"), { recursive: true, mode: 0o700 }),
      mkdir(home, { mode: 0o700 }),
      mkdir(cargoHome, { mode: 0o700 }),
      mkdir(temporaryDirectory, { mode: 0o700 }),
      writeFile(sentinel, "operator-home-secret", { flag: "wx", mode: 0o600 }),
    ]);
    await Promise.all([
      writeFile(
        join(workspace, "Cargo.toml"),
        '[package]\nname = "sandbox-probe"\nversion = "0.1.0"\nedition = "2024"\n',
      ),
      writeFile(join(workspace, "src/main.rs"), "fn main() {}\n"),
    ]);
    const profile = createSandboxProfile({ jobDirectory: directory, runtime });
    const nativeProfile = createSandboxProfile({
      jobDirectory: directory,
      runtime,
      networkAccess: false,
    });
    assert.match(profile, /\(allow network\*\)/);
    assert.doesNotMatch(profile, /\(deny network\*\)/);
    assert.match(nativeProfile, /\(deny network\*\)/);
    assert.doesNotMatch(nativeProfile, /\(allow network\*\)/);
    for (const policy of [profile, nativeProfile]) {
      assert.match(policy, /\(deny process-info\*\)/);
      assert.match(policy, /\(deny process-info-setcontrol\)/);
      assert.match(policy, /SecurityServer/);
      assert.doesNotMatch(policy, /\(allow default\)/);
    }
    await Promise.all([
      writeSandboxProfile(profilePath, profile),
      writeSandboxProfile(nativeProfilePath, nativeProfile),
    ]);
    await assert.rejects(writeSandboxProfile(profilePath, profile), { code: "EEXIST" });

    process.env.NANOCODEX_CI_MACOS_TOKEN = "runner-api-secret";
    process.env.OPENAI_API_KEY = "ambient-api-secret";
    const genericEnv = sandboxEnvironment({
      jobDirectory: directory,
      homeDirectory: home,
      cargoHome,
      temporaryDirectory,
      workspace,
      runtime,
    });
    const env = sandboxEnvironment({
      jobDirectory: directory,
      homeDirectory: home,
      cargoHome,
      temporaryDirectory,
      workspace,
      runtime,
      cargoOffline: true,
    });
    assert.equal(genericEnv.CARGO_NET_OFFLINE, undefined);
    assert.equal(env.CARGO_NET_OFFLINE, "true");
    assert.equal(env.NANOCODEX_CI_MACOS_TOKEN, undefined);
    assert.equal(env.OPENAI_API_KEY, undefined);
    const run = (executable, arguments_) => runCapturedProcess({
      executable: runtime.sandboxExec,
      arguments: ["-f", nativeProfilePath, executable, ...arguments_],
      cwd: workspace,
      env,
      timeoutMs: 10_000,
      killGraceMs: 50,
      headBytes: 64 * 1024,
      tailBytes: 64 * 1024,
    });

    const readHome = await run("/bin/cat", [sentinel]);
    assert.notEqual(readHome.exitCode, 0);
    assert.doesNotMatch(readHome.stdout.body.toString("utf8"), /operator-home-secret/);

    const keychain = await run("/usr/bin/security", [
      "show-keychain-info",
      join(runtime.realHome, "Library/Keychains/login.keychain-db"),
    ]);
    assert.notEqual(keychain.exitCode, 0);

    const parentInspection = await run("/bin/ps", ["eww", "-p", String(process.pid)]);
    assert.notEqual(parentInspection.exitCode, 0);
    assert.doesNotMatch(
      parentInspection.stdout.body.toString("utf8"),
      /runner-api-secret|ambient-api-secret/,
    );

    const newSession = await run("/usr/bin/python3", [
      "-c",
      "import os; os.setsid()",
    ]);
    assert.notEqual(newSession.exitCode, 0);
    const newProcessGroup = await run("/usr/bin/python3", [
      "-c",
      "import os; os.setpgid(0, 0)",
    ]);
    assert.notEqual(newProcessGroup.exitCode, 0);

    const outsideWrite = await run("/usr/bin/touch", [outside]);
    assert.notEqual(outsideWrite.exitCode, 0);
    await assert.rejects(stat(outside), { code: "ENOENT" });

    const profileMutation = await run("/usr/bin/touch", [nativeProfilePath]);
    assert.notEqual(profileMutation.exitCode, 0);

    const environment = await run("/usr/bin/env", []);
    assert.equal(environment.exitCode, 0);
    const observedEnvironment = environment.stdout.body.toString("utf8");
    assert.doesNotMatch(observedEnvironment, /runner-api-secret|ambient-api-secret/);
    assert.match(observedEnvironment, new RegExp(`HOME=${escapeRegExp(home)}`));
    assert.match(observedEnvironment, new RegExp(`CARGO_HOME=${escapeRegExp(cargoHome)}`));
    assert.match(observedEnvironment, /CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse/);
    assert.match(observedEnvironment, /CARGO_NET_OFFLINE=true/);

    const network = await run("/usr/bin/python3", [
      "-c",
      "import socket; socket.socket().bind(('127.0.0.1', 0))",
    ]);
    assert.notEqual(network.exitCode, 0);

    const rustc = await run(runtime.rustc, ["--version"]);
    assert.equal(rustc.exitCode, 0, rustc.stderr.body.toString("utf8"));
    assert.match(rustc.stdout.body.toString("utf8"), /^rustc 1\.98\.0 /);

    const cargo = await run(runtime.cargo, [
      "metadata",
      "--offline",
      "--format-version",
      "1",
      "--no-deps",
      "--manifest-path",
      join(workspace, "Cargo.toml"),
    ]);
    assert.equal(cargo.exitCode, 0, cargo.stderr.body.toString("utf8"));
  } finally {
    if (previousRunnerToken === undefined) delete process.env.NANOCODEX_CI_MACOS_TOKEN;
    else process.env.NANOCODEX_CI_MACOS_TOKEN = previousRunnerToken;
    if (previousApiToken === undefined) delete process.env.OPENAI_API_KEY;
    else process.env.OPENAI_API_KEY = previousApiToken;
    await Promise.all([
      rm(sentinel, { force: true }),
      rm(outside, { force: true }),
      rm(profilePath, { force: true }),
      rm(nativeProfilePath, { force: true }),
      rm(directory, { recursive: true, force: true }),
    ]);
  }
});

test("process cancellation terminates the detached process group", {
  skip: process.platform === "win32",
}, async () => {
  const directory = await mkdtemp(join(tmpdir(), "nanocodex-ci-macos-cancel-"));
  const marker = join(directory, "escaped-child.txt");
  const controller = new AbortController();
  const program = [
    "const { spawn } = require('node:child_process');",
    `spawn(process.execPath, ['-e', ${JSON.stringify(
      `setTimeout(() => require('node:fs').writeFileSync(${JSON.stringify(marker)}, 'escaped'), 900)`,
    )}], { stdio: 'ignore' });`,
    "console.log('started');",
    "setInterval(() => {}, 1000);",
  ].join("\n");
  try {
    const pending = runCapturedProcess({
      executable: process.execPath,
      arguments: ["-e", program],
      cwd: directory,
      env: { PATH: process.env.PATH ?? "" },
      signal: controller.signal,
      timeoutMs: 5_000,
      killGraceMs: 50,
      headBytes: 1_024,
      tailBytes: 1_024,
    });
    setTimeout(() => controller.abort(new Error("test cancellation")), 200);
    const result = await pending;
    assert.equal(result.cancelled, true);
    assert.equal(result.timedOut, false);
    assert.equal(result.exitCode, 130);
    assert.match(result.stdout.body.toString("utf8"), /started/);
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 1_000));
    await assert.rejects(stat(marker), { code: "ENOENT" });
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("a hanging heartbeat kills the process group before the claim is reclaimable", {
  skip: process.platform === "win32",
  timeout: 5_000,
}, async () => {
  const directory = await mkdtemp(join(tmpdir(), "nanocodex-ci-macos-reclaim-"));
  const ready = join(directory, "ready");
  const escaped = join(directory, "escaped-child.txt");
  const jobAbort = new AbortController();
  const initialMs = Date.parse("2026-08-22T00:00:00.000Z");
  const deadlineMs = initialMs + 120_000;
  let fakeNow = initialMs;
  let deadlineTimer;
  let heartbeatStartedResolve;
  const heartbeatStarted = new Promise((resolvePromise) => {
    heartbeatStartedResolve = resolvePromise;
  });
  let cancellationReason;
  const program = [
    "const { spawn } = require('node:child_process');",
    "process.on('SIGTERM', () => {});",
    `spawn(process.execPath, ['-e', ${JSON.stringify(
      [
        "process.on('SIGTERM', () => {});",
        `require('node:fs').writeFileSync(${JSON.stringify(ready)}, 'ready');`,
        `setTimeout(() => require('node:fs').writeFileSync(${JSON.stringify(escaped)}, 'escaped'), 400);`,
        "setInterval(() => {}, 1000);",
      ].join("\n"),
    )}], { stdio: 'ignore' });`,
    "setInterval(() => {}, 1000);",
  ].join("\n");
  try {
    const processResult = runCapturedProcess({
      executable: process.execPath,
      arguments: ["-e", program],
      cwd: directory,
      env: { PATH: process.env.PATH ?? "" },
      signal: jobAbort.signal,
      timeoutMs: 4_000,
      killGraceMs: 25,
      headBytes: 1_024,
      tailBytes: 1_024,
    });
    await waitForFile(ready);
    const heartbeat = heartbeatLoop({
      api: {
        heartbeat(_claim, _worker, { signal }) {
          heartbeatStartedResolve();
          return new Promise((_resolvePromise, rejectPromise) => {
            const reject = () => rejectPromise(signal.reason);
            if (signal.aborted) reject();
            else signal.addEventListener("abort", reject, { once: true });
          });
        },
      },
      claim: "claim-1",
      worker: "runner-1",
      leaseExpiresAt: new Date(deadlineMs).toISOString(),
      intervalMs: 30_000,
      safetyMarginMs: 10_000,
      now: () => fakeNow,
      wait: async () => {},
      setTimer(callback, milliseconds) {
        assert.equal(milliseconds, 110_000);
        deadlineTimer = { callback };
        return deadlineTimer;
      },
      clearTimer(timer) {
        if (deadlineTimer === timer) deadlineTimer = undefined;
      },
      onCancel(reason) {
        cancellationReason = reason;
        jobAbort.abort(new Error(reason));
      },
    });
    await heartbeatStarted;
    assert.ok(deadlineTimer);
    fakeNow = deadlineMs - 10_000;
    deadlineTimer.callback();
    const [execution] = await Promise.all([processResult, heartbeat]);
    assert.equal(execution.cancelled, true);
    assert.match(cancellationReason, /lease safety margin reached/);
    assert.ok(fakeNow < deadlineMs, "process group stopped before lease expiry");

    fakeNow = deadlineMs;
    assert.equal(fakeNow >= deadlineMs, true, "broker may now reclaim the expired claim");
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 500));
    await assert.rejects(stat(escaped), { code: "ENOENT" });
  } finally {
    jobAbort.abort();
    await rm(directory, { recursive: true, force: true });
  }
});

test("timeout remains a failure when a SIGTERM handler exits zero", {
  skip: process.platform === "win32",
}, async () => {
  const result = await runCapturedProcess({
    executable: process.execPath,
    arguments: [
      "-e",
      "process.on('SIGTERM', () => process.exit(0)); setInterval(() => {}, 1000);",
    ],
    cwd: tmpdir(),
    env: { PATH: process.env.PATH ?? "" },
    timeoutMs: 100,
    killGraceMs: 50,
    headBytes: 1_024,
    tailBytes: 1_024,
  });
  assert.equal(result.timedOut, true);
  assert.equal(result.cancelled, false);
  assert.equal(result.exitCode, 124);
});

function nativeClaim({ head, leaseExpiresAt }) {
  const cargoLockBlob = "c".repeat(40);
  const cargoVendorSha256 = "d".repeat(64);
  return {
    action: "run",
    claim: "claim-1",
    leaseExpiresAt,
    job: {
      id: `macos-native-build-${head}`,
      head,
      workflowId: "workflow-1",
      task: "native-build",
      source: {
        url: `https://ci.test/api/ci/source/${head}/archive`,
        size: 1,
        sha256: "a".repeat(64),
      },
      cargoVendor: {
        url: `https://ci.test/api/ci/cargo-vendor/${cargoLockBlob}/${cargoVendorSha256}/bundle.tar.gz`,
        size: 189_626_590,
        sha256: cargoVendorSha256,
      },
      publishedAt: "2026-08-22T01:02:03.004Z",
    },
  };
}

function thinArm64MachO() {
  const commandSize = 72 + 80;
  const payloadOffset = 32 + commandSize;
  const size = payloadOffset + 1;
  const binary = Buffer.alloc(size);
  binary.writeUInt32LE(0xfeedfacf, 0);
  binary.writeUInt32LE(0x0100000c, 4);
  binary.writeUInt32LE(0, 8);
  binary.writeUInt32LE(2, 12);
  binary.writeUInt32LE(1, 16);
  binary.writeUInt32LE(commandSize, 20);
  binary.writeUInt32LE(0, 24);
  binary.writeUInt32LE(0, 28);
  binary.writeUInt32LE(0x19, 32);
  binary.writeUInt32LE(commandSize, 36);
  binary.write("__TEXT", 40, "ascii");
  binary.writeBigUInt64LE(0n, 32 + 24);
  binary.writeBigUInt64LE(BigInt(size), 32 + 32);
  binary.writeBigUInt64LE(0n, 32 + 40);
  binary.writeBigUInt64LE(BigInt(size), 32 + 48);
  binary.writeUInt32LE(7, 32 + 56);
  binary.writeUInt32LE(5, 32 + 60);
  binary.writeUInt32LE(1, 32 + 64);
  binary.writeUInt32LE(0, 32 + 68);
  binary.write("__text", 32 + 72, "ascii");
  binary.write("__TEXT", 32 + 72 + 16, "ascii");
  binary.writeBigUInt64LE(0n, 32 + 72 + 32);
  binary.writeBigUInt64LE(1n, 32 + 72 + 40);
  binary.writeUInt32LE(payloadOffset, 32 + 72 + 48);
  binary.writeUInt32LE(0, 32 + 72 + 52);
  binary.writeUInt32LE(0, 32 + 72 + 56);
  binary.writeUInt32LE(0, 32 + 72 + 60);
  binary.writeUInt32LE(0, 32 + 72 + 64);
  binary.writeUInt32LE(0, 32 + 72 + 68);
  binary.writeUInt32LE(0, 32 + 72 + 72);
  binary.writeUInt32LE(0, 32 + 72 + 76);
  binary[payloadOffset] = 0xc3;
  return binary;
}

async function waitForFile(path, timeoutMs = 2_000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      await stat(path);
      return;
    } catch (error) {
      if (error?.code !== "ENOENT") throw error;
      if (Date.now() >= deadline) throw new Error(`timed out waiting for ${path}`);
      await new Promise((resolvePromise) => setTimeout(resolvePromise, 10));
    }
  }
}

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

function tarArchive(entries) {
  const chunks = [];
  for (const entry of entries) {
    const body = entry.body ?? Buffer.alloc(0);
    const header = Buffer.alloc(512);
    writeTarText(header, entry.path, 0, 100);
    writeTarOctal(header, entry.type === "symlink" ? 0o777 : 0o644, 100, 8);
    writeTarOctal(header, 0, 108, 8);
    writeTarOctal(header, 0, 116, 8);
    writeTarOctal(header, body.byteLength, 124, 12);
    writeTarOctal(header, 0, 136, 12);
    header.fill(0x20, 148, 156);
    header[156] = entry.type === "directory" ? 0x35 : entry.type === "symlink" ? 0x32 : 0x30;
    writeTarText(header, entry.link ?? "", 157, 100);
    writeTarText(header, "ustar\0", 257, 6);
    writeTarText(header, "00", 263, 2);
    const checksum = header.reduce((sum, byte) => sum + byte, 0);
    const encoded = checksum.toString(8).padStart(6, "0");
    header.write(encoded, 148, 6, "ascii");
    header[154] = 0;
    header[155] = 0x20;
    chunks.push(header, body, Buffer.alloc((512 - (body.byteLength % 512)) % 512));
  }
  chunks.push(Buffer.alloc(1024));
  return Buffer.concat(chunks);
}

function writeTarText(header, value, offset, length) {
  const bytes = Buffer.from(value);
  assert.ok(bytes.byteLength <= length);
  bytes.copy(header, offset);
}

function writeTarOctal(header, value, offset, length) {
  const encoded = value.toString(8).padStart(length - 1, "0");
  header.write(encoded, offset, length - 1, "ascii");
  header[offset + length - 1] = 0;
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function listen(server) {
  return new Promise((resolvePromise, rejectPromise) => {
    server.once("error", rejectPromise);
    server.listen(0, "127.0.0.1", () => {
      server.removeListener("error", rejectPromise);
      resolvePromise();
    });
  });
}
