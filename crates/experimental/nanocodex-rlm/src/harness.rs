use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    HarnessMemory, HarnessRefinement, HarnessSkill, PromptNote, SnapshotError, SubagentSpec,
    snapshot::{HarnessSnapshot, digest_parts},
};

const MAX_HARNESS_TEXT_BYTES: usize = 64 * 1024;

pub(crate) struct HarnessStore {
    current: Mutex<HarnessSnapshot>,
    enabled_subagents: RwLock<Arc<[SubagentSpec]>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum HarnessEdit {
    CreatePromptNote {
        id: String,
        text: String,
        #[serde(default = "enabled_by_default")]
        enabled: bool,
    },
    UpdatePromptNote {
        id: String,
        text: Option<String>,
        enabled: Option<bool>,
    },
    DeletePromptNote {
        id: String,
    },
    CreateMemory {
        id: String,
        name: String,
        content: String,
        #[serde(default = "enabled_by_default")]
        enabled: bool,
    },
    UpdateMemory {
        id: String,
        name: Option<String>,
        content: Option<String>,
        enabled: Option<bool>,
    },
    DeleteMemory {
        id: String,
    },
    CreateSkill {
        id: String,
        name: String,
        description: String,
        instructions: String,
        #[serde(default = "enabled_by_default")]
        enabled: bool,
    },
    UpdateSkill {
        id: String,
        name: Option<String>,
        description: Option<String>,
        instructions: Option<String>,
        enabled: Option<bool>,
    },
    DeleteSkill {
        id: String,
    },
    CreateSubagent {
        id: String,
        name: String,
        description: String,
        instructions: String,
        #[serde(default = "enabled_by_default")]
        enabled: bool,
    },
    UpdateSubagent {
        id: String,
        name: Option<String>,
        description: Option<String>,
        instructions: Option<String>,
        enabled: Option<bool>,
    },
    DeleteSubagent {
        id: String,
    },
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AppliedHarnessRevision {
    pub(crate) revision: u64,
    pub(crate) digest: Box<str>,
    pub(crate) operation: Box<str>,
    pub(crate) context: Box<str>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum HarnessStoreError {
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    #[error("invalid continual harness edit: {0}")]
    Invalid(String),
    #[error("failed to serialize continual harness: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("failed to persist continual harness `{path}`: {source}")]
    Persist {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("no archived continual harness revision {0}")]
    UnknownRevision(u64),
}

impl HarnessStore {
    pub(crate) fn new(initial: HarnessSnapshot) -> Self {
        let enabled_subagents = enabled_subagents(&initial);
        Self {
            current: Mutex::new(initial),
            enabled_subagents: RwLock::new(enabled_subagents),
        }
    }

    pub(crate) async fn snapshot(&self) -> HarnessSnapshot {
        self.current.lock().await.clone()
    }

    pub(crate) async fn enabled_subagent(&self, id: &str) -> Option<SubagentSpec> {
        self.current.lock().await.enabled_subagent(id).cloned()
    }

    pub(crate) fn enabled_subagents(&self) -> Arc<[SubagentSpec]> {
        self.enabled_subagents
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) async fn apply(
        &self,
        edit: HarnessEdit,
        trigger: String,
    ) -> Result<AppliedHarnessRevision, HarnessStoreError> {
        validate_text(&trigger, "refinement trigger")?;
        let mut current = self.current.lock().await;
        let mut candidate = current.clone();
        let operation = candidate.apply_edit(edit)?;
        candidate.revision = current.revision.saturating_add(1);
        candidate.refinements.push(HarnessRefinement {
            revision: candidate.revision,
            trigger: trigger.trim().to_owned().into_boxed_str(),
            operation: operation.clone().into_boxed_str(),
        });
        candidate.validate()?;
        persist_replacement(&current, &mut candidate).await?;
        let result = AppliedHarnessRevision {
            revision: candidate.revision,
            digest: candidate.digest().to_owned().into_boxed_str(),
            operation: operation.into_boxed_str(),
            context: render_context(&candidate).into_boxed_str(),
        };
        *current = candidate;
        self.refresh_enabled_subagents(&current);
        Ok(result)
    }

    pub(crate) async fn rollback(
        &self,
        target_revision: u64,
        trigger: String,
    ) -> Result<AppliedHarnessRevision, HarnessStoreError> {
        validate_text(&trigger, "rollback trigger")?;
        let mut current = self.current.lock().await;
        let archived_path = find_archived_revision(&current, target_revision).await?;
        let archived = HarnessSnapshot::load(&archived_path)?;
        let mut candidate = archived;
        candidate.source_path = current.source_path.clone();
        candidate.revision = current.revision.saturating_add(1);
        let operation = format!("rollback_to_revision:{target_revision}");
        candidate.refinements = current.refinements.clone();
        candidate.refinements.push(HarnessRefinement {
            revision: candidate.revision,
            trigger: trigger.trim().to_owned().into_boxed_str(),
            operation: operation.clone().into_boxed_str(),
        });
        candidate.validate()?;
        persist_replacement(&current, &mut candidate).await?;
        let result = AppliedHarnessRevision {
            revision: candidate.revision,
            digest: candidate.digest().to_owned().into_boxed_str(),
            operation: operation.into_boxed_str(),
            context: render_context(&candidate).into_boxed_str(),
        };
        *current = candidate;
        self.refresh_enabled_subagents(&current);
        Ok(result)
    }

    fn refresh_enabled_subagents(&self, snapshot: &HarnessSnapshot) {
        *self
            .enabled_subagents
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = enabled_subagents(snapshot);
    }
}

fn enabled_subagents(snapshot: &HarnessSnapshot) -> Arc<[SubagentSpec]> {
    snapshot
        .subagents()
        .iter()
        .filter(|spec| spec.enabled())
        .cloned()
        .collect::<Vec<_>>()
        .into()
}

impl HarnessSnapshot {
    fn apply_edit(&mut self, edit: HarnessEdit) -> Result<String, HarnessStoreError> {
        match edit {
            HarnessEdit::CreatePromptNote { id, text, enabled } => {
                ensure_absent(&self.prompt_notes, &id, |entry| entry.id())?;
                validate_text(&text, "prompt note text")?;
                self.prompt_notes.push(PromptNote {
                    id: id.clone().into_boxed_str(),
                    text: text.into_boxed_str(),
                    enabled,
                });
                Ok(format!("create_prompt_note:{id}"))
            }
            HarnessEdit::UpdatePromptNote { id, text, enabled } => {
                require_update(text.is_some() || enabled.is_some(), &id)?;
                let entry = find_mut(&mut self.prompt_notes, &id, |entry| entry.id())?;
                if let Some(text) = text {
                    validate_text(&text, "prompt note text")?;
                    entry.text = text.into_boxed_str();
                }
                if let Some(enabled) = enabled {
                    entry.enabled = enabled;
                }
                Ok(format!("update_prompt_note:{id}"))
            }
            HarnessEdit::DeletePromptNote { id } => {
                remove(&mut self.prompt_notes, &id, |entry| entry.id())?;
                Ok(format!("delete_prompt_note:{id}"))
            }
            HarnessEdit::CreateMemory {
                id,
                name,
                content,
                enabled,
            } => {
                ensure_absent(&self.memories, &id, |entry| entry.id())?;
                validate_text(&name, "memory name")?;
                validate_text(&content, "memory content")?;
                self.memories.push(HarnessMemory {
                    id: id.clone().into_boxed_str(),
                    name: name.into_boxed_str(),
                    content: content.into_boxed_str(),
                    enabled,
                });
                Ok(format!("create_memory:{id}"))
            }
            HarnessEdit::UpdateMemory {
                id,
                name,
                content,
                enabled,
            } => {
                require_update(
                    name.is_some() || content.is_some() || enabled.is_some(),
                    &id,
                )?;
                let entry = find_mut(&mut self.memories, &id, |entry| entry.id())?;
                update_text(&mut entry.name, name, "memory name")?;
                update_text(&mut entry.content, content, "memory content")?;
                update_enabled(&mut entry.enabled, enabled);
                Ok(format!("update_memory:{id}"))
            }
            HarnessEdit::DeleteMemory { id } => {
                remove(&mut self.memories, &id, |entry| entry.id())?;
                Ok(format!("delete_memory:{id}"))
            }
            HarnessEdit::CreateSkill {
                id,
                name,
                description,
                instructions,
                enabled,
            } => {
                ensure_absent(&self.skills, &id, |entry| entry.id())?;
                validate_text(&name, "skill name")?;
                validate_text(&description, "skill description")?;
                validate_text(&instructions, "skill instructions")?;
                self.skills.push(HarnessSkill {
                    id: id.clone().into_boxed_str(),
                    name: name.into_boxed_str(),
                    description: description.into_boxed_str(),
                    instructions: instructions.into_boxed_str(),
                    enabled,
                });
                Ok(format!("create_skill:{id}"))
            }
            HarnessEdit::UpdateSkill {
                id,
                name,
                description,
                instructions,
                enabled,
            } => {
                require_update(
                    name.is_some()
                        || description.is_some()
                        || instructions.is_some()
                        || enabled.is_some(),
                    &id,
                )?;
                let entry = find_mut(&mut self.skills, &id, |entry| entry.id())?;
                update_text(&mut entry.name, name, "skill name")?;
                update_text(&mut entry.description, description, "skill description")?;
                update_text(&mut entry.instructions, instructions, "skill instructions")?;
                update_enabled(&mut entry.enabled, enabled);
                Ok(format!("update_skill:{id}"))
            }
            HarnessEdit::DeleteSkill { id } => {
                remove(&mut self.skills, &id, |entry| entry.id())?;
                Ok(format!("delete_skill:{id}"))
            }
            HarnessEdit::CreateSubagent {
                id,
                name,
                description,
                instructions,
                enabled,
            } => {
                ensure_absent(&self.subagents, &id, |entry| entry.id())?;
                validate_text(&name, "subagent name")?;
                validate_text(&description, "subagent description")?;
                validate_text(&instructions, "subagent instructions")?;
                self.subagents.push(SubagentSpec {
                    id: id.clone().into_boxed_str(),
                    name: name.into_boxed_str(),
                    description: description.into_boxed_str(),
                    instructions: instructions.into_boxed_str(),
                    enabled,
                });
                Ok(format!("create_subagent:{id}"))
            }
            HarnessEdit::UpdateSubagent {
                id,
                name,
                description,
                instructions,
                enabled,
            } => {
                require_update(
                    name.is_some()
                        || description.is_some()
                        || instructions.is_some()
                        || enabled.is_some(),
                    &id,
                )?;
                let entry = find_mut(&mut self.subagents, &id, |entry| entry.id())?;
                update_text(&mut entry.name, name, "subagent name")?;
                update_text(&mut entry.description, description, "subagent description")?;
                update_text(
                    &mut entry.instructions,
                    instructions,
                    "subagent instructions",
                )?;
                update_enabled(&mut entry.enabled, enabled);
                Ok(format!("update_subagent:{id}"))
            }
            HarnessEdit::DeleteSubagent { id } => {
                remove(&mut self.subagents, &id, |entry| entry.id())?;
                Ok(format!("delete_subagent:{id}"))
            }
        }
    }
}

pub(crate) fn render_context(snapshot: &HarnessSnapshot) -> String {
    let mut output = format!(
        "# Continual harness revision {}\n\nThis complete snapshot supersedes all earlier continual-harness revision messages.",
        snapshot.revision()
    );
    render_entries(
        &mut output,
        "Prompt notes",
        snapshot
            .prompt_notes()
            .iter()
            .filter(|entry| entry.enabled()),
        |entry| format!("`{}`: {}", entry.id(), entry.text()),
    );
    render_entries(
        &mut output,
        "Memories",
        snapshot.memories().iter().filter(|entry| entry.enabled()),
        |entry| format!("`{}` — {}: {}", entry.id(), entry.name(), entry.content()),
    );
    render_entries(
        &mut output,
        "Skills",
        snapshot.skills().iter().filter(|entry| entry.enabled()),
        |entry| {
            format!(
                "`{}` — {}: {}\n  Instructions: {}",
                entry.id(),
                entry.name(),
                entry.description(),
                entry.instructions()
            )
        },
    );
    render_entries(
        &mut output,
        "Subagent specifications",
        snapshot.subagents().iter().filter(|entry| entry.enabled()),
        |entry| {
            format!(
                "`{}` — {}: {}\n  Instructions: {}",
                entry.id(),
                entry.name(),
                entry.description(),
                entry.instructions()
            )
        },
    );
    output
}

async fn persist_replacement(
    current: &HarnessSnapshot,
    candidate: &mut HarnessSnapshot,
) -> Result<(), HarnessStoreError> {
    let source = toml::to_string_pretty(candidate)?;
    let bytes = source.as_bytes();
    let path = current.source_path();
    archive_current(current).await?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("nanocodex.harness.toml");
    let temporary = path.with_file_name(format!(".{filename}.tmp-{}", uuid::Uuid::now_v7()));
    tokio::fs::write(&temporary, bytes)
        .await
        .map_err(|source| HarnessStoreError::Persist {
            path: temporary.clone(),
            source,
        })?;
    if let Err(source) = tokio::fs::rename(&temporary, path).await {
        drop(tokio::fs::remove_file(&temporary).await);
        return Err(HarnessStoreError::Persist {
            path: path.to_path_buf(),
            source,
        });
    }
    candidate.digest = digest_parts([bytes]).into();
    candidate.source_path = current.source_path.clone();
    candidate.source = Arc::from(bytes);
    Ok(())
}

async fn archive_current(snapshot: &HarnessSnapshot) -> Result<(), HarnessStoreError> {
    let directory = history_directory(snapshot);
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(|source| HarnessStoreError::Persist {
            path: directory.clone(),
            source,
        })?;
    let archive = directory.join(format!(
        "revision-{}-{}.toml",
        snapshot.revision(),
        snapshot.digest()
    ));
    if tokio::fs::try_exists(&archive)
        .await
        .map_err(|source| HarnessStoreError::Persist {
            path: archive.clone(),
            source,
        })?
    {
        return Ok(());
    }
    tokio::fs::write(&archive, snapshot.source())
        .await
        .map_err(|source| HarnessStoreError::Persist {
            path: archive,
            source,
        })
}

async fn find_archived_revision(
    snapshot: &HarnessSnapshot,
    revision: u64,
) -> Result<PathBuf, HarnessStoreError> {
    let directory = history_directory(snapshot);
    let mut entries =
        tokio::fs::read_dir(&directory)
            .await
            .map_err(|source| HarnessStoreError::Persist {
                path: directory.clone(),
                source,
            })?;
    let prefix = format!("revision-{revision}-");
    while let Some(entry) =
        entries
            .next_entry()
            .await
            .map_err(|source| HarnessStoreError::Persist {
                path: directory.clone(),
                source,
            })?
    {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            return Ok(entry.path());
        }
    }
    Err(HarnessStoreError::UnknownRevision(revision))
}

fn history_directory(snapshot: &HarnessSnapshot) -> PathBuf {
    let path = snapshot.source_path();
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("nanocodex.harness.toml");
    path.with_file_name(format!("{filename}.history"))
}

fn render_entries<'a, T: 'a>(
    output: &mut String,
    heading: &str,
    entries: impl Iterator<Item = &'a T>,
    render: impl Fn(&T) -> String,
) {
    let entries = entries.map(render).collect::<Vec<_>>();
    if entries.is_empty() {
        return;
    }
    output.push_str("\n\n## ");
    output.push_str(heading);
    for entry in entries {
        output.push_str("\n\n- ");
        output.push_str(&entry);
    }
}

fn ensure_absent<T>(
    entries: &[T],
    id: &str,
    select: impl Fn(&T) -> &str,
) -> Result<(), HarnessStoreError> {
    if entries.iter().any(|entry| select(entry) == id) {
        return Err(HarnessStoreError::Invalid(format!(
            "entry `{id}` already exists"
        )));
    }
    Ok(())
}

fn find_mut<'a, T>(
    entries: &'a mut [T],
    id: &str,
    select: impl Fn(&T) -> &str,
) -> Result<&'a mut T, HarnessStoreError> {
    entries
        .iter_mut()
        .find(|entry| select(entry) == id)
        .ok_or_else(|| HarnessStoreError::Invalid(format!("unknown entry `{id}`")))
}

