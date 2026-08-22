use std::{env, net::IpAddr, path::PathBuf, sync::Arc, time::Duration};

use eyre::{Result, WrapErr, bail, eyre};
use nanocodex::{
    AgentEvents, Model, Nanocodex, OpenAi, Thinking, Tools, Turn, TurnResult, TurnUsage,
    agent::events::{
        AgentEventKind, AssistantDelta, ModelCallCompleted, ModelCallStarted, RunStarted,
        RunStatus, RunTerminal, monotonic_now_ns,
    },
    oai::{
        MODEL,
        auth::{
            OpenAiAuth, OpenAiAuthError, OpenAiAuthFuture, OpenAiAuthSnapshot, OpenAiAuthSource,
            load_chatgpt_auth,
        },
        responses::Usage,
        transport::ResponsesTransport,
    },
};
use serde::Serialize;
use tokio::time::timeout;

const EVENT_STREAM_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
const PROMPT_CACHE_KEY: &str = "nanocodex-paired-fx-model-latency-v1";

#[derive(Clone, Copy)]
enum Transport {
    WebSocket,
    Https,
}

impl Transport {
    const fn responses(self) -> ResponsesTransport {
        match self {
            Self::WebSocket => ResponsesTransport::WebSocket,
            Self::Https => ResponsesTransport::Https,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::WebSocket => "websocket",
            Self::Https => "https",
        }
    }

    const fn event_name(self) -> &'static str {
        match self {
            Self::WebSocket => "responses_websocket_v2",
            Self::Https => "responses_https_sse",
        }
    }
}

struct Args {
    cwd: PathBuf,
    auth_file: PathBuf,
    api_base_url: String,
    websocket_url: Option<String>,
    transport: Transport,
    instructions: String,
    prompt: String,
    expected: String,
    source_commit: String,
}

struct StartedTurn {
    turn: Turn,
    submitted_ns: u64,
    accepted_ns: u64,
}

struct CompletedTurn {
    result: TurnResult,
    result_completed_ns: u64,
    submitted_ns: u64,
    accepted_ns: u64,
    events: ObservedEvents,
}

#[derive(Default)]
struct ObservedEvents {
    assistant_text: String,
    assistant_messages: Vec<String>,
    assistant_delta_count: usize,
    event_count: usize,
    first_sequence: Option<u64>,
    last_sequence: Option<u64>,
    first_delta_emitted_ns: Option<u64>,
    first_delta_received_ns: Option<u64>,
    first_delta_source_received_ns: Option<u64>,
    run_started: Vec<RunStarted>,
    model_calls_started: Vec<ModelCallStarted>,
    model_calls_completed: Vec<ModelCallCompleted>,
    run_completed: Option<RunTerminal>,
}

#[derive(Serialize)]
struct BenchmarkReport {
    schema_version: u32,
    benchmark: &'static str,
    provenance: Provenance,
    transport: &'static str,
    timing_ns: TimingMeasurement,
    model_call: ModelCallMeasurement,
    usage: UsageMeasurement,
    turn_usage: TurnUsageMeasurement,
    events: EventMeasurement,
    verified: Verification,
}

#[derive(Serialize)]
struct Provenance {
    implementation: &'static str,
    source_commit: String,
    model: &'static str,
    thinking: &'static str,
    fast_mode: bool,
    workspace: String,
    instructions_fnv1a64: String,
    prompt_fnv1a64: String,
    expected_fnv1a64: String,
    prompt_cache_key_fnv1a64: String,
}

#[derive(Serialize)]
struct TimingMeasurement {
    prompt_submit_to_acceptance: u64,
    prompt_submit_to_first_assistant_delta_emitted: u64,
    prompt_acceptance_to_first_assistant_delta_emitted: u64,
    prompt_submit_to_first_assistant_delta_received: u64,
    prompt_acceptance_to_first_assistant_delta_received: u64,
    assistant_delta_emit_to_receive: u64,
    provider_source_to_assistant_delta_emit: Option<u64>,
    prompt_submit_to_result_completion: u64,
    prompt_acceptance_to_result_completion: u64,
}

