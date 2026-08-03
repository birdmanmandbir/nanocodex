use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=src/system_audio_capture.swift");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("build output directory"))
        .join("nanocodex-system-audio-capture");
    let status = Command::new("xcrun")
        .args(["swiftc", "-O", "-parse-as-library"])
        .arg(manifest.join("src/system_audio_capture.swift"))
        .arg("-o")
        .arg(&output)
        .status()
        .expect("xcrun must be available to build macOS meeting capture");
    assert!(
        status.success(),
        "failed to compile macOS meeting capture helper"
    );
}
