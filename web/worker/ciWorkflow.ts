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
  ciSourceLane,
  isCiSourcePublication,
  isNanocodexCiProviderData,
  isCiSourceTree,
  isSha1,
  isSha256,
  sourceTreeFingerprint,
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
  npmPreviewVersion,
  pythonCacheInputs,
  pythonCommand,
  rustBuildCacheInputs,
  rustBuildCacheCommand,
  rustResultCacheCommand,
  rustResultCacheInputs,
  rustQualityCacheInputs,
  rustPipeline,
  refreshSourceCommand,
  staticVmCacheInputs,
  typosCommand,
  websiteArtifactCommand,
  websiteDependencyCacheInputs,
  websiteDependencyCommand,
  websiteResultCacheCommand,
  websiteResultCacheInputs,
} from "./ciWorkflowPlan.ts";
import {
  linuxDistributionPlan,
  normalNativeLinuxPlan,
  stableReleaseValidationCommand,
  type DistributionOutput,
} from "./ciDistribution.ts";
import {
  failureMarkerKey,
  terminateActiveSandboxes,
  terminationMarkerKey,
} from "./ciSandboxes.ts";
import {
  CI_MAC_EVENT_TYPE,
  type CiMacCompletionEvent,
  type CiMacJobRecord,
} from "./ciMacJobs.ts";
import {
  type CiPublicationLease,
  promoteCiReleaseAsset,
  type CiReleaseAsset,
  type CiReleaseDraft,
  type CiReleaseKind,
  type CiReleaseStagingFence,
} from "./ciReleases.ts";

type NanocodexSourceProvider = {
  id: "nanocodex-source";
  event: { type: "push" };
  providerData: NanocodexCiProviderData;
};

type NanocodexCiBindings = CiBindings & {
  CI_SOURCE: R2Bucket;
  CI_PUBLIC_ORIGIN: string;
  CI_MACOS_JOBS: DurableObjectNamespace;
  CI_REPOSITORY: DurableObjectNamespace;
  CI_RELEASES: DurableObjectNamespace;
  CI_RELEASE_TOKEN: string;
};

