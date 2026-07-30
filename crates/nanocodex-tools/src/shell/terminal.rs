use std::{
    collections::{HashMap, VecDeque},
    fmt, io,
    path::Path,
    sync::{
        Arc, Mutex as StdMutex, Weak,
        atomic::{AtomicI64, Ordering},
    },
};

use tokio::sync::broadcast;
use tracing::info;

use super::Session;

const EVENT_CAPACITY: usize = 256;
const MAX_ARCHIVED_TERMINALS: usize = 16;
const MAX_TERMINAL_OUTPUT_BYTES: usize = 1024 * 1024;

/// Opaque identity for one PTY owned by a tool runtime.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TerminalId(i64);

impl fmt::Display for TerminalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Character-cell dimensions for a PTY.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSize {
    /// Number of terminal rows.
    pub rows: u16,
    /// Number of terminal columns.
    pub columns: u16,
}

impl TerminalSize {
    /// Creates a character-cell terminal size.
    #[must_use]
    pub const fn new(rows: u16, columns: u16) -> Self {
        Self { rows, columns }
    }
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self::new(24, 80)
    }
}

/// Stable metadata for one attachable PTY.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct TerminalInfo {
    /// Runtime-scoped terminal identity.
    pub id: TerminalId,
    /// Provider identity of the `exec_command` call that opened the terminal.
    pub call_id: Arc<str>,
    /// Exact shell command supplied by the model.
    pub command: Arc<str>,
    /// Resolved working directory used to launch the command.
    pub working_directory: Arc<Path>,
    /// Initial or most recently requested terminal size.
    pub size: TerminalSize,
}

/// One push notification from the optional raw PTY stream.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum TerminalEvent {
    /// A model-requested command opened an attachable PTY.
    Opened(TerminalInfo),
    /// Exact output bytes were read from the PTY.
    Output {
        /// Terminal that produced the bytes.
        id: TerminalId,
        /// Absolute byte offset of the first byte in this chunk.
        offset: u64,
        /// Exact bytes in process order.
        bytes: Arc<[u8]>,
    },
    /// The PTY process exited after all available output was drained.
    Exited {
        /// Terminal that exited.
        id: TerminalId,
        /// Process exit status.
        exit_code: i32,
        /// Absolute offset immediately after the final observed output byte.
        output_end: u64,
    },
}

/// Bounded non-consuming raw output retained for attachment or lag recovery.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct TerminalSnapshot {
    /// Terminal metadata captured with this snapshot.
    pub info: TerminalInfo,
    /// Absolute offset of the first retained byte.
    pub output_start: u64,
    /// Absolute offset immediately after the last retained byte.
    pub output_end: u64,
    /// Retained exact PTY bytes.
    pub output: Arc<[u8]>,
    /// Exit status, or `None` while the process remains live.
    pub exit_code: Option<i32>,
}

impl TerminalSnapshot {
    /// Returns whether earlier output was discarded from the bounded snapshot.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.output_start != 0
    }
}

/// Failure while controlling or observing an application-facing PTY.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TerminalError {
    /// The terminal is not active and no retained snapshot exists.
    #[error("unknown terminal {0}")]
    Unknown(TerminalId),
    /// The terminal already exited and cannot accept more control operations.
    #[error("terminal {0} has exited")]
    Exited(TerminalId),
    /// A resize requested an empty character-cell dimension.
    #[error("terminal dimensions must be non-zero")]
    InvalidSize,
    /// Writing exact bytes to the PTY failed.
    #[error("failed to write to terminal {id}: {source}")]
    Write {
        /// Terminal that rejected the write.
        id: TerminalId,
        /// Underlying PTY failure.
        #[source]
        source: io::Error,
    },
    /// Resizing the PTY failed.
    #[error("failed to resize terminal {id}: {source}")]
    Resize {
        /// Terminal that rejected the resize.
        id: TerminalId,
        /// Underlying PTY failure.
        #[source]
        source: io::Error,
    },
}

/// Failure while receiving terminal lifecycle notifications.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum TerminalEventError {
    /// The receiver fell behind the bounded event stream.
    #[error("terminal event receiver lagged by {skipped} events")]
    Lagged {
        /// Number of notifications discarded for this receiver.
        skipped: u64,
    },
}

/// Receiving half of an optional, runtime-scoped terminal event stream.
pub struct TerminalEvents {
    receiver: broadcast::Receiver<TerminalEvent>,
}

impl TerminalEvents {
    /// Receives the next terminal event, `None` when all controls are dropped,
    /// or a lag error that can be recovered with [`TerminalControl::snapshots`].
    pub async fn recv(&mut self) -> Result<Option<TerminalEvent>, TerminalEventError> {
        match self.receiver.recv().await {
            Ok(event) => Ok(Some(event)),
            Err(broadcast::error::RecvError::Closed) => Ok(None),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                Err(TerminalEventError::Lagged { skipped })
            }
        }
    }
}

