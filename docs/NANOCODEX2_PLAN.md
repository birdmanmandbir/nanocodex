# Nanocodex2 plan

## Outcome

Ship `nanocodex2` as an application-owned Rust terminal client for
account-owned managed Nanocodex agents. Its terminal presentation and
interaction model track Tact 0.6.6 at
`clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa`; its engine is the
Nanocodex managed-agent API.

Nanocodex2 never constructs a local `Nanocodex` agent, opens an OpenAI transport, or
accepts `OPENAI_API_KEY`. It authenticates with an account-issued
`NANOCODEX_API_KEY` (`ncx_live_...`) and sends it only to the configured
`NANOCODEX_MANAGED_URL`. Provider credentials, conversation history, tools,
model sockets, and turn execution remain owned by the managed service.

“1:1 with Tact's TUI” means every user-visible Tact 0.6.6 terminal state and
interaction: layout, composer, transcript, streaming, actions, selection,
themes, keyboard and mouse controls, queues, session restore and search,
forks, settings, subagent and memory panels, review and handoff flows, and
responsive behavior. Golden PTY captures from the pinned Tact checkpoint are
the presentation authority. Nanocodex2 does not ship a control that is absent,
disabled, or behaviorally reduced merely because the current managed API lacks
the operation; that gap is fixed at the managed boundary first.

Tact-owned local model construction is replaced, not copied. Its updater and
release pipeline are distribution concerns outside TUI parity. Memory,
subagents, forks, auxiliary review/handoff inference, and any other visible
agent-backed feature must run through managed agents without a hidden local
model fallback.

The Tact-derived binary is Apache-2.0 and carries source attribution to the
pinned upstream checkpoint. Stable Nanocodex library crates remain unchanged
unless Nanocodex2 demonstrates a reusable SDK defect independently of the hosted
product contract.

## Progress

- [x] Create the isolated `feat/nanocodex2` worktree and pin the Tact parity
  checkpoint.
- [x] Add the second Rust binary target and the typed managed REST/SSE client.
- [x] Import the pinned Tact terminal component tree under Apache-2.0
  attribution, without importing its local model runtime.
- [x] Import the account-scoped managed history search and canonical
  Rust-backed subagent implementation from the supplied
  `nanocodex-memory-search` working slice.
- [x] Finish slice 1 with the shared replayable subscriber/cache layer and a
  real managed-agent detach/resume smoke.
- [x] Finish adapting the pinned Tact TUI to the managed lifecycle.
- [x] Prove two independent Nanocodex2 processes can create/join one managed
  room, exchange replayable messages, and address the room's private managed
  agent without sharing account or provider credentials.
- [x] Saturate the managed-agent and multiplayer Durable Object paths with a
  bounded Rust harness, fix each owning bottleneck, and rerun the affected ramp.
- [x] Close every missing managed capability required by a visible Tact flow.
- [x] Pass the PTY parity, representative replay, performance, and release
  gates.

## Ownership

- `bin/nanocodex` owns a second `nanocodex2` binary target, its configuration,
  secret wrapper, managed HTTP/SSE client, terminal state, event projection,
  and local UI preferences.
- `services/managed` continues to own account authorization, durable agents,
  accepted turns, cursor replay, cancellation, deletion, workspace policy,
  tools, and provider egress.
- Nanocodex2 stores no transcript authority. A small mode-`0600` state file may
  retain only non-provider routing state needed to reopen an owned agent and
  resume after its last fully processed cursor.
- The existing `nanocodex` binary and Ratatui consumer remain independent.
  Nanocodex2 does not become a second stable SDK surface or force its hosted
  policy into `nanocodex-agent`.

## Backend mapping

