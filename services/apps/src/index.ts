import { WorkerEntrypoint } from "cloudflare:workers";

import {
  issueLaunchTicket,
  validateRuntimeSessionClaims,
  verifyLaunchTicket,
  type LaunchTicketClaims,
  type RuntimeSessionClaims,
} from "./auth";
import { appRequest, appResponse } from "./boundary";
import { parseArtifact, type BuildArtifact } from "./builder";
import {
  AppState,
  NanocodexCapability,
  type CapabilityProps,
  type ManagedAgentService,
} from "./capability";
import type { AppGitService } from "./git";
import {
  LaunchTicketStore,
  consumeLaunchTicket,
  recordLaunchTicket,
} from "./launch-tickets";
import {
  AppRegistry,
  RegistryError,
  activateRevision,
  failJob,
  getApp,
  getJob,
  listApps,
  startJob,
  validateTenantId,
  type App,
  type AppSummary,
  type Job,
  type TenantId,
} from "./registry";
import { AppBuildWorkflow, type BuildWorkflowParams } from "./workflow";

export { AppBuildWorkflow, AppRegistry, AppState, LaunchTicketStore, NanocodexCapability };

const MAX_JSON_BYTES = 64 * 1024;
const MAX_PROMPT_CHARS = 24 * 1024;
const APP_GRANTS = Object.freeze([
  "profile:read",
  "state:read",
  "state:write",
  "ai:generate",
  "agents:run",
]);
const USER_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const APP_PATH = /^\/a\/([a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)(\/.*)?$/;

export interface Env {
  AI: Ai;
  APP_ARTIFACTS: R2Bucket;
  APP_BUILDS: Workflow<BuildWorkflowParams>;
  APP_GIT: AppGitService;
  APP_LAUNCH_TICKETS: DurableObjectNamespace<LaunchTicketStore>;
  APP_REGISTRY: DurableObjectNamespace<AppRegistry>;
  APP_STATE: DurableObjectNamespace<AppState>;
  ASSETS?: Fetcher;
  LAUNCH_TICKET_SECRET?: string;
  LOADER: WorkerLoader;
  NANOCODEX_AGENTS: ManagedAgentService;
  RUNTIME_ORIGIN?: string;
}

type AppPlatformProps = Readonly<{ clientId: "nanocodex-managed" }>;
type RuntimePlatformProps = Readonly<{ clientId: "nanocodex-app-runtime" }>;

type AppLoopbackExports = Readonly<{
  NanocodexCapability(options: { props: CapabilityProps }): Fetcher;
}>;

const health = {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname !== "/__health" || request.method !== "GET") {
      return json({ error: "not_found" }, 404);
    }
    const ready = configured(env);
    return json({ ready, runtime: "dynamic-app-control" }, ready ? 200 : 503);
  },
} satisfies ExportedHandler<Env>;

export default health;

export class AppPlatform extends WorkerEntrypoint<Env, AppPlatformProps> {
  async request(userId: string, request: Request): Promise<Response> {
    if (this.ctx.props.clientId !== "nanocodex-managed") {
      throw new Error("app platform is restricted to the managed account gateway");
    }
    if (!configured(this.env)) return json({ error: "platform_unavailable" }, 503);
    if (!USER_ID.test(userId)) return json({ error: "unauthorized" }, 401);
    const tenantId = validateTenantId(`user:${userId}`);
    const url = new URL(request.url);
    if (url.pathname !== "/apps" && !url.pathname.startsWith("/apps/")) {
      return json({ error: "not_found" }, 404);
    }
    if (request.method !== "GET" && request.method !== "HEAD") {
      if (request.headers.get("origin") !== url.origin) return json({ error: "forbidden" }, 403);
    }
    if (url.pathname.startsWith("/apps/api/")) {
      return routePlatformApi(request, this.env, url, tenantId, userId);
    }
    if (request.method === "GET" || request.method === "HEAD") {
      return serveConsoleAsset(request, this.env, url);
    }
    return json({ error: "method_not_allowed" }, 405);
  }
}

export class RuntimePlatform extends WorkerEntrypoint<Env, RuntimePlatformProps> {
  async redeemLaunchTicket(token: string): Promise<LaunchTicketClaims | null> {
    this.#requireRuntime();
    if (!configured(this.env)) return null;
    const claims = await verifyLaunchTicket(token, this.env.LAUNCH_TICKET_SECRET);
    if (!claims) return null;
    return await consumeLaunchTicket(this.env.APP_LAUNCH_TICKETS, claims) ? claims : null;
  }

