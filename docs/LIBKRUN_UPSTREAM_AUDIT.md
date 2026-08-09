# libkrun upstream audit for confidential VMs

## Checkpoint and decision

Nanocodex `master` pins `containers/libkrun` `2.0.0-dev` revision
`df85b8b75f55e8ef1b06b5bc18f08dc6d7b5aeb0`; this implementation branch pins
the reviewed `b71a880` backport described below. This audit compares every
upstream commit after the base through `main` revision
`07fd40dcf6da8e14dd47e16a535531f0383fe52c`, inspected on 2026-08-09.

Do not advance the Nanocodex pin wholesale yet. The range contains useful
Nitro and device fixes, but it also replaces libkrun's init configuration path
and removes the non-Nitro `krun_set_exec`, environment, workdir, and rlimit
APIs used by the current audited VMM boundary. Port that new init contract as a
separate vertical slice with ordinary-VM parity before changing the pin.

The TEE JSON files still contain `workload_id`, `tee_data`, and
`attestation_url`, but upstream code only deserializes and stores those fields.
No current guest-init or VMM path consumes them for remote attestation. CPU
count, RAM, and TEE selection are the only effective inputs, and the file path
silently overrides an earlier `krun_set_vm_config` call. Treat the JSON
attestation fields as stale configuration, not a security mechanism.

The first focused upstream patch is
[`gakonst/libkrun@2b86bbf`](https://github.com/gakonst/libkrun/commit/2b86bbfda26361592f2651cac529da8427c52eeb):
it adds typed `krun_set_tee_type` selection for SNP and TDX while preserving
the legacy JSON API. Both variants pass warnings-denied Clippy and focused C
API tests on `dev-georgios`. It intentionally does not claim or implement
guest attestation.

Nanocodex uses the equivalent reviewed backport
[`gakonst/libkrun@b71a880`](https://github.com/gakonst/libkrun/commit/b71a880a44d66c55c4e1fb1b37aece4affa313b9)
on top of the existing `df85b8b` checkpoint. This keeps the current ordinary
VM/init contract byte-compatible while the upstream init migration is
evaluated separately.

## Commit classification

| Commit | Upstream change | Nanocodex classification |
| --- | --- | --- |
| `e3072c5` | Match assisted/generated trailers case-insensitively in CI | Out of scope: upstream CI policy. |
| `574cc39` | Ban Cursor co-author trailers | Out of scope: upstream CI policy. |
| `1c6208e` | Add `aws-nitro,net` Clippy coverage | Adopt with a future pin; useful feature-matrix gate. |
| `4e6c7e5` | Exclude AWS Nitro from a virtiofs-only import | Adopt with the Nitro slice; required compile fix. |
| `ec5a3ca` | Fix Nitro rootfs `Option` handling | Adopt with the Nitro slice; supersedes part of the original repair plan. |
| `b034fe8` | Remove a needless Nitro return | Out of scope: mechanical cleanup. |
| `5e3e476` | Remove Gemini CI configuration | Out of scope: upstream CI policy. |
| `c43eb9b` | Accumulate display damage regions | Defer: ordinary display correctness, unrelated to confidential assignment. |
| `a7bfb4d` | Reject invalid virtio-gpu blob-map offsets | Adopt before any GPU-enabled artifact; security-relevant even though virtio-gpu is not confidential GPU assignment. |
| `8a0dc11` | Upgrade `rand` to 0.10 | Adopt with the selected upstream checkpoint after compatibility validation. |
| `b042918` | Enforce static linking for guest init | Adopt: measured guests need a closed init dependency set. |
| `3eec435` | Add a musl target for static init checks | Adopt as build evidence for the measured guest. |
| `0225a9f` | Fix test-runner argument quoting | Adopt with upstream test tooling. |
| `60e8991` | Add glob and negation support to the test runner | Defer: useful test ergonomics, not a runtime dependency. |
| `ff1f16f` | Add `LIBKRUNFW_SRC` test-build selection | Adopt: required to test exact SNP/TDX firmware inputs reproducibly. |
| `7dd02fc` | Replace the test runner's `all` special case | Defer: test-tool cleanup. |
| `c652b56` | Update CODEOWNERS | Out of scope. |
| `454157a` | Move init and init-blob crates under `init/` | Evaluate as part of the init-contract migration; path-only by itself. |
| `7f7bc09` | Add ffier-based init configuration | Evaluate/port: replaces the command transport owned by the current Nanocodex VMM boundary. |
| `36b32ea` | Port tests to `Config::apply()` | Evaluate with the init migration and use as parity evidence. |
| `e3deaba` | Port examples to `Config::apply()` | Evaluate with the init migration and compile as consumer evidence. |
| `f1af859` | Add `krun_append_kernel_cmdline` | Adopt if the measured guest needs explicit boot inputs; hash every appended value into the launch manifest. |
| `3328d7c` | Load init config before block-root pivot | Adopt: necessary for explicit block roots and measured init configuration. |
| `502116e` | Remove implicit init injection | Evaluate/port: materially changes how the guest attester and workload enter the measured root. |
| `4d2201e` | Remove exec/env/workdir/rlimits APIs outside Nitro | Evaluate/port before repinning; directly breaks Nanocodex's current `KrunVm::run` boundary. |
| `07fd40d` | Verify generated init-blob bindings in CI | Adopt with the ffier init migration. |

## Required follow-up

1. Reproduce the ordinary Nanocodex VM lifecycle against `07fd40d` using the
   new init configuration, including exact argv/environment preservation,
   private launch records, cancellation, and process cleanup.
2. Decide whether to backport the typed TEE API to the pinned revision for the
   first SNP artifact or advance the pin after ordinary-VM parity. Do not carry
   two long-lived libkrun behavior forks.
3. Remove the dead attestation-shaped JSON fields in a compatibility-conscious
   upstream patch after typed TEE selection lands.
4. Add explicit SNP launch policy and host-data/measurement inputs; typed TEE
   selection alone is not an attestation interface.
5. Implement the measured guest attester and native report channel before
   Nanocodex can mark `MeasuredGuestAttester` available.
6. Build the exact SEV and TDX libkrun/libkrunfw pairs and retain their revision
   and firmware identities in the measured-workload manifest.

Upstream `amd-sev` and `tdx` warnings-denied Clippy pass on the Ubuntu host.
Focused typed-API tests pass for both variants. The broader upstream
`krun-vmm` TEE unit-test target does not currently compile because existing
test helpers still call non-TEE `setup_vm` and vCPU constructors; that is an
upstream baseline failure and must be repaired independently rather than
hidden in the typed API patch.
