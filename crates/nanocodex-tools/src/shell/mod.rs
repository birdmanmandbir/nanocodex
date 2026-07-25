mod output;
mod process;
mod selection;
mod tool;

pub(crate) use tool::{ExecCommandHandler, WriteStdinHandler};

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde::Serialize;
use tokio::{sync::Mutex, task::JoinHandle, time::timeout};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalSessionInfo {
    pub id: i64,
    pub command: String,
}

#[derive(Debug, thiserror::Error)]
pub enum TerminalInputError {
    #[error("unknown or completed terminal session {0}")]
    UnknownSession(i64),
    #[error("exec session {0} is not attached to a terminal")]
    NotATerminal(i64),
    #[error("failed to write to terminal session {session_id}: {source}")]
    Write {
        session_id: i64,
        #[source]
        source: std::io::Error,
    },
}

const DEFAULT_EXEC_YIELD_MS: u64 = 10_000;
const DEFAULT_WRITE_YIELD_MS: u64 = 250;
const DEFAULT_POLL_YIELD_MS: u64 = 5_000;
const DRAIN_GRACE: Duration = Duration::from_secs(2);
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
const MAX_TERMINAL_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_LIVE_SESSIONS: usize = 64;
const MAX_TERMINAL_ARCHIVES: usize = 16;

pub(crate) struct ExecCommand {
    script: String,
    workdir: Option<String>,
    shell: Option<String>,
    login: Option<bool>,
    tty: bool,
    interactive: bool,
    yield_time_ms: Option<i64>,
    max_output_tokens: Option<i64>,
}

impl ExecCommand {
    pub(crate) const fn new(
        script: String,
        workdir: Option<String>,
        shell: Option<String>,
        login: Option<bool>,
        tty: bool,
        yield_time_ms: Option<i64>,
        max_output_tokens: Option<i64>,
    ) -> Self {
        Self {
            script,
            workdir,
            shell,
            login,
            tty,
            interactive: false,
            yield_time_ms,
            max_output_tokens,
        }
    }

    pub(crate) const fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }
}

pub(crate) struct WriteStdin {
    session_id: i64,
    chars: String,
    yield_time_ms: Option<i64>,
    max_output_tokens: Option<i64>,
}

impl WriteStdin {
    pub(crate) const fn new(
        session_id: i64,
        chars: String,
        yield_time_ms: Option<i64>,
        max_output_tokens: Option<i64>,
    ) -> Self {
        Self {
            session_id,
            chars,
            yield_time_ms,
            max_output_tokens,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct ExecCommandResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    chunk_id: Option<String>,
    wall_time_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_token_count: Option<usize>,
    output: String,
}

pub(crate) struct ShellSessions {
    sessions: Mutex<SessionStore>,
    next_session_id: AtomicI64,
    default_shell: selection::Shell,
}

impl ShellSessions {
    pub(crate) fn new() -> Self {
        Self {
            sessions: Mutex::new(SessionStore::default()),
            next_session_id: AtomicI64::new(1),
            default_shell: selection::default_user_shell(),
        }
    }

    pub(crate) const fn default_shell_name(&self) -> &'static str {
        self.default_shell.name()
    }

    pub(crate) async fn execute(
        &self,
        command: ExecCommand,
        workspace: &Path,
    ) -> ExecCommandResult {
        let started_at = Instant::now();
        let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed);
        let workdir = resolve_workdir(workspace, command.workdir.as_deref());
        let shell = command.shell.as_deref().map_or_else(
            || self.default_shell.clone(),
            selection::get_shell_by_model_provided_path,
        );
        let (environment, secrets) = process::sanitized_environment();
        let spawned = match process::spawn(
            &command.script,
            &workdir,
            &shell,
            command.login.unwrap_or(true),
            command.tty || command.interactive,
            &environment,
        ) {
            Ok(spawned) => spawned,
            Err(error) => {
                return ExecCommandResult::failed(
                    started_at.elapsed(),
                    format!("failed to spawn {}: {error}", shell.path().display()),
                );
            }
        };
        let session = Session::new(
            session_id,
            spawned,
            secrets,
            command.script,
            command.interactive,
        );
        let _interaction_guard = session.interaction.lock().await;
        let pruned = self.sessions.lock().await.insert(Arc::clone(&session));
        if let Some(pruned) = pruned {
            pruned.terminate().await;
        }

        let default_yield_ms = if command.interactive {
            DEFAULT_WRITE_YIELD_MS
        } else {
            DEFAULT_EXEC_YIELD_MS
        };
        let yield_time = duration_ms(command.yield_time_ms, default_yield_ms, 250, 30_000);
        let _interaction = session.begin_interaction();
        let result = session
            .wait_for_output(yield_time, command.max_output_tokens, started_at)
            .await;
        self.retire_completed(&session, &result).await;
        result
    }

