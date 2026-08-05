use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use accessibility::{AXAttribute, AXUIElement, attribute::AXUIElementAttributes};
use async_trait::async_trait;
use core_foundation::{base::TCFType, string::CFString, url::CFURL};
use core_graphics::{
    event::{CGEvent, CGEventFlags, CGEventType, CGMouseButton, KeyCode, ScrollEventUnit},
    event_source::{CGEventSource, CGEventSourceStateID},
    geometry::CGPoint,
};
use image::ImageEncoder as _;
use nanocodex_computer_macos::{
    AccessibilityNotificationMonitor, AccessibilitySignalSnapshot, NativeImageData, NativeWindow,
    PermissionRequest, activate_application, capture_window, element_rect,
    enable_application_accessibility as enable_native_application_accessibility,
    frontmost_application_pid, mark_synthetic, request_accessibility, request_screen_capture,
    screen_locked, window as native_window_by_id,
};
use sha2::{Digest as _, Sha256};
use tracing::{Instrument as _, Span, field::Empty, info, info_span};
use url::Url;

use super::Backend;
use crate::{
    AccessibilityUpdate, Application, ApplicationSelector, CapturedImage, ComputerAction,
    ComputerError, ComputerFrame, ComputerFramePhase, ComputerObservation, ComputerOutput, Element,
    ElementRef, InteractionTarget, KeyModifier, MouseButton, Permission, Point, Rect,
    ScreenshotArtifact, SettlePolicy, Window,
    driver::{FrameSink, PointerSink, RunState},
};

const MAX_TEXT_CHARS: usize = 2_000;
const MAX_TREE_DEPTH: usize = 80;
const MAX_RETAINED_SCREENSHOTS: usize = 16;
const ACCESSIBILITY_PRIME_ELEMENTS: usize = 64;
const ACCESSIBILITY_PRIME_THRESHOLD: usize = 12;
const ACCESSIBILITY_PRIME_TIMEOUT: Duration = Duration::from_secs(5);
const APPLICATION_ACTIVATION_TIMEOUT: Duration = Duration::from_millis(750);

#[derive(Clone)]
struct Target {
    application: Application,
    window: Window,
    follow_key_window: bool,
}

impl Target {
    fn with_native_window(mut self, window: &NativeWindow) -> Self {
        self.window = public_window(window);
        self
    }
}

type Discovery = (Vec<Application>, Vec<Window>, Vec<NativeWindow>);

struct VisualSample {
    application: Application,
    window: Window,
    image: Arc<CapturedImage>,
    loading: bool,
    follow_key_window: bool,
}

struct AccessibilityRevision {
    generation: u64,
    pid: i32,
    window_id: u32,
    elements: Vec<Element>,
}

struct ObservationRequest {
    target: Target,
    generation: u64,
    sequence: u64,
    screenshot: bool,
    settled: bool,
    artifact_root: PathBuf,
    maximum_elements: usize,
}

struct ApplicationPriming {
    pid: i32,
    initial_elements: usize,
    final_elements: usize,
    activated: bool,
    focus_restored: bool,
}

pub(super) struct MacosBackend {
    artifact_root: PathBuf,
    settle: SettlePolicy,
    maximum_elements: usize,
    target: Option<Target>,
    generation: u64,
    accessibility_revision: Option<AccessibilityRevision>,
    screenshots: VecDeque<PathBuf>,
    intervention_target: Arc<super::InterventionTarget>,
    allowed_bundle_ids: Option<HashSet<String>>,
    accessibility_monitor: Option<AccessibilityNotificationMonitor>,
    allowed_url_origins: Option<Vec<Url>>,
    blocked_url: Option<String>,
}

impl MacosBackend {
    pub(super) const fn new(
        artifact_root: PathBuf,
        settle: SettlePolicy,
        maximum_elements: usize,
        intervention_target: Arc<super::InterventionTarget>,
        allowed_bundle_ids: Option<HashSet<String>>,
        allowed_url_origins: Option<Vec<Url>>,
    ) -> Self {
        Self {
            artifact_root,
            settle,
            maximum_elements,
            target: None,
            generation: 0,
            accessibility_revision: None,
            screenshots: VecDeque::new(),
            intervention_target,
            allowed_bundle_ids,
            accessibility_monitor: None,
            allowed_url_origins,
            blocked_url: None,
        }
    }

    fn authorize_application(&self, application: &Application) -> Result<(), ComputerError> {
        let Some(allowed) = &self.allowed_bundle_ids else {
            return Ok(());
        };
        let authorized = application
            .bundle_id
            .as_ref()
            .is_some_and(|bundle_id| allowed.contains(bundle_id));
        authorized
            .then_some(())
            .ok_or_else(|| ComputerError::ApplicationDenied {
                application: application
                    .bundle_id
                    .clone()
                    .unwrap_or_else(|| application.name.clone()),
            })
    }

    fn authorize_bundle_id(&self, bundle_id: &str) -> Result<(), ComputerError> {
        self.allowed_bundle_ids
            .as_ref()
            .is_none_or(|allowed| allowed.contains(bundle_id))
            .then_some(())
            .ok_or_else(|| ComputerError::ApplicationDenied {
                application: bundle_id.to_owned(),
            })
    }

    fn authorize_url(&self, raw: &str) -> Result<(), ComputerError> {
        enforce_url_policy(raw, self.allowed_url_origins.as_deref())
    }

    fn authorize_observation_urls(
        &mut self,
        observed: &ComputerObservation,
    ) -> Result<(), ComputerError> {
        let denied = observed
            .elements
            .iter()
            .filter(|element| element.role == "AXWebArea")
            .filter_map(|element| element.url.as_deref())
            .find_map(|url| self.authorize_url(url).err());
        if let Some(ComputerError::UrlDenied { url }) = denied {
            self.blocked_url = Some(url.clone());
            return Err(ComputerError::UrlDenied { url });
        }
        Ok(())
    }

    async fn observe(
        &mut self,
        sequence: u64,
        screenshot: bool,
        settled: bool,
        phase: ComputerFramePhase,
        frames: &mut FrameSink,
    ) -> Result<ComputerObservation, ComputerError> {
        let target = self.target.clone().ok_or(ComputerError::NoTarget)?;
        let follow_key_window = target.follow_key_window;
        self.generation = self.generation.saturating_add(1);
        let generation = self.generation;
        let maximum = self.maximum_elements;
        let request = ObservationRequest {
            target,
            generation,
            sequence,
            screenshot,
            settled,
            artifact_root: self.artifact_root.clone(),
            maximum_elements: maximum,
        };
        let span = info_span!(
            target: "nanocodex_computer",
            "computer.observe",
            computer.action.sequence = sequence,
            computer.observation.generation = generation,
            computer.screenshot.requested = screenshot,
            computer.accessibility.maximum_elements = maximum,
            computer.accessibility.element_count = Empty,
            computer.accessibility.update.added = Empty,
            computer.accessibility.update.changed = Empty,
            computer.accessibility.update.removed = Empty,
            computer.accessibility.notification.revision = Empty,
            computer.screenshot.bytes = Empty,
            computer.screenshot.width = Empty,
            computer.screenshot.height = Empty,
            duration_ns = Empty,
            status = Empty,
            otel.status_code = Empty,
        );
        let worker_parent = span.clone();
        let started = Instant::now();
        let outcome = async move {
            tokio::task::spawn_blocking(move || observe_target(request, &worker_parent))
                .await
                .map_err(|error| ComputerError::Native {
                    message: format!("observation worker panicked: {error}"),
                })?
        }
        .instrument(span.clone())
        .await;
        finish_span(&span, started, &outcome);
        let (mut observed, image) = outcome?;
        span.record(
            "computer.accessibility.notification.revision",
            self.accessibility_signal().revision,
        );
        self.authorize_observation_urls(&observed)?;
        self.apply_accessibility_revision(&mut observed);
        record_observation(&span, &observed, image.as_deref());
        self.target = Some(Target {
            application: observed.application.clone(),
            window: observed.window.clone(),
            follow_key_window,
        });
        if let Some(image) = image {
            frames.publish(ComputerFrame {
                sequence,
                generation,
                application: observed.application.clone(),
                window: observed.window.clone(),
                phase,
                image,
            });
        }
        self.retain_screenshot(&observed).await;
        Ok(observed)
    }

    async fn settled_observation(
        &mut self,
        sequence: u64,
        state: Arc<RunState>,
        frames: &mut FrameSink,
        protected_frontmost: Option<i32>,
    ) -> Result<ComputerObservation, ComputerError> {
        let span = info_span!(
            target: "nanocodex_computer",
            "computer.app.settle",
            computer.action.sequence = sequence,
            computer.settle.timeout_ms = millis(self.settle.timeout),
            computer.settle.sample_interval_ms = millis(self.settle.sample_interval),
            computer.settle.minimum_duration_ms = millis(self.settle.minimum_duration),
            computer.settle.sample_count = Empty,
            computer.settle.loading_sample_count = Empty,
            computer.settle.notification_wake_count = Empty,
            computer.settle.poll_wake_count = Empty,
            computer.settle.wait_duration_ns = Empty,
            computer.focus.steal_count = Empty,
            computer.focus.restore_count = Empty,
            computer.accessibility.notification.count = Empty,
            computer.accessibility.notification.tree_count = Empty,
            computer.accessibility.notification.window_count = Empty,
            computer.accessibility.notification.busy_count = Empty,
            computer.settle.settled = Empty,
            duration_ns = Empty,
            status = Empty,
            otel.status_code = Empty,
        );
        let started = Instant::now();
        let outcome = self
            .settled_observation_inner(sequence, state, frames, &span, protected_frontmost)
            .instrument(span.clone())
            .await;
        finish_span(&span, started, &outcome);
        outcome
    }

    fn accessibility_signal(&self) -> AccessibilitySignalSnapshot {
        self.accessibility_monitor
            .as_ref()
            .map(AccessibilityNotificationMonitor::snapshot)
            .unwrap_or_default()
    }

    async fn wait_for_observation_change(
        &self,
        revision: u64,
        deadline: Instant,
    ) -> Result<bool, ComputerError> {
        let timeout = self
            .settle
            .sample_interval
            .min(deadline.saturating_duration_since(Instant::now()));
        if timeout.is_zero() {
            return Ok(false);
        }
        let Some(waiter) = self
            .accessibility_monitor
            .as_ref()
            .map(AccessibilityNotificationMonitor::waiter)
        else {
            tokio::time::sleep(timeout).await;
            return Ok(false);
        };
        tokio::task::spawn_blocking(move || waiter.wait_for_change(revision, timeout))
            .await
            .map_err(|error| ComputerError::Native {
                message: format!("Accessibility wait worker panicked: {error}"),
            })
    }

