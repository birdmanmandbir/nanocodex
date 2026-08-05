use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use nanocodex_eval::profile::{BenchmarkSelection, ResolvedProfile};
use sha2::{Digest as _, Sha256};

use crate::{
    gdpval::{PARQUET_PATH as GDPVAL_PARQUET_PATH, asset_paths as gdpval_asset_paths},
    genebench_pro::decode_manifest,
    profile::Benchmark,
};

const TERMINAL_BENCH_REVISION: &str = "5c8eadf1f393183288fa08b8f73ca9a469cc5e00";
const ARENA_HARD_REVISION: &str = "196f6b826783b3da7310e361a805fa36f0be83f3";
const SWE_BENCH_REVISION: &str = "f7bbbb2ccdf479001d6467c9e34af59e44a840f9";
const SWE_ATLAS_REVISION: &str = "6de82c3603fb9e254170b440d7560441eb257176";
const GPQA_REVISION: &str = "56686c06f5e19865c153de0fdb11be3890014df7";
const GPQA_ZIP_SHA256: &str = "461ae7329f15a3e35f8184d2dac24b990f34fdf12f366ca4062d8e6638cd08dc";
const GPQA_DIAMOND_SHA256: &str =
    "41d1213cd7a4998605a26c2798500652572007161b3a92817ba46b35befcd305";
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
const MRCR_REVISION: &str = "f4c69fae7cf81f7ca26b9fee34b392a50f6b8a1d";
const HEALTHBENCH_PROFESSIONAL_REVISION: &str = "349962fd46dd02343a0d8a606491baf59154ea1a";
const HEALTHBENCH_PROFESSIONAL_SHA256: &str =
    "d44b08e6e952e04c945e2c406f02533d9e7a989a84e35820ee7efdff20c9e4e2";
pub(crate) const GDPVAL_REVISION: &str = "11e7900cdcac61bc4daf59e65feb238acda98fbf";
const GDPVAL_PARQUET_SHA256: &str =
    "f8422fab9b21d90c0ee5f0659842ab666d418cb8940842918f9f4b0df7ae0202";