    pub(crate) async fn write_stdin(&self, request: WriteStdin) -> ExecCommandResult {
        let started_at = Instant::now();
        let session = self.sessions.lock().await.get(request.session_id);
        let Some(session) = session else {
            return ExecCommandResult::failed(
                started_at.elapsed(),
                format!("unknown or completed exec session {}", request.session_id),
            );
        };

        let _interaction_guard = session.interaction.lock().await;
        let _interaction = session.begin_interaction();
        if !request.chars.is_empty() {
            let written = if request.chars == "\u{3}" && !session.tty {
                session.interrupt().await
            } else {
                session.write(&request.chars).await
            };
            if let Err(error) = written {
                return ExecCommandResult::failed(
                    started_at.elapsed(),
                    format!(
                        "failed to write to exec session {}: {error}",
                        request.session_id
                    ),
                );
            }
        }
        let (default, minimum, maximum) = if request.chars.is_empty() {
            (DEFAULT_POLL_YIELD_MS, 5_000, 300_000)
        } else {
            (DEFAULT_WRITE_YIELD_MS, 250, 30_000)
        };
        let yield_time = duration_ms(request.yield_time_ms, default, minimum, maximum);
        let result = session
            .wait_for_output(yield_time, request.max_output_tokens, started_at)
            .await;
        self.retire_completed(&session, &result).await;
        result
    }

    async fn retire_completed(&self, session: &Session, result: &ExecCommandResult) {
        if result.exit_code.is_none() {
            return;
        }
        let archive = session.tty.then(|| session.terminal_output());
        let archive = match archive {
            Some(output) => Some(output.await),
            None => None,
        };
        let mut store = self.sessions.lock().await;
        if let Some(output) = archive {
            store.archive_terminal(session.id, output);
        }
        store.remove(session.id);
    }

    pub(crate) async fn terminal_sessions(&self) -> Vec<TerminalSessionInfo> {
        let store = self.sessions.lock().await;
        let mut sessions = store
            .sessions
            .values()
            .filter(|session| session.tty && session.interactive)
            .map(|session| TerminalSessionInfo {
                id: session.id,
                command: session.command.clone(),
            })
            .collect::<Vec<_>>();
        sessions.sort_unstable_by_key(|session| session.id);
        sessions
    }

    pub(crate) async fn write_terminal(
        &self,
        session_id: i64,
        input: &[u8],
    ) -> Result<(), TerminalInputError> {
        let session = self
            .sessions
            .lock()
            .await
            .get(session_id)
            .ok_or(TerminalInputError::UnknownSession(session_id))?;
        if !session.tty {
            return Err(TerminalInputError::NotATerminal(session_id));
        }
        session
            .write_bytes(input)
            .await
            .map_err(|source| TerminalInputError::Write { session_id, source })
    }

