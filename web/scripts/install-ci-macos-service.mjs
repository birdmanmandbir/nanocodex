#!/usr/bin/env node

import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { constants as fsConstants } from "node:fs";
import {
  access,
  chmod,
  copyFile,
  lstat,
  mkdir,
  open,
  realpath,
  rename,
  rm,
  stat,
} from "node:fs/promises";
import { arch, platform, userInfo } from "node:os";
import {
  basename,
  dirname,
  isAbsolute,
  join,
  normalize,
  relative,
  resolve,
} from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

export const SERVICE_LABEL = "me.nanocodex.ci-macos-runner";
export const KEYCHAIN_SERVICE = `${SERVICE_LABEL}.token`;
export const KEYCHAIN_ACCOUNT = "runner";
export const THROTTLE_INTERVAL_SECONDS = 30;
export const LOG_ROTATION_INTERVAL_SECONDS = 30;
export const MAX_CURRENT_LOG_BYTES = 8 * 1024 * 1024;
export const RETAINED_LOG_BYTES = 4 * 1024 * 1024;

const SECURITY = "/usr/bin/security";
const LAUNCHCTL = "/bin/launchctl";
const ENV = "/usr/bin/env";
const SYSTEM_PATH = "/usr/bin:/bin:/usr/sbin:/sbin";
const RUNNER_SOURCE = fileURLToPath(new URL("./ci-macos-runner.mjs", import.meta.url));
const RUNNER_ID = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,199}$/;
const ACCOUNT_NAME = /^[A-Za-z0-9_][A-Za-z0-9._-]{0,254}$/;
const CONTROLLER_ROLES = Object.freeze([
  Object.freeze({
    name: "master",
    label: "me.nanocodex.ci-controller.master",
    keychainAccounts: Object.freeze([
      "CI_MASTER_SOURCE_WRITE_TOKEN",
      "NANOCODEX_GITHUB_STATUS_TOKEN",
      "CLOUDFLARE_API_TOKEN",
      "NANOCODEX_GIT_TOKEN",
    ]),
  }),
  Object.freeze({
    name: "pr",
    label: "me.nanocodex.ci-controller.pr",
    keychainAccounts: Object.freeze([
      "CI_PR_SOURCE_WRITE_TOKEN",
      "NANOCODEX_GITHUB_STATUS_TOKEN",
    ]),
  }),
]);
const CREDENTIAL_ENVIRONMENT_KEYS = Object.freeze([
  "GITHUB_TOKEN",
  "GH_TOKEN",
  "GITHUB_PAT",
  "GIT_ASKPASS",
  "GIT_SSH_COMMAND",
  "SSH_AUTH_SOCK",
  "NPM_TOKEN",
  "NODE_AUTH_TOKEN",
  "CARGO_REGISTRY_TOKEN",
  "CARGO_REGISTRIES_CRATES_IO_TOKEN",
  "DOCKER_AUTH_CONFIG",
  "CLOUDFLARE_API_TOKEN",
  "CLOUDFLARE_API_KEY",
  "CLOUDFLARE_EMAIL",
  "AWS_ACCESS_KEY_ID",
  "AWS_SECRET_ACCESS_KEY",
  "AWS_SESSION_TOKEN",
  "GOOGLE_APPLICATION_CREDENTIALS",
  "AZURE_CLIENT_ID",
  "AZURE_CLIENT_SECRET",
  "AZURE_TENANT_ID",
  "NANOCODEX_CI_TOKEN",
  "NANOCODEX_CI_SOURCE_TOKEN",
  "NANOCODEX_CI_CONTROL_TOKEN",
  "NANOCODEX_CI_MACOS_TOKEN",
]);
export const FORBIDDEN_RUNNER_ENVIRONMENT_KEYS = Object.freeze([
  ...new Set([
    ...CREDENTIAL_ENVIRONMENT_KEYS,
    "NODE_OPTIONS",
    "NODE_PATH",
    "BASH_ENV",
    "ENV",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_FALLBACK_FRAMEWORK_PATH",
    "DYLD_IMAGE_SUFFIX",
    "DYLD_PRINT_LIBRARIES",
    "DYLD_PRINT_APIS",
    "DYLD_PRINT_ENV",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "SSH_AGENT_PID",
    "GIT_SSH",
    "GIT_SSH_VARIANT",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "CF_API_TOKEN",
    "CLOUDFLARE_ACCOUNT_ID",
    "R2_ACCESS_KEY_ID",
    "R2_SECRET_ACCESS_KEY",
    "CI_CONTROL_TOKEN",
    "CI_MACOS_RUNNER_TOKEN",
    "CI_RELEASE_TOKEN",
    "NANOCODEX_SANDBOX_CONTROL_TOKEN",
    "CI_MASTER_SOURCE_WRITE_TOKEN",
    "CI_PR_SOURCE_WRITE_TOKEN",
    "NANOCODEX_GITHUB_STATUS_TOKEN",
    "NANOCODEX_CI_MACOS_RUNNER_TOKEN",
    "NANOCODEX_GIT_MIRROR_TOKEN",
    "NANOCODEX_GIT_TOKEN",
    "NANOCODEX_RELEASE_TOKEN",
    "OPENAI_API_KEY",
  ]),
]);
const PATH_KEYS = Object.freeze([
  "homeDirectory",
  "libraryDirectory",
  "applicationSupportDirectory",
  "vendorDirectory",
  "serviceDirectory",
  "logsDirectory",
  "temporaryDirectory",
  "runnerPath",
  "wrapperPath",
  "stdoutPath",
  "stderrPath",
  "stdoutArchivePath",
  "stderrArchivePath",
  "launchAgentsDirectory",
  "plistPath",
]);

export class ServiceConfigurationError extends Error {
  constructor(message, options) {
    super(message, options);
    this.name = "ServiceConfigurationError";
  }
}

export function validateOrigin(value) {
  if (typeof value !== "string" || value === "" || value.trim() !== value) {
    throw new ServiceConfigurationError("origin must be a non-empty HTTPS origin");
  }
  let url;
  try {
    url = new URL(value);
  } catch (error) {
    throw new ServiceConfigurationError("origin must be a valid HTTPS origin", { cause: error });
  }
  if (
    url.protocol !== "https:" ||
    url.username !== "" ||
    url.password !== "" ||
    url.pathname !== "/" ||
    url.search !== "" ||
    url.hash !== "" ||
    url.hostname === ""
  ) {
    throw new ServiceConfigurationError(
      "origin must contain only an HTTPS scheme, host, and optional port",
    );
  }
  if (value !== url.origin && value !== `${url.origin}/`) {
    throw new ServiceConfigurationError("origin must be written as a canonical HTTPS origin");
  }
  return url.origin;
}

