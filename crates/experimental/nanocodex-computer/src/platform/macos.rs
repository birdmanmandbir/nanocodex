use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use accessibility::{AXAttribute, AXUIElement, attribute::AXUIElementAttributes};
use async_trait::async_trait;
use core_foundation::{base::TCFType, string::CFString};
use core_graphics::{
    event::{CGEvent, CGEventFlags, CGEventType, CGMouseButton, KeyCode, ScrollEventUnit},
    event_source::{CGEventSource, CGEventSourceStateID},
    geometry::CGPoint,
};
use image::ImageEncoder as _;
use nanocodex_computer_macos::{
    NativeImageData, NativeWindow, PermissionRequest, activate_application, capture_window,
    element_rect, enable_application_accessibility as enable_native_application_accessibility,
    frontmost_application_pid, mark_synthetic, request_accessibility, request_screen_capture,
    window as native_window_by_id,
};
use sha2::{Digest as _, Sha256};
use tracing::{Instrument as _, Span, field::Empty, info, info_span};

use super::Backend;
use crate::{
    Application, ApplicationSelector, CapturedImage, ComputerAction, ComputerError, ComputerFrame,
    ComputerFramePhase, ComputerObservation, ComputerOutput, Element, ElementRef,
    InteractionTarget, KeyModifier, MouseButton, Permission, Point, Rect, ScreenshotArtifact,
    SettlePolicy, Window,
    driver::{FrameSink, RunState},
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
}

type Discovery = (Vec<Application>, Vec<Window>, Vec<NativeWindow>);

struct VisualSample {
    application: Application,
    window: Window,
    image: Arc<CapturedImage>,
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
    screenshots: VecDeque<PathBuf>,
    intervention_target: Arc<super::InterventionTarget>,
}

