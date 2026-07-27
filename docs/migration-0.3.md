# Migrating from 0.2 to 0.3

Nanocodex 0.3 changes ownership and import paths without intentionally removing
0.2 behavior. The main `nanocodex` package is now a thin facade; applications
that already use it can migrate incrementally while lower-level consumers
depend on only the component they need.

## Package moves

| 0.2 package or surface | 0.3 owner | Compatibility |
| --- | --- | --- |
| `nanocodex` runtime | `nanocodex-agent` | Reexported by `nanocodex` |
| `nanocodex-core` | `nanocodex-oai-api` | `nanocodex-core` remains a reexport |
| `nanocodex-service` | `nanocodex-oai-api` | `nanocodex-service` remains a reexport |
| `nanocodex-mcp` | `nanocodex-tools::mcp` | Move imports; no separate 0.3 MCP package |
| `nanocodex-macros::tool` | `nanocodex-tools-macros` | Prefer `nanocodex::tool` or `nanocodex_tools::tool` |
| Nanoeval | `nanocodex-eval`, `nanocodex-eval-harbor` | Existing job inputs remain readable |

`nanocodex-mcp` and `nanocodex-macros` remain at 0.2 on crates.io. Their
implementations are not duplicated in 0.3.

## Construct an agent

Passing an API key directly remains supported. New code can make the OpenAI
boundary explicit and independently configure its concrete Tower stack:

```rust,no_run
use nanocodex::{Nanocodex, OpenAi};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let openai = OpenAi::new(std::env::var("OPENAI_API_KEY")?)?;
let (agent, _events) = Nanocodex::builder(openai)
    .instructions(
        "You are a Rust coding agent. Preserve unrelated work and run relevant tests.",
    )
    .workspace(std::env::current_dir()?)
    .build()?;

let result = agent
    .prompt("Explain the cause of the failing parser test.")
    .await?
    .await?;
println!("{}", result.final_message());
# Ok(())
# }
```

`prompt(...).await` still means that the private driver accepted and ordered the
prompt. The returned `Turn` is now directly awaitable and is also a typed
per-turn stream. `turn.result().await` remains an equivalent convenience.
Cloning `Nanocodex` still creates a cheap command handle to the same session;
`spawn`, `fork`, and `fork_from` create independent drivers with the documented
clean or committed history.

## Session identities and durable state

Explicit session IDs now use the `SessionId` UUIDv7 newtype:

```rust,no_run
use nanocodex::{Nanocodex, OpenAi, SessionId};

# fn build() -> Result<(), Box<dyn std::error::Error>> {
let openai = OpenAi::new(std::env::var("OPENAI_API_KEY")?)?;
let session_id = SessionId::new();
let (_agent, _events) = Nanocodex::builder(openai)
    .instructions("Answer concisely and preserve exact identifiers.")
    .session_id(session_id)
    .build()?;
# Ok(())
# }
```

Parsing a persisted ID rejects malformed UUIDs and UUID versions other than 7.
The version-1 `SessionSnapshot` wire shape from 0.2 is retained. Resume still
creates a fresh transport and tool runtime, drops provider response IDs, and
replays authoritative typed history before healthy incremental continuation.
Codex-compatible rollout reading and writing remain supported.

## Tools and MCP

The dependency-light `Tool` contract and wire types live in
`nanocodex-oai-api`. Implementations, the heterogeneous registry, built-ins,
shell lifecycle, Code Mode, MCP transports, authenticated discovery, and
`tool_search` live in `nanocodex-tools`. MCP is part of a normal native tools
build rather than a facade feature.

Existing macro-based tools only need an import change:

```rust,no_run
use nanocodex::tool;

#[tool(description = "Add two signed 64-bit integers.")]
async fn add(left: i64, right: i64) -> Result<i64, std::convert::Infallible> {
    Ok(left + right)
}
```

## Lower-level Responses sessions

Applications that need Responses without an agent can use the managed state
machine directly:

```rust,no_run
use futures_util::TryStreamExt;
use nanocodex_oai_api::{OpenAi, ResponseEvent};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
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
println!("{}", completed.output_text());
# Ok(())
# }
```

The session owns typed committed history, continuation, reconnect replay, and
atomic compaction. The agent remains responsible for deciding when to compact.

## Usage, events, and evaluation

Completed turns retain exact provider token usage. Supplying a
`PricingSnapshot` adds a typed estimated USD amount with immutable source,
effective date, and rate provenance; missing pricing or usage is explicit and
never reported as zero.

The lossless `AgentEvent` envelope and JSONL adapter remain available.
`AgentEvent::data()` adds normalized run, assistant, reasoning, tool, model,
and context views. Tracing observes the same ordered runtime values under
bounded init4-style operation spans.

Nanoeval workflows now run from this repository:

```sh
nanocodex eval run --task tasks/write-greeting --trials 5 --thinking medium
nanocodex eval inspect .nanocodex/evals/<job-id>
nanocodex eval compare terminal-bench/configure-git-webserver
```

Task definitions and verifiers are not rewritten during migration. New jobs
live beneath `.nanocodex`; supported legacy Nanoeval job metadata remains
readable.

## Publication policy

The facade, OpenAI, tools, agent, browser, React, and eval crates are published
at one synchronized 0.3 version. VM composition and Nanocentaur remain Git/path
components because their reviewed libkrun and managed-deployment dependencies
are not crates.io release inputs.
