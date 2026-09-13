/// Metadata for a single remote filesystem entry returned by SFTP listing.
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub name: String,
    pub full_path: String,
    pub is_dir: bool,
    /// Raw size in bytes (0 for directories or unknown).
    pub size: u64,
    /// Modification time as Unix timestamp (seconds, u32 = SFTP wire format).
    pub modified: u32,
    /// POSIX permission bits (the low 12, i.e. rwx + setuid/setgid/sticky).
    /// 0 when the server didn't report permissions. Used to prefill the chmod
    /// dialog (#84).
    pub mode: u32,
    /// Full SFTP permission/type bits. Zero means the server omitted them.
    pub permissions_mode: u32,
    /// Numeric owner and group IDs. SFTP servers do not consistently provide
    /// names, so the UI falls back to these values.
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub owner: Option<String>,
    pub group: Option<String>,
    /// Stable kind key used by the UI and automation clients.
    pub file_type: String,
}

pub(crate) fn file_type_from_mode(mode: u32) -> &'static str {
    match mode & 0o170_000 {
        0o040_000 => "directory",
        0o120_000 => "symlink",
        0o140_000 => "socket",
        0o060_000 => "block-device",
        0o020_000 => "character-device",
        0o010_000 => "fifo",
        0o100_000 => "file",
        _ => "unknown",
    }
}

/// Format the complete POSIX mode in the same form users recognize from
/// `ls -l`, including setuid, setgid and sticky-bit markers.
pub(crate) fn format_permissions(mode: u32) -> String {
    if mode == 0 {
        return "-".to_string();
    }
    let type_char = match file_type_from_mode(mode) {
        "directory" => 'd',
        "symlink" => 'l',
        "socket" => 's',
        "block-device" => 'b',
        "character-device" => 'c',
        "fifo" => 'p',
        _ => '-',
    };
    let mut out = String::with_capacity(10);
    out.push(type_char);
    for (bit, ch) in [
        (0o400, 'r'), (0o200, 'w'), (0o100, 'x'),
        (0o040, 'r'), (0o020, 'w'), (0o010, 'x'),
        (0o004, 'r'), (0o002, 'w'), (0o001, 'x'),
    ] {
        out.push(if mode & bit != 0 { ch } else { '-' });
    }
    for (index, special, lower, upper) in [
        (3, 0o4000, 'x', 's'),
        (6, 0o2000, 'x', 's'),
        (9, 0o1000, 'x', 't'),
    ] {
        if mode & special != 0 {
            let execute = out.as_bytes()[index] == lower as u8;
            out.replace_range(index..=index, &(if execute { upper } else { upper.to_ascii_uppercase() }).to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_posix_permissions_and_special_bits() {
        assert_eq!(format_permissions(0o040755), "drwxr-xr-x");
        assert_eq!(format_permissions(0o100640), "-rw-r-----");
        assert_eq!(format_permissions(0o104755), "-rwsr-xr-x");
        assert_eq!(format_permissions(0o102755), "-rwxr-sr-x");
        assert_eq!(format_permissions(0o101755), "-rwxr-xr-t");
        assert_eq!(format_permissions(0), "-");
    }

    #[test]
    fn identifies_file_types_from_mode() {
        assert_eq!(file_type_from_mode(0o040755), "directory");
        assert_eq!(file_type_from_mode(0o120777), "symlink");
        assert_eq!(file_type_from_mode(0o100644), "file");
    }
}

/// One node in the remote directory tree panel.
#[derive(Debug, Clone)]
pub struct RemoteTreeNode {
    pub path: String,
    pub name: String,
    pub depth: u32,
    pub expanded: bool,
    pub has_children: bool,
}
