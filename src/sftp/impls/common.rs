//! Pure helpers shared by the SFTP panel's transfer backends.
//!
//! The SFTP subsystem is not available everywhere (OpenWrt ships Dropbear,
//! which has no sftp-server), so the panel can fall back to driving the
//! server's `scp` binary over the same SSH connection. The scp backend speaks
//! to the remote through shell commands (`find`, `stat`, `pwd`), so everything
//! in this module is text-in / text-out: no SSH types, no IO — pure functions a
//! test can run offline. The `scp` wire protocol and the SSH plumbing live in
//! the worker modules next door.

use crate::ssh::{file_type_from_mode, RemoteEntry};

/// Build the shell pipeline that lists `path`'s direct children as one
/// `%F|%s|%a|%u|%g|%Y|%n` line each.
///
/// BusyBox (OpenWrt) has no `find -printf` and no `base64`, so the listing is
/// `find -print0 | xargs -0 stat -c`; `stat -c` is verified to support all
/// needed escapes. `%F` is the type ("directory", "regular file", ...), `%s`
/// the size, `%a` the octal permissions, `%u`/`%g` the numeric owner/group,
/// `%Y` the mtime epoch, and `%n` the path (last, so a name containing the `|`
/// separator cannot shift the fixed fields — a known WinSCP-level limitation).
pub(crate) fn scp_stat_command(path: &str) -> String {
    let separator = '|';
    format!(
        // `find`'s stderr is deliberately NOT suppressed: a permission-denied
        // directory must surface as an error, not as a silently empty listing.
        // `stat`'s stderr is suppressed so an empty directory (where xargs runs
        // `stat` with no operands) stays an empty listing instead of "missing
        // operand".
        "find {} -mindepth 1 -maxdepth 1 -print0 \
         | xargs -0 stat -c '%F{separator}%s{separator}%a{separator}%u{separator}%g{separator}%Y{separator}%n' 2>/dev/null",
        sh_quote(path)
    )
}

/// Parse the `stat -c` output of [`scp_stat_command`] into [`RemoteEntry`]s.
///
/// Malformed lines (stray stderr, a path the stat could not read) are skipped.
/// A permission-denied directory is not silently empty: `find`'s stderr stays
/// in the output, so the caller can turn it into an error rather than showing
/// an empty folder. A genuinely empty directory has no stat output at all.
pub(crate) fn parse_scp_stat_output(output: &str) -> Vec<RemoteEntry> {
    output
        .lines()
        .filter_map(|line| parse_scp_stat_line(line))
        .collect()
}

/// `stat -c '%F|%s|%a|%u|%g|%Y|%n'` → one entry, or `None` if the line does not
/// parse (a path is not present in every split, a field is not a number, ...).
fn parse_scp_stat_line(line: &str) -> Option<RemoteEntry> {
    let line = line.trim_end_matches('\r');
    if line.is_empty() {
        return None;
    }
    // `%n` is last and may itself contain `|`, so split into 7 parts max: the
    // trailing part is the whole name (the `|` it holds stays inside it).
    let mut fields = line.splitn(7, '|');
    let f_type = fields.next()?.trim();
    let size = fields.next()?.trim().parse::<u64>().ok()?;
    let mode = u32::from_str_radix(fields.next()?.trim(), 8).ok()?;
    let uid = fields.next()?.trim().parse::<u32>().ok()?;
    let gid = fields.next()?.trim().parse::<u32>().ok()?;
    let modified = fields.next()?.trim().parse::<u64>().ok()?;
    let full_path = fields.next()?.trim().to_string();
    if full_path.is_empty() {
        return None;
    }
    let name = full_path
        .rsplit('/')
        .next()
        .unwrap_or(&full_path)
        .to_string();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    // `%F`'s text varies between GNU and BusyBox ("regular empty file" vs
    // "regular file"), so map it to the type bits by keyword, defaulting to a
    // regular file for anything unrecognised (the browse-first safe default).
    let type_bits = stat_type_bits(f_type);
    let permissions_mode = type_bits | (mode & 0o7777);
    Some(RemoteEntry {
        name,
        full_path,
        is_dir: (permissions_mode & 0o170_000) == 0o040_000,
        size,
        modified: modified as u32,
        mode: mode & 0o7777,
        permissions_mode,
        uid: Some(uid),
        gid: Some(gid),
        owner: None,
        group: None,
        file_type: file_type_from_mode(permissions_mode).to_string(),
    })
}

