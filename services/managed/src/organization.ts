import { DurableObject } from "cloudflare:workers";

import {
  authenticatePersistentAccount,
  attachOrganization,
  detachOrganization,
  isUserId,
  listRevalidatedTeamSummaries,
  organizationIdFromString,
  requireSameOriginMutation,
  type AccountAuthEnv,
} from "./account-auth";
import { withHardDeadline } from "./deadline";

const INVITATION = /^([0-9a-f]{64})\.([A-Za-z0-9_-]{43})$/;
const DIGEST = /^[A-Za-z0-9_-]{43}$/;
const MAX_REQUEST_BYTES = 4 * 1024;
const MAX_PENDING_INVITATIONS = 100;
const MAX_RETAINED_INVITATIONS = 512;
const MAX_MEMBERS = 256;
const INVITATION_TTL_MS = 7 * 24 * 60 * 60_000;
const TEAM_IO_TIMEOUT_MS = 10_000;

export type TeamRole = "owner" | "member";

export type TeamSummary = Readonly<{
  id: string;
  name: string;
  role: TeamRole;
  created_at: number;
}>;

export type TeamAuthorization = Readonly<
  | { authorized: false }
  | {
    authorized: true;
    team: Readonly<{ id: string; name: string; created_at: number }>;
    membership: Readonly<{ user_id: string; role: TeamRole; joined_at: number }>;
  }
>;

type OrganizationRow = {
  id: string;
  name: string;
  creator_id: string;
  created_at: number;
};

type MemberRow = {
  user_id: string;
  role: TeamRole;
  joined_at: number;
};

type InvitationRow = {
  digest: string;
  role: TeamRole;
  expires_at: number;
  accepted_by: string | null;
  accepted_at: number | null;
};

