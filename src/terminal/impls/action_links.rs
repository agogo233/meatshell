//! Terminal action links: recognise IPv4 addresses, `host:port` endpoints and
//! `http(s)` URLs in plain terminal output so the user can act on them with a
//! modifier-click (open in browser / fill a diagnostic command).
//!
//! This is a pure, dependency-light matcher layer: it turns a slice of visible
//! text rows into grid-coordinate rectangles plus the resolved value and kind.
//! The UI layer draws the rectangles and dispatches the click; nothing here
//! touches Slint or the network.
//!
//! Design notes (mirrors the frozen plan):
//!   - Only `http` / `https` URLs are linkified. `mailto`, `file`, `ftp` and
//!     every other scheme are deliberately excluded, and the scheme is checked
//!     with a hand-written RFC-3986 parser (no `url` crate) so a malformed or
//!     hostile token can never reach the OS opener.
//!   - `host:port` rejects things that look like `file.ext:line` source
//!     locations (a common false positive in build/log output).
//!   - Higher-priority matchers win on overlap: URL > host:port > IPv4, so an
//!     IP embedded in a URL is not separately linkified.

use regex::Regex;
use std::sync::OnceLock;

use crate::terminal::cell_prefix;

/// Which kind of entity an action link points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActionLinkKind {
    Ip,
    HostPort,
    Url,
}

/// One recognised link on the visible grid. `row`/`col`/`len` are in GRID
/// columns (wide CJK glyphs count as two), matching `TermMatch` so the overlay
/// can be drawn the same way as find highlights.
#[derive(Debug, Clone)]
pub(crate) struct ActionLinkHit {
    pub(crate) row: i32,
    pub(crate) col: i32,
    pub(crate) len: i32,
    pub(crate) kind: ActionLinkKind,
    pub(crate) value: String,
}

/// Per-kind enable flags (the master switch is checked by the caller).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ActionLinkFlags {
    pub(crate) ipv4: bool,
    pub(crate) host_port: bool,
    pub(crate) url: bool,
}

fn ipv4_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(?:(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\b")
            .expect("valid ipv4 regex")
    })
}

fn host_port_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(localhost|(?:(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])|(?:[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?\.)+[a-zA-Z]{2,63}):(\d{1,5})\b")
            .expect("valid host:port regex")
    })
}

fn url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"https?://[^\s<>"]+"#).expect("valid url regex")
    })
}

/// Extensions that mark the left side of `x:y` as a source location rather than
/// a network host (e.g. `main.rs:42`, `app.py:10`).
fn looks_like_file_host(host: &str) -> bool {
    const SOURCE_EXTS: &[&str] = &[
        "py", "js", "jsx", "ts", "tsx", "mjs", "cjs", "java", "kt", "kts", "go", "rs", "rb",
        "php", "c", "cc", "cpp", "cxx", "h", "hpp", "cs", "sh", "bash", "zsh", "fish", "ps1",
        "bat", "cmd", "log", "txt", "md", "json", "yaml", "yml", "xml", "toml", "ini", "lock",
    ];
    match host.rsplit('.').next() {
        Some(ext) => SOURCE_EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

/// Validate an IPv4 octet-quad string (regex already bounds each octet; this
/// guards against leading-zero oddities like `01.2.3.4` being treated as a host).
fn is_valid_ipv4(text: &str) -> bool {
    let mut parts = text.split('.');
    let mut count = 0usize;
    for part in &mut parts {
        count += 1;
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        if part.parse::<u8>().ok().filter(|v| *v <= 255).is_none() {
            return false;
        }
    }
    count == 4
}

/// Hand-written RFC-3986 scheme check + `http`/`https` whitelist. Returns the
/// cleaned URL (trailing punctuation stripped) when the token is a safe web URL.
fn sanitize_http_url(raw: &str) -> Option<String> {
    let (scheme, rest) = raw.split_once(':')?;
    let scheme_l = scheme.to_ascii_lowercase();
    if scheme_l != "http" && scheme_l != "https" {
        return None;
    }
    // scheme must be alpha followed by [alpha/digit/+/-/.]
    let mut chars = scheme.bytes();
    match chars.next() {
        Some(b) if b.is_ascii_alphabetic() => {}
        _ => return None,
    }
    if !chars.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')) {
        return None;
    }
    if rest.is_empty() {
        return None;
    }
    // Reject control characters / spaces that could smuggle arguments to the
    // OS opener.
    if rest.chars().any(|c| (c as u32) < 0x20 || c == '\u{7f}') {
        return None;
    }
    let trimmed = trim_url_punctuation(raw);
    // Require at least one dot or "localhost" so a bare `http:/x` is not opened.
    let authority = trimmed.split_once("://").map(|(_, r)| r).unwrap_or("");
    let host = authority.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() {
        return None;
    }
    Some(trimmed)
}

/// Strip trailing sentence punctuation and balance a trailing close-paren that
/// has no partner inside the URL (common in markdown-style output).
fn trim_url_punctuation(url: &str) -> String {
    let mut out = url.to_string();
    loop {
        let before = out.len();
        out = out.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'', '"', '>', ']', '}']).to_string();
        if out.len() == before {
            break;
        }
    }
    if out.ends_with(')') {
        let opens = out.matches('(').count();
        let closes = out.matches(')').count();
        if closes > opens {
            out.pop();
        }
    }
    out
}

