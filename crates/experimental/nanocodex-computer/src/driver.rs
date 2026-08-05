use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Instant,
};

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::{Instrument as _, Span, field::Empty, info_span};
use uuid::Uuid;

use crate::{
    Application, ComputerAction, ComputerActionResult, ComputerBuildError, ComputerError,
    ComputerEvent, ComputerFrame, InterventionReason, Point, SettlePolicy, Window,
    platform::{self, Backend},
};

#[cfg(test)]
use crate::ComputerOutput;

const RUNNING: u8 = 0;
const PAUSED: u8 = 1;
const STOPPED: u8 = 2;
const COMMAND_CAPACITY: usize = 16;
const EVENT_CAPACITY: usize = 256;

pub(crate) struct RunState {
    status: AtomicU8,
}

impl RunState {
    const fn new() -> Self {
        Self {
            status: AtomicU8::new(RUNNING),
        }
    }

    pub(crate) fn ensure_running(&self) -> Result<(), ComputerError> {
        match self.status.load(Ordering::Acquire) {
            RUNNING => Ok(()),
            PAUSED => Err(ComputerError::Paused),
            _ => Err(ComputerError::Stopped),
        }
    }
}

/// Cheap cloneable handle to one serial native computer-use session.
#[derive(Clone)]
pub struct Computer {
    inner: Arc<Inner>,
}

struct Inner {
    commands: mpsc::Sender<Command>,
    control: ComputerControl,
    events: broadcast::Sender<ComputerEvent>,
    frames: watch::Receiver<Option<Arc<ComputerFrame>>>,
    pointers: watch::Receiver<Option<AgentPointer>>,
    artifact_root: PathBuf,
    owned_artifacts: bool,
    _intervention_monitor: Option<platform::InterventionMonitor>,
}

#[derive(Clone, Copy)]
pub(crate) struct AgentPointer {
    pub(crate) point: Point,
    pub(crate) pressed: bool,
}

#[derive(Clone)]
pub(crate) struct PointerSink {
    pointers: watch::Sender<Option<AgentPointer>>,
}

impl PointerSink {
    pub(crate) fn publish(&self, point: Point, pressed: bool) {
        let _ = self.pointers.send(Some(AgentPointer { point, pressed }));
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.control.state.status.store(STOPPED, Ordering::Release);
        if self.owned_artifacts
            && let Err(error) = std::fs::remove_dir_all(&self.artifact_root)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.artifact_root.display(), %error, "failed to remove computer artifacts");
        }
    }
}

/// Receives the contractual ordered lifecycle stream.
pub struct ComputerEvents {
    receiver: broadcast::Receiver<ComputerEvent>,
}

impl ComputerEvents {
    /// Receives the next event, an explicit lag marker, or `None` after the
    /// driver has stopped.
    pub async fn recv(&mut self) -> Option<ComputerEvent> {
        match self.receiver.recv().await {
            Ok(event) => Some(event),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                Some(ComputerEvent::Lagged { skipped })
            }
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }
}

/// A coalescing stream that always retains the newest human-facing visual frame.
#[derive(Clone)]
pub struct ComputerFrames {
    receiver: watch::Receiver<Option<Arc<ComputerFrame>>>,
}

impl ComputerFrames {
    /// Borrows the newest frame without waiting.
    #[must_use]
    pub fn latest(&self) -> Option<Arc<ComputerFrame>> {
        self.receiver.borrow().clone()
    }

