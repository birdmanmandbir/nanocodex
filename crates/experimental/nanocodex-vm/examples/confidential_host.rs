use std::collections::BTreeSet;

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
        let launch_missing = report.launch_missing().collect::<BTreeSet<_>>();
        let status = if report.is_launch_supported() {
            "launch-ready"
        } else {
            "launch-blocked"
        };
        println!("{profile:?}: {status}");
        for missing in report.missing() {
            let phase = if launch_missing.contains(&missing) {
                "launch"
            } else {
                "post-launch evidence"
            };
            println!("  missing ({phase}): {missing}");
        }
    }
    Ok(())
}
