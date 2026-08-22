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
  bindingsCommand,
  bindingsDependencyCacheInputs,
  bindingsDependencyCommand,
  cargoCacheInputs,
  cargoDependencyCommand,
  pythonCommand,
  refreshSourceCommand,
  rustBuildCacheInputs,
  rustBuildCacheCommand,
  rustPipeline,
  rustSecPolicyCommand,
  websiteCommand,
  websiteDependencyCacheInputs,
  websiteDependencyCommand,
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
  assert.ok(command.indexOf("sha256sum") < command.indexOf("tar --extract"));
  assert.ok(command.indexOf("rev-parse") < command.indexOf("cargo deny --frozen"));
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
  assert.deepEqual(bindingsDependencyCacheInputs(), [
    ...cargoCacheInputs(),
    ...BINDINGS_PROJECTS.map((project) => `${project}/package.json`),
    "**/.npmrc",
    ...BINDINGS_PROJECTS.map((project) => `${project}/package-lock.json`),
  ]);
  assert.deepEqual(websiteDependencyCacheInputs(), [
    "web/ci/Dockerfile",
    ...WEBSITE_PROJECTS.map((project) => `${project}/package.json`),
    "**/.npmrc",
    ...WEBSITE_PROJECTS.map((project) => `${project}/package-lock.json`),
    "web/patches/**/*.patch",
  ]);
  await Promise.all([...new Set([...BINDINGS_PROJECTS, ...WEBSITE_PROJECTS])].flatMap((project) => [
    stat(new URL(`../../${project}/package.json`, import.meta.url)),
    stat(new URL(`../../${project}/package-lock.json`, import.meta.url)),
  ]));
  assert.deepEqual(rustBuildCacheInputs(), cargoCacheInputs());
  assert.ok(
    rustBuildCacheInputs().every((path) => !path.includes("src") && !path.endsWith("**/*")),
    "workspace source changes reuse the compatible Cargo target layer",
  );
  for (const command of [bindingsDependencyCommand(), websiteDependencyCommand()]) {
    assert.doesNotMatch(command, /\.node-modules\.tar|\btar\b/);
    assert.match(command, /\.node-modules-staging/);
  }
  assert.match(rustBuildCacheCommand(), /cargo test --workspace --locked --no-run/);
  assert.match(rustBuildCacheCommand(), /! -name \.cargo-target/);
  assert.match(refreshSourceCommand("cargo test"), /-exec touch/);
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

    const [install, verify, retainNodeModules] = bindingsDependencyCommand().split(" && ");
    assert.ok(install);
    assert.ok(verify);
    assert.ok(retainNodeModules);
    assert.match(install, /xargs -0 -n 1 -P 4/);
    assert.match(install, /npm ci --prefix "\$1" \|\| exit 255/);

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
    await assert.rejects(stat(resolve(directory, ".node-modules.tar")));

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
    await assert.rejects(stat(resolve(directory, ".node-modules.tar")));
  } finally {
    await rm(directory, { recursive: true, force: true });
    await rm(harnessDirectory, { recursive: true, force: true });
  }
});

test("bindings, website, and both Python versions preserve the GitHub CI gates", () => {
  const bindings = bindingsCommand();
  assert.match(bindings, /wasm32-unknown-unknown/);
  assert.match(bindings, /build-js-package\.sh/);
  assert.match(bindings, /examples\/vercel-workflows/);
  assert.match(bindings, /examples\/react-vite/);
  assert.match(bindings, /web-wasm\.tar/);
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
  for (const version of ["3.11", "3.14"] as const) {
    const python = pythonCommand(version);
    assert.match(python, new RegExp(`/opt/python/${version.replace(".", "\\.")}/bin/python`));
    assert.match(python, /maturin build --locked/);
    assert.match(python, /mypy --strict/);
    assert.match(python, /benchmark_binding\.py --check/);
  }
});

