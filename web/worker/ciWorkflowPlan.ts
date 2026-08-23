import { EXACT_SOURCE_TREE_PATH } from "./ciSource.ts";

export type CiPipelineStep = {
  name: string;
  command: string;
  timeoutMs: number;
};

const RUST_WORKSPACE_MANIFESTS = [
  "Cargo.toml",
  "bin/nanocodex/Cargo.toml",
  "bin/nanousd/Cargo.toml",
  "bin/nanousd-api/Cargo.toml",
  "crates/nanocodex/Cargo.toml",
  "crates/nanocodex-agent/Cargo.toml",
  "crates/nanocodex-durability/Cargo.toml",
  "crates/experimental/nanocodex-browser/Cargo.toml",
  "crates/experimental/nanocodex-egress/Cargo.toml",
  "crates/experimental/nanocodex-voice/Cargo.toml",
  "crates/experimental/nanocodex-voice-protocol/Cargo.toml",
  "crates/experimental/nanocodex-eval/Cargo.toml",
  "crates/experimental/nanocodex-eval-adapters/Cargo.toml",
  "crates/experimental/nanocodex-vm/Cargo.toml",
  "crates/nanocodex-oai-api/Cargo.toml",
  "crates/nanocodex-observability/Cargo.toml",
  "crates/nanocodex-subagents/Cargo.toml",
  "crates/nanocodex-tools/Cargo.toml",
  "crates/nanocodex-tools/macros/Cargo.toml",
  "examples/Cargo.toml",
  "examples/exe-dev/Cargo.toml",
  "js/bindings/Cargo.toml",
  "py/bindings/Cargo.toml",
];

const BINDINGS_JAVASCRIPT_PROJECTS = [
  "js/bindings",
  "js/artifacts",
  "js/react",
  "examples/node",
  "examples/rivet-actors",
  "examples/cloudflare-workers",
  "examples/vercel-workflows",
  "examples/react-vite",
];

const WEBSITE_JAVASCRIPT_PROJECTS = [
  "js/bindings",
  "js/artifacts",
  "js/react",
  "web",
];

const BINDINGS_WASM_INPUTS = [
  "scripts/build-js-package.sh",
  "js/bindings/scripts/deduplicate-wasm.mjs",
  "js/bindings/scripts/write-package-types.mjs",
  "js/bindings/src/**/*",
  "crates/nanocodex/src/**/*",
  "crates/nanocodex-agent/src/**/*",
  "crates/nanocodex-durability/src/**/*",
  "crates/nanocodex-oai-api/src/**/*",
  "crates/nanocodex-subagents/src/**/*",
  "crates/nanocodex-tools/src/**/*",
  "crates/experimental/nanocodex-voice-protocol/src/**/*",
];

