use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use nanocodex_eval::{
    TaskOutput,
    import::{
        CasePlan, DatasetImporter, DatasetPlan, Environment, Harness, ImportError, SourceIdentity,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{safe_case_id, sha256_file, sha256_values};

const QUERY_TEMPLATE_BROWSECOMP: &str = r#"{question}

Your response should be in the following format:
Explanation: {{your explanation for your final answer}}
Exact Answer: {{your succinct, final answer}}
Confidence: {{your confidence score between 0% and 100% for your answer}}"#;

const QUERY_TEMPLATE_MULTICHOICE: &str = r#"Answer the following multiple choice question. The last line of your response should be of the following format: 'Answer: $LETTER' (without quotes) where LETTER is one of ABCD. Think step by step before answering.

{question}

A) {a}
B) {b}
C) {c}
D) {d}"#;

const OFFICIAL_FILES: &[&str] = &[
    "LICENSE",
    "browsecomp_eval.py",
    "common.py",
    "gpqa_eval.py",
    "healthbench_eval.py",
    "types.py",
    "sampler/chat_completion_sampler.py",
    "sampler/responses_sampler.py",
];
const ADAPTER_FILES: &[&str] = &["Dockerfile", "test.sh", "grade.py", "gpqa_prepare.py"];

/// OpenAI's lightweight reference evals used in the GPT-5.6 report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenAiSimpleEval {
    /// BrowseComp with the official model grader.
    BrowseComp,
    /// HealthBench with the official rubric grader.
    HealthBench,
    /// HealthBench Professional with GPT-5.4-low rubric grading and the
    /// published response-length adjustment.
    HealthBenchProfessional,
    /// GPQA Diamond with the official answer extraction pattern.
    GpqaDiamond,
}

impl OpenAiSimpleEval {
    const fn as_str(self) -> &'static str {
        match self {
            Self::BrowseComp => "browsecomp",
            Self::HealthBench => "healthbench",
            Self::HealthBenchProfessional => "healthbench_professional",
            Self::GpqaDiamond => "gpqa_diamond",
        }
    }
}

/// Imports a pinned OpenAI `simple-evals` dataset and an external verifier
/// harness into immutable tasks.
#[derive(Clone, Debug)]
pub struct OpenAiSimpleEvals {
    name: Box<str>,
    checkout: PathBuf,
    harness: PathBuf,
    data: PathBuf,
    revision: Box<str>,
    eval: OpenAiSimpleEval,
    environment: Environment,
    limit: Option<usize>,
}

