//! Window construction: build and wire one application window, plus the
//! window-level teardown, centering / raise-to-front helpers and the
//! settings / session sync callbacks. Split out of app.rs to match the
//! upstream module boundary (open_window).

use super::*;

/// Tear down one window's workers (SSH + SFTP) and hide its detachable
/// monitor windows. Idempotent: repeated calls see empty maps. Also aborts
/// every queued auth prompt (host key / credentials / MFA) owned by this
/// window, answering reject/cancel so the blocked connection attempts fail
/// cleanly instead of hanging on a dialog that will never show (#multi-window).
pub(crate) fn teardown_window(
    window_id: u64,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    sftp_handles: &SftpHandles,
    proc_weak: &slint::Weak<ProcWindow>,
    sys_weak: &slint::Weak<SystemInfoWindow>,
    editor_weak: &slint::Weak<EditorWindow>,
) {
    abort_window_prompts(window_id);
    {
        let mut sessions = handles.borrow_mut();
        for handle in sessions.values() {
            handle.close();
        }
        sessions.clear();
    }
    if let Ok(mut sftp) = sftp_handles.lock() {
        for handle in sftp.values() {
            handle.close();
        }
        sftp.clear();
    }
    if let Some(w) = proc_weak.upgrade() {
        let _ = w.hide();
    }
    if let Some(w) = sys_weak.upgrade() {
        let _ = w.hide();
    }
    if let Some(w) = editor_weak.upgrade() {
        let _ = w.hide();
    }
}

/// Per-window build context, mirroring the upstream `open_window` split's
/// `WinCtx`: the agent launchers take it instead of a dozen loose arguments.
pub(crate) struct WinCtx {
    pub(crate) core: Rc<AppCore>,
    pub(crate) store: Rc<RefCell<ConfigStore>>,
    pub(crate) registry: Rc<WindowRegistry<slint::Weak<AppWindow>>>,
    pub(crate) handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    pub(crate) sftp_handles: SftpHandles,
    pub(crate) bufs: TermBuffers,
    pub(crate) window: Rc<AppWindow>,
    pub(crate) proc_win: Rc<ProcWindow>,
    pub(crate) sys_win: Rc<SystemInfoWindow>,
    pub(crate) editor_win: Rc<EditorWindow>,
    pub(crate) window_id: u64,
    pub(crate) pending_window_size_restore: Rc<Cell<Option<(f32, f32)>>>,
    pub(crate) window_size_tracking_ready: Rc<Cell<bool>>,
}

/// File-dialog helper. The upstream split wraps a native parent window handle
/// so rfd dialogs stay modal to the owning frameless window; this single-file
/// layout builds a plain dialog (rfd still inherits the app window), keeping
/// the `DialogOwner::of_weak(&weak).file().pick_folder()` call sites intact.
pub(crate) struct DialogOwner(rfd::FileDialog);

impl DialogOwner {
    pub(crate) fn of_weak(_weak: &slint::Weak<AppWindow>) -> Self {
        DialogOwner(rfd::FileDialog::new())
    }

    pub(crate) fn file(self) -> rfd::FileDialog {
        self.0
    }

    pub(crate) fn message(&self) -> rfd::MessageDialog {
        rfd::MessageDialog::new()
    }
}

