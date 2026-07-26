use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use nanocodex_tools::{
    StandardTool, ToolContext, ToolExecution, ToolInput, ToolResult, ToolRuntime,
};
use nanovm::EgressLease;
use nix::{
    errno::Errno,
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, oneshot},
    task::JoinSet,
};
use tracing::{Instrument, Span, info, info_span};

use crate::{
    VmToolClient,
    protocol::{
        ControlResponse, ExecuteRequest, ExecuteResponse, ReadFileRequest, ReadFileResponse,
        SessionRequest, SessionResponse, ShutdownRequest, ToolRequest, ToolResponse,
        WireToolContext, WireToolInput, WriteFileRequest,
    },
};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// One trusted command executed by the evaluation harness inside the guest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VmCommand {
    program: String,
    arguments: Vec<String>,
    current_directory: String,
    environment: Vec<(String, String)>,
    timeout: Duration,
}

impl VmCommand {
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            current_directory: "/".to_owned(),
            environment: Vec::new(),
            timeout: Duration::from_mins(1),
        }
    }

    #[must_use]
    pub fn arg(mut self, argument: impl Into<String>) -> Self {
        self.arguments.push(argument.into());
        self
    }

    #[must_use]
    pub fn current_directory(mut self, directory: impl Into<String>) -> Self {
        self.current_directory = directory.into();
        self
    }

    #[must_use]
    pub fn environment(mut self, environment: impl IntoIterator<Item = (String, String)>) -> Self {
        self.environment.extend(environment);
        self
    }

    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Complete output from one trusted harness command in the guest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VmCommandOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum VmToolSessionError {
    #[error("failed to spawn the VMM process: {0}")]
    Spawn(#[source] std::io::Error),

    #[error("the VMM process did not expose piped {0}")]
    MissingPipe(&'static str),

    #[error("VM tool console I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("VM tool protocol JSON failed: {0}")]
    Json(#[from] serde_json::Error),

    #[error("the VM tool console closed before replying")]
    Closed,

    #[error("VM tool response router failed: {0}")]
    Router(String),

    #[error("guest tool execution failed: {0}")]
    Guest(String),

    #[error("guest command exceeded {0:?}")]
    GuestTimeout(Duration),

    #[error("invalid VM tool response: {0}")]
    Protocol(&'static str),

    #[error("the VMM did not exit within {0:?} after guest shutdown")]
    ShutdownTimeout(Duration),

    #[error("the VMM exited unsuccessfully after guest shutdown: {0}")]
    VmmExit(ExitStatus),

    #[error("egress was already provisioned for this VM session")]
    EgressAlreadyProvisioned,

    #[error("egress guest file path is not valid UTF-8: {0}")]
    EgressFilePath(PathBuf),
}

/// Owner of one persistent VMM child carrying workspace tool calls.
///
/// Keep this value alive for the complete root-agent tree. Clone
/// [`VmToolSessionHandle`] or [`crate::VmTools`] into each driver's tool
/// factory; all of those handles route to this one VM.
pub struct VmToolSession {
    handle: VmToolSessionHandle,
    egress: Option<EgressLease>,
}

/// Clone-cheap capability for sending workspace tool calls to one owned VM.
#[derive(Clone)]
pub struct VmToolSessionHandle {
    inner: Arc<VmToolSessionInner>,
}

struct VmToolSessionInner {
    spawned_at: Instant,
    next_id: AtomicU64,
    closing: AtomicBool,
    input: Mutex<Option<ChildStdin>>,
    output: Mutex<Option<ChildStdout>>,
    pending: StdMutex<PendingState>,
    child: StdMutex<Option<Child>>,
}

#[derive(Default)]
struct PendingState {
    closed: Option<String>,
    requests: HashMap<u64, PendingResponse>,
}

struct PendingResponse {
    span: Span,
    response: oneshot::Sender<Result<(SessionResponse, usize), String>>,
}

struct PendingRequestGuard {
    inner: Weak<VmToolSessionInner>,
    id: u64,
    armed: bool,
}

impl VmToolSession {
    /// Spawns a VMM command whose guest process runs [`crate::serve_guest`].
    ///
    /// The command's stdin and stdout are reserved for the typed protocol;
    /// stderr remains available for VMM and guest diagnostics.
    ///
    /// # Errors
    ///
    /// Returns an error when the child or either protocol pipe cannot be
    /// created.
    pub fn spawn(command: &mut Command) -> Result<Self, VmToolSessionError> {
        let program = command
            .as_std()
            .get_program()
            .to_string_lossy()
            .into_owned();
        let command_content = format!("{:?}", command.as_std());
        let argument_count = command.as_std().get_args().count();
        let span = info_span!(
            target: "nanocodex_vm",
            "vm.session.spawn",
            otel.kind = "internal",
            otel.status_code = tracing::field::Empty,
            process.executable.name = program.as_str(),
            process.command_args.count = argument_count,
            process.id = tracing::field::Empty,
            status = tracing::field::Empty,
            error.message = tracing::field::Empty,
            duration_ns = tracing::field::Empty,
        );
        record_vm_content(&span, "vm.command", &command_content);
        let started_at = Instant::now();
        let result = span.in_scope(|| {
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .map_err(VmToolSessionError::Spawn)?;
            if let Some(process_id) = child.id() {
                span.record("process.id", process_id);
            }
            let input = child
                .stdin
                .take()
                .ok_or(VmToolSessionError::MissingPipe("stdin"))?;
            let output = child
                .stdout
                .take()
                .ok_or(VmToolSessionError::MissingPipe("stdout"))?;
            Ok(Self {
                handle: VmToolSessionHandle {
                    inner: Arc::new(VmToolSessionInner {
                        spawned_at: Instant::now(),
                        next_id: AtomicU64::new(0),
                        closing: AtomicBool::new(false),
                        input: Mutex::new(Some(input)),
                        output: Mutex::new(Some(output)),
                        pending: StdMutex::new(PendingState::default()),
                        child: StdMutex::new(Some(child)),
                    }),
                },
                egress: None,
            })
        });
        record_vm_result(&span, started_at, &result);
        result
    }

    /// Returns a clone-cheap capability for this session.
    #[must_use]
    pub fn handle(&self) -> VmToolSessionHandle {
        self.handle.clone()
    }

    /// Returns the standard VM-backed workspace tool factory for this session.
    #[must_use]
    pub fn tools(&self) -> crate::VmTools {
        crate::VmTools::new(self.handle())
    }

    /// Provisions provider-owned public files and retains the complete egress
    /// lease for the lifetime of this VM session.
    ///
    /// Call this exactly once, after spawning the VMM and before exposing tool
    /// handles to an agent. The same lease must already have been applied to
    /// the VM configuration and guest command with [`EgressLease::configure`].
    ///
    /// # Errors
    ///
    /// Returns an error when egress was already provisioned, a guest path is
    /// not UTF-8, or the guest rejects a file write.
    pub async fn provision_egress(
        &mut self,
        egress: EgressLease,
    ) -> Result<(), VmToolSessionError> {
        if self.egress.is_some() {
            return Err(VmToolSessionError::EgressAlreadyProvisioned);
        }
        let files = egress.guest_files().cloned().collect::<Vec<_>>();
        // Retain revocable provider state even when provisioning fails. The
        // session remains deliberately non-retryable and dropping its owner
        // tears down both the VMM and the incomplete lease.
        self.egress = Some(egress);
        for file in files {
            let path = file
                .guest_path()
                .to_str()
                .ok_or_else(|| VmToolSessionError::EgressFilePath(file.guest_path().to_owned()))?;
            self.write_file(path, file.contents().to_vec(), file.mode())
                .await?;
        }
        Ok(())
    }

    /// Writes one harness-owned file into the guest after the agent phase.
    ///
    /// # Errors
    ///
    /// Returns an error when the console closes, file creation fails in the
    /// guest, or the typed response is invalid.
    pub async fn write_file(
        &self,
        path: impl Into<String>,
        contents: Vec<u8>,
        mode: u32,
    ) -> Result<(), VmToolSessionError> {
        self.handle.write_file(path, contents, mode).await
    }

    /// Reads one result artifact from the guest.
    ///
    /// # Errors
    ///
    /// Returns an error when the console closes, the file cannot be read, or
    /// the typed response is invalid.
    pub async fn read_file(&self, path: impl Into<String>) -> Result<Vec<u8>, VmToolSessionError> {
        self.handle.read_file(path).await
    }

    /// Executes a trusted evaluation-harness command in the guest.
    ///
    /// # Errors
    ///
    /// Returns an error when the console closes, the command cannot run or
    /// exceeds its deadline, or the typed response is invalid.
    pub async fn command(&self, command: VmCommand) -> Result<VmCommandOutput, VmToolSessionError> {
        self.handle.command(command).await
    }

    /// Flushes guest filesystems and waits for the VMM process to exit.
    ///
    /// Consuming the owner prevents a cloned tool capability from shutting
    /// down the VM while another driver in the same agent tree is using it.
    ///
    /// # Errors
    ///
    /// Returns an error when the guest cannot acknowledge the request, the
    /// VMM does not stop promptly, or it exits unsuccessfully.
    pub async fn shutdown(self) -> Result<(), VmToolSessionError> {
        self.handle.inner.closing.store(true, Ordering::Release);
        let response = self
            .handle
            .control_request_inner(|id| SessionRequest::Shutdown(ShutdownRequest { id }), true)
            .await?;
        let SessionResponse::Shutdown(response) = response else {
            return Err(VmToolSessionError::Protocol("expected a shutdown response"));
        };
        control_result(response)?;
        self.handle.inner.input.lock().await.take();

        let child = lock_unpoisoned(&self.handle.inner.child).take();
        let Some(mut child) = child else {
            return Err(VmToolSessionError::Closed);
        };
        let status = if let Ok(status) = tokio::time::timeout(SHUTDOWN_TIMEOUT, child.wait()).await
        {
            status?
        } else {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(VmToolSessionError::ShutdownTimeout(SHUTDOWN_TIMEOUT));
        };
        close_pending(&self.handle.inner, "VM session shut down");
        if !status.success() {
            return Err(VmToolSessionError::VmmExit(status));
        }
        Ok(())
    }
}

impl Drop for VmToolSession {
    fn drop(&mut self) {
        self.handle.inner.closing.store(true, Ordering::Release);
        close_pending(&self.handle.inner, "VM session owner was dropped");
        if let Some(child) = lock_unpoisoned(&self.handle.inner.child).as_mut() {
            let _ = child.start_kill();
        }
    }
}

impl VmToolSessionHandle {
    async fn request(
        &self,
        tool: StandardTool,
        input: ToolInput,
        context: ToolContext<'_>,
    ) -> Result<ToolExecution, VmToolSessionError> {
        let (input_kind, input_bytes) = match &input {
            ToolInput::Function(arguments) => ("function", arguments.get().len()),
            ToolInput::Freeform(input) => ("freeform", input.len()),
        };
        let span = info_span!(
            target: "nanocodex_vm",
            "vm.tool.rpc",
            otel.kind = "client",
            otel.status_code = tracing::field::Empty,
            rpc.system = "libkrun.console",
            rpc.method = tool.name(),
            tool.name = tool.name(),
            session.id = context.session_id,
            tool.call_id = context.call_id,
            tool.input.kind = input_kind,
            tool.input.bytes = input_bytes,
            rpc.request.id = tracing::field::Empty,
            rpc.request.bytes = tracing::field::Empty,
            rpc.response.bytes = tracing::field::Empty,
            rpc.queue.duration_ns = tracing::field::Empty,
            vm.session.first_call = tracing::field::Empty,
            vm.session.age_ns = tracing::field::Empty,
            tool.success = tracing::field::Empty,
            status = tracing::field::Empty,
            error.message = tracing::field::Empty,
            duration_ns = tracing::field::Empty,
        );
        let started_at = Instant::now();
        let result = self
            .request_inner(tool, input, context, &span)
            .instrument(span.clone())
            .await;
        if let Ok(execution) = &result {
            span.record("tool.success", execution.success);
        }
        record_vm_result(&span, started_at, &result);
        result
    }

    async fn request_inner(
        &self,
        tool: StandardTool,
        input: ToolInput,
        context: ToolContext<'_>,
        span: &tracing::Span,
    ) -> Result<ToolExecution, VmToolSessionError> {
        let request = SessionRequest::Tool(ToolRequest {
            id: 0,
            tool,
            input: WireToolInput::from(input),
            context: WireToolContext {
                model: context.model.to_owned(),
                session_id: context.session_id.to_owned(),
                call_id: context.call_id.to_owned(),
                output_token_budget: context.output_token_budget,
            },
        });
        let (response, response_bytes) = self.send_request(request, span, false).await?;
        span.record("rpc.response.bytes", response_bytes);
        span.record("vm.session.age_ns", elapsed_ns(self.inner.spawned_at));
        let SessionResponse::Tool(response) = response else {
            return Err(VmToolSessionError::Protocol("expected a tool response"));
        };
        match (response.execution, response.error) {
            (Some(execution), None) => ToolExecution::from_wire(execution).map_err(Into::into),
            (None, Some(error)) => Err(VmToolSessionError::Guest(error)),
            _ => Err(VmToolSessionError::Protocol(
                "expected exactly one of execution or error",
            )),
        }
    }

    /// Writes one harness-owned file into the guest.
    ///
    /// # Errors
    ///
    /// Returns an error when the session is closed, guest file creation fails,
    /// or the response is invalid.
    pub async fn write_file(
        &self,
        path: impl Into<String>,
        contents: Vec<u8>,
        mode: u32,
    ) -> Result<(), VmToolSessionError> {
        let response = self
            .control_request(|id| {
                SessionRequest::WriteFile(WriteFileRequest {
                    id,
                    path: path.into(),
                    contents,
                    mode,
                })
            })
            .await?;
        let SessionResponse::WriteFile(response) = response else {
            return Err(VmToolSessionError::Protocol(
                "expected a write-file response",
            ));
        };
        control_result(response)
    }

    /// Reads one harness-owned result artifact from the guest.
    ///
    /// # Errors
    ///
    /// Returns an error when the session is closed, the file cannot be read,
    /// or the response is invalid.
    pub async fn read_file(&self, path: impl Into<String>) -> Result<Vec<u8>, VmToolSessionError> {
        let response = self
            .control_request(|id| {
                SessionRequest::ReadFile(ReadFileRequest {
                    id,
                    path: path.into(),
                })
            })
            .await?;
        let SessionResponse::ReadFile(ReadFileResponse {
            contents, error, ..
        }) = response
        else {
            return Err(VmToolSessionError::Protocol(
                "expected a read-file response",
            ));
        };
        match (contents, error) {
            (Some(contents), None) => Ok(contents),
            (None, Some(error)) => Err(VmToolSessionError::Guest(error)),
            _ => Err(VmToolSessionError::Protocol(
                "expected exactly one of contents or error",
            )),
        }
    }

    /// Executes one trusted harness command in the guest.
    ///
    /// # Errors
    ///
    /// Returns an error when the session is closed, the command fails to
    /// start, exceeds its deadline, or returns an invalid response.
    pub async fn command(&self, command: VmCommand) -> Result<VmCommandOutput, VmToolSessionError> {
        let command_timeout = command.timeout;
        let timeout_millis = u64::try_from(command_timeout.as_millis()).unwrap_or(u64::MAX);
        let response = self
            .control_request(|id| {
                SessionRequest::Execute(ExecuteRequest {
                    id,
                    program: command.program,
                    arguments: command.arguments,
                    current_directory: command.current_directory,
                    environment: command.environment,
                    timeout_millis,
                })
            })
            .await?;
        let SessionResponse::Execute(ExecuteResponse {
            exit_code,
            stdout,
            stderr,
            error,
            timed_out,
            ..
        }) = response
        else {
            return Err(VmToolSessionError::Protocol("expected an execute response"));
        };
        match (exit_code, stdout, stderr, error, timed_out) {
            (Some(exit_code), Some(stdout), Some(stderr), None, false) => Ok(VmCommandOutput {
                exit_code,
                stdout,
                stderr,
            }),
            (None, None, None, None, true) => {
                Err(VmToolSessionError::GuestTimeout(command_timeout))
            }
            (None, None, None, Some(error), false) => Err(VmToolSessionError::Guest(error)),
            _ => Err(VmToolSessionError::Protocol(
                "invalid execute response fields",
            )),
        }
    }

    async fn control_request(
        &self,
        make_request: impl FnOnce(u64) -> SessionRequest,
    ) -> Result<SessionResponse, VmToolSessionError> {
        self.control_request_inner(make_request, false).await
    }

    async fn control_request_inner(
        &self,
        make_request: impl FnOnce(u64) -> SessionRequest,
        allow_closing: bool,
    ) -> Result<SessionResponse, VmToolSessionError> {
        let response = self
            .send_request(make_request(0), &Span::current(), allow_closing)
            .await?
            .0;
        Ok(response)
    }

    async fn send_request(
        &self,
        mut request: SessionRequest,
        span: &Span,
        allow_closing: bool,
    ) -> Result<(SessionResponse, usize), VmToolSessionError> {
        if self.inner.closing.load(Ordering::Acquire) && !allow_closing {
            return Err(VmToolSessionError::Closed);
        }
        self.ensure_reader().await?;
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        set_request_id(&mut request, id);
        span.record("rpc.request.id", id);
        span.record("vm.session.first_call", id == 0);
        let encoded = serde_json::to_string(&request)?;
        span.record("rpc.request.bytes", encoded.len());
        record_vm_content(span, "tool.request", &encoded);

        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = lock_unpoisoned(&self.inner.pending);
            if let Some(error) = &pending.closed {
                return Err(VmToolSessionError::Router(error.clone()));
            }
            pending.requests.insert(
                id,
                PendingResponse {
                    span: span.clone(),
                    response: sender,
                },
            );
        }
        let mut guard = PendingRequestGuard {
            inner: Arc::downgrade(&self.inner),
            id,
            armed: true,
        };
        let queued_at = Instant::now();
        let write_result = async {
            let mut input = self.inner.input.lock().await;
            span.record("rpc.queue.duration_ns", elapsed_ns(queued_at));
            let input = input.as_mut().ok_or(VmToolSessionError::Closed)?;
            input.write_all(encoded.as_bytes()).await?;
            input.write_all(b"\n").await?;
            input.flush().await?;
            Ok::<_, VmToolSessionError>(())
        }
        .await;
        write_result?;
        let response = receiver.await.map_err(|_| VmToolSessionError::Closed)?;
        guard.armed = false;
        response.map_err(VmToolSessionError::Router)
    }

    async fn ensure_reader(&self) -> Result<(), VmToolSessionError> {
        let mut output = self.inner.output.lock().await;
        if let Some(output) = output.take() {
            let inner = Arc::downgrade(&self.inner);
            tokio::spawn(async move {
                route_responses(output, inner).await;
            });
            return Ok(());
        }
        let pending = lock_unpoisoned(&self.inner.pending);
        match &pending.closed {
            Some(error) => Err(VmToolSessionError::Router(error.clone())),
            None => Ok(()),
        }
    }
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        if self.armed
            && let Some(inner) = self.inner.upgrade()
        {
            lock_unpoisoned(&inner.pending).requests.remove(&self.id);
        }
    }
}

async fn route_responses(output: ChildStdout, inner: Weak<VmToolSessionInner>) {
    let mut lines = BufReader::new(output).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                if let Some(inner) = inner.upgrade() {
                    close_pending(&inner, "VM tool console closed");
                }
                return;
            }
            Err(error) => {
                if let Some(inner) = inner.upgrade() {
                    close_pending(&inner, &format!("VM tool console read failed: {error}"));
                }
                return;
            }
        };
        let response = match serde_json::from_str::<SessionResponse>(&line) {
            Ok(response) => response,
            Err(error) => {
                if let Some(inner) = inner.upgrade() {
                    close_pending(&inner, &format!("invalid VM tool response: {error}"));
                }
                return;
            }
        };
        let Some(inner) = inner.upgrade() else {
            return;
        };
        let id = response.id();
        let pending = lock_unpoisoned(&inner.pending).requests.remove(&id);
        if let Some(pending) = pending {
            record_vm_content(&pending.span, "tool.response", &line);
            let _ = pending.response.send(Ok((response, line.len())));
        } else {
            info!(
                target: "nanocodex_vm",
                rpc_response_id = id,
                "discarded response for a cancelled VM request"
            );
        }
    }
}

