# Ask Nanocodex for Raycast

This local Raycast extension is a thin UI over `nanocodex/node`. The Node
entrypoint hosts the same Rust/WASM agent as the browser package; it does not
spawn the Nanocodex CLI. Nanocodex owns the agent lifecycle and `mppx` owns the
caller-funded Tempo payment session.

## Develop

Build the Nanocodex JavaScript/WASM package first, then install and import the
extension:

```sh
cd js/bindings
npm install
npm test

cd ../../examples/raycast
npm install
npm run build
npm run dev
```

The development command imports `Ask Nanocodex` into the running Raycast app.
It can then be stopped; the imported local extension remains available.

## Use as the default fallback

In Raycast Settings, open **Launcher → Fallback Commands**, add
**Ask Nanocodex**, move it to the first position, and remove **Quick AI** if it
is present.

The supported flow is:

```text
Cmd-Space → type a question → Enter
```

Raycast passes the root query as `fallbackText`. The extension durably queues
the prompt and launches a no-view background command. That worker preloads the
Rust/WASM runtime and active Tempo Wallet in parallel, silently reuses a pending
or published access key from `~/.tempo/wallet/store.json`, reuses a persisted
MPP payment channel, and streams the Nanocodex answer. It opens Tempo Wallet
only when the delegated key is missing or expired.

The command uses `nanocodex-tui`'s framework-independent transcript reducer.
Reasoning summaries, assistant output, tool activity, plans, retries, and
warmup status are checkpointed in Raycast's extension support directory and
stream into the same conversation model as the browser TUI. Closing or popping
the chat view only detaches that UI; it does not cancel the turn. Reopen **Ask
Nanocodex** to see the live job under **Background Jobs**.

After the first answer, type in the Raycast search bar and press Enter to send
a follow-up. Submitting while a turn is active queues one follow-up. The
background worker serializes jobs that share the MPP channel and reuses the
same warm Agent for adjacent turns on the same thread.

Opening the command without a query shows recent and archived conversations
from `~/.codex/sessions`. Selecting one streams its retained typed history into
a fresh WASM driver, preserving its thread and prompt-cache lineage. New
Raycast conversations receive a normal Codex UUID and rollout; each completed
turn is durably appended to that JSONL file, so later Raycast and Codex sessions
see the same conversation. The extension refuses to append if another process
has changed the rollout in the meantime.

The chat stays mounted as one Raycast detail row and batches streaming events
before rendering. This avoids rebuilding and reselecting a list row for every
token, while reasoning, tool activity, warmup, retries, and follow-on turns
remain live.

Raycast still owns the worker process and can terminate commands that exceed
its background execution limit or when Raycast itself quits. Job state is
durable: reopening the command relaunches an interrupted job from the last
completed conversation boundary. Each completed rollout turn carries the job
ID, so recovery detects a turn that was saved immediately before a worker exit
instead of submitting it twice.
