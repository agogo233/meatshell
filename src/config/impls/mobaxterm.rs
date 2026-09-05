//! MobaXterm `.mxtsessions` export compatibility.
//!
//! MobaXterm exports sessions as a Windows-1252 INI file (see the community
//! reverse-engineering notes, e.g. Ruzgfpegk's ".mxtsessions file format"):
//!
//! ```text
//! [Bookmarks]              ; root folder, `SubRep` is the folder name
//! SubRep=
//! ImgNum=42
//! prod-server=#109#0%192.168.1.5%22%admin%%...%#MobaFont%10%...%15%...%#0#comment#-1
//! [Bookmarks_1]
//! SubRep=My Folder
//! ImgNum=41
//! old-switch=#98#7%10.0.0.1%23%cisco%%...%#...#0# #-1
//! ```
//!
//! A session line is `name=<msg>#<icon>#<group1>#<group2>#<start>#<comments>#<color>`,
//! where the leading `#`-delimited field is the unused "display reconnection
//! message". `group1` is a `%`-separated list whose indices depend on the
//! session type (SSH: `0` type, `1` host, `2` port, `3` user, `14` private key,
//! `19..22` proxy). `group2` is the terminal settings (`5` = charset).
//!
//! Only SSH (`0`) and Telnet (`7`) sessions map to meatshell. Passwords are
//! encrypted by MobaXterm and are not recoverable, so they are left empty (the
//! connect-time prompt asks for them).

use anyhow::{bail, Result};

use super::structs::{AuthMethod, Secret, Session, SessionKind};

