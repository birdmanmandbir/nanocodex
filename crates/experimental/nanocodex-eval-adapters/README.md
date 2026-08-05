# nanocodex-eval-adapters

This crate converts third-party benchmark layouts into the immutable task
boundary owned by `nanocodex-eval`. It contains format knowledge, not evaluator
or VM policy.

```text
third-party source at a pinned revision
│
├── Harbor adapter ─────────────── lossless existing task packages
├── Arena/OpenAI adapters ──────── prompts, official source, final answers
├── SWE-bench adapter ──────────── official instance images + harness
├── GeneBench-Pro adapter ──────── staged scientific data + reference grader
├── GraphWalks adapter ─────────── official Parquet + published F1 grader
├── MRCR adapter ───────────────── typed transcript + similarity grader
├── HealthBench Professional ───── typed transcript + isolated rubric judge
├── GDPval public ───────────────── workspace artifacts + pairwise judge
├── GPQA Diamond ────────────────── seeded choices + exact-answer grader
└── external harness adapter ───── benchmark-owned executable semantics
                 │
                 ▼
             DatasetPlan
                 │
                 ▼
        content-addressed ImportStore
        └── <dataset>/<digest>/
            ├── dataset.json       safe provenance, no local source paths
            └── tasks/<case>/      complete immutable execution inputs
                 │
                 ▼
       normal Task / VM / scheduler / resume / evidence path
```

Import is a build step. Once imported, task image preparation, reusable image
and verifier caches, fresh writable overlays, scheduling, resume, and retained
evidence have no benchmark-specific branches.

## Adapter matrix

| Source | Adapter behavior | Grading authority |
| --- | --- | --- |
| Harbor, Terminal-Bench, DeepSWE, Frontier-Bench, StableBench | Snapshots schema 1.1/1.3 packages without rewriting instructions, images, tests, or artifacts; preserves optional pre-verifier artifact capture | Packaged `tests/test.sh` |
| Arena-Hard | Converts each question to a final-message case | Caller-packaged official Arena judge harness |
| OpenAI Evals `Match` / `Includes` | Reads registry YAML and JSONL; preserves their starts-with / substring behavior | Snapshotted deterministic harness |
| Other OpenAI Evals classes | Refused by the declarative adapter | Official code through `ExternalHarness` |
| SWE-bench | Preserves problem statements and official instance-image naming; packages exact instance metadata | Caller-packaged official SWE-bench harness |
| GeneBench-Pro public package | Makes only problem `data_files/` candidate-visible and retains config, ground truth, tolerances, and grader outside the candidate VM | OpenAI's pinned `reference_grader.py`; `passed` is authoritative |
| GraphWalks | Reads both official Parquet partitions without flattening prompts and retains expected node sets outside the candidate VM | OpenAI's published final-line extraction and set-F1 contract; raw `f1` is authoritative |
| MRCR v2 | Reads all six corrected official Parquet partitions and preserves alternating user/assistant messages as a hashed typed transcript | OpenAI's published prefix check and `difflib.SequenceMatcher` ratio; raw `similarity` is authoritative |
| HealthBench Professional | Reads OpenAI's pinned 525-case release, preserves complete conversations, and keeps physician responses, canary, and rubrics outside the candidate VM | Published rubric equation and length adjustment with an evaluator-owned GPT-5.6 Sol low judge; this is a public reproduction, not the unavailable internal grader |
| GDPval public release | Reads OpenAI's pinned 220-task Parquet release, exposes only task reference files to the candidate, and keeps expert deliverables and 10,453 human-authored rubric items verifier-only | Order-swapped pairwise comparison against the expert deliverable with an evaluator-owned GPT-5.6 Sol low judge; this is a public reproduction, not Artificial Analysis GDPval-AA v2 |
| SWE-Atlas QnA | Reads the pinned 124-task split and preserves its prompts and official repository images; makes the repository's harness-level agent network restriction explicit, transfers only `/logs/agent/answer.txt`, and runs grading in a pristine verifier VM | Benchmark-owned rubric verifier through the evaluator's subscription-backed OpenAI-compatible judge endpoint |
| GPQA Diamond | Reads the authors' pinned 198-question Diamond CSV and reproduces their continuous CPython seed-0 answer permutation and zero-shot prompt | Authors' ordered answer parser and deterministic exact-letter comparison |
| MLE-bench, PaperBench, and private suites | Reads a prepared generic external manifest | Benchmark-owned harness |

Unsupported semantics fail during import. The adapters do not flatten
multi-message prompts, invent reference answers, translate custom Python
graders, or modify tasks to make a candidate pass.

Continuous benchmark rewards keep their raw named dimensions and declare a
task-owned binary classification policy. GraphWalks retains `f1` and MRCR
retains `similarity` for plots; both use exact-one classification for the
generic pass axis so partial credit is not silently promoted to a passing
attempt. HealthBench retains each rubric judgment, raw example score, response
length, and `length_adjusted_score`; its official aggregate is the clipped mean
of adjusted example scores, so individual values may be outside `[0, 1]`.
GDPval retains both presentation orders, every rubric decision, retry evidence,
and `public_score`; Artificial Analysis's exact judge panel, office-format
patches, and Elo pipeline are not public, so this score must never be labeled
GDPval-AA v2. The policy is part of import identity, the immutable task package,
each trial lock, and reconstructed reports.

The GPT-5.6 report's public benchmark families map onto the same small set of
routes rather than VM modes:

