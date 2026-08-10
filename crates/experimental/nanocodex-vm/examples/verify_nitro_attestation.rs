use std::{collections::BTreeMap, io, path::PathBuf};

use nanocodex_vm::host::{NitroVerificationPolicy, NitroVerifier, verify_attestation};

mod attestation_support;

use attestation_support::{
    invalid_argument, load_attestation, now_unix_seconds, parse_hex, print_verified, read_bounded,
    value,
};

const MAX_ROOT_BYTES: usize = 64 * 1024;

struct Options {
    input: PathBuf,
    aws_root: PathBuf,
    pcrs: BTreeMap<usize, Vec<u8>>,
    maximum_age_seconds: u64,
    maximum_future_skew_seconds: u64,
    module_id: Option<String>,
    exact_pcr_set: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::parse()?;
    let attestation = load_attestation(&options.input)?;
    attestation.verify_key_proof()?;
    let challenge = attestation.bundle().request().challenge().clone();
    let root = read_bounded(&options.aws_root, MAX_ROOT_BYTES)?;
    let mut policy = NitroVerificationPolicy::new(
        challenge.policy_id(),
        root,
        options.pcrs,
        options.maximum_age_seconds,
    )?
    .with_maximum_future_skew(options.maximum_future_skew_seconds)
    .with_exact_pcr_set(options.exact_pcr_set);
    if let Some(module_id) = options.module_id {
        policy = policy.with_module_id(module_id);
    }
    let verifier = NitroVerifier::new(policy);
    let verified = verify_attestation(
        attestation.into_bundle(),
        &challenge,
        now_unix_seconds()?,
        &verifier,
    )
    .await?;
    print_verified(&verified, challenge.policy_id())
}

impl Options {
    fn parse() -> Result<Self, io::Error> {
        let mut input = None;
        let mut aws_root = None;
        let mut pcrs = BTreeMap::new();
        let mut maximum_age_seconds = 300;
        let mut maximum_future_skew_seconds = 30;
        let mut module_id = None;
        let mut exact_pcr_set = false;
        let mut arguments = std::env::args().skip(1);
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--input" => input = Some(value(&mut arguments, "--input")?.into()),
                "--aws-root" => aws_root = Some(value(&mut arguments, "--aws-root")?.into()),
                "--pcr" => {
                    let encoded = value(&mut arguments, "--pcr")?;
                    let (index, digest) = encoded
                        .split_once('=')
                        .ok_or_else(|| invalid_argument("--pcr must use INDEX=96_HEX syntax"))?;
                    let index = index.parse::<usize>().map_err(|source| {
                        invalid_argument(format!("invalid PCR index: {source}"))
                    })?;
                    let digest = parse_hex::<48>(digest, "PCR")?.to_vec();
                    if pcrs.insert(index, digest).is_some() {
                        return Err(invalid_argument(format!("PCR {index} was repeated")));
                    }
                }
                "--maximum-age" => {
                    maximum_age_seconds = parse_u64(&mut arguments, "--maximum-age")?
                }
                "--maximum-future-skew" => {
                    maximum_future_skew_seconds =
                        parse_u64(&mut arguments, "--maximum-future-skew")?
                }
                "--module-id" => module_id = Some(value(&mut arguments, "--module-id")?),
                "--exact-pcr-set" => exact_pcr_set = true,
                "--help" | "-h" => {
                    println!(
                        "usage: verify_nitro_attestation --input PATH|- --aws-root DER_PATH --pcr INDEX=96_HEX [--pcr INDEX=96_HEX ...] [--maximum-age SECONDS] [--maximum-future-skew SECONDS] [--module-id ID] [--exact-pcr-set]"
                    );
                    std::process::exit(0);
                }
                other => return Err(invalid_argument(format!("unknown argument {other:?}"))),
            }
        }
        Ok(Self {
            input: input.ok_or_else(|| invalid_argument("missing --input"))?,
            aws_root: aws_root.ok_or_else(|| invalid_argument("missing --aws-root"))?,
            pcrs,
            maximum_age_seconds,
            maximum_future_skew_seconds,
            module_id,
            exact_pcr_set,
        })
    }
}

fn parse_u64(arguments: &mut impl Iterator<Item = String>, option: &str) -> Result<u64, io::Error> {
    value(arguments, option)?
        .parse()
        .map_err(|source| invalid_argument(format!("invalid integer for {option}: {source}")))
}