fn remove<T>(
    entries: &mut Vec<T>,
    id: &str,
    select: impl Fn(&T) -> &str,
) -> Result<(), HarnessStoreError> {
    let index = entries
        .iter()
        .position(|entry| select(entry) == id)
        .ok_or_else(|| HarnessStoreError::Invalid(format!("unknown entry `{id}`")))?;
    entries.remove(index);
    Ok(())
}

fn update_text(
    target: &mut Box<str>,
    replacement: Option<String>,
    label: &str,
) -> Result<(), HarnessStoreError> {
    if let Some(replacement) = replacement {
        validate_text(&replacement, label)?;
        *target = replacement.into_boxed_str();
    }
    Ok(())
}

fn require_update(changed: bool, id: &str) -> Result<(), HarnessStoreError> {
    if !changed {
        return Err(HarnessStoreError::Invalid(format!(
            "update for `{id}` must provide at least one changed field"
        )));
    }
    Ok(())
}

const fn update_enabled(target: &mut bool, replacement: Option<bool>) {
    if let Some(replacement) = replacement {
        *target = replacement;
    }
}

fn validate_text(value: &str, label: &str) -> Result<(), HarnessStoreError> {
    if value.trim().is_empty() {
        return Err(HarnessStoreError::Invalid(format!(
            "{label} must not be empty"
        )));
    }
    if value.len() > MAX_HARNESS_TEXT_BYTES {
        return Err(HarnessStoreError::Invalid(format!(
            "{label} exceeds the {MAX_HARNESS_TEXT_BYTES}-byte limit"
        )));
    }
    Ok(())
}

const fn enabled_by_default() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn edits_persist_monotonic_revisions_and_can_rollback() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("harness.toml");
        fs::write(
            &path,
            "version = 1\nrevision = 0\n[[subagents]]\nid = 'general'\nname = 'General'\ndescription = 'General work'\ninstructions = 'Inspect'\n",
        )
        .unwrap();
        let store = HarnessStore::new(HarnessSnapshot::load(&path).unwrap());

        let applied = store
            .apply(
                HarnessEdit::CreateMemory {
                    id: "retry".to_owned(),
                    name: "Retry".to_owned(),
                    content: "Retry flaky probes once.".to_owned(),
                    enabled: true,
                },
                "a probe failed transiently".to_owned(),
            )
            .await
            .unwrap();
        assert_eq!(applied.revision, 1);
        assert!(applied.context.contains("Retry flaky probes once"));
        assert_eq!(HarnessSnapshot::load(&path).unwrap().revision(), 1);

        let rolled_back = store
            .rollback(0, "the retry hid deterministic failures".to_owned())
            .await
            .unwrap();
        assert_eq!(rolled_back.revision, 2);
        assert!(store.snapshot().await.memories().is_empty());
        assert_eq!(store.snapshot().await.refinements().len(), 2);
    }
}