export function validateRunnerId(value) {
  if (typeof value !== "string" || !RUNNER_ID.test(value)) {
    throw new ServiceConfigurationError(
      "runner ID must be 1-200 ASCII letters, digits, dots, underscores, colons, or hyphens",
    );
  }
  return value;
}

export function validateAccountName(value) {
  if (typeof value !== "string" || !ACCOUNT_NAME.test(value)) {
    throw new ServiceConfigurationError("passwd account name is invalid");
  }
  return value;
}

export function validateNodeIdentity(value) {
  if (value !== "node/darwin/arm64") {
    throw new ServiceConfigurationError(
      `selected Node binary must report node/darwin/arm64, received ${String(value)}`,
    );
  }
  return value;
}

export function validateAbsolutePath(value, name = "path") {
  if (
    typeof value !== "string" ||
    value === "" ||
    value.length > 4_096 ||
    value.trim() !== value ||
    /[\u0000-\u001f\u007f]/.test(value) ||
    !isAbsolute(value) ||
    normalize(value) !== value ||
    (value !== "/" && value.endsWith("/"))
  ) {
    throw new ServiceConfigurationError(`${name} must be a normalized absolute path`);
  }
  return value;
}

export function validateHost({
  platform: hostPlatform,
  arch: hostArchitecture,
  uid,
  euid = uid,
  homeDirectory,
  username,
}) {
  if (hostPlatform !== "darwin" || hostArchitecture !== "arm64") {
    throw new ServiceConfigurationError(
      `macOS CI service installation requires darwin/arm64, received ${hostPlatform}/${hostArchitecture}`,
    );
  }
  if (!Number.isSafeInteger(uid) || uid <= 0 || uid > 2_147_483_647) {
    throw new ServiceConfigurationError("a non-root numeric user ID is required");
  }
  if (euid !== uid) {
    throw new ServiceConfigurationError("run the installer directly as the target user, not through sudo");
  }
  const validatedHome = validateAbsolutePath(homeDirectory, "home directory");
  const validatedUsername = validateAccountName(username);
  if (validatedHome === "/" || dirname(validatedHome) === validatedHome) {
    throw new ServiceConfigurationError("home directory must not be a filesystem root");
  }
  return Object.freeze({
    uid,
    guiDomain: `gui/${uid}`,
    homeDirectory: validatedHome,
    username: validatedUsername,
  });
}

export function deriveServicePaths(homeDirectory) {
  const home = validateAbsolutePath(homeDirectory, "home directory");
  if (home === "/" || dirname(home) === home) {
    throw new ServiceConfigurationError("home directory must not be a filesystem root");
  }
  const libraryDirectory = join(home, "Library");
  const applicationSupportDirectory = join(libraryDirectory, "Application Support");
  const vendorDirectory = join(applicationSupportDirectory, "nanocodex");
  const serviceDirectory = join(vendorDirectory, "ci-macos-runner");
  const logsDirectory = join(serviceDirectory, "logs");
  const temporaryDirectory = join(serviceDirectory, "tmp");
  const launchAgentsDirectory = join(libraryDirectory, "LaunchAgents");
  return Object.freeze({
    homeDirectory: home,
    libraryDirectory,
    applicationSupportDirectory,
    vendorDirectory,
    serviceDirectory,
    logsDirectory,
    temporaryDirectory,
    runnerPath: join(serviceDirectory, "ci-macos-runner.mjs"),
    wrapperPath: join(serviceDirectory, "run-ci-macos-runner.sh"),
    stdoutPath: join(logsDirectory, "stdout.log"),
    stderrPath: join(logsDirectory, "stderr.log"),
    stdoutArchivePath: join(logsDirectory, "stdout.log.1"),
    stderrArchivePath: join(logsDirectory, "stderr.log.1"),
    launchAgentsDirectory,
    plistPath: join(launchAgentsDirectory, `${SERVICE_LABEL}.plist`),
  });
}

export function validateServicePaths(paths) {
  if (paths == null || typeof paths !== "object" || Array.isArray(paths)) {
    throw new ServiceConfigurationError("service paths must be an object");
  }
  const keys = Object.keys(paths).sort();
  if (keys.join("\0") !== [...PATH_KEYS].sort().join("\0")) {
    throw new ServiceConfigurationError("service paths must contain only the fixed service layout");
  }
  const expected = deriveServicePaths(paths.homeDirectory);
  for (const key of PATH_KEYS) {
    validateAbsolutePath(paths[key], key);
    if (paths[key] !== expected[key]) {
      throw new ServiceConfigurationError(`${key} is outside the fixed per-user service layout`);
    }
  }
  assertDescendant(paths.logsDirectory, paths.serviceDirectory, "logs directory");
  assertDescendant(paths.temporaryDirectory, paths.serviceDirectory, "temporary directory");
  assertDescendant(paths.stdoutPath, paths.logsDirectory, "stdout path");
  assertDescendant(paths.stderrPath, paths.logsDirectory, "stderr path");
  assertDescendant(paths.stdoutArchivePath, paths.logsDirectory, "stdout archive path");
  assertDescendant(paths.stderrArchivePath, paths.logsDirectory, "stderr archive path");
  assertDescendant(paths.runnerPath, paths.serviceDirectory, "runner path");
  assertDescendant(paths.wrapperPath, paths.serviceDirectory, "wrapper path");
  assertDescendant(paths.plistPath, paths.launchAgentsDirectory, "LaunchAgent plist path");
  return paths;
}

export function launchAgentProgramArguments(paths, { username, uid }) {
  validateServicePaths(paths);
  const account = validateAccountName(username);
  validateGuiUid(uid);
  return Object.freeze([
    ENV,
    "-i",
    `HOME=${paths.homeDirectory}`,
    `USER=${account}`,
    `LOGNAME=${account}`,
    `PATH=${SYSTEM_PATH}`,
    `TMPDIR=${paths.temporaryDirectory}`,
    "LANG=en_US.UTF-8",
    `__CF_USER_TEXT_ENCODING=0x${uid.toString(16).toUpperCase()}:0x0:0x0`,
    paths.wrapperPath,
  ]);
}

