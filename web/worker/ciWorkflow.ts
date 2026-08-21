import {
  CIWorkflow,
  isCiRunnerFailure,
  type CiContext,
  type CiParams,
  type CiRunnerResult,
} from "@cloudflare/ci";
import type { CiBindings } from "@cloudflare/ci/worker";
import { CiSandbox } from "@cloudflare/ci/worker";
import type { SourceControlAdapter } from "@cloudflare/ci/worker/source-control";
import type { WorkflowEvent, WorkflowStep } from "cloudflare:workers";

import {
  cargoVendorBundleKey,
  isCiSourceTree,
  isSha1,
  isSha256,
  rustSecAdvisoryBundleKey,
  sourceArchiveKey,
  sourceTreeKey,
  type NanocodexCiProviderData,
} from "./ciSource.ts";
import {
  bindingsCommand,
  cargoCacheInputs,
  cargoDependencyCommand,
  completeDependencyCacheInputs,
  javascriptDependencyCommand,
  pythonCommand,
  refreshSourceCommand,
  rustBuildCacheCommand,
  rustPipeline,
  websiteCommand,
} from "./ciWorkflowPlan.ts";

type NanocodexSourceProvider = {
  id: "nanocodex-source";
  event: { type: "push" };
  providerData: NanocodexCiProviderData;
};

type NanocodexCiBindings = CiBindings & {
  CI_SOURCE: R2Bucket;
  CI_PUBLIC_ORIGIN: string;
};

const COMMON_ENV = {
  CARGO_HOME: "/workspace/.cargo-home",
  CARGO_TARGET_DIR: "/workspace/.cargo-target",
  CARGO_INCREMENTAL: "0",
  CARGO_TERM_COLOR: "always",
};

export { CiSandbox };

export class NanocodexCI extends CIWorkflow<
  NanocodexSourceProvider,
  NanocodexCiBindings
