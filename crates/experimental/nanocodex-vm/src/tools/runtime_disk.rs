use std::{
    fs::{self, File},
    io::{self, Read as _, Seek as _, SeekFrom, Write as _},
    path::{Component, Path, PathBuf},
    time::{Instant, UNIX_EPOCH},
};

use arcbox_ext4::{
    Formatter, Reader,
    constants::{file_mode, make_mode},
    error::{FormatError, ReadError},
    formatter::{FileTimestamps, FormatOptions},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, info_span};
use uuid::Uuid;

use crate::measured_guest::{
    AuthenticatedGuestRootV1, DmVerityRootV1, ManifestSha256, MeasuredGuestManifestError,
};

const FILESYSTEM_BYTES: u64 = 128 * 1024 * 1024;
const VERITY_BLOCK_BYTES: u64 = 4_096;
const VERITY_DATA_BLOCKS: u64 = FILESYSTEM_BYTES / VERITY_BLOCK_BYTES;
const VERITY_DIGEST_BYTES: u64 = 32;
const VERITY_HASHES_PER_BLOCK: u64 = VERITY_BLOCK_BYTES / VERITY_DIGEST_BYTES;
// 32,768 data blocks require 256 leaf blocks, two intermediate blocks, and
// one root block. The tree is stored root-first after the ext4 data region.
const VERITY_HASH_BLOCKS: u64 = 259;
const ROOT_IMAGE_BYTES: u64 = FILESYSTEM_BYTES + VERITY_HASH_BLOCKS * VERITY_BLOCK_BYTES;
const GUEST_PATH: &str = "/nanocodex-vm-guest";
const GUEST_FIRMWARE_FALLBACK_PATH: &str = "/bin/sh";
const RUNTIME_ROOT_DIRECTORIES: [&str; 11] = [
    "/bin",
    "/dev",
    "/dev/pts",
    "/dev/shm",
    "/proc",
    "/sys",
    "/sys/fs",
    "/sys/fs/cgroup",
    "/mnt",
    "/run",
    "/tmp",
];
const IDENTITY_VERSION: &[u8] = b"nanocodex-vm-guest-runtime-v5-dm-verity\0";
const RECORD_VERSION: u32 = 3;
const IMAGE_MANIFEST_VERSION: u32 = 1;
const IMAGE_MANIFEST_DOMAIN: &[u8] = b"nanocodex-vm-guest-image-manifest-v1\0";
const VERITY_SALT_DOMAIN: &[u8] = b"nanocodex-vm-guest-dm-verity-salt-v1\0";
const FILESYSTEM_LABEL: &str = "nanocodex-guest";

/// Exact identity of one file in a reproducible guest image.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuestImageFile {
    path: String,
    bytes: u64,
    sha256: String,
}

impl GuestImageFile {
    /// Returns the portable guest-relative artifact path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the exact file length.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Returns the lowercase SHA-256 digest.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// Reproducible ext4 output described by [`GuestImageManifestV1`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuestImageFilesystem {
    format: String,
    uuid: String,
    label: String,
    bytes: u64,
    sha256: String,
    read_only: bool,
}

impl GuestImageFilesystem {
    /// Returns the deterministic filesystem UUID.
    #[must_use]
    pub fn uuid(&self) -> &str {
        &self.uuid
    }

    /// Returns the exact length of the ext4 data region.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Returns the lowercase SHA-256 digest of the ext4 data region.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// Exact dm-verity layout appended to a reproducible guest filesystem.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuestImageVerity {
    format_version: u32,
    algorithm: String,
    salt: String,
    root_hash: String,
    data_block_bytes: u32,
    hash_block_bytes: u32,
    data_blocks: u64,
    hash_start_block: u64,
    hash_blocks: u64,
}

impl GuestImageVerity {
    /// Returns the dm-verity on-disk format version.
    #[must_use]
    pub const fn format_version(&self) -> u32 {
        self.format_version
    }

    /// Returns the dm-verity digest algorithm.
    #[must_use]
    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    /// Returns the lowercase deterministic salt.
    #[must_use]
    pub fn salt(&self) -> &str {
        &self.salt
    }

    /// Returns the lowercase root hash that must enter measured early boot.
    #[must_use]
    pub fn root_hash(&self) -> &str {
        &self.root_hash
    }

    /// Returns the authenticated data-block size.
    #[must_use]
    pub const fn data_block_bytes(&self) -> u32 {
        self.data_block_bytes
    }

    /// Returns the hash-tree block size.
    #[must_use]
    pub const fn hash_block_bytes(&self) -> u32 {
        self.hash_block_bytes
    }

    /// Returns the number of authenticated ext4 data blocks.
    #[must_use]
    pub const fn data_blocks(&self) -> u64 {
        self.data_blocks
    }

    /// Returns the first hash-tree block on the combined root device.
    #[must_use]
    pub const fn hash_start_block(&self) -> u64 {
        self.hash_start_block
    }

    /// Returns the exact number of appended hash-tree blocks.
    #[must_use]
    pub const fn hash_blocks(&self) -> u64 {
        self.hash_blocks
    }
}

/// Exact identity of the complete ext4-plus-dm-verity root device.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuestRootImage {
    bytes: u64,
    sha256: String,
}

impl GuestRootImage {
    /// Returns the complete root-device length.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Returns the lowercase SHA-256 digest of the complete root device.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// Strict, timestamp-free manifest for the minimal Nanocodex guest image.
///
/// This proves artifact identity only. It deliberately does not claim that a
/// VM authenticated the external root disk; a complete confidential launch
/// must include this image in a [`crate::host::MeasuredGuestManifestV1`] with
/// an enforced authenticated-root policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuestImageManifestV1 {
    version: u32,
    operating_system: String,
    architecture: String,
    supervisor: GuestImageFile,
    filesystem: GuestImageFilesystem,
    dm_verity: GuestImageVerity,
    root_image: GuestRootImage,
}

impl GuestImageManifestV1 {
    /// Returns the only schema version accepted by this build.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// Returns the target architecture recorded in the image.
    #[must_use]
    pub fn architecture(&self) -> &str {
        &self.architecture
    }

    /// Returns the exact guest supervisor identity.
    #[must_use]
    pub const fn supervisor(&self) -> &GuestImageFile {
        &self.supervisor
    }

    /// Returns the exact ext4 image identity.
    #[must_use]
    pub const fn filesystem(&self) -> &GuestImageFilesystem {
        &self.filesystem
    }

    /// Returns the authenticated-root parameters for measured early boot.
    #[must_use]
    pub const fn dm_verity(&self) -> &GuestImageVerity {
        &self.dm_verity
    }

    /// Returns the exact combined root-device identity.
    #[must_use]
    pub const fn root_image(&self) -> &GuestRootImage {
        &self.root_image
    }

    /// Converts the generated image identity into the root policy required by
    /// a confidential VM launch.
    ///
    /// # Errors
    ///
    /// Returns an error if a digest or UUID no longer satisfies the strict
    /// measured-launch schema.
    pub fn authenticated_root(
        &self,
    ) -> Result<AuthenticatedGuestRootV1, MeasuredGuestManifestError> {
        let dm_verity = DmVerityRootV1::new(
            ManifestSha256::from_hex(&self.dm_verity.root_hash)?,
            ManifestSha256::from_hex(&self.dm_verity.salt)?,
            self.dm_verity.data_blocks,
            self.dm_verity.hash_start_block,
        )?;
        AuthenticatedGuestRootV1::dm_verity(self.filesystem.uuid.clone(), dm_verity)
    }