test("the bindings snapshot retains only its node_modules and Cargo roots", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-ci-node-cache-"));
  try {
    await Promise.all(BINDINGS_PROJECTS.map((project) =>
      mkdir(resolve(directory, project, "node_modules"), { recursive: true })
    ));
    const packageDirectory = resolve(directory, "js/bindings/node_modules/example");
    await mkdir(packageDirectory, { recursive: true });
    await mkdir(resolve(directory, ".cargo-home"), { recursive: true });
    await mkdir(resolve(directory, ".cargo-target"), { recursive: true });
    await writeFile(resolve(packageDirectory, "index.js"), "export default 1;\n");
    await writeFile(resolve(directory, "js/bindings/source.js"), "remove me\n");
    await writeFile(resolve(directory, ".cargo-home/cache"), "cargo\n");
    await writeFile(resolve(directory, ".cargo-target/cache"), "target\n");
    await symlink("source.js", resolve(directory, "js/bindings/source-link"));
    const [, , retainNodeModules] = bindingsDependencyCommand().split(" && ");
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
    assert.equal(await readFile(resolve(directory, ".cargo-home/cache"), "utf8"), "cargo\n");
    assert.equal(await readFile(resolve(directory, ".cargo-target/cache"), "utf8"), "target\n");
    await assert.rejects(readFile(resolve(directory, "js/bindings/source.js"), "utf8"));
    await assert.rejects(stat(resolve(directory, "js/bindings/source-link")));
    await assert.rejects(stat(resolve(directory, ".node-modules.tar")));
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

test("the Rust cache preserves exact reruns and refreshes changed source", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-ci-rust-cache-"));
  try {
    const source = resolve(directory, "crates/example/src/lib.rs");
    await Promise.all([
      mkdir(resolve(directory, ".cargo"), { recursive: true }),
      mkdir(resolve(directory, "bin"), { recursive: true }),
      mkdir(resolve(directory, "crates/example/src"), { recursive: true }),
      mkdir(resolve(directory, "examples"), { recursive: true }),
      mkdir(resolve(directory, "js/bindings/src"), { recursive: true }),
      mkdir(resolve(directory, "py/bindings/src"), { recursive: true }),
      mkdir(resolve(directory, "web"), { recursive: true }),
    ]);
    await Promise.all([
      writeFile(resolve(directory, "Cargo.toml"), "[workspace]\n"),
      writeFile(resolve(directory, "Cargo.lock"), "version = 4\n"),
      writeFile(resolve(directory, ".cargo/config.toml"), "[net]\noffline = true\n"),
      writeFile(resolve(directory, "js/bindings/Cargo.toml"), "[package]\nname = \"js\"\n"),
      writeFile(resolve(directory, "py/bindings/Cargo.toml"), "[package]\nname = \"py\"\n"),
      writeFile(resolve(directory, "examples/Cargo.toml"), "[package]\nname = \"examples\"\n"),
      writeFile(resolve(directory, "web/style.css"), "body { color: black; }\n"),
    ]);
    await writeFile(source, "pub fn value() -> u8 { 1 }\n");
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
    await utimes(source, new Date(1_000), new Date(1_000));
    const exact = run(refreshSourceCommand("true"));
    assert.equal(exact.status, 0, exact.stderr);
    assert.equal((await stat(source)).mtimeMs, 1_000);

    assert.equal(run(marker).status, 0);
    await writeFile(resolve(directory, "web/style.css"), "body { color: white; }\n");
    await utimes(source, new Date(1_000), new Date(1_000));
    const webOnly = run(refreshSourceCommand("true"));
    assert.equal(webOnly.status, 0, webOnly.stderr);
    assert.equal((await stat(source)).mtimeMs, 1_000);

    assert.equal(run(marker).status, 0);
    await writeFile(source, "pub fn value() -> u8 { 2 }\n");
    await utimes(source, new Date(1_000), new Date(1_000));
    const changed = run(refreshSourceCommand("true"));
    assert.equal(changed.status, 0, changed.stderr);
    assert.ok((await stat(source)).mtimeMs > 1_000);
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
    bindingsDependencyCommand(),
    websiteDependencyCommand(),
    rustBuildCacheCommand(),
    refreshSourceCommand("cargo test"),
    bindingsCommand(),
    websiteCommand(
      "https://ci.example/api/ci/runs/0123456789012345678901234567890123456789/artifacts/web-wasm.tar",
      3_500_000,
      "a".repeat(64),
    ),
    pythonCommand("3.11"),
    pythonCommand("3.14"),
    ...rustPipeline(RUSTSEC).map(({ command }) => command),
  ];
  for (const command of commands) {
    const result = spawnSync("bash", ["-n", "-c", command], { encoding: "utf8" });
    assert.equal(result.status, 0, result.stderr);
  }
});
