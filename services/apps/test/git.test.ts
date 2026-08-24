import { describe, expect, it } from "vitest";

import { appRepositoryName } from "../src/git";

describe("app Git repository identity", () => {
  it("derives a separate private namespace from a valid app UUID", () => {
    expect(appRepositoryName("0198e2c4-365e-7a66-a58f-d4e5b46a7dad"))
      .toBe("app-0198e2c4-365e-7a66-a58f-d4e5b46a7dad");
  });

  it("does not accept repository names or arbitrary app identifiers from callers", () => {
    expect(() => appRepositoryName("thread-0198e2c4-365e-7a66-a58f-d4e5b46a7dad")).toThrow(/UUID/);
    expect(() => appRepositoryName("../another-tenant")).toThrow(/UUID/);
  });
});
