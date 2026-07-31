use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
    sync::Arc,
};

use async_trait::async_trait;
use nanocodex_egress::{
    EgressProxy, SecretEgress, SecretResolver, SecretResolverError, UnmatchedEgress,
};
pub use nanocodex_vm::host::EgressLease;
use nanocodex_vm::host::{EgressError as LeaseError, EgressFile, Network};
use thiserror::Error;
use url::Url;

use crate::{CapabilityName, PolicyStore, secrets::MANAGED_SECRET_PROVIDER};

/// Server-derived identity supplied to an egress provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressContext {
    agent_id: String,
    principal: String,
}

impl EgressContext {
    /// Creates an egress identity from authenticated managed-service state.
    #[must_use]
    pub fn new(agent_id: impl Into<String>, principal: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            principal: principal.into(),
        }
    }

    /// Returns the managed agent identifier.
    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Returns the effective principal identifier.
    #[must_use]
    pub fn principal(&self) -> &str {
        &self.principal
    }
}

/// Managed policy boundary that resolves capability names into one VM lease.
#[async_trait]
pub trait EgressProvider: Send + Sync {
    /// Acquires outbound access for one authenticated managed agent.
    ///
    /// # Errors
    ///
    /// Returns an error when a capability is denied, two routes conflict, or
    /// host-side proxy provisioning fails.
    async fn acquire(
        &self,
        context: &EgressContext,
        requested: &BTreeSet<CapabilityName>,
    ) -> Result<EgressLease, EgressError>;
}

/// One server-configured external proxy route.
#[derive(Clone)]
pub struct ProxyProfile {
    name: String,
    proxy_url: String,
    no_proxy: String,
    ca_certificate: Option<PathBuf>,
}

impl ProxyProfile {
    /// Creates a profile after validating an absolute HTTP(S) proxy URL.
    ///
    /// # Errors
    ///
    /// Returns [`EgressError::InvalidProxyUrl`] for a relative or unsupported URL.
    pub fn new(name: impl Into<String>, proxy_url: impl Into<String>) -> Result<Self, EgressError> {
        let name = name.into();
        let proxy_url = proxy_url.into();
        let parsed = Url::parse(&proxy_url).map_err(|_| EgressError::InvalidProxyUrl)?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(EgressError::InvalidProxyUrl);
        }
        Ok(Self {
            name,
            proxy_url,
            no_proxy: String::new(),
            ca_certificate: None,
        })
    }

    /// Sets the standard proxy bypass list exported to the guest.
    #[must_use]
    pub fn no_proxy(mut self, value: impl Into<String>) -> Self {
        self.no_proxy = value.into();
        self
    }

    /// Sets a certificate path already present inside the guest rootfs.
    #[must_use]
    pub fn ca_certificate(mut self, path: impl Into<PathBuf>) -> Self {
        self.ca_certificate = Some(path.into());
        self
    }

    fn lease(&self) -> Result<EgressLease, EgressError> {
        let mut lease = EgressLease::internet();
        for (name, value) in self.environment() {
            lease.insert_environment(name, value)?;
        }
        Ok(lease)
    }

    fn environment(&self) -> BTreeMap<String, String> {
        let mut environment = BTreeMap::from([
            ("http_proxy".to_owned(), self.proxy_url.clone()),
            ("https_proxy".to_owned(), self.proxy_url.clone()),
            ("HTTP_PROXY".to_owned(), self.proxy_url.clone()),
            ("HTTPS_PROXY".to_owned(), self.proxy_url.clone()),
            ("no_proxy".to_owned(), self.no_proxy.clone()),
            ("NO_PROXY".to_owned(), self.no_proxy.clone()),
        ]);
        if let Some(certificate) = &self.ca_certificate {
            let certificate = certificate.to_string_lossy().into_owned();
            environment.extend([
                ("SSL_CERT_FILE".to_owned(), certificate.clone()),
                ("REQUESTS_CA_BUNDLE".to_owned(), certificate.clone()),
                ("CURL_CA_BUNDLE".to_owned(), certificate),
            ]);
        }
        environment
    }
}

