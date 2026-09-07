//! A single, size-capped diagnostic log file (#86).
//!
//! Writes go to `<log_dir>/error.log` — its own `log/` folder beside the exe
//! (portable-first; see [`crate::config::log_dir`]), kept separate from the
//! config dir. The file is capped at a fixed size:
//! when the next write would exceed the cap it is truncated to empty and writing
//! restarts from the top — so there is always exactly one file, at most `cap`
//! bytes, that auto-overwrites its old content. This lets users (e.g. behind a
//! bastion) send their disconnect reason without setting RUST_LOG.
//!
//! The panic hook also lands here: `panic = "abort"` in release means a panic
//! never unwinds, so the tracing subscriber's buffered layers are not
//! guaranteed a flush before the process dies. The hook therefore writes
//! straight to the file (append, no shared locks — a panic while the tracing
//! writer holds its mutex must not deadlock the hook) and `sync_all()`s before
//! returning; `PanicInfo`'s `file:line:col` is a compile-time literal that
//! symbol stripping cannot remove, so even a stripped Windows release build
//! records exactly where the process died.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use super::writer::{CappedFile, CappedWriter, Guard};

/// `<log_dir>/error.log`, in its own `log/` folder beside the exe.
pub fn path() -> Option<PathBuf> {
    let dir = crate::config::log_dir();
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("error.log"))
}

/// Headroom over the tracing layer's cap so the hook's write is never lost to
/// a concurrent truncate.
const PANIC_HOOK_CAP_BYTES: u64 = 45 * 1024 * 1024;

/// Install a panic hook that appends the panic site, message and a backtrace
/// to `error.log` before the (release) `panic = "abort"` kills the process.
/// Must run before `init_tracing` so even early panics are recorded; the
/// previous hook is still invoked afterwards to preserve its behaviour.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string payload>".to_string());
        let report = format!(
            "\n===== PANIC {} =====\nthread: {thread_name}\nlocation: {location}\nmessage: {payload}\nbacktrace:\n{}\n===== END PANIC =====\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            std::backtrace::Backtrace::force_capture(),
        );
        append_panic_report(&report);
        // stderr for dev runs (the release GUI subsystem has no console).
        let _ = std::io::stderr().write_all(report.as_bytes());
        previous(info);
    }));
}

/// Append `report` to `error.log` bypassing every lock the tracing layer uses
/// (a panic while the tracing writer holds its mutex would deadlock the hook
/// otherwise). Best-effort: IO failures are swallowed — panicking inside the
/// panic hook would abort without a report.
fn append_panic_report(report: &str) {
    let Some(path) = path() else { return };
    // Truncate when near the cap so the report always fits (CappedFile's
    // truncate-and-restart, with headroom for this one write).
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() + report.len() as u64 > PANIC_HOOK_CAP_BYTES && File::create(&path).is_err()
        {
            return;
        }
    }
    let mut file = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let _ = file.write_all(report.as_bytes());
    let _ = file.sync_all();
}

impl CappedFile {
    pub fn open(path: PathBuf, cap: u64) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path,
            file,
            written,
            cap,
        })
    }
}

impl Write for CappedFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written.saturating_add(buf.len() as u64) > self.cap {
            // Truncate to empty and start over so we never exceed the cap.
            self.file = File::create(&self.path)?;
            self.written = 0;
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl CappedWriter {
    pub fn new(cf: CappedFile) -> Self {
        Self(Arc::new(Mutex::new(cf)))
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CappedWriter {
    type Writer = Guard<'a>;
    fn make_writer(&'a self) -> Self::Writer {
        Guard(self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Write for Guard<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
