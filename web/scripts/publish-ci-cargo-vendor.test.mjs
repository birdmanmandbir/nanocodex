import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import {
  chmod,
  mkdtemp,
  open,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import test from "node:test";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";

import { encodeFrameHeader } from "./ci-pr-cargo-builder.mjs";
import {
  cargoVendorFrame,
  deterministicMultipartRequestId,
  main,
  readFramedArtifact,
} from "./publish-ci-cargo-vendor.mjs";

const execFileAsync = promisify(execFile);
const publisherPath = fileURLToPath(new URL("./publish-ci-cargo-vendor.mjs", import.meta.url));

const baseHead = "1".repeat(40);
const pullRequestHead = "2".repeat(40);
const mergeHead = "3".repeat(40);
const cargoLockBlob = "4".repeat(40);

test("fd3 frame parser enforces magic, canonical descriptor, hash, exact size, and EOF", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-uploader-frame-"));
  const payload = Buffer.from("deterministic vendor payload");
  const descriptor = prDescriptor(payload);
  try {
    const valid = await writeFrame(directory, "valid", descriptor, payload);
    const parsed = await readFramedArtifact(valid.handle.fd);
    assert.deepEqual(parsed.descriptor, descriptor);
    await assert.rejects(
      readFramedArtifact(valid.handle.fd, { expectedGid: process.getgid() + 1 }),
      /private controller-owned regular file/,
    );
    await valid.handle.close();

    const trailing = await writeFrame(
      directory,
      "trailing",
      descriptor,
      Buffer.concat([payload, Buffer.from("x")]),
    );
    await assert.rejects(readFramedArtifact(trailing.handle.fd), /trailing bytes/);
    await trailing.handle.close();

    const corrupt = await writeFrame(
      directory,
      "corrupt",
      descriptor,
      Buffer.from("deterministic vendor payloae"),
    );
    await assert.rejects(readFramedArtifact(corrupt.handle.fd), /does not match/);
    await corrupt.handle.close();

    const noncanonicalJson = Buffer.from(JSON.stringify({
      version: descriptor.version,
      number: descriptor.number,
      baseHead: descriptor.baseHead,
      pullRequestHead: descriptor.pullRequestHead,
      mergeHead: descriptor.mergeHead,
      cargoLockBlob: descriptor.cargoLockBlob,
      key: descriptor.key,
      size: descriptor.size,
      sha256: descriptor.sha256,
    }));
    const header = Buffer.alloc(cargoVendorFrame.magic.length + 8);
    cargoVendorFrame.magic.copy(header);
    header.writeUInt32BE(cargoVendorFrame.version, cargoVendorFrame.magic.length);
    header.writeUInt32BE(noncanonicalJson.length, cargoVendorFrame.magic.length + 4);
    const path = resolve(directory, "noncanonical.frame");
    await writeFile(path, Buffer.concat([header, noncanonicalJson, payload]), { mode: 0o600 });
    await chmod(path, 0o600);
    const noncanonical = await open(path, "r");
    await assert.rejects(readFramedArtifact(noncanonical.fd), /not canonical JSON/);
    await noncanonical.close();

    const oversized = Buffer.alloc(cargoVendorFrame.magic.length + 8);
    cargoVendorFrame.magic.copy(oversized);
    oversized.writeUInt32BE(cargoVendorFrame.version, cargoVendorFrame.magic.length);
    oversized.writeUInt32BE(
      cargoVendorFrame.maximumDescriptorBytes + 1,
      cargoVendorFrame.magic.length + 4,
    );
    const oversizedPath = resolve(directory, "oversized.frame");
    await writeFile(oversizedPath, Buffer.concat([oversized, Buffer.from("x")]), { mode: 0o600 });
    await chmod(oversizedPath, 0o600);
    const oversizedHandle = await open(oversizedPath, "r");
    await assert.rejects(readFramedArtifact(oversizedHandle.fd), /descriptor length/);
    await oversizedHandle.close();
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("upload-only publisher HEADs and uploads the exact framed hash without executing Cargo", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-uploader-cli-"));
  const payload = Buffer.from("one immutable Cargo vendor bundle");
  const descriptor = prDescriptor(payload);
  const artifact = await writeFrame(directory, "upload", descriptor, payload);
  const marker = resolve(directory, "cargo-ran");
  const fakeCargo = resolve(directory, "cargo");
  const requests = [];
  let uploadId;
  let requestId;
  let stagingId;
  const parts = [];
  const server = createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const body = Buffer.concat(chunks);
    requests.push({
      authorization: request.headers.authorization,
      method: request.method,
      url: request.url,
      body,
    });
    const exact = `/api/ci/cargo-vendor/${cargoLockBlob}/${descriptor.sha256}`;
    if (request.method === "HEAD" && request.url === `${exact}/bundle.tar.gz`) {
      response.writeHead(404).end();
      return;
    }
    if (request.method === "POST" && request.url === `${exact}/multipart`) {
      const value = JSON.parse(body);
      requestId = value.requestId;
      assert.match(requestId, /^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/);
      uploadId = "fixture-upload";
      stagingId = requestId;
      response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({
        key: descriptor.key,
        cargoLockBlob,
        size: descriptor.size,
        sha256: descriptor.sha256,
        requestId,
        uploadId,
        stagingId,
        partSize: value.partSize,
        partCount: value.partCount,
      }));
      return;
    }
    if (request.method === "PUT" && request.url === `${exact}/multipart/parts/1`) {
      assert.equal(request.headers["x-nanocodex-upload-id"], uploadId);
      assert.equal(request.headers["x-nanocodex-staging-id"], stagingId);
      assert.equal(request.headers["x-nanocodex-sha256"], sha256(body));
      parts.push(body);
      response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({
        partNumber: 1,
        etag: createHash("md5").update(body).digest("hex"),
        size: body.length,
        sha256: sha256(body),
      }));
      return;
    }
    if (request.method === "POST" && request.url === `${exact}/multipart/complete`) {
      assert.deepEqual(Buffer.concat(parts), payload);
      response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({
        key: descriptor.key,
        cargoLockBlob,
        size: descriptor.size,
        sha256: descriptor.sha256,
        uploaded: true,
      }));
      return;
    }
    response.writeHead(418).end();
  });
  try {
    await writeFile(fakeCargo, `#!/bin/sh\necho ran > ${JSON.stringify(marker)}\n`, { mode: 0o755 });
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const address = server.address();
    assert.ok(address && typeof address === "object");
    const logs = [];
    const result = await main({
      args: [],
      fd: artifact.handle.fd,
      env: {
        PATH: `${directory}:/usr/bin:/bin`,
        NANOCODEX_CI_ORIGIN: `http://127.0.0.1:${address.port}`,
        NANOCODEX_CI_TOKEN: "source-only-token",
      },
      log: (line) => logs.push(line),
    });
    assert.deepEqual(result, descriptor);
    assert.deepEqual(logs, [canonical(descriptor)]);
    assert.equal(await readFile(marker).catch(() => null), null);
    assert.ok(requests.every(({ authorization }) => authorization === "Bearer source-only-token"));
    assert.deepEqual(requests.map(({ method, url }) => ({ method, url })), [
      { method: "HEAD", url: `/api/ci/cargo-vendor/${cargoLockBlob}/${descriptor.sha256}/bundle.tar.gz` },
      { method: "POST", url: `/api/ci/cargo-vendor/${cargoLockBlob}/${descriptor.sha256}/multipart` },
      { method: "PUT", url: `/api/ci/cargo-vendor/${cargoLockBlob}/${descriptor.sha256}/multipart/parts/1` },
      { method: "POST", url: `/api/ci/cargo-vendor/${cargoLockBlob}/${descriptor.sha256}/multipart/complete` },
    ]);
    const source = await readFile(
      new URL("./publish-ci-cargo-vendor.mjs", import.meta.url),
      "utf8",
    );
    assert.doesNotMatch(source, /node:child_process|\bexecFile\b|\bspawn\s*\(/);
    assert.doesNotMatch(source, /cargo\s+(?:fetch|vendor)|NANOCODEX_REPO\s*\?\?/);
  } finally {
    await artifact.handle.close();
    if (server.listening) await new Promise((done) => server.close(done));
    await rm(directory, { recursive: true, force: true });
  }
});

