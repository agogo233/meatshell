use super::*;

/// A fresh temporary folder, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("meatshell-inplace-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

#[test]
fn classifies_portable_and_tarball_builds_in_writable_folders() {
    let dir = TempDir::new();
    let exe = dir.0.join("meatshell.exe");
    assert_eq!(
        classify("windows", "x86_64", &exe, None, false),
        Method::InPlace(Target::WindowsPortable { exe: exe.clone() })
    );
    assert_eq!(classify("windows", "aarch64", &exe, None, false), Method::Manual);
    let exe = dir.0.join("meatshell");
    assert_eq!(
        classify("linux", "aarch64", &exe, None, false),
        Method::InPlace(Target::LinuxTarball { exe: exe.clone() })
    );
}

#[test]
fn classifies_appimage_and_package_manager_installs() {
    let dir = TempDir::new();
    let file = dir.0.join("meatshell.AppImage");
    write(&file, "appimage");
    let mounted = Path::new("/tmp/.mount_meatsh/usr/bin/meatshell");
    assert_eq!(
        classify("linux", "x86_64", mounted, Some(file.clone()), false),
        Method::InPlace(Target::AppImage { file: file.clone() })
    );
    assert_eq!(
        classify("linux", "x86_64", Path::new("/usr/bin/meatshell"), None, false),
        Method::PackageManager
    );
    assert_eq!(
        classify("linux", "x86_64", Path::new("/app/bin/meatshell"), None, false),
        Method::PackageManager
    );
    // install-linux.sh's root-owned copy is not a package; it needs the release page.
    assert_eq!(
        classify("linux", "x86_64", Path::new("/usr/local/bin/meatshell"), None, false),
        Method::Manual
    );
    assert_eq!(
        classify("linux", "x86_64", &dir.0.join("meatshell"), None, true),
        Method::PackageManager
    );
    // A stale $APPIMAGE that no longer exists falls back to the executable.
    assert_eq!(
        classify("linux", "x86_64", &dir.0.join("meatshell"), Some(dir.0.join("gone")), false),
        Method::InPlace(Target::LinuxTarball { exe: dir.0.join("meatshell") })
    );
}

#[test]
fn classifies_mac_bundles_only_with_the_bundle_layout() {
    let dir = TempDir::new();
    let bundle = dir.0.join("MeatShell.app");
    let exe = bundle.join("Contents/MacOS/meatshell");
    write(&exe, "binary");
    assert_eq!(
        classify("macos", "aarch64", &exe, None, false),
        Method::InPlace(Target::MacApp { bundle })
    );
    assert_eq!(
        classify("macos", "aarch64", &dir.0.join("meatshell"), None, false),
        Method::Manual
    );
}

#[test]
fn unwritable_or_unknown_locations_need_the_release_page() {
    let missing = Path::new("/definitely/not/here/meatshell");
    assert_eq!(classify("linux", "x86_64", missing, None, false), Method::Manual);
    assert_eq!(classify("freebsd", "x86_64", missing, None, false), Method::Manual);
}

#[test]
fn asset_names_match_the_release_workflow() {
    let exe = PathBuf::from("meatshell");
    let tarball = Target::LinuxTarball { exe: exe.clone() };
    assert_eq!(
        asset_names(&Target::WindowsPortable { exe: exe.clone() }, "v0.7.8", "x86_64", None),
        ["meatshell-v0.7.8-windows-x86_64.zip"]
    );
    assert_eq!(
        asset_names(&Target::AppImage { file: exe.clone() }, "v0.7.8", "x86_64", None),
        ["meatshell-v0.7.8-linux-x86_64.AppImage"]
    );
    assert_eq!(
        asset_names(&tarball, "v0.7.8", "aarch64", None),
        ["meatshell-v0.7.8-linux-aarch64.tar.gz"]
    );
    assert_eq!(
        asset_names(&tarball, "v0.7.8", "x86_64", Some("glibc228")),
        ["meatshell-v0.7.8-linux-x86_64-glibc228.tar.gz"]
    );
    // macOS: the .dmg first (#503), the .zip as the fallback.
    assert_eq!(
        asset_names(&Target::MacApp { bundle: exe }, "v0.7.8", "aarch64", None),
        ["meatshell-v0.7.8-macos-aarch64.dmg", "meatshell-v0.7.8-macos-aarch64.zip"]
    );
}

