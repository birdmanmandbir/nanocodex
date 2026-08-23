# Nanocodex web

The public product site, native documentation, live browser-agent consumer,
repository browser, and evaluation evidence for Nanocodex. The coding agent is
the library; this application proves that the same owned Rust lifecycle can sit
behind an opinionated web interface without turning that interface into an SDK
protocol.

The public demo family is explicit in the shared navigation:

- **Home** explains the library and links the independent proofs.
- **Agent** is one player using the browser-owned Rust/WASM agent in the TUI.
- **Multiplayer** is many humans in one ordered, replayable Durable Object room
  with one private, host-invoked, tool-free managed agent.
- **World** is one human in a game world populated by many browser-owned AI
  residents.

Multiplayer is the managed-agent deployment proof rather than another browser
agent. The website Worker forwards only `/v1/rooms` through its
`MULTIPLAYER_BACKEND` Service Binding. Invite capabilities stay in URL
fragments until exchanged for room-scoped HttpOnly cookies; the browser sees
room cursors and final agent replies, never managed agent/turn capabilities or
provider credentials. The managed runtime, in turn, has only a private
credential-broker Service Binding and fixed placeholders for both OAuth and
normal OpenAI API-key modes.

Exact `POST /v1/rooms` requests receive a create-room-only capability from the
website Worker's `MULTIPLAYER_ALLOCATOR_TOKEN` secret. The proxy strips every
browser-supplied `Authorization` header, and the Multiplayer page never asks
for or stores a deployment credential. Configure it to the same random value
as the private managed Worker's `NANOCODEX_ROOM_ALLOCATOR_TOKEN`; it is
deliberately distinct from `NANOCODEX_ADMIN_TOKEN` and cannot create, inspect,
prompt, or delete raw managed agents:

```bash
cd web
npx wrangler secret put MULTIPLAYER_ALLOCATOR_TOKEN
```

The managed Worker remains `workers_dev = false`; its
`MULTIPLAYER_BACKEND` Service Binding is the production entry point. Production
also fails closed unless the checked-in per-client and global room-allocation
rate-limit bindings are available; cross-origin allocation requests are
rejected before the server capability is used. A singleton backend quota object
adds the authoritative cross-location ceiling: 16 active two-hour rooms, 32
allocations/hour, and 240 admitted agent turns/hour across the deployment.

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

