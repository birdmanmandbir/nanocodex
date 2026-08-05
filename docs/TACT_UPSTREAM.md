# Tact upstream

Nanocodex's native terminal application, agent extensions, memory subsystem,
and browser review client are derived from
[clabby/tact](https://github.com/clabby/tact), released under Apache-2.0.

The imported checkpoint is Tact v0.3.6, commit
`1d9ccaefd1d8613dab020812af04a91cd9b4c52c` (2026-08-04). The imported surface
includes Tact's `app`, `core`, `review`, and `tui` modules, its Criterion TUI
benchmark, its memory and subagent design notes, and `web/review`.

Nanocodex modifies that source to:

- retain Nanocodex's existing CLI, authentication, OpenAI transport, model,
  Tempo MPP, browser, VM, MCP, observability, and shutdown boundaries;
- construct every TUI pane, fork, and subagent from the same cloneable
  Nanocodex runtime recipe;
- use `NANOCODEX_HOME`, `NANOCODEX_CONFIG`, and Nanocodex release artifacts;
- brand user-visible terminal and review surfaces as Nanocodex;
- keep the reviewed Tact Ratatui stack isolated with dependency aliases; and
- conform the imported implementation and tests to this workspace's lint,
  unsafe-code, release, and CI policies.

The original Apache-2.0 terms are available in the repository's
[`LICENSE-APACHE`](../LICENSE-APACHE). Files carrying a Tact derivation notice
have been modified from that checkpoint.
