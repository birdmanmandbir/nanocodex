#!/usr/bin/env node

import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { constants as fsConstants } from "node:fs";
import {
  access,
  chmod,
  lstat,
  mkdir,
  open,
  readdir,
  realpath,
  rename,
  rm,
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

const SECURITY = "/usr/bin/security";
const LAUNCHCTL = "/bin/launchctl";
const SUDO = "/usr/bin/sudo";
const ID = "/usr/bin/id";
const DSCL = "/usr/bin/dscl";
const ENV = "/usr/bin/env";
const SHELL = "/bin/sh";
const REALPATH = "/bin/realpath";
const SCRIPT_DIRECTORY = dirname(fileURLToPath(import.meta.url));
const INSTALLER_REPOSITORY = resolve(SCRIPT_DIRECTORY, "../..");
const INTERNAL_CLEAN_ARGUMENT = "--nanocodex-clean-controller-environment";
const EXTERNAL_PROBE_ARGUMENT = "--environment-probe";
const MAX_PROBE_OUTPUT_BYTES = 64 * 1024;
const MAX_REMOVAL_ENTRIES = 20_000;
const MAX_PLIST_BYTES = 64 * 1024;
const MAX_WRAPPER_BYTES = 512 * 1024;
const QUERY_TIMEOUT_MS = 10_000;
const MUTATION_TIMEOUT_MS = 30_000;
const KEYCHAIN_PROMPT_TIMEOUT_MS = 5 * 60 * 1_000;
const STARTUP_PROBE_ATTEMPTS = 20;
const STARTUP_PROBE_INTERVAL_MS = 250;
const STARTUP_STABLE_PROBES = 5;
const PROCESS_GROUP_TERM_GRACE_MS = 1_250;
const PROCESS_GROUP_REAP_TIMEOUT_MS = 5_000;

export const PR_PREP_HELPER_PATH =
  "/Library/PrivilegedHelperTools/dev.nanocodex.ci-pr-cargo-builder";
export const PR_PREP_CARGO_PATH =
  "/Library/PrivilegedHelperTools/dev.nanocodex.ci-cargo";
export const PR_PREP_HELPER_VERSION = "2026-08-22.1";
export const PR_PREP_HELPER_SHA256 =
  "247a453952f53a03aa9189ced4fac97ef98ce6f8c6564d9472ece3127bf41a93";
export const PR_PREP_HELPER_MAX_BYTES = 1024 * 1024;
export const PR_PREP_CARGO_MAX_BYTES = 128 * 1024 * 1024;
export const PR_PREP_NODE_MAX_BYTES = 256 * 1024 * 1024;
export const PR_PREP_CARGO_RELEASE = "1.98.0";
export const PR_PREP_HOME_DIRECTORIES = Object.freeze([
  "/var/empty",
  "/private/var/empty",
]);
const PR_PREP_CANONICAL_HOME_DIRECTORY = "/private/var/empty";
const PR_PREP_HOME_PARENT_DIRECTORIES = Object.freeze([
  "/",
  "/private",
  "/private/var",
]);
export const PR_PREP_NON_LOGIN_SHELLS = Object.freeze([
  "/usr/bin/false",
  "/bin/false",
  "/usr/sbin/nologin",
  "/sbin/nologin",
]);
export const UNAVOIDABLE_MACOS_GROUP_IDS = Object.freeze([12, 61, 79]);
const UNAVOIDABLE_MACOS_GROUPS = Object.freeze([
  Object.freeze({
    name: "everyone",
    gid: 12,
    generatedUid: "ABCDEFAB-CDEF-ABCD-EFAB-CDEF0000000C",
    recordNames: Object.freeze(["everyone", "BUILTIN\\Everyone"]),
  }),
  Object.freeze({
    name: "localaccounts",
    gid: 61,
    generatedUid: "ABCDEFAB-CDEF-ABCD-EFAB-CDEF0000003D",
    recordNames: Object.freeze(["localaccounts"]),
  }),
  Object.freeze({
    name: "_appserverusr",
    gid: 79,
    generatedUid: "ABCDEFAB-CDEF-ABCD-EFAB-CDEF0000004F",
    recordNames: Object.freeze(["_appserverusr", "appserverusr"]),
  }),
]);
const PR_PREP_RESERVED_GROUP_IDS = Object.freeze([0, 20, 80]);
const PR_PREP_RESERVED_ACCOUNT_NAMES = Object.freeze([
  "root",
  "daemon",
  "nobody",
  "_nobody",
  "guest",
  "_guest",
  "shared",
  "_www",
  "_softwareupdate",
  "_installer",
  "_update_sharing",
  "_appstore",
  "_assetcache",
  "_usbmuxd",
]);
const PR_PREP_PROBE_TIMEOUT_MS = 10_000;
const PR_PREP_BUILD_AUTH_TIMEOUT_MS = 10_000;

export const THROTTLE_INTERVAL_SECONDS = 30;
export const LAUNCHD_EXIT_TIMEOUT_SECONDS = 30;
export const LOG_ROTATION_INTERVAL_SECONDS = 30;
export const MAX_CURRENT_LOG_BYTES = 8 * 1024 * 1024;
export const RETAINED_LOG_BYTES = 4 * 1024 * 1024;

export const SERVICE_LABELS = Object.freeze({
  master: "me.nanocodex.ci-controller.master",
  pr: "me.nanocodex.ci-controller.pr",
});
const MACOS_RUNNER_LABEL = "me.nanocodex.ci-macos-runner";
const MACOS_RUNNER_KEYCHAIN_SERVICE = `${MACOS_RUNNER_LABEL}.token`;
const MACOS_RUNNER_KEYCHAIN_ACCOUNT = "runner";

export const ROLE_SECRET_ALLOWLISTS = Object.freeze({
  master: Object.freeze([
    "NANOCODEX_CI_TOKEN",
    "NANOCODEX_GITHUB_STATUS_TOKEN",
    "CLOUDFLARE_API_TOKEN",
    "NANOCODEX_GIT_TOKEN",
  ]),
  pr: Object.freeze([
    "NANOCODEX_CI_TOKEN",
    "NANOCODEX_GITHUB_STATUS_TOKEN",
  ]),
});

export const ROLE_SOURCE_CAPABILITIES = Object.freeze({
  master: "CI_MASTER_SOURCE_WRITE_TOKEN",
  pr: "CI_PR_SOURCE_WRITE_TOKEN",
});

export const ROLE_KEYCHAIN_ACCOUNTS = Object.freeze({
  master: Object.freeze({
    NANOCODEX_CI_TOKEN: ROLE_SOURCE_CAPABILITIES.master,
    NANOCODEX_GITHUB_STATUS_TOKEN: "NANOCODEX_GITHUB_STATUS_TOKEN",
    CLOUDFLARE_API_TOKEN: "CLOUDFLARE_API_TOKEN",
    NANOCODEX_GIT_TOKEN: "NANOCODEX_GIT_TOKEN",
  }),
  pr: Object.freeze({
    NANOCODEX_CI_TOKEN: ROLE_SOURCE_CAPABILITIES.pr,
    NANOCODEX_GITHUB_STATUS_TOKEN: "NANOCODEX_GITHUB_STATUS_TOKEN",
  }),
});

export const PR_FORBIDDEN_SECRET_KEYS = Object.freeze([
  "AWS_ACCESS_KEY_ID",
  "AWS_SECRET_ACCESS_KEY",
  "AWS_SESSION_TOKEN",
  "CARGO_REGISTRY_TOKEN",
  "CARGO_REGISTRIES_CRATES_IO_TOKEN",
  "CF_API_TOKEN",
  "CI_CONTROL_TOKEN",
  "CI_MASTER_SOURCE_WRITE_TOKEN",
  "CI_MACOS_RUNNER_TOKEN",
  "CI_RELEASE_TOKEN",
  "NANOCODEX_SANDBOX_CONTROL_TOKEN",
  "CLOUDFLARE_API_KEY",
  "CLOUDFLARE_API_TOKEN",
  "CLOUDFLARE_EMAIL",
  "GH_ENTERPRISE_TOKEN",
  "GH_TOKEN",
  "GITHUB_ENTERPRISE_TOKEN",
  "GITHUB_TOKEN",
  "GIT_MIRROR_TOKEN",
  "NANOCODEX_CI_CONTROL_TOKEN",
  "NANOCODEX_CI_MACOS_RUNNER_TOKEN",
  "NANOCODEX_CI_MACOS_TOKEN",
  "NANOCODEX_GIT_MIRROR_TOKEN",
  "NANOCODEX_GIT_TOKEN",
  "NANOCODEX_RELEASE_TOKEN",
  "NODE_AUTH_TOKEN",
  "NPM_TOKEN",
  "R2_ACCESS_KEY_ID",
  "R2_SECRET_ACCESS_KEY",
]);

const AMBIENT_AUTHORITY_KEYS = Object.freeze([
  ...new Set([
    ...PR_FORBIDDEN_SECRET_KEYS,
    ...ROLE_SECRET_ALLOWLISTS.master,
    ...ROLE_SECRET_ALLOWLISTS.pr,
    ...Object.values(ROLE_KEYCHAIN_ACCOUNTS.master),
    ...Object.values(ROLE_KEYCHAIN_ACCOUNTS.pr),
    "AZURE_CLIENT_ID",
    "AZURE_CLIENT_SECRET",
    "AZURE_TENANT_ID",
    "DOCKER_AUTH_CONFIG",
    "GITHUB_PAT",
    "GIT_ASKPASS",
    "GIT_SSH_COMMAND",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "NANOCODEX_CI_SOURCE_TOKEN",
    "SSH_AUTH_SOCK",
  ]),
]);

const ROLE_DEFINITIONS = Object.freeze({
  master: Object.freeze({
    label: SERVICE_LABELS.master,
    scriptRelativePath: "web/scripts/ci-controller.mjs",
    keychainService: `${SERVICE_LABELS.master}.secrets`,
  }),
  pr: Object.freeze({
    label: SERVICE_LABELS.pr,
    scriptRelativePath: "web/scripts/ci-pr-controller.mjs",
    keychainService: `${SERVICE_LABELS.pr}.secrets`,
  }),
});

const PATH_KEYS = Object.freeze([
  "homeDirectory",
  "libraryDirectory",
  "applicationSupportDirectory",
  "vendorDirectory",
  "controllersDirectory",
  "serviceDirectory",
  "logsDirectory",
  "temporaryDirectory",
  "statePath",
  "stateLockPath",
  "wrapperPath",
  "stdoutPath",
  "stderrPath",
  "stdoutArchivePath",
  "stderrArchivePath",
  "launchAgentsDirectory",
  "plistPath",
]);

export class ControllerServiceConfigurationError extends Error {
  constructor(message, options) {
    super(message, options);
    this.name = "ControllerServiceConfigurationError";
  }
}

export function validateRole(value) {
  if (value !== "master" && value !== "pr") {
    throw new ControllerServiceConfigurationError("role must be exactly master or pr");
  }
  return value;
}

export function validateOrigin(value, name = "origin") {
  if (typeof value !== "string" || value === "" || value.trim() !== value) {
    throw new ControllerServiceConfigurationError(`${name} must be a non-empty HTTPS origin`);
  }
  let url;
  try {
    url = new URL(value);
  } catch (cause) {
    throw new ControllerServiceConfigurationError(`${name} must be a valid HTTPS origin`, {
      cause,
    });
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
    throw new ControllerServiceConfigurationError(
      `${name} must contain only an HTTPS scheme, host, and optional port`,
    );
  }
  if (value !== url.origin && value !== `${url.origin}/`) {
    throw new ControllerServiceConfigurationError(`${name} must be canonical`);
  }
  return url.origin;
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
    throw new ControllerServiceConfigurationError(`${name} must be a normalized absolute path`);
  }
  return value;
}

export function validateCloudflareAccountId(value) {
  if (typeof value !== "string" || !/^[a-f0-9]{32}$/.test(value)) {
    throw new ControllerServiceConfigurationError(
      "Cloudflare account ID must be exactly 32 lowercase hexadecimal characters",
    );
  }
  return value;
}

function validateSha256(value, name = "SHA-256") {
  if (typeof value !== "string" || !/^[a-f0-9]{64}$/.test(value)) {
    throw new ControllerServiceConfigurationError(
      `${name} must be exactly 64 lowercase hexadecimal characters`,
    );
  }
  return value;
}

export function validateNodeIdentity(value, hostArchitecture) {
  if (hostArchitecture !== "arm64" && hostArchitecture !== "x64") {
    throw new ControllerServiceConfigurationError("unsupported macOS host architecture");
  }
  const expected = `node/darwin/${hostArchitecture}`;
  if (value !== expected) {
    throw new ControllerServiceConfigurationError(
      `selected Node binary must report ${expected}, received ${String(value)}`,
    );
  }
  return value;
}

export function validatePrPrepUsername(value, name = "PR preparation username") {
  if (typeof value !== "string" || !/^[a-z_][a-z0-9_-]{0,30}$/.test(value)) {
    throw new ControllerServiceConfigurationError(
      `${name} must match /^[a-z_][a-z0-9_-]{0,30}$/`,
    );
  }
  return value;
}

export function parseMacOsPasswdRecord(value, expectedUsername) {
  const username = validatePrPrepUsername(expectedUsername);
  if (
    typeof value !== "string" || value.includes("\u0000") || value.includes("\r") ||
    !value.endsWith("\n")
  ) {
    throw new ControllerServiceConfigurationError("PR preparation passwd record is invalid");
  }
  const line = value.slice(0, -1);
  if (line === "" || line.includes("\n")) {
    throw new ControllerServiceConfigurationError("PR preparation passwd record is ambiguous");
  }
  const fields = line.split(":");
  if (fields.length !== 10 || fields[0] !== username || fields[1] === "") {
    throw new ControllerServiceConfigurationError(
      "PR preparation account must have one exact passwd record",
    );
  }
  const uid = parseBoundedIdentifier(fields[2], "PR preparation UID");
  const gid = parseBoundedIdentifier(fields[3], "PR preparation GID");
  const homeDirectory = validateAbsolutePath(fields[8], "PR preparation home directory");
  const shell = validateAbsolutePath(fields[9], "PR preparation shell");
  return Object.freeze({ username, uid, gid, homeDirectory, shell });
}

export function parseNumericGroupList(value, name = "group list") {
  if (
    typeof value !== "string" || value.includes("\u0000") || value.includes("\r") ||
    !/^(?:0|[1-9][0-9]*)(?: (?:0|[1-9][0-9]*))*\n$/.test(value)
  ) {
    throw new ControllerServiceConfigurationError(`${name} is invalid`);
  }
  const groups = value.slice(0, -1).split(" ")
    .map((entry) => parseBoundedIdentifier(entry, name));
  if (new Set(groups).size !== groups.length) {
    throw new ControllerServiceConfigurationError(`${name} contains duplicate group IDs`);
  }
  return Object.freeze(groups);
}

export function parseLocalAccountUidList(value) {
  if (!canonicalCommandListing(value)) {
    throw new ControllerServiceConfigurationError("local account UID list is invalid");
  }
  const entries = [];
  for (const line of value.slice(0, -1).split("\n")) {
    const match = /^(\S+)\s+(-?[0-9]+)$/.exec(line);
    if (!match) {
      throw new ControllerServiceConfigurationError("local account UID list is ambiguous");
    }
    entries.push(Object.freeze({
      username: match[1],
      uid: parseSignedIdentifier(match[2], "local account UID"),
    }));
  }
  if (entries.length === 0) {
    throw new ControllerServiceConfigurationError("local account UID list is empty");
  }
  return Object.freeze(entries);
}

export function parseLocalAccountIdentityList(uniqueIds, primaryGids) {
  const uids = parseDirectoryNameIdList(uniqueIds, "local account UID", "uid");
  const gids = parseDirectoryNameIdList(
    primaryGids,
    "local account primary GID",
    "primaryGid",
  );
  if (
    uids.length !== gids.length ||
    uids.some((entry, index) => entry.name !== gids[index].name)
  ) {
    throw new ControllerServiceConfigurationError(
      "local account UID and primary-GID inventories disagree",
    );
  }
  return Object.freeze(uids.map((entry, index) => Object.freeze({
    name: entry.name,
    uid: entry.uid,
    primaryGid: gids[index].primaryGid,
  })));
}

export function parseLocalGroupIdentityList(value) {
  if (!canonicalCommandListing(value)) {
    throw new ControllerServiceConfigurationError("local group identity list is invalid");
  }
  const entries = [];
  const names = new Set();
  for (const line of value.slice(0, -1).split("\n")) {
    const match = /^(\S+)\s+(-?[0-9]+)$/.exec(line);
    if (!match || names.has(match[1])) {
      throw new ControllerServiceConfigurationError("local group identity list is ambiguous");
    }
    names.add(match[1]);
    entries.push(Object.freeze({
      name: match[1],
      gid: parseSignedIdentifier(match[2], "local group GID"),
    }));
  }
  if (entries.length === 0) {
    throw new ControllerServiceConfigurationError("local group identity list is empty");
  }
  return Object.freeze(entries);
}

function parseDirectoryNameIdList(value, description, field) {
  if (!canonicalCommandListing(value)) {
    throw new ControllerServiceConfigurationError(`${description} identity list is invalid`);
  }
  const entries = [];
  const names = new Set();
  for (const line of value.slice(0, -1).split("\n")) {
    const match = /^(\S+)\s+(-?[0-9]+)$/.exec(line);
    if (!match || names.has(match[1])) {
      throw new ControllerServiceConfigurationError(
        `${description} identity list is ambiguous`,
      );
    }
    names.add(match[1]);
    entries.push(Object.freeze({
      name: match[1],
      [field]: parseSignedIdentifier(match[2], description),
    }));
  }
  if (entries.length === 0) {
    throw new ControllerServiceConfigurationError(`${description} identity list is empty`);
  }
  return Object.freeze(entries);
}

export function parsePrPrepDirectoryUserRecord(value, expectedUsername) {
  const username = validatePrPrepUsername(expectedUsername);
  const attributes = parseDirectoryServiceAttributes(value, "PR preparation user");
  assertExactAttributeNames(attributes, [
    "GeneratedUID",
    "NFSHomeDirectory",
    "PrimaryGroupID",
    "RecordName",
    "UniqueID",
    "UserShell",
  ], ["AuthenticationAuthority"]);
  requireSingleAttribute(attributes, "RecordName", username, "PR preparation RecordName");
  const uid = parseBoundedIdentifier(
    requireSingleAttribute(attributes, "UniqueID", undefined, "PR preparation UniqueID"),
    "PR preparation UniqueID",
  );
  const gid = parseBoundedIdentifier(
    requireSingleAttribute(
      attributes,
      "PrimaryGroupID",
      undefined,
      "PR preparation PrimaryGroupID",
    ),
    "PR preparation PrimaryGroupID",
  );
  const homeDirectory = validateAbsolutePath(
    requireSingleAttribute(
      attributes,
      "NFSHomeDirectory",
      undefined,
      "PR preparation NFSHomeDirectory",
    ),
    "PR preparation NFSHomeDirectory",
  );
  const shell = validateAbsolutePath(
    requireSingleAttribute(attributes, "UserShell", undefined, "PR preparation UserShell"),
    "PR preparation UserShell",
  );
  const generatedUid = requireSingleAttribute(
    attributes,
    "GeneratedUID",
    undefined,
    "PR preparation GeneratedUID",
  );
  if (!isCanonicalGeneratedUid(generatedUid)) {
    throw new ControllerServiceConfigurationError(
      "PR preparation GeneratedUID must be one canonical uppercase UUID",
    );
  }
  if (attributes.has("AuthenticationAuthority")) {
    throw new ControllerServiceConfigurationError(
      "PR preparation account must have no AuthenticationAuthority or login authority",
    );
  }
  return Object.freeze({ username, uid, gid, homeDirectory, shell, generatedUid });
}

function isCanonicalGeneratedUid(value) {
  return typeof value === "string" &&
    /^[A-F0-9]{8}-(?:[A-F0-9]{4}-){3}[A-F0-9]{12}$/.test(value);
}

export function parsePrPrepDirectoryGroupRecord(value, expectedName, expectedGid) {
  const name = validatePrPrepUsername(expectedName, "PR preparation primary group name");
  const gid = validateNonnegativeIdentifier(expectedGid, "PR preparation primary group GID");
  const attributes = parseDirectoryServiceAttributes(value, "PR preparation primary group");
  assertExactAttributeNames(attributes, ["GeneratedUID", "PrimaryGroupID", "RecordName"], [
    "GroupMembers",
    "GroupMembership",
    "NestedGroups",
  ]);
  requireSingleAttribute(attributes, "RecordName", name, "PR preparation group RecordName");
  const observedGid = parseBoundedIdentifier(
    requireSingleAttribute(
      attributes,
      "PrimaryGroupID",
      undefined,
      "PR preparation group PrimaryGroupID",
    ),
    "PR preparation group PrimaryGroupID",
  );
  if (observedGid !== gid) {
    throw new ControllerServiceConfigurationError(
      "PR preparation primary group record disagrees with its authoritative GID",
    );
  }
  const generatedUid = requireSingleAttribute(
    attributes,
    "GeneratedUID",
    undefined,
    "PR preparation group GeneratedUID",
  );
  if (!isCanonicalGeneratedUid(generatedUid)) {
    throw new ControllerServiceConfigurationError(
      "PR preparation group GeneratedUID must be one canonical uppercase UUID",
    );
  }
  const nestedGroupUuids = Object.freeze([...(attributes.get("NestedGroups") ?? [])]);
  if (nestedGroupUuids.length !== 0) {
    throw new ControllerServiceConfigurationError(
      "PR preparation primary group must not inherit another NestedGroups member set",
    );
  }
  return Object.freeze({
    name,
    gid,
    generatedUid,
    members: Object.freeze([...(attributes.get("GroupMembership") ?? [])]),
    memberUuids: Object.freeze([...(attributes.get("GroupMembers") ?? [])]),
    nestedGroupUuids,
  });
}

export function parseUnavoidableMacOsGroupRecord(value, expectedGid) {
  const expected = UNAVOIDABLE_MACOS_GROUPS.find((group) => group.gid === expectedGid);
  if (expected == null) {
    throw new ControllerServiceConfigurationError("unavoidable macOS group expectation is invalid");
  }
  const attributes = parseDirectoryServiceAttributes(
    value,
    `unavoidable macOS group ${expected.gid}`,
  );
  assertExactAttributeNames(attributes, ["GeneratedUID", "PrimaryGroupID", "RecordName"]);
  const names = attributes.get("RecordName");
  if (
    names?.length !== expected.recordNames.length ||
    new Set(names).size !== names.length ||
    names.some((name) => !expected.recordNames.includes(name))
  ) {
    throw new ControllerServiceConfigurationError(
      `unavoidable macOS group ${expected.gid} has an unexpected RecordName identity`,
    );
  }
  requireSingleAttribute(
    attributes,
    "PrimaryGroupID",
    String(expected.gid),
    `unavoidable macOS group ${expected.gid} PrimaryGroupID`,
  );
  requireSingleAttribute(
    attributes,
    "GeneratedUID",
    expected.generatedUid,
    `unavoidable macOS group ${expected.gid} GeneratedUID`,
  );
  return Object.freeze({
    name: expected.name,
    gid: expected.gid,
    generatedUid: expected.generatedUid,
    recordNames: Object.freeze([...expected.recordNames]),
  });
}

export function validatePrPrepHomeSnapshot(snapshot, expectedPath) {
  const path = validateAbsolutePath(expectedPath, "PR preparation home directory");
  if (!PR_PREP_HOME_DIRECTORIES.includes(path)) {
    throw new ControllerServiceConfigurationError(
      `PR preparation home must be ${PR_PREP_HOME_DIRECTORIES.join(" or ")}`,
    );
  }
  if (
    snapshot == null || typeof snapshot !== "object" || Array.isArray(snapshot) ||
    !Array.isArray(snapshot.parents) ||
    snapshot.parents.length !== PR_PREP_HOME_PARENT_DIRECTORIES.length
  ) {
    throw new ControllerServiceConfigurationError(
      "PR preparation empty-home parent chain is incomplete",
    );
  }
  for (let index = 0; index < PR_PREP_HOME_PARENT_DIRECTORIES.length; index += 1) {
    const entry = snapshot.parents[index];
    const expectedParent = PR_PREP_HOME_PARENT_DIRECTORIES[index];
    if (
      entry?.path !== expectedParent || entry.canonicalPath !== expectedParent ||
      entry.kind !== "directory" || entry.symbolicLink !== false || entry.uid !== 0 ||
      !Number.isSafeInteger(entry.gid) || entry.gid < 0 ||
      entry.specialMode !== 0 ||
      !validUnixMode(entry.mode) || (entry.mode & 0o022) !== 0
    ) {
      throw new ControllerServiceConfigurationError(
        `${expectedParent} is not a non-writable root-owned real empty-home parent`,
      );
    }
  }
  const home = snapshot.home;
  if (
    home?.path !== path || home.canonicalPath !== PR_PREP_CANONICAL_HOME_DIRECTORY ||
    home.kind !== "directory" || home.symbolicLink !== false || home.uid !== 0 ||
    !Number.isSafeInteger(home.gid) || home.gid < 0 ||
    home.specialMode !== 0 ||
    !validUnixMode(home.mode) || (home.mode & 0o022) !== 0
  ) {
    throw new ControllerServiceConfigurationError(
      "PR preparation home must resolve to the root-owned non-writable system empty directory",
    );
  }
  return Object.freeze({
    path,
    canonicalPath: home.canonicalPath,
    uid: home.uid,
    gid: home.gid,
    mode: home.mode,
  });
}