    /// Parses a strict version-one image manifest.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown fields, unsupported versions, or invalid
    /// artifact identities.
    pub fn from_json(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    fn canonical_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        // Serialization of a struct has declaration order and contains no map,
        // timestamp, or host path. These bytes are the versioned digest input.
        serde_json::to_vec(self)
    }

    /// Returns the domain-separated SHA-256 of canonical compact JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if serde cannot encode this manifest.
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        let mut hasher = Sha256::new();
        hasher.update(IMAGE_MANIFEST_DOMAIN);
        hasher.update(self.canonical_bytes()?);
        Ok(hex::encode(hasher.finalize()))
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self.version != IMAGE_MANIFEST_VERSION
            || self.operating_system != "linux"
            || !matches!(self.architecture.as_str(), "x86_64" | "aarch64")
            || self.supervisor.path != GUEST_PATH
            || self.supervisor.bytes == 0
            || !is_sha256_digest(&self.supervisor.sha256)
            || self.filesystem.format != "ext4"
            || self.filesystem.label != FILESYSTEM_LABEL
            || self.filesystem.bytes != FILESYSTEM_BYTES
            || !self.filesystem.read_only
            || !is_sha256_digest(&self.filesystem.sha256)
            || self.dm_verity.format_version != 1
            || self.dm_verity.algorithm != "sha256"
            || self.dm_verity.data_block_bytes != VERITY_BLOCK_BYTES as u32
            || self.dm_verity.hash_block_bytes != VERITY_BLOCK_BYTES as u32
            || self.dm_verity.data_blocks != VERITY_DATA_BLOCKS
            || self.dm_verity.hash_start_block != VERITY_DATA_BLOCKS
            || self.dm_verity.hash_blocks != VERITY_HASH_BLOCKS
            || !is_sha256_digest(&self.dm_verity.salt)
            || !is_sha256_digest(&self.dm_verity.root_hash)
            || expected_verity_salt(&self.filesystem.sha256).as_deref()
                != Some(self.dm_verity.salt.as_str())
            || self.root_image.bytes != ROOT_IMAGE_BYTES
            || !is_sha256_digest(&self.root_image.sha256)
            || expected_filesystem_uuid(&self.supervisor.sha256).as_deref()
                != Some(self.filesystem.uuid.as_str())
        {
            Err("invalid or unsupported reproducible guest image manifest")
        } else {
            Ok(())
        }
    }
}

impl<'de> Deserialize<'de> for GuestImageManifestV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireManifest {
            version: u32,
            operating_system: String,
            architecture: String,
            supervisor: GuestImageFile,
            filesystem: GuestImageFilesystem,
            dm_verity: GuestImageVerity,
            root_image: GuestRootImage,
        }

        let wire = WireManifest::deserialize(deserializer)?;
        let manifest = Self {
            version: wire.version,
            operating_system: wire.operating_system,
            architecture: wire.architecture,
            supervisor: wire.supervisor,
            filesystem: wire.filesystem,
            dm_verity: wire.dm_verity,
            root_image: wire.root_image,
        };
        manifest.validate().map_err(serde::de::Error::custom)?;
        Ok(manifest)
    }
}

/// Whether preparing a guest runtime disk reused or created its cache entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestRuntimeDiskStatus {
    /// A validated content-addressed disk already existed.
    Hit,
    /// This call formatted and atomically published the disk.
    Created,
}

impl GuestRuntimeDiskStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Created => "created",
        }
    }
}

/// A content-addressed ext4 disk containing the Nanocodex VM guest runtime.
///
/// The disk remains in the caller-selected cache after this value is dropped,
/// so it can be mounted read-only by many VM attempts. Clones only copy the
/// path, digest, and preparation result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuestRuntimeDisk {
    path: PathBuf,
    digest: String,
    manifest: GuestImageManifestV1,
    manifest_digest: String,
    status: GuestRuntimeDiskStatus,
}

impl GuestRuntimeDisk {
    /// Stages a Linux guest ELF into a reusable ext4 disk.
    ///
    /// `cache` is the VM cache root, not its `runtimes` subdirectory. For
    /// example:
    ///
    /// ```no_run
    /// use nanocodex_vm::tools::{GuestRuntimeDisk, GuestRuntimeDiskStatus};
    ///
    /// # fn prepare() -> Result<(), Box<dyn std::error::Error>> {
    /// let runtime = GuestRuntimeDisk::prepare(
    ///     "target/aarch64-unknown-linux-musl/debug/nanocodex-vm-guest",
    ///     ".cache/vm",
    /// )?;
    /// assert!(matches!(
    ///     runtime.status(),
    ///     GuestRuntimeDiskStatus::Hit | GuestRuntimeDiskStatus::Created
    /// ));
    /// let read_only_ext4 = runtime.path();
    /// # let _ = read_only_ext4;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Equal binary bytes produce the same SHA-256 digest and cache path. A
    /// healthy warm call validates an atomic size/mtime record rather than
    /// rereading the binary or opening ext4. A changed source, disk, or record
    /// falls back to a complete byte-for-byte validation. Concurrent callers
    /// serialize on a per-digest filesystem lock and publish through unique
    /// temporary files. The caller-selected cache root and every managed
    /// descendant must be real directories or regular files; descendant
    /// symlinks are rejected.
    ///
    /// # Errors
    ///
    /// Returns an error if the binary cannot be read, is not an ELF, the cache
    /// cannot be accessed, or a new ext4 disk cannot be formatted or validated.
    pub fn prepare(
        binary: impl AsRef<Path>,
        cache: impl AsRef<Path>,
    ) -> Result<Self, GuestRuntimeDiskError> {
        let binary = binary.as_ref();
        let cache = cache.as_ref();
        let span = info_span!(
            target: "nanocodex_vm",
            "vm.guest_runtime.prepare",
            otel.kind = "internal",
            otel.status_code = tracing::field::Empty,
            runtime.binary.path = %binary.display(),
            runtime.binary.bytes = tracing::field::Empty,
            runtime.cache.path = %cache.display(),
            runtime.digest = tracing::field::Empty,
            runtime.cache.status = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
            status = tracing::field::Empty,
            error.message = tracing::field::Empty,
        );
        let started = Instant::now();
        let result = span.in_scope(|| Self::prepare_inner(binary, cache));
        span.record("duration_ms", started.elapsed().as_secs_f64() * 1_000.0);
        match &result {
            Ok(runtime) => {
                span.record("otel.status_code", "OK");
                span.record("status", "completed");
                span.record("runtime.digest", runtime.digest());
                span.record("runtime.cache.status", runtime.status().as_str());
                if let Ok(metadata) = fs::metadata(binary) {
                    span.record("runtime.binary.bytes", metadata.len());
                }
            }
            Err(error) => {
                span.record("otel.status_code", "ERROR");
                span.record("status", "failed");
                span.record("error.message", error.to_string());
            }
        }
        result
    }

    /// Returns the prepared ext4 disk path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the lowercase SHA-256 cache identity.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Returns the strict artifact manifest for the supervisor and ext4 image.
    #[must_use]
    pub const fn manifest(&self) -> &GuestImageManifestV1 {
        &self.manifest
    }

