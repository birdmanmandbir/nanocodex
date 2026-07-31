#[cfg(feature = "mpp")]
mod mpp_gate;

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use eyre::{Result, bail};
use futures_util::{StreamExt, stream};
use nanocentaur::{
    AdminAuthorizer, AgentManager, ApiState, CapabilityEgress, CapabilityName,
    CompositeSecretManager, ContentBlock, CreateAgent, CreateAgentResponse, CreateTurn,
    EgressProvider, EnvironmentSecretManager, FileSecretManager, FreePaymentGate,
    ManagedAgentFactory, ManagedEgress, MockAgentFactory, NanocodexAgentFactory, PaymentGate,
    PolicyStore, ProxyProfile, SecretManager, SecretRef, TurnDelivery, TurnStatus, TurnView,
    backend::run_vmm,
};
use nanocodex_oai_api::auth::OpenAiAuth;
use nanocodex_vm::tools::GuestRuntimeDisk;
use reqwest::Client;
use serde::Serialize;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "nanocentaur")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the hosted-agent REST API.
    Serve(Box<Serve>),
    /// Exercise N independent agents through the real HTTP API.
    Bench(Bench),
    #[command(hide = true)]
    Vmm {
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Args)]
struct Serve {
    #[arg(long, default_value = "127.0.0.1:3000")]
    listen: SocketAddr,
    #[arg(long, env = "NANOCENTAUR_API_KEY")]
    api_key: String,
    #[arg(long, env = "NANOCENTAUR_ADMIN_TOKEN")]
    admin_token: String,
    #[arg(long, default_value = ".nanocentaur")]
    state_directory: PathBuf,
    #[arg(long, value_enum, default_value_t = Backend::Mock)]
    backend: Backend,
    #[arg(long, default_value_t = 25)]
    mock_delay_ms: u64,
    #[arg(long, requires = "openai_api_key")]
    rootfs: Option<PathBuf>,
    #[arg(long, env = "OPENAI_API_KEY", requires = "rootfs")]
    openai_api_key: Option<String>,
    /// Static Linux guest runtime used with raw ext4 roots.
    #[arg(long, env = "NANOCODEX_VM_GUEST_RUNTIME", requires = "rootfs")]
    vm_guest_runtime: Option<PathBuf>,
    /// Directory containing the platform libkrun firmware library.
    #[arg(long, requires = "rootfs")]
    firmware_directory: Option<PathBuf>,
    #[arg(long = "allow-capability")]
    allowed_capabilities: Vec<String>,
    /// All non-tool capabilities use this server-owned proxy URL.
    #[arg(long)]
    egress_proxy_url: Option<String>,
    /// Resolve the proxy URL from `NANOCENTAUR_SECRET_<KEY>` on the host.
    #[arg(long, conflicts_with = "egress_proxy_url")]
    egress_proxy_secret: Option<String>,
    /// Host directory exposed as the `file` secret provider.
    #[arg(long)]
    secret_directory: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = Payments::Free)]
    payments: Payments,
    #[cfg(feature = "mpp")]
    #[command(flatten)]
    mpp: MppArgs,
}

#[derive(Clone, Copy, ValueEnum)]
enum Backend {
    Mock,
    Nanocodex,
}

#[derive(Clone, Copy, ValueEnum)]
enum Payments {
    Free,
    Mpp,
}

#[cfg(feature = "mpp")]
#[derive(Args)]
struct MppArgs {
    #[arg(long = "mpp-rpc-url", env = "NANOCENTAUR_MPP_RPC_URL")]
    rpc_url: Option<String>,
    #[arg(long = "mpp-currency", env = "NANOCENTAUR_MPP_CURRENCY")]
    currency: Option<String>,
    #[arg(long = "mpp-recipient", env = "NANOCENTAUR_MPP_RECIPIENT")]
    recipient: Option<String>,
    #[arg(long = "mpp-escrow", env = "NANOCENTAUR_MPP_ESCROW")]
    escrow: Option<String>,
    #[arg(long = "mpp-chain-id", env = "NANOCENTAUR_MPP_CHAIN_ID")]
    chain_id: Option<u64>,
    #[arg(long = "mpp-close-key", env = "NANOCENTAUR_MPP_CLOSE_KEY")]
    close_key: Option<String>,
    #[arg(
        long = "mpp-challenge-secret",
        env = "NANOCENTAUR_MPP_CHALLENGE_SECRET"
    )]
    challenge_secret: Option<String>,
    #[arg(long = "mpp-unit-price", env = "NANOCENTAUR_MPP_UNIT_PRICE")]
    unit_price: Option<u128>,
    #[arg(
        long = "mpp-suggested-deposit",
        env = "NANOCENTAUR_MPP_SUGGESTED_DEPOSIT"
    )]
    suggested_deposit: Option<String>,
    #[arg(long = "mpp-fee-payer", default_value_t = false)]
    fee_payer: bool,
}