function parseDirectoryServiceAttributes(value, description) {
  if (!canonicalCommandListing(value)) {
    throw new ControllerServiceConfigurationError(`${description} record is invalid`);
  }
  const attributes = new Map();
  for (const line of value.slice(0, -1).split("\n")) {
    const match = /^([A-Za-z][A-Za-z0-9]*):(?: (.*))?$/.exec(line);
    if (!match || attributes.has(match[1])) {
      throw new ControllerServiceConfigurationError(`${description} record is ambiguous`);
    }
    const values = match[2] == null || match[2] === "" ? [] : match[2].split(/\s+/);
    if (values.some((entry) => entry === "" || /[\u0000-\u001f\u007f]/.test(entry))) {
      throw new ControllerServiceConfigurationError(`${description} record is ambiguous`);
    }
    attributes.set(match[1], Object.freeze(values));
  }
  return attributes;
}

function canonicalCommandListing(value) {
  return typeof value === "string" && value !== "" && value.endsWith("\n") &&
    !value.includes("\u0000") && !value.includes("\r") &&
    !value.slice(0, -1).split("\n").some((line) => line === "");
}

function assertExactAttributeNames(attributes, required, optional = []) {
  const allowed = new Set([...required, ...optional]);
  if (
    required.some((name) => !attributes.has(name)) ||
    [...attributes.keys()].some((name) => !allowed.has(name))
  ) {
    throw new ControllerServiceConfigurationError(
      "directory-service record has missing, duplicate, or unexpected attributes",
    );
  }
}

function requireSingleAttribute(attributes, name, expected, description) {
  const values = attributes.get(name);
  if (values?.length !== 1 || (expected != null && values[0] !== expected)) {
    throw new ControllerServiceConfigurationError(`${description} must contain one exact value`);
  }
  return values[0];
}

export function validatePrPrepAccount(record, {
  controllerUid,
  controllerUsername,
  controllerGids,
  localAccounts,
  localGroups,
  unavoidableGroups = [],
  directoryUser,
  primaryGroup,
  assignedRoles = [],
} = {}) {
  if (record == null || typeof record !== "object" || Array.isArray(record)) {
    throw new ControllerServiceConfigurationError("PR preparation account record is invalid");
  }
  const username = validatePrPrepUsername(record.username);
  const uid = validateNonnegativeIdentifier(record.uid, "PR preparation UID");
  const gid = validateNonnegativeIdentifier(record.gid, "PR preparation GID");
  if (!Number.isSafeInteger(controllerUid) || controllerUid <= 0) {
    throw new ControllerServiceConfigurationError("controller UID is invalid");
  }
  const controllerAccount = validatePrPrepUsername(
    controllerUsername,
    "controller username",
  );
  if (uid === 0 || uid === controllerUid || gid === 0 || username === controllerAccount) {
    throw new ControllerServiceConfigurationError(
      "PR preparation identity must be non-root and distinct from the controller service role",
    );
  }
  if (
    PR_PREP_RESERVED_GROUP_IDS.includes(gid) ||
    UNAVOIDABLE_MACOS_GROUP_IDS.includes(gid)
  ) {
    throw new ControllerServiceConfigurationError(
      `PR preparation primary GID ${gid} is shared or reserved instead of dedicated`,
    );
  }
  if (PR_PREP_RESERVED_ACCOUNT_NAMES.includes(username.toLowerCase())) {
    throw new ControllerServiceConfigurationError(
      "PR preparation must not reuse a root, guest, shared, or system login account",
    );
  }
  const homeDirectory = validateAbsolutePath(
    record.homeDirectory,
    "PR preparation home directory",
  );
  if (!PR_PREP_HOME_DIRECTORIES.includes(homeDirectory)) {
    throw new ControllerServiceConfigurationError(
      `PR preparation home must be ${PR_PREP_HOME_DIRECTORIES.join(" or ")}`,
    );
  }
  const shell = validateAbsolutePath(record.shell, "PR preparation shell");
  if (!PR_PREP_NON_LOGIN_SHELLS.includes(shell)) {
    throw new ControllerServiceConfigurationError(
      "PR preparation account must use a fixed non-login shell",
    );
  }
  if (!Array.isArray(record.supplementaryGids) || record.supplementaryGids.length === 0) {
    throw new ControllerServiceConfigurationError(
      "PR preparation account must have a numeric supplementary group record",
    );
  }
  const supplementaryGids = record.supplementaryGids.map((value) =>
    validateNonnegativeIdentifier(value, "PR preparation supplementary GID"));
  if (new Set(supplementaryGids).size !== supplementaryGids.length) {
    throw new ControllerServiceConfigurationError(
      "PR preparation supplementary group record contains duplicates",
    );
  }
  if (!supplementaryGids.includes(gid)) {
    throw new ControllerServiceConfigurationError(
      "PR preparation primary GID is absent from its group record",
    );
  }
  const avoidableSupplementary = supplementaryGids.filter((value) =>
    value !== gid && !UNAVOIDABLE_MACOS_GROUP_IDS.includes(value));
  if (avoidableSupplementary.length !== 0) {
    throw new ControllerServiceConfigurationError(
      `PR preparation account has avoidable supplementary groups: ` +
        avoidableSupplementary.join(","),
    );
  }
  const reservedGroup = supplementaryGids.find((value) =>
    PR_PREP_RESERVED_GROUP_IDS.includes(value));
  if (reservedGroup != null) {
    throw new ControllerServiceConfigurationError(
      `PR preparation account belongs to forbidden root/staff/admin group ${reservedGroup}`,
    );
  }
  if (!Array.isArray(controllerGids) || controllerGids.length === 0) {
    throw new ControllerServiceConfigurationError("controller group record is invalid");
  }
  const controllerGroupSet = new Set(controllerGids.map((value) =>
    validateNonnegativeIdentifier(value, "controller GID")));
  const avoidableShared = supplementaryGids.filter((value) =>
    controllerGroupSet.has(value) && !UNAVOIDABLE_MACOS_GROUP_IDS.includes(value));
  if (avoidableShared.length !== 0) {
    throw new ControllerServiceConfigurationError(
      `PR preparation and controller accounts share writable-capable group IDs: ` +
        avoidableShared.join(","),
    );
  }
  if (!Array.isArray(localAccounts) || localAccounts.length === 0) {
    throw new ControllerServiceConfigurationError("local account records are invalid");
  }
  const uidOwners = localAccounts.filter((entry) => entry?.uid === uid);
  const primaryGidOwners = localAccounts.filter((entry) => entry?.primaryGid === gid);
  if (
    uidOwners.length !== 1 ||
    uidOwners[0].name !== username ||
    primaryGidOwners.length !== 1 ||
    primaryGidOwners[0].name !== username ||
    localAccounts.some((entry) => entry?.name === username &&
      (entry?.uid !== uid || entry?.primaryGid !== gid))
  ) {
    throw new ControllerServiceConfigurationError(
      "PR preparation UID and primary GID must belong exclusively to one local passwd account",
    );
  }
  if (!Array.isArray(localGroups) || localGroups.length === 0) {
    throw new ControllerServiceConfigurationError("local group records are invalid");
  }
  const expectedUnavoidableGroups = UNAVOIDABLE_MACOS_GROUPS.filter((group) =>
    supplementaryGids.includes(group.gid));
  if (
    !Array.isArray(unavoidableGroups) ||
    unavoidableGroups.length !== expectedUnavoidableGroups.length ||
    expectedUnavoidableGroups.some((expectedGroup) => {
      const observed = unavoidableGroups.find((group) => group?.gid === expectedGroup.gid);
      return observed?.name !== expectedGroup.name ||
        observed.generatedUid !== expectedGroup.generatedUid ||
        !Array.isArray(observed.recordNames) ||
        observed.recordNames.length !== expectedGroup.recordNames.length ||
        observed.recordNames.some((name) => !expectedGroup.recordNames.includes(name));
    })
  ) {
    throw new ControllerServiceConfigurationError(
      "unavoidable macOS supplementary groups lack authoritative name/GUID records",
    );
  }
  for (const expectedGroup of UNAVOIDABLE_MACOS_GROUPS) {
    if (!supplementaryGids.includes(expectedGroup.gid)) continue;
    const mappings = localGroups.filter((entry) => entry?.gid === expectedGroup.gid);
    if (mappings.length !== 1 || mappings[0].name !== expectedGroup.name) {
      throw new ControllerServiceConfigurationError(
        `unavoidable macOS group ${expectedGroup.gid} has an ambiguous identity`,
      );
    }
  }
  const gidMappings = localGroups.filter((entry) => entry?.gid === gid);
  if (gidMappings.length !== 1 || gidMappings[0].name !== username) {
    throw new ControllerServiceConfigurationError(
      "PR preparation primary GID must map exactly once to its dedicated same-name group",
    );
  }
  if (
    directoryUser?.username !== username || directoryUser.uid !== uid ||
    directoryUser.gid !== gid || directoryUser.homeDirectory !== homeDirectory ||
    directoryUser.shell !== shell ||
    typeof directoryUser.generatedUid !== "string"
  ) {
    throw new ControllerServiceConfigurationError(
      "PR preparation passwd and DirectoryService user records disagree",
    );
  }
  if (
    primaryGroup?.name !== username || primaryGroup.gid !== gid ||
    !isCanonicalGeneratedUid(primaryGroup.generatedUid) ||
    primaryGroup.generatedUid === directoryUser.generatedUid ||
    !Array.isArray(primaryGroup.members) ||
    primaryGroup.members.some((member) => member !== username) ||
    !Array.isArray(primaryGroup.memberUuids) ||
    primaryGroup.memberUuids.some((member) => member !== directoryUser.generatedUid) ||
    !Array.isArray(primaryGroup.nestedGroupUuids) ||
    primaryGroup.nestedGroupUuids.length !== 0
  ) {
    throw new ControllerServiceConfigurationError(
      "PR preparation primary group is ambiguous or has another member",
    );
  }
  if (
    !Array.isArray(assignedRoles) ||
    assignedRoles.some((role) => typeof role !== "string")
  ) {
    throw new ControllerServiceConfigurationError("PR preparation role records are invalid");
  }
  if (assignedRoles.length !== 0) {
    throw new ControllerServiceConfigurationError(
      `PR preparation account must not be a controller or macOS runner account: ` +
        assignedRoles.join(","),
    );
  }
  return Object.freeze({
    username,
    uid,
    gid,
    homeDirectory,
    shell,
    generatedUid: directoryUser.generatedUid,
    primaryGroupName: primaryGroup.name,
    primaryGroupGeneratedUid: primaryGroup.generatedUid,
    supplementaryGids: Object.freeze([...supplementaryGids]),
  });
}

export function validatePrPrepHelperSnapshot(snapshot, trustedSha256) {
  if (!/^[a-f0-9]{64}$/.test(trustedSha256)) {
    throw new ControllerServiceConfigurationError("trusted PR helper SHA-256 is invalid");
  }
  if (snapshot == null || typeof snapshot !== "object" || Array.isArray(snapshot)) {
    throw new ControllerServiceConfigurationError("PR helper snapshot is invalid");
  }
  const expectedParents = ["/", "/Library", "/Library/PrivilegedHelperTools"];
  if (!Array.isArray(snapshot.parents) || snapshot.parents.length !== expectedParents.length) {
    throw new ControllerServiceConfigurationError("PR helper parent chain is incomplete");
  }
  for (let index = 0; index < expectedParents.length; index += 1) {
    const entry = snapshot.parents[index];
    const expectedPath = expectedParents[index];
    if (
      entry?.path !== expectedPath ||
      entry.canonicalPath !== expectedPath ||
      entry.kind !== "directory" ||
      entry.symbolicLink !== false ||
      entry.uid !== 0 ||
      entry.gid !== 0 ||
      entry.specialMode !== 0 ||
      !validUnixMode(entry.mode) ||
      (entry.mode & 0o022) !== 0
    ) {
      throw new ControllerServiceConfigurationError(
        `${expectedPath} is not a non-writable root-owned real helper parent`,
      );
    }
  }
  const helper = snapshot.helper;
  if (
    helper?.path !== PR_PREP_HELPER_PATH ||
    helper.canonicalPath !== PR_PREP_HELPER_PATH ||
    helper.kind !== "file" ||
    helper.symbolicLink !== false ||
    helper.uid !== 0 ||
    helper.gid !== 0 ||
    helper.specialMode !== 0 ||
    helper.mode !== 0o555 ||
    helper.nlink !== 1 ||
    !Number.isSafeInteger(helper.inode) ||
    helper.inode <= 0 ||
    !Number.isSafeInteger(helper.device) ||
    helper.device < 0 ||
    !Number.isSafeInteger(helper.size) ||
    helper.size <= 0 ||
    helper.size > PR_PREP_HELPER_MAX_BYTES ||
    helper.sha256 !== trustedSha256
  ) {
    throw new ControllerServiceConfigurationError(
      "installed PR helper must be the exact bounded root-owned mode-0555 singly linked file",
    );
  }
  return Object.freeze({
    path: helper.path,
    device: helper.device,
    inode: helper.inode,
    size: helper.size,
    sha256: helper.sha256,
    uid: helper.uid,
    gid: helper.gid,
    mode: helper.mode,
    nlink: helper.nlink,
  });
}

export function validateCargoVersionOutput(output, hostArchitecture) {
  if (hostArchitecture !== "arm64" && hostArchitecture !== "x64") {
    throw new ControllerServiceConfigurationError("unsupported Cargo host architecture");
  }
  if (
    typeof output !== "string" || output === "" || output.includes("\u0000") ||
    output.includes("\r") || !output.endsWith("\n") || output.length > 16 * 1024
  ) {
    throw new ControllerServiceConfigurationError("Cargo version output is noncanonical");
  }
  const lines = output.slice(0, -1).split("\n");
  const first = new RegExp(
    `^cargo ${PR_PREP_CARGO_RELEASE.replaceAll(".", "\\.")} ` +
      "\\(([a-f0-9]{7,40}) ([0-9]{4}-[0-9]{2}-[0-9]{2})\\)$",
  ).exec(lines.shift() ?? "");
  if (!first) {
    throw new ControllerServiceConfigurationError(
      `Cargo must report pinned release ${PR_PREP_CARGO_RELEASE}`,
    );
  }
  const fields = new Map();
  for (const line of lines) {
    const match = /^([a-z][a-z0-9-]*): (\S.*)$/.exec(line);
    if (!match || fields.has(match[1])) {
      throw new ControllerServiceConfigurationError("Cargo version identity is ambiguous");
    }
    fields.set(match[1], match[2]);
  }
  const expectedKeys = [
    "commit-date", "commit-hash", "host", "libcurl", "libgit2", "os", "release", "ssl",
  ];
  if (
    [...fields.keys()].sort().join("\0") !== expectedKeys.join("\0") ||
    fields.get("release") !== PR_PREP_CARGO_RELEASE ||
    !/^[a-f0-9]{40}$/.test(fields.get("commit-hash") ?? "") ||
    !fields.get("commit-hash").startsWith(first[1]) ||
    fields.get("commit-date") !== first[2]
  ) {
    throw new ControllerServiceConfigurationError("Cargo version identity is incomplete");
  }
  const expectedHost = hostArchitecture === "arm64"
    ? "aarch64-apple-darwin"
    : "x86_64-apple-darwin";
  if (fields.get("host") !== expectedHost) {
    throw new ControllerServiceConfigurationError(
      `Cargo host must be exactly ${expectedHost}`,
    );
  }
  return Object.freeze({
    release: PR_PREP_CARGO_RELEASE,
    host: expectedHost,
    output,
  });
}

export function validatePrPrepCargoSnapshot(
  snapshot,
  trustedSha256,
  versionOutput,
  hostArchitecture,
) {
  if (!/^[a-f0-9]{64}$/.test(trustedSha256)) {
    throw new ControllerServiceConfigurationError("trusted Cargo SHA-256 is invalid");
  }
  if (snapshot == null || typeof snapshot !== "object" || Array.isArray(snapshot)) {
    throw new ControllerServiceConfigurationError("fixed Cargo snapshot is invalid");
  }
  const expectedParents = ["/", "/Library", "/Library/PrivilegedHelperTools"];
  if (!Array.isArray(snapshot.parents) || snapshot.parents.length !== expectedParents.length) {
    throw new ControllerServiceConfigurationError("fixed Cargo parent chain is incomplete");
  }
  for (let index = 0; index < expectedParents.length; index += 1) {
    const entry = snapshot.parents[index];
    const expectedPath = expectedParents[index];
    if (
      entry?.path !== expectedPath || entry.canonicalPath !== expectedPath ||
      entry.kind !== "directory" || entry.symbolicLink !== false ||
      entry.uid !== 0 || entry.gid !== 0 || !validUnixMode(entry.mode) ||
      entry.specialMode !== 0 ||
      (entry.mode & 0o022) !== 0
    ) {
      throw new ControllerServiceConfigurationError(
        `${expectedPath} is not a non-writable root-owned/root-group real Cargo parent`,
      );
    }
  }
  const cargo = snapshot.cargo;
  if (
    cargo?.path !== PR_PREP_CARGO_PATH || cargo.canonicalPath !== PR_PREP_CARGO_PATH ||
    cargo.kind !== "file" || cargo.symbolicLink !== false ||
    cargo.uid !== 0 || cargo.gid !== 0 || cargo.nlink !== 1 ||
    cargo.specialMode !== 0 ||
    !validUnixMode(cargo.mode) || (cargo.mode & 0o111) === 0 ||
    (cargo.mode & 0o022) !== 0 ||
    !Number.isSafeInteger(cargo.inode) || cargo.inode <= 0 ||
    !Number.isSafeInteger(cargo.device) || cargo.device < 0 ||
    !Number.isSafeInteger(cargo.size) || cargo.size <= 0 ||
    cargo.size > PR_PREP_CARGO_MAX_BYTES || cargo.sha256 !== trustedSha256
  ) {
    throw new ControllerServiceConfigurationError(
      "fixed Cargo must be the exact bounded root-owned/root-group singly linked executable",
    );
  }
  const version = validateCargoVersionOutput(versionOutput, hostArchitecture);
  return Object.freeze({
    path: cargo.path,
    device: cargo.device,
    inode: cargo.inode,
    size: cargo.size,
    sha256: cargo.sha256,
    uid: cargo.uid,
    gid: cargo.gid,
    mode: cargo.mode,
    nlink: cargo.nlink,
    release: version.release,
    host: version.host,
    versionOutput: version.output,
  });
}

export function validatePrPrepNodeSnapshot(snapshot, nodeBinary) {
  const nodePath = validateAbsolutePath(nodeBinary, "PR preparation Node binary");
  if (snapshot == null || typeof snapshot !== "object" || Array.isArray(snapshot)) {
    throw new ControllerServiceConfigurationError("PR preparation Node snapshot is invalid");
  }
  const expectedParents = ancestorPaths(nodePath);
  if (!Array.isArray(snapshot.parents) || snapshot.parents.length !== expectedParents.length) {
    throw new ControllerServiceConfigurationError(
      "PR preparation Node root trust chain is incomplete",
    );
  }
  for (let index = 0; index < expectedParents.length; index += 1) {
    const entry = snapshot.parents[index];
    const expectedPath = expectedParents[index];
    if (
      entry?.path !== expectedPath || entry.canonicalPath !== expectedPath ||
      entry.kind !== "directory" || entry.symbolicLink !== false ||
      entry.uid !== 0 || entry.gid !== 0 || entry.specialMode !== 0 ||
      entry.accessControlList !== false || !validUnixMode(entry.mode) ||
      (entry.mode & 0o022) !== 0 || (entry.mode & 0o001) === 0
    ) {
      throw new ControllerServiceConfigurationError(
        `${expectedPath} is not a root-owned, root-group, no-ACL, non-writable real Node parent`,
      );
    }
  }
  const node = snapshot.node;
  if (
    node?.path !== nodePath || node.canonicalPath !== nodePath ||
    node.kind !== "file" || node.symbolicLink !== false ||
    node.uid !== 0 || node.gid !== 0 || node.specialMode !== 0 ||
    node.accessControlList !== false || !validUnixMode(node.mode) ||
    (node.mode & 0o001) === 0 || (node.mode & 0o022) !== 0 ||
    node.nlink !== 1 || !Number.isSafeInteger(node.device) || node.device < 0 ||
    !Number.isSafeInteger(node.inode) || node.inode <= 0 ||
    !Number.isSafeInteger(node.size) || node.size <= 0 ||
    node.size > PR_PREP_NODE_MAX_BYTES || !/^[a-f0-9]{64}$/.test(node.sha256)
  ) {
    throw new ControllerServiceConfigurationError(
      "PR preparation Node must be one bounded hash-pinned root-owned/root-group " +
        "no-ACL singly linked executable",
    );
  }
  return Object.freeze({
    path: node.path,
    device: node.device,
    inode: node.inode,
    size: node.size,
    sha256: node.sha256,
    uid: node.uid,
    gid: node.gid,
    mode: node.mode,
    nlink: node.nlink,
  });
}

export function validatePrPrepProbe(value, { uid, gid } = {}) {
  const expectedKeys = [
    "credentialEnvironmentNames",
    "freshHomePolicy",
    "gid",
    "helperVersion",
    "uid",
    "version",
  ];
  if (
    value == null ||
    typeof value !== "object" ||
    Array.isArray(value) ||
    Object.keys(value).sort().join("\0") !== expectedKeys.sort().join("\0") ||
    value.version !== 1 ||
    value.helperVersion !== PR_PREP_HELPER_VERSION ||
    value.freshHomePolicy !== "per-build-private-temporary" ||
    !Array.isArray(value.credentialEnvironmentNames) ||
    value.credentialEnvironmentNames.length !== 0 ||
    !Number.isSafeInteger(value.uid) ||
    value.uid <= 0 ||
    !Number.isSafeInteger(value.gid) ||
    value.gid <= 0 ||
    value.uid !== uid ||
    value.gid !== gid
  ) {
    throw new ControllerServiceConfigurationError(
      "PR preparation helper probe failed exact identity and empty-environment validation",
    );
  }
  return value;
}

