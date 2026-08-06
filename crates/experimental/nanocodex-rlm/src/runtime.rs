use std::{
    collections::HashMap,
    num::NonZeroUsize,
    sync::{Arc, Weak},
    time::Duration,
};

use nanocodex::{
    AgentEvents, Nanocodex, NanocodexBuilder, PromptRoute, Tools, Turn, TurnControl,
    agent::AgentHandle,
};
use tokio::sync::{Mutex, Notify, Semaphore, broadcast};
use tokio::task::JoinHandle;

use crate::{
    HarnessSnapshot, LaunchSnapshot, RlmAgentEvidence, RlmAgentId, RlmAgentSummary, RlmEvent,
    RlmEventKind, RlmEvidence, RlmMessage, RlmStatus, RlmTurnEvidence, RlmUsage, SubagentSpec,
    harness::{AppliedHarnessRevision, HarnessEdit, HarnessStore, render_context},
};

const DEFAULT_MAX_ACTIVE_TURNS: usize = 16;
const DEFAULT_MAX_DEPTH: usize = 4;
const EVENT_CHANNEL_CAPACITY: usize = 1_024;
const MAX_MESSAGE_BYTES: usize = 2 * 1_024;

/// Immutable bounds applied to one recursive runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RlmPolicy {
    max_active_turns: NonZeroUsize,
    max_depth: NonZeroUsize,
    harness_refinement: bool,
}

/// Invalid recursive runtime policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RlmPolicyError {
    /// No recursive model turn could be admitted.
    #[error("RLM max_active_turns must be greater than zero")]
    ZeroActiveTurns,
    /// Clean children could never be created.
    #[error("RLM max_depth must be greater than zero")]
    ZeroDepth,
}

/// Failure to execute one recursive lifecycle operation.
#[derive(Debug, thiserror::Error)]
pub enum RlmRuntimeError {
    /// The owning runtime no longer accepts work.
    #[error("RLM runtime is closed")]
    Closed,
    /// The requested harness specification was unavailable.
    #[error("unknown or disabled subagent specification `{0}`")]
    UnknownSpecification(String),
    /// The configured recursive depth was exhausted.
    #[error("RLM recursion depth limit {0} reached")]
    DepthLimit(usize),
    /// The configured active-turn capacity was exhausted.
    #[error("RLM active-turn capacity {0} reached")]
    Capacity(usize),
    /// A referenced retained child was unavailable.
    #[error("unknown subagent `{0}`")]
    UnknownAgent(String),
    /// The caller does not have lifecycle authority over the target.
    #[error("the calling agent cannot manage subagent `{0}`")]
    Unauthorized(String),
    /// A closed child cannot accept more work.
    #[error("subagent `{0}` is closed")]
    AgentClosed(String),
    /// A supplied task or message was invalid.
    #[error("invalid RLM input: {0}")]
    InvalidInput(String),
    /// Harness mutation is disabled for this runtime, such as during held-out evaluation.
    #[error("continual harness refinement is disabled")]
    HarnessReadOnly,
    /// The underlying Nanocodex child lifecycle failed.
    #[error("subagent lifecycle failed: {0}")]
    Agent(String),
    /// Durable continual harness state could not be read or changed.
    #[error("continual harness failed: {0}")]
    Harness(String),
}

/// Owning process-local recursive runtime.
#[derive(Clone)]
pub struct RlmRuntime {
    pub(crate) state: Arc<RuntimeState>,
}

/// Weak, cycle-free installer captured by `NanocodexBuilder::tools_factory`.
#[derive(Clone)]
pub struct RlmTools {
    pub(crate) state: Weak<RuntimeState>,
}

pub(crate) struct RuntimeState {
    pub(crate) launch: LaunchSnapshot,
    pub(crate) harness: HarnessStore,
    policy: RlmPolicy,
    active: Arc<Semaphore>,
    registry: Mutex<Registry>,
    changed: Notify,
    updates: broadcast::Sender<RlmEvent>,
}

#[derive(Default)]
struct Registry {
    closed: bool,
    root_by_session: HashMap<Box<str>, Box<str>>,
    scopes: HashMap<Box<str>, Scope>,
}

#[derive(Default)]
struct Scope {
    finalized: bool,
    finalization_complete: bool,
    root_handle: Option<AgentHandle>,
    creation_order: Vec<RlmAgentId>,
    children: HashMap<RlmAgentId, Child>,
    messages: Vec<RlmMessage>,
    events: Vec<RlmEvent>,
    next_message_id: u64,
    delivered_through: HashMap<Box<str>, u64>,
}

struct Child {
    agent: Nanocodex,
    parent: Option<RlmAgentId>,
    depth: usize,
    specification: Box<str>,
    task: Box<str>,
    status: RlmStatus,
    last_message: Option<Box<str>>,
    error: Option<Box<str>>,
    generation: u64,
    active_control: Option<TurnControl>,
    active_monitor: Option<JoinHandle<()>>,
    event_forwarder: Option<JoinHandle<()>>,
    turns: Vec<RlmTurnEvidence>,
    is_refiner: bool,
    refinement_applied: bool,
}

struct Reservation {
    root_session_id: Box<str>,
    parent: Option<RlmAgentId>,
    depth: usize,
    id: RlmAgentId,
}

impl Default for RlmPolicy {
    fn default() -> Self {
        Self {
            max_active_turns: NonZeroUsize::new(DEFAULT_MAX_ACTIVE_TURNS)
                .unwrap_or(NonZeroUsize::MIN),
            max_depth: NonZeroUsize::new(DEFAULT_MAX_DEPTH).unwrap_or(NonZeroUsize::MIN),
            harness_refinement: true,
        }
    }
}

