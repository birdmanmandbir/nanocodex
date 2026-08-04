//! Cross-process control and status for one profile run.

use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Utc};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

const STATE_VERSION: u32 = 1;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Durable status for the current or most recent invocation of one profile.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProfileRunStatus {
    version: u32,
    profile: String,
    pid: u32,
    phase: ProfileRunPhase,
    planned_attempts: usize,
    job_directory: Option<PathBuf>,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
}

/// Durable profile coordinator phase.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileRunPhase {
    /// Coordinator owns the lease and may admit work.
    Running,
    /// A stop was requested and admitted work is draining.
    Stopping,
    /// Every selected coordinate completed.
    Completed,
    /// The coordinator exited before completing the selected matrix.
    Interrupted,
}

/// Cross-process profile run controller rooted in one profile state directory.
#[derive(Clone, Debug)]
pub struct ProfileRunControl {
    root: PathBuf,
}

/// Exclusive active-run lease. Dropping an unfinished lease marks it interrupted.
pub struct ActiveProfileRun {
    control: ProfileRunControl,
    lock: File,
    status: ProfileRunStatus,
    finished: bool,
}

/// Profile run control I/O or lifecycle failure.
#[derive(Debug, thiserror::Error)]
pub enum ProfileRunControlError {
    /// Another coordinator owns the profile lease.
    #[error("profile {0:?} is already running")]
    AlreadyRunning(String),
    /// No coordinator owns the profile lease.
    #[error("profile {0:?} is not running")]
    NotRunning(String),
    /// Control-state I/O failed.
    #[error("profile run control I/O failed at {path}: {source}")]
    Io {
        /// Affected path.
        path: PathBuf,
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// Control-state JSON failed.
    #[error("profile run control JSON failed at {path}: {source}")]
    Json {
        /// Affected path.
        path: PathBuf,
        /// JSON failure.
        #[source]
        source: serde_json::Error,
    },
}

impl ProfileRunControl {
    /// Opens the controller rooted beside a profile's evaluator jobs.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Acquires the exclusive coordinator lease and publishes running state.
    pub fn acquire(
        &self,
        profile: impl Into<String>,
        planned_attempts: usize,
    ) -> Result<ActiveProfileRun, ProfileRunControlError> {
        fs::create_dir_all(&self.root).map_err(|source| self.io(self.root.clone(), source))?;
        let lock_path = self.root.join("run.lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| self.io(lock_path, source))?;
        let profile = profile.into();
        lock.try_lock_exclusive().map_err(|source| {
            if source.kind() == io::ErrorKind::WouldBlock {
                ProfileRunControlError::AlreadyRunning(profile.clone())
            } else {
                self.io(self.root.join("run.lock"), source)
            }
        })?;
        remove_if_exists(&self.stop_path()).map_err(|source| self.io(self.stop_path(), source))?;
        let status = ProfileRunStatus {
            version: STATE_VERSION,
            profile,
            pid: std::process::id(),
            phase: ProfileRunPhase::Running,
            planned_attempts,
            job_directory: None,
            started_at: Utc::now(),
            finished_at: None,
        };
        self.write_status(&status)?;
        Ok(ActiveProfileRun {
            control: self.clone(),
            lock,
            status,
            finished: false,
        })
    }

    /// Loads the durable status, if this profile has ever run.
    pub fn status(&self) -> Result<Option<ProfileRunStatus>, ProfileRunControlError> {
        let path = self.status_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(self.io(path, source)),
        };
        let mut status: ProfileRunStatus = serde_json::from_slice(&bytes)
            .map_err(|source| ProfileRunControlError::Json { path, source })?;
        if matches!(
            status.phase,
            ProfileRunPhase::Running | ProfileRunPhase::Stopping
        ) && !self.lease_is_active()?
        {
            status.phase = ProfileRunPhase::Interrupted;
            status.finished_at = Some(Utc::now());
            self.write_status(&status)?;
        }
        Ok(Some(status))
    }

