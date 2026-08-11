use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
#[cfg(feature = "host")]
use ed25519_dalek::Verifier as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

#[cfg(feature = "host")]
use crate::attestation::AttestedGuestKeyProofError;
use crate::attestation::{AttestationChallenge, GuestAttestation, GuestAttestationParameters};
#[cfg(feature = "host")]
use crate::verification::VerifiedAttestation;

const RECORD_DOMAIN: &[u8] = b"nanocodex-vm-command-record\0";
const RECORD_VERSION: u32 = 1;
pub(crate) const RECEIPT_SIGNATURE_DOMAIN: &[u8] = b"nanocodex-vm-command-receipt\0";
const MAX_PROGRAM_BYTES: usize = 4 * 1024;
const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_ARGUMENTS: usize = 256;
const MAX_TOTAL_ARGUMENT_BYTES: usize = 256 * 1024;
const MAX_COMMAND_TIMEOUT_MILLIS: u64 = 10 * 60 * 1_000;

/// Maximum executable size copied into the sealed guest `memfd`.
pub const MAX_ATTESTED_EXECUTABLE_BYTES: usize = 64 * 1024 * 1024;

/// Maximum combined stdout and stderr retained in an attestable receipt.
pub const MAX_ATTESTED_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;

/// One deterministic command executed by the measured guest supervisor.
///
/// The supervisor fixes the current directory to `/`, clears the environment,
/// sets only `LANG=C`, and supplies an empty standard input. It copies the
/// selected ELF into a sealed Linux `memfd` before execution, so the receipt's
/// executable digest identifies the exact bytes passed to `execve` rather than
/// a mutable guest path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttestedCommand {
    program: String,
    arguments: Vec<String>,
    timeout_millis: u64,
    max_output_bytes: usize,
}

impl<'de> Deserialize<'de> for AttestedCommand {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireCommand {
            program: String,
            arguments: Vec<String>,
            timeout_millis: u64,
            max_output_bytes: usize,
        }

        let wire = WireCommand::deserialize(deserializer)?;
        Self::with_limits(
            wire.program,
            wire.arguments,
            wire.timeout_millis,
            wire.max_output_bytes,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl AttestedCommand {
    /// Creates a command with a one-minute deadline and a 1 MiB combined
    /// output limit.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or oversized program or argument list.
    pub fn new(program: impl Into<String>) -> Result<Self, CommandProofInputError> {
        Self::with_limits(
            program.into(),
            Vec::new(),
            60_000,
            MAX_ATTESTED_COMMAND_OUTPUT_BYTES,
        )
    }

    /// Appends one exact UTF-8 argument.
    ///
    /// # Errors
    ///
    /// Returns an error if the resulting argument vector exceeds a protocol
    /// count or byte bound.
    pub fn arg(mut self, argument: impl Into<String>) -> Result<Self, CommandProofInputError> {
        self.arguments.push(argument.into());
        Self::with_limits(
            self.program,
            self.arguments,
            self.timeout_millis,
            self.max_output_bytes,
        )
    }

    /// Sets the command deadline.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero deadline or one longer than ten minutes.
    pub fn timeout_millis(mut self, timeout_millis: u64) -> Result<Self, CommandProofInputError> {
        self.timeout_millis = timeout_millis;
        Self::with_limits(
            self.program,
            self.arguments,
            self.timeout_millis,
            self.max_output_bytes,
        )
    }

    /// Sets the combined retained-output limit.
    ///
    /// # Errors
    ///
    /// Returns an error for zero or more than 1 MiB.
    pub fn max_output_bytes(
        mut self,
        max_output_bytes: usize,
    ) -> Result<Self, CommandProofInputError> {
        self.max_output_bytes = max_output_bytes;
        Self::with_limits(
            self.program,
            self.arguments,
            self.timeout_millis,
            self.max_output_bytes,
        )
    }

    fn with_limits(
        program: String,
        arguments: Vec<String>,
        timeout_millis: u64,
        max_output_bytes: usize,
    ) -> Result<Self, CommandProofInputError> {
        if program.is_empty() || program.len() > MAX_PROGRAM_BYTES {
            return Err(CommandProofInputError::InvalidProgram);
        }
        if arguments.len() > MAX_ARGUMENTS
            || arguments
                .iter()
                .any(|argument| argument.len() > MAX_ARGUMENT_BYTES)
            || arguments.iter().map(String::len).sum::<usize>() > MAX_TOTAL_ARGUMENT_BYTES
        {
            return Err(CommandProofInputError::ArgumentsTooLarge);
        }
        if timeout_millis == 0 || timeout_millis > MAX_COMMAND_TIMEOUT_MILLIS {
            return Err(CommandProofInputError::InvalidTimeout);
        }
        if max_output_bytes == 0 || max_output_bytes > MAX_ATTESTED_COMMAND_OUTPUT_BYTES {
            return Err(CommandProofInputError::InvalidOutputLimit);
        }
        Ok(Self {
            program,
            arguments,
            timeout_millis,
            max_output_bytes,
        })
    }

    /// Returns the guest path from which executable bytes are copied.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Returns the exact arguments following `argv[0]`.
    #[must_use]
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    /// Returns the execution deadline in milliseconds.
    #[must_use]
    pub const fn timeout_millis_value(&self) -> u64 {
        self.timeout_millis
    }

    /// Returns the combined retained-output bound.
    #[must_use]
    pub const fn max_output_bytes_value(&self) -> usize {
        self.max_output_bytes
    }

    #[cfg(all(feature = "guest-runtime", target_os = "linux"))]
    pub(crate) fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.clone())
            .chain(self.arguments.iter().cloned())
            .collect()
    }
}