    /// Returns the domain-separated SHA-256 of the canonical image manifest.
    ///
    /// This is suitable as an input to a complete measured-launch manifest,
    /// but is not by itself a hardware launch measurement or authenticated-root
    /// claim.
    #[must_use]
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }

    /// Returns whether this call reused or created the disk.
    #[must_use]
    pub const fn status(&self) -> GuestRuntimeDiskStatus {
        self.status
    }

    fn prepare_inner(binary: &Path, cache: &Path) -> Result<Self, GuestRuntimeDiskError> {
        let binary =
            fs::canonicalize(binary).map_err(|source| GuestRuntimeDiskError::ReadBinary {
                path: binary.to_path_buf(),
                source,
            })?;
        let cache = RuntimeCache::open(cache)?;
        let source_snapshot = binary_snapshot(&binary)?;
        let record_path = runtime_record_path(&cache, &binary)?;
        if let Some(runtime) = recorded_runtime_disk(&record_path, source_snapshot, &cache)? {
            return Ok(runtime);
        }

        let bytes = fs::read(&binary).map_err(|source| GuestRuntimeDiskError::ReadBinary {
            path: binary.clone(),
            source,
        })?;
        if !bytes.starts_with(b"\x7fELF") {
            return Err(GuestRuntimeDiskError::NotElf(binary));
        }
        if elf_architecture(&bytes) == "unknown" {
            return Err(GuestRuntimeDiskError::UnsupportedElfArchitecture(binary));
        }
        if binary_snapshot(&binary)? != source_snapshot {
            return Err(GuestRuntimeDiskError::BinaryChanged(binary));
        }

        let digest = runtime_digest(&bytes);
        let directory = cache.directory(Path::new("runtimes").join(&digest))?;
        let path = directory.join("runtime.ext4.verity");
        if valid_cached_disk(&path, &bytes)? {
            let manifest = guest_image_manifest(&bytes, &path)?;
            let runtime = Self::from_manifest(path, digest, manifest, GuestRuntimeDiskStatus::Hit)?;
            write_runtime_record(&record_path, source_snapshot, &runtime)?;
            return Ok(runtime);
        }

        let _lock = CacheLock::acquire(&cache, &digest)?;
        if valid_cached_disk(&path, &bytes)? {
            let manifest = guest_image_manifest(&bytes, &path)?;
            let runtime = Self::from_manifest(path, digest, manifest, GuestRuntimeDiskStatus::Hit)?;
            write_runtime_record(&record_path, source_snapshot, &runtime)?;
            return Ok(runtime);
        }

        let temporary = tempfile::Builder::new()
            .prefix(".runtime.")
            .tempfile_in(&directory)
            .map_err(|source| cache_error(directory.clone(), source))?
            .into_temp_path();
        let mut contents = bytes.as_slice();
        let filesystem_uuid = filesystem_uuid(&bytes);
        let mut formatter = Formatter::with_options(
            &temporary,
            FormatOptions::new(FILESYSTEM_BYTES)
                .uuid(filesystem_uuid)
                .label(FILESYSTEM_LABEL),
        )?;
        for directory in RUNTIME_ROOT_DIRECTORIES {
            formatter.create(
                directory,
                make_mode(file_mode::S_IFDIR, 0o755),
                None,
                Some(epoch_timestamps()),
                None,
                Some(0),
                Some(0),
                None,
            )?;
        }
        formatter.create(
            GUEST_PATH,
            make_mode(file_mode::S_IFREG, 0o755),
            None,
            Some(epoch_timestamps()),
            Some(&mut contents),
            Some(0),
            Some(0),
            None,
        )?;
        let mut fallback_contents = bytes.as_slice();
        formatter.create(
            GUEST_FIRMWARE_FALLBACK_PATH,
            make_mode(file_mode::S_IFREG, 0o755),
            None,
            Some(epoch_timestamps()),
            Some(&mut fallback_contents),
            Some(0),
            Some(0),
            None,
        )?;
        formatter.close()?;
        normalize_arcbox_inode_timestamps(&temporary)
            .map_err(|source| cache_error(temporary.to_path_buf(), source))?;
        validate_prepared_filesystem(&temporary, &bytes)?;
        append_dm_verity(&temporary)
            .map_err(|source| cache_error(temporary.to_path_buf(), source))?;
        validate_prepared_disk(&temporary, &bytes)?;
        temporary
            .persist(&path)
            .map_err(|error| cache_error(path.clone(), error.error))?;
        let manifest = guest_image_manifest(&bytes, &path)?;
        let runtime = Self::from_manifest(path, digest, manifest, GuestRuntimeDiskStatus::Created)?;
        write_runtime_record(&record_path, source_snapshot, &runtime)?;

        Ok(runtime)
    }

    fn from_manifest(
        path: PathBuf,
        digest: String,
        manifest: GuestImageManifestV1,
        status: GuestRuntimeDiskStatus,
    ) -> Result<Self, GuestRuntimeDiskError> {
        let manifest_digest = manifest
            .digest()
            .map_err(|source| cache_error(path.clone(), io::Error::other(source)))?;
        Ok(Self {
            path,
            digest,
            manifest,
            manifest_digest,
            status,
        })
    }
}

/// Failure while preparing a content-addressed VM guest runtime disk.
#[derive(Debug, thiserror::Error)]
pub enum GuestRuntimeDiskError {
    /// The guest runtime binary could not be read.
    #[error("failed to read VM guest runtime binary {}", path.display())]
    ReadBinary {
        /// Binary path supplied by the caller.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: io::Error,
    },
    /// The runtime binary changed while it was being indexed.
    #[error("VM guest runtime binary {} changed while it was being indexed", .0.display())]
    BinaryChanged(PathBuf),
    /// The supplied runtime is not a Linux ELF executable.
    #[error("VM guest runtime {} is not an ELF executable", .0.display())]
    NotElf(PathBuf),
    /// The supplied ELF does not target x86-64 or AArch64.
    #[error("VM guest runtime {} targets an unsupported ELF architecture", .0.display())]
    UnsupportedElfArchitecture(PathBuf),
    /// A cache directory, lock, temporary file, or publication failed.
    #[error("failed to access VM guest runtime cache at {}", path.display())]
    Cache {
        /// Cache path being accessed.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: io::Error,
    },
    /// Formatting the ext4 disk failed.
    #[error("failed to format VM guest runtime disk")]
    Format(#[from] FormatError),
    /// A newly formatted ext4 disk did not contain the expected runtime.
    #[error("prepared VM guest runtime disk {} failed validation", path.display())]
    InvalidPreparedDisk {
        /// Temporary disk path that failed validation.
        path: PathBuf,
        /// Underlying ext4 read error, when one was available.
        #[source]
        source: Option<ReadError>,
    },
}

struct RuntimeCache {
    root: PathBuf,
}

impl RuntimeCache {
    fn open(root: &Path) -> Result<Self, GuestRuntimeDiskError> {
        fs::create_dir_all(root).map_err(|source| cache_error(root.to_path_buf(), source))?;
        ensure_cache_directory(root)?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn directory(&self, relative: impl AsRef<Path>) -> Result<PathBuf, GuestRuntimeDiskError> {
        let relative = relative.as_ref();
        let mut directory = self.root.clone();
        for component in relative.components() {
            let Component::Normal(component) = component else {
                return Err(cache_error(
                    self.root.join(relative),
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "runtime cache path contains a non-normal component",
                    ),
                ));
            };
            directory.push(component);
            match fs::create_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(source) => return Err(cache_error(directory, source)),
            }
            ensure_cache_directory(&directory)?;
        }
        Ok(directory)
    }
}

