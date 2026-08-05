use std::{fmt, path::PathBuf, sync::Arc, time::Duration};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A point in global macOS display coordinates.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize, JsonSchema)]
pub struct Point {
    /// Horizontal coordinate in points.
    pub x: f64,
    /// Vertical coordinate in points.
    pub y: f64,
}

/// A rectangle in global macOS display coordinates.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize, JsonSchema)]
pub struct Rect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub width: f64,
    /// Height.
    pub height: f64,
}

impl Rect {
    /// Returns the center of the rectangle.
    #[must_use]
    pub fn center(self) -> Point {
        Point {
            x: self.x + self.width / 2.0,
            y: self.y + self.height / 2.0,
        }
    }
}

/// A running graphical application visible to the capture system.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
pub struct Application {
    /// Process identifier.
    pub pid: i32,
    /// Display name.
    pub name: String,
    /// Stable bundle identifier when macOS supplies one.
    pub bundle_id: Option<String>,
}

/// A capturable application window.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema)]
pub struct Window {
    /// Core Graphics window identifier.
    pub id: u32,
    /// Owning process identifier.
    pub pid: i32,
    /// Window title.
    pub title: Option<String>,
    /// Current frame.
    pub frame: Rect,
    /// Whether the window is currently on screen.
    pub on_screen: bool,
}

/// Selects an application without relying on its transient process identifier.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationSelector {
    /// Exact process identifier.
    Pid(i32),
    /// Exact bundle identifier, such as `com.apple.TextEdit`.
    BundleId(String),
    /// Case-insensitive exact application display name.
    Name(String),
}

/// A reference into one particular accessibility snapshot.
///
/// References deliberately expire after every observation or action. This
/// prevents an agent from silently operating on a different element after the
/// application has changed underneath it.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct ElementRef(pub String);

impl fmt::Display for ElementRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// One compact, actionable accessibility node.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema)]
pub struct Element {
    /// Generation-bound reference accepted by semantic actions.
    pub reference: ElementRef,
    /// Accessibility role, for example `AXButton`.
    pub role: String,
    /// Accessibility subrole when present.
    pub subrole: Option<String>,
    /// Human-facing label assembled from title, description, and help.
    pub label: Option<String>,
    /// Current scalar value when it is useful and bounded.
    pub value: Option<String>,
    /// Current selected text for editable controls when macOS exposes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_text: Option<String>,
    /// Placeholder text for editable controls.
    pub placeholder: Option<String>,
    /// Stable application-provided identifier when present.
    pub identifier: Option<String>,
    /// URL represented by a link or web document when exposed by Accessibility.
    pub url: Option<String>,
    /// Global bounds when macOS exposes them.
    pub frame: Option<Rect>,
    /// Whether the element accepts interaction.
    pub enabled: Option<bool>,
    /// Whether the element currently owns focus.
    pub focused: Option<bool>,
    /// Accessibility actions supported by the element.
    pub actions: Vec<String>,
}

/// Incremental accessibility-tree revision relative to the preceding state.
///
/// The complete current tree remains available in [`ComputerObservation::elements`].
/// This revision is a compact hint for consumers that can process unchanged
/// application state without retransmitting or rescanning the whole tree.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema)]
pub struct AccessibilityUpdate {
    /// Generation of the complete tree used as the revision base.
    pub base_generation: u64,
    /// Current elements that did not have an unambiguous prior identity.
    pub added: Vec<Element>,
    /// Current elements whose observable attributes changed.
    pub changed: Vec<Element>,
    /// Prior generation references whose elements disappeared.
    pub removed: Vec<ElementRef>,
}

/// A persisted PNG artifact included in a model-facing observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
pub struct ScreenshotArtifact {
    /// Local PNG path. The computer tool converts this into model image input.
    pub path: PathBuf,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// Content digest used for visual settling and preview deduplication.
    pub digest: String,
}