/// A release offering `files` (name → has a SHA-256 digest).
fn release_with(files: &[(&str, bool)]) -> Release {
    let assets: Vec<_> = files
        .iter()
        .map(|(name, digest)| {
            serde_json::json!({
                "name": name,
                "browser_download_url":
                    format!("https://github.com/yituorou/meatshell/releases/download/v0.8.0/{name}"),
                "size": 1234,
                "digest": digest.then(|| format!("sha256:{}", "a".repeat(64))),
            })
        })
        .collect();
    serde_json::from_value(serde_json::json!({ "tag_name": "v0.8.0", "assets": assets })).unwrap()
}

#[test]
fn mac_updates_prefer_the_dmg_and_fall_back_to_the_zip() {
    let mac = Target::MacApp { bundle: PathBuf::from("meatshell.app") };
    let names = asset_names(&mac, "v0.8.0", "aarch64", None);
    let dmg = "meatshell-v0.8.0-macos-aarch64.dmg";
    let zip = "meatshell-v0.8.0-macos-aarch64.zip";
    let picked = |release: &Release| -> Vec<String> {
        usable_assets(release, &names)
            .unwrap()
            .into_iter()
            .map(|(_, name)| name.to_string())
            .collect()
    };

    // Both published: dmg first, zip kept as the in-run fallback.
    assert_eq!(picked(&release_with(&[(zip, true), (dmg, true)])), [dmg, zip]);
    // Releases from before the .dmg existed: the zip.
    assert_eq!(picked(&release_with(&[(zip, true)])), [zip]);
    // A dmg without a checksum is never used.
    assert_eq!(picked(&release_with(&[(dmg, false), (zip, true)])), [zip]);
    // dmg-only releases (after the zip is retired) work too.
    assert_eq!(picked(&release_with(&[(dmg, true)])), [dmg]);
    // Nothing usable: the error names the zip, the long-standing file.
    let error = usable_assets(&release_with(&[]), &names).unwrap_err().to_string();
    assert!(error.contains(zip), "{error}");
}

