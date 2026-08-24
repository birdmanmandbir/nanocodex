// Modified from clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa for Nanocodex2.
// SPDX-License-Identifier: Apache-2.0

//! Child-agent and directed-message values consumed by the imported TUI.
//!
//! The original definitions live in `tact-subagents` 0.6.6. This module preserves their UI and
//! wire shapes without owning agent construction, turns, tools, mailboxes, or shutdown.

use nanocodex::{Model, agent::events::AgentEvent};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Identifies a child within one root session's task tree.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct AgentId(u64);

impl AgentId {
    /// Creates an identifier from its wire value.
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the wire value.
    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Identifies a directed message within one root session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct MessageId(u64);

impl MessageId {
    /// Creates an identifier from its wire value.
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the wire value.
    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Correlates messages in one two-party conversation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct ThreadId(u64);

impl ThreadId {
    /// Creates an identifier from its wire value.
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the wire value.
    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Identifies the origin of a directed message.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum MessageSender {
    /// The root session that owns the task tree.
    Root,
    /// A child session in the task tree.
    Agent {
        /// The sending child.
        agent_id: AgentId,
    },
}

/// Controls when a directed message interrupts its recipient.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MessagePriority {
    /// Deliver after the recipient's active turn, or start an idle recipient.
    #[default]
    Deferred,
    /// Steer an active turn at its next safe model boundary.
    Urgent,
}

impl MessagePriority {
    /// Returns the stable wire name used in prompts and tool results.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Deferred => "deferred",
            Self::Urgent => "urgent",
        }
    }
}

/// Describes the coordination intent of a directed message.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MessagePurpose {
    /// Replace the recipient's task when the sender has management authority.
    Delegate,
    /// Share ordinary coordination context without replacing the task.
    #[default]
    Coordinate,
    /// Report evidence or a result that may affect another agent's work.
    Finding,
    /// Ask the recipient for information.
    Question,
    /// Answer the message identified by `in_reply_to`.
    Reply,
}

impl MessagePurpose {
    /// Returns the stable wire name used in prompts and tool results.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Delegate => "delegate",
            Self::Coordinate => "coordinate",
            Self::Finding => "finding",
            Self::Question => "question",
            Self::Reply => "reply",
        }
    }
}

/// Reports how a recipient accepted a message.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MessageDisposition {
    /// The message started a new turn on an idle recipient.
    Started,
    /// The message will run after the recipient's active turn.
    Queued,
    /// The message steered the recipient's active turn.
    Steered,
}

/// A bounded directed message between agents in one task tree.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct AgentMessage {
    /// The message identity.
    pub(crate) id: MessageId,
    /// The conversation containing this message.
    pub(crate) thread_id: ThreadId,
    /// The message origin.
    pub(crate) from: MessageSender,
    /// The recipient child.
    pub(crate) to: AgentId,
    /// The requested delivery behavior.
    pub(crate) priority: MessagePriority,
    /// The coordination intent.
    pub(crate) purpose: MessagePurpose,
    /// The prior message answered by this reply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) in_reply_to: Option<MessageId>,
    /// The bounded UTF-8 message body.
    pub(crate) body: String,
}

/// The retained messages in one two-party conversation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct AgentThread {
    /// The thread identity.
    pub(crate) id: ThreadId,
    /// The two endpoints permitted to participate in the thread.
    pub(crate) participants: [MessageSender; 2],
    /// Retained messages in delivery order.
    pub(crate) messages: Vec<AgentMessage>,
}

/// Tracks admission and terminal delivery separately.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum MessageDeliveryState {
    /// The recipient mailbox accepted the message.
    Admitted {
        /// How the recipient accepted the message.
        disposition: MessageDisposition,
    },
    /// The recipient incorporated the message into a turn.
    Delivered {
        /// How the recipient accepted the message.
        disposition: MessageDisposition,
    },
    /// Delivery reached a terminal failure.
    Failed {
        /// A bounded description of the failure.
        error: String,
    },
}

