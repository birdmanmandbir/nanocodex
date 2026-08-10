use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use aws_nitro_enclaves_nsm_api::api::{AttestationDoc, Digest};
use p384::ecdsa::{Signature, VerifyingKey, signature::Verifier as _};
use p384::pkcs8::DecodePublicKey as _;
use serde_cbor::Value;
use x509_parser::{certificate::X509Certificate, prelude::FromDer, time::ASN1Time};

use crate::{
    attestation::{AttestedComponent, EvidenceProfile, RawEvidence},
    verification::{
        NativeEvidenceVerifier, NativeVerificationContext, NativeVerificationError,
        VerifiedNativeBinding, VerifiedNativeEvidence,
    },
};

const NITRO_MEDIA_TYPE: &str = "application/vnd.amazon.nitro.attestation-cose";
const MAX_AWS_CERTIFICATES: usize = 16;

/// Named PCR and certificate policy for AWS Nitro Enclave evidence.
#[derive(Clone, Debug)]
pub struct NitroVerificationPolicy {
    policy_id: String,
    aws_root_der: Vec<u8>,
    expected_pcrs: BTreeMap<usize, Vec<u8>>,
    maximum_age_seconds: u64,
    maximum_future_skew_seconds: u64,
    expected_module_id: Option<String>,
    require_exact_pcr_set: bool,
}

impl NitroVerificationPolicy {
    /// Creates a policy pinned to one caller-reviewed AWS root and PCR set.
    ///
    /// # Errors
    ///
    /// Returns an error for empty identifiers, roots, PCR policies, or an
    /// invalid maximum document age.
    pub fn new(
        policy_id: impl Into<String>,
        aws_root_der: impl Into<Vec<u8>>,
        expected_pcrs: BTreeMap<usize, Vec<u8>>,
        maximum_age_seconds: u64,
    ) -> Result<Self, NativeVerificationError> {
        let policy_id = policy_id.into();
        let aws_root_der = aws_root_der.into();
        if policy_id.is_empty() || policy_id.len() > 256 {
            return Err(error(
                "Nitro policy id must contain between 1 and 256 bytes",
            ));
        }
        if aws_root_der.is_empty() || aws_root_der.len() > 64 * 1024 {
            return Err(error("Nitro root certificate has an invalid size"));
        }
        if expected_pcrs.is_empty()
            || expected_pcrs.len() > 32
            || expected_pcrs
                .iter()
                .any(|(index, value)| *index > 31 || value.len() != 48)
        {
            return Err(error(
                "Nitro policy requires 1 to 32 SHA-384 PCRs at indexes 0 through 31",
            ));
        }
        if maximum_age_seconds == 0 {
            return Err(error("Nitro maximum document age must be nonzero"));
        }
        Ok(Self {
            policy_id,
            aws_root_der,
            expected_pcrs,
            maximum_age_seconds,
            maximum_future_skew_seconds: 30,
            expected_module_id: None,
            require_exact_pcr_set: false,
        })
    }

    /// Sets the maximum permitted clock skew into the future.
    #[must_use]
    pub const fn with_maximum_future_skew(mut self, seconds: u64) -> Self {
        self.maximum_future_skew_seconds = seconds;
        self
    }

    /// Requires the signed NSM module identifier to match exactly.
    #[must_use]
    pub fn with_module_id(mut self, module_id: impl Into<String>) -> Self {
        self.expected_module_id = Some(module_id.into());
        self
    }

    /// Requires the document's complete PCR map, rather than an approved subset.
    #[must_use]
    pub const fn with_exact_pcr_set(mut self, required: bool) -> Self {
        self.require_exact_pcr_set = required;
        self
    }
}

/// Pure-Rust offline verifier for AWS Nitro Enclave COSE_Sign1 evidence.
pub struct NitroVerifier {
    policies: BTreeMap<String, NitroVerificationPolicy>,
}

impl NitroVerifier {
    /// Creates a verifier with one named policy.
    #[must_use]
    pub fn new(policy: NitroVerificationPolicy) -> Self {
        let mut policies = BTreeMap::new();
        policies.insert(policy.policy_id.clone(), policy);
        Self { policies }
    }

    /// Adds another named policy without silently replacing one.
    ///
    /// # Errors
    ///
    /// Returns an error for a duplicate policy identifier.
    pub fn with_policy(
        mut self,
        policy: NitroVerificationPolicy,
    ) -> Result<Self, NativeVerificationError> {
        if self.policies.contains_key(&policy.policy_id) {
            return Err(error(format!(
                "duplicate Nitro policy id {:?}",
                policy.policy_id
            )));
        }
        self.policies.insert(policy.policy_id.clone(), policy);
        Ok(self)
    }
}