test("create retries parse each response, cap Retry-After, and survive process restart", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-uploader-create-"));
  const payload = Buffer.from("deterministic multipart creation identity");
  const descriptor = prDescriptor(payload);
  const artifact = await writeFrame(directory, "create", descriptor, payload);
  const requestBodies = [];
  let createAttempts = 0;
  const server = createServer(async (request, response) => {
    const body = await requestBody(request);
    const exact = `/api/ci/cargo-vendor/${cargoLockBlob}/${descriptor.sha256}`;
    if (request.method === "HEAD" && request.url === `${exact}/bundle.tar.gz`) {
      response.writeHead(404).end();
      return;
    }
    if (request.method === "POST" && request.url === `${exact}/multipart`) {
      requestBodies.push(body);
      createAttempts += 1;
      if (createAttempts === 1) {
        response.writeHead(200, { "content-type": "application/json" }).end("{");
        return;
      }
      if (createAttempts === 2) {
        response.writeHead(503, {
          "content-type": "text/plain",
          "retry-after": "86400",
        }).end("retry without leaking source-only-token");
        return;
      }
      const value = JSON.parse(body);
      response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({
        key: descriptor.key,
        cargoLockBlob,
        size: descriptor.size,
        sha256: descriptor.sha256,
        requestId: value.requestId,
        uploadId: "recovered-create-upload",
        stagingId: value.requestId,
        partSize: value.partSize,
        partCount: value.partCount,
      }));
      return;
    }
    if (request.method === "PUT" && request.url === `${exact}/multipart/parts/1`) {
      response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({
        partNumber: 1,
        etag: createHash("md5").update(body).digest("hex"),
        size: body.length,
        sha256: sha256(body),
      }));
      return;
    }
    if (request.method === "POST" && request.url === `${exact}/multipart/complete`) {
      response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({
        key: descriptor.key,
        cargoLockBlob,
        size: descriptor.size,
        sha256: descriptor.sha256,
        uploaded: true,
      }));
      return;
    }
    response.writeHead(418).end();
  });
  try {
    const expected = deterministicMultipartRequestId(descriptor);
    assert.match(expected, /^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/);
    const program = `import {deterministicMultipartRequestId as id} from ${JSON.stringify(
      new URL("./publish-ci-cargo-vendor.mjs", import.meta.url).href,
    )};process.stdout.write(id(JSON.parse(process.argv[1])))`;
    const restarted = await Promise.all([1, 2].map(() => execFileAsync(process.execPath, [
      "--input-type=module",
      "--eval",
      program,
      canonical(descriptor),
    ], { encoding: "utf8", env: { PATH: "/usr/bin:/bin" } })));
    assert.deepEqual(restarted.map(({ stdout }) => stdout), [expected, expected]);

    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const address = server.address();
    assert.ok(address && typeof address === "object");
    const started = Date.now();
    assert.deepEqual(await main({
      args: [],
      fd: artifact.handle.fd,
      env: {
        NANOCODEX_CI_ORIGIN: `http://127.0.0.1:${address.port}`,
        NANOCODEX_CI_TOKEN: "source-only-token",
      },
      retryDelayCapMs: 20,
      log() {},
    }), descriptor);
    assert.ok(Date.now() - started < 1_000, "huge Retry-After must be capped");
    assert.equal(createAttempts, 3);
    assert.ok(requestBodies.every((body) => body.equals(requestBodies[0])));
    assert.equal(JSON.parse(requestBodies[0]).requestId, expected);
  } finally {
    await artifact.handle.close();
    if (server.listening) await new Promise((done) => server.close(done));
    await rm(directory, { recursive: true, force: true });
  }
});

