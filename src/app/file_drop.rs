//! OS file-drop handling for the terminal pane and docked SFTP panel:
//! drop-target hit routing, upload and highlight. Split out of app.rs to
//! match the upstream module boundary (file_drop).

use super::hit_test::*;
use super::*;

/// Current mouse cursor position in physical screen pixels (Windows).
#[cfg(windows)]
pub(crate) fn cursor_pos() -> Option<(i32, i32)> {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    extern "system" {
        fn GetCursorPos(p: *mut Point) -> i32;
    }
    let mut p = Point { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut p) } != 0 {
        Some((p.x, p.y))
    } else {
        None
    }
}

/// The OS delivers a drop of several files / folders as one `DroppedFile`
/// event per item, all in the same burst. They are collected here and handled
/// together once the burst is over, so the drop asks its name-conflict
/// question once instead of once per item (#572).
#[derive(Default)]
pub(crate) struct PendingDrop {
    paths: Vec<std::path::PathBuf>,
    /// Drop point of the burst's first item, logical client coordinates.
    at: Option<(f32, f32)>,
}

/// How long after the first item of a drop the rest are waited for. The items
/// arrive back to back within one event-loop pass; this only has to outlast it.
const DROP_BURST: std::time::Duration = std::time::Duration::from_millis(60);

/// Queue one dropped path; the first of a burst schedules the batch.
pub(crate) fn queue_file_drop(
    win: &AppWindow,
    sftp_handles: &SftpHandles,
    pending: &Rc<RefCell<PendingDrop>>,
    dock_stacks: &Rc<RefCell<DockStacks>>,
    path: std::path::PathBuf,
    hovered_pos: Option<(f32, f32)>,
) {
    let first = {
        let mut batch = pending.borrow_mut();
        let first = batch.paths.is_empty();
        if first {
            batch.at = drop_point(win, hovered_pos);
        }
        batch.paths.push(path);
        first
    };
    if !first {
        return;
    }
    let weak = win.as_weak();
    let sftp_handles = sftp_handles.clone();
    let pending = pending.clone();
    let dock_stacks = dock_stacks.clone();
    slint::Timer::single_shot(DROP_BURST, move || {
        let batch = std::mem::take(&mut *pending.borrow_mut());
        let (Some(win), Some((x, y))) = (weak.upgrade(), batch.at) else {
            return;
        };
        handle_file_drop_at(&win, &sftp_handles, &dock_stacks, batch.paths, x, y);
    });
}

/// Where a drop landed, in logical window-client coordinates.
///
/// `hovered_pos` is the caller's best guess, taken from the most recent
/// `CursorMoved` event (see the `WEvent::DroppedFile` handler). Windows ignores
/// it and queries the OS cursor directly instead: Win32 suppresses
/// `WM_MOUSEMOVE` for the window while an OLE drag-and-drop is in progress, so
/// `CursorMoved` can be stale by the time the drop lands. macOS and
/// X11/Wayland do deliver real pointer motion during drag-hover as ordinary
/// `CursorMoved` events, so the last one seen is accurate there — this is what
/// previously left drag-and-drop upload a no-op on every non-Windows platform
/// (#356).
#[cfg(windows)]
fn drop_point(win: &AppWindow, _hovered_pos: Option<(f32, f32)>) -> Option<(f32, f32)> {
    let w = win.window();
    let scale = w.scale_factor().max(0.01);
    let inner = w.with_winit_window(|ww| ww.inner_position().ok()).flatten()?;
    let (cx, cy) = cursor_pos()?;
    Some(((cx - inner.x) as f32 / scale, (cy - inner.y) as f32 / scale))
}

/// `None` when no CursorMoved was ever observed for this drag (e.g. the file
/// was dropped the instant it entered the window): without a position we
/// cannot tell which panel it landed on, so the drop is ignored rather than
/// uploaded to the wrong place.
#[cfg(not(windows))]
fn drop_point(_win: &AppWindow, hovered_pos: Option<(f32, f32)>) -> Option<(f32, f32)> {
    hovered_pos
}

