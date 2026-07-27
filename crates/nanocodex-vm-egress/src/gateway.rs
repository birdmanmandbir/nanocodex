use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Uri, uri::Authority};
use mpp::client::PaymentProvider;
use mpp_egress::{EgressPolicy, EgressRequest, MppEgress, RequestPolicy, RequestPolicyError};
use nanovm::{EgressError, EgressLease};
use thiserror::Error;
use url::Url;

use crate::{
    MppVmEgressError, SecretDelivery, SecretManager, SecretSpec, mpp_egress_layer,
    secret::safe_request_path,
};

/// Stable server-derived identity supplied to secret policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressContext {
    agent_id: String,
    principal: String,
}

impl EgressContext {
    /// Creates policy context from an agent ID and authenticated principal.
    #[must_use]
    pub fn new(agent_id: impl Into<String>, principal: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            principal: principal.into(),
        }
    }

    /// Returns the managed agent identity.
    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Returns the authenticated principal identity.
    #[must_use]
    pub fn principal(&self) -> &str {
        &self.principal
    }
}

/// Dynamic authorization source queried for every outbound request.
///
/// Implementations can read a managed policy store, so disabling a principal,
/// revoking a route, or rotating its source takes effect without restarting
/// the guest. Returned specs contain configuration only, never resolved values.
#[async_trait]
pub trait SecretPolicy: Send + Sync {
    /// Returns the complete currently authorized route set for one identity.
    async fn secrets(&self, context: &EgressContext) -> Result<Vec<SecretSpec>, SecretPolicyError>;
}

/// Immutable policy useful for standalone applications and tests.
#[derive(Clone, Debug, Default)]
pub struct StaticSecretPolicy {
    secrets: Vec<SecretSpec>,
}

impl StaticSecretPolicy {
    /// Creates an immutable policy from already validated specs.
    #[must_use]
    pub fn new(secrets: impl IntoIterator<Item = SecretSpec>) -> Self {
        Self {
            secrets: secrets.into_iter().collect(),
        }
    }
}

#[async_trait]
impl SecretPolicy for StaticSecretPolicy {
    async fn secrets(
        &self,
        _context: &EgressContext,
    ) -> Result<Vec<SecretSpec>, SecretPolicyError> {
        Ok(self.secrets.clone())
    }
}

/// Failure reading current secret authorization.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SecretPolicyError {
    /// The identity is disabled or not authorized for secret egress.
    #[error("secret policy denied this identity")]
    Denied,
    /// The backing policy store could not be queried.
    #[error("secret policy is unavailable: {0}")]
    Unavailable(String),
}

struct SecretRequestPolicy {
    context: EgressContext,
    policy: Arc<dyn SecretPolicy>,
    manager: Arc<dyn SecretManager>,
    unmatched: UnmatchedEgress,
}

#[async_trait]
impl RequestPolicy for SecretRequestPolicy {
    async fn authorize(
        &self,
        mut request: EgressRequest,
    ) -> Result<EgressRequest, RequestPolicyError> {
        let secrets = self
            .policy
            .secrets(&self.context)
            .await
            .map_err(|error| match error {
                SecretPolicyError::Denied => RequestPolicyError::Denied,
                SecretPolicyError::Unavailable(_) => RequestPolicyError::Unavailable,
            })?;
        if request.method() == Method::CONNECT {
            return authorize_connect(request, &secrets, self.unmatched);
        }
        let secret = match select_secret(&secrets, &request)? {
            SecretSelection::Inject(secret) => secret,
            SecretSelection::Unmatched if self.unmatched == UnmatchedEgress::Allow => {
                return Ok(request);
            }
            SecretSelection::Unmatched => return Err(RequestPolicyError::Denied),
        };
        let value = self
            .manager
            .resolve(secret.source())
            .await
            .map_err(|_| RequestPolicyError::Unavailable)?;
        apply_delivery(request.headers_mut(), secret.delivery(), &value)?;
        tracing::info!(
            target: "nanocodex_vm_egress",
            secret_route_id = %secret.id(),
            egress.agent.id = self.context.agent_id(),
            egress.principal.id = self.context.principal(),
            http.request.method = %request.method(),
            "authorized host-side secret injection"
        );
        Ok(request)
    }
}

