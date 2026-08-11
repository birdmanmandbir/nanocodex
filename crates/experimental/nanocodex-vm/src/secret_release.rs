use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use chacha20poly1305::{
    KeyInit as _, XChaCha20Poly1305, XNonce,
    aead::{Aead as _, Payload},
};
use hkdf::Hkdf;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};
use sha2_10::Sha256 as HkdfSha256;
use thiserror::Error;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::{Zeroize, Zeroizing};

#[cfg(feature = "host")]
use crate::command_proof::AttestedCommand;
use crate::command_proof::AttestedCommandRequest;
#[cfg(feature = "host")]
use crate::verification::VerifiedAttestation;

const ENVELOPE_DOMAIN: &[u8] = b"nanocodex-vm-secret-release\0";
const ENVELOPE_VERSION: u32 = 1;
/// Maximum secret bytes accepted by one attestation-gated release envelope.
pub const MAX_SECRET_RELEASE_BYTES: usize = 64 * 1024;
const MAX_CIPHERTEXT_BYTES: usize = 1024 * 1024;
#[cfg(all(feature = "guest-runtime", target_os = "linux"))]
pub(crate) const MAX_OPENED_SECRET_RELEASES: usize = 1024;

/// An AEAD envelope decryptable only by the evidence-bound guest identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretReleaseEnvelope {
    version: u32,
    recipient_key_sha256: [u8; 32],
    ephemeral_public_key: [u8; 32],
    nonce: [u8; 24],
    #[serde(with = "ciphertext_base64")]
    ciphertext: Vec<u8>,
}

impl SecretReleaseEnvelope {
    #[cfg(all(feature = "guest-runtime", target_os = "linux"))]
    pub(crate) fn digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(ENVELOPE_DOMAIN);
        hasher.update(self.version.to_be_bytes());
        hasher.update(self.recipient_key_sha256);
        hasher.update(self.ephemeral_public_key);
        hasher.update(self.nonce);
        hasher.update((self.ciphertext.len() as u64).to_be_bytes());
        hasher.update(&self.ciphertext);
        hasher.finalize().into()
    }
}

