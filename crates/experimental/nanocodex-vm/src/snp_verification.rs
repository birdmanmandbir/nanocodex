use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::Deserialize;
use sev::{
    certs::snp::{
        Certificate, Chain, Verifiable,
        builtin::{genoa, milan, turin},
        ca,
    },
    firmware::{
        guest::AttestationReport,
        host::{CertTableEntry, CertType, TcbVersion},
    },
    parser::ByteParser,
};
use uuid::Uuid;
use x509_parser::{
    certificate::X509Certificate,
    prelude::{CertificateRevocationList, FromDer},
    time::ASN1Time,
};

use crate::{
    attestation::{AttestedComponent, EvidenceProfile, RawEvidence},
    verification::{
        NativeEvidenceVerifier, NativeVerificationContext, NativeVerificationError,
        VerifiedNativeBinding, VerifiedNativeEvidence,
    },
};

const TSM_MEDIA_TYPE: &str = "application/vnd.nanocodex.linux-tsm-report+json";
const SNP_PROVIDER: &str = "sev_guest";
const MAX_CERT_TABLE_ENTRIES: usize = 32;
const CERT_TABLE_HEADER_BYTES: usize = 24;
const MAX_CRL_BYTES: usize = 4 * 1024 * 1024;

/// Component-wise SEV-SNP firmware security version.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SnpTcbVersion {
    /// Turin and later firmware component version.
    pub fmc: Option<u8>,
    /// PSP bootloader security version.
    pub bootloader: u8,
    /// PSP OS security version.
    pub tee: u8,
    /// SNP firmware security version.
    pub snp: u8,
    /// Lowest microcode patch level across all cores.
    pub microcode: u8,
}

impl SnpTcbVersion {
    fn is_satisfied_by(self, actual: TcbVersion) -> bool {
        self.fmc
            .is_none_or(|minimum| actual.fmc.is_some_and(|value| value >= minimum))
            && actual.bootloader >= self.bootloader
            && actual.tee >= self.tee
            && actual.snp >= self.snp
            && actual.microcode >= self.microcode
    }
}

/// Revocation behavior selected explicitly by an SNP relying party.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnpRevocationPolicy {
    /// Reject evidence unless appraisal has a current, AMD-signed CRL.
    RequireFreshCrl,
    /// Verify a CRL when present but permit offline evidence without one.
    AllowUnavailable,
}

/// Named relying-party appraisal policy for AMD SEV-SNP reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnpVerificationPolicy {
    policy_id: String,
    measurement: [u8; 48],
    minimum_tcb: SnpTcbVersion,
    revocation: SnpRevocationPolicy,
    minimum_guest_svn: u32,
    allow_smt: bool,
    require_single_socket: bool,
    expected_host_data: Option<[u8; 32]>,
    expected_id_key_digest: Option<[u8; 48]>,
    expected_author_key_digest: Option<[u8; 48]>,
    crl_der: Option<Vec<u8>>,
}

impl SnpVerificationPolicy {
    /// Creates an exact launch-measurement and minimum-TCB policy.
    ///
    /// # Errors
    ///
    /// Returns an error when `policy_id` is empty or unreasonably large.
    pub fn new(
        policy_id: impl Into<String>,
        measurement: [u8; 48],
        minimum_tcb: SnpTcbVersion,
        revocation: SnpRevocationPolicy,
    ) -> Result<Self, NativeVerificationError> {
        let policy_id = policy_id.into();
        if policy_id.is_empty() || policy_id.len() > 256 {
            return Err(NativeVerificationError::new(
                "SNP policy id must contain between 1 and 256 bytes",
            ));
        }
        Ok(Self {
            policy_id,
            measurement,
            minimum_tcb,
            revocation,
            minimum_guest_svn: 0,
            allow_smt: false,
            require_single_socket: false,
            expected_host_data: None,
            expected_id_key_digest: None,
            expected_author_key_digest: None,
            crl_der: None,
        })
    }

    /// Sets the minimum guest security version.
    #[must_use]
    pub const fn with_minimum_guest_svn(mut self, minimum_guest_svn: u32) -> Self {
        self.minimum_guest_svn = minimum_guest_svn;
        self
    }

    /// Permits an SNP policy and platform report with SMT enabled.
    #[must_use]
    pub const fn with_smt_allowed(mut self, allow_smt: bool) -> Self {
        self.allow_smt = allow_smt;
        self
    }