/// Fresh native-attestation inputs and one command to execute in that guest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttestedCommandRequest {
    attestation: GuestAttestationParameters,
    command: AttestedCommand,
}

impl AttestedCommandRequest {
    /// Creates one atomic execute-and-attest request.
    #[must_use]
    pub const fn new(attestation: GuestAttestationParameters, command: AttestedCommand) -> Self {
        Self {
            attestation,
            command,
        }
    }

    /// Returns the native-attestation parameters.
    #[must_use]
    pub const fn attestation(&self) -> &GuestAttestationParameters {
        &self.attestation
    }

    /// Returns the deterministic command policy.
    #[must_use]
    pub const fn command(&self) -> &AttestedCommand {
        &self.command
    }

    #[cfg(all(feature = "guest-runtime", target_os = "linux"))]
    pub(crate) fn into_parts(self) -> (GuestAttestationParameters, AttestedCommand) {
        (self.attestation, self.command)
    }
}

/// How the executed process terminated.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum CommandTermination {
    /// The process returned an exit code.
    ExitCode(i32),
    /// The process was terminated by a Unix signal.
    Signal(i32),
}

/// Canonical, challenge-bound description of one completed execution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRecord {
    challenge: AttestationChallenge,
    executable_sha256: [u8; 32],
    argv: Vec<String>,
    stdin_sha256: [u8; 32],
    stdout_sha256: [u8; 32],
    stderr_sha256: [u8; 32],
    termination: CommandTermination,
}

impl ExecutionRecord {
    #[cfg(any(test, all(feature = "guest-runtime", target_os = "linux")))]
    pub(crate) fn new(
        challenge: AttestationChallenge,
        executable_sha256: [u8; 32],
        argv: Vec<String>,
        stdout: &[u8],
        stderr: &[u8],
        termination: CommandTermination,
    ) -> Self {
        Self {
            challenge,
            executable_sha256,
            argv,
            stdin_sha256: Sha256::digest([]).into(),
            stdout_sha256: Sha256::digest(stdout).into(),
            stderr_sha256: Sha256::digest(stderr).into(),
            termination,
        }
    }

    /// Returns the challenge that prevents replay of this receipt.
    #[must_use]
    pub const fn challenge(&self) -> &AttestationChallenge {
        &self.challenge
    }

    /// Returns the digest of the sealed bytes passed to `execve`.
    #[must_use]
    pub const fn executable_sha256(&self) -> &[u8; 32] {
        &self.executable_sha256
    }

    /// Returns the exact `argv`, including caller-visible `argv[0]`.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// Returns the digest of the fixed empty stdin stream.
    #[must_use]
    pub const fn stdin_sha256(&self) -> &[u8; 32] {
        &self.stdin_sha256
    }

    /// Returns the captured stdout digest.
    #[must_use]
    pub const fn stdout_sha256(&self) -> &[u8; 32] {
        &self.stdout_sha256
    }

    /// Returns the captured stderr digest.
    #[must_use]
    pub const fn stderr_sha256(&self) -> &[u8; 32] {
        &self.stderr_sha256
    }

    /// Returns the exact process termination result.
    #[must_use]
    pub const fn termination(&self) -> CommandTermination {
        self.termination
    }

