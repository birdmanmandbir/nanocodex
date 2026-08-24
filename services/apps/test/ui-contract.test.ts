/// <reference types="vite/client" />

import { describe, expect, it } from "vitest";

import { appLaunchPath, formatBytes, hasCompletedBuild } from "../src/ui/AppsConsole";
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

  it("builds account-gated launch paths and readable artifact sizes", () => {
    expect(appLaunchPath("0198e2c4-365e-7a66-a58f-d4e5b46a7dad")).toBe(
      "/apps/api/apps/0198e2c4-365e-7a66-a58f-d4e5b46a7dad/launch",
    );
    expect(formatBytes(913)).toBe("913 B");
    expect(formatBytes(1_536)).toBe("1.5 KB");
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