const RUST_SOURCE_CACHE_HELPER = String.raw`#!/usr/bin/env python3
"""Preserve Cargo's incremental graph across source archive overlays."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import stat
import time
from pathlib import Path
from typing import Iterable


MANIFEST_VERSION = 1
EXPLICIT_FILES = (
    "Cargo.toml",
    "Cargo.lock",
    "js/bindings/Cargo.toml",
    "py/bindings/Cargo.toml",
)
RECURSIVE_ROOTS = (
    ".cargo",
    "bin",
    "crates",
    "js/bindings/src",
    "py/bindings/src",
)


def regular_files(directory: Path) -> Iterable[Path]:
    if not directory.is_dir():
        return
    for current, directories, filenames in os.walk(directory, followlinks=False):
        directories.sort()
        filenames.sort()
        current_path = Path(current)
        for filename in filenames:
            path = current_path / filename
            if stat.S_ISREG(path.lstat().st_mode):
                yield path


def source_files(workspace: Path) -> list[Path]:
    files: set[Path] = set()
    for relative in EXPLICIT_FILES:
        path = workspace / relative
        if path.exists() and stat.S_ISREG(path.lstat().st_mode):
            files.add(path)
    for relative in RECURSIVE_ROOTS:
        files.update(regular_files(workspace / relative))
    for path in regular_files(workspace / "examples"):
        if path.name == "Cargo.toml" or path.suffix == ".rs":
            files.add(path)
    return sorted(files, key=lambda path: path.relative_to(workspace).as_posix())


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def fingerprints(workspace: Path) -> dict[str, str]:
    return {
        path.relative_to(workspace).as_posix(): sha256(path)
        for path in source_files(workspace)
    }


def snapshot(workspace: Path, manifest: Path) -> None:
    payload = {
        "version": MANIFEST_VERSION,
        "files": fingerprints(workspace),
    }
    temporary = manifest.with_name(f"{manifest.name}.tmp")
    temporary.write_text(
        json.dumps(payload, sort_keys=True, separators=(",", ":")),
        encoding="utf-8",
    )
    temporary.replace(manifest)
    print(f"rust source cache: recorded {len(payload['files'])} inputs")


def previous_fingerprints(manifest: Path) -> dict[str, str]:
    try:
        payload = json.loads(manifest.read_text(encoding="utf-8"))
    except (FileNotFoundError, json.JSONDecodeError, OSError):
        return {}
    if not isinstance(payload, dict) or payload.get("version") != MANIFEST_VERSION:
        return {}
    files = payload.get("files")
    if not isinstance(files, dict):
        return {}
    if not all(isinstance(path, str) and isinstance(digest, str) for path, digest in files.items()):
        return {}
    return files


def refresh(workspace: Path, manifest: Path) -> None:
    try:
        reference_ns = manifest.stat().st_mtime_ns
    except FileNotFoundError:
        reference_ns = 0
    previous = previous_fingerprints(manifest)
    current = fingerprints(workspace)
    now_ns = time.time_ns()
    unchanged = 0
    changed = 0
    for relative, digest in current.items():
        path = workspace / relative
        if reference_ns > 0 and previous.get(relative) == digest:
            os.utime(path, ns=(reference_ns, reference_ns), follow_symlinks=False)
            unchanged += 1
        else:
            os.utime(path, ns=(now_ns, now_ns), follow_symlinks=False)
            changed += 1
    manifest.unlink(missing_ok=True)
    print(
        f"rust source cache: retained {unchanged} inputs; invalidated {changed} inputs"
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("operation", choices=("snapshot", "refresh"))
    parser.add_argument("workspace", type=Path)
    parser.add_argument("manifest", type=Path)
    arguments = parser.parse_args()
    workspace = arguments.workspace.resolve()
    manifest = arguments.manifest.resolve()
    if arguments.operation == "snapshot":
        snapshot(workspace, manifest)
    else:
        refresh(workspace, manifest)


if __name__ == "__main__":
    main()
`;

export type RustSecAdvisoryBundle = {
  url: string;
  revision: string;
  size: number;
  sha256: string;
};

export function rustPipeline(rustSec: RustSecAdvisoryBundle): CiPipelineStep[] {
  return [
    {
      name: "stable workspace tests",
      command: "cargo test --workspace --locked",
      timeoutMs: 45 * 60 * 1_000,
    },
    {
      name: "MSRV workspace tests",
      command: "cargo +1.97 test --workspace --locked",
      timeoutMs: 45 * 60 * 1_000,
    },
    {
      name: "quality",
      command: [
        "cargo fmt --all -- --check",
        "cargo clippy --workspace --all-targets --all-features --exclude nanocodex-bin -- -D warnings",
        "cargo clippy --package nanocodex-bin --all-features --bin nanocodex -- -D warnings",
        "cargo clippy --package nanocodex-bin --all-features --bench tui_render -- -D warnings",
        "cargo check --locked --package nanocodex-oai-api",
        "cargo check --locked --package nanocodex-observability",
        "cargo check --locked --package nanocodex-tools",
        "cargo check --locked --package nanocodex-agent",
        "cargo check --locked --package nanocodex-examples --bins",
        "RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked",
      ].join(" && "),
      timeoutMs: 60 * 60 * 1_000,
    },
    {
      name: "dependency policy",
      command: rustSecPolicyCommand(rustSec),
      timeoutMs: 30 * 60 * 1_000,
    },
    {
      name: "static VM guest",
      command: [
        "cargo check --locked --package nanocodex-vm --all-targets --no-default-features --features guest-runtime",
        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc cargo build --locked --package nanocodex-vm --bin nanocodex-vm-guest --no-default-features --features guest-runtime --target x86_64-unknown-linux-musl",
      ].join(" && "),
      timeoutMs: 30 * 60 * 1_000,
    },
  ];
}