    /// Returns the deterministic SHA-256 binding signed by the guest key.
    #[must_use]
    pub fn binding_sha256(&self) -> [u8; 32] {
        Sha256::digest(self.canonical_bytes()).into()
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut encoded = Vec::new();
        encoded.extend_from_slice(RECORD_DOMAIN);
        encoded.extend_from_slice(&RECORD_VERSION.to_be_bytes());
        push_bytes(&mut encoded, self.challenge.nonce());
        push_bytes(&mut encoded, self.challenge.policy_id().as_bytes());
        encoded.extend_from_slice(&self.challenge.expires_at_unix_seconds().to_be_bytes());
        push_bytes(&mut encoded, &self.executable_sha256);
        encoded.extend_from_slice(&(self.argv.len() as u64).to_be_bytes());
        for argument in &self.argv {
            push_bytes(&mut encoded, argument.as_bytes());
        }
        push_bytes(&mut encoded, &self.stdin_sha256);
        push_bytes(&mut encoded, &self.stdout_sha256);
        push_bytes(&mut encoded, &self.stderr_sha256);
        match self.termination {
            CommandTermination::ExitCode(code) => {
                encoded.push(0);
                encoded.extend_from_slice(&code.to_be_bytes());
            }
            CommandTermination::Signal(signal) => {
                encoded.push(1);
                encoded.extend_from_slice(&signal.to_be_bytes());
            }
        }
        encoded
    }
}

/// Signed output and record emitted by the measured execution supervisor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttestedCommandReceipt {
    record: ExecutionRecord,
    #[serde(serialize_with = "serialize_base64")]
    stdout: Vec<u8>,
    #[serde(serialize_with = "serialize_base64")]
    stderr: Vec<u8>,
    #[serde(serialize_with = "serialize_base64")]
    signature: Vec<u8>,
}

impl<'de> Deserialize<'de> for AttestedCommandReceipt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireReceipt {
            record: ExecutionRecord,
            #[serde(deserialize_with = "deserialize_output")]
            stdout: Vec<u8>,
            #[serde(deserialize_with = "deserialize_output")]
            stderr: Vec<u8>,
            #[serde(deserialize_with = "deserialize_signature")]
            signature: Vec<u8>,
        }

        let wire = WireReceipt::deserialize(deserializer)?;
        if wire.stdout.len().saturating_add(wire.stderr.len()) > MAX_ATTESTED_COMMAND_OUTPUT_BYTES {
            return Err(serde::de::Error::custom("command output exceeds 1 MiB"));
        }
        Ok(Self {
            record: wire.record,
            stdout: wire.stdout,
            stderr: wire.stderr,
            signature: wire.signature,
        })
    }
}

impl AttestedCommandReceipt {
    #[cfg(any(test, all(feature = "guest-runtime", target_os = "linux")))]
    pub(crate) fn new(
        record: ExecutionRecord,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        signature: [u8; 64],
    ) -> Self {
        Self {
            record,
            stdout,
            stderr,
            signature: signature.to_vec(),
        }
    }

    /// Returns the signed canonical execution record.
    #[must_use]
    pub const fn record(&self) -> &ExecutionRecord {
        &self.record
    }

    /// Returns complete bounded stdout.
    #[must_use]
    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    /// Returns complete bounded stderr.
    #[must_use]
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    /// Returns the evidence-bound guest key's detached signature.
    #[must_use]
    pub fn signature(&self) -> &[u8] {
        &self.signature
    }
}

/// Native evidence and its signed exact-command receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttestedCommandProof {
    attestation: GuestAttestation,
    receipt: AttestedCommandReceipt,
}

impl AttestedCommandProof {
    #[cfg(all(feature = "guest-runtime", target_os = "linux"))]
    pub(crate) const fn new(
        attestation: GuestAttestation,
        receipt: AttestedCommandReceipt,
    ) -> Self {
        Self {
            attestation,
            receipt,
        }
    }

    /// Returns the fresh native evidence bound to the receipt-signing key.
    #[must_use]
    pub const fn attestation(&self) -> &GuestAttestation {
        &self.attestation
    }

    /// Returns the exact-command receipt.
    #[must_use]
    pub const fn receipt(&self) -> &AttestedCommandReceipt {
        &self.receipt
    }
}

