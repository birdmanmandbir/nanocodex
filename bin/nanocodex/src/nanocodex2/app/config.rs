// Modified from clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa for Nanocodex2.
// SPDX-License-Identifier: Apache-2.0

//! Managed-client configuration and local presentation preferences.

use crate::{
    app::error::{ConfigError, Result},
    tui::theme::{Theme, ThemeMode},
};
use clap::ValueEnum;
use nanocodex::Thinking;
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;

pub(crate) const DEFAULT_MAX_SUBAGENTS: usize = 32;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReasoningEffort {
    Low,
    #[default]
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReasoningMode {
    #[default]
    Standard,
    Pro,
}

impl ReasoningMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Pro => "pro",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Config {
    #[serde(skip)]
    path: PathBuf,
    agent: AgentConfig,
    memory: MemoryConfig,
    theme: Theme,
    #[serde(skip)]
    reload: ReloadSource,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AgentConfig {
    workspace: PathBuf,
    thinking: ReasoningEffort,
    reasoning_mode: ReasoningMode,
    fast_mode: bool,
    max_subagents: usize,
    completion_hook: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct MemoryConfig {
    enabled: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ConfigOverrides {
    pub(crate) path: Option<PathBuf>,
    pub(crate) workspace: Option<PathBuf>,
    pub(crate) thinking: Option<ReasoningEffort>,
    pub(crate) reasoning_mode: Option<ReasoningMode>,
    pub(crate) max_subagents: Option<usize>,
}

#[derive(Clone, Debug)]
struct ReloadSource {
    overrides: ConfigOverrides,
    current_dir: PathBuf,
}

#[derive(Debug)]
pub(crate) struct ConfigReload {
    config: Config,
    workspace_changed: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigFile {
    agent: AgentConfigFile,
    memory: MemoryConfigFile,
    theme: Theme,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AgentConfigFile {
    workspace: Option<PathBuf>,
    thinking: Option<ReasoningEffort>,
    reasoning_mode: Option<ReasoningMode>,
    fast_mode: Option<bool>,
    max_subagents: Option<usize>,
    completion_hook: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MemoryConfigFile {
    enabled: bool,
}

impl Config {
    pub(crate) fn load(overrides: ConfigOverrides) -> Result<Self> {
        let current_dir = env::current_dir().map_err(ConfigError::CurrentDirectory)?;
        Self::load_with(overrides, &current_dir)
    }

    fn load_with(overrides: ConfigOverrides, current_dir: &Path) -> Result<Self> {
        let explicit = overrides.path.is_some();
        let path = match &overrides.path {
            Some(path) => resolve_path(path.clone(), current_dir),
            None => config_path()?,
        };
        let file = ConfigFile::read(&path, explicit)?;
        let config_dir = path.parent().unwrap_or(Path::new("."));
        let workspace = overrides
            .workspace
            .clone()
            .or(file.agent.workspace)
            .map(|path| resolve_path(path, config_dir))
            .unwrap_or_else(|| current_dir.to_path_buf());
        let reload = ReloadSource {
            overrides: overrides.clone(),
            current_dir: current_dir.to_path_buf(),
        };

        Ok(Self {
            path,
            agent: AgentConfig {
                workspace,
                thinking: overrides
                    .thinking
                    .or(file.agent.thinking)
                    .unwrap_or_default(),
                reasoning_mode: overrides
                    .reasoning_mode
                    .or(file.agent.reasoning_mode)
                    .unwrap_or_default(),
                fast_mode: file.agent.fast_mode.unwrap_or(false),
                max_subagents: overrides
                    .max_subagents
                    .or(file.agent.max_subagents)
                    .unwrap_or(DEFAULT_MAX_SUBAGENTS),
                completion_hook: non_empty(file.agent.completion_hook),
            },
            memory: MemoryConfig {
                enabled: file.memory.enabled,
            },
            theme: file.theme,
            reload,
        })
    }

    pub(crate) fn reload(&self) -> Result<ConfigReload> {
        let mut config = Self::load_with(self.reload.overrides.clone(), &self.reload.current_dir)?;
        let workspace_changed = config.agent.workspace != self.agent.workspace;
        config.agent.workspace.clone_from(&self.agent.workspace);
        Ok(ConfigReload {
            config,
            workspace_changed,
        })
    }

    pub(crate) fn set_thinking(&mut self, effort: ReasoningEffort) {
        self.agent.thinking = effort;
    }

    pub(crate) fn set_reasoning_mode(&mut self, mode: ReasoningMode) {
        self.agent.reasoning_mode = mode;
    }

    pub(crate) fn set_fast_mode(&mut self, enabled: bool) {
        self.agent.fast_mode = enabled;
    }

    pub(crate) fn set_max_subagents(&mut self, limit: usize) {
        self.agent.max_subagents = limit;
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn agent(&self) -> &AgentConfig {
        &self.agent
    }

    pub(crate) const fn memory(&self) -> &MemoryConfig {
        &self.memory
    }

    pub(crate) fn memory_path(&self) -> PathBuf {
        self.path
            .parent()
            .unwrap_or(Path::new("."))
            .join("memory/v1.sqlite3")
    }

    pub(crate) const fn theme(&self) -> &Theme {
        &self.theme
    }

    pub(crate) fn persist_thinking(&self, effort: ReasoningEffort) -> Result<()> {
        persist_setting(&self.path, "agent", "thinking", quoted(effort.as_str()))
    }

    pub(crate) fn persist_reasoning_mode(&self, mode: ReasoningMode) -> Result<()> {
        persist_setting(&self.path, "agent", "reasoning_mode", quoted(mode.as_str()))
    }

    pub(crate) fn persist_fast_mode(&self, enabled: bool) -> Result<()> {
        persist_setting(&self.path, "agent", "fast_mode", enabled.to_string())
    }

    pub(crate) fn persist_max_subagents(&self, limit: usize) -> Result<()> {
        persist_setting(&self.path, "agent", "max_subagents", limit.to_string())
    }

    pub(crate) fn persist_theme_mode(&self, mode: ThemeMode) -> Result<()> {
        persist_setting(&self.path, "theme", "mode", quoted(mode.as_str()))
    }
}

impl AgentConfig {
    pub(crate) fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub(crate) const fn thinking(&self) -> ReasoningEffort {
        self.thinking
    }

    pub(crate) const fn reasoning_mode(&self) -> ReasoningMode {
        self.reasoning_mode
    }

    pub(crate) const fn fast_mode(&self) -> bool {
        self.fast_mode
    }

    pub(crate) const fn max_subagents(&self) -> usize {
        self.max_subagents
    }

    pub(crate) fn completion_hook(&self) -> Option<&str> {
        self.completion_hook.as_deref()
    }
}

impl MemoryConfig {
    pub(crate) const fn enabled(&self) -> bool {
        self.enabled
    }
}

impl ConfigReload {
    pub(crate) fn into_parts(self) -> (Config, bool) {
        (self.config, self.workspace_changed)
    }
}

impl ReasoningEffort {
    pub(crate) const ALL: [Self; 5] = [Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
            Self::Xhigh => 3,
            Self::Max => 4,
        }
    }
}

impl From<ReasoningEffort> for Thinking {
    fn from(effort: ReasoningEffort) -> Self {
        match effort {
            ReasoningEffort::Low => Self::Low,
            ReasoningEffort::Medium => Self::Medium,
            ReasoningEffort::High => Self::High,
            ReasoningEffort::Xhigh => Self::Xhigh,
            ReasoningEffort::Max => Self::Max,
        }
    }
}

impl ConfigFile {
    fn read(path: &Path, explicit: bool) -> Result<Self> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(source) if source.kind() == ErrorKind::NotFound && !explicit => String::new(),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                }
                .into());
            }
        };
        toml::from_str(&contents).map_err(|source| {
            ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            }
            .into()
        })
    }
}

fn config_path() -> Result<PathBuf> {
    if let Some(home) = env::var_os("NANOCODEX_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home).join("config.toml"));
    }
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".nanocodex2/config.toml"))
        .ok_or_else(|| ConfigError::ConfigHomeUnavailable.into())
}