export function renderLaunchAgentPlist(paths, identity) {
  validateServicePaths(paths);
  const programArguments = launchAgentProgramArguments(paths, identity);
  const value = (text) => xmlEscape(text);
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${value(SERVICE_LABEL)}</string>
  <key>Program</key>
  <string>${value(ENV)}</string>
  <key>ProgramArguments</key>
  <array>
${programArguments.map((argument) => `    <string>${value(argument)}</string>`).join("\n")}
  </array>
  <key>WorkingDirectory</key>
  <string>${value(paths.serviceDirectory)}</string>
  <key>LimitLoadToSessionType</key>
  <string>Aqua</string>
  <key>ProcessType</key>
  <string>Background</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>${THROTTLE_INTERVAL_SECONDS}</integer>
  <key>ExitTimeOut</key>
  <integer>30</integer>
  <key>Umask</key>
  <string>077</string>
  <key>StandardOutPath</key>
  <string>${value(paths.stdoutPath)}</string>
  <key>StandardErrorPath</key>
  <string>${value(paths.stderrPath)}</string>
  <key>SoftResourceLimits</key>
  <dict>
    <key>Core</key>
    <integer>0</integer>
  </dict>
  <key>HardResourceLimits</key>
  <dict>
    <key>Core</key>
    <integer>0</integer>
  </dict>
</dict>
</plist>
`;
}

export function renderRunnerEnvironmentProbeProgram({
  nodeIdentity = "node/darwin/arm64",
  fixedEnvironment,
}) {
  if (!/^node\/(?:darwin|linux)\/(?:arm64|x64)$/.test(nodeIdentity)) {
    throw new ServiceConfigurationError("runner environment probe Node identity is invalid");
  }
  if (
    fixedEnvironment == null ||
    typeof fixedEnvironment !== "object" ||
    Array.isArray(fixedEnvironment) ||
    Object.values(fixedEnvironment).some((value) => typeof value !== "string")
  ) {
    throw new ServiceConfigurationError("runner environment probe fixed environment is invalid");
  }
  const expectedKeys = [...Object.keys(fixedEnvironment), "NANOCODEX_CI_MACOS_TOKEN"].sort();
  return `const fixed=${JSON.stringify(fixedEnvironment)};` +
    `const keys=Object.keys(process.env).sort();const expected=${JSON.stringify(expectedKeys)};` +
    `const wrong=Object.entries(fixed).filter(([name,value])=>process.env[name]!==value);` +
    `const token=process.env.NANOCODEX_CI_MACOS_TOKEN;` +
    `const identity=[process.release.name,process.platform,process.arch].join("/");` +
    `if(identity!==${JSON.stringify(nodeIdentity)}||typeof token!=="string"||token.length===0||` +
    `wrong.length||JSON.stringify(keys)!==JSON.stringify(expected)){` +
    `process.stderr.write("macOS runner environment failed exact allowlist validation\\n");` +
    `process.exit(78);}`;
}

export function renderRunnerWrapper({ paths, nodeBinary, origin, runnerId, username, uid }) {
  validateServicePaths(paths);
  const node = validateAbsolutePath(nodeBinary, "Node binary");
  const normalizedOrigin = validateOrigin(origin);
  const normalizedRunnerId = validateRunnerId(runnerId);
  const account = validateAccountName(username);
  validateGuiUid(uid);
  const executablePath = [...new Set([
    dirname(node),
    join(paths.homeDirectory, ".cargo", "bin"),
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
  ])].join(":");
  const fixedEnvironment = Object.freeze({
    HOME: paths.homeDirectory,
    USER: account,
    LOGNAME: account,
    PATH: executablePath,
    TMPDIR: paths.temporaryDirectory,
    LANG: "en_US.UTF-8",
    SHLVL: "0",
    __CF_USER_TEXT_ENCODING: `0x${uid.toString(16).toUpperCase()}:0x0:0x0`,
    NANOCODEX_CI_ORIGIN: normalizedOrigin,
    NANOCODEX_CI_MACOS_RUNNER_ID: normalizedRunnerId,
  });
  const fixedExports = Object.entries(fixedEnvironment)
    .map(([name, value]) => `export ${name}=${shellQuote(value)}`)
    .join("\n");
  const forbiddenChecks = FORBIDDEN_RUNNER_ENVIRONMENT_KEYS
    .map((name) => `[ "\${${name}+x}" = x ]`)
    .join(" ||\n   ");
  const environmentProbe = renderRunnerEnvironmentProbeProgram({ fixedEnvironment });

  return `#!/bin/sh
set -eu
umask 077
ulimit -c 0 2>/dev/null || true

# launchd starts /usr/bin/env -i before this interpreter. Reject direct or
# malformed invocation before starting even a credential-free child.
if ${forbiddenChecks}; then
  printf '%s\\n' 'nanocodex macOS CI wrapper rejected ambient authority' >&2
  exit 78
fi
if [ "\${HOME-}" != ${shellQuote(paths.homeDirectory)} ] ||
   [ "\${USER-}" != ${shellQuote(account)} ] ||
   [ "\${LOGNAME-}" != ${shellQuote(account)} ] ||
   [ "\${PATH-}" != ${shellQuote(SYSTEM_PATH)} ] ||
   [ "\${TMPDIR-}" != ${shellQuote(paths.temporaryDirectory)} ] ||
   [ "\${LANG-}" != 'en_US.UTF-8' ] ||
   [ "\${__CF_USER_TEXT_ENCODING-}" != ${shellQuote(`0x${uid.toString(16).toUpperCase()}:0x0:0x0`)} ]; then
  printf '%s\\n' 'nanocodex macOS CI wrapper rejected its launch environment' >&2
  exit 78
fi

readonly NODE_BINARY=${shellQuote(node)}
readonly RUNNER_PATH=${shellQuote(paths.runnerPath)}
readonly EXPECTED_UID=${uid}
readonly KEYCHAIN_SERVICE=${shellQuote(KEYCHAIN_SERVICE)}
readonly KEYCHAIN_ACCOUNT=${shellQuote(KEYCHAIN_ACCOUNT)}
readonly STDOUT_LOG=${shellQuote(paths.stdoutPath)}
readonly STDERR_LOG=${shellQuote(paths.stderrPath)}
readonly STDOUT_ARCHIVE=${shellQuote(paths.stdoutArchivePath)}
readonly STDERR_ARCHIVE=${shellQuote(paths.stderrArchivePath)}
readonly MAX_CURRENT_LOG_BYTES=${MAX_CURRENT_LOG_BYTES}
readonly RETAINED_LOG_BYTES=${RETAINED_LOG_BYTES}
readonly LOG_ROTATION_INTERVAL_SECONDS=${LOG_ROTATION_INTERVAL_SECONDS}

${fixedExports}

if [ -L "$TMPDIR" ] || [ ! -d "$TMPDIR" ] ||
   [ "$(/bin/realpath "$TMPDIR" 2>/dev/null)" != "$TMPDIR" ] ||
   [ "$(/usr/bin/stat -f '%u:%Mp:%Lp' "$TMPDIR" 2>/dev/null)" != "$EXPECTED_UID:0:700" ]; then
  printf '%s\\n' 'nanocodex macOS CI private temporary directory is unsafe' >&2
  exit 78
fi

rotate_log() {
  log_path=$1
  archive_path=$2
  if [ -L "$log_path" ] || [ ! -f "$log_path" ]; then
    return 0
  fi
  log_size=$(/usr/bin/stat -f '%z' "$log_path" 2>/dev/null || printf '0')
  case "$log_size" in
    ''|*[!0-9]*) return 0 ;;
  esac
  if [ "$log_size" -le "$MAX_CURRENT_LOG_BYTES" ]; then
    return 0
  fi
  temporary_path="\${archive_path}.tmp.$$"
  /bin/rm -f "$temporary_path"
  if /usr/bin/tail -c "$RETAINED_LOG_BYTES" "$log_path" > "$temporary_path"; then
    /bin/chmod 600 "$temporary_path"
    /bin/mv -f "$temporary_path" "$archive_path"
    : > "$log_path"
  else
    /bin/rm -f "$temporary_path"
    return 1
  fi
}