fn authorize_connect(
    request: EgressRequest,
    secrets: &[SecretSpec],
    unmatched: UnmatchedEgress,
) -> Result<EgressRequest, RequestPolicyError> {
    if unmatched == UnmatchedEgress::Allow {
        Ok(request)
    } else {
        let authority = request
            .uri()
            .authority()
            .cloned()
            .or_else(|| request.uri().to_string().parse().ok())
            .ok_or(RequestPolicyError::InvalidRequest)?;
        secrets
            .iter()
            .any(|secret| matching_upstream(secret, "https", &authority).is_some())
            .then_some(request)
            .ok_or(RequestPolicyError::Denied)
    }
}

enum SecretSelection<'a> {
    Inject(&'a SecretSpec),
    Unmatched,
}

fn select_secret<'a>(
    secrets: &'a [SecretSpec],
    request: &EgressRequest,
) -> Result<SecretSelection<'a>, RequestPolicyError> {
    let (scheme, authority) = request_origin(request.uri())?;
    let path = safe_request_path(request.uri().path()).ok_or(RequestPolicyError::InvalidRequest)?;
    let mut origin_matched = false;
    let mut explicit = None;
    let mut implicit = None;
    let mut implicit_ambiguous = false;
    for secret in secrets {
        let Some(upstream) = matching_upstream(secret, scheme, authority) else {
            continue;
        };
        origin_matched = true;
        if !allows_request(secret, &upstream, request.method(), &path) {
            continue;
        }
        match secret.delivery() {
            SecretDelivery::ReplaceHeader {
                header,
                placeholder,
            } if header_contains(request.headers(), header, placeholder) => {
                if explicit.replace(secret).is_some() {
                    return Err(RequestPolicyError::Denied);
                }
            }
            SecretDelivery::InjectHeader { .. } => {
                if implicit.is_some() {
                    implicit_ambiguous = true;
                } else {
                    implicit = Some(secret);
                }
            }
            SecretDelivery::ReplaceHeader { .. } => {}
        }
    }
    if !origin_matched {
        return Ok(SecretSelection::Unmatched);
    }
    explicit
        .or_else(|| (!implicit_ambiguous).then_some(implicit).flatten())
        .map(SecretSelection::Inject)
        .ok_or(RequestPolicyError::Denied)
}

fn request_origin(uri: &Uri) -> Result<(&str, &Authority), RequestPolicyError> {
    let scheme = uri.scheme_str().unwrap_or("http");
    let authority = uri.authority().ok_or(RequestPolicyError::InvalidRequest)?;
    Ok((scheme, authority))
}

fn matching_upstream(secret: &SecretSpec, scheme: &str, authority: &Authority) -> Option<Url> {
    let Ok(upstream) = Url::parse(secret.upstream()) else {
        return None;
    };
    (upstream.scheme() == scheme
        && upstream
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case(authority.host()))
        && upstream.port_or_known_default()
            == authority
                .port_u16()
                .or_else(|| (scheme == "https").then_some(443))
                .or_else(|| (scheme == "http").then_some(80)))
    .then_some(upstream)
}

fn allows_request(secret: &SecretSpec, upstream: &Url, method: &Method, path: &str) -> bool {
    let supported_method = [
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::PATCH,
        Method::DELETE,
        Method::HEAD,
        Method::OPTIONS,
    ]
    .iter()
    .any(|supported| supported == method);
    let within_upstream =
        safe_request_path(upstream.path()).is_some_and(|prefix| path_prefix_matches(&prefix, path));
    supported_method
        && within_upstream
        && (secret.rules().is_empty()
            || secret.rules().iter().any(|rule| {
                (rule.methods().is_empty()
                    || rule.methods().iter().any(|allowed| allowed.matches(method)))
                    && (rule.path_prefixes().is_empty()
                        || rule
                            .path_prefixes()
                            .iter()
                            .any(|prefix| path_prefix_matches(prefix, path)))
            }))
}

fn path_prefix_matches(prefix: &str, path: &str) -> bool {
    prefix == "/"
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| prefix.ends_with('/') || suffix.starts_with('/'))
}