| Family | Import route |
| --- | --- |
| Terminal-Bench 2.1 | `harbor` |
| DeepSWE v1.1 | installed `deep-swe-v1.1` Harbor recipe |
| SWE-Bench Pro | Historical/reference-only: OpenAI retracted its recommendation after its July 2026 task audit |
| GeneBench Pro public package | installed `genebench-pro-public` recipe |
| GraphWalks | installed `graphwalks` recipe pinned to OpenAI's corrected February 2026 data; the public extractor's known prefix bug is intentionally preserved and reported |
| MRCR v2 | installed `mrcr-v2` recipe pinned to OpenAI's corrected December 2025 data and published continuous grader |
| HealthBench Professional | installed `healthbench-professional` recipe pinned to OpenAI's 525-case public release and published scoring equation; exact internal-grader parity is unavailable |
| GDPval public release | installed `gdpval` recipe pinned to OpenAI's 220-task release; a public pairwise reproduction, not the unavailable GDPval-AA v2 pipeline |
| SWE-Atlas QnA | installed `swe-atlas-qna` recipe pinned to Scale's public 124-task QnA split; native Nanocodex runs with candidate networking disabled, while guest-CLI comparison awaits model-control-plane-only egress |
| GPQA Diamond | installed `gpqa-diamond` recipe pinned directly to the authors' 198-question release; it does not depend on deprecated `simple-evals` |
| BrowseComp | installed dedicated `browsecomp` recipe pinned to OpenAI's encrypted 1,266-row release; the generic deprecated `simple-evals` suite is not installed, and grading is labeled as a subscription-judge reproduction |
| ARC-AGI-3 | installed `arc-agi-3-public-smoke` recipe pinned to the official toolkit and benchmarking agent; `ls20` exercises the live action/frame/scorecard plumbing with an anonymous key and a three-action cap. Its environment score is evidence-only: an official-comparable run still requires the evaluator-owned per-frame model-turn topology |
| Agents' Last Exam, Big Finance Bench, LifeSciBench, FrontierMath, OSWorld, BenchCAD, CTF, SEC-Bench, ExploitBench, ExploitGym, KernelGen, NanoGPT, PostTrainBench, MMMU Pro, gdp.pdf, AutomationBench, Toolathlon, and ARC-AGI-3 | Not installed until a dedicated recipe or caller-supplied official `external` manifest exists |
| OpenAI-internal or unreleased suites | Not importable until the owner supplies tasks and grading semantics |

DeepSWE uses one lifecycle feature beyond ordinary same-VM Harbor tasks:
`pre_artifacts.sh` runs after agent tools are terminated, and its declared
artifacts are then copied into a pristine verifier VM. Nanocodex retains that
phase's stdout and stderr beside the verifier evidence. The native-only
`deep-swe-smoke` profile preserves DeepSWE's no-network candidate policy;
guest CLI harness comparisons remain blocked until their model control-plane
egress can be isolated from candidate shell egress.

SWE-Atlas QnA has the same guest-CLI boundary. Its checked-in task packages say
`allow_internet = true`, but the official runner narrows that during the agent
phase to only the selected model API hostname. The dedicated adapter therefore
runs native Nanocodex with no candidate network and a separate public-network
verifier. It refuses to present the current unrestricted stock-Codex VM path as
a comparable result; that arm enters the smoke only with model-control-plane-
only egress.

An `external` route means the normalized execution contract can preserve and
run a supplied official harness. It does not claim that a private dataset,
gated image, or unpublished grader is bundled here. Docker Compose sidecar
services and their service-owned artifact files also remain explicit
unsupported execution semantics; importing never silently drops those files.

Python grader code is not embedded in the Rust library or CLI. Import accepts
ordinary harness directories and pinned upstream checkouts, hashes every
consumed file, and snapshots them under each immutable task's `tests/`
package. The verifier sees that package at `/tests`; resumed runs therefore do
not depend on the original checkout. A shared dataset mount can replace the
per-task snapshot later if its measured storage cost warrants the extra
runtime contract.

## Profile CLI

Preparation acquires every selected built-in source, normalizes all selected
tasks, and prepares their runtimes once. Running then consumes only the
published preparation:

```sh
nanocodex eval prepare adapter-smoke
nanocodex eval run adapter-smoke
```

Custom imports take caller-pinned local source. Manifest built-ins instead let
`EvaluationWorkspace::prepare` acquire their versioned authoritative inputs
under the evaluator state directory before invoking these network-free
importers. Deprecated generic OpenAI `simple-evals` recipes are intentionally
outside the supported catalog. BrowseComp has a dedicated direct-data adapter
because OpenAI retains it as a reference implementation; HealthBench
Professional uses its direct official dataset release and an evaluator-owned
judge.

## External harness manifest

Executable benchmark semantics use one small transport manifest:

```toml
schema_version = "1"
name = "paperbench"

[source]
kind = "paperbench"
revision = "paperbench@<commit>"

[[case]]
id = "paper-001"
prompt = "Reproduce the supplied paper and leave all artifacts in the workspace."
output = "workspace" # or "final_message"
oci_image = "registry.example/paperbench/paper-001@sha256:<digest>"
harness = "paper-001/tests"
allow_internet = false

[case.resources]
cpus = 8
memory_mb = 32768
storage_mb = 102400
gpus = 0
```

Paths are relative to the manifest. `environment = "paper-001/environment"`
may replace `oci_image` for a Dockerfile context. The harness must contain
`test.sh`; a harness `Dockerfile` selects a separately imaged verifier.

The candidate mutates its workspace. For `final_message`, the evaluator writes
the exact assistant message to `answer.txt` under
`$NANOCODEX_EVAL_WORKSPACE` immediately before verification. A harness writes
either:

- `/logs/verifier/reward.txt` containing one finite number, or
- `/logs/verifier/reward.json` containing non-empty named finite rewards.

All other files under the harness remain benchmark-owned evidence or code.
