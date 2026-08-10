use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use ed25519_dalek::{Signer as _, SigningKey};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::timeout,
};

use crate::attestation::{
    AttestationInputError, AttestedComponent, AttestedGuestKeyProof, CpuAttestationProfile,
    EvidenceProfile, GuestAttestation, GuestAttestationBundle, GuestAttestationParameters,
    GuestAttestationRequest, MAX_RAW_EVIDENCE_BYTES, NvidiaAttestationProfile, RawEvidence,
    key_proof_message,
};
use crate::command_proof::{ExecutionRecord, receipt_signature_message};

const TSM_REPORT_ROOT: &str = "/sys/kernel/config/tsm/report";
const NVATTEST: &str = "nvattest";
const NVATTEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_COMMAND_OUTPUT_BYTES: usize = MAX_RAW_EVIDENCE_BYTES;
const NVIDIA_VENDOR_ID: &str = "0x10de";
const NVIDIA_B200_DEVICE_ID: &str = "0x2901";
static REPORT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Failure while collecting native evidence inside a confidential guest.
#[derive(Debug, Error)]
pub enum GuestAttestationError {
    /// The request contains invalid binding inputs.
    #[error("invalid guest attestation request: {0}")]
    InvalidRequest(#[from] AttestationInputError),
    /// The guest could not create its retained Ed25519 attestation identity.
    #[error("failed to generate the guest attestation identity: {0}")]
    IdentityRandom(#[from] getrandom::Error),
    /// No supported confidential CPU environment was visible to the guest.
    #[error("no supported guest CPU TEE was detected: {reason}")]
    NoSupportedCpuTee {
        /// Actionable probe failure.
        reason: String,
    },
    /// Linux exposed a TSM provider this collector does not understand.
    #[error("unsupported Linux TSM provider {provider:?}")]
    UnsupportedTsmProvider {
        /// Provider reported by the kernel.
        provider: String,
    },
    /// The visible B200 count is not one of the reviewed topologies.
    #[error("detected {count} NVIDIA B200 GPUs; supported automatic topologies are 1 and 8")]
    UnsupportedB200Topology {
        /// Visible B200 PCI function count.
        count: usize,
    },
    /// PCI topology inspection failed.
    #[error("failed to inspect PCI topology at {path}: {source}")]
    PciIo {
        /// Path which failed.
        path: PathBuf,
        /// Underlying filesystem failure.
        source: io::Error,
    },
    /// The Nitro Secure Module device could not be opened.
    #[error("AWS Nitro Secure Module device /dev/nsm is unavailable")]
    NitroNsmUnavailable,
    /// The Nitro Secure Module rejected the attestation request.
    #[error("AWS Nitro Secure Module attestation failed: {response}")]
    NitroNsmResponse {
        /// Debug representation of the unexpected native response.
        response: String,
    },
    /// An owned evidence-collection task could not be joined.
    #[error("guest evidence-collection task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    /// A Linux TSM configfs operation failed.
    #[error("Linux TSM report operation at {path} failed: {source}")]
    TsmIo {
        /// Path which failed.
        path: PathBuf,
        /// Underlying filesystem failure.
        source: io::Error,
    },
    /// Linux selected a different TSM provider than requested.
    #[error("Linux TSM provider is {actual:?}; expected {expected:?}")]
    TsmProviderMismatch {
        /// Requested provider.
        expected: String,
        /// Provider reported by configfs.
        actual: String,
    },
    /// Another writer changed the otherwise private report instance.
    #[error("Linux TSM report generation is {actual}; expected exactly one input write")]
    TsmGenerationMismatch {
        /// Observed generation counter.
        actual: u64,
    },
    /// The NVIDIA attestation executable could not be started or awaited.
    #[error("failed to run {program}: {source}")]
    CommandIo {
        /// Program being executed.
        program: &'static str,
        /// Underlying process failure.
        source: io::Error,
    },
    /// NVIDIA evidence collection exceeded its deadline.
    #[error("{program} exceeded its {seconds}-second deadline")]
    CommandTimeout {
        /// Program being executed.
        program: &'static str,
        /// Configured deadline.
        seconds: u64,
    },
    /// NVIDIA evidence collection produced more than the protocol permits.
    #[error("{program} {stream} exceeded {maximum} bytes")]
    CommandOutputTooLarge {
        /// Program being executed.
        program: &'static str,
        /// Stream which overflowed.
        stream: &'static str,
        /// Protocol limit.
        maximum: usize,
    },
    /// NVIDIA evidence collection returned a failed process status.
    #[error("{program} failed with {status}: {stderr}")]
    CommandFailed {
        /// Program being executed.
        program: &'static str,
        /// Display form of the exit status.
        status: String,
        /// Bounded diagnostic output.
        stderr: String,
    },
    /// NVIDIA evidence output was not valid JSON.
    #[error("invalid {device} evidence JSON: {source}")]
    NvidiaJson {
        /// Device class being collected.
        device: &'static str,
        /// JSON decoding failure.
        source: serde_json::Error,
    },
    /// NVIDIA's SDK reported a non-success result.
    #[error("NVIDIA {device} evidence failed with code {code}: {message}")]
    NvidiaResult {
        /// Device class being collected.
        device: &'static str,
        /// NVAT return code.
        code: i64,
        /// NVAT return message.
        message: String,
    },
    /// NVIDIA returned a topology other than the exact requested topology.
    #[error("NVIDIA returned {actual} {device} evidence objects; expected {expected}")]
    NvidiaTopologyMismatch {
        /// Device class being collected.
        device: &'static str,
        /// Exact required count.
        expected: usize,
        /// Observed count.
        actual: usize,
    },
    /// A device ordinal cannot be represented by the protocol.
    #[error("NVIDIA {device} ordinal {index} exceeds the protocol limit")]
    NvidiaOrdinalOverflow {
        /// Device class being collected.
        device: &'static str,
        /// Unrepresentable ordinal.
        index: usize,
    },
}

/// Guest-retained Ed25519 identity bound into native attestation evidence.
pub struct GuestAttestationIdentity {
    signing_key: SigningKey,
}

impl GuestAttestationIdentity {
    /// Generates a new identity from the operating system CSPRNG.
    ///
    /// # Errors
    ///
    /// Returns an error when the guest kernel cannot provide secure randomness.
    pub fn generate() -> Result<Self, GuestAttestationError> {
        let mut secret = [0_u8; 32];
        getrandom::fill(&mut secret)?;
        Ok(Self {
            signing_key: SigningKey::from_bytes(&secret),
        })
    }

    /// Returns the Ed25519 public key bound into evidence generated by this identity.
    #[must_use]
    pub fn public_key(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    /// Collects native evidence and proves possession of its bound guest key.
    ///
    /// # Errors
    ///
    /// Returns an error unless every requested component produces bounded
    /// native evidence.
    pub async fn collect(
        &self,
        parameters: GuestAttestationParameters,
    ) -> Result<GuestAttestation, GuestAttestationError> {
        let request = parameters.into_request(self.public_key().to_vec())?;
        let bundle = collect_attestation(request).await?;
        let signature = self
            .signing_key
            .sign(&key_proof_message(bundle.transcript_digest()))
            .to_bytes();
        Ok(GuestAttestation::new(
            bundle,
            AttestedGuestKeyProof::new(signature),
        ))
    }

    pub(crate) fn sign_execution_record(&self, record: &ExecutionRecord) -> [u8; 64] {
        self.signing_key
            .sign(&receipt_signature_message(record))
            .to_bytes()
    }
}

/// Detects the confidential CPU architecture visible inside the current guest.
///
/// Nitro NSM is checked first. SNP and TDX are identified through a private
/// Linux configfs TSM report instance rather than host-controlled CPU flags.
///
/// # Errors
///
/// Returns an actionable error when no supported native attestation provider
/// is visible or configfs cannot be accessed.
pub async fn detect_cpu_attestation_profile() -> Result<CpuAttestationProfile, GuestAttestationError>
{
    if fs::metadata("/dev/nsm").await.is_ok() {
        return Ok(CpuAttestationProfile::AwsNitro);
    }

    ensure_tsm_report_root().await?;
    let root = Path::new(TSM_REPORT_ROOT);
    let sequence = REPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let instance = root.join(format!("nanocodex-probe-{}-{sequence}", std::process::id()));
    fs::create_dir(&instance)
        .await
        .map_err(|source| tsm_io(&instance, source))?;
    let _guard = ReportInstance(instance.clone());
    let provider = read_file(&instance.join("provider")).await?;
    let provider = String::from_utf8_lossy(&provider).trim().to_owned();
    match provider.as_str() {
        "sev_guest" => Ok(CpuAttestationProfile::AmdSevSnp),
        "tdx_guest" => Ok(CpuAttestationProfile::IntelTdx),
        _ => Err(GuestAttestationError::UnsupportedTsmProvider { provider }),
    }
}

/// Detects one of the exact reviewed NVIDIA B200 topologies visible to the guest.
///
/// # Errors
///
/// Returns an error when B200 devices are visible but their count is neither
/// the single-GPU nor complete eight-GPU topology.
pub async fn detect_nvidia_attestation_profile()
-> Result<Option<NvidiaAttestationProfile>, GuestAttestationError> {
    let mut devices = match fs::read_dir("/sys/bus/pci/devices").await {
        Ok(devices) => devices,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(pci_io("/sys/bus/pci/devices", source)),
    };
    let mut count = 0_usize;
    while let Some(device) = devices
        .next_entry()
        .await
        .map_err(|source| pci_io("/sys/bus/pci/devices", source))?
    {
        let vendor_path = device.path().join("vendor");
        let vendor = fs::read_to_string(&vendor_path)
            .await
            .map_err(|source| pci_io(vendor_path, source))?;
        let device_path = device.path().join("device");
        let device_id = fs::read_to_string(&device_path)
            .await
            .map_err(|source| pci_io(device_path, source))?;
        if vendor.trim().eq_ignore_ascii_case(NVIDIA_VENDOR_ID)
            && device_id.trim().eq_ignore_ascii_case(NVIDIA_B200_DEVICE_ID)
        {
            count += 1;
        }
    }
    nvidia_profile_for_b200_count(count)
}

const fn nvidia_profile_for_b200_count(
    count: usize,
) -> Result<Option<NvidiaAttestationProfile>, GuestAttestationError> {
    match count {
        0 => Ok(None),
        1 => Ok(Some(NvidiaAttestationProfile::B200Single)),
        8 => Ok(Some(NvidiaAttestationProfile::B200Hgx8EncryptedNvlink)),
        count => Err(GuestAttestationError::UnsupportedB200Topology { count }),
    }
}

/// Collects accelerator evidence and binds it into native CPU evidence.
///
/// The resulting order is all GPUs, all NVSwitches, and finally the CPU VM.
/// The CPU report-data field contains the canonical transcript digest, making
/// removal, replacement, or reordering of accelerator evidence detectable.
///
/// # Errors
///
/// Returns an error unless every requested component produces bounded native
/// evidence and the selected CPU TEE binds the resulting transcript.
pub async fn collect_attestation(
    request: GuestAttestationRequest,
) -> Result<GuestAttestationBundle, GuestAttestationError> {
    // Deserialization does not get to bypass the constructor's input bounds.
    let validated = GuestAttestationRequest::new(
        request.challenge().clone(),
        request.guest_public_key().to_vec(),
        *request.workload_manifest_digest(),
        request.cpu_profile(),
        request.nvidia_profile(),
    )?;

    let mut evidence = collect_nvidia_evidence(&validated).await?;
    let binding = validated.binding(evidence.iter().map(RawEvidence::digest).collect())?;
    let transcript_digest = binding.transcript_digest();
    evidence.push(collect_cpu_evidence(&validated, transcript_digest).await?);

    Ok(GuestAttestationBundle::new(
        validated,
        transcript_digest,
        evidence,
    ))
}

async fn collect_nvidia_evidence(
    request: &GuestAttestationRequest,
) -> Result<Vec<RawEvidence>, GuestAttestationError> {
    let Some(profile) = request.nvidia_profile() else {
        return Ok(Vec::new());
    };
    let nonce = encode_hex(request.challenge().nonce());
    let mut evidence = collect_nvidia_device("gpu", profile.gpu_count(), &nonce).await?;
    if profile.switch_count() != 0 {
        evidence.extend(collect_nvidia_device("nvswitch", profile.switch_count(), &nonce).await?);
    }
    Ok(evidence)
}

async fn collect_nvidia_device(
    device: &'static str,
    expected: usize,
    nonce: &str,
) -> Result<Vec<RawEvidence>, GuestAttestationError> {
    let output = run_bounded_command(
        NVATTEST,
        [
            "collect-evidence",
            "--device",
            device,
            "--nonce",
            nonce,
            "--format",
            "json",
        ],
    )
    .await?;
    let document: Value = serde_json::from_slice(&output)
        .map_err(|source| GuestAttestationError::NvidiaJson { device, source })?;
    let result_code = document
        .get("result_code")
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    if result_code != 0 {
        return Err(GuestAttestationError::NvidiaResult {
            device,
            code: result_code,
            message: document
                .get("result_message")
                .and_then(Value::as_str)
                .unwrap_or("missing result_message")
                .to_owned(),
        });
    }
    let entries = document
        .get("evidences")
        .and_then(Value::as_array)
        .ok_or_else(|| GuestAttestationError::NvidiaTopologyMismatch {
            device,
            expected,
            actual: 0,
        })?;
    if entries.len() != expected {
        return Err(GuestAttestationError::NvidiaTopologyMismatch {
            device,
            expected,
            actual: entries.len(),
        });
    }

    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let index = u16::try_from(index)
                .map_err(|_| GuestAttestationError::NvidiaOrdinalOverflow { device, index })?;
            let bytes = serde_json::to_vec(entry)
                .map_err(|source| GuestAttestationError::NvidiaJson { device, source })?;
            let (component, profile) = if device == "gpu" {
                (
                    AttestedComponent::NvidiaGpu { index },
                    EvidenceProfile::NvidiaGpu,
                )
            } else {
                (
                    AttestedComponent::NvidiaNvSwitch { index },
                    EvidenceProfile::NvidiaNvSwitch,
                )
            };
            RawEvidence::new(
                component,
                profile,
                "application/vnd.nvidia.nvat.evidence+json",
                bytes,
            )
            .map_err(Into::into)
        })
        .collect()
}

async fn collect_cpu_evidence(
    request: &GuestAttestationRequest,
    transcript_digest: [u8; 32],
) -> Result<RawEvidence, GuestAttestationError> {
    match request.cpu_profile() {
        CpuAttestationProfile::AmdSevSnp => {
            collect_tsm_evidence(request.cpu_profile(), "sev_guest", transcript_digest).await
        }
        CpuAttestationProfile::IntelTdx => {
            collect_tsm_evidence(request.cpu_profile(), "tdx_guest", transcript_digest).await
        }
        CpuAttestationProfile::AwsNitro => collect_nitro_evidence(request, transcript_digest).await,
    }
}

async fn collect_nitro_evidence(
    request: &GuestAttestationRequest,
    transcript_digest: [u8; 32],
) -> Result<RawEvidence, GuestAttestationError> {
    use aws_nitro_enclaves_nsm_api::{
        api::{Request, Response},
        driver::{nsm_exit, nsm_init, nsm_process_request},
    };

    let nonce = request.challenge().nonce().to_vec();
    let public_key = request.guest_public_key().to_vec();
    let document = tokio::task::spawn_blocking(move || {
        let descriptor = nsm_init();
        if descriptor < 0 {
            return Err(GuestAttestationError::NitroNsmUnavailable);
        }
        struct NsmDescriptor(i32);
        impl Drop for NsmDescriptor {
            fn drop(&mut self) {
                nsm_exit(self.0);
            }
        }
        let descriptor = NsmDescriptor(descriptor);
        let response = nsm_process_request(
            descriptor.0,
            Request::Attestation {
                user_data: Some(transcript_digest.to_vec().into()),
                nonce: Some(nonce.into()),
                public_key: Some(public_key.into()),
            },
        );
        match response {
            Response::Attestation { document } => Ok(document),
            response => Err(GuestAttestationError::NitroNsmResponse {
                response: format!("{response:?}"),
            }),
        }
    })
    .await??;

    RawEvidence::new(
        AttestedComponent::CpuVm,
        EvidenceProfile::AwsNitro,
        "application/vnd.amazon.nitro.attestation-cose",
        document,
    )
    .map_err(Into::into)
}

#[derive(Serialize)]
struct TsmEvidenceDocument {
    provider: String,
    generation: u64,
    report: String,
    auxiliary: Option<String>,
    manifest: Option<String>,
}

async fn collect_tsm_evidence(
    profile: CpuAttestationProfile,
    expected_provider: &str,
    transcript_digest: [u8; 32],
) -> Result<RawEvidence, GuestAttestationError> {
    ensure_tsm_report_root().await?;
    let root = Path::new(TSM_REPORT_ROOT);
    let sequence = REPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let instance = root.join(format!("nanocodex-{}-{sequence}", std::process::id()));
    fs::create_dir(&instance)
        .await
        .map_err(|source| tsm_io(&instance, source))?;
    let guard = ReportInstance(instance.clone());

    let mut report_data = [0_u8; 64];
    report_data[..transcript_digest.len()].copy_from_slice(&transcript_digest);
    write_file(&instance.join("inblob"), &report_data).await?;

    let provider = read_file(&instance.join("provider")).await?;
    let provider = String::from_utf8_lossy(&provider).trim().to_owned();
    if provider != expected_provider {
        return Err(GuestAttestationError::TsmProviderMismatch {
            expected: expected_provider.to_owned(),
            actual: provider,
        });
    }
    let report = read_file(&instance.join("outblob")).await?;
    let auxiliary = read_optional_file(&instance.join("auxblob")).await?;
    let manifest = read_optional_file(&instance.join("manifestblob")).await?;
    let generation_path = instance.join("generation");
    let generation_bytes = read_file(&generation_path).await?;
    let generation = String::from_utf8_lossy(&generation_bytes)
        .trim()
        .parse::<u64>()
        .map_err(|source| {
            tsm_io(
                &generation_path,
                io::Error::new(io::ErrorKind::InvalidData, source),
            )
        })?;
    if generation != 1 {
        return Err(GuestAttestationError::TsmGenerationMismatch { actual: generation });
    }

    let document = TsmEvidenceDocument {
        provider,
        generation,
        report: BASE64_STANDARD.encode(report),
        auxiliary: auxiliary.map(|bytes| BASE64_STANDARD.encode(bytes)),
        manifest: manifest.map(|bytes| BASE64_STANDARD.encode(bytes)),
    };
    let bytes = serde_json::to_vec(&document).map_err(|source| {
        tsm_io(
            &instance,
            io::Error::new(io::ErrorKind::InvalidData, source),
        )
    })?;
    drop(guard);
    RawEvidence::new(
        AttestedComponent::CpuVm,
        profile.evidence_profile(),
        "application/vnd.nanocodex.linux-tsm-report+json",
        bytes,
    )
    .map_err(Into::into)
}

async fn ensure_tsm_report_root() -> Result<(), GuestAttestationError> {
    if fs::metadata(TSM_REPORT_ROOT).await.is_ok() {
        return Ok(());
    }

    const CONFIGFS_ROOT: &str = "/sys/kernel/config";
    fs::create_dir_all(CONFIGFS_ROOT)
        .await
        .map_err(|source| tsm_io(Path::new(CONFIGFS_ROOT), source))?;
    match nix::mount::mount(
        Some("configfs"),
        CONFIGFS_ROOT,
        Some("configfs"),
        nix::mount::MsFlags::empty(),
        None::<&str>,
    ) {
        Ok(()) | Err(nix::errno::Errno::EBUSY) => {}
        Err(error) => {
            return Err(GuestAttestationError::NoSupportedCpuTee {
                reason: format!(
                    "{TSM_REPORT_ROOT} is absent and mounting configfs failed: {error}"
                ),
            });
        }
    }
    fs::metadata(TSM_REPORT_ROOT).await.map_err(|source| {
        GuestAttestationError::NoSupportedCpuTee {
            reason: format!("{TSM_REPORT_ROOT} is absent after mounting configfs: {source}"),
        }
    })?;
    Ok(())
}

struct ReportInstance(PathBuf);

impl Drop for ReportInstance {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

async fn write_file(path: &Path, bytes: &[u8]) -> Result<(), GuestAttestationError> {
    fs::write(path, bytes)
        .await
        .map_err(|source| tsm_io(path, source))
}

async fn read_file(path: &Path) -> Result<Vec<u8>, GuestAttestationError> {
    let file = fs::File::open(path)
        .await
        .map_err(|source| tsm_io(path, source))?;
    let mut bytes = Vec::new();
    file.take((MAX_RAW_EVIDENCE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| tsm_io(path, source))?;
    if bytes.len() > MAX_RAW_EVIDENCE_BYTES {
        return Err(tsm_io(
            path,
            io::Error::new(
                io::ErrorKind::InvalidData,
                "TSM value exceeds protocol bound",
            ),
        ));
    }
    Ok(bytes)
}

async fn read_optional_file(path: &Path) -> Result<Option<Vec<u8>>, GuestAttestationError> {
    match read_file(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(GuestAttestationError::TsmIo { source, .. })
            if source.kind() == io::ErrorKind::NotFound =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn tsm_io(path: &Path, source: io::Error) -> GuestAttestationError {
    GuestAttestationError::TsmIo {
        path: path.to_owned(),
        source,
    }
}

fn pci_io(path: impl Into<PathBuf>, source: io::Error) -> GuestAttestationError {
    GuestAttestationError::PciIo {
        path: path.into(),
        source,
    }
}

async fn run_bounded_command<const N: usize>(
    program: &'static str,
    arguments: [&str; N],
) -> Result<Vec<u8>, GuestAttestationError> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| GuestAttestationError::CommandIo { program, source })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| GuestAttestationError::CommandIo {
            program,
            source: io::Error::other("stdout pipe was not created"),
        })?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| GuestAttestationError::CommandIo {
            program,
            source: io::Error::other("stderr pipe was not created"),
        })?;

