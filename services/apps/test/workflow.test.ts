import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  buildProject: vi.fn(),
  commitProject: vi.fn(),
  failJob: vi.fn(),
  generateProject: vi.fn(),
  getAppBase: vi.fn(),
  parseArtifact: vi.fn(),
  publishRevision: vi.fn(),
  serializeArtifact: vi.fn(),
}));

vi.mock("cloudflare:workers", () => ({
  WorkflowEntrypoint: class {
    readonly env: unknown;

    constructor(_ctx: unknown, env: unknown) {
      this.env = env;
    }
  },
}));

vi.mock("../src/builder", () => ({
  APP_GENERATION_MODEL: "gpt-5",
  APP_POLICY_VERSION: 1,
  buildProject: mocks.buildProject,
  generateProject: mocks.generateProject,
  parseArtifact: mocks.parseArtifact,
  serializeArtifact: mocks.serializeArtifact,
}));

vi.mock("../src/registry", () => ({
  failJob: mocks.failJob,
  getAppBase: mocks.getAppBase,
  publishRevision: mocks.publishRevision,
}));

vi.mock("../src/git", () => ({ commitProject: mocks.commitProject }));

import { AppBuildWorkflow } from "../src/workflow";
import type { TenantId } from "../src/registry";

describe("app build workflow source ancestry", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("generates C from rolled-back A while appending C after Git head B", async () => {
    const tenantId = "team:engineering" as TenantId;
    const projectA = {
      name: "App A",
      slug: "app-a",
      entryPoint: "src/index.ts",
      files: [{ path: "src/index.ts", content: "export default 'A';" }],
    };
    const projectC = {
      ...projectA,
      files: [{ path: "src/index.ts", content: "export default 'C';" }],
    };
    const sourceA = "1".repeat(40);
    const sourceB = "2".repeat(40);
    const sourceC = "3".repeat(40);
    mocks.getAppBase.mockResolvedValue({
      app: { activeRevisionId: "a".repeat(64) },
      revision: {
        artifactKey: "apps/app-live/revisions/a/worker.json",
        sourceCommitOid: sourceA,
      },
      sourceHeadCommitOid: sourceB,
    });
    mocks.parseArtifact.mockResolvedValue({ project: projectA });
    mocks.generateProject.mockResolvedValue(projectC);
    mocks.buildProject.mockResolvedValue({
      mainModule: "compiled.js",
      policyVersion: 1,
      project: projectC,
      revision: "c".repeat(64),
    });
    mocks.serializeArtifact.mockReturnValue("serialized-c");
    mocks.commitProject.mockResolvedValue({ oid: sourceC, repository: "app-app-live" });
    mocks.publishRevision.mockResolvedValue({ appId: "app-live" });
    const bucket = {
      get: vi.fn()
        .mockResolvedValueOnce({ text: vi.fn().mockResolvedValue("artifact-a") })
        .mockResolvedValueOnce(null),
      put: vi.fn().mockResolvedValue({}),
    };
    const env = {
      AI: {},
      APP_ARTIFACTS: bucket,
      APP_GIT: {},
      APP_REGISTRY: {},
    };
    const workflow = new AppBuildWorkflow({} as ExecutionContext, env as never);
    const step = {
      do: vi.fn(async (_name: string, optionsOrCallback: unknown, callback?: () => unknown) => {
        const operation = typeof optionsOrCallback === "function" ? optionsOrCallback : callback;
        return await operation!();
      }),
    };

    await workflow.run({
      payload: {
        appId: "reserved-app-id",
        createdAt: "2026-08-25T10:03:00.000Z",
        jobId: "job-c",
        prompt: "Build C",
        tenantId,
        updateAppId: "app-live",
      },
    } as never, step as never);

    expect(mocks.generateProject).toHaveBeenCalledWith(env.AI, "Build C", projectA);
    expect(mocks.commitProject).toHaveBeenCalledWith(env.APP_GIT, expect.objectContaining({
      appId: "app-live",
      expectedAncestorOid: sourceB,
      jobId: "job-c",
      project: projectC,
    }));
    expect(mocks.publishRevision).toHaveBeenCalledWith(env.APP_REGISTRY, tenantId, expect.objectContaining({
      sourceCommitOid: sourceC,
    }));
  });
});
