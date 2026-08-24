import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { test } from "node:test";

import { encodePacketLine, parsePacketLines } from "./threadProtocol.ts";
import { ThreadGitRepository, type ThreadRepository } from "./threadRepository.ts";
import {
  handleAppGitRequest,
  handleThreadGitRequest,
  readGitProtocolRequest,
} from "./threadRoutes.ts";

const encoder = new TextEncoder();
const decoder = new TextDecoder();
const repositoryName = "thread-12345678-1234-4123-8123-123456789abc";
const appRepository = "app-12345678-1234-4123-8123-123456789abc";
const appRepositoryV7 = "app-018f1f4e-7b2c-7abc-8def-0123456789ab";
const remote = `https://repository.test/git/${repositoryName}`;
const ref = "refs/heads/nanocodex";
const zero = "0".repeat(40);
const headA = "a".repeat(40);
const headB = "b".repeat(40);
const headC = "c".repeat(40);

test("Git request bodies are rejected while reading once they exceed the limit", async () => {
  const result = await readGitProtocolRequest(new Request("https://repository.test", {
    method: "POST",
    body: new Uint8Array([1, 2, 3, 4, 5]),
  }), 4);
  assert.ok(result instanceof Response);
  assert.equal(result.status, 413);
});

test("app Git lane requires exact props, repository names, URL locators, and methods", async () => {
  const repositories = repositoryNamespace();
  const bucket = new MemoryBucket();
  const env = {
    GIT_OBJECTS: bucket as unknown as R2Bucket,
    THREAD_GIT_REPOSITORY: repositories.namespace,
  };
  const validUrl = `https://repository.test/git/${appRepository}/info/refs?service=git-receive-pack`;

  const wrongProps = await handleAppGitRequest(
    appRepository,
    new Request(validUrl),
    env,
    { clientId: "browser" },
  );
  assert.equal(wrongProps.status, 403);

  for (const invalid of [
    "thread-12345678-1234-4123-8123-123456789abc",
    "app-12345678-1234-5123-8123-123456789abc",
    "app-12345678-1234-4123-7123-123456789abc",
    "app-12345678-1234-4123-8123-123456789ABC",
  ]) {
    const response = await handleAppGitRequest(
      invalid,
      new Request(validUrl),
      env,
      { clientId: "nanocodex-apps" },
    );
    assert.equal(response.status, 400);
  }

  const mismatch = await handleAppGitRequest(
    appRepository,
    new Request(`https://repository.test/git/${appRepositoryV7}/info/refs?service=git-receive-pack`, {
      headers: { "x-nanocodex-repository": appRepository },
    }),
    env,
    { clientId: "nanocodex-apps" },
  );
  assert.equal(mismatch.status, 400);

  const unsupportedPath = await handleAppGitRequest(
    appRepository,
    new Request(`https://repository.test/git/${appRepository}/objects`),
    env,
    { clientId: "nanocodex-apps" },
  );
  assert.equal(unsupportedPath.status, 400);

  const unsupportedMethod = await handleAppGitRequest(
    appRepository,
    new Request(validUrl, { method: "POST" }),
    env,
    { clientId: "nanocodex-apps" },
  );
  assert.equal(unsupportedMethod.status, 405);
  assert.deepEqual(repositories.requestedNames, []);
  assert.equal(bucket.objects.size, 0);

  const ignoredHeader = await handleAppGitRequest(
    appRepository,
    new Request(validUrl, { headers: { "x-nanocodex-repository": appRepositoryV7 } }),
    env,
    { clientId: "nanocodex-apps" },
  );
  assert.equal(ignoredHeader.status, 200);
  assert.deepEqual(new Set(repositories.requestedNames), new Set([appRepository]));
});

