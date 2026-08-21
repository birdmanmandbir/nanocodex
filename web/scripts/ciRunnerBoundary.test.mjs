import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const runner = fileURLToPath(new URL("../ci/run-with-bounded-logs.sh", import.meta.url));
const capture = fileURLToPath(new URL("../ci/log-capture.py", import.meta.url));

test("CI runner finalizes bounded logs when its command times out", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nanocodex-ci-timeout-"));
  try {
    const child = spawn("bash", [runner, "printf 'early diagnostic\\n'; sleep 30"], {
      env: {
        ...process.env,
        NANOCODEX_CI_LOG_DIR: directory,
        NANOCODEX_CI_LOG_CAPTURE: capture,
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
    await new Promise((resolve) => setTimeout(resolve, 200));
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