impl RlmPolicy {
    /// Creates an explicit recursive depth and active-turn policy.
    ///
    /// # Errors
    ///
    /// Returns an error when either bound is zero.
    pub fn new(max_active_turns: usize, max_depth: usize) -> Result<Self, RlmPolicyError> {
        let max_active_turns =
            NonZeroUsize::new(max_active_turns).ok_or(RlmPolicyError::ZeroActiveTurns)?;
        let max_depth = NonZeroUsize::new(max_depth).ok_or(RlmPolicyError::ZeroDepth)?;
        Ok(Self {
            max_active_turns,
            max_depth,
            harness_refinement: true,
        })
    }

    /// Maximum simultaneously active recursive child turns.
    #[must_use]
    pub const fn max_active_turns(self) -> usize {
        self.max_active_turns.get()
    }

    /// Maximum child depth below a root agent.
    #[must_use]
    pub const fn max_depth(self) -> usize {
        self.max_depth.get()
    }

    /// Enables or disables durable harness mutation and background refinement.
    #[must_use]
    pub const fn with_harness_refinement(mut self, enabled: bool) -> Self {
        self.harness_refinement = enabled;
        self
    }

    /// Whether this runtime may mutate its durable harness document.
    #[must_use]
    pub const fn harness_refinement_enabled(self) -> bool {
        self.harness_refinement
    }
}

impl RlmRuntime {
    /// Creates a process-local runtime from one frozen launch snapshot.
    #[must_use]
    pub fn new(launch: LaunchSnapshot) -> Self {
        Self::with_policy(launch, RlmPolicy::default())
    }

    /// Creates a process-local runtime with explicit immutable bounds.
    #[must_use]
    pub fn with_policy(launch: LaunchSnapshot, policy: RlmPolicy) -> Self {
        let (updates, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let harness = HarnessStore::new(launch.harness().clone());
        Self {
            state: Arc::new(RuntimeState {
                launch,
                harness,
                policy,
                active: Arc::new(Semaphore::new(policy.max_active_turns())),
                registry: Mutex::new(Registry::default()),
                changed: Notify::new(),
                updates,
            }),
        }
    }

    /// Returns a weak installer suitable for every root and clean-child tools factory.
    #[must_use]
    pub fn tools(&self) -> RlmTools {
        RlmTools {
            state: Arc::downgrade(&self.state),
        }
    }

    /// Installs this runtime into a normal non-VM agent recipe.
    ///
    /// Every clean child receives a freshly materialized handler targeting its
    /// own driver while preserving the caller's selected base tools.
    #[must_use]
    pub fn agent_builder(&self, builder: NanocodexBuilder, tools: Tools) -> NanocodexBuilder {
        let installer = self.tools();
        builder
            .append_instructions(self.launch().root_instructions())
            .tools_factory(move |agent| installer.install(tools.clone(), agent))
    }

    /// Subscribes to recursive child lifecycle and raw agent events.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<RlmEvent> {
        self.state.updates.subscribe()
    }

    /// Returns the immutable launch snapshot shared by this runtime.
    #[must_use]
    pub fn launch(&self) -> &LaunchSnapshot {
        &self.state.launch
    }

    /// Returns the latest durable continual harness snapshot.
    pub async fn harness(&self) -> HarnessSnapshot {
        self.state.harness.snapshot().await
    }

    /// Projects all evidence currently retained for `root_session_id`.
    pub async fn evidence(&self, root_session_id: &str) -> Option<RlmEvidence> {
        self.state.evidence(root_session_id).await
    }

    /// Projects every root family in stable session-identity order.
    pub async fn all_evidence(&self) -> Vec<RlmEvidence> {
        self.state.all_evidence().await
    }

    /// Stops and joins every descendant of one root before an external
    /// observer, such as an evaluator verifier, examines shared state.
    pub async fn finalize_root(&self, root_session_id: &str) -> Result<(), RlmRuntimeError> {
        self.state.finalize_root(root_session_id).await
    }

    /// Rejects new work, closes every retained child, and joins agent cleanup.
    pub async fn shutdown(&self) {
        let roots = {
            let mut registry = self.state.registry.lock().await;
            registry.closed = true;
            registry.scopes.keys().cloned().collect::<Vec<_>>()
        };
        self.state.active.close();
        for root in roots {
            drop(self.state.finalize_root(&root).await);
        }
    }
}

impl RuntimeState {
    pub(crate) async fn spawn(
        self: &Arc<Self>,
        parent_handle: &AgentHandle,
        caller_session_id: &str,
        specification: &str,
        task: String,
    ) -> Result<RlmAgentSummary, RlmRuntimeError> {
        validate_text(&task, "subagent task", 64 * 1_024)?;
        let spec = self
            .harness
            .enabled_subagent(specification)
            .await
            .ok_or_else(|| RlmRuntimeError::UnknownSpecification(specification.to_owned()))?;
        self.spawn_spec(parent_handle, caller_session_id, spec, task, false)
            .await
    }

    pub(crate) async fn refine(
        self: &Arc<Self>,
        parent_handle: &AgentHandle,
        caller_session_id: &str,
        observation: String,
    ) -> Result<RlmAgentSummary, RlmRuntimeError> {
        if !self.policy.harness_refinement_enabled() {
            return Err(RlmRuntimeError::HarnessReadOnly);
        }
        validate_text(&observation, "refinement observation", 64 * 1_024)?;
        let snapshot = self.harness.snapshot().await;
        let task = format!(
            "Trajectory observation:\n{}\n\nCurrent durable harness:\n{}",
            observation.trim(),
            render_context(&snapshot)
        );
        let spec = SubagentSpec {
            id: "harness-refiner".into(),
            name: "Harness refiner".into(),
            description: "Reviews trajectory evidence and proposes one minimal durable update."
                .into(),
            instructions: self.launch.prompts().refiner().to_owned().into_boxed_str(),
            enabled: true,
        };
        self.spawn_spec(parent_handle, caller_session_id, spec, task, true)
            .await
    }

