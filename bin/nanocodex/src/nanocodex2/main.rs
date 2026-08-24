//! Nanocodex2: the Tact-parity terminal client for managed Nanocodex agents.

mod client;

use std::{
    env,
    io::{self, Write},
    process::ExitCode,
};

use clap::{Args, Parser, Subcommand, builder::NonEmptyStringValueParser};
use client::{ApiKey, EventCursor, ManagedClient, ManagedError, ManagedEventData, PromptInput};
use url::Url;
use uuid::Uuid;

const MANAGED_URL_ENV: &str = "NANOCODEX_MANAGED_URL";
const API_KEY_ENV: &str = "NANOCODEX_API_KEY";

#[derive(Parser)]
#[command(
    name = "nanocodex2",
    about = "Tact-compatible terminal client for managed Nanocodex agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a managed agent and print its receipt as JSON.
    New,
    /// List account-owned managed agents as JSON.
    List,
    /// Read one managed agent's durable state as JSON.
    State(AgentId),
    /// Read one managed turn's durable state as JSON.
    Turn(TurnId),
    /// Delete one managed agent and its retained state.
    Delete(AgentId),
    /// Submit one prompt and stream durable managed events as JSONL.
    Run(Run),
    /// Stream an owned agent's durable events from a cursor.
    Watch(Watch),
    /// Read one backward page of retained events.
    History(History),
    /// Steer an active managed turn.
    Steer(Steer),
    /// Cancel an active managed turn.
    Cancel(TurnId),
}

#[derive(Args)]
struct AgentId {
    /// Account-owned managed agent ID.
    agent_id: String,
}

#[derive(Args)]
struct TurnId {
    /// Account-owned managed agent ID.
    agent_id: String,
    /// Managed turn ID.
    turn_id: String,
}

#[derive(Args)]
struct Run {
    /// Prompt text.
    #[arg(value_parser = NonEmptyStringValueParser::new())]
    prompt: String,
    /// Resume this account-owned agent. A new one is created when omitted.
    #[arg(long)]
    agent: Option<String>,
    /// Stable turn ID. A random ID is generated when omitted.
    #[arg(long)]
    turn_id: Option<String>,
    /// Stable idempotency key. A random key is generated when omitted.
    #[arg(long)]
    idempotency_key: Option<String>,
}

#[derive(Args)]
struct Watch {
    /// Account-owned managed agent ID.
    agent_id: String,
    /// Resume strictly after this decimal cursor, or tail from `latest`.
    #[arg(long, default_value = "0")]
    cursor: String,
}

#[derive(Args)]
struct History {
    /// Account-owned managed agent ID.
    agent_id: String,
    /// Return rows strictly before this positive decimal cursor.
    #[arg(long)]
    before: Option<String>,
    /// Page size from 1 through 256.
    #[arg(long, default_value_t = 128)]
    limit: u16,
}

#[derive(Args)]
struct Steer {
    /// Account-owned managed agent ID.
    agent_id: String,
    /// Active managed turn ID.
    turn_id: String,
    /// Additional prompt text.
    #[arg(value_parser = NonEmptyStringValueParser::new())]
    prompt: String,
}

fn main() -> ExitCode {
    match try_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn try_main() -> Result<(), ManagedError> {
    let cli = Cli::parse();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| ManagedError::Configuration(format!("failed to start Tokio: {error}")))?
        .block_on(run(cli))
}

async fn run(cli: Cli) -> Result<(), ManagedError> {
    let client = client_from_environment()?;
    match cli.command {
        Command::New => write_json(&client.create().await?),
        Command::List => write_json(&client.list().await?),
        Command::State(command) => write_json(&client.state(&command.agent_id).await?),
        Command::Turn(command) => write_json(
            &client
                .turn_state(&command.agent_id, &command.turn_id)
                .await?,
        ),
        Command::Delete(command) => client.delete(&command.agent_id).await,
        Command::Run(command) => run_turn(&client, command).await,
        Command::Watch(command) => watch(&client, command).await,
        Command::History(command) => write_json(
            &client
                .history(&command.agent_id, command.before.as_deref(), command.limit)
                .await?,
        ),
        Command::Steer(command) => write_json(
            &client
                .steer(
                    &command.agent_id,
                    &command.turn_id,
                    &PromptInput::Text(command.prompt),
                )
                .await?,
        ),
        Command::Cancel(command) => {
            write_json(&client.cancel(&command.agent_id, &command.turn_id).await?)
        }
    }
}

