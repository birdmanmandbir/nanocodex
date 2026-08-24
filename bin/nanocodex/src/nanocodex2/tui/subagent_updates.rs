// Modified from clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa for Nanocodex2.
// SPDX-License-Identifier: Apache-2.0

//! Forwarding from subagent runtimes into the TUI event loop.

use nanocodex_subagents::{AgentUpdate, ScopedAgentUpdate, SubagentRuntimeId};
use tokio::sync::mpsc;

pub(crate) struct ForwardedSubagentUpdate {
    pub(crate) runtime_id: SubagentRuntimeId,
    pub(crate) root_session_id: String,
    pub(crate) update: AgentUpdate,
}

pub(crate) fn forward(
    runtime_id: SubagentRuntimeId,
    mut updates: mpsc::UnboundedReceiver<ScopedAgentUpdate>,
    sender: mpsc::UnboundedSender<ForwardedSubagentUpdate>,
) {
    tokio::spawn(async move {
        while let Some(update) = updates.recv().await {
            if sender
                .send(ForwardedSubagentUpdate {
                    runtime_id,
                    root_session_id: update.root_session_id,
                    update: update.update,
                })
                .is_err()
            {
                break;
            }
        }
    });
}
