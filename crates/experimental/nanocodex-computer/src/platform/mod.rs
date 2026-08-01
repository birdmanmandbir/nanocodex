use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;

use crate::{ComputerAction, ComputerError, ComputerOutput, SettlePolicy, driver::RunState};

#[cfg(target_os = "macos")]
pub(crate) type InterventionMonitor = nanocodex_computer_macos::HumanInputMonitor;
#[cfg(not(target_os = "macos"))]
pub(crate) struct InterventionMonitor;

#[cfg(target_os = "macos")]
mod macos;

#[async_trait]
pub(crate) trait Backend: Send {
    async fn execute(
        &mut self,
        action: ComputerAction,
        sequence: u64,
        state: Arc<RunState>,
    ) -> Result<ComputerOutput, ComputerError>;
}

pub(crate) fn native(
    artifact_root: PathBuf,
    settle: SettlePolicy,
    maximum_elements: usize,
) -> Box<dyn Backend> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacosBackend::new(
            artifact_root,
            settle,
            maximum_elements,
        ))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (artifact_root, settle, maximum_elements);
        Box::new(UnsupportedBackend)
    }
}

pub(crate) fn intervention_monitor(
    callback: impl Fn() + Send + Sync + 'static,
) -> Result<InterventionMonitor, &'static str> {
    #[cfg(target_os = "macos")]
    {
        nanocodex_computer_macos::HumanInputMonitor::spawn(callback)
            .map_err(|_| "event tap unavailable")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = callback;
        Err("event tap unavailable")
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
    ) -> Result<ComputerOutput, ComputerError> {
        Err(ComputerError::Unsupported {
            platform: std::env::consts::OS,
        })
    }
}