fn ensure_cache_directory(path: &Path) -> Result<(), GuestRuntimeDiskError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| cache_error(path.to_path_buf(), source))?;
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(cache_error(
            path.to_path_buf(),
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "runtime cache path component is not a directory",
            ),
        ))
    }
}

fn cache_file_metadata(path: &Path) -> Result<Option<fs::Metadata>, GuestRuntimeDiskError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(Some(metadata)),
        Ok(_) => Err(cache_error(
            path.to_path_buf(),
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "runtime cache file is not a regular file",
            ),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(cache_error(path.to_path_buf(), source)),
    }
}

fn ensure_cache_file(path: &Path) -> Result<(), GuestRuntimeDiskError> {
    cache_file_metadata(path)?.map_or_else(
        || {
            Err(cache_error(
                path.to_path_buf(),
                io::Error::new(io::ErrorKind::NotFound, "runtime cache file does not exist"),
            ))
        },
        |_| Ok(()),
    )
}

struct CacheLock(File);

impl CacheLock {
    fn acquire(cache: &RuntimeCache, digest: &str) -> Result<Self, GuestRuntimeDiskError> {
        let directory = cache.directory("locks/runtimes")?;
        let path = directory.join(format!("{digest}.lock"));
        let file = open_cache_lock(&path)?;
        fs2::FileExt::lock_exclusive(&file).map_err(|source| cache_error(path.clone(), source))?;
        Ok(Self(file))
    }
}

fn open_cache_lock(path: &Path) -> Result<File, GuestRuntimeDiskError> {
    match fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            ensure_cache_file(path)?;
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map_err(|source| cache_error(path.to_path_buf(), source))
        }
        Err(source) => Err(cache_error(path.to_path_buf(), source)),
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

fn runtime_digest(bytes: &[u8]) -> String {
    let mut identity = Sha256::new();
    identity.update(IDENTITY_VERSION);
    identity.update(bytes);
    hex::encode(identity.finalize())
}

fn filesystem_uuid(bytes: &[u8]) -> Uuid {
    let digest = Sha256::digest(bytes);
    filesystem_uuid_from_digest(&digest)
}

fn filesystem_uuid_from_digest(digest: &[u8]) -> Uuid {
    let mut uuid = [0_u8; 16];
    uuid.copy_from_slice(&digest[..16]);
    // RFC 9562 version 8 reserves the payload layout for application use.
    uuid[6] = (uuid[6] & 0x0f) | 0x80;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    Uuid::from_bytes(uuid)
}

fn expected_filesystem_uuid(supervisor_sha256: &str) -> Option<String> {
    let digest = hex::decode(supervisor_sha256).ok()?;
    (digest.len() == 32).then(|| filesystem_uuid_from_digest(&digest).to_string())
}

fn expected_verity_salt(filesystem_sha256: &str) -> Option<String> {
    let digest: [u8; 32] = hex::decode(filesystem_sha256).ok()?.try_into().ok()?;
    Some(hex::encode(verity_salt(&digest)))
}

fn verity_salt(filesystem_sha256: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(VERITY_SALT_DOMAIN);
    hasher.update(filesystem_sha256);
    hasher.finalize().into()
}

struct BuiltVerityTree {
    salt: [u8; 32],
    root_hash: [u8; 32],
    bytes: Vec<u8>,
}

fn append_dm_verity(path: &Path) -> io::Result<()> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() != FILESYSTEM_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "dm-verity input is not the exact ext4 data region",
        ));
    }
    let filesystem_sha256 = sha256_file_prefix(path, FILESYSTEM_BYTES)?;
    let tree = build_dm_verity_tree(path, verity_salt(&filesystem_sha256))?;
    if tree.bytes.len() as u64 != VERITY_HASH_BLOCKS * VERITY_BLOCK_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "dm-verity tree has an unexpected length",
        ));
    }
    let mut file = fs::OpenOptions::new().append(true).open(path)?;
    file.write_all(&tree.bytes)?;
    file.sync_all()
}

fn build_dm_verity_tree(path: &Path, salt: [u8; 32]) -> io::Result<BuiltVerityTree> {
    let mut file = File::open(path)?;
    let mut block = [0_u8; VERITY_BLOCK_BYTES as usize];
    let mut digests = Vec::with_capacity(VERITY_DATA_BLOCKS as usize);
    for _ in 0..VERITY_DATA_BLOCKS {
        file.read_exact(&mut block)?;
        digests.push(verity_hash(&salt, &block));
    }

    let mut levels = Vec::new();
    let root_hash = loop {
        let level = pack_verity_digests(&digests)?;
        let level_blocks = level.len() / VERITY_BLOCK_BYTES as usize;
        if level_blocks == 1 {
            let root_hash = verity_hash(&salt, &level);
            levels.push(level);
            break root_hash;
        }
        digests = level
            .chunks_exact(VERITY_BLOCK_BYTES as usize)
            .map(|block| verity_hash(&salt, block))
            .collect();
        levels.push(level);
    };
    let bytes = levels.into_iter().rev().flatten().collect();
    Ok(BuiltVerityTree {
        salt,
        root_hash,
        bytes,
    })
}

fn pack_verity_digests(digests: &[[u8; 32]]) -> io::Result<Vec<u8>> {
    if digests.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "dm-verity level has no digests",
        ));
    }
    let blocks = (digests.len() as u64)
        .checked_add(VERITY_HASHES_PER_BLOCK - 1)
        .ok_or_else(|| io::Error::other("dm-verity level length overflow"))?
        / VERITY_HASHES_PER_BLOCK;
    let bytes = blocks
        .checked_mul(VERITY_BLOCK_BYTES)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| io::Error::other("dm-verity level length overflow"))?;
    let mut level = vec![0_u8; bytes];
    for (index, digest) in digests.iter().enumerate() {
        let offset = index * VERITY_DIGEST_BYTES as usize;
        level[offset..offset + VERITY_DIGEST_BYTES as usize].copy_from_slice(digest);
    }
    Ok(level)
}

fn verity_hash(salt: &[u8; 32], block: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    // dm-verity format 1 prepends the salt and pads each stored digest to the
    // next power of two. SHA-256 is already 32 bytes, so no digest padding is
    // necessary before packing it into a 4 KiB hash block.
    hasher.update(salt);
    hasher.update(block);
    hasher.finalize().into()
}

const fn epoch_timestamps() -> FileTimestamps {
    FileTimestamps {
        access_lo: 0,
        access_hi: 0,
        modification_lo: 0,
        modification_hi: 0,
        creation_lo: 0,
        creation_hi: 0,
        now_lo: 0,
        now_hi: 0,
    }
}

