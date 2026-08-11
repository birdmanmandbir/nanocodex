use std::collections::BTreeSet;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{SeqAccess, Visitor},
};
use sha2::{Digest, Sha256};
use thiserror::Error;

const TRANSCRIPT_DOMAIN: &[u8] = b"nanocodex-vm-attestation-transcript\0";
const TRANSCRIPT_VERSION: u32 = 1;
pub(crate) const KEY_PROOF_DOMAIN: &[u8] = b"nanocodex-vm-attested-key-proof\0";
const MAX_POLICY_ID_BYTES: usize = 256;
const MAX_GUEST_PUBLIC_KEY_BYTES: usize = 4 * 1024;
const MAX_CHILD_EVIDENCE: usize = 256;
const MAX_MEDIA_TYPE_BYTES: usize = 128;
const MAX_BUNDLE_EVIDENCE: usize = 11;

/// Maximum native evidence size accepted for one attested component.
pub const MAX_RAW_EVIDENCE_BYTES: usize = 16 * 1024 * 1024;

/// Fresh relying-party input which native hardware evidence must bind.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttestationChallenge {
    nonce: [u8; 32],
    policy_id: String,
    expires_at_unix_seconds: u64,
}

impl<'de> Deserialize<'de> for AttestationChallenge {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireChallenge {
            nonce: [u8; 32],
            policy_id: String,
            expires_at_unix_seconds: u64,
        }

        let wire = WireChallenge::deserialize(deserializer)?;
        Self::new(wire.nonce, wire.policy_id, wire.expires_at_unix_seconds)
            .map_err(serde::de::Error::custom)
    }
}

impl AttestationChallenge {
    /// Creates a challenge with a caller-generated 256-bit nonce.
    ///
    /// # Errors
    ///
    /// Returns an error for an all-zero nonce, an empty or oversized policy
    /// identifier, or a zero expiry. Generating an unpredictable nonce remains
    /// the caller's responsibility.
    pub fn new(
        nonce: [u8; 32],
        policy_id: impl Into<String>,
        expires_at_unix_seconds: u64,
    ) -> Result<Self, AttestationInputError> {
        if nonce == [0; 32] {
            return Err(AttestationInputError::ZeroNonce);
        }
        let policy_id = policy_id.into();
        if policy_id.is_empty() {
            return Err(AttestationInputError::EmptyPolicyId);
        }
        if policy_id.len() > MAX_POLICY_ID_BYTES {
            return Err(AttestationInputError::PolicyIdTooLarge {
                actual: policy_id.len(),
                maximum: MAX_POLICY_ID_BYTES,
            });
        }
        if expires_at_unix_seconds == 0 {
            return Err(AttestationInputError::ZeroExpiry);
        }
        Ok(Self {
            nonce,
            policy_id,
            expires_at_unix_seconds,
        })
    }

    /// Returns the caller-generated nonce.
    #[must_use]
    pub const fn nonce(&self) -> &[u8; 32] {
        &self.nonce
    }

    /// Returns the appraisal-policy identity selected by the caller.
    #[must_use]
    pub fn policy_id(&self) -> &str {
        &self.policy_id
    }

    /// Returns the absolute challenge expiry as Unix seconds.
    #[must_use]
    pub const fn expires_at_unix_seconds(&self) -> u64 {
        self.expires_at_unix_seconds
    }
}

/// Inputs bound into a backend's native report-data field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttestationBinding {
    challenge: AttestationChallenge,
    guest_public_key: Vec<u8>,
    workload_manifest_digest: [u8; 32],
    child_evidence_digests: Vec<[u8; 32]>,
}

impl AttestationBinding {
    /// Creates one canonical transcript for a guest key and measured workload.
    ///
    /// Child evidence digests must be supplied in canonical component order;
    /// callers cannot repeat a digest to satisfy a required topology.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or oversized key, too many child
    /// components, or duplicate child evidence digests.
    pub fn new(
        challenge: AttestationChallenge,
        guest_public_key: impl Into<Vec<u8>>,
        workload_manifest_digest: [u8; 32],
        child_evidence_digests: Vec<[u8; 32]>,
    ) -> Result<Self, AttestationInputError> {
        let guest_public_key = guest_public_key.into();
        if guest_public_key.is_empty() {
            return Err(AttestationInputError::EmptyGuestPublicKey);
        }
        if guest_public_key.len() > MAX_GUEST_PUBLIC_KEY_BYTES {
            return Err(AttestationInputError::GuestPublicKeyTooLarge {
                actual: guest_public_key.len(),
                maximum: MAX_GUEST_PUBLIC_KEY_BYTES,
            });
        }
        if child_evidence_digests.len() > MAX_CHILD_EVIDENCE {
            return Err(AttestationInputError::TooManyChildEvidenceDigests {
                actual: child_evidence_digests.len(),
                maximum: MAX_CHILD_EVIDENCE,
            });
        }
        let unique = child_evidence_digests
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if unique.len() != child_evidence_digests.len() {
            return Err(AttestationInputError::DuplicateChildEvidenceDigest);
        }
        Ok(Self {
            challenge,
            guest_public_key,
            workload_manifest_digest,
            child_evidence_digests,
        })
    }

