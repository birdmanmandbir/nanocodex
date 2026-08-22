import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  stat,
  symlink,
  utimes,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { test } from "node:test";

import {
  bindingsArtifactCommand,
  bindingsCommand,
  bindingsBuildCacheCommand,
  bindingsBuildCacheInputs,
  bindingsResultCacheCommand,
  bindingsResultCacheInputs,
  cargoCacheInputs,
  cargoDependencyCommand,
  dependencyPolicyCacheInputs,
  exactSourceCacheInputs,
  msrvBuildCacheCommand,
  msrvBuildCacheInputs,
  parallelCommandGroups,
  pythonCacheInputs,
  pythonCommand,
  refreshSourceCommand,
  rustBuildCacheInputs,
  rustBuildCacheCommand,
  rustResultCacheCommand,
  rustResultCacheInputs,
  rustQualityCacheInputs,
  rustPipeline,
  rustSecPolicyCommand,
  typosCommand,
  websiteCommand,
  websiteArtifactCommand,
  websiteDependencyCacheInputs,
  websiteDependencyCommand,
  websiteResultCacheCommand,
  websiteResultCacheInputs,
} from "./ciWorkflowPlan.ts";

const RUSTSEC = {
  url: `https://ci.example/api/ci/rustsec-advisory-db/${"d".repeat(40)}/bundle.tar.gz`,
  revision: "d".repeat(40),
  size: 1_368_315,
  sha256: "e".repeat(64),
};

const BINDINGS_PROJECTS = [
  "js/bindings",
  "js/artifacts",
  "js/react",
  "examples/node",
  "examples/rivet-actors",
  "examples/cloudflare-workers",
  "examples/vercel-workflows",
  "examples/react-vite",
];
const WEBSITE_PROJECTS = ["js/bindings", "js/artifacts", "js/react", "web"];
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

test("the Cloudflare pipeline owns five deterministic Linux Rust gates", () => {
  const jobs = rustPipeline(RUSTSEC);
  assert.deepEqual(jobs.map(({ name }) => name), [
    "stable workspace tests",
    "MSRV workspace tests",
    "quality",
    "dependency policy",
    "static VM guest",
  ]);
  assert.equal(new Set(jobs.map(({ name }) => name)).size, jobs.length);
  assert.ok(jobs.every(({ timeoutMs }) => timeoutMs >= 30 * 60 * 1_000));
  assert.match(jobs[0]!.command, /cargo test --workspace --locked/);
  assert.match(jobs[1]!.command, /cargo \+1\.97 test/);
  assert.match(jobs[2]!.command, /cargo clippy/);
  assert.match(jobs[2]!.command, /cargo doc/);
  assert.match(jobs[3]!.command, /cargo deny --frozen check/);
  assert.match(jobs[4]!.command, /x86_64-unknown-linux-musl/);
});

test("dependency policy verifies and uses only the pinned owned RustSec database", () => {
  const command = rustSecPolicyCommand(RUSTSEC);
  assert.match(command, /curl --fail --silent --show-error/);
  assert.doesNotMatch(command, /--location/);
  assert.match(command, /sha256sum --check --status/);
  assert.match(command, /advisory-dbs\/advisory-db-3157b0e258782691/);
  assert.match(command, /rev-parse --verify 'HEAD\^\{commit\}'/);
  assert.match(command, /status --porcelain --untracked-files=all/);
  assert.match(command, /cargo deny --frozen check/);
  assert.doesNotMatch(command, /typos/);
  assert.ok(command.indexOf("sha256sum") < command.indexOf("tar --extract"));
  assert.ok(command.indexOf("rev-parse") < command.indexOf("cargo deny --frozen"));
  assert.equal(typosCommand(), "typos");
});

