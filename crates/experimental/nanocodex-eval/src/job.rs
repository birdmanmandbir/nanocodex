use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{DateTime, Utc};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    EvalError,
    durable::scan_manifest_trials,
    sweep::{RunCoordinate, RunManifest},
};

const JOB_FILE: &str = "job.json";
const LOCK_FILE: &str = ".nanoeval.lock";
const RUN_FILE: &str = "run.json";

/// Stable metadata and native storage for one reusable evaluator.
#[derive(Clone, Debug)]
pub(crate) struct EvalJob {
    id: Uuid,
    started_at: DateTime<Utc>,
    directory: PathBuf,
    parent_directory: PathBuf,
    resumed: bool,
    _lease: Arc<JobLease>,
}

#[derive(Debug, Deserialize, Serialize)]
struct JobIdentity {
    id: Uuid,
    started_at: DateTime<Utc>,
}

#[derive(Debug)]
struct JobLease {
    file: File,
}

impl Drop for JobLease {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

impl EvalJob {
    pub(crate) fn create(parent_directory: &Path) -> Result<Self, EvalError> {
        let parent_directory = prepare_parent_directory(parent_directory)?;
        let id = Uuid::now_v7();
        let directory = parent_directory.join(id.to_string());
        create_durable_directory_all(&directory)?;
        let lease = Self::lease(&directory)?;
        let started_at = Utc::now();
        Self::write_json(&directory, JOB_FILE, &JobIdentity { id, started_at })?;
        Ok(Self {
            id,
            started_at,
            directory,
            parent_directory,
            resumed: false,
            _lease: Arc::new(lease),
        })
    }

    pub(crate) fn resume_or_create(
        parent_directory: &Path,
        run: &RunManifest,
    ) -> Result<Self, EvalError> {
        let parent_directory = prepare_parent_directory(parent_directory)?;
        let mut candidates = Vec::new();
        for entry in fs::read_dir(&parent_directory)? {
            let entry = entry?;
            let directory = entry.path();
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Ok(identity) = Self::read_json::<JobIdentity>(&directory.join(JOB_FILE)) else {
                continue;
            };
            let Ok(retained) = Self::read_json::<RunManifest>(&directory.join(RUN_FILE)) else {
                continue;
            };
            if !retained.is_compatible_with(run) {
                continue;
            }
            let completed = scan_manifest_trials(&directory, identity.id, run)
                .map_err(|error| EvalError::InvalidDurableTrial(error.to_string()))?
                .len();
            if completed < run.attempt_count() {
                candidates.push((identity.started_at, identity, directory));
            }
        }
        candidates.sort_unstable_by_key(|(started_at, _, _)| *started_at);

        let Some((_, identity, directory)) = candidates.pop() else {
            return Self::create(&parent_directory);
        };
        let lease = Self::lease(&directory).map_err(|error| match error {
            EvalError::Io(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                EvalError::RunActive(directory.clone())
            }
            other => other,
        })?;
        Ok(Self {
            id: identity.id,
            started_at: identity.started_at,
            directory,
            parent_directory,
            resumed: true,
            _lease: Arc::new(lease),
        })
    }

    #[must_use]
    pub const fn id(&self) -> Uuid {
        self.id
    }

    #[must_use]
    pub const fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    #[must_use]
    pub fn parent_directory(&self) -> &Path {
        &self.parent_directory
    }

    #[must_use]
    pub const fn resumed(&self) -> bool {
        self.resumed
    }

    pub fn bind_run(&self, run: &RunManifest) -> Result<(), EvalError> {
        self.bind_run_with_sync(run, sync_directory)
    }

