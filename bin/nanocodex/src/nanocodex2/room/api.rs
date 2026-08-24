use std::{fmt, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;
use zeroize::Zeroize;

use super::protocol::{MemberId, ProtocolError, RoomId, valid_token, validated_display_name};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const TRANSPORT_ATTEMPTS: usize = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DELETE_ATTEMPTS: usize = 3;
const DELETE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const DELETE_RETRY_DELAY: Duration = Duration::from_millis(250);

pub(crate) struct AccountKey(String);

impl AccountKey {
    pub(crate) fn parse(value: String) -> Result<Self, RoomError> {
        let Some(rest) = value.strip_prefix("ncx_live_") else {
            return Err(RoomError::Configuration(
                "NANOCODEX_API_KEY must be an ncx_live account API key".to_owned(),
            ));
        };
        if rest.len() != 12 + 1 + 43 || !rest.is_ascii() {
            return Err(RoomError::Configuration(
                "NANOCODEX_API_KEY must be an ncx_live account API key".to_owned(),
            ));
        }
        let (id, separator_and_secret) = rest.split_at(12);
        let Some(secret) = separator_and_secret.strip_prefix('_') else {
            return Err(RoomError::Configuration(
                "NANOCODEX_API_KEY must be an ncx_live account API key".to_owned(),
            ));
        };
        if id.len() != 12
            || secret.len() != 43
            || !id.bytes().all(base64url_byte)
            || !secret.bytes().all(base64url_byte)
        {
            return Err(RoomError::Configuration(
                "NANOCODEX_API_KEY must be an ncx_live account API key".to_owned(),
            ));
        }
        Ok(Self(value))
    }
}

impl fmt::Debug for AccountKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccountKey([REDACTED])")
    }
}

impl Drop for AccountKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

fn base64url_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

#[derive(Clone)]
pub(crate) struct RoomApi {
    base_url: Url,
    public_http: reqwest::Client,
    account_http: Option<reqwest::Client>,
}

impl RoomApi {
    pub(crate) fn public(base_url: Url) -> Result<Self, RoomError> {
        let base_url = normalized_origin(base_url)?;
        let public_http = http_client(None)?;
        Ok(Self {
            base_url,
            public_http,
            account_http: None,
        })
    }

    pub(crate) fn authenticated(base_url: Url, account_key: AccountKey) -> Result<Self, RoomError> {
        let mut bearer = b"Bearer ".to_vec();
        bearer.extend_from_slice(account_key.0.as_bytes());
        let mut authorization = reqwest::header::HeaderValue::from_bytes(&bearer)
            .map_err(|_| RoomError::Configuration("NANOCODEX_API_KEY is invalid".to_owned()))?;
        bearer.zeroize();
        authorization.set_sensitive(true);
        let base_url = normalized_origin(base_url)?;
        let public_http = http_client(None)?;
        let account_http = http_client(Some(authorization))?;
        drop(account_key);
        Ok(Self {
            base_url,
            public_http,
            account_http: Some(account_http),
        })
    }

    pub(crate) async fn create(&self, display_name: &str) -> Result<CreatedRoom, RoomError> {
        let display_name = validated_display_name(display_name)?;
        let Some(http) = self.account_http.as_ref() else {
            return Err(RoomError::AuthenticationRequired);
        };
        let create_id = ReceiptId::generate();
        let body = serde_json::to_vec(&CreateRequest {
            create_id: create_id.as_str(),
            display_name,
        })
        .map_err(|_| RoomError::InvalidReceipt("failed to encode room creation"))?;
        let url = self.route("v1/rooms")?;
        let response = send_with_transport_retry(http, url, &body).await?;
        if response.status() != StatusCode::CREATED {
            return Err(response_error(response).await);
        }
        let mut wire: CreateReceiptWire = decode_json(response, None).await?;
        let cookie = MembershipCookie::extract(&wire.room_id, &wire.response_headers)?;
        let websocket_url = validate_websocket_url(
            &self.base_url,
            &wire.room_id,
            std::mem::take(&mut wire.websocket_url),
        )?;
        let invitation = RoomInvitation::parse(std::mem::take(&mut wire.invite_url))?;
        let mut invite = std::mem::take(&mut wire.invite);
        let invitation_matches = invitation.room_id == wire.room_id && invitation.invite == invite;
        invite.zeroize();
        if !invitation_matches {
            return Err(RoomError::InvalidReceipt(
                "room invitation does not match its creation receipt",
            ));
        }
        Ok(RoomMembership {
            receipt: CreateReceipt {
                room_id: wire.room_id.clone(),
                member_id: wire.member_id.clone(),
                websocket_url,
                invitation,
            },
            cookie,
            origin: self.base_url.clone(),
        })
    }