| Nanocodex2 behavior | Managed operation |
| --- | --- |
| New conversation | `POST /v1/agents` |
| Conversation picker | `GET /v1/agents` summaries |
| Restore transcript | backward pages from `GET /v1/agents/:id/events/history` |
| Follow live output | resumable `GET /v1/agents/:id/events` SSE |
| Submit | idempotent `POST /v1/agents/:id/turns` |
| Await completion | terminal event from the shared replayable stream |
| Steer | `POST /v1/agents/:id/turns/:turn/steer` |
| Cancel | `POST /v1/agents/:id/turns/:turn/cancel` |
| Delete | `DELETE /v1/agents/:id` |
| Reconnect | reopen the retained agent and continue strictly after the saved cursor |
| Create multiplayer room | account-authenticated `POST /v1/rooms` |
| Join multiplayer room | invite-capability `POST /v1/rooms/:id/join` |
| Shared room replay/live stream | member-cookie WebSocket `GET /v1/rooms/:id/ws?cursor=...` |
| Ask the room's managed agent | room WebSocket `say` with `target: "agent"` |
| Account history candidates | `POST /v1/history/sessions/search` |
| Exact retained session turns | `POST /v1/history/sessions/:id/read` |
| Account memory list | `GET /v1/memory` |
| Account memory read | `GET /v1/memory/:id` |

Completed managed turns project idempotently into the account's `MemoryScope`.
The terminal may search and render that source, but it must not recreate a
local memory database or emulate Tact's local memory mutations. Agentic history
search runs through the managed service's canonical Rust subagent task tree;
Nanocodex2 contains only task-tree presentation and never owns a child-agent
scheduler.

The Rust client follows the existing JavaScript client contract in
`js/bindings/managed/Agent.mjs`: strict IDs and cursors, three idempotent
submission attempts for transport failures only, shared replayable event
delivery, terminal-result caching, and typed server errors. It does not add a
second server protocol.

## Ordered implementation

### 1. Rust managed-client vertical slice

- Add the unpublished `nanocodex2` target in the existing `nanocodex-bin`
  package with `run`, `new`,
  `list`, `resume`, and interactive default entry points.
- Read the managed origin and account API key from environment/config without
  accepting the key in process arguments or persisting it.
- Implement typed create/list/get/delete, turn submit/state/steer/cancel,
  history pagination, and resumable SSE in Rust.
- Prove strict bearer routing, response validation, idempotent transport retry,
  monotonic cursor handling, reconnect, cancellation, and secret-safe errors
  against a deterministic local HTTP server.
- Exercise one real managed agent from a PTY with the provider credential
  absent from Nanocodex2's environment and process state.

Exit: `nanocodex2 run` can create or resume an account-owned agent, stream
canonical events, await the typed terminal result, detach, and resume without
duplicating a turn.

### 2. Tact terminal shell and static parity

- Port the Tact component tree, terminal lifecycle, scheduler, theme, composer,
  transcript model, markdown/diff/image rendering, selection, clipboard,
  editor, floating panels, and responsive layout from the pinned checkpoint.
- Preserve upstream module boundaries where they still describe UI ownership;
  replace every local-engine call with a typed managed operation. Keep the
  complete visible TUI even when its owning managed operation must be added.
- Add Apache attribution and a short parity ledger that classifies every
  top-level Tact subsystem as ported, adapted to a managed capability, or a
  non-TUI distribution concern.
- Derive deterministic terminal fixtures from the pinned Tact checkpoint at
  representative desktop and narrow sizes.

Exit: without a network, Nanocodex2 renders the same empty, composing,
streaming, tool, completed, error, picker, and overlay states with the same key
and mouse behavior.

### 3. Managed event projection

- Project managed `AgentEvent` values into the Tact transcript model without
  flattening known events to display strings.
- Keep acceptance, model/tool/reasoning streams, plans, patches, shell output,
  images, usage, terminal state, and errors ordered by durable cursor.
- Batch bursty SSE delivery and redraws with Tact's scheduler so rendering cost
  depends on the live tail rather than retained transcript size.
- Rehydrate from history pages before attaching at the captured durable head;
  deduplicate the replay-to-live handoff by cursor.

Exit: a real managed coding turn is visually and interactively equivalent to
the same retained event workload in Tact, and reload/reconnect produces no
missing or duplicate transcript rows.

### 4. Complete interactive lifecycle

- Wire submit, queued follow-ups, live steering, queue editing, cancellation,
  retryable/blocked failures, deletion, and clean terminal restoration through
  the Rust managed client.
