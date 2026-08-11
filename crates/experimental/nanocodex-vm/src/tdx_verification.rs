use std::collections::BTreeMap;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use dcap_qvl::{
    QuoteCollateralV3, QuotePolicy, configs::RustCryptoConfig, quote::Report, verify::QuoteVerifier,
};
use serde::Deserialize;

use crate::{
    attestation::{
        AttestedComponent, EvidenceProfile, RawEvidence, WorkloadMeasurement, tdx_rtmr_extend,
        tdx_workload_measurement_event,
    },
    verification::{
        NativeEvidenceVerifier, NativeVerificationContext, NativeVerificationError,
        VerifiedNativeBinding, VerifiedNativeEvidence,
    },
};

const TSM_MEDIA_TYPE: &str = "application/vnd.nanocodex.linux-tsm-report+json";
const TDX_PROVIDER: &str = "tdx_guest";

/// Named Intel TDX measurement policy with offline DCAP collateral.
#[derive(Clone, Debug)]
pub struct TdxVerificationPolicy {
    policy_id: String,
    collateral: QuoteCollateralV3,
    intel_root_der: Option<Vec<u8>>,
    mr_td: [u8; 48],
    rt_mrs: [[u8; 48]; 4],
    workload_rtmr3_base: Option<[u8; 48]>,
    mr_config_id: Option<[u8; 48]>,
    mr_owner: Option<[u8; 48]>,
    mr_owner_config: Option<[u8; 48]>,
    xfam: Option<[u8; 8]>,
    allow_dynamic_platform: bool,
    allow_cached_keys: bool,
    allow_smt: bool,
}

impl TdxVerificationPolicy {
    /// Creates a strict up-to-date TDX policy from retained DCAP collateral JSON.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid identifier or collateral document.
    pub fn new(
        policy_id: impl Into<String>,
        collateral_json: &[u8],
        mr_td: [u8; 48],
        rt_mrs: [[u8; 48]; 4],
    ) -> Result<Self, NativeVerificationError> {
        let policy_id = policy_id.into();
        if policy_id.is_empty() || policy_id.len() > 256 {
            return Err(error("TDX policy id must contain between 1 and 256 bytes"));
        }
        let collateral = serde_json::from_slice(collateral_json)
            .map_err(|source| error(format!("invalid TDX DCAP collateral JSON: {source}")))?;
        Ok(Self {
            policy_id,
            collateral,
            intel_root_der: None,
            mr_td,
            rt_mrs,
            workload_rtmr3_base: None,
            mr_config_id: None,
            mr_owner: None,
            mr_owner_config: None,
            xfam: None,
            allow_dynamic_platform: false,
            allow_cached_keys: false,
            allow_smt: false,
        })
    }

    /// Derives the required RTMR3 from this baseline and the attested workload.
    ///
    /// RTMR0 through RTMR2 remain exact. RTMR3 must equal one SHA-384 extend of
    /// the domain-separated workload-manifest event over `baseline`.
    #[must_use]
    pub const fn with_workload_rtmr3(mut self, baseline: [u8; 48]) -> Self {
        self.rt_mrs[3] = baseline;
        self.workload_rtmr3_base = Some(baseline);
        self
    }

    /// Replaces the built-in Intel production root with caller-reviewed DER.
    #[must_use]
    pub fn with_intel_root(mut self, root_der: impl Into<Vec<u8>>) -> Self {
        self.intel_root_der = Some(root_der.into());
        self
    }

    /// Requires exact TD configuration identity.
    #[must_use]
    pub const fn with_mr_config_id(mut self, value: [u8; 48]) -> Self {
        self.mr_config_id = Some(value);
        self
    }

    /// Requires exact TD owner identity.
    #[must_use]
    pub const fn with_mr_owner(mut self, value: [u8; 48]) -> Self {
        self.mr_owner = Some(value);
        self
    }

    /// Requires exact TD owner configuration identity.
    #[must_use]
    pub const fn with_mr_owner_config(mut self, value: [u8; 48]) -> Self {
        self.mr_owner_config = Some(value);
        self
    }

    /// Requires an exact XFAM value.
    #[must_use]
    pub const fn with_xfam(mut self, value: [u8; 8]) -> Self {
        self.xfam = Some(value);
        self
    }

    /// Allows a PCK certificate marked as a dynamic platform.
    #[must_use]
    pub const fn allow_dynamic_platform(mut self, allow: bool) -> Self {
        self.allow_dynamic_platform = allow;
        self
    }

    /// Allows a PCK certificate marked as using cached attestation keys.
    #[must_use]
    pub const fn allow_cached_keys(mut self, allow: bool) -> Self {
        self.allow_cached_keys = allow;
        self
    }

    /// Allows a PCK certificate marked as having simultaneous multithreading enabled.
    #[must_use]
    pub const fn allow_smt(mut self, allow: bool) -> Self {
        self.allow_smt = allow;
        self
    }
}

/// Pure-Rust offline Intel DCAP verifier for TDX quote evidence.
pub struct TdxVerifier {
    policies: BTreeMap<String, TdxVerificationPolicy>,
}

impl TdxVerifier {
    /// Creates a verifier with one named policy.
    #[must_use]
    pub fn new(policy: TdxVerificationPolicy) -> Self {
        let mut policies = BTreeMap::new();
        policies.insert(policy.policy_id.clone(), policy);
        Self { policies }
    }

    /// Adds another named policy without replacing one silently.
    ///
    /// # Errors
    ///
    /// Returns an error for a duplicate policy identifier.
    pub fn with_policy(
        mut self,
        policy: TdxVerificationPolicy,
    ) -> Result<Self, NativeVerificationError> {
        if self.policies.contains_key(&policy.policy_id) {
            return Err(error(format!(
                "duplicate TDX policy id {:?}",
                policy.policy_id
            )));
        }
        self.policies.insert(policy.policy_id.clone(), policy);
        Ok(self)
    }
}

