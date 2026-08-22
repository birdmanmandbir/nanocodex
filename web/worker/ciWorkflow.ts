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
  EXACT_SOURCE_TREE_PATH,
  cargoVendorBundleKey,
  isCiSourceTree,
  isSha1,
  isSha256,
  rustSecAdvisoryBundleKey,
  sourceArchiveKey,
  sourceTreeFingerprint,
  sourceTreeKey,
  type CiSourceTree,
  type NanocodexCiProviderData,
} from "./ciSource.ts";
import {
  bindingsArtifactCommand,
  bindingsBuildCacheCommand,
  bindingsBuildCacheInputs,
  bindingsResultCacheCommand,
  bindingsResultCacheInputs,
  cargoCacheInputs,
  cargoDependencyCommand,
  dependencyPolicyCacheInputs,
  exactSourceCacheInputs,
  msrvBuildCacheCommand,
  msrvBuildCacheInputs,
  pythonCacheInputs,
  pythonCommand,
  rustBuildCacheInputs,
  rustBuildCacheCommand,
  rustResultCacheCommand,
  rustResultCacheInputs,
  rustQualityCacheInputs,
  rustPipeline,
  typosCommand,
  websiteArtifactCommand,
  websiteDependencyCacheInputs,
  websiteDependencyCommand,
  websiteResultCacheCommand,
  websiteResultCacheInputs,
} from "./ciWorkflowPlan.ts";
import {
  failureMarkerKey,
  terminateActiveSandboxes,
  terminationMarkerKey,
} from "./ciSandboxes.ts";

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
  CARGO_BUILD_JOBS: "4",
  CARGO_INCREMENTAL: "0",
  CARGO_TERM_COLOR: "always",
  RUST_TEST_THREADS: "4",
};

const RUST_TEST_ENV = {
  ...COMMON_ENV,
  // DWARF dominates reusable snapshots but is not consumed by any CI gate.
  // Keep codegen and assertions identical while avoiding multi-gigabyte cache
  // writes, restores, and Clippy metadata builds.
  CARGO_PROFILE_DEV_DEBUG: "0",
  CARGO_PROFILE_TEST_DEBUG: "0",
};

const MSRV_ENV = {
  ...RUST_TEST_ENV,
  CARGO_TARGET_DIR: "/workspace/.cargo-target-msrv",
  RUST_TEST_THREADS: "1",
};

