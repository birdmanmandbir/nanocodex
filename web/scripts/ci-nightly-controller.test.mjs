import assert from "node:assert/strict";
import { once } from "node:events";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import test from "node:test";

import {
  dispatchNightlyRelease,
  main,
  nightlyConfiguration,
  nightlyFailureRecord,
  parseCiPublicOrigin,
  parseNightlyArguments,
  parseNightlyResponse,
} from "./ci-nightly-controller.mjs";

const head = "a".repeat(40);
const otherHead = "b".repeat(40);
const requestId = "123e4567-e89b-42d3-a456-426614174000";
const restartedRequestId = "123e4567-e89b-42d3-b456-426614174001";
const token = "release-secret.with-punctuation";
const scriptPath = fileURLToPath(
  new URL("./ci-nightly-controller.mjs", import.meta.url),
);

test("arguments and environment accept only the narrow nightly contract", () => {
  assert.deepEqual(parseNightlyArguments([]), { head: null });
  assert.deepEqual(parseNightlyArguments(["--head", head]), { head });
  for (const args of [
    ["--head"],
    ["--head", head.toUpperCase()],
    ["--head", "a".repeat(39)],
    ["--head", head, "extra"],
    ["--help"],
    [head],
  ]) {
    assert.throws(() => parseNightlyArguments(args), /usage:/);
  }

  assert.equal(parseCiPublicOrigin("https://ci.example.test"), "https://ci.example.test");
  assert.equal(parseCiPublicOrigin("https://ci.example.test/"), "https://ci.example.test");
  assert.equal(parseCiPublicOrigin("http://127.0.0.1:8787"), "http://127.0.0.1:8787");
  assert.equal(parseCiPublicOrigin("http://worker.localhost:8787"), "http://worker.localhost:8787");
  for (const origin of [
    "http://ci.example.test",
    "https://user:password@ci.example.test",
    "https://ci.example.test/prefix",
    "https://ci.example.test/?query=1",
    "https://ci.example.test/#fragment",
    "https://ci.example.test?",
    "https://ci.example.test#",
    "https://@ci.example.test",
    "https://ci.example.test/path/..",
    "https://ci.example.test/%2e",
    "https://ci.example.test\\",
    "https://ci.example\n.test",
    "https://ci.example\t.test",
    "https://ci.example\r.test",
    " https://ci.example.test",
    "not a url",
  ]) {
    assert.throws(() => parseCiPublicOrigin(origin), /CI_PUBLIC_ORIGIN/);
  }

  const config = nightlyConfiguration({
    CI_PUBLIC_ORIGIN: "https://ci.example.test/",
    CI_RELEASE_TOKEN: token,
  });
  assert.deepEqual(config, {
    origin: "https://ci.example.test",
    endpoint: "https://ci.example.test/api/ci/releases/nightly",
    releaseToken: token,
  });
  assert.throws(
    () => nightlyConfiguration({ CI_PUBLIC_ORIGIN: "https://ci.example.test" }),
    /CI_RELEASE_TOKEN/,
  );
  assert.throws(
    () => nightlyConfiguration({
      CI_PUBLIC_ORIGIN: "https://ci.example.test",
      CI_RELEASE_TOKEN: "secret with spaces",
    }),
    /CI_RELEASE_TOKEN/,
  );
});

test("no arguments POST no body and normalize an authoritative current response", async () => {
  const calls = [];
  const result = await dispatchNightlyRelease(
    configuration(),
    parseNightlyArguments([]),
    stubRuntime(async (url, init) => {
      calls.push(requestSnapshot(url, init));
      return jsonResponse({ status: "current", head }, 200);
    }),
  );
  assert.deepEqual(result, { status: "current", head, workflowId: null });
  assert.deepEqual(calls, [{
    url: "https://ci.example.test/api/ci/releases/nightly",
    method: "POST",
    authorization: `Bearer ${token}`,
    accept: "application/json",
    contentType: null,
    bodyPresent: false,
    body: null,
    redirect: "error",
  }]);
});

