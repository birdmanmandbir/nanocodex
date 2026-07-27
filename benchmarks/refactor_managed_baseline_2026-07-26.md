# Managed API baseline — 2026-07-26

This baseline isolates Nanocentaur's managed-service overhead from model and
public-network latency. The fixture uses the real Axum router, API-key
authentication, file-backed policy and session SQLite databases, actor
channels, durable events, and JSON projection. A zero-delay deterministic
agent supplies the terminal result without hiding any managed boundary.

## Environment

- MacBookPro18,2, Apple M1 Max, 32 GiB
- macOS 26.3.1 (25D771280a)
- rustc 1.97.1 (`8bab26f4f`, LLVM 22.1.6)
- Cargo `bench` profile, optimized with incremental compilation disabled
- Criterion 0.7, 20 samples, one-second warm-up, two-second measurement
- source at `refactor/10-managed@26fb885` plus this working slice

## Results

Criterion's reported confidence interval is shown verbatim. It is an estimate
interval, not a percentile.

| Operation | Complete measured boundary | Estimate |
| --- | --- | ---: |
| authorized agent view | HTTP parsing, key digest lookup, policy SQLite, actor request/reply, JSON response | 54.629–56.703 µs |
| idempotent turn replay | HTTP/JSON, auth, durable-key lookup, existing typed receipt response | 46.750–47.698 µs |
| accepted turn to durable terminal event | HTTP/JSON, auth, policy, session writes, actor/runtime execution, terminal usage/event commit, live cursor delivery | 2.4662–2.8532 ms |

The terminal measurement waits on the public durable event cursor rather than
busy-polling `GET /turn`, so it measures the owned lifecycle instead of a
client polling strategy. Even the complete mutation path remains below 3 ms on
this host. Normal turns therefore remain model/network latency bound.

Reproduce with:

```sh
just bench-managed
```

## Regression gates

Machine-local numeric budgets apply to this M1 Max baseline. CI retains the
deterministic contract tests; numeric comparison belongs on a pinned benchmark
host.

| Operation | Budget |
| --- | ---: |
| authorized agent view | ≤ 120 µs |
| idempotent turn replay | ≤ 100 µs |
| accepted turn to durable terminal event | ≤ 5 ms |

Structural gates:

- successful prompt acceptance and terminalization each cross one bounded
  agent command channel and persist through the single session-store owner;
- the idempotency key returns the original typed action without authorizing a
  second payment, running the backend again, or appending another event;
- completion is visible only after typed output, usage, optional USD cost,
  snapshot, and the terminal event commit together;
- restart recovery reads committed SQLite state and never commits a partial
  model response;
- HTTP, agent-command, and SQLite work use bounded root/child spans with the
  explicit caller subscriber and parent propagated across channels; and
- full-content tracing observes the normal request and lifecycle path without
  an additional configuration or data read.

Generated Criterion artifacts under `target/criterion` are intentionally not
committed.
