import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { test } from "node:test";

import {
  FORBIDDEN_RUNNER_ENVIRONMENT_KEYS,
  KEYCHAIN_ACCOUNT,
  KEYCHAIN_SERVICE,
  LOG_ROTATION_INTERVAL_SECONDS,
  MAX_CURRENT_LOG_BYTES,
  RETAINED_LOG_BYTES,
  SERVICE_LABEL,
  THROTTLE_INTERVAL_SECONDS,
  assertNoColocatedControllerState,
  classifyKeychainFind,
  classifyLaunchctlPrint,
  deriveServicePaths,
  keychainDeleteArguments,
  keychainFindArguments,
  keychainStoreArguments,
  launchAgentProgramArguments,
  launchctlArguments,
  parseCliArguments,
  renderLaunchAgentPlist,
  renderRunnerEnvironmentProbeProgram,
  renderRunnerWrapper,
  serviceRunbook,
  validateAbsolutePath,
  validateAccountName,
  validateHost,
  validateNodeIdentity,
  validateOrigin,
  validateRunnerId,
  validateServicePaths,
} from "./install-ci-macos-service.mjs";

const HOME = "/Users/ci-runner";
const USERNAME = "ci-runner";
const UID = typeof process.getuid === "function" ? process.getuid() : 501;

test("origin, runner ID, host, and absolute path validation are strict", () => {
  assert.equal(validateOrigin("https://ci.example.com"), "https://ci.example.com");
  assert.equal(validateOrigin("https://ci.example.com:8443/"), "https://ci.example.com:8443");
  for (const invalid of [
    "http://ci.example.com",
    "http://127.0.0.1:8787",
    "https://user:secret@ci.example.com",
    "https://ci.example.com/api",
    "https://ci.example.com/?query=yes",
    "https://ci.example.com/#fragment",
    "https://ci.example.com/a/..",
    "https://ci.example.com/%2e",
    " https://ci.example.com",
  ]) {
    assert.throws(() => validateOrigin(invalid), /HTTPS|origin/);
  }

  assert.equal(validateRunnerId("mac-studio.arm64:01"), "mac-studio.arm64:01");
  for (const invalid of ["", "-runner", "runner/id", "runner id", "a".repeat(201)]) {
    assert.throws(() => validateRunnerId(invalid), /runner ID/);
  }
  assert.equal(validateNodeIdentity("node/darwin/arm64"), "node/darwin/arm64");
  assert.throws(() => validateNodeIdentity("node/darwin/x64"), /node\/darwin\/arm64/);
  assert.throws(() => validateNodeIdentity("bun/darwin/arm64"), /node\/darwin\/arm64/);

  assert.equal(validateAbsolutePath("/opt/homebrew/bin/node", "Node"), "/opt/homebrew/bin/node");
  for (const invalid of [
    "node",
    "./node",
    "/opt/homebrew/../tmp/node",
    "/opt/homebrew/bin/node/",
    "/opt/homebrew/bin/node\n--unsafe",
  ]) {
    assert.throws(() => validateAbsolutePath(invalid, "Node"), /normalized absolute path/);
  }

  assert.deepEqual(
    validateHost({
      platform: "darwin",
      arch: "arm64",
      uid: 501,
      euid: 501,
      homeDirectory: HOME,
      username: USERNAME,
    }),
    { uid: 501, guiDomain: "gui/501", homeDirectory: HOME, username: USERNAME },
  );
  assert.throws(
    () => validateHost({
      platform: "darwin",
      arch: "x64",
      uid: 501,
      homeDirectory: HOME,
      username: USERNAME,
    }),
    /darwin\/arm64/,
  );
  assert.throws(
    () => validateHost({
      platform: "linux",
      arch: "arm64",
      uid: 501,
      homeDirectory: HOME,
      username: USERNAME,
    }),
    /darwin\/arm64/,
  );
  assert.throws(
    () => validateHost({
      platform: "darwin",
      arch: "arm64",
      uid: 0,
      homeDirectory: "/var/root",
      username: "root",
    }),
    /non-root/,
  );
  assert.throws(
    () => validateHost({
      platform: "darwin",
      arch: "arm64",
      uid: 501,
      euid: 0,
      homeDirectory: HOME,
      username: USERNAME,
    }),
    /sudo/,
  );
  assert.equal(validateAccountName("ci_runner-01"), "ci_runner-01");
  for (const invalid of ["", "runner/name", "runner name", "-runner", "a".repeat(256)]) {
    assert.throws(() => validateAccountName(invalid), /account name/);
  }
});

