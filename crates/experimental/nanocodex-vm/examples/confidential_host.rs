use nanocodex_vm::host::{Capabilities, ConfidentialVmProfile};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let capabilities = Capabilities::detect()?;
    for profile in [
        ConfidentialVmProfile::amd_sev_snp(),
        ConfidentialVmProfile::intel_tdx(),
        ConfidentialVmProfile::aws_nitro(),
        ConfidentialVmProfile::amd_sev_snp().nvidia_b200_single(),
        ConfidentialVmProfile::intel_tdx().nvidia_b200_hgx_8_encrypted_nvlink(),
    ] {
        let report = capabilities.confidential_report(&profile);
        let status = if report.is_supported() {
            "supported"
        } else {
            "unsupported"
        };
        println!("{}: {status}", profile.cpu_tee());
        for missing in report.missing() {
            println!("  missing: {missing}");
        }
    }
    Ok(())
}
