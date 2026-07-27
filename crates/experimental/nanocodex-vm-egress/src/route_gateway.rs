use std::{
    collections::{BTreeSet, HashMap},
    fmt::Write as _,
    sync::{Arc, RwLock},
};

use futures_util::TryStreamExt;
use http::{
    HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode, Uri,
    header::{
        AUTHORIZATION, CONNECTION, CONTENT_LENGTH, HOST, PROXY_AUTHORIZATION, TRANSFER_ENCODING,
    },
    uri::Authority,
};
use nanovm::{EgressError, EgressLease};
use reqwest::redirect::Policy;
use thiserror::Error;
use url::Url;

use crate::{
    EgressContext, SecretDelivery, SecretManager, SecretPolicy, SecretPolicyError, SecretSpec,
    gateway::allows_request,
};

/// Maximum accepted body size for one scoped reverse-gateway request.
pub const MAX_SECRET_GATEWAY_REQUEST_BYTES: usize = 16 * 1024 * 1024;
/// Maximum retained body size for one scoped reverse-gateway response.
pub const MAX_SECRET_GATEWAY_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone)]
struct LeaseAuthorization {
    context: EgressContext,
    policy: Arc<dyn SecretPolicy>,
    manager: Arc<dyn SecretManager>,
}

struct LeaseGuard {
    token: String,
    leases: Arc<RwLock<HashMap<String, LeaseAuthorization>>>,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if let Ok(mut leases) = self.leases.write() {
            leases.remove(&self.token);
        }
    }
}

/// Scoped reverse gateway for clients that use an SDK base URL but ignore
/// standard HTTP proxy variables.
///
/// [`crate::VmEgressBuilder::secret_gateway`] installs a random per-lease
/// route in each configured secret's guest base-URL variable. The route and
/// the normal authenticated proxy query the same live [`SecretPolicy`] and
/// [`SecretManager`], so rotation and revocation take effect immediately on
/// either transport. Resolved values never enter the VM lease.
pub struct SecretGateway {
    public_base_url: Url,
    client: reqwest::Client,
    leases: Arc<RwLock<HashMap<String, LeaseAuthorization>>>,
}

