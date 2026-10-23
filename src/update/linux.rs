//! One-click updates for Linux installations the system owns: the release
//! .deb, install-linux.sh's `/usr/local/bin/meatshell`, the Flatpak bundle and
//! the AUR `meatshell-bin` package.
//!
//! The first three download and verify the release file like every other
//! one-click update, then hand the privileged step to the system: `pkexec`
//! (one administrator prompt) runs apt or swaps the binary, and the host's
//! `flatpak` installs the bundle (it asks polkit itself for a system-wide
//! install). AUR packages are built from the AUR, so MeatShell opens a
//! terminal tab running the user's AUR helper instead.
//!
//! [`probe`] runs the few commands detection needs; everything that decides
//! anything from their output is a pure function so it can be tested on any
//! platform.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use anyhow::{bail, Context, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

/// The Flatpak application id (`packaging/flatpak`).
pub const FLATPAK_APP_ID: &str = "io.github.yituorou.meatshell";
/// The package name of the release .deb (`[package.metadata.deb]`).
pub const DEB_PACKAGE: &str = "meatshell";
/// The AUR package (`packaging/aur/PKGBUILD`).
pub const AUR_PACKAGE: &str = "meatshell-bin";
/// Built-in session id of the terminal tab that runs the AUR update.
pub const AUR_SESSION_ID: &str = "system:update-aur";
/// install-linux.sh's install location.
pub const SYSTEM_BINARY: &str = "/usr/local/bin/meatshell";

const AUR_INFO: &str = "https://aur.archlinux.org/rpc/v5/info?arg[]=meatshell-bin";

/// What detection learned about a Linux installation from the system.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    /// Running inside a Flatpak sandbox (`FLATPAK_ID` is set).
    pub in_flatpak: bool,
    /// The sandbox's `/.flatpak-info`, when it could be read.
    pub flatpak: Option<FlatpakInfo>,
    /// The package that owns the executable.
    pub owner: Option<Owner>,
    /// `pkexec` is installed to ask for administrator rights.
    pub pkexec: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    /// Owned by a dpkg package (Debian, Ubuntu …).
    Dpkg(String),
    /// Owned by a pacman package (Arch …).
    Pacman(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlatpakInfo {
    pub app_id: String,
    /// This sandbox's instance id (as listed by `flatpak ps` on the host).
    pub instance: String,
    /// Installed system-wide rather than per user.
    pub system: bool,
    /// The manifest grants `--talk-name=org.freedesktop.Flatpak`, which
    /// `flatpak-spawn --host` needs. Builds up to v0.7.9 lack it.
    pub host_commands: bool,
}

/// Gather what [`super::inplace`]'s classification needs for `exe`.
pub fn probe(exe: &Path) -> Probe {
    let in_flatpak = std::env::var_os("FLATPAK_ID").is_some();
    let flatpak = in_flatpak
        .then(|| std::fs::read_to_string("/.flatpak-info").ok())
        .flatten()
        .and_then(|text| parse_flatpak_info(&text));
    let packaged = exe.starts_with("/usr") && !exe.starts_with("/usr/local");
    let owner = if packaged && !in_flatpak {
        // Read the package databases directly: `dpkg-query -S` scans every
        // package's file list, too slow for start-up.
        dpkg_owns(Path::new("/var/lib/dpkg/info"), exe)
            .then(|| Owner::Dpkg(DEB_PACKAGE.to_string()))
            .or_else(|| {
                pacman_owns(Path::new("/var/lib/pacman/local"), AUR_PACKAGE, exe)
                    .then(|| Owner::Pacman(AUR_PACKAGE.to_string()))
            })
    } else {
        None
    };
    Probe {
        in_flatpak,
        flatpak,
        owner,
        pkexec: !in_flatpak && find_in_path("pkexec").is_some(),
    }
}

fn find_in_path(program: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    std::env::split_paths(&paths)
        .map(|dir| dir.join(program))
        .find(|path| path.is_file())
}

/// The release .deb's file list (`meatshell.list`, or `meatshell:<arch>.list`
/// on multi-arch systems) names `exe`.
pub fn dpkg_owns(info: &Path, exe: &Path) -> bool {
    ["", ":amd64", ":arm64"].iter().any(|arch| {
        std::fs::read_to_string(info.join(format!("{DEB_PACKAGE}{arch}.list")))
            .is_ok_and(|list| list.lines().any(|line| Path::new(line.trim()) == exe))
    })
}

/// pacman's local database has `<package>-<pkgver>-<pkgrel>/files` listing
/// `exe` (stored without the leading `/`).
pub fn pacman_owns(local: &Path, package: &str, exe: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(local) else {
        return false;
    };
    let Ok(relative) = exe.strip_prefix("/") else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // "meatshell-bin-0.7.9-1": the name, then exactly pkgver and pkgrel.
        let ours = name
            .strip_prefix(package)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|version| version.matches('-').count() == 1);
        ours && std::fs::read_to_string(entry.path().join("files"))
            .is_ok_and(|files| files.lines().any(|line| Path::new(line.trim()) == relative))
    })
}