> {
  static override getProvider(): SourceControlAdapter {
    // @cloudflare/ci's static declaration currently fixes this return type to
    // its default provider even though the instance API is generic.
    return nanocodexSource() as unknown as SourceControlAdapter;
  }

  protected async pipeline(
    event: WorkflowEvent<CiParams<NanocodexSourceProvider>>,
    step: WorkflowStep,
    ci: CiContext,
  ): Promise<void> {
    const head = event.payload.sha;
    const source = providerData(event.payload.providerData, head);
    const pipelineStartedAt = Date.now();
    await step.do("persist CI running state", EVIDENCE_STEP_CONFIG, async () => {
      await this.env.BACKUP_BUCKET.put(
        `runs/${head}/result.json`,
        JSON.stringify({
          version: 1,
          head,
          workflowId: event.instanceId,
          status: "running",
          rustSecRevision: source.rustSecRevision,
          rustSecSha256: source.rustSecSha256,
          startedAt: new Date().toISOString(),
          steps: [],
        }),
        { httpMetadata: { contentType: "application/json" } },
      );
    });

    const completed: Array<{
      name: string;
      exitCode: number;
      cacheHit: boolean;
      durationMs: number;
    }> = [];
    const artifacts: CiArtifact[] = [];
    let gatesCompleted = false;
    try {
      const cargoStartedAt = Date.now();
      const dependencies = await ci.runner({
        name: "cargo dependencies",
        command: cargoDependencyCommand(
          `${this.env.CI_PUBLIC_ORIGIN.replace(/\/$/, "")}/api/ci/cargo-vendor/${source.cargoLockBlob}/bundle.tar.gz`,
          source.cargoVendorSize,
          source.cargoVendorSha256,
        ),
        env: COMMON_ENV,
        cache: { inputs: cargoCacheInputs() },
        config: runnerConfig(20 * 60 * 1_000, 30 * 24 * 60 * 60, 1),
      });
      const cargoPersistence = persistRunner(
        this.env.BACKUP_BUCKET,
        head,
        dependencies,
        "cargo-dependencies",
      ).then((metadata) => {
        completed.push({
          name: "cargo dependencies",
          exitCode: dependencies.exitCode,
          cacheHit: metadata.cacheHit,
          durationMs: Date.now() - cargoStartedAt,
        });
      });

      const runRustJob = async (
        parent: CiRunnerResult,
        job: ReturnType<typeof rustPipeline>[number],
        refreshSource: boolean,
      ) => {
        const startedAt = Date.now();
        const result = await parent.runner({
          name: job.name,
          command: cleanupAfter(
            refreshSource ? refreshSourceCommand(job.command) : job.command,
          ),
          env:
            job.name === "MSRV workspace tests"
              ? {
                  ...COMMON_ENV,
                  CARGO_TARGET_DIR: "/tmp/nanocodex-msrv-target",
                }
              : COMMON_ENV,
          config: runnerConfig(job.timeoutMs, 24 * 60 * 60, 0, false),
        });
        await persistRunner(
          this.env.BACKUP_BUCKET,
          head,
          result,
          slug(job.name),
        );
        completed.push({
          name: job.name,
          exitCode: result.exitCode,
          cacheHit: false,
          durationMs: Date.now() - startedAt,
        });
      };
      const rustJobs = rustPipeline({
        url: `${this.env.CI_PUBLIC_ORIGIN.replace(/\/$/, "")}/api/ci/rustsec-advisory-db/${source.rustSecRevision}/bundle.tar.gz`,
        revision: source.rustSecRevision,
        size: source.rustSecSize,
        sha256: source.rustSecSha256,
      });
      const buildCacheBranch = (async () => {
        const buildCacheStartedAt = Date.now();
        const buildCache = await dependencies.runner({
          name: "Rust build cache",
          command: rustBuildCacheCommand(),
          env: COMMON_ENV,
          config: runnerConfig(45 * 60 * 1_000, 30 * 24 * 60 * 60),
        });
        const buildCachePersistence = persistRunner(
          this.env.BACKUP_BUCKET,
          head,
          buildCache,
          "rust-build-cache",
        ).then((metadata) => {
          completed.push({
            name: "Rust build cache",
            exitCode: buildCache.exitCode,
            cacheHit: metadata.cacheHit,
            durationMs: Date.now() - buildCacheStartedAt,
          });
        });
        const cachedRustJobs = rustJobs
          .filter(
            ({ name }) =>
              name === "stable workspace tests" || name === "quality",
          )
          .map((job) => runRustJob(buildCache, job, true));
        await allSettledOrThrow([buildCachePersistence, ...cachedRustJobs]);
      })();
      const directRustJobs = rustJobs
        .filter(
          ({ name }) => name !== "stable workspace tests" && name !== "quality",
        )
        .map((job) => runRustJob(dependencies, job, false));
      const pythonJobs = (["3.11", "3.14"] as const).map(async (version) => {
        const name = `Python ${version}`;
        const startedAt = Date.now();
        const result = await dependencies.runner({
          name,
          command: cleanupAfter(pythonCommand(version)),
          env: COMMON_ENV,
          config: runnerConfig(40 * 60 * 1_000, 24 * 60 * 60, 0, false),
        });
        await persistRunner(this.env.BACKUP_BUCKET, head, result, slug(name));
        completed.push({
          name,
          exitCode: result.exitCode,
          cacheHit: false,
          durationMs: Date.now() - startedAt,
        });
      });
      const webJob = (async () => {
        const completeDependenciesStartedAt = Date.now();
        const completeDependencies = await dependencies.runner({
          name: "all dependencies",
          command: javascriptDependencyCommand(),
          env: COMMON_ENV,
          cache: { inputs: completeDependencyCacheInputs() },
          config: runnerConfig(30 * 60 * 1_000, 30 * 24 * 60 * 60, 1),
        });
        const dependencyPersistence = persistRunner(
          this.env.BACKUP_BUCKET,
          head,
          completeDependencies,
          "all-dependencies",
        ).then((metadata) => {
          completed.push({
            name: "all dependencies",
            exitCode: completeDependencies.exitCode,
            cacheHit: metadata.cacheHit,
            durationMs: Date.now() - completeDependenciesStartedAt,
          });
        });
        const bindingsStartedAt = Date.now();
        const wasmArtifactKey = `runs/${head}/artifacts/web-wasm.tar`;
        const bindings = await completeDependencies.runner({
          name: "Node and browser bindings",
          command: cleanupAfter(bindingsCommand(), [".ci-output"]),
          env: COMMON_ENV,
          outputs: [
            {
              path: "/workspace/.ci-output/web-wasm.tar",
              sha256Path: "/workspace/.ci-output/web-wasm.tar.sha256",
              key: wasmArtifactKey,
              maxBytes: 16 * 1024 * 1024,
              contentType: "application/x-tar",
              customMetadata: { head, kind: "web-wasm" },
            },
          ],
          config: runnerConfig(60 * 60 * 1_000, 24 * 60 * 60, 0, false),
        });
        const wasmArtifact = artifactRecord(
          bindings.outputs?.[0],
          head,
          wasmArtifactKey,
          "web-wasm",
        );
        artifacts.push(wasmArtifact);
        const bindingsPersistence = persistRunner(
          this.env.BACKUP_BUCKET,
          head,
          bindings,
          "node-and-browser-bindings",
        ).then(() => {
          completed.push({
            name: "Node and browser bindings",
            exitCode: bindings.exitCode,
            cacheHit: false,
            durationMs: Date.now() - bindingsStartedAt,
          });
        });
        const websiteStartedAt = Date.now();
        const artifactKey = `runs/${head}/artifacts/web-dist.tar`;
        const website = await completeDependencies.runner({
          name: "website",
          command: cleanupAfter(
            websiteCommand(
              `${this.env.CI_PUBLIC_ORIGIN.replace(/\/$/, "")}/api/ci/runs/${head}/artifacts/web-wasm.tar`,
              wasmArtifact.size,
              wasmArtifact.sha256,
            ),
            [".ci-output"],
          ),
          env: COMMON_ENV,
          outputs: [
            {
              path: "/workspace/.ci-output/web-dist.tar",
              sha256Path: "/workspace/.ci-output/web-dist.tar.sha256",
              key: artifactKey,
              maxBytes: 64 * 1024 * 1024,
              contentType: "application/x-tar",
              customMetadata: { head, kind: "web-dist" },
            },
          ],
          config: runnerConfig(45 * 60 * 1_000, 24 * 60 * 60, 0, false),
        });
        await persistRunner(this.env.BACKUP_BUCKET, head, website, "website");
        artifacts.push(
          artifactRecord(
            website.outputs?.[0],
            head,
            artifactKey,
            "web-dist",
          ),
        );
        await Promise.all([dependencyPersistence, bindingsPersistence]);
        completed.push({
          name: "website",
          exitCode: website.exitCode,
          cacheHit: false,
          durationMs: Date.now() - websiteStartedAt,
        });
      })();
      await allSettledOrThrow([
        cargoPersistence,
        buildCacheBranch,
        ...directRustJobs,
        ...pythonJobs,
        webJob,
      ]);

      completed.sort((left, right) => left.name.localeCompare(right.name));
      await step.do("persist CI success", EVIDENCE_STEP_CONFIG, async () => {
        await this.env.BACKUP_BUCKET.put(
          `runs/${head}/result.json`,
          JSON.stringify({
            version: 1,
            head,
            workflowId: event.instanceId,
            status: "success",
            rustSecRevision: source.rustSecRevision,
            rustSecSha256: source.rustSecSha256,
            completedAt: new Date().toISOString(),
            durationMs: Date.now() - pipelineStartedAt,
            steps: completed,
            artifacts,
          }),
          { httpMetadata: { contentType: "application/json" } },
        );
      });
      gatesCompleted = true;
    } catch (cause) {
      if (!gatesCompleted) {
        await step.do("persist CI failure", EVIDENCE_STEP_CONFIG, async () => {
          await this.env.BACKUP_BUCKET.put(
            `runs/${head}/result.json`,
            JSON.stringify({
              version: 1,
              head,
              workflowId: event.instanceId,
              status: "failure",
              rustSecRevision: source.rustSecRevision,
              rustSecSha256: source.rustSecSha256,
              completedAt: new Date().toISOString(),
              durationMs: Date.now() - pipelineStartedAt,
              steps: completed.sort((left, right) =>
                left.name.localeCompare(right.name),
              ),
              failure: failureRecord(cause),
            }),
            { httpMetadata: { contentType: "application/json" } },
          );
        });
      }
      throw cause;
    }
  }
}

