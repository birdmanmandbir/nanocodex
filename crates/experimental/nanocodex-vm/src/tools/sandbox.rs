use std::{
    collections::HashMap,
    ffi::OsStr,
    fs, io,
    os::fd::AsFd,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError},
    time::Duration,
};

use caps::CapSet;
use futures_util::TryStreamExt;
use nix::{
    fcntl::{FcntlArg, FdFlag, fcntl},
    mount::{MntFlags, MsFlags, mount, umount2},
    sched::{CloneFlags, unshare},
    sys::prctl,
    unistd::{Gid, Uid, chdir, getgid, getpid, getuid, pivot_root, sethostname},
};
use rtnetlink::{LinkUnspec, new_connection};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, oneshot},
    task::JoinSet,
};

use super::{
    guest::{VmGuestError, read_frame, serve_io_with_environment, write_response},
    protocol::{
        CancelRequest, ControlResponse, ExecuteResponse, ReadFileResponse, ScopedResponse,
        SessionRequest, SessionResponse, ToolResponse,
    },
};

const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const ATTEMPT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const CGROUP_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const CGROUP_POLL_INTERVAL: Duration = Duration::from_millis(10);
const SUPERVISOR_SENTINEL: &str = "/var/lib/nanocodex/supervisor.sentinel";
const SUPERVISOR_SENTINEL_CONTENTS: &[u8] = b"nanocodex-owned-supervisor-v1\n";

pub(super) struct TaskSupervisorConfig {
    pub lower_device: PathBuf,
    pub lower_mount: PathBuf,
    pub attempts_root: PathBuf,
}

struct TaskSupervisor {
    config: TaskSupervisorConfig,
    executable: PathBuf,
    state: Mutex<SupervisorState>,
}

enum SupervisorState {
    Idle,
    Starting,
    Active(Arc<AttemptProcess>),
    Finishing,
    Poisoned,
}

struct AttemptProcess {
    generation: u64,
    directory: PathBuf,
    cgroup: PathBuf,
    input: Mutex<ChildStdin>,
    child: Mutex<Option<Child>>,
    pending: StdMutex<ChildPending>,
}

#[derive(Default)]
struct ChildPending {
    closed: Option<String>,
    requests: HashMap<u64, oneshot::Sender<Result<SessionResponse, String>>>,
}

pub(super) async fn serve_task_supervisor(
    config: TaskSupervisorConfig,
) -> Result<(), VmGuestError> {
    prepare_supervisor(&config)?;
    let supervisor = Arc::new(TaskSupervisor {
        config,
        executable: std::env::current_exe()?,
        state: Mutex::new(SupervisorState::Idle),
    });
    serve_supervisor_io(supervisor, tokio::io::stdin(), tokio::io::stdout()).await
}

async fn serve_supervisor_io(
    supervisor: Arc<TaskSupervisor>,
    input: impl tokio::io::AsyncRead + Unpin,
    mut output: impl tokio::io::AsyncWrite + Unpin,
) -> Result<(), VmGuestError> {
    let mut input = BufReader::new(input);
    let mut requests = JoinSet::<SessionResponse>::new();
    let mut active = HashMap::<u64, (tokio::task::AbortHandle, Option<u64>)>::new();
    let mut accepting = true;
    let mut shutdown = None;

    while accepting || !requests.is_empty() {
        tokio::select! {
            joined = requests.join_next(), if !requests.is_empty() => {
                match joined.ok_or(VmGuestError::Closed)? {
                    Ok(response) => {
                        active.remove(&response.id());
                        write_response(&mut output, &response, MAX_FRAME_BYTES).await?;
                    }
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => return Err(VmGuestError::Task(error.to_string())),
                }
            }
            frame = read_frame(&mut input), if accepting && requests.len() < 64 => {
                let Some(frame) = frame? else {
                    accepting = false;
                    requests.abort_all();
                    continue;
                };
                let request = serde_json::from_slice::<SessionRequest>(&frame)?;
                match request {
                    SessionRequest::Shutdown(request) => {
                        shutdown = Some(request);
                        accepting = false;
                        requests.abort_all();
                        active.clear();
                    }
                    SessionRequest::Cancel(request) => {
                        if let Some((task, generation)) = active.remove(&request.target_id) {
                            task.abort();
                            if let Some(generation) = request.generation.or(generation) {
                                supervisor.cancel(generation, request.id, request.target_id).await;
                            }
                        }
                        write_response(
                            &mut output,
                            &SessionResponse::Cancel(ControlResponse {
                                id: request.id,
                                error: None,
                            }),
                            MAX_FRAME_BYTES,
                        )
                        .await?;
                    }
                    request => {
                        let id = request.id();
                        if active.contains_key(&id) {
                            return Err(VmGuestError::DuplicateRequestId(id));
                        }
                        let generation = match &request {
                            SessionRequest::Scoped(request) => Some(request.generation),
                            _ => None,
                        };
                        let supervisor = Arc::clone(&supervisor);
                        let task = requests.spawn(async move {
                            supervisor.execute(request).await
                        });
                        active.insert(id, (task, generation));
                    }
                }
            }
        }
    }

    if let Some(request) = shutdown {
        let error = supervisor
            .shutdown_active()
            .await
            .err()
            .map(|error| error.to_string());
        write_response(
            &mut output,
            &SessionResponse::Shutdown(ControlResponse {
                id: request.id,
                error,
            }),
            MAX_FRAME_BYTES,
        )
        .await?;
    }
    Ok(())
}

