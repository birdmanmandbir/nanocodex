use std::{
    collections::BTreeSet,
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
};

use async_trait::async_trait;
use nanocodex_agent::{
    AgentEvents, Nanocodex, NanocodexError, Thinking, TurnUsage,
    events::AgentEvent,
    input::Prompt,
    session::{SessionId, SessionSnapshot},
};
use nanocodex_oai_api::{OpenAi, auth::OpenAiAuth};
use nanocodex_vm::{VmWorkspace, host::VmProcessConfig};
use tempfile::TempDir;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::{AgentCapabilities, CapabilityName, EgressContext, EgressProvider};

const RUNTIME_EVENT_CAPACITY: usize = 256;

// Keep this script printable ASCII without double quotes: libkrun currently
// wraps every guest argument in double quotes without escaping embedded ones.
const GUEST_WORKSPACE: &str = "/workspace";

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

/// Complete model result returned to the managed actor.
pub struct AgentRunResult {
    final_message: String,
    snapshot: Option<SessionSnapshot>,
    usage: TurnUsage,
}

impl AgentRunResult {
    /// Creates one completed managed runtime result.
    #[must_use]
    pub fn new(
        final_message: impl Into<String>,
        snapshot: Option<SessionSnapshot>,
        usage: TurnUsage,
    ) -> Self {
        Self {
            final_message: final_message.into(),
            snapshot,
            usage,
        }
    }

    /// Returns the final assistant text.
    #[must_use]
    pub fn final_message(&self) -> &str {
        &self.final_message
    }

    /// Returns the serializable completed model boundary, when available.
    #[must_use]
    pub const fn snapshot(&self) -> Option<&SessionSnapshot> {
        self.snapshot.as_ref()
    }

    /// Returns exact aggregate provider usage and optional USD estimate.
    #[must_use]
    pub const fn usage(&self) -> &TurnUsage {
        &self.usage
    }

    pub(crate) fn into_parts(self) -> (String, Option<SessionSnapshot>, TurnUsage) {
        (self.final_message, self.snapshot, self.usage)
    }
}

/// One accepted managed turn and its independently awaitable result.
#[must_use = "a managed turn continues running when dropped; await result() or retain its control"]
pub struct ManagedTurn {
    control: Arc<dyn ManagedTurnControl>,
    result: Pin<Box<dyn Future<Output = Result<AgentRunResult, AgentError>> + Send>>,
}

impl ManagedTurn {
    /// Creates a turn from one exact control capability and completion future.
    pub fn new(
        control: Arc<dyn ManagedTurnControl>,
        result: impl Future<Output = Result<AgentRunResult, AgentError>> + Send + 'static,
    ) -> Self {
        Self {
            control,
            result: Box::pin(result),
        }
    }

    /// Returns a cheap cloneable capability targeting this exact turn.
    #[must_use]
    pub fn control(&self) -> Arc<dyn ManagedTurnControl> {
        Arc::clone(&self.control)
    }

    /// Waits for the final managed runtime result.
    ///
    /// # Errors
    ///
    /// Returns the backend execution or lifecycle error.
    pub async fn result(self) -> Result<AgentRunResult, AgentError> {
        self.await
    }
}

impl Future for ManagedTurn {
    type Output = Result<AgentRunResult, AgentError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        self.result.as_mut().poll(context)
    }
}

/// Fresh runtime returned by a managed agent factory.
pub struct SpawnedAgent {
    agent: Arc<dyn ManagedAgent>,
    events: mpsc::Receiver<AgentEvent>,
}

impl SpawnedAgent {
    /// Creates a fresh runtime from its prompt capability and event receiver.
    #[must_use]
    pub fn new(agent: Arc<dyn ManagedAgent>, events: mpsc::Receiver<AgentEvent>) -> Self {
        Self { agent, events }
    }