export function rustSecPolicyCommand(bundle: RustSecAdvisoryBundle): string {
  const archive = "/tmp/nanocodex-ci-rustsec.tar.gz";
  const database = "/workspace/.cargo-home/advisory-dbs/advisory-db-3157b0e258782691";
  return [
    `curl --fail --silent --show-error --output ${archive} ${shellQuote(bundle.url)}`,
    `test "$(wc -c < ${archive} | tr -d ' ')" -eq ${bundle.size}`,
    `printf '%s  %s\\n' ${shellQuote(bundle.sha256)} ${archive} | sha256sum --check --status`,
    "rm -rf /workspace/.cargo-home/advisory-dbs",
    `tar --extract --gzip --file ${archive} --directory /workspace/.cargo-home --no-same-owner --no-same-permissions`,
    `rm ${archive}`,
    `test "$(git -C ${database} rev-parse --verify 'HEAD^{commit}')" = ${shellQuote(bundle.revision)}`,
    `test -z "$(git -C ${database} status --porcelain --untracked-files=all)"`,
    "./scripts/check-experimental-boundary.sh",
    "cargo deny --frozen check",
    "./scripts/check-crate-boundaries.sh",
    "./scripts/check-rustls-provider.sh",
  ].join(" && ");
}

export function dependencyPolicyCacheInputs(): string[] {
  return [
    ...cargoCacheInputs(),
    "deny.toml",
    "scripts/check-experimental-boundary.sh",
    "scripts/check-crate-boundaries.sh",
    "scripts/check-rustls-provider.sh",
  ];
}

export function typosCommand(): string {
  return "typos";
}

export function cargoCacheInputs(): string[] {
  return [
    "Cargo.lock",
    ...RUST_WORKSPACE_MANIFESTS,
    ".cargo/config.toml",
    "web/ci/Dockerfile",
  ];
}

export function cargoDependencyCommand(
  bundleUrl: string,
  bundleSize: number,
  bundleSha256: string,
): string {
  let parsed: URL;
  try {
    parsed = new URL(bundleUrl);
  } catch {
    throw new TypeError("Cargo vendor bundle URL is invalid");
  }
  const identity = parsed.pathname.match(
    /^\/api\/ci\/cargo-vendor\/[a-f0-9]{40}\/([a-f0-9]{64})\/bundle\.tar\.gz$/,
  );
  if (
    !identity || identity[1] !== bundleSha256 ||
    (parsed.protocol !== "https:" && parsed.protocol !== "http:") ||
    parsed.username !== "" || parsed.password !== "" || parsed.search !== "" ||
    parsed.hash !== "" || !Number.isSafeInteger(bundleSize) || bundleSize <= 0
  ) throw new TypeError("Cargo vendor bundle descriptor is inconsistent");
  const bundle = "/tmp/nanocodex-ci-cargo-vendor.tar.gz";
  return [
    "mkdir -p /workspace/.cargo-home",
    `curl --fail --silent --show-error --output ${bundle} ${shellQuote(bundleUrl)}`,
    `test "$(wc -c < ${bundle} | tr -d ' ')" -eq ${bundleSize}`,
    `printf '%s  %s\\n' ${shellQuote(bundleSha256)} ${bundle} | sha256sum --check --status`,
    `tar --extract --gzip --file ${bundle} --directory /workspace/.cargo-home --no-same-owner --no-same-permissions`,
    `rm ${bundle}`,
    "CARGO_NET_OFFLINE=true cargo fetch --locked --offline",
    "find /workspace -mindepth 1 -maxdepth 1 ! -name .cargo-home -exec rm -rf -- {} +",
  ].join(" && ");
}

export function bindingsBuildCacheInputs(): string[] {
  return [
    ...cargoCacheInputs(),
    ...packageInputs(BINDINGS_JAVASCRIPT_PROJECTS),
    "**/.npmrc",
    ...lockfileInputs(BINDINGS_JAVASCRIPT_PROJECTS),
    ...BINDINGS_WASM_INPUTS,
  ];
}

export function bindingsResultCacheInputs(): string[] {
  return [
    ...bindingsBuildCacheInputs(),
    "js/bindings/**/*",
    "js/artifacts/**/*",
    "js/react/**/*",
    "examples/browser-cdn/**/*",
    "examples/node/**/*",
    "examples/rivet-actors/**/*",
    "examples/cloudflare-workers/**/*",
    "examples/vercel-workflows/**/*",
    "examples/react-vite/**/*",
  ];
}

