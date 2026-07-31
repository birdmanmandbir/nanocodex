#![allow(clippy::missing_errors_doc)]

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
};

use chrono::{DateTime, Utc};
use nanocodex_agent::{
    TurnUsage,
    events::{AgentEvent as NativeAgentEvent, AgentEventKind},
    input::{ImageDetail, Prompt, UserInput},
    session::SessionSnapshot,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{RwLock, broadcast, mpsc, oneshot};
use tracing::{Instrument, instrument::WithSubscriber};
use url::Url;
use uuid::Uuid;

use crate::{
    AgentCapabilities, AgentError, AgentIdentity, AgentRunResult, AgentSpec, ManagedAgent,
    ManagedAgentFactory, ManagedTurnControl,
    session::{CompletedTurn, NewTurn, SessionError, SessionStore, StoredSession},
};

const MAX_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_CONTENT_BLOCKS: usize = 64;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;
const EVENT_BROADCAST_CAPACITY: usize = 1_024;
const AGENT_COMMAND_CAPACITY: usize = 64;
const RUNTIME_EVENT_BATCH_SIZE: usize = 64;
const EVENT_REPLAY_PAGE_SIZE: usize = 256;

/// Request to create or resolve one managed agent.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CreateAgent {
    /// Optional client-scoped key that makes creation idempotent.
    pub context_key: Option<String>,
}

/// Result of creating or resolving a managed agent.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CreateAgentResponse {
    /// Stable opaque managed-agent identifier.
    pub agent_id: String,
    /// Whether this request created a new durable identity.
    pub created: bool,
    /// Current live lifecycle state.
    pub state: AgentStatus,
}

/// Current durable and live projection of one managed agent.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentView {
    /// Stable opaque managed-agent identifier.
    pub agent_id: String,
    /// Whether a turn is currently active.
    pub state: AgentStatus,
    /// Number of accepted turns waiting behind the active turn.
    pub queue_depth: usize,
    /// Client-scoped create-or-resolve key, when one was supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_key: Option<String>,
    /// Durable creation timestamp.
    pub created_at: DateTime<Utc>,
}

/// Live state of one managed agent actor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// No turn is active.
    Idle,
    /// One turn is active; additional turns may be steered or queued.
    Running,
}

/// Delivery policy for input submitted while another turn is active.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnDelivery {
    /// Steer the active turn when possible; otherwise start a new turn.
    #[default]
    Steer,
    /// Always create a distinct FIFO turn.
    Enqueue,
}

/// Bounded typed input for one managed turn.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTurn {
    /// Steering versus explicit queueing policy.
    #[serde(default)]
    pub delivery: TurnDelivery,
    /// Ordered user content blocks.
    pub content: Vec<ContentBlock>,
}

/// One typed user or assistant content block.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentBlock {
    /// UTF-8 text.
    Text {
        /// Complete text content.
        text: String,
    },
    /// Remote HTTP(S) image input.
    ImageUrl {
        /// Absolute image URL.
        url: String,
        /// Optional provider image-detail policy.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<ImageDetail>,
    },
    /// Remote HTTP(S) audio input.
    AudioUrl {
        /// Absolute audio URL.
        url: String,
    },
}

/// Scheduling action taken for accepted input.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnAction {
    /// A new turn started immediately.
    Started,
    /// Input was appended to the active turn.
    Steered,
    /// A distinct turn entered the FIFO queue.
    Queued,
}

/// Acceptance result for a managed turn request.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TurnActionResponse {
    /// Scheduling action taken.
    pub action: TurnAction,
    /// Stable durable turn identifier.
    pub turn_id: String,
    /// State immediately after acceptance.
    pub state: TurnStatus,
}

/// Durable projection of one managed turn.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TurnView {
    /// Stable durable turn identifier.
    pub turn_id: String,
    /// Owning managed-agent identifier.
    pub agent_id: String,
    /// Current or terminal lifecycle state.
    pub state: TurnStatus,
    /// Complete assistant output for a successful terminal turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<ContentBlock>,
    /// Stable public failure message, when failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Authoritative aggregate usage and optional versioned USD estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TurnUsage>,
    /// Durable acceptance timestamp.
    pub created_at: DateTime<Utc>,
    /// Durable terminal timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
}

/// Durable lifecycle state of one turn.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    /// Accepted behind another active turn.
    Queued,
    /// Currently executing.
    Running,
    /// Successfully committed.
    Completed,
    /// Ended without committing a successful result.
    Failed,
    /// Explicitly cancelled.
    Cancelled,
}

impl TurnStatus {
    /// Returns whether no further state transition is possible.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Result of branching a new managed agent from committed history.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ForkResponse {
    /// New managed-agent identifier.
    pub agent_id: String,
    /// Exact committed source boundary.
    pub forked_from: ForkSource,
    /// Initial state of the new agent.
    pub state: AgentStatus,
}

/// Durable source boundary used for a fork.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ForkSource {
    /// Source managed-agent identifier.
    pub agent_id: String,
    /// Exact completed turn, or `None` when no completed boundary existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

/// One monotonically ordered durable managed-agent event.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentEvent {
    /// Agent-relative monotonic sequence.
    pub id: u64,
    /// Owning managed-agent identifier.
    pub agent_id: String,
    /// Associated durable turn, when known.
    ///
    /// Native runtime events leave this unset because their optional stream is
    /// intentionally independent from managed turn-result completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Typed lifecycle or lossless native runtime payload.
    #[serde(flatten)]
    pub payload: AgentEventPayload,
    /// Durable observation timestamp.
    pub created_at: DateTime<Utc>,
}

/// Managed lifecycle events plus the lossless native Nanocodex firehose.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum AgentEventPayload {
    /// A turn began execution.
    #[serde(rename = "turn.started")]
    TurnStarted {
        /// State after the transition.
        state: TurnStatus,
    },
    /// A distinct turn entered the queue.
    #[serde(rename = "turn.queued")]
    TurnQueued {
        /// State after the transition.
        state: TurnStatus,
    },
    /// Cancellation was durably requested.
    #[serde(rename = "turn.cancel_requested")]
    TurnCancelRequested,
    /// A process restart moved an incomplete running turn back to the queue.
    #[serde(rename = "turn.interrupted")]
    TurnInterrupted {
        /// Whether wake-up will retry the turn.
        retrying: bool,
    },
    /// A successful result and usage were durably committed.
    #[serde(rename = "turn.completed")]
    TurnCompleted {
        /// Complete assistant output.
        output: Vec<ContentBlock>,
        /// Aggregate provider usage and optional estimated USD cost.
        usage: TurnUsage,
    },
    /// Cancellation reached its terminal boundary.
    #[serde(rename = "turn.cancelled")]
    TurnCancelled,
    /// Execution reached a terminal failure.
    #[serde(rename = "turn.failed")]
    TurnFailed {
        /// Stable public failure classification.
        error: TurnFailure,
    },
    /// Lossless native event emitted by the owned agent lifecycle.
    #[serde(rename = "runtime")]
    Runtime {
        /// Original typed Nanocodex event.
        event: NativeAgentEvent,
    },
}

/// Stable public managed-turn failure classification.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnFailure {
    /// The backend agent failed after accepting the turn.
    ManagedAgentExecutionFailed,
}