  async invokeApp(claimsInput: RuntimeSessionClaims, request: Request): Promise<Response> {
    this.#requireRuntime();
    if (!configured(this.env)) return json({ error: "platform_unavailable" }, 503);
    const claims = validateRuntimeSessionClaims(claimsInput);
    if (!claims || !USER_ID.test(claims.actorUserId)) return json({ error: "unauthorized" }, 401);
    const tenantId = validateTenantId(claims.tenantId);
    const app = await getApp(this.env.APP_REGISTRY, tenantId, claims.appId);
    if (!app || app.slug !== claims.slug || app.ownerId !== tenantId) {
      return json({ error: "not_found" }, 404);
    }
    return invokeDynamicApp(request, this.env, this.ctx, claims, app);
  }

  #requireRuntime(): void {
    if (this.ctx.props.clientId !== "nanocodex-app-runtime") {
      throw new Error("runtime platform is restricted to the app runtime");
    }
  }
}

async function routePlatformApi(
  request: Request,
  env: ConfiguredEnv,
  url: URL,
  tenantId: TenantId,
  actorUserId: string,
): Promise<Response> {
  try {
    if (url.pathname === "/apps/api/apps" && request.method === "GET") {
      const apps = await listApps(env.APP_REGISTRY, tenantId);
      return json({ apps: apps.map(appView), tenant: { id: tenantId, kind: "personal" } }, 200);
    }
    if (url.pathname === "/apps/api/builds" && request.method === "POST") {
      const body = await readJson(request);
      if (body instanceof Response) return body;
      const prompt = typeof body.prompt === "string" ? body.prompt.trim() : "";
      const updateAppId = typeof body.app_id === "string" && body.app_id ? body.app_id : undefined;
      if (!prompt || prompt.length > MAX_PROMPT_CHARS) {
        return json({ error: "prompt must contain 1-24576 characters" }, 400);
      }
      const jobId = crypto.randomUUID();
      const appId = crypto.randomUUID();
      const createdAt = new Date().toISOString();
      const job = await startJob(env.APP_REGISTRY, tenantId, {
        appId,
        createdAt,
        jobId,
        prompt,
        ...(updateAppId ? { updateAppId } : {}),
      });
      const params: BuildWorkflowParams = {
        appId,
        createdAt,
        jobId,
        prompt,
        tenantId,
        ...(updateAppId ? { updateAppId } : {}),
      };
      try {
        await env.APP_BUILDS.create({
          id: jobId,
          params,
          retention: { successRetention: "30 days", errorRetention: "30 days" },
        });
      } catch (error) {
        console.error(JSON.stringify({
          type: "dynamic_app_build.start_failed",
          job_id: jobId,
          tenant_id: tenantId,
          error: errorMessage(error),
        }));
        const failed = await failJob(env.APP_REGISTRY, tenantId, {
          error: "Build workflow unavailable. Try again.",
          failedAt: new Date().toISOString(),
          jobId,
        });
        return json({ error: "build_workflow_unavailable", job: jobView(failed) }, 502);
      }
      return json({ job: jobView(job) }, 202);
    }
    const jobMatch = url.pathname.match(/^\/apps\/api\/builds\/([A-Za-z0-9._:-]+)$/);
    if (jobMatch && request.method === "GET") {
      const job = await getJob(env.APP_REGISTRY, tenantId, jobMatch[1]);
      return job ? json({ job: jobView(job) }, 200) : json({ error: "not_found" }, 404);
    }
    const launchMatch = url.pathname.match(/^\/apps\/api\/apps\/([A-Za-z0-9._:-]+)\/launch$/);
    if (launchMatch && request.method === "POST") {
      const app = await getApp(env.APP_REGISTRY, tenantId, launchMatch[1]);
      if (!app) return json({ error: "not_found" }, 404);
      const ticket = await issueLaunchTicket(env.LAUNCH_TICKET_SECRET, {
        actorUserId,
        appId: app.appId,
        slug: app.slug,
        tenantId,
      });
      const claims = await verifyLaunchTicket(ticket, env.LAUNCH_TICKET_SECRET);
      if (!claims) throw new Error("launch ticket self-verification failed");
      await recordLaunchTicket(env.APP_LAUNCH_TICKETS, claims);
      const target = new URL("/__auth/launch", env.RUNTIME_ORIGIN);
      target.searchParams.set("ticket", ticket);
      return new Response(null, {
        status: 303,
        headers: {
          "cache-control": "no-store",
          location: target.href,
          "referrer-policy": "no-referrer",
        },
      });
    }
    const activateMatch = url.pathname.match(/^\/apps\/api\/apps\/([A-Za-z0-9._:-]+)\/activate$/);
    if (activateMatch && request.method === "POST") {
      const body = await readJson(request);
      if (body instanceof Response) return body;
      if (typeof body.revision !== "string" || !/^[0-9a-f]{64}$/.test(body.revision)) {
        return json({ error: "invalid_revision" }, 400);
      }
      if (typeof body.expected_revision !== "string" || !/^[0-9a-f]{64}$/.test(body.expected_revision)) {
        return json({ error: "invalid_expected_revision" }, 400);
      }
      const app = await activateRevision(env.APP_REGISTRY, tenantId, {
        activatedAt: new Date().toISOString(),
        appId: activateMatch[1],
        expectedActiveRevisionId: body.expected_revision,
        reason: body.reason === "rollback" ? "rollback" : "activate",
        revisionId: body.revision,
      });
      return json({ app: appView(app) }, 200);
    }
    const appMatch = url.pathname.match(/^\/apps\/api\/apps\/([A-Za-z0-9._:-]+)$/);
    if (appMatch && request.method === "GET") {
      const app = await getApp(env.APP_REGISTRY, tenantId, appMatch[1]);
      return app ? json({ app: appView(app) }, 200) : json({ error: "not_found" }, 404);
    }
    return json({ error: "not_found" }, 404);
  } catch (error) {
    if (error instanceof RegistryError) return json({ error: error.code, message: error.message }, error.status);
    console.error(JSON.stringify({
      type: "dynamic_app_platform.failed",
      path: url.pathname,
      tenant_id: tenantId,
      error: errorMessage(error),
    }));
    return json({ error: "platform_failure" }, 500);
  }
}

