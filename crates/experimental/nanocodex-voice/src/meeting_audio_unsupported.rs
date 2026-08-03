use nanocodex::oai::realtime::RealtimeAudio;
use tokio::sync::mpsc;

use crate::AudioError;

pub(super) struct MicrophoneCapture;

impl MicrophoneCapture {
    pub(super) fn open() -> Result<(Self, mpsc::Receiver<RealtimeAudio>), AudioError> {
        Err(AudioError::UnsupportedPlatform)
    }
}

pub(super) struct SystemCapture;

impl SystemCapture {
    pub(super) async fn open() -> Result<(Self, mpsc::Receiver<RealtimeAudio>), AudioError> {
        Err(AudioError::UnsupportedPlatform)
    }

    pub(super) async fn stopped_reason(&mut self) -> String {
        "system-audio capture stopped".to_owned()
    }
}
