// Modified from clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa for Nanocodex2.
// SPDX-License-Identifier: Apache-2.0

//! Independently scheduled ManagedAgent turn worker.

use crate::{
    app::config::ReasoningEffort,
    client::{PromptContent, PromptInput},
    core::{IMAGE_RENDERING_INSTRUCTIONS, MEMORY_REVIEW_CHECKPOINT},
    engine::{EngineError, ManagedAgent, ManagedAgentEvents, ManagedTurnControl},
    tui::{
        components::QueueId, pane::PaneId, prompt::Submission, session::ManagedSessionSnapshot,
        transcript::TurnId,
    },
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};
use tokio::{
    sync::{mpsc, oneshot},
    task::{JoinError, JoinSet},
};
use tokio_util::sync::CancellationToken;

pub(crate) enum WorkerCommand {
    Submit {
        pane: PaneId,
        id: TurnId,
        prompt: Submission,
    },
    Reflect {
        pane: PaneId,
        id: TurnId,
        instructions: Submission,
        context: ReflectionContext,
    },
    Auxiliary {
        pane: PaneId,
        id: TurnId,
        prompt: Submission,
        context: AuxiliaryContext,
        shutdown: CancellationToken,
        completion: oneshot::Sender<Result<String, AuxiliaryError>>,
    },
    Steer {
        pane: PaneId,
        queue_id: QueueId,
        fallback_id: TurnId,
        prompt: Submission,
    },
    ReplaceAgent {
        pane: PaneId,
        agent: ManagedAgent,
        memory_review: MemoryReviewState,
    },
    SetThinking {
        pane: PaneId,
        effort: ReasoningEffort,
    },
    SetFastMode {
        pane: PaneId,
        enabled: bool,
    },
    CancelAll(PaneId),
    OpenFork {
        pane: PaneId,
        parent_sequence: u64,
    },
    ClosePane(PaneId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuxiliaryContext {
    Clean,
    CurrentConversation,
}

pub(crate) struct ReflectionContext {
    config_path: PathBuf,
    workspace: PathBuf,
}

impl ReflectionContext {
    pub(crate) fn new(config_path: &Path, workspace: &Path) -> Self {
        Self {
            config_path: config_path.to_path_buf(),
            workspace: workspace.to_path_buf(),
        }
    }

    fn prompt(&self) -> String {
        let context = serde_json::json!({
            "config_path": self.config_path.to_string_lossy(),
            "workspace": self.workspace.to_string_lossy(),
        });
        format!("<reflection_context>\n{context}\n</reflection_context>")
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum AuxiliaryError {
    Cancelled,
    Failed(String),
}

pub(crate) enum WorkerEvent {
    TurnAccepted {
        pane: PaneId,
        id: TurnId,
    },
    TurnFinished {
        pane: PaneId,
        id: TurnId,
        error: Option<String>,
        snapshot: Option<Box<ManagedSessionSnapshot>>,
        terminal_expected: bool,
    },
    SteerAdmitted {
        pane: PaneId,
        queue_id: QueueId,
    },
    SteerPromoted {
        pane: PaneId,
        queue_id: QueueId,
        id: TurnId,
        prompt: Submission,
    },
    SteerFailed {
        pane: PaneId,
        queue_id: QueueId,
        error: String,
    },
    TurnsCancelled {
        pane: PaneId,
        count: usize,
        error: Option<String>,
    },
    ForkOpened {
        pane: PaneId,
        parent: PaneId,
        parent_sequence: u64,
        events: ManagedAgentEvents,
    },
    ForkFailed {
        pane: PaneId,
        error: String,
    },
    ThinkingUpdated {
        pane: PaneId,
        effort: ReasoningEffort,
        result: Result<(), EngineError>,
    },
    FastModeUpdated {
        pane: PaneId,
        enabled: bool,
        result: Result<(), EngineError>,
    },
    Stopped {
        error: Option<EngineError>,
    },
}

type TurnResult = Result<CompletedTurn, EngineError>;

struct CompletedTurn {
    final_message: String,
    snapshot: Option<Box<ManagedSessionSnapshot>>,
}

enum TurnPurpose {
    Conversation,
    Auxiliary(oneshot::Sender<Result<String, AuxiliaryError>>),
}

enum PromptKind {
    Conversation,
    Reflection(ReflectionContext),
    Auxiliary,
}

impl PromptKind {
    fn prepare(&self, prompt: &Submission, memory_review: MemoryReviewState) -> PromptInput {
        match self {
            Self::Conversation => memory_review.submission_prompt(prompt),
            Self::Reflection(context) => reflection_prompt(prompt, context),
            Self::Auxiliary => prompt.managed_prompt(),
        }
    }
}

struct TurnRequest {
    pane: PaneId,
    id: TurnId,
    prompt: Submission,
    purpose: TurnPurpose,
    auxiliary_context: Option<AuxiliaryContext>,
    shutdown: Option<CancellationToken>,
    prompt_kind: PromptKind,
}

const REFLECTION_PROMPT: &str = concat!(
    "This is a self-contained Tact reflection turn. Reflect on the conversation available in this ",
    "session and produce a report for the user. Start with the current conversation. Use the ",
    "additional instructions to narrow the topic or identify other workspaces, sessions, or task ",
    "families; otherwise sample relevant recent history from the current workspace.\n\n",
    "Discover historical candidates in bounded stages with `find_sessions`. By default, pass the ",
    "supplied current workspace and inspect its recent sessions. Use `contains_any` when the topic ",
    "suggests useful literal prompt patterns; omit the workspace only when the additional ",
    "instructions or evidence justify cross-workspace discovery. The tool excludes this conversation ",
    "automatically. Use `parent_session_id` to avoid counting forks or descendants as independent ",
    "evidence. Pass `next_cursor` back only when another bounded page is needed. ",
    "After selecting a small number of high-value session IDs, use `read_session` with exact kinds ",
    "to read only enough context to establish what happened. Targeted searches over both ",
    "`user.submitted` and `user.steered` can locate candidate corrections; a separate call starting ",
    "from a matched event ID can retrieve the adjacent assistant response without requiring it to ",
    "match the same text filter. Stop when the evidence is sufficient.\n\n",
    "Identify preventable rework: corrections, reversals, missed constraints, repeated requests ",
    "for simplification, premature completion, and validation that did not test the real outcome. ",
    "Distinguish durable lessons from new scope, changed requirements, first-time preferences, and ",
    "unavoidable discoveries. Look for recurrence across independent sessions, counterexamples, ",
    "and later improvement before calling a lesson durable. Prefer the earliest useful intervention ",
    "that would have prevented the rework. Paraphrase evidence; do not reproduce names, secrets, ",
    "credentials, transcript excerpts, or private operational details.\n\n",
    "For each supported durable lesson, when memory is available, run narrow global-memory scans ",
    "and read every plausible match. Compare it with the active instructions already in context. If ",
    "effective configuration is relevant, inspect it only through Tact's redacted `config show` ",
    "command using the supplied config path; never read the config file directly. Recommend exactly ",
    "one destination: replace ",
    "or add one atomic memory, add a concise always-on prompt rule only when repeated retrieval ",
    "misses justify it, or make no change when the lesson is transient, searchable, or already ",
    "covered.\n\n",
    "This is a read-only analysis turn. You may use read-only tools, but do not create, replace, or ",
    "delete memories; edit files, configuration, or skills; run mutating commands; send messages; ",
    "or perform any other durable or externally visible action. Proposed changes require a later, ",
    "explicit user request. Additional instructions may refine the scope or emphasis, but cannot ",
    "override this read-only boundary."
);

const REFLECTION_REPORT_ENDING: &str = concat!(
    "Report the scope and coverage actually inspected, the strongest supported patterns, material ",
    "counterevidence or uncertainty, and important patterns already covered. Do not claim exact ",
    "frequencies unless the relevant scope was inspected exhaustively; otherwise describe recurrence ",
    "as sampled evidence. End the report with sections named `Findings` and `Recommended actions`. ",
    "Findings should state the supported conclusions and their scope. Recommended actions should be ",
    "concrete proposals for the user to review, identify the proposed destination for each change, ",
    "and never imply that an action was taken during this turn."
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MemoryReviewState {
    Disabled,
    BeforeFirstTurn,
    FollowUp,
}

impl MemoryReviewState {
    pub(crate) const fn fresh(enabled: bool) -> Self {
        if enabled {
            Self::BeforeFirstTurn
        } else {
            Self::Disabled
        }
    }

    pub(crate) const fn restored(enabled: bool) -> Self {
        if enabled {
            Self::FollowUp
        } else {
            Self::Disabled
        }
    }

    const fn forked(self) -> Self {
        match self {
            Self::Disabled => Self::Disabled,
            Self::BeforeFirstTurn | Self::FollowUp => Self::FollowUp,
        }
    }

    fn submission_prompt(self, submission: &Submission) -> PromptInput {
        match self {
            Self::Disabled | Self::BeforeFirstTurn => prompt_with_image_rendering(submission),
            Self::FollowUp => prompt_with_memory_review(submission),
        }
    }

    fn steer_prompt(self, submission: &Submission) -> PromptInput {
        match self {
            Self::Disabled => prompt_with_image_rendering(submission),
            Self::BeforeFirstTurn | Self::FollowUp => prompt_with_memory_review(submission),
        }
    }

    fn turn_accepted(&mut self) {
        if *self == Self::BeforeFirstTurn {
            *self = Self::FollowUp;
        }
    }
}

fn prompt_with_memory_review(submission: &Submission) -> PromptInput {
    let mut prompt = prompt_with_image_rendering(submission);
    append_prompt_instructions(&mut prompt, MEMORY_REVIEW_CHECKPOINT);
    prompt
}

fn prompt_with_image_rendering(submission: &Submission) -> PromptInput {
    let mut prompt = submission.managed_prompt();
    append_prompt_instructions(&mut prompt, IMAGE_RENDERING_INSTRUCTIONS);
    prompt
}

fn append_prompt_instructions(prompt: &mut PromptInput, instructions: &str) {
    match prompt {
        PromptInput::Text(text) => {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(instructions);
        }
        PromptInput::Content(content) => content.push(PromptContent::Text {
            text: format!("\n\n{instructions}"),
        }),
    }
}

fn reflection_prompt(instructions: &Submission, context: &ReflectionContext) -> PromptInput {
    let mut prompt = instructions.managed_prompt();
    let context = context.prompt();
    match &mut prompt {
        PromptInput::Text(text) => {
            let instructions = std::mem::take(text);
            *text = format!(
                "{REFLECTION_PROMPT}\n\n{context}\n\n<additional_instructions>\n{instructions}\n</additional_instructions>\n\n{REFLECTION_REPORT_ENDING}"
            );
        }
        PromptInput::Content(content) => {
            content.insert(
                0,
                PromptContent::Text {
                    text: format!(
                        "{REFLECTION_PROMPT}\n\n{context}\n\n<additional_instructions>\n"
                    ),
                },
            );
            content.push(PromptContent::Text {
                text: format!("\n</additional_instructions>\n\n{REFLECTION_REPORT_ENDING}"),
            });
        }
    }
    prompt
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TurnKey {
    pane: PaneId,
    id: TurnId,
}

struct SteerRequest {
    pane: PaneId,
    queue_id: QueueId,
    fallback_id: TurnId,
    prompt: Submission,
}

pub(crate) fn spawn(
    agent: ManagedAgent,
    memory_review: MemoryReviewState,
    shutdown: CancellationToken,
) -> (
    mpsc::UnboundedSender<WorkerCommand>,
    mpsc::UnboundedReceiver<WorkerEvent>,
) {
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (updates, update_rx) = mpsc::unbounded_channel();
    tokio::spawn(run(agent, memory_review, command_rx, updates, shutdown));
    (commands, update_rx)
}

async fn run(
    agent: ManagedAgent,
    memory_review: MemoryReviewState,
    mut commands: mpsc::UnboundedReceiver<WorkerCommand>,
    updates: mpsc::UnboundedSender<WorkerEvent>,
    shutdown: CancellationToken,
) {
    let mut main = Some((PaneId::Main, agent));
    let mut fork = None::<(PaneId, ManagedAgent)>;
    let mut controls = HashMap::<TurnKey, ManagedTurnControl>::new();
    let mut memory_reviews = HashMap::from([(PaneId::Main, memory_review)]);
    let mut cancelled = HashSet::<TurnKey>::new();
    let mut turns = JoinSet::<(TurnKey, TurnPurpose, bool, TurnResult)>::new();

    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            result = turns.join_next(), if !turns.is_empty() => {
                finish_turn(result, false, &mut controls, &mut cancelled, &updates);
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    break;
                };
                let request = match command {
                    WorkerCommand::Submit { pane, id, prompt } => TurnRequest {
                        pane,
                        id,
                        prompt,
                        purpose: TurnPurpose::Conversation,
                        auxiliary_context: None,
                        shutdown: None,
                        prompt_kind: PromptKind::Conversation,
                    },
                    WorkerCommand::Reflect {
                        pane,
                        id,
                        instructions,
                        context,
                    } => TurnRequest {
                        pane,
                        id,
                        prompt: instructions,
                        purpose: TurnPurpose::Conversation,
                        auxiliary_context: None,
                        shutdown: None,
                        prompt_kind: PromptKind::Reflection(context),
                    },
                    WorkerCommand::Auxiliary {
                        pane,
                        id,
                        prompt,
                        context,
                        shutdown,
                        completion,
                    } => TurnRequest {
                        pane,
                        id,
                        prompt,
                        purpose: TurnPurpose::Auxiliary(completion),
                        auxiliary_context: Some(context),
                        shutdown: Some(shutdown),
                        prompt_kind: PromptKind::Auxiliary,
                    },
                    WorkerCommand::Steer {
                        pane,
                        queue_id,
                        fallback_id,
                        prompt,
                    } => {
                        let Some(agent) = agent_for(pane, main.as_ref(), fork.as_ref()) else {
                            drop(updates.send(WorkerEvent::SteerFailed {
                                pane,
                                queue_id,
                                error: "session pane is no longer available".to_owned(),
                            }));
                            continue;
                        };
                        let request = SteerRequest {
                            pane,
                            queue_id,
                            fallback_id,
                            prompt,
                        };
                        let memory_review = *memory_reviews
                            .get(&pane)
                            .expect("an available pane must have memory-review state");
                        let started_turn = steer_turn(
                            agent,
                            memory_review,
                            &mut controls,
                            &mut turns,
                            &updates,
                            request,
                        )
                        .await;
                        if started_turn {
                            memory_reviews
                                .get_mut(&pane)
                                .expect("an available pane must have memory-review state")
                                .turn_accepted();
                        }
                        continue;
                    }
                    WorkerCommand::ReplaceAgent {
                        pane,
                        agent,
                        memory_review,
                    } => {
                        debug_assert!(!controls.keys().any(|key| key.pane == pane));
                        let retired = if main.as_ref().is_some_and(|(id, _)| *id == pane) {
                            memory_reviews.insert(pane, memory_review);
                            main.replace((pane, agent)).map(|(_, agent)| agent)
                        } else if fork.as_ref().is_some_and(|(id, _)| *id == pane) {
                            memory_reviews.insert(pane, memory_review);
                            fork.replace((pane, agent)).map(|(_, agent)| agent)
                        } else {
                            drop(updates.send(WorkerEvent::ForkFailed {
                                pane,
                                error: "session pane is no longer available".to_owned(),
                            }));
                            Some(agent)
                        };
                        if let Some(retired) = retired {
                            drop(retired.shutdown().await);
                        }
                        continue;
                    }
                    WorkerCommand::SetThinking { pane, effort } => {
                        let result = match agent_for(pane, main.as_ref(), fork.as_ref()) {
                            Some(agent) => agent.set_thinking(effort.into()).await,
                            None => Err(EngineError::Shutdown),
                        };
                        drop(updates.send(WorkerEvent::ThinkingUpdated {
                            pane,
                            effort,
                            result,
                        }));
                        continue;
                    }
                    WorkerCommand::SetFastMode { pane, enabled } => {
                        let result = match agent_for(pane, main.as_ref(), fork.as_ref()) {
                            Some(agent) => agent.set_fast_mode(enabled).await,
                            None => Err(EngineError::Shutdown),
                        };
                        drop(updates.send(WorkerEvent::FastModeUpdated {
                            pane,
                            enabled,
                            result,
                        }));
                        continue;
                    }
                    WorkerCommand::CancelAll(pane) => {
                        cancel_pane(pane, &controls, &mut cancelled, &updates).await;
                        continue;
                    }
                    WorkerCommand::OpenFork {
                        pane,
                        parent_sequence,
                    } => {
                        if fork.is_some() {
                            drop(updates.send(WorkerEvent::ForkFailed {
                                pane,
                                error: "a forked session is already open".to_owned(),
                            }));
                            continue;
                        }
                        let Some((main_pane, agent)) = main.as_ref() else {
                            drop(updates.send(WorkerEvent::ForkFailed {
                                pane,
                                error: "the primary session is no longer available".to_owned(),
                            }));
                            continue;
                        };
                        match agent.fork().await {
                            Ok((agent, events)) => {
                                let memory_review = *memory_reviews
                                    .get(main_pane)
                                    .expect("the primary pane must have memory-review state");
                                let memory_review = memory_review.forked();
                                memory_reviews.insert(pane, memory_review);
                                fork = Some((pane, agent));
                                drop(updates.send(WorkerEvent::ForkOpened {
                                    pane,
                                    parent: *main_pane,
                                    parent_sequence,
                                    events,
                                }));
                            }
                            Err(error) => drop(updates.send(WorkerEvent::ForkFailed {
                                pane,
                                error: error.to_string(),
                            })),
                        }
                        continue;
                    }
                    WorkerCommand::ClosePane(pane) => {
                        let agent = if main.as_ref().is_some_and(|(id, _)| *id == pane) {
                            let agent = main.take().map(|(_, agent)| agent);
                            main = fork.take();
                            agent
                        } else if fork.as_ref().is_some_and(|(id, _)| *id == pane) {
                            fork.take().map(|(_, agent)| agent)
                        } else {
                            None
                        };
                        memory_reviews.remove(&pane);
                        close_pane(pane, agent, &controls, &mut cancelled, &updates).await;
                        continue;
                    }
                };
                let Some(agent) = agent_for(request.pane, main.as_ref(), fork.as_ref()) else {
                    reject_turn(request, "session pane is no longer available".to_owned(), &updates);
                    continue;
                };
                let pane = request.pane;
                let memory_review = *memory_reviews
                    .get(&pane)
                    .expect("an available pane must have memory-review state");
                let started_conversation = start_turn(
                    agent,
                    request,
                    memory_review,
                    &mut controls,
                    &mut turns,
                    &updates,
                )
                .await;
                if started_conversation {
                    memory_reviews
                        .get_mut(&pane)
                        .expect("an available pane must have memory-review state")
                        .turn_accepted();
                }
            }
        }
    }

    commands.close();
    while commands.try_recv().is_ok() {}

    drop(cancel_turns(&controls, None).await);
    let (main_shutdown, fork_shutdown) = tokio::join!(
        shutdown_agent(main.take().map(|(_, agent)| agent)),
        shutdown_agent(fork.take().map(|(_, agent)| agent)),
    );
    let shutdown_error = main_shutdown.err().or_else(|| fork_shutdown.err());

    while let Some(result) = turns.join_next().await {
        finish_turn(Some(result), true, &mut controls, &mut cancelled, &updates);
    }

    drop(updates.send(WorkerEvent::Stopped {
        error: shutdown_error,
    }));
}

async fn start_turn(
    agent: &ManagedAgent,
    request: TurnRequest,
    memory_review: MemoryReviewState,
    controls: &mut HashMap<TurnKey, ManagedTurnControl>,
    turns: &mut JoinSet<(TurnKey, TurnPurpose, bool, TurnResult)>,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
) -> bool {
    if request
        .shutdown
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        reject_cancelled_turn(request);
        return false;
    }
    let TurnRequest {
        pane,
        id,
        prompt,
        purpose,
        auxiliary_context,
        shutdown,
        prompt_kind,
    } = request;
    let auxiliary = auxiliary_context.is_some();
    let (isolated_agent, event_drain) = if let Some(context) = auxiliary_context {
        let create_agent = async {
            match context {
                AuxiliaryContext::Clean => agent.spawn().await,
                AuxiliaryContext::CurrentConversation => agent.fork().await,
            }
        };
        let spawned = if let Some(scope) = shutdown.clone() {
            tokio::select! {
                result = create_agent => result,
                () = scope.cancelled() => {
                    reject_cancelled_turn(TurnRequest {
                        pane,
                        id,
                        prompt,
                        purpose,
                        auxiliary_context: Some(context),
                        shutdown,
                        prompt_kind,
                    });
                    return false;
                }
            }
        } else {
            create_agent.await
        };
        let (agent, mut events) = match spawned {
            Ok(spawned) => spawned,
            Err(error) => {
                reject_turn(
                    TurnRequest {
                        pane,
                        id,
                        prompt,
                        purpose,
                        auxiliary_context: Some(context),
                        shutdown,
                        prompt_kind,
                    },
                    error.to_string(),
                    updates,
                );
                return false;
            }
        };
        let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
        (Some(agent), Some(drain))
    } else {
        (None, None)
    };
    let turn_agent = isolated_agent.as_ref().unwrap_or(agent);
    let snapshot = ManagedSessionSnapshot::new(turn_agent.identity().agent_id());
    let agent_prompt = prompt_kind.prepare(&prompt, memory_review);
    let turn = match turn_agent.prompt(agent_prompt).await {
        Ok(turn) => turn,
        Err(error) => {
            if let Some(agent) = isolated_agent {
                drop(agent.shutdown().await);
            }
            if let Some(drain) = event_drain {
                drop(drain.await);
            }
            reject_turn(
                TurnRequest {
                    pane,
                    id,
                    prompt,
                    purpose,
                    auxiliary_context,
                    shutdown,
                    prompt_kind,
                },
                error.to_string(),
                updates,
            );
            return false;
        }
    };
    let key = TurnKey { pane, id };
    let control = turn.control();
    let task_control = control.clone();
    controls.insert(key, control);
    turns.spawn(async move {
        let mut turn = Box::pin(turn);
        let (cancelled_by_scope, result) = match shutdown {
            Some(shutdown) => {
                tokio::select! {
                    result = turn.as_mut() => (false, result),
                    () = shutdown.cancelled() => {
                        drop(task_control.cancel().await);
                        (true, turn.await)
                    }
                }
            }
            None => (false, turn.await),
        };
        let result = result.map(|result| CompletedTurn {
            final_message: result.final_message().to_owned(),
            snapshot: (!auxiliary).then(|| Box::new(snapshot)),
        });
        if let Some(agent) = isolated_agent {
            drop(agent.shutdown().await);
        }
        if let Some(drain) = event_drain {
            drop(drain.await);
        }
        (key, purpose, cancelled_by_scope, result)
    });
    if !auxiliary {
        drop(updates.send(WorkerEvent::TurnAccepted { pane, id }));
    }
    !auxiliary
}

fn reject_turn(request: TurnRequest, error: String, updates: &mpsc::UnboundedSender<WorkerEvent>) {
    match request.purpose {
        TurnPurpose::Conversation => drop(updates.send(WorkerEvent::TurnFinished {
            pane: request.pane,
            id: request.id,
            error: Some(error),
            snapshot: None,
            terminal_expected: false,
        })),
        TurnPurpose::Auxiliary(completion) => {
            drop(completion.send(Err(AuxiliaryError::Failed(error))));
        }
    }
}

fn reject_cancelled_turn(request: TurnRequest) {
    if let TurnPurpose::Auxiliary(completion) = request.purpose {
        drop(completion.send(Err(AuxiliaryError::Cancelled)));
    }
}

async fn steer_turn(
    agent: &ManagedAgent,
    memory_review: MemoryReviewState,
    controls: &mut HashMap<TurnKey, ManagedTurnControl>,
    turns: &mut JoinSet<(TurnKey, TurnPurpose, bool, TurnResult)>,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
    request: SteerRequest,
) -> bool {
    let SteerRequest {
        pane,
        queue_id,
        fallback_id,
        prompt,
    } = request;
    let mut active = controls
        .iter()
        .filter(|(key, _)| key.pane == pane)
        .collect::<Vec<_>>();
    active.sort_unstable_by_key(|(key, _)| key.id);
    for (_, control) in active {
        match control.steer(memory_review.steer_prompt(&prompt)).await {
            Ok(()) => {
                drop(updates.send(WorkerEvent::SteerAdmitted { pane, queue_id }));
                return false;
            }
            Err(error) => {
                drop(updates.send(WorkerEvent::SteerFailed {
                    pane,
                    queue_id,
                    error: error.to_string(),
                }));
                return false;
            }
        }
    }

    match agent.prompt(memory_review.steer_prompt(&prompt)).await {
        Ok(turn) => {
            let snapshot = ManagedSessionSnapshot::new(agent.identity().agent_id());
            let control = turn.control();
            let key = TurnKey {
                pane,
                id: fallback_id,
            };
            turns.spawn(async move {
                let result = turn.await.map(|result| CompletedTurn {
                    final_message: result.final_message().to_owned(),
                    snapshot: Some(Box::new(snapshot)),
                });
                (key, TurnPurpose::Conversation, false, result)
            });
            controls.insert(key, control);
            drop(updates.send(WorkerEvent::TurnAccepted {
                pane,
                id: fallback_id,
            }));
            drop(updates.send(WorkerEvent::SteerPromoted {
                pane,
                queue_id,
                id: fallback_id,
                prompt,
            }));
            true
        }
        Err(error) => {
            drop(updates.send(WorkerEvent::SteerFailed {
                pane,
                queue_id,
                error: error.to_string(),
            }));
            false
        }
    }
}

async fn cancel_turns(
    controls: &HashMap<TurnKey, ManagedTurnControl>,
    pane: Option<PaneId>,
) -> (Vec<TurnKey>, Option<String>) {
    let pending = controls
        .iter()
        .filter(|(key, _)| pane.is_none_or(|pane| key.pane == pane))
        .map(|(&key, control)| (key, control.clone()))
        .collect::<Vec<_>>();
    let mut cancelled = Vec::with_capacity(pending.len());
    let mut first_error = None;
    for (key, control) in pending {
        match control.cancel().await {
            Ok(()) => cancelled.push(key),
            Err(error) if first_error.is_none() => first_error = Some(error.to_string()),
            Err(_) => {}
        }
    }
    (cancelled, first_error)
}

fn finish_turn(
    result: Option<Result<(TurnKey, TurnPurpose, bool, TurnResult), JoinError>>,
    shutting_down: bool,
    controls: &mut HashMap<TurnKey, ManagedTurnControl>,
    cancelled: &mut HashSet<TurnKey>,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
) {
    let Some(result) = result else {
        return;
    };
    let (key, purpose, cancelled_by_scope, result) = match result {
        Ok(result) => result,
        Err(error) => {
            drop(updates.send(WorkerEvent::TurnFinished {
                pane: PaneId::Main,
                id: TurnId::new(0),
                error: Some(format!("turn task stopped unexpectedly: {error}")),
                snapshot: None,
                terminal_expected: false,
            }));
            return;
        }
    };
    controls.remove(&key);
    let was_cancelled = cancelled.remove(&key);
    match purpose {
        TurnPurpose::Conversation => {
            let (error, snapshot) = match result {
                Ok(completed) => (None, completed.snapshot),
                Err(error)
                    if (shutting_down || was_cancelled || cancelled_by_scope)
                        && is_cancelled(&error) =>
                {
                    (None, None)
                }
                Err(error) => (Some(error.to_string()), None),
            };
            drop(updates.send(WorkerEvent::TurnFinished {
                pane: key.pane,
                id: key.id,
                error,
                snapshot,
                terminal_expected: true,
            }));
        }
        TurnPurpose::Auxiliary(completion) => {
            let result = match result {
                Ok(completed) => Ok(completed.final_message),
                Err(error)
                    if (shutting_down || was_cancelled || cancelled_by_scope)
                        && is_cancelled(&error) =>
                {
                    Err(AuxiliaryError::Cancelled)
                }
                Err(error) => Err(AuxiliaryError::Failed(error.to_string())),
            };
            drop(completion.send(result));
        }
    }
}

fn is_cancelled(error: &EngineError) -> bool {
    matches!(
        error,
        EngineError::Turn {
            state: "cancelled",
            ..
        }
    )
}

fn agent_for<'a>(
    pane: PaneId,
    main: Option<&'a (PaneId, ManagedAgent)>,
    fork: Option<&'a (PaneId, ManagedAgent)>,
) -> Option<&'a ManagedAgent> {
    main.filter(|(main_pane, _)| *main_pane == pane)
        .or_else(|| fork.filter(|(fork_pane, _)| *fork_pane == pane))
        .map(|(_, agent)| agent)
}

