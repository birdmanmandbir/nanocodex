# Attestable confidential VMs on libkrun

## Status and objective

This document is the implementation plan for making `nanocodex-vm` an owned
library boundary for launching and attesting confidential VMs through libkrun.
It is intentionally about the VM and its attached confidential devices. Agent
scheduling, fleet placement, model behavior, and proof that an arbitrary
command executed are separate consumers and are not part of this program.

The outcome is a caller-selected confidential-VM profile which:

1. fails before launch when the local host, libkrun artifact, firmware, or
   assigned devices cannot satisfy the profile;
2. launches the requested libkrun TEE variant without silently falling back to
   a normal VM or a weaker TEE;
3. produces fresh native hardware evidence bound to a caller challenge, an
   ephemeral guest key, and the intended measured workload;
4. verifies that evidence against explicit endorsements, reference values,
   revocation state, and caller policy; and
5. returns an attested VM capability only after verification succeeds.

Nanocodex will implement missing libkrun functionality and submit focused
changes upstream when libkrun does not yet expose the required launch,
measurement, attestation, or confidential-device behavior.

## Host behavior and caller policy

The host determines what is physically possible; it does not choose the
security policy.

- An AMD SEV-SNP host may satisfy an SNP profile.
- An Intel TDX host may satisfy a TDX profile.
- A supported EC2 host with `/dev/nitro_enclaves` may satisfy a Nitro profile.
- A confidential GPU profile additionally requires a supported CPU TEE,
  assignable confidential GPU, protected CPU-to-device path, matching guest
  driver, firmware, and device-attestation stack.
- A generic KVM or HVF host can run the existing ordinary VM profile but can
  never satisfy a confidential profile.

The caller requests one exact profile. Local discovery returns a typed report
explaining whether that profile is available. Launch rejects every missing or
weaker requirement. There is no `best available`, automatic downgrade, fleet
scheduler, host ranking, or remote placement API in this plan.

Attestation remains authoritative after discovery. A capability check can show
that the host appears able to launch SNP; only fresh verified evidence can show
that this particular VM launched with the accepted hardware, TCB, policy,
measurements, and devices.

## Security claims

The first release must make only these claims, qualified by the selected TEE:

- the evidence chains to an accepted hardware or vendor root;
- the evidence is fresh for the caller's challenge;
- debug, migration, SMT, and other security-relevant policy fields match the
  caller's appraisal policy;
- the initial measured VM state matches accepted reference values;
- an ephemeral public key generated inside the guest is bound to the evidence;
- every required confidential device has separately valid evidence and a
  verified binding to the parent VM; and
- no secret or confidential writable state is released before appraisal.

The initial measured state includes every executable component needed to trust
the in-guest attester: firmware, kernel, initrd, kernel command line, attester,
and a cryptographic identity for the root filesystem. Because attached block
disks are not automatically launch-measured, immutable roots use dm-verity (or
an equivalent measured authenticated root) whose root digest is included in
measured boot state. Writable confidential disks remain encrypted until a key
is released to an attested guest.

This program does not claim protection from denial of service, all side
channels, compromised accepted guest software, malicious workload semantics,
or rollback of state that is not tied to an external freshness authority.

## Explicit non-goals

- Proving that an arbitrary command executed or that an output is
  mathematically correct. A later measured execution supervisor may sign
  receipts with the attested guest key, but that is a distinct protocol.
- Running an agent inside the VM. Agents are one possible consumer after the
  confidential-VM contract exists.
- Fleet scheduling, cloud instance creation, placement, bidding, leasing,
  tenancy, or host admission.
- A lowest-common-denominator boolean such as `trusted: true` that discards
  vendor claims or differences in assurance.
- Silent use of TPM measured boot, a normal VM, or GPU device attestation as a
  substitute for a requested confidential VM.
- Moving attestation or VM policy into the stable agent, OpenAI, or tools
  crates.

## Evidence and appraisal model

