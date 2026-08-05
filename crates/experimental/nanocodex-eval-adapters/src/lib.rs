//! Format adapters for importing third-party benchmarks into `nanocodex-eval`.
//!
//! This crate owns upstream layout knowledge. It does not schedule agents,
//! prepare VM overlays, implement resume, or interpret benchmark-specific
//! scores. Adapters either preserve an existing task package, translate a
//! declarative format exactly, or require a benchmark-owned hermetic harness.

mod arena_hard;
mod external;
mod genebench_pro;
mod graphwalks;
mod harbor;
mod healthbench_professional;
mod mrcr;
mod openai_evals;
pub mod profile;
mod source;
mod swe_bench;

use std::{fs, path::Path};

pub use arena_hard::ArenaHard;
pub use external::ExternalHarness;
pub use genebench_pro::GeneBenchPro;
pub use graphwalks::GraphWalks;
pub use harbor::HarborDataset;
pub use healthbench_professional::HealthBenchProfessional;
pub use mrcr::Mrcr;
use nanocodex_eval::import::ImportError;
pub use openai_evals::OpenAiEvals;
use serde::Deserialize;
use sha2::{Digest, Sha256};
pub use source::{BuiltinSourceError, BuiltinSources};
pub use swe_bench::SweBench;

fn sha256_file(path: &Path) -> Result<String, ImportError> {
    let bytes = fs::read(path).map_err(|source| ImportError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn sha256_values(values: impl IntoIterator<Item = impl AsRef<[u8]>>) -> String {
    let mut digest = Sha256::new();
    for value in values {
        let value = value.as_ref();
        digest.update(Sha256::digest(value));
    }
    hex::encode(digest.finalize())
}

fn safe_case_id(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut separator = false;
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+') {
            output.push(char::from(byte));
            separator = false;
        } else if !separator && !output.is_empty() {
            output.push('-');
            separator = true;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() || output == "." || output == ".." {
        let digest = Sha256::digest(value.as_bytes());
        format!("case-{}", &hex::encode(digest)[..16])
    } else {
        output
    }
}

fn read_json_lines<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>, ImportError> {
    let text = fs::read_to_string(path).map_err(|source| ImportError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(line, value)| {
            serde_json::from_str(value).map_err(|source| {
                ImportError::Invalid(format!(
                    "failed to decode {} line {}: {source}",
                    path.display(),
                    line + 1
                ))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use nanocodex_eval::{
        NetworkPolicy, TaskOutput,
        import::{DatasetImporter, Environment, Harness, ImportStore},
    };
    use tempfile::tempdir;

    use sha2::{Digest as _, Sha256};

    use crate::{ArenaHard, ExternalHarness, GeneBenchPro, OpenAiEvals, SweBench};

    #[test]
    fn imports_arena_final_message_cases() {
        let source = tempdir().unwrap();
        let questions = source.path().join("question.jsonl");
        fs::write(
            &questions,
            r#"{"uid":"q-1","category":"hard","prompt":"Explain it."}
"#,
        )
        .unwrap();
        let harness = make_harness(source.path());
        let store = tempdir().unwrap();

        let dataset = ImportStore::new(store.path())
            .import(&ArenaHard::new(
                "arena-hard-v2",
                questions,
                "arena@abc",
                Environment::OciImage("debian:bookworm-slim".to_owned()),
                Harness::directory(harness).unwrap(),
            ))
            .unwrap();

        assert_eq!(dataset.tasks().len(), 1);
        assert_eq!(dataset.tasks()[0].prompt(), "Explain it.");
        assert_eq!(dataset.tasks()[0].output(), TaskOutput::FinalMessage);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &fs::read(dataset.tasks()[0].root().join("tests/case.json")).unwrap()
            )
            .unwrap()["category"],
            "hard"
        );
    }

    #[test]
    fn imports_genebench_data_only_into_the_candidate_environment() {
        let package = tempdir().unwrap();
        let problem = package.path().join("problems/case-1");
        fs::create_dir_all(problem.join("data_files")).unwrap();
        let config = br#"{
  "id": "case-1",
  "task": "Analyze the supplied data and return JSON.",
  "data_files": ["data_files/input.tsv.gz"],
  "ground_truth": {"value": 2},
  "grader": {"type": "numeric_tolerance", "config": {"key": "value"}}
}"#;
        let data = b"compressed-fixture";
        fs::write(problem.join("eval_config.json"), config).unwrap();
        fs::write(problem.join("data_files/input.tsv.gz"), data).unwrap();
        fs::write(
            package.path().join("reference_grader.py"),
            "# official grader\n",
        )
        .unwrap();
        let descriptor = |path: &str, bytes: &[u8]| {
            serde_json::json!({
                "path": path,
                "bytes": bytes.len(),
                "sha256": hex::encode(Sha256::digest(bytes)),
            })
        };
        fs::write(
            package.path().join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "problem_count": 1,
                "problems": [{
                    "eval_id": "case-1",
                    "eval_config": "problems/case-1/eval_config.json",
                    "files": [
                        descriptor("problems/case-1/eval_config.json", config),
                        descriptor("problems/case-1/data_files/input.tsv.gz", data),
                    ],
                }],
            }))
            .unwrap(),
        )
        .unwrap();
        let environment = package.path().join("candidate");
        fs::create_dir(&environment).unwrap();
        fs::write(
            environment.join("Dockerfile"),
            "FROM python:3.12-slim\nCOPY data_files /workspace/data_files\n",
        )
        .unwrap();
        let harness = make_harness(package.path());
        let store = tempdir().unwrap();

        let dataset = ImportStore::new(store.path())
            .import(&GeneBenchPro::new(
                package.path(),
                "openai/genebench@fixture",
                Environment::Dockerfile(environment),
                Harness::directory(harness).unwrap(),
            ))
            .unwrap();
        let task = &dataset.tasks()[0];

        assert_eq!(task.prompt(), "Analyze the supplied data and return JSON.");
        assert_eq!(task.output(), TaskOutput::FinalMessage);
        assert_eq!(task.network(), NetworkPolicy::Public);
        assert_eq!(
            fs::read(task.root().join("environment/data_files/input.tsv.gz")).unwrap(),
            data
        );
        assert!(task.root().join("tests/eval_config.json").is_file());
        assert!(task.root().join("tests/reference_grader.py").is_file());
        assert!(!task.root().join("environment/eval_config.json").exists());
    }

    #[test]
    fn imports_openai_match_without_reimplementing_custom_classes() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("evals")).unwrap();
        fs::create_dir_all(root.path().join("data/demo")).unwrap();
        fs::write(
            root.path().join("evals/demo.yaml"),
            r#"demo.match-v1:
  class: evals.elsuite.basic.match:Match
  args:
    samples_jsonl: demo/samples.jsonl
"#,
        )
        .unwrap();
        fs::write(
            root.path().join("data/demo/samples.jsonl"),
            r#"{"input":[{"role":"user","content":"2 + 2?"}],"ideal":["4","four"]}
"#,
        )
        .unwrap();
        let store = tempdir().unwrap();

        let dataset = ImportStore::new(store.path())
            .import(&OpenAiEvals::new(
                "openai-demo",
                root.path(),
                Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/openai-evals"),
                "demo.match-v1",
                "openai-evals@abc",
                Environment::OciImage("debian:bookworm-slim".to_owned()),
            ))
            .unwrap();

        assert_eq!(dataset.tasks()[0].prompt(), "2 + 2?");
        assert_eq!(dataset.tasks()[0].output(), TaskOutput::FinalMessage);
        assert!(
            dataset.tasks()[0]
                .root()
                .join("tests/expected.json")
                .is_file()
        );
    }

    #[test]
    fn imports_swe_bench_with_official_instance_image_identity() {
        let source = tempdir().unwrap();
        let instances = source.path().join("instances.jsonl");
        fs::write(
            &instances,
            r#"{"instance_id":"django__django-123","problem_statement":"Fix it.","repo":"django/django","base_commit":"abc","version":"5.0","patch":"diff","test_patch":"tests","FAIL_TO_PASS":["test"],"PASS_TO_PASS":[]}
"#,
        )
        .unwrap();
        let harness = make_harness(source.path());
        let store = tempdir().unwrap();

        let dataset = ImportStore::new(store.path())
            .import(&SweBench::new(
                "swe-verified",
                instances,
                "swe-bench@abc",
                "swebench",
                Harness::directory(harness).unwrap(),
            ))
            .unwrap();

        assert_eq!(dataset.tasks()[0].prompt(), "Fix it.");
        assert_eq!(
            dataset.tasks()[0].image().reference(),
            "swebench/sweb.eval.x86_64.django_1776_django-123:latest"
        );
        assert!(
            dataset.tasks()[0]
                .root()
                .join("tests/instance.json")
                .is_file()
        );
    }

    #[test]
    fn external_manifest_keeps_benchmark_harness_out_of_vm_policy() {
        let source = tempdir().unwrap();
        let harness = make_harness(source.path());
        fs::write(source.path().join("paper-1.json"), r#"{"paper":"1"}"#).unwrap();
        let manifest = source.path().join("paperbench.toml");
        fs::write(
            &manifest,
            format!(
                r#"schema_version = "1"
name = "paperbench"

[source]
kind = "paperbench"
revision = "paperbench@abc"

[[case]]
id = "paper-1"
prompt = "Reproduce the paper."
output = "workspace"
oci_image = "paperbench/environment@sha256:abc"
harness = {:?}

[[case.file]]
source = "paper-1.json"
destination = "case.json"
"#,
                harness.file_name().unwrap()
            ),
        )
        .unwrap();
        let store = tempdir().unwrap();

        let dataset = ImportStore::new(store.path())
            .import(&ExternalHarness::new(manifest))
            .unwrap();

        assert_eq!(dataset.source().kind(), "paperbench");
        assert_eq!(dataset.tasks()[0].prompt(), "Reproduce the paper.");
        assert_eq!(
            fs::read_to_string(dataset.tasks()[0].root().join("tests/case.json")).unwrap(),
            r#"{"paper":"1"}"#
        );
    }

    #[test]
    fn external_manifest_rejects_sources_outside_its_bundle() {
        let bundle = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let harness = make_harness(outside.path());
        let manifest = bundle.path().join("unsafe.toml");
        fs::write(
            &manifest,
            format!(
                r#"schema_version = "1"
name = "unsafe"

[source]
kind = "private"
revision = "private@abc"

[[case]]
id = "case"
prompt = "Do work."
output = "workspace"
oci_image = "debian:bookworm-slim"
harness = {:?}
"#,
                Path::new("..").join(harness.file_name().unwrap())
            ),
        )
        .unwrap();

        let error = ExternalHarness::new(manifest).plan().unwrap_err();
        assert!(error.to_string().contains("safe manifest-relative path"));
    }

    fn make_harness(root: &Path) -> std::path::PathBuf {
        let harness = root.join("harness");
        fs::create_dir(&harness).unwrap();
        fs::write(
            harness.join("test.sh"),
            "#!/bin/sh\nprintf '1\\n' > /logs/verifier/reward.txt\n",
        )
        .unwrap();
        harness
    }
}
