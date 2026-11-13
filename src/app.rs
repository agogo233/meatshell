//! Top-level UI state machine.
//!
//! Responsibilities:
//!   * Load the config store and expose sessions to Slint.
//!   * Drive the 1-Hz system sampler.
//!   * Manage the tab list + per-tab `SessionHandle` map.
//!   * Route Slint callbacks to the right domain module.
mod agents;
mod auth_dialogs;
mod aux_windows;
mod ai;
mod color_theme;
mod terminal_scheme;
pub(crate) mod core;
mod editor_host;
mod editor_tabs;
mod editor_syntax;
mod file_drop;
mod fonts;
mod helpers;
mod hit_test;
#[cfg(windows)]
mod jump_list;
mod key_input;
pub mod launch;
mod local_shells;
mod open_window;
mod panes;
mod port_forward;
mod session_trigger;
mod quick_commands;
mod resource_ui;
mod save_tab_session;
mod session_callbacks;
mod session_event;
mod session_models;
mod session_runtime;
mod session_test;
mod shortcuts;
mod sftp_callbacks;
mod sftp_local;
mod sftp_places;
mod sftp_ui;
mod sidebar;
mod single_instance;
mod tab_callbacks;
mod tab_session_log;
mod tab_transfer;
mod dock_stacks;
mod terminal_ui;
mod term_output;
mod transfer_rate;
mod transfer_speeds;
mod tray;
mod webdav;
mod window;

// [upstream] 上游 yituorou 将 app.rs 拆分为 aux_windows/open_window/panes/
// session_callbacks/settings_callbacks/misc_callbacks/tunnel_callbacks/
// window_events/key_input/hit_test/file_drop/fonts/helpers/term_output/
// session_editor 等子模块。本 fork 已完成"全量靠拢"自拆（2026-11）：按
// 上游形状把 app.rs 的 term_output/helpers/fonts/hit_test/file_drop/panes/
// key_input/open_window/session_callbacks 拆为独立模块；设置/misc/editor
// 接线仍内联在 open_window（fork 闭包捕获密集，经评估不强行拆分）。
// [merge-2026-10] 采纳上游编辑器标签页形态（#494）：启用上游新模块
// editor_host / editor_tabs / editor_syntax（语法 span 喂给 vendored
// i-slint-core 的原生 colour spans）、agents / tray、可定制快捷键
// shortcuts、SFTP places 书签 sftp_places，以及副窗接线 aux_windows。
// [attic-2026-10] 未接线/已替代的旧拆分副本保留在 src/attic/ 作为归档
// 层（rustc 对未引用模块零告警）；settings_callbacks/misc_callbacks 已由
// 8e3bb74 删除、helpers/session_callbacks 已被自拆新版替代并删除，
// session_test 已迁回 app/（5b68df5 启用上游 EditorTest 票据）、
// drag_out 暂缓、editor/tests 仍待显式决策取用。

use self::ai::*;
use self::auth_dialogs::*;
use self::editor_host::*;
use self::editor_tabs::*;
pub(crate) use self::file_drop::*;
use self::fonts::*;
use self::helpers::*;
pub(crate) use self::hit_test::*;
pub(crate) use self::key_input::*;
pub(crate) use self::open_window::*;
pub(crate) use self::panes::*;
use self::port_forward::*;
pub(crate) use self::session_callbacks::*;
use self::session_trigger::*;
use self::quick_commands::*;
use self::resource_ui::*;
use self::session_event::*;
use self::session_models::*;
use self::session_runtime::*;
use self::session_test::*;
use self::sftp_callbacks::*;
use self::sftp_local::*;
use self::sftp_ui::*;
use self::sidebar::*;
use self::dock_stacks::*;
use self::tab_callbacks::*;
use self::tab_transfer::*;
use self::terminal_ui::*;
use self::term_output::*;
use self::webdav::*;
use self::window::*;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// Max bytes merged into one Output event before starting a fresh chunk (#209).
/// The upstream perf fix lowered this from 64 KiB to 16 KiB; the pump-side
/// merge in session_runtime shares the term_output definition.
use term_output::OUTPUT_MERGE_BYTE_CAP;

/// Lock a mutex, recovering the guard from a poisoned lock instead of panicking.
/// A panic on any background thread must not take the whole UI down with it: the
/// data behind a poisoned mutex is as good as it was, so reuse it.
fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// Params for the legacy (pre-encryption) JSON import the WebDAV download
/// falls back to when the server holds no bundle. Runs on the UI thread: a
/// plain fetch + merge, no Argon2.
struct WebdavFallback {
    url: String,
    remote_path: String,
    username: String,
    password: String,
    accept_invalid_certs: bool,
}

