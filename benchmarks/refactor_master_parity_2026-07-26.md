# Master feature-parity gate — 2026-07-26

The refactor treats
`master@ad2952b9c8a4e6946440c7501783e814fa72a215` as a behavioral baseline.
Moving code between packages is not evidence by itself: every capability below
has a new owner plus a deterministic test, benchmark, adapter check, or retained
live proof.

The consolidation also preserves the committed temporary-repository surfaces
imported from `nanoeval/master@10aed6b4f67a76c23295c7d418742560def25416`
and Nanocentaur commits `ef15b6c` and `aca2221`. Task definitions and verifiers
remain unchanged.

## Capability ledger

| Baseline capability | Refactored owner | Executable evidence |
| --- | --- | --- |
| Complete typed Responses wire/domain data | `nanocodex-oai-api` | all-feature OAI tests, warnings-denied Rustdoc, request serialization fixtures |
| Persistent WebSocket and HTTPS/SSE, retry/reconnect, continuation, complete replay | `nanocodex-oai-api` | transport, one-complete-attempt Tower, session stream/await, reconnect, replay, and checkpoint tests |
| Batteries-included history and atomic compaction mechanics | `nanocodex-oai-api` | managed-session delta/full-history, failed-partial, compaction replacement, and standalone session tests |
| Compaction timing, follow-on policy, steering, queueing, cancellation | `nanocodex-agent` | focused lifecycle and repaired-history tests summarized in `refactor_agent_baseline_2026-07-26.md` |
| Cheap cloned handle, clean spawn, latest fork, exact `fork_from`, snapshots | `nanocodex-agent` | driver, lineage, concurrent fork, checkpoint, resume, and O(1) benchmark suites |
| UUIDv7 session identity and Codex-compatible rollouts | `nanocodex-oai-api`, `nanocodex-agent` | typed parse/serialization, explicit rollout identity, repair, append, resume, and fork-lineage tests |
| Caller-defined tools and `#[tool]` | `nanocodex-oai-api`, `nanocodex-tools-macros`, `nanocodex-tools` | direct-contract, direct-macro, facade-macro, schema, input/output, and error tests |
| Shell/process tools, plan, patch, image, web, Code Mode | `nanocodex-tools` | runtime, process-group cleanup, nested call, yield/wait, concurrency, cancellation, and WASM checks |
| MCP stdio/HTTP, OAuth, reload, deferred discovery, `tool_search` | `nanocodex-tools::mcp` | integration/stress tests and the 1/1,000-tool search plus warm dispatch benchmark |
| Lossless typed events, JSONL, CLI/TUI, resume, auth, update, MPP | facade and `bin/nanocodex` consumers | workspace tests, CLI parse/integration tests, JSONL/Harbor adapter tests, and live native smoke |
| Node/browser WASM and PyO3 lifecycle adapters | thin binding crates | Rust binding checks, npm runtime/type/package tests, and Python binding tests |
| Full-content tracing and explicit cross-channel parentage | `nanocodex-observability` plus each owner | deterministic capture layers and the retained multi-turn/subagent OTLP stress proof |
| Exact usage and optional versioned USD cost | OAI, agent, adapters, eval, managed API | pricing arithmetic/provenance, typed terminal event, CLI/TUI, JS/Python, ATIF/Harbor, SQLite restart tests |
| Deterministic host browser and React diagnostics | `nanocodex-browser`, `nanocodex-react` | complete direct/remote/Brave/passkey/audit/replay suite and browser baseline |
| Immutable OCI/Dockerfile disks and retained isolated tools | `nanovm-image`, `nanovm`, `nanocodex-vm` | cache-integrity/concurrency tests, retained RPC benchmarks, and signed libkrun smoke |
| Headed browser inside a VM | `nanocodex-browser-vm` | public-API boot/CDP/snapshot/screenshot/shutdown proof |
| MPP and scoped secret egress | `nanocodex-vm-egress` | payment replay, exact-route rejection, rotation/revocation, non-disclosure, HTTP/HTTPS, and unified live VM proof |
| Native, Terminal-Bench, Frontier-Bench, resume, inspect, compare, cleanup | `nanocodex-eval`, `nanocodex-eval-harbor`, `nanocodex eval` | eval parity ledger, durable fixtures, unchanged verifiers, and retained live result artifacts |
| Durable managed sessions, auth/policy, idempotency, replay, fork, recovery | `nanocentaur` | HTTP/SQLite/runtime tests, restart and exact-turn fork tests, egress integration, tracing topology, managed benchmark |

Detailed evidence and numeric budgets are retained in:

- [`refactor_agent_baseline_2026-07-26.md`](refactor_agent_baseline_2026-07-26.md)
- [`refactor_tools_baseline_2026-07-26.md`](refactor_tools_baseline_2026-07-26.md)
- [`refactor_observability_baseline_2026-07-26.md`](refactor_observability_baseline_2026-07-26.md)
- [`refactor_vm_baseline_2026-07-26.md`](refactor_vm_baseline_2026-07-26.md)
- [`refactor_browser_baseline_2026-07-26.md`](refactor_browser_baseline_2026-07-26.md)
- [`refactor_egress_baseline_2026-07-26.md`](refactor_egress_baseline_2026-07-26.md)
- [`refactor_eval_baseline_2026-07-26.md`](refactor_eval_baseline_2026-07-26.md)
- [`refactor_managed_baseline_2026-07-26.md`](refactor_managed_baseline_2026-07-26.md)

## Test-inventory guard

`scripts/audit-master-tests.sh` extracts unique Rust functions carrying a test
attribute from the pinned baseline and current tree. The current inventory is
774 tests versus 497 on master. It fails if any master test name disappears
without an explicit classified replacement.

Two names are intentionally classified:

- `rollout_rejects_a_non_uuid_explicit_session_id` is structurally replaced by
  `SessionId`, whose parser accepts only UUIDv7, plus
  `session_ids_are_serializable_uuid_v7_values` and
  `rollout_uses_an_explicit_typed_session_id`.
- `stress_repeated_tool_search` was an ignored manual loop. It is replaced by
  the reproducible `mcp_tool_search` Criterion suite over 1 and 1,000 tools,
  together with warm stdio dispatch.

The inventory is a deletion alarm, not the semantic proof. The capability
ledger and focused executable gates remain authoritative when tests are renamed
or contracts become structural.

Reproduce the guard with:

```sh
just parity-audit
```