export async function routeTeamRequest(
  request: Request,
  env: AccountAuthEnv,
  url: URL,
): Promise<Response | undefined> {
  const isTeamRoute = url.pathname === "/v1/teams"
    || url.pathname === "/v1/team-invitations/accept"
    || /^\/v1\/teams\/[^/]+\/(?:invitations|members(?:\/[^/]+)?)$/.test(url.pathname);
  if (!isTeamRoute) return undefined;

  const principal = await authenticatePersistentAccount(request, env, url);
  if (!principal) return json({ error: "unauthorized" }, 401);
  if (request.method !== "GET" && request.method !== "HEAD") {
    const originFailure = requireSameOriginMutation(request, url, principal);
    if (originFailure) return originFailure;
  }

  if (url.pathname === "/v1/teams") {
    if (request.method === "GET") {
      return json({ data: await listRevalidatedTeamSummaries(env, principal.userId) });
    }
    if (request.method !== "POST") return methodNotAllowed();
    const value = await boundedJson(request);
    if (value instanceof Response) return value;
    if (!exactObject(value, ["name"])) return json({ error: "invalid_team" }, 400);
    const name = normalizeTeamName(value.name);
    if (!name) return json({ error: "invalid_team_name" }, 400);

    const teamId = env.NANOCODEX_ORGANIZATIONS.newUniqueId().toString();
    let attachment: "attached" | "existing" | "limit";
    try {
      attachment = await attachOrganization(env, principal.userId, teamId);
    } catch {
      return json({ error: "team_service_unavailable" }, 503);
    }
    if (attachment === "limit") return json({ error: "team_limit_reached" }, 409);
    if (attachment === "existing") return json({ error: "team_creation_failed" }, 503);
    let initialized: Response;
    try {
      initialized = await organizationFetch(
        env,
        teamId,
        "/initialize",
        {
          method: "PUT",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ id: teamId, name, creator_id: principal.userId }),
        },
      );
    } catch {
      await detachOrganization(env, principal.userId, teamId).catch(() => {});
      return json({ error: "team_service_unavailable" }, 503);
    }
    if (!initialized.ok) {
      await initialized.body?.cancel();
      await detachOrganization(env, principal.userId, teamId).catch(() => {});
      return json({ error: "team_creation_failed" }, 503);
    }
    const authorization = await initialized.json<TeamAuthorization>();
    if (!authorization.authorized) {
      await detachOrganization(env, principal.userId, teamId).catch(() => {});
      return json({ error: "team_creation_failed" }, 503);
    }
    return json({ team: summaryFromAuthorization(authorization) }, 201);
  }

  if (url.pathname === "/v1/team-invitations/accept") {
    if (request.method !== "POST") return methodNotAllowed();
    const value = await boundedJson(request);
    if (value instanceof Response) return value;
    if (!exactObject(value, ["invitation"]) || typeof value.invitation !== "string") {
      return json({ error: "invalid_invitation" }, 400);
    }
    const parsed = value.invitation.match(INVITATION);
    if (!parsed) return json({ error: "invalid_invitation" }, 400);
    const teamId = parsed[1]!;
    if (!organizationIdFromString(env.NANOCODEX_ORGANIZATIONS, teamId)) {
      return json({ error: "invalid_invitation" }, 400);
    }
    const digest = await sha256(value.invitation);
    let attachment: "attached" | "existing" | "limit";
    try {
      attachment = await attachOrganization(env, principal.userId, teamId);
    } catch {
      return json({ error: "team_discovery_unavailable" }, 503);
    }
    if (attachment === "limit") return json({ error: "team_limit_reached" }, 409);
    let accepted: Response;
    try {
      accepted = await organizationFetch(
        env,
        teamId,
        "/invitations/accept",
        {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ actor_id: principal.userId, digest }),
        },
      );
    } catch {
      if (attachment === "attached") {
        await detachOrganization(env, principal.userId, teamId).catch(() => {});
      }
      return json({ error: "team_service_unavailable" }, 503);
    }
    if (!accepted.ok) {
      if (attachment === "attached") {
        await detachOrganization(env, principal.userId, teamId).catch(() => {});
      }
      return passThrough(accepted);
    }
    const body = await accepted.json<TeamAuthorization & { replayed?: boolean }>();
    if (!body.authorized) {
      if (attachment === "attached") {
        await detachOrganization(env, principal.userId, teamId).catch(() => {});
      }
      return json({ error: "invalid_invitation" }, 410);
    }
    return json({
      team: summaryFromAuthorization(body),
      replayed: body.replayed === true,
    }, body.replayed === true ? 200 : 201);
  }

  const invitationMatch = url.pathname.match(/^\/v1\/teams\/([^/]+)\/invitations$/);
  if (invitationMatch) {
    if (request.method !== "POST") return methodNotAllowed();
    const teamId = invitationMatch[1]!;
    if (!organizationIdFromString(env.NANOCODEX_ORGANIZATIONS, teamId)) {
      return json({ error: "not_found" }, 404);
    }
    const value = await boundedJson(request);
    if (value instanceof Response) return value;
    if (!exactObject(value, ["role"])) return json({ error: "invalid_invitation" }, 400);
    const role = value.role ?? "member";
    if (role !== "owner" && role !== "member") {
      return json({ error: "invalid_invitation_role" }, 400);
    }
    const secret = randomBase64Url(32);
    const invitation = `${teamId}.${secret}`;
    const digest = await sha256(invitation);
    let response: Response;
    try {
      response = await organizationFetch(
        env,
        teamId,
        "/invitations",
        {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({
            actor_id: principal.userId,
            digest,
            role,
            expires_at: Date.now() + INVITATION_TTL_MS,
          }),
        },
      );
    } catch {
      return json({ error: "team_service_unavailable" }, 503);
    }
    if (!response.ok) return passThrough(response);
    const created = await response.json<{ expires_at: number; role: TeamRole }>();
    return json({ invitation, expires_at: created.expires_at, role: created.role }, 201);
  }

  const memberMatch = url.pathname.match(/^\/v1\/teams\/([^/]+)\/members\/([^/]+)$/);
  if (memberMatch) {
    if (request.method !== "DELETE") return methodNotAllowed();
    const teamId = memberMatch[1]!;
    const memberId = memberMatch[2]!.toLowerCase();
    if (!organizationIdFromString(env.NANOCODEX_ORGANIZATIONS, teamId) || !isUserId(memberId)) {
      return json({ error: "not_found" }, 404);
    }
    let removed: Response;
    try {
      removed = await organizationFetch(
        env,
        teamId,
        `/members/${memberId}`,
        {
          method: "DELETE",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ actor_id: principal.userId }),
        },
      );
    } catch {
      return json({ error: "team_service_unavailable" }, 503);
    }
    if (!removed.ok) return passThrough(removed);
    await removed.body?.cancel();
    await detachOrganization(env, memberId, teamId).catch(() => {});
    return new Response(null, { status: 204 });
  }

  const membersMatch = url.pathname.match(/^\/v1\/teams\/([^/]+)\/members$/);
  if (membersMatch) {
    if (request.method !== "GET") return methodNotAllowed();
    const teamId = membersMatch[1]!;
    if (!organizationIdFromString(env.NANOCODEX_ORGANIZATIONS, teamId)) {
      return json({ error: "not_found" }, 404);
    }
    try {
      const response = await organizationFetch(
        env,
        teamId,
        `/members?actor_id=${principal.userId}`,
      );
      return passThrough(response);
    } catch {
      return json({ error: "team_service_unavailable" }, 503);
    }
  }

  return undefined;
}