    pub(crate) async fn terminal_output(
        &self,
        session_id: i64,
    ) -> Result<Vec<u8>, TerminalInputError> {
        let session = {
            let mut store = self.sessions.lock().await;
            if let Some(session) = store.get(session_id) {
                session
            } else {
                return store
                    .archived_terminal(session_id)
                    .ok_or(TerminalInputError::UnknownSession(session_id));
            }
        };
        if !session.tty {
            return Err(TerminalInputError::NotATerminal(session_id));
        }
        Ok(session.terminal_output().await)
    }

    pub(crate) async fn terminate_all(&self) {
        let sessions = {
            let mut store = self.sessions.lock().await;
            store.recency.clear();
            store.terminal_archives.clear();
            store
                .sessions
                .drain()
                .map(|(_, session)| session)
                .collect::<Vec<_>>()
        };
        for session in sessions {
            session.terminate().await;
        }
    }
}

#[derive(Default)]
struct SessionStore {
    sessions: HashMap<i64, Arc<Session>>,
    recency: VecDeque<i64>,
    terminal_archives: VecDeque<(i64, Vec<u8>)>,
}

impl SessionStore {
    fn insert(&mut self, session: Arc<Session>) -> Option<Arc<Session>> {
        let pruned = (self.sessions.len() >= MAX_LIVE_SESSIONS)
            .then(|| {
                let protected_from = self.recency.len().saturating_sub(8);
                self.recency.iter().take(protected_from).position(|id| {
                    self.sessions
                        .get(id)
                        .is_some_and(|session| !session.is_active())
                })
            })
            .flatten()
            .and_then(|index| {
                let id = self.recency.remove(index)?;
                self.sessions.remove(&id)
            });
        self.recency.push_back(session.id);
        self.sessions.insert(session.id, session);
        pruned
    }

    fn get(&mut self, id: i64) -> Option<Arc<Session>> {
        let session = self.sessions.get(&id).cloned()?;
        self.touch(id);
        Some(session)
    }

    fn remove(&mut self, id: i64) -> Option<Arc<Session>> {
        if let Some(index) = self.recency.iter().position(|candidate| *candidate == id) {
            self.recency.remove(index);
        }
        self.sessions.remove(&id)
    }

    fn touch(&mut self, id: i64) {
        if let Some(index) = self.recency.iter().position(|candidate| *candidate == id) {
            self.recency.remove(index);
        }
        self.recency.push_back(id);
    }

    fn archive_terminal(&mut self, id: i64, output: Vec<u8>) {
        self.terminal_archives.push_back((id, output));
        while self.terminal_archives.len() > MAX_TERMINAL_ARCHIVES {
            self.terminal_archives.pop_front();
        }
    }

    fn archived_terminal(&self, id: i64) -> Option<Vec<u8>> {
        self.terminal_archives
            .iter()
            .find(|(candidate, _)| *candidate == id)
            .map(|(_, output)| output.clone())
    }
}

struct Session {
    id: i64,
    command: String,
    tty: bool,
    interactive: bool,
    interaction: Mutex<()>,
    child: Mutex<process::ProcessChild>,
    stdin: Mutex<Option<process::ProcessStdin>>,
    process_group: Mutex<process::ProcessGroupGuard>,
    drains: Mutex<Option<Vec<JoinHandle<()>>>>,
    captured: Arc<Mutex<CapturedOutput>>,
    terminal_output: Arc<Mutex<TerminalOutput>>,
    secrets: Vec<String>,
    next_chunk_id: AtomicU64,
    active_interactions: AtomicU64,
}

