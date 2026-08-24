import git, {
  type GitHttpRequest,
  type GitHttpResponse,
  type HttpClient,
  type ReadCommitResult,
} from "isomorphic-git";

import {
  APP_POLICY_VERSION,
  canonicalJson,
  type GeneratedProject,
  validateProject,
} from "./builder";
import { createMemoryGitFs } from "./memory-fs";

const DIRECTORY = "/workspace";
const BRANCH = "nanocodex";
const REMOTE = "origin";
const MANIFEST_PATH = ".nanocodex/app.json";
const APP_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-7][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const COMMIT_OID = /^[0-9a-f]{40}$/;
const MAX_HTTP_BODY_BYTES = 34 * 1024 * 1024;

export interface AppGitService {
  request(repositoryName: string, request: Request): Promise<Response>;
}

export type CommitProjectInput = Readonly<{
  appId: string;
  expectedAncestorOid: string | null;
  jobId: string;
  prompt: string;
  createdAt: string;
  project: GeneratedProject;
}>;

export type SourceCommit = Readonly<{
  oid: string;
  repository: string;
}>;

export type SourceHistoryEntry = Readonly<{
  oid: string;
  message: string;
  committedAt: string;
}>;

export function appRepositoryName(appId: string): string {
  if (!APP_ID.test(appId)) throw new Error("appId must be a UUID");
  return `app-${appId}`;
}

export async function commitProject(
  service: AppGitService,
  input: CommitProjectInput,
): Promise<SourceCommit> {
  const repository = appRepositoryName(input.appId);
  const project = validateProject(input.project);
  const timestamp = canonicalTimestamp(input.createdAt);
  const fs = createMemoryGitFs();
  const http = appGitHttp(service, repository);
  const remoteUrl = repositoryUrl(repository);
  const manifest = canonicalJson({
    appId: input.appId,
    entryPoint: project.entryPoint,
    jobId: input.jobId,
    policyVersion: APP_POLICY_VERSION,
  });
  const desired = new Map(project.files.map((file) => [file.path, file.content]));
  desired.set(MANIFEST_PATH, manifest);

  await prepareRepository(fs, http, remoteUrl);
  const head = await resolveHead(fs);
  if (head && await headBelongsToJob(fs, head, input.jobId)) {
    await verifyRetryProject(fs, head, input.expectedAncestorOid, desired);
    return { oid: head, repository };
  }
  await verifySourceAncestor(fs, head, input.expectedAncestorOid);

  const tracked = new Set(await git.listFiles({ fs, dir: DIRECTORY }));

  for (const path of tracked) {
    if (desired.has(path)) continue;
    await git.remove({ fs, dir: DIRECTORY, filepath: path });
    await fs.promises.unlink(`${DIRECTORY}/${path}`);
  }
  for (const [path, content] of desired) {
    await ensureParents(fs, `${DIRECTORY}/${path}`);
    await fs.promises.writeFile(`${DIRECTORY}/${path}`, content);
    await git.add({ fs, dir: DIRECTORY, filepath: path });
  }

  const author = {
    name: "Nanocodex Apps",
    email: "apps@nanocodex.dev",
    timestamp: Math.floor(timestamp.valueOf() / 1_000),
    timezoneOffset: 0,
  };
  const oid = await git.commit({
    fs,
    dir: DIRECTORY,
    message: commitMessage(input.prompt, input.jobId),
    author,
    committer: author,
  });
  if (!COMMIT_OID.test(oid)) throw new Error("Git returned an invalid commit object ID");

  try {
    await git.push({
      fs,
      http,
      dir: DIRECTORY,
      remote: REMOTE,
      ref: BRANCH,
      remoteRef: BRANCH,
    });
  } catch (error) {
    if (await remoteHead(http, remoteUrl) !== oid) throw error;
  }
  if (await remoteHead(http, remoteUrl) !== oid) {
    throw new Error("app repository did not publish the expected commit");
  }
  await verifyPublishedProject(service, repository, oid, head ?? null, desired);
  return { oid, repository };
}

export async function sourceHistory(
  service: AppGitService,
  appId: string,
  depth = 100,
): Promise<readonly SourceHistoryEntry[]> {
  if (!Number.isSafeInteger(depth) || depth < 1 || depth > 200) {
    throw new Error("history depth must be between 1 and 200");
  }
  const repository = appRepositoryName(appId);
  const fs = createMemoryGitFs();
  const http = appGitHttp(service, repository);
  const remoteUrl = repositoryUrl(repository);
  await prepareRepository(fs, http, remoteUrl);
  const head = await resolveHead(fs);
  if (!head) return [];
  const commits = await git.log({ fs, dir: DIRECTORY, ref: BRANCH, depth });
  return commits.map(historyEntry);
}

function historyEntry(entry: ReadCommitResult): SourceHistoryEntry {
  return {
    oid: entry.oid,
    message: entry.commit.message,
    committedAt: new Date(entry.commit.committer.timestamp * 1_000).toISOString(),
  };
}

async function prepareRepository(
  fs: ReturnType<typeof createMemoryGitFs>,
  http: HttpClient,
  remoteUrl: string,
): Promise<void> {
  await fs.promises.mkdir(DIRECTORY);
  await git.init({ fs, dir: DIRECTORY, defaultBranch: BRANCH });
  await git.addRemote({ fs, dir: DIRECTORY, remote: REMOTE, url: remoteUrl });
  const head = await remoteHead(http, remoteUrl);
  if (!head) return;
  const fetched = await git.fetch({
    fs,
    http,
    dir: DIRECTORY,
    remote: REMOTE,
    ref: BRANCH,
    singleBranch: true,
  });
  const oid = fetched.fetchHead ?? head;
  await git.writeRef({ fs, dir: DIRECTORY, ref: `refs/heads/${BRANCH}`, value: oid, force: true });
  await git.checkout({ fs, dir: DIRECTORY, ref: BRANCH, force: true });
}

async function remoteHead(http: HttpClient, remoteUrl: string): Promise<string | undefined> {
  const refs = await git.listServerRefs({
    http,
    url: remoteUrl,
    forPush: true,
    protocolVersion: 2,
    prefix: "refs/heads/",
  });
  const ref = refs.find((candidate) => candidate.ref === BRANCH || candidate.ref === `refs/heads/${BRANCH}`);
  return ref && COMMIT_OID.test(ref.oid) ? ref.oid : undefined;
}

async function resolveHead(fs: ReturnType<typeof createMemoryGitFs>): Promise<string | undefined> {
  return git.resolveRef({ fs, dir: DIRECTORY, ref: "HEAD" }).catch(() => undefined);
}

async function headBelongsToJob(
  fs: ReturnType<typeof createMemoryGitFs>,
  oid: string,
  jobId: string,
): Promise<boolean> {
  try {
    const { blob } = await git.readBlob({ fs, dir: DIRECTORY, oid, filepath: MANIFEST_PATH });
    const value = JSON.parse(new TextDecoder().decode(blob)) as { jobId?: unknown };
    return value.jobId === jobId;
  } catch {
    return false;
  }
}

async function verifyPublishedProject(
  service: AppGitService,
  repository: string,
  expectedOid: string,
  previousOid: string | null,
  desired: ReadonlyMap<string, string>,
): Promise<void> {
  const fs = createMemoryGitFs();
  const http = appGitHttp(service, repository);
  await prepareRepository(fs, http, repositoryUrl(repository));
  const head = await resolveHead(fs);
  if (head !== expectedOid) throw new Error("app repository readback returned a different head");
  const { commit } = await git.readCommit({ fs, dir: DIRECTORY, oid: expectedOid });
  if (previousOid && (commit.parent.length !== 1 || commit.parent[0] !== previousOid)) {
    throw new Error("app repository update is not a direct fast-forward");
  }
  if (!previousOid && commit.parent.length !== 0) {
    throw new Error("initial app repository commit has an unexpected parent");
  }
  await verifyCheckedOutProject(fs, expectedOid, desired);
}

async function verifyCheckedOutProject(
  fs: ReturnType<typeof createMemoryGitFs>,
  oid: string,
  desired: ReadonlyMap<string, string>,
): Promise<void> {
  const tracked = (await git.listFiles({ fs, dir: DIRECTORY })).sort();
  const expected = [...desired.keys()].sort();
  if (tracked.length !== expected.length || tracked.some((path, index) => path !== expected[index])) {
    throw new Error("app repository readback returned a different source tree");
  }
  const decoder = new TextDecoder("utf-8", { fatal: true });
  for (const [path, content] of desired) {
    const { blob } = await git.readBlob({ fs, dir: DIRECTORY, oid, filepath: path });
    if (decoder.decode(blob) !== content) {
      throw new Error(`app repository readback changed ${path}`);
    }
  }
}

async function verifyRetryProject(
  fs: ReturnType<typeof createMemoryGitFs>,
  oid: string,
  expectedAncestorOid: string | null,
  desired: ReadonlyMap<string, string>,
): Promise<void> {
  const { commit } = await git.readCommit({ fs, dir: DIRECTORY, oid });
  if (expectedAncestorOid) {
    if (commit.parent.length !== 1) {
      throw new Error("app repository retry is not a direct fast-forward");
    }
    await verifySourceAncestor(fs, commit.parent[0], expectedAncestorOid);
  } else if (commit.parent.length !== 0) {
    throw new Error("initial app repository retry has an unexpected parent");
  }
  await verifyCheckedOutProject(fs, oid, desired);
}

async function verifySourceAncestor(
  fs: ReturnType<typeof createMemoryGitFs>,
  head: string | undefined,
  expectedAncestorOid: string | null,
): Promise<void> {
  if (!expectedAncestorOid) {
    if (head) throw new Error("initial app repository unexpectedly has source history");
    return;
  }
  if (!head) throw new Error("app repository is missing its published source revision");
  if (head === expectedAncestorOid) return;
  if (!await git.isDescendent({
    fs,
    dir: DIRECTORY,
    oid: head,
    ancestor: expectedAncestorOid,
    depth: -1,
  })) {
    throw new Error("app repository head does not descend from the published source revision");
  }
}

function appGitHttp(service: AppGitService, repository: string): HttpClient {
  return {
    request: async (input: GitHttpRequest): Promise<GitHttpResponse> => {
      const parsed = new URL(input.url);
      if (parsed.origin !== "https://git.internal" || !parsed.pathname.startsWith(`/git/${repository}/`)) {
        throw new Error("Git client attempted to escape its app repository");
      }
      const body = input.body ? await collect(input.body, MAX_HTTP_BODY_BYTES) : undefined;
      const response = await service.request(repository, new Request(parsed, {
        method: input.method ?? "GET",
        headers: input.headers,
        ...(body ? { body: arrayBuffer(body) } : {}),
      }));
      return {
        url: input.url,
        method: input.method,
        headers: Object.fromEntries(response.headers),
        body: response.body ? responseIterator(response.body) : undefined,
        statusCode: response.status,
        statusMessage: response.statusText,
      };
    },
  };
}

function repositoryUrl(repository: string): string {
  return `https://git.internal/git/${repository}`;
}

function arrayBuffer(bytes: Uint8Array): ArrayBuffer {
  const copy = new Uint8Array(bytes.byteLength);
  copy.set(bytes);
  return copy.buffer;
}

async function collect(
  source: AsyncIterableIterator<Uint8Array>,
  maxBytes: number,
): Promise<Uint8Array> {
  const chunks: Uint8Array[] = [];
  let total = 0;
  for await (const chunk of source) {
    total += chunk.byteLength;
    if (total > maxBytes) throw new Error("Git request exceeds the app repository limit");
    chunks.push(chunk);
  }
  const body = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return body;
}

async function* responseIterator(stream: ReadableStream<Uint8Array>): AsyncIterableIterator<Uint8Array> {
  const reader = stream.getReader();
  try {
    while (true) {
      const next = await reader.read();
      if (next.done) return;
      yield next.value;
    }
  } finally {
    reader.releaseLock();
  }
}

async function ensureParents(fs: ReturnType<typeof createMemoryGitFs>, path: string): Promise<void> {
  const segments = path.split("/").filter(Boolean);
  segments.pop();
  let current = "";
  for (const segment of segments) {
    current += `/${segment}`;
    await fs.promises.mkdir(current).catch((error: unknown) => {
      if (!isFsError(error, "EEXIST")) throw error;
    });
  }
}

function canonicalTimestamp(value: string): Date {
  const parsed = new Date(value);
  if (!Number.isFinite(parsed.valueOf()) || parsed.toISOString() !== value) {
    throw new Error("createdAt must be a canonical ISO timestamp");
  }
  return parsed;
}

function commitMessage(prompt: string, jobId: string): string {
  const summary = prompt.replace(/[\r\n\t]+/g, " ").trim().slice(0, 160) || "Update app";
  return `${summary}\n\nNanocodex-Job: ${jobId}`;
}

function isFsError(error: unknown, code: string): boolean {
  return Boolean(error && typeof error === "object" && "code" in error && error.code === code);
}
