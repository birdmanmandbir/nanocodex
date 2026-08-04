use std::path::PathBuf;

use clap::Args;
use eyre::Result;
use nanocodex_eval::profile::TaskPreparer;
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
        let result = workspace.run(self.target.profile.as_deref()).await?;
        println!("{result}");
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
}