    /// Requires the immutable SNP single-socket launch policy bit.
    #[must_use]
    pub const fn with_single_socket_required(mut self, required: bool) -> Self {
        self.require_single_socket = required;
        self
    }

    /// Requires exact hypervisor-supplied host data.
    #[must_use]
    pub const fn with_host_data(mut self, host_data: [u8; 32]) -> Self {
        self.expected_host_data = Some(host_data);
        self
    }

    /// Requires the exact launch ID-key digest.
    #[must_use]
    pub const fn with_id_key_digest(mut self, digest: [u8; 48]) -> Self {
        self.expected_id_key_digest = Some(digest);
        self
    }

    /// Requires the exact launch author-key digest.
    #[must_use]
    pub const fn with_author_key_digest(mut self, digest: [u8; 48]) -> Self {
        self.expected_author_key_digest = Some(digest);
        self
    }

    /// Supplies an AMD CRL resolved and retained by the relying party.
    ///
    /// A CRL carried in the guest's auxiliary certificate table remains
    /// supported for compatibility, but when both sources are present their
    /// bytes must agree exactly.
    ///
    /// # Errors
    ///
    /// Returns an error when the CRL is empty or exceeds the appraisal bound.
    pub fn with_crl_der(mut self, crl_der: Vec<u8>) -> Result<Self, NativeVerificationError> {
        if crl_der.is_empty() || crl_der.len() > MAX_CRL_BYTES {
            return Err(error(format!(
                "SNP endorsement CRL must contain between 1 and {MAX_CRL_BYTES} bytes"
            )));
        }
        self.crl_der = Some(crl_der);
        Ok(self)
    }
}

/// Pure-Rust, offline verifier for configfs TSM SEV-SNP evidence.
///
/// AMD's Milan, Genoa, and Turin roots are pinned by the maintained `sev`
/// crate. The verifier validates the complete certificate/report signature
/// path, VCEK identity and TCB extensions, trusted appraisal time, report-data
/// binding, and the named launch policy before returning claims.
pub struct SnpVerifier {
    policies: BTreeMap<String, SnpVerificationPolicy>,
}

impl SnpVerifier {
    /// Creates a verifier with one named policy.
    #[must_use]
    pub fn new(policy: SnpVerificationPolicy) -> Self {
        let mut policies = BTreeMap::new();
        policies.insert(policy.policy_id.clone(), policy);
        Self { policies }
    }

    /// Adds another named policy.
    ///
    /// # Errors
    ///
    /// Returns an error rather than replacing an existing policy silently.
    pub fn with_policy(
        mut self,
        policy: SnpVerificationPolicy,
    ) -> Result<Self, NativeVerificationError> {
        if self.policies.contains_key(&policy.policy_id) {
            return Err(NativeVerificationError::new(format!(
                "duplicate SNP policy id {:?}",
                policy.policy_id
            )));
        }
        self.policies.insert(policy.policy_id.clone(), policy);
        Ok(self)
    }
}

