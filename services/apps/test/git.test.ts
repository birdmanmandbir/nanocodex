import { describe, expect, it } from "vitest";

import { appRepositoryName } from "../src/git";
import type { TenantId } from "../src/registry";

describe("app Git repository identity", () => {
  const appId = "0198e2c4-365e-7a66-a58f-d4e5b46a7dad";
  const personal = "user:0198e2c4-365e-7a66-a58f-d4e5b46a7dad" as TenantId;
  const teamA = `team:${"a".repeat(64)}` as TenantId;
  const teamB = `team:${"b".repeat(64)}` as TenantId;

  it("derives deterministic disjoint private repositories from tenant and app identity", async () => {
    const personalName = await appRepositoryName(personal, appId);
    expect(personalName).toMatch(/^app-[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/);
    expect(await appRepositoryName(personal, appId)).toBe(personalName);
    expect(new Set(await Promise.all([
      appRepositoryName(personal, appId),
      appRepositoryName(teamA, appId),
      appRepositoryName(teamB, appId),
    ])).size).toBe(3);
  });

  it("does not accept tenant or repository identities from callers", async () => {
    await expect(appRepositoryName("team:../other" as TenantId, appId)).rejects.toThrow(/tenantId/);
    await expect(appRepositoryName(personal, "../another-tenant")).rejects.toThrow(/UUID/);
  });
});
