# Cloudflare CI baseline — 2026-08-22

This baseline covers the GitHub-free CI implementation in `web/`: immutable
source publication to R2, a Cloudflare Workflow, Sandbox Containers, the
Worker-owned run ledger and logs, content-addressed snapshots, and deployable
artifacts served by the Worker. The local runner was Wrangler on OrbStack with
12 host CPUs and 16 GiB of memory; Cloudflare's amd64 Sandbox image ran under
local emulation.

## End-to-end evidence

The first cache-seeding run, `f9c3f0a2`, exercised the complete matrix for
974.980 seconds. Its previously unproven long gates completed in real
Containers: quality in 365.333 seconds, stable workspace tests in 176.276
seconds, MSRV workspace tests in 219.741 seconds, website dependencies in
119.804 seconds, the website in 91.742 seconds, and Python 3.11 in 208.428
seconds. Python 3.14 alone failed its unchanged 100 ms import SLA, which led to
the package import optimization rather than a relaxed verifier.

The first fully green run, `962f94f7d372208fe1449944d40b5854673b703f`,
completed in 236.282 seconds. Both cold Python wheels built concurrently:
Python 3.11 passed in 208.334 seconds and Python 3.14 passed in 213.000 seconds.
All Rust correctness gates, dependency policy, the VM guest, bindings, website,
and both Python versions passed. The non-Python deterministic gates restored
successful results from their exact input closures.

The Python package now defers its 12.7 MB native module and runtime-only
`TypedDict` definitions until those APIs are accessed. It still tests the
installed release wheel and leaves the committed verifier unchanged. On the
green run, fresh import p50/p95 fell to 72.899/73.655 ms on Python 3.11 and
81.964/83.513 ms on Python 3.14. The same two imports measured 79.372 and
94.452 ms when run simultaneously in the exact local CI image before the full
wheel rebuild.

## Cache and artifact evidence

Local restore no longer sends large squashfs snapshots through a long-lived RPC
stream. Containers pull run-scoped, checksum-verified HTTP ranges in 64 MiB
pieces. A 2.3 GB Rust snapshot restored in 22.866 seconds and a 417 MB website
dependency snapshot restored in 5.723 seconds. Production retains the native
mounted-R2 restore path.

The green run produced and then served these immutable artifacts:

| Artifact | Bytes | SHA-256 | Tar entries |
| --- | ---: | --- | ---: |
| `web-wasm.tar` | 3,676,160 | `7b1866d61d9ada56dfb108c8e6fe0da945beda53e7b741d3358d72cbf49e2d2c` | 9 |
| `web-dist.tar` | 25,640,960 | `789228ea2d8787b7d5a5978a52dd696303fa8fc0095b7f954d1ae3c5ddafc077` | 446 |

Both files were downloaded again through their public Worker routes. Their byte
counts and locally computed hashes matched the ledger and
`x-nanocodex-sha256` response headers.

## Local validation

- A clean `npm ci` reapplied the maintained `@cloudflare/ci` patch.
- TypeScript and documentation checks passed.
- Twenty-nine focused CI surface, routing, theme, lifecycle, and local transport
  tests passed.
- The dashboard uses the shared current `--surface`, `--text`, and responsive
  theme tokens, preserves the last complete state while polling, and renders no
  transient loading UI.
