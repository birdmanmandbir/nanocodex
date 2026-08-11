use crate::{Computer, ComputerError};

#[cfg(target_os = "macos")]
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
use nanocodex_computer_macos::{
    NativeImageData, capture_window_in_process, window as native_window,
};
#[cfg(target_os = "macos")]
const LIVE_CAPTURE_INTERVAL: Duration = Duration::from_millis(250);

#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
struct PipSource {
    window_id: u32,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    on_screen: bool,
}

#[cfg(target_os = "macos")]
struct LivePipFrame {
    source: PipSource,
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

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
        let mut pointers = computer.pointers();
        let (source_tx, source_rx) = tokio::sync::watch::channel(None);
        let (live_tx, mut live_rx) = tokio::sync::watch::channel(None);
        let live_task = tokio::spawn(capture_live_frames(source_rx, live_tx));
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let task = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tokio::task::spawn_local(async move {
                let Ok(window) = nanocodex_computer_macos::NativePipWindow::new() else {
                    let _ = ready_tx.send(false);
                    return;
                };
                if let Some(frame) = frames.latest() {
                    let source = pip_source(&frame);
                    let _ = source_tx.send(Some(source));
                    window.set_source_frame(
                        source.window_id,
                        source.x,
                        source.y,
                        source.width,
                        source.height,
                        source.on_screen,
                    );
                    if window.update_png(frame.image.png()).is_err() {
                        let _ = ready_tx.send(false);
                        return;
                    }
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
                            let source = pip_source(&frame);
                            let _ = source_tx.send(Some(source));
                            window.set_source_frame(
                                source.window_id,
                                source.x,
                                source.y,
                                source.width,
                                source.height,
                                source.on_screen,
                            );
                            if window.update_png(frame.image.png()).is_err() {
                                tracing::warn!(target: "nanocodex_computer", generation = frame.generation, "native computer PIP frame rendering failed");
                                break;
                            }
                        }
                        changed = live_rx.changed() => {
                            if changed.is_err() {
                                break;
                            }
                            let Some(frame) = live_rx.borrow_and_update().clone() else {
                                continue;
                            };
                            let source = frame.source;
                            window.set_source_frame(
                                source.window_id,
                                source.x,
                                source.y,
                                source.width,
                                source.height,
                                source.on_screen,
                            );
                            if window.update_rgba(&frame.rgba, frame.width, frame.height).is_err() {
                                tracing::warn!(target: "nanocodex_computer", window_id = source.window_id, "native computer PIP live frame rendering failed");
                                break;
                            }
                        }
                        changed = pointers.changed() => {
                            if changed.is_err() {
                                break;
                            }
                            let Some(pointer) = *pointers.borrow_and_update() else {
                                continue;
                            };
                            window.set_agent_cursor(
                                pointer.point.x,
                                pointer.point.y,
                                pointer.pressed,
                                pointer.travel_duration,
                            );
                        }
                        _ = &mut stop_rx => break,
                    }
                }
                live_task.abort();
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

#[cfg(target_os = "macos")]
const fn pip_source(frame: &crate::ComputerFrame) -> PipSource {
    let bounds = frame.window.frame;
    PipSource {
        window_id: frame.window.id,
        x: bounds.x,
        y: bounds.y,
        width: bounds.width,
        height: bounds.height,
        on_screen: frame.window.on_screen,
    }
}

#[cfg(target_os = "macos")]
async fn capture_live_frames(
    mut source_rx: tokio::sync::watch::Receiver<Option<PipSource>>,
    live_tx: tokio::sync::watch::Sender<Option<Arc<LivePipFrame>>>,
) {
    let mut interval = tokio::time::interval(LIVE_CAPTURE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut unavailable_window = None;
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let Some(source) = *source_rx.borrow_and_update() else {
                    continue;
                };
                let started = Instant::now();
                let captured = tokio::task::spawn_blocking(move || {
                    capture_live_frame(source.window_id)
                }).await;
                match captured {
                    Ok(Ok(frame)) => {
                        let duration_ns = elapsed_ns(started.elapsed());
                        tracing::debug!(
                            target: "nanocodex_computer",
                            window_id = frame.source.window_id,
                            bytes = frame.rgba.len(),
                            width = frame.width,
                            height = frame.height,
                            duration_ns,
                            "captured native computer PIP frame"
                        );
                        unavailable_window = None;
                        if live_tx.send(Some(Arc::new(frame))).is_err() {
                            break;
                        }
                    }
                    Ok(Err(message)) => {
                        if unavailable_window != Some(source.window_id) {
                            tracing::warn!(target: "nanocodex_computer", window_id = source.window_id, %message, "native computer PIP live capture unavailable; retaining the latest agent frame");
                            unavailable_window = Some(source.window_id);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(target: "nanocodex_computer", %error, "native computer PIP capture worker panicked");
                        break;
                    }
                }
            }
            changed = source_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                interval.reset_immediately();
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn capture_live_frame(window_id: u32) -> Result<LivePipFrame, String> {
    let window = native_window(window_id)
        .ok_or_else(|| format!("window {window_id} is no longer available"))?;
    let image = capture_window_in_process(window_id).map_err(|error| error.to_string())?;
    let rgba = match image.data {
        NativeImageData::Rgba(rgba) => rgba,
        NativeImageData::Png(_) => {
            return Err("in-process PIP capture unexpectedly returned encoded data".to_owned());
        }
    };
    Ok(LivePipFrame {
        source: PipSource {
            window_id,
            x: window.x,
            y: window.y,
            width: window.width,
            height: window.height,
            on_screen: window.on_screen,
        },
        rgba,
        width: image.width,
        height: image.height,
    })
}

#[cfg(target_os = "macos")]
fn elapsed_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

impl Drop for ComputerPip {
    fn drop(&mut self) {
        if self.stop.take().is_none_or(|stop| stop.send(()).is_err()) {
            self.task.abort();
        }
    }
}