test("multipart create and completion recover lost acknowledgements with one request identity", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-uploader-recovery-"));
  const payload = Buffer.from("acknowledgement-loss-safe vendor payload");
  const descriptor = prDescriptor(payload);
  const artifact = await writeFrame(directory, "recovery", descriptor, payload);
  let committed = false;
  let createAttempts = 0;
  let stableCreateBody;
  const requests = [];
  const server = createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const body = Buffer.concat(chunks);
    requests.push({ method: request.method, url: request.url, body });
    const exact = `/api/ci/cargo-vendor/${cargoLockBlob}/${descriptor.sha256}`;
    if (request.method === "HEAD" && request.url === `${exact}/bundle.tar.gz`) {
      if (!committed) {
        response.writeHead(404).end();
        return;
      }
      response.writeHead(200, {
        "content-length": String(descriptor.size),
        "x-nanocodex-cargo-lock": cargoLockBlob,
        "x-nanocodex-key": descriptor.key,
        "x-nanocodex-sha256": descriptor.sha256,
      }).end();
      return;
    }
    if (request.method === "POST" && request.url === `${exact}/multipart`) {
      createAttempts += 1;
      stableCreateBody ??= body;
      assert.deepEqual(body, stableCreateBody);
      if (createAttempts === 1) {
        request.socket.destroy();
        return;
      }
      const value = JSON.parse(body);
      response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({
        key: descriptor.key,
        cargoLockBlob,
        size: descriptor.size,
        sha256: descriptor.sha256,
        requestId: value.requestId,
        uploadId: "recovered-upload",
        stagingId: value.requestId,
        partSize: value.partSize,
        partCount: value.partCount,
      }));
      return;
    }
    if (request.method === "PUT" && request.url === `${exact}/multipart/parts/1`) {
      response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({
        partNumber: 1,
        etag: createHash("md5").update(body).digest("hex"),
        size: body.length,
        sha256: sha256(body),
      }));
      return;
    }
    if (request.method === "POST" && request.url === `${exact}/multipart/complete`) {
      committed = true;
      request.socket.destroy();
      return;
    }
    response.writeHead(418).end();
  });
  try {
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const address = server.address();
    assert.ok(address && typeof address === "object");
    const result = await main({
      args: [],
      fd: artifact.handle.fd,
      env: {
        NANOCODEX_CI_ORIGIN: `http://127.0.0.1:${address.port}`,
        NANOCODEX_CI_TOKEN: "source-only-token",
      },
      log() {},
    });
    assert.deepEqual(result, descriptor);
    assert.equal(createAttempts, 2);
    assert.equal(committed, true);
    assert.equal(requests.filter(({ method }) => method === "DELETE").length, 0);
  } finally {
    await artifact.handle.close();
    if (server.listening) await new Promise((done) => server.close(done));
    await rm(directory, { recursive: true, force: true });
  }
});

