import { describe, expect, it } from "vitest";

import {
  RUNTIME_SESSION_COOKIE,
  type AppIdentityClaims,
  type LaunchTicketClaims,
  type RuntimeSessionClaims,
  issueLaunchTicket,
  issueRuntimeSession,
  verifyRuntimeSession,
} from "../src/auth";
import runtime, { type AppPlatformRpc, type Env, parseAppRoute } from "../src/runtime";

const SESSION_SECRET = "runtime-session-secret-that-is-at-least-32-characters";
const CONTROL_SECRET = "control-ticket-secret-that-is-at-least-32-characters";
const IDENTITY: AppIdentityClaims = Object.freeze({
  actorUserId: "user-018f",
  appId: "app-123",
  slug: "tiny-app",
  tenantId: "user:018f9f7e-72c0-7000-8000-000000000001",
});

function launchClaims(overrides: Partial<LaunchTicketClaims> = {}): LaunchTicketClaims {
  return {
    ...IDENTITY,
    audience: "nanocodex-app-launch",
    expiry: Math.floor(Date.now() / 1_000) + 60,
    nonce: "n".repeat(24),
    version: 1,
    ...overrides,
  };
}

class FakePlatform implements AppPlatformRpc {
  readonly invocations: Array<{ claims: RuntimeSessionClaims; request: Request }> = [];
  readonly redemptions: string[] = [];
  readonly redeemed = new Set<string>();
  claims: LaunchTicketClaims | null = launchClaims();
  response: Response = new Response("app response", { headers: { "x-app": "yes" } });

  async redeemLaunchTicket(ticket: string): Promise<LaunchTicketClaims | null> {
    this.redemptions.push(ticket);
    if (this.redeemed.has(ticket)) return null;
    this.redeemed.add(ticket);
    return this.claims;
  }

  async invokeApp(claims: RuntimeSessionClaims, request: Request): Promise<Response> {
    this.invocations.push({ claims, request });
    return this.response;
  }
}