/// A portable bundle decrypted on a worker thread, waiting for the UI-thread
/// timer to apply it: the store is `Rc`-bound to the UI thread, so only the
/// already-decrypted config can cross back. `download` picks the failure
/// wording ("下载失败" for a fetch, "导入失败" for a local file); `webdav`
/// routes the status message to the WebDAV page instead of the portable one;
/// `fallback` carries the legacy-import params when no bundle was on the
/// server.
struct BundleDelivery {
    result: Result<ConfigFile, String>,
    download: bool,
    webdav: bool,
    fallback: Option<WebdavFallback>,
}

type BundleMailbox = Arc<Mutex<Vec<BundleDelivery>>>;

/// Output parsed between UI-flush checkpoints during sustained traffic.
#[cfg(test)]
#[path = "../tests/app/terminal_ingest/mod.rs"]
mod ingest_frame_tests;

use anyhow::{Context, Result};
use i_slint_backend_winit::WinitWindowAccessor;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use tokio::runtime::Runtime;

use crate::app::core::{AppCore, TabRoute, TabRoutes, WindowRegistry, WindowState};
use crate::config::{
    is_reserved_session_group, named_display_groups, AuthMethod, ConfigFile, ConfigStore,
    OutputHighlightRule, Secret, Session, SessionKind, SessionLogMode,
};
use crate::i18n::t;
use crate::layout::{LogicalRect, TerminalWheelHit};
use crate::resource::system::{format_bytes_per_sec, format_mem};
use crate::resource::{
    LocalSnap, NetHist, TabStatus, TabStatuses,
};
use crate::resource::{SystemSampler, SystemSnapshot};
use crate::session::{ConnectCtx, PendingCred, PendingHostKey, PendingMfa};
use crate::sftp::{
    download_target_path, spawn_sftp, DedupPolicy, DownloadConflict, SftpHandles, SftpLastCwd,
    UploadDecision,
};
use crate::ssh::{
    format_mtime, format_size, push_authorized_keys, spawn_session, test_session_auth,
    ProcInfo, SessionCommand, SessionEvent, SessionHandle, SystemDetails,
};
#[cfg(windows)]
use crate::terminal::c0_letter_key_down;
use crate::terminal::{
    bare_ctrl_marker_workaround_enabled, BACK_TAB_BYTES, cell_prefix, clear_pending_paste,
    compile_output_rules, default_command, encode_command_bar_input, encode_mouse_event,
    encode_pasted_text, is_back_tab, is_terminal_interrupt, key_to_pty_bytes,
    paste_requires_large_review, should_drop_bare_ctrl_marker, store_pending_paste,
    take_pending_paste, terminal_uses_bracketed_paste, ActionLinkKind, CsiState,
    OutputHighlightPreset, PendingPaste, RenderGates,
    TabRenderGate, TermBuffer, TermBufferHandle, TermBuffers,
};
#[cfg(test)]
use crate::terminal::{
    build_row, highlight_plain_output, log_level_marker, normalize_pasted_newlines,
    text_cell_width, vt_span_colors, CompiledOutputRule, HistLine, HistSpan, Line,
};
#[cfg(any(target_os = "windows", test))]
use crate::terminal::{windows_process_ctrl_release, CtrlKeySide};
use crate::ui::*;
use crate::webdav::WebDavAcceptAnyCertVerifier;

fn tab_title_len(title: &str) -> i32 {
    title
        .chars()
        .map(|ch| if ch.is_ascii() { 1usize } else { 2usize })
        .sum::<usize>()
        .min(i32::MAX as usize) as i32
}

fn should_block_close(exit_confirmed: bool, has_live_sessions: bool) -> bool {
    !exit_confirmed && has_live_sessions
}


/// Number of samples kept for the sparkline.
const NET_HISTORY_LEN: usize = 60;

// UI-thread handle to the process core, published by `run()` before the
// event loop starts. Cross-thread callers (the single-instance IPC
// listener) run a capture-less closure via `invoke_from_event_loop` and
// fetch the non-Send `Rc<AppCore>` from here instead of moving it across
// threads.
thread_local! {
    static NEW_WINDOW_CORE: RefCell<Option<Rc<AppCore>>> = const { RefCell::new(None) };
}

