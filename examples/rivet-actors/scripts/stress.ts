import { randomUUID } from "node:crypto";

import { createNanocodexClient } from "../src/client.js";

const endpoint = process.env.RIVET_PUBLIC_ENDPOINT ?? "http://127.0.0.1:6420";
const actors = integerEnv("NANOCODEX_STRESS_ACTORS", 32, 1, 128);
const replaysPerActor = integerEnv("NANOCODEX_STRESS_REPLAYS", 128, 1, 2_048);
const concurrencyPerActor = integerEnv(
  "NANOCODEX_STRESS_CONCURRENCY_PER_ACTOR",
  8,
  1,
  64,
);
const client = createNanocodexClient(endpoint);
const sessions = Array.from({ length: actors }, (_, index) => {
  const handle = client.nanocodex.getOrCreate([`stress-${index}-${randomUUID()}`]);
  return {
    connection: handle.connect(),
    handle,
    request: { id: "seed", input: `Reply with exactly ACTOR_${index}` },
  };
});

try {
  await Promise.all(sessions.map(({ connection }) => connection.ready));
  const seeded = await Promise.allSettled(sessions.map(async ({ connection, request }) => {
    const result = await connection.prompt(request);
    if (result.final_message !== request.input.slice("Reply with exactly ".length)) {
      throw new Error(`unexpected seed result: ${result.final_message}`);
    }
  }));
  const seedFailure = seeded.find((result) => result.status === "rejected");
  if (seedFailure) throw seedFailure.reason;

  const expected = actors * replaysPerActor;
  const started = performance.now();
  let replayed = 0;
  for (let offset = 0; offset < replaysPerActor; offset += concurrencyPerActor) {
    const batchSize = Math.min(concurrencyPerActor, replaysPerActor - offset);
    const settled = await Promise.allSettled(sessions.flatMap(({ connection, request }) =>
      Array.from(
        { length: batchSize },
        () => connection.prompt(request),
      )));
    const failure = settled.find((result) => result.status === "rejected");
    if (failure) throw failure.reason;
    const results = settled.flatMap((result) =>
      result.status === "fulfilled" ? [result.value] : []);
    if (results.some((result) => result.type !== "turn_completed")) {
      throw new Error("a terminal replay diverged from its committed result");
    }
    replayed += results.length;
  }
  const elapsedMs = performance.now() - started;
  if (replayed !== expected) {
    throw new Error(`received ${replayed} terminal replays, expected ${expected}`);
  }
  console.log(JSON.stringify({
    actors,
    concurrency_per_actor: concurrencyPerActor,
    terminal_replays: expected,
    elapsed_ms: Math.round(elapsedMs),
    replays_per_second: Math.round(expected / (elapsedMs / 1_000)),
    status: "ok",
  }));
} finally {
  for (const { connection } of sessions) {
    connection.dispose();
  }
  await Promise.all(sessions.map(({ handle }) => handle.reset().catch(() => {})));
}

function integerEnv(name: string, fallback: number, minimum: number, maximum: number): number {
  const value = Number(process.env[name] ?? fallback);
  if (!Number.isSafeInteger(value) || value < minimum || value > maximum) {
    throw new Error(`${name} must be an integer from ${minimum} through ${maximum}`);
  }
  return value;
}
