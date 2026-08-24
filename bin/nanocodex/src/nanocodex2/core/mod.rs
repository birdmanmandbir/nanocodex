//! Managed-only construction boundary for the Nanocodex2 TUI.

pub(crate) mod extensions;

use crate::{
    app::{
        config::{Config, ReasoningEffort, ReasoningMode},
        error::Result,
    },
    engine::{ManagedAgent, ManagedAgentEvents},
    tui::session::ManagedSessionSnapshot,
};
use nanocodex::Model;
use std::sync::Arc;

pub(crate) struct ConfiguredAgent {
    pub(crate) agent: ManagedAgent,
    pub(crate) events: ManagedAgentEvents,
    pub(crate) instructions: Arc<str>,
    pub(crate) skills: Arc<[extensions::Skill]>,
    pub(crate) memory_enabled: bool,
}

impl ConfiguredAgent {
    pub(crate) async fn from_config(config: &Config) -> Result<Self> {
        Self::from_config_with_session(
            config,
            config.agent().thinking(),
            config.agent().reasoning_mode(),
            Model::Sol,
            None,
            None,
        )
        .await
    }

    pub(crate) async fn from_config_with_model(
        config: &Config,
        effort: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        model: Model,
    ) -> Result<Self> {
        Self::from_config_with_session(config, effort, reasoning_mode, model, None, None).await
    }

    pub(crate) async fn from_config_with_session(
        config: &Config,
        _effort: ReasoningEffort,
        _reasoning_mode: ReasoningMode,
        _model: Model,
        session_id: Option<&str>,
        snapshot: Option<ManagedSessionSnapshot>,
    ) -> Result<Self> {
        let client = crate::client_from_environment().map_err(crate::engine::EngineError::from)?;
        let resume_id = snapshot
            .as_ref()
            .map(|snapshot| snapshot.agent_id.as_str())
            .or(session_id);
        let (agent, events) = match resume_id {
            Some(agent_id) => ManagedAgent::resume(client, agent_id).await?,
            None => ManagedAgent::create(client).await?,
        };
        Ok(Self {
            agent,
            events,
            instructions: Arc::from(MANAGED_INSTRUCTIONS),
            skills: Arc::from([]),
            memory_enabled: config.memory().enabled(),
        })
    }
}

const MANAGED_INSTRUCTIONS: &str = concat!(
    "You are Nanocodex2, a durable managed Nanocodex agent. ",
    "The account-managed service owns your model lifecycle, tools, workspace, and retained history."
);

pub(crate) const IMAGE_RENDERING_INSTRUCTIONS: &str = concat!(
    "When the user asks to show a local image, include a Markdown image link in the response; ",
    "viewing it with a tool does not display it in the conversation. To show it, use Markdown image syntax ",
    "`![alt](absolute-path)` so Nanocodex2 can render it inline. Use an absolute path when the image is ",
    "outside the workspace."
);

pub(crate) const MEMORY_REVIEW_CHECKPOINT: &str = concat!(
    "<memory_review_checkpoint>\n",
    "This fixed Nanocodex2 control text is not user-authored. Treat the preceding later user message as ",
    "high-value feedback. Before the final answer, review the full available conversation for ",
    "durable corrections, rebuttals, preferences, constraints, authorization boundaries, scope ",
    "refinements, or further specification. A repository- or code-specific conclusion is eligible ",
    "when it can improve later changes or reviews and is expensive to rediscover. Name its scope. ",
    "Exclude transient task state and readily searchable facts. For a durable finding, run a fresh ",
    "targeted memory scan and then put, replace, or delete as appropriate. If no durable memory ",
    "change is warranted, continue without a memory call. Complete this review before the final ",
    "answer.\n",
    "</memory_review_checkpoint>"
);
