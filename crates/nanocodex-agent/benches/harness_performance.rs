//! Deterministic agent-harness benchmark with no provider, network, sandbox, or tool process.

use std::{
    collections::BTreeSet,
    env, fs,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Instant,
};

use eyre::{Result, WrapErr, bail, eyre};
use nanocodex_agent::{
    AgentEvents, ExecutionEnvironment, Nanocodex, OpenAi, ResponseError, Thinking, Tools,
    TurnResult,
    events::{AgentEventKind, monotonic_now_ns},
};
use nanocodex_oai_api::{
    ResponseEvent,
    responses::{ContentItem, MessageRole, ResponseItem, Usage, WarmupResponse},
    tower::{
        GenerationOutput, ResponsePipelineStats, ResponsesAttempt, ResponsesAttemptKind,
        ResponsesOutput, ResponsesServiceResponse,
    },
};
use serde::{Deserialize, Serialize};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, get_current_pid};
use tower::Service;

const PROMPT_CACHE_KEY: &str = "nanocodex-harness-performance-v1";

#[derive(Clone)]
struct ScriptedResponses {
    service_id: u64,
    state: Arc<HarnessState>,
}

#[derive(Default)]
struct HarnessState {
    next_service_id: AtomicU64,
    next_response_id: AtomicU64,
    attempts: Mutex<Vec<AttemptRecord>>,
}

#[derive(Clone, Serialize)]
struct AttemptRecord {
    service_id: u64,
    kind: &'static str,
    prompt_cache_key: String,
    model_call_index: Option<u32>,
    physical_attempt: u32,
    full_replay: bool,
    previous_response_id: bool,
    input_items: usize,
    input_bytes: usize,
    input_fnv1a64: String,
}

impl HarnessState {
    fn new_service(self: &Arc<Self>) -> ScriptedResponses {
        ScriptedResponses {
            service_id: self.next_service_id.fetch_add(1, Ordering::Relaxed) + 1,
            state: Arc::clone(self),
        }
    }

    fn response_id(&self) -> String {
        let id = self.next_response_id.fetch_add(1, Ordering::Relaxed) + 1;
        format!("resp_harness_{id}")
    }

    fn record(&self, record: AttemptRecord) {
        self.attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(record);
    }

    fn records(&self) -> Vec<AttemptRecord> {
        self.attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Service<ResponsesAttempt> for ScriptedResponses {
    type Response = ResponsesServiceResponse;
    type Error = ResponseError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ResponsesAttempt) -> Self::Future {
        let service_id = self.service_id;
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let input = request.input_items().collect::<Vec<_>>();
            let encoded = serde_json::to_vec(&input).map_err(ResponseError::service)?;
            let input_bytes = encoded.len();
            state.record(AttemptRecord {
                service_id,
                kind: attempt_kind(request.kind()),
                prompt_cache_key: request.prompt_cache_key().to_owned(),
                model_call_index: request.model_call_index(),
                physical_attempt: request.attempt(),
                full_replay: request.is_full_replay(),
                previous_response_id: request.previous_response_id().is_some(),
                input_items: input.len(),
                input_bytes,
                input_fnv1a64: fnv1a64(&encoded),
            });

            match request.kind() {
                ResponsesAttemptKind::Warmup => Ok(ResponsesServiceResponse::new(
                    ResponsesOutput::Warmup(WarmupResponse {
                        id: state.response_id(),
                        usage: None,
                    }),
                )),
                ResponsesAttemptKind::Generation => {
                    let started = Instant::now();
                    tokio::task::yield_now().await;
                    let first_output_ns = elapsed_ns(started);
                    request
                        .emit(ResponseEvent::OutputTextDelta("done".to_owned()))
                        .await;
                    let message = ResponseItem::message(
                        MessageRole::Assistant,
                        [ContentItem::output_text("done")],
                    );
                    request
                        .emit(ResponseEvent::OutputItemDone(message.clone()))
                        .await;
                    let input_tokens = u64::try_from(input_bytes.div_ceil(4)).unwrap_or(u64::MAX);
                    Ok(ResponsesServiceResponse::new(ResponsesOutput::Generation(
                        GenerationOutput {
                            id: state.response_id(),
                            status: "completed".to_owned(),
                            end_turn: Some(true),
                            final_message: Some("done".to_owned()),
                            output_items: vec![message],
                            code_calls: Vec::new(),
                            usage: Some(Usage {
                                input_tokens,
                                output_tokens: 1,
                                total_tokens: input_tokens.saturating_add(1),
                                ..Usage::default()
                            }),
                            time_to_first_event_ns: first_output_ns,
                            time_to_first_output_ns: Some(first_output_ns),
                            pipeline_stats: ResponsePipelineStats {
                                event_count: 2,
                                display_delta_count: 1,
                                display_delta_bytes: 4,
                                ..ResponsePipelineStats::default()
                            },
                        },
                    )))
                }
                ResponsesAttemptKind::Compaction => Err(ResponseError::service(
                    std::io::Error::other("the harness workload unexpectedly requested compaction"),
                )),
                _ => Err(ResponseError::service(std::io::Error::other(
                    "the harness received an unknown Responses attempt kind",
                ))),
            }
        })
    }
}