async fn cancel_pane(
    pane: PaneId,
    controls: &HashMap<TurnKey, ManagedTurnControl>,
    cancelled: &mut HashSet<TurnKey>,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
) {
    let (keys, error) = cancel_turns(controls, Some(pane)).await;
    let count = keys.len();
    cancelled.extend(keys);
    drop(updates.send(WorkerEvent::TurnsCancelled { pane, count, error }));
}

async fn close_pane(
    pane: PaneId,
    agent: Option<ManagedAgent>,
    controls: &HashMap<TurnKey, ManagedTurnControl>,
    cancelled: &mut HashSet<TurnKey>,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
) {
    let (keys, mut error) = cancel_turns(controls, Some(pane)).await;
    let count = keys.len();
    cancelled.extend(keys);
    if let Err(shutdown_error) = shutdown_agent(agent).await
        && error.is_none()
    {
        error = Some(shutdown_error.to_string());
    }
    drop(updates.send(WorkerEvent::TurnsCancelled { pane, count, error }));
}

async fn shutdown_agent(agent: Option<ManagedAgent>) -> Result<(), EngineError> {
    let Some(agent) = agent else {
        return Ok(());
    };
    agent.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::{MemoryReviewState, ReflectionContext, reflection_prompt};
    use crate::{
        client::{PromptContent, PromptInput},
        core::{IMAGE_RENDERING_INSTRUCTIONS, MEMORY_REVIEW_CHECKPOINT},
        tui::prompt::Submission,
    };
    use std::path::Path;

    fn prompt_text(prompt: PromptInput) -> String {
        match prompt {
            PromptInput::Text(text) => text,
            PromptInput::Content(content) => content
                .into_iter()
                .filter_map(|item| match item {
                    PromptContent::Text { text } => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    #[test]
    fn review_state_decorates_followups_and_steers_without_changing_display_text() {
        let initial = Submission::text("initial request".to_owned());
        let follow_up = Submission::text("actually, preserve ordering".to_owned());
        let steer = Submission::text("change direction".to_owned());
        let mut review = MemoryReviewState::fresh(true);

        let initial_prompt = prompt_text(review.submission_prompt(&initial));
        assert!(!initial_prompt.contains(MEMORY_REVIEW_CHECKPOINT));
        assert!(initial_prompt.contains(IMAGE_RENDERING_INSTRUCTIONS));
        review.turn_accepted();
        let follow_up_prompt = prompt_text(review.submission_prompt(&follow_up));
        assert_eq!(
            follow_up_prompt.matches(MEMORY_REVIEW_CHECKPOINT).count(),
            1
        );
        assert!(follow_up_prompt.contains(IMAGE_RENDERING_INSTRUCTIONS));
        let steer_prompt = prompt_text(MemoryReviewState::fresh(true).steer_prompt(&steer));
        assert_eq!(steer_prompt.matches(MEMORY_REVIEW_CHECKPOINT).count(), 1);
        assert!(steer_prompt.contains(IMAGE_RENDERING_INSTRUCTIONS));
        assert_eq!(follow_up.display_text(), "actually, preserve ordering");
        assert_eq!(steer.display_text(), "change direction");

        let disabled = MemoryReviewState::fresh(false);
        let disabled_prompt = prompt_text(disabled.steer_prompt(&steer));
        assert!(!disabled_prompt.contains(MEMORY_REVIEW_CHECKPOINT));
        assert!(disabled_prompt.contains(IMAGE_RENDERING_INSTRUCTIONS));
    }

    #[test]
    fn reflection_prompt_is_read_only_and_ends_with_reviewable_actions() {
        let context =
            ReflectionContext::new(Path::new("/tact/config.toml"), Path::new("/work/current"));
        let prompt = reflection_prompt(
            &Submission::text("Focus on validation gaps.".to_owned()),
            &context,
        );
        let text = prompt_text(prompt);

        assert!(text.contains("Focus on validation gaps."));
        assert!(text.contains("self-contained Tact reflection turn"));
        assert!(text.contains("`find_sessions`"));
        assert!(text.contains("`read_session`"));
        assert!(text.contains("`parent_session_id`"));
        assert!(text.contains("`user.submitted` and `user.steered`"));
        assert!(text.contains("unless the relevant scope was inspected exhaustively"));
        assert!(text.contains(r#""workspace":"/work/current""#));
        assert!(!text.contains("sqlite3"));
        assert!(!text.contains("session_database"));
        assert!(text.contains("global-memory scans"));
        assert!(text.contains("config show"));
        assert!(text.contains("read-only analysis turn"));
        assert!(text.contains("do not create, replace, or delete memories"));
        assert!(text.contains("`Findings`"));
        assert!(text.contains("`Recommended actions`"));
        assert!(!text.contains(MEMORY_REVIEW_CHECKPOINT));
    }

    #[test]
    fn restored_and_forked_sessions_start_with_followup_review() {
        let prompt = Submission::text("continue".to_owned());

        assert!(
            prompt_text(MemoryReviewState::restored(true).submission_prompt(&prompt))
                .contains(MEMORY_REVIEW_CHECKPOINT)
        );
        assert!(
            prompt_text(
                MemoryReviewState::fresh(true)
                    .forked()
                    .submission_prompt(&prompt)
            )
            .contains(MEMORY_REVIEW_CHECKPOINT)
        );
    }
}
