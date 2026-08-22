# Nanocodex web

The public product site, native documentation, live browser-agent consumer,
repository browser, and evaluation evidence for Nanocodex. The coding agent is
the library; this application proves that the same owned Rust lifecycle can sit
behind an opinionated web interface without turning that interface into an SDK
protocol.

## Stack

- Vite + React
- Cloudflare Vite plugin and Workers runtime
- Wrangler for preview and deployment
- just-bash over the thread's OPFS filesystem, with browser `git` and `gh` compatibility commands
- Pierre Trees and Diffs for the file tree, source viewer, and the single virtualized commit stream
- TanStack Virtual for the commit quick-jump and evaluation indexes
- Derived job, trial, trajectory, and verifier views

The visual and content direction is captured in [`DESIGN.md`](DESIGN.md): a
Berkeley Mono-first, black-and-white simplification inspired by fx.sh and shaped
around Nanocodex's library ownership model. Treat that brief as the north star
while the existing surfaces are recomposed incrementally.

## Development

```bash
cd web
npm install
npm run dev
```

The homepage consumes the publishable `nanocodex` and `nanocodex-react`
packages under `../js`; it does not reach into generated WASM artifacts. Its
React integration wraps the terminal in `NanocodexProvider`, creates the
browser agent with `useAgent({ enabled, threadId })`, and observes its typed
event stream with `useAgentEvents`. React owns no Worker lifecycle, agent
history, credential policy, or model-loop state.

The local Worker and Vite client run together at `https://localhost:5173` using
the Cloudflare Vite-plugin layout.

### Documentation

The product guide lives in `docs/src/pages` and is rendered by the lazy native
Docs surface under `/docs`. The Markdown stays the source of truth; the Vite
application supplies the shared shell, responsive navigation, heading links,
code copy controls, and route-aware reading layout. `npm run build` checks that
every page entered the Docs bundle and generates `llms.txt` plus
`llms-full.txt` in the Cloudflare asset tree. The docs are not a second service,
generator, or visual system.

In development, Vite reads repository metadata from Git, serves working-tree
files on demand, and streams history directly from Git only when the commit
view opens. Startup does not generate or rewrite repository blobs. Set
`NANOCODEX_REPO` to point the development view at another checkout.

`npm run build` does not inspect Git or generate repository assets. Production
repository data is published separately to R2 by `npm run
publish:repository`. The publisher derives one coherent generation from a Git
commit, projects only the canonical `master` ref, uploads only
previously unseen source blobs and commit patches, builds one verified clone
pack for exactly those refs, uploads that pack in bounded immutable parts, and
stores new Git objects once in bounded immutable pack-entry shards. The Worker
streams the pack parts byte-for-byte as the complete pack for a fresh clone,
but uses the object graph and reusable shards to send only the closure missing
from an incremental or shallow fetch. Shards are compacted after a bounded
number of generations. Publication advances one
Durable Object pointer only after every referenced R2 object exists, so a failed
or concurrent publisher cannot expose mixed tree, history, or Git data. The
commit view resolves an immutable generation manifest, streams its aggregate
patch from bounded parts instead of issuing a request per commit, then parses
and publishes it in bounded batches while yielding between batches so the
first diff and scrolling stay responsive.

For this single-repository deployment, R2 owns immutable bytes and one Durable
Object owns the current generation with compare-and-swap publication. D1 is
deliberately absent: there is no repository registry, account model, search
index, or relational query to justify it. Publishing requires the same
`GIT_MIRROR_TOKEN` secret on the Worker and `NANOCODEX_GIT_TOKEN` in the
publisher environment. The publisher also requires `/api/health` to attest the
same complete Git SHA before it makes an authenticated request or uploads an
object:

```bash
NANOCODEX_GIT_ORIGIN=https://nanocodex.me-7fb.workers.dev \
NANOCODEX_GIT_TOKEN=... \
npm run publish:repository
```

If the Durable Object contains an obsolete publication shape, the publisher
stops before uploading anything. Repair it atomically after deploying the
current Worker by explicitly opting into a current-format replacement:

```bash
NANOCODEX_GIT_ORIGIN=https://nanocodex.me-7fb.workers.dev \
NANOCODEX_GIT_TOKEN=... \
NANOCODEX_REPAIR_INVALID_PUBLICATION=1 \
npm run publish:repository
```

The replacement is accepted only while the stored publication is invalid; it
cannot overwrite a valid generation or bypass its compare-and-swap head.

### Cloudflare-native CI