#[derive(Serialize)]
struct ModelCallMeasurement {
    call_index: u32,
    attempt: u32,
    connection_generation: u32,
    status: String,
    duration_ns: u64,
    time_to_first_event_ns: u64,
    time_to_first_output_ns: Option<u64>,
    tool_calls: usize,
}

#[allow(clippy::struct_field_names)]
#[derive(Serialize)]
struct UsageMeasurement {
    reported: bool,
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

#[allow(clippy::struct_field_names)]
#[derive(Serialize)]
struct TurnUsageMeasurement {
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

#[derive(Serialize)]
struct EventMeasurement {
    count: usize,
    first_sequence: u64,
    last_sequence: u64,
    assistant_delta_count: usize,
    sequences_contiguous: bool,
}

#[derive(Serialize)]
struct Verification {
    final_output: bool,
    assistant_deltas: bool,
    one_model_call: bool,
    zero_tool_calls: bool,
    run_completed: bool,
    clean_shutdown: bool,
    auth_refresh_disabled: bool,
}

struct FrozenChatGptAuth {
    snapshot: OpenAiAuthSnapshot,
}

impl OpenAiAuthSource for FrozenChatGptAuth {
    fn validate(&self) -> Result<(), OpenAiAuthError> {
        Ok(())
    }

    fn snapshot(&self) -> OpenAiAuthFuture<'_, Result<OpenAiAuthSnapshot, OpenAiAuthError>> {
        let snapshot = self.snapshot.clone();
        Box::pin(async move { Ok(snapshot) })
    }

