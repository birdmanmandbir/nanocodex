use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use nanocodex_vm_egress::{
    CompositeSecretManager, EnvironmentSecretManager, FileSecretManager,
    MAX_SECRET_GATEWAY_REQUEST_BYTES, MAX_SECRET_GATEWAY_RESPONSE_BYTES, SecretConfigError,
    SecretDelivery, SecretError, SecretGateway, SecretGatewayError, SecretGuestConfig,
    SecretHttpMethod, SecretManager, SecretRef, SecretRequestRule, SecretSpec,
};
#[cfg(feature = "onepassword-sdk")]
pub use nanocodex_vm_egress::{
    ONEPASSWORD_CORE_SHA256, ONEPASSWORD_CORE_URL, ONEPASSWORD_CORE_VERSION,
    OnePasswordSdkConfigError, OnePasswordSdkSecretManager,
};
#[cfg(feature = "onepassword-connect")]
pub use nanocodex_vm_egress::{OnePasswordConnectConfigError, OnePasswordConnectSecretManager};

/// Administrative request for one host-resolved secret route.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSecret {
    /// Optional stable route identifier. A UUID is generated when omitted.
    pub id: Option<String>,
    /// Human-readable administrative name.
    pub name: String,
    /// Opaque provider-qualified reference; never resolved into `SQLite`.
    pub source: SecretRef,
    /// Credential-free HTTP(S) origin eligible to receive the secret.
    pub upstream: String,
    /// Allowed method and path scopes.
    #[serde(default)]
    pub rules: Vec<SecretRequestRule>,
    /// Host-side header injection behavior.
    pub delivery: SecretDelivery,
    /// Public environment projected into an authorized guest.
    pub guest: SecretGuestConfig,
}

/// Partial administrative update for one secret route.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PatchSecret {
    /// Replaces the human-readable name.
    pub name: Option<String>,
    /// Replaces the provider-qualified reference.
    pub source: Option<SecretRef>,
    /// Replaces the fixed upstream origin.
    pub upstream: Option<String>,
    /// Replaces all request scopes.
    pub rules: Option<Vec<SecretRequestRule>>,
    /// Replaces host-side delivery behavior.
    pub delivery: Option<SecretDelivery>,
    /// Replaces public guest configuration.
    pub guest: Option<SecretGuestConfig>,
    /// Enables or disables the route.
    pub enabled: Option<bool>,
}

/// Durable administrative view of one secret route.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SecretView {
    /// Stable route identifier.
    pub id: String,
    /// Human-readable administrative name.
    pub name: String,
    /// Opaque provider-qualified reference.
    pub source: SecretRef,
    /// Fixed credential-free upstream origin.
    pub upstream: String,
    /// Allowed method and path scopes.
    pub rules: Vec<SecretRequestRule>,
    /// Host-side header injection behavior.
    pub delivery: SecretDelivery,
    /// Public environment projected into an authorized guest.
    pub guest: SecretGuestConfig,
    /// Whether this route can currently be granted.
    pub enabled: bool,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last administrative update time.
    pub updated_at: DateTime<Utc>,
}

impl SecretView {
    /// Validates and projects this managed record into the reusable egress type.
    ///
    /// # Errors
    ///
    /// Returns an error when persisted route configuration violates the
    /// current egress contract.
    pub fn to_spec(&self) -> Result<SecretSpec, SecretConfigError> {
        build_spec(
            self.id.clone(),
            self.source.clone(),
            self.upstream.clone(),
            self.rules.clone(),
            self.delivery.clone(),
            self.guest.clone(),
        )
    }
}

pub(crate) fn validate_secret(secret: &CreateSecret) -> Result<(), SecretConfigError> {
    build_spec(
        secret.id.clone().unwrap_or_else(|| "generated".to_owned()),
        secret.source.clone(),
        secret.upstream.clone(),
        secret.rules.clone(),
        secret.delivery.clone(),
        secret.guest.clone(),
    )
    .map(drop)
}

fn build_spec(
    id: String,
    source: SecretRef,
    upstream: String,
    rules: Vec<SecretRequestRule>,
    delivery: SecretDelivery,
    guest: SecretGuestConfig,
) -> Result<SecretSpec, SecretConfigError> {
    let mut builder = SecretSpec::builder(id, source, upstream, delivery, guest);
    for rule in rules {
        builder = builder.rule(rule);
    }
    builder.build()
}
