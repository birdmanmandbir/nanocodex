use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::measured_guest::ManifestSha256;

const ENTRY_VERSION: u32 = 1;
const SIGNATURE_DOMAIN: &[u8] = b"nanocodex-vm-deployment-authorization-v1\0";
const ENTRY_DIGEST_DOMAIN: &[u8] = b"nanocodex-vm-deployment-history-entry-v1\0";
const MAX_HISTORY_BYTES: usize = 4 * 1024 * 1024;
const MAX_HISTORY_ENTRIES: usize = 4096;
const MAX_RELEASE_ID_BYTES: usize = 255;

/// Authorization state assigned to one measured deployment manifest.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentAuthorizationAction {
    /// Permits relying parties to release new plaintext after `effective_unix_seconds`.
    Authorize,
    /// Prevents relying parties from starting new plaintext sessions.
    Withdraw,
}

/// One signed, hash-chained manifest authorization or withdrawal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentHistoryEntry {
    version: u32,
    sequence: u64,
    previous_entry_sha256: ManifestSha256,
    manifest_sha256: ManifestSha256,
    release_id: String,
    action: DeploymentAuthorizationAction,
    effective_unix_seconds: u64,
    #[serde(with = "signature_base64")]
    signature: Vec<u8>,
}

#[derive(Serialize)]
struct UnsignedEntry<'a> {
    version: u32,
    sequence: u64,
    previous_entry_sha256: ManifestSha256,
    manifest_sha256: ManifestSha256,
    release_id: &'a str,
    action: DeploymentAuthorizationAction,
    effective_unix_seconds: u64,
}

impl DeploymentHistoryEntry {
    /// Signs one append-only authorization entry.
    ///
    /// `previous_entry_sha256` is SHA-256 of the empty string only for sequence
    /// zero. Every later entry must use the digest returned by
    /// [`Self::entry_sha256`] for its immediate predecessor.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid release identifier or chain position.
    pub fn sign(
        sequence: u64,
        previous_entry_sha256: ManifestSha256,
        manifest_sha256: ManifestSha256,
        release_id: impl Into<String>,
        action: DeploymentAuthorizationAction,
        effective_unix_seconds: u64,
        signing_key: &SigningKey,
    ) -> Result<Self, DeploymentHistoryError> {
        let release_id = release_id.into();
        validate_release_id(&release_id)?;
        validate_chain_position(sequence, previous_entry_sha256)?;
        let unsigned = UnsignedEntry {
            version: ENTRY_VERSION,
            sequence,
            previous_entry_sha256,
            manifest_sha256,
            release_id: &release_id,
            action,
            effective_unix_seconds,
        };
        let message = signature_message(&unsigned)?;
        Ok(Self {
            version: ENTRY_VERSION,
            sequence,
            previous_entry_sha256,
            manifest_sha256,
            release_id,
            action,
            effective_unix_seconds,
            signature: signing_key.sign(&message).to_bytes().to_vec(),
        })
    }

    /// Returns the sequence number in the public history.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the authorized or withdrawn measured manifest.
    #[must_use]
    pub const fn manifest_sha256(&self) -> ManifestSha256 {
        self.manifest_sha256
    }

    /// Returns the human-readable immutable release identifier.
    #[must_use]
    pub fn release_id(&self) -> &str {
        &self.release_id
    }

    /// Returns this entry's authorization action.
    #[must_use]
    pub const fn action(&self) -> DeploymentAuthorizationAction {
        self.action
    }

    /// Returns when relying parties should apply this entry.
    #[must_use]
    pub const fn effective_unix_seconds(&self) -> u64 {
        self.effective_unix_seconds
    }

    /// Returns the domain-separated digest used by the next entry and public checkpoint.
    ///
    /// # Errors
    ///
    /// Returns an error only if canonical JSON serialization fails.
    pub fn entry_sha256(&self) -> Result<ManifestSha256, DeploymentHistoryError> {
        let encoded = serde_json::to_vec(self).map_err(DeploymentHistoryError::Json)?;
        let mut hasher = Sha256::new();
        hasher.update(ENTRY_DIGEST_DOMAIN);
        hasher.update(encoded);
        Ok(ManifestSha256::from_bytes(hasher.finalize().into()))
    }