export function websiteDependencyCacheInputs(): string[] {
  return [
    "web/ci/Dockerfile",
    ...packageInputs(WEBSITE_JAVASCRIPT_PROJECTS),
    "**/.npmrc",
    ...lockfileInputs(WEBSITE_JAVASCRIPT_PROJECTS),
    "web/patches/**/*.patch",
  ];
}

export function exactSourceCacheInputs(): string[] {
  // The Nanocodex source adapter resolves this impossible repository path to
  // one synthetic blob containing the complete committed tree fingerprint.
  // That keeps exact-source gates correct without expanding and logging every
  // file in the repository for each runner.
  return [EXACT_SOURCE_TREE_PATH];
}

export function rustBuildCacheInputs(): string[] {
  // The snapshot retains Cargo homes, target output, and a content manifest.
  // After source overlay, unchanged inputs are backdated ahead of the compiled
  // graph while only changed inputs are invalidated. Source content therefore
  // must not fragment this reusable layer. The Worker-owned helper is embedded
  // in the runner command, which is itself part of the cache identity.
  return cargoCacheInputs();
}

export function rustQualityCacheInputs(): string[] {
  // Quality is a deterministic function of the Rust workspace rather than the
  // whole publication. Key its completed graph to every input Cargo can read,
  // including crate documentation and embedded prompts, while allowing a
  // website-only publication to reuse the attested result.
  return [
    ...cargoCacheInputs(),
    "bin/**/*",
    "crates/**/*",
    "examples/**/*.rs",
    "js/bindings/src/**/*",
    "py/bindings/src/**/*",
  ];
}

export function rustResultCacheInputs(): string[] {
  // Workspace tests consume the complete Rust package surface plus the
  // repository-owned task and benchmark fixtures loaded at runtime. Keeping
  // this narrower than the repository lets website-only publications reuse
  // already-attested native and MSRV results without omitting test data.
  return [
    ...rustQualityCacheInputs(),
    "tasks/**/*",
    "benchmarks/codex_parity_workload.json",
    "nanocodex.toml",
  ];
}

export function staticVmCacheInputs(): string[] {
  // This gate builds the guest-runtime feature and all nanocodex-vm targets.
  // Cargo's resolved local closure is VM -> tools -> oai-api; unrelated SDK,
  // binding, CLI, and eval source cannot affect the resulting binaries.
  return [
    ...cargoCacheInputs(),
    "crates/experimental/nanocodex-vm/**/*",
    "crates/nanocodex-oai-api/**/*",
    "crates/nanocodex-tools/**/*",
  ];
}

export function websiteResultCacheInputs(): string[] {
  // The downloaded WASM artifact is content-addressed in the command itself.
  // These are the remaining source inputs consumed by the website tests and
  // production build.
  return [
    ...websiteDependencyCacheInputs(),
    "web/**/*",
    "js/bindings/**/*",
    "js/artifacts/**/*",
    "js/react/**/*",
  ];
}

export function pythonCacheInputs(): string[] {
  // `cargo tree --edges normal -p nanocodex-python` resolves only this local
  // crate closure. In particular, VM/eval/CLI source cannot affect either
  // wheel, so it must not evict two expensive Python-version attestations.
  return [
    ...cargoCacheInputs(),
    "crates/nanocodex/**/*",
    "crates/nanocodex-agent/**/*",
    "crates/nanocodex-durability/**/*",
    "crates/nanocodex-oai-api/**/*",
    "crates/nanocodex-tools/**/*",
    "py/bindings/**/*",
    "examples/python/**/*",
  ];
}

export function msrvBuildCacheInputs(): string[] {
  return cargoCacheInputs();
}

export function bindingsBuildCacheCommand(): string {
  const retainedPaths = [
    ...BINDINGS_JAVASCRIPT_PROJECTS.map((project) => `${project}/node_modules`),
    "js/bindings/pkg-node",
    "js/bindings/pkg-web",
  ];
  return [
    javascriptInstallCommand(BINDINGS_JAVASCRIPT_PROJECTS),
    "cargo clippy --locked --target wasm32-unknown-unknown --package nanocodex-oai-api --package nanocodex-tools --package nanocodex-agent --package nanocodex --package nanocodex-wasm -- -D warnings",
    "./scripts/build-js-package.sh",
    retainWorkspacePathsCommand(retainedPaths),
  ].join(" && ");
}

export function websiteDependencyCommand(): string {
  return [
    javascriptInstallCommand(WEBSITE_JAVASCRIPT_PROJECTS),
    retainNodeModulesCommand(WEBSITE_JAVASCRIPT_PROJECTS),
  ].join(" && ");
}

