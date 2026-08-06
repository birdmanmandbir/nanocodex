use clap::{Args, builder::NonEmptyStringValueParser};
use eyre::{Result, eyre};
use nanocodex::AgentEvents;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::autonomous::{AutonomousArgs, AutonomousDecision};
use crate::config::AgentArgs;
use crate::vm::VmArgs;

#[derive(Args)]
pub(crate) struct Run {
    /// Prompt submitted to the agent.
    #[arg(value_parser = NonEmptyStringValueParser::new())]
    prompt: String,

    /// Submit the same prompt as sequential follow-on turns on one owned session.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=100))]
    repeat: u16,

    #[command(flatten)]
    autonomous: AutonomousArgs,
}

impl Run {
    pub(crate) async fn run(self, config: AgentArgs, vm: VmArgs) -> Result<()> {
        if self.autonomous.enabled() && self.repeat != 1 {
            return Err(eyre!("--repeat cannot be combined with autonomous mode"));
        }
        let workspace = config.cwd().to_owned();
        let configured = config.build(vm).await?;
        let handle = configured.handle;
        let mut events = configured.events;
        let root_session_id = events.request_id().to_owned();
        let mut stdout = tokio::io::stdout();
        let mut autonomous = self.autonomous.start();
        let run_result: Result<()> = async {
            let mut prompt = self.prompt.clone();
            let mut remaining_repeats = self.repeat;
            loop {
                let turn = handle.prompt(prompt).await?;
                let control = turn.control();
                let completion = async {
                    write_turn_jsonl(&mut events, &mut stdout).await?;
                    let result = turn.result().await?;
                    Ok::<_, eyre::Report>(result)
                };
                tokio::pin!(completion);
                let result = tokio::select! {
                    result = &mut completion => result?,
                    signal = tokio::signal::ctrl_c() => {
                        signal?;
                        // The driver may have completed while JSONL was still
                        // backpressured. A late cancellation rejection must not
                        // discard its already-produced terminal event.
                        let _ = control.cancel().await;
                        let _ = completion.await;
                        return Err(eyre!("interrupted"));
                    }
                };
                handle.flush_rollout().await?;
                if let Some(state) = &mut autonomous {
                    state.record_turn(result.usage());
                    if let Some(rlm) = &configured.rlm
                        && let Some(evidence) = rlm.evidence(&root_session_id).await
                    {
                        state.record_recursive_usage(&evidence.usage);
                    }
                    match state.decide(&workspace, configured.rlm.is_some()).await? {
                        AutonomousDecision::Continue(continuation) => {
                            prompt = continuation;
                            continue;
                        }
                        AutonomousDecision::Stop(stop) => {
                            eprintln!("autonomous: {stop}");
                            if stop.is_failure() {
                                return Err(eyre!("autonomous completion failed: {stop}"));
                            }
                            break;
                        }
                    }
                }
                remaining_repeats = remaining_repeats.saturating_sub(1);
                if remaining_repeats == 0 {
                    break;
                }
                prompt = self.prompt.clone();
            }
            Ok(())
        }
        .await;
        let rlm_finalize_result = if let Some(rlm) = &configured.rlm {
            rlm.finalize_root(&root_session_id).await
        } else {
            Ok(())
        };
        let agent_shutdown = handle.shutdown().await;
        drop(handle);
        drop(events);
        if let Some(rlm) = configured.rlm {
            rlm.shutdown().await;
        }
        if let Some(child_agents) = configured.child_agents {
            child_agents.shutdown().await;
        }
        let browser_shutdown_result = if let Some(browser) = configured.browser {
            browser.shutdown().await
        } else {
            Ok(())
        };
        let vm_shutdown_result = if let Some(vm) = configured.vm {
            vm.shutdown().await
        } else {
            Ok(())
        };
        let shutdown_result = if let Some(adapter) = configured.mpp_adapter {
            adapter.shutdown().await
        } else {
            Ok(())
        };
        run_result?;
        rlm_finalize_result?;
        agent_shutdown?;
        browser_shutdown_result?;
        vm_shutdown_result?;
        shutdown_result
    }
}

async fn write_turn_jsonl(
    events: &mut AgentEvents,
    output: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    while let Some(event) = events.recv().await {
        let terminal = event.kind.is_terminal();
        let mut record = serde_json::to_vec(&event)?;
        record.push(b'\n');
        output.write_all(&record).await?;
        output.flush().await?;
        if terminal {
            return Ok(());
        }
    }
    Err(eyre!(
        "agent event stream closed before the turn emitted a terminal event"
    ))
}
