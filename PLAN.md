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

## Immediate working slice: durable neural evaluation throughput

This is the only active execution track until it reaches its exit gate. The
design has three actors:

```text
controller --launches--> durable one-shot workers --claim/finish--> coordinator
```

SQLite is the task ledger. A worker owns exactly one task. The controller owns
only observation and admission; after launch, the OS owns each worker's
lifetime. There is no worker pool, second scheduler, wave, fixed topology, or
configured worker-count cap.

### Hard requirement: saturate the machine

While claimable work exists, the controller must drive the host to its highest
productive occupancy and keep it there. The bias is deliberately asymmetric:
unused healthy capacity is a controller bug, while a short overload is an
expected, recoverable probe. When the evidence supports multiple reasonable
targets, choose the higher one.

Infrastructure retries and OOM-killed workers are acceptable calibration
signals. They return their tasks to `unclaimed`, remain visible as attempt and
OTEL evidence, and inform the next occupancy decision. They are not benchmark
failures and the controller is not required to avoid them. Repeated OOMs that
produce no useful throughput or adaptation are a controller failure.

Growth is batched and may make large jumps. One-at-a-time admission is not the
default, and the controller never waits for a task batch or wave to finish
before reconsidering capacity. Workers whose launch has not yet settled count
as occupied, so repeated control ticks do not blindly compound the same launch.
As soon as those workers start, claim, exit, or change host pressure, the
controller can make another decision.

Saturation means the highest occupancy that produces useful terminal
throughput. The controller keeps probing upward until added occupancy produces
adverse pressure or stops improving completion rate. High CPU, load, resident
memory, or worker count alone is not a stop signal; using the machine is the
goal. It may stop launching when the host is stalling, swapping, OOM-killing,
slowing task progress, or producing more infrastructure failures. It then lets
existing workers drain naturally and never proactively kills them to move
downward. The conclusion is local to the recent task mix and telemetry, not a
remembered lower/upper worker bound. As soon as the adverse evidence clears or
the task mix changes, the controller probes upward again.

A no-launch cycle is valid only when at least one of these is true:

- the requested occupancy is already represented by live and starting workers;
- a settled upward probe produced kernel pressure, swap activity, OOM/process
  loss, degraded throughput, rising latency, or infrastructure failures; or
- no unreserved `unclaimed` row remains.

The controller computes whether a hold is admissible from those observations;
the model does not get to declare the host unhealthy through prose. A busy but
productive host remains eligible for another upward probe. A throughput hold
requires comparison with a higher, settled occupancy rather than an arbitrary
utilization or free-memory threshold.

"The model chose to hold" is not sufficient. This is a deterministic controller
guard, not prompt advice. If the model call fails, emits an invalid target, or
requests no growth on a healthy host with backlog, the controller retries
promptly and uses a geometric upward fallback: from zero, seed from detected
host parallelism; otherwise, target at least twice the current occupied count.
This is a growth floor, not a worker cap, and the model may request more.
Unsettled launches count as occupied and prevent the fallback from compounding
before the previous probe has materialized.

A saturation run is not proven merely because a worker count stayed constant.
It must show either a real resource bottleneck or no throughput gain from an
upward probe. Every cycle records enough evidence to explain why it launched,
held, or recovered.

### Coordinator: idempotent task truth

A task is `unclaimed`, `running`, `success`, or `failed`. `Failed` means the
task ran to completion and the verifier rejected it. Infrastructure failure and
process interruption are attempt outcomes; both return the task to
`unclaimed`.

- `claim(worker_id)` returns that worker's existing claim or atomically assigns
  one row. One worker can never own two active claims.
- `finish(claim_id, result)` accepts repeated identical publication and rejects
  a conflicting result.
- `worker_exited(worker_id)` idempotently releases zero or one unfinished claim
  and records one interrupted attempt.

### Worker: one durable task