    async fn settled_observation_inner(
        &mut self,
        sequence: u64,
        state: Arc<RunState>,
        frames: &mut FrameSink,
        span: &Span,
        protected_frontmost: Option<i32>,
    ) -> Result<ComputerObservation, ComputerError> {
        let started = Instant::now();
        let deadline = started + self.settle.timeout;
        let mut previous: Option<String> = None;
        let initial_signal = self.accessibility_signal();
        let mut previous_signal_revision: Option<u64> = None;
        let generation = self.generation.saturating_add(1);
        let mut sample_count = 0_u64;
        let mut loading_sample_count = 0_u64;
        let mut notification_wake_count = 0_u64;
        let mut poll_wake_count = 0_u64;
        let mut wait_duration = Duration::ZERO;
        let mut focus_steal_count = 0_u64;
        let mut focus_restore_count = 0_u64;
        loop {
            state.ensure_running()?;
            let target = self.target.clone().ok_or(ComputerError::NoTarget)?;
            sample_count = sample_count.saturating_add(1);
            let sample_parent = span.clone();
            let sample = tokio::task::spawn_blocking(move || {
                visual_sample(target, &sample_parent, sample_count)
            })
            .await
            .map_err(|error| ComputerError::Native {
                message: format!("observation worker panicked: {error}"),
            })??;
            self.target = Some(Target {
                application: sample.application.clone(),
                window: sample.window.clone(),
                follow_key_window: sample.follow_key_window,
            });
            if sample.loading {
                loading_sample_count = loading_sample_count.saturating_add(1);
            }
            if let Some(previous_frontmost) = protected_frontmost
                && previous_frontmost != sample.application.pid
            {
                let target_pid = sample.application.pid;
                let focus_restored = tokio::task::spawn_blocking(move || {
                    suppress_focus_steal(previous_frontmost, target_pid)
                })
                .await
                .map_err(|error| ComputerError::Native {
                    message: format!("focus observer panicked: {error}"),
                })?;
                if let Some(restored) = focus_restored {
                    focus_steal_count = focus_steal_count.saturating_add(1);
                    focus_restore_count = focus_restore_count.saturating_add(u64::from(restored));
                }
            }
            let signature = visual_signature(&sample);
            let signal = self.accessibility_signal();
            let matched = previous.as_deref() == Some(signature.as_str())
                && previous_signal_revision == Some(signal.revision)
                && !sample.loading
                && started.elapsed() >= self.settle.minimum_duration;
            let timed_out = Instant::now() >= deadline;
            let phase = if matched {
                ComputerFramePhase::Settled
            } else if timed_out {
                ComputerFramePhase::TimedOut
            } else {
                ComputerFramePhase::Settling
            };
            frames.publish(ComputerFrame {
                sequence,
                generation,
                application: sample.application.clone(),
                window: sample.window.clone(),
                phase,
                image: Arc::clone(&sample.image),
            });
            if matched || timed_out {
                span.record("computer.settle.sample_count", sample_count);
                span.record("computer.settle.loading_sample_count", loading_sample_count);
                span.record(
                    "computer.settle.notification_wake_count",
                    notification_wake_count,
                );
                span.record("computer.settle.poll_wake_count", poll_wake_count);
                span.record(
                    "computer.settle.wait_duration_ns",
                    elapsed_ns(wait_duration),
                );
                span.record("computer.focus.steal_count", focus_steal_count);
                span.record("computer.focus.restore_count", focus_restore_count);
                span.record(
                    "computer.accessibility.notification.count",
                    signal.revision.saturating_sub(initial_signal.revision),
                );
                span.record(
                    "computer.accessibility.notification.tree_count",
                    signal
                        .tree_revision
                        .saturating_sub(initial_signal.tree_revision),
                );
                span.record(
                    "computer.accessibility.notification.window_count",
                    signal
                        .window_revision
                        .saturating_sub(initial_signal.window_revision),
                );
                span.record(
                    "computer.accessibility.notification.busy_count",
                    signal
                        .busy_revision
                        .saturating_sub(initial_signal.busy_revision),
                );
                span.record("computer.settle.settled", matched);
                let root = self.artifact_root.clone();
                let maximum = self.maximum_elements;
                let settled = matched;
                let follow_key_window = sample.follow_key_window;
                let verify_span = info_span!(
                    target: "nanocodex_computer",
                    parent: span,
                    "computer.postcondition.verify",
                    computer.action.sequence = sequence,
                    computer.observation.generation = generation,
                    computer.accessibility.maximum_elements = maximum,
                    computer.accessibility.element_count = Empty,
                    computer.accessibility.update.added = Empty,
                    computer.accessibility.update.changed = Empty,
                    computer.accessibility.update.removed = Empty,
                    computer.screenshot.bytes = Empty,
                    duration_ns = Empty,
                    status = Empty,
                    otel.status_code = Empty,
                );
                let worker_parent = verify_span.clone();
                let verify_started = Instant::now();
                let observed = tokio::task::spawn_blocking(move || {
                    observation_from_visual(
                        sample,
                        generation,
                        sequence,
                        settled,
                        &root,
                        maximum,
                        &worker_parent,
                    )
                })
                .await
                .map_err(|error| ComputerError::Native {
                    message: format!("semantic observation worker panicked: {error}"),
                })?;
                finish_span(&verify_span, verify_started, &observed);
                let mut observed = observed?;
                let verified_signal = self.accessibility_signal();
                if matched && verified_signal.revision != signal.revision {
                    info!(
                        target: "nanocodex_computer",
                        candidate_revision = signal.revision,
                        verified_revision = verified_signal.revision,
                        "discarded postcondition invalidated by an Accessibility notification"
                    );
                    if let Some(screenshot) = &observed.screenshot
                        && let Err(error) = tokio::fs::remove_file(&screenshot.path).await
                        && error.kind() != std::io::ErrorKind::NotFound
                    {
                        tracing::warn!(path = %screenshot.path.display(), %error, "failed to prune invalidated computer screenshot");
                    }
                    previous = Some(signature);
                    previous_signal_revision = Some(verified_signal.revision);
                    continue;
                }
                self.authorize_observation_urls(&observed)?;
                self.apply_accessibility_revision(&mut observed);
                record_observation(&verify_span, &observed, None);
                drop(verify_span);
                self.generation = generation;
                self.target = Some(Target {
                    application: observed.application.clone(),
                    window: observed.window.clone(),
                    follow_key_window,
                });
                self.retain_screenshot(&observed).await;
                return Ok(observed);
            }
            previous = Some(signature);
            previous_signal_revision = Some(signal.revision);
            let wait_started = Instant::now();
            let notification_wake = self
                .wait_for_observation_change(signal.revision, deadline)
                .await?;
            wait_duration += wait_started.elapsed();
            if notification_wake {
                notification_wake_count = notification_wake_count.saturating_add(1);
            } else {
                poll_wake_count = poll_wake_count.saturating_add(1);
            }
        }
    }

    async fn retain_screenshot(&mut self, observed: &ComputerObservation) {
        if let Some(screenshot) = &observed.screenshot {
            self.screenshots.push_back(screenshot.path.clone());
        }
        while self.screenshots.len() > MAX_RETAINED_SCREENSHOTS {
            if let Some(path) = self.screenshots.pop_front()
                && let Err(error) = tokio::fs::remove_file(&path).await
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(path = %path.display(), %error, "failed to prune old computer screenshot");
            }
        }
    }

    fn apply_accessibility_revision(&mut self, observed: &mut ComputerObservation) {
        observed.accessibility_update = self.accessibility_revision.as_ref().and_then(|previous| {
            (previous.pid == observed.application.pid && previous.window_id == observed.window.id)
                .then(|| {
                    accessibility_update(
                        previous.generation,
                        &previous.elements,
                        &observed.elements,
                    )
                })
        });
        self.accessibility_revision = Some(AccessibilityRevision {
            generation: observed.generation,
            pid: observed.application.pid,
            window_id: observed.window.id,
            elements: observed.elements.clone(),
        });
    }

    async fn mutate(
        &mut self,
        action: NativeAction,
        sequence: u64,
        state: Arc<RunState>,
        frames: &mut FrameSink,
        pointers: &PointerSink,
    ) -> Result<ComputerOutput, ComputerError> {
        state.ensure_running()?;
        let target = self.target.clone().ok_or(ComputerError::NoTarget)?;
        let generation = self.generation;
        let maximum = self.maximum_elements;
        let allowed_url_origins = self.allowed_url_origins.clone();
        let protected_frontmost = tokio::task::spawn_blocking(frontmost_application_pid)
            .await
            .map_err(|error| ComputerError::Native {
                message: format!("frontmost application observer panicked: {error}"),
            })?;
        let action_kind = action.kind();
        let dispatch = action.dispatch();
        let initial_pointer_fallbacks = self.intervention_target.synthetic_pointer_fallback_count();
        let intervention_target = Arc::clone(&self.intervention_target);
        let pointers = pointers.clone();
        let span = info_span!(
            target: "nanocodex_computer",
            "computer.input.dispatch",
            computer.action.sequence = sequence,
            computer.input.kind = action_kind,
            computer.input.dispatch = dispatch,
            computer.target.pid = target.application.pid,
            computer.target.window_id = target.window.id,
            computer.input.synthetic_pointer_fallback_count = Empty,
            duration_ns = Empty,
            status = Empty,
            otel.status_code = Empty,
        );
        let started = Instant::now();
        let outcome = async move {
            tokio::task::spawn_blocking(move || {
                native_action(
                    &target,
                    generation,
                    maximum,
                    action,
                    allowed_url_origins.as_deref(),
                    &intervention_target,
                    &pointers,
                )
            })
            .await
            .map_err(|error| ComputerError::Native {
                message: format!("input worker panicked: {error}"),
            })?
        }
        .instrument(span.clone())
        .await;
        span.record(
            "computer.input.synthetic_pointer_fallback_count",
            self.intervention_target
                .synthetic_pointer_fallback_count()
                .saturating_sub(initial_pointer_fallbacks),
        );
        finish_span(&span, started, &outcome);
        if let Err(ComputerError::UrlDenied { url }) = &outcome {
            self.blocked_url = Some(url.clone());
        }
        outcome?;
        drop(span);
        Ok(ComputerOutput::State {
            state: self
                .settled_observation(sequence, state, frames, protected_frontmost)
                .await?,
        })
    }
}

