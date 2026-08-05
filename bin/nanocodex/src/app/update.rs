//! Nanocodex release discovery and verified release-artifact downloads.

use reqwest::Client;
use semver::Version;
use serde::Deserialize;
use std::{
    io::{self, Read, Write},
    time::Duration,
};
use tact_sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;

const GITHUB_LATEST_RELEASE: &str =
    "https://api.github.com/repos/gakonst/nanocodex/releases/latest";
const RELEASE_DOWNLOAD_BASE: &str = "https://github.com/gakonst/nanocodex/releases/download";
const CHECKSUMS_NAME: &str = "SHA256SUMS";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_CHECKSUM_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Error)]
pub(crate) enum UpdateError {
    #[error("the built-in Nanocodex version is invalid: {0}")]
    CurrentVersion(#[source] semver::Error),
    #[error("GitHub returned an invalid release version `{version}`: {source}")]
    ReleaseVersion {
        version: String,
        #[source]
        source: semver::Error,
    },
    #[error("failed to create the update HTTP client: {0}")]
    Client(#[source] reqwest::Error),
    #[error("failed to {operation}: {source}")]
    Http {
        operation: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("GitHub returned invalid release metadata: {0}")]
    GithubMetadata(#[source] serde_json::Error),
    #[error("{name} exceeds the {limit}-byte download limit")]
    DownloadTooLarge { name: String, limit: u64 },
    #[error("release v{version} is missing `{name}`")]
    MissingAsset { version: Version, name: String },
    #[error("release checksum file `{name}` is malformed")]
    ChecksumFile { name: String },
    #[error("downloaded release artifact does not match `{name}`")]
    ArtifactChecksumMismatch { name: String },
    #[error("failed to create temporary release storage: {0}")]
    TemporaryStorage(#[source] io::Error),
    #[error("failed to write downloaded release data: {0}")]
    TemporaryWrite(#[source] io::Error),
}

#[derive(Debug, Deserialize)]
struct GithubReleaseResponse {
    tag_name: String,
}

pub(crate) async fn check_for_update() -> Result<Option<Version>, UpdateError> {
    let current = Version::parse(env!("CARGO_PKG_VERSION")).map_err(UpdateError::CurrentVersion)?;
    let client = http_client()?;
    let bytes = fetch_bytes(
        &client,
        GITHUB_LATEST_RELEASE,
        "GitHub release metadata",
        MAX_METADATA_BYTES,
    )
    .await?;
    let response: GithubReleaseResponse =
        serde_json::from_slice(&bytes).map_err(UpdateError::GithubMetadata)?;
    let raw_version = response
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&response.tag_name);
    let latest = Version::parse(raw_version).map_err(|source| UpdateError::ReleaseVersion {
        version: response.tag_name,
        source,
    })?;
    Ok((latest > current).then_some(latest))
}

pub(crate) async fn download_verified_release_artifact(
    version: &Version,
    artifact_name: &str,
    max_artifact_bytes: u64,
) -> Result<NamedTempFile, UpdateError> {
    let client = http_client()?;
    let base = format!("{RELEASE_DOWNLOAD_BASE}/v{version}");
    let checksum_url = format!("{base}/{CHECKSUMS_NAME}");
    let checksum_bytes =
        fetch_bytes(&client, &checksum_url, CHECKSUMS_NAME, MAX_CHECKSUM_BYTES).await?;
    let expected = checksum_for(&checksum_bytes, artifact_name)
        .map_err(|()| UpdateError::ChecksumFile {
            name: CHECKSUMS_NAME.to_owned(),
        })?
        .ok_or_else(|| UpdateError::MissingAsset {
            version: version.clone(),
            name: artifact_name.to_owned(),
        })?;

    let artifact = NamedTempFile::new().map_err(UpdateError::TemporaryStorage)?;
    download_to_file(
        &client,
        &format!("{base}/{artifact_name}"),
        artifact_name,
        max_artifact_bytes,
        &artifact,
    )
    .await?;
    let actual = hash_file(&artifact).map_err(UpdateError::TemporaryWrite)?;
    if actual != expected {
        return Err(UpdateError::ArtifactChecksumMismatch {
            name: artifact_name.to_owned(),
        });
    }
    Ok(artifact)
}

fn http_client() -> Result<Client, UpdateError> {
    Client::builder()
        .user_agent(concat!("nanocodex/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(UpdateError::Client)
}

async fn fetch_bytes(
    client: &Client,
    url: &str,
    name: &str,
    limit: u64,
) -> Result<Vec<u8>, UpdateError> {
    let mut response = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|source| UpdateError::Http {
            operation: "download release metadata",
            source,
        })?;
    enforce_content_length(&response, name, limit)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|source| UpdateError::Http {
        operation: "read release metadata",
        source,
    })? {
        let length = (bytes.len() as u64).saturating_add(chunk.len() as u64);
        if length > limit {
            return Err(UpdateError::DownloadTooLarge {
                name: name.to_owned(),
                limit,
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn download_to_file(
    client: &Client,
    url: &str,
    name: &str,
    limit: u64,
    file: &NamedTempFile,
) -> Result<(), UpdateError> {
    let mut response = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|source| UpdateError::Http {
            operation: "download release artifact",
            source,
        })?;
    enforce_content_length(&response, name, limit)?;
    let mut output = file.reopen().map_err(UpdateError::TemporaryWrite)?;
    let mut written = 0_u64;
    while let Some(chunk) = response.chunk().await.map_err(|source| UpdateError::Http {
        operation: "read release artifact",
        source,
    })? {
        written = written.saturating_add(chunk.len() as u64);
        if written > limit {
            return Err(UpdateError::DownloadTooLarge {
                name: name.to_owned(),
                limit,
            });
        }
        output
            .write_all(&chunk)
            .map_err(UpdateError::TemporaryWrite)?;
    }
    output.flush().map_err(UpdateError::TemporaryWrite)
}

fn enforce_content_length(
    response: &reqwest::Response,
    name: &str,
    limit: u64,
) -> Result<(), UpdateError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err(UpdateError::DownloadTooLarge {
            name: name.to_owned(),
            limit,
        });
    }
    Ok(())
}

fn checksum_for(bytes: &[u8], artifact_name: &str) -> Result<Option<[u8; 32]>, ()> {
    let text = std::str::from_utf8(bytes).map_err(|_| ())?;
    let mut match_digest = None;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let digest = fields.next().ok_or(())?;
        let name = fields.next().ok_or(())?.trim_start_matches('*');
        if fields.next().is_some() {
            return Err(());
        }
        if name == artifact_name {
            if match_digest.is_some() {
                return Err(());
            }
            match_digest = parse_hex_checksum(digest);
            match_digest.ok_or(())?;
        } else if parse_hex_checksum(digest).is_none() {
            return Err(());
        }
    }
    Ok(match_digest)
}

fn parse_hex_checksum(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let bytes = hex::decode(value).ok()?;
    bytes.try_into().ok()
}

fn hash_file(file: &NamedTempFile) -> io::Result<[u8; 32]> {
    let mut input = file.reopen()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::checksum_for;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn finds_exact_artifact_checksum() {
        let checksums = format!("{DIGEST}  first\n{DIGEST} *wanted\n");
        assert_eq!(
            checksum_for(checksums.as_bytes(), "wanted"),
            Ok(Some(hex::decode(DIGEST).unwrap().try_into().unwrap()))
        );
    }

    #[test]
    fn rejects_duplicate_or_malformed_checksum_entries() {
        let duplicate = format!("{DIGEST}  wanted\n{DIGEST}  wanted\n");
        assert_eq!(checksum_for(duplicate.as_bytes(), "wanted"), Err(()));
        assert_eq!(checksum_for(b"not-a-checksum  wanted\n", "wanted"), Err(()));
        assert_eq!(
            checksum_for(format!("{DIGEST}  wanted extra\n").as_bytes(), "wanted"),
            Err(())
        );
    }
}