test("an explicit head POSTs the exact minimal body and accepts replay states", async (t) => {
  for (const status of ["accepted", "restarted"]) {
    await t.test(status, async () => {
      const calls = [];
      const result = await dispatchNightlyRelease(
        configuration(),
        parseNightlyArguments(["--head", head]),
        stubRuntime(async (url, init) => {
          calls.push(requestSnapshot(url, init));
          return jsonResponse({
            status,
            head,
            workflowId: `nightly-${head}`,
            requestId: status === "restarted" ? restartedRequestId : requestId,
          }, 202);
        }),
      );
      assert.deepEqual(result, {
        status,
        head,
        workflowId: `nightly-${head}`,
        requestId: status === "restarted" ? restartedRequestId : requestId,
      });
      assert.deepEqual(calls, [{
        url: "https://ci.example.test/api/ci/releases/nightly",
        method: "POST",
        authorization: `Bearer ${token}`,
        accept: "application/json",
        contentType: "application/json",
        bodyPresent: true,
        body: JSON.stringify({ head }),
        redirect: "error",
      }]);
    });
  }

  assert.deepEqual(parseNightlyResponse({ status: "current", head }), {
    status: "current",
    head,
    workflowId: null,
  });
  for (const invalid of [
    { status: "accepted", head, workflowId: `nightly-${head}` },
    { status: "accepted", head, workflowId: `nightly-${otherHead}`, requestId },
    { status: "accepted", head: otherHead, workflowId: `nightly-${otherHead}`, requestId },
    { status: "accepted", head, workflowId: `nightly-${head}`, requestId: requestId.toUpperCase() },
    { status: "accepted", head, workflowId: `nightly-${head}`, requestId: requestId.replace("42d3", "12d3") },
    { status: "accepted", head, workflowId: `nightly-${head}`, requestId, extra: true },
    { status: "queued", head, workflowId: `nightly-${head}`, requestId },
    { status: "current", head, workflowId: null },
    { status: "current", head, requestId },
    { status: "current", head, extra: true },
  ]) {
    assert.throws(() => parseNightlyResponse(invalid, head), /invalid/);
  }
});

test("request timeout is bounded and retries the same operation", async () => {
  let attempts = 0;
  const snapshots = [];
  const result = await dispatchNightlyRelease(
    configuration(),
    { head },
    stubRuntime(async (url, init) => {
      attempts += 1;
      snapshots.push(requestSnapshot(url, init));
      if (attempts === 1) {
        return new Promise((resolve, reject) => {
          const abort = () => reject(init.signal.reason ?? new Error("aborted"));
          if (init.signal.aborted) abort();
          else init.signal.addEventListener("abort", abort, { once: true });
        });
      }
      return jsonResponse(
        { status: "accepted", head, workflowId: `nightly-${head}`, requestId },
        202,
      );
    }, { requestTimeoutMs: 10 }),
  );
  assert.equal(attempts, 2);
  assert.deepEqual(snapshots[0], snapshots[1]);
  assert.equal(result.status, "accepted");
});

test("the deadline wins even when fetch ignores its abort signal", async () => {
  let finishFetch;
  const ignoredFetch = new Promise((resolveFetch) => {
    finishFetch = resolveFetch;
  });
  const operation = dispatchNightlyRelease(
    configuration(),
    { head },
    stubRuntime(() => ignoredFetch, {
      requestTimeoutMs: 5,
      maximumAttempts: 1,
    }),
  ).then(
    () => ({ type: "resolved" }),
    (cause) => ({ type: "rejected", cause }),
  );
  const outcome = await Promise.race([
    operation,
    new Promise((resolveWait) => {
      setTimeout(
        () => resolveWait({ type: "test-timeout" }),
        100,
      ).unref();
    }),
  ]);
  assert.equal(outcome.type, "rejected");
  assert.match(outcome.cause.message, /timed out after 5ms/);
  finishFetch(jsonResponse({
    status: "accepted",
    head,
    workflowId: `nightly-${head}`,
    requestId,
  }, 202));
  await ignoredFetch;
});

test("an unpinned replay stays bodyless and adopts the server-selected head", async () => {
  const snapshots = [];
  let attempts = 0;
  const result = await dispatchNightlyRelease(
    configuration(),
    { head: null },
    stubRuntime(async (url, init) => {
      attempts += 1;
      snapshots.push(requestSnapshot(url, init));
      if (attempts === 1) throw new Error("connection closed after dispatch");
      return jsonResponse({
        status: "accepted",
        head: otherHead,
        workflowId: `nightly-${otherHead}`,
        requestId,
      }, 202);
    }),
  );
  assert.deepEqual(result, {
    status: "accepted",
    head: otherHead,
    workflowId: `nightly-${otherHead}`,
    requestId,
  });
  assert.equal(snapshots.length, 2);
  assert.deepEqual(snapshots[0], snapshots[1]);
  assert.equal(snapshots[0].bodyPresent, false);
});