/// Handle one drop burst of any number of items at the recorded drop point.
///
/// Local behaviour kept: drops are accepted over the terminal panel *and* the
/// docked SFTP panel (#drag-onto-shell, #file-drop-sftp), and session-sync
/// mirrors each upload to every other online session (#sync). Upstream
/// behaviour adopted: the whole burst asks its name-conflict question once
/// (#572), and dropping onto the split view's local pane copies the files into
/// that local folder instead of uploading them.
pub(crate) fn handle_file_drop_at(
    win: &AppWindow,
    sftp_handles: &SftpHandles,
    dock_stacks: &Rc<RefCell<DockStacks>>,
    paths: Vec<std::path::PathBuf>,
    client_x: f32,
    client_y: f32,
) {
    if paths.is_empty() {
        return;
    }
    let active = win.get_active_tab_id().to_string();
    if active == "welcome" {
        return;
    }
    // Split view: dropping onto the *local* half copies the files into the
    // local folder that half is showing instead of uploading them to the
    // remote one.
    if let Some((tab, dir)) = sftp_local_pane_hit(win, client_x, client_y) {
        for path in &paths {
            copy_into(win, &tab, std::path::PathBuf::from(&dir), path.clone());
        }
        return;
    }
    // Accept drops anywhere over the whole terminal panel ("shell page") *and*
    // over the docked SFTP panel, so dragging a file onto either uploads it to
    // the session's current directory.
    if file_drop_target(win, dock_stacks, client_x, client_y).is_none() {
        return; // dropped outside both panels — ignore
    }
    let dir = active_sftp_path(win, &active);
    if dir.is_empty() {
        return;
    }
    // Session-sync (#sync): when both toggles are on, also mirror the drop to
    // every other online session — each into *its own* current SFTP dir. This
    // matches the upload button's behaviour (drag-and-drop is a separate path).
    let sync = win.get_sync_input() && win.get_sync_upload_enabled();
    let other_dirs = if sync {
        terminal_sftp_paths(win)
    } else {
        HashMap::new()
    };
    // Ask once, for the whole drop, about names that are already taken
    // remotely (#572). Returns one decision per path; `None` = cancelled.
    let Some(decisions) = resolve_upload_conflict(win, &active, &paths) else {
        set_sftp_status(win, &active, t("已取消上传", "Upload cancelled"));
        return;
    };
    if let Ok(handles) = sftp_handles.lock() {
        if let Some(h) = handles.get(&active) {
            win.set_download_open(true);
            for (path, decision) in paths.iter().zip(decisions) {
                h.upload(path.clone(), dir.clone(), decision);
            }
        }
        if sync {
            // The decisions were made against the active tab's listing;
            // mirrors get their own live check in the worker instead.
            for (id, h) in handles.iter() {
                if id.as_str() == active {
                    continue;
                }
                if let Some(d) = other_dirs.get(id).filter(|d| !d.is_empty()) {
                    for path in &paths {
                        h.upload(path.clone(), d.clone(), UploadDecision::Proceed);
                    }
                }
            }
        }
    }
}

/// The file-drop target under a client-space point, returned in dock-area
/// coordinates so the highlight overlay can draw it directly. Matches the
/// active session's terminal pane or the docked SFTP panel.
pub(crate) fn file_drop_target(
    win: &AppWindow,
    dock_stacks: &Rc<RefCell<DockStacks>>,
    x: f32,
    y: f32,
) -> Option<LogicalRect> {
    let frame = dock_area_frame(win);
    if let Some((_active, term, _state)) = active_terminal_panel_rects(win) {
        if contains_logical(term, x, y) {
            return Some(LogicalRect {
                x: term.x - frame.x,
                y: term.y - frame.y,
                w: term.w,
                h: term.h,
            });
        }
    }
    let Some(sftp) = sftp_panel_client_rect(win, dock_stacks) else {
        return None;
    };
    contains_logical(sftp, x, y).then(|| LogicalRect {
        x: sftp.x - frame.x,
        y: sftp.y - frame.y,
        w: sftp.w,
        h: sftp.h,
    })
}

/// (Re)draw the file-drop highlight for the current cursor position. Windows
/// stops delivering mouse motion while an OS drag is in flight, so this runs
/// from a timer while the drag is alive (#file-drop-hl).
#[cfg(windows)]
pub(crate) fn update_file_drop_highlight(win: &AppWindow, dock_stacks: &Rc<RefCell<DockStacks>>) {
    let Some((cx, cy)) = cursor_pos() else {
        win.set_file_drop_hl(false);
        return;
    };
    let w = win.window();
    let scale = w.scale_factor().max(0.01);
    let Some(inner) = w.with_winit_window(|ww| ww.inner_position().ok()).flatten() else {
        return;
    };
    let x = (cx - inner.x) as f32 / scale;
    let y = (cy - inner.y) as f32 / scale;
    match file_drop_target(win, dock_stacks, x, y) {
        Some(hit) => {
            win.set_file_drop_hl(true);
            win.set_file_drop_hl_x(hit.x);
            win.set_file_drop_hl_y(hit.y);
            win.set_file_drop_hl_w(hit.w);
            win.set_file_drop_hl_h(hit.h);
        }
        None => win.set_file_drop_hl(false),
    }
}

#[cfg(not(windows))]
pub(crate) fn update_file_drop_highlight(_win: &AppWindow, _dock_stacks: &Rc<RefCell<DockStacks>>) {}
