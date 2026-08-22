import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const runner = fileURLToPath(new URL("../ci/run-with-bounded-logs.sh", import.meta.url));
const capture = fileURLToPath(new URL("../ci/log-capture.py", import.meta.url));
const dockerfile = fileURLToPath(new URL("../ci/Dockerfile", import.meta.url));
const boundarySmoke = fileURLToPath(
  new URL("../ci/smoke-runner-boundary.sh", import.meta.url),
);
const serverPatch = fileURLToPath(
  new URL("../ci/patch-sandbox-control-server.mjs", import.meta.url),
);
const clientPatch = fileURLToPath(
  new URL("../patches/@cloudflare+sandbox+0.12.1.patch", import.meta.url),
);

test("the root Sandbox control plane is pinned and authenticated end to end", async () => {
  const [image, server, client] = await Promise.all([
    readFile(dockerfile, "utf8"),
    readFile(serverPatch, "utf8"),
    readFile(clientPatch, "utf8"),
  ]);

  assert.match(
    image,
    /cloudflare\/sandbox:0\.12\.1@sha256:ea9b35e61c800eddbc4450fad333e5dd26033a06f7d36624388b0711bef9f8c5/,
  );
  assert.match(image, /nanocodex-patch-sandbox-control-server\.mjs/);
  assert.match(
    image,
    /ENTRYPOINT \["\/usr\/local\/bin\/bun", "\/container-server\/dist\/index\.js"\]/,
  );

  assert.match(
    server,
    /80f83fb4f8ad2a3ecc75f1964f48d8e7d50d921e52fe4840cb3d45cb020227a6/,
  );
  assert.match(server, /identity\.uid !== 0 \|\| identity\.gid !== 0/);
  assert.match(server, /canonical !== target/);
  assert.match(server, /nanocodexControlAuthorized/);
  assert.match(server, /X-Nanocodex-Sandbox-Control/);
  assert.match(server, /J===\"\/api\/ping\"/);
  assert.match(server, /status:401/);
  assert.match(server, /count\(input, functionNeedle\) !== 1/);
  assert.match(server, /count\(input, fetchNeedle\) !== 1/);

  assert.deepEqual(
    [...client.matchAll(/^diff --git a\/(.+) b\/(.+)$/gm)].map((match) => [match[1], match[2]]),
    [[
      "node_modules/@cloudflare/sandbox/dist/sandbox-DKG3H156.js",
      "node_modules/@cloudflare/sandbox/dist/sandbox-DKG3H156.js",
    ]],
  );
  assert.match(client, /\^\[a-f0-9\]\{64\}\$/);
  assert.match(client, /`\$\{controlSecret\}\.\$\{this\.ctx\.id\.toString\(\)\}`/);
  assert.match(client, /"X-Nanocodex-Sandbox-Control": controlToken/);
});

test("Linux CI payloads have one powerless UID and a root UID-wide reap before backup", async () => {
  const [script, image, smoke] = await Promise.all([
    readFile(runner, "utf8"),
    readFile(dockerfile, "utf8"),
    readFile(boundarySmoke, "utf8"),
  ]);

  assert.match(image, /groupadd --gid 10001 nanocodex-ci/);
  assert.match(image, /useradd[\s\S]*--uid 10001[\s\S]*--gid 10001/);
  assert.match(image, /--shell \/usr\/sbin\/nologin/);
  assert.match(image, /sed -i '\/\^nanocodex-ci:\/d' \/etc\/subuid/);
  assert.match(image, /procps/);
  assert.match(image, /util-linux/);
  assert.match(image, /RUN \/usr\/local\/libexec\/nanocodex-ci-boundary-smoke/);

  assert.match(smoke, /NANOCODEX_CI_BOUNDARY_PROBE=must-not-cross/);
  assert.match(smoke, /NoNewPrivs:/);
  assert.match(smoke, /Cap\(Inh\|Prm\|Eff\|Bnd\|Amb\)/);
  assert.match(smoke, /socket\.AF_PACKET/);
  assert.match(smoke, /os\.setsid\(\)/);
  assert.match(smoke, /\[\[ \$status -eq 125 \]\]/);
  assert.match(smoke, /\/api\/process\/start/);
  assert.match(smoke, /WEBSOCKET 401/);
  assert.match(smoke, /http:\/\/\$\(hostname -i\):3000/);
  assert.match(smoke, /! -r \/proc\/\$PPID\/environ/);

  assert.match(script, /readonly executor_uid=10001/);
  assert.match(script, /readonly executor_gid=10001/);
  assert.match(script, /readonly ci_log_dir=\/tmp/);
  assert.match(script, /\/usr\/bin\/env -i "\$\{executor_environment\[@\]\}"/);
  assert.match(script, /--clear-groups/);
  assert.match(script, /--inh-caps=-all/);
  assert.match(script, /--ambient-caps=-all/);
  assert.match(script, /--bounding-set=-all/);
  assert.match(script, /--no-new-privs/);
  assert.match(script, /\(\$2 == uid \|\| \$3 == uid\) && \$4 !~ \/\^Z\//);
  assert.match(script, /builtin kill -STOP "\$pid"/);
  assert.match(script, /builtin kill -KILL "\$pid"/);
  assert.match(script, /clean_passes -ge 2/);
  assert.match(script, /linux_reap_executor[\s\S]*finish_captures/);
  assert.match(script, /! -type d ! -type l[\s\S]*-links \+1/);
  assert.doesNotMatch(script, /NANOCODEX_CI_(?:UID|GID|EXECUTOR|SETUID)/);
});

test("CI runner finalizes bounded logs when its command times out", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nanocodex-ci-timeout-"));
  const ready = join(directory, "command-ready");
  try {
    const child = spawn("bash", [
      runner,
      "printf 'early diagnostic\\n'; printf 'ready\\n' > \"$NANOCODEX_CI_TEST_READY\"; sleep 30",
    ], {
      env: {
        ...process.env,
        NANOCODEX_CI_LOG_DIR: directory,
        NANOCODEX_CI_LOG_CAPTURE: capture,
        NANOCODEX_CI_TEST_READY: ready,
      },
      stdio: "ignore",
    });
    const closed = new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("CI runner did not terminate")), 8_000);
      child.once("error", reject);
      child.once("close", (code) => {
        clearTimeout(timer);
        resolve(code);
      });
    });
    await waitForContent(ready, "ready\n");
    assert.equal(child.kill("SIGTERM"), true);
    const exitCode = await closed;
    assert.equal(exitCode, 124);
    assert.equal(await readFile(join(directory, "ci-step.out"), "utf8"), "early diagnostic\n");
    assert.deepEqual(
      JSON.parse(await readFile(join(directory, "ci-step.out.meta.json"), "utf8")),
      { bytesObserved: 17, bytesStored: 17, truncated: false },
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

async function waitForContent(path, expected) {
  const deadline = Date.now() + 5_000;
  for (;;) {
    if (await readFile(path, "utf8").catch(() => undefined) === expected) return;
    if (Date.now() >= deadline) throw new Error("CI child did not become ready");
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}
