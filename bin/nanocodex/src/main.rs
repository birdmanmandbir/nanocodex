mod auth;
mod config;
mod credits;
mod eval;
mod mcp;
mod mpp;
mod observability;
mod resource;
mod run;
mod subagents;
mod tui;
mod update;
mod version;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, builder::NonEmptyStringValueParser};
use eyre::{Result, WrapErr};
use nanocodex::RolloutConfig;

use config::AgentArgs;
use observability::ObservabilityArgs;

#[derive(Parser)]
#[command(
    version = version::SHORT_VERSION,
    long_version = version::LONG_VERSION,
    about = "An interactive coding agent and headless JSONL runner",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    agent: AgentArgs,

    #[command(flatten)]
    observability: ObservabilityArgs,

    /// Submit an initial prompt immediately after the TUI opens.
    #[arg(long, value_parser = NonEmptyStringValueParser::new())]
    prompt: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Manage `ChatGPT` subscription login.
    Auth(auth::Auth),
    /// Inspect or purchase Nanocodex NANOUSD credits.
    Credits(credits::Credits),
    /// Run and inspect durable agent evaluations.
    Eval(eval::Eval),
    /// Run one prompt and stream JSONL events to stdout.
    Run(Box<RunCommand>),
    /// Resume a Codex or Nanocodex thread in the interactive TUI.
    Resume(Box<ResumeCommand>),
    /// Update this executable from a GitHub release channel.
    Update(update::Update),
}

#[derive(Args)]
struct RunCommand {
    #[command(flatten)]
    run: run::Run,

    #[command(flatten)]
    agent: AgentArgs,

    #[command(flatten)]
    observability: ObservabilityArgs,
}

#[derive(Args)]
struct ResumeCommand {
    /// Codex thread UUID to resume.
    #[arg(value_parser = NonEmptyStringValueParser::new())]
    thread_id: String,

    #[command(flatten)]
    agent: AgentArgs,

    #[command(flatten)]
    observability: ObservabilityArgs,

    /// Submit an initial follow-on prompt immediately after the TUI opens.
    #[arg(long, value_parser = NonEmptyStringValueParser::new())]
    prompt: Option<String>,
}

