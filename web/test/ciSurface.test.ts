import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const app = source("../src/NanocodexApp.tsx");
const main = source("../src/main.tsx");
const routeLoaders = source("../src/routeLoaders.ts");
const ci = source("../src/Ci.tsx");
const css = source("../src/ci.css");
const readme = source("../README.md");

test("CI is a direct, themed application surface", () => {
  assert.match(main, /preloadDirectSurface\(directUrl\)/);
  assert.match(routeLoaders, /export const loadCi = \(\) =>\s*import\("\.\/Ci"\)/);
  assert.match(routeLoaders, /surface === "ci"\) await loadCi\(\)/);
  assert.doesNotMatch(main, /CiStandalone/);
  assert.match(app, /nextSurface === "ci"/);
  assert.match(app, /<Ci \/>/);
  assert.match(app, /productNavigation\.map/);
  assert.doesNotMatch(ci, /CiStandalone/);
});

test("CI presents the complete Worker pipeline without transient loading UI", () => {
  assert.match(ci, /Cloudflare-native CI/);
  assert.equal([...ci.matchAll(/\[\s*"[a-z0-9-]+",/g)].length, 17);
  assert.match(ci, /authenticated Apple Silicon tests and verified arm64 binary/);
  assert.match(ci, /without GitHub Actions/);
  assert.match(ci, /\{gates\.length\} gates\. One source\./);
  assert.doesNotMatch(ci, /Loading|spinner|aria-busy|connecting|waiting/);
});

test("CI follows the compact shared theme and mobile interaction baseline", () => {
  assert.match(css, /\.ci-dashboard \{[^}]*width:\s*min\(100%, 860px\)/);
  assert.match(css, /100svh - var\(--shell-header-height\)/);
  assert.match(css, /\.ci-error button,\s*\.ci-runs button \{[^}]*min-height:\s*44px/);
  assert.doesNotMatch(css, /gradient|box-shadow/);
});

test("CI preserves complete state while polling and exposes truthful gate progress", () => {
  assert.match(ci, /if \(!overview && !error\) return null/);
  assert.match(ci, /window\.setTimeout\(\(\) => void poll\(\), 5_000\)/);
  assert.doesNotMatch(ci, /setInterval/);
  assert.match(ci, /status === "running"/);
  assert.match(ci, /status === "terminated"/);
  assert.match(ci, /not started/);
});

test("CI operations document four isolated macOS identities", () => {
  assert.match(readme, /fourth identity/);
  assert.match(
    readme,
    /\/Library\/PrivilegedHelperTools\/dev\.nanocodex\.ci-pr-cargo-builder/,
  );
  assert.match(readme, /NOPASSWD:NOSETENV/);
  assert.match(readme, /--prep-user nanocodex-ci-pr-prep/);
  assert.match(readme, /upload-only process/);
  assert.doesNotMatch(readme, /controllers first run a credential-free\s+`cargo fetch/);
});

test("CI cutover keeps authority and retention fail-closed until live proof", () => {
  assert.match(readme, /r2 bucket lifecycle list nanocodex-ci/);
  assert.match(readme, /no rule covers `distribution\/`, `release-import\/`/);
  assert.match(readme, /Keep every GitHub Actions workflow active until/);
  assert.match(readme, /make the single GitHub status\s+context `ci success` required/);
  assert.match(readme, /Do not push a stable tag while the broad tag-triggered `release\.yml` is active/);
  assert.match(readme, /old nightly client\s+has crossed to the Cloudflare updater/);
});

function source(path: string) {
  return readFileSync(new URL(path, import.meta.url), "utf8");
}