The implementation follows the IETF RATS separation between Evidence,
Endorsements, Reference Values, Appraisal Policy, Attestation Results, and the
Relying Party. EAT and RATS conceptual-message wrappers are preferred for
interchange, but native evidence is always retained byte-for-byte.

The core types belong in the experimental `nanocodex-vm` crate initially:

- `ConfidentialVmProfile`: exact requested TEE, resource, policy, device, and
  evidence requirements;
- `ConfidentialHostReport`: detected build, firmware, kernel, hardware,
  quoting, and device capabilities plus typed rejection reasons;
- `AttestationChallenge`: nonce, protocol version, policy identity, expiry,
  and optional relying-party context;
- `RawEvidence`: owned bytes, registered media type, evidence profile, and
  producer identity;
- `AttestedComponent`: CPU VM, enclave, GPU, switch, firmware component, or
  measured guest component;
- `EvidenceBinding`: the mechanism binding parent, child, challenge, key, and
  topology rather than merely asserting that they belong together;
- `EvidenceBundle`: ordered native evidence and a component/binding graph;
- `ReferenceValues` and `Endorsements`: caller-provided or explicitly resolved
  verification inputs with provenance and validity;
- `ComponentAppraisal`: native verified claims without lossy normalization;
- `AttestationResult`: the complete component graph, appraisals, warnings,
  policy identity, and terminal decision; and
- `AttestedVm`: the live VM capability paired with the accepted result and
  attested session public key.

Known evidence profiles receive typed adapters. Unknown evidence is never
accepted by generic parsing. Callers may supply a verifier implementation for
a known profile, but there is no dynamically loaded plugin framework.

## Key and evidence binding protocol

Every backend implements the same protocol obligation using its native fields:

1. The relying party creates a random challenge with an expiry and policy ID.
2. The guest attester generates a fresh signing or key-agreement key inside the
   confidential VM.
3. A domain-separated transcript digest covers the protocol version,
   challenge, ephemeral public key, expected workload-manifest digest, and
   required child-device evidence digests.
4. The native attestation mechanism binds that digest into SNP `REPORT_DATA`,
   TDX `REPORTDATA`, Nitro nonce/public-key/user-data, CCA challenge data, or
   the corresponding backend field.
5. Each GPU or switch receives a derived challenge and returns native device
   evidence. The parent evidence or a hardware-protected channel must bind the
   devices to the VM; equal nonces alone are not sufficient.
6. The verifier validates signatures, certificate chains, collateral,
   revocation, freshness, measurements, TCB, debug and migration policy,
   device state, and every required graph edge.
7. Only a successful appraisal creates `AttestedVm` or permits secret release.

The protocol uses canonical encodings and fixed domain separators. Raw reports,
certificates, collateral, reference values, and appraisal output are retained
for reproducibility. Private session keys never cross the guest boundary.

## Backend matrix

### AMD SEV-SNP

This is the first libkrun CVM backend.

- Build a dedicated x86_64 Linux libkrun artifact with `amd-sev` rather than
  unifying it with the generic `blk,net` build.
- Require the SNP KVM VM type, guest-memfd/private-memory support, `/dev/sev`,
  an SNP-capable CPU and firmware, and `libkrunfw-sev.so.5`.
- Accept only SNP, not legacy SEV or SEV-ES, for the initial profile.
- Require an ext4 block root; reject directory roots and every virtiofs share.
- Replace `krun_set_root_disk_remount`, which is unavailable in TEE builds,
  with the TEE block-root behavior expected by libkrun's guest init.
- Set the required split-IRQ-chip policy instead of the generic path's current
  unconditional `false`.
- Add an in-guest report collector and verification of the AMD certificate
  chain, signed TCB, policy, launch digest, and report-data binding.
- Calculate and publish reproducible expected launch measurements for exact
  libkrunfw, kernel, initrd, command line, attester, and dm-verity root inputs.

### Intel TDX

- Build a dedicated x86_64 Linux libkrun artifact with `tdx` and matching
  `libkrunfw-tdx.so.5`.