    /// Returns the relying-party challenge.
    #[must_use]
    #[cfg(feature = "host")]
    pub const fn challenge(&self) -> &AttestationChallenge {
        &self.challenge
    }

    /// Returns the guest-generated public key.
    #[must_use]
    #[cfg(feature = "host")]
    pub fn guest_public_key(&self) -> &[u8] {
        &self.guest_public_key
    }

    /// Returns the expected measured-workload manifest digest.
    #[must_use]
    #[cfg(feature = "host")]
    pub const fn workload_manifest_digest(&self) -> &[u8; 32] {
        &self.workload_manifest_digest
    }

    /// Returns child evidence digests in canonical component order.
    #[must_use]
    #[cfg(feature = "host")]
    pub fn child_evidence_digests(&self) -> &[[u8; 32]] {
        &self.child_evidence_digests
    }

    /// Calculates the domain-separated SHA-256 transcript digest.
    ///
    /// SNP `REPORT_DATA`, TDX `REPORTDATA`, Nitro user data, and later native
    /// mechanisms bind this digest without redefining its field ordering.
    #[must_use]
    pub fn transcript_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(TRANSCRIPT_DOMAIN);
        hasher.update(TRANSCRIPT_VERSION.to_be_bytes());
        hash_bytes(&mut hasher, self.challenge.nonce());
        hash_bytes(&mut hasher, self.challenge.policy_id.as_bytes());
        hasher.update(self.challenge.expires_at_unix_seconds.to_be_bytes());
        hash_bytes(&mut hasher, &self.guest_public_key);
        hash_bytes(&mut hasher, &self.workload_manifest_digest);
        hasher.update((self.child_evidence_digests.len() as u32).to_be_bytes());
        for digest in &self.child_evidence_digests {
            hash_bytes(&mut hasher, digest);
        }
        hasher.finalize().into()
    }
}

/// Native evidence format retained for one component.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceProfile {
    /// Untrusted software evidence for end-to-end development smoke tests.
    #[cfg(feature = "development-attestation")]
    Development,
    /// AMD SEV-SNP attestation report.
    AmdSevSnp,
    /// Intel TDX quote.
    IntelTdx,
    /// AWS Nitro attestation document.
    AwsNitro,
    /// NVIDIA GPU attestation report.
    NvidiaGpu,
    /// NVIDIA NVSwitch attestation report.
    NvidiaNvSwitch,
}

/// Native CPU attestation mechanism requested from the guest.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CpuAttestationProfile {
    /// Untrusted software collector for end-to-end development smoke tests.
    #[cfg(feature = "development-attestation")]
    Development,
    /// AMD SEV-SNP through Linux's `sev_guest` TSM provider.
    AmdSevSnp,
    /// Intel TDX through Linux's `tdx_guest` TSM provider.
    IntelTdx,
    /// AWS Nitro Enclaves through the Nitro Secure Module.
    AwsNitro,
}

impl CpuAttestationProfile {
    /// Returns the native evidence profile emitted by this mechanism.
    #[must_use]
    pub const fn evidence_profile(self) -> EvidenceProfile {
        match self {
            #[cfg(feature = "development-attestation")]
            Self::Development => EvidenceProfile::Development,
            Self::AmdSevSnp => EvidenceProfile::AmdSevSnp,
            Self::IntelTdx => EvidenceProfile::IntelTdx,
            Self::AwsNitro => EvidenceProfile::AwsNitro,
        }
    }
}

/// NVIDIA device topology whose evidence must be included in the CPU report.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NvidiaAttestationProfile {
    /// Exactly one Hopper H100 GPU, with no NVSwitch evidence required.
    H100Single,
    /// Exactly one B200 GPU, with NVLink disabled.
    B200Single,
    /// Exactly eight B200 GPUs and two NVSwitches in encrypted MPT mode.
    B200Hgx8EncryptedNvlink,
}

impl NvidiaAttestationProfile {
    /// Returns the exact number of GPU evidence objects required.
    #[must_use]
    pub const fn gpu_count(self) -> usize {
        match self {
            Self::H100Single | Self::B200Single => 1,
            Self::B200Hgx8EncryptedNvlink => 8,
        }
    }

    /// Returns the exact number of NVSwitch evidence objects required.
    #[must_use]
    pub const fn switch_count(self) -> usize {
        match self {
            Self::H100Single | Self::B200Single => 0,
            Self::B200Hgx8EncryptedNvlink => 2,
        }
    }
}

/// Host-selected inputs for an attestation generated by the guest session.
///
/// Unlike [`GuestAttestationRequest`], this type does not accept a public key.
/// The guest generates and retains that key before constructing the native
/// evidence request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuestAttestationParameters {
    challenge: AttestationChallenge,
    workload_manifest_digest: [u8; 32],
    cpu_profile: CpuAttestationProfile,
    nvidia_profile: Option<NvidiaAttestationProfile>,
}