/// Complete model-facing state returned after an observation or mutating action.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema)]
pub struct ComputerObservation {
    /// Monotonic state generation. Element references embed this generation.
    pub generation: u64,
    /// Selected application.
    pub application: Application,
    /// Selected window.
    pub window: Window,
    /// Compact actionable accessibility tree, in depth-first order.
    pub elements: Vec<Element>,
    /// Incremental update from the preceding state for this exact window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessibility_update: Option<AccessibilityUpdate>,
    /// Fresh screenshot when requested and permitted.
    pub screenshot: Option<ScreenshotArtifact>,
    /// Whether the post-action state reached two equal samples before timeout.
    pub settled: bool,
}

/// One captured PNG shared by live human-facing consumers.
///
/// Frame bytes stay in memory and are reference counted so a preview or TUI
/// never needs to reread a model artifact from disk. This type is deliberately
/// absent from model-facing serialization and schemas.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedImage {
    width: u32,
    height: u32,
    digest: String,
    png: Arc<[u8]>,
}

impl CapturedImage {
    pub(crate) const fn new(width: u32, height: u32, digest: String, png: Arc<[u8]>) -> Self {
        Self {
            width,
            height,
            digest,
            png,
        }
    }

    /// Returns the encoded PNG width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Returns the encoded PNG height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Returns the stable content digest used for coalescing and settling.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Borrows the encoded PNG bytes.
    #[must_use]
    pub fn png(&self) -> &[u8] {
        &self.png
    }
}

/// Progress represented by one human-facing visual frame.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComputerFramePhase {
    /// A caller explicitly attached to or observed the target.
    Observed,
    /// The target is still changing after a mutating action.
    Settling,
    /// Two consecutive visual samples matched.
    Settled,
    /// The settling deadline expired before consecutive samples matched.
    TimedOut,
}

/// One coalesced live frame intended for a TUI, preview, or human observer.
///
/// Unlike [`ComputerObservation`], a frame contains no accessibility tree and
/// is never sent to the model automatically.
#[derive(Clone, Debug, PartialEq)]
pub struct ComputerFrame {
    /// Action sequence that caused this frame.
    pub sequence: u64,
    /// Generation that the eventual model-facing observation will use.
    pub generation: u64,
    /// Selected application.
    pub application: Application,
    /// Selected window.
    pub window: Window,
    /// Current visual progress.
    pub phase: ComputerFramePhase,
    /// Shared encoded image.
    pub image: Arc<CapturedImage>,
}

/// A semantic element reference or an absolute point.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InteractionTarget {
    /// Resolve the center of a current accessibility element.
    Element(ElementRef),
    /// Use a global display coordinate.
    Point(Point),
}

/// Mouse button used by click and drag operations.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    /// Primary button.
    #[default]
    Left,
    /// Secondary button.
    Right,
    /// Middle button.
    Center,
}

/// Modifier held around a key event.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeyModifier {
    Command,
    Control,
    Option,
    Shift,
    Function,
}

/// A complete native computer operation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ComputerAction {
    /// Enumerate visible graphical applications and their windows.
    ListApplications,
    /// Launch an application in the background by exact bundle identifier.
    OpenApplication { bundle_id: String },
    /// Select an already running application and optionally a specific window.
    Attach {
        application: ApplicationSelector,
        window_id: Option<u32>,
    },
    /// Return fresh accessibility state and, by default, a screenshot.
    Observe {
        #[serde(default = "default_true")]
        screenshot: bool,
    },
    /// Click a semantic element or global point.
    Click {
        target: InteractionTarget,
        #[serde(default)]
        button: MouseButton,
    },
    /// Drag between semantic elements or points.
    Drag {
        from: InteractionTarget,
        to: InteractionTarget,
        #[serde(default = "default_drag_duration_ms")]
        duration_ms: u64,
    },
    /// Scroll at the current cursor or an explicit target.
    Scroll {
        delta_x: i32,
        delta_y: i32,
        at: Option<InteractionTarget>,
    },
    /// Send one physical key with optional modifiers to the selected process.
    PressKey {
        key: String,
        #[serde(default)]
        modifiers: Vec<KeyModifier>,
    },
    /// Insert Unicode text into the selected process.
    TypeText { text: String },
    /// Set an editable accessibility element's value without using clipboard state.
    SetValue {
        reference: ElementRef,
        value: String,
    },
    /// Perform a named accessibility action, such as `AXPress` or `AXShowMenu`.
    PerformAction { reference: ElementRef, name: String },
    /// Wait while continuing to honor pause and intervention controls.
    Wait { milliseconds: u64 },
}