/// A complete thread snapshot emitted when one message changes state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct AgentMessageUpdate {
    /// The message whose delivery state changed.
    pub(crate) message_id: MessageId,
    /// The current retained thread.
    pub(crate) thread: AgentThread,
    /// The message's new delivery state.
    pub(crate) delivery: MessageDeliveryState,
}

/// The lifecycle state of a child session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum AgentStatus {
    /// The child exists but has not started a turn.
    Pending,
    /// The child has an active turn.
    Running,
    /// The child submitted a schema-valid result.
    Completed {
        /// The validated structured result.
        output: serde_json::Value,
    },
    /// The most recent turn was interrupted and the session remains reusable.
    Interrupted,
    /// The most recent turn failed and the session remains reusable.
    Failed {
        /// A bounded description of the failure.
        error: String,
    },
    /// The runtime is stopping the child and rejecting new work.
    Closing,
    /// The child is terminal and cannot be reused.
    Closed,
}

impl AgentStatus {
    /// Returns whether the child still owns or is stopping active work.
    pub(crate) const fn is_active(&self) -> bool {
        matches!(self, Self::Pending | Self::Running | Self::Closing)
    }
}

/// Describes a child session and its position in the task tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentDescriptor {
    /// The child identity within its root session.
    pub(crate) id: AgentId,
    /// The underlying managed-agent identity.
    pub(crate) session_id: String,
    /// The model selected for the child.
    pub(crate) model: Model,
    /// The short specialization assigned by the caller.
    pub(crate) role: String,
    /// The child's current delegated task.
    pub(crate) task: String,
    /// The child that spawned this agent, or `None` for a direct child of the root.
    pub(crate) parent: Option<AgentId>,
}

/// A typed observation projected by a child-agent adapter.
#[derive(Debug)]
pub(crate) enum AgentUpdate {
    /// A child was created or its delegated task changed.
    Added(AgentDescriptor),
    /// The child emitted an agent event.
    Event {
        /// The child that emitted the event.
        id: AgentId,
        /// The underlying session event.
        event: AgentEvent,
    },
    /// A child's lifecycle state changed.
    Status {
        /// The affected child.
        id: AgentId,
        /// The new lifecycle state.
        status: AgentStatus,
    },
    /// A directed message changed delivery state.
    Message(AgentMessageUpdate),
}

/// Associates one runtime update with its owning root session.
pub(crate) struct ScopedAgentUpdate {
    /// The root managed-agent session that owns the task tree.
    pub(crate) root_session_id: String,
    /// The typed runtime observation.
    pub(crate) update: AgentUpdate,
}

/// Identifies one adapter instance so late updates can be rejected.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SubagentRuntimeId(u64);

impl SubagentRuntimeId {
    /// Creates an identity allocated by the owning adapter.
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the adapter-owned identity value.
    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentId, AgentStatus, MessagePriority, MessagePurpose, MessageSender, SubagentRuntimeId,
    };

    #[test]
    fn identities_preserve_wire_and_display_values() {
        let id = AgentId::new(42);
        assert_eq!(id.get(), 42);
        assert_eq!(id.to_string(), "42");
        assert_eq!(serde_json::to_value(id).unwrap(), 42);
        assert_eq!(SubagentRuntimeId::new(9).get(), 9);
    }

    #[test]
    fn active_status_matches_tact_filter_semantics() {
        assert!(AgentStatus::Pending.is_active());
        assert!(AgentStatus::Running.is_active());
        assert!(AgentStatus::Closing.is_active());
        assert!(!AgentStatus::Closed.is_active());
        assert!(!AgentStatus::Interrupted.is_active());
    }

    #[test]
    fn message_wire_names_and_sender_shape_match_tact() {
        assert_eq!(MessagePriority::Urgent.as_str(), "urgent");
        assert_eq!(MessagePurpose::Finding.as_str(), "finding");
        assert_eq!(
            serde_json::to_value(MessageSender::Agent {
                agent_id: AgentId::new(3),
            })
            .unwrap(),
            serde_json::json!({ "kind": "agent", "agent_id": 3 })
        );
    }
}
