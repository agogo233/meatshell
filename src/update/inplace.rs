//! One-click updates for builds that replace their own files (#390): the
//! Windows portable .zip, the Linux AppImage and .tar.gz, and the macOS .app.
//! The Windows MSI keeps its Windows Installer handoff (`windows.rs`); Linux
//! package-manager installs (.deb, Flatpak, AUR) are left to the package manager.
//!
//! [`detect`] classifies the running build. [`prepare`] downloads the matching
//! release file into a hidden staging folder next to the installation (same
//! volume, so the final swap is a rename), verifies its SHA-256 and unpacks it.
//! [`Prepared::install`] swaps the new files in and rolls back if the swap
//! fails, and [`relaunch`] starts the new build with [`WAIT_PID_ARG`] so it
//! waits for this process to exit before claiming the single-instance lock.
//!
//! User data is never touched: a portable build keeps it in `config/` and
//! `log/` next to the executable, which no release archive contains, and those
//! names are skipped even if one ever did.

use super::*;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Passed to the relaunched build: wait for this PID to exit before starting.
pub const WAIT_PID_ARG: &str = "--update-wait-pid";
/// Hidden staging folder created next to the installation.
const STAGING: &str = ".meatshell-update";
/// Marks a staging folder whose swap completed; its leftovers are safe to delete.
const DONE: &str = "done";
/// Top-level names in an installation folder an update must never replace.
const PROTECTED: &[&str] = &["config", "log", STAGING];

/// What an in-place update replaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Unzipped Windows portable build: `<dir>/meatshell.exe`.
    WindowsPortable { exe: PathBuf },
    /// A Linux AppImage file (`$APPIMAGE`).
    AppImage { file: PathBuf },
    /// An unpacked Linux .tar.gz: `<dir>/meatshell`.
    LinuxTarball { exe: PathBuf },
    /// A macOS application bundle.
    MacApp { bundle: PathBuf },
}

/// How the running build can be updated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    /// Installed with the MSI: Windows Installer replaces it (`windows.rs`).
    #[cfg_attr(not(windows), allow(dead_code))]
    Msi(PathBuf),
    /// Replace the build's own files.
    InPlace(Target),
    /// .deb, Flatpak, AUR …: the package manager owns the files.
    PackageManager,
    /// Unknown layout or not writable by this user: use the release page.
    Manual,
}

/// Classify the running build.
pub fn detect() -> Method {
    #[cfg(windows)]
    if let Some(exe) = super::windows::installed_executable() {
        return Method::Msi(exe);
    }
    // A development build would overwrite `target/debug/meatshell`.
    if cfg!(debug_assertions) && std::env::var_os("MEATSHELL_ALLOW_DEV_UPDATE").is_none() {
        return Method::Manual;
    }
    let Ok(exe) = std::env::current_exe() else {
        return Method::Manual;
    };
    classify(
        std::env::consts::OS,
        std::env::consts::ARCH,
        &exe,
        std::env::var_os("APPIMAGE").map(PathBuf::from),
        std::env::var_os("FLATPAK_ID").is_some(),
    )
}

