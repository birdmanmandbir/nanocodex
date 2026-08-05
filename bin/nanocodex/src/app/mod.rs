//! Application boundaries for configuration, authentication, and command dispatch.
//!
//! Derived from Tact v0.3.6 (Apache-2.0), modified for Nanocodex.

#![allow(dead_code)]
#![allow(
    clippy::missing_const_for_fn,
    clippy::redundant_clone,
    clippy::use_self
)]

mod auth;
pub(crate) mod browser;
pub(crate) mod config;
pub(crate) mod error;
pub(crate) mod installation;
mod secret;
pub(crate) mod update;