- Map Tact's session picker to managed agent summaries and event-history search;
  keep the managed agent ID private from normal transcript presentation.
- Persist only the last owned agent and acknowledged cursor with restrictive
  permissions. Recover unfinished accepted turns from durable state.
- Preserve Tact's completion hook, external editor, clipboard, prompt history,
  theme reload, and local-only presentation settings where they do not create
  another agent engine.

Exit: new, resume, disconnect during inference, reconnect, follow-on, steer,
cancel, and delete all pass through visible controls in a real PTY.

### 5. Managed-capability closure

- Compare every Tact action with the capabilities returned by the managed
  agent. Add the missing managed service operations for fork/branch, model and
  effort changes, auxiliary review/handoff turns, skills, memory, subagents,
  and any other agent-backed TUI behavior before wiring the corresponding
  control.
- Keep durable conversation and tool ownership in the managed service. Local
  TUI persistence may cache presentation but may not emulate a missing server
  lifecycle.
- Run auxiliary inference through scoped managed agents and delete disposable
  auxiliaries after their durable terminal result. Never issue a hidden local
  model call.

Exit: the parity ledger has no visible or behavioral gap. Every Tact TUI action
works through a managed capability and has the same enabled/disabled state,
feedback, cancellation, and recovery behavior at the pinned checkpoint.

### 6. Representative performance and release gate

- Replay retained Codex rollout traces and long Amp thread exports at multiple
  terminal sizes, including streaming bursts and long-history tails.
- Record state-update throughput, frame construction, rendered frame count,
  changed cells/output volume, retained memory, input-to-frame latency, resize,
  reconnect, and history-hydration behavior against both Tact and the current
  Nanocodex Ratatui consumer.
- Run focused rustfmt, warnings-denied Clippy, client/component tests, PTY
  journeys, crate-boundary checks, and a live managed-agent smoke.
- Document installation, API-key issuance, secret handling, managed origin,
  recovery, capability gaps, attribution, and exact validated checkpoints.

Exit: Nanocodex2 passes the Tact parity fixtures and real PTY journeys, remains
responsive on representative long sessions, and all inference observed during
validation belongs to managed agents.

### 7. Multiplayer and Durable Object saturation

- Add room create/join/connect/send/replay commands whose membership cookie is
  retained only in process memory. The creator prints a bounded invite URL;
  the joiner pastes it through standard input so the capability is absent from
  process arguments and normal logs.
- First prove two independent CLI processes observe the same ordered member
  events and managed-agent reply, then reuse that exact Rust transport in the
  load harness rather than maintaining a synthetic second protocol client.
- Ramp allocator pressure, concurrent rooms, members per room, room-message
  fanout, managed-agent turns, reconnect/replay storms, deliberately stalled
  acknowledgements, and create/delete churn independently before combining
  them. Record request and frame throughput, p50/p95/p99 latency, durable cursor
  lag, reconnect recovery, error classes, Worker/DO logs, CPU time, storage
  operations, alarms, retries, and cost-relevant operation counts.
- Treat documented quotas and explicit backpressure as expected outcomes.
  Stop a ramp on credential leakage, unrelated-service impact, uncontrolled
  spend, sustained data loss/reordering, unrecovered Durable Object failures,
  or a rising error/latency curve whose owning boundary is already clear.
- Fix the highest boundary that owns each demonstrated bottleneck, deploy the
  coherent slice, rerun the failing stage, and push the measured result before
  expanding concurrency further.

Exit: the two-CLI room journey passes locally and on a disposable Cloudflare
deployment; the saturation ledger names the tested envelope, first limiting
resource, intended quota behavior, defects found, fixes adopted, and the
retested throughput/latency envelope.

## Stop conditions

- Stop if Nanocodex2 is about to receive an OpenAI or ChatGPT provider credential.
- Stop if a UI workaround would become a second conversation history or turn
  authority instead of fixing the managed boundary.
- Stop if copying a Tact subsystem adds distribution policy unrelated to the
  1:1 terminal experience.
- Stop a parity or performance claim on the first authoritative PTY or replay
  mismatch, fix the owning boundary, and rerun from that boundary.
