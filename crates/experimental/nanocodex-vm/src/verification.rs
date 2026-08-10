use std::collections::BTreeSet;

use async_trait::async_trait;
use thiserror::Error;

use crate::attestation::{
    AttestationBinding, AttestationChallenge, AttestedComponent, CpuAttestationProfile,
    EvidenceProfile, GuestAttestationBundle, NvidiaAttestationProfile, RawEvidence,
};

/// Binding recovered by a vendor cryptographic evidence verifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerifiedNativeBinding {
    /// First 32 bytes of an SNP or TDX native report-data field.
    CpuTranscript([u8; 32]),
    /// All three independently signed AWS Nitro attestation fields.
    AwsNitro {
        /// Native `nonce` field.
        nonce: [u8; 32],
        /// Native `public_key` field.
        guest_public_key: Vec<u8>,
        /// Native `user_data` field.
        transcript_digest: [u8; 32],
    },
    /// Nonce recovered from signed NVIDIA device evidence.
    NvidiaNonce([u8; 32]),
}

/// Fabric state recovered from signed NVIDIA claims and platform inspection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerifiedNvidiaFabric {
    /// NVLink is disabled and no NVSwitch is assigned.
    Disabled,
    /// The device is a member of the complete encrypted HGX B200 MPT fabric.
    EncryptedHgxB200Mpt,
}

/// Vendor-verified claims for one exact native evidence object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedNativeEvidence {
    evidence_digest: [u8; 32],
    component: AttestedComponent,
    profile: EvidenceProfile,
    binding: VerifiedNativeBinding,
    policy_passed: bool,
    trusted_boot: bool,
    debug_disabled: bool,
    hardware_identity: String,
    nvidia_fabric: Option<VerifiedNvidiaFabric>,
}

impl VerifiedNativeEvidence {
    /// Creates claims returned by a trusted vendor verification backend.
    ///
    /// The composite verifier independently matches the evidence digest,
    /// component, profile, nonce/report-data binding, and requested topology.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        evidence_digest: [u8; 32],
        component: AttestedComponent,
        profile: EvidenceProfile,
        binding: VerifiedNativeBinding,
        policy_passed: bool,
        trusted_boot: bool,
        debug_disabled: bool,
        hardware_identity: impl Into<String>,
        nvidia_fabric: Option<VerifiedNvidiaFabric>,
    ) -> Self {
        Self {
            evidence_digest,
            component,
            profile,
            binding,
            policy_passed,
            trusted_boot,
            debug_disabled,
            hardware_identity: hardware_identity.into(),
            nvidia_fabric,
        }
    }

    /// Returns the digest of the exact evidence which was verified.
    #[must_use]
    pub const fn evidence_digest(&self) -> &[u8; 32] {
        &self.evidence_digest
    }

    /// Returns the signed component identity class.
    #[must_use]
    pub const fn component(&self) -> &AttestedComponent {
        &self.component
    }

    /// Returns the native evidence profile.
    #[must_use]
    pub const fn profile(&self) -> EvidenceProfile {
        self.profile
    }

    /// Returns the native nonce or report-data binding.
    #[must_use]
    pub const fn binding(&self) -> &VerifiedNativeBinding {
        &self.binding
    }

    /// Returns the stable signed hardware identity selected by the backend.
    #[must_use]
    pub fn hardware_identity(&self) -> &str {
        &self.hardware_identity
    }

    /// Returns whether the backend's named measurement policy passed.
    #[must_use]
    pub const fn policy_passed(&self) -> bool {
        self.policy_passed
    }

    /// Returns whether native claims establish the backend's trusted boot or launch state.
    #[must_use]
    pub const fn trusted_boot(&self) -> bool {
        self.trusted_boot
    }

    /// Returns whether native claims show production debug policy.
    #[must_use]
    pub const fn debug_disabled(&self) -> bool {
        self.debug_disabled
    }

    /// Returns appraised NVIDIA fabric state for a device claim.
    #[must_use]
    pub const fn nvidia_fabric(&self) -> Option<VerifiedNvidiaFabric> {
        self.nvidia_fabric
    }
}

/// Context a cryptographic backend must apply while appraising evidence.
#[derive(Clone, Copy, Debug)]
pub struct NativeVerificationContext<'a> {
    binding: &'a AttestationBinding,
    transcript_digest: [u8; 32],
    now_unix_seconds: u64,
}

