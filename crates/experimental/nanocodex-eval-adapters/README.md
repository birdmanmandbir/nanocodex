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
| Harbor, Terminal-Bench, Frontier-Bench, StableBench | Snapshots schema 1.1/1.3 packages without rewriting instructions, images, tests, or artifacts | Packaged `tests/test.sh` |
| Arena-Hard | Converts each question to a final-message case | Caller-packaged official Arena judge harness |
| OpenAI Evals `Match` / `Includes` | Reads registry YAML and JSONL; preserves their starts-with / substring behavior | Snapshotted deterministic harness |
| OpenAI `simple-evals` BrowseComp and GPQA Diamond | Reproduces the published prompt/data preparation and snapshots the pinned upstream implementation | OpenAI reference grader or answer extraction |
| HealthBench | Preserves the complete grader conversation and rubric metadata for single-turn cases | OpenAI reference rubric grader through the evaluator-pinned OpenAI judge |
| HealthBench Professional | Converts the public dataset shape and applies the published length adjustment for single-turn cases | OpenAI external reference grader through the evaluator-pinned OpenAI judge; OpenAI's reported internal harness is not public |
| Other OpenAI Evals classes | Refused by the declarative adapter | Official code through `ExternalHarness` |
| SWE-bench | Preserves problem statements and official instance-image naming; packages exact instance metadata | Caller-packaged official SWE-bench harness |
| MLE-bench, PaperBench, and private suites | Reads a prepared generic external manifest | Benchmark-owned harness |

Unsupported semantics fail during import. The adapters do not flatten
multi-message prompts, invent reference answers, translate custom Python
graders, or modify tasks to make a candidate pass.

The GPT-5.6 report's public benchmark families map onto the same small set of
routes rather than VM modes:

| Family | Import route |
| --- | --- |
| Terminal-Bench 2.1 | `harbor` |
| BrowseComp, HealthBench Professional, GPQA Diamond | `openai-simple-evals` |
| SWE-Bench Pro | `swe-bench` with its official instance images and harness |
| Agents' Last Exam, GDPval-AA, Artificial Analysis, FrontierMath, OSWorld, BenchCAD, CTF, SEC-Bench, ExploitBench, ExploitGym, GeneBench v1, KernelBench/KernelGen, NanoGPT, PostTrainBench, MMMU Pro, Toolathlon, MRCR, GraphWalks, and ARC-AGI | `external` with the benchmark owner's pinned image/Dockerfile and grader |
| OpenAI-internal or unreleased suites | Not importable until the owner supplies tasks and grading semantics |

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

## CLI

Imports are durable and idempotent:

```sh
nanocodex eval import harbor \
  --name terminal-bench-3 \
  --revision harbor@<commit> \
  /data/terminal-bench/tasks

nanocodex eval import arena-hard \
  --name arena-hard-v2 \
  --revision arena-hard-auto@<commit> \
  --harness /data/arena/official-judge-adapter \
  /data/arena/data/arena-hard-v2.0/question.jsonl

nanocodex eval import openai-evals \
  --name openai-proofreader \
  --revision openai-evals@<commit> \
  --harness crates/experimental/nanocodex-eval-adapters/assets/openai-evals \
  --eval proofreader.dev.v0 \
  /data/openai-evals/evals/registry

git clone https://github.com/openai/simple-evals /data/simple-evals
git -C /data/simple-evals checkout --detach <commit>

nanocodex eval import openai-simple-evals \
  --name browsecomp \
  --revision openai/simple-evals@<commit> \
  --harness crates/experimental/nanocodex-eval-adapters/assets/openai-simple-evals \
  --eval browse-comp \
  /data/simple-evals /data/browse_comp_test_set.csv

nanocodex eval import openai-simple-evals \
  --name healthbench-professional-smoke \
  --revision openai/simple-evals@<commit>+openai/healthbench-professional \
  --harness crates/experimental/nanocodex-eval-adapters/assets/openai-simple-evals \
  --eval health-bench-professional \
  --limit 2 \
  /data/simple-evals /data/healthbench_professional_eval.jsonl

nanocodex eval import swe-bench \
  --name swe-bench-verified \
  --revision swe-bench@<commit> \
  --harness /data/swe-bench/official-verifier-adapter \
  /data/swe-bench-verified.jsonl
```

The command prints the imported `tasks/` directory. Pass that directory to the
existing runner:

```sh
nanocodex eval --suite <printed-tasks-directory> --trials 5
```

Custom imports take caller-pinned local source. Manifest built-ins instead let
`EvaluationWorkspace::prepare` acquire their versioned authoritative inputs
under the evaluator state directory before invoking these network-free
importers. The deprecated `simple-evals` checkout is only a frozen internal
reference source for stable `browsecomp`, `gpqa-diamond`, and `healthbench`
catalog entries; it is not a profile-facing benchmark or execution framework.

Model judges use GPT-5.6 Sol through the evaluator-owned judge runtime. That
runtime uses the operator's local OpenAI subscription by default and the
explicitly selected API-key authentication as a fallback. It exposes the
Responses and Chat Completions protocol shapes needed by the pinned reference
graders, and records the effective grader model with their evidence. Only a
run-scoped endpoint and bearer token enter the isolated verifier; neither
candidate guests nor durable evidence receive provider credentials. Because
HealthBench Professional's published external setting uses GPT-5.4-low, a
GPT-5.6-Sol-graded smoke proves adapter behavior but is not labeled as a
directly comparable official HealthBench Professional score.

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