export async function authorizeTeam(
  env: Pick<AccountAuthEnv, "NANOCODEX_ORGANIZATIONS">,
  actorUserId: unknown,
  teamId: unknown,
): Promise<TeamAuthorization> {
  if (!isUserId(actorUserId) || typeof teamId !== "string"
    || !organizationIdFromString(env.NANOCODEX_ORGANIZATIONS, teamId)) {
    return { authorized: false };
  }
  try {
    const response = await organizationFetch(
      env,
      teamId,
      `/authority/${actorUserId}`,
    );
    if (!response.ok) {
      await response.body?.cancel();
      return { authorized: false };
    }
    const authorization = await response.json<TeamAuthorization>();
    return validAuthorization(authorization, actorUserId, teamId)
      ? authorization
      : { authorized: false };
  } catch {
    return { authorized: false };
  }
}

export class Organization extends DurableObject<Record<string, never>> {
  constructor(ctx: DurableObjectState, env: Record<string, never>) {
    super(ctx, env);
    this.ctx.storage.sql.exec(`
      CREATE TABLE IF NOT EXISTS organization_state (
        singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
        id TEXT NOT NULL UNIQUE,
        name TEXT NOT NULL,
        creator_id TEXT NOT NULL,
        created_at INTEGER NOT NULL
      );
      CREATE TABLE IF NOT EXISTS organization_members (
        user_id TEXT PRIMARY KEY,
        role TEXT NOT NULL CHECK (role IN ('owner', 'member')),
        joined_at INTEGER NOT NULL,
        invited_by TEXT
      );
      CREATE TABLE IF NOT EXISTS organization_invitations (
        digest TEXT PRIMARY KEY,
        role TEXT NOT NULL CHECK (role IN ('owner', 'member')),
        created_by TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        accepted_by TEXT,
        accepted_at INTEGER
      );
    `);
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/initialize" && request.method === "PUT") {
      const value = await boundedJson(request);
      if (value instanceof Response) return value;
      const id = typeof value.id === "string" ? value.id.toLowerCase() : "";
      const creatorId = typeof value.creator_id === "string" ? value.creator_id.toLowerCase() : "";
      const name = normalizeTeamName(value.name);
      if (!exactObject(value, ["id", "name", "creator_id"])
        || id !== this.ctx.id.toString() || !isUserId(creatorId) || !name) {
        return json({ error: "invalid_team" }, 400);
      }
      const now = Date.now();
      let conflict = false;
      this.ctx.storage.transactionSync(() => {
        if (this.#organization()) {
          conflict = true;
          return;
        }
        this.ctx.storage.sql.exec(
          `INSERT INTO organization_state (singleton, id, name, creator_id, created_at)
           VALUES (1, ?, ?, ?, ?)`,
          id,
          name,
          creatorId,
          now,
        );
        this.ctx.storage.sql.exec(
          `INSERT INTO organization_members (user_id, role, joined_at, invited_by)
           VALUES (?, 'owner', ?, NULL)`,
          creatorId,
          now,
        );
      });
      if (conflict) return json({ error: "team_already_initialized" }, 409);
      return json(this.#authorization(creatorId), 201);
    }

    const authorityMatch = url.pathname.match(/^\/authority\/([^/]+)$/);
    if (authorityMatch && request.method === "GET") {
      const userId = authorityMatch[1]!.toLowerCase();
      if (!isUserId(userId)) return json({ authorized: false }, 404);
      const authorization = this.#authorization(userId);
      return authorization.authorized
        ? json(authorization)
        : json({ authorized: false }, 404);
    }

    if (url.pathname === "/invitations" && request.method === "POST") {
      const value = await boundedJson(request);
      if (value instanceof Response) return value;
      const actorId = typeof value.actor_id === "string" ? value.actor_id.toLowerCase() : "";
      const expiresAt = value.expires_at;
      if (!exactObject(value, ["actor_id", "digest", "role", "expires_at"])
        || !isUserId(actorId) || typeof value.digest !== "string" || !DIGEST.test(value.digest)
        || (value.role !== "owner" && value.role !== "member")
        || !Number.isSafeInteger(expiresAt)
        || Number(expiresAt) <= Date.now()
        || Number(expiresAt) > Date.now() + INVITATION_TTL_MS + 60_000) {
        return json({ error: "invalid_invitation" }, 400);
      }
      if (this.#member(actorId)?.role !== "owner") return json({ error: "not_found" }, 404);
      const now = Date.now();
      this.#pruneInvitations(now);
      const pending = this.ctx.storage.sql.exec<{ count: number }>(
        `SELECT COUNT(*) AS count FROM organization_invitations
         WHERE accepted_at IS NULL AND expires_at > ?`,
        now,
      ).toArray()[0]?.count ?? 0;
      if (pending >= MAX_PENDING_INVITATIONS) {
        return json({ error: "invitation_limit_reached" }, 409);
      }
      const retained = this.ctx.storage.sql.exec<{ count: number }>(
        "SELECT COUNT(*) AS count FROM organization_invitations",
      ).toArray()[0]?.count ?? 0;
      if (retained >= MAX_RETAINED_INVITATIONS) {
        return json({ error: "invitation_limit_reached" }, 409);
      }
      this.ctx.storage.sql.exec(
        `INSERT INTO organization_invitations (
           digest, role, created_by, created_at, expires_at, accepted_by, accepted_at
         ) VALUES (?, ?, ?, ?, ?, NULL, NULL)`,
        value.digest,
        value.role,
        actorId,
        now,
        expiresAt,
      );
      return json({ expires_at: expiresAt, role: value.role }, 201);
    }

    if (url.pathname === "/invitations/accept" && request.method === "POST") {
      const value = await boundedJson(request);
      if (value instanceof Response) return value;
      const actorId = typeof value.actor_id === "string" ? value.actor_id.toLowerCase() : "";
      if (!exactObject(value, ["actor_id", "digest"])
        || !isUserId(actorId) || typeof value.digest !== "string" || !DIGEST.test(value.digest)) {
        return json({ error: "invalid_invitation" }, 400);
      }
      const now = Date.now();
      let replayed = false;
      let unavailable = false;
      let memberLimit = false;
      this.ctx.storage.transactionSync(() => {
        const invitation = this.#invitation(value.digest as string);
        if (!invitation || invitation.expires_at <= now) {
          unavailable = true;
          return;
        }
        if (invitation.accepted_at !== null) {
          if (invitation.accepted_by === actorId && this.#member(actorId)?.role === invitation.role) {
            replayed = true;
          } else {
            unavailable = true;
          }
          return;
        }
        if (this.#member(actorId)) {
          unavailable = true;
          return;
        }
        if (this.#memberCount() >= MAX_MEMBERS) {
          memberLimit = true;
          return;
        }
        this.ctx.storage.sql.exec(
          `INSERT INTO organization_members (user_id, role, joined_at, invited_by)
           VALUES (?, ?, ?, (SELECT created_by FROM organization_invitations WHERE digest = ?))`,
          actorId,
          invitation.role,
          now,
          invitation.digest,
        );
        this.ctx.storage.sql.exec(
          `UPDATE organization_invitations SET accepted_by = ?, accepted_at = ?
           WHERE digest = ? AND accepted_at IS NULL`,
          actorId,
          now,
          invitation.digest,
        );
      });
      if (memberLimit) return json({ error: "team_member_limit_reached" }, 409);
      if (unavailable) return json({ error: "invalid_or_expired_invitation" }, 410);
      const authorization = this.#authorization(actorId);
      return authorization.authorized
        ? json({ ...authorization, replayed }, replayed ? 200 : 201)
        : json({ error: "invitation_acceptance_failed" }, 503);
    }

    const memberMatch = url.pathname.match(/^\/members\/([^/]+)$/);
    if (memberMatch && request.method === "DELETE") {
      const targetId = memberMatch[1]!.toLowerCase();
      const value = await boundedJson(request);
      if (value instanceof Response) return value;
      const actorId = typeof value.actor_id === "string" ? value.actor_id.toLowerCase() : "";
      if (!exactObject(value, ["actor_id"]) || !isUserId(actorId) || !isUserId(targetId)) {
        return json({ error: "not_found" }, 404);
      }
      if (this.#member(actorId)?.role !== "owner") return json({ error: "not_found" }, 404);
      const target = this.#member(targetId);
      if (!target) return json({ error: "not_found" }, 404);
      if (target.role === "owner" && this.#ownerCount() === 1) {
        return json({ error: "last_owner" }, 409);
      }
      this.ctx.storage.transactionSync(() => {
        this.ctx.storage.sql.exec("DELETE FROM organization_members WHERE user_id = ?", targetId);
        this.ctx.storage.sql.exec(
          "DELETE FROM organization_invitations WHERE created_by = ? AND accepted_at IS NULL",
          targetId,
        );
      });
      return new Response(null, { status: 204 });
    }

    if (url.pathname === "/members" && request.method === "GET") {
      const actorId = url.searchParams.get("actor_id")?.toLowerCase() ?? "";
      if (!isUserId(actorId) || this.#member(actorId)?.role !== "owner") {
        return json({ error: "not_found" }, 404);
      }
      const members = this.ctx.storage.sql.exec<MemberRow>(
        `SELECT user_id, role, joined_at FROM organization_members
         ORDER BY joined_at, user_id LIMIT ?`,
        MAX_MEMBERS,
      ).toArray();
      return json({ data: members });
    }

    return json({ error: "not_found" }, 404);
  }

  #organization(): OrganizationRow | undefined {
    return this.ctx.storage.sql.exec<OrganizationRow>(
      `SELECT id, name, creator_id, created_at FROM organization_state WHERE singleton = 1`,
    ).toArray()[0];
  }

  #member(userId: string): MemberRow | undefined {
    return this.ctx.storage.sql.exec<MemberRow>(
      `SELECT user_id, role, joined_at FROM organization_members WHERE user_id = ?`,
      userId,
    ).toArray()[0];
  }

