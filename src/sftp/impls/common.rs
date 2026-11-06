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

/// Stat one remote path (not a directory listing) in the pipeline's
/// `stat -c` format, so [`parse_scp_stat_output`] can read the line.
pub(crate) fn scp_stat_single_command(path: &str) -> String {
    format!("stat -c '%F|%s|%a|%u|%g|%Y|%n' {}", sh_quote(path))
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

/// `ls -lAn` command used when the busybox `stat` applet is missing.
pub(crate) fn scp_long_command(path: &str) -> String {
    format!("ls -lAn -- {}", sh_quote(path))
}

/// `ls -A` command. Just enough to prove a directory has direct children, so it
/// can cross-check an unexpectedly empty SFTP `read_dir` without making a full
/// metadata round-trip.
pub(crate) fn scp_list_names_command(path: &str) -> String {
    format!("ls -A -- {}", sh_quote(path))
}

/// Parse `ls -lAn` output into [`RemoteEntry`]s.
///
/// Only the columns needed by the panel are retained. `ls` has no portable epoch
/// column, so `modified` is deliberately left at zero rather than guessed from its
/// abbreviated date text.
pub(crate) fn parse_scp_long_output(output: &str, parent: &str) -> Vec<RemoteEntry> {
    output
        .lines()
        .filter_map(|line| parse_scp_long_line(line, parent))
        .collect()
}

fn parse_scp_long_line(line: &str, parent: &str) -> Option<RemoteEntry> {
    let line = line.trim_end_matches('\r');
    if line.is_empty() || line == "total" || line.starts_with("total ") {
        return None;
    }
    let mut fields = line.split_whitespace();
    let mode_text = fields.next()?;
    if mode_text.len() != 10 {
        return None;
    }
    let type_bits = match mode_text.chars().next()? {
        'd' => 0o040_000,
        '-' => 0o100_000,
        'l' => 0o120_000,
        'p' => 0o010_000,
        's' => 0o140_000,
        'b' => 0o060_000,
        'c' => 0o020_000,
        _ => return None,
    };
    let mode = parse_perm_string(mode_text.get(1..)?)?;
    let _links = fields.next()?.parse::<u32>().ok()?;
    let uid = fields.next()?.parse::<u32>().ok()?;
    let gid = fields.next()?.parse::<u32>().ok()?;
    let size = fields.next()?.parse::<u64>().ok()?;
    let _mon = fields.next()?;
    let _day = fields.next()?;
    let _time_or_year = fields.next()?;
    let rest = line[byte_index_after_fields(line, 8)?..].trim();
    if rest.is_empty() {
        return None;
    }
    let name_end = match rest.split_once(" -> ") {
        Some((name, _)) if type_bits == 0o120_000 => name.trim_end(),
        _ => rest,
    };
    if name_end.is_empty() || name_end == "." || name_end == ".." {
        return None;
    }
    let name = name_end.to_string();
    let full_path = format!("{}/{}", parent.trim_end_matches('/'), name);
    let permissions_mode = type_bits | (mode & 0o7777);
    Some(RemoteEntry {
        name,
        full_path,
        is_dir: type_bits == 0o040_000,
        size,
        modified: 0,
        mode: mode & 0o7777,
        permissions_mode,
        uid: Some(uid),
        gid: Some(gid),
        owner: None,
        group: None,
        file_type: file_type_from_mode(permissions_mode).to_string(),
    })
}

/// Parse `ls -l`'s 9-char permission field (`rwxrwxrwx`, plus `s`/`t` special
/// bits) into a `0o077`-range mode. Each slot contributes its position weight
/// when its bit is set. `s`/`t` (lowercase) sit in an exec slot with exec
/// set; `S`/`T` (uppercase) have exec clear. The setuid/sticky bits themselves
/// (0o4000/0o2000) are not tracked — the panel only shows the nine rwx bits.
fn parse_perm_string(perm: &str) -> Option<u32> {
    let chars: Vec<char> = perm.chars().take(9).collect();
    if chars.len() != 9 {
        return None;
    }
    // 1 when the slot's rwx bit is set, 0 when clear.
    let bit = |c: char| -> Option<u32> {
        match c {
            'r' | 'w' | 'x' | 's' | 't' => Some(1),
            'S' | 'T' | '-' => Some(0),
            _ => None,
        }
    };
    const WEIGHTS: [u32; 9] = [
        0o400, 0o200, 0o100, 0o040, 0o020, 0o010, 0o004, 0o002, 0o001,
    ];
    let mut mode = 0u32;
    for (c, weight) in chars.iter().zip(WEIGHTS) {
        mode += bit(*c)? * weight;
    }
    Some(mode)
}

/// Return the byte position after the first `count` whitespace-separated fields.
fn byte_index_after_fields(line: &str, count: usize) -> Option<usize> {
    let mut index = 0usize;
    for _ in 0..count {
        index += line[index..].find(|c: char| !c.is_whitespace())?;
        index += line[index..].find(|c: char| c.is_whitespace())?;
    }
    index += line[index..].find(|c: char| !c.is_whitespace())?;
    Some(index)
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

/// `touch <path>` — create an empty file after an existence probe.
pub(crate) fn scp_touch_command(path: &str) -> String {
    format!("touch {}", sh_quote(path))
}

/// `rm -rf <path>` — recursively remove a file or directory. Paths are quoted
/// and are never a bare root: callers reject empty/root paths before sending it.
pub(crate) fn scp_remove_recursive_command(path: &str) -> String {
    format!("rm -rf {}", sh_quote(path))
}

/// `chmod <octal> <path>` — change permission bits.
pub(crate) fn scp_chmod_command(path: &str, mode: u32) -> String {
    format!("chmod {:o} {}", mode & 0o7777, sh_quote(path))
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
    fn ls_fallback_parses_entries() {
        let entries = parse_scp_long_output(
            "total 8\n\
             drwxr-xr-x 2 0 0 4096 Jan  1  2024 logs\n\
             -rw-r--r-- 1 1000 1000 1234 Jan  1 08:00 my report.txt\n\
             lrwxrwxrwx 1 0 0 9 Jan  1  2024 link -> /root/logs\n",
            "/root",
        );
        assert_eq!(entries.len(), 3);
        assert!(entries[0].is_dir && entries[0].name == "logs");
        assert_eq!(entries[1].size, 1234);
        assert_eq!(entries[1].modified, 0);
        assert_eq!(entries[2].name, "link");
        assert_eq!(entries[2].file_type, "link");
    }

    #[test]
    fn ls_fallback_skips_unparseable_lines() {
        assert!(parse_scp_long_output("total 0\nnot a listing\n", "/root").is_empty());
    }

    #[test]
    fn ls_fallback_parses_setuid_and_sticky() {
        let entries = parse_scp_long_output(
            "-rwsr-xr-x 1 0 0 12 Jan  1 08:00 tool\n\
             -rw-r--r-T 1 0 0 0 Jan  1 08:00 swap\n",
            "/bin",
        );
        assert_eq!(entries.len(), 2);
        // `s` (setuid, exec set) keeps the owner-exec bit → 755; `T` (sticky,
        // exec clear) drops the other-exec bit → 644. The 0o4000/0o2000 bits
        // are not tracked, so both compare against the 0o777 rwx value.
        assert_eq!(entries[0].mode, 0o755);
        assert_eq!(entries[1].mode, 0o644);
    }

    #[test]
    fn perm_string_maps_rwx_to_bits() {
        assert_eq!(parse_perm_string("rwxr-xr-x"), Some(0o755));
        assert_eq!(parse_perm_string("rw-r--r--"), Some(0o644));
        assert_eq!(parse_perm_string("--------"), Some(0));
        // `s` keeps the owner-exec bit, `S` clears it (setuid bit not tracked).
        assert_eq!(parse_perm_string("rwsr-xr-x"), Some(0o755));
        assert_eq!(parse_perm_string("rwSrw-r--"), Some(0o664));
        // `T` clears the other-exec bit (sticky bit not tracked).
        assert_eq!(parse_perm_string("rwxrwxr-T"), Some(0o774));
        assert_eq!(parse_perm_string("short"), None);
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
        assert_eq!(scp_remove_recursive_command("/a b"), "rm -rf '/a b'");
        assert_eq!(scp_touch_command("/a b"), "touch '/a b'");
        assert_eq!(scp_chmod_command("/a b", 0o755), "chmod 755 '/a b'");
        assert_eq!(scp_long_command("/a b"), "ls -lAn -- '/a b'");
        assert_eq!(scp_list_names_command("/a b"), "ls -A -- '/a b'");
        assert_eq!(
            scp_stat_single_command("/tmp/a b"),
            "stat -c '%F|%s|%a|%u|%g|%Y|%n' '/tmp/a b'"
        );
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
