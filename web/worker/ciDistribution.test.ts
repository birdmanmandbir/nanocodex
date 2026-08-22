import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { test } from "node:test";

import { cargoCacheInputs } from "./ciWorkflowPlan.ts";
import {
  DISTRIBUTION_OUTPUT_ROOT,
  linuxCliDistributionCacheInputs,
  linuxDistributionCacheInputs,
  linuxDistributionPlan,
  linuxVmGuestDistributionCacheInputs,
  nightlyLinuxDistributionPlan,
  normalNativeLinuxCacheInputs,
  normalNativeLinuxPlan,
  NORMAL_NATIVE_LINUX_OUTPUT_ROOT,
  stableReleaseValidationCommand,
  stableLinuxDistributionPlan,
} from "./ciDistribution.ts";

const SHA = "a".repeat(40);
const PUBLISHED_AT = "2026-08-22T12:34:56.789Z";
const CLI = "nanocodex-x86_64-unknown-linux-gnu";
const GUEST = "nanocodex-vm-guest-x86_64-unknown-linux-musl";

test("normal CI builds one deterministic lane-neutral Linux native CLI", () => {
  const plan = normalNativeLinuxPlan({ testedSha: SHA, publishedAt: PUBLISHED_AT });
  const repeated = normalNativeLinuxPlan({
    testedSha: SHA,
    publishedAt: PUBLISHED_AT,
  });

  assert.deepEqual(plan, repeated);
  assert.equal(plan.testedSha, SHA);
  assert.equal(plan.publishedAt, PUBLISHED_AT);
  assert.equal(plan.tagName, "pr");
  assert.equal(plan.profile, "nightly");
  assert.equal(plan.target, "x86_64-unknown-linux-gnu");
  assertBashSyntax(plan.command);
  assert.match(plan.command, /export CARGO_NET_OFFLINE=true/);
  assert.match(plan.command, /export CARGO_TARGET_DIR=\/workspace\/\.cargo-target/);
  assert.match(
    plan.command,
    new RegExp(
      `TAG_NAME=pr VERGEN_GIT_SHA='${SHA}' VERGEN_BUILD_TIMESTAMP='${escapeRegExp(PUBLISHED_AT)}' cargo build --locked --profile nightly --package nanocodex-bin --bin nanocodex --features tempo --target x86_64-unknown-linux-gnu`,
    ),
  );
  assert.equal(countMatches(plan.command, /\bcargo build\b/g), 1);
  assert.match(plan.command, /test -f/);
  assert.match(plan.command, /test ! -L/);
  assert.match(plan.command, /test -s/);
  assert.match(plan.command, /test -x/);
  assert.match(plan.command, /native_size/);
  assert.match(plan.command, /readelf -hW/);
  assert.match(plan.command, /ELF64/);
  assert.match(plan.command, /Advanced Micro Devices X86-64/);
  assert.match(plan.command, /file -b/);
  assert.match(plan.command, /executable, x86-64/);

  assert.deepEqual(plan.outputs, [
    {
      kind: "native-cli",
      name: CLI,
      platform: "x86_64-unknown-linux-gnu",
      path: `${NORMAL_NATIVE_LINUX_OUTPUT_ROOT}/${CLI}`,
      sha256Path: `${NORMAL_NATIVE_LINUX_OUTPUT_ROOT}/.sha256/${CLI}.sha256`,
      maxBytes: 128 * 1024 * 1024,
      contentType: "application/octet-stream",
    },
  ]);
  assert.match(plan.command, new RegExp(escapeRegExp(plan.outputs[0].path)));
  assert.match(plan.command, new RegExp(escapeRegExp(plan.outputs[0].sha256Path)));
  assert.match(plan.command, /sha256sum/);
});