test("app Git lane creates, fetches, and CAS-updates an isolated repository", async () => {
  const repositories = repositoryNamespace();
  const bucket = new MemoryBucket();
  const env = {
    GIT_OBJECTS: bucket as unknown as R2Bucket,
    THREAD_GIT_REPOSITORY: repositories.namespace,
  };

  assert.equal((await advertiseApp(env, appRepositoryV7)).status, 200);
  const empty = await advertiseApp(env, appRepository);
  assert.match((await responseLines(empty))[1]!, new RegExp(`^${zero} capabilities\\^\\{\\}`));
  assert.equal((await pushApp(env, appRepository, zero, headA)).status, 200);
  assert.equal((await pushApp(env, appRepository, headA, headB)).status, 200);

  const repository = await currentRepository(repositories.repository(appRepository));
  assert.equal(repository.head, headB);
  assert.deepEqual(repository.packs.map(({ oldOid, newOid }) => ({ oldOid, newOid })), [
    { oldOid: zero, newOid: headA },
    { oldOid: headA, newOid: headB },
  ]);
  assert.ok(repository.packs.every(({ key }) => key.startsWith(
    `app-repositories/${appRepository}/`,
  )));

  const fetched = await fetchAppV2(env, appRepository, headB, [headA]);
  assert.equal(packObjectCount(await uploadPackBytes(fetched)), 1);

  const stale = await pushApp(env, appRepository, headA, headC);
  assert.equal(stale.status, 200);
  assert.deepEqual((await responseLines(stale)).slice(0, 2), [
    "unpack error stale ref; pull and retry\n",
    `ng ${ref} stale ref; pull and retry\n`,
  ]);
  assert.equal((await currentRepository(repositories.repository(appRepository))).head, headB);
  assert.equal(bucket.objects.size, 2);
});

test("app and public thread Git retain distinct DO and R2 namespaces", async () => {
  const repositories = repositoryNamespace();
  const bucket = new MemoryBucket();
  const env = {
    GIT_OBJECTS: bucket as unknown as R2Bucket,
    THREAD_GIT_REPOSITORY: repositories.namespace,
  };
  const matchingThread = "thread-12345678-1234-4123-8123-123456789abc";

  assert.equal((await pushApp(env, appRepository, zero, headA)).status, 200);
  assert.equal((await pushThread(env, matchingThread, zero, headB)).status, 200);
  assert.equal((await currentRepository(repositories.repository(appRepository))).head, headA);
  assert.equal((await currentRepository(repositories.repository(matchingThread))).head, headB);
  assert.deepEqual(new Set([...bucket.objects.keys()].map((key) => key.split("/", 1)[0])), new Set([
    "app-repositories",
    "thread-repositories",
  ]));

  const publicAppRequest = new Request(
    `https://repository.test/git/${appRepository}/info/refs?service=git-upload-pack`,
  );
  assert.equal(
    await handleThreadGitRequest(publicAppRequest, env, new URL(publicAppRequest.url)),
    undefined,
  );
  const publicThreadRequest = new Request(
    `https://repository.test/git/${matchingThread}/info/refs?service=git-upload-pack`,
  );
  assert.ok(await handleThreadGitRequest(publicThreadRequest, env, new URL(publicThreadRequest.url)));
});