The repository also contains an application-owned CI control plane built from
Workers, Workflows, Containers, Durable Objects, and R2. It does not use
GitHub Actions, GitHub webhooks, GitHub status APIs, or Cloudflare Artifacts.
The existing browser-thread Git receiver remains unchanged and the public Git
mirror stays read-only.

A trusted publisher creates a deterministic compressed archive of exactly the
committed `master` tree plus a Git-blob manifest used for dependency-cache
fingerprints. Git replacement objects, grafts, unsafe local Git configuration,
global/system Git configuration, untracked files, tracked changes, and gitlinks
are rejected or isolated before publication. The two objects are uploaded
immutably to R2 before one Durable Object
compare-and-swap advances the source head. That same transaction writes a
dispatch outbox. Its alarm starts the Workflow with the deterministic ID
`ci-<commit>`, so an upload retry, a lost HTTP response, or a lost Workflow
create acknowledgement cannot create a second logical run.

Each Workflow checks out its immutable commit archive rather than resolving the
latest source head. A Cargo cache is keyed by the exact workspace manifest
graph. Bindings and website dependencies use separate content-addressed
snapshots: the bindings layer retains Cargo sources plus only its eight declared
project-root `node_modules` trees, while the website layer retains only the four
linked-package and site dependency trees it actually consumes. Their keys
include the exact relevant npm manifests, lockfiles, runner image, and
patch-package inputs. Git-sourced Cargo packages are restored from an immutable,
checksum-verified R2 bundle keyed by the committed `Cargo.lock` blob before
`cargo fetch`; a cold runner never clones those dependencies from GitHub.
After the shared Cargo download cache, the MSRV, policy, VM, npm, and
stable-build-snapshot branches start concurrently. The native target snapshot
is keyed by the exact Cargo graph and runner image rather than workspace source.
It retains Cargo homes, the completed stable test graph, the all-feature check
graph used by Clippy, and the fingerprint of the source that produced them.
Every consumer overlays the immutable current source and
touches all Rust inputs when that fingerprint changes, so Cargo reuses compatible
dependency output while rebuilding every affected crate, build script, and proc
macro. Quality branches from that reusable target and is content-addressed by
every Rust workspace input, including crate documentation and embedded prompts.
A matching publication restores the completed Clippy, independent-crate, and
rustdoc graph; changed Rust input reruns the full gate and publishes a new
30-day snapshot. Stable tests branch from that exact quality snapshot and start
only after the compile-heavy saturation phase releases the host, so their
wall-clock lifecycle assertions never compete with a Rust compiler. The
MSRV and JavaScript consumers follow, then both Python versions run together.
Cargo and libtest are explicitly capped at four CPUs, and the MSRV gate uses one
libtest thread, so a local Containers emulator cannot oversubscribe each runner
to every host core. Ten container slots leave room for parent runners that are
still draining logs. The two JavaScript dependency
layers seed concurrently on a cold head and avoid restoring the former
multi-gigabyte combined workspace into either consumer. The bindings gate
streams only its
small tested WASM package to R2 and skips its otherwise multi-gigabyte workspace
snapshot. The website starts from its site-only dependency snapshot, restores
that checksum-verified WASM package, and streams its tested deployment tar
straight back to R2. Runtime correctness suites are never skipped, while the
deterministic quality gate can reuse an exact successful result. No correctness
runner is retried; only network-backed dependency preparation gets one retry.
Success and failure logs, step records, final results, required parent/cache
snapshots, and cache pointers are retained in the
`nanocodex-ci` R2 bucket; no separate hosted artifact product is required.
All eight terminal Rust, Python, bindings, and website runners explicitly skip
workspace snapshots; quality retains the parent snapshot consumed by stable.
Immutable source archives live in the separately credentialed
`nanocodex-ci-source` bucket.

Every Sandbox registers its exact runner ID under the run before container work
starts. An authenticated termination first writes a run tombstone, terminates the
Workflow, then reconciles every registered Sandbox across three teardown sweeps;
a runner rechecks the tombstone between long phases, and a failed teardown retains
its marker for a safe operator retry. A deterministic gate failure writes its own
stop marker and immediately tears down active siblings instead of waiting for a
known-doomed fanout to finish.

Runner output is captured through a bounded 32 MiB head plus 32 MiB tail per
stream. The step record includes observed/stored byte counts and a truncation
flag. Every command uploads those bounded logs directly to R2 before its
Sandbox is destroyed, while Workflow state carries only the small preview and
R2 references. Timeout cleanup terminates the command process group, drains the
capture FIFOs, and retains the early diagnostic before recording a typed
timeout failure. Snapshot creation and log finalization have a separate
five-minute Workflow margin beyond each command timeout. The pinned
`@cloudflare/ci` 0.1.0 package is patched by `postinstall` to provide this
behavior until the runner exposes the same R2 log sink upstream. Runner images
also pin Node 22.15.0, both Python interpreters, the Rust and MSRV toolchains,
and every installed Cargo utility; a floating package-manager runtime cannot
silently change the gate.