fn attempt_kind(kind: ResponsesAttemptKind) -> &'static str {
    match kind {
        ResponsesAttemptKind::Warmup => "warmup",
        ResponsesAttemptKind::Generation => "generation",
        ResponsesAttemptKind::Compaction => "compaction",
        _ => "unknown",
    }
}

#[derive(Clone)]
struct Args {
    history_turns: usize,
    prompt_bytes: usize,
    prefix_bytes: usize,
    retained_forks: usize,
    cache_probe_forks: usize,
    startup_samples: usize,
    source_commit: String,
    output: Option<PathBuf>,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = Self {
            history_turns: 32,
            prompt_bytes: 4 * 1_024,
            prefix_bytes: 32 * 1_024,
            retained_forks: 128,
            cache_probe_forks: 3,
            startup_samples: 50,
            source_commit: option_env!("NANOCODEX_SOURCE_COMMIT")
                .unwrap_or("unknown")
                .to_owned(),
            output: None,
        };
        let mut values = env::args().skip(1);
        while let Some(name) = values.next() {
            if name == "--bench" {
                continue;
            }
            let value = values
                .next()
                .ok_or_else(|| eyre!("missing value for {name}"))?;
            match name.as_str() {
                "--history-turns" => args.history_turns = parse_usize(&name, &value)?,
                "--prompt-bytes" => args.prompt_bytes = parse_usize(&name, &value)?,
                "--prefix-bytes" => args.prefix_bytes = parse_usize(&name, &value)?,
                "--retained-forks" => args.retained_forks = parse_usize(&name, &value)?,
                "--cache-probe-forks" => args.cache_probe_forks = parse_usize(&name, &value)?,
                "--startup-samples" => args.startup_samples = parse_usize(&name, &value)?,
                "--source-commit" => args.source_commit = value,
                "--output" => args.output = Some(PathBuf::from(value)),
                _ => bail!("unknown argument {name}"),
            }
        }
        if args.history_turns < 4 {
            bail!("--history-turns must be at least 4");
        }
        if args.prompt_bytes < 64 || args.prefix_bytes < 64 {
            bail!("--prompt-bytes and --prefix-bytes must be at least 64");
        }
        if args.retained_forks == 0
            || args.cache_probe_forks == 0
            || args.cache_probe_forks > args.retained_forks
            || args.startup_samples == 0
        {
            bail!(
                "fork and startup sample counts must be non-zero, and cache probes cannot exceed retained forks"
            );
        }
        Ok(args)
    }
}

