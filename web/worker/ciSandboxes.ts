const SANDBOX_ID =
  /^[a-z0-9-]{1,16}-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const TERMINATION_SWEEPS = 3;
const TERMINATION_SETTLE_MS = 250;

export type CiSandboxControlEnv = {
  BACKUP_BUCKET: R2Bucket;
  SANDBOX: DurableObjectNamespace<import("@cloudflare/ci/worker").CiSandbox>;
};

type SandboxTermination = {
  key: string;
  runnerId: string;
  status: "destroyed" | "failed";
  error?: string;
};

export async function terminateActiveSandboxes(
  env: CiSandboxControlEnv,
  head: string,
  options: { deleteMarkers?: boolean } = {},
) {
  const attempts = new Map<string, SandboxTermination>();
  for (let sweep = 0; sweep < TERMINATION_SWEEPS; sweep++) {
    const targets = await activeSandboxTargets(env.BACKUP_BUCKET, head);
    if (targets.length === 0) break;
    const settled = await Promise.all(targets.map(async ({ key, runnerId }) => {
      try {
        await destroyActiveSandbox(env.SANDBOX, runnerId);
        return { key, runnerId, status: "destroyed" as const };
      } catch (cause) {
        return {
          key,
          runnerId,
          status: "failed" as const,
          error: boundedError(cause),
        };
      }
    }));
    for (const result of settled) attempts.set(result.runnerId, result);
    if (sweep + 1 < TERMINATION_SWEEPS) await settleTermination();
  }

  const destroyed: string[] = [];
  const failed: Array<{ runnerId: string; error: string }> = [];
  for (const result of [...attempts.values()].sort((left, right) =>
    left.runnerId.localeCompare(right.runnerId)
  )) {
    const marker = await env.BACKUP_BUCKET.head(result.key);
    if (result.status === "destroyed" || !marker) {
      if (options.deleteMarkers !== false) await env.BACKUP_BUCKET.delete(result.key);
      destroyed.push(result.runnerId);
    } else {
      failed.push({ runnerId: result.runnerId, error: result.error! });
    }
  }
  return { destroyed, failed };
}

export function terminationMarkerKey(head: string) {
  return `runs/${head}/control/terminated.json`;
}

export function failureMarkerKey(head: string) {
  return `runs/${head}/control/failed.json`;
}

async function activeSandboxTargets(bucket: R2Bucket, head: string) {
  const prefix = `runs/${head}/sandboxes/`;
  const markers = await listAll(bucket, prefix);
  return markers.flatMap(({ key }) => {
    const file = key.slice(prefix.length);
    const runnerId = file.endsWith(".json") ? file.slice(0, -5) : "";
    return SANDBOX_ID.test(runnerId) ? [{ key, runnerId }] : [];
  }).sort((left, right) => left.runnerId.localeCompare(right.runnerId));
}

async function listAll(bucket: R2Bucket, prefix: string): Promise<R2Object[]> {
  const objects: R2Object[] = [];
  let cursor: string | undefined;
  do {
    const page = await bucket.list({ prefix, cursor, limit: 1_000 });
    objects.push(...page.objects);
    if (!page.truncated) break;
    if (!page.cursor || page.cursor === cursor) {
      throw new Error("active Sandbox registry pagination did not advance");
    }
    cursor = page.cursor;
  } while (cursor);
  return objects;
}

async function destroyActiveSandbox(
  namespace: CiSandboxControlEnv["SANDBOX"],
  runnerId: string,
) {
  const sandbox = namespace.get(namespace.idFromName(runnerId));
  let failure: unknown;
  for (let attempt = 1; attempt <= 3; attempt++) {
    try {
      await sandbox.destroy();
      return;
    } catch (cause) {
      failure = cause;
    }
  }
  throw new Error(`failed to destroy active CI Sandbox ${runnerId}`, { cause: failure });
}

function settleTermination() {
  return new Promise<void>((resolve) => setTimeout(resolve, TERMINATION_SETTLE_MS));
}

function boundedError(cause: unknown): string {
  const message = cause instanceof Error ? cause.message : String(cause);
  return message.slice(0, 2_000);
}