impl Session {
    fn new(
        id: i64,
        spawned: process::SpawnedProcess,
        secrets: Vec<String>,
        command: String,
        interactive: bool,
    ) -> Arc<Self> {
        let tty = matches!(spawned.stdin.as_ref(), Some(process::ProcessStdin::Pty(_)));
        let captured = Arc::new(Mutex::new(CapturedOutput::default()));
        let terminal_output = Arc::new(Mutex::new(TerminalOutput::default()));
        let drains = match spawned.output {
            process::ProcessOutput::Pipes { stdout, stderr } => vec![
                tokio::spawn(output::drain(
                    stdout,
                    Arc::clone(&captured),
                    None,
                    MAX_CAPTURE_BYTES,
                )),
                tokio::spawn(output::drain(
                    stderr,
                    Arc::clone(&captured),
                    None,
                    MAX_CAPTURE_BYTES,
                )),
            ],
            process::ProcessOutput::Pty(reader) => vec![output::drain_blocking(
                reader,
                Arc::clone(&captured),
                Arc::clone(&terminal_output),
                MAX_CAPTURE_BYTES,
            )],
        };
        Arc::new(Self {
            id,
            command,
            tty,
            interactive,
            interaction: Mutex::new(()),
            child: Mutex::new(spawned.child),
            stdin: Mutex::new(spawned.stdin),
            process_group: Mutex::new(spawned.process_group),
            drains: Mutex::new(Some(drains)),
            captured,
            terminal_output,
            secrets,
            next_chunk_id: AtomicU64::new(1),
            active_interactions: AtomicU64::new(0),
        })
    }

    fn begin_interaction(&self) -> ActiveInteraction<'_> {
        self.active_interactions.fetch_add(1, Ordering::AcqRel);
        ActiveInteraction { session: self }
    }

    fn is_active(&self) -> bool {
        self.active_interactions.load(Ordering::Acquire) > 0
    }

    async fn terminate(&self) {
        let _ = self.process_group.lock().await.terminate_and_disarm();
    }

    async fn interrupt(&self) -> std::io::Result<()> {
        self.process_group.lock().await.interrupt()
    }

    async fn write(&self, chars: &str) -> std::io::Result<()> {
        self.write_bytes(chars.as_bytes()).await
    }

    async fn write_bytes(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut stdin = self.stdin.lock().await;
        let stdin = stdin.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdin is closed")
        })?;
        stdin.write(bytes).await
    }

    async fn terminal_output(&self) -> Vec<u8> {
        self.terminal_output.lock().await.snapshot()
    }

    async fn wait_for_output(
        &self,
        yield_time: Duration,
        max_output_tokens: Option<i64>,
        started_at: Instant,
    ) -> ExecCommandResult {
        let status = {
            let mut child = self.child.lock().await;
            timeout(yield_time, child.wait()).await
        };
        let exit_code = match status {
            Ok(Ok(exit_code)) => {
                self.process_group.lock().await.disarm();
                self.finish_drains().await;
                Some(exit_code)
            }
            Ok(Err(error)) => {
                let _ = self.process_group.lock().await.terminate_and_disarm();
                let message = format!("failed to wait for shell command: {error}");
                self.captured
                    .lock()
                    .await
                    .push(message.as_bytes(), MAX_CAPTURE_BYTES);
                Some(1)
            }
            Err(_) => None,
        };
        let (output, original_token_count) = self.take_output(max_output_tokens).await;
        ExecCommandResult {
            chunk_id: Some(format!(
                "{}-{}",
                self.id,
                self.next_chunk_id.fetch_add(1, Ordering::Relaxed)
            )),
            wall_time_seconds: started_at.elapsed().as_secs_f64(),
            exit_code,
            session_id: exit_code.is_none().then_some(self.id),
            original_token_count,
            output,
        }
    }

    async fn finish_drains(&self) {
        let handles = self.drains.lock().await.take();
        let Some(handles) = handles else {
            return;
        };
        let deadline = tokio::time::Instant::now() + DRAIN_GRACE;
        for mut handle in handles {
            if tokio::time::timeout_at(deadline, &mut handle)
                .await
                .is_err()
            {
                handle.abort();
                let _ = handle.await;
            }
        }
    }

    async fn take_output(&self, max_output_tokens: Option<i64>) -> (String, Option<usize>) {
        let captured = self.captured.lock().await.take();
        let raw = String::from_utf8_lossy(&captured.with_omission_marker()).into_owned();
        let limit = output::effective_token_limit(max_output_tokens);
        let (output, limited) = output::redact_and_limit(raw, &self.secrets, limit);
        let was_truncated = limited || captured.omitted_bytes > 0;
        (
            output,
            was_truncated.then_some(captured.total_bytes.saturating_add(3) / 4),
        )
    }
}