fn classify(
    os: &str,
    arch: &str,
    exe: &Path,
    appimage: Option<PathBuf>,
    flatpak: bool,
) -> Method {
    let parent_writable = |path: &Path| path.parent().is_some_and(dir_is_writable);
    match os {
        "windows" if arch == "x86_64" && parent_writable(exe) => {
            Method::InPlace(Target::WindowsPortable { exe: exe.to_path_buf() })
        }
        "linux" => {
            // /usr/local is install-linux.sh's root-owned copy, not a package, and
            // the app cannot sudo: it needs the release page. Checked outright so
            // the classification does not depend on /usr/local being writable.
            if exe.starts_with("/usr/local") {
                return Method::Manual;
            }
            let packaged = exe.starts_with("/usr");
            if flatpak || exe.starts_with("/app") || packaged {
                return Method::PackageManager;
            }
            if let Some(file) = appimage.filter(|file| file.is_file()) {
                return if arch == "x86_64" && parent_writable(&file) {
                    Method::InPlace(Target::AppImage { file })
                } else {
                    Method::Manual
                };
            }
            if matches!(arch, "x86_64" | "aarch64") && parent_writable(exe) {
                Method::InPlace(Target::LinuxTarball { exe: exe.to_path_buf() })
            } else {
                Method::Manual
            }
        }
        "macos" if matches!(arch, "x86_64" | "aarch64") => {
            // <bundle>.app/Contents/MacOS/<exe>
            let bundle = exe
                .parent()
                .filter(|dir| dir.file_name().is_some_and(|n| n == "MacOS"))
                .and_then(Path::parent)
                .filter(|dir| dir.file_name().is_some_and(|n| n == "Contents"))
                .and_then(Path::parent)
                .filter(|dir| dir.extension().is_some_and(|ext| ext == "app"));
            match bundle {
                Some(bundle) if parent_writable(bundle) && dir_is_writable(bundle) => {
                    Method::InPlace(Target::MacApp { bundle: bundle.to_path_buf() })
                }
                _ => Method::Manual,
            }
        }
        _ => Method::Manual,
    }
}

/// True only if a file can really be created in `dir` (Program Files, /opt and
/// /Applications can refuse writes even when the folder exists).
fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".meatshell-write-probe-{}", std::process::id()));
    match File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// The release files that can update `target`, most preferred first. A Linux
/// .tar.gz fetches the same variant this build came from
/// (`MEATSHELL_RELEASE_VARIANT`, e.g. `glibc228`, set in CI).
///
/// macOS prefers the .dmg (#503) and falls back to the .zip. The .zip must
/// stay in releases for as long as builds that only know the .zip (v0.7.8
/// up to the first build with this fallback) are still around.
fn asset_names(target: &Target, tag: &str, arch: &str, variant: Option<&str>) -> Vec<String> {
    match target {
        Target::WindowsPortable { .. } => vec![format!("meatshell-{tag}-windows-x86_64.zip")],
        Target::AppImage { .. } => vec![format!("meatshell-{tag}-linux-x86_64.AppImage")],
        Target::LinuxTarball { .. } => vec![match variant {
            Some(variant) => format!("meatshell-{tag}-linux-{arch}-{variant}.tar.gz"),
            None => format!("meatshell-{tag}-linux-{arch}.tar.gz"),
        }],
        Target::MacApp { .. } => vec![
            format!("meatshell-{tag}-macos-{arch}.dmg"),
            format!("meatshell-{tag}-macos-{arch}.zip"),
        ],
    }
}

/// The files among `names` this release offers that also pass the download
/// checks in [`Release::asset`] (official URL, non-empty, SHA-256), in
/// preference order. If none qualifies, the last name's error explains why.
fn usable_assets<'a>(
    release: &'a Release,
    names: &'a [String],
) -> Result<Vec<(&'a Asset, &'a str)>> {
    let mut usable = Vec::new();
    let mut last_error = None;
    for name in names {
        match release.asset(name) {
            Ok(asset) => usable.push((asset, name.as_str())),
            Err(error) => last_error = Some(error),
        }
    }
    if usable.is_empty() {
        return Err(last_error.unwrap_or_else(|| anyhow::anyhow!("No update file for this build")));
    }
    Ok(usable)
}

impl Target {
    /// The path the update replaces.
    fn path(&self) -> &Path {
        match self {
            Target::WindowsPortable { exe } | Target::LinuxTarball { exe } => exe,
            Target::AppImage { file } => file,
            Target::MacApp { bundle } => bundle,
        }
    }

    /// Staging lives next to the replaced path so the swap is a same-volume rename.
    fn staging_root(&self) -> Result<PathBuf> {
        Ok(self
            .path()
            .parent()
            .context("The installation has no parent folder")?
            .join(STAGING))
    }
}

