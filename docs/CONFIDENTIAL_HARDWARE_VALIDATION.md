# Confidential VM hardware validation

This is the release gate for the confidential profiles in PR #141. A build or
capability probe is not attestation evidence. Each supported profile needs a
retained launch record, native evidence, successful independent appraisal, and
the negative cases below on hardware dedicated to that profile.

## Record layout

Keep the records outside Git while they contain platform identifiers or
operator policy. Publish a redacted, content-addressed summary only after the
operator has approved it. One run directory contains:

- `build.json`: Nanocodex, libkrun, libkrunfw, kernel, initrd, guest attester,
  root-image, driver, firmware, NVAT, and verifier revisions and SHA-256 hashes;
- `host.json`: CPU model, firmware/TEE state, KVM capabilities, IOMMU state,
  selected PCI functions, IOMMU groups, and the exact profile requested;
- `request.json`: the relying-party challenge, policy ID, expiry, expected
  workload-manifest digest, CPU profile, and optional NVIDIA profile;
- `bundle.json`: the byte-exact `GuestAttestationBundle` returned by the guest;
- `key-proof.json`: the detached Ed25519 proof over the attestation transcript;
- `collateral/`: the exact endorsements, CRLs, reference values, RIMs, and
  policy files used by native verification, with provenance and validity;
- `appraisal.json`: native verifier identity/status and the accepted composite
  claims, including the digest of every evidence object;
- `console.log`: bounded VMM/guest diagnostics with secrets excluded; and
- `tamper.json`: every required negative case and its exact rejection variant.

The challenge nonce is generated immediately before launch from a
cryptographically secure relying-party RNG. Appraisal uses a trusted explicit
Unix time and must complete before the challenge expiry. A successful record
is never reused as freshness evidence for another launch.

## Common launch gate

Before a profile-specific run:

1. Build one dedicated libkrun TEE artifact. Never feature-unify SNP and TDX.
2. Record and verify the complete measured-workload manifest before launch.
3. Use an immutable authenticated guest root and keep writable secrets locked.
4. Run `ConfidentialHostReport` and retain every capability result.
5. Start the exact profile without fallback, collect evidence in the guest,
   and retain the unmodified bundle.
6. Resolve collateral independently of the evidence-producing guest.
7. Run the native verifier for every component and then
   `verify_attestation`.
8. Prove possession of the attested guest key before releasing a test secret.
9. Release only a synthetic test secret, confirm it is usable by the intended
   measured guest process, and destroy it after the run.

The initial collection smoke is one command on the appropriate machine:
`just attest-libkrun-snp`, `just attest-libkrun-tdx QGS_SOCKET`, or, from
inside a pre-launched Nitro Enclave, `just attest-example`. Supply a
relying-party `--nonce-hex` for records; the guest-generated demonstration
nonce is not an acceptable freshness record.

No production key release is enabled by a hardware bring-up run.

## AMD SEV-SNP runner

The runner must expose SNP KVM support, `/dev/sev`, guest private memory, and
the exact SEV libkrun/libkrunfw pair. Retain the configfs TSM `sev_guest`
`outblob` report and `auxblob` certificate table.

Acceptance requires an independently trusted AMD root and VCEK/VLEK chain,
valid report signature and TCB, VMPL 0, disabled debug and migration policy,
the exact expected launch measurement and host data, and a 64-byte
`REPORT_DATA` whose first 32 bytes are the Nanocodex transcript and whose last
32 bytes are zero.

Required failures: changed nonce, key, workload digest, report byte,
measurement, policy, TCB collateral, expiry, and child-evidence ordering.

## Intel TDX runner

The runner must expose TDX KVM support, guest private memory, the exact TDX
libkrun/libkrunfw pair, and a pinned QGS socket. The pinned libkrun relays the
guest's bounded configfs TSM `GetQuote` request to QGS with QGS 1.1 framing;
the retained evidence must therefore contain a complete DCAP quote, never only
a TDREPORT.

Acceptance requires offline DCAP QVL verification with retained PCK chain,
CRLs, TCB info, QE identity, TDX module identity, supplemental data, and
appraisal time. MRTD, owned RTMRs, attributes, XFAM, debug state, TCB status,
and the complete 64-byte `REPORTDATA` must match policy.

Required failures: stopped QGS, malformed relay length, stale collateral,
changed MRTD or RTMR, debug enabled, changed transcript, and expired challenge.

