use std::{fmt, fs::File, io, io::Read as _, path::Path};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest as _, Sha256};

const MANIFEST_VERSION: u32 = 1;
const MANIFEST_DOMAIN: &[u8] = b"nanocodex-vm-measured-guest-manifest-v1\0";
const MAX_TEXT_BYTES: usize = 4 * 1024;
const MAX_COMMAND_LINE_BYTES: usize = 64 * 1024;
const MAX_WORKLOAD_COMPONENTS: usize = 256;
const MAX_ARGUMENTS: usize = 256;

/// Maximum accepted encoded measured-guest manifest size.
pub const MAX_MEASURED_GUEST_MANIFEST_BYTES: usize = 1024 * 1024;

/// Lowercase hexadecimal SHA-256 value used by measured-guest manifests.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ManifestSha256([u8; 32]);

impl ManifestSha256 {
    /// Constructs a digest value from exact raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Hashes an in-memory artifact.
    #[must_use]
    pub fn digest(bytes: impl AsRef<[u8]>) -> Self {
        Self(Sha256::digest(bytes.as_ref()).into())
    }

    /// Parses one exact lowercase SHA-256 value.
    ///
    /// # Errors
    ///
    /// Returns an error unless `value` is exactly 64 lowercase hexadecimal
    /// characters.
    pub fn from_hex(value: &str) -> Result<Self, MeasuredGuestManifestError> {
        parse_hex(value, "SHA-256").map(Self)
    }

    /// Returns the raw digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Returns lowercase hexadecimal.
    #[must_use]
    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Debug for ManifestSha256 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ManifestSha256")
            .field(&self.to_hex())
            .finish()
    }
}

impl Serialize for ManifestSha256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ManifestSha256 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_hex(&value).map_err(de::Error::custom)
    }
}

/// Lowercase hexadecimal SHA-384 reference measurement.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct ManifestSha384([u8; 48]);

impl ManifestSha384 {
    /// Parses one exact lowercase SHA-384 value.
    ///
    /// # Errors
    ///
    /// Returns an error unless `value` is exactly 96 lowercase hexadecimal
    /// characters.
    pub fn from_hex(value: &str) -> Result<Self, MeasuredGuestManifestError> {
        parse_hex(value, "SHA-384").map(Self)
    }

    /// Returns the raw digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 48] {
        &self.0
    }

    /// Returns lowercase hexadecimal.
    #[must_use]
    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Debug for ManifestSha384 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ManifestSha384")
            .field(&self.to_hex())
            .finish()
    }
}

impl Serialize for ManifestSha384 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ManifestSha384 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_hex(&value).map_err(de::Error::custom)
    }
}

/// Portable identity of one build or launch artifact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredArtifactV1 {
    name: String,
    bytes: u64,
    sha256: ManifestSha256,
}

impl MeasuredArtifactV1 {
    /// Hashes one regular file under a portable basename.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid portable name or unreadable file.
    pub fn from_file(
        name: impl Into<String>,
        path: impl AsRef<Path>,
    ) -> Result<Self, MeasuredGuestManifestError> {
        let name = name.into();
        validate_name(&name)?;
        let path = path.as_ref();
        let metadata =
            path.metadata()
                .map_err(|source| MeasuredGuestManifestError::ReadArtifact {
                    path: path.to_owned(),
                    source,
                })?;
        if !metadata.is_file() {
            return Err(MeasuredGuestManifestError::ArtifactNotFile(path.to_owned()));
        }
        let mut file =
            File::open(path).map_err(|source| MeasuredGuestManifestError::ReadArtifact {
                path: path.to_owned(),
                source,
            })?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).map_err(|source| {
                MeasuredGuestManifestError::ReadArtifact {
                    path: path.to_owned(),
                    source,
                }
            })?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(Self {
            name,
            bytes: metadata.len(),
            sha256: ManifestSha256(hasher.finalize().into()),
        })
    }

    /// Constructs an identity from already appraised bytes and digest.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid portable name or zero-length artifact.
    pub fn new(
        name: impl Into<String>,
        bytes: u64,
        sha256: ManifestSha256,
    ) -> Result<Self, MeasuredGuestManifestError> {
        let name = name.into();
        validate_name(&name)?;
        if bytes == 0 {
            return Err(MeasuredGuestManifestError::InvalidArtifactBytes);
        }
        Ok(Self {
            name,
            bytes,
            sha256,
        })
    }

    /// Returns the portable artifact name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns its exact length.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Returns its SHA-256 identity.
    #[must_use]
    pub const fn sha256(&self) -> ManifestSha256 {
        self.sha256
    }
}

