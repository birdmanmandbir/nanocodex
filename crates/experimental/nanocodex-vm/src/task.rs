use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use arcbox_ext4::{
    Formatter, Reader,
    constants::{file_mode, make_mode},
};
use nanocodex_tools::ToolsBuilder;
use tokio::process::Command;

use crate::{
    BlockDevice, EgressLease, GuestCommand, Network, VmCommand, VmCommandOutput, VmConfig,
    VmToolSession, VmToolSessionError, VmToolSessionHandle, VmTools,
};

const DEFAULT_CPUS: u8 = 2;
const DEFAULT_MEMORY_MIB: u32 = 1_024;
const DEFAULT_WORKSPACE: &str = "/app";
const DEFAULT_SHELL: &str = "sh";
const BLOCK_SIZE: u32 = 4_096;
const MIN_SUPERVISOR_DISK_BYTES: u64 = 128 * 1024 * 1024;
const SUPERVISOR_EXECUTABLE: &str = "/nanocodex-vm-guest";
const SUPERVISOR_SENTINEL: &str = "/var/lib/nanocodex/supervisor.sentinel";
const SUPERVISOR_SENTINEL_CONTENTS: &[u8] = b"nanocodex-owned-supervisor-v1\n";
const TASK_BLOCK_ID: &str = "nanocodex-task-lower";
const TASK_DEVICE: &str = "/dev/vdb";
const TASK_LOWER_MOUNT: &str = "/mnt/nanocodex-task";
const ATTEMPTS_ROOT: &str = "/var/lib/nanocodex/attempts";

/// High-level builder for one retained task VM with sequential attempt sandboxes.
pub struct TaskVmBuilder {
    supervisor_rootfs: PathBuf,
    immutable_task_rootfs: PathBuf,
    vmm_executable: PathBuf,
    vmm_arguments: Vec<OsString>,
    guest_runtime_disk: Option<PathBuf>,
    firmware_directory: Option<PathBuf>,
    workspace: String,
    shell: String,
    cpus: u8,
    memory_mib: u32,
    environment: Vec<(OsString, OsString)>,
}

/// One retained task VM that admits exactly one isolated attempt at a time.
///
/// Task VMs in this initial slice are deliberately offline. Each attempt uses
/// the same immutable task lower filesystem but receives a fresh overlay,
/// process tree, and Linux namespace set inside the guest.
pub struct TaskVm {
    session: VmToolSession,
    state: Arc<Mutex<TaskVmState>>,
    supervisor_rootfs: PathBuf,
    workspace: String,
    shell: String,
    environment: Vec<(String, String)>,
    boot_duration: Duration,
}

/// One generation-scoped attempt inside a retained [`TaskVm`].
///
/// Cloned tools remain generation-scoped. After [`Self::finish`] succeeds,
/// stale clones are rejected by the guest and can never address a later
/// attempt.
pub struct TaskVmAttempt {
    control: VmToolSessionHandle,
    scoped: VmToolSessionHandle,
    state: Arc<Mutex<TaskVmState>>,
    generation: u64,
    workspace: String,
    shell: String,
    finished: AtomicBool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskVmState {
    Idle { next_generation: u64 },
    Starting { generation: u64 },
    Active { generation: u64 },
    Finishing { generation: u64 },
    Poisoned,
}

struct TaskVmTransitionGuard {
    state: Arc<Mutex<TaskVmState>>,
    expected: TaskVmState,
    armed: bool,
}

/// Whether an attempt's overlay remains on the private supervisor disk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptRetention {
    /// Delete the overlay only after all attempt processes have drained.
    Discard,
    /// Keep the overlay under the generation-keyed supervisor directory.
    Retain,
}

/// Failure to prepare, use, or stop a retained task VM.
#[derive(Debug, thiserror::Error)]
pub enum TaskVmError {
    /// A private disk, runtime, VMM, or firmware path was unavailable.
    #[error("invalid task VM path {path}: {reason}")]
    InvalidPath {
        /// Rejected path.
        path: PathBuf,
        /// Stable reason for rejection.
        reason: &'static str,
    },

