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

  it("prevents arbitrary code from minting host cookies or weakening browser policy", () => {
    const hardened = appResponse(new Response("ok", {
      headers: {
        "access-control-allow-credentials": "true",
        "access-control-allow-origin": "*",
        "authentication-info": "secret",
        "cache-control": "public, max-age=31536000",
        "clear-site-data": '"cookies"',
        "content-security-policy": "default-src * 'unsafe-inline' 'unsafe-eval'",
        "content-security-policy-report-only": "default-src *; report-uri https://attacker.example/report",
        "cross-origin-opener-policy": "unsafe-none",
        "cross-origin-resource-policy": "cross-origin",
        "origin-agent-cluster": "?0",
        "permissions-policy": "camera=*, microphone=*",
        "referrer-policy": "unsafe-url",
        refresh: "0; url=https://attacker.example/leak",
        "service-worker-allowed": "/",
        "set-cookie": "nanocodex_account=forged; Secure; Path=/",
        "timing-allow-origin": "*",
        "x-app-header": "visible",
        "x-frame-options": "ALLOWALL",
        "x-nanocodex-owner": "private-owner",
      },
    }));

    expect(hardened.headers.get("access-control-allow-credentials")).toBeNull();
    expect(hardened.headers.get("access-control-allow-origin")).toBeNull();
    expect(hardened.headers.get("authentication-info")).toBeNull();
    expect(hardened.headers.get("clear-site-data")).toBeNull();
    expect(hardened.headers.get("content-security-policy-report-only")).toBeNull();
    expect(hardened.headers.get("cross-origin-opener-policy")).toBeNull();
    expect(hardened.headers.get("cross-origin-resource-policy")).toBeNull();
    expect(hardened.headers.get("origin-agent-cluster")).toBeNull();
    expect(hardened.headers.get("permissions-policy")).toBeNull();
    expect(hardened.headers.get("refresh")).toBeNull();
    expect(hardened.headers.get("service-worker-allowed")).toBeNull();
    expect(hardened.headers.get("set-cookie")).toBeNull();
    expect(hardened.headers.get("timing-allow-origin")).toBeNull();
    expect(hardened.headers.get("x-nanocodex-owner")).toBeNull();
    expect(hardened.headers.get("x-app-header")).toBe("visible");
    expect(hardened.headers.get("cache-control")).toBe("no-store");
    expect(hardened.headers.get("referrer-policy")).toBe("no-referrer");
    expect(hardened.headers.get("x-content-type-options")).toBe("nosniff");
    expect(hardened.headers.get("x-frame-options")).toBe("DENY");
  });

  it("overwrites generated CSP while allowing only bundled inline execution", () => {
    const hardened = appResponse(new Response("ok", {
      headers: {
        "content-security-policy": "default-src *",
      },
    }));

    const policy = hardened.headers.get("content-security-policy");
    expect(policy).toBe([
      "default-src 'none'",
      "script-src 'unsafe-inline'",
      "script-src-attr 'none'",
      "style-src 'unsafe-inline'",
      "img-src data:",
      "worker-src 'none'",
      "connect-src 'none'",
      "form-action 'none'",
      "frame-src 'none'",
      "object-src 'none'",
      "base-uri 'none'",
      "frame-ancestors 'none'",
    ].join("; "));
    expect(policy).not.toContain("default-src *");
    expect(policy).not.toContain("unsafe-eval");
  });
});
