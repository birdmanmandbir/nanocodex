# nanocodex-rlm

`nanocodex-rlm` is an unpublished, application-owned recursive language-model
runtime built only from Nanocodex's public agent and tool APIs. It gives Code
Mode an evolving catalog of async subagent functions while leaving
conversation, transport, and tool-runtime ownership in each spawned
`Nanocodex` driver.

Prime Agent supplies the semantic model: orchestration is programmatic,
children start asynchronously, clean children may recurse, and supplemental
harness state describes reusable subagents. Tact supplies evidence that this
belongs in a consumer using `tools_factory`, `AgentHandle::spawn`, typed events,
and explicit cleanup rather than in the stable Nanocodex agent crates. Neither
project's exact model-facing API is copied.

## Contract

The Responses API exposes Nanocodex's stable `exec` and `wait` Code Mode
boundary. RLM operations are provided by a dynamic runtime provider, so their
names, descriptions, and schemas are not rendered into the model-visible
`exec` definition. Each fresh JavaScript cell receives their current metadata
through `ALL_TOOLS` and their callable async functions through `tools`.

The stable runtime control operations are:

- `spawn`: start a clean child and return its identity without awaiting its
  final answer;
- `list`: inspect the current recursive family;
- `send`: deliver a follow-up or steering message;
- `wait`: wait for messages or lifecycle changes without polling;
- `interrupt`: stop active work while retaining the child; and
- `close`: terminally stop a retained child subtree and join cleanup.
- `harness_state`: read current prompt notes, memories, skills, subagent specs,
  and refinement history;
- `harness_apply`: persist one small evidence-backed CRUD edit;
- `harness_rollback`: restore an archived revision as a new revision; and
- `refine_harness`: asynchronously ask a clean refiner to inspect one
  trajectory observation and apply at most one justified edit.

The native adapter exposes those controls as `tools.spawn_agent`,
`tools.list_agents`, `tools.send_agent_message`, `tools.wait_agent`,
`tools.interrupt_agent`, `tools.close_agent`, `tools.harness_state`,
`tools.harness_apply`, `tools.harness_rollback`, and `tools.refine_harness`.
Each enabled harness subagent additionally materializes as a runtime function:

```js
const available = ALL_TOOLS.filter(({ name }) =>
  name.startsWith("subagent__")
)

const child = await tools.subagent__runtime_investigator({
  task: "Trace cancellation ownership and report concrete evidence."
})
```

The generated function starts a clean child and returns its identity without
waiting for completion. Creating, updating, disabling, deleting, or rolling
back a subagent refreshes this catalog. A new function is visible in
`ALL_TOOLS` in the next fresh cell and is callable immediately by name through
the `tools` proxy. None of those changes rebuild the Responses request prefix.

## Prompt and harness inputs

`PromptPack::load` reads this immutable launch bundle:

```text
prompts/
├── orchestration.md   root-facing programming guidance
├── subagent.md        guidance injected into every clean child
├── refiner.md         immutable background-refinement policy
└── tools.toml         runtime descriptions for stable control operations
```

`HarnessSnapshot::load` reads a separate evolving TOML document:

```toml
version = 1
revision = 3

[[prompt_notes]]
id = "delegate-independent-work"
text = "Delegate independent investigations concurrently."
enabled = true

[[memories]]
id = "flaky-probe"
name = "Flaky probe"
content = "Retry a transient probe once before changing implementation."

[[skills]]
id = "verify-small-diff"
name = "Verify a small diff"
description = "Runs focused checks after a local change."
instructions = "Format, run the focused test, then inspect the exact diff."

[[subagents]]
id = "runtime_investigator"
name = "Runtime investigator"
description = "Traces ownership, cancellation, and cleanup."
instructions = "Cite concrete files and report unresolved ownership questions."
enabled = true
```

All inputs are validated and content-addressed before an agent starts. Subagent
IDs contain only ASCII letters, digits, and underscores so each ID maps to one
unambiguous `subagent__<id>` JavaScript function. The `LaunchSnapshot` records
the immutable prompt identity and initial harness identity. During a run,
`HarnessStore` serializes edits, increments the revision, atomically replaces
the TOML file, archives the previous document for rollback, and refreshes a
synchronous runtime projection consumed when Code Mode starts a cell. Root and
retained children share the latest revision.

The immutable orchestration prompt is appended to the selected agent
instructions and shared by roots and clean children. An accepted revision never
rewrites it or the Responses tool definitions. The provider changes only the
functions materialized inside Code Mode. The complete supplemental snapshot is
also appended as a developer-context delta through
`AgentHandle::append_developer_message`. When a turn is active, Nanocodex queues
that delta and commits it after the turn, before the next prompt. The Responses
request prefix, tool order, stable prompt-cache key, and all preceding
conversation bytes therefore remain unchanged. New clean children receive the
latest selected specification before their first prompt.

