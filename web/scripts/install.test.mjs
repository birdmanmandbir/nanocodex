import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  readlink,
  rm,
  stat,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";
import { spawnSync } from "node:child_process";
import test from "node:test";

const installer = fileURLToPath(new URL("../../install", import.meta.url));
const releaseOrigin = "https://nanocodex.me-7fb.workers.dev";
const releaseApiPath = "/api/releases";
const maximumBytes = 256 * 1024 * 1024;
const linuxCli = "nanocodex-x86_64-unknown-linux-gnu";
const macosCli = "nanocodex-aarch64-apple-darwin";
const guestAssetBinary = "nanocodex-vm-guest-x86_64-unknown-linux-musl";
const guestStoreBinary = "nanocodex-vm-guest";

test("the shell installer parses as Bash", () => {
  const result = spawnSync("bash", ["-n", installer], { encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr);
});

test("v0.5.0 bootstraps from the raw Linux CLI without a VM guest", async (t) => {
  const harness = await createHarness(t);
  const cli = Buffer.from("#!/bin/sh\nprintf '%s|%s\\n' \"$0\" \"$*\"\n");
  const tag = "v0.5.0";
  const gzipUrl = rollingAsset(`${linuxCli}.gz`);
  const rawUrl = rollingAsset(linuxCli);

  const result = await harness.run({
    [gzipUrl]: notFound(),
    [rawUrl]: assetFixture(tag, linuxCli, cli, { rolling: true }),
  });
  assert.equal(result.status, 0, result.stderr);

  const versionDirectory = join(harness.installRoot, "versions", "0.5.0");
  assert.deepEqual((await readdir(versionDirectory)).sort(), [
    "nanocodex",
    "nanocodex.sha256",
  ]);
  assert.deepEqual(await readFile(join(versionDirectory, "nanocodex")), cli);
  assert.equal(
    await readFile(join(versionDirectory, "nanocodex.sha256"), "utf8"),
    `${sha256(cli)}\n`,
  );
  assert.equal(await readlink(join(harness.installRoot, "current")), "versions/0.5.0");
  assert.deepEqual(await readFile(join(harness.installRoot, "updater", "nanocodex")), cli);
  assert.equal(
    await readFile(join(harness.installRoot, "updater", "nanocodex.sha256"), "utf8"),
    `${sha256(cli)}\n`,
  );
  assert.ok((await stat(join(harness.installRoot, "bin", "nanocodex"))).mode & 0o111);

  const normal = spawnSync(join(harness.installRoot, "bin", "nanocodex"), ["hello"], {
    encoding: "utf8",
  });
  assert.equal(normal.status, 0, normal.stderr);
  assert.match(normal.stdout, /\/current\/nanocodex\|hello\n$/);
  const update = spawnSync(join(harness.installRoot, "bin", "nanocodex"), ["update", "0.6.0"], {
    encoding: "utf8",
  });
  assert.equal(update.status, 0, update.stderr);
  assert.match(update.stdout, /\/updater\/nanocodex\|update 0\.6\.0\n$/);

  const calls = await harness.calls();
  assert.deepEqual(calls.map(({ url }) => url), [gzipUrl, rawUrl]);
  assertSecureCurlArguments(calls);
});

test("later Linux stable installs gzip CLI and guest as one coherent version", async (t) => {
  const harness = await createHarness(t);
  const tag = "v0.6.0";
  const cli = Buffer.from("later gzip cli");
  const guestBinary = Buffer.from("later gzip guest");
  const cliArchive = gzipSync(cli);
  const guestArchive = gzipSync(guestBinary);
  const cliAsset = `${linuxCli}.gz`;
  const guestAsset = `${guestAssetBinary}.gz`;
  const cliUrl = rollingAsset(cliAsset);
  const guestUrl = immutableAsset(tag, guestAsset);

  const result = await harness.run({
    [cliUrl]: assetFixture(tag, cliAsset, cliArchive, { rolling: true }),
    [guestUrl]: assetFixture(tag, guestAsset, guestArchive),
  });
  assert.equal(result.status, 0, result.stderr);

  const versionDirectory = join(harness.installRoot, "versions", "0.6.0");
  assert.deepEqual((await readdir(versionDirectory)).sort(), [
    "nanocodex",
    "nanocodex-vm-guest",
    "nanocodex-vm-guest.sha256",
    "nanocodex.sha256",
  ]);
  assert.deepEqual(await readFile(join(versionDirectory, "nanocodex")), cli);
  assert.deepEqual(await readFile(join(versionDirectory, guestStoreBinary)), guestBinary);
  assert.equal(
    await readFile(join(versionDirectory, "nanocodex.sha256"), "utf8"),
    `${sha256(cli)}\n`,
  );
  assert.equal(
    await readFile(join(versionDirectory, `${guestStoreBinary}.sha256`), "utf8"),
    `${sha256(guestBinary)}\n`,
  );
  assert.ok((await stat(join(versionDirectory, guestStoreBinary))).mode & 0o111);
  assert.equal(await readlink(join(harness.installRoot, "current")), "versions/0.6.0");

  const calls = await harness.calls();
  assert.deepEqual(calls.map(({ url }) => url), [cliUrl, guestUrl]);
  assertSecureCurlArguments(calls);
  assert.equal(calls.some(({ url }) => url.includes("/channels/latest/assets/nanocodex-vm")), false);
});

test("macOS remains CLI-only after the legacy release", async (t) => {
  const harness = await createHarness(t, { system: "Darwin", machine: "arm64" });
  const tag = "v0.8.0";
  const cli = Buffer.from("macos cli");
  const archive = gzipSync(cli);
  const asset = `${macosCli}.gz`;
  const url = rollingAsset(asset);

  const result = await harness.run({
    [url]: assetFixture(tag, asset, archive, { rolling: true }),
  });
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(
    (await readdir(join(harness.installRoot, "versions", "0.8.0"))).sort(),
    ["nanocodex", "nanocodex.sha256"],
  );
  assert.deepEqual((await harness.calls()).map(({ url: requested }) => requested), [url]);
});

test("redirects and noncanonical rolling locations are rejected", async (t) => {
  await t.test("redirect response is never followed", async (t) => {
    const harness = await createHarness(t);
    const url = rollingAsset(`${linuxCli}.gz`);
    const result = await harness.run({
      [url]: {
        status: 302,
        bodyBase64: Buffer.from("redirect").toString("base64"),
        headers: {
          "Content-Length": "8",
          Location: "https://evil.example/nanocodex",
        },
      },
    });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /HTTP 302/);
    const calls = await harness.calls();
    assert.equal(calls.length, 1);
    assertSecureCurlArguments(calls);
    await assertNoActivation(harness.installRoot, "0.6.0");
  });

  await t.test("rolling response must identify the exact immutable path", async (t) => {
    const harness = await createHarness(t);
    const tag = "v0.6.0";
    const body = gzipSync(Buffer.from("cli"));
    const asset = `${linuxCli}.gz`;
    const url = rollingAsset(asset);
    const fixture = assetFixture(tag, asset, body, { rolling: true });
    fixture.headers["Content-Location"] =
      `${releaseApiPath}/releases/stable/v0.5.0/assets/${asset}`;
    const result = await harness.run({ [url]: fixture });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /invalid Content-Location/);
    await assertNoActivation(harness.installRoot, "0.6.0");
  });
});

