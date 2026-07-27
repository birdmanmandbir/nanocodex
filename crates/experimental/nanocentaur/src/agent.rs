use std::{
    collections::BTreeSet,
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    process::Output,
    str::FromStr,
    sync::Arc,
};

use async_trait::async_trait;
use nanocodex_agent::{
    AgentEvent, AgentEvents, Nanocodex, NanocodexError, OpenAiAuth, PricingSnapshot, Prompt,
    SessionId, SessionSnapshot, Thinking, TurnUsage,
};
use nanocodex_vm::VmToolSession;
use nanovm::{GuestCommand, SharedDirectory, VmConfig, VmProcessConfig};
use tempfile::TempDir;
use thiserror::Error;
use tokio::{process::Command, sync::mpsc};

use crate::{AgentCapabilities, CapabilityName, EgressContext, EgressLease, EgressProvider};

const RUNTIME_EVENT_CAPACITY: usize = 256;

// Keep this script printable ASCII without double quotes: libkrun currently
// wraps every guest argument in double quotes without escaping embedded ones.
const AGENT_VMM_SCRIPT: &str = concat!(
    "set -eu; ",
    "workspace=$1; ",
    "runtime=$2; ",
    "mkdir -p $workspace; ",
    "mount -t virtiofs nanocentaur-workspace $workspace; ",
    "exec $runtime $workspace"
);

/// Immutable inputs used to construct or resume one hosted agent harness.
#[derive(Clone)]
pub struct AgentSpec {
    /// Stable managed-service agent identifier.
    pub agent_id: String,
    /// Effective principal authorized for this runtime.
    pub principal: String,
    /// Instructions snapshotted when the managed agent was created.
    pub instructions: Option<String>,
    /// Optional model reasoning effort.
    pub thinking: Option<Thinking>,
    /// Live capability grant used to choose tools and egress.
    pub capabilities: AgentCapabilities,
    /// Last completed model boundary, when waking a durable session.
    pub snapshot: Option<SessionSnapshot>,
}

/// A native Nanocodex event kept typed through the actor and SSE boundary.
#[derive(Clone, Debug)]
pub struct RuntimeEvent(
    /// Lossless event emitted by the owned agent lifecycle.
    pub AgentEvent,
);

/// Complete model result returned to the managed actor.
pub struct AgentRunResult {
    /// Final assistant text.
    pub final_message: String,
    /// Serializable completed model boundary.
    pub snapshot: Option<SessionSnapshot>,
    /// Exact aggregate provider usage and optional USD estimate.
    pub usage: TurnUsage,
}

/// One accepted managed turn and its independently awaitable result.
pub struct ManagedTurn {
    /// Cloneable steering and cancellation capability.
    pub control: Arc<dyn ManagedTurnControl>,
    /// Completion future owned by the managed actor.
    pub result: Pin<Box<dyn Future<Output = Result<AgentRunResult, AgentError>> + Send>>,
}

/// Fresh runtime returned by a managed agent factory.
pub struct SpawnedAgent {
    /// Cheap prompt capability for the live harness.
    pub agent: Arc<dyn ManagedAgent>,
    /// Bounded stream of native runtime events.
    pub events: mpsc::Receiver<RuntimeEvent>,
}

/// Prompt boundary required by Nanocentaur's durable actor.
#[async_trait]
pub trait ManagedAgent: Send + Sync {
    /// Accepts one prompt in the runtime's ordered turn queue.
    ///
    /// # Errors
    ///
    /// Returns an error when the live harness rejects the prompt.
    async fn prompt(&self, prompt: Prompt) -> Result<ManagedTurn, AgentError>;
}

/// Cloneable controls for one accepted runtime turn.
#[async_trait]
pub trait ManagedTurnControl: Send + Sync {
    /// Adds input to the active turn at the next safe response boundary.
    ///
    /// # Errors
    ///
    /// Returns an error when the turn is not active or its steer queue is full.
    async fn steer(&self, prompt: Prompt) -> Result<(), AgentError>;

