import assert from "node:assert/strict";
import test from "node:test";

import {
  assertDeploymentDocument,
  assertDeploymentEntry,
  assertDeploymentHealth,
  deployArguments,
  deployWorker,
  deploymentRevision,
  parseWorkerVersionId,
  wranglerEnvironment,
} from "./deploy-worker.mjs";

const revision = "a".repeat(40);
const deploymentTargets = [
  {
    accountId: "7fb82fc3b80331b2cd45f097acbd9ffc",
    environment: "production",
    name: "nanocodex",
    origin: "https://nanocodex.me-7fb.workers.dev",
    workflow: "nanocodex-ci",
  },
  {
    accountId: "16ce0442a940f01beefdb15a196a43ea",
    environment: "preview",
    name: "nanocodex-preview",
    origin: "https://nanocodex-preview.gakonst.workers.dev",
    workflow: "nanocodex-ci-preview",
  },
];

function deploymentConfig(target, overrides = {}) {
  return JSON.stringify({
    name: target.name,
    vars: {
      ENVIRONMENT: target.environment,
      CI_PUBLIC_ORIGIN: target.origin,
      CLOUDFLARE_ACCOUNT_ID: target.accountId,
    },
    ...overrides,
  });
}

test("deployment arguments bind the exact tagged commit to Worker health", () => {
  const arguments_ = deployArguments(revision);

  assert.deepEqual(arguments_, [
    "deploy",
    "--config",
    "dist/nanocodex/wrangler.json",
    "--strict",
    "--containers-rollout",
    "none",
    "--tag",
    revision,
    "--message",
    `gakonst/nanocodex@${revision}`,
    "--var",
    `DEPLOYMENT_SHA:${revision}`,
  ]);
  assert.equal(arguments_.includes("versions"), false);
  assert.equal(arguments_.includes("--env"), false);
});

test("CI deployment accepts only an explicit immutable source revision", () => {
  assert.equal(deploymentRevision(revision), revision);
  for (const invalid of ["", "abc", "A".repeat(40), `${revision}\n`]) {
    if (invalid === "") continue;
    assert.throws(() => deploymentRevision(invalid), /full commit SHA/);
  }
});

test("deployment reads the current Worker version from full deploy output", () => {
  const workerVersionId = "12345678-1234-1234-1234-123456789abc";
  assert.equal(
    parseWorkerVersionId(`Uploaded\n\u001b[32mCurrent Version ID:\u001b[0m ${workerVersionId}\n`),
    workerVersionId,
  );
  assert.throws(() => parseWorkerVersionId("Uploaded without an ID"));
  assert.throws(() => parseWorkerVersionId([
    `Current Version ID: ${workerVersionId}`,
    "Current Version ID: abcdefab-abcd-abcd-abcd-abcdefabcdef",
  ].join("\n")));
});

test("deployment ignores ambient Wrangler environment selection", () => {
  const environment = Object.freeze({
    CLOUDFLARE_API_TOKEN: "deployment-token",
    CLOUDFLARE_ENV: "development",
    PATH: "/usr/bin:/bin",
  });

  assert.deepEqual(wranglerEnvironment(environment), {
    CLOUDFLARE_API_TOKEN: "deployment-token",
    PATH: "/usr/bin:/bin",
  });
  assert.equal(environment.CLOUDFLARE_ENV, "development");
});

test("deployment accepts flattened production and preview targets", async () => {
  const workerVersionId = "12345678-1234-1234-1234-123456789abc";
  for (const target of deploymentTargets) {
    const commands = [];
    const requests = [];
    let output = "";

    const health = await deployWorker({
      revision,
      origin: target.origin,
      accountId: target.accountId,
      readConfig: async () => deploymentConfig(target),
      run: async (arguments_) => {
        commands.push(arguments_);
        return [
          `Uploaded ${target.name}`,
          `Deployed ${target.name} triggers`,
          `  workflow: ${target.workflow}`,
          `Current Version ID: ${workerVersionId}`,
        ].join("\n");
      },
      fetchImpl: async (input, init) => {
        const url = new URL(input);
        requests.push({ init, url });
        if (url.pathname === "/api/health") {
          return Response.json({ deployment_sha: revision, status: "ok" });
        }
        if (url.pathname === "/") {
          return new Response([
            "<!doctype html>",
            '<script type="module" src="/assets/index-Ab_9.js"></script>',
            '<div id="root"></div>',
          ].join("\n"), {
            headers: { "content-type": "text/html; charset=utf-8" },
          });
        }
        if (url.pathname === "/assets/index-Ab_9.js" && init.method === "HEAD") {
          return new Response(null, {
            headers: {
              "cache-control": "public, max-age=31536000, immutable",
              "content-type": "text/javascript",
            },
          });
        }
        throw new Error(`unexpected deployment request: ${url}`);
      },
      write: (chunk) => {
        output += chunk;
      },
    });

    assert.deepEqual(commands, [deployArguments(revision)]);
    assert.deepEqual(requests.map(({ url }) => url.pathname), [
      "/api/health",
      "/",
      "/assets/index-Ab_9.js",
    ]);
    assert.equal(requests[0].url.searchParams.get("revision"), revision);
    assert.deepEqual(health, { deployment_sha: revision, status: "ok" });
    assert.deepEqual(JSON.parse(output), {
      deploymentSha: revision,
      origin: target.origin,
      status: "ok",
      workerVersionId,
    });
  }
});

