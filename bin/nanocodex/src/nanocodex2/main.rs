//! Nanocodex2: the Tact-parity terminal client for managed Nanocodex agents.

mod app;
mod client;
mod core;
mod engine;
mod load;
mod review;
mod room;
mod tui;

use std::{
    env,
    io::{self, Write},
    process::ExitCode,
    time::Duration,
};

use clap::{Args, Parser, Subcommand, builder::NonEmptyStringValueParser};
use client::{ApiKey, EventCursor, ManagedClient, ManagedError, ManagedEventData, PromptInput};
use load::{ConnectReplayBehavior, SaturationConfig};
use room::{
    AccountKey, RoomApi, RoomConnection, RoomCursor, RoomError, RoomEventMessage, RoomEvents,
    RoomInvitation, RoomServerMessage, RoomTarget,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

const MANAGED_URL_ENV: &str = "NANOCODEX_MANAGED_URL";
const API_KEY_ENV: &str = "NANOCODEX_API_KEY";

pub(crate) fn install_tls_provider() {
    nanocodex::oai::transport::install_default_rustls_crypto_provider();
}

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
    /// Create or join a shared managed-agent room.
    Room(Room),
    /// Run a bounded, cleanup-owning managed-room saturation wave.
    Load(Load),
}

#[derive(Args)]
struct Load {
    /// Independent rooms created concurrently (1 through 8).
    #[arg(long, default_value_t = 1)]
    rooms: usize,
    /// Anonymous invited members per room (1 through 15).
    #[arg(long, default_value_t = 1)]
    guests_per_room: usize,
    /// Durable room messages sent by each guest (0 through 8).
    #[arg(long, default_value_t = 1)]
    messages_per_guest: usize,
    /// Hosted agent prompts sent in each room (0 through 4).
    #[arg(long, default_value_t = 0)]
    agent_prompts_per_room: usize,
    /// Reconnect every member at cursor zero and verify complete replay.
    #[arg(long)]
    replay: bool,
    /// Hard wall-clock deadline, including cleanup (30 through 900 seconds).
    #[arg(long, default_value_t = 60)]
    max_seconds: u64,
}

#[derive(Args)]
struct Room {
    #[command(subcommand)]
    command: RoomCommand,
}

#[derive(Subcommand)]
enum RoomCommand {
    /// Create a room, print its invite, and connect as its owner.
    Create(RoomCreate),
    /// Read an invite URL from stdin and connect as a guest.
    Join(RoomJoin),
}

#[derive(Args)]
struct RoomCreate {
    /// Name shown to other room members.
    #[arg(long, default_value = "Host")]
    name: String,
}

#[derive(Args)]
struct RoomJoin {
    /// Name shown to other room members.
    #[arg(long, default_value = "Guest")]
    name: String,
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
    prompt: Option<String>,
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
    install_tls_provider();
    let cli = Cli::parse();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| ManagedError::Configuration(format!("failed to start Tokio: {error}")))?
        .block_on(run(cli))
}

async fn run(cli: Cli) -> Result<(), ManagedError> {
    let command = cli.command;
    if matches!(&command, Command::Run(command) if command.prompt.is_none()) {
        return run_tui().await;
    }
    if let Command::Room(command) = command {
        return run_room(command).await.map_err(room_error);
    }
    if let Command::Load(command) = command {
        return run_load(command).await;
    }
    let client = client_from_environment()?;
    match command {
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
        Command::Room(_) => unreachable!("room command returned before managed-agent dispatch"),
        Command::Load(_) => unreachable!("load command returned before managed-agent dispatch"),
    }
}

pub(crate) fn client_from_environment() -> Result<ManagedClient, ManagedError> {
    let base_url = managed_url_from_environment()?;
    let api_key = env::var(API_KEY_ENV).map_err(|_| {
        ManagedError::Configuration(format!(
            "{API_KEY_ENV} must be set to an account-issued ncx_live key"
        ))
    })?;
    ManagedClient::new(base_url, ApiKey::parse(api_key)?)
}

fn managed_url_from_environment() -> Result<Url, ManagedError> {
    let base_url = env::var(MANAGED_URL_ENV).map_err(|_| {
        ManagedError::Configuration(format!(
            "{MANAGED_URL_ENV} must be set to the managed origin"
        ))
    })?;
    let base_url = Url::parse(&base_url).map_err(|_| {
        ManagedError::Configuration(format!("{MANAGED_URL_ENV} is not a valid URL"))
    })?;
    Ok(base_url)
}

async fn run_load(command: Load) -> Result<(), ManagedError> {
    let managed_url = managed_url_from_environment()?;
    let account_key = env::var(API_KEY_ENV).map_err(|_| {
        ManagedError::Configuration(format!(
            "{API_KEY_ENV} must be set to run managed-room saturation"
        ))
    })?;
    let summary = load::run_saturation(SaturationConfig {
        managed_url,
        account_key: AccountKey::parse(account_key).map_err(room_error)?,
        rooms: command.rooms,
        guests_per_room: command.guests_per_room,
        messages_per_guest: command.messages_per_guest,
        agent_prompts_per_room: command.agent_prompts_per_room,
        connect_replay: if command.replay {
            ConnectReplayBehavior::ReconnectAllFromZero
        } else {
            ConnectReplayBehavior::LiveOnly
        },
        max_wall_time: Duration::from_secs(command.max_seconds),
    })
    .await
    .map_err(|error| ManagedError::Configuration(error.to_string()))?;
    let passed = summary.passed();
    write_json(&summary)?;
    if passed {
        Ok(())
    } else {
        Err(ManagedError::Configuration(
            "managed-room saturation reported failures; inspect the JSON summary".to_owned(),
        ))
    }
}