export function nanocodexSource(): SourceControlAdapter<NanocodexSourceProvider> {
  return {
    id: "nanocodex-source",
    repository: { owner: "gakonst", repo: "nanocodex" },
    create(env) {
      return new NanocodexSourceControl(env as NanocodexCiBindings);
    },
    accepts(source) {
      return (
        source.provider === "nanocodex-source" &&
        source.owner === "gakonst" &&
        source.repo === "nanocodex"
      );
    },
    assertSource(source) {
      if (!this.accepts(source)) {
        throw new Error(
          `Unsupported CI source: ${source.provider}:${source.owner}/${source.repo}`,
        );
      }
    },
  };
}

class NanocodexSourceControl {
  readonly #env: NanocodexCiBindings;

  constructor(env: NanocodexCiBindings) {
    this.#env = env;
  }

  receiveEvent(): Promise<null> {
    return Promise.resolve(null);
  }

  async getSourceCheckout(source: SourceIdentity) {
    const data = providerData(source.providerData, source.sha);
    const object = await this.#env.CI_SOURCE.head(data.archiveKey);
    if (
      !object ||
      object.size !== data.archiveSize ||
      object.customMetadata?.sha256 !== data.archiveSha256 ||
      object.checksums.sha256 == null ||
      checksumHex(object.checksums.sha256) !== data.archiveSha256
    )
      throw new Error(
        `CI source archive is missing or invalid for ${source.sha}`,
      );
    return {
      kind: "archive" as const,
      url: `${this.#env.CI_PUBLIC_ORIGIN.replace(/\/$/, "")}/api/ci/source/${source.sha}/archive`,
      size: data.archiveSize,
      sha256: data.archiveSha256,
    };
  }