/// Exact relying-party expectation for one command proof.
#[cfg(feature = "host")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandProofExpectation {
    challenge: AttestationChallenge,
    workload_manifest_sha256: [u8; 32],
    executable_sha256: [u8; 32],
    argv: Vec<String>,
    termination: CommandTermination,
}

#[cfg(feature = "host")]
impl CommandProofExpectation {
    /// Creates an exact challenge, executable, and argument expectation.
    #[must_use]
    pub const fn new(
        challenge: AttestationChallenge,
        workload_manifest_sha256: [u8; 32],
        executable_sha256: [u8; 32],
        argv: Vec<String>,
    ) -> Self {
        Self {
            challenge,
            workload_manifest_sha256,
            executable_sha256,
            argv,
            termination: CommandTermination::ExitCode(0),
        }
    }

    /// Overrides the default requirement that the command exits successfully.
    #[must_use]
    pub const fn termination(mut self, termination: CommandTermination) -> Self {
        self.termination = termination;
        self
    }
}

/// A receipt accepted only after native evidence and command verification.
#[cfg(feature = "host")]
#[derive(Clone, Debug)]
pub struct VerifiedCommandProof {
    record: ExecutionRecord,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Internally consistent evidence and receipt which still require vendor appraisal.
#[cfg(feature = "host")]
#[derive(Clone, Debug)]
pub struct CollectedCommandProof {
    record: ExecutionRecord,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[cfg(feature = "host")]
impl CollectedCommandProof {
    /// Returns the cryptographically consistent execution record.
    #[must_use]
    pub const fn record(&self) -> &ExecutionRecord {
        &self.record
    }

    /// Returns output authenticated by the as-yet-unappraised guest key.
    #[must_use]
    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    /// Returns stderr authenticated by the as-yet-unappraised guest key.
    #[must_use]
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }
}

/// Checks collection integrity before vendor-native evidence appraisal.
///
/// This verifies the exact command, output digests, guest-key proof, and both
/// signatures, but the key comes from unappraised evidence in the same bundle.
/// It is therefore a collection/debugging check, not a trusted execution
/// result. Call [`verify_command_proof`] after native evidence verification to
/// obtain [`VerifiedCommandProof`].
///
/// # Errors
///
/// Returns an error for replay, substitution, tampering, or malformed records.
#[cfg(feature = "host")]
pub fn verify_collected_command_proof(
    proof: &AttestedCommandProof,
    expected: &CommandProofExpectation,
) -> Result<CollectedCommandProof, CommandProofVerificationError> {
    proof.attestation.verify_key_proof()?;
    if proof
        .attestation
        .bundle()
        .request()
        .workload_manifest_digest()
        != &expected.workload_manifest_sha256
    {
        return Err(CommandProofVerificationError::WorkloadManifestMismatch);
    }
    verify_receipt(
        &proof.receipt,
        proof.attestation.bundle().request().guest_public_key(),
        expected,
    )?;
    if proof.receipt.record.challenge() != proof.attestation.bundle().request().challenge() {
        return Err(CommandProofVerificationError::ChallengeMismatch);
    }
    Ok(CollectedCommandProof {
        record: proof.receipt.record.clone(),
        stdout: proof.receipt.stdout.clone(),
        stderr: proof.receipt.stderr.clone(),
    })
}

#[cfg(feature = "host")]
impl VerifiedCommandProof {
    /// Returns the accepted execution record.
    #[must_use]
    pub const fn record(&self) -> &ExecutionRecord {
        &self.record
    }

    /// Returns authenticated stdout.
    #[must_use]
    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    /// Returns authenticated stderr.
    #[must_use]
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }
}

/// Verifies a command receipt against already-appraised native evidence.
///
/// The caller must first pass the proof's native bundle through
/// [`crate::host::verify_attestation`]. This function then requires that exact
/// appraised bundle, verifies the guest-key possession proof and receipt
/// signature, authenticates both output streams, and matches the caller's
/// challenge, executable digest, and complete `argv`.
///
/// # Errors
///
/// Returns an error for any substituted attestation, replay, command change,
/// output change, malformed key or signature, or invalid record shape.
#[cfg(feature = "host")]
pub fn verify_command_proof(
    proof: &AttestedCommandProof,
    verified_attestation: &VerifiedAttestation,
    expected: &CommandProofExpectation,
) -> Result<VerifiedCommandProof, CommandProofVerificationError> {
    if proof.attestation.bundle() != verified_attestation.bundle() {
        return Err(CommandProofVerificationError::AttestationMismatch);
    }
    if verified_attestation.workload_manifest_digest() != &expected.workload_manifest_sha256 {
        return Err(CommandProofVerificationError::WorkloadManifestMismatch);
    }
    let collected = verify_collected_command_proof(proof, expected)?;
    Ok(VerifiedCommandProof {
        record: collected.record,
        stdout: collected.stdout,
        stderr: collected.stderr,
    })
}