    /// Transfers the prompt capability and independent event receiver.
    #[must_use]
    pub fn into_parts(self) -> (Arc<dyn ManagedAgent>, mpsc::Receiver<AgentEvent>) {
        (self.agent, self.events)
    }
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

/// Host-side Nanocodex runtime whose workspace tools execute in one VM.
///
/// VM roots are disposable copies. The durable model boundary and explicit
/// host workspace remain outside the VM.
pub struct NanocodexAgentFactory {
    auth: OpenAiAuth,
    vmm_executable: PathBuf,
    rootfs_template: PathBuf,
    state_directory: PathBuf,
    guest_runtime_disk: Option<PathBuf>,
    firmware_directory: Option<PathBuf>,
    guest_shell: String,
    egress: Arc<dyn EgressProvider>,
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
            guest_runtime_disk: None,
            firmware_directory: None,
            guest_shell: "sh".to_owned(),
            egress,
        }
    }

    /// Selects the prepared read-only guest-runtime disk required by raw ext4 roots.
    #[must_use]
    pub fn guest_runtime_disk(mut self, guest_runtime_disk: impl Into<PathBuf>) -> Self {
        self.guest_runtime_disk = Some(guest_runtime_disk.into());
        self
    }

    /// Selects the directory containing the platform libkrun firmware library.
    #[must_use]
    pub fn firmware_directory(mut self, firmware_directory: impl Into<PathBuf>) -> Self {
        self.firmware_directory = Some(firmware_directory.into());
        self
    }

    fn agent_paths(&self, agent_id: &str) -> Result<(TempDir, AgentRootfs), AgentError> {
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
        Ok((directory, rootfs))
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
        let (runtime_directory, rootfs) = self.agent_paths(&spec.agent_id)?;
        let mut vm = VmWorkspace::builder(rootfs.path(), &self.vmm_executable)
            .vmm_argument("vmm")
            .vmm_argument("--config")
            .guest_workspace(GUEST_WORKSPACE)
            .shell(&self.guest_shell)
            .cpus(2)
            .memory_mib(1_024)
            .egress(egress);
        if let Some(runtime) = &self.guest_runtime_disk {
            vm = vm.guest_runtime_disk(runtime);
        }
        if let Some(firmware) = &self.firmware_directory {
            vm = vm.firmware_directory(firmware);
        }
        let vm = vm.launch().await?;
        let tools = vm
            .tools_builder()
            .web_search(spec.capabilities.contains("tools.web_search"))
            .image_generation(spec.capabilities.contains("tools.image_generation"))
            .build()?;

        let openai = OpenAi::new(self.auth.clone())?;
        let mut builder = Nanocodex::builder(openai)
            .workspace(GUEST_WORKSPACE)
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
        if let Some(snapshot) = spec.snapshot {
            builder = builder.resume(snapshot);
        }
        let (agent, events) = builder.build()?;
        let (event_sender, event_receiver) = mpsc::channel(RUNTIME_EVENT_CAPACITY);
        tokio::spawn(forward_events(events, event_sender));

        Ok(SpawnedAgent::new(
            Arc::new(NanocodexManagedAgent {
                agent,
                _vm: vm,
                _runtime_directory: runtime_directory,
            }),
            event_receiver,
        ))
    }
}

enum AgentRootfs {
    Directory(PathBuf),
    Ext4(PathBuf),
}

impl AgentRootfs {
    fn path(&self) -> &Path {
        match self {
            Self::Directory(path) | Self::Ext4(path) => path,
        }
    }
}

struct NanocodexManagedAgent {
    agent: Nanocodex,
    _vm: VmWorkspace,
    _runtime_directory: TempDir,
}

#[async_trait]
impl ManagedAgent for NanocodexManagedAgent {
    async fn prompt(&self, prompt: Prompt) -> Result<ManagedTurn, AgentError> {
        let turn = self.agent.prompt(prompt).await?;
        let control = Arc::new(NanocodexTurnControl(turn.control()));
        Ok(ManagedTurn::new(control, async move {
            let result = turn.result().await.map_err(map_nanocodex_error)?;
            Ok(AgentRunResult::new(
                result.final_message(),
                Some(result.snapshot()),
                result.usage().clone(),
            ))
        }))
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

async fn forward_events(mut events: AgentEvents, sender: mpsc::Sender<AgentEvent>) {
    while let Some(event) = events.recv().await {
        if sender.send(event).await.is_err() {
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
    /// The configured rootfs template is not supported.
    #[error("rootfs template is not a directory or ext4 file: {0}")]
    InvalidRootfsTemplate(PathBuf),
    /// The retained VM tool session failed.
    #[error("VM tool session failed")]
    VmWorkspace(#[from] nanocodex_vm::VmWorkspaceError),
    /// The hypervisor failed.
    #[error("VM setup failed")]
    Vm(#[from] nanocodex_vm::host::VmError),
    /// A private VM launch record failed.
    #[error("private VM process configuration failed")]
    VmProcess(#[from] nanocodex_vm::host::VmProcessError),
    /// OpenAI client setup failed.
    #[error("OpenAI client setup failed")]
    OpenAi(#[from] nanocodex_oai_api::OpenAiError),
    /// The owned agent lifecycle failed.
    #[error("Nanocodex setup or execution failed")]
    Nanocodex(#[from] nanocodex_agent::NanocodexError),
    /// The configured tool registry failed to build.
    #[error("Nanocodex tool setup failed")]
    Tools(#[from] nanocodex_agent::tools::ToolsBuildError),
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
