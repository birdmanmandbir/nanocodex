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

## Active milestones

### 1. Runtime and Code Mode parity

- Land the focused Code Mode contract alignment from
  [PR #95](https://github.com/gakonst/nanocodex/pull/95) independently of the
  evaluation stack it came from.
- Preserve one model-facing Code Mode tool, deferred runtime discovery, exact
  tool ordering, caller-owned exposure policy, and the current provider-native
  Responses namespace boundary.
- Refresh the Codex parity ledger through a deliberately selected current
  checkpoint. Port only relevant invariants with direct regression evidence;
  keep app-server, approval, provider-portability, and unrelated TUI behavior
  classified out of scope.

### 2. Browser identity, placement, and visibility

- Finish the private-browser desktop profile import in
  [PR #93](https://github.com/gakonst/nanocodex/pull/93). Cookie extraction must
  remain harness-owned, allowlisted, typed, non-mutating, and absent from the
  model-callable schema.
- Replace the single CLI browser boolean with deliberate caller-owned session
  policy while preserving a migration path for bare `--browser`:
  - `private-host`: a Nanocodex-owned host browser;
  - `private-vm`: the existing isolated browser VM; and
  - `user-chrome`: an explicitly enabled bridge to the user's running Chrome.
- Keep presentation independent from placement. `background` and `visible` are
  presentation choices; Chromium running under Xvfb is headed but is not
  visible until a bounded display or screenshot-stream consumer exists.
- Keep tab acquisition explicit and fail closed:
  - private modes create owned tabs;
  - user-Chrome mode creates a tab in a named Nanocodex group by default;
  - taking over an existing tab requires an explicit user selection or exact
    tab mention matched against freshly listed ID, title, URL, browser instance,
    recency, and group metadata; and
  - claimed user tabs remain in their existing group and are released rather
    than closed at cleanup.
- Implement user-Chrome control through a Nanocodex Chrome extension and a
  versioned native-messaging host. Do not attach ordinary remote CDP to a
  personal default profile and do not depend on the proprietary ChatGPT Chrome
  extension or its protocol.
- Lease every controlled tab to one browser session, surface user/debugger
  interruption as cancellation, clean up agent-created tabs deterministically,
  and keep open-tab inventory out of model context unless the caller explicitly
  provides the selected tab.
- Decide whether `private-vm` means the dedicated browser VM or a browser
  co-located with the retained workspace VM before promising guest-localhost
  testing. If the processes remain in different VMs, expose an explicit,
  bounded forwarding path rather than relying on host-network accidents.
- Preserve `BrowserTool` as an ordinary caller-owned provider. Any extension
  backend must either support the declared action contract or expose an honest
  capability-specific contract; unsupported actions must not be discovered as
  usable.

### 3. Application-owned terminals and managed sessions

- Rebase and finish the narrow application-owned terminal contract in
  [PR #79](https://github.com/gakonst/nanocodex/pull/79). Retained process state,
  cancellation, output bounds, and descendant cleanup remain owned by the
  tool runtime rather than the agent driver.
- Review and land [PR #89](https://github.com/gakonst/nanocodex/pull/89) only as
  a concrete experimental managed-session consumer: durable actors may own
  Nanocodex sessions, idempotency, event replay, policy, disposable VMs, and
  service projection, but the stable agent crates must not become an app server
  or generic scheduler.
- Keep payment, tenant, authentication, deployment, and secret-routing policy
  in the consuming application. Lower `nanocodex-*` libraries own typed seams,
  not one hosted product's policy.

### 4. Evaluation as product evidence

- Treat the merged VM-backed differential foundation from
  [PR #61](https://github.com/gakonst/nanocodex/pull/61) as the one execution
  substrate. Do not revive a separate Nanoeval runner.
- Continue [PR #72](https://github.com/gakonst/nanocodex/pull/72) directly on
  that merged contract, then validate StableBench with exact retained
  artifacts, canonical verifier output, scorer provenance, model usage, and
  cost.
- Build the next evaluation UX slice on PR #72 rather than adding another
  benchmark path. PR #72's typed dataset adapters, content-addressed
  `ImportStore`, immutable normalized tasks, VM image preparation, canonical
  verifier boundary, typed events, durable coordinate checkpoints, drain, and
  exact resume are the lower-level contracts. The normal CLI should compose
  them behind one manifest-driven `prepare`/`run` lifecycle; the existing
  detailed import and inspection commands may remain available for debugging.
- Keep deterministic task correctness authoritative. Optional model scoring may
  add evidence but must not overwrite verifier rewards or turn scorer failure
  into a false correctness failure.
- Re-evaluate the older recursive task-tooling experiment in
  [PR #32](https://github.com/gakonst/nanocodex/pull/32) only after current
  differential evidence identifies a concrete need. Do not merge a generic
  recursive scheduler because the branch exists.
- Never modify benchmark tasks, verifiers, images, or expected outputs to make
  Nanocodex pass. Inspect exact JSONL, trajectories, retained files, verifier
  logs, and cold-versus-warm timing before making an evaluation claim.

#### Prepared evaluation profiles

The user-facing configuration should stay deliberately small. Nanocodex from
the currently running executable is always the primary treatment and is not
selected as a candidate in configuration. Top-level host and harness tables
define reusable inventory, supported upstream benchmarks come from a built-in
catalog, and optional benchmark tables are needed only for custom sources. The
selected profile chooses the host pool, additional harnesses, and task set.
With no additional harness selected the evaluator runs only Nanocodex; with one
or more selected harnesses the same `run` command automatically runs the
complete matched differential or multi-harness matrix. There is no separate
normal `diff` workflow.

The repository-level `nanocodex.toml` should initially support this shape:

```toml
default = "release"

[hosts.dev-georgios]
ssh = "ubuntu@dev-georgios"
dir = "/mnt/nanocodex-evals"

[harness.codex]
command = "/path/to/codex"

[harness.codex.variant.code-mode-only]
args = [
  "--config", "features.code_mode=true",
  "--config", "features.code_mode_only=true",
]

[harness.codex.variant.code-mode]
args = [
  "--config", "features.code_mode=true",
  "--config", "features.code_mode_only=false",
]

# Optional: compare the current in-process Nanocodex with an older CLI build.
[harness.nanocodex]
command = "/opt/nanocodex/0.3.0/nanocodex"

[profiles.release]
hosts = ["dev-georgios"]
harnesses = ["codex.code-mode-only", "codex.code-mode", "nanocodex"]
tasks = ["terminal-bench-2.1"]
trials = 10
model = ["gpt-5.6-sol"]
thinking = ["high"]
```

The fields have narrow meanings:

- The top-level `default` chooses the profile used when the command omits its
  optional profile positional.
- `[hosts.<alias>]` contains only SSH reachability and an optional retained
  state directory. State defaults to `~/.nanocodex/evals` on each machine. The
  explicit `dev-georgios` directory keeps all large state on its dedicated
  3.5 TB drive; preparation must bind the receipt to the mounted filesystem so
  a missing mount cannot redirect work onto the root disk.
- Each `[harness.<name>]` table defines one reusable additional executable
  harness. The table key selects its known CLI driver and is the stable value
  referenced by a profile; `command` is the executable staged into guest
  attempts. The driver owns invocation, lifecycle capture, cancellation,
  trajectory normalization, usage, and version discovery. Names such as
  `codex`, `nanocodex`, and future `claude-code` drivers all enter the same
  evaluator treatment model without introducing provider portability into the
  Nanocodex SDK. Preparation derives the authoritative result identity from
  the driver name, reported version, configuration, and executable digest.
- A harness with no variants contributes one default treatment. Optional
  `[harness.<name>.variant.<variant>]` tables contribute named treatments whose
  `args` are appended by the driver as an argv vector without a shell. Base
  harness `args` apply to every variant. Profiles may select the whole harness
  name or exact `<harness>.<variant>` treatments. This general mechanism covers
  Codex feature configuration, Claude Code model flags, and other CLI
  experiments without adding evaluator fields for every harness option; the
  complete argv is validated during preparation and bound into retained
  identity.
- Stable built-in benchmark names such as `terminal-bench-2.1`,
  `arena-hard-v2`, `openai-evals`, `gpqa-diamond`, and `swe-bench` need no
  configuration. Each installed Nanocodex version owns a reviewed catalog of
  pinned acquisition and adapter recipes for the upstream benchmarks it
  supports. Preparation fetches or reuses the exact source, invokes the PR #72
  importer, and records source and normalized-task digests; it never resolves
  a floating `latest` version.
- `[benchmark.<name>]` is reserved for a private dataset, a local fork, or an
  upstream format not in that catalog. It selects one PR #72 adapter and
  supplies only that adapter's pinned source inputs. Custom names may not
  shadow built-ins. Unknown fields and unsupported source semantics fail during
  preparation. Source acquisition remains orchestration around the adapter;
  the adapter itself continues to normalize and snapshot consumed inputs
  without owning network or credential policy.
- `[profiles.<name>]` is one complete runnable preset. It selects the host pool,
  additional harnesses, task selectors, valid trial target, and model-facing
  sweep dimensions. `model` and `thinking` are arrays whose Cartesian product
  defines the model sweep, even when either contains only one value. One
  `tasks` entry may select a complete benchmark by its name or one normalized
  case as `<benchmark>/<task>`; both forms may be mixed.
  `web_search` defaults to `false` and is omitted unless the profile explicitly
  enables a web-enabled treatment.
  Omitting `hosts` means local execution; omitting `harnesses` means current
  Nanocodex only. It contains no concurrency, memory, VM count,
  preparation-worker, host-weight, shard-placement, or retry-tuning knobs.

Harness-specific model or effort settings may be expressed by named harness
variants when a cross-model comparison such as Claude Code cannot share the
profile's OpenAI settings. Reports must label the complete harness, version,
variant, effective argv, model, and digest bundle rather than imply that a
cross-model result isolates harness quality.

#### Benchmark adapters and harness CLI drivers

Keep the two integration boundaries separate and cross-productable:

| Boundary | Runs during | Input | Output |
| --- | --- | --- | --- |
| Benchmark adapter | `prepare` | One pinned third-party dataset or official harness bundle | PR #72 `DatasetPlan`, then one content-addressed `ImportedDataset` of ordinary `Task` values |
| Harness CLI driver | `prepare` and each selected `run` arm | One pinned guest CLI, named variant, and exact argv | One prepared treatment and canonical attempt evidence |

The built-in benchmark catalog and custom benchmark configuration both resolve
through PR #72's existing concrete importers:

- `harbor`: Harbor-family task packages, including Terminal-Bench,
  Frontier-Bench, and StableBench;
- `arena-hard`: Arena questions plus the caller-supplied official judge
  package;
- `openai-evals`: supported declarative Match/Includes definitions plus the
  snapshotted deterministic grader;
- `openai-simple-evals`: BrowseComp, GPQA Diamond, HealthBench, and HealthBench
  Professional with the pinned official source used by PR #72;
- `swe-bench`: official instances, instance-image identity, and verifier
  package; and
- `external`: a benchmark-owned hermetic manifest for PaperBench, MLE-style,
  private, or otherwise executable benchmark semantics.

The release coverage target is every benchmark family recorded from OpenAI's
GPT-5.6 reports, not only the suites that happen to have native import formats:

- Terminal-Bench 2.1;
- BrowseComp, HealthBench, HealthBench Professional, and GPQA Diamond;
- SWE-Bench Pro;
- Agents' Last Exam, GDPval-AA, Artificial Analysis, and FrontierMath;
- OSWorld and BenchCAD;
- CTF, SEC-Bench, ExploitBench, and ExploitGym;
- KernelBench/KernelGen, NanoGPT, and PostTrainBench; and
- MMMU Pro, Toolathlon, MRCR, GraphWalks, and ARC-AGI.

Milestone one proves every importer route with one real local task. Milestone
two adds a stable built-in catalog entry and retained official-verifier evidence
for every public or obtainable suite above. Gated or unpublished suites remain
explicitly unavailable until authoritative task material and grading semantics
can be acquired; an `external` manifest is a transport for an official harness,
not permission to invent a substitute benchmark.

Each built-in catalog entry owns its stable user-facing name, authoritative
source location, pinned revision, adapter choice, official verifier assets,
default task environment, and any acquisition or credential requirements. The
orchestrator resolves and caches that recipe under the evaluator state
directory, then passes the local pinned inputs to the adapter; importers remain
deterministic and network-free. Catalog entries are versioned with Nanocodex,
and a gated dataset fails with an actionable preparation error instead of
requiring users to restate adapter plumbing in TOML.

For custom sources, `[benchmark.<name>].adapter` must deserialize through the
same closed, typed adapter configuration with unknown fields denied, construct
the corresponding PR #72 `DatasetImporter`, and pass only its `DatasetPlan` to
`ImportStore`. Once imported, no scheduler, harness executor, VM attempt,
resume, or report path may branch on the original benchmark format. PR #72's
`Harness` and `ExternalHarness` names refer to benchmark-owned verifier
packages; user-facing documentation should call those benchmark verifiers to
avoid confusing them with agent harnesses or their CLI drivers.

The initial harness-driver registry should contain concrete `codex` and
external `nanocodex` CLI drivers. `claude-code` is added only with a real
smoke-tested implementation. A harness table may use an alias and select its
driver explicitly when comparing more than one build:

```toml
[harness.codex-previous]
driver = "codex"
command = "/opt/codex/previous/codex"
```

When `driver` is omitted it defaults to the harness table key. Do not expose a
generic arbitrary-command claim until a versioned wrapper protocol exists;
unknown driver names fail during preparation.

Every harness CLI driver owns the complete guest process boundary:

1. Resolve, validate, version, hash, and stage the guest-architecture
   executable and any read-only runtime assets.
2. Combine driver-owned mandatory protocol arguments, base `args`, and named
   variant `args` into one exact argv template. No shell parses it, and the
   prompt remains a separately owned value inserted at the driver's defined
   boundary.
3. Validate requested variants and capabilities, including final-message
   capture, workspace behavior, network policy, model identity, usage/cost
   reporting, and native trajectory availability.
4. Stage only explicitly selected credentials, CA material, and inherited
   environment names. Secret values never enter the manifest or report; a
   domain-separated digest binds the effective secret-bearing configuration to
   preparation identity.
5. Launch the process in the fresh task guest, from the normalized workspace,
   under a process group with bounded stdout/stderr, timeout, cancellation, and
   descendant cleanup.
6. Retain the complete native output streams and driver diagnostics, then
   project available lifecycle, tool, final-message, usage, cost, and model
   evidence into the common attempt report without inventing unsupported data.

Generic argv does not make harness semantics generic. For example, Codex gives
its remote `/models` tool selector precedence over local feature flags. The
Codex driver must preserve the existing evaluator capture-proxy/catalog
override implied by the configured args and reject preparation unless a smoke
probe observes the requested model-visible tool contract. Other drivers own
equivalent sidecars, environment translation, and semantic validation behind
the same configuration surface.

Current Nanocodex is not forced through this guest CLI contract. It remains the
implicit host-native treatment that drives the same task VM through canonical
workspace tools. The scheduler should own a small internal prepared-treatment
enum for host-native Nanocodex versus guest CLI harnesses, while both produce
the same task outcome, timing, evidence, and report boundary. This keeps the
eval application extensible without adding a harness/provider abstraction to
the Nanocodex SDK.

#### Local adapter smoke profile

Profiles should normally mention built-in benchmark names directly in `tasks`.
The bare name selects the whole benchmark; `<benchmark>/<task>` selects one
normalized case. Preparation imports only the referenced benchmarks, resolves
selectors against their stable normalized task IDs, and fails on unknown,
ambiguous, or overlapping selectors rather than silently running a coordinate
twice. `nanocodex eval list` lists the built-in catalog, and
`nanocodex eval list <benchmark>` lists the pinned task namespace using the
prepared or cached import so users do not need to inspect import-store
internals.

The local smoke profile needs configuration only for its deliberately custom
`external` adapter fixture. All upstream-supported routes use the built-in
catalog:

```toml
[benchmark.external-smoke]
adapter = "external"
manifest = "eval/fixtures/external-smoke/benchmark.toml"

[profiles.adapter-smoke]
# No hosts: run locally.
harnesses = ["codex.code-mode-only", "nanocodex"]
tasks = [
  "terminal-bench-2.1/fix-git",
  "arena-hard-v2/<question-id>",
  "openai-evals/<eval-id>/<case-id>",
  "gpqa-diamond/<case-id>",
  "swe-bench/<instance-id>",
  "external-smoke/<case-id>",
]
trials = 1
model = ["gpt-5.6-sol"]
thinking = ["low"]
```

The implementation PR must replace the illustrative task placeholders with
stable IDs from its pinned real smoke sources. The smoke matrix is one selected
task from every benchmark adapter by current host-native Nanocodex and one
variant of every implemented guest harness CLI driver, exactly one valid trial
each. As benchmark adapters or CLI drivers are added, this profile and its
completeness test must be updated together. `prepare adapter-smoke` must import,
stage, boot-test, and validate every route; `run adapter-smoke` must execute and
verify the complete local cross-product; `status adapter-smoke --watch` and
`report adapter-smoke` must work while it runs. The smoke gate tests plumbing
and evidence completeness, not benchmark quality, so a legitimate verifier
score of zero may still be an operationally successful smoke result.

#### `prepare` and `run`

The normal commands are:

```sh
nanocodex eval prepare [PROFILE] [--host HOST | --hosts HOSTS]
nanocodex eval run [PROFILE]
nanocodex eval run [PROFILE] --rerun [TASK ...]
nanocodex eval status [PROFILE] --watch
nanocodex eval stop [PROFILE]
nanocodex eval report [PROFILE]
```

The profile positional is optional when top-level `default` is set. Users never
name campaigns or output directories. Preparation generates an immutable
identity from the resolved profile, normalized task digests, current
Nanocodex build, external harness executables and configurations, selected
hosts, guest runtime, verifier inputs, and all model-facing policy.

Delivery is deliberately staged. Milestones one and two run locally. Milestone
three introduces the coordinator/runner protocol against exactly one remote
runner, `ubuntu@dev-georgios`, and saturates that machine while retaining state
on its verified 3.5 TB filesystem. The manifest and protocol remain compatible
with a host list, but cross-host placement, replication, and work stealing are
deferred until retained measurements show that one box is memory-bound rather
than model/API-bound. The CLI must reject multiple hosts clearly until that
later capability is actually implemented.

`prepare` is mandatory and performs all heavyweight setup before model spend:

1. Resolve the complete benchmark, task, treatment, sweep, and trial matrix.
2. Resolve each selected built-in recipe or custom benchmark table, acquire and
   cache its pinned inputs, and invoke PR #72 adapters to normalize it into
   immutable ordinary tasks in the content-addressed import store.
3. Resolve and fingerprint the current host-side Nanocodex build. Stage every
   additional harness CLI into its guest execution path, discover its version,
   and reject unsupported modes or adapter protocols.
4. On the selected execution target, acquire an exclusive evaluator lease,
   initialize its state directory, inventory the otherwise-idle machine,
   prepare all task/verifier/runtime images and disks, validate credentials and
   sidecars, and boot-test every distinct prepared execution shape. Milestones
   one and two target the local machine; milestone three performs the same
   protocol through the `dev-georgios` runner.
5. Compute the resource-vector admission plan from measured CPU, memory, disk,
   VM, verifier, and API behavior. Preparation fails as a whole when any
   selected matrix coordinate has no runnable placement. A later measured need
   for multiple runners extends this step with capacity-weighted placement and
   selective artifact replication; it does not change task or run identity.
6. Persist and sync an immutable preparation receipt on the coordinator and
   each runner. Repeating the same preparation is idempotent and reuses
   content-addressed artifacts.

The current Nanocodex driver remains a normal host process whose tools operate
against the task VM. Additional harnesses, including stock Codex, Claude Code,
or an older Nanocodex executable, run through their CLI drivers inside fresh
guest attempts. Every treatment sees the same normalized task and canonical
verifier contract; paired treatments for one coordinate remain on the same
host.

`run` performs no implicit imports, builds, installs, or image preparation. It
requires a complete matching receipt, then starts or resumes the full matrix:

```text
normalized tasks
  x profile sweep points
  x current Nanocodex and configured harness variants
  x valid trial target
```

The coordinator owns one global work-conserving queue, initially consumed by
the single `dev-georgios` runner. Every coordinate has exactly one active
durable lease, so coordinator or runner recovery cannot duplicate model spend
or publish two results for the same coordinate. Long estimated work starts
early, and completed harness arms release their resources independently. There
are no normal lane or memory knobs: the selected host is assumed to be an idle
evaluator worker, and the runner continually admits all safe eligible work
while retaining the exact reason for any CPU, memory, disk, verifier, API, or
preparation limit. Once memory pressure is demonstrated as the limiter, the
same queue and lease protocol may add runner eligibility, work stealing,
artifact placement, and trial distribution across several hosts; all
treatments for one matched coordinate must still remain on the same host.

#### Durable control, monitoring, and reporting

Running and observing must be separate capabilities over one durable state
machine. `run` starts or resumes the lightweight coordinator and attaches the
live UI; detaching that UI must not terminate admitted remote work.
`status --watch` can attach during preparation or execution from another
terminal and reconstructs its view from retained coordinator events and atomic
coordinate checkpoints rather than process-local stdout. It must remain useful
after client lag, terminal loss, coordinator restart, or host reconnect.

The live view must show at least:

- preparation readiness by host, benchmark, task, verifier, and harness;
- exact completed, active, pending, infrastructure-invalid, and quarantined
  coordinates against the valid target;
- current score, coverage, and paired deltas for every harness treatment;
- active phase for each arm and host-wide CPU, admitted memory, disk, VM,
  verifier, and model/API utilization;
- model-bound versus harness-bound time, pending backlog age versus admission
  wait, dominant failure signatures, saturation limiter, and ETA range; and
- the exact Nanocodex, harness, model, benchmark, guest, and verifier identities
  underlying the displayed numbers.

`stop` drains by default: it closes admission, lets admitted coordinates reach
their atomic boundary, and leaves the profile resumable. An explicit immediate
stop may cancel active attempts and remove only disposable overlays. Re-running
`run` with the same profile resumes the exact prepared matrix and skips every
valid checkpoint. A rerun invocation selects completed task names, failure
classes, or treatments into retained replacement lineage without overwriting
the original evidence. The normal task-selector form remains
Foundry-like (`eval run --rerun mailman query-optimize`) rather than requiring
users to locate retained directories or compose a second manifest.

`report` reads the same durable state and works while execution is active,
clearly marking partial coverage. Once complete it freezes a website-ready
report containing the exact manifest and provenance, aggregate and per-task
scores, pairwise deltas and uncertainty, coverage, usage and cost, cold setup
versus warm agent timing, infrastructure exclusions, failure cohorts, and
links to canonical trajectories, verifier evidence, and logs. No report claim
may hide missing coordinates or mix incompatible preparation identities.

### 5. Next release gate

- Select the next version only after the included milestones and migrations are
  known; do not infer a version number from unreleased workspace metadata.
- Update root and per-crate changelogs, public READMEs, migration notes, and
  release examples from the actual merged graph.
- Run crate-boundary checks, rustfmt, warnings-denied Clippy, workspace and
  all-target tests, rustdoc/doctests/examples, WASM, Node/browser, PyO3,
  CLI/Ratatui, static VM guest, live native smoke, focused browser/VM trials,
  and the relevant differential suites.
- Publish only from a clean, reviewed commit with required checks green; retain
  exact release and evaluation artifacts.

## Current execution order

1. [x] Merge PR #50, ship `0.3.0`, and establish the layered stable SDK.
2. [x] Land retained VM tools, browser/VM automation, Realtime voice, reusable
   hosted transports, composable egress, Cloudflare, and Rivet consumers.
3. [ ] Finish and merge the focused Code Mode parity slice in PR #95.
4. [ ] Reconcile and advance the Codex parity checkpoint with a complete commit
   classification and direct evidence for every adopted behavior.
5. [ ] Fix, validate, and merge desktop profile import in PR #93.
6. [ ] Build browser placement and presentation policy for private host and
   private VM sessions, then prove both through the CLI consumer.
7. [ ] Prototype the user-Chrome extension/native-host path; prove exact tab
   claiming, grouping, visible cursor feedback, interruption, leasing, and
   cleanup before exposing it as normal CLI policy.
8. [ ] Rebase and decide PR #79, then review PR #89 against the stable-core and
   application-policy boundaries above.
9. [ ] Continue the profile-driven evaluation UX directly in PR #72, using its
   normalized benchmark imports as the only evaluation path. The first
   milestone is a local `adapter-smoke` profile that prepares, runs, verifies,
   monitors, and reports one real task through every built-in benchmark adapter
   with current Nanocodex and every implemented guest harness CLI driver.
10. [ ] Extend the same PR #72 workflow from that smoke gate to complete local
    profiles covering the recorded GPT-5.6 benchmark families: automatic
    multi-harness/model/thinking matrices, durable monitoring,
    interruption/resume/rerun, and website-ready reporting behind
    `nanocodex eval prepare` and `nanocodex eval run`.
11. [ ] Run the coordinator/runner architecture with one saturated remote
    runner on `dev-georgios`, retaining all heavyweight state on its verified
    3.5 TB drive. Add cross-host sharding only after measurements show that
    machine memory, rather than model/API capacity, is the material limiter.
12. [ ] Decide whether PR #32 still solves a demonstrated problem or should be
    replaced by a smaller application-owned experiment.
13. [ ] Cut the next release only after all selected milestones pass the full
    release gate.

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