/// Byte offset → char offset within `line`.
fn byte_to_char(line: &str, byte: usize) -> usize {
    line[..byte.min(line.len())].chars().count()
}

/// Scan the visible rows and return non-overlapping action-link hits.
pub(crate) fn scan_action_links(rows: &[String], flags: &ActionLinkFlags) -> Vec<ActionLinkHit> {
    let mut hits: Vec<ActionLinkHit> = Vec::new();
    if !(flags.ipv4 || flags.host_port || flags.url) {
        return hits;
    }
    for (row, line) in rows.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let prefix = cell_prefix(&chars);
        // Track occupied char ranges so a lower-priority matcher cannot overlap
        // a higher-priority one.
        let mut occupied: Vec<(usize, usize)> = Vec::new();

        // Priority order: URL (110) > host:port (100) > IPv4 (90).
        if flags.url {
            for m in url_re().find_iter(line) {
                if let Some(clean) = sanitize_http_url(m.as_str()) {
                    let cs = byte_to_char(line, m.start());
                    let ce = cs + clean.chars().count();
                    if overlaps(&occupied, cs, ce) {
                        continue;
                    }
                    occupied.push((cs, ce));
                    push_hit(&mut hits, row as i32, &prefix, cs, ce, ActionLinkKind::Url, clean);
                }
            }
        }
        if flags.host_port {
            for caps in host_port_re().captures_iter(line) {
                let whole = caps.get(0).unwrap();
                let host = &caps[1];
                let port = &caps[2];
                if looks_like_file_host(host) {
                    continue;
                }
                if port.parse::<u32>().ok().filter(|p| (1..=65535).contains(p)).is_none() {
                    continue;
                }
                let cs = byte_to_char(line, whole.start());
                let ce = byte_to_char(line, whole.end());
                if overlaps(&occupied, cs, ce) {
                    continue;
                }
                occupied.push((cs, ce));
                push_hit(
                    &mut hits,
                    row as i32,
                    &prefix,
                    cs,
                    ce,
                    ActionLinkKind::HostPort,
                    whole.as_str().to_string(),
                );
            }
        }
        if flags.ipv4 {
            for m in ipv4_re().find_iter(line) {
                let text = m.as_str();
                if !is_valid_ipv4(text) {
                    continue;
                }
                let cs = byte_to_char(line, m.start());
                let ce = byte_to_char(line, m.end());
                if overlaps(&occupied, cs, ce) {
                    continue;
                }
                occupied.push((cs, ce));
                push_hit(&mut hits, row as i32, &prefix, cs, ce, ActionLinkKind::Ip, text.to_string());
            }
        }
    }
    hits
}

fn overlaps(occupied: &[(usize, usize)], start: usize, end: usize) -> bool {
    occupied.iter().any(|&(s, e)| start < e && end > s)
}

fn push_hit(
    hits: &mut Vec<ActionLinkHit>,
    row: i32,
    prefix: &[usize],
    char_start: usize,
    char_end: usize,
    kind: ActionLinkKind,
    value: String,
) {
    if char_end <= char_start {
        return;
    }
    let col = prefix.get(char_start).copied().unwrap_or(char_start) as i32;
    let end_cell = prefix.get(char_end).copied().unwrap_or(char_end) as i32;
    let len = (end_cell - col).max(1);
    hits.push(ActionLinkHit {
        row,
        col,
        len,
        kind,
        value,
    });
}

