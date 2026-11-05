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
}
