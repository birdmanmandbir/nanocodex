# Agent harness performance

`harness_performance` measures the part of latency and memory that Nanocodex
owns: runtime construction, agent startup, prompt admission, typed event
delivery, retained history, historical forks, and prompt-cache-preserving
request construction. It deliberately removes provider, network, sandbox, VM,
and tool-process variance.

This is the baseline beneath host adapters. A Cloudflare Worker, Vercel
Workflow, native CLI, or VM-backed consumer may add its own cold-start and
binding cost, but it should not redefine these SDK metrics or force a provider
abstraction into the library.

## Workload and boundary

The default release-profile workload runs in one process on one Tokio worker:

- one in-process scripted Tower service with a single scheduled yield before
  the first `AssistantDelta`;
- no socket, DNS, provider, sandbox, VM, or tool subprocess;
- an empty tool registry, a 32-KiB stable prefix, 32 sequential 4-KiB prompts,
  and historical checkpoints at turns 8, 16, and 32;
- 128 retained historical forks, three cache-probe forks, and 50 fresh-agent
  startup samples; and
- typed JSON output containing distributions and every audited request shape.

Compilation is outside the timed process. RSS is process resident memory from
the host, so comparisons require the same OS, architecture, allocator, build
profile, and workload.

## Metrics

| Metric | Starts | Ends | What it isolates |
| --- | --- | --- | --- |
| Runtime startup | process `main` / runtime build | Tokio runtime ready | executable and async-runtime startup |
| Agent build | immediately before `Nanocodex::builder(...).build()` | driver handle and event stream returned | owned driver/channel/session construction |
| Build-to-first-delta | fresh agent build start | typed `AssistantDelta` received | fresh agent plus prompt admission and event path |
| Warm TTFT | `prompt()` submission | typed `AssistantDelta` received | reused-driver scheduling and event delivery |
| Event delivery | service emits the delta | benchmark receives it | channel/event-envelope overhead |
| Fork construction | before `fork_from(checkpoint)` | fork handle and stream returned | checkpoint/history sharing plus child-driver construction |
| Retained-fork memory | RSS after history | RSS with all forks retained | amortized resident footprint of live forks |
| Cache eligibility | every scripted request | audited request ledger | stable key/prefix, warmup reuse, incremental chaining, and request bytes |

TTFT here is not model TTFT: the scripted service has no inference. It is the
minimum client-side tax that live provider TTFT sits on top of.

## Run it

```sh
just bench-harness
```

To retain the report explicitly:

```sh
cargo bench -p nanocodex-agent --bench harness_performance -- \
  --source-commit "$(git rev-parse HEAD)" \
  --output /tmp/nanocodex-harness-performance.json
```

The benchmark also accepts `--history-turns`, `--prompt-bytes`,
`--prefix-bytes`, `--retained-forks`, `--cache-probe-forks`, and
`--startup-samples`. Keep the default workload for comparisons; change one
dimension at a time for diagnosis.

The process exits unsuccessfully if the cache key changes, a fork unexpectedly
warms a replacement service, or a generation stops using the expected
incremental request path.

## Reference baseline

Seven consecutive post-build process launches on 2026-08-22 used
macOS/aarch64 and source
`094476b4dbb8c36f903688586e6f1f8b182af2db`. The release binary and filesystem
page cache were warm. Each row summarizes the seven per-run p50 values; these
numbers are a reference shape, not a cross-host regression threshold:

| Measurement | Median run p50 | Observed run-p50 range |
| --- | ---: | ---: |
| Process `main` to runtime ready | 132 µs | 126-148 µs |
| Tokio runtime build | 59 µs | 53-66 µs |
| Fresh agent build | 18.0 µs | 17.7-18.9 µs |
| Fresh build-to-first-delta | 275 µs | 269-276 µs |
| Warm submit-to-first-delta | 73.6 µs | 67.3-77.4 µs |
| Delta emission-to-receipt | 6.6 µs | 6.5-6.8 µs |
| Historical fork construction | 211 µs | 209-213 µs |
| Retained RSS per fork | 85.9 KiB | 85.0-86.5 KiB |
| 128-fork retained RSS delta | 10.73 MiB | 10.63-10.81 MiB |

Every run observed one stable prompt-cache key, one shared warmup across four
service instances, three avoided fork warmups, 35/35 incremental generation
requests, and no full-history generation replay. The 32-turn history plus 128
retained forks ended at a median 22.25 MiB process RSS.

The startup clock begins inside process `main`; executable loading before
`main` is a host/package measurement. Warm page-cache results should not be
presented as cold package launch time. At this scale, scheduler placement still
moves agent-build and fork distributions even without provider noise. Compare
repeated idle-host runs and inspect p50, p95, and maxima rather than treating
one sample as a product claim.

## Prompt-cache interpretation

The deterministic harness proves cache eligibility, not provider cache hits.
It verifies the stable cache key and byte-stable prefix, records request hashes
and sizes, checks response-ID chaining, and detects redundant warmups. It does
not fabricate `cached_tokens` or a cache-hit percentage because only the
provider can report those.

Use the live [`RESPONSE_TRANSPORT_BENCH.md`](RESPONSE_TRANSPORT_BENCH.md) suite
for cached-input tokens, network TTFT, WebSocket/HTTPS setup, and server-side
checkpoint behavior. Keep the two result sets separate:

1. this harness attributes SDK startup, scheduling, memory, and fork costs;
2. a host-adapter benchmark adds Worker/Workflow/WASM/native startup and memory;
3. a live provider benchmark adds network, queueing, inference, and actual
   prompt-cache usage.

That layering makes regressions actionable and avoids solving provider or
sandbox variance with adapters in the core agent contract.