For a local run, start OrbStack or another Docker-compatible engine, put the
four development-only CI values in the repository `.env`, and run
`npm run dev:ci` from this directory. The command rebuilds the development
Worker, explicitly enables both container-backed Durable Objects, and serves
the dashboard plus source API at `http://127.0.0.1:8787/ci`. The explicit
container opt-in is required because the normal visual-development loop keeps
containers disabled for startup speed.

Create both buckets, configure S3 API credentials scoped only to the backup
bucket, and set separate source-publication and Workflow-control tokens before
the first deployment:

```bash
cd web
npx wrangler r2 bucket create nanocodex-ci
npx wrangler r2 bucket create nanocodex-ci-source
npx wrangler r2 bucket lifecycle add nanocodex-ci ci-backups backups/ --expire-days 31 --force
npx wrangler r2 bucket lifecycle add nanocodex-ci ci-cache cache/ --expire-days 31 --force
npx wrangler r2 bucket lifecycle add nanocodex-ci ci-runs runs/ --expire-days 90 --force
npx wrangler secret put CI_SOURCE_WRITE_TOKEN
npx wrangler secret put CI_CONTROL_TOKEN
npx wrangler secret put R2_ACCESS_KEY_ID
npx wrangler secret put R2_SECRET_ACCESS_KEY
npm run deploy
```

Sandbox TTLs are restore-time checks, not physical deletion. The three
lifecycle rules above are therefore required to bound backup, cache-pointer,
and run-evidence storage. The Durable Object separately removes source objects
when their terminal run ages out of the retained 100-run index. Development
uses `nanocodex-ci-development` and `nanocodex-ci-source-development`; create
those buckets and configure separate `--env development` secrets rather than
reusing production credentials or cache state.

Publish the RustSec snapshot and lockfile-addressed Git dependency bundle, then
publish a clean committed `master` checkout from the trusted machine. Neither
dependency publisher contacts a remote: the RustSec publisher rematerializes
the selected commit through a local shallow fetch into a fresh sanitized Git
repository, while the Cargo publisher verifies that every required cached Git
checkout is the exact clean lockfile revision before packing it in offline
mode. Publisher subprocesses receive no CI publication token. Existing immutable
objects are reused without rebuilding or uploading bytes.

```bash
NANOCODEX_CI_ORIGIN=https://nanocodex.me-7fb.workers.dev \
NANOCODEX_CI_TOKEN=... \
NANOCODEX_RUSTSEC_REPO=/path/to/advisory-db \
npm run publish:ci-rustsec

NANOCODEX_CI_ORIGIN=https://nanocodex.me-7fb.workers.dev \
NANOCODEX_CI_TOKEN=... \
npm run publish:ci-cargo-vendor

NANOCODEX_CI_ORIGIN=https://nanocodex.me-7fb.workers.dev \
NANOCODEX_CI_TOKEN=... \
NANOCODEX_RUSTSEC_REVISION=<full-published-advisory-db-commit> \
npm run publish:ci-source
```

`GET /api/ci/runs` and `GET /api/ci/runs/<40-hex-commit>` expose the retained
Workflow/result state. Successful bindings and website gates export their
tested archives directly to immutable, checksum-verified R2 and serve them at
`GET /api/ci/runs/<40-hex-commit>/artifacts/{web-wasm,web-dist}.tar`; this is the
owned replacement for a hosted artifact service. Step records and logs are available
under `GET /api/ci/runs/<commit>/steps/<step>/{result.json,stdout.log,stderr.log}`.
An authenticated `POST` request to
the latter path's `/terminate` action uses `CI_CONTROL_TOKEN`. Runs are not
restarted in place because commit-addressed evidence is immutable; publish a
new commit for a new run. The publisher's
`NANOCODEX_CI_TOKEN` must contain the value configured as
`CI_SOURCE_WRITE_TOKEN`. The archive URL is
public and commit-addressed because this is a public source repository; write
authority and R2 backup credentials never enter a runner or checkout URL.

