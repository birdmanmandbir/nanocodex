use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use http::{HeaderName, Method};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use url::Url;

#[cfg(feature = "onepassword-connect")]
mod onepassword_connect;
#[cfg(feature = "onepassword-sdk")]
mod onepassword_sdk;

#[cfg(feature = "onepassword-connect")]
pub use onepassword_connect::{OnePasswordConnectConfigError, OnePasswordConnectSecretManager};
#[cfg(feature = "onepassword-sdk")]
pub use onepassword_sdk::{
    ONEPASSWORD_CORE_SHA256, ONEPASSWORD_CORE_URL, ONEPASSWORD_CORE_VERSION,
    OnePasswordSdkConfigError, OnePasswordSdkSecretManager,
};

const MAX_FILE_SECRET_BYTES: u64 = 1_024 * 1_024;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_ENVIRONMENT_NAME_BYTES: usize = 128;
const MAX_HEADER_NAME_BYTES: usize = 128;
const MAX_PATH_PREFIX_BYTES: usize = 2_048;

/// Opaque host-side secret-manager reference.
///
/// The reference is safe configuration. This egress layer never places the
/// value it resolves to in a guest lease, snapshot, model input, or durable
/// agent state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    provider: String,
    key: String,
}

impl SecretRef {
    /// Creates a provider-qualified reference.
    #[must_use]
    pub fn new(provider: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            key: key.into(),
        }
    }

    /// Returns the provider registry name.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Returns the provider-owned opaque key.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }
}

/// HTTP method accepted by one secret request rule.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SecretHttpMethod {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `PATCH`.
    Patch,
    /// `DELETE`.
    Delete,
    /// `HEAD`.
    Head,
    /// `OPTIONS`.
    Options,
}

impl SecretHttpMethod {
    pub(crate) fn matches(self, method: &Method) -> bool {
        matches!(
            (self, method),
            (Self::Get, &Method::GET)
                | (Self::Post, &Method::POST)
                | (Self::Put, &Method::PUT)
                | (Self::Patch, &Method::PATCH)
                | (Self::Delete, &Method::DELETE)
                | (Self::Head, &Method::HEAD)
                | (Self::Options, &Method::OPTIONS)
        )
    }
}

/// Method and normalized path scope for one secret.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRequestRule {
    methods: BTreeSet<SecretHttpMethod>,
    path_prefixes: Vec<String>,
}

impl SecretRequestRule {
    /// Creates a rule that initially accepts every supported method and path.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one accepted method.
    #[must_use]
    pub fn method(mut self, method: SecretHttpMethod) -> Self {
        self.methods.insert(method);
        self
    }

    /// Adds one absolute path-segment prefix such as `/v1/responses`.
    #[must_use]
    pub fn path_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.path_prefixes.push(prefix.into());
        self
    }

    /// Returns the accepted methods. An empty set accepts every supported method.
    #[must_use]
    pub const fn methods(&self) -> &BTreeSet<SecretHttpMethod> {
        &self.methods
    }

    /// Returns accepted absolute path prefixes. An empty list accepts every path.
    #[must_use]
    pub fn path_prefixes(&self) -> &[String] {
        &self.path_prefixes
    }
}

/// Host-side credential delivery applied after request authorization.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SecretDelivery {
    /// Replaces the complete header with `prefix + resolved_secret`.
    InjectHeader {
        /// Header name, for example `authorization`.
        header: String,
        /// Non-secret prefix, for example `Bearer `.
        prefix: String,
    },
    /// Replaces an exact placeholder inside a caller-supplied header.
    ReplaceHeader {
        /// Header name containing the placeholder.
        header: String,
        /// Public placeholder exported to the guest.
        placeholder: String,
    },
}

impl SecretDelivery {
    /// Creates complete header injection.
    #[must_use]
    pub fn inject_header(header: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self::InjectHeader {
            header: header.into(),
            prefix: prefix.into(),
        }
    }

    /// Creates placeholder replacement inside an existing header.
    #[must_use]
    pub fn replace_header(header: impl Into<String>, placeholder: impl Into<String>) -> Self {
        Self::ReplaceHeader {
            header: header.into(),
            placeholder: placeholder.into(),
        }
    }

    /// Returns the credential-bearing header name.
    #[must_use]
    pub fn header(&self) -> &str {
        match self {
            Self::InjectHeader { header, .. } | Self::ReplaceHeader { header, .. } => header,
        }
    }