impl<'a> NativeVerificationContext<'a> {
    /// Returns the complete relying-party challenge and policy identity.
    #[must_use]
    pub const fn challenge(&self) -> &'a AttestationChallenge {
        self.binding.challenge()
    }

    /// Returns the guest key and complete canonical binding being appraised.
    #[must_use]
    pub const fn attestation_binding(&self) -> &'a AttestationBinding {
        self.binding
    }

    /// Returns the transcript expected in the CPU native report-data field.
    #[must_use]
    pub const fn transcript_digest(&self) -> &[u8; 32] {
        &self.transcript_digest
    }

    /// Returns the relying party's trusted appraisal time as Unix seconds.
    #[must_use]
    pub const fn now_unix_seconds(&self) -> u64 {
        self.now_unix_seconds
    }
}

/// Failure reported by a vendor cryptographic verification backend.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("{message}")]
pub struct NativeVerificationError {
    message: String,
}

impl NativeVerificationError {
    /// Creates a bounded-display verification failure.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Trusted backend for platform-native signature, certificate, and policy checks.
///
/// Implementations are expected to use AMD SNP endorsements, Intel DCAP QVL,
/// AWS's Nitro root, or NVIDIA NVAT/NRAS as appropriate. Returning claims is a
/// security boundary: the composite verifier assumes the backend has validated
/// the native signature and the measurement policy named by `policy_id`.
#[async_trait]
pub trait NativeEvidenceVerifier: Send + Sync {
    /// Cryptographically verifies one exact native evidence object.
    async fn verify(
        &self,
        evidence: &RawEvidence,
        context: NativeVerificationContext<'_>,
    ) -> Result<VerifiedNativeEvidence, NativeVerificationError>;
}

/// Dispatches heterogeneous CPU evidence to exact architecture backends.
pub struct CpuVerifierSet<S, T, A> {
    snp: S,
    tdx: T,
    nitro: A,
}

impl<S, T, A> CpuVerifierSet<S, T, A> {
    /// Creates the complete CPU verifier dispatch table.
    #[must_use]
    pub const fn new(snp: S, tdx: T, nitro: A) -> Self {
        Self { snp, tdx, nitro }
    }
}

#[async_trait]
impl<S, T, A> NativeEvidenceVerifier for CpuVerifierSet<S, T, A>
where
    S: NativeEvidenceVerifier,
    T: NativeEvidenceVerifier,
    A: NativeEvidenceVerifier,
{
    async fn verify(
        &self,
        evidence: &RawEvidence,
        context: NativeVerificationContext<'_>,
    ) -> Result<VerifiedNativeEvidence, NativeVerificationError> {
        match evidence.profile() {
            EvidenceProfile::AmdSevSnp => self.snp.verify(evidence, context).await,
            EvidenceProfile::IntelTdx => self.tdx.verify(evidence, context).await,
            EvidenceProfile::AwsNitro => self.nitro.verify(evidence, context).await,
            EvidenceProfile::NvidiaGpu | EvidenceProfile::NvidiaNvSwitch => Err(
                NativeVerificationError::new("CPU verifier received NVIDIA evidence"),
            ),
        }
    }
}

/// Dispatches CPU and NVIDIA evidence to independently reviewed backends.
pub struct NativeVerifierSet<C, N> {
    cpu: C,
    nvidia: N,
}

impl<C, N> NativeVerifierSet<C, N> {
    /// Creates an exact profile-based verifier dispatch table.
    #[must_use]
    pub const fn new(cpu: C, nvidia: N) -> Self {
        Self { cpu, nvidia }
    }
}

#[async_trait]
impl<C, N> NativeEvidenceVerifier for NativeVerifierSet<C, N>
where
    C: NativeEvidenceVerifier,
    N: NativeEvidenceVerifier,
{
    async fn verify(
        &self,
        evidence: &RawEvidence,
        context: NativeVerificationContext<'_>,
    ) -> Result<VerifiedNativeEvidence, NativeVerificationError> {
        match evidence.profile() {
            EvidenceProfile::AmdSevSnp | EvidenceProfile::IntelTdx | EvidenceProfile::AwsNitro => {
                self.cpu.verify(evidence, context).await
            }
            EvidenceProfile::NvidiaGpu | EvidenceProfile::NvidiaNvSwitch => {
                self.nvidia.verify(evidence, context).await
            }
        }
    }
}

/// A complete attestation accepted by native and composite policy.
#[derive(Clone, Debug)]
pub struct VerifiedAttestation {
    bundle: GuestAttestationBundle,
    claims: Vec<VerifiedNativeEvidence>,
}

impl VerifiedAttestation {
    /// Returns the guest public key whose possession can be challenged next.
    #[must_use]
    pub fn guest_public_key(&self) -> &[u8] {
        self.bundle.request().guest_public_key()
    }

