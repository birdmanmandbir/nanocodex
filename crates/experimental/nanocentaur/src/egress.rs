use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
    sync::Arc,
};

use async_trait::async_trait;
use mpp::client::MultiProvider;
use nanocodex_vm_egress::{
    EgressContext as VmEgressContext, SecretGateway, SecretManager, SecretPolicy,
    SecretPolicyError, UnmatchedEgress, VmEgress,
};
pub use nanovm::EgressLease;
use nanovm::{EgressError as LeaseError, Network};
use thiserror::Error;
use url::Url;

use crate::{CapabilityName, PolicyStore};

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

    fn vm_context(&self) -> VmEgressContext {
        VmEgressContext::new(self.agent_id.clone(), self.principal.clone())
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
/// Secret resolution and MPP remain in `nanocodex-vm-egress`; this type only
/// projects Nanocentaur's authenticated `SQLite` identity into that reusable
/// boundary.
pub struct ManagedEgress {
    policy: Arc<PolicyStore>,
    secrets: Arc<dyn SecretManager>,
    capabilities: CapabilityEgress,
    payments: MultiProvider,
    secret_gateway: Option<Arc<SecretGateway>>,
}

impl ManagedEgress {
    /// Creates managed egress with no configured outbound payment provider.
    #[must_use]
    pub fn new(
        policy: Arc<PolicyStore>,
        secrets: Arc<dyn SecretManager>,
        capabilities: CapabilityEgress,
    ) -> Self {
        Self {
            policy,
            secrets,
            capabilities,
            payments: MultiProvider::new(),
            secret_gateway: None,
        }
    }

    /// Replaces the MPP provider set used for unmatched paid destinations.
    #[must_use]
    pub fn payments(mut self, payments: MultiProvider) -> Self {
        self.payments = payments;
        self
    }

    /// Adds scoped base-URL routes for guest SDKs that ignore proxy variables.
    #[must_use]
    pub fn secret_gateway(mut self, gateway: Arc<SecretGateway>) -> Self {
        self.secret_gateway = Some(gateway);
        self
    }
}

#[async_trait]
impl EgressProvider for ManagedEgress {
    async fn acquire(
        &self,
        context: &EgressContext,
        requested: &BTreeSet<CapabilityName>,
    ) -> Result<EgressLease, EgressError> {
        let base = self.capabilities.acquire(context, requested).await?;
        let policy: Arc<dyn SecretPolicy> = Arc::new(ManagedSecretPolicy {
            store: Arc::clone(&self.policy),
        });
        let vm_context = context.vm_context();
        let configured = policy
            .secrets(&vm_context)
            .await
            .map_err(|error| EgressError::Provider(error.to_string()))?;
        if configured.is_empty() {
            return Ok(base);
        }
        if base
            .guest_environment()
            .keys()
            .any(|name| name.eq_ignore_ascii_case("http_proxy"))
        {
            return Err(EgressError::ConflictingProxyProfiles);
        }

        let unmatched = if matches!(base.network(), Network::Internet) {
            UnmatchedEgress::Allow
        } else {
            UnmatchedEgress::Deny
        };
        let mut builder = VmEgress::builder(self.payments.clone())
            .secrets(vm_context, policy, Arc::clone(&self.secrets))
            .unmatched_egress(unmatched);
        if let Some(gateway) = &self.secret_gateway {
            builder = builder.secret_gateway(Arc::clone(gateway));
        }
        let egress = builder
            .spawn()
            .await
            .map_err(|error| EgressError::Provider(error.to_string()))?;
        Ok(egress.lease())
    }
}

struct ManagedSecretPolicy {
    store: Arc<PolicyStore>,
}

#[async_trait]
impl SecretPolicy for ManagedSecretPolicy {
    async fn secrets(
        &self,
        context: &VmEgressContext,
    ) -> Result<Vec<nanocodex_vm_egress::SecretSpec>, SecretPolicyError> {
        let secrets = self
            .store
            .agent_effective_secrets(context.agent_id(), context.principal())
            .map_err(|error| SecretPolicyError::Unavailable(error.to_string()))?;
        secrets
            .iter()
            .map(|secret| {
                secret
                    .to_spec()
                    .map_err(|error| SecretPolicyError::Unavailable(error.to_string()))
            })
            .collect()
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
}