    pub(crate) async fn join(
        &self,
        invitation: &RoomInvitation,
        display_name: &str,
    ) -> Result<JoinedRoom, RoomError> {
        if invitation.origin() != self.base_url.origin().ascii_serialization() {
            return Err(RoomError::Configuration(
                "room invitation origin does not match the managed origin".to_owned(),
            ));
        }
        let display_name = validated_display_name(display_name)?;
        let join_id = ReceiptId::generate();
        let body = serde_json::to_vec(&JoinRequest {
            invite: invitation.invite(),
            display_name,
            join_id: join_id.as_str(),
        })
        .map_err(|_| RoomError::InvalidReceipt("failed to encode room join"))?;
        let url = self.route(&format!("v1/rooms/{}/join", invitation.room_id))?;
        // This client has no default Authorization header. Room admission is
        // solely authorized by the fragment-carried invite capability.
        let response = send_with_transport_retry(&self.public_http, url, &body).await?;
        if !matches!(response.status(), StatusCode::OK | StatusCode::CREATED) {
            return Err(response_error(response).await);
        }
        let wire: JoinReceiptWire = decode_json(response, Some(&invitation.room_id)).await?;
        let websocket_url =
            validate_websocket_url(&self.base_url, &wire.room_id, wire.websocket_url)?;
        Ok(RoomMembership {
            cookie: wire.cookie,
            origin: self.base_url.clone(),
            receipt: JoinReceipt {
                room_id: wire.room_id,
                member_id: wire.member_id,
                websocket_url,
            },
        })
    }

    /// Deletes an account-owned room without exposing its owner capability.
    ///
    /// Deletion is settled by either 204 or 404. A 503 means durable child
    /// cleanup is still pending, so it is retried a small, bounded number of
    /// times; other HTTP and transport failures remain typed for the caller.
    pub(crate) async fn delete_owned_room(&self, room_id: &RoomId) -> Result<(), RoomError> {
        let Some(http) = self.account_http.as_ref() else {
            return Err(RoomError::AuthenticationRequired);
        };
        let url = self.route(&format!("v1/rooms/{room_id}"))?;
        for attempt in 0..DELETE_ATTEMPTS {
            let response = http
                .delete(url.clone())
                .timeout(DELETE_REQUEST_TIMEOUT)
                .send()
                .await
                .map_err(RoomError::Transport)?;
            match response.status() {
                StatusCode::NO_CONTENT | StatusCode::NOT_FOUND => return Ok(()),
                StatusCode::SERVICE_UNAVAILABLE if attempt + 1 < DELETE_ATTEMPTS => {
                    tokio::time::sleep(DELETE_RETRY_DELAY * (attempt as u32 + 1)).await;
                }
                _ => return Err(response_error(response).await),
            }
        }
        Err(RoomError::InvalidReceipt(
            "room deletion exhausted its bounded retry policy",
        ))
    }

    fn route(&self, path: &str) -> Result<Url, RoomError> {
        self.base_url
            .join(path)
            .map_err(|_| RoomError::InvalidReceipt("managed room route is invalid"))
    }
}

impl fmt::Debug for RoomApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RoomApi")
            .field("base_url", &self.base_url)
            .field("authenticated", &self.account_http.is_some())
            .finish_non_exhaustive()
    }
}

pub(crate) type CreatedRoom = RoomMembership<CreateReceipt>;
pub(crate) type JoinedRoom = RoomMembership<JoinReceipt>;

pub(crate) struct RoomMembership<R> {
    pub(super) receipt: R,
    pub(super) cookie: MembershipCookie,
    pub(super) origin: Url,
}