/// File-type bits for a `stat %F` string. Unknown strings default to a regular
/// file so an unrecognised server never turns a readable file into a "dir".
fn stat_type_bits(f_type: &str) -> u32 {
    let f = f_type.to_ascii_lowercase();
    if f.contains("directory") {
        0o040_000
    } else if f.contains("symbolic link") || f.contains("symlink") {
        0o120_000
    } else if f.contains("fifo") {
        0o010_000
    } else if f.contains("socket") {
        0o140_000
    } else if f.contains("block") {
        0o060_000
    } else if f.contains("character") {
        0o020_000
    } else {
        // "regular file", "regular empty file", and anything unexpected.
        0o100_000
    }
}

/// Single-quoted POSIX shell word (a one-liner, kept local to this module so
/// the helper can stay free of the SSH/worker types around it).
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// SCP wire protocol (pure encode/decode)
// ---------------------------------------------------------------------------
//
// Byte-level view of the classic scp protocol spoken by `scp -f` / `scp -t`
// (dropbear's scp descends from OpenSSH 4.3p2; OpenSSH ≥ 9 keeps the classic
// protocol whenever it runs the remote side). One ack = one `\x00`; an error
// frame is a `\x01`/`\x02` byte followed by a message line. Arrows show the
// direction from OUR side.
//
// DOWNLOAD — remote runs `scp -q [-p] [-r] -f <path>`:
//   us  → \x00                      immediately after exec: the remote source
//                                   calls its response() reader first
//   src → T<mtime> 0 <atime> 0\n    only with -p; us → \x00
//   src → C%04o <size> <name>\n     us → \x00, then the source sends exactly
//                                   <size> bytes plus a final \x00; us → \x00
//   src → D%04o 0 <name>\n          only with -r; us → \x00
//   src → E\n                       us → \x00
//   src → \x01<error line>\n        unsolicited: that entry failed, the source
//                                   continues with the next one
//   end: channel EOF, exit status 0
//
// UPLOAD — remote runs `scp -q -t <path>`; we always target a temp name and
// `mv -f` it into place afterwards, so a failed or cancelled upload can never
// damage an existing file:
//   src → \x00                      the sink writes its first byte before
//                                   reading anything
//   us  → C%04o <size> <name>\n     src → \x00 only if it opened the target
//   us  → <size> bytes + \x00       src → \x00
//   A `\x01` ack to the C frame means the sink could not open the target and
//   has already moved on to the next frame: NO data may follow, the file must
//   be skipped. The end is exit status 0, then `mv -f` promotes the temp.
//
// Frame names travel inside the line, so a name containing `\n` can never be
// represented (dropbear's source refuses it); `/`, `.` and `..` are refused by
// the sink. [`scp_frame_name_ok`] applies the same rules to both directions.

/// Is this name safe to put into a C/D frame, and safe to turn into a local
/// path when it arrives in one? Rejects empty, dot entries, separators and
/// control bytes (mirrors what both scp ends themselves enforce).
pub(crate) fn scp_frame_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name
            .bytes()
            .any(|b| b == b'/' || b == b'\\' || b == b'\n' || b == b'\0')
}

/// `C%04o <size> <name>\n` — the header announcing one file.
pub(crate) fn scp_c_frame(mode: u32, size: u64, name: &str) -> String {
    format!("C{:04o} {} {}\n", mode & 0o7777, size, name)
}

/// `D%04o 0 <name>\n` — one directory level (the size field is always 0).
pub(crate) fn scp_d_frame(mode: u32, name: &str) -> String {
    format!("D{:04o} 0 {}\n", mode & 0o7777, name)
}

/// `E\n` — closes one directory level.
pub(crate) fn scp_e_frame() -> &'static str {
    "E\n"
}

/// One parsed download frame: the kind byte plus its line payload.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ScpFrame {
    /// `C` — one file: permission bits, size in bytes, name.
    File { mode: u32, size: u64, name: String },
    /// `D` — enter one directory level.
    Dir { mode: u32, name: String },
    /// `T` — mtime/atime hint preceding the next C/D frame (only with -p).
    Times { mtime: u64 },
    /// `E` — leave one directory level.
    End,
}

/// Parse the payload after a frame's kind byte: `kind` is the frame's first
/// byte (`C`, `D`, `T` or `E`), `rest` the remainder of its line without the
/// trailing `\n`. The name is taken verbatim after the second space, so names
/// containing spaces survive the round trip (the source formats with exactly
/// two separator spaces).
pub(crate) fn scp_parse_frame(kind: u8, rest: &str) -> Option<ScpFrame> {
    match kind {
        b'C' => {
            let (mode, rest) = rest.split_once(' ')?;
            let (size, name) = rest.split_once(' ')?;
            Some(ScpFrame::File {
                mode: u32::from_str_radix(mode, 8).ok()?,
                size: size.parse().ok()?,
                name: name.to_string(),
            })
        }
        b'D' => {
            let (mode, rest) = rest.split_once(' ')?;
            let (_zero, name) = rest.split_once(' ')?;
            Some(ScpFrame::Dir {
                mode: u32::from_str_radix(mode, 8).ok()?,
                name: name.to_string(),
            })
        }
        b'T' => {
            let (mtime, _rest) = rest.split_once(' ')?;
            Some(ScpFrame::Times {
                mtime: mtime.parse().ok()?,
            })
        }
        b'E' => (rest.is_empty()).then_some(ScpFrame::End),
        _ => None,
    }
}

