use std::{
    ffi::OsString,
    io::{self, Write as _},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::timeout,
};

use crate::{
    attestation::{EvidenceProfile, MAX_RAW_EVIDENCE_BYTES, RawEvidence},
    verification::{
        NativeEvidenceVerifier, NativeVerificationContext, NativeVerificationError,
        VerifiedNativeBinding, VerifiedNativeEvidence,
    },
};

const NVATTEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Local NVIDIA NVAT verifier for collected GPU and NVSwitch evidence.
///
/// The configured `nvattest` executable owns native certificate, RIM, OCSP,
/// signature, nonce, and measurement validation. Nanocodex then parses the
/// exact JSON claims and applies its independent composite policy. The caller
/// must provide a Rego policy selecting exact accepted models and reference
/// values, and owns installation, executable provenance, and NVAT's local
/// collateral and network configuration.
#[derive(Clone, Debug)]
pub struct NvidiaNvattestVerifier {
    program: PathBuf,
    relying_party_policy: PathBuf,
}

impl NvidiaNvattestVerifier {
    /// Uses `nvattest` from `PATH` and one mandatory operator-owned Rego policy.
    #[must_use]
    pub fn local(relying_party_policy: impl Into<PathBuf>) -> Self {
        Self {
            program: PathBuf::from("nvattest"),
            relying_party_policy: relying_party_policy.into(),
        }
    }

    /// Uses one explicit NVAT executable and mandatory operator-owned Rego policy.
    #[must_use]
    pub fn program(program: impl Into<PathBuf>, relying_party_policy: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            relying_party_policy: relying_party_policy.into(),
        }
    }
}

#[async_trait]
impl NativeEvidenceVerifier for NvidiaNvattestVerifier {
    async fn verify(
        &self,
        evidence: &RawEvidence,
        context: NativeVerificationContext<'_>,
    ) -> Result<VerifiedNativeEvidence, NativeVerificationError> {
        let (device, file_option) = match evidence.profile() {
            EvidenceProfile::NvidiaGpu => ("gpu", "--gpu-evidence-file"),
            EvidenceProfile::NvidiaNvSwitch => ("nvswitch", "--nvswitch-evidence-file"),
            profile => {
                return Err(NativeVerificationError::new(format!(
                    "NVAT cannot verify {profile:?} evidence"
                )));
            }
        };
        let entry: Value = serde_json::from_slice(evidence.bytes()).map_err(|error| {
            NativeVerificationError::new(format!("invalid collected NVAT evidence JSON: {error}"))
        })?;
        let mut evidence_file = NamedTempFile::new().map_err(native_io("create NVAT evidence"))?;
        serde_json::to_writer(
            &mut evidence_file,
            &json!({
                "evidences": [entry],
                "result_code": 0,
                "result_message": "Ok"
            }),
        )
        .map_err(|error| {
            NativeVerificationError::new(format!("encode NVAT evidence file: {error}"))
        })?;
        evidence_file
            .flush()
            .map_err(native_io("flush NVAT evidence"))?;

        let nonce = encode_hex(context.challenge().nonce());
        let mut arguments = vec![
            OsString::from("attest"),
            OsString::from("--device"),
            OsString::from(device),
            OsString::from("--nonce"),
            OsString::from(&nonce),
            OsString::from("--verifier"),
            OsString::from("local"),
            OsString::from(if device == "gpu" {
                "--gpu-evidence-source"
            } else {
                "--nvswitch-evidence-source"
            }),
            OsString::from("file"),
            OsString::from(file_option),
            evidence_file.path().as_os_str().to_owned(),
            OsString::from("--format"),
            OsString::from("json"),
        ];
        arguments.push(OsString::from("--relying-party-policy"));
        arguments.push(self.relying_party_policy.as_os_str().to_owned());
        let output = run_bounded(&self.program, &arguments).await?;
        let document: Value = serde_json::from_slice(&output).map_err(|error| {
            NativeVerificationError::new(format!("invalid NVAT appraisal JSON: {error}"))
        })?;
        let result_code = document
            .get("result_code")
            .and_then(Value::as_i64)
            .unwrap_or(-1);
        if result_code != 0 {
            return Err(NativeVerificationError::new(format!(
                "NVAT appraisal failed with code {result_code}: {}",
                document
                    .get("result_message")
                    .and_then(Value::as_str)
                    .unwrap_or("missing result_message")
            )));
        }
        if document.get("detached_eat").is_none_or(Value::is_null) {
            return Err(NativeVerificationError::new(
                "NVAT appraisal omitted its signed detached EAT",
            ));
        }
        let claims = document
            .get("claims")
            .and_then(Value::as_array)
            .filter(|claims| claims.len() == 1)
            .ok_or_else(|| {
                NativeVerificationError::new("NVAT must return exactly one claim per evidence")
            })?;
        verified_claims(evidence, device, &nonce, &claims[0])
    }
}

