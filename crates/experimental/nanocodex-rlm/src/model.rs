use nanocodex::agent::events::AgentEvent;
use serde::{Deserialize, Serialize};

use crate::HarnessRefinement;

/// Opaque identity of one retained recursive subagent.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RlmAgentId(pub(crate) Box<str>);

/// Current lifecycle state of one retained subagent.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RlmStatus {
    /// A model turn is currently active.
    Running,
    /// The latest turn completed and the retained child can receive follow-ups.
    Idle,
    /// The latest turn failed; the retained child may still receive a follow-up.
    Failed,
    /// Active work was interrupted and the retained child remains reusable.
    Interrupted,
    /// The child was terminally closed.
    Closed,
}

impl RlmStatus {
    pub(crate) const fn is_wait_ready(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// Compact directory entry returned to orchestrating code.
#[derive(Clone, Debug, Serialize)]
pub struct RlmAgentSummary {
    /// Child identity.
    pub agent_id: RlmAgentId,
    /// Parent child identity, or `None` when the root spawned this child.
    pub parent_agent_id: Option<RlmAgentId>,
    /// Selected harness specification.
    pub specification: Box<str>,
    /// Assigned task.
    pub task: Box<str>,
    /// Current lifecycle state.
    pub status: RlmStatus,
    /// Latest completed assistant message, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_message: Option<Box<str>>,
    /// Latest failure diagnostic, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Box<str>>,
}

/// One bounded message exchanged within a recursive family.
#[derive(Clone, Debug, Serialize)]
pub struct RlmMessage {
    /// Monotonic message identity within the root family.
    pub message_id: u64,
    /// Sending child, or `None` when sent by the root.
    pub from_agent_id: Option<RlmAgentId>,
    /// Receiving child, or `None` when addressed to the root.
    pub to_agent_id: Option<RlmAgentId>,
    /// Complete message body.
    pub message: Box<str>,
}

/// Aggregate provider usage from recursive child turns.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RlmUsage {
    /// Provider-reported input tokens.
    pub input_tokens: u64,
    /// Input tokens served from cache.
    pub cached_input_tokens: u64,
    /// Input tokens newly written into cache.
    pub cache_write_input_tokens: u64,
    /// Provider-reported output tokens.
    pub output_tokens: u64,
    /// Reasoning tokens included in output tokens.
    pub reasoning_output_tokens: u64,
    /// Provider-reported total tokens.
    pub total_tokens: u64,
    /// Exact locally estimated cost in billionths of one USD.
    pub estimated_nano_usd: u64,
    /// Number of turns whose provider usage was unavailable.
    pub incomplete_turns: u64,
}

impl RlmUsage {
    pub(crate) fn add_turn(&mut self, usage: &nanocodex::TurnUsage) {
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens());
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens());
        self.cache_write_input_tokens = self
            .cache_write_input_tokens
            .saturating_add(usage.cache_write_input_tokens());
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens());
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(usage.reasoning_output_tokens());
        self.total_tokens = self.total_tokens.saturating_add(usage.total_tokens());
        match usage.estimated_cost() {
            Some(cost) => {
                self.estimated_nano_usd = self
                    .estimated_nano_usd
                    .saturating_add(cost.amount().nano_usd());
            }
            None => self.incomplete_turns = self.incomplete_turns.saturating_add(1),
        }
    }

    pub(crate) const fn add(&mut self, usage: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens);
        self.cache_write_input_tokens = self
            .cache_write_input_tokens
            .saturating_add(usage.cache_write_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(usage.reasoning_output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(usage.total_tokens);
        self.estimated_nano_usd = self
            .estimated_nano_usd
            .saturating_add(usage.estimated_nano_usd);
        self.incomplete_turns = self.incomplete_turns.saturating_add(usage.incomplete_turns);
    }
}

/// One completed or failed child turn retained independently from raw events.
#[derive(Clone, Debug, Serialize)]
pub struct RlmTurnEvidence {
    /// Monotonic turn generation for this child.
    pub generation: u64,
    /// Prompt delivered to the clean or retained child.
    pub prompt: Box<str>,
    /// Final assistant message for a successful turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_message: Option<Box<str>>,
    /// Complete failure diagnostic for an unsuccessful turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Box<str>>,
    /// Exact usage for this child turn.
    pub usage: RlmUsage,
}

/// Retained evidence for one child in the recursive tree.
#[derive(Clone, Debug, Serialize)]
pub struct RlmAgentEvidence {
    /// Current compact descriptor.
    pub agent: RlmAgentSummary,
    /// Every observed turn in generation order.
    pub turns: Vec<RlmTurnEvidence>,
}

/// Complete process-local recursive evidence associated with one root session.
#[derive(Clone, Debug, Serialize)]
pub struct RlmEvidence {
    /// Root Nanocodex session identity.
    pub root_session_id: Box<str>,
    /// Digest of launch prompts plus the frozen harness.
    pub launch_digest: Box<str>,
    /// Frozen harness revision.
    pub harness_revision: u64,
    /// Latest durable harness revision when evidence was projected.
    pub final_harness_revision: u64,
    /// Digest of the latest durable harness document.
    pub final_harness_digest: Box<str>,
    /// Ordered refinements visible at evidence projection time.
    pub refinements: Vec<HarnessRefinement>,
    /// Recursive children in creation order.
    pub agents: Vec<RlmAgentEvidence>,
    /// All messages retained for the family.
    pub messages: Vec<RlmMessage>,
    /// Complete child event stream, tagged with tree identity.
    pub events: Vec<RlmEvent>,
    /// Aggregate usage across every completed child turn.
    pub usage: RlmUsage,
}

/// One recursive runtime update suitable for a UI or evaluator adapter.
#[derive(Clone, Debug, Serialize)]
pub struct RlmEvent {
    /// Root family identity.
    pub root_session_id: Box<str>,
    /// Child associated with this update.
    pub agent_id: RlmAgentId,
    /// Runtime-specific update body.
    pub kind: RlmEventKind,
}

/// Runtime and raw Nanocodex updates emitted by recursive children.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RlmEventKind {
    /// A child entered the registry.
    Added(RlmAgentSummary),
    /// A child lifecycle state changed.
    Status(RlmStatus),
    /// One complete typed child-agent event.
    Agent(AgentEvent),
    /// One message was committed to the family mailbox.
    Message(RlmMessage),
    /// One child turn reached a terminal boundary.
    Turn(RlmTurnEvidence),
}

impl RlmAgentId {
    pub(crate) fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string().into_boxed_str())
    }

    /// Returns the opaque string identity.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RlmAgentId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}
