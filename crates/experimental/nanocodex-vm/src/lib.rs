//! VM lifecycle, images, and retained Nanocodex workspace tools.
//!
//! This crate owns the libkrun boundary, immutable VM configuration,
//! process-private launch records, gvproxy lifecycle, provider-neutral egress
//! capabilities, image construction, and the retained host/guest tool
//! protocol. It does not own payment providers, secrets, or evaluation
//! scheduling policy.
//!
//! # Prepare a retained workspace
//!
//! ```no_run
//! use nanocodex_vm::VmWorkspaceBuilder;
//!
//! # async fn prepare() -> Result<(), Box<dyn std::error::Error>> {
//! let workspace = VmWorkspaceBuilder::private_from(
//!     ".cache/nanocodex/images/task.ext4",
//!     ".nanocodex/sessions/018f/root.ext4",
//!     "nanocodex-vmm",
//! )?
//! .guest_runtime_disk(".cache/nanocodex/runtime.ext4")
//! .firmware_directory(".cache/libkrunfw/libkrunfw")
//! .launch()
//! .await?;
//! let tools = workspace.tools_builder().build()?;
//! # drop(tools);
//! workspace.shutdown().await?;
//! # Ok(())
//! # }
//! ```
//!
//! The application-owned `nanocodex-vmm` process reads the private launch
//! record appended by the library and calls [`VmProcessConfig::run`]. macOS
//! packaging signs that process with the hypervisor entitlement; Linux uses
//! the same host API without signing. [`VmConfig`], [`GuestCommand`], and
//! [`KrunVm::run`] remain low-level escape hatches.

#![deny(unsafe_code, missing_docs, rustdoc::broken_intra_doc_links)]

#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
mod capabilities;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
mod command;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
mod config;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
mod egress;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
mod gvproxy;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
pub mod image;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
mod krun;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
mod process;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
mod task;
pub mod tools;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
mod workspace;

#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
pub use capabilities::{Capabilities, KrunFeature};
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
pub use command::GuestCommand;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
pub use config::{BlockDevice, Network, RootFilesystem, SharedDirectory, VmConfig};
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
pub use egress::{
    EgressError, EgressFile, EgressLease, EgressMount, GUEST_EGRESS_ROOT, MAX_EGRESS_FILE_BYTES,
};
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
pub use gvproxy::{Gvproxy, GvproxyError};
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
pub use krun::{KrunVm, KrunVmControl, VmError};
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
pub use process::{PrivateVmProcessConfig, VmProcessConfig, VmProcessError};
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
pub use task::{AttemptRetention, TaskVm, TaskVmAttempt, TaskVmBuilder, TaskVmError};
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
pub use tools::{
    GuestRuntimeDisk, GuestRuntimeDiskError, GuestRuntimeDiskStatus, VmCommand, VmCommandOutput,
    VmCommandPartialOutput, VmTool, VmToolClient, VmToolSession, VmToolSessionError,
    VmToolSessionHandle, VmTools,
};
#[cfg(all(feature = "guest-runtime", target_os = "linux"))]
pub use tools::{
    VmGuestError, run_task_attempt_child, run_task_attempt_helper, serve_guest, serve_task_guest,
};
#[cfg(all(feature = "guest-runtime", not(target_os = "linux")))]
pub use tools::{VmGuestError, serve_guest};
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
pub use workspace::{VmWorkspace, VmWorkspaceBuilder, VmWorkspaceError};

/// The complete upstream libkrun API pinned by this workspace's lockfile.
///
/// Prefer `nanocodex-vm`'s typed API. This escape hatch permits specialized
/// libkrun functionality without waiting for a typed wrapper.
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
pub use ::krun as raw;