function javascriptInstallCommand(
  javascriptProjects: readonly string[],
): string {
  const nodeModules = javascriptProjects.map(
    (project) => `${project}/node_modules`,
  );
  return [
    `printf '%s\\0' ${javascriptProjects.map(shellQuote).join(" ")} | xargs -0 -n 1 -P 4 sh -c 'npm ci --prefix "$1" || exit 255' sh`,
    `printf '%s\\0' ${nodeModules.map(shellQuote).join(" ")} | xargs -0 -n 1 test -d`,
  ].join(" && ");
}

function retainWorkspacePathsCommand(paths: readonly string[]): string {
  const retained = paths.map(shellQuote).join(" ");
  const script = [
    "staging=/workspace/.ci-cache-staging",
    "rm -rf \"$staging\"",
    `for retained in ${retained}; do test -e "/workspace/$retained"; mkdir -p "$staging/$(dirname "$retained")"; mv "/workspace/$retained" "$staging/$retained"; done`,
    "cleanup_complete=",
    "for cleanup_attempt in 1 2 3; do find /workspace -mindepth 1 -maxdepth 1 ! -name .ci-cache-staging -exec rm -rf -- {} + || :; remaining=$(find /workspace -mindepth 1 -maxdepth 1 ! -name .ci-cache-staging -print -quit) || break; if test -z \"$remaining\"; then cleanup_complete=1; break; fi; done",
    "test \"$cleanup_complete\" = 1",
    `for retained in ${retained}; do mkdir -p "/workspace/$(dirname "$retained")"; mv "$staging/$retained" "/workspace/$retained"; done`,
    "rm -rf \"$staging\"",
  ].join("; ");
  return `sh -eu -c ${shellQuote(script)}`;
}

function retainNodeModulesCommand(projects: readonly string[]): string {
  const projectList = projects.map(shellQuote).join(" ");
  const script = [
    "staging=/workspace/.node-modules-staging",
    "rm -rf \"$staging\"",
    `for project in ${projectList}; do mkdir -p "$staging/$project"; mv "/workspace/$project/node_modules" "$staging/$project/node_modules"; done`,
    "find /workspace -mindepth 1 -maxdepth 1 ! -name .node-modules-staging -exec rm -rf -- {} +",
    `for project in ${projectList}; do mkdir -p "/workspace/$project"; mv "$staging/$project/node_modules" "/workspace/$project/node_modules"; done`,
    "rm -rf \"$staging\"",
  ].join("; ");
  return `sh -eu -c ${shellQuote(script)}`;
}

function packageInputs(projects: readonly string[]): string[] {
  return projects.map((project) => `${project}/package.json`);
}

function lockfileInputs(projects: readonly string[]): string[] {
  return projects.map((project) => `${project}/package-lock.json`);
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", `'"'"'`)}'`;
}

export function rustBuildCacheCommand(): string {
  return rustCompilationCacheCommand(
    [
      "cargo check --locked --workspace --all-targets --all-features --exclude nanocodex-bin",
      "cargo check --locked --package nanocodex-bin --all-features --bin nanocodex",
      "cargo check --locked --package nanocodex-bin --all-features --bench tui_render",
      "cargo test --workspace --locked --no-run",
    ].join(" && "),
    ".cargo-target",
  );
}

export function msrvBuildCacheCommand(): string {
  return rustCompilationCacheCommand(
    "cargo +1.97 test --workspace --locked --no-run",
    ".cargo-target-msrv",
  );
}

export function rustResultCacheCommand(command: string): string {
  return [
    refreshSourceCommand(command),
    "find /workspace -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +",
  ].join(" && ");
}

function rustCompilationCacheCommand(
  command: string,
  targetDirectory: string,
): string {
  return [
    sourceCacheCommand("snapshot"),
    command,
    `find /workspace -mindepth 1 -maxdepth 1 ! -name .cargo-home ! -name ${targetDirectory} ! -name .rust-source-manifest.json -exec rm -rf -- {} +`,
  ].join(" && ");
}

export function refreshSourceCommand(command: string): string {
  return [
    sourceCacheCommand("refresh"),
    command,
  ].join(" && ");
}

function sourceCacheCommand(operation: "snapshot" | "refresh"): string {
  return `python3 -c ${shellQuote(RUST_SOURCE_CACHE_HELPER)} ${operation} /workspace /workspace/.rust-source-manifest.json`;
}