if [ ! -x "$NODE_BINARY" ]; then
  printf '%s\\n' 'nanocodex macOS CI Node binary is not executable' >&2
  exit 78
fi
if [ ! -r "$RUNNER_PATH" ]; then
  printf '%s\\n' 'nanocodex macOS CI runner is not readable' >&2
  exit 78
fi

rotate_log "$STDOUT_LOG" "$STDOUT_ARCHIVE" || true
rotate_log "$STDERR_LOG" "$STDERR_ARCHIVE" || true
readonly RUNNER_PID=$$
rotate_logs() {
  while /bin/kill -0 "$RUNNER_PID" 2>/dev/null; do
    /bin/sleep "$LOG_ROTATION_INTERVAL_SECONDS" || return 0
    /bin/kill -0 "$RUNNER_PID" 2>/dev/null || return 0
    rotate_log "$STDOUT_LOG" "$STDOUT_ARCHIVE" || true
    rotate_log "$STDERR_LOG" "$STDERR_ARCHIVE" || true
  done
}
# Start before loading the Keychain item so the log monitor cannot inherit the
# runner token. exec below preserves RUNNER_PID for the lifetime of Node.
rotate_logs </dev/null >/dev/null 2>&1 &

if ! NANOCODEX_CI_MACOS_TOKEN="$(${SECURITY} find-generic-password -a "$KEYCHAIN_ACCOUNT" -s "$KEYCHAIN_SERVICE" -w)"; then
  unset NANOCODEX_CI_MACOS_TOKEN
  printf '%s\\n' 'nanocodex macOS CI token could not be read from Keychain' >&2
  exit 78
fi
if [ -z "$NANOCODEX_CI_MACOS_TOKEN" ]; then
  unset NANOCODEX_CI_MACOS_TOKEN
  printf '%s\\n' 'nanocodex macOS CI token in Keychain is empty' >&2
  exit 78
fi
export NANOCODEX_CI_MACOS_TOKEN

unset PWD OLDPWD SHLVL _
if ! "$NODE_BINARY" --eval ${shellQuote(environmentProbe)}; then
  unset NANOCODEX_CI_MACOS_TOKEN
  printf '%s\\n' 'nanocodex macOS CI environment validation failed' >&2
  exit 78
