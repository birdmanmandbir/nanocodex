import {
  LAUNCH_TRANSACTION_COOKIE,
  RUNTIME_SESSION_COOKIE,
  type FrameSessionClaims,
  type LaunchTicketClaims,
  cookieValue,
  createLaunchTransaction,
  expiredLaunchTransactionCookie,
  expiredRuntimeSessionCookie,
  frameSessionCookie,
  frameSessionCookieName,
  issueFrameSession,
  isSameOriginPost,
  issueRuntimeSession,
  launchTransactionCookie,
  runtimeSessionCookie,
  validateLaunchTicketClaims,
  verifyFrameSession,
  verifyRuntimeSession,
} from "./auth";
import { appRequest, appResponse } from "./boundary";

const MAX_LAUNCH_TICKET_BYTES = 2 * 1024;
const MAX_APP_REQUEST_BYTES = 1024 * 1024;
const MAX_APP_RESPONSE_BYTES = 2 * 1024 * 1024;
const APP_ID_SOURCE = "[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}";
const SLUG = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;
const APP_ROUTE = new RegExp(`^/a/(${APP_ID_SOURCE})/([^/]+)(/.*)?$`);
const FRAME_ROUTE = new RegExp(
  `^/__frame/([A-Za-z0-9_-]+\\.[A-Za-z0-9_-]{43})/a/(${APP_ID_SOURCE})/([^/]+)(/.*)?$`,
);

export interface AppPlatformRpc {
  redeemLaunchTicket(ticket: string, transaction: string): Promise<LaunchTicketClaims | null>;
  invokeApp(frameClaims: FrameSessionClaims, request: Request, publicPrefix: string): Promise<Response>;
}

export interface Env {
  APP_PLATFORM?: AppPlatformRpc;
  MANAGED_ORIGIN?: string;
  RUNTIME_SESSION_SECRET?: string;
}

export type AppRoute = Readonly<{
  appId: string;
  rest: string;
  slug: string;
}>;

type FrameRoute = AppRoute & Readonly<{ token: string }>;

type ConfiguredEnv = Env & {
  APP_PLATFORM: AppPlatformRpc;
  MANAGED_ORIGIN: string;
  RUNTIME_SESSION_SECRET: string;
};

export const runtime = {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname === "/__health") {
      if (request.method !== "GET") return json({ error: "method_not_allowed" }, 405);
      const ready = configured(env);
      return json({ ready, runtime: "dynamic-app-runtime" }, ready ? 200 : 503);
    }
    if (url.pathname === "/favicon.ico") {
      if (request.method !== "GET") return json({ error: "method_not_allowed" }, 405);
      return new Response(null, {
        status: 204,
        headers: { "cache-control": "public, max-age=86400" },
      });
    }

    if (!configured(env)) return failurePage("App authentication is not configured.", 503);

    if (url.pathname === "/__auth/begin") {
      if (request.method !== "GET") return json({ error: "method_not_allowed" }, 405);
      return beginLaunch(url, env);
    }
    if (url.pathname === "/__auth/launch") {
      if (request.method !== "GET") return json({ error: "method_not_allowed" }, 405);
      return launch(request, url, env);
    }
    if (url.pathname === "/__auth/logged-out") {
      if (request.method !== "GET") return json({ error: "method_not_allowed" }, 405);
      return failurePage("You are signed out.", 200);
    }

    const frame = parseFrameRoute(url.pathname);
    if (frame) return invokeFrame(request, env, frame);

    const route = parseAppRoute(url.pathname);
    if (!route) return failurePage("The requested app route does not exist.", 404);

    const sessionClaims = await verifyRuntimeSession(
      cookieValue(request, RUNTIME_SESSION_COOKIE),
      env.RUNTIME_SESSION_SECRET,
    );
    if (!sessionClaims || sessionClaims.appId !== route.appId || sessionClaims.slug !== route.slug) {
      if ((request.method === "GET" || request.method === "HEAD") && acceptsHtml(request)) {
        return failurePage("Open this private app from the Nanocodex console.", 401);
      }
      return json({ error: "unauthorized" }, 401);
    }

    if (route.rest === "/__host/logout") {
      if (request.method !== "POST") return json({ error: "method_not_allowed" }, 405);
      if (!isSameOriginPost(request)) return json({ error: "forbidden" }, 403);
      return new Response(null, {
        status: 303,
        headers: {
          "cache-control": "no-store",
          location: "/__auth/logged-out",
          "referrer-policy": "no-referrer",
          "set-cookie": expiredRuntimeSessionCookie(route.appId),
        },
      });
    }
    if ((request.method !== "GET" && request.method !== "HEAD") || !acceptsHtml(request)) {
      return json({ error: "app_frame_required" }, 409);
    }

    const frameToken = await issueFrameSession(
      env.RUNTIME_SESSION_SECRET,
      sessionClaims,
      sessionClaims.nonce,
    );
    return hostPage(route, frameToken, sessionClaims.nonce, request.method === "HEAD");
  },
} satisfies ExportedHandler<Env>;