impl AgentEventPayload {
    /// Returns the stable SSE event name for this payload.
    #[must_use]
    pub const fn event_name(&self) -> &'static str {
        match self {
            Self::TurnStarted { .. } => "turn.started",
            Self::TurnQueued { .. } => "turn.queued",
            Self::TurnCancelRequested => "turn.cancel_requested",
            Self::TurnInterrupted { .. } => "turn.interrupted",
            Self::TurnCompleted { .. } => "turn.completed",
            Self::TurnCancelled => "turn.cancelled",
            Self::TurnFailed { .. } => "turn.failed",
            Self::Runtime { event } => runtime_event_name(event.kind),
        }
    }
}

/// Shared registry of lightweight actor handles. `SQLite` owns durable session
/// state; each actor is a disposable live projection that owns one harness.
pub struct AgentManager {
    factory: Arc<dyn ManagedAgentFactory>,
    agents: RwLock<HashMap<String, AgentHandle>>,
    sessions: SessionStore,
}

impl AgentManager {
    /// Opens the durable session store and prepares lazy agent wake-up.
    ///
    /// # Errors
    ///
    /// Returns a durability or filesystem error when the state directory
    /// cannot be initialized.
    pub fn new(
        factory: Arc<dyn ManagedAgentFactory>,
        state_directory: impl Into<PathBuf>,
    ) -> Result<Self, ManagerError> {
        let state_directory = state_directory.into();
        let sessions = SessionStore::open(state_directory.join("sessions.sqlite"))?;
        Ok(Self {
            factory,
            agents: RwLock::new(HashMap::new()),
            sessions,
        })
    }

    /// Registers or wakes an authorized durable agent.
    pub async fn register(&self, identity: AgentIdentity) -> Result<AgentView, ManagerError> {
        self.ensure(identity).await?.view().await
    }

    /// Returns current state, waking the disposable actor when necessary.
    pub async fn get(&self, identity: AgentIdentity) -> Result<AgentView, ManagerError> {
        self.ensure(identity).await?.view().await
    }

    /// Resolves an earlier accepted request by its agent-scoped key.
    pub async fn find_turn_by_idempotency_key(
        &self,
        identity: AgentIdentity,
        key: &str,
    ) -> Result<Option<TurnActionResponse>, ManagerError> {
        validate_idempotency_key(Some(key))?;
        self.sessions
            .find_request(identity.id, key.to_owned())
            .await
            .map_err(Into::into)
    }

    /// Validates and durably accepts input for steering or FIFO execution.
    pub async fn create_turn(
        &self,
        identity: AgentIdentity,
        request: CreateTurn,
        idempotency_key: Option<String>,
    ) -> Result<TurnActionResponse, ManagerError> {
        validate_content(&request.content)?;
        validate_idempotency_key(idempotency_key.as_deref())?;
        self.ensure(identity)
            .await?
            .create_turn(request, idempotency_key)
            .await
    }

    /// Returns one durable turn projection.
    pub async fn get_turn(
        &self,
        identity: AgentIdentity,
        turn_id: &str,
    ) -> Result<TurnView, ManagerError> {
        self.ensure(identity)
            .await?
            .get_turn(turn_id.to_owned())
            .await
    }

    /// Requests cancellation of one queued or active turn.
    ///
    /// Returns `true` when cancellation was newly accepted and `false` when
    /// the backend had already reached a non-cancellable boundary.
    pub async fn cancel_turn(
        &self,
        identity: AgentIdentity,
        turn_id: &str,
    ) -> Result<bool, ManagerError> {
        self.ensure(identity)
            .await?
            .cancel(turn_id.to_owned())
            .await
    }

    /// Drops one live harness while retaining all durable state.
    ///
    /// Returns whether a live actor was present.
    pub async fn evict(&self, identity: AgentIdentity) -> Result<bool, ManagerError> {
        self.ensure(identity).await?.evict().await
    }

    /// Opens an ordered replay-then-live cursor after an event sequence.
    pub async fn events(
        &self,
        identity: AgentIdentity,
        after_event_id: u64,
    ) -> Result<EventCursor, ManagerError> {
        self.ensure(identity).await?.subscribe(after_event_id).await
    }

    /// Branches committed history into a fresh agent and workspace.
    ///
    /// `turn_id = None` selects the latest completed turn.
    pub async fn fork(
        &self,
        source_identity: AgentIdentity,
        target_identity: AgentIdentity,
        turn_id: Option<&str>,
    ) -> Result<ForkResponse, ManagerError> {
        let forked_from = self
            .sessions
            .fork(
                source_identity.id,
                target_identity.id.clone(),
                turn_id.map(str::to_owned),
            )
            .await?;
        let target_id = target_identity.id.clone();
        self.ensure(target_identity).await?;
        Ok(ForkResponse {
            agent_id: target_id,
            forked_from,
            state: AgentStatus::Idle,
        })
    }

    /// Deletes durable session state after stopping any live runtime.
    pub async fn delete(&self, agent_id: &str) -> Result<(), ManagerError> {
        let mut agents = self.agents.write().await;
        if let Some(handle) = agents.get(agent_id).cloned() {
            handle.delete().await?;
        }
        let result = self.sessions.delete(agent_id.to_owned()).await;
        agents.remove(agent_id);
        result.map_err(Into::into)
    }

    /// Stops every live harness without deleting its durable `SQLite` session.
    /// A later manager can wake the same agents from the event log.
    pub async fn shutdown(&self) -> Result<(), ManagerError> {
        let handles = self
            .agents
            .write()
            .await
            .drain()
            .map(|(_, handle)| handle)
            .collect::<Vec<_>>();
        for handle in handles {
            handle.shutdown().await?;
        }
        Ok(())
    }

    async fn ensure(&self, identity: AgentIdentity) -> Result<AgentHandle, ManagerError> {
        let existing = self.agents.read().await.get(&identity.id).cloned();
        if let Some(handle) = existing {
            handle
                .refresh_policy(
                    identity.principal.permissions,
                    identity.principal.secret_revision,
                )
                .await?;
            return Ok(handle);
        }
        let stored = self.sessions.load(identity.id.clone()).await?;
        let capabilities = identity.principal.permissions.clone();
        let secret_revision = identity.principal.secret_revision;
        let handle = {
            let mut agents = self.agents.write().await;
            if let Some(handle) = agents.get(&identity.id).cloned() {
                handle
            } else {
                let id = identity.id.clone();
                let (sender, receiver) = mpsc::channel(AGENT_COMMAND_CAPACITY);
                let sender = AgentSender(sender);
                let handle = AgentHandle {
                    sender: sender.clone(),
                };
                let actor = AgentActor::new(
                    identity,
                    Arc::clone(&self.factory),
                    self.sessions.clone(),
                    stored,
                    sender,
                );
                tokio::spawn(actor.run(receiver));
                agents.insert(id, handle.clone());
                handle
            }
        };
        handle.refresh_policy(capabilities, secret_revision).await?;
        Ok(handle)
    }
}

#[derive(Clone)]
struct AgentHandle {
    sender: AgentSender,
}

#[derive(Clone)]
struct AgentSender(mpsc::Sender<AgentCommandEnvelope>);

struct AgentCommandEnvelope {
    dispatch: tracing::Dispatch,
    parent: tracing::Span,
    queued_at: std::time::Instant,
    command: AgentCommand,
}

impl AgentSender {
    async fn send(
        &self,
        command: AgentCommand,
    ) -> Result<(), mpsc::error::SendError<AgentCommandEnvelope>> {
        self.0
            .send(AgentCommandEnvelope {
                dispatch: tracing::dispatcher::get_default(Clone::clone),
                parent: tracing::Span::current(),
                queued_at: std::time::Instant::now(),
                command,
            })
            .await
    }
}