    /// Returns the measured workload manifest digest.
    #[must_use]
    pub const fn workload_manifest_digest(&self) -> &[u8; 32] {
        self.bundle.request().workload_manifest_digest()
    }

    /// Returns the appraised native component claims in canonical order.
    #[must_use]
    pub fn claims(&self) -> &[VerifiedNativeEvidence] {
        &self.claims
    }

    /// Returns the exact evidence retained for audit and re-appraisal.
    #[must_use]
    pub const fn bundle(&self) -> &GuestAttestationBundle {
        &self.bundle
    }
}

/// Composite verification failure. No attested handle is issued on any variant.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AttestationVerificationError {
    /// The relying-party challenge has expired.
    #[error("attestation challenge expired at {expires_at}; current time is {now}")]
    ChallengeExpired {
        /// Challenge expiry as Unix seconds.
        expires_at: u64,
        /// Appraisal time as Unix seconds.
        now: u64,
    },
    /// The response does not contain the exact expected challenge.
    #[error("attestation response does not match the relying-party challenge")]
    ChallengeMismatch,
    /// The evidence list does not have the exact required component order.
    #[error("attestation evidence topology does not match the requested profile")]
    TopologyMismatch,
    /// The response's transcript digest was not reproducible from exact child evidence.
    #[error("attestation transcript digest does not match the response evidence")]
    TranscriptMismatch,
    /// A native verifier rejected evidence.
    #[error("native verification failed for component {index}: {source}")]
    NativeVerification {
        /// Canonical component index.
        index: usize,
        /// Backend failure.
        source: NativeVerificationError,
    },
    /// A backend returned claims for different bytes, component, or format.
    #[error("native verification claims do not identify component {index} evidence")]
    NativeIdentityMismatch {
        /// Canonical component index.
        index: usize,
    },
    /// A backend did not recover the required native binding.
    #[error("native verification binding does not match component {index}")]
    NativeBindingMismatch {
        /// Canonical component index.
        index: usize,
    },
    /// Vendor measurement policy, trusted boot/launch, or debug policy failed.
    #[error("native security policy failed for component {index}")]
    NativePolicyMismatch {
        /// Canonical component index.
        index: usize,
    },
    /// Two required device components resolved to the same signed identity.
    #[error("native verification repeated a device hardware identity at component {index}")]
    DuplicateHardwareIdentity {
        /// Canonical component index.
        index: usize,
    },
    /// NVIDIA fabric state does not match the exact requested topology.
    #[error("NVIDIA fabric policy failed for component {index}")]
    NvidiaFabricMismatch {
        /// Canonical component index.
        index: usize,
    },
}

