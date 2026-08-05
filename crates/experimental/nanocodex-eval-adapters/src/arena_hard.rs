use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use nanocodex_eval::{
    TaskOutput,
    import::{
        CasePlan, DatasetImporter, DatasetPlan, Environment, Harness, ImportError, SourceIdentity,
    },
};

use crate::{read_json_lines, safe_case_id, sha256_file};

const FINAL_RESPONSE_INSTRUCTIONS: &str = "Return the complete answer in the final assistant \
message. The benchmark judge sees only that message and cannot inspect files in the workspace, \
so do not refer to local artifacts as the answer.";

/// Arena-Hard prompt importer using a caller-packaged official judge harness.
#[derive(Clone, Debug)]
pub struct ArenaHard {
    name: Box<str>,
    questions: PathBuf,
    revision: Box<str>,
    environment: Environment,
    harness: Harness,
    limit: Option<usize>,
    baseline_answers: Option<PathBuf>,
}

impl ArenaHard {
    /// Creates an Arena-Hard importer.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        questions_jsonl: impl Into<PathBuf>,
        revision: impl Into<String>,
        environment: Environment,
        harness: Harness,
    ) -> Self {
        Self {
            name: name.into().into_boxed_str(),
            questions: questions_jsonl.into(),
            revision: revision.into().into_boxed_str(),
            environment,
            harness,
            limit: None,
            baseline_answers: None,
        }
    }

    /// Binds the benchmark's published baseline answers into every judged case.
    #[must_use]
    pub fn baseline_answers(mut self, path: impl Into<PathBuf>) -> Self {
        self.baseline_answers = Some(path.into());
        self
    }

    /// Restricts import to a deterministic prefix for smoke validation.
    #[must_use]
    pub const fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }
}

impl DatasetImporter for ArenaHard {
    fn plan(&self) -> Result<DatasetPlan, ImportError> {
        let mut questions = read_json_lines::<ArenaQuestion>(&self.questions)?;
        if let Some(limit) = self.limit {
            if limit == 0 {
                return Err(ImportError::Invalid(
                    "Arena-Hard import limit must be greater than zero".to_owned(),
                ));
            }
            questions.truncate(limit);
        }
        let baselines = self
            .baseline_answers
            .as_ref()
            .map(|path| read_json_lines::<ArenaAnswer>(path))
            .transpose()?
            .unwrap_or_default()
            .into_iter()
            .map(|answer| (answer.uid.clone(), answer))
            .collect::<BTreeMap<_, _>>();
        let source_digest = if let Some(path) = &self.baseline_answers {
            crate::sha256_values([sha256_file(&self.questions)?, sha256_file(path)?])
        } else {
            sha256_file(&self.questions)?
        };
        let source = SourceIdentity::new("arena-hard", self.revision.as_ref(), source_digest)?;
        let mut plan = DatasetPlan::new(self.name.as_ref(), source)?;
        for question in questions {
            let metadata = serde_json::to_vec(&question).map_err(|source| ImportError::Json {
                path: self.questions.clone(),
                source,
            })?;
            let uid = question.uid.clone();
            let mut case = CasePlan::hermetic(
                safe_case_id(&question.uid),
                question.prompt.clone(),
                self.environment.clone(),
                self.harness.clone(),
            )?
            .instructions(FINAL_RESPONSE_INSTRUCTIONS)
            .output(TaskOutput::FinalMessage)
            .harness_file("case.json", metadata, 0o600)?;
            if let Some(baseline) = baselines.get(&uid) {
                let baseline =
                    serde_json::to_vec(baseline).map_err(|source| ImportError::Json {
                        path: self
                            .baseline_answers
                            .clone()
                            .unwrap_or_else(|| self.questions.clone()),
                        source,
                    })?;
                case = case.harness_file("baseline.json", baseline, 0o600)?;
            } else if self.baseline_answers.is_some() {
                return Err(ImportError::Invalid(format!(
                    "Arena-Hard baseline answer is missing for {uid}"
                )));
            }
            plan = plan.case(case);
        }
        Ok(plan)
    }
}

#[derive(Deserialize, Serialize)]
struct ArenaQuestion {
    uid: String,
    category: String,
    #[serde(default)]
    subcategory: Option<String>,
    prompt: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct ArenaAnswer {
    uid: String,
    model: String,
    messages: Vec<serde_json::Value>,
}