This Worker pipeline covers every current Linux test, quality, policy,
VM-guest, browser/WASM, Python, and website gate. Cloudflare Containers are
Linux-only, so the existing macOS matrix entry cannot run on this substrate;
the CodeQL workflow scan, production promotion, release, and nightly workflows
are outside this first CI slice. Every run pins the exact RustSec
revision and owned archive checksum, verifies the extracted Git checkout, and
runs `cargo deny --frozen check`; neither the policy gate nor cold Cargo setup
contacts GitHub.

Production serves the website indexes, immutable file and patch objects, and a
read-only Git protocol-v2 endpoint from that publication. Clone the mirror with
`git clone https://nanocodex.me-7fb.workers.dev/git`. This CI slice deliberately
stops at the checksum-addressed tested deployment tar; it does not deploy or
publish production state. Promotion can consume that exact artifact later
without adding GitHub to the CI runtime.

Each browser thread owns an OPFS working tree and an `origin` Cloudflare Git
remote on branch `nanocodex`. The Files and Commits surfaces read that thread's
actual Git objects in the browser; file blobs and commit patches are generated
on demand and released when the view refreshes. Push and pull notifications
cross the page/agent Worker boundary so an open repository view can preserve
its last complete render until the replacement snapshot is ready.

### Live eval view

`/evals` is part of the same production Vite and React application as the
Nanocodex homepage, embedded TUI, repository tree, and commit history. The
website reads its public API directly from the Cloudflare Worker. D1 owns the
task board and normalized result index; R2 owns task packages, case records,
and complete evidence. There is no coordinator host, tunnel, origin override,
or Access credential in the website read path.

Native benchmark hosts are disposable compute clients. They claim R2-backed
tasks from the Worker and authenticate every mutation with
`NANOCODEX_EVALS_WRITE_TOKEN`; they are never an authority for website reads.

The API is deliberately workset-oriented: the client loads the retained
workset index, drills into one workset's task summaries, loads one selected
treatment matrix, then requests a single opaque case ID for terminal evidence.
TanStack Query is the only application cache and owns polling, cancellation,
retry, and the overview/workset/task/case query lifetimes. There is no second eval-only HTML entry,
React root, Vite configuration, Node eval server, or browser-side SQL path.

The homepage is also a real embedded-agent demo with three deliberately thin
layers:

- `../js/bindings` publishes `nanocodex`, the viem-v3-style imperative client.
  Runtime entrypoints expose flattened `Agent.create` factories, decorated
  domain actions, standalone `Actions` namespaces, and typed watcher handles.
- `../js/react` publishes `nanocodex-react`, the wagmi-like headless React owner. Its provider and
  hooks manage the module Worker lifecycle, readiness, commands, and event
  subscriptions without imposing presentation policy.
- `nanocodex/tools` owns the framework-independent live React document,
  bounded workspace store, and typed artifact tool used by the web consumer.
- `AgentTerminal` is the optimized Ratatui-faithful consumer: native colors,
  rendering hierarchy, queue/steer behavior, `/btw`, historical branch editing,
  branch navigation, per-branch drafts, clipboard images, and key bindings over
  virtualized transcripts.

The module Worker loads the generated `nanocodex-wasm` package, and the Rust
engine owns the persistent Responses session, typed history, event stream, and
tool loop. Each thread opens one OPFS workspace shared by just-bash, Rust
`apply_patch`, isomorphic-git, the file viewer, commit history, uploads,
downloads, and the artifact dock. The model receives the standard
`exec_command` and Rust `apply_patch` tools rather than separate list/read/write
or Git tools. Shell commands include normal virtual Unix commands plus `git`
and `gh`; `git push origin nanocodex` publishes the same objects the
Commits view reads from the Cloudflare thread remote. Files survive agent,
Worker, and page restarts without being copied into conversation snapshots.
The Cloudflare Worker upgrades `/api/responses` and proxies OpenAI
tool calls. It accepts a user-provided OpenAI key into a one-hour Durable Object
session and returns only an opaque `HttpOnly`, `SameSite=Strict` cookie. The key
is never placed in a URL, local storage, React state, or WASM configuration.

Custom interfaces use the typed `render_artifact` tool composed by
`nanocodex/tools/browser`, alongside `exec_command`, `web__run`, and
`image_gen__imagegen`. The tool accepts JavaScript source defining a real React
`App`; `React`, an `html` tagged-template helper, and `sendPrompt` are supplied by the isolated iframe
runtime. Published documents live under `.nanocodex/artifacts` in the same Git
working tree and open in a fullscreen dock. Reusing an artifact ID replaces the
interface in place, so voice or text turns can continuously retheme and extend
it. Generated code has no imports, network access, or access to the parent page;
explicit `sendPrompt` actions re-enter the normal queued prompt lifecycle.
The browser agent requires an explicit user OpenAI key or ChatGPT session. A
presented session that cannot be read fails explicitly instead of falling back
to another credential.

