//! Per-session SFTP path bookmarks and recently visited folders (#500).
//!
//! Remote paths only mean something on the host they came from, so places are
//! stored per saved session id (see `ConfigFile::sftp_places`).

use serde::{Deserialize, Serialize};

/// How many recently visited folders are remembered per session.
pub const SFTP_RECENT_CAP: usize = 15;
/// Upper bound on bookmarks per session; the oldest is dropped beyond it.
pub const SFTP_BOOKMARK_CAP: usize = 50;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SftpPlaces {
    /// In the order they were added.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bookmarks: Vec<String>,
    /// Most recent first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<String>,
}

/// Canonical form used for comparison and storage: trimmed, no trailing
/// slash (except the root itself). `None` for an empty path.
pub fn normalize_sftp_path(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut s = trimmed.to_string();
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    Some(s)
}

impl SftpPlaces {
    pub fn is_empty(&self) -> bool {
        self.bookmarks.is_empty() && self.recent.is_empty()
    }

    /// Move `path` to the front of the recent list. Returns whether anything
    /// changed (re-listing the folder already on top is a no-op, so plain
    /// refreshes don't rewrite the config file).
    pub fn record_visit(&mut self, path: &str) -> bool {
        let Some(p) = normalize_sftp_path(path) else {
            return false;
        };
        if self.recent.first() == Some(&p) {
            return false;
        }
        self.recent.retain(|r| *r != p);
        self.recent.insert(0, p);
        self.recent.truncate(SFTP_RECENT_CAP);
        true
    }

    pub fn is_bookmarked(&self, path: &str) -> bool {
        normalize_sftp_path(path).is_some_and(|p| self.bookmarks.contains(&p))
    }

    /// Add or remove `path`; returns whether it is bookmarked afterwards.
    pub fn toggle_bookmark(&mut self, path: &str) -> bool {
        let Some(p) = normalize_sftp_path(path) else {
            return false;
        };
        if let Some(i) = self.bookmarks.iter().position(|b| *b == p) {
            self.bookmarks.remove(i);
            false
        } else {
            if self.bookmarks.len() >= SFTP_BOOKMARK_CAP {
                self.bookmarks.remove(0);
            }
            self.bookmarks.push(p);
            true
        }
    }

    pub fn remove_bookmark(&mut self, path: &str) {
        if let Some(p) = normalize_sftp_path(path) {
            self.bookmarks.retain(|b| *b != p);
        }
    }

    pub fn clear_recent(&mut self) {
        self.recent.clear();
    }
}
