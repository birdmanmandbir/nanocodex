# Native attestation and NVIDIA B200 enablement

## Decision

PR #141 remains draft until it has live evidence from the hardware it claims to
support. Development proceeds in three independently testable layers:

1. a measured guest attester and native CPU-TEE verification;
2. audited libkrun VFIO/IOMMUFD assignment for one exact device bundle; and
3. composite CPU, GPU, switch, and topology appraisal before a GPU becomes
   ready or any secret is released.

The first NVIDIA targets are deliberately exact:

| Profile | Assigned device bundle | Interconnect policy |
| --- | --- | --- |
| `b200-single` | one whole B200 PCI function; zero NVSwitches | every GPU NVLink disabled |
| `b200-hgx-8` | all eight B200 PCI functions and the two-NVSwitch fabric represented by its required CX-7 bridge functions | all 18 links per GPU enabled and encrypted in Blackwell MPT CC mode |

These are two supported configurations, not a request to run a 1-GPU and an
8-GPU CVM simultaneously on the same eight-GPU baseboard. A profile never
falls back between them. Hopper PPCIe is not accepted for the encrypted-NVLink
profile because NVIDIA documents its GPU-to-GPU NVLink/NVSwitch traffic as
unencrypted. Blackwell MPT CC explicitly provides encrypted peer-to-peer
NVLink for up to eight GPUs.

Primary NVIDIA references:

