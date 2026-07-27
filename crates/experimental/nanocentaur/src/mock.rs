use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use nanocodex_agent::{AgentEvent, AgentEventKind, Prompt, TurnUsage};
use serde::Serialize;
use serde_json::value::to_raw_value;
use tokio::sync::{Mutex, mpsc};

use crate::{
    AgentError, AgentRunResult, AgentSpec, ManagedAgent, ManagedAgentFactory, ManagedTurn,
    ManagedTurnControl, RuntimeEvent, SpawnedAgent,
};

const MOCK_EVENT_CAPACITY: usize = 256;

/// Deterministic harness used by API tests and the no-model-cost concurrency
/// benchmark. It models Nanocodex's per-agent FIFO and exact active-turn
/// steering behavior.
pub struct MockAgentFactory {
    delay: Duration,
    usage: TurnUsage,
}

impl MockAgentFactory {
    /// Creates a deterministic backend with one fixed completion delay.
    #[must_use]
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            usage: TurnUsage::default(),
        }
    }

    /// Replaces the deterministic usage returned by every mock turn.
    #[must_use]
    pub fn usage(mut self, usage: TurnUsage) -> Self {
        self.usage = usage;
        self
    }
}

#[async_trait]
impl ManagedAgentFactory for MockAgentFactory {
    async fn create(&self, spec: AgentSpec) -> Result<SpawnedAgent, AgentError> {
        let (sender, events) = mpsc::channel(MOCK_EVENT_CAPACITY);
        Ok(SpawnedAgent {
            agent: Arc::new(MockAgent {
                id: spec.agent_id,
                delay: self.delay,
                sender,
                serial: Arc::new(Mutex::new(())),
                next_event: Arc::new(AtomicU64::new(1)),
                usage: self.usage.clone(),
            }),
            events,
        })
    }
}

struct MockAgent {
    id: String,
    delay: Duration,
    sender: mpsc::Sender<RuntimeEvent>,
    serial: Arc<Mutex<()>>,
    next_event: Arc<AtomicU64>,
    usage: TurnUsage,
}

#[async_trait]
impl ManagedAgent for MockAgent {
    async fn prompt(&self, prompt: Prompt) -> Result<ManagedTurn, AgentError> {
        let control = Arc::new(MockTurnControl {
            state: AtomicU8::new(0),
            cancelled: AtomicBool::new(false),
            sender: self.sender.clone(),
            next_event: Arc::clone(&self.next_event),
        });
        let run_control = Arc::clone(&control);
        let serial = Arc::clone(&self.serial);
        let sender = self.sender.clone();
        let delay = self.delay;
        let id = self.id.clone();
        let usage = self.usage.clone();
        Ok(ManagedTurn {
            control,
            result: Box::pin(async move {
                let _serial = serial.lock().await;
                run_control.state.store(1, Ordering::Release);
                let _ = sender
                    .send(runtime_event(
                        &run_control.next_event,
                        AgentEventKind::RunStarted,
                        EmptyPayload {},
                    ))
                    .await;
                tokio::time::sleep(delay).await;
                if run_control.cancelled.load(Ordering::Acquire) {
                    let _ = sender
                        .send(runtime_event(
                            &run_control.next_event,
                            AgentEventKind::RunFailed,
                            EmptyPayload {},
                        ))
                        .await;
                    run_control.state.store(2, Ordering::Release);
                    return Err(AgentError::TurnNotCancellable);
                }
                drop(prompt);
                let final_message = format!("mock agent {id} completed");
                let _ = sender
                    .send(runtime_event(
                        &run_control.next_event,
                        AgentEventKind::AssistantMessage,
                        AssistantPayload {
                            content: final_message.clone(),
                        },
                    ))
                    .await;
                let _ = sender
                    .send(runtime_event(
                        &run_control.next_event,
                        AgentEventKind::RunCompleted,
                        EmptyPayload {},
                    ))
                    .await;
                run_control.state.store(2, Ordering::Release);
                Ok(AgentRunResult {
                    final_message,
                    snapshot: None,
                    usage,
                })
            }),
        })
    }
}

struct MockTurnControl {
    state: AtomicU8,
    cancelled: AtomicBool,
    sender: mpsc::Sender<RuntimeEvent>,
    next_event: Arc<AtomicU64>,
}

#[async_trait]
impl ManagedTurnControl for MockTurnControl {
    async fn steer(&self, prompt: Prompt) -> Result<(), AgentError> {
        if self.state.load(Ordering::Acquire) != 1 {
            return Err(AgentError::TurnNotSteerable);
        }
        drop(prompt);
        let _ = self
            .sender
            .send(runtime_event(
                &self.next_event,
                AgentEventKind::RunSteered,
                EmptyPayload {},
            ))
            .await;
        Ok(())
    }

    async fn cancel(&self) -> Result<(), AgentError> {
        if self.state.load(Ordering::Acquire) == 2 {
            return Err(AgentError::TurnNotCancellable);
        }
        self.cancelled.store(true, Ordering::Release);
        Ok(())
    }
}

#[derive(Serialize)]
struct EmptyPayload {}

#[derive(Serialize)]
struct AssistantPayload {
    content: String,
}

fn runtime_event(
    next_event: &AtomicU64,
    kind: AgentEventKind,
    payload: impl Serialize,
) -> RuntimeEvent {
    RuntimeEvent(AgentEvent {
        protocol_version: 1,
        request_id: Arc::from("mock"),
        seq: next_event.fetch_add(1, Ordering::Relaxed),
        kind,
        payload: to_raw_value(&payload)
            .expect("mock event payload is serializable")
            .into(),
    })
}