fn set_request_id(request: &mut SessionRequest, id: u64) {
    match request {
        SessionRequest::Tool(request) => request.id = id,
        SessionRequest::WriteFile(request) => request.id = id,
        SessionRequest::ReadFile(request) => request.id = id,
        SessionRequest::Execute(request) => request.id = id,
        SessionRequest::Shutdown(request) => request.id = id,
    }
}

fn close_pending(inner: &VmToolSessionInner, message: &str) {
    let requests = {
        let mut pending = lock_unpoisoned(&inner.pending);
        if pending.closed.is_none() {
            pending.closed = Some(message.to_owned());
        }
        std::mem::take(&mut pending.requests)
    };
    for (_, request) in requests {
        let _ = request.response.send(Err(message.to_owned()));
    }
}

fn lock_unpoisoned<T>(mutex: &StdMutex<T>) -> StdMutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn control_result(response: ControlResponse) -> Result<(), VmToolSessionError> {
    match response.error {
        None => Ok(()),
        Some(error) => Err(VmToolSessionError::Guest(error)),
    }
}

fn record_vm_result<T, E>(span: &tracing::Span, started_at: Instant, result: &Result<T, E>)
where
    E: std::fmt::Display,
{
    let duration_ns = elapsed_ns(started_at);
    span.record("duration_ns", duration_ns);
    match result {
        Ok(_) => {
            span.record("status", "completed");
            span.record("otel.status_code", "OK");
            span.in_scope(|| {
                info!(
                    target: "nanocodex_vm",
                    duration_ns,
                    status = "completed",
                    "VM operation completed"
                );
            });
        }
        Err(error) => {
            span.record("status", "failed");
            span.record("otel.status_code", "ERROR");
            span.record("error.message", tracing::field::display(error));
            span.in_scope(|| {
                info!(
                    target: "nanocodex_vm",
                    duration_ns,
                    status = "failed",
                    error = %error,
                    "VM operation failed"
                );
            });
        }
    }
}