#[cfg(feature = "host")]
fn verify_receipt(
    receipt: &AttestedCommandReceipt,
    guest_public_key: &[u8],
    expected: &CommandProofExpectation,
) -> Result<(), CommandProofVerificationError> {
    let record = &receipt.record;
    if record.challenge != expected.challenge {
        return Err(CommandProofVerificationError::ChallengeMismatch);
    }
    if record.executable_sha256 != expected.executable_sha256
        || record.argv != expected.argv
        || record.termination != expected.termination
    {
        return Err(CommandProofVerificationError::CommandMismatch);
    }
    if record.argv.is_empty()
        || record.argv.len() > MAX_ARGUMENTS + 1
        || record
            .argv
            .iter()
            .any(|argument| argument.len() > MAX_ARGUMENT_BYTES)
        || record.argv.iter().map(String::len).sum::<usize>()
            > MAX_TOTAL_ARGUMENT_BYTES + MAX_PROGRAM_BYTES
        || record.stdin_sha256 != Sha256::digest([]).as_slice()
    {
        return Err(CommandProofVerificationError::InvalidRecord);
    }
    if record.stdout_sha256 != Sha256::digest(&receipt.stdout).as_slice()
        || record.stderr_sha256 != Sha256::digest(&receipt.stderr).as_slice()
    {
        return Err(CommandProofVerificationError::OutputMismatch);
    }
    let public_key = <&[u8; 32]>::try_from(guest_public_key)
        .map_err(|_| CommandProofVerificationError::InvalidGuestKey)?;
    let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(public_key)
        .map_err(|_| CommandProofVerificationError::InvalidGuestKey)?;
    let signature = ed25519_dalek::Signature::from_slice(&receipt.signature)
        .map_err(|_| CommandProofVerificationError::InvalidSignature)?;
    verifying_key
        .verify(&receipt_signature_message(record), &signature)
        .map_err(|_| CommandProofVerificationError::SignatureMismatch)
}

pub(crate) fn receipt_signature_message(record: &ExecutionRecord) -> Vec<u8> {
    let binding = record.binding_sha256();
    let mut message = Vec::with_capacity(RECEIPT_SIGNATURE_DOMAIN.len() + binding.len());
    message.extend_from_slice(RECEIPT_SIGNATURE_DOMAIN);
    message.extend_from_slice(&binding);
    message
}

fn push_bytes(target: &mut Vec<u8>, bytes: &[u8]) {
    target.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    target.extend_from_slice(bytes);
}

/// Rejected command-proof request.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CommandProofInputError {
    /// Program path is empty or exceeds 4 KiB.
    #[error("attested command program is empty or exceeds 4 KiB")]
    InvalidProgram,
    /// Arguments exceed the bounded count or encoded size.
    #[error("attested command arguments exceed protocol bounds")]
    ArgumentsTooLarge,
    /// Deadline is zero or exceeds ten minutes.
    #[error("attested command deadline must be between 1 ms and 10 minutes")]
    InvalidTimeout,
    /// Retained output is zero or exceeds 1 MiB.
    #[error("attested command output limit must be between 1 byte and 1 MiB")]
    InvalidOutputLimit,
}

/// Rejected command proof after native evidence appraisal.
#[cfg(feature = "host")]
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CommandProofVerificationError {
    /// The proof did not contain the exact appraised evidence bundle.
    #[error("command proof does not contain the appraised attestation bundle")]
    AttestationMismatch,
    /// The guest-key possession proof was invalid.
    #[error(transparent)]
    AttestedKey(#[from] AttestedGuestKeyProofError),
    /// The receipt or native evidence did not bind the expected fresh challenge.
    #[error("command proof challenge does not match")]
    ChallengeMismatch,
    /// The evidence did not bind the expected measured supervisor manifest.
    #[error("command proof workload manifest does not match")]
    WorkloadManifestMismatch,
    /// The executable digest or argument vector changed.
    #[error("command proof executable or argv does not match")]
    CommandMismatch,
    /// The canonical record violated fixed supervisor semantics or bounds.
    #[error("command proof record is invalid")]
    InvalidRecord,
    /// Retained output did not match its signed digest.
    #[error("command proof output digest does not match")]
    OutputMismatch,
    /// Native evidence did not bind a valid Ed25519 key.
    #[error("command proof guest key is invalid")]
    InvalidGuestKey,
    /// Receipt signature was not 64-byte Ed25519.
    #[error("command proof signature is invalid")]
    InvalidSignature,
    /// Receipt was not signed by the evidence-bound guest key.
    #[error("command proof signature does not match the attested guest key")]
    SignatureMismatch,
}

fn serialize_base64<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&BASE64_STANDARD.encode(bytes))
}