impl AgentHandle {
    async fn view(&self) -> Result<AgentView, ManagerError> {
        self.request(|reply| AgentCommand::View { reply }).await
    }

    async fn refresh_policy(
        &self,
        capabilities: AgentCapabilities,
        secret_revision: u64,
    ) -> Result<(), ManagerError> {
        self.request(|reply| AgentCommand::RefreshPolicy {
            capabilities,
            secret_revision,
            reply,
        })
        .await
    }

    async fn create_turn(
        &self,
        request: CreateTurn,
        idempotency_key: Option<String>,
    ) -> Result<TurnActionResponse, ManagerError> {
        self.request(|reply| AgentCommand::CreateTurn {
            request,
            idempotency_key,
            reply,
        })
        .await
    }

    async fn get_turn(&self, turn_id: String) -> Result<TurnView, ManagerError> {
        self.request(|reply| AgentCommand::GetTurn { turn_id, reply })
            .await
    }

    async fn cancel(&self, turn_id: String) -> Result<bool, ManagerError> {
        self.request(|reply| AgentCommand::Cancel { turn_id, reply })
            .await
    }

    async fn evict(&self) -> Result<bool, ManagerError> {
        self.request(|reply| AgentCommand::Evict { reply }).await
    }

    async fn subscribe(&self, after_event_id: u64) -> Result<EventCursor, ManagerError> {
        let subscription = self
            .request(|reply| AgentCommand::Subscribe {
                after_event_id,
                reply,
            })
            .await?;
        Ok(EventCursor {
            sender: self.sender.clone(),
            receiver: subscription.receiver,
            after_event_id,
            replay: subscription.replay.events,
            replay_exhausted: subscription.replay.exhausted,
        })
    }

    async fn shutdown(&self) -> Result<(), ManagerError> {
        self.request(|reply| AgentCommand::Shutdown { reply }).await
    }

    async fn delete(&self) -> Result<(), ManagerError> {
        self.request(|reply| AgentCommand::Delete { reply }).await
    }

    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, ManagerError>>) -> AgentCommand,
    ) -> Result<T, ManagerError> {
        let (reply, response) = oneshot::channel();
        let command = command(reply);
        self.sender
            .send(command)
            .await
            .map_err(|_| ManagerError::ActorStopped)?;
        response.await.map_err(|_| ManagerError::ActorStopped)?
    }
}

enum AgentCommand {
    View {
        reply: Reply<AgentView>,
    },
    RefreshPolicy {
        capabilities: AgentCapabilities,
        secret_revision: u64,
        reply: Reply<()>,
    },
    CreateTurn {
        request: CreateTurn,
        idempotency_key: Option<String>,
        reply: Reply<TurnActionResponse>,
    },
    GetTurn {
        turn_id: String,
        reply: Reply<TurnView>,
    },
    Cancel {
        turn_id: String,
        reply: Reply<bool>,
    },
    Evict {
        reply: Reply<bool>,
    },
    Subscribe {
        after_event_id: u64,
        reply: Reply<EventSubscription>,
    },
    Replay {
        after_event_id: u64,
        reply: Reply<EventReplay>,
    },
    RuntimeEvents(Vec<NativeAgentEvent>),
    TurnFinished {
        turn_id: String,
        result: Box<Result<AgentRunResult, AgentError>>,
    },
    Shutdown {
        reply: Reply<()>,
    },
    Delete {
        reply: Reply<()>,
    },
}

impl AgentCommand {
    const fn name(&self) -> &'static str {
        match self {
            Self::View { .. } => "view",
            Self::RefreshPolicy { .. } => "refresh_policy",
            Self::CreateTurn { .. } => "create_turn",
            Self::GetTurn { .. } => "get_turn",
            Self::Cancel { .. } => "cancel",
            Self::Evict { .. } => "evict",
            Self::Subscribe { .. } => "subscribe",
            Self::Replay { .. } => "replay",
            Self::RuntimeEvents(_) => "runtime_events",
            Self::TurnFinished { .. } => "turn_finished",
            Self::Shutdown { .. } => "shutdown",
            Self::Delete { .. } => "delete",
        }
    }
}

type Reply<T> = oneshot::Sender<Result<T, ManagerError>>;

struct Runtime {
    agent: Arc<dyn ManagedAgent>,
}

struct StoredTurn {
    view: TurnView,
    inputs: Vec<Vec<ContentBlock>>,
    control: Option<Arc<dyn ManagedTurnControl>>,
}

struct EventSubscription {
    receiver: broadcast::Receiver<AgentEvent>,
    replay: EventReplay,
}

struct EventReplay {
    events: VecDeque<AgentEvent>,
    exhausted: bool,
}

impl EventReplay {
    fn new(events: Vec<AgentEvent>) -> Self {
        let exhausted = events.len() < EVENT_REPLAY_PAGE_SIZE;
        Self {
            events: events.into(),
            exhausted,
        }
    }
}

struct AgentActor {
    id: String,
    principal_id: String,
    context_key: Option<String>,
    instructions: Option<String>,
    thinking: Option<nanocodex_agent::Thinking>,
    capabilities: AgentCapabilities,
    secret_revision: u64,
    created_at: DateTime<Utc>,
    factory: Arc<dyn ManagedAgentFactory>,
    sessions: SessionStore,
    self_sender: AgentSender,
    active_turn: Option<String>,
    cancel_requested: HashSet<String>,
    turns: HashMap<String, StoredTurn>,
    turn_order: VecDeque<String>,
    runtime: Option<Runtime>,
    runtime_policy_dirty: bool,
    snapshot: Option<SessionSnapshot>,
    event_sender: broadcast::Sender<AgentEvent>,
}