    fn verify(&self, verifying_key: &VerifyingKey) -> Result<(), DeploymentHistoryError> {
        if self.version != ENTRY_VERSION {
            return Err(DeploymentHistoryError::UnsupportedVersion(self.version));
        }
        validate_release_id(&self.release_id)?;
        validate_chain_position(self.sequence, self.previous_entry_sha256)?;
        let signature = Signature::from_slice(&self.signature)
            .map_err(|_| DeploymentHistoryError::InvalidSignature)?;
        let unsigned = UnsignedEntry {
            version: self.version,
            sequence: self.sequence,
            previous_entry_sha256: self.previous_entry_sha256,
            manifest_sha256: self.manifest_sha256,
            release_id: &self.release_id,
            action: self.action,
            effective_unix_seconds: self.effective_unix_seconds,
        };
        verifying_key
            .verify(&signature_message(&unsigned)?, &signature)
            .map_err(|_| DeploymentHistoryError::SignatureMismatch)
    }
}

/// A verified history whose exact head was pinned by a transparency checkpoint.
#[derive(Clone, Debug)]
pub struct VerifiedDeploymentHistory {
    entries: Vec<DeploymentHistoryEntry>,
    head_sha256: ManifestSha256,
}

impl VerifiedDeploymentHistory {
    /// Verifies signed JSONL, its hash chain, and a separately pinned public head.
    ///
    /// Requiring `expected_head_sha256` prevents a valid prefix from being
    /// treated as the current history. Callers obtain that head from their
    /// reviewed release policy or an append-only transparency checkpoint.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, oversized, unsigned, reordered,
    /// truncated, or forked history.
    pub fn from_jsonl(
        bytes: &[u8],
        authorization_public_key: [u8; 32],
        expected_head_sha256: ManifestSha256,
    ) -> Result<Self, DeploymentHistoryError> {
        if bytes.len() > MAX_HISTORY_BYTES {
            return Err(DeploymentHistoryError::HistoryTooLarge(bytes.len()));
        }
        let verifying_key = VerifyingKey::from_bytes(&authorization_public_key)
            .map_err(|_| DeploymentHistoryError::InvalidPublicKey)?;
        let mut entries: Vec<DeploymentHistoryEntry> = Vec::new();
        let line_count = bytes.split(|byte| *byte == b'\n').count();
        for (line_index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
            if line.is_empty() {
                if line_index + 1 == line_count {
                    continue;
                }
                return Err(DeploymentHistoryError::EmptyLine(line_index + 1));
            }
            if entries.len() >= MAX_HISTORY_ENTRIES {
                return Err(DeploymentHistoryError::TooManyEntries);
            }
            entries.push(serde_json::from_slice(line).map_err(DeploymentHistoryError::Json)?);
        }
        if entries.is_empty() {
            return Err(DeploymentHistoryError::EmptyHistory);
        }
        let mut previous = ManifestSha256::digest([]);
        let mut last_effective = 0;
        for (index, entry) in entries.iter().enumerate() {
            entry.verify(&verifying_key)?;
            let expected_sequence =
                u64::try_from(index).map_err(|_| DeploymentHistoryError::TooManyEntries)?;
            if entry.sequence != expected_sequence {
                return Err(DeploymentHistoryError::SequenceMismatch {
                    expected: expected_sequence,
                    actual: entry.sequence,
                });
            }
            if index > 0 && entry.previous_entry_sha256 != previous {
                return Err(DeploymentHistoryError::PreviousDigestMismatch(
                    entry.sequence,
                ));
            }
            if entry.effective_unix_seconds < last_effective {
                return Err(DeploymentHistoryError::EffectiveTimeRegression(
                    entry.sequence,
                ));
            }
            previous = entry.entry_sha256()?;
            last_effective = entry.effective_unix_seconds;
        }
        if previous != expected_head_sha256 {
            return Err(DeploymentHistoryError::HeadMismatch);
        }
        Ok(Self {
            entries,
            head_sha256: previous,
        })
    }

    /// Returns the separately pinned and verified history head.
    #[must_use]
    pub const fn head_sha256(&self) -> ManifestSha256 {
        self.head_sha256
    }

    /// Returns every verified history entry in sequence order.
    #[must_use]
    pub fn entries(&self) -> &[DeploymentHistoryEntry] {
        &self.entries
    }

