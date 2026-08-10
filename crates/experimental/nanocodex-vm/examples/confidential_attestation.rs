use std::{
    io,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nanocodex_vm::{
    host::{
        AttestationChallenge, ConfidentialVmProfile, CpuAttestationProfile, EgressLease,
        GuestAttestationParameters, GuestCommand, NvidiaAttestationProfile, VmConfig,
    },
    tools::{GuestRuntimeDisk, VmToolSession},
};
use serde::Serialize;
use tokio::process::Command;

#[derive(Clone, Copy)]
enum CpuProfile {
    Snp,
    Tdx,
}

#[derive(Clone, Copy)]
enum NvidiaProfile {
    Off,
    Single,
    Hgx8,
}

struct Options {
    cpu: CpuProfile,
    nvidia: NvidiaProfile,
    vmm: PathBuf,
    guest: PathBuf,
    cache: PathBuf,
    qgs: Option<PathBuf>,
    device_bundle: Option<PathBuf>,
}

#[derive(Serialize)]
struct Output {
    schema_version: u32,
    status: &'static str,
    key_proof_verified: bool,
    warning: &'static str,
    attestation: nanocodex_vm::host::GuestAttestation,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::parse()?;
    let runtime = GuestRuntimeDisk::prepare(&options.guest, &options.cache)?;
    let manifest_digest = decode_digest(runtime.digest())?;
    let (vm_profile, cpu_profile) = match options.cpu {
        CpuProfile::Snp => (
            ConfidentialVmProfile::amd_sev_snp(),
            CpuAttestationProfile::AmdSevSnp,
        ),
        CpuProfile::Tdx => (
            ConfidentialVmProfile::intel_tdx(),
            CpuAttestationProfile::IntelTdx,
        ),
    };
    let (vm_profile, nvidia_profile) = match options.nvidia {
        NvidiaProfile::Off => (vm_profile, None),
        NvidiaProfile::Single => (
            vm_profile.nvidia_b200_single(),
            Some(NvidiaAttestationProfile::B200Single),
        ),
        NvidiaProfile::Hgx8 => (
            vm_profile.nvidia_b200_hgx_8_encrypted_nvlink(),
            Some(NvidiaAttestationProfile::B200Hgx8EncryptedNvlink),
        ),
    };

    let mut vm = VmConfig::ext4(runtime.path())
        .network(nanocodex_vm::host::Network::Disabled)
        .confidential(vm_profile);
    if let Some(qgs) = options.qgs {
        vm = vm.tdx_quote_generation_socket(qgs);
    }
    if let Some(bundle) = options.device_bundle {
        vm = attach_device_bundle(vm, &bundle)?;
    }
    let mut vmm = Command::new(options.vmm);
    vmm.arg("vm-run-config");
    let session = VmToolSession::spawn_configured(
        vmm,
        vm,
        GuestCommand::new("/nanocodex-vm-guest").arg("/"),
        EgressLease::disabled(),
    )
    .await?;

    let mut nonce = [0_u8; 32];
    getrandom::fill(&mut nonce)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let challenge = AttestationChallenge::new(
        nonce,
        "nanocodex-libkrun-example-v1",
        now.checked_add(300)
            .ok_or_else(|| io::Error::other("attestation expiry overflow"))?,
    )?;
    let parameters =
        GuestAttestationParameters::new(challenge, manifest_digest, cpu_profile, nvidia_profile);
    let collection =
        tokio::time::timeout(Duration::from_secs(180), session.attest(parameters)).await;
    let result = match collection {
        Ok(result) => result,
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "guest attestation exceeded 180 seconds",
            )
            .into());
        }
    };
    let attestation = match result {
        Ok(attestation) => attestation,
        Err(error) => return Err(error.into()),
    };
    attestation.verify_key_proof()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            schema_version: 1,
            status: "native_evidence_collected",
            key_proof_verified: true,
            warning: "native evidence is not trusted until a relying party verifies vendor signatures and its measurement policy",
            attestation,
        })?
    );
    session.shutdown().await?;
    Ok(())
}