fn verified_claims(
    evidence: &RawEvidence,
    device: &str,
    expected_nonce: &str,
    claim: &Value,
) -> Result<VerifiedNativeEvidence, NativeVerificationError> {
    let claim_device = required_string(claim, "x-nvidia-device-type")?;
    let nonce = required_string(claim, "eat_nonce")?;
    let ueid = required_string(claim, "ueid")?;
    let model = required_string(claim, "hwmodel")?;
    let measurement_success = required_string(claim, "measres")?.eq_ignore_ascii_case("success");
    let secure_boot = claim.get("secboot").and_then(Value::as_bool) == Some(true);
    let debug_disabled = required_string(claim, "dbgstat")?.eq_ignore_ascii_case("disabled");
    let (parsed, signature, nonce_match, architecture) = if device == "gpu" {
        (
            required_bool(claim, "x-nvidia-gpu-attestation-report-parsed")?,
            required_bool(claim, "x-nvidia-gpu-attestation-report-signature-verified")?,
            required_bool(claim, "x-nvidia-gpu-attestation-report-nonce-match")?,
            required_bool(claim, "x-nvidia-gpu-arch-check")?,
        )
    } else {
        (
            required_bool(claim, "x-nvidia-switch-attestation-report-parsed")?,
            required_bool(
                claim,
                "x-nvidia-switch-attestation-report-signature-verified",
            )?,
            required_bool(claim, "x-nvidia-switch-attestation-report-nonce-match")?,
            required_bool(claim, "x-nvidia-switch-arch-check")?,
        )
    };
    let policy_passed = claim_device == device
        && nonce.eq_ignore_ascii_case(expected_nonce)
        && measurement_success
        && parsed
        && signature
        && nonce_match
        && architecture;
    // Current NVAT claims expose switch PDI relationships but no signed
    // administrative/link state. Absence of a switch PDI cannot prove that
    // every NVLink is disabled. They also do not establish encrypted MPT
    // topology, so both exact fabric policies need additional bound evidence.
    let nvidia_fabric = None;
    Ok(VerifiedNativeEvidence::new(
        evidence.digest(),
        evidence.component().clone(),
        evidence.profile(),
        VerifiedNativeBinding::NvidiaNonce(decode_nonce(nonce)?),
        policy_passed,
        secure_boot,
        debug_disabled,
        format!("{ueid}:{model}"),
        nvidia_fabric,
    ))
}

fn required_string<'a>(claim: &'a Value, name: &str) -> Result<&'a str, NativeVerificationError> {
    claim.get(name).and_then(Value::as_str).ok_or_else(|| {
        NativeVerificationError::new(format!("NVAT claim {name:?} is missing or not a string"))
    })
}

fn required_bool(claim: &Value, name: &str) -> Result<bool, NativeVerificationError> {
    claim.get(name).and_then(Value::as_bool).ok_or_else(|| {
        NativeVerificationError::new(format!("NVAT claim {name:?} is missing or not boolean"))
    })
}

fn decode_nonce(nonce: &str) -> Result<[u8; 32], NativeVerificationError> {
    let bytes = hex::decode(nonce).map_err(|error| {
        NativeVerificationError::new(format!("invalid NVAT claim nonce: {error}"))
    })?;
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        NativeVerificationError::new(format!(
            "NVAT claim nonce is {} bytes; expected 32",
            bytes.len()
        ))
    })
}

fn native_io(operation: &'static str) -> impl FnOnce(io::Error) -> NativeVerificationError {
    move |error| NativeVerificationError::new(format!("{operation}: {error}"))
}

