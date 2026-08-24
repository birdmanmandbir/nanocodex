// Modified from clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa for Nanocodex2.
// SPDX-License-Identifier: Apache-2.0

//! User-configured lifecycle hooks.

#[cfg(unix)]
use std::env;
use std::{
    io,
    path::Path,
    process::{ExitStatus, Stdio},
};
use tokio::process::Command;

pub(crate) async fn execute(command: &str, workspace: &Path) -> io::Result<ExitStatus> {
    let mut process = shell_command(command);
    process
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
}

#[cfg(unix)]
fn shell_command(command: &str) -> Command {
    let shell = env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let mut process = Command::new(shell);
    process.args(["-lc", command]);
    process
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("cmd");
    process.args(["/C", command]);
    process
}
