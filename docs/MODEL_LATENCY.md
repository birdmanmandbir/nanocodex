# Model-latency scorecard

Nanocodex has one performance objective: minimize latency from an accepted user
prompt to the first nonempty semantic model output. A faster result is eligible
only when the complete agent contract remains intact. Startup time, binary
size, RSS, and total completion time are useful diagnostics, but they do not
enter the score.

## Scored boundary

The live cross-product score is provider TTFT:

```text
full upstream request flushed -> first nonempty semantic SSE delta received
```

Semantic output includes answer text, refusal text, reasoning text, and
reasoning-summary text. Empty deltas, lifecycle frames, and output-item shells
do not stop the clock. First answer-text delta is recorded separately so a
reasoning-first response cannot be mistaken for user-visible answer latency.
For Nanocodex alone, the report also records prompt submission and acceptance
through the first typed `AssistantDelta`, plus the transport-to-typed-event tax.

`response.completed` latency is retained for diagnosis only. It is not blended
with TTFT, and local startup or memory measurements never compensate for a
slower model result.

## Correctness eligibility

A latency sample is rejected unless all relevant behavior is preserved:

- fixed `gpt-5.6-sol`, low reasoning effort, non-fast service, and no priority
  tier;
- the exact benchmark instructions, prompt, and answer;
- the real nonempty production tool declarations, with no tool selected by the
  model for the one-generation workload;
- one completed model call, contiguous typed events, and exactly one terminal
  run event;
- stable prompt/cache identity and the expected incremental history shape;
- historical-fork boundary preservation, cancellation recovery, and joined
  shutdown in the browser adapter, plus the repository's exact typed replay
  regressions at the owning native boundary; and
- no leaked socket, task, secret, raw authorization header, or credential in a
  retained report.

The deterministic harness records exact default-system-prompt and default-tool
wire footprints. The Chromium suite independently gates inline-WASM and Worker
event order, follow-on history, forks, cancellation, recovery, and socket
cleanup. Removing instructions, tools, events, history, retries, or cleanup to
win a timing row therefore makes the row ineligible.

## Three measurement lanes

| Lane | Clock | Purpose |
| --- | --- | --- |
| Native deterministic | prompt submission to scripted typed delta | isolates Rust scheduling, request construction, and event projection without provider noise |
| Real Chromium loopback | browser prompt submission to typed `assistant.delta` | measures inline-WASM and package-Worker client tax with raw browser/server timestamps |
| Paid live comparison | upstream request flush to first semantic delta | scores actual model latency for Nanocodex and pinned fx on the fixed official route; first-answer and terminal timing remain diagnostics |

The first two lanes are regression evidence, not claims about provider speed.
The Chromium report records the repository commit/tree, dirty state, and
SHA-256 hashes of the JavaScript and WebAssembly responses actually served by
its owned Vite process. The native harness's `source_commit` remains explicitly
caller-declared. Chromium report encoding and publication happen only after the
scripted WebSocket listener has closed and verified that no clients remain.

Only the live lane can establish an fx comparison. Nanocodex's native persistent
WebSocket path remains a separate product diagnostic; fx currently buffers
`ask --json`, so the paired score holds both clients to the common HTTPS
Responses route and measures them at the shared streaming observer.

## Controlled fx protocol

The comparison is deliberately product-as-shipped, not an identical-request or
isolated-client microbenchmark. It pins fx source commit
`4a8f765c94f4e205ecae293d6d5c98ec9aef2200`, requires clean source checkouts,
requires each supplied executable to resolve under its checkout, then runs a
private mode-`0500` copy whose SHA-256 is rechecked before every child. The
executable digest is authoritative; checkout proximity is not a build
attestation.

Each executable uses its shipped request builder and default tool surface. The
comparison deliberately holds transport to the common HTTPS Responses route;
it does not claim to compare either product's default connection startup.

Before real credential contents are loaded or a provider request can be
forwarded, the runner executes the exact immutable binary copies twice each
against a scripted loopback SSE provider with unmistakably fake credentials.
It then executes Nanocodex twice more with a fake ID token whose nested boolean
FedRAMP claim is true. In both phases the fake bearer access token deliberately
carries the opposite claim, so the emitted header proves that policy follows
the ID token exclusively. These six provider-free runs must succeed from
distinct private workspaces, including a Nanocodex workspace whose path
contains every XML metacharacter, use fresh distinct UUIDv7 identities after
normalizing session and `msg_` wire namespaces to their underlying UUIDs,
produce stable canonical request fingerprints, and prove that only the
ID-claim-bearing runs emit the FedRAMP header. The full recursive bodies are
also checked against independently reviewed fixed hashes for the pinned fx
ReleaseSafe binary and the reviewed Nanocodex reference, so a candidate cannot
establish its own weaker contract. This covers every tool
description/schema/format, ordered context item, shipped fx runtime paragraph,
and Nanocodex Code Mode metadata entry. Every live request must pass both the
fixed contract and the actual-binary stability fingerprint before it can
consume provider budget.

Every run uses:

- one frozen ChatGPT/Codex auth snapshot in private isolated homes; upstream
  `401` is translated to a non-`401` child failure so neither client can refresh;
- the exact official Codex Responses HTTPS route, shared auth identity, model,
  low reasoning effort, standard service tier, benchmark instruction, final
  user input, and byte-exact ACK answer;
- fresh clients with no history or `previous_response_id`, complete
  implementation-specific default tool surfaces, and a fixed Nanocodex prompt
  cache key for its identical prefix;
- a pinned loopback fx model catalog served only for fx's exact
  `/models?client_version=0.148.0` discovery request, with the exact reviewed
  auth/product header set, exactly once before each fx generation, and
  explicitly advertising low effort;
