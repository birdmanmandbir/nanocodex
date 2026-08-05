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

The first native adapter exposes these as `tools.spawn_agent`,
`tools.list_agents`, `tools.send_agent_message`, `tools.wait_agent`,
`tools.interrupt_agent`, and `tools.close_agent` inside the existing persistent
Code Mode cell. Operation names, input schemas, bounds, authorization, and
lifecycle behavior are code-owned. Human-facing descriptions and examples are
loaded at launch from a prompt directory.

## Prompt and harness inputs

`PromptPack::load` reads this immutable launch bundle:

```text
prompts/
├── orchestration.md   root-facing programming guidance
├── subagent.md        guidance injected into every clean child
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

[[subagents]]
id = "runtime-investigator"
name = "Runtime investigator"
description = "Traces ownership, cancellation, and cleanup."
instructions = "Cite concrete files and report unresolved ownership questions."
enabled = true
```

All inputs are validated and content-addressed before an agent starts. A
normal run receives one frozen `LaunchSnapshot`; changing either file creates a
different treatment identity. Refinement produces a new harness revision
outside benchmark attempts; it never rewrites the immutable base Nanocodex
system prompt or changes an in-flight run.

## Evaluation

PR #72's ordinary `Sweep` is the first consumer. A baseline and RLM recipe use
the same task, trial, model, thinking, workspace, VM, verifier, and executable.
The RLM arm additionally writes `agent/rlm-evidence.json` before verification,
retaining the launch digest, recursive agent tree, linked child events, child
terminal results, aggregate usage, and cleanup status. PR #72's stock
`AgentResult` still reports root usage only, so cost comparisons must add the
evidence file's child usage; merging recursive accounting into the stock
aggregate is intentionally left explicit rather than silently undercounted.

Calibration may create a new harness revision. Held-out evaluation always
freezes that revision before constructing a sweep; benchmark trials never
mutate shared harness state.

Run the matched PR #72 sweep with:

```sh
cargo run -p nanocodex-examples --bin eval-rlm -- tasks/write-greeting
```

`NANOCODEX_RLM_PROMPTS` and `NANOCODEX_RLM_HARNESS` select alternate launch
inputs; `NANOCODEX_EVAL_TRIALS` changes the default three trials.

## Deliberate first-slice limits

- The runtime is process-local. It does not add a daemon or app-server protocol.
- Prompt and harness snapshots are durable; live children are not restored
  after process death.
- Subagents share the root's workspace and application authority. They are a
  context and coordination boundary, not a security boundary.
- The crate remains experimental until matched evaluation demonstrates a
  concrete benefit.