/// Embed the app icon PNG into the binary and set it as the X11 window icon.
///
/// On X11, the taskbar/dock icon for a running window comes from the
/// `_NET_WM_ICON` property, which winit sets via `Window::set_window_icon`.
/// When the app runs as a bare AppImage (or from a plain directory without
/// running install-linux.sh) there is no installed .desktop + icon, so the
/// dock falls back to a generic gear.  This call fixes that for X11 sessions.
///
/// On Wayland the dock icon is resolved by the compositor from the XDG
/// app-id → .desktop file mapping; `set_window_icon` is a no-op there, so
/// Wayland users still need AppImageLauncher or install-linux.sh for the
/// dock icon.  The `icon:` property in app.slint handles the in-title-bar
/// icon on both backends without any runtime work.
///
/// Windows gets its icon from the `.ico` embedded by winresource at link
/// time; macOS from the app bundle — neither path needs runtime decoding.
pub fn run(intent: crate::app::launch::LaunchIntent) -> Result<()> {
    // Load the renderer preference before creating any Slint window. Reuse the
    // same store for the rest of the app so startup does not read the config
    // twice merely to select a backend (#280).
    let config = ConfigStore::load().context("failed to load config")?;

    // Windows frameless-window attributes must be fixed before the first Slint
    // window is created; doing it afterwards leaves some Win10 machines with an
    // invisible frame that shifts mouse hit testing (#193).
    #[cfg(windows)]
    setup_windows_platform(config.renderer_mode());

    #[cfg(target_os = "linux")]
    setup_linux_platform(config.renderer_mode());

    // Immersive native title bar on macOS (must precede the first window).
    #[cfg(target_os = "macos")]
    setup_macos_platform(config.renderer_mode());

    // --- Single-instance coordination -------------------------------------
    // A second `meatshell --new-window` forwards to us and exits; we never
    // run two GUI instances for that entry point (Chrome-style). Plain
    // launches pass forward=false and never forward: if the endpoint is
    // taken they run as an independent second instance. IPC failures fall
    // through to a normal launch rather than blocking the app.
    let si_path = crate::app::single_instance::socket_path();
    let instance = match crate::app::single_instance::acquire(&si_path, intent.new_window) {
        Ok(i) => Some(i),
        Err(e) => {
            tracing::warn!("single-instance acquire failed: {e}");
            None
        }
    };
    if intent.new_window {
        if let Some(crate::app::single_instance::Instance::Forwarded) = instance {
            return Ok(());
        }
        // We are the primary (or IPC failed): fall through and open a window.
    }

    // --- Runtime + store -------------------------------------------------
    let runtime = Arc::new(Runtime::new().context("failed to start tokio runtime")?);
    let store = Rc::new(RefCell::new(config));
    // Reachable from the Slint-thread event handler for recording terminal
    // commands into history (#113).
    HISTORY_STORE.with(|s| *s.borrow_mut() = Some(store.clone()));

    let core = Rc::new(AppCore {
        runtime,
        store,
        registry: Rc::new(WindowRegistry::default()),
        window_states: Rc::new(RefCell::new(HashMap::new())),
        tab_routes: Arc::new(Mutex::new(HashMap::new())),
    });

    // IPC listener: forwarded "new-window" requests arrive on the listener
    // thread, but open_window() must run on the Slint UI thread — and
    // AppCore holds Rc state, so it cannot be captured by the
    // invoke_from_event_loop closure. The closure therefore captures nothing
    // and fetches the core from a UI-thread-local set just before the event
    // loop starts; until then (early startup) the listener retries briefly so
    // no request is lost.
    if let Some(crate::app::single_instance::Instance::Primary { listen }) = instance {
        std::thread::spawn(move || {
            listen.spawn(move |msg| {
                if msg == "new-window" {
                    tracing::info!("single-instance: new-window request received");
                    // invoke_from_event_loop fails forever once the event
                    // loop is gone (app quitting) — retry only briefly so
                    // this listener callback cannot spin indefinitely.
                    let deadline =
                        std::time::Instant::now() + std::time::Duration::from_secs(10);
                    while slint::invoke_from_event_loop(|| {
                        NEW_WINDOW_CORE.with(|c| {
                            if let Some(core) = c.borrow().clone() {
                                match open_window(core.clone(), true, None) {
                                    Ok(window_id) => {
                                        // The request came from an OS entry
                                        // point while we may be in the
                                        // background — bring the new window
                                        // to the front (best effort).
                                        if let Some(st) =
                                            core.window_states.borrow().get(&window_id)
                                        {
                                            if let Some(w) = st.weak.upgrade() {
                                                raise_to_front(&w);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("failed to open forwarded window: {e:#}")
                                    }
                                }
                            }
                        });
                    })
                    .is_err()
                    {
                        if std::time::Instant::now() >= deadline {
                            tracing::warn!(
                                "single-instance: event loop unreachable, dropping new-window request"
                            );
                            break;
                        }
                        // The event loop provider appears after startup begins;
                        // retry instead of dropping an explicit user action.
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            });
        });
    }

    // Set the Wayland app_id / X11 WM_CLASS *before* the window is created so
    // the Linux desktop shell can match the running window to the installed
    // `meatshell.desktop` entry and show our icon in the dock/taskbar.  (On
    // Windows the icon comes from the embedded .ico, so this is a no-op there.)
    let _ = slint::set_xdg_app_id("meatshell");

    // Taskbar jump list ("新建窗口") on Windows: register before the first
    // window shows so the entry is available immediately. Failure is
    // warn-only and never blocks startup.
    #[cfg(windows)]
    crate::app::jump_list::register_new_window_task();

    open_window(core.clone(), false, None)?;
    // System tray: keeps sessions running while all windows are hidden. The
    // timer owns the tray icon, so it must outlive the event loop below.
    let tray_timer = tray::install(core.clone());

    // Publish the core to the UI thread so the IPC listener's
    // invoke_from_event_loop closures can open windows without capturing the
    // non-Send Rc<AppCore> (see the listener above).
    NEW_WINDOW_CORE.with(|c| *c.borrow_mut() = Some(core.clone()));

    // Global loop: window.run() returns when *its* window closes, which is
    // wrong once several windows share the loop.
    let loop_result = slint::run_event_loop();
    if let Err(e) = &loop_result {
        tracing::warn!(
            "event loop exited with error ({e:#}); running bounded shutdown anyway"
        );
    }
    // Bound the shutdown instead of relying on Rust's drop glue: tokio's
    // Runtime Drop waits indefinitely for tasks that never finished
    // (telnet/local/sftp pumps, spawn_blocking helpers) — the observed
    // windowless lingering process on Windows 11. Take ownership of the
    // runtime when all holders have gone, give stragglers two seconds, then
    // hard-exit unconditionally. The event-loop error path goes through here
    // too: propagating with `?` would hand the runtime to the TLS destructor
    // chain, where a wedged blocking thread could hang the process forever.
    // Drop the tray before exit so no stale icon outlives the process.
    tray::clear();
    drop(tray_timer);
    NEW_WINDOW_CORE.with(|c| *c.borrow_mut() = None);
    if let Ok(core_owned) = Rc::try_unwrap(core) {
        if let Ok(runtime) = Arc::try_unwrap(core_owned.runtime) {
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
        }
    }
    std::process::exit(0);
}


/// Max rows in the session-dialog jump editor; matches the dialog's own cap
/// (session_dialog.slint) and the route bound in `config::jump_chain` (#211).
const MAX_JUMP_EDITOR_ROWS: usize = 16;

/// Preserve dangling ids as an unselected row so saving cannot silently go
/// direct: an id that no longer exists renders as row 0 ("no jump host")
/// and fails validation instead of dropping the hop.
fn jump_rows(ids: Vec<String>, choices: &ModelRc<SharedString>) -> ModelRc<JumpHop> {
    ModelRc::from(Rc::new(VecModel::from(
        ids.into_iter()
            .map(|id| {
                let choice = choices
                    .iter()
                    .position(|value| value.as_str() == id)
                    .map(|index| index as i32)
                    .unwrap_or(0);
                JumpHop {
                    id: id.into(),
                    choice,
                    placeholder: false,
                }
            })
            .collect::<Vec<_>>(),
    )))
}

/// Apply an edit to the dialog's jump rows, refreshing the model and clearing
/// any stale "test connection" status the edit invalidates.
fn update_jump_rows(window: &AppWindow, update: impl FnOnce(&mut Vec<JumpHop>)) {
    let mut rows = window.get_dialog_jumps().iter().collect::<Vec<_>>();
    update(&mut rows);
    window.set_dialog_jumps(ModelRc::from(Rc::new(VecModel::from(rows))));
    window.set_dialog_test_status("".into());
}

/// Reorder jump rows by drag-and-drop; out-of-range indices are ignored.
fn move_jump_row(rows: &mut Vec<JumpHop>, from: i32, to: i32) {
    if from >= 0 && to >= 0 && (from as usize) < rows.len() && (to as usize) < rows.len() {
        let row = rows.remove(from as usize);
        rows.insert(to as usize, row);
    }
}

/// Parse the batch-import textarea (#150). Each non-empty, non-`#` line is
/// `host|port|user|password|name`; trailing fields are optional (port → 22,
/// user → root, password → none, name → user@host). A leading header row such as
/// `host|port|username|password|name` is skipped. Dedup happens at the call site.

#[cfg(test)]
#[path = "../tests/app/welcome_sidebar/mod.rs"]
mod welcome_sidebar_tests;

// ---------------------------------------------------------------------------
// Tab callbacks
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "../tests/app/terminal_input/mod.rs"]
mod key_tests;

#[cfg(test)]
#[path = "../tests/app/terminal_rendering/mod.rs"]
mod selection_tests;

#[cfg(test)]
#[path = "../tests/app/output_highlighting/mod.rs"]
mod log_highlight_tests;

#[cfg(test)]
#[path = "../tests/app/text_editor/mod.rs"]
mod text_editor_tests;

#[cfg(test)]
#[path = "../tests/app/modal_layers.rs"]
mod modal_layers_tests;

#[cfg(test)]
mod jump_route_tests {
    use super::*;

    #[test]
    fn session_from_draft_saves_the_jump_chain_and_clears_empty_rows() {
        let saved = Session::new_empty();
        let mut draft = SessionDraft::default();
        draft.id = saved.id.clone().into();
        draft.kind = "ssh".into();
        draft.jumps = jump_rows(vec!["a".into(), "b".into()], &ModelRc::default());
        let session = session_from_draft(&draft, Some(&saved), vec![], vec![]);
        assert_eq!(session.jump_session_ids, ["a", "b"]);
        assert!(session.jump_session_id.is_empty());
        // An SSH route with no selected rows stays direct.
        draft.jumps = ModelRc::default();
        let session = session_from_draft(&draft, Some(&saved), vec![], vec![]);
        assert!(session.jump_session_ids.is_empty());
    }

    #[test]
    fn non_ssh_drafts_never_carry_a_jump_chain() {
        let saved = Session::new_empty();
        let mut draft = SessionDraft::default();
        draft.id = saved.id.clone().into();
        draft.kind = "telnet".into();
        draft.jumps = jump_rows(vec!["a".into()], &ModelRc::default());
        let session = session_from_draft(&draft, Some(&saved), vec![], vec![]);
        assert!(session.jump_session_ids.is_empty());
    }

    #[test]
    fn dangling_jump_row_stays_unselected_so_saving_cannot_go_direct() {
        let choices = ModelRc::from(Rc::new(VecModel::from(vec!["".into(), "a".into()])));
        let rows = jump_rows(vec!["missing".into(), "a".into()], &choices);
        assert_eq!(rows.row_data(0).unwrap().id, "missing");
        assert_eq!(rows.row_data(0).unwrap().choice, 0);
        assert_eq!(rows.row_data(1).unwrap().choice, 1);
    }

    #[test]
    fn move_jump_row_reorders_and_ignores_invalid_indices() {
        let mut rows = vec![
            JumpHop {
                id: "a".into(),
                ..JumpHop::default()
            },
            JumpHop {
                id: "b".into(),
                ..JumpHop::default()
            },
            JumpHop {
                id: "c".into(),
                ..JumpHop::default()
            },
        ];
        move_jump_row(&mut rows, 0, 2);
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["b", "c", "a"]
        );
        move_jump_row(&mut rows, 2, 0);
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        move_jump_row(&mut rows, -1, 1);
        move_jump_row(&mut rows, 0, 3);
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn blank_draft_fields_keep_existing_credentials_inline_secret_reveal() {
        let mut saved = Session::new_empty();
        saved.password = Secret::new("fixture-password");
        saved.private_key_inline = Secret::new("fixture-key");
        let mut draft = SessionDraft::default();
        draft.id = saved.id.clone().into();
        draft.kind = "ssh".into();
        draft.auth = "key".into();
        draft.private_key_inline_mode = true;
        draft.allow_secret_reveal = true;
        draft.session_log = "on".into();
        // Blank password/key fields while editing mean "keep the existing
        // one" (issue #10), and the route comes from the jump rows.
        draft.jumps = jump_rows(vec!["a".into()], &ModelRc::default());
        let session = session_from_draft(&draft, Some(&saved), vec![], vec![]);
        assert_eq!(session.password.as_str(), "fixture-password");
        assert_eq!(session.private_key_inline.as_str(), "fixture-key");
        assert_eq!(session.jump_session_ids, ["a"]);
        assert!(session.allow_secret_reveal);
        assert_eq!(session.session_log, SessionLogMode::On);
        // Typed values overwrite the stored ones.
        draft.password = "replacement".into();
        draft.private_key_inline = "replacement-key".into();
        let session = session_from_draft(&draft, Some(&saved), vec![], vec![]);
        assert_eq!(session.password.as_str(), "replacement");
        assert_eq!(session.private_key_inline.as_str(), "replacement-key");
    }
}
