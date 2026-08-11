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
use crate::command_proof::{AttestedCommandProof, AttestedCommandRequest};
#[cfg(feature = "host")]
use crate::verification::VerifiedAttestation;

const ENVELOPE_DOMAIN: &[u8] = b"nanocodex-vm-secret-release\0";
const ENVELOPE_VERSION: u32 = 1;
const RESPONSE_DOMAIN: &[u8] = b"nanocodex-vm-confidential-command-response\0";
const RESPONSE_VERSION: u32 = 1;
/// Maximum secret bytes accepted by one attestation-gated release envelope.
pub const MAX_SECRET_RELEASE_BYTES: usize = 64 * 1024;
const MAX_CIPHERTEXT_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_CIPHERTEXT_BYTES: usize = 40 * 1024 * 1024;
#[cfg(all(feature = "guest-runtime", target_os = "linux"))]
pub(crate) const MAX_OPENED_SECRET_RELEASES: usize = 1024;

/// An AEAD envelope decryptable only by the evidence-bound guest identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SecretReleaseEnvelope {
    version: u32,
    recipient_key_sha256: [u8; 32],
    ephemeral_public_key: [u8; 32],
    nonce: [u8; 24],
    #[serde(with = "ciphertext_base64")]
    ciphertext: Vec<u8>,
}

impl SecretReleaseEnvelope {
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

/// One verify-before-encrypt command retained with its relying-party response key.
///
/// The command policy and input are encrypted to the appraised guest. The
/// response key never enters the VM transport, so an intermediary can neither
/// read the request nor open the returned signed proof.
#[cfg(feature = "host")]
pub struct ConfidentialCommand {
    envelope: SecretReleaseEnvelope,
    response_key: Zeroizing<[u8; 32]>,
    envelope_sha256: [u8; 32],
}

#[cfg(feature = "host")]
impl std::fmt::Debug for ConfidentialCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfidentialCommand")
            .field("envelope_sha256", &self.envelope_sha256)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "host")]
impl ConfidentialCommand {
    pub(crate) fn into_parts(self) -> (SecretReleaseEnvelope, Zeroizing<[u8; 32]>, [u8; 32]) {
        (self.envelope, self.response_key, self.envelope_sha256)
    }
}

/// An encrypted signed command proof bound to one request envelope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfidentialCommandProof {
    version: u32,
    request_envelope_sha256: [u8; 32],
    nonce: [u8; 24],
    #[serde(with = "response_ciphertext_base64")]
    ciphertext: Vec<u8>,
}