impl TaskSupervisor {
    async fn execute(&self, request: SessionRequest) -> SessionResponse {
        match request {
            SessionRequest::Ready(request) => SessionResponse::Ready(ControlResponse {
                id: request.id,
                error: None,
            }),
            SessionRequest::BeginAttempt(request) => {
                let id = request.id;
                let result = self
                    .begin(
                        request.generation,
                        PathBuf::from(request.workspace),
                        request.environment,
                    )
                    .await;
                SessionResponse::BeginAttempt(ControlResponse {
                    id,
                    error: result.err().map(|error| error.to_string()),
                })
            }
            SessionRequest::Scoped(request) => {
                let id = request.id;
                let generation = request.generation;
                let result = self.scoped(generation, *request.request).await;
                match result {
                    Ok(response) => SessionResponse::Scoped(ScopedResponse {
                        id,
                        generation,
                        response: Some(Box::new(response)),
                        error: None,
                    }),
                    Err(error) => SessionResponse::Scoped(ScopedResponse {
                        id,
                        generation,
                        response: None,
                        error: Some(error.to_string()),
                    }),
                }
            }
            SessionRequest::FinishAttempt(request) => {
                let id = request.id;
                let result = self.finish(request.generation, request.retain).await;
                SessionResponse::FinishAttempt(ControlResponse {
                    id,
                    error: result.err().map(|error| error.to_string()),
                })
            }
            SessionRequest::Tool(request) => SessionResponse::Tool(ToolResponse::failed(
                request.id,
                "task supervisor requests must carry an attempt generation".to_owned(),
            )),
            SessionRequest::WriteFile(request) => {
                SessionResponse::WriteFile(control_error(request.id, "unscoped write"))
            }
            SessionRequest::CreateDirectory(request) => {
                SessionResponse::CreateDirectory(control_error(request.id, "unscoped mkdir"))
            }
            SessionRequest::ReadFile(request) => SessionResponse::ReadFile(ReadFileResponse {
                id: request.id,
                contents: None,
                error: Some("task supervisor requests must carry an attempt generation".to_owned()),
            }),
            SessionRequest::Execute(request) => SessionResponse::Execute(ExecuteResponse {
                id: request.id,
                exit_code: None,
                stdout: None,
                stderr: None,
                error: Some("task supervisor requests must carry an attempt generation".to_owned()),
                timed_out: false,
                output_limit_exceeded: false,
            }),
            SessionRequest::Cancel(request) => SessionResponse::Cancel(control_error(
                request.id,
                "cancel is handled by the supervisor loop",
            )),
            SessionRequest::Shutdown(request) => SessionResponse::Shutdown(control_error(
                request.id,
                "shutdown is handled by the supervisor loop",
            )),
        }
    }

