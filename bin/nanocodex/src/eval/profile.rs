use std::path::PathBuf;

use clap::Args;
use eyre::Result;
use nanocodex_eval::profile::TaskPreparer;
use nanocodex_eval::profile_run::ProfileRunPhase;
use nanocodex_eval_adapters::profile::EvaluationWorkspace;

use crate::eval::NativeEvaluationRuntime;

const CONFIG_FILE: &str = "nanocodex.toml";

#[derive(Args)]
pub(super) struct Prepare {
    #[command(flatten)]
    target: ProfileTarget,
}

#[derive(Args)]
pub(super) struct Run {
    #[command(flatten)]
    target: ProfileTarget,

    #[command(flatten)]
    agent: crate::config::EvalAgentArgs,

    /// Start a new run. Optional values select exact prepared tasks.
    #[arg(long, num_args = 0.., value_name = "TASK")]
    new: Option<Vec<String>>,
}

#[derive(Args)]
pub(super) struct Status {
    #[command(flatten)]
    target: ProfileTarget,

    /// Continue printing status until the run reaches a terminal phase.
    #[arg(long)]
    watch: bool,
}

#[derive(Args)]
pub(super) struct Stop {
    #[command(flatten)]
    target: ProfileTarget,
}

#[derive(Args)]
pub(super) struct Report {
    #[command(flatten)]
    target: ProfileTarget,
}

#[derive(Args)]
struct ProfileTarget {
    /// Evaluation profile. Uses the manifest's top-level `default` when omitted.
    profile: Option<String>,

    /// Evaluation manifest.
    #[arg(long, default_value = CONFIG_FILE)]
    config: PathBuf,

    /// Retained evaluator state. Defaults to ~/.nanocodex/evals.
    #[arg(long, value_name = "DIRECTORY")]
    dir: Option<PathBuf>,
}

impl Prepare {
    pub(super) async fn run(self) -> Result<()> {
        let workspace = self.target.workspace(NativeEvaluationRuntime::default())?;
        let prepared = workspace.prepare(self.target.profile.as_deref()).await?;
        println!("{prepared}");
        Ok(())
    }
}

impl Run {
    pub(super) async fn run(self) -> Result<()> {
        let workspace = self
            .target
            .workspace(NativeEvaluationRuntime::for_run(self.agent))?;
        let result = match self.new {
            Some(tasks) => {
                workspace
                    .start_new(self.target.profile.as_deref(), tasks)
                    .await?
            }
            None => workspace.run(self.target.profile.as_deref()).await?,
        };
        println!("{result}");
        Ok(())
    }
}

impl Status {
    pub(super) async fn run(self) -> Result<()> {
        let workspace = self.target.workspace(NativeEvaluationRuntime::default())?;
        loop {
            let status = workspace.status(self.target.profile.as_deref())?;
            let Some(status) = status else {
                println!("profile has not run");
                return Ok(());
            };
            println!("{status}");
            if !self.watch
                || !matches!(
                    status.run().phase(),
                    ProfileRunPhase::Running | ProfileRunPhase::Stopping
                )
            {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }
}

impl Stop {
    pub(super) fn run(self) -> Result<()> {
        let workspace = self.target.workspace(NativeEvaluationRuntime::default())?;
        let status = workspace.stop(self.target.profile.as_deref())?;
        println!("{status}");
        Ok(())
    }
}

impl Report {
    pub(super) fn run(self) -> Result<()> {
        let workspace = self.target.workspace(NativeEvaluationRuntime::default())?;
        let report = workspace.report(self.target.profile.as_deref())?;
        serde_json::to_writer_pretty(std::io::stdout().lock(), &report)?;
        println!();
        Ok(())
    }
}

impl ProfileTarget {
    fn workspace<P: TaskPreparer>(
        &self,
        runtime: P,
    ) -> std::result::Result<
        EvaluationWorkspace<P>,
        nanocodex_eval_adapters::profile::ProfileImportError,
    > {
        let mut workspace = EvaluationWorkspace::builder()
            .manifest(&self.config)
            .task_preparer(runtime);
        if let Some(directory) = &self.dir {
            workspace = workspace.state_directory(directory);
        }
        workspace.build()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::Parser as _;

    use super::{Prepare, ProfileTarget, Run};
    use crate::{
        Cli, Command,
        eval::{Eval, EvalCommand},
    };

    #[test]
    fn prepare_uses_one_optional_profile_positional_and_deliberate_paths() {
        let cli = Cli::try_parse_from([
            "nanocodex",
            "eval",
            "prepare",
            "adapter-smoke",
            "--config",
            "eval.toml",
            "--dir",
            "/mnt/evals",
        ])
        .unwrap();
        let Some(Command::Eval(Eval {
            command:
                Some(EvalCommand::Prepare(Prepare {
                    target:
                        ProfileTarget {
                            profile,
                            config,
                            dir,
                        },
                })),
            ..
        })) = cli.command
        else {
            panic!("expected profile preparation command");
        };

        assert_eq!(profile.as_deref(), Some("adapter-smoke"));
        assert_eq!(config, Path::new("eval.toml"));
        assert_eq!(dir.as_deref(), Some(Path::new("/mnt/evals")));
    }

    #[test]
    fn run_uses_the_same_profile_target() {
        let cli = Cli::try_parse_from([
            "nanocodex",
            "eval",
            "run",
            "adapter-smoke",
            "--config",
            "eval.toml",
            "--dir",
            "/mnt/evals",
            "--api-key",
            "test-key",
        ])
        .unwrap();
        let Some(Command::Eval(Eval {
            command:
                Some(EvalCommand::Run(Run {
                    target:
                        ProfileTarget {
                            profile,
                            config,
                            dir,
                        },
                    ..
                })),
            ..
        })) = cli.command
        else {
            panic!("expected profile run command");
        };

        assert_eq!(profile.as_deref(), Some("adapter-smoke"));
        assert_eq!(config, Path::new("eval.toml"));
        assert_eq!(dir.as_deref(), Some(Path::new("/mnt/evals")));
    }

    #[test]
    fn new_run_accepts_zero_or_more_prepared_task_selectors() {
        for (arguments, expected) in [
            (
                vec!["nanocodex", "eval", "run", "adapter-smoke", "--new"],
                Vec::new(),
            ),
            (
                vec![
                    "nanocodex",
                    "eval",
                    "run",
                    "adapter-smoke",
                    "--new",
                    "exact-answer",
                    "terminal-bench-2.1/fix-git",
                ],
                vec![
                    "exact-answer".to_owned(),
                    "terminal-bench-2.1/fix-git".to_owned(),
                ],
            ),
        ] {
            let cli = Cli::try_parse_from(arguments).unwrap();
            let Some(Command::Eval(Eval {
                command: Some(EvalCommand::Run(Run { new, .. })),
                ..
            })) = cli.command
            else {
                panic!("expected new profile run command");
            };
            assert_eq!(new, Some(expected));
        }
    }

    #[test]
    fn lifecycle_commands_share_the_profile_positional() {
        for arguments in [
            vec!["nanocodex", "eval", "status", "adapter-smoke", "--watch"],
            vec!["nanocodex", "eval", "stop", "adapter-smoke"],
            vec!["nanocodex", "eval", "report", "adapter-smoke"],
        ] {
            Cli::try_parse_from(arguments).unwrap();
        }
    }
}
