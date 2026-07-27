use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

use mpp_egress::MppEgress;
use nanovm::{EgressError, EgressFile, EgressLease, GUEST_EGRESS_ROOT};
use thiserror::Error;

// Retain the original guest-visible projection used by
// `nanocodex_vm::mpp_egress_layer`.
const GUEST_LAYER: &str = "mpp";
const CA_FILENAME: &str = "mpp-egress-ca.pem";
const CA_ENVIRONMENT: [&str; 5] = [
    "CURL_CA_BUNDLE",
    "SSL_CERT_FILE",
    "REQUESTS_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "GIT_SSL_CAINFO",
];

/// Converts one running host proxy into a VM-facing egress lease.
///
/// The guest receives only the authenticated proxy URL and public CA. Payment
/// providers, wallets, secret managers, request policy, and signing material
/// remain in the host proxy retained by the returned lease.
///
/// # Errors
///
/// Returns an error when the public CA is unavailable, generated environment
/// is not UTF-8, or the lease conflicts with a VM egress invariant.
pub fn mpp_egress_layer(egress: Arc<MppEgress>) -> Result<EgressLease, MppVmEgressError> {
    let certificate = egress.certificate_path();
    layer_from_parts(egress.environment(), &certificate, egress)
}

fn layer_from_parts<T>(
    environment: Vec<(OsString, OsString)>,
    certificate: &Path,
    guard: Arc<T>,
) -> Result<EgressLease, MppVmEgressError>
where
    T: Send + Sync + 'static,
{
    if !certificate.is_file() {
        return Err(MppVmEgressError::CertificateNotFile(
            certificate.to_path_buf(),
        ));
    }
    let guest_certificate = Path::new(GUEST_EGRESS_ROOT)
        .join(GUEST_LAYER)
        .join(CA_FILENAME);
    let guest_certificate = guest_certificate
        .to_str()
        .ok_or(MppVmEgressError::GuestCertificatePath)?
        .to_owned();

    let mut lease = EgressLease::internet();
    lease.insert_file(EgressFile::new(
        &guest_certificate,
        std::fs::read(certificate).map_err(MppVmEgressError::ReadCertificate)?,
        0o444,
    ))?;
    for (name, value) in environment {
        let name = name
            .into_string()
            .map_err(|_| MppVmEgressError::EnvironmentName)?;
        let value = if CA_ENVIRONMENT.contains(&name.as_str()) {
            guest_certificate.clone()
        } else {
            value
                .into_string()
                .map_err(|_| MppVmEgressError::EnvironmentValue(name.clone()))?
        };
        lease.insert_environment(name, value)?;
    }
    lease.retain(guard);
    Ok(lease)
}

/// Failure to project a host proxy into a VM egress lease.
#[derive(Debug, Error)]
pub enum MppVmEgressError {
    /// The proxy public CA path is not a regular file.
    #[error("host egress CA is not a regular file: {0}")]
    CertificateNotFile(PathBuf),
    /// The public CA could not be read.
    #[error("failed to read the host egress CA: {0}")]
    ReadCertificate(#[source] std::io::Error),
    /// The fixed guest CA destination was not valid UTF-8.
    #[error("host egress guest CA path is not valid UTF-8")]
    GuestCertificatePath,
    /// A proxy-provided environment name was not valid UTF-8.
    #[error("host egress produced a non-UTF-8 environment name")]
    EnvironmentName,
    /// A proxy-provided environment value was not valid UTF-8.
    #[error("host egress produced a non-UTF-8 value for `{0}`")]
    EnvironmentValue(String),
    /// The resulting capability conflicted with VM egress invariants.
    #[error(transparent)]
    Egress(#[from] EgressError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_exposes_only_proxy_capability_and_public_ca() {
        let directory = tempfile::tempdir().unwrap();
        let certificate = directory.path().join(CA_FILENAME);
        std::fs::write(&certificate, "public ca").unwrap();
        let guard = Arc::new(());
        let environment = vec![
            (
                OsString::from("HTTPS_PROXY"),
                OsString::from("http://lease:secret@127.0.0.1:1234"),
            ),
            (
                OsString::from("CURL_CA_BUNDLE"),
                certificate.as_os_str().to_owned(),
            ),
        ];

        let lease = layer_from_parts(environment, &certificate, Arc::clone(&guard)).unwrap();

        assert_eq!(
            lease.guest_environment().get("HTTPS_PROXY"),
            Some(&"http://lease:secret@127.0.0.1:1234".to_owned())
        );
        assert_eq!(
            lease.guest_environment().get("CURL_CA_BUNDLE"),
            Some(&"/tmp/nanocodex/egress/mpp/mpp-egress-ca.pem".to_owned())
        );
        assert_eq!(lease.guest_files().count(), 1);
        assert_eq!(Arc::strong_count(&guard), 2);
        assert!(!format!("{lease:?}").contains("secret"));
    }
}