fn parse_usize(name: &str, value: &str) -> Result<usize> {
    value
        .parse()
        .wrap_err_with(|| format!("{name} requires a positive integer"))
}

#[derive(Serialize)]
struct BenchmarkReport {
    schema_version: u32,
    implementation: &'static str,
    source_commit: String,
    platform: Platform,
    noise_boundary: NoiseBoundary,
    configuration: Configuration,
    startup: StartupReport,
    ttft: TtftReport,
    memory: MemoryReport,
    fork: ForkReport,
    prompt_cache: PromptCacheReport,
}

#[derive(Serialize)]
struct Platform {
    os: &'static str,
    arch: &'static str,
    tokio_worker_threads: usize,
}

#[derive(Serialize)]
struct NoiseBoundary {
    provider: &'static str,
    network: &'static str,
    sandbox: &'static str,
    tools: &'static str,
    model_output: &'static str,
}

#[derive(Serialize)]
struct Configuration {
    history_turns: usize,
    prompt_bytes: usize,
    prefix_bytes: usize,
    retained_forks: usize,
    cache_probe_forks: usize,
    startup_samples: usize,
}

#[derive(Serialize)]
struct StartupReport {
    process_main_to_runtime_ready_ns: u64,
    tokio_runtime_build_ns: u64,
    fresh_agent_build_ns: Distribution,
    build_start_to_prompt_accepted_ns: Distribution,
    build_start_to_first_delta_ns: Distribution,
}

#[derive(Serialize)]
struct TtftReport {
    cold_submit_to_first_delta_ns: u64,
    cold_accepted_to_first_delta_ns: u64,
    warm_submit_to_first_delta_ns: Distribution,
    warm_accepted_to_first_delta_ns: Distribution,
    warm_event_delivery_ns: Distribution,
    warm_submit_to_completion_ns: Distribution,
}

#[derive(Serialize)]
struct MemoryReport {
    unit: &'static str,
    process_before_runtime: u64,
    runtime_ready: u64,
    root_agent_ready: u64,
    after_history: u64,
    with_retained_forks: u64,
    retained_forks_delta: u64,
    retained_forks_bytes_each: u64,
}

#[derive(Serialize)]
struct ForkReport {
    historical_turns: Vec<usize>,
    retained_forks: usize,
    construction_ns: Distribution,
}

#[derive(Serialize)]
struct PromptCacheReport {
    expected_key: &'static str,
    observed_keys: Vec<String>,
    stable_key: bool,
    cache_probe_agents: usize,
    warmup_lifecycle_count: usize,
    warmup_sources: Vec<String>,
    service_instances: usize,
    service_warmup_attempts: usize,
    service_warmups_avoided: usize,
    service_warmup_avoidance_ratio: f64,
    warmup_prefix_hashes: Vec<String>,
    generation_requests: usize,
    incremental_generation_requests: usize,
    full_replay_generation_requests: usize,
    incremental_generation_ratio: f64,
    generation_input_items: usize,
    generation_input_bytes: usize,
    requests: Vec<AttemptRecord>,
}

#[derive(Serialize)]
struct Distribution {
    n: usize,
    min: u64,
    p50: u64,
    p95: u64,
    max: u64,
    mean: u64,
}

impl Distribution {
    fn new(samples: &[u64]) -> Self {
        let mut ordered = samples.to_vec();
        ordered.sort_unstable();
        let total = ordered.iter().copied().fold(0_u128, |total, value| {
            total.saturating_add(u128::from(value))
        });
        let mean = total
            .checked_div(ordered.len() as u128)
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(u64::MAX);
        Self {
            n: ordered.len(),
            min: ordered[0],
            p50: percentile(&ordered, 50),
            p95: percentile(&ordered, 95),
            max: ordered[ordered.len() - 1],
            mean,
        }
    }
}