export default runtime;

export function parseAppRoute(pathname: string): AppRoute | undefined {
  const match = APP_ROUTE.exec(pathname);
  if (!match || !SLUG.test(match[2])) return undefined;
  return { appId: match[1], slug: match[2], rest: match[3] ?? "" };
}

function parseFrameRoute(pathname: string): FrameRoute | undefined {
  const match = FRAME_ROUTE.exec(pathname);
  if (!match || match[1].length > MAX_LAUNCH_TICKET_BYTES || !SLUG.test(match[3])) return undefined;
  return { token: match[1], appId: match[2], slug: match[3], rest: match[4] ?? "" };
}

function beginLaunch(url: URL, env: ConfiguredEnv): Response {
  const intents = url.searchParams.getAll("intent");
  const workspaces = url.searchParams.getAll("workspace");
  if (intents.length !== 1 || !isBoundedLaunchTicket(intents[0])
    || workspaces.length !== 1 || !validWorkspace(workspaces[0])) {
    return failurePage("This app launch request is invalid.", 400);
  }
  const transaction = createLaunchTransaction();
  const target = new URL("/apps/api/launch/complete", env.MANAGED_ORIGIN);
  target.searchParams.set("intent", intents[0]);
  target.searchParams.set("transaction", transaction);
  target.searchParams.set("workspace", workspaces[0]);
  return new Response(null, {
    status: 303,
    headers: {
      "cache-control": "no-store",
      location: target.href,
      "referrer-policy": "no-referrer",
      "set-cookie": launchTransactionCookie(transaction),
    },
  });
}

function validWorkspace(value: string | undefined): boolean {
  return value === "personal"
    || (typeof value === "string"
      && /^team:[0-9a-f]{64}$/.test(value));
}

async function launch(request: Request, url: URL, env: ConfiguredEnv): Promise<Response> {
  const tickets = url.searchParams.getAll("ticket");
  if (tickets.length !== 1 || !isBoundedLaunchTicket(tickets[0])) {
    return failurePage("This app launch link is invalid.", 400);
  }
  const transaction = cookieValue(request, LAUNCH_TRANSACTION_COOKIE);
  if (!transaction || !/^[A-Za-z0-9_-]{22,64}$/.test(transaction)) {
    return failurePage("This app launch link is not valid in this browser.", 401);
  }

  let redeemed: LaunchTicketClaims | null;
  try {
    redeemed = await env.APP_PLATFORM.redeemLaunchTicket(tickets[0], transaction);
  } catch {
    console.error(JSON.stringify({ type: "dynamic_app_runtime.redeem_failed" }));
    return failurePage("This app launch link is invalid or has expired.", 401);
  }
  const claims = validateLaunchTicketClaims(redeemed);
  if (!claims || claims.transaction !== transaction) {
    return failurePage("This app launch link is invalid or has expired.", 401);
  }

  const token = await issueRuntimeSession(env.RUNTIME_SESSION_SECRET, claims);
  const headers = new Headers({
    "cache-control": "no-store",
    location: `/a/${claims.appId}/${claims.slug}/`,
    "referrer-policy": "no-referrer",
  });
  headers.append("set-cookie", runtimeSessionCookie(token, claims.appId));
  headers.append("set-cookie", expiredLaunchTransactionCookie());
  return new Response(null, {
    status: 303,
    headers,
  });
}

