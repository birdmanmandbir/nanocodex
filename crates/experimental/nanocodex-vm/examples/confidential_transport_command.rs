use std::{
    io,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nanocodex_vm::{
    host::{
        AttestationChallenge, AttestedCommand, AttestedCommandProof, AttestedCommandRequest,
        CommandProofExpectation, CpuAttestationProfile, GuestAttestationParameters,
        NvidiaAttestationProfile, verify_collected_command_proof,
    },
    tools::VmToolSession,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tokio::process::Command;

#[derive(Clone, Copy)]
enum CpuProfile {
    Snp,
    Tdx,
}

struct Options {
    cpu: CpuProfile,
    nvidia: Option<NvidiaAttestationProfile>,
    transport: String,
    transport_arguments: Vec<String>,
    local_guest: PathBuf,
    guest_program: String,
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
    let guest_bytes = std::fs::read(&options.local_guest)?;
    let executable_digest: [u8; 32] = Sha256::digest(&guest_bytes).into();
    let workload_digest = executable_digest;
    let cpu_profile = match options.cpu {
        CpuProfile::Snp => CpuAttestationProfile::AmdSevSnp,
        CpuProfile::Tdx => CpuAttestationProfile::IntelTdx,
    };

    let mut transport = Command::new(&options.transport);
    transport.args(&options.transport_arguments);
    let session = VmToolSession::spawn(&mut transport)?;
    tokio::time::timeout(Duration::from_secs(30), session.ready())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "guest readiness timed out"))??;

    let mut nonce = [0_u8; 32];
    getrandom::fill(&mut nonce)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let challenge = AttestationChallenge::new(
        nonce,
        "nanocodex-managed-command-proof-v1",
        now.checked_add(300)
            .ok_or_else(|| io::Error::other("attestation expiry overflow"))?,
    )?;
    let argv = vec![
        options.guest_program.clone(),
        "--proof-message".to_owned(),
        options.message.clone(),
    ];
    let command = AttestedCommand::new(&argv[0])?
        .arg(&argv[1])?
        .arg(&argv[2])?;
    let parameters = GuestAttestationParameters::new(
        challenge.clone(),
        workload_digest,
        cpu_profile,
        options.nvidia,
    );
    let request = AttestedCommandRequest::new(parameters, command);
    let proof = tokio::time::timeout(Duration::from_secs(180), session.prove_command(request))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "command proof timed out"))??;
    let expected =
        CommandProofExpectation::new(challenge, workload_digest, executable_digest, argv);
    let collected = verify_collected_command_proof(&proof, &expected)?;
    if collected.stdout() != format!("{}\n", options.message).as_bytes() {
        return Err(io::Error::other("authenticated command output was unexpected").into());
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            schema_version: 1,
            status: "managed_confidential_command_proof_collected",
            warning: "trust this receipt only after appraising the exact embedded native evidence and a measured workload policy",
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
        let mut nvidia = None;
        let mut transport = None;
        let mut transport_arguments = Vec::new();
        let mut local_guest = None;
        let mut guest_program = None;
        let mut message = "attested-managed-tool-call".to_owned();
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
                        "off" => None,
                        "h100-single" => Some(NvidiaAttestationProfile::H100Single),
                        "b200-single" => Some(NvidiaAttestationProfile::B200Single),
                        "b200-hgx8" => Some(NvidiaAttestationProfile::B200Hgx8EncryptedNvlink),
                        other => {
                            return Err(invalid(
                                "--nvidia",
                                other,
                                "off, h100-single, b200-single, or b200-hgx8",
                            ));
                        }
                    };
                }
                "--transport" => transport = Some(value(&mut arguments, "--transport")?),
                "--transport-arg" => {
                    transport_arguments.push(value(&mut arguments, "--transport-arg")?)
                }
                "--local-guest" => {
                    local_guest = Some(PathBuf::from(value(&mut arguments, "--local-guest")?))
                }
                "--guest-program" => {
                    guest_program = Some(value(&mut arguments, "--guest-program")?)
                }
                "--message" => message = value(&mut arguments, "--message")?,
                "--help" | "-h" => {
                    println!(
                        "usage: confidential_transport_command --profile snp|tdx [--nvidia off|h100-single|b200-single|b200-hgx8] --transport PROGRAM [--transport-arg ARG]... --local-guest PATH --guest-program PATH [--message TEXT]"
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
        Ok(Self {
            cpu: cpu.ok_or_else(|| missing("--profile"))?,
            nvidia,
            transport: transport.ok_or_else(|| missing("--transport"))?,
            transport_arguments,
            local_guest: local_guest.ok_or_else(|| missing("--local-guest"))?,
            guest_program: guest_program.ok_or_else(|| missing("--guest-program"))?,
            message,
        })
    }
}

fn value(arguments: &mut impl Iterator<Item = String>, option: &str) -> Result<String, io::Error> {
    arguments
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("missing {option}")))
}

fn missing(option: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("missing {option}"))
}

fn invalid(option: &str, value: &str, expected: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("invalid {option} value {value:?}; expected {expected}"),
    )
}
