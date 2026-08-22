import { cargoCacheInputs } from "./ciWorkflowPlan.ts";

export const DISTRIBUTION_OUTPUT_ROOT =
  "/workspace/.ci-output/distribution" as const;

const DISTRIBUTION_CHECKSUM_ROOT =
  `${DISTRIBUTION_OUTPUT_ROOT}/.sha256` as const;
const LINUX_GNU_TARGET = "x86_64-unknown-linux-gnu" as const;
const LINUX_MUSL_TARGET = "x86_64-unknown-linux-musl" as const;
const CLI_NAME = `nanocodex-${LINUX_GNU_TARGET}` as const;
export const NORMAL_NATIVE_LINUX_OUTPUT_ROOT =
  "/workspace/.ci-output/native-linux" as const;
const NORMAL_NATIVE_LINUX_CHECKSUM_ROOT =
  `${NORMAL_NATIVE_LINUX_OUTPUT_ROOT}/.sha256` as const;
const NORMAL_NATIVE_LINUX_MAX_BYTES = 128 * 1024 * 1024;
const VM_GUEST_NAME = `nanocodex-vm-guest-${LINUX_MUSL_TARGET}` as const;
const SHA256SUMS_NAME = "SHA256SUMS" as const;
const PROVENANCE_NAME = "PROVENANCE.json" as const;
const LOCAL_NANOCODEX_DEPENDENCY_REQUIREMENTS_QUERY =
  '[.packages[].dependencies[] | select(.source == null and (.name | startswith("nanocodex"))) | .req] | all(. == ("^" + $version))';

export type NormalNativeLinuxBuildInput = Readonly<{
  testedSha: string;
  publishedAt: string;
}>;

export type NormalNativeLinuxOutput = Readonly<{
  kind: "native-cli";
  name: typeof CLI_NAME;
  platform: typeof LINUX_GNU_TARGET;
  path: string;
  sha256Path: string;
  maxBytes: number;
  contentType: "application/octet-stream";
}>;

export type NormalNativeLinuxPlan = Readonly<{
  testedSha: string;
  publishedAt: string;
  tagName: "pr";
  profile: "nightly";
  target: typeof LINUX_GNU_TARGET;
  command: string;
  outputs: readonly [NormalNativeLinuxOutput];
  cacheInputs: readonly string[];
}>;

export type DistributionChannel = "nightly" | "stable";
export type DistributionPlatform =
  | typeof LINUX_GNU_TARGET
  | typeof LINUX_MUSL_TARGET
  | "linux";
export type DistributionOutputKind =
  | "cli"
  | "vm-guest"
  | "checksums"
  | "provenance";

export type DistributionBuildInput = Readonly<{
  channel: DistributionChannel;
  tagName: string;
  sha: string;
  buildTimestamp: string;
}>;

export type DistributionOutput = Readonly<{
  kind: DistributionOutputKind;
  name: string;
  platform: DistributionPlatform;
  path: string;
  sha256Path: string;
  maxBytes: number;
  contentType: string;
}>;

export type DistributionProvenance = Readonly<{
  schemaVersion: 1;
  builder: "nanocodex-cloudflare-ci";
  channel: DistributionChannel;
  tagName: string;
  sourceSha: string;
  buildTimestamp: string;
  profile: "nightly" | "release";
  artifacts: readonly Readonly<{
    name: string;
    kind: "cli" | "vm-guest";
    platform: typeof LINUX_GNU_TARGET | typeof LINUX_MUSL_TARGET;
    encoding: "identity" | "gzip";
  }>[];
  policy: Readonly<{
    cargoNetwork: "offline";
    rustlsProvider: "ring";
    vmGuest: "static-no-interpreter";
    gzip: "no-name-max-compression";
  }>;
}>;

export type LinuxDistributionPlan = Readonly<{
  channel: DistributionChannel;
  tagName: string;
  sha: string;
  buildTimestamp: string;
  profile: "nightly" | "release";
  command: string;
  outputs: readonly DistributionOutput[];
  cacheInputs: readonly string[];
  provenance: DistributionProvenance;
}>;

const BINARY_OUTPUTS = [
  output("cli", CLI_NAME, LINUX_GNU_TARGET, 128 * 1024 * 1024, "application/octet-stream"),
  output("cli", `${CLI_NAME}.gz`, LINUX_GNU_TARGET, 128 * 1024 * 1024, "application/gzip"),
  output(
    "vm-guest",
    VM_GUEST_NAME,
    LINUX_MUSL_TARGET,
    64 * 1024 * 1024,
    "application/octet-stream",
  ),
  output(
    "vm-guest",
    `${VM_GUEST_NAME}.gz`,
    LINUX_MUSL_TARGET,
    64 * 1024 * 1024,
    "application/gzip",
  ),
] as const satisfies readonly DistributionOutput[];