/// `scp -q [-p] [-r] -f <path>` — the download side. `-O` is deliberately
/// absent: dropbear's getopt does not know it and would exit with a usage
/// error (the classic protocol is what the remote side speaks anyway).
pub(crate) fn scp_download_command(remote: &str, recurse: bool, preserve_mtime: bool) -> String {
    let mut flags = String::from("-q");
    if preserve_mtime {
        flags.push_str(" -p");
    }
    if recurse {
        flags.push_str(" -r");
    }
    format!("scp {flags} -f {}", sh_quote(remote))
}

/// `scp -q -t <path>` — the upload side. No `-p`: we never send T frames,
/// matching the SFTP backend, which does not preserve upload mtimes either.
pub(crate) fn scp_upload_command(remote: &str) -> String {
    format!("scp -q -t {}", sh_quote(remote))
}

/// `test -e <path>` — existence probe (exit status 0 / 1).
pub(crate) fn scp_test_exists(path: &str) -> String {
    format!("test -e {}", sh_quote(path))
}

/// `test -d <path>` — directory probe (exit status 0 / 1).
pub(crate) fn scp_test_dir(path: &str) -> String {
    format!("test -d {}", sh_quote(path))
}

/// `mkdir -p <path>` — create one upload directory level.
pub(crate) fn scp_mkdir_command(path: &str) -> String {
    format!("mkdir -p {}", sh_quote(path))
}

/// `mv -f <from> <to>` — promote an uploaded temp file to its final name.
pub(crate) fn scp_move_command(from: &str, to: &str) -> String {
    format!("mv -f {} {}", sh_quote(from), sh_quote(to))
}

/// `rm -f <path>` — drop a temp file after a failed/cancelled upload.
pub(crate) fn scp_remove_command(path: &str) -> String {
    format!("rm -f {}", sh_quote(path))
}

