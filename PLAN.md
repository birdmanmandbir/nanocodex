# Nanocodex plan

## Objective

Build high-quality reusable Rust building blocks for frontier OpenAI agents.
Nanocodex makes a small number of deliberate choices about libraries, public
APIs, performance, and observability while following the supported Codex
harness behavior exactly. It does not reimplement policy already owned by the
model or harness.

Every stable crate must be useful independently, documented from its own
README, tested through its public paths, benchmarked at the boundaries it can
affect, and observable without adopting the Nanocodex CLI.

## Delivery stack

The refactor ships as three stacked, independently mergeable pull requests.
Part 1 is the completed PR #50 base for the active Part 2 work. Each PR must
preserve behavior available on `master` unless a removal is explicit and
covered by a regression or migration.

### PR 1 — Stable API refactor

1. **Re-establish Codex parity**
   - Treat `openai/codex@35eaf3ffb0bf2001486c68c47a3d946b34d16634`
     as the last authoritative reviewed checkpoint.
   - Inspect and classify every later upstream commit before advancing that
     checkpoint.
   - Differentially verify prompt-cache identity and stable prefixes;
     `AGENTS.md` and environment injection; typed history and
     `previous_response_id`; reconnect/full replay; automatic/manual
     compaction; steering/cancellation; completed-only commits; retries and
     fallback; tool ordering, errors, panics, and process cleanup; and shared
     ChatGPT authentication.
   - Fix demonstrated mismatches test-first. Record intentional differences
     explicitly; do not silently call them parity.

2. **Stabilize crate ownership and public paths**
   - `nanocodex-oai-api` owns the complete OpenAI boundary and honest Tower
     seams.
   - `nanocodex-tools` owns tool implementations, Code Mode, MCP, and deferred
     search.
   - `nanocodex-agent` owns the private driver, lifecycle, state, branching,
     snapshots, and rollouts.
   - `nanocodex` remains a thin Alloy-style facade.
   - Keep mutable run configuration, events plumbing, attempt factories,
     response/turn IDs, queues, sockets, and replay bookkeeping private.
   - Remove accidental exports, compatibility leftovers, duplicate bindings,
     empty directories, unused dependencies/features, and unnecessary cfgs.

3. **Make the stable APIs legible**
   - Give each stable crate a focused README included into crate docs.
   - Put the normal consumer path first and advanced Tower/protocol surfaces
     behind progressive disclosure.
   - Compile complete public examples through canonical paths.
   - Keep `OpenAiBuilder::{layer,service}` as the deliberate transport seam.

4. **Lock in performance and observability**
   - Define representative benchmarks and explicit thresholds for request
     construction, history replay/checkpointing, context accounting and
     compaction, event delivery, tool dispatch, Code Mode, MCP discovery/search,
     and changed TUI state/render work.
   - Follow init4-style bounded spans and explicit parent propagation while
     keeping contractual events independent from tracing.
   - Preserve full-fidelity ordered prompts, model traffic, reasoning and
     encrypted reasoning, tool activity, steering, cancellation, token/cache
     data, latency, and automatic `gpt-5.6-sol` USD cost.

5. **Prove the complete PR path**
   - Validate crate boundaries, formatting, warnings-denied Clippy, workspace
     and all-target tests, rustdoc/doctests/examples, WASM, Node/browser, PyO3,
     CLI/Ratatui, and a live native smoke.
   - Run the stock-Codex differential suite.
   - Terminal-Bench 2.1 milestone evaluation is delegated to the user's
     separate thread. This thread does not bootstrap Harbor, alter eval inputs,
     or wait on that result.
   - Fix every real PR #50 CI failure and leave required checks green with no
     known merge blocker.

PR 1 was verified at `c82205d` as mergeable and clean with green required
checks before Part 2 was rebased onto its completed tip.

### PR 2 — `nanocodex-eval` and required VM machinery

1. Consolidate the temporary Nanoeval evaluator and Harbor/ATIF projection into
   this workspace as `nanocodex-eval`.
2. Expose evaluations through `nanocodex eval <...>` with Harbor-compatible
   task, verifier, artifact, JSONL, ATIF, token, latency, and USD accounting.
3. Consolidate the complete VM lifecycle, image/cache, guest protocol, and
   VM-backed workspace tools in one `nanocodex-vm` crate, including
   Dockerfile-derived pre-snapshotted disks and reusable pre-baked images.
   Keep host tools as the normal default while allowing `nanocodex run`, the
   interactive TUI, and resumed TUI sessions to opt into one retained VM tool
   session explicitly.
4. Support full Terminal-Bench 2.1 and FrontierBench runs, including Daytona
   execution where configured. Do not weaken tasks or verifiers.
5. Produce on-demand PR build artifacts so a run can select a Nanocodex binary
   built from a pull request without enabling expensive evals on every change.
