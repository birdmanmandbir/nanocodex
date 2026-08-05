use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use nanocodex_eval::profile::ResolvedProfile;
use sha2::{Digest as _, Sha256};

use crate::{genebench_pro::decode_manifest, profile::Benchmark};

const TERMINAL_BENCH_REVISION: &str = "5c8eadf1f393183288fa08b8f73ca9a469cc5e00";
const ARENA_HARD_REVISION: &str = "196f6b826783b3da7310e361a805fa36f0be83f3";
const OPENAI_EVALS_REVISION: &str = "8eac7a7de5215c907fbddc30efdaf316913eccdd";
const OPENAI_EVALS_SMOKE_DATA_SHA256: &str =
    "a15177151c46b4526e67e84f3292a036b6d5441d9eddc8a88403337395745866";
const SWE_BENCH_REVISION: &str = "f7bbbb2ccdf479001d6467c9e34af59e44a840f9";
const SWE_VERIFIED_ROW_RESPONSE_SHA256: &str =
    "7c62220a467830a3a330dda51211ab4c1ba099124dffc8371fbec057933c47b8";
const GENEBENCH_PRO_REVISION: &str = "eb75a3c0996b3cedcc9af685bad02fd166848fa2";
const GENEBENCH_PRO_MANIFEST_SHA256: &str =
    "0e80d5dca9ac5211fb9dfa5c0ea8d26e9d557e2039c8f20b0f5a328ea3cd6c58";
const GENEBENCH_PRO_GRADER_SHA256: &str =
    "81a50853d1348237300ce90a7b48a9230b4edb5d1af30207c37f17f0de8bbb28";
const DEEP_SWE_REVISION: &str = "e016041a6ccf8da29906afc9a3f5a8df940a1f78";
const GRAPHWALKS_REVISION: &str = "f338bb265735a56a79f4b0f5def722c9c3268ead";
const GRAPHWALKS_SHORT_SHA256: &str =
    "54036036c91d8e04bb2a5fcd9e36f8e2a852cacece5dfc2b1ee40e3a6182b516";
const GRAPHWALKS_LONG_SHA256: &str =
    "537879431c72a42e3b500f80efc3047e7facb90390b6063d33679b4320985911";
const GENEBENCH_PRO_BASE: &str =
    "https://huggingface.co/datasets/openai/genebench-pro-public-package/resolve";

/// Pinned, workspace-owned source material for built-in benchmark recipes.
#[derive(Clone, Debug)]
pub struct BuiltinSources {
    root: PathBuf,
}

/// Pinned benchmark source acquisition or validation failed.
#[derive(Debug, thiserror::Error)]
pub enum BuiltinSourceError {
    /// Filesystem operation failed.
    #[error("built-in source filesystem operation failed at {path}: {source}")]
    Io {
        /// Affected path.
        path: PathBuf,
        /// Filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// A source command failed.
    #[error("built-in source command failed: {0}")]
    Command(String),
    /// A retained source no longer has its pinned identity.
    #[error("built-in source is stale: {0}")]
    Stale(String),
    /// The catalog name has no executable recipe yet.
    #[error("built-in benchmark {0:?} has no materialized recipe")]
    Unsupported(String),
}

impl BuiltinSources {
    /// Places authoritative checkouts and data under one durable workspace root.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Materializes every independent built-in selected by a profile in parallel.
    pub async fn prepare(&self, profile: &ResolvedProfile) -> Result<(), BuiltinSourceError> {
        let selected = profile
            .selections()
            .keys()
            .filter(|name| Self::is_materialized(name))
            .cloned()
            .collect::<BTreeSet<_>>();
        fs::create_dir_all(&self.root).map_err(|source| BuiltinSourceError::Io {
            path: self.root.clone(),
            source,
        })?;
        let mut jobs = tokio::task::JoinSet::new();
        for name in selected {
            let this = self.clone();
            jobs.spawn_blocking(move || {
                tracing::info!(benchmark = %name, "preparing built-in benchmark source");
                let result = this.materialize(&name);
                if result.is_ok() {
                    tracing::info!(benchmark = %name, "prepared built-in benchmark source");
                }
                result
            });
        }
        while let Some(result) = jobs.join_next().await {
            result.map_err(|error| BuiltinSourceError::Command(error.to_string()))??;
        }
        Ok(())
    }

