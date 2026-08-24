//! Application-private lifecycle adapter for account-owned managed agents.
//!
//! This module deliberately adapts the managed HTTP/SSE client. It never
//! constructs an in-process Nanocodex agent or reads provider credentials.

use std::{
    collections::{BTreeSet, HashMap},
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use nanocodex::agent::events::AgentEvent;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

use super::client::{
    AgentState, EventCursor, ManagedClient, ManagedError, ManagedEventData, ManagedEventStream,
    PromptInput, TurnState, TurnView,
};

/// Stable managed identity shared by a handle and all of its event receivers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedIdentity {
    agent_id: Arc<str>,
    session_id: Arc<str>,
}

impl ManagedIdentity {
    pub(crate) fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// A capability intentionally absent from the current managed API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnsupportedCapability {
    /// Creating another managed agent from this agent's retained history.
    ForkRetainedHistory,
    ThinkingUpdate,
    FastModeUpdate,
}

impl std::fmt::Display for UnsupportedCapability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ForkRetainedHistory => {
                formatter.write_str("forking a managed agent from retained history")
            }
            Self::ThinkingUpdate => formatter.write_str("updating managed reasoning effort"),
            Self::FastModeUpdate => formatter.write_str("updating managed fast mode"),
        }
    }
}

/// Typed failures at the managed lifecycle boundary.
#[derive(Clone, Debug, thiserror::Error)]
pub(crate) enum EngineError {
    #[error(transparent)]
    Managed(Arc<ManagedError>),
    #[error("managed lifecycle stopped")]
    Shutdown,
    #[error("managed event stream stopped: {0}")]
    Stream(String),
    #[error("managed turn {turn_id} {state}: {message}")]
    Turn {
        turn_id: String,
        state: &'static str,
        message: String,
    },
    #[error("there is no active managed turn")]
    NoActiveTurn,
    #[error("active managed turn is ambiguous ({count} unfinished turns)")]
    AmbiguousActiveTurn { count: usize },
    #[error("unsupported managed capability: {0}")]
    Unsupported(UnsupportedCapability),
    #[error("managed lifecycle response is inconsistent: {0}")]
    InvalidResponse(&'static str),
}

impl From<ManagedError> for EngineError {
    fn from(error: ManagedError) -> Self {
        Self::Managed(Arc::new(error))
    }
}

/// Caller-selected identities for idempotent prompt admission.
#[derive(Clone, Debug)]
pub(crate) struct ManagedPrompt {
    pub(crate) input: PromptInput,
    pub(crate) turn_id: String,
    pub(crate) idempotency_key: String,
}

impl ManagedPrompt {
    pub(crate) fn new(input: PromptInput) -> Self {
        Self {
            input,
            turn_id: format!("turn-{}", Uuid::new_v4()),
            idempotency_key: format!("ncx-{}", Uuid::new_v4()),
        }
    }

    pub(crate) fn with_ids(
        input: PromptInput,
        turn_id: impl Into<String>,
        idempotency_key: impl Into<String>,
    ) -> Self {
        Self {
            input,
            turn_id: turn_id.into(),
            idempotency_key: idempotency_key.into(),
        }
    }
}

/// Completed managed turn data retained independently from the event receiver.
#[derive(Clone, Debug)]
pub(crate) struct ManagedTurnResult {
    turn_id: String,
    final_message: String,
    usage: Option<Value>,
    usage_error: Option<String>,
}

impl ManagedTurnResult {
    pub(crate) fn turn_id(&self) -> &str {
        &self.turn_id
    }

    pub(crate) fn final_message(&self) -> &str {
        &self.final_message
    }

    pub(crate) fn into_final_message(self) -> String {
        self.final_message
    }

    pub(crate) fn usage(&self) -> Option<&Value> {
        self.usage.as_ref()
    }

    pub(crate) fn usage_error(&self) -> Option<&str> {
        self.usage_error.as_deref()
    }
}

/// One accepted prompt whose terminal result can be awaited independently.
pub(crate) struct ManagedTurn {
    turn_id: Arc<str>,
    completion: oneshot::Receiver<Result<ManagedTurnResult, EngineError>>,
    control: ManagedTurnControl,
}

impl ManagedTurn {
    pub(crate) fn turn_id(&self) -> &str {
        &self.turn_id
    }