/// Failure while sealing or opening an attestation-gated secret.
#[derive(Debug, Error)]
pub enum SecretReleaseError {
    /// The relying-party challenge expired before release authorization.
    #[error("attestation challenge expired at {expires_at}; release time is {now}")]
    ChallengeExpired {
        /// Challenge expiry as Unix seconds.
        expires_at: u64,
        /// Relying-party trusted release time as Unix seconds.
        now: u64,
    },
    /// The verified guest used the legacy signing-only key format.
    #[error("verified attestation does not contain an X25519 secret-release key")]
    MissingEncryptionKey,
    /// The evidence-bound key encoding is malformed.
    #[error("verified attestation contains a malformed guest key bundle")]
    InvalidGuestKey,
    /// The supplied secret exceeded the fixed release bound.
    #[error("secret is {actual} bytes; maximum is {maximum}")]
    SecretTooLarge {
        /// Supplied byte length.
        actual: usize,
        /// Protocol maximum.
        maximum: usize,
    },
    /// The envelope ciphertext exceeded the fixed protocol bound.
    #[error("secret-release ciphertext is {actual} bytes; maximum is {maximum}")]
    CiphertextTooLarge {
        /// Supplied byte length.
        actual: usize,
        /// Protocol maximum.
        maximum: usize,
    },
    /// Secure random generation failed.
    #[error("failed to generate secret-release randomness: {0}")]
    Random(#[from] getrandom::Error),
    /// The authorized request could not be encoded.
    #[error("failed to encode secret-release policy: {0}")]
    Encode(serde_json::Error),
    /// The decrypted request was malformed or violated a bound.
    #[error("failed to decode secret-release policy: {0}")]
    Decode(serde_json::Error),
    /// X25519 produced an invalid all-zero shared secret.
    #[error("secret-release X25519 key agreement produced an all-zero secret")]
    InvalidSharedSecret,
    /// The envelope is addressed to a different attested identity.
    #[error("secret-release envelope recipient does not match this guest identity")]
    WrongRecipient,
    /// This retained guest identity already opened the exact envelope.
    #[error("secret-release envelope has already been consumed")]
    Replay,
    /// The retained identity exhausted its bounded one-time release ledger.
    #[error("secret-release identity reached its {0}-envelope lifetime limit")]
    ReleaseLimit(usize),
    /// AEAD encryption failed.
    #[error("failed to encrypt secret-release envelope")]
    Encrypt,
    /// AEAD authentication or decryption failed.
    #[error("secret-release envelope authentication failed")]
    Decrypt,
    /// HKDF could not derive the fixed-size AEAD key.
    #[error("failed to derive secret-release AEAD key")]
    KeyDerivation,
    /// The plaintext carries an unsupported protocol version.
    #[error("unsupported secret-release envelope version {0}")]
    UnsupportedVersion(u32),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SecretReleasePayload {
    request: AttestedCommandRequest,
    executable_sha256: [u8; 32],
    secret: Vec<u8>,
}

impl Drop for SecretReleasePayload {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

#[cfg(any(test, all(feature = "guest-runtime", target_os = "linux")))]
pub(crate) struct OpenedSecretRelease {
    request: AttestedCommandRequest,
    executable_sha256: [u8; 32],
    secret: Zeroizing<Vec<u8>>,
}

#[cfg(any(test, all(feature = "guest-runtime", target_os = "linux")))]
impl OpenedSecretRelease {
    pub(crate) fn into_parts(self) -> (AttestedCommandRequest, [u8; 32], Zeroizing<Vec<u8>>) {
        (self.request, self.executable_sha256, self.secret)
    }
}

/// Encrypts a secret and exact command policy to an already-verified guest.
///
/// The guest will accept the envelope only under the same retained identity,
/// execute the exact expected static ELF bytes and `argv`, feed the secret on
/// stdin, and return an ordinary signed command proof. `now_unix_seconds` must
/// come from the relying party's trusted clock and prevents release after the
/// challenge expires.
#[cfg(feature = "host")]
pub fn seal_secret(
    verified: &VerifiedAttestation,
    now_unix_seconds: u64,
    command: AttestedCommand,
    executable_sha256: [u8; 32],
    secret: &[u8],
) -> Result<SecretReleaseEnvelope, SecretReleaseError> {
    let expires_at = verified
        .bundle()
        .request()
        .challenge()
        .expires_at_unix_seconds();
    if now_unix_seconds > expires_at {
        return Err(SecretReleaseError::ChallengeExpired {
            expires_at,
            now: now_unix_seconds,
        });
    }
    if secret.len() > MAX_SECRET_RELEASE_BYTES {
        return Err(SecretReleaseError::SecretTooLarge {
            actual: secret.len(),
            maximum: MAX_SECRET_RELEASE_BYTES,
        });
    }
    let recipient = verified
        .bundle()
        .request()
        .guest_encryption_public_key()
        .map_err(|_| SecretReleaseError::InvalidGuestKey)?
        .ok_or(SecretReleaseError::MissingEncryptionKey)?;
    let payload = SecretReleasePayload {
        request: AttestedCommandRequest::new(verified.bundle().request().parameters(), command),
        executable_sha256,
        secret: secret.to_vec(),
    };
    let plaintext =
        Zeroizing::new(serde_json::to_vec(&payload).map_err(SecretReleaseError::Encode)?);
    seal_to_recipient(recipient, &plaintext)
}

#[cfg(any(feature = "host", test))]
fn seal_to_recipient(
    recipient: &[u8; 32],
    plaintext: &[u8],
) -> Result<SecretReleaseEnvelope, SecretReleaseError> {
    let mut ephemeral_bytes = [0_u8; 32];
    let mut nonce = [0_u8; 24];
    getrandom::fill(&mut ephemeral_bytes)?;
    getrandom::fill(&mut nonce)?;
    let ephemeral_secret = StaticSecret::from(ephemeral_bytes);
    ephemeral_bytes.zeroize();
    let ephemeral_public_key = PublicKey::from(&ephemeral_secret).to_bytes();
    let shared = ephemeral_secret.diffie_hellman(&PublicKey::from(*recipient));
    if shared.as_bytes() == &[0; 32] {
        return Err(SecretReleaseError::InvalidSharedSecret);
    }
    let recipient_key_sha256 = Sha256::digest(recipient).into();
    let aad = associated_data(&recipient_key_sha256, &ephemeral_public_key, &nonce);
    let key = Zeroizing::new(derive_key(shared.as_bytes(), &aad)?);
    let ciphertext = XChaCha20Poly1305::new((&*key).into())
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| SecretReleaseError::Encrypt)?;
    if ciphertext.len() > MAX_CIPHERTEXT_BYTES {
        return Err(SecretReleaseError::CiphertextTooLarge {
            actual: ciphertext.len(),
            maximum: MAX_CIPHERTEXT_BYTES,
        });
    }
    Ok(SecretReleaseEnvelope {
        version: ENVELOPE_VERSION,
        recipient_key_sha256,
        ephemeral_public_key,
        nonce,
        ciphertext,
    })
}

#[cfg(any(test, all(feature = "guest-runtime", target_os = "linux")))]
pub(crate) fn open_secret(
    envelope: &SecretReleaseEnvelope,
    recipient_secret: &StaticSecret,
) -> Result<OpenedSecretRelease, SecretReleaseError> {
    if envelope.version != ENVELOPE_VERSION {
        return Err(SecretReleaseError::UnsupportedVersion(envelope.version));
    }
    if envelope.ciphertext.len() > MAX_CIPHERTEXT_BYTES {
        return Err(SecretReleaseError::CiphertextTooLarge {
            actual: envelope.ciphertext.len(),
            maximum: MAX_CIPHERTEXT_BYTES,
        });
    }
    let recipient_public = PublicKey::from(recipient_secret).to_bytes();
    if Sha256::digest(recipient_public).as_slice() != envelope.recipient_key_sha256 {
        return Err(SecretReleaseError::WrongRecipient);
    }
    let shared = recipient_secret.diffie_hellman(&PublicKey::from(envelope.ephemeral_public_key));
    if shared.as_bytes() == &[0; 32] {
        return Err(SecretReleaseError::InvalidSharedSecret);
    }
    let aad = associated_data(
        &envelope.recipient_key_sha256,
        &envelope.ephemeral_public_key,
        &envelope.nonce,
    );
    let key = Zeroizing::new(derive_key(shared.as_bytes(), &aad)?);
    let plaintext = XChaCha20Poly1305::new((&*key).into())
        .decrypt(
            XNonce::from_slice(&envelope.nonce),
            Payload {
                msg: &envelope.ciphertext,
                aad: &aad,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| SecretReleaseError::Decrypt)?;
    let mut payload: SecretReleasePayload =
        serde_json::from_slice(&plaintext).map_err(SecretReleaseError::Decode)?;
    if payload.secret.len() > MAX_SECRET_RELEASE_BYTES {
        return Err(SecretReleaseError::SecretTooLarge {
            actual: payload.secret.len(),
            maximum: MAX_SECRET_RELEASE_BYTES,
        });
    }
    Ok(OpenedSecretRelease {
        request: payload.request.clone(),
        executable_sha256: payload.executable_sha256,
        secret: Zeroizing::new(std::mem::take(&mut payload.secret)),
    })
}

fn derive_key(shared: &[u8; 32], aad: &[u8]) -> Result<[u8; 32], SecretReleaseError> {
    let salt = Sha256::digest(aad);
    let hkdf = Hkdf::<HkdfSha256>::new(Some(&salt), shared);
    let mut key = [0_u8; 32];
    hkdf.expand(ENVELOPE_DOMAIN, &mut key)
        .map_err(|_| SecretReleaseError::KeyDerivation)?;
    Ok(key)
}

fn associated_data(
    recipient_key_sha256: &[u8; 32],
    ephemeral_public_key: &[u8; 32],
    nonce: &[u8; 24],
) -> Vec<u8> {
    let mut aad = Vec::with_capacity(ENVELOPE_DOMAIN.len() + 4 + 32 + 32 + 24);
    aad.extend_from_slice(ENVELOPE_DOMAIN);
    aad.extend_from_slice(&ENVELOPE_VERSION.to_be_bytes());
    aad.extend_from_slice(recipient_key_sha256);
    aad.extend_from_slice(ephemeral_public_key);
    aad.extend_from_slice(nonce);
    aad
}

mod ciphertext_base64 {
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
        let maximum_encoded = MAX_CIPHERTEXT_BYTES.div_ceil(3) * 4;
        if encoded.len() > maximum_encoded {
            return Err(serde::de::Error::custom(
                "secret-release ciphertext exceeds protocol bound",
            ));
        }
        let decoded = BASE64_STANDARD
            .decode(encoded)
            .map_err(serde::de::Error::custom)?;
        if decoded.len() > MAX_CIPHERTEXT_BYTES {
            return Err(serde::de::Error::custom(
                "secret-release ciphertext exceeds protocol bound",
            ));
        }
        Ok(decoded)
    }
}

#[cfg(test)]
mod tests {
    use crate::attestation::{
        AttestationChallenge, CpuAttestationProfile, GuestAttestationParameters,
    };
    use crate::command_proof::AttestedCommand;

    use super::*;

    #[test]
    fn envelope_round_trip_and_tamper_rejection() {
        let recipient = StaticSecret::from([0x42; 32]);
        let recipient_public = PublicKey::from(&recipient).to_bytes();
        let challenge = AttestationChallenge::new([7; 32], "secret-policy", 2_000_000_000).unwrap();
        let parameters = GuestAttestationParameters::new(
            challenge,
            [8; 32],
            CpuAttestationProfile::AmdSevSnp,
            None,
        );
        let command = AttestedCommand::new("/bin/consumer")
            .unwrap()
            .arg("--once")
            .unwrap();
        let payload = SecretReleasePayload {
            request: AttestedCommandRequest::new(parameters.clone(), command.clone()),
            executable_sha256: [9; 32],
            secret: b"bound secret".to_vec(),
        };
        let plaintext = Zeroizing::new(serde_json::to_vec(&payload).unwrap());
        let envelope = seal_to_recipient(&recipient_public, &plaintext).unwrap();

        let (opened_request, opened_executable, opened_secret) =
            open_secret(&envelope, &recipient).unwrap().into_parts();
        assert_eq!(opened_request.attestation(), &parameters);
        assert_eq!(opened_request.command(), &command);
        assert_eq!(opened_executable, [9; 32]);
        assert_eq!(opened_secret.as_slice(), b"bound secret");

        assert!(matches!(
            open_secret(&envelope, &StaticSecret::from([0x24; 32])),
            Err(SecretReleaseError::WrongRecipient)
        ));

        let mut changed = envelope;
        changed.ciphertext[0] ^= 0x80;
        assert!(matches!(
            open_secret(&changed, &recipient),
            Err(SecretReleaseError::Decrypt)
        ));
    }
}
