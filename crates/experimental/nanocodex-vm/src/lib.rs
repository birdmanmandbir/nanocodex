//! VM lifecycle, images, and retained Nanocodex workspace tools.
//!
//! This crate owns the libkrun boundary, immutable VM configuration,
//! process-private launch records, gvproxy lifecycle, provider-neutral egress
//! capabilities, image construction, and the retained host/guest tool
//! protocol. It does not own payment providers, secrets, or evaluation
//! scheduling policy.
//!
//! # Configure a VM
//!
//! ```
//! use nanocodex_vm::{GuestCommand, Network, VmConfig};
//!
//! let vm = VmConfig::ext4("attempts/018f/root.ext4")
//!     .cpus(2)
//!     .memory_mib(768)
//!     .network(Network::Disabled);
//! let init = GuestCommand::new("/usr/local/bin/nanocodex-vm-guest")
//!     .arg("/workspace");
//! # let _ = (vm, init);
//! ```
//!
//! An application normally serializes that pair with [`VmProcessConfig`] and
//! starts a dedicated, entitled VMM subprocess. [`KrunVm::run`] is the
//! low-level blocking entry point for that private subprocess.

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
pub mod tools;

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
pub use tools::{
    GuestRuntimeDisk, GuestRuntimeDiskError, GuestRuntimeDiskStatus, VmCommand, VmCommandOutput,
    VmTool, VmToolClient, VmToolSession, VmToolSessionError, VmToolSessionHandle, VmTools,
};
#[cfg(feature = "guest-runtime")]
pub use tools::{VmGuestError, serve_guest};

/// The complete upstream libkrun API pinned by this workspace's lockfile.
///
/// Prefer `nanocodex-vm`'s typed API. This escape hatch permits specialized
/// libkrun functionality without waiting for a typed wrapper.
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
pub use ::krun as raw;
