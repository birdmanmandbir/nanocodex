use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::{Instrument as _, Span, field::Empty, info_span};
use uuid::Uuid;

use crate::{
    ComputerAction, ComputerActionResult, ComputerBuildError, ComputerError, ComputerEvent,
    ComputerFrame, ComputerOutput, InterventionReason, SettlePolicy,
    platform::{self, Backend},
};

#[cfg(target_os = "macos")]
use crate::Point;
#[cfg(any(target_os = "macos", test))]
use crate::{Application, Window};

const RUNNING: u8 = 0;
const PAUSED: u8 = 1;
const STOPPED: u8 = 2;
const COMMAND_CAPACITY: usize = 16;
const EVENT_CAPACITY: usize = 256;
const HUMAN_ACTIVITY_QUIET_PERIOD: Duration = Duration::from_secs(1);

pub(crate) struct RunState {
    status: AtomicU8,
    human: Mutex<HumanState>,
    human_changed: tokio::sync::Notify,
}

#[derive(Default)]
struct HumanState {
    revision: u64,
    dirty_target: Option<TargetRevision>,
    activity: Option<HumanActivity>,
    active_action: Option<TargetRevision>,
}

#[derive(Clone, Copy)]
struct HumanActivity {
    target_pid: i32,
    revision: u64,
    quiet_at: Instant,
}

#[derive(Clone, Copy)]
struct TargetRevision {
    target_pid: i32,
    revision: u64,
}

#[derive(Clone, Copy)]
struct ActionContext {
    revision: u64,
}

enum BeginActionError {
    HumanActive,
    Lifecycle(ComputerError),
}

#[derive(Clone, Copy)]
struct ActivityTransition {
    started: bool,
    replaced_target: Option<i32>,
}

#[derive(Clone, Copy)]
struct QuietedActivity {
    target_pid: i32,
    requires_requery: bool,
}

impl RunState {
    fn new() -> Self {
        Self {
            status: AtomicU8::new(RUNNING),
            human: Mutex::new(HumanState::default()),
            human_changed: tokio::sync::Notify::new(),
        }
    }

