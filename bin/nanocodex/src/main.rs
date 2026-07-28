mod auth;
mod config;
#[cfg(feature = "tempo")]
mod credits;
mod eval;
mod mcp;
#[cfg_attr(not(feature = "tempo"), path = "mpp_disabled.rs")]
mod mpp;
mod observability;
mod run;
mod subagents;
mod tui;
mod update;
mod version;
mod vm;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, builder::NonEmptyStringValueParser};
use eyre::{Result, WrapErr};
use nanocodex::agent::rollout::RolloutConfig;

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

    #[command(flatten)]
    vm: vm::VmArgs,

    /// Submit an initial prompt immediately after the TUI opens.
    #[arg(long, value_parser = NonEmptyStringValueParser::new())]
    prompt: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Manage `ChatGPT` subscription login.
    Auth(auth::Auth),
    /// Inspect or purchase Nanocodex NANOUSD credits.
    #[cfg(feature = "tempo")]
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

    #[command(flatten)]
    vm: vm::VmArgs,
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

    #[command(flatten)]
    vm: vm::VmArgs,

    /// Submit an initial follow-on prompt immediately after the TUI opens.
    #[arg(long, value_parser = NonEmptyStringValueParser::new())]
    prompt: Option<String>,
}

fn main() -> Result<()> {
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
    match cli.command {
        Some(Command::Auth(command)) => command.run().await,
        #[cfg(feature = "tempo")]
        Some(Command::Credits(command)) => command.run().await,
        Some(Command::Eval(command)) => command.run().await,
        Some(Command::Run(command)) => {
            let _observability = command.observability.install(false, command.agent.cwd())?;
            command.run.run(command.agent, command.vm).await
        }
        Some(Command::Resume(command)) => {
            let codex_home = config::default_codex_home()?;
            let session = RolloutConfig::new(&codex_home)
                .load_session(&command.thread_id)
                .wrap_err_with(|| format!("failed to load Codex thread {}", command.thread_id))?;
            let workspace = PathBuf::from(session.workspace());
            let _observability = command.observability.install(true, &workspace)?;
            tui::run(command.agent, command.vm, command.prompt, Some(session)).await
        }
        Some(Command::Update(command)) => command.run().await,
        None => {
            let _observability = cli.observability.install(true, cli.agent.cwd())?;
            tui::run(cli.agent, cli.vm, cli.prompt, None).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "tempo")]
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
            nanocodex::oai::transport::ResponsesTransport::Https
        );
    }

    #[cfg(feature = "tempo")]
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
            nanocodex::oai::transport::ResponsesTransport::Https
        );
    }

    #[test]
    fn openai_provider_is_explicitly_selectable() {
        let cli = Cli::try_parse_from(["nanocodex", "--provider.openai", "--api-key", "test-key"])
            .unwrap();

        assert!(!cli.agent.uses_tempo());
        assert_eq!(
            cli.agent.responses_transport(),
            nanocodex::oai::transport::ResponsesTransport::WebSocket
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
    fn vm_tools_are_opt_in_for_the_tui_and_one_shot_runner() {
        let tui = Cli::try_parse_from(["nanocodex"]).unwrap();
        assert!(!tui.vm.is_enabled());

        let tui = Cli::try_parse_from([
            "nanocodex",
            "--vm",
            "/tmp/rootfs.ext4",
            "--vm-workspace",
            "/workspace",
        ])
        .unwrap();
        assert!(tui.vm.is_enabled());

        let run = Cli::try_parse_from([
            "nanocodex",
            "run",
            "reply with ok",
            "--vm",
            "/tmp/rootfs.ext4",
        ])
        .unwrap();
        let Some(Command::Run(run)) = run.command else {
            panic!("run command was not parsed");
        };
        assert!(run.vm.is_enabled());
    }

    #[test]
    fn vm_tuning_requires_an_opted_in_rootfs() {
        let error = Cli::try_parse_from(["nanocodex", "--vm-cpus", "4"])
            .err()
            .unwrap();

        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
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

    #[cfg(feature = "tempo")]
    #[test]
    fn provider_selection_is_exclusive() {
        let error = Cli::try_parse_from(["nanocodex", "--provider.openai", "--provider.tempo"])
            .err()
            .unwrap();

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[cfg(not(feature = "tempo"))]
    #[test]
    fn tempo_provider_is_absent_from_direct_agent_builds() {
        let error = Cli::try_parse_from(["nanocodex", "--provider.tempo"])
            .err()
            .unwrap();

        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
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