  #invitation(digest: string): InvitationRow | undefined {
    return this.ctx.storage.sql.exec<InvitationRow>(
      `SELECT digest, role, expires_at, accepted_by, accepted_at
       FROM organization_invitations WHERE digest = ?`,
      digest,
    ).toArray()[0];
  }

  #authorization(userId: string): TeamAuthorization {
    const team = this.#organization();
    const membership = this.#member(userId);
    if (!team || !membership) return { authorized: false };
    return {
      authorized: true,
      team: { id: team.id, name: team.name, created_at: team.created_at },
      membership: {
        user_id: membership.user_id,
        role: membership.role,
        joined_at: membership.joined_at,
      },
    };
  }

  #ownerCount(): number {
    return this.ctx.storage.sql.exec<{ count: number }>(
      "SELECT COUNT(*) AS count FROM organization_members WHERE role = 'owner'",
    ).toArray()[0]?.count ?? 0;
  }

  #memberCount(): number {
    return this.ctx.storage.sql.exec<{ count: number }>(
      "SELECT COUNT(*) AS count FROM organization_members",
    ).toArray()[0]?.count ?? 0;
  }

  #pruneInvitations(now: number): void {
    this.ctx.storage.sql.exec(
      "DELETE FROM organization_invitations WHERE expires_at <= ?",
      now,
    );
  }
}