#[async_trait]
impl NativeEvidenceVerifier for NitroVerifier {
    async fn verify(
        &self,
        evidence: &RawEvidence,
        context: NativeVerificationContext<'_>,
    ) -> Result<VerifiedNativeEvidence, NativeVerificationError> {
        if evidence.component() != &AttestedComponent::CpuVm
            || evidence.profile() != EvidenceProfile::AwsNitro
            || evidence.media_type() != NITRO_MEDIA_TYPE
        {
            return Err(error("Nitro verifier received evidence of the wrong type"));
        }
        let policy = self
            .policies
            .get(context.challenge().policy_id())
            .ok_or_else(|| error("no Nitro policy matches the challenge policy id"))?;
        let cose = parse_cose_sign1(evidence.bytes())?;
        let document = AttestationDoc::from_binary(&cose.payload)
            .map_err(|source| error(format!("invalid Nitro attestation payload: {source:?}")))?;
        let now = appraisal_time(context.now_unix_seconds())?;
        let leaf = validate_certificate_chain(
            document.certificate.as_ref(),
            &document
                .cabundle
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>(),
            &policy.aws_root_der,
            now,
        )?;
        verify_cose_signature(&cose, &leaf)?;
        validate_document(&document, context, policy)?;

        Ok(VerifiedNativeEvidence::new(
            evidence.digest(),
            AttestedComponent::CpuVm,
            EvidenceProfile::AwsNitro,
            VerifiedNativeBinding::AwsNitro {
                nonce: *context.challenge().nonce(),
                guest_public_key: context.attestation_binding().guest_public_key().to_vec(),
                transcript_digest: *context.transcript_digest(),
            },
            true,
            true,
            true,
            format!("aws-nitro-nsm:{}", document.module_id),
            None,
        ))
    }
}

#[derive(Debug)]
struct CoseSign1 {
    protected: Vec<u8>,
    payload: Vec<u8>,
    signature: Vec<u8>,
}

fn error(message: impl Into<String>) -> NativeVerificationError {
    NativeVerificationError::new(message)
}

fn parse_cose_sign1(bytes: &[u8]) -> Result<CoseSign1, NativeVerificationError> {
    let value: Value = serde_cbor::from_slice(bytes)
        .map_err(|source| error(format!("invalid Nitro COSE_Sign1 CBOR: {source}")))?;
    let Value::Tag(18, tagged) = value else {
        return Err(error("Nitro evidence must carry COSE_Sign1 tag 18"));
    };
    let Value::Array(mut fields) = *tagged else {
        return Err(error("Nitro COSE_Sign1 body is not an array"));
    };
    if fields.len() != 4 {
        return Err(error("Nitro COSE_Sign1 must contain exactly four fields"));
    }
    let signature = expect_bytes(fields.pop(), "signature")?;
    let payload = expect_bytes(fields.pop(), "payload")?;
    let unprotected = fields
        .pop()
        .ok_or_else(|| error("Nitro COSE_Sign1 omitted unprotected headers"))?;
    let protected = expect_bytes(fields.pop(), "protected headers")?;
    let Value::Map(unprotected) = unprotected else {
        return Err(error("Nitro COSE unprotected headers are not a map"));
    };
    if !unprotected.is_empty() {
        return Err(error("Nitro COSE unprotected headers must be empty"));
    }
    let protected_map: Value = serde_cbor::from_slice(&protected)
        .map_err(|source| error(format!("invalid Nitro protected headers: {source}")))?;
    let Value::Map(protected_map) = protected_map else {
        return Err(error("Nitro protected headers are not a map"));
    };
    if protected_map.len() != 1
        || protected_map.get(&Value::Integer(1)) != Some(&Value::Integer(-35))
    {
        return Err(error("Nitro COSE protected algorithm is not ES384"));
    }
    if signature.len() != 96 || payload.is_empty() {
        return Err(error("Nitro COSE signature or payload has an invalid size"));
    }
    Ok(CoseSign1 {
        protected,
        payload,
        signature,
    })
}

fn expect_bytes(value: Option<Value>, label: &str) -> Result<Vec<u8>, NativeVerificationError> {
    match value {
        Some(Value::Bytes(bytes)) => Ok(bytes),
        _ => Err(error(format!("Nitro COSE {label} is not a byte string"))),
    }
}