impl<R: fmt::Debug> fmt::Debug for RoomMembership<R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RoomMembership")
            .field("receipt", &self.receipt)
            .field("cookie", &self.cookie)
            .finish_non_exhaustive()
    }
}

impl RoomMembership<CreateReceipt> {
    pub(crate) fn receipt(&self) -> &CreateReceipt {
        &self.receipt
    }
}

impl RoomMembership<JoinReceipt> {
    pub(crate) fn receipt(&self) -> &JoinReceipt {
        &self.receipt
    }
}

pub(crate) struct CreateReceipt {
    room_id: RoomId,
    member_id: MemberId,
    websocket_url: Url,
    invitation: RoomInvitation,
}

impl CreateReceipt {
    pub(crate) fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    pub(crate) fn member_id(&self) -> &MemberId {
        &self.member_id
    }

    pub(crate) fn invitation(&self) -> &RoomInvitation {
        &self.invitation
    }
}

impl fmt::Debug for CreateReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CreateReceipt")
            .field("room_id", &self.room_id)
            .field("member_id", &self.member_id)
            .field("websocket_url", &self.websocket_url)
            .field("invitation", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug)]
pub(crate) struct JoinReceipt {
    room_id: RoomId,
    member_id: MemberId,
    websocket_url: Url,
}

impl JoinReceipt {
    pub(crate) fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    pub(crate) fn member_id(&self) -> &MemberId {
        &self.member_id
    }
}

pub(crate) struct RoomInvitation {
    room_id: RoomId,
    invite: String,
    origin: String,
}

impl RoomInvitation {
    pub(crate) fn parse(encoded: impl Into<String>) -> Result<Self, RoomError> {
        let mut encoded = encoded.into();
        let result = (|| {
            let url = Url::parse(&encoded).map_err(|_| {
                RoomError::Configuration("room invitation URL is invalid".to_owned())
            })?;
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
                || url.path() != "/multiplayer"
            {
                return Err(RoomError::Configuration(
                    "room invitation URL is invalid".to_owned(),
                ));
            }
            let query = url.query_pairs().collect::<Vec<_>>();
            let fragment = url.fragment().ok_or_else(|| {
                RoomError::Configuration("room invitation is incomplete".to_owned())
            })?;
            let fragment = url::form_urlencoded::parse(fragment.as_bytes()).collect::<Vec<_>>();
            if query.len() != 1
                || query[0].0 != "room"
                || fragment.len() != 1
                || fragment[0].0 != "invite"
                || !valid_token(&fragment[0].1)
            {
                return Err(RoomError::Configuration(
                    "room invitation is incomplete".to_owned(),
                ));
            }
            let room_id = RoomId::parse(query[0].1.as_ref())?;
            Ok(Self {
                room_id,
                invite: fragment[0].1.to_string(),
                origin: url.origin().ascii_serialization(),
            })
        })();
        encoded.zeroize();
        result
    }

    pub(crate) fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    pub(crate) fn invite(&self) -> &str {
        &self.invite
    }

    pub(crate) fn origin(&self) -> &str {
        &self.origin
    }

    pub(crate) fn to_url(&self) -> Result<Url, RoomError> {
        let mut url = Url::parse(&self.origin)
            .map_err(|_| RoomError::InvalidReceipt("room invitation origin is invalid"))?;
        url.set_path("/multiplayer");
        url.set_query(Some(&format!("room={}", self.room_id)));
        url.set_fragment(Some(&format!("invite={}", self.invite)));
        Ok(url)
    }
}

impl fmt::Debug for RoomInvitation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RoomInvitation")
            .field("room_id", &self.room_id)
            .field("invite", &"[REDACTED]")
            .field("origin", &self.origin)
            .finish()
    }
}

impl Drop for RoomInvitation {
    fn drop(&mut self) {
        self.invite.zeroize();
    }
}

pub(crate) trait AdmissionReceipt {
    fn room_id(&self) -> &RoomId;
    fn member_id(&self) -> &MemberId;
    fn websocket_url(&self) -> &Url;
}