test("archive limits, lengths, and checksums fail closed", async (t) => {
  const body = gzipSync(Buffer.from("bounded cli"));
  const asset = `${linuxCli}.gz`;
  const url = rollingAsset(asset);

  for (const [name, mutate, error] of [
    [
      "declared oversize",
      (fixture) => { fixture.headers["Content-Length"] = String(maximumBytes + 1); },
      /oversized Content-Length/,
    ],
    [
      "streamed oversize",
      (fixture) => {
        fixture.sparseSize = maximumBytes + 1;
        fixture.headers["Content-Length"] = "1";
      },
      /failed to download|byte count did not match/,
    ],
    [
      "length mismatch",
      (fixture) => { fixture.headers["Content-Length"] = String(body.byteLength + 1); },
      /byte count did not match/,
    ],
    [
      "checksum mismatch",
      (fixture) => { fixture.headers["X-Nanocodex-Sha256"] = "0".repeat(64); },
      /checksum mismatch/,
    ],
  ]) {
    await t.test(name, async (t) => {
      const harness = await createHarness(t);
      await seedPreviousActivation(harness.installRoot);
      const fixture = assetFixture("v0.6.0", asset, body, { rolling: true });
      mutate(fixture);
      const result = await harness.run({ [url]: fixture });
      assert.notEqual(result.status, 0);
      assert.match(result.stderr, error);
      assert.equal(await readlink(join(harness.installRoot, "current")), "versions/old");
      await assert.rejects(stat(join(harness.installRoot, "versions", "0.6.0")));
      assertSecureCurlArguments(await harness.calls());
    });
  }
});