fi
exec "$NODE_BINARY" "$RUNNER_PATH"
`;
}

export function keychainStoreArguments() {
  // macOS security(1) prompts when -w is the final argument. The secret is
  // therefore never passed through this installer's arguments.
  return Object.freeze([
    "add-generic-password",
    "-a",
    KEYCHAIN_ACCOUNT,
    "-s",
    KEYCHAIN_SERVICE,
    "-D",
    "application password",
    "-l",
    "Nanocodex macOS CI runner token",
    "-T",
    SECURITY,
    "-U",
    "-w",
  ]);
}

export function keychainFindArguments() {
  return Object.freeze([
    "find-generic-password",
    "-a",
    KEYCHAIN_ACCOUNT,
    "-s",
    KEYCHAIN_SERVICE,
  ]);
}

export function keychainDeleteArguments() {
  return Object.freeze([
    "delete-generic-password",
    "-a",
    KEYCHAIN_ACCOUNT,
    "-s",
    KEYCHAIN_SERVICE,
  ]);
}

export function launchctlArguments(action, { uid, paths }) {
  validateServicePaths(paths);
  const context = validateGuiUid(uid);
  switch (action) {
    case "bootstrap":
      return Object.freeze(["bootstrap", context.domain, paths.plistPath]);
    case "bootout":
      return Object.freeze(["bootout", context.service]);
    case "kickstart":
      return Object.freeze(["kickstart", "-k", context.service]);
    case "print":
      return Object.freeze(["print", context.service]);
    default:
      throw new ServiceConfigurationError(`unsupported launchctl action: ${String(action)}`);
  }
}

export function classifyLaunchctlPrint({ code, signal = null, stderr = "" }) {
  if (signal != null) {
    throw new ServiceConfigurationError(
      `launchctl service query was terminated by ${String(signal)}${probeDetail(stderr)}`,
    );
  }
  if (code === 0) return true;
  if (
    code === 113 &&
    (stderr === "" ||
      /^(?:Bad request\.\n)?Could not find service "[^"\r\n]+" in domain for user gui: [1-9][0-9]*\n$/.test(stderr))
  ) return false;
  throw new ServiceConfigurationError(
    `launchctl service query failed with exit ${String(code)}${probeDetail(stderr)}`,
  );
}

export function classifyKeychainFind({ code, signal = null, stderr = "" }) {
  if (signal != null) {
    throw new ServiceConfigurationError(
      `Keychain item query was terminated by ${String(signal)}${probeDetail(stderr)}`,
    );
  }
  if (code === 0) return true;
  if (
    code === 44 &&
    (stderr === "" ||
      stderr ===
        "security: SecKeychainSearchCopyNext: The specified item could not be found " +
          "in the keychain.\n")
  ) return false;
  throw new ServiceConfigurationError(
    `Keychain item query failed with exit ${String(code)}${probeDetail(stderr)}`,
  );
}

export function assertNoColocatedControllerState(value) {
  if (
    value == null ||
    typeof value !== "object" ||
    Array.isArray(value) ||
    Object.keys(value).sort().join("\0") !== CONTROLLER_ROLES.map(({ name }) => name).sort().join("\0")
  ) {
    throw new ServiceConfigurationError("controller co-location state is invalid");
  }
  for (const { name, keychainAccounts } of CONTROLLER_ROLES) {
    const state = value[name];
    if (
      state == null ||
      typeof state !== "object" ||
      Array.isArray(state) ||
      typeof state.loaded !== "boolean" ||
      typeof state.plist !== "boolean" ||
      typeof state.state !== "boolean" ||
      !Array.isArray(state.keychainItems) ||
      state.keychainItems.length !== keychainAccounts.length ||
      state.keychainItems.some((present) => typeof present !== "boolean")
    ) {
      throw new ServiceConfigurationError(`${name} controller co-location state is invalid`);
    }
    if (state.loaded || state.plist || state.state || state.keychainItems.some(Boolean)) {
      throw new ServiceConfigurationError(
        `macOS runner refuses the ${name} controller role in the same passwd account; ` +
          "use four distinct dedicated identities",
      );
    }
  }
  return true;
}

export function parseCliArguments(argv) {
  if (!Array.isArray(argv) || argv.some((value) => typeof value !== "string")) {
    throw new ServiceConfigurationError("command arguments must be strings");
  }
  if (argv.length === 0 || argv[0] === "help" || argv[0] === "--help" || argv[0] === "-h") {
    if (argv.length > 1) throw new ServiceConfigurationError("help accepts no arguments");
    return Object.freeze({ command: "help" });
  }
  const [command, ...arguments_] = argv;
  if (command === "install" || command === "update") {
    const values = new Map();
    let replaceToken = false;
    for (let index = 0; index < arguments_.length; index += 1) {
      const flag = arguments_[index];
      if (flag === "--replace-token") {
        if (command !== "update" || replaceToken) {
          throw new ServiceConfigurationError("--replace-token is accepted once by update only");
        }
        replaceToken = true;
        continue;
      }
      if (flag !== "--origin" && flag !== "--runner-id" && flag !== "--node") {
        throw new ServiceConfigurationError(`unknown ${command} option: ${String(flag)}`);
      }
      if (values.has(flag)) {
        throw new ServiceConfigurationError(`${flag} may be provided only once`);
      }
      const value = arguments_[++index];
      if (value == null || value.startsWith("--")) {
        throw new ServiceConfigurationError(`${flag} requires a value`);
      }
      values.set(flag, value);
    }
    for (const flag of ["--origin", "--runner-id", "--node"]) {
      if (!values.has(flag)) throw new ServiceConfigurationError(`${command} requires ${flag}`);
    }
    return Object.freeze({
      command,
      origin: validateOrigin(values.get("--origin")),
      runnerId: validateRunnerId(values.get("--runner-id")),
      nodeBinary: validateAbsolutePath(values.get("--node"), "Node binary"),
      replaceToken,
    });
  }
  if (command === "uninstall") {
    if (arguments_.length > 1 || (arguments_.length === 1 && arguments_[0] !== "--remove-logs")) {
      throw new ServiceConfigurationError("uninstall accepts only the optional --remove-logs flag");
    }
    return Object.freeze({ command, removeLogs: arguments_[0] === "--remove-logs" });
  }
  if (command === "status") {
    if (arguments_.length !== 0) throw new ServiceConfigurationError("status accepts no arguments");
    return Object.freeze({ command });
  }
  throw new ServiceConfigurationError(`unknown command: ${String(command)}`);
}

export function serviceRunbook() {
  return `Operator assumptions:
  - Run this LaunchAgent from its own dedicated arm64 macOS login user; it must not contain either controller's LaunchAgent, state, or Keychain items.
  - Keep four identities distinct: master controller, PR controller, non-login PR preparation, and macOS runner.
  - Keep only the macOS CI runner token on that account; this installer stores it in Keychain.
  - Do not install GitHub, deploy, package-registry, cloud, or SSH-agent credentials for the account.
  - launchd executes /usr/bin/env -i with only fixed passwd identity, PATH, TMPDIR, and locale fields before the wrapper interpreter starts.
  - stdout/stderr retain one ${RETAINED_LOG_BYTES}-byte archive and rotate a current file above ${MAX_CURRENT_LOG_BYTES} bytes every ${LOG_ROTATION_INTERVAL_SECONDS} seconds.
  - Rotate the runner token with update --replace-token; ordinary update preserves the Keychain item.`;
}

export async function main(argv = process.argv.slice(2)) {
  const options = parseCliArguments(argv);
  if (options.command === "help") {
    process.stdout.write(usage());
    return;
  }
  const context = runtimeContext();
  const paths = deriveServicePaths(context.homeDirectory);
  let result;
  if (options.command === "install" || options.command === "update") {
    result = await installOrUpdate(options, context, paths);
  } else if (options.command === "uninstall") {
    result = await uninstall(options, context, paths);
  } else {
    result = await serviceStatus(context, paths);
  }
  process.stdout.write(`${JSON.stringify(result, null, 2)}\n`);
}

async function installOrUpdate(options, context, paths) {
  process.stderr.write(`${serviceRunbook()}\n`);
  await validateExistingPathChain(paths, context.uid);
  await assertNoColocatedControllerRoles(context);
  const [loaded, artifacts] = await Promise.all([
    isLoaded(context.uid, paths),
    installationArtifacts(paths),
  ]);
  const installed = Boolean(artifacts.plist || artifacts.runner || artifacts.wrapper);
  if (options.command === "install" && (installed || loaded)) {
    throw new ServiceConfigurationError("service artifacts already exist; use update or uninstall");
  }
  if (options.command === "update" && !installed && !loaded) {
    throw new ServiceConfigurationError("service is not installed; use install");
  }
  await assertSafeArtifacts(artifacts, context.uid, false);
  await ensureServiceDirectories(paths, context.uid);
  await assertSafeLogFiles(paths, context.uid, true);

  const [nodeBinary, runnerSource] = await Promise.all([
    canonicalExecutable(options.nodeBinary, "Node binary"),
    canonicalRegularFile(RUNNER_SOURCE, "macOS CI runner source"),
  ]);
  await assertSelectedNode(nodeBinary);
  const hasToken = await keychainItemExists();
  if (options.command === "install" || options.replaceToken || !hasToken) {
    process.stderr.write(
      "Store the macOS CI token in Keychain at the security prompt; the installer never reads it.\n",
    );
    await runChecked(SECURITY, keychainStoreArguments(), {
      operation: "store macOS CI token in Keychain",
      stdio: ["inherit", "ignore", "inherit"],
    });
  }

  const wrapper = renderRunnerWrapper({
    paths,
    nodeBinary,
    origin: options.origin,
    runnerId: options.runnerId,
    username: context.username,
    uid: context.uid,
  });
  const plist = renderLaunchAgentPlist(paths, context);
  await atomicCopyFile(runnerSource, paths.runnerPath, 0o400);
  await atomicWriteFile(paths.wrapperPath, wrapper, 0o500);
  await atomicWriteFile(paths.plistPath, plist, 0o600);

  if (loaded) {
    await runChecked(LAUNCHCTL, launchctlArguments("bootout", {
      uid: context.uid,
      paths,
    }), { operation: "boot out existing LaunchAgent" });
  }
  await runChecked(LAUNCHCTL, launchctlArguments("bootstrap", {
    uid: context.uid,
    paths,
  }), { operation: "bootstrap LaunchAgent" });
  await runChecked(LAUNCHCTL, launchctlArguments("kickstart", {
    uid: context.uid,
    paths,
  }), { operation: "kickstart LaunchAgent" });

  return Object.freeze({
    command: options.command,
    label: SERVICE_LABEL,
    guiDomain: `gui/${context.uid}`,
    serviceDirectory: paths.serviceDirectory,
    plistPath: paths.plistPath,
    origin: options.origin,
    runnerId: options.runnerId,
    nodeBinary,
  });
}

async function uninstall(options, context, paths) {
  validateServicePaths(paths);
  await validateExistingPathChain(paths, context.uid);
  if (options.removeLogs) await assertRemovableServiceDirectory(paths, context.uid);
  const loaded = await isLoaded(context.uid, paths);
  if (loaded) {
    await runChecked(LAUNCHCTL, launchctlArguments("bootout", {
      uid: context.uid,
      paths,
    }), { operation: "boot out LaunchAgent" });
  }
  if (await keychainItemExists()) {
    await runChecked(SECURITY, keychainDeleteArguments(), {
      operation: "delete macOS CI token from Keychain",
      stdio: ["ignore", "ignore", "inherit"],
    });
  }

  // These are exact, non-recursive targets. Logs remain in the fixed service
  // directory unless the operator explicitly selected --remove-logs.
  await rm(paths.plistPath, { force: true });
  await rm(paths.wrapperPath, { force: true });
  await rm(paths.runnerPath, { force: true });
  if (options.removeLogs) {
    assertExactServiceDirectory(paths.serviceDirectory, paths);
    await rm(paths.serviceDirectory, { recursive: true, force: true });
  }
  return Object.freeze({
    command: "uninstall",
    label: SERVICE_LABEL,
    guiDomain: `gui/${context.uid}`,
    serviceDirectory: paths.serviceDirectory,
    logsPreserved: !options.removeLogs,
  });
}

async function serviceStatus(context, paths) {
  validateServicePaths(paths);
  await validateExistingPathChain(paths, context.uid);
  await assertNoColocatedControllerRoles(context);
  await assertSafeLogFiles(paths, context.uid, false);
  const [loaded, artifacts, keychain] = await Promise.all([
    isLoaded(context.uid, paths),
    installationArtifacts(paths),
    keychainItemExists(),
  ]);
  await assertSafeArtifacts(artifacts, context.uid, true);
  return Object.freeze({
    command: "status",
    label: SERVICE_LABEL,
    guiDomain: `gui/${context.uid}`,
    installed: Boolean(artifacts.plist && artifacts.runner && artifacts.wrapper),
    loaded,
    keychainTokenPresent: keychain,
    artifacts: Object.freeze({
      plist: Boolean(artifacts.plist),
      runner: Boolean(artifacts.runner),
      wrapper: Boolean(artifacts.wrapper),
      logsDirectory: await exists(paths.logsDirectory),
    }),
    serviceDirectory: paths.serviceDirectory,
    plistPath: paths.plistPath,
  });
}

function runtimeContext() {
  const getuid = process.getuid;
  const geteuid = process.geteuid;
  if (typeof getuid !== "function" || typeof geteuid !== "function") {
    throw new ServiceConfigurationError("numeric Unix user identity is unavailable");
  }
  const identity = userInfo();
  const uid = getuid.call(process);
  const euid = geteuid.call(process);
  if (identity.uid !== uid || identity.uid !== euid) {
    throw new ServiceConfigurationError("system user identity does not match the running process");
  }
  return validateHost({
    platform: platform(),
    arch: arch(),
    uid,
    euid,
    // userInfo() resolves the account database entry and does not trust a
    // caller-controlled HOME environment variable.
    homeDirectory: identity.homedir,
    username: identity.username,
  });
}

function validateGuiUid(uid) {
  if (!Number.isSafeInteger(uid) || uid <= 0 || uid > 2_147_483_647) {
    throw new ServiceConfigurationError("launchctl requires a non-root numeric user ID");
  }
  const domain = `gui/${uid}`;
  return Object.freeze({ domain, service: `${domain}/${SERVICE_LABEL}` });
}

function assertDescendant(path, parent, name) {
  const suffix = relative(parent, path);
  if (suffix === "" || suffix === ".." || suffix.startsWith("../") || isAbsolute(suffix)) {
    throw new ServiceConfigurationError(`${name} must be contained by ${parent}`);
  }
}

function assertExactServiceDirectory(path, paths) {
  validateServicePaths(paths);
  if (path !== paths.serviceDirectory) {
    throw new ServiceConfigurationError("recursive removal is restricted to the exact service directory");
  }
}

function xmlEscape(value) {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&apos;");
}

function shellQuote(value) {
  return `'${value.replaceAll("'", `'"'"'`)}'`;
}

