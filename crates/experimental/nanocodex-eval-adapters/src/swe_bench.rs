use std::path::PathBuf;

use serde::Deserialize;

use nanocodex_eval::import::{
    CasePlan, DatasetImporter, DatasetPlan, Environment, Harness, ImportError, SourceIdentity,
};

use crate::{read_json_lines, safe_case_id, sha256_file, sha256_values};

/// SWE-bench instance importer using official prebuilt instance images and a
/// caller-packaged official evaluation harness.
#[derive(Clone, Debug)]
pub struct SweBench {
    name: Box<str>,
    instances: PathBuf,
    revision: Box<str>,
    namespace: Box<str>,
    architecture: Box<str>,
    image_tag: Box<str>,
    harness: Harness,
}

impl SweBench {
    /// Creates an importer for SWE-bench dataset JSONL.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        instances_jsonl: impl Into<PathBuf>,
        revision: impl Into<String>,
        namespace: impl Into<String>,
        harness: Harness,
    ) -> Self {
        Self {
            name: name.into().into_boxed_str(),
            instances: instances_jsonl.into(),
            revision: revision.into().into_boxed_str(),
            namespace: namespace.into().into_boxed_str(),
            architecture: "x86_64".into(),
            image_tag: "latest".into(),
            harness,
        }
    }

    /// Selects the official image architecture component.
    #[must_use]
    pub fn architecture(mut self, architecture: impl Into<String>) -> Self {
        self.architecture = architecture.into().into_boxed_str();
        self
    }

    /// Selects the official instance image tag.
    #[must_use]
    pub fn image_tag(mut self, tag: impl Into<String>) -> Self {
        self.image_tag = tag.into().into_boxed_str();
        self
    }
}

impl DatasetImporter for SweBench {
    fn plan(&self) -> Result<DatasetPlan, ImportError> {
        let instances = read_json_lines::<SweInstance>(&self.instances)?;
        let source_digest = sha256_values([
            sha256_file(&self.instances)?,
            self.namespace.to_string(),
            self.architecture.to_string(),
            self.image_tag.to_string(),
        ]);
        let source = SourceIdentity::new("swe-bench", self.revision.as_ref(), source_digest)?;
        let mut plan = DatasetPlan::new(self.name.as_ref(), source)?;
        for instance in instances {
            let encoded = instance.instance_id.to_lowercase().replace("__", "_1776_");
            let image = format!(
                "{}/sweb.eval.{}.{}:{}",
                self.namespace, self.architecture, encoded, self.image_tag
            );
            let metadata = serde_json::to_vec(&instance).map_err(|source| ImportError::Json {
                path: self.instances.clone(),
                source,
            })?;
            plan = plan.case(
                CasePlan::hermetic(
                    safe_case_id(&instance.instance_id),
                    instance.problem_statement.clone(),
                    Environment::OciImage(image),
                    self.harness.clone(),
                )?
                .harness_file("instance.json", metadata, 0o600)?,
            );
        }
        Ok(plan)
    }
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
struct SweInstance {
    instance_id: String,
    problem_statement: String,
    repo: String,
    base_commit: String,
    version: String,
    patch: String,
    test_patch: String,
    #[serde(default)]
    hints_text: String,
    #[serde(default, rename = "FAIL_TO_PASS")]
    fail_to_pass: serde_json::Value,
    #[serde(default, rename = "PASS_TO_PASS")]
    pass_to_pass: serde_json::Value,
}