fn guest_image_manifest(
    supervisor: &[u8],
    filesystem_path: &Path,
) -> Result<GuestImageManifestV1, GuestRuntimeDiskError> {
    let metadata = fs::metadata(filesystem_path)
        .map_err(|source| cache_error(filesystem_path.to_path_buf(), source))?;
    if !metadata.is_file() || metadata.len() != ROOT_IMAGE_BYTES {
        return Err(cache_error(
            filesystem_path.to_path_buf(),
            io::Error::new(
                io::ErrorKind::InvalidData,
                "root image does not have the canonical dm-verity layout",
            ),
        ));
    }
    let filesystem_sha256 = sha256_file_prefix(filesystem_path, FILESYSTEM_BYTES)
        .map_err(|source| cache_error(filesystem_path.to_path_buf(), source))?;
    let tree = build_dm_verity_tree(filesystem_path, verity_salt(&filesystem_sha256))
        .map_err(|source| cache_error(filesystem_path.to_path_buf(), source))?;
    if !file_region_equals(filesystem_path, FILESYSTEM_BYTES, &tree.bytes)
        .map_err(|source| cache_error(filesystem_path.to_path_buf(), source))?
    {
        return Err(cache_error(
            filesystem_path.to_path_buf(),
            io::Error::new(
                io::ErrorKind::InvalidData,
                "root image dm-verity tree does not authenticate its ext4 data",
            ),
        ));
    }
    let root_image_sha256 = sha256_file(filesystem_path)
        .map_err(|source| cache_error(filesystem_path.to_path_buf(), source))?;
    Ok(GuestImageManifestV1 {
        version: IMAGE_MANIFEST_VERSION,
        operating_system: "linux".to_owned(),
        architecture: elf_architecture(supervisor).to_owned(),
        supervisor: GuestImageFile {
            path: GUEST_PATH.to_owned(),
            bytes: supervisor.len() as u64,
            sha256: hex::encode(Sha256::digest(supervisor)),
        },
        filesystem: GuestImageFilesystem {
            format: "ext4".to_owned(),
            uuid: filesystem_uuid(supervisor).to_string(),
            label: FILESYSTEM_LABEL.to_owned(),
            bytes: FILESYSTEM_BYTES,
            sha256: hex::encode(filesystem_sha256),
            read_only: true,
        },
        dm_verity: GuestImageVerity {
            format_version: 1,
            algorithm: "sha256".to_owned(),
            salt: hex::encode(tree.salt),
            root_hash: hex::encode(tree.root_hash),
            data_block_bytes: VERITY_BLOCK_BYTES as u32,
            hash_block_bytes: VERITY_BLOCK_BYTES as u32,
            data_blocks: VERITY_DATA_BLOCKS,
            hash_start_block: VERITY_DATA_BLOCKS,
            hash_blocks: VERITY_HASH_BLOCKS,
        },
        root_image: GuestRootImage {
            bytes: metadata.len(),
            sha256: root_image_sha256,
        },
    })
}

fn elf_architecture(bytes: &[u8]) -> &'static str {
    if bytes.len() < 20 || !bytes.starts_with(b"\x7fELF") {
        return "unknown";
    }
    let machine = match bytes.get(5) {
        Some(2) => u16::from_be_bytes([bytes[18], bytes[19]]),
        _ => u16::from_le_bytes([bytes[18], bytes[19]]),
    };
    match machine {
        62 => "x86_64",
        183 => "aarch64",
        _ => "unknown",
    }
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1_024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn sha256_file_prefix(path: &Path, bytes: u64) -> io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut remaining = bytes;
    let mut buffer = [0_u8; 64 * 1_024];
    while remaining != 0 {
        let limit = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| io::Error::other("hash input length overflow"))?;
        file.read_exact(&mut buffer[..limit])?;
        hasher.update(&buffer[..limit]);
        remaining -= limit as u64;
    }
    Ok(hasher.finalize().into())
}

fn file_region_equals(path: &Path, offset: u64, expected: &[u8]) -> io::Result<bool> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = [0_u8; 64 * 1_024];
    for expected in expected.chunks(buffer.len()) {
        file.read_exact(&mut buffer[..expected.len()])?;
        if &buffer[..expected.len()] != expected {
            return Ok(false);
        }
    }
    Ok(true)
}

// arcbox-ext4 0.1.2 exposes deterministic file timestamps and UUIDs, but its
// implicit root and lost+found inodes use wall-clock timestamps. Normalize all
// allocated inode timestamp fields after formatting. This is intentionally
// limited to arcbox's checksum-free 4 KiB ext4 layout and fails closed if that
// layout changes.
fn normalize_arcbox_inode_timestamps(path: &Path) -> io::Result<()> {
    const SUPERBLOCK_OFFSET: u64 = 1_024;
    const SUPERBLOCK_BYTES: usize = 1_024;
    const EXT4_MAGIC_OFFSET: usize = 0x38;
    const LOG_BLOCK_SIZE_OFFSET: usize = 0x18;
    const INODES_COUNT_OFFSET: usize = 0x00;
    const FREE_INODES_COUNT_OFFSET: usize = 0x10;
    const INODES_PER_GROUP_OFFSET: usize = 0x28;
    const INODE_SIZE_OFFSET: usize = 0x58;
    const FEATURE_INCOMPAT_OFFSET: usize = 0x60;
    const FEATURE_RO_COMPAT_OFFSET: usize = 0x64;
    const DESCRIPTOR_SIZE_OFFSET: usize = 0xfe;
    const EXT4_FEATURE_INCOMPAT_64BIT: u32 = 0x80;
    const EXT4_FEATURE_RO_COMPAT_METADATA_CSUM: u32 = 0x400;
    const INODE_TABLE_LO_OFFSET: usize = 0x08;
    const TIMESTAMP_OFFSETS: [u64; 9] = [0x08, 0x0c, 0x10, 0x14, 0x84, 0x88, 0x8c, 0x90, 0x94];

    let mut file = fs::OpenOptions::new().read(true).write(true).open(path)?;
    let mut superblock = [0_u8; SUPERBLOCK_BYTES];
    file.seek(SeekFrom::Start(SUPERBLOCK_OFFSET))?;
    file.read_exact(&mut superblock)?;
    if read_u16(&superblock, EXT4_MAGIC_OFFSET)? != 0xef53
        || read_u32(&superblock, LOG_BLOCK_SIZE_OFFSET)? != 2
        || read_u32(&superblock, FEATURE_INCOMPAT_OFFSET)? & EXT4_FEATURE_INCOMPAT_64BIT != 0
        || read_u32(&superblock, FEATURE_RO_COMPAT_OFFSET)? & EXT4_FEATURE_RO_COMPAT_METADATA_CSUM
            != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported ext4 layout for reproducible timestamp normalization",
        ));
    }
    let block_size = 4_096_u64;
    let inode_size = u64::from(read_u16(&superblock, INODE_SIZE_OFFSET)?);
    let inodes_per_group = u64::from(read_u32(&superblock, INODES_PER_GROUP_OFFSET)?);
    let inode_count = u64::from(read_u32(&superblock, INODES_COUNT_OFFSET)?);
    let free_inode_count = u64::from(read_u32(&superblock, FREE_INODES_COUNT_OFFSET)?);
    let allocated_inodes = inode_count.checked_sub(free_inode_count).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "ext4 free inode count is invalid",
        )
    })?;
    let descriptor_size = u64::from(read_u16(&superblock, DESCRIPTOR_SIZE_OFFSET)?).max(32);
    if inode_size < 0x98 || inodes_per_group == 0 || descriptor_size < 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ext4 inode or group descriptor size is invalid",
        ));
    }

    let zero = 0_u32.to_le_bytes();
    for inode_index in 0..allocated_inodes {
        let group = inode_index / inodes_per_group;
        let group_inode = inode_index % inodes_per_group;
        let descriptor_offset = block_size
            .checked_add(group.checked_mul(descriptor_size).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "ext4 descriptor offset overflow",
                )
            })?)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "ext4 descriptor offset overflow",
                )
            })?;
        file.seek(SeekFrom::Start(
            descriptor_offset + INODE_TABLE_LO_OFFSET as u64,
        ))?;
        let mut inode_table = [0_u8; 4];
        file.read_exact(&mut inode_table)?;
        let inode_offset = u64::from(u32::from_le_bytes(inode_table))
            .checked_mul(block_size)
            .and_then(|offset| offset.checked_add(group_inode.checked_mul(inode_size)?))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "ext4 inode offset overflow")
            })?;
        for field_offset in TIMESTAMP_OFFSETS {
            file.seek(SeekFrom::Start(inode_offset + field_offset))?;
            file.write_all(&zero)?;
        }
    }
    file.flush()
}