    /// Requests graceful drain from the active coordinator.
    pub fn request_stop(&self) -> Result<ProfileRunStatus, ProfileRunControlError> {
        let mut status = self
            .status()?
            .ok_or_else(|| ProfileRunControlError::NotRunning(self.profile_name()))?;
        let lock_path = self.root.join("run.lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| self.io(lock_path, source))?;
        match lock.try_lock_exclusive() {
            Ok(()) => return Err(ProfileRunControlError::NotRunning(status.profile)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(source) => return Err(self.io(self.root.join("run.lock"), source)),
        }
        write_atomic(&self.stop_path(), b"stop\n")
            .map_err(|source| self.io(self.stop_path(), source))?;
        status.phase = ProfileRunPhase::Stopping;
        self.write_status(&status)?;
        Ok(status)
    }

    fn write_status(&self, status: &ProfileRunStatus) -> Result<(), ProfileRunControlError> {
        let path = self.status_path();
        let bytes =
            serde_json::to_vec_pretty(status).map_err(|source| ProfileRunControlError::Json {
                path: path.clone(),
                source,
            })?;
        write_atomic(&path, &bytes).map_err(|source| self.io(path, source))
    }

    fn status_path(&self) -> PathBuf {
        self.root.join("status.json")
    }

    fn stop_path(&self) -> PathBuf {
        self.root.join("stop.request")
    }

    fn profile_name(&self) -> String {
        self.root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_owned()
    }

    fn lease_is_active(&self) -> Result<bool, ProfileRunControlError> {
        let path = self.root.join("run.lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| self.io(path.clone(), source))?;
        match lock.try_lock_exclusive() {
            Ok(()) => {
                lock.unlock().map_err(|source| self.io(path, source))?;
                Ok(false)
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
            Err(source) => Err(self.io(path, source)),
        }
    }

    const fn io(&self, path: PathBuf, source: io::Error) -> ProfileRunControlError {
        ProfileRunControlError::Io { path, source }
    }
}

impl ActiveProfileRun {
    /// Publishes the UUID job directory once the evaluator opens it.
    pub fn job_directory(
        &mut self,
        directory: impl Into<PathBuf>,
    ) -> Result<(), ProfileRunControlError> {
        self.status.job_directory = Some(directory.into());
        if self.control.stop_path().is_file() {
            self.status.phase = ProfileRunPhase::Stopping;
        }
        self.control.write_status(&self.status)
    }

    /// Waits until another process requests graceful stop.
    pub async fn wait_for_stop(&self) -> Result<(), ProfileRunControlError> {
        loop {
            if self.control.stop_path().is_file() {
                return Ok(());
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Publishes successful completion and releases the lease on drop.
    pub fn complete(mut self) -> Result<ProfileRunStatus, ProfileRunControlError> {
        self.status.phase = ProfileRunPhase::Completed;
        self.status.finished_at = Some(Utc::now());
        self.control.write_status(&self.status)?;
        remove_if_exists(&self.control.stop_path())
            .map_err(|source| self.control.io(self.control.stop_path(), source))?;
        self.finished = true;
        Ok(self.status.clone())
    }
}

impl Drop for ActiveProfileRun {
    fn drop(&mut self) {
        if !self.finished {
            self.status.phase = ProfileRunPhase::Interrupted;
            self.status.finished_at = Some(Utc::now());
            let _ = self.control.write_status(&self.status);
        }
        let _ = self.lock.unlock();
    }
}

impl ProfileRunStatus {
    /// Stable profile name.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Coordinator process ID.
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Current durable lifecycle phase.
    pub const fn phase(&self) -> ProfileRunPhase {
        self.phase
    }

    /// Total selected coordinates.
    pub const fn planned_attempts(&self) -> usize {
        self.planned_attempts
    }

    /// Current retained evaluator job, once opened.
    pub fn job_directory(&self) -> Option<&Path> {
        self.job_directory.as_deref()
    }
}

impl std::fmt::Display for ProfileRunStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}: {:?} pid={} planned={}",
            self.profile, self.phase, self.pid, self.planned_attempts
        )?;
        if let Some(job) = &self.job_directory {
            write!(formatter, " job={}", job.display())?;
        }
        Ok(())
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("control path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write as _;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lease_rejects_duplicates_and_stop_is_durable() {
        let root = tempfile::tempdir().unwrap();
        let control = ProfileRunControl::new(root.path().join("smoke"));
        let mut active = control.acquire("smoke", 12).unwrap();
        assert!(matches!(
            control.acquire("smoke", 12),
            Err(ProfileRunControlError::AlreadyRunning(_))
        ));
        active.job_directory("job/one").unwrap();
        let stopped = control.request_stop().unwrap();
        assert_eq!(stopped.phase(), ProfileRunPhase::Stopping);
        active.wait_for_stop().await.unwrap();
        drop(active);
        assert_eq!(
            control.status().unwrap().unwrap().phase(),
            ProfileRunPhase::Interrupted
        );
    }

    #[test]
    fn completed_lease_rejects_late_stop() {
        let root = tempfile::tempdir().unwrap();
        let control = ProfileRunControl::new(root.path().join("smoke"));
        let active = control.acquire("smoke", 1).unwrap();
        active.complete().unwrap();
        assert!(matches!(
            control.request_stop(),
            Err(ProfileRunControlError::NotRunning(_))
        ));
    }
}
