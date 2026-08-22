import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import test from "node:test";

import { encodeFrameHeader } from "./ci-pr-cargo-builder.mjs";

import {
  CiRunTerminalError,
  StaleHeadError,
  acquireExclusiveLock,
  assertWebDeploymentToolchain,
  assertCheckoutState,
  assertFreshHead,
  authoritativeRustSecRepositoryUrl,
  cargoBuilderEnvironment,
  captureCargoVendorArtifact,
  controllerConfiguration,
  deploymentEnvironment,
  exclusiveLockCommand,
  extractTarArchive,
  githubStatusContext,
  githubStatusPayload,
  inspectTarArchive,
  parseArguments,
  parseCiRunState,
  parseLsRemote,
  parseMasterSourceState,
  prepareCargoHome,
  prepareWebDeploymentToolchain,
  publicationEnvironment,
  publishPreparedMasterCargoVendor,
  redactSecrets,
  refreshRustSecCheckout,
  repositoryEnvironment,
  resolveTrustedNpmCli,
  runControllerCycle,
  runRepositoryRepair,
  runWhileHeadIsCurrent,
  safeTarPath,
  selectWebDistArtifact,
  validateArtifactHeaders,
  validateCargoConfigurationDiscovery,
  validateCargoHome,
  validateNpmInstallTarget,
  verifyArtifactBytes,
} from "./ci-controller.mjs";

const head = "1".repeat(40);
const otherHead = "2".repeat(40);
const rustSecRevision = "3".repeat(40);
const cargoVendorSha256 = "4".repeat(64);

test("normal reconciliation requires its dedicated GitHub status capability", () => {
  const base = {
    NANOCODEX_CI_ORIGIN: "https://ci.example.test",
  };
  assert.equal(
    controllerConfiguration({
      ...base,
      NANOCODEX_GITHUB_STATUS_TOKEN: "status-authority",
    }, { requireRustSec: false }).githubToken,
    "status-authority",
  );
  assert.throws(
    () => controllerConfiguration(base, { requireRustSec: false }),
    /NANOCODEX_GITHUB_STATUS_TOKEN/,
  );
  assert.throws(
    () => controllerConfiguration({
      ...base,
      GITHUB_TOKEN: "ambient-github-authority",
    }, { requireRustSec: false }),
    /NANOCODEX_GITHUB_STATUS_TOKEN/,
  );
  assert.equal(
    controllerConfiguration({
      ...base,
      NANOCODEX_GITHUB_STATUS_TOKEN: "unneeded-status-authority",
      GITHUB_TOKEN: "ambient-github-authority",
    }, { requireGithubStatus: false, requireRustSec: false }).githubToken,
    undefined,
  );
});

test("controller parses only coherent Cloudflare run states for the requested head", () => {
  const archive = tarArchive(requiredEntries());
  const artifact = artifactFor(archive);
  const successful = successfulRun(artifact);
  const parsed = parseCiRunState(successful, head);
  assert.equal(parsed.outcome, "success");
  assert.equal(parsed.workflowStatus, "complete");
  assert.deepEqual(selectWebDistArtifact(parsed), artifact);

  const running = parseCiRunState({
    ...successful,
    workflow: { status: "running" },
    result: { ...successful.result, status: "running", artifacts: [] },
  }, head);
  assert.equal(running.outcome, "pending");

  assert.throws(
    () => parseCiRunState({ ...successful, head: otherHead }, head),
    /wrong head/,
  );
  assert.throws(
    () => parseCiRunState({ ...successful, workflow: { status: "complete" }, result: null }, head),
    /no successful result/,
  );
  assert.throws(
    () => selectWebDistArtifact({ ...parsed, result: { ...parsed.result, artifacts: [] } }),
    /exactly one/,
  );
  assert.deepEqual(parseArguments(["--once"]), {
    command: "run",
    once: true,
    help: false,
  });
  assert.deepEqual(parseArguments(["repair-repository", head]), {
    command: "repair-repository",
    head,
    once: true,
    help: false,
  });
  assert.throws(() => parseArguments(["repair-repository", "main"]), /full lowercase SHA-1/);
  assert.throws(() => parseArguments(["repair-repository"]), /requires exactly one/);
  assert.throws(() => parseArguments(["--pull-request"]), /unknown argument/);
});

test("controller binds source publication to the normalized deployable master lane", () => {
  const state = {
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
      workflowId: `ci-${head}`,
      state: "pending",
    },
  };
  assert.equal(parseMasterSourceState(state, head).publication.lane.type, "master");
  assert.throws(
    () => parseMasterSourceState({
      ...state,
      publication: {
        ...state.publication,
        branch: "pull/7/merge",
        ref: "refs/pull/7/merge",
        lane: { type: "pull_request", number: 7, pullRequestHead: otherHead },
      },
    }, head),
    /not the exact master lane/,
  );
  assert.throws(
    () => parseMasterSourceState({
      ...state,
      publication: { ...state.publication, lane: undefined },
    }, head),
    /not the exact master lane/,
  );
});

