//! Windows MSI handoff. The helper outlives the GUI and owns its temporary files.

use super::*;
use ::windows::core::w;
use ::windows::Win32::System::Registry::{
    RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY,
};
use std::fs::{self, File, OpenOptions};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const FILE_SHARE_READ: u32 = 1;
const CREATE_NO_WINDOW: u32 = 0x08000000;
const TEMP_FILES: &[&str] = &[
    "installer.part",
    "installer.msi",
    "install.ps1",
    "install.json",
    "ready",
    "go",
    "error",
];

pub fn installed_executable() -> Option<PathBuf> {
    if !cfg!(target_arch = "x86_64") {
        return None;
    }
    let mut buffer = vec![0u16; 32768];
    let mut bytes = (buffer.len() * 2) as u32;
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("Software\\meatshell"),
            w!("InstallDir"),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
        .ok()
        .ok()?;
    }
    let end = buffer.iter().position(|c| *c == 0)?;
    let installed = PathBuf::from(String::from_utf16(&buffer[..end]).ok()?).join("meatshell.exe");
    let actual = std::env::current_exe().ok()?.canonicalize().ok()?;
    let expected = installed.canonicalize().ok()?;
    actual
        .to_string_lossy()
        .eq_ignore_ascii_case(&expected.to_string_lossy())
        .then_some(installed)
}

fn cache_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "meatshell", "meatshell")
        .context("Could not locate the update cache")?;
    Ok(dirs.cache_dir().join("updates"))
}

/// Both the GUI and helper can hold a read lease. Cleanup needs exclusive access,
/// so a new application process cannot remove an active download or installer.
pub struct Download {
    directory: PathBuf,
    lease: Option<File>,
    handed_off: bool,
    digest: String,
}

impl Download {
    pub fn fetch(
        asset: &Asset,
        sources: &mirror::Sources,
        progress: impl FnMut(&str, u64, u64),
    ) -> Result<Self> {
        let root = cache_dir()?;
        fs::create_dir_all(&root)?;
        let directory = root.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&directory)?;
        let mut download = Self {
            directory,
            lease: None,
            handed_off: false,
            digest: asset.sha256()?.to_string(),
        };
        let lock = download.directory.join("lease");
        File::create(&lock)?;
        download.lease = Some(
            OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(lock)?,
        );
        let part = download.directory.join("installer.part");
        let mut output = File::create(&part)?;
        asset.download(&mut output, sources, progress)?;
        output.sync_all()?;
        drop(output);
        fs::rename(part, download.directory.join("installer.msi"))?;
        Ok(download)
    }

    /// Returns only after the helper holds its lease and can wait for this process.
    /// A failed handoff leaves the GUI running; dropping this download cleans it up.
    pub fn launch(&mut self, executable: &Path, data_dir: &Path, english: bool) -> Result<()> {
        let script = self.directory.join("install.ps1");
        // Windows PowerShell 5.1 requires a BOM for non-ASCII script text.
        fs::write(&script, include_str!("install.ps1"))?;
        fs::write(
            self.directory.join("install.json"),
            serde_json::to_vec(&serde_json::json!({
                "parent_pid": std::process::id(),
                "executable": executable,
                "data_dir": data_dir,
                "sha256": self.digest,
                "english": english,
            }))?,
        )?;
        for name in ["ready", "go", "error"] {
            let path = self.directory.join(name);
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        let system = std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?;
        let mut child = Command::new(
            PathBuf::from(system).join("System32/WindowsPowerShell/v1.0/powershell.exe"),
        )
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Could not start the Windows update helper")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if self.directory.join("ready").is_file() {
                if let Err(error) = fs::write(self.directory.join("go"), b"install") {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error).context("Could not confirm installation to the helper");
                }
                self.handed_off = true;
                self.lease.take();
                return Ok(());
            }
            if let Some(status) = child.try_wait()? {
                let detail = fs::read_to_string(self.directory.join("error"))
                    .unwrap_or_else(|_| status.to_string());
                bail!("The update helper could not prepare installation: {detail}");
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("The update helper did not respond. Try again.");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Download {
    fn drop(&mut self) {
        self.lease.take();
        if !self.handed_off {
            cleanup_directory(&self.directory);
        }
    }
}

fn cleanup_directory(directory: &Path) {
    let Ok(meta) = fs::symlink_metadata(directory) else {
        return;
    };
    // Do not follow directory junctions or symlinks while clearing the cache.
    if !meta.is_dir() || meta.file_attributes() & 0x400 != 0 {
        return;
    }
    let lock = directory.join("lease");
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&lock);
    if lease.is_err() && lock.exists() {
        return;
    }
    for name in TEMP_FILES {
        let path = directory.join(name);
        if let Err(error) = fs::remove_file(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    "could not remove update temporary file {}: {error}",
                    path.display()
                );
            }
        }
    }
    drop(lease);
    let _ = fs::remove_file(lock);
    let _ = fs::remove_dir(directory);
}

pub fn cleanup_stale_downloads() {
    let Ok(root) = cache_dir() else { return };
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if uuid::Uuid::parse_str(&entry.file_name().to_string_lossy()).is_ok() {
            cleanup_directory(&entry.path());
        }
    }
}

#[cfg(test)]
#[path = "../../tests/update/windows.rs"]
mod tests;