struct ActiveInteraction<'a> {
    session: &'a Session,
}

impl Drop for ActiveInteraction<'_> {
    fn drop(&mut self) {
        self.session
            .active_interactions
            .fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Default)]
pub(super) struct CapturedOutput {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    omitted_bytes: usize,
}

#[derive(Default)]
pub(super) struct TerminalOutput {
    bytes: VecDeque<u8>,
}

impl TerminalOutput {
    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend(bytes);
        let overflow = self.bytes.len().saturating_sub(MAX_TERMINAL_OUTPUT_BYTES);
        self.bytes.drain(..overflow);
    }

    fn snapshot(&self) -> Vec<u8> {
        self.bytes.iter().copied().collect()
    }
}

impl CapturedOutput {
    pub(super) fn push(&mut self, bytes: &[u8], limit: usize) {
        if bytes.is_empty() {
            return;
        }
        if limit == 0 {
            self.omitted_bytes = self.omitted_bytes.saturating_add(bytes.len());
            return;
        }

        let head_budget = limit / 2;
        let tail_budget = limit.saturating_sub(head_budget);
        let head_len = head_budget.saturating_sub(self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_len]);
        let tail_bytes = &bytes[head_len..];
        if tail_bytes.len() >= tail_budget {
            let kept_from = tail_bytes.len().saturating_sub(tail_budget);
            self.omitted_bytes = self
                .omitted_bytes
                .saturating_add(self.tail.len())
                .saturating_add(kept_from);
            self.tail.clear();
            self.tail.extend(&tail_bytes[kept_from..]);
        } else {
            self.tail.extend(tail_bytes);
            let excess = self.tail.len().saturating_sub(tail_budget);
            if excess > 0 {
                drop(self.tail.drain(..excess));
                self.omitted_bytes = self.omitted_bytes.saturating_add(excess);
            }
        }
    }

    fn take(&mut self) -> CapturedChunk {
        let head = std::mem::take(&mut self.head);
        let tail = std::mem::take(&mut self.tail);
        let omitted_bytes = std::mem::take(&mut self.omitted_bytes);
        CapturedChunk {
            total_bytes: head
                .len()
                .saturating_add(tail.len())
                .saturating_add(omitted_bytes),
            head,
            tail,
            omitted_bytes,
        }
    }
}

struct CapturedChunk {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    omitted_bytes: usize,
    total_bytes: usize,
}

impl CapturedChunk {
    fn with_omission_marker(&self) -> Vec<u8> {
        let marker = (self.omitted_bytes > 0)
            .then(|| format!("... {} bytes omitted ...", self.omitted_bytes));
        let marker_length = marker.as_ref().map_or(0, |marker| marker.len() + 2);
        let mut output = Vec::with_capacity(
            self.head
                .len()
                .saturating_add(self.tail.len())
                .saturating_add(marker_length),
        );
        output.extend_from_slice(&self.head);
        if let Some(marker) = marker {
            output.push(b'\n');
            output.extend_from_slice(marker.as_bytes());
            output.push(b'\n');
        }
        output.extend(self.tail.iter().copied());
        output
    }
}

impl ExecCommandResult {
    fn failed(wall_time: Duration, output: String) -> Self {
        Self {
            chunk_id: None,
            wall_time_seconds: wall_time.as_secs_f64(),
            exit_code: Some(1),
            session_id: None,
            original_token_count: None,
            output,
        }
    }
}