impl SecretGateway {
    /// Creates a gateway at an HTTP(S) base URL reachable from the guest.
    ///
    /// The embedding server must route
    /// `/internal/v1/secret-egress/{lease_token}/{secret_id}/...` to
    /// [`Self::forward`].
    ///
    /// # Errors
    ///
    /// Returns an error for a relative, credential-bearing, queried, or
    /// otherwise unsafe public URL, or when the redirect-disabled upstream
    /// client cannot be constructed.
    pub fn new(public_base_url: impl AsRef<str>) -> Result<Self, SecretGatewayError> {
        let public_base_url = Url::parse(public_base_url.as_ref())
            .map_err(|_| SecretGatewayError::InvalidPublicUrl)?;
        if !matches!(public_base_url.scheme(), "http" | "https")
            || public_base_url.host_str().is_none()
            || !public_base_url.username().is_empty()
            || public_base_url.password().is_some()
            || public_base_url.query().is_some()
            || public_base_url.fragment().is_some()
        {
            return Err(SecretGatewayError::InvalidPublicUrl);
        }
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .build()
            .map_err(SecretGatewayError::Client)?;
        Ok(Self {
            public_base_url,
            client,
            leases: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub(crate) fn install(
        &self,
        lease: &mut EgressLease,
        context: EgressContext,
        policy: Arc<dyn SecretPolicy>,
        manager: Arc<dyn SecretManager>,
        secrets: &[SecretSpec],
    ) -> Result<(), SecretGatewayError> {
        if secrets.is_empty() {
            return Ok(());
        }
        let token = random_token()?;
        for secret in secrets {
            lease.insert_environment(
                secret.guest().base_url_environment(),
                self.route_url(&token, secret.id())?,
            )?;
            if let Some(name) = secret.guest().placeholder_environment_name() {
                let placeholder = secret
                    .delivery()
                    .placeholder()
                    .ok_or(SecretGatewayError::InvalidConfiguration)?;
                lease.insert_environment(name, placeholder)?;
            }
        }
        self.leases
            .write()
            .map_err(|_| SecretGatewayError::Unavailable)?
            .insert(
                token.clone(),
                LeaseAuthorization {
                    context,
                    policy,
                    manager,
                },
            );
        lease.retain(Arc::new(LeaseGuard {
            token,
            leases: Arc::clone(&self.leases),
        }));
        Ok(())
    }

    pub(crate) fn matches_connect(&self, uri: &Uri) -> bool {
        if self.public_base_url.scheme() != "https" {
            return false;
        }
        uri.authority()
            .cloned()
            .or_else(|| uri.to_string().parse().ok())
            .is_some_and(|authority| self.matches_origin("https", &authority))
    }

    pub(crate) fn matches_request(&self, uri: &Uri) -> bool {
        let Some(authority) = uri.authority() else {
            return false;
        };
        let scheme = uri.scheme_str().unwrap_or(self.public_base_url.scheme());
        if !self.matches_origin(scheme, authority) {
            return false;
        }
        let base = self.public_base_url.path().trim_end_matches('/');
        let prefix = format!("{base}/internal/v1/secret-egress/");
        uri.path().starts_with(&prefix)
    }

    fn matches_origin(&self, scheme: &str, authority: &Authority) -> bool {
        self.public_base_url.scheme() == scheme
            && self
                .public_base_url
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case(authority.host()))
            && self.public_base_url.port_or_known_default()
                == authority
                    .port_u16()
                    .or_else(|| (scheme == "https").then_some(443))
                    .or_else(|| (scheme == "http").then_some(80))
    }

    /// Authorizes, injects, and forwards one request from a scoped route.
    ///
    /// Callers should bound an incoming streaming body before constructing the
    /// request. This method enforces the same limit again and bounds the
    /// upstream response while it is produced.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the lease expired, live policy denied the
    /// route, the request escaped its method/path scope, secret resolution
    /// failed, or the bounded upstream operation failed.
    pub async fn forward(
        &self,
        lease_token: &str,
        secret_id: &str,
        path: &str,
        request: Request<Vec<u8>>,
    ) -> Result<Response<Vec<u8>>, SecretGatewayError> {
        if request.body().len() > MAX_SECRET_GATEWAY_REQUEST_BYTES {
            return Err(SecretGatewayError::RequestTooLarge);
        }
        let authorization = self
            .leases
            .read()
            .map_err(|_| SecretGatewayError::Unavailable)?
            .get(lease_token)
            .cloned()
            .ok_or(SecretGatewayError::InvalidLease)?;
        let secrets = authorization
            .policy
            .secrets(&authorization.context)
            .await
            .map_err(SecretGatewayError::Policy)?;
        let secret = secrets
            .iter()
            .find(|secret| secret.id() == secret_id)
            .ok_or(SecretGatewayError::RequestDenied)?;
        let (parts, body) = request.into_parts();
        let destination = destination(secret, path, parts.uri.query())?;
        let upstream =
            Url::parse(secret.upstream()).map_err(|_| SecretGatewayError::InvalidConfiguration)?;
        if !allows_request(secret, &upstream, &parts.method, destination.path()) {
            return Err(SecretGatewayError::RequestDenied);
        }

        let mut headers = filtered_request_headers(&parts.headers, secret.delivery());
        let value = authorization
            .manager
            .resolve(secret.source())
            .await
            .map_err(|_| SecretGatewayError::Resolution)?;
        apply_delivery(&mut headers, secret.delivery(), &value)?;

        let upstream = self
            .client
            .request(parts.method.clone(), destination)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|_| SecretGatewayError::Upstream)?;
        let status = upstream.status();
        let headers = filtered_response_headers(upstream.headers());
        let mut body = Vec::new();
        let mut stream = upstream.bytes_stream();
        while let Some(chunk) = stream
            .try_next()
            .await
            .map_err(|_| SecretGatewayError::Upstream)?
        {
            if body.len().saturating_add(chunk.len()) > MAX_SECRET_GATEWAY_RESPONSE_BYTES {
                return Err(SecretGatewayError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }

        let mut response = Response::new(body);
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        tracing::info!(
            target: "nanocodex_vm_egress",
            secret_route_id = %secret.id(),
            egress.agent.id = authorization.context.agent_id(),
            egress.principal.id = authorization.context.principal(),
            http.request.method = %parts.method,
            http.response.status_code = status.as_u16(),
            "authorized scoped secret gateway request"
        );
        Ok(response)
    }

    fn route_url(&self, token: &str, secret_id: &str) -> Result<String, SecretGatewayError> {
        let mut url = self.public_base_url.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|()| SecretGatewayError::InvalidPublicUrl)?;
            segments.pop_if_empty();
            segments.extend(["internal", "v1", "secret-egress", token, secret_id]);
        }
        Ok(url.into())
    }
}

fn random_token() -> Result<String, SecretGatewayError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| SecretGatewayError::Random)?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut token, "{byte:02x}").map_err(|_| SecretGatewayError::Random)?;
    }
    Ok(token)
}

fn destination(
    secret: &SecretSpec,
    path: &str,
    query: Option<&str>,
) -> Result<Url, SecretGatewayError> {
    let path =
        crate::secret::safe_request_path(path).ok_or(SecretGatewayError::InvalidConfiguration)?;
    let mut destination =
        Url::parse(secret.upstream()).map_err(|_| SecretGatewayError::InvalidConfiguration)?;
    let base = destination.path().trim_end_matches('/');
    destination.set_path(&format!("{base}{path}"));
    destination.set_query(query);
    Ok(destination)
}