export function npmPreviewVersion(testedMergeHead: string): string {
  if (!/^[a-f0-9]{40}$/.test(testedMergeHead)) {
    throw new Error("npm preview identity must be a full lowercase tested merge SHA");
  }
  // Pull-request CI executes the synthetic merge commit, just as the existing
  // pkg-pr-new workflow versions the checked-out commit. Keep that exact tested
  // identity in the package version rather than labeling merge-tested bytes as
  // the unmerged pull-request head.
  return `0.0.0-preview-${testedMergeHead}`;
}

export function bindingsCommand(testedPullRequestMergeHead?: string): string {
  const previewVersion = testedPullRequestMergeHead === undefined
    ? undefined
    : npmPreviewVersion(testedPullRequestMergeHead);
  return [
    "npm run build --prefix js/artifacts",
    parallelCommandGroups([
      {
        label: "bindings runtime",
        command: [
          "node --test --test-timeout=15000 js/bindings/test/*.test.mjs",
          "npm run test:performance --prefix js/bindings",
        ].join(" && "),
      },
      {
        label: "bindings package",
        command: [
          "npm run test:typecheck --prefix js/bindings",
          "npm run check:package --prefix js/bindings",
          "node --test examples/browser-cdn/*.test.mjs",
        ].join(" && "),
      },
      {
        label: "JavaScript packages",
        command: [
          "npm test --prefix js/artifacts",
          "npm test --prefix js/react",
          "npm test --prefix examples/node",
        ].join(" && "),
      },
      {
        label: "JavaScript examples",
        command: [
          "npm run check --prefix examples/rivet-actors",
          "npm run check --prefix examples/cloudflare-workers",
          "npm run check --prefix examples/vercel-workflows",
          "npm test --prefix examples/react-vite",
          "npm run build --prefix examples/react-vite",
        ].join(" && "),
      },
    ]),
    "mkdir -p /workspace/.ci-output",
    "tar -C /workspace/js/bindings/pkg-web -cf /workspace/.ci-output/web-wasm.tar .",
    "sha256sum /workspace/.ci-output/web-wasm.tar > /workspace/.ci-output/web-wasm.tar.sha256",
    "package_name=$(cd /workspace/js/bindings && npm pack --pack-destination /workspace/.ci-output --silent)",
    "test -n \"$package_name\" && test \"$package_name\" = \"$(basename \"$package_name\")\"",
    "mv \"/workspace/.ci-output/$package_name\" /workspace/.ci-output/npm-package.tgz",
    "sha256sum /workspace/.ci-output/npm-package.tgz > /workspace/.ci-output/npm-package.tgz.sha256",
    ...(previewVersion === undefined ? [] : [
      `(cd /workspace/js/bindings && npm version ${shellQuote(previewVersion)} --no-git-tag-version --allow-same-version)`,
      "npm run check:package --prefix /workspace/js/bindings",
      "preview_name=$(cd /workspace/js/bindings && npm pack --pack-destination /workspace/.ci-output --silent)",
      "test -n \"$preview_name\" && test \"$preview_name\" = \"$(basename \"$preview_name\")\"",
      "mv \"/workspace/.ci-output/$preview_name\" /workspace/.ci-output/npm-preview.tgz",
      `test "$(tar -xOf /workspace/.ci-output/npm-preview.tgz package/package.json | node -p ${shellQuote("JSON.parse(require('fs').readFileSync(0, 'utf8')).version")})" = ${shellQuote(previewVersion)}`,
      "sha256sum /workspace/.ci-output/npm-preview.tgz > /workspace/.ci-output/npm-preview.tgz.sha256",
    ]),
    "rm -rf /workspace/.cargo-home /workspace/.cargo-target",
  ].join(" && ");
}

export function bindingsResultCacheCommand(testedPullRequestMergeHead?: string): string {
  return [
    bindingsCommand(testedPullRequestMergeHead),
    retainWorkspacePathsCommand([".ci-output"]),
  ].join(" && ");
}