test("guest validation failure cannot partially activate a later stable", async (t) => {
  const harness = await createHarness(t);
  await seedPreviousActivation(harness.installRoot);
  const tag = "v0.7.0";
  const cliAsset = `${linuxCli}.gz`;
  const guestAsset = `${guestAssetBinary}.gz`;
  const cliArchive = gzipSync(Buffer.from("valid cli"));
  const guestArchive = gzipSync(Buffer.from("invalid guest hash"));
  const cliUrl = rollingAsset(cliAsset);
  const guestUrl = immutableAsset(tag, guestAsset);
  const guestFixture = assetFixture(tag, guestAsset, guestArchive);
  guestFixture.headers["X-Nanocodex-Sha256"] = "f".repeat(64);

  const result = await harness.run({
    [cliUrl]: assetFixture(tag, cliAsset, cliArchive, { rolling: true }),
    [guestUrl]: guestFixture,
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /checksum mismatch/);
  assert.equal(await readlink(join(harness.installRoot, "current")), "versions/old");
  await assert.rejects(stat(join(harness.installRoot, "versions", "0.7.0")));
  assert.deepEqual((await harness.calls()).map(({ url }) => url), [cliUrl, guestUrl]);
});

test("an immutable guest from a different release identity is rejected", async (t) => {
  const harness = await createHarness(t);
  await seedPreviousActivation(harness.installRoot);
  const tag = "v0.9.0";
  const cliAsset = `${linuxCli}.gz`;
  const guestAsset = `${guestAssetBinary}.gz`;
  const cliUrl = rollingAsset(cliAsset);
  const guestUrl = immutableAsset(tag, guestAsset);

  const result = await harness.run({
    [cliUrl]: assetFixture(tag, cliAsset, gzipSync(Buffer.from("cli")), { rolling: true }),
    [guestUrl]: assetFixture("v0.10.0", guestAsset, gzipSync(Buffer.from("guest"))),
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /did not belong to the resolved stable release/);
  assert.equal(await readlink(join(harness.installRoot, "current")), "versions/old");
  await assert.rejects(stat(join(harness.installRoot, "versions", "0.9.0")));
});

async function createHarness(t, { system = "Linux", machine = "x86_64" } = {}) {
  const root = await mkdtemp(join(tmpdir(), "nanocodex-install-test-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const fakeBin = join(root, "fake-bin");
  const home = join(root, "home");
  const installRoot = join(root, "nanocodex");
  const fixturePath = join(root, "fixtures.json");
  const logPath = join(root, "curl.jsonl");
  const temporary = join(root, "tmp");
  await Promise.all([
    mkdir(fakeBin, { recursive: true }),
    mkdir(home, { recursive: true }),
    mkdir(temporary, { recursive: true }),
  ]);

  await writeExecutable(join(fakeBin, "uname"), `#!/bin/sh
case "\${1-}" in
  -s) printf '%s\\n' "\$FAKE_UNAME_SYSTEM" ;;
  -m) printf '%s\\n' "\$FAKE_UNAME_MACHINE" ;;
  *) exit 2 ;;
esac
`);
  await writeExecutable(join(fakeBin, "sha256sum"), `#!/usr/bin/env node
const fs = require("node:fs");
const crypto = require("node:crypto");
const path = process.argv[2];
const digest = crypto.createHash("sha256").update(fs.readFileSync(path)).digest("hex");
process.stdout.write(digest + "  " + path + "\\n");
`);
  await writeExecutable(join(fakeBin, "curl"), fakeCurlSource);

  const environment = {
    ...process.env,
    PATH: `${fakeBin}:${process.env.PATH ?? "/usr/bin:/bin"}`,
    HOME: home,
    NANOCODEX_DIR: installRoot,
    SHELL: "/no/profile",
    TMPDIR: temporary,
    FAKE_UNAME_SYSTEM: system,
    FAKE_UNAME_MACHINE: machine,
    FAKE_CURL_FIXTURES: fixturePath,
    FAKE_CURL_LOG: logPath,
  };

  return {
    installRoot,
    async run(fixtures) {
      await writeFile(fixturePath, JSON.stringify(fixtures));
      await rm(logPath, { force: true });
      return spawnSync("bash", [installer], { encoding: "utf8", env: environment });
    },
    async calls() {
      const contents = await readFile(logPath, "utf8");
      return contents.trim().split("\n").filter(Boolean).map((line) => JSON.parse(line));
    },
  };
}

function assetFixture(tag, name, body, { rolling = false } = {}) {
  const path = `${releaseApiPath}/releases/stable/${tag}/assets/${name}`;
  return {
    status: 200,
    bodyBase64: body.toString("base64"),
    headers: {
      "Content-Length": String(body.byteLength),
      "Content-Type": name.endsWith(".gz") ? "application/gzip" : "application/octet-stream",
      "X-Nanocodex-Release": tag,
      "X-Nanocodex-Sha256": sha256(body),
      ...(rolling ? { "Content-Location": path } : {}),
    },
  };
}

function notFound() {
  const body = Buffer.from("not found");
  return {
    status: 404,
    bodyBase64: body.toString("base64"),
    headers: {
      "Content-Length": String(body.byteLength),
      "Content-Type": "application/json",
    },
  };
}

function rollingAsset(name) {
  return `${releaseOrigin}${releaseApiPath}/channels/latest/assets/${name}`;
}

function immutableAsset(tag, name) {
  return `${releaseOrigin}${releaseApiPath}/releases/stable/${tag}/assets/${name}`;
}

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

function argumentValue(args, name) {
  const index = args.indexOf(name);
  assert.notEqual(index, -1, `missing curl argument ${name}`);
  return args[index + 1];
}

function assertSecureCurlArguments(calls) {
  assert.ok(calls.length > 0);
  for (const { args, url } of calls) {
    assert.equal(argumentValue(args, "--proto"), "=https");
    assert.equal(argumentValue(args, "--max-redirs"), "0");
    assert.equal(argumentValue(args, "--max-filesize"), String(maximumBytes));
    assert.equal(args.includes("--location"), false);
    assert.equal(args.includes("-L"), false);
    assert.match(url, /^https:\/\/nanocodex\.me-7fb\.workers\.dev\/api\/releases\//);
  }
}

async function seedPreviousActivation(installRoot) {
  const previous = join(installRoot, "versions", "old");
  await mkdir(previous, { recursive: true });
  await writeFile(join(previous, "nanocodex"), "old");
  await symlink("versions/old", join(installRoot, "current"));
}

async function assertNoActivation(installRoot, version) {
  await assert.rejects(stat(join(installRoot, "current")));
  await assert.rejects(stat(join(installRoot, "versions", version)));
}

async function writeExecutable(path, contents) {
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, contents);
  await chmod(path, 0o755);
}

const fakeCurlSource = `#!/usr/bin/env node
const fs = require("node:fs");
const args = process.argv.slice(2);
const url = args[args.length - 1];
fs.appendFileSync(process.env.FAKE_CURL_LOG, JSON.stringify({ args, url }) + "\\n");
const fixtures = JSON.parse(fs.readFileSync(process.env.FAKE_CURL_FIXTURES, "utf8"));
const fixture = fixtures[url];
if (!fixture) {
  process.stderr.write("missing fake curl fixture for " + url + "\\n");
  process.exit(2);
}
const value = (name) => {
  const index = args.indexOf(name);
  if (index < 0 || index + 1 >= args.length) throw new Error("missing " + name);
  return args[index + 1];
};
const output = value("--output");
if (fixture.sparseSize != null) {
  const descriptor = fs.openSync(output, "w");
  fs.ftruncateSync(descriptor, fixture.sparseSize);
  fs.closeSync(descriptor);
} else {
  fs.writeFileSync(output, Buffer.from(fixture.bodyBase64 ?? "", "base64"));
}
const status = String(fixture.status ?? 200).padStart(3, "0");
const headers = ["HTTP/1.1 " + status + " Fixture"];
for (const [name, headerValue] of Object.entries(fixture.headers ?? {})) {
  headers.push(name + ": " + headerValue);
}
fs.writeFileSync(value("--dump-header"), headers.join("\\r\\n") + "\\r\\n\\r\\n");
const rendered = value("--write-out")
  .replaceAll("%{http_code}", status)
  .replaceAll("%{url_effective}", fixture.effectiveUrl ?? url);
process.stdout.write(rendered);
process.exit(fixture.exitCode ?? 0);
`;
