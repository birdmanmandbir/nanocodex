use std::{
    fmt,
    fs::{self, OpenOptions},
    num::NonZeroU16,
    path::Path,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::capabilities::{Capabilities, KrunFeature};

/// CPU confidential-computing technology required for one VM.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CpuTee {
    /// AMD Secure Encrypted Virtualization with Secure Nested Paging.
    AmdSevSnp,
    /// Intel Trust Domain Extensions.
    IntelTdx,
    /// AWS Nitro Enclaves.
    AwsNitro,
}

impl fmt::Display for CpuTee {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AmdSevSnp => "AMD SEV-SNP",
            Self::IntelTdx => "Intel TDX",
            Self::AwsNitro => "AWS Nitro Enclaves",
        })
    }
}

/// Exact confidential-computing requirements selected by the caller.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfidentialVmProfile {
    cpu_tee: CpuTee,
    nvidia_gpus: u16,
    nvidia_nv_switches: u16,
}

impl ConfidentialVmProfile {
    /// Requires an AMD SEV-SNP VM with debug and migration disabled.
    #[must_use]
    pub const fn amd_sev_snp() -> Self {
        Self::new(CpuTee::AmdSevSnp)
    }

    /// Requires an Intel TDX VM with debug and migration disabled.
    #[must_use]
    pub const fn intel_tdx() -> Self {
        Self::new(CpuTee::IntelTdx)
    }

    /// Requires an AWS Nitro Enclave.
    #[must_use]
    pub const fn aws_nitro() -> Self {
        Self::new(CpuTee::AwsNitro)
    }

    const fn new(cpu_tee: CpuTee) -> Self {
        Self {
            cpu_tee,
            nvidia_gpus: 0,
            nvidia_nv_switches: 0,
        }
    }

    /// Requires exactly `count` confidential NVIDIA GPUs.
    #[must_use]
    pub const fn nvidia_gpus(mut self, count: NonZeroU16) -> Self {
        self.nvidia_gpus = count.get();
        self
    }

    /// Requires exactly `count` confidential NVIDIA NVSwitch components.
    #[must_use]
    pub const fn nvidia_nv_switches(mut self, count: NonZeroU16) -> Self {
        self.nvidia_nv_switches = count.get();
        self
    }

    /// Returns the required CPU TEE.
    #[must_use]
    pub const fn cpu_tee(&self) -> CpuTee {
        self.cpu_tee
    }

    /// Returns the exact number of required confidential NVIDIA GPUs.
    #[must_use]
    pub const fn nvidia_gpu_count(&self) -> u16 {
        self.nvidia_gpus
    }

    /// Returns the exact number of required confidential NVIDIA switches.
    #[must_use]
    pub const fn nvidia_nv_switch_count(&self) -> u16 {
        self.nvidia_nv_switches
    }
}

/// One prerequisite checked before a confidential VM may be created.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidentialCapability {
    /// The VMM is running on Linux.
    LinuxHost,
    /// The VMM is running on x86-64.
    X86_64Host,
    /// The process can open `/dev/kvm` for reading and writing.
    KvmDevice,
    /// The active libkrun artifact was built with its TEE feature.
    LibkrunTee,
    /// The active libkrun artifact was built for AMD SEV.
    LibkrunAmdSev,
    /// The active libkrun artifact was built for Intel TDX.
    LibkrunIntelTdx,
    /// The active libkrun artifact was built for AWS Nitro Enclaves.
    LibkrunAwsNitro,
    /// The process can open `/dev/sev` for reading and writing.
    AmdSevDevice,
    /// The host CPU advertises SEV-SNP.
    AmdSevSnpCpu,
    /// The loaded KVM AMD module has SEV-SNP enabled.
    AmdSevSnpKvm,
    /// The loaded KVM Intel module has TDX enabled.
    IntelTdxKvm,
    /// The process can open `/dev/nitro_enclaves` for reading and writing.
    AwsNitroDevice,
    /// Nanocodex has a measured guest attester for the selected backend.
    MeasuredGuestAttester,
    /// libkrun can assign and attest confidential NVIDIA GPUs.
    ConfidentialNvidiaGpuAssignment,
    /// libkrun can assign and attest confidential NVIDIA NVSwitch topology.
    ConfidentialNvidiaNvSwitchAssignment,
}