test("child environments isolate every publication and promotion authority", () => {
  const env = {
    PATH: "/safe/bin",
    HOME: "/safe/home",
    NANOCODEX_CI_ORIGIN: "https://ci.example.test",
    NANOCODEX_CI_TOKEN: "ci-secret",
    NANOCODEX_RUSTSEC_REPO: "/rustsec",
    NANOCODEX_RUSTSEC_REVISION: rustSecRevision,
    CLOUDFLARE_ACCOUNT_ID: "account",
    CLOUDFLARE_API_TOKEN: "cloudflare-secret",
    NANOCODEX_GIT_TOKEN: "mirror-secret",
    NANOCODEX_GITHUB_STATUS_TOKEN: "github-secret",
    GITHUB_TOKEN: "ambient-github-secret",
    AWS_SECRET_ACCESS_KEY: "ambient-aws-secret",
    CARGO_REGISTRY_TOKEN: "ambient-cargo-secret",
    CARGO_REGISTRIES_CRATES_IO_TOKEN: "ambient-crates-io-secret",
    CI_RELEASE_TOKEN: "ambient-release-secret",
    NPM_TOKEN: "ambient-npm-secret",
    OPENAI_API_KEY: "ambient-openai-secret",
  };
  const source = publicationEnvironment(env, "source", {
    cargoVendorSha256,
    repository: "/repo",
    rustSecRevision,
  });
  const cargo = publicationEnvironment(env, "cargo-vendor");
  const rustsec = publicationEnvironment(env, "rustsec", { rustSecRepository: "/rustsec" });
  const deployment = deploymentEnvironment(env, head, { origin: "https://web.example.test" });
  const repository = repositoryEnvironment(env, {
    repository: "/repo",
    origin: "https://web.example.test",
  });
  const repositoryRepair = repositoryEnvironment(env, {
    repository: "/repo",
    origin: "https://web.example.test",
    repair: true,
  });

  assert.deepEqual(authorityKeys(source), ["NANOCODEX_CI_TOKEN"]);
  assert.deepEqual(authorityKeys(cargo), ["NANOCODEX_CI_TOKEN"]);
  assert.deepEqual(authorityKeys(rustsec), ["NANOCODEX_CI_TOKEN"]);
  assert.deepEqual(authorityKeys(deployment), ["CLOUDFLARE_API_TOKEN"]);
  assert.deepEqual(authorityKeys(repository), ["NANOCODEX_GIT_TOKEN"]);
  assert.deepEqual(authorityKeys(repositoryRepair), ["NANOCODEX_GIT_TOKEN"]);
  assert.equal(source.NANOCODEX_RUSTSEC_REVISION, rustSecRevision);
  assert.equal(source.NANOCODEX_CI_CARGO_VENDOR_SHA256, cargoVendorSha256);
  assert.equal(rustsec.NANOCODEX_RUSTSEC_REPO, "/rustsec");
  assert.equal(cargo.CARGO_HOME, undefined);
  assert.equal(cargo.NANOCODEX_REPO, undefined);
  assert.equal(deployment.NANOCODEX_DEPLOYMENT_SHA, head);
  assert.equal(repositoryRepair.NANOCODEX_REPAIR_INVALID_PUBLICATION, "1");
  assert.equal(repositoryRepair.NANOCODEX_FORCE_SYNC, undefined);
  for (const child of [source, cargo, rustsec, deployment, repository, repositoryRepair]) {
    assert.equal(child.GITHUB_TOKEN, undefined);
    assert.equal(child.NANOCODEX_GITHUB_STATUS_TOKEN, undefined);
    assert.equal(child.AWS_SECRET_ACCESS_KEY, undefined);
    assert.equal(child.CARGO_REGISTRY_TOKEN, undefined);
    assert.equal(child.CARGO_REGISTRIES_CRATES_IO_TOKEN, undefined);
    assert.equal(child.CI_RELEASE_TOKEN, undefined);
    assert.equal(child.NPM_TOKEN, undefined);
    assert.equal(child.OPENAI_API_KEY, undefined);
  }
  assert.equal(
    redactSecrets("failed with ci-secret and github-secret", ["ci-secret", "github-secret"]),
    "failed with [redacted] and [redacted]",
  );
  assert.throws(
    () => publicationEnvironment(env, "source", {
      repository: "/repo",
      rustSecRevision,
    }),
    /Cargo vendor bundle/,
  );
});

