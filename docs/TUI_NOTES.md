# Native TUI

Nanocodex's native terminal application is derived from Tact v0.3.6. The
complete import and adaptation record is in [TACT_UPSTREAM.md](TACT_UPSTREAM.md).

## Interaction model

Submitting while a turn is active adds the prompt to the visible queue. Press
`Tab` to focus that queue. A queued item can be reordered with `Shift+Up` or
`Shift+Down`, edited with `e`, deleted with `d`, or sent into the active turn
with `Enter`. Steering is admitted at the agent's next safe boundary; if the
turn is no longer steerable, the same message is promoted to a new turn. Items
left queued are joined and submitted when the pane becomes idle.

The `/` action menu exposes new and resumed sessions, forks, reasoning effort,
fast mode, themes, config reload/editing, memory, subagents, context diagnostics,
and browser review. `Ctrl+F` forks directly when a completed checkpoint is
available. Each pane owns an independent Nanocodex session, transcript journal,
queue, subagent tree, and cancellation lifecycle.

The global shortcut popup is available from the action menu. Core bindings are:

| Binding | Action |
| --- | --- |
| `Enter` | Submit, or queue while a turn is active |
| `Tab` | Focus the queue when present |
| `Esc Esc` | Interrupt the active response |
| `Ctrl+C Ctrl+C` | Close a split pane, otherwise exit |
| `Ctrl+F` | Fork from the latest available checkpoint |
| `Ctrl+S` | Change reasoning effort |
| `Ctrl+G` | Edit the prompt in `$EDITOR` |
| `Ctrl+O` | Expand or collapse all tool calls |
| `Ctrl+Home` / `Ctrl+End` | Jump to the start / follow the latest output |
| `@` | Insert a workspace file |
| `!` | Run a local shell command from an empty prompt |

## Persistence, memory, and subagents

Sessions are persisted as compressed, append-only transcript journals under
the configured Nanocodex home. The resume picker restores the typed Nanocodex
snapshot and a deterministic projected transcript; forks create independent
session IDs and journals.

Memory is an optional global SQLite store with explicit scan/read/put/replace/
delete operations, optimistic versions, secret rejection, bounded retrieval,
and root-only mutation. See [TACT_MEMORY.md](TACT_MEMORY.md).

Subagents form a bounded per-root task tree. They support structured outputs,
descendant management, cross-branch messaging, deferred delivery, urgent
steering, recursive interruption, and a dedicated TUI tree. See
[TACT_SUBAGENTS.md](TACT_SUBAGENTS.md).

## Performance validation

The `tui_render` Criterion target is the imported Tact suite, compiled from
`bin/nanocodex/src/tui/bench.rs`. It measures first and idle frames, composer
updates, long transcript tails, streaming deltas, collapsed and expanded tool
views, highlighted patches, session catalog/load/projection, and warm/cold
resume behavior against large deterministic archives.

Run it with:

```sh
cargo bench -p nanocodex-bin --bench tui_render
```

Use `just bench-stream` for the focused cross-layer agent-event and TUI gate.
Retained user traces remain outside Git; committed benchmark fixtures contain
only deterministic generated structure.