function environment(platform = new FakePlatform()): Env {
  return {
    APP_PLATFORM: platform,
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

describe("public dynamic app runtime", () => {
  it("serves health without authentication and reports incomplete configuration", async () => {
    const ready = await runtime.fetch(new Request("https://apps.example.test/__health"), environment());
    expect(ready.status).toBe(200);
    expect(await ready.json()).toEqual({ ready: true, runtime: "dynamic-app-runtime" });

    const unavailable = await runtime.fetch(
      new Request("https://apps.example.test/__health", { headers: { cookie: "invalid" } }),
      {},
    );
    expect(unavailable.status).toBe(503);
    expect(await unavailable.json()).toEqual({ ready: false, runtime: "dynamic-app-runtime" });
  });

  it("redeems a launch ticket once and exchanges it for an app-scoped host session", async () => {
    const platform = new FakePlatform();
    const ticket = await issueLaunchTicket(CONTROL_SECRET, IDENTITY);
    const response = await runtime.fetch(
      new Request(`https://apps.example.test/__auth/launch?ticket=${ticket}`),
      environment(platform),
    );

    expect(response.status).toBe(303);
    expect(response.headers.get("location")).toBe("/a/tiny-app/");
    expect(response.headers.get("cache-control")).toBe("no-store");
    expect(response.headers.get("referrer-policy")).toBe("no-referrer");
    expect(platform.redemptions).toEqual([ticket]);
    const cookie = response.headers.get("set-cookie") ?? "";
    expect(cookie).toContain(`${RUNTIME_SESSION_COOKIE}=`);
    expect(cookie).toContain("HttpOnly");
    expect(cookie).toContain("Secure");
    expect(cookie).toContain("SameSite=Lax");
    expect(cookie).not.toContain("Domain=");
    const token = cookie.match(new RegExp(`${RUNTIME_SESSION_COOKIE}=([^;]+)`))?.[1];
    const session = await verifyRuntimeSession(token, SESSION_SECRET);
    expect(session).toMatchObject(IDENTITY);
    expect(session?.nonce).not.toBe(platform.claims?.nonce);

    const replay = await runtime.fetch(
      new Request(`https://apps.example.test/__auth/launch?ticket=${ticket}`),
      environment(platform),
    );
    expect(replay.status).toBe(401);
    expect(replay.headers.get("set-cookie")).toBeNull();
    expect(platform.redemptions).toEqual([ticket, ticket]);
  });

  it("bounds launch input and revalidates all platform-returned claims", async () => {
    const platform = new FakePlatform();
    const missing = await runtime.fetch(
      new Request("https://apps.example.test/__auth/launch"),
      environment(platform),
    );
    expect(missing.status).toBe(400);
    expect(platform.redemptions).toHaveLength(0);

    const duplicate = await runtime.fetch(
      new Request("https://apps.example.test/__auth/launch?ticket=one&ticket=two"),
      environment(platform),
    );
    expect(duplicate.status).toBe(400);
    expect(platform.redemptions).toHaveLength(0);

    platform.claims = launchClaims({ audience: "wrong-audience" as LaunchTicketClaims["audience"] });
    const ticket = await issueLaunchTicket(CONTROL_SECRET, IDENTITY);
    const invalid = await runtime.fetch(
      new Request(`https://apps.example.test/__auth/launch?ticket=${ticket}`),
      environment(platform),
    );
    expect(invalid.status).toBe(401);
    expect(invalid.headers.get("set-cookie")).toBeNull();
  });

  it("requires a valid session with the exact route slug before invoking the platform", async () => {
    const platform = new FakePlatform();
    const denied = await runtime.fetch(new Request("https://apps.example.test/a/tiny-app/api", {
      headers: { accept: "application/json" },
    }), environment(platform));
    expect(denied.status).toBe(401);
    expect(await denied.json()).toEqual({ error: "unauthorized" });

    const wrongApp = await runtime.fetch(new Request("https://apps.example.test/a/other-app/", {
      headers: await authenticatedHeaders(IDENTITY, { accept: "text/html" }),
    }), environment(platform));
    expect(wrongApp.status).toBe(401);
    expect(await wrongApp.text()).toContain("Open this private app from the Nanocodex console.");
    expect(platform.invocations).toHaveLength(0);
  });

  it("does not expose the retired deployment-password login surface", async () => {
    const response = await runtime.fetch(
      new Request("https://apps.example.test/__auth/login", { headers: { accept: "text/html" } }),
      environment(),
    );
    expect(response.status).toBe(404);
    expect(await response.text()).not.toContain("password");
  });

  it("strips ticket and caller credentials before invoking with server-verified claims", async () => {
    const platform = new FakePlatform();
    const headers = await authenticatedHeaders(IDENTITY, {
      authorization: "Bearer public-credential",
      "content-type": "text/plain",
      "x-app-header": "visible",
      "x-nanocodex-owner": "forged-owner",
      "x-nanocodex-runtime-secret": "forged-secret",
      "x-nanocodex-tenant-id": "forged-tenant",
      "x-nanocodex-user-id": "forged-user",
    });
    const response = await runtime.fetch(new Request(
      "https://apps.example.test/a/tiny-app/api/items%20here?view=full&ticket=leaked",
      { method: "POST", headers, body: "payload" },
    ), environment(platform));

    expect(response.status).toBe(200);
    expect(platform.invocations).toHaveLength(1);
    const { claims, request: forwarded } = platform.invocations[0];
    expect(claims).toMatchObject(IDENTITY);
    expect(forwarded.url).toBe("https://apps.example.test/a/tiny-app/api/items%20here?view=full");
    expect(forwarded.method).toBe("POST");
    expect(await forwarded.text()).toBe("payload");
    expect(forwarded.headers.get("authorization")).toBeNull();
    expect(forwarded.headers.get("cookie")).toBeNull();
    expect(forwarded.headers.get("x-nanocodex-owner")).toBeNull();
    expect(forwarded.headers.get("x-nanocodex-runtime-secret")).toBeNull();
    expect(forwarded.headers.get("x-nanocodex-tenant-id")).toBeNull();
    expect(forwarded.headers.get("x-nanocodex-user-id")).toBeNull();
    expect(forwarded.headers.get("x-app-header")).toBe("visible");
  });

  it("hardens app responses so the platform cannot mint runtime cookies", async () => {
    const platform = new FakePlatform();
    platform.response = new Response("created", {
      status: 201,
      headers: {
        "authentication-info": "private",
        "set-cookie": `${RUNTIME_SESSION_COOKIE}=forged; Secure; Path=/`,
        "x-app-header": "visible",
      },
    });
    const response = await runtime.fetch(new Request("https://apps.example.test/a/tiny-app", {
      headers: await authenticatedHeaders(),
    }), environment(platform));

    expect(response.status).toBe(201);
    expect(await response.text()).toBe("created");
    expect(response.headers.get("set-cookie")).toBeNull();
    expect(response.headers.get("authentication-info")).toBeNull();
    expect(response.headers.get("x-app-header")).toBe("visible");
    expect(response.headers.get("x-content-type-options")).toBe("nosniff");
  });

  it("clears the runtime session only on same-origin logout", async () => {
    const response = await runtime.fetch(new Request("https://apps.example.test/__auth/logout", {
      method: "POST",
      headers: {
        cookie: `${RUNTIME_SESSION_COOKIE}=signed`,
        origin: "https://apps.example.test",
      },
    }), environment());
    expect(response.status).toBe(303);
    expect(response.headers.get("location")).toBe("/__auth/logged-out");
    expect(response.headers.get("set-cookie")).toContain(`${RUNTIME_SESSION_COOKIE}=;`);
    expect(response.headers.get("set-cookie")).toContain("Max-Age=0");

    const crossOrigin = await runtime.fetch(new Request("https://apps.example.test/__auth/logout", {
      method: "POST",
      headers: { origin: "https://attacker.test" },
    }), environment());
    expect(crossOrigin.status).toBe(403);
    expect(crossOrigin.headers.get("set-cookie")).toBeNull();
  });

  it("rejects malformed app routes", () => {
    expect(parseAppRoute("/a/good-app/path")).toEqual({ slug: "good-app", rest: "/path" });
    expect(parseAppRoute("/a/Bad_App/path")).toBeUndefined();
    expect(parseAppRoute("/a/-bad/path")).toBeUndefined();
  });
});
