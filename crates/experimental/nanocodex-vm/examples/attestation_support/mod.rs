use std::{
    fs,
    io::{self, Read as _},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use nanocodex_vm::host::{GuestAttestation, VerifiedAttestation};
use serde::Serialize;
use serde_json::Value;

const MAX_ATTESTATION_BYTES: usize = 16 * 1024 * 1024;

#[derive(Serialize)]
struct Output<'a> {
    schema_version: u32,
    status: &'static str,
    policy_id: &'a str,
    hardware_identity: &'a str,
    guest_public_key_hex: String,
    workload_manifest_sha256: String,
}

pub fn load_attestation(path: &Path) -> Result<GuestAttestation, Box<dyn std::error::Error>> {
    let bytes = read_bounded(path, MAX_ATTESTATION_BYTES)?;
    let mut value: Value = serde_json::from_slice(&bytes)?;
    if let Some(attestation) = value.get_mut("attestation") {
        value = attestation.take();
    }
    Ok(serde_json::from_value(value)?)
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
            guest_public_key_hex: hex::encode(verified.guest_public_key()),
            workload_manifest_sha256: hex::encode(verified.workload_manifest_digest()),
        })?
    );
    Ok(())
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
