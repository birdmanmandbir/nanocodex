import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rename,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { arch, tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import {
  LOG_ROTATION_INTERVAL_SECONDS,
  LAUNCHD_EXIT_TIMEOUT_SECONDS,
  MAX_CURRENT_LOG_BYTES,
  PR_PREP_CARGO_PATH,
  PR_PREP_CARGO_RELEASE,
  PR_PREP_HELPER_PATH,
  PR_PREP_HELPER_SHA256,
  PR_PREP_HELPER_VERSION,
  PR_PREP_NODE_MAX_BYTES,
  PR_PREP_RUSTC_MANIFEST_PATH,
  PR_PREP_RUSTC_PATH,
  PR_PREP_RUSTC_RELEASE,
  PR_PREP_RUSTC_ROOT,
  PR_FORBIDDEN_SECRET_KEYS,
  RETAINED_LOG_BYTES,
  ROLE_KEYCHAIN_ACCOUNTS,
  ROLE_SECRET_ALLOWLISTS,
  ROLE_SOURCE_CAPABILITIES,
  SERVICE_LABELS,
  THROTTLE_INTERVAL_SECONDS,
  assertNoColocatedRoleState,
  assertPrPrepIdentityUnchanged,
  assertSafeOwnedEntry,
  captureTrustedPrPrepHelperSource,
  captureTrustedPrPrepNodeExecutable,
  classifyKeychainFind,
  classifyLaunchctlPrint,
  controllerScriptPath,
  deriveServicePaths,
  keychainDeleteArguments,
  keychainFindArguments,
  keychainAccountName,
  keychainStoreArguments,
  launchctlArguments,
  matchesExpectedControllerCommand,
  parseCliArguments,
  parseLocalAccountUidList,
  parseLocalAccountIdentityList,
  parseLocalGroupIdentityList,
  parseMacOsPasswdRecord,
  parseNumericGroupList,
  parsePrPrepRustcManifest,
  parsePrPrepDirectoryGroupRecord,
  parsePrPrepDirectoryUserRecord,
  parseUnavoidableMacOsGroupRecord,
  prPrepOperatorCleanup,
  parseLaunchctlPrint,
  renderControllerWrapper,
  renderLaunchAgentPlist,
  renderLiveEnvironmentProbeProgram,
  renderPrPrepProvisioning,
  renderPrPrepRustcProvisioningValidationProgram,
  renderPrPrepRuntimeValidationProgram,
  renderPrPrepSudoersRule,
  runGeneratedWrapperProbe,
  runOwnedProcessProbe,
  serviceRunbook,
  shouldStoreKeychainSecret,
  validateAbsolutePath,
  validateCargoVersionOutput,
  validateCloudflareAccountId,
  validateEnvironmentProbe,
  validateHost,
  validateLifecycleState,
  validateNodeIdentity,
  validateNoAclListing,
  validateOrigin,
  validatePrPrepAccount,
  validatePrPrepCargoSnapshot,
  validatePrPrepHomeSnapshot,
  validatePrPrepHelperSnapshot,
  validatePrPrepNodeSnapshot,
  validatePrPrepProbe,
  validatePrPrepRustcBundleSnapshot,
  validatePrPrepSudoEvidence,
  validatePrPrepSudoPolicy,
  validatePrPrepUsername,
  validateRecordedPrPrepIdentity,
  validateRustcVersionOutput,
  validateRole,
  validateServicePaths,
  prPrepSudoArguments,
} from "./install-ci-controller-service.mjs";

const TEST_DIRECTORY = dirname(fileURLToPath(import.meta.url));
const REPOSITORY = resolve(TEST_DIRECTORY, "../..");
const UID = typeof process.getuid === "function" ? process.getuid() : 501;
const GID = typeof process.getgid === "function" ? process.getgid() : 20;
const USERNAME = "ci-controller";
const PREP_USERNAME = "_nanocodex_pr_prep";
const PREP_UID = 399;
const PREP_GID = 399;
const HOME = "/Users/ci-controller";
const ACCOUNT_ID = "0123456789abcdef0123456789abcdef";
const PREP_GENERATED_UID = "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE";
const PREP_GROUP_GENERATED_UID = "BBBBBBBB-CCCC-DDDD-EEEE-FFFFFFFFFFFF";
const CARGO_SHA256 = "c".repeat(64);
const RUSTC_DRIVER = "librustc_driver-4031c0ff8e88f5d1.dylib";
const RUSTC_MANIFEST_VALUE = {
  version: 1,
  release: PR_PREP_RUSTC_RELEASE,
  host: "aarch64-apple-darwin",
  files: [
    { path: "bin/rustc", size: 400_000, sha256: "1".repeat(64) },
    { path: "lib/libLLVM.dylib", size: 140_000_000, sha256: "2".repeat(64) },
    { path: `lib/${RUSTC_DRIVER}`, size: 83_000_000, sha256: "3".repeat(64) },
  ],
};
const RUSTC_MANIFEST_TEXT = JSON.stringify(RUSTC_MANIFEST_VALUE) + "\n";
const RUSTC_MANIFEST_SHA256 = createHash("sha256")
  .update(RUSTC_MANIFEST_TEXT)
  .digest("hex");
const RUSTC_VERSION_OUTPUT = `rustc ${PR_PREP_RUSTC_RELEASE} (88d9e12ae 2026-08-18)
binary: rustc
commit-hash: 88d9e12ae178fab0fb5cc050a94da85685d449ea
commit-date: 2026-08-18
host: aarch64-apple-darwin
release: ${PR_PREP_RUSTC_RELEASE}
LLVM version: 22.1.8
`;
const CARGO_VERSION_OUTPUT = `cargo ${PR_PREP_CARGO_RELEASE} (012345678 2026-08-01)
release: ${PR_PREP_CARGO_RELEASE}
commit-hash: 0123456789abcdef0123456789abcdef01234567
commit-date: 2026-08-01
host: aarch64-apple-darwin
libgit2: 1.9.0 (sys:0.20.0 vendored)
libcurl: 8.12.1 (sys:0.4.80+curl-8.12.1 system ssl:(SecureTransport) LibreSSL/3.3.6)
ssl: OpenSSL 3.6.2 7 Apr 2026
os: Mac OS 26.0.0 [64-bit]
`;

test("role, host, origin, account, path, and lifecycle arguments are strict", () => {
  assert.equal(validateRole("master"), "master");
  assert.equal(validateRole("pr"), "pr");
  assert.throws(() => validateRole("release"), /master or pr/);

  assert.equal(validateOrigin("https://ci.example.test/"), "https://ci.example.test");
  for (const invalid of [
    "http://ci.example.test",
    "https://user:token@ci.example.test",
    "https://ci.example.test/api",
    "https://ci.example.test?query=yes",
    " https://ci.example.test",
  ]) {
    assert.throws(() => validateOrigin(invalid), /HTTPS|origin/);
  }
  assert.equal(validateCloudflareAccountId(ACCOUNT_ID), ACCOUNT_ID);
  assert.throws(() => validateCloudflareAccountId(ACCOUNT_ID.toUpperCase()), /lowercase/);
  assert.equal(validateAbsolutePath("/opt/node/bin/node"), "/opt/node/bin/node");
  for (const invalid of ["node", "/opt/node/../node", "/opt/node/", "/opt/node\n--eval"]) {
    assert.throws(() => validateAbsolutePath(invalid), /normalized absolute path/);
  }

  assert.deepEqual(validateHost({
    platform: "darwin",
    arch: "arm64",
    uid: 501,
    euid: 501,
    identityUid: 501,
    homeDirectory: HOME,
    username: USERNAME,
  }), {
    uid: 501,
    username: USERNAME,
    architecture: "arm64",
    guiDomain: "gui/501",
    homeDirectory: HOME,
  });
  assert.throws(() => validateHost({
    platform: "linux",
    arch: "arm64",
    uid: 501,
    homeDirectory: HOME,
    username: USERNAME,
  }), /darwin/);
  assert.throws(() => validateHost({
    platform: "darwin",
    arch: "arm64",
    uid: 501,
    euid: 0,
    identityUid: 501,
    homeDirectory: HOME,
    username: USERNAME,
  }), /sudo/);
  assert.equal(validateNodeIdentity("node/darwin/arm64", "arm64"), "node/darwin/arm64");
  assert.throws(() => validateNodeIdentity("node/darwin/x64", "arm64"), /node\/darwin\/arm64/);

  assert.deepEqual(parseCliArguments([
    "install",
    "--role",
    "master",
    "--origin",
    "https://ci.example.test/",
    "--node",
    "/opt/node/bin/node",
    "--repo",
    "/Users/ci/nanocodex",
    "--rustsec-repo",
    "/Users/ci/advisory-db",
    "--cloudflare-account-id",
    ACCOUNT_ID,
  ]), {
    command: "install",
    role: "master",
    origin: "https://ci.example.test",
    webOrigin: "https://ci.example.test",
    nodeBinary: "/opt/node/bin/node",
    repository: "/Users/ci/nanocodex",
    rustSecRepository: "/Users/ci/advisory-db",
    cloudflareAccountId: ACCOUNT_ID,
    replaceSecrets: false,
  });
  assert.deepEqual(parseCliArguments([
    "update",
    "--repo",
    "/Users/ci/nanocodex",
    "--replace-secrets",
    "--node",
    "/opt/node/bin/node",
    "--origin",
    "https://ci.example.test",
    "--role",
    "pr",
    "--prep-user",
    PREP_USERNAME,
    "--cargo-sha256",
    CARGO_SHA256,
    "--rustc-manifest-sha256",
    RUSTC_MANIFEST_SHA256,
  ]), {
    command: "update",
    role: "pr",
    origin: "https://ci.example.test",
    webOrigin: undefined,
    nodeBinary: "/opt/node/bin/node",
    repository: "/Users/ci/nanocodex",
    rustSecRepository: undefined,
    cloudflareAccountId: undefined,
    prepUsername: PREP_USERNAME,
    cargoSha256: CARGO_SHA256,
    rustcManifestSha256: RUSTC_MANIFEST_SHA256,
    replaceSecrets: true,
  });
  assert.deepEqual(parseCliArguments(["status", "--role", "pr"]), {
    command: "status",
    role: "pr",
  });
  assert.deepEqual(parseCliArguments([
    "uninstall",
    "--remove-data",
    "--role",
    "master",
  ]), {
    command: "uninstall",
    role: "master",
    removeData: true,
  });
  assert.throws(() => parseCliArguments([
    "install",
    "--role",
    "pr",
    "--origin",
    "https://ci.example.test",
    "--node",
    "/opt/node/bin/node",
    "--repo",
    "/Users/ci/nanocodex",
    "--token",
    "argv-secret",
  ]), /unknown install option/);
  assert.throws(() => parseCliArguments([
    "install",
    "--role",
    "pr",
    "--origin",
    "https://ci.example.test",
    "--node",
    "/opt/node/bin/node",
    "--repo",
    "/Users/ci/nanocodex",
    "--cloudflare-account-id",
    ACCOUNT_ID,
  ]), /forbidden for the pr role/);
  assert.throws(() => parseCliArguments([
    "install",
    "--role",
    "pr",
    "--origin",
    "https://ci.example.test",
    "--node",
    "/opt/node/bin/node",
    "--repo",
    "/Users/ci/nanocodex",
  ]), /requires --prep-user, --cargo-sha256, and --rustc-manifest-sha256/);
  assert.throws(() => parseCliArguments([
    "install",
    "--role",
    "master",
    "--origin",
    "https://ci.example.test",
    "--node",
    "/opt/node/bin/node",
    "--repo",
    "/Users/ci/nanocodex",
    "--rustsec-repo",
    "/Users/ci/advisory-db",
    "--cloudflare-account-id",
    ACCOUNT_ID,
    "--prep-user",
    PREP_USERNAME,
  ]), /forbidden for the master role/);
  assert.throws(() => parseCliArguments([
    "update",
    "--role",
    "pr",
    "--origin",
    "https://ci.example.test",
    "--node",
    "/opt/node/bin/node",
    "--repo",
    "/Users/ci/nanocodex",
  ]), /update pr requires --prep-user, --cargo-sha256, and --rustc-manifest-sha256/);
  assert.throws(() => parseCliArguments([
    "install",
    "--role",
    "master",
    "--origin",
    "https://ci.example.test",
    "--node",
    "/opt/node/bin/node",
    "--repo",
    "/Users/ci/nanocodex",
    "--rustsec-repo",
    "/Users/ci/advisory-db",
    "--cloudflare-account-id",
    ACCOUNT_ID,
    "--rustc-manifest-sha256",
    RUSTC_MANIFEST_SHA256,
  ]), /rustc-manifest-sha256.*forbidden for the master role/);
  assert.throws(
    () => parseCliArguments(["status", "--role", "pr", "--prep-user", PREP_USERNAME]),
    /status requires --role/,
  );
  assert.throws(
    () => parseCliArguments(["uninstall", "--role", "pr", "--prep-user", PREP_USERNAME]),
    /uninstall requires --role/,
  );
  for (const invalid of ["Prep User", "-prep", "prep$", "a".repeat(32)]) {
    assert.throws(() => validatePrPrepUsername(invalid), /must match/);
  }
});

test("PR preparation account is one unique non-login identity with isolated groups and no service role", () => {
  assert.deepEqual(
    parseMacOsPasswdRecord(
      `${PREP_USERNAME}:********:${PREP_UID}:${PREP_GID}::0:0:PR prep:/var/empty:/usr/bin/false\n`,
      PREP_USERNAME,
    ),
    {
      username: PREP_USERNAME,
      uid: PREP_UID,
      gid: PREP_GID,
      homeDirectory: "/var/empty",
      shell: "/usr/bin/false",
    },
  );
  assert.deepEqual(parseNumericGroupList(`${PREP_GID} 12 61 79\n`), [
    PREP_GID,
    12,
    61,
    79,
  ]);
  assert.deepEqual(parseLocalAccountUidList(`_nobody -2\nroot 0\n${PREP_USERNAME} ${PREP_UID}\n`), [
    { username: "_nobody", uid: -2 },
    { username: "root", uid: 0 },
    { username: PREP_USERNAME, uid: PREP_UID },
  ]);

  const directoryUser = parsePrPrepDirectoryUserRecord(
    `RecordName: ${PREP_USERNAME}\nUniqueID: ${PREP_UID}\nPrimaryGroupID: ${PREP_GID}\n` +
      `NFSHomeDirectory: /var/empty\nUserShell: /usr/bin/false\n` +
      `GeneratedUID: ${PREP_GENERATED_UID}\n`,
    PREP_USERNAME,
  );
  assert.deepEqual(directoryUser, {
    username: PREP_USERNAME,
    uid: PREP_UID,
    gid: PREP_GID,
    homeDirectory: "/var/empty",
    shell: "/usr/bin/false",
    generatedUid: PREP_GENERATED_UID,
  });
  const primaryGroup = parsePrPrepDirectoryGroupRecord(
    `RecordName: ${PREP_USERNAME}\nPrimaryGroupID: ${PREP_GID}\n` +
      `GeneratedUID: ${PREP_GROUP_GENERATED_UID}\n` +
      `GroupMembership: ${PREP_USERNAME}\nGroupMembers: ${PREP_GENERATED_UID}\n`,
    PREP_USERNAME,
    PREP_GID,
  );
  assert.deepEqual(primaryGroup, {
    name: PREP_USERNAME,
    gid: PREP_GID,
    generatedUid: PREP_GROUP_GENERATED_UID,
    members: [PREP_USERNAME],
    memberUuids: [PREP_GENERATED_UID],
    nestedGroupUuids: [],
  });
  assert.deepEqual(validatePrPrepHomeSnapshot({
    parents: ["/", "/private", "/private/var"].map((path) => ({
      path,
      canonicalPath: path,
      kind: "directory",
      symbolicLink: false,
      uid: 0,
      gid: 0,
      specialMode: 0,
      mode: 0o755,
    })),
    home: {
      path: "/var/empty",
      canonicalPath: "/private/var/empty",
      kind: "directory",
      symbolicLink: false,
      uid: 0,
      gid: 3,
      specialMode: 0,
      mode: 0o755,
    },
  }, "/var/empty"), {
    path: "/var/empty",
    canonicalPath: "/private/var/empty",
    uid: 0,
    gid: 3,
    mode: 0o755,
  });
  const localAccounts = parseLocalAccountIdentityList(
    `_nobody -2\n${PREP_USERNAME} ${PREP_UID}\nrunner 502\n`,
    `_nobody -2\n${PREP_USERNAME} ${PREP_GID}\nrunner 502\n`,
  );
  const localGroups = parseLocalGroupIdentityList(
    `${PREP_USERNAME} ${PREP_GID}\nstaff 20\neveryone 12\n` +
      "localaccounts 61\n_appserverusr 79\n",
  );
  const unavoidableGroups = [
    parseUnavoidableMacOsGroupRecord(
      "GeneratedUID: ABCDEFAB-CDEF-ABCD-EFAB-CDEF0000000C\n" +
        "PrimaryGroupID: 12\nRecordName: everyone BUILTIN\\Everyone\n",
      12,
    ),
    parseUnavoidableMacOsGroupRecord(
      "GeneratedUID: ABCDEFAB-CDEF-ABCD-EFAB-CDEF0000003D\n" +
        "PrimaryGroupID: 61\nRecordName: localaccounts\n",
      61,
    ),
    parseUnavoidableMacOsGroupRecord(
      "GeneratedUID: ABCDEFAB-CDEF-ABCD-EFAB-CDEF0000004F\n" +
        "PrimaryGroupID: 79\nRecordName: _appserverusr appserverusr\n",
      79,
    ),
  ];

  const record = {
    username: PREP_USERNAME,
    uid: PREP_UID,
    gid: PREP_GID,
    homeDirectory: "/var/empty",
    shell: "/usr/bin/false",
    generatedUid: PREP_GENERATED_UID,
    primaryGroupName: PREP_USERNAME,
    primaryGroupGeneratedUid: PREP_GROUP_GENERATED_UID,
    supplementaryGids: [PREP_GID, 12, 61, 79],
  };
  const policy = {
    controllerUid: 501,
    controllerUsername: USERNAME,
    controllerGids: [20, 12, 61, 79],
    localAccounts,
    localGroups,
    unavoidableGroups,
    directoryUser,
    primaryGroup,
    assignedRoles: [],
  };
  assert.deepEqual(validatePrPrepAccount(record, policy), record);
  assert.throws(
    () => validatePrPrepAccount({ ...record, uid: 501 }, {
      ...policy,
      localAccounts: [{ name: PREP_USERNAME, uid: 501, primaryGid: PREP_GID }],
    }),
    /distinct from the controller service role/,
  );
  assert.throws(
    () => validatePrPrepAccount({ ...record, username: USERNAME }, {
      ...policy,
      localAccounts: [{ name: USERNAME, uid: PREP_UID, primaryGid: PREP_GID }],
      directoryUser: { ...directoryUser, username: USERNAME },
      primaryGroup: { ...primaryGroup, name: USERNAME },
    }),
    /distinct from the controller service role/,
  );
  assert.throws(
    () => validatePrPrepAccount(record, { ...policy, localAccounts: [] }),
    /local account records are invalid/,
  );
  assert.throws(
    () => validatePrPrepAccount({
      ...record,
      supplementaryGids: [PREP_GID, 12, 61, 79, 80],
    }, policy),
    /admin group 80|avoidable supplementary groups/,
  );
  assert.throws(
    () => validatePrPrepAccount({ ...record, shell: "/bin/zsh" }, policy),
    /non-login shell/,
  );
  assert.throws(
    () => validatePrPrepAccount({ ...record, homeDirectory: "/Users/prep" }, policy),
    /home must be/,
  );
  assert.throws(
    () => validatePrPrepAccount({
      ...record,
      supplementaryGids: [PREP_GID, 12, 61, 79, 777],
    }, { ...policy, controllerGids: [20, 12, 61, 79, 777] }),
    /avoidable supplementary groups: 777/,
  );
  for (const role of ["master-controller", "pr-controller", "macos-runner"]) {
    assert.throws(
      () => validatePrPrepAccount(record, { ...policy, assignedRoles: [role] }),
      /must not be a controller or macOS runner account/,
    );
  }
  assert.throws(
    () => validatePrPrepAccount(record, {
      ...policy,
      localAccounts: [
        { name: PREP_USERNAME, uid: PREP_UID, primaryGid: PREP_GID },
        { name: "runner-alias", uid: PREP_UID, primaryGid: 502 },
      ],
    }),
    /exclusively to one local passwd account/,
  );
  for (const sharedGid of [12, 61, 79]) {
    assert.throws(
      () => validatePrPrepAccount({
        ...record,
        gid: sharedGid,
        supplementaryGids: [12, 61, 79],
      }, {
        ...policy,
        localAccounts: [{ name: PREP_USERNAME, uid: PREP_UID, primaryGid: sharedGid }],
        localGroups: [{ name: PREP_USERNAME, gid: sharedGid }],
        directoryUser: { ...directoryUser, gid: sharedGid },
        primaryGroup: { ...primaryGroup, gid: sharedGid },
      }),
      new RegExp(`primary GID ${sharedGid} is shared or reserved`),
    );
  }
  assert.throws(
    () => validatePrPrepAccount(record, {
      ...policy,
      localAccounts: [
        ...localAccounts,
        { name: "runner-shared", uid: 503, primaryGid: PREP_GID },
      ],
    }),
    /primary GID must belong exclusively/,
  );
  assert.throws(
    () => validatePrPrepAccount(record, {
      ...policy,
      localGroups: [...localGroups, { name: "ambiguous", gid: PREP_GID }],
    }),
    /map exactly once/,
  );
  assert.throws(
    () => validatePrPrepAccount(record, {
      ...policy,
      localGroups: localGroups.map((entry) =>
        entry.gid === PREP_GID ? { name: "_developer", gid: PREP_GID } : entry),
      primaryGroup: { ...primaryGroup, name: "_developer" },
    }),
    /dedicated same-name group/,
  );
  assert.throws(
    () => validatePrPrepAccount(record, {
      ...policy,
      localGroups: localGroups.map((entry) =>
        entry.gid === 12 ? { name: "not-everyone", gid: 12 } : entry),
    }),
    /unavoidable macOS group 12 has an ambiguous identity/,
  );
  assert.throws(
    () => validatePrPrepAccount(record, {
      ...policy,
      unavoidableGroups: policy.unavoidableGroups.map((entry) =>
        entry.gid === 12 ? { ...entry, generatedUid: PREP_GROUP_GENERATED_UID } : entry),
    }),
    /authoritative name\/GUID records/,
  );
  assert.throws(
    () => parseUnavoidableMacOsGroupRecord(
      "GeneratedUID: ABCDEFAB-CDEF-ABCD-EFAB-CDEF0000000D\n" +
        "PrimaryGroupID: 12\nRecordName: everyone BUILTIN\\Everyone\n",
      12,
    ),
    /GeneratedUID must contain one exact value/,
  );
  assert.throws(
    () => validatePrPrepAccount(record, {
      ...policy,
      primaryGroup: { ...primaryGroup, members: [PREP_USERNAME, "runner"] },
    }),
    /another member/,
  );
  assert.throws(
    () => validatePrPrepAccount(record, {
      ...policy,
      primaryGroup: { ...primaryGroup, nestedGroupUuids: ["FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF"] },
    }),
    /another member/,
  );
  assert.throws(
    () => validatePrPrepHomeSnapshot({
      parents: ["/", "/private", "/private/var"].map((path) => ({
        path,
        canonicalPath: path,
        kind: "directory",
        symbolicLink: false,
        uid: 0,
        gid: 0,
        specialMode: 0,
        mode: path === "/private/var" ? 0o775 : 0o755,
      })),
      home: {
        path: "/var/empty",
        canonicalPath: "/private/var/empty",
        kind: "directory",
        symbolicLink: false,
        uid: 0,
        gid: 3,
        specialMode: 0,
        mode: 0o755,
      },
    }, "/var/empty"),
    /non-writable root-owned real empty-home parent/,
  );
  assert.throws(
    () => parsePrPrepDirectoryGroupRecord(
      `RecordName: ${PREP_USERNAME}\nPrimaryGroupID: ${PREP_GID}\n` +
        `GeneratedUID: ${PREP_GROUP_GENERATED_UID}\n` +
        "NestedGroups: FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF\n",
      PREP_USERNAME,
      PREP_GID,
    ),
    /another member|NestedGroups/,
  );
  assert.throws(
    () => parsePrPrepDirectoryUserRecord(
      `RecordName: ${PREP_USERNAME}\nUniqueID: ${PREP_UID}\nPrimaryGroupID: ${PREP_GID}\n` +
        `NFSHomeDirectory: /var/empty\nUserShell: /usr/bin/false\n` +
        `GeneratedUID: ${PREP_GENERATED_UID}\nAuthenticationAuthority: ;ShadowHash;HASHLIST\n`,
      PREP_USERNAME,
    ),
    /no AuthenticationAuthority/,
  );
  assert.throws(
    () => parsePrPrepDirectoryGroupRecord(
      `RecordName: ${PREP_USERNAME} alias\nPrimaryGroupID: ${PREP_GID}\n` +
        `GeneratedUID: ${PREP_GROUP_GENERATED_UID}\n`,
      PREP_USERNAME,
      PREP_GID,
    ),
    /one exact value/,
  );
  assert.throws(
    () => parseMacOsPasswdRecord("", PREP_USERNAME),
    /passwd record is invalid/,
  );
});

test("master and PR Keychain allowlists are exact and PR excludes every promotion authority", () => {
  assert.deepEqual(ROLE_SECRET_ALLOWLISTS.master, [
    "NANOCODEX_CI_TOKEN",
    "NANOCODEX_GITHUB_STATUS_TOKEN",
    "CLOUDFLARE_API_TOKEN",
    "NANOCODEX_GIT_TOKEN",
  ]);
  assert.deepEqual(ROLE_SECRET_ALLOWLISTS.pr, [
    "NANOCODEX_CI_TOKEN",
    "NANOCODEX_GITHUB_STATUS_TOKEN",
  ]);
  assert.deepEqual(
    ROLE_SECRET_ALLOWLISTS.master.filter((name) => !ROLE_SECRET_ALLOWLISTS.pr.includes(name)),
    ["CLOUDFLARE_API_TOKEN", "NANOCODEX_GIT_TOKEN"],
  );
  assert.deepEqual(ROLE_SOURCE_CAPABILITIES, {
    master: "CI_MASTER_SOURCE_WRITE_TOKEN",
    pr: "CI_PR_SOURCE_WRITE_TOKEN",
  });
  assert.deepEqual(ROLE_KEYCHAIN_ACCOUNTS.master, {
    NANOCODEX_CI_TOKEN: "CI_MASTER_SOURCE_WRITE_TOKEN",
    NANOCODEX_GITHUB_STATUS_TOKEN: "NANOCODEX_GITHUB_STATUS_TOKEN",
    CLOUDFLARE_API_TOKEN: "CLOUDFLARE_API_TOKEN",
    NANOCODEX_GIT_TOKEN: "NANOCODEX_GIT_TOKEN",
  });
  assert.deepEqual(ROLE_KEYCHAIN_ACCOUNTS.pr, {
    NANOCODEX_CI_TOKEN: "CI_PR_SOURCE_WRITE_TOKEN",
    NANOCODEX_GITHUB_STATUS_TOKEN: "NANOCODEX_GITHUB_STATUS_TOKEN",
  });
  assert.equal(keychainAccountName("master", "NANOCODEX_CI_TOKEN"),
    "CI_MASTER_SOURCE_WRITE_TOKEN");
  assert.equal(keychainAccountName("pr", "NANOCODEX_CI_TOKEN"),
    "CI_PR_SOURCE_WRITE_TOKEN");
  assert.notEqual(
    keychainAccountName("master", "NANOCODEX_CI_TOKEN"),
    keychainAccountName("pr", "NANOCODEX_CI_TOKEN"),
  );
  for (const forbidden of [
    "CLOUDFLARE_API_TOKEN",
    "NANOCODEX_GIT_TOKEN",
    "NPM_TOKEN",
    "NODE_AUTH_TOKEN",
    "CI_RELEASE_TOKEN",
    "NANOCODEX_SANDBOX_CONTROL_TOKEN",
    "NANOCODEX_CI_MACOS_TOKEN",
    "NANOCODEX_CI_MACOS_RUNNER_TOKEN",
    "CI_MACOS_RUNNER_TOKEN",
    "CI_MASTER_SOURCE_WRITE_TOKEN",
    "CARGO_REGISTRY_TOKEN",
    "R2_SECRET_ACCESS_KEY",
  ]) {
    assert.ok(PR_FORBIDDEN_SECRET_KEYS.includes(forbidden), forbidden);
    assert.ok(!ROLE_SECRET_ALLOWLISTS.pr.includes(forbidden), forbidden);
  }
  assert.equal(new Set(PR_FORBIDDEN_SECRET_KEYS).size, PR_FORBIDDEN_SECRET_KEYS.length);
  assert.throws(
    () => keychainAccountName("master", "CI_PR_SOURCE_WRITE_TOKEN"),
    /not in the master controller Keychain allowlist/,
  );
  assert.throws(
    () => keychainAccountName("pr", "CI_MASTER_SOURCE_WRITE_TOKEN"),
    /not in the pr controller Keychain allowlist/,
  );
  assert.equal(assertNoColocatedRoleState("pr", {
    loaded: false,
    plist: false,
    wrapper: false,
    state: false,
    keychainItems: [false, false, false, false],
    macosRunner: {
      loaded: false,
      plist: false,
      state: false,
      keychainItem: false,
    },
  }), true);
  for (const state of [
    { loaded: true, plist: false, wrapper: false, state: false, keychainItems: [false, false, false, false] },
    { loaded: false, plist: true, wrapper: false, state: false, keychainItems: [false, false, false, false] },
    { loaded: false, plist: false, wrapper: true, state: false, keychainItems: [false, false, false, false] },
    { loaded: false, plist: false, wrapper: false, state: true, keychainItems: [false, false, false, false] },
    { loaded: false, plist: false, wrapper: false, state: false, keychainItems: [false, true, false, false] },
  ]) {
    assert.throws(
      () => assertNoColocatedRoleState("pr", {
        ...state,
        macosRunner: {
          loaded: false,
          plist: false,
          state: false,
          keychainItem: false,
        },
      }),
      /distinct dedicated login users/,
    );
  }
  for (const field of ["loaded", "plist", "state", "keychainItem"]) {
    assert.throws(
      () => assertNoColocatedRoleState("master", {
        loaded: false,
        plist: false,
        wrapper: false,
        state: false,
        keychainItems: [false, false],
        macosRunner: {
          loaded: false,
          plist: false,
          state: false,
          keychainItem: false,
          [field]: true,
        },
      }),
      /four distinct dedicated identities/,
      field,
    );
  }
});

test("update is the recovery path for partial or unloaded installations", () => {
  assert.deepEqual(validateLifecycleState("install", {
    loaded: false,
    plist: false,
    wrapper: false,
  }), { anyArtifacts: false, complete: false, partial: false });
  assert.throws(() => validateLifecycleState("install", {
    loaded: false,
    plist: true,
    wrapper: false,
  }), /use update or uninstall/);
  assert.throws(() => validateLifecycleState("update", {
    loaded: false,
    plist: false,
    wrapper: false,
  }), /absent; use install/);
  assert.deepEqual(validateLifecycleState("update", {
    loaded: false,
    plist: true,
    wrapper: true,
  }), { anyArtifacts: true, complete: true, partial: false });
  assert.deepEqual(validateLifecycleState("update", {
    loaded: false,
    plist: true,
    wrapper: false,
  }), { anyArtifacts: true, complete: false, partial: true });
  assert.deepEqual(validateLifecycleState("update", {
    loaded: true,
    plist: false,
    wrapper: false,
  }), { anyArtifacts: false, complete: false, partial: false });
});

test("role-scoped plists enter an empty environment before one fixed wrapper", () => {
  const master = deriveServicePaths("/Users/CI & Build", "master");
  const pr = deriveServicePaths("/Users/CI & Build", "pr");
  assert.equal(master.serviceDirectory, "/Users/CI & Build/Library/Application Support/nanocodex/ci-controllers/master");
  assert.equal(pr.serviceDirectory, "/Users/CI & Build/Library/Application Support/nanocodex/ci-controllers/pr");
  assert.equal(master.wrapperPath, `${master.serviceDirectory}/run-controller.sh`);
  assert.equal(pr.statePath, `${pr.serviceDirectory}/pr-state.json`);
  assert.equal(Object.hasOwn(pr, "cargoHomeDirectory"), false);
  assert.equal(master.plistPath, `/Users/CI & Build/Library/LaunchAgents/${SERVICE_LABELS.master}.plist`);
  assert.equal(pr.plistPath, `/Users/CI & Build/Library/LaunchAgents/${SERVICE_LABELS.pr}.plist`);
  assert.equal(validateServicePaths(master, "master"), master);
  assert.throws(() => validateServicePaths(master, "pr"), /fixed pr controller layout/);
  assert.throws(
    () => validateServicePaths({ ...master, stdoutPath: "/tmp/controller.log" }, "master"),
    /fixed master controller layout/,
  );

  const repository = "/Users/CI & Build/trusted/nanocodex";
  const plist = renderLaunchAgentPlist(master, "master", repository);
  assert.match(plist, /^<\?xml version="1\.0" encoding="UTF-8"\?>/);
  assert.match(plist, new RegExp(`<string>${SERVICE_LABELS.master}</string>`));
  assert.match(
    plist,
    /<key>Program<\/key>\s*<string>\/usr\/bin\/env<\/string>\s*<key>ProgramArguments<\/key>\s*<array>\s*<string>\/usr\/bin\/env<\/string>\s*<string>-i<\/string>\s*<string>\/Users\/CI &amp; Build\/Library\/Application Support\/nanocodex\/ci-controllers\/master\/run-controller\.sh<\/string>\s*<\/array>/,
  );
  assert.match(plist, /<key>WorkingDirectory<\/key>\s*<string>\/Users\/CI &amp; Build\/trusted\/nanocodex<\/string>/);
  assert.match(plist, /<key>RunAtLoad<\/key>\s*<true\/>/);
  assert.match(plist, /<key>KeepAlive<\/key>\s*<true\/>/);
  assert.doesNotMatch(plist, /SuccessfulExit/);
  assert.match(
    plist,
    new RegExp(`<key>ThrottleInterval</key>\\s*<integer>${THROTTLE_INTERVAL_SECONDS}</integer>`),
  );
  assert.ok(LAUNCHD_EXIT_TIMEOUT_SECONDS * 1_000 > 750);
  assert.match(
    plist,
    new RegExp(`<key>ExitTimeOut</key>\\s*<integer>${LAUNCHD_EXIT_TIMEOUT_SECONDS}</integer>`),
  );
  assert.match(plist, /<key>Umask<\/key>\s*<integer>63<\/integer>/);
  assert.doesNotMatch(plist, /<key>Umask<\/key>\s*<string>/);
  assert.match(plist, /<key>SoftResourceLimits<\/key>[\s\S]*?<key>Core<\/key>\s*<integer>0<\/integer>/);
  assert.doesNotMatch(
    plist,
    /EnvironmentVariables|TOKEN|CLOUDFLARE|NANOCODEX_CI_ORIGIN|node|ci-controller\.mjs/,
  );
  if (process.platform === "darwin") {
    const converted = spawnSync(
      "/usr/bin/plutil",
      ["-convert", "json", "-o", "-", "-"],
      { input: plist, encoding: "utf8" },
    );
    assert.equal(converted.status, 0, converted.stderr);
    const parsed = JSON.parse(converted.stdout);
    assert.equal(parsed.Umask, 63);
    assert.equal(parsed.Program, "/usr/bin/env");
    assert.deepEqual(parsed.ProgramArguments, ["/usr/bin/env", "-i", master.wrapperPath]);
    assert.equal(parsed.KeepAlive, true);
    assert.equal(parsed.EnvironmentVariables, undefined);
  }
});

test("controller plist clears pre-exec shell and Node injection before the wrapper", async () => {
  const scratch = await canonicalTemporaryDirectory("controller-plist-environment-");
  try {
    const paths = deriveServicePaths(scratch, "master");
    await mkdir(paths.serviceDirectory, { recursive: true });
    const marker = resolve(scratch, "ambient-hook-ran");
    const hook = resolve(scratch, "ambient-hook.sh");
    await writeFile(
      hook,
      `#!/bin/sh\nprintf 'ambient hook ran\\n' > ${JSON.stringify(marker)}\n`,
      { mode: 0o700 },
    );
    await writeFile(
      paths.wrapperPath,
      `#!/bin/sh\n` +
        `if [ -e ${JSON.stringify(marker)} ] || [ "\${NODE_OPTIONS+x}" = x ]; then exit 90; fi\n` +
        `printf 'clean\\n'\n`,
      { mode: 0o700 },
    );
    const plist = renderLaunchAgentPlist(paths, "master", scratch);
    assert.match(
      plist,
      /<key>Program<\/key>\s*<string>\/usr\/bin\/env<\/string>[\s\S]*?<string>-i<\/string>/,
    );
    const result = spawnSync("/usr/bin/env", ["-i", paths.wrapperPath], {
      env: {
        BASH_ENV: hook,
        ENV: hook,
        NODE_OPTIONS: `--require=${resolve(scratch, "ambient-node-hook.cjs")}`,
      },
      encoding: "utf8",
    });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.stdout, "clean\n");
    await assert.rejects(lstat(marker), (error) => error?.code === "ENOENT");
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

test("generated wrappers preserve the empty boundary, load only role Keychain items, probe live env, rotate logs, and add no lock", () => {
  const master = fixtureConfiguration("master");
  const pr = fixtureConfiguration("pr");
  const masterWrapper = renderControllerWrapper(master);
  const prWrapper = renderControllerWrapper(pr);
  const prRuntimeProgram = renderPrPrepRuntimeValidationProgram(pr.prPrep);
  assert.doesNotThrow(() => new Function(prRuntimeProgram));
  assert.match(prRuntimeProgram, /detached:true/);
  assert.match(prRuntimeProgram, /SIGTERM/);
  assert.match(prRuntimeProgram, /SIGKILL/);
  assert.doesNotMatch(prRuntimeProgram, /spawnSync/);
  assert.match(prRuntimeProgram, /nodeParents/);
  assert.match(prRuntimeProgram, new RegExp(String(PR_PREP_NODE_MAX_BYTES)));
  assert.match(prRuntimeProgram, /nodeExpected/);
  assert.equal(
    [...prRuntimeProgram.matchAll(/opened\(e\.nodeBinary/g)].length,
    2,
    "the exact sudo pathname must be hash/identity re-opened before and after sudo",
  );

  for (const wrapper of [masterWrapper, prWrapper]) {
    assert.match(wrapper, /^#!\/bin\/sh\nset -eu\numask 077\n/);
    assert.match(
      wrapper,
      /exec \/usr\/bin\/env -i \/bin\/sh "\$0" '--nanocodex-clean-controller-environment' run/,
    );
    assert.match(wrapper, /readonly NODE_BINARY='\/opt\/trusted\/node'/);
    assert.match(wrapper, /readonly REPOSITORY='\/Users\/ci-controller\/trusted\/nanocodex'/);
    assert.match(wrapper, /readonly CONTROLLER_SCRIPT='\/Users\/ci-controller\/trusted\/nanocodex\/web\/scripts\/ci-(?:pr-)?controller\.mjs'/);
    assert.match(wrapper, /export HOME='\/Users\/ci-controller'/);
    assert.match(wrapper, /readonly MAX_CURRENT_LOG_BYTES=8388608/);
    assert.match(wrapper, /readonly RETAINED_LOG_BYTES=4194304/);
    assert.match(wrapper, /readonly LOG_ROTATION_INTERVAL_SECONDS=30/);
    assert.match(wrapper, /\/usr\/bin\/tail -c "\$RETAINED_LOG_BYTES"/);
    assert.match(wrapper, /stopped after a log path failed its safety check/);
    assert.match(wrapper, /rotate_logs <\/dev\/null >\/dev\/null &/);
    assert.doesNotMatch(wrapper, /rotate_logs <\/dev\/null >\/dev\/null 2>&1 &/);
    assert.match(wrapper, /--eval/);
    assert.match(wrapper, /exec "\$NODE_BINARY" "\$CONTROLLER_SCRIPT"\n$/);
    assert.doesNotMatch(wrapper, /\/usr\/bin\/lockf|\bflock\b|pidfile|\.pid(?:['"\s]|$)/);
    const parsed = spawnSync("/bin/sh", ["-n"], { input: wrapper, encoding: "utf8" });
    assert.equal(parsed.status, 0, parsed.stderr);
  }

  assert.match(masterWrapper, /export CLOUDFLARE_ACCOUNT_ID='0123456789abcdef0123456789abcdef'/);
  assert.match(masterWrapper, /export NANOCODEX_RUSTSEC_REPO='\/Users\/ci-controller\/trusted\/advisory-db'/);
  for (const secretName of ROLE_SECRET_ALLOWLISTS.master) {
    const account = ROLE_KEYCHAIN_ACCOUNTS.master[secretName];
    assert.match(
      masterWrapper,
      new RegExp(`find-generic-password -a '${account}' -s "\\$KEYCHAIN_SERVICE" -w`),
    );
  }
  assert.ok(
    masterWrapper.indexOf("rotate_logs </dev/null") <
      masterWrapper.indexOf("find-generic-password"),
    "the background log monitor must start before any secret is loaded",
  );

  for (const secretName of ROLE_SECRET_ALLOWLISTS.pr) {
    const account = ROLE_KEYCHAIN_ACCOUNTS.pr[secretName];
    assert.match(
      prWrapper,
      new RegExp(`find-generic-password -a '${account}' -s "\\$KEYCHAIN_SERVICE" -w`),
    );
  }
  for (const forbidden of PR_FORBIDDEN_SECRET_KEYS) {
    assert.doesNotMatch(
      prWrapper,
      new RegExp(`find-generic-password -a '${forbidden}'`),
      `${forbidden} must never be read for PR`,
    );
    assert.doesNotMatch(
      prWrapper,
      new RegExp(`export ${forbidden}(?:=|\\s|$)`),
      `${forbidden} must never be exported for PR`,
    );
  }
  assert.match(prWrapper, /"\$NANOCODEX_CI_TOKEN" = "\$NANOCODEX_GITHUB_STATUS_TOKEN"/);
  assert.doesNotMatch(
    masterWrapper,
    /export CI_MASTER_SOURCE_WRITE_TOKEN|export CI_PR_SOURCE_WRITE_TOKEN/,
  );
  assert.doesNotMatch(
    prWrapper,
    /export CI_MASTER_SOURCE_WRITE_TOKEN|export CI_PR_SOURCE_WRITE_TOKEN/,
  );
  assert.doesNotMatch(masterWrapper, /find-generic-password -a 'CI_PR_SOURCE_WRITE_TOKEN'/);
  assert.doesNotMatch(prWrapper, /find-generic-password -a 'CI_MASTER_SOURCE_WRITE_TOKEN'/);
  assert.match(prWrapper, new RegExp(`export NANOCODEX_CI_PR_PREP_USER='${PREP_USERNAME}'`));
  assert.match(prWrapper, /Matching Defaults entries for/);
  assert.match(prWrapper, /assert_runtime_root_node_parent/);
  assert.match(prWrapper, /assert_runtime_root_node_file/);
  assert.match(prWrapper, /\/bin\/ls -lde/);
  assert.match(prWrapper, /for checked_node_parent in '\/' '\/opt' '\/opt\/trusted'/);
  assert.match(prWrapper, /timestamp_timeout/);
  assert.match(prWrapper, /--version.*--verbose/);
  assert.match(prWrapper, new RegExp(PR_PREP_CARGO_PATH.replaceAll(".", "\\.")));
  assert.match(prWrapper, new RegExp(CARGO_SHA256));
  assert.match(prWrapper, new RegExp(PR_PREP_RUSTC_PATH.replaceAll(".", "\\.")));
  assert.match(prWrapper, new RegExp(RUSTC_MANIFEST_SHA256));
  assert.match(prWrapper, /O_NOFOLLOW/);
  assert.match(prWrapper, /\["-vV"\]/);
  assert.match(prWrapper, /libLLVM\.dylib/);
  assert.match(prWrapper, /librustc_driver-/);
  assert.ok(
    prWrapper.indexOf("Matching Defaults entries for") <
      prWrapper.indexOf("find-generic-password"),
    "the complete sudo/account/helper/Cargo/rustc validation must succeed before Keychain",
  );
  assert.match(prWrapper, /credentialEnvironmentNames/);
  assert.match(prWrapper, new RegExp(PR_PREP_HELPER_VERSION.replaceAll(".", "\\.")));
  assert.doesNotMatch(prWrapper, /cargo-home/);
  assert.doesNotMatch(prWrapper, /export CLOUDFLARE_ACCOUNT_ID=/);
  assert.doesNotMatch(prWrapper, /export NANOCODEX_RUSTSEC_REPO=/);
  assert.doesNotMatch(masterWrapper, /CARGO_HOME|NANOCODEX_CI_PR_PREP_USER|PR_PREP_HELPER|\/usr\/bin\/sudo/);
});

test("failed empty-environment preparation probe exits before a credential reader can run", async () => {
  const wrapper = renderControllerWrapper(fixtureConfiguration("pr"));
  const probeIndex = wrapper.indexOf("Matching Defaults entries for");
  const failureIndex = wrapper.indexOf("account, helper, or rustc bundle identity drifted");
  const keychainIndex = wrapper.indexOf("find-generic-password");
  assert.ok(probeIndex >= 0 && probeIndex < failureIndex && failureIndex < keychainIndex);

  const scratch = await canonicalTemporaryDirectory("controller-probe-order-");
  try {
    const marker = resolve(scratch, "events");
    const failedProbe = resolve(scratch, "failed-probe.sh");
    const credentialReader = resolve(scratch, "credential-reader.sh");
    const gate = resolve(scratch, "gate.sh");
    await writeFile(failedProbe, "#!/bin/sh\nprintf 'probe\\n' >> \"$1\"\nexit 1\n", {
      mode: 0o700,
    });
    await writeFile(
      credentialReader,
      "#!/bin/sh\nprintf 'credential-read\\n' >> \"$1\"\n",
      { mode: 0o700 },
    );
    await writeFile(
      gate,
      "#!/bin/sh\nset -eu\nif ! \"$1\" \"$3\"; then exit 78; fi\n\"$2\" \"$3\"\n",
      { mode: 0o700 },
    );
    const result = spawnSync(gate, [failedProbe, credentialReader, marker], {
      env: {},
      encoding: "utf8",
    });
    assert.equal(result.status, 78, result.stderr);
    assert.equal(await readFile(marker, "utf8"), "probe\n");
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

test("PR lifecycle defers co-location Keychain probes until prep and rustc validation", async () => {
  const source = await readFile(
    resolve(TEST_DIRECTORY, "install-ci-controller-service.mjs"),
    "utf8",
  );
  const install = source.slice(
    source.indexOf("async function installOrUpdate("),
    source.indexOf("async function serviceStatus("),
  );
  const installRuntime = install.indexOf("const runtime = await validateTrustedRuntime(");
  const installKeychainAudit = install.lastIndexOf(
    "await assertNoColocatedRole(options.role, context);",
  );
  const installSecretRead = install.indexOf("await keychainItemExists(");
  assert.ok(
    installRuntime >= 0 && installRuntime < installKeychainAudit &&
      installKeychainAudit < installSecretRead,
  );

  const status = source.slice(
    source.indexOf("async function serviceStatus("),
    source.indexOf("async function validateInstallerRepository("),
  );
  const statusRuntime = status.indexOf("prPrep = await validatePrPrepRuntime(");
  const missingBoundary = status.indexOf(
    "PR status refuses Keychain inspection without an installed, validated prep boundary",
  );
  const statusKeychainAudit = status.lastIndexOf(
    "await assertNoColocatedRole(role, context);",
  );
  const statusSecretRead = status.indexOf("const keychainEntries = await Promise.all(");
  assert.ok(
    statusRuntime >= 0 && statusRuntime < missingBoundary &&
      missingBoundary < statusKeychainAudit && statusKeychainAudit < statusSecretRead,
  );
});

test("Keychain and launchctl helpers carry names but never values or controller locks", () => {
  const secretValue = "fixture-value-that-must-not-appear";
  for (const role of ["master", "pr"]) {
    const paths = deriveServicePaths(HOME, role);
    for (const secretName of ROLE_SECRET_ALLOWLISTS[role]) {
      const account = ROLE_KEYCHAIN_ACCOUNTS[role][secretName];
      const store = keychainStoreArguments(role, secretName);
      assert.equal(store.at(-1), "-w");
      assert.ok(!store.includes("-A"));
      assert.ok(!store.includes(secretValue));
      assert.deepEqual(keychainFindArguments(role, secretName), [
        "find-generic-password",
        "-a",
        account,
        "-s",
        `${SERVICE_LABELS[role]}.secrets`,
      ]);
      assert.deepEqual(keychainDeleteArguments(role, secretName), [
        "delete-generic-password",
        "-a",
        account,
        "-s",
        `${SERVICE_LABELS[role]}.secrets`,
      ]);
    }
    assert.deepEqual(launchctlArguments("bootstrap", { uid: 502, role, paths }), [
      "bootstrap",
      "gui/502",
      paths.plistPath,
    ]);
    assert.deepEqual(launchctlArguments("bootout", { uid: 502, role, paths }), [
      "bootout",
      `gui/502/${SERVICE_LABELS[role]}`,
    ]);
    assert.deepEqual(launchctlArguments("kickstart", { uid: 502, role, paths }), [
      "kickstart",
      "-k",
      `gui/502/${SERVICE_LABELS[role]}`,
    ]);
  }
  assert.throws(
    () => keychainStoreArguments("pr", "CLOUDFLARE_API_TOKEN"),
    /not in the pr controller Keychain allowlist/,
  );
  assert.equal(shouldStoreKeychainSecret({
    command: "install",
    replaceSecrets: false,
    present: true,
  }), true, "fresh install must overwrite any stale same-name Keychain item and ACL");
  assert.equal(shouldStoreKeychainSecret({
    command: "update",
    replaceSecrets: false,
    present: true,
  }), false, "ordinary update preserves an existing item");
  assert.equal(shouldStoreKeychainSecret({
    command: "update",
    replaceSecrets: false,
    present: false,
  }), true, "ordinary update repairs a missing item");
  assert.equal(shouldStoreKeychainSecret({
    command: "update",
    replaceSecrets: true,
    present: true,
  }), true, "explicit rotation overwrites an existing item");
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
    /exit 1/,
  );
  assert.throws(
    () => classifyLaunchctlPrint({ code: 77, stderr: "permission denied" }),
    /exit 77/,
  );
  assert.throws(
    () => classifyLaunchctlPrint({
      code: 77,
      stderr: "permission denied; service not found",
    }),
    /exit 77/,
  );
  assert.throws(
    () => classifyLaunchctlPrint({ code: 113, signal: "SIGTERM" }),
    /terminated by SIGTERM/,
  );
  assert.deepEqual(parseLaunchctlPrint({
    code: 0,
    stdout: [
      "gui/502/me.nanocodex.ci-controller.pr = {",
      "\tstate = running",
      "\tpid = 12345",
      "\tlast exit code = (never exited)",
      "\tresource coalition = {",
      "\t\tstate = active",
      "\t}",
      "}",
    ].join("\n"),
  }), {
    loaded: true,
    running: true,
    state: "running",
    pid: 12345,
    lastExitCode: null,
  });
  assert.deepEqual(parseLaunchctlPrint({
    code: 0,
    stdout: [
      "gui/502/me.nanocodex.ci-controller.pr = {",
      "\tstate = waiting",
      "\tlast exit code = 78",
      "}",
    ].join("\n"),
  }), {
    loaded: true,
    running: false,
    state: "waiting",
    pid: null,
    lastExitCode: 78,
  });
  assert.deepEqual(parseLaunchctlPrint({ code: 113 }), {
    loaded: false,
    running: false,
    state: null,
    pid: null,
    lastExitCode: null,
  });
  assert.throws(() => parseLaunchctlPrint({
    code: 0,
    stdout: "\tstate = running\n",
  }), /state and PID do not agree/);
  assert.equal(matchesExpectedControllerCommand(
    "/Applications/Node Runtime/node /Users/CI Operator/trusted/web/scripts/ci-controller.mjs\n",
    "/Applications/Node Runtime/node",
    "/Users/CI Operator/trusted/web/scripts/ci-controller.mjs",
  ), true);
  assert.equal(matchesExpectedControllerCommand(
    "/Applications/Node Runtime/node --eval probe\n",
    "/Applications/Node Runtime/node",
    "/Users/CI Operator/trusted/web/scripts/ci-controller.mjs",
  ), false, "the wrapper or environment-probe process is not controller liveness");
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
    () => classifyKeychainFind({ code: 36, stderr: "interaction is not allowed" }),
    /exit 36/,
  );
  assert.throws(
    () => classifyKeychainFind({
      code: 36,
      stderr: "interaction is not allowed; item not found",
    }),
    /exit 36/,
  );
  assert.throws(
    () => classifyKeychainFind({ code: 44, signal: "SIGKILL" }),
    /terminated by SIGKILL/,
  );
});

test("sudo provisioning embeds pinned bytes and audits the complete effective policy", async () => {
  const node = "/opt/trusted/node";
  assert.deepEqual(prPrepSudoArguments("--probe", PREP_USERNAME, node), [
    "-n",
    "-u",
    PREP_USERNAME,
    "--",
    node,
    PR_PREP_HELPER_PATH,
    "--probe",
  ]);
  assert.deepEqual(prPrepSudoArguments("--build", PREP_USERNAME, node), [
    "-n",
    "-u",
    PREP_USERNAME,
    "--",
    node,
    PR_PREP_HELPER_PATH,
    "--build",
  ]);
  assert.throws(() => prPrepSudoArguments("--shell", PREP_USERNAME, node), /--probe or --build/);

  const rule = renderPrPrepSudoersRule({
    controllerUsername: USERNAME,
    prepUsername: PREP_USERNAME,
    nodeBinary: node,
  });
  assert.equal(
    rule,
    `${USERNAME} ALL=(${PREP_USERNAME}) NOPASSWD:NOSETENV: ` +
      `${node} ${PR_PREP_HELPER_PATH} --probe, ` +
      `${node} ${PR_PREP_HELPER_PATH} --build`,
  );
  assert.match(renderPrPrepSudoersRule({
    controllerUsername: USERNAME,
    prepUsername: PREP_USERNAME,
    nodeBinary: "/Applications/Node Runtime/node",
  }), /\/Applications\/Node\\ Runtime\/node/);
  const helperPath = resolve(REPOSITORY, "web/scripts/ci-pr-cargo-builder.mjs");
  const helperPayload = await captureTrustedPrPrepHelperSource(helperPath, {
    expectedUid: UID,
  });
  assert.equal(helperPayload.sha256, PR_PREP_HELPER_SHA256);
  const provisioning = renderPrPrepProvisioning({
    controllerUsername: USERNAME,
    prepUsername: PREP_USERNAME,
    nodeBinary: node,
    trustedNode: {
      ...fixturePrPrepIdentity().node,
      path: node,
    },
    helperPayload,
    cargoSha256: CARGO_SHA256,
    rustcManifestSha256: RUSTC_MANIFEST_SHA256,
    hostArchitecture: "arm64",
  });
  assert.match(provisioning, /Four identities stay distinct/);
  assert.match(provisioning, /Never reuse a controller, runner, root, admin, guest, or shared account/);
  assert.match(provisioning, /Never chown\/chmod the PR checkout/);
  assert.match(provisioning, new RegExp(PR_PREP_CARGO_PATH.replaceAll(".", "\\.")));
  assert.match(provisioning, new RegExp(PR_PREP_RUSTC_ROOT.replaceAll(".", "\\.")));
  assert.match(provisioning, /Defaults:ci-controller env_reset/);
  assert.match(provisioning, /Defaults:ci-controller !setenv/);
  assert.match(provisioning, /Defaults:ci-controller timestamp_timeout=0/);
  assert.match(provisioning, /\/private\/var\/root\/\.dev\.nanocodex/);
  assert.match(provisioning, /\/usr\/bin\/shasum -a 256 <&3/);
  assert.match(provisioning, /\/usr\/bin\/shasum -a 256 <&5/);
  assert.match(provisioning, /\/usr\/bin\/shasum -a 256 <&6/);
  assert.match(provisioning, /\/usr\/bin\/shasum -a 256 <&7/);
  assert.match(provisioning, new RegExp(PR_PREP_HELPER_SHA256));
  assert.match(provisioning, new RegExp(CARGO_SHA256));
  assert.match(provisioning, new RegExp(RUSTC_MANIFEST_SHA256));
  assert.doesNotMatch(provisioning, new RegExp(helperPath.replaceAll("/", "\\/")));
  assert.equal(provisioning.includes(helperPath), false);
  assert.doesNotMatch(provisioning, /trustedHelperPath|\/Users\/ci-controller\/trusted/);
  assert.match(provisioning, /mktemp '\/Library\/PrivilegedHelperTools\/\.dev\.nanocodex/);
  assert.match(provisioning, /mktemp '\/private\/etc\/sudoers\.d\/\.dev\.nanocodex/);
  assert.match(provisioning, /visudo -cf "\$NANOCODEX_SUDOERS_TMP"/);
  assert.match(provisioning, /\/bin\/mv -f "\$NANOCODEX_SUDOERS_TMP" '\/private\/etc\/sudoers\.d\//);
  assert.doesNotMatch(provisioning, /\/bin\/cat > '\/private\/etc\/sudoers\.d/);
  assert.match(
    provisioning,
    /Uninstall deliberately leaves the account, helper, Cargo, rustc bundle, and sudoers file/,
  );
  assert.deepEqual(prPrepOperatorCleanup(), {
    prepAccountPreserved: true,
    helperPreserved: PR_PREP_HELPER_PATH,
    cargoPreserved: PR_PREP_CARGO_PATH,
    rustcBundlePreserved: PR_PREP_RUSTC_ROOT,
    sudoersPreserved: true,
    instruction:
      "root must separately remove the dedicated account, helper, Cargo, rustc bundle, " +
        "and sudoers rule",
  });
  const syntax = spawnSync("/bin/sh", ["-n"], { input: provisioning, encoding: "utf8" });
  assert.equal(syntax.status, 0, syntax.stderr);
  const encoded = /<<'NANOCODEX_HELPER_PAYLOAD'\n([A-Za-z0-9+/=\n]+)\nNANOCODEX_HELPER_PAYLOAD/.exec(
    provisioning,
  );
  assert.ok(encoded);
  const embedded = Buffer.from(encoded[1].replaceAll("\n", ""), "base64");
  assert.equal(createHash("sha256").update(embedded).digest("hex"), PR_PREP_HELPER_SHA256);
  assert.deepEqual(embedded, helperPayload.bytes);
  const rustcProvisioningProgram = renderPrPrepRustcProvisioningValidationProgram({
    trustedManifestSha256: RUSTC_MANIFEST_SHA256,
    hostArchitecture: "arm64",
  });
  const rustcProgramSyntax = spawnSync(process.execPath, ["--check"], {
    input: rustcProvisioningProgram,
    encoding: "utf8",
  });
  assert.equal(rustcProgramSyntax.status, 0, rustcProgramSyntax.stderr);
  assert.match(rustcProvisioningProgram, /O_NOFOLLOW/);
  assert.match(rustcProvisioningProgram, /-vV/);
  assert.equal(
    [...rustcProvisioningProgram.matchAll(/opened\(root\+"\/"\+files\[i\]\.path/g)].length,
    2,
    "root provisioning must re-open every compiler file after rustc executes",
  );
  const nodeAttestation = "assert_nanocodex_root_node '/opt/trusted/node'";
  const firstNodeAttestation = provisioning.indexOf(nodeAttestation);
  const rustcProbe = provisioning.indexOf("--eval");
  const secondNodeAttestation = provisioning.indexOf(
    nodeAttestation,
    firstNodeAttestation + 1,
  );
  assert.ok(firstNodeAttestation >= 0 && firstNodeAttestation < rustcProbe);
  assert.ok(secondNodeAttestation > rustcProbe);
  assert.throws(
    () => renderPrPrepProvisioning({
      controllerUsername: USERNAME,
      prepUsername: PREP_USERNAME,
      nodeBinary: node,
      trustedNode: { ...fixturePrPrepIdentity().node, path: node, uid: 501 },
      helperPayload,
      cargoSha256: CARGO_SHA256,
      rustcManifestSha256: RUSTC_MANIFEST_SHA256,
      hostArchitecture: "arm64",
    }),
    /captured root-owned Node file identity/,
  );
  assert.match(rustcProvisioningProgram, new RegExp(RUSTC_MANIFEST_SHA256));

  const probeValue = {
    credentialEnvironmentNames: [],
    freshHomePolicy: "per-build-private-temporary",
    gid: PREP_GID,
    helperVersion: PR_PREP_HELPER_VERSION,
    uid: PREP_UID,
    version: 1,
  };
  assert.equal(validatePrPrepProbe(probeValue, { uid: PREP_UID, gid: PREP_GID }), probeValue);
  const policyOutput = sudoPolicyListing(node);
  assert.deepEqual(validatePrPrepSudoPolicy(policyOutput, {
    controllerUsername: USERNAME,
    prepUsername: PREP_USERNAME,
    nodeBinary: node,
  }), {
    completeListing: true,
    helperOnly: true,
    noSetenv: true,
    nopasswd: true,
    timestampTimeoutZero: true,
  });
  const splitPolicy = policyOutput.replace(
    `${node} ${PR_PREP_HELPER_PATH} --probe, ${node} ${PR_PREP_HELPER_PATH} --build`,
    `${node} ${PR_PREP_HELPER_PATH} --probe\n` +
      `    (${PREP_USERNAME}) NOSETENV: NOPASSWD: ` +
      `${node} ${PR_PREP_HELPER_PATH} --build`,
  );
  assert.equal(validatePrPrepSudoPolicy(splitPolicy, {
    controllerUsername: USERNAME,
    prepUsername: PREP_USERNAME,
    nodeBinary: node,
  }).helperOnly, true);
  const denied = () => probeResult({ code: 1, stderr: "sudo: command not allowed\n" });
  const evidence = {
    policy: probeResult({ stdout: policyOutput }),
    probe: probeResult({ stdout: `${JSON.stringify(probeValue)}\n` }),
    build: probeResult({ code: 65, stderr: "ci-pr-cargo-builder: build input is empty\n" }),
  };
  const sudoContext = {
    uid: PREP_UID,
    gid: PREP_GID,
    controllerUsername: USERNAME,
    prepUsername: PREP_USERNAME,
    nodeBinary: node,
  };
  assert.deepEqual(validatePrPrepSudoEvidence(evidence, sudoContext), {
    probe: true,
    build: true,
    completeListing: true,
    helperOnly: true,
    noSetenv: true,
    nopasswd: true,
    timestampTimeoutZero: true,
  });

  const missing = structuredClone(evidence);
  missing.probe = denied();
  assert.throws(
    () => validatePrPrepSudoEvidence(missing, sudoContext),
    /allow the exact passwordless/,
  );
  for (const [name, output] of [
    ["extra command", policyOutput.replace(/\n$/, `, /bin/sh\n`)],
    ["extra grant", `${policyOutput}    (root) NOPASSWD: NOSETENV: /usr/bin/id\n`],
    ["wrong runas", policyOutput.replace(`(${PREP_USERNAME})`, "(root)")],
    ["SETENV", policyOutput.replace("NOSETENV:", "SETENV:")],
    ["PASSWD", policyOutput.replace("NOPASSWD:", "PASSWD:")],
    ["non-ASCII listing", policyOutput.replace("mac-builder", "mac-\ufffdbuilder")],
    ["missing timeout", policyOutput.replace(", timestamp_timeout=0", "")],
    ["cached timeout", policyOutput.replace("timestamp_timeout=0", "timestamp_timeout=15")],
  ]) {
    const changed = structuredClone(evidence);
    changed.policy.stdout = output;
    assert.throws(
      () => validatePrPrepSudoEvidence(changed, sudoContext),
      /sudo|policy|timestamp|runas|command|tag|grant/,
      name,
    );
  }
  const noisyPolicy = structuredClone(evidence);
  noisyPolicy.policy.stderr = "sudo: warning\n";
  assert.throws(
    () => validatePrPrepSudoEvidence(noisyPolicy, sudoContext),
    /succeed quietly/,
  );
  const missingBuild = structuredClone(evidence);
  missingBuild.build = denied();
  assert.throws(
    () => validatePrPrepSudoEvidence(missingBuild, sudoContext),
    /allow exact --build/,
  );
  const noncanonical = structuredClone(evidence);
  noncanonical.probe.stdout = ` ${JSON.stringify(probeValue)}\n`;
  assert.throws(
    () => validatePrPrepSudoEvidence(noncanonical, sudoContext),
    /canonical JSON/,
  );
});

test("filesystem identity checks reject wrong owners, writable files, and symlinks", async () => {
  const scratch = await canonicalTemporaryDirectory("controller-entry-");
  try {
    const file = resolve(scratch, "safe-file");
    await writeFile(file, "safe", { mode: 0o600 });
    assert.deepEqual(
      await assertSafeOwnedEntry(file, { uid: UID, kind: "file", exactMode: 0o600 }),
      { path: file, uid: UID, mode: 0o600, nlink: 1 },
    );
    await chmod(file, 0o666);
    await assert.rejects(
      assertSafeOwnedEntry(file, { uid: UID, kind: "file" }),
      /group or world writable/,
    );
    await chmod(file, 0o600);
    await chmod(file, 0o4600);
    await assert.rejects(
      assertSafeOwnedEntry(file, { uid: UID, kind: "file" }),
      /forbidden special mode bits/,
    );
    await chmod(file, 0o600);
    await assert.rejects(
      assertSafeOwnedEntry(file, { uid: UID + 1, kind: "file" }),
      /wrong user/,
    );
    const link = resolve(scratch, "link");
    await symlink(file, link);
    await assert.rejects(
      assertSafeOwnedEntry(link, { uid: UID, kind: "file" }),
      /symbolic link/,
    );
    const directory = resolve(scratch, "private");
    await mkdir(directory, { mode: 0o700 });
    await assertSafeOwnedEntry(directory, {
      uid: UID,
      kind: "directory",
      exactMode: 0o700,
    });
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

test("privileged prep Node pins a root-owned no-symlink, no-ACL, non-writable ancestor chain", () => {
  const safe = fixtureNodeSnapshot();
  assert.deepEqual(validatePrPrepNodeSnapshot(safe, "/opt/trusted/node"),
    fixturePrPrepIdentity().node);
  assert.equal(validateNoAclListing("-rwxr-xr-x  1 root  wheel  1024 Aug 22 00:00 node\n"), true);
  assert.equal(validateNoAclListing("-rwxr-xr-x@ 1 root  wheel  1024 Aug 22 00:00 node\n"), true);
  assert.throws(
    () => validateNoAclListing(
      "-rwxr-xr-x+ 1 root wheel 1024 Aug 22 00:00 node\n" +
        " 0: user:prep allow write\n",
    ),
    /ACL|ambiguous/,
  );
  for (const [name, mutate] of [
    ["writable ancestor", (value) => { value.parents[1].mode = 0o775; }],
    ["symlink ancestor", (value) => { value.parents[2].symbolicLink = true; }],
    ["aliased ancestor", (value) => { value.parents[2].canonicalPath = "/tmp/trusted"; }],
    ["controller-owned ancestor", (value) => { value.parents[1].uid = 501; }],
    ["shared-group ancestor", (value) => { value.parents[1].gid = 20; }],
    ["ancestor ACL", (value) => { value.parents[1].accessControlList = true; }],
    ["controller-owned executable", (value) => { value.node.uid = 501; }],
    ["shared-group executable", (value) => { value.node.gid = 20; }],
    ["writable executable", (value) => { value.node.mode = 0o775; }],
    ["prep cannot execute", (value) => { value.node.mode = 0o750; }],
    ["symlink executable", (value) => { value.node.symbolicLink = true; }],
    ["hard-linked executable", (value) => { value.node.nlink = 2; }],
    ["oversized executable", (value) => { value.node.size = PR_PREP_NODE_MAX_BYTES + 1; }],
    ["executable ACL", (value) => { value.node.accessControlList = true; }],
  ]) {
    const changed = structuredClone(safe);
    mutate(changed);
    assert.throws(
      () => validatePrPrepNodeSnapshot(changed, "/opt/trusted/node"),
      /Node|root-owned|no-ACL/,
      name,
    );
  }
});

test("privileged prep Node capture binds one O_NOFOLLOW fd across path swaps", async () => {
  const scratch = await canonicalTemporaryDirectory("controller-node-capture-");
  try {
    const node = resolve(scratch, "node");
    const replacement = resolve(scratch, "replacement");
    const original = Buffer.from("fixture trusted node AAAA\n");
    const changed = Buffer.from("fixture trusted node BBBB\n");
    assert.equal(original.length, changed.length);
    await writeFile(node, original, { mode: 0o755 });
    const captured = await captureTrustedPrPrepNodeExecutable(node, {
      expectedUid: UID,
      expectedGid: GID,
    });
    assert.equal(captured.sha256, createHash("sha256").update(original).digest("hex"));

    const link = resolve(scratch, "node-link");
    await symlink(node, link);
    await assert.rejects(
      captureTrustedPrPrepNodeExecutable(link, { expectedUid: UID, expectedGid: GID }),
      /ELOOP|symbolic|safe helper reads/,
    );

    await writeFile(replacement, changed, { mode: 0o755 });
    await assert.rejects(
      captureTrustedPrPrepNodeExecutable(node, {
        expectedUid: UID,
        expectedGid: GID,
        afterOpen: async () => rename(replacement, node),
      }),
      /path changed|identity while read/,
    );

    await writeFile(node, original, { mode: 0o755 });
    await assert.rejects(
      captureTrustedPrPrepNodeExecutable(node, {
        expectedUid: UID,
        expectedGid: GID,
        afterOpen: async () => writeFile(node, changed, { mode: 0o755 }),
      }),
      /changed identity|changed size/,
    );
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

test("fixed privileged helper and every parent fail closed on path, owner, mode, link, size, and hash drift", () => {
  const trustedHash = "b".repeat(64);
  const safe = fixtureHelperSnapshot(trustedHash);
  assert.deepEqual(validatePrPrepHelperSnapshot(safe, trustedHash), {
    path: PR_PREP_HELPER_PATH,
    device: 1,
    inode: 42,
    size: 4096,
    sha256: trustedHash,
    uid: 0,
    gid: 0,
    mode: 0o555,
    nlink: 1,
  });
  for (const [name, mutate] of [
    ["helper symlink", (value) => { value.helper.symbolicLink = true; }],
    ["helper setuid", (value) => { value.helper.specialMode = 0o4000; }],
    ["helper writable", (value) => { value.helper.mode = 0o755; }],
    ["helper wrong owner", (value) => { value.helper.uid = 501; }],
    ["helper wrong group", (value) => { value.helper.gid = 20; }],
    ["helper hard link", (value) => { value.helper.nlink = 2; }],
    ["helper oversized", (value) => { value.helper.size = 1024 * 1024 + 1; }],
    ["helper wrong hash", (value) => { value.helper.sha256 = "c".repeat(64); }],
    ["parent symlink", (value) => { value.parents[1].symbolicLink = true; }],
    ["parent special mode", (value) => { value.parents[1].specialMode = 0o1000; }],
    ["parent writable", (value) => { value.parents[2].mode = 0o775; }],
    ["parent wrong owner", (value) => { value.parents[0].uid = 501; }],
    ["parent wrong group", (value) => { value.parents[0].gid = 20; }],
    ["parent canonical alias", (value) => { value.parents[2].canonicalPath = "/tmp/alias"; }],
  ]) {
    const changed = structuredClone(safe);
    mutate(changed);
    assert.throws(
      () => validatePrPrepHelperSnapshot(changed, trustedHash),
      /helper|root-owned/,
      name,
    );
  }
  assert.throws(
    () => validatePrPrepHelperSnapshot(safe, "d".repeat(64)),
    /exact bounded root-owned/,
  );
});

test("trusted helper capture binds one O_NOFOLLOW fd across symlink, path, and same-size swaps", async () => {
  const scratch = await canonicalTemporaryDirectory("controller-helper-capture-");
  try {
    const source = resolve(scratch, "helper.mjs");
    const replacement = resolve(scratch, "replacement.mjs");
    const original = Buffer.from("export const helper = 'AAAA';\n");
    const changed = Buffer.from("export const helper = 'BBBB';\n");
    assert.equal(original.length, changed.length);
    const trustedSha256 = createHash("sha256").update(original).digest("hex");
    await writeFile(source, original, { mode: 0o600 });
    const captured = await captureTrustedPrPrepHelperSource(source, {
      expectedUid: UID,
      trustedSha256,
    });
    assert.deepEqual(captured.bytes, original);

    let reachedPostFstatHook = false;
    await chmod(source, 0o666);
    await assert.rejects(
      captureTrustedPrPrepHelperSource(source, {
        expectedUid: UID,
        trustedSha256,
        afterOpen: async () => { reachedPostFstatHook = true; },
      }),
      /changed identity|size\/link bound/,
    );
    assert.equal(reachedPostFstatHook, false, "unsafe source bytes must fail before the read hook");
    await chmod(source, 0o600);

    const link = resolve(scratch, "helper-link.mjs");
    await symlink(source, link);
    await assert.rejects(
      captureTrustedPrPrepHelperSource(link, { expectedUid: UID, trustedSha256 }),
      /ELOOP|symbolic|regular file|safe helper reads/,
    );

    await writeFile(replacement, changed, { mode: 0o600 });
    await assert.rejects(
      captureTrustedPrPrepHelperSource(source, {
        expectedUid: UID,
        trustedSha256,
        afterOpen: async () => rename(replacement, source),
      }),
      /path changed|identity while read/,
    );

    await writeFile(source, original, { mode: 0o600 });
    await assert.rejects(
      captureTrustedPrPrepHelperSource(source, {
        expectedUid: UID,
        trustedSha256,
        afterOpen: async () => writeFile(source, changed, { mode: 0o600 }),
      }),
      /changed identity|pinned bounded/,
    );
    await writeFile(source, changed, { mode: 0o600 });
    await assert.rejects(
      captureTrustedPrPrepHelperSource(source, { expectedUid: UID, trustedSha256 }),
      /pinned bounded/,
    );
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

test("fixed Cargo attestation pins root chain, executable bytes, release, and host", () => {
  assert.deepEqual(validateCargoVersionOutput(CARGO_VERSION_OUTPUT, "arm64"), {
    release: PR_PREP_CARGO_RELEASE,
    host: "aarch64-apple-darwin",
    output: CARGO_VERSION_OUTPUT,
  });
  const safe = fixtureCargoSnapshot(CARGO_SHA256);
  assert.deepEqual(
    validatePrPrepCargoSnapshot(safe, CARGO_SHA256, CARGO_VERSION_OUTPUT, "arm64"),
    fixturePrPrepIdentity().cargo,
  );
  for (const [name, mutate] of [
    ["symlink", (value) => { value.cargo.symbolicLink = true; }],
    ["setuid", (value) => { value.cargo.specialMode = 0o4000; }],
    ["wrong uid", (value) => { value.cargo.uid = 501; }],
    ["wrong gid", (value) => { value.cargo.gid = 20; }],
    ["hard link", (value) => { value.cargo.nlink = 2; }],
    ["not executable", (value) => { value.cargo.mode = 0o444; }],
    ["writable", (value) => { value.cargo.mode = 0o575; }],
    ["hash", (value) => { value.cargo.sha256 = "d".repeat(64); }],
    ["parent gid", (value) => { value.parents[2].gid = 20; }],
    ["parent special mode", (value) => { value.parents[2].specialMode = 0o1000; }],
    ["parent writable", (value) => { value.parents[1].mode = 0o775; }],
  ]) {
    const changed = structuredClone(safe);
    mutate(changed);
    assert.throws(
      () => validatePrPrepCargoSnapshot(
        changed,
        CARGO_SHA256,
        CARGO_VERSION_OUTPUT,
        "arm64",
      ),
      /Cargo|root-owned/,
      name,
    );
  }
  assert.throws(
    () => validateCargoVersionOutput(
      CARGO_VERSION_OUTPUT.replace(`release: ${PR_PREP_CARGO_RELEASE}`, "release: 1.97.0"),
      "arm64",
    ),
    /version identity/,
  );
  assert.throws(
    () => validateCargoVersionOutput(
      CARGO_VERSION_OUTPUT.replace("aarch64-apple-darwin", "x86_64-apple-darwin"),
      "arm64",
    ),
    /host must be exactly/,
  );
});

test("fixed rustc bundle pins canonical manifest, exact entries, every file, and -vV identity", () => {
  assert.deepEqual(
    parsePrPrepRustcManifest(RUSTC_MANIFEST_TEXT, RUSTC_MANIFEST_SHA256, "arm64"),
    {
      ...RUSTC_MANIFEST_VALUE,
      files: RUSTC_MANIFEST_VALUE.files,
      sha256: RUSTC_MANIFEST_SHA256,
      text: RUSTC_MANIFEST_TEXT,
    },
  );
  assert.deepEqual(validateRustcVersionOutput(RUSTC_VERSION_OUTPUT, "arm64"), {
    release: PR_PREP_RUSTC_RELEASE,
    host: "aarch64-apple-darwin",
    llvmVersion: "22.1.8",
    output: RUSTC_VERSION_OUTPUT,
  });
  const x64ManifestValue = {
    ...RUSTC_MANIFEST_VALUE,
    host: "x86_64-apple-darwin",
  };
  const x64ManifestText = JSON.stringify(x64ManifestValue) + "\n";
  assert.equal(
    parsePrPrepRustcManifest(
      x64ManifestText,
      createHash("sha256").update(x64ManifestText).digest("hex"),
      "x64",
    ).host,
    "x86_64-apple-darwin",
  );
  assert.equal(
    validateRustcVersionOutput(
      RUSTC_VERSION_OUTPUT.replace("aarch64-apple-darwin", "x86_64-apple-darwin"),
      "x64",
    ).host,
    "x86_64-apple-darwin",
  );
  const safe = fixtureRustcBundleSnapshot();
  assert.deepEqual(
    validatePrPrepRustcBundleSnapshot(
      safe,
      RUSTC_MANIFEST_SHA256,
      RUSTC_VERSION_OUTPUT,
      "arm64",
    ),
    fixtureRustcBundleIdentity(),
  );
  for (const [name, mutate] of [
    ["extra root entry", (value) => { value.entries.root.push("mutable"); }],
    ["second driver", (value) => { value.entries.lib.push("librustc_driver-deadbeef.dylib"); }],
    ["writable root", (value) => { value.directories.root.mode = 0o755; }],
    ["wrong non-writable root mode", (value) => { value.directories.root.mode = 0o545; }],
    ["directory ACL", (value) => { value.directories.lib.accessControlList = true; }],
    ["manifest symlink", (value) => { value.manifest.symbolicLink = true; }],
    ["manifest hard link", (value) => { value.manifest.nlink = 2; }],
    ["manifest owner", (value) => { value.manifest.uid = 501; }],
    ["manifest writable", (value) => { value.manifest.mode = 0o644; }],
    ["wrong non-writable manifest mode", (value) => { value.manifest.mode = 0o404; }],
    ["compiler not executable", (value) => { value.files[0].mode = 0o444; }],
    ["wrong non-writable compiler mode", (value) => { value.files[0].mode = 0o515; }],
    ["library writable", (value) => { value.files[1].mode = 0o644; }],
    ["driver hash", (value) => { value.files[2].sha256 = "4".repeat(64); }],
    ["parent ACL", (value) => { value.parents[1].accessControlList = true; }],
  ]) {
    const changed = structuredClone(safe);
    mutate(changed);
    assert.throws(
      () => validatePrPrepRustcBundleSnapshot(
        changed,
        RUSTC_MANIFEST_SHA256,
        RUSTC_VERSION_OUTPUT,
        "arm64",
      ),
      /rustc|manifest|compiler file set|root-owned/i,
      name,
    );
  }
  const reordered = JSON.stringify({
    release: PR_PREP_RUSTC_RELEASE,
    version: 1,
    host: "aarch64-apple-darwin",
    files: RUSTC_MANIFEST_VALUE.files,
  }) + "\n";
  assert.throws(
    () => parsePrPrepRustcManifest(
      reordered,
      createHash("sha256").update(reordered).digest("hex"),
      "arm64",
    ),
    /canonical release, host, or shape/,
  );
  assert.throws(
    () => parsePrPrepRustcManifest(
      RUSTC_MANIFEST_TEXT,
      "f".repeat(64),
      "arm64",
    ),
    /reviewed SHA-256/,
  );
  assert.throws(
    () => validateRustcVersionOutput(
      RUSTC_VERSION_OUTPUT.replace("aarch64-apple-darwin", "x86_64-apple-darwin"),
      "arm64",
    ),
    /canonical release/,
  );
});

test("recorded PR preparation identity detects status/update rustc and existing boundary drift", () => {
  const installed = fixturePrPrepIdentity();
  assert.deepEqual(validateRecordedPrPrepIdentity(installed), installed);
  assert.deepEqual(assertPrPrepIdentityUnchanged(installed, installed), installed);
  for (const [name, mutate] of [
    ["username", (value) => { value.username = "_different_prep"; }],
    ["uid", (value) => { value.uid += 1; }],
    ["gid", (value) => { value.gid += 1; }],
    ["helper inode", (value) => { value.helper.inode += 1; }],
    ["helper size", (value) => { value.helper.size += 1; }],
    ["helper hash", (value) => { value.helper.sha256 = "d".repeat(64); }],
    ["helper owner", (value) => { value.helper.uid = 501; }],
    ["helper mode", (value) => { value.helper.mode = 0o755; }],
    ["helper nlink", (value) => { value.helper.nlink = 2; }],
    ["Cargo inode", (value) => { value.cargo.inode += 1; }],
    ["Cargo hash", (value) => { value.cargo.sha256 = "d".repeat(64); }],
    ["Cargo owner", (value) => { value.cargo.gid = 20; }],
    ["Cargo version", (value) => { value.cargo.release = "1.97.0"; }],
    ["rustc root inode", (value) => { value.rustc.root.inode += 1; }],
    ["rustc manifest hash", (value) => { value.rustc.manifest.sha256 = "4".repeat(64); }],
    ["rustc compiler inode", (value) => { value.rustc.files[0].inode += 1; }],
    ["rustc LLVM writable", (value) => { value.rustc.files[1].mode = 0o644; }],
    ["rustc driver hash", (value) => { value.rustc.files[2].sha256 = "5".repeat(64); }],
    ["rustc host", (value) => { value.rustc.host = "x86_64-apple-darwin"; }],
    ["Node inode", (value) => { value.node.inode += 1; }],
    ["Node hash", (value) => { value.node.sha256 = "f".repeat(64); }],
    ["Node owner", (value) => { value.node.uid = 501; }],
    ["Node group", (value) => { value.node.gid = 20; }],
    ["Node mode", (value) => { value.node.mode = 0o775; }],
    ["Node hard link", (value) => { value.node.nlink = 2; }],
    ["primary group", (value) => { value.primaryGroupName = "staff"; }],
    ["GeneratedUID", (value) => { value.generatedUid = "invalid"; }],
    ["Node path", (value) => { value.nodeBinary = "/opt/other/node"; }],
    ["Node identity", (value) => { value.nodeIdentity = "node/darwin/x64"; }],
    ["sudo probe", (value) => { value.sudoProbe.probe = false; }],
    ["sudo timeout", (value) => { value.sudoProbe.timestampTimeoutZero = false; }],
  ]) {
    const changed = structuredClone(installed);
    mutate(changed);
    assert.throws(
      () => assertPrPrepIdentityUnchanged(changed, installed),
      /drifted|invalid|incomplete|root or the controller UID|dedicated-group boundary|same-name|Cargo|rustc|Node/,
      name,
    );
  }

  const fixed = {
    HOME,
    PATH: "/usr/bin:/bin",
    TMPDIR: "/Users/ci-controller/tmp",
    LANG: "C",
    SHLVL: "0",
    __CF_USER_TEXT_ENCODING: `0x${UID.toString(16).toUpperCase()}:0x0:0x0`,
    NANOCODEX_CI_ORIGIN: "https://ci.example.test",
    NANOCODEX_REPO: "/Users/ci-controller/trusted/nanocodex",
    NANOCODEX_CI_PR_STATE_PATH: "/Users/ci-controller/state.json",
    NANOCODEX_CI_PR_TMPDIR: "/Users/ci-controller/tmp",
    NANOCODEX_CI_PR_PREP_USER: PREP_USERNAME,
  };
  const probe = {
    version: 1,
    role: "pr",
    identity: "node/darwin/arm64",
    nodeBinary: "/opt/trusted/node",
    controllerScript: "/Users/ci-controller/trusted/nanocodex/web/scripts/ci-pr-controller.mjs",
    fixed,
    credentialKeys: [],
    deniedKeys: [],
    prPrep: installed,
    sudoProbe: true,
  };
  assert.equal(validateEnvironmentProbe(probe, {
    role: "pr",
    architecture: "arm64",
    expected: { NANOCODEX_CI_PR_PREP_USER: PREP_USERNAME },
  }), probe);
  assert.throws(
    () => validateEnvironmentProbe({
      ...probe,
      fixed: { ...fixed, NANOCODEX_CI_PR_PREP_USER: "_wrong_prep" },
    }, { role: "pr", architecture: "arm64" }),
    /username does not match/,
  );
});

test("trusted Node live probe admits exactly each role's secret names", {
  skip: process.platform !== "darwin",
}, () => {
  const commonEnvironment = {
    HOME,
    PATH: "/usr/bin:/bin",
    TMPDIR: "/Users/ci-controller/tmp",
    LANG: "C",
    SHLVL: "0",
    __CF_USER_TEXT_ENCODING: `0x${UID.toString(16).toUpperCase()}:0x0:0x0`,
    NANOCODEX_CI_ORIGIN: "https://ci.example.test",
    NANOCODEX_REPO: "/Users/ci-controller/trusted/nanocodex",
  };
  const fixedEnvironment = {
    ...commonEnvironment,
    NANOCODEX_CI_PR_STATE_PATH: "/Users/ci-controller/state.json",
    NANOCODEX_CI_PR_TMPDIR: "/Users/ci-controller/tmp",
    NANOCODEX_CI_PR_PREP_USER: PREP_USERNAME,
  };
  const program = renderLiveEnvironmentProbeProgram({
    role: "pr",
    architecture: arch(),
    fixedEnvironment,
  });
  const clean = {
    ...fixedEnvironment,
    NANOCODEX_CI_TOKEN: "source-value",
    NANOCODEX_GITHUB_STATUS_TOKEN: "github-value",
  };
  const accepted = spawnSync(process.execPath, ["--eval", program], {
    env: clean,
    encoding: "utf8",
  });
  assert.equal(accepted.status, 0, accepted.stderr);
  const forbidden = spawnSync(process.execPath, ["--eval", program], {
    env: { ...clean, CLOUDFLARE_API_TOKEN: "deploy-value" },
    encoding: "utf8",
  });
  assert.equal(forbidden.status, 78);
  assert.doesNotMatch(forbidden.stderr, /deploy-value/);
  for (const namedCapability of Object.values(ROLE_SOURCE_CAPABILITIES)) {
    const namedLeak = spawnSync(process.execPath, ["--eval", program], {
      env: { ...clean, [namedCapability]: "named-capability-value" },
      encoding: "utf8",
    });
    assert.equal(namedLeak.status, 78);
    assert.doesNotMatch(namedLeak.stderr, /named-capability-value/);
  }
  const sharedAuthority = spawnSync(process.execPath, ["--eval", program], {
    env: {
      ...clean,
      NANOCODEX_GITHUB_STATUS_TOKEN: clean.NANOCODEX_CI_TOKEN,
    },
    encoding: "utf8",
  });
  assert.equal(sharedAuthority.status, 78);
  assert.doesNotMatch(sharedAuthority.stderr, /source-value/);
  const unknown = spawnSync(process.execPath, ["--eval", program], {
    env: { ...clean, UNLISTED_PRIVATE_KEY: "ambient-value" },
    encoding: "utf8",
  });
  assert.equal(unknown.status, 78);
  assert.doesNotMatch(unknown.stderr, /ambient-value/);
  const benignAmbient = spawnSync(process.execPath, ["--eval", program], {
    env: { ...clean, UNLISTED_BENIGN: "ambient-value" },
    encoding: "utf8",
  });
  assert.equal(benignAmbient.status, 78);
  assert.doesNotMatch(benignAmbient.stderr, /ambient-value/);

  const masterFixed = {
    ...commonEnvironment,
    NANOCODEX_WEB_ORIGIN: "https://www.example.test",
    NANOCODEX_RUSTSEC_REPO: "/Users/ci-controller/trusted/advisory-db",
    CLOUDFLARE_ACCOUNT_ID: ACCOUNT_ID,
  };
  const masterProgram = renderLiveEnvironmentProbeProgram({
    role: "master",
    architecture: arch(),
    fixedEnvironment: masterFixed,
  });
  const masterEnvironment = {
    ...masterFixed,
    NANOCODEX_CI_TOKEN: "source-value",
    NANOCODEX_GITHUB_STATUS_TOKEN: "github-value",
    CLOUDFLARE_API_TOKEN: "cloudflare-value",
    NANOCODEX_GIT_TOKEN: "mirror-value",
  };
  const masterAccepted = spawnSync(process.execPath, ["--eval", masterProgram], {
    env: masterEnvironment,
    encoding: "utf8",
  });
  assert.equal(masterAccepted.status, 0, masterAccepted.stderr);
  const masterRegistryLeak = spawnSync(process.execPath, ["--eval", masterProgram], {
    env: { ...masterEnvironment, CARGO_REGISTRY_TOKEN: "cargo-value" },
    encoding: "utf8",
  });
  assert.equal(masterRegistryLeak.status, 78);
  assert.doesNotMatch(masterRegistryLeak.stderr, /cargo-value/);
  for (const namedCapability of Object.values(ROLE_SOURCE_CAPABILITIES)) {
    const namedLeak = spawnSync(process.execPath, ["--eval", masterProgram], {
      env: { ...masterEnvironment, [namedCapability]: "named-capability-value" },
      encoding: "utf8",
    });
    assert.equal(namedLeak.status, 78);
    assert.doesNotMatch(namedLeak.stderr, /named-capability-value/);
  }
});

test("generated master wrapper probe live-scrubs a contaminated environment without executing checkout code", {
  skip: process.platform !== "darwin",
}, async () => {
  const scratch = await canonicalTemporaryDirectory("controller-wrapper-");
  try {
    const home = resolve(scratch, "home");
    const paths = deriveServicePaths(home, "master");
    await mkdir(paths.temporaryDirectory, { recursive: true, mode: 0o700 });
    await chmod(paths.temporaryDirectory, 0o700);
    const rustSecRepository = resolve(scratch, "advisory-db");
    await mkdir(rustSecRepository, { mode: 0o700 });
    const repository = await realpath(REPOSITORY);
    const controllerScript = controllerScriptPath(repository, "master");
    const [nodeStat, repositoryStat, controllerStat, rustSecStat] = await Promise.all([
      lstat(process.execPath),
      lstat(repository),
      lstat(controllerScript),
      lstat(rustSecRepository),
    ]);
    const wrapper = renderControllerWrapper({
      role: "master",
      paths,
      nodeBinary: process.execPath,
      repository,
      controllerScript,
      origin: "https://ci.example.test",
      webOrigin: "https://www.example.test",
      rustSecRepository,
      cloudflareAccountId: ACCOUNT_ID,
      uid: UID,
      username: USERNAME,
      architecture: arch(),
      metadata: {
        node: recorded(nodeStat),
        repository: recorded(repositoryStat),
        controller: recorded(controllerStat),
        rustSec: recorded(rustSecStat),
      },
    });
    const wrapperPath = resolve(scratch, "wrapper.sh");
    await writeFile(wrapperPath, wrapper, { mode: 0o700 });
    await chmod(wrapperPath, 0o700);
    const result = await runGeneratedWrapperProbe(wrapperPath, {
      role: "master",
      architecture: arch(),
      expected: {
        HOME: home,
        TMPDIR: paths.temporaryDirectory,
        NANOCODEX_CI_ORIGIN: "https://ci.example.test",
        NANOCODEX_REPO: repository,
      },
    });
    assert.equal(result.role, "master");
    assert.deepEqual(result.credentialKeys, []);
    assert.deepEqual(result.deniedKeys, []);

    const nodeLink = resolve(scratch, "node-link");
    await symlink(process.execPath, nodeLink);
    const unsafeWrapper = renderControllerWrapper({
      role: "master",
      paths,
      nodeBinary: nodeLink,
      repository,
      controllerScript,
      origin: "https://ci.example.test",
      webOrigin: "https://www.example.test",
      rustSecRepository,
      cloudflareAccountId: ACCOUNT_ID,
      uid: UID,
      username: USERNAME,
      architecture: arch(),
      metadata: {
        node: recorded(nodeStat),
        repository: recorded(repositoryStat),
        controller: recorded(controllerStat),
        rustSec: recorded(rustSecStat),
      },
    });
    const unsafePath = resolve(scratch, "unsafe-wrapper.sh");
    await writeFile(unsafePath, unsafeWrapper, { mode: 0o700 });
    await chmod(unsafePath, 0o700);
    await assert.rejects(
      runGeneratedWrapperProbe(unsafePath, { role: "master", architecture: arch() }),
      /Node path failed its identity check/,
    );
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

test("probe/tool runner reaps a real TERM-ignoring pipe-holding descendant and rejects successful stderr", {
  skip: process.platform === "win32",
}, async () => {
  const scratch = await canonicalTemporaryDirectory("controller-process-group-");
  try {
    const ready = resolve(scratch, "ready");
    const descendant = `const fs=require("node:fs");` +
      `process.on("SIGTERM",()=>{});fs.writeFileSync(${JSON.stringify(ready)},"ready");` +
      `setInterval(()=>{},1000);`;
    const leader = `const fs=require("node:fs"),{spawn}=require("node:child_process");` +
      `const c=spawn(process.execPath,["--eval",${JSON.stringify(descendant)}],` +
      `{stdio:["ignore","inherit","inherit"]});` +
      `const timer=setInterval(()=>{if(fs.existsSync(${JSON.stringify(ready)})){` +
      `clearInterval(timer);c.unref();process.stdout.write(String(c.pid)+"\\n");` +
      `process.exit(0)}},5);`;
    const started = Date.now();
    const result = await runOwnedProcessProbe(process.execPath, ["--eval", leader], {
      timeoutMs: 10_000,
    });
    const elapsed = Date.now() - started;
    assert.equal(result.code, 0);
    assert.equal(result.stderr, "");
    assert.ok(elapsed >= 1_000, `TERM grace was only ${elapsed}ms`);
    const descendantPid = Number(result.stdout.trim());
    assert.ok(Number.isSafeInteger(descendantPid) && descendantPid > 0);
    assert.throws(
      () => process.kill(descendantPid, 0),
      (cause) => cause?.code === "ESRCH",
    );

    await assert.rejects(
      runOwnedProcessProbe(process.execPath, [
        "--eval",
        `process.stderr.write("unexpected successful warning\\n")`,
      ]),
      /succeeded with forbidden stderr/,
    );
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

test("probe result validation and runbooks fail closed without exposing secret values", () => {
  const fixed = {
    HOME,
    PATH: "/usr/bin:/bin",
    TMPDIR: "/Users/ci-controller/tmp",
    LANG: "C",
    NANOCODEX_CI_ORIGIN: "https://ci.example.test",
    NANOCODEX_REPO: "/Users/ci-controller/trusted/nanocodex",
  };
  const valid = {
    version: 1,
    role: "master",
    identity: "node/darwin/arm64",
    nodeBinary: "/opt/trusted/node",
    controllerScript: "/Users/ci-controller/trusted/nanocodex/web/scripts/ci-controller.mjs",
    fixed,
    credentialKeys: [],
    deniedKeys: [],
  };
  assert.equal(
    validateEnvironmentProbe(valid, {
      role: "master",
      architecture: "arm64",
      expected: { HOME },
    }),
    valid,
  );
  assert.throws(
    () => validateEnvironmentProbe({
      ...valid,
      credentialKeys: ["CARGO_REGISTRY_TOKEN"],
    }, { role: "master", architecture: "arm64" }),
    /failed closed/,
  );
  assert.throws(
    () => validateEnvironmentProbe({
      ...valid,
      controllerScript: "/tmp/controller.mjs",
    }, { role: "master", architecture: "arm64" }),
    /wrong script path/,
  );

  const masterRunbook = serviceRunbook("master");
  const prRunbook = serviceRunbook("pr");
  assert.match(masterRunbook, /passwd-backed home/);
  assert.match(masterRunbook, /master and PR require distinct users/);
  assert.match(masterRunbook, /Four OS identities remain distinct/);
  assert.match(masterRunbook, /Never share a writable group/);
  assert.match(masterRunbook, /kernel-held lock/);
  assert.match(masterRunbook, /no token is written to plist, argv, or a file/);
  assert.match(masterRunbook, /update --replace-secrets/);
  assert.match(
    masterRunbook,
    /CI_MASTER_SOURCE_WRITE_TOKEN as NANOCODEX_CI_TOKEN/,
  );
  assert.doesNotMatch(masterRunbook, /CI_PR_SOURCE_WRITE_TOKEN/);
  assert.match(prRunbook, /CI_PR_SOURCE_WRITE_TOKEN as NANOCODEX_CI_TOKEN/);
  assert.match(prRunbook, /existing local non-login --prep-user/);
  assert.match(prRunbook, new RegExp(PR_PREP_HELPER_PATH.replaceAll(".", "\\.")));
  assert.match(prRunbook, new RegExp(PR_PREP_CARGO_PATH.replaceAll(".", "\\.")));
  assert.match(prRunbook, /before its first Keychain read/);
  assert.match(
    prRunbook,
    /Uninstall never removes the prep account, helper, Cargo, rustc bundle, or sudoers rule/,
  );
  assert.match(prRunbook, /controller owns no persistent Cargo state/);
  assert.doesNotMatch(
    prRunbook,
    /CI_MASTER_SOURCE_WRITE_TOKEN|CLOUDFLARE_API_TOKEN|NANOCODEX_GIT_TOKEN/,
  );
  assert.match(prRunbook, new RegExp(`${RETAINED_LOG_BYTES}-byte archive`));
  assert.equal(MAX_CURRENT_LOG_BYTES, 8 * 1024 * 1024);
  assert.equal(LOG_ROTATION_INTERVAL_SECONDS, 30);
});

function fixtureConfiguration(role) {
  const paths = deriveServicePaths(HOME, role);
  const repository = "/Users/ci-controller/trusted/nanocodex";
  const base = {
    role,
    paths,
    nodeBinary: "/opt/trusted/node",
    repository,
    controllerScript: controllerScriptPath(repository, role),
    origin: "https://ci.example.test",
    uid: 501,
    username: USERNAME,
    architecture: "arm64",
    metadata: {
      node: { uid: 0, mode: 0o755, nlink: 1 },
      repository: { uid: 501, mode: 0o755, nlink: 12 },
      controller: { uid: 501, mode: 0o644, nlink: 1 },
    },
  };
  if (role === "master") {
    return {
      ...base,
      webOrigin: "https://www.example.test",
      rustSecRepository: "/Users/ci-controller/trusted/advisory-db",
      cloudflareAccountId: ACCOUNT_ID,
      metadata: {
        ...base.metadata,
        rustSec: { uid: 501, mode: 0o755, nlink: 8 },
      },
    };
  }
  return {
    ...base,
    prepUsername: PREP_USERNAME,
    prPrep: fixturePrPrepIdentity(),
  };
}

function fixturePrPrepIdentity() {
  return {
    username: PREP_USERNAME,
    uid: PREP_UID,
    gid: PREP_GID,
    homeDirectory: "/var/empty",
    shell: "/usr/bin/false",
    generatedUid: PREP_GENERATED_UID,
    primaryGroupName: PREP_USERNAME,
    primaryGroupGeneratedUid: PREP_GROUP_GENERATED_UID,
    supplementaryGids: [PREP_GID, 12, 61, 79],
    controllerUsername: USERNAME,
    controllerUid: 501,
    controllerGids: [20, 12, 61, 79],
    helperVersion: PR_PREP_HELPER_VERSION,
    helper: {
      path: PR_PREP_HELPER_PATH,
      device: 1,
      inode: 2,
      size: 1234,
      sha256: "a".repeat(64),
      uid: 0,
      gid: 0,
      mode: 0o555,
      nlink: 1,
    },
    cargo: {
      path: PR_PREP_CARGO_PATH,
      device: 1,
      inode: 3,
      size: 16 * 1024 * 1024,
      sha256: CARGO_SHA256,
      uid: 0,
      gid: 0,
      mode: 0o555,
      nlink: 1,
      release: PR_PREP_CARGO_RELEASE,
      host: "aarch64-apple-darwin",
      versionOutput: CARGO_VERSION_OUTPUT,
    },
    rustc: fixtureRustcBundleIdentity(),
    node: {
      path: "/opt/trusted/node",
      device: 1,
      inode: 4,
      size: 64 * 1024 * 1024,
      sha256: "e".repeat(64),
      uid: 0,
      gid: 0,
      mode: 0o755,
      nlink: 1,
    },
    nodeBinary: "/opt/trusted/node",
    nodeIdentity: "node/darwin/arm64",
    sudoProbe: {
      probe: true,
      build: true,
      completeListing: true,
      helperOnly: true,
      noSetenv: true,
      nopasswd: true,
      timestampTimeoutZero: true,
    },
  };
}

function fixtureRustcBundleIdentity() {
  const directory = (path, inode) => ({
    path,
    device: 1,
    inode,
    uid: 0,
    gid: 0,
    mode: 0o555,
    nlink: 2,
  });
  return {
    root: directory(PR_PREP_RUSTC_ROOT, 10),
    bin: directory(resolve(PR_PREP_RUSTC_ROOT, "bin"), 11),
    lib: directory(resolve(PR_PREP_RUSTC_ROOT, "lib"), 12),
    manifest: {
      path: PR_PREP_RUSTC_MANIFEST_PATH,
      device: 1,
      inode: 13,
      size: Buffer.byteLength(RUSTC_MANIFEST_TEXT),
      sha256: RUSTC_MANIFEST_SHA256,
      uid: 0,
      gid: 0,
      mode: 0o444,
      nlink: 1,
    },
    files: RUSTC_MANIFEST_VALUE.files.map((entry, index) => ({
      path: entry.path,
      absolutePath: resolve(PR_PREP_RUSTC_ROOT, entry.path),
      device: 1,
      inode: 14 + index,
      size: entry.size,
      sha256: entry.sha256,
      uid: 0,
      gid: 0,
      mode: index === 0 ? 0o555 : 0o444,
      nlink: 1,
    })),
    release: PR_PREP_RUSTC_RELEASE,
    host: "aarch64-apple-darwin",
    llvmVersion: "22.1.8",
    versionOutput: RUSTC_VERSION_OUTPUT,
  };
}

function fixtureHelperSnapshot(sha256) {
  const parents = ["/", "/Library", "/Library/PrivilegedHelperTools"].map((path) => ({
    path,
    canonicalPath: path,
    kind: "directory",
    symbolicLink: false,
    uid: 0,
    gid: 0,
    specialMode: 0,
    mode: 0o755,
    nlink: 2,
    inode: 1,
    device: 1,
    size: 0,
  }));
  return {
    parents,
    helper: {
      path: PR_PREP_HELPER_PATH,
      canonicalPath: PR_PREP_HELPER_PATH,
      kind: "file",
      symbolicLink: false,
      uid: 0,
      gid: 0,
      specialMode: 0,
      mode: 0o555,
      nlink: 1,
      inode: 42,
      device: 1,
      size: 4096,
      sha256,
    },
  };
}

function fixtureCargoSnapshot(sha256) {
  const parents = fixtureHelperSnapshot("a".repeat(64)).parents;
  return {
    parents,
    cargo: {
      path: PR_PREP_CARGO_PATH,
      canonicalPath: PR_PREP_CARGO_PATH,
      kind: "file",
      symbolicLink: false,
      uid: 0,
      gid: 0,
      specialMode: 0,
      mode: 0o555,
      nlink: 1,
      inode: 3,
      device: 1,
      size: 16 * 1024 * 1024,
      sha256,
    },
  };
}

function fixtureRustcBundleSnapshot() {
  const directory = (path, inode, mode) => ({
    path,
    canonicalPath: path,
    kind: "directory",
    symbolicLink: false,
    uid: 0,
    gid: 0,
    specialMode: 0,
    mode,
    nlink: 2,
    inode,
    device: 1,
    size: 0,
    accessControlList: false,
  });
  const file = (path, inode, size, sha256, mode) => ({
    path,
    canonicalPath: path,
    kind: "file",
    symbolicLink: false,
    uid: 0,
    gid: 0,
    specialMode: 0,
    mode,
    nlink: 1,
    inode,
    device: 1,
    size,
    sha256,
    accessControlList: false,
  });
  return {
    parents: [
      directory("/", 1, 0o755),
      directory("/Library", 2, 0o755),
      directory("/Library/PrivilegedHelperTools", 3, 0o755),
    ],
    directories: {
      root: directory(PR_PREP_RUSTC_ROOT, 10, 0o555),
      bin: directory(resolve(PR_PREP_RUSTC_ROOT, "bin"), 11, 0o555),
      lib: directory(resolve(PR_PREP_RUSTC_ROOT, "lib"), 12, 0o555),
    },
    entries: {
      root: ["bin", "lib", "manifest.json"],
      bin: ["rustc"],
      lib: ["libLLVM.dylib", RUSTC_DRIVER],
    },
    manifest: file(
      PR_PREP_RUSTC_MANIFEST_PATH,
      13,
      Buffer.byteLength(RUSTC_MANIFEST_TEXT),
      RUSTC_MANIFEST_SHA256,
      0o444,
    ),
    manifestText: RUSTC_MANIFEST_TEXT,
    files: RUSTC_MANIFEST_VALUE.files.map((entry, index) => file(
      resolve(PR_PREP_RUSTC_ROOT, entry.path),
      14 + index,
      entry.size,
      entry.sha256,
      index === 0 ? 0o555 : 0o444,
    )),
  };
}

function fixtureNodeSnapshot(sha256 = "e".repeat(64)) {
  const parents = ["/", "/opt", "/opt/trusted"].map((path) => ({
    path,
    canonicalPath: path,
    kind: "directory",
    symbolicLink: false,
    uid: 0,
    gid: 0,
    specialMode: 0,
    mode: 0o755,
    nlink: 2,
    inode: 1,
    device: 1,
    size: 0,
    accessControlList: false,
  }));
  return {
    parents,
    node: {
      path: "/opt/trusted/node",
      canonicalPath: "/opt/trusted/node",
      kind: "file",
      symbolicLink: false,
      uid: 0,
      gid: 0,
      specialMode: 0,
      mode: 0o755,
      nlink: 1,
      inode: 4,
      device: 1,
      size: 64 * 1024 * 1024,
      sha256,
      accessControlList: false,
    },
  };
}

function probeResult({
  code = 0,
  signal = null,
  stdout = "",
  stderr = "",
  timedOut = false,
} = {}) {
  return { code, signal, stdout, stderr, timedOut };
}

function sudoPolicyListing(nodeBinary) {
  return `Matching Defaults entries for ${USERNAME} on mac-builder:\n` +
    "    env_reset, !setenv, timestamp_timeout=0\n\n" +
    `User ${USERNAME} may run the following commands on mac-builder:\n` +
    `    (${PREP_USERNAME}) NOPASSWD: NOSETENV: ` +
    `${nodeBinary} ${PR_PREP_HELPER_PATH} --probe, ` +
    `${nodeBinary} ${PR_PREP_HELPER_PATH} --build\n`;
}

async function canonicalTemporaryDirectory(prefix) {
  return realpath(await mkdtemp(resolve(tmpdir(), prefix)));
}

function recorded(stat) {
  return {
    uid: stat.uid,
    mode: stat.mode & 0o777,
    nlink: stat.nlink,
  };
}