/// A verified, unpacked update waiting to be swapped in. Dropping it before
/// [`Prepared::install`] removes the staging folder.
#[derive(Debug)]
pub struct Prepared {
    target: Target,
    work: PathBuf,
    /// The new executable's folder, the new AppImage, or the new `.app`.
    payload: PathBuf,
    installed: bool,
}

/// Download, verify and unpack the update for `target`. When the preferred
/// file (the macOS .dmg) fails to download or unpack, the next one (the
/// .zip) is tried before giving up, so a problem with the newer format can
/// never block an update the older format could deliver.
pub fn prepare(
    release: &Release,
    target: &Target,
    mut progress: impl FnMut(u64, u64),
) -> Result<Prepared> {
    let names = asset_names(
        target,
        &release.tag_name,
        std::env::consts::ARCH,
        option_env!("MEATSHELL_RELEASE_VARIANT"),
    );
    let candidates = usable_assets(release, &names)?;
    let last = candidates.len() - 1;
    for (i, (asset, name)) in candidates.into_iter().enumerate() {
        match prepare_from(target, asset, name.ends_with(".dmg"), &mut progress) {
            Ok(prepared) => return Ok(prepared),
            Err(error) if i < last => {
                tracing::warn!("update: {name} failed ({error:#}); trying the next package");
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("usable_assets returns at least one candidate")
}

fn prepare_from(
    target: &Target,
    asset: &Asset,
    disk_image: bool,
    progress: impl FnMut(u64, u64),
) -> Result<Prepared> {
    let mut prepared = Prepared::stage(target)?;
    let part = prepared.work.join("package.part");
    let mut output = File::create(&part).context("Could not save the update")?;
    asset.download(&mut output, progress)?;
    output.sync_all()?;
    drop(output);
    let package = prepared.work.join("package");
    fs::rename(&part, &package)?;
    prepared.unpack(&package, disk_image)?;
    Ok(prepared)
}

impl Prepared {
    fn stage(target: &Target) -> Result<Self> {
        let root = target.staging_root()?;
        fs::create_dir_all(&root).context("Could not create the update folder")?;
        let work = root.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&work)?;
        Ok(Self {
            target: target.clone(),
            work,
            payload: PathBuf::new(),
            installed: false,
        })
    }

    /// Unpack the verified `package` and locate the new build inside it.
    /// `disk_image`: the package is a macOS .dmg rather than a .zip.
    fn unpack(&mut self, package: &Path, disk_image: bool) -> Result<()> {
        let extract = self.work.join("new");
        self.payload = match &self.target {
            Target::MacApp { .. } if disk_image => {
                copy_app_from_dmg(package, &self.work.join("mnt"), &extract)?;
                single_app_bundle(&extract)?
            }
            Target::WindowsPortable { .. } => {
                unzip(package, &extract)?;
                single_dir_containing(&extract, "meatshell.exe")?
            }
            Target::LinuxTarball { .. } => {
                untar_gz(package, &extract)?;
                single_dir_containing(&extract, "meatshell")?
            }
            Target::MacApp { .. } => {
                unzip(package, &extract)?;
                single_app_bundle(&extract)?
            }
            Target::AppImage { .. } => {
                // The AppImage is the payload itself.
                set_executable(package)?;
                self.payload = package.to_path_buf();
                return Ok(());
            }
        };
        let _ = fs::remove_file(package);
        Ok(())
    }

    /// Swap the new build in. Returns what to relaunch (the executable, the
    /// AppImage or the `.app`). On error the previous build is left in place.
    pub fn install(&mut self) -> Result<PathBuf> {
        let relaunch = match &self.target {
            Target::WindowsPortable { exe } => {
                // Windows refuses to overwrite a running .exe but allows renaming
                // it, so move it aside first and restore it if the swap fails.
                let old = old_exe_path(exe);
                remove_if_exists(&old)
                    .context("An older update is still in use; restart MeatShell and retry")?;
                fs::rename(exe, &old).context("Could not move the running MeatShell aside")?;
                if let Err(error) = fs::rename(self.payload.join("meatshell.exe"), exe) {
                    let _ = fs::rename(&old, exe);
                    return Err(error).context("Could not install the new MeatShell");
                }
                move_tree(&self.payload, parent(exe)?, &["meatshell.exe"]);
                exe.clone()
            }
            Target::LinuxTarball { exe } => {
                // Renaming over a running binary is atomic on Unix; the old
                // process keeps its open inode.
                fs::rename(self.payload.join("meatshell"), exe)
                    .context("Could not install the new MeatShell")?;
                move_tree(&self.payload, parent(exe)?, &["meatshell"]);
                exe.clone()
            }
            Target::AppImage { file } => {
                fs::rename(&self.payload, file).context("Could not replace the AppImage")?;
                file.clone()
            }
            Target::MacApp { bundle } => {
                let old = self.work.join("old.app");
                fs::rename(bundle, &old).context("Could not move the running app aside")?;
                if let Err(error) = fs::rename(&self.payload, bundle) {
                    let _ = fs::rename(&old, bundle);
                    return Err(error).context("Could not install the new app");
                }
                bundle.clone()
            }
        };
        self.installed = true;
        let _ = File::create(self.work.join(DONE));
        Ok(relaunch)
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        if !self.installed {
            let _ = fs::remove_dir_all(&self.work);
            if let Some(root) = self.work.parent() {
                let _ = fs::remove_dir(root); // only if empty
            }
        }
    }
}

fn parent(path: &Path) -> Result<&Path> {
    path.parent().context("The installation has no parent folder")
}

fn old_exe_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Shipped in every release archive; marks a folder as an unpacked release.
const RELEASE_MARKER: &str = "THIRD_PARTY_NOTICES.md";

/// Move the release's other files (README, docs, desktop entry …) over the
/// installation, skipping `skip` and [`PROTECTED`] at the top level. Only done
/// for an unpacked release folder, so a binary copied alone into e.g. `~/bin`
/// does not get documents dropped beside it. Best effort: the new executable is
/// already in place, so a failure here only leaves an older copy of a document.
fn move_tree(from: &Path, to: &Path, skip: &[&str]) {
    if !to.join(RELEASE_MARKER).is_file() {
        return;
    }
    let Ok(entries) = fs::read_dir(from) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if skip.contains(&text.as_ref()) || PROTECTED.contains(&text.as_ref()) {
            continue;
        }
        move_entry(&entry.path(), &to.join(&name));
    }
}

fn move_entry(from: &Path, to: &Path) {
    if from.is_dir() {
        let _ = fs::create_dir_all(to);
        if let Ok(entries) = fs::read_dir(from) {
            for entry in entries.flatten() {
                move_entry(&entry.path(), &to.join(entry.file_name()));
            }
        }
    } else if let Err(error) = fs::rename(from, to) {
        tracing::warn!("update: could not replace {}: {error}", to.display());
    }
}

/// The only folder in `extract`, which must contain `file`.
fn single_dir_containing(extract: &Path, file: &str) -> Result<PathBuf> {
    let mut dirs = fs::read_dir(extract)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir());
    let dir = dirs.next().context("The update package is empty")?;
    ensure!(dirs.next().is_none(), "Unexpected update package layout");
    ensure!(dir.join(file).is_file(), "The update package has no {file}");
    Ok(dir)
}

