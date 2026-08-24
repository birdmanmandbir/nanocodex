import {
  RUNTIME_SESSION_COOKIE,
  type LaunchTicketClaims,
  type RuntimeSessionClaims,
  cookieValue,
  expiredRuntimeSessionCookie,
  isSameOriginPost,
  issueRuntimeSession,
  runtimeSessionCookie,
  validateLaunchTicketClaims,
  verifyRuntimeSession,
} from "./auth";
import { appRequest, appResponse } from "./boundary";

const MAX_LAUNCH_TICKET_BYTES = 2 * 1024;
const SLUG = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;

export interface AppPlatformRpc {
  redeemLaunchTicket(ticket: string): Promise<LaunchTicketClaims | null>;
  invokeApp(sessionClaims: RuntimeSessionClaims, request: Request): Promise<Response>;
}

export interface Env {
  APP_PLATFORM?: AppPlatformRpc;
  RUNTIME_SESSION_SECRET?: string;
}

export type AppRoute = Readonly<{
  rest: string;
  slug: string;
}>;

type ConfiguredEnv = Env & {
  APP_PLATFORM: AppPlatformRpc;
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

    if (url.pathname === "/__auth/launch") {
      if (request.method !== "GET") return json({ error: "method_not_allowed" }, 405);
      return launch(url, env);
    }
    if (url.pathname === "/__auth/logout") {
      if (request.method !== "POST") return json({ error: "method_not_allowed" }, 405);
      if (!isSameOriginPost(request)) return json({ error: "forbidden" }, 403);
      return new Response(null, {
        status: 303,
        headers: {
          "cache-control": "no-store",
          location: "/__auth/logged-out",
          "referrer-policy": "no-referrer",
          "set-cookie": expiredRuntimeSessionCookie(),
        },
      });
    }
    if (url.pathname === "/__auth/logged-out") {
      if (request.method !== "GET") return json({ error: "method_not_allowed" }, 405);
      return failurePage("You are signed out.", 200);
    }

    const route = parseAppRoute(url.pathname);
    if (!route) return failurePage("The requested app route does not exist.", 404);

    const sessionClaims = await verifyRuntimeSession(
      cookieValue(request, RUNTIME_SESSION_COOKIE),
      env.RUNTIME_SESSION_SECRET,
    );
    if (!sessionClaims || sessionClaims.slug !== route.slug) {
      if ((request.method === "GET" || request.method === "HEAD") && acceptsHtml(request)) {
        return failurePage("Open this private app from the Nanocodex console.", 401);
      }
      return json({ error: "unauthorized" }, 401);
    }

    const sanitizedUrl = new URL(request.url);
    sanitizedUrl.searchParams.delete("ticket");
    const sanitized = appRequest(request);
    const forwarded = new Request(sanitizedUrl, sanitized);

    try {
      return appResponse(await env.APP_PLATFORM.invokeApp(sessionClaims, forwarded));
    } catch (error) {
      console.error(JSON.stringify({
        type: "dynamic_app_runtime.invoke_failed",
        app_id: sessionClaims.appId,
        app_slug: route.slug,
        tenant_id: sessionClaims.tenantId,
        error: error instanceof Error ? error.message : String(error),
      }));
      return failurePage("The app runtime is unavailable.", 502);
    }
  },
} satisfies ExportedHandler<Env>;

export default runtime;

export function parseAppRoute(pathname: string): AppRoute | undefined {
  const match = /^\/a\/([^/]+)(\/.*)?$/.exec(pathname);
  if (!match || !SLUG.test(match[1])) return undefined;
  return { slug: match[1], rest: match[2] ?? "" };
}

async function launch(url: URL, env: ConfiguredEnv): Promise<Response> {
  const tickets = url.searchParams.getAll("ticket");
  if (tickets.length !== 1 || !isBoundedLaunchTicket(tickets[0])) {
    return failurePage("This app launch link is invalid.", 400);
  }

  let redeemed: LaunchTicketClaims | null;
  try {
    redeemed = await env.APP_PLATFORM.redeemLaunchTicket(tickets[0]);
  } catch (error) {
    console.error(JSON.stringify({
      type: "dynamic_app_runtime.redeem_failed",
      error: error instanceof Error ? error.message : String(error),
    }));
    return failurePage("This app launch link is invalid or has expired.", 401);
  }
  const claims = validateLaunchTicketClaims(redeemed);
  if (!claims) return failurePage("This app launch link is invalid or has expired.", 401);

  const token = await issueRuntimeSession(env.RUNTIME_SESSION_SECRET, claims);
  return new Response(null, {
    status: 303,
    headers: {
      "cache-control": "no-store",
      location: `/a/${claims.slug}/`,
      "referrer-policy": "no-referrer",
      "set-cookie": runtimeSessionCookie(token),
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
