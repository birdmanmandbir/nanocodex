use std::{
    collections::BTreeMap,
    env,
    error::Error,
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
    process::Stdio,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use nanocodex::{Model, Nanocodex, OpenAi, Thinking, Tools, TurnUsage};
use nanocodex_examples::eval_support as support;
use nanocodex_rlm::{HarnessSnapshot, LaunchSnapshot, PromptPack, RlmPolicy, RlmRuntime, RlmUsage};
use reqwest::{Client, Method, StatusCode, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::{process::Command, sync::Semaphore, task::JoinSet, time::timeout};

const API_BASE_URL: &str = "https://three.arcprize.org";
const ARC_COOKIES_ENV: &str = "NANOCODEX_ARC_COOKIES";
const MAX_ANIMATION_FRAMES: usize = 7;
const MODEL_RETRIES_PER_FRAME: usize = 3;
const DEFAULT_TURN_TIMEOUT: Duration = Duration::from_secs(600);
const CONTROLLER_INSTRUCTIONS: &str = r#"You are the decision-making root for an official ARC-AGI-3 game. You receive one ARC_FRAME message at a time. Learn the unknown controls, dynamics, objects, and goal from visible transitions, preserve useful knowledge across levels, and minimize environment actions.

For each ARC_FRAME message, reason and use any available Code Mode orchestration internally. Your final response must contain only one JSON object in one of these exact forms:
{"actions":[{"action_type":"ACTION1"}]}
{"actions":[{"action_type":"ACTION6","x":12,"y":34}]}
{"actions":[{"action_type":"RESET"}]}
Choose only an action listed in that frame. ACTION6 uses zero-based x=column and y=row coordinates. Do not wrap the JSON in Markdown and do not mention any other action in the final response.

An ARC_TRAINING_REVIEW message arrives only after the scorecard is closed. It requests harness reflection rather than an environment action, so its final response may be ordinary concise text. Never use internet search, repository history, or memorized public-game solutions; derive decisions only from this run's observations and retained harness evidence."#;

type AnyError = Box<dyn Error + Send + Sync>;
type Result<T> = std::result::Result<T, AnyError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Baseline,
    Rlm,
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "baseline" => Ok(Self::Baseline),
            "rlm" => Ok(Self::Rlm),
            _ => Err(format!("invalid mode {value:?}; expected baseline or rlm")),
        }
    }
}

#[derive(Clone, Debug)]
struct Config {
    game: String,
    mode: Mode,
    thinking: Thinking,
    harness: PathBuf,
    output: Option<PathBuf>,
    action_multiplier: f64,
    max_actions: Option<u64>,
    turn_timeout: Duration,
    allow_refinement: bool,
    refine_every_actions: Option<u64>,
    concurrency: usize,
    scorecard_id: Option<String>,
    suite_game_limit: Option<usize>,
}

impl Config {
    fn parse() -> Result<Self> {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut config = Self {
            game: "ls20".to_owned(),
            mode: Mode::Rlm,
            thinking: Thinking::High,
            harness: workspace.join("rlm/arc-agi-3.harness.toml"),
            output: None,
            action_multiplier: 5.0,
            max_actions: None,
            turn_timeout: DEFAULT_TURN_TIMEOUT,
            allow_refinement: false,
            refine_every_actions: None,
            concurrency: 8,
            scorecard_id: None,
            suite_game_limit: None,
        };
        let mut args = env::args().skip(1);
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--game" => config.game = required_value(&mut args, "--game")?,
                "--mode" => {
                    config.mode = required_value(&mut args, "--mode")?
                        .parse()
                        .map_err(invalid)?;
                }
                "--thinking" => {
                    config.thinking = required_value(&mut args, "--thinking")?
                        .parse()
                        .map_err(invalid)?;
                }
                "--harness" => {
                    config.harness = PathBuf::from(required_value(&mut args, "--harness")?);
                }
                "--output" => {
                    config.output = Some(PathBuf::from(required_value(&mut args, "--output")?));
                }
                "--action-multiplier" => {
                    config.action_multiplier = required_value(&mut args, "--action-multiplier")?
                        .parse()
                        .map_err(|error| invalid(format!("invalid action multiplier: {error}")))?;
                }
                "--max-actions" => {
                    config.max_actions = Some(
                        required_value(&mut args, "--max-actions")?
                            .parse()
                            .map_err(|error| invalid(format!("invalid max actions: {error}")))?,
                    );
                }
                "--turn-timeout-seconds" => {
                    let seconds = required_value(&mut args, "--turn-timeout-seconds")?
                        .parse()
                        .map_err(|error| invalid(format!("invalid turn timeout: {error}")))?;
                    config.turn_timeout = Duration::from_secs(seconds);
                }
                "--allow-refinement" => config.allow_refinement = true,
                "--refine-every-actions" => {
                    config.refine_every_actions = Some(
                        required_value(&mut args, "--refine-every-actions")?
                            .parse()
                            .map_err(|error| {
                                invalid(format!("invalid refinement action interval: {error}"))
                            })?,
                    );
                }
                "--concurrency" => {
                    config.concurrency = required_value(&mut args, "--concurrency")?
                        .parse()
                        .map_err(|error| invalid(format!("invalid concurrency: {error}")))?;
                }
                "--scorecard-id" => {
                    config.scorecard_id = Some(required_value(&mut args, "--scorecard-id")?);
                }
                "--suite-game-limit" => {
                    config.suite_game_limit = Some(
                        required_value(&mut args, "--suite-game-limit")?
                            .parse()
                            .map_err(|error| {
                                invalid(format!("invalid suite game limit: {error}"))
                            })?,
                    );
                }
                "--help" | "-h" => {
                    println!(
                        "usage: eval-arc-agi-3-rlm [--game ls20] [--mode baseline|rlm] \
                         [--thinking low|medium|high|xhigh|max] [--harness PATH] \
                         [--output DIR] [--action-multiplier 5] [--max-actions N] \
                         [--turn-timeout-seconds 600] [--allow-refinement] \
                         [--refine-every-actions N] [--concurrency 8] \
                         [--suite-game-limit N]"
                    );
                    std::process::exit(0);
                }
                _ => return Err(invalid(format!("unknown argument {argument:?}"))),
            }
        }
        if !config.action_multiplier.is_finite() || config.action_multiplier <= 0.0 {
            return Err(invalid(
                "action multiplier must be finite and greater than zero",
            ));
        }
        if config.max_actions == Some(0) {
            return Err(invalid("max actions must be greater than zero"));
        }
        if config.turn_timeout.is_zero() {
            return Err(invalid("turn timeout must be greater than zero"));
        }
        if config.concurrency == 0 {
            return Err(invalid("concurrency must be greater than zero"));
        }
        if config.mode == Mode::Baseline && config.allow_refinement {
            return Err(invalid("baseline mode cannot refine an RLM harness"));
        }
        if config.refine_every_actions == Some(0) {
            return Err(invalid(
                "refinement action interval must be greater than zero",
            ));
        }
        if config.refine_every_actions.is_some() && !config.allow_refinement {
            return Err(invalid(
                "--refine-every-actions requires --allow-refinement",
            ));
        }
        if config.game.eq_ignore_ascii_case("all") && config.allow_refinement {
            return Err(invalid(
                "the complete suite requires a frozen harness; refine in a separate training run",
            ));
        }
        if config.game.eq_ignore_ascii_case("all") && config.scorecard_id.is_some() {
            return Err(invalid("the complete suite must own its scorecard"));
        }
        if config.suite_game_limit == Some(0) {
            return Err(invalid("suite game limit must be greater than zero"));
        }
        if config.suite_game_limit.is_some() && !config.game.eq_ignore_ascii_case("all") {
            return Err(invalid("--suite-game-limit requires --game all"));
        }
        Ok(config)
    }
}