/// Build and wire one application window. Called for the first window by
/// `run()` and for every subsequent window by the new-window entry points.
/// `at` pins the window to a physical screen position (tab detach); without
/// it the window cascades from the newest one or centers.
/// Returns the registry id used to unregister on close.
pub(crate) fn open_window(
    core: Rc<AppCore>,
    cascade: bool,
    at: Option<slint::PhysicalPosition>,
) -> Result<u64> {
    let runtime = core.runtime.clone();
    let store = core.store.clone();
    let registry = core.registry.clone();

    // Per-tab SSH handles (shell only; lives on Slint thread via Rc).
    let handles: Rc<RefCell<HashMap<String, SessionHandle>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // Per-tab SFTP handles — Arc<Mutex> so the event-pump OS thread and the
    // Slint UI thread can both post SftpCommands.
    let sftp_handles: SftpHandles = Arc::new(Mutex::new(HashMap::new()));
    // Per-tab cwd the SFTP panel last followed (see SftpLastCwd).
    let sftp_last_cwd: SftpLastCwd = Arc::new(Mutex::new(HashMap::new()));

    // Per-tab vt100 parsers + history logs (Arc<Mutex> so they can be cloned
    // into the thread that pumps session events into invoke_from_event_loop).
    let bufs: TermBuffers = Arc::new(Mutex::new(HashMap::new()));
    let render_gates: RenderGates = Arc::new(Mutex::new(HashMap::new()));

    // Last-known terminal pixel dimensions, updated by every terminal-resize
    // callback.  Shared so on_connect_session can pass a sensible initial PTY
    // size to spawn_session before the first resize callback fires.
    // Default: 80 cols × 24 rows (SSH spec minimum).
    let last_term_size: Arc<Mutex<(u32, u32)>> = Arc::new(Mutex::new((80, 24)));

    // --- Build window + models ------------------------------------------
    let window = Rc::new(AppWindow::new().context("failed to build Slint window")?);
    // Cascade origin must be captured *before* registering: once registered,
    // registry.newest() is this window itself. Registration itself is deferred
    // until after the last fallible construction below, so a failed monitor
    // window never leaves a stale registry entry.
    let cascade_origin = if cascade { registry.newest() } else { None };
    // Slint applies preferred-width/height while the native window is being
    // created. Do not treat those startup Resized events as user adjustments;
    // otherwise they overwrite the persisted size before restoration (#278).
    let window_size_tracking_ready = Rc::new(Cell::new(false));
    let pending_window_size_restore = Rc::new(Cell::new(None::<(f32, f32)>));

    // Show the crate version (from Cargo.toml at compile time) in the sidebar,
    // so the footer never drifts out of sync with the actual build.
    window.set_app_version(env!("CARGO_PKG_VERSION").into());
    // "Open files in a tab" (#494): seed from the persisted preference.
    window.set_editor_embedded(store.borrow().editor_embedded());

    // Set the window icon from the PNG embedded in the binary so the dock
    // shows the correct icon even without a system-installed .desktop entry
    // (e.g. AppImage without AppImageLauncher, or plain binary in ~/bin).
    #[cfg(target_os = "linux")]
    set_window_icon(&window);

    // The window defaults to frameless + custom title bar (#119). macOS keeps
    // its native decorations, so turn the custom bar off there.
    #[cfg(target_os = "macos")]
    window.set_custom_titlebar(false);

    // --- Detachable process monitor window (#23) -----------------------------
    // The process table is its own top-level OS window so it can be dragged
    // outside the main window (or onto a second monitor). Both windows render
    // the *same* VecModel, so the table stays live wherever it's parked; closing
    // it just hides it, so reopening is instant.
    let proc_rows_model: Rc<VecModel<ProcRow>> = Rc::new(VecModel::default());
    window.set_proc_list(ModelRc::from(proc_rows_model.clone()));
    let sys_metrics_model: Rc<VecModel<SysMetricRow>> = Rc::new(VecModel::default());
    let sys_net_rows_model: Rc<VecModel<SysNetRow>> = Rc::new(VecModel::default());
    let sys_disks_model: Rc<VecModel<DiskInfo>> = Rc::new(VecModel::default());
    let sys_overview_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_cpu_info_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_gpu_info_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_cpu_usage_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_memory_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_swap_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_network_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_filesystem_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    window.set_sys_metrics(ModelRc::from(sys_metrics_model.clone()));
    window.set_sys_net_rows(ModelRc::from(sys_net_rows_model.clone()));
    window.set_sys_disks(ModelRc::from(sys_disks_model.clone()));
    window.set_sys_overview_rows(ModelRc::from(sys_overview_model.clone()));
    window.set_sys_cpu_info_rows(ModelRc::from(sys_cpu_info_model.clone()));
    window.set_sys_gpu_info_rows(ModelRc::from(sys_gpu_info_model.clone()));
    window.set_sys_cpu_usage_rows(ModelRc::from(sys_cpu_usage_model.clone()));
    window.set_sys_memory_rows(ModelRc::from(sys_memory_model.clone()));
    window.set_sys_swap_rows(ModelRc::from(sys_swap_model.clone()));
    window.set_sys_network_rows(ModelRc::from(sys_network_model.clone()));
    window.set_sys_filesystem_rows(ModelRc::from(sys_filesystem_model.clone()));
    let proc_win = Rc::new(ProcWindow::new().context("failed to build process window")?);
    proc_win.set_custom_titlebar(cfg!(not(target_os = "macos")));
    proc_win.set_proc_list(ModelRc::from(proc_rows_model.clone()));
    let sys_win = Rc::new(SystemInfoWindow::new().context("failed to build system info window")?);
    // Detached built-in editor window (#70). Its save/find wiring is installed
    // by `wire_sftp_callbacks` (below); the in-window editor tabs (#494) are
    // wired here right after it. Kept alive like `proc_win`/`sys_win`: a hidden
    // Slint window is dropped unless a strong handle outlives `open_window`.
    let editor_win = Rc::new(EditorWindow::new().context("failed to build editor window")?);
    editor_win.set_custom_titlebar(cfg!(not(target_os = "macos")));
    editor_win.set_is_mac(cfg!(target_os = "macos"));
    sync_editor_theme(&window, &editor_win);
    // Every fallible construction has now succeeded — register the window.
    // (cascade_origin above was captured before this point, as required.)
    let window_id = registry.register(window.as_weak());
    sys_win.set_custom_titlebar(cfg!(not(target_os = "macos")));
    sys_win.set_metrics(ModelRc::from(sys_metrics_model.clone()));
    sys_win.set_nets(ModelRc::from(sys_net_rows_model.clone()));
    sys_win.set_disks(ModelRc::from(sys_disks_model.clone()));
    sys_win.set_overview_rows(ModelRc::from(sys_overview_model.clone()));
    sys_win.set_cpu_info_rows(ModelRc::from(sys_cpu_info_model.clone()));
    sys_win.set_gpu_info_rows(ModelRc::from(sys_gpu_info_model.clone()));
    sys_win.set_cpu_usage_rows(ModelRc::from(sys_cpu_usage_model.clone()));
    sys_win.set_memory_rows(ModelRc::from(sys_memory_model.clone()));
    sys_win.set_swap_rows(ModelRc::from(sys_swap_model.clone()));
    sys_win.set_network_rows(ModelRc::from(sys_network_model.clone()));
    sys_win.set_filesystem_rows(ModelRc::from(sys_filesystem_model.clone()));
    {
        // ✕ hides the window (data keeps flowing into the shared model).
        let weak = proc_win.as_weak();
        let main_weak = window.as_weak();
        proc_win.on_close_window(move || {
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
        sys_win.on_close_window(move || {
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

    // Apply the saved UI language.  The Rust-side flag drives `i18n::t(...)`;
    // `apply_to_slint` selects the bundled `.po` for the static `@tr(...)` text
    // (must run after the first component exists, which it now does).
    crate::i18n::set_language(store.borrow().language());
    crate::i18n::apply_to_slint();
    window.set_lang_en(crate::i18n::is_en());

    // Apply the saved (or system-detected) theme.
    // "dark" / "light" → use that directly; "system" or unset → ask the OS;
    // OS unknown → fall back to dark.
    {
        let is_dark = theme_pref_is_dark(&store.borrow());
        window.set_dark_mode(is_dark);
    }
    // On macOS, app shortcuts use Cmd (⌘) so physical Ctrl stays free for the
    // shell (#158); on Windows/Linux they stay Ctrl-based.
    window.set_is_mac(cfg!(target_os = "macos"));
    window.set_is_windows(cfg!(windows));

    // Apply the saved terminal font (Interface settings). An empty family keeps
    // the built-in default; the size always applies (defaults to 13).
    {
        let s = store.borrow();
        let fam = s.font_family().to_string();
        refresh_glyph_fallback(&fam, &window, &bufs);
        if !fam.is_empty() {
            window.set_term_font_family(fam.into());
        }
        window.set_term_font_size(s.font_size() as f32);
        window.set_terminal_line_spacing(s.terminal_line_spacing());
        window.set_term_font_bold(s.terminal_bold());
        window.set_term_cursor_style(s.terminal_cursor_style().into());
        if let Some(color) = parse_hex_color(s.terminal_cursor_color()) {
            window.set_term_cursor_color_hex(s.terminal_cursor_color().into());
            window.set_term_cursor_color(color);
        }
        window.set_output_highlight_enabled(s.output_highlight_enabled());
        window.set_json_format_output(s.json_format_output());
        window.set_session_log_enabled(s.session_log_enabled());
        window.set_session_log_dir(s.session_log_dir().to_string_lossy().to_string().into());
        window.set_session_log_dir_custom(!s.session_log_dir_setting().is_empty());
        window.set_output_highlight_preset(s.output_highlight_preset().into());
        window.set_output_highlight_rules(output_highlight_rule_model(&s));
        window.set_action_links_enabled(s.action_links_enabled());
        window.set_action_links_ipv4(s.action_links_ipv4_pref());
        window.set_action_links_host_port(s.action_links_host_port_pref());
        window.set_action_links_url(s.action_links_url_pref());
        window.set_search_history_mode(s.search_history_mode());
        window.set_command_history_min_len(s.command_history_min_len() as i32);
        window.set_command_history_max_len(s.command_history_max_len() as i32);
        window.set_ui_scale(s.ui_scale() as f32 / 100.0); // global UI zoom (#100)
        window.set_panel_font(s.panel_font() as f32 / 100.0); // settings-panel font scale
        window.set_renderer_mode(s.renderer_mode().into());
        window.set_mcp_enabled(s.mcp_enabled());
        window.set_mcp_use_saved_credentials(s.mcp_use_saved_credentials());
        window.set_mcp_allow_commands(s.mcp_allow_commands());
        window.set_mcp_allow_file_transfers(s.mcp_allow_file_transfers());
        // Settings › MCP: ready-to-paste client config with this exe's real path.
        let (command, args) = crate::mcp::client_launch();
        window.set_mcp_config_json(crate::mcp::client_config_json(&command, &args).into());
    }

    {
        let store = store.clone();
        window.on_set_mcp_permissions(move |enabled, credentials, commands, files| {
            let mut settings = store.borrow_mut();
            settings.set_mcp_enabled(enabled);
            settings.set_mcp_use_saved_credentials(credentials);
            settings.set_mcp_allow_commands(commands);
            settings.set_mcp_allow_file_transfers(files);
            let _ = settings.save();
        });
    }

    // Colour theme preset + the immersive-colours switch. Applied before the
    // wallpaper so a built-in wallpaper can still override light/dark.
    color_theme::apply_color_theme(&window, &store.borrow());

    // Terminal colour schemes (dark + light pair), before any terminal renders
    // (upstream 126072b).
    terminal_scheme::apply_terminal_schemes(&window, &store.borrow());

    // Apply the saved immersive wallpaper (overrides dark/light when set; a
    // missing custom file falls back to the plain theme).
    {
        let id = store.borrow().wallpaper().to_string();
        // Restoring a saved wallpaper must not override the user's persisted
        // light/dark preference. Built-in wallpapers only suggest their paired
        // theme when the user actively selects them (#theme-persistence).
        apply_wallpaper(&window, &store.borrow(), &bufs, &id, false);
    }
    // Editable inputs (e.g. the SFTP path bar) need a CJK-capable font: the
    // embedded mono font has no Chinese glyphs and native TextInput doesn't
    // glyph-fallback like Text does, so typed Chinese would render as tofu (#54).
    //
    // We must NOT hard-code one system font name: on macOS 26 (Tahoe) fontdb
    // failed to register "PingFang SC", so the UI default font resolved to nothing
    // and *all* text vanished (#129) — icons survived only because they use an
    // embedded font. Instead probe what fontdb actually loaded and pick the first
    // resolvable CJK family, falling back to the embedded "Meatshell Mono" so the
    // window is never fully blank even when the system font DB is unreadable.
    window.set_ui_font_family(resolve_ui_font_family());
    // Populate the Interface font picker with installed monospace families.
    window.set_term_fonts(ModelRc::from(Rc::new(VecModel::from(
        system_monospace_fonts(),
    ))));

    // Command bar (#55): seed quick commands + history from the config. Groups
    // start collapsed by default (#55).
    window.set_quick_commands(quick_cmd_model(
        &store.borrow(),
        &all_quick_group_names(&store.borrow()),
    ));
    window.set_command_history(history_model(&store.borrow()));
    set_history_view(&window, &store.borrow(), ""); // #101, #419

    // Interface setting: SFTP follows the terminal's cd. The shell event pumps
    // read this AtomicBool on every CwdChanged, so toggling applies live to
    // already-open sessions too.
    let sftp_follow_cd = Arc::new(std::sync::atomic::AtomicBool::new(
        store.borrow().sftp_follow_cd(),
    ));
    window.set_sftp_follow_cd(store.borrow().sftp_follow_cd());
    {
        let store = store.clone();
        let flag = sftp_follow_cd.clone();
        window.on_set_sftp_follow_cd(move |follow| {
            flag.store(follow, std::sync::atomic::Ordering::Relaxed);
            let mut s = store.borrow_mut();
            s.set_sftp_follow_cd(follow);
            let _ = s.save();
        });
    }

    // SFTP transfer-queue tuning (concurrency / rate limit / dedup / mtime).
    window.set_sftp_queue_concurrency(store.borrow().sftp_queue_concurrency() as i32);
    window.set_sftp_queue_rate_limit(store.borrow().sftp_queue_rate_limit_kbps() as i32);
    window.set_sftp_queue_dedup(store.borrow().sftp_queue_dedup().to_string().into());
    window.set_sftp_preserve_mtime(store.borrow().sftp_queue_preserve_mtime());
    {
        let store = store.clone();
        window.on_set_sftp_queue_concurrency(move |n| {
            let mut s = store.borrow_mut();
            s.set_sftp_queue_concurrency(n.max(1) as u32);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_sftp_queue_rate_limit(move |kbps| {
            let mut s = store.borrow_mut();
            s.set_sftp_queue_rate_limit_kbps(kbps.max(0) as u32);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_sftp_queue_dedup(move |policy| {
            let mut s = store.borrow_mut();
            s.set_sftp_queue_dedup(policy.to_string());
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_sftp_preserve_mtime(move |preserve| {
            let mut s = store.borrow_mut();
            s.set_sftp_queue_preserve_mtime(preserve);
            let _ = s.save();
        });
    }

    // SSH keepalive (Settings › Connection, #515): mirror + persist. The value
    // reaches every new SSH connection through `ssh::apply_keepalive`.
    window.set_ssh_keepalive_enabled(store.borrow().ssh_keepalive_enabled());
    window.set_ssh_keepalive_interval(store.borrow().ssh_keepalive_interval() as i32);
    window.set_ssh_keepalive_max(store.borrow().ssh_keepalive_max() as i32);
    {
        let store = store.clone();
        let registry = registry.clone();
        let weak = window.as_weak();
        window.on_set_ssh_keepalive(move |enabled, interval, max| {
            {
                let mut s = store.borrow_mut();
                s.set_ssh_keepalive(enabled, interval.max(0) as u32, max.max(0) as u32);
                let _ = s.save();
                // Show the clamped values the store actually kept.
                if let Some(w) = weak.upgrade() {
                    w.set_ssh_keepalive_enabled(s.ssh_keepalive_enabled());
                    w.set_ssh_keepalive_interval(s.ssh_keepalive_interval() as i32);
                    w.set_ssh_keepalive_max(s.ssh_keepalive_max() as i32);
                }
            }
            registry.broadcast_config_changed();
        });
    }

    // Grace auto-reconnect settings (#P3-H). The timer itself is created once
    // the window's ConnectCtx exists (see below); here we only mirror + persist.
    window.set_auto_reconnect_enabled(store.borrow().auto_reconnect_enabled());
    window.set_auto_reconnect_max_attempts(store.borrow().auto_reconnect_max_attempts() as i32);
    window.set_auto_reconnect_interval(store.borrow().auto_reconnect_interval_secs() as i32);
    {
        let store = store.clone();
        window.on_set_auto_reconnect_enabled(move |enabled| {
            let mut s = store.borrow_mut();
            s.set_auto_reconnect_enabled(enabled);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_auto_reconnect_max_attempts(move |n| {
            let mut s = store.borrow_mut();
            s.set_auto_reconnect_max_attempts(n.max(1) as u32);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_auto_reconnect_interval(move |secs| {
            let mut s = store.borrow_mut();
            s.set_auto_reconnect_interval_secs(secs.max(2) as u64);
            let _ = s.save();
        });
    }

    // Toolbar toggle: hide/show the quick-command bar (persisted globally).
    window.set_cmd_bar_hidden(store.borrow().cmd_bar_hidden());
    {
        let store = store.clone();
        let registry = registry.clone();
        window.on_set_cmd_bar_hidden(move |hidden| {
            let mut s = store.borrow_mut();
            s.set_cmd_bar_hidden(hidden);
            let _ = s.save();
            drop(s);
            registry.broadcast_config_changed();
        });
    }

    // Interface setting: always ask where to save on download (#87). Read live
    // by the download handler from the window property, so just set + persist.
    window.set_download_always_ask(store.borrow().download_always_ask());
    window.set_paste_confirm_enabled(store.borrow().paste_confirm_enabled());
    // Opt-in right-click copy or paste (upstream bf9c296). The window property
    // is read live by the right-click handler in key_input.rs.
    window.set_right_click_copy(store.borrow().right_click_copy());
    {
        let store = store.clone();
        let registry = registry.clone();
        window.on_set_right_click_copy(move |enabled| {
            let mut store = store.borrow_mut();
            store.set_right_click_copy(enabled);
            let _ = store.save();
            drop(store);
            registry.broadcast_config_changed();
        });
    }
    window.set_extra_paste_shortcuts_enabled(store.borrow().extra_paste_shortcuts_enabled());
    window.set_zen_mode(store.borrow().zen_mode());
    {
        let store = store.clone();
        window.on_set_download_always_ask(move |ask| {
            let mut s = store.borrow_mut();
            s.set_download_always_ask(ask);
            let _ = s.save();
        });
    }
    // --- Session logging (#265) -------------------------------------------
    {
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_session_log_enabled(move |enabled| {
            let dir = {
                let mut s = store.borrow_mut();
                s.set_session_log_enabled(enabled);
                let _ = s.save();
                s.session_log_dir()
            };
            // Apply to tabs that are already open, not only to new ones.
            let handles: Vec<TermBufferHandle> =
                lock_or_recover(&bufs).values().cloned().collect();
            for handle in handles {
                apply_session_log_to_buffer(&mut lock_or_recover(&handle), enabled, &dir);
            }
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        window.on_pick_session_log_dir(move || {
            let start = store.borrow().session_log_dir();
            let Some(folder) = rfd::FileDialog::new().set_directory(&start).pick_folder() else {
                return;
            };
            let effective = {
                let mut s = store.borrow_mut();
                s.set_session_log_dir(folder.to_string_lossy().to_string());
                let _ = s.save();
                s.session_log_dir()
            };
            if let Some(w) = weak.upgrade() {
                w.set_session_log_dir(effective.to_string_lossy().to_string().into());
                w.set_session_log_dir_custom(true);
            }
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        window.on_reset_session_log_dir(move || {
            let effective = {
                let mut s = store.borrow_mut();
                s.set_session_log_dir(String::new());
                let _ = s.save();
                s.session_log_dir()
            };
            if let Some(w) = weak.upgrade() {
                w.set_session_log_dir(effective.to_string_lossy().to_string().into());
                w.set_session_log_dir_custom(false);
            }
        });
    }
    {
        let store = store.clone();
        window.on_open_session_log_dir(move || {
            let dir = store.borrow().session_log_dir();
            if std::fs::create_dir_all(&dir).is_err() {
                return;
            }
            #[cfg(windows)]
            let _ = std::process::Command::new("explorer").arg(&dir).spawn();
            #[cfg(target_os = "macos")]
            let _ = std::process::Command::new("open").arg(&dir).spawn();
            #[cfg(all(not(windows), not(target_os = "macos")))]
            let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
        });
    }
    // The open-source link in the About panel opens this fork's repository.
    window.on_open_repo(|| {
        let url = "https://github.com/agogo233/meatshell";
        #[cfg(windows)]
        let _ = std::process::Command::new("explorer").arg(url).spawn();
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open").arg(url).spawn();
        #[cfg(all(not(windows), not(target_os = "macos")))]
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    });
    {
        let store = store.clone();
        window.on_set_paste_confirm_enabled(move |enabled| {
            let mut s = store.borrow_mut();
            s.set_paste_confirm_enabled(enabled);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_extra_paste_shortcuts_enabled(move |enabled| {
            let mut s = store.borrow_mut();
            s.set_extra_paste_shortcuts_enabled(enabled);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        let handles = handles.clone();
        let weak = window.as_weak();
        window.on_set_zen_mode(move |enabled| {
            let mut s = store.borrow_mut();
            s.set_zen_mode(enabled);
            let _ = s.save();
            let sidebar_visible = weak
                .upgrade()
                .map(|window| !window.get_sidebar_collapsed())
                .unwrap_or(false);
            for handle in handles.borrow().values() {
                handle.set_resource_monitoring(!enabled && sidebar_visible);
            }
        });
    }

    // Interface setting: collapse the sidebars by default (#78). Seed the
    // checkboxes, apply the collapsed state once at startup, and persist toggles.
    {
        let s = store.borrow();
        let collapse_sidebar = s.collapse_sidebar_default();
        let collapse_sftp = s.collapse_sftp_default();
        let sidebar_dock = s.sidebar_dock();
        let welcome_as_sidebar = s.welcome_as_sidebar();
        let quick_commands_as_sidebar = s.quick_commands_as_sidebar();
        let quick_panel_open = quick_commands_as_sidebar && s.quick_panel_open();
        let quick_panel_collapsed = s.quick_panel_collapsed();
        let quick_panel_dock = s.quick_panel_dock();
        let welcome_sidebar_dock = s.welcome_sidebar_dock();
        let ai_panel_dock = s.ai_panel_dock();
        let ai_panel_open = s.ai_panel_open();
        let ai_panel_collapsed = s.ai_panel_collapsed();
        let sidebar_collapsed = s.sidebar_collapsed().unwrap_or(collapse_sidebar);
        let welcome_collapsed = s.welcome_collapsed().unwrap_or(false);
        // Same-edge stacking (#dock-stack) replaces the old "one expanded panel
        // per edge" squeeze: panels restored onto one edge are stacked and
        // split there, exactly like a manual re-dock. A collapsed panel only
        // shares that edge's strip band with the others.
        window.set_collapse_sidebar_default(collapse_sidebar);
        window.set_collapse_sftp_default(collapse_sftp);
        // Restore the persisted panel docking layout (#dock).
        window.set_sidebar_width(s.sidebar_width());
        window.set_sidebar_height(s.sidebar_height());
        window.set_sidebar_dock(sidebar_dock.into());
        window.set_sftp_panel_width(s.sftp_panel_width());
        window.set_sftp_panel_height(s.sftp_panel_height());
        window.set_sftp_tree_width(s.sftp_tree_width());
        window.set_sftp_local_show_hidden(s.sftp_local_show_hidden());
        window.set_sftp_local_sort_key(s.sftp_local_sort_key().into());
        window.set_sftp_local_sort_dir(s.sftp_local_sort_dir());
        let columns = s.sftp_visible_columns();
        window.set_sftp_show_type(columns.iter().any(|column| column == "type"));
        window.set_sftp_show_size(columns.iter().any(|column| column == "size"));
        window.set_sftp_show_modified(columns.iter().any(|column| column == "modified"));
        window.set_sftp_show_permissions(columns.iter().any(|column| column == "permissions"));
        window.set_sftp_show_owner(columns.iter().any(|column| column == "owner"));
        window.set_sftp_show_group(columns.iter().any(|column| column == "group"));
        window.set_sftp_dock(s.sftp_dock().into());
        window.set_quick_commands_as_sidebar(quick_commands_as_sidebar);
        window.set_quick_panel_open(quick_panel_open);
        window.set_quick_panel_collapsed(quick_panel_collapsed);
        window.set_quick_panel_width(s.quick_panel_width());
        window.set_quick_panel_height(s.quick_panel_height());
        window.set_quick_panel_dock(quick_panel_dock.into());
        window.set_welcome_as_sidebar(welcome_as_sidebar);
        // AI chat panel: restore the persisted docking layout + chat settings.
        window.set_ai_panel_open(ai_panel_open);
        window.set_ai_panel_enabled(ai_panel_open);
        window.set_ai_panel_collapsed(ai_panel_collapsed);
        window.set_ai_panel_width(s.ai_panel_width());
        window.set_ai_panel_height(s.ai_panel_height());
        window.set_ai_panel_dock(ai_panel_dock.into());
        window.set_ai_base_url(s.ai_base_url().into());
        window.set_ai_api_key(s.ai_api_key().into());
        window.set_ai_model(s.ai_model().into());
        window.set_welcome_sidebar_width(s.welcome_sidebar_width());
        window.set_welcome_sidebar_dock(welcome_sidebar_dock.into());
        window.set_welcome_collapsed(welcome_collapsed);
        window.set_sidebar_collapsed(sidebar_collapsed);
        window.set_wallpaper_overlay(s.wallpaper_overlay());
        if collapse_sftp {
            window.set_sftp_collapsed(true);
            window.set_sftp_saved_height(s.sftp_panel_height());
        }
        // Capture the user's preferred size. The first native Resized event
        // drives restoration below; this is deterministic and avoids guessing
        // how long Slint/window-manager initialization takes (#278).
        let (ww, wh) = s.window_size();
        let preferred = (ww > 0.0 && wh > 0.0).then_some((ww, wh));
        pending_window_size_restore.set(preferred);
    }
    {
        let store = store.clone();
        window.on_set_collapse_sidebar_default(move |v| {
            let mut s = store.borrow_mut();
            s.set_collapse_sidebar_default(v);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_quick_commands_as_sidebar(move |v| {
            let mut s = store.borrow_mut();
            s.set_quick_commands_as_sidebar(v);
            let _ = s.save();
        });
    }
    {
        // Renderer selection is consumed before the first native window exists,
        // so persist it now and apply it on the next launch (#280).
        let store = store.clone();
        window.on_set_renderer_mode(move |mode: SharedString| {
            let mut s = store.borrow_mut();
            s.set_renderer_mode(mode.to_string());
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_persist_sidebar_width(move |w| {
            let mut s = store.borrow_mut();
            s.set_sidebar_width(w);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        let handles = handles.clone();
        let weak = window.as_weak();
        window.on_set_sidebar_collapsed(move |v| {
            let mut s = store.borrow_mut();
            s.set_sidebar_collapsed(v);
            let _ = s.save();
            let zen = weak.upgrade().map(|window| window.get_zen_mode()).unwrap_or(false);
            for handle in handles.borrow().values() {
                handle.set_resource_monitoring(!v && !zen);
            }
        });
    }
    {
        let store = store.clone();
        window.on_persist_welcome_sidebar_width(move |w| {
            let mut s = store.borrow_mut();
            s.set_welcome_sidebar_width(w);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_persist_welcome_sidebar_dock(move |dock| {
            let mut s = store.borrow_mut();
            s.set_welcome_sidebar_dock(dock.to_string());
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_welcome_collapsed(move |v| {
            let mut s = store.borrow_mut();
            s.set_welcome_collapsed(v);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_persist_wallpaper_overlay(move |v| {
            let mut s = store.borrow_mut();
            s.set_wallpaper_overlay(v);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_collapse_sftp_default(move |v| {
            let mut s = store.borrow_mut();
            s.set_collapse_sftp_default(v);
            let _ = s.save();
        });
    }

    // Session-sync upload setting (#sync). Persisted; only has effect while the
    // session-sync toggle is on. Read live from the window in the upload handler.
    window.set_sync_upload_enabled(store.borrow().sync_upload());
    {
        let store = store.clone();
        window.on_set_sync_upload_enabled(move |v| {
            let mut s = store.borrow_mut();
            s.set_sync_upload(v);
            let _ = s.save();
        });
    }

    // WebDAV config sync (#185): manual upload/download of the portable session
    // export JSON. It is intentionally not automatic on startup.
    {
        sync_settings_for_window(&window.as_weak(), &store.borrow());
        window.set_webdav_status(String::new().into());
    }
    // Bundle work (Argon2 KDF + WebDAV round trips) runs on worker threads so
    // the window stays responsive; decrypted configs come back through this
    // mailbox and the UI-thread timer below, because the store is Rc-bound.
    let bundle_mailbox: BundleMailbox = Arc::new(Mutex::new(Vec::new()));
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_save_webdav_settings(
            move |enabled: bool,
                  url: SharedString,
                  username: SharedString,
                  password: SharedString,
                  remote_path: SharedString,
                  mut accept_invalid_certs: bool| {
                // Turning verification off exposes every saved credential, so ask
                // once; a decline keeps the previous (verified) value.
                if accept_invalid_certs
                    && !store.borrow().webdav_accept_invalid_certs()
                    && !confirm_skip_cert_verification()
                {
                    accept_invalid_certs = false;
                    if let Some(w) = weak.upgrade() {
                        w.set_webdav_accept_invalid_certs(false);
                    }
                }
                // Also catch an https → http switch while verification is already off.
                if let Err(e) = require_tls_for_skipped_verification(
                    accept_invalid_certs,
                    url.as_str(),
                ) {
                    if let Some(w) = weak.upgrade() {
                        w.set_webdav_status(e.to_string().into());
                    }
                    return;
                }
                let mut s = store.borrow_mut();
                s.set_webdav_settings(
                    enabled,
                    url.to_string(),
                    username.to_string(),
                    password.to_string(),
                    remote_path.to_string(),
                    accept_invalid_certs,
                );
                let _ = s.save();
            },
        );
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_webdav_upload(move || {
            let Some(w) = weak.upgrade() else { return };
            let enabled = w.get_webdav_enabled();
            let url = w.get_webdav_url().to_string();
            let username = w.get_webdav_username().to_string();
            let password = w.get_webdav_password().to_string();
            let remote_path = w.get_webdav_remote_path().to_string();
            let accept_invalid_certs = w.get_webdav_accept_invalid_certs();
            {
                let mut s = store.borrow_mut();
                s.set_webdav_settings(
                    enabled,
                    url.clone(),
                    username.clone(),
                    password.clone(),
                    remote_path.clone(),
                    accept_invalid_certs,
                );
                let _ = s.save();
            }
            if !enabled {
                w.set_webdav_status(t("请先启用 WebDAV 同步", "enable WebDAV sync first").into());
                return;
            }
            // Sessions travel in the passphrase-encrypted bundle, not in the
            // fixed-key JSON export: that file exposed every saved password to
            // anyone who could read the WebDAV share.
            let pass = w.get_portable_passphrase().to_string();
            if pass.is_empty() {
                w.set_webdav_status(
                    t("请先填写便携包口令", "enter a bundle passphrase first").into(),
                );
                return;
            }
            let bundle_path = bundle_remote_path(&remote_path);
            // Serialize on the UI thread (the store is Rc-bound); the Argon2
            // KDF, the AEAD and the WebDAV round trips run on a worker.
            let payload = match store.borrow().bundle_payload() {
                Ok(payload) => payload,
                Err(e) => {
                    w.set_webdav_status(
                        format!("{}: {}", t("上传失败", "upload failed"), e).into(),
                    );
                    return;
                }
            };
            let count = store.borrow().sessions().len();
            let weak2 = weak.clone();
            w.set_webdav_status(t("上传中...", "uploading...").into());
            std::thread::spawn(move || {
                let res = ConfigStore::seal_bundle(&pass, &payload).and_then(|pack| {
                    let envelope = format!("{{\"meatshell_bundle\":1,\"data\":\"{pack}\"}}");
                    webdav_put_json(
                        &url,
                        &bundle_path,
                        &username,
                        &password,
                        accept_invalid_certs,
                        envelope,
                    )
                });
                // Retire the legacy plaintext copy once the bundle is in place.
                // Best effort: servers that forbid DELETE must not fail the
                // upload.
                if res.is_ok()
                    && resolved_url(&url, &remote_path) != resolved_url(&url, &bundle_path)
                {
                    if let Err(e) = webdav_delete(
                        &url,
                        &remote_path,
                        &username,
                        &password,
                        accept_invalid_certs,
                    ) {
                        tracing::warn!("legacy WebDAV connections file left behind: {e}");
                    }
                }
                let msg = match res {
                    Ok(()) => format!(
                        "{} {}{}",
                        t("已上传加密同步包", "uploaded the encrypted sync bundle"),
                        count,
                        webdav_status_suffix(accept_invalid_certs)
                    ),
                    Err(e) => format!("{}: {}", t("上传失败", "upload failed"), e),
                };
                let _ = weak2.upgrade_in_event_loop(move |w| w.set_webdav_status(msg.into()));
            });
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_cursor_color(move |value: SharedString| {
            let Some(color) = parse_hex_color(value.as_str()) else {
                return false;
            };
            {
                let mut s = store.borrow_mut();
                if !s.set_terminal_cursor_color(value.as_str()) {
                    return false;
                }
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_cursor_color(color);
            }
            true
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_add_output_highlight_rule(
            move |pattern: SharedString,
                  is_regex,
                  case_sensitive,
                  whole_line,
                  color: SharedString| {
                let pattern = pattern.trim().to_string();
                let validation = validate_output_highlight_rule(&pattern, is_regex, case_sensitive);
                let Some(w) = weak.upgrade() else {
                    return false;
                };
                if let Err(message) = validation {
                    w.set_output_highlight_rule_status(message.into());
                    return false;
                }
                if store.borrow().output_highlight_rules().len() >= 128 {
                    w.set_output_highlight_rule_status(
                        t("自定义规则最多 128 条", "Custom rules are limited to 128").into(),
                    );
                    return false;
                }
                {
                    let mut s = store.borrow_mut();
                    s.add_output_highlight_rule(OutputHighlightRule {
                        id: String::new(),
                        pattern,
                        regex: is_regex,
                        case_sensitive,
                        whole_line,
                        color: color.to_string(),
                        enabled: true,
                    });
                    let _ = s.save();
                    w.set_output_highlight_rules(output_highlight_rule_model(&s));
                    apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
                }
                w.set_output_highlight_rule_status("".into());
                true
            },
        );
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_remove_output_highlight_rule(move |index| {
            let Some(w) = weak.upgrade() else { return };
            let mut s = store.borrow_mut();
            s.remove_output_highlight_rule(index.max(0) as usize);
            let _ = s.save();
            w.set_output_highlight_rules(output_highlight_rule_model(&s));
            apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
            w.set_output_highlight_rule_status("".into());
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_output_highlight_rule_enabled(move |index, enabled| {
            let Some(w) = weak.upgrade() else { return };
            let mut s = store.borrow_mut();
            s.set_output_highlight_rule_enabled(index.max(0) as usize, enabled);
            let _ = s.save();
            w.set_output_highlight_rules(output_highlight_rule_model(&s));
            apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
        });
    }
    // Export the custom highlight rules to a portable JSON file.
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_export_output_highlight_rules(move || {
            let Some(path) = rfd::FileDialog::new()
                .set_file_name("meatshell-highlights.json")
                .add_filter("JSON", &["json"])
                .save_file()
            else {
                return;
            };
            let res = (|| -> anyhow::Result<usize> {
                let s = store.borrow();
                let json = s.export_output_highlight_rules()?;
                std::fs::write(&path, json)?;
                Ok(s.output_highlight_rules().len())
            })();
            if let Some(w) = weak.upgrade() {
                let hint = match res {
                    Ok(n) => format!("{} {}", t("已导出高亮规则", "exported"), n),
                    Err(e) => format!("{}: {}", t("导出失败", "export failed"), e),
                };
                w.set_output_highlight_rule_status(hint.into());
            }
        });
    }
    // Import highlight rules from a JSON file (merge by id / content, skip
    // invalid, respect the 128 cap), then rebuild every terminal immediately.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_import_output_highlight_rules(move || {
            let Some(path) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
            else {
                return;
            };
            let res = (|| -> anyhow::Result<(usize, usize, usize)> {
                let raw = std::fs::read_to_string(&path)?;
                let mut s = store.borrow_mut();
                let out = s.import_output_highlight_rules(&raw)?;
                let _ = s.save();
                Ok(out)
            })();
            if let Some(w) = weak.upgrade() {
                match res {
                    Ok((imported, updated, skipped)) => {
                        w.set_output_highlight_rule_status(
                            format!(
                                "{} {} / {} {} / {} {}",
                                t("已导入", "imported"),
                                imported,
                                t("已更新", "updated"),
                                updated,
                                t("跳过", "skipped"),
                                skipped
                            )
                            .into(),
                        );
                        let s = store.borrow();
                        w.set_output_highlight_rules(output_highlight_rule_model(&s));
                        apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
                    }
                    Err(e) => {
                        w.set_output_highlight_rule_status(
                            format!("{}: {}", t("导入失败", "import failed"), e).into(),
                        );
                    }
                }
            }
        });
    }
    // Interface settings: apply + persist the terminal font family / size.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_term_font(move |family: SharedString| {
            {
                let mut s = store.borrow_mut();
                s.set_font_family(family.to_string());
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                // The new font covers a different glyph set (#461).
                refresh_glyph_fallback(family.as_str(), &w, &bufs);
                w.set_term_font_family(family);
            }
        });
    }
    // Output highlighting: persist the switch/preset and immediately rebuild
    // every open terminal, including scrollback captured before the change.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_output_highlight(move |enabled, preset: SharedString| {
            let preset = preset.to_string();
            {
                let mut s = store.borrow_mut();
                s.set_output_highlight_enabled(enabled);
                s.set_output_highlight_preset(preset.clone());
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                apply_output_highlight(&w, &bufs, enabled, &preset);
            }
        });
    }
    // "Open files in a tab" (#494): persist the setting and mirror it onto
    // this window; other windows follow via the config broadcast below.
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_editor_embedded(move |embedded| {
            {
                let mut s = store.borrow_mut();
                s.set_editor_embedded(embedded);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_editor_embedded(embedded);
            }
        });
    }
    // Action links: persist the master + per-kind switches and refresh every
    // open terminal immediately.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_action_links(move |enabled, ipv4, host_port, url| {
            {
                let mut s = store.borrow_mut();
                s.set_action_links_enabled(enabled);
                s.set_action_links_ipv4(ipv4);
                s.set_action_links_host_port(host_port);
                s.set_action_links_url(url);
                let _ = s.save();
                let flags = action_link_flags(&s);
                let effective = s.action_links_enabled();
                if let Some(w) = weak.upgrade() {
                    apply_action_links(&w, &bufs, flags, effective);
                }
            }
        });
    }
    // Search mode (buffer / history): persist and push to every open terminal.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_search_history_mode(move |history| {
            {
                let mut s = store.borrow_mut();
                s.set_search_history_mode(history);
                let _ = s.save();
            }
            for buffer in lock_or_recover(&bufs).values() {
                lock_or_recover(&buffer).search_history_mode = history;
            }
            if let Some(w) = weak.upgrade() {
                w.set_search_history_mode(history);
            }
        });
    }
    // Command-history length filter (min/max chars, 0 = unlimited).
    {
        let store = store.clone();
        window.on_set_command_history_length_filter(move |min, max| {
            let mut s = store.borrow_mut();
            s.set_command_history_min_len(min.max(0) as u32);
            s.set_command_history_max_len(max.max(0) as u32);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_json_format_output(move |enabled| {
            {
                let mut settings = store.borrow_mut();
                settings.set_json_format_output(enabled);
                let _ = settings.save();
            }
            for buffer in lock_or_recover(&bufs).values() {
                lock_or_recover(&buffer).json_format_output = enabled;
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let registry = registry.clone();
        window.on_set_term_font_size(move |size: i32| {
            {
                let mut s = store.borrow_mut();
                s.set_font_size(size as u32);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_font_size(size as f32);
            }
            registry.broadcast_config_changed();
        });
    }
    {
        let store = store.clone();
        window.on_persist_sftp_tree_width(move |width| {
            let mut s = store.borrow_mut();
            s.set_sftp_tree_width(width);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        window.on_toggle_sftp_column(move |column: SharedString| {
            let columns = {
                let mut s = store.borrow_mut();
                let mut columns = s.sftp_visible_columns();
                if column != "name" {
                    if let Some(index) = columns.iter().position(|value| value == column.as_str()) {
                        columns.remove(index);
                    } else {
                        columns.push(column.to_string());
                    }
                    s.set_sftp_visible_columns(columns);
                }
                let _ = s.save();
                s.sftp_visible_columns()
            };
            if let Some(w) = weak.upgrade() {
                w.set_sftp_show_type(columns.iter().any(|value| value == "type"));
                w.set_sftp_show_size(columns.iter().any(|value| value == "size"));
                w.set_sftp_show_modified(columns.iter().any(|value| value == "modified"));
                w.set_sftp_show_permissions(columns.iter().any(|value| value == "permissions"));
                w.set_sftp_show_owner(columns.iter().any(|value| value == "owner"));
                w.set_sftp_show_group(columns.iter().any(|value| value == "group"));
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_terminal_line_spacing(move |spacing: f32| {
            let normalized = {
                let mut s = store.borrow_mut();
                s.set_terminal_line_spacing(spacing);
                let normalized = s.terminal_line_spacing();
                let _ = s.save();
                normalized
            };
            if let Some(w) = weak.upgrade() {
                w.set_terminal_line_spacing(normalized);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_font_bold(move |bold: bool| {
            {
                let mut s = store.borrow_mut();
                s.set_terminal_bold(bold);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_font_bold(bold);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_cursor_style(move |style: SharedString| {
            let normalized = {
                let mut s = store.borrow_mut();
                s.set_terminal_cursor_style(style.to_string());
                let normalized = s.terminal_cursor_style().to_string();
                let _ = s.save();
                normalized
            };
            if let Some(w) = weak.upgrade() {
                w.set_term_cursor_style(normalized.into());
            }
        });
    }
    // Global UI scale (#100): persist the percent and apply it live.
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_ui_scale(move |percent: i32| {
            let clamped = (percent.max(0) as u32).clamp(80, 200);
            {
                let mut s = store.borrow_mut();
                s.set_ui_scale(clamped);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_ui_scale(clamped as f32 / 100.0);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_panel_font(move |percent: i32| {
            let clamped = (percent.max(0) as u32).clamp(80, 160);
            {
                let mut s = store.borrow_mut();
                s.set_panel_font(clamped);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_panel_font(clamped as f32 / 100.0);
            }
        });
    }

    // Wallpaper: pick a built-in / none, or open the file dialog for a custom one.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs_wp = bufs.clone();
        let proc_weak = proc_win.as_weak();
        let editor_weak = editor_win.as_weak();
        let registry = registry.clone();
        window.on_set_wallpaper(move |id: SharedString| {
            let id = id.to_string();
            let mut selected_builtin_theme = None;
            if let Some(w) = weak.upgrade() {
                apply_wallpaper(&w, &store.borrow(), &bufs_wp, &id, true);
                // With immersive colours off the wallpaper is only a backdrop,
                // so it did not touch light/dark: nothing new to persist.
                if crate::wallpaper::is_builtin(&id) && store.borrow().immersive_colors() {
                    selected_builtin_theme = Some(w.get_dark_mode());
                }
                // Keep an already-open process window in sync with the change.
                if let Some(p) = proc_weak.upgrade() {
                    sync_proc_theme(&w, &p);
                }
                if let Some(e) = editor_weak.upgrade() {
                    sync_editor_theme(&w, &e);
                }
            }
            {
                let mut s = store.borrow_mut();
                s.set_wallpaper(id);
                // Choosing a built-in wallpaper applies its recommended palette once;
                // persist that result so it too survives the next launch. A later
                // manual theme toggle will overwrite this preference as expected.
                if let Some(dark) = selected_builtin_theme {
                    s.set_theme_pref(if dark { "dark" } else { "light" }.to_string());
                }
                let _ = s.save();
            }
            // Only the theme flip needs cross-window propagation; the wallpaper
            // image itself is not synced to other windows (YAGNI).
            if selected_builtin_theme.is_some() {
                registry.broadcast_config_changed();
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs_wp = bufs.clone();
        let proc_weak = proc_win.as_weak();
        let editor_weak = editor_win.as_weak();
        window.on_pick_wallpaper_file(move || {
            let picked = rfd::FileDialog::new()
                .set_title("选择壁纸 / Choose wallpaper")
                .add_filter("Images", &["png", "jpg", "jpeg", "webp", "bmp"])
                .pick_file();
            if let Some(path) = picked {
                let id = path.to_string_lossy().to_string();
                if let Some(w) = weak.upgrade() {
                    apply_wallpaper(&w, &store.borrow(), &bufs_wp, &id, false);
                    if let Some(p) = proc_weak.upgrade() {
                        sync_proc_theme(&w, &p);
                    }
                    if let Some(e) = editor_weak.upgrade() {
                        sync_editor_theme(&w, &e);
                    }
                }
                let mut s = store.borrow_mut();
                s.set_wallpaper(id);
                let _ = s.save();
            }
        });
    }

    let sessions_model: Rc<VecModel<SessionInfo>> = Rc::new(VecModel::default());
    window.set_sessions(ModelRc::from(sessions_model.clone()));
    sync_sessions_to_model(&store.borrow(), &sessions_model);
    window.set_wsl_profiles(wsl_profile_model(&store.borrow()));
    // Cross-window propagation: when another window persists sessions / theme /
    // language it broadcasts; re-sync what this window shows. The listener only
    // READS the store and updates this window's models (never saves), so a
    // broadcast can never re-enter the broadcast path.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let bufs = bufs.clone();
        registry.add_config_listener(window_id, Rc::new(move || {
            let Some(w) = weak.upgrade() else { return };
            // Rebuild the list with the window's current search filter.
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            // Re-apply the theme to the chrome AND every open terminal buffer.
            // Terminal colour schemes first (upstream 126072b): they feed the
            // terminal palette the dark-mode re-apply paints with.
            terminal_scheme::apply_terminal_schemes(&w, &store.borrow());
            apply_dark_mode(&w, &bufs, theme_pref_is_dark(&store.borrow()));
            // Colour theme preset + immersive switch (Settings › Wallpaper & theme).
            color_theme::apply_color_theme(&w, &store.borrow());
            // Language translations are process-global; refresh our flag only.
            w.set_lang_en(crate::i18n::is_en());
            // Local shells / WSL profiles and detected agents can change in
            // another window; rebuild this window's launch-menu groups.
            w.set_launch_shells(launch_shell_model(&store.borrow()));
            w.invoke_refresh_agent_launchers();
            // Command-bar visibility is a global preference.
            w.set_cmd_bar_hidden(store.borrow().cmd_bar_hidden());
            // Opt-in right-click copy or paste is a global preference too.
            w.set_right_click_copy(store.borrow().right_click_copy());
            // Persisted terminal font size (settings stepper) is global too.
            w.set_term_font_size(store.borrow().font_size() as f32);
        }));
    }
    {
        let weak = window.as_weak();
        window.on_pick_wsl_directory(move || {
            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                if let Some(w) = weak.upgrade() {
                    w.set_wsl_new_directory(folder.to_string_lossy().to_string().into());
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let registry = registry.clone();
        window.on_add_wsl_profile(move |name, distribution, directory| {
            {
                let mut s = store.borrow_mut();
                s.add_wsl_profile(
                    name.to_string(),
                    distribution.to_string(),
                    directory.to_string(),
                );
                let _ = s.save();
                if let Some(w) = weak.upgrade() {
                    w.set_wsl_profiles(wsl_profile_model(&s));
                    sync_sessions_for_window(&weak, &s, &sessions_model);
                }
            }
            registry.broadcast_config_changed();
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let registry = registry.clone();
        window.on_remove_wsl_profile(move |id| {
            {
                let mut s = store.borrow_mut();
                s.remove_wsl_profile(id.as_str());
                let _ = s.save();
                if let Some(w) = weak.upgrade() {
                    w.set_wsl_profiles(wsl_profile_model(&s));
                    sync_sessions_for_window(&weak, &s, &sessions_model);
                }
            }
            registry.broadcast_config_changed();
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let registry = registry.clone();
        let mailbox = bundle_mailbox.clone();
        window.on_webdav_download(move || {
            let Some(w) = weak.upgrade() else { return };
            let enabled = w.get_webdav_enabled();
            let url = w.get_webdav_url().to_string();
            let username = w.get_webdav_username().to_string();
            let password = w.get_webdav_password().to_string();
            let remote_path = w.get_webdav_remote_path().to_string();
            let accept_invalid_certs = w.get_webdav_accept_invalid_certs();
            {
                let mut s = store.borrow_mut();
                s.set_webdav_settings(
                    enabled,
                    url.clone(),
                    username.clone(),
                    password.clone(),
                    remote_path.clone(),
                    accept_invalid_certs,
                );
                let _ = s.save();
            }
            if !enabled {
                w.set_webdav_status(t("请先启用 WebDAV 同步", "enable WebDAV sync first").into());
                return;
            }
            // Encrypted bundle first; a server holding only the pre-encryption
            // file falls back to the legacy JSON export. A bundle that fails to
            // open never falls back, so a wrong passphrase can't double-import.
            let pass = w.get_portable_passphrase().to_string();
            if pass.is_empty() {
                // Without a passphrase only the legacy plaintext file can be
                // tried, and that import is fast (no KDF), so it stays inline.
                let res: Result<(usize, usize), anyhow::Error> = webdav_get_json(
                    &url,
                    &remote_path,
                    &username,
                    &password,
                    accept_invalid_certs,
                )
                .and_then(|json| store.borrow_mut().import_json(&json))
                .map_err(|e| {
                    // The bundle was never tried, so a missing legacy file most
                    // likely means the server only holds the encrypted bundle.
                    anyhow::anyhow!(
                        "{} ({e})",
                        t(
                            "远端可能只保存了加密同步包，请先填写便携包口令",
                            "the server may only hold an encrypted bundle; enter a bundle passphrase first"
                        )
                    )
                });
                let msg = match res {
                    Ok((added, skipped)) => {
                        sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
                        registry.broadcast_config_changed();
                        format!(
                            "{} {}, {} {}{}",
                            t("已导入", "imported"),
                            added,
                            t("跳过", "skipped"),
                            skipped,
                            webdav_status_suffix(accept_invalid_certs)
                        )
                    }
                    Err(e) => format!("{}: {}", t("下载失败", "download failed"), e),
                };
                w.set_webdav_status(msg.into());
                return;
            }
            // Fetch + Argon2 off the UI thread; the bundle timer applies the
            // decrypted config (or runs the legacy fallback) where the store
            // lives.
            let bundle_path = bundle_remote_path(&remote_path);
            w.set_webdav_status(t("下载中...", "downloading...").into());
            let mailbox = mailbox.clone();
            std::thread::spawn(move || {
                let (result, fallback) = match webdav_get_optional(
                    &url,
                    &bundle_path,
                    &username,
                    &password,
                    accept_invalid_certs,
                ) {
                    Ok(Some(body)) => (
                        extract_bundle_data(&body)
                            .map_err(|e| format!("{e:#}"))
                            .and_then(|pack| {
                                ConfigStore::decrypt_bundle(&pass, &pack)
                                    .map_err(|e| format!("{e:#}"))
                            }),
                        None,
                    ),
                    Ok(None) => (
                        // Placeholder: the timer checks `fallback` before
                        // `result`, so this Err never surfaces.
                        Err(String::new()),
                        Some(WebdavFallback {
                            url,
                            remote_path,
                            username,
                            password,
                            accept_invalid_certs,
                        }),
                    ),
                    Err(e) => (Err(format!("{e:#}")), None),
                };
                lock_or_recover(&mailbox).push(BundleDelivery {
                    result,
                    download: true,
                    webdav: true,
                    fallback,
                });
            });
        });
    }

    let tabs_model: Rc<VecModel<TabInfo>> = Rc::new(VecModel::default());
    tabs_model.push(TabInfo {
        id: "welcome".into(),
        title_len: tab_title_len(&t("新标签页", "New tab")),
        title: t("新标签页", "New tab").into(),
        kind: "welcome".into(),
        connected: false,
        local: false,
    });
    window.set_tabs(ModelRc::from(tabs_model.clone()));
    window.set_active_tab_id("welcome".into());

    let terminals_model: Rc<VecModel<TerminalState>> = Rc::new(VecModel::default());
    window.set_terminals(ModelRc::from(terminals_model.clone()));

    // Split-pane layout tree (v0.5). Starts as a single pane owning the welcome
    // tab; tab opens/closes/moves mutate it and re-flatten into the `panes`
    // model. `content_size` is the pane-area px size reported from Slint.
    // In welcome-as-sidebar mode the session list lives in a left panel, so the
    // layout starts empty (no "welcome" tab); otherwise it owns the welcome tab.
    let welcome_sidebar = store.borrow().welcome_as_sidebar();
    let layout: Rc<RefCell<crate::layout::Layout>> = Rc::new(RefCell::new(if welcome_sidebar {
        crate::layout::Layout::new(Vec::new(), String::new())
    } else {
        crate::layout::Layout::new(vec!["welcome".into()], "welcome".into())
    }));
    let content_size: Rc<std::cell::Cell<(f32, f32)>> =
        Rc::new(std::cell::Cell::new((1200.0, 800.0)));

    // Docked-panel edge stacks (#dock-stack): which window panels share an edge
    // at the same time. Restored from config and applied to the panel dock /
    // collapse state below, so a persisted stacked layout survives restart
    // even before the stacked renderer lights up.
    let dock_stacks: Rc<RefCell<DockStacks>> = Rc::new(RefCell::new(DockStacks::default()));
    {
        let saved = store.borrow().dock_stacks();
        {
            let mut ds = dock_stacks.borrow_mut();
            ds.from_saved(&saved);
        }
        for e in &saved {
            for s in &e.slots {
                let edge: slint::SharedString = e.edge.clone().into();
                match s.kind.as_str() {
                    "sidebar" => {
                        window.set_sidebar_dock(edge.clone());
                        window.set_sidebar_collapsed(false);
                    }
                    "welcome" => {
                        window.set_welcome_sidebar_dock(edge.clone());
                        window.set_welcome_collapsed(false);
                    }
                    "quick" => {
                        window.set_quick_panel_dock(edge.clone());
                        window.set_quick_panel_collapsed(false);
                    }
                    "ai" => {
                        window.set_ai_panel_dock(edge.clone());
                        window.set_ai_panel_collapsed(false);
                    }
                    "sftp" => {
                        window.set_sftp_dock(edge.clone());
                        window.set_sftp_collapsed(false);
                    }
                    _ => {}
                }
            }
        }
    }
    // Persistent pane / splitter models. refresh_panes updates these IN PLACE so
    // the rendered `for pane` / `for sp` elements are reused (terminals survive,
    // and the splitter keeps its pointer-grab during a drag).
    let panes_model: Rc<VecModel<PaneInfo>> = Rc::new(VecModel::default());
    window.set_panes(ModelRc::from(panes_model.clone()));
    let splitters_model: Rc<VecModel<SplitterInfo>> = Rc::new(VecModel::default());
    window.set_splitters(ModelRc::from(splitters_model.clone()));
    // The dock-area frame in logical px (window minus title bar), reported by
    // Slint via dock-area-resized. Rust must lay panels out in THIS frame; the
    // default matches the legacy dock-central-w/h defaults so the very first
    // pre-show pass already yields sane geometry.
    let da_size: Rc<std::cell::Cell<(f32, f32)>> = Rc::new(std::cell::Cell::new((1200.0, 800.0)));
    // Docked-panel stack models (#dock-stack): absolute rects for every
    // expanded panel + the dividers between stacked ones. Kept IN PLACE so the
    // panel components reuse their instances (draft/scroll state survives).
    let dock_panels_model: Rc<VecModel<PanelGeomInfo>> = Rc::new(VecModel::default());
    window.set_dock_panels(ModelRc::from(dock_panels_model.clone()));
    let dock_dividers_model: Rc<VecModel<DividerGeomInfo>> = Rc::new(VecModel::default());
    window.set_dock_dividers(ModelRc::from(dock_dividers_model.clone()));
    // First dock-geometry pass after restore; `dock-layout-changed()` keeps it
    // fresh whenever a panel docks, resizes or collapses.
    refresh_dock(
        &window,
        &dock_stacks,
        &dock_panels_model,
        &dock_dividers_model,
        da_size.get(),
    );
    // Any dock/collapse/size change anywhere wants the geometry recomputed.
    {
        let weak = window.as_weak();
        let ds = dock_stacks.clone();
        let pm = dock_panels_model.clone();
        let dm = dock_dividers_model.clone();
        let da = da_size.clone();
        window.on_dock_layout_changed(move || {
            if let Some(w) = weak.upgrade() {
                refresh_dock(&w, &ds, &pm, &dm, da.get());
            }
        });
        // The dock-area frame itself: window resizes change it without any
        // panel event, and `rest` no longer tracks parent sizes, so this is
        // the trigger that keeps the whole layout following the window. (Zen
        // toggles come through `dock-layout-changed` instead — the frame does
        // not resize there.)
        let weak4 = window.as_weak();
        let ds4 = dock_stacks.clone();
        let pm4 = dock_panels_model.clone();
        let dm4 = dock_dividers_model.clone();
        let da4 = da_size.clone();
        window.on_dock_area_resized(move |w: f32, h: f32| {
            let next = (w.max(1.0), h.max(1.0));
            let prev = da4.get();
            if prev == next {
                return;
            }
            // Restore-from-minimize and snap transitions report transient
            // areas: 1-2px DPI-rounding noise and sub-panel degenerate sizes.
            // Skip both without touching da4, so the noise accumulates past
            // the threshold (or the settled size arrives) before any refresh;
            // every other trigger path then keeps computing from a good area.
            if area_is_noise(prev, next, 2.0) || area_is_transient(next.0, next.1) {
                tracing::debug!(
                    "[DOCK] skipped transient area {:.0}x{:.0} (last good {:.0}x{:.0})",
                    next.0,
                    next.1,
                    prev.0,
                    prev.1
                );
                return;
            }
            da4.set(next);
            if let Some(win) = weak4.upgrade() {
                refresh_dock(&win, &ds4, &pm4, &dm4, next);
            }
        });
        // Dragging a divider between two stacked panels updates their ratio.
        // The divider passes its own split window: a top/bottom stack is laid
        // out inside the x range the side stacks left behind, so the cursor
        // position must be mapped against that window, not the dock area.
        let weak2 = window.as_weak();
        let ds2 = dock_stacks.clone();
        let pm2 = dock_panels_model.clone();
        let dm2 = dock_dividers_model.clone();
        let da2 = da_size.clone();
        window.on_stack_split_drag(
            move |edge: SharedString, index: i32, pos: f32, axis_start: f32, axis_len: f32| {
                let edge = edge.to_string();
                // The index arrives as Slint `int`; a negative value would wrap
                // to a huge usize and slip past `set_ratio`'s bounds check
                // (release builds wrap integers instead of panicking).
                let Ok(index) = usize::try_from(index) else {
                    return;
                };
                let ratio = if axis_len > 0.0 {
                    ((pos - axis_start) / axis_len).clamp(0.02, 0.98)
                } else {
                    0.5
                };
                {
                    let mut lay = ds2.borrow_mut();
                    lay.set_ratio(&edge, index, ratio);
                }
                if let Some(w) = weak2.upgrade() {
                    refresh_dock(&w, &ds2, &pm2, &dm2, da2.get());
                }
            },
        );
        // Dragging a stacked panel's in-edge handle resizes it along the dock
        // normal and re-runs the geometry pass.
        let weak3 = window.as_weak();
        let ds3 = dock_stacks.clone();
        let pm3 = dock_panels_model.clone();
        let dm3 = dock_dividers_model.clone();
        let da3 = da_size.clone();
        let panels_model_for_extent = dock_panels_model.clone();
        let terminals_for_extent = terminals_model.clone();
        window.on_panel_extent_drag(
            move |_panel_index: i32, pos: f32| {
                let panel = panels_model_for_extent.row_data(_panel_index as usize);
                if let Some(p) = panel {
                    let kind = p.kind.to_string();
                    let edge = p.edge.to_string();
                    let horizontal_edge = matches!(edge.as_str(), "left" | "right");
                    let thickness = pos.clamp(MIN_THICK, 2600.0);
                    if let Some(w) = weak3.upgrade() {
                        match (kind.as_str(), horizontal_edge) {
                            ("sidebar", true) => w.set_sidebar_width(thickness),
                            ("sidebar", false) => w.set_sidebar_height(thickness),
                            ("welcome", _) => w.set_welcome_sidebar_width(thickness),
                            ("quick", true) => w.set_quick_panel_width(thickness),
                            ("quick", false) => w.set_quick_panel_height(thickness),
                            ("ai", true) => w.set_ai_panel_width(thickness),
                            ("ai", false) => w.set_ai_panel_height(thickness),
                            // SFTP keeps its per-tab size memory (#v0.5): update
                            // the active tab's row too, then mirror both onto the
                            // window so the docked panel + extents stay in sync.
                            // The width floor is the file list's, not the shared
                            // panel one (#505).
                            ("sftp", true) => {
                                let thickness = thickness.max(MIN_SFTP_WIDTH);
                                w.set_sftp_panel_width(thickness);
                                let active = w.get_active_tab_id().to_string();
                                update_terminal_row(&terminals_for_extent, &active, |r| {
                                    r.sftp_panel_width = thickness
                                });
                                sync_active_sftp_to_root(&w, &terminals_for_extent);
                            }
                            ("sftp", false) => {
                                w.set_sftp_panel_height(thickness);
                                let active = w.get_active_tab_id().to_string();
                                update_terminal_row(&terminals_for_extent, &active, |r| {
                                    r.sftp_panel_height = thickness
                                });
                                sync_active_sftp_to_root(&w, &terminals_for_extent);
                            }
                            _ => {}
                        }
                        refresh_dock(&w, &ds3, &pm3, &dm3, da3.get());
                    }
                }
            },
        );
        // Drop a panel back into its own stack: reorder it within the edge by
        // where the pointer landed (#dock-reorder). Only the stacking axis
        // decides the slot; the other axis is a guard, so dragging the panel
        // across the window (a re-dock in progress) can never reorder it.
        let weak5 = window.as_weak();
        let ds5 = dock_stacks.clone();
        let store5 = store.clone();
        let pm5 = dock_panels_model.clone();
        let dm5 = dock_dividers_model.clone();
        let da5 = da_size.clone();
        window.on_stack_reorder(move |edge: SharedString, kind: SharedString, nx: f32, ny: f32| {
            let edge = edge.to_string();
            let kind = kind.to_string();
            let (w, h) = da5.get();
            let side = matches!(edge.as_str(), "left" | "right");
            // Side stacks split the y axis; top/bottom stacks split the x axis.
            let (axis_pos, guard_pos) = if side {
                (ny * h, nx * w)
            } else {
                (nx * w, ny * h)
            };
            let mut midpoints = Vec::new();
            let mut own_band: Option<(f32, f32)> = None;
            for i in 0..pm5.row_count() {
                let Some(p) = pm5.row_data(i) else {
                    continue;
                };
                if p.edge.as_str() != edge.as_str() {
                    continue;
                }
                let (a0, a1, g0, g1) = if side {
                    (p.y, p.y + p.h, p.x, p.x + p.w)
                } else {
                    (p.x, p.x + p.w, p.y, p.y + p.h)
                };
                if p.kind.as_str() == kind.as_str() {
                    // Skip the dragged panel's own centre, or a small nudge
                    // past it would already count as a swap.
                    own_band = Some((g0, g1));
                    continue;
                }
                midpoints.push((a0 + a1) / 2.0);
            }
            let Some((band_lo, band_hi)) = own_band else {
                return;
            };
            // 40px of leeway so a slightly sideways wobble while dragging along
            // the stacking axis still counts as a reorder, not a re-dock attempt.
            if guard_pos < band_lo - 40.0 || guard_pos > band_hi + 40.0 {
                return;
            }
            if midpoints.is_empty() {
                return;
            }
            let target = insertion_index(&midpoints, axis_pos);
            let moved = ds5.borrow_mut().move_to_index(&edge, &kind, target);
            if moved {
                // Persist the new order immediately: a reorder is a discrete
                // gesture, not a transient drag state, so save it now rather
                // than deferring to the next explicit save_layout.
                store5.borrow_mut().set_dock_stacks(ds5.borrow().to_saved());
                if let Err(err) = store5.borrow_mut().save() {
                    tracing::warn!("failed to save dock stacks: {err:#}");
                }
                if let Some(win) = weak5.upgrade() {
                    refresh_dock(&win, &ds5, &pm5, &dm5, da5.get());
                }
            }
        });
    }
    // Snapshot the layout and drop the RefCell guard before mutating Slint
    // models: a model change can synchronously run binding callbacks, and one
    // of those re-entering `layout.borrow*()` while this shared guard is still
    // alive would panic (RefCell already borrowed) → abort in release.
    let lay = layout.borrow().clone();
    refresh_panes(
        &window,
        &lay,
        content_size.get(),
        &tabs_model,
        &panes_model,
        &splitters_model,
    );
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        let cr_dock = dock_stacks.clone();
        let cr_pm = dock_panels_model.clone();
        let cr_dm = dock_dividers_model.clone();
        let cr_da = da_size.clone();
        window.on_content_resized(move |w: f32, h: f32| {
            let next = (w.max(1.0), h.max(1.0));
            if content_size.get() == next {
                return;
            }
            content_size.set(next);
            if let Some(win) = weak.upgrade() {
                let lay = layout.borrow().clone();
                refresh_panes(
                    &win,
                    &lay,
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
                refresh_dock(&win, &cr_dock, &cr_pm, &cr_dm, cr_da.get());
            }
        });
    }
    // Toggle welcome-as-sidebar at runtime: persist, then move the welcome tab in
    // or out of the split-tree (sidebar mode = no welcome tab) and re-flatten.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        let wds_dock = dock_stacks.clone();
        let wds_pm = dock_panels_model.clone();
        let wds_dm = dock_dividers_model.clone();
        let wds_da = da_size.clone();
        window.on_set_welcome_as_sidebar(move |v| {
            // The property is two-way-bound through InterfacePanel and changing
            // it destroys/recreates the Welcome subtree that owns the Switch.
            // Persist first: saving config does not touch the Slint tree, and
            // doing it synchronously means an immediate window close cannot
            // lose the preference. Only the property/layout transition needs
            // to wait until this Switch callback has returned (#323).
            {
                let mut s = store.borrow_mut();
                s.set_welcome_as_sidebar(v);
                if let Err(error) = s.save() {
                    tracing::warn!("failed to persist welcome sidebar preference: {error:#}");
                }
            }
            let weak = weak.clone();
            let layout = layout.clone();
            let content_size = content_size.clone();
            let tabs_model = tabs_model.clone();
            let panes_model = panes_model.clone();
            let splitters_model = splitters_model.clone();
            let wds_dock = wds_dock.clone();
            let wds_pm = wds_pm.clone();
            let wds_dm = wds_dm.clone();
            let wds_da = wds_da.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                if let Some(w) = weak.upgrade() {
                    w.set_welcome_as_sidebar(v);
                    {
                        let mut lay = layout.borrow_mut();
                        update_welcome_tab(&mut lay, v);
                    }
                    let lay = layout.borrow().clone();
                    refresh_panes(
                        &w,
                        &lay,
                        content_size.get(),
                        &tabs_model,
                        &panes_model,
                        &splitters_model,
                    );
                    refresh_dock(&w, &wds_dock, &wds_pm, &wds_dm, wds_da.get());
                }
            });
        });
    }
    // Per-session SFTP state: collapse + sizes live in each tab's TerminalState so
    // split panes / other tabs each keep their own (resizing/collapsing one no
    // longer bleeds onto the rest) (#v0.5).
    {
        let terminals_model = terminals_model.clone();
        let weak = window.as_weak();
        let store = store.clone();
        let ds = dock_stacks.clone();
        let pm = dock_panels_model.clone();
        let dm = dock_dividers_model.clone();
        let da = da_size.clone();
        window.on_set_pane_sftp_collapsed(move |tab_id: SharedString, v: bool| {
            update_terminal_row(&terminals_model, &tab_id, |r| r.sftp_collapsed = v);
            // Every NEW session is seeded from this default, so remember the
            // user's manual fold/unfold as the startup state: the next launch
            // shows SFTP the way it was left, not the factory default.
            {
                let mut s = store.borrow_mut();
                if s.collapse_sftp_default() != v {
                    s.set_collapse_sftp_default(v);
                    let _ = s.save();
                }
            }
            if let Some(w) = weak.upgrade() {
                // The docked panel's (un)dock state is driven by the ACTIVE
                // tab's collapse flag — mirror + recompute geometry (#dock-stack).
                w.set_collapse_sftp_default(v);
                sync_active_sftp_to_root(&w, &terminals_model);
                refresh_dock(&w, &ds, &pm, &dm, da.get());
            }
        });
    }
    {
        // Font zoom shortcuts. Ctrl+=/-/0 zooms one session: a per-tab px
        // override; Ctrl+0 resets that session to the size chosen in
        // Settings. Ctrl+Shift+=/-/0 zooms every session in this window
        // (shared term-font-size, cleared per-tab overrides so it visibly
        // applies to all); Ctrl+Shift+0 resets the window to the Settings
        // size. Zoom never persists globally — the settings stepper owns
        // that. Changing the size re-measures the cell grid and triggers the
        // PTY resize on its own.
        let weak = window.as_weak();
        let store = store.clone();
        let terminals_model = terminals_model.clone();
        window.on_zoom_term_font(move |tab_id: SharedString, direction: i32, window_wide: bool| {
            let Some(w) = weak.upgrade() else {
                return;
            };
            let settings_size = store.borrow().font_size() as i32;
            if window_wide {
                let next = if direction == 0 {
                    settings_size
                } else {
                    w.get_term_font_size() as i32 + direction
                };
                w.set_term_font_size(next.clamp(8, 32) as f32);
                // Per-tab overrides would pin sessions at their old size and
                // defeat "zoom everything", so drop them.
                use slint::Model as _;
                for row in terminals_model.iter() {
                    if row.font_size > 0 {
                        let id = row.id.to_string();
                        update_terminal_row(&terminals_model, &id, |r| r.font_size = 0);
                    }
                }
                return;
            }
            let tab_id = tab_id.to_string();
            if tab_id.is_empty() || tab_id == "welcome" {
                return;
            }
            use slint::Model as _;
            let Some(row) = terminals_model
                .iter()
                .find(|r| r.id.to_string() == tab_id)
            else {
                return;
            };
            if direction == 0 {
                // Back to the Settings size, regardless of the window base.
                update_terminal_row(&terminals_model, &tab_id, |r| {
                    r.font_size = settings_size.clamp(8, 32)
                });
                return;
            }
            let base = if row.font_size > 0 {
                row.font_size
            } else {
                w.get_term_font_size() as i32
            };
            let next = (base + direction).clamp(8, 32);
            update_terminal_row(&terminals_model, &tab_id, |r| r.font_size = next);
        });
    }
    {
        let terminals_model = terminals_model.clone();
        let weak = window.as_weak();
        let ds = dock_stacks.clone();
        let pm = dock_panels_model.clone();
        let dm = dock_dividers_model.clone();
        let da = da_size.clone();
        window.on_set_pane_sftp_height(move |tab_id: SharedString, v: f32| {
            update_terminal_row(&terminals_model, &tab_id, |r| r.sftp_panel_height = v);
            // Mirror to the global default so it persists (saved on close) and
            // seeds new sessions; other open tabs use their own field, unaffected.
            if let Some(w) = weak.upgrade() {
                w.set_sftp_panel_height(v);
                sync_active_sftp_to_root(&w, &terminals_model);
                refresh_dock(&w, &ds, &pm, &dm, da.get());
            }
        });
    }
    {
        let terminals_model = terminals_model.clone();
        let weak = window.as_weak();
        let ds = dock_stacks.clone();
        let pm = dock_panels_model.clone();
        let dm = dock_dividers_model.clone();
        let da = da_size.clone();
        window.on_set_pane_sftp_width(move |tab_id: SharedString, v: f32| {
            // The divider drag hands raw logical pixels; hold the file-list
            // floor here so the stored size and the rendered one agree (#505).
            let v = v.max(MIN_SFTP_WIDTH);
            update_terminal_row(&terminals_model, &tab_id, |r| r.sftp_panel_width = v);
            if let Some(w) = weak.upgrade() {
                w.set_sftp_panel_width(v);
                sync_active_sftp_to_root(&w, &terminals_model);
                refresh_dock(&w, &ds, &pm, &dm, da.get());
            }
        });
    }
    {
        let terminals_model = terminals_model.clone();
        window.on_set_pane_sftp_saved_height(move |tab_id: SharedString, v: f32| {
            update_terminal_row(&terminals_model, &tab_id, |r| r.sftp_saved_height = v);
        });
    }

    // Per-tab connection status + remote resources, the latest local sample,
    // and the local machine's network history (bottom sparkline).
    let tab_statuses: TabStatuses = Arc::new(Mutex::new(HashMap::new()));
    let local_snap: LocalSnap = Arc::new(Mutex::new(SystemSnapshot::default()));
    let local_net_hist: NetHist = Arc::new(Mutex::new(vec![0.0; NET_HISTORY_LEN]));

    // Per-tab display-name overrides set via "Rename session" (tab context
    // menu). Display only — the saved session keeps its own name.
    let tab_titles: Rc<RefCell<HashMap<String, String>>> = Rc::new(RefCell::new(HashMap::new()));

    // Repeated timers created below (system sampler). Parked in WindowState
    // so closing the window stops them instead of leaking.
    let window_timers: Rc<RefCell<Vec<slint::Timer>>> = Rc::new(RefCell::new(Vec::new()));
    let transfer_speeds = super::transfer_speeds::TransferSpeeds::new(&window);

    // Bundle results computed on worker threads are applied here, on the UI
    // thread that owns the store. 250 ms is invisible behind a network round
    // trip and costs one uncontended lock when idle.
    {
        let store = store.clone();
        let weak = window.as_weak();
        let mailbox = bundle_mailbox.clone();
        let sessions_model = sessions_model.clone();
        let registry = registry.clone();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(250),
            move || {
                let pending: Vec<BundleDelivery> = lock_or_recover(&mailbox).drain(..).collect();
                for delivery in pending {
                    let Some(w) = weak.upgrade() else { continue };
                    if let Some(fb) = delivery.fallback {
                        // No bundle on the server: try the legacy plaintext
                        // export (a fast fetch + merge, no KDF) right here.
                        let res = webdav_get_json(
                            &fb.url,
                            &fb.remote_path,
                            &fb.username,
                            &fb.password,
                            fb.accept_invalid_certs,
                        )
                        .and_then(|json| store.borrow_mut().import_json(&json));
                        let msg = match res {
                            Ok((added, skipped)) => {
                                sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
                                registry.broadcast_config_changed();
                                format!(
                                    "{} {}, {} {}{}",
                                    t("已导入", "imported"),
                                    added,
                                    t("跳过", "skipped"),
                                    skipped,
                                    webdav_status_suffix(fb.accept_invalid_certs)
                                )
                            }
                            Err(e) => format!("{}: {}", t("下载失败", "download failed"), e),
                        };
                        w.set_webdav_status(msg.into());
                        continue;
                    }
                    let msg = match delivery.result {
                        Ok(cfg) => match store.borrow_mut().apply_bundle(cfg) {
                            Ok(n) => {
                                sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
                                // Imported settings now live in the store but the
                                // fields above still show the old values; refresh
                                // them or the next upload/download would write
                                // them back on top of the imported config.
                                sync_settings_for_window(&weak, &store.borrow());
                                registry.broadcast_config_changed();
                                format!("{} {}", t("已导入便携包", "bundle imported"), n)
                            }
                            Err(e) => format!("{}: {}", t("导入失败", "import failed"), e),
                        },
                        Err(e) => format!(
                            "{}: {}",
                            t(
                                if delivery.download {
                                    "下载失败"
                                } else {
                                    "导入失败"
                                },
                                if delivery.download {
                                    "download failed"
                                } else {
                                    "import failed"
                                }
                            ),
                            e
                        ),
                    };
                    if delivery.webdav {
                        w.set_webdav_status(msg.into());
                    } else {
                        w.set_portable_status(msg.into());
                    }
                }
            },
        );
        window_timers.borrow_mut().push(timer);
    }

    // Saved secrets that survived a key regeneration: tell the user once,
    // rather than letting them wonder why every session now asks for a
    // password again.
    {
        let lost = store.borrow_mut().take_lost_secrets();
        if lost > 0 {
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                let msg = format!(
                    "{} {}",
                    t(
                        "本机加密密钥已重建，已清空无法解密的已保存密码/密钥数量：",
                        "The local encryption key was rebuilt; undecryptable saved secrets cleared:"
                    ),
                    lost
                );
                let _ = rfd::MessageDialog::new()
                    .set_title(t("部分已保存凭据失效", "some saved credentials lost"))
                    .set_description(msg)
                    .set_level(rfd::MessageLevel::Warning)
                    .show();
            });
        }
    }

    // Credentials an older build stored under the fixed cross-machine import
    // key: they decrypted fine, but they had to be resealed under this
    // machine's key — say so, once, instead of letting it happen silently.
    {
        let recovered = store.borrow_mut().take_recovered_secrets();
        if recovered > 0 {
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                let msg = format!(
                    "{} {}",
                    t(
                        "检测到旧版跨机导入写入的凭据，已改用本机密钥重新加密：",
                        "found credentials written by an older cross-machine import; re-encrypted with the local key:"
                    ),
                    recovered
                );
                let _ = rfd::MessageDialog::new()
                    .set_title(t("已升级凭据加密", "saved credentials re-encrypted"))
                    .set_description(msg)
                    .set_level(rfd::MessageLevel::Info)
                    .show();
            });
        }
    }

    // Expose this window's state to the rest of the process so tabs can be
    // dragged out into a new window or merged into another one (#tab-detach).
    core.window_states.borrow_mut().insert(
        window_id,
        WindowState {
            weak: window.as_weak(),
            main_win: window.clone(),
            handles: handles.clone(),
            bufs: bufs.clone(),
            gates: render_gates.clone(),
            statuses: tab_statuses.clone(),
            sftp_handles: sftp_handles.clone(),
            sftp_last_cwd: sftp_last_cwd.clone(),
            local_snap: local_snap.clone(),
            net_hist: local_net_hist.clone(),
            follow_cd: sftp_follow_cd.clone(),
            layout: layout.clone(),
            dock_stacks: dock_stacks.clone(),
            tabs_model: tabs_model.clone(),
            terminals_model: terminals_model.clone(),
            panes_model: panes_model.clone(),
            splitters_model: splitters_model.clone(),
            timers: window_timers.clone(),
            transfer_speeds: transfer_speeds.clone(),
            content_size: content_size.clone(),
            proc_win: proc_win.clone(),
            sys_win: sys_win.clone(),
            editor_win: editor_win.clone(),
            proc_weak: proc_win.as_weak(),
            sys_weak: sys_win.as_weak(),
        },
    );

    {
        let proc_weak = proc_win.as_weak();
        let handles = handles.clone();
        let statuses = tab_statuses.clone();
        let runtime = runtime.clone();
        proc_win.on_terminate_process(
            move |tab_id: SharedString, pid: SharedString, password: SharedString| {
                let tab_id = tab_id.to_string();
                let Ok(pid) = pid.parse::<u32>() else {
                    set_process_action_error(&proc_weak, t("无效的 PID", "Invalid PID"));
                    return;
                };

                // Re-check the source tab, PID, and owner against the latest sample;
                // the main window may have switched tabs since the menu was opened.
                let ownership = {
                    let states = lock_or_recover(&statuses);
                    states.get(&tab_id).map_or_else(
                        || Err(t("当前会话不可用", "The current session is unavailable")),
                        |status| {
                            status
                                .procs
                                .iter()
                                .find(|p| p.pid == pid)
                                .map(|process| process_needs_root(&status.user, &process.user))
                                .ok_or_else(|| t("进程已退出", "The process has already exited"))
                        },
                    )
                };
                let needs_root = match ownership {
                    Ok(value) => value,
                    Err(message) => {
                        set_process_action_error(&proc_weak, message);
                        return;
                    }
                };
                if needs_root && password.is_empty() {
                    set_process_action_error(
                        &proc_weak,
                        t(
                            "请输入管理员（sudo）密码",
                            "Enter the administrator (sudo) password",
                        ),
                    );
                    return;
                }

                let root_password =
                    needs_root.then(|| crate::config::Secret::new(password.to_string()));
                let response = handles
                    .borrow()
                    .get(&tab_id)
                    .map(|handle| handle.kill_process(pid, root_password));
                let Some(response) = response else {
                    set_process_action_error(
                        &proc_weak,
                        t("SSH 会话不可用", "The SSH session is unavailable"),
                    );
                    return;
                };

                let done_weak = proc_weak.clone();
                runtime.spawn(async move {
                    let result = response
                        .await
                        .unwrap_or_else(|_| crate::ssh::ProcessKillResult {
                            success: false,
                            message: t("SSH 会话已关闭", "The SSH session has closed").to_string(),
                        });
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(pw) = done_weak.upgrade() {
                            pw.set_action_busy(false);
                            pw.set_action_error(!result.success);
                            pw.set_action_status(result.message.into());
                        }
                    });
                });
            },
        );
    }

    // --- Wire callbacks --------------------------------------------------
    wire_session_callbacks(
        &window,
        window_id,
        store.clone(),
        registry.clone(),
        sessions_model.clone(),
        tabs_model.clone(),
        terminals_model.clone(),
        layout.clone(),
        content_size.clone(),
        panes_model.clone(),
        splitters_model.clone(),
        handles.clone(),
        bufs.clone(),
        render_gates.clone(),
        runtime.clone(),
        last_term_size.clone(),
        sftp_handles.clone(),
        sftp_last_cwd.clone(),
        tab_statuses.clone(),
        local_snap.clone(),
        local_net_hist.clone(),
        sftp_follow_cd.clone(),
        core.tab_routes.clone(),
        tab_titles.clone(),
        bundle_mailbox.clone(),
        editor_win.as_weak(),
    );

    // Tab-bar "+" launch menu: local shells + WSL profiles (#launch).
    window.set_launch_shells(launch_shell_model(&store.borrow()));

    // Agent launchers (#427): the tab-bar launch menu, the Quick Connect
    // "agents" group and the Settings → AI agents page.
    let agent_ctx = WinCtx {
        core: core.clone(),
        store: store.clone(),
        registry: registry.clone(),
        handles: handles.clone(),
        sftp_handles: sftp_handles.clone(),
        bufs: bufs.clone(),
        window: window.clone(),
        proc_win: proc_win.clone(),
        sys_win: sys_win.clone(),
        editor_win: editor_win.clone(),
        window_id,
        pending_window_size_restore: pending_window_size_restore.clone(),
        window_size_tracking_ready: window_size_tracking_ready.clone(),
    };
    agents::wire_agent_launchers(&agent_ctx, &sessions_model);

    // Colour theme presets + the immersive-colours switch.
    color_theme::wire(&agent_ctx);

    // Terminal colour schemes (upstream 126072b): dark/light scheme pickers
    // and per-buffer re-apply on change.
    terminal_scheme::wire(&agent_ctx);

    // Custom local shells (upstream #533/#538): Settings › Local shells.
    local_shells::wire_custom_shells(&agent_ctx);

    // Customisable keyboard shortcuts: match-shortcut dispatch + the Shortcuts
    // dialog (record / reset / reset all). The Slint handlers already call
    // `match-shortcut`; without this wiring every chord fell through to the
    // terminal as raw input.
    shortcuts::wire_shortcuts(&agent_ctx);

    // Editor window chrome (frameless drag + resize grip) and always-on-top
    // pin propagation (#450): a pinned main window must not cover its own
    // monitor / editor windows. Main-window process/system-info callbacks are
    // registered inline above (this window's single-file equivalents).
    aux_windows::wire_editor_window_chrome(&agent_ctx);
    aux_windows::wire_pin_on_top(&agent_ctx);

    // Recompute the sidebar whenever the active tab changes (fired from Slint's
    // `changed active-tab-id`).
    {
        let weak = window.as_weak();
        let statuses = tab_statuses.clone();
        let local = local_snap.clone();
        let net = local_net_hist.clone();
        let mirror_terminals = terminals_model.clone();
        let mirror_stacks = dock_stacks.clone();
        let mirror_pm = dock_panels_model.clone();
        let mirror_dm = dock_dividers_model.clone();
        let mirror_da = da_size.clone();
        window.on_refresh_sidebar(move || {
            if let Some(w) = weak.upgrade() {
                refresh_sidebar(&w, &statuses, &local, &net);
                // The docked SFTP panel follows the active tab: mirror that
                // tab's state, then re-run the dock geometry so an SFTP-backed
                // tab (un)docks the panel and local/welcome tabs pull it off.
                // Cheap: refresh_dock diffs rows and skips no-op frames.
                sync_active_sftp_to_root(&w, &mirror_terminals);
                refresh_dock(&w, &mirror_stacks, &mirror_pm, &mirror_dm, mirror_da.get());
            }
        });
    }

    // Switch UI language at runtime.  Static `@tr(...)` text updates live via
    // select_bundled_translation; we additionally refresh the Rust-driven
    // dynamic strings (sidebar status + the welcome tab title).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let tabs_model = tabs_model.clone();
        let registry = registry.clone();
        window.on_set_language(move |code| {
            crate::i18n::set_language(&code.to_string());
            {
                let mut s = store.borrow_mut();
                s.set_language(crate::i18n::current_code().to_string());
                let _ = s.save();
            }
            registry.broadcast_config_changed();
            // Re-translate the welcome tab's dynamic title.
            for i in 0..tabs_model.row_count() {
                if let Some(mut row) = tabs_model.row_data(i) {
                    if row.id.as_str() == "welcome" {
                        row.title_len = tab_title_len(&t("新标签页", "New tab"));
                        row.title = t("新标签页", "New tab").into();
                        tabs_model.set_row_data(i, row);
                    }
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_lang_en(crate::i18n::is_en());
                w.invoke_refresh_sidebar();
            }
        });
    }

    // Theme toggle: flip dark ↔ light, persist the preference, and re-render
    // every open terminal with the new ANSI palette so historical output is
    // also recoloured (not just new output).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs_theme = bufs.clone();
        let proc_weak = proc_win.as_weak();
        let editor_weak = editor_win.as_weak();
        let registry = registry.clone();
        window.on_toggle_theme(move || {
            let Some(w) = weak.upgrade() else { return };
            let next_dark = !w.get_dark_mode();
            // Flip theme + every terminal buffer + re-render (shared with wallpaper).
            apply_dark_mode(&w, &bufs_theme, next_dark);
            // Mirror the flip onto the detached process window and editor
            // window (their Theme globals are separate instances).
            if let Some(p) = proc_weak.upgrade() {
                sync_proc_theme(&w, &p);
            }
            if let Some(e) = editor_weak.upgrade() {
                sync_editor_theme(&w, &e);
            }
            let pref = if next_dark { "dark" } else { "light" };
            {
                let mut s = store.borrow_mut();
                s.set_theme_pref(pref.to_string());
                let _ = s.save();
            }
            registry.broadcast_config_changed();
        });
    }

    // Host-key confirmation dialog (#109-5): the user trusts or rejects the
    // presented server key; the decision fans back out to the blocked SSH/SFTP
    // handler(s) and the next queued prompt for THIS window (if any) is shown.
    {
        let weak = window.as_weak();
        window.on_hostkey_accept(move || {
            if let Some(w) = weak.upgrade() {
                resolve_front_hostkey(&w, window_id, true);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_hostkey_reject(move || {
            if let Some(w) = weak.upgrade() {
                resolve_front_hostkey(&w, window_id, false);
            }
        });
    }

    // Connect-time credential prompt (#110): the user supplies the missing
    // username/password (or cancels); the answer unblocks the SSH/SFTP auth.
    {
        let weak = window.as_weak();
        let registry = registry.clone();
        window.on_cred_accept(move || {
            if let Some(w) = weak.upgrade() {
                let remember = w.get_cred_remember();
                resolve_front_cred(&w, window_id, true);
                // "Remember" persisted new credentials onto the saved session
                // (auth_dialogs::persist_credentials); the registry is not
                // reachable there, so broadcast from this owning callback.
                if remember {
                    registry.broadcast_config_changed();
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_cred_reject(move || {
            if let Some(w) = weak.upgrade() {
                resolve_front_cred(&w, window_id, false);
            }
        });
    }

    // MFA / keyboard-interactive prompt (#86-MFA): the user enters the
    // verification code (or cancels); the answer unblocks the SSH/SFTP auth.
    {
        let weak = window.as_weak();
        window.on_mfa_submit(move || {
            if let Some(w) = weak.upgrade() {
                resolve_front_mfa(&w, window_id, true);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_mfa_cancel(move || {
            if let Some(w) = weak.upgrade() {
                resolve_front_mfa(&w, window_id, false);
            }
        });
    }

    // NIC selector: remember the user's choice for the active tab and refresh.
    {
        let weak = window.as_weak();
        let statuses = tab_statuses.clone();
        let local = local_snap.clone();
        let net = local_net_hist.clone();
        window.on_select_net_iface(move |iface: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let active = w.get_active_tab_id().to_string();
            if let Some(st) = lock_or_recover(&statuses).get_mut(&active) {
                st.selected_iface = iface.to_string();
                st.net_hist = vec![0.0; NET_HISTORY_LEN]; // reset graph for new NIC
            }
            refresh_sidebar(&w, &statuses, &local, &net);
        });
    }

    // Settings: preset download directory (load + pick + open).
    // Default to the user's Downloads folder so files land somewhere sensible
    // without a prompt; only fall back to "ask every time" if we can't locate it
    // (#85). Persist it on first run so the setting reflects the real path.
    if store.borrow().download_dir().is_empty() {
        if let Some(dl) = directories::UserDirs::new()
            .and_then(|u| u.download_dir().map(|p| p.to_string_lossy().to_string()))
        {
            let mut s = store.borrow_mut();
            s.set_download_dir(dl);
            let _ = s.save();
        }
    }
    window.set_download_dir(store.borrow().download_dir().to_string().into());
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_pick_download_dir(move || {
            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                let dir = folder.to_string_lossy().to_string();
                {
                    let mut s = store.borrow_mut();
                    s.set_download_dir(dir.clone());
                    let _ = s.save();
                }
                if let Some(w) = weak.upgrade() {
                    w.set_download_dir(dir.into());
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_open_download_dir(move || {
            let Some(w) = weak.upgrade() else { return };
            let dir = w.get_download_dir().to_string();
            if dir.is_empty() {
                return;
            }
            #[cfg(windows)]
            {
                let _ = std::process::Command::new("explorer").arg(&dir).spawn();
            }
            #[cfg(not(windows))]
            {
                let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
            }
        });
    }

    // Transfer records (download/upload progress + history) shown in the popup.
    let transfers_model: Rc<VecModel<TransferInfo>> = Rc::new(VecModel::default());
    window.set_transfers(ModelRc::from(transfers_model.clone()));
    {
        let tm = transfers_model.clone();
        window.on_clear_transfers(move || {
            transfer_speeds.clear();
            tm.set_vec(Vec::<TransferInfo>::new());
        });
    }
    {
        // Cancel a transfer by id. The id is a UUID unique across sessions, so we
        // broadcast to every SFTP handle — only the owning handle is registered
        // and will act on it (#100).
        let sftp_handles = sftp_handles.clone();
        window.on_cancel_transfer(move |id: SharedString| {
            if let Ok(handles) = sftp_handles.lock() {
                for h in handles.values() {
                    h.cancel_transfer(id.to_string());
                }
            }
        });
    }

    // Open-source libraries shown in the About popup.
    {
        let libs: Vec<SharedString> = [
            t("Slint — 图形界面框架 (GUI)", "Slint — GUI framework"),
            t(
                "russh / russh-keys — SSH 协议实现",
                "russh / russh-keys — SSH protocol",
            ),
            t(
                "russh-sftp — SFTP 文件传输",
                "russh-sftp — SFTP file transfer",
            ),
            t("ssh-key — SSH 密钥解析", "ssh-key — SSH key parsing"),
            t("tokio — 异步运行时", "tokio — async runtime"),
            t(
                "vt100 — 终端 (VT100/xterm) 解析",
                "vt100 — terminal (VT100/xterm) parser",
            ),
            t(
                "sysinfo — 本机资源采集",
                "sysinfo — local resource sampling",
            ),
            t(
                "serde / serde_json — 配置序列化",
                "serde / serde_json — config serialization",
            ),
            t("arboard — 系统剪贴板", "arboard — system clipboard"),
            t("rfd — 原生文件对话框", "rfd — native file dialogs"),
            t(
                "directories — 配置目录定位",
                "directories — config dir lookup",
            ),
            t("chrono — 日期时间处理", "chrono — date/time handling"),
            t("uuid — 唯一标识符", "uuid — unique identifiers"),
            t(
                "anyhow / thiserror — 错误处理",
                "anyhow / thiserror — error handling",
            ),
            t(
                "tracing / tracing-subscriber — 日志",
                "tracing / tracing-subscriber — logging",
            ),
            t(
                "futures / async-trait — 异步辅助",
                "futures / async-trait — async helpers",
            ),
            t("rand — 随机数", "rand — randomness"),
            t(
                "winresource — Windows 图标/资源嵌入",
                "winresource — Windows icon/resource embedding",
            ),
        ]
        .iter()
        .map(|s| (*s).into())
        .collect();
        window.set_about_libs(ModelRc::from(Rc::new(VecModel::from(libs))));
    }

    // New-window entry points: the TabBar button and the Ctrl+Shift+N /
    // ⌘⇧N shortcut both route here. Runs on the UI thread (Slint callback),
    // so open_window can build the window directly. A failed open must not
    // be silent — the click/keystroke already consumed (#multi-window).
    {
        let core = core.clone();
        window.on_new_window_clicked(move || {
            if let Err(e) = open_window(core.clone(), true, None) {
                tracing::warn!("failed to open new window: {e:#}");
            }
        });
    }

    wire_tab_callbacks(
        &window,
        window_id,
        core.clone(),
        tabs_model.clone(),
        terminals_model.clone(),
        layout.clone(),
        content_size.clone(),
        panes_model.clone(),
        splitters_model.clone(),
        handles.clone(),
        bufs.clone(),
        render_gates.clone(),
        sftp_handles.clone(),
        sftp_last_cwd.clone(),
        tab_titles.clone(),
    );
    // Editor tabs (#494): the `editor-docs` rows the embedded `EditorPane`
    // panes and the SFTP panel share, plus the window's tab/panes models.
    let editor_docs_model: Rc<VecModel<EditorDoc>> = Rc::new(VecModel::default());
    window.set_editor_docs(ModelRc::from(editor_docs_model.clone()));
    wire_editor_tabs(
        &window,
        EditorTabModels {
            docs: editor_docs_model,
            tabs: tabs_model.clone(),
            layout: layout.clone(),
            content_size: content_size.clone(),
            panes: panes_model.clone(),
            splitters: splitters_model.clone(),
        },
        sftp_handles.clone(),
    );
    // A local shell's running ssh connection can be saved as a session (#501).
    save_tab_session::wire(
        &window,
        store.clone(),
        registry.clone(),
        sessions_model.clone(),
    );
    wire_sftp_callbacks(
        &window,
        &editor_win,
        sftp_handles.clone(),
        sftp_last_cwd.clone(),
        store.clone(),
        tab_statuses.clone(),
    );
    // AI chat panel: install the (empty) conversation model before wiring the
    // send/stream callbacks, which push into it via downcast.
    window.set_ai_messages(ModelRc::from(Rc::new(VecModel::<AiMessage>::default())));
    wire_ai_callbacks(&window, &store, &bufs);
    let connect_ctx = ConnectCtx {
        weak: window.as_weak(),
        editor: editor_win.as_weak(),
        window_id,
        runtime: runtime.clone(),
        handles: handles.clone(),
        sftp_handles: sftp_handles.clone(),
        sftp_last_cwd: sftp_last_cwd.clone(),
        bufs: bufs.clone(),
        render_gates: render_gates.clone(),
        tab_statuses: tab_statuses.clone(),
        local_snap: local_snap.clone(),
        local_net_hist: local_net_hist.clone(),
        last_term_size: last_term_size.clone(),
        sftp_follow_cd: sftp_follow_cd.clone(),
        store: store.clone(),
        tab_routes: core.tab_routes.clone(),
    };
    wire_key_input(
        &window,
        handles.clone(),
        bufs.clone(),
        last_term_size.clone(),
        store.clone(),
        connect_ctx.clone(),
    );

    // SFTP path bookmarks + recent folders (#500): per-session places stored
    // in the config store and mirrored into every open tab of that session.
    sftp_places::wire_sftp_places(&window, store.clone(), tab_statuses.clone());

    // ── Grace auto-reconnect (#P3-H) ──────────────────────────────────────
    // Periodic scan: reconnect dropped (non-local) tabs in place, up to
    // `max_attempts` consecutive tries each (reset on a successful connect).
    // The timer always runs (cheap: it scans a mostly-empty map); the enabled
    // flag, attempt cap and scan interval are read live from the store, so
    // changing any of them takes effect without reopening the window. The
    // 1 Hz tick only decides *when* to scan; the scan itself is due-time
    // gated, so an idle app pays one early return per second.
    {
        let ctx = connect_ctx.clone();
        let next_scan = Rc::new(std::cell::Cell::new(std::time::Instant::now()));
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(1),
            move || {
                let (enabled, max_attempts, interval) = {
                    let cfg = ctx.store.borrow();
                    (
                        cfg.auto_reconnect_enabled(),
                        cfg.auto_reconnect_max_attempts(),
                        cfg.auto_reconnect_interval_secs(),
                    )
                };
                let due = std::time::Duration::from_secs(interval);
                let now = std::time::Instant::now();
                if !enabled {
                    // Keep the deadline fresh while disabled so re-enabling
                    // doesn't fire an immediate scan.
                    next_scan.set(now + due);
                    return;
                }
                if now < next_scan.get() {
                    return;
                }
                next_scan.set(now + due);
                let dead: Vec<String> = {
                    let statuses = lock_or_recover(&ctx.tab_statuses);
                    statuses
                        .iter()
                        .filter(|(_, st)| {
                            st.state == 2 && !st.is_local && st.reconnect_attempts < max_attempts
                        })
                        .map(|(id, _)| id.clone())
                        .collect()
                };
                for tab_id in dead {
                    // Count the attempt up-front so a failing reconnect still
                    // advances toward the cap (no hot loop); reset on connect.
                    if let Some(st) = lock_or_recover(&ctx.tab_statuses).get_mut(&tab_id) {
                        st.reconnect_attempts += 1;
                    }
                    reconnect_tab_in_place(&tab_id, &ctx);
                }
            },
        );
        // Park the timer in the window's timer vec so it stops on close.
        window_timers.borrow_mut().push(timer);
    }

    // --- Window activity, for idle-CPU throttling (#127) ----------------
    // Idle terminals shouldn't burn CPU: pause the sampler when the window is
    // minimized / occluded, throttle it when it's merely unfocused, and stop the
    // cursor blink whenever the window isn't focused (mirrors what Tabby / Windows
    // Terminal do). The winit event handler below updates this; the blink reads
    // Theme.window-focused.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum WinActivity {
        Active,     // focused & visible → full rate
        Background, // visible but unfocused → throttled
        Hidden,     // minimized / occluded → paused
    }
    let activity = Rc::new(std::cell::Cell::new(WinActivity::Active));
    // Once the user confirms shutdown, every subsequent native/custom close
    // request must pass through without reopening the modal. Windows Installer
    // and Restart Manager may issue more than one close request while replacing
    // the executable (#267).
    let exit_confirmed = Rc::new(Cell::new(false));

    // --- System sampler (1 Hz) ------------------------------------------
    let sampler = Rc::new(Mutex::new(SystemSampler::new()));
    let weak = window.as_weak();
    let tick_sampler = sampler.clone();
    let tick_statuses = tab_statuses.clone();
    let tick_local = local_snap.clone();
    let tick_net = local_net_hist.clone();
    let tick_activity = activity.clone();
    let mut bg_tick = 0u32;
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        SystemSampler::recommended_interval(),
        move || {
            let Some(window) = weak.upgrade() else { return };
            if window.get_sidebar_collapsed() || window.get_zen_mode() {
                return;
            }
            // Skip the (non-trivial) sysinfo refresh + sidebar repaint when no one
            // is looking, and back off to ~5 s when the window is in the background.
            match tick_activity.get() {
                WinActivity::Hidden => return,
                WinActivity::Background => {
                    bg_tick = bg_tick.wrapping_add(1);
                    if bg_tick % 5 != 0 {
                        return;
                    }
                }
                WinActivity::Active => {}
            }
            let snap = {
                let mut s = lock_or_recover(&tick_sampler);
                s.sample()
            };
            // Append the raw local throughput to the bottom-graph ring buffer
            // (normalisation happens at display time so the graph auto-scales).
            push_ring(&mut lock_or_recover(&tick_net), snap.net_bytes_per_sec as f32);
            // Stash the local sample; the sidebar shows it on the welcome tab
            // and in the bottom network graph.
            *lock_or_recover(&tick_local) = snap.clone();

            // Everything (status, CPU/mem/swap, both graphs) follows the
            // active tab; refresh_sidebar reads the stores we just updated.
            if sidebar_updates_visible(&window) {
                refresh_sidebar(&window, &tick_statuses, &tick_local, &tick_net);
            }
        },
    );
    // Keep the timer alive as long as this window exists: it lives in the
    // WindowState timers vec, which forget_window_state drops on close
    // (Slint timers stop when dropped) — no leaking needed.
    window_timers.borrow_mut().push(timer);

    // OS file drag-and-drop → upload to the active session's SFTP directory,
    // but only when the file is dropped over the terminal pane or the docked
    // SFTP panel. Windows queries the OS cursor directly (OLE suppresses
    // WM_MOUSEMOVE mid-drag); macOS/X11/Wayland reuse the last CursorMoved
    // position delivered during drag-hover (#356).
    {
        use i_slint_backend_winit::winit::event::{
            MouseScrollDelta, TouchPhase, WindowEvent as WEvent,
        };
        use i_slint_backend_winit::EventResult;
        let weak = window.as_weak();
        let sh = sftp_handles.clone();
        let wheel_bufs = bufs.clone();
        let close_handles = handles.clone();
        let close_sftp_handles = sftp_handles.clone();
        let ev_proc_weak = proc_win.as_weak();
        let ev_sys_weak = sys_win.as_weak();
        let ev_editor_weak = editor_win.as_weak();
        let ev_store = store.clone();
        let ev_activity = activity.clone();
        let ev_exit_confirmed = exit_confirmed.clone();
        let ev_registry = registry.clone();
        let ev_core = core.clone();
        let ev_ds = dock_stacks.clone();
        let ev_window_size_tracking_ready = window_size_tracking_ready.clone();
        let ev_pending_window_size_restore = pending_window_size_restore.clone();
        // A file drag is in flight (Windows): the hover highlight re-hits the
        // cursor on a timer because the OS swallows mouse motion mid-drag.
        let ev_file_hover = Rc::new(std::cell::Cell::new(false));
        {
            let weak_hl = window.as_weak();
            let ds_hl = dock_stacks.clone();
            let hover = ev_file_hover.clone();
            let hl_timer = slint::Timer::default();
            hl_timer.start(
                slint::TimerMode::Repeated,
                std::time::Duration::from_millis(80),
                move || {
                    if !hover.get() {
                        return;
                    }
                    if let Some(win) = weak_hl.upgrade() {
                        update_file_drop_highlight(&win, &ds_hl);
                    }
                },
            );
            window_timers.borrow_mut().push(hl_timer);
        }
        let mut last_cursor_logical: Option<(f32, f32)> = None;
        let mut macos_wheel_accum = 0.0_f32;
        // Track the inputs that make up WinActivity; recompute on each change.
        let mut focused = true;
        let mut minimized = false;
        let mut occluded = false;
        // Apply the Win11 rounded-corner hint once, on the first event (the HWND
        // reliably exists by then, unlike a pre-run timer) (#166).
        let mut chrome_done = false;
        window
            .window()
            .on_winit_window_event(move |slint_window, event| {
                if !chrome_done {
                    chrome_done = true;
                    if let Some(win) = weak.upgrade() {
                        apply_window_chrome(win.window());
                    }
                }
                // Recompute window activity, push it to the shared cell, and update
                // Theme.window-focused (gates the cursor blink) (#127).
                let apply_activity = |focused: bool, minimized: bool, occluded: bool| {
                    let act = if minimized || occluded {
                        WinActivity::Hidden
                    } else if focused {
                        WinActivity::Active
                    } else {
                        WinActivity::Background
                    };
                    let prev = ev_activity.get();
                    ev_activity.set(act);
                    if let Some(win) = weak.upgrade() {
                        win.set_window_focused(act == WinActivity::Active);
                        win.set_dynamic_ui_active(act == WinActivity::Active);
                        if prev == WinActivity::Hidden && act != WinActivity::Hidden {
                            win.set_terminal_restore_cover(true);
                            let weak2 = weak.clone();
                            slint::Timer::single_shot(
                                std::time::Duration::from_millis(120),
                                move || {
                                    if let Some(w) = weak2.upgrade() {
                                        w.set_terminal_restore_cover(false);
                                    }
                                },
                            );
                        }
                    }
                };
                match event {
                    #[cfg(target_os = "windows")]
                    WEvent::KeyboardInput { event, .. } => {
                        // Microsoft IME can relabel a Ctrl key-up as Process while
                        // retaining the physical Ctrl scan code. Slint drops Process,
                        // so deliver the missing modifier release directly.
                        if let Some(side) = windows_process_ctrl_release(
                            event.state,
                            &event.logical_key,
                            &event.physical_key,
                        ) {
                            let key = match side {
                                CtrlKeySide::Left => slint::platform::Key::Control,
                                CtrlKeySide::Right => slint::platform::Key::ControlR,
                            };
                            slint_window.dispatch_event(
                                slint::platform::WindowEvent::KeyReleased { text: key.into() },
                            );
                            tracing::debug!(
                                "restored Windows IME Process-key Ctrl release side={side:?}"
                            );
                            return EventResult::PreventDefault;
                        }
                    }
                    #[cfg(target_os = "windows")]
                    WEvent::Ime(i_slint_backend_winit::winit::event::Ime::Disabled) => {
                        // Windows emits Ime::Disabled when a composition ends, including
                        // while switching between Chinese and English input methods. The
                        // Slint winit backend intentionally ignores this notification, so
                        // after several switches the native input context can remain
                        // detached and every TextInput appears to stop accepting keys
                        // (#236). Re-associate the window with its current default IME;
                        // the focused Slint TextInput keeps owning text input as before.
                        slint_window.with_winit_window(|window| window.set_ime_allowed(true));
                    }
                    WEvent::DroppedFile(path) => {
                        if let Some(win) = weak.upgrade() {
                            #[cfg(target_os = "windows")]
                            {
                                ev_file_hover.set(false);
                                win.set_file_drop_hl(false);
                            }
                            // On Windows the handler queries the OS cursor
                            // position itself (see `cursor_pos`), since OLE
                            // drag-and-drop hover suppresses WM_MOUSEMOVE.
                            // On macOS/X11/Wayland the compositor delivers
                            // pointer motion during drag-hover as ordinary
                            // CursorMoved events, so the last one we saw is
                            // the drop point (#356).
                            handle_file_drop(
                                &win,
                                &sh,
                                path.clone(),
                                &ev_ds,
                                last_cursor_logical,
                            );
                        }
                    }
                    #[cfg(target_os = "windows")]
                    WEvent::HoveredFile(_path) => {
                        ev_file_hover.set(true);
                        if let Some(win) = weak.upgrade() {
                            update_file_drop_highlight(&win, &ev_ds);
                        }
                    }
                    #[cfg(target_os = "windows")]
                    WEvent::HoveredFileCancelled => {
                        ev_file_hover.set(false);
                        if let Some(win) = weak.upgrade() {
                            win.set_file_drop_hl(false);
                        }
                    }
                    WEvent::CursorMoved { position, .. } => {
                        if let Some(win) = weak.upgrade() {
                            let scale = win.window().scale_factor().max(0.01) as f64;
                            let p = position.to_logical::<f64>(scale);
                            last_cursor_logical = Some((p.x as f32, p.y as f32));
                        }
                    }
                    WEvent::MouseWheel { delta, phase, .. } if cfg!(target_os = "macos") => {
                        let Some((x, y)) = last_cursor_logical else {
                            return EventResult::Propagate;
                        };
                        let Some(win) = weak.upgrade() else {
                            return EventResult::Propagate;
                        };
                        if !macos_terminal_wheel_can_target_terminal(win.get_interface_open()) {
                            // Do not carry a partially accumulated settings gesture
                            // into the terminal after the modal closes.
                            macos_wheel_accum = 0.0;
                            return EventResult::Propagate;
                        }
                        let hit = terminal_wheel_hit(&win, &wheel_bufs, x, y);
                        let wheel_lines = match delta {
                            MouseScrollDelta::LineDelta(_, dy) => dy * 3.0,
                            MouseScrollDelta::PixelDelta(p) => {
                                let scale = win.window().scale_factor().max(0.01) as f64;
                                let p = p.to_logical::<f64>(scale);
                                p.y as f32 / 18.0
                            }
                        };
                        match hit {
                            Some(hit) => {
                                if hit.is_alt {
                                    // PTY forwarding needs whole-notch steps:
                                    // bank fractional frames locally and send one
                                    // arrow burst per crossed notch.
                                    macos_wheel_accum += wheel_lines;
                                    let whole = macos_wheel_accum.trunc() as i32;
                                    if whole != 0 {
                                        macos_wheel_accum -= whole as f32;
                                        win.invoke_terminal_wheel(
                                            hit.tab_id.into(),
                                            whole.signum(),
                                            hit.col,
                                            hit.row,
                                        );
                                    }
                                } else {
                                    // Normal scrollback: forward the raw
                                    // fraction. TermBuffer.scroll_accum banks
                                    // the sub-line remainder so a decaying
                                    // macOS momentum tail glides to a stop
                                    // instead of stepping fixed amounts.
                                    win.invoke_terminal_scroll(hit.tab_id.into(), wheel_lines);
                                }
                                // A finished gesture must not leave its sub-line
                                // remainder behind to be tipped over by an
                                // unrelated touch later on.
                                if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                                    macos_wheel_accum = 0.0;
                                }
                                // Claim every frame above the terminal — including
                                // zero and sub-line ones. Precise devices (trackpad,
                                // Magic Mouse) emit tiny PixelDelta frames, and mere
                                // finger contact on a Magic Mouse produces jitter
                                // frames without any real gesture. Letting those
                                // propagate would hand wheel events to the Slint
                                // TouchArea scroll-event path as well, double-
                                // feeding the same accumulator. Regular mice send
                                // LineDelta notches that always cross a whole line,
                                // which is why they never showed the problem.
                                //
                                // NOTE: arms of the outer `match event` below
                                // are statement position (the closure ends with
                                // its own `EventResult::Propagate`), so claim /
                                // pass decisions here must use explicit returns.
                                return EventResult::PreventDefault;
                            }
                            // Outside the terminal, native Slint scrolling
                            // (sidebars, dialogs) keeps working as before.
                            None => return EventResult::Propagate,
                        }
                    }
                    WEvent::Focused(f) => {
                        focused = *f;
                        apply_activity(focused, minimized, occluded);
                        if *f {
                            #[cfg(target_os = "windows")]
                            slint_window.with_winit_window(|window| window.set_ime_allowed(true));

                            // Some window managers deliver the first Resized event
                            // before the native window belongs to a monitor. Focus
                            // is a reliable second opportunity to seed restoration;
                            // request_inner_size will produce the Resized event that
                            // verifies the native window actually reached the target.
                            if !ev_window_size_tracking_ready.get() {
                                if let Some(win) = weak.upgrade() {
                                    if is_wayland_window(&win.window()) {
                                        ev_pending_window_size_restore.set(None);
                                        ev_window_size_tracking_ready.set(true);
                                        tracing::info!(
                                        "[WINDOW_SIZE] skipped persisted-size restore on Wayland"
                                    );
                                    } else if let Some(preferred) =
                                        ev_pending_window_size_restore.get()
                                    {
                                        if let Some(target) = clamp_window_size_to_monitor(
                                            &win.window(),
                                            Some(preferred),
                                        ) {
                                            tracing::info!(
                                                "[WINDOW_SIZE] focus retry saved={:.0}x{:.0} \
                                             target={:.0}x{:.0}",
                                                preferred.0,
                                                preferred.1,
                                                target.0,
                                                target.1,
                                            );
                                        }
                                    }
                                }
                            }
                            refresh_revealed_main_window(weak.clone());
                        }
                    }
                    WEvent::Occluded(o) => {
                        occluded = *o;
                        apply_activity(focused, minimized, occluded);
                        if !*o {
                            refresh_revealed_main_window(weak.clone());
                        }
                    }
                    WEvent::ScaleFactorChanged { .. } => {
                        // Moving a maximized frameless window between mixed-DPI
                        // monitors can leave Win11 reporting "maximized" while the
                        // native rectangle/render surface still has the old size.
                        refresh_revealed_main_window(weak.clone());
                    }
                    WEvent::Resized(size) => {
                        // A 0-sized resize is how Windows reports a minimize; track it
                        // so we pause the sampler while minimized (#127).
                        minimized = size.width == 0 || size.height == 0;
                        apply_activity(focused, minimized, occluded);
                        // Keep the maximize/restore icon (and resize-edge gating) in
                        // sync when the OS changes the window state (#119).
                        if let Some(win) = weak.upgrade() {
                            let maxed = win
                                .window()
                                .with_winit_window(|ww| ww.is_maximized())
                                .unwrap_or(false);
                            win.set_window_maximized(maxed);
                            if !ev_window_size_tracking_ready.get()
                                && is_wayland_window(&win.window())
                            {
                                // The configure size in this event is authoritative
                                // on Wayland. Accept and persist that actual size;
                                // never chase the advisory saved size (#286).
                                ev_pending_window_size_restore.set(None);
                                ev_window_size_tracking_ready.set(true);
                                tracing::info!(
                                    "[WINDOW_SIZE] accepted compositor size {}x{} on Wayland",
                                    size.width,
                                    size.height
                                );
                            }
                            if !ev_window_size_tracking_ready.get() {
                                if let Some(preferred) = ev_pending_window_size_restore.get() {
                                    let scale = win.window().scale_factor().max(0.01);
                                    let actual =
                                        (size.width as f32 / scale, size.height as f32 / scale);
                                    if let Some(target) =
                                        clamp_window_size_to_monitor(&win.window(), Some(preferred))
                                    {
                                        tracing::info!(
                                            "[WINDOW_SIZE] restore requested saved={:.0}x{:.0} \
                                         target={:.0}x{:.0} actual={:.0}x{:.0} scale={:.2}",
                                            preferred.0,
                                            preferred.1,
                                            target.0,
                                            target.1,
                                            actual.0,
                                            actual.1,
                                            scale,
                                        );
                                        if (actual.0 - target.0).abs() <= 2.0
                                            && (actual.1 - target.1).abs() <= 2.0
                                        {
                                            ev_pending_window_size_restore.set(None);
                                            ev_window_size_tracking_ready.set(true);
                                            tracing::info!(
                                                "[WINDOW_SIZE] restore settled at {:.0}x{:.0}",
                                                actual.0,
                                                actual.1
                                            );
                                        }
                                    } else {
                                        tracing::warn!(
                                            "[WINDOW_SIZE] restore deferred: no monitor available \
                                         saved={:.0}x{:.0}",
                                            preferred.0,
                                            preferred.1,
                                        );
                                    }
                                } else {
                                    // First run: accept the initialized size as the
                                    // baseline, but do not persist this startup event.
                                    ev_window_size_tracking_ready.set(true);
                                }
                                return EventResult::Propagate;
                            }
                            // Record the last user-adjusted windowed size while the
                            // resize event still carries authoritative native
                            // geometry. Persisting only during CloseRequested can
                            // observe an installer/minimize transition instead
                            // (#278). Keep writes in memory here; save_layout flushes
                            // the config on exit.
                            if ev_window_size_tracking_ready.get() && !maxed && !minimized {
                                let scale = win.window().scale_factor().max(0.01);
                                let width = size.width as f32 / scale;
                                let height = size.height as f32 / scale;
                                if width > 200.0 && height > 200.0 {
                                    ev_store.borrow_mut().set_window_size(width, height);
                                    tracing::debug!(
                                        "[WINDOW_SIZE] recorded user size {:.0}x{:.0}",
                                        width,
                                        height
                                    );
                                }
                            }
                        }
                    }
                    WEvent::CloseRequested => {
                        // Confirm before closing if there are open session tabs (#88),
                        // so a stray double-click on the title-bar icon / X / Alt+F4
                        // doesn't silently drop live sessions. Installer/Restart
                        // Manager may send repeated requests, so never intercept
                        // again after the user has confirmed shutdown (#267).
                        if should_block_close(
                            ev_exit_confirmed.get(),
                            !close_handles.borrow().is_empty(),
                        ) {
                            if let Some(win) = weak.upgrade() {
                                win.set_confirm_close_open(true);
                            }
                            return EventResult::PreventDefault;
                        }
                        ev_exit_confirmed.set(true);
                        // No sessions → the window is about to close; persist layout.
                        if let Some(win) = weak.upgrade() {
                            save_layout(&win, &ev_store, &ev_ds);
                            clear_zen_on_close(&win, &ev_store);
                        }
                        // The event is not prevented, so Slint will destroy this
                        // window. Mirror the confirmed custom-close path: tear down
                        // this window's workers and unregister it, quitting the
                        // shared event loop if it was the last one — otherwise a
                        // stale registry entry blocks the quit of a later window.
                        teardown_window(
                            window_id,
                            &close_handles,
                            &close_sftp_handles,
                            &ev_proc_weak,
                            &ev_sys_weak,
                            &ev_editor_weak,
                        );
                        if ev_registry.unregister(window_id) {
                            let _ = slint::quit_event_loop();
                        }
                        forget_window_state(&ev_core, window_id);
                    }
                    _ => {}
                }
                EventResult::Propagate
            });
    }
    // Confirm-close dialog "Close" → actually quit the event loop (#88).
    {
        let weak = window.as_weak();
        let proc_weak = proc_win.as_weak();
        let sys_weak = sys_win.as_weak();
        let editor_weak = editor_win.as_weak();
        let cc_store = store.clone();
        let cc_ds = dock_stacks.clone();
        let close_handles = handles.clone();
        let close_sftp_handles = sftp_handles.clone();
        let close_exit_confirmed = exit_confirmed.clone();
        let close_registry = registry.clone();
        let close_core = core.clone();
        window.on_confirm_close_yes(move || {
            // Guard against a double click and against another close request
            // arriving from Windows Installer while shutdown is in progress.
            if close_exit_confirmed.replace(true) {
                return;
            }
            if let Some(w) = weak.upgrade() {
                w.set_confirm_close_open(false);
                save_layout(&w, &cc_store, &cc_ds);
                clear_zen_on_close(&w, &cc_store);
                let _ = w.hide();
            }
            // Ask every worker to stop before the runtime/event loop is torn
            // down, and hide the detachable monitor windows. Clearing the maps
            // also makes any repeated close request see no live sessions and
            // pass through immediately.
            teardown_window(
                window_id,
                &close_handles,
                &close_sftp_handles,
                &proc_weak,
                &sys_weak,
                &editor_weak,
            );
            if close_registry.unregister(window_id) {
                let _ = slint::quit_event_loop();
            }
            forget_window_state(&close_core, window_id);
        });
    }

    // --- Custom title-bar window controls (#119) --------------------------
    {
        let weak = window.as_weak();
        window.on_win_minimize(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| ww.set_minimized(true));
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_win_maximize_toggle(move || {
            if let Some(w) = weak.upgrade() {
                let now = w.window().with_winit_window(|ww| {
                    let m = !ww.is_maximized();
                    ww.set_maximized(m);
                    m
                });
                if let Some(m) = now {
                    w.set_window_maximized(m);
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        let close_handles = handles.clone();
        let close_sftp_handles = sftp_handles.clone();
        let wc_proc_weak = proc_win.as_weak();
        let wc_sys_weak = sys_win.as_weak();
        let wc_editor_weak = editor_win.as_weak();
        let wc_store = store.clone();
        let wc_ds = dock_stacks.clone();
        let wc_exit_confirmed = exit_confirmed.clone();
        let wc_registry = registry.clone();
        let wc_core = core.clone();
        window.on_win_close(move || {
            if let Some(w) = weak.upgrade() {
                // Mirror the native-X behaviour: confirm if sessions are open.
                if !should_block_close(wc_exit_confirmed.get(), !close_handles.borrow().is_empty())
                {
                    wc_exit_confirmed.set(true);
                    save_layout(&w, &wc_store, &wc_ds);
                    clear_zen_on_close(&w, &wc_store);
                    // Tear down this window's workers and hide its monitor
                    // windows; quit only if it was the last one.
                    teardown_window(
                        window_id,
                        &close_handles,
                        &close_sftp_handles,
                        &wc_proc_weak,
                        &wc_sys_weak,
                        &wc_editor_weak,
                    );
                    let _ = w.hide();
                    if wc_registry.unregister(window_id) {
                        let _ = slint::quit_event_loop();
                    }
                    forget_window_state(&wc_core, window_id);
                } else {
                    w.set_confirm_close_open(true);
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_win_drag(move || {
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
        let weak = window.as_weak();
        window.on_win_resize(move |dir: i32| {
            if let Some(w) = weak.upgrade() {
                let d = match dir {
                    0 => ResizeDirection::North,
                    1 => ResizeDirection::South,
                    2 => ResizeDirection::East,
                    3 => ResizeDirection::West,
                    4 => ResizeDirection::NorthEast,
                    5 => ResizeDirection::NorthWest,
                    6 => ResizeDirection::SouthEast,
                    _ => ResizeDirection::SouthWest,
                };
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_resize_window(d);
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }

    // Position once shown (size is only known after the first frame). The
    // first window restores the saved position (or centers); later windows
    // cascade ~40 px from the newest existing window instead of stacking
    // exactly on top of it. The origin was captured above, before this window
    // was registered (registry.newest() would return this window itself).
    {
        let weak = window.as_weak();
        let origin = cascade_origin
            .and_then(|w| w.upgrade())
            .map(|w| w.window().position());
        let store_for_position = store.clone();
        slint::Timer::single_shot(std::time::Duration::from_millis(30), move || {
            let Some(w) = weak.upgrade() else { return };
            if let Some(pos) = at {
                // Tab detach pinned the window under the cursor (#tab-detach).
                w.window().set_position(pos);
                return;
            }
            match origin {
                Some(pos) => {
                    w.window().set_position(slint::PhysicalPosition::new(
                        pos.x + 40,
                        pos.y + 40,
                    ));
                }
                None => {
                    // Restore the saved window position if available. (0,0) is
                    // the serde default for an unset position, so only fall
                    // through to center_window when both coordinates are 0.
                    let saved = store_for_position.borrow().window_position();
                    let saved_size = store_for_position.borrow().window_size();
                    // The position's logical coordinates were written under the
                    // monitor scale that was active when the window closed.
                    // Re-project them with that saved scale (falling back to the
                    // live scale when an older config has none) so a mixed-DPI
                    // desktop restores the physical spot, not a scaled guess.
                    let saved_scale = store_for_position.borrow().window_scale();
                    let scale = w.window().scale_factor().max(0.01);
                    let pos_scale = if saved_scale > 0.5 { saved_scale } else { scale };
                    if (saved.0 != 0.0 || saved.1 != 0.0)
                        && saved.0.is_finite()
                        && saved.1.is_finite()
                    {
                        let size = w.window().size(); // physical pixels
                        // Mirror the size restore: prefer the persisted size
                        // (what the Resized handler will settle on) so the
                        // overlap check uses the rect the window will actually
                        // have, not a transient default.
                        let rw = if saved_size.0 > 0.0 {
                            saved_size.0 * scale
                        } else {
                            size.width as f32
                        };
                        let rh = if saved_size.1 > 0.0 {
                            saved_size.1 * scale
                        } else {
                            size.height as f32
                        };
                        let rect = (
                            saved.0 as f64 * pos_scale as f64,
                            saved.1 as f64 * pos_scale as f64,
                            rw as f64,
                            rh as f64,
                        );
                        let monitors = monitor_rects(&w.window());
                        let plausible = saved_position_plausible(saved.0, saved.1);
                        // No monitor geometry yet — the first timer tick can run
                        // before the winit window is attached to a display — so
                        // trust the saved position instead of centering with a
                        // zero size (which would pin the top-left to the screen
                        // centre and push a large window's bottom/right panels
                        // off-screen). Only a degenerate saved coordinate (the
                        // minimized -32000 sentinel, or an absurd value) falls
                        // back to centering.
                        let restore = if monitors.is_empty() {
                            plausible
                        } else {
                            window_rect_visible_on_any_monitor(rect, &monitors)
                        };
                        if restore {
                            tracing::info!(
                                "[WINDOW_POS] restore saved=({:.1},{:.1}) scale={:.2} monitors={}",
                                saved.0,
                                saved.1,
                                pos_scale,
                                monitors.len()
                            );
                            w.window().set_position(slint::PhysicalPosition::new(
                                (saved.0 * pos_scale) as i32,
                                (saved.1 * pos_scale) as i32,
                            ));
                        } else {
                            // The saved position fell off every screen (a second
                            // monitor was detached, or a minimized HWND's
                            // (-32000,-32000) sentinel was persisted), or it is
                            // degenerate with no monitors to verify against.
                            // Center on the primary instead of opening invisible.
                            tracing::warn!(
                                "[WINDOW_POS] center saved=({:.0},{:.0}) scale={:.2} monitors={} plausible={}",
                                saved.0,
                                saved.1,
                                pos_scale,
                                monitors.len(),
                                plausible
                            );
                            center_window(&w, Some(saved_size));
                        }
                    } else {
                        center_window(&w, Some(saved_size));
                    }
                }
            }
        });
    }

    // The old entry point was window.run(), which shows the window before
    // spinning the loop. run_event_loop() does not, so display it here —
    // without this the app starts but no window ever appears (#multi-window).
    window.show().context("failed to show window")?;
    // window.show() alone does not guarantee a complete first paint on every
    // platform/renderer: the docked bottom/right panels and SFTP toolbar
    // buttons can come up blank until the next Occluded/Focus event triggers
    // a redraw (regression from window.run() → run_event_loop(), #multi-window).
    refresh_revealed_main_window(window.as_weak());

    Ok(window_id)
}

/// Center the window on the primary monitor's work area (Windows).
#[cfg(windows)]
pub(crate) fn center_window(win: &AppWindow, expected: Option<(f32, f32)>) {
    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn SystemParametersInfoW(action: u32, uiparam: u32, pvparam: *mut Rect, winini: u32)
            -> i32;
    }
    const SPI_GETWORKAREA: u32 = 0x0030;

    let size = win.window().size(); // physical pixels
    // Before the first paint winit has no size yet; fall back to the saved /
    // preferred logical size so a zero-size window isn't centred by pinning
    // its top-left to the screen centre (which pushes a large window's bottom
    // and right edges — and its docked panels — off-screen).
    let (w, h) = if size.width == 0 || size.height == 0 {
        let scale = win.window().scale_factor().max(0.01);
        let (ew, eh) = expected
            .filter(|e| e.0 > 0.0 && e.1 > 0.0)
            .unwrap_or((1440.0, 900.0));
        ((ew * scale).round() as u32, (eh * scale).round() as u32)
    } else {
        (size.width, size.height)
    };
    let mut wa = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa, 0) };
    if ok == 0 {
        return;
    }
    let area_w = (wa.right - wa.left).max(0) as u32;
    let area_h = (wa.bottom - wa.top).max(0) as u32;
    let x = wa.left + ((area_w.saturating_sub(w)) / 2) as i32;
    let y = wa.top + ((area_h.saturating_sub(h)) / 2) as i32;
    win.window()
        .set_position(slint::PhysicalPosition::new(x, y));
}

/// Center the window on the primary monitor (X11 / macOS; Windows uses the
/// work-area version above). On Wayland the compositor owns the position, so
/// `set_outer_position` is advisory and a no-op — which is fine, since a
/// centered-on-primary request is always on a visible screen.
#[cfg(not(windows))]
pub(crate) fn center_window(win: &AppWindow, expected: Option<(f32, f32)>) {
    let _ = win.window().with_winit_window(|ww| {
        let scale = ww.scale_factor().max(0.01);
        // Before the first paint winit may not know the current monitor yet;
        // fall back to the primary so a persisted position that is off every
        // screen still lands somewhere a user can find it.
        let Some(monitor) = ww.current_monitor().or_else(|| ww.primary_monitor()) else {
            return;
        };
        let size = monitor.size();
        let pos = monitor.position();
        // Zero-size windows would centre by pinning their top-left to the
        // screen centre; use the saved / preferred logical size as a stand-in.
        let inner = ww.inner_size();
        let (w, h) = if inner.width == 0 || inner.height == 0 {
            let (ew, eh) = expected
                .filter(|e| e.0 > 0.0 && e.1 > 0.0)
                .unwrap_or((1440.0, 900.0));
            (ew as f64, eh as f64)
        } else {
            (inner.width as f64 / scale, inner.height as f64 / scale)
        };
        let mon_w = size.width as f64 / scale;
        let mon_h = size.height as f64 / scale;
        let mon_x = pos.x as f64 / scale;
        let mon_y = pos.y as f64 / scale;
        ww.set_outer_position(i_slint_backend_winit::winit::dpi::LogicalPosition::new(
            mon_x + (mon_w - w).max(0.0) / 2.0,
            mon_y + (mon_h - h).max(0.0) / 2.0,
        ));
    });
}

/// Re-push every setting the UI still displays into the live window.
///
/// `apply_bundle` replaces the whole store in one go, and the upload/download
/// and settings-save callbacks read these window fields back into the store
/// before doing work. Without this, the first click after a bundle import
/// would silently restore the pre-import settings and upload to the old
/// server. Called once at startup and again right after a successful import.
pub(crate) fn sync_settings_for_window(window: &slint::Weak<AppWindow>, store: &ConfigStore) {
    let Some(window) = window.upgrade() else { return };
    window.set_webdav_enabled(store.webdav_enabled());
    window.set_webdav_url(store.webdav_url().into());
    window.set_webdav_username(store.webdav_username().into());
    window.set_webdav_password(store.webdav_password().into());
    window.set_webdav_remote_path(store.webdav_remote_path().into());
    window.set_webdav_accept_invalid_certs(store.webdav_accept_invalid_certs());
    window.set_ai_base_url(store.ai_base_url().into());
    window.set_ai_api_key(store.ai_api_key().into());
    window.set_ai_model(store.ai_model().into());
    window.set_download_dir(store.download_dir().to_string().into());
    window.set_sync_upload_enabled(store.sync_upload());
}

// ---------------------------------------------------------------------------
// Model helpers
// ---------------------------------------------------------------------------

pub(crate) fn sync_sessions_for_window(
    window: &slint::Weak<AppWindow>,
    store: &ConfigStore,
    model: &VecModel<SessionInfo>,
) {
    let Some(window) = window.upgrade() else { return };
    let query = window.get_host_search_query().to_string();
    // Prefer in-place row updates: they keep the list's scroll position and
    // any running drag alive and skip the reallocation. A full set_vec
    // rebuild — which destroys the dragging row's pointer grab — happens
    // only when the row count changed, and the revision bump tells Welcome
    // to clear its stale drag state in exactly that case.
    if !refresh_session_rows_in_place(store, model, &query) {
        window.set_sessions_revision(window.get_sessions_revision() + 1);
    }
}