//! Drag remote SFTP files out to the OS file manager (#504).
//!
//! Windows: an OLE drag with *virtual files*. The data object offers
//! `FileGroupDescriptorW` (names, sizes, times, folders) and `FileContents`
//! (one `IStream` per file). Explorer creates the files itself, pulls their
//! bytes from our streams and shows its own copy-progress dialog; nothing is
//! staged in a temp folder first. The data object also implements
//! `IDataObjectAsyncCapability`, so Explorer copies on its own thread after
//! the drop instead of inside `DoDragDrop`.
//!
//! Threading: the data object lives on the UI thread (OLE drag-and-drop
//! needs the STA), but every `IStream` is marshalled from a dedicated MTA
//! thread, so Explorer's `Read` calls run on COM worker threads and a slow
//! network never blocks the UI. Bytes come from `SftpCommand::StreamFile`,
//! the folder listing from `SftpCommand::WalkForExport`.
//!
//! macOS: an AppKit dragging session of *file promises* (`drag_out_mac.rs`).
//! Finder picks where each dragged item goes and asks us to write it there;
//! [`PromiseDrag`] does the writing on a background queue, streaming the
//! same way as on Windows.
//!
//! Linux: not supported yet; the gesture does nothing there.

use crate::sftp::{DragTransfer, ExportEntry, SftpCommand};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// One dragged row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DragItem {
    /// Absolute remote path.
    pub remote: String,
    pub is_dir: bool,
}

impl DragItem {
    /// The name the item gets at the drop target.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn name(&self) -> &str {
        self.remote
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or("download")
    }
}

/// Transfers-panel label and byte total for a drag: the dragged item's
/// name, or "first +N" for several; total = all file sizes.
pub(crate) fn transfer_label(entries: &[ExportEntry]) -> (String, u64) {
    let roots: Vec<&str> = entries
        .iter()
        .filter(|e| !e.relative.contains('/'))
        .map(|e| e.relative.as_str())
        .collect();
    let name = match roots.as_slice() {
        [] => "…".to_string(),
        [one] => one.to_string(),
        [first, rest @ ..] => format!("{first} +{}", rest.len()),
    };
    let total = entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
    (name, total)
}

/// Max UTF-16 units in `FILEDESCRIPTORW::cFileName` (MAX_PATH incl. NUL).
pub(crate) const MAX_DESCRIPTOR_NAME: usize = 259;

/// Windows-safe relative path for a descriptor entry: `/` becomes `\`, each
/// component loses characters Windows forbids, and an over-long path keeps
/// its folders and shortens the last component (extension preserved).
pub(crate) fn descriptor_name(relative: &str) -> Vec<u16> {
    let parts: Vec<String> = relative
        .split('/')
        .filter(|p| !p.is_empty())
        .map(clean_component)
        .collect();
    let mut joined = parts.join("\\");
    let len = joined.encode_utf16().count();
    if len > MAX_DESCRIPTOR_NAME {
        let last = parts.last().cloned().unwrap_or_default();
        let prefix_len = len - last.encode_utf16().count();
        let room = MAX_DESCRIPTOR_NAME.saturating_sub(prefix_len).max(1);
        let (stem, ext) = match last.rfind('.') {
            Some(i) if i > 0 && last.len() - i <= 16 => (&last[..i], &last[i..]),
            _ => (last.as_str(), ""),
        };
        let keep = room.saturating_sub(ext.encode_utf16().count()).max(1);
        let short: String = stem.chars().take(keep).collect::<String>() + ext;
        let short: String = short.chars().take(room).collect();
        joined = joined[..joined.len() - last.len()].to_string() + &short;
    }
    joined.encode_utf16().take(MAX_DESCRIPTOR_NAME).collect()
}

fn clean_component(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim_end_matches([' ', '.']);
    let base = if trimmed.is_empty() { "_" } else { trimmed };
    let stem = base.split('.').next().unwrap_or(base).to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        format!("_{base}")
    } else {
        base.to_string()
    }
}

/// Unix seconds → Windows FILETIME (100 ns ticks since 1601-01-01).
pub(crate) fn unix_to_filetime(secs: u64) -> u64 {
    (secs + 11_644_473_600) * 10_000_000
}

/// Buffered, in-order reader over the chunks one `StreamFile` produces.
pub(crate) struct ChunkReader {
    buf: Vec<u8>,
    at: usize,
    done: bool,
}

/// Outcome of one [`ChunkReader::fill`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Fill {
    /// `n` bytes copied; `eof` once the stream has ended.
    Read {
        n: usize,
        eof: bool,
    },
    Failed(String),
}

