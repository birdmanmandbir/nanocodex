const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });

export const RUNTIME_SESSION_COOKIE = "__Host-nanocodex_app_runtime";
export const LAUNCH_TICKET_AUDIENCE = "nanocodex-app-launch";
export const RUNTIME_SESSION_AUDIENCE = "nanocodex-app-runtime";
export const LAUNCH_TICKET_TTL_SECONDS = 60;
export const RUNTIME_SESSION_TTL_SECONDS = 12 * 60 * 60;

const MAX_TOKEN_BYTES = 2 * 1024;
const MAX_SECRET_BYTES = 4 * 1024;
const IDENTIFIER = /^[A-Za-z0-9](?:[A-Za-z0-9._:-]{0,126}[A-Za-z0-9])?$/;
const USER_TENANT = /^user:[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const TEAM_TENANT = /^team:[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/;
const SLUG = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;
const NONCE = /^[A-Za-z0-9_-]{22,64}$/;
const SIGNATURE = /^[A-Za-z0-9_-]{43}$/;

export type AppIdentityClaims = Readonly<{
  actorUserId: string;
  appId: string;
  slug: string;
  tenantId: string;
}>;

export type LaunchTicketClaims = Readonly<AppIdentityClaims & {
  audience: typeof LAUNCH_TICKET_AUDIENCE;
  expiry: number;
  nonce: string;
  version: 1;
}>;

export type RuntimeSessionClaims = Readonly<AppIdentityClaims & {
  audience: typeof RUNTIME_SESSION_AUDIENCE;
  expiry: number;
  nonce: string;
  version: 1;
}>;

export async function issueLaunchTicket(
  secret: string,
  identity: AppIdentityClaims,
  now = Date.now(),
): Promise<string> {
  const claims = claimsFor(
    LAUNCH_TICKET_AUDIENCE,
    identity,
    Math.floor(now / 1_000) + LAUNCH_TICKET_TTL_SECONDS,
  );
  if (!validateLaunchTicketClaims(claims, now)) throw new TypeError("invalid launch ticket claims");
  return issueClaimsToken(claims, secret);
}

export async function verifyLaunchTicket(
  token: string | undefined,
  secret: string,
  now = Date.now(),
): Promise<LaunchTicketClaims | undefined> {
  return verifyClaimsToken(token, secret, now, validateLaunchTicketClaims);
}

export async function issueRuntimeSession(
  secret: string,
  identity: AppIdentityClaims,
  now = Date.now(),
): Promise<string> {
  const claims = claimsFor(
    RUNTIME_SESSION_AUDIENCE,
    identity,
    Math.floor(now / 1_000) + RUNTIME_SESSION_TTL_SECONDS,
  );
  if (!validateRuntimeSessionClaims(claims, now)) throw new TypeError("invalid runtime session claims");
  return issueClaimsToken(claims, secret);
}

export async function verifyRuntimeSession(
  token: string | undefined,
  secret: string,
  now = Date.now(),
): Promise<RuntimeSessionClaims | undefined> {
  return verifyClaimsToken(token, secret, now, validateRuntimeSessionClaims);
}

export function validateLaunchTicketClaims(
  value: unknown,
  now = Date.now(),
): LaunchTicketClaims | undefined {
  return validateClaims(value, LAUNCH_TICKET_AUDIENCE, LAUNCH_TICKET_TTL_SECONDS, now);
}

export function validateRuntimeSessionClaims(
  value: unknown,
  now = Date.now(),
): RuntimeSessionClaims | undefined {
  return validateClaims(value, RUNTIME_SESSION_AUDIENCE, RUNTIME_SESSION_TTL_SECONDS, now);
}

export function cookieValue(request: Request, name: string): string | undefined {
  const cookie = request.headers.get("cookie");
  if (!cookie) return undefined;
  for (const item of cookie.split(";")) {
    const separator = item.indexOf("=");
    if (separator < 0 || item.slice(0, separator).trim() !== name) continue;
    return item.slice(separator + 1).trim();
  }
  return undefined;
}

export function runtimeSessionCookie(token: string): string {
  return hostCookie(RUNTIME_SESSION_COOKIE, token, RUNTIME_SESSION_TTL_SECONDS);
}

export function expiredRuntimeSessionCookie(): string {
  return expiredHostCookie(RUNTIME_SESSION_COOKIE);
}

export function isSameOriginPost(request: Request): boolean {
  if (request.method !== "POST") return false;
  const origin = request.headers.get("origin");
  if (!origin) return false;
  return origin === new URL(request.url).origin;
}

function claimsFor<T extends typeof LAUNCH_TICKET_AUDIENCE | typeof RUNTIME_SESSION_AUDIENCE>(
  audience: T,
  identity: AppIdentityClaims,
  expiry: number,
): Readonly<AppIdentityClaims & { audience: T; expiry: number; nonce: string; version: 1 }> {
  return {
    actorUserId: identity.actorUserId,
    appId: identity.appId,
    audience,
    expiry,
    nonce: randomNonce(),
    slug: identity.slug,
    tenantId: identity.tenantId,
    version: 1,
  };
}

function validateClaims<T extends typeof LAUNCH_TICKET_AUDIENCE | typeof RUNTIME_SESSION_AUDIENCE>(
  value: unknown,
  audience: T,
  maxTtlSeconds: number,
  now: number,
): Readonly<AppIdentityClaims & { audience: T; expiry: number; nonce: string; version: 1 }> | undefined {
  if (!isRecord(value)) return undefined;
  const keys = Object.keys(value).sort();
  const expectedKeys = ["actorUserId", "appId", "audience", "expiry", "nonce", "slug", "tenantId", "version"];
  if (keys.length !== expectedKeys.length || keys.some((key, index) => key !== expectedKeys[index])) {
    return undefined;
  }
  const nowSeconds = Math.floor(now / 1_000);
  if (value.version !== 1 || value.audience !== audience
    || typeof value.expiry !== "number" || !Number.isSafeInteger(value.expiry)
    || value.expiry <= nowSeconds || value.expiry > nowSeconds + maxTtlSeconds
    || !validTenantId(value.tenantId) || !validIdentifier(value.actorUserId)
    || !validIdentifier(value.appId) || typeof value.slug !== "string" || !SLUG.test(value.slug)
    || typeof value.nonce !== "string" || !NONCE.test(value.nonce)) {
    return undefined;
  }
  return {
    actorUserId: value.actorUserId,
    appId: value.appId,
    audience,
    expiry: value.expiry,
    nonce: value.nonce,
    slug: value.slug,
    tenantId: value.tenantId,
    version: 1,
  };
}

async function issueClaimsToken(
  claims: LaunchTicketClaims | RuntimeSessionClaims,
  secret: string,
): Promise<string> {
  assertSecret(secret);
  const payload = base64Url(encoder.encode(JSON.stringify(claims)));
  return `${payload}.${await sign(payload, secret)}`;
}

async function verifyClaimsToken<T>(
  token: string | undefined,
  secret: string,
  now: number,
  validate: (value: unknown, now: number) => T | undefined,
): Promise<T | undefined> {
  if (!token || token.length > MAX_TOKEN_BYTES || !validSecret(secret)) return undefined;
  const fields = token.split(".");
  if (fields.length !== 2 || !/^[A-Za-z0-9_-]+$/.test(fields[0]) || !SIGNATURE.test(fields[1])) {
    return undefined;
  }
  if (!await verifyMac(fields[0], fields[1], secret)) return undefined;
  let parsed: unknown;
  try {
    parsed = JSON.parse(decoder.decode(fromBase64Url(fields[0])));
  } catch {
    return undefined;
  }
  const claims = validate(parsed, now);
  if (!claims) return undefined;
  const canonicalPayload = base64Url(encoder.encode(JSON.stringify(claims)));
  return textEqual(fields[0], canonicalPayload) ? claims : undefined;
}

async function sign(payload: string, secret: string): Promise<string> {
  assertSecret(secret);
  const key = await hmacKey(secret, ["sign"]);
  return base64Url(new Uint8Array(await crypto.subtle.sign("HMAC", key, encoder.encode(payload))));
}

async function verifyMac(payload: string, signature: string, secret: string): Promise<boolean> {
  if (!SIGNATURE.test(signature) || !validSecret(secret)) return false;
  let bytes: Uint8Array<ArrayBuffer>;
  try {
    bytes = fromBase64Url(signature);
  } catch {
    return false;
  }
  const key = await hmacKey(secret, ["verify"]);
  return crypto.subtle.verify("HMAC", key, bytes, encoder.encode(payload));
}

async function hmacKey(secret: string, usages: KeyUsage[]): Promise<CryptoKey> {
  return crypto.subtle.importKey(
    "raw",
    encoder.encode(secret),
    { hash: "SHA-256", name: "HMAC" },
    false,
    usages,
  );
}

function textEqual(left: string, right: string): boolean {
  return bytesEqual(encoder.encode(left), encoder.encode(right));
}

function bytesEqual(left: Uint8Array, right: Uint8Array): boolean {
  let mismatch = left.length ^ right.length;
  const length = Math.max(left.length, right.length);
  for (let index = 0; index < length; index += 1) {
    mismatch |= (left[index] ?? 0) ^ (right[index] ?? 0);
  }
  return mismatch === 0;
}

function randomNonce(): string {
  const nonce = new Uint8Array(18);
  crypto.getRandomValues(nonce);
  return base64Url(nonce);
}

function validIdentifier(value: unknown): value is string {
  return typeof value === "string" && IDENTIFIER.test(value);
}

function validTenantId(value: unknown): value is string {
  return typeof value === "string" && (USER_TENANT.test(value) || TEAM_TENANT.test(value));
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function assertSecret(secret: string): void {
  if (!validSecret(secret)) throw new TypeError("signing secret must contain 32-4096 UTF-8 bytes");
}

function validSecret(secret: string): boolean {
  if (typeof secret !== "string") return false;
  const length = encoder.encode(secret).byteLength;
  return length >= 32 && length <= MAX_SECRET_BYTES;
}

function hostCookie(name: string, token: string, maxAge: number): string {
  return `${name}=${token}; Path=/; Max-Age=${maxAge}; HttpOnly; Secure; SameSite=Lax`;
}

function expiredHostCookie(name: string): string {
  return `${name}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax`;
}

function base64Url(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

function fromBase64Url(value: string): Uint8Array<ArrayBuffer> {
  if (!/^[A-Za-z0-9_-]+$/.test(value) || value.length % 4 === 1) throw new TypeError("invalid base64url");
  const padded = value.replaceAll("-", "+").replaceAll("_", "/") + "=".repeat((4 - value.length % 4) % 4);
  const binary = atob(padded);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
  return bytes;
}