#[async_trait]
impl NativeEvidenceVerifier for SnpVerifier {
    async fn verify(
        &self,
        evidence: &RawEvidence,
        context: NativeVerificationContext<'_>,
    ) -> Result<VerifiedNativeEvidence, NativeVerificationError> {
        if evidence.component() != &AttestedComponent::CpuVm
            || evidence.profile() != EvidenceProfile::AmdSevSnp
            || evidence.media_type() != TSM_MEDIA_TYPE
        {
            return Err(error("SNP verifier received evidence of the wrong type"));
        }
        let policy = self
            .policies
            .get(context.challenge().policy_id())
            .ok_or_else(|| error("no SNP policy matches the challenge policy id"))?;
        let document: TsmEvidenceDocument = serde_json::from_slice(evidence.bytes())
            .map_err(|source| error(format!("invalid SNP TSM evidence document: {source}")))?;
        if document.provider != SNP_PROVIDER || document.generation != 1 {
            return Err(error("invalid SNP TSM provider or report generation"));
        }
        if document.manifest.is_some() {
            return Err(error("unexpected SNP TSM manifest evidence"));
        }
        let report_bytes = decode_base64("SNP report", &document.report)?;
        let report = AttestationReport::from_bytes(&report_bytes)
            .map_err(|source| error(format!("invalid SNP attestation report: {source}")))?;
        let auxiliary = document
            .auxiliary
            .as_deref()
            .ok_or_else(|| error("SNP evidence does not contain endorsement certificates"))?;
        let auxiliary = decode_base64("SNP auxiliary certificate table", auxiliary)?;
        let entries = parse_cert_table(&auxiliary)?;
        let vek = unique_certificate(&entries, CertType::VCEK, "VCEK")?;
        if entries
            .iter()
            .any(|entry| entry.cert_type == CertType::VLEK)
        {
            return Err(error(
                "VLEK evidence is not supported by the pinned VCEK roots",
            ));
        }
        let vek = Certificate::from_der(vek)
            .map_err(|source| error(format!("invalid SNP VCEK certificate: {source}")))?;
        let (chain, generation) = trusted_chain(&vek, &report)?;

        let appraisal_time = appraisal_time(context.now_unix_seconds())?;
        validate_certificate_time(&chain.ca.ark, appraisal_time, "ARK")?;
        validate_certificate_time(&chain.ca.ask, appraisal_time, "ASK")?;
        let vek_der = vek
            .to_der()
            .map_err(|source| error(format!("could not encode SNP VCEK: {source}")))?;
        let vek_x509 = parse_certificate(&vek_der, "VCEK")?;
        validate_x509_time(&vek_x509, appraisal_time, "VCEK")?;
        validate_vek_extensions(&vek_x509, &report)?;
        validate_revocation(
            &entries,
            &chain,
            &vek_x509,
            appraisal_time,
            policy.crl_der.as_deref(),
            policy.revocation,
        )?;
        validate_report_policy(&report, context.transcript_digest(), policy)?;

        Ok(VerifiedNativeEvidence::new(
            evidence.digest(),
            AttestedComponent::CpuVm,
            EvidenceProfile::AmdSevSnp,
            VerifiedNativeBinding::CpuTranscript(*context.transcript_digest()),
            true,
            true,
            true,
            format!(
                "amd-sev-snp:{generation}:vcek:{}",
                hex::encode(report.chip_id)
            ),
            None,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TsmEvidenceDocument {
    provider: String,
    generation: u64,
    report: String,
    auxiliary: Option<String>,
    manifest: Option<String>,
}

fn error(message: impl Into<String>) -> NativeVerificationError {
    NativeVerificationError::new(message)
}

fn decode_base64(label: &str, encoded: &str) -> Result<Vec<u8>, NativeVerificationError> {
    BASE64_STANDARD
        .decode(encoded)
        .map_err(|source| error(format!("invalid {label} base64: {source}")))
}

fn parse_cert_table(bytes: &[u8]) -> Result<Vec<CertTableEntry>, NativeVerificationError> {
    let mut raw_entries = Vec::new();
    let mut seen_guids = BTreeSet::new();
    let mut terminator_end = None;
    for index in 0..=MAX_CERT_TABLE_ENTRIES {
        let start = index
            .checked_mul(CERT_TABLE_HEADER_BYTES)
            .ok_or_else(|| error("SNP certificate table header overflow"))?;
        let end = start
            .checked_add(CERT_TABLE_HEADER_BYTES)
            .ok_or_else(|| error("SNP certificate table header overflow"))?;
        let header = bytes
            .get(start..end)
            .ok_or_else(|| error("SNP certificate table has no bounded terminator"))?;
        let guid_bytes: [u8; 16] = header[..16]
            .try_into()
            .map_err(|_| error("invalid SNP certificate GUID"))?;
        let offset = u32::from_le_bytes(
            header[16..20]
                .try_into()
                .map_err(|_| error("invalid SNP certificate offset"))?,
        ) as usize;
        let length = u32::from_le_bytes(
            header[20..24]
                .try_into()
                .map_err(|_| error("invalid SNP certificate length"))?,
        ) as usize;
        if guid_bytes == [0; 16] {
            if offset != 0 || length != 0 {
                return Err(error("malformed SNP certificate table terminator"));
            }
            terminator_end = Some(end);
            break;
        }
        if !seen_guids.insert(guid_bytes) {
            return Err(error("duplicate SNP certificate table GUID"));
        }
        if length == 0 {
            return Err(error("empty SNP certificate table entry"));
        }
        raw_entries.push((Uuid::from_bytes(guid_bytes), offset, length));
    }
    let certificate_start =
        terminator_end.ok_or_else(|| error("SNP certificate table exceeds the entry limit"))?;
    let mut ranges = Vec::with_capacity(raw_entries.len());
    let mut entries = Vec::with_capacity(raw_entries.len());
    for (guid, offset, length) in raw_entries {
        let end = offset
            .checked_add(length)
            .ok_or_else(|| error("SNP certificate table range overflow"))?;
        if offset < certificate_start || end > bytes.len() {
            return Err(error("SNP certificate table range is out of bounds"));
        }
        if ranges
            .iter()
            .any(|&(other_start, other_end)| offset < other_end && other_start < end)
        {
            return Err(error("overlapping SNP certificate table entries"));
        }
        ranges.push((offset, end));
        entries.push(
            CertTableEntry::from_guid(&guid, bytes[offset..end].to_vec())
                .map_err(|source| error(format!("invalid SNP certificate GUID: {source}")))?,
        );
    }
    Ok(entries)
}

fn unique_certificate<'a>(
    entries: &'a [CertTableEntry],
    cert_type: CertType,
    label: &str,
) -> Result<&'a [u8], NativeVerificationError> {
    let mut matching = entries.iter().filter(|entry| entry.cert_type == cert_type);
    let certificate = matching
        .next()
        .ok_or_else(|| error(format!("SNP evidence does not contain a {label}")))?;
    if matching.next().is_some() {
        return Err(error(format!(
            "SNP evidence contains multiple {label} entries"
        )));
    }
    Ok(certificate.data())
}

fn trusted_chain(
    vek: &Certificate,
    report: &AttestationReport,
) -> Result<(Chain, &'static str), NativeVerificationError> {
    let candidates = [
        ("milan", milan::ark(), milan::ask()),
        ("genoa", genoa::ark(), genoa::ask()),
        ("turin", turin::ark(), turin::ask()),
    ];
    let mut trusted = Vec::new();
    for (generation, ark, ask) in candidates {
        let (Ok(ark), Ok(ask)) = (ark, ask) else {
            continue;
        };
        let chain = Chain {
            ca: ca::Chain { ark, ask },
            vek: vek.clone(),
        };
        if (&chain, report).verify().is_ok() {
            trusted.push((chain, generation));
        }
    }
    match trusted.len() {
        1 => trusted
            .pop()
            .ok_or_else(|| error("internal SNP root selection failure")),
        0 => Err(error(
            "SNP report signature does not chain to a pinned AMD root",
        )),
        _ => Err(error(
            "SNP VCEK chains to more than one AMD generation root",
        )),
    }
}

fn appraisal_time(now_unix_seconds: u64) -> Result<ASN1Time, NativeVerificationError> {
    let seconds = i64::try_from(now_unix_seconds)
        .map_err(|_| error("SNP appraisal time is outside the X.509 range"))?;
    ASN1Time::from_timestamp(seconds)
        .map_err(|source| error(format!("invalid SNP appraisal time: {source}")))
}

fn validate_certificate_time(
    certificate: &Certificate,
    now: ASN1Time,
    label: &str,
) -> Result<(), NativeVerificationError> {
    let der = certificate
        .to_der()
        .map_err(|source| error(format!("could not encode SNP {label}: {source}")))?;
    let certificate = parse_certificate(&der, label)?;
    validate_x509_time(&certificate, now, label)
}

fn parse_certificate<'a>(
    der: &'a [u8],
    label: &str,
) -> Result<X509Certificate<'a>, NativeVerificationError> {
    let (remaining, certificate) = X509Certificate::from_der(der)
        .map_err(|source| error(format!("invalid SNP {label} X.509 certificate: {source}")))?;
    if !remaining.is_empty() {
        return Err(error(format!(
            "SNP {label} certificate contains trailing bytes"
        )));
    }
    Ok(certificate)
}

