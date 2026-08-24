import { describe, expect, it } from "vitest";

import { appRequest, appResponse } from "../src/boundary";

describe("dynamic app request boundary", () => {
  it("removes host credentials before invoking arbitrary code", () => {
    const forwarded = appRequest(new Request("https://apps.example.test/api/context", {
      headers: {
        authorization: "Bearer account-api-key",
        "cf-access-jwt-assertion": "access-token",
        cookie: "nanocodex_account=signed-session; app_theme=dark",
        "proxy-authorization": "Basic proxy-secret",
        "x-app-header": "visible",
        "x-nanocodex-owner": "private-owner",
      },
    }));

    expect(forwarded.headers.get("authorization")).toBeNull();
    expect(forwarded.headers.get("cf-access-jwt-assertion")).toBeNull();
    expect(forwarded.headers.get("cookie")).toBeNull();
    expect(forwarded.headers.get("proxy-authorization")).toBeNull();
    expect(forwarded.headers.get("x-nanocodex-owner")).toBeNull();
    expect(forwarded.headers.get("x-app-header")).toBe("visible");
  });

  it("prevents arbitrary code from minting host cookies", () => {
    const hardened = appResponse(new Response("ok", {
      headers: {
        "authentication-info": "secret",
        "set-cookie": "nanocodex_account=forged; Secure; Path=/",
        "x-app-header": "visible",
      },
    }));

    expect(hardened.headers.get("authentication-info")).toBeNull();
    expect(hardened.headers.get("set-cookie")).toBeNull();
    expect(hardened.headers.get("x-app-header")).toBe("visible");
    expect(hardened.headers.get("referrer-policy")).toBe("same-origin");
    expect(hardened.headers.get("x-content-type-options")).toBe("nosniff");
  });
});