test("thread routes persist refs, CAS updates, and retain every finalized pack", async () => {
  const durable = memoryRepository();
  const bucket = new MemoryBucket();
  const env = {
    GIT_OBJECTS: bucket as unknown as R2Bucket,
    THREAD_GIT_REPOSITORY: namespace(durable) as unknown as DurableObjectNamespace,
  };

  const empty = await advertise(env);
  assert.match((await responseLines(empty))[1]!, new RegExp(`^${zero} capabilities\\^\\{\\}`));

  assert.equal((await push(env, zero, headA)).status, 200);
  const firstAdvertisement = await advertise(env);
  assert.match((await responseLines(firstAdvertisement))[1]!, new RegExp(`^${headA} ${ref}`));
  assert.equal((await push(env, headA, headB)).status, 200);

  const current = await durable.fetch(new Request("https://repository.internal/thread"));
  const repository = await current.json() as ThreadRepository;
  assert.equal(repository.head, headB);
  assert.equal(repository.packs.length, 2);
  assert.deepEqual(repository.packs.map(({ oldOid, newOid }) => ({ oldOid, newOid })), [
    { oldOid: zero, newOid: headA },
    { oldOid: headA, newOid: headB },
  ]);
  assert.equal(bucket.objects.size, 2);
  assert.deepEqual(bucket.deleted, []);

  const [packA, packB] = repository.packs.map(({ key }) => key);
  bucket.resetReads();
  const incremental = await fetchV2(env, headB, [headA]);
  assert.equal(packObjectCount(await uploadPackBytes(incremental)), 1);
  assert.deepEqual(bucket.headCalls, [packB]);
  assert.deepEqual(bucket.getCalls, [packB]);

  bucket.resetReads();
  const legacyIncremental = await fetchLegacy(env, headB, [headA]);
  assert.equal(packObjectCount(await uploadPackBytes(legacyIncremental)), 1);
  assert.deepEqual(bucket.headCalls, [packB]);
  assert.deepEqual(bucket.getCalls, [packB]);

  bucket.resetReads();
  const currentFetch = await fetchV2(env, headB, [headA, headB]);
  assert.equal(packObjectCount(await uploadPackBytes(currentFetch)), 0);
  assert.deepEqual(bucket.headCalls, []);
  assert.deepEqual(bucket.getCalls, []);

  bucket.resetReads();
  const clone = await fetchV2(env, headB, []);
  assert.equal(packObjectCount(await uploadPackBytes(clone)), 2);
  assert.deepEqual(bucket.headCalls, [packA, packB]);
  assert.deepEqual(bucket.getCalls, [packA, packB]);

  const stale = await push(env, headA, headC);
  assert.equal(stale.status, 200);
  assert.deepEqual((await responseLines(stale)).slice(0, 2), [
    "unpack error stale ref; pull and retry\n",
    `ng ${ref} stale ref; pull and retry\n`,
  ]);
  assert.equal(bucket.objects.size, 2);
  assert.deepEqual(bucket.deleted, []);
  const unchanged = await durable.fetch(new Request("https://repository.internal/thread"));
  assert.equal(((await unchanged.json()) as ThreadRepository).head, headB);
});

test("a lost finalize response cannot delete a pack that the DO committed", async () => {
  const durable = memoryRepository();
  const bucket = new MemoryBucket();
  const env = {
    GIT_OBJECTS: bucket as unknown as R2Bucket,
    THREAD_GIT_REPOSITORY: namespace(durable, true) as unknown as DurableObjectNamespace,
  };

  const response = await push(env, zero, headA);
  assert.equal(response.status, 200);
  assert.match((await responseLines(response))[0]!, /^unpack error/);

  const current = await durable.fetch(new Request("https://repository.internal/thread"));
  const repository = await current.json() as ThreadRepository;
  assert.equal(repository.head, headA);
  assert.equal(repository.packs.length, 1);
  assert.equal(bucket.objects.size, 1);
  assert.deepEqual(bucket.deleted, []);
});

function memoryRepository(): ThreadGitRepository {
  const values = new Map<string, unknown>();
  const state = {
    storage: {
      get: async <T>(key: string) => structuredClone(values.get(key)) as T | undefined,
      put: async (key: string, value: unknown) => { values.set(key, structuredClone(value)); },
      delete: async (key: string) => values.delete(key),
    },
    blockConcurrencyWhile: async <T>(callback: () => Promise<T>) => callback(),
  } as unknown as DurableObjectState;
  return new ThreadGitRepository(state);
}

function namespace(durable: ThreadGitRepository, loseFinalizeResponse = false) {
  return {
    idFromName: () => ({}),
    get: () => ({ fetch: async (request: Request | string, init?: RequestInit) => {
      const internal = request instanceof Request ? request : new Request(request, init);
      const response = await durable.fetch(internal);
      if (loseFinalizeResponse && internal.url.endsWith("/receive/finalize") && response.ok) {
        throw new Error("simulated lost finalize response");
      }
      return response;
    } }),
  };
}

function repositoryNamespace(): {
  namespace: DurableObjectNamespace;
  repository(name: string): ThreadGitRepository;
  requestedNames: string[];
} {
  const repositories = new Map<string, ThreadGitRepository>();
  const requestedNames: string[] = [];
  const repository = (name: string) => {
    let durable = repositories.get(name);
    if (!durable) {
      durable = memoryRepository();
      repositories.set(name, durable);
    }
    return durable;
  };
  return {
    namespace: {
      idFromName: (name: string) => name,
      get: (name: string) => ({
        fetch: (request: Request | string, init?: RequestInit) => {
          requestedNames.push(name);
          return repository(name).fetch(request instanceof Request ? request : new Request(request, init));
        },
      }),
    } as unknown as DurableObjectNamespace,
    repository,
    requestedNames,
  };
}