    pub(crate) fn control(&self) -> ManagedTurnControl {
        self.control.clone()
    }
}

impl Future for ManagedTurn {
    type Output = Result<ManagedTurnResult, EngineError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.completion)
            .poll(context)
            .map(|result| result.unwrap_or(Err(EngineError::Shutdown)))
    }
}

/// Turn-specific live control that remains valid independently of the turn future.
#[derive(Clone)]
pub(crate) struct ManagedTurnControl {
    turn_id: Arc<str>,
    runtime: RuntimeHandle,
}

impl ManagedTurnControl {
    pub(crate) fn turn_id(&self) -> &str {
        &self.turn_id
    }

    pub(crate) async fn steer(&self, input: PromptInput) -> Result<(), EngineError> {
        self.runtime
            .request(|reply| Command::Steer {
                turn_id: self.turn_id.to_string(),
                input,
                reply,
            })
            .await
    }

    pub(crate) async fn cancel(&self) -> Result<(), EngineError> {
        self.runtime
            .request(|reply| Command::Cancel {
                turn_id: self.turn_id.to_string(),
                reply,
            })
            .await
    }
}

/// A canonical typed event receiver replayed from this adapter's attach point.
pub(crate) struct ManagedAgentEvents {
    identity: ManagedIdentity,
    receiver: mpsc::UnboundedReceiver<AgentEvent>,
}

impl ManagedAgentEvents {
    pub(crate) fn identity(&self) -> &ManagedIdentity {
        &self.identity
    }

    pub(crate) async fn recv(&mut self) -> Option<AgentEvent> {
        self.receiver.recv().await
    }

    pub(crate) fn try_recv(&mut self) -> Option<AgentEvent> {
        self.receiver.try_recv().ok()
    }
}

/// Cheap cloneable handle to one account-owned managed-agent lifecycle.
#[derive(Clone)]
pub(crate) struct ManagedAgent {
    identity: ManagedIdentity,
    runtime: RuntimeHandle,
}

impl ManagedAgent {
    /// Creates and attaches to a new durable managed agent.
    pub(crate) async fn create(
        client: ManagedClient,
    ) -> Result<(Self, ManagedAgentEvents), EngineError> {
        let receipt = client.create().await?;
        let state = client.state(&receipt.agent_id).await?;
        if state.agent_id != receipt.agent_id || state.session_id != receipt.session_id {
            return Err(EngineError::InvalidResponse(
                "create receipt and agent state identities differ",
            ));
        }
        Self::attach(client, state)
    }

    /// Attaches to an existing account-owned durable managed agent.
    pub(crate) async fn resume(
        client: ManagedClient,
        agent_id: &str,
    ) -> Result<(Self, ManagedAgentEvents), EngineError> {
        let state = client.state(agent_id).await?;
        if state.agent_id != agent_id {
            return Err(EngineError::InvalidResponse(
                "requested and returned agent identities differ",
            ));
        }
        Self::attach(client, state)
    }

    fn attach(
        client: ManagedClient,
        state: AgentState,
    ) -> Result<(Self, ManagedAgentEvents), EngineError> {
        let identity = ManagedIdentity {
            agent_id: Arc::from(state.agent_id.as_str()),
            session_id: Arc::from(state.session_id.as_str()),
        };
        let cursor = EventCursor::parse(state.latest_event_cursor)?;
        let stream = client.events(&state.agent_id, cursor)?;
        let (commands, receiver) = mpsc::channel(32);
        let (status_tx, status_rx) = watch::channel(None);
        let runtime = RuntimeHandle {
            commands,
            status: status_rx,
        };
        let driver_runtime = DriverRuntime {
            commands: runtime.commands.downgrade(),
            status: runtime.status.clone(),
        };
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let active_turns = state.active_turns.into_iter().collect();
        tokio::spawn(run_driver(
            client,
            identity.clone(),
            driver_runtime,
            stream,
            receiver,
            status_tx,
            active_turns,
            event_tx,
        ));
        Ok((
            Self {
                identity: identity.clone(),
                runtime,
            },
            ManagedAgentEvents {
                identity,
                receiver: event_rx,
            },
        ))
    }

    pub(crate) fn identity(&self) -> &ManagedIdentity {
        &self.identity
    }

    /// Accepts a prompt and returns a separately awaitable terminal result.
    pub(crate) async fn prompt(&self, input: PromptInput) -> Result<ManagedTurn, EngineError> {
        self.prompt_request(ManagedPrompt::new(input)).await
    }

