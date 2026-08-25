import { WorkflowEntrypoint, type WorkflowEvent, type WorkflowStep } from "cloudflare:workers";

import {
  APP_GENERATION_MODEL,
  APP_POLICY_VERSION,
  buildProject,
  generateProject,
  parseArtifact,
  serializeArtifact,
  type BuildArtifact,
  type GeneratedProject,
} from "./builder";
import {
  failJob,
  getAppBase,
  publishRevision,
  type App,
  type AppRegistry,
  type TenantId,
} from "./registry";
import { commitProject, type AppGitService } from "./git";

const MAX_ARTIFACT_BYTES = 512 * 1024;

export type BuildWorkflowParams = Readonly<{
  appId: string;
  createdAt: string;
  jobId: string;
  prompt: string;
  tenantId: TenantId;
  updateAppId?: string;
}>;

export interface BuildWorkflowEnv {
  AI: Ai;
  APP_ARTIFACTS: R2Bucket;
  APP_GIT: AppGitService;
  APP_REGISTRY: DurableObjectNamespace<AppRegistry>;
}

type StoredArtifact = Readonly<{
  artifactBytes: number;
  artifactKey: string;
  artifactHash: string;
  createdAt: string;
  displayName: string;
  mainModule: string;
  policyVersion: number;
  revisionId: string;
  slug: string;
  sourceSummary: string;
}>;

type BaseSource = Readonly<{
  expectedAncestorOid: string;
  project: GeneratedProject;
}>;

export class AppBuildWorkflow extends WorkflowEntrypoint<BuildWorkflowEnv, BuildWorkflowParams> {
  async run(event: Readonly<WorkflowEvent<BuildWorkflowParams>>, step: WorkflowStep): Promise<App> {
    const params = event.payload;
    try {
      const base = await step.do<BaseSource | null>(
        "load active source",
        async () => this.#loadBaseProject(params.tenantId, params.updateAppId),
      );
      const project = await step.do<GeneratedProject>(
        "generate project",
        {
          retries: { limit: 2, delay: "5 seconds", backoff: "exponential" },
          timeout: "5 minutes",
          sensitive: "output",
        },
        async () => generateProject(this.env.AI, params.prompt, base?.project),
      );
      const stored = await step.do<StoredArtifact>(
        "bundle and store immutable artifact",
        {
          retries: { limit: 1, delay: "3 seconds", backoff: "constant" },
          timeout: "3 minutes",
          sensitive: "output",
        },
        async () => this.#buildAndStore(params, project),
      );
      const source = await step.do(
        "commit source",
        {
          retries: { limit: 2, delay: "3 seconds", backoff: "exponential" },
          timeout: "3 minutes",
        },
        async () => commitProject(this.env.APP_GIT, {
          appId: params.updateAppId ?? params.appId,
          createdAt: params.createdAt,
          expectedAncestorOid: base?.expectedAncestorOid ?? null,
          jobId: params.jobId,
          project,
          prompt: params.prompt,
          tenantId: params.tenantId,
        }),
      );
      return await step.do<App>("publish and activate revision", async () => publishRevision(
        this.env.APP_REGISTRY,
        params.tenantId,
        {
          artifactBytes: stored.artifactBytes,
          artifactHash: stored.artifactHash,
          artifactKey: stored.artifactKey,
          createdAt: stored.createdAt,
          displayName: stored.displayName,
          generationModel: APP_GENERATION_MODEL,
          jobId: params.jobId,
          mainModule: stored.mainModule,
          policyVersion: stored.policyVersion,
          revisionId: stored.revisionId,
          slug: stored.slug,
          sourceCommitOid: source.oid,
          sourceSummary: stored.sourceSummary,
        },
      ));
    } catch (error) {
      try {
        await failJob(this.env.APP_REGISTRY, params.tenantId, {
          error: safeError(error),
          failedAt: new Date().toISOString(),
          jobId: params.jobId,
        });
      } catch (recordError) {
        console.error(JSON.stringify({
          type: "dynamic_app_build.failure_record_failed",
          job_id: params.jobId,
          error: safeError(recordError),
        }));
      }
      throw error;
    }
  }

  async #loadBaseProject(
    tenantId: TenantId,
    updateAppId: string | undefined,
  ): Promise<BaseSource | null> {
    if (!updateAppId) return null;
    const base = await getAppBase(this.env.APP_REGISTRY, tenantId, updateAppId);
    if (!base) throw new Error("update target no longer exists");
    const object = await this.env.APP_ARTIFACTS.get(base.revision.artifactKey);
    if (!object) throw new Error("active app artifact is missing");
    return {
      expectedAncestorOid: base.sourceHeadCommitOid,
      project: (await parseArtifact(await object.text())).project,
    };
  }

  async #buildAndStore(
    params: BuildWorkflowParams,
    project: GeneratedProject,
  ): Promise<StoredArtifact> {
    const artifact = await buildProject(project);
    const serialized = serializeArtifact(artifact);
    const artifactBytes = new TextEncoder().encode(serialized).byteLength;
    if (artifactBytes > MAX_ARTIFACT_BYTES) throw new Error("compiled app artifact exceeds 512 KiB");
    const targetAppId = params.updateAppId ?? params.appId;
    const artifactKey = `apps/${targetAppId}/revisions/${artifact.revision}/worker.json`;
    const existing = await this.env.APP_ARTIFACTS.get(artifactKey);
    if (existing) {
      if (await existing.text() !== serialized) throw new Error("immutable artifact key conflict");
    } else {
      const uploaded = await this.env.APP_ARTIFACTS.put(artifactKey, serialized, {
        customMetadata: {
          appId: targetAppId,
          policyVersion: String(APP_POLICY_VERSION),
          revision: artifact.revision,
          tenantId: params.tenantId,
        },
        httpMetadata: {
          cacheControl: "private, max-age=31536000, immutable",
          contentType: "application/json; charset=utf-8",
        },
        onlyIf: { etagDoesNotMatch: "*" },
      });
      if (!uploaded) {
        const raced = await this.env.APP_ARTIFACTS.get(artifactKey);
        if (!raced || await raced.text() !== serialized) {
          throw new Error("immutable artifact upload conflict");
        }
      }
    }
    return storedArtifact(artifact, artifactKey, artifactBytes);
  }
}

function storedArtifact(
  artifact: BuildArtifact,
  artifactKey: string,
  artifactBytes: number,
): StoredArtifact {
  return {
    artifactBytes,
    artifactHash: artifact.revision,
    artifactKey,
    createdAt: new Date().toISOString(),
    displayName: artifact.project.name,
    mainModule: artifact.mainModule,
    policyVersion: artifact.policyVersion,
    revisionId: artifact.revision,
    slug: artifact.project.slug,
    sourceSummary: JSON.stringify({
      entryPoint: artifact.project.entryPoint,
      files: artifact.project.files.map((file) => ({
        bytes: new TextEncoder().encode(file.content).byteLength,
        path: file.path,
      })),
    }),
  };
}

function safeError(error: unknown): string {
  const message = error instanceof Error ? `${error.name}: ${error.message}` : String(error);
  return message.replace(/[\r\n\t]+/g, " ").slice(0, 4_096) || "app build failed";
}