async function validateExistingPathChain(paths, uid) {
  validateServicePaths(paths);
  await assertDirectory(paths.homeDirectory, uid, { required: true });
  for (const directory of [
    paths.libraryDirectory,
    paths.applicationSupportDirectory,
    paths.vendorDirectory,
    paths.serviceDirectory,
    paths.logsDirectory,
    paths.launchAgentsDirectory,
  ]) {
    await assertDirectory(directory, uid, { required: false });
  }
  const temporary = await pathMetadata(paths.temporaryDirectory);
  if (temporary != null) {
    await assertDirectory(paths.temporaryDirectory, uid, { required: true });
    if ((temporary.mode & 0o777) !== 0o700) {
      throw new ServiceConfigurationError(
        `${paths.temporaryDirectory} must have private mode 700`,
      );
    }
  }
}

async function ensureServiceDirectories(paths, uid) {
  validateServicePaths(paths);
  await assertDirectory(paths.homeDirectory, uid, { required: true });
  await ensureDirectory(paths.libraryDirectory, uid, false);
  await ensureDirectory(paths.applicationSupportDirectory, uid, false);
  await ensureDirectory(paths.vendorDirectory, uid, true);
  await ensureDirectory(paths.serviceDirectory, uid, true);
  await ensureDirectory(paths.logsDirectory, uid, true);
  await ensureDirectory(paths.temporaryDirectory, uid, true);
  await ensureDirectory(paths.launchAgentsDirectory, uid, false);
}