impl ComputerAction {
    pub(crate) const fn kind(&self) -> &'static str {
        match self {
            Self::ListApplications => "list_applications",
            Self::OpenApplication { .. } => "open_application",
            Self::Attach { .. } => "attach",
            Self::Observe { .. } => "observe",
            Self::Click { .. } => "click",
            Self::Drag { .. } => "drag",
            Self::Scroll { .. } => "scroll",
            Self::PressKey { .. } => "press_key",
            Self::TypeText { .. } => "type_text",
            Self::SetValue { .. } => "set_value",
            Self::PerformAction { .. } => "perform_action",
            Self::Wait { .. } => "wait",
        }
    }
}

const fn default_true() -> bool {
    true
}

const fn default_drag_duration_ms() -> u64 {
    350
}

/// Result payload for one action.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ComputerOutput {
    /// Application and window discovery.
    Applications {
        applications: Vec<Application>,
        windows: Vec<Window>,
    },
    /// Fresh selected-application state.
    State { state: ComputerObservation },
    /// An application was launched; attach or observe it next.
    Opened { bundle_id: String },
    /// A non-observing operation completed.
    Done,
}

/// Timed result for one accepted action.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema)]
pub struct ComputerActionResult {
    /// Monotonic action sequence.
    pub sequence: u64,
    /// Wall-clock execution duration in milliseconds.
    pub elapsed_ms: u64,
    /// Typed output.
    pub output: ComputerOutput,
}

impl ComputerActionResult {
    pub(crate) fn image_paths(&self) -> impl Iterator<Item = &PathBuf> {
        let screenshot = match &self.output {
            ComputerOutput::State { state } => state.screenshot.as_ref(),
            _ => None,
        };
        screenshot.into_iter().map(|image| &image.path)
    }
}

/// Permission needed for a native operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Accessibility,
    ScreenRecording,
    InputMonitoring,
}

/// Why control was returned to the human.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InterventionReason {
    HumanInput,
    EscapeKey,
    Caller(String),
}

/// Live, ordered event independent from action results.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ComputerEvent {
    SessionStarted {
        session_id: String,
    },
    /// The bounded observer fell behind; subsequent events remain live.
    Lagged {
        skipped: u64,
    },
    TargetChanged {
        application: Application,
        window: Window,
    },
    ActionStarted {
        sequence: u64,
        action: ComputerAction,
    },
    Frame {
        sequence: u64,
        generation: u64,
        digest: String,
        phase: ComputerFramePhase,
    },
    ActionCompleted {
        result: ComputerActionResult,
    },
    PermissionRequired {
        permission: Permission,
        guidance: String,
    },
    Paused,
    Resumed,
    /// Physical input started targeting the attached application. Agent input
    /// yields until the quiet period elapses without changing sticky pause state.
    HumanActivityStarted {
        target_pid: i32,
        quiet_period_ms: u64,
    },
    /// The attached application has remained free of physical input long
    /// enough for queued actions to continue.
    HumanActivityEnded {
        target_pid: i32,
        requires_requery: bool,
    },
    UserIntervened {
        reason: InterventionReason,
    },
    Failed {
        sequence: Option<u64>,
        message: String,
    },
    Stopped,
}

/// Post-action settling policy.
#[derive(Clone, Copy, Debug)]
pub struct SettlePolicy {
    /// Delay between samples.
    pub sample_interval: Duration,
    /// Minimum time a mutation must remain visually stable and non-loading.
    pub minimum_duration: Duration,
    /// Maximum time spent waiting for two equal semantic/visual samples.
    pub timeout: Duration,
}

impl Default for SettlePolicy {
    fn default() -> Self {
        Self {
            sample_interval: Duration::from_millis(50),
            minimum_duration: Duration::from_millis(500),
            timeout: Duration::from_secs(5),
        }
    }
}