## Evaluation

PR #72's ordinary `Sweep` is the first consumer. A baseline and RLM recipe use
the same task, trial, model, thinking, workspace, VM, verifier, and executable.
The RLM arm additionally writes `agent/rlm-evidence.json` before verification,
retaining the launch digest, initial and final harness revisions, refinement
history, recursive agent tree, linked child events, child terminal results,
aggregate usage, and cleanup status. PR #72's stock
`AgentResult` still reports root usage only, so cost comparisons must add the
evidence file's child usage; merging recursive accounting into the stock
aggregate is intentionally left explicit rather than silently undercounted.

The example runs attempts sequentially because all RLM attempts intentionally
share one durable harness. A failed or successful trajectory can therefore
produce a small revision that the next attempt observes. For a held-out score,
copy the selected TOML revision to a fresh path and set
`RlmPolicy::with_harness_refinement(false)`; never let held-out tasks tune their
own treatment.

The example's cache key is derived only from the immutable prompt pack, not the
mutable harness digest or runtime subagent catalog. Builders cloned for RLM
attempts share the immutable prefix warmup. A regression test mutates the
catalog, proves that `subagent__reviewer` appears in the next Code Mode cell,
and compares the serialized model-visible tool definitions byte-for-byte.
Treatment and evidence identities still include the initial harness digest so
results remain attributable.

Run the matched PR #72 sweep with one or more task roots:

```sh
cargo run -p nanocodex-examples --bin eval-rlm -- \
  tasks/write-greeting tasks/another-task
```

`NANOCODEX_RLM_HARNESS` selects the mutable harness document. Copy a seed
before a sweep so refinements remain eval artifacts rather than dirtying a
checked-in example. The ARC-AGI-3 smoke seed records two trajectory-backed
refinements from the public smoke tasks:

```sh
cp examples/rlm/arc-agi-3-smoke.harness.toml /tmp/nanocodex-arc.harness.toml
NANOCODEX_RLM_HARNESS=/tmp/nanocodex-arc.harness.toml \
  cargo run -p nanocodex-examples --bin eval-rlm -- \
  /path/to/tn36-task /path/to/sb26-task
```

`NANOCODEX_RLM_PROMPTS` and `NANOCODEX_RLM_HARNESS` select alternate launch
inputs; `NANOCODEX_EVAL_TRIALS` changes the default three trials.

### Official live ARC-AGI-3 controller

`eval-arc-agi-3-rlm` is a thin application-owned controller for the public
ARC-AGI-3 API. It keeps one retained Nanocodex conversation across frames,
submits exactly one executable action per model turn, samples at most seven
animation frames with the official interpolation rule, applies the official
per-level `5 * baseline_actions` budgets, and closes the scorecard before any
optional training refinement. The root is the only environment actor;
recursive children receive read-only frame summaries and cannot call the game
API.

Run matched arms with the checked-in frozen harness:

```sh
cargo run -p nanocodex-examples --bin eval-arc-agi-3-rlm -- \
  --game vc33 --mode baseline --thinking low
cargo run -p nanocodex-examples --bin eval-arc-agi-3-rlm -- \
  --game vc33 --mode rlm --thinking low \
  --harness examples/rlm/arc-agi-3.harness.toml
```

`--max-actions N` is a development cap and is recorded as such; omit it for an
official-profile run. Each output directory retains incremental `run.json`,
the closed scorecard, exact root `root-events.jsonl`, harness start/final
copies, recursive `rlm-evidence.json`, root and child usage, cache-read tokens,
decision latency, and locally estimated total cost. Use `--allow-refinement`
only with a copied training harness. Held-out runs default to read-only harness
state.

Run the same runtime through the full native CLI with bundled immutable prompts
and a mutable harness file:

```sh
cargo run -p nanocodex-bin --bin nanocodex -- run \
  "delegate independent investigations, then synthesize them" \
  --rlm-harness crates/experimental/nanocodex-rlm/nanocodex.harness.toml
```

`--rlm-prompts <directory>` overrides the bundled prompt pack. The CLI keeps
the RLM runtime alive for the root session, finalizes its evidence, and closes
all retained children during normal shutdown.

## Deliberate first-slice limits

- The runtime is process-local. It does not add a daemon or app-server protocol.
- Harness snapshots and rollback archives are durable; live children are not restored
  after process death.
- Subagents share the root's workspace and application authority. They are a
  context and coordination boundary, not a security boundary.
- The crate remains experimental until matched evaluation demonstrates a
  concrete benefit.