impl fmt::Debug for ProxyProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProxyProfile")
            .field("name", &self.name)
            .field("proxy_url", &"<redacted>")
            .field("no_proxy", &self.no_proxy)
            .field("ca_certificate", &self.ca_certificate)
            .finish()
    }
}

#[derive(Clone, Debug)]
enum Route {
    Direct,
    Proxy(Arc<ProxyProfile>),
}

/// Static capability-to-network policy for direct or external-proxy egress.
#[derive(Clone, Debug, Default)]
pub struct CapabilityEgress {
    routes: BTreeMap<CapabilityName, Route>,
}

impl CapabilityEgress {
    /// Creates a fail-closed capability policy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allows one capability to use direct internet access.
    #[must_use]
    pub fn direct(mut self, capability: CapabilityName) -> Self {
        self.routes.insert(capability, Route::Direct);
        self
    }

    /// Allows one capability through a server-owned external proxy.
    #[must_use]
    pub fn proxy(mut self, capability: CapabilityName, profile: Arc<ProxyProfile>) -> Self {
        self.routes.insert(capability, Route::Proxy(profile));
        self
    }

    /// Returns every capability configured by this server.
    #[must_use]
    pub fn configured_capabilities(&self) -> BTreeSet<CapabilityName> {
        self.routes.keys().cloned().collect()
    }
}

#[async_trait]
impl EgressProvider for CapabilityEgress {
    async fn acquire(
        &self,
        _context: &EgressContext,
        requested: &BTreeSet<CapabilityName>,
    ) -> Result<EgressLease, EgressError> {
        if requested.is_empty() {
            return Ok(EgressLease::disabled());
        }

        let mut proxy: Option<&Arc<ProxyProfile>> = None;
        for capability in requested {
            match self
                .routes
                .get(capability)
                .ok_or_else(|| EgressError::CapabilityDenied(capability.clone()))?
            {
                Route::Direct => {}
                Route::Proxy(candidate) => {
                    if proxy.is_some_and(|selected| selected.name != candidate.name) {
                        return Err(EgressError::ConflictingProxyProfiles);
                    }
                    proxy = Some(candidate);
                }
            }
        }
        proxy.map_or_else(|| Ok(EgressLease::internet()), |profile| profile.lease())
    }
}

/// Managed composition of capability routing and live secret policy.
///
/// Secret references remain durable policy while values are resolved only in
/// the host proxy after the active principal grant is rechecked.
pub struct ManagedEgress {
    policy: Arc<PolicyStore>,
    secrets: Arc<dyn crate::SecretManager>,
    capabilities: CapabilityEgress,
}

impl ManagedEgress {
    /// Creates managed egress from durable policy, host secret providers, and
    /// static capability routes.
    #[must_use]
    pub fn new(
        policy: Arc<PolicyStore>,
        secrets: Arc<dyn crate::SecretManager>,
        capabilities: CapabilityEgress,
    ) -> Self {
        Self {
            policy,
            secrets,
            capabilities,
        }
    }
}

#[derive(Clone)]
struct LiveSecretResolver {
    policy: Arc<PolicyStore>,
    secrets: Arc<dyn crate::SecretManager>,
    context: EgressContext,
    routes: BTreeMap<String, crate::SecretView>,
}

#[async_trait]
impl SecretResolver for LiveSecretResolver {
    async fn resolve(
        &self,
        reference: &nanocodex_egress::SecretRef,
    ) -> Result<String, SecretResolverError> {
        if reference.provider() != MANAGED_SECRET_PROVIDER {
            return Err(SecretResolverError::Unavailable);
        }
        let configured = self
            .policy
            .agent_effective_secret(
                self.context.agent_id(),
                self.context.principal(),
                reference.key(),
            )
            .map_err(|_| SecretResolverError::Unavailable)?;
        if self.routes.get(reference.key()) != Some(&configured) {
            return Err(SecretResolverError::Unavailable);
        }
        self.secrets
            .resolve(&configured.source)
            .await
            .map_err(|_| SecretResolverError::Unavailable)
    }
}

const GUEST_CA_PATH: &str = "/tmp/nanocodex/egress/nanocentaur-ca.pem";

