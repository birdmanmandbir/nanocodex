import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";

test("Wrangler owns the Workflow, source, backup, repository, and production CI capacity", async () => {
  const config = JSON.parse(await readFile(new URL("../wrangler.jsonc", import.meta.url), "utf8"));
  assert.deepEqual(config.workflows, [{
    binding: "CI_WORKFLOW",
    name: "nanocodex-ci",
    class_name: "NanocodexCI",
  }]);
  assert.deepEqual(
    config.containers.find(({ class_name }: { class_name: string }) => class_name === "CiSandbox"),
    {
      class_name: "CiSandbox",
      image: "./ci/Dockerfile",
      max_instances: 32,
      instance_type: "standard-4",
    },
  );
  assert.ok(config.durable_objects.bindings.some(
    ({ name, class_name }: { name: string; class_name: string }) =>
      name === "CI_REPOSITORY" && class_name === "CiRepository",
  ));
  assert.ok(config.durable_objects.bindings.some(
    ({ name, class_name }: { name: string; class_name: string }) =>
      name === "SANDBOX" && class_name === "CiSandbox",
  ));
  assert.ok(config.durable_objects.bindings.some(
    ({ name, class_name }: { name: string; class_name: string }) =>
      name === "CI_MACOS_JOBS" && class_name === "CiMacJobs",
  ));
  assert.ok(config.durable_objects.bindings.some(
    ({ name, class_name }: { name: string; class_name: string }) =>
      name === "CI_RELEASES" && class_name === "CiReleases",
  ));
  assert.deepEqual(config.migrations.at(-1), {
    tag: "v10",
    new_sqlite_classes: ["CiMacJobs", "CiReleases"],
  });
  assert.deepEqual(
    config.r2_buckets.filter(({ binding }: { binding: string }) =>
      binding === "CI_SOURCE" || binding === "BACKUP_BUCKET"),
    [
      { binding: "CI_SOURCE", bucket_name: "nanocodex-ci-source" },
      { binding: "BACKUP_BUCKET", bucket_name: "nanocodex-ci" },
    ],
  );
  assert.equal(config.vars.BACKUP_BUCKET_NAME, "nanocodex-ci");
  assert.equal(config.vars.CI_PUBLIC_ORIGIN, "https://nanocodex.me-7fb.workers.dev");
  assert.deepEqual(config.secrets.required, [
    "GIT_MIRROR_TOKEN",
    "CI_MASTER_SOURCE_WRITE_TOKEN",
    "CI_PR_SOURCE_WRITE_TOKEN",
    "CI_CONTROL_TOKEN",
    "CI_MACOS_RUNNER_TOKEN",
    "CI_RELEASE_TOKEN",
    "NANOCODEX_SANDBOX_CONTROL_TOKEN",
    "R2_ACCESS_KEY_ID",
    "R2_SECRET_ACCESS_KEY",
  ]);
  assert.deepEqual(config.triggers.crons, ["0 5 * * *"]);
  assert.equal(config.r2_buckets.some(
    ({ binding }: { binding: string }) => binding === "ARTIFACTS",
  ), false);
  assert.equal(JSON.stringify(config).toLowerCase().includes("github"), false);
});

test("development repeats every non-inherited CI binding", async () => {
  const config = JSON.parse(await readFile(new URL("../wrangler.jsonc", import.meta.url), "utf8"));
  const development = config.env.development;
  assert.equal(development.workflows[0].binding, "CI_WORKFLOW");
  assert.ok(development.containers.some(
    ({ class_name }: { class_name: string }) => class_name === "CiSandbox",
  ));
  assert.ok(development.durable_objects.bindings.some(
    ({ name }: { name: string }) => name === "CI_REPOSITORY",
  ));
  assert.ok(development.durable_objects.bindings.some(
    ({ name }: { name: string }) => name === "CI_MACOS_JOBS",
  ));
  assert.ok(development.durable_objects.bindings.some(
    ({ name }: { name: string }) => name === "CI_RELEASES",
  ));
  assert.ok(development.r2_buckets.some(
    ({ binding }: { binding: string }) => binding === "CI_SOURCE",
  ));
  assert.ok(development.r2_buckets.some(
    ({ binding }: { binding: string }) => binding === "BACKUP_BUCKET",
  ));
  assert.deepEqual(
    development.r2_buckets.filter(({ binding }: { binding: string }) =>
      binding === "CI_SOURCE" || binding === "BACKUP_BUCKET"),
    [
      { binding: "CI_SOURCE", bucket_name: "nanocodex-ci-source-development" },
      { binding: "BACKUP_BUCKET", bucket_name: "nanocodex-ci-development" },
    ],
  );
  assert.equal(development.vars.BACKUP_BUCKET_NAME, "nanocodex-ci-development");
  assert.deepEqual(development.triggers.crons, []);
  assert.deepEqual(development.secrets.required, config.secrets.required);
});

