//! Download mirrors for one-click updates, like picking an Ubuntu mirror.
//!
//! GitHub release downloads can be slow from mainland China. When the UI is
//! Chinese *and* the system time zone is a mainland China zone, the user's
//! mirror list is raced against GitHub itself right before downloading, and
//! the fastest source wins; the rest are fallbacks in speed order.
//!
//! A mirror is a URL prefix placed in front of the official GitHub download
//! URL, e.g. `https://v4.gh-proxy.org/` +
//! `https://github.com/yituorou/meatshell/releases/download/v0.7.9/meatshell-v0.7.9-windows-x86_64.zip`.
//!
//! Mirrors only carry bytes. The URL behind the prefix has already passed
//! [`Release::asset`](super::Release::asset) and the SHA-256 comes from the
//! GitHub API, so a mirror that serves anything else fails verification and
//! the next source is tried.

use anyhow::{ensure, Context, Result};
use std::io::Read;
use std::time::{Duration, Instant};

/// Built-in mirror list, used until the user edits theirs. All are probed, so
/// order does not matter.
pub const DEFAULT_MIRRORS: &[&str] = &[
    "https://v4.gh-proxy.org/",
    "https://v6.gh-proxy.org/",
    "https://cdn.gh-proxy.org/",
    "https://axisnow.gh-proxy.org/",
];

/// Mainland China IANA zones (and their legacy aliases). Matching the zone
/// name rather than the UTC+8 offset keeps Singapore, Hong Kong, Taipei,
/// Perth… on GitHub.
const MAINLAND_ZONES: &[&str] = &[
    "Asia/Shanghai",
    "Asia/Chongqing",
    "Asia/Chungking",
    "Asia/Harbin",
    "Asia/Urumqi",
    "Asia/Kashgar",
    "PRC",
];

/// How much of the file a speed probe reads.
const PROBE_BYTES: u64 = 64 * 1024;

pub fn is_mainland_zone(zone: &str) -> bool {
    MAINLAND_ZONES.iter().any(|z| z.eq_ignore_ascii_case(zone))
}

/// True when the system time zone is a mainland China zone.
pub fn china_time_zone() -> bool {
    match iana_time_zone::get_timezone() {
        Ok(zone) => is_mainland_zone(&zone),
        Err(error) => {
            tracing::debug!("update: time zone lookup failed: {error}");
            false
        }
    }
}

/// Mirrors apply only to the Chinese UI in a mainland China time zone.
pub fn applies(english: bool) -> bool {
    !english && china_time_zone()
}

/// Validate a user-entered mirror prefix and normalise it to end with `/`.
pub fn normalize(input: &str) -> Result<String> {
    let mut prefix = input.trim().to_string();
    ensure!(!prefix.is_empty(), "Empty mirror address");
    if !prefix.ends_with('/') {
        prefix.push('/');
    }
    let url = url::Url::parse(&prefix).context("Invalid mirror address")?;
    ensure!(url.scheme() == "https", "Mirror must use https://");
    ensure!(url.host_str().is_some(), "Mirror has no host");
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Mirror must not contain a user name or password"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "Mirror must not contain ? or #"
    );
    Ok(prefix)
}

/// Where an update may be downloaded from. The empty prefix is GitHub itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sources {
    prefixes: Vec<String>,
    /// The source the user picked; tried first, without a speed test.
    preferred: Option<String>,
}

impl Sources {
    pub fn github() -> Self {
        Self { prefixes: vec![String::new()], preferred: None }
    }

    /// Try `choice` (a prefix, `""` = GitHub) first. Ignored if it is not
    /// one of these sources, e.g. a mirror removed since it was picked.
    pub fn prefer(mut self, choice: Option<&str>) -> Self {
        self.preferred = choice
            .map(|c| if c.is_empty() { Ok(String::new()) } else { normalize(c) })
            .and_then(Result::ok)
            .filter(|p| self.prefixes.contains(p));
        self
    }

    /// True when the download will speed-test the sources first.
    pub fn will_probe(&self) -> bool {
        self.has_mirrors() && self.preferred.is_none()
    }