/// Read the keys detection needs from a sandbox's `/.flatpak-info`.
pub fn parse_flatpak_info(text: &str) -> Option<FlatpakInfo> {
    let mut section = "";
    let mut app_id = None;
    let mut instance = None;
    let mut app_path = None;
    let mut host_commands = false;
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match (section, key) {
            ("Application", "name") => app_id = Some(value.to_string()),
            ("Instance", "instance-id") => instance = Some(value.to_string()),
            ("Instance", "app-path") => app_path = Some(value.to_string()),
            ("Session Bus Policy", "org.freedesktop.Flatpak") => {
                host_commands = value == "talk" || value == "own";
            }
            _ => {}
        }
    }
    Some(FlatpakInfo {
        app_id: app_id?,
        instance: instance?,
        // Per-user installations live in ~/.local/share/flatpak.
        system: !app_path?.contains("/.local/share/flatpak/"),
        host_commands,
    })
}

/// Where downloads for a system-owned installation are staged: the user's
/// cache, which root (pkexec) and the host's flatpak can both read. Inside the
/// sandbox `XDG_CACHE_HOME` is `~/.var/app/<id>/cache`, the same path on the host.
pub fn staging_root() -> Result<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .context("No cache folder for the update")?;
    Ok(cache.join("meatshell").join("update"))
}

/// Install a downloaded release .deb with apt (dpkg if apt is missing).
pub fn install_deb(package: &Path) -> Result<()> {
    if Path::new("/usr/bin/apt-get").is_file() {
        // pkexec clears the environment, so set apt's non-interactive mode here.
        elevated(
            "/usr/bin/env",
            &[
                OsStr::new("DEBIAN_FRONTEND=noninteractive"),
                OsStr::new("/usr/bin/apt-get"),
                OsStr::new("install"),
                OsStr::new("-y"),
                package.as_os_str(),
            ],
        )
    } else {
        elevated("/usr/bin/dpkg", &[OsStr::new("-i"), package.as_os_str()])
    }
}

/// Copy beside the target, then rename over it: atomic, and the running
/// process keeps its open inode.
const SWAP_BINARY: &str = r#"install -m 755 "$1" "$2.new" && mv -f "$2.new" "$2""#;

/// Replace the root-owned `dest` with `source`.
pub fn install_binary(source: &Path, dest: &Path) -> Result<()> {
    elevated(
        "/bin/sh",
        &[
            OsStr::new("-c"),
            OsStr::new(SWAP_BINARY),
            OsStr::new("sh"),
            source.as_os_str(),
            dest.as_os_str(),
        ],
    )
}

fn elevated(program: &str, args: &[&OsStr]) -> Result<()> {
    let output = Command::new("pkexec")
        .arg(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("Could not start pkexec")?;
    match output.status.code() {
        Some(0) => Ok(()),
        Some(126) => bail!("Administrator authorization was cancelled"),
        Some(127) => bail!("Not authorized to install the update"),
        _ => bail!("{}", failure_text(&output)),
    }
}

/// The error lines of a failed tool, skipping apt's notes and warnings.
pub fn failure_text(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("N:") && !line.starts_with("W:"))
        .collect();
    match lines.len() {
        0 => format!("The installer failed ({})", output.status),
        n => lines[n.saturating_sub(3)..].join(" "),
    }
}