    fn bind_run_with_sync<F>(
        &self,
        run: &RunManifest,
        mut sync_directory: F,
    ) -> Result<(), EvalError>
    where
        F: FnMut(&Path) -> io::Result<()>,
    {
        let path = self.directory.join(RUN_FILE);
        let encoded = serde_json::to_vec_pretty(run)?;
        if path.exists() {
            Self::verify_run(&path, run)?;
            sync_directory(&self.directory)?;
            return Ok(());
        }

        let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)?;
        temporary.write_all(&encoded)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(&path) {
            Ok(_) => {
                sync_directory(&self.directory)?;
                Ok(())
            }
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                Self::verify_run(&path, run)?;
                sync_directory(&self.directory)?;
                Ok(())
            }
            Err(error) => Err(error.error.into()),
        }
    }

    pub fn completed_coordinates(
        &self,
        run: &RunManifest,
    ) -> Result<BTreeSet<RunCoordinate>, EvalError> {
        scan_manifest_trials(&self.directory, self.id, run)
            .map(|trials| {
                trials
                    .into_iter()
                    .map(|trial| trial.coordinate().clone())
                    .collect()
            })
            .map_err(|error| EvalError::InvalidDurableTrial(error.to_string()))
    }

    fn verify_run(path: &Path, expected: &RunManifest) -> Result<(), EvalError> {
        let retained: RunManifest = serde_json::from_slice(&fs::read(path)?)?;
        if retained.is_compatible_with(expected) {
            Ok(())
        } else {
            Err(EvalError::RunConflict(path.to_path_buf()))
        }
    }

    fn lease(directory: &Path) -> Result<JobLease, EvalError> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(directory.join(LOCK_FILE))?;
        file.try_lock_exclusive()?;
        Ok(JobLease { file })
    }

    fn read_json<T>(path: &Path) -> Result<T, EvalError>
    where
        T: for<'de> Deserialize<'de>,
    {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }

    fn write_json(
        directory: &Path,
        filename: &str,
        value: &impl Serialize,
    ) -> Result<(), EvalError> {
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer_pretty(&mut temporary, value)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(directory.join(filename))
            .map_err(|error| error.error)?;
        sync_directory(directory)?;
        Ok(())
    }
}

fn prepare_parent_directory(path: &Path) -> io::Result<PathBuf> {
    require_durable_directory_sync()?;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    create_durable_directory_all(&absolute)?;
    fs::canonicalize(absolute)
}

fn create_durable_directory_all(path: &Path) -> io::Result<()> {
    create_durable_directory_all_with_sync(path, sync_directory)
}

fn create_durable_directory_all_with_sync<F>(path: &Path, mut sync_directory: F) -> io::Result<()>
where
    F: FnMut(&Path) -> io::Result<()>,
{
    let mut missing = Vec::new();
    let mut ancestor = path;
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "durable evaluation output ancestry must be symlink-free: {}",
                        ancestor.display()
                    ),
                ));
            }
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("path component is not a directory: {}", ancestor.display()),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(ancestor.to_path_buf());
                ancestor = ancestor.parent().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("directory has no existing ancestor: {}", path.display()),
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    }

    for directory in missing.into_iter().rev() {
        match fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&directory)?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "durable evaluation output ancestry must be symlink-free: {}",
                            directory.display()
                        ),
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }

    // Retrying after a failed fsync must not trust directory existence: a
    // prior call may have created an entry without durably committing it.
    // Revalidate the symlink-free contract and sync every parent to the root.
    for directory in path.ancestors() {
        let metadata = fs::symlink_metadata(directory)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "durable evaluation output ancestry must be symlink-free: {}",
                    directory.display()
                ),
            ));
        }
        if let Some(parent) = directory.parent() {
            sync_directory(parent)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "durable evaluation jobs require directory fsync support: {}",
            path.display()
        ),
    ))
}