fn deserialize_output<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_base64(deserializer, MAX_ATTESTED_COMMAND_OUTPUT_BYTES)
}

fn deserialize_signature<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let signature = deserialize_base64(deserializer, 64)?;
    if signature.len() != 64 {
        return Err(serde::de::Error::custom(
            "command proof signature must be 64 bytes",
        ));
    }
    Ok(signature)
}

fn deserialize_base64<'de, D>(deserializer: D, maximum: usize) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let encoded = String::deserialize(deserializer)?;
    let maximum_encoded = maximum.saturating_add(2) / 3 * 4;
    if encoded.len() > maximum_encoded {
        return Err(serde::de::Error::custom("base64 value exceeds bound"));
    }
    let decoded = BASE64_STANDARD
        .decode(encoded)
        .map_err(serde::de::Error::custom)?;
    if decoded.len() > maximum {
        return Err(serde::de::Error::custom("decoded value exceeds bound"));
    }
    Ok(decoded)
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use ed25519_dalek::{Signer as _, SigningKey};

    use super::*;

    fn fixture() -> (
        AttestedCommandReceipt,
        CommandProofExpectation,
        ed25519_dalek::VerifyingKey,
    ) {
        let challenge = AttestationChallenge::new([7; 32], "test-policy", 100).unwrap();
        let executable_sha256: [u8; 32] = Sha256::digest(b"exact executable").into();
        let argv = vec!["/bin/demo".to_owned(), "hello".to_owned()];
        let record = ExecutionRecord::new(
            challenge.clone(),
            executable_sha256,
            argv.clone(),
            b"hello\n",
            b"",
            CommandTermination::ExitCode(0),
        );
        let signing_key = SigningKey::from_bytes(&[9; 32]);
        let signature = signing_key
            .sign(&receipt_signature_message(&record))
            .to_bytes();
        (
            AttestedCommandReceipt::new(record, b"hello\n".to_vec(), Vec::new(), signature),
            CommandProofExpectation::new(
                challenge,
                Sha256::digest(b"measured supervisor").into(),
                executable_sha256,
                argv,
            ),
            signing_key.verifying_key(),
        )
    }

    #[test]
    fn verifies_exact_receipt() {
        let (receipt, expected, key) = fixture();
        verify_receipt(&receipt, key.as_bytes(), &expected).unwrap();
    }

    #[test]
    fn rejects_replay_challenge() {
        let (receipt, mut expected, key) = fixture();
        expected.challenge = AttestationChallenge::new([8; 32], "test-policy", 100).unwrap();
        assert_eq!(
            verify_receipt(&receipt, key.as_bytes(), &expected),
            Err(CommandProofVerificationError::ChallengeMismatch)
        );
    }

    #[test]
    fn rejects_changed_command() {
        let (receipt, mut expected, key) = fixture();
        expected.argv.push("changed".to_owned());
        assert_eq!(
            verify_receipt(&receipt, key.as_bytes(), &expected),
            Err(CommandProofVerificationError::CommandMismatch)
        );
    }

    #[test]
    fn rejects_tampered_output() {
        let (mut receipt, expected, key) = fixture();
        receipt.stdout = b"goodbye\n".to_vec();
        assert_eq!(
            verify_receipt(&receipt, key.as_bytes(), &expected),
            Err(CommandProofVerificationError::OutputMismatch)
        );
    }

    #[test]
    fn rejects_nonzero_exit_status_even_before_signature_check() {
        let (mut receipt, expected, key) = fixture();
        receipt.record.termination = CommandTermination::ExitCode(1);
        assert_eq!(
            verify_receipt(&receipt, key.as_bytes(), &expected),
            Err(CommandProofVerificationError::CommandMismatch)
        );
    }
}