fn main() -> Result<()> {
    install_rustls_crypto_provider();

    // Keep direct `cargo run` behavior consistent with the Justfile without
    // requiring shell-specific syntax to load the repository's `.env` file.
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();
    if let Some(Command::Eval(command)) = &cli.command
        && command.requires_synchronous_vm()
    {
        // libkrun's disk backend owns a small internal Tokio runtime. Entering
        // the blocking VMM loop from this process's async runtime makes disk
        // flush panic on guest exit, so dedicated VMM commands must run first.
        return command.run_synchronous_vm();
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    let uses_tempo = match &cli.command {
        Some(Command::Run(command)) => command.agent.uses_tempo(),
        Some(Command::Resume(command)) => command.agent.uses_tempo(),
        None => cli.agent.uses_tempo(),
        Some(Command::Auth(_) | Command::Credits(_) | Command::Eval(_) | Command::Update(_)) => {
            false
        }
    };
    if uses_tempo {
        resource::ensure_mpp_file_descriptor_capacity()?;
    }
    match cli.command {
        Some(Command::Auth(command)) => command.run().await,
        Some(Command::Credits(command)) => command.run().await,
        Some(Command::Eval(command)) => command.run().await,
        Some(Command::Run(command)) => {
            let _observability = command.observability.install(false, command.agent.cwd())?;
            command.run.run(command.agent).await
        }
        Some(Command::Resume(command)) => {
            let codex_home = config::default_codex_home()?;
            let session = RolloutConfig::new(&codex_home)
                .load_session(&command.thread_id)
                .wrap_err_with(|| format!("failed to load Codex thread {}", command.thread_id))?;
            let workspace = PathBuf::from(session.workspace());
            let _observability = command.observability.install(true, &workspace)?;
            tui::run(command.agent, command.prompt, Some(session)).await
        }
        Some(Command::Update(command)) => command.run().await,
        None => {
            let _observability = cli.observability.install(true, cli.agent.cwd())?;
            tui::run(cli.agent, cli.prompt, None).await
        }
    }
}

fn install_rustls_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tempo_flag_selects_the_tui_transport() {
        let cli = Cli::try_parse_from([
            "nanocodex",
            "--provider.tempo",
            "--provider.tempo.wallet-store",
            "/tmp/tempo-wallet.json",
        ])
        .unwrap();

        assert!(cli.command.is_none());
        assert!(cli.agent.uses_tempo());
        assert_eq!(
            cli.agent.responses_transport(),
            nanocodex::ResponsesTransport::Https
        );
    }

    #[test]
    fn rustls_crypto_provider_is_installed_idempotently() {
        install_rustls_crypto_provider();
        install_rustls_crypto_provider();

        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    #[test]
    fn tempo_flag_selects_the_one_shot_transport() {
        let cli = Cli::try_parse_from([
            "nanocodex",
            "run",
            "reply with ok",
            "--provider.tempo",
            "--provider.tempo.wallet-store",
            "/tmp/tempo-wallet.json",
        ])
        .unwrap();

        let Some(Command::Run(command)) = cli.command else {
            unreachable!();
        };
        assert!(command.agent.uses_tempo());
        assert_eq!(
            command.agent.responses_transport(),
            nanocodex::ResponsesTransport::Https
        );
    }

    #[test]
    fn openai_provider_is_explicitly_selectable() {
        let cli = Cli::try_parse_from(["nanocodex", "--provider.openai", "--api-key", "test-key"])
            .unwrap();

        assert!(!cli.agent.uses_tempo());
        assert_eq!(
            cli.agent.responses_transport(),
            nanocodex::ResponsesTransport::WebSocket
        );
    }

    #[test]
    fn raw_vm_is_dispatched_before_tokio_starts() {
        let cli = Cli::try_parse_from([
            "nanocodex",
            "eval",
            "vm",
            "run",
            "--root",
            "/tmp/rootfs.ext4",
            "--ext4",
            "--no-network",
            "/bin/true",
        ])
        .unwrap();

        let Some(Command::Eval(command)) = cli.command else {
            unreachable!();
        };
        assert!(command.requires_synchronous_vm());
    }

    #[test]
    fn ordinary_eval_stays_on_the_async_runtime() {
        let cli = Cli::try_parse_from(["nanocodex", "eval", "task", "/tmp/frontier-task"]).unwrap();

        let Some(Command::Eval(command)) = cli.command else {
            unreachable!();
        };
        assert!(!command.requires_synchronous_vm());
    }

    #[test]
    fn eval_does_not_accept_unimplemented_provider_flags() {
        let Err(error) = Cli::try_parse_from([
            "nanocodex",
            "eval",
            "task",
            "/tmp/frontier-task",
            "--provider.tempo",
        ]) else {
            panic!("eval unexpectedly accepted an unimplemented provider flag");
        };

        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn remote_browser_accepts_brave_cookie_origins() {
        let cli = Cli::try_parse_from([
            "nanocodex",
            "run",
            "inspect the deployment",
            "--browser-cdp",
            "ws://127.0.0.1:9222",
            "--browser-brave",
            "https://console.example.com",
            "--browser-brave",
            "https://company.okta.example",
        ])
        .unwrap();

        assert!(matches!(cli.command, Some(Command::Run(_))));
    }

    #[test]
    fn provider_selection_is_exclusive() {
        let error = Cli::try_parse_from(["nanocodex", "--provider.openai", "--provider.tempo"])
            .err()
            .unwrap();

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn resume_accepts_a_thread_id_and_agent_configuration() {
        let cli = Cli::try_parse_from([
            "nanocodex",
            "resume",
            "019c0d31-c308-7d91-bff4-5dca82d15ac6",
            "--provider.openai",
            "--api-key",
            "test-key",
            "--prompt",
            "continue",
        ])
        .unwrap();

        let Some(Command::Resume(command)) = cli.command else {
            panic!("resume command was not parsed");
        };
        assert_eq!(command.thread_id, "019c0d31-c308-7d91-bff4-5dca82d15ac6");
        assert_eq!(command.prompt.as_deref(), Some("continue"));
        assert!(!command.agent.uses_tempo());
    }
}
