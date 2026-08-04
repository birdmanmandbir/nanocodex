mod args;
mod cleanup;
mod diff;
mod import;
mod inspect;
mod profile;
mod run;

use std::{
    collections::BTreeMap,
    io::{self, Write},
    path::{Path, PathBuf},
};

use clap::{Args, Subcommand};
use eyre::Result;
use nanocodex_eval::{
    Task, TaskArtifact, VerifierCollect, VerifierEnvironmentMode,
    profile::{TaskPreparation, TaskPreparer},
    vm::{CachePolicy, VmTaskPreparer},
};
use serde::Serialize;

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
pub(crate) struct Eval {
    #[command(subcommand)]
    command: Option<EvalCommand>,

    #[command(flatten)]
    run: run::Run,
}

#[derive(Subcommand)]
enum EvalCommand {
    /// Resolve a profile and prepare all imports, images, and execution inputs.
    Prepare(profile::Prepare),

    /// Prepare task VM images directly without resolving an evaluation profile.
    PrepareImages(PrepareImages),

    /// Load, validate, and inspect a benchmark task directory.
    Task {
        /// Directory containing task.toml, instruction.md, and tests/test.sh.
        directory: PathBuf,

        /// Emit the complete loaded task as JSON.
        #[arg(long)]
        json: bool,

        /// Include the complete prompt in human-readable output.
        #[arg(long, conflicts_with = "json")]
        prompt: bool,
    },

    /// Convert a third-party dataset into content-addressed evaluator tasks.
    Import(import::Import),

    /// Explain a retained Harbor job or trial and surface exact failure evidence.
    Inspect(inspect::Inspect),

    /// Run matched Nanocodex and pinned stock-Codex task sweeps.
    Diff(diff::Diff),

    /// Remove disposable VM disks from completed retained trials.
    Cleanup(cleanup::Cleanup),
}

#[derive(Args)]
struct PrepareImages {
    /// Terminal-Bench task directory. Repeat to prepare several environments.
    #[arg(
        long = "task",
        value_name = "DIRECTORY",
        required_unless_present = "suites"
    )]
    tasks: Vec<PathBuf>,

    /// Terminal-Bench suite directory whose immediate task children should prepare.
    #[arg(
        long = "suite",
        value_name = "DIRECTORY",
        required_unless_present = "tasks"
    )]
    suites: Vec<PathBuf>,

    /// Content-addressed VM cache directory.
    #[arg(long, default_value = ".cache/vm")]
    cache: PathBuf,

    /// Resolve image references at the registry even when locally cached.
    #[arg(long)]
    refresh: bool,
}

#[derive(Clone, Debug, Default)]
struct NativeEvaluationRuntime {
    cache: Option<PathBuf>,
    refresh: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct NativeTaskPreparationError(String);

impl TaskPreparer for NativeEvaluationRuntime {
    type Error = NativeTaskPreparationError;

    fn prepare(
        &self,
        request: TaskPreparation,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        let cache = self
            .cache
            .clone()
            .unwrap_or_else(|| request.cache_directory().to_path_buf());
        let tasks = request.into_tasks();
        let refresh = self.refresh;
        async move {
            // Resolve the running, entitled VMM executable before a nested
            // guest-runtime build can cause Cargo's runner cache to rotate paths.
            let vmm = std::env::current_exe()
                .map_err(|error| NativeTaskPreparationError(error.to_string()))?;
            let runtime = run::prepare_vm_guest_runtime_from(None, &cache)
                .await
                .map_err(|error| NativeTaskPreparationError(format!("{error:#}")))?;
            let policy = if refresh {
                CachePolicy::Refresh
            } else {
                CachePolicy::Reuse
            };
            VmTaskPreparer::new(vmm, runtime)
                .cache_policy(policy)
                .prepare(TaskPreparation::new(tasks, cache))
                .await
                .map_err(|error| NativeTaskPreparationError(error.to_string()))
        }
    }
}

impl Eval {
    pub(crate) async fn run(self) -> Result<()> {
        enable_paint();
        run(self).await
    }
}

fn enable_paint() {
    let enable = yansi::Condition::os_support() && yansi::Condition::tty_and_color_live();
    yansi::whenever(yansi::Condition::cached(enable));
}

async fn run(eval: Eval) -> Result<()> {
    let Eval { command, run } = eval;
    match command {
        None => run.run().await?,
        Some(EvalCommand::Prepare(command)) => command.run().await?,
        Some(EvalCommand::PrepareImages(PrepareImages {
            tasks,
            suites,
            cache,
            refresh,
        })) => {
            let tasks = run::load_task_paths(tasks, suites)?
                .into_iter()
                .map(Task::load)
                .collect::<Result<Vec<_>, _>>()?;
            NativeEvaluationRuntime {
                cache: Some(cache.clone()),
                refresh,
            }
            .prepare(TaskPreparation::new(tasks, cache))
            .await?;
        }
        Some(EvalCommand::Task {
            directory,
            json,
            prompt,
        }) => {
            let task = Task::load(directory)?;
            let output = TaskOutput::from(&task);
            let stdout = io::stdout();
            let mut stdout = stdout.lock();
            if json {
                serde_json::to_writer_pretty(&mut stdout, &output)?;
                writeln!(stdout)?;
            } else {
                output.write_human(&mut stdout, prompt)?;
            }
        }
        Some(EvalCommand::Inspect(command)) => command.run()?,
        Some(EvalCommand::Import(command)) => command.run()?,
        Some(EvalCommand::Diff(command)) => command.run().await?,
        Some(EvalCommand::Cleanup(command)) => command.run()?,
    }
    Ok(())
}

#[derive(Serialize)]
struct TaskOutput<'a> {
    name: &'a str,
    description: &'a str,
    root: &'a Path,
    prompt: &'a str,
    image: &'a str,
    agent_timeout_sec: f64,
    verifier: VerifierOutput<'a>,
    artifacts: &'a [TaskArtifact],
    resources: ResourcesOutput,
    network: &'static str,
    environment: &'a BTreeMap<String, String>,
    requires_compose: bool,
}