fn resolve_path(path: PathBuf, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

fn quoted(value: &str) -> String {
    format!("\"{value}\"")
}

fn persist_setting(path: &Path, section: &str, key: &str, value: String) -> Result<()> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }
            .into());
        }
    };
    toml::from_str::<toml::Table>(&contents).map_err(|source| ConfigError::UpdateParse {
        path: path.to_path_buf(),
        source,
    })?;
    let updated = update_setting(&contents, section, key, &value);
    let parent = path.parent().ok_or_else(|| ConfigError::Write {
        path: path.to_path_buf(),
        source: std::io::Error::other("configuration path has no parent directory"),
    })?;
    fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    temporary
        .write_all(updated.as_bytes())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| ConfigError::Write {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    Ok(())
}

fn update_setting(contents: &str, section: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = contents.lines().map(str::to_owned).collect();
    let header = format!("[{section}]");
    let section_start = lines.iter().position(|line| line.trim() == header);
    if let Some(start) = section_start {
        let end = lines[start + 1..]
            .iter()
            .position(|line| line.trim_start().starts_with('['))
            .map_or(lines.len(), |offset| start + 1 + offset);
        if let Some(line) = lines[start + 1..end].iter_mut().find(|line| {
            line.split_once('=')
                .is_some_and(|(candidate, _)| candidate.trim() == key)
        }) {
            *line = format!("{key} = {value}");
        } else {
            lines.insert(end, format!("{key} = {value}"));
        }
    } else {
        if lines.last().is_some_and(|line| !line.is_empty()) {
            lines.push(String::new());
        }
        lines.push(header);
        lines.push(format!("{key} = {value}"));
    }
    let mut rendered = lines.join("\n");
    rendered.push('\n');
    rendered
}

