//! Host-owned outbound capabilities for isolated Nanocodex runtimes.
//!
//! This crate composes payment handling and scoped secret injection behind one
//! authenticated HTTP(S) proxy. A guest receives only a [`nanovm::EgressLease`]
//! containing the proxy capability and its public CA. Wallets, secret-manager
//! clients, resolved values, policy state, and revocation guards stay in the
//! host process.
//!
//! [`SecretManager`] and [`SecretPolicy`] are independently useful boundaries.
//! [`VmEgress`] combines them with MPP for a concrete VM session.
//!
//! # Define a scoped secret
//!
//! ```
//! use std::sync::Arc;
//!
//! use mpp::client::MultiProvider;
//! use nanocodex_vm_egress::{
//!     CompositeSecretManager, EgressContext, EnvironmentSecretManager,
//!     SecretDelivery, SecretGuestConfig, SecretHttpMethod, SecretRef,
//!     SecretRequestRule, SecretSpec, StaticSecretPolicy, VmEgress,
//! };
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let openai = SecretSpec::builder(
//!     "openai",
//!     SecretRef::new("environment", "OPENAI_API_KEY"),
//!     "https://api.openai.com",
//!     SecretDelivery::inject_header("authorization", "Bearer "),
//!     SecretGuestConfig::new("OPENAI_BASE_URL"),
//! )
//! .rule(
//!     SecretRequestRule::new()
//!         .method(SecretHttpMethod::Post)
//!         .path_prefix("/v1/responses"),
//! )
//! .build()?;
//!
//! assert_eq!(openai.id(), "openai");
//!
//! // The host variable `NANOCODEX_SECRET_OPENAI_API_KEY` is resolved for each
//! // authorized request. Its value never enters the returned VM lease.
//! let manager = CompositeSecretManager::new().provider(
//!     "environment",
//!     Arc::new(EnvironmentSecretManager::new("NANOCODEX_SECRET_")),
//! );
//! let egress = VmEgress::builder(MultiProvider::new())
//!     .secrets(
//!         EgressContext::new(
//!             "agent-019c-0000-7000-8000-000000000001",
//!             "service:nanocodex-local",
//!         ),
//!         Arc::new(StaticSecretPolicy::new([openai])),
//!         Arc::new(manager),
//!     )
//!     .spawn()
//!     .await?;
//! let lease = egress.lease();
//! assert_eq!(
//!     lease.guest_environment().get("OPENAI_BASE_URL"),
//!     Some(&"https://api.openai.com".to_owned()),
//! );
//!
//! drop(lease);
//! egress.shutdown().await?;
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs, rustdoc::broken_intra_doc_links)]

mod gateway;
mod provider;
mod route_gateway;
mod secret;

pub use gateway::{
    EgressContext, SecretEgressError, SecretPolicy, SecretPolicyError, StaticSecretPolicy,
    UnmatchedEgress, VmEgress, VmEgressBuilder, VmEgressError,
};
pub use provider::{MppVmEgressError, mpp_egress_layer};
pub use route_gateway::{
    MAX_SECRET_GATEWAY_REQUEST_BYTES, MAX_SECRET_GATEWAY_RESPONSE_BYTES, SecretGateway,
    SecretGatewayError,
};
pub use secret::{
    CompositeSecretManager, EnvironmentSecretManager, FileSecretManager, SecretConfigError,
    SecretDelivery, SecretError, SecretGuestConfig, SecretHttpMethod, SecretManager, SecretRef,
    SecretRequestRule, SecretSpec, SecretSpecBuilder,
};
#[cfg(feature = "onepassword-sdk")]
pub use secret::{
    ONEPASSWORD_CORE_SHA256, ONEPASSWORD_CORE_URL, ONEPASSWORD_CORE_VERSION,
    OnePasswordSdkConfigError, OnePasswordSdkSecretManager,
};
#[cfg(feature = "onepassword-connect")]
pub use secret::{OnePasswordConnectConfigError, OnePasswordConnectSecretManager};