export function validateRecordedPrPrepIdentity(value) {
  const expectedKeys = [
    "cargo",
    "controllerGids",
    "controllerUid",
    "controllerUsername",
    "generatedUid",
    "gid",
    "helper",
    "helperVersion",
    "homeDirectory",
    "node",
    "nodeBinary",
    "nodeIdentity",
    "primaryGroupGeneratedUid",
    "primaryGroupName",
    "shell",
    "sudoProbe",
    "supplementaryGids",
    "uid",
    "username",
  ];
  if (
    value == null ||
    typeof value !== "object" ||
    Array.isArray(value) ||
    Object.keys(value).sort().join("\0") !== expectedKeys.sort().join("\0")
  ) {
    throw new ControllerServiceConfigurationError(
      "recorded PR preparation identity has the wrong shape",
    );
  }
  const username = validatePrPrepUsername(value.username);
  const controllerUsername = validatePrPrepUsername(
    value.controllerUsername,
    "controller username",
  );
  const uid = validateNonnegativeIdentifier(value.uid, "recorded PR preparation UID");
  const gid = validateNonnegativeIdentifier(value.gid, "recorded PR preparation GID");
  const controllerUid = validatePositiveUid(value.controllerUid);
  if (
    uid === 0 || gid === 0 ||
    uid === controllerUid ||
    username === controllerUsername ||
    PR_PREP_RESERVED_ACCOUNT_NAMES.includes(username.toLowerCase()) ||
    PR_PREP_RESERVED_GROUP_IDS.includes(gid) ||
    UNAVOIDABLE_MACOS_GROUP_IDS.includes(gid)
  ) {
    throw new ControllerServiceConfigurationError(
      "recorded PR preparation UID is root or the controller UID",
    );
  }
  const supplementaryGids = validateRecordedGroupArray(
    value.supplementaryGids,
    "recorded PR preparation groups",
  );
  const controllerGids = validateRecordedGroupArray(
    value.controllerGids,
    "recorded controller groups",
  );
  if (
    !supplementaryGids.includes(gid) ||
    supplementaryGids.some((group) => PR_PREP_RESERVED_GROUP_IDS.includes(group)) ||
    supplementaryGids.some((group) =>
      group !== gid && !UNAVOIDABLE_MACOS_GROUP_IDS.includes(group)) ||
    supplementaryGids.some((group) =>
      controllerGids.includes(group) && !UNAVOIDABLE_MACOS_GROUP_IDS.includes(group))
  ) {
    throw new ControllerServiceConfigurationError(
      "recorded PR preparation groups violate the dedicated-group boundary",
    );
  }
  const homeDirectory = validateAbsolutePath(
    value.homeDirectory,
    "recorded PR preparation home",
  );
  if (!PR_PREP_HOME_DIRECTORIES.includes(homeDirectory)) {
    throw new ControllerServiceConfigurationError("recorded PR preparation home is not empty");
  }
  const shell = validateAbsolutePath(value.shell, "recorded PR preparation shell");
  if (!PR_PREP_NON_LOGIN_SHELLS.includes(shell)) {
    throw new ControllerServiceConfigurationError(
      "recorded PR preparation shell is login-capable",
    );
  }
  if (value.helperVersion !== PR_PREP_HELPER_VERSION) {
    throw new ControllerServiceConfigurationError("recorded PR helper version is invalid");
  }
  if (!isCanonicalGeneratedUid(value.generatedUid)) {
    throw new ControllerServiceConfigurationError("recorded PR GeneratedUID is invalid");
  }
  const primaryGroupName = validatePrPrepUsername(
    value.primaryGroupName,
    "recorded PR primary group name",
  );
  if (primaryGroupName !== username) {
    throw new ControllerServiceConfigurationError(
      "recorded PR primary group is not the dedicated same-name group",
    );
  }
  if (
    !isCanonicalGeneratedUid(value.primaryGroupGeneratedUid) ||
    value.primaryGroupGeneratedUid === value.generatedUid
  ) {
    throw new ControllerServiceConfigurationError(
      "recorded PR primary group GeneratedUID is invalid",
    );
  }
  const helper = value.helper;
  const helperKeys = [
    "device", "gid", "inode", "mode", "nlink", "path", "sha256", "size", "uid",
  ];
  if (
    helper == null ||
    typeof helper !== "object" ||
    Object.keys(helper).sort().join("\0") !== helperKeys.sort().join("\0") ||
    helper.path !== PR_PREP_HELPER_PATH ||
    !Number.isSafeInteger(helper.device) ||
    helper.device < 0 ||
    !Number.isSafeInteger(helper.inode) ||
    helper.inode <= 0 ||
    !Number.isSafeInteger(helper.size) ||
    helper.size <= 0 ||
    helper.size > PR_PREP_HELPER_MAX_BYTES ||
    !/^[a-f0-9]{64}$/.test(helper.sha256) ||
    helper.uid !== 0 || helper.gid !== 0 ||
    helper.mode !== 0o555 ||
    helper.nlink !== 1
  ) {
    throw new ControllerServiceConfigurationError("recorded PR helper metadata is invalid");
  }
  const nodeBinary = validateAbsolutePath(value.nodeBinary, "recorded PR helper Node binary");
  if (!/^node\/darwin\/(?:arm64|x64)$/.test(value.nodeIdentity)) {
    throw new ControllerServiceConfigurationError("recorded PR helper Node identity is invalid");
  }
  const node = value.node;
  const nodeKeys = [
    "device", "gid", "inode", "mode", "nlink", "path", "sha256", "size", "uid",
  ];
  if (
    node == null || typeof node !== "object" || Array.isArray(node) ||
    Object.keys(node).sort().join("\0") !== nodeKeys.sort().join("\0") ||
    node.path !== nodeBinary || node.uid !== 0 || node.gid !== 0 ||
    !Number.isSafeInteger(node.device) || node.device < 0 ||
    !Number.isSafeInteger(node.inode) || node.inode <= 0 ||
    !Number.isSafeInteger(node.size) || node.size <= 0 ||
    node.size > PR_PREP_NODE_MAX_BYTES || !/^[a-f0-9]{64}$/.test(node.sha256) ||
    !validUnixMode(node.mode) || (node.mode & 0o001) === 0 ||
    (node.mode & 0o022) !== 0 || node.nlink !== 1
  ) {
    throw new ControllerServiceConfigurationError(
      "recorded PR helper Node trust-chain metadata is invalid",
    );
  }
  const cargo = value.cargo;
  const cargoKeys = [
    "device", "gid", "host", "inode", "mode", "nlink", "path", "release", "sha256",
    "size", "uid", "versionOutput",
  ];
  const cargoArchitecture = value.nodeIdentity.endsWith("/arm64") ? "arm64" : "x64";
  if (
    cargo == null || typeof cargo !== "object" || Array.isArray(cargo) ||
    Object.keys(cargo).sort().join("\0") !== cargoKeys.sort().join("\0") ||
    cargo.path !== PR_PREP_CARGO_PATH || cargo.uid !== 0 || cargo.gid !== 0 ||
    !Number.isSafeInteger(cargo.device) || cargo.device < 0 ||
    !Number.isSafeInteger(cargo.inode) || cargo.inode <= 0 ||
    !Number.isSafeInteger(cargo.size) || cargo.size <= 0 ||
    cargo.size > PR_PREP_CARGO_MAX_BYTES || !/^[a-f0-9]{64}$/.test(cargo.sha256) ||
    !validUnixMode(cargo.mode) || (cargo.mode & 0o111) === 0 ||
    (cargo.mode & 0o022) !== 0 || cargo.nlink !== 1
  ) {
    throw new ControllerServiceConfigurationError("recorded fixed Cargo metadata is invalid");
  }
  const cargoVersion = validateCargoVersionOutput(cargo.versionOutput, cargoArchitecture);
  if (cargo.release !== cargoVersion.release || cargo.host !== cargoVersion.host) {
    throw new ControllerServiceConfigurationError("recorded fixed Cargo version identity is invalid");
  }
  if (
    value.sudoProbe == null ||
    typeof value.sudoProbe !== "object" ||
    Object.keys(value.sudoProbe).sort().join("\0") !==
      [
        "build", "completeListing", "helperOnly", "noSetenv", "nopasswd", "probe",
        "timestampTimeoutZero",
      ].sort().join("\0") ||
    value.sudoProbe.probe !== true ||
    value.sudoProbe.build !== true ||
    value.sudoProbe.completeListing !== true || value.sudoProbe.helperOnly !== true ||
    value.sudoProbe.noSetenv !== true || value.sudoProbe.nopasswd !== true ||
    value.sudoProbe.timestampTimeoutZero !== true
  ) {
    throw new ControllerServiceConfigurationError("recorded sudo probe is incomplete");
  }
  return Object.freeze({
    username,
    uid,
    gid,
    homeDirectory,
    shell,
    generatedUid: value.generatedUid,
    primaryGroupName,
    primaryGroupGeneratedUid: value.primaryGroupGeneratedUid,
    supplementaryGids,
    controllerUsername,
    controllerUid,
    controllerGids,
    helperVersion: value.helperVersion,
    helper: Object.freeze({ ...helper }),
    cargo: Object.freeze({ ...cargo }),
    node: Object.freeze({ ...node }),
    nodeBinary,
    nodeIdentity: value.nodeIdentity,
    sudoProbe: Object.freeze({
      probe: true,
      build: true,
      completeListing: true,
      helperOnly: true,
      noSetenv: true,
      nopasswd: true,
      timestampTimeoutZero: true,
    }),
  });
}

export function prPrepSudoArguments(mode, prepUsername, nodeBinary) {
  if (mode !== "--probe" && mode !== "--build") {
    throw new ControllerServiceConfigurationError("PR helper mode must be --probe or --build");
  }
  return Object.freeze([
    "-n",
    "-u",
    validatePrPrepUsername(prepUsername),
    "--",
    validateAbsolutePath(nodeBinary, "PR helper Node binary"),
    PR_PREP_HELPER_PATH,
    mode,
  ]);
}

export function validatePrPrepSudoPolicy(output, {
  controllerUsername,
  prepUsername,
  nodeBinary,
} = {}) {
  const controller = validatePrPrepUsername(controllerUsername, "controller username");
  const prep = validatePrPrepUsername(prepUsername);
  const node = validateAbsolutePath(nodeBinary, "PR helper Node binary");
  if (
    typeof output !== "string" || output === "" || output.includes("\u0000") ||
    output.length > MAX_PROBE_OUTPUT_BYTES || output.includes("\r") ||
    /[^\x09\x0a\x20-\x7e]/.test(output) || !output.endsWith("\n")
  ) {
    throw new ControllerServiceConfigurationError(
      "LC_ALL=C sudo -n -l output is absent, noncanonical, or oversized",
    );
  }
  const lines = output.slice(0, -1).split("\n");
  const defaultsPrefix = `Matching Defaults entries for ${controller} on `;
  const grantsPrefix = `User ${controller} may run the following commands on `;
  const defaultsIndexes = lines.flatMap((line, index) =>
    line.startsWith(defaultsPrefix) && line.endsWith(":") ? [index] : []);
  const grantsIndexes = lines.flatMap((line, index) =>
    line.startsWith(grantsPrefix) && line.endsWith(":") ? [index] : []);
  if (
    defaultsIndexes.length !== 1 || grantsIndexes.length !== 1 ||
    defaultsIndexes[0] !== 0 || grantsIndexes[0] <= defaultsIndexes[0] + 1
  ) {
    throw new ControllerServiceConfigurationError(
      "sudo -n -l must expose one complete effective Defaults section and one grant section",
    );
  }
  const defaultsLines = lines.slice(defaultsIndexes[0] + 1, grantsIndexes[0]);
  const commandLines = lines.slice(grantsIndexes[0] + 1);
  if (
    defaultsLines.some((line) => line !== "" && !/^\s+\S/.test(line)) ||
    commandLines.length === 0 || commandLines.some((line) => line !== "" && !/^\s+\S/.test(line))
  ) {
    throw new ControllerServiceConfigurationError("sudo -n -l contains an unparsed policy section");
  }
  const defaults = defaultsLines.filter((line) => line.trim() !== "").join(" ")
    .replaceAll(/\s+/g, " ");
  const timeoutValues = [...defaults.matchAll(
    /(?:^|,\s*|\s+)timestamp_timeout\s*=\s*([^,\s]+)/g,
  )].map((match) => match[1]);
  if (timeoutValues.length !== 1 || timeoutValues[0] !== "0") {
    throw new ControllerServiceConfigurationError(
      "effective sudo policy must contain exactly timestamp_timeout=0",
    );
  }
  const entries = [];
  for (const rawLine of commandLines.filter((line) => line.trim() !== "")) {
    const line = rawLine.trim().replaceAll(/\s+/g, " ");
    if (line.startsWith("(")) entries.push(line);
    else if (entries.length !== 0) entries[entries.length - 1] += ` ${line}`;
    else throw new ControllerServiceConfigurationError("sudo -n -l command wrapping is ambiguous");
  }
  const probeCommand = `${sudoersEscape(node)} ${sudoersEscape(PR_PREP_HELPER_PATH)} --probe`;
  const buildCommand = `${sudoersEscape(node)} ${sudoersEscape(PR_PREP_HELPER_PATH)} --build`;
  const tagged = (command) => new Set([
    `(${prep}) NOPASSWD: NOSETENV: ${command}`,
    `(${prep}) NOSETENV: NOPASSWD: ${command}`,
  ]);
  const combined = tagged(`${probeCommand}, ${buildCommand}`);
  const splitProbe = tagged(probeCommand);
  const splitBuild = tagged(buildCommand);
  const exactCombined = entries.length === 1 && combined.has(entries[0]);
  const exactSplit = entries.length === 2 &&
    entries.some((entry) => splitProbe.has(entry)) &&
    entries.some((entry) => splitBuild.has(entry));
  if (!exactCombined && !exactSplit) {
    throw new ControllerServiceConfigurationError(
      "effective sudo policy contains an extra runas, command, tag, grant, or wrong helper argv",
    );
  }
  return Object.freeze({
    completeListing: true,
    helperOnly: true,
    noSetenv: true,
    nopasswd: true,
    timestampTimeoutZero: true,
  });
}

export function validatePrPrepSudoEvidence(evidence, {
  uid,
  gid,
  controllerUsername,
  prepUsername,
  nodeBinary,
} = {}) {
  if (evidence == null || typeof evidence !== "object" || Array.isArray(evidence)) {
    throw new ControllerServiceConfigurationError("PR sudo probe evidence is invalid");
  }
  const policy = evidence.policy;
  if (
    !validProbeResult(policy) || policy.timedOut || policy.code !== 0 ||
    policy.signal != null || policy.stderr !== ""
  ) {
    throw new ControllerServiceConfigurationError(
      "LC_ALL=C sudo -n -l must succeed quietly before any Keychain access",
    );
  }
  validatePrPrepSudoPolicy(policy.stdout, {
    controllerUsername,
    prepUsername,
    nodeBinary,
  });
  const probe = evidence.probe;
  if (
    !validProbeResult(probe) ||
    probe.timedOut ||
    probe.code !== 0 ||
    probe.signal != null
  ) {
    throw new ControllerServiceConfigurationError(
      "sudoers must allow the exact passwordless PR helper --probe command",
    );
  }
  let parsed;
  try {
    parsed = JSON.parse(probe.stdout);
  } catch (cause) {
    throw new ControllerServiceConfigurationError(
      "PR helper sudo probe did not return canonical JSON",
      { cause },
    );
  }
  validatePrPrepProbe(parsed, { uid, gid });
  if (probe.stdout !== `${JSON.stringify(parsed)}\n` || probe.stderr !== "") {
    throw new ControllerServiceConfigurationError(
      "PR helper sudo probe output is not one clean canonical JSON line",
    );
  }
  const build = evidence.build;
  if (
    !validProbeResult(build) ||
    build.timedOut ||
    build.signal != null ||
    build.code !== 65 ||
    build.stdout !== ""
  ) {
    throw new ControllerServiceConfigurationError(
      "sudoers must allow exact --build with stdin-only input; empty stdin must fail before work",
    );
  }
  return Object.freeze({
    probe: true,
    build: true,
    completeListing: true,
    helperOnly: true,
    noSetenv: true,
    nopasswd: true,
    timestampTimeoutZero: true,
  });
}

export function assertPrPrepIdentityUnchanged(current, installed) {
  const next = validateRecordedPrPrepIdentity(current);
  const previous = validateRecordedPrPrepIdentity(installed);
  if (JSON.stringify(next) !== JSON.stringify(previous)) {
    throw new ControllerServiceConfigurationError(
      "installed PR preparation account, helper, Node, or sudo identity drifted",
    );
  }
  return next;
}

export function renderPrPrepSudoersRule({
  controllerUsername,
  prepUsername,
  nodeBinary,
}) {
  const controller = validatePrPrepUsername(controllerUsername, "controller username");
  const prep = validatePrPrepUsername(prepUsername);
  const node = validateAbsolutePath(nodeBinary, "PR helper Node binary");
  return `${controller} ALL=(${prep}) NOPASSWD:NOSETENV: ` +
    `${sudoersEscape(node)} ${sudoersEscape(PR_PREP_HELPER_PATH)} --probe, ` +
    `${sudoersEscape(node)} ${sudoersEscape(PR_PREP_HELPER_PATH)} --build`;
}

export function renderPrPrepProvisioning({
  controllerUsername,
  prepUsername,
  nodeBinary,
  helperPayload,
  cargoSha256,
}) {
  const controller = validatePrPrepUsername(controllerUsername, "controller username");
  const prep = validatePrPrepUsername(prepUsername);
  const node = validateAbsolutePath(nodeBinary, "PR helper Node binary");
  if (
    helperPayload == null || typeof helperPayload !== "object" ||
    !Buffer.isBuffer(helperPayload.bytes) || helperPayload.bytes.length <= 0 ||
    helperPayload.bytes.length > PR_PREP_HELPER_MAX_BYTES ||
    helperPayload.size !== helperPayload.bytes.length ||
    helperPayload.sha256 !== PR_PREP_HELPER_SHA256 ||
    createHash("sha256").update(helperPayload.bytes).digest("hex") !== helperPayload.sha256
  ) {
    throw new ControllerServiceConfigurationError(
      "root provisioning requires the exact pinned single-fd helper payload",
    );
  }
  const cargoHash = validateSha256(cargoSha256, "pinned Cargo SHA-256");
  const payload = helperPayload.bytes.toString("base64").match(/.{1,76}/g).join("\n");
  const sudoersPath = `/private/etc/sudoers.d/dev.nanocodex.ci-pr-cargo-builder-${controller}`;
  const rule = renderPrPrepSudoersRule({
    controllerUsername: controller,
    prepUsername: prep,
    nodeBinary: node,
  });
  return `# Review and run these privileged steps as root; the installer never runs them.
# Four identities stay distinct: master controller, PR controller (${controller}),
# PR Cargo preparation (${prep}), and the macOS runner.
# Provision ${prep} as one unique local passwd account and one dedicated primary
# group whose GID has no other primary owner, member, alias, or nested group.
# Keep /var/empty root-owned and non-writable; use /usr/bin/false; grant no login,
# authentication, admin/staff, Keychain, role-service, or avoidable group authority.
# Never reuse a controller, runner, root, admin, guest, or shared account.
# Never chown/chmod the PR checkout for ${prep}; never grant a shared writable group.
# Separately provision Cargo ${PR_PREP_CARGO_RELEASE} at ${PR_PREP_CARGO_PATH} with
# exact SHA-256 ${cargoHash}. It and every parent must be real, root:wheel,
# executable where applicable, singly linked, and non-group/world-writable.
# Separately provision ${node} as one singly linked root:wheel global Node file.
# It and every ancestor must be real, no-ACL, and non-group/world-writable; a
# controller-owned executable or a symlinked Homebrew convenience path is rejected.
# This script contains the already-opened, bounded, controller-owned helper bytes.
# It never names or opens the controller checkout while running as root.
set -eu
umask 077
NANOCODEX_ROOT_STAGE=
NANOCODEX_HELPER_TMP=
NANOCODEX_SUDOERS_TMP=
cleanup_nanocodex_provisioning() {
  if [ -n "$NANOCODEX_HELPER_TMP" ]; then /bin/rm -f "$NANOCODEX_HELPER_TMP"; fi
  if [ -n "$NANOCODEX_SUDOERS_TMP" ]; then /bin/rm -f "$NANOCODEX_SUDOERS_TMP"; fi
  if [ -n "$NANOCODEX_ROOT_STAGE" ]; then /bin/rm -rf "$NANOCODEX_ROOT_STAGE"; fi
}
trap cleanup_nanocodex_provisioning 0 1 2 15
NANOCODEX_ROOT_STAGE="$(/usr/bin/mktemp -d '/private/var/root/.dev.nanocodex.ci-pr-provision.XXXXXX')"
NANOCODEX_SUDOERS_TMP="$(/usr/bin/mktemp '/private/etc/sudoers.d/.dev.nanocodex.ci-pr-cargo-builder.XXXXXX')"
assert_nanocodex_root_directory() {
  checked_path=$1
  [ ! -L "$checked_path" ] && [ -d "$checked_path" ]
  [ "$(/bin/realpath "$checked_path")" = "$checked_path" ]
  [ "$(/usr/bin/stat -f '%u:%g' "$checked_path")" = '0:0' ]
  [ "$(/usr/bin/stat -f '%Mp' "$checked_path")" = 0 ]
  checked_mode=$(/usr/bin/stat -f '%Lp' "$checked_path")
  [ $((0$checked_mode & 022)) -eq 0 ]
}
assert_nanocodex_root_directory /
assert_nanocodex_root_directory /Library
assert_nanocodex_root_directory /Library/PrivilegedHelperTools
/usr/sbin/chown root:wheel "$NANOCODEX_ROOT_STAGE"
/bin/chmod 0700 "$NANOCODEX_ROOT_STAGE"
/usr/bin/base64 -D > "$NANOCODEX_ROOT_STAGE/helper" <<'NANOCODEX_HELPER_PAYLOAD'
${payload}
NANOCODEX_HELPER_PAYLOAD
/usr/sbin/chown root:wheel "$NANOCODEX_ROOT_STAGE/helper"
/bin/chmod 0400 "$NANOCODEX_ROOT_STAGE/helper"
exec 3< "$NANOCODEX_ROOT_STAGE/helper"
[ "$(/usr/bin/stat -f '%u:%g:%l:%Mp:%Lp:%z' /dev/fd/3)" = '0:0:1:0:400:${helperPayload.size}' ]
NANOCODEX_HELPER_SHA=$(/usr/bin/shasum -a 256 <&3)
[ "${"${NANOCODEX_HELPER_SHA%% *}"}" = '${helperPayload.sha256}' ]
exec 3<&-

[ ! -L ${shellQuote(PR_PREP_CARGO_PATH)} ] && [ -f ${shellQuote(PR_PREP_CARGO_PATH)} ]
[ "$(/bin/realpath ${shellQuote(PR_PREP_CARGO_PATH)})" = ${shellQuote(PR_PREP_CARGO_PATH)} ]
exec 4< ${shellQuote(PR_PREP_CARGO_PATH)}
NANOCODEX_CARGO_METADATA=$(/usr/bin/stat -f '%u:%g:%l:%Mp:%Lp:%z' /dev/fd/4)
NANOCODEX_OLD_IFS=$IFS
IFS=:
set -- $NANOCODEX_CARGO_METADATA
IFS=$NANOCODEX_OLD_IFS
[ "$1:$2:$3" = '0:0:1' ]
[ "$4" = 0 ]
NANOCODEX_CARGO_MODE=$5
NANOCODEX_CARGO_SIZE=$6
[ $((0$NANOCODEX_CARGO_MODE & 022)) -eq 0 ]
[ $((0$NANOCODEX_CARGO_MODE & 0111)) -ne 0 ]
[ "$NANOCODEX_CARGO_SIZE" -gt 0 ] && [ "$NANOCODEX_CARGO_SIZE" -le ${PR_PREP_CARGO_MAX_BYTES} ]
NANOCODEX_CARGO_SHA=$(/usr/bin/shasum -a 256 <&4)
[ "${"${NANOCODEX_CARGO_SHA%% *}"}" = '${cargoHash}' ]
exec 4<&-
if [ -e ${shellQuote(PR_PREP_HELPER_PATH)} ] || [ -L ${shellQuote(PR_PREP_HELPER_PATH)} ]; then
  [ ! -L ${shellQuote(PR_PREP_HELPER_PATH)} ] && [ -f ${shellQuote(PR_PREP_HELPER_PATH)} ]
  [ "$(/usr/bin/stat -f '%u:%g:%l' ${shellQuote(PR_PREP_HELPER_PATH)})" = '0:0:1' ]
  [ "$(/usr/bin/stat -f '%Mp' ${shellQuote(PR_PREP_HELPER_PATH)})" = 0 ]
  [ "$(/usr/bin/stat -f '%Lp' ${shellQuote(PR_PREP_HELPER_PATH)})" = 555 ]
fi
NANOCODEX_HELPER_TMP="$(/usr/bin/mktemp '/Library/PrivilegedHelperTools/.dev.nanocodex.ci-pr-cargo-builder.XXXXXX')"
/usr/bin/install -o root -g wheel -m 0555 "$NANOCODEX_ROOT_STAGE/helper" "$NANOCODEX_HELPER_TMP"
exec 5< "$NANOCODEX_HELPER_TMP"
[ "$(/usr/bin/stat -f '%u:%g:%l:%Mp:%Lp:%z' /dev/fd/5)" = '0:0:1:0:555:${helperPayload.size}' ]
NANOCODEX_INSTALLED_SHA=$(/usr/bin/shasum -a 256 <&5)
[ "${"${NANOCODEX_INSTALLED_SHA%% *}"}" = '${helperPayload.sha256}' ]
exec 5<&-
/bin/mv -f "$NANOCODEX_HELPER_TMP" ${shellQuote(PR_PREP_HELPER_PATH)}
NANOCODEX_HELPER_TMP=
exec 6< ${shellQuote(PR_PREP_HELPER_PATH)}
[ "$(/usr/bin/stat -f '%u:%g:%l:%Mp:%Lp:%z' /dev/fd/6)" = '0:0:1:0:555:${helperPayload.size}' ]
[ "$(/bin/realpath ${shellQuote(PR_PREP_HELPER_PATH)})" = ${shellQuote(PR_PREP_HELPER_PATH)} ]
NANOCODEX_FINAL_SHA=$(/usr/bin/shasum -a 256 <&6)
[ "${"${NANOCODEX_FINAL_SHA%% *}"}" = '${helperPayload.sha256}' ]
exec 6<&-
/bin/cat > "$NANOCODEX_SUDOERS_TMP" <<'NANOCODEX_SUDOERS'
Defaults:${controller} env_reset
Defaults:${controller} !setenv
Defaults:${controller} timestamp_timeout=0
${rule}
NANOCODEX_SUDOERS
/usr/sbin/chown root:wheel "$NANOCODEX_SUDOERS_TMP"
/bin/chmod 0440 "$NANOCODEX_SUDOERS_TMP"
/usr/sbin/visudo -cf "$NANOCODEX_SUDOERS_TMP"
if [ -e ${shellQuote(sudoersPath)} ] || [ -L ${shellQuote(sudoersPath)} ]; then
  [ ! -L ${shellQuote(sudoersPath)} ] && [ -f ${shellQuote(sudoersPath)} ]
  [ "$(/usr/bin/stat -f '%u:%g:%l' ${shellQuote(sudoersPath)})" = '0:0:1' ]
  [ "$(/usr/bin/stat -f '%Mp' ${shellQuote(sudoersPath)})" = 0 ]
  [ "$(/usr/bin/stat -f '%Lp' ${shellQuote(sudoersPath)})" = 440 ]
fi
/bin/mv -f "$NANOCODEX_SUDOERS_TMP" ${shellQuote(sudoersPath)}
NANOCODEX_SUDOERS_TMP=
[ "$(/usr/bin/stat -f '%u:%g:%l:%Mp:%Lp' ${shellQuote(sudoersPath)})" = '0:0:1:0:440' ]
/usr/sbin/visudo -cf ${shellQuote(sudoersPath)}
/bin/rm -rf "$NANOCODEX_ROOT_STAGE"
NANOCODEX_ROOT_STAGE=
trap - 0 1 2 15
# Uninstall deliberately leaves the account, helper, and sudoers file for explicit root cleanup.`;
}

export function prPrepOperatorCleanup() {
  return Object.freeze({
    prepAccountPreserved: true,
    helperPreserved: PR_PREP_HELPER_PATH,
    sudoersPreserved: true,
    instruction: "root must separately remove the dedicated account, helper, and sudoers rule",
  });
}

export function validateHost({
  platform: hostPlatform,
  arch: hostArchitecture,
  uid,
  euid = uid,
  identityUid = uid,
  homeDirectory,
  username,
}) {
  if (
    hostPlatform !== "darwin" ||
    (hostArchitecture !== "arm64" && hostArchitecture !== "x64")
  ) {
    throw new ControllerServiceConfigurationError(
      `controller LaunchAgents require darwin/arm64 or darwin/x64, received ` +
        `${hostPlatform}/${hostArchitecture}`,
    );
  }
  if (!Number.isSafeInteger(uid) || uid <= 0 || uid > 2_147_483_647) {
    throw new ControllerServiceConfigurationError("a non-root numeric user ID is required");
  }
  if (euid !== uid || identityUid !== uid) {
    throw new ControllerServiceConfigurationError(
      "run the installer directly as the target passwd user, not through sudo",
    );
  }
  if (
    typeof username !== "string" ||
    username === "" ||
    username.length > 255 ||
    /[\u0000-\u001f\u007f]/.test(username)
  ) {
    throw new ControllerServiceConfigurationError("passwd username is invalid");
  }
  const home = validateAbsolutePath(homeDirectory, "passwd home directory");
  if (home === "/" || dirname(home) === home) {
    throw new ControllerServiceConfigurationError("passwd home directory must not be a root");
  }
  return Object.freeze({
    uid,
    username,
    architecture: hostArchitecture,
    guiDomain: `gui/${uid}`,
    homeDirectory: home,
  });
}