fn percentile(ordered: &[u64], percentile: usize) -> u64 {
    let index = (ordered.len() - 1).saturating_mul(percentile).div_ceil(100);
    ordered[index]
}

#[derive(Default)]
struct TurnObservation {
    accepted_ns: u64,
    first_delta_emitted_ns: u64,
    submit_to_first_delta_ns: u64,
    accepted_to_first_delta_ns: u64,
    event_delivery_ns: u64,
    submit_to_completion_ns: u64,
    warmup_key: Option<String>,
    warmup_source: Option<String>,
}

#[derive(Deserialize)]
struct WarmupStartedPayload {
    prompt_cache_key: String,
}

#[derive(Deserialize)]
struct WarmupCompletedPayload {
    source: String,
}

struct RssSampler {
    system: System,
    pid: sysinfo::Pid,
}

impl RssSampler {
    fn new() -> Result<Self> {
        Ok(Self {
            system: System::new(),
            pid: get_current_pid().map_err(|error| eyre!(error))?,
        })
    }

    fn sample(&mut self) -> Result<u64> {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[self.pid]),
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        self.system
            .process(self.pid)
            .map(sysinfo::Process::memory)
            .ok_or_else(|| eyre!("the benchmark process disappeared from the process table"))
    }
}

fn main() -> Result<()> {
    let entered = Instant::now();
    let args = Args::parse()?;
    let mut rss = RssSampler::new()?;
    let process_before_runtime = rss.sample()?;
    let runtime_started = Instant::now();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .wrap_err("failed to build the benchmark Tokio runtime")?;
    let tokio_runtime_build_ns = elapsed_ns(runtime_started);
    let process_main_to_runtime_ready_ns = elapsed_ns(entered);
    let runtime_ready = rss.sample()?;

    let mut report = runtime.block_on(run_benchmark(&args, &mut rss))?;
    report.startup.process_main_to_runtime_ready_ns = process_main_to_runtime_ready_ns;
    report.startup.tokio_runtime_build_ns = tokio_runtime_build_ns;
    report.memory.process_before_runtime = process_before_runtime;
    report.memory.runtime_ready = runtime_ready;
    let encoded = serde_json::to_string_pretty(&report)? + "\n";
    if let Some(path) = &args.output {
        fs::write(path, &encoded)
            .wrap_err_with(|| format!("failed to write {}", path.display()))?;
    }
    print!("{encoded}");
    Ok(())
}