/// Parse a MobaXterm `.mxtsessions` (or single-session `.moba`) file into
/// meatshell sessions. Unsupported session types (RDP/VNC/SFTP/…) and entries
/// without a host are skipped; `SubRep` folder names become session groups.
pub(super) fn parse_export(raw: &str) -> Result<Vec<Session>> {
    let mut sessions = Vec::new();
    let mut group = String::new();
    let mut saw_session_line = false;

    for raw_line in raw.lines() {
        let line = raw_line.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('[') {
            if trimmed.starts_with('[') {
                group.clear();
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key == "SubRep" {
            group = restore_escapes(value);
            continue;
        }
        if !value.starts_with('#') {
            continue;
        }

        // `parts[0]` is the reconnection-message field; `parts[1]` the icon
        // number; `parts[2]` the first `%`-group; `parts[3]` the terminal
        // settings; the tail holds start-mode / comments / tab colour.
        let parts: Vec<&str> = value.split('#').collect();
        if parts.len() < 4 {
            continue;
        }
        saw_session_line = true;
        let g1: Vec<&str> = parts[2].split('%').collect();
        let g2: Vec<&str> = parts.get(3).copied().unwrap_or("").split('%').collect();

        let kind = match g1.first().copied().unwrap_or("") {
            "0" => SessionKind::Ssh,
            "7" => SessionKind::Telnet,
            other => {
                tracing::warn!("MobaXterm import: skipping session '{}' of unsupported type '{}'", key, other);
                continue;
            }
        };
        let host = restore_escapes(g1.get(1).copied().unwrap_or("")).trim().to_string();
        if host.is_empty() {
            tracing::warn!("MobaXterm import: skipping session '{}' with an empty host", key);
            continue;
        }
        let default_port = if kind == SessionKind::Telnet { 23 } else { 22 };
        let port = g1
            .get(2)
            .copied()
            .unwrap_or("")
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .unwrap_or(default_port);
        let user = restore_escapes(g1.get(3).copied().unwrap_or("")).trim().to_string();
        let user = if user.eq_ignore_ascii_case("<default>") {
            String::new()
        } else {
            user
        };
        let private_key_path = restore_escapes(g1.get(14).copied().unwrap_or("")).trim().to_string();
        let proxy = build_proxy(&g1);
        let encoding = charset_to_encoding(g2.get(5).copied().unwrap_or(""));
        let note = restore_escapes(parts.get(5).copied().unwrap_or("")).trim().to_string();

        let name = restore_escapes(key);
        sessions.push(Session {
            name,
            host,
            port,
            user,
            auth: if private_key_path.is_empty() {
                AuthMethod::Password
            } else {
                AuthMethod::Key
            },
            private_key_path,
            proxy: Secret::new(proxy),
            kind,
            group: group.clone(),
            encoding,
            note,
            ..Session::new_empty()
        });
    }

    if !saw_session_line {
        bail!("not a MobaXterm session export (no session lines found)");
    }
    if sessions.is_empty() {
        bail!("MobaXterm export contains no supported sessions");
    }
    Ok(sessions)
}

/// Restore the tokens MobaXterm writes in place of characters that would clash
/// with its `#` / `%` separators or a literal `C:` drive root.
fn restore_escapes(value: &str) -> String {
    value
        .replace("__PERCENT__", "%")
        .replace("__PIPE__", "|")
        .replace("__DIEZE__", "#")
        .replace("__PTVIRG__", ";")
        .replace("__DBLQUO__", "\"")
        .replace("_CurrentDrive_", "C:")
}

/// Map a MobaXterm proxy (indices 19..22) to a meatshell proxy URL. Only the
/// supported SOCKS5 / HTTP kinds are carried over; everything else (Socks4,
/// Telnet, Local, SSH forwarding…) is ignored.
fn build_proxy(g1: &[&str]) -> String {
    let scheme = match g1.get(19).copied().unwrap_or("") {
        "2" => Some("socks5"),
        "3" => Some("http"),
        _ => None,
    };
    let Some(scheme) = scheme else {
        return String::new();
    };
    let host = restore_escapes(g1.get(20).copied().unwrap_or("")).trim().to_string();
    if host.is_empty() {
        return String::new();
    }
    // A non-numeric port would build a malformed proxy URL; fall back to the
    // format's documented default instead.
    let port = g1
        .get(21)
        .copied()
        .unwrap_or("")
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .unwrap_or(1080);
    let login = restore_escapes(g1.get(22).copied().unwrap_or("")).trim().to_string();
    let cred = if login.is_empty() {
        String::new()
    } else {
        format!("{login}@")
    };
    format!("{scheme}://{cred}{host}:{port}")
}

/// Map a MobaXterm terminal charset id (index 5 of the terminal settings group)
/// to an encoding label understood by `encoding_rs` (and thus the terminal
/// decoder). Anything unknown — including id `15` (UTF-8) — falls back to UTF-8.
fn charset_to_encoding(value: &str) -> String {
    match value {
        "0" => "ISO-8859-1".to_string(),
        "13" => "ISO-8859-15".to_string(),
        "22" => "CP850".to_string(),
        _ => "UTF-8".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures follow the real field layout (`group1` indices 0..22 and
    // `group2` index 5 = charset; the terminal group starts with the font name
    // and size, so the charset sits five slots in, not first).
    #[test]
    fn parses_folders_ssh_and_telnet_sessions() {
        let raw = "\
[Bookmarks]\r\n\
SubRep=\r\n\
ImgNum=42\r\n\
prod-server=#109#0%192.0.2.10%22%admin%%1%1%%%%%%0%%%%1%1%%%%1080%#MobaFont%10%0%0%1%15%236,236,236%30,30,30#0# #-1\r\n\
win-box=#91#4%192.0.2.20%3389%admin%0%0#15%80%24#0# #-1\r\n\
[Bookmarks_1]\r\n\
SubRep=My Folder\r\n\
ImgNum=41\r\n\
real-export= #109#0%roottest%22%%%0%-1%%%%%0%-1%0%%%-1%0%0%0%%1080%%0%0%1%%0%%%%0%-1%-1%0#MobaFont%10%0%0%-1%15%236,236,236%30,30,30%180,180,192%0%-1%0%%xterm%-1%0%_Std_Colors_0_%80%24%0%0%-1%<none>%%0%1%-1%-1#0# #-1\r\n\
old-switch=#98#7%10.0.0.1%23%cisco%%1%1%%%%%%0%%%%1%1%%%%1080%#MobaFont%10%0%0%1%13%236,236,236%30,30,30#0# #-1\r\n";

        let sessions = parse_export(raw).unwrap();
        assert_eq!(sessions.len(), 3);

        let ssh = &sessions[0];
        assert_eq!(ssh.name, "prod-server");
        assert_eq!(ssh.host, "192.0.2.10");
        assert_eq!(ssh.port, 22);
        assert_eq!(ssh.user, "admin");
        assert!(matches!(ssh.kind, SessionKind::Ssh));
        assert!(ssh.group.is_empty());
        assert_eq!(ssh.encoding, "UTF-8");

        // A verbatim line from a real `.mxtsessions` export (sessionator
        // fixture): 35 `group1` fields and a 28-field terminal group.
        let real = &sessions[1];
        assert_eq!(real.host, "roottest");
        assert_eq!(real.user, "");
        assert!(matches!(real.auth, AuthMethod::Password));
        assert_eq!(real.proxy.as_str(), "");
        assert_eq!(real.encoding, "UTF-8");
        assert_eq!(real.note, "");

        let telnet = &sessions[2];
        assert_eq!(telnet.name, "old-switch");
        assert_eq!(telnet.host, "10.0.0.1");
        assert_eq!(telnet.port, 23);
        assert_eq!(telnet.user, "cisco");
        assert!(matches!(telnet.kind, SessionKind::Telnet));
        assert_eq!(telnet.group, "My Folder");
        assert_eq!(telnet.encoding, "ISO-8859-15");
    }

    #[test]
    fn restores_escapes_and_uses_key_auth_for_ppk_sessions() {
        let raw = "\
[Bookmarks]\r\n\
SubRep=My Folder\r\n\
keyhost=#109#0%192.0.2.30%22%admin__PERCENT__x%%1%1%%%%%%0%%_CurrentDrive_\\Users\\me\\.ssh\\id.ppk%%1%1%%2%proxy.example.com%8080%me#MobaFont%10%0%0%1%15%236,236,236%30,30,30#0# note with __DIEZE__ and __PIPE__#-1\r\n";

        let sessions = parse_export(raw).unwrap();
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.auth, AuthMethod::Key);
        assert_eq!(session.private_key_path, "C:\\Users\\me\\.ssh\\id.ppk");
        assert_eq!(session.user, "admin%x");
        assert_eq!(session.proxy.as_str(), "socks5://me@proxy.example.com:8080");
        assert_eq!(session.note, "note with # and |");
        assert_eq!(session.group, "My Folder");
    }

    #[test]
    fn handles_defaults_short_fields_and_empty_hosts() {
        let raw = "\
[Bookmarks]\r\n\
SubRep=\r\n\
defuser=#109#0%192.0.2.40%22%<default>%%1%1%%%%%%1%%%%1%1%%%%1080%#MobaFont%10%0%0%1%0%236,236,236%30,30,30#0# #-1\r\n\
short=#109#0%192.0.2.50%%#MobaFont%10%0%0%1%22%236,236,236%30,30,30#0# #-1\r\n\
nohost=#109#0%%22%root%%0%1%%%%%%%%%%%%%%%%#MobaFont%10%0%0%1%15%236,236,236%30,30,30#0# #-1\r\n";

        let sessions = parse_export(raw).unwrap();
        assert_eq!(sessions.len(), 2);

        let defuser = &sessions[0];
        assert_eq!(defuser.name, "defuser");
        assert_eq!(defuser.user, "");
        assert_eq!(defuser.encoding, "ISO-8859-1");

        let short = &sessions[1];
        assert_eq!(short.name, "short");
        assert_eq!(short.host, "192.0.2.50");
        assert_eq!(short.port, 22);
        assert_eq!(short.user, "");
        assert!(matches!(short.auth, AuthMethod::Password));
        assert_eq!(short.encoding, "CP850");
    }

    #[test]
    fn rejects_files_without_supported_sessions() {
        // No `name=#…` session lines at all → not a MobaXterm export.
        let error = parse_export("[Bookmarks]\r\nSubRep=\r\nImgNum=42\r\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a MobaXterm session export"));
        // Only unsupported session types (here RDP, type 4) → nothing to import.
        let error = parse_export(
            "win-box=#91#4%192.0.2.20%3389%admin%0%0#15%80%24#0# #-1\r\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("no supported sessions"));
    }
}