impl GuestAttestationParameters {
    /// Creates a guest-owned-key attestation request.
    #[must_use]
    pub const fn new(
        challenge: AttestationChallenge,
        workload_manifest_digest: [u8; 32],
        cpu_profile: CpuAttestationProfile,
        nvidia_profile: Option<NvidiaAttestationProfile>,
    ) -> Self {
        Self {
            challenge,
            workload_manifest_digest,
            cpu_profile,
            nvidia_profile,
        }
    }

    /// Returns the relying-party challenge.
    #[must_use]
    pub const fn challenge(&self) -> &AttestationChallenge {
        &self.challenge
    }

    /// Returns the expected measured-workload manifest digest.
    #[must_use]
    pub const fn workload_manifest_digest(&self) -> &[u8; 32] {
        &self.workload_manifest_digest
    }

    /// Returns the requested CPU attestation mechanism.
    #[must_use]
    pub const fn cpu_profile(&self) -> CpuAttestationProfile {
        self.cpu_profile
    }

    /// Returns the requested accelerator topology, if any.
    #[must_use]
    pub const fn nvidia_profile(&self) -> Option<NvidiaAttestationProfile> {
        self.nvidia_profile
    }

    #[cfg(any(test, all(feature = "guest-runtime", target_os = "linux")))]
    pub(crate) fn into_request(
        self,
        guest_public_key: Vec<u8>,
    ) -> Result<GuestAttestationRequest, AttestationInputError> {
        GuestAttestationRequest::new(
            self.challenge,
            guest_public_key,
            self.workload_manifest_digest,
            self.cpu_profile,
            self.nvidia_profile,
        )
    }
}

/// Stable identity of one independently appraised component.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AttestedComponent {
    /// Parent confidential CPU VM or enclave.
    CpuVm,
    /// One GPU identified by its canonical device UUID.
    NvidiaGpu {
        /// Stable collector ordinal; verification resolves the signed UEID.
        index: u16,
    },
    /// One NVSwitch identified by its canonical device UUID.
    NvidiaNvSwitch {
        /// Stable collector ordinal; verification resolves the signed UEID.
        index: u16,
    },
}

/// Bounded native hardware evidence retained byte-for-byte.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RawEvidence {
    component: AttestedComponent,
    profile: EvidenceProfile,
    media_type: String,
    #[serde(serialize_with = "serialize_base64")]
    bytes: Vec<u8>,
}

impl<'de> Deserialize<'de> for RawEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireEvidence {
            component: AttestedComponent,
            profile: EvidenceProfile,
            media_type: String,
            #[serde(deserialize_with = "deserialize_evidence_base64")]
            bytes: Vec<u8>,
        }

        let wire = WireEvidence::deserialize(deserializer)?;
        Self::new(wire.component, wire.profile, wire.media_type, wire.bytes)
            .map_err(serde::de::Error::custom)
    }
}

/// Bounded request accepted by the guest attestation collector.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuestAttestationRequest {
    challenge: AttestationChallenge,
    #[serde(
        serialize_with = "serialize_base64",
        deserialize_with = "deserialize_guest_key_base64"
    )]
    guest_public_key: Vec<u8>,
    workload_manifest_digest: [u8; 32],
    cpu_profile: CpuAttestationProfile,
    nvidia_profile: Option<NvidiaAttestationProfile>,
}

