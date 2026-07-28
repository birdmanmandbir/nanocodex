use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use clap::{ArgAction, Args, builder::NonEmptyStringValueParser};
use eyre::{Result, WrapErr, eyre};
use fs2::FileExt as _;
use nanocodex::tools::ToolsBuilder;
use nanocodex_vm::{
    BlockDevice, GuestCommand, Network, VmConfig, VmToolSession, VmToolSessionError,
};
use tokio::{process::Command, time::sleep};
use tracing::info;

const DEFAULT_CPUS: u8 = 2;
const DEFAULT_MEMORY_MIB: u32 = 1_024;
const DEFAULT_EXT4_WORKSPACE: &str = "/app";
const DEFAULT_DIRECTORY_WORKSPACE: &str = "/workspace";
const DEFAULT_SHELL: &str = "bash";
const EMBEDDED_GUEST_RUNTIME: &str = "/usr/local/bin/nanocodex-vm-guest";
const GUEST_RUNTIME_BLOCK_ID: &str = "nanocodex-runtime";
const GUEST_RUNTIME_BLOCK_DEVICE: &str = "/dev/vdb";
const GUEST_RUNTIME_MOUNT: &str = "/run/nanocodex";
const BLOCK_GUEST_RUNTIME: &str = "/run/nanocodex/nanocodex-vm-guest";
const DEFAULT_KRUNFW_DIRECTORY: &str = ".cache/libkrunfw/libkrunfw";
const CAPABILITY_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const CAPABILITY_DRAIN_INTERVAL: Duration = Duration::from_millis(10);

/// Opt-in VM workspace-tool configuration for normal agent sessions.
#[derive(Args, Default)]
pub(crate) struct VmArgs {
    /// Run workspace-mutating tools in this writable VM root filesystem.
    ///
    /// The path may be a raw ext4 image or a directory rootfs. The selected
    /// root is modified in place for the lifetime of the agent session.
    #[arg(long = "vm", visible_alias = "vm-rootfs", value_name = "ROOTFS")]
    rootfs: Option<PathBuf>,

    /// Absolute working directory inside the VM.
    #[arg(
        long,
        value_name = "PATH",
        requires = "rootfs",
        value_parser = NonEmptyStringValueParser::new()
    )]
    vm_workspace: Option<String>,

    /// Number of virtual CPUs assigned to the VM.
    #[arg(
        long,
        value_name = "COUNT",
        requires = "rootfs",
        value_parser = clap::value_parser!(u8).range(1..)
    )]
    vm_cpus: Option<u8>,

    /// Guest memory in mebibytes.
    #[arg(
        long,
        value_name = "MIB",
        requires = "rootfs",
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    vm_memory_mib: Option<u32>,

    /// Shell name described to the model for the VM environment.
    #[arg(
        long,
        value_name = "SHELL",
        requires = "rootfs",
        value_parser = NonEmptyStringValueParser::new()
    )]
    vm_shell: Option<String>,

    /// Disable guest internet socket proxying.
    #[arg(long, requires = "rootfs", action = ArgAction::SetTrue)]
    vm_no_network: bool,
}

pub(crate) struct ConfiguredVm {
    session: VmToolSession,
    workspace: String,
    shell: String,
    _root_lock: Option<File>,
}

impl VmArgs {
    #[cfg(test)]
    pub(crate) const fn is_enabled(&self) -> bool {
        self.rootfs.is_some()
    }