- Detect the required KVM TDX extensions, guest private memory, host TDX module,
  and a working quote-generation path.
- Enforce libkrun's actual vCPU and memory limits as typed capability failures,
  not late launch errors.
- Collect TD reports in the guest and obtain remotely verifiable DCAP quotes.
- Verify MRTD, RTMR values, attributes, XFAM, TCB status, collateral,
  revocation, and report-data binding.
- Define which RTMR contains the authenticated root and later runtime
  extensions; do not give one register multiple undocumented owners.

### AWS Nitro Enclaves

- Build and test the separate `aws-nitro` libkrun flavor and EIF/init assets.
- Detect `/dev/nitro_enclaves`, allocator readiness, CPU and memory allocation,
  vsock, and required EIF identity.
- Repair the pinned 2.0 development API where the Nitro example and
  feature-gated virtiofs/rootfs configuration disagree.
- Collect the COSE-signed Nitro attestation document inside the enclave.
- Verify the AWS root, certificate validity, module ID, PCR policy, freshness,
  and nonce/public-key/user-data binding.
- Keep Nitro's parent-instance and I/O limitations visible instead of mapping
  them onto SNP or TDX semantics.

### NVIDIA confidential GPUs and NVSwitch

This is a libkrun extension program, not use of the existing virtio-gpu path.
The first target is one Hopper or later confidential GPU assigned to an SNP or
TDX guest; multi-GPU and NVSwitch follow only after single-device binding is
proven.

- Add the required VFIO/device-assignment boundary to libkrun without exposing
  a broad unsafe host-device API from `nanocodex-vm`.
- Support confidential DMA/private-memory sharing required by the selected CPU
  TEE and kernel.
- Establish and verify the protected CPU-TEE-to-GPU channel, including the
  applicable SPDM, PCIe IDE, TDISP, or vendor protocol.
- Run a supported NVIDIA guest driver in the measured root.
- Collect per-device reports, device certificate chains, firmware/VBIOS and
  driver RIM measurements, nonce, debug state, CC mode, UUID, and topology.
- Support local verification and an explicit NRAS-backed verifier. The result
  records which verifier and reference service were trusted.
- Represent every GPU and switch as its own component. Multi-device success
  requires all mandatory component appraisals and topology bindings.
- Never claim that GPU attestation proves a CUDA kernel or model executed; it
  proves the accepted GPU/device stack and its binding to the CVM.

### AMD trusted I/O and other confidential devices

- Track SEV-TIO/TDISP support in host KVM, firmware, libkrun, and concrete
  compatible devices.
- Add a backend only when native device evidence, protected assignment, and a
  reproducible verifier are available.
- Do not infer AMD Instinct confidential-GPU support from generic TDISP
  capability alone.

### Arm CCA

Arm CCA is a later libkrun investigation because libkrun already has an
aarch64 KVM path but does not currently own Realm launch and attestation.

- Audit KVM Realm support, RMM/firmware ownership, protected memory, RSI token
  collection, and CCA platform/Realm token verification.
- Prototype a dedicated CCA libkrun flavor only after the kernel and firmware
  ABI is stable enough for an audited boundary.
- Preserve CCA RIM and REM semantics rather than pretending they are SNP
  measurements.

IBM Secure Execution and SGX remain evidence-format research, not libkrun VM
commitments: IBM requires an s390x VM backend libkrun does not have, while SGX
is a process-enclave model rather than a confidential VM.

## Artifact and feature ownership

libkrun TEE flavors are compile-time variants with incompatible APIs and
firmware. They will not be merged into one Cargo feature-unified binary.

- The ordinary workspace build continues to produce the generic VMM.
- Dedicated build invocations produce clearly named generic, SNP, TDX, Nitro,
  and later confidential-device VMM artifacts.
- Each artifact records its Nanocodex revision, libkrun revision, enabled
  features, target, firmware identity, and guest-attester identity in a signed
  or content-addressed build manifest.