test("dependency and Rust compilation snapshots are content addressed", async () => {
  assert.deepEqual(cargoCacheInputs(), [
    "Cargo.lock",
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
    ".cargo/config.toml",
    "web/ci/Dockerfile",
  ]);
  assert.deepEqual(bindingsBuildCacheInputs(), [
    ...cargoCacheInputs(),
    ...BINDINGS_PROJECTS.map((project) => `${project}/package.json`),
    "**/.npmrc",
    ...BINDINGS_PROJECTS.map((project) => `${project}/package-lock.json`),
    ...BINDINGS_WASM_INPUTS,
  ]);
  assert.deepEqual(bindingsResultCacheInputs(), [
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
  ]);
  assert.deepEqual(websiteDependencyCacheInputs(), [
    "web/ci/Dockerfile",
    ...WEBSITE_PROJECTS.map((project) => `${project}/package.json`),
    "**/.npmrc",
    ...WEBSITE_PROJECTS.map((project) => `${project}/package-lock.json`),
    "web/patches/**/*.patch",
  ]);
  assert.deepEqual(exactSourceCacheInputs(), ["/.nanocodex-ci/source-tree"]);
  assert.deepEqual(dependencyPolicyCacheInputs(), [
    ...cargoCacheInputs(),
    "deny.toml",
    "scripts/check-experimental-boundary.sh",
    "scripts/check-crate-boundaries.sh",
    "scripts/check-rustls-provider.sh",
  ]);
  await Promise.all([...new Set([...BINDINGS_PROJECTS, ...WEBSITE_PROJECTS])].flatMap((project) => [
    stat(new URL(`../../${project}/package.json`, import.meta.url)),
    stat(new URL(`../../${project}/package-lock.json`, import.meta.url)),
  ]));
  assert.deepEqual(rustBuildCacheInputs(), [
    ...cargoCacheInputs(),
    "web/ci/rust-source-cache.py",
  ]);
  assert.deepEqual(msrvBuildCacheInputs(), [
    ...cargoCacheInputs(),
    "web/ci/rust-source-cache.py",
  ]);
  assert.deepEqual(rustQualityCacheInputs(), [
    ...cargoCacheInputs(),
    "web/ci/rust-source-cache.py",
    "bin/**/*",
    "crates/**/*",
    "examples/**/*.rs",
    "js/bindings/src/**/*",
    "py/bindings/src/**/*",
  ]);
  assert.deepEqual(rustResultCacheInputs(), [
    ...rustQualityCacheInputs(),
    "tasks/**/*",
    "benchmarks/codex_parity_workload.json",
    "nanocodex.toml",
  ]);
  assert.deepEqual(websiteResultCacheInputs(), [
    ...websiteDependencyCacheInputs(),
    "web/**/*",
    "js/bindings/**/*",
    "js/artifacts/**/*",
    "js/react/**/*",
  ]);
  assert.deepEqual(pythonCacheInputs(), [
    ...rustQualityCacheInputs(),
    "py/bindings/**/*",
    "examples/python/**/*",
  ]);
  assert.ok(
    rustBuildCacheInputs().every((path) => !path.includes("src") && !path.endsWith("**/*")),
    "workspace source changes reuse the compatible Cargo target layer",
  );
  for (const command of [bindingsBuildCacheCommand(), websiteDependencyCommand()]) {
    assert.doesNotMatch(command, /\.node-modules\.tar|\btar\b/);
  }
  assert.match(bindingsBuildCacheCommand(), /\.ci-cache-staging/);
  assert.match(websiteDependencyCommand(), /\.node-modules-staging/);
  assert.match(rustBuildCacheCommand(), /cargo test --workspace --locked --no-run/);
  assert.match(
    msrvBuildCacheCommand(),
    /cargo \+1\.97 test --workspace --locked --no-run/,
  );
  assert.match(rustBuildCacheCommand(), /! -name \.cargo-target/);
  assert.match(msrvBuildCacheCommand(), /! -name \.cargo-target-msrv/);
  assert.match(
    rustBuildCacheCommand(),
    /cargo check --locked --workspace --all-targets --all-features --exclude nanocodex-bin/,
  );
  assert.match(rustBuildCacheCommand(), /--bin nanocodex/);
  assert.match(rustBuildCacheCommand(), /--bench tui_render/);
  assert.doesNotMatch(rustBuildCacheCommand(), /cargo clean/);
  assert.doesNotMatch(msrvBuildCacheCommand(), /cargo .*clean/);
  assert.match(
    refreshSourceCommand("cargo test"),
    /rust-source-cache\.py refresh \/workspace \/workspace\/\.rust-source-manifest\.json/,
  );
  const qualityCache = rustResultCacheCommand("cargo clippy --workspace");
  assert.match(qualityCache, /cargo clippy --workspace/);
  assert.match(qualityCache, /rust-source-cache\.py refresh/);
  assert.match(
    qualityCache,
    /find \/workspace -mindepth 1 -maxdepth 1 -exec rm -rf -- \{\} \+/,
  );
  assert.doesNotMatch(qualityCache, /! -name \.cargo-target/);
  assert.match(bindingsResultCacheCommand(), /\.ci-output/);
  assert.match(bindingsResultCacheCommand(), /\.ci-cache-staging/);
  assert.match(bindingsArtifactCommand(), /sha256sum --check/);
});