/// Failure while sealing or opening an attestation-gated secret.
#[derive(Debug, Error)]
pub enum SecretReleaseError {
    /// The relying-party challenge expired before release authorization.
    #[cfg(feature = "host")]
    #[error("attestation challenge expired at {expires_at}; release time is {now}")]
    ChallengeExpired {
        /// Challenge expiry as Unix seconds.
        expires_at: u64,
        /// Relying-party trusted release time as Unix seconds.
        now: u64,
    },
    /// The verified guest used the legacy signing-only key format.
    #[cfg(feature = "host")]
    #[error("verified attestation does not contain an X25519 secret-release key")]
    MissingEncryptionKey,
    /// The evidence-bound key encoding is malformed.
    #[cfg(feature = "host")]
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
    /// The encrypted response exceeded the fixed protocol bound.
    #[error("confidential command response ciphertext is {actual} bytes; maximum is {maximum}")]
    ResponseCiphertextTooLarge {
        /// Supplied byte length.
        actual: usize,
        /// Protocol maximum.
        maximum: usize,
    },
    /// Secure random generation failed.
    #[error("failed to generate secret-release randomness: {0}")]
    Random(#[from] getrandom::Error),
    /// The authorized request could not be encoded.
    #[cfg(feature = "host")]
    #[error("failed to encode secret-release policy: {0}")]
    Encode(serde_json::Error),
    /// The decrypted request was malformed or violated a bound.
    #[error("failed to decode secret-release policy: {0}")]
    Decode(serde_json::Error),
    /// The signed command proof could not be encoded for encryption.
    #[error("failed to encode confidential command proof: {0}")]
    ResponseEncode(serde_json::Error),
    /// The decrypted response was not a signed command proof.
    #[cfg(feature = "host")]
    #[error("failed to decode confidential command proof: {0}")]
    ResponseDecode(serde_json::Error),
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
    #[cfg(feature = "host")]
    #[error("failed to encrypt secret-release envelope")]
    Encrypt,
    /// AEAD authentication or decryption failed.
    #[error("secret-release envelope authentication failed")]
    Decrypt,
    /// Response encryption failed.
    #[error("failed to encrypt confidential command response")]
    ResponseEncrypt,
    /// Response authentication or decryption failed.
    #[cfg(feature = "host")]
    #[error("confidential command response authentication failed")]
    ResponseDecrypt,
    /// HKDF could not derive the fixed-size AEAD key.
    #[error("failed to derive secret-release AEAD key")]
    KeyDerivation,
    /// The plaintext carries an unsupported protocol version.
    #[error("unsupported secret-release envelope version {0}")]
    UnsupportedVersion(u32),
    /// The response carries an unsupported protocol version.
    #[cfg(feature = "host")]
    #[error("unsupported confidential command response version {0}")]
    UnsupportedResponseVersion(u32),
    /// The response was produced for a different encrypted request.
    #[cfg(feature = "host")]
    #[error("confidential command response is bound to a different request envelope")]
    ResponseRequestMismatch,
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
pub(crate) struct OpenedConfidentialCommand {
    release: OpenedSecretRelease,
    response_key: Zeroizing<[u8; 32]>,
    envelope_sha256: [u8; 32],
}

#[cfg(any(test, all(feature = "guest-runtime", target_os = "linux")))]
type OpenedConfidentialCommandParts = (
    AttestedCommandRequest,
    [u8; 32],
    Zeroizing<Vec<u8>>,
    Zeroizing<[u8; 32]>,
    [u8; 32],
);

#[cfg(any(test, all(feature = "guest-runtime", target_os = "linux")))]
impl OpenedConfidentialCommand {
    pub(crate) fn into_parts(self) -> OpenedConfidentialCommandParts {
        let (request, executable_sha256, secret) = self.release.into_parts();
        (
            request,
            executable_sha256,
            secret,
            self.response_key,
            self.envelope_sha256,
        )
    }
}

#[cfg(any(test, all(feature = "guest-runtime", target_os = "linux")))]
impl OpenedSecretRelease {
    pub(crate) fn into_parts(self) -> (AttestedCommandRequest, [u8; 32], Zeroizing<Vec<u8>>) {
        (self.request, self.executable_sha256, self.secret)
    }
}

/// Encrypts a secret and exact command policy to an already-verified guest.
///
/// The guest will accept the request only under the same retained identity,
/// execute the exact expected static ELF bytes and `argv`, feed the secret on
/// stdin, and encrypt the complete signed proof back to the relying party.
/// `now_unix_seconds` must come from the relying party's trusted clock and
/// prevents release after the challenge expires.
#[cfg(feature = "host")]
pub fn seal_confidential_command(
    verified: &VerifiedAttestation,
    now_unix_seconds: u64,
    command: AttestedCommand,
    executable_sha256: [u8; 32],
    secret: &[u8],
) -> Result<ConfidentialCommand, SecretReleaseError> {
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
    let (envelope, response_key) = seal_to_recipient(recipient, &plaintext)?;
    let envelope_sha256 = envelope.digest();
    Ok(ConfidentialCommand {
        envelope,
        response_key: Zeroizing::new(derive_response_key(&response_key, &envelope_sha256)?),
        envelope_sha256,
    })
}

#[cfg(any(feature = "host", test))]
fn seal_to_recipient(
    recipient: &[u8; 32],
    plaintext: &[u8],
) -> Result<(SecretReleaseEnvelope, Zeroizing<[u8; 32]>), SecretReleaseError> {
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
    Ok((
        SecretReleaseEnvelope {
            version: ENVELOPE_VERSION,
            recipient_key_sha256,
            ephemeral_public_key,
            nonce,
            ciphertext,
        },
        Zeroizing::new(*shared.as_bytes()),
    ))
}

#[cfg(any(test, all(feature = "guest-runtime", target_os = "linux")))]
pub(crate) fn open_confidential_command(
    envelope: &SecretReleaseEnvelope,
    recipient_secret: &StaticSecret,
) -> Result<OpenedConfidentialCommand, SecretReleaseError> {
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
    let envelope_sha256 = envelope.digest();
    Ok(OpenedConfidentialCommand {
        release: OpenedSecretRelease {
            request: payload.request.clone(),
            executable_sha256: payload.executable_sha256,
            secret: Zeroizing::new(std::mem::take(&mut payload.secret)),
        },
        response_key: Zeroizing::new(derive_response_key(shared.as_bytes(), &envelope_sha256)?),
        envelope_sha256,
    })
}

#[cfg(all(feature = "guest-runtime", target_os = "linux"))]
pub(crate) fn seal_confidential_proof(
    proof: &AttestedCommandProof,
    response_key: &[u8; 32],
    request_envelope_sha256: [u8; 32],
) -> Result<ConfidentialCommandProof, SecretReleaseError> {
    let plaintext =
        Zeroizing::new(serde_json::to_vec(proof).map_err(SecretReleaseError::ResponseEncode)?);
    seal_response_bytes(&plaintext, response_key, request_envelope_sha256)
}

#[cfg(feature = "host")]
pub(crate) fn open_confidential_proof(
    proof: &ConfidentialCommandProof,
    response_key: &[u8; 32],
    request_envelope_sha256: [u8; 32],
) -> Result<AttestedCommandProof, SecretReleaseError> {
    let plaintext = open_response_bytes(proof, response_key, request_envelope_sha256)?;
    serde_json::from_slice(&plaintext).map_err(SecretReleaseError::ResponseDecode)
}

#[cfg(any(test, all(feature = "guest-runtime", target_os = "linux")))]
fn seal_response_bytes(
    plaintext: &[u8],
    response_key: &[u8; 32],
    request_envelope_sha256: [u8; 32],
) -> Result<ConfidentialCommandProof, SecretReleaseError> {
    let mut nonce = [0_u8; 24];
    getrandom::fill(&mut nonce)?;
    let aad = response_associated_data(&request_envelope_sha256, &nonce);
    let ciphertext = XChaCha20Poly1305::new(response_key.into())
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| SecretReleaseError::ResponseEncrypt)?;
    if ciphertext.len() > MAX_RESPONSE_CIPHERTEXT_BYTES {
        return Err(SecretReleaseError::ResponseCiphertextTooLarge {
            actual: ciphertext.len(),
            maximum: MAX_RESPONSE_CIPHERTEXT_BYTES,
        });
    }
    Ok(ConfidentialCommandProof {
        version: RESPONSE_VERSION,
        request_envelope_sha256,
        nonce,
        ciphertext,
    })
}

#[cfg(test)]
pub(crate) fn test_seal_response(
    plaintext: &[u8],
    response_key: &[u8; 32],
    request_envelope_sha256: [u8; 32],
) -> ConfidentialCommandProof {
    seal_response_bytes(plaintext, response_key, request_envelope_sha256)
        .expect("test response encryption should succeed")
}

#[cfg(any(feature = "host", test))]
fn open_response_bytes(
    proof: &ConfidentialCommandProof,
    response_key: &[u8; 32],
    request_envelope_sha256: [u8; 32],
) -> Result<Zeroizing<Vec<u8>>, SecretReleaseError> {
    if proof.version != RESPONSE_VERSION {
        return Err(SecretReleaseError::UnsupportedResponseVersion(
            proof.version,
        ));
    }
    if proof.request_envelope_sha256 != request_envelope_sha256 {
        return Err(SecretReleaseError::ResponseRequestMismatch);
    }
    if proof.ciphertext.len() > MAX_RESPONSE_CIPHERTEXT_BYTES {
        return Err(SecretReleaseError::ResponseCiphertextTooLarge {
            actual: proof.ciphertext.len(),
            maximum: MAX_RESPONSE_CIPHERTEXT_BYTES,
        });
    }
    let aad = response_associated_data(&request_envelope_sha256, &proof.nonce);
    XChaCha20Poly1305::new(response_key.into())
        .decrypt(
            XNonce::from_slice(&proof.nonce),
            Payload {
                msg: &proof.ciphertext,
                aad: &aad,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| SecretReleaseError::ResponseDecrypt)
}

fn derive_key(shared: &[u8; 32], aad: &[u8]) -> Result<[u8; 32], SecretReleaseError> {
    let salt = Sha256::digest(aad);
    let hkdf = Hkdf::<HkdfSha256>::new(Some(&salt), shared);
    let mut key = [0_u8; 32];
    hkdf.expand(ENVELOPE_DOMAIN, &mut key)
        .map_err(|_| SecretReleaseError::KeyDerivation)?;
    Ok(key)
}

fn derive_response_key(
    shared: &[u8; 32],
    request_envelope_sha256: &[u8; 32],
) -> Result<[u8; 32], SecretReleaseError> {
    let hkdf = Hkdf::<HkdfSha256>::new(Some(request_envelope_sha256), shared);
    let mut key = [0_u8; 32];
    hkdf.expand(RESPONSE_DOMAIN, &mut key)
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

fn response_associated_data(request_envelope_sha256: &[u8; 32], nonce: &[u8; 24]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(RESPONSE_DOMAIN.len() + 4 + 32 + 24);
    aad.extend_from_slice(RESPONSE_DOMAIN);
    aad.extend_from_slice(&RESPONSE_VERSION.to_be_bytes());
    aad.extend_from_slice(request_envelope_sha256);
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

mod response_ciphertext_base64 {
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
        let maximum_encoded = MAX_RESPONSE_CIPHERTEXT_BYTES.div_ceil(3) * 4;
        if encoded.len() > maximum_encoded {
            return Err(serde::de::Error::custom(
                "confidential command response ciphertext exceeds protocol bound",
            ));
        }
        let decoded = BASE64_STANDARD
            .decode(encoded)
            .map_err(serde::de::Error::custom)?;
        if decoded.len() > MAX_RESPONSE_CIPHERTEXT_BYTES {
            return Err(serde::de::Error::custom(
                "confidential command response ciphertext exceeds protocol bound",
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
        let (envelope, shared) = seal_to_recipient(&recipient_public, &plaintext).unwrap();
        let wire = serde_json::to_vec(&envelope).unwrap();

        assert!(!wire.windows(12).any(|window| window == b"bound secret"));
        assert!(!wire.windows(13).any(|window| window == b"/bin/consumer"));
        assert!(!wire.windows(6).any(|window| window == b"--once"));

        let opened = open_confidential_command(&envelope, &recipient).unwrap();
        let (
            opened_request,
            opened_executable,
            opened_secret,
            opened_response_key,
            envelope_sha256,
        ) = opened.into_parts();
        assert_eq!(opened_request.attestation(), &parameters);
        assert_eq!(opened_request.command(), &command);
        assert_eq!(opened_executable, [9; 32]);
        assert_eq!(opened_secret.as_slice(), b"bound secret");
        assert_eq!(
            opened_response_key.as_ref(),
            &derive_response_key(&shared, &envelope_sha256).unwrap()
        );

        assert!(matches!(
            open_confidential_command(&envelope, &StaticSecret::from([0x24; 32])),
            Err(SecretReleaseError::WrongRecipient)
        ));

        let mut changed = envelope;
        changed.ciphertext[0] ^= 0x80;
        assert!(matches!(
            open_confidential_command(&changed, &recipient),
            Err(SecretReleaseError::Decrypt)
        ));
    }

    #[test]
    fn confidential_response_is_bound_private_and_tamper_evident() {
        let response_key = [0x31; 32];
        let request_digest = [0x72; 32];
        let plaintext = b"dashboard-private-output-sentinel";
        let proof = seal_response_bytes(plaintext, &response_key, request_digest).unwrap();
        let wire = serde_json::to_vec(&proof).unwrap();

        assert!(
            !wire
                .windows(plaintext.len())
                .any(|window| window == plaintext)
        );
        assert_eq!(
            open_response_bytes(&proof, &response_key, request_digest)
                .unwrap()
                .as_slice(),
            plaintext
        );
        assert!(matches!(
            open_response_bytes(&proof, &[0x13; 32], request_digest),
            Err(SecretReleaseError::ResponseDecrypt)
        ));
        assert!(matches!(
            open_response_bytes(&proof, &response_key, [0x27; 32]),
            Err(SecretReleaseError::ResponseRequestMismatch)
        ));

        let mut changed = proof;
        changed.ciphertext[0] ^= 0x80;
        assert!(matches!(
            open_response_bytes(&changed, &response_key, request_digest),
            Err(SecretReleaseError::ResponseDecrypt)
        ));
    }
}