    /// GitHub plus every valid, distinct mirror; invalid entries are skipped.
    pub fn with_mirrors(mirrors: &[String]) -> Self {
        let mut sources = Self::github();
        for mirror in mirrors {
            match normalize(mirror) {
                Ok(prefix) if !sources.prefixes.contains(&prefix) => sources.prefixes.push(prefix),
                Ok(_) => {}
                Err(error) => tracing::warn!("update: skipping mirror {mirror:?}: {error:#}"),
            }
        }
        sources
    }

    pub fn has_mirrors(&self) -> bool {
        self.prefixes.len() > 1
    }

    /// `(label, url)` pairs to try for `url`. A picked source comes first and
    /// the rest follow in list order; otherwise sources are probed and tried
    /// fastest first. GitHub is always kept as the last resort.
    pub fn ranked(&self, url: &str, size: u64) -> Vec<(String, String)> {
        let prefixes = if let Some(preferred) = &self.preferred {
            preferred_order(&self.prefixes, preferred)
        } else if self.has_mirrors() {
            let timings: Vec<(String, Option<Duration>)> = self
                .prefixes
                .iter()
                .cloned()
                .zip(probe_all(&self.prefixes, url, size))
                .collect();
            for (prefix, time) in &timings {
                tracing::info!(
                    "update: probe {} -> {}",
                    label(prefix),
                    time.map_or("failed".into(), |t| format!("{} ms", t.as_millis()))
                );
            }
            order(timings)
        } else {
            self.prefixes.clone()
        };
        prefixes
            .into_iter()
            .map(|prefix| (label(&prefix), format!("{prefix}{url}")))
            .collect()
    }
}

/// Probe every prefix (`""` = GitHub) against `url` in parallel; the result
/// at each index is that source's time, or `None` if it failed.
pub fn probe_all(prefixes: &[String], url: &str, size: u64) -> Vec<Option<Duration>> {
    std::thread::scope(|scope| {
        let probes: Vec<_> = prefixes
            .iter()
            .map(|prefix| {
                let target = format!("{prefix}{url}");
                scope.spawn(move || probe(&target, size))
            })
            .collect();
        probes.into_iter().map(|p| p.join().ok().flatten()).collect()
    })
}

/// The file the Settings speed test downloads the start of: the Windows zip
/// of release `tag`, which every release ships.
pub fn speed_test_url(tag: &str) -> String {
    format!("https://github.com/agogo233/meatshell/releases/download/{tag}/meatshell-{tag}-windows-x86_64.zip")
}

/// Host shown to the user for a source.
pub fn label(prefix: &str) -> String {
    if prefix.is_empty() {
        return "github.com".into();
    }
    url::Url::parse(prefix)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| prefix.to_string())
}

/// The picked source, then the other mirrors in list order, then GitHub.
fn preferred_order(prefixes: &[String], preferred: &str) -> Vec<String> {
    let mut order = vec![preferred.to_string()];
    order.extend(
        prefixes
            .iter()
            .filter(|p| !p.is_empty() && p.as_str() != preferred)
            .cloned(),
    );
    if !preferred.is_empty() {
        order.push(String::new());
    }
    order
}

/// Responding sources by speed; failed mirrors are dropped, GitHub never is.
fn order(mut timings: Vec<(String, Option<Duration>)>) -> Vec<String> {
    timings.sort_by_key(|(_, time)| time.unwrap_or(Duration::MAX));
    timings
        .into_iter()
        .filter(|(prefix, time)| time.is_some() || prefix.is_empty())
        .map(|(prefix, _)| prefix)
        .collect()
}

/// Time to fetch the first [`PROBE_BYTES`] of the real file, or `None` if
/// the source fails, is slow, or answers with an HTML page.
fn probe(url: &str, size: u64) -> Option<Duration> {
    let start = Instant::now();
    let response = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(4))
        .timeout(Duration::from_secs(8))
        .build()
        .get(url)
        .set("User-Agent", "meatshell-updater")
        .set("Range", &format!("bytes=0-{}", PROBE_BYTES - 1))
        .call()
        .ok()?;
    if !matches!(response.status(), 200 | 206) || response.content_type().starts_with("text/html")
    {
        return None;
    }
    let want = PROBE_BYTES.min(size);
    let read = std::io::copy(
        &mut response.into_reader().take(want),
        &mut std::io::sink(),
    )
    .ok()?;
    (read == want).then(|| start.elapsed())
}

#[cfg(test)]
#[path = "../../tests/update/mirror.rs"]
mod tests;