    pub(crate) fn placeholder(&self) -> Option<&str> {
        match self {
            Self::InjectHeader { .. } => None,
            Self::ReplaceHeader { placeholder, .. } => Some(placeholder),
        }
    }
}

/// Public guest configuration associated with one secret route.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretGuestConfig {
    #[serde(rename = "base_url_env", alias = "base_url_environment")]
    base_url_environment: String,
    #[serde(
        default,
        rename = "placeholder_env",
        alias = "placeholder_environment",
        skip_serializing_if = "Option::is_none"
    )]
    placeholder_environment: Option<String>,
}

impl SecretGuestConfig {
    /// Creates guest configuration exporting the route origin under one name.
    #[must_use]
    pub fn new(base_url_environment: impl Into<String>) -> Self {
        Self {
            base_url_environment: base_url_environment.into(),
            placeholder_environment: None,
        }
    }

    /// Exports the public replacement placeholder under one environment name.
    #[must_use]
    pub fn placeholder_environment(mut self, name: impl Into<String>) -> Self {
        self.placeholder_environment = Some(name.into());
        self
    }

    /// Returns the environment name receiving the route origin.
    #[must_use]
    pub fn base_url_environment(&self) -> &str {
        &self.base_url_environment
    }

    /// Returns the environment name receiving a public placeholder.
    #[must_use]
    pub fn placeholder_environment_name(&self) -> Option<&str> {
        self.placeholder_environment.as_deref()
    }
}

/// Validated host-side secret route.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SecretSpec {
    id: String,
    source: SecretRef,
    upstream: String,
    rules: Vec<SecretRequestRule>,
    delivery: SecretDelivery,
    guest: SecretGuestConfig,
}

impl SecretSpec {
    /// Starts a validated secret-route builder.
    #[must_use]
    pub fn builder(
        id: impl Into<String>,
        source: SecretRef,
        upstream: impl Into<String>,
        delivery: SecretDelivery,
        guest: SecretGuestConfig,
    ) -> SecretSpecBuilder {
        SecretSpecBuilder {
            id: id.into(),
            source,
            upstream: upstream.into(),
            rules: Vec::new(),
            delivery,
            guest,
        }
    }

    /// Returns the stable policy identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the host secret-manager reference.
    #[must_use]
    pub const fn source(&self) -> &SecretRef {
        &self.source
    }

    /// Returns the authorized HTTP(S) upstream base URL.
    #[must_use]
    pub fn upstream(&self) -> &str {
        &self.upstream
    }

    /// Returns request authorization rules.
    #[must_use]
    pub fn rules(&self) -> &[SecretRequestRule] {
        &self.rules
    }

    /// Returns the host-side header delivery policy.
    #[must_use]
    pub const fn delivery(&self) -> &SecretDelivery {
        &self.delivery
    }

    /// Returns public guest configuration.
    #[must_use]
    pub const fn guest(&self) -> &SecretGuestConfig {
        &self.guest
    }

    pub(crate) fn validate(&self) -> Result<(), SecretConfigError> {
        validate_spec(self)
    }
}

/// Builder for one validated [`SecretSpec`].
pub struct SecretSpecBuilder {
    id: String,
    source: SecretRef,
    upstream: String,
    rules: Vec<SecretRequestRule>,
    delivery: SecretDelivery,
    guest: SecretGuestConfig,
}

impl SecretSpecBuilder {
    /// Adds one request authorization rule.
    #[must_use]
    pub fn rule(mut self, rule: SecretRequestRule) -> Self {
        self.rules.push(rule);
        self
    }

    /// Validates and creates the secret route.
    ///
    /// # Errors
    ///
    /// Returns a typed error for unsafe identifiers, origins, headers, paths,
    /// environment names, or placeholder configuration.
    pub fn build(self) -> Result<SecretSpec, SecretConfigError> {
        let spec = SecretSpec {
            id: self.id,
            source: self.source,
            upstream: self.upstream,
            rules: self.rules,
            delivery: self.delivery,
            guest: self.guest,
        };
        spec.validate()?;
        Ok(spec)
    }
}