test("completion exact-HEAD recovers every uncertain response without deleting canonical data", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-uploader-completion-"));
  const modes = ["non-2xx", "truncated-2xx", "invalid-2xx", "mismatched-2xx"];
  const fixtures = new Map();
  for (const mode of modes) {
    const payload = Buffer.from(`completion recovery ${mode}`);
    const descriptor = prDescriptor(payload);
    fixtures.set(descriptor.sha256, {
      mode,
      payload,
      descriptor,
      committed: false,
      headCount: 0,
      deletes: 0,
    });
  }
  const server = createServer(async (request, response) => {
    const body = await requestBody(request);
    const match = request.url?.match(new RegExp(
      `^/api/ci/cargo-vendor/${cargoLockBlob}/([a-f0-9]{64})(?:/(.*))?$`,
    ));
    const fixture = match && fixtures.get(match[1]);
    if (!fixture) {
      response.writeHead(418).end();
      return;
    }
    const suffix = match[2];
    if (request.method === "HEAD" && suffix === "bundle.tar.gz") {
      fixture.headCount += 1;
      if (!fixture.committed) {
        response.writeHead(404).end();
        return;
      }
      response.writeHead(200, {
        "content-length": String(fixture.descriptor.size),
        "x-nanocodex-cargo-lock": cargoLockBlob,
        "x-nanocodex-key": fixture.descriptor.key,
        "x-nanocodex-sha256": fixture.descriptor.sha256,
      }).end();
      return;
    }
    if (request.method === "POST" && suffix === "multipart") {
      const value = JSON.parse(body);
      response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({
        key: fixture.descriptor.key,
        cargoLockBlob,
        size: fixture.descriptor.size,
        sha256: fixture.descriptor.sha256,
        requestId: value.requestId,
        uploadId: `upload-${fixture.mode}`,
        stagingId: value.requestId,
        partSize: value.partSize,
        partCount: value.partCount,
      }));
      return;
    }
    if (request.method === "PUT" && suffix === "multipart/parts/1") {
      response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({
        partNumber: 1,
        etag: createHash("md5").update(body).digest("hex"),
        size: body.length,
        sha256: sha256(body),
      }));
      return;
    }
    if (request.method === "POST" && suffix === "multipart/complete") {
      fixture.committed = true;
      if (fixture.mode === "non-2xx") {
        response.writeHead(503, { "content-type": "text/plain" }).end("uncertain completion");
      } else if (fixture.mode === "truncated-2xx") {
        response.writeHead(200, { "content-type": "application/json" }).end('{"key":');
      } else if (fixture.mode === "invalid-2xx") {
        response.writeHead(200, { "content-type": "application/json" }).end("{}");
      } else {
        response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({
          key: fixture.descriptor.key,
          cargoLockBlob,
          size: fixture.descriptor.size,
          sha256: "f".repeat(64),
          uploaded: true,
        }));
      }
      return;
    }
    if (request.method === "DELETE" && suffix === "multipart") {
      fixture.deletes += 1;
      response.writeHead(204).end();
      return;
    }
    response.writeHead(418).end();
  });
  const artifacts = [];
  try {
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const address = server.address();
    assert.ok(address && typeof address === "object");
    for (const fixture of fixtures.values()) {
      const artifact = await writeFrame(directory, fixture.mode, fixture.descriptor, fixture.payload);
      artifacts.push(artifact);
      assert.deepEqual(await main({
        args: [],
        fd: artifact.handle.fd,
        env: {
          NANOCODEX_CI_ORIGIN: `http://127.0.0.1:${address.port}`,
          NANOCODEX_CI_TOKEN: "source-only-token",
        },
        log() {},
      }), fixture.descriptor);
      assert.equal(fixture.headCount, 2, `${fixture.mode} must use one exact recovery HEAD`);
      assert.equal(fixture.deletes, 0, `${fixture.mode} must not delete after exact HEAD proof`);
    }
  } finally {
    await Promise.all(artifacts.map(({ handle }) => handle.close().catch(() => undefined)));
    if (server.listening) await new Promise((done) => server.close(done));
    await rm(directory, { recursive: true, force: true });
  }
});

