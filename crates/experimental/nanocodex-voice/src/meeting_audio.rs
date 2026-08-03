use std::fmt::Display;
#[cfg(target_os = "macos")]
use std::process::Stdio;

use cpal::{
    Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use nanocodex::oai::realtime::RealtimeAudio;
use tokio::sync::mpsc;
#[cfg(target_os = "macos")]
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    task::JoinHandle,
};

#[cfg(target_os = "macos")]
use crate::audio::Microphone;
use crate::{AudioError, audio::build_input};

const AUDIO_QUEUE_FRAMES: usize = 8;
#[cfg(target_os = "macos")]
const SYSTEM_READ_BYTES: usize = 480 * size_of::<f32>();

pub(super) struct MicrophoneCapture {
    _stream: Stream,
}

impl MicrophoneCapture {
    pub(super) fn open() -> Result<(Self, mpsc::Receiver<RealtimeAudio>), AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or(AudioError::NoInputDevice)?;
        let supported = device
            .default_input_config()
            .map_err(|error| backend("failed to read the default microphone format", error))?;
        let config: StreamConfig = supported.clone().into();
        let (sender, receiver) = mpsc::channel(AUDIO_QUEUE_FRAMES);
        let stream = build_input(&device, supported.sample_format(), config, sender)?;
        stream
            .play()
            .map_err(|error| backend("failed to start the microphone", error))?;
        Ok((Self { _stream: stream }, receiver))
    }
}

#[cfg(target_os = "macos")]
pub(super) struct SystemCapture {
    _directory: tempfile::TempDir,
    child: Child,
    reader: JoinHandle<()>,
    diagnostics: JoinHandle<()>,
}

#[cfg(target_os = "macos")]
impl SystemCapture {
    pub(super) async fn open() -> Result<(Self, mpsc::Receiver<RealtimeAudio>), AudioError> {
        let executable =
            include_bytes!(concat!(env!("OUT_DIR"), "/nanocodex-system-audio-capture"));
        let directory = tempfile::Builder::new()
            .prefix("nanocodex-system-audio-")
            .tempdir()
            .map_err(|error| backend("failed to stage the system-audio helper", error))?;
        let helper = directory.path().join("capture");
        std::fs::write(&helper, executable)
            .map_err(|error| backend("failed to install the system-audio helper", error))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| backend("failed to make the system-audio helper executable", error))?;

        let mut child = Command::new(&helper)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| backend("failed to start macOS system-audio capture", error))?;
        let mut stdout = child.stdout.take().ok_or_else(|| AudioError::Backend {
            operation: "failed to start macOS system-audio capture",
            message: "capture helper did not expose stdout".to_owned(),
        })?;
        let mut stderr = child.stderr.take().ok_or_else(|| AudioError::Backend {
            operation: "failed to start macOS system-audio capture",
            message: "capture helper did not expose stderr".to_owned(),
        })?;
        let (sender, receiver) = mpsc::channel(AUDIO_QUEUE_FRAMES);
        let reader =
            tokio::spawn(async move {
                let mut capture = Microphone::new(24_000, 1, sender);
                let mut bytes = vec![0_u8; SYSTEM_READ_BYTES];
                let mut pending = Vec::with_capacity(SYSTEM_READ_BYTES * 2);
                loop {
                    let read = match stdout.read(&mut bytes).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => read,
                    };
                    pending.extend_from_slice(&bytes[..read]);
                    let complete = pending.len() / size_of::<f32>() * size_of::<f32>();
                    capture.push(pending[..complete].chunks_exact(size_of::<f32>()).map(
                        |sample| f32::from_ne_bytes([sample[0], sample[1], sample[2], sample[3]]),
                    ));
                    pending.drain(..complete);
                }
            });
        let diagnostics = tokio::spawn(async move {
            let mut message = String::new();
            if stderr.read_to_string(&mut message).await.is_ok() && !message.trim().is_empty() {
                tracing::warn!(message = message.trim(), "system-audio helper stopped");
            }
        });
        Ok((
            Self {
                _directory: directory,
                child,
                reader,
                diagnostics,
            },
            receiver,
        ))
    }
}

#[cfg(target_os = "macos")]
impl Drop for SystemCapture {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        self.reader.abort();
        self.diagnostics.abort();
    }
}

#[cfg(not(target_os = "macos"))]
pub(super) struct SystemCapture;

#[cfg(not(target_os = "macos"))]
impl SystemCapture {
    pub(super) async fn open() -> Result<(Self, mpsc::Receiver<RealtimeAudio>), AudioError> {
        Err(AudioError::SystemAudioUnsupported)
    }
}

fn backend(operation: &'static str, error: impl Display) -> AudioError {
    AudioError::Backend {
        operation,
        message: error.to_string(),
    }
}
