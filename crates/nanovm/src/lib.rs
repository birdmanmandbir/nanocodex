#![deny(unsafe_code)]

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
pub use krun::{KrunVm, VmError};
pub use process::{PrivateVmProcessConfig, VmProcessConfig, VmProcessError};