fn record_vm_content(span: &tracing::Span, kind: &'static str, content: &str) {
    span.in_scope(|| {
        info!(
            target: "nanocodex_vm",
            content_kind = kind,
            content,
            "VM tool content"
        );
    });
}

fn elapsed_ns(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

#[async_trait::async_trait]
impl VmToolClient for VmToolSessionHandle {
    async fn execute(
        &self,
        tool: StandardTool,
        input: ToolInput,
        context: ToolContext<'_>,
    ) -> ToolResult {
        self.request(tool, input, context)
            .await
            .map_err(|error| Box::new(error) as _)
    }
}

pub(crate) async fn serve_guest(workspace: &Path) -> Result<(), VmToolSessionError> {
    serve_guest_io(workspace, tokio::io::stdin(), tokio::io::stdout()).await
}

const MAX_IN_FLIGHT_GUEST_REQUESTS: usize = 64;

async fn serve_guest_io(
    workspace: &Path,
    input: impl AsyncRead + Unpin,
    mut output: impl tokio::io::AsyncWrite + Unpin,
) -> Result<(), VmToolSessionError> {
    let runtime = Arc::new(ToolRuntime::new(workspace, None, None));
    let mut lines = BufReader::new(input).lines();
    let mut requests = JoinSet::new();
    let mut accepting = true;
    let mut shutdown = None;

    let result = async {
        while accepting || !requests.is_empty() {
            tokio::select! {
                joined = requests.join_next(), if !requests.is_empty() => {
                    let response = joined
                        .ok_or(VmToolSessionError::Closed)?
                        .map_err(|error| VmToolSessionError::Guest(error.to_string()))?;
                    write_guest_response(&mut output, &response).await?;
                }
                line = lines.next_line(),
                    if accepting && requests.len() < MAX_IN_FLIGHT_GUEST_REQUESTS =>
                {
                    let Some(line) = line? else {
                        accepting = false;
                        continue;
                    };
                    match serde_json::from_str::<SessionRequest>(&line)? {
                        SessionRequest::Shutdown(request) => {
                            shutdown = Some(request);
                            accepting = false;
                        }
                        request => {
                            let runtime = Arc::clone(&runtime);
                            requests.spawn(async move {
                                execute_guest_request(runtime, request).await
                            });
                        }
                    }
                }
            }
        }
        Ok::<_, VmToolSessionError>(shutdown)
    }
    .await;

    runtime.control().cancel().await;
    if let Some(request) = result? {
        let response = SessionResponse::Shutdown(sync_guest_filesystems(request).await);
        write_guest_response(&mut output, &response).await?;
    }
    Ok(())
}

async fn execute_guest_request(
    runtime: Arc<ToolRuntime>,
    request: SessionRequest,
) -> SessionResponse {
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
        SessionRequest::WriteFile(request) => {
            SessionResponse::WriteFile(write_guest_file(request).await)
        }
        SessionRequest::ReadFile(request) => {
            SessionResponse::ReadFile(read_guest_file(request).await)
        }
        SessionRequest::Execute(request) => {
            SessionResponse::Execute(execute_guest_command(request).await)
        }
        SessionRequest::Shutdown(request) => SessionResponse::Shutdown(ControlResponse {
            id: request.id,
            error: Some("shutdown cannot be dispatched as a concurrent request".to_owned()),
        }),
    }
}