async fn run_room(command: Room) -> Result<(), RoomError> {
    let managed_url = managed_url_from_environment()
        .map_err(|error| RoomError::Configuration(error.to_string()))?;
    match command.command {
        RoomCommand::Create(command) => {
            let api_key = env::var(API_KEY_ENV).map_err(|_| {
                RoomError::Configuration(format!(
                    "{API_KEY_ENV} must be set to create a managed room"
                ))
            })?;
            let api = RoomApi::authenticated(managed_url, AccountKey::parse(api_key)?)?;
            let membership = api.create(&command.name).await?;
            let invitation = membership.receipt().invitation().to_url()?;
            println!("Room: {}", membership.receipt().room_id());
            println!("Invite: {invitation}");
            println!(
                "Share that invite with the second terminal. Type /agent <prompt> to ask the hosted agent, or /quit to leave."
            );
            let (connection, events) = membership.connect(&RoomCursor::zero()).await?;
            run_room_terminal(connection, events).await
        }
        RoomCommand::Join(command) => {
            eprint!("Paste the room invite URL: ");
            io::stderr().flush().map_err(|error| {
                RoomError::Configuration(format!("failed to prompt for room invite: {error}"))
            })?;
            let mut input = String::new();
            BufReader::new(tokio::io::stdin())
                .read_line(&mut input)
                .await
                .map_err(|error| {
                    RoomError::Configuration(format!("failed to read room invite: {error}"))
                })?;
            let invitation = RoomInvitation::parse(input.trim())?;
            let api = RoomApi::public(managed_url)?;
            let membership = api.join(&invitation, &command.name).await?;
            println!(
                "Joined room {} as {}.",
                membership.receipt().room_id(),
                command.name
            );
            println!("Type /agent <prompt> to ask the hosted agent, or /quit to leave.");
            let (connection, events) = membership.connect(&RoomCursor::zero()).await?;
            run_room_terminal(connection, events).await
        }
    }
}

async fn run_room_terminal(
    connection: RoomConnection,
    mut events: RoomEvents,
) -> Result<(), RoomError> {
    println!(
        "Connected: {} member(s), {} online.",
        connection.ready().members.len(),
        connection.ready().online_member_ids.len()
    );
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let line = line.map_err(|error| {
                    RoomError::Configuration(format!("failed to read room input: {error}"))
                })?;
                let Some(line) = line else {
                    connection.close().await?;
                    return Ok(());
                };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if line == "/quit" {
                    connection.close().await?;
                    return Ok(());
                }
                if let Some(prompt) = line.strip_prefix("/agent ") {
                    connection.say_agent(prompt).await?;
                } else if let Some(message) = line.strip_prefix("/room ") {
                    connection.say_room(message).await?;
                } else {
                    connection.say_room(line).await?;
                }
            }
            event = events.next() => {
                let Some(event) = event else {
                    return Err(RoomError::CommandChannelClosed);
                };
                print_room_message(event?);
            }
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|error| {
                    RoomError::Configuration(format!("failed to listen for Ctrl-C: {error}"))
                })?;
                connection.close().await?;
                return Ok(());
            }
        }
    }
}

fn print_room_message(message: RoomServerMessage) {
    match message {
        RoomServerMessage::RoomEvent { event, .. } => match event {
            RoomEventMessage::MemberJoined { member } => {
                println!("• {} joined", member.name);
            }
            RoomEventMessage::MemberMessage {
                member,
                text,
                target,
                ..
            } => match target {
                RoomTarget::Room => println!("{}: {text}", member.name),
                RoomTarget::Agent => println!("{} → agent: {text}", member.name),
            },
            RoomEventMessage::AgentMessage { text, .. } => println!("agent: {text}"),
            RoomEventMessage::AgentError { code, .. } => {
                println!("agent error: {code:?}");
            }
        },
        RoomServerMessage::ReplayPaused {
            cursor,
            latest_cursor,
        } => {
            println!("• replay {cursor}/{latest_cursor}");
        }
        RoomServerMessage::Presence { online_member_ids } => {
            println!("• {} member(s) online", online_member_ids.len());
        }
        RoomServerMessage::Error { code, message, .. } => {
            eprintln!("Room error ({code}): {message}");
        }
        RoomServerMessage::Accepted { .. }
        | RoomServerMessage::Pong { .. }
        | RoomServerMessage::Ready { .. } => {}
    }
}

fn room_error(error: RoomError) -> ManagedError {
    ManagedError::Configuration(error.to_string())
}

async fn run_turn(client: &ManagedClient, command: Run) -> Result<(), ManagedError> {
    let prompt = command.prompt.ok_or_else(|| {
        ManagedError::Configuration("interactive run was routed to headless execution".to_owned())
    })?;
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
            &PromptInput::Text(prompt),
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

async fn run_tui() -> Result<(), ManagedError> {
    let config = app::config::Config::load(app::config::ConfigOverrides::default())
        .map_err(|error| ManagedError::Configuration(error.to_string()))?;
    let shutdown = CancellationToken::new();
    let signal_shutdown = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_shutdown.cancel();
        }
    });
    tui::run(config, tui::StartupMode::NewSession, shutdown)
        .await
        .map(|_| ())
        .map_err(|error| ManagedError::Configuration(error.to_string()))
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
