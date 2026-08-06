# nanocodex-rlm

`nanocodex-rlm` is an unpublished, application-owned recursive language-model
runtime built only from Nanocodex's public agent and tool APIs. It gives Code
Mode a stable set of subagent operations while leaving conversation,
transport, and tool-runtime ownership in each spawned `Nanocodex` driver.

Prime Agent supplies the semantic model: orchestration is programmatic,
children start asynchronously, clean children may recurse, and supplemental
harness state describes reusable subagents. Tact supplies evidence that this
belongs in a consumer using `tools_factory`, `AgentHandle::spawn`, typed events,
and explicit cleanup rather than in the stable Nanocodex agent crates. Neither
project's exact model-facing API is copied.

## Contract

The Responses API still exposes Nanocodex's single `exec` Code Mode tool. The
stable logical operations are:

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

The first native adapter exposes these as `tools.spawn_agent`,
`tools.list_agents`, `tools.send_agent_message`, `tools.wait_agent`,
`tools.interrupt_agent`, and `tools.close_agent` inside the existing persistent
Code Mode cell. Continual operations are available as `tools.harness_state`,
`tools.harness_apply`, `tools.harness_rollback`, and `tools.refine_harness`.
Operation names, schemas, ordering, bounds, authorization, and lifecycle
behavior are code-owned and immutable. Human-facing descriptions and examples
are loaded at launch from a prompt directory.

## Prompt and harness inputs

`PromptPack::load` reads this immutable launch bundle:

```text
prompts/
├── orchestration.md   root-facing programming guidance
├── subagent.md        guidance injected into every clean child
├── refiner.md         immutable background-refinement policy
└── tools.toml         model-facing descriptions for stable operations
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
id = "runtime-investigator"
name = "Runtime investigator"
description = "Traces ownership, cancellation, and cleanup."
instructions = "Cite concrete files and report unresolved ownership questions."
enabled = true
```

All inputs are validated and content-addressed before an agent starts. The
`LaunchSnapshot` records the immutable prompt/tool prefix and initial harness
identity. During a run, `HarnessStore` serializes edits, increments the revision,
atomically replaces the TOML file, and archives the previous document for
rollback. Root and retained children share the latest revision.

An accepted revision never rewrites Nanocodex's system prompt or tool
definitions. Instead, its complete supplemental snapshot is appended as a
developer-context delta through `AgentHandle::append_developer_message`. When a
turn is active, Nanocodex queues that delta and commits it after the turn, before
the next prompt. The Responses request prefix, tool order, stable prompt-cache
key, and all preceding conversation bytes therefore remain unchanged. New clean
children receive the latest selected specification before their first prompt.

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
mutable harness digest. Builders cloned for RLM attempts share the immutable
prefix warmup. Treatment and evidence identities still include the initial
harness digest so results remain attributable.

Run the matched PR #72 sweep with:

```sh
cargo run -p nanocodex-examples --bin eval-rlm -- tasks/write-greeting
```

`NANOCODEX_RLM_PROMPTS` and `NANOCODEX_RLM_HARNESS` select alternate launch
inputs; `NANOCODEX_EVAL_TRIALS` changes the default three trials.

## Deliberate first-slice limits

- The runtime is process-local. It does not add a daemon or app-server protocol.
- Harness snapshots and rollback archives are durable; live children are not restored
  after process death.
- Subagents share the root's workspace and application authority. They are a
  context and coordination boundary, not a security boundary.
- The crate remains experimental until matched evaluation demonstrates a
  concrete benefit.