/// Source and toolchain preimage needed to reproduce a measured guest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredGuestSourceV1 {
    repository: String,
    revision: String,
    cargo_lock_sha256: ManifestSha256,
    rustc_commit: String,
    target: String,
}

impl MeasuredGuestSourceV1 {
    /// Creates a pinned source/toolchain identity.
    ///
    /// # Errors
    ///
    /// Returns an error for empty or unreasonably large text fields.
    pub fn new(
        repository: impl Into<String>,
        revision: impl Into<String>,
        cargo_lock_sha256: ManifestSha256,
        rustc_commit: impl Into<String>,
        target: impl Into<String>,
    ) -> Result<Self, MeasuredGuestManifestError> {
        let value = Self {
            repository: repository.into(),
            revision: revision.into(),
            cargo_lock_sha256,
            rustc_commit: rustc_commit.into(),
            target: target.into(),
        };
        for field in [
            &value.repository,
            &value.revision,
            &value.rustc_commit,
            &value.target,
        ] {
            validate_text(field)?;
        }
        Ok(value)
    }
}

/// Complete file inputs whose exact bytes affect the confidential launch.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredGuestArtifactsV1 {
    vmm: MeasuredArtifactV1,
    libkrun: MeasuredArtifactV1,
    firmware: MeasuredArtifactV1,
    kernel: MeasuredArtifactV1,
    initrd: MeasuredArtifactV1,
    supervisor: MeasuredArtifactV1,
    root_image: MeasuredArtifactV1,
}

impl MeasuredGuestArtifactsV1 {
    /// Collects every required launch artifact.
    #[must_use]
    pub const fn new(
        vmm: MeasuredArtifactV1,
        libkrun: MeasuredArtifactV1,
        firmware: MeasuredArtifactV1,
        kernel: MeasuredArtifactV1,
        initrd: MeasuredArtifactV1,
        supervisor: MeasuredArtifactV1,
        root_image: MeasuredArtifactV1,
    ) -> Self {
        Self {
            vmm,
            libkrun,
            firmware,
            kernel,
            initrd,
            supervisor,
            root_image,
        }
    }

    /// Returns the exact measured execution supervisor.
    #[must_use]
    pub const fn supervisor(&self) -> &MeasuredArtifactV1 {
        &self.supervisor
    }
}

/// dm-verity parameters enforced by the measured early guest before root use.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DmVerityRootV1 {
    root_hash: ManifestSha256,
    salt: ManifestSha256,
    data_block_bytes: u32,
    hash_block_bytes: u32,
    data_blocks: u64,
    hash_start_block: u64,
}

impl DmVerityRootV1 {
    /// Creates an exact dm-verity table identity.
    ///
    /// # Errors
    ///
    /// Returns an error unless both block sizes are 4096 and the data/hash
    /// ranges are nonempty and nonoverlapping.
    pub const fn new(
        root_hash: ManifestSha256,
        salt: ManifestSha256,
        data_blocks: u64,
        hash_start_block: u64,
    ) -> Result<Self, MeasuredGuestManifestError> {
        if data_blocks == 0 || hash_start_block < data_blocks {
            return Err(MeasuredGuestManifestError::InvalidVerityLayout);
        }
        Ok(Self {
            root_hash,
            salt,
            data_block_bytes: 4_096,
            hash_block_bytes: 4_096,
            data_blocks,
            hash_start_block,
        })
    }

    /// Returns the trusted root hash.
    #[must_use]
    pub const fn root_hash(&self) -> ManifestSha256 {
        self.root_hash
    }

    /// Returns the exact salt.
    #[must_use]
    pub const fn salt(&self) -> ManifestSha256 {
        self.salt
    }

    /// Returns the data-block size.
    #[must_use]
    pub const fn data_block_bytes(&self) -> u32 {
        self.data_block_bytes
    }

    /// Returns the hash-block size.
    #[must_use]
    pub const fn hash_block_bytes(&self) -> u32 {
        self.hash_block_bytes
    }

    /// Returns the number of authenticated data blocks.
    #[must_use]
    pub const fn data_blocks(&self) -> u64 {
        self.data_blocks
    }

    /// Returns the first hash-tree block on the hash device.
    #[must_use]
    pub const fn hash_start_block(&self) -> u64 {
        self.hash_start_block
    }
}

/// Root artifact plus the mechanism that authenticates every block before use.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatedGuestRootV1 {
    filesystem_uuid: String,
    dm_verity: DmVerityRootV1,
}

impl AuthenticatedGuestRootV1 {
    /// Creates a root policy. Plain read-only ext4 has no constructor because
    /// host-side read-only flags do not authenticate an untrusted block device.
    ///
    /// # Errors
    ///
    /// Returns an error unless `filesystem_uuid` is a canonical UUID.
    pub fn dm_verity(
        filesystem_uuid: impl Into<String>,
        dm_verity: DmVerityRootV1,
    ) -> Result<Self, MeasuredGuestManifestError> {
        let filesystem_uuid = filesystem_uuid.into();
        validate_filesystem_uuid(&filesystem_uuid)?;
        Ok(Self {
            filesystem_uuid,
            dm_verity,
        })
    }

