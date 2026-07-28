#![cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]

use std::{fs::File, io::Read, path::PathBuf, time::Duration};

use nanocodex_vm::{AttemptRetention, GuestRuntimeDisk, TaskVm, TaskVmError, VmCommand};
use sha2::{Digest, Sha256};

#[tokio::test]
async fn sequential_attempts_share_only_the_immutable_lower() {
    let Some(vmm) = std::env::var_os("NANOCODEX_VM_VMM").map(PathBuf::from) else {
        eprintln!("live task VM test skipped; NANOCODEX_VM_VMM is unset");
        return;
    };
    let Some(rootfs) = std::env::var_os("NANOCODEX_VM_ROOTFS").map(PathBuf::from) else {
        eprintln!("live task VM test skipped; NANOCODEX_VM_ROOTFS is unset");
        return;
    };
    let Some(runtime_binary) = std::env::var_os("NANOCODEX_VM_RUNTIME").map(PathBuf::from) else {
        eprintln!("live task VM test skipped; NANOCODEX_VM_RUNTIME is unset");
        return;
    };
    let Some(firmware) = std::env::var_os("NANOCODEX_VM_FIRMWARE").map(PathBuf::from) else {
        eprintln!("live task VM test skipped; NANOCODEX_VM_FIRMWARE is unset");
        return;
    };
    let vmm_arguments = std::env::var_os("NANOCODEX_VM_VMM_ARGS")
        .map(|arguments| {
            arguments
                .to_string_lossy()
                .split_ascii_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let directory = tempfile::tempdir().unwrap();
    let runtime = GuestRuntimeDisk::prepare(&runtime_binary, directory.path()).unwrap();
    let supervisor = directory.path().join("supervisor.ext4");
    let lower_before = sha256_file(&rootfs);
    let mut builder = TaskVm::private_from(&rootfs, &supervisor, &vmm)
        .unwrap()
        .guest_runtime_disk(runtime.path())
        .firmware_directory(firmware)
        .guest_workspace("/workspace")
        .cpus(2)
        .memory_mib(768);
    for argument in vmm_arguments {
        builder = builder.vmm_argument(argument);
    }
    let vm = builder.launch().await.unwrap();

    let attempt_a = vm.begin_attempt().await.unwrap();
    attempt_a.ready().await.unwrap();
    assert_success(
        attempt_a
            .command(
                VmCommand::new("/bin/sh")
                    .arg("-c")
                    .arg(
                        "printf A > /workspace/agent-A; \
                         sleep 300 </dev/null >/dev/null 2>&1 &",
                    )
                    .timeout(Duration::from_secs(10)),
            )
            .await
            .unwrap(),
    );
    let verifier_a = attempt_a
        .command(
            VmCommand::new("/bin/sh")
                .arg("-c")
                .arg("test \"$(cat /workspace/agent-A)\" = A; printf A > /workspace/verifier-A"),
        )
        .await
        .unwrap();
    assert_success(verifier_a);
    attempt_a.finish(AttemptRetention::Discard).await.unwrap();

    let attempt_b = vm.begin_attempt().await.unwrap();
    attempt_b.ready().await.unwrap();
    let stale = attempt_a
        .command(VmCommand::new("/bin/true"))
        .await
        .unwrap_err();
    assert!(matches!(stale, TaskVmError::Session(_)));
    let verifier_b = attempt_b
        .command(VmCommand::new("/bin/sh").arg("-c").arg(
            "test ! -e /workspace/agent-A; \
                     test ! -e /workspace/verifier-A; \
                     printf B > /workspace/agent-B; \
                     test \"$(cat /workspace/agent-B)\" = B; \
                     printf B > /workspace/verifier-B",
        ))
        .await
        .unwrap();
    assert_success(verifier_b);
    attempt_b.finish(AttemptRetention::Discard).await.unwrap();

    drop(attempt_a);
    drop(attempt_b);
    vm.shutdown().await.unwrap();
    assert_eq!(sha256_file(&rootfs), lower_before);
}

fn assert_success(output: nanocodex_vm::VmCommandOutput) {
    assert_eq!(
        output.exit_code,
        0,
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn sha256_file(path: &std::path::Path) -> Vec<u8> {
    let mut file = File::open(path).unwrap();
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    digest.finalize().to_vec()
}
