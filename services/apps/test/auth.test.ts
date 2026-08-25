import { describe, expect, it } from "vitest";

import {
  FRAME_SESSION_AUDIENCE,
  LAUNCH_INTENT_AUDIENCE,
  LAUNCH_INTENT_TTL_SECONDS,
  LAUNCH_TICKET_AUDIENCE,
  LAUNCH_TICKET_TTL_SECONDS,
  RUNTIME_SESSION_AUDIENCE,
  RUNTIME_SESSION_COOKIE,
  RUNTIME_SESSION_TTL_SECONDS,
  cookieValue,
  expiredRuntimeSessionCookie,
  frameSessionCookie,
  frameSessionCookieName,
  isSameOriginPost,
  issueFrameSession,
  issueLaunchIntent,
  issueLaunchTicket,
  issueRuntimeSession,
  runtimeSessionCookie,
  validateLaunchTicketClaims,
  validateRuntimeSessionClaims,
  verifyFrameSession,
  verifyLaunchIntent,
  verifyLaunchTicket,
  verifyRuntimeSession,
} from "../src/auth";

const CONTROL_SECRET = "control-ticket-secret-that-is-at-least-32-characters";
const RUNTIME_SECRET = "runtime-session-secret-that-is-at-least-32-characters";
const IDENTITY = Object.freeze({
  actorUserId: "user-018f",
  appId: "app-123",
  slug: "tiny-app",
  tenantId: "user:018f9f7e-72c0-7000-8000-000000000001",
});
const TRANSACTION = "transaction-nonce-12345678";