test("the per-user service layout is fixed and rejects escaped log or plist paths", () => {
  const paths = deriveServicePaths(HOME);
  assert.deepEqual(paths, {
    homeDirectory: HOME,
    libraryDirectory: `${HOME}/Library`,
    applicationSupportDirectory: `${HOME}/Library/Application Support`,
    vendorDirectory: `${HOME}/Library/Application Support/nanocodex`,
    serviceDirectory: `${HOME}/Library/Application Support/nanocodex/ci-macos-runner`,
    logsDirectory: `${HOME}/Library/Application Support/nanocodex/ci-macos-runner/logs`,
    temporaryDirectory: `${HOME}/Library/Application Support/nanocodex/ci-macos-runner/tmp`,
    runnerPath: `${HOME}/Library/Application Support/nanocodex/ci-macos-runner/ci-macos-runner.mjs`,
    wrapperPath: `${HOME}/Library/Application Support/nanocodex/ci-macos-runner/run-ci-macos-runner.sh`,
    stdoutPath: `${HOME}/Library/Application Support/nanocodex/ci-macos-runner/logs/stdout.log`,
    stderrPath: `${HOME}/Library/Application Support/nanocodex/ci-macos-runner/logs/stderr.log`,
    stdoutArchivePath: `${HOME}/Library/Application Support/nanocodex/ci-macos-runner/logs/stdout.log.1`,
    stderrArchivePath: `${HOME}/Library/Application Support/nanocodex/ci-macos-runner/logs/stderr.log.1`,
    launchAgentsDirectory: `${HOME}/Library/LaunchAgents`,
    plistPath: `${HOME}/Library/LaunchAgents/${SERVICE_LABEL}.plist`,
  });
  assert.equal(validateServicePaths(paths), paths);
  assert.throws(
    () => validateServicePaths({ ...paths, stdoutPath: "/tmp/stdout.log" }),
    /fixed per-user service layout/,
  );
  assert.throws(
    () => validateServicePaths({
      ...paths,
      plistPath: `${HOME}/Library/Application Support/nanocodex/ci-macos-runner/agent.plist`,
    }),
    /fixed per-user service layout/,
  );
  assert.throws(() => deriveServicePaths("/"), /filesystem root/);
  assert.throws(() => validateServicePaths({ ...paths, extra: "/tmp" }), /fixed service layout/);
});