async function currentRepository(durable: ThreadGitRepository): Promise<ThreadRepository> {
  const response = await durable.fetch(new Request("https://repository.internal/thread"));
  assert.equal(response.status, 200);
  return response.json() as Promise<ThreadRepository>;
}

async function advertiseApp(env: {
  GIT_OBJECTS: R2Bucket;
  THREAD_GIT_REPOSITORY: DurableObjectNamespace;
}, name: string): Promise<Response> {
  return handleAppGitRequest(
    name,
    new Request(`https://repository.test/git/${name}/info/refs?service=git-receive-pack`),
    env,
    { clientId: "nanocodex-apps" },
  );
}

async function pushApp(env: {
  GIT_OBJECTS: R2Bucket;
  THREAD_GIT_REPOSITORY: DurableObjectNamespace;
}, name: string, oldOid: string, newOid: string): Promise<Response> {
  return handleAppGitRequest(
    name,
    receiveRequest(`https://repository.test/git/${name}/git-receive-pack`, oldOid, newOid),
    env,
    { clientId: "nanocodex-apps" },
  );
}

async function pushThread(env: {
  GIT_OBJECTS: R2Bucket;
  THREAD_GIT_REPOSITORY: DurableObjectNamespace;
}, name: string, oldOid: string, newOid: string): Promise<Response> {
  const request = receiveRequest(
    `https://repository.test/git/${name}/git-receive-pack`,
    oldOid,
    newOid,
  );
  const response = await handleThreadGitRequest(request, env, new URL(request.url));
  assert.ok(response);
  return response;
}

function receiveRequest(url: string, oldOid: string, newOid: string): Request {
  const body = concatenate([
    encodePacketLine(`${oldOid} ${newOid} ${ref}\0 report-status\n`),
    encoder.encode("0000"),
    sourcePack(),
  ]);
  return new Request(url, { method: "POST", body: body.slice().buffer });
}

async function fetchAppV2(env: {
  GIT_OBJECTS: R2Bucket;
  THREAD_GIT_REPOSITORY: DurableObjectNamespace;
}, name: string, want: string, haves: readonly string[]): Promise<Response> {
  const body = concatenate([
    encodePacketLine("command=fetch\n"),
    encoder.encode("0001"),
    encodePacketLine(`want ${want}\n`),
    ...haves.map((oid) => encodePacketLine(`have ${oid}\n`)),
    encodePacketLine("done\n"),
    encoder.encode("0000"),
  ]);
  return handleAppGitRequest(
    name,
    new Request(`https://repository.test/git/${name}/git-upload-pack`, {
      method: "POST",
      headers: { "git-protocol": "version=2" },
      body: body.slice().buffer,
    }),
    env,
    { clientId: "nanocodex-apps" },
  );
}

async function advertise(env: {
  GIT_OBJECTS: R2Bucket;
  THREAD_GIT_REPOSITORY: DurableObjectNamespace;
}): Promise<Response> {
  const request = new Request(`${remote}/info/refs?service=git-receive-pack`);
  const response = await handleThreadGitRequest(request, env, new URL(request.url));
  assert.ok(response);
  return response;
}

async function push(env: {
  GIT_OBJECTS: R2Bucket;
  THREAD_GIT_REPOSITORY: DurableObjectNamespace;
}, oldOid: string, newOid: string): Promise<Response> {
  const pack = sourcePack();
  const body = concatenate([
    encodePacketLine(`${oldOid} ${newOid} ${ref}\0 report-status\n`),
    encoder.encode("0000"),
    pack,
  ]);
  const request = new Request(`${remote}/git-receive-pack`, {
    method: "POST",
    body: body.slice().buffer,
  });
  const response = await handleThreadGitRequest(request, env, new URL(request.url));
  assert.ok(response);
  return response;
}