    /// Stops the turn and its owned descendants.
    ///
    /// # Errors
    ///
    /// Returns an error when the turn is already terminal.
    async fn cancel(&self) -> Result<(), AgentError>;
}

/// Factory boundary used by the durable manager and deterministic tests.
#[async_trait]
pub trait ManagedAgentFactory: Send + Sync {
    /// Creates a fresh runtime from one durable specification.
    ///
    /// # Errors
    ///
    /// Returns an error when policy, VM, tool, or model setup fails.
    async fn create(&self, spec: AgentSpec) -> Result<SpawnedAgent, AgentError>;
}

/// Host-side Nanocodex runtime whose workspace tools execute in one `NanoVM`.
///
/// VM roots are disposable copies. The durable model boundary and explicit
/// host workspace remain outside the VM.
pub struct NanocodexAgentFactory {
    auth: OpenAiAuth,
    vmm_executable: PathBuf,
    rootfs_template: PathBuf,
    state_directory: PathBuf,
    guest_runtime: String,
    guest_shell: String,
    egress: Arc<dyn EgressProvider>,
    pricing: Option<PricingSnapshot>,
}

impl NanocodexAgentFactory {
    /// Creates a VM-backed factory from explicit trusted host inputs.
    #[must_use]
    pub fn new(
        auth: OpenAiAuth,
        vmm_executable: impl Into<PathBuf>,
        rootfs_template: impl Into<PathBuf>,
        state_directory: impl Into<PathBuf>,
        egress: Arc<dyn EgressProvider>,
    ) -> Self {
        Self {
            auth,
            vmm_executable: vmm_executable.into(),
            rootfs_template: rootfs_template.into(),
            state_directory: state_directory.into(),
            guest_runtime: "/usr/local/bin/nanocodex-vm-guest".to_owned(),
            guest_shell: "sh".to_owned(),
            egress,
            pricing: None,
        }
    }

    /// Applies one immutable pricing snapshot to every hosted agent turn.
    ///
    /// Provider token usage remains authoritative. The snapshot supplies only
    /// the versioned rates and provenance used to derive
    /// [`TurnUsage::estimated_cost`].
    #[must_use]
    pub fn pricing(mut self, pricing: PricingSnapshot) -> Self {
        self.pricing = Some(pricing);
        self
    }

    /// Replaces the companion tool-server path inside the guest.
    #[must_use]
    pub fn guest_runtime(mut self, guest_runtime: impl Into<String>) -> Self {
        self.guest_runtime = guest_runtime.into();
        self
    }

    fn agent_paths(&self, agent_id: &str) -> Result<(TempDir, AgentRootfs, PathBuf), AgentError> {
        let runtimes = self.state_directory.join("runtimes");
        std::fs::create_dir_all(&runtimes)?;
        let directory = tempfile::Builder::new()
            .prefix(&format!("{agent_id}-"))
            .tempdir_in(runtimes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        }
        let rootfs = copy_rootfs(&self.rootfs_template, directory.path())?;
        let workspace = self.state_directory.join("workspaces").join(agent_id);
        std::fs::create_dir_all(&workspace)?;
        Ok((directory, rootfs, std::fs::canonicalize(workspace)?))
    }
}