impl fmt::Display for ConfidentialCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::LinuxHost => "Linux host",
            Self::X86_64Host => "x86-64 host",
            Self::KvmDevice => "read-write /dev/kvm",
            Self::LibkrunTee => "TEE-enabled libkrun artifact",
            Self::LibkrunAmdSev => "AMD SEV libkrun artifact",
            Self::LibkrunIntelTdx => "Intel TDX libkrun artifact",
            Self::LibkrunAwsNitro => "AWS Nitro libkrun artifact",
            Self::AmdSevDevice => "read-write /dev/sev",
            Self::AmdSevSnpCpu => "SEV-SNP CPU support",
            Self::AmdSevSnpKvm => "SEV-SNP-enabled KVM AMD module",
            Self::IntelTdxKvm => "TDX-enabled KVM Intel module",
            Self::AwsNitroDevice => "read-write /dev/nitro_enclaves",
            Self::MeasuredGuestAttester => "measured guest attester",
            Self::ConfidentialNvidiaGpuAssignment => {
                "confidential NVIDIA GPU assignment and attestation"
            }
            Self::ConfidentialNvidiaNvSwitchAssignment => {
                "confidential NVIDIA NVSwitch assignment and attestation"
            }
        })
    }
}

/// Result of checking one prerequisite on the local VMM host.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfidentialCapabilityCheck {
    capability: ConfidentialCapability,
    available: bool,
}

impl ConfidentialCapabilityCheck {
    const fn new(capability: ConfidentialCapability, available: bool) -> Self {
        Self {
            capability,
            available,
        }
    }

    /// Returns the checked prerequisite.
    #[must_use]
    pub const fn capability(&self) -> ConfidentialCapability {
        self.capability
    }

    /// Returns whether the exact prerequisite is currently available.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        self.available
    }
}

/// Complete local appraisal of whether one exact profile can be launched.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfidentialHostReport {
    profile: ConfidentialVmProfile,
    checks: Vec<ConfidentialCapabilityCheck>,
}

impl ConfidentialHostReport {
    pub(crate) fn detect(profile: &ConfidentialVmProfile, capabilities: &Capabilities) -> Self {
        Self::evaluate(
            profile,
            HostFacts::detect(),
            LibkrunFacts::from(capabilities),
            false,
            false,
            false,
        )
    }

    fn evaluate(
        profile: &ConfidentialVmProfile,
        host: HostFacts,
        libkrun: LibkrunFacts,
        measured_guest_attester: bool,
        confidential_nvidia_gpu_assignment: bool,
        confidential_nvidia_nv_switch_assignment: bool,
    ) -> Self {
        let mut checks = Vec::with_capacity(12);
        checks.push(ConfidentialCapabilityCheck::new(
            ConfidentialCapability::LinuxHost,
            host.linux,
        ));
        checks.push(ConfidentialCapabilityCheck::new(
            ConfidentialCapability::X86_64Host,
            host.x86_64,
        ));

        match profile.cpu_tee {
            CpuTee::AmdSevSnp => {
                checks.extend([
                    ConfidentialCapabilityCheck::new(ConfidentialCapability::KvmDevice, host.kvm),
                    ConfidentialCapabilityCheck::new(
                        ConfidentialCapability::LibkrunTee,
                        libkrun.tee,
                    ),
                    ConfidentialCapabilityCheck::new(
                        ConfidentialCapability::LibkrunAmdSev,
                        libkrun.amd_sev,
                    ),
                    ConfidentialCapabilityCheck::new(
                        ConfidentialCapability::AmdSevDevice,
                        host.amd_sev,
                    ),
                    ConfidentialCapabilityCheck::new(
                        ConfidentialCapability::AmdSevSnpCpu,
                        host.amd_sev_snp_cpu,
                    ),
                    ConfidentialCapabilityCheck::new(
                        ConfidentialCapability::AmdSevSnpKvm,
                        host.amd_sev_snp_kvm,
                    ),
                ]);
            }
            CpuTee::IntelTdx => {
                checks.extend([
                    ConfidentialCapabilityCheck::new(ConfidentialCapability::KvmDevice, host.kvm),
                    ConfidentialCapabilityCheck::new(
                        ConfidentialCapability::LibkrunTee,
                        libkrun.tee,
                    ),
                    ConfidentialCapabilityCheck::new(
                        ConfidentialCapability::LibkrunIntelTdx,
                        libkrun.intel_tdx,
                    ),
                    ConfidentialCapabilityCheck::new(
                        ConfidentialCapability::IntelTdxKvm,
                        host.intel_tdx_kvm,
                    ),
                ]);
            }
            CpuTee::AwsNitro => {
                checks.extend([
                    ConfidentialCapabilityCheck::new(
                        ConfidentialCapability::LibkrunAwsNitro,
                        libkrun.aws_nitro,
                    ),
                    ConfidentialCapabilityCheck::new(
                        ConfidentialCapability::AwsNitroDevice,
                        host.aws_nitro,
                    ),
                ]);
            }
        }

        checks.push(ConfidentialCapabilityCheck::new(
            ConfidentialCapability::MeasuredGuestAttester,
            measured_guest_attester,
        ));
        if profile.nvidia_gpus > 0 {
            checks.push(ConfidentialCapabilityCheck::new(
                ConfidentialCapability::ConfidentialNvidiaGpuAssignment,
                confidential_nvidia_gpu_assignment,
            ));
        }
        if profile.nvidia_nv_switches > 0 {
            checks.push(ConfidentialCapabilityCheck::new(
                ConfidentialCapability::ConfidentialNvidiaNvSwitchAssignment,
                confidential_nvidia_nv_switch_assignment,
            ));
        }

        Self {
            profile: profile.clone(),
            checks,
        }
    }

