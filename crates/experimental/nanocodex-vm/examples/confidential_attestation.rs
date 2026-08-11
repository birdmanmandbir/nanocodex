use std::{
    fs::File,
    io::{self, Read as _},
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
use sha2::{Digest as _, Sha256};
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
    guest: Option<PathBuf>,
    runtime_disk: Option<PathBuf>,
    image_manifest: Option<PathBuf>,
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
    let (runtime_path, manifest_digest, authenticated_root) =
        match (&options.guest, &options.runtime_disk) {
            (Some(guest), None) => {
                let runtime = GuestRuntimeDisk::prepare(guest, &options.cache)?;
                let digest = decode_digest(runtime.manifest_digest())?;
                let authenticated_root = runtime.manifest().authenticated_root()?;
                (runtime.path().to_path_buf(), digest, authenticated_root)
            }
            (None, Some(runtime_disk)) => {
                let manifest_path = options.image_manifest.as_ref().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--runtime-disk requires --image-manifest",
                    )
                })?;
                let manifest = load_image_manifest(manifest_path, runtime_disk)?;
                let digest = decode_digest(&manifest.digest()?)?;
                let authenticated_root = manifest.authenticated_root()?;
                (runtime_disk.clone(), digest, authenticated_root)
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "exactly one guest runtime source is required",
                )
                .into());
            }
        };
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

    let mut vm = VmConfig::authenticated_ext4(runtime_path, authenticated_root)
        .network(nanocodex_vm::host::Network::Disabled)
        .confidential(vm_profile);
    if matches!(options.cpu, CpuProfile::Snp) {
        vm = vm.snp_launch_commitment(manifest_digest);
    }
    if let Some(qgs) = options.qgs {
        vm = vm.tdx_quote_generation_socket(qgs);
    }
    if let Some(bundle) = options.device_bundle {
        vm = attach_device_bundle(vm, &bundle)?;
    }
    let mut vmm = Command::new(options.vmm);
    vmm.args(["vm-run-config", "--config"]);
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
    let mut parameters =
        GuestAttestationParameters::new(challenge, manifest_digest, cpu_profile, nvidia_profile);
    if matches!(options.cpu, CpuProfile::Tdx) {
        parameters = parameters.measure_workload_in_tdx_rtmr3();
    }
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
        let mut runtime_disk = None;
        let mut image_manifest = None;
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
                "--runtime-disk" => {
                    runtime_disk = Some(value(&mut arguments, "--runtime-disk")?.into());
                }
                "--image-manifest" => {
                    image_manifest = Some(value(&mut arguments, "--image-manifest")?.into());
                }
                "--cache" => cache = value(&mut arguments, "--cache")?.into(),
                "--qgs" => qgs = Some(value(&mut arguments, "--qgs")?.into()),
                "--device-bundle" => {
                    device_bundle = Some(value(&mut arguments, "--device-bundle")?.into());
                }
                "--help" | "-h" => {
                    println!(
                        "usage: confidential_attestation --profile snp|tdx --vmm PATH (--guest PATH | --runtime-disk PATH --image-manifest PATH) [--cache PATH] [--qgs PATH] [--nvidia off|b200-single|b200-hgx8] [--device-bundle PATH]"
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
        match (&guest, &runtime_disk) {
            (Some(_), None) | (None, Some(_)) => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "exactly one of --guest and --runtime-disk is required",
                ));
            }
        }
        if runtime_disk.is_some() != image_manifest.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--runtime-disk and --image-manifest must be supplied together",
            ));
        }
        if !matches!(nvidia, NvidiaProfile::Off) && runtime_disk.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "B200 attestation requires --runtime-disk with /nanocodex-vm-guest, the matching NVIDIA guest driver, and nvattest",
            ));
        }
        Ok(Self {
            cpu,
            nvidia,
            vmm: vmm.ok_or_else(|| missing("--vmm"))?,
            guest,
            runtime_disk,
            image_manifest,
            cache,
            qgs,
            device_bundle,
        })
    }
}

fn sha256_file(path: &std::path::Path) -> Result<[u8; 32], io::Error> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("runtime disk {} is not a regular file", path.display()),
        ));
    }

    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn load_image_manifest(
    manifest_path: &std::path::Path,
    runtime_disk: &std::path::Path,
) -> Result<nanocodex_vm::tools::GuestImageManifestV1, Box<dyn std::error::Error>> {
    const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

    let mut bytes = Vec::new();
    File::open(manifest_path)?
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "guest image manifest exceeds 64 KiB",
        )
        .into());
    }
    let manifest = nanocodex_vm::tools::GuestImageManifestV1::from_json(&bytes)?;
    let metadata = std::fs::metadata(runtime_disk)?;
    if manifest.root_image().bytes() != metadata.len()
        || manifest.root_image().sha256() != hex::encode(sha256_file(runtime_disk)?)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime disk does not match the strict guest image manifest",
        )
        .into());
    }
    Ok(manifest)
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
            format!("manifest digest was {} bytes; expected 32", bytes.len()),
        )
    })
}
