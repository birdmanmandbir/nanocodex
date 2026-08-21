import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const app = source("../src/NanocodexApp.tsx");
const main = source("../src/main.tsx");
const ci = source("../src/Ci.tsx");
const css = source("../src/ci.css");

test("CI is a direct, visible application surface", () => {
  assert.match(main, /directPath === "\/ci"/);
  assert.match(main, /<CiStandalone \/>/);
  assert.match(app, /nextSurface === "ci"/);
  assert.match(app, /<Ci \/>/);
  assert.match(ci, /<main id="top"><Ci \/><\/main>/);
});

test("CI presents the complete Worker pipeline without transient loading UI", () => {
  assert.match(ci, /Cloudflare-native continuous integration/);
  assert.equal([...ci.matchAll(/\["[a-z0-9-]+",/g)].length, 12);
  assert.doesNotMatch(ci, /Loading|spinner|aria-busy/);
});

test("CI uses the full-width shell and mobile interaction baseline", () => {
  assert.match(css, /\.ci-dashboard \{[^}]*width:\s*100%/);
  assert.match(css, /100svh - var\(--shell-header-height\)/);
  assert.match(css, /\.ci-error button, \.ci-runs button \{[^}]*min-height:\s*44px/);
});

function source(path: string) {
  return readFileSync(new URL(path, import.meta.url), "utf8");
}