#[derive(Serialize)]
struct VerifierOutput<'a> {
    script: &'a Path,
    timeout_sec: f64,
    environment: &'a BTreeMap<String, String>,
    environment_mode: VerifierEnvironmentMode,
    collect: &'a [VerifierCollect],
}

#[derive(Serialize)]
struct ResourcesOutput {
    cpus: u32,
    memory_mb: u64,
    storage_mb: u64,
    gpus: u32,
}

impl<'a> From<&'a Task> for TaskOutput<'a> {
    fn from(task: &'a Task) -> Self {
        Self {
            name: task.name(),
            description: task.description(),
            root: task.root(),
            prompt: task.prompt(),
            image: task.image().reference(),
            agent_timeout_sec: task.agent_timeout().as_secs_f64(),
            verifier: VerifierOutput {
                script: task.verifier().script(),
                timeout_sec: task.verifier().timeout().as_secs_f64(),
                environment: task.verifier().environment(),
                environment_mode: task.verifier().environment_mode(),
                collect: task.verifier().collect(),
            },
            artifacts: task.artifacts(),
            resources: ResourcesOutput {
                cpus: task.resources().cpus,
                memory_mb: task.resources().memory_mb,
                storage_mb: task.resources().storage_mb,
                gpus: task.resources().gpus,
            },
            network: task.network().as_str(),
            environment: task.environment(),
            requires_compose: task.requires_compose(),
        }
    }
}

impl TaskOutput<'_> {
    fn write_human(&self, mut output: impl Write, include_prompt: bool) -> io::Result<()> {
        writeln!(output, "{}", self.name)?;
        writeln!(output, "  root: {}", self.root.display())?;
        writeln!(output, "  image: {}", self.image)?;
        writeln!(output, "  prompt: {} bytes", self.prompt.len())?;
        writeln!(
            output,
            "  timeout: {}s agent, {}s verifier",
            self.agent_timeout_sec, self.verifier.timeout_sec
        )?;
        writeln!(
            output,
            "  resources: {} CPU, {} MiB memory, {} MiB storage, {} GPU",
            self.resources.cpus,
            self.resources.memory_mb,
            self.resources.storage_mb,
            self.resources.gpus
        )?;
        writeln!(output, "  network: {}", self.network)?;
        writeln!(
            output,
            "  verifier: {} ({})",
            self.verifier.script.display(),
            self.verifier.environment_mode.as_str()
        )?;
        writeln!(output, "  artifacts: {}", self.artifacts.len())?;
        writeln!(output, "  requires compose: {}", self.requires_compose)?;
        if include_prompt {
            writeln!(output, "\n{}", self.prompt)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::{CommandFactory, Parser};

    use super::{Eval, EvalCommand};
    use crate::{Cli, Command};

    #[test]
    fn prepare_images_accepts_repeated_tasks_in_input_order() {
        let cli = Cli::try_parse_from([
            "nanocodex",
            "eval",
            "prepare-images",
            "--task",
            "tasks/first",
            "--task",
            "tasks/second",
        ])
        .unwrap();
        let Some(Command::Eval(Eval {
            command: Some(EvalCommand::PrepareImages(super::PrepareImages { tasks, .. })),
            ..
        })) = cli.command
        else {
            panic!("expected vm prepare command");
        };

        assert_eq!(
            tasks,
            [
                Path::new("tasks/first").to_path_buf(),
                Path::new("tasks/second").to_path_buf()
            ]
        );
    }

    #[test]
    fn prepare_images_accepts_a_complete_suite() {
        let cli = Cli::try_parse_from([
            "nanocodex",
            "eval",
            "prepare-images",
            "--suite",
            "terminal-bench-2-1",
        ])
        .unwrap();
        let Some(Command::Eval(Eval {
            command: Some(EvalCommand::PrepareImages(super::PrepareImages { suites, .. })),
            ..
        })) = cli.command
        else {
            panic!("expected vm prepare command");
        };

        assert_eq!(suites, [Path::new("terminal-bench-2-1").to_path_buf()]);
    }

    #[test]
    fn complete_eval_surface_is_nested_under_nanocodex() {
        for arguments in [
            vec![
                "nanocodex",
                "eval",
                "--task",
                "tasks/write-greeting",
                "--vm-cache",
                "/var/cache/nanocodex-vm",
            ],
            vec![
                "nanocodex",
                "eval",
                "task",
                "tasks/write-greeting",
                "--json",
            ],
            vec!["nanocodex", "eval", "inspect", ".nanocodex/evals/job"],
            vec![
                "nanocodex",
                "eval",
                "diff",
                "--task",
                "tasks/write-greeting",
                "--codex-bin",
                "/tmp/codex",
                "--vm-cache",
                "/var/cache/nanocodex-vm",
                "--thinking",
                "medium",
            ],
            vec![
                "nanocodex",
                "eval",
                "cleanup",
                ".nanocodex/evals",
                "--dry-run",
            ],
        ] {
            Cli::try_parse_from(arguments).expect("supported eval command must parse");
        }

        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("eval"));
    }
}
