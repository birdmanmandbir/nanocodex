# Nanocodex plan

## Objective

Build high-quality reusable Rust building blocks for frontier OpenAI agents.
Nanocodex makes a small number of deliberate choices about models, transport,
ownership, tools, performance, and observability. The embeddable APIs are the
product; the CLI, Ratatui application, hosted WASM deployments, managed-agent
service, and evaluation harnesses are concrete consumers that prove those APIs.

Every stable crate must remain useful independently, documented from its own
README, tested through its public paths, benchmarked at the boundaries it can
affect, and observable without adopting the Nanocodex CLI. Experimental crates
may move faster, but they must preserve the same ownership discipline and may
not leak application policy into the stable graph.

## Current baseline

The PR #50 refactor and the `0.3.0` release are complete. The repository has
since moved beyond that delivery boundary:

- the stable crates own the OpenAI, tool, agent, observability, and facade
  boundaries described in the workspace instructions;
- retained VM-backed workspace tools, composable egress, and host-side secret
  routing are available as experimental application infrastructure;
- deterministic browser automation, a dedicated headed browser VM, and the
  normal CLI browser provider are implemented;
- reusable GPT Realtime voice sessions and the Ratatui voice consumer are
  implemented with current Codex transport behavior;
- native, Python, Node, browser WASM, Cloudflare Durable Object, and Rivet Actor
  consumers exercise the same owned session API; and
- nightly and stable release automation, differential workloads, retained
  traces, and performance gates exist and must continue to protect the public
  contracts.

Do not reopen completed refactor work unless a regression, current consumer, or
measured boundary demonstrates a concrete problem.

## Delivery model

There is no longer one repository-wide pull request boundary. Work advances as
small, independently mergeable vertical slices. Each slice must have a real
consumer, preserve unrelated behavior, and leave `master` releasable.

For each slice:

1. establish the public or application-owned contract and its owner;
2. implement the complete lifecycle, including cancellation and cleanup;
3. add focused deterministic regressions and representative measurements;
4. validate affected native, WASM, language-binding, example, and crate-boundary
   consumers; and
5. merge only with required checks green and no known migration ambiguity.

Codex remains behavioral evidence rather than an API specification. Before a
new parity claim, reconcile the authoritative checkpoint in the workspace
instructions with the classifications already recorded in
[`docs/CODEX_PARITY.md`](docs/CODEX_PARITY.md), update the local Codex checkout,
and classify every intervening commit as port/evaluate/defer/out-of-scope. Do
not let an unreviewed upstream change silently redefine Nanocodex behavior.

## Current phase

The foundational refactor is no longer the roadmap. Nanocodex is entering the
product-buildout and measured-optimization phase, organized as three parts.

### Part 1: build and prove the agent foundation — substantially complete

- The layered SDK, owned lifecycle, Code Mode parity work, retained VM tools,
  browser and voice foundations, hosted transports, and the `0.3.0` release are
  complete.
