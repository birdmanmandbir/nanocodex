use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const TRANSCRIPT_DOMAIN: &[u8] = b"nanocodex-vm-attestation-transcript\0";
const TRANSCRIPT_VERSION: u32 = 1;
const MAX_POLICY_ID_BYTES: usize = 256;
const MAX_GUEST_PUBLIC_KEY_BYTES: usize = 4 * 1024;
const MAX_CHILD_EVIDENCE: usize = 256;
const MAX_MEDIA_TYPE_BYTES: usize = 128;
const MAX_COMPONENT_ID_BYTES: usize = 256;

/// Maximum native evidence size accepted for one attested component.
pub const MAX_RAW_EVIDENCE_BYTES: usize = 16 * 1024 * 1024;

/// Fresh relying-party input which native hardware evidence must bind.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttestationChallenge {
    nonce: [u8; 32],
    policy_id: String,
    expires_at_unix_seconds: u64,
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
    pub const fn challenge(&self) -> &AttestationChallenge {
        &self.challenge
    }

    /// Returns the guest-generated public key.
    #[must_use]
    pub fn guest_public_key(&self) -> &[u8] {
        &self.guest_public_key
    }

    /// Returns the expected measured-workload manifest digest.
    #[must_use]
    pub const fn workload_manifest_digest(&self) -> &[u8; 32] {
        &self.workload_manifest_digest
    }

    /// Returns child evidence digests in canonical component order.
    #[must_use]
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

/// Stable identity of one independently appraised component.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AttestedComponent {
    /// Parent confidential CPU VM or enclave.
    CpuVm,
    /// One GPU identified by its canonical device UUID.
    NvidiaGpu {
        /// Vendor device UUID retained without normalization.
        uuid: String,
    },
    /// One NVSwitch identified by its canonical device UUID.
    NvidiaNvSwitch {
        /// Vendor device UUID retained without normalization.
        uuid: String,
    },
}

/// Bounded native hardware evidence retained byte-for-byte.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawEvidence {
    component: AttestedComponent,
    profile: EvidenceProfile,
    media_type: String,
    bytes: Vec<u8>,
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
    /// A device component identifier was empty.
    #[error("attested device component identifier must not be empty")]
    EmptyComponentId,
    /// A device component identifier was not ASCII.
    #[error("attested device component identifier must be ASCII")]
    NonAsciiComponentId,
    /// A device component identifier exceeded its protocol bound.
    #[error("attested device component identifier is {actual} bytes; maximum is {maximum}")]
    ComponentIdTooLarge {
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

fn validate_component(component: &AttestedComponent) -> Result<(), AttestationInputError> {
    let (AttestedComponent::NvidiaGpu { uuid } | AttestedComponent::NvidiaNvSwitch { uuid }) =
        component
    else {
        return Ok(());
    };
    if uuid.is_empty() {
        return Err(AttestationInputError::EmptyComponentId);
    }
    if uuid.len() > MAX_COMPONENT_ID_BYTES {
        return Err(AttestationInputError::ComponentIdTooLarge {
            actual: uuid.len(),
            maximum: MAX_COMPONENT_ID_BYTES,
        });
    }
    if !uuid.is_ascii() {
        return Err(AttestationInputError::NonAsciiComponentId);
    }
    Ok(())
}

const fn validate_component_profile(
    component: &AttestedComponent,
    profile: EvidenceProfile,
) -> Result<(), AttestationInputError> {
    let matches = match component {
        AttestedComponent::CpuVm => matches!(
            profile,
            EvidenceProfile::AmdSevSnp | EvidenceProfile::IntelTdx | EvidenceProfile::AwsNitro
        ),
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
    fn challenge_and_component_id_reject_ambient_identifiers() {
        assert_eq!(
            AttestationChallenge::new([0; 32], "policy", 1).unwrap_err(),
            AttestationInputError::ZeroNonce
        );
        assert_eq!(
            RawEvidence::new(
                AttestedComponent::NvidiaGpu {
                    uuid: String::new(),
                },
                EvidenceProfile::NvidiaGpu,
                "application/octet-stream",
                [1],
            )
            .unwrap_err(),
            AttestationInputError::EmptyComponentId
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
}
