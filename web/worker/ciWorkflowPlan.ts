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

const JAVASCRIPT_PROJECTS = [
  "js/bindings",
  "js/artifacts",
  "js/react",
  "js/tui",
  "js/tui-react",
  "js/terminal",
  "web",
  "examples/node",
  "examples/rivet-actors",
  "examples/cloudflare-workers",
  "examples/vercel-workflows",
  "examples/react-vite",
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

export function completeDependencyCacheInputs(): string[] {
  return [
    ...cargoCacheInputs(),
    ...JAVASCRIPT_PROJECTS.map((project) => `${project}/package.json`),
    "**/.npmrc",
    ...JAVASCRIPT_PROJECTS.map((project) => `${project}/package-lock.json`),
    "web/patches/**/*.patch",
  ];
}

export function javascriptDependencyCommand(): string {
  const nodeModules = JAVASCRIPT_PROJECTS.map(
    (project) => `${project}/node_modules`,
  );
  return [
    `printf '%s\\0' ${JAVASCRIPT_PROJECTS.map(shellQuote).join(" ")} | xargs -0 -n 1 -P 4 sh -c 'npm ci --prefix "$1" || exit 255' sh`,
    `printf '%s\\0' ${nodeModules.map(shellQuote).join(" ")} | xargs -0 -n 1 test -d`,
    `printf '%s\\0' ${nodeModules.map(shellQuote).join(" ")} | tar --null --files-from=- -cf /workspace/.node-modules.tar`,
    "find /workspace -mindepth 1 -maxdepth 1 ! -name .cargo-home ! -name .cargo-target ! -name .node-modules.tar -exec rm -rf -- {} +",
  ].join(" && ");
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", `'"'"'`)}'`;
}

export function rustBuildCacheCommand(): string {
  return [
    `${sourceFingerprintCommand()} > /workspace/.rust-source-fingerprint`,
    "cargo test --workspace --locked --no-run",
    "find /workspace -mindepth 1 -maxdepth 1 ! -name .cargo-home ! -name .cargo-target ! -name .rust-source-fingerprint -exec rm -rf -- {} +",
  ].join(" && ");
}

export function refreshSourceCommand(command: string): string {
  return [
    `current_source_fingerprint=$(${sourceFingerprintCommand()})`,
    "cached_source_fingerprint=$(cat /workspace/.rust-source-fingerprint 2>/dev/null || true)",
    "if [ \"$current_source_fingerprint\" != \"$cached_source_fingerprint\" ]; then find /workspace -path '*/node_modules' -prune -o -path /workspace/.cargo-home -prune -o -path /workspace/.cargo-target -prune -o -path /workspace/.rust-source-fingerprint -prune -o -type f -exec touch -- {} +; fi",
    "rm -f /workspace/.rust-source-fingerprint",
    command,
  ].join(" && ");
}

function sourceFingerprintCommand(): string {
  return "find /workspace -path '*/node_modules' -prune -o -path /workspace/.cargo-home -prune -o -path /workspace/.cargo-target -prune -o -path /workspace/.rust-source-fingerprint -prune -o -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum";
}

export function bindingsCommand(): string {
  return [
    "tar -xf /workspace/.node-modules.tar -C /workspace",
    "rm /workspace/.node-modules.tar",
    "cargo clippy --locked --target wasm32-unknown-unknown --package nanocodex-oai-api --package nanocodex-tools --package nanocodex-agent --package nanocodex --package nanocodex-wasm -- -D warnings",
    "npm run build --prefix js/artifacts",
    "npm run build --prefix js/tui",
    "./scripts/build-js-package.sh",
    "node --test --test-timeout=15000 js/bindings/test/*.test.mjs",
    "npm run test:performance --prefix js/bindings",
    "npm run test:typecheck --prefix js/bindings",
    "npm run check:package --prefix js/bindings",
    "npm test --prefix js/artifacts",
    "npm test --prefix js/react",
    "npm test --prefix js/tui",
    "npm test --prefix js/tui-react",
    "node --test examples/browser-cdn/*.test.mjs",
    "npm test --prefix examples/node",
    "npm run check --prefix examples/rivet-actors",
    "npm run check --prefix examples/cloudflare-workers",
    "npm run check --prefix examples/vercel-workflows",
    "npm test --prefix examples/react-vite",
    "npm run build --prefix examples/react-vite",
    "mkdir -p /workspace/.ci-output",
    "tar -C /workspace/js/bindings/pkg-web -cf /workspace/.ci-output/web-wasm.tar .",
    "sha256sum /workspace/.ci-output/web-wasm.tar > /workspace/.ci-output/web-wasm.tar.sha256",
    "rm -rf /workspace/.cargo-home /workspace/.cargo-target",
  ].join(" && ");
}

export function websiteCommand(
  wasmUrl: string,
  wasmSize: number,
  wasmSha256: string,
): string {
  const wasmBundle = "/tmp/nanocodex-ci-web-wasm.tar";
  return [
    "tar -xf /workspace/.node-modules.tar -C /workspace",
    "rm /workspace/.node-modules.tar",
    `curl --fail --location --silent --show-error --output ${wasmBundle} ${shellQuote(wasmUrl)}`,
    `test "$(wc -c < ${wasmBundle} | tr -d ' ')" -eq ${wasmSize}`,
    `printf '%s  %s\\n' ${shellQuote(wasmSha256)} ${wasmBundle} | sha256sum --check --status`,
    "mkdir -p /workspace/js/bindings/pkg-web",
    `tar -xf ${wasmBundle} -C /workspace/js/bindings/pkg-web`,
    `rm ${wasmBundle}`,
    "npm run build --prefix js/artifacts",
    "npm run build --prefix js/tui",
    "npm run build --prefix js/tui-react",
    "npm run test:terminal --prefix web",
    "node --experimental-strip-types --test web/worker/*.test.ts web/test/*.test.ts web/scripts/*.test.mjs",
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