const PUBLICATION_LEASE_ID =
  /^(0|[1-9][0-9]{0,15})\.[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;

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
  "Linux native CLI",
  "macOS stable workspace tests and native CLI",
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
    if (
      event.payload.ref !== source.lane.ref ||
      event.payload.branch !== source.lane.branch
    ) throw new Error("CI event source lane does not match provider data");
    if (source.distribution) {
      await this.distributionPipeline(event, step, ci, source);
      return;
    }
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
      const macGate = "macOS stable workspace tests and native CLI";
      const macStartedAt = progress.start(macGate);
      const macJob = (async () => {
        const expectedJobId = `macos-native-build-${head}`;
        await step.do("queue macOS native build", EVIDENCE_STEP_CONFIG, async () => {
          const response = await ciMacJobs(this.env).fetch("https://ci-macos/jobs", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({
              head,
              workflowId: event.instanceId,
              task: "native-build",
              publishedAt: source.publishedAt,
              source: {
                url: `${this.env.CI_PUBLIC_ORIGIN.replace(/\/$/, "")}/api/ci/source/${head}/archive`,
                size: source.archiveSize,
                sha256: source.archiveSha256,
              },
              cargoVendor: {
                url: ciCargoVendorUrl(this.env.CI_PUBLIC_ORIGIN, source),
                size: source.cargoVendorSize,
                sha256: source.cargoVendorSha256,
              },
            }),
          });
          if (!response.ok) {
            throw new Error(`Failed to queue macOS CI job: ${await response.text()}`);
          }
          const value = await response.json() as { job?: CiMacJobRecord };
          if (value.job?.id !== expectedJobId) {
            throw new Error(`macOS CI queued an invalid job for ${head}`);
          }
          return { jobId: value.job.id };
        });
        const received = await step.waitForEvent<CiMacCompletionEvent>(
          "wait for macOS native build",
          { type: CI_MAC_EVENT_TYPE, timeout: "75 minutes" },
        );
        const result = validateMacCompletion(
          received.payload,
          expectedJobId,
          head,
          event.instanceId,
          "native-build",
        );
        if (result.result.outcome !== "success" || !result.result.asset) {
          throw new Error(
            `macOS native build failed: ${result.result.error ?? `exit ${result.result.exitCode}`}`,
          );
        }
        const artifact = await step.do(
          "publish macOS native CLI",
          EVIDENCE_STEP_CONFIG,
          () => promoteMacNativeArtifact(this.env.BACKUP_BUCKET, head, result),
        );
        artifacts.push(artifact);
        const summary = {
          name: macGate,
          exitCode: result.result.exitCode,
          cacheHit: false,
          durationMs: Date.now() - macStartedAt,
        };
        completed.push(summary);
        await progress.complete(macGate, summary);
      })();
      void macJob.catch(() => undefined);
      const cargoStartedAt = progress.start("Cargo dependencies");
      const dependencies = await ci.runner({
        name: "cargo dependencies",
        command: cargoDependencyCommand(
          ciCargoVendorUrl(this.env.CI_PUBLIC_ORIGIN, source),
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
      const runLinuxNative = async (parent: CiRunnerResult) => {
        const name = "Linux native CLI";
        const startedAt = progress.start(name);
        const plan = normalNativeLinuxPlan({
          testedSha: head,
          publishedAt: source.publishedAt,
        });
        const prepared = await parent.runner({
          name: "Linux native CLI build cache",
          command: [
            refreshSourceCommand(plan.command),
            "find /workspace -mindepth 1 -maxdepth 1 ! -name .ci-output -exec rm -rf -- {} +",
          ].join("\n"),
          env: COMMON_ENV,
          cache: { inputs: [...plan.cacheInputs] },
          config: runnerConfig(90 * 60 * 1_000, 30 * 24 * 60 * 60),
        });
        const cacheMetadata = await persistRunner(
          this.env.BACKUP_BUCKET,
          head,
          prepared,
          "linux-native-cli-build-cache",
        );
        const output = plan.outputs[0];
        const key = `runs/${head}/artifacts/${output.name}`;
        const published = await prepared.runner({
          name: "Publish Linux native CLI",
          command: `test -s ${shellQuote(output.path)} && sha256sum --check ${shellQuote(output.sha256Path)}`,
          env: COMMON_ENV,
          outputs: [{
            path: output.path,
            sha256Path: output.sha256Path,
            key,
            maxBytes: output.maxBytes,
            contentType: output.contentType,
            customMetadata: {
              head,
              kind: "native-cli",
              name: output.name,
              platform: output.platform,
            },
          }],
          config: runnerConfig(15 * 60 * 1_000, 24 * 60 * 60, 0, false),
        });
        const artifact = nativeArtifactRecord(
          published.outputs?.[0],
          head,
          key,
          output.name,
          output.platform,
        );
        artifacts.push(artifact);
        await persistRunner(
          this.env.BACKUP_BUCKET,
          head,
          published,
          "publish-linux-native-cli",
        );
        const summary = {
          name,
          exitCode: published.exitCode,
          cacheHit: cacheMetadata.cacheHit,
          durationMs: Date.now() - startedAt,
        };
        completed.push(summary);
        await progress.complete(name, summary);
      };
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
                { cacheInputs: staticVmCacheInputs() },
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
        const npmArtifactKey = `runs/${head}/artifacts/npm-package.tgz`;
        const npmPreview = source.lane.type === "pull_request"
          ? {
            key: `runs/${head}/artifacts/npm-preview.tgz`,
            mergeHead: head,
            packageVersion: npmPreviewVersion(head),
            pullRequest: source.lane.number,
            pullRequestHead: source.lane.pullRequestHead,
          }
          : undefined;
        const bindingsVerification = await bindingsBuildState.result.runner({
          name: "Node and browser bindings",
          command: bindingsResultCacheCommand(npmPreview?.mergeHead),
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
          command: bindingsArtifactCommand(npmPreview?.mergeHead),
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
            {
              path: "/workspace/.ci-output/npm-package.tgz",
              sha256Path: "/workspace/.ci-output/npm-package.tgz.sha256",
              key: npmArtifactKey,
              maxBytes: 16 * 1024 * 1024,
              contentType: "application/gzip",
              customMetadata: { head, kind: "npm-package" },
            },
            ...(npmPreview
              ? [{
                path: "/workspace/.ci-output/npm-preview.tgz",
                sha256Path: "/workspace/.ci-output/npm-preview.tgz.sha256",
                key: npmPreview.key,
                maxBytes: 16 * 1024 * 1024,
                contentType: "application/gzip",
                customMetadata: {
                  head,
                  kind: "npm-preview",
                  packageVersion: npmPreview.packageVersion,
                  pullRequest: String(npmPreview.pullRequest),
                  pullRequestHead: npmPreview.pullRequestHead,
                },
              }]
              : []),
          ],
          config: runnerConfig(60 * 60 * 1_000, 24 * 60 * 60, 0, false),
        });
        const wasmArtifact = artifactRecord(
          bindingsArtifact.outputs?.[0],
          head,
          wasmArtifactKey,
          "web-wasm",
        );
        const npmArtifact = artifactRecord(
          bindingsArtifact.outputs?.[1],
          head,
          npmArtifactKey,
          "npm-package",
        );
        const previewArtifact = npmPreview
          ? npmPreviewArtifactRecord(bindingsArtifact.outputs?.[2], npmPreview)
          : undefined;
        artifacts.push(
          wasmArtifact,
          npmArtifact,
          ...(previewArtifact ? [previewArtifact] : []),
        );
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
        await retainContentAddressedArtifact(
          this.env.BACKUP_BUCKET,
          npmArtifact,
          "npm-package",
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
        const deploymentArtifact = artifactRecord(
          websiteArtifact.outputs?.[0],
          head,
          artifactKey,
          "web-dist",
        );
        artifacts.push(deploymentArtifact);
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
      await Promise.all([macJob, runLinuxNative(stableBuildCache)]);
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

  private async distributionPipeline(
    event: WorkflowEvent<CiParams<NanocodexSourceProvider>>,
    step: WorkflowStep,
    ci: CiContext,
    source: NanocodexCiProviderData,
  ): Promise<void> {
    const distribution = source.distribution;
    if (!distribution) throw new Error("Distribution request is missing");
    const head = event.payload.sha;
    const kind = distribution.channel === "stable" ? "stable" : "commit";
    const releaseId = distribution.channel === "stable" ? distribution.tagName : head;
    const releaseTag = distribution.channel === "stable"
      ? distribution.tagName
      : `nightly-${head}`;
    const prefix = `distribution/${kind}/${releaseId}`;
    const release = ciReleases(this.env);
    const existing = await release.fetch(
      `https://ci-releases/releases/${kind}/${releaseId}`,
    );
    if (existing.ok) {
      const manifest = await existing.json() as {
        commit?: string;
        finalizedAt?: string;
      };
      if (
        manifest.commit !== head || typeof manifest.finalizedAt !== "string" ||
        !Number.isFinite(Date.parse(manifest.finalizedAt))
      ) {
        throw new Error(`Immutable release ${releaseId} targets another commit`);
      }
      if (distribution.channel === "nightly") {
        const finalizedAt = manifest.finalizedAt;
        const publication = await replayedNightlyPublication(release, head, manifest);
        await step.do("persist distribution success", EVIDENCE_STEP_CONFIG, () =>
          persistDistributionSuccess(
            this.env.BACKUP_BUCKET,
            prefix,
            distribution,
            head,
            event.instanceId,
            publication,
            0,
            finalizedAt,
          ));
      }
      return;
    }
    if (existing.status !== 404) {
      throw new Error(`Failed to inspect immutable release ${releaseId}`);
    }

    const startedAt = Date.now();
    let finalizationMayHaveCommitted = false;
    await step.do("persist distribution running state", EVIDENCE_STEP_CONFIG, async () => {
      await this.env.BACKUP_BUCKET.put(
        `${prefix}/result.json`,
        JSON.stringify({
          version: 1,
          status: "running",
          channel: distribution.channel,
          tagName: distribution.tagName,
          head,
          workflowId: event.instanceId,
          startedAt: new Date(startedAt).toISOString(),
        }),
        { httpMetadata: { contentType: "application/json" } },
      );
    });

    try {
      const dependencies = await ci.runner({
        name: `${distribution.channel} distribution dependencies`,
        command: cargoDependencyCommand(
          ciCargoVendorUrl(this.env.CI_PUBLIC_ORIGIN, source),
          source.cargoVendorSize,
          source.cargoVendorSha256,
        ),
        env: COMMON_ENV,
        cache: { inputs: cargoCacheInputs() },
        config: runnerConfig(20 * 60 * 1_000, 30 * 24 * 60 * 60, 1),
      });
      await persistDistributionRunner(
        this.env.BACKUP_BUCKET,
        prefix,
        dependencies,
        "dependencies",
      );

      let buildParent = dependencies;
      if (distribution.channel === "stable") {
        const validation = await dependencies.runner({
          name: `validate ${distribution.tagName}`,
          command: stableReleaseValidationCommand(distribution.tagName),
          env: COMMON_ENV,
          config: runnerConfig(90 * 60 * 1_000, 24 * 60 * 60),
        });
        await persistDistributionRunner(
          this.env.BACKUP_BUCKET,
          prefix,
          validation,
          "validation",
        );
        buildParent = validation;
      }

      const plan = linuxDistributionPlan({
        channel: distribution.channel,
        tagName: distribution.tagName,
        sha: head,
        buildTimestamp: distribution.buildTimestamp,
      });
      const stageId = await digestHex(
        new TextEncoder().encode(`${event.instanceId}:${kind}:${releaseId}`),
      );
      const stagingComponentPrefix =
        `distribution-staging/${kind}/${releaseId}/${stageId}/components/linux`;
      const stagingKeys = plan.outputs.map((output) =>
        `${stagingComponentPrefix}/${output.name}`
      );
      const stagingFence = await registerReleaseStaging(
        release,
        this.env.CI_RELEASE_TOKEN,
        kind,
        releaseId,
        head,
        stageId,
        stagingKeys,
      );
      const componentPrefix = `${prefix}/components/linux`;
      const linuxPromise = buildParent.runner({
        name: `${distribution.channel} Linux distribution`,
        command: plan.command,
        env: COMMON_ENV,
        outputs: plan.outputs.map((output) => ({
          path: output.path,
          sha256Path: output.sha256Path,
          key: `${stagingComponentPrefix}/${output.name}`,
          maxBytes: output.maxBytes,
          contentType: output.contentType,
          customMetadata: {
            kind: "distribution-staging-component",
            channel: distribution.channel,
            head,
            releaseKind: kind,
            releaseId,
            stageId,
            name: output.name,
          },
        })),
        config: runnerConfig(90 * 60 * 1_000, 24 * 60 * 60, 0, false),
      });
      void linuxPromise.catch(() => undefined);

      const macPromise = (async () => {
        const expectedJobId = `macos-release-build-${event.instanceId}`;
        await step.do(`queue ${distribution.channel} macOS distribution`, EVIDENCE_STEP_CONFIG, async () => {
          const response = await ciMacJobs(this.env).fetch("https://ci-macos/jobs", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({
              head,
              workflowId: event.instanceId,
              task: "release-build",
              source: {
                url: `${this.env.CI_PUBLIC_ORIGIN.replace(/\/$/, "")}/api/ci/source/${head}/archive`,
                size: source.archiveSize,
                sha256: source.archiveSha256,
              },
              cargoVendor: {
                url: ciCargoVendorUrl(this.env.CI_PUBLIC_ORIGIN, source),
                size: source.cargoVendorSize,
                sha256: source.cargoVendorSha256,
              },
              release: {
                channel: distribution.channel,
                tagName: distribution.tagName,
                buildTimestamp: distribution.buildTimestamp,
              },
            }),
          });
          if (!response.ok) {
            throw new Error(`Failed to queue macOS distribution: ${await response.text()}`);
          }
          const value = await response.json() as { job?: CiMacJobRecord };
          if (value.job?.id !== expectedJobId) {
            throw new Error(`macOS distribution queued an invalid job for ${head}`);
          }
          return { jobId: value.job.id };
        });
        const received = await step.waitForEvent<CiMacCompletionEvent>(
          `wait for ${distribution.channel} macOS distribution`,
          { type: CI_MAC_EVENT_TYPE, timeout: "90 minutes" },
        );
        const completion = validateMacCompletion(
          received.payload,
          expectedJobId,
          head,
          event.instanceId,
          "release-build",
        );
        if (completion.result.outcome !== "success" || !completion.result.asset) {
          throw new Error(
            `macOS distribution failed: ${completion.result.error ?? `exit ${completion.result.exitCode}`}`,
          );
        }
        return completion;
      })();
      void macPromise.catch(() => undefined);

      const [linux, mac] = await Promise.all([linuxPromise, macPromise]);
      const macAsset = macReleaseArtifact(head, kind, releaseId, mac);
      await persistDistributionRunner(
        this.env.BACKUP_BUCKET,
        prefix,
        linux,
        "linux",
      );
      await this.env.BACKUP_BUCKET.put(
        `${prefix}/steps/macos/result.json`,
        JSON.stringify({
          version: 1,
          jobId: mac.jobId,
          head: mac.head,
          workflowId: mac.workflowId,
          task: mac.task,
          completedAt: mac.completedAt,
          result: mac.result,
        }),
        { httpMetadata: { contentType: "application/json" } },
      );
      const linuxArtifacts = plan.outputs
        .map((output, index) => ({ output, persisted: linux.outputs?.[index] }))
        .filter(({ output }) => output.kind === "cli" || output.kind === "vm-guest")
        .map(({ output, persisted }) => ({
          output,
          persisted,
          asset: distributionAsset(
            persisted,
            output,
            `${stagingComponentPrefix}/${output.name}`,
            `${componentPrefix}/${output.name}`,
          ),
        }));
      const linuxAssets = linuxArtifacts.map(({ asset }) => asset);
      const assets: CiReleaseAsset[] = [
        ...linuxAssets,
        {
          name: macAsset.name,
          platform: macAsset.platform,
          key: macAsset.key,
          size: macAsset.size,
          sha256: macAsset.sha256,
          contentType: macAsset.contentType,
        },
      ];
      const npmName = distribution.channel === "stable"
        ? `nanocodex-${distribution.tagName.slice(1)}.tgz`
        : `nanocodex-${head.slice(0, 10)}.tgz`;
      const npm = await describeNpmReleaseArtifact(
        this.env.BACKUP_BUCKET,
        head,
        kind,
        releaseId,
        npmName,
      );
      assets.push(npm);
      assets.sort((left, right) => left.name.localeCompare(right.name));

      const provenanceBody = new TextEncoder().encode(JSON.stringify({
        version: 1,
        builder: "nanocodex-cloudflare-ci",
        channel: distribution.channel,
        tagName: distribution.tagName,
        sourceSha: head,
        buildTimestamp: distribution.buildTimestamp,
        linux: plan.provenance,
        macos: {
          builder: "authenticated-external-macos-runner",
          asset: {
            name: macAsset.name,
            size: macAsset.size,
            sha256: macAsset.sha256,
          },
        },
      }));
      const provenanceAsset = await releaseObjectDescriptor(
        `${prefix}/PROVENANCE.json`,
        provenanceBody,
        "PROVENANCE.json",
        "linux",
        "application/json",
      );
      assets.push(provenanceAsset);
      assets.sort((left, right) => left.name.localeCompare(right.name));

      const checksums = assets
        .map(({ sha256, name }) => `${sha256}  ${name}\n`)
        .join("");
      const checksumBody = new TextEncoder().encode(checksums);
      const checksumAsset = await releaseObjectDescriptor(
        `${prefix}/SHA256SUMS`,
        checksumBody,
        "SHA256SUMS",
        "linux",
        "text/plain; charset=utf-8",
      );
      assets.push(checksumAsset);
      assets.sort((left, right) => left.name.localeCompare(right.name));

      const stageDraft = async () => {
        const expectedChannel = await releaseChannelId(
          release,
          distribution.channel === "stable" ? "latest" : "nightly",
        );
        const draft: CiReleaseDraft = {
          version: 1,
          kind,
          tag: releaseTag,
          commit: head,
          channel: distribution.channel === "stable" ? "latest" : "nightly",
          expectedChannel,
          assets,
        };
        const response = await release.fetch(
          `https://ci-releases/drafts/${kind}/${releaseId}`,
          {
            method: "PUT",
            headers: {
              authorization: `Bearer ${this.env.CI_RELEASE_TOKEN}`,
              "content-type": "application/json",
            },
            body: JSON.stringify(draft),
          },
        );
        if (!response.ok) {
          throw new Error(`Failed to stage release ${releaseId}: ${await response.text()}`);
        }
        return response;
      };

      // The exact draft owns every immutable final key before the first final
      // copy. Linux outputs remain under a separately registered, bounded
      // staging record until all final objects have been reconciled.
      const owningDraft = await stageDraft();
      await owningDraft.body?.cancel().catch(() => undefined);
      await step.do(
        `publish ${distribution.channel} immutable distribution assets`,
        EVIDENCE_STEP_CONFIG,
        async () => {
          const promotedLinux = await Promise.all(linuxArtifacts.map(
            ({ output, persisted, asset }) =>
              promoteStagedLinuxReleaseArtifact(
                this.env.BACKUP_BUCKET,
                head,
                kind,
                releaseId,
                stageId,
                output,
                persisted,
                asset,
              ),
          ));
          const promotedMac = await promoteMacReleaseArtifact(
            this.env.BACKUP_BUCKET,
            head,
            kind,
            releaseId,
            mac,
          );
          const promotedNpm = await promoteNpmReleaseArtifact(
            this.env.BACKUP_BUCKET,
            head,
            kind,
            releaseId,
            npmName,
          );
          const retainedProvenance = await retainReleaseObject(
            this.env.BACKUP_BUCKET,
            provenanceAsset.key,
            provenanceBody,
            provenanceAsset.name,
            provenanceAsset.platform,
            provenanceAsset.contentType,
          );
          const retainedChecksums = await retainReleaseObject(
            this.env.BACKUP_BUCKET,
            checksumAsset.key,
            checksumBody,
            checksumAsset.name,
            checksumAsset.platform,
            checksumAsset.contentType,
          );
          const reconciled = [
            ...promotedLinux,
            promotedMac,
            promotedNpm,
            retainedProvenance,
            retainedChecksums,
          ].sort((left, right) => left.name.localeCompare(right.name));
          if (JSON.stringify(reconciled) !== JSON.stringify(assets)) {
            throw new Error(`Immutable distribution assets changed for ${releaseId}`);
          }
          return { assets: reconciled };
        },
      );
      await deleteReleaseStaging(
        release,
        this.env.CI_RELEASE_TOKEN,
        kind,
        releaseId,
        head,
        stageId,
        stagingFence,
      );

      if (distribution.channel === "stable") {
        const draftResponse = await stageDraft();
        const staged = await draftResponse.json();
        await step.do("persist stable distribution ready state", EVIDENCE_STEP_CONFIG, async () => {
          await this.env.BACKUP_BUCKET.put(
            `${prefix}/result.json`,
            JSON.stringify({
              version: 1,
              status: "ready",
              channel: distribution.channel,
              tagName: distribution.tagName,
              head,
              workflowId: event.instanceId,
              durationMs: Date.now() - startedAt,
              completedAt: new Date().toISOString(),
              staged,
            }),
            { httpMetadata: { contentType: "application/json" } },
          );
        });
        return;
      }

      // Refresh only the unpublished channel CAS before taking the global
      // publication lease; the immutable object inventory is already owned.
      const retainedDraft = await stageDraft();
      await retainedDraft.body?.cancel().catch(() => undefined);
      await requireCurrentMaster(this.env.CI_REPOSITORY, head);
      const publicationLease = await acquireNightlyPublicationLease(
        release,
        this.env.CI_RELEASE_TOKEN,
        head,
        `workflow:${event.instanceId}`,
      );
      let publication: unknown;
      try {
        await requireCurrentMaster(this.env.CI_REPOSITORY, head);
        const draftResponse = await stageDraft();
        await draftResponse.body?.cancel().catch(() => undefined);
        await requireCurrentMaster(this.env.CI_REPOSITORY, head);
        let finalized: Response;
        try {
          finalized = await release.fetch(
            `https://ci-releases/drafts/${kind}/${releaseId}/finalize`,
            {
              method: "POST",
              headers: {
                authorization: `Bearer ${this.env.CI_RELEASE_TOKEN}`,
                ...publicationLeaseHeaders(publicationLease),
              },
            },
          );
        } catch (finalizeCause) {
          finalizationMayHaveCommitted = true;
          throw finalizeCause;
        }
        if (!finalized.ok) {
          throw new Error(`Failed to finalize release ${releaseId}: ${await finalized.text()}`);
        }
        finalizationMayHaveCommitted = true;
        publication = await finalized.json();
      } finally {
        await releaseNightlyPublicationLease(
          release,
          this.env.CI_RELEASE_TOKEN,
          publicationLease,
        );
      }
      await step.do("persist distribution success", EVIDENCE_STEP_CONFIG, async () => {
        await persistDistributionSuccess(
          this.env.BACKUP_BUCKET,
          prefix,
          distribution,
          head,
          event.instanceId,
          publication,
          Date.now() - startedAt,
          new Date().toISOString(),
        );
      });
    } catch (cause) {
      if (distribution.channel === "nightly" && finalizationMayHaveCommitted) {
        const finalized = await release.fetch(
          `https://ci-releases/releases/${kind}/${releaseId}`,
        ).catch(() => undefined);
        if (finalized?.ok) {
          const manifest = await finalized.json().catch(() => undefined) as {
            commit?: string;
            finalizedAt?: string;
          } | undefined;
          if (
            manifest?.commit === head && typeof manifest.finalizedAt === "string" &&
            Number.isFinite(Date.parse(manifest.finalizedAt))
          ) {
            const completedAt = manifest.finalizedAt;
            const publication = await replayedNightlyPublication(release, head, manifest);
            await step.do(
              "reconcile finalized distribution success",
              EVIDENCE_STEP_CONFIG,
              () =>
                persistDistributionSuccess(
                  this.env.BACKUP_BUCKET,
                  prefix,
                  distribution,
                  head,
                  event.instanceId,
                  publication,
                  Date.now() - startedAt,
                  completedAt,
                ),
            );
            return;
          }
        }
        // An ambiguous or acknowledged-success finalize may already have
        // committed. Leave the prior running evidence intact so a restart or
        // current-channel dispatch can reconcile success; a failure record
        // must never overwrite it.
        throw cause;
      }
      await step.do("persist distribution failure", EVIDENCE_STEP_CONFIG, async () => {
        await this.env.BACKUP_BUCKET.put(
          `${prefix}/result.json`,
          JSON.stringify({
            version: 1,
            status: "failure",
            channel: distribution.channel,
            tagName: distribution.tagName,
            head,
            workflowId: event.instanceId,
            durationMs: Date.now() - startedAt,
            completedAt: new Date().toISOString(),
            failure: failureRecord(cause),
          }),
          { httpMetadata: { contentType: "application/json" } },
        );
      });
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
  if (!isNanocodexCiProviderData(value, head)) {
    throw new Error("CI provider data is invalid");
  }
  return value;
}

function ciCargoVendorUrl(
  origin: string,
  source: NanocodexCiProviderData,
): string {
  return `${origin.replace(/\/$/, "")}/api/ci/${source.cargoVendorKey}`;
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
  kind?: "native-cli" | "npm-preview";
  name?: string;
  platform?: string;
  packageVersion?: string;
  pullRequest?: number;
  pullRequestHead?: string;
};

type CiArtifactKind = "web-wasm" | "web-dist" | "npm-package" | "npm-preview";

type NpmPreviewIdentity = {
  key: string;
  mergeHead: string;
  packageVersion: string;
  pullRequest: number;
  pullRequestHead: string;
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
  kind: CiArtifactKind,
): CiArtifact {
  const sha256 = output?.sha256;
  const expectedContentType = kind === "npm-package" || kind === "npm-preview"
    ? "application/gzip"
    : "application/x-tar";
  if (
    output == null ||
    output.key !== key ||
    output.size <= 0 ||
    !isSha256(sha256) ||
    output.contentType !== expectedContentType
  )
    throw new Error(`${kind} artifact is invalid for ${head}`);
  return { key, size: output.size, sha256, contentType: output.contentType };
}

function nativeArtifactRecord(
  output: NonNullable<CiRunnerResult["outputs"]>[number] | undefined,
  head: string,
  key: string,
  name: "nanocodex-x86_64-unknown-linux-gnu" | "nanocodex-aarch64-apple-darwin",
  platform: "x86_64-unknown-linux-gnu" | "aarch64-apple-darwin",
): CiArtifact {
  if (
    output == null || output.key !== key || output.size <= 0 ||
    output.size > 128 * 1024 * 1024 || !isSha256(output.sha256) ||
    output.contentType !== "application/octet-stream" ||
    key !== `runs/${head}/artifacts/${name}` || name !== `nanocodex-${platform}`
  ) throw new Error(`native CLI artifact ${name} is invalid for ${head}`);
  return {
    key,
    size: output.size,
    sha256: output.sha256,
    contentType: output.contentType,
    kind: "native-cli",
    name,
    platform,
  };
}

function npmPreviewArtifactRecord(
  output: NonNullable<CiRunnerResult["outputs"]>[number] | undefined,
  identity: NpmPreviewIdentity,
): CiArtifact {
  if (
    output == null || output.key !== identity.key || output.size <= 0 ||
    output.size > 16 * 1024 * 1024 || !isSha256(output.sha256) ||
    output.contentType !== "application/gzip" ||
    identity.key !== `runs/${identity.mergeHead}/artifacts/npm-preview.tgz` ||
    !isSha1(identity.mergeHead) || !isSha1(identity.pullRequestHead) ||
    !Number.isSafeInteger(identity.pullRequest) || identity.pullRequest <= 0 ||
    identity.packageVersion !== npmPreviewVersion(identity.mergeHead)
  ) throw new Error(`npm preview artifact is invalid for ${identity.mergeHead}`);
  return {
    key: identity.key,
    size: output.size,
    sha256: output.sha256,
    contentType: output.contentType,
    kind: "npm-preview",
    packageVersion: identity.packageVersion,
    pullRequest: identity.pullRequest,
    pullRequestHead: identity.pullRequestHead,
  };
}

async function retainContentAddressedArtifact(
  bucket: R2Bucket,
  artifact: CiArtifact,
  kind: "web-wasm" | "npm-package",
): Promise<string> {
  const extension = kind === "npm-package" ? "tgz" : "tar";
  const relativeKey = `${kind}/${artifact.sha256}.${extension}`;
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
  kind: "web-wasm" | "npm-package",
): boolean {
  return object != null && object.key === key && object.size === artifact.size &&
    object.customMetadata?.kind === kind &&
    object.customMetadata?.sha256 === artifact.sha256 &&
    object.checksums.sha256 != null &&
    checksumHex(object.checksums.sha256) === artifact.sha256;
}

function ciReleases(env: Pick<NanocodexCiBindings, "CI_RELEASES">) {
  return env.CI_RELEASES.get(env.CI_RELEASES.idFromName("nanocodex"));
}

async function replayedNightlyPublication(
  release: DurableObjectStub,
  head: string,
  manifest: { commit?: string; finalizedAt?: string },
): Promise<{ manifest: unknown; pointer: unknown | null }> {
  const response = await release.fetch("https://ci-releases/channels/nightly");
  if (!response.ok) {
    throw new Error(`Failed to reconcile finalized nightly release ${head}`);
  }
  const channel = await response.json().catch(() => undefined) as {
    manifest?: unknown;
    pointer?: { commit?: unknown };
  } | undefined;
  if (channel?.pointer?.commit === head) {
    if (JSON.stringify(channel.manifest) !== JSON.stringify(manifest)) {
      throw new Error(`Finalized nightly release ${head} does not match its channel`);
    }
    return { manifest, pointer: channel.pointer };
  }
  return { manifest, pointer: null };
}

async function persistDistributionSuccess(
  bucket: R2Bucket,
  prefix: string,
  distribution: NonNullable<NanocodexCiProviderData["distribution"]>,
  head: string,
  workflowId: string,
  publication: unknown,
  durationMs: number,
  completedAt: string,
): Promise<void> {
  await bucket.put(
    `${prefix}/result.json`,
    JSON.stringify({
      version: 1,
      status: "success",
      channel: distribution.channel,
      tagName: distribution.tagName,
      head,
      workflowId,
      durationMs,
      completedAt,
      publication,
    }),
    { httpMetadata: { contentType: "application/json" } },
  );
}

async function requireCurrentMaster(
  repository: DurableObjectNamespace,
  expectedHead: string,
): Promise<void> {
  const response = await repository.get(repository.idFromName("nanocodex")).fetch(
    "https://ci-repository/state",
  );
  if (!response.ok) {
    throw new Error(`Failed to recheck authoritative master for ${expectedHead}`);
  }
  const value = await response.json().catch(() => undefined) as {
    publication?: unknown;
    run?: unknown;
  } | undefined;
  const publication = value?.publication;
  const run = value?.run as Record<string, unknown> | undefined;
  if (
    !isCiSourcePublication(publication) || ciSourceLane(publication).type !== "master" ||
    !run || run.version !== 1 || run.head !== publication.head ||
    run.workflowId !== `ci-${publication.head}` || run.state !== "dispatched"
  ) throw new Error("Authoritative master state is invalid");
  if (publication.head !== expectedHead) {
    throw new Error(`Nightly release head ${expectedHead} is no longer current master`);
  }
}

async function acquireNightlyPublicationLease(
  release: DurableObjectStub,
  token: string,
  head: string,
  owner: string,
): Promise<CiPublicationLease> {
  const response = await release.fetch("https://ci-releases/publication-lease/acquire", {
    method: "POST",
    headers: {
      authorization: `Bearer ${token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({ owner, kind: "commit", id: head, commit: head }),
  });
  if (!response.ok) {
    throw new Error(`Failed to acquire nightly publication lease: ${await response.text()}`);
  }
  const value = await response.json().catch(() => undefined) as Partial<CiPublicationLease>;
  const expiresAt = Date.parse(value?.expiresAt ?? "");
  if (
    !value || value.version !== 1 || value.kind !== "commit" || value.id !== head ||
    value.commit !== head || value.owner !== owner || typeof value.leaseId !== "string" ||
    !PUBLICATION_LEASE_ID.test(value.leaseId) || !Number.isSafeInteger(value.generation) ||
    (value.generation ?? 0) <= 0 || !value.leaseId.startsWith(`${value.generation}.`) ||
    !Number.isFinite(expiresAt) || expiresAt <= Date.now()
  ) throw new Error("Nightly publication lease is invalid");
  return value as CiPublicationLease;
}

function publicationLeaseHeaders(lease: CiPublicationLease): Record<string, string> {
  return {
    "x-nanocodex-publication-lease-id": lease.leaseId,
    "x-nanocodex-publication-lease-owner": lease.owner,
    "x-nanocodex-publication-lease-generation": String(lease.generation),
  };
}

async function releaseNightlyPublicationLease(
  release: DurableObjectStub,
  token: string,
  lease: CiPublicationLease,
): Promise<void> {
  const response = await release.fetch(
    `https://ci-releases/publication-lease/${encodeURIComponent(lease.leaseId)}`,
    {
      method: "DELETE",
      headers: {
        authorization: `Bearer ${token}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({ owner: lease.owner }),
    },
  );
  if (!response.ok) {
    throw new Error(`Failed to release nightly publication lease: ${await response.text()}`);
  }
  await response.body?.cancel().catch(() => undefined);
}

async function registerReleaseStaging(
  release: DurableObjectStub,
  token: string,
  kind: CiReleaseKind,
  releaseId: string,
  commit: string,
  stageId: string,
  keys: string[],
): Promise<CiReleaseStagingFence> {
  const response = await release.fetch(
    `https://ci-releases/staging/${kind}/${releaseId}/${stageId}`,
    {
      method: "PUT",
      headers: {
        authorization: `Bearer ${token}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({ version: 1, commit, keys }),
    },
  );
  if (!response.ok) {
    throw new Error(`Failed to register release staging: ${await response.text()}`);
  }
  const value: unknown = await response.json().catch(() => undefined);
  const fence = value && typeof value === "object" && !Array.isArray(value)
    ? (value as { fence?: unknown }).fence
    : undefined;
  if (
    !fence || typeof fence !== "object" || Array.isArray(fence) ||
    Object.keys(fence).sort().join(",") !== "fenceId,generation" ||
    typeof (fence as { fenceId?: unknown }).fenceId !== "string" ||
    !PUBLICATION_LEASE_ID.test((fence as { fenceId: string }).fenceId) ||
    typeof (fence as { generation?: unknown }).generation !== "number" ||
    !Number.isSafeInteger((fence as { generation: number }).generation) ||
    (fence as { generation: number }).generation <= 0 ||
    (fence as { fenceId: string }).fenceId.split(".", 1)[0] !==
      String((fence as { generation: number }).generation)
  ) throw new Error("Release staging returned an invalid cleanup fence");
  return fence as CiReleaseStagingFence;
}

async function deleteReleaseStaging(
  release: DurableObjectStub,
  token: string,
  kind: CiReleaseKind,
  releaseId: string,
  commit: string,
  stageId: string,
  fence: CiReleaseStagingFence,
): Promise<void> {
  const response = await release.fetch(
    `https://ci-releases/staging/${kind}/${releaseId}/${stageId}`,
    {
      method: "DELETE",
      headers: {
        authorization: `Bearer ${token}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({ commit, ...fence }),
    },
  );
  if (!response.ok) {
    throw new Error(`Failed to collect release staging: ${await response.text()}`);
  }
  await response.body?.cancel().catch(() => undefined);
}

function distributionAsset(
  output: NonNullable<CiRunnerResult["outputs"]>[number] | undefined,
  expected: DistributionOutput,
  sourceKey: string,
  key: string,
): CiReleaseAsset {
  if (
    !output || output.key !== sourceKey || output.size <= 0 ||
    output.size > expected.maxBytes || !isSha256(output.sha256) ||
    output.contentType !== expected.contentType
  ) throw new Error(`Distribution artifact ${expected.name} is invalid`);
  return {
    name: expected.name,
    platform: expected.platform,
    key,
    size: output.size,
    sha256: output.sha256,
    contentType: output.contentType,
  };
}

async function promoteStagedLinuxReleaseArtifact(
  bucket: R2Bucket,
  head: string,
  kind: CiReleaseKind,
  releaseId: string,
  stageId: string,
  output: DistributionOutput,
  persisted: NonNullable<CiRunnerResult["outputs"]>[number] | undefined,
  asset: CiReleaseAsset,
): Promise<CiReleaseAsset> {
  const sourceKey =
    `distribution-staging/${kind}/${releaseId}/${stageId}/components/linux/${output.name}`;
  if (
    !persisted || persisted.key !== sourceKey || persisted.size !== asset.size ||
    persisted.sha256 !== asset.sha256 || persisted.contentType !== asset.contentType
  ) throw new Error(`Staged Linux distribution artifact ${output.name} changed`);
  return promoteCiReleaseAsset(bucket, {
    kind,
    id: releaseId,
    commit: head,
    component: "linux",
    source: {
      key: sourceKey,
      size: asset.size,
      sha256: asset.sha256,
      contentType: asset.contentType,
      customMetadata: {
        kind: "distribution-staging-component",
        channel: kind === "stable" ? "stable" : "nightly",
        head,
        releaseKind: kind,
        releaseId,
        stageId,
        name: output.name,
      },
    },
    asset,
  });
}

async function promoteNpmReleaseArtifact(
  bucket: R2Bucket,
  head: string,
  kind: CiReleaseKind,
  releaseId: string,
  name: string,
): Promise<CiReleaseAsset> {
  const asset = await describeNpmReleaseArtifact(bucket, head, kind, releaseId, name);
  const sourceKey = `runs/${head}/artifacts/npm-package.tgz`;
  return promoteCiReleaseAsset(bucket, {
    kind,
    id: releaseId,
    commit: head,
    component: "npm",
    source: {
      key: sourceKey,
      size: asset.size,
      sha256: asset.sha256,
      contentType: "application/gzip",
      customMetadata: { head, kind: "npm-package", sha256: asset.sha256 },
    },
    asset,
  });
}

async function describeNpmReleaseArtifact(
  bucket: R2Bucket,
  head: string,
  kind: CiReleaseKind,
  releaseId: string,
  name: string,
): Promise<CiReleaseAsset> {
  const sourceKey = `runs/${head}/artifacts/npm-package.tgz`;
  const key = `distribution/${kind}/${releaseId}/components/npm/${name}`;
  const retained = await bucket.head(key);
  if (retained) {
    const replayed = retainedNpmReleaseAsset(
      retained,
      key,
      sourceKey,
      head,
      kind,
      releaseId,
      name,
    );
    if (replayed) return replayed;
    throw new Error(`Immutable npm distribution asset conflicts at ${key}`);
  }

  const source = await bucket.head(sourceKey);
  const sha256 = source?.customMetadata?.sha256;
  if (
    !source || source.key !== sourceKey || source.size <= 0 ||
    source.size > 16 * 1024 * 1024 || !isSha256(sha256) ||
    source.httpMetadata?.contentType !== "application/gzip" ||
    source.customMetadata?.head !== head ||
    source.customMetadata?.kind !== "npm-package" ||
    source.checksums.sha256 == null || checksumHex(source.checksums.sha256) !== sha256
  ) throw new Error(`Tested npm package is unavailable for ${head}`);
  const asset: CiReleaseAsset = {
    name,
    platform: "npm",
    key,
    size: source.size,
    sha256,
    contentType: "application/gzip",
  };
  return asset;
}

function retainedNpmReleaseAsset(
  object: R2Object,
  key: string,
  sourceKey: string,
  head: string,
  kind: CiReleaseKind,
  releaseId: string,
  name: string,
): CiReleaseAsset | undefined {
  const sha256 = object.customMetadata?.sha256;
  if (
    object.key !== key || object.size <= 0 || object.size > 16 * 1024 * 1024 ||
    !isSha256(sha256) || object.httpMetadata?.contentType !== "application/gzip" ||
    object.customMetadata?.kind !== "distribution-component" ||
    object.customMetadata?.releaseKind !== kind ||
    object.customMetadata?.releaseId !== releaseId ||
    object.customMetadata?.commit !== head ||
    object.customMetadata?.component !== "npm" ||
    object.customMetadata?.sourceKey !== sourceKey ||
    object.customMetadata?.name !== name ||
    object.customMetadata?.platform !== "npm" ||
    object.checksums.sha256 == null || checksumHex(object.checksums.sha256) !== sha256
  ) return undefined;
  return {
    name,
    platform: "npm",
    key,
    size: object.size,
    sha256,
    contentType: "application/gzip",
  };
}

async function retainReleaseObject(
  bucket: R2Bucket,
  key: string,
  body: Uint8Array,
  name: string,
  platform: CiReleaseAsset["platform"],
  contentType: string,
): Promise<CiReleaseAsset> {
  const asset = await releaseObjectDescriptor(
    key,
    body,
    name,
    platform,
    contentType,
  );
  const { sha256 } = asset;
  const existing = await bucket.head(key);
  if (existing) {
    if (
      existing.key !== key || existing.size !== body.byteLength ||
      existing.httpMetadata?.contentType !== contentType ||
      existing.checksums.sha256 == null || checksumHex(existing.checksums.sha256) !== sha256
    ) throw new Error(`Immutable release metadata conflicts at ${key}`);
  } else {
    const stored = await bucket.put(key, body, {
      onlyIf: { etagDoesNotMatch: "*" },
      sha256,
      httpMetadata: { contentType },
      customMetadata: { kind: "release-metadata", name, sha256 },
    });
    const retained = stored ?? await bucket.head(key);
    if (
      !retained || retained.size !== body.byteLength || retained.checksums.sha256 == null ||
      checksumHex(retained.checksums.sha256) !== sha256
    ) throw new Error(`Failed to retain release metadata at ${key}`);
  }
  return asset;
}

async function releaseObjectDescriptor(
  key: string,
  body: Uint8Array,
  name: string,
  platform: CiReleaseAsset["platform"],
  contentType: string,
): Promise<CiReleaseAsset> {
  return {
    name,
    platform,
    key,
    size: body.byteLength,
    sha256: await digestHex(body),
    contentType,
  };
}

async function releaseChannelId(
  release: DurableObjectStub,
  channel: "latest" | "nightly",
): Promise<string | null> {
  const response = await release.fetch(`https://ci-releases/channels/${channel}`);
  if (response.status === 404) return null;
  if (!response.ok) throw new Error(`Failed to read release channel ${channel}`);
  const value = await response.json() as { pointer?: { id?: unknown } };
  if (typeof value.pointer?.id !== "string") {
    throw new Error(`Release channel ${channel} returned an invalid pointer`);
  }
  return value.pointer.id;
}

async function persistDistributionRunner(
  bucket: R2Bucket,
  prefix: string,
  result: CiRunnerResult,
  name: string,
): Promise<void> {
  const stepPrefix = `${prefix}/steps/${name}`;
  const [stdout, stderr] = result.persistedLogs == null
    ? await Promise.all([
      persistLog(bucket, `${stepPrefix}/stdout.log`, result.logs.stdout),
      persistLog(bucket, `${stepPrefix}/stderr.log`, result.logs.stderr),
    ])
    : [result.persistedLogs.stdout, result.persistedLogs.stderr];
  await bucket.put(`${stepPrefix}/result.json`, JSON.stringify({
    version: 1,
    exitCode: result.exitCode,
    timedOut: result.timedOut,
    stdout,
    stderr,
    logMetadata: result.logMetadata,
    outputs: result.outputs ?? [],
  }), { httpMetadata: { contentType: "application/json" } });
}

async function digestHex(value: Uint8Array): Promise<string> {
  const bytes = new Uint8Array(value.byteLength);
  bytes.set(value);
  return checksumHex(await crypto.subtle.digest("SHA-256", bytes.buffer));
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

function ciMacJobs(env: Pick<NanocodexCiBindings, "CI_MACOS_JOBS">) {
  return env.CI_MACOS_JOBS.get(env.CI_MACOS_JOBS.idFromName("nanocodex"));
}

function validateMacCompletion(
  value: unknown,
  jobId: string,
  head: string,
  workflowId: string,
  task: "workspace-test" | "native-build" | "release-build",
): CiMacCompletionEvent {
  if (value == null || typeof value !== "object") {
    throw new Error("macOS CI returned an invalid completion event");
  }
  const event = value as Partial<CiMacCompletionEvent>;
  const result = event.result;
  if (
    event.version !== 1 || event.jobId !== jobId || event.head !== head ||
    event.workflowId !== workflowId || event.task !== task ||
    typeof event.completedAt !== "string" || !Number.isFinite(Date.parse(event.completedAt)) ||
    !result || (result.outcome !== "success" && result.outcome !== "failure") ||
    !Number.isSafeInteger(result.exitCode) ||
    (result.outcome === "success" ? result.exitCode !== 0 : result.exitCode === 0) ||
    !result.logs?.stdout || !result.logs.stderr ||
    (task === "workspace-test" && result.asset != null) ||
    (task !== "workspace-test" && result.outcome === "success" &&
      !isMacNativeAsset(result.asset, jobId)) ||
    (result.outcome === "failure" && result.asset != null)
  ) {
    throw new Error(`macOS CI returned an invalid completion event for ${head}`);
  }
  return event as CiMacCompletionEvent;
}

function isMacNativeAsset(
  value: unknown,
  jobId: string,
): value is NonNullable<CiMacCompletionEvent["result"]["asset"]> {
  if (value == null || typeof value !== "object") return false;
  const asset = value as Record<string, unknown>;
  const name = "nanocodex-aarch64-apple-darwin";
  return asset.name === name && asset.platform === "aarch64-apple-darwin" &&
    typeof asset.key === "string" &&
    new RegExp(
      `^macos/jobs/${jobId}/attempts/[0-9a-f-]{36}/assets/${name}$`,
    ).test(asset.key) &&
    typeof asset.size === "number" && Number.isSafeInteger(asset.size) &&
    asset.size > 0 && asset.size <= 128 * 1024 * 1024 &&
    typeof asset.sha256 === "string" && isSha256(asset.sha256) &&
    asset.contentType === "application/octet-stream";
}

async function promoteMacNativeArtifact(
  bucket: R2Bucket,
  head: string,
  completion: CiMacCompletionEvent,
): Promise<CiArtifact> {
  const sourceAsset = completion.result.asset;
  const name = "nanocodex-aarch64-apple-darwin" as const;
  const platform = "aarch64-apple-darwin" as const;
  if (!sourceAsset || !isMacNativeAsset(sourceAsset, completion.jobId)) {
    throw new Error(`macOS native CLI source is invalid for ${head}`);
  }
  const key = `runs/${head}/artifacts/${name}`;
  const artifact: CiArtifact = {
    key,
    size: sourceAsset.size,
    sha256: sourceAsset.sha256,
    contentType: sourceAsset.contentType,
    kind: "native-cli",
    name,
    platform,
  };
  const existing = await bucket.head(key);
  if (existing) {
    if (matchesNativeArtifact(existing, artifact, head)) return artifact;
    throw new Error(`macOS native CLI conflicts at ${key}`);
  }
  const source = await bucket.get(sourceAsset.key);
  if (
    !source || source.key !== sourceAsset.key || source.size !== sourceAsset.size ||
    source.customMetadata?.sha256 !== sourceAsset.sha256 ||
    source.customMetadata?.platform !== platform || source.checksums.sha256 == null ||
    checksumHex(source.checksums.sha256) !== sourceAsset.sha256
  ) {
    await source?.body.cancel();
    throw new Error(`macOS native CLI source is missing or invalid for ${head}`);
  }
  const created = await bucket.put(key, source.body, {
    onlyIf: { etagDoesNotMatch: "*" },
    sha256: sourceAsset.sha256,
    httpMetadata: { contentType: sourceAsset.contentType },
    customMetadata: {
      head,
      kind: "native-cli",
      name,
      platform,
      sha256: sourceAsset.sha256,
    },
  });
  const retained = created ?? await bucket.head(key);
  if (!matchesNativeArtifact(retained, artifact, head)) {
    throw new Error(`Failed to publish macOS native CLI at ${key}`);
  }
  return artifact;
}

function matchesNativeArtifact(
  object: R2Object | null,
  artifact: CiArtifact,
  head: string,
): boolean {
  return object != null && object.key === artifact.key &&
    object.size === artifact.size && object.customMetadata?.head === head &&
    object.customMetadata?.kind === "native-cli" &&
    object.customMetadata?.name === artifact.name &&
    object.customMetadata?.platform === artifact.platform &&
    object.customMetadata?.sha256 === artifact.sha256 &&
    object.checksums.sha256 != null &&
    checksumHex(object.checksums.sha256) === artifact.sha256;
}

async function promoteMacReleaseArtifact(
  bucket: R2Bucket,
  head: string,
  kind: CiReleaseKind,
  releaseId: string,
  completion: CiMacCompletionEvent,
): Promise<CiReleaseAsset> {
  const asset = macReleaseArtifact(head, kind, releaseId, completion);
  const sourceAsset = completion.result.asset;
  if (!sourceAsset) throw new Error(`macOS distribution source is invalid for ${head}`);
  return promoteCiReleaseAsset(bucket, {
    kind,
    id: releaseId,
    commit: head,
    component: "macos",
    source: {
      key: sourceAsset.key,
      size: sourceAsset.size,
      sha256: sourceAsset.sha256,
      contentType: sourceAsset.contentType,
      customMetadata: {
        job: completion.jobId,
        platform: sourceAsset.platform,
        sha256: sourceAsset.sha256,
      },
    },
    asset,
  });
}

function macReleaseArtifact(
  head: string,
  kind: CiReleaseKind,
  releaseId: string,
  completion: CiMacCompletionEvent,
): CiReleaseAsset {
  const sourceAsset = completion.result.asset;
  if (!sourceAsset || !isMacNativeAsset(sourceAsset, completion.jobId)) {
    throw new Error(`macOS distribution source is invalid for ${head}`);
  }
  return {
    ...sourceAsset,
    key: `distribution/${kind}/${releaseId}/components/macos/${sourceAsset.name}`,
  };
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