The reusable `browser(...)` tool bundle gives the browser agent a bounded
`dataset` tool. It can inspect public
Parquet URLs, Hugging Face dataset/config/split exports, and uncompressed JSONL
URLs without downloading whole datasets into memory. Parquet reads use HTTP
ranges and filter/projection pushdown where possible; JSONL reads incrementally
scan the response stream. Dataset handles are scoped to an agent session. Query
limits and offsets accept any nonnegative safe range; input-byte and output-byte
budgets remain bounded. Partial results report `complete: false` and an opaque
`nextCursor` that retains projection and filters while resuming at the physical
Parquet row batch or JSONL byte position. The implementation and Parquet codecs
are lazy chunks, so ordinary agent sessions do not download them. Direct sources
must permit browser CORS. Parquet sources must honor byte-range requests; JSONL
sources must honor them when continuing from a cursor.

For example, ask the web agent to “inspect the `main` config’s `train` split of
`openai/gsm8k`, show its schema, and find five examples containing arithmetic.”
The resulting tool flow is equivalent to:

```json
{"operation":"open","source":{"kind":"huggingface","dataset":"openai/gsm8k","config":"main","split":"train"}}
{"operation":"query","dataset_id":"<returned id>","columns":["question","answer"],"filters":[{"column":"question","op":"contains","value":"how many"}],"limit":5}
{"operation":"query","dataset_id":"<returned id>","cursor":"<returned nextCursor>","limit":5}
{"operation":"close","dataset_id":"<returned id>"}
```

Run `npm run bench:dataset` in `js/bindings` for the deterministic 100,000-row
Snappy Parquet/JSONL browser-path benchmark. It reports cold and repeated query
latency, pulled bytes, range requests, scanned rows, and cache hits.

Development uses `vite-plugin-mkcert` so the browser Agent exercises its secure
context requirements under the same HTTPS boundary as production.

Local development reads the optional ignored root `.env` through the repository
workflow. BYOK uses the `BYOK_SESSIONS` Durable Object binding; ChatGPT login
uses its separate server-owned session boundary.

The browser agent does not use JavaScript Promise Integration (JSPI). Its
consumer startup gate checks only the platform APIs used by the shipped path:
a secure context, module Worker support, WebAssembly, WebSocket,
`crypto.randomUUID`, OPFS, and Web Locks. These are normal current stable
Safari/iPhone Safari capabilities; the real wasm-bindgen initialization remains
the authority for the shipped module and reports an actionable failure instead
of requiring Safari Technology Preview or a beta-only JSPI API.

Streaming events are coalesced once per animation frame before updating the
semantic transcript, and each independently scrolling transcript is
virtualized. `npm test` keeps the
event accumulator bounded under a 20,000-delta burst and covers assistant,
reasoning, and tool lifecycle updates.

The homepage also exposes the release contract: the checksum-verifying install
command, in-place `nanocodex update`, the crates.io SDK entry point, and links
to the latest GitHub Release and grouped conventional-commit changelog. GitHub
release notes also credit each pull request contributor.

Navigation stays available whenever an input is not active: `H`, `T`, `C`, `R`,
and `E` switch between Home, Code, Commits, Requests, and Evals. The repository
homepage is the root route. In Code, `Ctrl+P` searches the left tree and `Ctrl+F` opens the
fuzzy all-file jumper. In Commits, `F` searches history. Code and commit
scrolling are left to Pierre CodeView and the browser's native input behavior.

## Production

Production promotion is intentionally separate from the Cloudflare-native CI
slice above. CI validates and retains the complete tested deployment graph but
does not mutate the hosted Worker. Local commands build and preview it:

```bash
npm run build
npm run preview
```

For a break-glass production deployment, start from a clean commit and preserve
the same attestation contract before running `publish:repository`:

```bash
npm run deploy
```

The deploy command requires `HEAD` to equal the fetched `origin/master`, binds
that full commit SHA into the Worker version, rolls only that version to 100%
without rebuilding unchanged containers, and does not return successfully until
the live health endpoint attests the same revision.

Do not publish repository data until the hosted `/api/health` reports that
exact `deployment_sha`. The publisher enforces this ordering independently. An
authenticated operator can publish the already-deployed master revision with:

```bash
NANOCODEX_GIT_ORIGIN=https://nanocodex.me-7fb.workers.dev \
NANOCODEX_GIT_TOKEN=... \
npm run publish:repository
```