    async fn spawn_spec(
        self: &Arc<Self>,
        parent_handle: &AgentHandle,
        caller_session_id: &str,
        spec: SubagentSpec,
        task: String,
        is_refiner: bool,
    ) -> Result<RlmAgentSummary, RlmRuntimeError> {
        let specification = spec.id().to_owned();
        self.remember_root_handle(caller_session_id, parent_handle.clone())
            .await;
        let permit = Arc::clone(&self.active)
            .try_acquire_owned()
            .map_err(|_| RlmRuntimeError::Capacity(self.policy.max_active_turns()))?;
        let reservation = self.reserve(caller_session_id).await?;
        let (agent, events) = parent_handle
            .spawn()
            .await
            .map_err(|error| RlmRuntimeError::Agent(error.to_string()))?;
        let session_id = agent.session_id().to_string().into_boxed_str();
        let prompt = child_instructions(&self.launch, &spec, &reservation, &task);
        if let Err(error) = agent.append_developer_message(prompt).await {
            drop(agent.shutdown().await);
            return Err(RlmRuntimeError::Agent(error.to_string()));
        }
        let summary = {
            let mut registry = self.registry.lock().await;
            if registry.closed {
                drop(registry);
                drop(agent.shutdown().await);
                return Err(RlmRuntimeError::Closed);
            }
            registry
                .root_by_session
                .insert(session_id.clone(), reservation.root_session_id.clone());
            let scope = registry
                .scopes
                .entry(reservation.root_session_id.clone())
                .or_default();
            if scope.finalized {
                drop(registry);
                drop(agent.shutdown().await);
                return Err(RlmRuntimeError::Closed);
            }
            let child = Child {
                agent: agent.clone(),
                parent: reservation.parent.clone(),
                depth: reservation.depth,
                specification: specification.into_boxed_str(),
                task: task.clone().into_boxed_str(),
                status: RlmStatus::Running,
                last_message: None,
                error: None,
                generation: 1,
                active_control: None,
                active_monitor: None,
                event_forwarder: None,
                turns: Vec::new(),
                is_refiner,
                refinement_applied: false,
            };
            let summary = child.summary(reservation.id.clone());
            scope.creation_order.push(reservation.id.clone());
            scope.children.insert(reservation.id.clone(), child);
            summary
        };
        self.emit(
            &reservation.root_session_id,
            &reservation.id,
            RlmEventKind::Added(summary.clone()),
        )
        .await;
        let event_forwarder = self.forward_events(
            reservation.root_session_id.clone(),
            reservation.id.clone(),
            events,
        );
        {
            let mut registry = self.registry.lock().await;
            if let Some(child) = registry
                .scopes
                .get_mut(&reservation.root_session_id)
                .and_then(|scope| scope.children.get_mut(&reservation.id))
            {
                child.event_forwarder = Some(event_forwarder);
            }
        }
        self.start_turn_with_permit(
            reservation.root_session_id,
            reservation.id,
            task,
            agent,
            permit,
            1,
        )
        .await?;
        Ok(summary)
    }

    async fn remember_root_handle(&self, caller_session_id: &str, handle: AgentHandle) {
        let mut registry = self.registry.lock().await;
        let root = registry.root_for(caller_session_id);
        let scope = registry.scopes.entry(root).or_default();
        if scope.agent_for_session(caller_session_id).is_none() {
            scope.root_handle = Some(handle);
        }
    }

    async fn reserve(&self, caller_session_id: &str) -> Result<Reservation, RlmRuntimeError> {
        let registry = self.registry.lock().await;
        if registry.closed {
            return Err(RlmRuntimeError::Closed);
        }
        let root_session_id = registry
            .root_by_session
            .get(caller_session_id)
            .cloned()
            .unwrap_or_else(|| caller_session_id.to_owned().into_boxed_str());
        let parent = registry
            .scopes
            .get(&root_session_id)
            .and_then(|scope| scope.agent_for_session(caller_session_id));
        if registry
            .scopes
            .get(&root_session_id)
            .is_some_and(|scope| scope.finalized)
        {
            return Err(RlmRuntimeError::Closed);
        }
        let depth = match &parent {
            Some(parent) => registry
                .scopes
                .get(&root_session_id)
                .and_then(|scope| scope.children.get(parent))
                .map_or(1, |child| child.depth.saturating_add(1)),
            None => 1,
        };
        if depth > self.policy.max_depth() {
            return Err(RlmRuntimeError::DepthLimit(self.policy.max_depth()));
        }
        Ok(Reservation {
            root_session_id,
            parent,
            depth,
            id: RlmAgentId::new(),
        })
    }