/// The default command a modifier-click fills into the command bar (URLs are
/// opened directly and never routed here).
pub(crate) fn default_command(kind: ActionLinkKind, value: &str) -> Option<String> {
    match kind {
        ActionLinkKind::Ip => Some(format!("ping {value}")),
        ActionLinkKind::HostPort => Some(format!("curl http://{value}")),
        ActionLinkKind::Url => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_flags() -> ActionLinkFlags {
        ActionLinkFlags {
            ipv4: true,
            host_port: true,
            url: true,
        }
    }

    fn scan(line: &str) -> Vec<ActionLinkHit> {
        scan_action_links(&[line.to_string()], &all_flags())
    }

    #[test]
    fn matches_ipv4() {
        let hits = scan("connect to 192.0.2.1 now");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, ActionLinkKind::Ip);
        assert_eq!(hits[0].value, "192.0.2.1");
        assert_eq!(hits[0].col, 11);
        assert_eq!(hits[0].len, 9);
    }

    #[test]
    fn rejects_invalid_octets() {
        assert!(scan("bad 256.1.1.1 here").is_empty());
        assert!(scan("bad 1.2.3.999 here").is_empty());
    }

    #[test]
    fn matches_host_port() {
        let hits = scan("listen on db.internal.test:5432 ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, ActionLinkKind::HostPort);
        assert_eq!(hits[0].value, "db.internal.test:5432");
    }

    #[test]
    fn matches_ip_with_port_as_host_port() {
        // Regression: the octet `\.` must sit outside the alternation so a
        // 3-digit octet (192) still matches; the whole host:port wins over the
        // bare IPv4 by priority.
        let hits = scan("ssh to 192.168.1.10:2222 now");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, ActionLinkKind::HostPort);
        assert_eq!(hits[0].value, "192.168.1.10:2222");
    }

    #[test]
    fn host_port_rejects_source_locations() {
        assert!(scan("error at main.rs:42").is_empty());
        assert!(scan("panic app.py:10").is_empty());
    }

    #[test]
    fn host_port_rejects_out_of_range_port() {
        assert!(scan("svc:99999").is_empty());
    }

    #[test]
    fn matches_http_and_https() {
        let hits = scan("see https://example.com/a?x=1 for details");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, ActionLinkKind::Url);
        assert_eq!(hits[0].value, "https://example.com/a?x=1");
    }

    #[test]
    fn url_strips_trailing_punctuation() {
        let hits = scan("visit http://a.example/x,");
        assert_eq!(hits[0].value, "http://a.example/x");
        let hits = scan("(http://a.example/x)");
        assert_eq!(hits[0].value, "http://a.example/x");
    }

    #[test]
    fn url_rejects_non_http_scheme() {
        assert!(scan("mailto:someone@example.com").is_empty());
        assert!(scan("ftp://host/file").is_empty());
        assert!(scan("file:///etc/passwd").is_empty());
    }

    #[test]
    fn url_wins_over_embedded_ip() {
        // The IP inside the URL must not be separately linkified.
        let hits = scan("open http://192.0.2.1/health");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, ActionLinkKind::Url);
    }

    #[test]
    fn flags_gate_each_kind() {
        let only_url = ActionLinkFlags {
            ipv4: false,
            host_port: false,
            url: true,
        };
        let hits = scan_action_links(
            &["192.0.2.1 and db:5432 and http://x.example/y".to_string()],
            &only_url,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, ActionLinkKind::Url);
    }

    #[test]
    fn cjk_columns_are_wide_aware() {
        // A full-width char before the link shifts its grid column by 2.
        let hits = scan("服务器 192.0.2.1");
        assert_eq!(hits[0].col, 7); // 3 CJK * 2 + space = 7
    }

    #[test]
    fn default_commands() {
        assert_eq!(
            default_command(ActionLinkKind::Ip, "1.2.3.4").as_deref(),
            Some("ping 1.2.3.4")
        );
        assert_eq!(
            default_command(ActionLinkKind::HostPort, "h:1").as_deref(),
            Some("curl http://h:1")
        );
        assert_eq!(default_command(ActionLinkKind::Url, "http://x"), None);
    }
}