    /// Returns the exact profile which was evaluated.
    #[must_use]
    pub const fn profile(&self) -> &ConfidentialVmProfile {
        &self.profile
    }

    /// Returns every required capability in deterministic appraisal order.
    #[must_use]
    pub fn checks(&self) -> &[ConfidentialCapabilityCheck] {
        &self.checks
    }

    /// Iterates over unavailable requirements.
    pub fn missing(&self) -> impl Iterator<Item = ConfidentialCapability> + '_ {
        self.checks
            .iter()
            .filter(|check| !check.available)
            .map(|check| check.capability)
    }

    /// Returns whether every exact profile requirement is available.
    #[must_use]
    pub fn is_supported(&self) -> bool {
        self.missing().next().is_none()
    }

    /// Rejects the profile unless every exact requirement is available.
    ///
    /// # Errors
    ///
    /// Returns all missing capabilities without selecting a weaker profile.
    pub fn ensure_supported(&self) -> Result<(), ConfidentialVmError> {
        let missing = self.missing().collect::<Vec<_>>();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(ConfidentialVmError::UnsupportedProfile {
                tee: self.profile.cpu_tee,
                missing,
            })
        }
    }
}

/// Failure to satisfy an exact confidential-VM request.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ConfidentialVmError {
    /// The selected local host or VMM artifact lacks required capabilities.
    #[error("{tee} confidential VM is unsupported; missing capabilities: {missing:?}")]
    UnsupportedProfile {
        /// Requested CPU TEE.
        tee: CpuTee,
        /// Every unavailable requirement in deterministic appraisal order.
        missing: Vec<ConfidentialCapability>,
    },
    /// The ordinary VM configuration is incompatible with confidential launch.
    #[error("invalid confidential VM configuration: {0}")]
    InvalidConfig(&'static str),
}

#[derive(Clone, Copy)]
struct HostFacts {
    linux: bool,
    x86_64: bool,
    kvm: bool,
    amd_sev: bool,
    amd_sev_snp_cpu: bool,
    amd_sev_snp_kvm: bool,
    intel_tdx_kvm: bool,
    aws_nitro: bool,
}

impl HostFacts {
    fn detect() -> Self {
        Self {
            linux: cfg!(target_os = "linux"),
            x86_64: cfg!(target_arch = "x86_64"),
            kvm: device_is_read_write(Path::new("/dev/kvm")),
            amd_sev: device_is_read_write(Path::new("/dev/sev")),
            amd_sev_snp_cpu: proc_cpu_has_flag("sev_snp"),
            amd_sev_snp_kvm: module_parameter_enabled(&["/sys/module/kvm_amd/parameters/sev_snp"]),
            intel_tdx_kvm: module_parameter_enabled(&[
                "/sys/module/kvm_intel/parameters/tdx",
                "/sys/module/kvm_intel/parameters/enable_tdx",
            ]),
            aws_nitro: device_is_read_write(Path::new("/dev/nitro_enclaves")),
        }
    }
}