/// The arguments of the host command that installs the Flatpak `bundle`.
pub fn flatpak_install_args(bundle: &Path, system: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["--host", "flatpak", "install"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.push(if system { "--system" } else { "--user" }.into());
    // --reinstall: a bundle of an installed app is refused otherwise. The
    // running instance keeps its deployment until it exits; data in
    // ~/.var/app is not touched.
    for arg in ["--noninteractive", "--reinstall", "--bundle"] {
        args.push(arg.into());
    }
    args.push(bundle.into());
    args
}

/// Install the downloaded bundle with the host's flatpak.
pub fn install_flatpak(bundle: &Path, system: bool) -> Result<()> {
    let output = Command::new("flatpak-spawn")
        .args(flatpak_install_args(bundle, system))
        .stdin(Stdio::null())
        .output()
        .context("Could not start flatpak-spawn")?;
    if !output.status.success() {
        bail!("{}", failure_text(&output));
    }
    Ok(())
}

/// Single-quoted POSIX shell word.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Host shell script that waits (bounded) for this sandbox instance to exit
/// and then starts the app again. It backgrounds itself so `flatpak-spawn`
/// returns at once: while a process of ours is running, the instance stays.
/// Sandbox and host PIDs differ, so the wait uses the instance id.
pub fn flatpak_relaunch_script(instance: &str, args: &[OsString]) -> String {
    let mut run = format!("flatpak run {FLATPAK_APP_ID}");
    for arg in args {
        run.push(' ');
        run.push_str(&quote(&arg.to_string_lossy()));
    }
    format!(
        "( sleep 0.5; i=0; \
         while [ \"$i\" -lt 300 ] && flatpak ps --columns=instance 2>/dev/null | grep -qxF {instance}; \
         do sleep 0.2; i=$((i+1)); done; exec {run} ) </dev/null >/dev/null 2>&1 &",
        instance = quote(instance),
    )
}

pub fn relaunch_flatpak(instance: &str, args: &[OsString]) -> Result<()> {
    let output = Command::new("flatpak-spawn")
        .args(["--host", "/bin/sh", "-c"])
        .arg(flatpak_relaunch_script(instance, args))
        .stdin(Stdio::null())
        .output()
        .context("Could not start flatpak-spawn")?;
    if !output.status.success() {
        bail!("{}", failure_text(&output));
    }
    Ok(())
}

/// The version the AUR currently offers, e.g. `0.7.9` for `0.7.9-1`.
pub fn aur_version() -> Result<String> {
    let body = ureq::get(AUR_INFO)
        .set("User-Agent", "meatshell-update-check")
        .timeout(Duration::from_secs(15))
        .call()
        .context("Could not check the AUR")?
        .into_string()?;
    parse_aur_version(&body).context("Invalid AUR response")
}

pub fn parse_aur_version(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let version = value
        .get("results")?
        .as_array()?
        .iter()
        .find(|result| result.get("Name").and_then(|n| n.as_str()) == Some(AUR_PACKAGE))?
        .get("Version")?
        .as_str()?;
    // pkgver-pkgrel, possibly with an epoch: "1:0.7.9-1".
    let version = version.rsplit_once(':').map_or(version, |(_, v)| v);
    let version = version.split_once('-').map_or(version, |(v, _)| v);
    Some(version.to_string())
}

/// Shell line the AUR update tab runs: the user's AUR helper, or makepkg.
/// Wrapped in `/bin/sh -c` because the tab's login shell may be fish.
pub fn aur_update_command() -> String {
    let script = format!(
        "if command -v paru >/dev/null 2>&1; then paru -S {pkg}; \
         elif command -v yay >/dev/null 2>&1; then yay -S {pkg}; \
         else d=$(mktemp -d) && git clone https://aur.archlinux.org/{pkg}.git \"$d/{pkg}\" \
         && cd \"$d/{pkg}\" && makepkg -si; fi; \
         echo; echo \"When this finishes, click Restart in MeatShell. / 完成后请在 MeatShell 中点击“重启”。\"",
        pkg = AUR_PACKAGE,
    );
    format!("/bin/sh -c {}", quote(&script))
}

#[cfg(test)]
#[path = "../../tests/update/linux.rs"]
mod tests;
