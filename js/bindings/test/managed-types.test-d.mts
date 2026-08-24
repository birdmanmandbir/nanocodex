import { Agent, ManagedError } from "nanocodex/managed";
import type {
  ManagedAgent,
  ManagedEvent,
  ManagedTurnResult,
} from "nanocodex/managed";

declare const apiKey: string;

async function checkManaged() {
  const created: ManagedAgent = await Agent.create();
  const opened: ManagedAgent = Agent.open("0198d3f0-8844-7000-8000-000000000001");
  const serverAgent = await Agent.get("0198d3f0-8844-7000-8000-000000000001", {
    baseUrl: "https://managed.example",
    apiKey,
  });
  const agents: readonly ManagedAgent[] = await Agent.list({
    baseUrl: new URL("https://managed.example"),
    apiKey,
  });
  const turn = serverAgent.turn.prompt({
    input: [{ type: "text", text: "hello" }],
    idempotencyKey: "request-1",
  });
  const accepted: string = await turn.accepted();
  const result: ManagedTurnResult = await turn.result();
  result.finalMessage;
  result.usage?.input_tokens;
  result.citations[0]?.sources[0]?.cursor;
  const searched = await Agent.searchHistory(
    { query: "remember", limit: 8, agentic: true },
    { baseUrl: "https://managed.example", apiKey },
  );
  searched.answer;
  searched.citations[0]?.thread_id;
  const found = await Agent.findThreads(
    { query: "remember", limit: 8 },
    { baseUrl: "https://managed.example", apiKey },
  );
  const read = await Agent.readThread(
    {
      thread_id: found.results[0]!.thread_id,
      turn_ids: [found.results[0]!.turn_id],
    },
    { baseUrl: "https://managed.example", apiKey },
  );
  read.turns[0]?.assistant;
  read.citations[0]?.sources[0]?.cursor;
  for await (const event of serverAgent.events.watch({ cursor: result.cursor ?? "0" })) {
    const typed: ManagedEvent = event;
    typed.cursor;
    typed.data.type;
  }
  await turn.cancel();
  await created.delete();
  opened.id;
  await Agent.delete(accepted, { baseUrl: "https://managed.example", apiKey });
  new ManagedError("failed", "failed", { status: 500 });

  await Agent.create({
    // @ts-expect-error managed agents never accept provider credentials.
    providerApiKey: "sk-provider",
  });
  await Agent.create({
    // @ts-expect-error managed agents never accept runtime environments.
    env: {},
  });
}

void checkManaged;