fn appraisal_time(now_unix_seconds: u64) -> Result<ASN1Time, NativeVerificationError> {
    let seconds = i64::try_from(now_unix_seconds)
        .map_err(|_| error("Nitro appraisal time is outside the X.509 range"))?;
    ASN1Time::from_timestamp(seconds)
        .map_err(|source| error(format!("invalid Nitro appraisal time: {source}")))
}

fn parse_certificate<'a>(
    der: &'a [u8],
    label: &str,
) -> Result<X509Certificate<'a>, NativeVerificationError> {
    let (remaining, certificate) = X509Certificate::from_der(der)
        .map_err(|source| error(format!("invalid Nitro {label} certificate: {source}")))?;
    if !remaining.is_empty() {
        return Err(error(format!(
            "Nitro {label} certificate contains trailing bytes"
        )));
    }
    Ok(certificate)
}

fn validate_certificate_chain(
    leaf_der: &[u8],
    cabundle: &[&[u8]],
    root_der: &[u8],
    now: ASN1Time,
) -> Result<Vec<u8>, NativeVerificationError> {
    if cabundle.is_empty() || cabundle.len() > MAX_AWS_CERTIFICATES {
        return Err(error("Nitro CA bundle has an invalid certificate count"));
    }
    if cabundle
        .iter()
        .filter(|candidate| **candidate == root_der)
        .count()
        != 1
    {
        return Err(error(
            "Nitro CA bundle does not contain the pinned AWS root exactly once",
        ));
    }
    let leaf = parse_certificate(leaf_der, "leaf")?;
    validate_certificate_time(&leaf, now, "leaf")?;
    if leaf
        .basic_constraints()
        .map_err(|source| error(format!("invalid Nitro leaf constraints: {source}")))?
        .is_some_and(|constraints| constraints.value.ca)
    {
        return Err(error("Nitro leaf certificate is a CA"));
    }
    if leaf
        .key_usage()
        .map_err(|source| error(format!("invalid Nitro leaf key usage: {source}")))?
        .is_some_and(|usage| !usage.value.digital_signature())
    {
        return Err(error("Nitro leaf certificate cannot sign attestations"));
    }
    let certificates = cabundle
        .iter()
        .enumerate()
        .map(|(index, der)| parse_certificate(der, &format!("CA {index}")))
        .collect::<Result<Vec<_>, _>>()?;
    for (index, certificate) in certificates.iter().enumerate() {
        validate_certificate_time(certificate, now, &format!("CA {index}"))?;
        let constraints = certificate
            .basic_constraints()
            .map_err(|source| error(format!("invalid Nitro CA constraints: {source}")))?
            .ok_or_else(|| error("Nitro CA certificate has no basic constraints"))?;
        if !constraints.value.ca {
            return Err(error("Nitro CA bundle contains a non-CA certificate"));
        }
        if certificate
            .key_usage()
            .map_err(|source| error(format!("invalid Nitro CA key usage: {source}")))?
            .is_some_and(|usage| !usage.value.key_cert_sign())
        {
            return Err(error("Nitro CA certificate cannot sign certificates"));
        }
    }

    let mut used = BTreeSet::new();
    let mut current = &leaf;
    loop {
        let candidates = certificates
            .iter()
            .enumerate()
            .filter(|(index, certificate)| {
                !used.contains(index) && certificate.subject() == current.issuer()
            })
            .collect::<Vec<_>>();
        if candidates.len() != 1 {
            return Err(error("Nitro certificate chain has no unique issuer path"));
        }
        let (issuer_index, issuer) = candidates[0];
        let path_length = issuer
            .basic_constraints()
            .map_err(|source| error(format!("invalid Nitro CA constraints: {source}")))?
            .ok_or_else(|| error("Nitro CA certificate has no basic constraints"))?
            .value
            .path_len_constraint;
        if path_length.is_some_and(|maximum| used.len() > maximum as usize) {
            return Err(error("Nitro certificate chain exceeds a CA path limit"));
        }
        current
            .verify_signature(Some(issuer.public_key()))
            .map_err(|source| error(format!("invalid Nitro certificate signature: {source}")))?;
        used.insert(issuer_index);
        if cabundle[issuer_index] == root_der {
            if issuer.subject() != issuer.issuer() {
                return Err(error("pinned Nitro root is not self-issued"));
            }
            issuer
                .verify_signature(None)
                .map_err(|source| error(format!("invalid Nitro root signature: {source}")))?;
            if used.len() != certificates.len() {
                return Err(error(
                    "Nitro CA bundle contains certificates outside the trust path",
                ));
            }
            break;
        }
        current = issuer;
    }
    Ok(leaf_der.to_vec())
}

