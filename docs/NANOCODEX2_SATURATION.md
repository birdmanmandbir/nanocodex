# Nanocodex2 managed saturation ledger

This is the execution ledger for stressing the account-managed agent and
multiplayer-room paths with the same Rust transports used by `nanocodex2`.
Traffic must target one explicitly configured Nanocodex deployment. Provider
credentials, membership cookies, and invite capabilities are never written to
logs or retained artifacts.

## Known boundaries

| Boundary | Current limit |
| --- | ---: |
| unfinished turns per managed agent | 16 |
| managed WebSockets per agent | 64 |
| SSE subscribers per agent | 32 |
| active multiplayer rooms | 16 |
| multiplayer room creations per hour | 32 |
| room lease | 2 hours |
| guest invite uses | 31 |
| room WebSockets | 64 total, 4 per member |
| unfinished managed-agent jobs per room | 16 |
| replay batch | 16 events or 64 KiB |
| room agent turns | 6/member/minute, 60/room/hour, 240/deployment/hour |
| room chat | 30 events or 64 KiB/member/minute |
| room aggregate chat | 240 events or 512 KiB/room/minute |
| append-only event log | 64 MiB, with terminal reserve |

The deployment-wide `MultiplayerQuota` singleton is on every room allocation,
release, and room-agent admission. Within a room, `#sayTail` serializes budget
checks, event append, idempotency binding, and managed-agent job creation.
Fanout then schedules catch-up work per socket. Measurements identified a
separate, accidental singleton in the egress subject directory as the first
real cross-room bottleneck; subject ownership is now sharded by subject. The
deployment-wide quota object intentionally remains a singleton.

## Measured evidence

The following disposable Cloudflare waves used the repository driver and the
same Rust transport as `nanocodex2 room`. Every completed wave revoked its
temporary API key, deleted all run-owned rooms, and removed both disposable
Workers.

| Wave | Result | High-signal latency |
| --- | --- | --- |
| canary: 1 room, 3 clients, 1 chat, 1 hosted turn | 9/9 chat deliveries, 3/3 terminal deliveries, 3/3 replay | clean cleanup |
| mixed: 4 rooms, 9 clients/room, 6 chats/guest, 1 hosted turn/room | 196 sends, 1,764 chat deliveries, 36 terminal deliveries, 36 replay | clean cleanup |
| max transport: 8 rooms, 16 clients/room, 8 chats/guest | 256 connect/reconnect, 960 sends, 15,360/15,360 fanout, 128/128 replay | create p99 3.319s; connect p99 15.336s; fanout p99 682ms; replay p99 15.887s |
| hosted-turn validation: 2 rooms, 2 prompts/room | 4/4 turns, 12/12 terminal deliveries, 6/6 replay | terminal p99 6.949s |
| hosted-turn wave: 8 rooms, 4 prompts/room, 4 observers/room | 32/32 turns, 128/128 live terminal deliveries, 32/32 replay | terminal p50 7.654s; p95 12.239s; p99/max 12.325s |

The max transport wave completed in 36.581 seconds with no failed operation or
invariant violation. The corrected 32-turn wave completed in 23.894 seconds;
all terminals were normal `messages`, with no rate-limit or error terminal.

## Bottlenecks found and fixed

1. Fresh disposable deployments intermittently returned 500 while Durable
   Object classes propagated. The driver now records a bounded 10-second warmup
   before account bootstrap and retries only the cold bootstrap boundary.
2. Concurrent room initialization serialized through one global
   `AgentSubjectDirectory` Durable Object, causing `room_initialization_failed`
   at 8-room admission. Subject bind/resolve ownership now uses
   `subject-v1:<subject>` shards; the same 8-room stage then admitted 8/8 rooms.
3. A transient create response could lose the caller's durable identity. The
   Rust client now retries the exact serialized create body and `create_id`
   across the server's initialization alarm window.
4. The first multi-room hosted-turn wave keyed terminals by cursor alone.
   Cursors are room-local, so valid terminals collided in the harness. Terminal
   identity is now `(room, accepted_cursor)` and the full 32-turn wave passed.