impl AdmissionReceipt for CreateReceipt {
    fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    fn member_id(&self) -> &MemberId {
        &self.member_id
    }

    fn websocket_url(&self) -> &Url {
        &self.websocket_url
    }
}

impl AdmissionReceipt for JoinReceipt {
    fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    fn member_id(&self) -> &MemberId {
        &self.member_id
    }

    fn websocket_url(&self) -> &Url {
        &self.websocket_url
    }
}

pub(super) struct MembershipCookie(String);

impl MembershipCookie {
    fn extract(room_id: &RoomId, headers: &reqwest::header::HeaderMap) -> Result<Self, RoomError> {
        let expected_name = room_cookie_name(room_id);
        let mut found = None;
        for header in headers.get_all(header::SET_COOKIE) {
            let encoded = header.to_str().map_err(|_| {
                RoomError::InvalidReceipt("room response contains a malformed cookie")
            })?;
            let pair = encoded.split(';').next().unwrap_or_default().trim();
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            if name.trim() != expected_name {
                continue;
            }
            if found.is_some() || !valid_token(value.trim()) {
                return Err(RoomError::InvalidReceipt(
                    "room response contains an invalid membership cookie",
                ));
            }
            found = Some(Self(format!("{expected_name}={}", value.trim())));
        }
        found.ok_or(RoomError::InvalidReceipt(
            "room response omitted its membership cookie",
        ))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for MembershipCookie {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MembershipCookie([REDACTED])")
    }
}

impl Drop for MembershipCookie {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

fn room_cookie_name(room_id: &RoomId) -> String {
    format!("nanocodex_room_{}", room_id.as_str().replace('-', ""))
}

struct ReceiptId(String);

impl ReceiptId {
    fn generate() -> Self {
        let mut bytes = [0_u8; 32];
        bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
        let encoded = URL_SAFE_NO_PAD.encode(bytes);
        bytes.zeroize();
        debug_assert!(valid_token(&encoded));
        Self(encoded)
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for ReceiptId {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Serialize)]
struct CreateRequest<'a> {
    create_id: &'a str,
    display_name: &'a str,
}

#[derive(Serialize)]
struct JoinRequest<'a> {
    invite: &'a str,
    display_name: &'a str,
    join_id: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateReceiptBody {
    room_id: RoomId,
    member_id: MemberId,
    invite: String,
    invite_url: String,
    websocket_url: String,
}

struct CreateReceiptWire {
    room_id: RoomId,
    member_id: MemberId,
    invite: String,
    invite_url: String,
    websocket_url: String,
    response_headers: reqwest::header::HeaderMap,
}

impl Drop for CreateReceiptWire {
    fn drop(&mut self) {
        self.invite.zeroize();
        self.invite_url.zeroize();
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinReceiptBody {
    room_id: RoomId,
    member_id: MemberId,
    websocket_url: String,
}

struct JoinReceiptWire {
    room_id: RoomId,
    member_id: MemberId,
    websocket_url: String,
    cookie: MembershipCookie,
}

async fn decode_json<T>(
    response: reqwest::Response,
    expected_room_id: Option<&RoomId>,
) -> Result<T, RoomError>
where
    T: DecodeReceipt,
{
    T::decode(response, expected_room_id).await
}

trait DecodeReceipt: Sized {
    async fn decode(
        response: reqwest::Response,
        expected_room_id: Option<&RoomId>,
    ) -> Result<Self, RoomError>;
}

impl DecodeReceipt for CreateReceiptWire {
    async fn decode(
        response: reqwest::Response,
        _expected_room_id: Option<&RoomId>,
    ) -> Result<Self, RoomError> {
        let headers = response.headers().clone();
        let body: CreateReceiptBody = decode_body(response).await?;
        Ok(Self {
            room_id: body.room_id,
            member_id: body.member_id,
            invite: body.invite,
            invite_url: body.invite_url,
            websocket_url: body.websocket_url,
            response_headers: headers,
        })
    }
}

impl DecodeReceipt for JoinReceiptWire {
    async fn decode(
        response: reqwest::Response,
        expected_room_id: Option<&RoomId>,
    ) -> Result<Self, RoomError> {
        let headers = response.headers().clone();
        let body: JoinReceiptBody = decode_body(response).await?;
        if expected_room_id.is_some_and(|expected| expected != &body.room_id) {
            return Err(RoomError::InvalidReceipt(
                "join receipt changed its room id",
            ));
        }
        let cookie = MembershipCookie::extract(&body.room_id, &headers)?;
        Ok(Self {
            room_id: body.room_id,
            member_id: body.member_id,
            websocket_url: body.websocket_url,
            cookie,
        })
    }
}

async fn send_with_transport_retry(
    http: &reqwest::Client,
    url: Url,
    body: &[u8],
) -> Result<reqwest::Response, RoomError> {
    let mut last_error = None;
    for _ in 0..TRANSPORT_ATTEMPTS {
        match http
            .post(url.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.to_vec())
            .send()
            .await
        {
            Ok(response) => return Ok(response),
            Err(error) => last_error = Some(error),
        }
    }
    Err(RoomError::Transport(last_error.ok_or(
        RoomError::InvalidReceipt("room transport retry lost its error"),
    )?))
}

async fn response_error(response: reqwest::Response) -> RoomError {
    let status = response.status();
    let code = decode_body::<ErrorBody>(response)
        .await
        .map_or_else(|_| "invalid_error_response".to_owned(), |body| body.error);
    RoomError::Http { status, code }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorBody {
    error: String,
}

async fn decode_body<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> Result<T, RoomError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(RoomError::ResponseTooLarge);
    }
    let bytes = response.bytes().await.map_err(RoomError::Transport)?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(RoomError::ResponseTooLarge);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| RoomError::InvalidReceipt("managed room response is malformed"))
}

fn http_client(
    authorization: Option<reqwest::header::HeaderValue>,
) -> Result<reqwest::Client, RoomError> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(authorization) = authorization {
        headers.insert(header::AUTHORIZATION, authorization);
    }
    reqwest::Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(RoomError::Transport)
}

fn normalized_origin(mut url: Url) -> Result<Url, RoomError> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(RoomError::Configuration(
            "managed room URL must be an HTTP(S) origin".to_owned(),
        ));
    }
    url.set_path("/");
    Ok(url)
}