    async fn begin(
        &self,
        generation: u64,
        workspace: PathBuf,
        environment: Vec<(String, String)>,
    ) -> Result<(), VmGuestError> {
        if !is_normal_absolute(&workspace) {
            return Err(sandbox_error(
                "attempt workspace must be a normal absolute path",
            ));
        }
        validate_supervisor_sentinel()?;
        {
            let mut state = self.state.lock().await;
            match *state {
                SupervisorState::Idle => *state = SupervisorState::Starting,
                SupervisorState::Poisoned => {
                    return Err(sandbox_error("task supervisor is poisoned"));
                }
                SupervisorState::Starting
                | SupervisorState::Active(_)
                | SupervisorState::Finishing => {
                    return Err(sandbox_error("another attempt is already active"));
                }
            }
        }

        let process = match self
            .spawn_attempt(generation, &workspace, &environment)
            .await
        {
            Ok(process) => process,
            Err(error) => {
                *self.state.lock().await = SupervisorState::Poisoned;
                return Err(error);
            }
        };
        let ready = process
            .request(SessionRequest::Ready(super::protocol::ReadyRequest {
                id: 0,
            }))
            .await;
        if let Err(error) = ready {
            let _ = process.kill_and_drain().await;
            *self.state.lock().await = SupervisorState::Poisoned;
            return Err(error);
        }
        let mut state = self.state.lock().await;
        if !matches!(*state, SupervisorState::Starting) {
            *state = SupervisorState::Poisoned;
            return Err(sandbox_error("attempt state changed during setup"));
        }
        *state = SupervisorState::Active(process);
        Ok(())
    }

    async fn scoped(
        &self,
        generation: u64,
        request: SessionRequest,
    ) -> Result<SessionResponse, VmGuestError> {
        if matches!(
            request,
            SessionRequest::BeginAttempt(_)
                | SessionRequest::Scoped(_)
                | SessionRequest::FinishAttempt(_)
                | SessionRequest::Shutdown(_)
        ) {
            return Err(sandbox_error("nested attempt lifecycle request"));
        }
        let process = {
            let state = self.state.lock().await;
            match &*state {
                SupervisorState::Active(process) if process.generation == generation => {
                    Arc::clone(process)
                }
                SupervisorState::Active(_) | SupervisorState::Idle => {
                    return Err(sandbox_error("stale or inactive attempt generation"));
                }
                SupervisorState::Starting | SupervisorState::Finishing => {
                    return Err(sandbox_error("attempt lifecycle transition is in progress"));
                }
                SupervisorState::Poisoned => {
                    return Err(sandbox_error("task supervisor is poisoned"));
                }
            }
        };
        process.request(request).await
    }

    async fn finish(&self, generation: u64, retain: bool) -> Result<(), VmGuestError> {
        let process = {
            let mut state = self.state.lock().await;
            let process = match &*state {
                SupervisorState::Active(process) if process.generation == generation => {
                    Arc::clone(process)
                }
                SupervisorState::Active(_) | SupervisorState::Idle => {
                    return Err(sandbox_error("stale or inactive attempt generation"));
                }
                SupervisorState::Starting | SupervisorState::Finishing => {
                    return Err(sandbox_error("attempt lifecycle transition is in progress"));
                }
                SupervisorState::Poisoned => {
                    return Err(sandbox_error("task supervisor is poisoned"));
                }
            };
            *state = SupervisorState::Finishing;
            process
        };

        let result = process
            .finish(retain)
            .await
            .and_then(|()| validate_supervisor_sentinel());
        let mut state = self.state.lock().await;
        match result {
            Ok(()) => {
                *state = SupervisorState::Idle;
                Ok(())
            }
            Err(error) => {
                *state = SupervisorState::Poisoned;
                Err(error)
            }
        }
    }

    async fn cancel(&self, generation: u64, id: u64, target_id: u64) {
        let process = {
            let state = self.state.lock().await;
            match &*state {
                SupervisorState::Active(process) if process.generation == generation => {
                    Some(Arc::clone(process))
                }
                _ => None,
            }
        };
        if let Some(process) = process {
            process.cancel(id, target_id).await;
        }
    }

    async fn shutdown_active(&self) -> Result<(), VmGuestError> {
        let process = {
            let mut state = self.state.lock().await;
            match &*state {
                SupervisorState::Active(process) => {
                    let process = Arc::clone(process);
                    *state = SupervisorState::Finishing;
                    Some(process)
                }
                SupervisorState::Starting | SupervisorState::Finishing => {
                    *state = SupervisorState::Poisoned;
                    None
                }
                SupervisorState::Idle | SupervisorState::Poisoned => None,
            }
        };
        if let Some(process) = process {
            process.kill_and_drain().await?;
        }
        validate_supervisor_sentinel()?;
        Ok(())
    }

