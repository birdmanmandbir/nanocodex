use std::{
    borrow::Cow,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{Args, ValueHint};
use eyre::{Context, Result, bail, eyre};
use flate2::read::GzDecoder;
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::{Client, Response, StatusCode, Url, header};
use semver::Version;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use crate::version;

mod pr;
mod store;

use store::VersionStore;

const PUBLIC_RELEASE_ORIGIN: &str = "https://nanocodex.me-7fb.workers.dev";
const PUBLIC_RELEASE_API: &str = "https://nanocodex.me-7fb.workers.dev/api/releases";
const STABLE_RELEASE_API: &str =
    "https://nanocodex.me-7fb.workers.dev/api/releases/channels/latest";
const TAGGED_STABLE_RELEASE_API: &str =
    "https://nanocodex.me-7fb.workers.dev/api/releases/releases/stable";
const NIGHTLY_RELEASE_API: &str =
    "https://nanocodex.me-7fb.workers.dev/api/releases/channels/nightly";
const COMMIT_RELEASE_API: &str =
    "https://nanocodex.me-7fb.workers.dev/api/releases/releases/commit";
#[cfg(test)]
const LEGACY_STABLE_WITHOUT_VM_GUEST_TAG: &str = "v0.5.0";
const VM_GUEST_ASSET: &str = "nanocodex-vm-guest-x86_64-unknown-linux-musl";
const VM_GUEST_PLATFORM: &str = "x86_64-unknown-linux-musl";
const DOWNLOAD_ATTEMPTS: usize = 5;
const DOWNLOAD_RETRY_DELAY: Duration = Duration::from_millis(250);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_METADATA_BYTES: usize = 256 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;

pub(crate) fn prepare_legacy_nightly_bootstrap() -> Result<()> {
    if version::IS_NIGHTLY {
        VersionStore::prepare_legacy_nightly_bootstrap()?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum DownloadError {
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    #[error("release service returned HTTP {0}")]
    HttpStatus(StatusCode),
    #[error("release service returned an invalid or oversized error response")]
    InvalidErrorResponse,
    #[error("download was redirected away from its canonical endpoint")]
    UnexpectedRedirect,
    #[error("download exceeds the 256 MiB limit")]
    TooLarge,
}

#[derive(Debug, Args)]
pub(crate) struct Update {
    /// Download or activate an exact release, such as 0.2.0.
    #[arg(
        value_name = "VERSION",
        value_parser = parse_requested_version,
        conflicts_with_all = ["nightly", "pr", "path"]
    )]
    version: Option<Version>,

    /// Download and activate the latest nightly build.
    #[arg(long, conflicts_with_all = ["version", "pr", "path"])]
    nightly: bool,

    /// Download and activate a verified on-demand pull-request artifact.
    #[arg(
        long,
        value_name = "NUMBER",
        value_parser = parse_pr_number,
        conflicts_with_all = ["version", "nightly", "path"]
    )]
    pr: Option<u64>,

    /// Cache and activate a trusted local Nanocodex binary.
    #[arg(
        long,
        value_name = "PATH",
        value_hint = ValueHint::FilePath,
        conflicts_with_all = ["version", "nightly", "pr"]
    )]
    path: Option<PathBuf>,

    /// Reinstall the selected release even when it is already installed.
    #[arg(long, conflicts_with_all = ["pr", "path"])]
    force: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
struct ReleaseManifest {
    version: u8,
    kind: String,
    id: String,
    tag: String,
    commit: String,
    channel: String,
    finalized_at: String,
    manifest_sha256: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
struct ReleaseAsset {
    name: String,
    platform: String,
    size: u64,
    sha256: String,
    content_type: String,
    download_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReleasePointer {
    version: u8,
    channel: String,
    kind: String,
    id: String,
    tag: String,
    commit: String,
    generation: u64,
    updated_at: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseChannel {
    pointer: ReleasePointer,
    manifest: ReleaseManifest,
}

#[derive(Debug, Deserialize)]
struct ReleaseServiceError {
    error: String,
}

// Field declaration order is lexicographic to match ciReleases.ts canonicalJson.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CanonicalManifest<'a> {
    assets: Vec<CanonicalAsset<'a>>,
    channel: &'a str,
    commit: &'a str,
    finalized_at: &'a str,
    id: &'a str,
    kind: &'a str,
    tag: &'a str,
    version: u8,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CanonicalAsset<'a> {
    content_type: &'a str,
    download_path: &'a str,
    name: &'a str,
    platform: &'a str,
    sha256: &'a str,
    size: u64,
}

impl Update {
    pub(crate) async fn run(self) -> Result<()> {
        let manager_version = Version::parse(env!("CARGO_PKG_VERSION"))
            .wrap_err("the installed Nanocodex version is invalid")?;
        VersionStore::promote_running_legacy_nightly_manager()?;
        let store_handle = VersionStore::discover()?;
        let store = store_handle.lock_exclusive()?;
        let manager_key = manager_key(&manager_version);
        store.prepare(
            &manager_key,
            stable_version_requires_vm_guest(&manager_version),
        )?;
        let previous = store.active()?.unwrap_or_else(|| manager_key.clone());

        if let Some(path) = &self.path {
            return install_local_binary(path, &store, &previous);
        }
        if self.pr.is_none()
            && !self.nightly
            && !self.force
            && let Some(requested) = &self.version
        {
            let key = requested.to_string();
            let is_self_bridge =
                running_self_bridge_requires_manifest(&key, &manager_key, requested);
            // The running bridge is accepted only against raw hashes from the
            // immutable manifest, which is not available on this shortcut.
            if !is_self_bridge {
                let cached = if stable_version_requires_vm_guest(requested) {
                    store.is_cached_with_vm_guest(&key)?
                } else {
                    store.is_cached(&key)?
                };
                if cached {
                    if stable_version_requires_vm_guest(requested) {
                        store.activate_with_vm_guest(&key)?;
                    } else {
                        store.activate(&key)?;
                    }
                    maybe_promote_manager(&store, &key, requested, &manager_version)?;
                    report_activation(&previous, &key, false);
                    return Ok(());
                }
            }
        }
        let mut client_builder = Client::builder()
            .user_agent(format!("nanocodex/{}", version::SEMVER_VERSION))
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT);
        if self.pr.is_none() {
            client_builder = client_builder.redirect(reqwest::redirect::Policy::none());
        }
        let client = client_builder
            .build()
            .wrap_err("failed to create the update client")?;
        if let Some(pr) = self.pr {
            return install_pr_binary(&client, pr, &store, &previous).await;
        }

        if self.nightly {
            self.run_nightly(&client, &store, &previous).await
        } else {
            self.run_stable(&client, &store, &previous, &manager_key, &manager_version)
                .await
        }
    }

