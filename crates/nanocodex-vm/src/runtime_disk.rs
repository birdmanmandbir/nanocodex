use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    time::{Instant, UNIX_EPOCH},
};

use arcbox_ext4::{
    Formatter, Reader,
    constants::{file_mode, make_mode},
    error::{FormatError, ReadError},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, info_span};

const BLOCK_SIZE: u32 = 4_096;
const DISK_BYTES: u64 = 128 * 1024 * 1024;
const GUEST_PATH: &str = "/nanocodex-vm-guest";
const IDENTITY_VERSION: &[u8] = b"nanoeval-vm-guest-runtime-v2\0";
const RECORD_VERSION: u32 = 1;

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
    status: GuestRuntimeDiskStatus,
}

impl GuestRuntimeDisk {
    /// Stages a Linux guest ELF into a reusable ext4 disk.
    ///
    /// `cache` is the VM cache root, not its `runtimes` subdirectory. For
    /// example:
    ///
    /// ```no_run
    /// use nanocodex_vm::{GuestRuntimeDisk, GuestRuntimeDiskStatus};
    ///
    /// # fn prepare() -> Result<(), Box<dyn std::error::Error>> {
    /// let runtime = GuestRuntimeDisk::prepare(
    ///     "target/aarch64-unknown-linux-musl/release/nanocodex-vm-guest",
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
    /// Equal binary bytes produce the same SHA-256 digest and cache path,
    /// including caches created by Nanoeval's `v2` runtime staging. A healthy
    /// warm call validates an atomic size/mtime record rather than rereading the
    /// binary or opening ext4. A changed source, disk, or record falls back to a
    /// complete byte-for-byte validation. Concurrent callers serialize on a
    /// per-digest filesystem lock and publish through unique temporary files.
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
        let source_snapshot = binary_snapshot(&binary)?;
        let record_path = runtime_record_path(cache, &binary);
        if let Some((digest, path)) = recorded_runtime_disk(&record_path, source_snapshot, cache)? {
            return Ok(Self {
                path,
                digest,
                status: GuestRuntimeDiskStatus::Hit,
            });
        }

        let bytes = fs::read(&binary).map_err(|source| GuestRuntimeDiskError::ReadBinary {
            path: binary.clone(),
            source,
        })?;
        if !bytes.starts_with(b"\x7fELF") {
            return Err(GuestRuntimeDiskError::NotElf(binary));
        }
        if binary_snapshot(&binary)? != source_snapshot {
            return Err(GuestRuntimeDiskError::BinaryChanged(binary));
        }

        let digest = runtime_digest(&bytes);
        let directory = cache.join("runtimes").join(&digest);
        let path = directory.join("runtime.ext4");
        if valid_cached_disk(&path, &bytes)? {
            write_runtime_record(&record_path, source_snapshot, &digest, &path)?;
            return Ok(Self {
                path,
                digest,
                status: GuestRuntimeDiskStatus::Hit,
            });
        }

        fs::create_dir_all(&directory).map_err(|source| cache_error(directory.clone(), source))?;
        let _lock = CacheLock::acquire(cache, &digest)?;
        if valid_cached_disk(&path, &bytes)? {
            write_runtime_record(&record_path, source_snapshot, &digest, &path)?;
            return Ok(Self {
                path,
                digest,
                status: GuestRuntimeDiskStatus::Hit,
            });
        }

        let temporary = tempfile::Builder::new()
            .prefix(".runtime.")
            .tempfile_in(&directory)
            .map_err(|source| cache_error(directory.clone(), source))?
            .into_temp_path();
        let mut contents = bytes.as_slice();
        let mut formatter = Formatter::new(&temporary, BLOCK_SIZE, DISK_BYTES)?;
        formatter.create(
            GUEST_PATH,
            make_mode(file_mode::S_IFREG, 0o755),
            None,
            None,
            Some(&mut contents),
            Some(0),
            Some(0),
            None,
        )?;
        formatter.close()?;
        validate_prepared_disk(&temporary, &bytes)?;
        temporary
            .persist(&path)
            .map_err(|error| cache_error(path.clone(), error.error))?;
        write_runtime_record(&record_path, source_snapshot, &digest, &path)?;