#[cfg(unix)]
const fn require_durable_directory_sync() -> io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn require_durable_directory_sync() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "durable evaluation jobs require directory fsync support",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::PathBuf;

    use nanocodex_agent::Nanocodex;
    use tempfile::tempdir;

    use super::*;
    use crate::{Sweep, Task};

    #[test]
    fn creates_and_syncs_each_missing_output_directory_component() {
        let output = tempdir().unwrap();
        let first = output.path().join("one");
        let second = first.join("two");
        let nested = second.join("three");
        let mut synced = Vec::new();

        create_durable_directory_all_with_sync(&nested, |directory| {
            synced.push(directory.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert!(nested.is_dir());
        assert_eq!(&synced[..3], &[second, first, output.path().to_path_buf()]);
        let job = EvalJob::create(&nested).unwrap();
        assert_eq!(job.parent_directory(), fs::canonicalize(&nested).unwrap());
        assert!(job.directory().is_dir());
    }

    #[test]
    fn retries_parent_sync_for_an_existing_but_uncommitted_directory() {
        let output = tempdir().unwrap();
        let target = output.path().join("created-before-sync-failure");
        let error = create_durable_directory_all_with_sync(&target, |_| {
            Err(io::Error::other("injected sync failure"))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(target.is_dir());

        let mut retried_syncs = Vec::new();
        create_durable_directory_all_with_sync(&target, |directory| {
            retried_syncs.push(directory.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert_eq!(retried_syncs.first(), Some(&output.path().to_path_buf()));
    }

    #[test]
    fn rejects_symlinks_in_the_output_ancestry_without_following_them() {
        let output = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let redirect = output.path().join("redirect");
        std::os::unix::fs::symlink(outside.path(), &redirect).unwrap();

        let error = create_durable_directory_all_with_sync(&redirect.join("nested"), |_| Ok(()))
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!outside.path().join("nested").exists());
    }

    #[test]
    fn atomically_binds_one_finite_run() {
        let output = tempdir().unwrap();
        let job = EvalJob::create(output.path()).unwrap();
        assert!(!job.resumed());
        let first = sweep(1);
        let first_run = first.manifest();

        job.bind_run(&first_run).unwrap();
        job.bind_run(&first_run).unwrap();
        let retained: RunManifest =
            serde_json::from_slice(&fs::read(job.directory().join(RUN_FILE)).unwrap()).unwrap();
        assert_eq!(retained, first_run);

        let error = job.bind_run(&sweep(2).manifest()).unwrap_err();
        assert!(matches!(error, EvalError::RunConflict(_)));
    }

    #[test]
    fn binding_retry_syncs_an_existing_manifest_after_a_sync_failure() {
        let output = tempdir().unwrap();
        let job = EvalJob::create(output.path()).unwrap();
        let run = sweep(1).manifest();

        let error = job
            .bind_run_with_sync(&run, |_| Err(io::Error::other("injected sync failure")))
            .unwrap_err();
        assert!(matches!(error, EvalError::Io(error) if error.kind() == io::ErrorKind::Other));
        assert!(job.directory().join(RUN_FILE).is_file());

        let mut synced = Vec::new();
        job.bind_run_with_sync(&run, |directory| {
            synced.push(directory.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert_eq!(synced, [job.directory().to_path_buf()]);
    }

    #[test]
    fn resumes_the_latest_matching_incomplete_job() {
        let output = tempdir().unwrap();
        let run = sweep(2).manifest();
        let first = EvalJob::resume_or_create(output.path(), &run).unwrap();
        first.bind_run(&run).unwrap();
        let first_id = first.id();
        drop(first);

        let resumed = EvalJob::resume_or_create(output.path(), &run).unwrap();
        assert!(resumed.resumed());
        assert_eq!(resumed.id(), first_id);
    }

    #[test]
    fn resumes_a_legacy_manifest_without_task_names() {
        let output = tempdir().unwrap();
        let run = sweep(2).manifest();
        let legacy = manifest_with_task_name(&run, None);
        let first = EvalJob::create(output.path()).unwrap();
        first.bind_run(&legacy).unwrap();
        let first_id = first.id();
        drop(first);

        let resumed = EvalJob::resume_or_create(output.path(), &run).unwrap();
        assert!(resumed.resumed());
        assert_eq!(resumed.id(), first_id);
        resumed.bind_run(&run).unwrap();
    }

    #[test]
    fn does_not_resume_a_task_renamed_at_the_same_root() {
        let output = tempdir().unwrap();
        let run = sweep(2).manifest();
        let renamed = manifest_with_task_name(&run, Some("nanoeval/renamed-task"));
        let first = EvalJob::create(output.path()).unwrap();
        first.bind_run(&run).unwrap();
        let first_id = first.id();
        drop(first);

        let fresh = EvalJob::resume_or_create(output.path(), &renamed).unwrap();
        assert!(!fresh.resumed());
        assert_ne!(fresh.id(), first_id);
    }

    #[test]
    fn refuses_to_open_an_incomplete_job_that_is_still_active() {
        let output = tempdir().unwrap();
        let run = sweep(2).manifest();
        let active = EvalJob::resume_or_create(output.path(), &run).unwrap();
        active.bind_run(&run).unwrap();

        let error = EvalJob::resume_or_create(output.path(), &run).unwrap_err();
        assert!(
            matches!(error, EvalError::RunActive(directory) if directory == active.directory())
        );
    }

    #[test]
    fn recognizes_only_a_durable_terminal_trial_for_a_coordinate() {
        let output = tempdir().unwrap();
        let job = EvalJob::create(output.path()).unwrap();
        let sweep = sweep(2);
        let run = sweep.manifest();
        job.bind_run(&run).unwrap();
        let coordinate = sweep.attempts().next().unwrap().coordinate();
        let abandoned = job.directory().join("write-greeting__test__001__abandoned");
        fs::create_dir_all(&abandoned).unwrap();
        assert!(job.completed_coordinates(&run).unwrap().is_empty());

        let id = Uuid::now_v7();
        let compact_id = id.simple().to_string();
        let trial_name = format!("write-greeting__test__001__{}", &compact_id[..8]);
        let directory = job.directory().join(&trial_name);
        fs::create_dir(&directory).unwrap();
        let task = &sweep.tasks()[0];
        fs::write(
            directory.join("result.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "id": id,
                "task_name": task.name(),
                "trial_name": trial_name,
                "task_id": {
                    "path": task.root(),
                },
                "config": {
                    "task": {
                        "path": task.root(),
                    },
                    "trial_name": trial_name,
                    "trials_dir": job.directory(),
                    "job_id": job.id(),
                },
            }))
            .unwrap(),
        )
        .unwrap();

        let completed = job.completed_coordinates(&run).unwrap();
        assert_eq!(completed.len(), 1);
        assert!(completed.contains(&coordinate));
    }

    fn sweep(trials: u16) -> Sweep {
        Sweep::builder()
            .task(
                Task::load(
                    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"),
                )
                .unwrap(),
            )
            .trials(trials)
            .agent(
                "test",
                Nanocodex::builder(nanocodex_agent::OpenAi::new("test-key").unwrap()),
            )
            .unwrap()
            .build()
            .unwrap()
    }

    fn manifest_with_task_name(run: &RunManifest, name: Option<&str>) -> RunManifest {
        let mut retained = serde_json::to_value(run).unwrap();
        let task = retained["tasks"][0].as_object_mut().unwrap();
        if let Some(name) = name {
            task.insert("name".to_owned(), serde_json::json!(name));
        } else {
            task.remove("name");
        }
        serde_json::from_value(retained).unwrap()
    }
}

#[cfg(all(test, not(unix)))]
mod unsupported_platform_tests {
    use tempfile::tempdir;

    use super::EvalJob;
    use crate::EvalError;

    #[test]
    fn job_creation_fails_closed_without_durable_directory_sync() {
        let output = tempdir().unwrap();
        let target = output.path().join("new-output");

        let error = EvalJob::create(&target).unwrap_err();

        assert!(
            matches!(error, EvalError::Io(error) if error.kind() == std::io::ErrorKind::Unsupported)
        );
        assert!(!target.exists());
    }
}