fn header_contains(headers: &HeaderMap, name: &str, needle: &str) -> bool {
    HeaderName::from_bytes(name.as_bytes())
        .ok()
        .is_some_and(|name| {
            headers
                .get_all(name)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .any(|value| value.contains(needle))
        })
}

fn apply_delivery(
    headers: &mut HeaderMap,
    delivery: &SecretDelivery,
    secret: &str,
) -> Result<(), RequestPolicyError> {
    let name = HeaderName::from_bytes(delivery.header().as_bytes())
        .map_err(|_| RequestPolicyError::Unavailable)?;
    match delivery {
        SecretDelivery::InjectHeader { prefix, .. } => {
            let value = HeaderValue::from_str(&format!("{prefix}{secret}"))
                .map_err(|_| RequestPolicyError::Unavailable)?;
            headers.insert(name, value);
        }
        SecretDelivery::ReplaceHeader { placeholder, .. } => {
            let value = headers
                .get(&name)
                .and_then(|value| value.to_str().ok())
                .filter(|value| value.contains(placeholder))
                .ok_or(RequestPolicyError::Denied)?;
            let value = HeaderValue::from_str(&value.replace(placeholder, secret))
                .map_err(|_| RequestPolicyError::Unavailable)?;
            headers.insert(name, value);
        }
    }
    Ok(())
}

/// One running unified host proxy and its reusable VM lease.
pub struct VmEgress {
    proxy: Arc<MppEgress>,
    lease: EgressLease,
}

impl VmEgress {
    /// Starts a builder with an application-owned MPP payment provider.
    #[must_use]
    pub fn builder<P>(provider: P) -> VmEgressBuilder<P>
    where
        P: PaymentProvider + 'static,
    {
        VmEgressBuilder::new(provider)
    }

    /// Returns a cloneable lease for one VM lifetime.
    ///
    /// Lease clones keep the proxy, wallet, policy, and secret managers alive.
    #[must_use]
    pub fn lease(&self) -> EgressLease {
        self.lease.clone()
    }

    /// Returns the authenticated host proxy URL.
    ///
    /// Prefer [`Self::lease`] for guest configuration. The URL is exposed for
    /// direct child-process consumers and integration tests.
    #[must_use]
    pub fn proxy_url(&self) -> String {
        self.proxy.proxy_url()
    }

    /// Gracefully drains the proxy after every issued lease has been dropped.
    ///
    /// # Errors
    ///
    /// Returns [`VmEgressError::LeaseInUse`] while another lease still retains
    /// the proxy, or a typed proxy shutdown error.
    pub async fn shutdown(self) -> Result<(), VmEgressError> {
        let Self { proxy, lease } = self;
        drop(lease);
        let proxy = Arc::try_unwrap(proxy).map_err(|_| VmEgressError::LeaseInUse)?;
        proxy.shutdown().await.map_err(VmEgressError::Proxy)
    }
}

/// Builder for one unified payment and secret egress proxy.
pub struct VmEgressBuilder<P> {
    provider: P,
    mpp_policy: EgressPolicy,
    secrets: Option<SecretConfiguration>,
    unmatched: UnmatchedEgress,
}

struct SecretConfiguration {
    context: EgressContext,
    policy: Arc<dyn SecretPolicy>,
    manager: Arc<dyn SecretManager>,
}