test("deployment rejects malformed, unflattened, and cross-target config before Wrangler", async () => {
  const [production, preview] = deploymentTargets;
  const previewConfig = JSON.parse(deploymentConfig(preview));
  const cases = [
    {
      name: "malformed JSON",
      serializedConfig: "{",
      error: /valid JSON/,
    },
    {
      name: "non-object JSON",
      serializedConfig: "[]",
      error: /must be an object/,
    },
    {
      name: "unflattened config",
      serializedConfig: JSON.stringify({ ...previewConfig, env: {} }),
      error: /flattened and omit env/,
    },
    {
      name: "production config for preview",
      serializedConfig: deploymentConfig(production),
      error: /Worker name must match the preview target/,
    },
    {
      name: "wrong environment",
      serializedConfig: deploymentConfig(preview, {
        vars: { ...previewConfig.vars, ENVIRONMENT: "production" },
      }),
      error: /ENVIRONMENT must match/,
    },
    {
      name: "wrong public origin",
      serializedConfig: deploymentConfig(preview, {
        vars: { ...previewConfig.vars, CI_PUBLIC_ORIGIN: production.origin },
      }),
      error: /CI_PUBLIC_ORIGIN must match/,
    },
    {
      name: "wrong config account",
      serializedConfig: deploymentConfig(preview, {
        vars: { ...previewConfig.vars, CLOUDFLARE_ACCOUNT_ID: production.accountId },
      }),
      error: /config CLOUDFLARE_ACCOUNT_ID must match/,
    },
  ];

  for (const testCase of cases) {
    let wranglerCalls = 0;
    await assert.rejects(deployWorker({
      revision,
      origin: preview.origin,
      accountId: preview.accountId,
      readConfig: async () => testCase.serializedConfig,
      run: async () => {
        wranglerCalls += 1;
        throw new Error("Wrangler must not run");
      },
    }), testCase.error, testCase.name);
    assert.equal(wranglerCalls, 0, testCase.name);
  }

  let wranglerCalls = 0;
  await assert.rejects(deployWorker({
    revision,
    origin: preview.origin,
    accountId: production.accountId,
    readConfig: async () => deploymentConfig(preview),
    run: async () => {
      wranglerCalls += 1;
      throw new Error("Wrangler must not run");
    },
  }), /CLOUDFLARE_ACCOUNT_ID must match the preview deployment target/);
  assert.equal(wranglerCalls, 0);
});

test("deployment health accepts only the exact revision", () => {
  assert.doesNotThrow(() => assertDeploymentHealth({
    deployment_sha: revision,
    status: "ok",
  }, revision));
  assert.throws(() => assertDeploymentHealth({
    deployment_sha: "b".repeat(40),
    status: "ok",
  }, revision));
  assert.throws(() => assertDeploymentHealth({
    deployment_sha: revision,
    status: "error",
  }, revision));
});

test("deployment delivery requires a complete homepage and immutable entry module", () => {
  const document = [
    "<!doctype html>",
    '<script type="module" crossorigin src="/assets/index-Ab_9.js"></script>',
    '<div id="root"></div>',
  ].join("\n");
  const response = new Response(document, {
    headers: { "content-type": "text/html; charset=utf-8" },
  });
  assert.equal(assertDeploymentDocument(response, document), "/assets/index-Ab_9.js");
  assert.throws(() => assertDeploymentDocument(new Response("not found", {
    status: 404,
    headers: { "content-type": "text/plain" },
  }), "not found"));

  assert.doesNotThrow(() => assertDeploymentEntry(new Response("export {};", {
    headers: {
      "cache-control": "public, max-age=31536000, immutable",
      "content-type": "text/javascript",
    },
  })));
  assert.throws(() => assertDeploymentEntry(new Response("export {};", {
    headers: {
      "cache-control": "no-store",
      "content-type": "text/javascript",
    },
  })));
});