    async fn spawn_attempt(
        &self,
        generation: u64,
        workspace: &Path,
        environment: &[(String, String)],
    ) -> Result<Arc<AttemptProcess>, VmGuestError> {
        let directory = self.config.attempts_root.join(format!("{generation:020}"));
        let cgroup = PathBuf::from(format!("/sys/fs/cgroup/nanocodex-attempt-{generation}"));
        if directory.exists() || cgroup.exists() {
            return Err(sandbox_error("attempt generation path already exists"));
        }
        for child in ["upper", "work", "root"] {
            fs::create_dir_all(directory.join(child))?;
        }
        let environment_file = directory.join("environment.json");
        let mut environment_output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&environment_file)?;
        serde_json::to_writer(&mut environment_output, environment)?;
        environment_output.sync_all()?;
        fs::create_dir(&cgroup)?;

        let mut child = Command::new(&self.executable)
            .arg("--attempt-helper")
            .arg(generation.to_string())
            .arg(&self.config.lower_mount)
            .arg(&directory)
            .arg(workspace)
            .arg(&cgroup)
            .arg(&environment_file)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| sandbox_error("attempt helper did not expose protocol stdin"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| sandbox_error("attempt helper did not expose protocol stdout"))?;
        let process = Arc::new(AttemptProcess {
            generation,
            directory,
            cgroup,
            input: Mutex::new(input),
            child: Mutex::new(Some(child)),
            pending: StdMutex::new(ChildPending::default()),
        });
        let routed = Arc::clone(&process);
        tokio::spawn(async move {
            routed.route(output).await;
        });
        Ok(process)
    }
}

impl AttemptProcess {
    async fn request(&self, request: SessionRequest) -> Result<SessionResponse, VmGuestError> {
        let id = request.id();
        let encoded = serde_json::to_vec(&request)?;
        if encoded.len() > MAX_FRAME_BYTES {
            return Err(VmGuestError::FrameTooLarge);
        }
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = lock_unpoisoned(&self.pending);
            if let Some(error) = &pending.closed {
                return Err(sandbox_error(error));
            }
            if pending.requests.insert(id, sender).is_some() {
                return Err(VmGuestError::DuplicateRequestId(id));
            }
        }
        let write = async {
            let mut input = self.input.lock().await;
            input.write_all(&encoded).await?;
            input.write_all(b"\n").await?;
            input.flush().await
        }
        .await;
        if let Err(error) = write {
            lock_unpoisoned(&self.pending).requests.remove(&id);
            return Err(error.into());
        }
        receiver
            .await
            .map_err(|_| VmGuestError::Closed)?
            .map_err(|error| sandbox_error(&error))
    }

    async fn cancel(&self, id: u64, target_id: u64) {
        if let Some(pending) = lock_unpoisoned(&self.pending).requests.remove(&target_id) {
            let _ = pending.send(Err("attempt request cancelled".to_owned()));
        }
        let _ = self
            .request(SessionRequest::Cancel(CancelRequest {
                id,
                target_id,
                generation: None,
            }))
            .await;
    }

    async fn route(self: Arc<Self>, output: tokio::process::ChildStdout) {
        let mut output = BufReader::new(output);
        loop {
            let frame = match read_frame(&mut output).await {
                Ok(Some(frame)) => frame,
                Ok(None) => {
                    self.close_pending("attempt protocol closed");
                    return;
                }
                Err(error) => {
                    self.close_pending(&error.to_string());
                    return;
                }
            };
            let response = match serde_json::from_slice::<SessionResponse>(&frame) {
                Ok(response) => response,
                Err(error) => {
                    self.close_pending(&format!("invalid attempt response: {error}"));
                    return;
                }
            };
            if let Some(pending) = lock_unpoisoned(&self.pending)
                .requests
                .remove(&response.id())
            {
                let _ = pending.send(Ok(response));
            }
        }
    }

    fn close_pending(&self, error: &str) {
        let requests = {
            let mut pending = lock_unpoisoned(&self.pending);
            pending.closed = Some(error.to_owned());
            std::mem::take(&mut pending.requests)
        };
        for (_, request) in requests {
            let _ = request.send(Err(error.to_owned()));
        }
    }

    async fn finish(&self, retain: bool) -> Result<(), VmGuestError> {
        let shutdown_id = u64::MAX.saturating_sub(self.generation);
        let graceful = tokio::time::timeout(
            ATTEMPT_SHUTDOWN_TIMEOUT,
            self.request(SessionRequest::Shutdown(super::protocol::ShutdownRequest {
                id: shutdown_id,
            })),
        )
        .await;
        if !matches!(
            graceful,
            Ok(Ok(SessionResponse::Shutdown(ControlResponse {
                error: None,
                ..
            })))
        ) {
            self.kill_cgroup()?;
        }
        self.wait_helper().await?;
        self.kill_cgroup()?;
        self.drain_cgroup().await?;
        fs::remove_dir(&self.cgroup)?;
        if !retain {
            fs::remove_dir_all(&self.directory)?;
        }
        Ok(())
    }

    async fn kill_and_drain(&self) -> Result<(), VmGuestError> {
        self.kill_cgroup()?;
        self.wait_helper().await?;
        self.drain_cgroup().await?;
        fs::remove_dir(&self.cgroup)?;
        Ok(())
    }

    fn kill_cgroup(&self) -> Result<(), VmGuestError> {
        fs::write(self.cgroup.join("cgroup.kill"), b"1\n")?;
        Ok(())
    }

    async fn wait_helper(&self) -> Result<(), VmGuestError> {
        let child = self.child.lock().await.take();
        let Some(mut child) = child else {
            return Ok(());
        };
        match tokio::time::timeout(ATTEMPT_SHUTDOWN_TIMEOUT, child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(error.into()),
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                Err(sandbox_error(
                    "attempt helper did not exit after cgroup kill",
                ))
            }
        }
    }

    async fn drain_cgroup(&self) -> Result<(), VmGuestError> {
        let started = tokio::time::Instant::now();
        loop {
            let processes = fs::read_to_string(self.cgroup.join("cgroup.procs"))?;
            let events = fs::read_to_string(self.cgroup.join("cgroup.events"))?;
            if processes.trim().is_empty() && events.lines().any(|line| line == "populated 0") {
                return Ok(());
            }
            if started.elapsed() >= CGROUP_DRAIN_TIMEOUT {
                return Err(sandbox_error("attempt cgroup did not drain"));
            }
            tokio::time::sleep(CGROUP_POLL_INTERVAL).await;
        }
    }
}