    let stdout_task = tokio::spawn(read_bounded(stdout, "stdout"));
    let stderr_task = tokio::spawn(read_bounded(stderr, "stderr"));
    let status = match timeout(NVATTEST_TIMEOUT, child.wait()).await {
        Ok(result) => {
            result.map_err(|source| GuestAttestationError::CommandIo { program, source })?
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(GuestAttestationError::CommandTimeout {
                program,
                seconds: NVATTEST_TIMEOUT.as_secs(),
            });
        }
    };
    let stdout = stdout_task.await??;
    let stderr = stderr_task.await??;
    if !status.success() {
        return Err(GuestAttestationError::CommandFailed {
            program,
            status: status.to_string(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        });
    }
    Ok(stdout)
}

async fn read_bounded<R>(reader: R, stream: &'static str) -> Result<Vec<u8>, GuestAttestationError>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    reader
        .take((MAX_COMMAND_OUTPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| GuestAttestationError::CommandIo {
            program: NVATTEST,
            source,
        })?;
    if bytes.len() > MAX_COMMAND_OUTPUT_BYTES {
        return Err(GuestAttestationError::CommandOutputTooLarge {
            program: NVATTEST,
            stream,
            maximum: MAX_COMMAND_OUTPUT_BYTES,
        });
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
    use crate::attestation::NvidiaAttestationProfile;

    #[test]
    fn nonce_hex_is_lowercase_and_fixed_width() {
        assert_eq!(encode_hex(&[0, 1, 0xab, 0xff]), "0001abff");
    }

    #[test]
    fn nvidia_profiles_require_exact_topologies() {
        assert_eq!(NvidiaAttestationProfile::B200Single.gpu_count(), 1);
        assert_eq!(NvidiaAttestationProfile::B200Single.switch_count(), 0);
        assert_eq!(
            NvidiaAttestationProfile::B200Hgx8EncryptedNvlink.gpu_count(),
            8
        );
        assert_eq!(
            NvidiaAttestationProfile::B200Hgx8EncryptedNvlink.switch_count(),
            2
        );
    }

    #[test]
    fn b200_auto_detection_is_exact_and_fail_closed() {
        assert_eq!(nvidia_profile_for_b200_count(0).unwrap(), None);
        assert_eq!(
            nvidia_profile_for_b200_count(1).unwrap(),
            Some(NvidiaAttestationProfile::B200Single)
        );
        assert_eq!(
            nvidia_profile_for_b200_count(8).unwrap(),
            Some(NvidiaAttestationProfile::B200Hgx8EncryptedNvlink)
        );
        assert!(matches!(
            nvidia_profile_for_b200_count(2),
            Err(GuestAttestationError::UnsupportedB200Topology { count: 2 })
        ));
    }
}