#[async_trait]
impl Backend for MacosBackend {
    async fn execute(
        &mut self,
        action: ComputerAction,
        sequence: u64,
        state: Arc<RunState>,
        frames: &mut FrameSink,
        pointers: &PointerSink,
    ) -> Result<ComputerOutput, ComputerError> {
        if let Some(url) = &self.blocked_url {
            return Err(ComputerError::UrlDenied { url: url.clone() });
        }
        if tokio::task::spawn_blocking(screen_locked)
            .await
            .map_err(|error| ComputerError::Native {
                message: format!("lock-screen observer panicked: {error}"),
            })?
        {
            return Err(ComputerError::ScreenLocked);
        }
        match action {
            ComputerAction::ListApplications => {
                let span = info_span!(
                    target: "nanocodex_computer",
                    "computer.target.discover",
                    computer.application.count = Empty,
                    computer.window.count = Empty,
                    duration_ns = Empty,
                    status = Empty,
                    otel.status_code = Empty,
                );
                let started = Instant::now();
                let outcome = tokio::task::spawn_blocking(discover)
                    .await
                    .map_err(|error| ComputerError::Native {
                        message: format!("discovery worker panicked: {error}"),
                    })?;
                finish_span(&span, started, &outcome);
                let (mut applications, mut windows, native) = outcome?;
                applications.retain(|application| self.authorize_application(application).is_ok());
                let allowed_pids = applications
                    .iter()
                    .map(|application| application.pid)
                    .collect::<HashSet<_>>();
                windows.retain(|window| allowed_pids.contains(&window.pid));
                span.record("computer.application.count", applications.len());
                span.record("computer.window.count", windows.len());
                drop(native);
                Ok(ComputerOutput::Applications {
                    applications,
                    windows,
                })
            }
            ComputerAction::OpenApplication { bundle_id } => {
                if bundle_id.trim().is_empty() || bundle_id.starts_with('-') {
                    return Err(ComputerError::InvalidAction {
                        message: "bundle_id must be a non-empty exact identifier".to_owned(),
                    });
                }
                self.authorize_bundle_id(&bundle_id)?;
                let span = info_span!(
                    target: "nanocodex_computer",
                    "computer.application.open",
                    computer.application.bundle_id = bundle_id.as_str(),
                    computer.application.pid = Empty,
                    computer.application.accessibility.initial_elements = Empty,
                    computer.application.accessibility.final_elements = Empty,
                    computer.application.activated_for_accessibility = Empty,
                    computer.application.focus_restored = Empty,
                    duration_ns = Empty,
                    status = Empty,
                    otel.status_code = Empty,
                );
                let started = Instant::now();
                let previous_frontmost = tokio::task::spawn_blocking(frontmost_application_pid)
                    .await
                    .map_err(|error| ComputerError::Native {
                        message: format!("frontmost application observer panicked: {error}"),
                    })?;
                let prime_bundle_id = bundle_id.clone();
                let outcome = async {
                    tokio::process::Command::new("/usr/bin/open")
                        .arg("-g")
                        .arg("-b")
                        .arg(&bundle_id)
                        .status()
                        .await
                        .map_err(|error| ComputerError::Native {
                            message: format!("failed to launch {bundle_id}: {error}"),
                        })
                        .and_then(|status| {
                            status
                                .success()
                                .then_some(())
                                .ok_or_else(|| ComputerError::Native {
                                    message: format!(
                                        "LaunchServices rejected {bundle_id} with {status}"
                                    ),
                                })
                        })?;
                    wait_for_application(&bundle_id).await?;
                    tokio::task::spawn_blocking(move || {
                        prime_application_accessibility(&prime_bundle_id, previous_frontmost)
                    })
                    .await
                    .map_err(|error| ComputerError::Native {
                        message: format!("application accessibility primer panicked: {error}"),
                    })?
                }
                .instrument(span.clone())
                .await;
                finish_span(&span, started, &outcome);
                let priming = outcome?;
                span.record("computer.application.pid", priming.pid);
                span.record(
                    "computer.application.accessibility.initial_elements",
                    priming.initial_elements,
                );
                span.record(
                    "computer.application.accessibility.final_elements",
                    priming.final_elements,
                );
                span.record(
                    "computer.application.activated_for_accessibility",
                    priming.activated,
                );
                span.record(
                    "computer.application.focus_restored",
                    priming.focus_restored,
                );
                Ok(ComputerOutput::Opened { bundle_id })
            }
            ComputerAction::Attach {
                application,
                window_id,
            } => {
                let span = info_span!(
                    target: "nanocodex_computer",
                    "computer.target.attach",
                    computer.target.requested_window_id = window_id,
                    computer.target.pid = Empty,
                    computer.target.window_id = Empty,
                    computer.accessibility.notification.enabled = Empty,
                    duration_ns = Empty,
                    status = Empty,
                    otel.status_code = Empty,
                );
                let started = Instant::now();
                let outcome =
                    tokio::task::spawn_blocking(move || select_target(application, window_id))
                        .await
                        .map_err(|error| ComputerError::Native {
                            message: format!("attach worker panicked: {error}"),
                        })?;
                finish_span(&span, started, &outcome);
                let target = outcome?;
                self.authorize_application(&target.application)?;
                let target_pid = target.application.pid;
                let monitor = tokio::task::spawn_blocking(move || {
                    AccessibilityNotificationMonitor::spawn(target_pid)
                })
                .await
                .map_err(|error| ComputerError::Native {
                    message: format!("Accessibility monitor worker panicked: {error}"),
                })?;
                self.accessibility_monitor = match monitor {
                    Ok(monitor) => Some(monitor),
                    Err(_) => {
                        tracing::warn!(
                            target: "nanocodex_computer",
                            pid = target.application.pid,
                            "Accessibility notifications unavailable; retaining polling fallback"
                        );
                        None
                    }
                };
                span.record("computer.target.pid", target.application.pid);
                span.record("computer.target.window_id", target.window.id);
                span.record(
                    "computer.accessibility.notification.enabled",
                    self.accessibility_monitor.is_some(),
                );
                drop(span);
                self.intervention_target.set_pid(target.application.pid);
                self.target = Some(target);
                Ok(ComputerOutput::State {
                    state: self
                        .observe(sequence, true, true, ComputerFramePhase::Observed, frames)
                        .await?,
                })
            }
            ComputerAction::Observe { screenshot } => Ok(ComputerOutput::State {
                state: self
                    .observe(
                        sequence,
                        screenshot,
                        true,
                        ComputerFramePhase::Observed,
                        frames,
                    )
                    .await?,
            }),
            ComputerAction::Click { target, button } => {
                self.mutate(
                    NativeAction::Click { target, button },
                    sequence,
                    state,
                    frames,
                    pointers,
                )
                .await
            }
            ComputerAction::Drag {
                from,
                to,
                duration_ms,
            } => {
                if !(20..=10_000).contains(&duration_ms) {
                    return Err(ComputerError::InvalidAction {
                        message: "drag duration_ms must be between 20 and 10000".to_owned(),
                    });
                }
                self.mutate(
                    NativeAction::Drag {
                        from,
                        to,
                        duration_ms,
                    },
                    sequence,
                    state,
                    frames,
                    pointers,
                )
                .await
            }
            ComputerAction::Scroll {
                delta_x,
                delta_y,
                at,
            } => {
                self.mutate(
                    NativeAction::Scroll {
                        delta_x,
                        delta_y,
                        at,
                    },
                    sequence,
                    state,
                    frames,
                    pointers,
                )
                .await
            }
            ComputerAction::PressKey { key, modifiers } => {
                self.mutate(
                    NativeAction::PressKey { key, modifiers },
                    sequence,
                    state,
                    frames,
                    pointers,
                )
                .await
            }
            ComputerAction::TypeText { text } => {
                if text.chars().count() > 100_000 {
                    return Err(ComputerError::InvalidAction {
                        message: "type_text is limited to 100000 Unicode scalar values".to_owned(),
                    });
                }
                self.mutate(
                    NativeAction::TypeText { text },
                    sequence,
                    state,
                    frames,
                    pointers,
                )
                .await
            }
            ComputerAction::SetValue { reference, value } => {
                self.mutate(
                    NativeAction::SetValue { reference, value },
                    sequence,
                    state,
                    frames,
                    pointers,
                )
                .await
            }
            ComputerAction::PerformAction { reference, name } => {
                self.mutate(
                    NativeAction::PerformAction { reference, name },
                    sequence,
                    state,
                    frames,
                    pointers,
                )
                .await
            }
            ComputerAction::Wait { milliseconds } => {
                if milliseconds > 60_000 {
                    return Err(ComputerError::InvalidAction {
                        message: "one wait is limited to 60000ms".to_owned(),
                    });
                }
                let span = info_span!(
                    target: "nanocodex_computer",
                    "computer.wait",
                    computer.wait.requested_ms = milliseconds,
                    duration_ns = Empty,
                    status = Empty,
                    otel.status_code = Empty,
                );
                let started = Instant::now();
                let outcome =
                    async {
                        let deadline =
                            tokio::time::Instant::now() + Duration::from_millis(milliseconds);
                        while tokio::time::Instant::now() < deadline {
                            state.ensure_running()?;
                            tokio::time::sleep(Duration::from_millis(50).min(
                                deadline.saturating_duration_since(tokio::time::Instant::now()),
                            ))
                            .await;
                        }
                        Ok::<(), ComputerError>(())
                    }
                    .instrument(span.clone())
                    .await;
                finish_span(&span, started, &outcome);
                outcome?;
                drop(span);
                if self.target.is_some() {
                    Ok(ComputerOutput::State {
                        state: self
                            .observe(sequence, true, true, ComputerFramePhase::Observed, frames)
                            .await?,
                    })
                } else {
                    Ok(ComputerOutput::Done)
                }
            }
        }
    }
}