- The launch record names one requested profile and one matching VMM artifact.
- Compile-time guards reject incompatible feature combinations.
- Runtime capability detection verifies that the selected artifact actually
  reports the required libkrun features; a feature bit is not treated as proof
  of usable hardware.
- Firmware filenames and identities are profile-owned rather than hidden behind
  the current generic `libkrunfw.so.5` default.

The first implementation slice decides whether variant builds remain feature-
selected builds of the existing hidden `nanocodex vm-run-config` entry point or
become smaller unpublished VMM packages. The decision gate is concrete: normal
workspace `--all-targets` validation must not unify incompatible libkrun
features, and each artifact must retain the existing dedicated-process cleanup
contract. Dynamic loading is not introduced unless separate artifacts cannot
satisfy those constraints.

## libkrun upstream workstream

Implementation uses a dedicated worktree of `containers/libkrun` at the pinned
`2.0.0-dev` revision `df85b8b75f55e8ef1b06b5bc18f08dc6d7b5aeb0`.
Before patching it, classify relevant upstream changes since that revision so
already-landed fixes are not reimplemented. Every upstream change is isolated,
tested in libkrun's supported feature matrix, and submitted as a focused
libkrun commit before Nanocodex pins it. A temporary Nanocodex fork is
acceptable while upstream review is pending; long-lived behavior forks are
not.

The initial classification through upstream `07fd40d` is retained in
[`LIBKRUN_UPSTREAM_AUDIT.md`](LIBKRUN_UPSTREAM_AUDIT.md). It records the Nitro
fixes already upstream, the incompatible init/API migration, dead
attestation-shaped JSON fields, and the first typed TEE API patch.

Initial libkrun issues to resolve:

1. Restore or replace the remote-attestation functionality lost when the old C
   init was removed. The current Rust init parses TEE configuration but does
   not perform the prior KBS/LUKS attestation flow.
2. Replace stale JSON fields and file-only TEE configuration with explicit,
   typed C/Rust APIs where the VMM needs policy or measurement inputs.
3. Make TEE block-root and init behavior explicit so callers do not reference
   APIs compiled out under `feature = "tee"`.
4. Complete TDX quote/report support and document its exact measured regions,
   resource limits, and guest requirements.
5. Repair and test the Nitro 2.0 rootfs/API path.
6. Expose deterministic launch-measurement inputs or calculation tooling without
   making the untrusted host the attestation authority.
7. Add confidential device assignment, protected DMA, and evidence plumbing
   for NVIDIA confidential GPUs, then remove the current architectural
   incompatibility between TEE and the relevant device path.
8. Evaluate an Arm CCA flavor after the x86 and Nitro paths are complete.

Upstream tests must cover negative configurations and feature combinations, not
only successful launches. Nanocodex retains compatibility shims only while its
pinned libkrun revision requires them.

## Ordered implementation slices

### Slice 0: contracts, audit, and retained evidence

- [ ] Write the threat model and exact claim vocabulary into the
  `nanocodex-vm` README.
- [ ] Audit every libkrun TEE, Nitro, firmware, rootfs, memory, network, and
  device API at the pinned revision.
- [ ] Choose the separate-artifact layout and prove generic workspace builds do
  not feature-unify confidential variants.
- [ ] Define the profile, capability report, challenge, raw evidence,
  component graph, binding, reference, endorsement, and result types.
- [ ] Define canonical transcript hashing and key binding with test vectors.
- [ ] Establish a retained, redistributable fixture corpus of valid and invalid
  native evidence, collateral, reference values, and expected appraisals.
- [ ] Add parsers with strict size/depth bounds and fuzz targets before
  accepting untrusted CBOR, COSE, JSON, X.509, or vendor binary reports.

Exit gate: an ordinary VM remains unchanged; fixture-based appraisals are
deterministic; unsupported profiles fail with typed reasons; no code path can
construct `AttestedVm` without a successful result.