The August 2026 GCP `c4-standard-288-metal` probe validated KVM TDX launch with
the configured 1 GiB measured layout and delivered the guest's 1,052-byte
configfs GetQuote request to QGS. Quote production then stopped outside the VMM:
Intel PCS returned HTTP 404 for the physical platform and PCCS reported no PCK
certificate data. The guest kernel consequently exposed an empty `outblob`.
Nanocodex rejects that result as `EmptyTsmReport`; it is not retained as
evidence. A passing bare-metal record therefore requires cloud/operator PCK
enrollment or an already provisioned PCCS cache, not another relay fallback.

## AWS Nitro runner

The runner must use a Nitro-capable EC2 parent with allocator resources and an
EIF built from the recorded manifest. Retain the NSM COSE document byte for
byte.

Acceptance requires a pinned AWS root, complete valid certificate chain, COSE
signature, module ID and PCR policy, timestamp policy, and exact equality of
the native `nonce`, `public_key`, and `user_data` fields to the challenge,
attested guest key, and Nanocodex transcript respectively.

Required failures: changed PCR, nonce, public key, user data, certificate,
timestamp policy, and challenge expiry.

## One B200 runner

Use exactly one whole `10de:2901` GPU function, no CX-7 fabric bridge, and no
NVSwitch. Resolve the `ConfidentialDeviceBundle` before VMM context creation;
the function must be bound to `vfio-pci` in a complete selected IOMMU group.
Record reset ownership, CC mode, driver and firmware versions, and every NVLink
state.

Acceptance requires the parent SNP or TDX appraisal plus one nonce-bound GPU
appraisal with valid certificate/RIM/signature claims, secure boot, disabled
debug, CC mode, the expected B200 identity, and no signed switch PDI. Every
NVLink must be disabled through a measured in-guest observation included in the
parent transcript; current NVAT claims do not contain that state. A CUDA smoke
proves usability only, not execution attestation.

Required failures: ordinary GPU mode, wrong PCI ID, non-`vfio-pci` owner,
unselected IOMMU sibling, enabled NVLink, changed GPU evidence, missing GPU
evidence, and parent evidence that binds a different GPU digest.

## Eight B200 encrypted-MPT runner

Use exactly eight whole B200 GPU functions and the four platform-selected CX-7
bridge functions required to manage the two-switch HGX fabric. All selected
functions must pass the same identity, ownership, IOMMU-group, reset, VPD, and
firmware admission transaction before launch.

Acceptance additionally requires signed evidence for all eight GPUs and both
NVSwitches, exact PDI adjacency for the accepted baseboard, all 18 links per
GPU and all 72 links per switch, and an appraised Blackwell MPT CC encrypted
fabric state. NCCL peer-to-peer validation runs only after that appraisal.

Current NVIDIA tooling does not expose the signed switch/topology/MPT claims
needed for this decision. Until it does, `NvidiaNvattestVerifier` deliberately
returns no encrypted-MPT fabric claim and this profile must fail.

GCP cannot currently serve as this libkrun PCI-passthrough runner. Its A4 B200
offering is only `a4-highgpu-8g`, and A4 is a VM rather than bare metal. A4X is
also a VM with four GB200/B200 GPUs; A4X Max is bare metal but contains four
GB300/B300 GPUs. In addition, GCP's documented Confidential VM GPU combinations
are A3 High H100 with TDX and G4 RTX PRO 6000 with SEV, not B200. See the
[accelerator machine table](https://docs.cloud.google.com/compute/docs/accelerator-optimized-machines)
and [Confidential VM supported configurations](https://docs.cloud.google.com/confidential-computing/confidential-vm/docs/supported-configurations).
The required runners remain a PCIe B200 bare-metal host for the one-GPU profile
and an HGX/DGX B200 bare-metal host for the eight-GPU encrypted-fabric profile.

Required failures, once the vendor claims exist: each omitted GPU, switch, or
bridge; every single changed evidence object; duplicate/reordered evidence;
wrong PDI edge; one disabled or unencrypted link; mixed firmware; partial MPT
partition; and parent evidence from a different child-evidence transcript.

## Enablement decision

A profile becomes non-draft only when its implementation, native verifier,
launch/key-release integration, complete positive record, and full negative
matrix have been reviewed together. Availability of another TEE or a smaller
GPU topology cannot waive a missing gate.
