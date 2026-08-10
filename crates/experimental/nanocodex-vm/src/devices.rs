use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    io::Read as _,
    path::{Path, PathBuf},
    str::FromStr,
};

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::confidential::ConfidentialNvidiaProfile;

const PCI_SYSFS_ROOT: &str = "/sys/bus/pci/devices";
const NVIDIA_VENDOR_ID: u16 = 0x10de;
const NVIDIA_B200_DEVICE_ID: u16 = 0x2901;
const MELLANOX_VENDOR_ID: u16 = 0x15b3;
const MAX_PCI_VPD_BYTES: usize = 64 * 1024;

/// Canonical PCI domain, bus, slot, and function address.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct PciAddress(String);

impl<'de> Deserialize<'de> for PciAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl PciAddress {
    /// Parses a canonical lowercase `dddd:bb:ss.f` address.
    ///
    /// # Errors
    ///
    /// Returns an error for abbreviated, uppercase, or out-of-range addresses.
    pub fn new(address: impl Into<String>) -> Result<Self, DeviceBundleError> {
        let address = address.into();
        validate_pci_address(&address)?;
        Ok(Self(address))
    }

    /// Returns the canonical sysfs address string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PciAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for PciAddress {
    type Err = DeviceBundleError;

    fn from_str(address: &str) -> Result<Self, Self::Err> {
        Self::new(address)
    }
}

/// Security-relevant function of one assigned PCI device.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidentialPciRole {
    /// One complete NVIDIA B200 GPU function.
    NvidiaB200Gpu,
    /// One CX-7 bridge function used to manage the B200 NVSwitch fabric.
    NvidiaCx7FabricBridge,
}

/// Expected immutable PCI identity for one assigned function.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfidentialPciDevice {
    address: PciAddress,
    role: ConfidentialPciRole,
    vendor_id: u16,
    device_id: u16,
    expected_vpd_sha256: Option<[u8; 32]>,
}

impl ConfidentialPciDevice {
    /// Creates one reviewed NVIDIA B200 identity (`10de:2901`).
    #[must_use]
    pub const fn b200(address: PciAddress) -> Self {
        Self {
            address,
            role: ConfidentialPciRole::NvidiaB200Gpu,
            vendor_id: NVIDIA_VENDOR_ID,
            device_id: NVIDIA_B200_DEVICE_ID,
            expected_vpd_sha256: None,
        }
    }

    /// Creates one platform-selected CX-7 bridge identity.
    ///
    /// The device ID is explicit because HGX B200 platform revisions can use
    /// different CX-7 functions. Live admission must additionally validate
    /// the platform VPD against an operator-owned reference value.
    #[must_use]
    pub const fn cx7_fabric_bridge(
        address: PciAddress,
        device_id: u16,
        expected_vpd_sha256: [u8; 32],
    ) -> Self {
        Self {
            address,
            role: ConfidentialPciRole::NvidiaCx7FabricBridge,
            vendor_id: MELLANOX_VENDOR_ID,
            device_id,
            expected_vpd_sha256: Some(expected_vpd_sha256),
        }
    }

    /// Returns the canonical host PCI address.
    #[must_use]
    pub const fn address(&self) -> &PciAddress {
        &self.address
    }

    /// Returns the device's security-relevant role.
    #[must_use]
    pub const fn role(&self) -> ConfidentialPciRole {
        self.role
    }

    /// Returns the expected PCI vendor ID.
    #[must_use]
    pub const fn vendor_id(&self) -> u16 {
        self.vendor_id
    }

    /// Returns the expected PCI device ID.
    #[must_use]
    pub const fn device_id(&self) -> u16 {
        self.device_id
    }

    /// Returns the operator-pinned VPD digest required for a CX-7 bridge PF.
    #[must_use]
    pub const fn expected_vpd_sha256(&self) -> Option<&[u8; 32]> {
        self.expected_vpd_sha256.as_ref()
    }
}

/// Exact host device assignment required by one B200 profile.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfidentialDeviceBundle {
    profile: ConfidentialNvidiaProfile,
    devices: Vec<ConfidentialPciDevice>,
}

