use std::{
    fs::{self, File},
    io::{ErrorKind, Write},
    ops::Deref,
    path::{Path, PathBuf},
};

#[cfg(not(unix))]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::{
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use eyre::{Context, Result, bail, eyre};
use fs2::FileExt as _;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

const STORE_LOCK_FILE: &str = ".update.lock";
const CHECKSUM_FILE: &str = "nanocodex.sha256";
const VM_GUEST_BINARY_NAME: &str = "nanocodex-vm-guest";
const VM_GUEST_CHECKSUM_FILE: &str = "nanocodex-vm-guest.sha256";
const CHECKSUM_BYTES: usize = 65;
#[cfg(unix)]
const MAX_CACHED_BINARY_BYTES: u64 = 256 * 1024 * 1024;
#[cfg(unix)]
static NEXT_BRIDGE_TEMPORARY: AtomicU64 = AtomicU64::new(0);

#[cfg(windows)]
const BINARY_NAME: &str = "nanocodex.exe";
#[cfg(not(windows))]
const BINARY_NAME: &str = "nanocodex";

pub(super) struct VersionStore {
    root: PathBuf,
    #[cfg(unix)]
    pinned: Option<Arc<PinnedStoreDirectories>>,
}

pub(super) struct LockedVersionStore {
    store: VersionStore,
    _lock: File,
}