    /// Waits until a newer frame is available.
    pub async fn changed(&mut self) -> Result<Arc<ComputerFrame>, ComputerError> {
        loop {
            self.receiver
                .changed()
                .await
                .map_err(|_| ComputerError::DriverExited)?;
            if let Some(state) = self.receiver.borrow_and_update().clone() {
                return Ok(state);
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct FrameSink {
    frames: watch::Sender<Option<Arc<ComputerFrame>>>,
    events: broadcast::Sender<ComputerEvent>,
    last_target: Option<(Application, Window)>,
}

impl FrameSink {
    pub(crate) fn publish(&mut self, frame: ComputerFrame) {
        let target = (frame.application.clone(), frame.window.clone());
        if self.last_target.as_ref() != Some(&target) {
            send(
                &self.events,
                ComputerEvent::TargetChanged {
                    application: target.0.clone(),
                    window: target.1.clone(),
                },
            );
            self.last_target = Some(target);
        }
        send(
            &self.events,
            ComputerEvent::Frame {
                sequence: frame.sequence,
                generation: frame.generation,
                digest: frame.image.digest().to_owned(),
                phase: frame.phase,
            },
        );
        let _ = self.frames.send(Some(Arc::new(frame)));
    }
}

/// Out-of-band pause, resume, intervention, and stop capability.
#[derive(Clone)]
pub struct ComputerControl {
    state: Arc<RunState>,
    notices: mpsc::UnboundedSender<ControlNotice>,
}

impl ComputerControl {
    /// Pauses new actions immediately. In-flight settling also stops.
    pub fn pause(&self) {
        if self
            .state
            .status
            .compare_exchange(RUNNING, PAUSED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = self.notices.send(ControlNotice::Paused);
        }
    }

    /// Returns control to the agent after a human inspection or intervention.
    pub fn resume(&self) {
        if self
            .state
            .status
            .compare_exchange(PAUSED, RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = self.notices.send(ControlNotice::Resumed);
        }
    }

    /// Immediately records a human takeover and pauses agent actions.
    pub fn intervene(&self, reason: InterventionReason) {
        if self
            .state
            .status
            .compare_exchange(RUNNING, PAUSED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = self.notices.send(ControlNotice::Intervened(reason));
        }
    }

    /// Permanently stops the session.
    pub fn stop(&self) {
        if self.state.status.swap(STOPPED, Ordering::AcqRel) != STOPPED {
            let _ = self.notices.send(ControlNotice::Stopped);
        }
    }

    /// Returns whether actions are currently paused.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.state.status.load(Ordering::Acquire) == PAUSED
    }
}

enum ControlNotice {
    Paused,
    Resumed,
    Intervened(InterventionReason),
    Stopped,
}

enum Command {
    Execute {
        action: ComputerAction,
        parent: Span,
        queued_at: Instant,
        reply: oneshot::Sender<Result<ComputerActionResult, ComputerError>>,
    },
}

/// Configures a native computer-use session.
pub struct ComputerBuilder {
    artifact_root: Option<PathBuf>,
    maximum_elements: usize,
    settle: SettlePolicy,
    backend: Option<Box<dyn Backend>>,
    observe_human_input: bool,
    allowed_bundle_ids: Option<HashSet<String>>,
    allowed_url_origins: Option<Vec<String>>,
}

impl Default for ComputerBuilder {
    fn default() -> Self {
        Self {
            artifact_root: None,
            maximum_elements: 1_500,
            settle: SettlePolicy::default(),
            backend: None,
            observe_human_input: true,
            allowed_bundle_ids: None,
            allowed_url_origins: None,
        }
    }
}

impl ComputerBuilder {
    /// Stores screenshot artifacts beneath an explicit caller-owned directory.
    #[must_use]
    pub fn artifact_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.artifact_root = Some(path.into());
        self
    }

    /// Bounds one accessibility snapshot's depth-first element count.
    #[must_use]
    pub const fn maximum_elements(mut self, maximum: usize) -> Self {
        self.maximum_elements = maximum;
        self
    }

    /// Selects post-action visual/semantic settling policy.
    #[must_use]
    pub const fn settle_policy(mut self, policy: SettlePolicy) -> Self {
        self.settle = policy;
        self
    }

    /// Selects whether physical input directed at the attached application
    /// automatically pauses computer actions. Synthetic Nanocodex input and
    /// input targeting other applications are ignored.
    #[must_use]
    pub const fn observe_human_input(mut self, enabled: bool) -> Self {
        self.observe_human_input = enabled;
        self
    }

    /// Restricts discovery, launch, and attachment to explicitly allowed
    /// bundle identifiers. Calling this at least once changes the session from
    /// unrestricted to allowlist mode.
    #[must_use]
    pub fn allow_bundle_id(mut self, bundle_id: impl Into<String>) -> Self {
        self.allowed_bundle_ids
            .get_or_insert_default()
            .insert(bundle_id.into());
        self
    }

    /// Restricts browser documents and semantic links to explicitly allowed
    /// HTTP(S) origins. Calling this at least once enables fail-closed URL mode.
    #[must_use]
    pub fn allow_url_origin(mut self, origin: impl Into<String>) -> Self {
        self.allowed_url_origins
            .get_or_insert_default()
            .push(origin.into());
        self
    }

    /// Builds the actor and returns its independent event stream.
    pub fn build(mut self) -> Result<(Computer, ComputerEvents), ComputerBuildError> {
        if self.maximum_elements == 0 {
            return Err(ComputerBuildError::Configuration {
                message: "maximum_elements must be non-zero".to_owned(),
            });
        }
        if self.allowed_bundle_ids.as_ref().is_some_and(|bundle_ids| {
            bundle_ids
                .iter()
                .any(|bundle_id| bundle_id.trim().is_empty() || bundle_id.starts_with('-'))
        }) {
            return Err(ComputerBuildError::Configuration {
                message: "allowed bundle identifiers must be non-empty exact identifiers"
                    .to_owned(),
            });
        }
        let allowed_url_origins = self
            .allowed_url_origins
            .take()
            .map(|origins| {
                origins
                    .into_iter()
                    .map(|origin| {
                        let parsed = url::Url::parse(&origin).map_err(|error| {
                            ComputerBuildError::Configuration {
                                message: format!("invalid allowed URL origin {origin:?}: {error}"),
                            }
                        })?;
                        if !matches!(parsed.scheme(), "http" | "https")
                            || parsed.host_str().is_none()
                            || parsed.cannot_be_a_base()
                            || !parsed.username().is_empty()
                            || parsed.password().is_some()
                            || parsed.path() != "/"
                            || parsed.query().is_some()
                            || parsed.fragment().is_some()
                        {
                            return Err(ComputerBuildError::Configuration {
                                message: format!(
                                    "allowed URL origin must be an absolute HTTP(S) origin: {origin:?}"
                                ),
                            });
                        }
                        Ok(parsed)
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let _runtime =
            tokio::runtime::Handle::try_current().map_err(|_| ComputerBuildError::Runtime)?;
        let (artifact_root, owned_artifacts) = match self.artifact_root.take() {
            Some(path) => (path, false),
            None => (
                std::env::temp_dir().join(format!("nanocodex-computer-{}", Uuid::now_v7())),
                true,
            ),
        };
        std::fs::create_dir_all(&artifact_root).map_err(|source| {
            ComputerBuildError::ArtifactDirectory {
                path: artifact_root.clone(),
                source,
            }
        })?;

        let intervention_target = platform::intervention_target();
        let backend = self.backend.take().unwrap_or_else(|| {
            platform::native(
                artifact_root.clone(),
                self.settle,
                self.maximum_elements,
                Arc::clone(&intervention_target),
                self.allowed_bundle_ids.take(),
                allowed_url_origins,
            )
        });
        let (commands_tx, commands_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (events_tx, events_rx) = broadcast::channel(EVENT_CAPACITY);
        let (frames_tx, frames_rx) = watch::channel(None);
        let (pointers_tx, pointers_rx) = watch::channel(None);
        let (notices_tx, notices_rx) = mpsc::unbounded_channel();
        let state = Arc::new(RunState::new());
        let control = ComputerControl {
            state: Arc::clone(&state),
            notices: notices_tx,
        };
        let intervention_monitor = self.observe_human_input.then(|| {
            let control = control.clone();
            platform::intervention_monitor(intervention_target, move |event| {
                tracing::info!(
                    target: "nanocodex_computer",
                    event_kind = event.kind,
                    source_pid = event.source_pid,
                    target_pid = event.target_pid,
                    location_x = event.x,
                    location_y = event.y,
                    "physical input targeted the attached application"
                );
                control.intervene(InterventionReason::HumanInput);
            })
        });
        let (intervention_monitor, startup_permission) = match intervention_monitor {
            Some(Ok(monitor)) => (Some(monitor), None),
            Some(Err(_)) => (
                None,
                Some(ComputerEvent::PermissionRequired {
                    permission: crate::Permission::InputMonitoring,
                    guidance: "enable Input Monitoring for automatic human takeover; manual preview controls remain available"
                        .to_owned(),
                }),
            ),
            None => (None, None),
        };
        let session_id = Uuid::now_v7().to_string();
        tokio::spawn(run_driver(
            session_id,
            backend,
            commands_rx,
            notices_rx,
            events_tx.clone(),
            frames_tx,
            PointerSink {
                pointers: pointers_tx,
            },
            state,
            startup_permission,
        ));
        Ok((
            Computer {
                inner: Arc::new(Inner {
                    commands: commands_tx,
                    control,
                    events: events_tx,
                    frames: frames_rx,
                    pointers: pointers_rx,
                    artifact_root,
                    owned_artifacts,
                    _intervention_monitor: intervention_monitor,
                }),
            },
            ComputerEvents {
                receiver: events_rx,
            },
        ))
    }
}

impl Computer {
    /// Starts configuring a native computer-use session.
    #[must_use]
    pub fn builder() -> ComputerBuilder {
        ComputerBuilder::default()
    }

    /// Builds a session with bounded defaults.
    pub fn new() -> Result<(Self, ComputerEvents), ComputerBuildError> {
        Self::builder().build()
    }

    /// Returns an out-of-band human intervention capability.
    #[must_use]
    pub fn control(&self) -> ComputerControl {
        self.inner.control.clone()
    }

    /// Subscribes to the coalescing latest-frame stream.
    #[must_use]
    pub fn frames(&self) -> ComputerFrames {
        ComputerFrames {
            receiver: self.inner.frames.clone(),
        }
    }

    pub(crate) fn pointers(&self) -> watch::Receiver<Option<AgentPointer>> {
        self.inner.pointers.clone()
    }

    /// Subscribes to the ordered lifecycle stream.
    ///
    /// Each subscriber has an independent bounded cursor and receives an
    /// explicit lag marker if it falls behind.
    #[must_use]
    pub fn events(&self) -> ComputerEvents {
        ComputerEvents {
            receiver: self.inner.events.subscribe(),
        }
    }

    /// Returns the session's screenshot artifact directory.
    #[must_use]
    pub fn artifact_root(&self) -> &Path {
        &self.inner.artifact_root
    }

    /// Queues one action in strict session order and waits for its typed result.
    pub async fn execute(
        &self,
        action: ComputerAction,
    ) -> Result<ComputerActionResult, ComputerError> {
        self.inner.control.state.ensure_running()?;
        let (reply, result) = oneshot::channel();
        let parent = Span::current();
        let queued_at = Instant::now();
        self.inner
            .commands
            .send(Command::Execute {
                action,
                parent,
                queued_at,
                reply,
            })
            .await
            .map_err(|_| ComputerError::DriverExited)?;
        result.await.map_err(|_| ComputerError::DriverExited)?
    }

    /// Stops the session. All clones become permanently unusable.
    pub fn stop(&self) {
        self.inner.control.stop();
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the private actor takes each independently owned channel and service"
)]
async fn run_driver(
    session_id: String,
    mut backend: Box<dyn Backend>,
    mut commands: mpsc::Receiver<Command>,
    mut notices: mpsc::UnboundedReceiver<ControlNotice>,
    events: broadcast::Sender<ComputerEvent>,
    frames: watch::Sender<Option<Arc<ComputerFrame>>>,
    pointers: PointerSink,
    state: Arc<RunState>,
    startup_permission: Option<ComputerEvent>,
) {
    let mut frame_sink = FrameSink {
        frames,
        events: events.clone(),
        last_target: None,
    };
    send(
        &events,
        ComputerEvent::SessionStarted {
            session_id: session_id.clone(),
        },
    );
    if let Some(event) = startup_permission {
        send(&events, event);
    }
    let mut sequence = 0_u64;
    loop {
        tokio::select! {
            biased;
            notice = notices.recv() => {
                match notice {
                    Some(ControlNotice::Paused) => send(&events, ComputerEvent::Paused),
                    Some(ControlNotice::Resumed) => send(&events, ComputerEvent::Resumed),
                    Some(ControlNotice::Intervened(reason)) => {
                        send(&events, ComputerEvent::UserIntervened { reason });
                    }
                    Some(ControlNotice::Stopped) | None => break,
                }
            }
            command = commands.recv() => {
                let Some(Command::Execute {
                    action,
                    parent,
                    queued_at,
                    reply,
                }) = command else { break };
                sequence = sequence.saturating_add(1);
                if let Err(error) = state.ensure_running() {
                    let _ = reply.send(Err(error));
                    continue;
                }
                send(&events, ComputerEvent::ActionStarted {
                    sequence,
                    action: action.clone(),
                });
                let span = info_span!(
                    target: "nanocodex_computer",
                    parent: &parent,
                    "computer.action",
                    computer.session.id = session_id.as_str(),
                    computer.action.sequence = sequence,
                    computer.action.kind = action.kind(),
                    computer.action.queue.duration_ns = Empty,
                    duration_ns = Empty,
                    status = Empty,
                    otel.status_code = Empty,
                );
                span.record(
                    "computer.action.queue.duration_ns",
                    elapsed_ns(queued_at.elapsed()),
                );
                let started = Instant::now();
                let outcome = async {
                    trace_content("computer.action.input", &action);
                    backend
                        .execute(
                            action,
                            sequence,
                            Arc::clone(&state),
                            &mut frame_sink,
                            &pointers,
                        )
                        .await
                }
                .instrument(span.clone())
                .await;
                span.record("duration_ns", elapsed_ns(started.elapsed()));
                match outcome {
                    Ok(output) => {
                        let result = ComputerActionResult {
                            sequence,
                            elapsed_ms: millis(started.elapsed()),
                            output,
                        };
                        send(&events, ComputerEvent::ActionCompleted {
                            result: result.clone(),
                        });
                        span.record("status", "ok");
                        span.record("otel.status_code", "OK");
                        span.in_scope(|| trace_content("computer.action.output", &result));
                        let _ = reply.send(Ok(result));
                    }
                    Err(error) => {
                        span.record("status", "failed");
                        span.record("otel.status_code", "ERROR");
                        span.in_scope(|| tracing::error!(%error, "computer action failed"));
                        if let ComputerError::Permission { permission, guidance } = &error {
                            send(&events, ComputerEvent::PermissionRequired {
                                permission: *permission,
                                guidance: guidance.clone(),
                            });
                        }
                        send(&events, ComputerEvent::Failed {
                            sequence: Some(sequence),
                            message: error.to_string(),
                        });
                        let _ = reply.send(Err(error));
                    }
                }
            }
        }
    }
    state.status.store(STOPPED, Ordering::Release);
    send(&events, ComputerEvent::Stopped);
}

fn send(events: &broadcast::Sender<ComputerEvent>, event: ComputerEvent) {
    let _ = events.send(event);
}

fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn elapsed_ns(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn trace_content(name: &'static str, value: &impl serde::Serialize) {
    match serde_json::to_string(value) {
        Ok(content) => tracing::info!(
            target: "nanocodex_computer",
            event_name = name,
            content,
            "computer observed ordered content"
        ),
        Err(error) => tracing::warn!(
            target: "nanocodex_computer",
            event_name = name,
            %error,
            "failed to serialize computer content"
        ),
    }
}

#[cfg(test)]
pub(crate) fn recording_builder() -> (ComputerBuilder, Arc<std::sync::Mutex<Vec<ComputerAction>>>) {
    let actions = Arc::new(std::sync::Mutex::new(Vec::new()));
    let backend = RecordingBackend {
        actions: Arc::clone(&actions),
    };
    (
        ComputerBuilder {
            backend: Some(Box::new(backend)),
            observe_human_input: false,
            ..ComputerBuilder::default()
        },
        actions,
    )
}

#[cfg(test)]
struct RecordingBackend {
    actions: Arc<std::sync::Mutex<Vec<ComputerAction>>>,
}

#[cfg(test)]
#[async_trait::async_trait]
impl Backend for RecordingBackend {
    async fn execute(
        &mut self,
        action: ComputerAction,
        sequence: u64,
        _state: Arc<RunState>,
        frames: &mut FrameSink,
        _pointers: &PointerSink,
    ) -> Result<ComputerOutput, ComputerError> {
        if matches!(action, ComputerAction::Observe { .. }) {
            frames.publish(ComputerFrame {
                sequence,
                generation: sequence,
                application: Application {
                    pid: 1,
                    name: "Fixture".to_owned(),
                    bundle_id: Some("dev.nanocodex.fixture".to_owned()),
                },
                window: Window {
                    id: 1,
                    pid: 1,
                    title: Some("Fixture window".to_owned()),
                    frame: crate::Rect {
                        x: 0.0,
                        y: 0.0,
                        width: 1.0,
                        height: 1.0,
                    },
                    on_screen: true,
                },
                phase: crate::ComputerFramePhase::Observed,
                image: Arc::new(crate::CapturedImage::new(
                    1,
                    1,
                    "fixture-digest".to_owned(),
                    Arc::from(&b"fixture-png"[..]),
                )),
            });
        }
        self.actions.lock().unwrap().push(action);
        Ok(ComputerOutput::Done)
    }
}