async function invokeFrame(request: Request, env: ConfiguredEnv, route: FrameRoute): Promise<Response> {
  const claims = await verifyFrameSession(route.token, env.RUNTIME_SESSION_SECRET);
  if (!claims || claims.appId !== route.appId || claims.slug !== route.slug) {
    return json({ error: "unauthorized" }, 401);
  }
  if (request.headers.get("sec-fetch-dest") === "document") {
    return failurePage("Open this private app through its Nanocodex app window.", 403);
  }
  if (request.method === "OPTIONS") return framePreflight(request);
  if (cookieValue(request, frameSessionCookieName(route.appId)) !== claims.transaction) {
    return json({ error: "unauthorized" }, 401);
  }
  const publicPrefix = `/__frame/${route.token}/a/${route.appId}/${route.slug}`;
  const sanitizedUrl = new URL(request.url);
  sanitizedUrl.pathname = `/a/${route.appId}/${route.slug}${route.rest}`;
  sanitizedUrl.searchParams.delete("ticket");
  const sanitized = appRequest(request);
  const requestBody = await boundedBody(sanitized.body, MAX_APP_REQUEST_BYTES);
  if (requestBody === undefined) {
    return await frameResponse(json({ error: "request_too_large" }, 413), new URL(request.url).origin);
  }
  const forwarded = new Request(sanitizedUrl, new Request(sanitized, {
    ...(requestBody.byteLength > 0 ? { body: arrayBuffer(requestBody) } : {}),
  }));
  try {
    const response = await env.APP_PLATFORM.invokeApp(claims, forwarded, publicPrefix);
    return await frameResponse(appResponse(response), new URL(request.url).origin);
  } catch {
    console.error(JSON.stringify({
      type: "dynamic_app_runtime.invoke_failed",
      app_id: claims.appId,
      app_slug: route.slug,
      tenant_id: claims.tenantId,
    }));
    return await frameResponse(json({ error: "app_runtime_unavailable" }, 502), new URL(request.url).origin);
  }
}

function framePreflight(request: Request): Response {
  const requested = request.headers.get("access-control-request-headers") ?? "";
  const allowed = requested.split(",").map((header) => header.trim().toLowerCase()).filter(Boolean);
  if (allowed.some((header) => !/^(?:content-type|idempotency-key|accept)$/.test(header))) {
    return json({ error: "cors_header_denied" }, 403);
  }
  return new Response(null, {
    status: 204,
    headers: {
      "access-control-allow-headers": allowed.join(", "),
      "access-control-allow-credentials": "true",
      "access-control-allow-methods": "GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS",
      "access-control-allow-origin": "null",
      "access-control-max-age": "600",
      "cache-control": "no-store",
      vary: "Origin",
    },
  });
}