async fn wait_for_application(bundle_id: &str) -> Result<(), ComputerError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let requested_bundle_id = bundle_id.to_owned();
        let ready = tokio::task::spawn_blocking(move || {
            nanocodex_computer_macos::windows()
                .into_iter()
                .any(|window| {
                    window.bundle_id.as_deref() == Some(requested_bundle_id.as_str())
                        && normal_window_candidate(&window)
                })
        })
        .await
        .map_err(|error| ComputerError::Native {
            message: format!("application launch observer panicked: {error}"),
        })?;
        if ready {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ComputerError::TargetNotFound {
                message: format!("{bundle_id} launched but exposed no capturable window within 5s"),
            });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn prime_application_accessibility(
    bundle_id: &str,
    previous_frontmost: Option<i32>,
) -> Result<ApplicationPriming, ComputerError> {
    let target = select_target(ApplicationSelector::BundleId(bundle_id.to_owned()), None)?;
    let pid = target.application.pid;
    let observation = (|| {
        let initial_elements = accessibility_element_count(&target, ACCESSIBILITY_PRIME_ELEMENTS)?;
        let mut final_elements = initial_elements;
        let needs_activation = initial_elements <= ACCESSIBILITY_PRIME_THRESHOLD;
        let activated = needs_activation
            && frontmost_application_pid() != Some(pid)
            && activate_application(pid);
        if needs_activation {
            let deadline = Instant::now() + ACCESSIBILITY_PRIME_TIMEOUT;
            while final_elements <= ACCESSIBILITY_PRIME_THRESHOLD && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
                final_elements =
                    accessibility_element_count(&target, ACCESSIBILITY_PRIME_ELEMENTS)?;
            }
        }
        Ok::<_, ComputerError>((initial_elements, final_elements, activated))
    })();
    let activation_requested = observation
        .as_ref()
        .is_ok_and(|(_, _, activated)| *activated);
    let focus_restored = previous_frontmost.is_some_and(|previous| {
        previous != pid && restore_frontmost_application(previous, pid, activation_requested)
    });
    let (initial_elements, final_elements, activated) = observation?;
    Ok(ApplicationPriming {
        pid,
        initial_elements,
        final_elements,
        activated,
        focus_restored,
    })
}

fn restore_frontmost_application(previous: i32, target: i32, activation_requested: bool) -> bool {
    if activation_requested {
        wait_for_frontmost_application(target, APPLICATION_ACTIVATION_TIMEOUT);
    }
    frontmost_application_pid() == Some(previous)
        || activate_application(previous)
            && wait_for_frontmost_application(previous, APPLICATION_ACTIVATION_TIMEOUT)
}

fn wait_for_frontmost_application(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while frontmost_application_pid() != Some(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    frontmost_application_pid() == Some(pid)
}

enum NativeAction {
    Click {
        target: InteractionTarget,
        button: MouseButton,
    },
    Drag {
        from: InteractionTarget,
        to: InteractionTarget,
        duration_ms: u64,
    },
    Scroll {
        delta_x: i32,
        delta_y: i32,
        at: Option<InteractionTarget>,
    },
    PressKey {
        key: String,
        modifiers: Vec<KeyModifier>,
    },
    TypeText {
        text: String,
    },
    SetValue {
        reference: ElementRef,
        value: String,
    },
    PerformAction {
        reference: ElementRef,
        name: String,
    },
}

impl NativeAction {
    const fn kind(&self) -> &'static str {
        match self {
            Self::Click { .. } => "click",
            Self::Drag { .. } => "drag",
            Self::Scroll { .. } => "scroll",
            Self::PressKey { .. } => "press_key",
            Self::TypeText { .. } => "type_text",
            Self::SetValue { .. } => "set_value",
            Self::PerformAction { .. } => "perform_action",
        }
    }

    const fn dispatch(&self) -> &'static str {
        match self {
            Self::SetValue { .. } => "accessibility_value",
            Self::PerformAction { .. } => "accessibility_action",
            Self::Click {
                target: InteractionTarget::Element(_),
                ..
            } => "accessibility_press_or_cg_event",
            Self::Click { .. }
            | Self::Drag { .. }
            | Self::Scroll { .. }
            | Self::PressKey { .. }
            | Self::TypeText { .. } => "cg_event",
        }
    }
}

fn discover() -> Result<Discovery, ComputerError> {
    let native_windows = nanocodex_computer_macos::windows();
    let mut applications = native_windows
        .iter()
        .filter(|window| window.pid > 0 && !window.owner_name.is_empty())
        .map(|window| Application {
            pid: window.pid,
            name: window.owner_name.clone(),
            bundle_id: window.bundle_id.clone(),
        })
        .collect::<Vec<_>>();
    applications.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then(left.pid.cmp(&right.pid))
    });
    applications.dedup_by_key(|app| app.pid);

    let mut windows = native_windows.iter().map(public_window).collect::<Vec<_>>();
    windows.sort_by_key(|window| (!window.on_screen, window.pid, window.id));
    Ok((applications, windows, native_windows))
}

fn select_target(
    selector: ApplicationSelector,
    window_id: Option<u32>,
) -> Result<Target, ComputerError> {
    let follow_key_window = window_id.is_none();
    let (applications, _, native_windows) = discover()?;
    let application = applications
        .into_iter()
        .find(|application| match &selector {
            ApplicationSelector::Pid(pid) => application.pid == *pid,
            ApplicationSelector::BundleId(bundle) => {
                application.bundle_id.as_deref() == Some(bundle.as_str())
            }
            ApplicationSelector::Name(name) => application.name.eq_ignore_ascii_case(name),
        })
        .ok_or_else(|| ComputerError::TargetNotFound {
            message: format!("no running application matches {selector:?}"),
        })?;
    let matching_windows = native_windows
        .iter()
        .filter(|window| window.pid == application.pid)
        .filter(|window| window_id.is_none_or(|id| window.id == id));
    let native_window = if window_id.is_some() {
        matching_windows.into_iter().next()
    } else {
        matching_windows
            .clone()
            .find(|window| primary_window_candidate(window))
            .or_else(|| {
                matching_windows
                    .clone()
                    .find(|window| normal_window_candidate(window))
            })
            .or_else(|| matching_windows.clone().find(|window| window.on_screen))
    }
    .ok_or_else(|| ComputerError::TargetNotFound {
        message: format!(
            "application {} has no matching capturable window",
            application.name
        ),
    })?;
    Ok(Target {
        application,
        window: public_window(native_window),
        follow_key_window,
    })
}

fn primary_window_candidate(window: &NativeWindow) -> bool {
    normal_window_candidate(window) && window.title.as_ref().is_some_and(|title| !title.is_empty())
}

fn normal_window_candidate(window: &NativeWindow) -> bool {
    window.on_screen && window.width >= 64.0 && window.height >= 64.0
}

fn public_window(window: &NativeWindow) -> Window {
    Window {
        id: window.id,
        pid: window.pid,
        title: window.title.clone(),
        frame: Rect {
            x: window.x,
            y: window.y,
            width: window.width,
            height: window.height,
        },
        on_screen: window.on_screen,
    }
}

fn refresh_target_window(target: Target) -> Result<Target, ComputerError> {
    let native_windows = nanocodex_computer_macos::windows()
        .into_iter()
        .filter(|window| window.pid == target.application.pid)
        .collect::<Vec<_>>();
    if !target.follow_key_window {
        let native = native_windows
            .iter()
            .find(|window| window.id == target.window.id)
            .ok_or_else(|| ComputerError::TargetNotFound {
                message: format!("window {} is no longer capturable", target.window.id),
            })?;
        return Ok(target.with_native_window(native));
    }

    let application = AXUIElement::application(target.application.pid);
    let _ = application.set_messaging_timeout(0.5);
    let mut accessibility_windows = Vec::with_capacity(2);
    if let Ok(window) = application.focused_window()
        && accessibility_window_candidate(&window)
    {
        accessibility_windows.push(window);
    }
    if let Ok(window) = application.main_window()
        && accessibility_window_candidate(&window)
        && !accessibility_windows
            .iter()
            .any(|candidate| candidate == &window)
    {
        accessibility_windows.push(window);
    }
    for accessibility_window in accessibility_windows {
        let Some((x, y, width, height)) = element_rect(&accessibility_window) else {
            continue;
        };
        let accessibility_frame = Rect {
            x,
            y,
            width,
            height,
        };
        if let Some(native) = native_windows
            .iter()
            .filter(|window| normal_window_candidate(window))
            .filter_map(|window| {
                let frame = public_window(window).frame;
                matching_window_frames(accessibility_frame, frame)
                    .then_some((window, window_frame_distance(accessibility_frame, frame)))
            })
            .min_by(|(_, left), (_, right)| left.total_cmp(right))
            .map(|(window, _)| window)
        {
            if native.id != target.window.id {
                info!(
                    target: "nanocodex_computer",
                    pid = target.application.pid,
                    previous_window_id = target.window.id,
                    window_id = native.id,
                    "followed the application's focused accessibility window"
                );
            }
            return Ok(target.with_native_window(native));
        }
    }

    if let Some(native) = native_windows
        .iter()
        .find(|window| window.id == target.window.id)
    {
        return Ok(target.with_native_window(native));
    }
    let native = native_windows
        .iter()
        .find(|window| primary_window_candidate(window))
        .or_else(|| {
            native_windows
                .iter()
                .find(|window| normal_window_candidate(window))
        })
        .ok_or_else(|| ComputerError::TargetNotFound {
            message: format!(
                "application {} has no remaining capturable window",
                target.application.name
            ),
        })?;
    Ok(target.with_native_window(native))
}

fn accessibility_loading(target: &Target) -> Result<bool, ComputerError> {
    if request_accessibility() == PermissionRequest::Prompted {
        return Err(ComputerError::Permission {
            permission: Permission::Accessibility,
            guidance: permission_guidance(Permission::Accessibility),
        });
    }
    let application = AXUIElement::application(target.application.pid);
    application
        .set_messaging_timeout(0.5)
        .map_err(|error| ComputerError::Native {
            message: format!("failed to configure accessibility timeout: {error}"),
        })?;
    let Some((root, _)) = select_accessibility_window(&application, target) else {
        return Ok(false);
    };
    let mut elements = Vec::with_capacity(64);
    walk_accessibility(&root, 0, 256, &mut elements);
    Ok(elements.iter().any(|element| {
        element.element_busy().ok().is_some_and(bool::from)
            || string_attribute(element.role()).as_deref() == Some("AXBusyIndicator")
    }))
}

fn suppress_focus_steal(previous_frontmost: i32, target_pid: i32) -> Option<bool> {
    if frontmost_application_pid() != Some(target_pid) {
        return None;
    }
    let restored = restore_frontmost_application(previous_frontmost, target_pid, false);
    info!(
        target: "nanocodex_computer",
        previous_frontmost,
        target_pid,
        restored,
        "suppressed target application focus steal"
    );
    Some(restored)
}