impl MacosBackend {
    pub(super) const fn new(
        artifact_root: PathBuf,
        settle: SettlePolicy,
        maximum_elements: usize,
        intervention_target: Arc<super::InterventionTarget>,
    ) -> Self {
        Self {
            artifact_root,
            settle,
            maximum_elements,
            target: None,
            generation: 0,
            screenshots: VecDeque::new(),
            intervention_target,
        }
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
        let (observed, image) = outcome?;
        record_observation(&span, &observed, image.as_deref());
        self.target = Some(Target {
            application: observed.application.clone(),
            window: observed.window.clone(),
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
    ) -> Result<ComputerObservation, ComputerError> {
        let span = info_span!(
            target: "nanocodex_computer",
            "computer.app.settle",
            computer.action.sequence = sequence,
            computer.settle.timeout_ms = millis(self.settle.timeout),
            computer.settle.sample_interval_ms = millis(self.settle.sample_interval),
            computer.settle.sample_count = Empty,
            computer.settle.settled = Empty,
            duration_ns = Empty,
            status = Empty,
            otel.status_code = Empty,
        );
        let started = Instant::now();
        let outcome = self
            .settled_observation_inner(sequence, state, frames, &span)
            .instrument(span.clone())
            .await;
        finish_span(&span, started, &outcome);
        outcome
    }

    async fn settled_observation_inner(
        &mut self,
        sequence: u64,
        state: Arc<RunState>,
        frames: &mut FrameSink,
        span: &Span,
    ) -> Result<ComputerObservation, ComputerError> {
        let deadline = Instant::now() + self.settle.timeout;
        let mut previous: Option<String> = None;
        let generation = self.generation.saturating_add(1);
        let mut sample_count = 0_u64;
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
            let signature = visual_signature(&sample);
            let matched = previous.as_deref() == Some(signature.as_str());
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
                span.record("computer.settle.settled", matched);
                let root = self.artifact_root.clone();
                let maximum = self.maximum_elements;
                let settled = matched;
                let verify_span = info_span!(
                    target: "nanocodex_computer",
                    parent: span,
                    "computer.postcondition.verify",
                    computer.action.sequence = sequence,
                    computer.observation.generation = generation,
                    computer.accessibility.maximum_elements = maximum,
                    computer.accessibility.element_count = Empty,
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
                let observed = observed?;
                record_observation(&verify_span, &observed, None);
                drop(verify_span);
                self.generation = generation;
                self.target = Some(Target {
                    application: observed.application.clone(),
                    window: observed.window.clone(),
                });
                self.retain_screenshot(&observed).await;
                return Ok(observed);
            }
            previous = Some(signature);
            tokio::time::sleep(self.settle.sample_interval).await;
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

    async fn mutate(
        &mut self,
        action: NativeAction,
        sequence: u64,
        state: Arc<RunState>,
        frames: &mut FrameSink,
    ) -> Result<ComputerOutput, ComputerError> {
        state.ensure_running()?;
        let target = self.target.clone().ok_or(ComputerError::NoTarget)?;
        let generation = self.generation;
        let maximum = self.maximum_elements;
        let action_kind = action.kind();
        let dispatch = action.dispatch();
        let span = info_span!(
            target: "nanocodex_computer",
            "computer.input.dispatch",
            computer.action.sequence = sequence,
            computer.input.kind = action_kind,
            computer.input.dispatch = dispatch,
            computer.target.pid = target.application.pid,
            computer.target.window_id = target.window.id,
            duration_ns = Empty,
            status = Empty,
            otel.status_code = Empty,
        );
        let started = Instant::now();
        let outcome = async move {
            tokio::task::spawn_blocking(move || native_action(&target, generation, maximum, action))
                .await
                .map_err(|error| ComputerError::Native {
                    message: format!("input worker panicked: {error}"),
                })?
        }
        .instrument(span.clone())
        .await;
        finish_span(&span, started, &outcome);
        outcome?;
        drop(span);
        Ok(ComputerOutput::State {
            state: self.settled_observation(sequence, state, frames).await?,
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
    ) -> Result<ComputerOutput, ComputerError> {
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
                let (applications, windows, native) = outcome?;
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
                span.record("computer.target.pid", target.application.pid);
                span.record("computer.target.window_id", target.window.id);
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
                )
                .await
            }
            ComputerAction::PressKey { key, modifiers } => {
                self.mutate(
                    NativeAction::PressKey { key, modifiers },
                    sequence,
                    state,
                    frames,
                )
                .await
            }
            ComputerAction::TypeText { text } => {
                if text.chars().count() > 100_000 {
                    return Err(ComputerError::InvalidAction {
                        message: "type_text is limited to 100000 Unicode scalar values".to_owned(),
                    });
                }
                self.mutate(NativeAction::TypeText { text }, sequence, state, frames)
                    .await
            }
            ComputerAction::SetValue { reference, value } => {
                self.mutate(
                    NativeAction::SetValue { reference, value },
                    sequence,
                    state,
                    frames,
                )
                .await
            }
            ComputerAction::PerformAction { reference, name } => {
                self.mutate(
                    NativeAction::PerformAction { reference, name },
                    sequence,
                    state,
                    frames,
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
    let native_window =
        native_window_by_id(target.window.id).ok_or_else(|| ComputerError::TargetNotFound {
            message: format!("window {} is no longer capturable", target.window.id),
        })?;
    let target = Target {
        application: target.application,
        window: public_window(&native_window),
    };
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
    let native_window =
        native_window_by_id(target.window.id).ok_or_else(|| ComputerError::TargetNotFound {
            message: format!("window {} is no longer capturable", target.window.id),
        })?;
    Ok(VisualSample {
        application: target.application,
        window: public_window(&native_window),
        image: capture_image(&native_window, parent, Some(sample_index))?,
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
        placeholder: string_attribute(element.placeholder_value()),
        identifier: string_attribute(element.identifier()),
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
        || element.placeholder.is_some()
        || element.identifier.is_some()
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

fn string_attribute(value: Result<CFString, accessibility::Error>) -> Option<String> {
    value
        .ok()
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
        .map(|value| truncate(value, MAX_TEXT_CHARS))
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
        "{}\0{:?}\0{:?}\0{:?}\0{:?}",
        element.role, element.identifier, element.label, element.value, element.frame
    )
    .bytes()
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
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
) -> Result<(), ComputerError> {
    let pid = target.application.pid;
    match action {
        NativeAction::Click {
            target: InteractionTarget::Element(reference),
            button: MouseButton::Left,
        } => {
            let (public, element) = resolve_element(target, generation, maximum, &reference)?;
            if public.actions.iter().any(|action| action == "AXPress") {
                return element
                    .perform_action(&CFString::new("AXPress"))
                    .map_err(|error| ComputerError::Native {
                        message: format!("AXPress failed for {reference}: {error}"),
                    });
            }
            post_click(
                pid,
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
            pid,
            resolve_point(target, generation, maximum, point)?,
            button,
        ),
        NativeAction::Drag {
            from,
            to,
            duration_ms,
        } => post_drag(
            pid,
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
                post_move(pid, resolve_point(target, generation, maximum, at)?)?;
            }
            let source = event_source()?;
            let event =
                CGEvent::new_scroll_event(source, ScrollEventUnit::PIXEL, 2, delta_y, delta_x, 0)
                    .map_err(|()| native_event_error("scroll"))?;
            post_event(&event, pid);
            Ok(())
        }
        NativeAction::PressKey { key, modifiers } => post_key(pid, &key, &modifiers),
        NativeAction::TypeText { text } => post_text(pid, &text),
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
            if !public.actions.iter().any(|action| action == &name) {
                return Err(ComputerError::InvalidAction {
                    message: format!("element {reference} does not advertise {name}"),
                });
            }
            element
                .perform_action(&CFString::new(&name))
                .map_err(|error| ComputerError::Native {
                    message: format!("{name} failed for {reference}: {error}"),
                })
        }
    }
}

fn event_source() -> Result<CGEventSource, ComputerError> {
    CGEventSource::new(CGEventSourceStateID::Private)
        .map_err(|()| native_event_error("event source"))
}

fn post_click(pid: i32, point: Point, button: MouseButton) -> Result<(), ComputerError> {
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
    post_event(&down, pid);
    std::thread::sleep(Duration::from_millis(20));
    post_event(&up, pid);
    Ok(())
}

fn post_move(pid: i32, point: Point) -> Result<(), ComputerError> {
    let event = CGEvent::new_mouse_event(
        event_source()?,
        CGEventType::MouseMoved,
        CGPoint::new(point.x, point.y),
        CGMouseButton::Left,
    )
    .map_err(|()| native_event_error("mouse move"))?;
    post_event(&event, pid);
    Ok(())
}

fn post_drag(pid: i32, from: Point, to: Point, duration_ms: u64) -> Result<(), ComputerError> {
    let source = event_source()?;
    let down = CGEvent::new_mouse_event(
        source.clone(),
        CGEventType::LeftMouseDown,
        CGPoint::new(from.x, from.y),
        CGMouseButton::Left,
    )
    .map_err(|()| native_event_error("drag down"))?;
    post_event(&down, pid);
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
        post_event(&event, pid);
        std::thread::sleep(Duration::from_millis(duration_ms / steps));
    }
    let up = CGEvent::new_mouse_event(
        source,
        CGEventType::LeftMouseUp,
        CGPoint::new(to.x, to.y),
        CGMouseButton::Left,
    )
    .map_err(|()| native_event_error("drag up"))?;
    post_event(&up, pid);
    Ok(())
}

fn post_key(pid: i32, key: &str, modifiers: &[KeyModifier]) -> Result<(), ComputerError> {
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
    post_event(&down, pid);
    std::thread::sleep(Duration::from_millis(10));
    post_event(&up, pid);
    Ok(())
}

fn post_text(pid: i32, text: &str) -> Result<(), ComputerError> {
    let source = event_source()?;
    for chunk in unicode_chunks(text, 20) {
        let down = CGEvent::new_keyboard_event(source.clone(), 0, true)
            .map_err(|()| native_event_error("Unicode key down"))?;
        let up = CGEvent::new_keyboard_event(source.clone(), 0, false)
            .map_err(|()| native_event_error("Unicode key up"))?;
        down.set_string(chunk);
        up.set_string(chunk);
        post_event(&down, pid);
        post_event(&up, pid);
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

fn post_event(event: &CGEvent, pid: i32) {
    mark_synthetic(event);
    event.post_to_pid(pid);
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
}