- [PR #95](https://github.com/gakonst/nanocodex/pull/95) and the other extracted
  runtime prerequisites are merged. Refreshing the Codex parity ledger remains
  ongoing maintenance, not a prerequisite for starting product work.
- [PR #61](https://github.com/gakonst/nanocodex/pull/61) is merged. Its
  `nanocodex eval` command and `nanocodex-eval` library are now the authoritative
  way to measure Nanocodex against stock Codex.
- Part 1 closes for `0.4.0` when a frozen release candidate has a complete,
  retained, reproducible differential evidence set suitable for the website.

### Part 2: make the intelligence useful everywhere — active

- Build product surfaces as real consumers of the owned session API. Current
  work includes application-owned terminals in
  [PR #79](https://github.com/gakonst/nanocodex/pull/79), owned interactive
  computer use in [PR #102](https://github.com/gakonst/nanocodex/pull/102), and
  voice/meeting workflows in
  [PR #116](https://github.com/gakonst/nanocodex/pull/116).
- Make the same intelligence deployable wherever users and applications need
  it. The Vercel Workflow and Cloudflare Durable Object consumers in PRs
  [#112](https://github.com/gakonst/nanocodex/pull/112) and
  [#113](https://github.com/gakonst/nanocodex/pull/113) are merged; durable
  platform sandboxes in [PR #114](https://github.com/gakonst/nanocodex/pull/114),
  managed agents in [PR #89](https://github.com/gakonst/nanocodex/pull/89), and
  the retained exe.dev experiment in
  [PR #119](https://github.com/gakonst/nanocodex/pull/119) continue that work.
- Keep these surfaces thin and application-owned. Product-specific deployment,
  UI, tenancy, payment, authentication, and orchestration policy must not widen
  the stable SDK into an app server or platform abstraction.

### Part 3: push the frontier with measured ideas — beginning

- Use the Part 1 baseline to identify concrete performance and capability gaps,
  then optimize against retained differential evidence instead of intuition.
- Expand evaluation beyond Terminal-Bench through
  [PR #72](https://github.com/gakonst/nanocodex/pull/72). StableBench is another
  suite consumed by `nanocodex eval`, not a second evaluation system.
- Exercise ideas such as application-owned subagent orchestration and the RLM
  experiment in [PR #32](https://github.com/gakonst/nanocodex/pull/32) against
  the same immutable baselines. Promote only demonstrated improvements, and do
  not turn a successful experiment into a generic core scheduler by default.

Parts 2 and 3 may advance concurrently. The eval baseline is the anchor that
lets both move quickly without losing correctness or comparability.

## Evaluation contract

`nanocodex eval` is the one operator entry point for release evaluation,
iteration, and differential analysis. Do not run a parallel direct-Harbor
campaign or maintain a second campaign format. Benchmark-owned tasks, schemas,
images, and verifiers may be consumed unchanged behind the Nanocodex evaluator;
the orchestration, durability, comparison, and retained evidence remain owned
by `nanocodex-eval`.

Every public performance claim must come from an immutable matched plan:

- pin the Nanocodex commit and binary digest, stock Codex version and digest,
  model, reasoning effort, tool exposure, task corpus, task-image digest,
  verifier, repetition count, and evaluator configuration;
- give both arms the same task inputs and resources, retain every attempt, and
  classify infrastructure failures as incomplete evidence rather than scores;
- use the evaluator's prepared-image reuse, disposable overlays, exact resume,
  fast path, and memory-weighted scheduler to saturate the eval host safely;
- inspect canonical verifier output, JSONL, trajectories, retained files, model
  usage, cost, and cold-versus-warm timing before publishing an aggregate; and
- generate website data from the retained records rather than manually copying
  headline numbers.

For the `0.4.0` campaign, `ubuntu@dev-georgios` is the primary x86_64 host and
all run roots, images, caches, logs, and reports belong under the dedicated
3.5 TB `/mnt/nanocodex-evals` volume. Root-disk space is not evaluation
capacity. Start with a one-task release-binary smoke, then raise concurrency
aggressively until CPU, memory, or measured throughput establishes the host's
real limit. Resume the same immutable plan after interruption; never discard
completed coordinates just to restart with different scheduling.

Terminal-Bench establishes the broad matched coding-agent baseline. PR #72
expands the same retained comparison model to StableBench and establishes the
pattern for further suites. Deterministic task correctness remains
authoritative. Optional model scoring may add evidence, but may not overwrite
verifier rewards or convert scorer failure into a false correctness failure.

Never modify benchmark tasks, verifiers, images, expected outputs, or scorer
inputs to make Nanocodex pass.

## `0.4.0` evidence and release gate

- Repair and smoke the current nightly `nanocodex eval` VM path before spending
  model budget; a tool-console or guest-launch failure is an evaluator
  regression, not benchmark evidence.
- Freeze the candidate commit and run the full matched Terminal-Bench campaign
  with enough repetitions to report totals, paired outcomes, per-task results,
  uncertainty, usage, cost, and timing against pinned stock Codex.
- Rebase PR #72 onto the merged eval foundation and complete the selected
  StableBench differential campaign with canonical correctness and complete
  scorer provenance.
- Publish only claims directly supported by complete retained comparisons. “As
  good as Codex” requires the matched aggregate and important slices to support
  that statement; isolated wins and incomplete comparisons are shown as such.
- Project the immutable release evidence into the website, retain a machine-
  readable artifact index, and link every displayed aggregate back to its
  candidate, configuration, and run records.
- Run crate-boundary checks, rustfmt, warnings-denied Clippy, workspace and
  all-target tests, rustdoc/doctests/examples, WASM, Node/browser, PyO3,
  CLI/Ratatui, static VM guest, and focused browser/VM trials on the same frozen
  candidate before publishing `0.4.0`.
- Update root and per-crate changelogs, public READMEs, migration notes, and
  release examples from the actual merged graph. Publish only from a clean,
  reviewed commit with required checks green.

## Current execution order

1. [x] Ship `0.3.0` and establish the layered, library-first SDK.
2. [x] Merge Code Mode/runtime parity prerequisites and the VM-backed
   differential foundation in PR #61.
3. [ ] Fix the latest-nightly evaluator launch regression and prove one matched
   task end to end with `nanocodex eval` on the dedicated eval volume.
4. [ ] Freeze the `0.4.0` candidate and complete the aggressively parallel,
   resumable Terminal-Bench differential campaign against pinned stock Codex.
5. [ ] Rebase and finish PR #72, then retain the selected StableBench evidence
   through the same evaluator and artifact model.
6. [ ] Generate the website evidence projection, review every public claim, and
   cut `0.4.0` from the evaluated candidate.
7. [ ] Advance PRs #79, #102, and #116 as focused terminal, computer-use, and
   voice product consumers, measuring affected boundaries as they land.
8. [ ] Advance PRs #89, #114, and #119 as application-owned ways to make
   Nanocodex available across managed, edge, and hosted environments.
9. [ ] Re-evaluate PR #32 and subsequent orchestration ideas against the frozen
   baseline; keep only improvements demonstrated by retained differentials.

## Current non-goals

- No provider abstraction, broad model portability layer, generic agent app
  server, compatibility framework, approval subsystem, or stable multi-agent
  scheduler in the core SDK.
- No caller-visible socket tasks, mutable run state, replay bookkeeping,
  browser tab leases, VM internals, or transport response identifiers.
- No model-selected browser placement, ambient takeover of arbitrary personal
  tabs, default-profile remote-debugging escape hatch, or silent fallback from
  an isolated browser to the user's authenticated browser.
- No browser audio-device ownership or generic realtime protocol in the core
  library.
- No provider/payment behavior in public `nanocodex-*` libraries and no generic
  secret manager in the tool runtime.
- No cosmetic CLI/TUI lifecycle rewrite without a demonstrated correctness or
  measured performance reason.
- No benchmark, task, verifier, or retained-artifact modification made solely
  to improve an evaluation result.
