use crate::{Computer, ComputerError};

/// In-process, non-activating native floating renderer for live computer frames.
pub struct ComputerPip {
    task: tokio::task::AbortHandle,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    _computer: Computer,
}

impl ComputerPip {
    /// Opens the renderer on the macOS main thread and drives it from the
    /// coalescing frame stream. The caller must run inside a Tokio `LocalSet`
    /// hosted on the process main thread.
    #[cfg(target_os = "macos")]
    pub async fn spawn(computer: &Computer) -> Result<Self, ComputerError> {
        let mut frames = computer.frames();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let task = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tokio::task::spawn_local(async move {
                let Ok(window) = nanocodex_computer_macos::NativePipWindow::new() else {
                    let _ = ready_tx.send(false);
                    return;
                };
                if let Some(frame) = frames.latest()
                    && window.update_png(frame.image.png()).is_err()
                {
                    let _ = ready_tx.send(false);
                    return;
                }
                tracing::info!(target: "nanocodex_computer", "native computer PIP host ready");
                let _ = ready_tx.send(true);
                let mut pump = tokio::time::interval(std::time::Duration::from_millis(16));
                loop {
                    tokio::select! {
                        _ = pump.tick() => {
                            if window.pump().is_err() {
                                tracing::warn!(target: "nanocodex_computer", "native computer PIP event pump failed");
                                break;
                            }
                        }
                        frame = frames.changed() => {
                            let Ok(frame) = frame else { break };
                            if window.update_png(frame.image.png()).is_err() {
                                tracing::warn!(target: "nanocodex_computer", generation = frame.generation, "native computer PIP frame rendering failed");
                                break;
                            }
                        }
                        _ = &mut stop_rx => break,
                    }
                }
                let _ = window.hide();
            })
        }))
        .map_err(|_| ComputerError::Native {
            message: "native computer PIP requires a Tokio LocalSet on the macOS main thread"
                .to_owned(),
        })?;
        if !ready_rx.await.unwrap_or(false) {
            task.abort();
            return Err(ComputerError::Native {
                message: "failed to create native computer PIP on the macOS main thread".to_owned(),
            });
        }
        Ok(Self {
            task: task.abort_handle(),
            stop: Some(stop_tx),
            _computer: computer.clone(),
        })
    }

    /// Returns unsupported on non-macOS targets.
    #[cfg(not(target_os = "macos"))]
    pub async fn spawn(_computer: &Computer) -> Result<Self, ComputerError> {
        Err(ComputerError::Unsupported {
            platform: std::env::consts::OS,
        })
    }

    /// Closes the floating renderer without stopping computer actions.
    pub fn close(self) {}
}

impl Drop for ComputerPip {
    fn drop(&mut self) {
        if self.stop.take().is_none_or(|stop| stop.send(()).is_err()) {
            self.task.abort();
        }
    }
}
