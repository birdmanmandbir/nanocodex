import { DurableObject } from "cloudflare:workers";

import type { LaunchIntentClaims, LaunchTicketClaims } from "./auth";

type LaunchClaims = LaunchIntentClaims | LaunchTicketClaims;

type StoredTicket = Readonly<{
  claims: LaunchClaims;
  consumed: boolean;
}>;

export class LaunchTicketStore extends DurableObject {
  async issue(claims: LaunchClaims): Promise<void> {
    await this.ctx.storage.transaction(async (storage) => {
      const existing = await storage.get<StoredTicket>("ticket");
      if (existing) {
        if (JSON.stringify(existing.claims) === JSON.stringify(claims) && !existing.consumed) return;
        throw new Error("launch ticket nonce is already in use");
      }
      await storage.put("ticket", { claims, consumed: false } satisfies StoredTicket);
      await storage.setAlarm(claims.expiry * 1_000);
    });
  }

  async consume(claims: LaunchClaims): Promise<boolean> {
    return this.ctx.storage.transaction(async (storage) => {
      const stored = await storage.get<StoredTicket>("ticket");
      if (!stored || stored.consumed || stored.claims.expiry <= Math.floor(Date.now() / 1_000)) {
        return false;
      }
      if (JSON.stringify(stored.claims) !== JSON.stringify(claims)) return false;
      await storage.put("ticket", { ...stored, consumed: true } satisfies StoredTicket);
      return true;
    });
  }

  async alarm(): Promise<void> {
    await this.ctx.storage.deleteAll();
  }
}

export async function recordLaunchIntent(
  namespace: DurableObjectNamespace<LaunchTicketStore>,
  claims: LaunchIntentClaims,
): Promise<void> {
  return namespace.getByName(claims.nonce).issue(claims);
}

export async function consumeLaunchIntent(
  namespace: DurableObjectNamespace<LaunchTicketStore>,
  claims: LaunchIntentClaims,
): Promise<boolean> {
  return namespace.getByName(claims.nonce).consume(claims);
}

export async function recordLaunchTicket(
  namespace: DurableObjectNamespace<LaunchTicketStore>,
  claims: LaunchTicketClaims,
): Promise<void> {
  return namespace.getByName(claims.nonce).issue(claims);
}

export async function consumeLaunchTicket(
  namespace: DurableObjectNamespace<LaunchTicketStore>,
  claims: LaunchTicketClaims,
): Promise<boolean> {
  return namespace.getByName(claims.nonce).consume(claims);
}