6. Separate cold image/bootstrap time from warm agent work and retain exact
   run artifacts for comparisons.

The following is the planned completion gate for this PR, not a claim that the
sweep scheduler already implements it:

- Use [`docs/GPT_5_6_EVALS.md`](docs/GPT_5_6_EVALS.md) as the source inventory
  and plot/drilldown contract for the initial GPT-5.6 tuning loop.
- A sweep records the exact dataset, task, and verifier revisions; Nanocodex
  build; model and configuration identifiers; agent, tool, and prompt
  parameters; seed and repetition; and pricing revision. Any such parameter can
  vary. Retained aggregate data must support OpenAI-style comparison plots whose
  primary axes are success rate, estimated cost, and latency, with drilldown to
  every task/configuration attempt.
- Turbo runs execute configurations concurrently with explicit, configurable
  bounds. Queueing, concurrency, and cancellation policy remain visible in the
  retained run configuration.
- The allocation invariant is one retained VM per task across a parameter
  sweep, never one VM per configuration. Configurations run concurrently in
  isolated directories inside that task VM; only the immutable task
  image/bootstrap state and VM lifetime are shared.
- Every configuration starts from an identically seeded private workspace and
  process group, receives collision-free ports and temporary paths, writes
  independent verifier output, and cannot observe mutable state from another
  configuration.
- Every attempt retains its exact inputs, ordered outputs and events,
  trajectory, verifier artifacts and output, resolved configuration, token and
  cost accounting, and latency breakdown. Cold image/bootstrap time is
  attributed separately from scheduler queue wait and warm agent, model, tool,
  and verifier work.
- `nanocodex eval ...` can run the full configured suite against a selected
  local or PR build through the consolidated Rust/library path, and comparison
  records remain useful without rerunning the benchmark.

PR 2 is complete when `nanocodex eval ...` can run the full configured suite
against a selected local or PR build using the consolidated Rust/library path.

Current Part 2 status (2026-07-28): the two crate boundaries, high-level
retained VM workspace API, native and per-attempt KVM execution, Harbor/ATIF
projection, durable resume, plot-ready aggregates, explicit host/VM selection,
Linux musl host build, lean guest build, and focused live native/KVM evidence
are implemented. New jobs retain the exact executable digest, Git/build
identity, model, tool profile, pricing revision, seed status, scheduling
policy, task-package digest, ordered evidence, and separate queue, readiness,
cold image/cache, and warm-attempt timing. The allocation gate above is not
yet implemented: the current eval adapter owns one agent VM and, where needed,
one verifier VM per attempt. Its single guest RPC runtime has no tenant
namespace boundary, so sharing it concurrently would let one configuration
address another configuration's paths. Completion requires a task-scoped
environment lifecycle plus tenant-scoped guest filesystem/process namespaces,
then full configured-suite and bounded representative sweep evidence. Do not
describe the current per-attempt adapter as one-VM-per-task isolation.

### PR 3 — Experimental managed-agent components

1. Add experimental browser-on-VM, Centaur durability/managed-agent work, and
   related proxy components under `crates/experimental/` where they are
   reusable libraries.
2. Keep executables and Tempo-specific integration under `bin/`.
3. Add the egress-VM boundary that encapsulates MPP payments and secrets egress
   without adding Tempo dependencies to stable Nanocodex crates.
4. Reuse the VM and eval foundations from PR 2; do not duplicate their runtime
   or artifact model.
5. Require a concrete consumer, focused tests, tracing, and benchmarks before
   promoting any experimental component into the stable crate graph.

## Current execution order

1. [x] Complete the [Codex parity ledger](docs/CODEX_PARITY.md) from the pinned
   checkpoint through local `openai/codex@3418498f01422f5f650ea645d4bd19e05c3a9616`.
2. [x] Finish the behavior-preserving rollout, model/run, tool/runtime, and
   driver module decompositions.
3. [x] Audit stable public paths, crate docs, examples, dependencies, features,
   cfgs, and crate boundaries.
4. [x] Verify each parity contract and fix confirmed mismatches test-first.
5. [x] Finish benchmark thresholds and full-fidelity observability verification.
6. [x] Run all in-scope consumer, differential, and smoke gates. Terminal-Bench
   milestone evaluation is delegated to the user's separate thread.
7. [x] Commit and push coherent PR #50 slices, remediate CI, and verify at
   `c82205d` that the pull request is `MERGEABLE`/`CLEAN` with green required
   checks.

## Current non-goals

- No provider abstraction, generic app server, compatibility layer, approval
  subsystem, or alternate agent runtime.
- No audio implementation work.
- No new `.service(...)` transport design without a concrete consumer.
- No cosmetic CLI/TUI lifecycle rewrite when existing behavior is accepted.
- No browser, managed-agent, proxy, or other Part 3 experimental-crate work.
- No benchmark, task, or verifier modification made solely to improve an eval
  score.