        Ok(Self {
            path,
            digest,
            status: GuestRuntimeDiskStatus::Created,
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

struct CacheLock(File);

impl CacheLock {
    fn acquire(cache: &Path, digest: &str) -> Result<Self, GuestRuntimeDiskError> {
        let directory = cache.join("locks").join("runtimes");
        fs::create_dir_all(&directory).map_err(|source| cache_error(directory.clone(), source))?;
        let path = directory.join(format!("{digest}.lock"));
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|source| cache_error(path.clone(), source))?;
        fs2::FileExt::lock_exclusive(&file).map_err(|source| cache_error(path.clone(), source))?;
        Ok(Self(file))
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
    format!("{:x}", identity.finalize())
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

fn runtime_record_path(cache: &Path, binary: &Path) -> PathBuf {
    let mut identity = Sha256::new();
    identity.update(b"nanocodex-vm-runtime-record-v1\0");
    identity.update(binary.as_os_str().as_encoded_bytes());
    cache
        .join("runtime-records")
        .join(format!("{:x}.json", identity.finalize()))
}

fn recorded_runtime_disk(
    record_path: &Path,
    source: FileSnapshot,
    cache: &Path,
) -> Result<Option<(String, PathBuf)>, GuestRuntimeDiskError> {
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
        || record.disk_bytes != DISK_BYTES
    {
        return Ok(None);
    }

    let path = cache
        .join("runtimes")
        .join(&record.digest)
        .join("runtime.ext4");
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(cache_error(path, source)),
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    let disk = FileSnapshot::from_metadata(&metadata)
        .map_err(|source| cache_error(path.clone(), source))?;
    if disk.bytes != record.disk_bytes || disk.modified_unix_ns != record.disk_modified_unix_ns {
        return Ok(None);
    }
    Ok(Some((record.digest, path)))
}

fn write_runtime_record(
    record_path: &Path,
    source: FileSnapshot,
    digest: &str,
    disk_path: &Path,
) -> Result<(), GuestRuntimeDiskError> {
    let metadata =
        fs::metadata(disk_path).map_err(|source| cache_error(disk_path.to_path_buf(), source))?;
    let disk = FileSnapshot::from_metadata(&metadata)
        .map_err(|source| cache_error(disk_path.to_path_buf(), source))?;
    let record = GuestRuntimeRecord {
        version: RECORD_VERSION,
        binary_bytes: source.bytes,
        binary_modified_unix_ns: source.modified_unix_ns,
        digest: digest.to_owned(),
        disk_bytes: disk.bytes,
        disk_modified_unix_ns: disk.modified_unix_ns,
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
    fs::create_dir_all(&directory).map_err(|source| cache_error(directory.clone(), source))?;
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
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => return Err(cache_error(path.to_path_buf(), source)),
    };
    if !metadata.is_file() || metadata.len() != DISK_BYTES {
        return Ok(false);
    }
    let Ok(mut reader) = Reader::new(path) else {
        return Ok(false);
    };
    let Ok((_, inode)) = reader.stat(GUEST_PATH) else {
        return Ok(false);
    };
    if !inode.is_reg() || inode.file_size() != binary.len() as u64 || inode.mode & 0o777 != 0o755 {
        return Ok(false);
    }
    Ok(reader
        .read_file(GUEST_PATH, 0, None)
        .is_ok_and(|cached| cached == binary))
}

fn validate_prepared_disk(path: &Path, binary: &[u8]) -> Result<(), GuestRuntimeDiskError> {
    let metadata = fs::metadata(path).map_err(|source| cache_error(path.to_path_buf(), source))?;
    if !metadata.is_file() || metadata.len() != DISK_BYTES {
        return Err(GuestRuntimeDiskError::InvalidPreparedDisk {
            path: path.to_path_buf(),
            source: None,
        });
    }
    let mut reader =
        Reader::new(path).map_err(|source| GuestRuntimeDiskError::InvalidPreparedDisk {
            path: path.to_path_buf(),
            source: Some(source),
        })?;
    let (_, inode) =
        reader
            .stat(GUEST_PATH)
            .map_err(|source| GuestRuntimeDiskError::InvalidPreparedDisk {
                path: path.to_path_buf(),
                source: Some(source),
            })?;
    if !inode.is_reg() || inode.file_size() != binary.len() as u64 || inode.mode & 0o777 != 0o755 {
        return Err(GuestRuntimeDiskError::InvalidPreparedDisk {
            path: path.to_path_buf(),
            source: None,
        });
    }
    let contents = reader.read_file(GUEST_PATH, 0, None).map_err(|source| {
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
    Ok(())
}

fn cache_error(path: PathBuf, source: io::Error) -> GuestRuntimeDiskError {
    GuestRuntimeDiskError::Cache { path, source }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use arcbox_ext4::Reader;

    use super::{GUEST_PATH, GuestRuntimeDisk, GuestRuntimeDiskStatus, runtime_digest};

    #[test]
    fn prepares_valid_content_addressed_disk_and_reuses_it() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("nanocodex-vm-guest");
        let bytes = b"\x7fELF deterministic guest runtime";
        fs::write(&binary, bytes).unwrap();
        let cache = directory.path().join("cache");

        let created = GuestRuntimeDisk::prepare(&binary, &cache).unwrap();
        assert_eq!(created.status(), GuestRuntimeDiskStatus::Created);
        assert_eq!(created.digest(), runtime_digest(bytes));
        assert_eq!(
            created.path(),
            cache
                .join("runtimes")
                .join(created.digest())
                .join("runtime.ext4")
        );

        let mut reader = Reader::new(created.path()).unwrap();
        let (_, inode) = reader.stat(GUEST_PATH).unwrap();
        assert!(inode.is_reg());
        assert_eq!(inode.file_size(), bytes.len() as u64);
        assert_eq!(inode.mode & 0o777, 0o755);
        assert_eq!(reader.read_file(GUEST_PATH, 0, None).unwrap(), bytes);

        let reused = GuestRuntimeDisk::prepare(&binary, &cache).unwrap();
        assert_eq!(reused.status(), GuestRuntimeDiskStatus::Hit);
        assert_eq!(reused.path(), created.path());
        assert_eq!(reused.digest(), created.digest());
    }

    #[test]
    fn repairs_an_invalid_cache_entry_under_the_same_identity() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("nanocodex-vm-guest");
        let bytes = b"\x7fELF repaired guest runtime";
        fs::write(&binary, bytes).unwrap();
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
        let bytes = b"\x7fELF original guest runtime";
        fs::write(&binary, bytes).unwrap();
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
}