impl ChunkReader {
    pub(crate) fn new() -> Self {
        Self {
            buf: Vec::new(),
            at: 0,
            done: false,
        }
    }

    /// The stream ended and every byte was handed out.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn at_eof(&self) -> bool {
        self.done && self.at >= self.buf.len()
    }

    /// Fill `out` as far as possible, pulling chunks from `next` (which
    /// blocks; `None` = end of stream). Short only at end of stream.
    pub(crate) fn fill(
        &mut self,
        out: &mut [u8],
        mut next: impl FnMut() -> Option<Result<Vec<u8>, String>>,
    ) -> Fill {
        let mut n = 0;
        while n < out.len() {
            if self.at < self.buf.len() {
                let take = (self.buf.len() - self.at).min(out.len() - n);
                out[n..n + take].copy_from_slice(&self.buf[self.at..self.at + take]);
                self.at += take;
                n += take;
                continue;
            }
            if self.done {
                break;
            }
            match next() {
                Some(Ok(chunk)) => {
                    self.buf = chunk;
                    self.at = 0;
                }
                Some(Err(error)) => return Fill::Failed(error),
                None => self.done = true,
            }
        }
        Fill::Read {
            n,
            eof: self.at_eof(),
        }
    }
}

/// Drag-out is available on this platform.
pub(crate) const SUPPORTED: bool = cfg!(any(windows, target_os = "macos"));

/// Start dragging `items` out of `window`. On Windows this blocks until the
/// drop or cancel; on macOS it returns once AppKit's dragging session has
/// started. Returns whether the drag started (Windows: was dropped).
#[cfg(windows)]
pub(crate) fn start(
    _window: &slint::Window,
    commands: UnboundedSender<SftpCommand>,
    items: Vec<DragItem>,
) -> bool {
    imp::start(
        commands,
        items.into_iter().map(|item| item.remote).collect(),
    )
}

