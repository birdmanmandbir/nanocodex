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

test("deployment runs one full command before injected delivery checks", async () => {
  const workerVersionId = "12345678-1234-1234-1234-123456789abc";
  const origin = "https://nanocodex.example.test";
  const commands = [];
  const requests = [];
  let output = "";

  const health = await deployWorker({
    revision,
    origin,
    run: async (arguments_) => {
      commands.push(arguments_);
      return [
        "Uploaded nanocodex",
        "Deployed nanocodex triggers",
        "  workflow: nanocodex-ci-preview",
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
    origin,
    status: "ok",
    workerVersionId,
  });
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