#[derive(Clone, Copy)]
struct LibkrunFacts {
    tee: bool,
    amd_sev: bool,
    intel_tdx: bool,
    aws_nitro: bool,
}

impl From<&Capabilities> for LibkrunFacts {
    fn from(capabilities: &Capabilities) -> Self {
        Self {
            tee: capabilities.has(KrunFeature::Tee),
            amd_sev: capabilities.has(KrunFeature::AmdSev),
            intel_tdx: capabilities.has(KrunFeature::IntelTdx),
            aws_nitro: capabilities.has(KrunFeature::AwsNitro),
        }
    }
}

fn device_is_read_write(path: &Path) -> bool {
    OpenOptions::new().read(true).write(true).open(path).is_ok()
}

fn proc_cpu_has_flag(expected: &str) -> bool {
    fs::read_to_string("/proc/cpuinfo").is_ok_and(|cpuinfo| {
        cpuinfo.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, flags)| {
                name.trim().eq_ignore_ascii_case("flags")
                    && flags.split_ascii_whitespace().any(|flag| flag == expected)
            })
        })
    })
}

fn module_parameter_enabled(paths: &[&str]) -> bool {
    paths.iter().any(|path| {
        fs::read_to_string(path).is_ok_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "y" | "yes"
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_snp_host() -> HostFacts {
        HostFacts {
            linux: true,
            x86_64: true,
            kvm: true,
            amd_sev: true,
            amd_sev_snp_cpu: true,
            amd_sev_snp_kvm: true,
            intel_tdx_kvm: false,
            aws_nitro: false,
        }
    }

    fn snp_libkrun() -> LibkrunFacts {
        LibkrunFacts {
            tee: true,
            amd_sev: true,
            intel_tdx: false,
            aws_nitro: false,
        }
    }

    #[test]
    fn exact_snp_profile_is_supported_only_when_every_gate_passes() {
        let profile = ConfidentialVmProfile::amd_sev_snp();
        let report = ConfidentialHostReport::evaluate(
            &profile,
            complete_snp_host(),
            snp_libkrun(),
            true,
            false,
            false,
        );

        assert!(report.is_supported());
        assert_eq!(report.profile(), &profile);
        assert!(report.missing().next().is_none());
    }

    #[test]
    fn missing_requirements_are_complete_and_never_downgraded() {
        let profile = ConfidentialVmProfile::amd_sev_snp();
        let mut host = complete_snp_host();
        host.amd_sev = false;
        host.amd_sev_snp_kvm = false;
        let mut libkrun = snp_libkrun();
        libkrun.amd_sev = false;
        let report = ConfidentialHostReport::evaluate(&profile, host, libkrun, false, false, false);

        assert_eq!(
            report.missing().collect::<Vec<_>>(),
            [
                ConfidentialCapability::LibkrunAmdSev,
                ConfidentialCapability::AmdSevDevice,
                ConfidentialCapability::AmdSevSnpKvm,
                ConfidentialCapability::MeasuredGuestAttester,
            ]
        );
        assert!(matches!(
            report.ensure_supported(),
            Err(ConfidentialVmError::UnsupportedProfile {
                tee: CpuTee::AmdSevSnp,
                ..
            })
        ));
    }

    #[test]
    fn confidential_gpu_and_switch_require_independent_support() {
        let profile = ConfidentialVmProfile::amd_sev_snp()
            .nvidia_gpus(NonZeroU16::new(2).unwrap())
            .nvidia_nv_switches(NonZeroU16::new(1).unwrap());
        let report = ConfidentialHostReport::evaluate(
            &profile,
            complete_snp_host(),
            snp_libkrun(),
            true,
            false,
            false,
        );

        assert_eq!(
            report.missing().collect::<Vec<_>>(),
            [
                ConfidentialCapability::ConfidentialNvidiaGpuAssignment,
                ConfidentialCapability::ConfidentialNvidiaNvSwitchAssignment,
            ]
        );
    }

    #[test]
    fn profile_round_trip_preserves_exact_device_counts() {
        let profile = ConfidentialVmProfile::intel_tdx()
            .nvidia_gpus(NonZeroU16::new(8).unwrap())
            .nvidia_nv_switches(NonZeroU16::new(2).unwrap());

        let encoded = serde_json::to_vec(&profile).unwrap();
        let decoded = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(profile, decoded);
    }
}
