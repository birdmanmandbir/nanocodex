# Nanocodex2 imported-TUI compatibility

This report covers the Tact TUI imported from `clabby/tact` at
`a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa` (Tact 0.6.6). The compatibility code is private to
the `nanocodex2` binary and intentionally contains no agent engine, child-agent runtime, memory
persistence, or network behavior.

## Implemented model surface

`compat/memory.rs` preserves the complete UI-visible shapes and derives from `tact-memory` 0.6.6:

- `MemoryKey`, including `local`, `remote`, and `is_local`
- `MemoryRecord`
- `MemorySource`
- `RemoteRole`
- `MemoryAccess`

The fields are the exact fields constructed, compared, sorted, filtered, and rendered by
`tui/components/{app,memory,root}.rs`. Serde names remain compatible with Tact's memory wire
values.

`compat/subagents.rs` preserves the complete UI-visible shapes and pure behavior from
`tact-subagents` 0.6.6:

- `AgentId`, `MessageId`, `ThreadId`, and `SubagentRuntimeId`
- `MessageSender`, `MessagePriority`, `MessagePurpose`, `MessageDisposition`, and
  `MessageDeliveryState`
- `AgentMessage`, `AgentThread`, and `AgentMessageUpdate`
- `AgentStatus`, including the exact `is_active` predicate used by the tree filter
- `AgentDescriptor`, `AgentUpdate`, and `ScopedAgentUpdate`

The ID wrappers retain ordering, hashing, display, and transparent serde behavior. Directed-message
types retain Tact's tagged serde shapes. `AgentDescriptor` deliberately continues to use
`nanocodex::Model`, and `AgentUpdate::Event` continues to carry
`nanocodex::agent::events::AgentEvent`, because the imported transcript projection consumes those
types directly. `get` accessors and public-to-this-binary constructors are the only integration
extensions to the original model surface.

The eventual binary root can preserve the unmodified `tact_memory::...` and
`tact_subagents::...` paths by declaring `mod compat`, reexporting the appropriate model items at
the binary crate root, and aliasing the crate root with `extern crate self as tact_memory` and
`extern crate self as tact_subagents`. Runtime items listed below must have owners before those
aliases cover every current import.

## Exact terminal dependency aliases

The imported UI must not bind to the package's existing workspace versions (`ratatui` 0.29.0 and
`crossterm` 0.28.1). It was authored and pinned against the following package identities:

```toml
crossterm-tact = { package = "crossterm", version = "=0.29.0", features = ["event-stream", "osc52"] }
ratatui-tact = { package = "ratatui", version = "=0.30.2" }
ratatui-image-tact = { package = "ratatui-image", version = "=11.0.6", default-features = false, features = ["crossterm"] }
```

The copied source now refers to these crates as `crossterm_tact`, `ratatui_tact`, and
`ratatui_image_tact`. Exact (`=`) requirements are recommended for the compatibility boundary;
plain `"0.x.y"` Cargo requirements admit later semver-compatible releases.

`ratatui-image` 11.0.6 must resolve against the same Ratatui 0.30 line used by the UI. Its
`SlicedProtocol`, `SlicedImage`, `SignedPosition`, `Picker`, and `ProtocolType` participate directly
in component fields and widget rendering, so substituting Ratatui 0.29 produces distinct,
incompatible Rust types.

The imported markdown code also requires its already-selected parser identity:

```toml
pulldown-cmark-tact = { package = "pulldown-cmark", version = "=0.13.4", default-features = false }
```

## Version-sensitive Ratatui and Crossterm API inventory

The owning compatibility reason for Ratatui 0.30.2 is `tui/terminal.rs`: both custom `Backend`
implementations use the 0.30 associated `Backend::Error` and return `Result<_, Self::Error>` from
`draw`, cursor, clear, size, window-size, and flush methods. The workspace Ratatui 0.29 `Backend`
contract is not source-compatible with these implementations. The TUI also directly relies on
0.30 `Frame::area`, `Frame::buffer_mut`, `Frame::set_cursor_position`, `Position`, `Rect::contains`,
`Buffer`/`Cell` access, `Terminal::draw`, `Terminal::insert_before`, `TestBackend`, and the
`WindowSize`/`ClearType` backend types. These calls and the image widget implementations must all
come from one Ratatui package identity.

The Crossterm 0.29 surface used by the TUI is:

- `EventStream` behind `event-stream`, plus key press/repeat kinds, paste, focus, resize, mouse,
  and modifier events
- bracketed-paste, focus-change, mouse-capture, and keyboard-enhancement commands
- alternate-screen and raw-mode lifecycle commands
- synchronized-update commands
- `clipboard::CopyToClipboard` behind `osc52`
- the `execute!` and `queue!` macros

In particular, `CopyToClipboard` plus the `osc52` feature is not supplied by the existing
Crossterm 0.28.1 dependency configuration. Keeping terminal events and commands on the explicit
0.29 alias also prevents accidental cross-version type mixing.

## Unresolved APIs and owning boundaries

The following imports remain deliberately unresolved by `compat`; implementing them here would
create the runtime behavior this slice forbids:

- `tact_memory::MemoryError`, `MemoryStore`, and `SelectedMemoryStore`. Current TUI calls require
  `SelectedMemoryStore::{local,source,access}` and `MemoryStore::{list,put,delete}`. A managed/local
  memory owner must provide real compare-and-swap conflict reporting (`MemoryError::Conflict`) and
  access provenance. A fake or no-op store would make delete/reload behavior misleading.
- `tact_subagents::Subagents`. Current TUI calls require `new`, `clone`, `runtime_id`,
  `set_max_concurrency`, `set_thinking`, `set_fast_mode`, `cancel_all`, and `close_all`, plus an
  unbounded `ScopedAgentUpdate` receiver. The managed engine must own child creation, lifecycle,
  cancellation, and update delivery. The model module supplies the DTOs that adapter should emit.
- `AgentUpdate::Event` still expects Nanocodex's normalized `AgentEvent`. If the managed service
  does not expose equivalent child event envelopes, the engine must either translate durable
  managed events at that boundary or explicitly disable child transcript inspection; the UI model
  cannot infer missing tool/reasoning/lifecycle events.
- The imported TUI's direct `nanocodex::{Nanocodex, AgentEvents, TurnControl, TurnResult,
  SessionSnapshot}` lifecycle references are outside this compatibility slice. They must be
  replaced by the managed engine adapter before the TUI module is linked.

No unsupported method silently succeeds in this compatibility layer. Unresolved behavior remains a
compile-time integration obligation rather than a false UI success path.
