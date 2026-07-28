use std::{
    fs, io,
    path::{Path, PathBuf},
};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use sha2::{Digest, Sha256};

const PACKAGE_FILES: [&str; 3] = ["task.toml", "instruction.md", "README.md"];
const PACKAGE_DIRECTORIES: [&str; 4] = ["environment", "tests", "solution", "steps"];

pub(crate) fn task_content_digest(root: &Path) -> io::Result<String> {
    let matcher = package_ignore_matcher(root)?;
    let mut files = Vec::new();
    for name in PACKAGE_FILES {
        let path = root.join(name);
        if path.is_file() && !is_ignored(root, &path, matcher.as_ref()) {
            files.push(path);
        }
    }
    for name in PACKAGE_DIRECTORIES {
        let directory = root.join(name);
        if directory.is_dir() {
            collect_package_files(root, &directory, matcher.as_ref(), &mut files)?;
        }
    }
    files.sort_by_key(|path| relative_name(root, path));

    let mut outer = Sha256::new();
    for path in files {
        let relative = relative_name(root, &path);
        let file_hash = hex_digest(&fs::read(&path)?);
        outer.update(relative.as_bytes());
        outer.update([0]);
        outer.update(file_hash.as_bytes());
        outer.update(b"\n");
    }
    Ok(hex::encode(outer.finalize()))
}

fn collect_package_files(
    root: &Path,
    directory: &Path,
    matcher: Option<&Gitignore>,
    files: &mut Vec<PathBuf>,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if is_ignored(root, &path, matcher) {
            continue;
        }
        if file_type.is_dir() {
            collect_package_files(root, &path, matcher, files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn package_ignore_matcher(root: &Path) -> io::Result<Option<Gitignore>> {
    let path = root.join(".gitignore");
    if !path.is_file() {
        return Ok(None);
    }
    let mut builder = GitignoreBuilder::new(root);
    builder.add(path);
    builder.build().map(Some).map_err(io::Error::other)
}

fn is_ignored(root: &Path, path: &Path, matcher: Option<&Gitignore>) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    if let Some(matcher) = matcher {
        return matcher
            .matched_path_or_any_parents(relative, path.is_dir())
            .is_ignore();
    }

    relative.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        name == "__pycache__"
            || name == ".DS_Store"
            || name.ends_with(".pyc")
            || name.ends_with(".swp")
            || name.ends_with(".swo")
            || name.ends_with('~')
    })
}

fn relative_name(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn hex_digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::task_content_digest;

    #[test]
    fn matches_harbors_package_hash_for_the_fixture() {
        let task = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting");

        assert_eq!(
            task_content_digest(&task).unwrap(),
            "e1a05661b2068b6f93e0874941d1fc930604d5c58965eacbc5cc4b4a95882d59"
        );
    }
}