async fn run_benchmark(args: &Args, rss: &mut RssSampler) -> Result<BenchmarkReport> {
    let workspace = tempfile::tempdir().wrap_err("failed to create benchmark workspace")?;
    let state = Arc::new(HarnessState::default());
    let service_state = Arc::clone(&state);
    let openai = OpenAi::builder("harness-only")
        .service(move || service_state.new_service())
        .build()?;
    let tools = Tools::builder().without_defaults().build()?;
    let instructions = sized_text("Deterministic harness prefix. ", args.prefix_bytes);
    let environment = ExecutionEnvironment::new("2026-08-22", "Etc/UTC");
    let root_builder = Nanocodex::builder(openai.clone())
        .instructions(instructions.clone())
        .thinking(Thinking::Low)
        .workspace(workspace.path())
        .execution_environment(environment.clone())
        .prompt_cache_key(PROMPT_CACHE_KEY)
        .shared_prompt_cache()
        .tools(tools.clone());
    let startup_builder = Nanocodex::builder(openai)
        .instructions(instructions)
        .thinking(Thinking::Low)
        .workspace(workspace.path())
        .execution_environment(environment)
        .prompt_cache_key(PROMPT_CACHE_KEY)
        .tools(tools);

    let root_build_started = Instant::now();
    let (root, mut root_events) = root_builder.build()?;
    let _root_build_ns = elapsed_ns(root_build_started);
    let root_agent_ready = rss.sample()?;

    let mut checkpoints = Vec::with_capacity(args.history_turns);
    let mut observations = Vec::with_capacity(args.history_turns);
    for turn in 1..=args.history_turns {
        let prompt = sized_text(&format!("Harness turn {turn:04}. "), args.prompt_bytes);
        let (result, observation) = measured_turn(&root, &mut root_events, prompt).await?;
        checkpoints.push(result);
        observations.push(observation);
    }
    let after_history = rss.sample()?;

    let historical_turns = historical_turns(args.history_turns);
    let mut retained = Vec::with_capacity(args.retained_forks);
    let mut fork_samples = Vec::with_capacity(args.retained_forks);
    for sample in 0..args.retained_forks {
        let turn = historical_turns[sample % historical_turns.len()];
        let started = Instant::now();
        let child = root.fork_from(&checkpoints[turn - 1]).await?;
        fork_samples.push(elapsed_ns(started));
        retained.push(child);
    }
    tokio::task::yield_now().await;
    let with_retained_forks = rss.sample()?;
    let retained_forks_delta = with_retained_forks.saturating_sub(after_history);

    let mut cache_observations = Vec::with_capacity(args.cache_probe_forks + 1);
    if let Some(first) = observations.first() {
        cache_observations.push((first.warmup_key.clone(), first.warmup_source.clone()));
    }
    for (index, (child, events)) in retained.iter_mut().take(args.cache_probe_forks).enumerate() {
        let prompt = sized_text(&format!("Fork cache probe {index:04}. "), args.prompt_bytes);
        let (_, observation) = measured_turn(child, events, prompt).await?;
        cache_observations.push((observation.warmup_key, observation.warmup_source));
    }
    let workload_attempts = state.records();

    for (child, _) in &retained {
        child.shutdown().await?;
    }
    drop(retained);
    root.shutdown().await?;

    let mut startup_build = Vec::with_capacity(args.startup_samples);
    let mut startup_accept = Vec::with_capacity(args.startup_samples);
    let mut startup_delta = Vec::with_capacity(args.startup_samples);
    for sample in 0..args.startup_samples {
        let build_started_ns = monotonic_now_ns();
        let build_started = Instant::now();
        let (agent, mut events) = startup_builder.clone().build()?;
        startup_build.push(elapsed_ns(build_started));
        let prompt = sized_text(
            &format!("Fresh agent startup sample {sample:04}. "),
            args.prompt_bytes,
        );
        let (_, observation) = measured_turn(&agent, &mut events, prompt).await?;
        startup_accept.push(observation.accepted_ns.saturating_sub(build_started_ns));
        startup_delta.push(
            observation
                .first_delta_emitted_ns
                .saturating_sub(build_started_ns),
        );
        agent.shutdown().await?;
    }

    let cold = observations
        .first()
        .ok_or_else(|| eyre!("the benchmark produced no turn observations"))?;
    let warm = observations
        .get(1..)
        .ok_or_else(|| eyre!("the benchmark produced no warm turn observations"))?;
    let warm_submit = warm
        .iter()
        .map(|sample| sample.submit_to_first_delta_ns)
        .collect::<Vec<_>>();
    let warm_accepted = warm
        .iter()
        .map(|sample| sample.accepted_to_first_delta_ns)
        .collect::<Vec<_>>();
    let warm_delivery = warm
        .iter()
        .map(|sample| sample.event_delivery_ns)
        .collect::<Vec<_>>();
    let warm_completion = warm
        .iter()
        .map(|sample| sample.submit_to_completion_ns)
        .collect::<Vec<_>>();
    let prompt_cache = prompt_cache_report(&cache_observations, &workload_attempts);
    if !prompt_cache.stable_key {
        bail!("prompt-cache identity changed across root and fork requests");
    }
    if prompt_cache.service_warmup_attempts != 1
        || prompt_cache.service_warmups_avoided != args.cache_probe_forks
        || prompt_cache.service_instances != prompt_cache.cache_probe_agents
    {
        bail!(
            "expected one service warmup, one avoided warmup per cache-probe fork, and one service per agent; got {}, {}, and {}",
            prompt_cache.service_warmup_attempts,
            prompt_cache.service_warmups_avoided,
            prompt_cache.service_instances
        );
    }

    Ok(BenchmarkReport {
        schema_version: 1,
        implementation: "nanocodex",
        source_commit: args.source_commit.clone(),
        platform: Platform {
            os: env::consts::OS,
            arch: env::consts::ARCH,
            tokio_worker_threads: 1,
        },
        noise_boundary: NoiseBoundary {
            provider: "in_process_scripted_tower_service",
            network: "none",
            sandbox: "none",
            tools: "empty_registry",
            model_output: "one_scheduled_yield_then_done",
        },
        configuration: Configuration {
            history_turns: args.history_turns,
            prompt_bytes: args.prompt_bytes,
            prefix_bytes: args.prefix_bytes,
            retained_forks: args.retained_forks,
            cache_probe_forks: args.cache_probe_forks,
            startup_samples: args.startup_samples,
        },
        startup: StartupReport {
            process_main_to_runtime_ready_ns: 0,
            tokio_runtime_build_ns: 0,
            fresh_agent_build_ns: Distribution::new(&startup_build),
            build_start_to_prompt_accepted_ns: Distribution::new(&startup_accept),
            build_start_to_first_delta_ns: Distribution::new(&startup_delta),
        },
        ttft: TtftReport {
            cold_submit_to_first_delta_ns: cold.submit_to_first_delta_ns,
            cold_accepted_to_first_delta_ns: cold.accepted_to_first_delta_ns,
            warm_submit_to_first_delta_ns: Distribution::new(&warm_submit),
            warm_accepted_to_first_delta_ns: Distribution::new(&warm_accepted),
            warm_event_delivery_ns: Distribution::new(&warm_delivery),
            warm_submit_to_completion_ns: Distribution::new(&warm_completion),
        },
        memory: MemoryReport {
            unit: "bytes_rss",
            process_before_runtime: 0,
            runtime_ready: 0,
            root_agent_ready,
            after_history,
            with_retained_forks,
            retained_forks_delta,
            retained_forks_bytes_each: retained_forks_delta
                / u64::try_from(args.retained_forks).unwrap_or(u64::MAX),
        },
        fork: ForkReport {
            historical_turns,
            retained_forks: args.retained_forks,
            construction_ns: Distribution::new(&fork_samples),
        },
        prompt_cache,
    })
}