test("the local CI command enables container execution on the public runner origin", async () => {
  const packageDocument = JSON.parse(
    await readFile(new URL("../package.json", import.meta.url), "utf8"),
  );
  const command = packageDocument.scripts["dev:ci"] as string;
  assert.match(command, /NANOCODEX_DEV_CONTAINERS=1/);
  assert.match(command, /CLOUDFLARE_ENV=development/);
  assert.match(command, /wrangler\.js dev --enable-containers/);
  assert.match(command, /--port 8787 --ip 127\.0\.0\.1/);
  assert.equal(
    packageDocument.scripts["ci:macos-runner"],
    "node scripts/ci-macos-runner.mjs",
  );
  assert.equal(packageDocument.scripts["ci:controller"], "node scripts/ci-controller.mjs");
  assert.equal(
    packageDocument.scripts["ci:pr-controller"],
    "node scripts/ci-pr-controller.mjs",
  );
  assert.equal(
    packageDocument.scripts["ci:release-controller"],
    "node scripts/ci-release-controller.mjs",
  );
  assert.equal(
    packageDocument.scripts["ci:install-controller-service"],
    "node scripts/install-ci-controller-service.mjs",
  );
  assert.equal(
    packageDocument.scripts["ci:install-macos-service"],
    "node scripts/install-ci-macos-service.mjs",
  );
});

