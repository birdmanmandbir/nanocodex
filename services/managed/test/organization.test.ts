import { env, SELF, runInDurableObject } from "cloudflare:test";
import { exports as workerExports } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

import {
  ManagedAgentEntrypoint,
  type Env,
} from "../src/index";
import type { TeamAuthorization } from "../src/organization";

const testEnv = env as unknown as Env;
const ORIGIN = "https://example.test";
const OWNER_ID = "a1111111-1111-4111-8111-111111111111";
const MEMBER_ID = "a2222222-2222-4222-8222-222222222222";
const OTHER_ID = "a3333333-3333-4333-8333-333333333333";
const SECOND_OWNER_ID = "a4444444-4444-4444-8444-444444444444";

describe("managed team authority", () => {
  it("requires a persistent passkey session and exact same-origin mutations", async () => {
    const token = "A".repeat(43);
    const apiKey = `ncx_live_${"a".repeat(12)}_${"b".repeat(43)}`;
    await seedPasskeySession(OWNER_ID, token);
    await seedApiKey(OWNER_ID, apiKey);

    const unauthenticated = await createTeam(undefined, "Unauthenticated");
    expect(unauthenticated.status).toBe(401);
    const apiKeyDenied = await createTeam(undefined, "API key", {
      authorization: `Bearer ${apiKey}`,
      origin: ORIGIN,
    });
    expect(apiKeyDenied.status).toBe(401);
    const crossOrigin = await createTeam(token, "Cross origin", {
      origin: "https://attacker.test",
    });
    expect(crossOrigin.status).toBe(403);
    expect(await crossOrigin.json()).toEqual({ error: "forbidden_origin" });

    const created = await createTeam(token, "  Acme Agents  ");
    expect(created.status).toBe(201);
    const receipt = await created.json<{
      team: { id: string; name: string; role: string; created_at: number };
    }>();
    expect(receipt.team).toMatchObject({ name: "Acme Agents", role: "owner" });

    const list = await SELF.fetch(`${ORIGIN}/v1/teams`, { headers: accountHeaders(token) });
    expect(await list.json()).toEqual({ data: [receipt.team] });
    const me = await SELF.fetch(`${ORIGIN}/v1/me`, { headers: accountHeaders(token) });
    expect(await me.json()).toMatchObject({ teams: [receipt.team] });

    const members = await SELF.fetch(`${ORIGIN}/v1/teams/${receipt.team.id}/members`, {
      headers: accountHeaders(token),
    });
    expect(members.status).toBe(200);
    expect(await members.json()).toEqual({
      data: [{ user_id: OWNER_ID, role: "owner", joined_at: receipt.team.created_at }],
    });

    const immutable = await testEnv.NANOCODEX_ORGANIZATIONS.getByName(receipt.team.id).fetch(
      "https://organization.internal/initialize",
      {
        method: "PUT",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ id: receipt.team.id, name: "Renamed", creator_id: OTHER_ID }),
      },
    );
    expect(immutable.status).toBe(409);
    expect((await (await SELF.fetch(`${ORIGIN}/v1/teams`, {
      headers: accountHeaders(token),
    })).json<{ data: Array<{ name: string }> }>()).data[0]?.name).toBe("Acme Agents");
  });

  it("stores one-use expiring invitations hashed and enforces roles and revocation", async () => {
    const ownerToken = "C".repeat(43);
    const memberToken = "D".repeat(43);
    const otherToken = "E".repeat(43);
    const secondOwnerToken = "F".repeat(43);
    await Promise.all([
      seedPasskeySession(OWNER_ID, ownerToken),
      seedPasskeySession(MEMBER_ID, memberToken),
      seedPasskeySession(OTHER_ID, otherToken),
      seedPasskeySession(SECOND_OWNER_ID, secondOwnerToken),
    ]);
    const team = await createdTeam(ownerToken, "Roles Team");

    const crossOriginInvite = await invite(ownerToken, team.id, "member", "https://attacker.test");
    expect(crossOriginInvite.status).toBe(403);
    const invitationResponse = await invite(ownerToken, team.id, "member");
    expect(invitationResponse.status).toBe(201);
    const invitation = await invitationResponse.json<{
      invitation: string;
      expires_at: number;
      role: string;
    }>();
    expect(invitation.invitation).toMatch(new RegExp(`^${team.id}\\.[A-Za-z0-9_-]{43}$`));
    expect(invitation.role).toBe("member");

    const storedInvitations = await runInDurableObject(
      testEnv.NANOCODEX_ORGANIZATIONS.getByName(team.id),
      (_instance, state) => state.storage.sql.exec<{ digest: string; expires_at: number }>(
        "SELECT digest, expires_at FROM organization_invitations",
      ).toArray(),
    );
    expect(storedInvitations).toHaveLength(1);
    expect(storedInvitations[0]!.digest).not.toContain(invitation.invitation);
    expect(storedInvitations[0]!.digest).toMatch(/^[A-Za-z0-9_-]{43}$/);

    const crossOriginAccept = await accept(memberToken, invitation.invitation, "https://attacker.test");
    expect(crossOriginAccept.status).toBe(403);
    const accepted = await accept(memberToken, invitation.invitation);
    expect(accepted.status).toBe(201);
    expect(await accepted.json()).toMatchObject({
      replayed: false,
      team: { id: team.id, role: "member" },
    });
    const replay = await accept(memberToken, invitation.invitation);
    expect(replay.status).toBe(200);
    expect(await replay.json()).toMatchObject({ replayed: true });
    expect((await accept(otherToken, invitation.invitation)).status).toBe(410);

    expect((await invite(memberToken, team.id, "owner")).status).toBe(404);
    const memberListDenied = await SELF.fetch(`${ORIGIN}/v1/teams/${team.id}/members`, {
      headers: accountHeaders(memberToken),
    });
    expect(memberListDenied.status).toBe(404);
    expect((await remove(memberToken, team.id, OWNER_ID)).status).toBe(404);

    const service = managedAuthority("nanocodex-apps");
    expect(await service.authorizeTeam(OWNER_ID, team.id)).toMatchObject({
      authorized: true,
      membership: { role: "owner", user_id: OWNER_ID },
    });
    expect(await service.authorizeTeam(MEMBER_ID, team.id)).toMatchObject({
      authorized: true,
      membership: { role: "member", user_id: MEMBER_ID },
    });
    expect(await managedAuthority("attacker").authorizeTeam(OWNER_ID, team.id)).toEqual({
      authorized: false,
    });

    const ownerInvitation = await (await invite(ownerToken, team.id, "owner")).json<{
      invitation: string;
    }>();
    const formerOwnerInvitation = await (await invite(ownerToken, team.id, "owner")).json<{
      invitation: string;
    }>();
    expect((await accept(secondOwnerToken, ownerInvitation.invitation)).status).toBe(201);
    expect((await remove(ownerToken, team.id, OWNER_ID)).status).toBe(204);
    expect(await service.authorizeTeam(OWNER_ID, team.id)).toEqual({ authorized: false });
    expect((await accept(otherToken, formerOwnerInvitation.invitation)).status).toBe(410);
    expect((await remove(secondOwnerToken, team.id, SECOND_OWNER_ID)).status).toBe(409);
    expect(await (await remove(secondOwnerToken, team.id, SECOND_OWNER_ID)).json()).toEqual({
      error: "last_owner",
    });

    expect((await remove(secondOwnerToken, team.id, MEMBER_ID)).status).toBe(204);
    expect(await service.authorizeTeam(MEMBER_ID, team.id)).toEqual({ authorized: false });
    const revokedDiscovery = await SELF.fetch(`${ORIGIN}/v1/teams`, {
      headers: accountHeaders(memberToken),
    });
    expect(await revokedDiscovery.json()).toEqual({ data: [] });

    const expiring = await (await invite(secondOwnerToken, team.id, "member")).json<{
      invitation: string;
    }>();
    await runInDurableObject(
      testEnv.NANOCODEX_ORGANIZATIONS.getByName(team.id),
      (_instance, state) => {
        state.storage.sql.exec(
          "UPDATE organization_invitations SET expires_at = ? WHERE accepted_at IS NULL",
          Date.now() - 1,
        );
      },
    );
    expect((await accept(otherToken, expiring.invitation)).status).toBe(410);

    const oversized = await SELF.fetch(`${ORIGIN}/v1/teams`, {
      method: "POST",
      headers: {
        ...accountHeaders(secondOwnerToken),
        "content-type": "application/json",
        origin: ORIGIN,
      },
      body: new ReadableStream({
        start(controller) {
          controller.enqueue(new TextEncoder().encode(`{"name":"${"x".repeat(5_000)}"}`));
          controller.close();
        },
      }),
    });
    expect(oversized.status).toBe(413);
  });

  it("never turns forged or stale UserAccount discovery references into authority", async () => {
    const token = "G".repeat(43);
    await seedPasskeySession(OTHER_ID, token);
    const forgedTeamId = "afffffff-ffff-4fff-8fff-ffffffffffff";
    const attached = await testEnv.NANOCODEX_USERS.getByName(OTHER_ID).fetch(
      "https://user.internal/organizations",
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ organizationId: forgedTeamId }),
      },
    );
    expect(attached.status).toBe(201);

    expect(await (await SELF.fetch(`${ORIGIN}/v1/teams`, {
      headers: accountHeaders(token),
    })).json()).toEqual({ data: [] });
    expect(await (await SELF.fetch(`${ORIGIN}/v1/me`, {
      headers: accountHeaders(token),
    })).json()).toMatchObject({ teams: [] });
    expect(await managedAuthority("nanocodex-apps").authorizeTeam(OTHER_ID, forgedTeamId)).toEqual({
      authorized: false,
    });
  });
});