async function frameResponse(response: Response, runtimeOrigin: string): Promise<Response> {
  const headers = new Headers(response.headers);
  const hadBody = response.body !== null;
  const source = runtimeOrigin.replaceAll(";", "");
  headers.set("access-control-allow-origin", "null");
  headers.set("access-control-allow-credentials", "true");
  headers.set("content-security-policy", [
    "default-src 'none'",
    `script-src 'unsafe-inline' ${source}`,
    "script-src-attr 'none'",
    `style-src 'unsafe-inline' ${source}`,
    `img-src data: blob: ${source}`,
    `font-src data: ${source}`,
    `connect-src ${source}`,
    "worker-src 'none'",
    `form-action ${source}`,
    "frame-src 'none'",
    "object-src 'none'",
    "base-uri 'none'",
    `frame-ancestors ${source}`,
    `navigate-to ${source}`,
    "sandbox allow-forms allow-modals allow-scripts",
  ].join("; "));
  headers.set("cross-origin-resource-policy", "same-site");
  headers.set("permissions-policy", "camera=(), microphone=(), geolocation=(), payment=(), usb=()");
  headers.set("vary", "Origin");
  headers.set("x-frame-options", "SAMEORIGIN");
  const declaredLength = Number(headers.get("content-length") ?? "0");
  if (!Number.isFinite(declaredLength) || declaredLength < 0 || declaredLength > MAX_APP_RESPONSE_BYTES) {
    await response.body?.cancel("app response exceeds limit").catch(() => undefined);
    return frameResponse(json({ error: "response_too_large" }, 502), runtimeOrigin);
  }
  const bytes = await boundedBody(response.body, MAX_APP_RESPONSE_BYTES);
  if (bytes === undefined) return frameResponse(json({ error: "response_too_large" }, 502), runtimeOrigin);
  const contentType = headers.get("content-type")?.toLowerCase() ?? "";
  if (!contentType.includes("text/html") || bytes.byteLength === 0) {
    const permitsBody = response.status !== 101 && response.status !== 204
      && response.status !== 205 && response.status !== 304;
    if (hadBody && permitsBody) headers.set("content-length", String(bytes.byteLength));
    else headers.delete("content-length");
    return new Response(hadBody && permitsBody ? arrayBuffer(bytes) : null, {
      headers,
      status: response.status,
      statusText: response.statusText,
    });
  }
  const bootstrap = `<script>(()=>{const f=window.fetch.bind(window);window.fetch=(i,n)=>{const u=new URL(typeof i==="string"||i instanceof URL?i:i.url,location.href);return f(i,u.origin===${JSON.stringify(source)}?{...n,credentials:"include"}:n)}})();</script>`;
  let html: string;
  try {
    html = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    return frameResponse(json({ error: "invalid_html" }, 502), runtimeOrigin);
  }
  const body = /<head(?:\s[^>]*)?>/i.test(html)
    ? html.replace(/<head(?:\s[^>]*)?>/i, (head) => `${head}${bootstrap}`)
    : `${bootstrap}${html}`;
  headers.delete("content-length");
  return new Response(body, { headers, status: response.status, statusText: response.statusText });
}

async function boundedBody(
  stream: ReadableStream<Uint8Array> | null,
  maxBytes: number,
): Promise<Uint8Array | undefined> {
  if (!stream) return new Uint8Array();
  const reader = stream.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  try {
    while (true) {
      const next = await reader.read();
      if (next.done) break;
      total += next.value.byteLength;
      if (total > maxBytes) {
        await reader.cancel("body exceeds limit").catch(() => undefined);
        return undefined;
      }
      chunks.push(next.value);
    }
  } catch {
    return undefined;
  } finally {
    reader.releaseLock();
  }
  const body = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return body;
}

function arrayBuffer(bytes: Uint8Array): ArrayBuffer {
  const copy = new Uint8Array(bytes.byteLength);
  copy.set(bytes);
  return copy.buffer;
}

