import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";

test("Wrangler owns the Workflow, source, backup, repository, and ten CI slots", async () => {
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
      max_instances: 10,
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
  assert.deepEqual(config.migrations.at(-1), {
    tag: "v7",
    new_sqlite_classes: ["CiRepository", "CiSandbox"],
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
    "CI_SOURCE_WRITE_TOKEN",
    "CI_CONTROL_TOKEN",
    "R2_ACCESS_KEY_ID",
    "R2_SECRET_ACCESS_KEY",
  ]);
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
});

test("terminal gates skip snapshots and publish the website artifact in-place", async () => {
  const [workflow, sandboxRunner, cache] = await Promise.all([
    readFile(new URL("./ciWorkflow.ts", import.meta.url), "utf8"),
    readFile(
      new URL("../node_modules/@cloudflare/ci/src/ci/runners/sandbox.ts", import.meta.url),
      "utf8",
    ),
    readFile(
      new URL("../node_modules/@cloudflare/ci/src/ci/cache.ts", import.meta.url),
      "utf8",
    ),
  ]);
  assert.match(workflow, /runnerConfig\(job\.timeoutMs, 24 \* 60 \* 60, 0, false\)/);
  assert.match(workflow, /runnerConfig\(40 \* 60 \* 1_000, 24 \* 60 \* 60, 0, false\)/);
  assert.match(workflow, /path: "\/workspace\/\.ci-output\/web-wasm\.tar"/);
  assert.match(workflow, /path: "\/workspace\/\.ci-output\/web-dist\.tar"/);
  assert.match(sandboxRunner, /localBucket: this\.env\.ENVIRONMENT === 'development'/);
  assert.match(
    sandboxRunner,
    /transport: this\.env\.ENVIRONMENT === 'development' \? 'rpc' : 'http'/,
  );
  assert.match(workflow, /name: "Node and browser bindings"[\s\S]*?runnerConfig\(60 \* 60 \* 1_000, 24 \* 60 \* 60, 0, false\)/);
  assert.match(workflow, /const website = await completeDependencies\.runner/);
  assert.doesNotMatch(workflow, /const website = await bindings\.runner/);
  assert.match(workflow, /outputs\?\.\[0\]/);
  assert.doesNotMatch(workflow, /getSandbox/);
  assert.match(sandboxRunner, /input\.createSnapshot/);
  assert.match(sandboxRunner, /persistOutputs/);
  assert.match(sandboxRunner, /checksum mismatch/);
  assert.match(cache, /localBucket: z\.boolean\(\)\.optional\(\)/);
  assert.match(cache, /localBucket: input\.snapshot\.localBucket/);
  assert.match(cache, /env\.ENVIRONMENT === 'development'/);
});
