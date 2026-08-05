//! Format adapters for importing third-party benchmarks into `nanocodex-eval`.
//!
//! This crate owns upstream layout knowledge. It does not schedule agents,
//! prepare VM overlays, implement resume, or interpret benchmark-specific
//! scores. Adapters either preserve an existing task package, translate a
//! declarative format exactly, or require a benchmark-owned hermetic harness.

mod arena_hard;
mod external;
mod harbor;
mod openai_evals;
mod openai_simple_evals;
pub mod profile;
mod source;
mod swe_bench;

use std::{fs, path::Path};

pub use arena_hard::ArenaHard;
pub use external::ExternalHarness;
pub use harbor::HarborDataset;
use nanocodex_eval::import::ImportError;
pub use openai_evals::OpenAiEvals;
pub use openai_simple_evals::{OpenAiSimpleEval, OpenAiSimpleEvals};
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

    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use nanocodex_eval::{
        TaskOutput,
        import::{DatasetImporter, Environment, Harness, ImportStore},
    };
    use tempfile::tempdir;

    use crate::{
        ArenaHard, ExternalHarness, OpenAiEvals, OpenAiSimpleEval, OpenAiSimpleEvals, SweBench,
    };
    use sha2::{Digest, Sha256};

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
    fn imports_openai_simple_evals_with_official_grader_sources() {
        let source = tempdir().unwrap();
        make_simple_evals_checkout(source.path());
        let health = source.path().join("health.jsonl");
        fs::write(
            &health,
            r#"{"prompt":[{"role":"user","content":"How should I proceed?"}],"rubrics":[{"criterion":"Is helpful","points":1,"tags":[]}],"example_tags":[],"prompt_id":"health-1"}
"#,
        )
        .unwrap();
        let store = tempdir().unwrap();

        let dataset = ImportStore::new(store.path())
            .import(&OpenAiSimpleEvals::new(
                "healthbench",
                source.path(),
                openai_simple_evals_harness(),
                health,
                "openai/simple-evals@abc",
                OpenAiSimpleEval::HealthBench,
                Environment::OciImage("debian:bookworm-slim".to_owned()),
            ))
            .unwrap();

        let task = &dataset.tasks()[0];
        assert_eq!(task.prompt(), "How should I proceed?");
        assert_eq!(task.output(), TaskOutput::FinalMessage);
        assert!(
            task.root()
                .join("tests/official/healthbench_eval.py")
                .is_file()
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &fs::read(task.root().join("tests/case.json")).unwrap()
            )
            .unwrap()["kind"],
            "healthbench"
        );
    }

    #[test]
    fn imports_browsecomp_and_gpqa_reference_prompt_shapes() {
        let source = tempdir().unwrap();
        make_simple_evals_checkout(source.path());
        let browse = source.path().join("browse.csv");
        let canary = "fixture-canary";
        fs::write(
            &browse,
            format!(
                "problem,answer,problem_topic,canary\n{},{},fixture,{}\n",
                encrypt("Who wrote Hamlet?", canary),
                encrypt("William Shakespeare", canary),
                canary
            ),
        )
        .unwrap();
        let gpqa = source.path().join("gpqa.csv");
        fs::write(
            &gpqa,
            "Question,Correct Answer,Incorrect Answer 1,Incorrect Answer 2,Incorrect Answer 3\nWhat is 2+2?,4,3,5,6\n",
        )
        .unwrap();
        let store = tempdir().unwrap();

        let browse = ImportStore::new(store.path())
            .import(&OpenAiSimpleEvals::new(
                "browsecomp",
                source.path(),
                openai_simple_evals_harness(),
                browse,
                "openai/simple-evals@abc",
                OpenAiSimpleEval::BrowseComp,
                Environment::OciImage("debian:bookworm-slim".to_owned()),
            ))
            .unwrap();
        let gpqa = ImportStore::new(store.path())
            .import(&OpenAiSimpleEvals::new(
                "gpqa",
                source.path(),
                openai_simple_evals_harness(),
                gpqa,
                "openai/simple-evals@abc",
                OpenAiSimpleEval::GpqaDiamond,
                Environment::OciImage("debian:bookworm-slim".to_owned()),
            ))
            .unwrap();

        assert!(browse.tasks()[0].prompt().contains("Who wrote Hamlet?"));
        assert!(
            browse.tasks()[0]
                .prompt()
                .contains("Exact Answer: {your succinct, final answer}")
        );
        assert!(gpqa.tasks()[0].prompt().contains("Answer: $LETTER"));
        assert!(gpqa.tasks()[0].prompt().contains("A)"));
        assert_eq!(gpqa.tasks().len(), 4);
        assert!(
            gpqa.tasks()[0]
                .root()
                .join("tests/official/gpqa_eval.py")
                .is_file()
        );
    }

    #[test]
    fn imports_healthbench_professional_with_published_scoring_mode() {
        let source = tempdir().unwrap();
        make_simple_evals_checkout(source.path());
        let data = source.path().join("healthbench-professional.jsonl");
        fs::write(
            &data,
            r#"{"id":"case-1","conversation":{"messages":[{"role":"user","content":"Draft a clinical note."}]},"rubric_items":[{"criterion_text":"Includes an assessment.","points":8}],"use_case":"writing","type":"good_faith","difficulty":"typical","specialty":"general"}
"#,
        )
        .unwrap();
        let store = tempdir().unwrap();

        let dataset = ImportStore::new(store.path())
            .import(&OpenAiSimpleEvals::new(
                "healthbench-professional",
                source.path(),
                openai_simple_evals_harness(),
                data,
                "openai/simple-evals@abc",
                OpenAiSimpleEval::HealthBenchProfessional,
                Environment::OciImage("debian:bookworm-slim".to_owned()),
            ))
            .unwrap();

        let metadata: serde_json::Value = serde_json::from_slice(
            &fs::read(dataset.tasks()[0].root().join("tests/case.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(metadata["kind"], "healthbench_professional");
        assert_eq!(
            metadata["rubrics"][0]["criterion"],
            "Includes an assessment."
        );
        assert!(
            dataset.tasks()[0]
                .root()
                .join("tests/official/sampler/responses_sampler.py")
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

    fn make_simple_evals_checkout(root: &Path) {
        fs::create_dir(root.join("sampler")).unwrap();
        for relative in [
            "LICENSE",
            "browsecomp_eval.py",
            "common.py",
            "gpqa_eval.py",
            "healthbench_eval.py",
            "types.py",
            "sampler/chat_completion_sampler.py",
            "sampler/responses_sampler.py",
        ] {
            fs::write(root.join(relative), format!("# fixture {relative}\n")).unwrap();
        }
    }

    fn openai_simple_evals_harness() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/openai-simple-evals")
    }

    fn encrypt(value: &str, password: &str) -> String {
        let key = Sha256::digest(password.as_bytes());
        let encrypted = value
            .as_bytes()
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ key[index % key.len()])
            .collect::<Vec<_>>();
        BASE64.encode(encrypted)
    }
}