export function deriveServicePaths(homeDirectory, role) {
  const home = validateAbsolutePath(homeDirectory, "passwd home directory");
  validateRole(role);
  if (home === "/" || dirname(home) === home) {
    throw new ControllerServiceConfigurationError("passwd home directory must not be a root");
  }
  const libraryDirectory = join(home, "Library");
  const applicationSupportDirectory = join(libraryDirectory, "Application Support");
  const vendorDirectory = join(applicationSupportDirectory, "nanocodex");
  const controllersDirectory = join(vendorDirectory, "ci-controllers");
  const serviceDirectory = join(controllersDirectory, role);
  const logsDirectory = join(serviceDirectory, "logs");
  const temporaryDirectory = join(serviceDirectory, "tmp");
  const statePath = join(serviceDirectory, "pr-state.json");
  const launchAgentsDirectory = join(libraryDirectory, "LaunchAgents");
  return Object.freeze({
    homeDirectory: home,
    libraryDirectory,
    applicationSupportDirectory,
    vendorDirectory,
    controllersDirectory,
    serviceDirectory,
    logsDirectory,
    temporaryDirectory,
    statePath,
    stateLockPath: `${statePath}.lock`,
    wrapperPath: join(serviceDirectory, "run-controller.sh"),
    stdoutPath: join(logsDirectory, "stdout.log"),
    stderrPath: join(logsDirectory, "stderr.log"),
    stdoutArchivePath: join(logsDirectory, "stdout.log.1"),
    stderrArchivePath: join(logsDirectory, "stderr.log.1"),
    launchAgentsDirectory,
    plistPath: join(launchAgentsDirectory, `${ROLE_DEFINITIONS[role].label}.plist`),
  });
}

export function validateServicePaths(paths, role) {
  validateRole(role);
  if (paths == null || typeof paths !== "object" || Array.isArray(paths)) {
    throw new ControllerServiceConfigurationError("service paths must be an object");
  }
  if (Object.keys(paths).sort().join("\0") !== [...PATH_KEYS].sort().join("\0")) {
    throw new ControllerServiceConfigurationError(
      "service paths must contain only the fixed role-scoped layout",
    );
  }
  const expected = deriveServicePaths(paths.homeDirectory, role);
  for (const key of PATH_KEYS) {
    validateAbsolutePath(paths[key], key);
    if (paths[key] !== expected[key]) {
      throw new ControllerServiceConfigurationError(
        `${key} is outside the fixed ${role} controller layout`,
      );
    }
  }
  for (const [path, parent, name] of [
    [paths.serviceDirectory, paths.controllersDirectory, "service directory"],
    [paths.logsDirectory, paths.serviceDirectory, "logs directory"],
    [paths.temporaryDirectory, paths.serviceDirectory, "temporary directory"],
    [paths.statePath, paths.serviceDirectory, "PR state path"],
    [paths.wrapperPath, paths.serviceDirectory, "wrapper path"],
    [paths.stdoutPath, paths.logsDirectory, "stdout path"],
    [paths.stderrPath, paths.logsDirectory, "stderr path"],
    [paths.stdoutArchivePath, paths.logsDirectory, "stdout archive path"],
    [paths.stderrArchivePath, paths.logsDirectory, "stderr archive path"],
    [paths.plistPath, paths.launchAgentsDirectory, "LaunchAgent plist path"],
  ]) {
    assertDescendant(path, parent, name);
  }
  return paths;
}

export function controllerScriptPath(repository, role) {
  const root = validateAbsolutePath(repository, "repository");
  validateRole(role);
  const path = join(root, ROLE_DEFINITIONS[role].scriptRelativePath);
  assertDescendant(path, root, "controller script");
  return path;
}

export function renderLaunchAgentPlist(paths, role, repository) {
  validateServicePaths(paths, role);
  const root = validateAbsolutePath(repository, "repository");
  const value = (text) => xmlEscape(text);
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${value(ROLE_DEFINITIONS[role].label)}</string>
  <key>Program</key>
  <string>${value(ENV)}</string>
  <key>ProgramArguments</key>
  <array>
    <string>${value(ENV)}</string>
    <string>-i</string>
    <string>${value(paths.wrapperPath)}</string>
  </array>
  <key>WorkingDirectory</key>
  <string>${value(root)}</string>
  <key>LimitLoadToSessionType</key>
  <string>Aqua</string>
  <key>ProcessType</key>
  <string>Background</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>${THROTTLE_INTERVAL_SECONDS}</integer>
  <key>ExitTimeOut</key>
  <integer>${LAUNCHD_EXIT_TIMEOUT_SECONDS}</integer>
  <key>Umask</key>
  <integer>63</integer>
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

export function renderControllerWrapper(configuration) {
  const normalized = normalizeWrapperConfiguration(configuration);
  const {
    role,
    paths,
    nodeBinary,
    repository,
    controllerScript,
    origin,
    webOrigin,
    rustSecRepository,
    cloudflareAccountId,
    uid,
    username,
    architecture,
    metadata,
    prPrep,
  } = normalized;
  const executablePath = [...new Set([
    dirname(nodeBinary),
    join(paths.homeDirectory, ".cargo", "bin"),
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
  ])].join(":");
  const fixedEnvironment = {
    HOME: paths.homeDirectory,
    USER: username,
    LOGNAME: username,
    PATH: executablePath,
    TMPDIR: paths.temporaryDirectory,
    LANG: "C",
    NANOCODEX_CI_ORIGIN: origin,
    NANOCODEX_REPO: repository,
    ...(role === "master"
      ? {
          NANOCODEX_WEB_ORIGIN: webOrigin,
          NANOCODEX_RUSTSEC_REPO: rustSecRepository,
          CLOUDFLARE_ACCOUNT_ID: cloudflareAccountId,
        }
      : {
          SHLVL: "0",
          __CF_USER_TEXT_ENCODING: `0x${uid.toString(16).toUpperCase()}:0x0:0x0`,
          NANOCODEX_CI_PR_STATE_PATH: paths.statePath,
          NANOCODEX_CI_PR_TMPDIR: paths.temporaryDirectory,
          NANOCODEX_CI_PR_PREP_USER: prPrep.username,
        }),
  };
  const preflightProgram = renderPreflightProbeProgram({
    role,
    architecture,
    nodeBinary,
    repository,
    controllerScript,
    fixedEnvironment,
    prPrep,
  });
  const liveProgram = renderLiveEnvironmentProbeProgram({
    role,
    architecture,
    fixedEnvironment,
  });
  const fixedExports = Object.entries(fixedEnvironment)
    .map(([name, value]) => `export ${name}=${shellQuote(value)}`)
    .join("\n");
  const secretAssignments = ROLE_SECRET_ALLOWLISTS[role]
    .map((name) => {
      const account = keychainAccountName(role, name);
      return `${name}="$(${SECURITY} find-generic-password -a ${shellQuote(account)} -s "$KEYCHAIN_SERVICE" -w)" || secret_failure ${shellQuote(name)}\nif [ -z "$${name}" ]; then secret_failure ${shellQuote(name)}; fi`;
    })
    .join("\n");
  const secretExports = `export ${ROLE_SECRET_ALLOWLISTS[role].join(" ")}`;
  const prNodeParentArguments = role === "pr"
    ? ancestorPaths(nodeBinary).map(shellQuote).join(" ")
    : "";

  return `#!/bin/sh
set -eu
umask 077
ulimit -c 0 2>/dev/null || true

# The outer stage accepts only launchd's empty argv or the installer's
# non-secret probe request. It then rebuilds the environment from nothing.
if [ "\${1-}" != ${shellQuote(INTERNAL_CLEAN_ARGUMENT)} ]; then
  if [ "$#" -eq 0 ]; then
    exec ${ENV} -i ${SHELL} "$0" ${shellQuote(INTERNAL_CLEAN_ARGUMENT)} run
  fi
  if [ "$#" -eq 1 ] && [ "$1" = ${shellQuote(EXTERNAL_PROBE_ARGUMENT)} ]; then
    exec ${ENV} -i ${SHELL} "$0" ${shellQuote(INTERNAL_CLEAN_ARGUMENT)} probe
  fi
  printf '%s\\n' 'nanocodex controller wrapper rejects arguments' >&2
  exit 64
fi
if [ "$#" -ne 2 ] || { [ "$2" != run ] && [ "$2" != probe ]; }; then
  printf '%s\\n' 'nanocodex controller wrapper rejected its internal stage' >&2
  exit 64
fi
readonly WRAPPER_MODE=$2

readonly EXPECTED_UID=${uid}
readonly NODE_BINARY=${shellQuote(nodeBinary)}
readonly NODE_OWNER_UID=${metadata.node.uid}
readonly NODE_MODE=${metadata.node.mode.toString(8)}
readonly REPOSITORY=${shellQuote(repository)}
readonly REPOSITORY_MODE=${metadata.repository.mode.toString(8)}
readonly CONTROLLER_SCRIPT=${shellQuote(controllerScript)}
readonly CONTROLLER_MODE=${metadata.controller.mode.toString(8)}
readonly KEYCHAIN_SERVICE=${shellQuote(ROLE_DEFINITIONS[role].keychainService)}
readonly STDOUT_LOG=${shellQuote(paths.stdoutPath)}
readonly STDERR_LOG=${shellQuote(paths.stderrPath)}
readonly STDOUT_ARCHIVE=${shellQuote(paths.stdoutArchivePath)}
readonly STDERR_ARCHIVE=${shellQuote(paths.stderrArchivePath)}
readonly MAX_CURRENT_LOG_BYTES=${MAX_CURRENT_LOG_BYTES}
readonly RETAINED_LOG_BYTES=${RETAINED_LOG_BYTES}
readonly LOG_ROTATION_INTERVAL_SECONDS=${LOG_ROTATION_INTERVAL_SECONDS}
${role === "pr" ? `readonly PR_PREP_USER=${shellQuote(prPrep.username)}
readonly PR_PREP_HELPER=${shellQuote(PR_PREP_HELPER_PATH)}
` : ""}

${fixedExports}
${role === "pr" ? "unset PWD OLDPWD _" : ""}

runtime_metadata() {
  ${REALPATH} "$1" 2>/dev/null || return 1
}

assert_runtime_directory() {
  checked_path=$1
  checked_uid=$2
  checked_mode=$3
  [ ! -L "$checked_path" ] && [ -d "$checked_path" ] || return 1
  [ "$(runtime_metadata "$checked_path")" = "$checked_path" ] || return 1
  [ "$(/usr/bin/stat -f '%u' "$checked_path" 2>/dev/null)" = "$checked_uid" ] || return 1
  [ "$(/usr/bin/stat -f '%Mp' "$checked_path" 2>/dev/null)" = 0 ] || return 1
  [ "$(/usr/bin/stat -f '%Lp' "$checked_path" 2>/dev/null)" = "$checked_mode" ] || return 1
}

assert_runtime_file() {
  checked_path=$1
  checked_uid=$2
  checked_mode=$3
  executable=$4
  [ ! -L "$checked_path" ] && [ -f "$checked_path" ] || return 1
  [ "$(runtime_metadata "$checked_path")" = "$checked_path" ] || return 1
  [ "$(/usr/bin/stat -f '%u' "$checked_path" 2>/dev/null)" = "$checked_uid" ] || return 1
  [ "$(/usr/bin/stat -f '%l' "$checked_path" 2>/dev/null)" = 1 ] || return 1
  [ "$(/usr/bin/stat -f '%Mp' "$checked_path" 2>/dev/null)" = 0 ] || return 1
  [ "$(/usr/bin/stat -f '%Lp' "$checked_path" 2>/dev/null)" = "$checked_mode" ] || return 1
  if [ "$executable" = yes ]; then [ -x "$checked_path" ] || return 1; fi
}

assert_no_acl() {
  acl_listing=$(/bin/ls -lde "$1" 2>/dev/null) || return 1
  [ "$(printf '%s' "$acl_listing" | /usr/bin/wc -l)" -eq 0 ] || return 1
  acl_mode=\${acl_listing%% *}
  case "$acl_mode" in
    [-bcdlps]?????????|[-bcdlps]?????????@) ;;
    *) return 1 ;;
  esac
  case "$acl_mode" in *+*) return 1 ;; esac
}

assert_runtime_root_node_parent() {
  checked_path=$1
  [ ! -L "$checked_path" ] && [ -d "$checked_path" ] || return 1
  [ "$(runtime_metadata "$checked_path")" = "$checked_path" ] || return 1
  [ "$(/usr/bin/stat -f '%u:%g' "$checked_path" 2>/dev/null)" = '0:0' ] || return 1
  [ "$(/usr/bin/stat -f '%Mp' "$checked_path" 2>/dev/null)" = 0 ] || return 1
  checked_mode=$(/usr/bin/stat -f '%Lp' "$checked_path" 2>/dev/null) || return 1
  [ "$((0$checked_mode & 022))" -eq 0 ] || return 1
  [ "$((0$checked_mode & 001))" -ne 0 ] || return 1
  assert_no_acl "$checked_path"
}

assert_runtime_root_node_file() {
  checked_path=$1
  checked_mode=$2
  [ ! -L "$checked_path" ] && [ -f "$checked_path" ] || return 1
  [ "$(runtime_metadata "$checked_path")" = "$checked_path" ] || return 1
  [ "$(/usr/bin/stat -f '%u:%g' "$checked_path" 2>/dev/null)" = '0:0' ] || return 1
  [ "$(/usr/bin/stat -f '%l' "$checked_path" 2>/dev/null)" = 1 ] || return 1
  [ "$(/usr/bin/stat -f '%Mp' "$checked_path" 2>/dev/null)" = 0 ] || return 1
  [ "$(/usr/bin/stat -f '%Lp' "$checked_path" 2>/dev/null)" = "$checked_mode" ] || return 1
  [ -x "$checked_path" ] || return 1
  assert_no_acl "$checked_path"
}

${role === "pr" ? `for checked_node_parent in ${prNodeParentArguments}; do
  if ! assert_runtime_root_node_parent "$checked_node_parent"; then
    printf '%s\\n' 'nanocodex PR preparation Node ancestor failed its root trust check' >&2
    exit 78
  fi
done
if ! assert_runtime_root_node_file "$NODE_BINARY" "$NODE_MODE"; then
` : `if ! assert_runtime_file "$NODE_BINARY" "$NODE_OWNER_UID" "$NODE_MODE" yes; then
`}  printf '%s\\n' 'nanocodex controller Node path failed its identity check' >&2
  exit 78
fi
if ! assert_runtime_directory "$REPOSITORY" "$EXPECTED_UID" "$REPOSITORY_MODE"; then
  printf '%s\\n' 'nanocodex controller repository path failed its identity check' >&2
  exit 78
fi
if ! assert_runtime_file "$CONTROLLER_SCRIPT" "$EXPECTED_UID" "$CONTROLLER_MODE" no; then
  printf '%s\\n' 'nanocodex controller script path failed its identity check' >&2
  exit 78
fi
if ! assert_runtime_directory "$TMPDIR" "$EXPECTED_UID" 700; then
  printf '%s\\n' 'nanocodex controller temporary path failed its identity check' >&2
  exit 78
fi
${role === "pr" ? `if ! "$NODE_BINARY" --eval ${shellQuote(renderPrPrepRuntimeValidationProgram(prPrep))}; then
  printf '%s\\n' 'nanocodex PR preparation account or helper identity drifted' >&2
  exit 78
fi
` : ""}
${role === "master" ? `if ! assert_runtime_directory ${shellQuote(rustSecRepository)} "$EXPECTED_UID" ${metadata.rustSec.mode.toString(8)}; then
  printf '%s\\n' 'nanocodex RustSec repository path failed its identity check' >&2
  exit 78
fi
` : ""}
cd "$REPOSITORY"

if [ "$WRAPPER_MODE" = probe ]; then
  exec "$NODE_BINARY" --eval ${shellQuote(preflightProgram)}
fi

assert_log_file() {
  log_path=$1
  [ ! -L "$log_path" ] && [ -f "$log_path" ] || return 1
  [ "$(/usr/bin/stat -f '%u' "$log_path" 2>/dev/null)" = "$EXPECTED_UID" ] || return 1
  [ "$(/usr/bin/stat -f '%l' "$log_path" 2>/dev/null)" = 1 ] || return 1
  [ "$(/usr/bin/stat -f '%Lp' "$log_path" 2>/dev/null)" = 600 ] || return 1
}

rotate_log() {
  log_path=$1
  archive_path=$2
  assert_log_file "$log_path" || return 1
  if [ -e "$archive_path" ] || [ -L "$archive_path" ]; then
    assert_log_file "$archive_path" || return 1
  fi
  log_size=$(/usr/bin/stat -f '%z' "$log_path" 2>/dev/null || printf invalid)
  case "$log_size" in ''|*[!0-9]*) return 1 ;; esac
  if [ "$log_size" -le "$MAX_CURRENT_LOG_BYTES" ]; then return 0; fi
  temporary_path="\${archive_path}.tmp.$$"
  /bin/rm -f "$temporary_path"
  if /usr/bin/tail -c "$RETAINED_LOG_BYTES" "$log_path" > "$temporary_path"; then
    /bin/chmod 600 "$temporary_path"
    /bin/mv -f "$temporary_path" "$archive_path"
    : > "$log_path"
    /bin/chmod 600 "$log_path"
  else
    /bin/rm -f "$temporary_path"
    return 1
  fi
}

if ! rotate_log "$STDOUT_LOG" "$STDOUT_ARCHIVE" ||
   ! rotate_log "$STDERR_LOG" "$STDERR_ARCHIVE"; then
  printf '%s\\n' 'nanocodex controller log paths failed their safety check' >&2
  exit 78
fi
readonly CONTROLLER_PID=$$
rotate_logs() {
  while /bin/kill -0 "$CONTROLLER_PID" 2>/dev/null; do
    /bin/sleep "$LOG_ROTATION_INTERVAL_SECONDS" || return 0
    /bin/kill -0 "$CONTROLLER_PID" 2>/dev/null || return 0
    if ! rotate_log "$STDOUT_LOG" "$STDOUT_ARCHIVE" ||
       ! rotate_log "$STDERR_LOG" "$STDERR_ARCHIVE"; then
      printf '%s\n' 'nanocodex controller stopped after a log path failed its safety check' >&2
      /bin/kill -TERM "$CONTROLLER_PID" 2>/dev/null || true
      return 1
    fi
  done
}

# This monitor is spawned before secrets exist and therefore cannot inherit
# them. The shell variables below are not exported until every read succeeds.
rotate_logs </dev/null >/dev/null &
readonly LOG_MONITOR_PID=$!
cleanup_monitor() {
  /bin/kill "$LOG_MONITOR_PID" 2>/dev/null || true
}
trap cleanup_monitor 0 1 2 15

secret_failure() {
  secret_name=$1
  ${ROLE_SECRET_ALLOWLISTS[role].map((name) => `unset ${name}`).join("\n  ")}
  printf '%s\\n' "nanocodex controller Keychain item could not be loaded: $secret_name" >&2
  exit 78
}

${secretAssignments}
${role === "pr" ? `if [ "$NANOCODEX_CI_TOKEN" = "$NANOCODEX_GITHUB_STATUS_TOKEN" ]; then
  secret_failure 'role tokens must be distinct'
fi
` : ""}${secretExports}

# This trusted, fixed Node program sees the exact environment intended for the
# controller and validates names only. It never prints or accepts secret values.
if ! "$NODE_BINARY" --eval ${shellQuote(liveProgram)}; then
  printf '%s\\n' 'nanocodex controller live environment probe failed' >&2
  exit 78
fi

trap - 0 1 2 15
exec "$NODE_BINARY" "$CONTROLLER_SCRIPT"
`;
}

export function renderLiveEnvironmentProbeProgram({
  role,
  architecture,
  fixedEnvironment,
}) {
  validateRole(role);
  if (architecture !== "arm64" && architecture !== "x64") {
    throw new ControllerServiceConfigurationError("live probe architecture is invalid");
  }
  if (!isStringRecord(fixedEnvironment)) {
    throw new ControllerServiceConfigurationError("live probe environment is invalid");
  }
  const allowed = ROLE_SECRET_ALLOWLISTS[role];
  const denied = AMBIENT_AUTHORITY_KEYS.filter((name) => !allowed.includes(name));
  return `const allowed=${JSON.stringify(allowed)};const denied=${JSON.stringify(denied)};` +
    `const fixed=${JSON.stringify(fixedEnvironment)};const own=(n)=>Object.prototype.hasOwnProperty.call(process.env,n);` +
    `const credential=(n)=>/(?:TOKEN|SECRET|PASSWORD|API_KEY|PRIVATE_KEY)/i.test(n);` +
    `const present=Object.keys(process.env).filter(credential).sort();const expected=[...allowed].sort();` +
    `const missing=allowed.filter((n)=>typeof process.env[n]!=="string"||process.env[n].trim().length===0);` +
    `const values=allowed.map((n)=>process.env[n].trim());const duplicate=new Set(values).size!==values.length;` +
    `const forbidden=denied.filter(own);const wrong=Object.entries(fixed).filter(([n,v])=>process.env[n]!==v);` +
    `const keys=Object.keys(process.env).sort();const expectedKeys=[...Object.keys(fixed),...allowed].sort();` +
    `const identity=[process.release.name,process.platform,process.arch].join("/");` +
    `if(identity!==${JSON.stringify(`node/darwin/${architecture}`)}||missing.length||duplicate||forbidden.length||wrong.length||JSON.stringify(present)!==JSON.stringify(expected)` +
    `${role === "pr" ? `||JSON.stringify(keys)!==JSON.stringify(expectedKeys)` : ""}){` +
    `process.stderr.write("controller environment names failed allowlist validation\\n");process.exit(78);}`;
}

export function validateEnvironmentProbe(value, {
  role,
  architecture,
  expected = {},
} = {}) {
  validateRole(role);
  if (architecture != null && architecture !== "arm64" && architecture !== "x64") {
    throw new ControllerServiceConfigurationError("probe architecture is invalid");
  }
  if (
    value == null ||
    typeof value !== "object" ||
    Array.isArray(value) ||
    value.version !== 1 ||
    value.role !== role ||
    !/^node\/darwin\/(?:arm64|x64)$/.test(value.identity) ||
    !Array.isArray(value.credentialKeys) ||
    value.credentialKeys.length !== 0 ||
    !Array.isArray(value.deniedKeys) ||
    value.deniedKeys.length !== 0 ||
    !isStringRecord(value.fixed)
  ) {
    throw new ControllerServiceConfigurationError("live wrapper environment probe failed closed");
  }
  if (architecture != null && value.identity !== `node/darwin/${architecture}`) {
    throw new ControllerServiceConfigurationError("wrapper probe used the wrong Node architecture");
  }
  for (const [name, expectedValue] of Object.entries(expected)) {
    if (value.fixed[name] !== expectedValue) {
      throw new ControllerServiceConfigurationError(
        `wrapper probe returned the wrong fixed ${name}`,
      );
    }
  }
  validateOrigin(value.fixed.NANOCODEX_CI_ORIGIN, "probed CI origin");
  const repository = validateAbsolutePath(value.fixed.NANOCODEX_REPO, "probed repository");
  if (value.controllerScript !== controllerScriptPath(repository, role)) {
    throw new ControllerServiceConfigurationError("wrapper probe returned the wrong script path");
  }
  validateAbsolutePath(value.nodeBinary, "probed Node binary");
  if (role === "pr") {
    validateRecordedPrPrepIdentity(value.prPrep);
    if (value.sudoProbe !== true) {
      throw new ControllerServiceConfigurationError(
        "wrapper probe did not record a successful PR preparation sudo probe",
      );
    }
    if (value.fixed.NANOCODEX_CI_PR_PREP_USER !== value.prPrep.username) {
      throw new ControllerServiceConfigurationError(
        "wrapper probe PR preparation username does not match its fixed environment",
      );
    }
  } else if (value.prPrep != null || value.sudoProbe != null) {
    throw new ControllerServiceConfigurationError(
      "master wrapper probe contains forbidden PR preparation state",
    );
  }
  return value;
}

export function keychainStoreArguments(role, secretName) {
  const definition = roleDefinition(role);
  const account = keychainAccountName(role, secretName);
  return Object.freeze([
    "add-generic-password",
    "-a",
    account,
    "-s",
    definition.keychainService,
    "-D",
    "application password",
    "-l",
    `Nanocodex ${role} controller ${account} as ${secretName}`,
    "-T",
    SECURITY,
    "-U",
    "-w",
  ]);
}

export function keychainFindArguments(role, secretName) {
  const definition = roleDefinition(role);
  const account = keychainAccountName(role, secretName);
  return Object.freeze([
    "find-generic-password",
    "-a",
    account,
    "-s",
    definition.keychainService,
  ]);
}

export function keychainDeleteArguments(role, secretName) {
  const definition = roleDefinition(role);
  const account = keychainAccountName(role, secretName);
  return Object.freeze([
    "delete-generic-password",
    "-a",
    account,
    "-s",
    definition.keychainService,
  ]);
}

export function shouldStoreKeychainSecret({ command, replaceSecrets, present }) {
  if (command !== "install" && command !== "update") {
    throw new ControllerServiceConfigurationError("Keychain storage policy requires install or update");
  }
  if (typeof replaceSecrets !== "boolean" || typeof present !== "boolean") {
    throw new ControllerServiceConfigurationError("Keychain storage policy inputs must be boolean");
  }
  return command === "install" || replaceSecrets || !present;
}

export function assertNoColocatedRoleState(role, {
  loaded,
  plist,
  wrapper,
  state,
  keychainItems,
  macosRunner,
}) {
  validateRole(role);
  if (
    typeof loaded !== "boolean" ||
    typeof plist !== "boolean" ||
    typeof wrapper !== "boolean" ||
    typeof state !== "boolean" ||
    !Array.isArray(keychainItems) ||
    keychainItems.length !== ROLE_SECRET_ALLOWLISTS[role === "master" ? "pr" : "master"].length ||
    keychainItems.some((value) => typeof value !== "boolean") ||
    macosRunner == null ||
    typeof macosRunner !== "object" ||
    Array.isArray(macosRunner) ||
    typeof macosRunner.loaded !== "boolean" ||
    typeof macosRunner.plist !== "boolean" ||
    typeof macosRunner.state !== "boolean" ||
    typeof macosRunner.keychainItem !== "boolean"
  ) {
    throw new ControllerServiceConfigurationError("dedicated-role isolation state is invalid");
  }
  if (loaded || plist || wrapper || state || keychainItems.some(Boolean)) {
    const opposite = role === "master" ? "pr" : "master";
    throw new ControllerServiceConfigurationError(
      `${role} controller refuses the ${opposite} role in the same passwd account; ` +
        "use distinct dedicated login users",
    );
  }
  if (
    macosRunner.loaded || macosRunner.plist || macosRunner.state ||
    macosRunner.keychainItem
  ) {
    throw new ControllerServiceConfigurationError(
      `${role} controller refuses the macOS runner role in the same passwd account; ` +
        "use four distinct dedicated identities",
    );
  }
  return true;
}

export function validateLifecycleState(command, { loaded, plist, wrapper }) {
  if (
    (command !== "install" && command !== "update") ||
    typeof loaded !== "boolean" ||
    typeof plist !== "boolean" ||
    typeof wrapper !== "boolean"
  ) {
    throw new ControllerServiceConfigurationError("controller lifecycle state is invalid");
  }
  const anyArtifacts = plist || wrapper;
  const complete = plist && wrapper;
  if (command === "install" && (anyArtifacts || loaded)) {
    throw new ControllerServiceConfigurationError(
      "controller service artifacts already exist; use update or uninstall",
    );
  }
  if (command === "update" && !anyArtifacts && !loaded) {
    throw new ControllerServiceConfigurationError(
      "controller service is absent; use install",
    );
  }
  return Object.freeze({ anyArtifacts, complete, partial: anyArtifacts && !complete });
}

export function launchctlArguments(action, { uid, role, paths }) {
  validateServicePaths(paths, role);
  const context = validateGuiUid(uid, role);
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
      throw new ControllerServiceConfigurationError(
        `unsupported launchctl action: ${String(action)}`,
      );
  }
}