    /// Accepts an idempotent prompt with caller-owned turn and submission IDs.
    pub(crate) async fn prompt_request(
        &self,
        prompt: ManagedPrompt,
    ) -> Result<ManagedTurn, EngineError> {
        self.runtime
            .request(|reply| Command::Prompt { prompt, reply })
            .await
    }

    /// Recovers an independently awaitable result for a retained turn.
    pub(crate) async fn resume_turn(&self, turn_id: &str) -> Result<ManagedTurn, EngineError> {
        self.runtime
            .request(|reply| Command::ResumeTurn {
                turn_id: turn_id.to_owned(),
                reply,
            })
            .await
    }

    /// Returns turn-specific live control when exactly one turn is unfinished.
    pub(crate) async fn active_turn(&self) -> Result<ManagedTurnControl, EngineError> {
        self.runtime
            .request(|reply| Command::Active { reply })
            .await
    }

    pub(crate) async fn steer(&self, input: PromptInput) -> Result<(), EngineError> {
        self.active_turn().await?.steer(input).await
    }

    pub(crate) async fn cancel(&self) -> Result<(), EngineError> {
        self.active_turn().await?.cancel().await
    }

    /// Adds a receiver after replaying every canonical event observed since attach.
    pub(crate) async fn subscribe(&self) -> Result<ManagedAgentEvents, EngineError> {
        let receiver = self
            .runtime
            .request(|reply| Command::Subscribe { reply })
            .await?;
        Ok(ManagedAgentEvents {
            identity: self.identity.clone(),
            receiver,
        })
    }

    /// Creates an independent fresh managed agent for auxiliary work.
    pub(crate) async fn spawn(&self) -> Result<(ManagedAgent, ManagedAgentEvents), EngineError> {
        self.runtime.request(|reply| Command::Spawn { reply }).await
    }

    /// The managed service currently has no create-from-history operation.
    pub(crate) async fn fork(&self) -> Result<(ManagedAgent, ManagedAgentEvents), EngineError> {
        Err(EngineError::Unsupported(
            UnsupportedCapability::ForkRetainedHistory,
        ))
    }

    pub(crate) async fn set_thinking(
        &self,
        _thinking: nanocodex::Thinking,
    ) -> Result<(), EngineError> {
        Err(EngineError::Unsupported(
            UnsupportedCapability::ThinkingUpdate,
        ))
    }

    pub(crate) async fn set_fast_mode(&self, _enabled: bool) -> Result<(), EngineError> {
        Err(EngineError::Unsupported(
            UnsupportedCapability::FastModeUpdate,
        ))
    }

    /// Stops only this local adapter. The durable remote agent is retained.
    pub(crate) async fn shutdown(&self) -> Result<(), EngineError> {
        self.runtime
            .request(|reply| Command::Shutdown { reply })
            .await
    }
}

#[derive(Clone)]
struct RuntimeHandle {
    commands: mpsc::Sender<Command>,
    status: watch::Receiver<Option<EngineError>>,
}

struct DriverRuntime {
    commands: mpsc::WeakSender<Command>,
    status: watch::Receiver<Option<EngineError>>,
}

impl DriverRuntime {
    fn handle(&self) -> Result<RuntimeHandle, EngineError> {
        let commands = self.commands.upgrade().ok_or(EngineError::Shutdown)?;
        Ok(RuntimeHandle {
            commands,
            status: self.status.clone(),
        })
    }
}

impl RuntimeHandle {
    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, EngineError>>) -> Command,
    ) -> Result<T, EngineError> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(command(reply))
            .await
            .map_err(|_| self.stopped_error())?;
        result.await.map_err(|_| self.stopped_error())?
    }

    fn stopped_error(&self) -> EngineError {
        self.status
            .borrow()
            .clone()
            .unwrap_or(EngineError::Shutdown)
    }
}

enum Command {
    Prompt {
        prompt: ManagedPrompt,
        reply: Reply<ManagedTurn>,
    },
    ResumeTurn {
        turn_id: String,
        reply: Reply<ManagedTurn>,
    },
    Active {
        reply: Reply<ManagedTurnControl>,
    },
    Steer {
        turn_id: String,
        input: PromptInput,
        reply: Reply<()>,
    },
    Cancel {
        turn_id: String,
        reply: Reply<()>,
    },
    Subscribe {
        reply: Reply<mpsc::UnboundedReceiver<AgentEvent>>,
    },
    Spawn {
        reply: Reply<(ManagedAgent, ManagedAgentEvents)>,
    },
    Shutdown {
        reply: Reply<()>,
    },
}

