use std::path::PathBuf;

use crate::Permission;

/// Failure configuring a computer-use session.
#[derive(Debug, thiserror::Error)]
pub enum ComputerBuildError {
    /// The runtime artifact directory could not be created.
    #[error("failed to create computer-use artifact directory {path}: {source}")]
    ArtifactDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A builder value was invalid.
    #[error("invalid computer-use configuration: {message}")]
    Configuration { message: String },
    /// No Tokio runtime was active while building the actor-backed session.
    #[error("computer-use sessions must be built inside a Tokio runtime")]
    Runtime,
}

/// Native action or lifecycle failure.
#[derive(Debug, thiserror::Error)]
pub enum ComputerError {
    /// The platform has no native implementation.
    #[error("native computer use is unsupported on {platform}")]
    Unsupported { platform: &'static str },
    /// A required macOS privacy permission has not been granted.
    #[error("{permission:?} permission is required: {guidance}")]
    Permission {
        permission: Permission,
        guidance: String,
    },
    /// No application/window has been attached.
    #[error("no target is attached; run list_applications and attach first")]
    NoTarget,
    /// The desktop is locked; native observation and input fail closed.
    #[error("the macOS desktop is locked; unlock it manually before using computer control")]
    ScreenLocked,
    /// The embedding application's allowlist excludes the requested app.
    #[error("application {application} is not authorized for this computer session")]
    ApplicationDenied { application: String },
    /// The attached browser exposed a URL outside caller-owned policy.
    #[error("computer use stopped after encountering a disallowed URL: {url}")]
    UrlDenied { url: String },
    /// An application or window could not be found.
    #[error("target not found: {message}")]
    TargetNotFound { message: String },
    /// An element reference belongs to an old state generation.
    #[error("stale element reference {reference}; observe again and use a fresh reference")]
    StaleReference { reference: String },
    /// Physical human input invalidated the state used to choose an action.
    #[error(
        "human input changed the attached application; call observe before the next mutating action"
    )]
    RequeryRequired,
    /// The action was rejected while the human owns control.
    #[error("computer use is paused; resume it through ComputerControl")]
    Paused,
    /// The session has been stopped.
    #[error("computer-use session is stopped")]
    Stopped,
    /// A requested key name is unknown.
    #[error("unknown key {key:?}")]
    UnknownKey { key: String },
    /// An action argument was invalid.
    #[error("invalid computer action: {message}")]
    InvalidAction { message: String },
    /// A native framework rejected the operation.
    #[error("native computer operation failed: {message}")]
    Native { message: String },
    /// Artifact I/O failed.
    #[error("computer artifact I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The private driver exited unexpectedly.
    #[error("computer-use driver exited")]
    DriverExited,
}
