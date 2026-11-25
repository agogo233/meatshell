//! Process, system-info and editor auxiliary window callbacks.
//!
//! The process / system-info monitor callbacks are wired inline in `app.rs`
//! (single-file equivalents), so only the editor chrome and the always-on-top
//! propagation are called from here.

use super::*;


/// Editor window drag and resize callbacks. The OS close button goes through
/// the unsaved-changes check; the actual close (`close-editor`) is wired with
/// the other editor logic in `editor_host::wire_editor_host`.
pub(super) fn wire_editor_window_chrome(ctx: &WinCtx) {
    let WinCtx { editor_win, .. } = ctx;
    {
        let close_request = editor_win.as_weak();
        editor_win.window().on_close_requested(move || {
            if let Some(editor) = close_request.upgrade() {
                editor.invoke_request_close();
            }
            slint::CloseRequestResponse::KeepWindowShown
        });
    }
    {
        let weak = editor_win.as_weak();
        editor_win.on_win_drag(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_window();
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        use i_slint_backend_winit::winit::window::ResizeDirection;
        let weak = editor_win.as_weak();
        editor_win.on_win_resize_se(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_resize_window(ResizeDirection::SouthEast);
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
}

/// Always-on-top pin (#450): a pinned main window would otherwise cover its own
/// editor and monitor windows, so they follow its pin state.
pub(super) fn wire_pin_on_top(ctx: &WinCtx) {
    let WinCtx {
        window,
        proc_win,
        sys_win,
        editor_win,
        ..
    } = ctx;
    let proc_weak = proc_win.as_weak();
    let sys_weak = sys_win.as_weak();
    let editor_weak = editor_win.as_weak();
    window.on_pinned_changed(move |pinned| {
        if let Some(w) = proc_weak.upgrade() {
            w.set_pinned(pinned);
        }
        if let Some(w) = sys_weak.upgrade() {
            w.set_pinned(pinned);
        }
        if let Some(w) = editor_weak.upgrade() {
            w.set_pinned(pinned);
        }
    });
}