async function ensureDirectory(path, uid, privateDirectory) {
  let created = false;
  try {
    await mkdir(path, { mode: 0o700 });
    created = true;
  } catch (error) {
    if (error?.code !== "EEXIST") throw error;
  }
  await assertDirectory(path, uid, { required: true });
  if (created || privateDirectory) await chmod(path, 0o700);
}

async function assertDirectory(path, uid, { required }) {
  const metadata = await pathMetadata(path);
  if (!metadata) {
    if (required) throw new ServiceConfigurationError(`${path} does not exist`);
    return;
  }
  if (!metadata.isDirectory() || metadata.isSymbolicLink()) {
    throw new ServiceConfigurationError(`${path} must be a real directory, not a link`);
  }
  if (metadata.uid !== uid) {
    throw new ServiceConfigurationError(`${path} must be owned by user ${uid}`);
  }
}

async function installationArtifacts(paths) {
  const [plist, runner, wrapper] = await Promise.all([
    pathMetadata(paths.plistPath),
    pathMetadata(paths.runnerPath),
    pathMetadata(paths.wrapperPath),
  ]);
  return Object.freeze({ plist, runner, wrapper });
}

async function assertSafeArtifacts(artifacts, uid, requireInstalledModes) {
  const installedModes = Object.freeze({ plist: 0o600, runner: 0o400, wrapper: 0o500 });
  for (const [name, metadata] of Object.entries(artifacts)) {
    if (!metadata) continue;
    if (!metadata.isFile() || metadata.isSymbolicLink() || metadata.uid !== uid) {
      throw new ServiceConfigurationError(`existing ${name} artifact is not a user-owned regular file`);
    }
    if (requireInstalledModes && (metadata.mode & 0o777) !== installedModes[name]) {
      throw new ServiceConfigurationError(
        `existing ${name} artifact has unsafe mode ${(metadata.mode & 0o777).toString(8)}`,
      );
    }
  }
}

async function assertSafeLogFiles(paths, uid, tightenPermissions) {
  for (const path of [
    paths.stdoutPath,
    paths.stderrPath,
    paths.stdoutArchivePath,
    paths.stderrArchivePath,
  ]) {
    const metadata = await pathMetadata(path);
    if (!metadata) continue;
    if (
      !metadata.isFile() ||
      metadata.isSymbolicLink() ||
      metadata.uid !== uid ||
      metadata.nlink !== 1
    ) {
      throw new ServiceConfigurationError(
        `${path} must be a singly linked, user-owned regular log file`,
      );
    }
    if (tightenPermissions) await chmod(path, 0o600);
  }
}

async function assertRemovableServiceDirectory(paths, uid) {
  const metadata = await pathMetadata(paths.serviceDirectory);
  if (!metadata) return;
  if (!metadata.isDirectory() || metadata.isSymbolicLink() || metadata.uid !== uid) {
    throw new ServiceConfigurationError(
      "refusing recursive removal because the exact service path is not a user-owned directory",
    );
  }
  assertExactServiceDirectory(paths.serviceDirectory, paths);
}

async function canonicalExecutable(path, name) {
  const requested = validateAbsolutePath(path, name);
  let canonical;
  try {
    canonical = validateAbsolutePath(await realpath(requested), `${name} canonical path`);
    const metadata = await stat(canonical);
    if (!metadata.isFile()) throw new ServiceConfigurationError(`${name} must be a regular file`);
    await access(canonical, fsConstants.X_OK);
  } catch (error) {
    if (error instanceof ServiceConfigurationError) throw error;
    throw new ServiceConfigurationError(`${name} must resolve to an executable regular file`, {
      cause: error,
    });
  }
  return canonical;
}

async function canonicalRegularFile(path, name) {
  const requested = validateAbsolutePath(path, name);
  try {
    const canonical = validateAbsolutePath(await realpath(requested), `${name} canonical path`);
    const metadata = await stat(canonical);
    if (!metadata.isFile()) throw new ServiceConfigurationError(`${name} must be a regular file`);
    return canonical;
  } catch (error) {
    if (error instanceof ServiceConfigurationError) throw error;
    throw new ServiceConfigurationError(`${name} must resolve to a regular file`, { cause: error });
  }
}

async function assertSelectedNode(nodeBinary) {
  const result = await runOutputProbe(nodeBinary, [
    "--eval",
    "process.stdout.write([process.release.name,process.platform,process.arch].join('/'))",
  ]);
  if (result.timedOut) {
    throw new ServiceConfigurationError("selected Node binary identity probe timed out");
  }
  if (result.code !== 0) {
    throw new ServiceConfigurationError(
      `selected Node binary identity probe failed with exit ${result.code}${probeDetail(result.stderr)}`,
    );
  }
  validateNodeIdentity(result.stdout);
}

async function atomicCopyFile(source, destination, mode) {
  validateAbsolutePath(source, "copy source");
  validateAbsolutePath(destination, "copy destination");
  const temporary = temporarySibling(destination);
  try {
    await copyFile(source, temporary, fsConstants.COPYFILE_EXCL);
    await chmod(temporary, mode);
    const handle = await open(temporary, "r");
    try {
      await handle.sync();
    } finally {
      await handle.close();
    }
    await rename(temporary, destination);
  } finally {
    await rm(temporary, { force: true });
  }
}

