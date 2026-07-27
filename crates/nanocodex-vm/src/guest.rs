use std::{
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use nanocodex_tools::{ToolContext, ToolRuntime};
use nix::{
    errno::Errno,
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use thiserror::Error;
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
        BufReader,
    },
    process::{Child, Command},
    sync::mpsc,
    task::JoinSet,
};

use crate::protocol::{
    ControlResponse, ExecuteRequest, ExecuteResponse, ReadFileRequest, ReadFileResponse,
    SessionRequest, SessionResponse, ShutdownRequest, ToolResponse, WriteFileRequest,
};

const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_CONTROL_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_IN_FLIGHT_REQUESTS: usize = 64;

/// Failure while serving VM tool requests inside the guest.
#[derive(Debug, Error)]
pub enum VmGuestError {
    #[error("VM tool console I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("VM tool protocol JSON failed: {0}")]
    Json(#[from] serde_json::Error),

    #[error("the VM tool console closed before a complete frame")]
    Closed,

    #[error("guest tool execution task failed: {0}")]
    Task(String),

    #[error("VM tool protocol frame exceeded the {MAX_FRAME_BYTES}-byte limit")]
    FrameTooLarge,
}

pub(crate) async fn serve(workspace: &Path) -> Result<(), VmGuestError> {
    serve_io(workspace, tokio::io::stdin(), tokio::io::stdout()).await
}

async fn serve_io(
    workspace: &Path,
    input: impl AsyncRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
) -> Result<(), VmGuestError> {
    let runtime = Arc::new(ToolRuntime::new(workspace, None, None));
    let mut input = BufReader::new(input);
    let mut requests = JoinSet::new();
    let mut accepting = true;
    let mut shutdown = None;

    let result = async {
        while accepting || !requests.is_empty() {
            tokio::select! {
                joined = requests.join_next(), if !requests.is_empty() => {
                    match joined.ok_or(VmGuestError::Closed)? {
                        Ok(response) => write_response(&mut output, &response).await?,
                        Err(error) if error.is_cancelled() && !accepting => {}
                        Err(error) => return Err(VmGuestError::Task(error.to_string())),
                    }
                }
                frame = read_frame(&mut input),
                    if accepting && requests.len() < MAX_IN_FLIGHT_REQUESTS =>
                {
                    let Some(frame) = frame? else {
                        accepting = false;
                        requests.abort_all();
                        continue;
                    };
                    match serde_json::from_slice::<SessionRequest>(&frame)? {
                        SessionRequest::Shutdown(request) => {
                            shutdown = Some(request);
                            accepting = false;
                            runtime.control().cancel().await;
                            requests.abort_all();
                        }
                        request => {
                            let runtime = Arc::clone(&runtime);
                            requests.spawn(async move { execute_request(runtime, request).await });
                        }
                    }
                }
            }
        }
        Ok::<_, VmGuestError>(shutdown)
    }
    .await;

    runtime.control().cancel().await;
    if let Some(request) = result? {
        let response = SessionResponse::Shutdown(sync_filesystems(request).await);
        write_response(&mut output, &response).await?;
    }
    Ok(())
}

async fn execute_request(runtime: Arc<ToolRuntime>, request: SessionRequest) -> SessionResponse {
    match request {
        SessionRequest::Tool(request) => {
            let context = ToolContext {
                model: &request.context.model,
                session_id: &request.context.session_id,
                call_id: &request.context.call_id,
                history: &[],
                output_token_budget: request.context.output_token_budget,
            };
            let execution = runtime
                .execute_tool(request.tool.name(), request.input.into(), context)
                .await;
            SessionResponse::Tool(match execution.into_wire() {
                Ok(execution) => ToolResponse::completed(request.id, execution),
                Err(error) => ToolResponse::failed(request.id, error.to_string()),
            })
        }
        SessionRequest::WriteFile(request) => SessionResponse::WriteFile(write_file(request).await),
        SessionRequest::ReadFile(request) => SessionResponse::ReadFile(read_file(request).await),
        SessionRequest::Execute(request) => {
            SessionResponse::Execute(execute_command(request).await)
        }
        SessionRequest::Shutdown(request) => SessionResponse::Shutdown(ControlResponse {
            id: request.id,
            error: Some("shutdown cannot be dispatched as a concurrent request".to_owned()),
        }),
    }
}

async fn write_response(
    output: &mut (impl AsyncWrite + Unpin),
    response: &SessionResponse,
) -> Result<(), VmGuestError> {
    let mut encoded = serde_json::to_vec(response)?;
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(VmGuestError::FrameTooLarge);
    }
    encoded.push(b'\n');
    output.write_all(&encoded).await?;
    output.flush().await?;
    Ok(())
}

