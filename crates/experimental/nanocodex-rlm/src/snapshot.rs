use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const HARNESS_VERSION: u32 = 1;
const ORCHESTRATION_PROMPT: &str = "orchestration.md";
const SUBAGENT_PROMPT: &str = "subagent.md";
const TOOL_DESCRIPTIONS: &str = "tools.toml";

/// Immutable launch-time orchestration prose loaded from ordinary text files.
#[derive(Clone, Debug)]
pub struct PromptPack {
    orchestration: Arc<str>,
    subagent: Arc<str>,
    tools: ToolDescriptions,
    digest: Arc<str>,
}

/// Launch-loaded model-facing prose for stable recursive operations.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDescriptions {
    spawn: Box<str>,
    list: Box<str>,
    send: Box<str>,
    wait: Box<str>,
    interrupt: Box<str>,
    close: Box<str>,
}

/// One versioned, immutable projection of evolving supplemental harness state.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessSnapshot {
    version: u32,
    revision: u64,
    #[serde(default)]
    prompt_notes: Vec<PromptNote>,
    #[serde(default)]
    subagents: Vec<SubagentSpec>,
    #[serde(skip)]
    digest: Arc<str>,
}

/// Complete frozen prompt and harness identity used by one treatment.
#[derive(Clone, Debug)]
pub struct LaunchSnapshot {
    prompts: PromptPack,
    harness: HarnessSnapshot,
    digest: Arc<str>,
}

/// One optional supplemental instruction supplied to root agents.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromptNote {
    id: Box<str>,
    text: Box<str>,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
}

/// One reusable named clean-subagent specification.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentSpec {
    id: Box<str>,
    name: Box<str>,
    description: Box<str>,
    instructions: Box<str>,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
}

/// Failure to load or validate a launch snapshot.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// One required launch file could not be read.
    #[error("failed to read RLM launch file `{path}`: {source}")]
    Read {
        /// File that could not be read.
        path: PathBuf,
        /// Underlying filesystem error.
        source: std::io::Error,
    },
    /// The harness was not valid TOML for the supported schema.
    #[error("failed to parse RLM harness `{path}`: {source}")]
    Parse {
        /// Harness file that could not be parsed.
        path: PathBuf,
        /// TOML decoding failure.
        source: toml::de::Error,
    },
    /// A required value was empty or an identifier was duplicated.
    #[error("invalid RLM launch snapshot: {0}")]
    Invalid(String),
    /// The harness schema version is not supported.
    #[error("unsupported RLM harness version {actual}; expected {expected}")]
    UnsupportedVersion {
        /// Supported schema version.
        expected: u32,
        /// Version observed in the file.
        actual: u32,
    },
}

impl PromptPack {
    /// Loads and validates the orchestration, subagent, and tool-description
    /// files from `directory`.
    ///
    /// # Errors
    ///
    /// Returns an error when either file is unavailable or empty.
    pub fn load(directory: impl AsRef<Path>) -> Result<Self, SnapshotError> {
        let directory = directory.as_ref();
        let orchestration_path = directory.join(ORCHESTRATION_PROMPT);
        let subagent_path = directory.join(SUBAGENT_PROMPT);
        let tools_path = directory.join(TOOL_DESCRIPTIONS);
        let orchestration = read_nonempty(&orchestration_path, "orchestration prompt")?;
        let subagent = read_nonempty(&subagent_path, "subagent prompt")?;
        let tools_source = read_nonempty(&tools_path, "tool descriptions")?;
        let tools: ToolDescriptions =
            toml::from_str(&tools_source).map_err(|source| SnapshotError::Parse {
                path: tools_path,
                source,
            })?;
        tools.validate()?;
        let digest = digest_parts([
            ORCHESTRATION_PROMPT.as_bytes(),
            orchestration.as_bytes(),
            SUBAGENT_PROMPT.as_bytes(),
            subagent.as_bytes(),
            TOOL_DESCRIPTIONS.as_bytes(),
            tools_source.as_bytes(),
        ]);
        Ok(Self {
            orchestration: orchestration.into(),
            subagent: subagent.into(),
            tools,
            digest: digest.into(),
        })
    }

    /// Root-facing orchestration guidance.
    #[must_use]
    pub fn orchestration(&self) -> &str {
        &self.orchestration
    }

    /// Guidance injected into every clean child.
    #[must_use]
    pub fn subagent(&self) -> &str {
        &self.subagent
    }

    /// Model-facing operation descriptions loaded with this prompt pack.
    #[must_use]
    pub const fn tools(&self) -> &ToolDescriptions {
        &self.tools
    }