async fn write_guest_response(
    output: &mut (impl tokio::io::AsyncWrite + Unpin),
    response: &SessionResponse,
) -> Result<(), VmToolSessionError> {
    let mut encoded = serde_json::to_vec(response)?;
    encoded.push(b'\n');
    output.write_all(&encoded).await?;
    output.flush().await?;
    Ok(())
}

async fn sync_guest_filesystems(request: ShutdownRequest) -> ControlResponse {
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

async fn write_guest_file(request: WriteFileRequest) -> ControlResponse {
    let result = async {
        let path = PathBuf::from(&request.path);
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("file path has no parent"))?;
        tokio::fs::create_dir_all(parent).await?;
        tokio::fs::write(&path, request.contents).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(request.mode)).await?;
        }
        Ok::<_, std::io::Error>(())
    }
    .await;
    ControlResponse {
        id: request.id,
        error: result.err().map(|error| error.to_string()),
    }
}

async fn read_guest_file(request: ReadFileRequest) -> ReadFileResponse {
    match tokio::fs::read(&request.path).await {
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

async fn execute_guest_command(request: ExecuteRequest) -> ExecuteResponse {
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
    #[cfg(unix)]
    command.process_group(0);

    let timeout = Duration::from_millis(request.timeout_millis);
    match execute_command_to_output(&mut command, timeout).await {
        Ok(Some(output)) => ExecuteResponse {
            id: request.id,
            exit_code: Some(output.status.code().unwrap_or(1)),
            stdout: Some(output.stdout),
            stderr: Some(output.stderr),
            error: None,
            timed_out: false,
        },
        Ok(None) => ExecuteResponse {
            id: request.id,
            exit_code: None,
            stdout: None,
            stderr: None,
            error: None,
            timed_out: true,
        },
        Err(error) => ExecuteResponse {
            id: request.id,
            exit_code: None,
            stdout: None,
            stderr: None,
            error: Some(error.to_string()),
            timed_out: false,
        },
    }
}

async fn execute_command_to_output(
    command: &mut Command,
    timeout: Duration,
) -> std::io::Result<Option<std::process::Output>> {
    let mut child = command.spawn()?;
    let process_group = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .map(Pid::from_raw);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("guest command stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("guest command stderr was not piped"))?;
    let stdout = tokio::spawn(read_to_end(stdout));
    let stderr = tokio::spawn(read_to_end(stderr));

    let status = if let Ok(status) = tokio::time::timeout(timeout, child.wait()).await {
        status?
    } else {
        if let Some(process_group) = process_group {
            match killpg(process_group, Signal::SIGKILL) {
                Ok(()) | Err(Errno::ESRCH) => {}
                Err(error) => return Err(std::io::Error::other(error)),
            }
        } else {
            child.start_kill()?;
        }
        child.wait().await?;
        stdout.await.map_err(std::io::Error::other)??;
        stderr.await.map_err(std::io::Error::other)??;
        return Ok(None);
    };
    let stdout = stdout.await.map_err(std::io::Error::other)??;
    let stderr = stderr.await.map_err(std::io::Error::other)??;
    Ok(Some(std::process::Output {
        status,
        stdout,
        stderr,
    }))
}

async fn read_to_end(mut reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    reader.read_to_end(&mut output).await?;
    Ok(output)
}

#[cfg(test)]
mod tracing_tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use nanocodex_tools::{StandardTool, ToolContext, ToolInput};
    use serde_json::{json, value::to_raw_value};
    use tracing::{Id, Instrument, Subscriber, field::Visit, span::Attributes};
    use tracing_subscriber::{
        Layer, layer::Context as LayerContext, prelude::*, registry::LookupSpan,
    };

    use super::VmToolSession;

    static TRACE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[derive(Clone, Default)]
    struct TraceCapture {
        spans: Arc<Mutex<HashMap<u64, CapturedSpan>>>,
        names: Arc<Mutex<Vec<&'static str>>>,
    }

    struct CapturedSpan {
        name: &'static str,
        parent: Option<u64>,
        fields: HashMap<String, String>,
    }

    struct FieldCapture<'a>(&'a mut HashMap<String, String>);

    impl Visit for FieldCapture<'_> {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    impl<S> Layer<S> for TraceCapture
    where
        S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    {
        fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: LayerContext<'_, S>) {
            self.names
                .lock()
                .unwrap()
                .push(attributes.metadata().name());
            let parent = attributes
                .parent()
                .map(|parent| parent.clone().into_u64())
                .or_else(|| {
                    attributes
                        .is_contextual()
                        .then(|| context.current_span().id().map(Id::into_u64))
                        .flatten()
                });
            let mut fields = HashMap::new();
            attributes.record(&mut FieldCapture(&mut fields));
            self.spans.lock().unwrap().insert(
                id.clone().into_u64(),
                CapturedSpan {
                    name: attributes.metadata().name(),
                    parent,
                    fields,
                },
            );
        }

        fn on_record(
            &self,
            id: &Id,
            values: &tracing::span::Record<'_>,
            _context: LayerContext<'_, S>,
        ) {
            if let Some(span) = self.spans.lock().unwrap().get_mut(&id.clone().into_u64()) {
                values.record(&mut FieldCapture(&mut span.fields));
            }
        }
    }

    #[test]
    fn vm_rpc_is_timed_and_parented_to_the_calling_tool() {
        let _test_guard = TRACE_TEST_LOCK.lock().unwrap();
        let response = r#"{"kind":"tool","payload":{"id":0,"execution":{"output":"ok","success":true,"code_mode_value":null,"metadata":null,"process_trace":null},"error":null}}"#;
        let script = format!("IFS= read -r request\nprintf '%s\\n' '{response}'");
        let mut command = tokio::process::Command::new("/bin/sh");
        command.arg("-c").arg(script);
        let capture = TraceCapture::default();
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(capture.clone()));
        tracing::callsite::rebuild_interest_cache();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        tracing::dispatcher::with_default(&dispatch, || {
            runtime.block_on(async {
                let session = VmToolSession::spawn(&mut command).unwrap();
                let context = ToolContext {
                    model: "test-model",
                    session_id: "test-session",
                    call_id: "test-call",
                    history: &[],
                    output_token_budget: 1_000,
                };
                let execution = session
                    .handle()
                    .request(
                        StandardTool::ExecCommand,
                        ToolInput::Function(to_raw_value(&json!({"cmd": "true"})).unwrap()),
                        context,
                    )
                    .instrument(tracing::info_span!("test.tool.execute"))
                    .await
                    .unwrap();
                assert!(execution.success);
            });
        });

        let spans = capture.spans.lock().unwrap();
        let (tool_id, _) = spans
            .iter()
            .find(|(_, span)| span.name == "test.tool.execute")
            .unwrap();
        let rpc = spans
            .values()
            .find(|span| span.name == "vm.tool.rpc")
            .unwrap();
        assert_eq!(rpc.parent, Some(*tool_id));
        assert_eq!(
            rpc.fields.get("status").map(String::as_str),
            Some("completed")
        );
        assert_eq!(
            rpc.fields.get("vm.session.first_call").map(String::as_str),
            Some("true")
        );
        assert!(rpc.fields.contains_key("rpc.queue.duration_ns"));
        assert!(rpc.fields.contains_key("duration_ns"));
        assert!(capture.names.lock().unwrap().contains(&"vm.session.spawn"));
    }

    #[test]
    fn next_request_discards_a_cancelled_requests_late_response() {
        let _test_guard = TRACE_TEST_LOCK.lock().unwrap();
        let first = r#"{"kind":"write_file","payload":{"id":0,"error":null}}"#;
        let second = r#"{"kind":"write_file","payload":{"id":1,"error":null}}"#;
        let script = format!(
            "IFS= read -r first\nsleep 0.05\nprintf '%s\\n' '{first}'\nIFS= read -r second\nprintf '%s\\n' '{second}'"
        );
        let mut command = tokio::process::Command::new("/bin/sh");
        command.arg("-c").arg(script);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let session = VmToolSession::spawn(&mut command).unwrap();
            let cancelled = tokio::time::timeout(
                Duration::from_millis(10),
                session.write_file("/first", Vec::new(), 0o600),
            )
            .await;
            assert!(cancelled.is_err());

            session
                .write_file("/second", Vec::new(), 0o600)
                .await
                .unwrap();
        });
    }

    #[test]
    fn concurrent_handles_multiplex_out_of_order_responses() {
        let _test_guard = TRACE_TEST_LOCK.lock().unwrap();
        let first = r#"{"kind":"write_file","payload":{"id":1,"error":null}}"#;
        let second = r#"{"kind":"write_file","payload":{"id":0,"error":null}}"#;
        let script =
            format!("IFS= read -r first\nIFS= read -r second\nprintf '%s\\n' '{first}' '{second}'");
        let mut command = tokio::process::Command::new("/bin/sh");
        command.arg("-c").arg(script);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let session = VmToolSession::spawn(&mut command).unwrap();
            let completed = tokio::time::timeout(Duration::from_secs(1), async {
                tokio::join!(
                    session.write_file("/first", Vec::new(), 0o600),
                    session.write_file("/second", Vec::new(), 0o600),
                )
            })
            .await
            .expect("both requests should be written before either response");
            completed.0.unwrap();
            completed.1.unwrap();
        });
    }
}