fn validate_spec(spec: &SecretSpec) -> Result<(), SecretConfigError> {
    validate_identifier(&spec.id).ok_or(SecretConfigError::InvalidId)?;
    if spec.source.provider.trim().is_empty()
        || spec.source.key.trim().is_empty()
        || spec.source.provider.len() > MAX_IDENTIFIER_BYTES
        || spec.source.key.len() > 4 * 1_024
    {
        return Err(SecretConfigError::InvalidSource);
    }
    let upstream = Url::parse(&spec.upstream).map_err(|_| SecretConfigError::InvalidUpstream)?;
    if !matches!(upstream.scheme(), "http" | "https")
        || upstream.host_str().is_none()
        || !upstream.username().is_empty()
        || upstream.password().is_some()
        || upstream.query().is_some()
        || upstream.fragment().is_some()
        || !valid_path_prefix(upstream.path())
    {
        return Err(SecretConfigError::InvalidUpstream);
    }
    for rule in &spec.rules {
        for prefix in &rule.path_prefixes {
            if !valid_path_prefix(prefix) {
                return Err(SecretConfigError::InvalidPathPrefix);
            }
        }
    }
    let header = spec.delivery.header();
    if header.is_empty()
        || header.len() > MAX_HEADER_NAME_BYTES
        || HeaderName::from_bytes(header.as_bytes()).is_err()
        || [
            "host",
            "content-length",
            "transfer-encoding",
            "connection",
            "proxy-authorization",
        ]
        .iter()
        .any(|reserved| header.eq_ignore_ascii_case(reserved))
    {
        return Err(SecretConfigError::InvalidHeader);
    }
    validate_environment_name(&spec.guest.base_url_environment)?;
    if let Some(environment) = &spec.guest.placeholder_environment {
        validate_environment_name(environment)?;
    }
    if let Some(placeholder) = spec.delivery.placeholder()
        && spec.guest.placeholder_environment.as_deref() != Some(placeholder)
    {
        return Err(SecretConfigError::PlaceholderMismatch);
    }
    Ok(())
}

fn validate_identifier(value: &str) -> Option<()> {
    (!value.is_empty()
        && value.len() <= MAX_IDENTIFIER_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')))
    .then_some(())
}

fn valid_path_prefix(prefix: &str) -> bool {
    prefix.starts_with('/')
        && prefix.len() <= MAX_PATH_PREFIX_BYTES
        && !prefix.contains('\\')
        && !prefix.split('/').any(|segment| segment == "..")
        && !contains_ambiguous_path_escape(prefix)
}

pub(crate) fn safe_request_path(path: &str) -> Option<String> {
    if path.contains('\\')
        || path.split('/').any(|segment| segment == "..")
        || contains_ambiguous_path_escape(path)
    {
        return None;
    }
    Some(format!("/{}", path.trim_start_matches('/')))
}

fn contains_ambiguous_path_escape(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            index += 1;
            continue;
        }
        let encoded = bytes
            .get(index + 1..index + 3)
            .and_then(|digits| decode_hex_byte(digits[0], digits[1]));
        let Some(encoded) = encoded else {
            return true;
        };
        if matches!(encoded, b'%' | b'.' | b'/' | b'\\' | b'?' | b'#') {
            return true;
        }
        index += 3;
    }
    false
}

fn decode_hex_byte(high: u8, low: u8) -> Option<u8> {
    Some(hex_value(high)? << 4 | hex_value(low)?)
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

fn validate_environment_name(value: &str) -> Result<(), SecretConfigError> {
    let mut bytes = value.bytes();
    let valid_start = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_uppercase() || byte == b'_');
    if value.len() > MAX_ENVIRONMENT_NAME_BYTES
        || !valid_start
        || !bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        Err(SecretConfigError::InvalidEnvironmentName)
    } else {
        Ok(())
    }
}

/// Pluggable host boundary for environment, Vault, cloud KMS, or Iron secrets.
#[async_trait]
pub trait SecretManager: Send + Sync {
    /// Resolves one opaque reference without exposing it to the guest.
    async fn resolve(&self, reference: &SecretRef) -> Result<String, SecretError>;
}

/// Bounded file-backed secret manager rooted at one canonical directory.
pub struct FileSecretManager {
    root: PathBuf,
}

impl FileSecretManager {
    /// Creates a file provider suitable for Docker secrets, tmpfs, or CSI.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be canonicalized or is not a
    /// directory.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, SecretError> {
        let root = std::fs::canonicalize(root).map_err(SecretError::Io)?;
        if !root.is_dir() {
            return Err(SecretError::InvalidRoot);
        }
        Ok(Self { root })
    }
}

