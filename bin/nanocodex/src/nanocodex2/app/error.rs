// Modified from clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa for Nanocodex2.
// SPDX-License-Identifier: Apache-2.0

//! Typed errors used by the imported terminal boundary.

use crate::tui::{session::SessionError, transcript::TranscriptError};
use std::{env::VarError, io, path::PathBuf, result::Result as StdResult};
use thiserror::Error;

pub(crate) type Result<T> = StdResult<T, Error>;

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error(transparent)]
    Agent(#[from] crate::engine::EngineError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    ExternalEditor(#[from] ExternalEditorError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Transcript(#[from] TranscriptError),
}

#[derive(Debug, Error)]
pub(crate) enum ExternalEditorError {
    #[error("$EDITOR is unavailable: {0}")]
    Unavailable(#[source] VarError),
    #[error("failed to parse $EDITOR value `{command}`")]
    Parse { command: String },
    #[error("failed to create an external-editor draft: {0}")]
    CreateDraft(#[source] io::Error),
    #[error("failed to write the external-editor draft: {0}")]
    WriteDraft(#[source] io::Error),
    #[error("failed to launch external editor `{program}`: {source}")]
    Launch {
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to read the external-editor draft: {0}")]
    ReadDraft(#[source] io::Error),
}

#[derive(Debug, Error)]
pub(crate) enum ConfigError {
    #[error(
        "could not determine the config directory; set NANOCODEX_HOME or pass an explicit config path"
    )]
    ConfigHomeUnavailable,
    #[error("failed to determine the current directory: {0}")]
    CurrentDirectory(#[source] io::Error),
    #[error("failed to read configuration file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse configuration file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to update configuration file {path}: {source}")]
    UpdateParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to write configuration file {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Error)]
pub(crate) enum RuntimeError {
    #[error(
        "interactive mode requires terminal stdin and stdout; use a Nanocodex2 headless command for JSONL output"
    )]
    InteractiveTerminal,
    #[error("terminal operation failed: {0}")]
    Terminal(#[source] io::Error),
    #[error("the external-editor task stopped unexpectedly: {0}")]
    ExternalEditorTask(#[source] tokio::task::JoinError),
    #[error("the effort update task stopped unexpectedly: {0}")]
    EffortUpdateTask(#[source] tokio::task::JoinError),
    #[error("the fast-mode update task stopped unexpectedly: {0}")]
    FastModeUpdateTask(#[source] tokio::task::JoinError),
    #[error("the new-session task stopped unexpectedly: {0}")]
    NewSessionTask(#[source] tokio::task::JoinError),
    #[error("the handoff task stopped unexpectedly: {0}")]
    HandoffTask(#[source] tokio::task::JoinError),
    #[error("the session task stopped unexpectedly: {0}")]
    SessionTask(#[source] tokio::task::JoinError),
    #[error("the managed-agent worker stopped before accepting a command")]
    AgentWorkerStopped,
}
