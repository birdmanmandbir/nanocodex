#[cfg(target_os = "linux")]
use std::{ffi::OsString, path::PathBuf};

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), nanocodex_vm::VmGuestError> {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [mode, lower_device, lower_mount, attempts_root]
            if mode == &OsString::from("--task-supervisor") =>
        {
            nanocodex_vm::serve_task_guest(
                PathBuf::from(lower_device),
                PathBuf::from(lower_mount),
                PathBuf::from(attempts_root),
            )
            .await
        }
        [mode, rest @ ..] if mode == &OsString::from("--attempt-helper") => {
            nanocodex_vm::run_task_attempt_helper(rest).await
        }
        [mode, rest @ ..] if mode == &OsString::from("--attempt-child") => {
            nanocodex_vm::run_task_attempt_child(rest).await
        }
        [] => nanocodex_vm::serve_guest("/workspace").await,
        [workspace] => nanocodex_vm::serve_guest(PathBuf::from(workspace)).await,
        _ => Err(nanocodex_vm::VmGuestError::Sandbox(
            "invalid nanocodex-vm-guest arguments".to_owned(),
        )),
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("nanocodex-vm-guest must be built for a Linux guest target");
}
