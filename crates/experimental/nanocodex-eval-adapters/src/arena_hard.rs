use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use nanocodex_eval::{
    TaskOutput,
    import::{
        CasePlan, DatasetImporter, DatasetPlan, Environment, Harness, ImportError, SourceIdentity,
    },
};

use crate::{read_json_lines, safe_case_id, sha256_file};

/// Arena-Hard prompt importer using a caller-packaged official judge harness.
#[derive(Clone, Debug)]
pub struct ArenaHard {
    name: Box<str>,
    questions: PathBuf,
    revision: Box<str>,
    environment: Environment,
    harness: Harness,
    limit: Option<usize>,
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
        }
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
        let source = SourceIdentity::new(
            "arena-hard",
            self.revision.as_ref(),
            sha256_file(&self.questions)?,
        )?;
        let mut plan = DatasetPlan::new(self.name.as_ref(), source)?;
        for question in questions {
            let metadata = serde_json::to_vec(&question).map_err(|source| ImportError::Json {
                path: self.questions.clone(),
                source,
            })?;
            plan = plan.case(
                CasePlan::hermetic(
                    safe_case_id(&question.uid),
                    question.prompt.clone(),
                    self.environment.clone(),
                    self.harness.clone(),
                )?
                .output(TaskOutput::FinalMessage)
                .harness_file("case.json", metadata, 0o600)?,
            );
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