    async fn run_stable(
        &self,
        client: &Client,
        store: &VersionStore,
        previous: &str,
        manager_key: &str,
        manager_version: &Version,
    ) -> Result<()> {
        let release = fetch_stable_release(client, self.version.as_ref()).await?;
        let selected = validate_stable_release(&release, self.version.as_ref())?;
        let vm_guest = if vm_guest_binary_asset_name().is_some() {
            stable_vm_guest_asset(&release, &selected)?
        } else {
            None
        };
        let key = selected.to_string();
        let binary_name = binary_asset_name()?;
        let (binary, binary_raw, binary_compressed) = find_preferred_release_asset(
            &release,
            binary_name,
            binary_asset_platform(binary_name)?,
        )?;
        let is_self_bridge = vm_guest.is_some() && key == manager_key;
        if !self.force {
            let cached = if let Some((_, vm_guest_raw, _)) = vm_guest
                && is_self_bridge
            {
                store.is_bridge_cached_with_vm_guest(
                    &key,
                    &binary_raw.sha256,
                    &vm_guest_raw.sha256,
                )?
            } else if vm_guest.is_some() {
                store.is_cached_with_vm_guest(&key)?
            } else {
                store.is_cached(&key)?
            };
            if cached {
                activate_stable_version(
                    store,
                    &key,
                    &binary_raw.sha256,
                    vm_guest.map(|(_, raw, _)| raw),
                    is_self_bridge,
                )?;
                maybe_promote_manager(store, &key, &selected, manager_version)?;
                report_activation(previous, &key, false);
                return Ok(());
            }
        }

        let archive = download_release_asset(client, &release, binary, true).await?;
        let contents = unpack_release_asset(archive, binary, binary_raw, binary_compressed)?;
        if let Some((guest, guest_raw, guest_compressed)) = vm_guest {
            let guest_archive = download_release_asset(client, &release, guest, true).await?;
            let guest_contents =
                unpack_release_asset(guest_archive, guest, guest_raw, guest_compressed)?;
            install_stable_with_vm_guest(
                store,
                &key,
                manager_key,
                UnpackedReleaseAsset {
                    contents: &contents,
                    raw_sha256: &binary_raw.sha256,
                },
                UnpackedReleaseAsset {
                    contents: &guest_contents,
                    raw_sha256: &guest_raw.sha256,
                },
                self.force,
            )?;
        } else {
            if self.force {
                store.reinstall(&key, &contents)?;
            } else {
                store.install(&key, &contents)?;
            }
        }
        activate_stable_version(
            store,
            &key,
            &binary_raw.sha256,
            vm_guest.map(|(_, raw, _)| raw),
            is_self_bridge,
        )?;
        maybe_promote_manager(store, &key, &selected, manager_version)?;
        report_activation(previous, &key, true);
        Ok(())
    }

    async fn run_nightly(
        &self,
        client: &Client,
        store: &VersionStore,
        previous: &str,
    ) -> Result<()> {
        let release = fetch_nightly_release(client).await?;
        let key = nightly_key(&release)?;
        let requires_vm_guest = vm_guest_binary_asset_name().is_some();
        if !self.force {
            let cached = if requires_vm_guest {
                store.is_cached_with_vm_guest(&key)?
            } else {
                store.is_cached(&key)?
            };
            if cached {
                if requires_vm_guest {
                    store.activate_with_vm_guest(&key)?;
                } else {
                    store.activate(&key)?;
                }
                store.promote_manager(&key)?;
                report_activation(previous, &key, false);
                return Ok(());
            }
        }

        let binary_name = binary_asset_name()?;
        let (binary, binary_raw, compressed) = find_preferred_release_asset(
            &release,
            binary_name,
            binary_asset_platform(binary_name)?,
        )?;
        let archive = download_release_asset(client, &release, binary, true).await?;
        let contents = unpack_release_asset(archive, binary, binary_raw, compressed)?;
        if let Some(guest_name) = vm_guest_binary_asset_name() {
            let (guest, guest_raw, compressed) =
                find_preferred_release_asset(&release, guest_name, VM_GUEST_PLATFORM)?;
            let guest_archive = download_release_asset(client, &release, guest, true).await?;
            let guest_contents = unpack_release_asset(guest_archive, guest, guest_raw, compressed)?;
            if self.force {
                store.reinstall_with_vm_guest(&key, &contents, &guest_contents)?;
            } else {
                store.install_with_vm_guest(&key, &contents, &guest_contents)?;
            }
        } else {
            if self.force {
                store.reinstall(&key, &contents)?;
            } else {
                store.install(&key, &contents)?;
            }
        }
        if requires_vm_guest {
            store.activate_with_vm_guest(&key)?;
        } else {
            store.activate(&key)?;
        }
        store.promote_manager(&key)?;
        report_activation(previous, &key, true);
        Ok(())
    }
}

fn activate_stable_version(
    store: &VersionStore,
    key: &str,
    binary_sha256: &str,
    vm_guest_raw: Option<&ReleaseAsset>,
    is_self_bridge: bool,
) -> Result<()> {
    if let Some(vm_guest_raw) = vm_guest_raw
        && is_self_bridge
    {
        store.activate_bridge_with_vm_guest(key, binary_sha256, &vm_guest_raw.sha256)
    } else if vm_guest_raw.is_some() {
        store.activate_with_vm_guest(key)
    } else {
        store.activate(key)
    }
}

struct UnpackedReleaseAsset<'a> {
    contents: &'a [u8],
    raw_sha256: &'a str,
}

fn install_stable_with_vm_guest(
    store: &VersionStore,
    key: &str,
    manager_key: &str,
    binary: UnpackedReleaseAsset<'_>,
    vm_guest: UnpackedReleaseAsset<'_>,
    force: bool,
) -> Result<()> {
    if key == manager_key {
        return store.install_bridge_with_vm_guest(
            key,
            binary.contents,
            vm_guest.contents,
            binary.raw_sha256,
            vm_guest.raw_sha256,
        );
    }
    if force {
        store.reinstall_with_vm_guest(key, binary.contents, vm_guest.contents)
    } else {
        store.install_with_vm_guest(key, binary.contents, vm_guest.contents)
    }
}

async fn fetch_release_metadata<T: DeserializeOwned>(
    client: &Client,
    url: &str,
    description: &str,
) -> Result<T> {
    let expected_url = canonical_public_release_url(url)?;
    let response = client
        .get(expected_url.clone())
        .header(header::ACCEPT, "application/json")
        .send()
        .await
        .wrap_err_with(|| format!("failed to query the {description}"))?;
    if response.url() != &expected_url {
        bail!(
            "the Nanocodex release service redirected the {description} away from its canonical endpoint"
        );
    }

    let status = response.status();
    let bytes = read_bounded_release_body(response, description).await?;
    if !status.is_success() {
        let code = serde_json::from_slice::<ReleaseServiceError>(&bytes)
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
            bail!(
                "the Nanocodex release service returned HTTP {status} for the {description}: {code}"
            );
        }
        bail!("the Nanocodex release service returned HTTP {status} for the {description}");
    }

    serde_json::from_slice(&bytes).wrap_err_with(|| {
        format!("the Nanocodex release service returned invalid {description} metadata")
    })
}

async fn read_bounded_release_body(response: Response, description: &str) -> Result<Vec<u8>> {
    let content_length = response.content_length();
    let capacity = bounded_release_body_capacity(content_length, description)?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.wrap_err_with(|| format!("failed to read the {description}"))?;
        append_bounded_release_body(&mut bytes, &chunk, description)?;
    }
    if content_length.is_some_and(|length| length != bytes.len() as u64) {
        bail!(
            "the Nanocodex release service returned an inconsistent content length for the {description}"
        );
    }
    Ok(bytes)
}

fn bounded_release_body_capacity(content_length: Option<u64>, description: &str) -> Result<usize> {
    if content_length.is_some_and(|length| length > MAX_METADATA_BYTES as u64) {
        bail!("the Nanocodex release service returned an oversized {description} response body");
    }
    Ok(content_length.unwrap_or_default() as usize)
}

fn append_bounded_release_body(bytes: &mut Vec<u8>, chunk: &[u8], description: &str) -> Result<()> {
    if chunk.len() > MAX_METADATA_BYTES.saturating_sub(bytes.len()) {
        bail!("the Nanocodex release service returned an oversized {description} response body");
    }
    bytes.extend_from_slice(chunk);
    Ok(())
}

async fn fetch_stable_release(
    client: &Client,
    requested: Option<&Version>,
) -> Result<ReleaseManifest> {
    let url = stable_release_api(requested);
    let description = requested.map_or_else(
        || "latest stable Nanocodex release pointer".to_owned(),
        |version| format!("Nanocodex {version} release"),
    );
    if requested.is_some() {
        fetch_release_metadata(client, url.as_ref(), &description).await
    } else {
        let channel: ReleaseChannel =
            fetch_release_metadata(client, url.as_ref(), &description).await?;
        let selected = validate_stable_channel(&channel)?;

        let immutable_url = stable_release_api(Some(&selected));
        let release: ReleaseManifest = fetch_release_metadata(
            client,
            immutable_url.as_ref(),
            &format!("immutable stable Nanocodex release at v{selected}"),
        )
        .await?;
        validate_stable_release(&release, Some(&selected))?;
        validate_channel_manifest_match(&channel, &release, "stable")?;
        Ok(release)
    }
}

