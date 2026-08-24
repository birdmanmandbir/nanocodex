/// <reference types="vite/client" />

import { describe, expect, it } from "vitest";

import {
  appLaunchPath,
  formatBytes,
  hasCompletedBuild,
  workspacePath,
} from "../src/ui/AppsConsole";
import consoleSource from "../src/ui/AppsConsole.tsx?raw";
import mainSource from "../src/ui/main.tsx?raw";

describe("owner console contract", () => {
  it("has exactly one declarative React root", () => {
    expect((mainSource.match(/\bcreateRoot\s*\(/g) ?? []).length).toBe(1);
    expect(consoleSource).not.toMatch(/\bcreateRoot\s*\(/);
  });

  it("does not introduce transient placeholder UI", () => {
    expect(`${mainSource}\n${consoleSource}`).not.toMatch(/\b(?:loading|spinner|skeleton|Suspense)\b/i);
  });

  it("adds one canonical workspace selector to every app operation", () => {
    const opaqueTeamId = "a".repeat(64);
    expect(workspacePath("/apps/api/apps", "personal")).toBe(
      "/apps/api/apps?workspace=personal",
    );
    expect(workspacePath("/apps/api/builds/job-one", `team:${opaqueTeamId}`)).toBe(
      `/apps/api/builds/job-one?workspace=team%3A${opaqueTeamId}`,
    );
    expect(appLaunchPath("app-one", `team:${opaqueTeamId}`)).toBe(
      `/apps/api/apps/app-one/launch?workspace=team%3A${opaqueTeamId}`,
    );
    expect(() => workspacePath("/apps/api/apps?workspace=personal", "personal")).toThrow(
      "workspace selector is already present",
    );
    expect(formatBytes(913)).toBe("913 B");
    expect(formatBytes(1_536)).toBe("1.5 KB");
  });

  it("keeps team authority and invitations in component state", () => {
    expect(consoleSource).toContain('requestJson<MeResponse>("/v1/me")');
    expect(consoleSource).toContain('"/v1/team-invitations/accept"');
    expect(consoleSource).toContain("setFreshInvitation");
    expect(consoleSource).toContain("navigator.clipboard.writeText(freshInvitation.token)");
    expect(consoleSource).not.toMatch(/localStorage|sessionStorage|location\.(?:hash|search)/);
  });

  it("gates owner mutations while retaining member launch and history", () => {
    expect(consoleSource).toContain('const canManage = selectedWorkspace === "personal" || selectedTeam?.role === "owner"');
    expect(consoleSource).toContain("{canManage ? (");
    expect(consoleSource).toContain("<LaunchApp app={app} workspace={workspace}");
    expect(consoleSource).toContain("canManage && !isActive");
  });

  it("threads the selected workspace through list, build, poll, activate, and launch", () => {
    expect(consoleSource).toContain('workspacePath("/apps/api/apps", workspace)');
    expect(consoleSource).toContain("workspacePath(`/apps/api/builds/${encodeURIComponent(id)}`, workspace)");
    expect(consoleSource).toContain('workspacePath("/apps/api/builds", workspace)');
    expect(consoleSource).toContain("workspacePath(`/apps/api/apps/${encodeURIComponent(app.id)}/activate`, workspace)");
    expect(consoleSource).toContain("appLaunchPath(app.id, workspace)");
  });

  it("detects a published build directly from polled API state", () => {
    expect(hasCompletedBuild([
      {
        job: {
          id: "job-one",
          app_id: "app-one",
          status: "completed",
          revision: "a".repeat(64),
          error: null,
          created_at: "2026-08-25T00:00:00.000Z",
          completed_at: "2026-08-25T00:01:00.000Z",
        },
      },
    ])).toBe(true);
    expect(hasCompletedBuild([])).toBe(false);
  });
});
