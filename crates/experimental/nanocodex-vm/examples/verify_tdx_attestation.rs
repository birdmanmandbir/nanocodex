use std::{io, path::PathBuf};

use nanocodex_vm::host::{
    NativeVerifierSet, NvidiaNvattestVerifier, TdxVerificationPolicy, TdxVerifier,
    verify_attestation,
};

mod attestation_support;

use attestation_support::{
    invalid_argument, load_attestation, now_unix_seconds, parse_hex, print_verified, read_bounded,
    value,
};

const MAX_COLLATERAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_ROOT_BYTES: usize = 64 * 1024;

struct Options {
    input: PathBuf,
    collateral: PathBuf,
    intel_root: Option<PathBuf>,
    nvidia_policy: Option<PathBuf>,
    nvattest: PathBuf,
    mr_td: [u8; 48],
    rt_mrs: [[u8; 48]; 4],
    mr_config_id: Option<[u8; 48]>,
    mr_owner: Option<[u8; 48]>,
    mr_owner_config: Option<[u8; 48]>,
    xfam: Option<[u8; 8]>,
    allow_dynamic_platform: bool,
    allow_cached_keys: bool,
    allow_smt: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::parse()?;
    let attestation = load_attestation(&options.input)?;
    attestation.verify_key_proof()?;
    let challenge = attestation.bundle().request().challenge().clone();
    let collateral = read_bounded(&options.collateral, MAX_COLLATERAL_BYTES)?;
    let mut policy = TdxVerificationPolicy::new(
        challenge.policy_id(),
        &collateral,
        options.mr_td,
        options.rt_mrs,
    )?;
    if let Some(path) = options.intel_root {
        policy = policy.with_intel_root(read_bounded(&path, MAX_ROOT_BYTES)?);
    }
    if let Some(value) = options.mr_config_id {
        policy = policy.with_mr_config_id(value);
    }
    if let Some(value) = options.mr_owner {
        policy = policy.with_mr_owner(value);
    }
    if let Some(value) = options.mr_owner_config {
        policy = policy.with_mr_owner_config(value);
    }
    if let Some(value) = options.xfam {
        policy = policy.with_xfam(value);
    }
    policy = policy
        .allow_dynamic_platform(options.allow_dynamic_platform)
        .allow_cached_keys(options.allow_cached_keys)
        .allow_smt(options.allow_smt);
    let verifier = TdxVerifier::new(policy);
    let has_nvidia = attestation.bundle().request().nvidia_profile().is_some();
    if has_nvidia != options.nvidia_policy.is_some() {
        return Err(invalid_argument(
            "--nvidia-policy is required exactly when the attestation contains NVIDIA evidence",
        )
        .into());
    }
    let bundle = attestation.into_bundle();
    let now = now_unix_seconds()?;
    let verified = if let Some(nvidia_policy) = options.nvidia_policy {
        let nvidia = NvidiaNvattestVerifier::program(options.nvattest, nvidia_policy);
        let verifiers = NativeVerifierSet::new(verifier, nvidia);
        verify_attestation(bundle, &challenge, now, &verifiers).await?
    } else {
        verify_attestation(bundle, &challenge, now, &verifier).await?
    };
    print_verified(&verified, challenge.policy_id())
}

impl Options {
    fn parse() -> Result<Self, io::Error> {
        let mut input = None;
        let mut collateral = None;
        let mut intel_root = None;
        let mut nvidia_policy = None;
        let mut nvattest = PathBuf::from("nvattest");
        let mut mr_td = None;
        let mut rt_mrs = [None; 4];
        let mut mr_config_id = None;
        let mut mr_owner = None;
        let mut mr_owner_config = None;
        let mut xfam = None;
        let mut allow_dynamic_platform = false;
        let mut allow_cached_keys = false;
        let mut allow_smt = false;
        let mut arguments = std::env::args().skip(1);
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--input" => input = Some(value(&mut arguments, "--input")?.into()),
                "--collateral" => collateral = Some(value(&mut arguments, "--collateral")?.into()),
                "--intel-root" => intel_root = Some(value(&mut arguments, "--intel-root")?.into()),
                "--nvidia-policy" => {
                    nvidia_policy = Some(value(&mut arguments, "--nvidia-policy")?.into())
                }
                "--nvattest" => nvattest = value(&mut arguments, "--nvattest")?.into(),
                "--mrtd" => mr_td = Some(parse_hex(&value(&mut arguments, "--mrtd")?, "MRTD")?),
                "--rtmr0" | "--rtmr1" | "--rtmr2" | "--rtmr3" => {
                    let index = usize::from(argument.as_bytes()[6] - b'0');
                    rt_mrs[index] = Some(parse_hex(
                        &value(&mut arguments, &argument)?,
                        &argument[2..].to_ascii_uppercase(),
                    )?);
                }
                "--mr-config-id" => {
                    mr_config_id = Some(parse_hex(
                        &value(&mut arguments, "--mr-config-id")?,
                        "MRCONFIGID",
                    )?)
                }
                "--mr-owner" => {
                    mr_owner = Some(parse_hex(&value(&mut arguments, "--mr-owner")?, "MROWNER")?)
                }
                "--mr-owner-config" => {
                    mr_owner_config = Some(parse_hex(
                        &value(&mut arguments, "--mr-owner-config")?,
                        "MROWNERCONFIG",
                    )?)
                }
                "--xfam" => xfam = Some(parse_hex(&value(&mut arguments, "--xfam")?, "XFAM")?),
                "--allow-dynamic-platform" => allow_dynamic_platform = true,
                "--allow-cached-keys" => allow_cached_keys = true,
                "--allow-smt" => allow_smt = true,
                "--help" | "-h" => {
                    println!(
                        "usage: verify_tdx_attestation --input PATH|- --collateral PATH --mrtd 96_HEX --rtmr0 96_HEX --rtmr1 96_HEX --rtmr2 96_HEX --rtmr3 96_HEX [--intel-root DER_PATH] [--nvidia-policy REGO_PATH] [--nvattest PATH] [--mr-config-id 96_HEX] [--mr-owner 96_HEX] [--mr-owner-config 96_HEX] [--xfam 16_HEX] [--allow-dynamic-platform] [--allow-cached-keys] [--allow-smt]"
                    );
                    std::process::exit(0);
                }
                other => return Err(invalid_argument(format!("unknown argument {other:?}"))),
            }
        }
        Ok(Self {
            input: input.ok_or_else(|| invalid_argument("missing --input"))?,
            collateral: collateral.ok_or_else(|| invalid_argument("missing --collateral"))?,
            intel_root,
            nvidia_policy,
            nvattest,
            mr_td: mr_td.ok_or_else(|| invalid_argument("missing --mrtd"))?,
            rt_mrs: [0, 1, 2, 3].map(|index| {
                rt_mrs[index].unwrap_or_else(|| {
                    eprintln!("missing --rtmr{index}");
                    std::process::exit(2);
                })
            }),
            mr_config_id,
            mr_owner,
            mr_owner_config,
            xfam,
            allow_dynamic_platform,
            allow_cached_keys,
            allow_smt,
        })
    }
}