impl<'de> Deserialize<'de> for GuestAttestationRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireRequest {
            challenge: AttestationChallenge,
            #[serde(deserialize_with = "deserialize_guest_key_base64")]
            guest_public_key: Vec<u8>,
            workload_manifest_digest: [u8; 32],
            cpu_profile: CpuAttestationProfile,
            nvidia_profile: Option<NvidiaAttestationProfile>,
        }

        let wire = WireRequest::deserialize(deserializer)?;
        Self::new(
            wire.challenge,
            wire.guest_public_key,
            wire.workload_manifest_digest,
            wire.cpu_profile,
            wire.nvidia_profile,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl GuestAttestationRequest {
    /// Creates and validates a guest evidence request.
    ///
    /// # Errors
    ///
    /// Returns an error when the binding inputs violate protocol bounds.
    pub fn new(
        challenge: AttestationChallenge,
        guest_public_key: impl Into<Vec<u8>>,
        workload_manifest_digest: [u8; 32],
        cpu_profile: CpuAttestationProfile,
        nvidia_profile: Option<NvidiaAttestationProfile>,
    ) -> Result<Self, AttestationInputError> {
        let guest_public_key = guest_public_key.into();
        AttestationBinding::new(
            challenge.clone(),
            guest_public_key.clone(),
            workload_manifest_digest,
            Vec::new(),
        )?;
        Ok(Self {
            challenge,
            guest_public_key,
            workload_manifest_digest,
            cpu_profile,
            nvidia_profile,
        })
    }

    /// Returns the relying-party challenge.
    #[must_use]
    pub const fn challenge(&self) -> &AttestationChallenge {
        &self.challenge
    }

    /// Returns the guest-generated public key.
    #[must_use]
    pub fn guest_public_key(&self) -> &[u8] {
        &self.guest_public_key
    }

    /// Returns the measured workload manifest digest.
    #[must_use]
    pub const fn workload_manifest_digest(&self) -> &[u8; 32] {
        &self.workload_manifest_digest
    }

    /// Returns the requested CPU attestation mechanism.
    #[must_use]
    pub const fn cpu_profile(&self) -> CpuAttestationProfile {
        self.cpu_profile
    }

    /// Returns the requested accelerator topology, if any.
    #[must_use]
    pub const fn nvidia_profile(&self) -> Option<NvidiaAttestationProfile> {
        self.nvidia_profile
    }

    #[cfg(any(test, all(feature = "guest-runtime", target_os = "linux")))]
    pub(crate) fn binding(
        &self,
        child_evidence_digests: Vec<[u8; 32]>,
    ) -> Result<AttestationBinding, AttestationInputError> {
        AttestationBinding::new(
            self.challenge.clone(),
            self.guest_public_key.clone(),
            self.workload_manifest_digest,
            child_evidence_digests,
        )
    }
}

/// Complete native evidence response emitted by the guest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuestAttestationBundle {
    request: GuestAttestationRequest,
    transcript_digest: [u8; 32],
    #[serde(deserialize_with = "deserialize_bounded_evidence")]
    evidence: Vec<RawEvidence>,
}

impl GuestAttestationBundle {
    #[cfg(any(test, all(feature = "guest-runtime", target_os = "linux")))]
    pub(crate) const fn new(
        request: GuestAttestationRequest,
        transcript_digest: [u8; 32],
        evidence: Vec<RawEvidence>,
    ) -> Self {
        Self {
            request,
            transcript_digest,
            evidence,
        }
    }

    /// Returns the request bound into the response.
    #[must_use]
    pub const fn request(&self) -> &GuestAttestationRequest {
        &self.request
    }

    /// Returns the digest placed in the native CPU report-data field.
    #[must_use]
    pub const fn transcript_digest(&self) -> &[u8; 32] {
        &self.transcript_digest
    }

    /// Returns child evidence first and CPU evidence last.
    #[must_use]
    pub fn evidence(&self) -> &[RawEvidence] {
        &self.evidence
    }
}

/// Ed25519 proof that the guest which collected the evidence possesses the
/// private key corresponding to the public key bound into the CPU evidence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttestedGuestKeyProof {
    #[serde(serialize_with = "serialize_base64")]
    signature: Vec<u8>,
}

impl<'de> Deserialize<'de> for AttestedGuestKeyProof {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireProof {
            #[serde(deserialize_with = "deserialize_ed25519_signature")]
            signature: Vec<u8>,
        }

        let proof = WireProof::deserialize(deserializer)?;
        Ok(Self {
            signature: proof.signature,
        })
    }
}

impl AttestedGuestKeyProof {
    #[cfg(any(test, all(feature = "guest-runtime", target_os = "linux")))]
    pub(crate) fn new(signature: [u8; 64]) -> Self {
        Self {
            signature: signature.to_vec(),
        }
    }

    /// Returns the detached Ed25519 signature bytes.
    #[must_use]
    pub fn signature(&self) -> &[u8] {
        &self.signature
    }
}

/// Native evidence plus proof that its bound guest public key is live.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuestAttestation {
    bundle: GuestAttestationBundle,
    key_proof: AttestedGuestKeyProof,
}

impl GuestAttestation {
    #[cfg(any(test, all(feature = "guest-runtime", target_os = "linux")))]
    pub(crate) const fn new(
        bundle: GuestAttestationBundle,
        key_proof: AttestedGuestKeyProof,
    ) -> Self {
        Self { bundle, key_proof }
    }

    /// Returns the complete native evidence bundle.
    #[must_use]
    pub const fn bundle(&self) -> &GuestAttestationBundle {
        &self.bundle
    }

    /// Returns the guest-key possession proof.
    #[must_use]
    pub const fn key_proof(&self) -> &AttestedGuestKeyProof {
        &self.key_proof
    }