export function classifyLaunchctlPrint({ code, signal = null, stderr = "" }) {
  if (signal != null) {
    throw new ControllerServiceConfigurationError(
      `launchctl service query was terminated by ${String(signal)}${probeDetail(stderr)}`,
    );
  }
  if (code === 0) return true;
  if (
    code === 113 &&
    (stderr === "" ||
      /^(?:Bad request\.\n)?Could not find service "[^"\r\n]+" in domain for user gui: [1-9][0-9]*\n$/.test(stderr))
  ) return false;
  throw new ControllerServiceConfigurationError(
    `launchctl service query failed with exit ${String(code)}${probeDetail(stderr)}`,
  );
}

export function parseLaunchctlPrint(result) {
  const loaded = classifyLaunchctlPrint(result);
  if (!loaded) {
    return Object.freeze({
      loaded: false,
      running: false,
      state: null,
      pid: null,
      lastExitCode: null,
    });
  }
  const stdout = typeof result.stdout === "string" ? result.stdout : "";
  const topLevel = "(?:\\t| {4})";
  const stateMatches = [
    ...stdout.matchAll(new RegExp(`^${topLevel}state = ([A-Za-z][A-Za-z0-9_-]*)\\s*$`, "gm")),
  ];
  const pidMatches = [
    ...stdout.matchAll(new RegExp(`^${topLevel}pid = ([1-9][0-9]*)\\s*$`, "gm")),
  ];
  const exitMatches = [
    ...stdout.matchAll(new RegExp(`^${topLevel}last exit code = (-?[0-9]+)\\s*$`, "gm")),
  ];
  if (stateMatches.length !== 1 || pidMatches.length > 1 || exitMatches.length > 1) {
    throw new ControllerServiceConfigurationError(
      "launchctl returned an ambiguous controller service state",
    );
  }
  const state = stateMatches[0][1];
  const pid = pidMatches.length === 0 ? null : Number(pidMatches[0][1]);
  const lastExitCode = exitMatches.length === 0 ? null : Number(exitMatches[0][1]);
  if (
    (pid != null && (!Number.isSafeInteger(pid) || pid <= 0)) ||
    (lastExitCode != null && !Number.isSafeInteger(lastExitCode))
  ) {
    throw new ControllerServiceConfigurationError("launchctl returned invalid numeric state");
  }
  const running = state === "running" && pid != null;
  if ((state === "running") !== (pid != null)) {
    throw new ControllerServiceConfigurationError(
      "launchctl controller state and PID do not agree",
    );
  }
  return Object.freeze({ loaded, running, state, pid, lastExitCode });
}

export function matchesExpectedControllerCommand(output, nodeBinary, controllerScript) {
  const node = validateAbsolutePath(nodeBinary, "expected controller Node binary");
  const script = validateAbsolutePath(controllerScript, "expected controller script");
  if (typeof output !== "string" || output.includes("\u0000")) return false;
  return output.replace(/\r?\n$/, "") === `${node} ${script}`;
}

