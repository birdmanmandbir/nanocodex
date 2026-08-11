use std::{io, path::PathBuf};

use nanocodex_vm::host::{
    SnpRevocationPolicy, SnpTcbVersion, SnpVerificationPolicy, SnpVerifier, verify_attestation,
};

mod attestation_support;

use attestation_support::{load_attestation, now_unix_seconds, parse_hex, print_verified};

struct Options {
    input: PathBuf,
    crl: Option<PathBuf>,
    measurement: [u8; 48],
    minimum_tcb: SnpTcbVersion,
    minimum_guest_svn: u32,
    allow_smt: bool,
    require_single_socket: bool,
    revocation: SnpRevocationPolicy,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::parse()?;
    let attestation = load_attestation(&options.input)?;
    attestation.verify_key_proof()?;
    let challenge = attestation.bundle().request().challenge().clone();
    let mut policy = SnpVerificationPolicy::new(
        challenge.policy_id(),
        options.measurement,
        options.minimum_tcb,
        options.revocation,
    )?
    .with_minimum_guest_svn(options.minimum_guest_svn)
    .with_smt_allowed(options.allow_smt)
    .with_single_socket_required(options.require_single_socket);
    if let Some(crl) = options.crl {
        policy = policy.with_crl_der(attestation_support::read_bounded(&crl, 4 * 1024 * 1024)?)?;
    }
    let verifier = SnpVerifier::new(policy);
    let now = now_unix_seconds()?;
    let verified =
        verify_attestation(attestation.into_bundle(), &challenge, now, &verifier).await?;
    print_verified(&verified, challenge.policy_id())
}

impl Options {
    fn parse() -> Result<Self, io::Error> {
        let mut input = None;
        let mut crl = None;
        let mut measurement = None;
        let mut minimum_tcb = SnpTcbVersion::default();
        let mut minimum_guest_svn = 0;
        let mut allow_smt = false;
        let mut require_single_socket = false;
        let mut revocation = SnpRevocationPolicy::RequireFreshCrl;
        let mut arguments = std::env::args().skip(1);
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--input" => input = Some(value(&mut arguments, "--input")?.into()),
                "--crl" => crl = Some(value(&mut arguments, "--crl")?.into()),
                "--measurement" => {
                    measurement = Some(parse_hex(
                        &value(&mut arguments, "--measurement")?,
                        "measurement",
                    )?)
                }
                "--minimum-fmc" => {
                    minimum_tcb.fmc = Some(parse_u8(&mut arguments, "--minimum-fmc")?)
                }
                "--minimum-bootloader" => {
                    minimum_tcb.bootloader = parse_u8(&mut arguments, "--minimum-bootloader")?
                }
                "--minimum-tee" => minimum_tcb.tee = parse_u8(&mut arguments, "--minimum-tee")?,
                "--minimum-snp" => minimum_tcb.snp = parse_u8(&mut arguments, "--minimum-snp")?,
                "--minimum-microcode" => {
                    minimum_tcb.microcode = parse_u8(&mut arguments, "--minimum-microcode")?
                }
                "--minimum-guest-svn" => {
                    minimum_guest_svn = value(&mut arguments, "--minimum-guest-svn")?
                        .parse()
                        .map_err(invalid_number("--minimum-guest-svn"))?
                }
                "--allow-smt" => allow_smt = true,
                "--require-single-socket" => require_single_socket = true,
                "--allow-missing-crl" => revocation = SnpRevocationPolicy::AllowUnavailable,
                "--help" | "-h" => {
                    println!(
                        "usage: verify_snp_attestation --input PATH|- --measurement 96_HEX [--crl AMD_CRL.der] [--minimum-fmc N] [--minimum-bootloader N] [--minimum-tee N] [--minimum-snp N] [--minimum-microcode N] [--minimum-guest-svn N] [--allow-smt] [--require-single-socket] [--allow-missing-crl]"
                    );
                    std::process::exit(0);
                }
                other => return Err(invalid_argument(format!("unknown argument {other:?}"))),
            }
        }
        Ok(Self {
            input: input.ok_or_else(|| invalid_argument("missing --input"))?,
            crl,
            measurement: measurement.ok_or_else(|| invalid_argument("missing --measurement"))?,
            minimum_tcb,
            minimum_guest_svn,
            allow_smt,
            require_single_socket,
            revocation,
        })
    }
}

fn value(arguments: &mut impl Iterator<Item = String>, option: &str) -> Result<String, io::Error> {
    attestation_support::value(arguments, option)
}

fn parse_u8(
    arguments: &mut impl Iterator<Item = String>,
    option: &'static str,
) -> Result<u8, io::Error> {
    value(arguments, option)?
        .parse()
        .map_err(invalid_number(option))
}

fn invalid_number(option: &'static str) -> impl FnOnce(std::num::ParseIntError) -> io::Error {
    move |source| invalid_argument(format!("invalid integer for {option}: {source}"))
}

fn invalid_argument(message: impl Into<String>) -> io::Error {
    attestation_support::invalid_argument(message)
}
