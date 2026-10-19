use super::*;

fn staged_directory() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meatshell-update-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("lease"), b"").unwrap();
    fs::write(dir.join("installer.msi"), b"fixture").unwrap();
    dir
}

#[test]
fn cleanup_waits_for_both_gui_and_helper_leases() {
    let dir = staged_directory();
    let lease = || {
        OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(dir.join("lease"))
            .unwrap()
    };
    let gui = lease();
    let helper = lease();
    cleanup_directory(&dir);
    assert!(dir.join("installer.msi").exists());
    drop(gui);
    cleanup_directory(&dir);
    assert!(dir.join("installer.msi").exists());
    drop(helper);
    cleanup_directory(&dir);
    assert!(!dir.exists());
}

#[test]
fn cleanup_removes_only_known_temporary_files() {
    let dir = staged_directory();
    fs::write(dir.join("unrelated.txt"), b"keep").unwrap();
    cleanup_directory(&dir);
    assert!(!dir.join("installer.msi").exists());
    assert_eq!(fs::read(dir.join("unrelated.txt")).unwrap(), b"keep");
    fs::remove_file(dir.join("unrelated.txt")).unwrap();
    fs::remove_dir(dir).unwrap();
}

#[test]
fn dropping_an_unused_download_removes_its_package() {
    let dir = staged_directory();
    let package = Download {
        directory: dir.clone(),
        lease: None,
        handed_off: false,
        digest: String::new(),
    };
    drop(package);
    assert!(!dir.exists());
}