fn filtered_request_headers(headers: &HeaderMap, delivery: &SecretDelivery) -> HeaderMap {
    let delivery_header = delivery.header();
    let connection_headers = connection_headers(headers);
    headers
        .iter()
        .filter(|(name, _)| {
            !is_hop_by_hop(name)
                && !connection_headers.contains(name.as_str())
                && *name != HOST
                && *name != CONTENT_LENGTH
                && *name != PROXY_AUTHORIZATION
                && (*name != AUTHORIZATION || name.as_str().eq_ignore_ascii_case(delivery_header))
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn filtered_response_headers(headers: &HeaderMap) -> HeaderMap {
    let connection_headers = connection_headers(headers);
    headers
        .iter()
        .filter(|(name, _)| {
            !is_hop_by_hop(name)
                && !connection_headers.contains(name.as_str())
                && *name != CONTENT_LENGTH
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    *name == CONNECTION
        || *name == TRANSFER_ENCODING
        || matches!(
            name.as_str(),
            "keep-alive" | "proxy-authenticate" | "te" | "trailer" | "upgrade"
        )
}

fn connection_headers(headers: &HeaderMap) -> BTreeSet<String> {
    headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn apply_delivery(
    headers: &mut HeaderMap,
    delivery: &SecretDelivery,
    secret: &str,
) -> Result<(), SecretGatewayError> {
    let name = HeaderName::from_bytes(delivery.header().as_bytes())
        .map_err(|_| SecretGatewayError::InvalidConfiguration)?;
    match delivery {
        SecretDelivery::InjectHeader { prefix, .. } => {
            let value = HeaderValue::from_str(&format!("{prefix}{secret}"))
                .map_err(|_| SecretGatewayError::Resolution)?;
            headers.insert(name, value);
        }
        SecretDelivery::ReplaceHeader { placeholder, .. } => {
            let value = headers
                .get(&name)
                .and_then(|value| value.to_str().ok())
                .filter(|value| value.contains(placeholder))
                .ok_or(SecretGatewayError::MissingPlaceholder)?;
            let value = HeaderValue::from_str(&value.replace(placeholder, secret))
                .map_err(|_| SecretGatewayError::Resolution)?;
            headers.insert(name, value);
        }
    }
    Ok(())
}

/// Scoped reverse-gateway failure.
#[derive(Debug, Error)]
pub enum SecretGatewayError {
    /// The public route base was not a safe absolute HTTP(S) URL.
    #[error("secret gateway public URL is invalid")]
    InvalidPublicUrl,
    /// The redirect-disabled upstream client could not be built.
    #[error("secret gateway HTTP client could not be built")]
    Client(#[source] reqwest::Error),
    /// Secure lease-token generation failed.
    #[error("secret gateway lease token could not be generated")]
    Random,
    /// The lease expired or never existed.
    #[error("secret gateway lease is invalid")]
    InvalidLease,
    /// Live secret policy denied or failed to resolve the identity.
    #[error("secret gateway policy failed")]
    Policy(#[source] SecretPolicyError),
    /// The secret, method, origin, or path was outside the lease scope.
    #[error("secret gateway request is outside the configured rules")]
    RequestDenied,
    /// The incoming request exceeded the bounded body limit.
    #[error("secret gateway request body is too large")]
    RequestTooLarge,
    /// The upstream response exceeded the bounded body limit.
    #[error("secret gateway upstream response is too large")]
    ResponseTooLarge,
    /// Validated route configuration could not be projected safely.
    #[error("secret gateway configuration is invalid")]
    InvalidConfiguration,
    /// Placeholder replacement was requested without the public placeholder.
    #[error("secret gateway placeholder is missing")]
    MissingPlaceholder,
    /// The host secret manager could not resolve or encode the value.
    #[error("secret could not be resolved")]
    Resolution,
    /// The upstream request failed.
    #[error("secret gateway upstream request failed")]
    Upstream,
    /// The in-process lease registry is unavailable.
    #[error("secret gateway is unavailable")]
    Unavailable,
    /// Public guest environment projection failed.
    #[error(transparent)]
    Lease(#[from] EgressError),
}

impl SecretGatewayError {
    /// Returns the stable HTTP status for an embedding server adapter.
    #[must_use]
    pub const fn status_code(&self) -> StatusCode {
        match self {
            Self::InvalidLease | Self::Policy(_) | Self::RequestDenied => StatusCode::FORBIDDEN,
            Self::MissingPlaceholder | Self::InvalidConfiguration => StatusCode::BAD_REQUEST,
            Self::RequestTooLarge | Self::ResponseTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::InvalidPublicUrl
            | Self::Client(_)
            | Self::Random
            | Self::Resolution
            | Self::Upstream
            | Self::Unavailable
            | Self::Lease(_) => StatusCode::BAD_GATEWAY,
        }
    }

    /// Returns a stable structural classification for tracing and metrics.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::InvalidPublicUrl => "invalid_public_url",
            Self::Client(_) => "client",
            Self::Random => "random",
            Self::InvalidLease => "invalid_lease",
            Self::Policy(_) => "policy",
            Self::RequestDenied => "request_denied",
            Self::RequestTooLarge => "request_too_large",
            Self::ResponseTooLarge => "response_too_large",
            Self::InvalidConfiguration => "invalid_configuration",
            Self::MissingPlaceholder => "missing_placeholder",
            Self::Resolution => "resolution",
            Self::Upstream => "upstream",
            Self::Unavailable => "unavailable",
            Self::Lease(_) => "lease",
        }
    }
}