    /// Returns the canonical ext4 UUID authenticated by this policy.
    #[must_use]
    pub fn filesystem_uuid(&self) -> &str {
        &self.filesystem_uuid
    }

    /// Returns the exact dm-verity table identity.
    #[must_use]
    pub const fn verity(&self) -> &DmVerityRootV1 {
        &self.dm_verity
    }
}

/// CPU confidential-computing profile selected for this exact launch.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MeasuredGuestCpuV1 {
    /// AMD SEV-SNP.
    AmdSevSnp,
    /// Intel TDX.
    IntelTdx,
    /// AWS Nitro Enclaves.
    AwsNitro,
}

/// Deliberate resource and boot policy whose changes invalidate the manifest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredGuestLaunchV1 {
    cpu: MeasuredGuestCpuV1,
    cpus: u16,
    memory_mib: u32,
    kernel_command_line: String,
    network: String,
    debug: bool,
    migration: bool,
}

impl MeasuredGuestLaunchV1 {
    /// Creates a production launch policy with debug and migration disabled.
    ///
    /// # Errors
    ///
    /// Returns an error for zero resources, an empty/oversized kernel command
    /// line, or a network mode other than `disabled` or `controlled-egress`.
    pub fn production(
        cpu: MeasuredGuestCpuV1,
        cpus: u16,
        memory_mib: u32,
        kernel_command_line: impl Into<String>,
        network: impl Into<String>,
    ) -> Result<Self, MeasuredGuestManifestError> {
        let kernel_command_line = kernel_command_line.into();
        let network = network.into();
        let launch = Self {
            cpu,
            cpus,
            memory_mib,
            kernel_command_line,
            network,
            debug: false,
            migration: false,
        };
        validate_launch(&launch)?;
        Ok(launch)
    }
}

/// Why one workload component must be measured before it handles plaintext.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadComponentKindV1 {
    /// Application or service code.
    ApplicationCode,
    /// Immutable OCI image manifest.
    ContainerImage,
    /// Model-weight artifact or immutable model snapshot.
    ModelWeights,
    /// Policy or runtime configuration.
    Configuration,
}

/// Exact application, container, model, or configuration identity.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadComponentV1 {
    kind: WorkloadComponentKindV1,
    name: String,
    sha256: ManifestSha256,
}

impl WorkloadComponentV1 {
    /// Creates one measured workload component.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid portable name.
    pub fn new(
        kind: WorkloadComponentKindV1,
        name: impl Into<String>,
        sha256: ManifestSha256,
    ) -> Result<Self, MeasuredGuestManifestError> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Self { kind, name, sha256 })
    }

    /// Returns why this component must be measured.
    #[must_use]
    pub const fn kind(&self) -> WorkloadComponentKindV1 {
        self.kind
    }

    /// Returns the portable component name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the exact component digest.
    #[must_use]
    pub const fn sha256(&self) -> ManifestSha256 {
        self.sha256
    }
}

/// Everything allowed to touch plaintext for one workload invocation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredWorkloadV1 {
    components: Vec<WorkloadComponentV1>,
    argv: Vec<String>,
}

impl MeasuredWorkloadV1 {
    /// Creates a canonical ordered workload identity.
    ///
    /// Components are sorted by kind and name, so semantically identical sets
    /// have one digest. At least one application-code component is required.
    ///
    /// # Errors
    ///
    /// Returns an error for empty, duplicate, oversized, or incomplete input.
    pub fn new(
        mut components: Vec<WorkloadComponentV1>,
        argv: Vec<String>,
    ) -> Result<Self, MeasuredGuestManifestError> {
        if components.is_empty()
            || components.len() > MAX_WORKLOAD_COMPONENTS
            || !components
                .iter()
                .any(|component| component.kind == WorkloadComponentKindV1::ApplicationCode)
            || argv.is_empty()
            || argv.len() > MAX_ARGUMENTS
            || argv.iter().any(|argument| {
                argument.is_empty() || argument.len() > MAX_TEXT_BYTES || argument.contains('\0')
            })
        {
            return Err(MeasuredGuestManifestError::InvalidWorkload);
        }
        for component in &components {
            validate_name(&component.name)?;
        }
        components.sort();
        if components
            .windows(2)
            .any(|pair| pair[0].kind == pair[1].kind && pair[0].name == pair[1].name)
        {
            return Err(MeasuredGuestManifestError::DuplicateWorkloadComponent);
        }
        Ok(Self { components, argv })
    }

