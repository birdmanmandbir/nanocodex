// Derived from clabby/tact@1d9ccaefd1d8613dab020812af04a91cd9b4c52c (Apache-2.0).
// Modified for Nanocodex's CLI-owned module paths and runtime wiring.

#![allow(dead_code, unused_imports)]

//! Reusable child-agent tools and the typed runtime/UI update boundary.

mod capacity;
mod harness;
mod message;
mod model;
mod runtime;
mod simplify;
mod task_tree;
mod tools;

pub(crate) use model::{
    AgentDescriptor, AgentId, AgentMessage, AgentMessageUpdate, AgentStatus, AgentThread,
    AgentUpdate, MessageDeliveryState, MessageDisposition, MessageId, MessagePriority,
    MessagePurpose, MessageSender, ScopedAgentUpdate, SubagentRuntimeId, ThreadId,
};
use std::sync::Arc;

use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
};

pub(crate) use runtime::{Registry, SubagentControl, channel};
pub(crate) use tools::{SubagentToolSet, install_tools};

pub(crate) const DEFAULT_MAX_SUBAGENTS: usize = 32;
const UPDATE_SUBSCRIBER_CAPACITY: usize = 256;

pub(crate) struct ChildAgents {
    root_session_id: String,
    control: SubagentControl,
    updates: broadcast::Sender<Arc<ScopedAgentUpdate>>,
    update_task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl ChildAgents {
    pub(crate) fn new(
        root_session_id: String,
        control: SubagentControl,
        mut updates: mpsc::UnboundedReceiver<ScopedAgentUpdate>,
    ) -> Arc<Self> {
        let (update_tx, _) = broadcast::channel(UPDATE_SUBSCRIBER_CAPACITY);
        let update_fanout = update_tx.clone();
        let update_task = tokio::spawn(async move {
            while let Some(update) = updates.recv().await {
                // No receiver is the normal headless case. Keep draining the runtime channel so
                // producers never depend on a UI being attached or pay for shared ownership.
                if update_fanout.receiver_count() != 0 {
                    drop(update_fanout.send(Arc::new(update)));
                }
            }
        });
        Arc::new(Self {
            root_session_id,
            control,
            updates: update_tx,
            update_task: tokio::sync::Mutex::new(Some(update_task)),
        })
    }

    pub(crate) fn subscribe_updates(&self) -> broadcast::Receiver<Arc<ScopedAgentUpdate>> {
        self.updates.subscribe()
    }

    pub(crate) async fn shutdown(&self) {
        self.control.close_all(&self.root_session_id).await;
        if let Some(update_task) = self.update_task.lock().await.take() {
            update_task.abort();
            drop(update_task.await);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentStatus, AgentUpdate, ChildAgents, ScopedAgentUpdate, channel};

    #[tokio::test]
    async fn update_drain_fans_out_shared_scoped_updates() {
        let (registry, control, updates) = channel(1);
        let child_agents = ChildAgents::new("root".to_owned(), control, updates);
        let mut first = child_agents.subscribe_updates();
        let mut second = child_agents.subscribe_updates();

        registry
            .updates
            .send(ScopedAgentUpdate {
                root_session_id: "root".to_owned(),
                update: AgentUpdate::Status {
                    id: super::AgentId::new(7),
                    status: AgentStatus::Running,
                },
            })
            .unwrap();

        let first = first.recv().await.unwrap();
        let second = second.recv().await.unwrap();
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert_eq!(first.root_session_id, "root");
        assert!(matches!(
            &first.update,
            AgentUpdate::Status {
                status: AgentStatus::Running,
                ..
            }
        ));

        child_agents.shutdown().await;
    }

    #[tokio::test]
    async fn update_drain_keeps_running_without_tui_subscribers() {
        let (registry, control, updates) = channel(1);
        let child_agents = ChildAgents::new("root".to_owned(), control, updates);

        registry
            .updates
            .send(ScopedAgentUpdate {
                root_session_id: "root".to_owned(),
                update: AgentUpdate::Status {
                    id: super::AgentId::new(1),
                    status: AgentStatus::Running,
                },
            })
            .unwrap();
        tokio::task::yield_now().await;
        assert!(
            !child_agents
                .update_task
                .lock()
                .await
                .as_ref()
                .unwrap()
                .is_finished()
        );

        child_agents.shutdown().await;
    }
}
