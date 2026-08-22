use eyre::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{Client, Url, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use super::{MAX_BINARY_BYTES, PUBLIC_RELEASE_ORIGIN, download_from_url, lower_hex};

const PUBLIC_PULL_REQUEST_API: &str = "https://nanocodex.me-7fb.workers.dev/api/ci/pull-requests";
const REPOSITORY: &str = "gakonst/nanocodex";
const MAX_MANIFEST_BYTES: usize = 256 * 1024;

pub(super) struct Artifact {
    pub(super) contents: Vec<u8>,
    pub(super) pull_request_head: String,
    pub(super) merge_head: String,
    pub(super) manifest_sha256: String,
    pub(super) run_url: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
struct PullRequestResponse {
    version: u8,
    lane: PullRequestLane,
    run: PullRequestRun,
    native: Option<NativeProjection>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
struct PullRequestLane {
    #[serde(rename = "type")]
    kind: String,
    number: u64,
    branch: String,
    r#ref: String,
    merge_head: String,
    pull_request_head: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
struct PullRequestRun {
    version: u8,
    head: String,
    state: String,
    published_at: String,
    workflow: WorkflowState,
    result: Option<PullRequestResult>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct WorkflowState {
    status: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
struct PullRequestResult {
    version: u8,
    head: String,
    status: String,
    completed_at: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
struct NativeProjection {
    manifest_sha256: String,
    manifest_path: String,
    manifest: PullRequestManifest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
struct PullRequestManifest {
    version: u8,
    repository: String,
    pull_request: u64,
    pull_request_head: String,
    merge_head: String,
    workflow_id: String,
    completed_at: String,
    artifacts: Vec<PullRequestArtifact>,
    manifest_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
struct PullRequestArtifact {
    name: String,
    platform: String,
    size: u64,
    sha256: String,
    content_type: String,
    download_path: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CanonicalManifest<'a> {
    artifacts: Vec<CanonicalArtifact<'a>>,
    completed_at: &'a str,
    merge_head: &'a str,
    pull_request: u64,
    pull_request_head: &'a str,
    repository: &'a str,
    version: u8,
    workflow_id: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CanonicalArtifact<'a> {
    content_type: &'a str,
    download_path: &'a str,
    name: &'a str,
    platform: &'a str,
    sha256: &'a str,
    size: u64,
}

#[derive(Deserialize)]
struct ServiceError {
    error: String,
}

pub(super) async fn download(client: &Client, number: u64, asset_name: &str) -> Result<Artifact> {
    let alias_url = format!("{PUBLIC_PULL_REQUEST_API}/{number}");
    let alias: PullRequestResponse = fetch_json(client, &alias_url, "pull-request build").await?;
    let alias_native = validate_pull_request(&alias, number)?.clone();

    let exact_url = public_url(&alias_native.manifest_path, "pull-request manifest")?;
    let exact: PullRequestManifest = fetch_json(
        client,
        exact_url.as_str(),
        "immutable pull-request manifest",
    )
    .await?;
    validate_manifest(
        &exact,
        number,
        &alias.lane.pull_request_head,
        &alias.lane.merge_head,
    )?;
    if exact != alias_native.manifest {
        bail!("the current pull-request pointer does not match its immutable manifest");
    }

    let asset = exact
        .artifacts
        .iter()
        .find(|artifact| artifact.name == asset_name)
        .ok_or_else(|| eyre::eyre!("PR #{number} has no artifact named {asset_name}"))?;
    let asset_url = public_url(&asset.download_path, "pull-request artifact")?;
    let contents = download_from_url(client, asset_url, asset_name, true).await?;
    verify_asset(asset, &contents)?;

    let current: PullRequestResponse =
        fetch_json(client, &alias_url, "current pull-request build").await?;
    let current_native = validate_pull_request(&current, number)?;
    if current.lane.pull_request_head != alias.lane.pull_request_head
        || current.lane.merge_head != alias.lane.merge_head
        || current_native.manifest_sha256 != alias_native.manifest_sha256
        || current_native.manifest != alias_native.manifest
    {
        bail!("PR #{number} changed while its artifact was downloading");
    }

    Ok(Artifact {
        contents,
        pull_request_head: alias.lane.pull_request_head,
        merge_head: alias.lane.merge_head,
        manifest_sha256: alias_native.manifest_sha256,
        run_url: exact_url.to_string(),
    })
}

async fn fetch_json<T: DeserializeOwned>(client: &Client, url: &str, label: &str) -> Result<T> {
    let response = client
        .get(url)
        .header(header::ACCEPT, "application/json")
        .send()
        .await
        .wrap_err_with(|| format!("failed to query the {label}"))?;
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MANIFEST_BYTES as u64)
    {
        bail!("the Nanocodex CI service returned an oversized {label}");
    }
    let capacity = response
        .content_length()
        .unwrap_or(0)
        .min(MAX_MANIFEST_BYTES as u64) as usize;
    let mut bytes = Vec::with_capacity(capacity);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.wrap_err_with(|| format!("failed to read the {label}"))?;
        if chunk.len() > MAX_MANIFEST_BYTES.saturating_sub(bytes.len()) {
            bail!("the Nanocodex CI service returned an oversized {label}");
        }
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let code = serde_json::from_slice::<ServiceError>(&bytes)
            .ok()
            .map(|value| value.error)
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 128
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            });
        if let Some(code) = code {
            bail!("the Nanocodex CI service returned HTTP {status} for {label}: {code}");
        }
        bail!("the Nanocodex CI service returned HTTP {status} for {label}");
    }
    serde_json::from_slice(&bytes)
        .wrap_err_with(|| format!("the Nanocodex CI service returned invalid {label} metadata"))
}

fn validate_pull_request(value: &PullRequestResponse, number: u64) -> Result<&NativeProjection> {
    if value.version != 1
        || value.lane.kind != "pull_request"
        || value.lane.number != number
        || value.lane.branch != format!("pull/{number}/merge")
        || value.lane.r#ref != format!("refs/pull/{number}/merge")
        || !lower_hex(&value.lane.pull_request_head, 40)
        || !lower_hex(&value.lane.merge_head, 40)
        || value.run.version != 1
        || value.run.head != value.lane.merge_head
        || value.run.state != "dispatched"
        || value.run.published_at.is_empty()
    {
        bail!("the Nanocodex CI service returned an invalid current PR #{number} build");
    }
    if value.run.workflow.status != "complete" {
        let result = value
            .run
            .result
            .as_ref()
            .map(|result| result.status.as_str())
            .unwrap_or("no result");
        match value.run.workflow.status.as_str() {
            "errored" => bail!("PR #{number} CI failed ({result})"),
            "terminated" => bail!("PR #{number} CI was terminated ({result})"),
            "queued" | "running" | "paused" | "waiting" | "unknown" => {
                bail!(
                    "PR #{number} CI is {} ({result}); no successful artifact is ready",
                    value.run.workflow.status
                )
            }
            _ => bail!("the Nanocodex CI service returned an invalid PR #{number} workflow state"),
        }
    }
    let result = value
        .run
        .result
        .as_ref()
        .ok_or_else(|| eyre::eyre!("PR #{number} CI completed without a result"))?;
    if result.version != 1 || result.head != value.lane.merge_head || result.completed_at.is_empty()
    {
        bail!("the Nanocodex CI service returned an invalid current PR #{number} result");
    }
    if result.status != "success" {
        bail!("PR #{number} CI completed with result {}", result.status);
    }
    let native = value
        .native
        .as_ref()
        .ok_or_else(|| eyre::eyre!("PR #{number} CI passed without a native artifact manifest"))?;
    validate_manifest(
        &native.manifest,
        number,
        &value.lane.pull_request_head,
        &value.lane.merge_head,
    )?;
    if native.manifest_sha256 != native.manifest.manifest_sha256
        || native.manifest.completed_at != result.completed_at
        || native.manifest_path != manifest_path(&native.manifest)
    {
        bail!("the current PR #{number} pointer is inconsistent with its manifest");
    }
    Ok(native)
}

fn validate_manifest(
    manifest: &PullRequestManifest,
    number: u64,
    pull_request_head: &str,
    merge_head: &str,
) -> Result<()> {
    if manifest.version != 1
        || manifest.repository != REPOSITORY
        || manifest.pull_request != number
        || manifest.pull_request_head != pull_request_head
        || manifest.merge_head != merge_head
        || manifest.workflow_id != format!("ci-{merge_head}")
        || manifest.completed_at.is_empty()
        || !lower_hex(&manifest.manifest_sha256, 64)
    {
        bail!("the immutable PR #{number} manifest has invalid provenance");
    }
    let expected = [
        ("nanocodex-aarch64-apple-darwin", "aarch64-apple-darwin"),
        (
            "nanocodex-x86_64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
        ),
    ];
    if manifest.artifacts.len() != expected.len() {
        bail!("the immutable PR #{number} manifest has an incomplete native artifact set");
    }
    for (artifact, (name, platform)) in manifest.artifacts.iter().zip(expected) {
        if artifact.name != name
            || artifact.platform != platform
            || artifact.content_type != "application/octet-stream"
            || artifact.size == 0
            || artifact.size > MAX_BINARY_BYTES
            || !lower_hex(&artifact.sha256, 64)
            || artifact.download_path
                != format!(
                    "/api/ci/pull-requests/{number}/builds/{pull_request_head}/{merge_head}/artifacts/{name}"
                )
        {
            bail!("the immutable PR #{number} manifest has an invalid {name} artifact");
        }
    }
    let actual = manifest_sha256(manifest)?;
    if actual != manifest.manifest_sha256 {
        bail!(
            "PR #{number} manifest checksum mismatch: declared {}, calculated {actual}",
            manifest.manifest_sha256
        );
    }
    Ok(())
}

fn manifest_sha256(manifest: &PullRequestManifest) -> Result<String> {
    let canonical = CanonicalManifest {
        artifacts: manifest
            .artifacts
            .iter()
            .map(|artifact| CanonicalArtifact {
                content_type: &artifact.content_type,
                download_path: &artifact.download_path,
                name: &artifact.name,
                platform: &artifact.platform,
                sha256: &artifact.sha256,
                size: artifact.size,
            })
            .collect(),
        completed_at: &manifest.completed_at,
        merge_head: &manifest.merge_head,
        pull_request: manifest.pull_request,
        pull_request_head: &manifest.pull_request_head,
        repository: &manifest.repository,
        version: manifest.version,
        workflow_id: &manifest.workflow_id,
    };
    let bytes = serde_json::to_vec(&canonical).wrap_err("failed to canonicalize PR manifest")?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn manifest_path(manifest: &PullRequestManifest) -> String {
    format!(
        "/api/ci/pull-requests/{}/builds/{}/{}/manifests/{}",
        manifest.pull_request,
        manifest.pull_request_head,
        manifest.merge_head,
        manifest.manifest_sha256,
    )
}

fn public_url(path: &str, label: &str) -> Result<Url> {
    if !path.starts_with('/') || path.contains('?') || path.contains('#') || path.contains('\\') {
        bail!("the {label} path is invalid");
    }
    let origin = Url::parse(PUBLIC_RELEASE_ORIGIN)
        .wrap_err("the built-in Nanocodex release origin is invalid")?;
    let url = origin
        .join(path)
        .wrap_err_with(|| format!("the {label} path is invalid"))?;
    if url.scheme() != origin.scheme()
        || url.host_str() != origin.host_str()
        || url.port_or_known_default() != origin.port_or_known_default()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != path
    {
        bail!("the {label} path escaped the Nanocodex CI origin");
    }
    Ok(url)
}

fn verify_asset(asset: &PullRequestArtifact, contents: &[u8]) -> Result<()> {
    if contents.len() as u64 != asset.size {
        bail!(
            "size mismatch for {}: manifest declared {}, downloaded {}",
            asset.name,
            asset.size,
            contents.len()
        );
    }
    let actual = hex::encode(Sha256::digest(contents));
    if actual != asset.sha256 {
        bail!(
            "checksum mismatch for {}: manifest declared {}, downloaded {actual}",
            asset.name,
            asset.sha256
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const MERGE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn validates_exact_current_manifest_and_platform_set() {
        let response = fixture();
        let native = validate_pull_request(&response, 7).unwrap();
        assert_eq!(native.manifest_path, manifest_path(&native.manifest));
        assert_eq!(
            native.manifest_sha256,
            "aaa3b9fb149a86032e2df83157f58c04ba7ddb7fffa79eea57cbdc2b4f3f13ff"
        );
    }

    #[test]
    fn reports_non_ready_and_failed_runs_before_manifest_validation() {
        let mut pending = fixture();
        pending.run.workflow.status = "running".into();
        pending.run.result = None;
        pending.native = None;
        assert!(
            validate_pull_request(&pending, 7)
                .unwrap_err()
                .to_string()
                .contains("no successful artifact is ready")
        );

        let mut failed = fixture();
        failed.run.workflow.status = "errored".into();
        failed.run.result.as_mut().unwrap().status = "failure".into();
        failed.native = None;
        assert!(
            validate_pull_request(&failed, 7)
                .unwrap_err()
                .to_string()
                .contains("CI failed")
        );
    }

    #[test]
    fn rejects_tampered_identity_hash_asset_and_path() {
        let mut value = fixture();
        value.lane.pull_request_head = "c".repeat(40);
        assert!(validate_pull_request(&value, 7).is_err());

        let mut value = fixture();
        value.native.as_mut().unwrap().manifest.artifacts[0].sha256 = "0".repeat(64);
        assert!(validate_pull_request(&value, 7).is_err());

        let mut value = fixture();
        value.native.as_mut().unwrap().manifest.artifacts[0].download_path =
            "https://evil.test/x".into();
        assert!(validate_pull_request(&value, 7).is_err());

        assert!(public_url("//evil.test/x", "artifact").is_err());
        assert!(public_url("/api/ci/x?token=y", "artifact").is_err());
    }

    #[test]
    fn verifies_downloaded_binary_size_and_checksum() {
        let body = b"native binary";
        let mut native = fixture().native.unwrap();
        let mut asset = native.manifest.artifacts.remove(0);
        asset.size = body.len() as u64;
        asset.sha256 = hex::encode(Sha256::digest(body));
        verify_asset(&asset, body).unwrap();
        assert!(verify_asset(&asset, b"other").is_err());
    }

    fn fixture() -> PullRequestResponse {
        let mut manifest = PullRequestManifest {
            version: 1,
            repository: REPOSITORY.into(),
            pull_request: 7,
            pull_request_head: HEAD.into(),
            merge_head: MERGE.into(),
            workflow_id: format!("ci-{MERGE}"),
            completed_at: "2026-08-22T01:00:00.000Z".into(),
            artifacts: vec![
                artifact("nanocodex-aarch64-apple-darwin", "aarch64-apple-darwin"),
                artifact(
                    "nanocodex-x86_64-unknown-linux-gnu",
                    "x86_64-unknown-linux-gnu",
                ),
            ],
            manifest_sha256: String::new(),
        };
        manifest.manifest_sha256 = manifest_sha256(&manifest).unwrap();
        let manifest_path = manifest_path(&manifest);
        PullRequestResponse {
            version: 1,
            lane: PullRequestLane {
                kind: "pull_request".into(),
                number: 7,
                branch: "pull/7/merge".into(),
                r#ref: "refs/pull/7/merge".into(),
                merge_head: MERGE.into(),
                pull_request_head: HEAD.into(),
            },
            run: PullRequestRun {
                version: 1,
                head: MERGE.into(),
                state: "dispatched".into(),
                published_at: "2026-08-22T00:00:00.000Z".into(),
                workflow: WorkflowState {
                    status: "complete".into(),
                },
                result: Some(PullRequestResult {
                    version: 1,
                    head: MERGE.into(),
                    status: "success".into(),
                    completed_at: manifest.completed_at.clone(),
                }),
            },
            native: Some(NativeProjection {
                manifest_sha256: manifest.manifest_sha256.clone(),
                manifest_path,
                manifest,
            }),
        }
    }

    fn artifact(name: &str, platform: &str) -> PullRequestArtifact {
        PullRequestArtifact {
            name: name.into(),
            platform: platform.into(),
            size: if platform == "aarch64-apple-darwin" {
                12
            } else {
                10
            },
            sha256: if platform == "aarch64-apple-darwin" {
                "66b20fd3073ec3e730ac717d823737ec413e7c1c5c1ab78119bcd71cfa3a87bd".into()
            } else {
                "736e21597f781320cdfb761196a1c92c8ab504c7ff8759d904c38fb0098d959d".into()
            },
            content_type: "application/octet-stream".into(),
            download_path: format!(
                "/api/ci/pull-requests/7/builds/{HEAD}/{MERGE}/artifacts/{name}"
            ),
        }
    }
}