async function fetchV2(env: {
  GIT_OBJECTS: R2Bucket;
  THREAD_GIT_REPOSITORY: DurableObjectNamespace;
}, want: string, haves: readonly string[]): Promise<Response> {
  const body = concatenate([
    encodePacketLine("command=fetch\n"),
    encoder.encode("0001"),
    encodePacketLine(`want ${want}\n`),
    ...haves.map((oid) => encodePacketLine(`have ${oid}\n`)),
    encodePacketLine("done\n"),
    encoder.encode("0000"),
  ]);
  const request = new Request(`${remote}/git-upload-pack`, {
    method: "POST",
    headers: { "git-protocol": "version=2" },
    body: body.slice().buffer,
  });
  const response = await handleThreadGitRequest(request, env, new URL(request.url));
  assert.ok(response);
  return response;
}

async function fetchLegacy(env: {
  GIT_OBJECTS: R2Bucket;
  THREAD_GIT_REPOSITORY: DurableObjectNamespace;
}, want: string, haves: readonly string[]): Promise<Response> {
  const body = concatenate([
    encodePacketLine(`want ${want} side-band-64k ofs-delta\n`),
    encoder.encode("0000"),
    ...haves.map((oid) => encodePacketLine(`have ${oid}\n`)),
    encodePacketLine("done\n"),
  ]);
  const request = new Request(`${remote}/git-upload-pack`, {
    method: "POST",
    body: body.slice().buffer,
  });
  const response = await handleThreadGitRequest(request, env, new URL(request.url));
  assert.ok(response);
  return response;
}

async function uploadPackBytes(response: Response): Promise<Uint8Array> {
  assert.equal(response.status, 200);
  const packets = parsePacketLines(new Uint8Array(await response.arrayBuffer()));
  const chunks: Uint8Array[] = [];
  for (const packet of packets) {
    if (packet.kind !== "data") continue;
    const line = decoder.decode(packet.data);
    if (line === "packfile\n" || line === "NAK\n" || line.startsWith("ACK ")) continue;
    assert.equal(packet.data[0], 1);
    chunks.push(packet.data.subarray(1));
  }
  return concatenate(chunks);
}

function packObjectCount(pack: Uint8Array): number {
  assert.equal(decoder.decode(pack.subarray(0, 4)), "PACK");
  return new DataView(pack.buffer, pack.byteOffset, pack.byteLength).getUint32(8);
}

async function responseLines(response: Response): Promise<string[]> {
  return packetLines(new Uint8Array(await response.arrayBuffer()));
}

function packetLines(bytes: Uint8Array): string[] {
  return parsePacketLines(bytes)
    .filter((packet) => packet.kind === "data")
    .map((packet) => decoder.decode(packet.data));
}

class MemoryBucket {
  readonly objects = new Map<string, Uint8Array>();
  readonly deleted: string[] = [];
  readonly headCalls: string[] = [];
  readonly getCalls: string[] = [];

  async put(key: string, value: unknown) {
    const bytes = value instanceof Uint8Array
      ? value
      : new Uint8Array(await new Response(value as BodyInit).arrayBuffer());
    this.objects.set(key, bytes.slice());
    return {};
  }

  async delete(key: string) {
    this.deleted.push(key);
    this.objects.delete(key);
  }

  async head(key: string) {
    this.headCalls.push(key);
    const bytes = this.objects.get(key);
    return bytes ? { size: bytes.byteLength } : null;
  }

  async get(key: string) {
    this.getCalls.push(key);
    const bytes = this.objects.get(key);
    return bytes ? { arrayBuffer: async () => bytes.slice().buffer } : null;
  }

  resetReads() {
    this.headCalls.length = 0;
    this.getCalls.length = 0;
  }
}

function sourcePack(): Uint8Array {
  const header = new Uint8Array(12);
  header.set(encoder.encode("PACK"));
  const view = new DataView(header.buffer);
  view.setUint32(4, 2);
  view.setUint32(8, 1);
  const body = concatenate([header, Uint8Array.of(0x31, 0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01)]);
  return concatenate([body, new Uint8Array(createHash("sha1").update(body).digest())]);
}

function concatenate(chunks: readonly Uint8Array[]): Uint8Array {
  const output = new Uint8Array(chunks.reduce((total, chunk) => total + chunk.byteLength, 0));
  let offset = 0;
  for (const chunk of chunks) {
    output.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return output;
}