impl ConfidentialDeviceBundle {
    /// Creates the exact one-B200, zero-bridge assignment.
    #[must_use]
    pub fn b200_single(gpu: PciAddress) -> Self {
        Self {
            profile: ConfidentialNvidiaProfile::B200Single,
            devices: vec![ConfidentialPciDevice::b200(gpu)],
        }
    }

    /// Creates the exact eight-B200, four-CX-7-function HGX assignment.
    ///
    /// # Errors
    ///
    /// Returns an error when any PCI address is repeated.
    pub fn b200_hgx_8(
        gpus: [PciAddress; 8],
        cx7_bridges: [(PciAddress, u16, [u8; 32]); 4],
    ) -> Result<Self, DeviceBundleError> {
        let devices = gpus
            .into_iter()
            .map(ConfidentialPciDevice::b200)
            .chain(cx7_bridges.into_iter().map(|(address, device_id, vpd)| {
                ConfidentialPciDevice::cx7_fabric_bridge(address, device_id, vpd)
            }))
            .collect::<Vec<_>>();
        reject_duplicate_addresses(&devices)?;
        Ok(Self {
            profile: ConfidentialNvidiaProfile::B200Hgx8EncryptedNvlink,
            devices,
        })
    }

    /// Returns the exact NVIDIA profile this bundle can satisfy.
    #[must_use]
    pub const fn profile(&self) -> ConfidentialNvidiaProfile {
        self.profile
    }

    /// Returns GPU functions first and CX-7 bridge functions second.
    #[must_use]
    pub fn devices(&self) -> &[ConfidentialPciDevice] {
        &self.devices
    }

    /// Resolves and validates this bundle against Linux PCI sysfs.
    ///
    /// Validation requires exact vendor/device identities, `vfio-pci`
    /// ownership, an exact operator-pinned CX-7 VPD digest, an IOMMU group for
    /// every function, and no unlisted sibling in any selected group. It does
    /// not validate GPU CC mode, reset ownership, or NVLink state; those remain
    /// separate launch gates.
    ///
    /// # Errors
    ///
    /// Returns the first deterministic identity, ownership, isolation, or
    /// topology failure.
    pub fn resolve_linux(&self) -> Result<ResolvedConfidentialDeviceBundle, DeviceBundleError> {
        self.resolve_at(Path::new(PCI_SYSFS_ROOT))
    }

    fn resolve_at(
        &self,
        pci_root: &Path,
    ) -> Result<ResolvedConfidentialDeviceBundle, DeviceBundleError> {
        validate_shape(self.profile, &self.devices)?;
        reject_duplicate_addresses(&self.devices)?;
        let selected = self
            .devices
            .iter()
            .map(|device| device.address.as_str())
            .collect::<BTreeSet<_>>();
        let mut iommu_groups = BTreeMap::new();
        for device in &self.devices {
            let root = pci_root.join(device.address.as_str());
            let actual_vendor = read_hex_u16(&root.join("vendor"))?;
            let actual_device = read_hex_u16(&root.join("device"))?;
            if actual_vendor != device.vendor_id || actual_device != device.device_id {
                return Err(DeviceBundleError::PciIdentityMismatch {
                    address: device.address.clone(),
                    expected_vendor: device.vendor_id,
                    expected_device: device.device_id,
                    actual_vendor,
                    actual_device,
                });
            }
            if let Some(expected) = device.expected_vpd_sha256 {
                let actual = read_vpd_digest(&root.join("vpd"))?;
                if actual != expected {
                    return Err(DeviceBundleError::VpdMismatch {
                        address: device.address.clone(),
                        expected,
                        actual,
                    });
                }
            }
            let driver =
                fs::read_link(root.join("driver")).map_err(|source| DeviceBundleError::Sysfs {
                    path: root.join("driver"),
                    source,
                })?;
            if driver.file_name().and_then(|name| name.to_str()) != Some("vfio-pci") {
                return Err(DeviceBundleError::NotBoundToVfio {
                    address: device.address.clone(),
                    driver: driver
                        .file_name()
                        .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
                });
            }
            let group_link = root.join("iommu_group");
            let group_path =
                fs::canonicalize(&group_link).map_err(|source| DeviceBundleError::Sysfs {
                    path: group_link,
                    source,
                })?;
            let group = group_path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.parse::<u32>().ok())
                .ok_or_else(|| DeviceBundleError::InvalidIommuGroup {
                    address: device.address.clone(),
                    path: group_path.clone(),
                })?;
            for sibling in read_group_devices(&group_path)? {
                if !selected.contains(sibling.as_str()) {
                    return Err(DeviceBundleError::UnassignedIommuSibling {
                        address: device.address.clone(),
                        sibling,
                    });
                }
            }
            iommu_groups.insert(device.address.clone(), group);
        }
        Ok(ResolvedConfidentialDeviceBundle {
            bundle: self.clone(),
            iommu_groups,
        })
    }
}

