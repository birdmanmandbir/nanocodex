// Modified from clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa for Nanocodex2.
// SPDX-License-Identifier: Apache-2.0

//! Update boundary retained for the imported TUI.

use semver::Version;
use thiserror::Error;

/// Reserved error type for a future Nanocodex2 distribution-owned updater.
#[derive(Debug, Error)]
#[error("Nanocodex2 update discovery is unavailable")]
pub(crate) struct UpdateError;

/// Nanocodex2 deliberately does not query Tact's release channels.
pub(crate) async fn check_for_update() -> Result<Option<Version>, UpdateError> {
    Ok(None)
}
