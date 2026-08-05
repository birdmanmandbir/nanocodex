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
| MLE-bench, PaperBench, and private suites | Reads a prepared generic external manifest | Benchmark-owned harness |

Unsupported semantics fail during import. The adapters do not flatten
multi-message prompts, invent reference answers, translate custom Python
graders, or modify tasks to make a candidate pass.

Continuous benchmark rewards keep their raw named dimensions and declare a
task-owned binary classification policy. GraphWalks retains `f1` for plots and
uses exact-one classification for the generic pass axis; partial overlap is not
silently promoted to a passing attempt. The policy is part of import identity,
the immutable task package, each trial lock, and reconstructed reports.

The GPT-5.6 report's public benchmark families map onto the same small set of
routes rather than VM modes:

| Family | Import route |
| --- | --- |
| Terminal-Bench 2.1 | `harbor` |
| DeepSWE v1.1 | installed `deep-swe-v1.1` Harbor recipe |
| SWE-Bench Pro | Historical/reference-only: OpenAI retracted its recommendation after its July 2026 task audit |
| GeneBench Pro public package | installed `genebench-pro-public` recipe |
| GraphWalks | installed `graphwalks` recipe pinned to OpenAI's corrected February 2026 data; the public extractor's known prefix bug is intentionally preserved and reported |
| Agents' Last Exam, GDPval-AA, Big Finance Bench, LifeSciBench, HealthBench Professional, BrowseComp, GPQA, FrontierMath, OSWorld, BenchCAD, CTF, SEC-Bench, ExploitBench, ExploitGym, KernelGen, NanoGPT, PostTrainBench, MMMU Pro, gdp.pdf, AutomationBench, Toolathlon, MRCR, and ARC-AGI-3 | Not installed until a dedicated recipe or caller-supplied official `external` manifest exists |
| OpenAI-internal or unreleased suites | Not importable until the owner supplies tasks and grading semantics |

DeepSWE uses one lifecycle feature beyond ordinary same-VM Harbor tasks:
`pre_artifacts.sh` runs after agent tools are terminated, and its declared
artifacts are then copied into a pristine verifier VM. Nanocodex retains that
phase's stdout and stderr beside the verifier evidence. The native-only
`deep-swe-smoke` profile preserves DeepSWE's no-network candidate policy;
guest CLI harness comparisons remain blocked until their model control-plane
egress can be isolated from candidate shell egress.

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
importers. Deprecated OpenAI `simple-evals` recipes are intentionally outside
the supported catalog.

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