Each worker runs from an immutable `releases/<revision>/nanocodex`, claims one
task, executes it once, durably saves the outcome, publishes it, and exits:

```text
workers/<worker-id>/process.json
workers/<worker-id>/claim.json
workers/<worker-id>/result.json
workers/<worker-id>/artifacts/
```

The launcher creates the worker ID and directory before spawn and uses that ID
in an independent OS/systemd unit. The worker unit is not in the controller's
process group or service cgroup: stopping, restarting, or OOM-killing the
controller cannot signal the worker. The worker unit does own that worker's VM,
proxy, and other descendants so the OS removes the complete task process tree
when the worker exits or is killed.

The worker finalizes artifacts before atomically writing `result.json`. It
retries coordinator requests without changing their identities. If it dies
before `result.json`, reconciliation returns its task to `unclaimed`. If it
dies after `result.json`, reconciliation publishes that saved result. Cleanup
happens only after either path is acknowledged.

Controller death does not affect workers. Coordinator death does not release
claims. A restarted controller first reconciles worker directories and OS
process identity, then makes another admission decision. A per-node lock allows
only one controller to reconcile and launch, but never owns worker lifetimes.

### Controller cycle

The controller reevaluates on worker lifecycle changes and on a short telemetry
timer. It does not wait for task completion. Every cycle:

1. Reconcile saved results and confirmed worker exits.
2. Observe live and starting workers, backlog, terminal throughput, claim/start
   latency, infrastructure/interrupted attempts, memory, swap, load,
   CPU/memory pressure, and current/remaining task resource declarations.
3. Ask the model, under a bounded decision deadline, only for an absolute
   `desired_workers` count and a reason.
4. Validate the decision against the saturation invariant.
5. Let `occupied` be live plus unsettled workers. Reserve one currently
   unclaimed row for each occupied worker that has not claimed yet. Launch
   `min(desired_workers - occupied, unclaimed - reservations)`, with both
   differences floored at zero, then continue observing.

The model has no shell, PID, HTTP, or process-killing tools. Its objective is
terminal completions per unit time, not worker count. Verifier pass and verifier
failure both count as useful completions; infrastructure and interrupted
attempts count against the controller.

### Observability

SQLite stores only task, claim, attempt, and result truth. OTEL records active
and starting workers, backlog, completions, throughput, launches, exits,
infrastructure failures, memory, swap, load, and pressure by `node.id`. Each
decision records its observations, desired occupancy, actual launches, and
reason. OTEL failure never blocks evaluation.

### Build order

1. [ ] Make coordinator claim, finish, and worker-exit operations idempotent.
2. [ ] Make one worker durable across lost responses, coordinator restart, and
   death before or after `result.json`.
3. [ ] Add controller reconciliation and independent OS-unit multi-worker
   launch.
4. [ ] Add the neural desired-occupancy decision, saturation guard, and OTEL.
5. [ ] Install immutable releases and prove the complete system on the real
   benchmark host.

Do not begin saturation tuning until one worker's durability cases pass.
Thereafter, every orchestration change must be run on the real host; do not wait
for a wave or rely on prompt-prose tests.

### Acceptance cases

- [ ] Repeated claim, finish, and worker-exit requests change SQLite exactly
  once; one worker never owns two claims.
- [ ] Pass and verifier failure are terminal. Infrastructure failure and
  interruption retain attempt evidence and return the task to `unclaimed`.
- [ ] Killing a worker before `result.json` requeues once; killing it after
  `result.json` publishes the saved result instead of rerunning the task.
- [ ] Killing only the controller leaves every worker and claim running. Its
  replacement adopts them before launching more; killing one worker removes
  that worker's VM/proxy tree without affecting its siblings.
- [ ] Killing the controller before, during, and after a multi-worker launch
  produces neither an untracked process nor a duplicate launch after restart.
- [ ] Starting two controllers for one node leaves exactly one lock holder able
  to reconcile or launch; losing that controller does not affect workers.