export function classifyKeychainFind({ code, signal = null, stderr = "" }) {
  if (signal != null) {
    throw new ControllerServiceConfigurationError(
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
  throw new ControllerServiceConfigurationError(
    `Keychain item query failed with exit ${String(code)}${probeDetail(stderr)}`,
  );
}

export function parseCliArguments(argv) {
  if (!Array.isArray(argv) || argv.some((value) => typeof value !== "string")) {
    throw new ControllerServiceConfigurationError("command arguments must be strings");
  }
  if (argv.length === 0 || ["help", "--help", "-h"].includes(argv[0])) {
    if (argv.length > 1) {
      throw new ControllerServiceConfigurationError("help accepts no arguments");
    }
    return Object.freeze({ command: "help" });
  }
  const [command, ...arguments_] = argv;
  if (command === "install" || command === "update") {
    const values = new Map();
    let replaceSecrets = false;
    const valueFlags = new Set([
      "--role",
      "--origin",
      "--web-origin",
      "--node",
      "--repo",
      "--rustsec-repo",
      "--cloudflare-account-id",
      "--prep-user",
      "--cargo-sha256",
    ]);
    for (let index = 0; index < arguments_.length; index += 1) {
      const flag = arguments_[index];
      if (flag === "--replace-secrets") {
        if (command !== "update" || replaceSecrets) {
          throw new ControllerServiceConfigurationError(
            "--replace-secrets is accepted once by update only",
          );
        }
        replaceSecrets = true;
        continue;
      }
      if (!valueFlags.has(flag)) {
        throw new ControllerServiceConfigurationError(
          `unknown ${command} option: ${String(flag)}`,
        );
      }
      if (values.has(flag)) {
        throw new ControllerServiceConfigurationError(`${flag} may be provided only once`);
      }
      const value = arguments_[++index];
      if (value == null || value.startsWith("--")) {
        throw new ControllerServiceConfigurationError(`${flag} requires a value`);
      }
      values.set(flag, value);
    }
    for (const flag of ["--role", "--origin", "--node", "--repo"]) {
      if (!values.has(flag)) {
        throw new ControllerServiceConfigurationError(`${command} requires ${flag}`);
      }
    }
    const role = validateRole(values.get("--role"));
    if (role === "master") {
      for (const flag of ["--rustsec-repo", "--cloudflare-account-id"]) {
        if (!values.has(flag)) {
          throw new ControllerServiceConfigurationError(`${command} master requires ${flag}`);
        }
      }
      if (values.has("--prep-user") || values.has("--cargo-sha256")) {
        throw new ControllerServiceConfigurationError(
          "--prep-user and --cargo-sha256 are forbidden for the master role",
        );
      }
    } else {
      for (const flag of ["--web-origin", "--rustsec-repo", "--cloudflare-account-id"]) {
        if (values.has(flag)) {
          throw new ControllerServiceConfigurationError(`${flag} is forbidden for the pr role`);
        }
      }
      if (!values.has("--prep-user") || !values.has("--cargo-sha256")) {
        throw new ControllerServiceConfigurationError(
          `${command} pr requires --prep-user and --cargo-sha256`,
        );
      }
    }
    const origin = validateOrigin(values.get("--origin"));
    return Object.freeze({
      command,
      role,
      origin,
      webOrigin: role === "master"
        ? validateOrigin(values.get("--web-origin") ?? origin, "web origin")
        : undefined,
      nodeBinary: validateAbsolutePath(values.get("--node"), "Node binary"),
      repository: validateAbsolutePath(values.get("--repo"), "repository"),
      rustSecRepository: role === "master"
        ? validateAbsolutePath(values.get("--rustsec-repo"), "RustSec repository")
        : undefined,
      cloudflareAccountId: role === "master"
        ? validateCloudflareAccountId(values.get("--cloudflare-account-id"))
        : undefined,
      ...(role === "pr"
        ? {
            prepUsername: validatePrPrepUsername(values.get("--prep-user")),
            cargoSha256: validateSha256(values.get("--cargo-sha256"), "Cargo SHA-256"),
          }
        : {}),
      replaceSecrets,
    });
  }
  if (command === "status") {
    return Object.freeze({ command, role: parseRoleOnly(arguments_, "status") });
  }
  if (command === "uninstall") {
    let removeData = false;
    const filtered = [];
    for (const argument of arguments_) {
      if (argument === "--remove-data") {
        if (removeData) {
          throw new ControllerServiceConfigurationError(
            "--remove-data may be provided only once",
          );
        }
        removeData = true;
      } else {
        filtered.push(argument);
      }
    }
    return Object.freeze({
      command,
      role: parseRoleOnly(filtered, "uninstall"),
      removeData,
    });
  }
  throw new ControllerServiceConfigurationError(`unknown command: ${String(command)}`);
}

export function serviceRunbook(role) {
  validateRole(role);
  const secrets = ROLE_SECRET_ALLOWLISTS[role]
    .map((name) => {
      const account = keychainAccountName(role, name);
      return account === name ? name : `${account} as ${name}`;
    })
    .join(", ");
  return `Operator assumptions for the ${role} controller:
  - Four OS identities remain distinct: master controller, PR controller, PR Cargo preparation, and macOS runner.
  - Run this LaunchAgent from a dedicated macOS login user with a passwd-backed home; master and PR require distinct users, and the installer refuses co-location.
  - The service is in that user's Aqua launchd domain and requires the dedicated user to remain logged in.
  - The trusted checkout and selected Node path are fixed, real, and not group/world writable; PR's sudo Node and every ancestor are root:wheel, no-ACL, and contain no symlink.
  - Keychain loads exactly: ${secrets}.
  - The wrapper reconstructs an empty environment before Keychain access; no token is written to plist, argv, or a file.
  - Keep the account free of unrelated source-control, deploy, mirror, registry, release, macOS-runner, cloud, and SSH credentials.
  - Never reuse either controller account, the macOS runner, root, admin, guest, or a shared account as the PR preparation user.
  - Never share a writable group with PR preparation or directly chown/chmod/ACL-widen a PR checkout; preparation independently fetches exact refs into fresh private state.
${role === "pr" ? `  - PR install/update requires an existing local non-login --prep-user with a unique UID/GID, a root-owned non-writable /var/empty home, one non-nested dedicated primary group, no avoidable supplementary groups, and no credentials.
  - Root must separately install the byte-exact helper at ${PR_PREP_HELPER_PATH} mode 0555 and a visudo-checked root-owned 0440 exact-command NOSETENV sudoers rule.
  - Root must separately provision the selected global Node executable as a singly linked root:wheel file under an entirely root:wheel, no-ACL, non-group/world-writable real parent chain; controller-owned and symlink paths fail closed.
  - Root must provision pinned Cargo ${PR_PREP_CARGO_RELEASE} only at ${PR_PREP_CARGO_PATH}; install/update requires its exact SHA-256 and startup/status revalidate its bytes, version, ownership, link count, and real root-only parent chain.
  - LC_ALL=C sudo -n -l must expose one complete Defaults/grant inventory with timestamp_timeout=0 and only the two NOPASSWD:NOSETENV helper commands. Unsupported listing formats or inherited/group grants fail closed and must be removed by the operator.
  - The wrapper validates account/group/helper/Cargo/Node and complete sudo identity before its first Keychain read; --build accepts canonical stdin only.
  - Uninstall never removes the prep account, helper, or sudoers rule; root performs that explicit operator cleanup.
` : ""}  - The PR preparation helper owns fresh private temporary home and Cargo state per build; the controller owns no persistent Cargo state.
  - The controllers retain their own kernel-held lock; this service adds no PID file or wrapper lock.
  - launchd restarts the controller after every exit; update and uninstall stop it with an explicit bootout.
  - stdout/stderr retain one ${RETAINED_LOG_BYTES}-byte archive and rotate a current file above ${MAX_CURRENT_LOG_BYTES} bytes every ${LOG_ROTATION_INTERVAL_SECONDS} seconds.
  - Rotate the role allowlist with update --replace-secrets; ordinary update preserves existing Keychain items.`;
}

export async function assertSafeOwnedEntry(path, {
  uid,
  kind,
  exactMode,
  allowRootOwner = false,
  requireSingleLink = kind === "file",
  executable = false,
} = {}) {
  const checked = validateAbsolutePath(path);
  if (!Number.isSafeInteger(uid) || uid <= 0) {
    throw new ControllerServiceConfigurationError("safe path check requires a non-root UID");
  }
  if (kind !== "file" && kind !== "directory") {
    throw new ControllerServiceConfigurationError("safe path check requires a file or directory");
  }
  let metadata;
  try {
    metadata = await lstat(checked);
  } catch (cause) {
    throw new ControllerServiceConfigurationError(`${checked} does not exist`, { cause });
  }
  if (metadata.isSymbolicLink()) {
    throw new ControllerServiceConfigurationError(`${checked} must not be a symbolic link`);
  }
  if (
    (kind === "file" && !metadata.isFile()) ||
    (kind === "directory" && !metadata.isDirectory())
  ) {
    throw new ControllerServiceConfigurationError(`${checked} must be a real ${kind}`);
  }
  if (metadata.uid !== uid && !(allowRootOwner && metadata.uid === 0)) {
    throw new ControllerServiceConfigurationError(`${checked} is owned by the wrong user`);
  }
  const mode = metadata.mode & 0o777;
  if ((metadata.mode & 0o7000) !== 0) {
    throw new ControllerServiceConfigurationError(`${checked} has forbidden special mode bits`);
  }
  if ((mode & 0o022) !== 0) {
    throw new ControllerServiceConfigurationError(`${checked} is group or world writable`);
  }
  if (exactMode != null && mode !== exactMode) {
    throw new ControllerServiceConfigurationError(
      `${checked} has mode ${mode.toString(8)}, expected ${exactMode.toString(8)}`,
    );
  }
  if (requireSingleLink && metadata.nlink !== 1) {
    throw new ControllerServiceConfigurationError(`${checked} must have exactly one hard link`);
  }
  let canonical;
  try {
    canonical = validateAbsolutePath(await realpath(checked), `${checked} canonical path`);
  } catch (cause) {
    if (cause instanceof ControllerServiceConfigurationError) throw cause;
    throw new ControllerServiceConfigurationError(`${checked} could not be canonicalized`, {
      cause,
    });
  }
  if (canonical !== checked) {
    throw new ControllerServiceConfigurationError(`${checked} contains a symbolic-link path`);
  }
  if (executable) {
    try {
      await access(checked, fsConstants.X_OK);
    } catch (cause) {
      throw new ControllerServiceConfigurationError(`${checked} is not executable`, { cause });
    }
  }
  return Object.freeze({
    path: checked,
    uid: metadata.uid,
    mode,
    nlink: metadata.nlink,
  });
}

export async function runGeneratedWrapperProbe(wrapperPath, {
  role,
  architecture,
  expected = {},
  timeoutMs = 5_000,
} = {}) {
  validateAbsolutePath(wrapperPath, "wrapper probe path");
  validateRole(role);
  const sentinel = `synthetic-${randomUUID()}`;
  const contaminated = {
    HOME: "/wrong/ambient/home",
    PATH: "/wrong/ambient/path",
    NANOCODEX_UNKNOWN_PRIVATE_KEY: sentinel,
    NANOCODEX_CI_PR_PREP_USER: "ambient-wrong-prep-user",
  };
  for (const name of AMBIENT_AUTHORITY_KEYS) contaminated[name] = sentinel;
  // Exercise the same native pre-interpreter boundary as the LaunchAgent. The
  // synthetic ambient authority reaches /usr/bin/env itself but no shell.
  const result = await runOutputProbe(ENV, ["-i", wrapperPath, EXTERNAL_PROBE_ARGUMENT], {
    env: contaminated,
    timeoutMs,
  });
  if (result.timedOut) {
    throw new ControllerServiceConfigurationError("wrapper environment probe timed out");
  }
  if (result.code !== 0) {
    throw new ControllerServiceConfigurationError(
      `wrapper environment probe failed with exit ${result.code}${probeDetail(result.stderr)}`,
    );
  }
  if (result.stderr !== "") {
    throw new ControllerServiceConfigurationError(
      "wrapper environment probe succeeded with forbidden stderr",
    );
  }
  let parsed;
  try {
    parsed = JSON.parse(result.stdout);
  } catch (cause) {
    throw new ControllerServiceConfigurationError(
      "wrapper environment probe did not return bounded JSON",
      { cause },
    );
  }
  return validateEnvironmentProbe(parsed, { role, architecture, expected });
}

export async function main(argv = process.argv.slice(2)) {
  const options = parseCliArguments(argv);
  if (options.command === "help") {
    process.stdout.write(usage());
    return;
  }
  const context = runtimeContext();
  const paths = deriveServicePaths(context.homeDirectory, options.role);
  let result;
  if (options.command === "install" || options.command === "update") {
    result = await installOrUpdate(options, context, paths);
  } else if (options.command === "status") {
    result = await serviceStatus(options.role, context, paths);
  } else {
    result = await uninstall(options, context, paths);
  }
  process.stdout.write(`${JSON.stringify(result, null, 2)}\n`);
}

async function installOrUpdate(options, context, paths) {
  await validateExistingPathChain(paths, options.role, context.uid);
  await assertNoColocatedRole(options.role, context);
  let trustedHelperPayload;
  let trustedPrPrepNode;
  if (options.role === "pr") {
    const repository = await validateInstallerRepository(options.repository, context.uid);
    const nodeBinary = validateAbsolutePath(options.nodeBinary, "Node binary");
    trustedPrPrepNode = await inspectPrPrepNode(nodeBinary, context);
    trustedHelperPayload = await captureTrustedPrPrepHelperSource(
      join(repository, "web", "scripts", "ci-pr-cargo-builder.mjs"),
      { expectedUid: context.uid },
    );
    await Promise.all([
      inspectPrPrepAccount(options.prepUsername, context),
      inspectPrPrepCargo(options.cargoSha256, context.architecture),
    ]);
  }
  process.stderr.write(`${serviceRunbook(options.role)}\n`);
  if (options.role === "pr") {
    process.stderr.write(`${renderPrPrepProvisioning({
      controllerUsername: context.username,
      prepUsername: options.prepUsername,
      nodeBinary: options.nodeBinary,
      helperPayload: trustedHelperPayload,
      cargoSha256: options.cargoSha256,
    })}\n`);
  }
  const [initialLaunchStatus, artifacts] = await Promise.all([
    serviceLaunchStatus(context.uid, options.role, paths),
    installationArtifacts(paths),
  ]);
  const { loaded } = initialLaunchStatus;
  validateLifecycleState(options.command, {
    loaded,
    plist: Boolean(artifacts.plist),
    wrapper: Boolean(artifacts.wrapper),
  });
  await assertSafeArtifacts(artifacts, context.uid, options.command === "update");
  const runtime = await validateTrustedRuntime(options, context, paths, {
    trustedHelperPayload,
    trustedPrPrepNode,
  });
  await ensureServiceDirectories(paths, options.role, context.uid);
  await assertSafeLogAndStateFiles(paths, options.role, context.uid, false);
  const wrapper = renderControllerWrapper({
    ...options,
    ...runtime,
    paths,
    uid: context.uid,
    username: context.username,
    architecture: context.architecture,
  });
  const plist = renderLaunchAgentPlist(paths, options.role, runtime.repository);
  const temporaryWrapper = await writeTemporaryFile(paths.wrapperPath, wrapper, 0o700);
  try {
    await runGeneratedWrapperProbe(temporaryWrapper, {
      role: options.role,
      architecture: context.architecture,
      expected: {
        HOME: paths.homeDirectory,
        TMPDIR: paths.temporaryDirectory,
        NANOCODEX_CI_ORIGIN: options.origin,
        NANOCODEX_REPO: runtime.repository,
        ...(options.role === "pr"
          ? { NANOCODEX_CI_PR_PREP_USER: runtime.prPrep.username }
          : {}),
      },
    });
    for (const secretName of ROLE_SECRET_ALLOWLISTS[options.role]) {
      const present = await keychainItemExists(options.role, secretName, context);
      if (shouldStoreKeychainSecret({
        command: options.command,
        replaceSecrets: options.replaceSecrets,
        present,
      })) {
        const account = keychainAccountName(options.role, secretName);
        process.stderr.write(
          `Store ${account} for the ${options.role} controller as ${secretName} at the ` +
            "Keychain prompt; " +
            "the installer never reads it.\n",
        );
        await runChecked(SECURITY, keychainStoreArguments(options.role, secretName), {
          operation: `store ${options.role} ${secretName} in Keychain`,
          context,
          stdio: ["inherit", "ignore", "inherit"],
          allowSuccessfulStderr: true,
          timeoutMs: KEYCHAIN_PROMPT_TIMEOUT_MS,
        });
      }
    }
    await rename(temporaryWrapper, paths.wrapperPath);
    await atomicWriteFile(paths.plistPath, plist, 0o600);
  } finally {
    await rm(temporaryWrapper, { force: true });
  }

  if (loaded) {
    await runChecked(
      LAUNCHCTL,
      launchctlArguments("bootout", { uid: context.uid, role: options.role, paths }),
      { operation: "boot out existing controller LaunchAgent", context },
    );
  }
  await runChecked(
    LAUNCHCTL,
    launchctlArguments("bootstrap", { uid: context.uid, role: options.role, paths }),
    { operation: "bootstrap controller LaunchAgent", context },
  );
  await runChecked(
    LAUNCHCTL,
    launchctlArguments("kickstart", { uid: context.uid, role: options.role, paths }),
    { operation: "kickstart controller LaunchAgent", context },
  );
  const running = await waitForServiceRunning(
    context.uid,
    options.role,
    paths,
    {
      nodeBinary: runtime.nodeBinary,
      controllerScript: runtime.controllerScript,
      context,
    },
  );

  return Object.freeze({
    command: options.command,
    role: options.role,
    label: ROLE_DEFINITIONS[options.role].label,
    guiDomain: context.guiDomain,
    running: true,
    pid: running.pid,
    serviceDirectory: paths.serviceDirectory,
    plistPath: paths.plistPath,
    repository: runtime.repository,
    controllerScript: runtime.controllerScript,
    nodeBinary: runtime.nodeBinary,
    keychainSecrets: ROLE_SECRET_ALLOWLISTS[options.role],
    ...(runtime.prPrep
      ? { prPrep: runtime.prPrep, sudoProbe: true }
      : {}),
  });
}

async function serviceStatus(role, context, paths) {
  validateServicePaths(paths, role);
  await validateExistingPathChain(paths, role, context.uid);
  await assertNoColocatedRole(role, context);
  await assertSafeLogAndStateFiles(paths, role, context.uid, false);
  const [initialLaunchStatus, artifacts] = await Promise.all([
    serviceLaunchStatus(context.uid, role, paths),
    installationArtifacts(paths),
  ]);
  await assertSafeArtifacts(artifacts, context.uid, true);
  const installed = Boolean(artifacts.plist && artifacts.wrapper);
  let environmentProbe = false;
  let launchStatus = initialLaunchStatus;
  let controllerProcess = false;
  let prPrep;
  if (installed) {
    const probe = await runGeneratedWrapperProbe(paths.wrapperPath, {
      role,
      architecture: context.architecture,
      expected: {
        HOME: paths.homeDirectory,
        TMPDIR: paths.temporaryDirectory,
      },
    });
    const installedPlist = await readBoundedTextFile(
      paths.plistPath,
      MAX_PLIST_BYTES,
      context.uid,
    );
    const expectedPlist = renderLaunchAgentPlist(
      paths,
      role,
      probe.fixed.NANOCODEX_REPO,
    );
    if (installedPlist !== expectedPlist) {
      throw new ControllerServiceConfigurationError(
        "installed LaunchAgent plist does not match the fixed generated contract",
      );
    }
    environmentProbe = true;
    if (role === "pr") {
      prPrep = await validatePrPrepRuntime({
        prepUsername: probe.prPrep.username,
        nodeBinary: probe.nodeBinary,
        context,
        expected: probe.prPrep,
      });
    }
    launchStatus = await serviceLaunchStatus(context.uid, role, paths);
    controllerProcess = launchStatus.running && await expectedControllerProcess(
      launchStatus.pid,
      probe.nodeBinary,
      probe.controllerScript,
      context,
    );
  }
  if (role === "pr" && !installed) {
    throw new ControllerServiceConfigurationError(
      "PR status refuses Keychain inspection without an installed, validated prep boundary",
    );
  }
  const keychainEntries = await Promise.all(
    ROLE_SECRET_ALLOWLISTS[role].map(async (name) => [
      name,
      await keychainItemExists(role, name, context),
    ]),
  );
  return Object.freeze({
    command: "status",
    role,
    label: ROLE_DEFINITIONS[role].label,
    guiDomain: context.guiDomain,
    installed,
    loaded: launchStatus.loaded,
    running: controllerProcess,
    launchdRunning: launchStatus.running,
    controllerProcess,
    state: launchStatus.state,
    pid: launchStatus.pid,
    lastExitCode: launchStatus.lastExitCode,
    environmentProbe,
    keychain: Object.freeze(Object.fromEntries(keychainEntries)),
    artifacts: Object.freeze({
      plist: Boolean(artifacts.plist),
      wrapper: Boolean(artifacts.wrapper),
      logsDirectory: await exists(paths.logsDirectory),
      temporaryDirectory: await exists(paths.temporaryDirectory),
      state: role === "pr" && await exists(paths.statePath),
    }),
    serviceDirectory: paths.serviceDirectory,
    plistPath: paths.plistPath,
    ...(prPrep ? { prPrep, sudoProbe: true } : {}),
  });
}

async function uninstall(options, context, paths) {
  const { role } = options;
  validateServicePaths(paths, role);
  await validateExistingPathChain(paths, role, context.uid);
  const artifacts = await installationArtifacts(paths);
  await assertSafeArtifacts(artifacts, context.uid, true);
  await assertSafeLogAndStateFiles(paths, role, context.uid, false);
  const launchStatus = await serviceLaunchStatus(context.uid, role, paths);
  if (launchStatus.loaded) {
    await runChecked(
      LAUNCHCTL,
      launchctlArguments("bootout", { uid: context.uid, role, paths }),
      { operation: "boot out controller LaunchAgent", context },
    );
  }
  for (const secretName of ROLE_SECRET_ALLOWLISTS[role]) {
    if (await keychainItemExists(role, secretName, context)) {
      await runChecked(SECURITY, keychainDeleteArguments(role, secretName), {
        operation: `delete ${role} ${secretName} from Keychain`,
        context,
      });
    }
  }
  await rm(paths.plistPath, { force: true });
  await rm(paths.wrapperPath, { force: true });
  if (options.removeData) {
    assertExactServiceDirectory(paths.serviceDirectory, paths, role);
    await assertSafeRemovalTree(paths.serviceDirectory, context.uid);
    await rm(paths.serviceDirectory, { recursive: true, force: true });
  }
  return Object.freeze({
    command: "uninstall",
    role,
    label: ROLE_DEFINITIONS[role].label,
    guiDomain: context.guiDomain,
    serviceDirectory: paths.serviceDirectory,
    dataPreserved: !options.removeData,
    ...(role === "pr"
      ? {
          operatorCleanup: prPrepOperatorCleanup(),
        }
      : {}),
  });
}

async function validateTrustedRuntime(options, context, paths, {
  trustedHelperPayload,
  trustedPrPrepNode,
} = {}) {
  const requestedRepository = await validateInstallerRepository(options.repository, context.uid);
  const repositoryMetadata = await assertSafeOwnedEntry(requestedRepository, {
    uid: context.uid,
    kind: "directory",
  });
  const controllerScript = controllerScriptPath(requestedRepository, options.role);
  const controllerMetadata = await assertSafeOwnedEntry(controllerScript, {
    uid: context.uid,
    kind: "file",
  });
  if (options.role === "pr") {
    await assertSafeOwnedEntry(controllerScriptPath(requestedRepository, "master"), {
      uid: context.uid,
      kind: "file",
    });
  }
  const nodeBinary = validateAbsolutePath(options.nodeBinary, "Node binary");
  if (pathContains(requestedRepository, nodeBinary)) {
    throw new ControllerServiceConfigurationError(
      "Node binary must be outside the checkout so installer probes never execute checkout code",
    );
  }
  let nodeMetadata;
  let prPrepNode;
  if (options.role === "pr") {
    prPrepNode = await inspectPrPrepNode(nodeBinary, context, {
      expected: trustedPrPrepNode,
    });
    nodeMetadata = Object.freeze({
      path: prPrepNode.path,
      uid: prPrepNode.uid,
      mode: prPrepNode.mode,
      nlink: prPrepNode.nlink,
    });
  } else {
    nodeMetadata = await assertSafeOwnedEntry(nodeBinary, {
      uid: context.uid,
      kind: "file",
      allowRootOwner: true,
      executable: true,
    });
    await assertSelectedNode(nodeBinary, context);
  }
  let prPrep;
  if (options.role === "pr") {
    prPrep = await validatePrPrepRuntime({
      prepUsername: options.prepUsername,
      nodeBinary,
      context,
      trustedHelperSha256: trustedHelperPayload?.sha256,
      cargoSha256: options.cargoSha256,
      trustedNode: prPrepNode,
    });
  }
  let rustSecRepository;
  let rustSecMetadata;
  if (options.role === "master") {
    rustSecRepository = validateAbsolutePath(options.rustSecRepository, "RustSec repository");
    if (
      pathContains(requestedRepository, rustSecRepository) ||
      pathContains(rustSecRepository, requestedRepository)
    ) {
      throw new ControllerServiceConfigurationError(
        "RustSec and product repositories must be separate real directories",
      );
    }
    rustSecMetadata = await assertSafeOwnedEntry(rustSecRepository, {
      uid: context.uid,
      kind: "directory",
    });
  }
  if (pathContains(requestedRepository, paths.serviceDirectory)) {
    throw new ControllerServiceConfigurationError(
      "controller service state must be outside the trusted checkout",
    );
  }
  return Object.freeze({
    repository: requestedRepository,
    controllerScript,
    nodeBinary,
    rustSecRepository,
    prPrep,
    metadata: Object.freeze({
      repository: repositoryMetadata,
      controller: controllerMetadata,
      node: nodeMetadata,
      ...(rustSecMetadata ? { rustSec: rustSecMetadata } : {}),
    }),
  });
}

async function validateInstallerRepository(repository, uid) {
  const requestedRepository = validateAbsolutePath(repository, "repository");
  const sourceRepository = validateAbsolutePath(
    await realpath(INSTALLER_REPOSITORY),
    "installer repository",
  );
  if (requestedRepository !== sourceRepository) {
    throw new ControllerServiceConfigurationError(
      "--repo must be the exact real checkout containing this installer",
    );
  }
  await assertSafeOwnedEntry(requestedRepository, {
    uid,
    kind: "directory",
  });
  return requestedRepository;
}

async function validatePrPrepRuntime({
  prepUsername,
  nodeBinary,
  context,
  trustedHelperSha256,
  cargoSha256,
  trustedNode,
  expected,
}) {
  const expectedIdentity = expected == null ? null : validateRecordedPrPrepIdentity(expected);
  const helperHash = validateSha256(
    trustedHelperSha256 ?? expectedIdentity?.helper.sha256,
    "trusted PR helper SHA-256",
  );
  const cargoHash = validateSha256(
    cargoSha256 ?? expectedIdentity?.cargo.sha256,
    "trusted Cargo SHA-256",
  );
  const expectedNode = trustedNode ?? expectedIdentity?.node;
  const [account, helper, cargo, node] = await Promise.all([
    inspectPrPrepAccount(prepUsername, context),
    inspectPrPrepHelper(helperHash),
    inspectPrPrepCargo(cargoHash, context.architecture),
    inspectPrPrepNode(nodeBinary, context, { expected: expectedNode }),
  ]);
  const sudoProbe = await probePrPrepSudoBoundary({
    controllerUsername: context.username,
    prepUsername: account.username,
    uid: account.uid,
    gid: account.gid,
    nodeBinary,
  });
  const nodeAfterSudo = await inspectPrPrepNode(nodeBinary, context, { expected: node });
  const recorded = validateRecordedPrPrepIdentity({
    ...account,
    controllerUsername: context.username,
    controllerUid: context.uid,
    helperVersion: PR_PREP_HELPER_VERSION,
    helper,
    cargo,
    node: nodeAfterSudo,
    nodeBinary,
    nodeIdentity: `node/darwin/${context.architecture}`,
    sudoProbe,
  });
  if (expectedIdentity != null) {
    assertPrPrepIdentityUnchanged(recorded, expectedIdentity);
  }
  return recorded;
}

async function inspectPrPrepAccount(prepUsername, context) {
  const username = validatePrPrepUsername(prepUsername);
  const [
    passwd,
    prepGroups,
    controllerGroups,
    localAccountUidList,
    localAccountGidList,
    localGroupList,
  ] = await Promise.all([
    runProbe(ID, ["-P", username], context),
    runProbe(ID, ["-G", username], context),
    runProbe(ID, ["-G", context.username], context),
    runProbe(
      DSCL,
      ["/Local/Default", "-list", "/Users", "UniqueID"],
      context,
    ),
    runProbe(
      DSCL,
      ["/Local/Default", "-list", "/Users", "PrimaryGroupID"],
      context,
    ),
    runProbe(DSCL, ["/Local/Default", "-list", "/Groups", "PrimaryGroupID"], context),
  ]);
  for (const [name, result] of [
    ["passwd", passwd],
    ["supplementary group", prepGroups],
    ["controller group", controllerGroups],
    ["local account UID", localAccountUidList],
    ["local account primary GID", localAccountGidList],
    ["local group", localGroupList],
  ]) {
    if (result.code !== 0 || result.signal != null || result.stderr !== "") {
      throw new ControllerServiceConfigurationError(
        `PR preparation ${name} record is missing, noisy, or unreadable` +
          probeDetail(result.stderr),
      );
    }
  }
  const parsed = parseMacOsPasswdRecord(passwd.stdout, username);
  const localAccounts = parseLocalAccountIdentityList(
    localAccountUidList.stdout,
    localAccountGidList.stdout,
  );
  const localGroups = parseLocalGroupIdentityList(localGroupList.stdout);
  const supplementaryGids = parseNumericGroupList(
    prepGroups.stdout,
    "PR preparation group list",
  );
  const expectedUnavoidableGroups = UNAVOIDABLE_MACOS_GROUPS.filter((group) =>
    supplementaryGids.includes(group.gid));
  const groupMappings = localGroups.filter((entry) => entry.gid === parsed.gid);
  if (groupMappings.length !== 1) {
    throw new ControllerServiceConfigurationError(
      "PR preparation primary GID must resolve to one unambiguous local group record",
    );
  }
  const [directoryUserResult, primaryGroupResult, homeSnapshot, ...unavoidableResults] =
    await Promise.all([
    runProbe(DSCL, [
      "/Local/Default",
      "-read",
      `/Users/${username}`,
      "RecordName",
      "UniqueID",
      "PrimaryGroupID",
      "NFSHomeDirectory",
      "UserShell",
      "GeneratedUID",
      "AuthenticationAuthority",
    ], context),
    runProbe(DSCL, [
      "/Local/Default",
      "-read",
      `/Groups/${groupMappings[0].name}`,
      "RecordName",
      "PrimaryGroupID",
      "GeneratedUID",
      "GroupMembership",
      "GroupMembers",
      "NestedGroups",
    ], context),
    inspectPrPrepHome(parsed.homeDirectory),
    ...expectedUnavoidableGroups.map((group) => runProbe(DSCL, [
      "/Local/Default",
      "-read",
      `/Groups/${group.name}`,
      "RecordName",
      "PrimaryGroupID",
      "GeneratedUID",
    ], context)),
  ]);
  for (const [name, result] of [
    ["authoritative user", directoryUserResult],
    ["authoritative primary group", primaryGroupResult],
    ...unavoidableResults.map((result, index) => [
      `unavoidable group ${expectedUnavoidableGroups[index].gid}`,
      result,
    ]),
  ]) {
    if (result.code !== 0 || result.signal != null || result.stderr !== "") {
      throw new ControllerServiceConfigurationError(
        `PR preparation ${name} record is missing, noisy, or unreadable` +
          probeDetail(result.stderr),
      );
    }
  }
  const directoryUser = parsePrPrepDirectoryUserRecord(directoryUserResult.stdout, username);
  const primaryGroup = parsePrPrepDirectoryGroupRecord(
    primaryGroupResult.stdout,
    groupMappings[0].name,
    parsed.gid,
  );
  validatePrPrepHomeSnapshot(homeSnapshot, parsed.homeDirectory);
  const unavoidableGroups = unavoidableResults.map((result, index) =>
    parseUnavoidableMacOsGroupRecord(result.stdout, expectedUnavoidableGroups[index].gid));
  const assignedRoles = await discoverPrepAccountRoles(parsed.homeDirectory);
  const controllerGids = parseNumericGroupList(
    controllerGroups.stdout,
    "controller group list",
  );
  const account = validatePrPrepAccount({
    ...parsed,
    supplementaryGids,
  }, {
    controllerUid: context.uid,
    controllerUsername: context.username,
    controllerGids,
    localAccounts,
    localGroups,
    unavoidableGroups,
    directoryUser,
    primaryGroup,
    assignedRoles,
  });
  return Object.freeze({ ...account, controllerGids });
}

async function inspectPrPrepHome(homeDirectory) {
  const parents = [];
  for (const path of PR_PREP_HOME_PARENT_DIRECTORIES) {
    parents.push(await filesystemSnapshot(path));
  }
  return Object.freeze({
    parents: Object.freeze(parents),
    home: await filesystemSnapshot(homeDirectory),
  });
}

async function discoverPrepAccountRoles(homeDirectory) {
  const launchAgents = join(homeDirectory, "Library", "LaunchAgents");
  const candidates = [
    ["master-controller", `${SERVICE_LABELS.master}.plist`],
    ["pr-controller", `${SERVICE_LABELS.pr}.plist`],
    ["macos-runner", "me.nanocodex.ci-macos-runner.plist"],
  ];
  const roles = [];
  for (const [role, filename] of candidates) {
    if (await pathMetadata(join(launchAgents, filename))) roles.push(role);
  }
  return Object.freeze(roles);
}

async function inspectPrPrepHelper(trustedSha256) {
  const parents = [];
  for (const path of ["/", "/Library", "/Library/PrivilegedHelperTools"]) {
    parents.push(await filesystemSnapshot(path));
  }
  const installed = await readBoundedBinarySnapshot(
    PR_PREP_HELPER_PATH,
    PR_PREP_HELPER_MAX_BYTES,
    { expectedUid: 0, expectedGid: 0, requireSafeMode: true },
  );
  const helperEntry = await filesystemSnapshot(PR_PREP_HELPER_PATH, installed);
  return validatePrPrepHelperSnapshot({
    parents,
    helper: helperEntry,
  }, trustedSha256);
}

async function inspectPrPrepCargo(trustedSha256, architecture) {
  const parents = [];
  for (const path of ["/", "/Library", "/Library/PrivilegedHelperTools"]) {
    parents.push(await filesystemSnapshot(path));
  }
  const opened = await readBoundedBinarySnapshot(
    PR_PREP_CARGO_PATH,
    PR_PREP_CARGO_MAX_BYTES,
    { expectedUid: 0, expectedGid: 0, requireSafeMode: true },
  );
  const cargoEntry = await filesystemSnapshot(PR_PREP_CARGO_PATH, opened);
  const version = await runOutputProbe(
    PR_PREP_CARGO_PATH,
    ["--version", "--verbose"],
    {
      cwd: "/",
      env: {
        HOME: "/var/empty",
        CARGO_HOME: "/var/empty",
        RUSTUP_HOME: "/var/empty",
        PATH: "/usr/bin:/bin",
        LANG: "C",
        LC_ALL: "C",
      },
      timeoutMs: PR_PREP_PROBE_TIMEOUT_MS,
    },
  );
  if (
    version.timedOut || version.signal != null || version.code !== 0 || version.stderr !== ""
  ) {
    throw new ControllerServiceConfigurationError(
      "fixed Cargo version probe must succeed quietly from the exact executable",
    );
  }
  return validatePrPrepCargoSnapshot({ parents, cargo: cargoEntry }, trustedSha256, version.stdout,
    architecture);
}

async function probePrPrepSudoBoundary({
  controllerUsername,
  prepUsername,
  uid,
  gid,
  nodeBinary,
}) {
  const policyEnvironment = {
    HOME: "/var/empty",
    USER: controllerUsername,
    LOGNAME: controllerUsername,
    PATH: "/usr/bin:/bin:/usr/sbin:/sbin",
    LANG: "C",
    LC_ALL: "C",
  };
  const policy = await runOutputProbe(
    SUDO,
    ["-n", "-l"],
    { cwd: "/", env: policyEnvironment, timeoutMs: PR_PREP_PROBE_TIMEOUT_MS },
  );
  const probe = await runOutputProbe(
    SUDO,
    prPrepSudoArguments("--probe", prepUsername, nodeBinary),
    { env: {}, timeoutMs: PR_PREP_PROBE_TIMEOUT_MS },
  );
  const build = await runOutputProbe(
    SUDO,
    prPrepSudoArguments("--build", prepUsername, nodeBinary),
    { env: {}, timeoutMs: PR_PREP_BUILD_AUTH_TIMEOUT_MS },
  );

  return validatePrPrepSudoEvidence({
    policy,
    probe,
    build,
  }, { uid, gid, controllerUsername, prepUsername, nodeBinary });
}

function validProbeResult(value) {
  return value != null &&
    typeof value === "object" &&
    Number.isInteger(value.code) &&
    (value.signal == null || typeof value.signal === "string") &&
    typeof value.stdout === "string" &&
    typeof value.stderr === "string" &&
    typeof value.timedOut === "boolean";
}

function normalizeWrapperConfiguration(configuration) {
  if (configuration == null || typeof configuration !== "object") {
    throw new ControllerServiceConfigurationError("wrapper configuration is required");
  }
  const role = validateRole(configuration.role);
  const paths = validateServicePaths(configuration.paths, role);
  const repository = validateAbsolutePath(configuration.repository, "repository");
  const controllerScript = validateAbsolutePath(
    configuration.controllerScript,
    "controller script",
  );
  if (controllerScript !== controllerScriptPath(repository, role)) {
    throw new ControllerServiceConfigurationError(
      "controller script must be the fixed role entrypoint inside the repository",
    );
  }
  const metadata = configuration.metadata;
  for (const name of ["repository", "controller", "node"]) {
    if (!validRecordedMetadata(metadata?.[name])) {
      throw new ControllerServiceConfigurationError(`wrapper ${name} metadata is invalid`);
    }
  }
  if (role === "master" && !validRecordedMetadata(metadata?.rustSec)) {
    throw new ControllerServiceConfigurationError("wrapper RustSec metadata is invalid");
  }
  const normalized = {
    role,
    paths,
    nodeBinary: validateAbsolutePath(configuration.nodeBinary, "Node binary"),
    repository,
    controllerScript,
    origin: validateOrigin(configuration.origin),
    uid: validatePositiveUid(configuration.uid),
    username: validateUsername(configuration.username),
    architecture: validateArchitecture(configuration.architecture),
    metadata,
  };
  if (role === "master") {
    normalized.webOrigin = validateOrigin(configuration.webOrigin, "web origin");
    normalized.rustSecRepository = validateAbsolutePath(
      configuration.rustSecRepository,
      "RustSec repository",
    );
    normalized.cloudflareAccountId = validateCloudflareAccountId(
      configuration.cloudflareAccountId,
    );
    if (configuration.prPrep != null || configuration.prepUsername != null) {
      throw new ControllerServiceConfigurationError(
        "master wrapper refuses PR preparation configuration",
      );
    }
  } else if (configuration.prPrep == null) {
    throw new ControllerServiceConfigurationError(
      "PR wrapper requires a validated preparation identity",
    );
  } else if (
    configuration.rustSecRepository != null ||
    configuration.cloudflareAccountId != null ||
    configuration.webOrigin != null
  ) {
    throw new ControllerServiceConfigurationError(
      "PR wrapper refuses master-only Cloudflare and RustSec configuration",
    );
  } else {
    normalized.prPrep = validateRecordedPrPrepIdentity(configuration.prPrep);
    if (
      (configuration.prepUsername != null &&
        validatePrPrepUsername(configuration.prepUsername) !== normalized.prPrep.username) ||
      normalized.prPrep.controllerUsername !== normalized.username ||
      normalized.prPrep.controllerUid !== normalized.uid ||
      normalized.prPrep.nodeBinary !== normalized.nodeBinary ||
      normalized.prPrep.nodeIdentity !== `node/darwin/${normalized.architecture}`
    ) {
      throw new ControllerServiceConfigurationError(
        "PR wrapper controller, preparation, or Node identity differs from its record",
      );
    }
  }
  return Object.freeze(normalized);
}

export function renderPrPrepRuntimeValidationProgram(identity) {
  const expected = validateRecordedPrPrepIdentity(identity);
  const parentPaths = ["/", "/Library", "/Library/PrivilegedHelperTools"];
  const nodeParentPaths = ancestorPaths(expected.nodeBinary);
  const homeParentPaths = PR_PREP_HOME_PARENT_DIRECTORIES;
  const helperProbe = `${JSON.stringify({
    credentialEnvironmentNames: [],
    freshHomePolicy: "per-build-private-temporary",
    gid: expected.gid,
    helperVersion: PR_PREP_HELPER_VERSION,
    uid: expected.uid,
    version: 1,
  })}\n`;
  const sudoProbeCommand = `${sudoersEscape(expected.nodeBinary)} ` +
    `${sudoersEscape(PR_PREP_HELPER_PATH)} --probe`;
  const sudoBuildCommand = `${sudoersEscape(expected.nodeBinary)} ` +
    `${sudoersEscape(PR_PREP_HELPER_PATH)} --build`;
  return `const fs=require("node:fs"),cp=require("node:child_process"),crypto=require("node:crypto");
const e=${JSON.stringify(expected)},parents=${JSON.stringify(parentPaths)},nodeParents=${JSON.stringify(nodeParentPaths)},homeParents=${JSON.stringify(homeParentPaths)},sharedGroups=${JSON.stringify(UNAVOIDABLE_MACOS_GROUPS)};
const fail=()=>process.exit(78),clean={HOME:"/var/empty",USER:e.controllerUsername,LOGNAME:e.controllerUsername,LANG:"C",LC_ALL:"C",PATH:"/usr/bin:/bin:/usr/sbin:/sbin"};
const active=new Set(),stops=new Map(),sleep=(ms)=>new Promise((resolve)=>setTimeout(resolve,ms));
const send=(pid,signal)=>{try{process.kill(-pid,signal);return true}catch(error){if(error&&error.code==="ESRCH")return false;throw error}};
const gone=async(pid,ms)=>{const deadline=Date.now()+ms;for(;;){if(!send(pid,0))return true;if(Date.now()>=deadline)return false;await sleep(25)}};
const stop=(pid)=>{if(!Number.isSafeInteger(pid)||pid<=0)return Promise.resolve();if(stops.has(pid))return stops.get(pid);const task=(async()=>{if(send(pid,"SIGTERM")&&!await gone(pid,${PROCESS_GROUP_TERM_GRACE_MS})){send(pid,"SIGKILL");if(!await gone(pid,${PROCESS_GROUP_REAP_TIMEOUT_MS}))throw new Error("process group survived SIGKILL")}})().finally(()=>{active.delete(pid);stops.delete(pid)});stops.set(pid,task);return task};
let aborting=false;const abort=()=>{if(aborting)return;aborting=true;Promise.all([...active].map(stop)).finally(()=>process.exit(78))};process.once("SIGINT",abort);process.once("SIGTERM",abort);process.once("exit",()=>{for(const pid of active)try{process.kill(-pid,"SIGKILL")}catch{}});
const run=(file,args,env=clean)=>new Promise((resolve,reject)=>{const child=cp.spawn(file,args,{cwd:"/",env,detached:true,stdio:["ignore","pipe","pipe"]});if(Number.isSafeInteger(child.pid)&&child.pid>0)active.add(child.pid);const stdout=[],stderr=[];let stdoutBytes=0,stderrBytes=0,status=null,signal=null,spawnError,overflow=false,timedOut=false,closed=false,cleaned=false,cleanupError,cleanupTask;const finish=()=>{if(!closed||!cleaned)return;clearTimeout(timer);if(spawnError||cleanupError)return reject(spawnError||cleanupError);if(overflow)return reject(new Error("probe output overflow"));resolve({status:status==null?1:status,signal,stdout:Buffer.concat(stdout).toString("utf8"),stderr:Buffer.concat(stderr).toString("utf8"),timedOut})};const cleanup=()=>{if(cleanupTask)return cleanupTask;cleanupTask=stop(child.pid).then(()=>{cleaned=true;finish()},(error)=>{cleanupError=error;cleaned=true;finish()});return cleanupTask};const collect=(chunks,name)=>(chunk)=>{if(name==="stdout")stdoutBytes+=chunk.length;else stderrBytes+=chunk.length;if(stdoutBytes>${MAX_PROBE_OUTPUT_BYTES}||stderrBytes>${MAX_PROBE_OUTPUT_BYTES}){overflow=true;cleanup();return}chunks.push(Buffer.from(chunk))};child.stdout.on("data",collect(stdout,"stdout"));child.stderr.on("data",collect(stderr,"stderr"));child.once("error",(error)=>{spawnError=error;cleanup()});child.once("exit",(code,exitSignal)=>{status=code;signal=exitSignal;cleanup()});child.once("close",(code,exitSignal)=>{if(status==null)status=code;if(signal==null)signal=exitSignal;closed=true;cleanup();finish()});const timer=setTimeout(()=>{timedOut=true;cleanup()},${PR_PREP_PROBE_TIMEOUT_MS})});
const quiet=(r)=>{if(r.timedOut||r.status!==0||r.signal||typeof r.stdout!=="string"||r.stderr!=="")fail();return r.stdout};
const aclFree=(text)=>{if(typeof text!=="string"||!text.endsWith("\\n")||text.includes("\\r")||text.includes("\\0"))fail();const body=text.slice(0,-1),mode=body.split(/\\s/,1)[0];if(body.includes("\\n")||mode.endsWith("+")||!/^[bcdlps-][rwxStTs-]{9}@?$/.test(mode))fail()};
const lines=(text)=>{if(typeof text!=="string"||!text.endsWith("\\n")||text.includes("\\r")||text.includes("\\0"))fail();const a=text.slice(0,-1).split("\\n");if(!a.length||a.some((line)=>!line))fail();return a};
const numeric=(text)=>{if(!/^(?:0|[1-9][0-9]*)(?: (?:0|[1-9][0-9]*))*\\n$/.test(text))fail();const a=text.slice(0,-1).split(" ").map(Number);if(a.some((n)=>!Number.isSafeInteger(n)||n<0)||new Set(a).size!==a.length)fail();return a.sort((a,b)=>a-b)};
const listing=(text)=>{const m=new Map();for(const l of lines(text)){const x=/^(\\S+)\\s+(-?(?:0|[1-9][0-9]*))$/.exec(l),n=Number(x&&x[2]);if(!x||!Number.isSafeInteger(n)||m.has(x[1]))fail();m.set(x[1],n)}return m};
const attrs=(text)=>{const m=new Map();for(const l of lines(text)){const x=/^([A-Za-z][A-Za-z0-9]*):(?: (.*))?$/.exec(l);if(!x||m.has(x[1]))fail();m.set(x[1],x[2]?x[2].split(/\\s+/):[])}return m};
const one=(m,k,v)=>{const a=m.get(k);if(!a||a.length!==1||(v!==undefined&&a[0]!==String(v)))fail();return a[0]};
const same=(a,b)=>["dev","ino","uid","gid","mode","nlink","size","mtimeMs","ctimeMs"].every((k)=>a[k]===b[k]);
const opened=(path,maximum,expected)=>{if(typeof fs.constants.O_NOFOLLOW!=="number")fail();const fd=fs.openSync(path,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW);try{const before=fs.fstatSync(fd);if(!before.isFile()||before.nlink!==1||(before.mode&0o7000)!==0||before.size<=0||before.size>maximum)fail();const bytes=Buffer.alloc(before.size);let at=0;while(at<bytes.length){const n=fs.readSync(fd,bytes,at,bytes.length-at,at);if(!n)break;at+=n}const extra=Buffer.alloc(1);if(at!==bytes.length||fs.readSync(fd,extra,0,1,before.size)!==0)fail();const after=fs.fstatSync(fd),pathStat=fs.lstatSync(path);if(!same(before,after)||!same(after,pathStat)||pathStat.isSymbolicLink()||fs.realpathSync(path)!==path)fail();const actual={device:after.dev,inode:after.ino,size:after.size,sha256:crypto.createHash("sha256").update(bytes).digest("hex"),uid:after.uid,gid:after.gid,mode:after.mode&511,nlink:after.nlink};for(const [k,v] of Object.entries(expected))if(actual[k]!==v)fail();return actual}finally{fs.closeSync(fd)}};
const sudoPolicy=(text)=>{if(!text.endsWith("\\n")||text.includes("\\r")||[...text].some((c)=>{const n=c.charCodeAt(0);return n!==9&&n!==10&&(n<32||n>126)}))fail();const lines=text.slice(0,-1).split("\\n"),dp="Matching Defaults entries for "+e.controllerUsername+" on ",gp="User "+e.controllerUsername+" may run the following commands on ";const di=lines.flatMap((l,i)=>l.startsWith(dp)&&l.endsWith(":")?[i]:[]),gi=lines.flatMap((l,i)=>l.startsWith(gp)&&l.endsWith(":")?[i]:[]);if(di.length!==1||gi.length!==1||di[0]!==0||gi[0]<=1)fail();const defaults=lines.slice(1,gi[0]).filter((l)=>l.trim()).join(" ").replace(/\\s+/g," "),timeouts=[...defaults.matchAll(/(?:^|,\\s*|\\s+)timestamp_timeout\\s*=\\s*([^,\\s]+)/g)].map((m)=>m[1]);if(timeouts.length!==1||timeouts[0]!=="0")fail();const entries=[];for(const raw of lines.slice(gi[0]+1).filter((l)=>l.trim())){const line=raw.trim().replace(/\\s+/g," ");if(line.startsWith("("))entries.push(line);else if(entries.length)entries[entries.length-1]+=" "+line;else fail()}const p=${JSON.stringify(sudoProbeCommand)},b=${JSON.stringify(sudoBuildCommand)},tag=(c)=>new Set(["("+e.username+") NOPASSWD: NOSETENV: "+c,"("+e.username+") NOSETENV: NOPASSWD: "+c]),combined=tag(p+", "+b),sp=tag(p),sb=tag(b),ok=entries.length===1&&combined.has(entries[0])||entries.length===2&&entries.some((x)=>sp.has(x))&&entries.some((x)=>sb.has(x));if(!ok)fail()};
;(async()=>{try{
const p=quiet(await run(${JSON.stringify(ID)},["-P",e.username]));if(!p.endsWith("\\n")||p.includes("\\r")||p.slice(0,-1).includes("\\n"))fail();const line=p.slice(0,-1),f=line.split(":");if(f.length!==10||f[0]!==e.username||f[1]===""||Number(f[2])!==e.uid||Number(f[3])!==e.gid||f[8]!==e.homeDirectory||f[9]!==e.shell)fail();
const prep=numeric(quiet(await run(${JSON.stringify(ID)},["-G",e.username]))),controller=numeric(quiet(await run(${JSON.stringify(ID)},["-G",e.controllerUsername]))),ep=[...e.supplementaryGids].sort((a,b)=>a-b),ec=[...e.controllerGids].sort((a,b)=>a-b);if(JSON.stringify(prep)!==JSON.stringify(ep)||JSON.stringify(controller)!==JSON.stringify(ec))fail();
const uids=listing(quiet(await run(${JSON.stringify(DSCL)},["/Local/Default","-list","/Users","UniqueID"]))),gids=listing(quiet(await run(${JSON.stringify(DSCL)},["/Local/Default","-list","/Users","PrimaryGroupID"]))),groups=listing(quiet(await run(${JSON.stringify(DSCL)},["/Local/Default","-list","/Groups","PrimaryGroupID"])));if(uids.get(e.username)!==e.uid||gids.get(e.username)!==e.gid||[...uids.values()].filter((v)=>v===e.uid).length!==1||[...gids.values()].filter((v)=>v===e.gid).length!==1||groups.get(e.primaryGroupName)!==e.gid||[...groups.values()].filter((v)=>v===e.gid).length!==1)fail();for(const g of sharedGroups)if(prep.includes(g.gid)){if(groups.get(g.name)!==g.gid||[...groups.values()].filter((v)=>v===g.gid).length!==1)fail();const sr=attrs(quiet(await run(${JSON.stringify(DSCL)},["/Local/Default","-read","/Groups/"+g.name,"RecordName","PrimaryGroupID","GeneratedUID"]))),names=sr.get("RecordName")||[];if(names.length!==g.recordNames.length||new Set(names).size!==names.length||names.some((v)=>!g.recordNames.includes(v)))fail();one(sr,"PrimaryGroupID",g.gid);one(sr,"GeneratedUID",g.generatedUid)}
const ur=attrs(quiet(await run(${JSON.stringify(DSCL)},["/Local/Default","-read","/Users/"+e.username,"RecordName","UniqueID","PrimaryGroupID","NFSHomeDirectory","UserShell","GeneratedUID","AuthenticationAuthority"])));if(ur.has("AuthenticationAuthority"))fail();one(ur,"RecordName",e.username);one(ur,"UniqueID",e.uid);one(ur,"PrimaryGroupID",e.gid);one(ur,"NFSHomeDirectory",e.homeDirectory);one(ur,"UserShell",e.shell);one(ur,"GeneratedUID",e.generatedUid);
const gr=attrs(quiet(await run(${JSON.stringify(DSCL)},["/Local/Default","-read","/Groups/"+e.primaryGroupName,"RecordName","PrimaryGroupID","GeneratedUID","GroupMembership","GroupMembers","NestedGroups"])));one(gr,"RecordName",e.primaryGroupName);one(gr,"PrimaryGroupID",e.gid);one(gr,"GeneratedUID",e.primaryGroupGeneratedUid);if((gr.get("GroupMembership")||[]).some((v)=>v!==e.username)||(gr.get("GroupMembers")||[]).some((v)=>v!==e.generatedUid)||(gr.get("NestedGroups")||[]).length)fail();
for(const path of homeParents){const s=fs.lstatSync(path),mode=s.mode&511;if(s.isSymbolicLink()||!s.isDirectory()||s.uid!==0||(s.mode&0o7000)!==0||(mode&0o022)!==0||fs.realpathSync(path)!==path)fail()}const hs=fs.lstatSync(e.homeDirectory),hm=hs.mode&511;if(hs.isSymbolicLink()||!hs.isDirectory()||hs.uid!==0||(hs.mode&0o7000)!==0||(hm&0o022)!==0||fs.realpathSync(e.homeDirectory)!==${JSON.stringify(PR_PREP_CANONICAL_HOME_DIRECTORY)})fail();
for(const path of parents){const s=fs.lstatSync(path),mode=s.mode&511;if(s.isSymbolicLink()||!s.isDirectory()||s.uid!==0||s.gid!==0||(s.mode&0o7000)!==0||(mode&0o022)!==0||fs.realpathSync(path)!==path)fail()}
for(const path of nodeParents){const s=fs.lstatSync(path),mode=s.mode&511;if(s.isSymbolicLink()||!s.isDirectory()||s.uid!==0||s.gid!==0||(s.mode&0o7000)!==0||(mode&0o022)!==0||(mode&1)===0||fs.realpathSync(path)!==path)fail();aclFree(quiet(await run("/bin/ls",["-lde",path])))}
const nodeExpected={device:e.node.device,inode:e.node.inode,size:e.node.size,sha256:e.node.sha256,uid:0,gid:0,mode:e.node.mode,nlink:1};aclFree(quiet(await run("/bin/ls",["-lde",e.nodeBinary])));opened(e.nodeBinary,${PR_PREP_NODE_MAX_BYTES},nodeExpected);
opened(${JSON.stringify(PR_PREP_HELPER_PATH)},${PR_PREP_HELPER_MAX_BYTES},{device:e.helper.device,inode:e.helper.inode,size:e.helper.size,sha256:e.helper.sha256,uid:0,gid:0,mode:e.helper.mode,nlink:1});opened(${JSON.stringify(PR_PREP_CARGO_PATH)},${PR_PREP_CARGO_MAX_BYTES},{device:e.cargo.device,inode:e.cargo.inode,size:e.cargo.size,sha256:e.cargo.sha256,uid:0,gid:0,mode:e.cargo.mode,nlink:1});
const cv=await run(${JSON.stringify(PR_PREP_CARGO_PATH)},["--version","--verbose"],{HOME:"/var/empty",CARGO_HOME:"/var/empty",RUSTUP_HOME:"/var/empty",PATH:"/usr/bin:/bin",LANG:"C",LC_ALL:"C"});if(quiet(cv)!==e.cargo.versionOutput)fail();sudoPolicy(quiet(await run(${JSON.stringify(SUDO)},["-n","-l"])));
const hp=await run(${JSON.stringify(SUDO)},["-n","-u",e.username,"--",e.nodeBinary,${JSON.stringify(PR_PREP_HELPER_PATH)},"--probe"],{});if(quiet(hp)!==${JSON.stringify(helperProbe)})fail();
aclFree(quiet(await run("/bin/ls",["-lde",e.nodeBinary])));opened(e.nodeBinary,${PR_PREP_NODE_MAX_BYTES},nodeExpected);
const node=[process.release.name,process.platform,process.arch].join("/");if(process.execPath!==e.nodeBinary||node!==e.nodeIdentity)fail();
}catch{fail()}})()`;
}

function renderPrPrepProbeValidationProgram(identity) {
  const expectedIdentity = validateRecordedPrPrepIdentity(identity);
  const expected = {
    credentialEnvironmentNames: [],
    freshHomePolicy: "per-build-private-temporary",
    gid: expectedIdentity.gid,
    helperVersion: PR_PREP_HELPER_VERSION,
    uid: expectedIdentity.uid,
    version: 1,
  };
  return `const fs=require("node:fs");const input=fs.readFileSync(0,"utf8");` +
    `const expected=${JSON.stringify(JSON.stringify(expected))};` +
    `if(input.length>4096||input!==expected)process.exit(78);`;
}

function renderPreflightProbeProgram({
  role,
  architecture,
  nodeBinary,
  repository,
  controllerScript,
  fixedEnvironment,
  prPrep,
}) {
  const denied = AMBIENT_AUTHORITY_KEYS;
  return `const fixed=${JSON.stringify(fixedEnvironment)};const denied=${JSON.stringify(denied)};` +
    `const own=(n)=>Object.prototype.hasOwnProperty.call(process.env,n);` +
    `const credentialKeys=Object.keys(process.env).filter((n)=>/(?:TOKEN|SECRET|PASSWORD|API_KEY|PRIVATE_KEY)/i.test(n)).sort();` +
    `const deniedKeys=denied.filter(own);const wrong=Object.entries(fixed).filter(([n,v])=>process.env[n]!==v);` +
    `const unexpected=Object.keys(process.env).filter((n)=>!Object.prototype.hasOwnProperty.call(fixed,n));` +
    `const value={version:1,role:${JSON.stringify(role)},identity:[process.release.name,process.platform,process.arch].join("/"),` +
    `nodeBinary:process.execPath,controllerScript:${JSON.stringify(controllerScript)},fixed,credentialKeys,deniedKeys` +
    `${role === "pr" ? `,prPrep:${JSON.stringify(prPrep)},sudoProbe:true` : ""}};` +
    `if(value.identity!==${JSON.stringify(`node/darwin/${architecture}`)}||process.execPath!==${JSON.stringify(nodeBinary)}||wrong.length` +
    `${role === "pr" ? "||unexpected.length" : ""}||credentialKeys.length||deniedKeys.length){` +
    `process.stderr.write("controller preflight environment failed allowlist validation\\n");process.exit(78);}` +
    `if(fixed.NANOCODEX_REPO!==${JSON.stringify(repository)})process.exit(78);` +
    `process.stdout.write(JSON.stringify(value));`;
}

function runtimeContext() {
  if (typeof process.getuid !== "function" || typeof process.geteuid !== "function") {
    throw new ControllerServiceConfigurationError("numeric Unix user identity is unavailable");
  }
  const identity = userInfo();
  return validateHost({
    platform: platform(),
    arch: arch(),
    uid: process.getuid(),
    euid: process.geteuid(),
    identityUid: identity.uid,
    homeDirectory: identity.homedir,
    username: identity.username,
  });
}

function roleDefinition(role) {
  return ROLE_DEFINITIONS[validateRole(role)];
}

function validateRoleSecret(role, secretName) {
  validateRole(role);
  if (!ROLE_SECRET_ALLOWLISTS[role].includes(secretName)) {
    throw new ControllerServiceConfigurationError(
      `${String(secretName)} is not in the ${role} controller Keychain allowlist`,
    );
  }
  return secretName;
}

export function keychainAccountName(role, secretName) {
  validateRoleSecret(role, secretName);
  const account = ROLE_KEYCHAIN_ACCOUNTS[role][secretName];
  if (typeof account !== "string" || account === "") {
    throw new ControllerServiceConfigurationError(
      `${String(secretName)} has no ${role} controller Keychain account mapping`,
    );
  }
  return account;
}

function validateGuiUid(uid, role) {
  validateRole(role);
  if (!Number.isSafeInteger(uid) || uid <= 0 || uid > 2_147_483_647) {
    throw new ControllerServiceConfigurationError(
      "launchctl requires a non-root numeric user ID",
    );
  }
  const domain = `gui/${uid}`;
  return Object.freeze({
    domain,
    service: `${domain}/${ROLE_DEFINITIONS[role].label}`,
  });
}

function parseRoleOnly(arguments_, command) {
  if (
    arguments_.length !== 2 ||
    arguments_[0] !== "--role" ||
    arguments_[1]?.startsWith("--")
  ) {
    throw new ControllerServiceConfigurationError(`${command} requires --role <master|pr>`);
  }
  return validateRole(arguments_[1]);
}

function validatePositiveUid(value) {
  if (!Number.isSafeInteger(value) || value <= 0 || value > 2_147_483_647) {
    throw new ControllerServiceConfigurationError("wrapper UID is invalid");
  }
  return value;
}

function validateNonnegativeIdentifier(value, name) {
  if (!Number.isSafeInteger(value) || value < 0 || value > 2_147_483_647) {
    throw new ControllerServiceConfigurationError(`${name} is invalid`);
  }
  return value;
}

function parseBoundedIdentifier(value, name) {
  if (typeof value !== "string" || !/^(?:0|[1-9][0-9]*)$/.test(value)) {
    throw new ControllerServiceConfigurationError(`${name} is invalid`);
  }
  return validateNonnegativeIdentifier(Number(value), name);
}

function parseSignedIdentifier(value, name) {
  if (typeof value !== "string" || !/^-?(?:0|[1-9][0-9]*)$/.test(value)) {
    throw new ControllerServiceConfigurationError(`${name} is invalid`);
  }
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < -2_147_483_648 || number > 2_147_483_647) {
    throw new ControllerServiceConfigurationError(`${name} is invalid`);
  }
  return number;
}

function validUnixMode(value) {
  return Number.isSafeInteger(value) && value >= 0 && value <= 0o777;
}

function validateRecordedGroupArray(value, name) {
  if (!Array.isArray(value) || value.length === 0) {
    throw new ControllerServiceConfigurationError(`${name} is invalid`);
  }
  const groups = value.map((entry) => validateNonnegativeIdentifier(entry, name));
  if (new Set(groups).size !== groups.length) {
    throw new ControllerServiceConfigurationError(`${name} contains duplicate IDs`);
  }
  return Object.freeze([...groups]);
}

function validateUsername(value) {
  if (
    typeof value !== "string" ||
    value === "" ||
    value.length > 255 ||
    /[\u0000-\u001f\u007f]/.test(value)
  ) {
    throw new ControllerServiceConfigurationError("wrapper username is invalid");
  }
  return value;
}

function validateArchitecture(value) {
  if (value !== "arm64" && value !== "x64") {
    throw new ControllerServiceConfigurationError("wrapper architecture is invalid");
  }
  return value;
}

function validRecordedMetadata(value) {
  return value != null &&
    typeof value === "object" &&
    Number.isSafeInteger(value.uid) &&
    value.uid >= 0 &&
    Number.isSafeInteger(value.mode) &&
    value.mode >= 0 &&
    value.mode <= 0o777;
}

function isStringRecord(value) {
  return value != null &&
    typeof value === "object" &&
    !Array.isArray(value) &&
    Object.values(value).every((entry) => typeof entry === "string");
}

function assertDescendant(path, parent, name) {
  const suffix = relative(parent, path);
  if (suffix === "" || suffix === ".." || suffix.startsWith("../") || isAbsolute(suffix)) {
    throw new ControllerServiceConfigurationError(`${name} must be contained by ${parent}`);
  }
}

function ancestorPaths(path) {
  const checked = validateAbsolutePath(path, "ancestor path");
  const parents = [];
  let current = dirname(checked);
  while (true) {
    parents.push(current);
    if (current === "/") break;
    const parent = dirname(current);
    if (parent === current) {
      throw new ControllerServiceConfigurationError("absolute path ancestor chain is invalid");
    }
    current = parent;
  }
  return Object.freeze(parents.reverse());
}

function pathContains(parent, candidate) {
  const suffix = relative(parent, candidate);
  return suffix === "" || (suffix !== ".." && !suffix.startsWith("../") && !isAbsolute(suffix));
}

function assertExactServiceDirectory(path, paths, role) {
  validateServicePaths(paths, role);
  if (path !== paths.serviceDirectory) {
    throw new ControllerServiceConfigurationError(
      "recursive removal is restricted to the exact role-scoped service directory",
    );
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
  return `'${String(value).replaceAll("'", `'"'"'`)}'`;
}

function sudoersEscape(value) {
  const escaped = new Set([
    "\\", ",", ":", "=", "#", " ", "\t", "*", "?", "[", "]", "!", '"',
  ]);
  return [...String(value)].map((character) =>
    escaped.has(character) ? `\\${character}` : character).join("");
}

async function validateExistingPathChain(paths, role, uid) {
  validateServicePaths(paths, role);
  await assertOptionalDirectory(paths.homeDirectory, uid, { required: true, privateMode: null });
  for (const directory of [
    paths.libraryDirectory,
    paths.applicationSupportDirectory,
    paths.launchAgentsDirectory,
  ]) {
    await assertOptionalDirectory(directory, uid, { required: false, privateMode: null });
  }
  for (const directory of [
    paths.vendorDirectory,
    paths.controllersDirectory,
    paths.serviceDirectory,
    paths.logsDirectory,
    paths.temporaryDirectory,
  ]) {
    await assertOptionalDirectory(directory, uid, { required: false, privateMode: 0o700 });
  }
}

async function ensureServiceDirectories(paths, role, uid) {
  validateServicePaths(paths, role);
  await assertOptionalDirectory(paths.homeDirectory, uid, { required: true, privateMode: null });
  await ensureDirectory(paths.libraryDirectory, uid, null);
  await ensureDirectory(paths.applicationSupportDirectory, uid, null);
  await ensureDirectory(paths.vendorDirectory, uid, 0o700);
  await ensureDirectory(paths.controllersDirectory, uid, 0o700);
  await ensureDirectory(paths.serviceDirectory, uid, 0o700);
  await ensureDirectory(paths.logsDirectory, uid, 0o700);
  await ensureDirectory(paths.temporaryDirectory, uid, 0o700);
  await ensureDirectory(paths.launchAgentsDirectory, uid, null);
  for (const path of [paths.stdoutPath, paths.stderrPath]) {
    if (!(await pathMetadata(path))) {
      const handle = await open(path, "wx", 0o600);
      await handle.close();
      await chmod(path, 0o600);
    }
  }
  await assertSafeLogAndStateFiles(paths, role, uid, true);
}

async function ensureDirectory(path, uid, privateMode) {
  let created = false;
  try {
    await mkdir(path, { mode: privateMode ?? 0o700 });
    created = true;
  } catch (cause) {
    if (cause?.code !== "EEXIST") throw cause;
  }
  if (created) await chmod(path, privateMode ?? 0o700);
  await assertOptionalDirectory(path, uid, { required: true, privateMode });
}

async function assertOptionalDirectory(path, uid, { required, privateMode }) {
  const metadata = await pathMetadata(path);
  if (!metadata) {
    if (required) throw new ControllerServiceConfigurationError(`${path} does not exist`);
    return;
  }
  await assertSafeOwnedEntry(path, {
    uid,
    kind: "directory",
    exactMode: privateMode ?? undefined,
  });
}

async function installationArtifacts(paths) {
  const [plist, wrapper] = await Promise.all([
    pathMetadata(paths.plistPath),
    pathMetadata(paths.wrapperPath),
  ]);
  return Object.freeze({ plist, wrapper });
}

async function assertSafeArtifacts(artifacts, uid, requireInstalledModes) {
  const modes = Object.freeze({ plist: 0o600, wrapper: 0o700 });
  const maximumSizes = Object.freeze({ plist: MAX_PLIST_BYTES, wrapper: MAX_WRAPPER_BYTES });
  for (const [name, metadata] of Object.entries(artifacts)) {
    if (!metadata) continue;
    if (
      !metadata.isFile() ||
      metadata.isSymbolicLink() ||
      metadata.uid !== uid ||
      metadata.nlink !== 1 ||
      (metadata.mode & 0o022) !== 0 ||
      metadata.size <= 0 ||
      metadata.size > maximumSizes[name]
    ) {
      throw new ControllerServiceConfigurationError(
        `existing ${name} artifact is not a safe singly linked user-owned file`,
      );
    }
    if (requireInstalledModes && (metadata.mode & 0o777) !== modes[name]) {
      throw new ControllerServiceConfigurationError(
        `existing ${name} artifact has unsafe mode ${(metadata.mode & 0o777).toString(8)}`,
      );
    }
  }
}

async function assertSafeLogAndStateFiles(paths, role, uid, required) {
  const candidates = [
    paths.stdoutPath,
    paths.stderrPath,
    paths.stdoutArchivePath,
    paths.stderrArchivePath,
    ...(role === "pr" ? [paths.statePath, paths.stateLockPath] : []),
  ];
  for (const path of candidates) {
    const metadata = await pathMetadata(path);
    if (!metadata) {
      if (required && (path === paths.stdoutPath || path === paths.stderrPath)) {
        throw new ControllerServiceConfigurationError(`${path} does not exist`);
      }
      continue;
    }
    await assertSafeOwnedEntry(path, {
      uid,
      kind: "file",
      exactMode: 0o600,
    });
  }
}

async function assertSafeRemovalTree(root, uid) {
  const rootMetadata = await pathMetadata(root);
  if (!rootMetadata) return;
  await assertSafeOwnedEntry(root, { uid, kind: "directory", exactMode: 0o700 });
  let entries = 0;
  const visit = async (directory) => {
    for (const name of await readdir(directory)) {
      entries += 1;
      if (entries > MAX_REMOVAL_ENTRIES) {
        throw new ControllerServiceConfigurationError(
          "service data tree is too large for safe recursive removal",
        );
      }
      const path = join(directory, name);
      const metadata = await lstat(path);
      if (metadata.isSymbolicLink() || metadata.uid !== uid) {
        throw new ControllerServiceConfigurationError(
          "service data removal refuses links or entries owned by another user",
        );
      }
      if (metadata.isDirectory()) {
        await visit(path);
      } else if (!metadata.isFile() || metadata.nlink !== 1) {
        throw new ControllerServiceConfigurationError(
          "service data removal accepts only real singly linked files and directories",
        );
      }
    }
  };
  await visit(root);
}

async function assertSelectedNode(nodeBinary, context) {
  const result = await runOutputProbe(nodeBinary, [
    "--eval",
    "process.stdout.write([process.release.name,process.platform,process.arch].join('/'))",
  ], {
    env: sanitizedInstallerEnvironment(context),
    timeoutMs: 5_000,
  });
  if (result.timedOut) {
    throw new ControllerServiceConfigurationError("selected Node identity probe timed out");
  }
  if (result.code !== 0) {
    throw new ControllerServiceConfigurationError(
      `selected Node identity probe failed with exit ${result.code}${probeDetail(result.stderr)}`,
    );
  }
  validateNodeIdentity(result.stdout, context.architecture);
}

async function writeTemporaryFile(destination, contents, mode) {
  validateAbsolutePath(destination, "write destination");
  if (typeof contents !== "string") throw new TypeError("file contents must be text");
  const temporary = temporarySibling(destination);
  let handle;
  try {
    handle = await open(temporary, "wx", mode);
    await handle.writeFile(contents, "utf8");
    await handle.sync();
    await handle.close();
    handle = undefined;
    await chmod(temporary, mode);
    return temporary;
  } catch (cause) {
    await handle?.close().catch(() => undefined);
    await rm(temporary, { force: true });
    throw cause;
  }
}

async function readBoundedTextFile(path, maximumBytes, uid) {
  const noFollow = fsConstants.O_NOFOLLOW;
  if (typeof noFollow !== "number") {
    throw new ControllerServiceConfigurationError("safe status reads require O_NOFOLLOW");
  }
  const handle = await open(path, fsConstants.O_RDONLY | noFollow);
  try {
    const metadata = await handle.stat();
    if (
      !metadata.isFile() ||
      metadata.uid !== uid ||
      metadata.nlink !== 1 ||
      metadata.size <= 0 ||
      metadata.size > maximumBytes
    ) {
      throw new ControllerServiceConfigurationError("installed text artifact is unsafe or oversized");
    }
    return await handle.readFile("utf8");
  } finally {
    await handle.close();
  }
}

export async function captureTrustedPrPrepHelperSource(path, {
  expectedUid = process.getuid?.(),
  trustedSha256 = PR_PREP_HELPER_SHA256,
  afterOpen,
} = {}) {
  const checkedUid = validatePositiveUid(expectedUid);
  const pinned = validateSha256(trustedSha256, "trusted PR helper SHA-256");
  const snapshot = await readBoundedBinarySnapshot(path, PR_PREP_HELPER_MAX_BYTES, {
    afterOpen,
    expectedUid: checkedUid,
    requireSafeMode: true,
  });
  if (
    snapshot.metadata.uid !== checkedUid || snapshot.metadata.nlink !== 1 ||
    (snapshot.metadata.mode & 0o7022) !== 0 || snapshot.sha256 !== pinned
  ) {
    throw new ControllerServiceConfigurationError(
      "trusted PR helper source must be the pinned bounded singly linked controller-owned bytes",
    );
  }
  return Object.freeze({
    path: snapshot.canonicalPath,
    size: snapshot.bytes.length,
    sha256: snapshot.sha256,
    bytes: Buffer.from(snapshot.bytes),
    identity: Object.freeze(fileIdentity(snapshot.metadata)),
  });
}

export async function captureTrustedPrPrepNodeExecutable(path, {
  expectedUid = 0,
  expectedGid = 0,
  afterOpen,
} = {}) {
  const uid = validateNonnegativeIdentifier(expectedUid, "trusted Node owner UID");
  const gid = validateNonnegativeIdentifier(expectedGid, "trusted Node owner GID");
  const snapshot = await readBoundedBinarySnapshot(path, PR_PREP_NODE_MAX_BYTES, {
    afterOpen,
    expectedUid: uid,
    expectedGid: gid,
    requireSafeMode: true,
  });
  return filesystemSnapshot(path, snapshot);
}

async function readBoundedBinarySnapshot(path, maximumBytes, {
  afterOpen,
  expectedUid,
  expectedGid,
  requireSafeMode = false,
} = {}) {
  const checked = validateAbsolutePath(path, "bounded binary path");
  const noFollow = fsConstants.O_NOFOLLOW;
  if (typeof noFollow !== "number") {
    throw new ControllerServiceConfigurationError("safe helper reads require O_NOFOLLOW");
  }
  const handle = await open(checked, fsConstants.O_RDONLY | noFollow);
  try {
    const metadata = await handle.stat();
    if (
      !metadata.isFile() ||
      metadata.nlink !== 1 ||
      metadata.size <= 0 ||
      metadata.size > maximumBytes ||
      (expectedUid != null && metadata.uid !== expectedUid) ||
      (expectedGid != null && metadata.gid !== expectedGid) ||
      (requireSafeMode && (metadata.mode & 0o7022) !== 0)
    ) {
      throw new ControllerServiceConfigurationError(
        `${checked} changed identity or exceeded its size/link bound while read`,
      );
    }
    await afterOpen?.(Object.freeze({
      path: checked,
      fd: handle.fd,
      identity: Object.freeze(fileIdentity(metadata)),
    }));
    const bytes = Buffer.alloc(metadata.size);
    let offset = 0;
    while (offset < bytes.length) {
      const { bytesRead } = await handle.read(
        bytes,
        offset,
        Math.min(64 * 1024, bytes.length - offset),
        offset,
      );
      if (bytesRead === 0) break;
      offset += bytesRead;
    }
    const overflow = Buffer.alloc(1);
    const { bytesRead: overflowBytes } = await handle.read(overflow, 0, 1, metadata.size);
    if (offset !== metadata.size || overflowBytes !== 0) {
      throw new ControllerServiceConfigurationError(`${checked} changed size while read`);
    }
    const after = await handle.stat();
    if (!sameFileIdentity(metadata, after)) {
      throw new ControllerServiceConfigurationError(`${checked} changed identity while read`);
    }
    const pathEntry = await lstat(checked);
    if (!sameFileIdentity(after, pathEntry) || pathEntry.isSymbolicLink()) {
      throw new ControllerServiceConfigurationError(`${checked} path changed while read`);
    }
    const canonicalPath = validateAbsolutePath(
      await realpath(checked),
      `${checked} canonical path`,
    );
    if (canonicalPath !== checked) {
      throw new ControllerServiceConfigurationError(`${checked} contains a symbolic-link path`);
    }
    return Object.freeze({
      metadata: after,
      canonicalPath,
      sha256: createHash("sha256").update(bytes).digest("hex"),
      bytes,
    });
  } finally {
    await handle.close();
  }
}

function fileIdentity(metadata) {
  return {
    dev: metadata.dev,
    ino: metadata.ino,
    uid: metadata.uid,
    gid: metadata.gid,
    mode: metadata.mode,
    nlink: metadata.nlink,
    size: metadata.size,
    mtimeMs: metadata.mtimeMs,
    ctimeMs: metadata.ctimeMs,
  };
}

function sameFileIdentity(left, right) {
  const a = fileIdentity(left);
  const b = fileIdentity(right);
  return Object.keys(a).every((key) => a[key] === b[key]);
}

export function validateNoAclListing(value) {
  if (
    typeof value !== "string" || value === "" || value.includes("\0") ||
    value.includes("\r") || !value.endsWith("\n") || value.length > MAX_PROBE_OUTPUT_BYTES
  ) {
    throw new ControllerServiceConfigurationError("filesystem ACL listing is noncanonical");
  }
  const lines = value.slice(0, -1).split("\n");
  const mode = /^(\S+)\s/.exec(lines[0] ?? "")?.[1];
  if (
    lines.length !== 1 ||
    !/^[bcdlps-][rwxStTs-]{9}@?$/.test(mode ?? "") ||
    mode.includes("+")
  ) {
    throw new ControllerServiceConfigurationError(
      "filesystem entry has an ACL or ambiguous access metadata",
    );
  }
  return true;
}

async function assertNoAccessControlList(path, context) {
  const result = await runProbe("/bin/ls", ["-lde", path], context);
  if (result.code !== 0 || result.signal != null || result.stderr !== "") {
    throw new ControllerServiceConfigurationError(
      `${path} ACL metadata is missing, noisy, or unreadable${probeDetail(result.stderr)}`,
    );
  }
  validateNoAclListing(result.stdout);
}

async function snapshotPrPrepNode(nodeBinary, context, { afterOpen } = {}) {
  const nodePath = validateAbsolutePath(nodeBinary, "PR preparation Node binary");
  const parentPaths = ancestorPaths(nodePath);
  const parentEntries = [];
  // Establish trust from / downwards. Once a parent is proven root-owned and
  // non-writable, the controller and preparation identities cannot swap the
  // next pathname while it is inspected.
  for (const path of parentPaths) {
    await assertNoAccessControlList(path, context);
    parentEntries.push(Object.freeze({
      ...(await filesystemSnapshot(path)),
      accessControlList: false,
    }));
  }
  await assertNoAccessControlList(nodePath, context);
  const nodeEntry = Object.freeze({
    ...(await captureTrustedPrPrepNodeExecutable(nodePath, { afterOpen })),
    accessControlList: false,
  });
  return Object.freeze({
    parents: Object.freeze(parentEntries),
    node: nodeEntry,
  });
}

async function inspectPrPrepNode(nodeBinary, context, { expected } = {}) {
  const beforeSnapshot = await snapshotPrPrepNode(nodeBinary, context);
  const before = validatePrPrepNodeSnapshot(beforeSnapshot, nodeBinary);
  if (expected != null && JSON.stringify(before) !== JSON.stringify(expected)) {
    throw new ControllerServiceConfigurationError(
      "PR preparation Node path, bytes, or filesystem identity drifted",
    );
  }
  await assertSelectedNode(nodeBinary, context);
  const after = validatePrPrepNodeSnapshot(
    await snapshotPrPrepNode(nodeBinary, context),
    nodeBinary,
  );
  if (JSON.stringify(after) !== JSON.stringify(before)) {
    throw new ControllerServiceConfigurationError(
      "PR preparation Node path changed across its executable identity probe",
    );
  }
  return after;
}

async function filesystemSnapshot(path, openedSnapshot) {
  const checked = validateAbsolutePath(path);
  const metadata = openedSnapshot?.metadata ?? await lstat(checked);
  const canonicalPath = openedSnapshot?.canonicalPath ?? validateAbsolutePath(
    await realpath(checked),
    `${checked} canonical path`,
  );
  return Object.freeze({
    path: checked,
    canonicalPath,
    kind: metadata.isFile() ? "file" : metadata.isDirectory() ? "directory" : "other",
    symbolicLink: metadata.isSymbolicLink(),
    uid: metadata.uid,
    gid: metadata.gid,
    mode: metadata.mode & 0o777,
    specialMode: metadata.mode & 0o7000,
    nlink: metadata.nlink,
    inode: metadata.ino,
    device: metadata.dev,
    size: metadata.size,
    ...(openedSnapshot ? { sha256: openedSnapshot.sha256 } : {}),
  });
}

async function atomicWriteFile(destination, contents, mode) {
  const temporary = await writeTemporaryFile(destination, contents, mode);
  try {
    await rename(temporary, destination);
  } finally {
    await rm(temporary, { force: true });
  }
}

function temporarySibling(destination) {
  return join(
    dirname(destination),
    `.${basename(destination)}.${process.pid}.${randomUUID()}.tmp`,
  );
}

async function assertNoColocatedRole(role, context) {
  const opposite = role === "master" ? "pr" : "master";
  const oppositePaths = deriveServicePaths(context.homeDirectory, opposite);
  const macosRunnerServiceDirectory = join(
    context.homeDirectory,
    "Library",
    "Application Support",
    "nanocodex",
    "ci-macos-runner",
  );
  const macosRunnerPlistPath = join(
    context.homeDirectory,
    "Library",
    "LaunchAgents",
    `${MACOS_RUNNER_LABEL}.plist`,
  );
  await validateExistingPathChain(oppositePaths, opposite, context.uid);
  const [
    launchStatus,
    artifacts,
    oppositeState,
    keychainItems,
    macosRunnerLaunchResult,
    macosRunnerPlist,
    macosRunnerState,
    macosRunnerKeychainResult,
  ] = await Promise.all([
    serviceLaunchStatus(context.uid, opposite, oppositePaths),
    installationArtifacts(oppositePaths),
    pathMetadata(oppositePaths.serviceDirectory),
    Promise.all(
      ROLE_SECRET_ALLOWLISTS[opposite].map((name) =>
        keychainItemExists(opposite, name, context)),
    ),
    runProbe(
      LAUNCHCTL,
      ["print", `gui/${context.uid}/${MACOS_RUNNER_LABEL}`],
      runtimeContextForChild(context.uid),
    ),
    pathMetadata(macosRunnerPlistPath),
    pathMetadata(macosRunnerServiceDirectory),
    runProbe(
      SECURITY,
      [
        "find-generic-password",
        "-a",
        MACOS_RUNNER_KEYCHAIN_ACCOUNT,
        "-s",
        MACOS_RUNNER_KEYCHAIN_SERVICE,
      ],
      context,
    ),
  ]);
  return assertNoColocatedRoleState(role, {
    loaded: launchStatus.loaded,
    plist: Boolean(artifacts.plist),
    wrapper: Boolean(artifacts.wrapper),
    state: oppositeState != null,
    keychainItems,
    macosRunner: Object.freeze({
      loaded: parseLaunchctlPrint(macosRunnerLaunchResult).loaded,
      plist: macosRunnerPlist != null,
      state: macosRunnerState != null,
      keychainItem: classifyKeychainFind(macosRunnerKeychainResult),
    }),
  });
}

async function serviceLaunchStatus(uid, role, paths) {
  const result = await runProbe(
    LAUNCHCTL,
    launchctlArguments("print", { uid, role, paths }),
    runtimeContextForChild(uid),
  );
  return parseLaunchctlPrint(result);
}

async function waitForServiceRunning(uid, role, paths, {
  nodeBinary,
  controllerScript,
  context,
}) {
  let status;
  let stablePid = null;
  let stableProbes = 0;
  for (let attempt = 0; attempt < STARTUP_PROBE_ATTEMPTS; attempt += 1) {
    status = await serviceLaunchStatus(uid, role, paths);
    const expected = status.running && await expectedControllerProcess(
      status.pid,
      nodeBinary,
      controllerScript,
      context,
    );
    if (expected) {
      if (status.pid === stablePid) {
        stableProbes += 1;
      } else {
        stablePid = status.pid;
        stableProbes = 1;
      }
      if (stableProbes >= STARTUP_STABLE_PROBES) return status;
    } else {
      stablePid = null;
      stableProbes = 0;
    }
    if (attempt + 1 < STARTUP_PROBE_ATTEMPTS) {
      await new Promise((resolvePromise) => {
        setTimeout(resolvePromise, STARTUP_PROBE_INTERVAL_MS);
      });
    }
  }
  const detail = status?.loaded
    ? `state ${status.state ?? "unknown"}, last exit ${status.lastExitCode ?? "unknown"}`
    : "LaunchAgent registration disappeared";
  throw new ControllerServiceConfigurationError(
    `controller did not reach a stable exact Node/script process after kickstart: ${detail}`,
  );
}

async function expectedControllerProcess(pid, nodeBinary, controllerScript, context) {
  if (!Number.isSafeInteger(pid) || pid <= 0) return false;
  const result = await runProbe(
    "/bin/ps",
    ["-ww", "-p", String(pid), "-o", "command="],
    context,
  );
  if (result.code !== 0) return false;
  return matchesExpectedControllerCommand(result.stdout, nodeBinary, controllerScript);
}

async function keychainItemExists(role, secretName, context) {
  const result = await runProbe(
    SECURITY,
    keychainFindArguments(role, secretName),
    context,
  );
  return classifyKeychainFind(result);
}

async function runChecked(executable, arguments_, {
  operation,
  context,
  stdio = ["ignore", "pipe", "pipe"],
  allowSuccessfulStderr = false,
  timeoutMs = MUTATION_TIMEOUT_MS,
}) {
  const result = await runChild(executable, arguments_, {
    stdio,
    env: sanitizedInstallerEnvironment(context),
    timeoutMs,
    rejectSuccessfulStderr: !allowSuccessfulStderr,
  });
  if (result.timedOut) throw new Error(`${operation} timed out`);
  if (result.code !== 0) {
    throw new Error(`${operation} failed with exit ${result.code}`);
  }
}

function runChild(executable, arguments_, {
  stdio,
  env,
  timeoutMs,
  rejectSuccessfulStderr,
}) {
  const descriptors = Array.isArray(stdio) ? stdio : [stdio, stdio, stdio];
  if (
    descriptors.length !== 3 ||
    descriptors.some((entry) =>
      entry !== "ignore" && entry !== "inherit" && entry !== "pipe")
  ) {
    throw new ControllerServiceConfigurationError("mutation stdio policy is invalid");
  }
  return runOwnedProcess(executable, arguments_, {
    cwd: "/",
    env,
    input: descriptors[0],
    stdoutMode: descriptors[1],
    stderrMode: descriptors[2],
    timeoutMs,
    rejectSuccessfulStderr,
  });
}

async function runProbe(executable, arguments_, context) {
  const result = await runOwnedProcess(executable, arguments_, {
    cwd: "/",
    env: sanitizedInstallerEnvironment(context),
    input: "ignore",
    stdoutMode: "pipe",
    stderrMode: "pipe",
    timeoutMs: QUERY_TIMEOUT_MS,
    rejectSuccessfulStderr: true,
  });
  if (result.timedOut) {
    throw new ControllerServiceConfigurationError("system query timed out");
  }
  return result;
}

function runOutputProbe(executable, arguments_, {
  cwd = "/",
  env,
  timeoutMs,
  signal,
}) {
  return runOwnedProcess(executable, arguments_, {
    cwd,
    env,
    input: "ignore",
    stdoutMode: "pipe",
    stderrMode: "pipe",
    timeoutMs,
    signal,
    rejectSuccessfulStderr: true,
  });
}

export function runOwnedProcessProbe(executable, arguments_, options = {}) {
  return runOwnedProcess(executable, arguments_, {
    cwd: options.cwd ?? "/",
    env: options.env ?? {},
    input: "ignore",
    stdoutMode: "pipe",
    stderrMode: "pipe",
    timeoutMs: options.timeoutMs ?? QUERY_TIMEOUT_MS,
    signal: options.signal,
    rejectSuccessfulStderr: true,
  });
}

function runOwnedProcess(executable, arguments_, {
  cwd,
  env,
  input,
  stdoutMode,
  stderrMode,
  timeoutMs,
  signal,
  rejectSuccessfulStderr,
}) {
  return new Promise((resolvePromise, rejectPromise) => {
    const posixGroup = process.platform !== "win32";
    const child = spawn(executable, arguments_, {
      cwd,
      env,
      shell: false,
      detached: posixGroup,
      stdio: [input, stdoutMode, stderrMode],
    });
    const stdout = [];
    const stderr = [];
    let stdoutBytes = 0;
    let stderrBytes = 0;
    let code = null;
    let closeSignal = null;
    let spawnError;
    let timedOut = false;
    let outputExceeded = false;
    let aborted;
    let closed = false;
    let cleanupDone = false;
    let settled = false;
    let cleanupTask;

    const send = (signalName) => {
      if (child.pid == null) return false;
      try {
        if (posixGroup) process.kill(-child.pid, signalName);
        else child.kill(signalName);
        return true;
      } catch (cause) {
        if (cause?.code === "ESRCH") return false;
        throw cause;
      }
    };
    const beginCleanup = () => {
      if (cleanupTask != null) return cleanupTask;
      cleanupTask = (async () => {
        let termSent = false;
        try {
          termSent = send("SIGTERM");
          if (termSent) {
            const goneAfterTerm = await waitForOwnedProcessGroup(
              child.pid,
              PROCESS_GROUP_TERM_GRACE_MS,
              posixGroup,
            );
            if (!goneAfterTerm) {
              send("SIGKILL");
              const goneAfterKill = await waitForOwnedProcessGroup(
                child.pid,
                PROCESS_GROUP_REAP_TIMEOUT_MS,
                posixGroup,
              );
              if (!goneAfterKill) {
                throw new ControllerServiceConfigurationError(
                  "detached probe/tool process group did not exit after SIGKILL",
                );
              }
            }
          }
        } finally {
          cleanupDone = true;
          maybeFinish();
        }
      })().catch((cause) => {
        spawnError ??= cause;
        cleanupDone = true;
        maybeFinish();
      });
      return cleanupTask;
    };
    const maybeFinish = () => {
      if (settled || !closed || !cleanupDone) return;
      settled = true;
      clearTimeout(timeout);
      signal?.removeEventListener("abort", onAbort);
      if (spawnError != null) {
        rejectPromise(spawnError);
        return;
      }
      if (outputExceeded) {
        rejectPromise(new ControllerServiceConfigurationError(
          "probe/tool process output exceeded its bound",
        ));
        return;
      }
      if (aborted != null) {
        rejectPromise(aborted);
        return;
      }
      const result = {
        code: code ?? 1,
        signal: closeSignal,
        stdout: Buffer.concat(stdout, stdoutBytes).toString("utf8"),
        stderr: Buffer.concat(stderr, stderrBytes).toString("utf8"),
        timedOut,
      };
      if (rejectSuccessfulStderr && result.code === 0 && result.stderr !== "") {
        rejectPromise(new ControllerServiceConfigurationError(
          "probe/tool process succeeded with forbidden stderr",
        ));
        return;
      }
      resolvePromise(result);
    };
    const collect = (chunks, stream) => (chunk) => {
      if (stream === "stdout") stdoutBytes += chunk.length;
      else stderrBytes += chunk.length;
      if (stdoutBytes > MAX_PROBE_OUTPUT_BYTES || stderrBytes > MAX_PROBE_OUTPUT_BYTES) {
        outputExceeded = true;
        beginCleanup();
        return;
      }
      chunks.push(Buffer.from(chunk));
    };
    child.stdout?.on("data", collect(stdout, "stdout"));
    child.stderr?.on("data", collect(stderr, "stderr"));
    child.once("error", (cause) => {
      spawnError = cause;
      if (child.pid == null) {
        cleanupDone = true;
      } else {
        beginCleanup();
      }
    });
    child.once("exit", (exitCode, signalName) => {
      code = exitCode;
      closeSignal = signalName;
      beginCleanup();
    });
    child.once("close", (exitCode, signalName) => {
      if (code == null) code = exitCode;
      if (closeSignal == null) closeSignal = signalName;
      closed = true;
      if (cleanupTask == null) beginCleanup();
      maybeFinish();
    });
    const onAbort = () => {
      aborted = signal?.reason ?? new DOMException("operation aborted", "AbortError");
      beginCleanup();
    };
    signal?.addEventListener("abort", onAbort, { once: true });
    if (signal?.aborted) onAbort();
    const timeout = setTimeout(() => {
      timedOut = true;
      beginCleanup();
    }, timeoutMs);
    timeout.unref?.();
  });
}

async function waitForOwnedProcessGroup(pid, timeoutMs, posixGroup) {
  if (!Number.isSafeInteger(pid) || pid <= 0) return true;
  const deadline = Date.now() + timeoutMs;
  while (true) {
    let exists = false;
    try {
      if (posixGroup) process.kill(-pid, 0);
      else process.kill(pid, 0);
      exists = true;
    } catch (cause) {
      if (cause?.code !== "ESRCH") throw cause;
    }
    if (!exists) return true;
    if (Date.now() >= deadline) return false;
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 25));
  }
}

