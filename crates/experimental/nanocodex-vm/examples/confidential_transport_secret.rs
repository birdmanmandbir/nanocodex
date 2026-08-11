use std::{
    io,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nanocodex_vm::{
    host::{
        AttestationChallenge, AttestedCommand, CommandProofExpectation, CpuAttestationProfile,
        GuestAttestationParameters, NativeVerifierSet, NvidiaAttestationProfile,
        NvidiaNvattestVerifier, TdxVerificationPolicy, TdxVerifier, verify_attestation,
        verify_released_secret_proof,
    },
    tools::VmToolSession,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tokio::process::Command;
use zeroize::Zeroizing;

mod attestation_support;

use attestation_support::{parse_hex, read_bounded};

const MAX_COLLATERAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_SECRET_BYTES: usize = 64 * 1024;

struct Options {
    transport: String,
    transport_arguments: Vec<String>,
    local_guest: PathBuf,
    guest_program: String,
    secret: PathBuf,
    collateral: PathBuf,
    nvidia_policy: PathBuf,
    nvattest: PathBuf,
    mr_td: [u8; 48],
    rt_mrs: [[u8; 48]; 4],
    allow_dynamic_platform: bool,
    allow_cached_keys: bool,
    allow_smt: bool,
}

#[derive(Serialize)]
struct Output {
    schema_version: u32,
    status: &'static str,
    trust_boundary: &'static str,
    workload_manifest_sha256: String,
    executable_sha256: String,
    request_plaintext_sha256: String,
    response_plaintext_sha256: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::parse()?;
    let secret = Zeroizing::new(read_bounded(&options.secret, MAX_SECRET_BYTES)?);
    if secret.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "secret cannot be empty").into());
    }
    let guest_bytes = std::fs::read(&options.local_guest)?;
    let executable_digest: [u8; 32] = Sha256::digest(&guest_bytes).into();
    let workload_digest = executable_digest;

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
        "nanocodex-managed-confidential-secret-v1",
        now.checked_add(300)
            .ok_or_else(|| io::Error::other("attestation expiry overflow"))?,
    )?;
    let argv = vec![options.guest_program.clone(), "--proof-stdin".to_owned()];
    let command = AttestedCommand::new(&argv[0])?
        .arg(&argv[1])?
        .timeout_millis(30_000)?;
    let parameters = GuestAttestationParameters::new(
        challenge.clone(),
        workload_digest,
        CpuAttestationProfile::IntelTdx,
        Some(NvidiaAttestationProfile::H100Single),
    );
    let attestation = tokio::time::timeout(Duration::from_secs(180), session.attest(parameters))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "attestation timed out"))??;
    attestation.verify_key_proof()?;

    let mut policy = TdxVerificationPolicy::new(
        "nanocodex-managed-confidential-secret-v1",
        &read_bounded(&options.collateral, MAX_COLLATERAL_BYTES)?,
        options.mr_td,
        options.rt_mrs,
    )?;
    policy = policy
        .allow_dynamic_platform(options.allow_dynamic_platform)
        .allow_cached_keys(options.allow_cached_keys)
        .allow_smt(options.allow_smt);
    let nvidia = NvidiaNvattestVerifier::program(&options.nvattest, &options.nvidia_policy);
    let verifiers = NativeVerifierSet::new(TdxVerifier::new(policy), nvidia);
    let verified = verify_attestation(
        attestation.into_bundle(),
        &challenge,
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        &verifiers,
    )
    .await?;

    let confidential_command = verified.seal_confidential_command(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        command,
        executable_digest,
        &secret,
    )?;
    let proof = tokio::time::timeout(
        Duration::from_secs(60),
        session.prove_confidential_command(confidential_command),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "secret proof timed out"))??;
    let secret_sha256: [u8; 32] = Sha256::digest(&secret).into();
    let expectation =
        CommandProofExpectation::new(challenge, workload_digest, executable_digest, argv)
            .stdin_sha256(secret_sha256);
    let result = verify_released_secret_proof(&proof, &verified, &expectation)?;
    if result.stdout() != secret.as_slice() || !result.stderr().is_empty() {
        return Err(io::Error::other("authenticated secret round trip changed plaintext").into());
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            schema_version: 1,
            status: "appraisal_gated_confidential_secret_round_trip_verified",
            trust_boundary: "managed GCP TDX measures the provider boot chain, not the uploaded Nanocodex supervisor",
            workload_manifest_sha256: hex::encode(workload_digest),
            executable_sha256: hex::encode(executable_digest),
            request_plaintext_sha256: hex::encode(secret_sha256),
            response_plaintext_sha256: hex::encode(Sha256::digest(result.stdout())),
        })?
    );
    session.shutdown().await?;
    Ok(())
}