    /// Returns the concrete adapter recipe for one prepared built-in.
    pub fn benchmark(&self, name: &str) -> Result<Benchmark, BuiltinSourceError> {
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
        match name {
            "terminal-bench-2.1" => Ok(Benchmark::Harbor {
                source: self.root.join("terminal-bench-2-1/tasks"),
                revision: format!("harbor-framework/terminal-bench-2-1@{TERMINAL_BENCH_REVISION}"),
            }),
            "arena-hard-v2" => Ok(Benchmark::ArenaHard {
                questions: self
                    .root
                    .join("arena-hard-auto/data/arena-hard-v2.0/question.jsonl"),
                harness: assets.join("arena-hard"),
                baseline: Some(self.root.join(
                    "arena-hard-auto/data/arena-hard-v2.0/model_answer/o3-mini-2025-01-31.jsonl",
                )),
                revision: format!("lm-sys/arena-hard-auto@{ARENA_HARD_REVISION}"),
                image: "debian:bookworm-slim".to_owned(),
                limit: None,
            }),
            "openai-evals" => Ok(Benchmark::OpenaiEvals {
                registry: self.root.join("openai-evals/evals/registry"),
                harness: assets.join("openai-evals"),
                eval: "computer-science-problems.s1.simple-v0".to_owned(),
                revision: format!("openai/evals@{OPENAI_EVALS_REVISION}"),
                image: "debian:bookworm-slim".to_owned(),
            }),
            "swe-bench-verified-smoke" => Ok(Benchmark::SweBench {
                instances: self.root.join("data/swe-bench-verified-smoke.jsonl"),
                harness: assets.join("swe-bench"),
                revision:
                    "princeton-nlp/SWE-bench_Verified@c104f840cc67f8b6eec6f759ebc8b2693d585d4a"
                        .to_owned(),
                namespace: "swebench".to_owned(),
                architecture: "x86_64".to_owned(),
                image_tag: "latest".to_owned(),
            }),
            "genebench-pro-public" => Ok(Benchmark::GeneBenchPro {
                package: self.root.join("genebench-pro-public-package"),
                revision: format!("openai/genebench-pro-public-package@{GENEBENCH_PRO_REVISION}"),
                environment: assets.join("genebench-pro/environment"),
                harness: assets.join("genebench-pro/verifier"),
            }),
            "deep-swe-v1.1" => Ok(Benchmark::Harbor {
                source: self.root.join("deep-swe/tasks"),
                revision: format!("datacurve-ai/deep-swe@{DEEP_SWE_REVISION}"),
            }),
            "graphwalks" => Ok(Benchmark::Graphwalks {
                source: self.root.join("graphwalks"),
                revision: format!("openai/graphwalks@{GRAPHWALKS_REVISION}"),
                harness: assets.join("graphwalks"),
                image: "python:3.12-slim".to_owned(),
            }),
            other => Err(BuiltinSourceError::Unsupported(other.to_owned())),
        }
    }

    pub(crate) fn is_materialized(name: &str) -> bool {
        matches!(
            name,
            "terminal-bench-2.1"
                | "arena-hard-v2"
                | "openai-evals"
                | "swe-bench-verified-smoke"
                | "genebench-pro-public"
                | "deep-swe-v1.1"
                | "graphwalks"
        )
    }

