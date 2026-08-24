import { describe, expect, it } from "vitest";

import { createMemoryGitFs } from "../src/memory-fs";

describe("memory git filesystem", () => {
  it("supports the promise filesystem operations isomorphic-git requires", async () => {
    const fs = createMemoryGitFs();
    await fs.promises.mkdir("/workspace/src", { recursive: true });
    await fs.promises.writeFile("/workspace/src/index.js", "export default 1;");

    expect(await fs.promises.readdir("/workspace/src")).toEqual(["index.js"]);
    expect(await fs.promises.readFile("/workspace/src/index.js", "utf8")).toBe("export default 1;");
    expect((await fs.promises.stat("/workspace/src/index.js") as { isFile(): boolean }).isFile()).toBe(true);

    await fs.promises.unlink("/workspace/src/index.js");
    await fs.promises.rmdir("/workspace/src");
    expect(await fs.promises.readdir("/workspace")).toEqual([]);
  });

  it("rejects path escapes and non-empty directory removal", async () => {
    const fs = createMemoryGitFs();
    await fs.promises.mkdir("/workspace");
    await fs.promises.writeFile("/workspace/file", new Uint8Array([1]));

    await expect(fs.promises.readFile("/../../secret")).rejects.toMatchObject({ code: "EPERM" });
    await expect(fs.promises.rmdir("/workspace")).rejects.toMatchObject({ code: "ENOTEMPTY" });
  });
});