/// Host-resolved device bundle ready for an audited VMM assignment boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedConfidentialDeviceBundle {
    bundle: ConfidentialDeviceBundle,
    iommu_groups: BTreeMap<PciAddress, u32>,
}

impl ResolvedConfidentialDeviceBundle {
    /// Returns the exact validated device bundle.
    #[must_use]
    pub const fn bundle(&self) -> &ConfidentialDeviceBundle {
        &self.bundle
    }

    /// Returns each PCI address's validated IOMMU group.
    #[must_use]
    pub const fn iommu_groups(&self) -> &BTreeMap<PciAddress, u32> {
        &self.iommu_groups
    }
}

/// Rejected confidential PCI bundle or Linux host assignment.
#[derive(Debug, Error)]
pub enum DeviceBundleError {
    /// A PCI address was not in canonical form.
    #[error("PCI address {address:?} is not canonical dddd:bb:ss.f")]
    InvalidPciAddress {
        /// Rejected address.
        address: String,
    },
    /// One PCI function was listed more than once.
    #[error("PCI address {address} is duplicated in the confidential device bundle")]
    DuplicatePciAddress {
        /// Repeated address.
        address: PciAddress,
    },
    /// A deserialized bundle did not match its named exact topology.
    #[error("confidential device bundle does not match its named B200 topology")]
    TopologyMismatch,
    /// Linux sysfs could not be inspected.
    #[error("failed to inspect {path}: {source}")]
    Sysfs {
        /// Path which failed.
        path: PathBuf,
        /// Underlying filesystem failure.
        source: std::io::Error,
    },
    /// A selected PCI function had a different immutable identity.
    #[error(
        "PCI function {address} is {actual_vendor:04x}:{actual_device:04x}; expected {expected_vendor:04x}:{expected_device:04x}"
    )]
    PciIdentityMismatch {
        /// Selected address.
        address: PciAddress,
        /// Expected vendor ID.
        expected_vendor: u16,
        /// Expected device ID.
        expected_device: u16,
        /// Observed vendor ID.
        actual_vendor: u16,
        /// Observed device ID.
        actual_device: u16,
    },
    /// A CX-7 PF did not match its operator-reviewed production VPD.
    #[error(
        "PCI function {address} VPD SHA-256 was {actual}; expected {expected}",
        actual = hex::encode(.actual),
        expected = hex::encode(.expected)
    )]
    VpdMismatch {
        /// Selected bridge PF.
        address: PciAddress,
        /// Pinned VPD digest.
        expected: [u8; 32],
        /// Observed VPD digest.
        actual: [u8; 32],
    },
    /// A selected function was not owned by `vfio-pci`.
    #[error("PCI function {address} is bound to {driver:?}, not vfio-pci")]
    NotBoundToVfio {
        /// Selected address.
        address: PciAddress,
        /// Observed driver name.
        driver: String,
    },
    /// Linux exposed an invalid IOMMU group link.
    #[error("PCI function {address} has invalid IOMMU group path {path}")]
    InvalidIommuGroup {
        /// Selected address.
        address: PciAddress,
        /// Resolved group path.
        path: PathBuf,
    },
    /// A selected IOMMU group contains a function absent from the bundle.
    #[error("PCI function {address} has unassigned IOMMU sibling {sibling}")]
    UnassignedIommuSibling {
        /// Selected address whose group was inspected.
        address: PciAddress,
        /// Unlisted sibling.
        sibling: PciAddress,
    },
}