/// Cloneable control and subscription capability for PTYs owned by one runtime.
#[derive(Clone)]
pub struct TerminalControl {
    hub: Arc<TerminalHub>,
}

impl fmt::Debug for TerminalControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TerminalControl")
            .finish_non_exhaustive()
    }
}

impl TerminalControl {
    pub(crate) fn new() -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            hub: Arc::new(TerminalHub {
                state: StdMutex::new(TerminalState::default()),
                events,
                next_session_id: AtomicI64::new(1),
            }),
        }
    }

    /// Subscribes to PTYs opened after this call.
    ///
    /// Call this before starting a model turn. Existing live and recently
    /// completed terminals remain available through [`Self::snapshots`].
    #[must_use]
    pub fn subscribe(&self) -> TerminalEvents {
        TerminalEvents {
            receiver: self.hub.events.subscribe(),
        }
    }

    /// Returns bounded non-consuming snapshots for live and recently completed PTYs.
    #[must_use]
    pub fn snapshots(&self) -> Vec<TerminalSnapshot> {
        self.hub.snapshots()
    }

    /// Returns one bounded non-consuming PTY snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError::Unknown`] after an unknown or expired identity.
    pub fn snapshot(&self, id: TerminalId) -> Result<TerminalSnapshot, TerminalError> {
        self.hub.snapshot(id)
    }

    /// Writes exact bytes directly to a live PTY.
    ///
    /// Input is not added to model history. It is emitted verbatim through
    /// tracing and may contain credentials or other sensitive values.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown or completed terminal or a PTY write failure.
    pub async fn write(
        &self,
        id: TerminalId,
        input: impl AsRef<[u8]>,
    ) -> Result<(), TerminalError> {
        let input = input.as_ref();
        trace_terminal_input(id, input);
        let session = self.hub.live_session(id)?;
        session
            .write_bytes(input)
            .await
            .map_err(|source| TerminalError::Write { id, source })
    }

    /// Changes the character-cell dimensions of a live PTY.
    ///
    /// # Errors
    ///
    /// Returns an error for zero dimensions, an unknown or completed terminal,
    /// or an operating-system resize failure.
    pub async fn resize(&self, id: TerminalId, size: TerminalSize) -> Result<(), TerminalError> {
        if size.rows == 0 || size.columns == 0 {
            return Err(TerminalError::InvalidSize);
        }
        let session = self.hub.live_session(id)?;
        session
            .resize(size)
            .await
            .map_err(|source| TerminalError::Resize { id, source })?;
        self.hub.record_size(id, size);
        Ok(())
    }

    pub(super) fn register(&self, session: &Arc<Session>, info: TerminalInfo) {
        self.hub.register(session, info);
    }

    pub(super) fn next_session_id(&self) -> i64 {
        self.hub.next_session_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(super) fn opened(&self, id: TerminalId) {
        self.hub.opened(id);
    }

    pub(super) fn output(&self, id: TerminalId, bytes: &[u8]) {
        self.hub.output(id, bytes);
    }

    pub(super) fn exited(&self, id: TerminalId, exit_code: i32) {
        self.hub.exited(id, exit_code);
    }
}

fn trace_terminal_input(id: TerminalId, input: &[u8]) {
    info!(
        target: "nanocodex_tools",
        terminal_id = %id,
        input_bytes = input.len(),
        sensitive = true,
        terminal.input = ?input,
        "writing human input to tool terminal"
    );
}

struct TerminalHub {
    state: StdMutex<TerminalState>,
    events: broadcast::Sender<TerminalEvent>,
    next_session_id: AtomicI64,
}

#[derive(Default)]
struct TerminalState {
    active: HashMap<TerminalId, ActiveTerminal>,
    archives: VecDeque<TerminalSnapshot>,
}

struct ActiveTerminal {
    info: TerminalInfo,
    session: Weak<Session>,
    output: RetainedOutput,
}

#[derive(Default)]
struct RetainedOutput {
    bytes: VecDeque<u8>,
    start: u64,
    end: u64,
}

impl RetainedOutput {
    fn push(&mut self, bytes: &[u8]) -> u64 {
        let offset = self.end;
        self.end = self
            .end
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        self.bytes.extend(bytes);
        let overflow = self.bytes.len().saturating_sub(MAX_TERMINAL_OUTPUT_BYTES);
        self.bytes.drain(..overflow);
        self.start = self
            .start
            .saturating_add(u64::try_from(overflow).unwrap_or(u64::MAX));
        offset
    }

    fn snapshot(&self, info: TerminalInfo, exit_code: Option<i32>) -> TerminalSnapshot {
        TerminalSnapshot {
            info,
            output_start: self.start,
            output_end: self.end,
            output: self.bytes.iter().copied().collect::<Vec<_>>().into(),
            exit_code,
        }
    }
}