impl Deref for LockedVersionStore {
    type Target = VersionStore;

    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

#[cfg(unix)]
struct PinnedStoreDirectories {
    root_path: PathBuf,
    root: File,
    root_device: u64,
    root_inode: u64,
    lock_identity: Option<(u64, u64)>,
    versions_path: PathBuf,
    versions: File,
    versions_device: u64,
    versions_inode: u64,
}

#[cfg(unix)]
struct PinnedVersionDirectory {
    path: PathBuf,
    key: String,
    store: Arc<PinnedStoreDirectories>,
    directory: File,
    device: u64,
    inode: u64,
}

#[cfg(unix)]
enum VersionDirectoryState {
    Missing,
    Invalid,
    Pinned(PinnedVersionDirectory),
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryState {
    Missing,
    Exact,
    Mismatch,
    Invalid,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BridgeMutationPoint {
    GuestWrite,
    GuestCommit,
    ChecksumWrite,
    ChecksumCommit,
    Activation,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CoherentInstallMutationPoint {
    BeforePublish,
}

#[cfg(unix)]
enum ExclusiveEntryWriteError {
    Open(rustix::io::Errno),
    AfterCreate(eyre::Report),
}

impl VersionStore {
    pub(super) fn discover() -> Result<Self> {
        let root = if let Some(root) = std::env::var_os("NANOCODEX_DIR") {
            PathBuf::from(root)
        } else {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .ok_or_else(|| eyre!("HOME is not set; set NANOCODEX_DIR explicitly"))?;
            PathBuf::from(home).join(".nanocodex")
        };
        if root.as_os_str().is_empty() {
            bail!("NANOCODEX_DIR cannot be empty");
        }
        Ok(Self {
            root,
            #[cfg(unix)]
            pinned: None,
        })
    }

    #[cfg(test)]
    pub(super) fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            #[cfg(unix)]
            pinned: None,
        }
    }

    pub(super) fn lock_exclusive(&self) -> Result<LockedVersionStore> {
        #[cfg(unix)]
        let (root, lock) = self.open_lock_file()?;
        #[cfg(not(unix))]
        let lock = self.open_lock_file()?;
        lock.lock_exclusive()
            .wrap_err("failed to lock the Nanocodex version store")?;
        #[cfg(unix)]
        {
            self.require_lock_identity(&root, &lock)?;
            let pinned = Arc::new(PinnedStoreDirectories::open(
                self.root.clone(),
                root,
                Some(&lock),
            )?);
            pinned.require_identity()?;
            Ok(LockedVersionStore {
                store: Self {
                    root: self.root.clone(),
                    pinned: Some(pinned),
                },
                _lock: lock,
            })
        }

        #[cfg(not(unix))]
        Ok(LockedVersionStore {
            store: Self {
                root: self.root.clone(),
            },
            _lock: lock,
        })
    }

    #[cfg(unix)]
    fn require_lock_identity(&self, root: &File, lock: &File) -> Result<()> {
        use rustix::fs::{AtFlags, FileType, statat};
        use std::os::unix::fs::MetadataExt as _;

        let descriptor = lock
            .metadata()
            .wrap_err("failed to inspect the locked Nanocodex version store")?;
        let named = statat(root, STORE_LOCK_FILE, AtFlags::SYMLINK_NOFOLLOW)
            .wrap_err("failed to recheck the Nanocodex version-store lock")?;
        if !descriptor.file_type().is_file()
            || !FileType::from_raw_mode(named.st_mode).is_file()
            || descriptor.nlink() != 1
            || named.st_nlink != 1
            || descriptor.dev() != named.st_dev as u64
            || descriptor.ino() != named.st_ino as u64
        {
            bail!("the Nanocodex version-store lock changed while it was acquired");
        }
        Ok(())
    }

    #[cfg(unix)]
    fn open_lock_file(&self) -> Result<(File, File)> {
        use rustix::fs::{Mode, OFlags, fstat, openat};

        let root = pin_root_directory(&self.root)?;
        let descriptor = openat(
            &root,
            STORE_LOCK_FILE,
            OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .wrap_err("failed to open the Nanocodex version-store lock")?;
        let metadata =
            fstat(&descriptor).wrap_err("failed to inspect the Nanocodex version-store lock")?;
        if !rustix::fs::FileType::from_raw_mode(metadata.st_mode).is_file() {
            bail!(
                "{} is not a regular version-store lock file",
                self.root.join(STORE_LOCK_FILE).display()
            );
        }
        Ok((root, File::from(descriptor)))
    }

    #[cfg(not(unix))]
    fn open_lock_file(&self) -> Result<File> {
        fs::create_dir_all(&self.root).wrap_err("failed to create the Nanocodex version store")?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(self.root.join(STORE_LOCK_FILE))
            .wrap_err("failed to open the Nanocodex version-store lock")
    }

    #[cfg(unix)]
    fn pinned_directories(&self) -> Result<Arc<PinnedStoreDirectories>> {
        if let Some(pinned) = &self.pinned {
            pinned.require_identity()?;
            return Ok(Arc::clone(pinned));
        }
        let root = pin_root_directory(&self.root)?;
        let pinned = Arc::new(PinnedStoreDirectories::open(self.root.clone(), root, None)?);
        pinned.require_identity()?;
        Ok(pinned)
    }

    #[cfg(unix)]
    fn require_store_identity(&self) -> Result<()> {
        if let Some(pinned) = &self.pinned {
            pinned.require_identity()?;
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn require_store_identity(&self) -> Result<()> {
        Ok(())
    }

    pub(super) fn prepare(
        &self,
        manager_version: &str,
        defer_manager_activation: bool,
    ) -> Result<()> {
        let executable = std::env::current_exe()
            .wrap_err("failed to locate the running Nanocodex executable")?;
        let contents = fs::read(&executable)
            .wrap_err_with(|| format!("failed to read {}", executable.display()))?;
        self.prepare_with_contents(manager_version, &contents, defer_manager_activation)?;
        self.seed_running_updater_checksum(&executable, &contents)
    }

    fn prepare_with_contents(
        &self,
        manager_version: &str,
        contents: &[u8],
        defer_manager_activation: bool,
    ) -> Result<()> {
        validate_key(manager_version)?;
        #[cfg(unix)]
        self.pinned_directories()?;
        #[cfg(not(unix))]
        fs::create_dir_all(self.versions_dir())
            .wrap_err("failed to create the Nanocodex version store")?;
        fs::create_dir_all(self.root.join("updater"))
            .wrap_err("failed to create the Nanocodex updater directory")?;
        self.require_store_identity()?;
        fs::create_dir_all(self.root.join("bin"))
            .wrap_err("failed to create the Nanocodex bin directory")?;
        self.require_store_identity()?;

        let active = self.active()?;
        let updater_exists = self.updater_path().is_file();
        if (!updater_exists || active.is_none()) && !self.is_cached(manager_version)? {
            self.install(manager_version, contents)?;
        }
        if !updater_exists {
            atomic_write(&self.updater_path(), contents, true)?;
            self.write_updater_checksum(contents)?;
        }
        if active.is_none() && !defer_manager_activation {
            self.activate(manager_version)?;
        }

        #[cfg(unix)]
        self.install_launcher()?;

        Ok(())
    }

    pub(super) fn is_cached(&self, key: &str) -> Result<bool> {
        validate_key(key)?;
        #[cfg(unix)]
        {
            let VersionDirectoryState::Pinned(directory) = self.inspect_version_directory(key)?
            else {
                return Ok(false);
            };
            directory.locally_checksummed_entry(BINARY_NAME, CHECKSUM_FILE)
        }

        #[cfg(not(unix))]
        file_matches_checksum(&self.binary_path(key), &self.checksum_path(key))
    }

    pub(super) fn install(&self, key: &str, contents: &[u8]) -> Result<()> {
        validate_key(key)?;

        #[cfg(unix)]
        return self.install_version_directory(key, contents, None, false);

        #[cfg(not(unix))]
        {
            let directory = self.version_dir(key);
            fs::create_dir_all(&directory)
                .wrap_err_with(|| format!("failed to create {}", directory.display()))?;
            atomic_write(&self.binary_path(key), contents, true)?;
            let checksum = hex::encode(Sha256::digest(contents));
            atomic_write(
                &self.checksum_path(key),
                format!("{checksum}\n").as_bytes(),
                false,
            )
        }
    }

    pub(super) fn reinstall(&self, key: &str, contents: &[u8]) -> Result<()> {
        validate_key(key)?;

        #[cfg(unix)]
        return self.install_version_directory(key, contents, None, true);

        #[cfg(not(unix))]
        self.install(key, contents)
    }

    pub(super) fn install_with_vm_guest(
        &self,
        key: &str,
        binary: &[u8],
        vm_guest: &[u8],
    ) -> Result<()> {
        validate_key(key)?;

        #[cfg(unix)]
        return self.install_version_directory(key, binary, Some(vm_guest), false);

        #[cfg(not(unix))]
        {
            fs::create_dir_all(self.versions_dir())
                .wrap_err("failed to create the Nanocodex version store")?;
            if self.is_cached_with_vm_guest(key)? {
                return Ok(());
            }

            let directory = self.version_dir(key);
            match fs::symlink_metadata(&directory) {
                Ok(_) => {
                    bail!(
                        "cannot coherently replace incomplete Nanocodex version {}; remove {} and retry",
                        key,
                        directory.display()
                    );
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .wrap_err_with(|| format!("failed to inspect {}", directory.display()));
                }
            }

            let staging = tempfile::Builder::new()
                .prefix(".install-")
                .tempdir_in(self.versions_dir())
                .wrap_err("failed to stage the Nanocodex version")?;
            atomic_write(&staging.path().join(BINARY_NAME), binary, true)?;
            atomic_write(
                &staging.path().join(CHECKSUM_FILE),
                format!("{}\n", hex::encode(Sha256::digest(binary))).as_bytes(),
                false,
            )?;
            atomic_write(&staging.path().join(VM_GUEST_BINARY_NAME), vm_guest, true)?;
            atomic_write(
                &staging.path().join(VM_GUEST_CHECKSUM_FILE),
                format!("{}\n", hex::encode(Sha256::digest(vm_guest))).as_bytes(),
                false,
            )?;
            fs::rename(staging.path(), &directory)
                .wrap_err_with(|| format!("failed to install {}", directory.display()))?;
            Ok(())
        }
    }

    pub(super) fn reinstall_with_vm_guest(
        &self,
        key: &str,
        binary: &[u8],
        vm_guest: &[u8],
    ) -> Result<()> {
        validate_key(key)?;

        #[cfg(unix)]
        return self.install_version_directory(key, binary, Some(vm_guest), true);

        #[cfg(not(unix))]
        self.install_with_vm_guest(key, binary, vm_guest)
    }

    #[cfg(unix)]
    fn install_version_directory(
        &self,
        key: &str,
        binary: &[u8],
        vm_guest: Option<&[u8]>,
        replace_existing: bool,
    ) -> Result<()> {
        self.install_version_directory_inner(key, binary, vm_guest, replace_existing, |_| Ok(()))
    }

    #[cfg(unix)]
    fn install_version_directory_inner(
        &self,
        key: &str,
        binary: &[u8],
        vm_guest: Option<&[u8]>,
        replace_existing: bool,
        mut hook: impl FnMut(CoherentInstallMutationPoint) -> Result<()>,
    ) -> Result<()> {
        use rustix::fs::fsync;

        let binary_sha256 = hex::encode(Sha256::digest(binary));
        let vm_guest_sha256 = vm_guest.map(|contents| hex::encode(Sha256::digest(contents)));
        let existing = match self.inspect_version_directory(key)? {
            VersionDirectoryState::Missing => None,
            VersionDirectoryState::Invalid => {
                bail!(
                    "cannot coherently install Nanocodex version {key}: the version path is not a regular directory"
                );
            }
            VersionDirectoryState::Pinned(directory) => {
                if !replace_existing {
                    let exact = if let Some(vm_guest_sha256) = &vm_guest_sha256 {
                        directory.matches_complete_bridge(&binary_sha256, vm_guest_sha256)?
                    } else {
                        directory.matches_exact_cli(&binary_sha256)?
                    };
                    if exact {
                        return Ok(());
                    }
                }
                if !replace_existing
                    || !directory.locally_checksummed_entry(BINARY_NAME, CHECKSUM_FILE)?
                    || (vm_guest.is_some()
                        && !directory.locally_checksummed_entry(
                            VM_GUEST_BINARY_NAME,
                            VM_GUEST_CHECKSUM_FILE,
                        )?)
                {
                    bail!(
                        "cannot coherently replace incomplete Nanocodex version {key}; remove {} and retry",
                        directory.path.display()
                    );
                }
                Some(directory)
            }
        };

        let pinned = self.pinned_directories()?;
        let staging = create_staged_version_directory(Arc::clone(&pinned))?;
        write_staged_version_entry(&staging.directory, BINARY_NAME, binary, true)?;
        write_staged_version_entry(
            &staging.directory,
            CHECKSUM_FILE,
            format!("{binary_sha256}\n").as_bytes(),
            false,
        )?;
        if let (Some(vm_guest), Some(vm_guest_sha256)) = (vm_guest, &vm_guest_sha256) {
            write_staged_version_entry(&staging.directory, VM_GUEST_BINARY_NAME, vm_guest, true)?;
            write_staged_version_entry(
                &staging.directory,
                VM_GUEST_CHECKSUM_FILE,
                format!("{vm_guest_sha256}\n").as_bytes(),
                false,
            )?;
        }
        staging
            .directory
            .sync_all()
            .wrap_err("failed to sync the staged Nanocodex version")?;

        hook(CoherentInstallMutationPoint::BeforePublish)?;
        pinned.require_identity()?;
        if let Some(existing) = &existing
            && (!existing.locally_checksummed_entry(BINARY_NAME, CHECKSUM_FILE)?
                || (vm_guest.is_some()
                    && !existing
                        .locally_checksummed_entry(VM_GUEST_BINARY_NAME, VM_GUEST_CHECKSUM_FILE)?))
        {
            bail!("Nanocodex version {key} changed before it could be replaced");
        }
        let staged_exact = if let Some(vm_guest_sha256) = &vm_guest_sha256 {
            staging.matches_complete_bridge(&binary_sha256, vm_guest_sha256)?
        } else {
            staging.matches_exact_cli(&binary_sha256)?
        };
        if !staged_exact {
            bail!("the staged Nanocodex version changed before publication");
        }
        if existing.is_some() {
            rename_exchange_at(&pinned.versions, &staging.key, key)
                .wrap_err_with(|| format!("failed to replace Nanocodex version {key}"))?;
        } else {
            rename_noreplace_at(&pinned.versions, &staging.key, key)
                .wrap_err_with(|| format!("failed to install Nanocodex version {key}"))?;
        }
        fsync(&pinned.versions).wrap_err("failed to sync the Nanocodex versions directory")?;
        if let Some(existing) = existing {
            cleanup_exchanged_version_directory(&pinned.versions, &staging.key, &existing)?;
        }
        Ok(())
    }

    pub(super) fn is_bridge_cached_with_vm_guest(
        &self,
        key: &str,
        binary_sha256: &str,
        vm_guest_sha256: &str,
    ) -> Result<bool> {
        validate_key(key)?;
        validate_sha256(binary_sha256)?;
        validate_sha256(vm_guest_sha256)?;

        #[cfg(unix)]
        {
            let VersionDirectoryState::Pinned(directory) = self.inspect_version_directory(key)?
            else {
                return Ok(false);
            };
            directory.matches_complete_bridge(binary_sha256, vm_guest_sha256)
        }

        #[cfg(not(unix))]
        strict_bridge_paths_match(&self.version_dir(key), binary_sha256, vm_guest_sha256)
    }

    pub(super) fn install_bridge_with_vm_guest(
        &self,
        key: &str,
        binary: &[u8],
        vm_guest: &[u8],
        binary_sha256: &str,
        vm_guest_sha256: &str,
    ) -> Result<()> {
        validate_key(key)?;
        validate_contents_sha256("release CLI", binary, binary_sha256)?;
        validate_contents_sha256("release VM guest", vm_guest, vm_guest_sha256)?;

        #[cfg(unix)]
        return self.install_bridge_with_vm_guest_inner(
            key,
            binary,
            vm_guest,
            binary_sha256,
            vm_guest_sha256,
            |_| Ok(()),
        );

        #[cfg(not(unix))]
        {
            let directory = self.version_dir(key);
            let metadata = match fs::symlink_metadata(&directory) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    return self.install_with_vm_guest(key, binary, vm_guest);
                }
                Err(error) => {
                    return Err(error)
                        .wrap_err_with(|| format!("failed to inspect {}", directory.display()));
                }
            };
            if !metadata.file_type().is_dir()
                || !strict_bridge_paths_match(&directory, binary_sha256, vm_guest_sha256)?
            {
                bail!(
                    "cannot adopt the Linux VM guest for Nanocodex version {key}: the cached version does not exactly match the release manifest"
                );
            }
            Ok(())
        }
    }

    #[cfg(unix)]
    fn install_bridge_with_vm_guest_inner(
        &self,
        key: &str,
        binary: &[u8],
        vm_guest: &[u8],
        binary_sha256: &str,
        vm_guest_sha256: &str,
        mut hook: impl FnMut(BridgeMutationPoint) -> Result<()>,
    ) -> Result<()> {
        let directory = match self.inspect_version_directory(key)? {
            VersionDirectoryState::Missing => {
                return self.install_with_vm_guest(key, binary, vm_guest);
            }
            VersionDirectoryState::Invalid => {
                bail!(
                    "cannot adopt the Linux VM guest for Nanocodex version {key}: the version path is not a regular directory"
                );
            }
            VersionDirectoryState::Pinned(directory) => directory,
        };
        directory.require_cli(binary_sha256, key)?;

        let guest = directory.hash_entry_state(VM_GUEST_BINARY_NAME, vm_guest_sha256)?;
        let guest_checksum =
            directory.checksum_entry_state(VM_GUEST_CHECKSUM_FILE, vm_guest_sha256)?;
        match (guest, guest_checksum) {
            (EntryState::Exact, EntryState::Exact) => {
                directory.require_complete_bridge(binary_sha256, vm_guest_sha256, key)?;
                return Ok(());
            }
            (EntryState::Exact, EntryState::Missing) => {}
            (EntryState::Missing, EntryState::Missing) => {
                hook(BridgeMutationPoint::GuestWrite)?;
                directory.require_identity_and_cli(binary_sha256, key)?;
                directory.write_new_entry(VM_GUEST_BINARY_NAME, vm_guest, true, || {
                    hook(BridgeMutationPoint::GuestCommit)?;
                    directory.require_identity_and_cli(binary_sha256, key)
                })?;
            }
            (EntryState::Missing, _) => {
                bail!(
                    "cannot adopt the Linux VM guest for Nanocodex version {key}: a VM guest checksum exists without its binary"
                );
            }
            _ => {
                bail!(
                    "cannot adopt the Linux VM guest for Nanocodex version {key}: the cached VM guest is special, corrupt, or does not match the release manifest"
                );
            }
        }

        hook(BridgeMutationPoint::ChecksumWrite)?;
        directory.require_identity_and_cli(binary_sha256, key)?;
        if directory.hash_entry_state(VM_GUEST_BINARY_NAME, vm_guest_sha256)? != EntryState::Exact {
            bail!(
                "cannot complete the Linux VM guest adoption for Nanocodex version {key}: the VM guest changed"
            );
        }
        directory.write_new_entry(
            VM_GUEST_CHECKSUM_FILE,
            format!("{vm_guest_sha256}\n").as_bytes(),
            false,
            || {
                hook(BridgeMutationPoint::ChecksumCommit)?;
                directory.require_identity_and_cli(binary_sha256, key)?;
                if directory.hash_entry_state(VM_GUEST_BINARY_NAME, vm_guest_sha256)?
                    != EntryState::Exact
                {
                    bail!(
                        "cannot complete the Linux VM guest adoption for Nanocodex version {key}: the VM guest changed"
                    );
                }
                Ok(())
            },
        )?;
        directory
            .matches_complete_bridge(binary_sha256, vm_guest_sha256)?
            .then_some(())
            .ok_or_else(|| {
                eyre!("the adopted Linux VM guest for Nanocodex version {key} did not verify")
            })
    }

    pub(super) fn activate_bridge_with_vm_guest(
        &self,
        key: &str,
        binary_sha256: &str,
        vm_guest_sha256: &str,
    ) -> Result<()> {
        validate_key(key)?;
        validate_sha256(binary_sha256)?;
        validate_sha256(vm_guest_sha256)?;

        #[cfg(unix)]
        return self.activate_bridge_with_vm_guest_inner(
            key,
            binary_sha256,
            vm_guest_sha256,
            |_| Ok(()),
        );

        #[cfg(not(unix))]
        {
            if !self.is_bridge_cached_with_vm_guest(key, binary_sha256, vm_guest_sha256)? {
                bail!(
                    "Nanocodex bridge version {key} is not installed exactly as declared by the release manifest"
                );
            }
            self.activate(key)
        }
    }

    #[cfg(unix)]
    fn activate_bridge_with_vm_guest_inner(
        &self,
        key: &str,
        binary_sha256: &str,
        vm_guest_sha256: &str,
        mut hook: impl FnMut(BridgeMutationPoint) -> Result<()>,
    ) -> Result<()> {
        let VersionDirectoryState::Pinned(directory) = self.inspect_version_directory(key)? else {
            bail!("Nanocodex bridge version {key} is not a regular installed version directory");
        };
        if !directory.matches_complete_bridge(binary_sha256, vm_guest_sha256)? {
            bail!("Nanocodex bridge version {key} does not exactly match the release manifest");
        }
        self.activate_symlink_with_check(key, || {
            hook(BridgeMutationPoint::Activation)?;
            directory.require_complete_bridge(binary_sha256, vm_guest_sha256, key)
        })?;
        self.install_launcher()
    }

    #[cfg(unix)]
    fn inspect_version_directory(&self, key: &str) -> Result<VersionDirectoryState> {
        use rustix::{
            fs::{AtFlags, FileType, Mode, OFlags, openat, statat},
            io::Errno,
        };
        use std::os::unix::fs::MetadataExt as _;

        let store = self.pinned_directories()?;
        let path = self.version_dir(key);
        let metadata = match statat(&store.versions, key, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(metadata) => metadata,
            Err(Errno::NOENT) => {
                return Ok(VersionDirectoryState::Missing);
            }
            Err(error) => {
                return Err(error)
                    .wrap_err_with(|| format!("failed to inspect {}", path.display()));
            }
        };
        if !FileType::from_raw_mode(metadata.st_mode).is_dir() {
            return Ok(VersionDirectoryState::Invalid);
        }
        let descriptor = openat(
            &store.versions,
            key,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .wrap_err_with(|| format!("failed to pin {}", path.display()))?;
        let directory = File::from(descriptor);
        let pinned = directory
            .metadata()
            .wrap_err_with(|| format!("failed to inspect pinned {}", path.display()))?;
        if !pinned.file_type().is_dir()
            || pinned.dev() != metadata.st_dev as u64
            || pinned.ino() != metadata.st_ino as u64
        {
            bail!("{} changed while it was being pinned", path.display());
        }
        Ok(VersionDirectoryState::Pinned(PinnedVersionDirectory {
            path,
            key: key.to_owned(),
            store,
            directory,
            device: pinned.dev(),
            inode: pinned.ino(),
        }))
    }

    pub(super) fn is_cached_with_vm_guest(&self, key: &str) -> Result<bool> {
        validate_key(key)?;
        #[cfg(unix)]
        {
            let VersionDirectoryState::Pinned(directory) = self.inspect_version_directory(key)?
            else {
                return Ok(false);
            };
            Ok(
                directory.locally_checksummed_entry(BINARY_NAME, CHECKSUM_FILE)?
                    && directory
                        .locally_checksummed_entry(VM_GUEST_BINARY_NAME, VM_GUEST_CHECKSUM_FILE)?,
            )
        }

        #[cfg(not(unix))]
        Ok(self.is_cached(key)?
            && file_matches_checksum(
                &self.version_dir(key).join(VM_GUEST_BINARY_NAME),
                &self.version_dir(key).join(VM_GUEST_CHECKSUM_FILE),
            )?)
    }

    pub(super) fn activate_with_vm_guest(&self, key: &str) -> Result<()> {
        validate_key(key)?;

        #[cfg(unix)]
        return self.activate_with_vm_guest_inner(key, |_| Ok(()));

        #[cfg(not(unix))]
        {
            if !self.is_cached_with_vm_guest(key)? {
                bail!("Nanocodex version {key} is not installed with a coherent Linux VM guest");
            }
            self.activate(key)
        }
    }

    #[cfg(unix)]
    fn activate_with_vm_guest_inner(
        &self,
        key: &str,
        mut hook: impl FnMut(BridgeMutationPoint) -> Result<()>,
    ) -> Result<()> {
        let VersionDirectoryState::Pinned(directory) = self.inspect_version_directory(key)? else {
            bail!("Nanocodex version {key} is not installed as a regular directory");
        };
        if !directory.locally_checksummed_with_vm_guest()? {
            bail!("Nanocodex version {key} does not contain a coherent Linux VM guest");
        }
        self.activate_symlink_with_check(key, || {
            hook(BridgeMutationPoint::Activation)?;
            if !directory.locally_checksummed_with_vm_guest()? {
                bail!("Nanocodex version {key} changed before it could be activated");
            }
            Ok(())
        })?;
        self.install_launcher()
    }

    pub(super) fn activate(&self, key: &str) -> Result<()> {
        validate_key(key)?;

        #[cfg(unix)]
        {
            let VersionDirectoryState::Pinned(directory) = self.inspect_version_directory(key)?
            else {
                bail!("Nanocodex version {key} is not installed as a regular directory");
            };
            if !directory.locally_checksummed_entry(BINARY_NAME, CHECKSUM_FILE)? {
                bail!("Nanocodex version {key} is not installed or its checksum is invalid");
            }
            self.activate_symlink_with_check(key, || {
                if !directory.locally_checksummed_entry(BINARY_NAME, CHECKSUM_FILE)? {
                    bail!("Nanocodex version {key} changed before it could be activated");
                }
                Ok(())
            })?;
            self.install_launcher()?;
        }

        #[cfg(not(unix))]
        {
            if !self.is_cached(key)? {
                bail!("Nanocodex version {key} is not installed or its checksum is invalid");
            }
            self_replace::self_replace(self.binary_path(key)).wrap_err(
                "failed to replace the running Nanocodex executable with the selected version",
            )?;
            atomic_write(
                &self.root.join("active-version"),
                format!("{key}\n").as_bytes(),
                false,
            )?;
        }

        Ok(())
    }

    pub(super) fn active(&self) -> Result<Option<String>> {
        #[cfg(unix)]
        {
            let target = match fs::read_link(self.root.join("current")) {
                Ok(target) => target,
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(error).wrap_err("failed to read the active Nanocodex link");
                }
            };
            target
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
                .ok_or_else(|| eyre!("the active Nanocodex link has an invalid target"))
                .map(Some)
        }

        #[cfg(not(unix))]
        {
            let path = self.root.join("active-version");
            match fs::read_to_string(&path) {
                Ok(key) => Ok(Some(key.trim().to_owned())),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
                Err(error) => {
                    Err(error).wrap_err_with(|| format!("failed to read {}", path.display()))
                }
            }
        }
    }

    pub(super) fn promote_manager(&self, key: &str) -> Result<()> {
        validate_key(key)?;

        #[cfg(unix)]
        {
            let VersionDirectoryState::Pinned(directory) = self.inspect_version_directory(key)?
            else {
                bail!("cannot promote missing Nanocodex version {key} to updater");
            };
            let contents = directory
                .read_locally_checksummed_entry(BINARY_NAME, CHECKSUM_FILE)?
                .ok_or_else(|| {
                    eyre!("cannot promote invalid Nanocodex version {key} to updater")
                })?;
            self.require_store_identity()?;
            atomic_write(&self.updater_path(), &contents, true)?;
            self.write_updater_checksum(&contents)?;
        }

        #[cfg(not(unix))]
        if !self.is_cached(key)? {
            bail!("cannot promote missing Nanocodex version {key} to updater");
        }

        Ok(())
    }

    #[cfg(unix)]
    pub(super) fn prepare_legacy_nightly_bootstrap() -> Result<bool> {
        let executable = std::env::current_exe()
            .wrap_err("failed to locate the running Nanocodex executable")?;
        Self::with_locked_legacy_nightly_store(&executable, |store| store.install_launcher())
    }

    #[cfg(not(unix))]
    pub(super) fn prepare_legacy_nightly_bootstrap() -> Result<bool> {
        Ok(false)
    }

    #[cfg(unix)]
    pub(super) fn promote_running_legacy_nightly_manager() -> Result<bool> {
        let executable = std::env::current_exe()
            .wrap_err("failed to locate the running Nanocodex executable")?;
        Self::with_locked_legacy_nightly_store(&executable, |store| {
            store.promote_manager("nightly")
        })
    }

    #[cfg(not(unix))]
    pub(super) fn promote_running_legacy_nightly_manager() -> Result<bool> {
        Ok(false)
    }

    #[cfg(unix)]
    fn with_locked_legacy_nightly_store(
        executable: &Path,
        action: impl FnOnce(&LockedVersionStore) -> Result<()>,
    ) -> Result<bool> {
        let Some(store) = Self::legacy_nightly_store_for(executable)? else {
            return Ok(false);
        };
        let locked = store.lock_exclusive()?;
        let Some(rechecked) = Self::legacy_nightly_store_for(executable)? else {
            return Ok(false);
        };
        if rechecked.root != store.root {
            bail!("the legacy nightly version store changed while it was locked");
        }
        action(&locked)?;
        Ok(true)
    }

    #[cfg(unix)]
    fn legacy_nightly_store_for(executable: &Path) -> Result<Option<Self>> {
        let executable = executable
            .canonicalize()
            .wrap_err_with(|| format!("failed to resolve {}", executable.display()))?;
        let Some(version_directory) = executable.parent() else {
            return Ok(None);
        };
        let Some(versions_directory) = version_directory.parent() else {
            return Ok(None);
        };
        if versions_directory
            .file_name()
            .and_then(|name| name.to_str())
            != Some("versions")
        {
            return Ok(None);
        }
        let Some(root) = versions_directory.parent() else {
            return Ok(None);
        };
        let store = Self {
            root: root.to_path_buf(),
            pinned: None,
        };
        if store.active()?.as_deref() != Some("nightly") || store.updater_checksum_path().is_file()
        {
            return Ok(None);
        }
        let active_binary = match store.binary_path("nightly").canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).wrap_err("failed to resolve the active nightly Nanocodex");
            }
        };
        if executable != active_binary {
            return Ok(None);
        }