fn validate_pci_address(address: &str) -> Result<(), DeviceBundleError> {
    let bytes = address.as_bytes();
    let punctuation =
        bytes.get(4) == Some(&b':') && bytes.get(7) == Some(&b':') && bytes.get(10) == Some(&b'.');
    let hex_positions = [0, 1, 2, 3, 5, 6, 8, 9, 11];
    if bytes.len() != 12
        || !punctuation
        || hex_positions
            .into_iter()
            .any(|index| !bytes[index].is_ascii_hexdigit() || bytes[index].is_ascii_uppercase())
        || !matches!(bytes[11], b'0'..=b'7')
    {
        return Err(DeviceBundleError::InvalidPciAddress {
            address: address.to_owned(),
        });
    }
    Ok(())
}

fn validate_shape(
    profile: ConfidentialNvidiaProfile,
    devices: &[ConfidentialPciDevice],
) -> Result<(), DeviceBundleError> {
    let gpu_count = devices
        .iter()
        .filter(|device| device.role == ConfidentialPciRole::NvidiaB200Gpu)
        .count();
    let bridge_count = devices
        .iter()
        .filter(|device| device.role == ConfidentialPciRole::NvidiaCx7FabricBridge)
        .count();
    let expected = match profile {
        ConfidentialNvidiaProfile::B200Single => (1, 0),
        ConfidentialNvidiaProfile::B200Hgx8EncryptedNvlink => (8, 4),
    };
    if (gpu_count, bridge_count) != expected || devices.len() != gpu_count + bridge_count {
        return Err(DeviceBundleError::TopologyMismatch);
    }
    if profile == ConfidentialNvidiaProfile::B200Hgx8EncryptedNvlink {
        let bridges = devices
            .iter()
            .filter(|device| device.role == ConfidentialPciRole::NvidiaCx7FabricBridge)
            .collect::<Vec<_>>();
        let Some(slot) = bridges.first().map(|device| &device.address.as_str()[..11]) else {
            return Err(DeviceBundleError::TopologyMismatch);
        };
        let functions = bridges
            .iter()
            .map(|device| device.address.as_str().as_bytes()[11])
            .collect::<BTreeSet<_>>();
        if bridges
            .iter()
            .any(|device| &device.address.as_str()[..11] != slot)
            || functions != BTreeSet::from(*b"0123")
        {
            return Err(DeviceBundleError::TopologyMismatch);
        }
    }
    Ok(())
}

fn reject_duplicate_addresses(devices: &[ConfidentialPciDevice]) -> Result<(), DeviceBundleError> {
    let mut addresses = BTreeSet::new();
    for device in devices {
        if !addresses.insert(device.address.clone()) {
            return Err(DeviceBundleError::DuplicatePciAddress {
                address: device.address.clone(),
            });
        }
    }
    Ok(())
}

