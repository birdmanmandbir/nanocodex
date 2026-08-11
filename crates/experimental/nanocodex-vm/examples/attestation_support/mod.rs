#![allow(dead_code)]

use std::{
    fs,
    io::{self, Read as _},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use nanocodex_vm::host::{
    AttestedCommandProof, GuestAttestation, ManifestSha256, MeasuredGuestCpuV1,
    MeasuredGuestManifestV1, MeasuredGuestReferenceV1, VerifiedAttestation, VerifiedCommandProof,
    WorkloadComponentKindV1,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_ATTESTATION_BYTES: usize = 16 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const INFERENCE_CONFIG_DOMAIN: &[u8] = b"nanocodex-vm-vllm-inference-config-v1\0";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceManifest {
    pub schema_version: u32,
    pub policy_id: String,
    pub measured_guest: MeasuredGuestManifestV1,
    pub guest_program: String,
    pub container: String,
    pub server_image_id: String,
    pub server_image_reference: String,
    pub model: String,
    pub model_revision: String,
    pub model_snapshot_sha256: String,
}

impl InferenceManifest {
    pub fn argv(&self) -> Vec<String> {
        vec![
            self.guest_program.clone(),
            "--proof-vllm-inference".to_owned(),
            self.container.clone(),
            self.server_image_id.clone(),
            self.server_image_reference.clone(),
            self.model.clone(),
            self.model_revision.clone(),
        ]
    }

    pub const fn guest_executable_sha256(&self) -> [u8; 32] {
        *self
            .measured_guest
            .artifacts()
            .supervisor()
            .sha256()
            .as_bytes()
    }

    pub fn tdx_reference(&self) -> Result<([u8; 48], [[u8; 48]; 4]), io::Error> {
        let MeasuredGuestReferenceV1::IntelTdx {
            mrtd,
            rtmr0,
            rtmr1,
            rtmr2,
            rtmr3_baseline,
        } = self.measured_guest.reference()
        else {
            return Err(invalid_argument(
                "inference manifest does not contain TDX reference values",
            ));
        };
        Ok((
            *mrtd.as_bytes(),
            [
                *rtmr0.as_bytes(),
                *rtmr1.as_bytes(),
                *rtmr2.as_bytes(),
                *rtmr3_baseline.as_bytes(),
            ],
        ))
    }
}

#[derive(Serialize)]
struct InferenceConfiguration<'a> {
    policy_id: &'a str,
    guest_program: &'a str,
    container: &'a str,
    server_image_id: &'a str,
    server_image_reference: &'a str,
    model: &'a str,
    model_revision: &'a str,
}

#[derive(Serialize)]
struct Output<'a> {
    schema_version: u32,
    status: &'static str,
    policy_id: &'a str,
    hardware_identity: &'a str,
    components: Vec<ComponentOutput<'a>>,
    guest_public_key_hex: String,
    workload_manifest_sha256: String,
}

#[derive(Serialize)]
struct ComponentOutput<'a> {
    component: String,
    profile: String,
    hardware_identity: &'a str,
    evidence_sha256: String,
    trusted_boot: bool,
    debug_disabled: bool,
    nvidia_fabric: Option<String>,
}

#[derive(Serialize)]
struct CommandOutput<'a> {
    schema_version: u32,
    status: &'static str,
    policy_id: &'a str,
    components: Vec<ComponentOutput<'a>>,
    workload_manifest_sha256: String,
    executable_sha256: String,
    argv: &'a [String],
    termination: String,
    stdout: Value,
    stderr_utf8: String,
}

pub fn load_attestation(path: &Path) -> Result<GuestAttestation, Box<dyn std::error::Error>> {
    let bytes = read_bounded(path, MAX_ATTESTATION_BYTES)?;
    let mut value: Value = serde_json::from_slice(&bytes)?;
    if let Some(attestation) = value.get_mut("attestation") {
        value = attestation.take();
    } else if let Some(attestation) = value.pointer_mut("/proof/attestation") {
        value = attestation.take();
    }
    Ok(serde_json::from_value(value)?)
}

pub fn load_command_proof(path: &Path) -> Result<AttestedCommandProof, Box<dyn std::error::Error>> {
    let bytes = read_bounded(path, MAX_ATTESTATION_BYTES)?;
    let mut value: Value = serde_json::from_slice(&bytes)?;
    if let Some(proof) = value.get_mut("proof") {
        value = proof.take();
    }
    Ok(serde_json::from_value(value)?)
}