    /// Requires this manifest's latest effective entry to authorize new plaintext.
    ///
    /// # Errors
    ///
    /// Returns an error if no authorization is effective at the relying
    /// party's trusted time or the latest effective action is withdrawal.
    pub fn require_authorized(
        &self,
        manifest_sha256: ManifestSha256,
        now_unix_seconds: u64,
    ) -> Result<&DeploymentHistoryEntry, DeploymentHistoryError> {
        let mut latest_by_manifest = BTreeMap::new();
        for entry in &self.entries {
            if entry.effective_unix_seconds <= now_unix_seconds {
                latest_by_manifest.insert(entry.manifest_sha256, entry);
            }
        }
        let entry = latest_by_manifest
            .get(&manifest_sha256)
            .copied()
            .ok_or(DeploymentHistoryError::ManifestNotAuthorized)?;
        if entry.action != DeploymentAuthorizationAction::Authorize {
            return Err(DeploymentHistoryError::ManifestWithdrawn);
        }
        Ok(entry)
    }
}

/// Invalid deployment authorization or history.
#[derive(Debug, Error)]
pub enum DeploymentHistoryError {
    /// The JSONL input exceeded its fixed public bound.
    #[error("deployment history is {0} bytes; maximum is {MAX_HISTORY_BYTES}")]
    HistoryTooLarge(usize),
    /// The history contained no entries.
    #[error("deployment history is empty")]
    EmptyHistory,
    /// The history exceeded its fixed entry bound.
    #[error("deployment history exceeds {MAX_HISTORY_ENTRIES} entries")]
    TooManyEntries,
    /// A nonterminal JSONL line was empty.
    #[error("deployment history line {0} is empty")]
    EmptyLine(usize),
    /// An entry used an unsupported schema version.
    #[error("unsupported deployment history entry version {0}")]
    UnsupportedVersion(u32),
    /// A release identifier was malformed.
    #[error("release ID must contain 1 to {MAX_RELEASE_ID_BYTES} portable bytes")]
    InvalidReleaseId,
    /// Genesis or a later entry used an invalid chain predecessor.
    #[error("deployment history sequence and previous digest are inconsistent")]
    InvalidChainPosition,
    /// The authorization root key was malformed.
    #[error("deployment authorization public key is invalid")]
    InvalidPublicKey,
    /// An entry's signature encoding was malformed.
    #[error("deployment authorization signature is invalid")]
    InvalidSignature,
    /// An entry was not signed by the pinned authorization key.
    #[error("deployment authorization signature does not match the pinned key")]
    SignatureMismatch,
    /// Sequence did not match physical JSONL order.
    #[error("deployment history sequence is {actual}; expected {expected}")]
    SequenceMismatch {
        /// Sequence required by physical JSONL order.
        expected: u64,
        /// Sequence encoded in the signed entry.
        actual: u64,
    },
    /// A hash-chain link did not identify the immediate predecessor.
    #[error("deployment history entry {0} does not identify its predecessor")]
    PreviousDigestMismatch(u64),
    /// Effective authorization time moved backwards.
    #[error("deployment history entry {0} regresses effective time")]
    EffectiveTimeRegression(u64),
    /// The verified history did not reach the separately pinned checkpoint.
    #[error("deployment history head does not match the pinned transparency checkpoint")]
    HeadMismatch,
    /// No authorization was effective for this manifest.
    #[error("measured deployment manifest is not authorized")]
    ManifestNotAuthorized,
    /// The manifest's latest effective action was withdrawal.
    #[error("measured deployment manifest has been withdrawn")]
    ManifestWithdrawn,
    /// Canonical JSON encoding or decoding failed.
    #[error("invalid deployment history JSON: {0}")]
    Json(serde_json::Error),
}

fn validate_release_id(release_id: &str) -> Result<(), DeploymentHistoryError> {
    if release_id.is_empty()
        || release_id.len() > MAX_RELEASE_ID_BYTES
        || !release_id.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'/' | b':')
        })
    {
        Err(DeploymentHistoryError::InvalidReleaseId)
    } else {
        Ok(())
    }
}

fn validate_chain_position(
    sequence: u64,
    previous_entry_sha256: ManifestSha256,
) -> Result<(), DeploymentHistoryError> {
    let genesis = ManifestSha256::digest([]);
    if (sequence == 0) == (previous_entry_sha256 == genesis) {
        Ok(())
    } else {
        Err(DeploymentHistoryError::InvalidChainPosition)
    }
}

