//! Prime-style recursive orchestration built as a thin Nanocodex consumer.

#![deny(missing_docs, rustdoc::broken_intra_doc_links)]

mod model;
mod runtime;
mod snapshot;
mod tools;

pub use model::{
    RlmAgentEvidence, RlmAgentId, RlmAgentSummary, RlmEvent, RlmEventKind, RlmEvidence, RlmMessage,
    RlmStatus, RlmTurnEvidence, RlmUsage,
};
pub use runtime::{RlmPolicy, RlmPolicyError, RlmRuntime, RlmRuntimeError, RlmTools};
pub use snapshot::{
    HarnessSnapshot, LaunchSnapshot, PromptNote, PromptPack, SnapshotError, SubagentSpec,
    ToolDescriptions,
};