pub(super) async fn run_attempt_helper(
    arguments: &[std::ffi::OsString],
) -> Result<(), VmGuestError> {
    let [
        generation,
        lower,
        directory,
        workspace,
        cgroup,
        environment_file,
    ] = arguments
    else {
        return Err(sandbox_error("attempt helper requires six arguments"));
    };
    let generation = generation
        .to_str()
        .ok_or_else(|| sandbox_error("attempt generation is not UTF-8"))?
        .parse::<u64>()
        .map_err(|error| sandbox_error(format!("invalid attempt generation: {error}")))?;
    let lower = PathBuf::from(lower);
    let directory = PathBuf::from(directory);
    let workspace = PathBuf::from(workspace);
    let cgroup = PathBuf::from(cgroup);
    let environment_file = PathBuf::from(environment_file);
    enter_cgroup(&cgroup)
        .map_err(|error| sandbox_error(format!("enter attempt cgroup: {error}")))?;
    enter_namespaces()
        .map_err(|error| sandbox_error(format!("create attempt namespaces: {error}")))?;
    bring_loopback_up()
        .await
        .map_err(|error| sandbox_error(format!("configure private loopback: {error}")))?;
    prepare_overlay(&lower, &directory, &workspace)
        .map_err(|error| sandbox_error(format!("prepare attempt overlay: {error}")))?;

    let executable = std::env::current_exe()?;
    let mut child = Command::new(executable)
        .arg("--attempt-child")
        .arg(generation.to_string())
        .arg(directory.join("root"))
        .arg(workspace)
        .arg(environment_file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let mut child_input = child
        .stdin
        .take()
        .ok_or_else(|| sandbox_error("attempt runtime has no private protocol input"))?;
    let mut child_output = child
        .stdout
        .take()
        .ok_or_else(|| sandbox_error("attempt runtime has no private protocol output"))?;
    let input_proxy = tokio::spawn(async move {
        let mut host_input = tokio::io::stdin();
        tokio::io::copy(&mut host_input, &mut child_input).await?;
        child_input.shutdown().await
    });
    let output_proxy = tokio::spawn(async move {
        let mut host_output = tokio::io::stdout();
        tokio::io::copy(&mut child_output, &mut host_output).await?;
        host_output.flush().await
    });
    let status = child.wait().await?;
    input_proxy.abort();
    let output_result = tokio::time::timeout(ATTEMPT_SHUTDOWN_TIMEOUT, output_proxy)
        .await
        .map_err(|_| sandbox_error("attempt protocol output proxy did not drain"))?
        .map_err(|error| sandbox_error(format!("attempt protocol output proxy failed: {error}")))?;
    output_result?;
    if status.success() {
        Ok(())
    } else {
        Err(sandbox_error(format!(
            "attempt runtime exited with {status}"
        )))
    }
}

pub(super) async fn run_attempt_child(
    arguments: &[std::ffi::OsString],
) -> Result<(), VmGuestError> {
    let [_generation, root, workspace, environment_file] = arguments else {
        return Err(sandbox_error("attempt child requires four arguments"));
    };
    let root = PathBuf::from(root);
    let workspace = PathBuf::from(workspace);
    let environment_file = PathBuf::from(environment_file);
    let environment =
        serde_json::from_slice::<Vec<(String, String)>>(&fs::read(&environment_file)?)?;
    fs::remove_file(environment_file)?;
    mount_proc(&root).map_err(|error| sandbox_error(format!("mount attempt procfs: {error}")))?;
    pivot_into(&root).map_err(|error| sandbox_error(format!("pivot attempt root: {error}")))?;
    enter_runtime_user_namespace().map_err(|error| {
        sandbox_error(format!("enter least-privilege runtime namespace: {error}"))
    })?;
    protect_runtime_process()
        .map_err(|error| sandbox_error(format!("protect attempt runtime process: {error}")))?;
    serve_io_with_environment(
        &workspace,
        environment,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await
}

fn prepare_supervisor(config: &TaskSupervisorConfig) -> Result<(), VmGuestError> {
    validate_supervisor_sentinel()?;
    let lower_mount = ensure_directory_beneath(Path::new("/"), &config.lower_mount)?;
    ensure_directory_beneath(Path::new("/"), &config.attempts_root)?;
    mount(
        Some(config.lower_device.as_path()),
        lower_mount.as_path(),
        Some(OsStr::new("ext4")),
        MsFlags::MS_RDONLY | MsFlags::MS_NODEV | MsFlags::MS_NOSUID,
        None::<&OsStr>,
    )
    .map_err(|error| sandbox_error(format!("mount overlay: {error}")))?;
    Ok(())
}

fn validate_supervisor_sentinel() -> Result<(), VmGuestError> {
    if fs::read(SUPERVISOR_SENTINEL)? != SUPERVISOR_SENTINEL_CONTENTS {
        return Err(sandbox_error("trusted supervisor sentinel changed"));
    }
    Ok(())
}

fn enter_cgroup(cgroup: &Path) -> Result<(), VmGuestError> {
    fs::write(
        cgroup.join("cgroup.procs"),
        format!("{}\n", getpid().as_raw()),
    )?;
    Ok(())
}

fn enter_namespaces() -> Result<(), VmGuestError> {
    let uid = getuid();
    let gid = getgid();
    unshare(CloneFlags::CLONE_NEWUSER).map_err(nix_error)?;
    write_user_maps(uid, gid)?;
    unshare(
        CloneFlags::CLONE_NEWNS
            | CloneFlags::CLONE_NEWIPC
            | CloneFlags::CLONE_NEWUTS
            | CloneFlags::CLONE_NEWNET
            | CloneFlags::CLONE_NEWPID,
    )
    .map_err(nix_error)?;
    mount(
        None::<&OsStr>,
        Path::new("/"),
        None::<&OsStr>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&OsStr>,
    )
    .map_err(nix_error)?;
    Ok(())
}

fn write_user_maps(uid: Uid, gid: Gid) -> Result<(), VmGuestError> {
    match fs::write("/proc/self/setgroups", b"deny\n") {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    fs::write("/proc/self/uid_map", format!("0 {} 1\n", uid.as_raw()))?;
    fs::write("/proc/self/gid_map", format!("0 {} 1\n", gid.as_raw()))?;
    Ok(())
}

fn enter_runtime_user_namespace() -> Result<(), VmGuestError> {
    let uid = getuid();
    let gid = getgid();
    unshare(CloneFlags::CLONE_NEWUSER).map_err(nix_error)?;
    write_user_maps(uid, gid)
}

fn protect_runtime_process() -> Result<(), VmGuestError> {
    prctl::set_dumpable(false).map_err(nix_error)?;
    set_close_on_exec(std::io::stdin().as_fd())?;
    set_close_on_exec(std::io::stdout().as_fd())?;
    prctl::set_no_new_privs().map_err(nix_error)?;
    for set in [
        CapSet::Bounding,
        CapSet::Ambient,
        CapSet::Inheritable,
        CapSet::Effective,
        CapSet::Permitted,
    ] {
        caps::clear(None, set)
            .map_err(|error| sandbox_error(format!("clear {set:?} capabilities: {error}")))?;
    }
    for set in [
        CapSet::Bounding,
        CapSet::Ambient,
        CapSet::Inheritable,
        CapSet::Effective,
        CapSet::Permitted,
    ] {
        let retained = caps::read(None, set)
            .map_err(|error| sandbox_error(format!("read {set:?} capabilities: {error}")))?;
        if !retained.is_empty() {
            return Err(sandbox_error(format!(
                "runtime retained {set:?} capabilities"
            )));
        }
    }
    Ok(())
}

fn set_close_on_exec(fd: impl AsFd) -> Result<(), VmGuestError> {
    fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
        .map(drop)
        .map_err(nix_error)
}

async fn bring_loopback_up() -> Result<(), VmGuestError> {
    let (connection, handle, _) =
        new_connection().map_err(|error| sandbox_error(format!("open route netlink: {error}")))?;
    let connection = tokio::spawn(connection);
    let result = async {
        let mut links = handle.link().get().match_name("lo".to_owned()).execute();
        let loopback = links
            .try_next()
            .await
            .map_err(|error| sandbox_error(format!("find loopback: {error}")))?
            .ok_or_else(|| sandbox_error("private network namespace has no loopback interface"))?;
        handle
            .link()
            .set(
                LinkUnspec::new_with_index(loopback.header.index)
                    .up()
                    .build(),
            )
            .execute()
            .await
            .map_err(|error| sandbox_error(format!("bring loopback up: {error}")))
    }
    .await;
    connection.abort();
    result
}

fn prepare_overlay(lower: &Path, directory: &Path, workspace: &Path) -> Result<(), VmGuestError> {
    let root = directory.join("root");
    let upper = directory.join("upper");
    let work = directory.join("work");
    let options = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.display(),
        upper.display(),
        work.display()
    );
    mount(
        Some(OsStr::new("overlay")),
        root.as_path(),
        Some(OsStr::new("overlay")),
        MsFlags::MS_NODEV | MsFlags::MS_NOSUID,
        Some(options.as_str()),
    )
    .map_err(nix_error)?;
    for path in [
        Path::new("/.oldroot"),
        Path::new("/dev"),
        Path::new("/logs"),
        Path::new("/proc"),
        Path::new("/run"),
        Path::new("/tests"),
        Path::new("/tmp"),
        workspace,
    ] {
        ensure_directory_beneath(&root, path)?;
    }
    let dev = ensure_directory_beneath(&root, Path::new("/dev"))?;
    mount_tmpfs(&dev, "mode=755")
        .map_err(|error| sandbox_error(format!("mount private /dev: {error}")))?;
    let dev_pts = ensure_directory_beneath(&root, Path::new("/dev/pts"))?;
    mount(
        Some(OsStr::new("devpts")),
        dev_pts.as_path(),
        Some(OsStr::new("devpts")),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some(OsStr::new("newinstance,ptmxmode=0666,mode=0620")),
    )
    .map_err(|error| sandbox_error(format!("mount private devpts: {error}")))?;
    #[cfg(unix)]
    std::os::unix::fs::symlink("pts/ptmx", dev.join("ptmx"))?;
    for device in ["null", "zero", "random", "urandom"] {
        let destination = dev.join(device);
        fs::File::create(&destination)?;
        mount(
            Some(Path::new("/dev").join(device).as_path()),
            destination.as_path(),
            None::<&OsStr>,
            MsFlags::MS_BIND,
            None::<&OsStr>,
        )
        .map_err(|error| sandbox_error(format!("bind safe device {device}: {error}")))?;
    }
    for (path, options) in [
        ("run", "mode=755"),
        ("tmp", "mode=1777"),
        ("tests", "mode=755"),
        ("logs", "mode=755"),
    ] {
        let destination = ensure_directory_beneath(&root, Path::new("/").join(path).as_path())?;
        mount_tmpfs(&destination, options)
            .map_err(|error| sandbox_error(format!("mount private /{path}: {error}")))?;
    }
    Ok(())
}

fn rooted(root: &Path, absolute: &Path) -> Result<PathBuf, VmGuestError> {
    if !is_normal_absolute(absolute) {
        return Err(sandbox_error("attempt path must be a normal absolute path"));
    }
    let mut rooted = root.to_path_buf();
    for component in absolute.components().skip(1) {
        let std::path::Component::Normal(component) = component else {
            return Err(sandbox_error(
                "attempt path contains a non-normal component",
            ));
        };
        rooted.push(component);
    }
    Ok(rooted)
}

fn is_normal_absolute(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(std::path::Component::RootDir))
        && components.all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn ensure_directory_beneath(root: &Path, absolute: &Path) -> Result<PathBuf, VmGuestError> {
    let rooted = rooted(root, absolute)?;
    let root_metadata = fs::symlink_metadata(root)?;
    if !root_metadata.file_type().is_dir() || root_metadata.file_type().is_symlink() {
        return Err(sandbox_error("attempt root is not a real directory"));
    }
    let mut current = root.to_path_buf();
    for component in absolute.components().skip(1) {
        let std::path::Component::Normal(component) = component else {
            return Err(sandbox_error(
                "attempt path contains a non-normal component",
            ));
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(sandbox_error(format!(
                    "attempt directory {} is a symlink or non-directory",
                    current.display()
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                let metadata = fs::symlink_metadata(&current)?;
                if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                    return Err(sandbox_error(format!(
                        "attempt directory {} was replaced during setup",
                        current.display()
                    )));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    debug_assert_eq!(current, rooted);
    Ok(rooted)
}

fn mount_tmpfs(path: &Path, options: &str) -> Result<(), VmGuestError> {
    mount(
        Some(OsStr::new("tmpfs")),
        path,
        Some(OsStr::new("tmpfs")),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some(options),
    )
    .map_err(nix_error)
}

fn mount_proc(root: &Path) -> Result<(), VmGuestError> {
    let proc = ensure_directory_beneath(root, Path::new("/proc"))?;
    mount(
        Some(OsStr::new("proc")),
        proc.as_path(),
        Some(OsStr::new("proc")),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        Some(OsStr::new("hidepid=2,subset=pid")),
    )
    .map_err(nix_error)
}

fn pivot_into(root: &Path) -> Result<(), VmGuestError> {
    chdir(root).map_err(nix_error)?;
    pivot_root(Path::new("."), Path::new(".oldroot")).map_err(nix_error)?;
    chdir(Path::new("/")).map_err(nix_error)?;
    umount2(Path::new("/.oldroot"), MntFlags::MNT_DETACH).map_err(nix_error)?;
    fs::remove_dir("/.oldroot")?;
    sethostname("nanocodex-attempt").map_err(nix_error)?;
    Ok(())
}

fn control_error(id: u64, operation: &str) -> ControlResponse {
    ControlResponse {
        id,
        error: Some(format!(
            "{operation}: task supervisor requests must carry an attempt generation"
        )),
    }
}

fn sandbox_error(message: impl AsRef<str>) -> VmGuestError {
    VmGuestError::Sandbox(message.as_ref().to_owned())
}

fn nix_error(error: nix::errno::Errno) -> VmGuestError {
    VmGuestError::Sandbox(error.to_string())
}

fn lock_unpoisoned<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_workspace_is_rooted_beneath_overlay() {
        assert_eq!(
            rooted(Path::new("/attempt/root"), Path::new("/work/tree")).unwrap(),
            PathBuf::from("/attempt/root/work/tree")
        );
        assert!(rooted(Path::new("/attempt/root"), Path::new("relative")).is_err());
        assert!(rooted(Path::new("/attempt/root"), Path::new("/work/../escape")).is_err());
        assert!(rooted(Path::new("/attempt/root"), Path::new("/work/./escape")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn directory_setup_rejects_symlinks_from_the_lower_filesystem() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("root");
        let outside = fixture.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("workspace")).unwrap();

        let error = ensure_directory_beneath(&root, Path::new("/workspace/task")).unwrap_err();

        assert!(error.to_string().contains("symlink or non-directory"));
        assert!(!outside.join("task").exists());
    }

    #[test]
    fn directory_setup_creates_only_normal_components_beneath_root() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("root");
        fs::create_dir(&root).unwrap();

        let created = ensure_directory_beneath(&root, Path::new("/workspace/task")).unwrap();

        assert_eq!(created, root.join("workspace/task"));
        assert!(created.is_dir());
        assert!(ensure_directory_beneath(&root, Path::new("/workspace/../../escape")).is_err());
    }

    #[test]
    fn generation_directories_sort_in_numeric_order() {
        let first = format!("{:020}", 9_u64);
        let second = format!("{:020}", 10_u64);
        assert!(first < second);
    }
}