fn signature_message(unsigned: &UnsignedEntry<'_>) -> Result<Vec<u8>, DeploymentHistoryError> {
    let encoded = serde_json::to_vec(unsigned).map_err(DeploymentHistoryError::Json)?;
    let mut message = Vec::with_capacity(SIGNATURE_DOMAIN.len() + encoded.len());
    message.extend_from_slice(SIGNATURE_DOMAIN);
    message.extend_from_slice(&encoded);
    Ok(message)
}

mod signature_base64 {
    use super::*;

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&BASE64_STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() > 88 {
            return Err(serde::de::Error::custom("signature exceeds 64 bytes"));
        }
        let decoded = BASE64_STANDARD
            .decode(encoded)
            .map_err(serde::de::Error::custom)?;
        if decoded.len() != 64 {
            return Err(serde::de::Error::custom("signature must be 64 bytes"));
        }
        Ok(decoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(entries: &[DeploymentHistoryEntry]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for entry in entries {
            serde_json::to_writer(&mut bytes, entry).unwrap();
            bytes.push(b'\n');
        }
        bytes
    }

    #[test]
    fn verifies_authorization_withdrawal_and_pinned_head() {
        let key = SigningKey::from_bytes(&[0x42; 32]);
        let manifest = ManifestSha256::digest(b"manifest-a");
        let first = DeploymentHistoryEntry::sign(
            0,
            ManifestSha256::digest([]),
            manifest,
            "release/1",
            DeploymentAuthorizationAction::Authorize,
            100,
            &key,
        )
        .unwrap();
        let second = DeploymentHistoryEntry::sign(
            1,
            first.entry_sha256().unwrap(),
            manifest,
            "release/1-withdrawn",
            DeploymentAuthorizationAction::Withdraw,
            200,
            &key,
        )
        .unwrap();
        let head = second.entry_sha256().unwrap();
        let verified = VerifiedDeploymentHistory::from_jsonl(
            &history(&[first.clone(), second]),
            key.verifying_key().to_bytes(),
            head,
        )
        .unwrap();

        assert_eq!(verified.require_authorized(manifest, 150).unwrap(), &first);
        assert!(matches!(
            verified.require_authorized(manifest, 250),
            Err(DeploymentHistoryError::ManifestWithdrawn)
        ));
        assert!(matches!(
            VerifiedDeploymentHistory::from_jsonl(
                &history(&[first]),
                key.verifying_key().to_bytes(),
                head,
            ),
            Err(DeploymentHistoryError::HeadMismatch)
        ));
    }

    #[test]
    fn rejects_forks_reordering_and_signature_tampering() {
        let key = SigningKey::from_bytes(&[0x24; 32]);
        let first = DeploymentHistoryEntry::sign(
            0,
            ManifestSha256::digest([]),
            ManifestSha256::digest(b"manifest-a"),
            "release/1",
            DeploymentAuthorizationAction::Authorize,
            100,
            &key,
        )
        .unwrap();
        let second = DeploymentHistoryEntry::sign(
            1,
            first.entry_sha256().unwrap(),
            ManifestSha256::digest(b"manifest-b"),
            "release/2",
            DeploymentAuthorizationAction::Authorize,
            200,
            &key,
        )
        .unwrap();
        let head = second.entry_sha256().unwrap();

        assert!(
            VerifiedDeploymentHistory::from_jsonl(
                &history(&[second.clone(), first.clone()]),
                key.verifying_key().to_bytes(),
                head,
            )
            .is_err()
        );

        let mut forked = second.clone();
        forked.previous_entry_sha256 = ManifestSha256::digest(b"fork");
        assert!(matches!(
            VerifiedDeploymentHistory::from_jsonl(
                &history(&[first.clone(), forked]),
                key.verifying_key().to_bytes(),
                head,
            ),
            Err(DeploymentHistoryError::SignatureMismatch)
        ));

        let mut tampered = first;
        tampered.release_id.push('x');
        assert!(matches!(
            VerifiedDeploymentHistory::from_jsonl(
                &history(&[tampered, second]),
                key.verifying_key().to_bytes(),
                head,
            ),
            Err(DeploymentHistoryError::SignatureMismatch)
        ));
    }
}
