<div align="center">

<h1>Nanocodex</h1>

<p><strong>Building blocks for frontier OpenAI agents.</strong></p>

[![CI](https://img.shields.io/github/actions/workflow/status/gakonst/nanocodex/ci.yml?branch=master)][ci]
[![Crates.io](https://img.shields.io/crates/v/nanocodex.svg)][crates]
[![Docs.rs](https://img.shields.io/docsrs/nanocodex)][docs]
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)][license]

**[Thesis](#thesis)** · **[Agent API](#the-agent-api)** ·
**[Components](#components)** · **[Performance](#performance-is-a-contract)** ·
**[Plan](PLAN.md)**

[ci]: https://github.com/gakonst/nanocodex/actions/workflows/ci.yml
[crates]: https://crates.io/crates/nanocodex
[docs]: https://docs.rs/nanocodex
[license]: LICENSE-MIT

</div>

---

Nanocodex is a Rust toolkit for building frontier OpenAI agents. It provides a
Tower-native OpenAI Responses client, model-facing tool contracts, MCP and Code
Mode, context management, and an owned agent lifecycle. The same repository is
growing downward into isolated VMs, headed browser workers, policy-aware
egress, and reproducible evaluations.

The components are designed to work together, but each implementation crate
must also have a coherent API when used on its own. The top-level `nanocodex`
crate is an intentionally thin, Alloy-style facade with a small prelude.

> The repository is being reorganized around this API in a stack of reviewable
> changes. [`PLAN.md`](PLAN.md) is the active execution plan;
> [`REFACTOR.md`](REFACTOR.md) retains the design record that led to it.

## Thesis

Nanocodex starts with three opinions.

### Small, excellent building blocks

Agent infrastructure is easier to understand and improve when the pieces have
sharp ownership and useful APIs. A Responses client should be usable without
an agent loop. Tools should be usable without a CLI. A VM should not know what
an evaluation is. The high-level agent should compose those parts rather than
hide a second implementation of them.

### The model and harness are co-designed

We do not try to outsmart behavior that the model and Codex harness already
make explicit. Instructions, `AGENTS.md`, tool shapes, response ordering,
compaction, cache identity, continuation, reconnect replay, cancellation, and
process cleanup are part of the model-facing contract.

Codex is evidence rather than an API to clone. Nanocodex preserves the
invariants that affect agent behavior while giving them a smaller,
library-first Rust interface.

### Performance is part of API design

Every hot component owns representative `cargo bench` workloads and explicit
performance budgets. Optimizations must improve retained, realistic traces;
refactors must preserve asymptotic contracts such as constant-time forks and
delta-sized healthy turns. End-to-end evals measure the model–harness pair, not
the model in isolation.

## The agent API

The golden path is one owned agent session, one cheap cloneable handle, and an
optional independent event stream:

```rust,ignore
use nanocodex::{Nanocodex, OpenAi};

let openai = OpenAi::new(std::env::var("OPENAI_API_KEY")?)?;

let (agent, _events) = Nanocodex::builder(openai)
    .instructions(
        "You are a Rust coding agent. Make focused changes, preserve unrelated \
         work, and run relevant tests before finishing.",
    )
    .workspace(std::env::current_dir()?)
    .build()?;

// Awaiting prompt means the turn was accepted and ordered.
let turn = agent
    .prompt("Find the cause of the failing test and explain it.")
    .await?;

// Awaiting the turn drains its stream and returns the complete typed result.
let result = turn.await?;
println!("{}", result.final_message());
```

`Turn` is both a typed stream and a future for its completed `TurnResult`.
Normal consumers can stream one turn directly; `AgentEvents` remains the
session-wide firehose for adapters, tracing, JSONL, and durable recording.

Turns can be steered or cancelled without exposing internal IDs:

```rust,ignore
let turn = agent
    .prompt("Investigate the parser regression and prepare a fix.")
    .await?;
let control = turn.control();

control
    .steer("Do not edit Cargo.lock; keep the change inside the parser crate.")
    .await?;

let result = turn.await?;
```

Follow-on prompts reuse retained history automatically. `agent.clone()` is
another handle to the same ordered driver. `spawn()` creates a clean sibling;
`fork()` branches from the latest safe committed boundary; and
`fork_from(&result)` branches from the exact completed checkpoint represented
by that result.

The agent owns policy: its tool loop, `AGENTS.md` discovery, compaction timing,
workspace behavior, cancellation, and branching. It does not expose response
IDs, socket tasks, queue capacities, or mutable conversation internals.

### Usage, cost, and events

Every completed turn reports exact aggregate token counts. USD is estimated
automatically from the provider usage using OpenAI's published `gpt-5.6-sol`
standard or priority rates:

```rust,ignore
use nanocodex::{Nanocodex, OpenAi};

let openai = OpenAi::new(std::env::var("OPENAI_API_KEY")?)?;
let (agent, _events) = Nanocodex::builder(openai)
    .instructions("Answer concisely and preserve exact identifiers.")
    .build()?;

let result = agent.prompt("Explain the identifier req_7f3.").await?.await?;
println!("{} tokens", result.usage().total_tokens());
if let Some(cost) = result.usage().estimated_cost() {
    println!("estimated {}", cost.amount());
} else {
    println!("cost unavailable: {}", result.usage().cost_status().as_str());
}
```

The same exact estimate appears in the terminal typed event, JSONL adapter,
tracing spans, CLI, and language bindings. `CostStatus` reports when the
provider omitted usage, so missing accounting data is never reported as a
zero-dollar turn.
`AgentEvent::data()` exposes normalized run, assistant, reasoning, tool, model,
and compaction events. The original raw payload remains available for a
lossless OpenAI and transport firehose.

## The Responses API without the agent

`nanocodex-oai-api` is the lower-level, Tower-native OpenAI client. Its managed
session owns typed history, usage, continuation state, reconnect replay, and
the persistent transport. It does not decide when an agent should compact.

```rust,ignore
use futures_util::TryStreamExt;
use nanocodex_oai_api::{OpenAi, ResponseEvent};

let openai = OpenAi::new(std::env::var("OPENAI_API_KEY")?)?;
let mut session = openai
    .instructions(
        "Remember user-provided facts and say when required information is missing.",
    )
    .build()?;

let mut turn = session.turn();
let mut response = turn.create("Remember that the deployment region is us-west-2.");

while let Some(event) = response.try_next().await? {
    if let ResponseEvent::OutputTextDelta(delta) = event {
        print!("{delta}");
    }
}

let completed = response.await?;
assert!(completed.output_text().contains("us-west-2"));
```

`Session` is a client-side state machine, not a provider-side session resource.
One `ResponseTurn` corresponds to one logical agent turn and retains
turn-scoped protocol state across multiple Responses calls. Tool outputs and
steering become subsequent `create(...)` calls on that same turn. The agent
may explicitly call `turn.compact().await?` at a safe boundary.

`Response` implements:

```rust,ignore
Stream<Item = Result<ResponseEvent, ResponseError>>
IntoFuture<Output = Result<CompletedResponse, ResponseError>>
```

A response commits only after the provider's terminal completion event.
Dropping or failing a partial response commits no history and executes no
tool. A replacement connection drops its dead continuation ID and replays the
complete authoritative typed history.

## Tools

The dependency-light tool contract lives with the Responses types in
`nanocodex-oai-api`. `nanocodex-tools` supplies the heterogeneous registry,
built-ins, shell and process lifecycle, MCP, `tool_search`, and Code Mode.
MCP is part of the tools crate, not an optional public subsystem.

Application tools can implement the trait directly or use `#[tool]`:

```rust,ignore
use nanocodex::tool;

#[tool(description = "Add two signed 64-bit integers.")]
async fn add(
    left: i64,
    right: i64,
) -> Result<i64, std::convert::Infallible> {
    Ok(left + right)
}
```

The macro generates the same `Tool` implementation accepted by the registry.
Tool schemas, inputs, outputs, errors, and call context are typed; workspace,
agent-driver, MCP-connection, and process-manager state stay in higher
implementations.

## Evaluations

`nanocodex-eval` runs immutable tasks through fresh agent sessions and
workspaces. Scheduling is bounded by both concurrency and task-declared memory;
completed attempts are durable and an interrupted finite sweep resumes without
re-running committed results. Events are optional and independent from typed
results.

```rust,ignore
use nanocodex::{Nanocodex, OpenAi, Thinking};
use nanocodex_eval::{Evaluator, Sweep, Task};

let agent = Nanocodex::builder(OpenAi::new(std::env::var("OPENAI_API_KEY")?)?)
    .instructions(
        "Work directly in the provided workspace. Complete the requested task, \
         verify your changes, and keep the final answer concise.",
    )
    .thinking(Thinking::Medium);
let sweep = Sweep::builder()
    .task(Task::load("tasks/write-greeting")?)
    .agent("gpt-5.6-sol-medium", agent.clone())?
    .trials(5)
    .build()?;
let (evaluator, _events) = Evaluator::builder(agent)
    .output_directory(".nanocodex/evals")
    .max_concurrency(4)
    .max_memory_mb(16_384)
    .resume_incomplete(&sweep)
    .build()?;

let results = evaluator.sweep(sweep).await?;
println!("{} fresh attempts", results.attempts().len());
```

`nanocodex_eval::harbor` projects an independent event subscription into
canonical Harbor and ATIF artifacts; it does not become a second result owner.
The CLI composes native execution, VM image preparation, Terminal-Bench,
Frontier-Bench artifact handoff, inspection, published-result comparison, and
cleanup:

```sh
nanocodex eval --task tasks/write-greeting --trials 5 --thinking medium
nanocodex eval prepare --task /path/to/terminal-bench-task
nanocodex eval inspect .nanocodex/evals/<job-id>
nanocodex eval compare terminal-bench/configure-git-webserver
nanocodex eval cleanup .nanocodex/evals --dry-run
```

Normal agent sessions keep host workspace tools by default. The TUI, one-shot
runner, and resume command can instead route `exec_command`, `write_stdin`,
`apply_patch`, and `view_image` through one retained VM:

```sh
nanocodex --vm .nanocodex/vm/session-rootfs.ext4 --vm-workspace /app
nanocodex run "inspect the repository" \
  --vm .nanocodex/vm/session-rootfs.ext4 --vm-workspace /app
```

The rootfs is writable and modified in place, so it must be a session-private
copy rather than an immutable eval cache entry. Web search, image generation,
MCP, and `update_plan` remain independently configured host-side capabilities.

Estimated USD cost is retained and printed automatically from provider-reported
usage and Nanocodex's built-in pricing catalog. Existing Nanoeval jobs and
environment overrides remain readable during migration, but new state is
written beneath `.nanocodex`.

## Components

The core dependency direction is:

```text
nanocodex                         thin facade, modules, and prelude
├── nanocodex-agent              owned agent lifecycle and policy
│   ├── nanocodex-tools          tool runtime, MCP, tool_search, Code Mode
│   │   ├── nanocodex-oai-api    shared tool and Responses contracts
│   │   └── nanocodex-tools-macros native #[tool] implementation
│   └── nanocodex-oai-api        Tower client, sessions, context, Responses
├── nanocodex-oai-api            direct provider-module reexport
├── nanocodex-tools              direct tools-module reexport
└── nanocodex-observability      native observability-module reexport
```

The facade root is the canonical common path. Detailed APIs live under their
owning `nanocodex::agent`, `nanocodex::oai`, or `nanocodex::tools` module;
lower-level consumers can instead depend on the corresponding component crate.

Tempo-specific application code stays under `bin/`:

```text
bin/nanocodex/src/mpp/egress.rs private paid-egress implementation
bin/nanousd                       shared private credits protocol
bin/nanousd-api                   credits service
```

The monorepo also contains independently useful systems components. VM and
evaluation components live under
[`crates/experimental/`](crates/experimental/README.md):

| Component | Responsibility |
| --- | --- |
| `nanocodex-vm` *(experimental)* | VM lifecycle and images plus retained guest-backed workspace tools |
| `nanocodex-eval` *(experimental)* | Typed tasks and sweeps, durable results, and Harbor/ATIF projection |

The CLI is a consumer of these libraries. Evaluation is exposed as
`nanocodex eval ...`; it does not install Nanocodex into every task image or
move model decisions into Python.

## Performance is a contract

We distinguish deterministic library budgets from noisy live service
measurements.

Hard contracts include:

- fork and completed-checkpoint creation are O(1) over retained history;
- a healthy incremental turn serializes the new delta rather than cloning full
  history;
- retry reuses replayable owned state and cannot duplicate a partial side
  effect;
- event fanout shares raw payloads, preserves lossless monotonic order, and
  skips serialization as soon as every receiver is dropped;
- VM images are prepared once and attempts start from cheap immutable
  snapshots or reflinks;
- browser and eval hot paths never rebuild unchanged images; and
- cancellation terminates subprocess groups, VM work, and descendants.

Each owning crate carries benchmarks for its public hot paths. Retained trace
fixtures cover realistic response streams, long conversations, tool bursts,
VM startup, browser actions, and eval scheduling. Numeric regression gates are
set only after the benchmark has a reproducible fixture and recorded baseline.
Live model and network measurements remain trends with raw artifacts, not
machine-independent unit-test thresholds.

The target for normal turns is simple: the model and network dominate the
critical path. Traces separate provider wait from queueing, serialization,
parsing, event delivery, and tool work, while independent work runs as bounded
siblings under init4-style spans. Token usage and built-in OpenAI pricing also
produce one consistent estimated USD cost for library results, the CLI, and
evals.

Existing measurements and their methodology live under
[`benchmarks/`](benchmarks/) and [`docs/`](docs/). The refactor ports them next
to their new owners rather than discarding the evidence.

## Installation

The facade crate:

```sh
cargo add nanocodex
```

Lower-level consumers can depend only on the component they need:

```sh
cargo add nanocodex-oai-api
cargo add nanocodex-tools
cargo add nanocodex-agent
```

`nanocodex-oai-api` enables its Tower transports and managed session client by
default on native targets. Native `nanocodex-tools` builds include MCP,
`tool_search`, Code Mode, image processing, and remote tools; MCP is not an
optional compatibility layer.

The experimental VM and evaluation crates are not published on crates.io.
`nanocodex-vm` currently tracks a reviewed libkrun Git checkpoint, so Git/path
consumers select them explicitly:

```sh
cargo add nanocodex-vm --git https://github.com/gakonst/nanocodex
cargo add nanocodex-eval --git https://github.com/gakonst/nanocodex
```

The daily-driver CLI is available on macOS and Linux:

```sh
curl -fsSL https://nanocodex.paradigm.xyz | bash
```

## Development

Run `cargo doc --open` from the repository root to browse the public
`nanocodex` facade and follow its links into the lower-level crates.

The normal local gates are:

```sh
./scripts/check-crate-boundaries.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
```

Run focused `cargo bench -p <crate>` suites while changing a hot component.
Use `just run` for a live native agent smoke and the configured eval commands
only at milestone gates. Eval tasks and verifiers are evidence; they are never
modified to make Nanocodex pass.

## License

Licensed under either of:

- Apache License, Version 2.0
- MIT License

at your option.