async fn fetch_nightly_release(client: &Client) -> Result<ReleaseManifest> {
    let channel: ReleaseChannel = fetch_release_metadata(
        client,
        NIGHTLY_RELEASE_API,
        "nightly Nanocodex release pointer",
    )
    .await?;
    validate_nightly_channel(&channel)?;

    let commit = channel.pointer.commit.as_str();
    let url = format!("{COMMIT_RELEASE_API}/{commit}");
    let release: ReleaseManifest = fetch_release_metadata(
        client,
        &url,
        &format!("immutable nightly release at {commit}"),
    )
    .await?;
    validate_nightly_release(&release, Some(commit))?;
    validate_channel_manifest_match(&channel, &release, "nightly")?;
    Ok(release)
}

fn stable_release_api(version: Option<&Version>) -> Cow<'static, str> {
    if let Some(version) = version {
        Cow::Owned(format!("{TAGGED_STABLE_RELEASE_API}/v{version}"))
    } else {
        Cow::Borrowed(STABLE_RELEASE_API)
    }
}

fn canonical_public_release_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).wrap_err("a built-in Nanocodex release URL is invalid")?;
    let origin = Url::parse(PUBLIC_RELEASE_ORIGIN)
        .wrap_err("the built-in Nanocodex release origin is invalid")?;
    let api = Url::parse(PUBLIC_RELEASE_API)
        .wrap_err("the built-in Nanocodex release API URL is invalid")?;
    if url.origin() != origin.origin()
        || url.query().is_some()
        || url.fragment().is_some()
        || url
            .path()
            .strip_prefix(api.path())
            .is_none_or(|suffix| !suffix.starts_with('/'))
    {
        bail!("a Nanocodex release URL is outside the canonical public release API");
    }
    Ok(url)
}

fn parse_requested_version(value: &str) -> std::result::Result<Version, String> {
    let version = Version::parse(value.strip_prefix('v').unwrap_or(value))
        .map_err(|_| format!("{value:?} is not a semantic version such as 0.2.0"))?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        return Err(format!(
            "{value:?} is not a stable release version such as 0.2.0"
        ));
    }
    Ok(version)
}

fn parse_pr_number(value: &str) -> std::result::Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
        .ok_or_else(|| "pull-request number must be a positive integer".to_owned())
}

fn manager_key(version_number: &Version) -> String {
    if version::IS_NIGHTLY {
        "nightly".to_owned()
    } else if version::SEMVER_VERSION.contains("-dev+") {
        format!("dev-{}", version::SEMVER_VERSION)
    } else {
        version_number.to_string()
    }
}

fn install_local_binary(path: &Path, store: &VersionStore, previous: &str) -> Result<()> {
    let contents = fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
    let digest = hex::encode(Sha256::digest(&contents));
    let key = format!("local-{}", &digest[..12]);
    store.install(&key, &contents)?;
    store.activate(&key)?;
    println!(
        "installed and activated nanocodex {key} from {} (previously {previous})",
        path.canonicalize()
            .unwrap_or_else(|_| path.to_path_buf())
            .display()
    );
    Ok(())
}

async fn install_pr_binary(
    client: &Client,
    number: u64,
    store: &VersionStore,
    previous: &str,
) -> Result<()> {
    let asset_name = binary_asset_name()?;
    let artifact = pr::download(client, number, asset_name).await?;
    let key = format!(
        "pr-{number}-{}-{}",
        artifact.pull_request_head, artifact.merge_head
    );
    store.install(&key, &artifact.contents)?;
    store.activate(&key)?;
    println!(
        "installed and activated nanocodex PR #{number} head {} tested as merge {} \
         (manifest {}, {}, previously {previous})",
        artifact.pull_request_head, artifact.merge_head, artifact.manifest_sha256, artifact.run_url,
    );
    Ok(())
}

fn maybe_promote_manager(
    store: &VersionStore,
    key: &str,
    selected: &Version,
    manager: &Version,
) -> Result<()> {
    if selected > manager {
        store.promote_manager(key)?;
    }
    Ok(())
}

fn report_activation(previous: &str, selected: &str, downloaded: bool) {
    if previous == selected {
        if downloaded {
            println!("reinstalled nanocodex {selected}");
        } else {
            println!("nanocodex {selected} is already active");
        }
    } else if downloaded {
        println!("installed and activated nanocodex {selected} (previously {previous})");
    } else {
        println!("switched nanocodex {previous} -> {selected}");
    }
}

async fn download_from_url(
    client: &Client,
    url: Url,
    asset_name: &str,
    show_progress: bool,
) -> Result<Vec<u8>> {
    download_from_url_inner(client, url, asset_name, show_progress, false).await
}

async fn download_release_from_url(
    client: &Client,
    url: Url,
    asset_name: &str,
    show_progress: bool,
) -> Result<Vec<u8>> {
    download_from_url_inner(client, url, asset_name, show_progress, true).await
}

async fn download_from_url_inner(
    client: &Client,
    url: Url,
    asset_name: &str,
    show_progress: bool,
    require_exact_url: bool,
) -> Result<Vec<u8>> {
    if show_progress {
        eprintln!("downloading {asset_name}...");
    }
    for attempt in 0..DOWNLOAD_ATTEMPTS {
        let result = download_once(client, url.clone(), show_progress, require_exact_url).await;

        match result {
            Ok(contents) => return Ok(contents),
            Err(error) if attempt + 1 < DOWNLOAD_ATTEMPTS && retryable_download_error(&error) => {
                let delay = DOWNLOAD_RETRY_DELAY.saturating_mul(1 << attempt);
                if show_progress {
                    eprintln!(
                        "download interrupted ({error}); retrying {}/{} in {:.2}s...",
                        attempt + 2,
                        DOWNLOAD_ATTEMPTS,
                        delay.as_secs_f64()
                    );
                }
                tokio::time::sleep(delay).await;
            }
            Err(error) => {
                return Err(error).wrap_err_with(|| {
                    format!(
                        "failed to download {} after {} attempt{}",
                        asset_name,
                        attempt + 1,
                        if attempt == 0 { "" } else { "s" }
                    )
                });
            }
        }
    }

    unreachable!("the download attempt loop always returns")
}

async fn download_once(
    client: &Client,
    url: Url,
    show_progress: bool,
    require_exact_url: bool,
) -> std::result::Result<Vec<u8>, DownloadError> {
    let expected_url = url.clone();
    let response = client
        .get(url)
        .header(header::ACCEPT, "application/octet-stream")
        .send()
        .await?;
    if require_exact_url && response.url() != &expected_url {
        return Err(DownloadError::UnexpectedRedirect);
    }
    if require_exact_url && !response.status().is_success() {
        let status = response.status();
        read_bounded_release_body(response, "release asset error")
            .await
            .map_err(|_| DownloadError::InvalidErrorResponse)?;
        return Err(DownloadError::HttpStatus(status));
    }
    let response = response.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ARCHIVE_BYTES)
    {
        return Err(DownloadError::TooLarge);
    }

    let progress = if show_progress {
        download_progress(response.content_length())
    } else {
        ProgressBar::hidden()
    };
    let mut contents = Vec::with_capacity(
        response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .map(|length| length.min(MAX_ARCHIVE_BYTES as usize))
            .unwrap_or_default(),
    );
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                if contents.len().saturating_add(chunk.len()) > MAX_ARCHIVE_BYTES as usize {
                    progress.finish_and_clear();
                    return Err(DownloadError::TooLarge);
                }
                progress.inc(chunk.len() as u64);
                contents.extend_from_slice(&chunk);
            }
            Err(error) => {
                progress.finish_and_clear();
                return Err(error.into());
            }
        }
    }
    progress.finish_and_clear();
    Ok(contents)
}