#[cfg(test)]
mod guest_command_tests {
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::{execute_guest_command, serve_guest_io};
    use crate::protocol::{ExecuteRequest, SessionRequest, SessionResponse, ShutdownRequest};

    #[tokio::test]
    async fn timeout_kills_descendants_holding_output_pipes() {
        let started_at = Instant::now();
        let response = execute_guest_command(ExecuteRequest {
            id: 1,
            program: "/bin/sh".to_owned(),
            arguments: vec!["-c".to_owned(), "sleep 30 & wait".to_owned()],
            current_directory: "/".to_owned(),
            environment: Vec::new(),
            timeout_millis: 25,
        })
        .await;

        assert!(response.timed_out);
        assert!(response.error.is_none());
        assert!(started_at.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn guest_dispatches_independent_requests_concurrently() {
        let workspace = tempfile::tempdir().unwrap();
        let marker = workspace.path().join("second-started");
        let (host, guest) = tokio::io::duplex(64 * 1024);
        let (host_read, mut host_write) = tokio::io::split(host);
        let (guest_read, guest_write) = tokio::io::split(guest);
        let guest_task = tokio::spawn({
            let workspace = workspace.path().to_owned();
            async move { serve_guest_io(&workspace, guest_read, guest_write).await }
        });

        let requests = [
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
            }),
            SessionRequest::Execute(ExecuteRequest {
                id: 1,
                program: "/usr/bin/touch".to_owned(),
                arguments: vec![marker.to_string_lossy().into_owned()],
                current_directory: workspace.path().to_string_lossy().into_owned(),
                environment: Vec::new(),
                timeout_millis: 5_000,
            }),
            SessionRequest::Shutdown(ShutdownRequest { id: 2 }),
        ];
        for request in requests {
            host_write
                .write_all(&serde_json::to_vec(&request).unwrap())
                .await
                .unwrap();
            host_write.write_all(b"\n").await.unwrap();
        }
        drop(host_write);

        let mut responses = BufReader::new(host_read).lines();
        let mut first_succeeded = false;
        for _ in 0..3 {
            let line = responses.next_line().await.unwrap().unwrap();
            if let SessionResponse::Execute(response) =
                serde_json::from_str::<SessionResponse>(&line).unwrap()
                && response.id == 0
            {
                first_succeeded = !response.timed_out && response.error.is_none();
            }
        }
        guest_task.await.unwrap().unwrap();
        assert!(first_succeeded);
        assert!(marker.is_file());
    }
}
