mod app;
mod auth;
mod browser;
mod config;
mod core;
#[cfg(feature = "tempo")]
mod credits;
#[cfg(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
))]
mod eval;
#[cfg(not(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
)))]
#[path = "eval_unsupported.rs"]
mod eval;
mod mcp;
#[cfg_attr(not(feature = "tempo"), path = "mpp_disabled.rs")]
mod mpp;
mod observability;
mod review;
mod run;
mod subagents;
mod tui;
mod update;
mod version;
#[cfg(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
))]
mod vm;
#[cfg(not(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
)))]
#[path = "vm_unsupported.rs"]
mod vm;

use clap::{Args, Parser, Subcommand, builder::NonEmptyStringValueParser};
use eyre::{Result, eyre};
use tokio_util::sync::CancellationToken;

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
    /// Run and inspect durable VM-backed agent evaluations.
    Eval(eval::Eval),
    /// Internal entrypoint for one dedicated libkrun VMM process.
    #[command(hide = true)]
    VmRunConfig(vm::VmRunConfig),
    /// Run one prompt and stream JSONL events to stdout.
    Run(Box<RunCommand>),
    /// Resume a Nanocodex TUI session.
    Resume(Box<ResumeCommand>),
    /// Install, cache, or switch CLI builds.
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
    /// TUI session UUID to resume. Omit it to open the session picker.
    #[arg(value_parser = NonEmptyStringValueParser::new())]
    thread_id: Option<String>,

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
    nanocodex::oai::transport::install_default_rustls_crypto_provider();
    // Keep direct `cargo run` behavior consistent with the Justfile without
    // requiring shell-specific syntax to load the repository's `.env` file.
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();
    if let Some(Command::VmRunConfig(command)) = &cli.command {
        return command.run();
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
        Some(Command::VmRunConfig(_)) => unreachable!("VMM commands run before Tokio starts"),
        Some(Command::Run(command)) => {
            let _observability = command.observability.install(false, command.agent.cwd())?;
            command.run.run(command.agent, command.vm).await
        }
        Some(Command::Resume(command)) => {
            let startup = command.thread_id.map_or(
                tui::StartupMode::ResumeSelector,
                tui::StartupMode::ResumeSession,
            );
            run_tui(
                command.agent,
                command.vm,
                command.observability,
                startup,
                command.prompt,
            )
            .await
        }
        Some(Command::Update(command)) => command.run().await,
        None => {
            run_tui(
                cli.agent,
                cli.vm,
                cli.observability,
                tui::StartupMode::NewSession,
                cli.prompt,
            )
            .await
        }
    }
}

async fn run_tui(
    agent: AgentArgs,
    vm: vm::VmArgs,
    observability: ObservabilityArgs,
    startup: tui::StartupMode,
    prompt: Option<String>,
) -> Result<()> {
    let config = agent.load_tui_config()?;
    let _observability = observability.install(true, config.agent().workspace())?;
    let (config, resources) = agent.prepare_tui(vm, config).await?;
    let shutdown = CancellationToken::new();
    let run = tui::run(config, startup, prompt, shutdown.clone());
    tokio::pin!(run);
    let result = tokio::select! {
        result = &mut run => result.map_err(|error| eyre!(error)),
        signal = tokio::signal::ctrl_c() => {
            shutdown.cancel();
            let result = run.await.map_err(|error| eyre!(error));
            signal?;
            result
        }
    };
    let resource_result = resources.shutdown().await;
    result?;
    resource_result
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
    fn vm_tools_are_opt_in_for_tui_and_one_shot_runs() {
        let tui = Cli::try_parse_from(["nanocodex"]).unwrap();
        assert!(!tui.vm.is_enabled());

        let tui = Cli::try_parse_from([
            "nanocodex",
            "--vm",
            "/tmp/rootfs",
            "--vm-workspace",
            "/workspace",
        ])
        .unwrap();
        assert!(tui.vm.is_enabled());

        let run = Cli::try_parse_from(["nanocodex", "run", "reply with ok", "--vm", "/tmp/rootfs"])
            .unwrap();
        let Some(Command::Run(run)) = run.command else {
            panic!("run command was not parsed");
        };
        assert!(run.vm.is_enabled());
    }

    #[test]
    fn brave_browser_tool_and_all_cookies_are_enabled_by_default() {
        let tui = Cli::try_parse_from(["nanocodex"]).unwrap();
        assert!(tui.agent.browser_enabled());
        assert!(tui.agent.copies_all_browser_cookies());
        assert!(tui.agent.uses_brave_browser());

        let tui = Cli::try_parse_from(["nanocodex", "--browser"]).unwrap();
        assert!(tui.agent.browser_enabled());
        assert!(tui.agent.uses_brave_browser());

        let brave =
            Cli::try_parse_from(["nanocodex", "--browser=brave", "--cookies=true"]).unwrap();
        assert!(brave.agent.browser_enabled());
        assert!(brave.agent.uses_brave_browser());

        let chromium =
            Cli::try_parse_from(["nanocodex", "--browser=chromium", "--cookies=true"]).unwrap();
        assert!(chromium.agent.browser_enabled());
        assert!(!chromium.agent.uses_brave_browser());

        let all_cookies = Cli::try_parse_from(["nanocodex", "--cookies=all"]).unwrap();
        assert!(all_cookies.agent.browser_enabled());

        let no_cookies = Cli::try_parse_from(["nanocodex", "--cookies=none"]).unwrap();
        assert!(no_cookies.agent.browser_enabled());
        assert!(!no_cookies.agent.copies_all_browser_cookies());

        let chrome_source =
            Cli::try_parse_from(["nanocodex", "--browser=brave", "--cookies=chrome"]).unwrap();
        assert!(chrome_source.agent.browser_enabled());

        let firefox_source =
            Cli::try_parse_from(["nanocodex", "--browser", "--cookies=firefox"]).unwrap();
        assert!(firefox_source.agent.browser_enabled());

        let safari_source =
            Cli::try_parse_from(["nanocodex", "--browser", "--cookies=safari"]).unwrap();
        assert!(safari_source.agent.browser_enabled());

        let run = Cli::try_parse_from(["nanocodex", "run", "inspect example.com"]).unwrap();
        let Some(Command::Run(run)) = run.command else {
            panic!("run command was not parsed");
        };
        assert!(run.agent.browser_enabled());

        let disabled =
            Cli::try_parse_from(["nanocodex", "--browser=none", "--cookies=none"]).unwrap();
        assert!(!disabled.agent.browser_enabled());
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
        assert_eq!(
            command.thread_id.as_deref(),
            Some("019c0d31-c308-7d91-bff4-5dca82d15ac6")
        );
        assert_eq!(command.prompt.as_deref(), Some("continue"));
        assert!(!command.agent.uses_tempo());
    }

    #[test]
    fn resume_without_a_thread_id_opens_discovery_path() {
        let cli = Cli::try_parse_from(["nanocodex", "resume", "--provider.openai"])
            .expect("resume should accept an omitted thread UUID");

        let Some(Command::Resume(command)) = cli.command else {
            panic!("resume command was not parsed");
        };
        assert!(command.thread_id.is_none());
    }
}