function hostPage(route: AppRoute, frameToken: string, frameTransaction: string, head: boolean): Response {
  const framePath = `/__frame/${frameToken}/a/${route.appId}/${route.slug}${route.rest || "/"}`;
  const html = `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>${escapeHtml(route.slug)} · Nanocodex Apps</title><style>:root{color-scheme:dark;font-family:Inter,ui-sans-serif,system-ui,sans-serif}*{box-sizing:border-box}body{margin:0;height:100vh;display:grid;grid-template-rows:48px 1fr;background:#09090b;color:#fafafa}header{display:flex;align-items:center;justify-content:space-between;padding:0 16px;border-bottom:1px solid #27272a;background:#111113}strong{font-size:14px;letter-spacing:-.01em}form{margin:0}button{border:1px solid #3f3f46;border-radius:8px;padding:7px 11px;background:#18181b;color:#fafafa;font:inherit;cursor:pointer}iframe{width:100%;height:100%;border:0;background:#fff}</style></head><body><header><strong>${escapeHtml(route.slug)}</strong><form action="/a/${route.appId}/${route.slug}/__host/logout" method="post"><button type="submit">Close private session</button></form></header><iframe title="${escapeHtml(route.slug)}" src="${framePath}" sandbox="allow-forms allow-modals allow-scripts"></iframe></body></html>`;
  return new Response(head ? null : html, {
    status: 200,
    headers: {
      "cache-control": "no-store",
      "content-security-policy": "default-src 'none'; style-src 'unsafe-inline'; frame-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
      "content-type": "text/html; charset=utf-8",
      "referrer-policy": "no-referrer",
      "set-cookie": frameSessionCookie(route.appId, frameTransaction),
      "x-content-type-options": "nosniff",
    },
  });
}

function isBoundedLaunchTicket(value: string): boolean {
  return value.length <= MAX_LAUNCH_TICKET_BYTES
    && /^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]{43}$/.test(value);
}

function configured(env: Env): env is ConfiguredEnv {
  return typeof env.APP_PLATFORM?.redeemLaunchTicket === "function"
    && typeof env.APP_PLATFORM.invokeApp === "function"
    && typeof env.MANAGED_ORIGIN === "string"
    && /^https:\/\/[^/]+$/.test(env.MANAGED_ORIGIN)
    && typeof env.RUNTIME_SESSION_SECRET === "string"
    && new TextEncoder().encode(env.RUNTIME_SESSION_SECRET).byteLength >= 32
    && new TextEncoder().encode(env.RUNTIME_SESSION_SECRET).byteLength <= 4 * 1024;
}

function acceptsHtml(request: Request): boolean {
  return request.headers.get("accept")?.includes("text/html") ?? false;
}

function json(body: unknown, status: number): Response {
  return Response.json(body, {
    status,
    headers: {
      "cache-control": "no-store",
      "referrer-policy": "no-referrer",
      "x-content-type-options": "nosniff",
    },
  });
}

function failurePage(message: string, status: number): Response {
  return new Response(`<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Nanocodex Apps</title><style>:root{color-scheme:dark;font-family:Inter,ui-sans-serif,system-ui,sans-serif}*{box-sizing:border-box}body{margin:0;min-height:100vh;display:grid;place-items:center;background:#09090b;color:#fafafa}main{width:min(460px,calc(100% - 32px));padding:40px;border:1px solid #27272a;border-radius:22px;background:#111113}.eyebrow{color:#a1a1aa;font-size:12px;font-weight:700;letter-spacing:.14em;text-transform:uppercase}h1{margin:12px 0;font-size:38px;letter-spacing:-.045em;line-height:1.05}p{color:#a1a1aa;line-height:1.6}@media(max-width:520px){main{padding:28px}h1{font-size:32px}}</style></head><body><main><div class="eyebrow">Private Nanocodex app</div><h1>${escapeHtml(message)}</h1><p>Return to the Nanocodex console to open an app.</p></main></body></html>`, {
    status,
    headers: {
      "cache-control": "no-store",
      "content-security-policy": "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; frame-ancestors 'none'",
      "content-type": "text/html; charset=utf-8",
      "referrer-policy": "no-referrer",
      "x-content-type-options": "nosniff",
    },
  });
}

function escapeHtml(value: string): string {
  return value.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;").replaceAll("'", "&#39;");
}