const METADATA_OUTPUTS = [
  output(
    "checksums",
    SHA256SUMS_NAME,
    "linux",
    64 * 1024,
    "text/plain; charset=utf-8",
  ),
  output(
    "provenance",
    PROVENANCE_NAME,
    "linux",
    64 * 1024,
    "application/json",
  ),
] as const satisfies readonly DistributionOutput[];

const DISTRIBUTION_OUTPUTS = [
  ...BINARY_OUTPUTS,
  ...METADATA_OUTPUTS,
] as const satisfies readonly DistributionOutput[];

/** Build the one lane-neutral native Linux CLI consumed by normal CI. */
export function normalNativeLinuxPlan(
  input: NormalNativeLinuxBuildInput,
): NormalNativeLinuxPlan {
  const identity = validateNormalNativeLinuxInput(input);
  const source =
    `/workspace/.cargo-target/${LINUX_GNU_TARGET}/nightly/nanocodex`;
  const path = `${NORMAL_NATIVE_LINUX_OUTPUT_ROOT}/${CLI_NAME}`;
  const sha256Path = `${NORMAL_NATIVE_LINUX_CHECKSUM_ROOT}/${CLI_NAME}.sha256`;
  const command = [
    "set -euo pipefail",
    "umask 022",
    "export CARGO_NET_OFFLINE=true",
    "export CARGO_TARGET_DIR=/workspace/.cargo-target",
    `TAG_NAME=pr VERGEN_GIT_SHA=${shellQuote(identity.testedSha)} VERGEN_BUILD_TIMESTAMP=${shellQuote(identity.publishedAt)} cargo build --locked --profile nightly --package nanocodex-bin --bin nanocodex --features tempo --target ${LINUX_GNU_TARGET}`,
    `rm -rf -- ${shellQuote(NORMAL_NATIVE_LINUX_OUTPUT_ROOT)}`,
    `install -d -m 0755 -- ${shellQuote(NORMAL_NATIVE_LINUX_CHECKSUM_ROOT)}`,
    `install -m 0755 -- ${shellQuote(source)} ${shellQuote(path)}`,
    `test -f ${shellQuote(path)}`,
    `test ! -L ${shellQuote(path)}`,
    `test -s ${shellQuote(path)}`,
    `test -x ${shellQuote(path)}`,
    `native_size="$(wc -c < ${shellQuote(path)} | tr -d '[:space:]')"`,
    `case "$native_size" in ''|*[!0-9]*) printf '%s\\n' 'Native CLI size is invalid' >&2; exit 1;; esac`,
    `test "$native_size" -gt 0`,
    `test "$native_size" -le ${NORMAL_NATIVE_LINUX_MAX_BYTES}`,
    [
      "if command -v readelf >/dev/null 2>&1; then",
      `  native_header="$(LC_ALL=C readelf -hW -- ${shellQuote(path)})"`,
      `  printf '%s\\n' "$native_header" | grep -Eq '^  Class:[[:space:]]+ELF64$'`,
      `  printf '%s\\n' "$native_header" | grep -Eq '^  Machine:[[:space:]]+(Advanced Micro Devices X86-64|AMD x86-64)$'`,
      `  printf '%s\\n' "$native_header" | grep -Eq '^  Type:[[:space:]]+(EXEC|DYN)[[:space:]]'`,
      `  printf '%s\\n' "$native_header" | grep -Eq '^  Entry point address:[[:space:]]+0x[0-9a-fA-F]*[1-9a-fA-F][0-9a-fA-F]*$'`,
      "elif command -v file >/dev/null 2>&1; then",
      `  native_kind="$(LC_ALL=C file -b -- ${shellQuote(path)})"`,
      `  printf '%s\\n' "$native_kind" | grep -Eq '^ELF 64-bit .* (pie )?executable, x86-64,'`,
      "else",
      `  printf '%s\\n' 'readelf or file is required to verify the native CLI' >&2`,
      "  exit 1",
      "fi",
    ].join("\n"),
    `LC_ALL=C sha256sum -- ${shellQuote(path)} > ${shellQuote(sha256Path)}`,
    `test -f ${shellQuote(sha256Path)}`,
    `test ! -L ${shellQuote(sha256Path)}`,
    `test -s ${shellQuote(sha256Path)}`,
  ].join("\n");

  return {
    ...identity,
    tagName: "pr",
    profile: "nightly",
    target: LINUX_GNU_TARGET,
    command,
    outputs: [{
      kind: "native-cli",
      name: CLI_NAME,
      platform: LINUX_GNU_TARGET,
      path,
      sha256Path,
      maxBytes: NORMAL_NATIVE_LINUX_MAX_BYTES,
      contentType: "application/octet-stream",
    }],
    cacheInputs: normalNativeLinuxCacheInputs(),
  };
}