test("LaunchAgent plist has one fixed wrapper, failure-only keepalive, throttling, and bounded logs", () => {
  const paths = deriveServicePaths("/Users/CI & Build");
  const identity = { username: USERNAME, uid: UID };
  const plist = renderLaunchAgentPlist(paths, identity);
  const programArguments = launchAgentProgramArguments(paths, identity);
  assert.match(plist, /^<\?xml version="1\.0" encoding="UTF-8"\?>/);
  assert.match(plist, new RegExp(`<key>Label</key>\\s*<string>${SERVICE_LABEL}</string>`));
  assert.match(plist, /<key>Program<\/key>\s*<string>\/usr\/bin\/env<\/string>/);
  assert.match(
    plist,
    /<key>ProgramArguments<\/key>\s*<array>\s*<string>\/usr\/bin\/env<\/string>\s*<string>-i<\/string>[\s\S]*?<string>HOME=\/Users\/CI &amp; Build<\/string>[\s\S]*?<string>USER=ci-runner<\/string>[\s\S]*?<string>LOGNAME=ci-runner<\/string>[\s\S]*?<string>PATH=\/usr\/bin:\/bin:\/usr\/sbin:\/sbin<\/string>[\s\S]*?<string>TMPDIR=\/Users\/CI &amp; Build\/Library\/Application Support\/nanocodex\/ci-macos-runner\/tmp<\/string>[\s\S]*?<string>LANG=en_US.UTF-8<\/string>[\s\S]*?<string>__CF_USER_TEXT_ENCODING=0x[0-9A-F]+:0x0:0x0<\/string>[\s\S]*?<string>\/Users\/CI &amp; Build\/Library\/Application Support\/nanocodex\/ci-macos-runner\/run-ci-macos-runner\.sh<\/string>\s*<\/array>/,
  );
  assert.match(plist, /<key>RunAtLoad<\/key>\s*<true\/>/);
  assert.match(
    plist,
    /<key>KeepAlive<\/key>\s*<dict>\s*<key>SuccessfulExit<\/key>\s*<false\/>\s*<\/dict>/,
  );
  assert.match(
    plist,
    new RegExp(`<key>ThrottleInterval</key>\\s*<integer>${THROTTLE_INTERVAL_SECONDS}</integer>`),
  );
  assert.match(
    plist,
    /<key>StandardOutPath<\/key>\s*<string>\/Users\/CI &amp; Build\/Library\/Application Support\/nanocodex\/ci-macos-runner\/logs\/stdout\.log<\/string>/,
  );
  assert.match(
    plist,
    /<key>StandardErrorPath<\/key>\s*<string>\/Users\/CI &amp; Build\/Library\/Application Support\/nanocodex\/ci-macos-runner\/logs\/stderr\.log<\/string>/,
  );
  assert.match(plist, /<key>Umask<\/key>\s*<string>077<\/string>/);
  assert.match(plist, /<key>SoftResourceLimits<\/key>[\s\S]*?<key>Core<\/key>\s*<integer>0<\/integer>/);
  assert.doesNotMatch(plist, /NANOCODEX_CI_MACOS_TOKEN|EnvironmentVariables|https:\/\//);
  assert.deepEqual(programArguments, [
    "/usr/bin/env",
    "-i",
    "HOME=/Users/CI & Build",
    `USER=${USERNAME}`,
    `LOGNAME=${USERNAME}`,
    "PATH=/usr/bin:/bin:/usr/sbin:/sbin",
    "TMPDIR=/Users/CI & Build/Library/Application Support/nanocodex/ci-macos-runner/tmp",
    "LANG=en_US.UTF-8",
    `__CF_USER_TEXT_ENCODING=0x${UID.toString(16).toUpperCase()}:0x0:0x0`,
    paths.wrapperPath,
  ]);
  if (process.platform === "darwin") {
    const converted = spawnSync(
      "/usr/bin/plutil",
      ["-convert", "json", "-o", "-", "-"],
      { input: plist, encoding: "utf8" },
    );
    assert.equal(converted.status, 0, converted.stderr);
    const parsed = JSON.parse(converted.stdout);
    assert.equal(parsed.Program, "/usr/bin/env");
    assert.deepEqual(parsed.ProgramArguments, programArguments);
    assert.equal(parsed.EnvironmentVariables, undefined);
  }
});

test("wrapper reads only from Keychain and execs the selected Node binary without a secret argument", () => {
  const paths = deriveServicePaths("/Users/CI Operator's Home");
  const wrapper = renderRunnerWrapper({
    paths,
    nodeBinary: "/Applications/Node's Runtime/bin/node",
    origin: "https://ci.example.com",
    runnerId: "mac-studio-01",
    username: USERNAME,
    uid: UID,
  });
  assert.match(wrapper, /^#!\/bin\/sh\nset -eu\numask 077\n/);
  assert.match(
    wrapper,
    /readonly NODE_BINARY='\/Applications\/Node'"'"'s Runtime\/bin\/node'/,
  );
  assert.match(
    wrapper,
    /readonly RUNNER_PATH='\/Users\/CI Operator'"'"'s Home\/Library\/Application Support\/nanocodex\/ci-macos-runner\/ci-macos-runner\.mjs'/,
  );
  assert.match(wrapper, /export NANOCODEX_CI_ORIGIN='https:\/\/ci\.example\.com'/);
  assert.match(wrapper, /export NANOCODEX_CI_MACOS_RUNNER_ID='mac-studio-01'/);
  assert.match(wrapper, /rejected ambient authority/);
  for (const name of [
    "NODE_OPTIONS", "NODE_PATH", "BASH_ENV", "ENV", "DYLD_INSERT_LIBRARIES",
    "LD_PRELOAD", "SSH_AUTH_SOCK", "GITHUB_TOKEN", "R2_SECRET_ACCESS_KEY",
    "CI_RELEASE_TOKEN", "NANOCODEX_SANDBOX_CONTROL_TOKEN",
    "NANOCODEX_CI_CONTROL_TOKEN",
  ]) {
    assert.ok(FORBIDDEN_RUNNER_ENVIRONMENT_KEYS.includes(name), name);
    assert.match(wrapper, new RegExp(`\\$\\{${name}\\+x\\}`));
  }
  assert.match(wrapper, /export USER='ci-runner'/);
  assert.match(wrapper, /export LOGNAME='ci-runner'/);
  assert.match(wrapper, /export SHLVL='0'/);
  assert.match(wrapper, /export TMPDIR='\/Users\/CI Operator'"'"'s Home\/Library\/Application Support\/nanocodex\/ci-macos-runner\/tmp'/);
  assert.match(wrapper, /private temporary directory is unsafe/);
  assert.match(wrapper, /'%u:%Mp:%Lp'/);
  assert.match(wrapper, new RegExp(`readonly MAX_CURRENT_LOG_BYTES=${MAX_CURRENT_LOG_BYTES}`));
  assert.match(wrapper, new RegExp(`readonly RETAINED_LOG_BYTES=${RETAINED_LOG_BYTES}`));
  assert.match(
    wrapper,
    new RegExp(`readonly LOG_ROTATION_INTERVAL_SECONDS=${LOG_ROTATION_INTERVAL_SECONDS}`),
  );
  assert.match(wrapper, /\/usr\/bin\/tail -c "\$RETAINED_LOG_BYTES" "\$log_path"/);
  assert.match(wrapper, /: > "\$log_path"/);
  assert.match(wrapper, /rotate_logs <\/dev\/null >\/dev\/null 2>&1 &/);
  assert.match(
    wrapper,
    new RegExp(
      `/usr/bin/security find-generic-password -a "\\$KEYCHAIN_ACCOUNT" -s "\\$KEYCHAIN_SERVICE" -w`,
    ),
  );
  assert.match(wrapper, /export NANOCODEX_CI_MACOS_TOKEN\n\nunset PWD OLDPWD SHLVL _/);
  assert.match(wrapper, /"\$NODE_BINARY" --eval/);
  assert.match(wrapper, /exec "\$NODE_BINARY" "\$RUNNER_PATH"/);
  assert.doesNotMatch(wrapper, /exec[^\n]*NANOCODEX_CI_MACOS_TOKEN/);
  assert.doesNotMatch(wrapper, /set -x|printenv|env "?NANOCODEX_CI_MACOS_TOKEN/);
  assert.ok(
    wrapper.indexOf("rotate_logs </dev/null") <
      wrapper.indexOf("security find-generic-password"),
    "log monitor must start before the Keychain token is loaded",
  );
  const parsed = spawnSync("/bin/sh", ["-n"], {
    input: wrapper,
    encoding: "utf8",
  });
  assert.equal(parsed.status, 0, parsed.stderr);
});

test("plist isolation clears pre-exec injection and the token-bearing Node probe rejects ambient authority", async () => {
  const paths = deriveServicePaths(HOME);
  const arguments_ = launchAgentProgramArguments(paths, { username: USERNAME, uid: UID });
  const observed = spawnSync(arguments_[0], [
    ...arguments_.slice(1, -1),
    process.execPath,
    "--eval",
    "process.stdout.write(JSON.stringify(process.env))",
  ], {
    env: {
      ...process.env,
      NODE_OPTIONS: "--require=/definitely/not/a/real/preload.cjs",
      NODE_PATH: "/untrusted/node/modules",
      BASH_ENV: "/untrusted/shell-startup",
      ENV: "/untrusted/shell-env",
      GITHUB_TOKEN: "ambient-github-value",
      R2_SECRET_ACCESS_KEY: "ambient-r2-value",
      NANOCODEX_CI_CONTROL_TOKEN: "ambient-controller-value",
    },
    encoding: "utf8",
  });
  assert.equal(observed.status, 0, observed.stderr);
  assert.deepEqual(JSON.parse(observed.stdout), {
    HOME,
    USER: USERNAME,
    LOGNAME: USERNAME,
    PATH: "/usr/bin:/bin:/usr/sbin:/sbin",
    TMPDIR: paths.temporaryDirectory,
    LANG: "en_US.UTF-8",
    __CF_USER_TEXT_ENCODING: `0x${UID.toString(16).toUpperCase()}:0x0:0x0`,
  });

  const scratch = await mkdtemp(resolve(tmpdir(), "macos-service-preexec-"));
  try {
    const startup = resolve(scratch, "ambient-startup.sh");
    const marker = resolve(scratch, "executed");
    await writeFile(startup, `printf injected > '${marker}'\n`, { mode: 0o600 });
    const shell = spawnSync(arguments_[0], [
      ...arguments_.slice(1, -1),
      "/bin/bash",
      "-c",
      "exit 0",
    ], {
      env: { ...process.env, BASH_ENV: startup, ENV: startup },
      encoding: "utf8",
    });
    assert.equal(shell.status, 0, shell.stderr);
    await assert.rejects(readFile(marker, "utf8"), /ENOENT/);
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }

  const fixedEnvironment = {
    HOME,
    USER: USERNAME,
    LOGNAME: USERNAME,
    PATH: "/trusted/node:/usr/bin:/bin",
    TMPDIR: paths.temporaryDirectory,
    LANG: "en_US.UTF-8",
    SHLVL: "0",
    __CF_USER_TEXT_ENCODING: `0x${UID.toString(16).toUpperCase()}:0x0:0x0`,
    NANOCODEX_CI_ORIGIN: "https://ci.example.test",
    NANOCODEX_CI_MACOS_RUNNER_ID: "macos-arm64-1",
  };
  const program = renderRunnerEnvironmentProbeProgram({
    nodeIdentity: [process.release.name, process.platform, process.arch].join("/"),
    fixedEnvironment,
  });
  const clean = { ...fixedEnvironment, NANOCODEX_CI_MACOS_TOKEN: "runner-value" };
  const accepted = spawnSync(process.execPath, ["--eval", program], {
    env: clean,
    encoding: "utf8",
  });
  assert.equal(accepted.status, 0, accepted.stderr);
  for (const name of ["NODE_OPTIONS", "SSH_AUTH_SOCK", "GITHUB_TOKEN", "R2_ACCESS_KEY_ID"]) {
    const rejected = spawnSync(process.execPath, ["--eval", program], {
      env: { ...clean, [name]: "ambient-secret-value" },
      encoding: "utf8",
    });
    assert.equal(rejected.status, 78, name);
    assert.doesNotMatch(rejected.stderr, /ambient-secret-value|runner-value/);
  }
});

test("macOS runner account rejects either controller's loaded, state, plist, or Keychain metadata", () => {
  const empty = {
    master: {
      loaded: false,
      plist: false,
      state: false,
      keychainItems: [false, false, false, false],
    },
    pr: { loaded: false, plist: false, state: false, keychainItems: [false, false] },
  };
  assert.equal(assertNoColocatedControllerState(empty), true);
  for (const [role, field] of [
    ["master", "loaded"],
    ["master", "plist"],
    ["master", "state"],
    ["pr", "loaded"],
    ["pr", "plist"],
    ["pr", "state"],
  ]) {
    const changed = structuredClone(empty);
    changed[role][field] = true;
    assert.throws(
      () => assertNoColocatedControllerState(changed),
      /four distinct dedicated identities/,
      `${role} ${field}`,
    );
  }
  for (const role of ["master", "pr"]) {
    const changed = structuredClone(empty);
    changed[role].keychainItems[1] = true;
    assert.throws(
      () => assertNoColocatedControllerState(changed),
      /four distinct dedicated identities/,
    );
  }
  assert.throws(
    () => assertNoColocatedControllerState({ master: empty.master }),
    /co-location state is invalid/,
  );
});

test("Keychain and launchctl argument helpers never carry a token and target only the user GUI domain", () => {
  const secret = "fixture-secret-that-must-never-appear";
  const store = keychainStoreArguments();
  assert.deepEqual(store.slice(-2), ["-U", "-w"]);
  assert.equal(store.at(-1), "-w", "-w must be final so security prompts");
  assert.ok(!store.includes(secret));
  assert.ok(!store.includes("-A"), "Keychain item must not allow every application");
  assert.deepEqual(keychainFindArguments(), [
    "find-generic-password",
    "-a",
    KEYCHAIN_ACCOUNT,
    "-s",
    KEYCHAIN_SERVICE,
  ]);
  assert.deepEqual(keychainDeleteArguments(), [
    "delete-generic-password",
    "-a",
    KEYCHAIN_ACCOUNT,
    "-s",
    KEYCHAIN_SERVICE,
  ]);

  const paths = deriveServicePaths(HOME);
  assert.deepEqual(launchctlArguments("bootstrap", { uid: 502, paths }), [
    "bootstrap",
    "gui/502",
    paths.plistPath,
  ]);
  assert.deepEqual(launchctlArguments("bootout", { uid: 502, paths }), [
    "bootout",
    `gui/502/${SERVICE_LABEL}`,
  ]);
  assert.deepEqual(launchctlArguments("kickstart", { uid: 502, paths }), [
    "kickstart",
    "-k",
    `gui/502/${SERVICE_LABEL}`,
  ]);
  assert.deepEqual(launchctlArguments("print", { uid: 502, paths }), [
    "print",
    `gui/502/${SERVICE_LABEL}`,
  ]);
  assert.throws(() => launchctlArguments("bootstrap", { uid: 0, paths }), /non-root/);
  assert.throws(() => launchctlArguments("load", { uid: 502, paths }), /unsupported/);

  assert.equal(classifyLaunchctlPrint({ code: 0 }), true);
  assert.equal(classifyLaunchctlPrint({ code: 113 }), false);
  assert.equal(classifyLaunchctlPrint({
    code: 113,
    stderr: "Bad request.\nCould not find service \"me.nanocodex.absent\" " +
      "in domain for user gui: 502\n",
  }), false);
  assert.throws(
    () => classifyLaunchctlPrint({ code: 113, stderr: "permission denied\n" }),
    /failed with exit 113/,
  );
  assert.throws(
    () => classifyLaunchctlPrint({ code: 1, stderr: "Could not find service in domain" }),
    /failed with exit 1/,
  );
  assert.throws(
    () => classifyLaunchctlPrint({ code: 77, stderr: "permission denied" }),
    /failed with exit 77/,
  );
  assert.throws(
    () => classifyLaunchctlPrint({
      code: 77,
      stderr: "permission denied; service not found",
    }),
    /failed with exit 77/,
  );
  assert.throws(
    () => classifyLaunchctlPrint({ code: 113, signal: "SIGTERM" }),
    /terminated by SIGTERM/,
  );
  assert.equal(classifyKeychainFind({ code: 0 }), true);
  assert.equal(classifyKeychainFind({ code: 44 }), false);
  assert.equal(classifyKeychainFind({
    code: 44,
    stderr: "security: SecKeychainSearchCopyNext: The specified item could not be found " +
      "in the keychain.\n",
  }), false);
  assert.throws(
    () => classifyKeychainFind({ code: 44, stderr: "interaction is not allowed\n" }),
    /failed with exit 44/,
  );
  assert.throws(
    () => classifyKeychainFind({
      code: 1,
      stderr: "The specified item could not be found in the keychain.",
    }),
    /failed with exit 1/,
  );
  assert.throws(
    () => classifyKeychainFind({ code: 36, stderr: "user interaction is not allowed" }),
    /failed with exit 36/,
  );
  assert.throws(
    () => classifyKeychainFind({
      code: 36,
      stderr: "interaction is not allowed; item not found",
    }),
    /failed with exit 36/,
  );
  assert.throws(
    () => classifyKeychainFind({ code: 44, signal: "SIGKILL" }),
    /terminated by SIGKILL/,
  );
});

test("CLI has explicit lifecycle commands, no token argv, and preserves logs by default", () => {
  assert.deepEqual(parseCliArguments([
    "install",
    "--origin",
    "https://ci.example.com/",
    "--runner-id",
    "mac-01",
    "--node",
    "/opt/homebrew/bin/node",
  ]), {
    command: "install",
    origin: "https://ci.example.com",
    runnerId: "mac-01",
    nodeBinary: "/opt/homebrew/bin/node",
    replaceToken: false,
  });
  assert.deepEqual(parseCliArguments([
    "update",
    "--node",
    "/opt/homebrew/bin/node",
    "--replace-token",
    "--runner-id",
    "mac-01",
    "--origin",
    "https://ci.example.com",
  ]), {
    command: "update",
    origin: "https://ci.example.com",
    runnerId: "mac-01",
    nodeBinary: "/opt/homebrew/bin/node",
    replaceToken: true,
  });
  assert.deepEqual(parseCliArguments(["uninstall"]), {
    command: "uninstall",
    removeLogs: false,
  });
  assert.deepEqual(parseCliArguments(["uninstall", "--remove-logs"]), {
    command: "uninstall",
    removeLogs: true,
  });
  assert.deepEqual(parseCliArguments(["status"]), { command: "status" });
  assert.throws(
    () => parseCliArguments([
      "install",
      "--origin",
      "https://ci.example.com",
      "--runner-id",
      "mac-01",
      "--node",
      "/opt/homebrew/bin/node",
      "--token",
      "argv-secret",
    ]),
    /unknown install option: --token/,
  );
  assert.throws(
    () => parseCliArguments([
      "install",
      "--origin",
      "https://ci.example.com",
      "--runner-id",
      "mac-01",
      "--node",
      "/opt/homebrew/bin/node",
      "--replace-token",
    ]),
    /update only/,
  );

  const runbook = serviceRunbook();
  assert.match(runbook, /dedicated arm64 macOS login user/);
  assert.match(runbook, /four identities distinct/);
  assert.match(runbook, /\/usr\/bin\/env -i/);
  assert.match(runbook, /Do not install GitHub, deploy, package-registry/);
  assert.match(runbook, /stores it in Keychain/);
  assert.match(runbook, /update --replace-token/);
  assert.match(runbook, new RegExp(`${RETAINED_LOG_BYTES}-byte archive`));
});