impl<P> VmEgressBuilder<P>
where
    P: PaymentProvider + 'static,
{
    /// Creates a payment-capable builder with bounded default MPP policy.
    #[must_use]
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            mpp_policy: EgressPolicy::default(),
            secrets: None,
            unmatched: UnmatchedEgress::Deny,
        }
    }

    /// Replaces request, connection, and payment replay limits.
    #[must_use]
    pub fn mpp_policy(mut self, policy: EgressPolicy) -> Self {
        self.mpp_policy = policy;
        self
    }

    /// Adds dynamically authorized secret injection to the same proxy.
    #[must_use]
    pub fn secrets(
        mut self,
        context: EgressContext,
        policy: Arc<dyn SecretPolicy>,
        manager: Arc<dyn SecretManager>,
    ) -> Self {
        self.secrets = Some(SecretConfiguration {
            context,
            policy,
            manager,
        });
        self
    }

    /// Selects whether destinations with no configured secret origin pass
    /// through to ordinary MPP handling.
    ///
    /// The default is [`UnmatchedEgress::Deny`]. Allowing unmatched traffic
    /// never bypasses method or path restrictions for a configured secret
    /// origin; those requests still fail closed.
    #[must_use]
    pub const fn unmatched_egress(mut self, policy: UnmatchedEgress) -> Self {
        self.unmatched = policy;
        self
    }

    /// Starts the host proxy and creates its VM-facing lease.
    ///
    /// Without [`Self::secrets`], all authenticated destinations are forwarded
    /// with ordinary MPP handling. With secret policy installed, unmatched
    /// origins, methods, and paths fail closed.
    ///
    /// # Errors
    ///
    /// Returns a typed payment-proxy, secret-policy, guest-configuration, or
    /// lease-projection error. A failed start leaves no background proxy.
    pub async fn spawn(self) -> Result<VmEgress, VmEgressError> {
        let (proxy, initial_secrets) = if let Some(secrets) = self.secrets {
            let initial = secrets
                .policy
                .secrets(&secrets.context)
                .await
                .map_err(VmEgressError::Policy)?;
            let request_policy = SecretRequestPolicy {
                context: secrets.context,
                policy: secrets.policy,
                manager: secrets.manager,
                unmatched: self.unmatched,
            };
            (
                MppEgress::start_with_request_policy(
                    self.provider,
                    self.mpp_policy,
                    request_policy,
                )
                .await
                .map_err(VmEgressError::Proxy)?,
                initial,
            )
        } else {
            (
                MppEgress::start(self.provider, self.mpp_policy)
                    .await
                    .map_err(VmEgressError::Proxy)?,
                Vec::new(),
            )
        };
        let proxy = Arc::new(proxy);
        let mut lease = mpp_egress_layer(Arc::clone(&proxy))?;
        add_secret_environment(&mut lease, &initial_secrets)?;
        Ok(VmEgress { proxy, lease })
    }
}

/// Policy for destinations that do not match any configured secret origin.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UnmatchedEgress {
    /// Reject unmatched destinations before contacting them.
    #[default]
    Deny,
    /// Forward unmatched destinations through ordinary MPP handling.
    Allow,
}

fn add_secret_environment(
    lease: &mut EgressLease,
    secrets: &[SecretSpec],
) -> Result<(), SecretEgressError> {
    let mut environment = BTreeMap::new();
    for secret in secrets {
        insert_environment(
            &mut environment,
            secret.guest().base_url_environment(),
            secret.upstream().to_owned(),
        )?;
        if let Some(name) = secret.guest().placeholder_environment_name() {
            let placeholder = secret
                .delivery()
                .placeholder()
                .ok_or(SecretEgressError::Placeholder)?;
            insert_environment(&mut environment, name, placeholder.to_owned())?;
        }
    }
    for (name, value) in environment {
        lease.insert_environment(name, value)?;
    }
    Ok(())
}

fn insert_environment(
    environment: &mut BTreeMap<String, String>,
    name: &str,
    value: String,
) -> Result<(), SecretEgressError> {
    if environment.insert(name.to_owned(), value).is_some() {
        return Err(SecretEgressError::DuplicateEnvironment(name.to_owned()));
    }
    Ok(())
}

/// Failure projecting public secret route configuration into a guest lease.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SecretEgressError {
    /// Two authorized routes claim the same guest environment name.
    #[error("duplicate secret guest environment `{0}`")]
    DuplicateEnvironment(String),
    /// Placeholder guest configuration did not use placeholder delivery.
    #[error("secret placeholder environment requires replace-header delivery")]
    Placeholder,
    /// Public environment conflicted with another egress layer.
    #[error(transparent)]
    Egress(#[from] EgressError),
}

