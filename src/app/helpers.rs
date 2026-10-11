//! Small free-standing helpers shared across the app module.

/// `app_cursor` mirrors the remote terminal's DECCKM mode (`\x1b[?1h/l`):
/// when true the four arrow keys must use SS3 sequences (`\x1bOA`…) instead
/// of the default CSI sequences (`\x1b[A`…).  Full-screen apps like nano and
/// vim set this mode on startup.
/// Write `text` to the system clipboard. Call from a dedicated thread, never the
/// UI thread (arboard pumps the Win32 message loop / blocks).
///
/// On Linux the clipboard selection only persists while the owning client stays
/// alive, so we use arboard's `set().wait()`, which blocks this thread until
/// another app takes ownership — otherwise the copied text vanishes the moment
/// the `Clipboard` handle is dropped. Combined with the `wayland-data-control`
/// feature this is also what makes copy work on Wayland sessions (issue #47).
pub(super) fn clipboard_set_text(text: String) {
    #[cfg(target_os = "linux")]
    let result = {
        use arboard::SetExtLinux as _;
        arboard::Clipboard::new().and_then(|mut cb| cb.set().wait().text(text))
    };
    #[cfg(not(target_os = "linux"))]
    let result = arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text));
    if let Err(e) = result {
        tracing::warn!("clipboard set_text error: {}", e);
    }
}

/// Split a stored proxy URL into `(type, host:port)` for the session dialog.
///
/// `""` → `("none", "")`. Recognises `socks5`/`socks5h`/`socks` and
/// `http`/`https` scheme prefixes. A value without a (recognised) scheme is
/// treated as SOCKS5, matching proxy.rs's parse default, so older configs that
/// stored a bare `host:port` keep working.
/// Parse a "vX.Y.Z" / "X.Y.Z" tag into a comparable tuple, or None if it isn't
/// a three-part numeric version. A pre-release suffix on the patch (e.g.
/// "3-rc1") is tolerated by taking its leading digits (#48).
pub(super) fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let mut it = s.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor, patch))
}

pub(super) fn split_proxy(url: &str) -> (String, String) {
    let s = url.trim();
    if s.is_empty() {
        return ("none".to_string(), String::new());
    }
    let lower = s.to_ascii_lowercase();
    for p in ["http://", "https://"] {
        if lower.starts_with(p) {
            return (
                "http".to_string(),
                s[p.len()..].trim_end_matches('/').to_string(),
            );
        }
    }
    for p in ["socks5h://", "socks5://", "socks://"] {
        if lower.starts_with(p) {
            return (
                "socks5".to_string(),
                s[p.len()..].trim_end_matches('/').to_string(),
            );
        }
    }
    ("socks5".to_string(), s.trim_end_matches('/').to_string())
}

pub(super) fn parent_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
        None => "/".to_string(),
    }
}

/// Owner window for native (rfd) file and message dialogs.
///
/// On Windows an unowned dialog can never appear above a window that is kept
/// on top (#450), so a picker opened from a pinned window would open hidden
/// behind it. Owned dialogs stay above their owner. Capture the owner on the
/// UI thread with [`DialogOwner::of`]; it is `Copy + Send`, so dialogs started
/// from worker threads can use it too. Other platforms keep unowned dialogs:
/// a macOS owner would turn them into sheets, and their modal panels already
/// sit above floating windows.
#[derive(Clone, Copy, Default)]
pub(super) struct DialogOwner {
    #[cfg(windows)]
    handles: Option<(
        raw_window_handle::RawWindowHandle,
        raw_window_handle::RawDisplayHandle,
    )>,
}

// SAFETY: only the raw HWND/display handles are stored and handed to rfd,
// which itself treats them as `Send`; dialogs are opened while the owning
// window is alive.
#[cfg(windows)]
unsafe impl Send for DialogOwner {}

impl DialogOwner {
    pub(super) fn of(window: &slint::Window) -> Self {
        #[cfg(windows)]
        {
            use i_slint_backend_winit::WinitWindowAccessor;
            use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
            let handles = window
                .with_winit_window(|ww| {
                    Some((
                        ww.window_handle().ok()?.as_raw(),
                        ww.display_handle().ok()?.as_raw(),
                    ))
                })
                .flatten();
            Self { handles }
        }
        #[cfg(not(windows))]
        {
            let _ = window;
            Self::default()
        }
    }

    /// Owner for a dialog opened from a window callback; unowned if the
    /// window is already gone.
    pub(super) fn of_weak<T: slint::ComponentHandle>(weak: &slint::Weak<T>) -> Self {
        weak.upgrade()
            .map(|w| Self::of(w.window()))
            .unwrap_or_default()
    }

    pub(super) fn file(&self) -> rfd::FileDialog {
        let dialog = rfd::FileDialog::new();
        #[cfg(windows)]
        if self.handles.is_some() {
            return dialog.set_parent(self);
        }
        dialog
    }

    pub(super) fn message(&self) -> rfd::MessageDialog {
        let dialog = rfd::MessageDialog::new();
        #[cfg(windows)]
        if self.handles.is_some() {
            return dialog.set_parent(self);
        }
        dialog
    }
}

#[cfg(windows)]
impl raw_window_handle::HasWindowHandle for DialogOwner {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let (window, _) = self
            .handles
            .ok_or(raw_window_handle::HandleError::Unavailable)?;
        // SAFETY: the handle came from a live winit window (see `of`).
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(window) })
    }
}

#[cfg(windows)]
impl raw_window_handle::HasDisplayHandle for DialogOwner {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        let (_, display) = self
            .handles
            .ok_or(raw_window_handle::HandleError::Unavailable)?;
        // SAFETY: as above.
        Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(display) })
    }
}