    fn materialize(&self, name: &str) -> Result<(), BuiltinSourceError> {
        match name {
            "terminal-bench-2.1" => self.git_checkout(
                "terminal-bench-2-1",
                "https://github.com/harbor-framework/terminal-bench-2-1.git",
                TERMINAL_BENCH_REVISION,
            ),
            "arena-hard-v2" => self.git_checkout(
                "arena-hard-auto",
                "https://github.com/lm-sys/arena-hard-auto.git",
                ARENA_HARD_REVISION,
            ),
            "openai-evals" => {
                self.git_checkout(
                    "openai-evals",
                    "https://github.com/openai/evals.git",
                    OPENAI_EVALS_REVISION,
                )?;
                self.materialize_lfs_file(
                    "openai-evals/evals/registry/data/test_comp_sci/questions.jsonl",
                    "https://media.githubusercontent.com/media/openai/evals/8eac7a7de5215c907fbddc30efdaf316913eccdd/evals/registry/data/test_comp_sci/questions.jsonl",
                    OPENAI_EVALS_SMOKE_DATA_SHA256,
                )
            }
            "swe-bench-verified-smoke" => {
                self.git_checkout(
                    "swe-bench",
                    "https://github.com/SWE-bench/SWE-bench.git",
                    SWE_BENCH_REVISION,
                )?;
                let response = self
                    .root
                    .join("data/swe-bench-verified-smoke.response.json");
                self.download(
                    "data/swe-bench-verified-smoke.response.json",
                    "https://datasets-server.huggingface.co/rows?dataset=princeton-nlp/SWE-bench_Verified&config=default&split=test&offset=0&length=1",
                    SWE_VERIFIED_ROW_RESPONSE_SHA256,
                )?;
                let destination = self.root.join("data/swe-bench-verified-smoke.jsonl");
                let document: serde_json::Value =
                    serde_json::from_slice(&fs::read(&response).map_err(|source| {
                        BuiltinSourceError::Io {
                            path: response.clone(),
                            source,
                        }
                    })?)
                    .map_err(|error| BuiltinSourceError::Stale(error.to_string()))?;
                let row = document
                    .get("rows")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|rows| rows.first())
                    .and_then(|entry| entry.get("row"))
                    .ok_or_else(|| {
                        BuiltinSourceError::Stale(
                            "SWE-bench dataset response has no first row".to_owned(),
                        )
                    })?;
                let mut bytes = serde_json::to_vec(row)
                    .map_err(|error| BuiltinSourceError::Stale(error.to_string()))?;
                bytes.push(b'\n');
                if destination.is_file() {
                    let retained =
                        fs::read(&destination).map_err(|source| BuiltinSourceError::Io {
                            path: destination.clone(),
                            source,
                        })?;
                    if retained != bytes {
                        return Err(BuiltinSourceError::Stale(format!(
                            "{} does not match the pinned dataset row",
                            destination.display()
                        )));
                    }
                } else {
                    fs::write(&destination, bytes).map_err(|source| BuiltinSourceError::Io {
                        path: destination,
                        source,
                    })?;
                }
                Ok(())
            }
            "genebench-pro-public" => self.materialize_genebench_pro(),
            "deep-swe-v1.1" => self.git_checkout(
                "deep-swe",
                "https://github.com/datacurve-ai/deep-swe.git",
                DEEP_SWE_REVISION,
            ),
            "graphwalks" => self.materialize_graphwalks(),
            other => Err(BuiltinSourceError::Unsupported(other.to_owned())),
        }
    }

    fn materialize_graphwalks(&self) -> Result<(), BuiltinSourceError> {
        let base = format!(
            "https://huggingface.co/datasets/openai/graphwalks/resolve/{GRAPHWALKS_REVISION}"
        );
        self.download(
            "graphwalks/graphwalks_128k_and_shorter.parquet",
            &format!("{base}/graphwalks_128k_and_shorter.parquet"),
            GRAPHWALKS_SHORT_SHA256,
        )?;
        self.download(
            "graphwalks/graphwalks_256k_to_1mil.parquet",
            &format!("{base}/graphwalks_256k_to_1mil.parquet"),
            GRAPHWALKS_LONG_SHA256,
        )
    }