### Slice 1: reproducible measured guest

- [ ] Build a minimal static in-guest attester separately from the workspace
  tool runtime.
- [ ] Produce a manifest covering VMM, libkrun, firmware, kernel, initrd,
  command line, attester, root image, and dm-verity root.
- [ ] Make immutable confidential roots reproducible and verifiable before
  mounting writable state.
- [ ] Add the challenge/evidence handshake over a bounded transport while
  treating the host relay as malicious.
- [ ] Prove guest private keys never enter launch records, host tracing, console
  diagnostics, or retained host files.

Exit gate: changing any measured input changes the expected manifest; replacing
the root or attester is rejected; cancelling startup terminates and reaps the
VMM exactly once.

### Slice 2: live SEV-SNP vertical

- [ ] Complete the SNP libkrun upstream fixes and dedicated artifact.
- [ ] Implement strict SNP host detection and configuration validation.
- [ ] Launch the measured guest on real SNP hardware and collect fresh reports.
- [ ] Verify the complete AMD chain and policy locally from retained inputs.
- [ ] Bind an ephemeral guest key and dm-verity root to `REPORT_DATA` and the
  accepted launch measurement.
- [ ] Add live tamper tests for nonce replay, debug/migration policy, firmware,
  kernel, initrd, command line, root digest, report data, TCB, and certificate
  chain.
- [ ] Benchmark cold launch, report generation, verification, memory overhead,
  and shutdown against the ordinary libkrun VM.

Exit gate: a caller can request SNP and receive either an attested VM with
complete retained evidence or a typed terminal failure; no secret is necessary
to demonstrate the contract.

### Slice 3: live TDX vertical

- [ ] Complete the TDX libkrun, firmware, and quote-generation path.
- [ ] Implement DCAP collateral resolution with explicit offline/online policy.
- [ ] Bind the same protocol transcript into TDX report data.
- [ ] Define and test MRTD/RTMR ownership and authenticated-root measurement.
- [ ] Port the SNP lifecycle and tamper suite without erasing TDX-specific
  claims.
- [ ] Record backend-specific performance and resource limits.

Exit gate: the same caller-facing VM lifecycle works for SNP and TDX while raw
evidence, appraisal, policy, and limitations remain backend-specific.

### Slice 4: live Nitro vertical

- [ ] Complete the libkrun Nitro upstream repairs and artifact build.
- [ ] Build a reproducible EIF containing the measured attester.
- [ ] Collect and verify fresh Nitro documents and exact PCR policy.
- [ ] Bind the session key using Nitro's native public-key and user-data fields.
- [ ] Port cancellation, failure, evidence retention, and tamper tests.

Exit gate: a third, structurally different TEE passes the same lifecycle
without adding a generic-cloud or scheduler abstraction.

### Slice 5: one confidential NVIDIA GPU

- [ ] Select one supported live combination: SNP or TDX plus one Hopper-or-later
  confidential GPU.
- [ ] Land the minimum audited libkrun device-assignment and protected-memory
  changes.
- [ ] Prove the protected CVM-to-GPU channel and collect native GPU evidence.
- [ ] Verify device certificates, RIMs, revocation, nonce, debug state, CC mode,
  firmware, VBIOS, driver, UUID, and VM binding.
- [ ] Return a composite component graph and reject CPU-only or GPU-only partial
  success when the profile requires both.
- [ ] Run a deterministic GPU memory/compute workload only as a device-path
  smoke; do not label it proof of workload execution.

Exit gate: host and VMM cannot substitute a different GPU, disable CC mode, or
mix evidence from another VM without appraisal failure.

### Slice 6: multi-GPU, NVSwitch, and additional devices

- [ ] Add multiple independently identified GPU components.
- [ ] Add NVSwitch/fabric evidence and protected topology edges.
- [ ] Reject missing, duplicated, reordered, or cross-host component evidence.
- [ ] Add AMD TIO/TDISP or another device backend only with a complete live
  evidence and verification story.