impl AgentActor {
    fn new(
        identity: AgentIdentity,
        factory: Arc<dyn ManagedAgentFactory>,
        sessions: SessionStore,
        stored: StoredSession,
        self_sender: AgentSender,
    ) -> Self {
        let (event_sender, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let agent_config = identity.principal.agent_config;
        let mut turns = HashMap::new();
        let mut turn_order = VecDeque::new();
        let mut cancel_requested = HashSet::new();
        for turn in stored.turns {
            if turn.cancel_requested {
                cancel_requested.insert(turn.view.turn_id.clone());
            }
            turn_order.push_back(turn.view.turn_id.clone());
            turns.insert(
                turn.view.turn_id.clone(),
                StoredTurn {
                    view: turn.view,
                    inputs: turn.inputs,
                    control: None,
                },
            );
        }
        Self {
            id: identity.id,
            principal_id: identity.principal.id,
            context_key: identity.context_key,
            instructions: agent_config.instructions,
            thinking: agent_config.reasoning_effort.map(Into::into),
            capabilities: identity.principal.permissions,
            secret_revision: identity.principal.secret_revision,
            created_at: identity.created_at,
            factory,
            sessions,
            self_sender,
            active_turn: None,
            cancel_requested,
            turns,
            turn_order,
            runtime: None,
            runtime_policy_dirty: false,
            snapshot: stored.snapshot,
            event_sender,
        }
    }

    async fn run(mut self, mut receiver: mpsc::Receiver<AgentCommandEnvelope>) {
        let recovery_span = tracing::info_span!(
            parent: None,
            "nanocentaur.agent.recover",
            agent.id = self.id,
        );
        if let Err(error) = self.recover_pending().instrument(recovery_span).await {
            tracing::error!(agent_id = self.id, %error, "failed to recover durable agent session");
        }
        while let Some(envelope) = receiver.recv().await {
            let command_name = envelope.command.name();
            let span = tracing::dispatcher::with_default(&envelope.dispatch, || {
                tracing::info_span!(
                    parent: &envelope.parent,
                    "nanocentaur.agent.command",
                    agent.id = self.id,
                    agent.command = command_name,
                    queue.duration_ns =
                        u64::try_from(envelope.queued_at.elapsed().as_nanos()).unwrap_or(u64::MAX),
                )
            });
            let should_stop = self
                .handle(envelope.command)
                .instrument(span)
                .with_subscriber(envelope.dispatch)
                .await;
            if should_stop {
                return;
            }
        }
    }

    async fn handle(&mut self, command: AgentCommand) -> bool {
        let mut should_stop = false;
        match command {
            AgentCommand::View { reply } => {
                drop(reply.send(Ok(self.view())));
            }
            AgentCommand::RefreshPolicy {
                capabilities,
                secret_revision,
                reply,
            } => {
                self.apply_policy(capabilities, secret_revision);
                drop(reply.send(Ok(())));
            }
            AgentCommand::CreateTurn {
                request,
                idempotency_key,
                reply,
            } => {
                let result = self.create_turn(request, idempotency_key).await;
                drop(reply.send(result));
            }
            AgentCommand::GetTurn { turn_id, reply } => {
                let result = if let Some(turn) = self.turns.get(&turn_id) {
                    Ok(turn.view.clone())
                } else {
                    match self.sessions.turn(self.id.clone(), turn_id).await {
                        Ok(Some(turn)) => Ok(turn),
                        Ok(None) => Err(ManagerError::NotFound),
                        Err(error) => Err(error.into()),
                    }
                };
                drop(reply.send(result));
            }
            AgentCommand::Cancel { turn_id, reply } => {
                let result = self.cancel(&turn_id).await;
                drop(reply.send(result));
            }
            AgentCommand::Evict { reply } => {
                let result = if self.active_turn.is_some() {
                    Ok(false)
                } else {
                    Ok(self.runtime.take().is_some())
                };
                drop(reply.send(result));
            }
            AgentCommand::Subscribe {
                after_event_id,
                reply,
            } => {
                let receiver = self.event_sender.subscribe();
                let replay = self
                    .sessions
                    .events_after(self.id.clone(), after_event_id, EVENT_REPLAY_PAGE_SIZE)
                    .await
                    .map(EventReplay::new)
                    .map_err(ManagerError::from);
                drop(reply.send(replay.map(|replay| EventSubscription { receiver, replay })));
            }
            AgentCommand::Replay {
                after_event_id,
                reply,
            } => {
                let replay = self
                    .sessions
                    .events_after(self.id.clone(), after_event_id, EVENT_REPLAY_PAGE_SIZE)
                    .await
                    .map(EventReplay::new)
                    .map_err(ManagerError::from);
                drop(reply.send(replay));
            }
            AgentCommand::RuntimeEvents(events) => {
                self.handle_runtime_events(events).await;
            }
            AgentCommand::TurnFinished { turn_id, result } => {
                self.finish_turn(&turn_id, *result).await;
            }
            AgentCommand::Shutdown { reply } => {
                self.runtime.take();
                drop(reply.send(Ok(())));
                should_stop = true;
            }
            AgentCommand::Delete { reply } => {
                if self.active_turn.is_some() {
                    drop(reply.send(Err(ManagerError::AgentBusy)));
                } else {
                    self.runtime.take();
                    drop(reply.send(Ok(())));
                    should_stop = true;
                }
            }
        }
        should_stop
    }

    fn apply_policy(&mut self, capabilities: AgentCapabilities, secret_revision: u64) {
        if self.capabilities == capabilities && self.secret_revision == secret_revision {
            return;
        }
        self.capabilities = capabilities;
        self.secret_revision = secret_revision;
        if self.active_turn.is_some() {
            self.runtime_policy_dirty = true;
        } else {
            self.runtime.take();
            self.runtime_policy_dirty = false;
        }
    }

    fn view(&self) -> AgentView {
        AgentView {
            agent_id: self.id.clone(),
            state: if self.active_turn.is_some() {
                AgentStatus::Running
            } else {
                AgentStatus::Idle
            },
            queue_depth: self
                .turns
                .values()
                .filter(|turn| turn.view.state == TurnStatus::Queued)
                .count(),
            context_key: self.context_key.clone(),
            created_at: self.created_at,
        }
    }

    async fn create_turn(
        &mut self,
        request: CreateTurn,
        idempotency_key: Option<String>,
    ) -> Result<TurnActionResponse, ManagerError> {
        tracing::info!(
            target: "nanocentaur::observed",
            agent_id = %self.id,
            request = ?request,
            idempotency_key = ?idempotency_key,
            "managed turn input observed"
        );
        if let Some(key) = &idempotency_key
            && let Some(response) = self
                .sessions
                .find_request(self.id.clone(), key.clone())
                .await?
        {
            return Ok(response);
        }

        let content = request.content;
        let prompt = to_prompt(content.clone());
        if request.delivery == TurnDelivery::Steer
            && let Some(response) = self
                .steer_active(prompt.clone(), content.clone(), idempotency_key.clone())
                .await?
        {
            return Ok(response);
        }

        self.ensure_runtime().await?;
        let managed = self
            .runtime
            .as_ref()
            .map(|runtime| Arc::clone(&runtime.agent))
            .ok_or_else(|| AgentError::Backend("runtime disappeared".to_owned()))?;
        let turn_id = Uuid::now_v7().to_string();
        let (action, state) = if self.active_turn.is_some() {
            (TurnAction::Queued, TurnStatus::Queued)
        } else {
            (TurnAction::Started, TurnStatus::Running)
        };
        let view = TurnView {
            turn_id: turn_id.clone(),
            agent_id: self.id.clone(),
            state,
            output: Vec::new(),
            error: None,
            usage: None,
            created_at: Utc::now(),
            completed_at: None,
        };
        let response = TurnActionResponse {
            action,
            turn_id: turn_id.clone(),
            state,
        };
        let event_payload = match action {
            TurnAction::Started => AgentEventPayload::TurnStarted { state },
            TurnAction::Queued => AgentEventPayload::TurnQueued { state },
            TurnAction::Steered => unreachable!(),
        };
        let event = self
            .sessions
            .record_turn(
                self.id.clone(),
                NewTurn {
                    view: view.clone(),
                    delivery: request.delivery,
                    content: content.clone(),
                    response: response.clone(),
                    idempotency_key: idempotency_key.clone(),
                    event: event_payload,
                },
            )
            .await?;
        if action == TurnAction::Started {
            self.active_turn = Some(turn_id.clone());
        }
        self.publish(event);
        self.turn_order.push_back(turn_id.clone());
        self.turns.insert(
            turn_id.clone(),
            StoredTurn {
                view,
                inputs: vec![content],
                control: None,
            },
        );
        match managed.prompt(prompt).await {
            Ok(managed_turn) => self.attach_turn(turn_id, managed_turn),
            Err(error) => {
                self.finish_turn(&turn_id, Err(error)).await;
            }
        }
        Ok(response)
    }

    async fn steer_active(
        &mut self,
        prompt: Prompt,
        content: Vec<ContentBlock>,
        idempotency_key: Option<String>,
    ) -> Result<Option<TurnActionResponse>, ManagerError> {
        let Some(active_id) = self.active_turn.clone() else {
            return Ok(None);
        };
        let Some(control) = self
            .turns
            .get(&active_id)
            .and_then(|turn| turn.control.as_ref().map(Arc::clone))
        else {
            return Ok(None);
        };
        match control.steer(prompt).await {
            Ok(()) => {
                tracing::info!(
                    target: "nanocentaur::observed",
                    agent_id = %self.id,
                    turn_id = %active_id,
                    content = ?content,
                    "managed steering input observed"
                );
                let response = TurnActionResponse {
                    action: TurnAction::Steered,
                    turn_id: active_id.clone(),
                    state: TurnStatus::Running,
                };
                self.sessions
                    .record_steer(
                        self.id.clone(),
                        active_id.clone(),
                        content.clone(),
                        response.clone(),
                        idempotency_key.clone(),
                    )
                    .await?;
                if let Some(turn) = self.turns.get_mut(&active_id) {
                    turn.inputs.push(content);
                }
                Ok(Some(response))
            }
            Err(AgentError::TurnNotSteerable) => {
                self.active_turn = None;
                Ok(None)
            }
            Err(AgentError::SteerQueueFull) => Err(ManagerError::SteerQueueFull),
            Err(error) => Err(error.into()),
        }
    }

    async fn cancel(&mut self, turn_id: &str) -> Result<bool, ManagerError> {
        tracing::info!(
            target: "nanocentaur::observed",
            agent_id = %self.id,
            turn_id = %turn_id,
            "managed cancellation observed"
        );
        let control = self
            .turns
            .get(turn_id)
            .filter(|turn| !turn.view.state.is_terminal())
            .and_then(|turn| turn.control.as_ref().map(Arc::clone))
            .ok_or(ManagerError::NotFound)?;
        match control.cancel().await {
            Ok(()) => {
                self.cancel_requested.insert(turn_id.to_owned());
                let event = self
                    .sessions
                    .request_cancel(self.id.clone(), turn_id.to_owned())
                    .await?;
                self.publish(event);
                Ok(true)
            }
            Err(AgentError::TurnNotCancellable) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    async fn ensure_runtime(&mut self) -> Result<(), AgentError> {
        if self.runtime.is_some() {
            return Ok(());
        }
        let spec = AgentSpec {
            agent_id: self.id.clone(),
            principal: self.principal_id.clone(),
            instructions: self.instructions.clone(),
            thinking: self.thinking,
            capabilities: self.capabilities.clone(),
            snapshot: self.snapshot.clone(),
        };
        tracing::info!(
            target: "nanocentaur::observed",
            agent_id = %spec.agent_id,
            principal_id = %spec.principal,
            instructions = ?spec.instructions,
            thinking = ?spec.thinking,
            capabilities = ?spec.capabilities,
            snapshot = ?spec.snapshot,
            "managed runtime specification observed"
        );
        let spawned = self.factory.create(spec).await?;
        let (agent, mut events) = spawned.into_parts();
        let sender = self.self_sender.clone();
        tokio::spawn(async move {
            let mut batch = Vec::with_capacity(RUNTIME_EVENT_BATCH_SIZE);
            loop {
                let received = events.recv_many(&mut batch, RUNTIME_EVENT_BATCH_SIZE).await;
                if received == 0 {
                    return;
                }
                let ready = batch;
                batch = Vec::with_capacity(RUNTIME_EVENT_BATCH_SIZE);
                if sender
                    .send(AgentCommand::RuntimeEvents(ready))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        self.runtime = Some(Runtime { agent });
        Ok(())
    }

    async fn finish_turn(&mut self, turn_id: &str, result: Result<AgentRunResult, AgentError>) {
        let cancelled = self.cancel_requested.remove(turn_id);
        let completed_at = Utc::now();
        let (status, output, error, snapshot, usage, payload) = match result {
            Ok(result) if !cancelled => {
                let (final_message, snapshot, usage) = result.into_parts();
                tracing::info!(
                    target: "nanocentaur::observed",
                    agent_id = %self.id,
                    turn_id = %turn_id,
                    final_message,
                    snapshot = ?snapshot,
                    usage = ?usage,
                    "managed turn result observed"
                );
                let output = vec![ContentBlock::Text {
                    text: final_message,
                }];
                (
                    TurnStatus::Completed,
                    output.clone(),
                    None,
                    snapshot,
                    Some(usage.clone()),
                    AgentEventPayload::TurnCompleted { output, usage },
                )
            }
            _ if cancelled => (
                TurnStatus::Cancelled,
                Vec::new(),
                None,
                None,
                None,
                AgentEventPayload::TurnCancelled,
            ),
            Ok(_) => {
                // The guarded cancellation arm above is the only remaining
                // successful-result path.
                unreachable!("successful non-cancelled turns are handled first");
            }
            Err(error) => {
                tracing::warn!(agent_id = self.id, turn_id, %error, "turn failed");
                (
                    TurnStatus::Failed,
                    Vec::new(),
                    Some("managed agent execution failed".to_owned()),
                    None,
                    None,
                    AgentEventPayload::TurnFailed {
                        error: TurnFailure::ManagedAgentExecutionFailed,
                    },
                )
            }
        };
        match self
            .sessions
            .finish_turn(
                self.id.clone(),
                turn_id.to_owned(),
                CompletedTurn {
                    status,
                    output,
                    error,
                    completed_at,
                    snapshot,
                    usage,
                    event: payload,
                },
            )
            .await
        {
            Ok(finished) => {
                if status == TurnStatus::Completed {
                    self.snapshot = finished.snapshot;
                }
                self.publish(finished.event);
            }
            Err(error) => {
                tracing::error!(agent_id = self.id, turn_id, %error, "failed to persist terminal turn");
                return;
            }
        }
        self.turns.remove(turn_id);
        self.turn_order.retain(|candidate| candidate != turn_id);
        self.advance_after(turn_id).await;
    }

    async fn handle_runtime_events(&mut self, events: Vec<NativeAgentEvent>) {
        let events = events
            .into_iter()
            .map(|event| {
                tracing::info!(
                    target: "nanocentaur::observed",
                    agent_id = %self.id,
                    event = ?event,
                    "native runtime event observed"
                );
                (None, AgentEventPayload::Runtime { event })
            })
            .collect();
        match self.sessions.append_events(self.id.clone(), events).await {
            Ok(events) => {
                for event in events {
                    self.publish(event);
                }
            }
            Err(error) => {
                tracing::error!(agent_id = self.id, %error, "failed to persist runtime events");
            }
        }
    }

    async fn advance_after(&mut self, completed_turn_id: &str) {
        if self.active_turn.as_deref() != Some(completed_turn_id) {
            return;
        }
        let next = self.turn_order.iter().find_map(|id| {
            self.turns
                .get(id)
                .filter(|turn| turn.view.state == TurnStatus::Queued)
                .map(|_| id.clone())
        });
        if let Some(next) = next {
            match self
                .sessions
                .mark_started(self.id.clone(), next.clone())
                .await
            {
                Ok(event) => {
                    self.active_turn = Some(next.clone());
                    if let Some(turn) = self.turns.get_mut(&next) {
                        turn.view.state = TurnStatus::Running;
                    }
                    self.publish(event);
                }
                Err(error) => {
                    tracing::error!(agent_id = self.id, turn_id = next, %error, "failed to persist queued turn start");
                }
            }
        } else {
            self.active_turn = None;
            if self.runtime_policy_dirty {
                self.runtime.take();
                self.runtime_policy_dirty = false;
            }
        }
    }

    fn publish(&self, event: AgentEvent) {
        tracing::info!(
            target: "nanocentaur::observed",
            agent_id = %self.id,
            event = ?event,
            "durable managed event observed"
        );
        let _ = self.event_sender.send(event);
    }

    fn attach_turn(&mut self, turn_id: String, managed_turn: crate::ManagedTurn) {
        if let Some(turn) = self.turns.get_mut(&turn_id) {
            turn.control = Some(managed_turn.control());
        }
        let sender = self.self_sender.clone();
        let span = tracing::info_span!(
            parent: &tracing::Span::current(),
            "nanocentaur.turn.result",
            agent.id = self.id,
            turn.id = turn_id,
        );
        tokio::spawn(
            async move {
                let result = managed_turn.result().await;
                drop(
                    sender
                        .send(AgentCommand::TurnFinished {
                            turn_id,
                            result: Box::new(result),
                        })
                        .await,
                );
            }
            .instrument(span),
        );
    }

    async fn recover_pending(&mut self) -> Result<(), ManagerError> {
        let pending = self
            .turn_order
            .iter()
            .filter(|turn_id| {
                self.turns
                    .get(*turn_id)
                    .is_some_and(|turn| !turn.view.state.is_terminal())
            })
            .cloned()
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return Ok(());
        }
        self.ensure_runtime().await?;
        let managed = self
            .runtime
            .as_ref()
            .map(|runtime| Arc::clone(&runtime.agent))
            .ok_or_else(|| AgentError::Backend("runtime disappeared".to_owned()))?;
        let mut started = false;
        for turn_id in pending {
            if self.cancel_requested.contains(&turn_id) {
                self.finish_turn(&turn_id, Err(AgentError::TurnNotCancellable))
                    .await;
                continue;
            }
            let inputs = self
                .turns
                .get(&turn_id)
                .map(|turn| turn.inputs.clone())
                .ok_or(ManagerError::NotFound)?;
            let Some(first) = inputs.first().cloned() else {
                self.finish_turn(
                    &turn_id,
                    Err(AgentError::Backend(
                        "durable turn has no input content".to_owned(),
                    )),
                )
                .await;
                continue;
            };
            if !started {
                started = true;
                self.active_turn = Some(turn_id.clone());
                if let Some(turn) = self.turns.get_mut(&turn_id) {
                    turn.view.state = TurnStatus::Running;
                }
                let event = self
                    .sessions
                    .mark_started(self.id.clone(), turn_id.clone())
                    .await?;
                self.publish(event);
            }
            match managed.prompt(to_prompt(first)).await {
                Ok(managed_turn) => {
                    for steer in inputs.iter().skip(1).cloned() {
                        managed_turn.control().steer(to_prompt(steer)).await?;
                    }
                    self.attach_turn(turn_id, managed_turn);
                }
                Err(error) => self.finish_turn(&turn_id, Err(error)).await,
            }
        }
        Ok(())
    }
}

/// Replay-then-live ordered event cursor for one managed agent.
pub struct EventCursor {
    sender: AgentSender,
    receiver: broadcast::Receiver<AgentEvent>,
    after_event_id: u64,
    replay: VecDeque<AgentEvent>,
    replay_exhausted: bool,
}

impl EventCursor {
    /// Returns the next ordered event, recovering transparently after
    /// broadcast lag by replaying from `SQLite`.
    pub async fn recv(&mut self) -> Option<AgentEvent> {
        loop {
            if let Some(event) = self.replay.pop_front() {
                self.after_event_id = event.id;
                return Some(event);
            }
            if !self.replay_exhausted {
                self.replay().await?;
                continue;
            }
            match self.receiver.recv().await {
                Ok(event) if event.id > self.after_event_id => {
                    self.after_event_id = event.id;
                    return Some(event);
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    self.replay_exhausted = false;
                    self.replay().await?;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    async fn replay(&mut self) -> Option<()> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(AgentCommand::Replay {
                after_event_id: self.after_event_id,
                reply,
            })
            .await
            .ok()?;
        let replay = response.await.ok()?.ok()?;
        self.replay = replay.events;
        self.replay_exhausted = replay.exhausted;
        Some(())
    }
}

const fn runtime_event_name(kind: AgentEventKind) -> &'static str {
    match kind {
        AgentEventKind::ApiEvent => "api.event",
        AgentEventKind::AssistantDelta => "assistant.delta",
        AgentEventKind::AssistantMessage => "assistant.message",
        AgentEventKind::ReasoningSummaryDelta => "reasoning.summary.delta",
        AgentEventKind::RunStarted => "run.started",
        AgentEventKind::RunSteered => "run.steered",
        AgentEventKind::RunError => "run.error",
        AgentEventKind::RunCompleted => "run.completed",
        AgentEventKind::RunFailed => "run.failed",
        AgentEventKind::ToolCall => "tool.call",
        AgentEventKind::ToolResult => "tool.result",
        AgentEventKind::ModelWarmupStarted => "model.warmup.started",
        AgentEventKind::ModelWarmupCompleted => "model.warmup.completed",
        AgentEventKind::ModelWarmupFailed => "model.warmup.failed",
        AgentEventKind::ModelCallStarted => "model.call.started",
        AgentEventKind::ModelCallCompleted => "model.call.completed",
        AgentEventKind::ModelCallFailed => "model.call.failed",
        AgentEventKind::ModelCompactionStarted => "model.compaction.started",
        AgentEventKind::ModelCompactionCompleted => "model.compaction.completed",
        AgentEventKind::ModelCompactionFailed => "model.compaction.failed",
        AgentEventKind::ModelAttemptStarted => "model.attempt.started",
        AgentEventKind::ModelAttemptFailed => "model.attempt.failed",
        AgentEventKind::ModelAttemptRetrying => "model.attempt.retrying",
        AgentEventKind::ModelConnectionStarted => "model.connection.started",
        AgentEventKind::ModelConnectionCompleted => "model.connection.completed",
        AgentEventKind::ModelConnectionFailed => "model.connection.failed",
    }
}

fn to_prompt(content: Vec<ContentBlock>) -> Prompt {
    Prompt::content(content.into_iter().map(|block| match block {
        ContentBlock::Text { text } => UserInput::Text { text },
        ContentBlock::ImageUrl { url, detail } => UserInput::Image {
            image_url: url,
            detail,
        },
        ContentBlock::AudioUrl { url } => UserInput::Audio { audio_url: url },
    }))
}

fn validate_content(content: &[ContentBlock]) -> Result<(), ManagerError> {
    if content.is_empty() || content.len() > MAX_CONTENT_BLOCKS {
        return Err(ManagerError::Invalid("content must contain 1 to 64 blocks"));
    }
    let encoded = serde_json::to_vec(content)
        .map_err(|_| ManagerError::Invalid("content must be valid JSON"))?;
    if encoded.len() > MAX_CONTENT_BYTES {
        return Err(ManagerError::Invalid(
            "content must not exceed 1048576 encoded bytes",
        ));
    }
    for block in content {
        match block {
            ContentBlock::Text { text } if text.trim().is_empty() => {
                return Err(ManagerError::Invalid("text blocks must not be empty"));
            }
            ContentBlock::ImageUrl { url, .. } | ContentBlock::AudioUrl { url } => {
                let url = Url::parse(url).map_err(|_| {
                    ManagerError::Invalid("media URLs must be absolute HTTP(S) URLs")
                })?;
                if !matches!(url.scheme(), "http" | "https") {
                    return Err(ManagerError::Invalid(
                        "media URLs must be absolute HTTP(S) URLs",
                    ));
                }
            }
            ContentBlock::Text { .. } => {}
        }
    }
    Ok(())
}

fn validate_idempotency_key(value: Option<&str>) -> Result<(), ManagerError> {
    if value.is_some_and(|value| value.is_empty() || value.len() > MAX_IDEMPOTENCY_KEY_BYTES) {
        Err(ManagerError::Invalid(
            "Idempotency-Key must contain 1 to 256 bytes",
        ))
    } else {
        Ok(())
    }
}

#[derive(Debug, Error)]
/// Managed lifecycle, validation, or durability failure.
pub enum ManagerError {
    /// The authorized agent or turn does not exist.
    #[error("agent or turn was not found")]
    NotFound,
    /// Request content or metadata violated a bounded public contract.
    #[error("invalid request: {0}")]
    Invalid(&'static str),
    /// The active turn cannot accept additional steering immediately.
    #[error("the active turn's steering queue is full")]
    SteerQueueFull,
    /// Forking requires an idle source.
    #[error("agent must be idle before it can be forked")]
    AgentBusy,
    /// The requested completed fork boundary does not exist.
    #[error("completed fork boundary was not found")]
    ForkBoundaryNotFound,
    /// The disposable in-process actor stopped unexpectedly.
    #[error("agent actor stopped")]
    ActorStopped,
    /// `SQLite` command or replay persistence failed.
    #[error("session durability failed: {0}")]
    Durability(String),
    /// The configured backend agent failed.
    #[error("managed agent could not be created or controlled")]
    Agent(#[from] AgentError),
}

impl From<SessionError> for ManagerError {
    fn from(error: SessionError) -> Self {
        match error {
            SessionError::ForkBoundaryNotFound => Self::ForkBoundaryNotFound,
            error => Self::Durability(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;
    use nanocodex_agent::{CostStatus, TurnUsage};
    use nanocodex_oai_api::{
        pricing::{ServiceTier, estimate},
        responses::{InputTokenDetails, Usage},
    };
    use serde_json::json;

    use super::*;
    use crate::{AgentConfig, EffectivePrincipal, ManagedTurn, MockAgentFactory, SpawnedAgent};

    struct SilentFactory;

    struct SilentAgent;

    struct SilentTurnControl;

    #[async_trait]
    impl ManagedAgentFactory for SilentFactory {
        async fn create(&self, _spec: AgentSpec) -> Result<SpawnedAgent, AgentError> {
            let (_events, receiver) = mpsc::channel(1);
            Ok(SpawnedAgent::new(Arc::new(SilentAgent), receiver))
        }
    }

    #[async_trait]
    impl ManagedAgent for SilentAgent {
        async fn prompt(&self, _prompt: Prompt) -> Result<ManagedTurn, AgentError> {
            Ok(ManagedTurn::new(
                Arc::new(SilentTurnControl),
                std::future::ready(Ok(AgentRunResult::new(
                    "silent completion",
                    None,
                    TurnUsage::default(),
                ))),
            ))
        }
    }

    #[async_trait]
    impl ManagedTurnControl for SilentTurnControl {
        async fn steer(&self, _prompt: Prompt) -> Result<(), AgentError> {
            Err(AgentError::TurnNotSteerable)
        }

        async fn cancel(&self) -> Result<(), AgentError> {
            Err(AgentError::TurnNotCancellable)
        }
    }

    fn identity(id: &str) -> AgentIdentity {
        AgentIdentity {
            id: id.to_owned(),
            owner_client_id: "client".to_owned(),
            context_key: None,
            principal: EffectivePrincipal {
                id: "principal".to_owned(),
                agent_config: AgentConfig::default(),
                permissions: AgentCapabilities::default(),
                secret_revision: 0,
            },
            created_at: Utc::now(),
        }
    }

    fn turn(text: &str) -> CreateTurn {
        CreateTurn {
            delivery: TurnDelivery::Steer,
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
        }
    }

    fn priced_usage() -> TurnUsage {
        let estimated_cost = estimate(
            &Usage {
                input_tokens: 1_000,
                input_tokens_details: Some(InputTokenDetails {
                    cached_tokens: 100,
                    cache_write_tokens: 50,
                }),
                output_tokens: 200,
                output_tokens_details: None,
                total_tokens: 1_200,
            },
            ServiceTier::Standard,
        );
        serde_json::from_value(json!({
            "input_tokens": 1_000,
            "cached_input_tokens": 100,
            "cache_write_input_tokens": 50,
            "output_tokens": 200,
            "reasoning_output_tokens": 120,
            "total_tokens": 1_200,
            "estimated_cost": estimated_cost,
            "cost_status": "estimated_from_usage"
        }))
        .unwrap()
    }

    async fn wait_for_completed(
        manager: &AgentManager,
        identity: AgentIdentity,
        turn_id: &str,
    ) -> TurnView {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let view = manager.get_turn(identity.clone(), turn_id).await.unwrap();
                if view.state.is_terminal() {
                    return view;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn agent_actor_owns_one_ordered_typed_event_stream() {
        let directory = tempfile::tempdir().unwrap();
        let manager = AgentManager::new(
            Arc::new(MockAgentFactory::new(Duration::from_millis(5)).usage(priced_usage())),
            directory.path(),
        )
        .unwrap();
        let identity = identity("agent");
        manager.register(identity.clone()).await.unwrap();
        let mut events = manager.events(identity.clone(), 0).await.unwrap();
        manager
            .create_turn(
                identity,
                CreateTurn {
                    delivery: TurnDelivery::Steer,
                    content: vec![ContentBlock::Text {
                        text: "hello".to_owned(),
                    }],
                },
                None,
            )
            .await
            .unwrap();

        let mut saw_runtime = false;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap();
            match event.payload {
                AgentEventPayload::Runtime { .. } => saw_runtime = true,
                AgentEventPayload::TurnCompleted { .. } => break,
                AgentEventPayload::TurnStarted { .. }
                | AgentEventPayload::TurnQueued { .. }
                | AgentEventPayload::TurnCancelRequested
                | AgentEventPayload::TurnInterrupted { .. }
                | AgentEventPayload::TurnCancelled
                | AgentEventPayload::TurnFailed { .. } => {}
            }
        }
        assert!(saw_runtime);
    }

    #[tokio::test]
    async fn completion_does_not_wait_for_optional_runtime_events() {
        let directory = tempfile::tempdir().unwrap();
        let manager = AgentManager::new(Arc::new(SilentFactory), directory.path()).unwrap();
        let identity = identity("silent");
        manager.register(identity.clone()).await.unwrap();
        let response = manager
            .create_turn(identity.clone(), turn("complete without events"), None)
            .await
            .unwrap();

        let completed = tokio::time::timeout(
            Duration::from_millis(250),
            wait_for_completed(&manager, identity, &response.turn_id),
        )
        .await
        .expect("turn results must remain independent from the event stream");
        assert_eq!(completed.state, TurnStatus::Completed);
        let [ContentBlock::Text { text }] = completed.output.as_slice() else {
            panic!("expected one text output block");
        };
        assert_eq!(text, "silent completion");
    }

    #[tokio::test]
    async fn deletion_is_rejected_atomically_while_a_turn_is_active() {
        let directory = tempfile::tempdir().unwrap();
        let manager = AgentManager::new(
            Arc::new(MockAgentFactory::new(Duration::from_millis(100))),
            directory.path(),
        )
        .unwrap();
        let identity = identity("delete-race");
        manager.register(identity.clone()).await.unwrap();
        let response = manager
            .create_turn(identity.clone(), turn("still running"), None)
            .await
            .unwrap();

        assert!(matches!(
            manager.delete(&identity.id).await,
            Err(ManagerError::AgentBusy)
        ));
        wait_for_completed(&manager, identity.clone(), &response.turn_id).await;
        manager.delete(&identity.id).await.unwrap();
        assert!(
            manager
                .sessions
                .turn(identity.id, response.turn_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn durable_event_replay_is_paged_without_losing_order() {
        let directory = tempfile::tempdir().unwrap();
        let manager = AgentManager::new(
            Arc::new(MockAgentFactory::new(Duration::ZERO)),
            directory.path(),
        )
        .unwrap();
        let identity = identity("paged-replay");
        manager.register(identity.clone()).await.unwrap();
        let event_count = EVENT_REPLAY_PAGE_SIZE * 3 + 17;
        manager
            .sessions
            .append_events(
                identity.id.clone(),
                (0..event_count)
                    .map(|_| (None, AgentEventPayload::TurnCancelRequested))
                    .collect(),
            )
            .await
            .unwrap();

        let mut events = manager.events(identity, 0).await.unwrap();
        for expected in 1..=event_count {
            let event = events.recv().await.unwrap();
            assert_eq!(event.id, u64::try_from(expected).unwrap());
        }
    }

    #[tokio::test]
    async fn completed_turns_events_and_idempotency_survive_restart() {
        let directory = tempfile::tempdir().unwrap();
        let identity = identity("durable");
        let manager = AgentManager::new(
            Arc::new(MockAgentFactory::new(Duration::from_millis(5)).usage(priced_usage())),
            directory.path(),
        )
        .unwrap();
        manager.register(identity.clone()).await.unwrap();
        let response = manager
            .create_turn(
                identity.clone(),
                turn("persist me"),
                Some("request-1".to_owned()),
            )
            .await
            .unwrap();
        let completed = wait_for_completed(&manager, identity.clone(), &response.turn_id).await;
        assert_eq!(completed.state, TurnStatus::Completed);
        let completed_usage = completed.usage.clone().unwrap();
        assert_eq!(
            completed_usage.cost_status(),
            CostStatus::EstimatedFromUsage
        );
        assert!(completed_usage.estimated_cost().is_some());
        let mut first_events = manager.events(identity.clone(), 0).await.unwrap();
        let (terminal_id, terminal_usage) = loop {
            let event = first_events.recv().await.unwrap();
            if let AgentEventPayload::TurnCompleted { usage, .. } = event.payload {
                break (event.id, usage);
            }
        };
        assert_eq!(terminal_usage, completed_usage);
        manager.shutdown().await.unwrap();
        drop(manager);

        let restarted = AgentManager::new(
            Arc::new(MockAgentFactory::new(Duration::from_millis(5))),
            directory.path(),
        )
        .unwrap();
        restarted.register(identity.clone()).await.unwrap();
        let restored = restarted
            .get_turn(identity.clone(), &response.turn_id)
            .await
            .unwrap();
        assert_eq!(restored.state, TurnStatus::Completed);
        assert_eq!(restored.usage.as_ref(), Some(&completed_usage));
        assert_eq!(
            restarted
                .find_turn_by_idempotency_key(identity.clone(), "request-1")
                .await
                .unwrap()
                .unwrap()
                .turn_id,
            response.turn_id
        );
        let mut replay = restarted.events(identity, 0).await.unwrap();
        let last = loop {
            let event = replay.recv().await.unwrap();
            if event.id == terminal_id {
                break event;
            }
        };
        let AgentEventPayload::TurnCompleted { usage, .. } = last.payload else {
            panic!("expected a durable terminal completion event");
        };
        assert_eq!(usage, completed_usage);
    }

    #[tokio::test]
    async fn interrupted_running_turn_is_retried_from_sqlite() {
        let directory = tempfile::tempdir().unwrap();
        let identity = identity("interrupted");
        let manager = AgentManager::new(
            Arc::new(MockAgentFactory::new(Duration::from_secs(1))),
            directory.path(),
        )
        .unwrap();
        manager.register(identity.clone()).await.unwrap();
        let response = manager
            .create_turn(identity.clone(), turn("retry me"), None)
            .await
            .unwrap();
        manager.shutdown().await.unwrap();
        drop(manager);

        let restarted = AgentManager::new(
            Arc::new(MockAgentFactory::new(Duration::from_millis(5))),
            directory.path(),
        )
        .unwrap();
        restarted.register(identity.clone()).await.unwrap();
        let completed = wait_for_completed(&restarted, identity.clone(), &response.turn_id).await;
        assert_eq!(completed.state, TurnStatus::Completed);
        let mut events = restarted.events(identity, 0).await.unwrap();
        let mut interrupted = false;
        loop {
            match events.recv().await.unwrap().payload {
                AgentEventPayload::TurnInterrupted { retrying } => interrupted = retrying,
                AgentEventPayload::TurnCompleted { .. } => break,
                AgentEventPayload::TurnStarted { .. }
                | AgentEventPayload::TurnQueued { .. }
                | AgentEventPayload::TurnCancelRequested
                | AgentEventPayload::TurnCancelled
                | AgentEventPayload::TurnFailed { .. }
                | AgentEventPayload::Runtime { .. } => {}
            }
        }
        assert!(interrupted);
    }

    #[tokio::test]
    async fn fork_copies_session_history_without_copying_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let source = identity("source");
        let target = identity("target");
        let manager = AgentManager::new(
            Arc::new(MockAgentFactory::new(Duration::from_millis(5)).usage(priced_usage())),
            directory.path(),
        )
        .unwrap();
        manager.register(source.clone()).await.unwrap();
        let response = manager
            .create_turn(source.clone(), turn("branch here"), None)
            .await
            .unwrap();
        wait_for_completed(&manager, source.clone(), &response.turn_id).await;
        let source_workspace = directory.path().join("workspaces").join("source");
        std::fs::create_dir_all(&source_workspace).unwrap();
        std::fs::write(source_workspace.join("pet.txt"), "do not clone").unwrap();

        let forked = manager
            .fork(source, target.clone(), Some(response.turn_id.as_str()))
            .await
            .unwrap();
        assert_eq!(
            forked.forked_from.turn_id.as_deref(),
            Some(response.turn_id.as_str())
        );
        let inherited = manager
            .get_turn(target.clone(), &response.turn_id)
            .await
            .unwrap();
        assert_eq!(inherited.state, TurnStatus::Completed);
        assert_eq!(inherited.usage, Some(priced_usage()));
        assert!(
            !directory
                .path()
                .join("workspaces")
                .join("target")
                .join("pet.txt")
                .exists()
        );
        let first = manager
            .events(target, 0)
            .await
            .unwrap()
            .recv()
            .await
            .unwrap();
        assert_eq!(first.agent_id, "target");
    }
}