test("every request is deadline-bounded and response diagnostics redact the bearer token", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-uploader-timeout-"));
  const timeoutPayload = Buffer.from("request timeout payload");
  const timeoutDescriptor = prDescriptor(timeoutPayload);
  const timeoutArtifact = await writeFrame(
    directory,
    "timeout",
    timeoutDescriptor,
    timeoutPayload,
  );
  const redactionPayload = Buffer.from("credential redaction payload");
  const redactionDescriptor = prDescriptor(redactionPayload);
  const redactionArtifact = await writeFrame(
    directory,
    "redaction",
    redactionDescriptor,
    redactionPayload,
  );
  const token = "source-only-token-never-print";
  const sockets = new Set();
  const server = createServer(async (request, response) => {
    await requestBody(request);
    if (request.url?.includes(timeoutDescriptor.sha256)) return;
    if (
      request.method === "HEAD" && request.url?.includes(redactionDescriptor.sha256)
    ) {
      response.writeHead(404).end();
      return;
    }
    if (
      request.method === "POST" &&
      request.url?.endsWith(`/${redactionDescriptor.sha256}/multipart`)
    ) {
      response.writeHead(503, {
        "content-type": "text/plain",
        "retry-after": "999999999",
      }).end(`Bearer ${token}; exact=${token}`);
      return;
    }
    response.writeHead(418).end();
  });
  server.on("connection", (socket) => {
    sockets.add(socket);
    socket.once("close", () => sockets.delete(socket));
  });
  try {
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const address = server.address();
    assert.ok(address && typeof address === "object");
    const origin = `http://127.0.0.1:${address.port}`;
    const started = Date.now();
    await assert.rejects(
      main({
        args: [],
        fd: timeoutArtifact.handle.fd,
        env: { NANOCODEX_CI_ORIGIN: origin, NANOCODEX_CI_TOKEN: token },
        requestTimeoutMs: 40,
        log() {},
      }),
      /(?:timed out|Timeout|aborted)/i,
    );
    assert.ok(Date.now() - started < 1_000, "request timeout must be a short hard bound");

    let failure;
    try {
      await main({
        args: [],
        fd: redactionArtifact.handle.fd,
        env: { NANOCODEX_CI_ORIGIN: origin, NANOCODEX_CI_TOKEN: token },
        retryDelayCapMs: 0,
        log() {},
      });
    } catch (cause) {
      failure = cause;
    }
    assert.ok(failure instanceof Error);
    assert.doesNotMatch(failure.message, new RegExp(token));
    assert.match(failure.message, /\[redacted\]/);
  } finally {
    await timeoutArtifact.handle.close();
    await redactionArtifact.handle.close();
    for (const socket of sockets) socket.destroy();
    if (server.listening) await new Promise((done) => server.close(done));
    await rm(directory, { recursive: true, force: true });
  }
});

