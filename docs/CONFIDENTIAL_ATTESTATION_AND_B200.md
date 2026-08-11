# Native attestation and NVIDIA B200 enablement

## Decision

PR #141 remains draft until each libkrun launch path has live evidence from the
hardware it claims to support. The software boundary implements three
independently testable layers:

1. a measured guest evidence collector, verifier interface, local NVIDIA
   verifier, and fail-closed composite verification;
2. exact Linux PCI/IOMMU bundle resolution and an audited pinned libkrun
   VFIO-cdev/IOMMUFD assignment boundary; and
3. typed CPU, GPU, switch, and topology appraisal policy integrated into the
   launch-and-collect examples. GPU ReadyState and secret release remain closed
   until the complete composite appraisal succeeds.

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

NVIDIA R595 ships Blackwell MPT CC for supported HGX B200/B300 systems and
documents encrypted peer-to-peer NVLink inside one CVM. The current GPU and
NVSwitch attestation claim schemas still do not sign the complete link
administrative state or encrypted MPT partition identity. The 8-GPU
implementation is therefore intentionally fail-closed: its typed path and
exact evidence counts exist, but the profile cannot pass remote appraisal
until NVIDIA exposes a signed fabric-state claim.

Primary NVIDIA references:

- [R595 trusted-computing release notes](https://docs.nvidia.com/595trd1-trusted-computing-solutions-release-notes.pdf)
- [Confidential Computing deployment guide](https://docs.nvidia.com/cc-deployment-guide-tdx-snp.pdf)
- [Secure AI with Blackwell and Hopper](https://docs.nvidia.com/nvidia-secure-ai-with-blackwell-and-hopper-gpus-whitepaper.pdf)
- [Fabric Manager topology and assignment guide](https://docs.nvidia.com/hgx-platforms/fabric-manager-user-guide/index.html)
- [GPU and switch attestation](https://docs.nvidia.com/attestation/attestation-client-tools-sdk/latest/gpu_and_switch_attestation.html)
- [NVIDIA attestation claims](https://docs.nvidia.com/attestation/advanced-documentation/latest/claims-guide/introduction.html)
- [Blackwell multi-GPU attestation](https://docs.nvidia.com/attestation/quick-start-guide/latest/attestation-examples/blackwell_multi_gpu.html)
- [NVIDIA System Management Interface](https://docs.nvidia.com/deploy/nvidia-smi/index.html)

## Measured guest attester

The attester is a mode of the small static `nanocodex-vm-guest` program built
separately from the agent. `nanocodex-vm-guest --attest` accepts one bounded
JSON request on stdin and emits one bounded evidence bundle.
`nanocodex-vm-guest --attest-example` additionally auto-detects Nitro NSM or
the Linux TSM provider and an exact supported B200 topology. The typed VM
transport exposes the same operation as `VmToolSession::attest`; the host is
never trusted to construct evidence or an appraisal result.

For every launch it:

1. receives a versioned challenge containing a 256-bit nonce, policy identity,
   expiry, and expected workload-manifest digest;
2. lazily generates and retains an Ed25519 identity in the measured guest
   supervisor, unless the low-level `--attest` API is being used with a
   caller-owned in-guest key;
3. enumerates required child devices and collects their nonce-bound evidence;
4. hashes the ordered child evidence and topology into the canonical
   `AttestationBinding` transcript;
5. places that digest in the CPU TEE's native report-data field;
6. returns the public key, exact native evidence, and an Ed25519 signature over
   the domain-separated transcript to prove live key possession, never the
   private key; and
7. remains blocked from secret access until the relying
   party returns an accepted result bound to that challenge.

Requests, responses, individual evidence objects, and the complete bundle have
independent byte/count/depth limits. Cancellation closes the channel and kills
the VMM through the existing process-group lifecycle.

The native bundle types, base64 encoding, exact component ordering, transcript
digest, key-possession proof, and verifier boundary are shared by host and
guest builds. `just attest-example` is the direct in-guest smoke;
`just attest-libkrun-snp` and `just attest-libkrun-tdx` are complete
launch-and-collect entry points. The Linux collector and static musl artifact
are tested on x86-64 Ubuntu with warnings denied; live hardware tests remain
mandatory.

### AMD SEV-SNP

The guest uses Linux's configfs TSM report ABI. The `sev_guest` provider emits
the native SNP report in `outblob` and the GHCB certificate table in `auxblob`.
The 32-byte transcript digest is copied into the first half of the 64-byte
`REPORT_DATA`; the remaining half is zero and verified as zero. `SnpVerifier`
performs appraisal entirely in Rust. Its bounded certificate-table parser
validates pinned AMD Milan, Genoa, or Turin ARK/ASK/VCEK chains, X.509 validity
and optional CRLs, report signature, chip and TCB identity, VMPL,
debug/migration/SMT policy, exact launch measurement, minimum guest/TCB
versions, and report-data binding. VLEK evidence is rejected until it has an
explicit endorsement policy. The Linux ABI is specified by the
[SEV guest API](https://docs.kernel.org/virt/coco/sev-guest.html).

The measured manifest must reproduce libkrun's injected firmware, kernel,
initrd, command line, static attester, and authenticated root digest. A raw
ext4 digest by itself is not a launch measurement: the initial root becomes
dm-verity and its root hash is included in measured boot state.

### Intel TDX

The guest uses the `tdx_guest` TSM provider to generate a TDREPORT with the
same 64-byte report-data convention and return a DCAP quote. A host Quote
Generation Service must service the guest's `GetQuote` hypercall. The QGS and relay are untrusted for
correctness; signature and collateral verification are authoritative. The
`TdxVerifier` performs offline quote verification through a pure-Rust DCAP QVL
using retained caller-supplied PCS collateral and a strict current-status
policy. It verifies the PCK chain and collateral, quote signatures, QE and TDX
module status, debug/attribute policy, MRTD, all four RTMRs, optional XFAM and
owner/configuration fields, freshness, and report-data binding. Intel's
two-stage TDREPORT/quote model is described in the
[Linux TDX documentation](https://docs.kernel.org/arch/x86/tdx.html).

`VmConfig::tdx_quote_generation_socket` selects an explicit host QGS Unix
socket and is rejected for non-TDX profiles. The pinned libkrun handles the
TDX `GetQuote` exit, bounds and validates the shared request buffer, relays it
with QGS 1.1 framing, and returns the quote to the guest's configfs TSM
request. A raw TDREPORT is never substituted for a remotely verifiable quote.

RTMR ownership is fixed before implementation: firmware/boot owns one register,
the authenticated root and attester manifest own one, and later runtime
extensions own a separate register. An online Intel service can be an explicit
verifier option, but offline DCAP verification with retained collateral is the
baseline.

### AWS Nitro Enclaves

The guest requests a COSE-signed attestation document directly through AWS's
Rust NSM API. The challenge nonce uses the native `nonce` field, the guest
public key uses `public_key`, and the 32-byte workload transcript uses
`user_data`. `NitroVerifier` uses bounded CBOR, strict tagged COSE Sign1 ES384,
a caller-pinned AWS root, a unique X.509 path with validity and path-length
checks, timestamp freshness, exact SHA-384 PCR policy, optional module
identity, and all three signed bindings. AWS documents the exact CBOR structure and
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

`ConfidentialDeviceBundle` implements the admission half of this boundary. It
accepts only canonical full PCI BDFs, pins B200 to `10de:2901`,
requires one GPU for the single profile or eight GPUs plus exactly functions
`.0` through `.3` of one CX-7 bridge slot for HGX, rejects duplicates, checks
every vendor/device ID, requires a caller-pinned SHA-256 production VPD for
each CX-7 function, requires `vfio-pci`, resolves every IOMMU group, and
rejects any unassigned group sibling. Reset ownership, CC mode, and link state
deliberately remain additional live admission gates rather than being inferred
from PCI IDs. The pinned libkrun accepts the resolved VFIO cdevs, creates one
IOMMUFD IOAS for the VM, cold-plugs functions at deterministic guest BDFs,
configures BARs and MSI-X, and maps only DMA-eligible guest pages.
Confidential private-page conversion unmaps DMA before the page returns to
private state.

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

Native evidence is always retained. `NativeEvidenceVerifier` is the explicit
vendor cryptographic boundary and `verify_attestation` independently enforces
challenge freshness, exact requested topology, exact evidence digests,
CPU-to-child transcript binding, nonce binding, secure boot, disabled debug,
measurement policy, and NVIDIA fabric mode. Only that function can produce a
`VerifiedAttestation`; a vendor `success: true` boolean is insufficient.

- SNP uses a bounded safe certificate-table parser and pure-Rust signature,
  chain, revocation, TCB, and measurement policy with pinned AMD roots.
- TDX uses a pure-Rust DCAP QVL with caller-retained collateral and strict
  quote/TCB/measurement policy.
- Nitro uses bounded CBOR/COSE/X.509 parsing, pure-Rust P-384 verification, and
  a pinned AWS root supplied by caller policy.
- NVIDIA local verification wraps the Attestation SDK; NRAS is a separate
  explicit trust/availability choice. Detached EAT/JWT results and the raw SPDM
  evidence they appraise are both retained.

`NvidiaNvattestVerifier` now implements the local NVIDIA side. It reconstructs
one bounded evidence file per component, invokes NVAT with the exact challenge
nonce and a mandatory operator Rego policy, requires one claim and a detached EAT,
and checks device type, nonce, secure boot, debug state, measurement result,
architecture, parse status, and report signature. A single GPU is classified
as having no signed switch relationship when its claim has no switch PDI, but
that does not prove every NVLink is disabled because the current claim schema
has no link-state field. Neither disabled NVLink nor encrypted MPT is inferred,
which keeps both exact GPU profiles closed until the additional measured guest
observation or signed vendor claim is present.

No verifier performs ambient network access. Collateral resolution happens
before appraisal through a caller-selected offline or online resolver and all
bytes, provenance, validity intervals, and appraisal time enter the result.

## Ordered implementation and live gates

1. Retain positive and negative real-hardware fixtures for the implemented
   SNP, TDX/DCAP, Nitro, and NVIDIA verifier backends.
2. Extend the implemented typed session attestation and guest-key proof into
   key-release gating, with malicious-relay protocol tests. The separate
   sealed-ELF command receipt already exercises this binding, but does not
   release secrets or replace vendor-native appraisal.
3. Add SNP verification fixtures, measurement tooling, then a live
   SNP record.
4. Retain the implemented libkrun TDX QGS relay and DCAP appraisal record, then
   complete a production-quote libkrun TDX record on a KVM TDX host enrolled
   with Intel PCS.
5. Finish Nitro's dedicated libkrun launch path and produce a live Nitro record.
6. Validate the pinned libkrun IOMMUFD/VFIO support with one B200 and
   CPU-plus-GPU appraisal before enabling ReadyState.
7. Use the implemented exact eight-GPU/two-switch/four-CX-7 bundle to validate
   encrypted NVLink once a signed vendor fabric claim exists, then retain a
   live tamper matrix.

Managed GCP SNP and TDX guests have produced fresh native evidence which passed
the strict offline AMD and Intel appraisers. A GCP C4 bare-metal host also
validated the nested libkrun TDX launch, configured measured-memory layout,
guest configfs GetQuote exit, and 1,052-byte QGS relay. That host was not
enrolled in Intel PCS: PCCS received HTTP 404 instead of PCK certificate data,
so no production quote existed and Nanocodex correctly rejected the empty TSM
report. A GCP C4D bare-metal SNP probe was blocked earlier because BIOS had not
reserved the RMP table. These are operator provisioning blockers, not passing
attestation records.

GCP A4 exposes eight B200s only inside a VM, while its B200 bare-metal-adjacent
offerings have different four-GPU GB200/GB300 topologies. GCP also does not list
B200 among its Confidential VM GPU combinations. It therefore cannot validate
libkrun's host VFIO/IOMMUFD boundary or either exact B200 profile. Hardware
claims still need an enrolled bare-metal SNP/TDX runner, Nitro, PCIe one-B200,
and HGX/DGX eight-B200 runners. PR #141 stays draft, and the GPU runtime gate
remains closed, until those records exist.

The retained artifact layout, positive gates, and mandatory tamper matrix are
defined in
[`CONFIDENTIAL_HARDWARE_VALIDATION.md`](CONFIDENTIAL_HARDWARE_VALIDATION.md).