function validAuthorization(
  value: TeamAuthorization,
  actorUserId: string,
  teamId: string,
): value is Extract<TeamAuthorization, { authorized: true }> {
  return value?.authorized === true
    && value.team?.id === teamId
    && typeof value.team.name === "string"
    && Number.isSafeInteger(value.team.created_at)
    && value.membership?.user_id === actorUserId
    && (value.membership.role === "owner" || value.membership.role === "member")
    && Number.isSafeInteger(value.membership.joined_at);
}

function summaryFromAuthorization(
  authorization: Extract<TeamAuthorization, { authorized: true }>,
): TeamSummary {
  return {
    id: authorization.team.id,
    name: authorization.team.name,
    role: authorization.membership.role,
    created_at: authorization.team.created_at,
  };
}

function normalizeTeamName(value: unknown): string | undefined {
  if (typeof value !== "string") return undefined;
  const name = value.trim().normalize("NFC");
  if (!name || [...name].length > 80 || /[\u0000-\u001f\u007f]/.test(name)) return undefined;
  if (new TextEncoder().encode(name).byteLength > 240) return undefined;
  return name;
}

function exactObject(value: Record<string, unknown>, allowed: readonly string[]): boolean {
  const keys = Object.keys(value);
  return keys.every((key) => allowed.includes(key)) && allowed.every((key) => keys.includes(key));
}

