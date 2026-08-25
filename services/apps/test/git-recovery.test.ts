import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  add: vi.fn(),
  addRemote: vi.fn(),
  checkout: vi.fn(),
  commit: vi.fn(),
  fetch: vi.fn(),
  init: vi.fn(),
  isDescendent: vi.fn(),
  listFiles: vi.fn(),
  listServerRefs: vi.fn(),
  push: vi.fn(),
  readBlob: vi.fn(),
  readCommit: vi.fn(),
  remove: vi.fn(),
  resolveRef: vi.fn(),
  writeRef: vi.fn(),
}));

vi.mock("isomorphic-git", () => ({ default: mocks }));

import { canonicalJson } from "../src/builder";
import { appRepositoryName, commitProject } from "../src/git";
import type { TenantId } from "../src/registry";

describe("app Git orphan-push recovery", () => {
  const appId = "0198e2c4-365e-7a66-a58f-d4e5b46a7dad";
  const published = "1".repeat(40);
  const orphan = "2".repeat(40);
  const committed = "3".repeat(40);
  const source = "export default { fetch: () => new Response('C') };";
  const tenantId = `team:${"a".repeat(64)}` as TenantId;
  const manifest = canonicalJson({
    appId,
    entryPoint: "src/index.ts",
    jobId: "job-c",
    policyVersion: 1,
    tenantId,
  });
  const input = {
    appId,
    createdAt: "2026-08-25T10:03:00.000Z",
    expectedAncestorOid: published,
    jobId: "job-c",
    project: {
      name: "App C",
      slug: "app-c",
      entryPoint: "src/index.ts",
      files: [{ path: "src/index.ts", content: source }],
    },
    prompt: "Build C",
    tenantId,
  };

  beforeEach(() => {
    vi.clearAllMocks();
    let advertised = orphan;
    let resolved = orphan;
    mocks.init.mockResolvedValue(undefined);
    mocks.addRemote.mockResolvedValue(undefined);
    mocks.listServerRefs.mockImplementation(async () => [{ ref: "nanocodex", oid: advertised }]);
    mocks.fetch.mockImplementation(async () => ({ fetchHead: advertised }));
    mocks.writeRef.mockResolvedValue(undefined);
    mocks.checkout.mockResolvedValue(undefined);
    mocks.resolveRef.mockImplementation(async () => resolved);
    mocks.isDescendent.mockResolvedValue(true);
    mocks.listFiles
      .mockResolvedValueOnce([])
      .mockResolvedValue([".nanocodex/app.json", "src/index.ts"]);
    mocks.add.mockResolvedValue(undefined);
    mocks.remove.mockResolvedValue(undefined);
    mocks.commit.mockResolvedValue(committed);
    mocks.push.mockImplementation(async () => {
      advertised = committed;
      resolved = committed;
    });
    mocks.readCommit.mockResolvedValue({ commit: { parent: [orphan] } });
    mocks.readBlob.mockImplementation(async ({ oid, filepath }: { oid: string; filepath: string }) => {
      if (oid === orphan) {
        return { blob: new TextEncoder().encode('{"jobId":"orphaned-job"}') };
      }
      return {
        blob: new TextEncoder().encode(filepath === ".nanocodex/app.json" ? manifest : source),
      };
    });
  });

  it("appends after a validated orphan commit and reuses the same commit on retry", async () => {
    const service = { request: vi.fn() };
    const repository = await appRepositoryName(tenantId, appId);

    await expect(commitProject(service, input)).resolves.toEqual({
      oid: committed,
      repository,
    });
    await expect(commitProject(service, input)).resolves.toEqual({
      oid: committed,
      repository,
    });

    expect(mocks.isDescendent).toHaveBeenCalledWith(expect.objectContaining({
      ancestor: published,
      oid: orphan,
    }));
    expect(mocks.commit).toHaveBeenCalledTimes(1);
    expect(mocks.push).toHaveBeenCalledTimes(1);
    expect(mocks.readCommit).toHaveBeenCalledWith(expect.objectContaining({ oid: committed }));
  });

  it("rejects readback unless the new commit directly follows the fetched orphan head", async () => {
    mocks.readCommit.mockResolvedValue({ commit: { parent: [published] } });

    await expect(commitProject({ request: vi.fn() }, input))
      .rejects.toThrow("app repository update is not a direct fast-forward");
  });
});