fn read_hex_u16(path: &Path) -> Result<u16, DeviceBundleError> {
    let value = fs::read_to_string(path).map_err(|source| DeviceBundleError::Sysfs {
        path: path.to_owned(),
        source,
    })?;
    u16::from_str_radix(value.trim().trim_start_matches("0x"), 16).map_err(|source| {
        DeviceBundleError::Sysfs {
            path: path.to_owned(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
        }
    })
}

fn read_vpd_digest(path: &Path) -> Result<[u8; 32], DeviceBundleError> {
    let mut value = Vec::new();
    fs::File::open(path)
        .map_err(|source| DeviceBundleError::Sysfs {
            path: path.to_owned(),
            source,
        })?
        .take((MAX_PCI_VPD_BYTES + 1) as u64)
        .read_to_end(&mut value)
        .map_err(|source| DeviceBundleError::Sysfs {
            path: path.to_owned(),
            source,
        })?;
    if value.is_empty() || value.len() > MAX_PCI_VPD_BYTES {
        return Err(DeviceBundleError::Sysfs {
            path: path.to_owned(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "PCI VPD is empty or exceeds 64 KiB",
            ),
        });
    }
    Ok(Sha256::digest(value).into())
}

fn read_group_devices(group_path: &Path) -> Result<Vec<PciAddress>, DeviceBundleError> {
    let devices_path = group_path.join("devices");
    let entries = fs::read_dir(&devices_path).map_err(|source| DeviceBundleError::Sysfs {
        path: devices_path.clone(),
        source,
    })?;
    entries
        .map(|entry| {
            let entry = entry.map_err(|source| DeviceBundleError::Sysfs {
                path: devices_path.clone(),
                source,
            })?;
            PciAddress::new(entry.file_name().to_string_lossy())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn address(value: &str) -> PciAddress {
        PciAddress::new(value).unwrap()
    }

    #[test]
    fn pci_addresses_require_canonical_full_form() {
        assert_eq!(address("0000:1b:00.0").as_str(), "0000:1b:00.0");
        for invalid in ["1b:00.0", "0000:1B:00.0", "0000:1b:00.8", "garbage"] {
            assert!(matches!(
                PciAddress::new(invalid),
                Err(DeviceBundleError::InvalidPciAddress { .. })
            ));
        }
    }

    #[test]
    fn single_b200_bundle_has_no_ambient_fabric_devices() {
        let bundle = ConfidentialDeviceBundle::b200_single(address("0000:1b:00.0"));

        assert_eq!(bundle.profile(), ConfidentialNvidiaProfile::B200Single);
        assert_eq!(bundle.devices().len(), 1);
        assert_eq!(bundle.devices()[0].vendor_id(), NVIDIA_VENDOR_ID);
        assert_eq!(bundle.devices()[0].device_id(), NVIDIA_B200_DEVICE_ID);
    }

    #[test]
    fn hgx_bundle_rejects_a_repeated_function() {
        let repeated = address("0000:1b:00.0");
        let gpus = std::array::from_fn(|index| {
            if index == 7 {
                repeated.clone()
            } else {
                address(&format!("0000:{:02x}:00.0", 0x1b + index))
            }
        });
        let bridges = std::array::from_fn(|index| {
            (
                address(&format!("0000:05:00.{index}")),
                0x1021,
                [u8::try_from(index).unwrap(); 32],
            )
        });

        assert!(matches!(
            ConfidentialDeviceBundle::b200_hgx_8(gpus, bridges),
            Err(DeviceBundleError::DuplicatePciAddress { .. })
        ));
    }

    fn fake_pci_device(temporary: &tempfile::TempDir, address: &str, group: u32) -> PathBuf {
        let pci_root = temporary.path().join("pci");
        let device_root = pci_root.join(address);
        let group_root = temporary.path().join("groups").join(group.to_string());
        let driver_root = temporary.path().join("drivers/vfio-pci");
        fs::create_dir_all(&device_root).unwrap();
        fs::create_dir_all(group_root.join("devices")).unwrap();
        fs::create_dir_all(&driver_root).unwrap();
        fs::write(device_root.join("vendor"), "0x10de\n").unwrap();
        fs::write(device_root.join("device"), "0x2901\n").unwrap();
        symlink(&driver_root, device_root.join("driver")).unwrap();
        symlink(&group_root, device_root.join("iommu_group")).unwrap();
        symlink(&device_root, group_root.join("devices").join(address)).unwrap();
        pci_root
    }

    #[test]
    fn linux_resolution_requires_exact_identity_vfio_and_complete_group() {
        let temporary = tempfile::tempdir().unwrap();
        let pci_root = fake_pci_device(&temporary, "0000:1b:00.0", 7);
        let bundle = ConfidentialDeviceBundle::b200_single(address("0000:1b:00.0"));

        let resolved = bundle.resolve_at(&pci_root).unwrap();
        assert_eq!(resolved.iommu_groups()[&address("0000:1b:00.0")], 7);

        let sibling = address("0000:1b:00.1");
        symlink(
            temporary.path().join("unassigned"),
            temporary
                .path()
                .join("groups/7/devices")
                .join(sibling.as_str()),
        )
        .unwrap();
        assert!(matches!(
            bundle.resolve_at(&pci_root),
            Err(DeviceBundleError::UnassignedIommuSibling {
                sibling: actual,
                ..
            }) if actual == sibling
        ));
    }

    #[test]
    fn vpd_digest_is_exact_and_nonempty() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("vpd");
        fs::write(&path, b"reviewed production VPD").unwrap();
        let expected: [u8; 32] = Sha256::digest(b"reviewed production VPD").into();

        assert_eq!(read_vpd_digest(&path).unwrap(), expected);
        fs::write(&path, b"").unwrap();
        assert!(matches!(
            read_vpd_digest(&path),
            Err(DeviceBundleError::Sysfs { .. })
        ));
    }
}