    /// Preparing the private supervisor filesystem failed.
    #[error("failed to prepare task VM files: {0}")]
    Io(#[from] std::io::Error),

    /// Preparing the trusted supervisor filesystem failed.
    #[error("failed to prepare trusted task VM supervisor: {0}")]
    SupervisorImage(String),

    /// The underlying retained guest session failed.
    #[error(transparent)]
    Session(#[from] VmToolSessionError),

    /// A task VM operation raced or violated the one-attempt lifecycle.
    #[error("task VM lifecycle rejected the operation: {0}")]
    Lifecycle(&'static str),

    /// A configured task environment entry cannot be represented in the guest protocol.
    #[error("invalid task VM environment: {0}")]
    InvalidEnvironment(&'static str),

    /// Attempt setup or teardown failed, so reuse is unsafe.
    #[error("task VM is poisoned and must be shut down and recycled")]
    Poisoned,
}

impl TaskVm {
    /// Starts a builder for a fresh trusted supervisor root and immutable task root.
    ///
    /// `private_supervisor_rootfs` is a destination and must not exist. Launch
    /// formats it from the configured Nanocodex guest runtime; task image
    /// contents are never copied into or executed by the supervisor.
    #[must_use]
    pub fn builder(
        private_supervisor_rootfs: impl Into<PathBuf>,
        immutable_task_rootfs: impl Into<PathBuf>,
        vmm_executable: impl Into<PathBuf>,
    ) -> TaskVmBuilder {
        TaskVmBuilder::new(
            private_supervisor_rootfs,
            immutable_task_rootfs,
            vmm_executable,
        )
    }

    /// Validates a private supervisor destination and starts its builder.
    ///
    /// # Errors
    ///
    /// Returns an error when the immutable image or destination is invalid.
    pub fn private_from(
        immutable_task_rootfs: impl AsRef<Path>,
        private_supervisor_rootfs: impl Into<PathBuf>,
        vmm_executable: impl Into<PathBuf>,
    ) -> Result<TaskVmBuilder, TaskVmError> {
        TaskVmBuilder::private_from(
            immutable_task_rootfs,
            private_supervisor_rootfs,
            vmm_executable,
        )
    }

    /// Returns the private supervisor disk used for retained attempt overlays.
    #[must_use]
    pub fn supervisor_rootfs(&self) -> &Path {
        &self.supervisor_rootfs
    }

    /// Returns how long launch through the typed guest-ready handshake took.
    #[must_use]
    pub const fn boot_duration(&self) -> Duration {
        self.boot_duration
    }

    /// Begins one fresh attempt generation.
    ///
    /// # Errors
    ///
    /// Returns an error when another attempt is active, the VM is poisoned, or
    /// the guest cannot construct the sandbox. A guest setup failure poisons
    /// the VM because partial namespace state cannot be reused safely.
    pub async fn begin_attempt(&self) -> Result<TaskVmAttempt, TaskVmError> {
        let generation = {
            let mut state = lock_unpoisoned(&self.state);
            match *state {
                TaskVmState::Idle { next_generation } => {
                    *state = TaskVmState::Starting {
                        generation: next_generation,
                    };
                    next_generation
                }
                TaskVmState::Poisoned => return Err(TaskVmError::Poisoned),
                TaskVmState::Starting { .. }
                | TaskVmState::Active { .. }
                | TaskVmState::Finishing { .. } => {
                    return Err(TaskVmError::Lifecycle(
                        "another task attempt is already active",
                    ));
                }
            }
        };
        let mut transition = TaskVmTransitionGuard::new(
            Arc::clone(&self.state),
            TaskVmState::Starting { generation },
        );
        let control = self.session.handle();
        control
            .begin_attempt(generation, self.workspace.clone(), self.environment.clone())
            .await?;
        transition.complete(TaskVmState::Active { generation })?;
        Ok(TaskVmAttempt {
            scoped: control.scoped(generation),
            control,
            state: Arc::clone(&self.state),
            generation,
            workspace: self.workspace.clone(),
            shell: self.shell.clone(),
            finished: AtomicBool::new(false),
        })
    }

    /// Stops the retained VM, forcibly recycling an incomplete attempt when needed.
    ///
    /// A poisoned VM may still be shut down; the guest supervisor performs a
    /// final fail-closed attempt kill before acknowledging shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error when the VMM cannot stop or be killed cleanly.
    pub async fn shutdown(&self) -> Result<(), TaskVmError> {
        let force = {
            let mut state = lock_unpoisoned(&self.state);
            match *state {
                TaskVmState::Idle { .. } => false,
                TaskVmState::Poisoned => true,
                TaskVmState::Starting { .. }
                | TaskVmState::Active { .. }
                | TaskVmState::Finishing { .. } => {
                    *state = TaskVmState::Poisoned;
                    true
                }
            }
        };
        if force || self.session.shutdown().await.is_err() {
            self.session.force_shutdown().await?;
        }
        Ok(())
    }
}

impl TaskVmBuilder {
    /// Configures a fresh trusted supervisor-root destination.
    #[must_use]
    pub fn new(
        private_supervisor_rootfs: impl Into<PathBuf>,
        immutable_task_rootfs: impl Into<PathBuf>,
        vmm_executable: impl Into<PathBuf>,
    ) -> Self {
        Self {
            supervisor_rootfs: private_supervisor_rootfs.into(),
            immutable_task_rootfs: immutable_task_rootfs.into(),
            vmm_executable: vmm_executable.into(),
            vmm_arguments: Vec::new(),
            guest_runtime_disk: None,
            firmware_directory: None,
            workspace: DEFAULT_WORKSPACE.to_owned(),
            shell: DEFAULT_SHELL.to_owned(),
            cpus: DEFAULT_CPUS,
            memory_mib: DEFAULT_MEMORY_MIB,
            environment: Vec::new(),
        }
    }

    /// Validates one private supervisor destination for the immutable task image.
    ///
    /// The destination is deliberately not cloned from `immutable_task_rootfs`.
    /// [`Self::launch`] formats a clean filesystem containing only the selected
    /// Nanocodex guest runtime and trusted supervisor state.
    ///
    /// # Errors
    ///
    /// Returns an error when the source is not a file, the destination already
    /// exists, or its parent cannot be prepared.
    pub fn private_from(
        immutable_task_rootfs: impl AsRef<Path>,
        private_supervisor_rootfs: impl Into<PathBuf>,
        vmm_executable: impl Into<PathBuf>,
    ) -> Result<Self, TaskVmError> {
        let source = immutable_task_rootfs.as_ref();
        if !source.is_file() {
            return Err(TaskVmError::InvalidPath {
                path: source.to_path_buf(),
                reason: "immutable task root is not a raw ext4 image",
            });
        }
        let destination = private_supervisor_rootfs.into();
        if destination.exists() {
            return Err(TaskVmError::InvalidPath {
                path: destination,
                reason: "private supervisor destination already exists",
            });
        }
        let parent = destination
            .parent()
            .ok_or_else(|| TaskVmError::InvalidPath {
                path: destination.clone(),
                reason: "private supervisor destination has no parent",
            })?;
        fs::create_dir_all(parent)?;
        Ok(Self::new(destination, source, vmm_executable))
    }

    /// Appends one application-owned VMM argument.
    #[must_use]
    pub fn vmm_argument(mut self, argument: impl Into<OsString>) -> Self {
        self.vmm_arguments.push(argument.into());
        self
    }

    /// Selects the prepared read-only guest-runtime disk.
    #[must_use]
    pub fn guest_runtime_disk(mut self, disk: impl Into<PathBuf>) -> Self {
        self.guest_runtime_disk = Some(disk.into());
        self
    }

    /// Selects the platform libkrun firmware directory.
    #[must_use]
    pub fn firmware_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.firmware_directory = Some(directory.into());
        self
    }

    /// Sets the absolute task workspace inside each attempt.
    #[must_use]
    pub fn guest_workspace(mut self, workspace: impl Into<String>) -> Self {
        self.workspace = workspace.into();
        self
    }

    /// Sets the shell description exposed to the model.
    #[must_use]
    pub fn shell(mut self, shell: impl Into<String>) -> Self {
        self.shell = shell.into();
        self
    }

    /// Sets the task VM virtual CPU count.
    #[must_use]
    pub const fn cpus(mut self, cpus: u8) -> Self {
        self.cpus = cpus;
        self
    }

    /// Sets the task VM memory in mebibytes.
    #[must_use]
    pub const fn memory_mib(mut self, memory_mib: u32) -> Self {
        self.memory_mib = memory_mib;
        self
    }

    /// Extends the environment inherited by every attempt runtime.
    #[must_use]
    pub fn environment(
        mut self,
        environment: impl IntoIterator<Item = (impl Into<OsString>, impl Into<OsString>)>,
    ) -> Self {
        self.environment.extend(
            environment
                .into_iter()
                .map(|(name, value)| (name.into(), value.into())),
        );
        self
    }

    /// Boots the offline task VM and waits for the trusted supervisor handshake.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid inputs, VMM launch failure, or guest
    /// supervisor readiness failure.
    pub async fn launch(self) -> Result<TaskVm, TaskVmError> {
        if !is_normal_absolute(Path::new(&self.workspace)) {
            return Err(TaskVmError::InvalidPath {
                path: PathBuf::from(self.workspace),
                reason: "guest workspace is not a normal absolute path",
            });
        }
        if self.supervisor_rootfs.exists() {
            return Err(TaskVmError::InvalidPath {
                path: self.supervisor_rootfs,
                reason: "private supervisor destination already exists",
            });
        }
        for (path, reason) in [
            (
                &self.immutable_task_rootfs,
                "immutable task root is not a file",
            ),
            (&self.vmm_executable, "VMM executable is not a file"),
        ] {
            if !path.is_file() {
                return Err(TaskVmError::InvalidPath {
                    path: path.clone(),
                    reason,
                });
            }
        }
        let runtime = self
            .guest_runtime_disk
            .ok_or_else(|| TaskVmError::InvalidPath {
                path: PathBuf::new(),
                reason: "guest runtime disk was not configured",
            })?;
        if !runtime.is_file() {
            return Err(TaskVmError::InvalidPath {
                path: runtime,
                reason: "guest runtime disk is not a file",
            });
        }
        let environment = normalized_environment(&self.environment)?;
        materialize_supervisor_root(
            &runtime,
            &self.immutable_task_rootfs,
            &self.supervisor_rootfs,
        )?;
        let mut supervisor_guard = SupervisorRootGuard::new(self.supervisor_rootfs.clone());

        let config = VmConfig::ext4(&self.supervisor_rootfs)
            .cpus(self.cpus)
            .memory_mib(self.memory_mib)
            .network(Network::Disabled)
            .block_device(BlockDevice::read_only(
                TASK_BLOCK_ID,
                &self.immutable_task_rootfs,
            ));
        let guest = GuestCommand::new(SUPERVISOR_EXECUTABLE).args([
            "--task-supervisor",
            TASK_DEVICE,
            TASK_LOWER_MOUNT,
            ATTEMPTS_ROOT,
        ]);

        let mut command = Command::new(&self.vmm_executable);
        command.args(self.vmm_arguments);
        if let Some(firmware) = self.firmware_directory {
            let firmware = firmware.canonicalize()?;
            #[cfg(target_os = "linux")]
            command.env("LD_LIBRARY_PATH", firmware);
            #[cfg(target_os = "macos")]
            command.env("DYLD_LIBRARY_PATH", firmware);
        }
        let started_at = Instant::now();
        let session =
            VmToolSession::spawn_configured(command, config, guest, EgressLease::disabled())
                .await?;
        supervisor_guard.disarm();
        Ok(TaskVm {
            session,
            state: Arc::new(Mutex::new(TaskVmState::Idle { next_generation: 1 })),
            supervisor_rootfs: self.supervisor_rootfs,
            workspace: self.workspace,
            shell: self.shell,
            environment,
            boot_duration: started_at.elapsed(),
        })
    }
}

impl TaskVmAttempt {
    /// Returns this attempt's monotonic generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Waits for the attempt-local runtime to answer a scoped readiness request.
    ///
    /// # Errors
    ///
    /// Returns an error when setup failed or this generation is no longer active.
    pub async fn ready(&self) -> Result<(), TaskVmError> {
        self.scoped.ready().await?;
        Ok(())
    }

    /// Returns generation-scoped VM workspace tools.
    #[must_use]
    pub fn tools(&self) -> VmTools {
        VmTools::new(self.scoped.clone())
    }

    /// Returns a normal tool builder routed only to this attempt.
    #[must_use]
    pub fn tools_builder(&self) -> ToolsBuilder {
        self.tools()
            .tools_builder()
            .working_directory(self.workspace.clone())
            .default_shell(self.shell.clone())
    }

    /// Writes one verifier-owned file into this attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when this generation is stale or the write fails.
    pub async fn write_file(
        &self,
        path: impl Into<String>,
        contents: Vec<u8>,
        mode: u32,
    ) -> Result<(), TaskVmError> {
        self.scoped.write_file(path, contents, mode).await?;
        Ok(())
    }

    /// Writes one verifier-owned file with a stable guest modification time.
    ///
    /// # Errors
    ///
    /// Returns an error when this generation is stale or the write fails.
    pub async fn write_file_with_mtime(
        &self,
        path: impl Into<String>,
        contents: Vec<u8>,
        mode: u32,
        mtime_seconds: i64,
    ) -> Result<(), TaskVmError> {
        self.scoped
            .write_file_with_mtime(path, contents, mode, mtime_seconds)
            .await?;
        Ok(())
    }

    /// Creates or updates one verifier-owned directory in this attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when this generation is stale or the operation fails.
    pub async fn create_directory(
        &self,
        path: impl Into<String>,
        mode: u32,
        mtime_seconds: Option<i64>,
    ) -> Result<(), TaskVmError> {
        self.scoped
            .create_directory(path, mode, mtime_seconds)
            .await?;
        Ok(())
    }

    /// Reads one artifact from this attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when this generation is stale or the read fails.
    pub async fn read_file(&self, path: impl Into<String>) -> Result<Vec<u8>, TaskVmError> {
        Ok(self.scoped.read_file(path).await?)
    }

    /// Executes a trusted verifier command in the same sandbox as the agent.
    ///
    /// # Errors
    ///
    /// Returns an error when this generation is stale or execution fails.
    pub async fn command(&self, command: VmCommand) -> Result<VmCommandOutput, TaskVmError> {
        Ok(self.scoped.command(command).await?)
    }

    /// Drains and finishes this attempt, retaining or discarding its overlay.
    ///
    /// A teardown error poisons the task VM; callers must shut it down and
    /// recycle from the immutable task image before running another coordinate.
    ///
    /// # Errors
    ///
    /// Returns an error for repeated/concurrent finish or unprovable teardown.
    pub async fn finish(&self, retention: AttemptRetention) -> Result<(), TaskVmError> {
        {
            let mut state = lock_unpoisoned(&self.state);
            match *state {
                TaskVmState::Active { generation } if generation == self.generation => {
                    *state = TaskVmState::Finishing {
                        generation: self.generation,
                    };
                }
                TaskVmState::Poisoned => return Err(TaskVmError::Poisoned),
                _ => {
                    return Err(TaskVmError::Lifecycle(
                        "attempt is not the active generation",
                    ));
                }
            }
        }
        let mut transition = TaskVmTransitionGuard::new(
            Arc::clone(&self.state),
            TaskVmState::Finishing {
                generation: self.generation,
            },
        );
        let result = self
            .control
            .finish_attempt(
                self.generation,
                matches!(retention, AttemptRetention::Retain),
            )
            .await;
        match result {
            Ok(()) => {
                transition.complete(TaskVmState::Idle {
                    next_generation: self.generation.saturating_add(1),
                })?;
                self.finished.store(true, Ordering::Release);
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for TaskVmAttempt {
    fn drop(&mut self) {
        if self.finished.load(Ordering::Acquire) {
            return;
        }
        let mut state = lock_unpoisoned(&self.state);
        if matches!(
            *state,
            TaskVmState::Starting { generation }
                | TaskVmState::Active { generation }
                | TaskVmState::Finishing { generation }
                if generation == self.generation
        ) {
            *state = TaskVmState::Poisoned;
        }
    }
}

impl TaskVmTransitionGuard {
    const fn new(state: Arc<Mutex<TaskVmState>>, expected: TaskVmState) -> Self {
        Self {
            state,
            expected,
            armed: true,
        }
    }

    fn complete(&mut self, next: TaskVmState) -> Result<(), TaskVmError> {
        let mut state = lock_unpoisoned(&self.state);
        if *state != self.expected {
            *state = TaskVmState::Poisoned;
            self.armed = false;
            return Err(TaskVmError::Poisoned);
        }
        *state = next;
        self.armed = false;
        Ok(())
    }
}

impl Drop for TaskVmTransitionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = lock_unpoisoned(&self.state);
        if *state == self.expected {
            *state = TaskVmState::Poisoned;
        }
    }
}

struct SupervisorRootGuard {
    path: PathBuf,
    armed: bool,
}

impl SupervisorRootGuard {
    const fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SupervisorRootGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn materialize_supervisor_root(
    runtime_disk: &Path,
    task_rootfs: &Path,
    destination: &Path,
) -> Result<(), TaskVmError> {
    if destination.exists() {
        return Err(TaskVmError::InvalidPath {
            path: destination.to_path_buf(),
            reason: "private supervisor destination already exists",
        });
    }
    let parent = destination
        .parent()
        .ok_or_else(|| TaskVmError::InvalidPath {
            path: destination.to_path_buf(),
            reason: "private supervisor destination has no parent",
        })?;
    fs::create_dir_all(parent)?;
    let task_bytes = fs::metadata(task_rootfs)?.len();
    let disk_bytes = task_bytes.max(MIN_SUPERVISOR_DISK_BYTES);
    let mut runtime = Reader::new(runtime_disk)
        .map_err(|error| TaskVmError::SupervisorImage(error.to_string()))?;
    let runtime_bytes = runtime
        .read_file(SUPERVISOR_EXECUTABLE, 0, None)
        .map_err(|error| TaskVmError::SupervisorImage(error.to_string()))?;
    if !runtime_bytes.starts_with(b"\x7fELF") {
        return Err(TaskVmError::SupervisorImage(
            "configured guest runtime disk did not contain an ELF runtime".to_owned(),
        ));
    }

    let temporary = tempfile::Builder::new()
        .prefix(".nanocodex-supervisor.")
        .tempfile_in(parent)?
        .into_temp_path();
    let mut formatter = Formatter::new(&temporary, BLOCK_SIZE, disk_bytes)
        .map_err(|error| TaskVmError::SupervisorImage(error.to_string()))?;
    for directory in [
        "/dev",
        "/mnt",
        TASK_LOWER_MOUNT,
        "/proc",
        "/run",
        "/sys",
        "/tmp",
        "/var",
        "/var/lib",
        "/var/lib/nanocodex",
    ] {
        formatter
            .create(
                directory,
                make_mode(file_mode::S_IFDIR, 0o755),
                None,
                None,
                None,
                Some(0),
                Some(0),
                None,
            )
            .map_err(|error| TaskVmError::SupervisorImage(error.to_string()))?;
    }
    let mut runtime_contents = runtime_bytes.as_slice();
    formatter
        .create(
            SUPERVISOR_EXECUTABLE,
            make_mode(file_mode::S_IFREG, 0o755),
            None,
            None,
            Some(&mut runtime_contents),
            Some(0),
            Some(0),
            None,
        )
        .map_err(|error| TaskVmError::SupervisorImage(error.to_string()))?;
    let mut sentinel = SUPERVISOR_SENTINEL_CONTENTS;
    formatter
        .create(
            SUPERVISOR_SENTINEL,
            make_mode(file_mode::S_IFREG, 0o400),
            None,
            None,
            Some(&mut sentinel),
            Some(0),
            Some(0),
            None,
        )
        .map_err(|error| TaskVmError::SupervisorImage(error.to_string()))?;
    formatter
        .close()
        .map_err(|error| TaskVmError::SupervisorImage(error.to_string()))?;
    temporary
        .persist(destination)
        .map_err(|error| TaskVmError::Io(error.error))?;
    Ok(())
}

fn is_normal_absolute(path: &Path) -> bool {
    let bytes = path.as_os_str().as_encoded_bytes();
    bytes.first() == Some(&b'/')
        && bytes[1..]
            .split(|byte| *byte == b'/')
            .all(|component| !component.is_empty() && component != b"." && component != b"..")
}

fn normalized_environment(
    environment: &[(OsString, OsString)],
) -> Result<Vec<(String, String)>, TaskVmError> {
    environment
        .iter()
        .map(|(name, value)| {
            let name = name.to_str().ok_or(TaskVmError::InvalidEnvironment(
                "environment names must be UTF-8",
            ))?;
            let value = value.to_str().ok_or(TaskVmError::InvalidEnvironment(
                "environment values must be UTF-8",
            ))?;
            if name.is_empty() || name.contains('=') || name.contains('\0') {
                return Err(TaskVmError::InvalidEnvironment(
                    "environment names must be non-empty and contain neither `=` nor NUL",
                ));
            }
            if value.contains('\0') {
                return Err(TaskVmError::InvalidEnvironment(
                    "environment values must not contain NUL",
                ));
            }
            Ok((name.to_owned(), value.to_owned()))
        })
        .collect()
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_path_must_be_lexically_normal_and_absolute() {
        assert!(is_normal_absolute(Path::new("/work/tree")));
        assert!(!is_normal_absolute(Path::new("work/tree")));
        assert!(!is_normal_absolute(Path::new("/work/../tree")));
        assert!(!is_normal_absolute(Path::new("/work/./tree")));
    }
}
