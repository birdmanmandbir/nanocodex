import { execFile, spawn } from "node:child_process";
import { createWriteStream } from "node:fs";
import {
  mkdtemp,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, resolve } from "node:path";
import { pipeline } from "node:stream/promises";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";

const execFileAsync = promisify(execFile);
const scriptPath = fileURLToPath(import.meta.url);
const projectRoot = resolve(dirname(scriptPath), "..");
const repositoryPath = resolve(
  process.env.NANOCODEX_REPO ?? resolve(projectRoot, ".."),
);
const uploadConcurrency = 12;

if (resolve(process.argv[1] ?? "") === scriptPath) {
  await main();
}

async function main() {
  const origin = requiredEnvironment("NANOCODEX_GIT_ORIGIN").replace(/\/$/, "");
  const token = requiredEnvironment("NANOCODEX_GIT_TOKEN");
  const previous = await readRemoteState(origin, token);
  const head = await git(["rev-parse", "HEAD"]);
  if (previous?.publication?.head === head && process.env.NANOCODEX_FORCE_SYNC !== "1") {
    console.log(`Cloudflare repository is current (${head.slice(0, 7)})`);
    return;
  }

  const temporaryDirectory = await mkdtemp(resolve(tmpdir(), "nanocodex-git-"));
  try {
    const dataDirectory = resolve(temporaryDirectory, "data");
    await run(process.execPath, [resolve(projectRoot, "scripts", "sync-nanocodex.mjs")], {
      cwd: projectRoot,
      env: {
        ...process.env,
        NANOCODEX_DATA_DIR: dataDirectory,
        NANOCODEX_EMIT_OBJECTS: "1",
        NANOCODEX_FORCE_SYNC: "1",
        NANOCODEX_REPO: repositoryPath,
      },
    });

    const packPath = resolve(temporaryDirectory, "repository.pack");
    const indexPath = resolve(temporaryDirectory, "repository.idx");
    await writeCompletePack(packPath);
    const { stdout: packHashOutput } = await execFileAsync(
      "git",
      ["index-pack", "--index-version=2", "-o", indexPath, packPath],
      { cwd: repositoryPath, encoding: "utf8" },
    );
    const packHash = packHashOutput.trim();
    if (!/^[a-f0-9]{40}$/.test(packHash)) {
      throw new Error(`git index-pack returned an invalid hash: ${packHash}`);
    }
    await execFileAsync("git", ["verify-pack", "-s", indexPath], {
      cwd: repositoryPath,
      encoding: "utf8",
    });

    const [snapshot, commits, refs, blobNames, patchNames, commitPageNames] = await Promise.all([
      readJson(resolve(dataDirectory, "repository.json")),
      readJson(resolve(dataDirectory, "commits.json")),
      readRefs(),
      listObjectNames(resolve(dataDirectory, "blobs"), ".txt"),
      listObjectNames(resolve(dataDirectory, "patches"), ".patch"),
      listObjectNames(resolve(dataDirectory, "commit-pages"), ".json"),
    ]);
    if (snapshot.repository?.head !== head) {
      throw new Error("repository changed while its publication was being built");
    }

    const inventory = {
      version: 1,
      head,
      blobs: blobNames,
      patches: patchNames,
    };
    const previousInventory = previous?.inventory ?? { blobs: [], patches: [] };
    const plan = buildUploadPlan(inventory, previousInventory);
    console.log(
      `Uploading ${plan.blobs.length} new blobs and ${plan.patches.length} new patches`,
    );
    await mapConcurrent(
      [
        ...plan.blobs.map((id) => ({
          remote: `blobs/${id}`,
          local: resolve(dataDirectory, "blobs", `${id}.txt`),
        })),
        ...plan.patches.map((id) => ({
          remote: `patches/${id}`,
          local: resolve(dataDirectory, "patches", `${id}.patch`),
        })),
      ],
      uploadConcurrency,
      ({ remote, local }) => uploadFile(origin, token, remote, local),
    );

    const generationPrefix = `generations/${head}`;
    const inventoryPath = resolve(temporaryDirectory, "inventory.json");
    await writeFile(inventoryPath, `${JSON.stringify(inventory)}\n`);
    await Promise.all([
      uploadFile(origin, token, `${generationPrefix}/repository.json`, resolve(dataDirectory, "repository.json")),
      uploadFile(origin, token, `${generationPrefix}/commits.json`, resolve(dataDirectory, "commits.json")),
      uploadFile(origin, token, `${generationPrefix}/inventory.json`, inventoryPath),
      uploadFile(origin, token, `${generationPrefix}/repository.idx`, indexPath),
      mapConcurrent(commitPageNames, uploadConcurrency, (name) => uploadFile(
          origin,
          token,
          `${generationPrefix}/commits/${name}`,
          resolve(dataDirectory, "commit-pages", `${name}.json`),
        )),
    ]);
    await uploadFile(origin, token, `${generationPrefix}/repository.pack`, packPath);

    const publication = {
      version: 1,
      head,
      branch: snapshot.repository.branch,
      refs,
      snapshotKey: `${generationPrefix}/repository.json`,
      commitsKey: `${generationPrefix}/commits.json`,
      inventoryKey: `${generationPrefix}/inventory.json`,
      packKey: `${generationPrefix}/repository.pack`,
      packIndexKey: `${generationPrefix}/repository.idx`,
      packHash,
      publishedAt: new Date().toISOString(),
    };
    const response = await authenticatedFetch(`${origin}/api/git/publish`, token, {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        expectedHead: previous?.publication?.head ?? null,
        publication,
      }),
    });
    if (!response.ok) throw new Error(await responseError("publish", response));
    console.log(
      `Published ${snapshot.repository.fullName} ${head.slice(0, 7)} (${commits.length} commits, ${packHash.slice(0, 7)} pack)`,
    );
  } finally {
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
}