/// Remote temp name for an atomic upload: `.<name>.part-<pid>-<seq>` inside
/// the target directory (same directory ⇒ `mv` is a rename, not a copy).
pub(crate) fn scp_tmp_name(base: &str, seq: u64) -> String {
    format!(".{base}.part-{}-{seq}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_command_quotes_the_path_and_keeps_the_separator() {
        let cmd = scp_stat_command("/tmp/a b");
        assert!(cmd.contains("find '/tmp/a b'"));
        assert!(cmd.contains("-mindepth 1 -maxdepth 1 -print0"));
        assert!(cmd.contains("xargs -0 stat -c '%F|%s|%a|%u|%g|%Y|%n'"));
    }

    #[test]
    fn parses_a_regular_file_line() {
        let entries = parse_scp_stat_output(
            "regular file|1234|644|1000|1000|1700000000|/home/u/report.txt\n",
        );
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.name, "report.txt");
        assert_eq!(e.full_path, "/home/u/report.txt");
        assert!(!e.is_dir);
        assert_eq!(e.size, 1234);
        assert_eq!(e.mode, 0o644);
        assert_eq!(e.permissions_mode & 0o170_000, 0o100_000);
        assert_eq!(e.uid, Some(1000));
        assert_eq!(e.gid, Some(1000));
        assert_eq!(e.file_type, "file");
    }

    #[test]
    fn parses_a_directory_line() {
        let entries = parse_scp_stat_output("directory|4096|755|0|0|1700000000|/home/u/logs\n");
        let e = &entries[0];
        assert!(e.is_dir);
        assert_eq!(e.permissions_mode & 0o170_000, 0o040_000);
        assert_eq!(e.file_type, "directory");
    }

    #[test]
    fn a_name_containing_the_separator_keeps_the_rest_as_the_name() {
        let entries = parse_scp_stat_output("regular file|10|644|0|0|1|/home/u/a|b.txt\n");
        let e = &entries[0];
        // `%n` is the last field, so the `|` inside the name stays inside it.
        assert_eq!(e.full_path, "/home/u/a|b.txt");
        assert_eq!(e.name, "a|b.txt");
        assert_eq!(e.size, 10);
    }

    #[test]
    fn skips_malformed_lines_and_dot_entries() {
        let entries = parse_scp_stat_output(
            "not a stat line\nregular file|1|644|0|0|1|/home/u/.\nregular file|2|644|0|0|1|/home/u/..\n",
        );
        assert!(entries.is_empty());
    }

    #[test]
    fn unknown_type_defaults_to_a_regular_file() {
        let entries = parse_scp_stat_output("some new type|5|600|0|0|1|/x\n");
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].is_dir);
        assert_eq!(entries[0].permissions_mode & 0o170_000, 0o100_000);
    }

    #[test]
    fn busybox_regular_empty_file_maps_to_a_file() {
        let entries = parse_scp_stat_output("regular empty file|0|644|0|0|1|/x\n");
        assert!(!entries[0].is_dir);
        assert_eq!(entries[0].file_type, "file");
    }

    #[test]
    fn transfer_commands_quote_their_paths() {
        assert_eq!(
            scp_download_command("/tmp/a b", false, false),
            "scp -q -f '/tmp/a b'"
        );
        assert_eq!(
            scp_download_command("/x", true, true),
            "scp -q -p -r -f '/x'"
        );
        assert_eq!(scp_upload_command("/y"), "scp -q -t '/y'");
        assert_eq!(scp_test_exists("/a b"), "test -e '/a b'");
        assert_eq!(scp_test_dir("/a b"), "test -d '/a b'");
        assert_eq!(scp_mkdir_command("/a b"), "mkdir -p '/a b'");
        assert_eq!(
            scp_move_command("/from f", "/to t"),
            "mv -f '/from f' '/to t'"
        );
        assert_eq!(scp_remove_command("/a b"), "rm -f '/a b'");
    }

    #[test]
    fn c_frame_round_trips_with_spaces_in_the_name() {
        let frame = scp_c_frame(0o100_644, 12345, "my report v2.txt");
        assert_eq!(frame, "C0644 12345 my report v2.txt\n");
        let ScpFrame::File { mode, size, name } =
            scp_parse_frame(b'C', "0644 12345 my report v2.txt").unwrap()
        else {
            panic!("C frame did not parse as a file");
        };
        assert_eq!(mode, 0o644);
        assert_eq!(size, 12345);
        assert_eq!(name, "my report v2.txt");
    }

    #[test]
    fn a_name_with_a_leading_space_survives_verbatim() {
        let ScpFrame::File { name, .. } = scp_parse_frame(b'C', "0644 1  x").unwrap() else {
            panic!("C frame did not parse as a file");
        };
        assert_eq!(name, " x");
    }

    #[test]
    fn zero_byte_file_and_d_t_e_frames_parse() {
        let ScpFrame::File { size, name, .. } = scp_parse_frame(b'C', "0644 0 empty").unwrap()
        else {
            panic!("C frame did not parse as a file");
        };
        assert_eq!((size, name.as_str()), (0, "empty"));
        let ScpFrame::Dir { mode, name } = scp_parse_frame(b'D', "0755 0 logs").unwrap() else {
            panic!("D frame did not parse as a dir");
        };
        assert_eq!(mode, 0o755);
        assert_eq!(name, "logs");
        assert_eq!(scp_parse_frame(b'E', ""), Some(ScpFrame::End));
        let ScpFrame::Times { mtime } = scp_parse_frame(b'T', "1700000000 0 1700000000 0").unwrap()
        else {
            panic!("T frame did not parse as times");
        };
        assert_eq!(mtime, 1700000000);
    }

    #[test]
    fn malformed_frames_return_none() {
        assert!(scp_parse_frame(b'C', "0644 name").is_none());
        assert!(scp_parse_frame(b'C', "644x 1 n").is_none());
        assert!(scp_parse_frame(b'C', "").is_none());
        assert!(scp_parse_frame(b'D', "0755 logs").is_none());
        assert!(scp_parse_frame(b'T', "").is_none());
        assert!(scp_parse_frame(b'E', "junk").is_none());
        assert!(scp_parse_frame(b'X', "anything").is_none());
    }

    #[test]
    fn frame_names_reject_separators_and_dot_entries() {
        for name in ["", ".", "..", "a/b", "a\\b", "a\nb", "a\0b"] {
            assert!(!scp_frame_name_ok(name), "{name:?} must be refused");
        }
        for name in ["a b.txt", "中文.txt", ".hidden", "a|b.txt", "v2.tar.gz"] {
            assert!(scp_frame_name_ok(name), "{name:?} must be accepted");
        }
    }

    #[test]
    fn tmp_name_is_hidden_and_unique_per_call() {
        let a = scp_tmp_name("report.txt", 1);
        let b = scp_tmp_name("report.txt", 2);
        assert!(a.starts_with(".report.txt.part-"));
        assert!(a.ends_with("-1"));
        assert_ne!(a, b);
    }
}