        Ok(Some(store))
    }

    fn write_updater_checksum(&self, contents: &[u8]) -> Result<()> {
        self.require_store_identity()?;
        let checksum = hex::encode(Sha256::digest(contents));
        atomic_write(
            &self.updater_checksum_path(),
            format!("{checksum}\n").as_bytes(),
            false,
        )
    }

    fn seed_running_updater_checksum(&self, executable: &Path, contents: &[u8]) -> Result<()> {
        if self.updater_checksum_path().is_file() {
            return Ok(());
        }
        let executable = executable
            .canonicalize()
            .wrap_err_with(|| format!("failed to resolve {}", executable.display()))?;
        let updater = self
            .updater_path()
            .canonicalize()
            .wrap_err("failed to resolve the Nanocodex updater")?;
        if executable == updater {
            self.write_updater_checksum(contents)?;
        }
        Ok(())
    }

    fn versions_dir(&self) -> PathBuf {
        self.root.join("versions")
    }

    fn version_dir(&self, key: &str) -> PathBuf {
        self.versions_dir().join(key)
    }

    fn binary_path(&self, key: &str) -> PathBuf {
        self.version_dir(key).join(BINARY_NAME)
    }

    #[cfg(any(not(unix), test))]
    fn checksum_path(&self, key: &str) -> PathBuf {
        self.version_dir(key).join(CHECKSUM_FILE)
    }