async function atomicWriteFile(destination, contents, mode) {
  validateAbsolutePath(destination, "write destination");
  if (typeof contents !== "string") throw new TypeError("atomic file contents must be a string");
  const temporary = temporarySibling(destination);
  let handle;
  try {
    handle = await open(temporary, "wx", mode);
    await handle.writeFile(contents, "utf8");
    await handle.sync();
    await handle.close();
    handle = undefined;
    await chmod(temporary, mode);
    await rename(temporary, destination);
  } finally {
    await handle?.close().catch(() => undefined);
    await rm(temporary, { force: true });
  }
}

function temporarySibling(destination) {
  return join(
    dirname(destination),
    `.${basename(destination)}.${process.pid}.${randomUUID()}.tmp`,
  );
}

async function isLoaded(uid, paths) {
  const result = await runProbe(LAUNCHCTL, launchctlArguments("print", { uid, paths }));
  return classifyLaunchctlPrint(result);
}

async function assertNoColocatedControllerRoles(context) {
  const entries = await Promise.all(CONTROLLER_ROLES.map(async (role) => {
    const serviceDirectory = join(
      context.homeDirectory,
      "Library",
      "Application Support",
      "nanocodex",
      "ci-controllers",
      role.name,
    );
    const plistPath = join(
      context.homeDirectory,
      "Library",
      "LaunchAgents",
      `${role.label}.plist`,
    );
    const [launchResult, plist, state, keychainItems] = await Promise.all([
      runProbe(LAUNCHCTL, ["print", `gui/${context.uid}/${role.label}`]),
      pathMetadata(plistPath),
      pathMetadata(serviceDirectory),
      Promise.all(role.keychainAccounts.map(async (account) => {
        const result = await runProbe(SECURITY, [
          "find-generic-password",
          "-a",
          account,
          "-s",
          `${role.label}.secrets`,
        ]);
        return classifyKeychainFind(result);
      })),
    ]);
    return [role.name, Object.freeze({
      loaded: classifyLaunchctlPrint(launchResult),
      plist: plist != null,
      state: state != null,
      keychainItems: Object.freeze(keychainItems),
    })];
  }));
  return assertNoColocatedControllerState(Object.freeze(Object.fromEntries(entries)));
}

async function keychainItemExists() {
  const result = await runProbe(SECURITY, keychainFindArguments());
  return classifyKeychainFind(result);
}

async function runChecked(executable, arguments_, { operation, stdio = "inherit" }) {
  const result = await runChild(executable, arguments_, { stdio });
  if (result.code !== 0) {
    throw new Error(`${operation} failed with exit ${result.code}`);
  }
}

function runChild(executable, arguments_, { stdio }) {
  return new Promise((resolvePromise, rejectPromise) => {
    const child = spawn(executable, arguments_, {
      stdio,
      shell: false,
      env: sanitizedInstallerEnvironment(),
    });
    child.once("error", rejectPromise);
    child.once("close", (code, signal) => {
      resolvePromise({ code: code ?? 1, signal });
    });
  });
}

function runProbe(executable, arguments_) {
  return new Promise((resolvePromise, rejectPromise) => {
    const child = spawn(executable, arguments_, {
      stdio: ["ignore", "ignore", "pipe"],
      shell: false,
      env: sanitizedInstallerEnvironment(),
    });
    let stderr = "";
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk) => {
      if (stderr.length < 8_192) stderr += chunk.slice(0, 8_192 - stderr.length);
    });
    child.once("error", rejectPromise);
    child.once("close", (code, signal) => {
      resolvePromise({ code: code ?? 1, signal, stderr });
    });
  });
}

function runOutputProbe(executable, arguments_) {
  return new Promise((resolvePromise, rejectPromise) => {
    const child = spawn(executable, arguments_, {
      stdio: ["ignore", "pipe", "pipe"],
      shell: false,
      env: sanitizedInstallerEnvironment(),
    });
    let stdout = "";
    let stderr = "";
    let timedOut = false;
    const append = (current, chunk) => current.length >= 8_192
      ? current
      : current + chunk.slice(0, 8_192 - current.length);
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      stdout = append(stdout, chunk);
    });
    child.stderr.on("data", (chunk) => {
      stderr = append(stderr, chunk);
    });
    const timeout = setTimeout(() => {
      timedOut = true;
      child.kill("SIGKILL");
    }, 5_000);
    child.once("error", (error) => {
      clearTimeout(timeout);
      rejectPromise(error);
    });
    child.once("close", (code, signal) => {
      clearTimeout(timeout);
      resolvePromise({ code: code ?? 1, signal, stdout, stderr, timedOut });
    });
  });
}

function sanitizedInstallerEnvironment() {
  const identity = userInfo();
  return {
    HOME: identity.homedir,
    USER: identity.username,
    LOGNAME: identity.username,
    PATH: SYSTEM_PATH,
    TMPDIR: "/private/tmp",
    LANG: "C",
    LC_ALL: "C",
  };
}

function probeDetail(stderr) {
  const detail = stderr.trim().replaceAll(/\s+/g, " ").slice(0, 500);
  return detail === "" ? "" : `: ${detail}`;
}

async function pathMetadata(path) {
  try {
    return await lstat(path);
  } catch (error) {
    if (error?.code === "ENOENT") return null;
    throw error;
  }
}

async function exists(path) {
  return (await pathMetadata(path)) != null;
}

function usage() {
  return `Usage:
  install-ci-macos-service.mjs install --origin <https-origin> --runner-id <id> --node <absolute-node>
  install-ci-macos-service.mjs update --origin <https-origin> --runner-id <id> --node <absolute-node> [--replace-token]
  install-ci-macos-service.mjs uninstall [--remove-logs]
  install-ci-macos-service.mjs status

Install prompts through macOS security(1) so the CI token never appears in argv,
the LaunchAgent plist, or service logs. Uninstall preserves logs by default.

${serviceRunbook()}
`;
}

const invokedPath = process.argv[1] == null ? null : pathToFileURL(resolve(process.argv[1])).href;
if (invokedPath === import.meta.url) {
  main().catch((error) => {
    process.stderr.write(
      `install-ci-macos-service: ${error instanceof Error ? error.message : String(error)}\n`,
    );
    process.exitCode = 1;
  });
}
