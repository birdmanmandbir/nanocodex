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

use crate::{RlmAgentId, RlmRuntimeError, RlmTools, runtime::RuntimeState};

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
                parent: agent,
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

    use crate::{HarnessSnapshot, LaunchSnapshot, PromptPack, RlmRuntime};

    #[test]
    fn tool_definitions_use_launch_loaded_descriptions() {
        let directory = tempdir().unwrap();
        let prompts = directory.path().join("prompts");
        fs::create_dir(&prompts).unwrap();
        fs::write(prompts.join("orchestration.md"), "ROOT GUIDANCE").unwrap();
        fs::write(prompts.join("subagent.md"), "CHILD GUIDANCE").unwrap();
        fs::write(
            prompts.join("tools.toml"),
            "spawn = 'CUSTOM SPAWN'\nlist = 'CUSTOM LIST'\nsend = 'CUSTOM SEND'\nwait = 'CUSTOM WAIT'\ninterrupt = 'CUSTOM INTERRUPT'\nclose = 'CUSTOM CLOSE'\n",
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
        assert!(encoded.contains("general"));
    }
}