    fn updater_path(&self) -> PathBuf {
        self.root.join("updater").join(BINARY_NAME)
    }

    fn updater_checksum_path(&self) -> PathBuf {
        self.root.join("updater").join(CHECKSUM_FILE)
    }

    #[cfg(unix)]
    fn activate_symlink_with_check(
        &self,
        key: &str,
        before_activation: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        use rustix::{
            fs::{AtFlags, fsync, renameat, symlinkat, unlinkat},
            io::Errno,
        };

        let pinned = self.pinned_directories()?;
        let serial = NEXT_BRIDGE_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let temporary = format!(".current-{}-{serial}", std::process::id());
        match unlinkat(&pinned.root, temporary.as_str(), AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(error) => {
                return Err(error).wrap_err("failed to remove a stale activation link");
            }
        }
        symlinkat(
            Path::new("versions").join(key),
            &pinned.root,
            temporary.as_str(),
        )
        .wrap_err("failed to create the active Nanocodex link")?;
        if let Err(error) = before_activation() {
            let _ = unlinkat(&pinned.root, temporary.as_str(), AtFlags::empty());
            return Err(error);
        }
        pinned.require_identity()?;
        if let Err(error) = renameat(&pinned.root, temporary.as_str(), &pinned.root, "current") {
            let _ = unlinkat(&pinned.root, temporary.as_str(), AtFlags::empty());
            return Err(error).wrap_err("failed to activate the selected Nanocodex version");
        }
        fsync(&pinned.root).wrap_err("failed to sync the active Nanocodex version")?;
        Ok(())
    }