export function normalNativeLinuxCacheInputs(): string[] {
  return [
    ...cargoCacheInputs(),
    "bin/**/*",
    "crates/**/*",
  ];
}

/** Build both Linux distribution binaries and their deterministic metadata. */
export function linuxDistributionPlan(
  input: DistributionBuildInput,
): LinuxDistributionPlan {
  const identity = validateIdentity(input);
  const profile = identity.channel === "nightly" ? "nightly" : "release";
  const profileFlag = profile === "nightly" ? "--profile nightly" : "--release";
  const provenance = distributionProvenance(identity, profile);
  const cliSource = `/workspace/.cargo-target/${LINUX_GNU_TARGET}/${profile}/nanocodex`;
  const guestSource = `/workspace/.cargo-target/${LINUX_MUSL_TARGET}/${profile}/nanocodex-vm-guest`;
  const guestHeaders = "/tmp/nanocodex-vm-guest-program-headers.txt";
  const identityEnvironment = [
    `TAG_NAME=${shellQuote(identity.tagName)}`,
    `VERGEN_GIT_SHA=${shellQuote(identity.sha)}`,
    `VERGEN_BUILD_TIMESTAMP=${shellQuote(identity.buildTimestamp)}`,
  ].join(" ");
  const checksumNames = [
    ...BINARY_OUTPUTS.map(({ name }) => name),
    PROVENANCE_NAME,
  ];

  const checksumCommands = checksumNames.map(
    (name) =>
      `sha256sum ${shellQuote(name)} > ${shellQuote(`${DISTRIBUTION_CHECKSUM_ROOT}/${name}.sha256`)}`,
  );
  checksumCommands.push(
    `sha256sum ${shellQuote(SHA256SUMS_NAME)} > ${shellQuote(`${DISTRIBUTION_CHECKSUM_ROOT}/${SHA256SUMS_NAME}.sha256`)}`,
  );

  const command = [
    "set -eu",
    "export CARGO_NET_OFFLINE=true",
    "./scripts/check-rustls-provider.sh",
    `${identityEnvironment} cargo build --locked ${profileFlag} --package nanocodex-bin --bin nanocodex --features tempo --target ${LINUX_GNU_TARGET} --target-dir /workspace/.cargo-target`,
    `${identityEnvironment} CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc cargo build --locked ${profileFlag} --package nanocodex-vm --bin nanocodex-vm-guest --no-default-features --features guest-runtime --target ${LINUX_MUSL_TARGET} --target-dir /workspace/.cargo-target`,
    `LC_ALL=C readelf -lW ${shellQuote(guestSource)} > ${shellQuote(guestHeaders)}`,
    `if grep -q '[[:space:]]INTERP[[:space:]]' ${shellQuote(guestHeaders)}; then cat ${shellQuote(guestHeaders)} >&2; printf '%s\\n' 'VM guest has a dynamic program interpreter' >&2; exit 1; fi`,
    `rm -rf -- ${shellQuote(DISTRIBUTION_OUTPUT_ROOT)}`,
    `mkdir -p ${shellQuote(DISTRIBUTION_CHECKSUM_ROOT)}`,
    `install -m 0755 ${shellQuote(cliSource)} ${shellQuote(`${DISTRIBUTION_OUTPUT_ROOT}/${CLI_NAME}`)}`,
    `install -m 0755 ${shellQuote(guestSource)} ${shellQuote(`${DISTRIBUTION_OUTPUT_ROOT}/${VM_GUEST_NAME}`)}`,
    `gzip -n -9 -c ${shellQuote(`${DISTRIBUTION_OUTPUT_ROOT}/${CLI_NAME}`)} > ${shellQuote(`${DISTRIBUTION_OUTPUT_ROOT}/${CLI_NAME}.gz`)}`,
    `gzip -n -9 -c ${shellQuote(`${DISTRIBUTION_OUTPUT_ROOT}/${VM_GUEST_NAME}`)} > ${shellQuote(`${DISTRIBUTION_OUTPUT_ROOT}/${VM_GUEST_NAME}.gz`)}`,
    `printf '%s\\n' ${shellQuote(JSON.stringify(provenance))} > ${shellQuote(`${DISTRIBUTION_OUTPUT_ROOT}/${PROVENANCE_NAME}`)}`,
    `(cd ${shellQuote(DISTRIBUTION_OUTPUT_ROOT)} && sha256sum ${checksumNames.map(shellQuote).join(" ")} > ${shellQuote(SHA256SUMS_NAME)} && ${checksumCommands.join(" && ")})`,
    `test -x ${shellQuote(`${DISTRIBUTION_OUTPUT_ROOT}/${CLI_NAME}`)}`,
    `test -x ${shellQuote(`${DISTRIBUTION_OUTPUT_ROOT}/${VM_GUEST_NAME}`)}`,
    ...DISTRIBUTION_OUTPUTS
      .filter(({ kind }) => kind !== "cli" && kind !== "vm-guest")
      .map(({ path }) => `test -s ${shellQuote(path)}`),
    ...BINARY_OUTPUTS
      .filter(({ contentType }) => contentType === "application/gzip")
      .map(({ path }) => `test -s ${shellQuote(path)}`),
  ].join(" && ");

  return {
    ...identity,
    profile,
    command,
    outputs: DISTRIBUTION_OUTPUTS.map((value) => ({ ...value })),
    cacheInputs: linuxDistributionCacheInputs(),
    provenance,
  };
}