impl OpenAiSimpleEvals {
    /// Creates an importer for one official `simple-evals` data file.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        checkout: impl Into<PathBuf>,
        harness: impl Into<PathBuf>,
        data: impl Into<PathBuf>,
        revision: impl Into<String>,
        eval: OpenAiSimpleEval,
        environment: Environment,
    ) -> Self {
        Self {
            name: name.into().into_boxed_str(),
            checkout: checkout.into(),
            harness: harness.into(),
            data: data.into(),
            revision: revision.into().into_boxed_str(),
            eval,
            environment,
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

impl DatasetImporter for OpenAiSimpleEvals {
    fn plan(&self) -> Result<DatasetPlan, ImportError> {
        validate_checkout(&self.checkout)?;
        validate_harness(&self.harness)?;
        let mut source_inputs = vec![self.eval.as_str().to_owned(), sha256_file(&self.data)?];
        for relative in OFFICIAL_FILES {
            source_inputs.push(sha256_file(&self.checkout.join(relative))?);
        }
        for relative in ADAPTER_FILES {
            source_inputs.push(sha256_file(&self.harness.join(relative))?);
        }
        let source = SourceIdentity::new(
            "openai-simple-evals",
            self.revision.as_ref(),
            sha256_values(source_inputs.iter().map(String::as_bytes)),
        )?;
        let harness = Harness::directory(&self.harness)?;
        let limit = self.limit.unwrap_or(usize::MAX);
        if limit == 0 {
            return Err(ImportError::Invalid(
                "OpenAI simple-evals import limit must be greater than zero".to_owned(),
            ));
        }
        let cases = match self.eval {
            OpenAiSimpleEval::BrowseComp => browsecomp_cases(&self.data, limit)?,
            OpenAiSimpleEval::HealthBench => healthbench_cases(&self.data, limit)?,
            OpenAiSimpleEval::HealthBenchProfessional => {
                healthbench_professional_cases(&self.data, limit)?
            }
            OpenAiSimpleEval::GpqaDiamond => {
                gpqa_cases(&self.data, &self.harness.join("gpqa_prepare.py"), limit)?
            }
        };
        if cases.is_empty() {
            return Err(ImportError::Invalid(
                "OpenAI simple-evals source contains no cases".to_owned(),
            ));
        }
        let mut plan = DatasetPlan::new(self.name.as_ref(), source)?;
        for (index, case) in cases.into_iter().enumerate() {
            let id = format!("{}-{index:06}", safe_case_id(self.eval.as_str()));
            let metadata =
                serde_json::to_vec(&case.metadata).map_err(|source| ImportError::Json {
                    path: self.data.clone(),
                    source,
                })?;
            let mut planned =
                CasePlan::hermetic(id, case.prompt, self.environment.clone(), harness.clone())?
                    .output(TaskOutput::FinalMessage)
                    .harness_file("case.json", metadata, 0o600)?
                    .harness_file("official/__init__.py", Vec::new(), 0o644)?
                    .harness_file("official/sampler/__init__.py", Vec::new(), 0o644)?;
            for relative in OFFICIAL_FILES {
                let source_path = self.checkout.join(relative);
                let bytes = fs::read(&source_path).map_err(|source| ImportError::Io {
                    path: source_path,
                    source,
                })?;
                planned =
                    planned.harness_file(PathBuf::from("official").join(relative), bytes, 0o644)?;
            }
            plan = plan.case(planned);
        }
        Ok(plan)
    }
}

struct PreparedCase {
    prompt: String,
    metadata: serde_json::Value,
}

#[derive(Deserialize)]
struct BrowseRow {
    problem: String,
    answer: String,
    canary: String,
}

fn browsecomp_cases(path: &Path, limit: usize) -> Result<Vec<PreparedCase>, ImportError> {
    let mut reader = csv::Reader::from_path(path).map_err(|source| {
        ImportError::Invalid(format!("failed to decode {}: {source}", path.display()))
    })?;
    reader
        .deserialize::<BrowseRow>()
        .take(limit)
        .map(|row| {
            let row = row.map_err(|source| {
                ImportError::Invalid(format!("failed to decode {}: {source}", path.display()))
            })?;
            let question = decrypt(&row.problem, &row.canary)?;
            let correct_answer = decrypt(&row.answer, &row.canary)?;
            Ok(PreparedCase {
                prompt: QUERY_TEMPLATE_BROWSECOMP
                    .replace("{question}", &question)
                    .replace("{{", "{")
                    .replace("}}", "}"),
                metadata: serde_json::json!({
                    "kind": "browsecomp",
                    "question": question,
                    "correct_answer": correct_answer,
                }),
            })
        })
        .collect()
}

fn decrypt(ciphertext: &str, password: &str) -> Result<String, ImportError> {
    let encrypted = BASE64.decode(ciphertext).map_err(|source| {
        ImportError::Invalid(format!("invalid BrowseComp ciphertext: {source}"))
    })?;
    let key = Sha256::digest(password.as_bytes());
    let decrypted = encrypted
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ key[index % key.len()])
        .collect::<Vec<_>>();
    String::from_utf8(decrypted)
        .map_err(|source| ImportError::Invalid(format!("invalid BrowseComp plaintext: {source}")))
}

#[derive(Deserialize, Serialize)]
struct HealthRow {
    prompt: Vec<HealthMessage>,
    rubrics: Vec<serde_json::Value>,
    example_tags: Vec<String>,
    prompt_id: String,
}

#[derive(Deserialize, Serialize)]
struct HealthMessage {
    role: String,
    content: String,
}

#[derive(Deserialize, Serialize)]
struct ProfessionalHealthRow {
    id: String,
    conversation: ProfessionalConversation,
    rubric_items: Vec<ProfessionalRubricItem>,
    use_case: String,
    r#type: String,
    difficulty: String,
    specialty: String,
}

#[derive(Deserialize, Serialize)]
struct ProfessionalConversation {
    messages: Vec<HealthMessage>,
}

#[derive(Deserialize, Serialize)]
struct ProfessionalRubricItem {
    criterion_text: String,
    points: f64,
}

fn healthbench_cases(path: &Path, limit: usize) -> Result<Vec<PreparedCase>, ImportError> {
    let text = fs::read_to_string(path).map_err(|source| ImportError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(limit)
        .map(|(index, line)| {
            let row: HealthRow = serde_json::from_str(line).map_err(|source| {
                ImportError::Invalid(format!(
                    "failed to decode {} line {}: {source}",
                    path.display(),
                    index + 1
                ))
            })?;
            if row.prompt.len() != 1 || row.prompt[0].role != "user" {
                return Err(ImportError::Invalid(format!(
                    "HealthBench row {} is not one user message and cannot be represented by one evaluator turn",
                    index + 1
                )));
            }
            let prompt = row.prompt[0].content.clone();
            let metadata = serde_json::to_value(&row).map_err(|source| ImportError::Json {
                path: path.to_path_buf(),
                source,
            })?;
            let mut metadata = metadata.as_object().cloned().ok_or_else(|| {
                ImportError::Invalid("HealthBench row did not encode as an object".to_owned())
            })?;
            metadata.insert("kind".to_owned(), serde_json::json!("healthbench"));
            Ok(PreparedCase {
                prompt,
                metadata: metadata.into(),
            })
        })
        .collect()
}

fn healthbench_professional_cases(
    path: &Path,
    limit: usize,
) -> Result<Vec<PreparedCase>, ImportError> {
    let text = fs::read_to_string(path).map_err(|source| ImportError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(limit)
        .map(|(index, line)| {
            let row: ProfessionalHealthRow = serde_json::from_str(line).map_err(|source| {
                ImportError::Invalid(format!(
                    "failed to decode {} line {}: {source}",
                    path.display(),
                    index + 1
                ))
            })?;
            if row.conversation.messages.len() != 1
                || row.conversation.messages[0].role != "user"
            {
                return Err(ImportError::Invalid(format!(
                    "HealthBench Professional row {} has prior conversation turns, which the evaluator cannot seed without changing their roles",
                    index + 1
                )));
            }
            let prompt = row.conversation.messages[0].content.clone();
            let rubrics = row
                .rubric_items
                .iter()
                .map(|item| {
                    serde_json::json!({
                        "criterion": item.criterion_text,
                        "points": item.points,
                        "tags": [],
                    })
                })
                .collect::<Vec<_>>();
            Ok(PreparedCase {
                prompt,
                metadata: serde_json::json!({
                    "kind": "healthbench_professional",
                    "prompt": row.conversation.messages,
                    "rubrics": rubrics,
                    "example_tags": [],
                    "id": row.id,
                    "use_case": row.use_case,
                    "type": row.r#type,
                    "difficulty": row.difficulty,
                    "specialty": row.specialty,
                }),
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct PreparedGpqa {
    question: String,
    choices: [String; 4],
    correct_answer: String,
}

fn gpqa_cases(
    path: &Path,
    prepare_script: &Path,
    limit: usize,
) -> Result<Vec<PreparedCase>, ImportError> {
    let output = Command::new("python3")
        .arg(prepare_script)
        .arg(path)
        .arg(limit.to_string())
        .output()
        .map_err(|source| ImportError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if !output.status.success() {
        return Err(ImportError::Invalid(format!(
            "official GPQA permutation preparation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|source| {
            ImportError::Invalid(format!("GPQA preparation was not UTF-8: {source}"))
        })?
        .lines()
        .map(|line| {
            let row: PreparedGpqa = serde_json::from_str(line).map_err(|source| {
                ImportError::Invalid(format!("failed to decode prepared GPQA row: {source}"))
            })?;
            let prompt = QUERY_TEMPLATE_MULTICHOICE
                .replace("{question}", &row.question)
                .replace("{a}", &row.choices[0])
                .replace("{b}", &row.choices[1])
                .replace("{c}", &row.choices[2])
                .replace("{d}", &row.choices[3]);
            Ok(PreparedCase {
                prompt,
                metadata: serde_json::json!({
                    "kind": "gpqa_diamond",
                    "correct_answer": row.correct_answer,
                }),
            })
        })
        .collect()
}

fn validate_checkout(checkout: &Path) -> Result<(), ImportError> {
    for relative in OFFICIAL_FILES {
        let path = checkout.join(relative);
        if !path.is_file() {
            return Err(ImportError::Invalid(format!(
                "OpenAI simple-evals checkout is missing {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn validate_harness(harness: &Path) -> Result<(), ImportError> {
    for relative in ADAPTER_FILES {
        let path = harness.join(relative);
        if !path.is_file() {
            return Err(ImportError::Invalid(format!(
                "OpenAI simple-evals harness is missing {}",
                path.display()
            )));
        }
    }
    Ok(())
}