test("Cargo dependencies restore the exact owned Git bundle before fetching", () => {
  const command = cargoDependencyCommand(
    "https://ci.example/api/ci/cargo-vendor/0123456789012345678901234567890123456789/bundle.tar.gz",
    3_900_842,
    "a".repeat(64),
  );
  assert.match(command, /curl --fail --location --silent --show-error/);
  assert.match(command, /test "\$\(wc -c/);
  assert.match(command, /sha256sum --check --status/);
  assert.match(command, /tar --extract --gzip/);
  assert.match(command, /--no-same-owner --no-same-permissions/);
  assert.ok(command.indexOf("tar --extract") < command.indexOf("cargo fetch --locked"));
  assert.ok(command.indexOf("cargo fetch --locked") < command.indexOf("find /workspace"));
});

test("npm installs use four fail-fast workers before snapshot pruning", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-ci-npm-install-"));
  const harnessDirectory = await mkdtemp(resolve(tmpdir(), "nanocodex-ci-npm-harness-"));
  try {
    const executableDirectory = resolve(harnessDirectory, "bin");
    const stateDirectory = resolve(harnessDirectory, "state");
    const npm = resolve(executableDirectory, "npm");
    await mkdir(executableDirectory);
    await mkdir(stateDirectory);
    await writeFile(
      npm,
      [
        "#!/bin/sh",
        "project=$3",
        "printf '%s\\n' \"$project\" >> \"$CI_TEST_STATE/projects\"",
        ": > \"$CI_TEST_STATE/active.$$\"",
        "find \"$CI_TEST_STATE\" -name 'active.*' | wc -l >> \"$CI_TEST_STATE/concurrency\"",
        "sleep 0.1",
        "rm \"$CI_TEST_STATE/active.$$\"",
        "mkdir -p \"$project/node_modules/example\"",
        "printf 'installed\\n' > \"$project/node_modules/example/index.js\"",
      ].join("\n"),
    );
    await chmod(npm, 0o755);

    const buildParts = bindingsBuildCacheCommand().split(" && ");
    const install = buildParts[0];
    const verify = buildParts[1];
    const retainNodeModules = buildParts.at(-1);
    assert.ok(install);
    assert.ok(verify);
    assert.ok(retainNodeModules);
    assert.match(install, /xargs -0 -n 1 -P 4/);
    assert.match(install, /npm ci --prefix "\$1" \|\| exit 255/);

    await Promise.all([
      mkdir(resolve(directory, "js/bindings/pkg-node"), { recursive: true }),
      mkdir(resolve(directory, "js/bindings/pkg-web"), { recursive: true }),
    ]);
    await Promise.all([
      writeFile(resolve(directory, "js/bindings/pkg-node/nanocodex.js"), "node\n"),
      writeFile(resolve(directory, "js/bindings/pkg-web/nanocodex.js"), "web\n"),
    ]);
    const adaptedSnapshot = `${verify} && ${retainNodeModules}`.replaceAll(
      "/workspace",
      "$CI_TEST_WORKSPACE",
    );
    const result = spawnSync("bash", ["-c", `${install} && ${adaptedSnapshot}`], {
      cwd: directory,
      encoding: "utf8",
      env: {
        ...process.env,
        CI_TEST_STATE: stateDirectory,
        CI_TEST_WORKSPACE: directory,
        PATH: `${executableDirectory}:${process.env.PATH}`,
      },
    });
    assert.equal(result.status, 0, result.stderr);
    const projects = (await readFile(resolve(stateDirectory, "projects"), "utf8"))
      .trim()
      .split("\n");
    assert.deepEqual(projects.sort(), [...BINDINGS_PROJECTS].sort());
    const concurrency = (await readFile(resolve(stateDirectory, "concurrency"), "utf8"))
      .trim()
      .split("\n")
      .map(Number);
    assert.equal(Math.max(...concurrency), 4);
    assert.ok(concurrency.every((count) => count <= 4));
    await stat(resolve(directory, "js/bindings/node_modules/example/index.js"));
    await assert.rejects(stat(resolve(directory, ".ci-cache-staging")));

    await rm(resolve(stateDirectory, "projects"));
    await rm(resolve(stateDirectory, "concurrency"));
    await writeFile(
      npm,
      [
        "#!/bin/sh",
        "printf '%s\\n' \"$3\" >> \"$CI_TEST_STATE/projects\"",
        "[ \"$3\" != 'js/bindings' ] || exit 1",
        "sleep 0.1",
      ].join("\n"),
    );
    const failed = spawnSync("bash", ["-c", `${install} && ${adaptedSnapshot}`], {
      cwd: directory,
      encoding: "utf8",
      env: {
        ...process.env,
        CI_TEST_STATE: stateDirectory,
        CI_TEST_WORKSPACE: directory,
        PATH: `${executableDirectory}:${process.env.PATH}`,
      },
    });
    assert.notEqual(failed.status, 0);
    const attempted = (await readFile(resolve(stateDirectory, "projects"), "utf8"))
      .trim()
      .split("\n");
    assert.ok(attempted.length < BINDINGS_PROJECTS.length);
    await assert.rejects(stat(resolve(directory, ".ci-cache-staging")));
  } finally {
    await rm(directory, { recursive: true, force: true });
    await rm(harnessDirectory, { recursive: true, force: true });
  }
});

