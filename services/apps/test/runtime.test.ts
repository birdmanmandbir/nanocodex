import { describe, expect, it } from "vitest";

import {
  LAUNCH_TRANSACTION_COOKIE,
  RUNTIME_SESSION_COOKIE,
  type AppIdentityClaims,
  type FrameSessionClaims,
  type LaunchTicketClaims,
  issueFrameSession,
  issueLaunchIntent,
  issueLaunchTicket,
  issueRuntimeSession,
  frameSessionCookieName,
  verifyRuntimeSession,
} from "../src/auth";
import runtime, { type AppPlatformRpc, type Env, parseAppRoute } from "../src/runtime";

const SESSION_SECRET = "runtime-session-secret-that-is-at-least-32-characters";
const CONTROL_SECRET = "control-ticket-secret-that-is-at-least-32-characters";
const TRANSACTION = "transaction-nonce-12345678";
const IDENTITY: AppIdentityClaims = Object.freeze({
  actorUserId: "user-018f",
  appId: "0198e2c4-365e-7a66-a58f-d4e5b46a7dae",
  slug: "tiny-app",
  tenantId: "user:018f9f7e-72c0-7000-8000-000000000001",
});

function launchClaims(overrides: Partial<LaunchTicketClaims> = {}): LaunchTicketClaims {
  return {
    ...IDENTITY,
    audience: "nanocodex-app-launch",
    expiry: Math.floor(Date.now() / 1_000) + 60,
    nonce: "n".repeat(24),
    transaction: TRANSACTION,
    version: 1,
    ...overrides,
  };
}

class FakePlatform implements AppPlatformRpc {
  readonly invocations: Array<{ claims: FrameSessionClaims; prefix: string; request: Request }> = [];
  readonly redemptions: Array<{ ticket: string; transaction: string }> = [];
  readonly redeemed = new Set<string>();
  claims: LaunchTicketClaims | null = launchClaims();
  response: Response = new Response("app response", { headers: { "x-app": "yes" } });

  async redeemLaunchTicket(ticket: string, transaction: string): Promise<LaunchTicketClaims | null> {
    this.redemptions.push({ ticket, transaction });
    if (this.redeemed.has(ticket) || this.claims?.transaction !== transaction) return null;
    this.redeemed.add(ticket);
    return this.claims;
  }

  async invokeApp(claims: FrameSessionClaims, request: Request, prefix: string): Promise<Response> {
    this.invocations.push({ claims, prefix, request });
    return this.response;
  }
}

function environment(platform = new FakePlatform()): Env {
  return {
    APP_PLATFORM: platform,
    MANAGED_ORIGIN: "https://nanocodex.example.test",
    RUNTIME_SESSION_SECRET: SESSION_SECRET,
  };
}

async function authenticatedHeaders(
  identity: AppIdentityClaims = IDENTITY,
  extra?: HeadersInit,
): Promise<Headers> {
  const headers = new Headers(extra);
  const token = await issueRuntimeSession(SESSION_SECRET, identity);
  headers.set("cookie", `${RUNTIME_SESSION_COOKIE}=${token}`);
  return headers;
}

async function frameUrl(rest = "/api"): Promise<string> {
  const token = await issueFrameSession(SESSION_SECRET, IDENTITY, TRANSACTION);
  return `https://apps.example.test/__frame/${token}/a/${IDENTITY.appId}/${IDENTITY.slug}${rest}`;
}

function frameCookieHeader(): string {
  return `${frameSessionCookieName(IDENTITY.appId)}=${TRANSACTION}`;
}