fn validate_x509_time(
    certificate: &X509Certificate<'_>,
    now: ASN1Time,
    label: &str,
) -> Result<(), NativeVerificationError> {
    if !certificate.validity().is_valid_at(now) {
        return Err(error(format!(
            "SNP {label} certificate is not valid at appraisal time"
        )));
    }
    Ok(())
}

fn validate_vek_extensions(
    vek: &X509Certificate<'_>,
    report: &AttestationReport,
) -> Result<(), NativeVerificationError> {
    let common_name = vek
        .subject()
        .iter_common_name()
        .next()
        .and_then(|name| name.as_str().ok())
        .ok_or_else(|| error("SNP VCEK has no UTF-8 common name"))?;
    if !common_name.to_ascii_lowercase().contains("vcek") {
        return Err(error("SNP endorsement certificate is not a VCEK"));
    }
    require_integer_extension(
        vek,
        "1.3.6.1.4.1.3704.1.3.1",
        report.reported_tcb.bootloader,
    )?;
    require_integer_extension(vek, "1.3.6.1.4.1.3704.1.3.2", report.reported_tcb.tee)?;
    require_integer_extension(vek, "1.3.6.1.4.1.3704.1.3.3", report.reported_tcb.snp)?;
    require_integer_extension(vek, "1.3.6.1.4.1.3704.1.3.8", report.reported_tcb.microcode)?;
    if let Some(fmc) = report.reported_tcb.fmc {
        require_integer_extension(vek, "1.3.6.1.4.1.3704.1.3.9", fmc)?;
    }
    let hwid = extension_value(vek, "1.3.6.1.4.1.3704.1.4")?;
    let hwid = parse_octet_string(hwid, 64, "VCEK hardware id")?;
    if hwid != report.chip_id {
        return Err(error("SNP VCEK hardware id does not match report chip id"));
    }
    Ok(())
}