    async fn start_turn_with_permit(
        self: &Arc<Self>,
        root_session_id: Box<str>,
        id: RlmAgentId,
        prompt: String,
        agent: Nanocodex,
        permit: tokio::sync::OwnedSemaphorePermit,
        generation: u64,
    ) -> Result<(), RlmRuntimeError> {
        let turn = match agent.prompt(prompt.clone()).await {
            Ok(turn) => turn,
            Err(error) => {
                let message = error.to_string();
                let evidence = RlmTurnEvidence {
                    generation,
                    prompt: prompt.into_boxed_str(),
                    final_message: None,
                    error: Some(message.clone().into_boxed_str()),
                    usage: RlmUsage {
                        incomplete_turns: 1,
                        ..RlmUsage::default()
                    },
                };
                {
                    let mut registry = self.registry.lock().await;
                    if let Some(child) = registry
                        .scopes
                        .get_mut(&root_session_id)
                        .and_then(|scope| scope.children.get_mut(&id))
                    {
                        child.status = RlmStatus::Failed;
                        child.error = evidence.error.clone();
                        child.turns.push(evidence.clone());
                    }
                }
                self.emit(&root_session_id, &id, RlmEventKind::Turn(evidence))
                    .await;
                self.emit(
                    &root_session_id,
                    &id,
                    RlmEventKind::Status(RlmStatus::Failed),
                )
                .await;
                return Err(RlmRuntimeError::Agent(message));
            }
        };
        let control = turn.control();
        {
            let mut registry = self.registry.lock().await;
            let child = registry
                .scopes
                .get_mut(&root_session_id)
                .and_then(|scope| scope.children.get_mut(&id))
                .ok_or_else(|| RlmRuntimeError::UnknownAgent(id.to_string()))?;
            child.status = RlmStatus::Running;
            child.error = None;
            child.active_control = Some(control);
        }
        let state = Arc::clone(self);
        let monitor_root = root_session_id.clone();
        let monitor_id = id.clone();
        let monitor = tokio::spawn(async move {
            state
                .finish_turn(monitor_root, monitor_id, generation, prompt, turn)
                .await;
            drop(permit);
        });
        {
            let mut registry = self.registry.lock().await;
            let child = registry
                .scopes
                .get_mut(&root_session_id)
                .and_then(|scope| scope.children.get_mut(&id))
                .ok_or_else(|| RlmRuntimeError::UnknownAgent(id.to_string()))?;
            child.active_monitor = Some(monitor);
        }
        self.emit(
            &root_session_id,
            &id,
            RlmEventKind::Status(RlmStatus::Running),
        )
        .await;
        Ok(())
    }

    async fn finish_turn(
        self: Arc<Self>,
        root_session_id: Box<str>,
        id: RlmAgentId,
        generation: u64,
        prompt: String,
        turn: Turn,
    ) {
        let result = turn.result().await;
        let (status, evidence) = match result {
            Ok(result) => {
                let mut usage = RlmUsage::default();
                usage.add_turn(result.usage());
                (
                    RlmStatus::Idle,
                    RlmTurnEvidence {
                        generation,
                        prompt: prompt.into_boxed_str(),
                        final_message: Some(result.into_final_message().into_boxed_str()),
                        error: None,
                        usage,
                    },
                )
            }
            Err(error) => (
                RlmStatus::Failed,
                RlmTurnEvidence {
                    generation,
                    prompt: prompt.into_boxed_str(),
                    final_message: None,
                    error: Some(error.to_string().into_boxed_str()),
                    usage: RlmUsage {
                        incomplete_turns: 1,
                        ..RlmUsage::default()
                    },
                },
            ),
        };
        let effective_status = {
            let mut registry = self.registry.lock().await;
            let Some(child) = registry
                .scopes
                .get_mut(&root_session_id)
                .and_then(|scope| scope.children.get_mut(&id))
            else {
                return;
            };
            if child.generation != generation {
                return;
            }
            child.active_control = None;
            child.last_message = evidence.final_message.clone();
            child.error = evidence.error.clone();
            child.turns.push(evidence.clone());
            if child.status == RlmStatus::Running {
                child.status = status;
            }
            child.status
        };
        self.emit(&root_session_id, &id, RlmEventKind::Turn(evidence))
            .await;
        self.emit(
            &root_session_id,
            &id,
            RlmEventKind::Status(effective_status),
        )
        .await;
        self.changed.notify_waiters();
    }