/// Failure to start or stop unified VM egress.
#[derive(Debug, Error)]
pub enum VmEgressError {
    /// The MPP-aware host proxy failed.
    #[error(transparent)]
    Proxy(#[from] mpp_egress::EgressError),
    /// Initial dynamic authorization failed.
    #[error(transparent)]
    Policy(SecretPolicyError),
    /// Public secret route environment was invalid or conflicting.
    #[error(transparent)]
    Secret(#[from] SecretEgressError),
    /// Host proxy projection into a neutral VM lease failed.
    #[error(transparent)]
    Lease(#[from] MppVmEgressError),
    /// Another issued VM lease still retains the proxy.
    #[error("cannot shut down VM egress while a lease is still active")]
    LeaseInUse,
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        sync::{Mutex, RwLock},
    };

    use axum::{
        Router,
        extract::Request,
        http::{StatusCode, header::WWW_AUTHENTICATE},
        response::IntoResponse,
        routing::{any, get},
    };
    use mpp::{
        Base64UrlJson, MppError, PaymentChallenge, PaymentCredential, PaymentPayload,
        client::PaymentProvider, format_www_authenticate,
    };

    use super::*;
    use crate::{
        SecretConfigError, SecretGuestConfig, SecretHttpMethod, SecretRef, SecretRequestRule,
    };

    #[derive(Clone, Default)]
    struct NoPayments;

    impl PaymentProvider for NoPayments {
        fn supports(&self, _method: &str, _intent: &str) -> bool {
            false
        }

        async fn pay(&self, _challenge: &PaymentChallenge) -> Result<PaymentCredential, MppError> {
            Err(MppError::UnsupportedPaymentMethod("test".to_owned()))
        }
    }

    #[derive(Clone, Default)]
    struct MockPayments {
        payments: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl PaymentProvider for MockPayments {
        fn supports(&self, method: &str, intent: &str) -> bool {
            method == "test" && intent == "charge"
        }

        async fn pay(&self, challenge: &PaymentChallenge) -> Result<PaymentCredential, MppError> {
            self.payments
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(PaymentCredential::new(
                challenge.to_echo(),
                PaymentPayload::hash("unified-egress-proof"),
            ))
        }
    }

    struct RotatingSecret {
        value: RwLock<String>,
    }

    #[async_trait]
    impl SecretManager for RotatingSecret {
        async fn resolve(
            &self,
            _reference: &crate::SecretRef,
        ) -> Result<String, crate::SecretError> {
            Ok(self.value.read().unwrap().clone())
        }
    }

    struct RevocablePolicy {
        secrets: RwLock<Option<Vec<SecretSpec>>>,
    }

    #[async_trait]
    impl SecretPolicy for RevocablePolicy {
        async fn secrets(
            &self,
            _context: &EgressContext,
        ) -> Result<Vec<SecretSpec>, SecretPolicyError> {
            self.secrets
                .read()
                .unwrap()
                .clone()
                .ok_or(SecretPolicyError::Denied)
        }
    }

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
        type Writer = LogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            LogWriter(Arc::clone(&self.0))
        }
    }

    impl Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn scoped_secret(upstream: String) -> SecretSpec {
        SecretSpec::builder(
            "test",
            SecretRef::new("test", "token"),
            upstream,
            SecretDelivery::inject_header("authorization", "Bearer "),
            SecretGuestConfig::new("TEST_BASE_URL"),
        )
        .rule(
            SecretRequestRule::new()
                .method(SecretHttpMethod::Get)
                .path_prefix("/allowed/"),
        )
        .build()
        .unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unified_proxy_injects_rotated_secrets_and_denies_other_paths() {
        let logs = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::INFO)
            .with_writer(logs.clone())
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        tracing::callsite::rebuild_interest_cache();
        let _subscriber = tracing::dispatcher::set_default(&dispatch);
        let observed = Arc::new(Mutex::new(Vec::<String>::new()));
        let upstream_observed = Arc::clone(&observed);
        let app = Router::new().fallback(any(move |request: Request| {
            let observed = Arc::clone(&upstream_observed);
            async move {
                observed.lock().unwrap().push(
                    request
                        .headers()
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned(),
                );
                "ok"
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let manager = Arc::new(RotatingSecret {
            value: RwLock::new("alpha-secret".to_owned()),
        });
        let policy = Arc::new(RevocablePolicy {
            secrets: RwLock::new(Some(vec![scoped_secret(format!("http://{address}"))])),
        });
        let egress = VmEgress::builder(NoPayments)
            .secrets(
                EgressContext::new("agent-1", "principal-1"),
                policy.clone(),
                manager.clone(),
            )
            .spawn()
            .await
            .unwrap();
        let lease = egress.lease();
        assert!(
            lease
                .guest_environment()
                .values()
                .all(|value| { !value.contains("alpha-secret") && !value.contains("beta-secret") })
        );
        assert!(!format!("{lease:?}").contains("alpha-secret"));
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(egress.proxy_url()).unwrap())
            .build()
            .unwrap();

        let allowed = client
            .get(format!("http://{address}/allowed/one"))
            .send()
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
        *manager.value.write().unwrap() = "beta-secret".to_owned();
        let allowed = client
            .get(format!("http://{address}/allowed/two"))
            .send()
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
        let denied = client
            .get(format!("http://{address}/admin"))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        *policy.secrets.write().unwrap() = None;
        let revoked = client
            .get(format!("http://{address}/allowed/three"))
            .send()
            .await
            .unwrap();
        assert_eq!(revoked.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            observed.lock().unwrap().as_slice(),
            ["Bearer alpha-secret", "Bearer beta-secret"]
        );
        let trace = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(!trace.contains("alpha-secret"));
        assert!(!trace.contains("beta-secret"));

        drop(client);
        drop(lease);
        egress.shutdown().await.unwrap();
        server.abort();
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "the test keeps one complete secret-plus-payment proxy proof linear"
    )]
    async fn one_proxy_composes_secret_injection_and_mpp_replay() {
        let secret_origin = Router::new().route(
            "/allowed",
            get(|request: Request| async move {
                request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned()
            }),
        );
        let secret_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let secret_address = secret_listener.local_addr().unwrap();
        let secret_server =
            tokio::spawn(async move { axum::serve(secret_listener, secret_origin).await.unwrap() });

        let challenge = format_www_authenticate(&PaymentChallenge::new(
            "unified-proof",
            "test.local",
            "test",
            "charge",
            Base64UrlJson::from_value(&serde_json::json!({
                "amount": "1",
                "currency": "test"
            }))
            .unwrap(),
        ))
        .unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let payment_origin = Router::new().route(
            "/paid",
            get({
                let calls = Arc::clone(&calls);
                move |request: Request| {
                    let challenge = challenge.clone();
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if request.headers().contains_key("authorization") {
                            (StatusCode::OK, "paid").into_response()
                        } else {
                            (
                                StatusCode::PAYMENT_REQUIRED,
                                [(WWW_AUTHENTICATE, challenge)],
                                "payment required",
                            )
                                .into_response()
                        }
                    }
                }
            }),
        );
        let payment_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let payment_address = payment_listener.local_addr().unwrap();
        let payment_server = tokio::spawn(async move {
            axum::serve(payment_listener, payment_origin).await.unwrap();
        });

        let manager = Arc::new(RotatingSecret {
            value: RwLock::new("host-only".to_owned()),
        });
        let policy = Arc::new(StaticSecretPolicy::new([SecretSpec::builder(
            "secret-origin",
            SecretRef::new("test", "token"),
            format!("http://{secret_address}"),
            SecretDelivery::inject_header("authorization", "Bearer "),
            SecretGuestConfig::new("SECRET_BASE_URL"),
        )
        .rule(
            SecretRequestRule::new()
                .method(SecretHttpMethod::Get)
                .path_prefix("/allowed"),
        )
        .build()
        .unwrap()]));
        let payments = MockPayments::default();
        let payment_count = Arc::clone(&payments.payments);
        let egress = VmEgress::builder(payments)
            .secrets(
                EgressContext::new("agent-1", "principal-1"),
                policy,
                manager,
            )
            .unmatched_egress(UnmatchedEgress::Allow)
            .spawn()
            .await
            .unwrap();
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(egress.proxy_url()).unwrap())
            .build()
            .unwrap();

        let secret = client
            .get(format!("http://{secret_address}/allowed"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(secret, "Bearer host-only");
        let paid = client
            .get(format!("http://{payment_address}/paid"))
            .send()
            .await
            .unwrap();
        assert_eq!(paid.status(), StatusCode::OK);
        assert_eq!(payment_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);

        drop(client);
        egress.shutdown().await.unwrap();
        secret_server.abort();
        payment_server.abort();
    }

    #[tokio::test]
    #[ignore = "manual public-network HTTPS secret-injection smoke"]
    async fn live_https_secret_injection_smoke() {
        let manager = Arc::new(RotatingSecret {
            value: RwLock::new("host-only-https-proof".to_owned()),
        });
        let policy = Arc::new(StaticSecretPolicy::new([SecretSpec::builder(
            "https-proof",
            SecretRef::new("test", "token"),
            "https://postman-echo.com",
            SecretDelivery::inject_header("x-nanocodex-proof", ""),
            SecretGuestConfig::new("HTTPS_PROOF_BASE_URL"),
        )
        .rule(
            SecretRequestRule::new()
                .method(SecretHttpMethod::Get)
                .path_prefix("/headers"),
        )
        .build()
        .unwrap()]));
        let egress = VmEgress::builder(NoPayments)
            .secrets(
                EgressContext::new("https-proof", "local-test"),
                policy,
                manager,
            )
            .spawn()
            .await
            .unwrap();
        let lease = egress.lease();
        let ca = lease.guest_files().next().unwrap().contents();
        let client = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(ca).unwrap())
            .proxy(reqwest::Proxy::all(egress.proxy_url()).unwrap())
            .build()
            .unwrap();

        let response = client
            .get("https://postman-echo.com/headers")
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status, StatusCode::OK, "response body: {body}");
        let response: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            response
                .get("headers")
                .and_then(|headers| headers.get("x-nanocodex-proof"))
                .and_then(serde_json::Value::as_str),
            Some("host-only-https-proof")
        );

        drop(client);
        drop(lease);
        egress.shutdown().await.unwrap();
    }