fn validate_certificate_time(
    certificate: &X509Certificate<'_>,
    now: ASN1Time,
    label: &str,
) -> Result<(), NativeVerificationError> {
    if !certificate.validity().is_valid_at(now) {
        return Err(error(format!(
            "Nitro {label} certificate is not valid at appraisal time"
        )));
    }
    Ok(())
}

fn verify_cose_signature(cose: &CoseSign1, leaf_der: &[u8]) -> Result<(), NativeVerificationError> {
    let leaf = parse_certificate(leaf_der, "leaf")?;
    let verifying_key = VerifyingKey::from_public_key_der(leaf.public_key().raw)
        .map_err(|source| error(format!("invalid Nitro P-384 leaf key: {source}")))?;
    let signature = Signature::from_slice(&cose.signature)
        .map_err(|source| error(format!("invalid Nitro ES384 signature: {source}")))?;
    let signature_structure = Value::Array(vec![
        Value::Text("Signature1".to_owned()),
        Value::Bytes(cose.protected.clone()),
        Value::Bytes(Vec::new()),
        Value::Bytes(cose.payload.clone()),
    ]);
    let message = serde_cbor::to_vec(&signature_structure).map_err(|source| {
        error(format!(
            "could not encode Nitro signature structure: {source}"
        ))
    })?;
    verifying_key
        .verify(&message, &signature)
        .map_err(|source| {
            error(format!(
                "Nitro COSE signature verification failed: {source}"
            ))
        })
}

fn validate_document(
    document: &AttestationDoc,
    context: NativeVerificationContext<'_>,
    policy: &NitroVerificationPolicy,
) -> Result<(), NativeVerificationError> {
    if document.digest != Digest::SHA384 {
        return Err(error("Nitro document does not use the SHA-384 PCR bank"));
    }
    if document.module_id.is_empty()
        || policy
            .expected_module_id
            .as_ref()
            .is_some_and(|expected| expected != &document.module_id)
    {
        return Err(error("Nitro module identity does not match policy"));
    }
    let now_millis = context
        .now_unix_seconds()
        .checked_mul(1_000)
        .ok_or_else(|| error("Nitro appraisal timestamp overflow"))?;
    let oldest = now_millis.saturating_sub(
        policy
            .maximum_age_seconds
            .checked_mul(1_000)
            .ok_or_else(|| error("Nitro maximum age overflow"))?,
    );
    let newest = now_millis
        .checked_add(
            policy
                .maximum_future_skew_seconds
                .checked_mul(1_000)
                .ok_or_else(|| error("Nitro future skew overflow"))?,
        )
        .ok_or_else(|| error("Nitro future timestamp overflow"))?;
    if document.timestamp < oldest || document.timestamp > newest {
        return Err(error(
            "Nitro document timestamp is outside policy freshness",
        ));
    }
    if document.nonce.as_ref().map(|value| value.as_slice())
        != Some(context.challenge().nonce().as_slice())
        || document.public_key.as_ref().map(|value| value.as_slice())
            != Some(context.attestation_binding().guest_public_key())
        || document.user_data.as_ref().map(|value| value.as_slice())
            != Some(context.transcript_digest().as_slice())
    {
        return Err(error(
            "Nitro signed nonce, public key, or user data binding is incorrect",
        ));
    }
    if policy.require_exact_pcr_set && document.pcrs.len() != policy.expected_pcrs.len() {
        return Err(error("Nitro document PCR set is not exact"));
    }
    for (index, expected) in &policy.expected_pcrs {
        if document.pcrs.get(index).map(AsRef::as_ref) != Some(expected.as_slice()) {
            return Err(error(format!("Nitro PCR {index} does not match policy")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cose_parser_requires_protected_es384() {
        let value = Value::Tag(
            18,
            Box::new(Value::Array(vec![
                Value::Bytes(serde_cbor::to_vec(&BTreeMap::from([(1_i8, -7_i8)])).unwrap()),
                Value::Map(BTreeMap::new()),
                Value::Bytes(vec![1]),
                Value::Bytes(vec![0; 96]),
            ])),
        );
        let bytes = serde_cbor::to_vec(&value).unwrap();

        assert_eq!(
            parse_cose_sign1(&bytes).unwrap_err().to_string(),
            "Nitro COSE protected algorithm is not ES384"
        );
    }

    #[test]
    fn policy_rejects_non_sha384_pcr_values() {
        let policy =
            NitroVerificationPolicy::new("policy", vec![1], BTreeMap::from([(0, vec![0; 32])]), 60);

        assert_eq!(
            policy.unwrap_err().to_string(),
            "Nitro policy requires 1 to 32 SHA-384 PCRs at indexes 0 through 31"
        );
    }
}