fn observe_target(
    request: ObservationRequest,
    parent: &Span,
) -> Result<(ComputerObservation, Option<Arc<CapturedImage>>), ComputerError> {
    let ObservationRequest {
        target,
        generation,
        sequence,
        screenshot: include_screenshot,
        settled,
        artifact_root,
        maximum_elements,
    } = request;
    let target = refresh_target_window(target)?;
    let native_window =
        native_window_by_id(target.window.id).ok_or_else(|| ComputerError::TargetNotFound {
            message: format!("window {} is no longer capturable", target.window.id),
        })?;
    let (elements, image) = std::thread::scope(|scope| {
        let capture = include_screenshot.then(|| {
            let capture_parent = parent.clone();
            let native_window = &native_window;
            scope.spawn(move || capture_image(native_window, &capture_parent, None))
        });
        let elements = accessibility_snapshot(&target, generation, maximum_elements, parent)
            .map(|elements| elements.into_iter().map(|(element, _)| element).collect());
        let image = capture.map(|capture| {
            capture.join().map_err(|_| ComputerError::Native {
                message: "screenshot worker panicked".to_owned(),
            })?
        });
        (elements, image.transpose())
    });
    let elements = elements?;
    let image = image?;
    let screenshot = image
        .as_deref()
        .map(|image| persist_image(image, generation, sequence, &artifact_root, parent))
        .transpose()?;
    Ok((
        ComputerObservation {
            generation,
            application: target.application,
            window: target.window,
            elements,
            accessibility_update: None,
            screenshot,
            settled,
        },
        image,
    ))
}

fn visual_sample(
    target: Target,
    parent: &Span,
    sample_index: u64,
) -> Result<VisualSample, ComputerError> {
    if screen_locked() {
        return Err(ComputerError::ScreenLocked);
    }
    let target = refresh_target_window(target)?;
    let native_window =
        native_window_by_id(target.window.id).ok_or_else(|| ComputerError::TargetNotFound {
            message: format!("window {} is no longer capturable", target.window.id),
        })?;
    let loading = accessibility_loading(&target)?;
    Ok(VisualSample {
        application: target.application,
        window: public_window(&native_window),
        image: capture_image(&native_window, parent, Some(sample_index))?,
        loading,
        follow_key_window: target.follow_key_window,
    })
}

fn observation_from_visual(
    sample: VisualSample,
    generation: u64,
    sequence: u64,
    settled: bool,
    artifact_root: &Path,
    maximum_elements: usize,
    parent: &Span,
) -> Result<ComputerObservation, ComputerError> {
    let target = Target {
        application: sample.application,
        window: sample.window,
        follow_key_window: sample.follow_key_window,
    };
    let elements = accessibility_snapshot(&target, generation, maximum_elements, parent)?
        .into_iter()
        .map(|(element, _)| element)
        .collect();
    let screenshot = persist_image(&sample.image, generation, sequence, artifact_root, parent)?;
    Ok(ComputerObservation {
        generation,
        application: target.application,
        window: target.window,
        elements,
        accessibility_update: None,
        screenshot: Some(screenshot),
        settled,
    })
}

fn accessibility_snapshot(
    target: &Target,
    generation: u64,
    maximum_elements: usize,
    parent: &Span,
) -> Result<Vec<(Element, AXUIElement)>, ComputerError> {
    let span = info_span!(
        target: "nanocodex_computer",
        parent: parent,
        "computer.accessibility.snapshot",
        computer.observation.generation = generation,
        computer.accessibility.maximum_elements = maximum_elements,
        computer.accessibility.visited_count = Empty,
        computer.accessibility.element_count = Empty,
        computer.accessibility.truncated = Empty,
        duration_ns = Empty,
        status = Empty,
        otel.status_code = Empty,
    );
    let started = Instant::now();
    let raw = match span.in_scope(|| accessibility_elements(target, maximum_elements)) {
        Ok(raw) => raw,
        Err(error) => {
            finish_span::<(), _>(&span, started, &Err(&error));
            return Err(error);
        }
    };
    span.record("computer.accessibility.visited_count", raw.len());
    span.record(
        "computer.accessibility.truncated",
        raw.len() == maximum_elements,
    );
    let mut output = Vec::with_capacity(raw.len());
    for (raw_index, element) in raw.into_iter().enumerate() {
        let mut public = public_element(&element, generation, raw_index);
        if should_include(&public) {
            public.reference = reference_for(generation, raw_index, &public);
            output.push((public, element));
        }
    }
    span.record("computer.accessibility.element_count", output.len());
    finish_span::<(), ComputerError>(&span, started, &Ok(()));
    Ok(output)
}

fn accessibility_elements(
    target: &Target,
    maximum_elements: usize,
) -> Result<Vec<AXUIElement>, ComputerError> {
    if request_accessibility() == PermissionRequest::Prompted {
        return Err(ComputerError::Permission {
            permission: Permission::Accessibility,
            guidance: permission_guidance(Permission::Accessibility),
        });
    }
    let application = AXUIElement::application(target.application.pid);
    application
        .set_messaging_timeout(2.0)
        .map_err(|error| ComputerError::Native {
            message: format!("failed to configure accessibility timeout: {error}"),
        })?;
    enable_application_accessibility(&application);
    let (root, match_strategy) =
        select_accessibility_window(&application, target).ok_or_else(|| {
            ComputerError::TargetNotFound {
                message: format!(
                    "window {} is capturable but has no matching accessibility window",
                    target.window.id
                ),
            }
        })?;
    info!(
        target: "nanocodex_computer",
        window_id = target.window.id,
        match_strategy,
        "matched the attached capture window to its accessibility window"
    );
    let mut raw = Vec::with_capacity(maximum_elements.min(256));
    walk_accessibility(&root, 0, maximum_elements, &mut raw);
    Ok(raw)
}

fn accessibility_element_count(
    target: &Target,
    maximum_elements: usize,
) -> Result<usize, ComputerError> {
    match accessibility_elements(target, maximum_elements) {
        Ok(elements) => Ok(elements.len()),
        Err(ComputerError::TargetNotFound { .. }) => Ok(0),
        Err(error) => Err(error),
    }
}

fn enable_application_accessibility(application: &AXUIElement) {
    let span = info_span!(
        target: "nanocodex_computer",
        "computer.accessibility.enable",
        computer.accessibility.manual = Empty,
        computer.accessibility.enhanced = Empty,
    );
    let enablement = enable_native_application_accessibility(application);
    span.record("computer.accessibility.manual", enablement.manual);
    span.record("computer.accessibility.enhanced", enablement.enhanced);
}

fn select_accessibility_window(
    application: &AXUIElement,
    target: &Target,
) -> Option<(AXUIElement, &'static str)> {
    let windows = application.windows().ok()?;
    let mut candidates = windows
        .iter()
        .filter(|window| accessibility_window_candidate(window))
        .map(|window| {
            let window = (*window).clone();
            let title = string_attribute(window.title());
            let frame = element_rect(&window).map(|(x, y, width, height)| Rect {
                x,
                y,
                width,
                height,
            });
            (window, title, frame)
        })
        .collect::<Vec<_>>();
    if let Some((window, _, _)) = candidates
        .iter()
        .filter_map(|candidate @ (_, _, frame)| {
            let frame =
                frame.filter(|frame| matching_window_frames(*frame, target.window.frame))?;
            Some((candidate, window_frame_distance(frame, target.window.frame)))
        })
        .min_by(|(_, left), (_, right)| left.total_cmp(right))
        .map(|(candidate, _)| candidate)
    {
        return Some((window.clone(), "frame"));
    }

    if let Some(title) = target.window.title.as_deref() {
        let mut matching_titles = candidates
            .iter()
            .filter(|(_, candidate, _)| candidate.as_deref() == Some(title));
        let matching = matching_titles.next();
        if matching.is_some() && matching_titles.next().is_none() {
            return matching.map(|(window, _, _)| (window.clone(), "unique_title"));
        }
    }

    (candidates.len() == 1).then(|| (candidates.swap_remove(0).0, "only_window"))
}

fn accessibility_window_candidate(element: &AXUIElement) -> bool {
    string_attribute(element.role()).as_deref() == Some("AXWindow")
}

fn matching_window_frames(left: Rect, right: Rect) -> bool {
    const EDGE_TOLERANCE_POINTS: f64 = 8.0;
    (left.x - right.x).abs() <= EDGE_TOLERANCE_POINTS
        && (left.y - right.y).abs() <= EDGE_TOLERANCE_POINTS
        && (left.width - right.width).abs() <= EDGE_TOLERANCE_POINTS
        && (left.height - right.height).abs() <= EDGE_TOLERANCE_POINTS
}

fn window_frame_distance(left: Rect, right: Rect) -> f64 {
    (left.x - right.x).abs()
        + (left.y - right.y).abs()
        + (left.width - right.width).abs()
        + (left.height - right.height).abs()
}

fn walk_accessibility(
    element: &AXUIElement,
    depth: usize,
    maximum: usize,
    output: &mut Vec<AXUIElement>,
) {
    if depth > MAX_TREE_DEPTH
        || output.len() >= maximum
        || output.iter().any(|candidate| candidate == element)
    {
        return;
    }
    output.push(element.clone());
    if let Ok(children) = element.children() {
        for child in children.iter() {
            walk_accessibility(&child, depth + 1, maximum, output);
            if output.len() >= maximum {
                break;
            }
        }
    }
}

fn public_element(element: &AXUIElement, generation: u64, index: usize) -> Element {
    let label = string_attribute(element.title())
        .or_else(|| string_attribute(element.description()))
        .or_else(|| string_attribute(element.help()));
    let actions = element
        .action_names()
        .map(|actions| actions.iter().map(|action| action.to_string()).collect())
        .unwrap_or_default();
    let mut public = Element {
        reference: ElementRef(format!("e{generation}_{index}")),
        role: string_attribute(element.role()).unwrap_or_else(|| "AXUnknown".to_owned()),
        subrole: string_attribute(element.subrole()),
        label,
        value: scalar_value(element),
        selected_text: selected_text(element),
        placeholder: string_attribute(element.placeholder_value()),
        identifier: string_attribute(element.identifier()),
        url: url_attribute(element),
        frame: None,
        enabled: None,
        focused: None,
        actions,
    };
    if should_include(&public) {
        public.frame = element_rect(element).map(|(x, y, width, height)| Rect {
            x,
            y,
            width,
            height,
        });
        public.enabled = element.enabled().ok().map(bool::from);
        public.focused = element.focused().ok().map(bool::from);
    }
    public
}

fn should_include(element: &Element) -> bool {
    !element.actions.is_empty()
        || element.label.is_some()
        || element.value.is_some()
        || element.selected_text.is_some()
        || element.placeholder.is_some()
        || element.identifier.is_some()
        || element.url.is_some()
        || matches!(
            element.role.as_str(),
            "AXTextField" | "AXTextArea" | "AXWebArea" | "AXLink" | "AXButton" | "AXMenuItem"
        )
}

fn scalar_value(element: &AXUIElement) -> Option<String> {
    let value = element.value().ok()?;
    if !value.instance_of::<CFString>() {
        return None;
    }
    let value = value.downcast::<CFString>()?.to_string();
    Some(truncate(value, MAX_TEXT_CHARS))
}