test("normal native identity rejects noncanonical and hostile input", () => {
  for (const testedSha of [
    "a".repeat(39),
    "A".repeat(40),
    "g".repeat(40),
    `${SHA}; touch /tmp/native-plan-pwned`,
  ]) {
    assert.throws(
      () => normalNativeLinuxPlan({ testedSha, publishedAt: PUBLISHED_AT }),
      /exactly 40 lowercase hexadecimal/,
    );
  }
  for (const publishedAt of [
    "2026-08-22T12:34:56Z",
    "2026-08-22T15:34:56.789+03:00",
    "2026-02-30T12:34:56.789Z",
    "not-a-timestamp",
    `${PUBLISHED_AT}'$(touch /tmp/native-plan-pwned)`,
  ]) {
    assert.throws(
      () => normalNativeLinuxPlan({ testedSha: SHA, publishedAt }),
      /canonical UTC RFC 3339/,
    );
  }
  assert.throws(
    () =>
      normalNativeLinuxPlan({
        testedSha: SHA,
        publishedAt: PUBLISHED_AT,
        channel: "pull-request",
      } as Parameters<typeof normalNativeLinuxPlan>[0]),
    /contain only publishedAt and testedSha/,
  );
});

test("normal native plan has no release, guest, compression, ledger, or network surface", () => {
  const plan = normalNativeLinuxPlan({ testedSha: SHA, publishedAt: PUBLISHED_AT });
  const buildSurface = JSON.stringify({
    command: plan.command,
    outputs: plan.outputs,
  });

  assert.doesNotMatch(buildSurface, /\b(?:stable|release|distribution|channel)\b/i);
  assert.doesNotMatch(buildSurface, /vm[ -]?guest|nanocodex-vm/i);
  assert.doesNotMatch(buildSurface, /gzip|SHA256SUMS|PROVENANCE\.json/i);
  assert.doesNotMatch(plan.command, /https?:\/\//i);
  assert.doesNotMatch(plan.command, /\b(?:curl|wget|gh|git|npm|apt(?:-get)?)\b/i);
  assert.doesNotMatch(plan.command, /GITHUB_|CI_RELEASE|BACKUP_BUCKET/i);
  assert.equal(plan.outputs.length, 1);
  assert.equal(plan.outputs[0].contentType, "application/octet-stream");
  assert.ok(plan.outputs[0].maxBytes > 0);
});

test("normal native cache inputs cover only Cargo, CLI, and library sources", () => {
  assert.deepEqual(normalNativeLinuxCacheInputs(), [
    ...cargoCacheInputs(),
    "bin/**/*",
    "crates/**/*",
  ]);
  assert.deepEqual(
    normalNativeLinuxPlan({ testedSha: SHA, publishedAt: PUBLISHED_AT })
      .cacheInputs,
    normalNativeLinuxCacheInputs(),
  );
});

test("stable Linux distribution builds deterministic CLI and static guest artifacts", () => {
  const plan = stableLinuxDistributionPlan("v1.2.3", SHA);

  assert.equal(plan.channel, "stable");
  assert.equal(plan.tagName, "v1.2.3");
  assert.equal(plan.sha, SHA);
  assert.equal(plan.profile, "release");
  assertBashSyntax(plan.command);
  assert.match(plan.command, /\.\/scripts\/check-rustls-provider\.sh/);
  assert.ok(
    plan.command.indexOf("./scripts/check-rustls-provider.sh") <
      plan.command.indexOf("cargo build"),
  );
  assert.match(
    plan.command,
    new RegExp(
      `TAG_NAME='v1\\.2\\.3' VERGEN_GIT_SHA='${SHA}' VERGEN_BUILD_TIMESTAMP='1970-01-01T00:00:00Z' cargo build`,
    ),
  );
  assert.match(
    plan.command,
    /cargo build --locked --release --package nanocodex-bin --bin nanocodex --features tempo --target x86_64-unknown-linux-gnu/,
  );
  assert.match(
    plan.command,
    /CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc cargo build --locked --release --package nanocodex-vm --bin nanocodex-vm-guest --no-default-features --features guest-runtime --target x86_64-unknown-linux-musl/,
  );
  assert.match(plan.command, /readelf -lW/);
  assert.match(plan.command, /INTERP/);
  assert.match(plan.command, /VM guest has a dynamic program interpreter/);
  assert.equal(countMatches(plan.command, /gzip -n -9 -c/g), 2);
  assert.match(plan.command, /sha256sum .* > 'SHA256SUMS'/);
  assert.match(plan.command, /export CARGO_NET_OFFLINE=true/);
  assertNoNetworkOrGitHub(plan.command);

  assert.deepEqual(
    plan.outputs.map(({ kind, name, platform, contentType }) => ({
      kind,
      name,
      platform,
      contentType,
    })),
    [
      {
        kind: "cli",
        name: CLI,
        platform: "x86_64-unknown-linux-gnu",
        contentType: "application/octet-stream",
      },
      {
        kind: "cli",
        name: `${CLI}.gz`,
        platform: "x86_64-unknown-linux-gnu",
        contentType: "application/gzip",
      },
      {
        kind: "vm-guest",
        name: GUEST,
        platform: "x86_64-unknown-linux-musl",
        contentType: "application/octet-stream",
      },
      {
        kind: "vm-guest",
        name: `${GUEST}.gz`,
        platform: "x86_64-unknown-linux-musl",
        contentType: "application/gzip",
      },
      {
        kind: "checksums",
        name: "SHA256SUMS",
        platform: "linux",
        contentType: "text/plain; charset=utf-8",
      },
      {
        kind: "provenance",
        name: "PROVENANCE.json",
        platform: "linux",
        contentType: "application/json",
      },
    ],
  );
  assert.deepEqual(plan.provenance, {
    schemaVersion: 1,
    builder: "nanocodex-cloudflare-ci",
    channel: "stable",
    tagName: "v1.2.3",
    sourceSha: SHA,
    buildTimestamp: "1970-01-01T00:00:00Z",
    profile: "release",
    artifacts: [
      {
        name: CLI,
        kind: "cli",
        platform: "x86_64-unknown-linux-gnu",
        encoding: "identity",
      },
      {
        name: `${CLI}.gz`,
        kind: "cli",
        platform: "x86_64-unknown-linux-gnu",
        encoding: "gzip",
      },
      {
        name: GUEST,
        kind: "vm-guest",
        platform: "x86_64-unknown-linux-musl",
        encoding: "identity",
      },
      {
        name: `${GUEST}.gz`,
        kind: "vm-guest",
        platform: "x86_64-unknown-linux-musl",
        encoding: "gzip",
      },
    ],
    policy: {
      cargoNetwork: "offline",
      rustlsProvider: "ring",
      vmGuest: "static-no-interpreter",
      gzip: "no-name-max-compression",
    },
  });
});

test("nightly distribution fixes TAG_NAME and uses the nightly profile", () => {
  const plan = nightlyLinuxDistributionPlan(SHA);
  const repeated = nightlyLinuxDistributionPlan(SHA);

  assert.deepEqual(plan, repeated);
  assert.equal(plan.channel, "nightly");
  assert.equal(plan.tagName, "nightly");
  assert.equal(plan.profile, "nightly");
  assertBashSyntax(plan.command);
  assert.match(
    plan.command,
    new RegExp(
      `TAG_NAME='nightly' VERGEN_GIT_SHA='${SHA}' VERGEN_BUILD_TIMESTAMP='1970-01-01T00:00:00Z' cargo build`,
    ),
  );
  assert.equal(countMatches(plan.command, /--profile nightly/g), 2);
  assert.doesNotMatch(plan.command, /--release/);
  assert.match(plan.command, new RegExp(`${CLI.replaceAll("-", "\\-")}\\.gz`));
  assert.match(plan.command, new RegExp(`${GUEST.replaceAll("-", "\\-")}\\.gz`));
  assert.match(plan.command, /PROVENANCE\.json/);
  assert.match(plan.command, /SHA256SUMS/);
  assertNoNetworkOrGitHub(plan.command);
});

test("distribution identities reject noncanonical channels, tags, and SHAs", () => {
  for (const tagName of [
    "1.2.3",
    "v01.2.3",
    "v1.02.3",
    "v1.2.03",
    "v1.2",
    "v1.2.3-rc.1",
    "v1.2.3; touch /tmp/pwned",
    `v${"1".repeat(64)}.2.3`,
  ]) {
    assert.throws(
      () => stableLinuxDistributionPlan(tagName, SHA),
      /canonical vMAJOR\.MINOR\.PATCH/,
    );
  }
  for (const sha of ["a".repeat(39), "A".repeat(40), "g".repeat(40), `${SHA};`]) {
    assert.throws(() => nightlyLinuxDistributionPlan(sha), /40 lowercase hexadecimal/);
  }
  assert.throws(
    () =>
      linuxDistributionPlan({
        channel: "nightly",
        tagName: "nightly-latest",
        sha: SHA,
        buildTimestamp: "1970-01-01T00:00:00Z",
      }),
    /exactly nightly/,
  );
  assert.throws(
    () =>
      linuxDistributionPlan({
        channel: "candidate" as "stable",
        tagName: "v1.2.3",
        sha: SHA,
        buildTimestamp: "1970-01-01T00:00:00Z",
      }),
    /channel must be nightly or stable/,
  );
});

test("distribution outputs and checksum sidecars stay within fixed byte-bounded paths", () => {
  const plan = nightlyLinuxDistributionPlan(SHA);

  assert.equal(new Set(plan.outputs.map(({ name }) => name)).size, plan.outputs.length);
  for (const output of plan.outputs) {
    assert.equal(output.path, `${DISTRIBUTION_OUTPUT_ROOT}/${output.name}`);
    assert.equal(
      output.sha256Path,
      `${DISTRIBUTION_OUTPUT_ROOT}/.sha256/${output.name}.sha256`,
    );
    assert.ok(output.maxBytes > 0 && output.maxBytes <= 128 * 1024 * 1024);
    assert.ok(!output.path.includes(".."));
    assert.ok(!output.sha256Path.includes(".."));
    assert.match(plan.command, new RegExp(escapeRegExp(output.path)));
    assert.match(plan.command, new RegExp(escapeRegExp(output.sha256Path)));
  }
  assert.match(
    plan.command,
    new RegExp(
      `sha256sum '${CLI}' '${CLI}\\.gz' '${GUEST}' '${GUEST}\\.gz' 'PROVENANCE\\.json' > 'SHA256SUMS'`,
    ),
  );
});

test("distribution cache inputs cover the exact Cargo, policy, CLI, and guest sources", () => {
  assert.deepEqual(linuxCliDistributionCacheInputs(), [
    ...cargoCacheInputs(),
    "scripts/check-rustls-provider.sh",
    "bin/**/*",
    "crates/**/*",
  ]);
  assert.deepEqual(linuxVmGuestDistributionCacheInputs(), [
    ...cargoCacheInputs(),
    "crates/experimental/nanocodex-vm/**/*",
    "crates/nanocodex-oai-api/**/*",
    "crates/nanocodex-tools/**/*",
  ]);
  assert.deepEqual(linuxDistributionCacheInputs(), [
    ...new Set([
      ...linuxCliDistributionCacheInputs(),
      ...linuxVmGuestDistributionCacheInputs(),
    ]),
  ]);
  assert.deepEqual(
    nightlyLinuxDistributionPlan(SHA).cacheInputs,
    linuxDistributionCacheInputs(),
  );
});

test("stable publication validates the exact release tree before staging", () => {
  const validation = stableReleaseValidationCommand("v1.2.3");
  assert.equal(validation, stableReleaseValidationCommand("v1.2.3"));
  assertBashSyntax(validation);
  assert.match(
    validation,
    /select\(\.name == "nanocodex"\) \| \.version'\)" = '1\.2\.3'/,
  );
  assert.match(
    validation,
    /js\/bindings\/package\.json'\)\.version\"\)" = '1\.2\.3'/,
  );
  assert.match(
    validation,
    /select\(\.name == "nanocodex-python"\) \| \.version'\)" = '1\.2\.3'/,
  );
  assert.match(validation, /__version__ = "1\.2\.3"/);
  assert.match(validation, /## \[1\.2\.3\]/);
  assert.match(validation, /generated by git-cliff/);
  assert.match(validation, /scripts\/release-crates\.sh check/);
  assert.match(validation, /cargo package --locked --no-verify/);
  assert.match(validation, /scripts\/check-docs\.sh/);
  assert.doesNotMatch(validation, /github/i);

  for (const tagName of ["1.2.3", "v1.2", "v1.2.3-rc.1"]) {
    assert.throws(
      () => stableReleaseValidationCommand(tagName),
      /canonical vMAJOR\.MINOR\.PATCH/,
    );
  }
});

test("stable publication requires exact caret requirements for every local nanocodex dependency", () => {
  const validation = stableReleaseValidationCommand("v1.2.3");
  const requirementChecks = validation
    .split(" && ")
    .filter((statement) => statement.includes("jq -e --arg version"));
  assert.equal(requirementChecks.length, 1);
  const requirementCheck = requirementChecks[0];

  assert.match(
    requirementCheck,
    /select\(\.source == null and \(\.name \| startswith\("nanocodex"\)\)\)/,
  );
  assert.match(
    requirementCheck,
    /all\(\. == \("\^" \+ \$version\)\)/,
  );
  assert.match(requirementCheck, /--arg version '1\.2\.3'/);

  const validMetadata = dependencyMetadataFixture(["^1.2.3", "^1.2.3"]);
  validMetadata.packages[0].dependencies.push(
    { name: "serde", source: null, req: "^1" },
    {
      name: "nanocodex-registry-fixture",
      source: "registry+https://github.com/rust-lang/crates.io-index",
      req: "^99.0.0",
    },
  );
  assertMetadataRequirementCheck(requirementCheck, validMetadata, true);

  for (const requirement of ["^1.2.4", "~1.2.3", "1.2.3", "*"]) {
    assertMetadataRequirementCheck(
      requirementCheck,
      dependencyMetadataFixture(["^1.2.3", requirement]),
      false,
    );
  }
});

function assertBashSyntax(command: string): void {
  const result = spawnSync("bash", ["-n"], {
    input: `${command}\n`,
    encoding: "utf8",
  });
  assert.equal(result.status, 0, result.stderr);
}

function assertNoNetworkOrGitHub(command: string): void {
  assert.doesNotMatch(command, /https?:\/\//i);
  assert.doesNotMatch(command, /\b(?:curl|wget|gh|git)\b/i);
  assert.doesNotMatch(command, /GITHUB_/i);
  assert.match(command, /CARGO_NET_OFFLINE=true/);
}

function dependencyMetadataFixture(requirements: readonly string[]): {
  packages: Array<{
    dependencies: Array<{ name: string; source: string | null; req: string }>;
  }>;
} {
  return {
    packages: requirements.map((req, index) => ({
      dependencies: [{ name: `nanocodex-fixture-${index}`, source: null, req }],
    })),
  };
}

function assertMetadataRequirementCheck(
  requirementCheck: string,
  metadata: unknown,
  expectedToPass: boolean,
): void {
  const result = spawnSync(
    "bash",
    [
      "-c",
      `cargo() { printf '%s' "$NANOCODEX_METADATA_FIXTURE"; }\n${requirementCheck}`,
    ],
    {
      encoding: "utf8",
      env: {
        ...process.env,
        NANOCODEX_METADATA_FIXTURE: JSON.stringify(metadata),
      },
    },
  );
  if (expectedToPass) {
    assert.equal(result.status, 0, result.stderr);
  } else {
    assert.notEqual(result.status, 0, "invalid dependency metadata passed");
  }
}

function countMatches(value: string, pattern: RegExp): number {
  return [...value.matchAll(pattern)].length;
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