fn download_progress(total_size: Option<u64>) -> ProgressBar {
    let progress = total_size.map_or_else(ProgressBar::new_spinner, ProgressBar::new);
    let template = if total_size.is_some() {
        "{spinner:.green} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})"
    } else {
        "{spinner:.green} {bytes} downloaded ({bytes_per_sec})"
    };
    if let Ok(style) = ProgressStyle::with_template(template) {
        progress.set_style(style.progress_chars("#>-"));
    }
    progress
}

async fn download_release_asset(
    client: &Client,
    release: &ReleaseManifest,
    asset: &ReleaseAsset,
    show_progress: bool,
) -> Result<Vec<u8>> {
    let url = release_asset_url(release, asset)?;
    let contents = download_release_from_url(client, url, &asset.name, show_progress).await?;
    verify_release_asset_contents(asset, &contents)?;
    Ok(contents)
}

fn verify_release_asset_contents(asset: &ReleaseAsset, contents: &[u8]) -> Result<()> {
    if contents.len() as u64 != asset.size {
        bail!(
            "size mismatch for {}: release manifest declared {}, downloaded {}",
            asset.name,
            asset.size,
            contents.len()
        );
    }
    let actual = hex::encode(Sha256::digest(contents));
    if actual != asset.sha256 {
        bail!(
            "checksum mismatch for {}: release manifest declared {}, downloaded {actual}",
            asset.name,
            asset.sha256
        );
    }
    Ok(())
}

fn retryable_download_error(error: &DownloadError) -> bool {
    match error {
        DownloadError::Request(error) => error.status().is_none_or(retryable_download_status),
        DownloadError::HttpStatus(status) => retryable_download_status(*status),
        DownloadError::InvalidErrorResponse
        | DownloadError::UnexpectedRedirect
        | DownloadError::TooLarge => false,
    }
}

fn retryable_download_status(status: StatusCode) -> bool {
    status.is_server_error()
        || matches!(
            status,
            StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
        )
}

fn parse_release_version(tag: &str) -> Result<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag))
        .wrap_err_with(|| format!("release tag {tag:?} is not a semantic version"))
}

fn validate_stable_release(
    release: &ReleaseManifest,
    requested: Option<&Version>,
) -> Result<Version> {
    if release.version != 1
        || release.kind != "stable"
        || release.channel != "latest"
        || release.id != release.tag
    {
        bail!("the Nanocodex release service returned inconsistent stable release metadata");
    }
    validate_release_manifest(release)?;

    let selected = parse_release_version(&release.tag)?;
    let canonical_tag = format!("v{}.{}.{}", selected.major, selected.minor, selected.patch);
    if release.tag != canonical_tag || !selected.pre.is_empty() || !selected.build.is_empty() {
        bail!(
            "stable release tag {:?} is not a canonical vMAJOR.MINOR.PATCH version",
            release.tag
        );
    }
    if let Some(requested) = requested
        && requested != &selected
    {
        bail!(
            "the Nanocodex release service returned {} for requested version {requested}",
            release.tag
        );
    }
    stable_vm_guest_asset(release, &selected)?;
    Ok(selected)
}

fn validate_nightly_release(release: &ReleaseManifest, commit: Option<&str>) -> Result<()> {
    if release.version != 1
        || release.kind != "commit"
        || release.channel != "nightly"
        || release.id != release.commit
        || release.tag != format!("nightly-{}", release.commit)
        || commit.is_some_and(|commit| commit != release.commit.as_str())
    {
        bail!("the Nanocodex release service returned inconsistent nightly release metadata");
    }
    validate_release_manifest(release)
}

fn validate_stable_channel(channel: &ReleaseChannel) -> Result<Version> {
    let selected = validate_stable_release(&channel.manifest, None)?;
    validate_release_pointer(channel, "latest", "stable")?;
    Ok(selected)
}

fn validate_nightly_channel(channel: &ReleaseChannel) -> Result<()> {
    validate_nightly_release(&channel.manifest, None)?;
    validate_release_pointer(channel, "nightly", "commit")
}

fn validate_channel_manifest_match(
    channel: &ReleaseChannel,
    immutable: &ReleaseManifest,
    channel_name: &str,
) -> Result<()> {
    if &channel.manifest != immutable {
        bail!("the {channel_name} release pointer does not match its immutable release manifest");
    }
    Ok(())
}

fn validate_release_pointer(
    channel: &ReleaseChannel,
    expected_channel: &str,
    expected_kind: &str,
) -> Result<()> {
    let pointer = &channel.pointer;
    let manifest = &channel.manifest;
    if pointer.version != 1
        || pointer.channel != expected_channel
        || pointer.kind != expected_kind
        || pointer.generation == 0
        || pointer.updated_at.is_empty()
        || pointer.id != manifest.id
        || pointer.tag != manifest.tag
        || pointer.commit != manifest.commit
        || pointer.channel != manifest.channel
        || pointer.kind != manifest.kind
    {
        bail!("the Nanocodex release service returned an inconsistent {expected_channel} pointer");
    }
    Ok(())
}

fn validate_release_manifest(release: &ReleaseManifest) -> Result<()> {
    if release.version != 1
        || !lower_hex(&release.commit, 40)
        || release.finalized_at.is_empty()
        || release.assets.is_empty()
        || release.assets.len() > 64
        || !lower_hex(&release.manifest_sha256, 64)
    {
        bail!(
            "release {} contains invalid public manifest metadata",
            release.tag
        );
    }
    for (index, asset) in release.assets.iter().enumerate() {
        if !valid_release_asset_name(&asset.name)
            || asset.size == 0
            || asset.size > MAX_ARCHIVE_BYTES
            || !lower_hex(&asset.sha256, 64)
            || asset.content_type.is_empty()
            || release.assets[..index]
                .iter()
                .any(|seen| seen.name == asset.name)
        {
            bail!(
                "release {} contains invalid metadata for {}",
                release.tag,
                asset.name
            );
        }
        validate_release_asset_path(release, asset)?;
    }

    let actual = release_manifest_sha256(release)?;
    if actual != release.manifest_sha256 {
        bail!(
            "manifest checksum mismatch for release {}: declared {}, calculated {actual}",
            release.tag,
            release.manifest_sha256
        );
    }
    Ok(())
}