    pub(crate) fn ensure_running(&self) -> Result<(), ComputerError> {
        match self.status.load(Ordering::Acquire) {
            RUNNING => Ok(()),
            PAUSED => Err(ComputerError::Paused),
            _ => Err(ComputerError::Stopped),
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn ensure_action_current(&self) -> Result<(), ComputerError> {
        self.ensure_running()?;
        let human = self.human();
        if human.active_action.is_some_and(|action| {
            human.dirty_target.is_some_and(|dirty| {
                dirty.target_pid == action.target_pid && dirty.revision > action.revision
            })
        }) {
            return Err(ComputerError::RequeryRequired);
        }
        Ok(())
    }

    fn begin_action(
        &self,
        target_pid: Option<i32>,
        coordinates_with_human_activity: bool,
        requires_fresh_observation: bool,
    ) -> Result<ActionContext, BeginActionError> {
        self.ensure_running().map_err(BeginActionError::Lifecycle)?;
        let mut human = self.human();
        if coordinates_with_human_activity && human.activity.is_some() {
            return Err(BeginActionError::HumanActive);
        }
        if requires_fresh_observation
            && target_pid.is_some_and(|pid| {
                human
                    .dirty_target
                    .is_some_and(|dirty| dirty.target_pid == pid)
            })
        {
            return Err(BeginActionError::Lifecycle(ComputerError::RequeryRequired));
        }
        let context = ActionContext {
            revision: human.revision,
        };
        human.active_action = target_pid.map(|target_pid| TargetRevision {
            target_pid,
            revision: context.revision,
        });
        Ok(context)
    }

    fn finish_action(&self) {
        self.human().active_action = None;
    }

    fn reconcile_observation(&self, target_pid: i32, context: ActionContext) -> bool {
        let mut human = self.human();
        let invalidated = human.dirty_target.is_some_and(|dirty| {
            dirty.target_pid == target_pid && dirty.revision > context.revision
        });
        if !invalidated {
            human.dirty_target = None;
        }
        !invalidated
    }

    fn record_human_activity(&self, target_pid: i32, quiet_period: Duration) -> ActivityTransition {
        let now = Instant::now();
        let mut human = self.human();
        human.revision = human.revision.saturating_add(1);
        let revision = human.revision;
        human.dirty_target = Some(TargetRevision {
            target_pid,
            revision,
        });
        let replaced_target = human
            .activity
            .filter(|activity| activity.target_pid != target_pid || activity.quiet_at <= now)
            .map(|activity| activity.target_pid);
        let started = human
            .activity
            .is_none_or(|activity| activity.target_pid != target_pid || activity.quiet_at <= now);
        human.activity = Some(HumanActivity {
            target_pid,
            revision,
            quiet_at: now + quiet_period,
        });
        drop(human);
        self.human_changed.notify_waiters();
        ActivityTransition {
            started,
            replaced_target,
        }
    }

    fn has_human_activity(&self) -> bool {
        self.human().activity.is_some()
    }

    async fn wait_for_human_quiet(&self) -> Result<Option<QuietedActivity>, ComputerError> {
        loop {
            self.ensure_running()?;
            let notified = self.human_changed.notified();
            let Some(activity) = self.human().activity else {
                return Ok(None);
            };
            let now = Instant::now();
            if activity.quiet_at <= now {
                let mut human = self.human();
                if human.activity.is_some_and(|current| {
                    current.revision == activity.revision && current.quiet_at <= Instant::now()
                }) {
                    human.activity = None;
                    let requires_requery = human
                        .dirty_target
                        .is_some_and(|dirty| dirty.target_pid == activity.target_pid);
                    return Ok(Some(QuietedActivity {
                        target_pid: activity.target_pid,
                        requires_requery,
                    }));
                }
                continue;
            }
            tokio::select! {
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(activity.quiet_at)) => {}
                () = notified => {}
            }
        }
    }

    fn human(&self) -> MutexGuard<'_, HumanState> {
        self.human.lock().unwrap_or_else(|poisoned| {
            tracing::error!(target: "nanocodex_computer", "human activity state lock was poisoned");
            poisoned.into_inner()
        })
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
    #[cfg(target_os = "macos")]
    pointers: watch::Receiver<Option<AgentPointer>>,
    artifact_root: PathBuf,
    owned_artifacts: bool,
    _intervention_monitor: Option<platform::InterventionMonitor>,
}

#[derive(Clone, Copy)]
#[cfg(target_os = "macos")]
pub(crate) struct AgentPointer {
    pub(crate) point: Point,
    pub(crate) pressed: bool,
    pub(crate) travel_duration: Duration,
}

#[derive(Clone)]
pub(crate) struct PointerSink {
    #[cfg(target_os = "macos")]
    pointers: watch::Sender<Option<AgentPointer>>,
}

#[cfg(target_os = "macos")]
impl PointerSink {
    pub(crate) fn publish(&self, point: Point, pressed: bool) {
        let _ = self.pointers.send(Some(AgentPointer {
            point,
            pressed,
            travel_duration: Duration::ZERO,
        }));
    }

    pub(crate) fn prepare(&self, point: Point) -> Duration {
        let duration =
            self.pointers
                .borrow()
                .as_ref()
                .map_or(Duration::from_millis(180), |previous| {
                    let distance = ((point.x - previous.point.x).powi(2)
                        + (point.y - previous.point.y).powi(2))
                    .sqrt();
                    if distance < 2.0 {
                        Duration::ZERO
                    } else {
                        Duration::from_secs_f64((0.08 + distance / 2_400.0).clamp(0.08, 0.32))
                    }
                });
        let _ = self.pointers.send(Some(AgentPointer {
            point,
            pressed: false,
            travel_duration: duration,
        }));
        duration
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
    #[cfg(any(target_os = "macos", test))]
    frames: watch::Sender<Option<Arc<ComputerFrame>>>,
    #[cfg(any(target_os = "macos", test))]
    events: broadcast::Sender<ComputerEvent>,
    #[cfg(any(target_os = "macos", test))]
    last_target: Option<(Application, Window)>,
}

impl FrameSink {
    #[cfg(any(target_os = "macos", test))]
    const fn new(
        frames: watch::Sender<Option<Arc<ComputerFrame>>>,
        events: broadcast::Sender<ComputerEvent>,
    ) -> Self {
        Self {
            frames,
            events,
            last_target: None,
        }
    }

