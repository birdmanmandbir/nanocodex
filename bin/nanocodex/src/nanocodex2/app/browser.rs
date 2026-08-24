// Modified from clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa for Nanocodex2.
// SPDX-License-Identifier: Apache-2.0

//! Platform browser launcher used by the terminal UI.

use std::{
    io,
    process::{Command, Stdio},
};

pub(crate) fn open(url: &str) -> io::Result<()> {
    let mut command = command(url)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

fn command(url: &str) -> io::Result<Command> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "automatic browser launch is unsupported on this platform",
    ));

    command.arg(url);
    Ok(command)
}
