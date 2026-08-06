use std::{sync::Arc, time::Duration};

use nanocodex::{
    Tool, Tools,
    agent::AgentHandle,
    tools::{
        ToolContext, ToolDefinition, ToolInput, ToolOutput, ToolResult, ToolsBuildError,
        contract::async_trait,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{RlmAgentId, RlmRuntimeError, RlmTools, harness::HarnessEdit, runtime::RuntimeState};

const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_WAIT_TIMEOUT: Duration = Duration::from_secs(300);

impl RlmTools {
    /// Adds the recursive operations to one fresh agent-relative tool collection.
    ///
    /// # Errors
    ///
    /// Returns the standard tool-registry error for duplicate or invalid names.
    pub fn install(&self, tools: Tools, agent: AgentHandle) -> Result<Tools, ToolsBuildError> {
        tools
            .into_builder()
            .tool(SpawnAgent {
                parent: agent.clone(),
                state: self.state.clone(),
            })
            .tool(ListAgents {
                state: self.state.clone(),
            })
            .tool(SendAgentMessage {
                state: self.state.clone(),
            })
            .tool(WaitAgent {
                state: self.state.clone(),
            })
            .tool(ChangeLifecycle {
                state: self.state.clone(),
                operation: LifecycleOperation::Interrupt,
            })
            .tool(ChangeLifecycle {
                state: self.state.clone(),
                operation: LifecycleOperation::Close,
            })
            .tool(HarnessState {
                state: self.state.clone(),
            })
            .tool(HarnessMutation {
                agent: agent.clone(),
                state: self.state.clone(),
                operation: HarnessMutationOperation::Apply,
            })
            .tool(HarnessMutation {
                agent: agent.clone(),
                state: self.state.clone(),
                operation: HarnessMutationOperation::Rollback,
            })
            .tool(RefineHarness {
                parent: agent,
                state: self.state.clone(),
            })
            .build()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnArgs {
    specification: String,
    task: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    include_closed: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    to: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    #[serde(default)]
    agent_ids: Vec<RlmAgentId>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetArgs {
    agent_id: RlmAgentId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessApplyArgs {
    trigger: String,
    edit: HarnessEdit,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessRollbackArgs {
    revision: u64,
    trigger: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefineArgs {
    observation: String,
}

#[derive(Serialize)]
struct WaitReport {
    agents: Vec<crate::RlmAgentSummary>,
    messages: Vec<crate::RlmMessage>,
    timed_out: bool,
}

#[derive(Serialize)]
struct LifecycleReport {
    agents: Vec<crate::RlmAgentSummary>,
}

#[derive(Serialize)]
struct HarnessMutationReport {
    revision: u64,
    digest: Box<str>,
    operation: Box<str>,
    context_queued_for_agents: usize,
    context_queue_failures: Vec<String>,
}

struct SpawnAgent {
    parent: AgentHandle,
    state: std::sync::Weak<RuntimeState>,
}

struct ListAgents {
    state: std::sync::Weak<RuntimeState>,
}

struct SendAgentMessage {
    state: std::sync::Weak<RuntimeState>,
}

struct WaitAgent {
    state: std::sync::Weak<RuntimeState>,
}

struct HarnessState {
    state: std::sync::Weak<RuntimeState>,
}

struct HarnessMutation {
    agent: AgentHandle,
    state: std::sync::Weak<RuntimeState>,
    operation: HarnessMutationOperation,
}

struct RefineHarness {
    parent: AgentHandle,
    state: std::sync::Weak<RuntimeState>,
}

#[derive(Clone, Copy)]
enum HarnessMutationOperation {
    Apply,
    Rollback,
}

#[derive(Clone, Copy)]
enum LifecycleOperation {
    Interrupt,
    Close,
}

struct ChangeLifecycle {
    state: std::sync::Weak<RuntimeState>,
    operation: LifecycleOperation,
}

#[async_trait]
impl Tool for SpawnAgent {
    fn definition(&self) -> ToolDefinition {
        spawn_definition(&self.state)
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let args: SpawnArgs = input.decode_json()?;
        let state = upgrade(&self.state)?;
        let summary = state
            .spawn(
                &self.parent,
                context.session_id(),
                &args.specification,
                args.task,
            )
            .await?;
        json_output(summary)
    }
}

fn spawn_definition(state: &std::sync::Weak<RuntimeState>) -> ToolDefinition {
    let description = state.upgrade().map_or_else(
        || "Start one clean recursive subagent.".to_owned(),
        |state| {
            format!(
                "{}\n\n{}",
                state.launch.root_instructions(),
                state.launch.prompts().tools().spawn()
            )
        },
    );
    ToolDefinition::function(
        "spawn_agent",
        description,
        json!({
            "type": "object",
            "properties": {
                "specification": {
                    "type": "string",
                    "description": "Enabled specification ID listed in this tool's launch guidance."
                },
                "task": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Complete focused task for the clean child."
                }
            },
            "required": ["specification", "task"],
            "additionalProperties": false
        }),
    )
}

#[async_trait]
impl Tool for ListAgents {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "list_agents",
            description(&self.state, |state| state.launch.prompts().tools().list()),
            json!({
                "type": "object",
                "properties": {
                    "include_closed": {
                        "type": "boolean",
                        "default": false
                    }
                },
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let args: ListArgs = input.decode_json()?;
        let state = upgrade(&self.state)?;
        json_output(state.list(context.session_id(), args.include_closed).await)
    }
}

#[async_trait]
impl Tool for SendAgentMessage {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "send_agent_message",
            description(&self.state, |state| state.launch.prompts().tools().send()),
            json!({
                "type": "object",
                "properties": {
                    "to": {
                        "type": "string",
                        "minLength": 1,
                        "description": "A retained agent ID, or `parent` when called by a child."
                    },
                    "message": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 2048
                    }
                },
                "required": ["to", "message"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let args: SendArgs = input.decode_json()?;
        let state = upgrade(&self.state)?;
        let message = state
            .send(context.session_id(), &args.to, args.message)
            .await?;
        json_output(message)
    }
}

#[async_trait]
impl Tool for WaitAgent {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "wait_agent",
            description(&self.state, |state| state.launch.prompts().tools().wait()),
            json!({
                "type": "object",
                "properties": {
                    "agent_ids": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1 },
                        "description": "Selected agent IDs. May be empty when waiting only for inbound messages."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 300000,
                        "description": "Defaults to 30000 milliseconds."
                    }
                },
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let args: WaitArgs = input.decode_json()?;
        let state = upgrade(&self.state)?;
        let timeout = args
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_WAIT_TIMEOUT)
            .min(MAX_WAIT_TIMEOUT);
        let (agents, messages, timed_out) = state
            .wait(context.session_id(), &args.agent_ids, timeout)
            .await?;
        json_output(WaitReport {
            agents,
            messages,
            timed_out,
        })
    }
}

#[async_trait]
impl Tool for ChangeLifecycle {
    fn definition(&self) -> ToolDefinition {
        let (name, description) = match self.operation {
            LifecycleOperation::Interrupt => (
                "interrupt_agent",
                description(&self.state, |state| {
                    state.launch.prompts().tools().interrupt()
                }),
            ),
            LifecycleOperation::Close => (
                "close_agent",
                description(&self.state, |state| state.launch.prompts().tools().close()),
            ),
        };
        ToolDefinition::function(
            name,
            description,
            json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string", "minLength": 1 }
                },
                "required": ["agent_id"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let args: TargetArgs = input.decode_json()?;
        let state = upgrade(&self.state)?;
        let agents = match self.operation {
            LifecycleOperation::Interrupt => {
                state
                    .interrupt(context.session_id(), &args.agent_id)
                    .await?
            }
            LifecycleOperation::Close => state.close(context.session_id(), &args.agent_id).await?,
        };
        json_output(LifecycleReport { agents })
    }
}

#[async_trait]
impl Tool for HarnessState {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "harness_state",
            description(&self.state, |state| {
                state.launch.prompts().tools().harness_state()
            }),
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
        let _: serde_json::Map<String, serde_json::Value> = input.decode_json()?;
        let state = upgrade(&self.state)?;
        json_output(state.harness_snapshot().await)
    }
}

#[async_trait]
impl Tool for HarnessMutation {
    fn definition(&self) -> ToolDefinition {
        match self.operation {
            HarnessMutationOperation::Apply => ToolDefinition::function(
                "harness_apply",
                description(&self.state, |state| {
                    state.launch.prompts().tools().harness_apply()
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "trigger": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Concrete trajectory evidence motivating this one small change."
                        },
                        "edit": harness_edit_schema()
                    },
                    "required": ["trigger", "edit"],
                    "additionalProperties": false
                }),
            ),
            HarnessMutationOperation::Rollback => ToolDefinition::function(
                "harness_rollback",
                description(&self.state, |state| {
                    state.launch.prompts().tools().harness_rollback()
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "revision": { "type": "integer", "minimum": 0 },
                        "trigger": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Observed regression motivating rollback."
                        }
                    },
                    "required": ["revision", "trigger"],
                    "additionalProperties": false
                }),
            ),
        }
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let state = upgrade(&self.state)?;
        let (revision, queued, failures) = match self.operation {
            HarnessMutationOperation::Apply => {
                let args: HarnessApplyArgs = input.decode_json()?;
                state
                    .apply_harness_edit(
                        context.session_id(),
                        self.agent.clone(),
                        args.edit,
                        args.trigger,
                    )
                    .await?
            }
            HarnessMutationOperation::Rollback => {
                let args: HarnessRollbackArgs = input.decode_json()?;
                state
                    .rollback_harness(
                        context.session_id(),
                        self.agent.clone(),
                        args.revision,
                        args.trigger,
                    )
                    .await?
            }
        };
        json_output(HarnessMutationReport {
            revision: revision.revision,
            digest: revision.digest,
            operation: revision.operation,
            context_queued_for_agents: queued,
            context_queue_failures: failures,
        })
    }
}

#[async_trait]
impl Tool for RefineHarness {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "refine_harness",
            description(&self.state, |state| state.launch.prompts().tools().refine()),
            json!({
                "type": "object",
                "properties": {
                    "observation": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Concrete success, failure, or repeated tactic from the current trajectory."
                    }
                },
                "required": ["observation"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let args: RefineArgs = input.decode_json()?;
        let state = upgrade(&self.state)?;
        let summary = state
            .refine(&self.parent, context.session_id(), args.observation)
            .await?;
        json_output(summary)
    }
}

fn harness_edit_schema() -> serde_json::Value {
    let enabled = json!({ "type": "boolean" });
    let text = json!({ "type": "string", "minLength": 1 });
    let create = |operation: &str,
                  properties: serde_json::Map<String, serde_json::Value>,
                  required: Vec<&str>| {
        let mut all = serde_json::Map::new();
        all.insert("operation".to_owned(), json!({ "const": operation }));
        all.extend(properties);
        let mut required = required;
        required.insert(0, "operation");
        json!({
            "type": "object",
            "properties": all,
            "required": required,
            "additionalProperties": false
        })
    };
    let fields = |pairs: &[(&str, serde_json::Value)]| {
        pairs
            .iter()
            .map(|(name, schema)| ((*name).to_owned(), schema.clone()))
            .collect::<serde_json::Map<_, _>>()
    };
    json!({
        "oneOf": [
            create("create_prompt_note", fields(&[("id", text.clone()), ("text", text.clone()), ("enabled", enabled.clone())]), vec!["id", "text"]),
            create("update_prompt_note", fields(&[("id", text.clone()), ("text", text.clone()), ("enabled", enabled.clone())]), vec!["id"]),
            create("delete_prompt_note", fields(&[("id", text.clone())]), vec!["id"]),
            create("create_memory", fields(&[("id", text.clone()), ("name", text.clone()), ("content", text.clone()), ("enabled", enabled.clone())]), vec!["id", "name", "content"]),
            create("update_memory", fields(&[("id", text.clone()), ("name", text.clone()), ("content", text.clone()), ("enabled", enabled.clone())]), vec!["id"]),
            create("delete_memory", fields(&[("id", text.clone())]), vec!["id"]),
            create("create_skill", fields(&[("id", text.clone()), ("name", text.clone()), ("description", text.clone()), ("instructions", text.clone()), ("enabled", enabled.clone())]), vec!["id", "name", "description", "instructions"]),
            create("update_skill", fields(&[("id", text.clone()), ("name", text.clone()), ("description", text.clone()), ("instructions", text.clone()), ("enabled", enabled.clone())]), vec!["id"]),
            create("delete_skill", fields(&[("id", text.clone())]), vec!["id"]),
            create("create_subagent", fields(&[("id", text.clone()), ("name", text.clone()), ("description", text.clone()), ("instructions", text.clone()), ("enabled", enabled.clone())]), vec!["id", "name", "description", "instructions"]),
            create("update_subagent", fields(&[("id", text.clone()), ("name", text.clone()), ("description", text.clone()), ("instructions", text.clone()), ("enabled", enabled)]), vec!["id"]),
            create("delete_subagent", fields(&[("id", text)]), vec!["id"])
        ]
    })
}

fn upgrade(state: &std::sync::Weak<RuntimeState>) -> Result<Arc<RuntimeState>, RlmRuntimeError> {
    state.upgrade().ok_or(RlmRuntimeError::Closed)
}

fn description(
    state: &std::sync::Weak<RuntimeState>,
    select: impl FnOnce(&RuntimeState) -> &str,
) -> String {
    state.upgrade().map_or_else(
        || "Recursive runtime is closed.".to_owned(),
        |state| select(&state).to_owned(),
    )
}

fn json_output(value: impl Serialize) -> ToolResult {
    let value = serde_json::to_value(value)?;
    Ok(ToolOutput::from_json(value, true))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use crate::{HarnessSnapshot, LaunchSnapshot, PromptPack, RlmRuntime, harness::HarnessEdit};

    #[test]
    fn tool_definitions_use_launch_loaded_descriptions() {
        let directory = tempdir().unwrap();
        let prompts = directory.path().join("prompts");
        fs::create_dir(&prompts).unwrap();
        fs::write(prompts.join("orchestration.md"), "ROOT GUIDANCE").unwrap();
        fs::write(prompts.join("subagent.md"), "CHILD GUIDANCE").unwrap();
        fs::write(prompts.join("refiner.md"), "REFINER GUIDANCE").unwrap();
        fs::write(
            prompts.join("tools.toml"),
            "spawn = 'CUSTOM SPAWN'\nlist = 'CUSTOM LIST'\nsend = 'CUSTOM SEND'\nwait = 'CUSTOM WAIT'\ninterrupt = 'CUSTOM INTERRUPT'\nclose = 'CUSTOM CLOSE'\nharness_state = 'CUSTOM STATE'\nharness_apply = 'CUSTOM APPLY'\nharness_rollback = 'CUSTOM ROLLBACK'\nrefine = 'CUSTOM REFINE'\n",
        )
        .unwrap();
        let harness = directory.path().join("harness.toml");
        fs::write(
            &harness,
            "version = 1\nrevision = 0\n[[subagents]]\nid = 'general'\nname = 'General'\ndescription = 'General work'\ninstructions = 'Inspect'\n",
        )
        .unwrap();
        let runtime = RlmRuntime::new(LaunchSnapshot::new(
            PromptPack::load(prompts).unwrap(),
            HarnessSnapshot::load(harness).unwrap(),
        ));
        let definition = super::spawn_definition(&runtime.tools().state);
        let encoded = serde_json::to_string(&definition).unwrap();
        assert!(encoded.contains("ROOT GUIDANCE"));
        assert!(encoded.contains("CUSTOM SPAWN"));
        assert!(!encoded.contains("General work"));
    }

    #[tokio::test]
    async fn harness_refinement_does_not_change_the_cached_tool_prefix() {
        let directory = tempdir().unwrap();
        let prompts = directory.path().join("prompts");
        fs::create_dir(&prompts).unwrap();
        fs::write(prompts.join("orchestration.md"), "STABLE ROOT").unwrap();
        fs::write(prompts.join("subagent.md"), "STABLE CHILD").unwrap();
        fs::write(prompts.join("refiner.md"), "STABLE REFINER").unwrap();
        fs::write(
            prompts.join("tools.toml"),
            "spawn='spawn'\nlist='list'\nsend='send'\nwait='wait'\ninterrupt='interrupt'\nclose='close'\nharness_state='state'\nharness_apply='apply'\nharness_rollback='rollback'\nrefine='refine'\n",
        )
        .unwrap();
        let harness = directory.path().join("harness.toml");
        fs::write(
            &harness,
            "version=1\nrevision=0\n[[subagents]]\nid='general'\nname='General'\ndescription='General work'\ninstructions='Inspect'\n",
        )
        .unwrap();
        let runtime = RlmRuntime::new(LaunchSnapshot::new(
            PromptPack::load(prompts).unwrap(),
            HarnessSnapshot::load(harness).unwrap(),
        ));
        let launch_digest = runtime.launch().digest().to_owned();
        let before = serde_json::to_vec(&super::spawn_definition(&runtime.tools().state)).unwrap();

        runtime
            .state
            .harness
            .apply(
                HarnessEdit::CreateSubagent {
                    id: "reviewer".to_owned(),
                    name: "Reviewer".to_owned(),
                    description: "Reviews changes".to_owned(),
                    instructions: "Inspect the diff".to_owned(),
                    enabled: true,
                },
                "a review caught a regression".to_owned(),
            )
            .await
            .unwrap();

        let after = serde_json::to_vec(&super::spawn_definition(&runtime.tools().state)).unwrap();
        assert_eq!(before, after);
        assert_eq!(runtime.launch().digest(), launch_digest);
        assert_eq!(runtime.harness().await.revision(), 1);
    }
}
