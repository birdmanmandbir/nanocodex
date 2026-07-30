# Nanocodex on Cloudflare Durable Objects

This example runs the real Rust/WASM Nanocodex harness inside one SQLite-backed
Durable Object per agent session. The front Worker only authenticates session
creation and routes capability URLs; the object owns the agent, OpenAI
Responses WebSocket, client sockets, typed history, tools, and durable commits.

```text
client WebSocket ──> Worker router ──> one NanocodexSession object
                                         ├─ Rust/WASM Nanocodex driver
                                         ├─ persistent OpenAI WebSocket
                                         ├─ hibernatable client sockets
                                         └─ SQLite snapshot + terminal turns
```

Hot follow-on turns reuse the same WASM agent, cache identity, typed history,
and upstream socket. Each completed turn atomically commits its Nanocodex
snapshot and terminal client result. Failed partial turns never enter durable
history. Repeating a completed client turn ID returns the stored terminal result
without another model call; duplicates received during cold initialization are
coalesced before the first await for the same reason.

An outbound WebSocket prevents a Durable Object from hibernating while it is
retained. A one-shot idle alarm therefore shuts down Nanocodex after 30 seconds
(configurable), closing the OpenAI socket. Client WebSockets use Cloudflare's
hibernation API and remain connected. Their next command wakes the object and
resumes complete client-owned typed history from SQLite. See Cloudflare's
[Durable Object lifecycle](https://developers.cloudflare.com/durable-objects/concepts/durable-object-lifecycle/)
and [WebSocket hibernation](https://developers.cloudflare.com/durable-objects/best-practices/websockets/)
documentation for the underlying behavior.

## Run locally

From the repository root, build the optimized WASM package and install the
example:

```sh
just build-wasm
npm ci --prefix examples/cloudflare-workers
```

Create `examples/cloudflare-workers/.dev.vars` (it is ignored by Git):

```dotenv
OPENAI_API_KEY=your-key
NANOCODEX_ADMIN_TOKEN=local-admin-token
AGENT_IDLE_TIMEOUT_MS=1000
```

Start workerd in one terminal and run the live probes in another:

```sh
npm run dev --prefix examples/cloudflare-workers
npm run smoke --prefix examples/cloudflare-workers
npm run stress --prefix examples/cloudflare-workers
```

The smoke performs a real model turn, verifies duplicate suppression, waits for
idle teardown, then proves that a follow-on remembers history after the agent
is reconstructed. The stress probe defaults to 32 client sockets and 4,096
round trips against one object. Override `NANOCODEX_WORKER_URL`,
`NANOCODEX_ADMIN_TOKEN`, `NANOCODEX_STRESS_CLIENTS`, or
`NANOCODEX_STRESS_PINGS` as needed.

For deterministic transport development without model usage, run
`npm run mock:openai`, set
`OPENAI_WEBSOCKET_URL=ws://127.0.0.1:8790` in `.dev.vars`, and run the same
smoke. The mock speaks the actual streamed Responses protocol, so the full
Rust/WASM driver, snapshot, idle shutdown, and restore paths still execute.

## Validate and deploy

```sh
npm run check --prefix examples/cloudflare-workers
cd examples/cloudflare-workers
npx wrangler secret put OPENAI_API_KEY
npx wrangler secret put NANOCODEX_ADMIN_TOKEN
npx wrangler deploy
```

`npm run check` type-checks, runs the Worker-native Durable Object suite
(including forced eviction with a live hibernatable client socket), and asks
Wrangler to build the complete WASM deployment without uploading it.

`POST /sessions` requires `Authorization: Bearer $NANOCODEX_ADMIN_TOKEN` and
returns a random session ID plus WebSocket URL. The session ID is then a bearer
capability: do not log or expose it. Production applications can replace this
small router policy with their own authentication while leaving the object and
Nanocodex lifecycle unchanged.

Client WebSocket commands are JSON objects:

```jsonc
{ "type": "prompt", "id": "client-turn-1", "input": "Hello" }
{ "type": "steer", "id": "client-turn-1", "input": "Be concise" }
{ "type": "cancel", "id": "client-turn-1" }
{ "type": "status" }
{ "type": "ping", "nonce": "health-1" }
```

The object streams contractual Nanocodex events as `{ "type": "event", ... }`
and emits exactly one application terminal message, `turn_completed` or
`turn_failed`, for each accepted client turn.
