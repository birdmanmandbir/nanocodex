use std::{env, path::PathBuf};

use nanocodex::{Nanocodex, OpenAi};
use nanocodex_eval::{Evaluator, Sweep, Task, harbor::Harbor, vm::VmBackend};
use nanocodex_examples::eval_support as support;
use nanocodex_rlm::{HarnessSnapshot, LaunchSnapshot, PromptPack, RlmRuntime};

#[tokio::main]
async fn main() -> Result<(), support::AnyError> {
    let task_paths = env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let task_paths = if task_paths.is_empty() {
        vec![PathBuf::from("tasks/write-greeting")]
    } else {
        task_paths
    };
    let tasks = task_paths
        .into_iter()
        .map(Task::load)
        .collect::<Result<Vec<_>, _>>()?;
    let crate_root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../crates/experimental/nanocodex-rlm");
    let prompts = env::var_os("NANOCODEX_RLM_PROMPTS")
        .map_or_else(|| crate_root.join("prompts"), PathBuf::from);
    let harness = env::var_os("NANOCODEX_RLM_HARNESS")
        .map_or_else(|| crate_root.join("nanocodex.harness.toml"), PathBuf::from);
    let trials = env::var("NANOCODEX_EVAL_TRIALS")
        .ok()
        .map(|value| value.parse::<u16>())
        .transpose()?
        .unwrap_or(3);

    let prompts = PromptPack::load(prompts)?;
    let launch = LaunchSnapshot::new(prompts, HarnessSnapshot::load(harness)?);
    let runtime = RlmRuntime::new(launch);
    let prompt_cache_key = runtime.prompt_cache_key();
    let treatment = format!("rlm-{}", runtime.launch().digest());
    let agent = Nanocodex::builder(OpenAi::new(support::auth()?)?);
    let rlm_agent = agent
        .clone()
        .append_instructions(runtime.launch().root_instructions())
        .prompt_cache_key(prompt_cache_key)
        .shared_prompt_cache();
    let sweep = Sweep::builder()
        .tasks(tasks.clone())
        .trials(trials)
        .agent("baseline", agent.clone())?
        .agent(&treatment, rlm_agent)?
        .build()?;
    let planned = sweep.attempt_count();

    let installer = runtime.tools();
    let tools_treatment = treatment.clone();
    let finalizer_runtime = runtime.clone();
    let finalizer_treatment = treatment.clone();
    let backend = support::vm_resources(tasks)
        .await?
        .backend_with(
            VmBackend::builder()
                .agent_tools_factory(move |agent_id, tools, agent| {
                    if agent_id.is_some_and(|id| id.as_str() == tools_treatment) {
                        installer.install(tools, agent)
                    } else {
                        Ok(tools)
                    }
                })
                .attempt_finalizer(move |agent_id, session_id, directory| {
                    let runtime = finalizer_runtime.clone();
                    let enabled = agent_id
                        .as_ref()
                        .is_some_and(|id| id.as_str() == finalizer_treatment);
                    async move {
                        if !enabled {
                            return Ok::<(), support::AnyError>(());
                        }
                        let root_session_id = session_id.to_string();
                        runtime.finalize_root(&root_session_id).await?;
                        if let Some(evidence) = runtime.evidence(&root_session_id).await {
                            let path = directory.join("agent/rlm-evidence.json");
                            let mut bytes = serde_json::to_vec_pretty(&evidence)?;
                            bytes.push(b'\n');
                            tokio::fs::write(path, bytes).await?;
                        }
                        Ok(())
                    }
                }),
        )
        .await?;
    let evaluator = Evaluator::builder(agent, backend)
        .output_directory(".nanocodex/evals/rlm")
        // Continual refinement is deliberately ordered: concurrent attempts
        // must not race while evolving one durable harness document.
        .max_concurrency(1)
        .resume_incomplete(sweep)
        .build()?;

    let remaining = evaluator.remaining_attempts()?;
    let run = evaluator.sweep();
    let recorder = Harbor::new(&evaluator)?.record(run.events().subscribe())?;
    let results = run.await?;
    let job = recorder.finish_all(remaining).await?;

    println!(
        "completed {}/{} attempts ({} resumed)",
        results.attempts().len(),
        planned,
        results.skipped()
    );
    println!("RLM treatment: {treatment}");
    println!("artifacts: {}", job.directory().display());
    Ok(())
}