fn client_from_environment() -> Result<ManagedClient, ManagedError> {
    let base_url = env::var(MANAGED_URL_ENV).map_err(|_| {
        ManagedError::Configuration(format!(
            "{MANAGED_URL_ENV} must be set to the managed origin"
        ))
    })?;
    let base_url = Url::parse(&base_url).map_err(|_| {
        ManagedError::Configuration(format!("{MANAGED_URL_ENV} is not a valid URL"))
    })?;
    let api_key = env::var(API_KEY_ENV).map_err(|_| {
        ManagedError::Configuration(format!(
            "{API_KEY_ENV} must be set to an account-issued ncx_live key"
        ))
    })?;
    ManagedClient::new(base_url, ApiKey::parse(api_key)?)
}

async fn run_turn(client: &ManagedClient, command: Run) -> Result<(), ManagedError> {
    let agent_id = match command.agent {
        Some(agent_id) => {
            client.state(&agent_id).await?;
            agent_id
        }
        None => client.create().await?.agent_id,
    };
    let turn_id = command
        .turn_id
        .unwrap_or_else(|| format!("turn-{}", Uuid::new_v4()));
    let idempotency_key = command
        .idempotency_key
        .unwrap_or_else(|| format!("ncx-{}", Uuid::new_v4()));
    let accepted = client
        .submit(
            &agent_id,
            Some(&turn_id),
            &idempotency_key,
            &PromptInput::Text(command.prompt),
        )
        .await?;
    let accepted_turn_id = accepted.turn_id.clone();

    if let Some(terminal) = accepted.terminal {
        if let Some(result) = terminal.terminal_result(&accepted_turn_id) {
            let final_message = result?;
            eprintln!("{final_message}");
            return Ok(());
        }
    }

    let cursor = EventCursor::parse(accepted.accepted_cursor)?;
    let mut events = client.events(&agent_id, cursor)?;
    loop {
        let event = events.next().await?;
        if let Some(agent_event) = event.data.agent_event()? {
            write_json_line(&agent_event)?;
        } else {
            write_json_line(&event)?;
        }
        if let ManagedEventData::StreamFailed { error } = &event.data {
            return Err(ManagedError::Turn {
                turn_id: accepted_turn_id,
                state: "stream_failed".to_owned(),
                message: error.clone(),
            });
        }
        let belongs_to_turn = event.turn_id.as_deref() == Some(&accepted_turn_id)
            || event.data.turn_id() == Some(&accepted_turn_id);
        if belongs_to_turn {
            if let Some(result) = event.data.terminal_result(&accepted_turn_id) {
                let final_message = result?;
                eprintln!("{final_message}");
                return Ok(());
            }
        }
    }
}

async fn watch(client: &ManagedClient, command: Watch) -> Result<(), ManagedError> {
    let mut events = client.events(&command.agent_id, EventCursor::parse(command.cursor)?)?;
    loop {
        write_json_line(&events.next().await?)?;
    }
}

fn write_json<T: serde::Serialize>(value: &T) -> Result<(), ManagedError> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, value)
        .map_err(|_| ManagedError::InvalidResponse("failed to encode output"))?;
    output
        .write_all(b"\n")
        .and_then(|()| output.flush())
        .map_err(|_| ManagedError::InvalidResponse("failed to write output"))
}

fn write_json_line<T: serde::Serialize>(value: &T) -> Result<(), ManagedError> {
    write_json(value)
}