const CI_GATE_NAMES = [
  "Cargo dependencies",
  "Rust build cache",
  "MSRV build cache",
  "stable workspace tests",
  "MSRV workspace tests",
  "quality",
  "dependency policy",
  "typos",
  "static VM guest",
  "Python 3.11",
  "Python 3.14",
  "Bindings build cache",
  "Website dependencies",
  "Node and browser bindings",
  "website",
] as const;

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
    const progress = new CiProgress(this.env.BACKUP_BUCKET, head, pipelineStartedAt);
    await step.do("persist CI running state", EVIDENCE_STEP_CONFIG, async () => {
      await Promise.all([
        this.env.BACKUP_BUCKET.put(
          `runs/${head}/result.json`,
          JSON.stringify({
            version: 1,
            head,
            workflowId: event.instanceId,
            status: "running",
            rustSecRevision: source.rustSecRevision,
            rustSecSha256: source.rustSecSha256,
            startedAt: new Date(pipelineStartedAt).toISOString(),
            steps: [],
          }),
          { httpMetadata: { contentType: "application/json" } },
        ),
        progress.persistInitial(),
        this.env.BACKUP_BUCKET.delete(failureMarkerKey(head)),
      ]);
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
      const cargoStartedAt = progress.start("Cargo dependencies");
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
      ).then(async (metadata) => {
        const summary = {
          name: "cargo dependencies",
          exitCode: dependencies.exitCode,
          cacheHit: metadata.cacheHit,
          durationMs: Date.now() - cargoStartedAt,
        };
        completed.push(summary);
        await progress.complete("Cargo dependencies", summary);
      });

      const runRustJob = async (
        parent: CiContext | CiRunnerResult,
        job: ReturnType<typeof rustPipeline>[number],
        options: {
          cacheInputs?: string[];
        } = {},
      ): Promise<CiRunnerResult> => {
        const { cacheInputs } = options;
        const startedAt = progress.start(job.name);
        const jobEnv = job.name === "MSRV workspace tests"
          ? MSRV_ENV
          : job.name === "stable workspace tests" || job.name === "quality"
            ? RUST_TEST_ENV
            : COMMON_ENV;
        const result = await parent.runner({
          name: job.name,
          command: cacheInputs
            ? job.command
            : cleanupAfter(job.command),
          env: jobEnv,
          ...(cacheInputs ? { cache: { inputs: cacheInputs } } : {}),
          config: runnerConfig(
            job.timeoutMs,
            cacheInputs ? 30 * 24 * 60 * 60 : 24 * 60 * 60,
            0,
            cacheInputs != null,
          ),
        });
        const metadata = await persistRunner(
          this.env.BACKUP_BUCKET,
          head,
          result,
          slug(job.name),
        );
        const summary = {
          name: job.name,
          exitCode: result.exitCode,
          cacheHit: metadata.cacheHit,
          durationMs: Date.now() - startedAt,
        };
        completed.push(summary);
        await progress.complete(job.name, summary);
        return result;
      };
      const rustJobs = rustPipeline({
        url: `${this.env.CI_PUBLIC_ORIGIN.replace(/\/$/, "")}/api/ci/rustsec-advisory-db/${source.rustSecRevision}/bundle.tar.gz`,
        revision: source.rustSecRevision,
        size: source.rustSecSize,
        sha256: source.rustSecSha256,
      });
      const stableJob = rustJobs.find(
        ({ name }) => name === "stable workspace tests",
      );
      if (!stableJob) throw new Error("stable workspace test gate is missing");
      const qualityJob = rustJobs.find(({ name }) => name === "quality");
      if (!qualityJob) throw new Error("quality gate is missing");
      const buildCacheBranch = (async () => {
        const buildCacheStartedAt = progress.start("Rust build cache");
        const buildCache = await dependencies.runner({
          name: "Rust build cache",
          command: rustBuildCacheCommand(),
          env: RUST_TEST_ENV,
          cache: { inputs: rustBuildCacheInputs() },
          config: runnerConfig(45 * 60 * 1_000, 30 * 24 * 60 * 60),
        });
        const buildCachePersistence = persistRunner(
          this.env.BACKUP_BUCKET,
          head,
          buildCache,
          "rust-build-cache",
        ).then(async (metadata) => {
          const summary = {
            name: "Rust build cache",
            exitCode: buildCache.exitCode,
            cacheHit: metadata.cacheHit,
            durationMs: Date.now() - buildCacheStartedAt,
          };
          completed.push(summary);
          await progress.complete("Rust build cache", summary);
        });
        await buildCachePersistence;
        return buildCache;
      })();
      const qualityBranch = (async () => {
        const buildCache = await buildCacheBranch;
        return runRustJob(
          buildCache,
          {
            ...qualityJob,
            command: rustResultCacheCommand(qualityJob.command),
          },
          { cacheInputs: rustQualityCacheInputs() },
        );
      })();
      const msrvJob = rustJobs.find(
        ({ name }) => name === "MSRV workspace tests",
      );
      if (!msrvJob) throw new Error("MSRV workspace test gate is missing");
      const msrvBuildCacheBranch = (async () => {
        const startedAt = progress.start("MSRV build cache");
        const result = await dependencies.runner({
          name: "MSRV build cache",
          command: msrvBuildCacheCommand(),
          env: MSRV_ENV,
          cache: { inputs: msrvBuildCacheInputs() },
          config: runnerConfig(45 * 60 * 1_000, 30 * 24 * 60 * 60),
        });
        const metadata = await persistRunner(
          this.env.BACKUP_BUCKET,
          head,
          result,
          "msrv-build-cache",
        );
        const summary = {
          name: "MSRV build cache",
          exitCode: result.exitCode,
          cacheHit: metadata.cacheHit,
          durationMs: Date.now() - startedAt,
        };
        completed.push(summary);
        await progress.complete("MSRV build cache", summary);
        return result;
      })();
      const directRustJobs = [
        runRustJob(
          ci,
          {
            name: "typos",
            command: cleanupAfter(typosCommand()),
            timeoutMs: 10 * 60 * 1_000,
          },
          { cacheInputs: exactSourceCacheInputs() },
        ),
        ...rustJobs
          .filter(
            ({ name }) =>
              name !== "stable workspace tests" &&
              name !== "MSRV workspace tests" &&
              name !== "quality",
          )
          .map((job) => {
            if (job.name === "static VM guest") {
              return runRustJob(
                dependencies,
                { ...job, command: cleanupAfter(job.command) },
                { cacheInputs: rustQualityCacheInputs() },
              );
            }
            if (job.name === "dependency policy") {
              return runRustJob(
                dependencies,
                { ...job, command: cleanupAfter(job.command) },
                { cacheInputs: dependencyPolicyCacheInputs() },
              );
            }
            return runRustJob(dependencies, job);
          }),
      ];
      const runPythonJobs = () =>
        (["3.11", "3.14"] as const).map(async (version) => {
          const name = `Python ${version}`;
          const startedAt = progress.start(name);
          const result = await dependencies.runner({
            name,
            command: cleanupAfter(pythonCommand(version)),
            env: COMMON_ENV,
            cache: { inputs: pythonCacheInputs() },
            config: runnerConfig(40 * 60 * 1_000, 30 * 24 * 60 * 60),
          });
          const metadata = await persistRunner(
            this.env.BACKUP_BUCKET,
            head,
            result,
            slug(name),
          );
          const summary = {
            name,
            exitCode: result.exitCode,
            cacheHit: metadata.cacheHit,
            durationMs: Date.now() - startedAt,
          };
          completed.push(summary);
          await progress.complete(name, summary);
        });
      const prepareCachedLayer = (
        parent: CiContext | CiRunnerResult,
        name: "Bindings build cache" | "Website dependencies",
        command: string,
        inputs: string[],
      ) => {
        const startedAt = progress.start(name);
        return parent.runner({
          name,
          command,
          env: COMMON_ENV,
          cache: { inputs },
          config: runnerConfig(30 * 60 * 1_000, 30 * 24 * 60 * 60, 1),
        }).then((result) => {
          const persistence = persistRunner(
            this.env.BACKUP_BUCKET,
            head,
            result,
            slug(name),
          ).then(async (metadata) => {
            const summary = {
              name,
              exitCode: result.exitCode,
              cacheHit: metadata.cacheHit,
              durationMs: Date.now() - startedAt,
            };
            completed.push(summary);
            await progress.complete(name, summary);
          });
          // Child gates can outlive this evidence write by minutes. Observe a
          // rejection immediately while preserving it for the pipeline await.
          void persistence.catch(() => undefined);
          return { result, persistence };
        });
      };
      const webPreparation = (async () => {
        const bindingsBuildStatePromise = prepareCachedLayer(
          dependencies,
          "Bindings build cache",
          bindingsBuildCacheCommand(),
          bindingsBuildCacheInputs(),
        );
        const websiteDependencyStatePromise = prepareCachedLayer(
          ci,
          "Website dependencies",
          websiteDependencyCommand(),
          websiteDependencyCacheInputs(),
        );
        // Observe either independent cache preparation failure immediately;
        // the saturation barrier below remains their lifecycle owner.
        void bindingsBuildStatePromise.catch(() => undefined);
        void websiteDependencyStatePromise.catch(() => undefined);
        const [bindingsBuildState, websiteDependencyState] = await Promise.all([
          bindingsBuildStatePromise,
          websiteDependencyStatePromise,
        ]);
        return { bindingsBuildState, websiteDependencyState };
      })();
      const runWebJob = async () => {
        const { bindingsBuildState, websiteDependencyState } =
          await webPreparation;
        const bindingsStartedAt = progress.start("Node and browser bindings");
        const wasmArtifactKey = `runs/${head}/artifacts/web-wasm.tar`;
        const bindingsVerification = await bindingsBuildState.result.runner({
          name: "Node and browser bindings",
          command: bindingsResultCacheCommand(),
          env: COMMON_ENV,
          cache: { inputs: bindingsResultCacheInputs() },
          config: runnerConfig(60 * 60 * 1_000, 30 * 24 * 60 * 60),
        });
        const bindingsVerificationPersistence = persistRunner(
          this.env.BACKUP_BUCKET,
          head,
          bindingsVerification,
          "node-and-browser-bindings",
        );
        void bindingsVerificationPersistence.catch(() => undefined);
        const bindingsArtifact = await bindingsVerification.runner({
          name: "Publish browser artifact",
          command: bindingsArtifactCommand(),
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
          bindingsArtifact.outputs?.[0],
          head,
          wasmArtifactKey,
          "web-wasm",
        );
        artifacts.push(wasmArtifact);
        const bindingsArtifactPersistence = persistRunner(
          this.env.BACKUP_BUCKET,
          head,
          bindingsArtifact,
          "publish-browser-artifact",
        );
        void bindingsArtifactPersistence.catch(() => undefined);
        const wasmContentKey = await retainContentAddressedArtifact(
          this.env.BACKUP_BUCKET,
          wasmArtifact,
          "web-wasm",
        );
        const bindingsPersistence = Promise.all([
          bindingsVerificationPersistence,
          bindingsArtifactPersistence,
        ]).then(async ([metadata]) => {
          const summary = {
            name: "Node and browser bindings",
            exitCode: bindingsArtifact.exitCode,
            cacheHit: metadata.cacheHit,
            durationMs: Date.now() - bindingsStartedAt,
          };
          completed.push(summary);
          await progress.complete("Node and browser bindings", summary);
        });
        const websiteStartedAt = progress.start("website");
        const artifactKey = `runs/${head}/artifacts/web-dist.tar`;
        const websiteVerification = await websiteDependencyState.result.runner({
          name: "website",
          command: websiteResultCacheCommand(
            `${this.env.CI_PUBLIC_ORIGIN.replace(/\/$/, "")}/api/ci/artifacts/${wasmContentKey}`,
            wasmArtifact.size,
            wasmArtifact.sha256,
          ),
          env: COMMON_ENV,
          cache: { inputs: websiteResultCacheInputs() },
          config: runnerConfig(45 * 60 * 1_000, 30 * 24 * 60 * 60),
        });
        const websiteVerificationPersistence = persistRunner(
          this.env.BACKUP_BUCKET,
          head,
          websiteVerification,
          "website",
        );
        void websiteVerificationPersistence.catch(() => undefined);
        const websiteArtifact = await websiteVerification.runner({
          name: "Publish website artifact",
          command: websiteArtifactCommand(),
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
        const websiteArtifactPersistence = persistRunner(
          this.env.BACKUP_BUCKET,
          head,
          websiteArtifact,
          "publish-website-artifact",
        );
        artifacts.push(
          artifactRecord(
            websiteArtifact.outputs?.[0],
            head,
            artifactKey,
            "web-dist",
          ),
        );
        const [websiteMetadata] = await Promise.all([
          websiteVerificationPersistence,
          websiteArtifactPersistence,
          bindingsBuildState.persistence,
          websiteDependencyState.persistence,
          bindingsPersistence,
        ]);
        const summary = {
          name: "website",
          exitCode: websiteArtifact.exitCode,
          cacheHit: websiteMetadata.cacheHit,
          durationMs: Date.now() - websiteStartedAt,
        };
        completed.push(summary);
        await progress.complete("website", summary);
      };
      const saturationBarrier = Promise.all([
        cargoPersistence,
        ...directRustJobs,
        webPreparation,
      ]);
      const [, msrvBuildCache] = await Promise.all([
        qualityBranch,
        msrvBuildCacheBranch,
        saturationBarrier,
      ]);
      // Compilation can saturate the shared host. Finish every reusable target
      // and the compile-heavy quality gate first, then give the stable suite's
      // wall-clock assertions the host without a competing Rust compiler.
      const stableBuildCache = await buildCacheBranch;
      await runRustJob(
        stableBuildCache,
        {
          ...stableJob,
          command: rustResultCacheCommand(stableJob.command),
        },
        {
          cacheInputs: rustResultCacheInputs(),
        },
      );
      // The remaining MSRV and JavaScript suites are bounded to separate cache
      // trees and together fit the host after the stable suite releases it.
      await Promise.all([
        runRustJob(
          msrvBuildCache,
          {
            ...msrvJob,
            command: rustResultCacheCommand(msrvJob.command),
          },
          {
            cacheInputs: rustResultCacheInputs(),
          },
        ),
        runWebJob(),
      ]);
      // The binding gates contain wall-clock SLAs. Run both versions together,
      // but only after compile-heavy gates release their CPU allocations.
      await Promise.all(runPythonJobs());

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
      const operatorTerminated = await this.env.BACKUP_BUCKET.head(
        terminationMarkerKey(head),
      );
      if (!gatesCompleted && !operatorTerminated) {
        await this.env.BACKUP_BUCKET.put(
          failureMarkerKey(head),
          JSON.stringify({
            version: 1,
            head,
            failedAt: new Date().toISOString(),
            failure: failureRecord(cause),
          }),
          {
            onlyIf: { etagDoesNotMatch: "*" },
            httpMetadata: { contentType: "application/json" },
            customMetadata: { kind: "ci-run-failure", head },
          },
        );
        const cleanup = await terminateActiveSandboxes(this.env, head, {
          deleteMarkers: false,
        }).catch((cleanupCause) => ({
          destroyed: [],
          failed: [{
            runnerId: "registry",
            error: failureRecord(cleanupCause).message,
          }],
        }));
        if (cleanup.failed.length > 0) {
          console.error("Failed to stop every CI Sandbox after a gate failure", cleanup);
        }
        await progress.failRunning(cause).catch((progressCause) => {
          console.error("Failed to persist CI gate progress", progressCause);
        });
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
  readonly #trees = new Map<string, Promise<CiSourceTree | null>>();

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
    const tree = await this.#sourceTree(source.sha, data);
    if (!tree) return null;
    const patterns = paths.map(globToRegExp);
    const blobs = tree.files
      .filter((file) => patterns.some((pattern) => pattern.test(file.path)))
      .map(({ path, sha }) => ({ path, sha }));
    if (paths.includes(EXACT_SOURCE_TREE_PATH)) {
      blobs.push({
        path: EXACT_SOURCE_TREE_PATH,
        sha: await sourceTreeFingerprint(tree),
      });
    }
    return blobs;
  }

  #sourceTree(
    head: string,
    data: NanocodexCiProviderData,
  ): Promise<CiSourceTree | null> {
    const retained = this.#trees.get(head);
    if (retained) return retained;
    const pending = (async () => {
      const object = await this.#env.CI_SOURCE.get(data.treeKey);
      if (
        !object ||
        object.customMetadata?.sha256 !== data.treeSha256 ||
        object.checksums.sha256 == null ||
        checksumHex(object.checksums.sha256) !== data.treeSha256
      ) return null;
      const tree: unknown = await object.json().catch(() => undefined);
      return isCiSourceTree(tree, head) ? tree : null;
    })();
    this.#trees.set(head, pending);
    return pending;
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

type CiProgressStep = {
  name: string;
  slug: string;
  status: "pending" | "running" | "success" | "failure" | "terminated";
  startedAt?: string;
  completedAt?: string;
  durationMs?: number;
  exitCode?: number;
  cacheHit?: boolean;
  message?: string;
};

class CiProgress {
  readonly #bucket: R2Bucket;
  readonly #head: string;
  readonly #startedAt: number;
  readonly #steps = new Map<string, CiProgressStep>();
  #writeTail = Promise.resolve();
  #writeError: unknown;

  constructor(bucket: R2Bucket, head: string, startedAt: number) {
    this.#bucket = bucket;
    this.#head = head;
    this.#startedAt = startedAt;
    for (const name of CI_GATE_NAMES) {
      this.#steps.set(name, {
        name,
        slug: slug(name),
        status: "pending",
      });
    }
  }

  persistInitial(): Promise<void> {
    this.#queueSnapshot();
    return this.#flush();
  }

  start(name: string): number {
    const startedAt = Date.now();
    const gate = this.#gate(name);
    gate.status = "running";
    gate.startedAt = new Date(startedAt).toISOString();
    delete gate.completedAt;
    delete gate.durationMs;
    delete gate.exitCode;
    delete gate.cacheHit;
    delete gate.message;
    this.#queueSnapshot();
    return startedAt;
  }

  async complete(
    name: string,
    summary: { exitCode: number; cacheHit: boolean; durationMs: number },
  ): Promise<void> {
    const gate = this.#gate(name);
    gate.status = summary.exitCode === 0 ? "success" : "failure";
    gate.completedAt = new Date().toISOString();
    gate.durationMs = summary.durationMs;
    gate.exitCode = summary.exitCode;
    gate.cacheHit = summary.cacheHit;
    this.#queueSnapshot();
    await this.#flush();
  }

  async failRunning(cause: unknown): Promise<void> {
    const failure = failureRecord(cause);
    const failedGates = isCiRunnerFailure(cause)
      ? new Set([
        cause.runner.name,
        ...cause.diagnostics.failures.map(({ runner }) => runner.name),
      ].map(slug))
      : null;
    const completedAt = Date.now();
    for (const gate of this.#steps.values()) {
      if (gate.status !== "running") continue;
      const failed = failedGates == null || failedGates.has(gate.slug);
      gate.status = failed ? "failure" : "terminated";
      gate.completedAt = new Date(completedAt).toISOString();
      gate.durationMs = gate.startedAt
        ? Math.max(0, completedAt - Date.parse(gate.startedAt))
        : undefined;
      gate.message = failed
        ? failure.message
        : `stopped after ${failure.name} failed`;
    }
    this.#queueSnapshot();
    await this.#flush();
  }

  #gate(name: string): CiProgressStep {
    const gate = this.#steps.get(name);
    if (!gate) throw new Error(`Unknown CI gate: ${name}`);
    return gate;
  }

  #queueSnapshot(): void {
    const body = JSON.stringify({
      version: 1,
      head: this.#head,
      startedAt: new Date(this.#startedAt).toISOString(),
      updatedAt: new Date().toISOString(),
      steps: [...this.#steps.values()],
    });
    this.#writeTail = this.#writeTail
      .then(async () => {
        if (this.#writeError) return;
        const object = await this.#bucket.put(
          `runs/${this.#head}/progress.json`,
          body,
          { httpMetadata: { contentType: "application/json" } },
        );
        if (!object) throw new Error(`Failed to persist CI progress for ${this.#head}`);
      })
      .catch((cause) => {
        this.#writeError ??= cause;
      });
  }

  async #flush(): Promise<void> {
    await this.#writeTail;
    if (this.#writeError) throw this.#writeError;
  }
}

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

async function retainContentAddressedArtifact(
  bucket: R2Bucket,
  artifact: CiArtifact,
  kind: "web-wasm",
): Promise<string> {
  const relativeKey = `${kind}/${artifact.sha256}.tar`;
  const key = `artifacts/${relativeKey}`;
  const existing = await bucket.head(key);
  if (existing) {
    if (matchesContentAddressedArtifact(existing, key, artifact, kind)) return relativeKey;
    throw new Error(`Content-addressed ${kind} artifact conflicts at ${key}`);
  }

  const source = await bucket.get(artifact.key);
  if (
    !source || source.key !== artifact.key || source.size !== artifact.size ||
    source.checksums.sha256 == null ||
    checksumHex(source.checksums.sha256) !== artifact.sha256
  ) {
    await source?.body.cancel();
    throw new Error(`Published ${kind} artifact is invalid at ${artifact.key}`);
  }
  const body = await source.arrayBuffer();
  if (body.byteLength !== artifact.size) {
    throw new Error(`Published ${kind} artifact body is truncated at ${artifact.key}`);
  }
  const created = await bucket.put(key, body, {
    onlyIf: { etagDoesNotMatch: "*" },
    httpMetadata: { contentType: artifact.contentType },
    customMetadata: { kind, sha256: artifact.sha256 },
    sha256: artifact.sha256,
  });
  const retained = created ?? await bucket.head(key);
  if (!matchesContentAddressedArtifact(retained, key, artifact, kind)) {
    throw new Error(`Failed to retain content-addressed ${kind} artifact at ${key}`);
  }
  return relativeKey;
}

function matchesContentAddressedArtifact(
  object: R2Object | null,
  key: string,
  artifact: CiArtifact,
  kind: "web-wasm",
): boolean {
  return object != null && object.key === key && object.size === artifact.size &&
    object.customMetadata?.kind === kind &&
    object.customMetadata?.sha256 === artifact.sha256 &&
    object.checksums.sha256 != null &&
    checksumHex(object.checksums.sha256) === artifact.sha256;
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
      name: value.runner.name,
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