#[async_trait]
impl ManagedAgentFactory for NanocodexAgentFactory {
    async fn create(&self, spec: AgentSpec) -> Result<SpawnedAgent, AgentError> {
        let egress_capabilities: BTreeSet<CapabilityName> = spec
            .capabilities
            .names()
            .iter()
            .filter(|capability| {
                !capability.as_str().starts_with("tools.")
                    && !capability.as_str().starts_with("agent.")
            })
            .cloned()
            .collect();
        let egress = self
            .egress
            .acquire(
                &EgressContext::new(spec.agent_id.clone(), spec.principal.clone()),
                &egress_capabilities,
            )
            .await?;
        let (runtime_directory, rootfs, workspace) = self.agent_paths(&spec.agent_id)?;
        let workspace = workspace
            .into_os_string()
            .into_string()
            .map_err(|_| AgentError::WorkspaceNotUtf8)?;

        let vm = rootfs
            .vm_config()
            .cpus(2)
            .memory_mib(1_024)
            .shared_directory(SharedDirectory::read_write(
                "nanocentaur-workspace",
                &workspace,
            ));
        let guest = GuestCommand::new("/bin/sh").args([
            "-c",
            AGENT_VMM_SCRIPT,
            "nanocentaur-vmm",
            &workspace,
            &self.guest_runtime,
        ]);
        let mut command = Command::new(&self.vmm_executable);
        configure_vmm_host_environment(&mut command);
        command.args(["vmm", "--config"]);
        let vm = VmToolSession::spawn_configured(command, vm, guest, egress).await?;
        let tools = vm
            .tools()
            .tools_builder()
            .web_search(spec.capabilities.contains("tools.web_search"))
            .image_generation(spec.capabilities.contains("tools.image_generation"))
            .working_directory(workspace.clone())
            .default_shell(self.guest_shell.clone())
            .build()?;

        let mut builder = Nanocodex::builder(self.auth.clone())
            .workspace(&workspace)
            .tools(tools);
        if let Ok(session_id) = SessionId::from_str(&spec.agent_id) {
            builder = builder.session_id(session_id);
        }
        if let Some(instructions) = spec.instructions {
            builder = builder.instructions(instructions);
        }
        if let Some(thinking) = spec.thinking {
            builder = builder.thinking(thinking);
        }
        if let Some(pricing) = &self.pricing {
            builder = builder.pricing(pricing.clone());
        }
        if let Some(snapshot) = spec.snapshot {
            builder = builder.resume(snapshot.rebase_workspace(&workspace)?);
        }
        let (agent, events) = builder.build()?;
        let (event_sender, event_receiver) = mpsc::channel(RUNTIME_EVENT_CAPACITY);
        tokio::spawn(forward_events(events, event_sender));

        Ok(SpawnedAgent {
            agent: Arc::new(NanocodexManagedAgent {
                agent,
                _vm: vm,
                _runtime_directory: runtime_directory,
            }),
            events: event_receiver,
        })
    }
}

