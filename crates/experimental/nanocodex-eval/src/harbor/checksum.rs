use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use super::HarborError;

/// Matches `dirhash(directory, "sha256")`, which Harbor currently records as
/// `TrialResult.task_checksum`.
pub(crate) fn directory_hash(root: &Path) -> Result<String, HarborError> {
    let mut ancestors = HashSet::new();
    hash_directory(root, &mut ancestors)?.ok_or_else(|| HarborError::EmptyTask(root.to_path_buf()))
}

fn hash_directory(
    directory: &Path,
    ancestors: &mut HashSet<PathBuf>,
) -> Result<Option<String>, HarborError> {
    let canonical = fs::canonicalize(directory)?;
    if !ancestors.insert(canonical) {
        return Err(HarborError::CyclicTaskDirectory(directory.to_path_buf()));
    }

    let result = (|| {
        let mut descriptors = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::metadata(&path)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if metadata.is_dir() {
                if let Some(hash) = hash_directory(&path, ancestors)? {
                    descriptors.push(format!("dirhash:{hash}\0name:{name}"));
                }
            } else if metadata.is_file() {
                let hash = hex_digest(&fs::read(path)?);
                descriptors.push(format!("data:{hash}\0name:{name}"));
            }
        }
        if descriptors.is_empty() {
            return Ok(None);
        }
        descriptors.sort();
        Ok(Some(hex_digest(descriptors.join("\0\0").as_bytes())))
    })();

    ancestors.remove(&fs::canonicalize(directory)?);
    result
}

fn hex_digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::directory_hash;

    #[test]
    fn matches_harbor_directory_hash_for_the_fixture() {
        let task = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting");

        assert_eq!(
            directory_hash(&task).unwrap(),
            "eaa13434b21464b5a55c6a61b660c89fee3364084233f48311d29c143722390f"
        );
    }
}