async fn measured_turn(
    agent: &Nanocodex,
    events: &mut AgentEvents,
    prompt: String,
) -> Result<(TurnResult, TurnObservation)> {
    let submitted_ns = monotonic_now_ns();
    let turn = agent.prompt(prompt).await?;
    let accepted_ns = monotonic_now_ns();
    let mut observation = TurnObservation {
        accepted_ns,
        ..TurnObservation::default()
    };
    let mut completion_ns = None;
    while let Some(timed) = events.recv_timed().await {
        let received_ns = monotonic_now_ns();
        match timed.event.kind {
            AgentEventKind::ModelWarmupStarted => {
                let payload: WarmupStartedPayload =
                    serde_json::from_str(timed.event.payload.get())?;
                observation.warmup_key = Some(payload.prompt_cache_key);
            }
            AgentEventKind::ModelWarmupCompleted => {
                let payload: WarmupCompletedPayload =
                    serde_json::from_str(timed.event.payload.get())?;
                observation.warmup_source = Some(payload.source);
            }
            AgentEventKind::AssistantDelta if observation.first_delta_emitted_ns == 0 => {
                observation.first_delta_emitted_ns = timed.timing.emitted_ns;
                observation.submit_to_first_delta_ns =
                    timed.timing.emitted_ns.saturating_sub(submitted_ns);
                observation.accepted_to_first_delta_ns =
                    timed.timing.emitted_ns.saturating_sub(accepted_ns);
                observation.event_delivery_ns = received_ns.saturating_sub(timed.timing.emitted_ns);
            }
            AgentEventKind::RunCompleted | AgentEventKind::RunFailed => {
                completion_ns = Some(timed.timing.emitted_ns);
                break;
            }
            _ => {}
        }
    }
    let result = turn.result().await?;
    if result.final_message() != "done" {
        bail!(
            "scripted service returned an unexpected final message: {:?}",
            result.final_message()
        );
    }
    if observation.first_delta_emitted_ns == 0 {
        bail!("turn completed without an assistant delta");
    }
    observation.submit_to_completion_ns = completion_ns
        .ok_or_else(|| eyre!("turn completed without a terminal event"))?
        .saturating_sub(submitted_ns);
    Ok((result, observation))
}

