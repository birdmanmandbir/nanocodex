# Nanocodex2

Nanocodex2 is the Tact-style terminal client for account-owned Nanocodex
managed agents. The terminal process is a Rust control-plane client: model
sockets, conversation history, tools, memory, and child-agent execution stay in
the managed service.

## Build and run

```sh
cargo build -p nanocodex-bin --bin nanocodex2

export NANOCODEX_MANAGED_URL=https://your-managed-nanocodex-origin.example
export NANOCODEX_API_KEY=ncx_live_...

target/debug/nanocodex2 run
```

`NANOCODEX_API_KEY` must be an account-issued `ncx_live_...` key. Nanocodex2
does not read `OPENAI_API_KEY`, accept a provider credential, or persist the
account key. Keep the key out of command-line arguments because process lists
and shell history commonly expose them.

`nanocodex2 run` opens the interactive terminal. Supplying a prompt runs the
same managed lifecycle headlessly and emits durable events as JSONL:

```sh
nanocodex2 run "inspect this repository"
nanocodex2 run --agent <owned-agent-id> "continue"
```

The CLI also exposes focused managed operations through `new`, `list`, `state`,
`turn`, `watch`, `history`, `steer`, `cancel`, and `delete`. Run
`nanocodex2 --help` for their exact arguments.

## Put two CLIs in one room

The room owner needs an account key. An invited guest does not. Keep the invite
out of argv: the join command reads it from standard input so the fragment
capability is not exposed through a process listing.

In terminal 1:

```sh
export NANOCODEX_MANAGED_URL=https://your-managed-nanocodex-origin.example
export NANOCODEX_API_KEY=ncx_live_...
nanocodex2 room create --name Alice
```

Copy the printed `Invite:` URL. In terminal 2:

```sh
export NANOCODEX_MANAGED_URL=https://your-managed-nanocodex-origin.example
nanocodex2 room join --name Bob
```

Paste the invite at the prompt. A plain line sends room chat; `/room <message>`
is the explicit form. `/agent <prompt>` addresses the room's private hosted
agent and streams the same durable result to both terminals. `/quit` closes the
current membership connection.

## Controlled room load

`load` uses the exact Rust room transport used by the interactive commands. It
checks population, cursor ordering, fanout, hosted-agent terminals, reconnect
replay, and settled deletion, then emits one bounded JSON summary. The built-in
limits are 8 rooms, 15 guests per room, 8 messages per guest, 4 hosted prompts
per room, and 900 seconds.

```sh
ulimit -n 4096
nanocodex2 load \
  --rooms 8 \
  --guests-per-room 15 \
  --messages-per-guest 8 \
  --agent-prompts-per-room 4 \
  --replay \
  --max-seconds 180
```

The higher descriptor limit is a client-host requirement at this envelope. A
macOS shell left at its common 256-descriptor limit fails WebSocket upgrades in
the harness before the Durable Objects are saturated.

For disposable Cloudflare infrastructure, the repository-owned driver creates
uniquely named Workers, issues and revokes a temporary account key, invokes the
Rust binary, and removes the Workers in its cleanup path:

```sh
NANOCODEX2_LOAD_BINARY="$PWD/target/debug/nanocodex2" \
NANOCODEX2_LOAD_ROOMS=8 \
NANOCODEX2_LOAD_GUESTS_PER_ROOM=15 \
NANOCODEX2_LOAD_MESSAGES_PER_GUEST=8 \
NANOCODEX2_LOAD_AGENT_PROMPTS_PER_ROOM=4 \
npm run smoke:cloudflare:multiplayer --prefix services/managed
```

See [NANOCODEX2_SATURATION.md](NANOCODEX2_SATURATION.md) for the measured
envelope, failures found, and fixes verified.

## Hosted memory and subagents

The managed service owns memory and subagent execution. Nanocodex2 only renders
their typed events and invokes account-scoped managed operations. It contains
no local memory database, local child-agent scheduler, provider client, or
provider credential fallback. The managed memory/search implementation was
integrated from `/private/tmp/nanocodex-memory-search.3vdEWJ/`; that source path
is development provenance, not a runtime dependency.

## Provenance

The terminal component tree is derived from Tact 0.6.6 at
`clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa` under Apache-2.0.
Attribution and the upstream license are in
`bin/nanocodex/third-party/tact/`. Tact's local model, memory, and child-agent
runtimes are not part of Nanocodex2.