test("bindings, website, and both Python versions preserve the GitHub CI gates", () => {
  const bindingsBuild = bindingsBuildCacheCommand();
  const bindings = bindingsCommand();
  assert.match(bindingsBuild, /wasm32-unknown-unknown/);
  assert.match(bindingsBuild, /build-js-package\.sh/);
  assert.match(bindings, /examples\/vercel-workflows/);
  assert.match(bindings, /examples\/react-vite/);
  assert.match(bindings, /web-wasm\.tar/);
  assert.equal((bindings.match(/group_pid_\d+=\$!/g) ?? []).length, 4);
  assert.equal((bindings.match(/wait "\$group_pid_\d+"/g) ?? []).length, 4);
  assert.match(bindings, /ci group finish:/);
  assert.ok(
    bindings.indexOf("npm run build --prefix js/artifacts") <
      bindings.indexOf("group_pid_0=$!"),
  );
  assert.ok(
    bindings.indexOf('exit "$group_failure"') <
      bindings.indexOf("web-wasm.tar"),
  );
  const website = websiteCommand(
    "https://ci.example/api/ci/runs/0123456789012345678901234567890123456789/artifacts/web-wasm.tar",
    3_500_000,
    "a".repeat(64),
  );
  assert.match(website, /curl --fail --location/);
  assert.match(website, /sha256sum --check --status/);
  assert.match(website, /npm test --prefix web/);
  assert.doesNotMatch(website, /js\/(?:tui|tui-react|terminal)|test:terminal/);
  assert.match(website, /build:from-wasm/);
  assert.match(website, /web-dist\.tar/);
  const websiteResult = websiteResultCacheCommand(
    "https://ci.example/api/ci/runs/0123456789012345678901234567890123456789/artifacts/web-wasm.tar",
    3_500_000,
    "a".repeat(64),
  );
  assert.match(websiteResult, /web-dist\.tar/);
  assert.match(websiteResult, /\.ci-cache-staging/);
  assert.match(websiteArtifactCommand(), /sha256sum --check/);
  for (const version of ["3.11", "3.14"] as const) {
    const python = pythonCommand(version);
    assert.match(python, new RegExp(`/opt/python/${version.replace(".", "\\.")}/bin/python`));
    assert.match(python, /maturin build --locked/);
    assert.match(python, /mypy --strict/);
    assert.match(python, /benchmark_binding\.py --check/);
  }
});