#[cfg(target_os = "macos")]
pub(crate) fn start(
    window: &slint::Window,
    commands: UnboundedSender<SftpCommand>,
    items: Vec<DragItem>,
) -> bool {
    use i_slint_backend_winit::WinitWindowAccessor;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let view = window
        .with_winit_window(|ww| match ww.window_handle().ok()?.as_raw() {
            RawWindowHandle::AppKit(handle) => Some(handle.ns_view),
            _ => None,
        })
        .flatten();
    match view {
        Some(view) => mac::begin(view, commands, items),
        None => false,
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn start(
    _window: &slint::Window,
    _commands: UnboundedSender<SftpCommand>,
    _items: Vec<DragItem>,
) -> bool {
    false
}

#[cfg(target_os = "macos")]
#[path = "drag_out_mac.rs"]
mod mac;

/// How long writing a dropped item may wait for the folder walk.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const WALK_TIMEOUT: Duration = Duration::from_secs(120);
/// Bytes buffered ahead of the local file per stream: 64 × ~32 KB ≈ 2 MB.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const PROMISE_BUFFER_CHUNKS: usize = 64;

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum WalkState {
    Pending(mpsc::Receiver<Result<Vec<ExportEntry>, String>>),
    Done(Result<Vec<ExportEntry>, String>),
}

/// One drag of file promises (macOS): the drop target says where each
/// dragged item goes, and [`PromiseDrag::write`] creates it there. Shared by
/// every promise of the drag and called from background threads.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct PromiseDrag {
    /// Dropped once the drag is over, so a finished drag never keeps the
    /// SFTP worker's channel open.
    commands: Mutex<Option<UnboundedSender<SftpCommand>>>,
    walk: Mutex<WalkState>,
    /// This drag's Transfers-panel row, created by the first write.
    transfer: Mutex<Option<Arc<DragTransfer>>>,
    /// Promises not yet written.
    pending: AtomicUsize,
    walk_timeout: Duration,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl PromiseDrag {
    /// Start walking `items` right away; writes wait for what is left of it.
    pub(crate) fn start(
        commands: UnboundedSender<SftpCommand>,
        items: &[DragItem],
    ) -> Option<Self> {
        Self::with_timeout(commands, items, WALK_TIMEOUT)
    }

    fn with_timeout(
        commands: UnboundedSender<SftpCommand>,
        items: &[DragItem],
        walk_timeout: Duration,
    ) -> Option<Self> {
        if items.is_empty() {
            return None;
        }
        let (reply, walk) = mpsc::channel();
        let roots = items.iter().map(|item| item.remote.clone()).collect();
        commands
            .send(SftpCommand::WalkForExport { roots, reply })
            .ok()?;
        Some(Self {
            commands: Mutex::new(Some(commands)),
            walk: Mutex::new(WalkState::Pending(walk)),
            transfer: Mutex::new(None),
            pending: AtomicUsize::new(items.len()),
            walk_timeout,
        })
    }

    fn entries(&self) -> Result<Vec<ExportEntry>, String> {
        let mut walk = self.walk.lock().unwrap();
        if let WalkState::Pending(rx) = &*walk {
            let result = rx
                .recv_timeout(self.walk_timeout)
                .unwrap_or_else(|_| Err("listing the remote folder timed out".into()));
            *walk = WalkState::Done(result);
        }
        match &*walk {
            WalkState::Done(result) => result.clone(),
            WalkState::Pending(_) => unreachable!(),
        }
    }

    fn transfer_for(&self, entries: &[ExportEntry]) -> Arc<DragTransfer> {
        self.transfer
            .lock()
            .unwrap()
            .get_or_insert_with(|| {
                let (name, total) = transfer_label(entries);
                Arc::new(DragTransfer::new(
                    format!("drag-{}", uuid::Uuid::new_v4()),
                    name,
                    total,
                ))
            })
            .clone()
    }

    /// Create `item` at `dest` (the path the drop target chose for it): the
    /// file, or the folder and everything under it. A failed write removes
    /// the partial file it was writing. Always counts the promise as done.
    pub(crate) fn write(&self, item: &DragItem, dest: &Path) -> Result<(), String> {
        let result = self.write_item(item, dest);
        if let Err(error) = &result {
            tracing::warn!("drag-out: {} → {}: {error}", item.remote, dest.display());
            if let Some(transfer) = self.transfer.lock().unwrap().as_ref() {
                transfer.interrupted.store(true, Ordering::Relaxed);
            }
        }
        if self.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.finish();
        }
        result
    }

    fn write_item(&self, item: &DragItem, dest: &Path) -> Result<(), String> {
        let entries = self.entries()?;
        let commands = self
            .commands
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "the drag has ended".to_string())?;
        let root = entries
            .iter()
            .find(|entry| entry.remote == item.remote)
            .map(|entry| entry.relative.clone())
            .ok_or_else(|| format!("{} is no longer on the server", item.remote))?;
        let progress = self.transfer_for(&entries);
        materialise(dest, &root, &entries, |entry, file| {
            stream_into(&commands, entry, &progress, file)
        })
    }

    /// The drag is over: report the Transfers row (if any bytes were asked
    /// for) and let go of the SFTP worker. Safe to call more than once.
    pub(crate) fn finish(&self) {
        let commands = self.commands.lock().unwrap().take();
        let transfer = self.transfer.lock().unwrap().take();
        if let (Some(commands), Some(transfer)) = (commands, transfer) {
            let _ = commands.send(SftpCommand::FinishDragTransfer(transfer));
        }
    }
}

