//! Retained VM sessions for Nanocodex workspace tools.
//!
//! This crate keeps the model-visible tool contract identical while forwarding
//! `exec_command`, `write_stdin`, `apply_patch`, and `view_image` to one
//! isolated guest. The default `host` feature owns the VMM child, cancellation,
//! bounded protocol, and egress lease. The narrow `guest` feature compiles the
//! companion server without the host VM or network-client dependency graph.

#![cfg_attr(
    feature = "host",
    doc = r#"
# Compose VM-backed tools

```no_run
use nanocodex_vm::VmToolSession;
use nanovm::{EgressLease, GuestCommand, VmConfig};
use tokio::process::Command;

# async fn build() -> Result<(), Box<dyn std::error::Error>> {
let vmm = Command::new("nanovm-vmm");
let session = VmToolSession::spawn_configured(
    vmm,
    VmConfig::ext4("attempts/018f/root.ext4")
        .cpus(2)
        .memory_mib(768),
    GuestCommand::new("/usr/local/bin/nanocodex-vm-guest")
        .arg("/workspace"),
    EgressLease::disabled(),
)
.await?;
let tools = session
    .tools()
    .tools_builder()
    .working_directory("/workspace")
    .build()?;
# let _ = tools;
# Ok(())
# }
```

Dropping the last session capability kills the VMM. Call
[`VmToolSession::shutdown`] when the application wants a graceful guest
filesystem sync and bounded exit wait.
"#
)]
#![cfg_attr(
    feature = "guest",
    doc = r#"
# Run the companion guest server

The dedicated guest process reserves stdin/stdout for the bounded typed
protocol and keeps one native workspace-tool runtime alive:

```no_run
use nanocodex_vm::serve_guest;

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
serve_guest("/workspace").await?;
# Ok(())
# }
```
"#
)]
#![deny(missing_docs, rustdoc::broken_intra_doc_links)]

#[cfg(feature = "guest")]
mod guest;
#[cfg(feature = "mpp")]
mod mpp;
mod protocol;
#[cfg(feature = "host")]
mod runtime_disk;
#[cfg(feature = "host")]
mod session;

#[cfg(feature = "guest")]
use std::path::Path;
#[cfg(feature = "host")]
use std::sync::Arc;

#[cfg(feature = "host")]
use nanocodex_tools::{
    StandardTool, Tool, ToolContext, ToolDefinition, ToolInput, ToolResult, Tools, ToolsBuilder,
    UpdatePlanTool,
};

#[cfg(feature = "guest")]
pub use guest::VmGuestError;
#[cfg(feature = "mpp")]
pub use mpp::{MppVmEgressError, mpp_egress_layer};
#[cfg(feature = "host")]
pub use runtime_disk::{GuestRuntimeDisk, GuestRuntimeDiskError, GuestRuntimeDiskStatus};
#[cfg(feature = "host")]
pub use session::{
    VmCommand, VmCommandOutput, VmToolSession, VmToolSessionError, VmToolSessionHandle,
};

/// One VM-aware execution capability shared by all proxied workspace tools.
///
/// The concrete client owns transport, guest session routing, cancellation,
/// and conversion of the guest's typed result into Nanocodex's `ToolResult`.
#[async_trait::async_trait]
#[cfg(feature = "host")]
pub trait VmToolClient: Send + Sync {
    /// Executes one standard tool through the client-owned VM capability.
    async fn execute(
        &self,
        tool: StandardTool,
        input: ToolInput,
        context: ToolContext<'_>,
    ) -> ToolResult;
}

/// Clone-cheap factory for the standard tools whose effects belong in a VM.
#[derive(Clone)]
#[cfg(feature = "host")]
pub struct VmTools {
    client: Arc<dyn VmToolClient>,
}

#[cfg(feature = "host")]
impl VmTools {
    /// Creates a VM tool family over one clone-cheap execution capability.
    #[must_use]
    pub fn new(client: impl VmToolClient + 'static) -> Self {
        Self {
            client: Arc::new(client),
        }
    }

    /// Returns the VM-backed `exec_command` tool.
    #[must_use]
    pub fn exec_command_tool(&self) -> VmTool {
        self.tool(StandardTool::ExecCommand)
    }

