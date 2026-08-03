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
    NativeImageData, NativeWindow, PermissionRequest, capture_window, element_rect, mark_synthetic,
    request_accessibility, request_screen_capture, window as native_window_by_id,
};
use sha2::{Digest as _, Sha256};

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

pub(super) struct MacosBackend {
    artifact_root: PathBuf,
    settle: SettlePolicy,
    maximum_elements: usize,
    target: Option<Target>,
    generation: u64,
    screenshots: VecDeque<PathBuf>,
}

impl MacosBackend {
    pub(super) const fn new(
        artifact_root: PathBuf,
        settle: SettlePolicy,
        maximum_elements: usize,
    ) -> Self {
        Self {
            artifact_root,
            settle,
            maximum_elements,
            target: None,
            generation: 0,
            screenshots: VecDeque::new(),
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
        let root = self.artifact_root.clone();
        let maximum = self.maximum_elements;
        let (observed, image) = tokio::task::spawn_blocking(move || {
            observe_target(
                target, generation, sequence, screenshot, settled, &root, maximum,
            )
        })
        .await
        .map_err(|error| ComputerError::Native {
            message: format!("observation worker panicked: {error}"),
        })??;
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
        let deadline = Instant::now() + self.settle.timeout;
        let mut previous: Option<String> = None;
        let generation = self.generation.saturating_add(1);
        loop {
            state.ensure_running()?;
            let target = self.target.clone().ok_or(ComputerError::NoTarget)?;
            let sample = tokio::task::spawn_blocking(move || visual_sample(target))
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
                let root = self.artifact_root.clone();
                let maximum = self.maximum_elements;
                let settled = matched;
                let observed = tokio::task::spawn_blocking(move || {
                    observation_from_visual(sample, generation, sequence, settled, &root, maximum)
                })
                .await
                .map_err(|error| ComputerError::Native {
                    message: format!("semantic observation worker panicked: {error}"),
                })??;
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
        tokio::task::spawn_blocking(move || native_action(&target, generation, maximum, action))
            .await
            .map_err(|error| ComputerError::Native {
                message: format!("input worker panicked: {error}"),
            })??;
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
                let (applications, windows, _) = tokio::task::spawn_blocking(discover)
                    .await
                    .map_err(|error| ComputerError::Native {
                        message: format!("discovery worker panicked: {error}"),
                    })??;
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
                let status = tokio::process::Command::new("/usr/bin/open")
                    .arg("-b")
                    .arg(&bundle_id)
                    .status()
                    .await
                    .map_err(|error| ComputerError::Native {
                        message: format!("failed to launch {bundle_id}: {error}"),
                    })?;
                if !status.success() {
                    return Err(ComputerError::Native {
                        message: format!("LaunchServices rejected {bundle_id} with {status}"),
                    });
                }
                Ok(ComputerOutput::Opened { bundle_id })
            }
            ComputerAction::Attach {
                application,
                window_id,
            } => {
                let target =
                    tokio::task::spawn_blocking(move || select_target(application, window_id))
                        .await
                        .map_err(|error| ComputerError::Native {
                            message: format!("attach worker panicked: {error}"),
                        })??;
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
                let deadline = tokio::time::Instant::now() + Duration::from_millis(milliseconds);
                while tokio::time::Instant::now() < deadline {
                    state.ensure_running()?;
                    tokio::time::sleep(
                        Duration::from_millis(50)
                            .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
                    )
                    .await;
                }
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
    target: Target,
    generation: u64,
    sequence: u64,
    include_screenshot: bool,
    settled: bool,
    artifact_root: &Path,
    maximum_elements: usize,
) -> Result<(ComputerObservation, Option<Arc<CapturedImage>>), ComputerError> {
    let native_window =
        native_window_by_id(target.window.id).ok_or_else(|| ComputerError::TargetNotFound {
            message: format!("window {} is no longer capturable", target.window.id),
        })?;
    let target = Target {
        application: target.application,
        window: public_window(&native_window),
    };
    let (elements, image) = std::thread::scope(|scope| {
        let capture = include_screenshot.then(|| scope.spawn(|| capture_image(&native_window)));
        let elements = accessibility_snapshot(&target, generation, maximum_elements)
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
        .map(|image| persist_image(image, generation, sequence, artifact_root))
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

fn visual_sample(target: Target) -> Result<VisualSample, ComputerError> {
    let native_window =
        native_window_by_id(target.window.id).ok_or_else(|| ComputerError::TargetNotFound {
            message: format!("window {} is no longer capturable", target.window.id),
        })?;
    Ok(VisualSample {
        application: target.application,
        window: public_window(&native_window),
        image: capture_image(&native_window)?,
    })
}

fn observation_from_visual(
    sample: VisualSample,
    generation: u64,
    sequence: u64,
    settled: bool,
    artifact_root: &Path,
    maximum_elements: usize,
) -> Result<ComputerObservation, ComputerError> {
    let target = Target {
        application: sample.application,
        window: sample.window,
    };
    let elements = accessibility_snapshot(&target, generation, maximum_elements)?
        .into_iter()
        .map(|(element, _)| element)
        .collect();
    let screenshot = persist_image(&sample.image, generation, sequence, artifact_root)?;
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
) -> Result<Vec<(Element, AXUIElement)>, ComputerError> {
    let raw = accessibility_elements(target, maximum_elements)?;
    let mut output = Vec::with_capacity(raw.len());
    for (raw_index, element) in raw.into_iter().enumerate() {
        let mut public = public_element(&element, generation, raw_index);
        if should_include(&public) {
            public.reference = reference_for(generation, raw_index, &public);
            output.push((public, element));
        }
    }
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
    let root = select_accessibility_window(&application, target).unwrap_or(application);
    let mut raw = Vec::with_capacity(maximum_elements.min(256));
    walk_accessibility(&root, 0, maximum_elements, &mut raw);
    Ok(raw)
}

fn select_accessibility_window(application: &AXUIElement, target: &Target) -> Option<AXUIElement> {
    let windows = application.windows().ok()?;
    windows
        .iter()
        .find(|window| {
            target.window.title.as_ref().is_some_and(|title| {
                #[allow(clippy::cmp_owned)]
                window
                    .title()
                    .is_ok_and(|candidate| candidate.to_string() == *title)
            })
        })
        .map(|window| (*window).clone())
        .or_else(|| application.focused_window().ok())
        .or_else(|| application.main_window().ok())
}

fn walk_accessibility(
    element: &AXUIElement,
    depth: usize,
    maximum: usize,
    output: &mut Vec<AXUIElement>,
) {
    if depth > MAX_TREE_DEPTH || output.len() >= maximum {
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
    ElementRef(format!("e{generation}_{index}_{hash:016x}"))
}

fn resolve_element(
    target: &Target,
    generation: u64,
    maximum: usize,
    reference: &ElementRef,
) -> Result<(Element, AXUIElement), ComputerError> {
    let raw_index =
        reference_index(reference, generation).ok_or_else(|| ComputerError::StaleReference {
            reference: reference.0.clone(),
        })?;
    let element = accessibility_elements(target, maximum)?
        .into_iter()
        .nth(raw_index)
        .ok_or_else(|| ComputerError::StaleReference {
            reference: reference.0.clone(),
        })?;
    let mut public = public_element(&element, generation, raw_index);
    if !should_include(&public) {
        return Err(ComputerError::StaleReference {
            reference: reference.0.clone(),
        });
    }
    public.reference = reference_for(generation, raw_index, &public);
    if public.reference != *reference {
        return Err(ComputerError::StaleReference {
            reference: reference.0.clone(),
        });
    }
    Ok((public, element))
}

fn reference_index(reference: &ElementRef, generation: u64) -> Option<usize> {
    let remainder = reference.0.strip_prefix(&format!("e{generation}_"))?;
    let (index, hash) = remainder.split_once('_')?;
    (!hash.is_empty()).then(|| index.parse().ok()).flatten()
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

fn capture_image(window: &NativeWindow) -> Result<Arc<CapturedImage>, ComputerError> {
    if request_screen_capture() == PermissionRequest::Prompted {
        return Err(ComputerError::Permission {
            permission: Permission::ScreenRecording,
            guidance: permission_guidance(Permission::ScreenRecording),
        });
    }
    let image = capture_window(window.id).map_err(|error| ComputerError::Native {
        message: format!("window {} could not be captured: {error}", window.id),
    })?;
    let png = match image.data {
        NativeImageData::Png(png) => png,
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
            png
        }
    };
    let digest = hex_digest(Sha256::digest(&png).as_slice());
    Ok(Arc::new(CapturedImage::new(
        image.width,
        image.height,
        digest,
        Arc::from(png),
    )))
}

fn persist_image(
    image: &CapturedImage,
    generation: u64,
    sequence: u64,
    artifact_root: &Path,
) -> Result<ScreenshotArtifact, ComputerError> {
    let filename = format!("frame-{sequence:06}-{generation:06}.png");
    let path = artifact_root.join(filename);
    std::fs::write(&path, image.png()).map_err(|source| ComputerError::Io {
        path: path.clone(),
        source,
    })?;
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
    fn element_reference_exposes_only_its_generation_scoped_raw_index() {
        let reference = ElementRef("e7_42_deadbeef".to_owned());
        assert_eq!(reference_index(&reference, 7), Some(42));
        assert_eq!(reference_index(&reference, 8), None);
        assert_eq!(reference_index(&ElementRef("e7_42".to_owned()), 7), None);
    }
}
