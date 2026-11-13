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

/// Handle an OS file drop: if it landed over the active session's terminal
/// pane or over the docked SFTP panel, upload the file to that session's
/// current remote directory (#drag-onto-shell, #file-drop-sftp).
///
/// `hovered_pos` is the caller's best guess at the drop point in logical
/// window-client coordinates, taken from the most recent `CursorMoved` event
/// (see the `WEvent::DroppedFile` handler). Windows ignores it and queries
/// the OS cursor directly instead: Win32 suppresses `WM_MOUSEMOVE` for the
/// window while an OLE drag-and-drop is in progress, so `CursorMoved` can be
/// stale by the time the drop lands. macOS and X11/Wayland do deliver real
/// pointer motion during drag-hover as ordinary `CursorMoved` events, so the
/// last one seen is accurate there — this is what previously left
/// drag-and-drop upload a no-op on every non-Windows platform (#356).
#[cfg(windows)]
pub(crate) fn handle_file_drop(
    win: &AppWindow,
    sftp_handles: &SftpHandles,
    path: std::path::PathBuf,
    dock_stacks: &Rc<RefCell<DockStacks>>,
    _hovered_pos: Option<(f32, f32)>,
) {
    let active = win.get_active_tab_id().to_string();
    if active == "welcome" {
        return;
    }
    let w = win.window();
    let scale = w.scale_factor().max(0.01);
    let Some(inner) = w.with_winit_window(|ww| ww.inner_position().ok()).flatten() else {
        return;
    };
    let Some((cx, cy)) = cursor_pos() else {
        return;
    };
    // Drop point in logical client coordinates.
    let client_x = (cx - inner.x) as f32 / scale;
    let client_y = (cy - inner.y) as f32 / scale;
    // Accept drops anywhere over the whole terminal panel ("shell page") *and*
    // over the docked SFTP panel, so dragging a file onto either uploads it to
    // the session's current directory.
    if file_drop_target(win, dock_stacks, client_x, client_y).is_none() {
        return; // dropped outside both panels — ignore
    }
    upload_dropped_file(win, sftp_handles, path, &active);
}

#[cfg(not(windows))]
pub(crate) fn handle_file_drop(
    win: &AppWindow,
    sftp_handles: &SftpHandles,
    path: std::path::PathBuf,
    dock_stacks: &Rc<RefCell<DockStacks>>,
    hovered_pos: Option<(f32, f32)>,
) {
    let active = win.get_active_tab_id().to_string();
    if active == "welcome" {
        return;
    }
    let Some((client_x, client_y)) = hovered_pos else {
        // No CursorMoved was ever observed for this drag (e.g. the file was
        // dropped the instant it entered the window). Without a position we
        // cannot tell which panel it landed on, so do nothing rather than
        // guess — matches the previous no-op rather than uploading to the
        // wrong place.
        return;
    };
    // Same target rule as Windows: terminal panel or docked SFTP panel.
    if file_drop_target(win, dock_stacks, client_x, client_y).is_none() {
        return; // dropped outside both panels — ignore
    }
    upload_dropped_file(win, sftp_handles, path, &active);
}

/// Shared drop-upload body for both platform front-ends above.
pub(crate) fn upload_dropped_file(
    win: &AppWindow,
    sftp_handles: &SftpHandles,
    path: std::path::PathBuf,
    active: &str,
) {
    let dir = active_sftp_path(win, active);
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
    // The duplicate prompt mirrors the upload button's behaviour; a dropped
    // folder merges on the worker side, so it never prompts.
    let dedup = configured_dedup_policy(&win.as_weak());
    let known = terminal_sftp_entry_names(win, active);
    let decision = upload_decision(&path, &known, dedup, path.is_dir());
    if let Ok(handles) = sftp_handles.lock() {
        if let Some(h) = handles.get(active) {
            win.set_download_open(true);
            h.upload(path.clone(), dir, decision);
        }
        if sync {
            // The decision was made against the active tab's listing; mirrors
            // get their own live check in the worker instead.
            for (id, h) in handles.iter() {
                if id.as_str() == active {
                    continue;
                }
                if let Some(d) = other_dirs.get(id).filter(|d| !d.is_empty()) {
                    h.upload(path.clone(), d.clone(), UploadDecision::Proceed);
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

