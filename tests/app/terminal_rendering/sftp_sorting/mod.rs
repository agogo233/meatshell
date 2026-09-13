use super::*;

fn sftp_entry(name: &str, is_dir: bool) -> SftpEntry {
    SftpEntry {
        name: name.into(),
        full_path: format!("/{name}").into(),
        is_dir,
        size: String::new().into(),
        size_bytes: 0.0,
        modified: String::new().into(),
        modified_ts: 0.0,
        mode: 0,
        file_type: if is_dir { "directory" } else { "file" }.into(),
        permissions: String::new().into(),
        permissions_mode: 0,
        owner: String::new().into(),
        group: String::new().into(),
        selected: false,
    }
}

fn sftp_names(entries: &[SftpEntry]) -> Vec<String> {
    entries.iter().map(|e| e.name.to_string()).collect()
}

#[test]
fn sftp_name_sort_uses_natural_numeric_order() {
    let mut entries = vec![
        sftp_entry("file100", false),
        sftp_entry("file10", false),
        sftp_entry("file2", false),
        sftp_entry("file11", false),
        sftp_entry("file1", false),
    ];
    sort_sftp_entries(&mut entries, "name", 1);
    assert_eq!(
        sftp_names(&entries),
        vec!["file1", "file2", "file10", "file11", "file100"]
    );

    sort_sftp_entries(&mut entries, "name", -1);
    assert_eq!(
        sftp_names(&entries),
        vec!["file100", "file11", "file10", "file2", "file1"]
    );
}

#[test]
fn sftp_default_sort_keeps_dirs_first_with_natural_names() {
    let mut entries = vec![
        sftp_entry("file100", false),
        sftp_entry("dir10", true),
        sftp_entry("file11", false),
        sftp_entry("dir2", true),
    ];
    sort_sftp_entries(&mut entries, "", 0);
    assert_eq!(
        sftp_names(&entries),
        vec!["dir2", "dir10", "file11", "file100"]
    );
}

#[test]
fn sftp_metadata_columns_sort_without_breaking_directory_grouping() {
    let mut entries = vec![
        {
            let mut e = sftp_entry("z-file", false);
            e.permissions = "-rw-------".into();
            e.permissions_mode = 0o100600;
            e
        },
        {
            let mut e = sftp_entry("a-dir", true);
            e.permissions = "drwxr-xr-x".into();
            e.permissions_mode = 0o40755;
            e
        },
        {
            let mut e = sftp_entry("a-file", false);
            e.permissions = "-rwx------".into();
            e.permissions_mode = 0o100700;
            e
        },
    ];
    sort_sftp_entries(&mut entries, "permissions", 1);
    assert_eq!(sftp_names(&entries), vec!["a-dir", "z-file", "a-file"]);
}