fn release_manifest_sha256(release: &ReleaseManifest) -> Result<String> {
    let assets = release
        .assets
        .iter()
        .map(|asset| CanonicalAsset {
            content_type: &asset.content_type,
            download_path: &asset.download_path,
            name: &asset.name,
            platform: &asset.platform,
            sha256: &asset.sha256,
            size: asset.size,
        })
        .collect();
    let canonical = CanonicalManifest {
        assets,
        channel: &release.channel,
        commit: &release.commit,
        finalized_at: &release.finalized_at,
        id: &release.id,
        kind: &release.kind,
        tag: &release.tag,
        version: release.version,
    };
    let bytes = serde_json::to_vec(&canonical)
        .wrap_err_with(|| format!("failed to canonicalize release {}", release.tag))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_release_asset_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= 160
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn stable_vm_guest_asset<'a>(
    release: &'a ReleaseManifest,
    version: &Version,
) -> Result<Option<(&'a ReleaseAsset, &'a ReleaseAsset, bool)>> {
    let compressed_name = format!("{VM_GUEST_ASSET}.gz");
    let mut raw_assets = release
        .assets
        .iter()
        .filter(|asset| asset.name == VM_GUEST_ASSET);
    let raw = raw_assets.next();
    let duplicate_raw = raw.is_some() && raw_assets.next().is_some();
    let mut compressed_assets = release
        .assets
        .iter()
        .filter(|asset| asset.name == compressed_name);
    let compressed = compressed_assets.next();
    let duplicate_compressed = compressed.is_some() && compressed_assets.next().is_some();
    let guest_shaped_assets = release
        .assets
        .iter()
        .filter(|asset| {
            asset.name == VM_GUEST_ASSET
                || asset.name == compressed_name
                || asset.platform == VM_GUEST_PLATFORM
        })
        .count();

    if raw.is_none() && compressed.is_none() && guest_shaped_assets == 0 {
        if stable_version_omits_vm_guest(version) {
            return Ok(None);
        }
        bail!(
            "stable release {} does not contain the required Linux VM guest assets",
            release.tag
        );
    }
    let (Some(raw), Some(compressed)) = (raw, compressed) else {
        bail!(
            "stable release {} contains a partial or mislabeled Linux VM guest asset pair",
            release.tag
        );
    };
    if duplicate_raw || duplicate_compressed || guest_shaped_assets != 2 {
        bail!(
            "stable release {} contains duplicate or mislabeled Linux VM guest assets",
            release.tag
        );
    }
    for (asset, content_type) in [
        (raw, "application/octet-stream"),
        (compressed, "application/gzip"),
    ] {
        if asset.platform != VM_GUEST_PLATFORM
            || asset.content_type != content_type
            || asset.size == 0
            || asset.size > MAX_ARCHIVE_BYTES
            || !lower_hex(&asset.sha256, 64)
        {
            bail!(
                "stable release {} contains mislabeled metadata for {}",
                release.tag,
                asset.name
            );
        }
        validate_release_asset_path(release, asset)?;
    }
    Ok(Some((compressed, raw, true)))
}

fn nightly_key(release: &ReleaseManifest) -> Result<String> {
    validate_nightly_release(release, None)?;
    Ok(format!("nightly-{}", release.commit))
}

fn find_preferred_release_asset<'a>(
    release: &'a ReleaseManifest,
    binary_name: &str,
    expected_platform: &str,
) -> Result<(&'a ReleaseAsset, &'a ReleaseAsset, bool)> {
    let raw = required_raw_release_asset(release, binary_name, expected_platform)?;
    let compressed_name = format!("{binary_name}.gz");
    let mut matching = release
        .assets
        .iter()
        .filter(|asset| asset.name == compressed_name);
    let compressed = matching.next();
    if matching.next().is_some() {
        bail!(
            "release {} contains duplicate {compressed_name} assets",
            release.tag
        );
    }
    let (asset, is_compressed) = compressed.map_or((raw, false), |asset| (asset, true));
    if asset.platform != expected_platform {
        bail!(
            "release {} labels {} as platform {}",
            release.tag,
            asset.name,
            asset.platform
        );
    }
    if asset.size == 0 || asset.size > MAX_ARCHIVE_BYTES {
        bail!(
            "release {} declares an invalid size for {}",
            release.tag,
            asset.name
        );
    }
    if !lower_hex(&asset.sha256, 64) {
        bail!(
            "release {} declares an invalid checksum for {}",
            release.tag,
            asset.name
        );
    }
    let expected_content_type = if is_compressed {
        "application/gzip"
    } else {
        "application/octet-stream"
    };
    if asset.content_type != expected_content_type {
        bail!(
            "release {} labels {} as content type {}",
            release.tag,
            asset.name,
            asset.content_type
        );
    }
    release_asset_url(release, asset)?;
    Ok((asset, raw, is_compressed))
}

fn required_raw_release_asset<'a>(
    release: &'a ReleaseManifest,
    binary_name: &str,
    expected_platform: &str,
) -> Result<&'a ReleaseAsset> {
    let mut matching = release
        .assets
        .iter()
        .filter(|asset| asset.name == binary_name);
    let raw = matching.next().ok_or_else(|| {
        eyre!(
            "release {} does not contain raw asset {binary_name}; see {PUBLIC_RELEASE_API}/releases/{}/{}",
            release.tag,
            release.kind,
            release.id
        )
    })?;
    if matching.next().is_some() {
        bail!(
            "release {} contains duplicate {binary_name} assets",
            release.tag
        );
    }
    if raw.platform != expected_platform
        || raw.content_type != "application/octet-stream"
        || raw.size == 0
        || raw.size > MAX_BINARY_BYTES
        || !lower_hex(&raw.sha256, 64)
    {
        bail!(
            "release {} contains invalid raw metadata for {binary_name}",
            release.tag
        );
    }
    release_asset_url(release, raw)?;
    Ok(raw)
}

fn validate_release_asset_path(release: &ReleaseManifest, asset: &ReleaseAsset) -> Result<()> {
    let expected_path = format!(
        "/api/releases/releases/{}/{}/assets/{}",
        release.kind, release.id, asset.name
    );
    if asset.download_path != expected_path {
        bail!(
            "release {} returned an invalid download path for {}",
            release.tag,
            asset.name
        );
    }
    Ok(())
}

fn release_asset_url(release: &ReleaseManifest, asset: &ReleaseAsset) -> Result<Url> {
    validate_release_asset_path(release, asset)?;
    let origin = Url::parse(PUBLIC_RELEASE_ORIGIN)
        .wrap_err("the built-in Nanocodex release origin is invalid")?
        .join(&asset.download_path)
        .wrap_err_with(|| format!("release {} returned an invalid asset URL", release.tag))?;
    let url = canonical_public_release_url(origin.as_str())?;
    if url.path() != asset.download_path {
        bail!(
            "release {} returned a non-canonical asset URL for {}",
            release.tag,
            asset.name
        );
    }
    Ok(url)
}

fn binary_asset_name() -> Result<&'static str> {
    binary_asset_name_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn binary_asset_platform(binary_name: &str) -> Result<&str> {
    binary_name
        .strip_prefix("nanocodex-")
        .ok_or_else(|| eyre!("invalid Nanocodex binary asset name {binary_name:?}"))
}

fn vm_guest_binary_asset_name() -> Option<&'static str> {
    vm_guest_binary_asset_name_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn stable_version_requires_vm_guest(version: &Version) -> bool {
    stable_version_requires_vm_guest_for(std::env::consts::OS, std::env::consts::ARCH, version)
}

fn stable_version_omits_vm_guest(version: &Version) -> bool {
    version == &Version::new(0, 5, 0)
}

fn running_self_bridge_requires_manifest(key: &str, manager_key: &str, version: &Version) -> bool {
    running_self_bridge_requires_manifest_for(
        key,
        manager_key,
        std::env::consts::OS,
        std::env::consts::ARCH,
        version,
    )
}

fn running_self_bridge_requires_manifest_for(
    key: &str,
    manager_key: &str,
    os: &str,
    arch: &str,
    version: &Version,
) -> bool {
    key == manager_key && stable_version_requires_vm_guest_for(os, arch, version)
}

fn stable_version_requires_vm_guest_for(os: &str, arch: &str, version: &Version) -> bool {
    vm_guest_binary_asset_name_for(os, arch).is_some() && !stable_version_omits_vm_guest(version)
}

fn vm_guest_binary_asset_name_for(os: &str, arch: &str) -> Option<&'static str> {
    matches!((os, arch), ("linux", "x86_64")).then_some(VM_GUEST_ASSET)
}

fn binary_asset_name_for(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Ok("nanocodex-x86_64-unknown-linux-gnu"),
        ("macos", "aarch64") => Ok("nanocodex-aarch64-apple-darwin"),
        _ => Err(eyre!("self-update is not supported on {os} {arch}")),
    }
}

fn decompress_release_asset(archive: &[u8], asset_name: &str) -> Result<Vec<u8>> {
    let mut contents = Vec::new();
    GzDecoder::new(archive)
        .take(MAX_BINARY_BYTES + 1)
        .read_to_end(&mut contents)
        .wrap_err_with(|| format!("failed to decompress {asset_name}"))?;
    if contents.len() as u64 > MAX_BINARY_BYTES {
        bail!("decompressed {asset_name} exceeds the 256 MiB limit");
    }
    Ok(contents)
}

