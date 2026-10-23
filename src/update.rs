//! Release discovery and verified downloads, independent of the desktop UI.

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub mod inplace;
pub mod linux;
pub mod mirror;
#[cfg(windows)]
pub mod windows;

// Fork distribution point: this build's releases live on the fork (agogo233),
// not upstream (yituorou) — pointing at upstream would offer users an
// "upgrade" back to a build without the fork's own fixes (#update-fork-repo).
pub const RELEASE_PAGE: &str = "https://github.com/agogo233/meatshell/releases/latest";
const RELEASE_API: &str = "https://api.github.com/repos/agogo233/meatshell/releases/latest";
// The release manifest (latest.json) replaces the anonymous API lookup, whose
// 60 req/h/IP cap otherwise shows up as a spurious "rate limited".
const RELEASE_MANIFEST: &str =
    "https://github.com/agogo233/meatshell/releases/latest/download/latest.json";
static API_RETRY_AT: Mutex<Option<SystemTime>> = Mutex::new(None);

#[derive(Debug)]
pub struct RateLimited {
    pub retry_at: SystemTime,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let time: chrono::DateTime<chrono::Local> = self.retry_at.into();
        write!(
            f,
            "GitHub API rate limit reached; retry after {}",
            time.format("%H:%M:%S")
        )
    }
}

impl std::error::Error for RateLimited {}

#[derive(Clone, Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

fn version(tag: &str) -> Option<(u32, u32, u32)> {
    let mut parts = tag.strip_prefix('v').unwrap_or(tag).split('.');
    let value = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(value)
}

pub fn newer_release(body: &str, current: &str) -> Result<Option<Release>> {
    let release: Release = serde_json::from_str(body).context("Invalid release response")?;
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let latest = version(&release.tag_name).context("Invalid stable release version")?;
    let current = version(current.split('-').next().unwrap_or(current))
        .context("Invalid installed version")?;
    Ok((latest > current).then_some(release))
}

/// True if `version` (e.g. `0.7.10`, from a package repository) is at least
/// release `tag` (e.g. `v0.7.10`). Unparseable versions count as caught up, so
/// a format change never hides an update.
pub fn caught_up(version: &str, tag: &str) -> bool {
    match (self::version(version), self::version(tag)) {
        (Some(version), Some(tag)) => version >= tag,
        _ => true,
    }
}

pub fn check() -> Result<Option<Release>> {
    check_release(RELEASE_MANIFEST, RELEASE_API, &API_RETRY_AT)
}

fn request(url: &str) -> std::result::Result<ureq::Response, ureq::Error> {
    ureq::get(url)
        .set("User-Agent", "meatshell-update-check")
        .timeout(Duration::from_secs(15))
        .call()
}

fn check_release(
    manifest_url: &str,
    api_url: &str,
    api_retry_at: &Mutex<Option<SystemTime>>,
) -> Result<Option<Release>> {
    // Release assets do not consume the anonymous REST API quota shared by an IP.
    let response = match request(manifest_url) {
        Ok(response) => response,
        Err(ureq::Error::Status(404, _)) => {
            // Older releases have no manifest. Only this path needs the API.
            if let Some(retry_at) = *api_retry_at.lock().unwrap() {
                if retry_at > SystemTime::now() {
                    return Err(RateLimited { retry_at }.into());
                }
            }
            match request(api_url) {
                Err(ureq::Error::Status(status, response)) if status == 403 || status == 429 => {
                    if let Some(limit) = rate_limit(&response, SystemTime::now()) {
                        *api_retry_at.lock().unwrap() = Some(limit.retry_at);
                        return Err(limit.into());
                    }
                    return Err(ureq::Error::Status(status, response))
                        .context("Could not check GitHub releases");
                }
                result => result.context("Could not check GitHub releases")?,
            }
        }
        Err(error) => return Err(error).context("Could not read the update manifest"),
    };
    let body = response.into_string()?;
    newer_release(&body, env!("CARGO_PKG_VERSION"))
}

