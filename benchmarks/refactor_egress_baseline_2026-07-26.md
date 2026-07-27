# VM egress baseline — 2026-07-26

This is the first reproducible baseline for the refactor's unified VM-egress
slice. It measures the actual authenticated proxy and origin path rather than
only route matching, and keeps direct loopback latency beside the proxy results
so host and scheduler noise remain visible.

## Environment

- MacBookPro18,2, Apple M1 Max, 32 GiB
- macOS 26.3.1 (25D771280a)
- rustc 1.97.1 (`8bab26f4f`, LLVM 22.1.6)
- Cargo `bench` profile, optimized, debug information disabled
- Criterion 0.7, 20 samples, one-second warm-up, two-second measurement
- one retained Axum loopback origin and retained Reqwest connection pools

The source worktree was based on `refactor/08-egress@db02cf4`; the
measurements include the uncommitted unified proxy recorded by this document.

## Results

Criterion's reported interval is shown verbatim. It is a confidence interval
for the estimate, not a percentile claim.

| Operation | Fixture | Estimate |
| --- | --- | --- |
| direct origin round trip | retained HTTP/1.1 loopback connection | 31.479–31.887 µs |
| authenticated MPP proxy round trip | ephemeral Basic capability, bounded body, host forwarding | 66.039–77.525 µs |
| dynamic policy and secret injection | policy read, route match, per-request resolution, host header injection, forwarding | 68.140–69.748 µs |

The complete dynamic authorization path remains tens of microseconds above a
direct in-process origin. It is below VM RPC, browser action, public-network,
and model latency, so it does not move the intended critical path away from
the model or useful tool work. `SecretSpec` is a validated type, so this path
does not revalidate immutable configuration and parses each matching origin
only once.

Reproduce with:

```sh
just bench-egress
```

## Regression gates

Machine-local numeric budgets apply to this M1 Max baseline. CI enforces the
structural and deterministic stress gates; numeric comparisons run on a pinned
benchmark host.

| Operation | Budget |
| --- | --- |
| direct origin round trip | ≤ 50 µs |
| authenticated MPP proxy round trip | ≤ 120 µs |
| dynamic policy and secret injection | ≤ 120 µs |

Hard structural gates:

- a guest receives one authenticated proxy capability and public CA, never a
  wallet, payment provider, secret manager, resolved secret, or policy store;
- policy authorizes identity, origin, method, and unambiguous path before
  secret resolution;
- configured-origin method or path misses fail closed even when unmatched
  ordinary egress is allowed;
- every authorized request resolves its source again, so rotation and
  revocation take effect without restarting the guest;
- MPP and secret delivery share one guest-visible front proxy and preserve one
  exact logical request ID across a paid replay;
- request bodies, connections, origin concurrency, redirects, and response
  streaming remain bounded;
- canceling a client request releases origin admission and cannot prevent
  bounded proxy shutdown;
- 10,000 bounded-parallel paid requests each commit exactly once, replay the
  byte-identical body once, and never roll back a successful payment;
- complete child-visible requests and MPP retries remain traceable in order,
  while the egress layer never records its resolved value or origin-facing
  injected header and never puts either in a lease, guest state, snapshot, or
  debug output; and
- explicit shutdown drains the proxy, while dropping its final guard stops it;
  both paths release every host capability.

Full-fidelity tracing still applies to origin responses. If an upstream echoes
a credential, the agent observes it and the response is intentionally traced;
the egress boundary does not redact observed data.

## Live proof and feature parity

The retained `vm-tools` proof boots the current libkrun guest, exercises
`exec_command`, `write_stdin`, `apply_patch`, and `view_image`, calls a scoped
secret origin, and completes an MPP `402` payment/replay through one
`VmEgress`. The exact host secret is observed by the authorized origin but is
absent from guest output and retained guest state; the paid origin is contacted
exactly twice and the provider exactly once.

The headed browser proof uses the same lease through gvproxy. Chromium answers
only proxy authentication challenges, renders the secret-authorized HTTP page,
trusts the ephemeral interception CA for a real HTTPS navigation, captures a
semantic snapshot and PNG, and shuts Chromium, gvproxy, the VMM, and proxy down
together.

An ignored public-network smoke additionally sends a scoped HTTPS request
through CONNECT interception and verifies that Postman Echo observes the
host-injected `x-nanocodex-proof` header. This separately proves HTTPS secret
route matching rather than inferring it from HTTP injection plus HTTPS trust.

Master's direct `MppEgress` API remains available. The former
`nanocodex_vm::mpp_egress_layer` path is reexported from the new owner for
source compatibility, while `nanocodex-vm-egress` adds the unified builder.
File, environment, composite, 1Password Connect, and optional 1Password SDK
secret providers retain their behavior behind the standalone `SecretManager`
contract.
