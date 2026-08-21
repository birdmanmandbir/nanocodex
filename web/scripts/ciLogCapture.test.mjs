import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const script = fileURLToPath(new URL("../ci/log-capture.py", import.meta.url));

test("CI log capture retains a bounded head and tail with byte counters", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nanocodex-ci-log-"));
  const output = join(directory, "stdout.log");
  try {
    const capture = spawnSync("python3", [script, output], {
      input: "0123456789",
      env: {
        ...process.env,
        NANOCODEX_CI_LOG_HEAD_BYTES: "4",
        NANOCODEX_CI_LOG_TAIL_BYTES: "3",
      },
      encoding: "utf8",
    });
    assert.equal(capture.status, 0, capture.stderr);
    assert.equal(
      await readFile(output, "utf8"),
      "0123\n[... nanocodex CI omitted 3 log bytes ...]\n789",
    );
    assert.deepEqual(JSON.parse(await readFile(`${output}.meta.json`, "utf8")), {
      bytesObserved: 10,
      bytesStored: 51,
      truncated: true,
    });
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
