use std::path::Path;

use nanocodex_eval::Task;
use nanocodex_vm::image::{CachePolicy, ImageError, PreparedRootDisk, VmImageBuilder};

const BYTES_PER_MIB: u64 = 1024 * 1024;

pub(super) async fn prepare_task_image(
    builder: &VmImageBuilder,
    task: &Task,
    cache: &Path,
    policy: CachePolicy,
) -> Result<PreparedRootDisk, ImageError> {
    builder
        .prepare(
            task.environment_directory(),
            task.resources().storage_mb.saturating_mul(BYTES_PER_MIB),
            cache,
            policy,
        )
        .await
}

pub(super) async fn prepare_verifier_image(
    builder: &VmImageBuilder,
    task: &Task,
    cache: &Path,
    policy: CachePolicy,
) -> Result<PreparedRootDisk, ImageError> {
    builder
        .prepare(
            task.root().join("tests"),
            task.resources().storage_mb.saturating_mul(BYTES_PER_MIB),
            cache,
            policy,
        )
        .await
}