#[derive(Args)]
struct Bench {
    #[arg(long, default_value = "http://127.0.0.1:3000")]
    url: String,
    #[arg(long, env = "NANOCENTAUR_API_KEY")]
    api_key: String,
    #[arg(long, default_value_t = 32)]
    agents: usize,
    #[arg(long)]
    concurrency: Option<usize>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let command = Cli::parse().command;
    if let Command::Vmm { config } = command {
        return run_vmm(&config).map_err(Into::into);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            match command {
                Command::Serve(arguments) => serve(*arguments).await,
                Command::Bench(arguments) => bench(arguments).await,
                Command::Vmm { .. } => unreachable!("VMM mode returned before starting Tokio"),
            }
        })
}

async fn serve(arguments: Serve) -> Result<()> {
    let capabilities = arguments
        .allowed_capabilities
        .iter()
        .map(|name| CapabilityName::new(name.clone()))
        .collect::<Result<Vec<_>, _>>()?;
    let proxy_url = match (
        arguments.egress_proxy_url.clone(),
        arguments.egress_proxy_secret.clone(),
    ) {
        (Some(url), None) => Some(url),
        (None, Some(key)) => EnvironmentSecretManager::new("NANOCENTAUR_SECRET_")
            .resolve(&SecretRef::new("environment", key))
            .await
            .map(Some)?,
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!("clap rejects conflicting proxy options"),
    };
    let proxy = proxy_url
        .map(|url| ProxyProfile::new("configured", url).map(Arc::new))
        .transpose()?;
    let mut egress = CapabilityEgress::new();
    for capability in capabilities
        .iter()
        .filter(|capability| !capability.as_str().starts_with("tools."))
    {
        egress = if let Some(profile) = &proxy {
            egress.proxy(capability.clone(), Arc::clone(profile))
        } else {
            egress.direct(capability.clone())
        };
    }

    let payments = payment_gate(&arguments)?;
    let policy = Arc::new(PolicyStore::open(
        arguments.state_directory.join("policy.sqlite"),
    )?);
    policy.bootstrap(
        "standalone",
        "Standalone API client",
        &arguments.api_key,
        "standalone",
        capabilities,
    )?;
    let managed_egress: Arc<dyn EgressProvider> =
        managed_egress(&arguments, Arc::clone(&policy), egress)?;

    let factory: Arc<dyn ManagedAgentFactory> = match arguments.backend {
        Backend::Mock => Arc::new(MockAgentFactory::new(Duration::from_millis(
            arguments.mock_delay_ms,
        ))),
        Backend::Nanocodex => {
            let rootfs = arguments
                .rootfs
                .clone()
                .ok_or_else(|| eyre::eyre!("--rootfs is required for nanocodex backend"))?;
            let api_key = arguments
                .openai_api_key
                .clone()
                .ok_or_else(|| eyre::eyre!("--openai-api-key is required for nanocodex backend"))?;
            let mut factory = NanocodexAgentFactory::new(
                OpenAiAuth::api_key(api_key),
                std::env::current_exe()?,
                &rootfs,
                &arguments.state_directory,
                managed_egress,
            );
            if rootfs.is_file() {
                let runtime = arguments
                    .vm_guest_runtime
                    .as_ref()
                    .ok_or_else(|| eyre::eyre!("raw ext4 roots require --vm-guest-runtime ELF"))?;
                let runtime =
                    GuestRuntimeDisk::prepare(runtime, arguments.state_directory.join("vm-cache"))?;
                factory = factory.guest_runtime_disk(runtime.path());
            } else if arguments.vm_guest_runtime.is_some() {
                bail!("--vm-guest-runtime is only valid with a raw ext4 root");
            }
            if let Some(firmware) = &arguments.firmware_directory {
                factory = factory.firmware_directory(firmware);
            }
            Arc::new(factory)
        }
    };
    let manager = Arc::new(AgentManager::new(factory, &arguments.state_directory)?);
    let app = ApiState::new(
        manager,
        policy,
        Arc::new(AdminAuthorizer::new(&arguments.admin_token)?),
        payments,
    )
    .router();
    let listener = tokio::net::TcpListener::bind(arguments.listen).await?;
    tracing::info!(address = %arguments.listen, "nanocentaur listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn managed_egress(
    arguments: &Serve,
    policy: Arc<PolicyStore>,
    egress: CapabilityEgress,
) -> Result<Arc<ManagedEgress>> {
    let mut manager = CompositeSecretManager::new().provider(
        "environment",
        Arc::new(EnvironmentSecretManager::new("NANOCENTAUR_SECRET_")),
    );
    if let Some(directory) = &arguments.secret_directory {
        manager = manager.provider("file", Arc::new(FileSecretManager::new(directory)?));
    }
    Ok(Arc::new(ManagedEgress::new(
        policy,
        Arc::new(manager),
        egress,
    )))
}

fn payment_gate(arguments: &Serve) -> Result<Arc<dyn PaymentGate>> {
    match arguments.payments {
        Payments::Free => Ok(Arc::new(FreePaymentGate)),
        Payments::Mpp => {
            #[cfg(feature = "mpp")]
            {
                use mpp_gate::{MppGateConfig, MppSessionGate};
                let required = |value: &Option<String>, name: &str| {
                    value
                        .clone()
                        .ok_or_else(|| eyre::eyre!("{name} is required with --payments mpp"))
                };
                let gate = MppSessionGate::new(MppGateConfig {
                    rpc_url: required(&arguments.mpp.rpc_url, "--mpp-rpc-url")?,
                    currency: required(&arguments.mpp.currency, "--mpp-currency")?,
                    recipient: required(&arguments.mpp.recipient, "--mpp-recipient")?,
                    escrow_contract: required(&arguments.mpp.escrow, "--mpp-escrow")?,
                    chain_id: arguments
                        .mpp
                        .chain_id
                        .ok_or_else(|| eyre::eyre!("--mpp-chain-id is required"))?,
                    close_key: required(&arguments.mpp.close_key, "--mpp-close-key")?,
                    challenge_secret: required(
                        &arguments.mpp.challenge_secret,
                        "--mpp-challenge-secret",
                    )?,
                    unit_price: arguments
                        .mpp
                        .unit_price
                        .ok_or_else(|| eyre::eyre!("--mpp-unit-price is required"))?,
                    suggested_deposit: arguments.mpp.suggested_deposit.clone(),
                    fee_payer: arguments.mpp.fee_payer,
                })?;
                Ok(Arc::new(gate))
            }
            #[cfg(not(feature = "mpp"))]
            {
                bail!("rebuild with `--features mpp` to enable MPP sessions")
            }
        }
    }
}

async fn bench(arguments: Bench) -> Result<()> {
    if arguments.agents == 0 {
        bail!("--agents must be greater than zero");
    }
    let concurrency = arguments.concurrency.unwrap_or(arguments.agents).max(1);
    let client = Client::new();
    let started = Instant::now();
    let results = stream::iter(0..arguments.agents)
        .map(|index| {
            benchmark_agent(
                client.clone(),
                arguments.url.clone(),
                arguments.api_key.clone(),
                index,
            )
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;
    let elapsed = started.elapsed();
    let mut latencies = results
        .into_iter()
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .map(|duration| duration.as_secs_f64() * 1_000.0)
        .collect::<Vec<_>>();
    latencies.sort_by(f64::total_cmp);
    let output = BenchmarkReport {
        agents: arguments.agents,
        concurrency,
        wall_ms: elapsed.as_secs_f64() * 1_000.0,
        throughput_agents_per_second: count_as_f64(arguments.agents) / elapsed.as_secs_f64(),
        latency_ms: LatencyReport {
            p50: percentile(&latencies, 50, 100),
            p95: percentile(&latencies, 95, 100),
            max: latencies.last().copied().unwrap_or_default(),
        },
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

async fn benchmark_agent(
    client: Client,
    base_url: String,
    api_key: String,
    index: usize,
) -> Result<Duration> {
    let started = Instant::now();
    let agent: CreateAgentResponse = client
        .post(format!("{base_url}/v1/agent/new"))
        .header("x-api-key", &api_key)
        .json(&CreateAgent {
            context_key: Some(format!("benchmark:{index}")),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let agent_id = agent.agent_id;
    let turn: nanocentaur::TurnActionResponse = client
        .post(format!("{base_url}/v1/agent/{agent_id}/turn"))
        .header("x-api-key", &api_key)
        .header("idempotency-key", format!("benchmark-{index}"))
        .json(&CreateTurn {
            delivery: TurnDelivery::Steer,
            content: vec![ContentBlock::Text {
                text: format!("ping {index}"),
            }],
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let turn_id = turn.turn_id;
    loop {
        let terminal: TurnView = client
            .get(format!("{base_url}/v1/agent/{agent_id}/turn/{turn_id}"))
            .header("x-api-key", &api_key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        match terminal.state {
            TurnStatus::Completed => break,
            TurnStatus::Failed | TurnStatus::Cancelled => {
                bail!("agent {index} ended in {:?}", terminal.state);
            }
            TurnStatus::Queued | TurnStatus::Running => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }
    Ok(started.elapsed())
}

#[derive(Serialize)]
struct BenchmarkReport {
    agents: usize,
    concurrency: usize,
    wall_ms: f64,
    throughput_agents_per_second: f64,
    latency_ms: LatencyReport,
}

#[derive(Serialize)]
struct LatencyReport {
    p50: f64,
    p95: f64,
    max: f64,
}

fn percentile(sorted: &[f64], numerator: usize, denominator: usize) -> f64 {
    let last = sorted.len().saturating_sub(1);
    let index = last
        .saturating_mul(numerator)
        .saturating_add(denominator / 2)
        / denominator;
    sorted.get(index).copied().unwrap_or_default()
}

fn count_as_f64(value: usize) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}