impl Options {
    fn parse() -> Result<Self, io::Error> {
        let mut cpu = None;
        let mut nvidia = NvidiaProfile::Off;
        let mut vmm = None;
        let mut guest = None;
        let mut cache = PathBuf::from(".cache/nanocodex/attestation-example");
        let mut qgs = None;
        let mut device_bundle = None;
        let mut arguments = std::env::args().skip(1);
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--profile" => {
                    cpu = Some(match value(&mut arguments, "--profile")?.as_str() {
                        "snp" => CpuProfile::Snp,
                        "tdx" => CpuProfile::Tdx,
                        other => return Err(invalid("--profile", other, "snp or tdx")),
                    });
                }
                "--nvidia" => {
                    nvidia = match value(&mut arguments, "--nvidia")?.as_str() {
                        "off" => NvidiaProfile::Off,
                        "b200-single" => NvidiaProfile::Single,
                        "b200-hgx8" => NvidiaProfile::Hgx8,
                        other => {
                            return Err(invalid(
                                "--nvidia",
                                other,
                                "off, b200-single, or b200-hgx8",
                            ));
                        }
                    };
                }
                "--vmm" => vmm = Some(value(&mut arguments, "--vmm")?.into()),
                "--guest" => guest = Some(value(&mut arguments, "--guest")?.into()),
                "--cache" => cache = value(&mut arguments, "--cache")?.into(),
                "--qgs" => qgs = Some(value(&mut arguments, "--qgs")?.into()),
                "--device-bundle" => {
                    device_bundle = Some(value(&mut arguments, "--device-bundle")?.into());
                }
                "--help" | "-h" => {
                    println!(
                        "usage: confidential_attestation --profile snp|tdx --vmm PATH --guest PATH [--cache PATH] [--qgs PATH] [--nvidia off|b200-single|b200-hgx8] [--device-bundle PATH]"
                    );
                    std::process::exit(0);
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown argument {other:?}"),
                    ));
                }
            }
        }
        let cpu = cpu.ok_or_else(|| missing("--profile"))?;
        if qgs.is_some() && !matches!(cpu, CpuProfile::Tdx) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--qgs is valid only with --profile tdx",
            ));
        }
        if matches!(nvidia, NvidiaProfile::Off) != device_bundle.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--nvidia and --device-bundle must be supplied together",
            ));
        }
        Ok(Self {
            cpu,
            nvidia,
            vmm: vmm.ok_or_else(|| missing("--vmm"))?,
            guest: guest.ok_or_else(|| missing("--guest"))?,
            cache,
            qgs,
            device_bundle,
        })
    }
}

#[cfg(target_os = "linux")]
fn attach_device_bundle(
    vm: nanocodex_vm::host::VmConfig,
    path: &std::path::Path,
) -> Result<nanocodex_vm::host::VmConfig, Box<dyn std::error::Error>> {
    let file = std::fs::File::open(path)?;
    let bundle = serde_json::from_reader(file)?;
    Ok(vm.confidential_devices(bundle))
}

#[cfg(not(target_os = "linux"))]
fn attach_device_bundle(
    _vm: nanocodex_vm::host::VmConfig,
    _path: &std::path::Path,
) -> Result<nanocodex_vm::host::VmConfig, Box<dyn std::error::Error>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "confidential PCI assignment requires Linux",
    )
    .into())
}

fn value(arguments: &mut impl Iterator<Item = String>, option: &str) -> Result<String, io::Error> {
    arguments.next().ok_or_else(|| missing(option))
}

fn missing(option: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("missing required {option} value"),
    )
}

fn invalid(option: &str, actual: &str, expected: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("invalid {option} value {actual:?}; expected {expected}"),
    )
}

fn decode_digest(encoded: &str) -> Result<[u8; 32], io::Error> {
    let bytes = hex::decode(encoded).map_err(io::Error::other)?;
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("runtime digest was {} bytes; expected 32", bytes.len()),
        )
    })
}