type Reply<T> = oneshot::Sender<Result<T, EngineError>>;
type Completion = oneshot::Sender<Result<ManagedTurnResult, EngineError>>;

async fn run_driver(
    client: ManagedClient,
    identity: ManagedIdentity,
    runtime: DriverRuntime,
    mut stream: ManagedEventStream,
    mut commands: mpsc::Receiver<Command>,
    status: watch::Sender<Option<EngineError>>,
    active_turns: BTreeSet<String>,
    initial_subscriber: mpsc::UnboundedSender<AgentEvent>,
) {
    let mut state = DriverState {
        client,
        identity,
        runtime,
        active_turns,
        completions: HashMap::new(),
        terminal: HashMap::new(),
        events: Subscribers::new(initial_subscriber),
    };
    let terminal_error = loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    break EngineError::Shutdown;
                };
                if state.command(command).await {
                    break EngineError::Shutdown;
                }
            }
            event = stream.next() => {
                match event {
                    Ok(event) => {
                        if let Err(error) = state.event(event.data) {
                            break error;
                        }
                    }
                    Err(error) => break EngineError::Stream(error.to_string()),
                }
            }
        }
    };
    state.fail_pending(terminal_error.clone());
    let _ = status.send(Some(terminal_error));
}

struct DriverState {
    client: ManagedClient,
    identity: ManagedIdentity,
    runtime: DriverRuntime,
    active_turns: BTreeSet<String>,
    completions: HashMap<String, Vec<Completion>>,
    terminal: HashMap<String, Result<ManagedTurnResult, EngineError>>,
    events: Subscribers<AgentEvent>,
}

impl DriverState {
    async fn command(&mut self, command: Command) -> bool {
        match command {
            Command::Prompt { prompt, reply } => {
                let result = self.submit(prompt).await;
                let _ = reply.send(result);
            }
            Command::ResumeTurn { turn_id, reply } => {
                let result = self.resume_turn(turn_id).await;
                let _ = reply.send(result);
            }
            Command::Active { reply } => {
                let result = self.active_control();
                let _ = reply.send(result);
            }
            Command::Steer {
                turn_id,
                input,
                reply,
            } => {
                let result = self
                    .client
                    .steer(self.identity.agent_id(), &turn_id, &input)
                    .await
                    .map(|_| ())
                    .map_err(EngineError::from);
                let _ = reply.send(result);
            }
            Command::Cancel { turn_id, reply } => {
                let result = self
                    .client
                    .cancel(self.identity.agent_id(), &turn_id)
                    .await
                    .map(|_| ())
                    .map_err(EngineError::from);
                let _ = reply.send(result);
            }
            Command::Subscribe { reply } => {
                let _ = reply.send(Ok(self.events.subscribe()));
            }
            Command::Spawn { reply } => {
                let result = ManagedAgent::create(self.client.clone()).await;
                let _ = reply.send(result);
            }
            Command::Shutdown { reply } => {
                let _ = reply.send(Ok(()));
                return true;
            }
        }
        false
    }

    async fn submit(&mut self, prompt: ManagedPrompt) -> Result<ManagedTurn, EngineError> {
        let view = self
            .client
            .submit(
                self.identity.agent_id(),
                Some(&prompt.turn_id),
                &prompt.idempotency_key,
                &prompt.input,
            )
            .await?;
        if view.turn_id != prompt.turn_id {
            return Err(EngineError::InvalidResponse(
                "accepted turn identity differs from submitted turn identity",
            ));
        }
        self.turn_from_view(view)
    }

    async fn resume_turn(&mut self, turn_id: String) -> Result<ManagedTurn, EngineError> {
        if let Some(result) = self.terminal.get(&turn_id).cloned() {
            return self.completed_turn(turn_id, result);
        }
        let view = self
            .client
            .turn_state(self.identity.agent_id(), &turn_id)
            .await?;
        if view.turn_id != turn_id {
            return Err(EngineError::InvalidResponse(
                "requested and returned turn identities differ",
            ));
        }
        self.turn_from_view(view)
    }