function sanitizedInstallerEnvironment(context) {
  const homeDirectory = context?.homeDirectory ?? userInfo().homedir;
  const username = context?.username ?? userInfo().username;
  const environment = {
    HOME: homeDirectory,
    USER: username,
    LOGNAME: username,
    PATH: "/usr/bin:/bin:/usr/sbin:/sbin",
    LANG: "C",
    LC_ALL: "C",
  };
  for (const name of ["TMPDIR", "TZ", "SSL_CERT_FILE", "SSL_CERT_DIR"]) {
    const value = process.env[name];
    if (typeof value === "string" && value !== "") environment[name] = value;
  }
  return environment;
}

function runtimeContextForChild(uid) {
  const identity = userInfo();
  if (identity.uid !== uid) {
    throw new ControllerServiceConfigurationError("passwd user changed during installation");
  }
  return {
    homeDirectory: identity.homedir,
    username: identity.username,
  };
}

function probeDetail(stderr) {
  const detail = String(stderr).trim().replaceAll(/\s+/g, " ").slice(0, 500);
  return detail === "" ? "" : `: ${detail}`;
}

async function pathMetadata(path) {
  try {
    return await lstat(path);
  } catch (cause) {
    if (cause?.code === "ENOENT") return null;
    throw cause;
  }
}