fn extension_value<'a>(
    certificate: &'a X509Certificate<'a>,
    oid: &str,
) -> Result<&'a [u8], NativeVerificationError> {
    certificate
        .extensions()
        .iter()
        .find(|extension| extension.oid.to_id_string() == oid)
        .map(|extension| extension.value)
        .ok_or_else(|| error(format!("SNP VCEK is missing required extension {oid}")))
}

fn require_integer_extension(
    certificate: &X509Certificate<'_>,
    oid: &str,
    expected: u8,
) -> Result<(), NativeVerificationError> {
    let bytes = extension_value(certificate, oid)?;
    let actual = match bytes {
        [0x02, 0x01, value] => *value,
        [0x02, 0x02, 0x00, value] => *value,
        _ => {
            return Err(error(format!(
                "SNP VCEK extension {oid} is not a canonical u8"
            )));
        }
    };
    if actual != expected {
        return Err(error(format!(
            "SNP VCEK extension {oid} does not match report TCB"
        )));
    }
    Ok(())
}

fn parse_octet_string<'a>(
    bytes: &'a [u8],
    expected_length: usize,
    label: &str,
) -> Result<&'a [u8], NativeVerificationError> {
    // Older AMD VCEKs predate the nested X.509 OCTET STRING encoding and
    // carry the fixed-width HWID directly in extnValue.
    if bytes.len() == expected_length {
        return Ok(bytes);
    }
    if expected_length >= 128
        || bytes.len() != expected_length + 2
        || bytes[0] != 0x04
        || bytes[1] as usize != expected_length
    {
        return Err(error(format!("SNP {label} has invalid DER encoding")));
    }
    Ok(&bytes[2..])
}

fn validate_revocation(
    entries: &[CertTableEntry],
    chain: &Chain,
    vek: &X509Certificate<'_>,
    now: ASN1Time,
    relying_party_crl: Option<&[u8]>,
    policy: SnpRevocationPolicy,
) -> Result<(), NativeVerificationError> {
    let crl_entries: Vec<_> = entries
        .iter()
        .filter(|entry| entry.cert_type == CertType::CRL)
        .collect();
    if crl_entries.len() > 1 {
        return Err(error("SNP evidence contains multiple CRL entries"));
    }
    let evidence_crl = crl_entries.first().map(|entry| entry.data());
    let crl_der = match (relying_party_crl, evidence_crl) {
        (Some(external), Some(embedded)) if external != embedded => {
            return Err(error("SNP relying-party and evidence CRLs do not match"));
        }
        (Some(external), _) => Some(external),
        (None, Some(embedded)) => Some(embedded),
        (None, None) => None,
    };
    let Some(crl_der) = crl_der else {
        return match policy {
            SnpRevocationPolicy::RequireFreshCrl => {
                Err(error("SNP policy requires a fresh endorsement CRL"))
            }
            SnpRevocationPolicy::AllowUnavailable => Ok(()),
        };
    };
    let (remaining, crl) = CertificateRevocationList::from_der(crl_der)
        .map_err(|source| error(format!("invalid SNP endorsement CRL: {source}")))?;
    if !remaining.is_empty() {
        return Err(error("SNP endorsement CRL contains trailing bytes"));
    }
    let ark_der = chain
        .ca
        .ark
        .to_der()
        .map_err(|source| error(format!("could not encode SNP ARK: {source}")))?;
    let ask_der = chain
        .ca
        .ask
        .to_der()
        .map_err(|source| error(format!("could not encode SNP ASK: {source}")))?;
    let ark = parse_certificate(&ark_der, "ARK")?;
    let ask = parse_certificate(&ask_der, "ASK")?;
    if crl.verify_signature(ark.public_key()).is_err()
        && crl.verify_signature(ask.public_key()).is_err()
    {
        return Err(error(
            "SNP endorsement CRL is not signed by the trusted chain",
        ));
    }
    if crl.last_update() > now || crl.next_update().is_none_or(|next| next < now) {
        return Err(error(
            "SNP endorsement CRL is not current at appraisal time",
        ));
    }
    let revoked = crl.iter_revoked_certificates().any(|entry| {
        entry.raw_serial() == ark.raw_serial()
            || entry.raw_serial() == ask.raw_serial()
            || entry.raw_serial() == vek.raw_serial()
    });
    if revoked {
        return Err(error(
            "SNP endorsement chain contains a revoked certificate",
        ));
    }
    Ok(())
}