    #[test]
    fn ambiguous_routes_fail_closed() {
        let first = scoped_secret("https://example.com".to_owned());
        let second = SecretSpec::builder(
            "second",
            SecretRef::new("test", "token"),
            "https://example.com",
            SecretDelivery::inject_header("authorization", "Bearer "),
            SecretGuestConfig::new("SECOND_BASE_URL"),
        )
        .rule(
            SecretRequestRule::new()
                .method(SecretHttpMethod::Get)
                .path_prefix("/allowed/"),
        )
        .build()
        .unwrap();
        let request = EgressRequest::new(
            Method::GET,
            "https://example.com/allowed/value".parse().unwrap(),
            HeaderMap::new(),
        );
        assert!(matches!(
            select_secret(&[first, second], &request),
            Err(RequestPolicyError::Denied)
        ));
    }

    #[test]
    fn path_scopes_include_the_upstream_base_and_stop_at_segment_boundaries() {
        let secret = SecretSpec::builder(
            "scoped",
            SecretRef::new("test", "token"),
            "https://example.com/api",
            SecretDelivery::inject_header("authorization", "Bearer "),
            SecretGuestConfig::new("SCOPED_BASE_URL"),
        )
        .rule(SecretRequestRule::new().path_prefix("/api/allowed"))
        .build()
        .unwrap();

        for path in ["/api/allowed", "/api/allowed/value"] {
            let request = EgressRequest::new(
                Method::GET,
                format!("https://example.com{path}").parse().unwrap(),
                HeaderMap::new(),
            );
            assert!(matches!(
                select_secret(std::slice::from_ref(&secret), &request),
                Ok(SecretSelection::Inject(_))
            ));
        }
        for path in ["/api/allowed-suffix", "/outside"] {
            let request = EgressRequest::new(
                Method::GET,
                format!("https://example.com{path}").parse().unwrap(),
                HeaderMap::new(),
            );
            assert!(matches!(
                select_secret(std::slice::from_ref(&secret), &request),
                Err(RequestPolicyError::Denied)
            ));
        }
    }

    #[test]
    fn removes_transport_owned_headers_from_secret_configuration() {
        use http::header::{CONTENT_LENGTH, HOST, PROXY_AUTHORIZATION};

        let spec = SecretSpec::builder(
            "bad",
            SecretRef::new("test", "token"),
            "https://example.com",
            SecretDelivery::inject_header(HOST.as_str(), ""),
            SecretGuestConfig::new("TEST_BASE_URL"),
        )
        .build();
        assert_eq!(spec.unwrap_err(), SecretConfigError::InvalidHeader);
        assert_eq!(CONTENT_LENGTH.as_str(), "content-length");
        assert_eq!(PROXY_AUTHORIZATION.as_str(), "proxy-authorization");
    }
}
