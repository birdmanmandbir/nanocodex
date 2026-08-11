#![cfg_attr(feature = "host", doc = include_str!("../README.md"))]
#![cfg_attr(not(feature = "host"), doc = include_str!("../GUEST_RUNTIME.md"))]
#![deny(unsafe_code, missing_docs, rustdoc::broken_intra_doc_links)]

#[cfg(all(feature = "libkrun-amd-sev", feature = "libkrun-intel-tdx"))]
compile_error!("one VMM artifact cannot contain both AMD SEV-SNP and Intel TDX libkrun variants");

#[cfg(any(
    feature = "guest-runtime",
    all(
        feature = "host",
        any(
            all(target_os = "linux", not(target_env = "musl")),
            all(target_os = "macos", target_arch = "aarch64")
        )
    )
))]
mod attestation;
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
mod capabilities;
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
mod child_lifetime;
#[cfg(all(feature = "guest-runtime", target_os = "linux"))]
mod guest_attestation;
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
pub use child_lifetime::terminate_child_with_parent;
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
mod command;
#[cfg(any(
    feature = "guest-runtime",
    all(
        feature = "host",
        any(
            all(target_os = "linux", not(target_env = "musl")),
            all(target_os = "macos", target_arch = "aarch64")
        )
    )
))]
mod command_proof;
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
mod confidential;
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
mod config;
#[cfg(all(feature = "host", target_os = "linux", not(target_env = "musl")))]
mod devices;
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
mod egress;
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
mod gvproxy;
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
pub mod image;
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
mod krun;
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
mod nitro_verification;
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
mod nvidia_verification;
#[cfg(any(
    all(feature = "guest-runtime", target_os = "linux"),
    all(
        feature = "host",
        any(
            all(target_os = "linux", not(target_env = "musl")),
            all(target_os = "macos", target_arch = "aarch64")
        )
    )
))]
mod overlay;
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
mod process;
#[cfg(any(
    feature = "guest-runtime",
    all(
        feature = "host",
        any(
            all(target_os = "linux", not(target_env = "musl")),
            all(target_os = "macos", target_arch = "aarch64")
        )
    )
))]
mod secret_release;
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
mod snp_verification;
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
mod tdx_verification;
#[cfg(any(
    feature = "guest-runtime",
    all(
        feature = "host",
        any(
            all(target_os = "linux", not(target_env = "musl")),
            all(target_os = "macos", target_arch = "aarch64")
        )
    )
))]
pub mod tools;
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
mod verification;
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
mod workspace;

/// Low-level host-side VM configuration and lifecycle components.
///
/// Most applications should start with [`crate::VmWorkspaceBuilder`]. This
/// module is for custom VMM entry points, network/egress policy, and direct
/// libkrun lifecycle ownership.
#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
pub mod host {
    #[cfg(all(target_os = "linux", not(target_env = "musl")))]
    pub use crate::devices::{
        ConfidentialDeviceBundle, ConfidentialPciDevice, ConfidentialPciRole, DeviceBundleError,
        PciAddress, ResolvedConfidentialDeviceBundle, VfioAssignment,
    };
    pub use crate::nitro_verification::{NitroVerificationPolicy, NitroVerifier};
    pub use crate::nvidia_verification::NvidiaNvattestVerifier;
    pub use crate::snp_verification::{
        SnpRevocationPolicy, SnpTcbVersion, SnpVerificationPolicy, SnpVerifier,
    };
    pub use crate::tdx_verification::{TdxVerificationPolicy, TdxVerifier};

    pub use crate::{
        attestation::{
            AttestationBinding, AttestationChallenge, AttestationInputError, AttestedComponent,
            AttestedGuestKeyProof, AttestedGuestKeyProofError, CpuAttestationProfile,
            EvidenceProfile, GuestAttestation, GuestAttestationBundle, GuestAttestationParameters,
            GuestAttestationRequest, MAX_RAW_EVIDENCE_BYTES, NvidiaAttestationProfile, RawEvidence,
            WorkloadMeasurement,
        },
        capabilities::{Capabilities, KrunFeature},
        command::GuestCommand,
        command_proof::{
            AttestedCommand, AttestedCommandProof, AttestedCommandReceipt, AttestedCommandRequest,
            CollectedCommandProof, CommandProofExpectation, CommandProofInputError,
            CommandProofVerificationError, CommandTermination, ExecutionRecord,
            MAX_ATTESTED_COMMAND_OUTPUT_BYTES, MAX_ATTESTED_EXECUTABLE_BYTES, VerifiedCommandProof,
            verify_collected_command_proof, verify_command_proof, verify_released_secret_proof,
        },
        confidential::{
            ConfidentialCapability, ConfidentialCapabilityCheck, ConfidentialHostReport,
            ConfidentialNvidiaProfile, ConfidentialVmError, ConfidentialVmProfile, CpuTee,
        },
        config::{BlockDevice, Network, RootFilesystem, SharedDirectory, VmConfig},
        egress::{
            EgressError, EgressFile, EgressLease, EgressMount, GUEST_EGRESS_ROOT,
            MAX_EGRESS_FILE_BYTES,
        },
        gvproxy::{Gvproxy, GvproxyError},
        krun::{KrunVm, KrunVmControl, VmError},
        overlay::{OverlayDiskError, create_sparse_overlay_disk, overlay_guest_command},
        process::{PrivateVmProcessConfig, VmProcessConfig, VmProcessError},
        secret_release::{
            MAX_SECRET_RELEASE_BYTES, SecretReleaseEnvelope, SecretReleaseError, seal_secret,
        },
        verification::{
            AttestationVerificationError, CpuVerifierSet, NativeEvidenceVerifier,
            NativeVerificationContext, NativeVerificationError, NativeVerifierSet,
            VerifiedAttestation, VerifiedNativeBinding, VerifiedNativeEvidence,
            VerifiedNvidiaFabric, verify_attestation,
        },
    };
}

/// Guest-side collection of native confidential-computing evidence.
#[cfg(all(feature = "guest-runtime", target_os = "linux"))]
pub mod guest {
    pub use crate::{
        attestation::{
            AttestationChallenge, AttestationInputError, AttestedGuestKeyProof,
            AttestedGuestKeyProofError, CpuAttestationProfile, GuestAttestation,
            GuestAttestationBundle, GuestAttestationParameters, GuestAttestationRequest,
            NvidiaAttestationProfile, WorkloadMeasurement,
        },
        command_proof::{
            AttestedCommand, AttestedCommandProof, AttestedCommandReceipt, AttestedCommandRequest,
            CommandProofInputError, CommandTermination, ExecutionRecord,
            MAX_ATTESTED_COMMAND_OUTPUT_BYTES, MAX_ATTESTED_EXECUTABLE_BYTES,
        },
        guest_attestation::{
            GuestAttestationError, GuestAttestationIdentity, collect_attestation,
            detect_cpu_attestation_profile, detect_nvidia_attestation_profile,
        },
        secret_release::{SecretReleaseEnvelope, SecretReleaseError},
    };
}

#[cfg(all(
    feature = "host",
    any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )
))]
pub use workspace::{VmWorkspace, VmWorkspaceBuilder, VmWorkspaceError};
