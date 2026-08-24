// Modified from clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa for Nanocodex2.
// SPDX-License-Identifier: Apache-2.0


//! Stable identities for sessions that can move between primary and fork roles.

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum PaneId {
    Main,
    Fork(u64),
}