impl Options {
    fn parse() -> Result<Self, io::Error> {
        let mut transport = None;
        let mut transport_arguments = Vec::new();
        let mut local_guest = None;
        let mut guest_program = None;
        let mut secret = None;
        let mut collateral = None;
        let mut nvidia_policy = None;
        let mut nvattest = PathBuf::from("nvattest");
        let mut mr_td = None;
        let mut rt_mrs = [None; 4];
        let mut allow_dynamic_platform = false;
        let mut allow_cached_keys = false;
        let mut allow_smt = false;
        let mut arguments = std::env::args().skip(1);
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--transport" => transport = Some(value(&mut arguments, "--transport")?),
                "--transport-arg" => {
                    transport_arguments.push(value(&mut arguments, "--transport-arg")?)
                }
                "--local-guest" => local_guest = Some(value(&mut arguments, &argument)?.into()),
                "--guest-program" => guest_program = Some(value(&mut arguments, &argument)?),
                "--secret" => secret = Some(value(&mut arguments, &argument)?.into()),
                "--collateral" => collateral = Some(value(&mut arguments, &argument)?.into()),
                "--nvidia-policy" => nvidia_policy = Some(value(&mut arguments, &argument)?.into()),
                "--nvattest" => nvattest = value(&mut arguments, &argument)?.into(),
                "--mrtd" => mr_td = Some(parse_hex(&value(&mut arguments, &argument)?, "MRTD")?),
                value if value.starts_with("--rtmr") && value.len() == 7 => {
                    let index = value[6..].parse::<usize>().map_err(|_| invalid(value))?;
                    if index >= rt_mrs.len() {
                        return Err(invalid(value));
                    }
                    rt_mrs[index] = Some(parse_hex(
                        &self::value(&mut arguments, value)?,
                        &format!("RTMR{index}"),
                    )?);
                }
                "--allow-dynamic-platform" => allow_dynamic_platform = true,
                "--allow-cached-keys" => allow_cached_keys = true,
                "--allow-smt" => allow_smt = true,
                "--help" | "-h" => {
                    println!(
                        "usage: confidential_transport_secret --transport PROGRAM [--transport-arg ARG]... --local-guest PATH --guest-program PATH --secret PATH --collateral PATH --nvidia-policy REGO_PATH [--nvattest PATH] --mrtd 96_HEX --rtmr0 96_HEX --rtmr1 96_HEX --rtmr2 96_HEX --rtmr3 96_HEX [--allow-dynamic-platform] [--allow-cached-keys] [--allow-smt]"
                    );
                    std::process::exit(0);
                }
                other => return Err(invalid(other)),
            }
        }
        Ok(Self {
            transport: transport.ok_or_else(|| missing("--transport"))?,
            transport_arguments,
            local_guest: local_guest.ok_or_else(|| missing("--local-guest"))?,
            guest_program: guest_program.ok_or_else(|| missing("--guest-program"))?,
            secret: secret.ok_or_else(|| missing("--secret"))?,
            collateral: collateral.ok_or_else(|| missing("--collateral"))?,
            nvidia_policy: nvidia_policy.ok_or_else(|| missing("--nvidia-policy"))?,
            nvattest,
            mr_td: mr_td.ok_or_else(|| missing("--mrtd"))?,
            rt_mrs: [0, 1, 2, 3].map(|index| {
                rt_mrs[index].unwrap_or_else(|| {
                    eprintln!("missing --rtmr{index}");
                    std::process::exit(2);
                })
            }),
            allow_dynamic_platform,
            allow_cached_keys,
            allow_smt,
        })
    }
}

fn value(arguments: &mut impl Iterator<Item = String>, option: &str) -> Result<String, io::Error> {
    arguments.next().ok_or_else(|| missing(option))
}

fn missing(option: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("missing {option}"))
}

fn invalid(option: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("unknown or invalid argument {option:?}"),
    )
}