test("web deployment toolchain is clean-installed without ambient npm authority and attested", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-web-toolchain-test-"));
  const repository = resolve(directory, "repository");
  const webRoot = resolve(repository, "web");
  const nodeModules = resolve(webRoot, "node_modules");
  await mkdir(resolve(nodeModules, "stale"), { recursive: true });
  await mkdir(resolve(webRoot, "patches"), { recursive: true });
  await Promise.all([
    writeFile(resolve(webRoot, "package.json"), '{"name":"fixture","private":true}\n'),
    writeFile(resolve(webRoot, "package-lock.json"), '{"name":"fixture","lockfileVersion":3}\n'),
    writeFile(resolve(nodeModules, "stale", "payload.js"), "stale\n"),
    writeFile(
      resolve(webRoot, "patches", "@cloudflare+ci+0.1.0.patch"),
      "authoritative CI patch\n",
    ),
    writeFile(
      resolve(webRoot, "patches", "@cloudflare+sandbox+0.12.1.patch"),
      "authoritative Sandbox patch\n",
    ),
  ]);
  const checkoutPhases = [];
  let npmEnvironment;
  try {
    const verifyCheckout = async (expected, phase) => {
      assert.equal(expected, head);
      checkoutPhases.push(phase);
    };
    const verifyRepositoryFile = async (expected, relativePath) => {
      assert.equal(expected, head);
      return gitBlob(await readFile(resolve(repository, relativePath)));
    };
    const attestation = await prepareWebDeploymentToolchain({
      env: {
        PATH: "/safe/bin",
        HOME: "/ambient/home",
        CARGO_HOME: "/ambient/cargo",
        RUSTUP_HOME: "/ambient/rustup",
        NPM_TOKEN: "ambient-npm-token",
        NODE_AUTH_TOKEN: "ambient-node-token",
        NANOCODEX_CI_TOKEN: "ambient-ci-token",
        CLOUDFLARE_API_TOKEN: "ambient-cloudflare-token",
        NANOCODEX_GIT_TOKEN: "ambient-mirror-token",
      },
      head,
      repository,
      webRoot,
      verifyCheckout,
      verifyRepositoryFile,
      runNpm: async (args, childEnvironment) => {
        assert.deepEqual(args, ["ci", "--prefix", "web", "--ignore-scripts"]);
        npmEnvironment = childEnvironment;
        assert.notEqual(childEnvironment.HOME, "/ambient/home");
        assert.equal(childEnvironment.USERPROFILE, childEnvironment.HOME);
        assert.equal(childEnvironment.CARGO_HOME, undefined);
        assert.equal(childEnvironment.RUSTUP_HOME, undefined);
        for (const name of [
          "NPM_TOKEN",
          "NODE_AUTH_TOKEN",
          "NANOCODEX_CI_TOKEN",
          "CLOUDFLARE_API_TOKEN",
          "NANOCODEX_GIT_TOKEN",
        ]) assert.equal(childEnvironment[name], undefined);
        assert.equal(childEnvironment.NPM_CONFIG_IGNORE_SCRIPTS, "true");
        assert.equal(childEnvironment.NPM_CONFIG_REGISTRY, "https://registry.npmjs.org/");
        assert.notEqual(
          childEnvironment.NPM_CONFIG_USERCONFIG,
          childEnvironment.NPM_CONFIG_GLOBALCONFIG,
        );
        for (const path of [
          childEnvironment.NPM_CONFIG_USERCONFIG,
          childEnvironment.NPM_CONFIG_GLOBALCONFIG,
        ]) {
          const identity = await lstat(path);
          assert.ok(identity.isFile());
          assert.equal(identity.mode & 0o777, 0o600);
        }
        await rm(nodeModules, { recursive: true });
        await mkdir(resolve(nodeModules, "wrangler"), { recursive: true });
        await mkdir(resolve(nodeModules, "patch-package"), { recursive: true });
        await Promise.all([
          writeFile(resolve(nodeModules, "wrangler", "cli.js"), "export {};\n"),
          writeFile(resolve(nodeModules, "patch-package", "index.js"), "require('./dist');\n"),
        ]);
      },
      runPatchPackage: async (args, childEnvironment) => {
        assert.deepEqual(args, [
          resolve(await realpath(nodeModules), "patch-package", "index.js"),
          "--error-on-fail",
        ]);
        assert.equal(childEnvironment, npmEnvironment);
        await writeFile(resolve(nodeModules, "wrangler", "patched"), "yes\n");
      },
    });
    assert.equal(
      npmEnvironment.HOME.startsWith(resolve(
        await realpath(tmpdir()),
        "nanocodex-ci-npm-home-",
      )),
      true,
    );
    assert.deepEqual(checkoutPhases, [
      "before web deployment toolchain install",
      "after npm ci and before Cloudflare patch",
      "after web deployment toolchain install",
    ]);
    await assertWebDeploymentToolchain({
      attestation,
      head,
      phase: "before authority",
      repository,
      webRoot,
      verifyCheckout,
      verifyRepositoryFile,
    });

    await writeFile(resolve(nodeModules, "wrangler", "cli.js"), "tampered\n");
    await assert.rejects(
      assertWebDeploymentToolchain({
        attestation,
        head,
        phase: "before deploy",
        repository,
        webRoot,
        verifyCheckout,
        verifyRepositoryFile,
      }),
      /tampered or stale/,
    );
    await writeFile(resolve(nodeModules, "wrangler", "cli.js"), "export {};\n");
    await writeFile(
      resolve(webRoot, "patches", "@cloudflare+sandbox+0.12.1.patch"),
      "tampered Sandbox patch\n",
    );
    await assert.rejects(
      assertWebDeploymentToolchain({
        attestation,
        head,
        phase: "before Cloudflare authority",
        repository,
        webRoot,
        verifyCheckout,
        verifyRepositoryFile,
      }),
      /tampered or stale/,
    );
    await writeFile(
      resolve(webRoot, "patches", "@cloudflare+sandbox+0.12.1.patch"),
      "authoritative Sandbox patch\n",
    );
    await chmod(nodeModules, 0o777);
    await assert.rejects(
      assertWebDeploymentToolchain({
        attestation,
        head,
        phase: "before authority with unsafe permissions",
        repository,
        webRoot,
        verifyCheckout,
        verifyRepositoryFile,
      }),
      /group- or world-writable/,
    );
    await chmod(nodeModules, 0o755);
    await writeFile(resolve(nodeModules, "extraneous.js"), "stale\n");
    await assert.rejects(
      assertWebDeploymentToolchain({
        attestation,
        head,
        phase: "before repository publication",
        repository,
        webRoot,
        verifyCheckout,
        verifyRepositoryFile,
      }),
      /tampered or stale/,
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("npm CLI is pinned to the trusted Node distribution and rejects builtin credentials", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-npm-runtime-test-"));
  const node = resolve(directory, "runtime", "bin", "node");
  const npmRoot = resolve(directory, "runtime", "lib", "node_modules", "npm");
  const npmCli = resolve(npmRoot, "bin", "npm-cli.js");
  const npmrc = resolve(npmRoot, "npmrc");
  await Promise.all([
    mkdir(resolve(directory, "runtime", "bin"), { recursive: true }),
    mkdir(resolve(npmRoot, "bin"), { recursive: true }),
  ]);
  try {
    await Promise.all([
      writeFile(node, "trusted node\n", { mode: 0o755 }),
      writeFile(npmCli, "trusted npm\n", { mode: 0o755 }),
      writeFile(npmrc, "prefix=${HOME}/.npm-global\n", { mode: 0o644 }),
    ]);
    assert.equal(await resolveTrustedNpmCli(node), await realpath(npmCli));
    await rm(npmrc);
    assert.equal(await resolveTrustedNpmCli(node), await realpath(npmCli));
    for (const key of [
      "//registry.npmjs.org/:_auth",
      "//registry.npmjs.org/:_authToken",
      "//registry.npmjs.org/:username",
      "//registry.npmjs.org/:_password",
      "//registry.npmjs.org/:certfile",
      "//registry.npmjs.org/:keyfile",
      "@cloudflare:registry",
    ]) {
      await writeFile(npmrc, `${key}=forbidden\n`);
      await assert.rejects(resolveTrustedNpmCli(node), /credential authority/, key);
    }
    assert.match(await resolveTrustedNpmCli(), /\/npm\/bin\/npm-cli\.js$/);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("npm preflight rejects project npmrc and symlink node_modules without touching its target", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-web-npm-target-test-"));
  const repository = resolve(directory, "repository");
  const webRoot = resolve(repository, "web");
  const outside = resolve(directory, "outside");
  await Promise.all([
    mkdir(webRoot, { recursive: true }),
    mkdir(outside, { mode: 0o700 }),
  ]);
  await writeFile(resolve(outside, "sentinel"), "preserve\n");
  await symlink(outside, resolve(webRoot, "node_modules"), "dir");
  try {
    await assert.rejects(validateNpmInstallTarget(webRoot), /symlink node_modules/);
    assert.equal(await readFile(resolve(outside, "sentinel"), "utf8"), "preserve\n");
    await rm(resolve(webRoot, "node_modules"));
    await writeFile(resolve(webRoot, ".npmrc"), "//registry.npmjs.org/:_authToken=forbidden\n");
    let invoked = false;
    await assert.rejects(
      prepareWebDeploymentToolchain({
        env: {},
        head,
        repository,
        webRoot,
        verifyCheckout: async () => undefined,
        verifyRepositoryFile: async () => head,
        runNpm: async () => {
          invoked = true;
        },
        runPatchPackage: async () => {
          invoked = true;
        },
      }),
      /rejects project configuration/,
    );
    assert.equal(invoked, false);
    await rm(resolve(webRoot, ".npmrc"));
    await Promise.all([
      writeFile(resolve(webRoot, "package.json"), "{}\n"),
      writeFile(resolve(webRoot, "package-lock.json"), '{"lockfileVersion":3}\n'),
    ]);
    await assert.rejects(
      prepareWebDeploymentToolchain({
        env: {},
        head,
        repository,
        webRoot,
        verifyCheckout: async () => undefined,
        verifyRepositoryFile: async (_expected, relativePath) => {
          if (relativePath.includes("patches/")) {
            throw new Error("authoritative Cloudflare patch is missing");
          }
          return head;
        },
        runNpm: async () => {
          invoked = true;
        },
        runPatchPackage: async () => {
          invoked = true;
        },
      }),
      /patch is missing/,
    );
    assert.equal(invoked, false);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("controller Cargo home is repo-scoped, private, real, and authority-free", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-cargo-home-test-"));
  const gitCommonDirectory = resolve(directory, "repository.git");
  await mkdir(gitCommonDirectory, { mode: 0o700 });
  try {
    const cargoHome = await prepareCargoHome(gitCommonDirectory);
    assert.equal(
      cargoHome,
      resolve(await realpath(gitCommonDirectory), "nanocodex-ci-cargo-home"),
    );
    const identity = await lstat(cargoHome);
    assert.ok(identity.isDirectory());
    assert.ok(!identity.isSymbolicLink());
    assert.equal(identity.mode & 0o777, 0o700);
    assert.equal(await validateCargoHome(cargoHome), cargoHome);

    for (const name of ["config", "config.toml", "credentials", "credentials.toml"]) {
      await writeFile(resolve(cargoHome, name), "[registry]\ntoken = 'forbidden'\n", {
        mode: 0o600,
      });
      await assert.rejects(validateCargoHome(cargoHome), new RegExp(`authority file: ${name}`));
      await rm(resolve(cargoHome, name));
    }

    await chmod(cargoHome, 0o755);
    await assert.rejects(validateCargoHome(cargoHome), /private to its owner/);
    await chmod(cargoHome, 0o700);

    const symlinkTarget = resolve(directory, "symlink-target");
    await mkdir(symlinkTarget, { mode: 0o700 });
    await rm(cargoHome, { recursive: true });
    await symlink(symlinkTarget, cargoHome, "dir");
    await assert.rejects(prepareCargoHome(gitCommonDirectory), /real directory, not a symlink/);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("Cargo config discovery rejects ignored checkout and ambient ancestor authority", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-cargo-config-test-"));
  const gitCommonDirectory = resolve(directory, "repository.git");
  const parent = resolve(directory, "parent");
  const repository = resolve(parent, "checkout");
  await Promise.all([
    mkdir(gitCommonDirectory, { mode: 0o700 }),
    mkdir(resolve(repository, ".cargo"), { recursive: true, mode: 0o700 }),
  ]);
  try {
    const cargoHome = await prepareCargoHome(gitCommonDirectory);
    const ignoredConfig = resolve(repository, ".cargo", "config");
    await writeFile(ignoredConfig, "[source.crates-io]\nreplace-with = 'host-mirror'\n");
    await assert.rejects(validateCargoConfigurationDiscovery({
      cargoHome,
      repository,
      verifyRepositoryConfig: async (name) => {
        if (name === "config") throw new Error("config is ignored and not tracked by master");
      },
    }), /ignored and not tracked/);

    await rm(ignoredConfig);
    const authoritativeConfig = resolve(repository, ".cargo", "config.toml");
    await writeFile(authoritativeConfig, "[net]\ngit-fetch-with-cli = false\n");
    const canonicalRepository = await realpath(repository);
    const canonicalAuthoritativeConfig = await realpath(authoritativeConfig);
    assert.equal(await validateCargoConfigurationDiscovery({
      cargoHome,
      repository,
      verifyRepositoryConfig: async (name, path) => {
        assert.equal(name, "config.toml");
        assert.equal(path, canonicalAuthoritativeConfig);
      },
    }), canonicalRepository);

    await mkdir(resolve(parent, ".cargo"), { mode: 0o700 });
    const ambientConfig = resolve(parent, ".cargo", "config.toml");
    await writeFile(ambientConfig, "[registry]\nglobal-credential-providers = ['ambient']\n");
    const canonicalAmbientConfig = await realpath(ambientConfig);
    await assert.rejects(validateCargoConfigurationDiscovery({
      cargoHome,
      repository,
      verifyRepositoryConfig: async () => undefined,
    }), (error) =>
      error.message === `trusted CI Cargo rejects ambient configuration: ${canonicalAmbientConfig}`
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("master Cargo build child is credential-empty and uploader remains the only authority", () => {
  const child = cargoBuilderEnvironment({
    PATH: "/hostile/bin",
    HOME: "/hostile/home",
    CARGO_HOME: "/hostile/cargo",
    NODE_OPTIONS: "--require=/hostile/inject.cjs",
    NANOCODEX_CI_TOKEN: "source-authority",
    NANOCODEX_GITHUB_STATUS_TOKEN: "github-authority",
    CARGO_REGISTRY_TOKEN: "registry-authority",
    CLOUDFLARE_API_TOKEN: "deploy-authority",
  });
  assert.deepEqual(child, {
    PATH: "/usr/bin:/bin",
    LANG: "C.UTF-8",
    LC_ALL: "C.UTF-8",
  });
  const uploader = publicationEnvironment({
    NANOCODEX_CI_ORIGIN: "https://ci.example.test",
    NANOCODEX_CI_TOKEN: "source-authority",
  }, "cargo-vendor");
  assert.deepEqual(authorityKeys(child), []);
  assert.deepEqual(authorityKeys(uploader), ["NANOCODEX_CI_TOKEN"]);
  assert.equal(uploader.NANOCODEX_REPO, undefined);
  assert.equal(uploader.CARGO_HOME, undefined);
});

test("a successful Cargo builder must keep stderr empty", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-builder-stderr-"));
  try {
    await assert.rejects(
      captureCargoVendorArtifact({
        args: ["--eval", "process.stderr.write('unexpected diagnostic')"],
        artifactDirectory: directory,
        command: process.execPath,
        cwd: directory,
        env: { PATH: "/usr/bin:/bin", LANG: "C.UTF-8" },
        input: Buffer.from("{}"),
        timeoutMs: 10_000,
      }),
      /successful Cargo builder emitted stderr/,
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("bounded Cargo builder cancellation kills a SIGTERM-ignoring descendant group", async () => {
  if (process.platform === "win32") return;
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-builder-cancel-"));
  const script = resolve(directory, "builder.mjs");
  const pidPath = resolve(directory, "descendant.pid");
  await writeFile(script, `
    import { spawn } from "node:child_process";
    import { writeFileSync } from "node:fs";
    const child = spawn(process.execPath, ["--eval",
      "process.on('SIGTERM',()=>{});setInterval(()=>{},1000)"
    ], { stdio: "ignore" });
    writeFileSync(${JSON.stringify(pidPath)}, String(child.pid));
    process.on("SIGTERM", () => {});
    setInterval(() => {}, 1000);
  `);
  const controller = new AbortController();
  try {
    const running = captureCargoVendorArtifact({
      args: [script],
      artifactDirectory: directory,
      command: process.execPath,
      cwd: directory,
      env: { PATH: "/usr/bin:/bin", LANG: "C.UTF-8" },
      input: Buffer.from("{}"),
      signal: controller.signal,
      timeoutMs: 10_000,
    });
    let descendant;
    for (let attempt = 0; attempt < 100; attempt += 1) {
      descendant = Number(await readFile(pidPath, "utf8").catch(() => ""));
      if (Number.isSafeInteger(descendant) && descendant > 0) break;
      await new Promise((resolvePromise) => setTimeout(resolvePromise, 10));
    }
    assert.ok(Number.isSafeInteger(descendant) && descendant > 0);
    controller.abort(new DOMException("test cancellation", "AbortError"));
    await assert.rejects(
      running.catch((cause) => {
        assertProcessDead(descendant);
        throw cause;
      }),
      /test cancellation/,
    );
    await waitForProcessGone(descendant);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("master rejects a successful leader that left a TERM-ignoring detached-stdio descendant", async () => {
  if (process.platform === "win32") return;
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-master-success-group-"));
  const pidPath = resolve(directory, "processes.json");
  const controller = new AbortController();
  let processes;
  let running;
  try {
    const frame = masterCargoFrame();
    const program = leaderWithTermIgnoringDescendant(
      pidPath,
      `require("node:fs").writeSync(1,Buffer.from(${JSON.stringify(frame.toString("base64"))},"base64"));`,
    );
    running = captureCargoVendorArtifact({
      args: ["--eval", program],
      artifactDirectory: directory,
      command: process.execPath,
      cwd: directory,
      env: { PATH: "/usr/bin:/bin", LANG: "C.UTF-8" },
      input: Buffer.from("{}"),
      signal: controller.signal,
      timeoutMs: 10_000,
    });
    processes = await waitForProcessFixture(pidPath);
    await waitForProcessGone(processes.leader);
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

test("master abort wins after leader exit and awaits descendant group cleanup", async () => {
  if (process.platform === "win32") return;
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-master-exit-abort-"));
  const pidPath = resolve(directory, "processes.json");
  const controller = new AbortController();
  let processes;
  let running;
  try {
    const frame = masterCargoFrame();
    const program = leaderWithTermIgnoringDescendant(
      pidPath,
      `require("node:fs").writeSync(1,Buffer.from(${JSON.stringify(frame.toString("base64"))},"base64"));`,
    );
    running = captureCargoVendorArtifact({
      args: ["--eval", program],
      artifactDirectory: directory,
      command: process.execPath,
      cwd: directory,
      env: { PATH: "/usr/bin:/bin", LANG: "C.UTF-8" },
      input: Buffer.from("{}"),
      signal: controller.signal,
      timeoutMs: 10_000,
    });
    processes = await waitForProcessFixture(pidPath);
    await waitForProcessGone(processes.leader);
    controller.abort(new DOMException("superseded master", "AbortError"));
    await assert.rejects(
      running.catch((cause) => {
        assertProcessDead(processes.descendant);
        throw cause;
      }),
      (cause) => cause?.name === "AbortError" && cause.message === "superseded master",
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

test("master always completes build before exact-object reuse inspection", async () => {
  const descriptor = {
    version: 1,
    head,
    cargoLockBlob: otherHead,
    key: `cargo-vendor/${otherHead}/${cargoVendorSha256}/bundle.tar.gz`,
    size: 123,
    sha256: cargoVendorSha256,
  };
  const calls = [];
  const observed = await publishPreparedMasterCargoVendor({
    build: async () => {
      calls.push("build");
      return {
        descriptor,
        cleanup: async () => calls.push("cleanup"),
      };
    },
    assertFresh: async () => calls.push("fresh"),
    upload: async () => {
      assert.deepEqual(calls, ["build", "fresh"]);
      calls.push("head-or-upload");
    },
  }, head);
  assert.equal(observed.sha256, cargoVendorSha256);
  assert.deepEqual(calls, ["build", "fresh", "head-or-upload", "fresh", "cleanup"]);
});

test("checkout and stale-head checks require exact authoritative master", () => {
  assert.equal(assertFreshHead(head, head, "before deploy"), head);
  assert.throws(
    () => assertFreshHead(head, otherHead, "after deploy"),
    (error) => error.name === "StaleHeadError" && error.phase === "after deploy",
  );
  assert.equal(assertCheckoutState({
    ref: "refs/heads/master",
    head,
    trackingHead: head,
    status: "",
  }, head), head);
  assert.throws(() => assertCheckoutState({
    ref: "HEAD",
    head,
    trackingHead: head,
    status: "",
  }, head), /attached master/);
  assert.throws(() => assertCheckoutState({
    ref: "refs/heads/master",
    head,
    trackingHead: head,
    status: "?? unexpected\0",
  }, head), /clean tracked and untracked/);

  const refs = parseLsRemote(
    `${head}\tHEAD\n${head}\trefs/heads/master\n`,
    ["HEAD", "refs/heads/master"],
  );
  assert.equal(refs.get("HEAD"), head);
  assert.throws(
    () => parseLsRemote(`${head}\trefs/heads/not-master\n`, ["refs/heads/master"]),
    /unexpected output/,
  );
});

test("exclusive controller lock is process-held and ignores crash-stale file contents", async () => {
  assert.deepEqual(exclusiveLockCommand("darwin"), {
    command: "/usr/bin/lockf",
    args: ["-s", "-t", "0", "3"],
  });
  assert.deepEqual(exclusiveLockCommand("linux"), {
    command: "flock",
    args: ["--exclusive", "--nonblock", "--conflict-exit-code", "75", "3"],
  });
  assert.throws(() => exclusiveLockCommand("win32"), /unsupported/);

  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-controller-lock-test-"));
  const path = resolve(directory, "controller.lock");
  await writeFile(path, "stale metadata from a crashed controller\n", { mode: 0o600 });
  let release;
  try {
    release = await acquireExclusiveLock(path, { repository: "/trusted/repository" });
    const owner = JSON.parse(await readFile(path, "utf8"));
    assert.equal(owner.version, 1);
    assert.equal(owner.pid, process.pid);
    assert.equal(owner.repository, "/trusted/repository");
    await assert.rejects(
      acquireExclusiveLock(path, { repository: "/trusted/repository" }),
      /already locked/,
    );
    await release();
    release = undefined;

    const reacquired = await acquireExclusiveLock(path, {
      repository: "/trusted/repository",
    });
    await reacquired();
    assert.ok((await readFile(path, "utf8")).includes(`"pid":${process.pid}`));
  } finally {
    await release?.();
    await rm(directory, { recursive: true, force: true });
  }
});

test("RustSec refresh requires the exact official HTTPS main and a clean fast-forward", async () => {
  const previous = "4".repeat(40);
  const fixture = rustSecGitFixture({
    head: previous,
    authoritativeHead: rustSecRevision,
  });
  assert.equal(await refreshRustSecCheckout(fixture.git), rustSecRevision);
  assert.deepEqual(
    fixture.calls.find((args) => args[0] === "fetch"),
    [
      "fetch",
      "--no-tags",
      "--no-recurse-submodules",
      authoritativeRustSecRepositoryUrl,
      "refs/heads/main:refs/remotes/origin/main",
    ],
  );
  assert.ok(fixture.calls.some((args) =>
    args[0] === "merge" &&
    args[1] === "--ff-only" &&
    args.at(-1) === rustSecRevision
  ));

  const wrongRemote = rustSecGitFixture({
    remoteUrl: "https://example.test/untrusted/advisory-db.git",
  });
  await assert.rejects(
    refreshRustSecCheckout(wrongRemote.git),
    /authoritative official HTTPS remote/,
  );
  assert.ok(!wrongRemote.calls.some((args) => args[0] === "fetch"));

  const dirty = rustSecGitFixture({ status: "?? injected-advisory.md\0" });
  await assert.rejects(refreshRustSecCheckout(dirty.git), /clean tracked and untracked/);
  assert.ok(!dirty.calls.some((args) => args[0] === "fetch"));

  const detached = rustSecGitFixture({ ref: "HEAD" });
  await assert.rejects(refreshRustSecCheckout(detached.git), /attached main/);
  assert.ok(!detached.calls.some((args) => args[0] === "fetch"));

  const diverged = rustSecGitFixture({
    head: previous,
    authoritativeHead: rustSecRevision,
    fastForward: false,
  });
  await assert.rejects(refreshRustSecCheckout(diverged.git), /cannot fast-forward/);
  assert.ok(!diverged.calls.some((args) => args[0] === "merge"));

  const rewritten = rustSecGitFixture({
    configNames: ["core.repositoryformatversion", "url.file:///tmp/evil.insteadof"],
  });
  await assert.rejects(refreshRustSecCheckout(rewritten.git), /local Git configuration/);
  assert.ok(!rewritten.calls.some((args) => args[0] === "fetch"));
});

test("web-dist verification binds declaration, transport headers, bytes, and safe extraction", async () => {
  const archive = tarArchive(requiredEntries());
  const artifact = artifactFor(archive);
  const headers = artifactHeaders(artifact);
  assert.equal(validateArtifactHeaders(headers, artifact, head), artifact);
  assert.equal(verifyArtifactBytes(archive, artifact), archive);
  assert.deepEqual(
    inspectTarArchive(archive).filter(({ type }) => type === "file").map(({ path }) => path),
    ["nanocodex/wrangler.json", "client/index.html", "client/assets/app.js"],
  );

  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-controller-test-"));
  const destination = resolve(directory, "dist");
  try {
    await extractTarArchive(archive, destination);
    assert.equal(await readFile(resolve(destination, "nanocodex/wrangler.json"), "utf8"), "{}\n");
    assert.equal(await readFile(resolve(destination, "client/index.html"), "utf8"), "<main></main>\n");
  } finally {
    await rm(directory, { recursive: true, force: true });
  }

  assert.throws(
    () => validateArtifactHeaders(
      new Headers([...headers, ["x-nanocodex-sha256", "f".repeat(64)]]),
      artifact,
      head,
    ),
    /headers do not match/,
  );
  assert.throws(
    () => verifyArtifactBytes(Buffer.concat([archive, Buffer.from("extra")]), artifact),
    /expected/,
  );
});

test("tar path policy rejects traversal, absolute paths, links, and topology conflicts", () => {
  assert.equal(safeTarPath("./client/assets/app.js"), "client/assets/app.js");
  for (const unsafe of ["../escape", "./../escape", "/absolute", "C:/windows", "a\\b", "a//b"]) {
    assert.throws(() => safeTarPath(unsafe), /unsafe web-dist archive path/);
  }
  assert.throws(
    () => inspectTarArchive(tarArchive([
      ...requiredEntries(),
      { path: "../escape", body: "bad" },
    ])),
    /unsafe web-dist archive path/,
  );
  assert.throws(
    () => inspectTarArchive(tarArchive([
      ...requiredEntries(),
      { path: "client/link", type: "2", link: "../../outside" },
    ])),
    /rejects entry type/,
  );
  assert.throws(
    () => inspectTarArchive(tarArchive([
      { path: "nanocodex", body: "file" },
      ...requiredEntries(),
    ])),
    /beneath file/,
  );
});

test("GitHub status payloads use one stable context and immutable run target", () => {
  for (const state of ["pending", "success", "failure", "error"]) {
    const payload = githubStatusPayload(state, head, "https://ci.example.test/base?ignored=1");
    assert.equal(payload.state, state);
    assert.equal(payload.context, githubStatusContext);
    assert.equal(payload.target_url, `https://ci.example.test/api/ci/runs/${head}`);
    assert.ok(payload.description.length > 0 && payload.description.length <= 140);
    assert.deepEqual(Object.keys(payload).sort(), [
      "context",
      "description",
      "state",
      "target_url",
    ]);
  }
  assert.throws(
    () => githubStatusPayload("neutral", head, "https://ci.example.test"),
    /unsupported/,
  );
});

test("controller orders exact green promotion and stops terminal failures before deployment", async () => {
  const archive = tarArchive(requiredEntries());
  const artifact = artifactFor(archive);
  const successful = parseCiRunState(successfulRun(artifact), head);
  const calls = [];
  const operations = orderedOperations(calls, successful, archive);
  const result = await runControllerCycle(operations);
  assert.deepEqual(result, { head, action: "promoted", artifact });
  assert.deepEqual(calls, [
    "checkout",
    `toolchain:${head}`,
    `status:pending:${head}`,
    `rustsec:${head}`,
    `cargo:${head}`,
    `source:${head}:${rustSecRevision}:${cargoVendorSha256}`,
    `wait:${head}`,
    `fresh:before promotion:${head}`,
    `promoted?:${head}`,
    `download:${head}:${artifact.sha256}`,
    `install:${artifact.sha256}:${archive.length}`,
    `fresh:before deploy:${head}`,
    `deploy:${head}`,
    `fresh:after deploy:${head}`,
    `repository:${head}`,
    `fresh:after repository publication:${head}`,
    `verify:${head}`,
    `fresh:after promotion verification:${head}`,
    `status:success:${head}`,
  ]);

  const failedCalls = [];
  const failure = {
    head,
    workflowStatus: "errored",
    resultStatus: "failure",
    outcome: "failure",
    result: { failure: { message: "quality failed" } },
  };
  await assert.rejects(
    runControllerCycle(orderedOperations(failedCalls, failure, archive)),
    (error) => error instanceof CiRunTerminalError && error.githubState === "failure",
  );
  assert.deepEqual(failedCalls, [
    "checkout",
    `toolchain:${head}`,
    `status:pending:${head}`,
    `rustsec:${head}`,
    `cargo:${head}`,
    `source:${head}:${rustSecRevision}:${cargoVendorSha256}`,
    `wait:${head}`,
    `fresh:before terminal status:${head}`,
    `status:failure:${head}`,
  ]);
});

test("already-promoted green heads are verified without redeployment", async () => {
  const archive = tarArchive(requiredEntries());
  const artifact = artifactFor(archive);
  const successful = parseCiRunState(successfulRun(artifact), head);
  const calls = [];
  const operations = orderedOperations(calls, successful, archive);
  operations.isPromoted = async (value) => {
    calls.push(`promoted?:${value}`);
    return true;
  };
  assert.equal((await runControllerCycle(operations)).action, "already-promoted");
  assert.deepEqual(calls.slice(-4), [
    `fresh:before promotion:${head}`,
    `promoted?:${head}`,
    `fresh:after promotion inspection:${head}`,
    `status:success:${head}`,
  ]);
  assert.ok(!calls.some((call) => call.startsWith("deploy:")));
});

test("authoritative master monitor aborts a long operation on a bounded supersession check", async () => {
  let reads = 0;
  let operationAborted = false;
  await assert.rejects(
    runWhileHeadIsCurrent(
      head,
      (signal) => new Promise((_resolveOperation, rejectOperation) => {
        const abort = () => {
          operationAborted = true;
          rejectOperation(signal.reason);
        };
        signal.addEventListener("abort", abort, { once: true });
      }),
      {
        phase: "while waiting for CI",
        pollMs: 1,
        readAuthoritativeHead: async () => ++reads === 1 ? head : otherHead,
      },
    ),
    (error) =>
      error instanceof StaleHeadError &&
      error.expected === head &&
      error.observed === otherHead &&
      error.phase === "while waiting for CI",
  );
  assert.equal(operationAborted, true);
  assert.equal(reads, 2);
});

test("superseded master receives no terminal status or promotion", async () => {
  const archive = tarArchive(requiredEntries());
  const artifact = artifactFor(archive);
  const successful = parseCiRunState(successfulRun(artifact), head);

  for (const scenario of [
    "during-wait",
    "wait-error",
    "terminal-failure",
    "after-success",
  ]) {
    const calls = [];
    const run = scenario === "terminal-failure"
      ? {
          head,
          workflowStatus: "errored",
          resultStatus: "failure",
          outcome: "failure",
          result: { failure: { message: "quality failed" } },
        }
      : successful;
    const operations = orderedOperations(calls, run, archive);
    if (scenario === "during-wait" || scenario === "wait-error") {
      operations.waitForRun = async (value) => {
        calls.push(`wait:${value}`);
        if (scenario === "during-wait") {
          throw new StaleHeadError(head, otherHead, "while waiting for Cloudflare CI");
        }
        throw new Error("CI backend unavailable");
      };
    }
    if (scenario !== "during-wait") {
      const baseAssertFresh = operations.assertFresh;
      operations.assertFresh = async (value, phase) => {
        await baseAssertFresh(value, phase);
        if (
          (["wait-error", "terminal-failure"].includes(scenario) &&
            phase === "before terminal status") ||
          (scenario === "after-success" && phase === "before promotion")
        ) throw new StaleHeadError(head, otherHead, phase);
      };
    }
    await assert.rejects(
      runControllerCycle(operations),
      (error) => error instanceof StaleHeadError && error.observed === otherHead,
      scenario,
    );
    assert.deepEqual(
      calls.filter((call) => call.startsWith("status:")),
      [`status:pending:${head}`],
      scenario,
    );
    assert.ok(
      !calls.some((call) => /^(?:download|install|deploy|repository|verify):/.test(call)),
      scenario,
    );
  }
});

test("master-lock repository repair proves exact local, remote, deployment, and mirror state", async () => {
  const calls = [];
  const toolchain = { version: 1, head };
  const operations = {
    synchronizeCheckout: async () => {
      calls.push("checkout");
      return head;
    },
    prepareToolchain: async (value) => {
      calls.push(`toolchain:${value}`);
      return toolchain;
    },
    assertFresh: async (value, phase) => calls.push(`fresh:${phase}:${value}`),
    verifyDeployment: async (value) => calls.push(`deployment:${value}`),
    publishRepository: async (value, observedToolchain, _signal, options) => {
      assert.equal(observedToolchain, toolchain);
      assert.deepEqual(options, { repair: true });
      calls.push(`repository-repair:${value}`);
    },
    verifyPromotion: async (value) => calls.push(`verify:${value}`),
  };
  assert.deepEqual(await runRepositoryRepair(operations, head), {
    head,
    action: "repository-repaired",
  });
  assert.deepEqual(calls, [
    "checkout",
    `toolchain:${head}`,
    `fresh:before repository repair deployment proof:${head}`,
    `deployment:${head}`,
    `fresh:before repository repair publication:${head}`,
    `repository-repair:${head}`,
    `fresh:after repository repair publication:${head}`,
    `verify:${head}`,
    `fresh:after repository repair verification:${head}`,
  ]);

  const mismatchCalls = [];
  await assert.rejects(
    runRepositoryRepair({
      synchronizeCheckout: async () => {
        mismatchCalls.push("checkout");
        return otherHead;
      },
    }, head),
    (error) =>
      error instanceof StaleHeadError &&
      error.phase === "repository repair local HEAD",
  );
  assert.deepEqual(mismatchCalls, ["checkout"]);
});

function orderedOperations(calls, run, archive) {
  const toolchain = { version: 1, head };
  return {
    synchronizeCheckout: async () => {
      calls.push("checkout");
      return head;
    },
    prepareToolchain: async (value) => {
      calls.push(`toolchain:${value}`);
      return toolchain;
    },
    updateStatus: async (value, state) => calls.push(`status:${state}:${value}`),
    publishRustSec: async (value, observedToolchain) => {
      assert.equal(observedToolchain, toolchain);
      calls.push(`rustsec:${value}`);
      return rustSecRevision;
    },
    publishCargoVendor: async (value, observedToolchain) => {
      assert.equal(observedToolchain, toolchain);
      calls.push(`cargo:${value}`);
      return { sha256: cargoVendorSha256 };
    },
    publishSource: async (value, revision, observedCargoSha, observedToolchain) => {
      assert.equal(observedCargoSha, cargoVendorSha256);
      assert.equal(observedToolchain, toolchain);
      calls.push(`source:${value}:${revision}:${observedCargoSha}`);
    },
    waitForRun: async (value) => {
      calls.push(`wait:${value}`);
      return run;
    },
    assertFresh: async (value, phase) => calls.push(`fresh:${phase}:${value}`),
    isPromoted: async (value) => {
      calls.push(`promoted?:${value}`);
      return false;
    },
    downloadArtifact: async (value, artifact) => {
      calls.push(`download:${value}:${artifact.sha256}`);
      return archive;
    },
    installArtifact: async (bytes, artifact) => {
      calls.push(`install:${artifact.sha256}:${bytes.length}`);
    },
    deploy: async (value, observedToolchain) => {
      assert.equal(observedToolchain, toolchain);
      calls.push(`deploy:${value}`);
    },
    publishRepository: async (value, observedToolchain) => {
      assert.equal(observedToolchain, toolchain);
      calls.push(`repository:${value}`);
    },
    verifyPromotion: async (value) => calls.push(`verify:${value}`),
  };
}

function successfulRun(artifact) {
  return {
    version: 1,
    head,
    workflowId: `ci-${head}`,
    state: "dispatched",
    workflow: { status: "complete" },
    result: {
      version: 1,
      head,
      workflowId: `ci-${head}`,
      status: "success",
      artifacts: [
        {
          key: `runs/${head}/artifacts/web-wasm.tar`,
          size: 100,
          sha256: "a".repeat(64),
          contentType: "application/x-tar",
        },
        artifact,
      ],
    },
    progress: null,
  };
}

function artifactFor(archive) {
  return {
    key: `runs/${head}/artifacts/web-dist.tar`,
    size: archive.length,
    sha256: createHash("sha256").update(archive).digest("hex"),
    contentType: "application/x-tar",
  };
}

function gitBlob(body) {
  const bytes = Buffer.isBuffer(body) ? body : Buffer.from(body);
  return createHash("sha1")
    .update(`blob ${bytes.length}\0`)
    .update(bytes)
    .digest("hex");
}

function masterCargoFrame() {
  const payload = Buffer.from("master process-group fixture");
  const sha256 = createHash("sha256").update(payload).digest("hex");
  const descriptor = {
    version: 1,
    head,
    cargoLockBlob: otherHead,
    key: `cargo-vendor/${otherHead}/${sha256}/bundle.tar.gz`,
    size: payload.length,
    sha256,
  };
  return Buffer.concat([encodeFrameHeader(descriptor), payload]);
}

function leaderWithTermIgnoringDescendant(pidPath, afterReady = "") {
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
    `if(existsSync(${JSON.stringify(pidPath)})){clearInterval(ready);${afterReady}}`,
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

function artifactHeaders(artifact) {
  return new Headers({
    "cache-control": "public, max-age=31536000, immutable",
    "content-disposition": `attachment; filename="nanocodex-${head}-web-dist.tar"`,
    "content-length": String(artifact.size),
    "content-type": artifact.contentType,
    "x-content-type-options": "nosniff",
    "x-nanocodex-sha256": artifact.sha256,
  });
}

function authorityKeys(env) {
  return Object.keys(env)
    .filter((name) => /(?:TOKEN|SECRET|API_KEY)$/.test(name))
    .sort();
}

function rustSecGitFixture({
  remoteUrl = authoritativeRustSecRepositoryUrl,
  ref = "refs/heads/main",
  head: initialHead = rustSecRevision,
  authoritativeHead = rustSecRevision,
  status = "",
  fastForward = true,
  configNames = [
    "core.repositoryformatversion",
    "remote.origin.url",
    "remote.origin.fetch",
    "branch.main.remote",
    "branch.main.merge",
  ],
} = {}) {
  const calls = [];
  let head = initialHead;
  let trackingHead = initialHead;
  const git = async (args) => {
    calls.push([...args]);
    if (args[0] === "config" && args.includes("--name-only")) {
      return { stdout: `${configNames.join("\n")}\n`, stderr: "" };
    }
    if (args[0] === "config" && args.at(-1) === "remote.origin.url") {
      return { stdout: `${remoteUrl}\n`, stderr: "" };
    }
    if (args[0] === "symbolic-ref") return { stdout: `${ref}\n`, stderr: "" };
    if (args[0] === "status") return { stdout: status, stderr: "" };
    if (args[0] === "fetch") {
      trackingHead = authoritativeHead;
      return { stdout: "", stderr: "" };
    }
    if (args[0] === "rev-parse" && args.at(-1) === "HEAD^{commit}") {
      return { stdout: `${head}\n`, stderr: "" };
    }
    if (
      args[0] === "rev-parse" &&
      args.at(-1) === "refs/remotes/origin/main^{commit}"
    ) return { stdout: `${trackingHead}\n`, stderr: "" };
    if (args[0] === "merge-base") {
      if (fastForward) return { stdout: "", stderr: "" };
      const error = new Error("merge-base rejected a non-fast-forward update");
      error.exitCode = 1;
      throw error;
    }
    if (args[0] === "merge") {
      head = args.at(-1);
      return { stdout: "", stderr: "" };
    }
    throw new Error(`unexpected test Git command: ${args.join(" ")}`);
  };
  return { calls, git };
}

function requiredEntries() {
  return [
    { path: "nanocodex/wrangler.json", body: "{}\n" },
    { path: "client/index.html", body: "<main></main>\n" },
    { path: "client/assets/app.js", body: "export {};\n", mode: 0o644 },
  ];
}

function tarArchive(entries) {
  const parts = [];
  for (const entry of entries) {
    const header = Buffer.alloc(512);
    const body = Buffer.from(entry.body ?? "");
    writeTarString(header, 0, 100, entry.path);
    writeTarOctal(header, 100, 8, entry.mode ?? (entry.type === "5" ? 0o755 : 0o644));
    writeTarOctal(header, 108, 8, 0);
    writeTarOctal(header, 116, 8, 0);
    writeTarOctal(header, 124, 12, body.length);
    writeTarOctal(header, 136, 12, 0);
    header.fill(0x20, 148, 156);
    header[156] = (entry.type ?? "0").charCodeAt(0);
    if (entry.link) writeTarString(header, 157, 100, entry.link);
    header.write("ustar\0", 257, 6, "latin1");
    header.write("00", 263, 2, "ascii");
    header.write("root", 265, 4, "ascii");
    header.write("root", 297, 4, "ascii");
    const checksum = header.reduce((sum, byte) => sum + byte, 0);
    const checksumText = checksum.toString(8).padStart(6, "0");
    header.write(checksumText, 148, 6, "ascii");
    header[154] = 0;
    header[155] = 0x20;
    parts.push(header, body, Buffer.alloc((512 - (body.length % 512)) % 512));
  }
  parts.push(Buffer.alloc(1024));
  return Buffer.concat(parts);
}

function writeTarString(header, offset, length, value) {
  const body = Buffer.from(value, "utf8");
  assert.ok(body.length < length, `test tar path exceeds ${length} bytes`);
  body.copy(header, offset);
}

function writeTarOctal(header, offset, length, value) {
  const body = value.toString(8).padStart(length - 1, "0");
  assert.ok(body.length < length, "test tar number overflow");
  header.write(body, offset, length - 1, "ascii");
  header[offset + length - 1] = 0;
}