function managedAuthority(clientId: string): Pick<ManagedAgentEntrypoint, "authorizeTeam"> {
  type Loopback = {
    ManagedAgentEntrypoint(options: {
      props: { clientId: string };
    }): Pick<ManagedAgentEntrypoint, "authorizeTeam">;
  };
  return (workerExports as unknown as Loopback).ManagedAgentEntrypoint({ props: { clientId } });
}

async function createdTeam(token: string, name: string): Promise<{
  id: string;
  name: string;
  role: string;
  created_at: number;
}> {
  const response = await createTeam(token, name);
  expect(response.status).toBe(201);
  return (await response.json<{ team: {
    id: string;
    name: string;
    role: string;
    created_at: number;
  } }>()).team;
}

function createTeam(
  token: string | undefined,
  name: string,
  extraHeaders: Record<string, string> = {},
): Promise<Response> {
  return SELF.fetch(`${ORIGIN}/v1/teams`, {
    method: "POST",
    headers: {
      ...(token ? accountHeaders(token) : {}),
      "content-type": "application/json",
      origin: ORIGIN,
      ...extraHeaders,
    },
    body: JSON.stringify({ name }),
  });
}

function invite(
  token: string,
  teamId: string,
  role: "owner" | "member",
  origin = ORIGIN,
): Promise<Response> {
  return SELF.fetch(`${ORIGIN}/v1/teams/${teamId}/invitations`, {
    method: "POST",
    headers: { ...accountHeaders(token), "content-type": "application/json", origin },
    body: JSON.stringify({ role }),
  });
}