test("parallel command groups execute quoted payloads and propagate failures", () => {
  const success = spawnSync("bash", [
    "-c",
    parallelCommandGroups([
      { label: "first group", command: "printf 'first payload\\n'" },
      { label: "quoted ' group", command: "printf 'second payload\\n'" },
    ]),
  ], { encoding: "utf8" });
  assert.equal(success.status, 0, success.stderr);
  assert.match(success.stdout, /first payload/);
  assert.match(success.stdout, /second payload/);

  const failure = spawnSync("bash", [
    "-c",
    parallelCommandGroups([
      { label: "passing group", command: "true" },
      { label: "failing group", command: "false" },
    ]),
  ], { encoding: "utf8" });
  assert.equal(failure.status, 1, failure.stderr);
});

test("the bindings build snapshot retains generated WASM and node_modules only", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-ci-node-cache-"));
  try {
    await Promise.all(BINDINGS_PROJECTS.map((project) =>
      mkdir(resolve(directory, project, "node_modules"), { recursive: true })
    ));
    const packageDirectory = resolve(directory, "js/bindings/node_modules/example");
    await mkdir(packageDirectory, { recursive: true });
    await mkdir(resolve(directory, "js/bindings/pkg-node"), { recursive: true });
    await mkdir(resolve(directory, "js/bindings/pkg-web"), { recursive: true });
    await mkdir(resolve(directory, ".cargo-home"), { recursive: true });
    await mkdir(resolve(directory, ".cargo-target"), { recursive: true });
    await writeFile(resolve(packageDirectory, "index.js"), "export default 1;\n");
    await writeFile(resolve(directory, "js/bindings/pkg-node/nanocodex.js"), "node\n");
    await writeFile(resolve(directory, "js/bindings/pkg-web/nanocodex.js"), "web\n");
    await writeFile(resolve(directory, "js/bindings/source.js"), "remove me\n");
    await writeFile(resolve(directory, ".cargo-home/cache"), "cargo\n");
    await writeFile(resolve(directory, ".cargo-target/cache"), "target\n");
    await symlink("source.js", resolve(directory, "js/bindings/source-link"));
    const retainNodeModules = bindingsBuildCacheCommand().split(" && ").at(-1);
    assert.ok(retainNodeModules);
    const command = retainNodeModules.replaceAll(
      "/workspace",
      "$CI_TEST_WORKSPACE",
    );
    const result = spawnSync("bash", ["-c", command], {
      cwd: directory,
      encoding: "utf8",
      env: { ...process.env, CI_TEST_WORKSPACE: directory },
    });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(
      await readFile(resolve(directory, "js/bindings/node_modules/example/index.js"), "utf8"),
      "export default 1;\n",
    );
    assert.equal(
      await readFile(resolve(directory, "js/bindings/pkg-node/nanocodex.js"), "utf8"),
      "node\n",
    );
    assert.equal(
      await readFile(resolve(directory, "js/bindings/pkg-web/nanocodex.js"), "utf8"),
      "web\n",
    );
    await assert.rejects(stat(resolve(directory, ".cargo-home")));
    await assert.rejects(stat(resolve(directory, ".cargo-target")));
    await assert.rejects(readFile(resolve(directory, "js/bindings/source.js"), "utf8"));
    await assert.rejects(stat(resolve(directory, "js/bindings/source-link")));
    await assert.rejects(stat(resolve(directory, ".ci-cache-staging")));
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("the website snapshot drops Cargo and unrelated package roots", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-ci-web-cache-"));
  try {
    await Promise.all([
      ...WEBSITE_PROJECTS.map((project) =>
        mkdir(resolve(directory, project, "node_modules/example"), { recursive: true })
      ),
      mkdir(resolve(directory, ".cargo-home"), { recursive: true }),
      mkdir(resolve(directory, ".cargo-target"), { recursive: true }),
      mkdir(resolve(directory, "examples/node"), { recursive: true }),
    ]);
    await Promise.all([
      writeFile(resolve(directory, "web/node_modules/example/index.js"), "site\n"),
      writeFile(resolve(directory, "js/artifacts/node_modules/example/index.js"), "artifacts\n"),
      writeFile(resolve(directory, ".cargo-home/cache"), "remove\n"),
      writeFile(resolve(directory, ".cargo-target/cache"), "remove\n"),
      writeFile(resolve(directory, "examples/node/source.js"), "remove\n"),
    ]);
    const [, , retainNodeModules] = websiteDependencyCommand().split(" && ");
    assert.ok(retainNodeModules);
    const command = retainNodeModules.replaceAll(
      "/workspace",
      "$CI_TEST_WORKSPACE",
    );
    const result = spawnSync("bash", ["-c", command], {
      cwd: directory,
      encoding: "utf8",
      env: { ...process.env, CI_TEST_WORKSPACE: directory },
    });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(
      await readFile(resolve(directory, "web/node_modules/example/index.js"), "utf8"),
      "site\n",
    );
    assert.equal(
      await readFile(resolve(directory, "js/artifacts/node_modules/example/index.js"), "utf8"),
      "artifacts\n",
    );
    await assert.rejects(stat(resolve(directory, ".cargo-home")));
    await assert.rejects(stat(resolve(directory, ".cargo-target")));
    await assert.rejects(stat(resolve(directory, "examples/node")));
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("the Rust cache preserves unchanged crate mtimes and invalidates only changed source", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-ci-rust-cache-"));
  try {
    const source = resolve(directory, "crates/example/src/lib.rs");
    const unchangedSource = resolve(directory, "crates/example/src/unchanged.rs");
    const manifest = resolve(directory, ".rust-source-manifest.json");
    await Promise.all([
      mkdir(resolve(directory, ".cargo"), { recursive: true }),
      mkdir(resolve(directory, "bin"), { recursive: true }),
      mkdir(resolve(directory, "crates/example/src"), { recursive: true }),
      mkdir(resolve(directory, "examples"), { recursive: true }),
      mkdir(resolve(directory, "js/bindings/src"), { recursive: true }),
      mkdir(resolve(directory, "py/bindings/src"), { recursive: true }),
      mkdir(resolve(directory, "web/ci"), { recursive: true }),
    ]);
    await Promise.all([
      writeFile(resolve(directory, "Cargo.toml"), "[workspace]\n"),
      writeFile(resolve(directory, "Cargo.lock"), "version = 4\n"),
      writeFile(resolve(directory, ".cargo/config.toml"), "[net]\noffline = true\n"),
      writeFile(resolve(directory, "js/bindings/Cargo.toml"), "[package]\nname = \"js\"\n"),
      writeFile(resolve(directory, "py/bindings/Cargo.toml"), "[package]\nname = \"py\"\n"),
      writeFile(resolve(directory, "examples/Cargo.toml"), "[package]\nname = \"examples\"\n"),
      writeFile(resolve(directory, "web/style.css"), "body { color: black; }\n"),
      writeFile(
        resolve(directory, "web/ci/rust-source-cache.py"),
        await readFile(new URL("../ci/rust-source-cache.py", import.meta.url), "utf8"),
      ),
    ]);
    await Promise.all([
      writeFile(source, "pub fn value() -> u8 { 1 }\n"),
      writeFile(unchangedSource, "pub fn unchanged() -> u8 { 1 }\n"),
    ]);
    const marker = rustBuildCacheCommand().split(" && ")[0]!;
    const adapt = (command: string) => command.replaceAll(
      "/workspace",
      "$CI_TEST_WORKSPACE",
    );
    const run = (command: string) => spawnSync("bash", ["-c", adapt(command)], {
      cwd: directory,
      encoding: "utf8",
      env: { ...process.env, CI_TEST_WORKSPACE: directory },
    });

    assert.equal(run(marker).status, 0);
    const exactReference = (await stat(manifest)).mtimeMs;
    const freshArchiveTime = new Date(Date.now() + 30_000);
    await Promise.all([
      utimes(source, freshArchiveTime, freshArchiveTime),
      utimes(unchangedSource, freshArchiveTime, freshArchiveTime),
    ]);
    const exact = run(refreshSourceCommand("true"));
    assert.equal(exact.status, 0, exact.stderr);
    assert.ok(Math.abs((await stat(source)).mtimeMs - exactReference) < 2);
    assert.ok(Math.abs((await stat(unchangedSource)).mtimeMs - exactReference) < 2);

    assert.equal(run(marker).status, 0);
    const webOnlyReference = (await stat(manifest)).mtimeMs;
    await writeFile(resolve(directory, "web/style.css"), "body { color: white; }\n");
    await utimes(source, freshArchiveTime, freshArchiveTime);
    const webOnly = run(refreshSourceCommand("true"));
    assert.equal(webOnly.status, 0, webOnly.stderr);
    assert.ok(Math.abs((await stat(source)).mtimeMs - webOnlyReference) < 2);

    assert.equal(run(marker).status, 0);
    const changedReference = (await stat(manifest)).mtimeMs;
    await writeFile(source, "pub fn value() -> u8 { 2 }\n");
    await Promise.all([
      utimes(source, new Date(1_000), new Date(1_000)),
      utimes(unchangedSource, freshArchiveTime, freshArchiveTime),
    ]);
    const changed = run(refreshSourceCommand("true"));
    assert.equal(changed.status, 0, changed.stderr);
    assert.ok((await stat(source)).mtimeMs > changedReference);
    assert.ok(Math.abs((await stat(unchangedSource)).mtimeMs - changedReference) < 2);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("every generated container command is valid Bash", () => {
  const commands = [
    cargoDependencyCommand(
      "https://ci.example/api/ci/cargo-vendor/0123456789012345678901234567890123456789/bundle.tar.gz",
      3_900_842,
      "a".repeat(64),
    ),
    bindingsBuildCacheCommand(),
    websiteDependencyCommand(),
    rustBuildCacheCommand(),
    msrvBuildCacheCommand(),
    refreshSourceCommand("cargo test"),
    bindingsCommand(),
    bindingsResultCacheCommand(),
    bindingsArtifactCommand(),
    websiteCommand(
      "https://ci.example/api/ci/runs/0123456789012345678901234567890123456789/artifacts/web-wasm.tar",
      3_500_000,
      "a".repeat(64),
    ),
    websiteResultCacheCommand(
      "https://ci.example/api/ci/runs/0123456789012345678901234567890123456789/artifacts/web-wasm.tar",
      3_500_000,
      "a".repeat(64),
    ),
    websiteArtifactCommand(),
    pythonCommand("3.11"),
    pythonCommand("3.14"),
    ...rustPipeline(RUSTSEC).map(({ command }) => command),
  ];
  for (const command of commands) {
    const result = spawnSync("bash", ["-n", "-c", command], { encoding: "utf8" });
    assert.equal(result.status, 0, result.stderr);
  }
});