    /// Verifies that the evidence-bound Ed25519 key signed this transcript.
    ///
    /// This proves possession of the guest key; it does not replace vendor
    /// verification of the native CPU or accelerator evidence.
    ///
    /// # Errors
    ///
    /// Returns an error for a malformed public key, malformed signature, or
    /// signature mismatch. The check also validates the declared component
    /// topology and recomputes the signed transcript from the retained child
    /// evidence before accepting the proof.
    pub fn verify_key_proof(&self) -> Result<(), AttestedGuestKeyProofError> {
        let transcript_digest = self.recompute_transcript_digest()?;
        if self.bundle.transcript_digest != transcript_digest {
            return Err(AttestedGuestKeyProofError::TranscriptMismatch);
        }
        let public_key = <&[u8; 32]>::try_from(self.bundle.request().guest_public_key())
            .map_err(|_| AttestedGuestKeyProofError::InvalidPublicKeyLength)?;
        let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(public_key)
            .map_err(|_| AttestedGuestKeyProofError::InvalidPublicKey)?;
        let signature = ed25519_dalek::Signature::from_slice(self.key_proof.signature())
            .map_err(|_| AttestedGuestKeyProofError::InvalidSignature)?;
        verifying_key
            .verify_strict(&key_proof_message(&transcript_digest), &signature)
            .map_err(|_| AttestedGuestKeyProofError::SignatureMismatch)
    }

    fn recompute_transcript_digest(&self) -> Result<[u8; 32], AttestedGuestKeyProofError> {
        let mut expected = Vec::new();
        if let Some(nvidia) = self.bundle.request.nvidia_profile {
            for index in 0..nvidia.gpu_count() {
                let index = u16::try_from(index)
                    .map_err(|_| AttestedGuestKeyProofError::TopologyMismatch)?;
                expected.push((
                    AttestedComponent::NvidiaGpu { index },
                    EvidenceProfile::NvidiaGpu,
                ));
            }
            for index in 0..nvidia.switch_count() {
                let index = u16::try_from(index)
                    .map_err(|_| AttestedGuestKeyProofError::TopologyMismatch)?;
                expected.push((
                    AttestedComponent::NvidiaNvSwitch { index },
                    EvidenceProfile::NvidiaNvSwitch,
                ));
            }
        }
        expected.push((
            AttestedComponent::CpuVm,
            self.bundle.request.cpu_profile.evidence_profile(),
        ));
        if self.bundle.evidence.len() != expected.len()
            || self
                .bundle
                .evidence
                .iter()
                .zip(expected)
                .any(|(actual, (component, profile))| {
                    actual.component != component || actual.profile != profile
                })
        {
            return Err(AttestedGuestKeyProofError::TopologyMismatch);
        }

        let child_evidence_digests = self.bundle.evidence[..self.bundle.evidence.len() - 1]
            .iter()
            .map(RawEvidence::digest)
            .collect();
        AttestationBinding::new(
            self.bundle.request.challenge.clone(),
            self.bundle.request.guest_public_key.clone(),
            self.bundle.request.workload_manifest_digest,
            child_evidence_digests,
        )
        .map(|binding| binding.transcript_digest())
        .map_err(|_| AttestedGuestKeyProofError::InvalidTranscript)
    }

    /// Consumes the response and returns the native evidence bundle.
    #[must_use]
    pub fn into_bundle(self) -> GuestAttestationBundle {
        self.bundle
    }
}

/// Failure while checking possession of an evidence-bound guest key.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum AttestedGuestKeyProofError {
    /// The retained components do not match the profile selected by the request.
    #[error("attested evidence does not match the requested component topology")]
    TopologyMismatch,
    /// The retained binding inputs cannot form a canonical transcript.
    #[error("attested evidence cannot form a canonical transcript")]
    InvalidTranscript,
    /// The declared digest does not match the retained binding inputs and child evidence.
    #[error("attested transcript digest does not match the retained evidence")]
    TranscriptMismatch,
    /// The evidence did not bind a 32-byte Ed25519 public key.
    #[error("attested guest public key is not 32-byte Ed25519")]
    InvalidPublicKeyLength,
    /// The bound bytes are not an Ed25519 verification key.
    #[error("attested guest public key is invalid Ed25519")]
    InvalidPublicKey,
    /// The detached proof is not an Ed25519 signature.
    #[error("attested guest key proof is not a 64-byte Ed25519 signature")]
    InvalidSignature,
    /// The proof does not sign the attestation transcript.
    #[error("attested guest key proof does not match the attestation transcript")]
    SignatureMismatch,
}

pub(crate) fn key_proof_message(transcript_digest: &[u8; 32]) -> Vec<u8> {
    let mut message = Vec::with_capacity(KEY_PROOF_DOMAIN.len() + transcript_digest.len());
    message.extend_from_slice(KEY_PROOF_DOMAIN);
    message.extend_from_slice(transcript_digest);
    message
}