Local Multiplayer development uses the remote `nanocodex-durable-agent`
Service Binding, so it requires Cloudflare authentication, a deployed private
managed Worker, and a local `MULTIPLAYER_ALLOCATOR_TOKEN` matching that
Worker's `NANOCODEX_ROOM_ALLOCATOR_TOKEN`. Keep the default HTTPS development
origin so room cookies remain Secure and sockets use `wss`; see the
[Cloudflare Worker example](../examples/cloudflare-workers/README.md#multiplayer-managed-agent-rooms)
for the deployment and live-smoke workflow.

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
Workers, Workflows, Containers, Durable Objects, R2, and one authenticated
Apple Silicon runner. It replaces GitHub Actions and Cloudflare Artifacts;
GitHub remains the authoritative Git, pull-request, and commit-status service.
The existing browser-thread Git receiver remains unchanged and the public Git
mirror stays read-only.

Two trusted polling controllers replace repository webhooks. The master
controller fast-forwards a clean attached checkout to authoritative
`origin/master`, refreshes the official RustSec checkout, publishes the exact
source and full `Cargo.lock` vendor bundle, waits for Cloudflare CI, posts the
single GitHub status context `ci success`, deploys only the checksum-verified
green `web-dist.tar`, publishes the Cloudflare Git mirror, and verifies the live
Worker and mirror at the same commit. A separate, credential-minimal PR
controller reads open pull requests from GitHub, constructs each exact tested
merge commit in a disposable checkout, publishes that lane, and posts status on
the PR head. PR-derived work receives no deploy, mirror, registry, release, or
Cloudflare account authority.

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
Every consumer overlays the immutable current source and compares a retained
per-file content manifest. Unchanged Rust inputs are backdated to preserve Cargo
fingerprints while only changed files receive a fresh timestamp, so compatible
crate, build-script, and proc-macro output survives an unrelated source edit.
Quality branches from that reusable target and is content-addressed by
every Rust workspace input, including crate documentation and embedded prompts.
A matching publication restores the completed Clippy, independent-crate, and
rustdoc graph; changed Rust input reruns the full gate and publishes a new
30-day snapshot. Stable tests branch from the reusable native target and start
only after the compile-heavy saturation phase releases the host, so their
wall-clock lifecycle assertions never compete with a Rust compiler. The
static VM build uses the same exact Rust-input key for its compact successful
result, avoiding an identical cross-target rebuild on website-only changes. The
stable and MSRV result records use that Rust input surface too, so site-only
publications restore their completed suites. Cargo deny and architecture policy
are keyed only by the manifest graph, policy files, and their executable checks.
The independent spelling gate keys the whole repository through one canonical
synthetic tree blob instead of expanding thousands of paths, so a documentation
edit reruns only the cheap whole-tree check. JavaScript consumers follow, then
both Python versions run together.
Each Python gate is content-addressed by the exact local Rust dependency closure
of the binding, its Python consumer inputs, and the pinned runner image. VM,
eval, and CLI source therefore cannot evict either wheel attestation. The static
guest gate follows its own VM -> tools -> OpenAI API closure. A successful
Python miss tests the installed release wheel before retaining only a minimal
result snapshot; an exact hit does not compile or install that wheel again.
Cargo and libtest are explicitly capped at four CPUs, and the MSRV gate uses one
libtest thread, so a local Containers emulator cannot oversubscribe each runner
to every host core. Ten container slots leave room for parent runners that are
still draining logs. The two JavaScript dependency
layers seed concurrently on a cold head and avoid restoring the former
multi-gigabyte combined workspace into either consumer. The bindings gate
keys its complete four-way verification fanout to all Rust, package, test, and
consumer inputs. A miss retains only the tested WASM tar; an exact hit restores
that small result instead of rerunning unchanged JavaScript suites. A tiny
terminal child streams the checksum-verified package to R2 without snapshotting
a multi-gigabyte workspace. Its immutable digest also addresses a small R2 copy,
so the website cache command is independent of the producing commit. The website
starts from its site-only dependency snapshot, restores that checksum-verified
WASM package, and streams its tested deployment tar straight back to R2.
Deterministic Rust, MSRV, quality, policy, static-VM, Python, bindings, and
website verification gates reuse successful results whenever their exact
declared inputs are unchanged. No correctness runner is retried; only
network-backed dependency preparation gets one retry.
Wrangler's local R2 fallback exposes development-only, run-scoped HTTP ranges
that each Sandbox pulls in verified 64 MiB pieces before checking the exact
squashfs byte count and extracting it. The transfer never enters Cloudflare's
local RPC file stream, which can wedge after enough cumulative data. Each range
has a progress deadline and clean retry; only large restores serialize, compact
result and artifact snapshots can pass, and production keeps the native mounted
R2 restore path.
Success and failure logs, step records, final results, required parent/cache
snapshots, and cache pointers are retained in the
`nanocodex-ci` R2 bucket; no separate hosted artifact product is required.
Five terminal Rust, bindings, and website runners explicitly skip workspace
snapshots. Quality retains the parent snapshot consumed by stable; static VM and
each Python gate retain an empty workspace solely as content-addressed success
records.
Immutable source archives live in the separately credentialed
`nanocodex-ci-source` bucket.

Normal CI also emits raw native CLIs at
`runs/<commit>/artifacts/nanocodex-<target>`. Linux builds inside the pinned
Container; macOS runs the stable workspace suite and arm64 release build in one
offline, network-denied claim on the authenticated external runner. The runner
downloads the same source and complete Cargo vendor bundle, validates a thin
arm64 Mach-O, and uploads only bounded logs and the binary. Its long-lived host
is defense in depth rather than VM isolation, so it must run under a dedicated,
credential-empty macOS account.

Every Sandbox registers its exact runner ID under the run before container work
starts. An authenticated termination first writes a run tombstone, terminates the
Workflow, then reconciles every registered Sandbox across three teardown sweeps;
a runner rechecks the tombstone between long phases, and a failed teardown retains
its marker for a safe operator retry. A deterministic gate failure writes its own
stop marker and immediately tears down active siblings instead of waiting for a
known-doomed fanout to finish. Progress records attribute the actual runner as
failed and mark interrupted siblings as terminated, so the dashboard never
misreports collateral teardown as a second test failure.

Runner output is captured through a bounded 32 MiB head plus 32 MiB tail per
stream. The step record includes observed/stored byte counts and a truncation
flag. Every command uploads those bounded logs directly to R2 before its
Sandbox is destroyed, while Workflow state carries only the small preview and
R2 references. Long-running commands are observed through five-second process
status requests instead of one lifetime SSE connection, so a local proxy stream
cannot erase healthy work at its five-minute boundary. The same loop checks run
tombstones every 30 seconds and bounds transient connection retries. Timeout
cleanup terminates the command process group, drains the capture FIFOs, and
retains the early diagnostic before recording a typed timeout failure. Snapshot
creation and log finalization have a separate five-minute Workflow margin beyond
each command timeout. The pinned `@cloudflare/ci` 0.1.0 package is patched by
`postinstall` to provide this behavior until the runner exposes the same R2 log
sink upstream. The pinned `@cloudflare/sandbox` 0.12.1 client is patched at the
same trusted install boundary: it derives a distinct control token for each
Sandbox Durable Object, passes it only to the root container server, and
authenticates every control request. The image pins and patches the exact
upstream server bytes so all process, file, backup, RPC, and WebSocket routes
except static health/version reads require that token. Repository commands run
as a dedicated unprivileged UID through an empty, explicit environment, cannot
read the root server environment, and are reaped UID-wide before log or
snapshot credentials can be used. Runner images also pin Node 22.15.0, both
Python interpreters, the Rust and MSRV toolchains, and every installed Cargo
utility; a floating package-manager runtime cannot silently change the gate.

For a local run, start OrbStack or another Docker-compatible engine, put the
development-only CI capabilities in the repository `.env`, and run
`npm run dev:ci` from this directory. The command rebuilds the development
Worker, explicitly enables both container-backed Durable Objects, and serves
the dashboard plus source API at `http://127.0.0.1:8787/ci`. The explicit
container opt-in is required because the normal visual-development loop keeps
containers disabled for startup speed.

Create both buckets, configure S3 API credentials scoped only to the backup
bucket, and set distinct master-source, PR-source, control, macOS-runner,
release, Git-mirror, and Sandbox-control tokens before the first deployment.
The values are capabilities and must not be reused across roles. The Sandbox
control secret must be exactly 32 random bytes encoded as 64 lowercase hex
characters; the client derives the per-Sandbox value without exposing this
root secret to repository code.

```bash
cd web
npx wrangler r2 bucket create nanocodex-ci
npx wrangler r2 bucket create nanocodex-ci-source
npx wrangler r2 bucket lifecycle add nanocodex-ci ci-backups backups/ --expire-days 31 --force
npx wrangler r2 bucket lifecycle add nanocodex-ci ci-cache cache/ --expire-days 31 --force
npx wrangler r2 bucket lifecycle add nanocodex-ci ci-artifacts artifacts/ --expire-days 90 --force
npx wrangler r2 bucket lifecycle add nanocodex-ci ci-runs runs/ --expire-days 90 --force
npx wrangler r2 bucket lifecycle list nanocodex-ci
npx wrangler secret put CI_MASTER_SOURCE_WRITE_TOKEN
npx wrangler secret put CI_PR_SOURCE_WRITE_TOKEN
npx wrangler secret put CI_CONTROL_TOKEN
npx wrangler secret put CI_MACOS_RUNNER_TOKEN
npx wrangler secret put CI_RELEASE_TOKEN
npx wrangler secret put NANOCODEX_SANDBOX_CONTROL_TOKEN
npx wrangler secret put GIT_MIRROR_TOKEN
npx wrangler secret put R2_ACCESS_KEY_ID
npx wrangler secret put R2_SECRET_ACCESS_KEY
npm run deploy
```

Sandbox TTLs are restore-time checks, not physical deletion. The four
lifecycle rules above are therefore required to bound backup, cache-pointer,
content-addressed artifact, and run-evidence storage. The CI repository keeps
100 run records and separately retires unreferenced source archives; the macOS
broker pages through attempt/job cleanup seven days after terminal completion.
Keep R2's default automatic abort of incomplete multipart uploads after seven
days (or configure a shorter bound). Durable multipart-create recovery waits
for that bound before replacing a create whose acknowledgement was lost.
Stable release objects intentionally have no expiration. Development
uses `nanocodex-ci-development` and `nanocodex-ci-source-development`; create
those buckets and configure separate `--env development` secrets rather than
reusing production credentials or cache state. Generate each Sandbox-control
value with `openssl rand -hex 32`; never reuse the production value in
development.

Distribution Workflow writes first use the separate `distribution-staging/`
namespace. The release ledger registers the exact bounded key set before a
runner writes, creates the complete draft before promoting any immutable final
key, and durably collects abandoned staging after seven days. Do not add an R2
lifecycle rule over `distribution/` or the public release namespaces; finalized
assets are intentionally permanent.

Retain the production `lifecycle list` output with the deployment evidence and
verify that no rule covers `distribution/`, `release-import/`, or public
release assets. A configuration file is not proof that the account accepted the
rules.

Install the trusted controllers as LaunchAgents from two distinct, dedicated
macOS login accounts. Exactly four local identities form the service boundary:
the master-controller login, the PR-controller login, the non-login
credential-empty PR preparation account, and the macOS-runner login. No two may
resolve to the same passwd identity. Each controller installer rejects the
other controller's loaded service, LaunchAgent, state directory, or role
Keychain metadata and also rejects the macOS runner's loaded service,
LaunchAgent, state directory, or runner Keychain metadata. The runner installer
performs the inverse checks against both controllers. These probes request only
existence/metadata, never token values, and fail closed on unreadable or
ambiguous `launchctl`, filesystem, or Keychain results. Remove stale artifacts
from the wrong account before installing; their presence is co-location, not a
migration request.

The PR controller additionally requires the separately provisioned non-login
preparation identity and rejects a `--prep-user` whose username or UID is the
current controller role. Its only entrypoint is the root-owned
`/Library/PrivilegedHelperTools/dev.nanocodex.ci-pr-cargo-builder` byte-for-byte
matching the trusted checkout, through the installer's exact
`NOPASSWD:NOSETENV` sudo rule. That helper invokes only the separately reviewed
Cargo 1.98.0 binary at
`/Library/PrivilegedHelperTools/dev.nanocodex.ci-cargo`; the PR controller pins
its exact SHA-256 on every install and update. The installer validates these
boundaries but never creates the account, Cargo binary, or sudoers entry. Its
generated root payload embeds the already-opened, reviewed builder bytes, so
root never follows a controller-owned checkout path. Run each controller
command as its target login user, with a fixed clean checkout and a canonical
Node binary outside that checkout. For PR, the selected Node executable is also
part of the sudo boundary: root provisions it as a singly linked,
root-owned/root-group global executable under a complete root-owned/root-group
ancestor chain. The file and every ancestor are real (no symlink component),
have no ACL, and are non-group/world-writable. A controller-owned Node or a
Homebrew convenience symlink is rejected. Install, update, startup, and status
pin and recheck its exact path, device/inode, size, SHA-256, owner, mode, link
count, and `node/darwin/<arch>` identity before and after the exact sudo probes,
so the preparation account cannot replace the authorized pathname:

```bash
# Dedicated master-controller account.
npm run ci:install-controller-service -- install \
  --role master \
  --origin https://nanocodex.me-7fb.workers.dev \
  --node /absolute/path/to/node \
  --repo /absolute/path/to/nanocodex \
  --rustsec-repo /absolute/path/to/advisory-db \
  --cloudflare-account-id 7fb82fc3b80331b2cd45f097acbd9ffc

# Different dedicated PR-controller account.
npm run ci:install-controller-service -- install \
  --role pr \
  --origin https://nanocodex.me-7fb.workers.dev \
  --node /root-provisioned/no-symlink/path/to/node \
  --repo /absolute/path/to/nanocodex \
  --prep-user nanocodex-ci-pr-prep \
  --cargo-sha256 <reviewed-cargo-1.98.0-sha256>
```

The installer prompts directly into per-role Keychain items; no token enters a
plist, argv, or file. Each controller plist names `/usr/bin/env` as its first
executable and passes `-i` before the wrapper path, so no shell instruction can
observe launchd's ambient environment; the wrapper repeats that empty boundary
before constructing its exact role allowlist. Master loads exactly the
source-publication, GitHub status, Cloudflare deploy, and Git-mirror
capabilities. PR loads only source publication and GitHub status. The
preparation account has no Keychain, login session, agent, runner token, shared
writable group, controller role, or access to either controller's state. Never
reuse either controller identity or the authenticated macOS runner account, and
never `chown` a PR checkout across this boundary. `status`, `update`, and
`uninstall` use the same command with `--role`;
PR install/update also supplies
`--prep-user` and `--cargo-sha256`; `update --replace-secrets` rotates the exact
allowlist. The prep account must be a local locked non-login account with a
unique same-name primary group, no shared or nested membership, a root-owned
`/var/empty` home boundary, and no authentication, admin, Keychain, or service
role. Its complete `LC_ALL=C sudo -n -l` output must contain only the builder's
`--probe` and `--build` commands plus `timestamp_timeout=0`; inherited or group
grants fail closed.
Process status is not proof of a successful reconciliation, so production
health also requires a recent retained master run and matching live deployment.
Before a credentialed child starts, the master controller runs the trusted
Node distribution's npm CLI with lifecycle scripts disabled, explicitly applies
the committed `@cloudflare/ci` patch, and fingerprints the complete installed
toolchain. It rechecks that fingerprint and authoritative `master` before every
publication or promotion boundary. If `master` advances during a long fetch,
CI wait, deploy, or verification, the old head receives no terminal status and
the controller immediately reconciles the replacement.

Install the Apple Silicon executor from a third dedicated arm64 macOS login
account (the fourth identity overall, because the PR preparation account is
non-login):

```bash
rustup toolchain install 1.98.0 --profile minimal
npm run ci:install-macos-service -- install \
  --origin https://nanocodex.me-7fb.workers.dev \
  --runner-id macos-arm64-1 \
  --node /absolute/path/to/node
```

Its Keychain contains only the value configured in the Worker as
`CI_MACOS_RUNNER_TOKEN`. Do not expose GitHub, SSH-agent, cloud, deploy, or
registry credentials to that account. Its plist names `/usr/bin/env` as the
first executable and passes `-i` plus only fixed `HOME`, `USER`, `LOGNAME`,
system `PATH`, private service `TMPDIR`, locale, and UID-derived macOS text
encoding values before the wrapper's shebang interpreter can start. The wrapper
rejects malformed or directly contaminated invocation, starts its
credential-free log monitor before reading Keychain, and validates the final
token-bearing Node environment as an exact allowlist. `NODE_OPTIONS`,
`NODE_PATH`, shell startup hooks, `DYLD_*`/`LD_*`, SSH, GitHub, cloud, release,
R2, and controller authority therefore never reach the monitor or runner Node.
The token remains in Keychain and the inherited environment only; it is never
placed in a plist, argv, or file. The runner resolves the exact
`1.98.0-aarch64-apple-darwin` toolchain by canonical path; a floating `stable`
alias cannot change macOS CI or release bytes. All three LaunchAgents require
their dedicated Aqua users to remain logged in.

The individual source publishers remain available as focused recovery tools.
The master controller builds a deterministic vendor bundle in a token-free
phase. The PR controller sends only the exact public PR/base/merge identity to
the fixed helper, which fetches and vendors under the preparation UID and
returns a bounded checksum-framed bundle. The controller rechecks GitHub, then
an upload-only process receives the opened frame plus source authority; that
process cannot invoke Git or Cargo. Objects are keyed by both the `Cargo.lock`
blob and reproduced bundle SHA-256, so a PR first-write can never become a
master cache choice.
To repair only an obsolete Cloudflare repository-publication shape after the
current Worker is already live at the exact authoritative head, stop the master
LaunchAgent and run `npm run ci:controller -- repair-repository <full-sha>` from
that service account. The same process lock proves local `HEAD`, GitHub master,
live Worker health, and the requested SHA before exposing only mirror authority;
it cannot deploy, run CI, or post a status.

### Authority cutover

Keep every GitHub Actions workflow active until one deployed commit has retained
proof of a green master wave, an exact two-parent PR merge wave, PR
supersession/close cancellation, the real macOS gate, immutable artifacts, and
the deploy-plus-repository attestation. Then make the single GitHub status
context `ci success` required on `master` and prove that a failing native gate
blocks a merge before retiring `ci.yml`.

Do not push a stable tag while the broad tag-triggered `release.yml` is active;
it can irreversibly race the reviewed Cloudflare registry publication. Disable
that trigger first and retain only the exact-byte compatibility bridge described
in `docs/RELEASING.md`. Keep the old nightly surface until an old nightly client
has crossed to the Cloudflare updater. The current CodeQL job scans only Actions
YAML, so keep it while any Actions workflow remains and remove it with the final
workflow rather than claiming that RustSec or `cargo deny` is an equivalent
Actions-language scan.

The former master/manual pkg-pr-new publication and automatic SHA comment are
intentionally retired. Pull requests resolve their immutable npm and native
artifacts through `/api/ci/pull-requests/<number>`; normal master artifacts stay
on their checksum-bound run endpoint.

`GET /api/ci/runs` and `GET /api/ci/runs/<40-hex-commit>` expose the retained
Workflow/result state. `GET /api/ci/badge.svg` renders the current head's status
directly from that ledger for the repository badge. Successful bindings,
website, npm, and native gates export tested artifacts directly to immutable,
checksum-verified R2. They are served below the run's `/artifacts/` path; this
is the owned replacement for a hosted artifact service. Step records and logs are available
under `GET /api/ci/runs/<commit>/steps/<step>/{result.json,stdout.log,stderr.log}`.
An authenticated `POST` request to
the latter path's `/terminate` action uses `CI_CONTROL_TOKEN`. Runs are not
restarted in place because commit-addressed evidence is immutable; publish a
new commit for a new run. The master publisher's `NANOCODEX_CI_TOKEN` must
contain the value configured as `CI_MASTER_SOURCE_WRITE_TOKEN`; the PR
publisher receives only `CI_PR_SOURCE_WRITE_TOKEN`. The two values are
deliberately non-compatible. Both roles may upload immutable public source and
Cargo-vendor objects, but only the master role can publish master or RustSec
state and only the PR role can mutate pull-request state. Archive URLs are
public and commit-addressed because this is a public source repository; write
authority and R2 backup credentials never enter a runner or checkout URL.

`GET /api/ci/pull-requests/<number>` resolves the current open PR's exact tested
merge, SHA-256-bound two-platform native manifest, and a separate npm preview
whose version is `0.0.0-preview-<tested-merge-sha>`. The normal tested
`npm-package.tgz` remains unchanged and is the only npm input accepted by stable
or nightly release staging. Stable and nightly release manifests and assets are
served under `/api/releases`; see `docs/RELEASING.md` for the staged
registry/publication boundary.

The pipeline covers the complete stable/MSRV Rust suites, rustfmt, warnings-
denied Clippy and rustdoc, dependency and crate-boundary policy, spelling,
static VM guest, Node/browser/WASM, Python 3.11/3.14, website, Linux native CLI,
and authenticated macOS workspace/native CLI gates. Every run pins the exact
RustSec revision and owned archive checksum, verifies the extracted Git
checkout, and runs `cargo deny --frozen check`. The scheduled nightly and
reviewed stable release paths consume only green master evidence. Production
promotion consumes the exact tested web artifact rather than rebuilding it.

Production serves the website indexes, immutable file and patch objects, and a
read-only Git protocol-v2 endpoint from that publication. Clone the mirror with
`git clone https://nanocodex.me-7fb.workers.dev/git`. After a green current
master run, the trusted controller checks authoritative master before and after
deployment and mirror publication, then proves `/api/health`, the public
repository snapshot, and protocol-v2 Git refs all attest the same commit before
posting success to GitHub.

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
command, in-place `nanocodex update`, the crates.io SDK entry point, the
Worker-owned `latest` manifest, and the conventional-commit changelog.

Navigation stays available whenever an input is not active: `H`, `T`, `C`, `R`,
and `E` switch between Home, Code, Commits, Requests, and Evals. The repository
homepage is the root route. In Code, `Ctrl+P` searches the left tree and `Ctrl+F` opens the
fuzzy all-file jumper. In Commits, `F` searches history. Code and commit
scrolling are left to Pierre CodeView and the browser's native input behavior.

## Production

The trusted master controller owns normal production promotion. It installs the
exact checksum-verified `web-dist.tar` from the green run, deploys that already-
built graph, publishes the repository mirror, and marks `ci success` only after
all live attestations match. Local commands remain useful for development:

```bash
npm run build
npm run preview
```

For a break-glass deployment, stop or disable the master LaunchAgent first,
start from a clean authoritative `master` commit, and preserve the same
deployment-before-mirror ordering:

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
