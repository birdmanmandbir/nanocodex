import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const app = source("../src/NanocodexApp.tsx");
const main = source("../src/main.tsx");
const ci = source("../src/Ci.tsx");
const css = source("../src/ci.css");

test("CI is a direct, themed application surface", () => {
  assert.match(main, /surface === "ci"[^;]+import\("\.\/Ci"\)/);
  assert.doesNotMatch(main, /CiStandalone/);
  assert.match(app, /nextSurface === "ci"/);
  assert.match(app, /<Ci \/>/);
  assert.match(app, /productNavigation\.map/);
  assert.doesNotMatch(ci, /CiStandalone/);
});

test("CI presents the complete Worker pipeline without transient loading UI", () => {
  assert.match(ci, /Cloudflare-native CI/);
  assert.equal([...ci.matchAll(/\["[a-z0-9-]+",/g)].length, 12);
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
  assert.match(ci, /not started/);
});

function source(path: string) {
  return readFileSync(new URL(path, import.meta.url), "utf8");
}