    #[cfg(unix)]
    fn install_launcher(&self) -> Result<()> {
        const LAUNCHER: &str = r#"#!/bin/sh
set -eu

case "$0" in
    */*) launcher=$0 ;;
    *) launcher=$(command -v "$0") ;;
esac
bin_dir=$(CDPATH= cd -- "$(dirname -- "$launcher")" && pwd -P)
install_root=$(dirname -- "$bin_dir")
export NANOCODEX_DIR="$install_root"

if [ "${1-}" = "update" ] && [ -f "$install_root/updater/nanocodex.sha256" ]; then
    exec "$install_root/updater/nanocodex" "$@"
fi
exec "$install_root/current/nanocodex" "$@"
"#;

        self.require_store_identity()?;
        let path = self.root.join("bin").join(BINARY_NAME);
        if fs::read(&path).is_ok_and(|contents| contents == LAUNCHER.as_bytes()) {
            return Ok(());
        }
        atomic_write(&path, LAUNCHER.as_bytes(), true)
    }
}

#[cfg(unix)]
impl PinnedStoreDirectories {
    fn open(root_path: PathBuf, root: File, lock: Option<&File>) -> Result<Self> {
        use rustix::{
            fs::{AtFlags, FileType, Mode, OFlags, mkdirat, openat, statat},
            io::Errno,
        };
        use std::os::unix::fs::MetadataExt as _;

        match mkdirat(&root, "versions", Mode::RUSR | Mode::WUSR | Mode::XUSR) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(error) => {
                return Err(error).wrap_err("failed to create the Nanocodex versions directory");
            }
        }
        let versions = File::from(
            openat(
                &root,
                "versions",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .wrap_err("failed to pin the Nanocodex versions directory")?,
        );
        let root_metadata = root
            .metadata()
            .wrap_err("failed to inspect the pinned Nanocodex store")?;
        let lock_identity = lock
            .map(|lock| {
                let metadata = lock
                    .metadata()
                    .wrap_err("failed to inspect the pinned Nanocodex store lock")?;
                if !metadata.file_type().is_file() || metadata.nlink() != 1 {
                    bail!("the pinned Nanocodex store lock is not a regular file");
                }
                Ok((metadata.dev(), metadata.ino()))
            })
            .transpose()?;
        let versions_metadata = versions
            .metadata()
            .wrap_err("failed to inspect the pinned Nanocodex versions directory")?;
        let named_versions = statat(&root, "versions", AtFlags::SYMLINK_NOFOLLOW)
            .wrap_err("failed to recheck the Nanocodex versions directory")?;
        if !root_metadata.file_type().is_dir()
            || !versions_metadata.file_type().is_dir()
            || !FileType::from_raw_mode(named_versions.st_mode).is_dir()
            || versions_metadata.dev() != named_versions.st_dev as u64
            || versions_metadata.ino() != named_versions.st_ino as u64
        {
            bail!("the Nanocodex versions directory changed while it was being pinned");
        }
        Ok(Self {
            root_path: root_path.clone(),
            root,
            root_device: root_metadata.dev(),
            root_inode: root_metadata.ino(),
            lock_identity,
            versions_path: root_path.join("versions"),
            versions,
            versions_device: versions_metadata.dev(),
            versions_inode: versions_metadata.ino(),
        })
    }

    fn require_identity(&self) -> Result<()> {
        use rustix::fs::{AtFlags, FileType, statat};
        use std::os::unix::fs::MetadataExt as _;

        let root = fs::symlink_metadata(&self.root_path)
            .wrap_err("failed to recheck the Nanocodex version store")?;
        let root_descriptor = self
            .root
            .metadata()
            .wrap_err("failed to recheck the pinned Nanocodex version store")?;
        if !root.file_type().is_dir()
            || !root_descriptor.file_type().is_dir()
            || root.dev() != self.root_device
            || root.ino() != self.root_inode
            || root_descriptor.dev() != self.root_device
            || root_descriptor.ino() != self.root_inode
        {
            bail!("the Nanocodex version store no longer names the locked directory");
        }

        if let Some((lock_device, lock_inode)) = self.lock_identity {
            let lock = statat(&self.root, STORE_LOCK_FILE, AtFlags::SYMLINK_NOFOLLOW)
                .wrap_err("failed to recheck the locked Nanocodex store lock")?;
            if !FileType::from_raw_mode(lock.st_mode).is_file()
                || lock.st_nlink != 1
                || lock.st_dev as u64 != lock_device
                || lock.st_ino as u64 != lock_inode
            {
                bail!("the Nanocodex version-store lock changed while it was held");
            }
        }

        let versions = fs::symlink_metadata(&self.versions_path)
            .wrap_err("failed to recheck the Nanocodex versions directory")?;
        let versions_descriptor = self
            .versions
            .metadata()
            .wrap_err("failed to recheck the pinned Nanocodex versions directory")?;
        let named_versions = statat(&self.root, "versions", AtFlags::SYMLINK_NOFOLLOW)
            .wrap_err("failed to recheck the locked Nanocodex versions directory")?;
        if !versions.file_type().is_dir()
            || !versions_descriptor.file_type().is_dir()
            || !FileType::from_raw_mode(named_versions.st_mode).is_dir()
            || versions.dev() != self.versions_device
            || versions.ino() != self.versions_inode
            || versions_descriptor.dev() != self.versions_device
            || versions_descriptor.ino() != self.versions_inode
            || named_versions.st_dev as u64 != self.versions_device
            || named_versions.st_ino as u64 != self.versions_inode
        {
            bail!("the Nanocodex versions directory no longer belongs to the locked store");
        }
        Ok(())
    }
}

#[cfg(unix)]
impl PinnedVersionDirectory {
    fn recheck_identity(&self) -> Result<()> {
        use rustix::fs::{AtFlags, FileType, statat};

        self.store.require_identity()?;
        let metadata = statat(
            &self.store.versions,
            self.key.as_str(),
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .wrap_err_with(|| format!("failed to recheck {}", self.path.display()))?;
        if !FileType::from_raw_mode(metadata.st_mode).is_dir()
            || metadata.st_dev as u64 != self.device
            || metadata.st_ino as u64 != self.inode
        {
            bail!(
                "{} no longer names the pinned Nanocodex version directory",
                self.path.display()
            );
        }
        Ok(())
    }

    fn entry_snapshot_stable(
        &self,
        name: &str,
        before: &fs::Metadata,
        after: &fs::Metadata,
    ) -> Result<bool> {
        use std::os::unix::fs::MetadataExt as _;

        use rustix::{
            fs::{AtFlags, FileType, statat},
            io::Errno,
        };

        let stable_descriptor = before.file_type().is_file()
            && after.file_type().is_file()
            && before.nlink() == 1
            && after.nlink() == 1
            && before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.mode() == after.mode()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec();
        if !stable_descriptor {
            return Ok(false);
        }
        let named = match statat(&self.directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(named) => named,
            Err(Errno::NOENT) => return Ok(false),
            Err(error) => {
                return Err(error).wrap_err_with(|| {
                    format!("failed to recheck {name} in {}", self.path.display())
                });
            }
        };
        Ok(FileType::from_raw_mode(named.st_mode).is_file()
            && named.st_nlink == 1
            && named.st_dev as u64 == before.dev()
            && named.st_ino as u64 == before.ino()
            && named.st_mode as u32 == before.mode()
            && named.st_size >= 0
            && named.st_size as u64 == before.len())
    }

    fn hash_entry_state(&self, name: &str, expected_sha256: &str) -> Result<EntryState> {
        use rustix::{
            fs::{Mode, OFlags, openat},
            io::Errno,
        };

        let named = self.named_regular_entry_state(name)?;
        if named != EntryState::Exact {
            return Ok(named);
        }
        let descriptor = match openat(
            &self.directory,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(descriptor) => descriptor,
            Err(Errno::NOENT) => return Ok(EntryState::Missing),
            Err(Errno::LOOP | Errno::NXIO | Errno::OPNOTSUPP) => {
                return Ok(EntryState::Invalid);
            }
            Err(error) => {
                return Err(error)
                    .wrap_err_with(|| format!("failed to open {name} in {}", self.path.display()));
            }
        };
        let mut file = File::from(descriptor);
        let before = file
            .metadata()
            .wrap_err_with(|| format!("failed to inspect {name} in {}", self.path.display()))?;
        use std::os::unix::fs::MetadataExt as _;
        if !before.file_type().is_file()
            || before.nlink() != 1
            || before.len() > MAX_CACHED_BINARY_BYTES
        {
            return Ok(EntryState::Invalid);
        }

        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        let mut total = 0_u64;
        {
            let mut limited = (&mut file).take(MAX_CACHED_BINARY_BYTES + 1);
            loop {
                let read = limited.read(&mut buffer).wrap_err_with(|| {
                    format!("failed to read {name} in {}", self.path.display())
                })?;
                if read == 0 {
                    break;
                }
                total += read as u64;
                digest.update(&buffer[..read]);
            }
        }
        let after = file
            .metadata()
            .wrap_err_with(|| format!("failed to recheck {name} in {}", self.path.display()))?;
        if total > MAX_CACHED_BINARY_BYTES || !self.entry_snapshot_stable(name, &before, &after)? {
            return Ok(EntryState::Invalid);
        }
        Ok(if hex::encode(digest.finalize()) == expected_sha256 {
            EntryState::Exact
        } else {
            EntryState::Mismatch
        })
    }

    fn read_locally_checksummed_entry(
        &self,
        name: &str,
        checksum_name: &str,
    ) -> Result<Option<Vec<u8>>> {
        use rustix::{
            fs::{Mode, OFlags, openat},
            io::Errno,
        };

        self.recheck_identity()?;
        let Some(expected) = self.checksum_value(checksum_name)? else {
            return Ok(None);
        };
        if self.named_regular_entry_state(name)? != EntryState::Exact {
            return Ok(None);
        }
        let descriptor = match openat(
            &self.directory,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(descriptor) => descriptor,
            Err(Errno::NOENT | Errno::LOOP | Errno::NXIO | Errno::OPNOTSUPP) => return Ok(None),
            Err(error) => {
                return Err(error)
                    .wrap_err_with(|| format!("failed to open {name} in {}", self.path.display()));
            }
        };
        let mut file = File::from(descriptor);
        let before = file
            .metadata()
            .wrap_err_with(|| format!("failed to inspect {name} in {}", self.path.display()))?;
        use std::os::unix::fs::MetadataExt as _;
        if !before.file_type().is_file()
            || before.nlink() != 1
            || before.len() > MAX_CACHED_BINARY_BYTES
        {
            return Ok(None);
        }
        let mut contents = Vec::with_capacity(before.len() as usize);
        (&mut file)
            .take(MAX_CACHED_BINARY_BYTES + 1)
            .read_to_end(&mut contents)
            .wrap_err_with(|| format!("failed to read {name} in {}", self.path.display()))?;
        let after = file
            .metadata()
            .wrap_err_with(|| format!("failed to recheck {name} in {}", self.path.display()))?;
        if contents.len() as u64 > MAX_CACHED_BINARY_BYTES
            || !self.entry_snapshot_stable(name, &before, &after)?
            || hex::encode(Sha256::digest(&contents)) != expected
        {
            return Ok(None);
        }
        self.recheck_identity()?;
        Ok(Some(contents))
    }

    fn checksum_entry_state(&self, name: &str, expected_sha256: &str) -> Result<EntryState> {
        let Some(value) = self.checksum_value(name)? else {
            return Ok(match self.entry_exists(name)? {
                false => EntryState::Missing,
                true => EntryState::Invalid,
            });
        };
        Ok(if value == expected_sha256 {
            EntryState::Exact
        } else {
            EntryState::Mismatch
        })
    }

    fn checksum_value(&self, name: &str) -> Result<Option<String>> {
        use rustix::{
            fs::{Mode, OFlags, openat},
            io::Errno,
        };

        if self.named_regular_entry_state(name)? != EntryState::Exact {
            return Ok(None);
        }
        let descriptor = match openat(
            &self.directory,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(descriptor) => descriptor,
            Err(Errno::NOENT | Errno::LOOP | Errno::NXIO | Errno::OPNOTSUPP) => return Ok(None),
            Err(error) => {
                return Err(error)
                    .wrap_err_with(|| format!("failed to open {name} in {}", self.path.display()));
            }
        };
        let mut file = File::from(descriptor);
        let before = file
            .metadata()
            .wrap_err_with(|| format!("failed to inspect {name} in {}", self.path.display()))?;
        use std::os::unix::fs::MetadataExt as _;
        if !before.file_type().is_file()
            || before.nlink() != 1
            || before.len() != CHECKSUM_BYTES as u64
        {
            return Ok(None);
        }
        let mut contents = Vec::with_capacity(CHECKSUM_BYTES + 1);
        (&mut file)
            .take((CHECKSUM_BYTES + 1) as u64)
            .read_to_end(&mut contents)
            .wrap_err_with(|| format!("failed to read {name} in {}", self.path.display()))?;
        let after = file
            .metadata()
            .wrap_err_with(|| format!("failed to recheck {name} in {}", self.path.display()))?;
        if contents.len() != CHECKSUM_BYTES
            || contents[64] != b'\n'
            || !self.entry_snapshot_stable(name, &before, &after)?
        {
            return Ok(None);
        }
        let checksum = std::str::from_utf8(&contents[..64]).ok();
        Ok(checksum
            .filter(|checksum| valid_sha256(checksum))
            .map(str::to_owned))
    }

    fn entry_exists(&self, name: &str) -> Result<bool> {
        use rustix::{
            fs::{AtFlags, statat},
            io::Errno,
        };

        match statat(&self.directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(Errno::NOENT) => Ok(false),
            Err(error) => Err(error)
                .wrap_err_with(|| format!("failed to inspect {name} in {}", self.path.display())),
        }
    }

    fn named_regular_entry_state(&self, name: &str) -> Result<EntryState> {
        use rustix::{
            fs::{AtFlags, FileType, statat},
            io::Errno,
        };

        match statat(&self.directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(metadata) if FileType::from_raw_mode(metadata.st_mode).is_file() => {
                Ok(EntryState::Exact)
            }
            Ok(_) => Ok(EntryState::Invalid),
            Err(Errno::NOENT) => Ok(EntryState::Missing),
            Err(error) => Err(error)
                .wrap_err_with(|| format!("failed to inspect {name} in {}", self.path.display())),
        }
    }

    fn locally_checksummed_entry(&self, name: &str, checksum_name: &str) -> Result<bool> {
        self.recheck_identity()?;
        let Some(expected) = self.checksum_value(checksum_name)? else {
            return Ok(false);
        };
        let matches = self.hash_entry_state(name, &expected)? == EntryState::Exact;
        self.recheck_identity()?;
        Ok(matches)
    }

    fn locally_checksummed_with_vm_guest(&self) -> Result<bool> {
        Ok(self.locally_checksummed_entry(BINARY_NAME, CHECKSUM_FILE)?
            && self.locally_checksummed_entry(VM_GUEST_BINARY_NAME, VM_GUEST_CHECKSUM_FILE)?)
    }

    fn require_cli(&self, binary_sha256: &str, key: &str) -> Result<()> {
        self.require_identity_and_cli(binary_sha256, key)
    }

    fn require_identity_and_cli(&self, binary_sha256: &str, key: &str) -> Result<()> {
        self.recheck_identity()?;
        if self.hash_entry_state(BINARY_NAME, binary_sha256)? != EntryState::Exact
            || self.checksum_entry_state(CHECKSUM_FILE, binary_sha256)? != EntryState::Exact
        {
            bail!(
                "cannot adopt the Linux VM guest for Nanocodex version {key}: the cached CLI and checksum do not exactly match the release manifest"
            );
        }
        self.recheck_identity()?;
        Ok(())
    }

    fn matches_exact_cli(&self, binary_sha256: &str) -> Result<bool> {
        if self.recheck_identity().is_err() {
            return Ok(false);
        }
        let matches = self.hash_entry_state(BINARY_NAME, binary_sha256)? == EntryState::Exact
            && self.checksum_entry_state(CHECKSUM_FILE, binary_sha256)? == EntryState::Exact;
        if self.recheck_identity().is_err() {
            return Ok(false);
        }
        Ok(matches)
    }

    fn matches_complete_bridge(&self, binary_sha256: &str, vm_guest_sha256: &str) -> Result<bool> {
        if !self.matches_exact_cli(binary_sha256)? {
            return Ok(false);
        }
        let matches = self.hash_entry_state(VM_GUEST_BINARY_NAME, vm_guest_sha256)?
            == EntryState::Exact
            && self.checksum_entry_state(VM_GUEST_CHECKSUM_FILE, vm_guest_sha256)?
                == EntryState::Exact;
        if self.recheck_identity().is_err() {
            return Ok(false);
        }
        Ok(matches)
    }

    fn require_complete_bridge(
        &self,
        binary_sha256: &str,
        vm_guest_sha256: &str,
        key: &str,
    ) -> Result<()> {
        self.recheck_identity()?;
        if !self.matches_complete_bridge(binary_sha256, vm_guest_sha256)? {
            bail!("Nanocodex bridge version {key} changed before it could be activated");
        }
        Ok(())
    }

    fn write_new_entry(
        &self,
        target: &str,
        contents: &[u8],
        executable: bool,
        before_commit: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        use rustix::{
            fs::{AtFlags, fsync, unlinkat},
            io::Errno,
        };

        let mut staged = None;
        for _ in 0..128 {
            let serial = NEXT_BRIDGE_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let temporary = format!(".bridge-{}-{serial}", std::process::id());
            let description = format!("temporary {target} in {}", self.path.display());
            match write_exclusive_entry_at(
                &self.directory,
                temporary.as_str(),
                contents,
                executable,
                &description,
            ) {
                Ok(file) => {
                    staged = Some((temporary, file));
                    break;
                }
                Err(ExclusiveEntryWriteError::Open(Errno::EXIST)) => continue,
                Err(ExclusiveEntryWriteError::Open(error)) => {
                    return Err(error).wrap_err_with(|| {
                        format!(
                            "failed to create a temporary {target} in {}",
                            self.path.display()
                        )
                    });
                }
                Err(ExclusiveEntryWriteError::AfterCreate(error)) => {
                    let _ = unlinkat(&self.directory, temporary.as_str(), AtFlags::empty());
                    return Err(error);
                }
            }
        }
        let (temporary, file) = staged.ok_or_else(|| {
            eyre!(
                "failed to reserve a temporary {target} name in {}",
                self.path.display()
            )
        })?;

        let result = (|| {
            drop(file);

            before_commit()?;
            rename_noreplace_at(&self.directory, &temporary, target).wrap_err_with(|| {
                format!("failed to install {target} in {}", self.path.display())
            })?;
            fsync(&self.directory).wrap_err_with(|| {
                format!(
                    "failed to sync {} after installing {target}",
                    self.path.display()
                )
            })?;
            Ok(())
        })();
        if result.is_err() {
            let _ = unlinkat(&self.directory, temporary.as_str(), AtFlags::empty());
        }
        result
    }
}

#[cfg(unix)]
fn write_exclusive_entry_at(
    directory: &File,
    name: &str,
    contents: &[u8],
    executable: bool,
    description: &str,
) -> std::result::Result<File, ExclusiveEntryWriteError> {
    use rustix::fs::{Mode, OFlags, fchmod, openat};

    let descriptor = openat(
        directory,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(ExclusiveEntryWriteError::Open)?;
    let mut file = File::from(descriptor);
    let result = (|| {
        file.write_all(contents)
            .wrap_err_with(|| format!("failed to write {description}"))?;
        let mode = if executable {
            Mode::RUSR | Mode::WUSR | Mode::XUSR | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH
        } else {
            Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH
        };
        fchmod(&file, mode)
            .wrap_err_with(|| format!("failed to set permissions for {description}"))?;
        file.sync_all()
            .wrap_err_with(|| format!("failed to sync {description}"))?;
        Ok(())
    })();
    match result {
        Ok(()) => Ok(file),
        Err(error) => Err(ExclusiveEntryWriteError::AfterCreate(error)),
    }
}

#[cfg(unix)]
fn cleanup_exchanged_version_directory(
    versions: &File,
    name: &str,
    old: &PinnedVersionDirectory,
) -> Result<()> {
    use rustix::{
        fs::{AtFlags, FileType, fsync, statat, unlinkat},
        io::Errno,
    };

    for entry in [
        BINARY_NAME,
        CHECKSUM_FILE,
        VM_GUEST_BINARY_NAME,
        VM_GUEST_CHECKSUM_FILE,
    ] {
        match unlinkat(&old.directory, entry, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            // Never recurse through a pathname. Unexpected entries leave a
            // hidden quarantine instead of risking replacement-tree mutation.
            Err(_) => return Ok(()),
        }
    }
    fsync(&old.directory).wrap_err("failed to sync the replaced Nanocodex version")?;
    let named = match statat(versions, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(named) => named,
        Err(Errno::NOENT) => return Ok(()),
        Err(error) => return Err(error).wrap_err("failed to inspect the replaced version"),
    };
    if !FileType::from_raw_mode(named.st_mode).is_dir()
        || named.st_dev as u64 != old.device
        || named.st_ino as u64 != old.inode
    {
        return Ok(());
    }
    match unlinkat(versions, name, AtFlags::REMOVEDIR) {
        Ok(()) | Err(Errno::NOENT | Errno::NOTEMPTY) => {}
        Err(error) => return Err(error).wrap_err("failed to remove the replaced version"),
    }
    fsync(versions).wrap_err("failed to sync replaced-version cleanup")
}

#[cfg(unix)]
fn create_staged_version_directory(
    store: Arc<PinnedStoreDirectories>,
) -> Result<PinnedVersionDirectory> {
    use rustix::{
        fs::{AtFlags, FileType, Mode, OFlags, mkdirat, openat, statat},
        io::Errno,
    };
    use std::os::unix::fs::MetadataExt as _;

    for _ in 0..128 {
        let serial = NEXT_BRIDGE_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let name = format!(".install-{}-{serial}", std::process::id());
        match mkdirat(
            &store.versions,
            name.as_str(),
            Mode::RUSR | Mode::WUSR | Mode::XUSR,
        ) {
            Ok(()) => {}
            Err(Errno::EXIST) => continue,
            Err(error) => {
                return Err(error).wrap_err("failed to create a staged Nanocodex version");
            }
        }
        let directory = File::from(
            openat(
                &store.versions,
                name.as_str(),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .wrap_err("failed to pin the staged Nanocodex version")?,
        );
        let descriptor = directory
            .metadata()
            .wrap_err("failed to inspect the staged Nanocodex version")?;
        let named = statat(&store.versions, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
            .wrap_err("failed to recheck the staged Nanocodex version")?;
        if !descriptor.file_type().is_dir()
            || !FileType::from_raw_mode(named.st_mode).is_dir()
            || descriptor.dev() != named.st_dev as u64
            || descriptor.ino() != named.st_ino as u64
        {
            bail!("the staged Nanocodex version changed while it was being pinned");
        }
        return Ok(PinnedVersionDirectory {
            path: store.versions_path.join(&name),
            key: name,
            store,
            directory,
            device: descriptor.dev(),
            inode: descriptor.ino(),
        });
    }
    bail!("failed to reserve a staged Nanocodex version directory name")
}

#[cfg(unix)]
fn write_staged_version_entry(
    directory: &File,
    name: &str,
    contents: &[u8],
    executable: bool,
) -> Result<()> {
    use rustix::fs::{AtFlags, FileType, statat};
    use std::os::unix::fs::MetadataExt as _;

    let description = format!("staged {name}");
    let file = match write_exclusive_entry_at(directory, name, contents, executable, &description) {
        Ok(file) => file,
        Err(ExclusiveEntryWriteError::Open(error)) => {
            return Err(error).wrap_err_with(|| format!("failed to create staged {name}"));
        }
        Err(ExclusiveEntryWriteError::AfterCreate(error)) => return Err(error),
    };
    let descriptor = file
        .metadata()
        .wrap_err_with(|| format!("failed to inspect staged {name}"))?;
    let named = statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
        .wrap_err_with(|| format!("failed to recheck staged {name}"))?;
    if !descriptor.file_type().is_file()
        || descriptor.nlink() != 1
        || !FileType::from_raw_mode(named.st_mode).is_file()
        || named.st_nlink != 1
        || descriptor.dev() != named.st_dev as u64
        || descriptor.ino() != named.st_ino as u64
    {
        bail!("staged {name} changed before publication");
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rename_noreplace_at(directory: &File, source: &str, target: &str) -> rustix::io::Result<()> {
    rustix::fs::renameat_with(
        directory,
        source,
        directory,
        target,
        rustix::fs::RenameFlags::NOREPLACE,
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rename_exchange_at(directory: &File, source: &str, target: &str) -> rustix::io::Result<()> {
    rustix::fs::renameat_with(
        directory,
        source,
        directory,
        target,
        rustix::fs::RenameFlags::EXCHANGE,
    )
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn rename_exchange_at(_directory: &File, _source: &str, _target: &str) -> rustix::io::Result<()> {
    Err(rustix::io::Errno::NOTSUP)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn rename_noreplace_at(_directory: &File, _source: &str, _target: &str) -> rustix::io::Result<()> {
    Err(rustix::io::Errno::NOTSUP)
}

#[cfg(unix)]
fn pin_root_directory(path: &Path) -> Result<File> {
    use rustix::fs::{Mode, OFlags, open};
    use std::os::unix::fs::MetadataExt as _;

    ensure_directory_path(path, "Nanocodex version store")?;
    let root = File::from(
        open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .wrap_err("failed to pin the Nanocodex version store")?,
    );
    let descriptor = root
        .metadata()
        .wrap_err("failed to inspect the pinned Nanocodex version store")?;
    let named =
        fs::symlink_metadata(path).wrap_err("failed to recheck the Nanocodex version store")?;
    if !descriptor.file_type().is_dir()
        || !named.file_type().is_dir()
        || descriptor.dev() != named.dev()
        || descriptor.ino() != named.ino()
    {
        bail!("{} changed while it was being pinned", path.display());
    }
    Ok(root)
}

#[cfg(unix)]
fn ensure_directory_path(path: &Path, description: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => bail!("{} is not a regular directory", path.display()),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir_all(path).wrap_err_with(|| format!("failed to create {description}"))?;
            let metadata = fs::symlink_metadata(path)
                .wrap_err_with(|| format!("failed to inspect {description}"))?;
            if !metadata.file_type().is_dir() {
                bail!("{} is not a regular directory", path.display());
            }
            Ok(())
        }
        Err(error) => Err(error).wrap_err_with(|| format!("failed to inspect {description}")),
    }
}

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.starts_with('.')
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
    {
        bail!("invalid Nanocodex version key {key:?}");
    }
    Ok(())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_sha256(value: &str) -> Result<()> {
    if !valid_sha256(value) {
        bail!("invalid lowercase SHA-256 digest {value:?}");
    }
    Ok(())
}

fn validate_contents_sha256(description: &str, contents: &[u8], expected: &str) -> Result<()> {
    validate_sha256(expected)?;
    let actual = hex::encode(Sha256::digest(contents));
    if actual != expected {
        bail!("{description} does not match release manifest SHA-256 {expected}");
    }
    Ok(())
}

#[cfg(any(not(unix), test))]
fn file_matches_checksum(path: &Path, checksum_path: &Path) -> Result<bool> {
    for candidate in [path, checksum_path] {
        match fs::symlink_metadata(candidate) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error)
                    .wrap_err_with(|| format!("failed to inspect {}", candidate.display()));
            }
        }
    }
    let expected = fs::read(checksum_path)
        .wrap_err_with(|| format!("failed to read {}", checksum_path.display()))?;
    if expected.len() != CHECKSUM_BYTES || expected[64] != b'\n' {
        return Ok(false);
    }
    let Some(expected) = std::str::from_utf8(&expected[..64])
        .ok()
        .filter(|expected| valid_sha256(expected))
    else {
        return Ok(false);
    };
    let contents =
        fs::read(path).wrap_err_with(|| format!("failed to read cached {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(contents)) == expected)
}

#[cfg(not(unix))]
fn strict_bridge_paths_match(
    directory: &Path,
    binary_sha256: &str,
    vm_guest_sha256: &str,
) -> Result<bool> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .wrap_err_with(|| format!("failed to inspect {}", directory.display()));
        }
    }
    for (name, expected, checksum_name) in [
        (BINARY_NAME, binary_sha256, CHECKSUM_FILE),
        (
            VM_GUEST_BINARY_NAME,
            vm_guest_sha256,
            VM_GUEST_CHECKSUM_FILE,
        ),
    ] {
        let path = directory.join(name);
        let checksum_path = directory.join(checksum_name);
        for candidate in [&path, &checksum_path] {
            match fs::symlink_metadata(candidate) {
                Ok(metadata) if metadata.file_type().is_file() => {}
                Ok(_) => return Ok(false),
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
                Err(error) => {
                    return Err(error)
                        .wrap_err_with(|| format!("failed to inspect {}", candidate.display()));
                }
            }
        }
        let checksum = fs::read(&checksum_path)
            .wrap_err_with(|| format!("failed to read {}", checksum_path.display()))?;
        if checksum != format!("{expected}\n").as_bytes() {
            return Ok(false);
        }
        let contents =
            fs::read(&path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
        if hex::encode(Sha256::digest(contents)) != expected {
            return Ok(false);
        }
    }
    Ok(true)
}

fn atomic_write(path: &Path, contents: &[u8], executable: bool) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)
        .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
    let mut temporary =
        NamedTempFile::new_in(parent).wrap_err("failed to create a temporary install file")?;
    temporary
        .write_all(contents)
        .wrap_err_with(|| format!("failed to write {}", path.display()))?;
    temporary
        .as_file()
        .sync_all()
        .wrap_err_with(|| format!("failed to sync {}", path.display()))?;

    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;

        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))
            .wrap_err_with(|| format!("failed to make {} executable", path.display()))?;
    }

    #[cfg(not(unix))]
    let _ = executable;

    temporary
        .persist(path)
        .map_err(|error| error.error)
        .wrap_err_with(|| format!("failed to install {}", path.display()))?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sha256(contents: &[u8]) -> String {
        hex::encode(Sha256::digest(contents))
    }

    #[test]
    fn retains_versions_and_switches_the_active_link() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        store
            .prepare_with_contents("0.3.0", b"current", false)
            .unwrap();

        assert_eq!(store.active().unwrap().as_deref(), Some("0.3.0"));
        assert_eq!(fs::read(store.binary_path("0.3.0")).unwrap(), b"current");
        assert_eq!(fs::read(store.updater_path()).unwrap(), b"current");
        assert!(
            file_matches_checksum(&store.updater_path(), &store.updater_checksum_path()).unwrap()
        );
        let launcher = fs::read_to_string(directory.path().join("bin/nanocodex")).unwrap();
        assert!(launcher.contains("updater/nanocodex"));
        assert!(launcher.contains("export NANOCODEX_DIR"));

        store.install("0.2.0", b"previous").unwrap();
        store.activate("0.2.0").unwrap();

        assert_eq!(store.active().unwrap().as_deref(), Some("0.2.0"));
        assert_eq!(fs::read(store.binary_path("0.2.0")).unwrap(), b"previous");
        assert_eq!(fs::read(store.binary_path("0.3.0")).unwrap(), b"current");
    }

    #[test]
    fn active_nightly_bootstraps_a_legacy_updater_without_copying_it() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        store
            .prepare_with_contents("0.3.0", b"legacy", false)
            .unwrap();
        store.install("nightly", b"nightly").unwrap();
        store.activate("nightly").unwrap();
        fs::remove_file(store.updater_checksum_path()).unwrap();

        assert!(
            VersionStore::legacy_nightly_store_for(&store.binary_path("nightly"))
                .unwrap()
                .is_some()
        );
        store.install_launcher().unwrap();
        assert_eq!(fs::read(store.updater_path()).unwrap(), b"legacy");
        let launcher = fs::read_to_string(directory.path().join("bin/nanocodex")).unwrap();
        assert!(launcher.contains("updater/nanocodex.sha256"));
        assert!(launcher.contains("updater/nanocodex"));
        assert!(launcher.contains("current/nanocodex"));

        VersionStore::legacy_nightly_store_for(&store.binary_path("nightly"))
            .unwrap()
            .unwrap()
            .promote_manager("nightly")
            .unwrap();
        assert_eq!(fs::read(store.updater_path()).unwrap(), b"nightly");
        assert!(store.updater_checksum_path().is_file());

        store.install("local-build", b"local").unwrap();
        store.activate("local-build").unwrap();
        assert_eq!(fs::read(store.updater_path()).unwrap(), b"nightly");
        assert!(
            file_matches_checksum(&store.updater_path(), &store.updater_checksum_path()).unwrap()
        );
    }

    #[test]
    fn running_legacy_updater_seeds_its_checksum_marker() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        store
            .prepare_with_contents("0.3.0", b"legacy", false)
            .unwrap();
        fs::remove_file(store.updater_checksum_path()).unwrap();

        store
            .seed_running_updater_checksum(&store.updater_path(), b"legacy")
            .unwrap();

        assert!(
            file_matches_checksum(&store.updater_path(), &store.updater_checksum_path()).unwrap()
        );
    }

    #[test]
    fn refuses_corrupted_cached_versions() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        store.install("0.2.0", b"original").unwrap();
        assert!(store.is_cached("0.2.0").unwrap());

        fs::write(store.binary_path("0.2.0"), b"corrupted").unwrap();

        assert!(!store.is_cached("0.2.0").unwrap());
        assert!(store.activate("0.2.0").is_err());
    }

    #[test]
    fn installs_cli_and_vm_guest_as_one_activatable_directory() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());

        store
            .install_with_vm_guest("nightly-build", b"cli", b"guest")
            .unwrap();
        store.activate("nightly-build").unwrap();

        assert!(store.is_cached_with_vm_guest("nightly-build").unwrap());
        assert_eq!(
            fs::read(directory.path().join("current/nanocodex-vm-guest")).unwrap(),
            b"guest"
        );

        fs::write(
            store
                .version_dir("nightly-build")
                .join(VM_GUEST_BINARY_NAME),
            b"corrupted",
        )
        .unwrap();
        assert!(!store.is_cached_with_vm_guest("nightly-build").unwrap());
    }

    #[test]
    fn forced_coherent_install_atomically_replaces_a_self_checksummed_cache() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        let key = "nightly-build";
        store
            .install_with_vm_guest(key, b"old cli", b"wrong guest")
            .unwrap();

        store
            .reinstall_with_vm_guest(key, b"manifest cli", b"manifest guest")
            .unwrap();

        assert_eq!(fs::read(store.binary_path(key)).unwrap(), b"manifest cli");
        assert_eq!(
            fs::read(store.version_dir(key).join(VM_GUEST_BINARY_NAME)).unwrap(),
            b"manifest guest"
        );
        assert!(store.is_cached_with_vm_guest(key).unwrap());
        assert!(fs::read_dir(store.versions_dir()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".install-")
        }));

        store
            .reinstall_with_vm_guest(key, b"final cli", b"final guest")
            .unwrap();
        assert!(fs::read_dir(store.versions_dir()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".install-")
        }));
    }

    #[test]
    fn coherent_guest_activation_rechecks_all_four_entries_before_switching() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        store.install("previous", b"previous").unwrap();
        store.activate("previous").unwrap();
        store
            .install_with_vm_guest("candidate", b"candidate cli", b"candidate guest")
            .unwrap();

        let error = store
            .activate_with_vm_guest_inner("candidate", |point| {
                if point == BridgeMutationPoint::Activation {
                    fs::remove_file(store.version_dir("candidate").join(VM_GUEST_CHECKSUM_FILE))?;
                }
                Ok(())
            })
            .unwrap_err();

        assert!(error.to_string().contains("changed before"));
        assert_eq!(store.active().unwrap().as_deref(), Some("previous"));
    }

    #[test]
    fn deferring_manager_activation_never_bootstraps_a_cli_only_bridge() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());

        store
            .prepare_with_contents("0.6.0", b"running cli", true)
            .unwrap();

        assert!(store.active().unwrap().is_none());
        assert!(store.is_cached("0.6.0").unwrap());
    }

    #[test]
    fn installer_never_follows_a_symlinked_version_directory() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        fs::create_dir_all(store.versions_dir()).unwrap();
        fs::write(outside.path().join("sentinel"), b"untouched").unwrap();
        symlink(outside.path(), store.version_dir("target")).unwrap();

        assert!(store.install("target", b"payload").is_err());
        assert_eq!(
            fs::read(outside.path().join("sentinel")).unwrap(),
            b"untouched"
        );
        assert!(!outside.path().join(BINARY_NAME).exists());
        assert!(!outside.path().join(CHECKSUM_FILE).exists());
    }

    #[test]
    fn staged_entries_remain_bound_to_the_pinned_versions_directory() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = VersionStore::at(parent.path().join("store"));
        let pinned = store.pinned_directories().unwrap();
        let staging = create_staged_version_directory(Arc::clone(&pinned)).unwrap();
        let staging_name = staging.key.clone();
        let displaced = parent.path().join("versions-old");
        fs::rename(&pinned.versions_path, &displaced).unwrap();
        fs::create_dir(&pinned.versions_path).unwrap();
        symlink(outside.path(), pinned.versions_path.join(&staging_name)).unwrap();

        write_staged_version_entry(&staging.directory, BINARY_NAME, b"payload", true).unwrap();

        assert_eq!(
            fs::read(displaced.join(staging_name).join(BINARY_NAME)).unwrap(),
            b"payload"
        );
        assert!(!outside.path().join(BINARY_NAME).exists());
    }

    #[test]
    fn staged_directory_exchange_before_publication_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        let displaced = directory.path().join("displaced-stage");

        let error = store
            .install_version_directory_inner("candidate", b"manifest cli", None, false, |point| {
                assert_eq!(point, CoherentInstallMutationPoint::BeforePublish);
                let staging = fs::read_dir(store.versions_dir())?
                    .find_map(|entry| {
                        let entry = entry.ok()?;
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".install-")
                            .then(|| entry.path())
                    })
                    .unwrap();
                fs::rename(&staging, &displaced)?;
                fs::create_dir(&staging)?;
                fs::write(staging.join(BINARY_NAME), b"attacker cli")?;
                fs::write(
                    staging.join(CHECKSUM_FILE),
                    format!("{}\n", sha256(b"attacker cli")),
                )?;
                Ok(())
            })
            .unwrap_err();

        assert!(error.to_string().contains("changed before publication"));
        assert!(!store.version_dir("candidate").exists());
        assert_eq!(
            fs::read(displaced.join(BINARY_NAME)).unwrap(),
            b"manifest cli"
        );
    }

    #[test]
    fn staged_content_mutation_before_publication_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());

        let error = store
            .install_version_directory_inner("candidate", b"manifest cli", None, false, |_| {
                let staging = fs::read_dir(store.versions_dir())?
                    .find_map(|entry| {
                        let entry = entry.ok()?;
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".install-")
                            .then(|| entry.path())
                    })
                    .unwrap();
                fs::write(staging.join(BINARY_NAME), b"attacker cli")?;
                fs::write(
                    staging.join(CHECKSUM_FILE),
                    format!("{}\n", sha256(b"attacker cli")),
                )?;
                Ok(())
            })
            .unwrap_err();

        assert!(error.to_string().contains("changed before publication"));
        assert!(!store.version_dir("candidate").exists());
    }

    #[test]
    fn adopts_vm_guest_into_an_exact_legacy_cli_only_version() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        let key = "0.6.0";
        store.install(key, b"bridge cli").unwrap();
        assert!(store.is_cached(key).unwrap());
        assert!(!store.is_cached_with_vm_guest(key).unwrap());
        assert!(!store.version_dir(key).join(VM_GUEST_BINARY_NAME).exists());
        let cli_before = fs::read(store.binary_path(key)).unwrap();
        let checksum_before = fs::read(store.checksum_path(key)).unwrap();
        let cli_sha256 = sha256(b"bridge cli");
        let guest_sha256 = sha256(b"bridge guest");

        store
            .install_bridge_with_vm_guest(
                key,
                b"bridge cli",
                b"bridge guest",
                &cli_sha256,
                &guest_sha256,
            )
            .unwrap();
        store
            .install_bridge_with_vm_guest(
                key,
                b"bridge cli",
                b"bridge guest",
                &cli_sha256,
                &guest_sha256,
            )
            .unwrap();

        assert!(store.is_cached_with_vm_guest(key).unwrap());
        assert!(
            store
                .is_bridge_cached_with_vm_guest(key, &cli_sha256, &guest_sha256)
                .unwrap()
        );
        assert_eq!(fs::read(store.binary_path(key)).unwrap(), cli_before);
        assert_eq!(fs::read(store.checksum_path(key)).unwrap(), checksum_before);
        assert_eq!(
            fs::read(store.version_dir(key).join(VM_GUEST_BINARY_NAME)).unwrap(),
            b"bridge guest"
        );
    }

    #[test]
    fn refuses_to_adopt_vm_guest_for_mismatched_or_corrupt_cli_versions() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        let cli_sha256 = sha256(b"bridge cli");
        let guest_sha256 = sha256(b"bridge guest");

        store.install("mismatched", b"different cli").unwrap();
        let error = store
            .install_bridge_with_vm_guest(
                "mismatched",
                b"bridge cli",
                b"bridge guest",
                &cli_sha256,
                &guest_sha256,
            )
            .unwrap_err();
        assert!(error.to_string().contains("do not exactly match"));
        assert_eq!(
            fs::read(store.binary_path("mismatched")).unwrap(),
            b"different cli"
        );
        assert!(
            !store
                .version_dir("mismatched")
                .join(VM_GUEST_BINARY_NAME)
                .exists()
        );

        store.install("corrupt", b"bridge cli").unwrap();
        fs::write(store.checksum_path("corrupt"), b"invalid\n").unwrap();
        let error = store
            .install_bridge_with_vm_guest(
                "corrupt",
                b"bridge cli",
                b"bridge guest",
                &cli_sha256,
                &guest_sha256,
            )
            .unwrap_err();
        assert!(error.to_string().contains("do not exactly match"));
        assert!(
            !store
                .version_dir("corrupt")
                .join(VM_GUEST_BINARY_NAME)
                .exists()
        );

        store.install("corrupt-guest", b"bridge cli").unwrap();
        let guest_path = store
            .version_dir("corrupt-guest")
            .join(VM_GUEST_BINARY_NAME);
        let guest_checksum_path = store
            .version_dir("corrupt-guest")
            .join(VM_GUEST_CHECKSUM_FILE);
        fs::write(&guest_path, b"different guest").unwrap();
        fs::write(
            &guest_checksum_path,
            format!("{}\n", hex::encode(Sha256::digest(b"different guest"))),
        )
        .unwrap();
        assert!(
            !store
                .is_bridge_cached_with_vm_guest("corrupt-guest", &cli_sha256, &guest_sha256,)
                .unwrap()
        );
        let error = store
            .install_bridge_with_vm_guest(
                "corrupt-guest",
                b"bridge cli",
                b"bridge guest",
                &cli_sha256,
                &guest_sha256,
            )
            .unwrap_err();
        assert!(error.to_string().contains("cached VM guest"));
        assert_eq!(fs::read(&guest_path).unwrap(), b"different guest");
    }

    #[test]
    fn coherent_installer_stays_atomic_for_later_versions() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        store.install("0.7.0", b"later cli").unwrap();

        let error = store
            .install_with_vm_guest("0.7.0", b"later cli", b"later guest")
            .unwrap_err();

        assert!(error.to_string().contains("cannot coherently replace"));
        assert!(!store.is_cached_with_vm_guest("0.7.0").unwrap());
    }

    #[test]
    fn retries_an_interrupted_vm_guest_adoption() {
        use std::os::unix::fs::MetadataExt as _;

        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        let key = "0.6.0";
        store.install(key, b"bridge cli").unwrap();
        let guest_path = store.version_dir(key).join(VM_GUEST_BINARY_NAME);
        let guest_checksum_path = store.version_dir(key).join(VM_GUEST_CHECKSUM_FILE);
        let cli_sha256 = sha256(b"bridge cli");
        let guest_sha256 = sha256(b"bridge guest");

        let error = store
            .install_bridge_with_vm_guest_inner(
                key,
                b"bridge cli",
                b"bridge guest",
                &cli_sha256,
                &guest_sha256,
                |point| {
                    if point == BridgeMutationPoint::ChecksumWrite {
                        bail!("injected interruption before checksum");
                    }
                    Ok(())
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("injected interruption"));
        assert_eq!(fs::read(&guest_path).unwrap(), b"bridge guest");
        assert!(!guest_checksum_path.exists());
        let guest_inode = fs::metadata(&guest_path).unwrap().ino();

        store
            .install_bridge_with_vm_guest(
                key,
                b"bridge cli",
                b"bridge guest",
                &cli_sha256,
                &guest_sha256,
            )
            .unwrap();
        assert_eq!(fs::metadata(&guest_path).unwrap().ino(), guest_inode);
        assert!(
            store
                .is_bridge_cached_with_vm_guest(key, &cli_sha256, &guest_sha256)
                .unwrap()
        );
    }

    #[test]
    fn rejects_symlinked_bridge_directories_and_canonical_files() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        let cli_sha256 = sha256(b"bridge cli");
        let guest_sha256 = sha256(b"bridge guest");

        store.install("target", b"bridge cli").unwrap();
        store
            .install_bridge_with_vm_guest(
                "target",
                b"bridge cli",
                b"bridge guest",
                &cli_sha256,
                &guest_sha256,
            )
            .unwrap();
        symlink(store.version_dir("target"), store.version_dir("linked")).unwrap();
        assert!(
            !store
                .is_bridge_cached_with_vm_guest("linked", &cli_sha256, &guest_sha256)
                .unwrap()
        );
        assert!(
            store
                .install_bridge_with_vm_guest(
                    "linked",
                    b"bridge cli",
                    b"bridge guest",
                    &cli_sha256,
                    &guest_sha256,
                )
                .is_err()
        );

        for (index, name) in [
            BINARY_NAME,
            CHECKSUM_FILE,
            VM_GUEST_BINARY_NAME,
            VM_GUEST_CHECKSUM_FILE,
        ]
        .into_iter()
        .enumerate()
        {
            let key = format!("file-link-{index}");
            store.install(&key, b"bridge cli").unwrap();
            store
                .install_bridge_with_vm_guest(
                    &key,
                    b"bridge cli",
                    b"bridge guest",
                    &cli_sha256,
                    &guest_sha256,
                )
                .unwrap();
            let path = store.version_dir(&key).join(name);
            let saved = store.root.join(format!("saved-{index}"));
            fs::rename(&path, &saved).unwrap();
            symlink(&saved, &path).unwrap();

            assert!(
                !store
                    .is_bridge_cached_with_vm_guest(&key, &cli_sha256, &guest_sha256)
                    .unwrap(),
                "{name} symlink was accepted"
            );
            assert!(
                store
                    .activate_bridge_with_vm_guest(&key, &cli_sha256, &guest_sha256)
                    .is_err(),
                "{name} symlink was activated"
            );
        }
    }

    #[test]
    fn rejects_special_bridge_entries_without_blocking() {
        use std::os::unix::fs::FileTypeExt as _;
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        let key = "special-guest";
        let cli_sha256 = sha256(b"bridge cli");
        let guest_sha256 = sha256(b"bridge guest");
        store.install(key, b"bridge cli").unwrap();
        let guest_path = store.version_dir(key).join(VM_GUEST_BINARY_NAME);
        let _socket = UnixListener::bind(&guest_path).unwrap();

        assert!(
            !store
                .is_bridge_cached_with_vm_guest(key, &cli_sha256, &guest_sha256)
                .unwrap()
        );
        assert!(
            store
                .install_bridge_with_vm_guest(
                    key,
                    b"bridge cli",
                    b"bridge guest",
                    &cli_sha256,
                    &guest_sha256,
                )
                .is_err()
        );
        assert_eq!(
            fs::symlink_metadata(&guest_path)
                .unwrap()
                .file_type()
                .is_socket(),
            true
        );
    }

    #[test]
    fn deletion_after_validation_does_not_recreate_a_guest_only_directory() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        let key = "deleted-bridge";
        let cli_sha256 = sha256(b"bridge cli");
        let guest_sha256 = sha256(b"bridge guest");
        store.install(key, b"bridge cli").unwrap();

        let error = store
            .install_bridge_with_vm_guest_inner(
                key,
                b"bridge cli",
                b"bridge guest",
                &cli_sha256,
                &guest_sha256,
                |point| {
                    if point == BridgeMutationPoint::GuestWrite {
                        fs::remove_dir_all(store.version_dir(key)).unwrap();
                    }
                    Ok(())
                },
            )
            .unwrap_err();

        assert!(error.to_string().contains("recheck"));
        assert!(!store.version_dir(key).exists());
        assert!(!store.version_dir(key).join(VM_GUEST_BINARY_NAME).exists());
    }

    #[test]
    fn directory_exchange_after_temp_write_never_mutates_the_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        let key = "swapped-bridge";
        let displaced = store.versions_dir().join("swapped-bridge-old");
        let cli_sha256 = sha256(b"bridge cli");
        let guest_sha256 = sha256(b"bridge guest");
        store.install(key, b"bridge cli").unwrap();

        let error = store
            .install_bridge_with_vm_guest_inner(
                key,
                b"bridge cli",
                b"bridge guest",
                &cli_sha256,
                &guest_sha256,
                |point| {
                    if point == BridgeMutationPoint::GuestCommit {
                        fs::rename(store.version_dir(key), &displaced).unwrap();
                        store.install(key, b"replacement cli").unwrap();
                    }
                    Ok(())
                },
            )
            .unwrap_err();

        assert!(error.to_string().contains("no longer names"));
        assert_eq!(
            fs::read(store.binary_path(key)).unwrap(),
            b"replacement cli"
        );
        assert!(!store.version_dir(key).join(VM_GUEST_BINARY_NAME).exists());
        assert!(!displaced.join(VM_GUEST_BINARY_NAME).exists());
        assert!(fs::read_dir(&displaced).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".bridge-")
        }));
    }

    #[test]
    fn activation_rechecks_the_pinned_bridge_before_switching_current() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        let key = "activation-race";
        let displaced = store.versions_dir().join("activation-race-old");
        let cli_sha256 = sha256(b"bridge cli");
        let guest_sha256 = sha256(b"bridge guest");
        store.install("previous", b"previous").unwrap();
        store.activate("previous").unwrap();
        store.install(key, b"bridge cli").unwrap();
        store
            .install_bridge_with_vm_guest(
                key,
                b"bridge cli",
                b"bridge guest",
                &cli_sha256,
                &guest_sha256,
            )
            .unwrap();

        let error = store
            .activate_bridge_with_vm_guest_inner(key, &cli_sha256, &guest_sha256, |point| {
                if point == BridgeMutationPoint::Activation {
                    fs::rename(store.version_dir(key), &displaced).unwrap();
                    store.install(key, b"replacement cli").unwrap();
                }
                Ok(())
            })
            .unwrap_err();

        assert!(error.to_string().contains("no longer names"));
        assert_eq!(store.active().unwrap().as_deref(), Some("previous"));
    }

    #[test]
    fn exclusive_store_lock_covers_the_whole_locked_store_lifetime() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        let locked = store.lock_exclusive().unwrap();
        let (_, contender) = store.open_lock_file().unwrap();

        let error = contender.try_lock_exclusive().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::WouldBlock);
        drop(locked);
        contender.try_lock_exclusive().unwrap();
        contender.unlock().unwrap();
    }

    #[test]
    fn lock_rejects_symlinked_store_and_versions_directories() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let linked_root = parent.path().join("linked-root");
        symlink(outside.path(), &linked_root).unwrap();
        assert!(VersionStore::at(&linked_root).lock_exclusive().is_err());

        let root = parent.path().join("real-root");
        fs::create_dir(&root).unwrap();
        symlink(outside.path(), root.join("versions")).unwrap();
        assert!(VersionStore::at(&root).lock_exclusive().is_err());
    }

    #[test]
    fn locked_store_never_writes_through_replaced_store_directories() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("store");
        let displaced_root = parent.path().join("store-old");
        let store = VersionStore::at(&root);
        let locked = store.lock_exclusive().unwrap();

        fs::rename(&root, &displaced_root).unwrap();
        fs::create_dir_all(root.join("versions")).unwrap();
        assert!(locked.install("root-swap", b"payload").is_err());
        assert!(!root.join("versions/root-swap").exists());

        drop(locked);
        fs::remove_dir_all(&root).unwrap();
        fs::rename(&displaced_root, &root).unwrap();
        let locked = store.lock_exclusive().unwrap();
        let displaced_versions = root.join("versions-old");
        fs::rename(root.join("versions"), &displaced_versions).unwrap();
        fs::create_dir(root.join("versions")).unwrap();
        assert!(locked.install("versions-swap", b"payload").is_err());
        assert!(!root.join("versions/versions-swap").exists());
    }

    #[test]
    fn locked_store_fails_closed_if_the_named_lock_is_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        let locked = store.lock_exclusive().unwrap();
        let lock_path = directory.path().join(STORE_LOCK_FILE);
        fs::rename(&lock_path, directory.path().join("old-lock")).unwrap();
        File::create(&lock_path).unwrap();

        assert!(locked.install("split-lock", b"payload").is_err());
        assert!(!locked.version_dir("split-lock").exists());
    }
}