#[cfg(test)]
mod tests {
    use super::{Config, ConfigOverrides, ReasoningEffort, ReasoningMode, update_setting};
    use std::fs;

    #[test]
    fn defaults_are_the_tui_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "").unwrap();
        let config = Config::load(ConfigOverrides {
            path: Some(path),
            workspace: Some(directory.path().to_path_buf()),
            ..ConfigOverrides::default()
        })
        .unwrap();

        assert_eq!(config.agent().thinking(), ReasoningEffort::Medium);
        assert_eq!(config.agent().reasoning_mode(), ReasoningMode::Standard);
        assert!(!config.agent().fast_mode());
        assert!(!config.memory().enabled());
    }

    #[test]
    fn setting_update_retains_unrelated_content() {
        let input = "# user comment\n[agent]\nfast_mode = false\n\n[future]\nvalue = 7\n";
        let output = update_setting(input, "agent", "fast_mode", "true");
        assert!(output.contains("# user comment"));
        assert!(output.contains("fast_mode = true"));
        assert!(output.contains("[future]\nvalue = 7"));
    }

    #[test]
    fn persistence_round_trips_reasoning_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "# preferences\n").unwrap();
        let config = Config::load(ConfigOverrides {
            path: Some(path.clone()),
            workspace: Some(directory.path().to_path_buf()),
            ..ConfigOverrides::default()
        })
        .unwrap();
        config.persist_thinking(ReasoningEffort::Xhigh).unwrap();
        config.persist_reasoning_mode(ReasoningMode::Pro).unwrap();

        let reloaded = Config::load(ConfigOverrides {
            path: Some(path),
            workspace: Some(directory.path().to_path_buf()),
            ..ConfigOverrides::default()
        })
        .unwrap();
        assert_eq!(reloaded.agent().thinking(), ReasoningEffort::Xhigh);
        assert_eq!(reloaded.agent().reasoning_mode(), ReasoningMode::Pro);
    }
}