- [R595 trusted-computing release notes](https://docs.nvidia.com/595trd1-trusted-computing-solutions-release-notes.pdf)
- [Confidential Computing deployment guide](https://docs.nvidia.com/cc-deployment-guide-tdx-snp.pdf)
- [Secure AI with Blackwell and Hopper](https://docs.nvidia.com/nvidia-secure-ai-with-blackwell-and-hopper-gpus-whitepaper.pdf)
- [Fabric Manager topology and assignment guide](https://docs.nvidia.com/hgx-platforms/fabric-manager-user-guide/index.html)
- [GPU and switch attestation](https://docs.nvidia.com/attestation/attestation-client-tools-sdk/latest/gpu_and_switch_attestation.html)
- [NVIDIA attestation claims](https://docs.nvidia.com/attestation/advanced-documentation/latest/claims-guide/introduction.html)

## Measured guest attester

The attester is a small static guest program built separately from the agent and
workspace-tool runtime. It owns a bounded request/response protocol over a
dedicated vsock port. The host is only a byte relay and is never trusted to
construct evidence or an appraisal result.

For every launch it:

1. receives a versioned challenge containing a 256-bit nonce, policy identity,
   expiry, and expected workload-manifest digest;
2. generates a fresh signing/key-agreement key inside the confidential guest;
3. enumerates required child devices and collects their nonce-bound evidence;
4. hashes the ordered child evidence and topology into the canonical
   `AttestationBinding` transcript;
5. places that digest in the CPU TEE's native report-data field;
6. returns the public key and exact native evidence, never the private key; and
7. remains blocked from workspace startup and secret access until the relying
   party returns an accepted result bound to that challenge.

Requests, responses, individual evidence objects, and the complete bundle have
independent byte/count/depth limits. Cancellation closes the channel and kills
the VMM through the existing process-group lifecycle.

### AMD SEV-SNP

The guest opens `/dev/sev-guest` and uses `SNP_GET_EXT_REPORT`, falling back to
`SNP_GET_REPORT` only when certificate collateral is supplied independently.
The 32-byte transcript digest is copied into the first half of the 64-byte
`REPORT_DATA`; the remaining half is zero and verified as zero. The verifier
checks the VCEK/VLEK chain, report signature, chip/TCB identity, VMPL, policy,
debug/migration/SMT requirements, launch measurement, freshness, and exact
report-data binding. The Linux ABI is specified by the
[SEV guest API](https://docs.kernel.org/virt/coco/sev-guest.html).

The measured manifest must reproduce libkrun's injected firmware, kernel,
initrd, command line, static attester, and authenticated root digest. A raw
ext4 digest by itself is not a launch measurement: the initial root becomes
dm-verity and its root hash is included in measured boot state.

### Intel TDX

The guest generates a TDREPORT with the same 64-byte report-data convention.
A host Quoting Generation Service converts it to a remotely verifiable DCAP
quote over a dedicated vsock relay. The QGS and relay are untrusted for
correctness; signature and collateral verification are authoritative. The
verifier checks quote signatures, PCK chain and CRLs, QE identity, TDX module
identity, TCB status, attributes, XFAM, MRTD, all owned RTMRs, freshness, and
report-data binding. Intel's two-stage TDREPORT/quote model is described in the
[Linux TDX documentation](https://docs.kernel.org/arch/x86/tdx.html).

RTMR ownership is fixed before implementation: firmware/boot owns one register,
the authenticated root and attester manifest own one, and later runtime
extensions own a separate register. An online Intel service can be an explicit
verifier option, but offline DCAP verification with retained collateral is the
baseline.

### AWS Nitro Enclaves

The guest requests a COSE-signed attestation document from the Nitro Secure
Module. The challenge nonce uses the native `nonce` field, the guest public key
uses `public_key`, and the workload transcript uses `user_data`, each within
Nitro's 1024-byte bounds. Verification covers the AWS root and CA bundle,
certificate validity, COSE signature, timestamp, module identity, expected
PCRs, and all three bindings. AWS documents the exact CBOR structure and
validation flow in [Verifying the root of trust](https://docs.aws.amazon.com/enclaves/latest/user/verify-root.html).

Nitro keeps a dedicated launch/image path; it is not represented as a KVM VM
with an ext4 disk or GPU assignment.

## B200 host and libkrun boundary

### Common prerequisites

Both B200 profiles require a supported SNP or TDX platform, Secure Boot and
IOMMU policy matching the selected deployment, a supported open NVIDIA guest
driver and CUDA stack, supported B200 firmware, every selected function bound
to `vfio-pci`, and the GPU configured in CC mode before launch. Version values
are policy inputs rather than hard-coded forever; NVIDIA's current R595 profile
calls for B200/B300 firmware 1.4.x and local Attestation SDK 2.6.3 or later for
MPT CC.

libkrun needs a narrow device-assignment API rather than a general host-BDF
escape hatch. The caller supplies an already-resolved `ConfidentialDeviceBundle`
containing canonical PCI identities, IOMMU groups, expected vendor/device IDs,
reset ownership, and exact topology. libkrun then owns:

- one IOMMUFD object per VM and VFIO device attachment to KVM;
- PCIe root ports, BAR/MMIO placement, MSI/MSI-X, and interrupt routing;
- mapping only explicitly shared guest memory for device DMA;
- teardown ordering, PF-FLR/reset, and group release on every failure path;
- rejection of multifunction/group siblings not present in the bundle; and
- no virtio-gpu substitution or fallback to an ordinary VM.

The initial API accepts only NVIDIA vendor `10de` and the reviewed B200 device
IDs/topologies. General VFIO assignment is out of scope.

### One B200

`b200-single` assigns exactly one complete B200 function to one CVM, requires
CC mode, requires every NVLink disabled, and contains no NVSwitch or bridge
component. CPU-to-GPU traffic uses the NVIDIA bounce-buffer encrypted path.
TDISP/PCIe IDE can become a separate stronger profile only after libkrun,
kernel, CPU, GPU, firmware, and driver support is demonstrated together; it is
never inferred from a B200 model number.

The guest open driver establishes its SPDM session, persistence mode keeps the
session keys alive, and the GPU remains Not Ready. The guest attester collects
native GPU report/certificate/RIM material through NVIDIA's SDK with a derived
nonce. Only accepted CPU-plus-GPU appraisal may set ReadyState.

### Eight B200s with encrypted NVLink

`b200-hgx-8` is the full HGX/DGX B200 fabric:

- exactly eight B200 GPU functions in one CVM;
- exactly two NVSwitch ASICs, reached through the platform's required CX-7
  bridge functions rather than direct NVSwitch passthrough;
- all 18 NVLinks on every GPU and all 72 links on each switch active;
- Blackwell MPT CC, never Hopper PPCIe;
- host `ib_umad` and Fabric Manager with symmetric partition-rail policy; and
- all devices in the same IOMMU/device assignment and appraisal transaction.

The host report validates the whole bundle before creating a VM. Partial
availability, mixed firmware, a device already owned by another driver, a
missing bridge, disabled link, unexpected IOMMU sibling, or a topology other
than the accepted baseboard fails before launch.

Inside the CVM, the driver establishes protected SPDM sessions and encrypted
NVLink keys. The attester collects eight GPU evidence objects, switch evidence,
and PDI/topology claims. The canonical order is PCI domain/bus/device/function,
then component kind and hardware identity. The parent CPU quote binds every
child evidence digest; the guest key signs the observed topology; the verifier
checks every GPU, both switches, bridge/fabric identity, PDI adjacency, CC
mode, debug state, firmware/RIM values, nonce, and encrypted-link policy. Equal
nonces alone never establish VM or topology binding.

ReadyState is set only after the composite result succeeds. A deterministic
CUDA/NCCL peer-to-peer test confirms usability and encrypted-MPT configuration,
but it is not treated as proof that an arbitrary workload executed.

## Verification boundary

Native evidence is always retained. A backend adapter can use a reviewed vendor
library, but the public API receives typed policy and returns typed component
claims; it does not accept a vendor `success: true` boolean as sufficient.

- SNP verification starts with a memory-safe native report parser and explicit
  AMD roots/collateral.
- TDX uses the Intel DCAP Quote Verification Library behind a small audited FFI
  boundary and retains the quote, collateral, supplemental data, and QVL status.
- Nitro uses bounded CBOR/COSE/X.509 parsing and a pinned AWS root supplied by
  caller policy.
- NVIDIA local verification wraps the Attestation SDK; NRAS is a separate
  explicit trust/availability choice. Detached EAT/JWT results and the raw SPDM
  evidence they appraise are both retained.

No verifier performs ambient network access. Collateral resolution happens
before appraisal through a caller-selected offline or online resolver and all
bytes, provenance, validity intervals, and appraisal time enter the result.

## Ordered implementation and live gates

1. Finish `EvidenceBundle`, endorsements/reference values, component graph,
   appraisal result, and unforgeable `AttestedVm` types.
2. Build the static attester plus malicious-relay protocol tests.
3. Add SNP collection, verification fixtures, measurement tooling, then a live
   SNP record.
4. Add TDX QGS relay, DCAP verification, fixtures, then a live TDX record.
5. Repair Nitro's dedicated libkrun path and produce a live Nitro record.
6. Land libkrun IOMMUFD/VFIO support and validate one B200 with CPU-plus-GPU
   appraisal before enabling ReadyState.
7. Extend the device bundle to the exact eight-GPU/two-switch/CX-7 topology,
   validate encrypted NVLink and all component evidence, and retain a live
   tamper matrix.

`dev-georgios` can compile and run deterministic tests but has none of the TEE
or GPU devices required by gates 3 through 7. Hardware claims therefore need
separate labelled SNP, TDX, Nitro, and HGX/DGX B200 runners. PR #141 stays
draft, and the runtime gate remains closed, until those records exist.