async function invokeDynamicApp(
  request: Request,
  env: ConfiguredEnv,
  ctx: ExecutionContext,
  claims: RuntimeSessionClaims,
  app: App,
): Promise<Response> {
  const revision = app.revisions.find((candidate) => candidate.revisionId === app.activeRevisionId);
  if (!revision) return json({ error: "active_revision_missing" }, 503);
  let artifact: BuildArtifact;
  try {
    const object = await env.APP_ARTIFACTS.get(revision.artifactKey);
    if (!object) throw new Error("artifact missing");
    artifact = await parseArtifact(await object.text());
  } catch (error) {
    console.error(JSON.stringify({
      type: "dynamic_app.artifact_failed",
      app_id: app.appId,
      revision: revision.revisionId,
      tenant_id: claims.tenantId,
      error: errorMessage(error),
    }));
    return json({ error: "app_artifact_unavailable" }, 503);
  }

  const capability = (ctx.exports as unknown as AppLoopbackExports).NanocodexCapability({
    props: {
      actorUserId: claims.actorUserId,
      appId: app.appId,
      basePath: `/a/${app.slug}/`,
      displayName: app.displayName,
      grants: APP_GRANTS,
      revision: revision.revisionId,
      tenantId: claims.tenantId,
    },
  });
  const worker = env.LOADER.get(
    `${claims.tenantId}:${app.appId}:${revision.revisionId}:${revision.policyVersion}`,
    async () => ({
      compatibilityDate: artifact.compatibilityDate,
      env: { NANOCODEX: capability },
      globalOutbound: null,
      limits: { cpuMs: 100, subRequests: 16 },
      mainModule: artifact.mainModule,
      modules: artifact.modules,
    }),
  );
  const incoming = new URL(request.url);
  const route = APP_PATH.exec(incoming.pathname);
  if (!route || route[1] !== app.slug) return json({ error: "not_found" }, 404);
  const virtualUrl = new URL(`https://app.internal${route[2] || "/"}`);
  virtualUrl.search = incoming.search;
  const sanitized = appRequest(request);
  const headers = new Headers(sanitized.headers);
  headers.set("x-forwarded-prefix", `/a/${app.slug}`);
  const forwarded = new Request(virtualUrl, new Request(sanitized, { headers }));
  try {
    const response = await worker.getEntrypoint().fetch(
      forwarded as unknown as Request<unknown, CfProperties<unknown>>,
    );
    return rewriteAppResponse(appResponse(response), app.slug);
  } catch (error) {
    console.error(JSON.stringify({
      type: "dynamic_app.failed",
      app_id: app.appId,
      revision: revision.revisionId,
      tenant_id: claims.tenantId,
      error: errorMessage(error),
    }));
    return json({ error: "app_failed" }, 502);
  }
}

