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
}