export function nightlyLinuxDistributionPlan(
  sha: string,
  buildTimestamp = "1970-01-01T00:00:00Z",
): LinuxDistributionPlan {
  return linuxDistributionPlan({
    channel: "nightly",
    tagName: "nightly",
    sha,
    buildTimestamp,
  });
}

export function stableLinuxDistributionPlan(
  tagName: string,
  sha: string,
  buildTimestamp = "1970-01-01T00:00:00Z",
): LinuxDistributionPlan {
  return linuxDistributionPlan({ channel: "stable", tagName, sha, buildTimestamp });
}

export function linuxCliDistributionCacheInputs(): string[] {
  return [
    ...cargoCacheInputs(),
    "scripts/check-rustls-provider.sh",
    "bin/**/*",
    "crates/**/*",
  ];
}

export function linuxVmGuestDistributionCacheInputs(): string[] {
  return [
    ...cargoCacheInputs(),
    "crates/experimental/nanocodex-vm/**/*",
    "crates/nanocodex-oai-api/**/*",
    "crates/nanocodex-tools/**/*",
  ];
}

export function linuxDistributionCacheInputs(): string[] {
  return [
    ...new Set([
      ...linuxCliDistributionCacheInputs(),
      ...linuxVmGuestDistributionCacheInputs(),
    ]),
  ];
}

export function stableReleaseValidationCommand(tagName: string): string {
  const { tagName: tag } = validateIdentity({
    channel: "stable",
    tagName,
    sha: "0".repeat(40),
    buildTimestamp: "1970-01-01T00:00:00Z",
  });
  const version = tag.slice(1);
  return [
    "set -eu",
    "export CARGO_NET_OFFLINE=true",
    `test "$(cargo metadata --no-deps --format-version 1 | jq -er '.packages[] | select(.name == \"nanocodex\") | .version')" = ${shellQuote(version)}`,
    `test "$(node -p \"require('./js/bindings/package.json').version\")" = ${shellQuote(version)}`,
    `test "$(cargo metadata --no-deps --format-version 1 | jq -er '.packages[] | select(.name == \"nanocodex-python\") | .version')" = ${shellQuote(version)}`,
    `grep -Fq ${shellQuote(`__version__ = "${version}"`)} py/bindings/python/nanocodex/__init__.py`,
    "grep -Fq 'dynamic = [\"version\"]' py/bindings/pyproject.toml",
    `grep -Fq ${shellQuote(`## [${version}]`)} CHANGELOG.md`,
    "grep -Fq '<!-- generated by git-cliff -->' CHANGELOG.md",
    "./scripts/release-crates.sh check",
    `for crate_path in $(./scripts/release-crates.sh paths); do grep -Fq ${shellQuote(`## [${version}]`)} "$crate_path/CHANGELOG.md" && grep -Fq '<!-- generated by git-cliff -->' "$crate_path/CHANGELOG.md"; done`,
    `cargo metadata --no-deps --format-version 1 | jq -e --arg version ${shellQuote(version)} ${shellQuote(LOCAL_NANOCODEX_DEPENDENCY_REQUIREMENTS_QUERY)} >/dev/null`,
    "bash -n install scripts/changelog.sh scripts/check-crate-boundaries.sh scripts/check-docs.sh scripts/check-rustls-provider.sh scripts/publish-crates.sh scripts/release-crates.sh",
    "./scripts/check-crate-boundaries.sh",
    "./scripts/check-rustls-provider.sh",
    "for crate in $(./scripts/release-crates.sh names); do cargo package --locked --no-verify --config .cargo/release.toml --package \"$crate\"; done",
    "./scripts/check-docs.sh",
  ].join(" && ");
}