fn resolve_workdir(workspace: &Path, requested: Option<&str>) -> PathBuf {
    let requested = requested.filter(|workdir| !workdir.is_empty());
    match requested.map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        Some(path) => workspace.join(path),
        None => workspace.to_owned(),
    }
}

fn duration_ms(requested: Option<i64>, default: u64, minimum: u64, maximum: u64) -> Duration {
    let requested = requested
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(default);
    Duration::from_millis(requested.clamp(minimum, maximum))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::{
        sync::Arc,
        time::{Duration, SystemTime},
    };

    use super::{CapturedOutput, ExecCommand, ShellSessions};
    #[cfg(unix)]
    use super::{TerminalInputError, WriteStdin};

    #[test]
    fn bounded_capture_keeps_head_and_tail_then_accepts_the_next_poll() {
        let mut captured = CapturedOutput::default();
        captured.push(b"abcdefgh", 4);
        let first = captured.take();
        let first = String::from_utf8(first.with_omission_marker()).expect("ASCII output");
        assert!(first.starts_with("ab\n... 4 bytes omitted ...\n"));
        assert!(first.ends_with("gh"));

        captured.push(b"next", 4);
        let second = captured.take();
        assert_eq!(second.with_omission_marker(), b"next");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn yielded_command_accepts_stdin_and_completes() {
        let sessions = ShellSessions::new();
        let first = sessions
            .execute(
                ExecCommand::new(
                    "read value; printf 'got:%s' \"$value\"".to_owned(),
                    None,
                    None,
                    Some(false),
                    false,
                    Some(250),
                    None,
                ),
                std::path::Path::new("/"),
            )
            .await;
        assert_eq!(first.session_id, Some(1));

        let second = sessions
            .write_stdin(WriteStdin::new(1, "hello\n".to_owned(), Some(1_000), None))
            .await;
        assert_eq!(second.exit_code, Some(0));
        assert_eq!(second.output, "got:hello");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn control_c_interrupts_a_non_tty_process_group() {
        let sessions = ShellSessions::new();
        let first = sessions
            .execute(
                ExecCommand::new(
                    "sleep 30".to_owned(),
                    None,
                    None,
                    Some(false),
                    false,
                    Some(250),
                    None,
                ),
                std::path::Path::new("/"),
            )
            .await;
        assert_eq!(first.session_id, Some(1));

        let interrupted = sessions
            .write_stdin(WriteStdin::new(1, "\u{3}".to_owned(), Some(1_000), None))
            .await;

        assert_eq!(interrupted.exit_code, Some(130));
        assert_eq!(interrupted.session_id, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_command_leaves_background_process_running()
    -> Result<(), Box<dyn std::error::Error>> {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "nanocodex-background-process-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory)?;
        let marker = directory.join("survived");
        let sessions = ShellSessions::new();

        let result = sessions
            .execute(
                ExecCommand::new(
                    format!(
                        "(sleep 1; printf survived > '{}') >/dev/null 2>&1 &",
                        marker.display()
                    ),
                    None,
                    None,
                    Some(false),
                    false,
                    Some(1_000),
                    None,
                ),
                std::path::Path::new("/"),
            )
            .await;

        tokio::time::timeout(Duration::from_secs(3), async {
            while !matches!(std::fs::read_to_string(&marker).as_deref(), Ok("survived")) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("background process should survive successful shell exit");
        let contents = std::fs::read_to_string(&marker)?;
        std::fs::remove_dir_all(directory)?;

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(contents, "survived");
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drain_grace_is_shared_across_output_streams() -> Result<(), Box<dyn std::error::Error>>
    {
        let sessions = ShellSessions::new();
        let started_at = std::time::Instant::now();
        let result = sessions
            .execute(
                ExecCommand::new(
                    "sleep 30 & printf '%s' $!".to_owned(),
                    None,
                    Some("/bin/sh".to_owned()),
                    Some(false),
                    false,
                    Some(10_000),
                    None,
                ),
                std::path::Path::new("/"),
            )
            .await;
        let elapsed = started_at.elapsed();
        let background_pid = result.output.trim();
        let _ = std::process::Command::new("kill")
            .args(["-9", background_pid])
            .status();

        assert_eq!(result.exit_code, Some(0));
        assert!(
            elapsed < Duration::from_millis(3_500),
            "two blocked drains consumed separate grace periods: {elapsed:?}"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_terminates_shell_descendants() -> Result<(), Box<dyn std::error::Error>> {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "nanocodex-cancelled-process-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory)?;
        let marker = directory.join("escaped");
        let sessions = ShellSessions::new();

        let result = sessions
            .execute(
                ExecCommand::new(
                    format!("(sleep 1; printf escaped > '{}') & wait", marker.display()),
                    None,
                    None,
                    Some(false),
                    false,
                    Some(250),
                    None,
                ),
                std::path::Path::new("/"),
            )
            .await;
        assert_eq!(result.session_id, Some(1));

        sessions.terminate_all().await;
        tokio::time::sleep(Duration::from_millis(1_250)).await;
        assert!(!marker.exists(), "a cancelled shell descendant escaped");
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tty_command_has_a_terminal_and_accepts_stdin() {
        let ready =
            std::env::temp_dir().join(format!("nanocodex-pty-ready-{}", std::process::id()));
        let _ = std::fs::remove_file(&ready);
        let sessions = ShellSessions::new();
        let first = sessions
            .execute(
                ExecCommand::new(
                    format!(
                        "test -t 0 && test -t 1 && test -t 2; stty -echo; : > '{}'; printf ready; read value; printf 'got:%s' \"$value\"",
                        ready.display()
                    ),
                    None,
                    None,
                    Some(false),
                    true,
                    Some(1_000),
                    None,
                ),
                std::path::Path::new("/"),
            )
            .await;
        assert_eq!(first.session_id, Some(1));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !ready.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("PTY command should disable echo before stdin is written");
        std::fs::remove_file(&ready).expect("PTY readiness marker should be removable");

        let second = sessions
            .write_stdin(WriteStdin::new(1, "hello\n".to_owned(), Some(1_000), None))
            .await;
        assert_eq!(second.exit_code, Some(0));
        assert_eq!(
            format!("{}{}", first.output, second.output),
            "readygot:hello"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn human_terminal_input_is_not_blocked_by_a_model_poll() {
        let sessions = Arc::new(ShellSessions::new());
        let first = sessions
            .execute(
                ExecCommand::new(
                    "stty -echo; read value; printf 'got:%s' \"$value\"".to_owned(),
                    None,
                    None,
                    Some(false),
                    true,
                    Some(250),
                    None,
                ),
                std::path::Path::new("/"),
            )
            .await;
        assert_eq!(first.session_id, Some(1));

        let polling_sessions = Arc::clone(&sessions);
        let poll = tokio::spawn(async move {
            polling_sessions
                .write_stdin(WriteStdin::new(1, String::new(), Some(5_000), None))
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        tokio::time::timeout(
            Duration::from_millis(500),
            sessions.write_terminal(1, b"hello\n"),
        )
        .await
        .expect("human input must not wait for the model's long poll")
        .expect("PTY input should be accepted");

        let result = poll.await.expect("poll task should complete");
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.contains("got:hello"), "{}", result.output);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_control_lists_only_ptys_and_rejects_other_sessions() {
        let sessions = ShellSessions::new();
        let plain = sessions
            .execute(
                ExecCommand::new(
                    "sleep 30".to_owned(),
                    None,
                    None,
                    Some(false),
                    false,
                    Some(250),
                    None,
                ),
                std::path::Path::new("/"),
            )
            .await;
        assert_eq!(plain.session_id, Some(1));
        let decorative_terminal = sessions
            .execute(
                ExecCommand::new(
                    "sleep 30".to_owned(),
                    None,
                    None,
                    Some(false),
                    true,
                    Some(250),
                    None,
                ),
                std::path::Path::new("/"),
            )
            .await;
        assert_eq!(decorative_terminal.session_id, Some(2));
        let terminal = sessions
            .execute(
                ExecCommand::new(
                    "stty -echo; read value".to_owned(),
                    None,
                    None,
                    Some(false),
                    true,
                    Some(250),
                    None,
                )
                .interactive(true),
                std::path::Path::new("/"),
            )
            .await;
        assert_eq!(terminal.session_id, Some(3));

        assert_eq!(
            sessions.terminal_sessions().await,
            vec![super::TerminalSessionInfo {
                id: 3,
                command: "stty -echo; read value".to_owned(),
            }]
        );
        assert!(matches!(
            sessions.write_terminal(1, b"nope\n").await,
            Err(TerminalInputError::NotATerminal(1))
        ));
        assert!(matches!(
            sessions.write_terminal(99, b"nope\n").await,
            Err(TerminalInputError::UnknownSession(99))
        ));

        sessions.terminate_all().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_output_snapshot_does_not_consume_model_output() {
        if !std::path::Path::new("/bin/bash").is_file() {
            return;
        }
        let sessions = ShellSessions::new();
        let first = sessions
            .execute(
                ExecCommand::new(
                    r"stty -echo; printf '\033[2J\033[H> first\r\n  second'; IFS= read -r -s -n 3 key; [[ $key == $'\033[B' ]] && printf '\033[2J\033[H  first\r\n> second'; read done".to_owned(),
                    None,
                    Some("/bin/bash".to_owned()),
                    Some(false),
                    true,
                    Some(250),
                    None,
                ),
                std::path::Path::new("/"),
            )
            .await;
        assert_eq!(
            first.session_id,
            Some(1),
            "PTY exited early with {:?}: {}",
            first.exit_code,
            first.output
        );
        sessions
            .write_terminal(1, b"\x1b[B")
            .await
            .expect("raw terminal input should be accepted");

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let output = sessions
                    .terminal_output(1)
                    .await
                    .expect("terminal snapshot should remain available");
                if String::from_utf8_lossy(&output).contains("> second") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal output should reach the snapshot");

        let polled = sessions
            .write_stdin(WriteStdin::new(1, "done\n".to_owned(), Some(1_000), None))
            .await;
        assert!(polled.output.contains("> second"));
        assert!(
            String::from_utf8_lossy(
                &sessions
                    .terminal_output(1)
                    .await
                    .expect("completed terminal output should remain archived")
            )
            .contains("> second")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tty_commands_receive_an_interactive_term() {
        let sessions = ShellSessions::new();
        let result = sessions
            .execute(
                ExecCommand::new(
                    "printf %s \"$TERM\"".to_owned(),
                    None,
                    None,
                    Some(false),
                    true,
                    Some(1_000),
                    None,
                ),
                std::path::Path::new("/"),
            )
            .await;
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.output, "xterm-256color");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn explicit_shell_runs_bash_syntax() {
        if !std::path::Path::new("/bin/bash").is_file() {
            return;
        }
        let sessions = ShellSessions::new();
        let result = sessions
            .execute(
                ExecCommand::new(
                    "[[ codex == codex ]] && printf bash".to_owned(),
                    None,
                    Some("/bin/bash".to_owned()),
                    Some(false),
                    false,
                    Some(1_000),
                    None,
                ),
                std::path::Path::new("/"),
            )
            .await;

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.output, "bash");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn default_cmd_shell_executes_a_command() {
        let sessions = ShellSessions::new();
        let result = sessions
            .execute(
                ExecCommand::new(
                    "echo windows".to_owned(),
                    None,
                    None,
                    Some(false),
                    false,
                    Some(1_000),
                    None,
                ),
                &std::env::temp_dir(),
            )
            .await;

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.output.trim(), "windows");
    }
}