fn selected_text(element: &AXUIElement) -> Option<String> {
    let attribute =
        AXAttribute::<core_foundation::base::CFType>::new(&CFString::new("AXSelectedText"));
    let value = element.attribute(&attribute).ok()?;
    let value = value.downcast::<CFString>()?.to_string();
    (!value.is_empty()).then(|| truncate(value, MAX_TEXT_CHARS))
}

fn string_attribute(value: Result<CFString, accessibility::Error>) -> Option<String> {
    value
        .ok()
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
        .map(|value| truncate(value, MAX_TEXT_CHARS))
}

fn url_attribute(element: &AXUIElement) -> Option<String> {
    let attribute = AXAttribute::<core_foundation::base::CFType>::new(&CFString::new("AXURL"));
    let value = element.attribute(&attribute).ok()?;
    if value.instance_of::<CFURL>() {
        return value
            .downcast::<CFURL>()
            .map(|url| truncate(url.get_string().to_string(), MAX_TEXT_CHARS));
    }
    value
        .downcast::<CFString>()
        .map(|url| truncate(url.to_string(), MAX_TEXT_CHARS))
}

fn truncate(value: String, maximum: usize) -> String {
    if value.chars().count() <= maximum {
        return value;
    }
    let mut value = value.chars().take(maximum).collect::<String>();
    value.push('…');
    value
}

fn reference_for(generation: u64, index: usize, element: &Element) -> ElementRef {
    let hash = element_hash(element);
    ElementRef(format!("e{generation}_{index}_{hash:016x}"))
}

fn element_hash(element: &Element) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in format!(
        "{}\0{:?}\0{:?}\0{:?}\0{:?}\0{:?}\0{:?}",
        element.role,
        element.identifier,
        element.label,
        element.value,
        element.selected_text,
        element.url,
        element.frame
    )
    .bytes()
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn accessibility_update(
    base_generation: u64,
    previous: &[Element],
    current: &[Element],
) -> AccessibilityUpdate {
    let mut previous_by_identity = HashMap::<String, Vec<usize>>::new();
    let mut current_by_identity = HashMap::<String, Vec<usize>>::new();
    for (index, element) in previous.iter().enumerate() {
        previous_by_identity
            .entry(element_identity(element))
            .or_default()
            .push(index);
    }
    for (index, element) in current.iter().enumerate() {
        current_by_identity
            .entry(element_identity(element))
            .or_default()
            .push(index);
    }

    let mut matched_previous = HashSet::new();
    let mut matched_current = HashSet::new();
    let mut changed = Vec::new();
    for (identity, current_indices) in &current_by_identity {
        let Some(previous_indices) = previous_by_identity.get(identity) else {
            continue;
        };
        if previous_indices.len() != 1 || current_indices.len() != 1 {
            continue;
        }
        let previous_index = previous_indices[0];
        let current_index = current_indices[0];
        matched_previous.insert(previous_index);
        matched_current.insert(current_index);
        if !same_element_state(&previous[previous_index], &current[current_index]) {
            changed.push(current[current_index].clone());
        }
    }

    AccessibilityUpdate {
        base_generation,
        added: current
            .iter()
            .enumerate()
            .filter(|(index, _)| !matched_current.contains(index))
            .map(|(_, element)| element.clone())
            .collect(),
        changed,
        removed: previous
            .iter()
            .enumerate()
            .filter(|(index, _)| !matched_previous.contains(index))
            .map(|(_, element)| element.reference.clone())
            .collect(),
    }
}

fn element_identity(element: &Element) -> String {
    let mut identity = format!(
        "{}\0{:?}\0{:?}\0{:?}\0{:?}",
        element.role, element.subrole, element.identifier, element.label, element.placeholder
    );
    if element.identifier.is_none() && element.label.is_none() && element.placeholder.is_none() {
        identity.push_str(&format!("\0{:?}", element.frame));
    }
    identity
}

fn same_element_state(left: &Element, right: &Element) -> bool {
    left.role == right.role
        && left.subrole == right.subrole
        && left.label == right.label
        && left.value == right.value
        && left.selected_text == right.selected_text
        && left.placeholder == right.placeholder
        && left.identifier == right.identifier
        && left.url == right.url
        && left.frame == right.frame
        && left.enabled == right.enabled
        && left.focused == right.focused
        && left.actions == right.actions
}

fn resolve_element(
    target: &Target,
    generation: u64,
    maximum: usize,
    reference: &ElementRef,
) -> Result<(Element, AXUIElement), ComputerError> {
    let (raw_index, expected_hash) =
        reference_parts(reference, generation).ok_or_else(|| ComputerError::StaleReference {
            reference: reference.0.clone(),
        })?;
    let elements = accessibility_elements(target, maximum)?;
    if let Some(element) = elements.get(raw_index) {
        let mut public = public_element(element, generation, raw_index);
        if should_include(&public) && element_hash(&public) == expected_hash {
            public.reference = reference.clone();
            return Ok((public, element.clone()));
        }
    }

    let mut relocated = None;
    for (current_index, element) in elements.into_iter().enumerate() {
        if current_index == raw_index {
            continue;
        }
        let mut public = public_element(&element, generation, current_index);
        if !should_include(&public) || element_hash(&public) != expected_hash {
            continue;
        }
        if relocated.is_some() {
            return Err(ComputerError::StaleReference {
                reference: reference.0.clone(),
            });
        }
        public.reference = reference.clone();
        relocated = Some((current_index, public, element));
    }
    let (relocated_index, public, element) =
        relocated.ok_or_else(|| ComputerError::StaleReference {
            reference: reference.0.clone(),
        })?;
    info!(
        target: "nanocodex_computer",
        generation,
        hinted_index = raw_index,
        relocated_index,
        "relocated a generation-valid accessibility reference after tree reordering"
    );
    Ok((public, element))
}

fn reference_parts(reference: &ElementRef, generation: u64) -> Option<(usize, u64)> {
    let remainder = reference.0.strip_prefix(&format!("e{generation}_"))?;
    let (index, hash) = remainder.split_once('_')?;
    let index = index.parse().ok()?;
    let hash = (hash.len() == 16)
        .then(|| u64::from_str_radix(hash, 16).ok())
        .flatten()?;
    Some((index, hash))
}

fn resolve_point(
    target: &Target,
    generation: u64,
    maximum: usize,
    interaction: InteractionTarget,
) -> Result<Point, ComputerError> {
    match interaction {
        InteractionTarget::Point(point) => validate_point(point),
        InteractionTarget::Element(reference) => {
            resolve_element(target, generation, maximum, &reference)?
                .0
                .frame
                .map(Rect::center)
                .ok_or_else(|| ComputerError::InvalidAction {
                    message: format!("element {reference} has no global bounds"),
                })
        }
    }
}

fn validate_point(point: Point) -> Result<Point, ComputerError> {
    if point.x.is_finite() && point.y.is_finite() {
        Ok(point)
    } else {
        Err(ComputerError::InvalidAction {
            message: "coordinates must be finite".to_owned(),
        })
    }
}

fn native_action(
    target: &Target,
    generation: u64,
    maximum: usize,
    action: NativeAction,
    allowed_url_origins: Option<&[Url]>,
    intervention_target: &super::InterventionTarget,
    pointers: &PointerSink,
) -> Result<(), ComputerError> {
    if screen_locked() {
        return Err(ComputerError::ScreenLocked);
    }
    let pid = target.application.pid;
    let input = NativeInput {
        pid,
        intervention_target,
        pointers,
    };
    match action {
        NativeAction::Click {
            target: InteractionTarget::Element(reference),
            button: MouseButton::Left,
        } => {
            let (public, element) = resolve_element(target, generation, maximum, &reference)?;
            authorize_element_url(&public, allowed_url_origins)?;
            if public.actions.iter().any(|action| action == "AXPress") {
                let point = public.frame.map(Rect::center);
                if let Some(point) = point {
                    pointers.publish(point, true);
                }
                let result = element
                    .perform_action(&CFString::new("AXPress"))
                    .map_err(|error| ComputerError::Native {
                        message: format!("AXPress failed for {reference}: {error}"),
                    });
                if let Some(point) = point {
                    pointers.publish(point, false);
                }
                return result;
            }
            post_click(
                &input,
                public
                    .frame
                    .map(Rect::center)
                    .ok_or_else(|| ComputerError::InvalidAction {
                        message: format!("element {reference} is neither pressable nor bounded"),
                    })?,
                MouseButton::Left,
            )
        }
        NativeAction::Click {
            target: point,
            button,
        } => post_click(
            &input,
            resolve_point(target, generation, maximum, point)?,
            button,
        ),
        NativeAction::Drag {
            from,
            to,
            duration_ms,
        } => post_drag(
            &input,
            resolve_point(target, generation, maximum, from)?,
            resolve_point(target, generation, maximum, to)?,
            duration_ms,
        ),
        NativeAction::Scroll {
            delta_x,
            delta_y,
            at,
        } => {
            if let Some(at) = at {
                post_move(&input, resolve_point(target, generation, maximum, at)?)?;
            }
            let source = event_source()?;
            let event =
                CGEvent::new_scroll_event(source, ScrollEventUnit::PIXEL, 2, delta_y, delta_x, 0)
                    .map_err(|()| native_event_error("scroll"))?;
            input.post(&event);
            Ok(())
        }
        NativeAction::PressKey { key, modifiers } => post_key(&input, &key, &modifiers),
        NativeAction::TypeText { text } => post_text(&input, &text),
        NativeAction::SetValue { reference, value } => {
            let (_, element) = resolve_element(target, generation, maximum, &reference)?;
            let attribute =
                AXAttribute::<core_foundation::base::CFType>::new(&CFString::new("AXValue"));
            if !element.is_settable(&attribute).unwrap_or(false) {
                return Err(ComputerError::InvalidAction {
                    message: format!("element {reference} does not expose a settable AXValue"),
                });
            }
            element
                .set_attribute(&attribute, CFString::new(&value).into_CFType())
                .map_err(|error| ComputerError::Native {
                    message: format!("setting AXValue for {reference} failed: {error}"),
                })
        }
        NativeAction::PerformAction { reference, name } => {
            if !name.starts_with("AX") || name.len() > 128 {
                return Err(ComputerError::InvalidAction {
                    message:
                        "accessibility action names must start with AX and be at most 128 bytes"
                            .to_owned(),
                });
            }
            let (public, element) = resolve_element(target, generation, maximum, &reference)?;
            authorize_element_url(&public, allowed_url_origins)?;
            if !public.actions.iter().any(|action| action == &name) {
                return Err(ComputerError::InvalidAction {
                    message: format!("element {reference} does not advertise {name}"),
                });
            }
            let point = (name == "AXPress")
                .then(|| public.frame.map(Rect::center))
                .flatten();
            if let Some(point) = point {
                pointers.publish(point, true);
            }
            let result = element
                .perform_action(&CFString::new(&name))
                .map_err(|error| ComputerError::Native {
                    message: format!("{name} failed for {reference}: {error}"),
                });
            if let Some(point) = point {
                pointers.publish(point, false);
            }
            result
        }
    }
}