async function exists(path) {
  return (await pathMetadata(path)) != null;
}

function usage() {
  return `Usage:
  install-ci-controller-service.mjs install --role master --origin <https-origin> --node <absolute-node> --repo <absolute-checkout> --rustsec-repo <absolute-checkout> --cloudflare-account-id <32-hex> [--web-origin <https-origin>]
  install-ci-controller-service.mjs install --role pr --origin <https-origin> --node <absolute-node> --repo <absolute-checkout> --prep-user <dedicated-non-login-user> --cargo-sha256 <64-lowercase-hex>
  install-ci-controller-service.mjs update <same role options> [--replace-secrets]
  install-ci-controller-service.mjs status --role <master|pr>
  install-ci-controller-service.mjs uninstall --role <master|pr> [--remove-data]

Secrets are prompted directly by macOS security(1). The installer never accepts
or reads a token and never executes a controller or any other checkout code.

${serviceRunbook("master")}

${serviceRunbook("pr")}
`;
}

const invokedPath = process.argv[1] == null ? null : pathToFileURL(resolve(process.argv[1])).href;
if (invokedPath === import.meta.url) {
  main().catch((cause) => {
    process.stderr.write(
      `install-ci-controller-service: ${cause instanceof Error ? cause.message : String(cause)}\n`,
    );
    process.exitCode = 1;
  });
}
