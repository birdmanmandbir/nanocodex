//! Small typed primitives for running isolated libkrun microVMs.
//!
//! `nanovm` owns the audited FFI boundary, immutable VM configuration,
//! process-private launch records, gvproxy lifecycle, and provider-neutral
//! egress capabilities. It does not own image building, agent tools, browser
//! policy, payments, or secrets.
//!
//! # Configure a VM
//!
//! ```
//! use nanovm::{GuestCommand, Network, VmConfig};
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

mod capabilities;
mod command;
mod config;
mod egress;
mod gvproxy;
mod krun;
mod process;

pub use capabilities::{Capabilities, KrunFeature};
pub use command::GuestCommand;
pub use config::{BlockDevice, Network, RootFilesystem, SharedDirectory, VmConfig};
pub use egress::{
    EgressError, EgressFile, EgressLease, EgressMount, GUEST_EGRESS_ROOT, MAX_EGRESS_FILE_BYTES,
};
pub use gvproxy::{Gvproxy, GvproxyError};
pub use krun::{KrunVm, KrunVmControl, VmError};
pub use process::{PrivateVmProcessConfig, VmProcessConfig, VmProcessError};

/// The complete upstream libkrun API pinned by this workspace's lockfile.
///
/// Prefer `nanovm`'s typed API. This escape hatch permits specialized
/// libkrun functionality without waiting for a typed wrapper.
pub use ::krun as raw;