fn authorize_element_url(
    element: &Element,
    allowed_url_origins: Option<&[Url]>,
) -> Result<(), ComputerError> {
    let Some(raw) = element.url.as_deref() else {
        return Ok(());
    };
    enforce_url_policy(raw, allowed_url_origins)
}

fn enforce_url_policy(raw: &str, allowed_url_origins: Option<&[Url]>) -> Result<(), ComputerError> {
    let Some(allowed) = allowed_url_origins else {
        return Ok(());
    };
    let parsed = Url::parse(raw).map_err(|_| ComputerError::UrlDenied {
        url: raw.to_owned(),
    })?;
    if matches!(parsed.scheme(), "http" | "https")
        && allowed
            .iter()
            .any(|allowed| allowed.origin() == parsed.origin())
    {
        return Ok(());
    }
    Err(ComputerError::UrlDenied {
        url: raw.to_owned(),
    })
}

fn event_source() -> Result<CGEventSource, ComputerError> {
    CGEventSource::new(CGEventSourceStateID::Private)
        .map_err(|()| native_event_error("event source"))
}

struct NativeInput<'a> {
    pid: i32,
    intervention_target: &'a super::InterventionTarget,
    pointers: &'a PointerSink,
}

impl NativeInput<'_> {
    fn post(&self, event: &CGEvent) {
        mark_synthetic(event);
        self.intervention_target
            .expect_synthetic_pointer_event(event);
        let kind = event.get_type();
        let pressed = match kind {
            CGEventType::LeftMouseDown
            | CGEventType::RightMouseDown
            | CGEventType::OtherMouseDown
            | CGEventType::LeftMouseDragged
            | CGEventType::RightMouseDragged
            | CGEventType::OtherMouseDragged => Some(true),
            CGEventType::MouseMoved
            | CGEventType::LeftMouseUp
            | CGEventType::RightMouseUp
            | CGEventType::OtherMouseUp => Some(false),
            _ => None,
        };
        if let Some(pressed) = pressed {
            let point = event.location();
            self.pointers.publish(
                Point {
                    x: point.x,
                    y: point.y,
                },
                pressed,
            );
        }
        event.post_to_pid(self.pid);
    }
}

fn post_click(
    input: &NativeInput<'_>,
    point: Point,
    button: MouseButton,
) -> Result<(), ComputerError> {
    let source = event_source()?;
    let (button, down, up) = match button {
        MouseButton::Left => (
            CGMouseButton::Left,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
        ),
        MouseButton::Right => (
            CGMouseButton::Right,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
        ),
        MouseButton::Center => (
            CGMouseButton::Center,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
        ),
    };
    let point = CGPoint::new(point.x, point.y);
    let down = CGEvent::new_mouse_event(source.clone(), down, point, button)
        .map_err(|()| native_event_error("mouse down"))?;
    let up = CGEvent::new_mouse_event(source, up, point, button)
        .map_err(|()| native_event_error("mouse up"))?;
    input.post(&down);
    std::thread::sleep(Duration::from_millis(20));
    input.post(&up);
    Ok(())
}

fn post_move(input: &NativeInput<'_>, point: Point) -> Result<(), ComputerError> {
    let event = CGEvent::new_mouse_event(
        event_source()?,
        CGEventType::MouseMoved,
        CGPoint::new(point.x, point.y),
        CGMouseButton::Left,
    )
    .map_err(|()| native_event_error("mouse move"))?;
    input.post(&event);
    Ok(())
}

fn post_drag(
    input: &NativeInput<'_>,
    from: Point,
    to: Point,
    duration_ms: u64,
) -> Result<(), ComputerError> {
    let source = event_source()?;
    let down = CGEvent::new_mouse_event(
        source.clone(),
        CGEventType::LeftMouseDown,
        CGPoint::new(from.x, from.y),
        CGMouseButton::Left,
    )
    .map_err(|()| native_event_error("drag down"))?;
    input.post(&down);
    let steps = (duration_ms / 16).clamp(2, 240);
    for step in 1..=steps {
        let progress = step as f64 / steps as f64;
        let point = CGPoint::new(
            from.x + (to.x - from.x) * progress,
            from.y + (to.y - from.y) * progress,
        );
        let event = CGEvent::new_mouse_event(
            source.clone(),
            CGEventType::LeftMouseDragged,
            point,
            CGMouseButton::Left,
        )
        .map_err(|()| native_event_error("drag move"))?;
        input.post(&event);
        std::thread::sleep(Duration::from_millis(duration_ms / steps));
    }
    let up = CGEvent::new_mouse_event(
        source,
        CGEventType::LeftMouseUp,
        CGPoint::new(to.x, to.y),
        CGMouseButton::Left,
    )
    .map_err(|()| native_event_error("drag up"))?;
    input.post(&up);
    Ok(())
}

fn post_key(
    input: &NativeInput<'_>,
    key: &str,
    modifiers: &[KeyModifier],
) -> Result<(), ComputerError> {
    let code = key_code(key).ok_or_else(|| ComputerError::UnknownKey {
        key: key.to_owned(),
    })?;
    let flags = modifier_flags(modifiers);
    let source = event_source()?;
    let down = CGEvent::new_keyboard_event(source.clone(), code, true)
        .map_err(|()| native_event_error("key down"))?;
    let up = CGEvent::new_keyboard_event(source, code, false)
        .map_err(|()| native_event_error("key up"))?;
    down.set_flags(flags);
    up.set_flags(flags);
    input.post(&down);
    std::thread::sleep(Duration::from_millis(10));
    input.post(&up);
    Ok(())
}

fn post_text(input: &NativeInput<'_>, text: &str) -> Result<(), ComputerError> {
    let source = event_source()?;
    for chunk in unicode_chunks(text, 20) {
        let down = CGEvent::new_keyboard_event(source.clone(), 0, true)
            .map_err(|()| native_event_error("Unicode key down"))?;
        let up = CGEvent::new_keyboard_event(source.clone(), 0, false)
            .map_err(|()| native_event_error("Unicode key up"))?;
        down.set_string(chunk);
        up.set_string(chunk);
        input.post(&down);
        input.post(&up);
    }
    Ok(())
}

fn unicode_chunks(value: &str, maximum_utf16: usize) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut units = 0;
    for (offset, character) in value.char_indices() {
        let next = character.len_utf16();
        if units > 0 && units + next > maximum_utf16 {
            chunks.push(&value[start..offset]);
            start = offset;
            units = 0;
        }
        units += next;
    }
    if start < value.len() {
        chunks.push(&value[start..]);
    }
    chunks
}

fn modifier_flags(modifiers: &[KeyModifier]) -> CGEventFlags {
    let unique = modifiers.iter().copied().collect::<HashSet<_>>();
    unique
        .into_iter()
        .fold(CGEventFlags::empty(), |flags, modifier| {
            flags
                | match modifier {
                    KeyModifier::Command => CGEventFlags::CGEventFlagCommand,
                    KeyModifier::Control => CGEventFlags::CGEventFlagControl,
                    KeyModifier::Option => CGEventFlags::CGEventFlagAlternate,
                    KeyModifier::Shift => CGEventFlags::CGEventFlagShift,
                    KeyModifier::Function => CGEventFlags::CGEventFlagSecondaryFn,
                }
        })
}

fn key_code(key: &str) -> Option<u16> {
    Some(match key.to_ascii_lowercase().as_str() {
        "a" => KeyCode::ANSI_A,
        "b" => KeyCode::ANSI_B,
        "c" => KeyCode::ANSI_C,
        "d" => KeyCode::ANSI_D,
        "e" => KeyCode::ANSI_E,
        "f" => KeyCode::ANSI_F,
        "g" => KeyCode::ANSI_G,
        "h" => KeyCode::ANSI_H,
        "i" => KeyCode::ANSI_I,
        "j" => KeyCode::ANSI_J,
        "k" => KeyCode::ANSI_K,
        "l" => KeyCode::ANSI_L,
        "m" => KeyCode::ANSI_M,
        "n" => KeyCode::ANSI_N,
        "o" => KeyCode::ANSI_O,
        "p" => KeyCode::ANSI_P,
        "q" => KeyCode::ANSI_Q,
        "r" => KeyCode::ANSI_R,
        "s" => KeyCode::ANSI_S,
        "t" => KeyCode::ANSI_T,
        "u" => KeyCode::ANSI_U,
        "v" => KeyCode::ANSI_V,
        "w" => KeyCode::ANSI_W,
        "x" => KeyCode::ANSI_X,
        "y" => KeyCode::ANSI_Y,
        "z" => KeyCode::ANSI_Z,
        "0" => KeyCode::ANSI_0,
        "1" => KeyCode::ANSI_1,
        "2" => KeyCode::ANSI_2,
        "3" => KeyCode::ANSI_3,
        "4" => KeyCode::ANSI_4,
        "5" => KeyCode::ANSI_5,
        "6" => KeyCode::ANSI_6,
        "7" => KeyCode::ANSI_7,
        "8" => KeyCode::ANSI_8,
        "9" => KeyCode::ANSI_9,
        "return" | "enter" => KeyCode::RETURN,
        "tab" => KeyCode::TAB,
        "space" => KeyCode::SPACE,
        "backspace" | "delete" => KeyCode::DELETE,
        "forward_delete" => KeyCode::FORWARD_DELETE,
        "escape" | "esc" => KeyCode::ESCAPE,
        "left" | "arrowleft" => KeyCode::LEFT_ARROW,
        "right" | "arrowright" => KeyCode::RIGHT_ARROW,
        "up" | "arrowup" => KeyCode::UP_ARROW,
        "down" | "arrowdown" => KeyCode::DOWN_ARROW,
        "home" => KeyCode::HOME,
        "end" => KeyCode::END,
        "page_up" => KeyCode::PAGE_UP,
        "page_down" => KeyCode::PAGE_DOWN,
        "f1" => KeyCode::F1,
        "f2" => KeyCode::F2,
        "f3" => KeyCode::F3,
        "f4" => KeyCode::F4,
        "f5" => KeyCode::F5,
        "f6" => KeyCode::F6,
        "f7" => KeyCode::F7,
        "f8" => KeyCode::F8,
        "f9" => KeyCode::F9,
        "f10" => KeyCode::F10,
        "f11" => KeyCode::F11,
        "f12" => KeyCode::F12,
        _ => return None,
    })
}