    pub(crate) async fn start(self) -> Result<Option<ConfiguredVm>> {
        let Some(rootfs) = self.rootfs else {
            return Ok(None);
        };
        let rootfs = rootfs
            .canonicalize()
            .wrap_err_with(|| format!("failed to resolve VM rootfs {}", rootfs.display()))?;
        let ext4 = rootfs.is_file();
        if !ext4 && !rootfs.is_dir() {
            return Err(eyre!(
                "VM rootfs is neither a raw ext4 image nor a directory: {}",
                rootfs.display()
            ));
        }
        let workspace = self.vm_workspace.unwrap_or_else(|| {
            if ext4 {
                DEFAULT_EXT4_WORKSPACE.to_owned()
            } else {
                DEFAULT_DIRECTORY_WORKSPACE.to_owned()
            }
        });
        if !Path::new(&workspace).is_absolute() {
            return Err(eyre!(
                "--vm-workspace must be an absolute guest path, got {workspace:?}"
            ));
        }
        let root_lock = ext4.then(|| lock_writable_rootfs(&rootfs)).transpose()?;
        let network = if self.vm_no_network {
            Network::Disabled
        } else {
            Network::Internet
        };
        let mut vm = if ext4 {
            VmConfig::ext4(&rootfs)
        } else {
            VmConfig::new(&rootfs)
        }
        .cpus(self.vm_cpus.unwrap_or(DEFAULT_CPUS))
        .memory_mib(self.vm_memory_mib.unwrap_or(DEFAULT_MEMORY_MIB))
        .network(network);

        let guest = if ext4 {
            let runtime = crate::eval::prepare_vm_guest_runtime().await?;
            vm = vm.block_device(BlockDevice::read_only(GUEST_RUNTIME_BLOCK_ID, runtime));
            GuestCommand::new("/bin/sh")
                .arg("-c")
                .arg(ext4_bootstrap_script(&workspace))
        } else {
            let runtime = rootfs.join(EMBEDDED_GUEST_RUNTIME.trim_start_matches('/'));
            if !runtime.is_file() {
                return Err(eyre!(
                    "directory VM rootfs is missing {}",
                    runtime.display()
                ));
            }
            GuestCommand::new("/bin/sh")
                .arg("-c")
                .arg(directory_bootstrap_script(&workspace))
        };

        let executable =
            std::env::current_exe().wrap_err("failed to resolve the VMM executable")?;
        let mut command = Command::new(executable);
        let firmware = Path::new(DEFAULT_KRUNFW_DIRECTORY);
        if firmware.join("libkrunfw.5.dylib").is_file() {
            command.env(
                "DYLD_LIBRARY_PATH",
                firmware
                    .canonicalize()
                    .wrap_err("failed to resolve the libkrun firmware directory")?,
            );
        }
        command.args(["eval", "vm", "run-config", "--config"]);
        let session =
            VmToolSession::spawn_vm(command, vm, guest).wrap_err("failed to start tool VM")?;
        session
            .ready()
            .await
            .wrap_err("tool VM guest runtime did not become ready")?;
        info!(
            target: "nanocodex_vm",
            vm_rootfs = %rootfs.display(),
            vm_workspace = workspace,
            vm_cpu_count = self.vm_cpus.unwrap_or(DEFAULT_CPUS),
            vm_memory_mib = self.vm_memory_mib.unwrap_or(DEFAULT_MEMORY_MIB),
            vm_network = if self.vm_no_network { "disabled" } else { "internet" },
            "normal agent workspace tools are running in a VM"
        );
        Ok(Some(ConfiguredVm {
            session,
            workspace,
            shell: self.vm_shell.unwrap_or_else(|| DEFAULT_SHELL.to_owned()),
            _root_lock: root_lock,
        }))
    }
}

impl ConfiguredVm {
    pub(crate) fn tools_builder(&self) -> ToolsBuilder {
        self.session
            .tools()
            .tools_builder()
            .working_directory(self.workspace.clone())
            .default_shell(self.shell.clone())
    }

    pub(crate) async fn shutdown(self) -> Result<()> {
        let started_at = Instant::now();
        loop {
            match self.session.shutdown().await {
                Ok(()) => return Ok(()),
                Err(VmToolSessionError::ActiveCapabilities(_))
                    if started_at.elapsed() < CAPABILITY_DRAIN_TIMEOUT =>
                {
                    sleep(CAPABILITY_DRAIN_INTERVAL).await;
                }
                Err(error) => return Err(error).wrap_err("failed to shut down the tool VM"),
            }
        }
    }
}

fn lock_writable_rootfs(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .wrap_err_with(|| format!("failed to open writable VM rootfs {}", path.display()))?;
    file.try_lock_exclusive()
        .wrap_err_with(|| format!("VM rootfs is already in use: {}", path.display()))?;
    Ok(file)
}

fn ext4_bootstrap_script(workspace: &str) -> String {
    let workspace = shell_word_without_double_quotes(workspace);
    format!(
        "set -eu; mkdir -p -- {workspace} {GUEST_RUNTIME_MOUNT}; \
         mount -t ext4 -o ro {GUEST_RUNTIME_BLOCK_DEVICE} {GUEST_RUNTIME_MOUNT}; \
         exec {BLOCK_GUEST_RUNTIME} {workspace}"
    )
}

fn directory_bootstrap_script(workspace: &str) -> String {
    let workspace = shell_word_without_double_quotes(workspace);
    format!(
        "set -eu; mkdir -p -- {workspace}; \
         exec {EMBEDDED_GUEST_RUNTIME} {workspace}"
    )
}

fn shell_word_without_double_quotes(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len().saturating_add(2));
    quoted.push('\'');
    for character in value.chars() {
        match character {
            '\'' => quoted.push_str("'\\''"),
            '"' => quoted.push_str("'$(printf '\\042')'"),
            character => quoted.push(character),
        }
    }
    quoted.push('\'');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_preserves_guest_workspace_as_one_shell_word() {
        let script = ext4_bootstrap_script("/work/a'b\"c");

        assert!(script.contains("mkdir -p -- '/work/a'\\''b'$(printf '\\042')'c'"));
        assert!(script.contains("exec /run/nanocodex/nanocodex-vm-guest"));
    }
}