fn read_u16(bytes: &[u8], offset: usize) -> io::Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated ext4 metadata"))?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated ext4 metadata"))?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileSnapshot {
    bytes: u64,
    modified_unix_ns: u64,
}

impl FileSnapshot {
    fn from_metadata(metadata: &fs::Metadata) -> io::Result<Self> {
        let modified = metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?;
        Ok(Self {
            bytes: metadata.len(),
            modified_unix_ns: u64::try_from(modified.as_nanos()).map_err(io::Error::other)?,
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GuestRuntimeRecord {
    version: u32,
    binary_bytes: u64,
    binary_modified_unix_ns: u64,
    digest: String,
    disk_bytes: u64,
    disk_modified_unix_ns: u64,
    manifest: GuestImageManifestV1,
    manifest_digest: String,
}

fn binary_snapshot(path: &Path) -> Result<FileSnapshot, GuestRuntimeDiskError> {
    let metadata = fs::metadata(path).map_err(|source| GuestRuntimeDiskError::ReadBinary {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(GuestRuntimeDiskError::ReadBinary {
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "runtime binary is not a file"),
        });
    }
    FileSnapshot::from_metadata(&metadata).map_err(|source| GuestRuntimeDiskError::ReadBinary {
        path: path.to_path_buf(),
        source,
    })
}

fn runtime_record_path(
    cache: &RuntimeCache,
    binary: &Path,
) -> Result<PathBuf, GuestRuntimeDiskError> {
    let mut identity = Sha256::new();
    identity.update(b"nanocodex-vm-runtime-record-v1\0");
    identity.update(binary.as_os_str().as_encoded_bytes());
    Ok(cache
        .directory("runtime-records")?
        .join(format!("{}.json", hex::encode(identity.finalize()))))
}

fn recorded_runtime_disk(
    record_path: &Path,
    source: FileSnapshot,
    cache: &RuntimeCache,
) -> Result<Option<GuestRuntimeDisk>, GuestRuntimeDiskError> {
    if cache_file_metadata(record_path)?.is_none() {
        return Ok(None);
    }
    let contents = match fs::read(record_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(cache_error(record_path.to_path_buf(), source)),
    };
    let record = match serde_json::from_slice::<GuestRuntimeRecord>(&contents) {
        Ok(record) => record,
        Err(error) => {
            info!(
                target: "nanocodex_vm",
                cache_record_path = %record_path.display(),
                error = %error,
                "ignoring invalid VM guest runtime cache record"
            );
            return Ok(None);
        }
    };
    if record.version != RECORD_VERSION
        || record.binary_bytes != source.bytes
        || record.binary_modified_unix_ns != source.modified_unix_ns
        || !is_sha256_digest(&record.digest)
        || record.disk_bytes != ROOT_IMAGE_BYTES
        || record.manifest.version != IMAGE_MANIFEST_VERSION
        || record.manifest.validate().is_err()
        || expected_filesystem_uuid(&record.manifest.supervisor.sha256).as_deref()
            != Some(record.manifest.filesystem.uuid.as_str())
        || !is_sha256_digest(&record.manifest.filesystem.sha256)
        || !is_sha256_digest(&record.manifest.supervisor.sha256)
        || !is_sha256_digest(&record.manifest_digest)
        || record.manifest.digest().ok().as_deref() != Some(record.manifest_digest.as_str())
    {
        return Ok(None);
    }

    let path = cache
        .directory(Path::new("runtimes").join(&record.digest))?
        .join("runtime.ext4.verity");
    let Some(metadata) = cache_file_metadata(&path)? else {
        return Ok(None);
    };
    let disk = FileSnapshot::from_metadata(&metadata)
        .map_err(|source| cache_error(path.clone(), source))?;
    if disk.bytes != record.disk_bytes || disk.modified_unix_ns != record.disk_modified_unix_ns {
        return Ok(None);
    }
    Ok(Some(GuestRuntimeDisk {
        path,
        digest: record.digest,
        manifest: record.manifest,
        manifest_digest: record.manifest_digest,
        status: GuestRuntimeDiskStatus::Hit,
    }))
}

fn write_runtime_record(
    record_path: &Path,
    source: FileSnapshot,
    runtime: &GuestRuntimeDisk,
) -> Result<(), GuestRuntimeDiskError> {
    let metadata = fs::metadata(runtime.path())
        .map_err(|source| cache_error(runtime.path().to_path_buf(), source))?;
    let disk = FileSnapshot::from_metadata(&metadata)
        .map_err(|source| cache_error(runtime.path().to_path_buf(), source))?;
    let record = GuestRuntimeRecord {
        version: RECORD_VERSION,
        binary_bytes: source.bytes,
        binary_modified_unix_ns: source.modified_unix_ns,
        digest: runtime.digest.clone(),
        disk_bytes: disk.bytes,
        disk_modified_unix_ns: disk.modified_unix_ns,
        manifest: runtime.manifest.clone(),
        manifest_digest: runtime.manifest_digest.clone(),
    };
    let directory = record_path
        .parent()
        .ok_or_else(|| {
            cache_error(
                record_path.to_path_buf(),
                io::Error::other("runtime cache record has no parent directory"),
            )
        })?
        .to_path_buf();
    ensure_cache_directory(&directory)?;
    let _ = cache_file_metadata(record_path)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".runtime-record.")
        .tempfile_in(&directory)
        .map_err(|source| cache_error(directory, source))?;
    serde_json::to_writer(temporary.as_file_mut(), &record)
        .map_err(|source| cache_error(record_path.to_path_buf(), io::Error::other(source)))?;
    temporary
        .into_temp_path()
        .persist(record_path)
        .map_err(|error| cache_error(record_path.to_path_buf(), error.error))?;
    Ok(())
}

fn is_sha256_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_cached_disk(path: &Path, binary: &[u8]) -> Result<bool, GuestRuntimeDiskError> {
    let Some(metadata) = cache_file_metadata(path)? else {
        return Ok(false);
    };
    if metadata.len() != ROOT_IMAGE_BYTES {
        return Ok(false);
    }
    let Ok(mut reader) = Reader::new(path) else {
        return Ok(false);
    };
    if RUNTIME_ROOT_DIRECTORIES.iter().any(|directory| {
        !reader
            .stat(directory)
            .is_ok_and(|(_, inode)| inode.is_dir())
    }) {
        return Ok(false);
    }
    for path in [GUEST_PATH, GUEST_FIRMWARE_FALLBACK_PATH] {
        let Ok((_, inode)) = reader.stat(path) else {
            return Ok(false);
        };
        if !inode.is_reg()
            || inode.file_size() != binary.len() as u64
            || inode.mode & 0o777 != 0o755
            || !reader
                .read_file(path, 0, None)
                .is_ok_and(|cached| cached == binary)
        {
            return Ok(false);
        }
    }
    valid_dm_verity(path).map_err(|source| cache_error(path.to_path_buf(), source))
}

fn valid_dm_verity(path: &Path) -> io::Result<bool> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() != ROOT_IMAGE_BYTES {
        return Ok(false);
    }
    let filesystem_sha256 = sha256_file_prefix(path, FILESYSTEM_BYTES)?;
    let tree = build_dm_verity_tree(path, verity_salt(&filesystem_sha256))?;
    file_region_equals(path, FILESYSTEM_BYTES, &tree.bytes)
}

fn validate_prepared_filesystem(path: &Path, binary: &[u8]) -> Result<(), GuestRuntimeDiskError> {
    let metadata = fs::metadata(path).map_err(|source| cache_error(path.to_path_buf(), source))?;
    if !metadata.is_file() || metadata.len() != FILESYSTEM_BYTES {
        return Err(GuestRuntimeDiskError::InvalidPreparedDisk {
            path: path.to_path_buf(),
            source: None,
        });
    }
    validate_prepared_filesystem_contents(path, binary)
}

fn validate_prepared_disk(path: &Path, binary: &[u8]) -> Result<(), GuestRuntimeDiskError> {
    let metadata = fs::metadata(path).map_err(|source| cache_error(path.to_path_buf(), source))?;
    if !metadata.is_file() || metadata.len() != ROOT_IMAGE_BYTES {
        return Err(GuestRuntimeDiskError::InvalidPreparedDisk {
            path: path.to_path_buf(),
            source: None,
        });
    }
    validate_prepared_filesystem_contents(path, binary)?;
    guest_image_manifest(binary, path).map(|_| ())
}

fn validate_prepared_filesystem_contents(
    path: &Path,
    binary: &[u8],
) -> Result<(), GuestRuntimeDiskError> {
    let mut reader =
        Reader::new(path).map_err(|source| GuestRuntimeDiskError::InvalidPreparedDisk {
            path: path.to_path_buf(),
            source: Some(source),
        })?;
    if RUNTIME_ROOT_DIRECTORIES.iter().any(|directory| {
        !reader
            .stat(directory)
            .is_ok_and(|(_, inode)| inode.is_dir())
    }) {
        return Err(GuestRuntimeDiskError::InvalidPreparedDisk {
            path: path.to_path_buf(),
            source: None,
        });
    }
    for guest_path in [GUEST_PATH, GUEST_FIRMWARE_FALLBACK_PATH] {
        let (_, inode) = reader.stat(guest_path).map_err(|source| {
            GuestRuntimeDiskError::InvalidPreparedDisk {
                path: path.to_path_buf(),
                source: Some(source),
            }
        })?;
        if !inode.is_reg()
            || inode.file_size() != binary.len() as u64
            || inode.mode & 0o777 != 0o755
        {
            return Err(GuestRuntimeDiskError::InvalidPreparedDisk {
                path: path.to_path_buf(),
                source: None,
            });
        }
        let contents = reader.read_file(guest_path, 0, None).map_err(|source| {
            GuestRuntimeDiskError::InvalidPreparedDisk {
                path: path.to_path_buf(),
                source: Some(source),
            }
        })?;
        if contents != binary {
            return Err(GuestRuntimeDiskError::InvalidPreparedDisk {
                path: path.to_path_buf(),
                source: None,
            });
        }
    }
    Ok(())
}

const fn cache_error(path: PathBuf, source: io::Error) -> GuestRuntimeDiskError {
    GuestRuntimeDiskError::Cache { path, source }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Read as _};

    use arcbox_ext4::Reader;

    use super::{
        GUEST_FIRMWARE_FALLBACK_PATH, GUEST_PATH, GuestRuntimeDisk, GuestRuntimeDiskStatus,
        ROOT_IMAGE_BYTES, RUNTIME_ROOT_DIRECTORIES, VERITY_DATA_BLOCKS, VERITY_HASH_BLOCKS,
        runtime_digest,
    };

    #[test]
    fn prepares_valid_content_addressed_disk_and_reuses_it() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("nanocodex-vm-guest");
        let bytes = elf(b"deterministic guest runtime");
        fs::write(&binary, &bytes).unwrap();
        let cache = directory.path().join("cache");

        let created = GuestRuntimeDisk::prepare(&binary, &cache).unwrap();
        assert_eq!(created.status(), GuestRuntimeDiskStatus::Created);
        assert_eq!(created.digest(), runtime_digest(&bytes));
        assert_eq!(
            created.path(),
            cache
                .join("runtimes")
                .join(created.digest())
                .join("runtime.ext4.verity")
        );
        assert_eq!(
            fs::metadata(created.path()).unwrap().len(),
            ROOT_IMAGE_BYTES
        );
        assert_eq!(created.manifest().root_image().bytes(), ROOT_IMAGE_BYTES);
        assert_eq!(
            created.manifest().dm_verity().data_blocks(),
            VERITY_DATA_BLOCKS
        );
        assert_eq!(
            created.manifest().dm_verity().hash_start_block(),
            VERITY_DATA_BLOCKS
        );
        assert_eq!(
            (ROOT_IMAGE_BYTES - created.manifest().filesystem().bytes()) / 4_096,
            VERITY_HASH_BLOCKS
        );

        let mut reader = Reader::new(created.path()).unwrap();
        for directory in RUNTIME_ROOT_DIRECTORIES {
            assert!(reader.stat(directory).unwrap().1.is_dir());
        }
        let (_, inode) = reader.stat(GUEST_PATH).unwrap();
        assert!(inode.is_reg());
        assert_eq!(inode.file_size(), bytes.len() as u64);
        assert_eq!(inode.mode & 0o777, 0o755);
        assert_eq!(reader.read_file(GUEST_PATH, 0, None).unwrap(), bytes);
        let (_, inode) = reader.stat(GUEST_FIRMWARE_FALLBACK_PATH).unwrap();
        assert!(inode.is_reg());
        assert_eq!(inode.file_size(), bytes.len() as u64);
        assert_eq!(inode.mode & 0o777, 0o755);
        assert_eq!(
            reader
                .read_file(GUEST_FIRMWARE_FALLBACK_PATH, 0, None)
                .unwrap(),
            bytes
        );

        let reused = GuestRuntimeDisk::prepare(&binary, &cache).unwrap();
        assert_eq!(reused.status(), GuestRuntimeDiskStatus::Hit);
        assert_eq!(reused.path(), created.path());
        assert_eq!(reused.digest(), created.digest());
        assert_eq!(reused.manifest(), created.manifest());
        assert_eq!(reused.manifest_digest(), created.manifest_digest());
    }