    /// Returns the canonical measured component set.
    #[must_use]
    pub fn components(&self) -> &[WorkloadComponentV1] {
        &self.components
    }

    /// Returns the exact plaintext-handling command line.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
    }
}

/// Hardware reference values computed offline from the same launch artifacts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "cpu", rename_all = "snake_case", deny_unknown_fields)]
pub enum MeasuredGuestReferenceV1 {
    /// Exact SEV-SNP launch measurement for this manifest's vCPU count.
    AmdSevSnp {
        /// Expected SNP `MEASUREMENT`.
        launch_measurement: ManifestSha384,
    },
    /// Exact TDX launch and runtime-register baseline values.
    IntelTdx {
        /// Expected `MRTD`.
        mrtd: ManifestSha384,
        /// Expected boot-owned `RTMR0`.
        rtmr0: ManifestSha384,
        /// Expected root/boot-owned `RTMR1`.
        rtmr1: ManifestSha384,
        /// Expected initrd/cmdline-owned `RTMR2`.
        rtmr2: ManifestSha384,
        /// Expected `RTMR3` before extending this manifest digest.
        rtmr3_baseline: ManifestSha384,
    },
    /// Exact Nitro PCR policy, sorted by PCR index.
    AwsNitro {
        /// Required PCR index/value pairs.
        pcrs: std::collections::BTreeMap<u8, ManifestSha384>,
    },
}

impl MeasuredGuestReferenceV1 {
    const fn cpu(&self) -> MeasuredGuestCpuV1 {
        match self {
            Self::AmdSevSnp { .. } => MeasuredGuestCpuV1::AmdSevSnp,
            Self::IntelTdx { .. } => MeasuredGuestCpuV1::IntelTdx,
            Self::AwsNitro { .. } => MeasuredGuestCpuV1::AwsNitro,
        }
    }
}

/// Complete, strict preimage for confidential-guest appraisal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredGuestManifestV1 {
    version: u32,
    source: MeasuredGuestSourceV1,
    artifacts: MeasuredGuestArtifactsV1,
    authenticated_root: AuthenticatedGuestRootV1,
    launch: MeasuredGuestLaunchV1,
    workload: MeasuredWorkloadV1,
    reference: MeasuredGuestReferenceV1,
}