    fn recover_unauthorized(
        &self,
        _rejected: &OpenAiAuthSnapshot,
    ) -> OpenAiAuthFuture<'_, Result<(), OpenAiAuthError>> {
        Box::pin(async {
            Err(OpenAiAuthError::LoginRequired(Arc::from(
                "the model-latency benchmark uses one immutable auth snapshot",
            )))
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    let managed_auth = load_chatgpt_auth(&args.auth_file)
        .wrap_err_with(|| format!("failed to load {}", args.auth_file.display()))?;
    let auth = OpenAiAuth::managed_chatgpt(Arc::new(FrozenChatGptAuth {
        snapshot: managed_auth
            .snapshot()
            .await
            .wrap_err("failed to freeze the benchmark auth snapshot")?,
    }));
    nanocodex::oai::transport::install_default_rustls_crypto_provider();
    let http_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .wrap_err("failed to build the benchmark HTTP client")?;
    let mut openai = OpenAi::builder(auth)
        .model(Model::Sol)
        .thinking(Thinking::Low)
        .fast_mode(false)
        .transport(args.transport.responses())
        .api_base_url(args.api_base_url.clone())
        .http_client(http_client);
    if let Some(websocket_url) = &args.websocket_url {
        openai = openai.websocket_url(websocket_url.clone());
    }
    let openai = openai.build()?;
    let tools = Tools::builder().build()?;
    let (agent, mut events) = Nanocodex::builder(openai)
        .model(Model::Sol)
        .instructions(args.instructions.clone())
        .thinking(Thinking::Low)
        .fast_mode(false)
        .prompt_cache_key(PROMPT_CACHE_KEY)
        .workspace(&args.cwd)
        .tools(tools)
        .build()?;

    let completed = measured_generation(&agent, &mut events, &args.prompt).await;
    let shutdown = agent.shutdown().await;
    if let Err(shutdown_error) = shutdown {
        return match completed {
            Ok(_) => Err(shutdown_error).wrap_err("agent shutdown failed"),
            Err(run_error) => Err(eyre!(
                "benchmark failed: {run_error:#}; agent shutdown also failed: {shutdown_error:#}"
            )),
        };
    }
    drop(agent);
    let completed = completed?;
    require_closed_event_stream(&mut events).await?;
    let report = finish_report(completed, args)?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

async fn measured_generation(
    agent: &Nanocodex,
    events: &mut AgentEvents,
    prompt: &str,
) -> Result<CompletedTurn> {
    let started = start_prompt(agent, prompt).await?;
    let submitted_ns = started.submitted_ns;
    let accepted_ns = started.accepted_ns;
    let ((result, result_completed_ns), events) =
        tokio::try_join!(await_result(started.turn), drain_events(events))?;
    Ok(CompletedTurn {
        result,
        result_completed_ns,
        submitted_ns,
        accepted_ns,
        events,
    })
}

async fn start_prompt(agent: &Nanocodex, prompt: &str) -> Result<StartedTurn> {
    let submitted_ns = monotonic_now_ns();
    let turn = agent.prompt(prompt).await?;
    Ok(StartedTurn {
        turn,
        submitted_ns,
        accepted_ns: monotonic_now_ns(),
    })
}

async fn await_result(turn: Turn) -> Result<(TurnResult, u64)> {
    let result = turn.result().await?;
    Ok((result, monotonic_now_ns()))
}

async fn drain_events(events: &mut AgentEvents) -> Result<ObservedEvents> {
    let mut observed = ObservedEvents::default();
    let mut next_sequence = 1_u64;
    while let Some(timed) = events.recv_timed().await {
        let received_ns = monotonic_now_ns();
        let event = timed.event;
        if event.seq != next_sequence {
            bail!(
                "agent event sequence is not contiguous: expected {next_sequence}, got {}",
                event.seq
            );
        }
        next_sequence = next_sequence
            .checked_add(1)
            .ok_or_else(|| eyre!("agent event sequence overflowed"))?;
        observed.first_sequence.get_or_insert(event.seq);
        observed.last_sequence = Some(event.seq);
        observed.event_count += 1;

        match event.kind {
            AgentEventKind::AssistantDelta => {
                let delta: AssistantDelta = event.decode_payload()?;
                if delta.model_call_index != 1 {
                    bail!(
                        "assistant delta belonged to model call {}, expected 1",
                        delta.model_call_index
                    );
                }
                observed.assistant_delta_count += 1;
                observed.assistant_text.push_str(&delta.text);
                if !delta.text.is_empty() && observed.first_delta_emitted_ns.is_none() {
                    observed.first_delta_emitted_ns = Some(timed.timing.emitted_ns);
                    observed.first_delta_received_ns = Some(received_ns);
                    observed.first_delta_source_received_ns = timed.timing.source_received_ns;
                }
            }
            AgentEventKind::AssistantMessage => {
                let message: nanocodex::agent::events::AssistantMessage = event.decode_payload()?;
                if message.model_call_index != 1 {
                    bail!(
                        "assistant message belonged to model call {}, expected 1",
                        message.model_call_index
                    );
                }
                observed.assistant_messages.push(message.text);
            }
            AgentEventKind::RunStarted => {
                observed.run_started.push(event.decode_payload()?);
            }
            AgentEventKind::ModelCallStarted => {
                observed.model_calls_started.push(event.decode_payload()?);
            }
            AgentEventKind::ModelCallCompleted => {
                let completed: ModelCallCompleted = event.decode_payload()?;
                if completed.tool_calls != 0 {
                    bail!(
                        "latency generation requested {} tool calls",
                        completed.tool_calls
                    );
                }
                observed.model_calls_completed.push(completed);
            }
            AgentEventKind::ToolCall | AgentEventKind::ToolResult => {
                bail!("latency generation emitted an unexpected tool event");
            }
            AgentEventKind::ModelCallFailed => {
                bail!(
                    "latency generation emitted model.call.failed: {}",
                    event.payload.get()
                );
            }
            AgentEventKind::ModelCompactionStarted
            | AgentEventKind::ModelCompactionCompleted
            | AgentEventKind::ModelCompactionFailed => {
                bail!("latency generation unexpectedly entered context compaction");
            }
            AgentEventKind::ModelWarmupFailed => {
                bail!(
                    "latency generation emitted model.warmup.failed: {}",
                    event.payload.get()
                );
            }
            AgentEventKind::RunError | AgentEventKind::RunFailed => {
                bail!(
                    "latency generation emitted {:?}: {}",
                    event.kind,
                    event.payload.get()
                );
            }
            AgentEventKind::RunCompleted => {
                if observed.run_completed.is_some() {
                    bail!("latency generation emitted run.completed more than once");
                }
                observed.run_completed = Some(event.decode_payload()?);
                return Ok(observed);
            }
            AgentEventKind::ApiEvent
            | AgentEventKind::ReasoningSummaryDelta
            | AgentEventKind::RunSteered
            | AgentEventKind::ModelWarmupStarted
            | AgentEventKind::ModelWarmupCompleted
            | AgentEventKind::ModelAttemptStarted
            | AgentEventKind::ModelAttemptFailed
            | AgentEventKind::ModelAttemptRetrying
            | AgentEventKind::ModelConnectionStarted
            | AgentEventKind::ModelConnectionCompleted
            | AgentEventKind::ModelConnectionFailed => {}
        }
    }
    Err(eyre!("agent event stream closed before run.completed"))
}

async fn require_closed_event_stream(events: &mut AgentEvents) -> Result<()> {
    match timeout(EVENT_STREAM_CLOSE_TIMEOUT, events.recv_timed()).await {
        Ok(None) => Ok(()),
        Ok(Some(timed)) => bail!(
            "agent emitted event sequence {} after run.completed and shutdown",
            timed.event.seq
        ),
        Err(_) => bail!("agent event stream remained open after shutdown"),
    }
}

fn finish_report(completed: CompletedTurn, args: Args) -> Result<BenchmarkReport> {
    if completed.result.final_message() != args.expected {
        bail!(
            "unexpected final output: expected {:?}, got {:?}",
            args.expected,
            completed.result.final_message()
        );
    }
    if completed.events.assistant_text != args.expected {
        bail!(
            "concatenated assistant.delta output differs: expected {:?}, got {:?}",
            args.expected,
            completed.events.assistant_text
        );
    }
    let [assistant_message] = completed.events.assistant_messages.as_slice() else {
        bail!(
            "latency generation must emit one complete assistant message, observed {}",
            completed.events.assistant_messages.len()
        );
    };
    if assistant_message != &args.expected {
        bail!(
            "complete assistant message differs: expected {:?}, got {:?}",
            args.expected,
            assistant_message
        );
    }
    let [run_started] = completed.events.run_started.as_slice() else {
        bail!(
            "latency generation must emit one run.started, observed {}",
            completed.events.run_started.len()
        );
    };
    validate_run_started(run_started, &args)?;
    let [call_started] = completed.events.model_calls_started.as_slice() else {
        bail!(
            "latency generation must start one model call, observed {}",
            completed.events.model_calls_started.len()
        );
    };
    validate_model_call_started(call_started)?;
    let [call] = completed.events.model_calls_completed.as_slice() else {
        bail!(
            "latency generation must complete one model call, observed {}",
            completed.events.model_calls_completed.len()
        );
    };
    validate_model_call(call, args.transport)?;
    let terminal = completed
        .events
        .run_completed
        .as_ref()
        .ok_or_else(|| eyre!("latency generation did not emit run.completed"))?;
    validate_terminal(terminal, &args)?;

    let first_delta_emitted_ns = completed
        .events
        .first_delta_emitted_ns
        .ok_or_else(|| eyre!("latency generation completed without assistant.delta"))?;
    let first_delta_received_ns = completed
        .events
        .first_delta_received_ns
        .ok_or_else(|| eyre!("latency generation did not receive assistant.delta"))?;
    let first_sequence = completed
        .events
        .first_sequence
        .ok_or_else(|| eyre!("latency generation emitted no events"))?;
    let last_sequence = completed
        .events
        .last_sequence
        .ok_or_else(|| eyre!("latency generation emitted no events"))?;

    let timing_ns = TimingMeasurement {
        prompt_submit_to_acceptance: elapsed_ns(
            completed.accepted_ns,
            completed.submitted_ns,
            "prompt acceptance",
        )?,
        prompt_submit_to_first_assistant_delta_emitted: elapsed_ns(
            first_delta_emitted_ns,
            completed.submitted_ns,
            "first assistant.delta emission from prompt submission",
        )?,
        prompt_acceptance_to_first_assistant_delta_emitted: elapsed_ns(
            first_delta_emitted_ns,
            completed.accepted_ns,
            "first assistant.delta emission from prompt acceptance",
        )?,
        prompt_submit_to_first_assistant_delta_received: elapsed_ns(
            first_delta_received_ns,
            completed.submitted_ns,
            "first assistant.delta receipt from prompt submission",
        )?,
        prompt_acceptance_to_first_assistant_delta_received: elapsed_ns(
            first_delta_received_ns,
            completed.accepted_ns,
            "first assistant.delta receipt from prompt acceptance",
        )?,
        assistant_delta_emit_to_receive: elapsed_ns(
            first_delta_received_ns,
            first_delta_emitted_ns,
            "first assistant.delta delivery",
        )?,
        provider_source_to_assistant_delta_emit: completed
            .events
            .first_delta_source_received_ns
            .map(|source_ns| {
                elapsed_ns(
                    first_delta_emitted_ns,
                    source_ns,
                    "provider source to assistant.delta emission",
                )
            })
            .transpose()?,
        prompt_submit_to_result_completion: elapsed_ns(
            completed.result_completed_ns,
            completed.submitted_ns,
            "result completion from prompt submission",
        )?,
        prompt_acceptance_to_result_completion: elapsed_ns(
            completed.result_completed_ns,
            completed.accepted_ns,
            "result completion from prompt acceptance",
        )?,
    };
    let usage = UsageMeasurement::from_provider(call.usage.as_ref());
    let turn_usage = TurnUsageMeasurement::from_turn(completed.result.usage());
    Ok(BenchmarkReport {
        schema_version: 1,
        benchmark: "paired_fx_model_latency",
        provenance: Provenance {
            implementation: "nanocodex",
            source_commit: args.source_commit,
            model: MODEL,
            thinking: Thinking::Low.as_str(),
            fast_mode: false,
            workspace: args.cwd.display().to_string(),
            instructions_fnv1a64: fnv1a64(args.instructions.as_bytes()),
            prompt_fnv1a64: fnv1a64(args.prompt.as_bytes()),
            expected_fnv1a64: fnv1a64(args.expected.as_bytes()),
            prompt_cache_key_fnv1a64: fnv1a64(PROMPT_CACHE_KEY.as_bytes()),
        },
        transport: args.transport.as_str(),
        timing_ns,
        model_call: ModelCallMeasurement {
            call_index: call.call_index,
            attempt: call.attempt,
            connection_generation: call.connection_generation,
            status: call.status.clone(),
            duration_ns: call.duration_ns,
            time_to_first_event_ns: call.time_to_first_event_ns,
            time_to_first_output_ns: call.time_to_first_output_ns,
            tool_calls: call.tool_calls,
        },
        usage,
        turn_usage,
        events: EventMeasurement {
            count: completed.events.event_count,
            first_sequence,
            last_sequence,
            assistant_delta_count: completed.events.assistant_delta_count,
            sequences_contiguous: true,
        },
        verified: Verification {
            final_output: true,
            assistant_deltas: true,
            one_model_call: true,
            zero_tool_calls: true,
            run_completed: true,
            clean_shutdown: true,
            auth_refresh_disabled: true,
        },
    })
}

fn validate_run_started(started: &RunStarted, args: &Args) -> Result<()> {
    if started.model != MODEL
        || started.effort != Thinking::Low.as_str()
        || started.transport != args.transport.event_name()
        || started.instruction_bytes != args.prompt.len()
    {
        bail!("run.started does not match the fixed benchmark configuration");
    }
    Ok(())
}

fn validate_model_call_started(started: &ModelCallStarted) -> Result<()> {
    if started.call_index != 1 || started.model != MODEL || started.effort != Thinking::Low.as_str()
    {
        bail!("model.call.started does not match the fixed benchmark generation");
    }
    Ok(())
}

fn validate_model_call(call: &ModelCallCompleted, transport: Transport) -> Result<()> {
    let expected_connection_generation = match transport {
        Transport::Https => 0,
        Transport::WebSocket => 1,
    };
    if call.call_index != 1
        || call.attempt != 1
        || call.connection_generation != expected_connection_generation
        || call.model != MODEL
        || call.status != "completed"
    {
        bail!("model.call.completed does not describe one completed {MODEL} generation");
    }
    if call.tool_calls != 0 {
        bail!(
            "latency generation completed {} tool calls",
            call.tool_calls
        );
    }
    Ok(())
}

fn validate_terminal(terminal: &RunTerminal, args: &Args) -> Result<()> {
    if terminal.status != RunStatus::Completed
        || terminal.model != MODEL
        || terminal.effort != Thinking::Low.as_str()
        || terminal.transport != args.transport.event_name()
        || terminal.metrics.model_calls != 1
        || terminal.metrics.tool_calls != 0
    {
        bail!("run.completed does not match the fixed one-generation benchmark");
    }
    Ok(())
}

impl UsageMeasurement {
    fn from_provider(usage: Option<&Usage>) -> Self {
        Self {
            reported: usage.is_some(),
            input_tokens: usage.map_or(0, |usage| usage.input_tokens),
            cached_input_tokens: usage
                .and_then(|usage| usage.input_tokens_details.as_ref())
                .map_or(0, |details| details.cached_tokens),
            cache_write_input_tokens: usage
                .and_then(|usage| usage.input_tokens_details.as_ref())
                .map_or(0, |details| details.cache_write_tokens),
            output_tokens: usage.map_or(0, |usage| usage.output_tokens),
            reasoning_output_tokens: usage
                .and_then(|usage| usage.output_tokens_details.as_ref())
                .map_or(0, |details| details.reasoning_tokens),
            total_tokens: usage.map_or(0, |usage| usage.total_tokens),
        }
    }
}

impl TurnUsageMeasurement {
    const fn from_turn(usage: &TurnUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens(),
            cached_input_tokens: usage.cached_input_tokens(),
            cache_write_input_tokens: usage.cache_write_input_tokens(),
            output_tokens: usage.output_tokens(),
            reasoning_output_tokens: usage.reasoning_output_tokens(),
            total_tokens: usage.total_tokens(),
        }
    }
}

fn elapsed_ns(later: u64, earlier: u64, label: &str) -> Result<u64> {
    later
        .checked_sub(earlier)
        .ok_or_else(|| eyre!("monotonic timestamp moved backwards while measuring {label}"))
}

fn parse_args() -> Result<Args> {
    let mut cwd = None;
    let mut auth_file = None;
    let mut api_base_url = None;
    let mut websocket_url = None;
    let mut transport = None;
    let mut instructions = None;
    let mut prompt = None;
    let mut expected = None;
    let mut source_commit = None;
    let mut arguments = env::args().skip(1);
    while let Some(flag) = arguments.next() {
        let mut value = || {
            arguments
                .next()
                .ok_or_else(|| eyre!("{flag} requires a value"))
        };
        match flag.as_str() {
            "--cwd" => set_once(&mut cwd, PathBuf::from(value()?), &flag)?,
            "--auth-file" => set_once(&mut auth_file, PathBuf::from(value()?), &flag)?,
            "--api-base-url" => set_once(&mut api_base_url, value()?, &flag)?,
            "--websocket-url" => set_once(&mut websocket_url, value()?, &flag)?,
            "--transport" => {
                let parsed = match value()?.as_str() {
                    "websocket" => Transport::WebSocket,
                    "https" => Transport::Https,
                    other => {
                        bail!("invalid --transport value {other:?}; expected websocket or https")
                    }
                };
                set_once(&mut transport, parsed, &flag)?;
            }
            "--instructions" => set_once(&mut instructions, value()?, &flag)?,
            "--prompt" => set_once(&mut prompt, value()?, &flag)?,
            "--expected" => set_once(&mut expected, value()?, &flag)?,
            "--source-commit" => set_once(&mut source_commit, value()?, &flag)?,
            _ => bail!("unknown argument {flag:?}"),
        }
    }

    let transport = transport.unwrap_or(Transport::Https);
    if matches!(transport, Transport::Https) && websocket_url.is_some() {
        bail!("--websocket-url requires --transport websocket");
    }
    if matches!(transport, Transport::WebSocket) && websocket_url.is_none() {
        bail!("--transport websocket requires an explicit loopback --websocket-url");
    }
    let cwd = required(cwd, "--cwd")?
        .canonicalize()
        .wrap_err("failed to canonicalize --cwd")?;
    if !cwd.is_dir() {
        bail!("--cwd must name a directory");
    }
    let auth_file = required(auth_file, "--auth-file")?
        .canonicalize()
        .wrap_err("failed to canonicalize --auth-file")?;
    if !auth_file.is_file() {
        bail!("--auth-file must name a file");
    }
    let api_base_url = require_loopback_url(
        required_nonempty(api_base_url, "--api-base-url")?,
        "--api-base-url",
        "http",
    )?;
    let websocket_url = optional_nonempty(websocket_url, "--websocket-url")?
        .map(|value| require_loopback_url(value, "--websocket-url", "ws"))
        .transpose()?;
    Ok(Args {
        cwd,
        auth_file,
        api_base_url,
        websocket_url,
        transport,
        instructions: required_nonempty(instructions, "--instructions")?,
        prompt: required_nonempty(prompt, "--prompt")?,
        expected: required_nonempty(expected, "--expected")?,
        source_commit: required_nonempty(source_commit, "--source-commit")?,
    })
}

fn set_once<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<()> {
    if slot.replace(value).is_some() {
        bail!("{flag} may be supplied only once");
    }
    Ok(())
}

fn required<T>(value: Option<T>, flag: &str) -> Result<T> {
    value.ok_or_else(|| eyre!("missing required argument {flag}"))
}

fn required_nonempty(value: Option<String>, flag: &str) -> Result<String> {
    let value = required(value, flag)?;
    if value.trim().is_empty() {
        bail!("{flag} must not be empty");
    }
    Ok(value)
}

fn optional_nonempty(value: Option<String>, flag: &str) -> Result<Option<String>> {
    value
        .map(|value| {
            if value.trim().is_empty() {
                bail!("{flag} must not be empty");
            }
            Ok(value)
        })
        .transpose()
}

fn require_loopback_url(value: String, flag: &str, scheme: &str) -> Result<String> {
    let parsed =
        reqwest::Url::parse(&value).wrap_err_with(|| format!("{flag} must be a valid URL"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| eyre!("{flag} must include a host"))?;
    let loopback = host
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback());
    if parsed.scheme() != scheme
        || !loopback
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        bail!("{flag} must be a credential-free {scheme} URL on a numeric loopback IP address");
    }
    Ok(value)
}

fn fnv1a64(bytes: &[u8]) -> String {
    let mut digest = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        digest ^= u64::from(*byte);
        digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{digest:016x}")
}