fn validate_report_policy(
    report: &AttestationReport,
    transcript_digest: &[u8; 32],
    policy: &SnpVerificationPolicy,
) -> Result<(), NativeVerificationError> {
    if report.version < 2 || report.sig_algo != 1 || report.vmpl != 0 {
        return Err(error(
            "SNP report version, signature algorithm, or VMPL is insecure",
        ));
    }
    if report.report_data[..32] != transcript_digest[..] || report.report_data[32..] != [0; 32] {
        return Err(error(
            "SNP report data does not contain the canonical transcript binding",
        ));
    }
    if report.measurement != policy.measurement {
        return Err(error("SNP launch measurement does not match policy"));
    }
    if report.guest_svn < policy.minimum_guest_svn
        || !policy.minimum_tcb.is_satisfied_by(report.reported_tcb)
        || !policy.minimum_tcb.is_satisfied_by(report.current_tcb)
        || !policy.minimum_tcb.is_satisfied_by(report.launch_tcb)
    {
        return Err(error(
            "SNP report does not satisfy minimum guest or firmware TCB",
        ));
    }
    if report.policy.debug_allowed() || report.policy.migrate_ma_allowed() {
        return Err(error(
            "SNP report permits debug or migration-agent association",
        ));
    }
    if !policy.allow_smt && (report.policy.smt_allowed() || report.plat_info.smt_enabled()) {
        return Err(error("SNP report permits SMT contrary to policy"));
    }
    if policy.require_single_socket && !report.policy.single_socket_required() {
        return Err(error("SNP report does not require a single socket"));
    }
    if policy
        .expected_host_data
        .is_some_and(|value| value != report.host_data)
        || policy
            .expected_id_key_digest
            .is_some_and(|value| value != report.id_key_digest)
        || policy
            .expected_author_key_digest
            .is_some_and(|value| value != report.author_key_digest)
    {
        return Err(error(
            "SNP report launch identity fields do not match policy",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MILAN_VCEK_BASE64: &str = "MIIFTDCCAvugAwIBAgIBADBGBgkqhkiG9w0BAQowOaAPMA0GCWCGSAFlAwQCAgUAoRwwGgYJKoZIhvcNAQEIMA0GCWCGSAFlAwQCAgUAogMCATCjAwIBATB7MRQwEgYDVQQLDAtFbmdpbmVlcmluZzELMAkGA1UEBhMCVVMxFDASBgNVBAcMC1NhbnRhIENsYXJhMQswCQYDVQQIDAJDQTEfMB0GA1UECgwWQWR2YW5jZWQgTWljcm8gRGV2aWNlczESMBAGA1UEAwwJU0VWLU1pbGFuMB4XDTIzMDQwMzE5MjM0M1oXDTMwMDQwMzE5MjM0M1owejEUMBIGA1UECwwLRW5naW5lZXJpbmcxCzAJBgNVBAYTAlVTMRQwEgYDVQQHDAtTYW50YSBDbGFyYTELMAkGA1UECAwCQ0ExHzAdBgNVBAoMFkFkdmFuY2VkIE1pY3JvIERldmljZXMxETAPBgNVBAMMCFNFVi1WQ0VLMHYwEAYHKoZIzj0CAQYFK4EEACIDYgAEoXrM2Ase3rDTnzCqXAixTHBwBR0pP86ZoN5V5mKnwnaFeqNAZ82ez1IckEicDUB11W5hzNor5pbvsbnU6dFd0v6jKwJIQJrMkUgXIkAtMaI3POGwRlCYBw9PY8x2IKPpo4IBFjCCARIwEAYJKwYBBAGceAEBBAMCAQAwFwYJKwYBBAGceAECBAoWCE1pbGFuLUIwMBEGCisGAQQBnHgBAwEEAwIBAzARBgorBgEEAZx4AQMCBAMCAQAwEQYKKwYBBAGceAEDBAQDAgEAMBEGCisGAQQBnHgBAwUEAwIBADARBgorBgEEAZx4AQMGBAMCAQAwEQYKKwYBBAGceAEDBwQDAgEAMBEGCisGAQQBnHgBAwMEAwIBCDARBgorBgEEAZx4AQMIBAMCAXMwTQYJKwYBBAGceAEEBEDUlVTscX9OWw/msUO88EBb164wRyft9GYD8qdq72o6vBXXrzjbdXA5Ap8O+s/QjiRDJIhHOMcrCC4vh6RNVB62MEYGCSqGSIb3DQEBCjA5oA8wDQYJYIZIAWUDBAICBQChHDAaBgkqhkiG9w0BAQgwDQYJYIZIAWUDBAICBQCiAwIBMKMDAgEBA4ICAQBOizm6CMC5PdFwNp2R50/f3g1GASo8XauAdQTaCfzCGYIEECIe+Unok4J0pR9pbphmd5KvsEemh1SEf28MwqkqmhnXa9HAUzb+uJ/9xjBITtxFubxg2C7Y5z5b7IdeBpYf6cHJS45wRKDPTOJGfpbSO7EQ6+XkMtUaLlbMrGroJLHlfLaFI9cMi5rDkL1nmiXxg5xJiFPo68mVZLiAR168XOGhTA1uVypU5gNsTHAIp4tmOlMj04dRkK8b2rYD20bNv81TRGIzPWdGt6k5uVXGM7VdnkO9LxlQYAXqF/w11p/TIJ4e4WV0vyLuYRJVdfEiswEGQewwWPSaOPFHQZnrpLS2iLJV1S5C9YkM7H+1fZVYeVRR39D9Bk/rCIRCoqkaFUgZZzyVF2SivT6AqLsvDuqg/UsvwFn0VeqkYhBublC6uXD6Zsogl1qYibxfkyO+SAMKil2WLiHEgX34/vS3SaevwCHgFAuR9DxpGOJafjWd0b4p0r+LaU/QWAWt6J+H3Mwkml41XsL91R7iP8V5TlXRgA8s31BJYUSr3Gqryk75+qi+L2zfqf28Gt4vqTH54ekRXL3Hr7UlUo9NF3iLswkpXbOvLvrwiRQgmoMRNdUXp3lUB6XXYUAGAVNj+Y+Xby9Gle+/F4jVcvBqmh3Ger9gvZzUPkpDeTL+fXarEA==";
    const MILAN_REPORT_HEX_BASE64: &str = "MDIwMDAwMDAwMDAwMDAwMDAwMDAwMzAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMTAwMDAwMDAzMDAwMDAwMDAwMDA4NzMwMTAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMEQ0NDdCNTVEMTk3NDkxQkZFMTVDRjI5OEY5REU5OTg2QjdBN0M0QkUyNDY4QjRGNkUyRDUzQjcxRDdDNjQ1ODEwQjBGMkNERkNBMDA0MDQzM0JFMDYzRkMxQTgyOTNGMEYzRjhEQUU3Qjc5RkVDQjNEMUNEODJCRDZBOTNFQkZEN0ExRTVDMjY2QzAxMDhEQkM5QkI5NEZBOTI2OTUxMzIwOTQwOTE1RDBBQUZCNDI0NjRCRDg4QjU3OUVBMTU4RDNFMUEwREMzOUIyQzYwQkQ5NUI5QzQ4MENEODE4NDFGMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDkyQjNCNDdENTlGMEEyQTEwQTc0QzU2Nzg4NjhBODAyMzhDRjU5M0MwMUE4MkYzQ0ZGQjg3OEU5MDRDMjhENUJGRkZGRkZGRkZGRkZGRkZGRkZGRkZGRkZGRkZGRkZGRkZGRkZGRkZGRkZGRkZGRkZGRkZGRkZGRkZGRkZGRkZGMDMwMDAwMDAwMDAwMDg3MzAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMEQ0OTU1NEVDNzE3RjRFNUIwRkU2QjE0M0JDRjA0MDVCRDdBRTMwNDcyN0VERjQ2NjAzRjJBNzZBRUY2QTNBQkMxNUQ3QUYzOERCNzU3MDM5MDI5RjBFRkFDRkQwOEUyNDQzMjQ4ODQ3MzhDNzJCMDgyRTJGODdBNDRENTQxRUI2MDMwMDAwMDAwMDAwMDg3MzA0MzQwMTAwMDQzNDAxMDAwMzAwMDAwMDAwMDAwODczMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwNjFBQjRGMTFBQTY2MTk5NzYyNUYyMzNERjQyQTRBRDU0NDQwRUVCN0E5NkVBNjNERTE3MENCQzI5QzM3QzAwNUNCNTQwNTQ4ODFFQzdEMkJFRTU2OUIwMkQwN0Y4MjcyMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMjA5RDdFQjlCRTkxOUExRDBCQUYxRDU3RkU2RUJGRUFCQkM1M0I3NzhDNkU5NzdFNDBCMTVDQTkzMUJCNkQ0NEM1QUI5RTMwQ0ZEQzczNDZDQjQxQUMwODNCOTBCRjQ5MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMA==";

    fn cert_entry(guid: &str, data: &[u8], offset: usize) -> Vec<u8> {
        let mut entry = Vec::new();
        entry.extend_from_slice(Uuid::parse_str(guid).unwrap().as_bytes());
        entry.extend_from_slice(&(offset as u32).to_le_bytes());
        entry.extend_from_slice(&(data.len() as u32).to_le_bytes());
        entry
    }

    #[test]
    fn bounded_cert_table_parser_rejects_out_of_bounds_data() {
        let mut table = cert_entry(&CertType::VCEK.to_string(), &[1, 2, 3], 48);
        table.extend_from_slice(&[0; CERT_TABLE_HEADER_BYTES]);
        table.extend_from_slice(&[1, 2]);

        assert_eq!(
            parse_cert_table(&table).unwrap_err().to_string(),
            "SNP certificate table range is out of bounds"
        );
    }

    #[test]
    fn bounded_cert_table_parser_rejects_overlaps() {
        let mut table = cert_entry(&CertType::VCEK.to_string(), &[1, 2], 72);
        table.extend_from_slice(&cert_entry(&CertType::ARK.to_string(), &[3, 4], 73));
        table.extend_from_slice(&[0; CERT_TABLE_HEADER_BYTES]);
        table.extend_from_slice(&[1, 2, 3]);

        assert_eq!(
            parse_cert_table(&table).unwrap_err().to_string(),
            "overlapping SNP certificate table entries"
        );
    }

    #[test]
    fn bounded_cert_table_parser_accepts_canonical_table() {
        let mut table = cert_entry(&CertType::VCEK.to_string(), &[1, 2, 3], 48);
        table.extend_from_slice(&[0; CERT_TABLE_HEADER_BYTES]);
        table.extend_from_slice(&[1, 2, 3]);

        let entries = parse_cert_table(&table).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].cert_type, CertType::VCEK);
        assert_eq!(entries[0].data(), &[1, 2, 3]);
    }

    #[test]
    fn verifies_real_milan_signature_chain_and_vcek_identity() {
        let report_hex = BASE64_STANDARD.decode(MILAN_REPORT_HEX_BASE64).unwrap();
        let report_bytes = hex::decode(report_hex).unwrap();
        let report = AttestationReport::from_bytes(&report_bytes).unwrap();
        let vek_der = BASE64_STANDARD.decode(MILAN_VCEK_BASE64).unwrap();
        let vek = Certificate::from_der(&vek_der).unwrap();

        let (_, generation) = trusted_chain(&vek, &report).unwrap();
        let vek_x509 = parse_certificate(&vek_der, "VCEK").unwrap();
        validate_vek_extensions(&vek_x509, &report).unwrap();

        assert_eq!(generation, "milan");
        let mut changed = report;
        changed.measurement[0] ^= 0x80;
        assert!(trusted_chain(&vek, &changed).is_err());
    }

    #[test]
    fn relying_party_crl_is_bounded() {
        let policy = || {
            SnpVerificationPolicy::new(
                "test",
                [0; 48],
                SnpTcbVersion::default(),
                SnpRevocationPolicy::RequireFreshCrl,
            )
            .unwrap()
        };

        assert_eq!(
            policy().with_crl_der(Vec::new()).unwrap_err().to_string(),
            format!("SNP endorsement CRL must contain between 1 and {MAX_CRL_BYTES} bytes")
        );
        assert!(policy().with_crl_der(vec![0; MAX_CRL_BYTES + 1]).is_err());
        assert!(policy().with_crl_der(vec![1]).is_ok());
    }
}
