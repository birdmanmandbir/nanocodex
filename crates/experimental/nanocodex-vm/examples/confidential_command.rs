use std::{
    io,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nanocodex_vm::{
    host::{
        AttestationChallenge, AttestedCommand, AttestedCommandProof, AttestedCommandRequest,
        CommandProofExpectation, ConfidentialVmProfile, CpuAttestationProfile, EgressLease,
        GuestAttestationParameters, GuestCommand, Network, VmConfig,
        verify_collected_command_proof,
    },
    tools::{GuestRuntimeDisk, VmToolSession},
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tokio::process::Command;

#[derive(Clone, Copy)]
enum CpuProfile {
    #[cfg(feature = "development-attestation")]
    Development,
    Snp,
    Tdx,
}

struct Options {
    cpu: CpuProfile,
    vmm: PathBuf,
    guest: PathBuf,
    cache: PathBuf,
    qgs: Option<PathBuf>,
    message: String,
}

#[derive(Serialize)]
struct Output {
    schema_version: u32,
    status: &'static str,
    warning: &'static str,
    expected_executable_sha256: String,
    proof: AttestedCommandProof,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::parse()?;
    let executable_digest: [u8; 32] = Sha256::digest(std::fs::read(&options.guest)?).into();
    let runtime = GuestRuntimeDisk::prepare(&options.guest, &options.cache)?;
    let runtime_digest = decode_digest(runtime.digest())?;
    let (vm_profile, cpu_profile) = match options.cpu {
        #[cfg(feature = "development-attestation")]
        CpuProfile::Development => (None, CpuAttestationProfile::Development),
        CpuProfile::Snp => (
            Some(ConfidentialVmProfile::amd_sev_snp()),
            CpuAttestationProfile::AmdSevSnp,
        ),
        CpuProfile::Tdx => (
            Some(ConfidentialVmProfile::intel_tdx()),
            CpuAttestationProfile::IntelTdx,
        ),
    };

    let mut vm = VmConfig::ext4(runtime.path()).network(Network::Disabled);
    if let Some(vm_profile) = vm_profile {
        vm = vm.confidential(vm_profile);
    }
    if matches!(options.cpu, CpuProfile::Snp) {
        vm = vm.snp_launch_commitment(runtime_digest);
    }
    if let Some(qgs) = options.qgs {
        vm = vm.tdx_quote_generation_socket(qgs);
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
        "nanocodex-command-proof-example-v1",
        now.checked_add(300)
            .ok_or_else(|| io::Error::other("attestation expiry overflow"))?,
    )?;
    let argv = vec![
        "/nanocodex-vm-guest".to_owned(),
        "--proof-message".to_owned(),
        options.message.clone(),
    ];
    let command = AttestedCommand::new(&argv[0])?
        .arg(&argv[1])?
        .arg(&argv[2])?;
    let mut parameters =
        GuestAttestationParameters::new(challenge.clone(), runtime_digest, cpu_profile, None);
    if matches!(options.cpu, CpuProfile::Tdx) {
        parameters = parameters.measure_workload_in_tdx_rtmr3();
    }
    let request = AttestedCommandRequest::new(parameters, command);
    let proof = tokio::time::timeout(Duration::from_secs(180), session.prove_command(request))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "command proof exceeded 180s"))??;
    let expected = CommandProofExpectation::new(challenge, runtime_digest, executable_digest, argv);
    let collected = verify_collected_command_proof(&proof, &expected)?;
    if collected.stdout() != format!("{}\n", options.message).as_bytes() {
        return Err(io::Error::other("authenticated command output was unexpected").into());
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            schema_version: 1,
            status: "command_proof_collected",
            warning: "the receipt is internally consistent but is trusted only after vendor-native evidence and measurement policy verification",
            expected_executable_sha256: hex::encode(executable_digest),
            proof,
        })?
    );
    session.shutdown().await?;
    Ok(())
}

impl Options {
    fn parse() -> Result<Self, io::Error> {
        let mut cpu = None;
        let mut vmm = None;
        let mut guest = None;
        let mut cache = PathBuf::from(".cache/nanocodex/command-proof-example");
        let mut qgs = None;
        let mut message = "confidential-vm-command-proof".to_owned();
        let mut arguments = std::env::args().skip(1);
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--profile" => {
                    cpu = Some(match value(&mut arguments, "--profile")?.as_str() {
                        #[cfg(feature = "development-attestation")]
                        "development" => CpuProfile::Development,
                        "snp" => CpuProfile::Snp,
                        "tdx" => CpuProfile::Tdx,
                        other => {
                            return Err(invalid(
                                "--profile",
                                other,
                                if cfg!(feature = "development-attestation") {
                                    "development, snp, or tdx"
                                } else {
                                    "snp or tdx"
                                },
                            ));
                        }
                    });
                }
                "--vmm" => vmm = Some(value(&mut arguments, "--vmm")?.into()),
                "--guest" => guest = Some(value(&mut arguments, "--guest")?.into()),
                "--cache" => cache = value(&mut arguments, "--cache")?.into(),
                "--qgs" => qgs = Some(value(&mut arguments, "--qgs")?.into()),
                "--message" => message = value(&mut arguments, "--message")?,
                "--help" | "-h" => {
                    println!(
                        "usage: confidential_command --profile snp|tdx{} --vmm PATH --guest PATH [--cache PATH] [--qgs PATH] [--message TEXT]",
                        if cfg!(feature = "development-attestation") {
                            "|development"
                        } else {
                            ""
                        }
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
        Ok(Self {
            cpu,
            vmm: vmm.ok_or_else(|| missing("--vmm"))?,
            guest: guest.ok_or_else(|| missing("--guest"))?,
            cache,
            qgs,
            message,
        })
    }
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
