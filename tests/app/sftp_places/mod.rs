use super::*;
use crate::config::{normalize_sftp_path, SFTP_BOOKMARK_CAP, SFTP_RECENT_CAP};

#[test]
fn paths_are_normalised_for_comparison() {
    assert_eq!(normalize_sftp_path("  /var/log/  ").as_deref(), Some("/var/log"));
    assert_eq!(normalize_sftp_path("/").as_deref(), Some("/"));
    assert_eq!(normalize_sftp_path("///").as_deref(), Some("/"));
    assert_eq!(normalize_sftp_path("   "), None);
}

#[test]
fn recent_is_newest_first_deduplicated_and_capped() {
    let mut p = SftpPlaces::default();
    assert!(p.record_visit("/a"));
    assert!(p.record_visit("/b"));
    // Re-visiting moves to the front instead of duplicating.
    assert!(p.record_visit("/a/"));
    assert_eq!(p.recent, ["/a", "/b"]);
    // Refreshing the folder already on top changes nothing (no config write).
    assert!(!p.record_visit("/a"));
    assert!(!p.record_visit(""));

    for i in 0..SFTP_RECENT_CAP + 5 {
        p.record_visit(&format!("/d{i}"));
    }
    assert_eq!(p.recent.len(), SFTP_RECENT_CAP);
    assert_eq!(p.recent[0], format!("/d{}", SFTP_RECENT_CAP + 4));
}

#[test]
fn bookmarks_toggle_remove_and_cap() {
    let mut p = SftpPlaces::default();
    assert!(p.toggle_bookmark("/srv/app/"));
    assert!(p.is_bookmarked("/srv/app"));
    assert!(!p.toggle_bookmark("/srv/app"));
    assert!(!p.is_bookmarked("/srv/app"));

    p.toggle_bookmark("/x");
    p.toggle_bookmark("/y");
    p.remove_bookmark("/x/");
    assert_eq!(p.bookmarks, ["/y"]);

    for i in 0..SFTP_BOOKMARK_CAP + 3 {
        p.toggle_bookmark(&format!("/b{i}"));
    }
    assert_eq!(p.bookmarks.len(), SFTP_BOOKMARK_CAP);
    assert!(!p.is_bookmarked("/y"), "oldest bookmark is dropped first");
}

#[test]
fn string_model_mirrors_the_list() {
    let m = string_model(&["/a".to_string(), "/b".to_string()]);
    assert_eq!(m.row_count(), 2);
    assert_eq!(m.row_data(1).unwrap().as_str(), "/b");
}