fn rate_limit(response: &ureq::Response, now: SystemTime) -> Option<RateLimited> {
    let retry_after = response.header("Retry-After");
    let exhausted = response.header("X-RateLimit-Remaining") == Some("0");
    if response.status() != 429 && retry_after.is_none() && !exhausted {
        return None;
    }
    let retry_at = retry_after
        .and_then(|value| {
            value
                .parse::<u64>()
                .ok()
                .and_then(|seconds| now.checked_add(Duration::from_secs(seconds)))
                .or_else(|| {
                    chrono::DateTime::parse_from_rfc2822(value)
                        .ok()
                        .map(SystemTime::from)
                })
        })
        .or_else(|| {
            exhausted
                .then(|| response.header("X-RateLimit-Reset"))
                .flatten()
                .and_then(|value| value.parse::<u64>().ok())
                .and_then(|seconds| UNIX_EPOCH.checked_add(Duration::from_secs(seconds)))
        })
        .filter(|time| *time > now)
        .unwrap_or(now + Duration::from_secs(60));
    Some(RateLimited { retry_at })
}

impl Release {
    pub fn windows_installer(&self) -> Result<&Asset> {
        self.asset(&format!("meatshell-{}-windows-x86_64.msi", self.tag_name))
    }

    /// The release file named `name`, accepted only from this repository's
    /// release download path for this tag and only with a SHA-256 digest.
    pub fn asset(&self, name: &str) -> Result<&Asset> {
        ensure!(version(&self.tag_name).is_some(), "Invalid release version");
        let asset = self
            .assets
            .iter()
            .find(|asset| asset.name == name)
            .with_context(|| format!("This release has no {name} yet. Try again later."))?;
        let url = url::Url::parse(&asset.browser_download_url)?;
        ensure!(
            url.scheme() == "https"
                && url.host_str() == Some("github.com")
                && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path()
                    == format!(
                        "/agogo233/meatshell/releases/download/{}/{name}",
                        self.tag_name
                    ),
            "Unexpected installer download URL"
        );
        ensure!(asset.size > 0, "The installer is empty");
        asset.sha256()?;
        Ok(asset)
    }
}

impl Asset {
    pub fn sha256(&self) -> Result<&str> {
        let digest = self
            .digest
            .as_deref()
            .and_then(|s| s.strip_prefix("sha256:"))
            .context(
                "This release has no SHA-256 checksum. Use the release page to install manually.",
            )?;
        ensure!(
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid installer checksum"
        );
        Ok(digest)
    }

    /// Download into `output` from the fastest of `sources`, falling back to
    /// the next source (GitHub last) when one fails or fails verification.
    /// `progress` receives the source's host, bytes received and total size.
    pub fn download(
        &self,
        output: &mut std::fs::File,
        sources: &mirror::Sources,
        mut progress: impl FnMut(&str, u64, u64),
    ) -> Result<()> {
        use std::io::{Seek, SeekFrom};
        let digest = self.sha256()?;
        let mut last_error = None;
        for (label, url) in sources.ranked(&self.browser_download_url, self.size) {
            output.set_len(0)?;
            output.seek(SeekFrom::Start(0))?;
            tracing::info!("update: downloading from {label}");
            let result = ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(15))
                .timeout_read(Duration::from_secs(60))
                .build()
                .get(&url)
                .set("User-Agent", "meatshell-updater")
                .call()
                .with_context(|| format!("Could not download the installer from {label}"))
                .and_then(|response| {
                    copy_verified(
                        response.into_reader(),
                        &mut *output,
                        self.size,
                        digest,
                        |received, size| progress(&label, received, size),
                    )
                });
            match result {
                Ok(()) => return Ok(()),
                Err(error) => {
                    tracing::warn!("update: {label} failed: {error:#}");
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("No download source")))
    }
}

fn copy_verified(
    mut input: impl Read,
    mut output: impl Write,
    size: u64,
    digest: &str,
    mut progress: impl FnMut(u64, u64),
) -> Result<()> {
    let mut hash = Sha256::new();
    let mut received = 0;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .context("Installer download interrupted")?;
        if count == 0 {
            break;
        }
        received += count as u64;
        ensure!(
            received <= size,
            "Installer is larger than the release metadata"
        );
        output
            .write_all(&buffer[..count])
            .context("Could not save the installer")?;
        hash.update(&buffer[..count]);
        progress(received, size);
    }
    ensure!(received == size, "Installer download is incomplete");
    let actual = format!("{:x}", hash.finalize());
    if !actual.eq_ignore_ascii_case(digest) {
        bail!("Installer checksum does not match the release");
    }
    output.flush()?;
    Ok(())
}

#[cfg(test)]
#[path = "../tests/update/download.rs"]
mod tests;