    /// Returns the VM-backed `write_stdin` tool.
    #[must_use]
    pub fn write_stdin_tool(&self) -> VmTool {
        self.tool(StandardTool::WriteStdin)
    }

    /// Returns the VM-backed `apply_patch` tool.
    #[must_use]
    pub fn apply_patch_tool(&self) -> VmTool {
        self.tool(StandardTool::ApplyPatch)
    }

    /// Returns the VM-backed `view_image` tool.
    #[must_use]
    pub fn view_image_tool(&self) -> VmTool {
        self.tool(StandardTool::ViewImage)
    }

    /// Starts a normal Nanocodex tool selection whose workspace effects are
    /// forwarded to this VM.
    ///
    /// Web search and image generation retain their normal host-side
    /// implementations. `update_plan` also stays host-side because it has no
    /// workspace effect. Callers can keep configuring the returned builder,
    /// including setting the guest-visible working directory and shell.
    #[must_use]
    pub fn tools_builder(&self) -> ToolsBuilder {
        Tools::builder()
            .workspace(false)
            .tool(self.exec_command_tool())
            .tool(self.write_stdin_tool())
            .tool(self.apply_patch_tool())
            .tool(self.view_image_tool())
            .tool(UpdatePlanTool::new())
    }

    fn tool(&self, standard: StandardTool) -> VmTool {
        VmTool {
            standard,
            client: Arc::clone(&self.client),
        }
    }
}

/// One standard Nanocodex tool whose execution is forwarded into a VM.
#[derive(Clone)]
#[cfg(feature = "host")]
pub struct VmTool {
    standard: StandardTool,
    client: Arc<dyn VmToolClient>,
}

#[cfg(feature = "host")]
impl VmTool {
    /// Returns which canonical standard tool this adapter implements.
    #[must_use]
    pub const fn standard(&self) -> StandardTool {
        self.standard
    }
}

#[async_trait::async_trait]
#[cfg(feature = "host")]
impl Tool for VmTool {
    fn definition(&self) -> ToolDefinition {
        self.standard.definition()
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        self.client.execute(self.standard, input, context).await
    }
}

/// Serves canonical workspace-tool requests over the guest's stdin/stdout.
///
/// A single invocation retains the native `ToolRuntime`, including interactive
/// shell sessions, until the input stream closes.
///
/// # Errors
///
/// Returns an error for malformed protocol messages or guest console I/O.
#[cfg(feature = "guest")]
pub async fn serve_guest(workspace: impl AsRef<Path>) -> Result<(), VmGuestError> {
    guest::serve(workspace.as_ref()).await
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use std::sync::Mutex;

    use nanocodex_tools::{StandardTool, Tool, ToolContext, ToolExecution, ToolInput};

    use super::{VmToolClient, VmTools};

    #[derive(Default)]
    struct RecordingClient {
        calls: Mutex<Vec<StandardTool>>,
    }

    #[async_trait::async_trait]
    impl VmToolClient for RecordingClient {
        async fn execute(
            &self,
            tool: StandardTool,
            _input: ToolInput,
            _context: ToolContext<'_>,
        ) -> nanocodex_tools::ToolResult {
            self.calls.lock().unwrap().push(tool);
            Ok(ToolExecution::text(tool.name()))
        }
    }

    #[test]
    fn composes_vm_workspace_tools_with_the_host_plan_tool() {
        let vm = VmTools::new(RecordingClient::default());
        let tools = vm
            .tools_builder()
            .working_directory("/workspace")
            .default_shell("sh")
            .build()
            .unwrap();

        assert!(!tools.workspace_enabled());
        assert!(tools.web_search_enabled());
        assert!(tools.image_generation_enabled());
    }

    #[test]
    fn definitions_are_the_upstream_standard_contracts() {
        let vm = VmTools::new(RecordingClient::default());
        for (tool, standard) in [
            (vm.exec_command_tool(), StandardTool::ExecCommand),
            (vm.write_stdin_tool(), StandardTool::WriteStdin),
            (vm.apply_patch_tool(), StandardTool::ApplyPatch),
            (vm.view_image_tool(), StandardTool::ViewImage),
        ] {
            assert_eq!(tool.definition().name(), standard.name());
            assert_eq!(
                serde_json::to_value(tool.definition()).unwrap(),
                serde_json::to_value(standard.definition()).unwrap()
            );
        }
    }
}