function accept(token: string, invitation: string, origin = ORIGIN): Promise<Response> {
  return SELF.fetch(`${ORIGIN}/v1/team-invitations/accept`, {
    method: "POST",
    headers: { ...accountHeaders(token), "content-type": "application/json", origin },
    body: JSON.stringify({ invitation }),
  });
}

function remove(token: string, teamId: string, userId: string): Promise<Response> {
  return SELF.fetch(`${ORIGIN}/v1/teams/${teamId}/members/${userId}`, {
    method: "DELETE",
    headers: { ...accountHeaders(token), "content-type": "application/json", origin: ORIGIN },
    body: JSON.stringify({}),
  });
}

function accountHeaders(token: string): Record<string, string> {
  return { cookie: `nanocodex_account=${token}` };
}

async function seedPasskeySession(userId: string, token: string): Promise<void> {
  const account = testEnv.NANOCODEX_USERS.getByName(userId);
  const provisioned = await account.fetch("https://user.internal/account", {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ id: userId, persistent: true }),
  });
  expect(provisioned.ok).toBe(true);

  const encodedUserId = btoa(userId).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
  const now = Math.floor(Date.now() / 1_000);
  const stored = await testEnv.NANOCODEX_AUTH.getByName("webauthn").fetch(
    `https://do.invalid/set?key=${encodeURIComponent(`session:${token}`)}`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        value: {
          credentialId: `credential-${token}`,
          publicKey: "0x01",
          userId: encodedUserId,
          issuedAt: now,
          expiresAt: now + 300,
        },
        ttl: 300,
      }),
    },
  );
  expect(stored.ok).toBe(true);
}

async function seedApiKey(userId: string, token: string): Promise<void> {
  const digest = await sha256(token);
  const id = token.match(/^ncx_live_([A-Za-z0-9_-]{12})_/)![1]!;
  const response = await testEnv.NANOCODEX_API_KEYS.getByName(digest).fetch(
    "https://api-key.internal/record",
    {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        id,
        label: "team test",
        prefix: token.slice(0, "ncx_live_".length + 12),
        createdAt: Date.now(),
        digest,
        userId,
      }),
    },
  );
  expect(response.status).toBe(201);
}

async function sha256(value: string): Promise<string> {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value)));
  let binary = "";
  for (const byte of digest) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}