#[async_trait]
impl SecretManager for FileSecretManager {
    async fn resolve(&self, reference: &SecretRef) -> Result<String, SecretError> {
        let relative = Path::new(&reference.key);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(SecretError::InvalidKey);
        }
        let canonical = tokio::fs::canonicalize(self.root.join(relative))
            .await
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    SecretError::NotFound {
                        provider: reference.provider.clone(),
                        key: reference.key.clone(),
                    }
                } else {
                    SecretError::Io(error)
                }
            })?;
        if !canonical.starts_with(&self.root) {
            return Err(SecretError::InvalidKey);
        }
        let file = tokio::fs::File::open(canonical)
            .await
            .map_err(SecretError::Io)?;
        let metadata = file.metadata().await.map_err(SecretError::Io)?;
        if !metadata.is_file() || metadata.len() > MAX_FILE_SECRET_BYTES {
            return Err(SecretError::InvalidFile);
        }
        let mut value = String::new();
        file.take(MAX_FILE_SECRET_BYTES + 1)
            .read_to_string(&mut value)
            .await
            .map_err(SecretError::Io)?;
        if value.len() as u64 > MAX_FILE_SECRET_BYTES {
            return Err(SecretError::InvalidFile);
        }
        Ok(value.trim_end_matches(['\r', '\n']).to_owned())
    }
}

/// Host environment secret manager with an explicit variable prefix.
pub struct EnvironmentSecretManager {
    prefix: String,
}

impl EnvironmentSecretManager {
    /// Creates an environment provider; `NANOCODEX_SECRET_` is a typical prefix.
    #[must_use]
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }
}

#[async_trait]
impl SecretManager for EnvironmentSecretManager {
    async fn resolve(&self, reference: &SecretRef) -> Result<String, SecretError> {
        let name = format!("{}{}", self.prefix, reference.key);
        env::var(&name).map_err(|_| SecretError::NotFound {
            provider: reference.provider.clone(),
            key: reference.key.clone(),
        })
    }
}

/// Heterogeneous provider registry keyed by [`SecretRef::provider`].
#[derive(Clone, Default)]
pub struct CompositeSecretManager {
    providers: BTreeMap<String, Arc<dyn SecretManager>>,
}

impl CompositeSecretManager {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs or replaces one named host provider.
    #[must_use]
    pub fn provider(mut self, name: impl Into<String>, provider: Arc<dyn SecretManager>) -> Self {
        self.providers.insert(name.into(), provider);
        self
    }
}

#[async_trait]
impl SecretManager for CompositeSecretManager {
    async fn resolve(&self, reference: &SecretRef) -> Result<String, SecretError> {
        self.providers
            .get(&reference.provider)
            .ok_or_else(|| SecretError::UnknownProvider(reference.provider.clone()))?
            .resolve(reference)
            .await
    }
}