fn configure_vmm_host_environment(command: &mut Command) {
    command.env_clear().env("PATH", "/usr/bin:/bin");
    for name in ["DYLD_LIBRARY_PATH", "LD_LIBRARY_PATH"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
}

enum AgentRootfs {
    Directory(PathBuf),
    Ext4(PathBuf),
}

impl AgentRootfs {
    fn vm_config(&self) -> VmConfig {
        match self {
            Self::Directory(path) => VmConfig::new(path),
            Self::Ext4(path) => VmConfig::ext4(path),
        }
    }
}

struct NanocodexManagedAgent {
    agent: Nanocodex,
    _vm: VmToolSession,
    _runtime_directory: TempDir,
}

#[async_trait]
impl ManagedAgent for NanocodexManagedAgent {
    async fn prompt(&self, prompt: Prompt) -> Result<ManagedTurn, AgentError> {
        let turn = self.agent.prompt(prompt).await?;
        let control = Arc::new(NanocodexTurnControl(turn.control()));
        Ok(ManagedTurn {
            control,
            result: Box::pin(async move {
                let result = turn.result().await.map_err(map_nanocodex_error)?;
                Ok(AgentRunResult {
                    final_message: result.final_message().to_owned(),
                    snapshot: Some(result.snapshot()),
                    usage: result.usage().clone(),
                })
            }),
        })
    }
}

struct NanocodexTurnControl(nanocodex_agent::TurnControl);

#[async_trait]
impl ManagedTurnControl for NanocodexTurnControl {
    async fn steer(&self, prompt: Prompt) -> Result<(), AgentError> {
        self.0.steer(prompt).await.map_err(map_nanocodex_error)
    }

    async fn cancel(&self) -> Result<(), AgentError> {
        self.0.cancel().await.map_err(map_nanocodex_error)
    }
}

fn map_nanocodex_error(error: NanocodexError) -> AgentError {
    match error {
        NanocodexError::TurnNotSteerable => AgentError::TurnNotSteerable,
        NanocodexError::SteerQueueFull => AgentError::SteerQueueFull,
        NanocodexError::TurnNotCancellable => AgentError::TurnNotCancellable,
        error => AgentError::Nanocodex(error),
    }
}

async fn forward_events(mut events: AgentEvents, sender: mpsc::Sender<RuntimeEvent>) {
    while let Some(event) = events.recv().await {
        if sender.send(RuntimeEvent(event)).await.is_err() {
            return;
        }
    }
}

/// Enters the blocking libkrun loop from a private typed launch record.
///
/// # Errors
///
/// Returns an error when the record cannot be read or the VM cannot run.
pub fn run_vmm(config_path: &Path) -> Result<(), AgentError> {
    VmProcessConfig::read(config_path)?.run()?;
    Ok(())
}

/// Runs one command in an isolated copy of a rootfs using an existing lease.
///
/// The VMM child receives only its private launch-record path. Host model and
/// secret-provider credentials are removed from its environment.
///
/// # Errors
///
/// Returns an error when the rootfs cannot be copied, public egress files
/// cannot be provisioned, or the VMM child cannot run.
pub async fn run_guest_command(
    vmm_executable: impl AsRef<Path>,
    rootfs_template: impl AsRef<Path>,
    lease: &EgressLease,
    command: Vec<String>,
) -> Result<Output, AgentError> {
    let (program, arguments) = command.split_first().ok_or(AgentError::EmptyGuestCommand)?;
    let runtime_directory = tempfile::Builder::new()
        .prefix("nanocentaur-command-")
        .tempdir()?;
    let rootfs = copy_rootfs(rootfs_template.as_ref(), runtime_directory.path())?;
    if let AgentRootfs::Directory(root) = &rootfs {
        copy_guest_files_into_rootfs(root, lease)?;
    } else if lease.guest_files().next().is_some() {
        return Err(AgentError::OneShotExt4FilesUnsupported);
    }
    let (vm, guest) = lease.configure(
        rootfs.vm_config().cpus(2).memory_mib(1_024),
        &GuestCommand::new(program).args(arguments),
    );
    let config = VmProcessConfig::new(vm, guest).write_private()?;
    let mut process = Command::new(vmm_executable.as_ref());
    configure_vmm_host_environment(&mut process);
    process
        .args(["one-shot-vmm", "--config"])
        .arg(config.path());
    let output = process.output().await?;
    drop(config);
    drop(runtime_directory);
    Ok(output)
}

/// Alias for [`run_vmm`] retained for the one-shot hidden child mode.
///
/// # Errors
///
/// Returns an error when the record cannot be read or the VM cannot run.
pub fn run_vmm_command(config_path: &Path) -> Result<(), AgentError> {
    run_vmm(config_path)
}

/// Managed runtime setup or execution failure.
#[derive(Debug, Error)]
pub enum AgentError {
    /// Steering targeted a queued or terminal turn.
    #[error("the targeted turn is not active for steering")]
    TurnNotSteerable,
    /// The active turn's steering queue is full.
    #[error("the active turn's steering queue is full")]
    SteerQueueFull,
    /// Cancellation targeted a terminal turn.
    #[error("the targeted turn is no longer cancellable")]
    TurnNotCancellable,
    /// Managed egress policy or provisioning failed.
    #[error("agent egress setup failed")]
    Egress(#[from] crate::EgressError),
    /// Filesystem or child-process I/O failed.
    #[error("VM configuration I/O failed")]
    Io(#[from] io::Error),
    /// A workspace cannot be represented as UTF-8.
    #[error("agent workspace path is not valid UTF-8")]
    WorkspaceNotUtf8,
    /// The configured rootfs template is not supported.
    #[error("rootfs template is not a directory or ext4 file: {0}")]
    InvalidRootfsTemplate(PathBuf),
    /// A one-shot command omitted its program.
    #[error("guest command must contain a program")]
    EmptyGuestCommand,
    /// A one-shot ext4 guest cannot receive pre-provisioned public files.
    #[error("one-shot ext4 commands do not support egress guest files")]
    OneShotExt4FilesUnsupported,
    /// The retained VM tool session failed.
    #[error("VM tool session failed")]
    VmSession(#[from] nanocodex_vm::VmToolSessionError),
    /// The hypervisor failed.
    #[error("VM setup failed")]
    Vm(#[from] nanovm::VmError),
    /// A private VM launch record failed.
    #[error("private VM process configuration failed")]
    VmProcess(#[from] nanovm::VmProcessError),
    /// The owned agent lifecycle failed.
    #[error("Nanocodex setup or execution failed")]
    Nanocodex(#[from] nanocodex_agent::NanocodexError),
    /// The configured tool registry failed to build.
    #[error("Nanocodex tool setup failed")]
    Tools(#[from] nanocodex_agent::ToolsBuildError),
    /// A caller-supplied runtime backend failed.
    #[error("managed agent failed: {0}")]
    Backend(String),
}

fn copy_rootfs(source: &Path, runtime: &Path) -> Result<AgentRootfs, AgentError> {
    if source.is_file() {
        let destination = runtime.join("rootfs.ext4");
        reflink_copy::reflink_or_copy(source, &destination)?;
        Ok(AgentRootfs::Ext4(destination))
    } else if source.is_dir() {
        let destination = runtime.join("rootfs");
        copy_directory(source, &destination)?;
        Ok(AgentRootfs::Directory(destination))
    } else {
        Err(AgentError::InvalidRootfsTemplate(source.to_owned()))
    }
}

fn copy_directory(source: &Path, destination: &Path) -> Result<(), io::Error> {
    std::fs::create_dir(destination)?;
    std::fs::set_permissions(
        destination,
        std::fs::symlink_metadata(source)?.permissions(),
    )?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source = entry.path();
        let destination = destination.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(&source)?;
        if metadata.file_type().is_symlink() {
            copy_symlink(&source, &destination)?;
        } else if metadata.is_dir() {
            copy_directory(&source, &destination)?;
        } else if metadata.is_file() {
            reflink_copy::reflink_or_copy(&source, &destination)?;
            std::fs::set_permissions(&destination, metadata.permissions())?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported rootfs entry: {}", source.display()),
            ));
        }
    }
    Ok(())
}

fn copy_guest_files_into_rootfs(rootfs: &Path, lease: &EgressLease) -> Result<(), io::Error> {
    for file in lease.guest_files() {
        let relative = file
            .guest_path()
            .strip_prefix("/")
            .map_err(|_| io::Error::other("egress guest file path must be absolute"))?;
        let destination = rootfs.join(relative);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&destination, file.contents())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(file.mode()))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(source: &Path, destination: &Path) -> Result<(), io::Error> {
    std::os::unix::fs::symlink(std::fs::read_link(source)?, destination)
}

#[cfg(not(unix))]
fn copy_symlink(_source: &Path, _destination: &Path) -> Result<(), io::Error> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "rootfs symlinks require a Unix host",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vmm_child_has_only_the_operational_host_environment_allowlist() {
        let mut command = Command::new("nanocentaur-server");
        configure_vmm_host_environment(&mut command);

        let names = command
            .as_std()
            .get_envs()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect::<BTreeSet<_>>();
        assert!(names.contains("PATH"));
        assert!(names.iter().all(|name| {
            matches!(
                name.as_str(),
                "PATH" | "DYLD_LIBRARY_PATH" | "LD_LIBRARY_PATH"
            )
        }));
        assert!(!names.iter().any(|name| {
            name.starts_with("NANOCENTAUR_SECRET_")
                || name.contains("API_KEY")
                || name.contains("TOKEN")
        }));
    }

    #[test]
    fn vmm_shell_script_fits_libkrun_argument_encoding() {
        assert!(
            AGENT_VMM_SCRIPT
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        );
        assert!(!AGENT_VMM_SCRIPT.contains('"'));
    }
}