5. At the 128-client local envelope, the default macOS 256-descriptor ceiling
   caused exactly 10 client-side WebSocket upgrade failures. Raising the
   harness shell to 4,096 descriptors produced exact 15,360-message fanout;
   this was a load-generator ceiling, not a Durable Object failure.

## Ordered ramps

1. Canary: one agent lifecycle and one three-member room. Prove replay,
   reconnect, managed-agent reply, deletion, and complete cleanup.
2. Account cardinality: create/list/run/delete at concurrency 1, 2, 4, 8, 16,
   32, and 64. No application quota is expected here.
3. Managed connections: approach 64 WebSockets and 32 SSE subscribers, then
   prove the next connection receives the documented 429.
4. Managed turns and idempotency: fill 16 unfinished turns, hammer identical
   IDs/keys concurrently, mutate one identity field at a time, and require one
   acceptance/model execution with exact 200 replay or 409 conflict behavior.
5. Cursor recovery: disconnect and reconnect from seeded cursors, including
   malformed and ahead cursors. Require exclusive ordered replay followed by a
   duplicate-free live handoff.
6. Room allocator: replay one `create_id`, fill 16 active rooms, replace a
   deleted lease, and exercise the non-refundable 32-creation hourly counter.
7. Membership and connections: replay `join_id`, exhaust 31 invite uses, fill
   four sockets per member and 64 per room, and verify exact limit responses.
8. Fanout: grow members and sockets independently, then run deterministic
   multi-producer chat bursts. Require identical global cursors at every fast
   client and exact idempotent replay.
9. Room managed-agent FIFO: fill 16 jobs, exercise member/room rate limits, and
   run the deployment-wide 240-turn boundary only under an explicit model-spend
   budget.
10. Replay and slow consumers: exceed one replay batch, delay or omit ACKs,
    stop reads, and reconnect in storms. Paused clients must remain fenced while
    fast clients continue without gaps or duplicates.
11. Churn and recovery: race create/join/delete/reconnect, abort responses after
    writes, exercise idle unload, and optionally redeploy only the owned
    disposable Worker.
12. Soak the last stable mix. Reach event-log exhaustion only in a disposable,
    budgeted object and stop after proving terminal reserve and cleanup.

Each concurrency level uses warmup, three fixed measurement windows, and a
drain/cleanup phase. A failed invariant stops downstream ramps until the owning
boundary is fixed and that stage passes again.

## Measurements

- offered, accepted, completed, replayed, conflicted, throttled, and failed
  operations per second;
- HTTP status and typed error code, WebSocket close code/reason, retry count,
  and `Retry-After`;
- p50/p90/p95/p99/max for request acceptance, upgrade-to-ready, first event,
  turn terminal, fanout delivery, replay catch-up, and settled deletion;
- cursor gaps, duplicates, regressions, terminal uniqueness, replay fences,
  per-client lag, and first-to-last fanout spread;
- account/room cardinality, roster/presence correctness, unfinished-job age,
  cleanup ledger size, and orphaned resources;
- harness CPU, RSS, descriptors, sockets, and network, kept separate from
  Cloudflare invocation duration, errors, storage operations, alarms, and
  model latency.

Every run emits bounded JSONL and a final manifest containing the branch
revision, managed deployment revision, seed, stage configuration, histograms,
error classes, invariant failures, safety budget consumption, and cleanup
result.

## Stop conditions

Expected limit responses stop only that dimension. Stop the whole wave and
preserve evidence on any cursor loss/reordering, duplicate acceptance or
terminal, cross-account/room data, credential/private-ID exposure, unexpected
HTTP 500 or WebSocket 1011, success above a hard cap, quota rejection below a
cap, cleanup that does not converge, or fast-client loss caused by a stalled
client.

Hold the current level when p99 exceeds twice the prior stable stage for three
windows, unexpected failures exceed 0.5% for one minute, or the low-rate canary
p95 doubles. Stop producers immediately when the configured rooms, agents,
sockets, model turns, bytes, wall time, or estimated spend budget is reached.
Drain observers and delete only run-owned resources recorded in the in-memory
cleanup ledger. Normal completion, typed failure, timeout, SIGINT, and SIGTERM
run cleanup. SIGKILL cannot execute cleanup; disposable Workers are uniquely
named for operator removal and rooms retain the bounded two-hour lease.