test("429 and 5xx use bounded retry delays before an acknowledgement", async () => {
  const delays = [];
  let attempts = 0;
  const result = await dispatchNightlyRelease(
    configuration(),
    { head },
    {
      ...stubRuntime(async () => {
        attempts += 1;
        if (attempts === 1) {
          return new Response("busy", {
            status: 429,
            headers: { "retry-after": "100" },
          });
        }
        if (attempts === 2) return new Response("unavailable", { status: 503 });
        return jsonResponse(
          { status: "accepted", head, workflowId: `nightly-${head}`, requestId },
          202,
        );
      }, { retryBaseMs: 1, maximumRetryDelayMs: 5 }),
      sleep: async (milliseconds) => delays.push(milliseconds),
    },
  );
  assert.equal(result.status, "accepted");
  assert.equal(attempts, 3);
  assert.deepEqual(delays, [5, 2]);
});

test("a lost acknowledgement is reconciled by an identical replay", async () => {
  const requests = [];
  let attempts = 0;
  const server = await localServer(async (request, response) => {
    const body = await requestText(request);
    requests.push({
      method: request.method,
      authorization: request.headers.authorization,
      contentType: request.headers["content-type"],
      body,
    });
    attempts += 1;
    if (attempts === 1) {
      request.socket.destroy();
      return;
    }
    response.writeHead(202, { "content-type": "application/json" });
    response.end(JSON.stringify({
      status: "restarted",
      head,
      workflowId: `nightly-${head}`,
      requestId: restartedRequestId,
    }));
  });
  try {
    const result = await dispatchNightlyRelease(
      nightlyConfiguration({
        CI_PUBLIC_ORIGIN: server.origin,
        CI_RELEASE_TOKEN: token,
      }),
      { head },
      stubRuntime(globalThis.fetch, { requestTimeoutMs: 1_000 }),
    );
    assert.deepEqual(result, {
      status: "restarted",
      head,
      workflowId: `nightly-${head}`,
      requestId: restartedRequestId,
    });
    assert.equal(requests.length, 2);
    assert.deepEqual(requests[0], requests[1]);
    assert.deepEqual(requests[0], {
      method: "POST",
      authorization: `Bearer ${token}`,
      contentType: "application/json",
      body: JSON.stringify({ head }),
    });
  } finally {
    await server.close();
  }
});

test("authentication and conflict failures never retry", async (t) => {
  for (const status of [401, 409]) {
    await t.test(String(status), async () => {
      let attempts = 0;
      await assert.rejects(
        dispatchNightlyRelease(
          configuration(),
          { head },
          stubRuntime(async () => {
            attempts += 1;
            return jsonResponse({ error: status === 401 ? "unauthorized" : "conflict" }, status);
          }),
        ),
        new RegExp(`HTTP ${status}`),
      );
      assert.equal(attempts, 1);
    });
  }
});

test("malformed, mismatched, and oversized success responses fail without replay", async (t) => {
  const cases = [
    {
      name: "wrong content type",
      response: () => new Response("{}", { status: 200 }),
      pattern: /application\/json/,
    },
    {
      name: "invalid JSON",
      response: () => new Response("{", {
        status: 200,
        headers: { "content-type": "application/json" },
      }),
      pattern: /invalid JSON/,
    },
    {
      name: "wrong identity",
      response: () => jsonResponse({
        status: "accepted",
        head: otherHead,
        workflowId: `nightly-${otherHead}`,
        requestId,
      }, 202),
      pattern: /invalid response identity/,
    },
    {
      name: "wrong workflow",
      response: () => jsonResponse({
        status: "accepted",
        head,
        workflowId: "nightly-wrong",
        requestId,
      }, 202),
      pattern: /invalid attempt identity/,
    },
    {
      name: "oversized declared body",
      response: () => new Response("{}", {
        status: 200,
        headers: {
          "content-type": "application/json",
          "content-length": String(16 * 1024 + 1),
        },
      }),
      pattern: /exceeded 16384 bytes/,
    },
    {
      name: "oversized streamed body",
      response: () => new Response(
        JSON.stringify({ status: "current", head }) + " ".repeat(16 * 1024),
        { status: 200, headers: { "content-type": "application/json" } },
      ),
      pattern: /exceeded 16384 bytes/,
    },
  ];
  for (const candidate of cases) {
    await t.test(candidate.name, async () => {
      let attempts = 0;
      await assert.rejects(
        dispatchNightlyRelease(
          configuration(),
          { head },
          stubRuntime(async () => {
            attempts += 1;
            return candidate.response();
          }),
        ),
        candidate.pattern,
      );
      assert.equal(attempts, 1);
    });
  }
});