test("artifact and deterministic gates use bounded reusable cache entries", async () => {
  const [workflow, sandboxRunner, runnerGroup, cache] = await Promise.all([
    readFile(new URL("./ciWorkflow.ts", import.meta.url), "utf8"),
    readFile(
      new URL("../node_modules/@cloudflare/ci/src/ci/runners/sandbox.ts", import.meta.url),
      "utf8",
    ),
    readFile(
      new URL("../node_modules/@cloudflare/ci/src/pipeline/runner-group.ts", import.meta.url),
      "utf8",
    ),
    readFile(
      new URL("../node_modules/@cloudflare/ci/src/ci/cache.ts", import.meta.url),
      "utf8",
    ),
  ]);
  assert.match(
    workflow,
    /cacheInputs \? 30 \* 24 \* 60 \* 60 : 24 \* 60 \* 60[\s\S]*?cacheInputs != null/,
  );
  assert.match(
    workflow,
    /command: rustResultCacheCommand\(qualityJob\.command\)[\s\S]*?rustQualityCacheInputs\(\)/,
  );
  assert.match(
    workflow,
    /job\.name === "static VM guest"[\s\S]*?command: cleanupAfter\(job\.command\)[\s\S]*?staticVmCacheInputs\(\)/,
  );
  assert.match(
    workflow,
    /job\.name === "dependency policy"[\s\S]*?command: cleanupAfter\(job\.command\)[\s\S]*?dependencyPolicyCacheInputs\(\)/,
  );
  assert.match(
    workflow,
    /name: "typos"[\s\S]*?command: cleanupAfter\(typosCommand\(\)\)[\s\S]*?cacheInputs: exactSourceCacheInputs\(\)/,
  );
  assert.match(
    workflow,
    /name,[\s\S]*?cache: \{ inputs: pythonCacheInputs\(\) \},[\s\S]*?runnerConfig\(40 \* 60 \* 1_000, 30 \* 24 \* 60 \* 60\)/,
  );
  assert.match(workflow, /path: "\/workspace\/\.ci-output\/web-wasm\.tar"/);
  assert.match(workflow, /path: "\/workspace\/\.ci-output\/web-dist\.tar"/);
  assert.match(sandboxRunner, /localBucket: this\.env\.ENVIRONMENT === 'development'/);
  assert.match(sandboxRunner, /transport: 'http'/);
  assert.match(workflow, /name: "Node and browser bindings"[\s\S]*?runnerConfig\(60 \* 60 \* 1_000, 24 \* 60 \* 60, 0, false\)/);
  assert.match(workflow, /prepareCachedLayer\(\s*dependencies,\s*"Bindings build cache"/);
  assert.match(workflow, /prepareCachedLayer\(\s*ci,\s*"Website dependencies"/);
  assert.match(
    workflow,
    /const webPreparation = \(async \(\) => \{[\s\S]*?bindingsBuildStatePromise[\s\S]*?websiteDependencyStatePromise[\s\S]*?return \{ bindingsBuildState, websiteDependencyState \};/,
  );
  assert.match(
    workflow,
    /const bindingsVerification = await bindingsBuildState\.result\.runner[\s\S]*?cache: \{ inputs: bindingsResultCacheInputs\(\) \}/,
  );
  assert.match(
    workflow,
    /const bindingsArtifact = await bindingsVerification\.runner[\s\S]*?command: bindingsArtifactCommand\(npmPreview\?\.mergeHead\)[\s\S]*?outputs:/,
  );
  assert.match(
    workflow,
    /retainContentAddressedArtifact[\s\S]*?const websiteVerification = await websiteDependencyState\.result\.runner[\s\S]*?\/api\/ci\/artifacts\/\$\{wasmContentKey\}[\s\S]*?cache: \{ inputs: websiteResultCacheInputs\(\) \}/,
  );
  assert.match(
    workflow,
    /const websiteArtifact = await websiteVerification\.runner[\s\S]*?command: websiteArtifactCommand\(\)[\s\S]*?outputs:/,
  );
  assert.doesNotMatch(workflow, /const websiteVerification = await bindings\.runner/);
  assert.match(workflow, /outputs\?\.\[0\]/);
  assert.doesNotMatch(workflow, /getSandbox/);
  assert.match(sandboxRunner, /input\.createSnapshot/);
  assert.match(sandboxRunner, /persistOutputs/);
  assert.match(sandboxRunner, /streamFile/);
  assert.equal(
    (sandboxRunner.match(/decodeFileStream\(\s*await sandbox\.readFileStream/g) ?? []).length,
    2,
  );
  assert.match(sandboxRunner, /checksum mismatch/);
  assert.match(sandboxRunner, /new FixedLengthStream\(size\)/);
  assert.match(sandboxRunner, /putMultipartStream\(bucket, key, value, size, options\)/);
  assert.match(sandboxRunner, /metadata\.stdout\.bytesStored/);
  assert.match(sandboxRunner, /putFixedLengthStream\(bucket, output\.key, stream, file\.size/);
  assert.match(sandboxRunner, /let localRestoreTail = Promise\.resolve\(\)/);
  assert.match(
    sandboxRunner,
    /await restoreSnapshot\([\s\S]*?runnerSandbox,[\s\S]*?input\.restore,[\s\S]*?this\.env,[\s\S]*?input\.sourceSha/,
  );
  assert.match(sandboxRunner, /env\.ENVIRONMENT !== 'development'/);
  assert.match(sandboxRunner, /LOCAL_RESTORE_CHUNK_BYTES = 64 \* 1024 \* 1024/);
  assert.match(
    sandboxRunner,
    /\/api\/ci\/local-backups\/\$\{snapshot\.id\}\/data\.sqsh\?run=\$\{sourceSha\}/,
  );
  assert.match(sandboxRunner, /curl[\s\S]*?--speed-time 15[\s\S]*?--range "\$offset-\$end"/);
  assert.doesNotMatch(sandboxRunner, /sandbox\.writeFile\(partPath, chunk\.body\)/);
  assert.match(sandboxRunner, /transport: 'http'/);
  assert.match(
    sandboxRunner,
    /stat -c %s[\s\S]*?\$\{size\}[\s\S]*?\/usr\/bin\/unsquashfs/,
  );
  assert.match(sandboxRunner, /LARGE_LOCAL_RESTORE_BYTES = 64 \* 1024 \* 1024/);
  assert.match(
    sandboxRunner,
    /serializeLocalSandboxStart\([\s\S]*?runnerSandbox\.exec\('true'[\s\S]*?\);[\s\S]*?if \(input\.restore\)/,
  );
  assert.match(
    sandboxRunner,
    /if \(size <= LARGE_LOCAL_RESTORE_BYTES\)[\s\S]*?restoreLocalSnapshotInChunks[\s\S]*?return/,
  );
  assert.match(sandboxRunner, /await registerActiveSandbox\(/);
  assert.match(sandboxRunner, /await assertRunActive\(this\.env\.BACKUP_BUCKET, input\.sourceSha\)/);
  assert.match(sandboxRunner, /runs\/\$\{head\}\/sandboxes\/\$\{runnerId\}\.json/);
  assert.match(sandboxRunner, /runs\/\$\{head\}\/control\/terminated\.json/);
  assert.match(sandboxRunner, /runs\/\$\{head\}\/control\/failed\.json/);
  assert.ok(
    (sandboxRunner.match(/await assertRunActive\(/g) ?? []).length >= 4,
    "long Sandbox phases recheck the run tombstone before continuing",
  );
  assert.ok(
    sandboxRunner.indexOf("if (sandbox) await destroySandbox(sandbox)") <
      sandboxRunner.indexOf("await this.env.BACKUP_BUCKET.delete(registryKey)"),
    "a Sandbox marker is deleted only after teardown succeeds",
  );
  assert.match(cache, /localBucket: z\.boolean\(\)\.optional\(\)/);
  assert.match(cache, /localBucket: input\.snapshot\.localBucket/);
  assert.match(cache, /env\.ENVIRONMENT === 'development'/);
  assert.match(sandboxRunner, /let localSandboxStartTail = Promise\.resolve\(\)/);
  assert.match(sandboxRunner, /await serializeLocalSandboxStart\(this\.env/);
  assert.match(
    sandboxRunner,
    /if \(env\.ENVIRONMENT !== 'development'\) return start\(\)/,
  );
  assert.match(sandboxRunner, /PROCESS_POLL_INTERVAL_MS = 5_000/);
  assert.match(
    sandboxRunner,
    /waitForRunnerProcess\([\s\S]*?await sandbox\.getProcess\(proc\.id\)/,
  );
  assert.match(
    sandboxRunner,
    /finally \{\s+disposeRpcStub\(current\);\s+\}/,
    "every polled process capability is released",
  );
  assert.match(
    sandboxRunner,
    /finally \{[\s\S]*?disposeRpcStub\(proc\);[\s\S]*?destroySandbox\(sandbox\)/,
    "the original process capability is released before Sandbox teardown",
  );
  assert.match(
    sandboxRunner,
    /PROCESS_RUN_CHECK_INTERVAL_MS[\s\S]*?await assertActive\(\)/,
  );
  assert.doesNotMatch(
    sandboxRunner,
    /\.waitForExit\(/,
    "long CI commands must not depend on one Sandbox SSE connection",
  );
  assert.match(workflow, /CARGO_BUILD_JOBS: "4"/);
  assert.match(workflow, /CARGO_PROFILE_DEV_DEBUG: "0"/);
  assert.match(workflow, /CARGO_PROFILE_TEST_DEBUG: "0"/);
  assert.match(workflow, /RUST_TEST_THREADS: "4"/);
  assert.match(
    workflow,
    /name: "cargo dependencies"[\s\S]*?env: COMMON_ENV,[\s\S]*?name: "Rust build cache"[\s\S]*?env: RUST_TEST_ENV,/,
  );
  assert.match(
    workflow,
    /const MSRV_ENV = \{[\s\S]*?CARGO_TARGET_DIR: "\/workspace\/\.cargo-target-msrv",[\s\S]*?RUST_TEST_THREADS: "1"/,
  );
  assert.doesNotMatch(runnerGroup, /await Promise\.allSettled/);
  assert.match(workflow, /name: value\.runner\.name/);
  assert.match(
    workflow,
    /const failedGates = isCiRunnerFailure\(cause\)[\s\S]*?gate\.status = failed \? "failure" : "terminated"/,
  );
  const saturationBarrier = workflow.indexOf(
    "const saturationBarrier = Promise.all([",
  );
  const saturationAwait = workflow.indexOf(
    "const [, msrvBuildCache] = await Promise.all([",
  );
  const buildCacheReuse = workflow.indexOf(
    "const stableBuildCache = await buildCacheBranch;",
  );
  const stableExecution = workflow.indexOf(
    "command: rustResultCacheCommand(stableJob.command),",
  );
  const msrvExecution = workflow.indexOf(
    "command: rustResultCacheCommand(msrvJob.command),",
  );
  const webExecution = workflow.indexOf("runWebJob(),");
  const pythonBarrier = workflow.indexOf("await Promise.all(runPythonJobs());");
  assert.ok(
    saturationBarrier >= 0,
    "compile-heavy gates have an explicit phase barrier",
  );
  assert.match(
    workflow,
    /const qualityBranch = \(async \(\) => \{[\s\S]*?rustResultCacheCommand\(qualityJob\.command\)[\s\S]*?rustQualityCacheInputs\(\)[\s\S]*?const saturationBarrier = Promise\.all\(\[/,
  );
  assert.ok(
    saturationAwait > saturationBarrier &&
      buildCacheReuse > saturationAwait &&
      stableExecution > buildCacheReuse,
    "stable tests start only after the compile-heavy quality phase releases the host",
  );
  assert.ok(
    msrvExecution > stableExecution,
    "MSRV tests start only after the stable workspace suite releases the host",
  );
  assert.ok(
    webExecution > stableExecution,
    "deadline-sensitive JavaScript tests start only after the stable workspace suite releases the host",
  );
  assert.ok(
    pythonBarrier > msrvExecution,
    "wall-clock Python gates start only after compile-heavy gates release the host",
  );
  assert.match(
    workflow,
    /name: "MSRV build cache"[\s\S]*?cache: \{ inputs: msrvBuildCacheInputs\(\) \}/,
  );
  assert.match(
    workflow,
    /command: rustResultCacheCommand\(stableJob\.command\)[\s\S]*?rustResultCacheInputs\(\)/,
  );
  assert.match(
    workflow,
    /command: rustResultCacheCommand\(msrvJob\.command\)[\s\S]*?rustResultCacheInputs\(\)/,
  );
  assert.match(workflow, /\(\["3\.11", "3\.14"\] as const\)\.map/);
  assert.ok(
    workflow.indexOf('await step.do("persist CI success"') <
      workflow.indexOf("gatesCompleted = true"),
    "success is terminal only after its evidence has been persisted",
  );
});

test("the main pipeline requires authenticated macOS evidence", async () => {
  const workflow = await readFile(new URL("./ciWorkflow.ts", import.meta.url), "utf8");
  assert.match(workflow, /"macOS stable workspace tests and native CLI"/);
  assert.match(
    workflow,
    /queue macOS native build[\s\S]*?wait for macOS native build[\s\S]*?await Promise\.all\(\[macJob, runLinuxNative/,
  );
  assert.match(workflow, /CI_MAC_EVENT_TYPE/);
  assert.match(workflow, /task: "native-build"/);
  assert.match(workflow, /promoteMacNativeArtifact/);

});

test("PR npm previews cannot replace the normal release package", async () => {
  const workflow = await readFile(new URL("./ciWorkflow.ts", import.meta.url), "utf8");
  assert.match(
    workflow,
    /const npmPreview = source\.lane\.type === "pull_request"[\s\S]*?npmPreviewVersion\(head\)/,
  );
  assert.match(
    workflow,
    /const npmArtifactKey = `runs\/\$\{head\}\/artifacts\/npm-package\.tgz`/,
  );
  assert.match(
    workflow,
    /source\.lane\.type === "pull_request"[\s\S]*?key: `runs\/\$\{head\}\/artifacts\/npm-preview\.tgz`/,
  );
  const promotionStart = workflow.indexOf("async function promoteNpmReleaseArtifact(");
  const promotionEnd = workflow.indexOf("\nfunction retainedNpmReleaseAsset(", promotionStart);
  assert.ok(promotionStart >= 0 && promotionEnd > promotionStart);
  const promotion = workflow.slice(promotionStart, promotionEnd);
  assert.match(
    promotion,
    /const sourceKey = `runs\/\$\{head\}\/artifacts\/npm-package\.tgz`/,
  );
  assert.doesNotMatch(promotion, /npm-preview/);
});

test("distribution stages verified stable assets for trusted publication and auto-finalizes nightly", async () => {
  const workflow = await readFile(new URL("./ciWorkflow.ts", import.meta.url), "utf8");
  assert.match(workflow, /isNanocodexCiProviderData\(value, head\)/);
  assert.match(
    workflow,
    /event\.payload\.ref !== source\.lane\.ref[\s\S]*?event\.payload\.branch !== source\.lane\.branch/,
  );
  const branch = workflow.indexOf("if (source.distribution)");
  const normalState = workflow.indexOf("persist CI running state");
  assert.ok(branch >= 0 && branch < normalState);
  assert.match(
    workflow,
    /const \[linux, mac\] = await Promise\.all\(\[linuxPromise, macPromise\]\)/,
  );
  const draft = workflow.indexOf("Failed to stage release");
  const ready = workflow.indexOf("persist stable distribution ready state");
  const finalize = workflow.indexOf("Failed to finalize release");
  assert.ok(draft >= 0 && draft < ready && ready < finalize);
  assert.doesNotMatch(workflow, /CARGO_REGISTRY_TOKEN|NPM_TOKEN/);
});