fn unpack_release_asset(
    archive: Vec<u8>,
    transfer: &ReleaseAsset,
    raw: &ReleaseAsset,
    compressed: bool,
) -> Result<Vec<u8>> {
    let contents = if compressed {
        decompress_release_asset(&archive, &transfer.name)?
    } else if archive.len() as u64 > MAX_BINARY_BYTES {
        bail!("{} exceeds the 256 MiB limit", transfer.name);
    } else {
        archive
    };
    verify_release_asset_contents(raw, &contents).wrap_err_with(|| {
        format!(
            "decompressed {} does not match raw manifest asset {}",
            transfer.name, raw.name
        )
    })?;
    Ok(contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Parser, Subcommand};
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: TestCommand,
    }

    #[derive(Subcommand)]
    enum TestCommand {
        Update(Update),
    }

    #[test]
    fn accepts_prefixed_and_plain_release_versions() {
        assert_eq!(
            parse_release_version("v1.2.3").unwrap(),
            Version::new(1, 2, 3)
        );
        assert_eq!(
            parse_release_version("1.2.3").unwrap(),
            Version::new(1, 2, 3)
        );
        assert!(parse_release_version("latest").is_err());
    }

    #[test]
    fn selects_cloudflare_stable_and_nightly_release_endpoints() {
        assert_eq!(stable_release_api(None), STABLE_RELEASE_API);
        assert_eq!(
            stable_release_api(Some(&Version::new(0, 2, 0))),
            format!("{TAGGED_STABLE_RELEASE_API}/v0.2.0")
        );
        assert!(STABLE_RELEASE_API.starts_with(PUBLIC_RELEASE_API));
        assert!(NIGHTLY_RELEASE_API.starts_with(PUBLIC_RELEASE_API));
        assert!(COMMIT_RELEASE_API.starts_with(PUBLIC_RELEASE_API));
    }

    #[test]
    fn bounds_streamed_release_metadata_chunks() {
        assert_eq!(
            bounded_release_body_capacity(Some(MAX_METADATA_BYTES as u64), "test release").unwrap(),
            MAX_METADATA_BYTES
        );
        assert!(
            bounded_release_body_capacity(Some(MAX_METADATA_BYTES as u64 + 1), "test release")
                .is_err()
        );

        let mut bytes = Vec::new();
        append_bounded_release_body(&mut bytes, &vec![b'x'; MAX_METADATA_BYTES], "test release")
            .unwrap();
        assert_eq!(bytes.len(), MAX_METADATA_BYTES);
        assert!(append_bounded_release_body(&mut bytes, b"x", "test release").is_err());
        assert_eq!(bytes.len(), MAX_METADATA_BYTES);

        let mut bytes = Vec::new();
        assert!(
            append_bounded_release_body(
                &mut bytes,
                &vec![b'x'; MAX_METADATA_BYTES + 1],
                "test release",
            )
            .is_err()
        );
        assert!(bytes.is_empty());
    }

    #[test]
    fn validates_cloudflare_stable_manifest_assets_and_checksums() {
        let archive = b"gzip archive";
        let release = stable_release(archive);
        assert_eq!(
            validate_stable_release(&release, Some(&Version::new(0, 5, 0))).unwrap(),
            Version::new(0, 5, 0)
        );
        validate_stable_channel(&release_channel(release.clone())).unwrap();

        let (asset, raw, compressed) = find_preferred_release_asset(
            &release,
            "nanocodex-x86_64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
        )
        .unwrap();
        assert!(compressed);
        assert_eq!(raw.name, "nanocodex-x86_64-unknown-linux-gnu");
        assert_eq!(
            release_asset_url(&release, asset).unwrap().as_str(),
            "https://nanocodex.me-7fb.workers.dev/api/releases/releases/stable/v0.5.0/assets/nanocodex-x86_64-unknown-linux-gnu.gz"
        );
        verify_release_asset_contents(asset, archive).unwrap();
        assert!(verify_release_asset_contents(asset, b"gzip archivf").is_err());
        assert!(verify_release_asset_contents(asset, b"short").is_err());
    }

    #[test]
    fn rejects_inconsistent_cloudflare_stable_metadata() {
        let mut release = stable_release(b"gzip archive");
        assert!(validate_stable_release(&release, Some(&Version::new(0, 6, 0))).is_err());

        release.commit = "A".repeat(40);
        assert!(validate_stable_release(&release, None).is_err());
        release.commit = "a".repeat(40);
        release.assets[0].download_path = "https://attacker.invalid/nanocodex.gz".to_owned();
        assert!(
            find_preferred_release_asset(
                &release,
                "nanocodex-x86_64-unknown-linux-gnu",
                "x86_64-unknown-linux-gnu",
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_stable_pointer_and_immutable_manifest_mismatch() {
        let release = stable_release(b"gzip archive");
        let channel = release_channel(release.clone());
        let mut immutable = release;
        immutable.finalized_at = "2026-08-22T00:00:01.000Z".to_owned();
        immutable = sign_release(immutable);

        validate_stable_release(&immutable, Some(&Version::new(0, 5, 0))).unwrap();
        assert!(validate_channel_manifest_match(&channel, &immutable, "stable").is_err());
    }

    #[test]
    fn requires_complete_canonical_stable_vm_guest_pair_after_legacy_release() {
        let release = stable_release_with_vm_guest(
            b"stable cli",
            b"stable guest",
            b"compressed stable guest",
        );
        validate_stable_release(&release, Some(&Version::new(0, 6, 0))).unwrap();
        let (guest, guest_raw, compressed) =
            stable_vm_guest_asset(&release, &Version::new(0, 6, 0))
                .unwrap()
                .unwrap();
        assert!(compressed);
        assert_eq!(guest.name, format!("{VM_GUEST_ASSET}.gz"));
        assert_eq!(guest_raw.name, VM_GUEST_ASSET);

        let mut missing = release.clone();
        missing
            .assets
            .retain(|asset| asset.name != format!("{VM_GUEST_ASSET}.gz"));
        missing = sign_release(missing);
        assert!(validate_stable_release(&missing, None).is_err());

        let mut mislabeled = release.clone();
        mislabeled
            .assets
            .iter_mut()
            .find(|asset| asset.name == VM_GUEST_ASSET)
            .unwrap()
            .platform = "linux".to_owned();
        mislabeled = sign_release(mislabeled);
        assert!(validate_stable_release(&mislabeled, None).is_err());

        let mut wrong_content_type = release.clone();
        wrong_content_type
            .assets
            .iter_mut()
            .find(|asset| asset.name == format!("{VM_GUEST_ASSET}.gz"))
            .unwrap()
            .content_type = "application/octet-stream".to_owned();
        wrong_content_type = sign_release(wrong_content_type);
        assert!(validate_stable_release(&wrong_content_type, None).is_err());

        let mut duplicate = release;
        let raw = duplicate
            .assets
            .iter()
            .find(|asset| asset.name == VM_GUEST_ASSET)
            .unwrap()
            .clone();
        duplicate.assets.push(raw);
        duplicate = sign_release(duplicate);
        assert!(validate_stable_release(&duplicate, None).is_err());
    }

    #[test]
    fn limits_legacy_cli_adoption_to_the_running_bridge_release() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        let bridge = Version::new(0, 6, 0);
        let bridge_key = bridge.to_string();
        store.install(&bridge_key, b"bridge cli").unwrap();

        install_stable_with_vm_guest(
            &store,
            &bridge_key,
            &bridge_key,
            UnpackedReleaseAsset {
                contents: b"bridge cli",
                raw_sha256: &hex::encode(Sha256::digest(b"bridge cli")),
            },
            UnpackedReleaseAsset {
                contents: b"bridge guest",
                raw_sha256: &hex::encode(Sha256::digest(b"bridge guest")),
            },
            false,
        )
        .unwrap();
        assert!(store.is_cached_with_vm_guest(&bridge_key).unwrap());

        let later = Version::new(0, 7, 0);
        let later_key = later.to_string();
        store.install(&later_key, b"later cli").unwrap();
        let error = install_stable_with_vm_guest(
            &store,
            &later_key,
            &bridge_key,
            UnpackedReleaseAsset {
                contents: b"later cli",
                raw_sha256: &hex::encode(Sha256::digest(b"later cli")),
            },
            UnpackedReleaseAsset {
                contents: b"later guest",
                raw_sha256: &hex::encode(Sha256::digest(b"later guest")),
            },
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot coherently replace"));
        assert!(!store.is_cached_with_vm_guest(&later_key).unwrap());
    }

    #[test]
    fn allows_only_the_imported_legacy_stable_manifest_to_omit_vm_guest() {
        let legacy = stable_release(b"gzip archive");
        assert!(
            stable_vm_guest_asset(&legacy, &Version::new(0, 5, 0))
                .unwrap()
                .is_none()
        );
        assert!(stable_version_omits_vm_guest(&Version::new(0, 5, 0)));
        assert!(!stable_version_omits_vm_guest(&Version::new(0, 6, 0)));

        let mut partial_legacy = legacy;
        partial_legacy.assets.push(release_asset(
            "stable",
            LEGACY_STABLE_WITHOUT_VM_GUEST_TAG,
            VM_GUEST_ASSET,
            VM_GUEST_PLATFORM,
            b"legacy guest",
        ));
        partial_legacy = sign_release(partial_legacy);
        assert!(validate_stable_release(&partial_legacy, None).is_err());

        assert!(stable_version_requires_vm_guest_for(
            "linux",
            "x86_64",
            &Version::new(0, 6, 0)
        ));
        assert!(!stable_version_requires_vm_guest_for(
            "linux",
            "x86_64",
            &Version::new(0, 5, 0)
        ));
        assert!(!stable_version_requires_vm_guest_for(
            "macos",
            "aarch64",
            &Version::new(0, 6, 0)
        ));
        assert!(running_self_bridge_requires_manifest_for(
            "0.6.0",
            "0.6.0",
            "linux",
            "x86_64",
            &Version::new(0, 6, 0),
        ));
        assert!(!running_self_bridge_requires_manifest_for(
            "0.7.0",
            "0.6.0",
            "linux",
            "x86_64",
            &Version::new(0, 7, 0),
        ));
    }

    #[test]
    fn accepts_raw_macos_stable_assets() {
        let contents = b"macOS binary";
        let name = "nanocodex-aarch64-apple-darwin";
        let mut release = stable_release(contents);
        release.assets = vec![ReleaseAsset {
            name: name.to_owned(),
            platform: "aarch64-apple-darwin".to_owned(),
            size: contents.len() as u64,
            sha256: hex::encode(Sha256::digest(contents)),
            content_type: "application/octet-stream".to_owned(),
            download_path: format!("/api/releases/releases/stable/v0.5.0/assets/{name}"),
        }];
        release.manifest_sha256 = release_manifest_sha256(&release).unwrap();

        validate_stable_release(&release, None).unwrap();
        let (asset, raw, compressed) =
            find_preferred_release_asset(&release, name, "aarch64-apple-darwin").unwrap();
        assert!(!compressed);
        assert_eq!(asset, raw);
        verify_release_asset_contents(asset, contents).unwrap();
    }

    #[test]
    fn supports_only_linux_x86_64_and_apple_silicon_binaries() {
        assert_eq!(
            binary_asset_name_for("linux", "x86_64").unwrap(),
            "nanocodex-x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            binary_asset_name_for("macos", "aarch64").unwrap(),
            "nanocodex-aarch64-apple-darwin"
        );
        assert!(binary_asset_name_for("linux", "aarch64").is_err());
        assert!(binary_asset_name_for("macos", "x86_64").is_err());
        assert!(binary_asset_name_for("windows", "x86_64").is_err());
    }

    #[test]
    fn parses_exact_pr_and_local_update_sources() {
        let TestCommand::Update(exact) = TestCli::try_parse_from(["nanocodex", "update", "v0.2.0"])
            .unwrap()
            .command;
        assert_eq!(exact.version, Some(Version::new(0, 2, 0)));

        let TestCommand::Update(pr) =
            TestCli::try_parse_from(["nanocodex", "update", "--pr", "50"])
                .unwrap()
                .command;
        assert_eq!(pr.pr, Some(50));

        let TestCommand::Update(path) =
            TestCli::try_parse_from(["nanocodex", "update", "--path", "/tmp/nanocodex"])
                .unwrap()
                .command;
        assert_eq!(path.path, Some(PathBuf::from("/tmp/nanocodex")));
    }

    #[test]
    fn rejects_conflicting_and_invalid_update_sources() {
        assert!(TestCli::try_parse_from(["nanocodex", "update", "0.2.0", "--nightly"]).is_err());
        assert!(TestCli::try_parse_from(["nanocodex", "update", "--pr", "0"]).is_err());
        assert!(TestCli::try_parse_from(["nanocodex", "update", "not-a-version"]).is_err());
        assert!(TestCli::try_parse_from(["nanocodex", "update", "0.2.0-rc.1"]).is_err());
        assert!(TestCli::try_parse_from(["nanocodex", "update", "0.2.0+build"]).is_err());
    }

    #[test]
    fn decompresses_release_assets() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(b"nanocodex binary").unwrap();
        let archive = encoder.finish().unwrap();

        assert_eq!(
            decompress_release_asset(&archive, "nanocodex-test.gz").unwrap(),
            b"nanocodex binary"
        );
        assert!(decompress_release_asset(b"not gzip", "nanocodex-test.gz").is_err());
    }

    #[test]
    fn rejects_compressed_bytes_that_do_not_match_the_raw_manifest_asset() {
        let raw_contents = b"manifest raw binary";
        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(b"different decompressed binary").unwrap();
        let archive = encoder.finish().unwrap();
        let name = "nanocodex-x86_64-unknown-linux-gnu";
        let mut release = stable_release(&archive);
        release.assets = vec![
            release_asset(
                "stable",
                LEGACY_STABLE_WITHOUT_VM_GUEST_TAG,
                name,
                "x86_64-unknown-linux-gnu",
                raw_contents,
            ),
            release_asset(
                "stable",
                LEGACY_STABLE_WITHOUT_VM_GUEST_TAG,
                &format!("{name}.gz"),
                "x86_64-unknown-linux-gnu",
                &archive,
            ),
        ];
        release = sign_release(release);

        validate_stable_release(&release, None).unwrap();
        let (transfer, raw, compressed) =
            find_preferred_release_asset(&release, name, "x86_64-unknown-linux-gnu").unwrap();
        verify_release_asset_contents(transfer, &archive).unwrap();
        let error = unpack_release_asset(archive, transfer, raw, compressed).unwrap_err();
        assert!(error.to_string().contains("raw manifest asset"));
    }

    #[test]
    fn verifies_the_canonical_public_manifest_digest() {
        let release = stable_release(b"gzip archive");
        assert_eq!(
            release.manifest_sha256,
            "1e9b1e1ed34d6b26f76e959f9a0eca105b7099aef5f6afe258fd2f31a2d5d4d5"
        );
        assert_eq!(
            release_manifest_sha256(&release).unwrap(),
            release.manifest_sha256
        );

        let mut tampered = release.clone();
        tampered.assets[0].size += 1;
        assert_ne!(
            release_manifest_sha256(&tampered).unwrap(),
            tampered.manifest_sha256
        );
        assert!(validate_stable_release(&tampered, None).is_err());
    }

    #[test]
    fn retries_only_transient_download_statuses() {
        assert!(retryable_download_status(StatusCode::REQUEST_TIMEOUT));
        assert!(retryable_download_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(retryable_download_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!retryable_download_status(StatusCode::NOT_FOUND));
    }

    #[test]
    fn validates_nightly_pointer_cli_and_vm_guest_assets() {
        let release = nightly_release(b"nightly cli", b"nightly guest");
        assert_eq!(
            release.manifest_sha256,
            "b87872fd8c24d5588f55ea4ee8cea75df09d68e07837752dbab596edb35ce7d0"
        );
        validate_nightly_release(&release, Some(&release.commit)).unwrap();
        assert_eq!(
            nightly_key(&release).unwrap(),
            format!("nightly-{}", "c".repeat(40))
        );

        let channel = release_channel(release.clone());
        validate_nightly_channel(&channel).unwrap();
        assert_eq!(channel.pointer.id, release.commit);

        let cli_name = "nanocodex-x86_64-unknown-linux-gnu";
        let (cli, cli_raw, cli_compressed) =
            find_preferred_release_asset(&release, cli_name, "x86_64-unknown-linux-gnu").unwrap();
        assert!(cli_compressed);
        assert_eq!(cli_raw.name, cli_name);
        verify_release_asset_contents(cli, b"nightly cli").unwrap();
        assert_eq!(
            release_asset_url(&release, cli).unwrap().as_str(),
            format!(
                "https://nanocodex.me-7fb.workers.dev/api/releases/releases/commit/{}/assets/{cli_name}.gz",
                release.commit
            )
        );

        let (guest, guest_raw, guest_compressed) =
            find_preferred_release_asset(&release, VM_GUEST_ASSET, VM_GUEST_PLATFORM).unwrap();
        assert!(guest_compressed);
        assert_eq!(guest_raw.name, VM_GUEST_ASSET);
        verify_release_asset_contents(guest, b"nightly guest").unwrap();
        assert_eq!(
            vm_guest_binary_asset_name_for("linux", "x86_64"),
            Some(VM_GUEST_ASSET)
        );
        assert_eq!(vm_guest_binary_asset_name_for("macos", "aarch64"), None);
    }

    #[test]
    fn rejects_misdirected_nightly_manifests_and_pointers() {
        let release = nightly_release(b"nightly cli", b"nightly guest");
        assert!(validate_nightly_release(&release, Some(&"d".repeat(40))).is_err());

        let mut wrong_tag = release.clone();
        wrong_tag.tag = "nightly-other".to_owned();
        wrong_tag.manifest_sha256 = release_manifest_sha256(&wrong_tag).unwrap();
        assert!(validate_nightly_release(&wrong_tag, None).is_err());

        let mut wrong_path = release.clone();
        wrong_path.assets[0].download_path = "/api/releases/channels/nightly/asset".to_owned();
        wrong_path.manifest_sha256 = release_manifest_sha256(&wrong_path).unwrap();
        assert!(validate_nightly_release(&wrong_path, None).is_err());

        let mut channel = release_channel(release);
        channel.pointer.commit = "d".repeat(40);
        assert!(validate_nightly_channel(&channel).is_err());
    }

    fn stable_release(contents: &[u8]) -> ReleaseManifest {
        let name = "nanocodex-x86_64-unknown-linux-gnu.gz";
        sign_release(ReleaseManifest {
            version: 1,
            kind: "stable".to_owned(),
            id: "v0.5.0".to_owned(),
            tag: "v0.5.0".to_owned(),
            commit: "a".repeat(40),
            channel: "latest".to_owned(),
            finalized_at: "2026-08-22T00:00:00.000Z".to_owned(),
            manifest_sha256: String::new(),
            assets: vec![
                release_asset(
                    "stable",
                    "v0.5.0",
                    "nanocodex-x86_64-unknown-linux-gnu",
                    "x86_64-unknown-linux-gnu",
                    b"raw binary",
                ),
                release_asset(
                    "stable",
                    "v0.5.0",
                    name,
                    "x86_64-unknown-linux-gnu",
                    contents,
                ),
            ],
        })
    }

    fn stable_release_with_vm_guest(
        cli: &[u8],
        guest: &[u8],
        compressed_guest: &[u8],
    ) -> ReleaseManifest {
        let tag = "v0.6.0";
        sign_release(ReleaseManifest {
            version: 1,
            kind: "stable".to_owned(),
            id: tag.to_owned(),
            tag: tag.to_owned(),
            commit: "b".repeat(40),
            channel: "latest".to_owned(),
            finalized_at: "2026-08-22T00:00:00.000Z".to_owned(),
            manifest_sha256: String::new(),
            assets: vec![
                release_asset("stable", tag, VM_GUEST_ASSET, VM_GUEST_PLATFORM, guest),
                release_asset(
                    "stable",
                    tag,
                    &format!("{VM_GUEST_ASSET}.gz"),
                    VM_GUEST_PLATFORM,
                    compressed_guest,
                ),
                release_asset(
                    "stable",
                    tag,
                    "nanocodex-x86_64-unknown-linux-gnu",
                    "x86_64-unknown-linux-gnu",
                    cli,
                ),
                release_asset(
                    "stable",
                    tag,
                    "nanocodex-x86_64-unknown-linux-gnu.gz",
                    "x86_64-unknown-linux-gnu",
                    cli,
                ),
            ],
        })
    }

    fn nightly_release(cli: &[u8], guest: &[u8]) -> ReleaseManifest {
        let commit = "c".repeat(40);
        sign_release(ReleaseManifest {
            version: 1,
            kind: "commit".to_owned(),
            id: commit.clone(),
            tag: format!("nightly-{commit}"),
            commit: commit.clone(),
            channel: "nightly".to_owned(),
            finalized_at: "2026-08-22T01:02:03.000Z".to_owned(),
            manifest_sha256: String::new(),
            assets: vec![
                release_asset("commit", &commit, VM_GUEST_ASSET, VM_GUEST_PLATFORM, guest),
                release_asset(
                    "commit",
                    &commit,
                    &format!("{VM_GUEST_ASSET}.gz"),
                    VM_GUEST_PLATFORM,
                    guest,
                ),
                release_asset(
                    "commit",
                    &commit,
                    "nanocodex-x86_64-unknown-linux-gnu",
                    "x86_64-unknown-linux-gnu",
                    cli,
                ),
                release_asset(
                    "commit",
                    &commit,
                    "nanocodex-x86_64-unknown-linux-gnu.gz",
                    "x86_64-unknown-linux-gnu",
                    cli,
                ),
            ],
        })
    }

    fn release_asset(
        kind: &str,
        id: &str,
        name: &str,
        platform: &str,
        contents: &[u8],
    ) -> ReleaseAsset {
        ReleaseAsset {
            name: name.to_owned(),
            platform: platform.to_owned(),
            size: contents.len() as u64,
            sha256: hex::encode(Sha256::digest(contents)),
            content_type: if name.ends_with(".gz") {
                "application/gzip".to_owned()
            } else {
                "application/octet-stream".to_owned()
            },
            download_path: format!("/api/releases/releases/{kind}/{id}/assets/{name}"),
        }
    }

    fn sign_release(mut release: ReleaseManifest) -> ReleaseManifest {
        release.manifest_sha256 = release_manifest_sha256(&release).unwrap();
        release
    }

    fn release_channel(manifest: ReleaseManifest) -> ReleaseChannel {
        ReleaseChannel {
            pointer: ReleasePointer {
                version: 1,
                channel: manifest.channel.clone(),
                kind: manifest.kind.clone(),
                id: manifest.id.clone(),
                tag: manifest.tag.clone(),
                commit: manifest.commit.clone(),
                generation: 2,
                updated_at: "2026-08-22T01:02:04.000Z".to_owned(),
            },
            manifest,
        }
    }
}
