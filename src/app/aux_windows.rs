//! Process, system-info and editor auxiliary window callbacks.

use super::*;

/// Process / system-info monitor window callbacks and the main window resize grip.
pub(super) fn wire_monitor_windows(ctx: &WinCtx) {
    let WinCtx {
        window,
        proc_win,
        sys_win,
        ..
    } = ctx;
    {
        // ✕ hides the window (data keeps flowing into the shared model).
        let weak = proc_win.as_weak();
        let main_weak = window.as_weak();
        proc_win.on_close(move || {
            if let Some(main) = main_weak.upgrade() {
                main.set_process_window_open(false);
            }
            if let Some(w) = weak.upgrade() {
                let _ = w.hide();
            }
        });
    }
    {
        proc_win.on_copy_pid(move |pid: SharedString| {
            let text = pid.to_string();
            std::thread::spawn(move || clipboard_set_text(text));
        });
    }
    {
        // Frameless titlebar drag, via winit on the process window's own handle.
        let weak = proc_win.as_weak();
        proc_win.on_win_drag(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_window();
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        // Bottom-right resize grip.
        use i_slint_backend_winit::winit::window::ResizeDirection;
        let weak = proc_win.as_weak();
        proc_win.on_win_resize_se(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_resize_window(ResizeDirection::SouthEast);
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        // Bottom-right resize grip on the main window (frameless mode only).
        // #main-resize-grip: mirrors proc_window's grip so the main window
        // gets the same OS-drag-resize-from-corner behavior when the OS title
        // bar is hidden (custom-titlebar mode on Windows/Linux).
        use i_slint_backend_winit::winit::window::ResizeDirection;
        let weak = window.as_weak();
        window.on_win_resize_se(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_resize_window(ResizeDirection::SouthEast);
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        // The sidebar "Processes" button shows / focuses the window.
        let win_weak = window.as_weak();
        let proc_weak = proc_win.as_weak();
        window.on_open_processes(move || {
            let (Some(main), Some(pw)) = (win_weak.upgrade(), proc_weak.upgrade()) else {
                return;
            };
            main.set_process_window_open(true);
            main.invoke_refresh_sidebar();
            pw.set_host(main.get_connection_state());
            sync_proc_theme(&main, &pw);
            let _ = pw.show();
            place_process_window(&main, &pw);
            pw.window().with_winit_window(|ww| ww.focus_window());
        });
    }
    {
        let weak = sys_win.as_weak();
        let main_weak = window.as_weak();
        sys_win.on_close(move || {
            if let Some(main) = main_weak.upgrade() {
                main.set_system_info_window_open(false);
            }
            if let Some(w) = weak.upgrade() {
                let _ = w.hide();
            }
        });
    }
    {
        let weak = sys_win.as_weak();
        sys_win.on_win_drag(move || {
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
        let weak = sys_win.as_weak();
        sys_win.on_win_resize_se(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_resize_window(ResizeDirection::SouthEast);
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        let win_weak = window.as_weak();
        let sys_weak = sys_win.as_weak();
        window.on_open_system_info(move || {
            let (Some(main), Some(sw)) = (win_weak.upgrade(), sys_weak.upgrade()) else {
                return;
            };
            // Detailed system information is remote-only. Keep this guard even
            // though the sidebar hides/disables its affordance when unavailable.
            if !main.get_system_info_available() {
                return;
            }
            main.set_system_info_window_open(true);
            main.invoke_refresh_sidebar();
            sw.set_host(main.get_conn_host());
            sw.set_connection_state(main.get_connection_state());
            sw.set_resource_title(main.get_resource_title());
            sync_system_info_theme(&main, &sw);
            let _ = sw.show();
            place_system_info_window(&main, &sw);
            sw.window().with_winit_window(|ww| ww.focus_window());
        });
    }
}

/// Editor window close, drag and resize callbacks.
pub(super) fn wire_editor_window_chrome(ctx: &WinCtx) {
    let WinCtx {
        window, editor_win, ..
    } = ctx;
    {
        let weak = editor_win.as_weak();
        let main_weak = window.as_weak();
        let close_request = editor_win.as_weak();
        editor_win.window().on_close_requested(move || {
            if let Some(editor) = close_request.upgrade() {
                editor.invoke_request_close();
            }
            slint::CloseRequestResponse::KeepWindowShown
        });
        editor_win.on_close_editor(move || {
            if let (Some(editor), Some(main)) = (weak.upgrade(), main_weak.upgrade()) {
                editor.set_editor_open(false);
                let _ = editor.hide();
                main.set_editor_open(false);
            }
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