    fn turn_from_view(&mut self, mut view: TurnView) -> Result<ManagedTurn, EngineError> {
        let turn_id = view.turn_id.clone();
        if let Some(data) = view.terminal.take() {
            let result = terminal_outcome(&data, &turn_id).ok_or(EngineError::InvalidResponse(
                "turn terminal payload does not match its identity",
            ))?;
            self.terminal.insert(turn_id.clone(), result.clone());
            self.active_turns.remove(&turn_id);
            return self.completed_turn(turn_id, result);
        }
        if is_terminal_state(view.state) {
            let result = terminal_state_outcome(&view);
            self.terminal.insert(turn_id.clone(), result.clone());
            self.active_turns.remove(&turn_id);
            return self.completed_turn(turn_id, result);
        }
        self.active_turns.insert(turn_id.clone());
        let (completion, receiver) = oneshot::channel();
        self.completions
            .entry(turn_id.clone())
            .or_default()
            .push(completion);
        self.pending_turn(turn_id, receiver)
    }

    fn active_control(&self) -> Result<ManagedTurnControl, EngineError> {
        match self.active_turns.len() {
            0 => Err(EngineError::NoActiveTurn),
            1 => self
                .active_turns
                .first()
                .ok_or(EngineError::NoActiveTurn)
                .and_then(|turn_id| self.control(Arc::from(turn_id.as_str()))),
            count => Err(EngineError::AmbiguousActiveTurn { count }),
        }
    }

    fn event(&mut self, data: ManagedEventData) -> Result<(), EngineError> {
        if let Some(event) = data.agent_event()? {
            self.events.publish(event);
        }
        if let Some(turn_id) = data.turn_id().map(str::to_owned) {
            match &data {
                ManagedEventData::TurnAccepted { .. }
                | ManagedEventData::TurnCancelling { .. }
                | ManagedEventData::TurnRetryable { .. } => {
                    self.active_turns.insert(turn_id);
                }
                ManagedEventData::TurnCompleted { .. }
                | ManagedEventData::TurnCancelled { .. }
                | ManagedEventData::TurnBlocked { .. }
                | ManagedEventData::TurnFailed { .. } => {
                    let result = terminal_outcome(&data, &turn_id).ok_or(
                        EngineError::InvalidResponse("terminal event identity is inconsistent"),
                    )?;
                    self.resolve(turn_id, result);
                }
                ManagedEventData::AgentCreated { .. }
                | ManagedEventData::Event { .. }
                | ManagedEventData::StreamFailed { .. } => {}
            }
        }
        if let ManagedEventData::StreamFailed { error } = data {
            return Err(EngineError::Stream(error));
        }
        Ok(())
    }

    fn resolve(&mut self, turn_id: String, result: Result<ManagedTurnResult, EngineError>) {
        self.active_turns.remove(&turn_id);
        self.terminal.insert(turn_id.clone(), result.clone());
        if let Some(waiters) = self.completions.remove(&turn_id) {
            for waiter in waiters {
                let _ = waiter.send(result.clone());
            }
        }
    }

    fn completed_turn(
        &self,
        turn_id: String,
        result: Result<ManagedTurnResult, EngineError>,
    ) -> Result<ManagedTurn, EngineError> {
        let (sender, receiver) = oneshot::channel();
        let _ = sender.send(result);
        self.pending_turn(turn_id, receiver)
    }

    fn pending_turn(
        &self,
        turn_id: String,
        completion: oneshot::Receiver<Result<ManagedTurnResult, EngineError>>,
    ) -> Result<ManagedTurn, EngineError> {
        let turn_id: Arc<str> = Arc::from(turn_id);
        Ok(ManagedTurn {
            control: self.control(turn_id.clone())?,
            turn_id,
            completion,
        })
    }

    fn control(&self, turn_id: Arc<str>) -> Result<ManagedTurnControl, EngineError> {
        Ok(ManagedTurnControl {
            turn_id,
            runtime: self.runtime.handle()?,
        })
    }

    fn fail_pending(&mut self, error: EngineError) {
        for (_, waiters) in self.completions.drain() {
            for waiter in waiters {
                let _ = waiter.send(Err(error.clone()));
            }
        }
    }
}

fn is_terminal_state(state: TurnState) -> bool {
    matches!(
        state,
        TurnState::Blocked | TurnState::Completed | TurnState::Cancelled | TurnState::Failed
    )
}