/// Stream one remote file into `file`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn stream_into(
    commands: &UnboundedSender<SftpCommand>,
    entry: &ExportEntry,
    progress: &Arc<DragTransfer>,
    file: &mut File,
) -> Result<(), String> {
    use std::io::Write;
    let (tx, mut rx) = tokio::sync::mpsc::channel(PROMISE_BUFFER_CHUNKS);
    commands
        .send(SftpCommand::StreamFile {
            remote: entry.remote.clone(),
            chunks: tx,
            progress: progress.clone(),
        })
        .map_err(|_| "the SFTP session has closed".to_string())?;
    // Called on a background queue, never inside the tokio runtime.
    while let Some(chunk) = rx.blocking_recv() {
        file.write_all(&chunk?).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Local path of walk entry `relative` when its dragged item `root` is
/// created at `dest`. `None` for entries of other items, or names that would
/// leave `dest` (`.`, `..`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn local_path_under(dest: &Path, root: &str, relative: &str) -> Option<PathBuf> {
    let rest = if relative == root {
        ""
    } else {
        relative.strip_prefix(root)?.strip_prefix('/')?
    };
    let mut path = dest.to_path_buf();
    for part in rest.split('/').filter(|part| !part.is_empty()) {
        if part == "." || part == ".." || part.contains('\0') {
            return None;
        }
        path.push(part);
    }
    Some(path)
}

/// Create dragged item `root` at `dest` from the walk `entries`, writing each
/// file's bytes with `copy`. Entries of other dragged items are skipped.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn materialise(
    dest: &Path,
    root: &str,
    entries: &[ExportEntry],
    mut copy: impl FnMut(&ExportEntry, &mut File) -> Result<(), String>,
) -> Result<(), String> {
    let io = |error: std::io::Error| error.to_string();
    let mut found = false;
    for entry in entries {
        let Some(path) = local_path_under(dest, root, &entry.relative) else {
            continue;
        };
        found = true;
        if entry.is_dir {
            fs::create_dir_all(&path).map_err(io)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io)?;
        }
        let mut file = File::create(&path).map_err(io)?;
        if let Err(error) = copy(entry, &mut file) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        if entry.mtime > 0 {
            let _ = file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(entry.mtime));
        }
    }
    if found {
        Ok(())
    } else {
        Err(format!("nothing to create for {root}"))
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::mem::ManuallyDrop;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex, OnceLock};
    use std::time::Duration;

    use crate::sftp::DragTransfer;

    use windows::core::{implement, w, Interface, HRESULT};
    use windows::Win32::Foundation::{
        BOOL, DATA_S_SAMEFORMATETC, DV_E_FORMATETC, DV_E_LINDEX, E_FAIL, E_NOTIMPL, E_POINTER,
        FILETIME, HGLOBAL, OLE_E_ADVISENOTSUPPORTED, STG_E_ACCESSDENIED, STG_E_INVALIDFUNCTION,
        STG_E_READFAULT, S_FALSE, S_OK,
    };
    use windows::Win32::System::Com::Marshal::CoMarshalInterThreadInterfaceInStream;
    use windows::Win32::System::Com::StructuredStorage::CoGetInterfaceAndReleaseStream;
    use windows::Win32::System::Com::{
        CoInitializeEx, CoTaskMemAlloc, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC,
        IEnumSTATDATA, ISequentialStream_Impl, IStream, IStream_Impl, COINIT_MULTITHREADED,
        DATADIR_GET, DVASPECT_CONTENT, FORMATETC, LOCKTYPE, STATFLAG, STATFLAG_NONAME, STATSTG,
        STGC, STGMEDIUM, STGMEDIUM_0, STGTY_STREAM, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END,
        STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
    };
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
    };
    use windows::Win32::System::Ole::{
        DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, DROPEFFECT, DROPEFFECT_COPY,
        DROPEFFECT_NONE,
    };
    use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
    use windows::Win32::UI::Shell::{
        IDataObjectAsyncCapability, IDataObjectAsyncCapability_Impl, SHCreateStdEnumFmtEtc,
        FILEDESCRIPTORW, FILEGROUPDESCRIPTORW,
    };

    const DRAGDROP_S_DROP: HRESULT = HRESULT(0x0004_0100);
    const DRAGDROP_S_CANCEL: HRESULT = HRESULT(0x0004_0101);
    const DRAGDROP_S_USEDEFAULTCURSORS: HRESULT = HRESULT(0x0004_0102);

    // FILEDESCRIPTOR flags (shlobj_core.h).
    const FD_ATTRIBUTES: u32 = 0x0000_0004;
    const FD_WRITESTIME: u32 = 0x0000_0020;
    const FD_FILESIZE: u32 = 0x0000_0040;
    const FD_PROGRESSUI: u32 = 0x0000_4000;
    const FD_UNICODE: u32 = 0x8000_0000;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

    /// Bytes buffered ahead of Explorer per file: 64 × ~32 KB ≈ 2 MB.
    const STREAM_BUFFER_CHUNKS: usize = 64;
    /// How long a drop may wait for a folder walk before giving up.
    const WALK_TIMEOUT: Duration = Duration::from_secs(120);

    struct Formats {
        descriptor: u16,
        contents: u16,
        preferred_effect: u16,
    }

    fn formats() -> &'static Formats {
        static FORMATS: OnceLock<Formats> = OnceLock::new();
        FORMATS.get_or_init(|| unsafe {
            Formats {
                descriptor: RegisterClipboardFormatW(w!("FileGroupDescriptorW")) as u16,
                contents: RegisterClipboardFormatW(w!("FileContents")) as u16,
                preferred_effect: RegisterClipboardFormatW(w!("Preferred DropEffect")) as u16,
            }
        })
    }

    fn format(cf: u16, tymed: u32, lindex: i32) -> FORMATETC {
        FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex,
            tymed,
        }
    }

    pub(super) fn start(commands: UnboundedSender<SftpCommand>, roots: Vec<String>) -> bool {
        if roots.is_empty() {
            return false;
        }
        // winit already initialises OLE for its drop target; this is a
        // harmless ref-count bump if so.
        unsafe {
            let _ = OleInitialize(None);
        }
        // Walk folders in the background right away; the drop only waits
        // for whatever is still left of it.
        let (reply, walk) = mpsc::channel();
        if commands
            .send(SftpCommand::WalkForExport { roots, reply })
            .is_err()
        {
            return false;
        }
        let data: IDataObject = DragData {
            commands,
            walk: Mutex::new(Walk::Pending(walk)),
            async_mode: AtomicBool::new(true),
            in_operation: AtomicBool::new(false),
            transfer: Mutex::new(None),
        }
        .into();
        let source: IDropSource = DropSource.into();
        let mut effect = DROPEFFECT_NONE;
        let hr = unsafe { DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) };
        hr == DRAGDROP_S_DROP && effect != DROPEFFECT_NONE
    }

    // --- Drop source ------------------------------------------------------

    #[implement(IDropSource)]
    struct DropSource;

    impl IDropSource_Impl for DropSource_Impl {
        fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
            if escape.as_bool() {
                DRAGDROP_S_CANCEL
            } else if keys.0 & MK_LBUTTON.0 == 0 {
                DRAGDROP_S_DROP
            } else {
                S_OK
            }
        }

        fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
            DRAGDROP_S_USEDEFAULTCURSORS
        }
    }

    // --- Data object --------------------------------------------------------

    enum Walk {
        Pending(mpsc::Receiver<Result<Vec<ExportEntry>, String>>),
        Done(Result<Vec<ExportEntry>, String>),
    }

    #[implement(IDataObject, IDataObjectAsyncCapability)]
    struct DragData {
        commands: UnboundedSender<SftpCommand>,
        walk: Mutex<Walk>,
        async_mode: AtomicBool,
        in_operation: AtomicBool,
        /// This drag's Transfers-panel row, created on the first file
        /// Explorer asks for; `None` again once finished.
        transfer: Mutex<Option<Arc<DragTransfer>>>,
    }

    /// Explorer released the data object: the copy is over (or never
    /// started), so report the row's final state.
    impl Drop for DragData {
        fn drop(&mut self) {
            self.finish_transfer();
        }
    }

    impl DragData {
        fn transfer_for(&self, entries: &[ExportEntry]) -> Arc<DragTransfer> {
            self.transfer
                .lock()
                .unwrap()
                .get_or_insert_with(|| {
                    let (name, total) = transfer_label(entries);
                    Arc::new(DragTransfer::new(
                        format!("drag-{}", uuid::Uuid::new_v4()),
                        name,
                        total,
                    ))
                })
                .clone()
        }

        fn finish_transfer(&self) {
            if let Some(transfer) = self.transfer.lock().unwrap().take() {
                let _ = self
                    .commands
                    .send(SftpCommand::FinishDragTransfer(transfer));
            }
        }

        /// The walked entries, waiting for the walk the first time.
        fn entries(&self) -> windows::core::Result<Vec<ExportEntry>> {
            let mut walk = self.walk.lock().unwrap();
            if let Walk::Pending(rx) = &*walk {
                let result = rx
                    .recv_timeout(WALK_TIMEOUT)
                    .unwrap_or_else(|_| Err("listing the remote folder timed out".into()));
                *walk = Walk::Done(result);
            }
            match &*walk {
                Walk::Done(Ok(entries)) if !entries.is_empty() => Ok(entries.clone()),
                Walk::Done(Ok(_)) => Err(E_FAIL.into()),
                Walk::Done(Err(error)) => {
                    tracing::warn!("drag-out: {error}");
                    Err(E_FAIL.into())
                }
                Walk::Pending(_) => unreachable!(),
            }
        }

        fn offers(fmt: &FORMATETC) -> bool {
            let f = formats();
            (fmt.cfFormat == f.descriptor && fmt.tymed & TYMED_HGLOBAL.0 as u32 != 0)
                || (fmt.cfFormat == f.contents && fmt.tymed & TYMED_ISTREAM.0 as u32 != 0)
                || (fmt.cfFormat == f.preferred_effect && fmt.tymed & TYMED_HGLOBAL.0 as u32 != 0)
        }
    }

    fn hglobal_from(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
        unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len())?;
            let ptr = GlobalLock(h) as *mut u8;
            if ptr.is_null() {
                return Err(E_FAIL.into());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            let _ = GlobalUnlock(h);
            Ok(h)
        }
    }

    fn descriptor_bytes(entries: &[ExportEntry]) -> Vec<u8> {
        let header = std::mem::offset_of!(FILEGROUPDESCRIPTORW, fgd);
        let item = std::mem::size_of::<FILEDESCRIPTORW>();
        let mut bytes = vec![0u8; header + item * entries.len()];
        bytes[..4].copy_from_slice(&(entries.len() as u32).to_ne_bytes());
        for (i, entry) in entries.iter().enumerate() {
            let mut fd = FILEDESCRIPTORW::default();
            fd.dwFlags = FD_ATTRIBUTES | FD_UNICODE | FD_PROGRESSUI;
            if entry.is_dir {
                fd.dwFileAttributes = FILE_ATTRIBUTE_DIRECTORY;
            } else {
                fd.dwFileAttributes = FILE_ATTRIBUTE_NORMAL;
                fd.dwFlags |= FD_FILESIZE;
                fd.nFileSizeHigh = (entry.size >> 32) as u32;
                fd.nFileSizeLow = entry.size as u32;
            }
            if entry.mtime > 0 {
                let t = unix_to_filetime(entry.mtime);
                fd.ftLastWriteTime = FILETIME {
                    dwLowDateTime: t as u32,
                    dwHighDateTime: (t >> 32) as u32,
                };
                fd.dwFlags |= FD_WRITESTIME;
            }
            // FILEDESCRIPTORW is packed: assign the array whole, no references.
            let name = descriptor_name(&entry.relative);
            let mut file_name = [0u16; 260];
            file_name[..name.len()].copy_from_slice(&name);
            fd.cFileName = file_name;
            let start = header + i * item;
            // SAFETY: FILEDESCRIPTORW is plain old data.
            let raw = unsafe {
                std::slice::from_raw_parts(&fd as *const FILEDESCRIPTORW as *const u8, item)
            };
            bytes[start..start + item].copy_from_slice(raw);
        }
        bytes
    }

    impl IDataObject_Impl for DragData_Impl {
        fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
            let fmt = unsafe { pformatetcin.as_ref() }.ok_or(E_POINTER)?;
            let f = formats();
            if fmt.cfFormat == f.descriptor && fmt.tymed & TYMED_HGLOBAL.0 as u32 != 0 {
                let entries = self.entries()?;
                let h = hglobal_from(&descriptor_bytes(&entries))?;
                return Ok(STGMEDIUM {
                    tymed: TYMED_HGLOBAL.0 as u32,
                    u: STGMEDIUM_0 { hGlobal: h },
                    pUnkForRelease: ManuallyDrop::new(None),
                });
            }
            if fmt.cfFormat == f.contents && fmt.tymed & TYMED_ISTREAM.0 as u32 != 0 {
                let entries = self.entries()?;
                let entry = usize::try_from(fmt.lindex)
                    .ok()
                    .and_then(|i| entries.get(i))
                    .filter(|e| !e.is_dir)
                    .ok_or(DV_E_LINDEX)?;
                let progress = self.transfer_for(&entries);
                let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BUFFER_CHUNKS);
                self.commands
                    .send(SftpCommand::StreamFile {
                        remote: entry.remote.clone(),
                        chunks: tx,
                        progress: progress.clone(),
                    })
                    .map_err(|_| E_FAIL)?;
                let stream = RemoteStream {
                    state: Mutex::new(StreamState {
                        rx,
                        reader: ChunkReader::new(),
                        pos: 0,
                        seek: 0,
                    }),
                    size: entry.size,
                    name: entry
                        .relative
                        .rsplit('/')
                        .next()
                        .unwrap_or("file")
                        .to_string(),
                    progress,
                };
                let proxy = stream_proxy(stream)?;
                return Ok(STGMEDIUM {
                    tymed: TYMED_ISTREAM.0 as u32,
                    u: STGMEDIUM_0 {
                        pstm: ManuallyDrop::new(Some(proxy)),
                    },
                    pUnkForRelease: ManuallyDrop::new(None),
                });
            }
            if fmt.cfFormat == f.preferred_effect && fmt.tymed & TYMED_HGLOBAL.0 as u32 != 0 {
                let h = hglobal_from(&DROPEFFECT_COPY.0.to_ne_bytes())?;
                return Ok(STGMEDIUM {
                    tymed: TYMED_HGLOBAL.0 as u32,
                    u: STGMEDIUM_0 { hGlobal: h },
                    pUnkForRelease: ManuallyDrop::new(None),
                });
            }
            Err(DV_E_FORMATETC.into())
        }

        fn GetDataHere(
            &self,
            _pformatetc: *const FORMATETC,
            _pmedium: *mut STGMEDIUM,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
            match unsafe { pformatetc.as_ref() } {
                Some(fmt) if DragData::offers(fmt) => S_OK,
                Some(_) => DV_E_FORMATETC,
                None => E_POINTER,
            }
        }

        fn GetCanonicalFormatEtc(
            &self,
            _pformatectin: *const FORMATETC,
            pformatetcout: *mut FORMATETC,
        ) -> HRESULT {
            if let Some(out) = unsafe { pformatetcout.as_mut() } {
                out.ptd = std::ptr::null_mut();
            }
            DATA_S_SAMEFORMATETC
        }

        fn SetData(
            &self,
            _pformatetc: *const FORMATETC,
            _pmedium: *const STGMEDIUM,
            _frelease: BOOL,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
            if dwdirection != DATADIR_GET.0 as u32 {
                return Err(E_NOTIMPL.into());
            }
            let f = formats();
            let list = [
                format(f.descriptor, TYMED_HGLOBAL.0 as u32, -1),
                format(f.contents, TYMED_ISTREAM.0 as u32, -1),
                format(f.preferred_effect, TYMED_HGLOBAL.0 as u32, -1),
            ];
            unsafe { SHCreateStdEnumFmtEtc(&list) }
        }

        fn DAdvise(
            &self,
            _pformatetc: *const FORMATETC,
            _advf: u32,
            _padvsink: Option<&IAdviseSink>,
        ) -> windows::core::Result<u32> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }

        fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }

        fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }
    }

    impl IDataObjectAsyncCapability_Impl for DragData_Impl {
        fn SetAsyncMode(&self, fdoopasync: BOOL) -> windows::core::Result<()> {
            self.async_mode
                .store(fdoopasync.as_bool(), Ordering::Relaxed);
            Ok(())
        }
        fn GetAsyncMode(&self) -> windows::core::Result<BOOL> {
            Ok(self.async_mode.load(Ordering::Relaxed).into())
        }
        fn StartOperation(
            &self,
            _pbcreserved: Option<&windows::Win32::System::Com::IBindCtx>,
        ) -> windows::core::Result<()> {
            self.in_operation.store(true, Ordering::Relaxed);
            Ok(())
        }
        fn InOperation(&self) -> windows::core::Result<BOOL> {
            Ok(self.in_operation.load(Ordering::Relaxed).into())
        }
        fn EndOperation(
            &self,
            _hresult: HRESULT,
            _pbcreserved: Option<&windows::Win32::System::Com::IBindCtx>,
            _dweffects: u32,
        ) -> windows::core::Result<()> {
            self.in_operation.store(false, Ordering::Relaxed);
            // Async extraction is over even if Explorer keeps a reference.
            self.finish_transfer();
            Ok(())
        }
    }

    // --- File contents stream ----------------------------------------------

    struct StreamState {
        rx: tokio::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
        reader: ChunkReader,
        /// Bytes handed to Explorer so far.
        pos: u64,
        /// Position reported by `Seek`; reads only work where `pos` is.
        seek: u64,
    }

    #[implement(IStream)]
    struct RemoteStream {
        state: Mutex<StreamState>,
        size: u64,
        name: String,
        progress: Arc<DragTransfer>,
    }

    /// Explorer let go of the file: if it stopped short of the end, the copy
    /// was cancelled (or the file skipped), which the final row reports.
    impl Drop for RemoteStream {
        fn drop(&mut self) {
            let state = self.state.get_mut().unwrap();
            if state.pos < self.size && !state.reader.at_eof() {
                self.progress.interrupted.store(true, Ordering::Relaxed);
            }
        }
    }

    impl ISequentialStream_Impl for RemoteStream_Impl {
        fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
            if pv.is_null() {
                return E_POINTER;
            }
            let mut state = self.state.lock().unwrap();
            if state.seek != state.pos {
                return STG_E_INVALIDFUNCTION;
            }
            // SAFETY: COM guarantees `pv` points at `cb` writable bytes.
            let out = unsafe { std::slice::from_raw_parts_mut(pv as *mut u8, cb as usize) };
            let StreamState { rx, reader, .. } = &mut *state;
            match reader.fill(out, || rx.blocking_recv()) {
                Fill::Read { n, eof } => {
                    state.pos += n as u64;
                    state.seek = state.pos;
                    if let Some(read) = unsafe { pcbread.as_mut() } {
                        *read = n as u32;
                    }
                    if eof && n < cb as usize {
                        S_FALSE
                    } else {
                        S_OK
                    }
                }
                Fill::Failed(error) => {
                    tracing::warn!("drag-out: reading {} failed: {error}", self.name);
                    STG_E_READFAULT
                }
            }
        }

        fn Write(&self, _pv: *const core::ffi::c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
            STG_E_ACCESSDENIED
        }
    }

    impl IStream_Impl for RemoteStream_Impl {
        fn Seek(
            &self,
            dlibmove: i64,
            dworigin: STREAM_SEEK,
            plibnewposition: *mut u64,
        ) -> windows::core::Result<()> {
            // Forward-only stream: positions can be *reported* anywhere (some
            // callers probe the size with Seek(0, END) and come back), but a
            // Read only works at the real position.
            let mut state = self.state.lock().unwrap();
            let base = match dworigin {
                STREAM_SEEK_SET => 0i128,
                STREAM_SEEK_CUR => state.seek as i128,
                STREAM_SEEK_END => self.size as i128,
                _ => return Err(STG_E_INVALIDFUNCTION.into()),
            };
            let target = base + dlibmove as i128;
            if target < 0 {
                return Err(STG_E_INVALIDFUNCTION.into());
            }
            state.seek = target as u64;
            if let Some(out) = unsafe { plibnewposition.as_mut() } {
                *out = state.seek;
            }
            Ok(())
        }

        fn SetSize(&self, _libnewsize: u64) -> windows::core::Result<()> {
            Err(STG_E_ACCESSDENIED.into())
        }

        fn CopyTo(
            &self,
            _pstm: Option<&IStream>,
            _cb: u64,
            _pcbread: *mut u64,
            _pcbwritten: *mut u64,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Commit(&self, _grfcommitflags: &STGC) -> windows::core::Result<()> {
            Ok(())
        }

        fn Revert(&self) -> windows::core::Result<()> {
            Ok(())
        }

        fn LockRegion(&self, _off: u64, _cb: u64, _ty: &LOCKTYPE) -> windows::core::Result<()> {
            Err(STG_E_INVALIDFUNCTION.into())
        }

        fn UnlockRegion(&self, _off: u64, _cb: u64, _ty: u32) -> windows::core::Result<()> {
            Err(STG_E_INVALIDFUNCTION.into())
        }

        fn Stat(
            &self,
            pstatstg: *mut STATSTG,
            grfstatflag: &STATFLAG,
        ) -> windows::core::Result<()> {
            let out = unsafe { pstatstg.as_mut() }.ok_or(E_POINTER)?;
            *out = STATSTG::default();
            out.r#type = STGTY_STREAM.0 as _;
            out.cbSize = self.size;
            if grfstatflag.0 & STATFLAG_NONAME.0 == 0 {
                let wide: Vec<u16> = self.name.encode_utf16().chain([0]).collect();
                unsafe {
                    let p = CoTaskMemAlloc(wide.len() * 2) as *mut u16;
                    if !p.is_null() {
                        std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                        out.pwcsName = windows::core::PWSTR(p);
                    }
                }
            }
            Ok(())
        }

        fn Clone(&self) -> windows::core::Result<IStream> {
            Err(E_NOTIMPL.into())
        }
    }

    // --- MTA hosting ---------------------------------------------------------

    type Job = Box<dyn FnOnce() + Send>;

    /// A thread that stays in the multithreaded apartment for the life of
    /// the process. Streams marshalled from here are called on COM worker
    /// threads, never the UI thread.
    fn mta() -> &'static Mutex<mpsc::Sender<Job>> {
        static MTA: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
        MTA.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("meatshell-com-mta".into())
                .spawn(move || {
                    unsafe {
                        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                    }
                    for job in rx {
                        job();
                    }
                })
                .expect("spawn COM MTA thread");
            Mutex::new(tx)
        })
    }

    /// Hand `stream` to the MTA and return a proxy for it usable here.
    fn stream_proxy(stream: RemoteStream) -> windows::core::Result<IStream> {
        let (tx, rx) = mpsc::sync_channel::<Result<usize, HRESULT>>(1);
        mta()
            .lock()
            .unwrap()
            .send(Box::new(move || {
                let result = unsafe {
                    let stream: IStream = stream.into();
                    CoMarshalInterThreadInterfaceInStream(&IStream::IID, &stream)
                        .map(|marshalled| marshalled.into_raw() as usize)
                        .map_err(|e| e.code())
                };
                let _ = tx.send(result);
            }))
            .map_err(|_| E_FAIL)?;
        let raw = rx.recv().map_err(|_| E_FAIL)??;
        unsafe {
            let marshalled = IStream::from_raw(raw as *mut core::ffi::c_void);
            let proxy: windows::core::Result<IStream> = CoGetInterfaceAndReleaseStream(&marshalled);
            // CoGetInterfaceAndReleaseStream released the marshalling stream.
            std::mem::forget(marshalled);
            proxy
        }
    }
}

// [attic] 该文件与测试均已移出编译单元（见 src/attic/README.md）。
// 路径随移动一并更正；本模块当前无人声明，故不参与编译。
#[cfg(test)]
#[path = "tests/drag_out.rs"]
mod tests;