impl TerminalHub {
    fn state(&self) -> std::sync::MutexGuard<'_, TerminalState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn register(&self, session: &Arc<Session>, info: TerminalInfo) {
        self.state().active.insert(
            info.id,
            ActiveTerminal {
                info,
                session: Arc::downgrade(session),
                output: RetainedOutput::default(),
            },
        );
    }

    fn opened(&self, id: TerminalId) {
        let info = self
            .state()
            .active
            .get(&id)
            .map(|active| active.info.clone());
        if let Some(info) = info {
            drop(self.events.send(TerminalEvent::Opened(info)));
        }
    }

    fn output(&self, id: TerminalId, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let offset = {
            let mut state = self.state();
            let Some(active) = state.active.get_mut(&id) else {
                return;
            };
            active.output.push(bytes)
        };
        info!(
            target: "nanocodex_tools",
            terminal_id = %id,
            output_offset = offset,
            output_bytes = bytes.len(),
            terminal.output = ?bytes,
            "observed tool terminal output"
        );
        drop(self.events.send(TerminalEvent::Output {
            id,
            offset,
            bytes: Arc::from(bytes),
        }));
    }

    fn exited(&self, id: TerminalId, exit_code: i32) {
        let snapshot = {
            let mut state = self.state();
            let Some(active) = state.active.remove(&id) else {
                return;
            };
            let snapshot = active.output.snapshot(active.info, Some(exit_code));
            state.archives.push_back(snapshot.clone());
            while state.archives.len() > MAX_ARCHIVED_TERMINALS {
                state.archives.pop_front();
            }
            snapshot
        };
        drop(self.events.send(TerminalEvent::Exited {
            id,
            exit_code,
            output_end: snapshot.output_end,
        }));
    }

    fn live_session(&self, id: TerminalId) -> Result<Arc<Session>, TerminalError> {
        let state = self.state();
        if let Some(active) = state.active.get(&id) {
            return active.session.upgrade().ok_or(TerminalError::Exited(id));
        }
        if state.archives.iter().any(|snapshot| snapshot.info.id == id) {
            return Err(TerminalError::Exited(id));
        }
        Err(TerminalError::Unknown(id))
    }

    fn record_size(&self, id: TerminalId, size: TerminalSize) {
        if let Some(active) = self.state().active.get_mut(&id) {
            active.info.size = size;
        }
    }

    fn snapshots(&self) -> Vec<TerminalSnapshot> {
        let state = self.state();
        let mut snapshots = state.archives.iter().cloned().collect::<Vec<_>>();
        snapshots.extend(
            state
                .active
                .values()
                .map(|active| active.output.snapshot(active.info.clone(), None)),
        );
        snapshots.sort_unstable_by_key(|snapshot| snapshot.info.id);
        snapshots
    }

    fn snapshot(&self, id: TerminalId) -> Result<TerminalSnapshot, TerminalError> {
        let state = self.state();
        if let Some(active) = state.active.get(&id) {
            return Ok(active.output.snapshot(active.info.clone(), None));
        }
        state
            .archives
            .iter()
            .find(|snapshot| snapshot.info.id == id)
            .cloned()
            .ok_or(TerminalError::Unknown(id))
    }
}

impl TerminalId {
    pub(super) const fn new(id: i64) -> Self {
        Self(id)
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write, sync::Arc};

    use super::{
        MAX_TERMINAL_OUTPUT_BYTES, RetainedOutput, StdMutex, TerminalId, trace_terminal_input,
    };

    struct TraceWriter(Arc<StdMutex<Vec<u8>>>);

    impl Write for TraceWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn tracing_preserves_exact_human_input_bytes() {
        let captured = Arc::new(StdMutex::new(Vec::new()));
        let writer = Arc::clone(&captured);
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || TraceWriter(Arc::clone(&writer)))
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);

        tracing::dispatcher::with_default(&dispatch, || {
            trace_terminal_input(TerminalId::new(7), b"swordfish\n");
        });

        let output = String::from_utf8(
            captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )
        .expect("trace output should be UTF-8");
        assert!(output.contains("terminal_id=7"));
        assert!(output.contains("input_bytes=10"));
        assert!(
            output.contains("terminal.input=[115, 119, 111, 114, 100, 102, 105, 115, 104, 10]"),
            "{output}"
        );
    }

    #[test]
    fn retained_output_reports_the_absolute_window_after_truncation() {
        let mut output = RetainedOutput::default();
        let bytes = vec![b'x'; MAX_TERMINAL_OUTPUT_BYTES + 7];

        assert_eq!(output.push(&bytes), 0);
        assert_eq!(output.start, 7);
        assert_eq!(
            output.end,
            u64::try_from(MAX_TERMINAL_OUTPUT_BYTES + 7).unwrap()
        );
        assert_eq!(output.bytes.len(), MAX_TERMINAL_OUTPUT_BYTES);
        assert_eq!(output.push(b"tail"), output.end - 4);
        assert_eq!(output.start, 11);
    }
}