fn required_value(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| invalid(format!("{name} requires a value")))
}

fn invalid(message: impl Into<String>) -> AnyError {
    Box::new(io::Error::new(io::ErrorKind::InvalidInput, message.into()))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GameMetadata {
    game_id: String,
    title: String,
    baseline_actions: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Frame {
    game_id: String,
    frame: Vec<Vec<Vec<u8>>>,
    state: String,
    levels_completed: u64,
    win_levels: u64,
    guid: String,
    available_actions: Vec<u8>,
    #[serde(default)]
    full_reset: bool,
    #[serde(default)]
    action_input: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
struct AnonKey {
    api_key: String,
}

#[derive(Clone, Debug, Deserialize)]
struct OpenScorecard {
    card_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ArcAction {
    action_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    x: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    y: Option<u8>,
}

#[derive(Debug, Serialize)]
struct ModelAttempt {
    output: String,
    usage: TurnUsage,
    parse_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ActionRecord {
    index: u64,
    source: &'static str,
    decision_duration_ms: u128,
    before: Frame,
    action: ArcAction,
    model_attempts: Vec<ModelAttempt>,
    after: Frame,
}

#[derive(Debug, Serialize)]
struct RunEvidence {
    schema_version: u32,
    benchmark: &'static str,
    protocol: &'static str,
    mode: Mode,
    model: &'static str,
    thinking: String,
    game: GameMetadata,
    action_budget_multiplier: f64,
    development_action_cap: Option<u64>,
    prompt_cache_key: String,
    immutable_prompt_digest: Option<String>,
    harness_revision_start: Option<u64>,
    harness_revision_final: Option<u64>,
    harness_refinement_enabled: bool,
    root_session_id: String,
    started_unix_ms: u128,
    completed_unix_ms: Option<u128>,
    actions: Vec<ActionRecord>,
    exit_reason: Option<String>,
    scorecard: Option<Value>,
    rlm_usage: Option<RlmUsage>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct SuiteEvidence {
    schema_version: u32,
    benchmark: &'static str,
    protocol: &'static str,
    mode: Mode,
    model: &'static str,
    thinking: String,
    action_budget_multiplier: f64,
    development_action_cap: Option<u64>,
    development_game_limit: Option<usize>,
    concurrency: usize,
    scorecard_id: String,
    scorecard_url: String,
    harness: Option<PathBuf>,
    started_unix_ms: u128,
    completed_unix_ms: Option<u128>,
    games: Vec<SuiteGameEvidence>,
    scorecard: Option<Value>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct SuiteGameEvidence {
    game_id: String,
    output: PathBuf,
    status: &'static str,
    exit_code: Option<i32>,
    error: Option<String>,
}

#[derive(Clone)]
struct ArcClient {
    http: Client,
    cookies: BTreeMap<String, String>,
    api_key: String,
    card_id: String,
}

impl ArcClient {
    async fn connect() -> Result<Self> {
        let mut client = Self {
            http: Client::builder().timeout(Duration::from_secs(30)).build()?,
            cookies: env::var(ARC_COOKIES_ENV)
                .ok()
                .map(|cookies| serde_json::from_str(&cookies))
                .transpose()?
                .unwrap_or_default(),
            api_key: env::var("ARC_API_KEY").unwrap_or_default(),
            card_id: String::new(),
        };
        if client.api_key.is_empty() {
            let key: AnonKey = client
                .request(Method::GET, "/api/games/anonkey", None)
                .await?;
            client.api_key = key.api_key;
        }
        Ok(client)
    }

    async fn open_scorecard(&mut self, mode: Mode) -> Result<()> {
        let opened: OpenScorecard = self
            .request(
                Method::POST,
                "/api/scorecard/open",
                Some(&json!({"tags": ["nanocodex", "arc-agi-3", mode_name(mode)]})),
            )
            .await?;
        self.card_id = opened.card_id;
        Ok(())
    }

    fn use_scorecard(&mut self, card_id: String) {
        self.card_id = card_id;
    }

    fn card_id(&self) -> &str {
        &self.card_id
    }

    fn api_key(&self) -> &str {
        &self.api_key
    }

    fn encoded_cookies(&self) -> Result<String> {
        Ok(serde_json::to_string(&self.cookies)?)
    }

    async fn games(&mut self) -> Result<Vec<GameMetadata>> {
        self.request(Method::GET, "/api/games", None).await
    }

    async fn reset_initial(&mut self, game_id: &str) -> Result<Frame> {
        self.request(
            Method::POST,
            "/api/cmd/RESET",
            Some(&json!({"card_id": self.card_id, "game_id": game_id})),
        )
        .await
    }

    async fn reset(&mut self, game_id: &str, guid: &str) -> Result<Frame> {
        self.request(
            Method::POST,
            "/api/cmd/RESET",
            Some(&json!({"game_id": game_id, "guid": guid})),
        )
        .await
    }

    async fn act(&mut self, game_id: &str, guid: &str, action: &ArcAction) -> Result<Frame> {
        let mut body = json!({"game_id": game_id, "guid": guid});
        if let Some(x) = action.x {
            body["x"] = json!(x);
        }
        if let Some(y) = action.y {
            body["y"] = json!(y);
        }
        self.request(
            Method::POST,
            &format!("/api/cmd/{}", action.action_type),
            Some(&body),
        )
        .await
    }

    async fn close(&mut self) -> Result<Value> {
        self.request(
            Method::POST,
            "/api/scorecard/close",
            Some(&json!({"card_id": self.card_id})),
        )
        .await
    }

    async fn request<T: DeserializeOwned>(
        &mut self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T> {
        let attempts = if method == Method::GET { 3 } else { 1 };
        let mut last_error = None;
        for attempt in 0..attempts {
            match self.request_once(method.clone(), path, body).await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    last_error = Some(error);
                    if attempt + 1 < attempts {
                        tokio::time::sleep(Duration::from_millis(500 * (1_u64 << attempt))).await;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| invalid(format!("ARC request failed for {path}"))))
    }

    async fn request_once<T: DeserializeOwned>(
        &mut self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T> {
        let mut request = self.http.request(method, format!("{API_BASE_URL}{path}"));
        if !self.api_key.is_empty() {
            request = request.header("X-API-Key", &self.api_key);
        }
        if !self.cookies.is_empty() {
            let cookie = self
                .cookies
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; ");
            request = request.header(header::COOKIE, cookie);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?;
        let status = response.status();
        for value in response.headers().get_all(header::SET_COOKIE).iter() {
            if let Ok(value) = value.to_str()
                && let Some(pair) = value.split(';').next()
                && let Some((name, value)) = pair.split_once('=')
            {
                self.cookies.insert(name.to_owned(), value.to_owned());
            }
        }
        let bytes = response.bytes().await?;
        if !status.is_success() {
            return Err(http_error(path, status, &bytes));
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
}

fn http_error(path: &str, status: StatusCode, body: &[u8]) -> AnyError {
    let body = String::from_utf8_lossy(body);
    let body = body.chars().take(2_000).collect::<String>();
    Box::new(io::Error::other(format!(
        "official ARC API {path} returned {status}: {body}"
    )))
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let config = Config::parse()?;
    if config.game.eq_ignore_ascii_case("all") {
        return run_suite(config).await;
    }
    run_game(config).await
}

async fn run_game(config: Config) -> Result<()> {
    let prompt_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../crates/experimental/nanocodex-rlm/prompts");
    let (runtime, immutable_prompt_digest, harness_revision_start, cache_key) =
        if config.mode == Mode::Rlm {
            let prompts = PromptPack::load(prompt_root)?;
            let digest = prompts.digest().to_owned();
            let harness = HarnessSnapshot::load(&config.harness)?;
            let revision = harness.revision();
            let launch = LaunchSnapshot::new(prompts, harness);
            let policy = RlmPolicy::new(4, 1)?.with_harness_refinement(config.allow_refinement);
            (
                Some(RlmRuntime::with_policy(launch, policy)),
                Some(digest.clone()),
                Some(revision),
                format!("nanocodex-arc-agi-3-rlm-v1-{digest}"),
            )
        } else {
            (
                None,
                None,
                None,
                "nanocodex-arc-agi-3-baseline-v1".to_owned(),
            )
        };

    let empty_tools = Tools::builder().without_defaults().build()?;
    let builder = Nanocodex::builder(OpenAi::new(support::auth()?)?)
        .model(Model::Sol)
        .thinking(config.thinking)
        .instructions(CONTROLLER_INSTRUCTIONS)
        .prompt_cache_key(&cache_key);
    let builder = match &runtime {
        Some(runtime) => runtime.agent_builder(builder, empty_tools),
        None => builder.tools(empty_tools),
    };
    let (agent, events) = builder.build()?;
    let root_session_id = agent.session_id().to_string();

    let mut arc = ArcClient::connect().await?;
    let games = arc.games().await?;
    let game = resolve_game(&games, &config.game)?.clone();
    let output = config.output.clone().unwrap_or_else(|| {
        PathBuf::from(".nanocodex/evals/arc-agi-3-rlm").join(format!(
            "{}-{}-{}",
            game.game_id,
            mode_name(config.mode),
            unix_ms()
        ))
    });
    prepare_output(&output).await?;
    if config.mode == Mode::Rlm {
        tokio::fs::copy(&config.harness, output.join("harness-start.toml")).await?;
    }
    let mut evidence = RunEvidence {
        schema_version: 1,
        benchmark: "arc-agi-3",
        protocol: "official-live-per-frame-retained-nanocodex-session",
        mode: config.mode,
        model: Model::Sol.as_str(),
        thinking: config.thinking.to_string(),
        game: game.clone(),
        action_budget_multiplier: config.action_multiplier,
        development_action_cap: config.max_actions,
        prompt_cache_key: cache_key,
        immutable_prompt_digest,
        harness_revision_start,
        harness_revision_final: harness_revision_start,
        harness_refinement_enabled: config.allow_refinement,
        root_session_id: root_session_id.clone(),
        started_unix_ms: unix_ms(),
        completed_unix_ms: None,
        actions: Vec::new(),
        exit_reason: None,
        scorecard: None,
        rlm_usage: None,
        error: None,
    };
    write_evidence(&output, &evidence).await?;
    let owns_scorecard = config.scorecard_id.is_none();
    match &config.scorecard_id {
        Some(card_id) => arc.use_scorecard(card_id.clone()),
        None => arc.open_scorecard(config.mode).await?,
    }
    let frame = match arc.reset_initial(&game.game_id).await {
        Ok(frame) => frame,
        Err(error) => {
            let cleanup = if owns_scorecard {
                arc.close().await.map(Some)
            } else {
                Ok(None)
            };
            let _ = agent.shutdown().await;
            if let Some(runtime) = &runtime {
                runtime.shutdown().await;
            }
            if let Err(cleanup) = cleanup {
                return Err(invalid(format!(
                    "initial ARC reset failed: {error}; scorecard cleanup failed: {cleanup}"
                )));
            }
            return Err(error);
        }
    };
    let event_file = fs::File::create(output.join("root-events.jsonl"))?;
    let event_task = tokio::spawn(events.write_jsonl(io::BufWriter::new(event_file)));

    let play_result = {
        let play = play_game(&config, &mut arc, &agent, frame, &mut evidence, &output);
        tokio::pin!(play);
        tokio::select! {
            result = &mut play => result,
            signal = tokio::signal::ctrl_c() => {
                signal?;
                Err(invalid("interrupted"))
            }
        }
    };
    if let Err(error) = &play_result {
        evidence.error = Some(error.to_string());
        evidence
            .exit_reason
            .get_or_insert_with(|| "AGENT_ERROR".to_owned());
    }

    let close_result = if owns_scorecard {
        arc.close().await.map(Some)
    } else {
        Ok(None)
    };
    match close_result {
        Ok(scorecard) => evidence.scorecard = scorecard,
        Err(error) => {
            let message = format!("scorecard cleanup failed: {error}");
            evidence.error = Some(match evidence.error.take() {
                Some(primary) => format!("{primary}; {message}"),
                None => message,
            });
        }
    }

    if config.allow_refinement
        && let (Some(runtime), Some(scorecard)) = (&runtime, &evidence.scorecard)
    {
        match serde_json::to_string(scorecard) {
            Ok(scorecard) => {
                let review = format!(
                    "ARC_TRAINING_REVIEW\nThe official scorecard is closed. Review this run and call the current refine_harness function with one concise, trajectory-grounded reusable failure or success observation. Apply at most one minimal harness edit, wait for the refiner, and report what changed. Game: {}. Actions: {}. Exit: {}. Scorecard: {}",
                    evidence.game.game_id,
                    evidence.actions.len(),
                    evidence.exit_reason.as_deref().unwrap_or("UNKNOWN"),
                    scorecard
                );
                match agent.prompt(review).await {
                    Ok(turn) => match timeout(config.turn_timeout, turn.result()).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => {
                            append_error(&mut evidence, format!("training review failed: {error}"))
                        }
                        Err(_) => {
                            append_error(&mut evidence, "training review timed out".to_owned())
                        }
                    },
                    Err(error) => append_error(
                        &mut evidence,
                        format!("training review was not accepted: {error}"),
                    ),
                }
            }
            Err(error) => append_error(
                &mut evidence,
                format!("training scorecard encoding failed: {error}"),
            ),
        }
        evidence.harness_revision_final = Some(runtime.harness().await.revision());
        if let Err(error) =
            tokio::fs::copy(&config.harness, output.join("harness-final.toml")).await
        {
            append_error(
                &mut evidence,
                format!("final harness capture failed: {error}"),
            );
        }
    }

    if let Some(runtime) = &runtime {
        if let Err(error) = runtime.finalize_root(&root_session_id).await {
            append_error(&mut evidence, format!("RLM finalization failed: {error}"));
        }
        if let Some(rlm_evidence) = runtime.evidence(&root_session_id).await {
            evidence.rlm_usage = Some(rlm_evidence.usage.clone());
            if let Err(error) =
                write_json_atomic(output.join("rlm-evidence.json"), &rlm_evidence).await
            {
                append_error(
                    &mut evidence,
                    format!("RLM evidence capture failed: {error}"),
                );
            }
        }
    }
    if let Err(error) = agent.shutdown().await {
        append_error(&mut evidence, format!("agent shutdown failed: {error}"));
    }
    drop(agent);
    match event_task.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            append_error(&mut evidence, format!("root event capture failed: {error}"))
        }
        Err(error) => append_error(&mut evidence, format!("root event task failed: {error}")),
    }
    if let Some(runtime) = &runtime {
        runtime.shutdown().await;
    }

    evidence.completed_unix_ms = Some(unix_ms());
    write_evidence(&output, &evidence).await?;
    print_summary(&evidence, &output);

    play_result?;
    if let Some(error) = evidence.error {
        return Err(invalid(error));
    }
    Ok(())
}

async fn run_suite(mut config: Config) -> Result<()> {
    let mut arc = ArcClient::connect().await?;
    let mut games = arc.games().await?;
    if games.len() != 25 {
        return Err(invalid(format!(
            "complete ARC-AGI-3 suite requires 25 games, but the API returned {}",
            games.len()
        )));
    }
    if let Some(limit) = config.suite_game_limit {
        games.truncate(limit.min(games.len()));
    }
    let output = config.output.clone().unwrap_or_else(|| {
        PathBuf::from(".nanocodex/evals/arc-agi-3-rlm").join(format!(
            "all-{}-{}",
            mode_name(config.mode),
            unix_ms()
        ))
    });
    prepare_output(&output).await?;
    let output = output.canonicalize()?;
    tokio::fs::create_dir_all(output.join("games")).await?;
    tokio::fs::create_dir_all(output.join("logs")).await?;
    if config.mode == Mode::Rlm {
        let frozen = output.join("harness-frozen.toml");
        tokio::fs::copy(&config.harness, &frozen).await?;
        config.harness = frozen;
    }
    let executable = env::current_exe()?;

    arc.open_scorecard(config.mode).await?;
    let scorecard_id = arc.card_id().to_owned();
    let scorecard_url = format!("{API_BASE_URL}/scorecards/{scorecard_id}");
    let mut suite = SuiteEvidence {
        schema_version: 1,
        benchmark: "arc-agi-3",
        protocol: if config.suite_game_limit.is_some() || config.max_actions.is_some() {
            "official-live-one-scorecard-development-suite"
        } else {
            "official-live-one-scorecard-25-fresh-nanocodex-processes"
        },
        mode: config.mode,
        model: Model::Sol.as_str(),
        thinking: config.thinking.to_string(),
        action_budget_multiplier: config.action_multiplier,
        development_action_cap: config.max_actions,
        development_game_limit: config.suite_game_limit,
        concurrency: config.concurrency,
        scorecard_id: scorecard_id.clone(),
        scorecard_url,
        harness: (config.mode == Mode::Rlm).then(|| config.harness.clone()),
        started_unix_ms: unix_ms(),
        completed_unix_ms: None,
        games: games
            .iter()
            .map(|game| SuiteGameEvidence {
                game_id: game.game_id.clone(),
                output: output.join("games").join(&game.game_id),
                status: "pending",
                exit_code: None,
                error: None,
            })
            .collect(),
        scorecard: None,
        error: None,
    };
    if let Err(error) = write_json_atomic(output.join("suite.json"), &suite).await {
        let close = arc.close().await;
        return Err(invalid(format!(
            "failed to retain opened suite scorecard: {error}; scorecard cleanup: {close:?}"
        )));
    }
    eprintln!(
        "ARC suite started: mode={} games={} concurrency={} scorecard={}",
        mode_name(config.mode),
        games.len(),
        config.concurrency,
        suite.scorecard_url
    );

    let encoded_cookies = match arc.encoded_cookies() {
        Ok(cookies) => cookies,
        Err(error) => {
            let close = arc.close().await;
            return Err(invalid(format!(
                "failed to retain ARC suite session: {error}; scorecard cleanup: {close:?}"
            )));
        }
    };
    let capacity = Arc::new(Semaphore::new(config.concurrency));
    let mut tasks = JoinSet::new();
    for (index, game) in games.into_iter().enumerate() {
        let capacity = Arc::clone(&capacity);
        let executable = executable.clone();
        let config = config.clone();
        let output = output.clone();
        let scorecard_id = scorecard_id.clone();
        let api_key = arc.api_key().to_owned();
        let encoded_cookies = encoded_cookies.clone();
        tasks.spawn(async move {
            let result: Result<_> = async {
                let _permit = capacity.acquire_owned().await?;
                run_suite_game(
                    &executable,
                    &config,
                    &output,
                    &scorecard_id,
                    &api_key,
                    &encoded_cookies,
                    &game.game_id,
                )
                .await
            }
            .await;
            (index, game.game_id, result)
        });
    }

    let mut interrupted = false;
    while !tasks.is_empty() {
        tokio::select! {
            joined = tasks.join_next() => {
                let Some(joined) = joined else { break };
                match joined {
                    Ok((index, game_id, Ok(status))) => {
                        let success = status.success();
                        suite.games[index].status = if success { "completed" } else { "failed" };
                        suite.games[index].exit_code = status.code();
                        if !success {
                            suite.games[index].error = Some(format!("game process exited {status}"));
                        }
                        eprintln!(
                            "ARC suite game {game_id}: {} ({}/{})",
                            suite.games[index].status,
                            suite.games.iter().filter(|game| game.status != "pending").count(),
                            suite.games.len()
                        );
                    }
                    Ok((index, game_id, Err(error))) => {
                        suite.games[index].status = "failed";
                        suite.games[index].error = Some(error.to_string());
                        eprintln!("ARC suite game {game_id}: failed: {error}");
                    }
                    Err(error) => {
                        suite.error = Some(format!("suite worker failed: {error}"));
                        eprintln!("ARC suite worker failed: {error}");
                    }
                }
                if let Err(error) = write_json_atomic(output.join("suite.json"), &suite).await {
                    suite.error = Some(format!("incremental suite evidence failed: {error}"));
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                }
            }
            signal = tokio::signal::ctrl_c() => {
                interrupted = true;
                suite.error = Some(match signal {
                    Ok(()) => "interrupted".to_owned(),
                    Err(error) => format!("interrupt listener failed: {error}"),
                });
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
            }
        }
    }

    match arc.close().await {
        Ok(scorecard) => {
            write_json_atomic(output.join("scorecard.json"), &scorecard).await?;
            suite.scorecard = Some(scorecard);
        }
        Err(error) => {
            let close_error = format!("scorecard cleanup failed: {error}");
            suite.error = Some(match suite.error.take() {
                Some(primary) => format!("{primary}; {close_error}"),
                None => close_error,
            });
        }
    }
    suite.completed_unix_ms = Some(unix_ms());
    write_json_atomic(output.join("suite.json"), &suite).await?;
    println!("scorecard: {}", suite.scorecard_url);
    println!("artifacts: {}", output.display());

    if interrupted {
        return Err(invalid("complete ARC suite was interrupted"));
    }
    if suite.games.iter().any(|game| game.status != "completed") {
        return Err(invalid("one or more complete ARC suite games failed"));
    }
    if let Some(error) = suite.error {
        return Err(invalid(error));
    }
    Ok(())
}

async fn run_suite_game(
    executable: &Path,
    config: &Config,
    suite_output: &Path,
    scorecard_id: &str,
    api_key: &str,
    encoded_cookies: &str,
    game_id: &str,
) -> Result<std::process::ExitStatus> {
    let output = suite_output.join("games").join(game_id);
    let stdout = fs::File::create(suite_output.join("logs").join(format!("{game_id}.stdout")))?;
    let stderr = fs::File::create(suite_output.join("logs").join(format!("{game_id}.stderr")))?;
    let mut command = Command::new(executable);
    command
        .args(["--game", game_id, "--mode", mode_name(config.mode)])
        .arg("--thinking")
        .arg(config.thinking.to_string())
        .arg("--output")
        .arg(&output)
        .arg("--action-multiplier")
        .arg(config.action_multiplier.to_string())
        .arg("--turn-timeout-seconds")
        .arg(config.turn_timeout.as_secs().to_string())
        .args(["--scorecard-id", scorecard_id])
        .env("ARC_API_KEY", api_key)
        .env(ARC_COOKIES_ENV, encoded_cookies)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true);
    if config.mode == Mode::Rlm {
        command.arg("--harness").arg(&config.harness);
    }
    if let Some(max_actions) = config.max_actions {
        command.arg("--max-actions").arg(max_actions.to_string());
    }
    Ok(command.spawn()?.wait().await?)
}

async fn play_game(
    config: &Config,
    arc: &mut ArcClient,
    agent: &Nanocodex,
    mut frame: Frame,
    evidence: &mut RunEvidence,
    output: &Path,
) -> Result<()> {
    validate_frame(&frame, &evidence.game.game_id)?;
    let total_official_budget = evidence
        .game
        .baseline_actions
        .iter()
        .map(|baseline| ((*baseline as f64) * config.action_multiplier).ceil() as u64)
        .sum::<u64>();
    let action_limit = config.max_actions.unwrap_or(total_official_budget);
    let mut level_actions = 0_u64;
    let mut previous_action = None::<String>;

    loop {
        if frame.state == "WIN" {
            evidence.exit_reason = Some("GAME_WIN".to_owned());
            return Ok(());
        }
        if evidence.actions.len() as u64 >= action_limit {
            evidence.exit_reason = Some(
                if config.max_actions.is_some() {
                    "DEVELOPMENT_ACTION_CAP"
                } else {
                    "ACTION_BUDGET"
                }
                .to_owned(),
            );
            return Ok(());
        }
        let level = usize::try_from(frame.levels_completed)?;
        let Some(baseline) = evidence.game.baseline_actions.get(level) else {
            evidence.exit_reason = Some("LEVEL_METADATA_EXHAUSTED".to_owned());
            return Ok(());
        };
        let level_budget = ((*baseline as f64) * config.action_multiplier).ceil() as u64;
        if level_actions >= level_budget {
            evidence.exit_reason = Some("ACTION_BUDGET".to_owned());
            return Ok(());
        }

        let before = frame.clone();
        let completed_actions = evidence.actions.len() as u64;
        let refinement_checkpoint = config.refine_every_actions.is_some_and(|interval| {
            completed_actions > 0 && completed_actions.is_multiple_of(interval)
        });
        let prompt = render_frame_prompt(
            &frame,
            completed_actions + 1,
            previous_action.as_deref(),
            refinement_checkpoint,
        );
        let decision_started = Instant::now();
        let reset_available =
            !evidence.actions.is_empty() && previous_action.as_deref() != Some("RESET");
        let (action, model_attempts) =
            choose_action(config, agent, &frame, reset_available, prompt).await?;
        let decision_duration_ms = decision_started.elapsed().as_millis();
        frame = arc
            .act(&evidence.game.game_id, &frame.guid, &action)
            .await?;
        validate_frame(&frame, &evidence.game.game_id)?;
        level_actions += 1;
        let prior_level = before.levels_completed;
        evidence.actions.push(ActionRecord {
            index: evidence.actions.len() as u64 + 1,
            source: "model",
            decision_duration_ms,
            before,
            action: action.clone(),
            model_attempts,
            after: frame.clone(),
        });
        previous_action = Some(action.action_type);
        if frame.levels_completed > prior_level {
            level_actions = 0;
        }
        write_evidence(output, evidence).await?;

        if frame.state == "WIN" {
            continue;
        }
        if matches!(frame.state.as_str(), "GAME_OVER" | "NOT_PLAYED") {
            if evidence.actions.len() as u64 >= action_limit || level_actions >= level_budget {
                continue;
            }
            let before = frame.clone();
            frame = arc.reset(&evidence.game.game_id, &frame.guid).await?;
            validate_frame(&frame, &evidence.game.game_id)?;
            level_actions += 1;
            evidence.actions.push(ActionRecord {
                index: evidence.actions.len() as u64 + 1,
                source: "forced_reset",
                decision_duration_ms: 0,
                before,
                action: ArcAction {
                    action_type: "RESET".to_owned(),
                    x: None,
                    y: None,
                },
                model_attempts: Vec::new(),
                after: frame.clone(),
            });
            previous_action = Some("RESET".to_owned());
            write_evidence(output, evidence).await?;
        }
    }
}

async fn choose_action(
    config: &Config,
    agent: &Nanocodex,
    frame: &Frame,
    reset_available: bool,
    initial_prompt: String,
) -> Result<(ArcAction, Vec<ModelAttempt>)> {
    let mut attempts = Vec::with_capacity(MODEL_RETRIES_PER_FRAME);
    let mut prompt = initial_prompt;
    for _ in 0..MODEL_RETRIES_PER_FRAME {
        let turn = agent.prompt(prompt).await?;
        let result = timeout(config.turn_timeout, turn.result())
            .await
            .map_err(|_| invalid("Nanocodex frame turn timed out"))??;
        let output = result.final_message().to_owned();
        match parse_action(&output, frame, reset_available) {
            Ok(action) => {
                attempts.push(ModelAttempt {
                    output,
                    usage: result.usage().clone(),
                    parse_error: None,
                });
                return Ok((action, attempts));
            }
            Err(error) => {
                let message = error.to_string();
                attempts.push(ModelAttempt {
                    output,
                    usage: result.usage().clone(),
                    parse_error: Some(message.clone()),
                });
                prompt = format!(
                    "Your previous ARC_FRAME response was not executable: {message}. Return only one valid JSON action object for the unchanged frame."
                );
            }
        }
    }
    Err(invalid(format!(
        "model failed to return a valid action after {MODEL_RETRIES_PER_FRAME} attempts"
    )))
}

fn parse_action(output: &str, frame: &Frame, reset_available: bool) -> Result<ArcAction> {
    let payload: Value = serde_json::from_str(output.trim())
        .map_err(|error| invalid(format!("response is not exact JSON: {error}")))?;
    let object = payload
        .as_object()
        .ok_or_else(|| invalid("action response must be an object"))?;
    if object.len() != 1 {
        return Err(invalid("action response must contain only `actions`"));
    }
    let actions = object
        .get("actions")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("`actions` must be an array"))?;
    if actions.len() != 1 {
        return Err(invalid("`actions` must contain exactly one action"));
    }
    let action = actions[0]
        .as_object()
        .ok_or_else(|| invalid("the action must be an object"))?;
    let action_type = action
        .get("action_type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("action_type must be a string"))?;
    let available = frame
        .available_actions
        .iter()
        .any(|value| action_type == format!("ACTION{value}"));
    if !available && !(action_type == "RESET" && reset_available) {
        return Err(invalid(format!("{action_type} is not available")));
    }
    if action_type == "ACTION6" {
        if action.len() != 3 {
            return Err(invalid("ACTION6 requires only action_type, x, and y"));
        }
        let x = coordinate(action.get("x"), "x")?;
        let y = coordinate(action.get("y"), "y")?;
        return Ok(ArcAction {
            action_type: action_type.to_owned(),
            x: Some(x),
            y: Some(y),
        });
    }
    if action.len() != 1 {
        return Err(invalid(format!("{action_type} does not accept arguments")));
    }
    Ok(ArcAction {
        action_type: action_type.to_owned(),
        x: None,
        y: None,
    })
}

fn coordinate(value: Option<&Value>, name: &str) -> Result<u8> {
    let value = value
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid(format!("{name} must be an integer")))?;
    if value > 63 {
        return Err(invalid(format!("{name} must be between 0 and 63")));
    }
    Ok(value as u8)
}

fn render_frame_prompt(
    frame: &Frame,
    turn: u64,
    previous_action: Option<&str>,
    refinement_checkpoint: bool,
) -> String {
    let mut output = format!(
        "ARC_FRAME {turn}\nState: {}\nLevels completed: {}",
        frame.state, frame.levels_completed
    );
    if let Some(previous_action) = previous_action {
        let _ = write!(output, "\nPrevious submitted action: {previous_action}");
    }
    for (index, grid) in interpolate_frames(&frame.frame, MAX_ANIMATION_FRAMES)
        .into_iter()
        .enumerate()
    {
        let _ = write!(output, "\n\nFrame {index}:");
        for row in grid {
            let _ = write!(output, "\n  {row:?}");
        }
    }
    output.push_str("\n\nAvailable actions:");
    for action in &frame.available_actions {
        if *action == 6 {
            output.push_str("\n- ACTION6 x y  (where x and y are integers 0-63)");
        } else {
            let _ = write!(output, "\n- ACTION{action}");
        }
    }
    if turn > 1 && previous_action != Some("RESET") {
        output.push_str("\n- RESET");
    }
    if refinement_checkpoint {
        output.push_str(
            "\n\nHARNESS_REFINEMENT_CHECKPOINT: Review the retained trajectory before choosing this action. If the evidence shows a repeated failure or reusable tactic, call refine_harness with one concise observation, wait for the refiner, and verify the resulting harness revision. Make at most one minimal edit, then return the required action JSON.",
        );
    }
    output
}

fn interpolate_frames(frames: &[Vec<Vec<u8>>], target: usize) -> Vec<&Vec<Vec<u8>>> {
    if frames.len() <= target {
        return frames.iter().collect();
    }
    if target == 1 {
        return vec![&frames[frames.len() - 1]];
    }
    (0..target)
        .map(|index| {
            let numerator = index * (frames.len() - 1);
            &frames[round_ratio_ties_even(numerator, target - 1)]
        })
        .collect()
}

fn round_ratio_ties_even(numerator: usize, denominator: usize) -> usize {
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    match (remainder * 2).cmp(&denominator) {
        std::cmp::Ordering::Less => quotient,
        std::cmp::Ordering::Greater => quotient + 1,
        std::cmp::Ordering::Equal if quotient.is_multiple_of(2) => quotient,
        std::cmp::Ordering::Equal => quotient + 1,
    }
}

fn validate_frame(frame: &Frame, game_id: &str) -> Result<()> {
    if frame.game_id != game_id {
        return Err(invalid("official ARC API returned the wrong game"));
    }
    if frame.guid.is_empty() || frame.frame.is_empty() {
        return Err(invalid("official ARC API returned an incomplete frame"));
    }
    if !matches!(
        frame.state.as_str(),
        "NOT_PLAYED" | "NOT_FINISHED" | "WIN" | "GAME_OVER"
    ) {
        return Err(invalid(format!("unknown ARC state {:?}", frame.state)));
    }
    if frame
        .available_actions
        .iter()
        .any(|action| !(1..=7).contains(action))
    {
        return Err(invalid("official ARC API returned an invalid action"));
    }
    Ok(())
}

fn resolve_game<'a>(games: &'a [GameMetadata], requested: &str) -> Result<&'a GameMetadata> {
    let requested = requested.to_ascii_lowercase();
    let matches = games
        .iter()
        .filter(|game| {
            game.game_id.to_ascii_lowercase() == requested
                || game.title.to_ascii_lowercase() == requested
                || game
                    .game_id
                    .to_ascii_lowercase()
                    .starts_with(&format!("{requested}-"))
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [game] => Ok(*game),
        [] => Err(invalid(format!(
            "official ARC API does not offer {requested:?}"
        ))),
        _ => Err(invalid(format!("game selector {requested:?} is ambiguous"))),
    }
}

async fn write_evidence(output: &Path, evidence: &RunEvidence) -> Result<()> {
    write_json_atomic(output.join("run.json"), evidence).await
}

async fn prepare_output(output: &Path) -> Result<()> {
    if tokio::fs::try_exists(output).await? {
        if tokio::fs::read_dir(output)
            .await?
            .next_entry()
            .await?
            .is_some()
        {
            return Err(invalid(format!(
                "output directory {} is not empty",
                output.display()
            )));
        }
    } else {
        tokio::fs::create_dir_all(output).await?;
    }
    Ok(())
}

async fn write_json_atomic(path: PathBuf, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    tokio::fs::write(&temporary, bytes).await?;
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

fn append_error(evidence: &mut RunEvidence, message: String) {
    evidence.error = Some(match evidence.error.take() {
        Some(primary) => format!("{primary}; {message}"),
        None => message,
    });
}

fn print_summary(evidence: &RunEvidence, output: &Path) {
    let (root_input, root_cached, root_output, root_cost_nanos) = evidence
        .actions
        .iter()
        .flat_map(|action| &action.model_attempts)
        .fold((0_u64, 0_u64, 0_u64, 0_u64), |totals, attempt| {
            let cost = attempt
                .usage
                .estimated_cost()
                .map_or(0, |cost| cost.amount().nano_usd());
            (
                totals.0 + attempt.usage.input_tokens(),
                totals.1 + attempt.usage.cached_input_tokens(),
                totals.2 + attempt.usage.output_tokens(),
                totals.3 + cost,
            )
        });
    let child_usage = evidence.rlm_usage.as_ref().cloned().unwrap_or_default();
    let score = evidence
        .scorecard
        .as_ref()
        .and_then(|scorecard| scorecard.get("score"))
        .and_then(Value::as_f64);
    println!(
        "game={} mode={} actions={} exit={} score={} root_input={} root_cached={} root_output={} child_input={} child_cached={} child_output={} estimated_total_cost=${:.6}",
        evidence.game.game_id,
        mode_name(evidence.mode),
        evidence.actions.len(),
        evidence.exit_reason.as_deref().unwrap_or("UNKNOWN"),
        score.map_or_else(|| "unknown".to_owned(), |score| format!("{score:.6}")),
        root_input,
        root_cached,
        root_output,
        child_usage.input_tokens,
        child_usage.cached_input_tokens,
        child_usage.output_tokens,
        root_cost_nanos.saturating_add(child_usage.estimated_nano_usd) as f64 / 1_000_000_000.0,
    );
    println!("artifacts: {}", output.display());
}

const fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Baseline => "baseline",
        Mode::Rlm => "rlm",
    }
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_in_arc_harness_is_valid() -> Result<()> {
        let harness = HarnessSnapshot::load(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("rlm/arc-agi-3.harness.toml"),
        )?;
        assert_eq!(harness.revision(), 5);
        Ok(())
    }

    fn frame() -> Frame {
        Frame {
            game_id: "test-game".to_owned(),
            frame: vec![vec![vec![0; 2]; 2]],
            state: "NOT_FINISHED".to_owned(),
            levels_completed: 0,
            win_levels: 1,
            guid: "guid".to_owned(),
            available_actions: vec![1, 6],
            full_reset: false,
            action_input: None,
        }
    }

    #[test]
    fn parses_only_one_strict_available_action() {
        assert_eq!(
            parse_action(
                r#"{"actions":[{"action_type":"ACTION1"}]}"#,
                &frame(),
                false,
            )
            .unwrap(),
            ArcAction {
                action_type: "ACTION1".to_owned(),
                x: None,
                y: None,
            }
        );
        assert!(parse_action("ACTION1", &frame(), false).is_err());
        assert!(
            parse_action(
                r#"{"actions":[{"action_type":"ACTION2"}]}"#,
                &frame(),
                false,
            )
            .is_err()
        );
        assert!(
            parse_action(r#"{"actions":[{"action_type":"RESET"}]}"#, &frame(), false,).is_err()
        );
        assert!(parse_action(r#"{"actions":[{"action_type":"RESET"}]}"#, &frame(), true,).is_ok());
    }

    #[test]
    fn validates_coordinate_shape_and_bounds() {
        assert_eq!(
            parse_action(
                r#"{"actions":[{"action_type":"ACTION6","x":12,"y":34}]}"#,
                &frame(),
                false,
            )
            .unwrap(),
            ArcAction {
                action_type: "ACTION6".to_owned(),
                x: Some(12),
                y: Some(34),
            }
        );
        assert!(
            parse_action(
                r#"{"actions":[{"action_type":"ACTION6","x":64,"y":34}]}"#,
                &frame(),
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn interpolation_matches_python_round_ties_to_even() {
        let frames = (0..12)
            .map(|value| vec![vec![value]])
            .collect::<Vec<Vec<Vec<u8>>>>();
        let sampled = interpolate_frames(&frames, 7)
            .into_iter()
            .map(|grid| grid[0][0])
            .collect::<Vec<_>>();
        assert_eq!(sampled, [0, 2, 4, 6, 7, 9, 11]);
    }

    #[test]
    fn online_refinement_checkpoint_preserves_the_action_contract() {
        let prompt = render_frame_prompt(&frame(), 6, Some("ACTION1"), true);
        assert!(prompt.contains("HARNESS_REFINEMENT_CHECKPOINT"));
        assert!(prompt.contains("return the required action JSON"));
        assert!(prompt.contains("- ACTION1"));
    }
}