- validated request bodies, exact product user agents, fresh distinct UUIDv7
  Nanocodex request identities with no cross-namespace or cross-run reuse, and
  one run-scoped freshness ledger shared by ordinary preflight, FedRAMP
  preflight, warmup, and measured execution. Requests must be defect-free
  HTTP/1.1 with exact origin-form request targets and exact
  per-product/per-endpoint header sets and values. Absolute-form,
  authority-bearing, query-bearing, and fragment-bearing generation or refresh
  targets are rejected before claim. Header validation includes the observer
  `Host`, untrimmed credential values, byte-exact media types, fx's
  endpoint-specific `Connection` header, Nanocodex's absence of `Connection`,
  and the reviewed absence of `Accept-Encoding`; every nonempty parser failure,
  including oversized request lines or headers, is counted exactly once before
  claim or budget reservation. Valid bodies are then forwarded raw without
  JSON reserialization or response buffering;
- a Darwin Seatbelt policy that permits only IPv4 TCP to one unique host-local
  port; the observer reserves that port on every IPv4 interface and accepts a
  request only when its local destination is `127.0.0.1`. IPv6, UDP, and
  child-process creation remain denied while threads are preserved; the parent
  observer alone can reach the fixed official HTTPS endpoint, and the runner
  refuses live execution when this combined boundary is unavailable;
- selector-owned bounded child stdout/stderr, explicit process-group
  termination and reap verification that retries transient group-signal and
  wait failures through one deadline, and tracked downstream handlers/upstream
  sockets that are owned from creation through TCP, TLS, and response handoff,
  interrupted, and drained to bounded shutdown deadlines before a successful
  report can be returned or published. Official DNS resolution runs once in a
  bounded, supervised helper before the observer starts, and every
  resolved-address TCP-plus-TLS attempt shares one deadline shorter than the
  shutdown budget;
- fail-closed SSE delivery: call-shaped or unknown output items, including
  `tool_search_call`, are withheld before the child can execute them;
- two discarded warmup pairs, then at least 20 even-count measured pairs with
  AB/BA order continuing across the warmup boundary; and
- retained request byte counts/hashes, stable product-specific fingerprints,
  tool-name hashes, provider timings, cached-input tokens, status, and failure
  codes, but never raw bodies, headers, client output, or credentials.

The primary summary is explicitly warm: both products must be functionally
valid, select no tool, have the scored TTFT, and report known positive
cached-input counts. First-answer and terminal timings are nullable diagnostics
and never gate the score. Absolute cache counts may differ because removing
either product's real tools would invalidate the comparison. Exact cache-count
equality is retained as a stricter sensitivity diagnostic; unknown cache
telemetry is never considered equal. At least 20 warm-primary pairs are
required.

Percentiles use nearest rank. Paired delta is `nanocodex - fx`, so a negative
value favors Nanocodex; differences within one millisecond are descriptive
ties, and win counts are explicitly non-inferential. The runner stops on the
first functional failure and exits nonzero for insufficient primary samples,
changed source/binary provenance, an auth mismatch, refresh attempt, unstable
request fingerprint, unexpected request, or incomplete schedule.
The retained document exposes only `time_to_first_model_output` under
`score.metric` as the machine-readable score; answer and terminal distributions
are labeled diagnostics. Any nonzero benchmark result is kept in memory for a
safe failure summary only and is never written to the requested report path.
Successful publication is one atomic, no-clobber hard-link commit from a
private mode-`0600` fsynced temporary file. Pre-commit I/O failures leave no
report; post-commit temporary-name cleanup or stdout notification failures do
not turn the published success into a failed exit.

## Run the local gates

```sh
just bench-model-latency-local
```

This runs the native harness and rebuilds the WASM package before exercising
inline and Worker paths in real Chromium. Reports can be retained explicitly
with the environment variables documented by the individual commands.

Before a live run, exercise the exact operator-supplied artifacts through the
source-controlled six-request provider-free integration gate:

```sh
just test-model-latency-actual-binaries \
  /absolute/path/to/pinned/fx-checkout \
  /absolute/path/to/pinned/fx-checkout/zig-out/bin/fx
```

The portable unit suite does not discover this test automatically because the
pinned external fx checkout is not a repository dependency. The live runner
nevertheless repeats the same immutable-copy preflight mandatorily before it
loads real credential contents.

## Run the paid live comparison

Build both pinned executables first. Nanocodex's narrow consumer package is:

```sh
cargo build --release -p nanocodex-model-latency-bench --bin model-latency-bench
```

On macOS, create the output directory, then invoke:

```sh
python3 benchmarks/paired_fx_model_latency.py \
  --fx-source-root /absolute/path/to/pinned/fx-checkout \
  --fx-bin /absolute/path/to/pinned/fx \
  --nanocodex-source-root "$PWD" \
  --nanocodex-bin "$PWD/target/release/model-latency-bench" \
  --nanocodex-commit "$(git rev-parse HEAD)" \
  --auth-file "$HOME/.codex/auth.json" \
  --warmup-pairs 2 \
  --trials 20 \
  --output "$PWD/.nanocodex/benchmarks/paired-fx-model-latency.json" \
  --confirm-paid-live-run
```

`--confirm-paid-live-run` is mandatory and authorizes real paid provider
requests. Without it the runner refuses before reading credential contents or
making a request. Even with confirmation, the actual-binary offline preflight,
independent request-contract verification, source checks, and egress-boundary
checks happen before the credential is loaded. The default schedule permits at
most 44 live
forwarded provider requests; the six scripted preflight requests are local and
free. The output path must not already exist. No live provider comparison was
executed while adding this benchmark; do not claim that Nanocodex beats fx until
a retained warm-primary run says so.