    fn materialize_genebench_pro(&self) -> Result<(), BuiltinSourceError> {
        let package = "genebench-pro-public-package";
        let manifest_relative = format!("{package}/manifest.json");
        let manifest_url = format!("{GENEBENCH_PRO_BASE}/{GENEBENCH_PRO_REVISION}/manifest.json");
        self.download(
            &manifest_relative,
            &manifest_url,
            GENEBENCH_PRO_MANIFEST_SHA256,
        )?;
        let grader_url =
            format!("{GENEBENCH_PRO_BASE}/{GENEBENCH_PRO_REVISION}/reference_grader.py");
        self.download(
            &format!("{package}/reference_grader.py"),
            &grader_url,
            GENEBENCH_PRO_GRADER_SHA256,
        )?;
        let manifest_path = self.root.join(&manifest_relative);
        let bytes = fs::read(&manifest_path).map_err(|source| BuiltinSourceError::Io {
            path: manifest_path.clone(),
            source,
        })?;
        let manifest =
            decode_manifest(&manifest_path, &bytes).map_err(BuiltinSourceError::Stale)?;
        for problem in manifest.problems {
            for file in problem.execution_files() {
                let relative_path = Path::new(&file.path);
                if relative_path.is_absolute()
                    || relative_path
                        .components()
                        .any(|component| !matches!(component, std::path::Component::Normal(_)))
                {
                    return Err(BuiltinSourceError::Stale(format!(
                        "GeneBench-Pro manifest contains unsafe path {:?}",
                        file.path
                    )));
                }
                let url = format!(
                    "{GENEBENCH_PRO_BASE}/{GENEBENCH_PRO_REVISION}/{}",
                    file.path
                );
                self.download(&format!("{package}/{}", file.path), &url, &file.sha256)?;
            }
        }
        Ok(())
    }