describe("dynamic app authentication", () => {
  it("issues bounded, audience-specific launch ticket claims", async () => {
    const now = Date.UTC(2026, 7, 24, 12);
    const ticket = await issueLaunchTicket(CONTROL_SECRET, IDENTITY, TRANSACTION, now);
    const claims = await verifyLaunchTicket(ticket, CONTROL_SECRET, now);

    expect(claims).toMatchObject({
      ...IDENTITY,
      audience: LAUNCH_TICKET_AUDIENCE,
      expiry: Math.floor(now / 1_000) + LAUNCH_TICKET_TTL_SECONDS,
      nonce: expect.stringMatching(/^[A-Za-z0-9_-]{24}$/),
      transaction: TRANSACTION,
      version: 1,
    });
    expect(await verifyLaunchTicket(ticket, "x".repeat(32), now)).toBeUndefined();
    expect(await verifyRuntimeSession(ticket, CONTROL_SECRET, now)).toBeUndefined();
    expect(await verifyLaunchTicket(ticket, CONTROL_SECRET, now + LAUNCH_TICKET_TTL_SECONDS * 1_000))
      .toBeUndefined();
  });

  it("rejects tampered, noncanonical, oversized, and out-of-policy claims", async () => {
    const now = Date.UTC(2026, 7, 24, 12);
    const ticket = await issueLaunchTicket(CONTROL_SECRET, IDENTITY, TRANSACTION, now);
    const [payload, signature] = ticket.split(".");
    const tampered = `${payload}.${signature[0] === "a" ? "b" : "a"}${signature.slice(1)}`;
    expect(await verifyLaunchTicket(tampered, CONTROL_SECRET, now)).toBeUndefined();
    expect(await verifyLaunchTicket("x".repeat(2_049), CONTROL_SECRET, now)).toBeUndefined();

    const valid = {
      ...IDENTITY,
      audience: LAUNCH_TICKET_AUDIENCE,
      expiry: Math.floor(now / 1_000) + 30,
      nonce: "n".repeat(24),
      transaction: TRANSACTION,
      version: 1,
    } as const;
    expect(validateLaunchTicketClaims(valid, now)).toEqual(valid);
    expect(validateLaunchTicketClaims({ ...valid, tenantId: "t".repeat(129) }, now)).toBeUndefined();
    expect(validateLaunchTicketClaims({ ...valid, slug: "Wrong_Slug" }, now)).toBeUndefined();
    expect(validateLaunchTicketClaims({ ...valid, expiry: valid.expiry + 31 }, now)).toBeUndefined();
    expect(validateLaunchTicketClaims({ ...valid, extra: "claim" }, now)).toBeUndefined();
  });

  it("separates account launch intents from browser-bound launch tickets", async () => {
    const now = Date.UTC(2026, 7, 24, 12);
    const intent = await issueLaunchIntent(CONTROL_SECRET, IDENTITY, now);
    expect(await verifyLaunchIntent(intent, CONTROL_SECRET, now)).toMatchObject({
      ...IDENTITY,
      audience: LAUNCH_INTENT_AUDIENCE,
      expiry: Math.floor(now / 1_000) + LAUNCH_INTENT_TTL_SECONDS,
    });
    expect(await verifyLaunchTicket(intent, CONTROL_SECRET, now)).toBeUndefined();
  });

  it("mints a distinct runtime-only session for the exact app identity", async () => {
    const now = Date.UTC(2026, 7, 24, 12);
    const token = await issueRuntimeSession(RUNTIME_SECRET, IDENTITY, now);
    const claims = await verifyRuntimeSession(token, RUNTIME_SECRET, now);

    expect(claims).toMatchObject({
      ...IDENTITY,
      audience: RUNTIME_SESSION_AUDIENCE,
      expiry: Math.floor(now / 1_000) + RUNTIME_SESSION_TTL_SECONDS,
      nonce: expect.stringMatching(/^[A-Za-z0-9_-]{24}$/),
      version: 1,
    });
    expect(validateRuntimeSessionClaims(claims, now)).toEqual(claims);
    expect(await verifyRuntimeSession(token, CONTROL_SECRET, now)).toBeUndefined();
    expect(await verifyRuntimeSession(token, RUNTIME_SECRET, now + RUNTIME_SESSION_TTL_SECONDS * 1_000))
      .toBeUndefined();
  });

  it("uses a separate signed audience for opaque app frames", async () => {
    const now = Date.UTC(2026, 7, 24, 12);
    const token = await issueFrameSession(RUNTIME_SECRET, IDENTITY, TRANSACTION, now);
    const claims = await verifyFrameSession(token, RUNTIME_SECRET, now);
    expect(claims).toMatchObject({
      ...IDENTITY,
      audience: FRAME_SESSION_AUDIENCE,
      transaction: TRANSACTION,
    });
    expect(await verifyRuntimeSession(token, RUNTIME_SECRET, now)).toBeUndefined();
  });

  it("allows the opaque app frame to send only its path-scoped session cookie", () => {
    const header = frameSessionCookie(IDENTITY.appId, TRANSACTION);
    expect(header).toContain(`${frameSessionCookieName(IDENTITY.appId)}=${TRANSACTION}`);
    expect(header).toContain("Path=/__frame/");
    expect(header).toContain("HttpOnly");
    expect(header).toContain("Secure");
    expect(header).toContain("SameSite=None");
    expect(header).not.toContain("Domain=");
  });

  it("marks runtime cookies host-only and clears only the runtime cookie", async () => {
    const token = await issueRuntimeSession(RUNTIME_SECRET, IDENTITY);
    const header = runtimeSessionCookie(token, IDENTITY.appId);
    expect(header).toContain(`${RUNTIME_SESSION_COOKIE}=${token}`);
    expect(header).toContain(`Path=/a/${IDENTITY.appId}/`);
    expect(header).toContain("HttpOnly");
    expect(header).toContain("Secure");
    expect(header).toContain("SameSite=Lax");
    expect(header).not.toContain("Domain=");
    expect(expiredRuntimeSessionCookie(IDENTITY.appId)).toContain(`${RUNTIME_SESSION_COOKIE}=;`);
    expect(expiredRuntimeSessionCookie(IDENTITY.appId)).toContain("Max-Age=0");
  });

  it("parses only the exact cookie name", () => {
    const request = new Request("https://apps.example.test/", {
      headers: {
        cookie: `other=1; ${RUNTIME_SESSION_COOKIE}=signed; suffix_${RUNTIME_SESSION_COOKIE}=bad`,
      },
    });
    expect(cookieValue(request, RUNTIME_SESSION_COOKIE)).toBe("signed");
  });

  it("accepts logout mutations only from the exact runtime origin", () => {
    expect(isSameOriginPost(new Request("https://apps.example.test/__auth/logout", {
      method: "POST",
      headers: { origin: "https://apps.example.test" },
    }))).toBe(true);
    expect(isSameOriginPost(new Request("https://apps.example.test/__auth/logout", {
      method: "POST",
      headers: { origin: "https://attacker.test" },
    }))).toBe(false);
    expect(isSameOriginPost(new Request("https://apps.example.test/__auth/logout", {
      method: "POST",
    }))).toBe(false);
  });
});