test("success and failure each emit one structured, secret-free terminal record", async () => {
  const output = [];
  const result = await main(
    ["--head", head],
    {
      CI_PUBLIC_ORIGIN: "https://ci.example.test",
      CI_RELEASE_TOKEN: token,
    },
    {
      ...stubRuntime(async () => jsonResponse({
        status: "accepted",
        head,
        workflowId: `nightly-${head}`,
        requestId,
      }, 202)),
      stdout: { write: (value) => output.push(value) },
    },
  );
  assert.deepEqual(result, {
    status: "accepted",
    head,
    workflowId: `nightly-${head}`,
    requestId,
  });
  assert.equal(output.length, 1);
  assert.deepEqual(JSON.parse(output[0]), result);

  const server = await localServer(async (_request, response) => {
    response.writeHead(401, { "content-type": "text/plain" });
    response.end(`Bearer ${token}; raw=${token}`);
  });
  try {
    const child = spawn(process.execPath, [scriptPath, "--head", head], {
      env: {
        CI_PUBLIC_ORIGIN: server.origin,
        CI_RELEASE_TOKEN: token,
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    const [stdout, stderr, [exitCode]] = await Promise.all([
      collect(child.stdout),
      collect(child.stderr),
      once(child, "close"),
    ]);
    assert.equal(exitCode, 1);
    assert.equal(stdout, "");
    assert.doesNotMatch(stderr, new RegExp(escapeRegExp(token)));
    const lines = stderr.trimEnd().split("\n");
    assert.equal(lines.length, 1);
    const failure = JSON.parse(lines[0]);
    assert.equal(failure.status, "error");
    assert.equal(failure.head, head);
    assert.equal(failure.workflowId, `nightly-${head}`);
    assert.match(failure.error, /HTTP 401/);

    const raw = nightlyFailureRecord(
      new Error(`request leaked Bearer ${token} and ${token}`),
      { CI_RELEASE_TOKEN: token },
      ["--head", head],
    );
    assert.doesNotMatch(JSON.stringify(raw), new RegExp(escapeRegExp(token)));
  } finally {
    await server.close();
  }
});

test("transport failures are sanitized before they escape the dispatcher", async () => {
  let attempts = 0;
  await assert.rejects(
    dispatchNightlyRelease(
      configuration(),
      { head },
      stubRuntime(async () => {
        attempts += 1;
        throw new Error(`socket rejected Authorization: Bearer ${token} (${token})`);
      }, { maximumAttempts: 2 }),
    ),
    (cause) => {
      assert.doesNotMatch(cause.message, new RegExp(escapeRegExp(token)));
      assert.match(cause.message, /\[redacted\]/);
      return true;
    },
  );
  assert.equal(attempts, 2);

  await assert.rejects(
    dispatchNightlyRelease(
      { ...configuration(), releaseToken: "x".repeat(8 * 1024 + 1) },
      { head },
      stubRuntime(async () => {
        throw new Error("must not fetch");
      }),
    ),
    /configuration is invalid/,
  );
});

function configuration() {
  return nightlyConfiguration({
    CI_PUBLIC_ORIGIN: "https://ci.example.test",
    CI_RELEASE_TOKEN: token,
  });
}

function stubRuntime(fetchImpl, overrides = {}) {
  return {
    fetchImpl,
    sleep: async () => undefined,
    requestTimeoutMs: 250,
    maximumAttempts: 4,
    retryBaseMs: 0,
    maximumRetryDelayMs: 0,
    ...overrides,
  };
}

function jsonResponse(value, status) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function requestSnapshot(url, init) {
  const headers = new Headers(init.headers);
  return {
    url: String(url),
    method: init.method,
    authorization: headers.get("authorization"),
    accept: headers.get("accept"),
    contentType: headers.get("content-type"),
    bodyPresent: Object.hasOwn(init, "body"),
    body: init.body ?? null,
    redirect: init.redirect,
  };
}

async function localServer(handler) {
  const server = createServer((request, response) => {
    Promise.resolve(handler(request, response)).catch((cause) => {
      response.destroy(cause);
    });
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  assert.ok(address && typeof address === "object");
  return {
    origin: `http://127.0.0.1:${address.port}`,
    close: async () => {
      server.closeAllConnections();
      await new Promise((resolveClose, reject) => {
        server.close((cause) => cause ? reject(cause) : resolveClose());
      });
    },
  };
}

async function requestText(request) {
  const chunks = [];
  for await (const chunk of request) chunks.push(Buffer.from(chunk));
  return Buffer.concat(chunks).toString("utf8");
}

async function collect(stream) {
  const chunks = [];
  let bytes = 0;
  for await (const chunk of stream) {
    bytes += chunk.byteLength;
    assert.ok(bytes <= 64 * 1024, "child output exceeded test bound");
    chunks.push(Buffer.from(chunk));
  }
  return Buffer.concat(chunks, bytes).toString("utf8");
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