export function buildUploadPlan(inventory, previousInventory) {
  const previousBlobs = new Set(previousInventory.blobs ?? []);
  const previousPatches = new Set(previousInventory.patches ?? []);
  return {
    blobs: inventory.blobs.filter((id) => !previousBlobs.has(id)),
    patches: inventory.patches.filter((id) => !previousPatches.has(id)),
  };
}

async function readRemoteState(origin, token) {
  const response = await authenticatedFetch(`${origin}/api/git/state`, token);
  if (response.status === 503 || response.status === 404) return null;
  if (!response.ok) throw new Error(await responseError("read state", response));
  return response.json();
}

async function writeCompletePack(path) {
  const child = spawn("git", ["pack-objects", "--all", "--stdout", "--revs"], {
    cwd: repositoryPath,
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stderr = "";
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk) => { stderr += chunk; });
  await Promise.all([
    pipeline(child.stdout, createWriteStream(path, { flags: "wx" })),
    new Promise((resolveExit, reject) => {
      child.once("error", reject);
      child.once("close", (code) => {
        if (code === 0) resolveExit();
        else reject(new Error(stderr.trim() || `git pack-objects exited with ${code}`));
      });
    }),
  ]);
}

async function readRefs() {
  const output = await git([
    "for-each-ref",
    "--format=%(refname)%00%(objectname)",
    "refs/heads",
    "refs/tags",
  ]);
  return output.split("\n").filter(Boolean).map((row) => {
    const [name, oid] = row.split("\0");
    if (!name || !/^[a-f0-9]{40}$/.test(oid ?? "")) {
      throw new Error(`invalid Git ref row: ${row}`);
    }
    return { name, oid };
  });
}

async function uploadFile(origin, token, remote, local) {
  const contents = await readFile(local);
  let lastError;
  for (let attempt = 1; attempt <= 5; attempt += 1) {
    const response = await authenticatedFetch(
      `${origin}/api/git/objects/${remote}`,
      token,
      { method: "PUT", body: contents },
    );
    if (response.ok) return;
    lastError = new Error(await responseError(`upload ${basename(local)}`, response));
    if (!isRetriableUploadStatus(response.status) || attempt === 5) break;
    await delay(250 * (2 ** (attempt - 1)));
  }
  throw lastError;
}

export function isRetriableUploadStatus(status) {
  return status === 401 || status === 408 || status === 425 || status === 429 || status >= 500;
}

function delay(milliseconds) {
  return new Promise((resolveDelay) => setTimeout(resolveDelay, milliseconds));
}

function authenticatedFetch(url, token, init = {}) {
  const headers = new Headers(init.headers);
  headers.set("authorization", `Bearer ${token}`);
  return fetch(url, { ...init, headers });
}

async function responseError(operation, response) {
  return `${operation} failed with HTTP ${response.status}: ${(await response.text()).slice(0, 1_000)}`;
}

async function listObjectNames(path, suffix) {
  return (await readdir(path))
    .filter((name) => name.endsWith(suffix))
    .map((name) => name.slice(0, -suffix.length))
    .sort();
}

async function readJson(path) {
  return JSON.parse(await readFile(path, "utf8"));
}

async function mapConcurrent(values, concurrency, operation) {
  let nextIndex = 0;
  await Promise.all(Array.from({ length: Math.min(concurrency, values.length) }, async () => {
    while (nextIndex < values.length) {
      const index = nextIndex++;
      await operation(values[index], index);
    }
  }));
}

async function run(command, args, options) {
  const child = spawn(command, args, { ...options, stdio: "inherit" });
  await new Promise((resolveExit, reject) => {
    child.once("error", reject);
    child.once("close", (code) => {
      if (code === 0) resolveExit();
      else reject(new Error(`${command} exited with ${code}`));
    });
  });
}

async function git(args) {
  const { stdout } = await execFileAsync("git", args, {
    cwd: repositoryPath,
    encoding: "utf8",
    maxBuffer: 16 * 1024 * 1024,
  });
  return stdout.trimEnd();
}

function requiredEnvironment(name) {
  const value = process.env[name]?.trim();
  if (!value) throw new Error(`${name} is required`);
  return value;
}
