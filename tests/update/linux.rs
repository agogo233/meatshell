use super::*;
use std::fs;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("meatshell-linux-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const FLATPAK_INFO: &str = "\
[Application]
name=io.github.yituorou.meatshell
runtime=runtime/org.freedesktop.Platform/x86_64/25.08

[Instance]
instance-id=1938274650
app-path=/home/me/.local/share/flatpak/app/io.github.yituorou.meatshell/x86_64/stable/abc/files
branch=stable
flatpak-version=1.16.0

[Session Bus Policy]
org.freedesktop.Flatpak=talk
";

#[test]
fn reads_the_sandbox_flatpak_info() {
    assert_eq!(
        parse_flatpak_info(FLATPAK_INFO),
        Some(FlatpakInfo {
            app_id: FLATPAK_APP_ID.into(),
            instance: "1938274650".into(),
            system: false,
            host_commands: true,
        })
    );
    let system = FLATPAK_INFO.replace("/home/me/.local/share/flatpak/app", "/var/lib/flatpak/app");
    assert!(parse_flatpak_info(&system).unwrap().system);
    // v0.7.9 bundles: no session bus permission for the host's flatpak.
    let without = FLATPAK_INFO.replace("org.freedesktop.Flatpak=talk", "");
    assert!(!parse_flatpak_info(&without).unwrap().host_commands);
    // The key only counts in its own section.
    let elsewhere = without.replace("branch=stable", "org.freedesktop.Flatpak=talk");
    assert!(!parse_flatpak_info(&elsewhere).unwrap().host_commands);
    assert_eq!(parse_flatpak_info("[Application]\nname=x\n"), None);
}

#[test]
fn finds_the_release_deb_in_the_dpkg_database() {
    let dir = TempDir::new();
    let exe = Path::new("/usr/bin/meatshell");
    assert!(!dpkg_owns(&dir.0, exe));
    fs::write(
        dir.0.join("meatshell.list"),
        "/.\n/usr\n/usr/bin\n/usr/bin/meatshell\n",
    )
    .unwrap();
    assert!(dpkg_owns(&dir.0, exe));
    assert!(!dpkg_owns(&dir.0, Path::new("/usr/bin/meatshell-helper")));
    assert!(!dpkg_owns(&dir.0.join("missing"), exe));
}

#[test]
fn finds_the_aur_package_in_the_pacman_database() {
    let dir = TempDir::new();
    let exe = Path::new("/usr/bin/meatshell");
    let entry = |name: &str, files: &str| {
        fs::create_dir_all(dir.0.join(name)).unwrap();
        fs::write(dir.0.join(name).join("files"), files).unwrap();
    };
    // Another package whose name starts with ours does not count.
    entry("meatshell-bin-extra-1.0-1", "%FILES%\nusr/bin/meatshell\n");
    assert!(!pacman_owns(&dir.0, AUR_PACKAGE, exe));
    entry(
        "meatshell-bin-0.7.9-1",
        "%FILES%\nusr/\nusr/bin/\nusr/bin/meatshell\n",
    );
    assert!(pacman_owns(&dir.0, AUR_PACKAGE, exe));
    assert!(!pacman_owns(
        &dir.0,
        AUR_PACKAGE,
        Path::new("/usr/bin/other")
    ));
}

#[test]
fn reads_the_aur_version() {
    let body = r#"{"resultcount":1,"results":[{"Name":"meatshell-bin","Version":"0.7.10-2"}],"type":"multiinfo","version":5}"#;
    assert_eq!(parse_aur_version(body).as_deref(), Some("0.7.10"));
    let epoch = body.replace("0.7.10-2", "1:0.8.0-1");
    assert_eq!(parse_aur_version(&epoch).as_deref(), Some("0.8.0"));
    assert_eq!(parse_aur_version(r#"{"results":[]}"#), None);
    assert!(crate::update::caught_up("0.7.10", "v0.7.10"));
    assert!(crate::update::caught_up("0.8.0", "v0.7.10"));
    assert!(!crate::update::caught_up("0.7.9", "v0.7.10"));
}

#[test]
fn flatpak_install_targets_the_installation_the_app_came_from() {
    let bundle = Path::new("/home/me/.var/app/x/cache/meatshell/update/u/meatshell.flatpak");
    let args = |system| {
        flatpak_install_args(bundle, system)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        args(false),
        [
            "--host",
            "flatpak",
            "install",
            "--user",
            "--noninteractive",
            "--reinstall",
            "--bundle",
            bundle.to_str().unwrap(),
        ]
    );
    assert!(args(true).contains(&"--system".to_string()));
}

#[test]
fn flatpak_relaunch_waits_for_this_instance_and_quotes_arguments() {
    let script = flatpak_relaunch_script("42", &["--data-dir".into(), "/tmp/it's here".into()]);
    assert!(script.contains("grep -qxF '42'"));
    assert!(script.contains(
        r#"exec flatpak run io.github.yituorou.meatshell '--data-dir' '/tmp/it'\''s here'"#
    ));
    // Detached, so flatpak-spawn returns and the sandbox can close.
    assert!(script.trim_end().ends_with('&'));
}

#[test]
fn aur_command_runs_in_any_login_shell() {
    let command = aur_update_command();
    // Wrapped for sh so fish users get the same script; no inner single quotes.
    assert!(command.starts_with("/bin/sh -c '"));
    assert_eq!(command.matches('\'').count(), 2);
    for helper in [
        "paru -S meatshell-bin",
        "yay -S meatshell-bin",
        "makepkg -si",
    ] {
        assert!(command.contains(helper), "{helper}");
    }
}