- [ ] Restarting the coordinator while a worker finishes loses no claim or
  result; the worker retries the same publication.
- [ ] Installing a new revision does not change a live worker's executable.
- [ ] Starting from zero with a large backlog launches multiple workers and
  repeatedly raises the target until an upward probe demonstrates a real
  bottleneck or throughput plateau. No static cap, one-at-a-time ramp, or wave
  tail is accepted.
- [ ] Repeated ticks while a launch is still settling do not launch duplicates;
  starting workers count as occupied and reserve rows they are about to claim.
- [ ] With backlog and healthy headroom, a failed, invalid, or conservative
  model response triggers prompt retry and geometric upward growth instead of
  an idle cycle.
- [ ] Under pressure, the controller stops admission without killing workers;
  after recovery it resumes growth on the next healthy cycle.
- [ ] An aggressive probe that causes worker OOMs preserves every task, records
  interrupted/infrastructure attempts, reclaims the rows, and adapts subsequent
  admission without manufacturing terminal benchmark failures.
- [ ] High utilization without stalls, regressions, or control failures does
  not by itself stop admission while backlog remains.
- [ ] A changing task mix cannot leave contradictory capacity bounds or strand
  claimable rows.
- [ ] The completed run has zero `unclaimed` and `running` rows, SQLite
  integrity is `ok`, every terminal task has evidence, and no worker, VM,
  proxy, marker, or temporary directory remains.
- [ ] OTEL can plot occupancy, utilization, throughput, overload, exits, and
  recovery per node for the complete run.

Judge the loop by terminal completions per hour, time to reach best observed
throughput, idle host time while backlog exists, infrastructure/interrupted
attempts per completion, and overload recovery time. The target is productive
adaptation, not zero retries or zero OOMs. A run fails the exit gate if healthy
capacity remains unused or the controller repeatedly thrashes without learning,
even when correctness tests pass.

Exit gate: focused deterministic and Linux process tests, rustfmt, and
warnings-denied Clippy pass; real-host evidence demonstrates autonomous,
aggressive saturation and recovery; controller and coordinator restarts lose no
work; OTEL explains the run; and the PR contains only this evaluation-runtime
slice against current `master`.

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

- Finish the immediate durable-evaluation-throughput slice above before adding
  another evaluation abstraction or benchmark family.
- Rebase and complete the VM-backed differential evaluation foundation in
  [PR #61](https://github.com/gakonst/nanocodex/pull/61) after its extracted
  Code Mode slice lands.
- Keep [PR #72](https://github.com/gakonst/nanocodex/pull/72) stacked until the
  base evaluation contract is merged, then validate StableBench with exact
  retained artifacts, canonical verifier output, scorer provenance, model
  usage, and cost.
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
3. [ ] Finish the immediate durable-neural-evaluation slice and satisfy its
   real-host saturation exit gate. Keep work in progress limited to this item.
4. [ ] Finish and merge the focused Code Mode parity slice in PR #95.
5. [ ] Reconcile and advance the Codex parity checkpoint with a complete commit
   classification and direct evidence for every adopted behavior.
6. [ ] Fix, validate, and merge desktop profile import in PR #93.
7. [ ] Build browser placement and presentation policy for private host and
   private VM sessions, then prove both through the CLI consumer.
8. [ ] Prototype the user-Chrome extension/native-host path; prove exact tab
   claiming, grouping, visible cursor feedback, interruption, leasing, and
   cleanup before exposing it as normal CLI policy.
9. [ ] Rebase and decide PR #79, then review PR #89 against the stable-core and
   application-policy boundaries above.
10. [ ] Rebase and merge PR #61, then complete the stacked StableBench work in
   PR #72 and record retained differential evidence.
11. [ ] Decide whether PR #32 still solves a demonstrated problem or should be
    replaced by a smaller application-owned experiment.
12. [ ] Cut the next release only after all selected milestones pass the full
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