    fn forward_events(
        self: &Arc<Self>,
        root_session_id: Box<str>,
        id: RlmAgentId,
        mut events: AgentEvents,
    ) -> JoinHandle<()> {
        let state = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let Some(state) = state.upgrade() else {
                    return;
                };
                state
                    .emit(&root_session_id, &id, RlmEventKind::Agent(event))
                    .await;
            }
        })
    }

    async fn emit(&self, root_session_id: &str, id: &RlmAgentId, kind: RlmEventKind) {
        let event = RlmEvent {
            root_session_id: root_session_id.to_owned().into_boxed_str(),
            agent_id: id.clone(),
            kind,
        };
        {
            let mut registry = self.registry.lock().await;
            if let Some(scope) = registry.scopes.get_mut(root_session_id) {
                scope.events.push(event.clone());
            }
        }
        drop(self.updates.send(event));
    }

    pub(crate) async fn harness_snapshot(&self) -> HarnessSnapshot {
        self.harness.snapshot().await
    }

    pub(crate) async fn apply_harness_edit(
        &self,
        caller_session_id: &str,
        caller_handle: AgentHandle,
        edit: HarnessEdit,
        trigger: String,
    ) -> Result<(AppliedHarnessRevision, usize, Vec<String>), RlmRuntimeError> {
        if !self.policy.harness_refinement_enabled() {
            return Err(RlmRuntimeError::HarnessReadOnly);
        }
        let claimed_refiner = self.claim_refiner_edit(caller_session_id).await?;
        let revision = match self.harness.apply(edit, trigger).await {
            Ok(revision) => revision,
            Err(error) => {
                if claimed_refiner {
                    self.release_refiner_edit(caller_session_id).await;
                }
                return Err(RlmRuntimeError::Harness(error.to_string()));
            }
        };
        let (queued, failures) = self
            .publish_harness_context(caller_session_id, caller_handle, &revision.context)
            .await;
        Ok((revision, queued, failures))
    }

    async fn claim_refiner_edit(&self, caller_session_id: &str) -> Result<bool, RlmRuntimeError> {
        let mut registry = self.registry.lock().await;
        let root = registry.root_for(caller_session_id);
        let Some(scope) = registry.scopes.get_mut(&root) else {
            return Ok(false);
        };
        let Some(id) = scope.agent_for_session(caller_session_id) else {
            return Ok(false);
        };
        let child = scope
            .children
            .get_mut(&id)
            .ok_or_else(|| RlmRuntimeError::UnknownAgent(id.to_string()))?;
        if !child.is_refiner {
            return Ok(false);
        }
        if child.refinement_applied {
            return Err(RlmRuntimeError::InvalidInput(
                "a background refiner may apply at most one harness edit".to_owned(),
            ));
        }
        child.refinement_applied = true;
        Ok(true)
    }

    async fn release_refiner_edit(&self, caller_session_id: &str) {
        let mut registry = self.registry.lock().await;
        let root = registry.root_for(caller_session_id);
        let Some(scope) = registry.scopes.get_mut(&root) else {
            return;
        };
        let Some(id) = scope.agent_for_session(caller_session_id) else {
            return;
        };
        if let Some(child) = scope.children.get_mut(&id)
            && child.is_refiner
        {
            child.refinement_applied = false;
        }
    }

    pub(crate) async fn rollback_harness(
        &self,
        caller_session_id: &str,
        caller_handle: AgentHandle,
        revision: u64,
        trigger: String,
    ) -> Result<(AppliedHarnessRevision, usize, Vec<String>), RlmRuntimeError> {
        if !self.policy.harness_refinement_enabled() {
            return Err(RlmRuntimeError::HarnessReadOnly);
        }
        let revision = self
            .harness
            .rollback(revision, trigger)
            .await
            .map_err(|error| RlmRuntimeError::Harness(error.to_string()))?;
        let (queued, failures) = self
            .publish_harness_context(caller_session_id, caller_handle, &revision.context)
            .await;
        Ok((revision, queued, failures))
    }

    async fn publish_harness_context(
        &self,
        caller_session_id: &str,
        caller_handle: AgentHandle,
        context: &str,
    ) -> (usize, Vec<String>) {
        let (root_handle, children) = {
            let mut registry = self.registry.lock().await;
            let root = registry.root_for(caller_session_id);
            let scope = registry.scopes.entry(root).or_default();
            if scope.agent_for_session(caller_session_id).is_none() {
                scope.root_handle = Some(caller_handle);
            }
            (
                scope.root_handle.clone(),
                scope
                    .children
                    .iter()
                    .filter(|(_, child)| child.status != RlmStatus::Closed)
                    .map(|(id, child)| (id.clone(), child.agent.clone()))
                    .collect::<Vec<_>>(),
            )
        };
        let mut queued = 0;
        let mut failures = Vec::new();
        if let Some(root) = root_handle {
            match root.append_developer_message(context.to_owned()).await {
                Ok(_) => queued += 1,
                Err(error) => failures.push(format!("root: {error}")),
            }
        }
        for (id, child) in children {
            match child.append_developer_message(context.to_owned()).await {
                Ok(_) => queued += 1,
                Err(error) => failures.push(format!("{id}: {error}")),
            }
        }
        (queued, failures)
    }

    pub(crate) async fn list(
        &self,
        caller_session_id: &str,
        include_closed: bool,
    ) -> Vec<RlmAgentSummary> {
        let registry = self.registry.lock().await;
        let Some(scope) = registry.scope_for(caller_session_id) else {
            return Vec::new();
        };
        scope
            .creation_order
            .iter()
            .filter_map(|id| {
                scope
                    .children
                    .get(id)
                    .map(|child| child.summary(id.clone()))
            })
            .filter(|summary| include_closed || summary.status != RlmStatus::Closed)
            .collect()
    }

    pub(crate) async fn send(
        self: &Arc<Self>,
        caller_session_id: &str,
        target: &str,
        message: String,
    ) -> Result<RlmMessage, RlmRuntimeError> {
        validate_text(&message, "agent message", MAX_MESSAGE_BYTES)?;
        let (root_session_id, caller, target_id, target_agent, generation, permit) = {
            let mut registry = self.registry.lock().await;
            let root_session_id = registry.root_for(caller_session_id);
            let scope = registry.scopes.entry(root_session_id.clone()).or_default();
            if scope.finalized {
                return Err(RlmRuntimeError::Closed);
            }
            let caller = scope.agent_for_session(caller_session_id);
            let target_id = if target == "parent" {
                let caller_id = caller.as_ref().ok_or_else(|| {
                    RlmRuntimeError::InvalidInput(
                        "root agents do not have a parent recipient".to_owned(),
                    )
                })?;
                scope
                    .children
                    .get(caller_id)
                    .ok_or_else(|| RlmRuntimeError::UnknownAgent(caller_id.to_string()))?
                    .parent
                    .clone()
            } else {
                Some(RlmAgentId(target.to_owned().into_boxed_str()))
            };
            let (target_agent, generation, permit) = match &target_id {
                Some(id) => {
                    let child = scope
                        .children
                        .get_mut(id)
                        .ok_or_else(|| RlmRuntimeError::UnknownAgent(id.to_string()))?;
                    if child.status == RlmStatus::Closed {
                        return Err(RlmRuntimeError::AgentClosed(id.to_string()));
                    }
                    let running = child.status == RlmStatus::Running;
                    let permit = if running {
                        None
                    } else {
                        Some(Arc::clone(&self.active).try_acquire_owned().map_err(|_| {
                            RlmRuntimeError::Capacity(self.policy.max_active_turns())
                        })?)
                    };
                    if !running {
                        child.generation = child.generation.saturating_add(1);
                        child.status = RlmStatus::Running;
                    }
                    (Some(child.agent.clone()), Some(child.generation), permit)
                }
                None => (None, None, None),
            };
            (
                root_session_id,
                caller,
                target_id,
                target_agent,
                generation,
                permit,
            )
        };
        let committed = self
            .commit_message(&root_session_id, caller, target_id.clone(), message.clone())
            .await;
        if let (Some(target_id), Some(agent), Some(generation)) =
            (target_id, target_agent, generation)
        {
            let route = agent
                .route_prompt(format!("Message from another agent:\n\n{message}"))
                .await
                .map_err(|error| RlmRuntimeError::Agent(error.to_string()));
            let route = match route {
                Ok(route) => route,
                Err(error) => {
                    if permit.is_some() {
                        let mut registry = self.registry.lock().await;
                        if let Some(child) = registry
                            .scopes
                            .get_mut(&root_session_id)
                            .and_then(|scope| scope.children.get_mut(&target_id))
                        {
                            child.status = RlmStatus::Failed;
                            child.error = Some(error.to_string().into_boxed_str());
                        }
                    }
                    self.changed.notify_waiters();
                    return Err(error);
                }
            };
            match route {
                PromptRoute::Steered => drop(permit),
                PromptRoute::Started(turn) => {
                    let permit = match permit {
                        Some(permit) => permit,
                        None => Arc::clone(&self.active)
                            .acquire_owned()
                            .await
                            .map_err(|_| RlmRuntimeError::Closed)?,
                    };
                    let control = turn.control();
                    {
                        let mut registry = self.registry.lock().await;
                        if let Some(child) = registry
                            .scopes
                            .get_mut(&root_session_id)
                            .and_then(|scope| scope.children.get_mut(&target_id))
                        {
                            child.status = RlmStatus::Running;
                            child.active_control = Some(control);
                        }
                    }
                    let state = Arc::clone(self);
                    let monitor_root = root_session_id.clone();
                    let monitor_id = target_id.clone();
                    let monitor = tokio::spawn(async move {
                        state
                            .finish_turn(monitor_root, monitor_id, generation, message, turn)
                            .await;
                        drop(permit);
                    });
                    let mut registry = self.registry.lock().await;
                    if let Some(child) = registry
                        .scopes
                        .get_mut(&root_session_id)
                        .and_then(|scope| scope.children.get_mut(&target_id))
                    {
                        child.active_monitor = Some(monitor);
                    }
                }
            }
        }
        Ok(committed)
    }

    async fn commit_message(
        &self,
        root_session_id: &str,
        from: Option<RlmAgentId>,
        to: Option<RlmAgentId>,
        message: String,
    ) -> RlmMessage {
        let committed = {
            let mut registry = self.registry.lock().await;
            let scope = registry
                .scopes
                .entry(root_session_id.to_owned().into_boxed_str())
                .or_default();
            scope.next_message_id = scope.next_message_id.saturating_add(1);
            let committed = RlmMessage {
                message_id: scope.next_message_id,
                from_agent_id: from,
                to_agent_id: to.clone(),
                message: message.into_boxed_str(),
            };
            scope.messages.push(committed.clone());
            committed
        };
        let event_id = to
            .or_else(|| committed.from_agent_id.clone())
            .unwrap_or_else(RlmAgentId::new);
        self.emit(
            root_session_id,
            &event_id,
            RlmEventKind::Message(committed.clone()),
        )
        .await;
        self.changed.notify_waiters();
        committed
    }

    pub(crate) async fn wait(
        &self,
        caller_session_id: &str,
        ids: &[RlmAgentId],
        timeout: Duration,
    ) -> Result<(Vec<RlmAgentSummary>, Vec<RlmMessage>, bool), RlmRuntimeError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            let observed = self.wait_projection(caller_session_id, ids).await?;
            if !observed.0.is_empty() || !observed.1.is_empty() {
                return Ok((observed.0, observed.1, false));
            }
            if tokio::time::timeout_at(deadline, &mut changed)
                .await
                .is_err()
            {
                let (agents, messages) = self.wait_projection(caller_session_id, ids).await?;
                return Ok((agents, messages, true));
            }
        }
    }

    async fn wait_projection(
        &self,
        caller_session_id: &str,
        ids: &[RlmAgentId],
    ) -> Result<(Vec<RlmAgentSummary>, Vec<RlmMessage>), RlmRuntimeError> {
        let mut registry = self.registry.lock().await;
        let root_session_id = registry.root_for(caller_session_id);
        let scope = registry.scopes.entry(root_session_id).or_default();
        for id in ids {
            if !scope.children.contains_key(id) {
                return Err(RlmRuntimeError::UnknownAgent(id.to_string()));
            }
        }
        let caller = scope.agent_for_session(caller_session_id);
        let agents = ids
            .iter()
            .filter_map(|id| {
                scope
                    .children
                    .get(id)
                    .map(|child| child.summary(id.clone()))
            })
            .filter(|summary| summary.status.is_wait_ready())
            .collect::<Vec<_>>();
        let mailbox = mailbox_key(caller.as_ref(), caller_session_id);
        let delivered = scope
            .delivered_through
            .get(mailbox.as_ref())
            .copied()
            .unwrap_or(0);
        let messages = scope
            .messages
            .iter()
            .filter(|message| {
                message.message_id > delivered && message.to_agent_id.as_ref() == caller.as_ref()
            })
            .cloned()
            .collect::<Vec<_>>();
        if let Some(last) = messages.last() {
            scope.delivered_through.insert(mailbox, last.message_id);
        }
        Ok((agents, messages))
    }

    pub(crate) async fn interrupt(
        &self,
        caller_session_id: &str,
        id: &RlmAgentId,
    ) -> Result<Vec<RlmAgentSummary>, RlmRuntimeError> {
        let (root, targets) = self.management_targets(caller_session_id, id).await?;
        let controls = {
            let mut registry = self.registry.lock().await;
            let scope = registry
                .scopes
                .get_mut(&root)
                .ok_or_else(|| RlmRuntimeError::UnknownAgent(id.to_string()))?;
            targets
                .iter()
                .filter_map(|target| {
                    let child = scope.children.get_mut(target)?;
                    child.status = RlmStatus::Interrupted;
                    child.active_control.take()
                })
                .collect::<Vec<_>>()
        };
        for control in controls {
            drop(control.cancel().await);
        }
        for target in &targets {
            self.emit(&root, target, RlmEventKind::Status(RlmStatus::Interrupted))
                .await;
        }
        self.changed.notify_waiters();
        Ok(self.summaries(&root, &targets).await)
    }

    pub(crate) async fn close(
        &self,
        caller_session_id: &str,
        id: &RlmAgentId,
    ) -> Result<Vec<RlmAgentSummary>, RlmRuntimeError> {
        let (root, targets) = self.management_targets(caller_session_id, id).await?;
        let agents = {
            let mut registry = self.registry.lock().await;
            let scope = registry
                .scopes
                .get_mut(&root)
                .ok_or_else(|| RlmRuntimeError::UnknownAgent(id.to_string()))?;
            targets
                .iter()
                .filter_map(|target| {
                    let child = scope.children.get_mut(target)?;
                    child.status = RlmStatus::Closed;
                    child.active_control = None;
                    Some(child.agent.clone())
                })
                .collect::<Vec<_>>()
        };
        for agent in agents {
            drop(agent.shutdown().await);
        }
        let tasks = {
            let mut registry = self.registry.lock().await;
            let scope = registry
                .scopes
                .get_mut(&root)
                .ok_or_else(|| RlmRuntimeError::UnknownAgent(id.to_string()))?;
            targets
                .iter()
                .flat_map(|target| {
                    let Some(child) = scope.children.get_mut(target) else {
                        return Vec::new();
                    };
                    [child.active_monitor.take(), child.event_forwarder.take()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        for task in tasks {
            drop(task.await);
        }
        for target in &targets {
            self.emit(&root, target, RlmEventKind::Status(RlmStatus::Closed))
                .await;
        }
        self.changed.notify_waiters();
        Ok(self.summaries(&root, &targets).await)
    }

    async fn management_targets(
        &self,
        caller_session_id: &str,
        id: &RlmAgentId,
    ) -> Result<(Box<str>, Vec<RlmAgentId>), RlmRuntimeError> {
        let registry = self.registry.lock().await;
        let root = registry.root_for(caller_session_id);
        let scope = registry
            .scopes
            .get(&root)
            .ok_or_else(|| RlmRuntimeError::UnknownAgent(id.to_string()))?;
        let caller = scope.agent_for_session(caller_session_id);
        if !scope.children.contains_key(id) {
            return Err(RlmRuntimeError::UnknownAgent(id.to_string()));
        }
        if let Some(caller) = &caller
            && (caller == id || !scope.is_descendant(caller, id))
        {
            return Err(RlmRuntimeError::Unauthorized(id.to_string()));
        }
        Ok((root, scope.subtree(id)))
    }

    async fn summaries(&self, root: &str, ids: &[RlmAgentId]) -> Vec<RlmAgentSummary> {
        let registry = self.registry.lock().await;
        let Some(scope) = registry.scopes.get(root) else {
            return Vec::new();
        };
        ids.iter()
            .filter_map(|id| {
                scope
                    .children
                    .get(id)
                    .map(|child| child.summary(id.clone()))
            })
            .collect()
    }

    async fn evidence(&self, root_session_id: &str) -> Option<RlmEvidence> {
        let harness = self.harness.snapshot().await;
        let registry = self.registry.lock().await;
        let scope = registry.scopes.get(root_session_id)?;
        let mut usage = RlmUsage::default();
        let agents = scope
            .creation_order
            .iter()
            .filter_map(|id| {
                let child = scope.children.get(id)?;
                let mut child_usage = RlmUsage::default();
                for turn in &child.turns {
                    child_usage.add(&turn.usage);
                }
                usage.add(&child_usage);
                Some(RlmAgentEvidence {
                    agent: child.summary(id.clone()),
                    turns: child.turns.clone(),
                })
            })
            .collect();
        Some(RlmEvidence {
            root_session_id: root_session_id.to_owned().into_boxed_str(),
            launch_digest: self.launch.digest().to_owned().into_boxed_str(),
            harness_revision: self.launch.harness().revision(),
            final_harness_revision: harness.revision(),
            final_harness_digest: harness.digest().to_owned().into_boxed_str(),
            refinements: harness.refinements().to_vec(),
            agents,
            messages: scope.messages.clone(),
            events: scope.events.clone(),
            usage,
        })
    }

    async fn all_evidence(&self) -> Vec<RlmEvidence> {
        let roots = {
            let registry = self.registry.lock().await;
            let mut roots = registry.scopes.keys().cloned().collect::<Vec<_>>();
            roots.sort_unstable();
            roots
        };
        let mut evidence = Vec::with_capacity(roots.len());
        for root in roots {
            if let Some(snapshot) = self.evidence(&root).await {
                evidence.push(snapshot);
            }
        }
        evidence
    }

    async fn finalize_root(&self, root_session_id: &str) -> Result<(), RlmRuntimeError> {
        let (ids, agents, controls) = loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            let owned = {
                let mut registry = self.registry.lock().await;
                let scope = registry
                    .scopes
                    .entry(root_session_id.to_owned().into_boxed_str())
                    .or_default();
                if scope.finalization_complete {
                    return Ok(());
                }
                if scope.finalized {
                    None
                } else {
                    scope.finalized = true;
                    let ids = scope.creation_order.clone();
                    let mut agents = Vec::with_capacity(ids.len());
                    let mut controls = Vec::new();
                    for id in &ids {
                        if let Some(child) = scope.children.get_mut(id) {
                            if child.status == RlmStatus::Running {
                                child.status = RlmStatus::Interrupted;
                            }
                            if let Some(control) = child.active_control.take() {
                                controls.push(control);
                            }
                            agents.push(child.agent.clone());
                        }
                    }
                    Some((ids, agents, controls))
                }
            };
            if let Some(owned) = owned {
                break owned;
            }
            changed.await;
        };
        for control in controls {
            drop(control.cancel().await);
        }
        let mut shutdowns = tokio::task::JoinSet::new();
        for agent in agents {
            shutdowns
                .spawn(async move { agent.shutdown().await.map_err(|error| error.to_string()) });
        }
        let mut failures = Vec::new();
        while let Some(result) = shutdowns.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(error),
                Err(error) => failures.push(error.to_string()),
            }
        }
        let tasks = {
            let mut registry = self.registry.lock().await;
            let Some(scope) = registry.scopes.get_mut(root_session_id) else {
                return Ok(());
            };
            ids.iter()
                .flat_map(|id| {
                    let Some(child) = scope.children.get_mut(id) else {
                        return Vec::new();
                    };
                    [child.active_monitor.take(), child.event_forwarder.take()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        for task in tasks {
            if let Err(error) = task.await {
                failures.push(error.to_string());
            }
        }
        {
            let mut registry = self.registry.lock().await;
            if let Some(scope) = registry.scopes.get_mut(root_session_id) {
                for id in &ids {
                    if let Some(child) = scope.children.get_mut(id) {
                        child.status = RlmStatus::Closed;
                        child.active_control = None;
                    }
                }
                scope.finalization_complete = true;
            }
        }
        for id in &ids {
            self.emit(root_session_id, id, RlmEventKind::Status(RlmStatus::Closed))
                .await;
        }
        self.changed.notify_waiters();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(RlmRuntimeError::Agent(failures.join("; ")))
        }
    }
}

impl Registry {
    fn root_for(&self, session_id: &str) -> Box<str> {
        self.root_by_session
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| session_id.to_owned().into_boxed_str())
    }

    fn scope_for(&self, session_id: &str) -> Option<&Scope> {
        let root = self
            .root_by_session
            .get(session_id)
            .map_or(session_id, AsRef::as_ref);
        self.scopes.get(root)
    }
}

impl Scope {
    fn agent_for_session(&self, session_id: &str) -> Option<RlmAgentId> {
        self.children.iter().find_map(|(id, child)| {
            (child.agent.session_id().to_string() == session_id).then(|| id.clone())
        })
    }

    fn is_descendant(&self, ancestor: &RlmAgentId, candidate: &RlmAgentId) -> bool {
        let mut current = self
            .children
            .get(candidate)
            .and_then(|child| child.parent.as_ref());
        while let Some(id) = current {
            if id == ancestor {
                return true;
            }
            current = self
                .children
                .get(id)
                .and_then(|child| child.parent.as_ref());
        }
        false
    }

    fn subtree(&self, root: &RlmAgentId) -> Vec<RlmAgentId> {
        self.creation_order
            .iter()
            .filter(|candidate| *candidate == root || self.is_descendant(root, candidate))
            .cloned()
            .collect()
    }
}

impl Child {
    fn summary(&self, id: RlmAgentId) -> RlmAgentSummary {
        RlmAgentSummary {
            agent_id: id,
            parent_agent_id: self.parent.clone(),
            specification: self.specification.clone(),
            task: self.task.clone(),
            status: self.status,
            last_message: self.last_message.clone(),
            error: self.error.clone(),
        }
    }
}

fn child_instructions(
    launch: &LaunchSnapshot,
    spec: &SubagentSpec,
    reservation: &Reservation,
    task: &str,
) -> String {
    format!(
        "{}\n\nSubagent specification: {} (`{}`)\n\n{}\n\nRecursive identity: {}\nParent: {}\nAssigned task:\n{}",
        launch.prompts().subagent().trim(),
        spec.name(),
        spec.id(),
        spec.instructions().trim(),
        reservation.id,
        reservation
            .parent
            .as_ref()
            .map_or("root", RlmAgentId::as_str),
        task.trim(),
    )
}

fn mailbox_key(caller: Option<&RlmAgentId>, root_session_id: &str) -> Box<str> {
    caller.map_or_else(
        || root_session_id.to_owned().into_boxed_str(),
        |id| id.as_str().to_owned().into_boxed_str(),
    )
}

fn validate_text(value: &str, label: &str, max_bytes: usize) -> Result<(), RlmRuntimeError> {
    if value.trim().is_empty() {
        return Err(RlmRuntimeError::InvalidInput(format!(
            "{label} must not be empty"
        )));
    }
    if value.len() > max_bytes {
        return Err(RlmRuntimeError::InvalidInput(format!(
            "{label} exceeds the {max_bytes}-byte limit"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{HarnessSnapshot, PromptPack};

    #[test]
    fn policy_rejects_zero_bounds() {
        assert_eq!(RlmPolicy::new(0, 1), Err(RlmPolicyError::ZeroActiveTurns));
        assert_eq!(RlmPolicy::new(1, 0), Err(RlmPolicyError::ZeroDepth));
        assert!(RlmPolicy::default().harness_refinement_enabled());
        assert!(
            !RlmPolicy::default()
                .with_harness_refinement(false)
                .harness_refinement_enabled()
        );
    }

    #[test]
    fn message_validation_counts_utf8_bytes() {
        assert!(validate_text("hello", "message", 5).is_ok());
        assert!(validate_text("ééé", "message", 5).is_err());
    }

    #[tokio::test]
    async fn finalizing_an_empty_root_freezes_attributable_evidence() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let launch = LaunchSnapshot::new(
            PromptPack::load(root.join("prompts")).unwrap(),
            HarnessSnapshot::load(root.join("nanocodex.harness.toml")).unwrap(),
        );
        let expected_digest = launch.digest().to_owned();
        let runtime = RlmRuntime::new(launch);

        runtime.finalize_root("root-session").await.unwrap();
        runtime.finalize_root("root-session").await.unwrap();

        let evidence = runtime.evidence("root-session").await.unwrap();
        assert_eq!(evidence.root_session_id.as_ref(), "root-session");
        assert_eq!(evidence.launch_digest.as_ref(), expected_digest);
        assert!(evidence.agents.is_empty());
        assert!(matches!(
            runtime.state.reserve("root-session").await,
            Err(RlmRuntimeError::Closed)
        ));
    }
}