impl MeasuredGuestManifestV1 {
    /// Constructs a complete manifest and verifies its cross-field invariants.
    ///
    /// # Errors
    ///
    /// Returns an error when platform references do not match the selected
    /// CPU profile, workload components are noncanonical, or an artifact/root
    /// field is invalid.
    pub fn new(
        source: MeasuredGuestSourceV1,
        artifacts: MeasuredGuestArtifactsV1,
        authenticated_root: AuthenticatedGuestRootV1,
        launch: MeasuredGuestLaunchV1,
        workload: MeasuredWorkloadV1,
        reference: MeasuredGuestReferenceV1,
    ) -> Result<Self, MeasuredGuestManifestError> {
        let manifest = Self {
            version: MANIFEST_VERSION,
            source,
            artifacts,
            authenticated_root,
            launch,
            workload,
            reference,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// Parses a strict manifest. Unknown fields and unsupported versions fail.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid JSON or a violated manifest invariant.
    pub fn from_json(bytes: &[u8]) -> Result<Self, MeasuredGuestManifestError> {
        if bytes.len() > MAX_MEASURED_GUEST_MANIFEST_BYTES {
            return Err(MeasuredGuestManifestError::ManifestTooLarge {
                bytes: bytes.len(),
                max: MAX_MEASURED_GUEST_MANIFEST_BYTES,
            });
        }
        serde_json::from_slice(bytes).map_err(MeasuredGuestManifestError::Json)
    }

    /// Serializes canonical compact JSON used as the digest preimage.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if serde cannot encode the manifest.
    pub fn canonical_json(&self) -> Result<Vec<u8>, MeasuredGuestManifestError> {
        serde_json::to_vec(self).map_err(MeasuredGuestManifestError::Json)
    }

    /// Returns the domain-separated workload-manifest digest bound into
    /// attestation and secret-release policy.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if serde cannot encode the manifest.
    pub fn digest(&self) -> Result<ManifestSha256, MeasuredGuestManifestError> {
        let mut hasher = Sha256::new();
        hasher.update(MANIFEST_DOMAIN);
        hasher.update(self.canonical_json()?);
        Ok(ManifestSha256(hasher.finalize().into()))
    }

    /// Returns the selected CPU TEE.
    #[must_use]
    pub const fn cpu(&self) -> MeasuredGuestCpuV1 {
        self.launch.cpu
    }

    /// Returns every launch artifact identity.
    #[must_use]
    pub const fn artifacts(&self) -> &MeasuredGuestArtifactsV1 {
        &self.artifacts
    }

    /// Returns the complete measured plaintext-handling workload.
    #[must_use]
    pub const fn workload(&self) -> &MeasuredWorkloadV1 {
        &self.workload
    }

    /// Returns the offline hardware reference values.
    #[must_use]
    pub const fn reference(&self) -> &MeasuredGuestReferenceV1 {
        &self.reference
    }

    fn validate(&self) -> Result<(), MeasuredGuestManifestError> {
        if self.version != MANIFEST_VERSION {
            return Err(MeasuredGuestManifestError::UnsupportedVersion(self.version));
        }
        for field in [
            &self.source.repository,
            &self.source.revision,
            &self.source.rustc_commit,
            &self.source.target,
        ] {
            validate_text(field)?;
        }
        for artifact in [
            &self.artifacts.vmm,
            &self.artifacts.libkrun,
            &self.artifacts.firmware,
            &self.artifacts.kernel,
            &self.artifacts.initrd,
            &self.artifacts.supervisor,
            &self.artifacts.root_image,
        ] {
            validate_name(&artifact.name)?;
            if artifact.bytes == 0 {
                return Err(MeasuredGuestManifestError::InvalidArtifactBytes);
            }
        }
        validate_filesystem_uuid(&self.authenticated_root.filesystem_uuid)?;
        let verity = &self.authenticated_root.dm_verity;
        if verity.data_block_bytes != 4_096
            || verity.hash_block_bytes != 4_096
            || verity.data_blocks == 0
            || verity.hash_start_block < verity.data_blocks
        {
            return Err(MeasuredGuestManifestError::InvalidVerityLayout);
        }
        validate_launch(&self.launch)?;
        if self.reference.cpu() != self.launch.cpu {
            return Err(MeasuredGuestManifestError::ReferenceProfileMismatch);
        }
        if matches!(&self.reference, MeasuredGuestReferenceV1::AwsNitro { pcrs } if pcrs.is_empty() || pcrs.keys().any(|index| *index > 31))
        {
            return Err(MeasuredGuestManifestError::InvalidNitroPcrs);
        }
        let verity_data_bytes = self
            .authenticated_root
            .dm_verity
            .data_blocks
            .checked_mul(u64::from(
                self.authenticated_root.dm_verity.data_block_bytes,
            ))
            .ok_or(MeasuredGuestManifestError::InvalidVerityLayout)?;
        let verity_hash_offset = self
            .authenticated_root
            .dm_verity
            .hash_start_block
            .checked_mul(u64::from(
                self.authenticated_root.dm_verity.hash_block_bytes,
            ))
            .ok_or(MeasuredGuestManifestError::InvalidVerityLayout)?;
        if verity_data_bytes > self.artifacts.root_image.bytes
            || verity_hash_offset >= self.artifacts.root_image.bytes
        {
            return Err(MeasuredGuestManifestError::InvalidVerityLayout);
        }
        let canonical =
            MeasuredWorkloadV1::new(self.workload.components.clone(), self.workload.argv.clone())?;
        if canonical != self.workload {
            return Err(MeasuredGuestManifestError::NonCanonicalWorkload);
        }
        if !self.workload.components.iter().any(|component| {
            component.kind == WorkloadComponentKindV1::ApplicationCode
                && component.sha256 == self.artifacts.supervisor.sha256
        }) {
            return Err(MeasuredGuestManifestError::MissingSupervisorComponent);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for MeasuredGuestManifestV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireManifest {
            version: u32,
            source: MeasuredGuestSourceV1,
            artifacts: MeasuredGuestArtifactsV1,
            authenticated_root: AuthenticatedGuestRootV1,
            launch: MeasuredGuestLaunchV1,
            workload: MeasuredWorkloadV1,
            reference: MeasuredGuestReferenceV1,
        }

        let wire = WireManifest::deserialize(deserializer)?;
        let manifest = Self {
            version: wire.version,
            source: wire.source,
            artifacts: wire.artifacts,
            authenticated_root: wire.authenticated_root,
            launch: wire.launch,
            workload: wire.workload,
            reference: wire.reference,
        };
        manifest.validate().map_err(de::Error::custom)?;
        Ok(manifest)
    }
}

/// Invalid input, artifact, or schema in a measured-guest manifest.
#[derive(Debug, thiserror::Error)]
pub enum MeasuredGuestManifestError {
    /// A hexadecimal digest had the wrong syntax or size.
    #[error("invalid {0} digest; expected exact lowercase hexadecimal")]
    InvalidDigest(&'static str),
    /// A portable artifact or component name was invalid.
    #[error("artifact names must be nonempty portable basenames of at most 255 bytes")]
    InvalidName,
    /// An informational source field was invalid.
    #[error("manifest text fields must contain between 1 and 4096 bytes without NUL")]
    InvalidText,
    /// A measured artifact was empty.
    #[error("measured artifacts must contain at least one byte")]
    InvalidArtifactBytes,
    /// A path supplied as an artifact was not a regular file.
    #[error("measured artifact {} is not a regular file", .0.display())]
    ArtifactNotFile(std::path::PathBuf),
    /// A measured artifact could not be read.
    #[error("failed to read measured artifact {}", path.display())]
    ReadArtifact {
        /// Artifact path.
        path: std::path::PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: io::Error,
    },
    /// dm-verity block ranges were empty or overlapping.
    #[error("dm-verity data and hash ranges must be nonempty and nonoverlapping")]
    InvalidVerityLayout,
    /// The filesystem UUID was not canonical.
    #[error("authenticated-root filesystem UUID is invalid or noncanonical")]
    InvalidFilesystemUuid,
    /// Launch resources, command line, or network policy was invalid.
    #[error("measured guest launch policy is invalid")]
    InvalidLaunchPolicy,
    /// Workload components or argv were incomplete or oversized.
    #[error("measured workload requires bounded argv and at least one application-code component")]
    InvalidWorkload,
    /// Two workload components were identical.
    #[error("measured workload contains a duplicate component")]
    DuplicateWorkloadComponent,
    /// Workload components were not in canonical order.
    #[error("measured workload components are not in canonical order")]
    NonCanonicalWorkload,
    /// The exact measured supervisor was absent from the plaintext-handling set.
    #[error("measured workload does not include the exact guest supervisor")]
    MissingSupervisorComponent,
    /// Reference values selected another CPU TEE.
    #[error("hardware reference values do not match the launch CPU profile")]
    ReferenceProfileMismatch,
    /// Nitro references omitted PCRs or used an unsupported index.
    #[error("Nitro reference values must pin PCR indexes 0 through 31")]
    InvalidNitroPcrs,
    /// The manifest schema is not supported by this build.
    #[error(
        "unsupported measured guest manifest version {0}; rebuild with version {MANIFEST_VERSION}"
    )]
    UnsupportedVersion(u32),
    /// JSON encoding or decoding failed.
    #[error("invalid measured guest manifest JSON: {0}")]
    Json(serde_json::Error),
    /// The encoded manifest exceeded its public input bound.
    #[error("measured guest manifest is {bytes} bytes; maximum is {max}")]
    ManifestTooLarge {
        /// Supplied encoded size.
        bytes: usize,
        /// Maximum accepted encoded size.
        max: usize,
    },
}

fn parse_hex<const N: usize>(
    value: &str,
    label: &'static str,
) -> Result<[u8; N], MeasuredGuestManifestError> {
    if value.len() != N * 2
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(MeasuredGuestManifestError::InvalidDigest(label));
    }
    let decoded =
        hex::decode(value).map_err(|_| MeasuredGuestManifestError::InvalidDigest(label))?;
    decoded
        .try_into()
        .map_err(|_| MeasuredGuestManifestError::InvalidDigest(label))
}

fn validate_name(value: &str) -> Result<(), MeasuredGuestManifestError> {
    if value.is_empty()
        || value.len() > 255
        || value == "."
        || value == ".."
        || value.contains(['/', '\\', '\0'])
    {
        Err(MeasuredGuestManifestError::InvalidName)
    } else {
        Ok(())
    }
}

fn validate_text(value: &str) -> Result<(), MeasuredGuestManifestError> {
    if value.is_empty() || value.len() > MAX_TEXT_BYTES || value.contains('\0') {
        Err(MeasuredGuestManifestError::InvalidText)
    } else {
        Ok(())
    }
}

fn validate_filesystem_uuid(value: &str) -> Result<(), MeasuredGuestManifestError> {
    let parsed = uuid::Uuid::parse_str(value)
        .map_err(|_| MeasuredGuestManifestError::InvalidFilesystemUuid)?;
    if parsed.to_string() == value {
        Ok(())
    } else {
        Err(MeasuredGuestManifestError::InvalidFilesystemUuid)
    }
}

fn validate_launch(value: &MeasuredGuestLaunchV1) -> Result<(), MeasuredGuestManifestError> {
    if value.cpus == 0
        || value.memory_mib == 0
        || value.kernel_command_line.is_empty()
        || value.kernel_command_line.len() > MAX_COMMAND_LINE_BYTES
        || value.kernel_command_line.contains('\0')
        || !matches!(value.network.as_str(), "disabled" | "controlled-egress")
        || value.debug
        || value.migration
    {
        Err(MeasuredGuestManifestError::InvalidLaunchPolicy)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(byte: u8) -> ManifestSha256 {
        ManifestSha256([byte; 32])
    }

    fn sha384(byte: u8) -> ManifestSha384 {
        ManifestSha384([byte; 48])
    }

    fn artifact(name: &str, byte: u8) -> MeasuredArtifactV1 {
        MeasuredArtifactV1::new(name, 1, digest(byte)).unwrap()
    }

    fn fixture() -> MeasuredGuestManifestV1 {
        MeasuredGuestManifestV1::new(
            MeasuredGuestSourceV1::new(
                "https://github.com/gakonst/nanocodex",
                "3b64cb6b72cd26e410916b90b0917d1290875c3c",
                digest(1),
                "8bab26f4f68e0e26f0bb7960be334d5b520ea452",
                "x86_64-unknown-linux-musl",
            )
            .unwrap(),
            MeasuredGuestArtifactsV1::new(
                artifact("nanocodex", 2),
                artifact("libkrun.so.1", 3),
                artifact("libkrunfw-sev.so.5", 4),
                artifact("vmlinuz", 5),
                artifact("initrd.img", 6),
                artifact("nanocodex-vm-guest", 7),
                MeasuredArtifactV1::new("root.ext4", 257 * 4_096, digest(8)).unwrap(),
            ),
            AuthenticatedGuestRootV1::dm_verity(
                "01234567-89ab-8def-8123-456789abcdef",
                DmVerityRootV1::new(digest(9), digest(10), 256, 256).unwrap(),
            )
            .unwrap(),
            MeasuredGuestLaunchV1::production(
                MeasuredGuestCpuV1::AmdSevSnp,
                4,
                1_024,
                "console=hvc0 root=/dev/dm-0 ro",
                "disabled",
            )
            .unwrap(),
            MeasuredWorkloadV1::new(
                vec![
                    WorkloadComponentV1::new(
                        WorkloadComponentKindV1::ModelWeights,
                        "qwen2.5-0.5b.safetensors",
                        digest(12),
                    )
                    .unwrap(),
                    WorkloadComponentV1::new(
                        WorkloadComponentKindV1::ApplicationCode,
                        "nanocodex-vm-guest",
                        digest(7),
                    )
                    .unwrap(),
                    WorkloadComponentV1::new(
                        WorkloadComponentKindV1::ContainerImage,
                        "vllm",
                        digest(11),
                    )
                    .unwrap(),
                ],
                vec!["/nanocodex-vm-guest".to_owned()],
            )
            .unwrap(),
            MeasuredGuestReferenceV1::AmdSevSnp {
                launch_measurement: sha384(13),
            },
        )
        .unwrap()
    }

    #[test]
    fn canonical_manifest_round_trips_with_stable_digest() {
        let manifest = fixture();
        let json = manifest.canonical_json().unwrap();
        let decoded = MeasuredGuestManifestV1::from_json(&json).unwrap();

        assert_eq!(decoded, manifest);
        assert_eq!(decoded.digest().unwrap(), manifest.digest().unwrap());
        assert_eq!(decoded.cpu(), MeasuredGuestCpuV1::AmdSevSnp);
    }

    #[test]
    fn rejects_unknown_fields_versions_and_noncanonical_workloads() {
        let manifest = fixture();
        let mut value = serde_json::to_value(&manifest).unwrap();
        value["unknown"] = serde_json::json!(true);
        assert!(MeasuredGuestManifestV1::from_json(&serde_json::to_vec(&value).unwrap()).is_err());

        let mut value = serde_json::to_value(&manifest).unwrap();
        value["version"] = serde_json::json!(2);
        assert!(matches!(
            MeasuredGuestManifestV1::from_json(&serde_json::to_vec(&value).unwrap()),
            Err(MeasuredGuestManifestError::Json(_))
        ));

        let mut value = serde_json::to_value(&manifest).unwrap();
        value["reference"]["unknown"] = serde_json::json!(true);
        assert!(MeasuredGuestManifestV1::from_json(&serde_json::to_vec(&value).unwrap()).is_err());

        let mut value = serde_json::to_value(&manifest).unwrap();
        value["workload"]["components"]
            .as_array_mut()
            .unwrap()
            .reverse();
        assert!(MeasuredGuestManifestV1::from_json(&serde_json::to_vec(&value).unwrap()).is_err());

        assert!(matches!(
            MeasuredGuestManifestV1::from_json(&vec![b' '; MAX_MEASURED_GUEST_MANIFEST_BYTES + 1]),
            Err(MeasuredGuestManifestError::ManifestTooLarge { .. })
        ));
    }

    #[test]
    fn rejects_nested_policy_that_bypasses_public_constructors() {
        let manifest = fixture();
        let mut invalid = Vec::new();

        let mut value = serde_json::to_value(&manifest).unwrap();
        value["authenticated_root"]["filesystem_uuid"] =
            serde_json::json!("01234567-89AB-8DEF-8123-456789ABCDEF");
        invalid.push(value);

        let mut value = serde_json::to_value(&manifest).unwrap();
        value["authenticated_root"]["dm_verity"]["data_block_bytes"] = serde_json::json!(512);
        invalid.push(value);

        let mut value = serde_json::to_value(&manifest).unwrap();
        value["authenticated_root"]["dm_verity"]["data_blocks"] = serde_json::json!(0);
        invalid.push(value);

        for field in ["debug", "migration"] {
            let mut value = serde_json::to_value(&manifest).unwrap();
            value["launch"][field] = serde_json::json!(true);
            invalid.push(value);
        }

        let mut value = serde_json::to_value(&manifest).unwrap();
        value["launch"]["kernel_command_line"] = serde_json::json!("root=/dev/dm-0\0debug");
        invalid.push(value);

        let mut value = serde_json::to_value(&manifest).unwrap();
        value["workload"]["components"][0]["name"] = serde_json::json!("../supervisor");
        invalid.push(value);

        let mut value = serde_json::to_value(&manifest).unwrap();
        value["launch"]["cpu"] = serde_json::json!("aws_nitro");
        value["reference"] = serde_json::json!({
            "cpu": "aws_nitro",
            "pcrs": { "32": "00".repeat(48) }
        });
        invalid.push(value);

        for value in invalid {
            assert!(
                MeasuredGuestManifestV1::from_json(&serde_json::to_vec(&value).unwrap()).is_err()
            );
        }
    }

    #[test]
    fn digest_changes_for_every_load_bearing_layer() {
        let original = fixture();
        let original_digest = original.digest().unwrap();
        for pointer in [
            "/source/revision",
            "/artifacts/firmware/sha256",
            "/artifacts/kernel/sha256",
            "/artifacts/initrd/sha256",
            "/artifacts/root_image/sha256",
            "/authenticated_root/dm_verity/root_hash",
            "/launch/kernel_command_line",
            "/workload/components/2/sha256",
            "/reference/launch_measurement",
        ] {
            let mut value = serde_json::to_value(&original).unwrap();
            let target = value.pointer_mut(pointer).unwrap();
            *target = if target.is_string() {
                let text = target.as_str().unwrap();
                if text.len() == 64 {
                    serde_json::json!(
                        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                    )
                } else if text.len() == 96 {
                    serde_json::json!(
                        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                    )
                } else {
                    serde_json::json!(format!("{text}-changed"))
                }
            } else {
                unreachable!()
            };
            let changed =
                MeasuredGuestManifestV1::from_json(&serde_json::to_vec(&value).unwrap()).unwrap();
            assert_ne!(changed.digest().unwrap(), original_digest, "{pointer}");
        }

        let mut value = serde_json::to_value(&original).unwrap();
        let changed_supervisor = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        value["artifacts"]["supervisor"]["sha256"] = serde_json::json!(changed_supervisor);
        value["workload"]["components"][0]["sha256"] = serde_json::json!(changed_supervisor);
        let changed =
            MeasuredGuestManifestV1::from_json(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_ne!(changed.digest().unwrap(), original_digest, "supervisor");
    }

    #[test]
    fn reference_profile_must_match_launch_profile() {
        let mut value = serde_json::to_value(fixture()).unwrap();
        value["reference"] = serde_json::json!({
            "cpu": "intel_tdx",
            "mrtd": "00".repeat(48),
            "rtmr0": "00".repeat(48),
            "rtmr1": "00".repeat(48),
            "rtmr2": "00".repeat(48),
            "rtmr3_baseline": "00".repeat(48)
        });

        assert!(MeasuredGuestManifestV1::from_json(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn attestation_parameters_use_the_canonical_manifest_commitment() {
        let manifest = fixture();
        let challenge = crate::attestation::AttestationChallenge::new(
            [0x42; 32],
            "measured-guest-fixture",
            1_800_000_000,
        )
        .unwrap();

        let parameters =
            crate::attestation::GuestAttestationParameters::from_measured_guest_manifest(
                challenge, &manifest, None,
            )
            .unwrap();

        assert_eq!(
            parameters.workload_manifest_digest(),
            manifest.digest().unwrap().as_bytes()
        );
        assert_eq!(
            parameters.cpu_profile(),
            crate::attestation::CpuAttestationProfile::AmdSevSnp
        );
        assert_eq!(
            parameters.workload_measurement(),
            crate::attestation::WorkloadMeasurement::ReportData
        );
    }
}
