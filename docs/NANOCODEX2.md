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

## Provenance

The terminal component tree is derived from Tact 0.6.6 at
`clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa` under Apache-2.0.
Attribution and the upstream license are in
`bin/nanocodex/third-party/tact/`. Tact's local model, memory, and child-agent
runtimes are not part of Nanocodex2.