/// Secret-manager resolution failure.
#[derive(Debug, Error)]
pub enum SecretError {
    /// No manager is registered for the reference provider.
    #[error("unknown secret provider `{0}`")]
    UnknownProvider(String),
    /// The provider could not find the requested key.
    #[error("secret `{provider}:{key}` was not found")]
    NotFound {
        /// Provider registry name.
        provider: String,
        /// Opaque provider key.
        key: String,
    },
    /// A provider returned an intentionally opaque failure.
    #[error("secret provider failed: {0}")]
    Provider(String),
    /// A provider rejected the reference syntax.
    #[error("secret reference `{provider}:{key}` is invalid")]
    InvalidReference {
        /// Provider registry name.
        provider: String,
        /// Rejected opaque key.
        key: String,
    },
    /// A file key was not a safe relative path.
    #[error("secret key is not a safe relative path")]
    InvalidKey,
    /// A file provider root was not a directory.
    #[error("secret file provider root must be a directory")]
    InvalidRoot,
    /// A file secret was not a bounded regular UTF-8 file.
    #[error("secret file must be a bounded regular UTF-8 file")]
    InvalidFile,
    /// A file provider operation failed.
    #[error("secret file provider failed")]
    Io(#[source] std::io::Error),
}

/// Invalid secret route configuration.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SecretConfigError {
    /// The secret ID was empty, too long, or contained unsafe bytes.
    #[error("secret id must use 1 to 256 ASCII letters, digits, dots, dashes, or underscores")]
    InvalidId,
    /// The manager provider or key was empty or unbounded.
    #[error("secret source provider and key must be non-empty and bounded")]
    InvalidSource,
    /// The upstream was not a credential-free absolute HTTP(S) base URL.
    #[error(
        "secret upstream must be a bounded absolute HTTP(S) base URL without credentials or query"
    )]
    InvalidUpstream,
    /// A rule path was not an unambiguous bounded absolute prefix.
    #[error("secret rule path prefixes must be safe bounded absolute paths")]
    InvalidPathPrefix,
    /// The delivery header was invalid or transport-owned.
    #[error("secret delivery header is invalid or unsafe")]
    InvalidHeader,
    /// A guest environment name was not an uppercase shell identifier.
    #[error("secret guest environment names must use uppercase shell identifier syntax")]
    InvalidEnvironmentName,
    /// Replace delivery and guest placeholder configuration differed.
    #[error("replace delivery placeholder must equal guest placeholder environment name")]
    PlaceholderMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn openai_spec() -> SecretSpec {
        SecretSpec::builder(
            "openai",
            SecretRef::new("environment", "OPENAI_API_KEY"),
            "https://api.openai.com",
            SecretDelivery::inject_header("authorization", "Bearer "),
            SecretGuestConfig::new("OPENAI_BASE_URL"),
        )
        .rule(
            SecretRequestRule::new()
                .method(SecretHttpMethod::Post)
                .path_prefix("/v1/responses"),
        )
        .build()
        .unwrap()
    }

    #[test]
    fn builder_produces_a_scoped_route() {
        let spec = openai_spec();
        assert_eq!(spec.id(), "openai");
        assert_eq!(spec.upstream(), "https://api.openai.com");
        assert_eq!(spec.rules().len(), 1);
    }

    #[test]
    fn rejects_credential_origins_and_ambiguous_prefixes() {
        let error = SecretSpec::builder(
            "openai",
            SecretRef::new("environment", "OPENAI_API_KEY"),
            "https://token@example.com",
            SecretDelivery::inject_header("authorization", "Bearer "),
            SecretGuestConfig::new("OPENAI_BASE_URL"),
        )
        .build()
        .unwrap_err();
        assert_eq!(error, SecretConfigError::InvalidUpstream);

        let error = SecretSpec::builder(
            "openai",
            SecretRef::new("environment", "OPENAI_API_KEY"),
            "https://api.openai.com",
            SecretDelivery::inject_header("authorization", "Bearer "),
            SecretGuestConfig::new("OPENAI_BASE_URL"),
        )
        .rule(SecretRequestRule::new().path_prefix("/v1/%2e%2e/admin"))
        .build()
        .unwrap_err();
        assert_eq!(error, SecretConfigError::InvalidPathPrefix);

        let error = SecretSpec::builder(
            "../escape",
            SecretRef::new("environment", "OPENAI_API_KEY"),
            "https://api.openai.com",
            SecretDelivery::inject_header("authorization", "Bearer "),
            SecretGuestConfig::new("OPENAI_BASE_URL"),
        )
        .build()
        .unwrap_err();
        assert_eq!(error, SecretConfigError::InvalidId);
    }

    #[test]
    fn request_paths_reject_every_ambiguous_encoding_before_policy_matching() {
        assert_eq!(
            safe_request_path("/allowed/resource").as_deref(),
            Some("/allowed/resource")
        );
        for path in [
            "/allowed/../admin",
            "/allowed/%2e%2e/admin",
            "/allowed/%2E%2E/admin",
            "/allowed/%252e%252e/admin",
            "/allowed%2f..%2fadmin",
            "/allowed%5c..%5cadmin",
            "/allowed\\..\\admin",
            "/allowed/%",
            "/allowed/%zz",
        ] {
            assert!(
                safe_request_path(path).is_none(),
                "ambiguous path was accepted: {path}"
            );
        }
    }

    #[tokio::test]
    async fn file_provider_rejects_escape_and_oversized_values() {
        let directory = tempfile::tempdir().unwrap();
        let manager = FileSecretManager::new(directory.path()).unwrap();
        let error = manager
            .resolve(&SecretRef::new("file", "../outside"))
            .await
            .unwrap_err();
        assert!(matches!(error, SecretError::InvalidKey));

        let path = directory.path().join("oversized");
        let file = std::fs::File::create(path).unwrap();
        file.set_len(MAX_FILE_SECRET_BYTES + 1).unwrap();
        let error = manager
            .resolve(&SecretRef::new("file", "oversized"))
            .await
            .unwrap_err();
        assert!(matches!(error, SecretError::InvalidFile));
    }
}