describe("public dynamic app runtime", () => {
  it("serves health without authentication and reports incomplete configuration", async () => {
    const ready = await runtime.fetch(new Request("https://apps.example.test/__health"), environment());
    expect(ready.status).toBe(200);
    expect(await ready.json()).toEqual({ ready: true, runtime: "dynamic-app-runtime" });

    const unavailable = await runtime.fetch(new Request("https://apps.example.test/__health"), {});
    expect(unavailable.status).toBe(503);
  });

  it("binds account authorization to a runtime-origin browser transaction", async () => {
    const platform = new FakePlatform();
    const intent = await issueLaunchIntent(CONTROL_SECRET, IDENTITY);
    const begin = await runtime.fetch(
      new Request(`https://apps.example.test/__auth/begin?intent=${intent}`),
      environment(platform),
    );
    expect(begin.status).toBe(303);
    const completion = new URL(begin.headers.get("location")!);
    expect(completion.origin).toBe("https://nanocodex.example.test");
    expect(completion.pathname).toBe("/apps/api/launch/complete");
    const transaction = completion.searchParams.get("transaction")!;
    expect(begin.headers.get("set-cookie")).toContain(`${LAUNCH_TRANSACTION_COOKIE}=${transaction}`);

    platform.claims = launchClaims({ transaction });
    const ticket = await issueLaunchTicket(CONTROL_SECRET, IDENTITY, transaction);
    const response = await runtime.fetch(new Request(
      `https://apps.example.test/__auth/launch?ticket=${ticket}`,
      { headers: { cookie: `${LAUNCH_TRANSACTION_COOKIE}=${transaction}` } },
    ), environment(platform));

    expect(response.status).toBe(303);
    expect(response.headers.get("location")).toBe(`/a/${IDENTITY.appId}/tiny-app/`);
    expect(platform.redemptions).toEqual([{ ticket, transaction }]);
    const cookies = response.headers.get("set-cookie") ?? "";
    expect(cookies).toContain(`${RUNTIME_SESSION_COOKIE}=`);
    expect(cookies).toContain(`Path=/a/${IDENTITY.appId}/`);
    expect(cookies).toContain(`${LAUNCH_TRANSACTION_COOKIE}=;`);
    const token = cookies.match(new RegExp(`${RUNTIME_SESSION_COOKIE}=([^;,]+)`))?.[1];
    expect(await verifyRuntimeSession(token, SESSION_SECRET)).toMatchObject(IDENTITY);

    const copied = await runtime.fetch(
      new Request(`https://apps.example.test/__auth/launch?ticket=${ticket}`),
      environment(platform),
    );
    expect(copied.status).toBe(401);
    expect(platform.redemptions).toHaveLength(1);
  });

  it("bounds launch input and revalidates returned transaction claims", async () => {
    const platform = new FakePlatform();
    expect((await runtime.fetch(
      new Request("https://apps.example.test/__auth/launch"), environment(platform),
    )).status).toBe(400);

    const ticket = await issueLaunchTicket(CONTROL_SECRET, IDENTITY, TRANSACTION);
    platform.claims = launchClaims({ transaction: "different-transaction-1234" });
    const invalid = await runtime.fetch(new Request(
      `https://apps.example.test/__auth/launch?ticket=${ticket}`,
      { headers: { cookie: `${LAUNCH_TRANSACTION_COOKIE}=${TRANSACTION}` } },
    ), environment(platform));
    expect(invalid.status).toBe(401);
    expect(invalid.headers.get("set-cookie")).toBeNull();
  });

  it("serves only a trusted host document for an authenticated app route", async () => {
    const platform = new FakePlatform();
    const response = await runtime.fetch(new Request(
      `https://apps.example.test/a/${IDENTITY.appId}/tiny-app/`,
      { headers: await authenticatedHeaders(IDENTITY, { accept: "text/html" }) },
    ), environment(platform));
    const html = await response.text();
    expect(response.status).toBe(200);
    expect(html).toContain("<iframe");
    expect(html).toContain('sandbox="allow-forms allow-modals allow-scripts"');
    expect(html).not.toContain("allow-same-origin");
    expect(html).toMatch(new RegExp(`/__frame/[A-Za-z0-9_.-]+/a/${IDENTITY.appId}/tiny-app/`));
    expect(response.headers.get("set-cookie")).toContain(`${frameSessionCookieName(IDENTITY.appId)}=`);
    expect(response.headers.get("set-cookie")).toContain("HttpOnly");
    expect(platform.invocations).toHaveLength(0);
  });

  it("requires the exact app-scoped host session", async () => {
    const platform = new FakePlatform();
    const denied = await runtime.fetch(new Request(
      `https://apps.example.test/a/${IDENTITY.appId}/tiny-app/api`,
      { headers: { accept: "application/json" } },
    ), environment(platform));
    expect(denied.status).toBe(401);

    const wrongApp = await runtime.fetch(new Request(
      "https://apps.example.test/a/0198e2c4-365e-7a66-a58f-d4e5b46a7daf/tiny-app/",
      { headers: await authenticatedHeaders(IDENTITY, { accept: "text/html" }) },
    ), environment(platform));
    expect(wrongApp.status).toBe(401);
    expect(platform.invocations).toHaveLength(0);
  });

  it("invokes generated code only through an opaque, app-bound frame URL", async () => {
    const platform = new FakePlatform();
    const url = await frameUrl("/api/items%20here?view=full&ticket=leaked");
    const response = await runtime.fetch(new Request(url, {
      method: "POST",
      headers: {
        authorization: "Bearer public-credential",
        cookie: `private=1; ${frameCookieHeader()}`,
        origin: "null",
        "x-app-header": "visible",
        "x-nanocodex-owner": "forged-owner",
      },
      body: "payload",
    }), environment(platform));

    expect(response.status).toBe(200);
    expect(platform.invocations).toHaveLength(1);
    const invocation = platform.invocations[0];
    expect(invocation.claims).toMatchObject(IDENTITY);
    expect(invocation.claims.audience).toBe("nanocodex-app-frame");
    expect(invocation.prefix).toMatch(new RegExp(`^/__frame/.+/a/${IDENTITY.appId}/tiny-app$`));
    expect(invocation.request.url).toBe(
      `https://apps.example.test/a/${IDENTITY.appId}/tiny-app/api/items%20here?view=full`,
    );
    expect(await invocation.request.text()).toBe("payload");
    expect(invocation.request.headers.get("authorization")).toBeNull();
    expect(invocation.request.headers.get("cookie")).toBeNull();
    expect(invocation.request.headers.get("x-nanocodex-owner")).toBeNull();
    expect(response.headers.get("access-control-allow-origin")).toBe("null");
    expect(response.headers.get("content-security-policy")).toContain("worker-src 'none'");
    expect(response.headers.get("content-security-policy")).toContain("connect-src https://apps.example.test");
    expect(response.headers.get("x-frame-options")).toBe("SAMEORIGIN");

    const copied = await runtime.fetch(new Request(url), environment(platform));
    expect(copied.status).toBe(401);
    expect(platform.invocations).toHaveLength(1);
  });

  it("preflights only the bounded headers used by generated app APIs", async () => {
    const allowed = await runtime.fetch(new Request(await frameUrl(), {
      method: "OPTIONS",
      headers: {
        origin: "null",
        "access-control-request-headers": "content-type, idempotency-key",
      },
    }), environment());
    expect(allowed.status).toBe(204);
    expect(allowed.headers.get("access-control-allow-origin")).toBe("null");

    const denied = await runtime.fetch(new Request(await frameUrl(), {
      method: "OPTIONS",
      headers: { "access-control-request-headers": "authorization" },
    }), environment());
    expect(denied.status).toBe(403);
  });

  it("strips generated origin-control headers before framing the response", async () => {
    const platform = new FakePlatform();
    platform.response = new Response("<!doctype html><head></head><body>created</body>", {
      status: 201,
      headers: {
        "access-control-allow-origin": "https://attacker.test",
        "content-type": "text/html; charset=utf-8",
        "service-worker-allowed": "/",
        "set-cookie": "forged=1; Secure; Path=/",
      },
    });
    const response = await runtime.fetch(new Request(await frameUrl(), {
      headers: { cookie: frameCookieHeader() },
    }), environment(platform));
    expect(response.status).toBe(201);
    expect(response.headers.get("set-cookie")).toBeNull();
    expect(response.headers.get("service-worker-allowed")).toBeNull();
    expect(response.headers.get("access-control-allow-origin")).toBe("null");
    expect(await response.text()).toContain('credentials:"include"');
  });

  it("clears only the exact app-scoped runtime session", async () => {
    const response = await runtime.fetch(new Request(
      `https://apps.example.test/a/${IDENTITY.appId}/tiny-app/__host/logout`,
      {
        method: "POST",
        headers: await authenticatedHeaders(IDENTITY, { origin: "https://apps.example.test" }),
      },
    ), environment());
    expect(response.status).toBe(303);
    expect(response.headers.get("set-cookie")).toContain(`Path=/a/${IDENTITY.appId}/`);

    const crossOrigin = await runtime.fetch(new Request(
      `https://apps.example.test/a/${IDENTITY.appId}/tiny-app/__host/logout`,
      {
        method: "POST",
        headers: await authenticatedHeaders(IDENTITY, { origin: "https://attacker.test" }),
      },
    ), environment());
    expect(crossOrigin.status).toBe(403);
  });

  it("rejects malformed and tenant-ambiguous app routes", () => {
    expect(parseAppRoute(`/a/${IDENTITY.appId}/good-app/path`)).toEqual({
      appId: IDENTITY.appId, slug: "good-app", rest: "/path",
    });
    expect(parseAppRoute("/a/good-app/path")).toBeUndefined();
    expect(parseAppRoute(`/a/${IDENTITY.appId}/Bad_App/path`)).toBeUndefined();
  });
});