#[async_trait]
impl EgressProvider for ManagedEgress {
    async fn acquire(
        &self,
        context: &EgressContext,
        requested: &BTreeSet<CapabilityName>,
    ) -> Result<EgressLease, EgressError> {
        let mut lease = self.capabilities.acquire(context, requested).await?;
        let configured = self
            .policy
            .agent_effective_secrets(context.agent_id(), context.principal())
            .map_err(|error| EgressError::Provider(error.to_string()))?;
        if configured.is_empty() {
            return Ok(lease);
        }
        if lease
            .guest_environment()
            .keys()
            .any(|name| name.eq_ignore_ascii_case("http_proxy"))
        {
            return Err(EgressError::ConflictingProxyProfiles);
        }

        let mut rules = Vec::new();
        let mut allow_loopback = false;
        for secret in &configured {
            let spec = secret
                .to_spec()
                .map_err(|error| EgressError::Provider(error.to_string()))?;
            allow_loopback |= Url::parse(spec.upstream())
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .is_some_and(|host| {
                    host.eq_ignore_ascii_case("localhost")
                        || host
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|address| address.is_loopback())
                });
            rules.extend(
                spec.egress_rules()
                    .map_err(|error| EgressError::Provider(error.to_string()))?,
            );
        }

        let unmatched = if matches!(lease.network(), Network::Internet) {
            UnmatchedEgress::Allow
        } else {
            UnmatchedEgress::Deny
        };
        let resolver = LiveSecretResolver {
            policy: Arc::clone(&self.policy),
            secrets: Arc::clone(&self.secrets),
            context: context.clone(),
            routes: configured
                .iter()
                .cloned()
                .map(|secret| (secret.id.clone(), secret))
                .collect(),
        };
        let secrets = SecretEgress::builder(resolver)
            .rules(rules)
            .unmatched(unmatched)
            .build()
            .map_err(|error| EgressError::Provider(error.to_string()))?;
        let proxy = EgressProxy::builder()
            .allow_loopback_upstreams(allow_loopback)
            .layer(secrets)
            .spawn()
            .await
            .map_err(|error| EgressError::Provider(error.to_string()))?;
        let route = proxy.route();
        lease.insert_file(EgressFile::new(
            GUEST_CA_PATH,
            route.ca_certificate_pem(),
            0o644,
        ))?;
        for (name, value) in route.environment(GUEST_CA_PATH) {
            let name = name.into_string().map_err(|_| {
                EgressError::Provider("egress environment name is not UTF-8".into())
            })?;
            let value = value.into_string().map_err(|_| {
                EgressError::Provider("egress environment value is not UTF-8".into())
            })?;
            lease.insert_environment(name, value)?;
        }
        lease.retain(Arc::new(proxy));
        Ok(lease)
    }
}