fn validate_websocket_url(
    origin: &Url,
    room_id: &RoomId,
    encoded: String,
) -> Result<Url, RoomError> {
    let actual = Url::parse(&encoded)
        .map_err(|_| RoomError::InvalidReceipt("room WebSocket URL is invalid"))?;
    let mut expected = origin
        .join(&format!("v1/rooms/{room_id}/ws"))
        .map_err(|_| RoomError::InvalidReceipt("room WebSocket route is invalid"))?;
    expected
        .set_scheme(if origin.scheme() == "https" {
            "wss"
        } else {
            "ws"
        })
        .map_err(|()| RoomError::InvalidReceipt("room WebSocket scheme is invalid"))?;
    if actual != expected {
        return Err(RoomError::InvalidReceipt(
            "room WebSocket URL changed its public origin or route",
        ));
    }
    Ok(actual)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RoomError {
    #[error("{0}")]
    Configuration(String),
    #[error("creating a managed room requires an account API key")]
    AuthenticationRequired,
    #[error("managed room request failed")]
    Transport(#[source] reqwest::Error),
    #[error("managed room request failed ({status}): {code}")]
    Http { status: StatusCode, code: String },
    #[error("managed room response exceeded {MAX_RESPONSE_BYTES} bytes")]
    ResponseTooLarge,
    #[error("managed room response is malformed: {0}")]
    InvalidReceipt(&'static str),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("managed room WebSocket handshake timed out")]
    HandshakeTimeout,
    #[error("managed room WebSocket failed")]
    WebSocket(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("managed room WebSocket closed before its ready message")]
    ClosedBeforeReady,
    #[error("managed room command channel is closed")]
    CommandChannelClosed,
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Json, Router, extract::State, http::HeaderMap as AxumHeaderMap, response::IntoResponse,
        routing::delete,
    };
    use reqwest::header::{HeaderMap, HeaderValue, SET_COOKIE};
    use serde_json::json;

    use super::super::protocol::RoomId;
    use super::{AccountKey, MembershipCookie, RoomApi, RoomInvitation};

    const ROOM: &str =
        "0198d214-0d9d-7a45-8a89-123456789abc~AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const INVITE: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    #[test]
    fn secrets_are_redacted() {
        let key =
            AccountKey::parse(format!("ncx_live_{}_{}", "a".repeat(12), "b".repeat(43))).unwrap();
        assert_eq!(format!("{key:?}"), "AccountKey([REDACTED])");

        let room = RoomId::parse(ROOM).unwrap();
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_str(&format!("other=value; Path=/, ignored=still-ignored")).unwrap(),
        );
        headers.append(
            SET_COOKIE,
            HeaderValue::from_str(&format!(
                "nanocodex_room_{}={INVITE}; Path=/v1/rooms/{ROOM}; HttpOnly",
                ROOM.replace('-', "")
            ))
            .unwrap(),
        );
        let cookie = MembershipCookie::extract(&room, &headers).unwrap();
        assert_eq!(format!("{cookie:?}"), "MembershipCookie([REDACTED])");
        assert!(!format!("{cookie:?}").contains(INVITE));
    }

    #[test]
    fn account_key_id_may_contain_base64url_underscore() {
        let key = AccountKey::parse(format!("ncx_live_{}_{}", "abc_defghijk", "b".repeat(43)));
        assert!(key.is_ok());
    }

    #[test]
    fn parses_fragment_only_invitation() {
        let encoded = format!("https://managed.example/multiplayer?room={ROOM}#invite={INVITE}");
        let invitation = RoomInvitation::parse(encoded).unwrap();
        assert_eq!(invitation.room_id().as_str(), ROOM);
        assert_eq!(invitation.invite(), INVITE);
        assert_eq!(
            invitation.to_url().unwrap().as_str(),
            format!("https://managed.example/multiplayer?room={ROOM}#invite={INVITE}")
        );
        assert!(!format!("{invitation:?}").contains(INVITE));
    }

    #[test]
    fn rejects_invite_in_query_or_extra_fields() {
        assert!(
            RoomInvitation::parse(format!(
                "https://managed.example/multiplayer?room={ROOM}&invite={INVITE}"
            ))
            .is_err()
        );
        assert!(
            RoomInvitation::parse(format!(
                "https://managed.example/multiplayer?room={ROOM}#invite={INVITE}&turn_id=x"
            ))
            .is_err()
        );
    }

    #[tokio::test]
    async fn owner_delete_retries_bounded_cleanup_pending_and_settles() {
        crate::install_tls_provider();
        let attempts = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/v1/rooms/{room_id}", delete(delete_room))
            .with_state(Arc::clone(&attempts));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let api = RoomApi::authenticated(
            format!("http://{address}").parse().unwrap(),
            AccountKey::parse(format!("ncx_live_{}_{}", "a".repeat(12), "b".repeat(43))).unwrap(),
        )
        .unwrap();

        api.delete_owned_room(&RoomId::parse(ROOM).unwrap())
            .await
            .unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        server.abort();
    }

    #[tokio::test]
    async fn owner_delete_treats_absent_room_as_settled() {
        crate::install_tls_provider();
        let app = Router::new().route(
            "/v1/rooms/{room_id}",
            delete(|| async { axum::http::StatusCode::NOT_FOUND }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let api = RoomApi::authenticated(
            format!("http://{address}").parse().unwrap(),
            AccountKey::parse(format!("ncx_live_{}_{}", "a".repeat(12), "b".repeat(43))).unwrap(),
        )
        .unwrap();

        api.delete_owned_room(&RoomId::parse(ROOM).unwrap())
            .await
            .unwrap();
        server.abort();
    }

    async fn delete_room(
        State(attempts): State<Arc<AtomicUsize>>,
        headers: AxumHeaderMap,
    ) -> impl IntoResponse {
        assert!(headers.contains_key(axum::http::header::AUTHORIZATION));
        if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "agent_cleanup_pending"})),
            )
                .into_response()
        } else {
            axum::http::StatusCode::NO_CONTENT.into_response()
        }
    }
}
