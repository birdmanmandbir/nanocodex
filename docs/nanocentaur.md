# Nanocentaur managed agents

Nanocentaur is the experimental host-managed agent layer built from Axum,
Nanocodex, SQLite, `nanocodex-vm`, and `nanocodex-egress`. The library owns
durable agent identity and policy; the server is a thin executable consumer.

```text
HTTP / SSE
    |
    v
Axum handlers
    |
    +--> SQLite policy
    |      API key + context key -> principal -> grants + agent config
    |
    `--> bounded agent command queue
             |
             v
         per-agent actor
           - live Nanocodex handle
           - active turn control
           - explicit queued turns
           - disposable VM workspace
             |
             v
         SQLite session actor
           - accepted turns and idempotency keys
           - append-only typed event log
           - completed session snapshots
```

The actor owns live runtime state. SQLite remains authoritative across actor
eviction and process restart. Each wake creates a fresh model transport and VM,
then resumes from the last completed typed `SessionSnapshot`.

Live actors retain only unfinished turn controls and inputs plus the latest
completed snapshot. Completed turn views and old idempotency records remain in
SQLite instead of growing every actor heap for the lifetime of a session.

## Agent API

```text
POST   /v1/agent/new
GET    /v1/agent/:agent_id
DELETE /v1/agent/:agent_id
POST   /v1/agent/:agent_id/evict

POST   /v1/agent/:agent_id/turn
GET    /v1/agent/:agent_id/turn/:turn_id
POST   /v1/agent/:agent_id/turn/:turn_id/cancel
GET    /v1/agent/:agent_id/events

POST   /v1/agent/:agent_id/fork
POST   /v1/agent/:agent_id/turn/:turn_id/fork

POST   /v1/payment-sessions
GET    /health
```

Create or resolve an agent:

```http
POST /v1/agent/new
X-API-Key: ...
Content-Type: application/json

{"context_key":"opaque-client-owned-key"}
```

The optional context key is scoped to the authenticated API client. It gives
create-or-return behavior but does not carry authority.

Send ordered typed content:

```http
POST /v1/agent/d18a.../turn
X-API-Key: ...
Idempotency-Key: client-message-42
Content-Type: application/json

{
  "content": [
    {"type":"text","text":"Inspect the failure"}
  ]
}
```

The default `steer` delivery follows the owned Nanocodex turn state: an idle
agent starts a turn; a running agent receives steering. A completion race falls
back from `TurnNotSteerable` to a new prompt. `"delivery":"enqueue"` always
uses the driver's FIFO prompt queue. A full steering queue returns
`409 steer_queue_full` and is never silently converted to enqueueing.

The SSE stream spans turns and supports durable replay:

```http
GET /v1/agent/d18a.../events
Accept: text/event-stream
Last-Event-ID: 42
```

Managed control events and native `nanocodex_agent::events::AgentEvent` values
remain typed through SQLite and SSE projection. Managed lifecycle events carry
the durable turn ID. Native runtime events remain session-ordered with no
synthetic turn attribution, so delayed or absent optional events can never
delay a typed turn result or be assigned to the wrong queued turn.
Runtime-event bursts share bounded SQLite transactions, and replay is fetched
in bounded pages before switching to the live broadcast tail.

## Durability and forks

`$STATE_DIRECTORY/sessions.sqlite` stores agents, turns, input blocks,
idempotency mappings, events, and fork lineage. Acceptance of a turn, its first
input, its idempotency mapping, and its accepted event are one transaction.

A process restart marks incomplete running turns interrupted, moves them back
to queued, and wakes them under the same managed turn ID. Only completed model
boundaries are persisted as Nanocodex snapshots.

A fork copies durable model history through a selected completed turn into a
new agent identity. It receives a fresh VM root and does not copy guest
filesystem mutations. Applications that need durable artifacts must export
them explicitly.

## Policy and authorization

The administration surface uses
`Authorization: Bearer $NANOCENTAUR_ADMIN_TOKEN`. It manages:

- API clients and rotatable API keys
- principals and context bindings
- roles and permissions
- principal/role grants
- host-resolved secret routes

Roles contain grants. A principal's agent configuration owns instructions and
reasoning effort, avoiding ambiguous prompt merging across roles. Effective
permissions are checked before each managed operation; agent configuration is
snapshotted at creation.

## Managed secret egress

Secret policy stores references, never values. The built-in server registers:

- `environment`: resolves `NANOCENTAUR_SECRET_<KEY>`
- `file`: resolves a bounded safe relative path under `--secret-directory`

A route fixes the upstream, allowed methods and path prefixes, header delivery,
and public guest variables. For example:

```json
{
  "id": "openai",
  "name": "OpenAI",
  "source": {"provider":"environment","key":"OPENAI"},
  "upstream": "https://api.openai.com",
  "rules": [{"methods":["POST"],"path_prefixes":["/v1/"]}],
  "delivery": {
    "type":"inject_header",
    "header":"authorization",
    "prefix":"Bearer "
  },
  "guest": {"base_url_env":"OPENAI_BASE_URL"}
}
```

For each VM wake, Nanocentaur creates an authenticated loopback proxy and an
ephemeral interception CA using `nanocodex-egress`. The guest receives proxy
variables, the public CA, origins, and public placeholders. The host resolves a
secret only after destination, method, path, active agent, principal, and live
route-specific grant checks pass. Revocation therefore affects the next
request even when several routes use the same host secret reference. A route
configuration change also fails closed on an existing proxy until policy
refresh replaces its VM runtime, preventing a rotated credential from being
sent through stale destination rules. The proxy owner is retained by the VM
egress lease and disappears with that runtime.

Capability routes are independent. No requested network capability produces a
network-disabled lease; configured capabilities may select direct internet or
one server-owned external proxy.

## Running

Mock backend:

```bash
cargo run -p nanocentaur-server -- serve \
  --api-key local-development-key \
  --admin-token local-admin-key \
  --backend mock
```

Real VM-backed Nanocodex backend with a directory root containing
`/usr/local/bin/nanocodex-vm-guest`:

```bash
cargo run -p nanocentaur-server -- serve \
  --api-key "$NANOCENTAUR_API_KEY" \
  --admin-token "$NANOCENTAUR_ADMIN_TOKEN" \
  --backend nanocodex \
  --rootfs ./rootfs \
  --openai-api-key "$OPENAI_API_KEY" \
  --allow-capability github.read
```

Raw ext4 roots additionally require `--vm-guest-runtime ELF`; the server
prepares its read-only runtime disk through the current `nanocodex-vm`
contract. Use `--firmware-directory` when libkrun firmware is not discoverable
through the platform loader.

The deterministic HTTP benchmark exercises authentication, SQLite durability,
actors, turns, and polling without model or VM cost:

```bash
cargo run -p nanocentaur-server -- bench \
  --api-key local-development-key \
  --agents 32
```

MPP payment sessions remain opt-in:

```bash
cargo run -p nanocentaur-server --features mpp -- serve ...
```

Authorization and policy checks run before payment verification, so rejected
callers are not charged. Concurrent requests sharing one agent-scoped
idempotency key are serialized across lookup, payment authorization, and
durable acceptance, preventing duplicate authorization for the same turn.