#[test]
fn disk_images_are_refused_off_macos() {
    if cfg!(target_os = "macos") {
        return;
    }
    let dir = TempDir::new();
    let error = copy_app_from_dmg(&dir.0.join("x.dmg"), &dir.0.join("mnt"), &dir.0.join("new"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("only supported on macOS"), "{error}");
}

/// Installed macOS builds from v0.7.8 until the .dmg-aware updater only know
/// `meatshell-<tag>-macos-<arch>.zip`. The release workflow must keep
/// producing and attaching that zip next to the .dmg (#503) until those
/// builds are gone — dropping it would break their one-click updates.
#[test]
fn release_workflow_keeps_the_macos_update_zip() {
    let workflow = include_str!("../../.github/workflows/release.yml");
    // Matrix names produce meatshell-<tag>-macos-<arch>.
    assert!(workflow.contains("name: macos-aarch64"));
    assert!(workflow.contains("name: macos-x86_64"));
    assert!(workflow.contains(r#"STAGE="meatshell-${VERSION}-${{ matrix.name }}""#));
    // The macOS branch packs the bundle into that zip and hands it to the
    // mandatory attach step.
    let mac = &workflow[workflow.find(r#"if [ "${{ runner.os }}" = "macOS" ]; then"#).unwrap()..];
    let mac = &mac[..mac.find("elif").unwrap()];
    assert!(mac.contains(r#"ditto -c -k --keepParent "$APP" "${STAGE}.zip""#));
    assert!(mac.contains(r#"echo "ASSET=${STAGE}.zip" >> "$GITHUB_ENV""#));
    // The step from its own "- name:" up to the next step's.
    let start = workflow.find("- name: Attach to GitHub Release").unwrap();
    let end = start + 1 + workflow[start + 1..].find("- name:").unwrap();
    let attach = &workflow[start..end];
    assert!(attach.contains("files: ${{ env.ASSET }}"));
    assert!(attach.contains("fail_on_unmatched_files: true"));
    // The .dmg is an extra, best-effort file.
    assert!(workflow.contains("- name: Build DMG (macOS)"));
    assert!(workflow.contains("files: ${{ env.DMG }}"));
}

/// Build `<root>/meatshell-v9.9.9-linux-x86_64/{meatshell,README.md,docs/REMOTE_MCP.md}` as a .tar.gz.
fn tarball(path: &Path) {
    let file = File::create(path).unwrap();
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    for (name, text, mode) in [
        ("meatshell-v9.9.9-linux-x86_64/meatshell", "new binary", 0o755),
        ("meatshell-v9.9.9-linux-x86_64/README.md", "new readme", 0o644),
        ("meatshell-v9.9.9-linux-x86_64/docs/REMOTE_MCP.md", "new docs", 0o644),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(text.len() as u64);
        header.set_mode(mode);
        header.set_cksum();
        builder.append_data(&mut header, name, text.as_bytes()).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap();
}

fn installed_tree(dir: &Path, exe_name: &str) -> PathBuf {
    let exe = dir.join(exe_name);
    write(&exe, "old binary");
    write(&dir.join("README.md"), "old readme");
    write(&dir.join(RELEASE_MARKER), "notices");
    write(&dir.join("config/config.json"), "user settings");
    write(&dir.join("log/meatshell.log"), "user log");
    exe
}

#[test]
fn tarball_update_replaces_program_files_and_keeps_user_data() {
    let dir = TempDir::new();
    let exe = installed_tree(&dir.0, "meatshell");
    let target = Target::LinuxTarball { exe: exe.clone() };
    let mut prepared = Prepared::stage(&target).unwrap();
    let package = prepared.work.join("package");
    tarball(&package);
    prepared.unpack(&package, false).unwrap();
    let work = prepared.work.clone();

    assert_eq!(prepared.install().unwrap(), exe);
    drop(prepared);

    assert_eq!(read(&exe), "new binary");
    assert_eq!(read(&dir.0.join("README.md")), "new readme");
    assert_eq!(read(&dir.0.join("docs/REMOTE_MCP.md")), "new docs");
    assert_eq!(read(&dir.0.join("config/config.json")), "user settings");
    assert_eq!(read(&dir.0.join("log/meatshell.log")), "user log");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&exe).unwrap().permissions().mode() & 0o111, 0o111);
    }
    // The finished staging folder is marked and removed by the next start.
    assert!(work.join(DONE).is_file());
    cleanup_staging(&target.staging_root().unwrap(), Duration::from_secs(3600));
    assert!(!target.staging_root().unwrap().exists());
}

#[test]
fn a_binary_outside_a_release_folder_gets_only_the_binary_replaced() {
    let dir = TempDir::new();
    let exe = dir.0.join("meatshell");
    write(&exe, "old binary");
    write(&dir.0.join("README.md"), "someone else's readme");
    let target = Target::LinuxTarball { exe: exe.clone() };
    let mut prepared = Prepared::stage(&target).unwrap();
    let package = prepared.work.join("package");
    tarball(&package);
    prepared.unpack(&package, false).unwrap();
    prepared.install().unwrap();
    drop(prepared);
    assert_eq!(read(&exe), "new binary");
    assert_eq!(read(&dir.0.join("README.md")), "someone else's readme");
    assert!(!dir.0.join("docs").exists());
}

#[test]
fn archives_with_unexpected_layouts_are_rejected() {
    let dir = TempDir::new();
    write(&dir.0.join("a/meatshell"), "x");
    write(&dir.0.join("b/meatshell"), "x");
    assert!(single_dir_containing(&dir.0, "meatshell").is_err());
    let dir = TempDir::new();
    write(&dir.0.join("a/README.md"), "x");
    assert!(single_dir_containing(&dir.0, "meatshell").is_err());
    let dir = TempDir::new();
    write(&dir.0.join("Other.app/Contents/Info.plist"), "x");
    assert!(single_app_bundle(&dir.0).is_err());
    fs::create_dir_all(dir.0.join("Other.app/Contents/MacOS")).unwrap();
    assert!(single_app_bundle(&dir.0).is_ok());
}

#[cfg(windows)]
#[test]
fn portable_zip_update_unpacks_with_the_system_tar() {
    let dir = TempDir::new();
    let source = TempDir::new();
    let top = source.0.join("meatshell-v9.9.9-windows-x86_64");
    write(&top.join("meatshell.exe"), "new exe");
    write(&top.join("docs/REMOTE_MCP.md"), "new docs");
    let exe = installed_tree(&dir.0, "meatshell.exe");
    let target = Target::WindowsPortable { exe: exe.clone() };
    let mut prepared = Prepared::stage(&target).unwrap();
    // tar.exe -a picks the zip format from the extension.
    let zip = prepared.work.join("package.zip");
    let tar = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/tar.exe");
    let status = Command::new(tar)
        .arg("-a")
        .arg("-cf")
        .arg(&zip)
        .arg("-C")
        .arg(&source.0)
        .arg("meatshell-v9.9.9-windows-x86_64")
        .status()
        .unwrap();
    assert!(status.success());
    prepared.unpack(&zip, false).unwrap();

    prepared.install().unwrap();
    drop(prepared);
    assert_eq!(read(&exe), "new exe");
    assert_eq!(read(&old_exe_path(&exe)), "old binary");
    assert_eq!(read(&dir.0.join("docs/REMOTE_MCP.md")), "new docs");
    assert_eq!(read(&dir.0.join("config/config.json")), "user settings");
}

#[test]
fn a_failed_swap_restores_the_previous_executable() {
    let dir = TempDir::new();
    let exe = installed_tree(&dir.0, "meatshell.exe");
    let target = Target::WindowsPortable { exe: exe.clone() };
    let mut prepared = Prepared::stage(&target).unwrap();
    // A payload without the new executable makes the second rename fail.
    prepared.payload = prepared.work.join("new/empty");
    fs::create_dir_all(&prepared.payload).unwrap();
    let work = prepared.work.clone();

    assert!(prepared.install().is_err());
    assert_eq!(read(&exe), "old binary");
    assert!(!old_exe_path(&exe).exists());
    drop(prepared);
    assert!(!work.exists(), "an abandoned update removes its staging folder");
    assert!(!dir.0.join(STAGING).exists());
}

#[test]
fn mac_bundle_swap_keeps_the_old_bundle_until_cleanup() {
    let dir = TempDir::new();
    let bundle = dir.0.join("MeatShell.app");
    write(&bundle.join("Contents/MacOS/meatshell"), "old app");
    let target = Target::MacApp { bundle: bundle.clone() };
    let mut prepared = Prepared::stage(&target).unwrap();
    let new_app = prepared.work.join("new/MeatShell.app");
    write(&new_app.join("Contents/MacOS/meatshell"), "new app");
    prepared.payload = new_app;
    let work = prepared.work.clone();

    assert_eq!(prepared.install().unwrap(), bundle);
    drop(prepared);
    assert_eq!(read(&bundle.join("Contents/MacOS/meatshell")), "new app");
    assert_eq!(read(&work.join("old.app/Contents/MacOS/meatshell")), "old app");
}

#[test]
fn appimage_update_replaces_the_file() {
    let dir = TempDir::new();
    let file = dir.0.join("MeatShell.AppImage");
    write(&file, "old image");
    let target = Target::AppImage { file: file.clone() };
    let mut prepared = Prepared::stage(&target).unwrap();
    let package = prepared.work.join("package");
    write(&package, "new image");
    prepared.unpack(&package, false).unwrap();
    assert_eq!(prepared.install().unwrap(), file);
    assert_eq!(read(&file), "new image");
}

#[test]
fn cleanup_keeps_unfinished_downloads_and_foreign_folders() {
    let dir = TempDir::new();
    let root = dir.0.join(STAGING);
    let busy = root.join(uuid::Uuid::new_v4().to_string());
    let done = root.join(uuid::Uuid::new_v4().to_string());
    write(&busy.join("package.part"), "downloading");
    write(&done.join(DONE), "");
    write(&root.join("not-ours/file"), "keep");
    cleanup_staging(&root, Duration::from_secs(3600));
    assert!(busy.exists());
    assert!(!done.exists());
    assert!(root.join("not-ours/file").exists());
    cleanup_staging(&root, Duration::ZERO);
    assert!(!busy.exists(), "a stale unfinished download is removed");
}

#[test]
fn relaunch_arguments_drop_an_earlier_wait_flag() {
    let args = ["--data-dir", "D:\\p", WAIT_PID_ARG, "42", "ssh://host"].map(OsString::from);
    assert_eq!(
        passthrough_args(args.into_iter()),
        ["--data-dir", "D:\\p", "ssh://host"].map(OsString::from)
    );
}

#[test]
fn waiting_strips_the_flag_and_returns_for_an_exited_process() {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--list")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    let mut args = vec!["meatshell".into(), WAIT_PID_ARG.into(), pid.to_string(), "--x".into()];
    let started = std::time::Instant::now();
    wait_for_previous(&mut args);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(args, ["meatshell", "--x"]);
}