    #[test]
    fn identical_supervisors_produce_bit_identical_images_and_manifests() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let binary = first.path().join("nanocodex-vm-guest");
        let bytes = elf(b"reproducible guest runtime across independent cache roots");
        fs::write(&binary, &bytes).unwrap();

        let first_image = GuestRuntimeDisk::prepare(&binary, first.path().join("cache")).unwrap();
        let second_image = GuestRuntimeDisk::prepare(&binary, second.path().join("cache")).unwrap();

        assert_eq!(first_image.manifest(), second_image.manifest());
        assert_eq!(
            first_image.manifest_digest(),
            second_image.manifest_digest()
        );
        assert_eq!(
            first_image.manifest().filesystem().sha256(),
            second_image.manifest().filesystem().sha256()
        );
        assert_files_equal(first_image.path(), second_image.path());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_nested_runtime_directory_symlinks_without_external_writes() {
        let bytes = elf(b"nested runtime cache escape");
        let digest = runtime_digest(&bytes);
        for relative in [
            std::path::PathBuf::from("runtimes"),
            std::path::PathBuf::from("runtimes").join(&digest),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let binary = directory.path().join("nanocodex-vm-guest");
            let cache = directory.path().join("cache");
            let outside = directory.path().join("outside");
            fs::write(&binary, &bytes).unwrap();
            fs::create_dir(&cache).unwrap();
            fs::create_dir(&outside).unwrap();
            let link = cache.join(&relative);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&outside, &link).unwrap();

            let error = GuestRuntimeDisk::prepare(&binary, &cache).unwrap_err();

            assert!(error.to_string().contains(&link.display().to_string()));
            assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_lock_and_record_directory_symlinks_without_external_writes() {
        let bytes = elf(b"nested lock and record escape");
        for relative in ["runtime-records", "locks", "locks/runtimes"] {
            let directory = tempfile::tempdir().unwrap();
            let binary = directory.path().join("nanocodex-vm-guest");
            let cache = directory.path().join("cache");
            let outside = directory.path().join("outside");
            fs::write(&binary, &bytes).unwrap();
            fs::create_dir(&cache).unwrap();
            fs::create_dir(&outside).unwrap();
            let link = cache.join(relative);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&outside, &link).unwrap();

            let error = GuestRuntimeDisk::prepare(&binary, &cache).unwrap_err();

            assert!(error.to_string().contains(&link.display().to_string()));
            assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_managed_cache_file_symlinks_without_external_writes() {
        let bytes = elf(b"managed cache file escape");
        let digest = runtime_digest(&bytes);
        for managed_file in ["record", "runtime", "lock"] {
            let directory = tempfile::tempdir().unwrap();
            let binary = directory.path().join("nanocodex-vm-guest");
            let cache = directory.path().join("cache");
            let outside = directory.path().join("outside-file");
            fs::write(&binary, &bytes).unwrap();
            fs::write(&outside, b"outside sentinel").unwrap();
            let path = match managed_file {
                "record" => {
                    let cache = super::RuntimeCache::open(&cache).unwrap();
                    let binary = fs::canonicalize(&binary).unwrap();
                    super::runtime_record_path(&cache, &binary).unwrap()
                }
                "runtime" => {
                    let parent = cache.join("runtimes").join(&digest);
                    fs::create_dir_all(&parent).unwrap();
                    parent.join("runtime.ext4.verity")
                }
                "lock" => {
                    let parent = cache.join("locks/runtimes");
                    fs::create_dir_all(&parent).unwrap();
                    parent.join(format!("{digest}.lock"))
                }
                _ => unreachable!(),
            };
            std::os::unix::fs::symlink(&outside, &path).unwrap();

            let error = GuestRuntimeDisk::prepare(&binary, &cache).unwrap_err();

            assert!(error.to_string().contains(&path.display().to_string()));
            assert_eq!(fs::read(&outside).unwrap(), b"outside sentinel");
        }
    }

    #[test]
    fn repairs_an_invalid_cache_entry_under_the_same_identity() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("nanocodex-vm-guest");
        let bytes = elf(b"repaired guest runtime");
        fs::write(&binary, &bytes).unwrap();
        let cache = directory.path().join("cache");
        let first = GuestRuntimeDisk::prepare(&binary, &cache).unwrap();
        fs::write(first.path(), b"corrupt").unwrap();

        let repaired = GuestRuntimeDisk::prepare(&binary, &cache).unwrap();

        assert_eq!(repaired.status(), GuestRuntimeDiskStatus::Created);
        let mut reader = Reader::new(repaired.path()).unwrap();
        assert_eq!(reader.read_file(GUEST_PATH, 0, None).unwrap(), bytes);
    }

    #[test]
    fn repairs_same_sized_runtime_content_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("nanocodex-vm-guest");
        let bytes = elf(b"original guest runtime");
        fs::write(&binary, &bytes).unwrap();
        let cache = directory.path().join("cache");
        let first = GuestRuntimeDisk::prepare(&binary, &cache).unwrap();

        let mut replacement = bytes.to_vec();
        let last = replacement.last_mut().unwrap();
        *last ^= 1;
        let mut formatter =
            arcbox_ext4::Formatter::new(first.path(), 4_096, 128 * 1024 * 1024).unwrap();
        let mut replacement = replacement.as_slice();
        formatter
            .create(
                GUEST_PATH,
                arcbox_ext4::constants::make_mode(
                    arcbox_ext4::constants::file_mode::S_IFREG,
                    0o755,
                ),
                None,
                None,
                Some(&mut replacement),
                Some(0),
                Some(0),
                None,
            )
            .unwrap();
        formatter.close().unwrap();

        let repaired = GuestRuntimeDisk::prepare(&binary, &cache).unwrap();

        assert_eq!(repaired.status(), GuestRuntimeDiskStatus::Created);
        let mut reader = Reader::new(repaired.path()).unwrap();
        assert_eq!(reader.read_file(GUEST_PATH, 0, None).unwrap(), bytes);
    }