test("SIGTERM aborts active upload and spends its cleanup budget on multipart DELETE", async () => {
  const directory = await mkdtemp(resolve(tmpdir(), "nanocodex-uploader-sigterm-"));
  const payload = Buffer.from("SIGTERM cleanup payload");
  const descriptor = prDescriptor(payload);
  const artifact = await writeFrame(directory, "sigterm", descriptor, payload);
  const token = "sigterm-source-token-never-print";
  let deleteCount = 0;
  let partStartedResolve;
  const partStarted = new Promise((resolvePromise) => {
    partStartedResolve = resolvePromise;
  });
  const server = createServer(async (request, response) => {
    const body = await requestBody(request);
    const exact = `/api/ci/cargo-vendor/${cargoLockBlob}/${descriptor.sha256}`;
    if (request.method === "HEAD" && request.url === `${exact}/bundle.tar.gz`) {
      response.writeHead(404).end();
      return;
    }
    if (request.method === "POST" && request.url === `${exact}/multipart`) {
      const value = JSON.parse(body);
      response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({
        key: descriptor.key,
        cargoLockBlob,
        size: descriptor.size,
        sha256: descriptor.sha256,
        requestId: value.requestId,
        uploadId: "active-sigterm-upload",
        stagingId: value.requestId,
        partSize: value.partSize,
        partCount: value.partCount,
      }));
      return;
    }
    if (request.method === "PUT" && request.url === `${exact}/multipart/parts/1`) {
      partStartedResolve();
      return;
    }
    if (request.method === "DELETE" && request.url === `${exact}/multipart`) {
      deleteCount += 1;
      response.writeHead(204).end();
      return;
    }
    response.writeHead(418).end();
  });
  let child;
  try {
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const address = server.address();
    assert.ok(address && typeof address === "object");
    child = spawn(process.execPath, [publisherPath], {
      env: {
        PATH: "/usr/bin:/bin",
        NANOCODEX_CI_ORIGIN: `http://127.0.0.1:${address.port}`,
        NANOCODEX_CI_TOKEN: token,
      },
      stdio: ["ignore", "pipe", "pipe", artifact.handle.fd],
    });
    const stdout = [];
    const stderr = [];
    child.stdout.on("data", (chunk) => stdout.push(chunk));
    child.stderr.on("data", (chunk) => stderr.push(chunk));
    await withTimeout(partStarted, 2_000, "uploader did not begin its part request");
    const stoppedAt = Date.now();
    child.kill("SIGTERM");
    const [code] = await withTimeout(once(child, "close"), 2_000, "uploader ignored SIGTERM");
    assert.equal(code, 1);
    assert.ok(Date.now() - stoppedAt < 1_250, "cleanup must fit the controller TERM grace");
    assert.equal(deleteCount, 1);
    assert.equal(Buffer.concat(stdout).length, 0);
    assert.doesNotMatch(Buffer.concat(stderr).toString("utf8"), new RegExp(token));
  } finally {
    if (child && child.exitCode == null) child.kill("SIGKILL");
    await artifact.handle.close();
    if (server.listening) await new Promise((done) => server.close(done));
    await rm(directory, { recursive: true, force: true });
  }
});

test("uploader rejects repository paths and ambient authorities before network access", async () => {
  await assert.rejects(
    main({
      args: [],
      fd: -1,
      env: {
        NANOCODEX_CI_ORIGIN: "https://ci.example.test",
        NANOCODEX_CI_TOKEN: "source-token",
        NANOCODEX_REPO: "/untrusted/checkout",
      },
      log() {},
    }),
    /rejects NANOCODEX_REPO/,
  );
  await assert.rejects(
    main({
      args: [],
      fd: -1,
      env: {
        NANOCODEX_CI_ORIGIN: "https://ci.example.test",
        NANOCODEX_CI_TOKEN: "source-token",
        GITHUB_TOKEN: "ambient-github",
      },
      log() {},
    }),
    /ambient authorities: GITHUB_TOKEN/,
  );
});

function prDescriptor(payload) {
  const hash = sha256(payload);
  return {
    version: 1,
    number: 17,
    baseHead,
    pullRequestHead,
    mergeHead,
    cargoLockBlob,
    key: `cargo-vendor/${cargoLockBlob}/${hash}/bundle.tar.gz`,
    size: payload.length,
    sha256: hash,
  };
}

async function writeFrame(directory, name, descriptor, payload) {
  const path = resolve(directory, `${name}.frame`);
  await writeFile(path, Buffer.concat([encodeFrameHeader(descriptor), payload]), { mode: 0o600 });
  await chmod(path, 0o600);
  return { path, handle: await open(path, "r") };
}

async function requestBody(request) {
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  return Buffer.concat(chunks);
}

async function withTimeout(promise, milliseconds, message) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, rejectPromise) => {
        timer = setTimeout(() => rejectPromise(new Error(message)), milliseconds);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

function canonical(value) {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value && typeof value === "object") {
    return `{${Object.keys(value).sort().map((key) =>
      `${JSON.stringify(key)}:${canonical(value[key])}`
    ).join(",")}}`;
  }
  return JSON.stringify(value);
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}