    /// Lowercase SHA-256 digest of the exact named prompt files.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

impl ToolDescriptions {
    fn validate(&self) -> Result<(), SnapshotError> {
        for (label, value) in [
            ("spawn tool description", self.spawn()),
            ("list tool description", self.list()),
            ("send tool description", self.send()),
            ("wait tool description", self.wait()),
            ("interrupt tool description", self.interrupt()),
            ("close tool description", self.close()),
        ] {
            validate_nonempty(value, label)?;
        }
        Ok(())
    }

    /// Description of asynchronous clean-child creation.
    #[must_use]
    pub fn spawn(&self) -> &str {
        &self.spawn
    }

    /// Description of recursive-family inspection.
    #[must_use]
    pub fn list(&self) -> &str {
        &self.list
    }

    /// Description of agent-to-agent messaging.
    #[must_use]
    pub fn send(&self) -> &str {
        &self.send
    }

    /// Description of lifecycle/message waiting.
    #[must_use]
    pub fn wait(&self) -> &str {
        &self.wait
    }

    /// Description of reusable interruption.
    #[must_use]
    pub fn interrupt(&self) -> &str {
        &self.interrupt
    }

    /// Description of terminal subtree closure.
    #[must_use]
    pub fn close(&self) -> &str {
        &self.close
    }
}

impl HarnessSnapshot {
    /// Loads, validates, and content-addresses one harness TOML document.
    ///
    /// # Errors
    ///
    /// Returns an error for unreadable TOML, unsupported versions, empty
    /// required values, or duplicate note/specification identifiers.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, SnapshotError> {
        let path = path.as_ref();
        let source = fs::read_to_string(path).map_err(|source| SnapshotError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let mut snapshot: Self =
            toml::from_str(&source).map_err(|source| SnapshotError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        snapshot.validate()?;
        snapshot.digest = digest_parts([source.as_bytes()]).into();
        Ok(snapshot)
    }

    fn validate(&self) -> Result<(), SnapshotError> {
        if self.version != HARNESS_VERSION {
            return Err(SnapshotError::UnsupportedVersion {
                expected: HARNESS_VERSION,
                actual: self.version,
            });
        }
        validate_unique(
            self.prompt_notes.iter().map(|note| note.id.as_ref()),
            "prompt note",
        )?;
        validate_unique(
            self.subagents.iter().map(|spec| spec.id.as_ref()),
            "subagent specification",
        )?;
        for note in &self.prompt_notes {
            validate_identifier(&note.id, "prompt note")?;
            validate_nonempty(&note.text, "prompt note text")?;
        }
        for spec in &self.subagents {
            validate_identifier(&spec.id, "subagent specification")?;
            validate_nonempty(&spec.name, "subagent name")?;
            validate_nonempty(&spec.description, "subagent description")?;
            validate_nonempty(&spec.instructions, "subagent instructions")?;
        }
        Ok(())
    }

    /// Harness schema version.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// Caller-managed monotonic harness revision.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// All prompt notes, including disabled entries.
    #[must_use]
    pub fn prompt_notes(&self) -> &[PromptNote] {
        &self.prompt_notes
    }

    /// All named subagent specifications, including disabled entries.
    #[must_use]
    pub fn subagents(&self) -> &[SubagentSpec] {
        &self.subagents
    }

    /// Finds one enabled named subagent specification.
    #[must_use]
    pub fn enabled_subagent(&self, id: &str) -> Option<&SubagentSpec> {
        self.subagents
            .iter()
            .find(|spec| spec.enabled && spec.id.as_ref() == id)
    }

    /// Lowercase SHA-256 digest of the exact harness file bytes.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

impl LaunchSnapshot {
    /// Combines already-loaded prompt and harness snapshots into one identity.
    #[must_use]
    pub fn new(prompts: PromptPack, harness: HarnessSnapshot) -> Self {
        let digest = digest_parts([prompts.digest().as_bytes(), harness.digest().as_bytes()]);
        Self {
            prompts,
            harness,
            digest: digest.into(),
        }
    }

    /// Immutable launch prompt pack.
    #[must_use]
    pub const fn prompts(&self) -> &PromptPack {
        &self.prompts
    }

    /// Frozen evolving harness state.
    #[must_use]
    pub const fn harness(&self) -> &HarnessSnapshot {
        &self.harness
    }

    /// Digest binding both prompt files and the harness snapshot.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Renders the root-facing supplemental developer message.
    #[must_use]
    pub fn root_instructions(&self) -> String {
        let mut output = self.prompts.orchestration().trim().to_owned();
        for note in self
            .harness
            .prompt_notes()
            .iter()
            .filter(|note| note.enabled())
        {
            output.push_str("\n\n- ");
            output.push_str(note.text());
        }
        let enabled = self
            .harness
            .subagents()
            .iter()
            .filter(|spec| spec.enabled())
            .collect::<Vec<_>>();
        if !enabled.is_empty() {
            output.push_str("\n\nAvailable subagent specifications:");
            for spec in enabled {
                output.push_str("\n\n- `");
                output.push_str(spec.id());
                output.push_str("` — ");
                output.push_str(spec.name());
                output.push_str(": ");
                output.push_str(spec.description());
            }
        }
        output
    }
}

impl PromptNote {
    /// Stable note identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Supplemental instruction text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether this note is included in new launch snapshots.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }
}

impl SubagentSpec {
    /// Stable specification identifier used by orchestration calls.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Human-readable subagent name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Root-facing description used when selecting a specification.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Instructions injected only into children using this specification.
    #[must_use]
    pub fn instructions(&self) -> &str {
        &self.instructions
    }

    /// Whether new children may select this specification.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }
}

const fn enabled_by_default() -> bool {
    true
}

fn read_nonempty(path: &Path, label: &str) -> Result<String, SnapshotError> {
    let value = fs::read_to_string(path).map_err(|source| SnapshotError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    validate_nonempty(&value, label)?;
    Ok(value)
}

fn validate_unique<'a>(
    values: impl IntoIterator<Item = &'a str>,
    label: &str,
) -> Result<(), SnapshotError> {
    let mut seen = HashSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(SnapshotError::Invalid(format!(
                "duplicate {label} identifier `{value}`"
            )));
        }
    }
    Ok(())
}

fn validate_identifier(value: &str, label: &str) -> Result<(), SnapshotError> {
    validate_nonempty(value, &format!("{label} identifier"))?;
    if !value.starts_with(|character: char| character.is_ascii_alphanumeric())
        || value.chars().any(|character| {
            !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_' | '.')
        })
    {
        return Err(SnapshotError::Invalid(format!(
            "{label} identifier `{value}` must be filesystem-safe"
        )));
    }
    Ok(())
}

fn validate_nonempty(value: &str, label: &str) -> Result<(), SnapshotError> {
    if value.trim().is_empty() {
        return Err(SnapshotError::Invalid(format!("{label} must not be empty")));
    }
    Ok(())
}

fn digest_parts<const N: usize>(parts: [&[u8]; N]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part);
    }
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn launch_snapshot_is_content_addressed_and_renders_enabled_state() {
        let directory = tempdir().unwrap();
        let prompts = directory.path().join("prompts");
        fs::create_dir(&prompts).unwrap();
        fs::write(prompts.join(ORCHESTRATION_PROMPT), "Coordinate with JS.").unwrap();
        fs::write(prompts.join(SUBAGENT_PROMPT), "Report evidence.").unwrap();
        fs::write(
            prompts.join(TOOL_DESCRIPTIONS),
            "spawn = 'spawn'\nlist = 'list'\nsend = 'send'\nwait = 'wait'\ninterrupt = 'interrupt'\nclose = 'close'\n",
        )
        .unwrap();
        let harness = directory.path().join("harness.toml");
        fs::write(
            &harness,
            r#"
version = 1
revision = 4

[[prompt_notes]]
id = "parallel"
text = "Parallelize independent work."

[[prompt_notes]]
id = "disabled"
text = "Do not render me."
enabled = false

[[subagents]]
id = "reviewer"
name = "Reviewer"
description = "Reviews changes."
instructions = "Inspect the diff."
"#,
        )
        .unwrap();

        let snapshot = LaunchSnapshot::new(
            PromptPack::load(&prompts).unwrap(),
            HarnessSnapshot::load(&harness).unwrap(),
        );

        assert_eq!(snapshot.harness().revision(), 4);
        assert_eq!(snapshot.digest().len(), 64);
        assert!(snapshot.root_instructions().contains("Parallelize"));
        assert!(snapshot.root_instructions().contains("`reviewer`"));
        assert!(!snapshot.root_instructions().contains("Do not render"));
    }

    #[test]
    fn duplicate_ids_and_unknown_fields_fail_closed() {
        let directory = tempdir().unwrap();
        let duplicate = directory.path().join("duplicate.toml");
        fs::write(
            &duplicate,
            r#"
version = 1
revision = 0
[[prompt_notes]]
id = "same"
text = "one"
[[prompt_notes]]
id = "same"
text = "two"
"#,
        )
        .unwrap();
        assert!(
            HarnessSnapshot::load(&duplicate)
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );

        let unknown = directory.path().join("unknown.toml");
        fs::write(&unknown, "version = 1\nrevision = 0\nsurprise = true\n").unwrap();
        assert!(matches!(
            HarnessSnapshot::load(&unknown),
            Err(SnapshotError::Parse { .. })
        ));
    }
}
