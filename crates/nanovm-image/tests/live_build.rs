use std::{fs, path::PathBuf};

use arcbox_ext4::Reader;
use nanocodex_vm::GuestRuntimeDisk;
use nanovm_image::{CachePolicy, VmImageBuilder};

const DISK_BYTES: u64 = 512 * 1024 * 1024;

#[tokio::test]
#[ignore = "requires a signed libkrun VMM, firmware, current guest ELF, and OCI cache"]
async fn run_instruction_uses_the_public_private_config_vmm_contract() {
    let vmm = required_path("NANOVM_IMAGE_VMM");
    let guest = required_path("NANOVM_IMAGE_RUNTIME");
    let firmware = required_path("NANOVM_IMAGE_FIRMWARE");
    let cache = required_path("NANOVM_IMAGE_CACHE");
    let runtime =
        GuestRuntimeDisk::prepare(guest, &cache).expect("content-addressed guest runtime disk");
    let context = tempfile::tempdir().expect("build context");
    fs::write(
        context.path().join("Dockerfile"),
        "FROM alpine:3.24\nRUN printf nanovm-image-live > /nanovm-image-proof\nWORKDIR /workspace\n",
    )
    .expect("Dockerfile");

    let image = VmImageBuilder::new(vmm, runtime.path())
        .firmware_directory(firmware)
        .vmm_arg("--vmm")
        .prepare(context.path(), DISK_BYTES, cache, CachePolicy::Reuse)
        .await
        .expect("prepared image");

    let mut disk = Reader::new(image.path()).expect("prepared ext4");
    assert_eq!(
        disk.read_file("/nanovm-image-proof", 0, Some(64))
            .expect("proof file"),
        b"nanovm-image-live"
    );
    assert_eq!(image.workdir(), "/workspace");
}

fn required_path(name: &str) -> PathBuf {
    std::env::var_os(name).map_or_else(
        || panic!("{name} must name an existing live-test input"),
        PathBuf::from,
    )
}