impl RawEvidence {
    /// Creates one bounded native evidence object.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or oversized media type or payload.
    pub fn new(
        component: AttestedComponent,
        profile: EvidenceProfile,
        media_type: impl Into<String>,
        bytes: impl Into<Vec<u8>>,
    ) -> Result<Self, AttestationInputError> {
        let media_type = media_type.into();
        let bytes = bytes.into();
        validate_component(&component)?;
        validate_component_profile(&component, profile)?;
        if media_type.is_empty() {
            return Err(AttestationInputError::EmptyMediaType);
        }
        if media_type.len() > MAX_MEDIA_TYPE_BYTES {
            return Err(AttestationInputError::MediaTypeTooLarge {
                actual: media_type.len(),
                maximum: MAX_MEDIA_TYPE_BYTES,
            });
        }
        if !media_type.is_ascii() {
            return Err(AttestationInputError::NonAsciiMediaType);
        }
        if bytes.is_empty() {
            return Err(AttestationInputError::EmptyEvidence);
        }
        if bytes.len() > MAX_RAW_EVIDENCE_BYTES {
            return Err(AttestationInputError::EvidenceTooLarge {
                actual: bytes.len(),
                maximum: MAX_RAW_EVIDENCE_BYTES,
            });
        }
        Ok(Self {
            component,
            profile,
            media_type,
            bytes,
        })
    }

    /// Returns the component which produced the evidence.
    #[must_use]
    pub const fn component(&self) -> &AttestedComponent {
        &self.component
    }

    /// Returns the native evidence profile.
    #[must_use]
    pub const fn profile(&self) -> EvidenceProfile {
        self.profile
    }

    /// Returns the registered or vendor-specific media type.
    #[must_use]
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    /// Returns the exact native evidence bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the SHA-256 digest used when binding child evidence.
    #[must_use]
    pub fn digest(&self) -> [u8; 32] {
        Sha256::digest(&self.bytes).into()
    }
}

/// Rejected bounded input at the attestation protocol boundary.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AttestationInputError {
    /// The challenge nonce was entirely zero.
    #[error("attestation challenge nonce must not be all zero")]
    ZeroNonce,
    /// The policy identifier was empty.
    #[error("attestation policy ID must not be empty")]
    EmptyPolicyId,
    /// The policy identifier exceeded its protocol bound.
    #[error("attestation policy ID is {actual} bytes; maximum is {maximum}")]
    PolicyIdTooLarge {
        /// Supplied size.
        actual: usize,
        /// Protocol maximum.
        maximum: usize,
    },
    /// The absolute challenge expiry was zero.
    #[error("attestation challenge expiry must not be zero")]
    ZeroExpiry,
    /// The guest public key was empty.
    #[error("attested guest public key must not be empty")]
    EmptyGuestPublicKey,
    /// The guest public key exceeded its protocol bound.
    #[error("attested guest public key is {actual} bytes; maximum is {maximum}")]
    GuestPublicKeyTooLarge {
        /// Supplied size.
        actual: usize,
        /// Protocol maximum.
        maximum: usize,
    },
    /// Too many child component digests were supplied.
    #[error("attestation transcript has {actual} child digests; maximum is {maximum}")]
    TooManyChildEvidenceDigests {
        /// Supplied count.
        actual: usize,
        /// Protocol maximum.
        maximum: usize,
    },
    /// A child evidence digest was repeated.
    #[error("attestation transcript contains a duplicate child evidence digest")]
    DuplicateChildEvidenceDigest,
    /// The evidence media type was empty.
    #[error("native evidence media type must not be empty")]
    EmptyMediaType,
    /// The evidence media type was not ASCII.
    #[error("native evidence media type must be ASCII")]
    NonAsciiMediaType,
    /// The evidence media type exceeded its protocol bound.
    #[error("native evidence media type is {actual} bytes; maximum is {maximum}")]
    MediaTypeTooLarge {
        /// Supplied size.
        actual: usize,
        /// Protocol maximum.
        maximum: usize,
    },
    /// The native evidence payload was empty.
    #[error("native evidence payload must not be empty")]
    EmptyEvidence,
    /// The native evidence payload exceeded its protocol bound.
    #[error("native evidence payload is {actual} bytes; maximum is {maximum}")]
    EvidenceTooLarge {
        /// Supplied size.
        actual: usize,
        /// Protocol maximum.
        maximum: usize,
    },
    /// The native evidence profile does not match its component kind.
    #[error("native evidence profile does not match the attested component kind")]
    ComponentProfileMismatch,
}

fn hash_bytes(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

const fn validate_component(_component: &AttestedComponent) -> Result<(), AttestationInputError> {
    Ok(())
}

fn serialize_base64<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&BASE64_STANDARD.encode(bytes))
}

const fn maximum_base64_len(maximum_decoded_len: usize) -> usize {
    maximum_decoded_len.div_ceil(3) * 4
}

fn deserialize_bounded_base64<'de, D>(
    deserializer: D,
    maximum_decoded_len: usize,
) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let encoded = String::deserialize(deserializer)?;
    let maximum_encoded_len = maximum_base64_len(maximum_decoded_len);
    if encoded.len() > maximum_encoded_len {
        return Err(serde::de::Error::custom(format_args!(
            "base64 field is {} bytes; maximum encoded length is {maximum_encoded_len}",
            encoded.len()
        )));
    }
    let decoded = BASE64_STANDARD
        .decode(encoded)
        .map_err(serde::de::Error::custom)?;
    if decoded.len() > maximum_decoded_len {
        return Err(serde::de::Error::custom(format_args!(
            "decoded base64 field is {} bytes; maximum is {maximum_decoded_len}",
            decoded.len()
        )));
    }
    Ok(decoded)
}

