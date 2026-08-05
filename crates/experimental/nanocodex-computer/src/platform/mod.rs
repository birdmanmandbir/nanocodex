use std::{collections::HashSet, path::PathBuf, sync::Arc};

use async_trait::async_trait;

use crate::{
    ComputerAction, ComputerError, ComputerOutput, SettlePolicy,
    driver::{FrameSink, RunState},
};

#[cfg(target_os = "macos")]
pub(crate) type InterventionMonitor = nanocodex_computer_macos::HumanInputMonitor;
#[cfg(target_os = "macos")]
pub(crate) type InterventionTarget = nanocodex_computer_macos::HumanInputTarget;
#[cfg(target_os = "macos")]
pub(crate) type InterventionEvent = nanocodex_computer_macos::HumanInputEvent;
#[cfg(not(target_os = "macos"))]
pub(crate) struct InterventionMonitor;
#[cfg(not(target_os = "macos"))]
pub(crate) struct InterventionTarget;
#[cfg(not(target_os = "macos"))]
pub(crate) struct InterventionEvent {
    pub(crate) kind: &'static str,
    pub(crate) source_pid: i64,
    pub(crate) target_pid: i32,
    pub(crate) x: f64,
    pub(crate) y: f64,
}

#[cfg(target_os = "macos")]
mod macos;

#[async_trait]
pub(crate) trait Backend: Send {
    async fn execute(
        &mut self,
        action: ComputerAction,
        sequence: u64,
        state: Arc<RunState>,
        frames: &mut FrameSink,
    ) -> Result<ComputerOutput, ComputerError>;
}

pub(crate) fn native(
    artifact_root: PathBuf,
    settle: SettlePolicy,
    maximum_elements: usize,
    intervention_target: Arc<InterventionTarget>,
    allowed_bundle_ids: Option<HashSet<String>>,
    allowed_url_origins: Option<Vec<url::Url>>,
) -> Box<dyn Backend> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacosBackend::new(
            artifact_root,
            settle,
            maximum_elements,
            intervention_target,
            allowed_bundle_ids,
            allowed_url_origins,
        ))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (
            artifact_root,
            settle,
            maximum_elements,
            intervention_target,
            allowed_bundle_ids,
            allowed_url_origins,
        );
        Box::new(UnsupportedBackend)
    }
}

pub(crate) fn intervention_monitor(
    target: Arc<InterventionTarget>,
    callback: impl Fn(InterventionEvent) + Send + Sync + 'static,
) -> Result<InterventionMonitor, &'static str> {
    #[cfg(target_os = "macos")]
    {
        nanocodex_computer_macos::HumanInputMonitor::spawn(target, callback)
            .map_err(|_| "event tap unavailable")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (target, callback);
        Err("event tap unavailable")
    }
}

pub(crate) fn intervention_target() -> Arc<InterventionTarget> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(InterventionTarget::default())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(InterventionTarget)
    }
}

#[cfg(not(target_os = "macos"))]
struct UnsupportedBackend;

#[cfg(not(target_os = "macos"))]
#[async_trait]
impl Backend for UnsupportedBackend {
    async fn execute(
        &mut self,
        _action: ComputerAction,
        _sequence: u64,
        _state: Arc<RunState>,
        _frames: &mut FrameSink,
    ) -> Result<ComputerOutput, ComputerError> {
        Err(ComputerError::Unsupported {
            platform: std::env::consts::OS,
        })
    }
}