fn terminal_state_outcome(view: &TurnView) -> Result<ManagedTurnResult, EngineError> {
    let state = match view.state {
        TurnState::Blocked => "blocked",
        TurnState::Completed => "completed",
        TurnState::Cancelled => "cancelled",
        TurnState::Failed => "failed",
        TurnState::Accepted | TurnState::Cancelling | TurnState::Retryable => "unfinished",
    };
    Err(EngineError::Turn {
        turn_id: view.turn_id.clone(),
        state,
        message: view.error.clone().unwrap_or_else(|| {
            if view.state == TurnState::Completed {
                "completed turn omitted its terminal payload".to_owned()
            } else {
                format!("managed turn is {state}")
            }
        }),
    })
}

fn terminal_outcome(
    data: &ManagedEventData,
    expected_turn_id: &str,
) -> Option<Result<ManagedTurnResult, EngineError>> {
    match data {
        ManagedEventData::TurnCompleted {
            id,
            final_message,
            usage,
            usage_error,
        } if id == expected_turn_id => Some(Ok(ManagedTurnResult {
            turn_id: id.clone(),
            final_message: final_message.clone(),
            usage: usage.clone(),
            usage_error: usage_error.clone(),
        })),
        ManagedEventData::TurnCancelled { id } if id == expected_turn_id => {
            Some(Err(EngineError::Turn {
                turn_id: id.clone(),
                state: "cancelled",
                message: "managed turn was cancelled".to_owned(),
            }))
        }
        ManagedEventData::TurnBlocked { id, error } if id == expected_turn_id => {
            Some(Err(EngineError::Turn {
                turn_id: id.clone(),
                state: "blocked",
                message: error.clone(),
            }))
        }
        ManagedEventData::TurnFailed { id, error } if id == expected_turn_id => {
            Some(Err(EngineError::Turn {
                turn_id: id.clone(),
                state: "failed",
                message: error.clone(),
            }))
        }
        _ => None,
    }
}

struct Subscribers<T> {
    cache: Vec<T>,
    senders: Vec<mpsc::UnboundedSender<T>>,
}

impl<T: Clone> Subscribers<T> {
    fn new(initial: mpsc::UnboundedSender<T>) -> Self {
        Self {
            cache: Vec::new(),
            senders: vec![initial],
        }
    }

    fn publish(&mut self, item: T) {
        self.cache.push(item.clone());
        self.senders
            .retain(|sender| sender.send(item.clone()).is_ok());
    }

    fn subscribe(&mut self) -> mpsc::UnboundedReceiver<T> {
        let (sender, receiver) = mpsc::unbounded_channel();
        for item in &self.cache {
            if sender.send(item.clone()).is_err() {
                return receiver;
            }
        }
        self.senders.push(sender);
        receiver
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::{EngineError, ManagedEventData, Subscribers, terminal_outcome};

    #[test]
    fn terminal_completion_preserves_message_and_usage() {
        let terminal = ManagedEventData::TurnCompleted {
            id: "turn-1".to_owned(),
            final_message: "done".to_owned(),
            usage: Some(json!({"input_tokens": 2, "output_tokens": 1})),
            usage_error: Some("cost unavailable".to_owned()),
        };

        let result = terminal_outcome(&terminal, "turn-1")
            .expect("terminal event")
            .expect("successful completion");
        assert_eq!(result.turn_id(), "turn-1");
        assert_eq!(result.final_message(), "done");
        assert_eq!(
            result.usage(),
            Some(&json!({"input_tokens": 2, "output_tokens": 1}))
        );
        assert_eq!(result.usage_error(), Some("cost unavailable"));
    }

    #[test]
    fn terminal_failure_is_typed() {
        let terminal = ManagedEventData::TurnBlocked {
            id: "turn-2".to_owned(),
            error: "provider disconnected".to_owned(),
        };

        let error = terminal_outcome(&terminal, "turn-2")
            .expect("terminal event")
            .expect_err("blocked turn");
        assert!(matches!(
            error,
            EngineError::Turn {
                state: "blocked",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn late_subscriber_replays_cache_then_follows_live_tail() {
        let (initial, mut initial_rx) = mpsc::unbounded_channel();
        let mut subscribers = Subscribers::new(initial);
        subscribers.publish("one");
        subscribers.publish("two");
        let mut late = subscribers.subscribe();
        subscribers.publish("three");

        assert_eq!(initial_rx.recv().await, Some("one"));
        assert_eq!(initial_rx.recv().await, Some("two"));
        assert_eq!(initial_rx.recv().await, Some("three"));
        assert_eq!(late.recv().await, Some("one"));
        assert_eq!(late.recv().await, Some("two"));
        assert_eq!(late.recv().await, Some("three"));
    }
}