/// Failure to authorize or provision managed VM egress.
#[derive(Debug, Error)]
pub enum EgressError {
    /// A requested capability is not configured by this server.
    #[error("egress capability `{0}` is not granted by this server")]
    CapabilityDenied(CapabilityName),
    /// Requested capabilities or secret injection require incompatible proxies.
    #[error("requested capabilities require conflicting proxy profiles")]
    ConflictingProxyProfiles,
    /// An external proxy was not an absolute HTTP(S) URL.
    #[error("proxy profile must use an absolute HTTP(S) URL")]
    InvalidProxyUrl,
    /// Reusable VM lease construction failed.
    #[error(transparent)]
    Lease(#[from] LeaseError),
    /// A host policy, payment, secret, or proxy component failed.
    #[error("egress provider failed: {0}")]
    Provider(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ConstantSecretManager;

    #[async_trait]
    impl crate::SecretManager for ConstantSecretManager {
        async fn resolve(
            &self,
            _reference: &crate::SecretRef,
        ) -> Result<String, crate::SecretError> {
            Ok("host-secret".to_owned())
        }
    }

    fn capability(name: &str) -> CapabilityName {
        CapabilityName::new(name).unwrap()
    }

    #[tokio::test]
    async fn no_capabilities_fail_closed_without_a_network_device() {
        let lease = CapabilityEgress::new()
            .acquire(&EgressContext::new("agent", "principal"), &BTreeSet::new())
            .await
            .unwrap();
        assert_eq!(lease.network(), &Network::Disabled);
        assert!(lease.guest_environment().is_empty());
    }

    #[tokio::test]
    async fn proxy_capability_injects_only_server_owned_configuration() {
        let profile = Arc::new(ProxyProfile::new("iron", "http://proxy.internal:8080").unwrap());
        let provider =
            CapabilityEgress::new().proxy(capability("github.read"), Arc::clone(&profile));
        let lease = provider
            .acquire(
                &EgressContext::new("agent", "principal"),
                &BTreeSet::from([capability("github.read")]),
            )
            .await
            .unwrap();
        assert_eq!(lease.network(), &Network::Internet);
        assert_eq!(
            lease.guest_environment().get("HTTPS_PROXY"),
            Some(&"http://proxy.internal:8080".to_owned())
        );
        assert!(!format!("{lease:?}").contains("proxy.internal"));
    }

    #[tokio::test]
    async fn unknown_capabilities_are_denied() {
        let error = CapabilityEgress::new()
            .acquire(
                &EgressContext::new("agent", "principal"),
                &BTreeSet::from([capability("github.read")]),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, EgressError::CapabilityDenied(_)));
    }

    #[tokio::test]
    async fn live_revocation_is_scoped_by_route_even_when_sources_match() {
        let policy = Arc::new(PolicyStore::in_memory().unwrap());
        policy
            .bootstrap("client", "Client", "api-key", "principal", [])
            .unwrap();
        for id in ["first", "second"] {
            policy
                .create_secret(crate::CreateSecret {
                    id: Some(id.to_owned()),
                    name: id.to_owned(),
                    source: crate::SecretRef::new("environment", "SHARED_KEY"),
                    upstream: "https://example.com".to_owned(),
                    rules: Vec::new(),
                    delivery: crate::SecretDelivery::inject_header("authorization", "Bearer "),
                    guest: crate::SecretGuestConfig::new(format!(
                        "{}_BASE_URL",
                        id.to_ascii_uppercase()
                    )),
                })
                .unwrap();
            policy.set_principal_secret("principal", id, true).unwrap();
        }
        let headers = axum::http::HeaderMap::from_iter([(
            axum::http::HeaderName::from_static("x-api-key"),
            axum::http::HeaderValue::from_static("api-key"),
        )]);
        let client = policy.authenticate(&headers).unwrap();
        let (identity, _) = policy
            .create_or_resolve_agent(&client, Some("route-revocation"))
            .unwrap();
        let routes = policy
            .agent_effective_secrets(&identity.id, "principal")
            .unwrap()
            .into_iter()
            .map(|secret| (secret.id.clone(), secret))
            .collect();
        let resolver = LiveSecretResolver {
            policy: Arc::clone(&policy),
            secrets: Arc::new(ConstantSecretManager),
            context: EgressContext::new(identity.id, "principal"),
            routes,
        };

        policy
            .set_principal_secret("principal", "first", false)
            .unwrap();
        let first = nanocodex_egress::SecretResolver::resolve(
            &resolver,
            &nanocodex_egress::SecretRef::new(MANAGED_SECRET_PROVIDER, "first"),
        )
        .await;
        assert_eq!(first.unwrap_err(), SecretResolverError::Unavailable);
        let second = nanocodex_egress::SecretResolver::resolve(
            &resolver,
            &nanocodex_egress::SecretRef::new(MANAGED_SECRET_PROVIDER, "second"),
        )
        .await
        .unwrap();
        assert_eq!(second, "host-secret");

        policy
            .patch_secret(
                "second",
                crate::PatchSecret {
                    source: Some(crate::SecretRef::new("environment", "ROTATED_KEY")),
                    ..crate::PatchSecret::default()
                },
            )
            .unwrap();
        let stale_route = nanocodex_egress::SecretResolver::resolve(
            &resolver,
            &nanocodex_egress::SecretRef::new(MANAGED_SECRET_PROVIDER, "second"),
        )
        .await;
        assert_eq!(stale_route.unwrap_err(), SecretResolverError::Unavailable);
    }
}