  async listTreeBlobs(source: SourceIdentity, paths: string[]) {
    const data = providerData(source.providerData, source.sha);
    const object = await this.#env.CI_SOURCE.get(data.treeKey);
    if (
      !object ||
      object.customMetadata?.sha256 !== data.treeSha256 ||
      object.checksums.sha256 == null ||
      checksumHex(object.checksums.sha256) !== data.treeSha256
    )
      return null;
    const tree: unknown = await object.json().catch(() => undefined);
    if (!isCiSourceTree(tree, source.sha)) return null;
    const patterns = paths.map(globToRegExp);
    return tree.files
      .filter((file) => patterns.some((pattern) => pattern.test(file.path)))
      .map(({ path, sha }) => ({ path, sha }));
  }

  getStepCredentialEnv(): Promise<Record<string, string>> {
    return Promise.resolve({});
  }

  getPushCredentials(): Promise<never> {
    return Promise.reject(new Error("Nanocodex CI source is publication-only"));
  }

  createPullRequest(): Promise<{ status: "skipped" }> {
    return Promise.resolve({ status: "skipped" });
  }

  startStepNotification(): Promise<null> {
    return Promise.resolve(null);
  }
}

type SourceIdentity = {
  owner: string;
  repo: string;
  sha: string;
  providerData: unknown;
};

function providerData(value: unknown, head: string): NanocodexCiProviderData {
  if (value == null || typeof value !== "object")
    throw new Error("CI provider data is missing");
  const data = value as Partial<NanocodexCiProviderData>;
  if (
    data.archiveKey !== sourceArchiveKey(head) ||
    !isSha256(data.archiveSha256) ||
    typeof data.archiveSize !== "number" ||
    !Number.isSafeInteger(data.archiveSize) ||
    data.archiveSize <= 0 ||
    data.archiveSize > 128 * 1024 * 1024 ||
    data.treeKey !== sourceTreeKey(head) ||
    !isSha256(data.treeSha256) ||
    !isSha1(data.cargoLockBlob) ||
    data.cargoVendorKey !== cargoVendorBundleKey(data.cargoLockBlob) ||
    !isSha256(data.cargoVendorSha256) ||
    typeof data.cargoVendorSize !== "number" ||
    !Number.isSafeInteger(data.cargoVendorSize) ||
    data.cargoVendorSize <= 0 ||
    data.cargoVendorSize > 16 * 1024 * 1024 ||
    !isSha1(data.rustSecRevision) ||
    data.rustSecKey !== rustSecAdvisoryBundleKey(data.rustSecRevision) ||
    !isSha256(data.rustSecSha256) ||
    typeof data.rustSecSize !== "number" ||
    !Number.isSafeInteger(data.rustSecSize) ||
    data.rustSecSize <= 0 ||
    data.rustSecSize > 16 * 1024 * 1024
  )
    throw new Error("CI provider data is invalid");
  return data as NanocodexCiProviderData;
}

function cleanupAfter(command: string, preserve: string[] = []): string {
  const exclusions = preserve
    .map((entry) => ` ! -name ${shellQuote(entry)}`)
    .join("");
  const cleanup = String.raw`status=$?; if [ "$status" -eq 0 ]; then find /workspace -mindepth 1 -maxdepth 1${exclusions} -exec rm -rf -- {} +; fi; exit "$status"`;
  return `bash -c ${shellQuote(`${command}; ${cleanup}`)}`;
}