/// Verifies freshness, topology, transcript composition, and every native signature.
///
/// `now_unix_seconds` is explicit so callers can use a trusted time source and
/// tests remain deterministic.
///
/// # Errors
///
/// Returns an error on any missing component, expired or changed challenge,
/// transcript mismatch, failed vendor appraisal, insecure component state, or
/// incorrect NVIDIA fabric mode.
pub async fn verify_attestation<V>(
    bundle: GuestAttestationBundle,
    expected_challenge: &AttestationChallenge,
    now_unix_seconds: u64,
    verifier: &V,
) -> Result<VerifiedAttestation, AttestationVerificationError>
where
    V: NativeEvidenceVerifier,
{
    if now_unix_seconds > expected_challenge.expires_at_unix_seconds() {
        return Err(AttestationVerificationError::ChallengeExpired {
            expires_at: expected_challenge.expires_at_unix_seconds(),
            now: now_unix_seconds,
        });
    }
    if bundle.request().challenge() != expected_challenge {
        return Err(AttestationVerificationError::ChallengeMismatch);
    }
    validate_topology(&bundle)?;

    let evidence = bundle.evidence();
    let child_digests = evidence[..evidence.len() - 1]
        .iter()
        .map(RawEvidence::digest)
        .collect();
    let binding = AttestationBinding::new(
        expected_challenge.clone(),
        bundle.request().guest_public_key().to_vec(),
        *bundle.request().workload_manifest_digest(),
        child_digests,
    )
    .map_err(|_| AttestationVerificationError::TranscriptMismatch)?;
    let transcript_digest = binding.transcript_digest();
    if bundle.transcript_digest() != &transcript_digest {
        return Err(AttestationVerificationError::TranscriptMismatch);
    }

    let context = NativeVerificationContext {
        binding: &binding,
        transcript_digest,
        now_unix_seconds,
    };
    let mut claims = Vec::with_capacity(evidence.len());
    let mut device_identities = BTreeSet::new();
    for (index, native) in evidence.iter().enumerate() {
        let verified = verifier
            .verify(native, context)
            .await
            .map_err(|source| AttestationVerificationError::NativeVerification { index, source })?;
        if verified.evidence_digest != native.digest()
            || verified.component != *native.component()
            || verified.profile != native.profile()
            || verified.hardware_identity.is_empty()
        {
            return Err(AttestationVerificationError::NativeIdentityMismatch { index });
        }
        let binding_matches = match native.component() {
            AttestedComponent::CpuVm => {
                expected_cpu_binding(native.profile(), &binding, transcript_digest)
                    .is_some_and(|expected| verified.binding == expected)
            }
            AttestedComponent::NvidiaGpu { .. } | AttestedComponent::NvidiaNvSwitch { .. } => {
                verified.binding == VerifiedNativeBinding::NvidiaNonce(*expected_challenge.nonce())
            }
        };
        if !binding_matches {
            return Err(AttestationVerificationError::NativeBindingMismatch { index });
        }
        if !verified.policy_passed || !verified.trusted_boot || !verified.debug_disabled {
            return Err(AttestationVerificationError::NativePolicyMismatch { index });
        }
        if !matches!(native.component(), AttestedComponent::CpuVm)
            && !device_identities.insert(verified.hardware_identity.clone())
        {
            return Err(AttestationVerificationError::DuplicateHardwareIdentity { index });
        }
        validate_fabric(bundle.request().nvidia_profile(), native, &verified, index)?;
        claims.push(verified);
    }

    Ok(VerifiedAttestation { bundle, claims })
}

fn expected_cpu_binding(
    profile: EvidenceProfile,
    binding: &AttestationBinding,
    transcript_digest: [u8; 32],
) -> Option<VerifiedNativeBinding> {
    match profile {
        EvidenceProfile::AwsNitro => Some(VerifiedNativeBinding::AwsNitro {
            nonce: *binding.challenge().nonce(),
            guest_public_key: binding.guest_public_key().to_vec(),
            transcript_digest,
        }),
        EvidenceProfile::AmdSevSnp | EvidenceProfile::IntelTdx => {
            Some(VerifiedNativeBinding::CpuTranscript(transcript_digest))
        }
        EvidenceProfile::NvidiaGpu | EvidenceProfile::NvidiaNvSwitch => None,
    }
}

fn validate_topology(bundle: &GuestAttestationBundle) -> Result<(), AttestationVerificationError> {
    let mut expected = Vec::new();
    if let Some(nvidia) = bundle.request().nvidia_profile() {
        for index in 0..nvidia.gpu_count() {
            expected.push((
                AttestedComponent::NvidiaGpu {
                    index: u16::try_from(index)
                        .map_err(|_| AttestationVerificationError::TopologyMismatch)?,
                },
                EvidenceProfile::NvidiaGpu,
            ));
        }
        for index in 0..nvidia.switch_count() {
            expected.push((
                AttestedComponent::NvidiaNvSwitch {
                    index: u16::try_from(index)
                        .map_err(|_| AttestationVerificationError::TopologyMismatch)?,
                },
                EvidenceProfile::NvidiaNvSwitch,
            ));
        }
    }
    expected.push((
        AttestedComponent::CpuVm,
        cpu_evidence_profile(bundle.request().cpu_profile()),
    ));
    if bundle.evidence().len() != expected.len()
        || bundle
            .evidence()
            .iter()
            .zip(expected)
            .any(|(actual, (component, profile))| {
                actual.component() != &component || actual.profile() != profile
            })
    {
        return Err(AttestationVerificationError::TopologyMismatch);
    }
    Ok(())
}

const fn cpu_evidence_profile(profile: CpuAttestationProfile) -> EvidenceProfile {
    profile.evidence_profile()
}