fn capture_image(
    window: &NativeWindow,
    parent: &Span,
    sample_index: Option<u64>,
) -> Result<Arc<CapturedImage>, ComputerError> {
    let phase = if sample_index.is_some() {
        "settling"
    } else {
        "observation"
    };
    let span = info_span!(
        target: "nanocodex_computer",
        parent: parent,
        "computer.screen.capture",
        computer.target.window_id = window.id,
        computer.capture.phase = phase,
        computer.capture.sample_index = sample_index.unwrap_or(0),
        computer.capture.backend = Empty,
        computer.screenshot.bytes = Empty,
        computer.screenshot.width = Empty,
        computer.screenshot.height = Empty,
        duration_ns = Empty,
        status = Empty,
        otel.status_code = Empty,
    );
    let started = Instant::now();
    let outcome = span.in_scope(|| capture_image_inner(window));
    finish_span(&span, started, &outcome);
    if let Ok((image, backend)) = &outcome {
        span.record("computer.capture.backend", *backend);
        span.record("computer.screenshot.bytes", image.png().len());
        span.record("computer.screenshot.width", image.width());
        span.record("computer.screenshot.height", image.height());
    }
    outcome.map(|(image, _)| image)
}

fn capture_image_inner(
    window: &NativeWindow,
) -> Result<(Arc<CapturedImage>, &'static str), ComputerError> {
    if request_screen_capture() == PermissionRequest::Prompted {
        return Err(ComputerError::Permission {
            permission: Permission::ScreenRecording,
            guidance: permission_guidance(Permission::ScreenRecording),
        });
    }
    let image = capture_window(window.id).map_err(|error| ComputerError::Native {
        message: format!("window {} could not be captured: {error}", window.id),
    })?;
    let (png, backend) = match image.data {
        NativeImageData::Png(png) => (png, "screencapture_service"),
        NativeImageData::Rgba(rgba) => {
            let mut png = Vec::new();
            image::codecs::png::PngEncoder::new(&mut png)
                .write_image(
                    &rgba,
                    image.width,
                    image.height,
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|error| ComputerError::Native {
                    message: format!("failed to encode screenshot: {error}"),
                })?;
            (png, "core_graphics_fallback")
        }
    };
    let digest = hex_digest(Sha256::digest(&png).as_slice());
    Ok((
        Arc::new(CapturedImage::new(
            image.width,
            image.height,
            digest,
            Arc::from(png),
        )),
        backend,
    ))
}

fn persist_image(
    image: &CapturedImage,
    generation: u64,
    sequence: u64,
    artifact_root: &Path,
    parent: &Span,
) -> Result<ScreenshotArtifact, ComputerError> {
    let span = info_span!(
        target: "nanocodex_computer",
        parent: parent,
        "computer.artifact.persist",
        computer.action.sequence = sequence,
        computer.observation.generation = generation,
        computer.screenshot.bytes = image.png().len(),
        duration_ns = Empty,
        status = Empty,
        otel.status_code = Empty,
    );
    let started = Instant::now();
    let filename = format!("frame-{sequence:06}-{generation:06}.png");
    let path = artifact_root.join(filename);
    let outcome = std::fs::write(&path, image.png()).map_err(|source| ComputerError::Io {
        path: path.clone(),
        source,
    });
    finish_span(&span, started, &outcome);
    outcome?;
    Ok(ScreenshotArtifact {
        path,
        width: image.width(),
        height: image.height(),
        digest: image.digest().to_owned(),
    })
}

fn permission_guidance(permission: Permission) -> String {
    match permission {
        Permission::Accessibility => "macOS opened its Accessibility request. Enable this executable in System Settings > Privacy & Security > Accessibility, then fully quit and relaunch it"
            .to_owned(),
        Permission::ScreenRecording => "macOS opened its Screen Recording request. Enable this executable in System Settings > Privacy & Security > Screen & System Audio Recording, then fully quit and relaunch it"
            .to_owned(),
        Permission::InputMonitoring => "enable Input Monitoring in System Settings > Privacy & Security to detect human takeover automatically"
            .to_owned(),
    }
}

fn visual_signature(sample: &VisualSample) -> String {
    let mut digest = Sha256::new();
    digest.update(sample.window.id.to_le_bytes());
    digest.update(sample.window.frame.x.to_bits().to_le_bytes());
    digest.update(sample.window.frame.y.to_bits().to_le_bytes());
    digest.update(sample.window.frame.width.to_bits().to_le_bytes());
    digest.update(sample.window.frame.height.to_bits().to_le_bytes());
    digest.update(sample.image.digest().as_bytes());
    hex_digest(digest.finalize().as_slice())
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn record_observation(span: &Span, observed: &ComputerObservation, image: Option<&CapturedImage>) {
    span.record(
        "computer.accessibility.element_count",
        observed.elements.len(),
    );
    if let Some(update) = &observed.accessibility_update {
        span.record("computer.accessibility.update.added", update.added.len());
        span.record(
            "computer.accessibility.update.changed",
            update.changed.len(),
        );
        span.record(
            "computer.accessibility.update.removed",
            update.removed.len(),
        );
    }
    if let Some(image) = image {
        span.record("computer.screenshot.bytes", image.png().len());
        span.record("computer.screenshot.width", image.width());
        span.record("computer.screenshot.height", image.height());
    }
}

fn finish_span<T, E>(span: &Span, started: Instant, result: &Result<T, E>) {
    span.record("duration_ns", elapsed_ns(started.elapsed()));
    if result.is_ok() {
        span.record("status", "ok");
        span.record("otel.status_code", "OK");
    } else {
        span.record("status", "failed");
        span.record("otel.status_code", "ERROR");
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn elapsed_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn native_event_error(operation: &str) -> ComputerError {
    ComputerError::Native {
        message: format!("CoreGraphics could not create {operation} event"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_input_chunks_never_split_scalars_or_exceed_limit() {
        let input = "1234567890123456789🦀more text";
        let chunks = unicode_chunks(input, 20);
        assert_eq!(chunks.concat(), input);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.encode_utf16().count() <= 20)
        );
    }

    #[test]
    fn known_key_names_are_case_insensitive() {
        assert_eq!(key_code("ESC"), Some(KeyCode::ESCAPE));
        assert_eq!(key_code("arrowLeft"), Some(KeyCode::LEFT_ARROW));
        assert_eq!(key_code("not-a-key"), None);
    }

    #[test]
    fn permission_guidance_describes_the_native_prompt_and_required_relaunch() {
        let screen = permission_guidance(Permission::ScreenRecording);
        assert!(screen.contains("macOS opened its Screen Recording request"));
        assert!(screen.contains("fully quit and relaunch"));

        let accessibility = permission_guidance(Permission::Accessibility);
        assert!(accessibility.contains("macOS opened its Accessibility request"));
        assert!(accessibility.contains("fully quit and relaunch"));
    }

    #[test]
    fn implicit_target_selection_rejects_tiny_auxiliary_windows() {
        let mut window = NativeWindow {
            id: 1,
            pid: 2,
            owner_name: "Fixture".to_owned(),
            bundle_id: Some("dev.nanocodex.fixture".to_owned()),
            title: Some("Document".to_owned()),
            x: 0.0,
            y: 0.0,
            width: 640.0,
            height: 480.0,
            on_screen: true,
        };
        assert!(primary_window_candidate(&window));
        window.width = 1.0;
        assert!(!normal_window_candidate(&window));
        window.width = 640.0;
        window.title = None;
        assert!(normal_window_candidate(&window));
        assert!(!primary_window_candidate(&window));
    }

    #[test]
    fn accessibility_window_matching_tolerates_coordinate_rounding_only() {
        let capture = Rect {
            x: 864.0,
            y: 33.0,
            width: 480.0,
            height: 1_083.0,
        };
        assert!(matching_window_frames(
            Rect {
                x: 865.0,
                height: 1_082.0,
                ..capture
            },
            capture
        ));
        assert!(!matching_window_frames(
            Rect {
                x: 1_152.0,
                ..capture
            },
            capture
        ));
    }

    #[test]
    fn element_reference_exposes_its_generation_scoped_hint_and_hash() {
        let reference = ElementRef("e7_42_deadbeefdeadbeef".to_owned());
        assert_eq!(
            reference_parts(&reference, 7),
            Some((42, 0xdead_beef_dead_beef))
        );
        assert_eq!(reference_parts(&reference, 8), None);
        assert_eq!(reference_parts(&ElementRef("e7_42".to_owned()), 7), None);
    }

    #[test]
    fn accessibility_revision_correlates_only_unambiguous_elements() {
        fn element(reference: &str, label: &str, value: &str) -> Element {
            Element {
                reference: ElementRef(reference.to_owned()),
                role: "AXButton".to_owned(),
                subrole: None,
                label: Some(label.to_owned()),
                value: Some(value.to_owned()),
                selected_text: None,
                placeholder: None,
                identifier: None,
                url: None,
                frame: None,
                enabled: Some(true),
                focused: Some(false),
                actions: vec!["AXPress".to_owned()],
            }
        }

        let previous = vec![
            element("e1_0_a", "Save", "off"),
            element("e1_1_b", "Gone", "old"),
            element("e1_2_c", "Duplicate", "one"),
            element("e1_3_d", "Duplicate", "two"),
        ];
        let current = vec![
            element("e2_0_e", "Save", "on"),
            element("e2_1_f", "New", "new"),
            element("e2_2_g", "Duplicate", "one"),
            element("e2_3_h", "Duplicate", "two"),
        ];
        let update = accessibility_update(1, &previous, &current);

        assert_eq!(update.base_generation, 1);
        assert_eq!(update.changed, vec![current[0].clone()]);
        assert_eq!(update.added, current[1..].to_vec());
        assert_eq!(
            update.removed,
            previous[1..]
                .iter()
                .map(|element| element.reference.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn url_policy_is_origin_scoped_and_fails_closed() {
        let allowed = [Url::parse("https://example.com").unwrap()];
        assert!(enforce_url_policy("https://example.com/inbox", Some(&allowed)).is_ok());
        assert!(matches!(
            enforce_url_policy("https://evil.example/inbox", Some(&allowed)),
            Err(ComputerError::UrlDenied { .. })
        ));
        assert!(matches!(
            enforce_url_policy("mailto:user@example.com", Some(&allowed)),
            Err(ComputerError::UrlDenied { .. })
        ));
        assert!(matches!(
            enforce_url_policy("not a URL", Some(&allowed)),
            Err(ComputerError::UrlDenied { .. })
        ));
    }
}