/// The only `.app` bundle in `extract`.
fn single_app_bundle(extract: &Path) -> Result<PathBuf> {
    let mut apps = fs::read_dir(extract)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.extension().is_some_and(|ext| ext == "app"));
    let app = apps.next().context("The update package has no app")?;
    ensure!(apps.next().is_none(), "Unexpected update package layout");
    ensure!(
        app.join("Contents/Info.plist").is_file() && app.join("Contents/MacOS").is_dir(),
        "The update package has an incomplete app"
    );
    Ok(app)
}

/// Unpack a .zip with the system's own tool: `tar.exe` (bsdtar, Windows 10+)
/// or `ditto` on macOS, which keeps the bundle's symlinks, permissions and
/// signature intact. Both refuse paths that escape the destination.
fn unzip(package: &Path, extract: &Path) -> Result<()> {
    fs::create_dir_all(extract)?;
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let system = std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?;
        let mut command = Command::new(PathBuf::from(system).join("System32").join("tar.exe"));
        command
            .arg("-xf")
            .arg(package)
            .arg("-C")
            .arg(extract)
            .creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("/usr/bin/ditto");
        command.arg("-x").arg("-k").arg(package).arg(extract);
        command
    };
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = package;
        bail!("Unpacking .zip updates is not supported on this system");
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        let output = command
            .stdin(Stdio::null())
            .output()
            .context("Could not start the unpacking tool")?;
        ensure!(
            output.status.success(),
            "Could not unpack the update: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(())
    }
}