fn validate_fabric(
    requested: Option<NvidiaAttestationProfile>,
    evidence: &RawEvidence,
    verified: &VerifiedNativeEvidence,
    index: usize,
) -> Result<(), AttestationVerificationError> {
    let required = match evidence.component() {
        AttestedComponent::CpuVm => None,
        AttestedComponent::NvidiaGpu { .. } | AttestedComponent::NvidiaNvSwitch { .. } => {
            Some(match requested {
                Some(NvidiaAttestationProfile::B200Single) => VerifiedNvidiaFabric::Disabled,
                Some(NvidiaAttestationProfile::B200Hgx8EncryptedNvlink) => {
                    VerifiedNvidiaFabric::EncryptedHgxB200Mpt
                }
                None => return Err(AttestationVerificationError::NvidiaFabricMismatch { index }),
            })
        }
    };
    if verified.nvidia_fabric != required {
        return Err(AttestationVerificationError::NvidiaFabricMismatch { index });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attestation::GuestAttestationRequest;

    struct AcceptingVerifier;

    #[async_trait]
    impl NativeEvidenceVerifier for AcceptingVerifier {
        async fn verify(
            &self,
            evidence: &RawEvidence,
            context: NativeVerificationContext<'_>,
        ) -> Result<VerifiedNativeEvidence, NativeVerificationError> {
            let (binding, fabric) = match evidence.component() {
                AttestedComponent::CpuVm => {
                    let binding = match evidence.profile() {
                        EvidenceProfile::AwsNitro => VerifiedNativeBinding::AwsNitro {
                            nonce: *context.challenge().nonce(),
                            guest_public_key: context
                                .attestation_binding()
                                .guest_public_key()
                                .to_vec(),
                            transcript_digest: *context.transcript_digest(),
                        },
                        _ => VerifiedNativeBinding::CpuTranscript(*context.transcript_digest()),
                    };
                    (binding, None)
                }
                _ => (
                    VerifiedNativeBinding::NvidiaNonce(*context.challenge().nonce()),
                    Some(VerifiedNvidiaFabric::Disabled),
                ),
            };
            Ok(VerifiedNativeEvidence::new(
                evidence.digest(),
                evidence.component().clone(),
                evidence.profile(),
                binding,
                true,
                true,
                true,
                "test-identity",
                fabric,
            ))
        }
    }

    fn challenge() -> AttestationChallenge {
        AttestationChallenge::new([7; 32], "test-policy", 2_000).unwrap()
    }

    fn bundle_with_gpu_byte(gpu_byte: u8) -> GuestAttestationBundle {
        let request = GuestAttestationRequest::new(
            challenge(),
            vec![1, 2, 3],
            [4; 32],
            CpuAttestationProfile::AmdSevSnp,
            Some(NvidiaAttestationProfile::B200Single),
        )
        .unwrap();
        let gpu = RawEvidence::new(
            AttestedComponent::NvidiaGpu { index: 0 },
            EvidenceProfile::NvidiaGpu,
            "application/json",
            [gpu_byte],
        )
        .unwrap();
        let binding = request.binding(vec![gpu.digest()]).unwrap();
        let digest = binding.transcript_digest();
        let cpu = RawEvidence::new(
            AttestedComponent::CpuVm,
            EvidenceProfile::AmdSevSnp,
            "application/octet-stream",
            [6],
        )
        .unwrap();
        GuestAttestationBundle::new(request, digest, vec![gpu, cpu])
    }

    #[tokio::test]
    async fn complete_composite_bundle_is_accepted() {
        let verified = verify_attestation(
            bundle_with_gpu_byte(5),
            &challenge(),
            1_999,
            &AcceptingVerifier,
        )
        .await
        .unwrap();
        assert_eq!(verified.claims().len(), 2);
        assert_eq!(verified.guest_public_key(), [1, 2, 3]);
    }

    #[tokio::test]
    async fn expired_challenge_is_rejected_before_native_verification() {
        assert!(matches!(
            verify_attestation(
                bundle_with_gpu_byte(5),
                &challenge(),
                2_001,
                &AcceptingVerifier
            )
            .await,
            Err(AttestationVerificationError::ChallengeExpired { .. })
        ));
    }

    #[tokio::test]
    async fn changed_child_evidence_breaks_the_transcript() {
        let original = bundle_with_gpu_byte(5);
        let changed_request = original.request().clone();
        let changed_gpu = RawEvidence::new(
            AttestedComponent::NvidiaGpu { index: 0 },
            EvidenceProfile::NvidiaGpu,
            "application/json",
            [9],
        )
        .unwrap();
        let changed_cpu = original.evidence()[1].clone();
        let changed = GuestAttestationBundle::new(
            changed_request,
            *original.transcript_digest(),
            vec![changed_gpu, changed_cpu],
        );
        assert_eq!(
            verify_attestation(changed, &challenge(), 1_999, &AcceptingVerifier)
                .await
                .unwrap_err(),
            AttestationVerificationError::TranscriptMismatch
        );
    }

    struct WrongNitroKeyVerifier;

    #[async_trait]
    impl NativeEvidenceVerifier for WrongNitroKeyVerifier {
        async fn verify(
            &self,
            evidence: &RawEvidence,
            context: NativeVerificationContext<'_>,
        ) -> Result<VerifiedNativeEvidence, NativeVerificationError> {
            Ok(VerifiedNativeEvidence::new(
                evidence.digest(),
                evidence.component().clone(),
                evidence.profile(),
                VerifiedNativeBinding::AwsNitro {
                    nonce: *context.challenge().nonce(),
                    guest_public_key: vec![0xff],
                    transcript_digest: *context.transcript_digest(),
                },
                true,
                true,
                true,
                "nitro-test",
                None,
            ))
        }
    }

    #[tokio::test]
    async fn nitro_requires_nonce_public_key_and_user_data() {
        let request = GuestAttestationRequest::new(
            challenge(),
            vec![1, 2, 3],
            [4; 32],
            CpuAttestationProfile::AwsNitro,
            None,
        )
        .unwrap();
        let digest = request.binding(Vec::new()).unwrap().transcript_digest();
        let cpu = RawEvidence::new(
            AttestedComponent::CpuVm,
            EvidenceProfile::AwsNitro,
            "application/cose",
            [6],
        )
        .unwrap();
        let bundle = GuestAttestationBundle::new(request, digest, vec![cpu]);

        assert_eq!(
            verify_attestation(bundle, &challenge(), 1_999, &WrongNitroKeyVerifier)
                .await
                .unwrap_err(),
            AttestationVerificationError::NativeBindingMismatch { index: 0 }
        );
    }

    struct DuplicateDeviceVerifier;

    #[async_trait]
    impl NativeEvidenceVerifier for DuplicateDeviceVerifier {
        async fn verify(
            &self,
            evidence: &RawEvidence,
            context: NativeVerificationContext<'_>,
        ) -> Result<VerifiedNativeEvidence, NativeVerificationError> {
            let (binding, fabric) = match evidence.component() {
                AttestedComponent::CpuVm => (
                    VerifiedNativeBinding::CpuTranscript(*context.transcript_digest()),
                    None,
                ),
                _ => (
                    VerifiedNativeBinding::NvidiaNonce(*context.challenge().nonce()),
                    Some(VerifiedNvidiaFabric::EncryptedHgxB200Mpt),
                ),
            };
            Ok(VerifiedNativeEvidence::new(
                evidence.digest(),
                evidence.component().clone(),
                evidence.profile(),
                binding,
                true,
                true,
                true,
                "replayed-device",
                fabric,
            ))
        }
    }

    #[tokio::test]
    async fn exact_hgx_counts_require_distinct_signed_device_identities() {
        let request = GuestAttestationRequest::new(
            challenge(),
            vec![1, 2, 3],
            [4; 32],
            CpuAttestationProfile::AmdSevSnp,
            Some(NvidiaAttestationProfile::B200Hgx8EncryptedNvlink),
        )
        .unwrap();
        let mut evidence = Vec::new();
        for index in 0..8_u16 {
            evidence.push(
                RawEvidence::new(
                    AttestedComponent::NvidiaGpu { index },
                    EvidenceProfile::NvidiaGpu,
                    "application/json",
                    [u8::try_from(index).unwrap_or_default().saturating_add(1)],
                )
                .unwrap(),
            );
        }
        for index in 0..2_u16 {
            evidence.push(
                RawEvidence::new(
                    AttestedComponent::NvidiaNvSwitch { index },
                    EvidenceProfile::NvidiaNvSwitch,
                    "application/json",
                    [u8::try_from(index).unwrap_or_default().saturating_add(20)],
                )
                .unwrap(),
            );
        }
        let digest = request
            .binding(evidence.iter().map(RawEvidence::digest).collect())
            .unwrap()
            .transcript_digest();
        evidence.push(
            RawEvidence::new(
                AttestedComponent::CpuVm,
                EvidenceProfile::AmdSevSnp,
                "application/octet-stream",
                [42],
            )
            .unwrap(),
        );
        let bundle = GuestAttestationBundle::new(request, digest, evidence);

        assert_eq!(
            verify_attestation(bundle, &challenge(), 1_999, &DuplicateDeviceVerifier)
                .await
                .unwrap_err(),
            AttestationVerificationError::DuplicateHardwareIdentity { index: 1 }
        );
    }
}