const RUNNER_FINALIZATION_MARGIN_MS = 5 * 60 * 1_000;
const EVIDENCE_STEP_CONFIG = {
  timeout: 60_000,
  retries: { limit: 3, delay: 1_000, backoff: "exponential" as const },
};

function runnerConfig(
  commandTimeoutMs: number,
  retention: number,
  retries = 0,
  snapshot = true,
) {
  return {
    timeout: commandTimeoutMs + RUNNER_FINALIZATION_MARGIN_MS,
    commandTimeoutMs,
    snapshotRetentionSeconds: retention,
    snapshot,
    retries: { limit: retries, delay: 30_000, backoff: "linear" as const },
  };
}

async function allSettledOrThrow(promises: Promise<unknown>[]): Promise<void> {
  const outcomes = await Promise.allSettled(promises);
  const failed = outcomes.find(
    (outcome): outcome is PromiseRejectedResult =>
      outcome.status === "rejected",
  );
  if (failed) throw failed.reason;
}

function runnerCacheHit(result: CiRunnerResult): boolean {
  return (
    typeof result.logs.stdout === "string" &&
    result.logs.stdout.startsWith("cache hit: reusing ")
  );
}

type CiArtifact = {
  key: string;
  size: number;
  sha256: string;
  contentType: string;
};

function artifactRecord(
  output: NonNullable<CiRunnerResult["outputs"]>[number] | undefined,
  head: string,
  key: string,
  kind: "web-wasm" | "web-dist",
): CiArtifact {
  const sha256 = output?.sha256;
  if (
    output == null ||
    output.key !== key ||
    output.size <= 0 ||
    !isSha256(sha256) ||
    output.contentType !== "application/x-tar"
  )
    throw new Error(`${kind} artifact is invalid for ${head}`);
  return { key, size: output.size, sha256, contentType: output.contentType };
}

async function persistRunner(
  bucket: R2Bucket,
  head: string,
  result: CiRunnerResult,
  name: string,
): Promise<{ cacheHit: boolean }> {
  const prefix = `runs/${head}/steps/${name}`;
  const cacheHit = runnerCacheHit(result);
  const [stdout, stderr] =
    result.persistedLogs == null
      ? await Promise.all([
          persistLog(bucket, `${prefix}/stdout.log`, result.logs.stdout),
          persistLog(bucket, `${prefix}/stderr.log`, result.logs.stderr),
        ])
      : [result.persistedLogs.stdout, result.persistedLogs.stderr];
  await bucket.put(
    `${prefix}/result.json`,
    JSON.stringify({
      version: 1,
      exitCode: result.exitCode,
      stdout,
      stderr,
      logMetadata: result.logMetadata,
      timedOut: result.timedOut,
      cacheHit,
      snapshot: result.snapshot,
      cachePointer: result.cachePointer ?? null,
      outputs: result.outputs ?? [],
    }),
    { httpMetadata: { contentType: "application/json" } },
  );
  return { cacheHit };
}

async function persistLog(
  bucket: R2Bucket,
  key: string,
  value: string | ReadableStream<Uint8Array>,
): Promise<{ key: string; size: number }> {
  const object = await bucket.put(key, value, {
    httpMetadata: { contentType: "text/plain; charset=utf-8" },
  });
  if (!object) throw new Error(`Failed to persist CI log ${key}`);
  return { key, size: object.size };
}

function failureRecord(value: unknown) {
  if (isCiRunnerFailure(value)) {
    return {
      name: value.name,
      message: value.message,
      diagnostics: value.diagnostics,
    };
  }
  return {
    name: value instanceof Error ? value.name : "Error",
    message: (value instanceof Error ? value.message : String(value)).slice(
      0,
      20_000,
    ),
  };
}

function slug(value: string): string {
  return value
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-|-$/g, "");
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", `'"'"'`)}'`;
}

function globToRegExp(glob: string): RegExp {
  let pattern = "^";
  for (let index = 0; index < glob.length; index++) {
    const char = glob[index]!;
    if (char === "*" && glob[index + 1] === "*") {
      if (glob[index + 2] === "/") {
        pattern += "(?:.*/)?";
        index += 2;
      } else {
        pattern += ".*";
        index += 1;
      }
    } else if (char === "*") {
      pattern += "[^/]*";
    } else {
      pattern += char.replace(/[\\^$.*+?()[\]{}|]/g, "\\$&");
    }
  }
  return new RegExp(`${pattern}$`);
}

function checksumHex(buffer: ArrayBuffer): string {
  return [...new Uint8Array(buffer)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}
