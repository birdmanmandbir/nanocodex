use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

use mpp_egress::MppEgress;
use nanovm::{EgressError, EgressLease, EgressMount};
use thiserror::Error;

const MPP_MOUNT_TAG: &str = "nanocodex-mpp-egress";
const GUEST_DIRECTORY: &str = "/run/nanocodex/egress/mpp";
const CA_FILENAME: &str = "mpp-egress-ca.pem";
const CA_ENVIRONMENT: [&str; 4] = [
    "CURL_CA_BUNDLE",
    "SSL_CERT_FILE",
    "REQUESTS_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
];

/// Converts one running MPP proxy into a VM-facing egress layer.
///
/// The guest receives only the proxy lease credentials and public CA. The
/// payment provider and wallet remain in the host proxy retained by the
/// returned lease.
///
/// # Errors
///
/// Returns an error when the proxy's public CA is unavailable, generated
/// environment is not UTF-8, or it conflicts with another value in the layer.
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
    let host_directory = certificate
        .parent()
        .ok_or_else(|| MppVmEgressError::CertificateWithoutParent(certificate.to_path_buf()))?;
    let guest_directory = PathBuf::from(GUEST_DIRECTORY);
    let guest_certificate = guest_directory.join(CA_FILENAME);
    let guest_certificate = guest_certificate
        .to_str()
        .ok_or(MppVmEgressError::GuestCertificatePath)?
        .to_owned();

    let mut lease = EgressLease::internet();
    lease.insert_mount(EgressMount {
        tag: MPP_MOUNT_TAG.to_owned(),
        host_path: host_directory.to_path_buf(),
        guest_path: guest_directory,
    })?;
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

#[derive(Debug, Error)]
pub enum MppVmEgressError {
    #[error("MPP egress CA is not a regular file: {0}")]
    CertificateNotFile(PathBuf),
    #[error("MPP egress CA path has no parent directory: {0}")]
    CertificateWithoutParent(PathBuf),
    #[error("MPP guest CA path is not valid UTF-8")]
    GuestCertificatePath,
    #[error("MPP egress produced a non-UTF-8 environment name")]
    EnvironmentName,
    #[error("MPP egress produced a non-UTF-8 value for `{0}`")]
    EnvironmentValue(String),
    #[error(transparent)]
    Egress(#[from] EgressError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpp_layer_routes_curl_through_proxy_and_mounts_public_ca() {
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
            Some(&format!("{GUEST_DIRECTORY}/{CA_FILENAME}"))
        );
        assert_eq!(lease.guest_mounts().count(), 1);
        assert_eq!(Arc::strong_count(&guard), 2);
        assert!(!format!("{lease:?}").contains("secret"));
    }
}