async fn read_frame(
    reader: &mut (impl AsyncBufRead + Unpin),
) -> Result<Option<Vec<u8>>, VmGuestError> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(VmGuestError::Closed)
            };
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if frame.len().saturating_add(newline) > MAX_FRAME_BYTES {
                return Err(VmGuestError::FrameTooLarge);
            }
            frame.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return Ok(Some(frame));
        }
        if frame.len().saturating_add(available.len()) > MAX_FRAME_BYTES {
            return Err(VmGuestError::FrameTooLarge);
        }
        let consumed = available.len();
        frame.extend_from_slice(available);
        reader.consume(consumed);
    }
}

async fn sync_filesystems(request: ShutdownRequest) -> ControlResponse {
    let error = match Command::new("/bin/sync").status().await {
        Ok(status) if status.success() => None,
        Ok(status) => Some(format!("sync exited with {status}")),
        Err(error) => Some(error.to_string()),
    };
    ControlResponse {
        id: request.id,
        error,
    }
}

async fn write_file(request: WriteFileRequest) -> ControlResponse {
    let result =
        atomic_write_file(&request.path, &request.contents, request.mode, request.id).await;
    ControlResponse {
        id: request.id,
        error: result.err().map(|error| error.to_string()),
    }
}

async fn atomic_write_file(
    path: &str,
    contents: &[u8],
    mode: u32,
    request_id: u64,
) -> std::io::Result<()> {
    let path = PathBuf::from(path);
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("file path has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("file path has no name"))?
        .to_string_lossy();
    tokio::fs::create_dir_all(parent).await?;
    let temporary = parent.join(format!(".{name}.nanocodex-{request_id}.tmp"));
    let result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .await?;
        file.write_all(contents).await?;
        file.flush().await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(mode))
                .await?;
        }
        drop(file);
        tokio::fs::rename(&temporary, &path).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

async fn read_file(request: ReadFileRequest) -> ReadFileResponse {
    let contents = async {
        let metadata = tokio::fs::metadata(&request.path).await?;
        if metadata.len() > MAX_CONTROL_FILE_BYTES {
            return Err(std::io::Error::other(format!(
                "file is {} bytes, exceeding the {MAX_CONTROL_FILE_BYTES}-byte control limit",
                metadata.len()
            )));
        }
        tokio::fs::read(&request.path).await
    }
    .await;
    match contents {
        Ok(contents) => ReadFileResponse {
            id: request.id,
            contents: Some(contents),
            error: None,
        },
        Err(error) => ReadFileResponse {
            id: request.id,
            contents: None,
            error: Some(error.to_string()),
        },
    }
}