async fn run_bounded(
    program: &PathBuf,
    arguments: &[OsString],
) -> Result<Vec<u8>, NativeVerificationError> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(native_io("start nvattest"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| NativeVerificationError::new("nvattest stdout pipe was not created"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| NativeVerificationError::new("nvattest stderr pipe was not created"))?;
    let stdout_task = tokio::spawn(read_bounded(stdout));
    let stderr_task = tokio::spawn(read_bounded(stderr));
    let status = match timeout(NVATTEST_TIMEOUT, child.wait()).await {
        Ok(result) => result.map_err(native_io("wait for nvattest"))?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(NativeVerificationError::new(
                "nvattest exceeded its 120-second deadline",
            ));
        }
    };
    let stdout = stdout_task
        .await
        .map_err(|error| NativeVerificationError::new(format!("join NVAT stdout: {error}")))??;
    let stderr = stderr_task
        .await
        .map_err(|error| NativeVerificationError::new(format!("join NVAT stderr: {error}")))??;
    if !status.success() {
        return Err(NativeVerificationError::new(format!(
            "nvattest failed with {status}: {}",
            String::from_utf8_lossy(&stderr)
        )));
    }
    Ok(stdout)
}

async fn read_bounded<R>(reader: R) -> Result<Vec<u8>, NativeVerificationError>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    reader
        .take((MAX_RAW_EVIDENCE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(native_io("read nvattest output"))?;
    if bytes.len() > MAX_RAW_EVIDENCE_BYTES {
        return Err(NativeVerificationError::new(
            "nvattest output exceeded the native evidence bound",
        ));
    }
    Ok(bytes)
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attestation::AttestedComponent;

    fn gpu_evidence() -> RawEvidence {
        RawEvidence::new(
            AttestedComponent::NvidiaGpu { index: 0 },
            EvidenceProfile::NvidiaGpu,
            "application/json",
            b"{}".to_vec(),
        )
        .unwrap()
    }

    #[test]
    fn no_switch_pdi_does_not_prove_disabled_nvlink() {
        let nonce = [0x42; 32];
        let nonce_hex = encode_hex(&nonce);
        let claim = json!({
            "x-nvidia-device-type": "gpu",
            "eat_nonce": nonce_hex,
            "ueid": "gpu-ueid",
            "hwmodel": "B200",
            "measres": "success",
            "secboot": true,
            "dbgstat": "disabled",
            "x-nvidia-gpu-attestation-report-parsed": true,
            "x-nvidia-gpu-attestation-report-signature-verified": true,
            "x-nvidia-gpu-attestation-report-nonce-match": true,
            "x-nvidia-gpu-arch-check": true,
            "x-nvidia-gpu-switch-pdis": []
        });

        let verified = verified_claims(&gpu_evidence(), "gpu", &nonce_hex, &claim).unwrap();
        assert_eq!(
            verified.binding(),
            &VerifiedNativeBinding::NvidiaNonce(nonce)
        );
        assert_eq!(verified.nvidia_fabric(), None);
    }

    #[test]
    fn switch_connected_claim_does_not_invent_encrypted_mpt() {
        let nonce_hex = encode_hex(&[0x42; 32]);
        let mut claim = json!({
            "x-nvidia-device-type": "gpu",
            "eat_nonce": nonce_hex,
            "ueid": "gpu-ueid",
            "hwmodel": "B200",
            "measres": "success",
            "secboot": true,
            "dbgstat": "disabled",
            "x-nvidia-gpu-attestation-report-parsed": true,
            "x-nvidia-gpu-attestation-report-signature-verified": true,
            "x-nvidia-gpu-attestation-report-nonce-match": true,
            "x-nvidia-gpu-arch-check": true,
            "x-nvidia-gpu-switch-pdis": ["pdi-1"]
        });
        let expected_nonce = claim["eat_nonce"].as_str().unwrap().to_owned();

        let verified = verified_claims(&gpu_evidence(), "gpu", &expected_nonce, &claim).unwrap();
        assert_eq!(verified.nvidia_fabric(), None);
        claim["x-nvidia-gpu-attestation-report-signature-verified"] = Value::Bool(false);
        let untrusted = verified_claims(&gpu_evidence(), "gpu", &expected_nonce, &claim).unwrap();
        assert!(!untrusted.policy_passed());
    }
}