    #[test]
    fn repairs_same_sized_dm_verity_tree_corruption() {
        use std::io::{Seek as _, SeekFrom, Write as _};

        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("nanocodex-vm-guest");
        let bytes = elf(b"authenticated root tree repair");
        fs::write(&binary, &bytes).unwrap();
        let cache = directory.path().join("cache");
        let first = GuestRuntimeDisk::prepare(&binary, &cache).unwrap();
        let original_manifest = first.manifest().clone();
        let mut image = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(first.path())
            .unwrap();
        image.seek(SeekFrom::End(-1)).unwrap();
        image.write_all(&[1]).unwrap();
        image.flush().unwrap();

        let repaired = GuestRuntimeDisk::prepare(&binary, &cache).unwrap();

        assert_eq!(repaired.status(), GuestRuntimeDiskStatus::Created);
        assert_eq!(repaired.manifest(), &original_manifest);
    }

    fn assert_files_equal(first: &std::path::Path, second: &std::path::Path) {
        let mut first = fs::File::open(first).unwrap();
        let mut second = fs::File::open(second).unwrap();
        let mut first_buffer = [0_u8; 64 * 1024];
        let mut second_buffer = [0_u8; 64 * 1024];
        loop {
            let first_read = first.read(&mut first_buffer).unwrap();
            let second_read = second.read(&mut second_buffer).unwrap();
            assert_eq!(first_read, second_read);
            assert_eq!(&first_buffer[..first_read], &second_buffer[..second_read]);
            if first_read == 0 {
                break;
            }
        }
    }

    fn elf(payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0_u8; 64];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }
}
