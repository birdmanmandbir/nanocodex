use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use super::{MeetingEvent, MeetingFailure};

pub(super) async fn run(
    _model: Arc<str>,
    _events: &mpsc::UnboundedSender<MeetingEvent>,
    _stopped: oneshot::Receiver<()>,
) -> Result<(), MeetingFailure> {
    Err(MeetingFailure::Mlx(
        "requires Apple Silicon macOS and a build with the `meeting-mlx` feature".to_owned(),
    ))
}