#[async_trait]
impl NativeEvidenceVerifier for TdxVerifier {
    async fn verify(
        &self,
        evidence: &RawEvidence,
        context: NativeVerificationContext<'_>,
    ) -> Result<VerifiedNativeEvidence, NativeVerificationError> {
        if evidence.component() != &AttestedComponent::CpuVm
            || evidence.profile() != EvidenceProfile::IntelTdx
            || evidence.media_type() != TSM_MEDIA_TYPE
        {
            return Err(error("TDX verifier received evidence of the wrong type"));
        }
        let policy = self
            .policies
            .get(context.challenge().policy_id())
            .ok_or_else(|| error("no TDX policy matches the challenge policy id"))?;
        if policy.workload_rtmr3_base.is_some()
            && context.attestation_binding().workload_measurement() != WorkloadMeasurement::TdxRtmr3
        {
            return Err(error(
                "TDX workload-RTMR3 policy requires an RTMR3-measured attestation request",
            ));
        }
        let document: TsmEvidenceDocument = serde_json::from_slice(evidence.bytes())
            .map_err(|source| error(format!("invalid TDX TSM evidence document: {source}")))?;
        if document.provider != TDX_PROVIDER || document.generation != 1 {
            return Err(error("invalid TDX TSM provider or report generation"));
        }
        // Validate optional configfs outputs even though DCAP appraisal uses the quote itself.
        for (label, value) in [
            ("TDX auxiliary evidence", document.auxiliary.as_deref()),
            ("TDX manifest evidence", document.manifest.as_deref()),
        ] {
            if let Some(value) = value {
                BASE64_STANDARD
                    .decode(value)
                    .map_err(|source| error(format!("invalid {label} base64: {source}")))?;
            }
        }
        let quote = BASE64_STANDARD
            .decode(document.report)
            .map_err(|source| error(format!("invalid TDX quote base64: {source}")))?;
        let verifier = match &policy.intel_root_der {
            Some(root) => QuoteVerifier::new(root.clone()),
            None => QuoteVerifier::new_prod(),
        }
        .with_config::<RustCryptoConfig>();
        let now = context.now_unix_seconds();
        let quote_policy = QuotePolicy::strict(now)
            .allow_dynamic_platform(policy.allow_dynamic_platform)
            .allow_cached_keys(policy.allow_cached_keys)
            .allow_smt(policy.allow_smt);
        let claims = verifier
            .verify_with_policy(&quote, policy.collateral.clone(), now, &quote_policy)
            .map_err(|source| error(format!("Intel DCAP quote verification failed: {source:#}")))?;
        if claims.tee_type != 0x81 {
            return Err(error("DCAP evidence is not a TDX quote"));
        }
        let report = match &claims.report {
            Report::TD10(report) => report,
            Report::TD15(report) => &report.base,
            Report::SgxEnclave(_) => return Err(error("DCAP evidence contains an SGX report")),
        };
        validate_report(
            report,
            context.transcript_digest(),
            context.attestation_binding().workload_manifest_digest(),
            policy,
        )?;

        Ok(VerifiedNativeEvidence::new(
            evidence.digest(),
            AttestedComponent::CpuVm,
            EvidenceProfile::IntelTdx,
            VerifiedNativeBinding::CpuTranscript(*context.transcript_digest()),
            true,
            true,
            true,
            format!(
                "intel-tdx:{}:{}",
                hex::encode(claims.platform.pck.fmspc),
                hex::encode(&claims.platform.pck.ppid)
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

fn validate_report(
    report: &dcap_qvl::quote::TDReport10,
    transcript_digest: &[u8; 32],
    workload_manifest_digest: &[u8; 32],
    policy: &TdxVerificationPolicy,
) -> Result<(), NativeVerificationError> {
    if report.report_data[..32] != transcript_digest[..] || report.report_data[32..] != [0; 32] {
        return Err(error(
            "TDX REPORTDATA does not contain the canonical transcript binding",
        ));
    }
    let expected_rtmr3 = policy.workload_rtmr3_base.map_or(policy.rt_mrs[3], |base| {
        tdx_rtmr_extend(
            &base,
            &tdx_workload_measurement_event(workload_manifest_digest),
        )
    });
    if report.mr_td != policy.mr_td
        || [report.rt_mr0, report.rt_mr1, report.rt_mr2] != policy.rt_mrs[..3]
        || report.rt_mr3 != expected_rtmr3
    {
        return Err(error(format!(
            "TDX MRTD or RTMR measurements do not match policy (actual MRTD={}, RTMR0={}, RTMR1={}, RTMR2={}, RTMR3={})",
            hex::encode(report.mr_td),
            hex::encode(report.rt_mr0),
            hex::encode(report.rt_mr1),
            hex::encode(report.rt_mr2),
            hex::encode(report.rt_mr3),
        )));
    }
    if policy
        .mr_config_id
        .is_some_and(|expected| expected != report.mr_config_id)
        || policy
            .mr_owner
            .is_some_and(|expected| expected != report.mr_owner)
        || policy
            .mr_owner_config
            .is_some_and(|expected| expected != report.mr_owner_config)
        || policy.xfam.is_some_and(|expected| expected != report.xfam)
    {
        return Err(error("TDX launch identity fields do not match policy"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_rejects_invalid_collateral() {
        let error = TdxVerificationPolicy::new("policy", b"{}", [0; 48], [[0; 48]; 4]).unwrap_err();

        assert!(
            error
                .to_string()
                .starts_with("invalid TDX DCAP collateral JSON:")
        );
    }
}