Exit gate: policy can require an exact confidential device topology and every
component is independently inspectable in the retained result.

### Slice 7: Arm CCA investigation and hardening

- [ ] Decide whether current KVM, RMM, firmware, and libkrun ABIs support an
  auditable CCA vertical.
- [ ] If viable, implement Realm launch plus platform/Realm token collection and
  verification as a dedicated libkrun flavor.
- [ ] Complete parser fuzzing, malicious-host transport tests, collateral
  expiry/revocation tests, rollback analysis, and an independent security
  review across all supported backends.
- [ ] Publish exact supported combinations and remove every experimental claim
  not backed by retained live evidence.

## Validation and hardware evidence

Deterministic CI uses captured evidence only when redistribution permits it.
Every fixture records its source backend, generation procedure, expected
policy, and whether identifiers were intentionally retained. Synthetic evidence
can test parsing and policy but can never support a hardware claim.

Live tests run on explicitly labelled hardware and retain:

- host CPU, firmware, kernel, KVM, and device capability reports;
- exact VMM, libkrun, libkrunfw, guest, driver, and image identities;
- raw challenge, native evidence, endorsements, collateral, reference values,
  and attestation results;
- launch, report, verification, and cleanup timing;
- negative-test mutation and expected failure class; and
- process/device cleanup evidence after success, failure, cancellation, and
  timeout.

Each supported profile needs at least one successful live record and the full
tamper matrix. CI skips unavailable hardware honestly and never substitutes a
simulator while reporting hardware success.

## Security and review gates

- All native evidence parsers have input bounds, no panicking runtime paths,
  and fuzz coverage.
- Certificate, collateral, and reference-value validity are evaluated at an
  explicit appraisal time with documented clock assumptions.
- Online vendor verification is an explicit trust and availability choice;
  local verification records the exact roots and collateral used.
- Migration, debug, SMT, device, and TCB policy are caller-visible typed claims.
- The host cannot inject an attestation result or session key through the
  private launch record.
- Raw evidence is observable and retained, but private keys and released
  secrets are never host-observed runtime data.
- Every failure before guest readiness, during evidence collection, during
  appraisal, or after rejection follows the existing bounded VMM cleanup
  contract.
- Crate-boundary checks keep this experimental work out of stable agent, OpenAI,
  tools, observability, and facade crates.

## Overall completion gate

The program is complete when SNP, TDX, Nitro, and at least one composite
CPU-TEE-plus-confidential-GPU profile can be selected through the same
`nanocodex-vm` lifecycle; each retains native evidence and backend-specific
claims; every required component and binding is appraised; unsupported or
weaker hosts fail closed; tampering and replay are rejected; cleanup remains
exact; and the libkrun changes required for those claims are either upstream or
carried as a small, reviewed, explicitly pinned delta.

Only after this gate should a separate plan define attestable command receipts
or place Nanocodex agents inside the resulting confidential VMs.

## Primary references

- [IETF RATS architecture](https://www.rfc-editor.org/rfc/rfc9334.html)
- [Entity Attestation Token](https://www.rfc-editor.org/rfc/rfc9711.html)
- [RATS Conceptual Message Wrapper](https://www.rfc-editor.org/rfc/rfc9999.html)
- [AMD SEV-SNP](https://www.amd.com/en/developer/sev.html)
- [Intel TDX guest and attestation](https://docs.kernel.org/arch/x86/tdx.html)
- [AWS Nitro Enclaves attestation](https://docs.aws.amazon.com/enclaves/latest/user/set-up-attestation.html)
- [NVIDIA GPU and switch attestation](https://docs.nvidia.com/attestation/attestation-client-tools-sdk/latest/gpu_and_switch_attestation.html)
- [NVIDIA confidential-container composite attestation](https://docs.nvidia.com/datacenter/cloud-native/confidential-containers/latest/attestation.html)
- [Confidential Containers supported TEEs](https://confidentialcontainers.org/docs/overview/)