function distributionProvenance(
  identity: DistributionBuildInput,
  profile: "nightly" | "release",
): DistributionProvenance {
  return {
    schemaVersion: 1,
    builder: "nanocodex-cloudflare-ci",
    channel: identity.channel,
    tagName: identity.tagName,
    sourceSha: identity.sha,
    buildTimestamp: identity.buildTimestamp,
    profile,
    artifacts: BINARY_OUTPUTS.map(({ kind, name, platform, contentType }) => ({
      name,
      kind,
      platform: platform as typeof LINUX_GNU_TARGET | typeof LINUX_MUSL_TARGET,
      encoding: contentType === "application/gzip" ? "gzip" : "identity",
    })),
    policy: {
      cargoNetwork: "offline",
      rustlsProvider: "ring",
      vmGuest: "static-no-interpreter",
      gzip: "no-name-max-compression",
    },
  };
}

function validateIdentity(
  input: DistributionBuildInput,
): DistributionBuildInput {
  if (input.channel !== "nightly" && input.channel !== "stable") {
    throw new TypeError("distribution channel must be nightly or stable");
  }
  if (!/^[0-9a-f]{40}$/.test(input.sha)) {
    throw new TypeError("distribution SHA must be exactly 40 lowercase hexadecimal characters");
  }
  if (!Number.isFinite(Date.parse(input.buildTimestamp))) {
    throw new TypeError("distribution build timestamp must be RFC 3339");
  }
  if (input.channel === "nightly") {
    if (input.tagName !== "nightly") {
      throw new TypeError("nightly distribution TAG_NAME must be exactly nightly");
    }
  } else if (
    input.tagName.length > 64 ||
    !/^v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$/.test(
      input.tagName,
    )
  ) {
    throw new TypeError("stable distribution TAG_NAME must be a canonical vMAJOR.MINOR.PATCH tag");
  }
  return {
    channel: input.channel,
    tagName: input.tagName,
    sha: input.sha,
    buildTimestamp: input.buildTimestamp,
  };
}

function validateNormalNativeLinuxInput(
  input: NormalNativeLinuxBuildInput,
): NormalNativeLinuxBuildInput {
  if (input == null || typeof input !== "object" || Array.isArray(input)) {
    throw new TypeError("normal native Linux build input must be an object");
  }
  const keys = Object.keys(input).sort();
  if (
    keys.length !== 2 || keys[0] !== "publishedAt" || keys[1] !== "testedSha"
  ) {
    throw new TypeError(
      "normal native Linux build input must contain only publishedAt and testedSha",
    );
  }
  if (!/^[0-9a-f]{40}$/.test(input.testedSha)) {
    throw new TypeError(
      "normal native Linux tested SHA must be exactly 40 lowercase hexadecimal characters",
    );
  }
  if (
    !/^\d{4}-(?:0[1-9]|1[0-2])-(?:0[1-9]|[12]\d|3[01])T(?:[01]\d|2[0-3]):[0-5]\d:[0-5]\d\.\d{3}Z$/.test(
      input.publishedAt,
    ) ||
    !Number.isFinite(Date.parse(input.publishedAt)) ||
    new Date(input.publishedAt).toISOString() !== input.publishedAt
  ) {
    throw new TypeError(
      "normal native Linux publishedAt must be a canonical UTC RFC 3339 timestamp",
    );
  }
  return {
    testedSha: input.testedSha,
    publishedAt: input.publishedAt,
  };
}

function output<
  const Kind extends DistributionOutputKind,
  const Name extends string,
  const Platform extends DistributionPlatform,
>(
  kind: Kind,
  name: Name,
  platform: Platform,
  maxBytes: number,
  contentType: string,
): DistributionOutput & Readonly<{
  kind: Kind;
  name: Name;
  platform: Platform;
}> {
  return {
    kind,
    name,
    platform,
    path: `${DISTRIBUTION_OUTPUT_ROOT}/${name}`,
    sha256Path: `${DISTRIBUTION_CHECKSUM_ROOT}/${name}.sha256`,
    maxBytes,
    contentType,
  };
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", `'"'"'`)}'`;
}