function rewriteAppResponse(response: Response, slug: string): Response {
  const headers = new Headers(response.headers);
  const location = headers.get("location");
  if (location?.startsWith("/") && !location.startsWith("//")) {
    headers.set("location", `/a/${slug}${location}`);
  }
  return new Response(response.body, { headers, status: response.status, statusText: response.statusText });
}

async function serveConsoleAsset(request: Request, env: ConfiguredEnv, url: URL): Promise<Response> {
  if (url.pathname === "/apps") {
    return new Response(null, { status: 308, headers: { location: "/apps/" } });
  }
  const assetUrl = new URL(request.url);
  assetUrl.pathname = url.pathname === "/apps/" ? "/" : url.pathname.slice("/apps".length);
  const response = await env.ASSETS.fetch(new Request(assetUrl, request));
  if (response.status !== 404 || assetUrl.pathname.startsWith("/assets/")) return response;
  assetUrl.pathname = "/";
  return env.ASSETS.fetch(new Request(assetUrl, request));
}

type ConfiguredEnv = Env & {
  ASSETS: Fetcher;
  LAUNCH_TICKET_SECRET: string;
  RUNTIME_ORIGIN: string;
};

function configured(env: Env): env is ConfiguredEnv {
  return typeof env.LAUNCH_TICKET_SECRET === "string"
    && new TextEncoder().encode(env.LAUNCH_TICKET_SECRET).byteLength >= 32
    && typeof env.RUNTIME_ORIGIN === "string"
    && /^https:\/\/[^/]+$/.test(env.RUNTIME_ORIGIN)
    && typeof env.ASSETS?.fetch === "function"
    && typeof env.APP_ARTIFACTS?.get === "function"
    && typeof env.APP_BUILDS?.create === "function"
    && typeof env.APP_GIT?.request === "function"
    && typeof env.APP_LAUNCH_TICKETS?.getByName === "function"
    && typeof env.APP_REGISTRY?.get === "function"
    && typeof env.APP_STATE?.get === "function"
    && typeof env.LOADER?.get === "function"
    && typeof env.NANOCODEX_AGENTS?.createAgent === "function"
    && typeof env.AI?.run === "function";
}

async function readJson(request: Request): Promise<Record<string, unknown> | Response> {
  const length = Number(request.headers.get("content-length") ?? "0");
  if (!Number.isFinite(length) || length > MAX_JSON_BYTES) return json({ error: "request_too_large" }, 413);
  const text = await request.text();
  if (text.length > MAX_JSON_BYTES) return json({ error: "request_too_large" }, 413);
  try {
    const value = JSON.parse(text) as unknown;
    return value && typeof value === "object" && !Array.isArray(value)
      ? value as Record<string, unknown>
      : json({ error: "invalid_json" }, 400);
  } catch {
    return json({ error: "invalid_json" }, 400);
  }
}

function jobView(job: Job): object {
  return {
    app_id: job.targetAppId,
    completed_at: job.completedAt,
    created_at: job.createdAt,
    error: job.error,
    id: job.jobId,
    revision: job.revisionId,
    status: job.status,
  };
}

function appView(app: App | AppSummary): object {
  return {
    active_revision: app.activeRevisionId,
    created_at: app.createdAt,
    display_name: app.displayName,
    id: app.appId,
    live_slug: app.liveSlug,
    revisions: app.revisions.map((revision) => ({
      artifact_bytes: revision.artifactBytes,
      created_at: revision.createdAt,
      generation_model: revision.generationModel,
      id: revision.revisionId,
      source_commit: revision.sourceCommitOid,
      source_summary: JSON.parse(revision.sourceSummary),
    })),
    slug: app.slug,
    updated_at: app.updatedAt,
  };
}

function json(body: unknown, status: number): Response {
  return Response.json(body, {
    status,
    headers: {
      "cache-control": "no-store",
      "x-content-type-options": "nosniff",
    },
  });
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