    #[cfg(not(any(target_os = "macos", test)))]
    fn new(
        frames: watch::Sender<Option<Arc<ComputerFrame>>>,
        events: broadcast::Sender<ComputerEvent>,
    ) -> Self {
        drop(frames);
        drop(events);
        Self {}
    }

    #[cfg(any(target_os = "macos", test))]
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
            self.state.human_changed.notify_waiters();
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
            self.state.human_changed.notify_waiters();
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
            self.state.human_changed.notify_waiters();
        }
    }

    /// Permanently stops the session.
    pub fn stop(&self) {
        if self.state.status.swap(STOPPED, Ordering::AcqRel) != STOPPED {
            let _ = self.notices.send(ControlNotice::Stopped);
            self.state.human_changed.notify_waiters();
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
    HumanActivityChanged,
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
    human_input_quiet_period: Duration,
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
            human_input_quiet_period: HUMAN_ACTIVITY_QUIET_PERIOD,
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
    /// temporarily yields control and invalidates its last observation.
    /// Synthetic Nanocodex input and input targeting other applications are
    /// ignored.
    #[must_use]
    pub const fn observe_human_input(mut self, enabled: bool) -> Self {
        self.observe_human_input = enabled;
        self
    }

    /// Selects how long the attached application must remain free of physical
    /// input before queued computer actions may continue.
    #[must_use]
    pub const fn human_input_quiet_period(mut self, duration: Duration) -> Self {
        self.human_input_quiet_period = duration;
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
        if self.human_input_quiet_period.is_zero() {
            return Err(ComputerBuildError::Configuration {
                message: "human_input_quiet_period must be non-zero".to_owned(),
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
        #[cfg(target_os = "macos")]
        let (pointer_sink, pointers_rx) = {
            let (pointers_tx, pointers_rx) = watch::channel(None);
            (
                PointerSink {
                    pointers: pointers_tx,
                },
                pointers_rx,
            )
        };
        #[cfg(not(target_os = "macos"))]
        let pointer_sink = PointerSink {};
        let (notices_tx, notices_rx) = mpsc::unbounded_channel();
        let state = Arc::new(RunState::new());
        let control = ComputerControl {
            state: Arc::clone(&state),
            notices: notices_tx,
        };
        let intervention_monitor = self.observe_human_input.then(|| {
            let control = control.clone();
            let events = events_tx.clone();
            let quiet_period = self.human_input_quiet_period;
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
                record_human_activity(&control, &events, event.target_pid, quiet_period);
            })
        });
        let (intervention_monitor, startup_permission) = match intervention_monitor {
            Some(Ok(monitor)) => (Some(monitor), None),
            Some(Err(_)) => (
                None,
                Some(ComputerEvent::PermissionRequired {
                    permission: crate::Permission::InputMonitoring,
                    guidance: "enable Input Monitoring for automatic human activity coordination; manual preview controls remain available"
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
            pointer_sink,
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
                    #[cfg(target_os = "macos")]
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

    #[cfg(target_os = "macos")]
    pub(crate) fn pointers(&self) -> watch::Receiver<Option<AgentPointer>> {
        self.inner.pointers.clone()
    }

    #[cfg(test)]
    pub(crate) fn simulate_human_input(&self, target_pid: i32, quiet_period: Duration) {
        record_human_activity(
            &self.inner.control,
            &self.inner.events,
            target_pid,
            quiet_period,
        );
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
    let mut frame_sink = FrameSink::new(frames, events.clone());
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
    let mut current_target_pid = None;
    'driver: loop {
        tokio::select! {
            biased;
            notice = notices.recv() => {
                match notice {
                    Some(ControlNotice::Paused) => send(&events, ComputerEvent::Paused),
                    Some(ControlNotice::Resumed) => send(&events, ComputerEvent::Resumed),
                    Some(ControlNotice::Intervened(reason)) => {
                        send(&events, ComputerEvent::UserIntervened { reason });
                    }
                    Some(ControlNotice::HumanActivityChanged) => {}
                    Some(ControlNotice::Stopped) | None => break,
                }
            }
            quieted = state.wait_for_human_quiet(), if state.has_human_activity() => {
                match quieted {
                    Ok(Some(quieted)) => send_human_activity_ended(&events, quieted),
                    Ok(None) => {}
                    Err(ComputerError::Stopped) => break,
                    Err(ComputerError::Paused) => {}
                    Err(error) => tracing::warn!(target: "nanocodex_computer", %error, "human activity gate failed"),
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
                let action_context = loop {
                    match state.begin_action(
                        current_target_pid,
                        coordinates_with_human_activity(&action),
                        requires_fresh_observation(&action),
                    ) {
                        Ok(context) => break context,
                        Err(BeginActionError::HumanActive) => {
                            match state.wait_for_human_quiet().await {
                                Ok(Some(quieted)) => send_human_activity_ended(&events, quieted),
                                Ok(None) => {}
                                Err(error) => {
                                    let _ = reply.send(Err(error));
                                    continue 'driver;
                                }
                            }
                        }
                        Err(BeginActionError::Lifecycle(error)) => {
                            send(&events, ComputerEvent::Failed {
                                sequence: Some(sequence),
                                message: error.to_string(),
                            });
                            let _ = reply.send(Err(error));
                            continue 'driver;
                        }
                    }
                };
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
                state.finish_action();
                let outcome = outcome.and_then(|output| {
                    let ComputerOutput::State { state: observed } = &output else {
                        return Ok(output);
                    };
                    if !state.reconcile_observation(observed.application.pid, action_context) {
                        return Err(ComputerError::RequeryRequired);
                    }
                    current_target_pid = Some(observed.application.pid);
                    Ok(output)
                });
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

fn record_human_activity(
    control: &ComputerControl,
    events: &broadcast::Sender<ComputerEvent>,
    target_pid: i32,
    quiet_period: Duration,
) {
    let transition = control
        .state
        .record_human_activity(target_pid, quiet_period);
    let _ = control.notices.send(ControlNotice::HumanActivityChanged);
    if let Some(target_pid) = transition.replaced_target {
        send(
            events,
            ComputerEvent::HumanActivityEnded {
                target_pid,
                requires_requery: true,
            },
        );
    }
    if transition.started {
        send(
            events,
            ComputerEvent::HumanActivityStarted {
                target_pid,
                quiet_period_ms: millis(quiet_period),
            },
        );
    }
}

fn send_human_activity_ended(events: &broadcast::Sender<ComputerEvent>, quieted: QuietedActivity) {
    send(
        events,
        ComputerEvent::HumanActivityEnded {
            target_pid: quieted.target_pid,
            requires_requery: quieted.requires_requery,
        },
    );
}

const fn coordinates_with_human_activity(action: &ComputerAction) -> bool {
    !matches!(
        action,
        ComputerAction::ListApplications | ComputerAction::OpenApplication { .. }
    )
}

const fn requires_fresh_observation(action: &ComputerAction) -> bool {
    matches!(
        action,
        ComputerAction::Click { .. }
            | ComputerAction::Drag { .. }
            | ComputerAction::Scroll { .. }
            | ComputerAction::PressKey { .. }
            | ComputerAction::TypeText { .. }
            | ComputerAction::SetValue { .. }
            | ComputerAction::PerformAction { .. }
    )
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
            let application = Application {
                pid: 1,
                name: "Fixture".to_owned(),
                bundle_id: Some("dev.nanocodex.fixture".to_owned()),
            };
            let window = Window {
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
            };
            frames.publish(ComputerFrame {
                sequence,
                generation: sequence,
                application: application.clone(),
                window: window.clone(),
                phase: crate::ComputerFramePhase::Observed,
                image: Arc::new(crate::CapturedImage::new(
                    1,
                    1,
                    "fixture-digest".to_owned(),
                    Arc::from(&b"fixture-png"[..]),
                )),
            });
            self.actions.lock().unwrap().push(action);
            return Ok(ComputerOutput::State {
                state: crate::ComputerObservation {
                    generation: sequence,
                    application,
                    window,
                    elements: Vec::new(),
                    accessibility_update: None,
                    screenshot: None,
                    settled: true,
                },
            });
        }
        self.actions.lock().unwrap().push(action);
        Ok(ComputerOutput::Done)
    }
}