fn prompt_cache_report(
    observations: &[(Option<String>, Option<String>)],
    attempts: &[AttemptRecord],
) -> PromptCacheReport {
    let observed_keys = attempts
        .iter()
        .map(|attempt| attempt.prompt_cache_key.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let warmup_lifecycle_count = observations.iter().filter(|(key, _)| key.is_some()).count();
    let warmup_sources = observations
        .iter()
        .filter_map(|(_, source)| source.clone())
        .collect();
    let warmups = attempts
        .iter()
        .filter(|attempt| attempt.kind == "warmup")
        .collect::<Vec<_>>();
    let generations = attempts
        .iter()
        .filter(|attempt| attempt.kind == "generation")
        .collect::<Vec<_>>();
    let incremental_generation_requests = generations
        .iter()
        .filter(|attempt| attempt.previous_response_id && !attempt.full_replay)
        .count();
    let full_replay_generation_requests = generations
        .iter()
        .filter(|attempt| attempt.full_replay)
        .count();
    let cache_probe_agents = observations.len();
    let service_instances = attempts
        .iter()
        .map(|attempt| attempt.service_id)
        .collect::<BTreeSet<_>>()
        .len();
    let service_warmups_avoided = cache_probe_agents.saturating_sub(warmups.len());
    let denominator = cache_probe_agents.max(1) as f64;
    let generation_denominator = generations.len().max(1) as f64;
    PromptCacheReport {
        expected_key: PROMPT_CACHE_KEY,
        stable_key: observed_keys.len() == 1 && observed_keys[0] == PROMPT_CACHE_KEY,
        observed_keys,
        cache_probe_agents,
        warmup_lifecycle_count,
        warmup_sources,
        service_instances,
        service_warmup_attempts: warmups.len(),
        service_warmups_avoided,
        service_warmup_avoidance_ratio: service_warmups_avoided as f64 / denominator,
        warmup_prefix_hashes: warmups
            .iter()
            .map(|attempt| attempt.input_fnv1a64.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        generation_requests: generations.len(),
        incremental_generation_requests,
        full_replay_generation_requests,
        incremental_generation_ratio: incremental_generation_requests as f64
            / generation_denominator,
        generation_input_items: generations.iter().map(|attempt| attempt.input_items).sum(),
        generation_input_bytes: generations.iter().map(|attempt| attempt.input_bytes).sum(),
        requests: attempts.to_vec(),
    }
}

fn historical_turns(turns: usize) -> Vec<usize> {
    [turns / 4, turns / 2, turns]
        .into_iter()
        .map(|turn| turn.max(1))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn sized_text(prefix: &str, bytes: usize) -> String {
    let mut text = String::with_capacity(bytes);
    text.push_str(prefix);
    text.extend(std::iter::repeat_n('x', bytes.saturating_sub(text.len())));
    text
}

fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn fnv1a64(bytes: &[u8]) -> String {
    let mut digest = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        digest ^= u64::from(*byte);
        digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{digest:016x}")
}