fn deserialize_evidence_base64<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_base64(deserializer, MAX_RAW_EVIDENCE_BYTES)
}

fn deserialize_guest_key_base64<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_base64(deserializer, MAX_GUEST_PUBLIC_KEY_BYTES)
}

fn deserialize_ed25519_signature<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let signature = deserialize_bounded_base64(deserializer, 64)?;
    if signature.len() != 64 {
        return Err(serde::de::Error::custom(format_args!(
            "Ed25519 signature is {} bytes; expected 64",
            signature.len()
        )));
    }
    Ok(signature)
}

fn deserialize_bounded_evidence<'de, D>(deserializer: D) -> Result<Vec<RawEvidence>, D::Error>
where
    D: Deserializer<'de>,
{
    struct EvidenceVisitor;

    impl<'de> Visitor<'de> for EvidenceVisitor {
        type Value = Vec<RawEvidence>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                formatter,
                "at most {MAX_BUNDLE_EVIDENCE} native evidence objects"
            )
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut evidence =
                Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(MAX_BUNDLE_EVIDENCE));
            while let Some(item) = sequence.next_element()? {
                if evidence.len() == MAX_BUNDLE_EVIDENCE {
                    return Err(serde::de::Error::custom(format_args!(
                        "attestation bundle has more than {MAX_BUNDLE_EVIDENCE} evidence objects"
                    )));
                }
                evidence.push(item);
            }
            Ok(evidence)
        }
    }

    deserializer.deserialize_seq(EvidenceVisitor)
}