pub fn load_inference_manifest(
    path: &Path,
) -> Result<(InferenceManifest, [u8; 32]), Box<dyn std::error::Error>> {
    let bytes = read_bounded(path, MAX_MANIFEST_BYTES)?;
    let manifest: InferenceManifest = serde_json::from_slice(&bytes)?;
    if manifest.schema_version != 1 {
        return Err(invalid_argument("unsupported inference manifest version").into());
    }
    if manifest.measured_guest.cpu() != MeasuredGuestCpuV1::IntelTdx
        || manifest.measured_guest.workload().argv() != manifest.argv()
    {
        return Err(invalid_argument(
            "inference policy requires a TDX measured guest with the exact vLLM argv",
        )
        .into());
    }
    let image_sha256 = manifest
        .server_image_reference
        .rsplit_once("@sha256:")
        .ok_or_else(|| invalid_argument("server_image_reference must pin an OCI digest"))?
        .1;
    let image_sha256 = ManifestSha256::from_hex(image_sha256)?;
    let model_sha256 = ManifestSha256::from_hex(&manifest.model_snapshot_sha256)?;
    let config = serde_json::to_vec(&InferenceConfiguration {
        policy_id: &manifest.policy_id,
        guest_program: &manifest.guest_program,
        container: &manifest.container,
        server_image_id: &manifest.server_image_id,
        server_image_reference: &manifest.server_image_reference,
        model: &manifest.model,
        model_revision: &manifest.model_revision,
    })?;
    let mut config_preimage = INFERENCE_CONFIG_DOMAIN.to_vec();
    config_preimage.extend_from_slice(&config);
    let config_sha256 = ManifestSha256::digest(config_preimage);
    for (kind, name, expected) in [
        (
            WorkloadComponentKindV1::ContainerImage,
            "vllm-image",
            image_sha256,
        ),
        (
            WorkloadComponentKindV1::ModelWeights,
            "model-snapshot",
            model_sha256,
        ),
        (
            WorkloadComponentKindV1::Configuration,
            "vllm-inference-config",
            config_sha256,
        ),
    ] {
        if !manifest
            .measured_guest
            .workload()
            .components()
            .iter()
            .any(|component| {
                component.kind() == kind
                    && component.name() == name
                    && component.sha256() == expected
            })
        {
            return Err(invalid_argument(format!(
                "measured guest is missing exact {name} component {expected:?}"
            ))
            .into());
        }
    }
    let digest = *manifest.measured_guest.digest()?.as_bytes();
    Ok((manifest, digest))
}

pub fn read_bounded(path: &Path, maximum: usize) -> Result<Vec<u8>, io::Error> {
    let mut bytes = Vec::new();
    if path.as_os_str() == "-" {
        io::stdin()
            .take((maximum + 1) as u64)
            .read_to_end(&mut bytes)?;
    } else {
        fs::File::open(path)?
            .take((maximum + 1) as u64)
            .read_to_end(&mut bytes)?;
    }
    if bytes.len() > maximum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("input exceeds {maximum} bytes"),
        ));
    }
    Ok(bytes)
}

pub fn parse_hex<const N: usize>(encoded: &str, label: &str) -> Result<[u8; N], io::Error> {
    let bytes = hex::decode(encoded).map_err(|source| invalid_argument(source.to_string()))?;
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        invalid_argument(format!(
            "{label} decoded to {} bytes; expected {N}",
            bytes.len()
        ))
    })
}

pub fn now_unix_seconds() -> Result<u64, std::time::SystemTimeError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

pub fn print_verified(
    verified: &VerifiedAttestation,
    policy_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let cpu = verified
        .claims()
        .last()
        .ok_or_else(|| io::Error::other("verified response had no CPU claim"))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            schema_version: 1,
            status: "native_evidence_verified",
            policy_id,
            hardware_identity: cpu.hardware_identity(),
            components: component_outputs(verified),
            guest_public_key_hex: hex::encode(verified.guest_public_key()),
            workload_manifest_sha256: hex::encode(verified.workload_manifest_digest()),
        })?
    );
    Ok(())
}

pub fn print_verified_command(
    verified: &VerifiedAttestation,
    command: &VerifiedCommandProof,
    policy_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let stdout = serde_json::from_slice(command.stdout())
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(command.stdout()).into_owned()));
    println!(
        "{}",
        serde_json::to_string_pretty(&CommandOutput {
            schema_version: 1,
            status: "native_evidence_and_command_verified",
            policy_id,
            components: component_outputs(verified),
            workload_manifest_sha256: hex::encode(verified.workload_manifest_digest()),
            executable_sha256: hex::encode(command.record().executable_sha256()),
            argv: command.record().argv(),
            termination: format!("{:?}", command.record().termination()),
            stdout,
            stderr_utf8: String::from_utf8_lossy(command.stderr()).into_owned(),
        })?
    );
    Ok(())
}

fn component_outputs(verified: &VerifiedAttestation) -> Vec<ComponentOutput<'_>> {
    verified
        .claims()
        .iter()
        .map(|claim| ComponentOutput {
            component: format!("{:?}", claim.component()),
            profile: format!("{:?}", claim.profile()),
            hardware_identity: claim.hardware_identity(),
            evidence_sha256: hex::encode(claim.evidence_digest()),
            trusted_boot: claim.trusted_boot(),
            debug_disabled: claim.debug_disabled(),
            nvidia_fabric: claim.nvidia_fabric().map(|fabric| format!("{fabric:?}")),
        })
        .collect()
}

pub fn value(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<String, io::Error> {
    arguments
        .next()
        .ok_or_else(|| invalid_argument(format!("missing value for {option}")))
}

pub fn invalid_argument(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