export function bindingsArtifactCommand(testedPullRequestMergeHead?: string): string {
  const previewVersion = testedPullRequestMergeHead === undefined
    ? undefined
    : npmPreviewVersion(testedPullRequestMergeHead);
  return [
    "test -s /workspace/.ci-output/web-wasm.tar",
    "sha256sum --check /workspace/.ci-output/web-wasm.tar.sha256",
    "test -s /workspace/.ci-output/npm-package.tgz",
    "sha256sum --check /workspace/.ci-output/npm-package.tgz.sha256",
    ...(previewVersion === undefined ? [] : [
      "test -s /workspace/.ci-output/npm-preview.tgz",
      "sha256sum --check /workspace/.ci-output/npm-preview.tgz.sha256",
      `test "$(tar -xOf /workspace/.ci-output/npm-preview.tgz package/package.json | node -p ${shellQuote("JSON.parse(require('fs').readFileSync(0, 'utf8')).version")})" = ${shellQuote(previewVersion)}`,
    ]),
  ].join(" && ");
}

export function parallelCommandGroups(
  groups: readonly { label: string; command: string }[],
): string {
  const launches = groups.map(({ label, command }, index) => {
    const body = [
      "group_started=$(date +%s)",
      `printf 'ci group start: %s\\n' ${shellQuote(label)}`,
      command,
      "group_status=$?",
      `printf 'ci group finish: %s (%ss, exit %s)\\n' ${shellQuote(label)} \"$(( $(date +%s) - group_started ))\" \"$group_status\"`,
      'exit "$group_status"',
    ].join("; ");
    return `( ${body} ) & group_pid_${index}=$!`;
  });
  const waits = groups.map(
    (_, index) => `wait \"$group_pid_${index}\" || group_failure=1`,
  );
  return `bash -c ${shellQuote([
    "set -u",
    ...launches,
    "group_failure=0",
    ...waits,
    'exit "$group_failure"',
  ].join("; "))}`;
}

export function websiteCommand(
  wasmUrl: string,
  wasmSize: number,
  wasmSha256: string,
): string {
  const wasmBundle = "/tmp/nanocodex-ci-web-wasm.tar";
  return [
    `curl --fail --location --silent --show-error --output ${wasmBundle} ${shellQuote(wasmUrl)}`,
    `test "$(wc -c < ${wasmBundle} | tr -d ' ')" -eq ${wasmSize}`,
    `printf '%s  %s\\n' ${shellQuote(wasmSha256)} ${wasmBundle} | sha256sum --check --status`,
    "mkdir -p /workspace/js/bindings/pkg-web",
    `tar -xf ${wasmBundle} -C /workspace/js/bindings/pkg-web`,
    `rm ${wasmBundle}`,
    "npm run build --prefix js/artifacts",
    "npm test --prefix web",
    "npm run build:from-wasm --prefix web",
    "mkdir -p /workspace/.ci-output",
    "tar -C /workspace/web/dist -cf /workspace/.ci-output/web-dist.tar .",
    "sha256sum /workspace/.ci-output/web-dist.tar > /workspace/.ci-output/web-dist.tar.sha256",
  ].join(" && ");
}

export function websiteResultCacheCommand(
  wasmUrl: string,
  wasmSize: number,
  wasmSha256: string,
): string {
  return [
    websiteCommand(wasmUrl, wasmSize, wasmSha256),
    retainWorkspacePathsCommand([".ci-output"]),
  ].join(" && ");
}

export function websiteArtifactCommand(): string {
  return "test -s /workspace/.ci-output/web-dist.tar && sha256sum --check /workspace/.ci-output/web-dist.tar.sha256";
}

export function pythonCommand(version: "3.11" | "3.14"): string {
  const python = `/opt/python/${version}/bin/python`;
  const temporary = `/tmp/nanocodex-python-${version.replace(".", "")}`;
  return [
    `${python} -m maturin build --locked --manifest-path py/bindings/Cargo.toml --interpreter ${python} --out dist/python-${version} --release`,
    `${python} py/bindings/benchmarks/check_package_size.py dist/python-${version}/*.whl`,
    `${python} -m venv ${temporary}`,
    `${temporary}/bin/python -m pip install --disable-pip-version-check dist/python-${version}/*.whl`,
    `cd /tmp && ${temporary}/bin/python -m unittest discover -s /workspace/py/bindings/tests -v`,
    `${temporary}/bin/python /workspace/py/bindings/tests/typing_consumer.py`,
    `cd /workspace && ${python} -m mypy --strict --python-executable ${temporary}/bin/python py/bindings/tests/typing_consumer.py`,
    `${temporary}/bin/python py/bindings/benchmarks/benchmark_binding.py --check`,
    `${temporary}/bin/python -m compileall -q examples/python`,
  ].join(" && ");
}