const fn validate_component_profile(
    component: &AttestedComponent,
    profile: EvidenceProfile,
) -> Result<(), AttestationInputError> {
    let matches = match component {
        AttestedComponent::CpuVm => {
            #[cfg(feature = "development-attestation")]
            let matches = matches!(
                profile,
                EvidenceProfile::Development
                    | EvidenceProfile::AmdSevSnp
                    | EvidenceProfile::IntelTdx
                    | EvidenceProfile::AwsNitro
            );
            #[cfg(not(feature = "development-attestation"))]
            let matches = matches!(
                profile,
                EvidenceProfile::AmdSevSnp | EvidenceProfile::IntelTdx | EvidenceProfile::AwsNitro
            );
            matches
        }
        AttestedComponent::NvidiaGpu { .. } => matches!(profile, EvidenceProfile::NvidiaGpu),
        AttestedComponent::NvidiaNvSwitch { .. } => {
            matches!(profile, EvidenceProfile::NvidiaNvSwitch)
        }
    };
    if matches {
        Ok(())
    } else {
        Err(AttestationInputError::ComponentProfileMismatch)
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer as _, SigningKey};

    use super::*;

    fn challenge() -> AttestationChallenge {
        AttestationChallenge::new([0x42; 32], "production-snp-v1", 2_000_000_000).unwrap()
    }

    #[test]
    fn transcript_digest_is_domain_separated_and_stable() {
        let binding = AttestationBinding::new(
            challenge(),
            vec![0x04, 0x11, 0x22, 0x33],
            [0x77; 32],
            vec![[0x88; 32], [0x99; 32]],
        )
        .unwrap();

        assert_eq!(
            hex::encode(binding.transcript_digest()),
            "6db25d05b8bbf1d7cbc88535cd4b5c58f9de823cb23cf236db7bd620e0a0c096"
        );
    }

    #[test]
    fn transcript_rejects_duplicate_child_evidence() {
        assert_eq!(
            AttestationBinding::new(challenge(), vec![1, 2, 3], [4; 32], vec![[5; 32], [5; 32]],)
                .unwrap_err(),
            AttestationInputError::DuplicateChildEvidenceDigest
        );
    }

    #[test]
    fn raw_evidence_is_bounded_and_retained_exactly() {
        let evidence = RawEvidence::new(
            AttestedComponent::CpuVm,
            EvidenceProfile::AmdSevSnp,
            "application/vnd.amd.snp.report",
            [1, 2, 3, 4],
        )
        .unwrap();

        assert_eq!(evidence.bytes(), [1, 2, 3, 4]);
        assert_eq!(evidence.component(), &AttestedComponent::CpuVm);
        assert!(matches!(
            RawEvidence::new(
                AttestedComponent::CpuVm,
                EvidenceProfile::AmdSevSnp,
                "application/octet-stream",
                vec![0; MAX_RAW_EVIDENCE_BYTES + 1],
            ),
            Err(AttestationInputError::EvidenceTooLarge { .. })
        ));
    }

    #[test]
    fn raw_evidence_json_is_bounded_base64_and_revalidated() {
        let evidence = RawEvidence::new(
            AttestedComponent::CpuVm,
            EvidenceProfile::IntelTdx,
            "application/octet-stream",
            [1, 2, 3, 4],
        )
        .unwrap();
        let encoded = serde_json::to_value(&evidence).unwrap();

        assert_eq!(encoded["bytes"], "AQIDBA==");
        assert_eq!(
            serde_json::from_value::<RawEvidence>(encoded).unwrap(),
            evidence
        );
        assert!(
            serde_json::from_value::<RawEvidence>(serde_json::json!({
                "component": { "kind": "cpu_vm" },
                "profile": "intel_tdx",
                "media_type": "application/octet-stream",
                "bytes": ""
            }))
            .is_err()
        );
    }

    #[test]
    fn challenge_and_profile_reject_invalid_inputs() {
        assert_eq!(
            AttestationChallenge::new([0; 32], "policy", 1).unwrap_err(),
            AttestationInputError::ZeroNonce
        );
        assert_eq!(
            RawEvidence::new(
                AttestedComponent::CpuVm,
                EvidenceProfile::NvidiaGpu,
                "application/octet-stream",
                [1],
            )
            .unwrap_err(),
            AttestationInputError::ComponentProfileMismatch
        );
    }

    #[test]
    fn serde_revalidates_challenges_requests_and_encoded_bounds() {
        assert!(
            serde_json::from_value::<AttestationChallenge>(serde_json::json!({
                "nonce": vec![0; 32],
                "policy_id": "policy",
                "expires_at_unix_seconds": 1
            }))
            .is_err()
        );

        let oversized_key = BASE64_STANDARD.encode(vec![1; MAX_GUEST_PUBLIC_KEY_BYTES + 1]);
        assert!(
            serde_json::from_value::<GuestAttestationRequest>(serde_json::json!({
                "challenge": challenge(),
                "guest_public_key": oversized_key,
                "workload_manifest_digest": vec![1; 32],
                "cpu_profile": "amd_sev_snp",
                "nvidia_profile": null
            }))
            .is_err()
        );

        let oversized_evidence = "A".repeat(maximum_base64_len(MAX_RAW_EVIDENCE_BYTES) + 1);
        assert!(
            serde_json::from_value::<RawEvidence>(serde_json::json!({
                "component": { "kind": "cpu_vm" },
                "profile": "amd_sev_snp",
                "media_type": "application/octet-stream",
                "bytes": oversized_evidence
            }))
            .is_err()
        );
    }

    #[test]
    fn bundle_deserialization_bounds_component_count() {
        let request = GuestAttestationRequest::new(
            challenge(),
            vec![1, 2, 3],
            [4; 32],
            CpuAttestationProfile::AmdSevSnp,
            None,
        )
        .unwrap();
        let item = RawEvidence::new(
            AttestedComponent::CpuVm,
            EvidenceProfile::AmdSevSnp,
            "application/octet-stream",
            [1],
        )
        .unwrap();
        let encoded = serde_json::json!({
            "request": request,
            "transcript_digest": vec![0; 32],
            "evidence": vec![item; MAX_BUNDLE_EVIDENCE + 1],
        });

        assert!(serde_json::from_value::<GuestAttestationBundle>(encoded).is_err());
    }

    #[test]
    fn guest_key_proof_signs_the_evidence_bound_transcript() {
        let signing_key = SigningKey::from_bytes(&[0x17; 32]);
        let parameters = GuestAttestationParameters::new(
            challenge(),
            [0x23; 32],
            CpuAttestationProfile::AmdSevSnp,
            None,
        );
        let request = parameters
            .into_request(signing_key.verifying_key().to_bytes().to_vec())
            .unwrap();
        let transcript_digest = request.binding(Vec::new()).unwrap().transcript_digest();
        let evidence = RawEvidence::new(
            AttestedComponent::CpuVm,
            EvidenceProfile::AmdSevSnp,
            "application/vnd.amd.snp.report",
            [0x42],
        )
        .unwrap();
        let bundle = GuestAttestationBundle::new(request, transcript_digest, vec![evidence]);
        let signature = signing_key
            .sign(&key_proof_message(&transcript_digest))
            .to_bytes();
        let attestation = GuestAttestation::new(bundle, AttestedGuestKeyProof::new(signature));

        attestation.verify_key_proof().unwrap();
        let mut encoded = serde_json::to_value(&attestation).unwrap();
        encoded["bundle"]["request"]["workload_manifest_digest"][0] = serde_json::json!(0xff);
        let changed: GuestAttestation = serde_json::from_value(encoded).unwrap();
        assert_eq!(
            changed.verify_key_proof(),
            Err(AttestedGuestKeyProofError::TranscriptMismatch)
        );

        let mut encoded = serde_json::to_value(&attestation).unwrap();
        encoded["bundle"]["transcript_digest"][0] = serde_json::json!(0xff);
        let changed: GuestAttestation = serde_json::from_value(encoded).unwrap();
        assert_eq!(
            changed.verify_key_proof(),
            Err(AttestedGuestKeyProofError::TranscriptMismatch)
        );
    }
}
