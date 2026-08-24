// Modified from clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa for Nanocodex2.
// SPDX-License-Identifier: Apache-2.0

//! Private data-model compatibility for the imported Tact TUI.
//!
//! These modules intentionally contain only UI-facing values and deterministic helpers. Managed
//! agent execution, child-agent orchestration, memory persistence, and remote memory access belong
//! to their owning application boundaries.

pub(crate) mod memory;
pub(crate) mod subagents;
