// Modified from clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa for Nanocodex2.
// SPDX-License-Identifier: Apache-2.0

//! Installation classification for UI-only distribution affordances.

/// Nanocodex2 currently has no self-update distribution contract. Treat every
/// build as development so the TUI never advertises Tact's updater.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum InstallationKind {
    Development,
}

impl InstallationKind {
    pub(crate) const fn is_development(&self) -> bool {
        true
    }
}

pub(crate) const fn current() -> &'static InstallationKind {
    &InstallationKind::Development
}