async fn execute_command(request: ExecuteRequest) -> ExecuteResponse {
    let mut command = Command::new(&request.program);
    command
        .args(&request.arguments)
        .current_dir(&request.current_directory)
        .env_clear()
        .envs(request.environment.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.process_group(0);

    let timeout = Duration::from_millis(request.timeout_millis);
    match command_output(&mut command, timeout, request.max_output_bytes).await {
        Ok(CommandOutcome::Completed(output)) => ExecuteResponse {
            id: request.id,
            exit_code: Some(output.status.code().unwrap_or(1)),
            stdout: Some(output.stdout),
            stderr: Some(output.stderr),
            error: None,
            timed_out: false,
            output_limit_exceeded: false,
        },
        Ok(CommandOutcome::TimedOut) => ExecuteResponse {
            id: request.id,
            exit_code: None,
            stdout: None,
            stderr: None,
            error: None,
            timed_out: true,
            output_limit_exceeded: false,
        },
        Ok(CommandOutcome::OutputLimitExceeded) => ExecuteResponse {
            id: request.id,
            exit_code: None,
            stdout: None,
            stderr: None,
            error: None,
            timed_out: false,
            output_limit_exceeded: true,
        },
        Err(error) => ExecuteResponse {
            id: request.id,
            exit_code: None,
            stdout: None,
            stderr: None,
            error: Some(error.to_string()),
            timed_out: false,
            output_limit_exceeded: false,
        },
    }
}

enum CommandOutcome {
    Completed(std::process::Output),
    TimedOut,
    OutputLimitExceeded,
}

enum WaitOutcome {
    Exited(ExitStatus),
    TimedOut,
    OutputLimitExceeded,
}

async fn command_output(
    command: &mut Command,
    timeout: Duration,
    max_output_bytes: usize,
) -> std::io::Result<CommandOutcome> {
    let mut child = command.spawn()?;
    let process_group = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .map(Pid::from_raw);
    let mut process_group_guard = ProcessGroupGuard(process_group);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("guest command stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("guest command stderr was not piped"))?;
    let retained = Arc::new(AtomicUsize::new(0));
    let (limit_sender, mut limit_receiver) = mpsc::channel(1);
    let mut stdout = tokio::spawn(read_bounded(
        stdout,
        Arc::clone(&retained),
        max_output_bytes,
        limit_sender.clone(),
    ));
    let mut stderr = tokio::spawn(read_bounded(
        stderr,
        Arc::clone(&retained),
        max_output_bytes,
        limit_sender,
    ));
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    let mut outcome = tokio::select! {
        status = child.wait() => WaitOutcome::Exited(status?),
        () = &mut deadline => WaitOutcome::TimedOut,
        Some(()) = limit_receiver.recv() => WaitOutcome::OutputLimitExceeded,
    };
    if !matches!(outcome, WaitOutcome::Exited(_)) {
        if let Some(status) = child.try_wait()? {
            outcome = WaitOutcome::Exited(status);
        } else {
            kill_process_group(&mut child, process_group)?;
            child.wait().await?;
        }
    }
    let (stdout, stderr) = tokio::time::timeout(Duration::from_secs(1), async {
        let stdout = (&mut stdout).await.map_err(std::io::Error::other)??;
        let stderr = (&mut stderr).await.map_err(std::io::Error::other)??;
        Ok::<_, std::io::Error>((stdout, stderr))
    })
    .await
    .unwrap_or_else(|_| {
        let _ = kill_process_group(&mut child, process_group);
        Err(std::io::Error::other(
            "guest command descendants kept output pipes open",
        ))
    })?;
    process_group_guard.disarm();
    let output_limit_exceeded = retained.load(Ordering::Relaxed) > max_output_bytes;
    match outcome {
        WaitOutcome::Exited(_) if output_limit_exceeded => Ok(CommandOutcome::OutputLimitExceeded),
        WaitOutcome::Exited(status) => Ok(CommandOutcome::Completed(std::process::Output {
            status,
            stdout,
            stderr,
        })),
        WaitOutcome::TimedOut => Ok(CommandOutcome::TimedOut),
        WaitOutcome::OutputLimitExceeded => Ok(CommandOutcome::OutputLimitExceeded),
    }
}

fn kill_process_group(child: &mut Child, process_group: Option<Pid>) -> std::io::Result<()> {
    if let Some(process_group) = process_group {
        let child_kill = child.start_kill();
        match killpg(process_group, Signal::SIGKILL) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(Errno::EPERM) => child_kill.or(Ok(())),
            Err(error) => Err(std::io::Error::other(error)),
        }
    } else {
        child.start_kill()
    }
}

struct ProcessGroupGuard(Option<Pid>);

impl ProcessGroupGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(process_group) = self.0 {
            let _ = killpg(process_group, Signal::SIGKILL);
        }
    }
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    retained: Arc<AtomicUsize>,
    limit: usize,
    limit_sender: mpsc::Sender<()>,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    let mut reported = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let offset = retained.fetch_add(read, Ordering::Relaxed);
        let allowed = limit.saturating_sub(offset).min(read);
        output.extend_from_slice(&buffer[..allowed]);
        if allowed < read && !reported {
            reported = true;
            let _ = limit_sender.try_send(());
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::{execute_command, serve_io};
    use crate::protocol::{ExecuteRequest, SessionRequest, SessionResponse, ShutdownRequest};

    const DEFAULT_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

    #[tokio::test]
    async fn timeout_kills_descendants_holding_output_pipes() {
        let started_at = Instant::now();
        let response = execute_command(ExecuteRequest {
            id: 1,
            program: "/bin/sh".to_owned(),
            arguments: vec!["-c".to_owned(), "sleep 30 & wait".to_owned()],
            current_directory: "/".to_owned(),
            environment: Vec::new(),
            timeout_millis: 25,
            max_output_bytes: DEFAULT_OUTPUT_BYTES,
        })
        .await;

        assert!(response.timed_out);
        assert!(response.error.is_none());
        assert!(started_at.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn command_output_is_bounded_while_it_is_produced() {
        let response = execute_command(ExecuteRequest {
            id: 1,
            program: "/usr/bin/yes".to_owned(),
            arguments: vec!["bounded".to_owned()],
            current_directory: "/".to_owned(),
            environment: Vec::new(),
            timeout_millis: 5_000,
            max_output_bytes: 8,
        })
        .await;

        assert!(response.output_limit_exceeded);
        assert!(!response.timed_out);
        assert!(response.error.is_none());
        assert!(response.stdout.is_none());
        assert!(response.stderr.is_none());
    }

    #[tokio::test]
    async fn independent_requests_execute_concurrently() {
        let workspace = tempfile::tempdir().unwrap();
        let marker = workspace.path().join("second-started");
        let (host, guest) = tokio::io::duplex(64 * 1024);
        let (host_read, mut host_write) = tokio::io::split(host);
        let (guest_read, guest_write) = tokio::io::split(guest);
        let guest_task = tokio::spawn({
            let workspace = workspace.path().to_owned();
            async move { serve_io(&workspace, guest_read, guest_write).await }
        });

        for request in [
            SessionRequest::Execute(ExecuteRequest {
                id: 0,
                program: "/bin/sh".to_owned(),
                arguments: vec![
                    "-c".to_owned(),
                    format!("while [ ! -f '{}' ]; do sleep 0.01; done", marker.display()),
                ],
                current_directory: workspace.path().to_string_lossy().into_owned(),
                environment: Vec::new(),
                timeout_millis: 5_000,
                max_output_bytes: DEFAULT_OUTPUT_BYTES,
            }),
            SessionRequest::Execute(ExecuteRequest {
                id: 1,
                program: "/usr/bin/touch".to_owned(),
                arguments: vec![marker.to_string_lossy().into_owned()],
                current_directory: workspace.path().to_string_lossy().into_owned(),
                environment: Vec::new(),
                timeout_millis: 5_000,
                max_output_bytes: DEFAULT_OUTPUT_BYTES,
            }),
        ] {
            host_write
                .write_all(&serde_json::to_vec(&request).unwrap())
                .await
                .unwrap();
            host_write.write_all(b"\n").await.unwrap();
        }

        let mut responses = BufReader::new(host_read).lines();
        for _ in 0..2 {
            let line = responses.next_line().await.unwrap().unwrap();
            assert!(matches!(
                serde_json::from_str::<SessionResponse>(&line).unwrap(),
                SessionResponse::Execute(response)
                    if response.error.is_none() && !response.timed_out
            ));
        }
        host_write
            .write_all(
                &serde_json::to_vec(&SessionRequest::Shutdown(ShutdownRequest { id: 2 })).unwrap(),
            )
            .await
            .unwrap();
        host_write.write_all(b"\n").await.unwrap();
        drop(host_write);
        let shutdown = responses.next_line().await.unwrap().unwrap();
        assert!(matches!(
            serde_json::from_str::<SessionResponse>(&shutdown).unwrap(),
            SessionResponse::Shutdown(response) if response.id == 2 && response.error.is_none()
        ));
        guest_task.await.unwrap().unwrap();
        assert!(marker.is_file());
    }

    #[tokio::test]
    async fn shutdown_aborts_in_flight_work_before_syncing() {
        let workspace = tempfile::tempdir().unwrap();
        let (host, guest) = tokio::io::duplex(64 * 1024);
        let (host_read, mut host_write) = tokio::io::split(host);
        let (guest_read, guest_write) = tokio::io::split(guest);
        let guest_task = tokio::spawn({
            let workspace = workspace.path().to_owned();
            async move { serve_io(&workspace, guest_read, guest_write).await }
        });
        for request in [
            SessionRequest::Execute(ExecuteRequest {
                id: 0,
                program: "/bin/sh".to_owned(),
                arguments: vec!["-c".to_owned(), "sleep 30 & wait".to_owned()],
                current_directory: workspace.path().to_string_lossy().into_owned(),
                environment: Vec::new(),
                timeout_millis: 60_000,
                max_output_bytes: DEFAULT_OUTPUT_BYTES,
            }),
            SessionRequest::Shutdown(ShutdownRequest { id: 1 }),
        ] {
            host_write
                .write_all(&serde_json::to_vec(&request).unwrap())
                .await
                .unwrap();
            host_write.write_all(b"\n").await.unwrap();
        }

        let mut responses = BufReader::new(host_read).lines();
        let line = tokio::time::timeout(Duration::from_secs(2), responses.next_line())
            .await
            .expect("shutdown must not wait for the in-flight command")
            .unwrap()
            .unwrap();
        assert!(matches!(
            serde_json::from_str::<SessionResponse>(&line).unwrap(),
            SessionResponse::Shutdown(response) if response.id == 1 && response.error.is_none()
        ));
        drop(host_write);
        guest_task.await.unwrap().unwrap();
    }
}
