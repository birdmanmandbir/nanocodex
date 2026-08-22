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
    "typos",
  ].join(" && ");
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
  const bundle = "/tmp/nanocodex-ci-cargo-vendor.tar.gz";
  return [
    "mkdir -p /workspace/.cargo-home",
    `curl --fail --location --silent --show-error --output ${bundle} ${shellQuote(bundleUrl)}`,
    `test "$(wc -c < ${bundle} | tr -d ' ')" -eq ${bundleSize}`,
    `printf '%s  %s\\n' ${shellQuote(bundleSha256)} ${bundle} | sha256sum --check --status`,
    `tar --extract --gzip --file ${bundle} --directory /workspace/.cargo-home --no-same-owner --no-same-permissions`,
    `rm ${bundle}`,
    "cargo fetch --locked",
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

export function websiteDependencyCacheInputs(): string[] {
  return [
    "web/ci/Dockerfile",
    ...packageInputs(WEBSITE_JAVASCRIPT_PROJECTS),
    "**/.npmrc",
    ...lockfileInputs(WEBSITE_JAVASCRIPT_PROJECTS),
    "web/patches/**/*.patch",
  ];
}

export function rustBuildCacheInputs(): string[] {
  // The snapshot retains only Cargo homes, target output, and a fingerprint of
  // the source that produced it. A restored runner overlays the exact current
  // tree and touches every Rust input when that fingerprint changes, so Cargo
  // safely reuses dependency artifacts while rebuilding affected workspace
  // crates. Source content therefore must not fragment this reusable layer.
  return cargoCacheInputs();
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
    "find /workspace -mindepth 1 -maxdepth 1 ! -name .ci-cache-staging -exec rm -rf -- {} +",
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

function rustCompilationCacheCommand(
  command: string,
  targetDirectory: string,
): string {
  return [
    `${sourceFingerprintCommand()} > /workspace/.rust-source-fingerprint`,
    command,
    `find /workspace -mindepth 1 -maxdepth 1 ! -name .cargo-home ! -name ${targetDirectory} ! -name .rust-source-fingerprint -exec rm -rf -- {} +`,
  ].join(" && ");
}

export function refreshSourceCommand(command: string): string {
  return [
    `current_source_fingerprint=$(${sourceFingerprintCommand()})`,
    "cached_source_fingerprint=$(cat /workspace/.rust-source-fingerprint 2>/dev/null || true)",
    "if [ \"$current_source_fingerprint\" != \"$cached_source_fingerprint\" ]; then find /workspace -path '*/node_modules' -prune -o -path /workspace/.cargo-home -prune -o -path /workspace/.cargo-target -prune -o -path /workspace/.cargo-target-msrv -prune -o -path /workspace/.rust-source-fingerprint -prune -o -type f -exec touch -- {} +; fi",
    "rm -f /workspace/.rust-source-fingerprint",
    command,
  ].join(" && ");
}

function sourceFingerprintCommand(): string {
  return "{ find /workspace/Cargo.toml /workspace/Cargo.lock /workspace/.cargo /workspace/bin /workspace/crates /workspace/js/bindings/Cargo.toml /workspace/js/bindings/src /workspace/py/bindings/Cargo.toml /workspace/py/bindings/src -type f -print0; find /workspace/examples -type f \\( -name '*.rs' -o -name Cargo.toml \\) -print0; } | sort -z | xargs -0 sha256sum | sha256sum";
}

export function bindingsCommand(): string {
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
    "rm -rf /workspace/.cargo-home /workspace/.cargo-target",
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