/// Copy the `.app` out of a verified macOS disk image (#503) into `extract`.
/// The image is mounted read-only at `mount` (hidden from Finder, no
/// auto-open) and always detached again, forcibly if a normal detach fails,
/// so a failed update never leaves a stray volume behind. `ditto` keeps the
/// bundle's symlinks, permissions and signature, as with the .zip.
fn copy_app_from_dmg(package: &Path, mount: &Path, extract: &Path) -> Result<()> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (package, mount, extract);
        bail!("Disk image updates are only supported on macOS");
    }
    #[cfg(target_os = "macos")]
    {
        fs::create_dir_all(extract)?;
        run(Command::new("/usr/bin/hdiutil")
            .args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-quiet", "-mountpoint"])
            .arg(mount)
            .arg(package))
        .context("Could not open the update disk image")?;

        /// Detaches the image on every exit path.
        struct Mounted<'a>(&'a Path);
        impl Drop for Mounted<'_> {
            fn drop(&mut self) {
                let detach = |force: bool| {
                    let mut command = Command::new("/usr/bin/hdiutil");
                    command.arg("detach").arg("-quiet");
                    if force {
                        command.arg("-force");
                    }
                    run(command.arg(self.0)).is_ok()
                };
                if !detach(false) && !detach(true) {
                    tracing::warn!("update: could not detach {}", self.0.display());
                }
            }
        }
        let _mounted = Mounted(mount);

        // The image holds the .app plus an /Applications shortcut.
        let app = single_app_bundle(mount)?;
        let name = app.file_name().context("The update package has no app")?;
        run(Command::new("/usr/bin/ditto").arg(&app).arg(extract.join(name)))
            .context("Could not copy the app out of the update disk image")?;
        Ok(())
    }
}

/// Run a helper tool, turning a non-zero exit into an error with its stderr.
#[cfg(target_os = "macos")]
fn run(command: &mut Command) -> Result<()> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .context("Could not start a system tool")?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Unpack a .tar.gz. The `tar` crate refuses entries that would land outside
/// `extract` and keeps the executable bit.
fn untar_gz(package: &Path, extract: &Path) -> Result<()> {
    fs::create_dir_all(extract)?;
    let file = File::open(package)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    archive.set_preserve_permissions(true);
    archive
        .unpack(extract)
        .context("Could not unpack the update")?;
    Ok(())
}

fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Start the new build so it waits for this process to exit, keeping the
/// original arguments (e.g. `--data-dir`) so it opens the same profile.
pub fn relaunch(path: &Path) -> Result<()> {
    let pid = std::process::id().to_string();
    let args = passthrough_args(std::env::args_os().skip(1));
    #[cfg(target_os = "macos")]
    let mut command = {
        // `open -n` starts a new instance of the (already replaced) bundle.
        let mut command = Command::new("/usr/bin/open");
        command.arg("-n").arg(path).arg("--args");
        command
    };
    #[cfg(not(target_os = "macos"))]
    let mut command = Command::new(path);
    command
        .args(args)
        .arg(WAIT_PID_ARG)
        .arg(&pid)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0000_0200); // CREATE_NEW_PROCESS_GROUP
    }
    #[cfg(target_os = "linux")]
    if let Some(appdir) = std::env::var_os("APPDIR") {
        // The old AppImage's mount disappears when this process exits; the new
        // AppImage's runtime sets these again for its own mount.
        for name in ["APPDIR", "APPIMAGE", "ARGV0", "OWD"] {
            command.env_remove(name);
        }
        if let Some(paths) = std::env::var_os("LD_LIBRARY_PATH") {
            let kept: Vec<PathBuf> = std::env::split_paths(&paths)
                .filter(|path| !path.starts_with(&appdir))
                .collect();
            match std::env::join_paths(kept) {
                Ok(joined) if !joined.is_empty() => command.env("LD_LIBRARY_PATH", joined),
                _ => command.env_remove("LD_LIBRARY_PATH"),
            };
        }
    }
    command.spawn().context("Could not start the new MeatShell")?;
    Ok(())
}

/// Arguments to hand to the relaunched build: the current ones minus any
/// earlier [`WAIT_PID_ARG`] pair.
fn passthrough_args(args: impl Iterator<Item = OsString>) -> Vec<OsString> {
    let mut kept = Vec::new();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        if arg == WAIT_PID_ARG {
            args.next();
        } else {
            kept.push(arg);
        }
    }
    kept
}

/// Called first thing in `main`: if this process was started by an update,
/// remove the flag from `args` and wait (bounded) for the previous process to
/// exit, so it does not forward this launch to the instance that is closing.
pub fn wait_for_previous(args: &mut Vec<String>) {
    let Some(index) = args.iter().position(|arg| arg == WAIT_PID_ARG) else {
        return;
    };
    let pid = args.get(index + 1).and_then(|value| value.parse::<u32>().ok());
    args.drain(index..(index + 2).min(args.len()));
    if let Some(pid) = pid {
        wait_for_exit(pid, Duration::from_secs(60));
    }
}

fn wait_for_exit(pid: u32, timeout: Duration) {
    use sysinfo::{Pid, ProcessStatus, ProcessesToUpdate, System};
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        match system.process(pid) {
            None => return,
            Some(process) if matches!(process.status(), ProcessStatus::Zombie | ProcessStatus::Dead) => return,
            Some(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    tracing::warn!("update: previous MeatShell (pid {pid}) is still running; starting anyway");
}

/// Remove what a finished update left behind: the renamed Windows executable
/// and completed staging folders (the old macOS bundle lives in one). An
/// unfinished staging folder is kept unless it is a day old, so a second
/// instance downloading an update is not disturbed.
pub fn cleanup_after_update() {
    let Method::InPlace(target) = detect() else {
        return;
    };
    if let Target::WindowsPortable { exe } = &target {
        let _ = remove_if_exists(&old_exe_path(exe));
    }
    if let Ok(root) = target.staging_root() {
        cleanup_staging(&root, Duration::from_secs(24 * 60 * 60));
    }
}

fn cleanup_staging(root: &Path, stale: Duration) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        // Only folders this module created (uuid names); never follow links.
        let ours = uuid::Uuid::parse_str(&entry.file_name().to_string_lossy()).is_ok();
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if !ours || !is_dir {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|age| age > stale);
        if path.join(DONE).is_file() || old {
            let _ = fs::remove_dir_all(&path);
        }
    }
    let _ = fs::remove_dir(root); // only if empty
}

#[cfg(test)]
#[path = "../../tests/update/inplace.rs"]
mod tests;