    fn git_checkout(
        &self,
        relative: &str,
        url: &str,
        revision: &str,
    ) -> Result<(), BuiltinSourceError> {
        let destination = self.root.join(relative);
        if destination.exists() {
            let head = command_output(
                Command::new("git")
                    .arg("-C")
                    .arg(&destination)
                    .args(["rev-parse", "HEAD"]),
            )?;
            if head.trim() != revision {
                return Err(BuiltinSourceError::Stale(format!(
                    "{} is at {}, expected {}; remove it and prepare again",
                    destination.display(),
                    head.trim(),
                    revision
                )));
            }
            let dirty = command_output(
                Command::new("git")
                    .arg("-C")
                    .arg(&destination)
                    .args(["status", "--porcelain"]),
            )?;
            if !dirty.trim().is_empty() && !self.allowed_materialized_lfs(relative, &dirty)? {
                return Err(BuiltinSourceError::Stale(format!(
                    "{} has local changes; built-in inputs must be immutable",
                    destination.display()
                )));
            }
            return Ok(());
        }
        let parent = destination.parent().unwrap_or(&self.root);
        fs::create_dir_all(parent).map_err(|source| BuiltinSourceError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let temporary = tempfile::Builder::new()
            .prefix(".source-")
            .tempdir_in(parent)
            .map_err(|source| BuiltinSourceError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        command_status(Command::new("git").arg("init").arg(temporary.path()))?;
        command_status(Command::new("git").arg("-C").arg(temporary.path()).args([
            "fetch",
            "--depth=1",
            url,
            revision,
        ]))?;
        command_status(
            Command::new("git")
                .arg("-C")
                .arg(temporary.path())
                .args(["checkout", "--detach", "FETCH_HEAD"])
                .env("GIT_LFS_SKIP_SMUDGE", "1"),
        )?;
        fs::rename(temporary.keep(), &destination).map_err(|source| BuiltinSourceError::Io {
            path: destination,
            source,
        })
    }

    fn download(
        &self,
        relative: &str,
        url: &str,
        expected_sha256: &str,
    ) -> Result<(), BuiltinSourceError> {
        let destination = self.root.join(relative);
        if destination.is_file() {
            return validate_sha256(&destination, expected_sha256);
        }
        let parent = destination.parent().unwrap_or(&self.root);
        fs::create_dir_all(parent).map_err(|source| BuiltinSourceError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let temporary =
            tempfile::NamedTempFile::new_in(parent).map_err(|source| BuiltinSourceError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        command_status(
            Command::new("curl")
                .args([
                    "--fail",
                    "--location",
                    "--silent",
                    "--show-error",
                    "--output",
                ])
                .arg(temporary.path())
                .arg(url),
        )?;
        validate_sha256(temporary.path(), expected_sha256)?;
        temporary
            .persist(&destination)
            .map_err(|error| BuiltinSourceError::Io {
                path: destination,
                source: error.error,
            })?;
        Ok(())
    }

    fn allowed_materialized_lfs(
        &self,
        checkout: &str,
        status: &str,
    ) -> Result<bool, BuiltinSourceError> {
        if checkout != "openai-evals"
            || status.trim() != "M evals/registry/data/test_comp_sci/questions.jsonl"
        {
            return Ok(false);
        }
        let path = self
            .root
            .join("openai-evals/evals/registry/data/test_comp_sci/questions.jsonl");
        let bytes = fs::read(&path).map_err(|source| BuiltinSourceError::Io { path, source })?;
        Ok(hex::encode(Sha256::digest(bytes)) == OPENAI_EVALS_SMOKE_DATA_SHA256)
    }

    fn materialize_lfs_file(
        &self,
        relative: &str,
        url: &str,
        sha256: &str,
    ) -> Result<(), BuiltinSourceError> {
        let destination = self.root.join(relative);
        let retained = fs::read(&destination).map_err(|source| BuiltinSourceError::Io {
            path: destination.clone(),
            source,
        })?;
        if hex::encode(Sha256::digest(&retained)) == sha256 {
            return Ok(());
        }
        let pointer = String::from_utf8_lossy(&retained);
        if !pointer.starts_with("version https://git-lfs.github.com/spec/v1\n")
            || !pointer.contains(&format!("oid sha256:{sha256}\n"))
        {
            return Err(BuiltinSourceError::Stale(format!(
                "{} is neither the pinned Git LFS pointer nor its materialized object",
                destination.display()
            )));
        }
        fs::remove_file(&destination).map_err(|source| BuiltinSourceError::Io {
            path: destination.clone(),
            source,
        })?;
        self.download(relative, url, sha256)
    }
}

fn validate_sha256(path: &Path, expected: &str) -> Result<(), BuiltinSourceError> {
    let bytes = fs::read(path).map_err(|source| BuiltinSourceError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let actual = hex::encode(Sha256::digest(bytes));
    if actual == expected {
        Ok(())
    } else {
        Err(BuiltinSourceError::Stale(format!(
            "{} has digest {actual}, expected {expected}",
            path.display()
        )))
    }
}

fn command_status(command: &mut Command) -> Result<(), BuiltinSourceError> {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .map_err(|error| BuiltinSourceError::Command(format!("{rendered}: {error}")))?;
    ensure_success(rendered, output).map(drop)
}

fn command_output(command: &mut Command) -> Result<String, BuiltinSourceError> {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .map_err(|error| BuiltinSourceError::Command(format!("{rendered}: {error}")))?;
    let output = ensure_success(rendered, output)?;
    String::from_utf8(output.stdout).map_err(|error| BuiltinSourceError::Command(error.to_string()))
}

fn ensure_success(rendered: String, output: Output) -> Result<Output, BuiltinSourceError> {
    if output.status.success() {
        Ok(output)
    } else {
        Err(BuiltinSourceError::Command(format!(
            "{rendered} exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}
