use std::{
    collections::HashMap,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::{
        Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use nanocodex_tools::{StandardTool, ToolContext, ToolExecution, ToolInput, ToolResult};
use nanovm::{
    EgressLease, GuestCommand, PrivateVmProcessConfig, VmConfig, VmProcessConfig, VmProcessError,
};
use thiserror::Error;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, Semaphore, mpsc, oneshot},
};
use tracing::{Instrument, Span, info, info_span};

use crate::{
    VmToolClient,
    protocol::{
        ControlResponse, ExecuteRequest, ExecuteResponse, ReadFileRequest, ReadFileResponse,
        SessionRequest, SessionResponse, ShutdownRequest, ToolRequest, WireToolContext,
        WireToolInput, WriteFileRequest,
    },
};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_COMMAND_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_GUEST_IN_FLIGHT_REQUESTS: usize = 64;
const MAX_HOST_IN_FLIGHT_REQUESTS: usize = MAX_GUEST_IN_FLIGHT_REQUESTS - 1;
const REQUEST_QUEUE_CAPACITY: usize = MAX_GUEST_IN_FLIGHT_REQUESTS;

/// One trusted command executed by the evaluation harness inside the guest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VmCommand {
    program: String,
    arguments: Vec<String>,
    current_directory: String,
    environment: Vec<(String, String)>,
    timeout: Duration,
    max_output_bytes: usize,
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
            max_output_bytes: DEFAULT_COMMAND_OUTPUT_BYTES,
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

    /// Bounds the combined stdout and stderr retained by this command.
    #[must_use]
    pub const fn max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
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

    #[error("guest command output exceeded the {0}-byte limit")]
    GuestOutputLimit(usize),

    #[error("invalid VM tool response: {0}")]
    Protocol(&'static str),

    #[error("VM tool protocol frame exceeded the {MAX_FRAME_BYTES}-byte limit")]
    FrameTooLarge,

    #[error("the VMM did not exit within {0:?} after guest shutdown")]
    ShutdownTimeout(Duration),

    #[error("the VMM exited unsuccessfully after guest shutdown: {0}")]
    VmmExit(ExitStatus),

    #[error("egress was already provisioned for this VM session")]
    EgressAlreadyProvisioned,

    #[error("cannot shut down the VM while {0} sibling capabilities are still alive")]
    ActiveCapabilities(usize),

    #[error("egress guest file path is not valid UTF-8: {0}")]
    EgressFilePath(PathBuf),

    #[error(transparent)]
    VmProcess(#[from] VmProcessError),
}

/// Owner of one persistent VMM child carrying workspace tool calls.
///
/// Keep this value alive for the complete root-agent tree. Clone
/// [`VmToolSessionHandle`] or [`crate::VmTools`] into each driver's tool
/// factory; all of those handles route to this one VM.
pub struct VmToolSession {
    handle: VmToolSessionHandle,
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
    input: mpsc::Sender<Vec<u8>>,
    request_slots: Semaphore,
    output: Mutex<Option<ChildStdout>>,
    pending: StdMutex<PendingState>,
    child: StdMutex<Option<Child>>,
    egress: StdMutex<Option<EgressLease>>,
    process_config: StdMutex<Option<PrivateVmProcessConfig>>,
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
    /// Configures, spawns, and provisions one VM from the same egress lease.
    ///
    /// `command` must invoke a dedicated VMM process that accepts the private
    /// [`VmProcessConfig`] path as its next argument. This method appends that
    /// path, starts the process, waits for the guest tool server, provisions
    /// public egress files, and retains provider guards with every returned
    /// tool capability.
    ///
    /// Prefer this operation to separately calling [`EgressLease::configure`],
    /// [`Self::spawn`], and [`Self::provision_egress`]: consuming the lease here
    /// prevents launch-time environment and retained provider state from
    /// diverging.
    ///
    /// # Errors
    ///
    /// Returns an error when private configuration cannot be written, the VMM
    /// cannot start, or guest egress provisioning fails.
    pub async fn spawn_configured(
        mut command: Command,
        vm: VmConfig,
        guest: GuestCommand,
        egress: EgressLease,
    ) -> Result<Self, VmToolSessionError> {
        let (vm, guest) = egress.configure(vm, &guest);
        let process_config = VmProcessConfig::new(vm, guest).write_private()?;
        command.arg(process_config.path());
        let session = Self::spawn(&mut command)?;
        *lock_unpoisoned(&session.handle.inner.process_config) = Some(process_config);
        session.provision_egress(egress).await?;
        Ok(session)
    }

    /// Spawns a VMM command whose guest process runs the companion guest server.
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
            let (input_sender, input_receiver) = mpsc::channel(REQUEST_QUEUE_CAPACITY);
            let inner = Arc::new(VmToolSessionInner {
                spawned_at: Instant::now(),
                next_id: AtomicU64::new(0),
                closing: AtomicBool::new(false),
                input: input_sender,
                request_slots: Semaphore::new(MAX_HOST_IN_FLIGHT_REQUESTS),
                output: Mutex::new(Some(output)),
                pending: StdMutex::new(PendingState::default()),
                child: StdMutex::new(Some(child)),
                egress: StdMutex::new(None),
                process_config: StdMutex::new(None),
            });
            tokio::spawn(write_requests(
                input,
                input_receiver,
                Arc::downgrade(&inner),
            ));
            Ok(Self {
                handle: VmToolSessionHandle { inner },
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
    pub async fn provision_egress(&self, egress: EgressLease) -> Result<(), VmToolSessionError> {
        let files = egress.guest_files().cloned().collect::<Vec<_>>();
        {
            let mut provisioned = lock_unpoisoned(&self.handle.inner.egress);
            if provisioned.is_some() {
                return Err(VmToolSessionError::EgressAlreadyProvisioned);
            }
            // Retain revocable provider state even when provisioning fails.
            // Tool handles keep the lease alive with the VMM, so dropping the
            // launch owner cannot silently revoke an active agent tree.
            *provisioned = Some(egress);
        }
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
    /// The operation rejects live sibling capabilities so it cannot stop the
    /// VM while another driver in the same agent tree is using it. Because the
    /// owner is borrowed, callers can drop those capabilities and retry.
    ///
    /// # Errors
    ///
    /// Returns an error when the guest cannot acknowledge the request, the
    /// VMM does not stop promptly, or it exits unsuccessfully.
    pub async fn shutdown(&self) -> Result<(), VmToolSessionError> {
        let sibling_capabilities = Arc::strong_count(&self.handle.inner).saturating_sub(1);
        if sibling_capabilities != 0 {
            return Err(VmToolSessionError::ActiveCapabilities(sibling_capabilities));
        }
        self.handle.inner.closing.store(true, Ordering::Release);
        let response = self
            .handle
            .control_request_inner(|id| SessionRequest::Shutdown(ShutdownRequest { id }), true)
            .await?;
        let SessionResponse::Shutdown(response) = response else {
            return Err(VmToolSessionError::Protocol("expected a shutdown response"));
        };
        control_result(response)?;

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

impl Drop for VmToolSessionInner {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::Release);
        close_pending(self, "last VM session capability was dropped");
        if let Some(child) = lock_unpoisoned(&self.child).as_mut() {
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
        let max_output_bytes = command.max_output_bytes;
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
                    max_output_bytes,
                })
            })
            .await?;
        let SessionResponse::Execute(ExecuteResponse {
            exit_code,
            stdout,
            stderr,
            error,
            timed_out,
            output_limit_exceeded,
            ..
        }) = response
        else {
            return Err(VmToolSessionError::Protocol("expected an execute response"));
        };
        match (
            exit_code,
            stdout,
            stderr,
            error,
            timed_out,
            output_limit_exceeded,
        ) {
            (Some(exit_code), Some(stdout), Some(stderr), None, false, false) => {
                Ok(VmCommandOutput {
                    exit_code,
                    stdout,
                    stderr,
                })
            }
            (None, None, None, None, true, false) => {
                Err(VmToolSessionError::GuestTimeout(command_timeout))
            }
            (None, None, None, None, false, true) => {
                Err(VmToolSessionError::GuestOutputLimit(max_output_bytes))
            }
            (None, None, None, Some(error), false, false) => Err(VmToolSessionError::Guest(error)),
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
        let _request_slot = if allow_closing {
            None
        } else {
            let permit = self
                .inner
                .request_slots
                .acquire()
                .await
                .map_err(|_| VmToolSessionError::Closed)?;
            if self.inner.closing.load(Ordering::Acquire) {
                return Err(VmToolSessionError::Closed);
            }
            Some(permit)
        };
        self.ensure_reader().await?;
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        set_request_id(&mut request, id);
        span.record("rpc.request.id", id);
        span.record("vm.session.first_call", id == 0);
        let encoded = serde_json::to_string(&request)?;
        if encoded.len() > MAX_FRAME_BYTES {
            return Err(VmToolSessionError::FrameTooLarge);
        }
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
        let mut frame = encoded.into_bytes();
        frame.push(b'\n');
        self.inner
            .input
            .send(frame)
            .await
            .map_err(|_| VmToolSessionError::Closed)?;
        span.record("rpc.queue.duration_ns", elapsed_ns(queued_at));
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

async fn write_requests(
    mut input: ChildStdin,
    mut requests: mpsc::Receiver<Vec<u8>>,
    inner: Weak<VmToolSessionInner>,
) {
    while let Some(frame) = requests.recv().await {
        let result = async {
            input.write_all(&frame).await?;
            input.flush().await
        }
        .await;
        if let Err(error) = result {
            if let Some(inner) = inner.upgrade() {
                close_pending(&inner, &format!("VM tool console write failed: {error}"));
            }
            return;
        }
    }
}

async fn route_responses(output: ChildStdout, inner: Weak<VmToolSessionInner>) {
    let mut output = BufReader::new(output);
    loop {
        let line = match read_frame(&mut output).await {
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
        let response = match serde_json::from_slice::<SessionResponse>(&line) {
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
            record_vm_content(
                &pending.span,
                "tool.response",
                &String::from_utf8_lossy(&line),
            );
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

async fn read_frame(
    reader: &mut (impl AsyncBufRead + Unpin),
) -> Result<Option<Vec<u8>>, VmToolSessionError> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(VmToolSessionError::Closed)
            };
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if frame.len().saturating_add(newline) > MAX_FRAME_BYTES {
                return Err(VmToolSessionError::FrameTooLarge);
            }
            frame.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return Ok(Some(frame));
        }
        if frame.len().saturating_add(available.len()) > MAX_FRAME_BYTES {
            return Err(VmToolSessionError::FrameTooLarge);
        }
        let consumed = available.len();
        frame.extend_from_slice(available);
        reader.consume(consumed);
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

    use super::{VmToolSession, VmToolSessionError};

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

    #[test]
    fn cancelled_partial_write_cannot_corrupt_the_next_request() {
        let _test_guard = TRACE_TEST_LOCK.lock().unwrap();
        let first = r#"{"kind":"write_file","payload":{"id":0,"error":null}}"#;
        let second = r#"{"kind":"write_file","payload":{"id":1,"error":null}}"#;
        let script = format!(
            "sleep 0.05\nIFS= read -r first\nprintf '%s\\n' '{first}'\nIFS= read -r second\nprintf '%s\\n' '{second}'"
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
                session.write_file("/large", vec![b'x'; 256 * 1024], 0o600),
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
    fn tool_capabilities_own_the_vmm_and_egress_lifetime() {
        let _test_guard = TRACE_TEST_LOCK.lock().unwrap();
        let mut command = tokio::process::Command::new("/bin/sh");
        command.arg("-c").arg("sleep 30");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let session = VmToolSession::spawn(&mut command).unwrap();
            let guard = Arc::new(());
            let weak_guard = Arc::downgrade(&guard);
            let mut egress = nanovm::EgressLease::disabled();
            egress.retain(guard);
            session.provision_egress(egress).await.unwrap();
            let tools = session.tools();

            drop(session);
            assert!(
                weak_guard.upgrade().is_some(),
                "dropping the launch owner must not revoke an active tool tree"
            );

            drop(tools);
            assert!(
                weak_guard.upgrade().is_none(),
                "the last VM capability must release retained egress state"
            );
        });
    }

    #[test]
    fn graceful_shutdown_rejects_live_sibling_capabilities() {
        let _test_guard = TRACE_TEST_LOCK.lock().unwrap();
        let response = r#"{"kind":"shutdown","payload":{"id":0,"error":null}}"#;
        let script = format!("IFS= read -r request\nprintf '%s\\n' '{response}'");
        let mut command = tokio::process::Command::new("/bin/sh");
        command.arg("-c").arg(script);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let session = VmToolSession::spawn(&mut command).unwrap();
            let handle = session.handle();
            assert!(matches!(
                session.shutdown().await,
                Err(VmToolSessionError::ActiveCapabilities(1))
            ));
            drop(handle);
            session.shutdown().await.unwrap();
        });
    }

    #[test]
    fn configured_spawn_retains_private_input_until_the_vmm_has_loaded_it() {
        let _test_guard = TRACE_TEST_LOCK.lock().unwrap();
        let response = r#"{"kind":"shutdown","payload":{"id":0,"error":null}}"#;
        let script = format!(
            "config=$1\nsleep 0.05\ntest -f \"$config\" || exit 9\nIFS= read -r request\nprintf '%s\\n' '{response}'"
        );
        let mut command = tokio::process::Command::new("/bin/sh");
        command.arg("-c").arg(script).arg("vm-test");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let session = VmToolSession::spawn_configured(
                command,
                nanovm::VmConfig::ext4("/unused/root.ext4"),
                nanovm::GuestCommand::new("/bin/true"),
                nanovm::EgressLease::disabled(),
            )
            .await
            .unwrap();
            session.shutdown().await.unwrap();
        });
    }
}
