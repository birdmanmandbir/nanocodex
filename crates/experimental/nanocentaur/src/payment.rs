use async_trait::async_trait;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Successful MPP proof returned to the client with a paid mutation.
#[derive(Clone, Debug)]
pub struct PaymentReceipt {
    /// Receipt value returned in the configured response header.
    pub header_value: String,
}

/// Body returned for a successful payment-protocol management request.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentManagementResponse {
    /// Management operation status.
    pub status: PaymentManagementStatus,
}

/// Result of a payment-protocol management request.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentManagementStatus {
    /// The management request completed.
    Ok,
}

/// A payment request is either billable application traffic, a protocol
/// management message, or missing a credential and therefore challenged.
#[derive(Clone, Debug)]
pub enum PaymentOutcome {
    /// Application traffic was authorized and may mutate state.
    Authorized(PaymentReceipt),
    /// The caller must satisfy the returned payment challenge.
    Challenge {
        /// Complete `WWW-Authenticate` challenge value.
        www_authenticate: String,
    },
    /// A protocol management message completed without application mutation.
    Management {
        /// Management response body.
        body: PaymentManagementResponse,
        /// Receipt returned with the management response.
        receipt: PaymentReceipt,
    },
}

/// Ingress payment authorization boundary.
#[async_trait]
pub trait PaymentGate: Send + Sync {
    /// Classifies and verifies one request from its headers.
    ///
    /// # Errors
    ///
    /// Returns a typed configuration or credential verification failure.
    async fn authorize(&self, headers: &HeaderMap) -> Result<PaymentOutcome, PaymentError>;
}

/// Development/test gate. Production starts with an MPP gate in the binary.
pub struct FreePaymentGate;

#[async_trait]
impl PaymentGate for FreePaymentGate {
    async fn authorize(&self, _headers: &HeaderMap) -> Result<PaymentOutcome, PaymentError> {
        Ok(PaymentOutcome::Authorized(PaymentReceipt {
            header_value: "free-development-mode".to_owned(),
        }))
    }
}

#[derive(Debug, Error)]
/// Ingress payment authorization failure.
pub enum PaymentError {
    /// The credential was absent, malformed, or invalid.
    #[error("invalid payment credential")]
    InvalidCredential,
    /// Server-side payment policy was invalid.
    #[error("payment configuration failed: {0}")]
    Configuration(String),
    /// Payment verification failed.
    #[error("payment verification failed: {0}")]
    Verification(String),
}