const MRCR_FILES: [(&str, &str); 6] = [
    (
        "2needle/2needle_0.parquet",
        "1c297b254bf64a31856b74918cd7db889a214503e0b67daa834e84f20df6aa93",
    ),
    (
        "2needle/2needle_1.parquet",
        "a5a1dc9ccc945623253d04d33c03d89aee2d676c88955ce368da2ab16a0ce94d",
    ),
    (
        "4needle/4needle_0.parquet",
        "4d4fa3d11ce064749de3cd039eef1a621e30a81c2c9b3e64f1df37f8afeaf312",
    ),
    (
        "4needle/4needle_1.parquet",
        "8dfdb94a208cf3eee73c4e7ac6ee8a5ccb7236c6934c13c6c5f67c0a9928cdf3",
    ),
    (
        "8needle/8needle_0.parquet",
        "65df601a2e0ae4a3cfb56920a6ef99f26c0de37c6b1018695e8aed684e6a94c1",
    ),
    (
        "8needle/8needle_1.parquet",
        "c80b19573bff1d38e1c157d6a0bdf9cfd1a8ab6372296174c9a7015e164189e3",
    ),
];
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
            .iter()
            .filter(|(name, _)| Self::is_materialized(name))
            .map(|(name, selection)| (name.clone(), selection.clone()))
            .collect::<Vec<_>>();
        fs::create_dir_all(&self.root).map_err(|source| BuiltinSourceError::Io {
            path: self.root.clone(),
            source,
        })?;
        let mut jobs = tokio::task::JoinSet::new();
        for (name, selection) in selected {
            let this = self.clone();
            jobs.spawn_blocking(move || {
                tracing::info!(benchmark = %name, "preparing built-in benchmark source");
                let result = this.materialize(&name, &selection);
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
            "swe-atlas-qna" => Ok(Benchmark::SweAtlasQna {
                source: self.root.join("swe-atlas/data/qa"),
                revision: format!("scaleapi/SWE-Atlas@{SWE_ATLAS_REVISION}"),
            }),
            "gpqa-diamond" => Ok(Benchmark::GpqaDiamond {
                source: self.root.join("gpqa-data/gpqa_diamond.csv"),
                revision: format!("idavidrein/gpqa@{GPQA_REVISION}"),
                harness: assets.join("gpqa-diamond"),
                image: "python:3.12-slim".to_owned(),
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
            "mrcr-v2" => Ok(Benchmark::Mrcr {
                source: self.root.join("mrcr"),
                revision: format!("openai/mrcr@{MRCR_REVISION}"),
                harness: assets.join("mrcr"),
                image: "python:3.12-slim".to_owned(),
            }),
            "healthbench-professional" => Ok(Benchmark::HealthbenchProfessional {
                source: self
                    .root
                    .join("healthbench-professional/healthbench_professional_eval.jsonl"),
                revision: format!(
                    "openai/healthbench-professional@{HEALTHBENCH_PROFESSIONAL_REVISION}"
                ),
                harness: assets.join("healthbench-professional"),
                image: "python:3.12-slim".to_owned(),
            }),
            "gdpval" => Ok(Benchmark::Gdpval {
                source: self.root.join("gdpval"),
                revision: format!("openai/gdpval@{GDPVAL_REVISION}"),
                environment: assets.join("gdpval/environment"),
                harness: assets.join("gdpval/verifier"),
            }),
            other => Err(BuiltinSourceError::Unsupported(other.to_owned())),
        }
    }

    pub(crate) fn is_materialized(name: &str) -> bool {
        matches!(
            name,
            "terminal-bench-2.1"
                | "arena-hard-v2"
                | "swe-bench-verified-smoke"
                | "swe-atlas-qna"
                | "gpqa-diamond"
                | "genebench-pro-public"
                | "deep-swe-v1.1"
                | "graphwalks"
                | "mrcr-v2"
                | "healthbench-professional"
                | "gdpval"
        )
    }

    fn materialize(
        &self,
        name: &str,
        selection: &BenchmarkSelection,
    ) -> Result<(), BuiltinSourceError> {
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
            "swe-atlas-qna" => self.git_checkout(
                "swe-atlas",
                "https://github.com/scaleapi/SWE-Atlas.git",
                SWE_ATLAS_REVISION,
            ),
            "gpqa-diamond" => self.materialize_gpqa_diamond(),
            "genebench-pro-public" => self.materialize_genebench_pro(),
            "deep-swe-v1.1" => self.git_checkout(
                "deep-swe",
                "https://github.com/datacurve-ai/deep-swe.git",
                DEEP_SWE_REVISION,
            ),
            "graphwalks" => self.materialize_graphwalks(),
            "mrcr-v2" => self.materialize_mrcr(),
            "healthbench-professional" => self.download(
                "healthbench-professional/healthbench_professional_eval.jsonl",
                &format!(
                    "https://huggingface.co/datasets/openai/healthbench-professional/resolve/{HEALTHBENCH_PROFESSIONAL_REVISION}/healthbench_professional_eval.jsonl"
                ),
                HEALTHBENCH_PROFESSIONAL_SHA256,
            ),
            "gdpval" => self.materialize_gdpval(selection),
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

    fn materialize_mrcr(&self) -> Result<(), BuiltinSourceError> {
        let base = format!("https://huggingface.co/datasets/openai/mrcr/resolve/{MRCR_REVISION}");
        std::thread::scope(|scope| {
            let downloads = MRCR_FILES
                .into_iter()
                .map(|(relative, sha256)| {
                    let base = &base;
                    scope.spawn(move || {
                        self.download(
                            &format!("mrcr/{relative}"),
                            &format!("{base}/{relative}"),
                            sha256,
                        )
                    })
                })
                .collect::<Vec<_>>();
            for download in downloads {
                download.join().map_err(|panic| {
                    BuiltinSourceError::Command(format!("MRCR download worker panicked: {panic:?}"))
                })??;
            }
            Ok(())
        })
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

    fn materialize_gdpval(&self, selection: &BenchmarkSelection) -> Result<(), BuiltinSourceError> {
        self.git_checkout(
            "gdpval",
            "https://huggingface.co/datasets/openai/gdpval.git",
            GDPVAL_REVISION,
        )?;
        let parquet = self.root.join("gdpval").join(GDPVAL_PARQUET_PATH);
        validate_sha256(&parquet, GDPVAL_PARQUET_SHA256)?;
        let assets =
            gdpval_asset_paths(&parquet, (!selection.is_all()).then_some(selection.tasks()))
                .map_err(|error| BuiltinSourceError::Stale(error.to_string()))?;
        let workers = assets.len().min(16);
        let mut buckets = vec![Vec::new(); workers];
        for (index, asset) in assets.into_iter().enumerate() {
            buckets[index % workers].push(asset);
        }
        std::thread::scope(|scope| {
            let downloads = buckets
                .into_iter()
                .map(|bucket| {
                    scope.spawn(move || {
                        for asset in bucket {
                            let relative = asset.to_str().ok_or_else(|| {
                                BuiltinSourceError::Stale(format!(
                                    "GDPval asset path is not UTF-8: {}",
                                    asset.display()
                                ))
                            })?;
                            self.materialize_checkout_lfs_file(
                                "gdpval",
                                relative,
                                GDPVAL_REVISION,
                            )?;
                        }
                        Ok(())
                    })
                })
                .collect::<Vec<_>>();
            for download in downloads {
                download.join().map_err(|panic| {
                    BuiltinSourceError::Command(format!(
                        "GDPval download worker panicked: {panic:?}"
                    ))
                })??;
            }
            Ok(())
        })
    }

    fn materialize_gpqa_diamond(&self) -> Result<(), BuiltinSourceError> {
        self.git_checkout(
            "gpqa",
            "https://github.com/idavidrein/gpqa.git",
            GPQA_REVISION,
        )?;
        let archive = self.root.join("gpqa/dataset.zip");
        validate_sha256(&archive, GPQA_ZIP_SHA256)?;
        let destination = self.root.join("gpqa-data/gpqa_diamond.csv");
        if destination.is_file() {
            return validate_sha256(&destination, GPQA_DIAMOND_SHA256);
        }
        let output = Command::new("unzip")
            .args(["-p", "-P", "deserted-untie-orchid"])
            .arg(&archive)
            .arg("dataset/gpqa_diamond.csv")
            .output()
            .map_err(|error| {
                BuiltinSourceError::Command(format!("failed to run unzip: {error}"))
            })?;
        if !output.status.success() {
            return Err(BuiltinSourceError::Command(format!(
                "unzip exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        if hex::encode(Sha256::digest(&output.stdout)) != GPQA_DIAMOND_SHA256 {
            return Err(BuiltinSourceError::Stale(
                "extracted GPQA Diamond CSV does not match the pinned object".to_owned(),
            ));
        }
        let parent = destination.parent().unwrap_or(&self.root);
        fs::create_dir_all(parent).map_err(|source| BuiltinSourceError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(parent).map_err(|source| BuiltinSourceError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        temporary
            .write_all(&output.stdout)
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|source| BuiltinSourceError::Io {
                path: temporary.path().to_path_buf(),
                source,
            })?;
        temporary
            .persist(&destination)
            .map_err(|error| BuiltinSourceError::Io {
                path: destination,
                source: error.error,
            })?;
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
            let dirty = command_output(Command::new("git").arg("-C").arg(&destination).args([
                "status",
                "--porcelain=v1",
                "-z",
            ]))?;
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
        let records = status
            .split('\0')
            .filter(|record| !record.is_empty())
            .collect::<Vec<_>>();
        if checkout != "gdpval" || records.is_empty() {
            return Ok(false);
        }
        for record in records {
            let (status, relative) = record.split_at(3.min(record.len()));
            if !matches!(status, " M " | " D ") {
                return Ok(false);
            }
            let expected = self.checkout_object(checkout, relative)?.sha256;
            if status == " M " {
                let path = self.root.join(checkout).join(relative);
                if validate_sha256(&path, &expected).is_err() {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    fn materialize_checkout_lfs_file(
        &self,
        checkout: &str,
        relative: &str,
        revision: &str,
    ) -> Result<String, BuiltinSourceError> {
        let object = self.checkout_object(checkout, relative)?;
        let expected = object.sha256;
        let destination = self.root.join(checkout).join(relative);
        if destination.is_file() && validate_sha256(&destination, &expected).is_ok() {
            return Ok(expected);
        }
        if let Some(bytes) = object.git_bytes {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|source| BuiltinSourceError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            fs::write(&destination, bytes).map_err(|source| BuiltinSourceError::Io {
                path: destination.clone(),
                source,
            })?;
            validate_sha256(&destination, &expected)?;
            return Ok(expected);
        }
        if destination.exists() {
            let bytes = fs::read(&destination).map_err(|source| BuiltinSourceError::Io {
                path: destination.clone(),
                source,
            })?;
            let retained = String::from_utf8_lossy(&bytes);
            if parse_lfs_sha256(&retained).as_deref() != Some(expected.as_str()) {
                return Err(BuiltinSourceError::Stale(format!(
                    "{} is neither the pinned Git LFS pointer nor its object",
                    destination.display()
                )));
            }
            fs::remove_file(&destination).map_err(|source| BuiltinSourceError::Io {
                path: destination.clone(),
                source,
            })?;
        }
        let url = format!(
            "https://huggingface.co/datasets/openai/gdpval/resolve/{revision}/{}",
            encode_url_path(relative)
        );
        self.download(&format!("{checkout}/{relative}"), &url, &expected)?;
        Ok(expected)
    }

    fn checkout_object(
        &self,
        checkout: &str,
        relative: &str,
    ) -> Result<CheckoutObject, BuiltinSourceError> {
        let bytes = command_bytes(
            Command::new("git")
                .arg("-C")
                .arg(self.root.join(checkout))
                .arg("show")
                .arg(format!("HEAD:{relative}")),
        )?;
        if let Ok(pointer) = std::str::from_utf8(&bytes)
            && let Some(sha256) = parse_lfs_sha256(pointer)
        {
            Ok(CheckoutObject {
                sha256,
                git_bytes: None,
            })
        } else {
            Ok(CheckoutObject {
                sha256: hex::encode(Sha256::digest(&bytes)),
                git_bytes: Some(bytes),
            })
        }
    }
}

struct CheckoutObject {
    sha256: String,
    git_bytes: Option<Vec<u8>>,
}

fn parse_lfs_sha256(pointer: &str) -> Option<String> {
    if !pointer.starts_with("version https://git-lfs.github.com/spec/v1\n") {
        return None;
    }
    pointer.lines().find_map(|line| {
        let digest = line.strip_prefix("oid sha256:")?;
        (digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')))
        .then(|| digest.to_owned())
    })
}

fn encode_url_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(b"0123456789ABCDEF"[usize::from(byte >> 4)]));
            encoded.push(char::from(b"0123456789ABCDEF"[usize::from(byte & 0x0f)]));
        }
    }
    encoded
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

fn command_bytes(command: &mut Command) -> Result<Vec<u8>, BuiltinSourceError> {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .map_err(|error| BuiltinSourceError::Command(format!("{rendered}: {error}")))?;
    Ok(ensure_success(rendered, output)?.stdout)
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