async function boundedJson(request: Request): Promise<Record<string, unknown> | Response> {
  if (!request.headers.get("content-type")?.toLowerCase().startsWith("application/json")) {
    return json({ error: "expected_json" }, 415);
  }
  const declared = Number(request.headers.get("content-length") ?? 0);
  if (Number.isFinite(declared) && declared > MAX_REQUEST_BYTES) {
    return json({ error: "request_too_large" }, 413);
  }
  try {
    const reader = request.body?.getReader();
    if (!reader) throw new Error();
    const chunks: Uint8Array[] = [];
    let total = 0;
    while (true) {
      const chunk = await reader.read();
      if (chunk.done) break;
      total += chunk.value.byteLength;
      if (total > MAX_REQUEST_BYTES) {
        await reader.cancel().catch(() => {});
        return json({ error: "request_too_large" }, 413);
      }
      chunks.push(chunk.value);
    }
    const bytes = new Uint8Array(total);
    let offset = 0;
    for (const chunk of chunks) {
      bytes.set(chunk, offset);
      offset += chunk.byteLength;
    }
    const encoded = new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(bytes);
    const value = JSON.parse(encoded) as unknown;
    if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error();
    return value as Record<string, unknown>;
  } catch {
    return json({ error: "invalid_json" }, 400);
  }
}

function randomBase64Url(bytes: number): string {
  const value = crypto.getRandomValues(new Uint8Array(bytes));
  let binary = "";
  for (const byte of value) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

async function sha256(value: string): Promise<string> {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value)));
  let binary = "";
  for (const byte of digest) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

function passThrough(response: Response): Response {
  return new Response(response.body, {
    status: response.status,
    statusText: response.statusText,
    headers: response.headers,
  });
}

function organizationFetch(
  env: Pick<AccountAuthEnv, "NANOCODEX_ORGANIZATIONS">,
  teamId: string,
  path: string,
  init: RequestInit = {},
): Promise<Response> {
  const objectId = organizationIdFromString(env.NANOCODEX_ORGANIZATIONS, teamId);
  if (!objectId) throw new Error("invalid organization identity");
  return withHardDeadline("organization authority", TEAM_IO_TIMEOUT_MS, (signal) => (
    env.NANOCODEX_ORGANIZATIONS.get(objectId).fetch(
      `https://organization.internal${path}`,
      { ...init, signal },
    )
  ));
}

function json(body: unknown, status = 200): Response {
  return Response.json(body, {
    status,
    headers: { "cache-control": "no-store", "x-content-type-options": "nosniff" },
  });
}

function methodNotAllowed(): Response {
  return json({ error: "method_not_allowed" }, 405);
}
