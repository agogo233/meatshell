//! Session list, session dialog and connection callbacks (the large
//! wire_session_callbacks block). Split out of app.rs to match the
//! upstream module boundary (session_callbacks).

use super::*;

/// Drops the second connect of a double-click (#511). Session rows connect on
/// each click release, and users coming from Xshell / FinalShell double-click.
#[derive(Default)]
struct DoubleClickGuard {
    last: Option<(String, std::time::Instant)>,
}

impl DoubleClickGuard {
    /// The default double-click time on Windows and macOS.
    const WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

    /// False when `id` was connected less than [`Self::WINDOW`] ago.
    fn allow(&mut self, id: &str, now: std::time::Instant) -> bool {
        let repeat = matches!(&self.last, Some((last_id, at))
            if last_id == id && now.saturating_duration_since(*at) < Self::WINDOW);
        if !repeat {
            self.last = Some((id.to_string(), now));
        }
        !repeat
    }
}

#[cfg(test)]
mod double_click_tests {
    use super::DoubleClickGuard;
    use std::time::{Duration, Instant};

    #[test]
    fn the_second_click_of_a_double_click_does_not_connect_again() {
        let mut guard = DoubleClickGuard::default();
        let t = Instant::now();
        assert!(guard.allow("rdp", t));
        assert!(!guard.allow("rdp", t + Duration::from_millis(180)));
        // The ignored click does not extend the window: a deliberate third
        // click after it connects again.
        assert!(guard.allow("rdp", t + Duration::from_millis(600)));
    }

    #[test]
    fn other_sessions_and_later_clicks_still_connect() {
        let mut guard = DoubleClickGuard::default();
        let t = Instant::now();
        assert!(guard.allow("a", t));
        assert!(guard.allow("b", t + Duration::from_millis(50)));
        assert!(guard.allow("a", t + Duration::from_millis(100)));
        assert!(guard.allow("a", t + Duration::from_secs(2)));
    }
}

pub(crate) fn wire_session_callbacks(
    window: &AppWindow,
    // Registry id of `window`; connect-time prompts are tagged with it so
    // their dialogs open (and abort on close) in this window (#multi-window).
    window_id: u64,
    store: Rc<RefCell<ConfigStore>>,
    registry: Rc<WindowRegistry<slint::Weak<AppWindow>>>,
    sessions_model: Rc<VecModel<SessionInfo>>,
    tabs_model: Rc<VecModel<TabInfo>>,
    terminals_model: Rc<VecModel<TerminalState>>,
    layout: Rc<RefCell<crate::layout::Layout>>,
    content_size: Rc<std::cell::Cell<(f32, f32)>>,
    panes_model: Rc<VecModel<PaneInfo>>,
    splitters_model: Rc<VecModel<SplitterInfo>>,
    handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    bufs: TermBuffers,
    render_gates: RenderGates,
    runtime: Arc<Runtime>,
    last_term_size: Arc<Mutex<(u32, u32)>>,
    sftp_handles: SftpHandles,
    sftp_last_cwd: SftpLastCwd,
    tab_statuses: TabStatuses,
    local_snap: LocalSnap,
    local_net_hist: NetHist,
    sftp_follow_cd: Arc<std::sync::atomic::AtomicBool>,
    tab_routes: TabRoutes,
    tab_titles: Rc<RefCell<HashMap<String, String>>>,
    bundle_mailbox: BundleMailbox,
    // Detached editor window of this window (#494): connects pass it down to
    // the session event pump through `ConnectCtx`.
    editor: slint::Weak<EditorWindow>,
) {
    // Working set of port forwards (#56) for the session being created/edited.
    // The forward add/delete callbacks mutate it; saving reads it into
    // Session.forwards; opening the dialog (new/edit) resets it.
    let edit_forwards: Rc<RefCell<Vec<PortFwd>>> =
        Rc::new(RefCell::new(vec![blank_forward_draft()]));
    let edit_triggers: Rc<RefCell<Vec<TriggerDraft>>> =
        Rc::new(RefCell::new(vec![blank_trigger_draft()]));
    let edit_trigger_secrets: Rc<RefCell<Vec<Secret>>> =
        Rc::new(RefCell::new(vec![Secret::default()]));
    // on_connect_session moves the panes_model binding into its closure; the
    // rename handler below needs its own handle, so clone up front.
    let panes_model_rename = panes_model.clone();

    // Rebuild the session list as the user edits the Quick Connect search.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_host_search_changed(move |query| {
            if let Some(window) = weak.upgrade() {
                let query = if query.trim().is_empty() {
                    SharedString::new()
                } else {
                    query
                };
                window.set_host_search_query(query.clone());
                sync_sessions_to_model_with_filter(
                    &store.borrow(),
                    &sessions_model,
                    query.as_str(),
                );
            }
        });
    }

    // New session -> open dialog with blank draft.
    let weak = window.as_weak();
    let ef_new = edit_forwards.clone();
    let et_new = edit_triggers.clone();
    let ets_new = edit_trigger_secrets.clone();
    let store_ng = store.clone();
    window.on_new_session_clicked(move || {
        if let Some(w) = weak.upgrade() {
            *ef_new.borrow_mut() = vec![blank_forward_draft()];
            *et_new.borrow_mut() = vec![blank_trigger_draft()];
            *ets_new.borrow_mut() = vec![Secret::default()];
            w.set_session_groups(session_groups_model(&store_ng.borrow()));
            w.set_dialog_forwards(forward_model(&ef_new.borrow()));
            w.set_dialog_triggers(trigger_model(&et_new.borrow()));
            let empty = Session::new_empty();
            let (jump_labels, jump_ids, _jump_idx) =
                jump_candidates(&store_ng.borrow(), &empty.id, "");
            w.set_jump_choices(jump_labels);
            w.set_jump_ids(jump_ids);
            w.set_dialog_jumps(ModelRc::default());
            w.set_dialog_id(empty.id.into());
            w.set_dialog_name("".into());
            w.set_dialog_host("".into());
            w.set_dialog_port("22".into());
            // No default username (#110): leaving it blank makes the connect-time
            // prompt ask for it, Xshell-style.
            w.set_dialog_user("".into());
            w.set_dialog_auth("password".into());
            w.set_dialog_password("".into());
            w.set_dialog_key_path("".into());
            w.set_dialog_key_inline("".into());
            w.set_dialog_key_inline_mode(false);
            w.set_dialog_test_status("".into());
            w.set_dialog_proxy_type("none".into());
            w.set_dialog_proxy_hostport("".into());
            w.set_dialog_group("".into());
            w.set_dialog_kind("ssh".into());
            w.set_dialog_serial_port("".into());
            w.set_session_serial_ports(session_serial_ports_model());
            w.set_dialog_baud("115200".into());
            w.set_dialog_data_bits("8".into());
            w.set_dialog_stop_bits("1".into());
            w.set_dialog_parity("none".into());
            w.set_dialog_flow("none".into());
            w.set_dialog_rdp_domain("".into());
            w.set_dialog_rdp_resolution(RDP_RESOLUTION_DEFAULT.into());
            w.set_dialog_rdp_width("1280".into());
            w.set_dialog_rdp_height("720".into());
            w.set_dialog_rdp_legacy_tls(false);
            w.set_dialog_rdp_admin(false);
            w.set_dialog_rdp_smart_sizing(false);
            w.set_dialog_rdp_cert_policy("ignore".into());
            w.set_dialog_rdp_network("auto".into());
            w.set_dialog_rdp_sound(false);
            w.set_dialog_rdp_clipboard(true);
            w.set_dialog_rdp_gfx(false);
            w.set_dialog_rdp_extra_args("".into());
            w.set_dialog_rdp_show_advanced(false);
            w.set_dialog_encoding("UTF-8".into());
            w.set_dialog_vt100_drawing(false);
            w.set_dialog_term_type(crate::config::DEFAULT_TERM_TYPE.into());
            w.set_dialog_session_log("default".into());
            w.set_dialog_disable_shell_integration(false);
            // A new session starts from a clean slate: the submit now takes
            // these from the draft, so leftover values from the session edited
            // before must not ride along (#438).
            w.set_dialog_sftp_mode("".into());
            w.set_dialog_sftp_command("".into());
            w.set_dialog_allow_secret_reveal(false);
            // MCP exposure is opt-in (#432): new sessions stay hidden from MCP
            // clients until the user ticks it, matching the stored default.
            w.set_dialog_allow_mcp_access(false);
            w.set_dialog_note("".into());
            w.set_dialog_editing(false);
            w.set_dialog_open(true);
        }
    });

    // Import hosts from ~/.ssh/config -> add them as sessions (skipping dups).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let registry = registry.clone();
        window.on_import_ssh_config(move || {
            let hosts = crate::ssh::ssh_config::parse_default();
            let mut added = 0usize;
            if hosts.is_empty() {
                if let Some(w) = weak.upgrade() {
                    w.set_ssh_import_hint(
                        t("未找到 ~/.ssh/config", "no ~/.ssh/config found").into(),
                    );
                }
                return;
            }
            {
                let mut s = store.borrow_mut();
                for h in hosts {
                    // Skip if a session already has this alias, or the same
                    // host + user pair.
                    let dup = s
                        .sessions()
                        .iter()
                        .any(|x| x.name == h.alias || (x.host == h.hostname && x.user == h.user));
                    if dup {
                        continue;
                    }
                    let auth = if h.identity_file.is_empty() {
                        AuthMethod::Password
                    } else {
                        AuthMethod::Key
                    };
                    s.upsert(Session {
                        name: h.alias,
                        host: h.hostname,
                        port: h.port,
                        user: if h.user.is_empty() {
                            "root".into()
                        } else {
                            h.user
                        },
                        auth,
                        private_key_path: h.identity_file,
                        ..Session::new_empty()
                    });
                    added += 1;
                }
                if added > 0 {
                    let _ = s.save();
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if added > 0 {
                registry.broadcast_config_changed();
            }
            if let Some(w) = weak.upgrade() {
                let hint = if added > 0 {
                    format!("{} {}", t("已导入", "imported"), added)
                } else {
                    t("没有新主机可导入", "no new hosts to import").to_string()
                };
                w.set_ssh_import_hint(hint.into());
            }
        });
    }

    // Export the whole config as a passphrase-encrypted bundle (#P3-I). The old
    // built-in-key JSON export let anyone derive the key from the source code.
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_export_sessions(move || {
            let Some(w) = weak.upgrade() else { return };
            let pass = w.get_portable_passphrase().to_string();
            if pass.is_empty() {
                w.set_ssh_import_hint(
                    t(
                        "请先在 设置 → 界面 填写便携包口令",
                        "enter a bundle passphrase in Settings → Interface first",
                    )
                    .into(),
                );
                return;
            }
            let Some(path) = rfd::FileDialog::new()
                .set_file_name("meatshell-connections.mpack")
                .add_filter("MeatShell bundle", &["mpack"])
                .save_file()
            else {
                return;
            };
            let payload = match store.borrow().bundle_payload() {
                Ok(payload) => payload,
                Err(e) => {
                    w.set_ssh_import_hint(
                        format!("{}: {}", t("导出失败", "export failed"), e).into(),
                    );
                    return;
                }
            };
            let count = store.borrow().sessions().len();
            let weak2 = weak.clone();
            std::thread::spawn(move || {
                let res = ConfigStore::seal_bundle(&pass, &payload).and_then(|pack| {
                    std::fs::write(&path, pack)
                        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
                });
                let hint = match res {
                    Ok(()) => format!(
                        "{} {}",
                        t("已导出连接（口令加密）", "exported (encrypted)"),
                        count
                    ),
                    Err(e) => format!("{}: {}", t("导出失败", "export failed"), e),
                };
                let _ = weak2.upgrade_in_event_loop(move |w| w.set_ssh_import_hint(hint.into()));
            });
        });
    }

    // Batch-import connections from pasted text (#150). One per line:
    // `host|port|user|password|name` (trailing fields optional).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let registry = registry.clone();
        window.on_batch_import_confirm(move |text: SharedString| {
            let parsed = parse_batch_import(text.as_str());
            let total = parsed.len();
            let mut added = 0usize;
            {
                let mut s = store.borrow_mut();
                for sess in parsed {
                    // Skip a host/user/port we already have.
                    let dup = s
                        .sessions()
                        .iter()
                        .any(|x| x.host == sess.host && x.user == sess.user && x.port == sess.port);
                    if dup {
                        continue;
                    }
                    s.upsert(sess);
                    added += 1;
                }
                if added > 0 {
                    let _ = s.save();
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if added > 0 {
                registry.broadcast_config_changed();
            }
            if let Some(w) = weak.upgrade() {
                let hint = if total == 0 {
                    t("没有可导入的连接", "nothing to import").to_string()
                } else if added > 0 {
                    format!("{} {}/{}", t("已导入", "imported"), added, total)
                } else {
                    t("没有新连接可导入(已存在)", "no new connections (all exist)").to_string()
                };
                w.set_ssh_import_hint(hint.into());
            }
        });
    }

    // Import sessions from a portable JSON file (issue #46).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let registry = registry.clone();
        window.on_import_sessions(move || {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .add_filter("MobaXterm", &["mxtsessions", "moba"])
                .pick_file()
            {
                let res = store.borrow_mut().import_from(&path);
                if let Some(w) = weak.upgrade() {
                    let hint = match res {
                            Ok((added, skipped)) => {
                            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
                            registry.broadcast_config_changed();
                            format!(
                                "{} {} / {} {}",
                                t("已导入", "imported"),
                                added,
                                t("跳过重复", "skipped"),
                                skipped
                            )
                        }
                        Err(e) => format!("{}: {}", t("导入失败", "import failed"), e),
                    };
                    w.set_ssh_import_hint(hint.into());
                }
            }
        });
    }

    // ── Passphrase-encrypted portable bundle (#P3-I) ──────────────────────
    // File export/import + WebDAV round-trip of the whole config, sealed with a
    // key derived from the passphrase the user typed in the WebDAV settings page.
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_export_portable_bundle(move || {
            let Some(w) = weak.upgrade() else { return };
            let pass = w.get_portable_passphrase().to_string();
            if pass.is_empty() {
                w.set_portable_status(
                    t("请先填写便携包口令", "enter a bundle passphrase first").into(),
                );
                return;
            }
            let Some(path) = rfd::FileDialog::new()
                .set_file_name("meatshell.mpack")
                .add_filter("MeatShell bundle", &["mpack"])
                .save_file()
            else {
                return;
            };
            let payload = match store.borrow().bundle_payload() {
                Ok(payload) => payload,
                Err(e) => {
                    w.set_portable_status(
                        format!("{}: {}", t("导出失败", "export failed"), e).into(),
                    );
                    return;
                }
            };
            let count = store.borrow().sessions().len();
            let weak2 = weak.clone();
            w.set_portable_status(t("导出中...", "exporting...").into());
            std::thread::spawn(move || {
                let res = ConfigStore::seal_bundle(&pass, &payload).and_then(|pack| {
                    std::fs::write(&path, pack)
                        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
                });
                let msg = match res {
                    Ok(()) => format!("{} {}", t("已导出便携包", "bundle exported"), count),
                    Err(e) => format!("{}: {}", t("导出失败", "export failed"), e),
                };
                let _ = weak2.upgrade_in_event_loop(move |w| w.set_portable_status(msg.into()));
            });
        });
    }
    {
        let weak = window.as_weak();
        let mailbox = bundle_mailbox.clone();
        window.on_import_portable_bundle(move || {
            let Some(w) = weak.upgrade() else { return };
            let pass = w.get_portable_passphrase().to_string();
            if pass.is_empty() {
                w.set_portable_status(
                    t("请先填写便携包口令", "enter a bundle passphrase first").into(),
                );
                return;
            }
            let Some(path) = rfd::FileDialog::new()
                .add_filter("MeatShell bundle", &["mpack"])
                .pick_file()
            else {
                return;
            };
            // The Argon2 KDF is deliberately slow; run it (and the file read)
            // off the UI thread and let the bundle timer apply the result.
            let mailbox = mailbox.clone();
            w.set_portable_status(t("解密中...", "decrypting...").into());
            std::thread::spawn(move || {
                let result = std::fs::read_to_string(&path)
                    .map_err(|e| format!("{e}"))
                    .and_then(|pack| {
                        ConfigStore::decrypt_bundle(&pass, &pack).map_err(|e| format!("{e:#}"))
                    });
                lock_or_recover(&mailbox).push(BundleDelivery {
                    result,
                    download: false,
                    webdav: false,
                    fallback: None,
                });
            });
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_upload_portable_bundle(move || {
            let Some(w) = weak.upgrade() else { return };
            let pass = w.get_portable_passphrase().to_string();
            let url = w.get_webdav_url().to_string();
            let username = w.get_webdav_username().to_string();
            let password = w.get_webdav_password().to_string();
            let remote_path = w.get_webdav_remote_path().to_string();
            let accept_invalid_certs = w.get_webdav_accept_invalid_certs();
            if pass.is_empty() {
                w.set_portable_status(
                    t("请先填写便携包口令", "enter a bundle passphrase first").into(),
                );
                return;
            }
            let bundle_path = bundle_remote_path(&remote_path);
            let payload = match store.borrow().bundle_payload() {
                Ok(payload) => payload,
                Err(e) => {
                    w.set_portable_status(
                        format!("{}: {}", t("上传失败", "upload failed"), e).into(),
                    );
                    return;
                }
            };
            let weak2 = weak.clone();
            w.set_portable_status(t("上传中...", "uploading...").into());
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
                let msg = match res {
                    Ok(()) => t("便携包已上传", "bundle uploaded").to_string(),
                    Err(e) => format!("{}: {}", t("上传失败", "upload failed"), e),
                };
                let _ = weak2.upgrade_in_event_loop(move |w| w.set_portable_status(msg.into()));
            });
        });
    }
    {
        let weak = window.as_weak();
        let mailbox = bundle_mailbox.clone();
        window.on_download_portable_bundle(move || {
            let Some(w) = weak.upgrade() else { return };
            let pass = w.get_portable_passphrase().to_string();
            let url = w.get_webdav_url().to_string();
            let username = w.get_webdav_username().to_string();
            let password = w.get_webdav_password().to_string();
            let remote_path = w.get_webdav_remote_path().to_string();
            let accept_invalid_certs = w.get_webdav_accept_invalid_certs();
            if pass.is_empty() {
                w.set_portable_status(
                    t("请先填写便携包口令", "enter a bundle passphrase first").into(),
                );
                return;
            }
            let bundle_path = bundle_remote_path(&remote_path);
            // Fetch + KDF off the UI thread; the bundle timer applies the
            // decrypted config where the store lives.
            let mailbox = mailbox.clone();
            w.set_portable_status(t("下载中...", "downloading...").into());
            std::thread::spawn(move || {
                let result =
                    webdav_get_json(&url, &bundle_path, &username, &password, accept_invalid_certs)
                        .map_err(|e| format!("{e:#}"))
                        .and_then(|body| {
                            extract_bundle_data(&body).map_err(|e| format!("{e}"))
                        })
                        .and_then(|pack| {
                            ConfigStore::decrypt_bundle(&pass, &pack).map_err(|e| format!("{e:#}"))
                        });
                lock_or_recover(&mailbox).push(BundleDelivery {
                    result,
                    download: true,
                    webdav: false,
                    fallback: None,
                });
            });
        });
    }

    // Edit -> open dialog prefilled.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let ef_edit = edit_forwards.clone();
        let et_edit = edit_triggers.clone();
        let ets_edit = edit_trigger_secrets.clone();
        window.on_edit_session(move |id: SharedString| {
            let id = id.to_string();
            let store = store.borrow();
            let Some(session) = store.get(&id) else {
                return;
            };
            *ef_edit.borrow_mut() = forward_drafts(&session.forwards);
            if ef_edit.borrow().is_empty() {
                ef_edit.borrow_mut().push(blank_forward_draft());
            }
            *et_edit.borrow_mut() = trigger_drafts(&session.triggers);
            *ets_edit.borrow_mut() = session.triggers.iter().map(|t| t.response.clone()).collect();
            if et_edit.borrow().is_empty() {
                et_edit.borrow_mut().push(blank_trigger_draft());
                ets_edit.borrow_mut().push(Secret::default());
            }
            if let Some(w) = weak.upgrade() {
                w.set_session_groups(session_groups_model(&store));
                w.set_dialog_forwards(forward_model(&ef_edit.borrow()));
                w.set_dialog_triggers(trigger_model(&et_edit.borrow()));
                w.set_dialog_id(session.id.clone().into());
                w.set_dialog_name(session.name.clone().into());
                w.set_dialog_host(session.host.clone().into());
                w.set_dialog_port(session.port.to_string().into());
                w.set_dialog_user(session.user.clone().into());
                w.set_dialog_auth(session.auth.as_str().into());
                // The reveal opt-in goes first: `set_dialog_key_inline_mode`
                // below makes the dialog ask for the stored credentials, and
                // that request must already see this session's own value
                // instead of whatever the previous one left behind.
                w.set_dialog_allow_secret_reveal(session.allow_secret_reveal);
                // Never echo the stored password back into the UI (issue #10) —
                // leave it blank; a blank field on save keeps the existing one.
                w.set_dialog_password("".into());
                w.set_dialog_key_path(session.private_key_path.clone().into());
                w.set_dialog_key_inline("".into());
                w.set_dialog_key_inline_mode(!session.private_key_inline.is_empty());
                w.set_dialog_test_status("".into());
                // Backfill the MCP-exposure opt-in from the saved session: the
                // dialog starts unchecked (fail-closed), so without this the
                // user's existing opt-in would silently flip off on save.
                w.set_dialog_allow_mcp_access(session.mcp_access);
                let (proxy_type, proxy_hostport) = split_proxy(session.proxy.as_str());
                w.set_dialog_proxy_type(proxy_type.into());
                w.set_dialog_proxy_hostport(proxy_hostport.into());
                let (jump_labels, jump_ids, _) =
                    jump_candidates(&store, &session.id, &session.jump_session_id);
                // Backfill the jump editor from the saved route. A resolvable
                // chain shows one row per hop (immediate first); a broken
                // route shows a test error and, when only the legacy single
                // hop is set, forces an explicit repair instead of silently
                // flattening it (#211).
                let route = store.resolve_jump_chain(session);
                let ids = match route {
                    Ok(hops) => hops.into_iter().rev().map(|hop| hop.id).collect(),
                    Err(err) => {
                        w.set_dialog_test_status(err.to_string().into());
                        if session.jump_session_ids.is_empty() {
                            vec![String::new(), session.jump_session_id.clone()]
                        } else {
                            session.jump_session_ids.clone()
                        }
                    }
                };
                w.set_dialog_jumps(jump_rows(ids, &jump_ids));
                w.set_jump_choices(jump_labels);
                w.set_jump_ids(jump_ids);
                w.set_dialog_group(session.group.clone().into());
                w.set_dialog_kind(session.kind.as_str().into());
                w.set_dialog_serial_port(session.serial_port.clone().into());
                w.set_session_serial_ports(session_serial_ports_model());
                w.set_dialog_baud(session.baud_rate.to_string().into());
                w.set_dialog_data_bits(session.data_bits.to_string().into());
                w.set_dialog_stop_bits(session.stop_bits.to_string().into());
                w.set_dialog_parity(session.parity.clone().into());
                w.set_dialog_flow(session.flow_control.clone().into());
                w.set_dialog_rdp_domain(session.rdp_domain.clone().into());
                w.set_dialog_rdp_resolution(
                    rdp_resolution_choice(
                        session.rdp_fullscreen,
                        session.rdp_width,
                        session.rdp_height,
                    )
                    .into(),
                );
                w.set_dialog_rdp_width(session.rdp_width.to_string().into());
                w.set_dialog_rdp_height(session.rdp_height.to_string().into());
                w.set_dialog_rdp_legacy_tls(session.rdp_legacy_tls);
                w.set_dialog_rdp_admin(session.rdp_admin);
                w.set_dialog_rdp_smart_sizing(session.rdp_smart_sizing);
                w.set_dialog_rdp_cert_policy(session.rdp_cert_policy.clone().into());
                w.set_dialog_rdp_network(session.rdp_network.clone().into());
                w.set_dialog_rdp_sound(session.rdp_sound);
                w.set_dialog_rdp_clipboard(session.rdp_clipboard);
                w.set_dialog_rdp_gfx(session.rdp_gfx);
                w.set_dialog_rdp_extra_args(session.rdp_extra_args.clone().into());
                w.set_dialog_rdp_show_advanced(!session.rdp_extra_args.is_empty());
                w.set_dialog_encoding(session.encoding.clone().into());
                w.set_dialog_vt100_drawing(session.vt100_drawing);
                w.set_dialog_term_type(session.effective_term_type().into());
                w.set_dialog_session_log(session.session_log.as_str().into());
                w.set_dialog_sftp_mode(session.sftp_mode.clone().into());
                w.set_dialog_sftp_command(session.sftp_command.clone().into());
                w.set_dialog_disable_shell_integration(session.disable_shell_integration);
                w.set_dialog_note(session.note.clone().into());
                w.set_dialog_editing(true);
                w.set_dialog_open(true);
            }
        });
    }

    // Reveal a saved credential from the open editor (#reveal-secret). The
    // dialog only asks after the user opted in with `allow_secret_reveal`, so
    // an unauthorized caller gets nothing back.
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_reveal_session_secret(move |private_key| {
            let Some(w) = weak.upgrade() else {
                return SharedString::new();
            };
            if !w.get_dialog_open() || !w.get_dialog_allow_secret_reveal() {
                return SharedString::new();
            }
            let store = store.borrow();
            let Some(session) = store.get(w.get_dialog_id().as_str()) else {
                return SharedString::new();
            };
            if private_key {
                session.private_key_inline.as_str().into()
            } else {
                session.password.as_str().into()
            }
        });
    }

    // Remove session.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let registry = registry.clone();
        window.on_remove_session(move |id: SharedString| {
            {
                let mut s = store.borrow_mut();
                s.remove(&id.to_string());
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            registry.broadcast_config_changed();
            if let Some(w) = weak.upgrade() {
                // Touch a property so the list re-renders reliably.
                let _ = w.get_sessions();
            }
        });
    }

    // Duplicate a session: clone it with a fresh id and a " (copy)" name (#41).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let registry = registry.clone();
        window.on_duplicate_session(move |id: SharedString| {
            let mut duplicated = false;
            {
                let mut s = store.borrow_mut();
                if let Some(orig) = s.get(&id.to_string()).cloned() {
                    let mut copy = orig;
                    copy.id = uuid::Uuid::new_v4().to_string();
                    copy.name = format!("{} {}", copy.name, t("副本", "(copy)"));
                    copy.last_used = None;
                    s.upsert(copy);
                    if let Err(err) = s.save() {
                        tracing::warn!("failed to save config: {err:#}");
                    }
                    duplicated = true;
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if duplicated {
                registry.broadcast_config_changed();
            }
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Move a session to another group (#41).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let registry = registry.clone();
        window.on_move_session(move |id: SharedString, group: SharedString| {
            let mut moved = false;
            {
                let mut s = store.borrow_mut();
                if let Some(orig) = s.get(&id.to_string()).cloned() {
                    let mut target = orig;
                    // "default" is the display label for ungrouped → store empty.
                    target.group = if group.as_str().eq_ignore_ascii_case("default") {
                        String::new()
                    } else if is_reserved_session_group(group.as_str().trim()) {
                        // `system` belongs exclusively to built-in local shells.
                        return;
                    } else {
                        group.to_string()
                    };
                    s.upsert(target);
                    if let Err(err) = s.save() {
                        tracing::warn!("failed to save config: {err:#}");
                    }
                    moved = true;
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if moved {
                registry.broadcast_config_changed();
            }
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Drag-to-reorder a host card among its same-group siblings. The stored
    // Vec order is the display order (no alphabetical sort), so a swap plus
    // re-sync is all it takes. Reordering while the list is filtered would map
    // visible hops onto the wrong stored neighbours, so bail out then — the
    // Slint side already disables the gesture while searching.
    //
    // Per-hop updates mutate the model IN PLACE (set_row_data): a full set_vec
    // rebuild would recreate the rows and drop the dragging row's pointer grab,
    // ending the drag after one hop. Saving + broadcasting are deferred to
    // reorder-session-end (pointer release).
    let sessions_dirty = Rc::new(std::cell::Cell::new(false));
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let registry = registry.clone();
        let sessions_dirty = sessions_dirty.clone();
        window.on_reorder_session(move |id: SharedString, dir: i32| {
            if weak
                .upgrade()
                .map(|window| !window.get_host_search_query().trim().is_empty())
                .unwrap_or(false)
            {
                return false;
            }
            let moved = {
                let mut s = store.borrow_mut();
                s.reorder_session(id.as_str(), dir as isize)
            };
            if moved {
                sessions_dirty.set(true);
                let query = weak
                    .upgrade()
                    .map(|w| w.get_host_search_query().to_string())
                    .unwrap_or_default();
                let in_place = refresh_session_rows_in_place(&store.borrow(), &sessions_model, &query);
                if !in_place {
                    // The hop changed the row count (e.g. a cross-group hop
                    // emptied the ungrouped section): the set_vec rebuild
                    // dropped the dragging row's pointer grab, so the
                    // release's reorder-end may never arrive — finalise now.
                    sessions_dirty.set(false);
                    if let Err(err) = store.borrow_mut().save() {
                        tracing::warn!("failed to save config: {err:#}");
                    }
                    sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
                    registry.broadcast_config_changed();
                    if let Some(w) = weak.upgrade() {
                        let _ = w.get_sessions();
                    }
                }
            }
            moved
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let registry = registry.clone();
        let sessions_dirty = sessions_dirty.clone();
        window.on_reorder_session_end(move || {
            if !sessions_dirty.replace(false) {
                return;
            }
            if let Err(err) = store.borrow_mut().save() {
                tracing::warn!("failed to save config: {err:#}");
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            registry.broadcast_config_changed();
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Collapse / expand a group in the welcome list (#41). Toggling flips the
    // `collapsed` flag on every row of that group in place — no full re-sync —
    // so the open/closed state stays put until the list is actually rebuilt.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_toggle_group(move |group: SharedString| {
            if weak
                .upgrade()
                .map(|window| !window.get_host_search_query().trim().is_empty())
                .unwrap_or(false)
            {
                return;
            }
            use slint::Model as _;
            let target = group.to_string();
            let n = sessions_model.row_count();
            // New state = the opposite of the group's first row.
            let mut new_state = false;
            for i in 0..n {
                if let Some(row) = sessions_model.row_data(i) {
                    if row.group.as_str() == target {
                        new_state = !row.collapsed;
                        break;
                    }
                }
            }
            for i in 0..n {
                if let Some(mut row) = sessions_model.row_data(i) {
                    if row.group.as_str() == target {
                        row.collapsed = new_state;
                        sessions_model.set_row_data(i, row);
                    }
                }
            }
            {
                let mut store = store.borrow_mut();
                store.set_session_group_collapsed(&target, new_state);
                if let Err(err) = store.save() {
                    tracing::warn!("failed to save Quick Connect folder state: {err:#}");
                }
            }
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Group create / rename (#41).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let registry = registry.clone();
        window.on_submit_group(move |orig: SharedString, name: SharedString| {
            let trimmed = name.trim();
            let error = {
                let s = store.borrow();
                if trimmed.is_empty() {
                    Some(t("请输入分组名称", "Enter a group name"))
                } else if is_reserved_session_group(trimmed) {
                    Some(t("该名称为系统保留分组", "This group name is reserved"))
                } else if (orig.is_empty() || !trimmed.eq_ignore_ascii_case(orig.as_str()))
                    && s.session_group_exists(trimmed)
                {
                    Some(t("分组已存在", "Group already exists"))
                } else {
                    None
                }
            };
            if let Some(message) = error {
                return SharedString::from(message);
            }
            {
                let mut s = store.borrow_mut();
                if orig.is_empty() {
                    s.add_group(trimmed.to_string());
                } else {
                    s.rename_group(orig.as_str(), trimmed.to_string());
                }
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            registry.broadcast_config_changed();
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
            SharedString::new()
        });
    }
    // Group delete (#41) — UI only offers this on empty groups.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let registry = registry.clone();
        window.on_delete_group(move |name: SharedString| {
            {
                let mut s = store.borrow_mut();
                s.remove_group(&name.to_string());
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            registry.broadcast_config_changed();
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Jump-chain editor rows in the session dialog (#211): add / update /
    // delete / move. Rows hold the chosen id + dropdown choice; saving maps
    // them back to `jump_session_ids` (see submit / `session_from_draft`).
    {
        let weak = window.as_weak();
        window.on_add_jump(move || {
            if let Some(w) = weak.upgrade() {
                update_jump_rows(&w, |rows| {
                    if rows.len() < MAX_JUMP_EDITOR_ROWS {
                        rows.push(JumpHop {
                            placeholder: true,
                            ..JumpHop::default()
                        });
                    }
                });
            }
        });
        let weak = window.as_weak();
        window.on_update_jump(move |index, choice| {
            if let Some(w) = weak.upgrade() {
                let Some(id) = w.get_jump_ids().row_data(choice.max(0) as usize) else {
                    return;
                };
                // Mutate the row IN PLACE (set_row_data), not through
                // `update_jump_rows`: that rebuilds the model, which recreates
                // the row and destroys the one whose dropdown popup is still
                // closing (#509). The helper also cleared the test status, so
                // do that here.
                let Ok(index) = usize::try_from(index) else {
                    return;
                };
                let rows = w.get_dialog_jumps();
                if let Some(row) = rows.row_data(index) {
                    let placeholder = row.placeholder && id.trim().is_empty();
                    rows.set_row_data(
                        index,
                        JumpHop {
                            id,
                            choice,
                            placeholder,
                        },
                    );
                    w.set_dialog_test_status("".into());
                }
            }
        });
        let weak = window.as_weak();
        window.on_delete_jump(move |index| {
            if let Some(w) = weak.upgrade() {
                update_jump_rows(&w, |rows| {
                    if index >= 0 && (index as usize) < rows.len() {
                        rows.remove(index as usize);
                    }
                });
            }
        });
        let weak = window.as_weak();
        window.on_move_jump(move |from, to| {
            if let Some(w) = weak.upgrade() {
                update_jump_rows(&w, |rows| move_jump_row(rows, from, to));
            }
        });
    }

    // Dialog submit -> persist + (optionally) connect.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let edit_forwards = edit_forwards.clone();
        let edit_triggers = edit_triggers.clone();
        let edit_trigger_secrets = edit_trigger_secrets.clone();
        let registry = registry.clone();
        window.on_session_dialog_submit(move |draft: SessionDraft| {
            let id = draft.id.to_string();
            let forwards = match validated_port_forwards(&edit_forwards.borrow()) {
                Ok(forwards) => forwards,
                Err(message) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                    return;
                }
            };
            let triggers = match validated_triggers(&edit_triggers.borrow(), &edit_trigger_secrets.borrow()) {
                Ok(triggers) => triggers,
                Err(message) => {
                    if let Some(w) = weak.upgrade() { w.set_dialog_test_status(message.into()); }
                    return;
                }
            };
            // The edit dialog never echoes the real password (issue #10): a blank
            // field while editing means "keep the existing password" rather than
            // "clear it".  Only overwrite when the user actually typed something.
            let password = if draft.password.is_empty() {
                store
                    .borrow()
                    .get(&id)
                    .map(|s| s.password.clone())
                    .unwrap_or_default()
            } else {
                Secret::new(draft.password.to_string())
            };
            let private_key_inline = if draft.private_key_inline_mode {
                if draft.private_key_inline.is_empty() {
                    store
                        .borrow()
                        .get(&id)
                        .map(|s| s.private_key_inline.clone())
                        .unwrap_or_default()
                } else {
                    Secret::new(draft.private_key_inline.to_string())
                }
            } else {
                Secret::default()
            };
            let private_key_path = if draft.private_key_inline_mode {
                String::new()
            } else {
                draft.private_key_path.to_string().replace('\\', "/")
            };
            let kind = crate::config::SessionKind::from_str(&draft.kind.to_string());
            // Auto-name: serial → port label; otherwise user@host, or just the
            // host when no username was given (#110).
            let auto_name = match kind {
                crate::config::SessionKind::Serial => {
                    format!("{} @{}", draft.serial_port, draft.baud_rate)
                }
                _ if draft.user.trim().is_empty() => draft.host.to_string(),
                _ => format!("{}@{}", draft.user, draft.host),
            };
            // Telnet defaults to port 23, RDP to 3389, SSH to 22; serial ignores
            // the port entirely.
            let default_port = match kind {
                crate::config::SessionKind::Telnet => 23,
                crate::config::SessionKind::Rdp => 3389,
                _ => 22,
            };
            let (rdp_fullscreen, rdp_width, rdp_height) = rdp_display_settings(
                &draft.rdp_resolution.to_string(),
                draft.rdp_width,
                draft.rdp_height,
            );
            // Immediate jump first; only SSH sessions carry a jump chain
            // (#211). Fresh unselected editor rows stay out of the route.
            let jump_session_ids = if kind == crate::config::SessionKind::Ssh {
                crate::config::draft_jump_ids(
                    draft.jumps.iter().map(|hop| (hop.id.to_string(), hop.placeholder)),
                )
            } else {
                Vec::new()
            };
            let new_session = Session {
                id,
                name: if draft.name.is_empty() {
                    auto_name
                } else {
                    draft.name.to_string()
                },
                host: draft.host.to_string(),
                port: if draft.port <= 0 {
                    default_port
                } else {
                    draft.port as u16
                },
                user: draft.user.to_string(),
                auth: AuthMethod::from_str(&draft.auth.to_string()),
                password,
                // Store the key path with forward slashes uniformly.
                private_key_path,
                private_key_inline,
                proxy: Secret::new(draft.proxy.to_string()),
                last_used: None,
                group: draft.group.to_string(),
                kind,
                local_distribution: String::new(),
                local_working_dir: String::new(),
                local_command: String::new(),
                local_program: String::new(),
                local_args: Vec::new(),
                serial_port: draft.serial_port.to_string(),
                baud_rate: if draft.baud_rate <= 0 {
                    115_200
                } else {
                    draft.baud_rate as u32
                },
                data_bits: draft.data_bits as u8,
                stop_bits: draft.stop_bits as u8,
                parity: draft.parity.to_string(),
                flow_control: draft.flow_control.to_string(),
                encoding: draft.encoding.to_string(),
                vt100_drawing: draft.vt100_drawing,
                term_type: crate::config::normalize_term_type(&draft.term_type).to_string(),
                session_log: SessionLogMode::from_str(draft.session_log.as_str()),
                forwards,
                triggers,
                disable_shell_integration: draft.disable_shell_integration,
                // Root / custom SFTP server command survive the save (#438).
                sftp_mode: if kind == SessionKind::Ssh {
                    normalized_sftp_mode(draft.sftp_mode.as_str(), draft.sftp_command.as_str())
                } else {
                    String::new()
                },
                sftp_command: if kind == SessionKind::Ssh {
                    draft.sftp_command.trim().to_string()
                } else {
                    String::new()
                },
                note: draft.note.to_string(),
                jump_session_id: String::new(),
                allow_secret_reveal: draft.allow_secret_reveal,
                jump_session_ids,
                mcp_access: draft.allow_mcp_access,
                rdp_domain: draft.rdp_domain.to_string(),
                rdp_fullscreen,
                rdp_width,
                rdp_height,
                rdp_legacy_tls: draft.rdp_legacy_tls,
                rdp_admin: draft.rdp_admin,
                rdp_smart_sizing: draft.rdp_smart_sizing,
                rdp_cert_policy: draft.rdp_cert_policy.to_string(),
                rdp_network: draft.rdp_network.to_string(),
                rdp_sound: draft.rdp_sound,
                rdp_clipboard: draft.rdp_clipboard,
                rdp_gfx: draft.rdp_gfx,
                rdp_extra_args: draft.rdp_extra_args.to_string(),
            };
            if new_session.kind == crate::config::SessionKind::Rdp {
                if let Err(message) = crate::rdp::validate_options(&new_session) {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                    return;
                }
            }
            {
                // An invalid jump chain (dangling/empty hop) must fail the save
                // with on-screen feedback instead of persisting a route that
                // cannot connect (#211). Non-SSH drafts resolve to no hops.
                if let Err(err) = store.borrow().resolve_jump_chain(&new_session) {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_test_status(err.to_string().into());
                    }
                    return;
                }
                let mut s = store.borrow_mut();
                s.upsert(new_session);
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            registry.broadcast_config_changed();
            if let Some(w) = weak.upgrade() {
                w.set_dialog_open(false);
            }
        });
    }

    // Active dialog connection test: a spawned runtime task (SSH handshake /
    // TCP reachability / serial open) with its abort handle, so
    // `stop-test` / `closed` / `changed` can cancel an in-flight test instead
    // of leaving a fire-and-forget task whose late host-key / credential / MFA
    // prompts would pop up against a closed or edited dialog (#276).
    // Ownership for the in-flight connection test: a finished, replaced or
    // closed test must never reopen auth UI or update a later draft (#276).
    // Uses the upstream `EditorTest` ticket mechanism (snapshot + active flag
    // + abort handle) instead of the older fire-and-forget join-handle slot.
    let editor_test: Rc<RefCell<EditorTest>> = Rc::new(RefCell::new(EditorTest::default()));

    // Preview the FreeRDP command line from the unsaved draft; the saved
    // password is never rendered.
    {
        let weak = window.as_weak();
        window.on_session_dialog_preview_rdp(move |draft: SessionDraft| {
            let session = session_from_draft(&draft, None, Vec::new(), Vec::new());
            if session.kind != crate::config::SessionKind::Rdp {
                return String::new().into();
            }
            crate::rdp::preview_args(&session).unwrap_or_else(|message| message).into()
        });
    }

    // Test connection from the session dialog. SSH tests use the same handshake,
    // host-key verification, proxy/jump routing, and authentication as a real
    // terminal connection (#276). Telnet and serial retain reachability tests.
    {
        let weak = window.as_weak();
        let runtime = runtime.clone();
        let store = store.clone();
        let edit_forwards = edit_forwards.clone();
        let edit_triggers = edit_triggers.clone();
        let edit_trigger_secrets = edit_trigger_secrets.clone();
        let editor_test = editor_test.clone();
        // Replace any in-flight test with this one. The snapshot is a static
        // counter below: the session dialog is modal, so the draft cannot be
        // edited while a test runs — the ticket still guards against late
        // host-key / credential / MFA prompts from a superseded test (#276).
        // A plain fn, not a closure: `impl Trait` parameters are only allowed
        // on functions/methods, not closure parameters.
        fn begin_test(
            runtime: &tokio::runtime::Runtime,
            editor_test: &Rc<RefCell<EditorTest>>,
            snapshot: &std::cell::Cell<u64>,
            task: impl std::future::Future<Output = ()> + Send + 'static,
        ) {
            snapshot.set(snapshot.get() + 1);
            editor_test.borrow_mut().begin(snapshot.get());
            let handle = runtime.spawn(task);
            editor_test.borrow_mut().attach(handle.abort_handle());
        }
        let test_snapshot = std::cell::Cell::new(0u64);
        window.on_session_dialog_test(move |draft: SessionDraft| {
            let kind = draft.kind.to_string();
            if kind == "serial" {
                let port_name = draft.serial_port.to_string();
                let baud = if draft.baud_rate <= 0 {
                    115_200
                } else {
                    draft.baud_rate as u32
                };
                let weak_done = weak.clone();
                begin_test(&runtime, &editor_test, &test_snapshot, async move {
                    let message = match tokio::task::spawn_blocking(move || {
                        serialport::new(&port_name, baud)
                            .timeout(std::time::Duration::from_millis(800))
                            .open()
                    })
                    .await
                    {
                        Ok(Ok(_)) => t("连接正常", "Connection OK").to_string(),
                        Ok(Err(e)) => format!("{}: {e}", t("连接失败", "Connection failed")),
                        Err(e) => format!("{}: {e}", t("连接失败", "Connection failed")),
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_done.upgrade() {
                            w.set_dialog_test_status(message.into());
                        }
                    });
                });
                return;
            }

            let existing = store.borrow().get(draft.id.as_str()).cloned();
            let forwards = match validated_port_forwards(&edit_forwards.borrow()) {
                Ok(forwards) => forwards,
                Err(message) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                    return;
                }
            };
            let triggers = match validated_triggers(&edit_triggers.borrow(), &edit_trigger_secrets.borrow()) {
                Ok(triggers) => triggers,
                Err(message) => {
                    if let Some(w) = weak.upgrade() { w.set_dialog_test_status(message.into()); }
                    return;
                }
            };
            let session = session_from_draft(&draft, existing.as_ref(), forwards, triggers);
            let weak_done = weak.clone();

            if kind == "ssh" {
                let jump = match resolve_jump(&store, &session) {
                    Ok(jump) => jump,
                    Err(error) => {
                        if let Some(w) = weak.upgrade() {
                            w.set_dialog_test_status(error.to_string().into());
                        }
                        return;
                    }
                };
                let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
                begin_test(&runtime, &editor_test, &test_snapshot, async move {
                    let mut test = Box::pin(test_session_auth(session, jump, events_tx));
                    let result = loop {
                        tokio::select! {
                            result = &mut test => break result,
                            event = events_rx.recv() => {
                                let Some(event) = event else { continue };
                                if matches!(
                                    event,
                                    SessionEvent::HostKeyPrompt { .. }
                                        | SessionEvent::CredentialPrompt { .. }
                                        | SessionEvent::MfaPrompt { .. }
                                ) {
                                    let weak_prompt = weak_done.clone();
                                    let _ = slint::invoke_from_event_loop(move || {
                                        let Some(w) = weak_prompt.upgrade() else { return };
                                        match event {
                                            SessionEvent::HostKeyPrompt {
                                                host,
                                                port,
                                                key_type,
                                                fingerprint,
                                                changed,
                                                responder,
                                            } => enqueue_hostkey_prompt(
                                                &w,
                                                window_id,
                                                host,
                                                port,
                                                key_type,
                                                fingerprint,
                                                changed,
                                                responder,
                                            ),
                                            SessionEvent::CredentialPrompt {
                                                session_id,
                                                host,
                                                user,
                                                need_user,
                                                need_password,
                                                responder,
                                            } => enqueue_cred_prompt(
                                                &w,
                                                window_id,
                                                session_id,
                                                host,
                                                user,
                                                need_user,
                                                need_password,
                                                responder,
                                            ),
                                            SessionEvent::MfaPrompt {
                                                session_id,
                                                host,
                                                prompt,
                                                echo,
                                                responder,
                                            } => enqueue_mfa_prompt(
                                                &w,
                                                window_id,
                                                session_id,
                                                host,
                                                prompt,
                                                echo,
                                                responder,
                                            ),
                                            _ => {}
                                        }
                                    });
                                }
                            }
                        }
                    };
                    let message = match result {
                        Ok(()) => t("连接正常", "Connection OK").to_string(),
                        Err(e) => format!("{}: {e:#}", t("连接失败", "Connection failed")),
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_done.upgrade() {
                            w.set_dialog_test_status(message.into());
                        }
                    });
                });
                return;
            }

            let host = session.host;
            let port = session.port;
            begin_test(&runtime, &editor_test, &test_snapshot, async move {
                let target = format!("{host}:{port}");
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    tokio::net::TcpStream::connect((host.as_str(), port)),
                )
                .await;
                let message = match result {
                    Ok(Ok(_)) => t("连接正常", "Connection OK").to_string(),
                    Ok(Err(e)) => format!("{}: {e}", t("连接失败", "Connection failed")),
                    Err(_) => format!("{}: {target}", t("连接超时", "Connection timed out")),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_done.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                });
            });
        });
    }

    // Dialog test lifecycle (#276): stop an in-flight connection test when the
    // user presses "Stop test", closes the dialog, or edits the draft. Without
    // these the spawned test task kept running fire-and-forget and its late
    // host-key / credential / MFA prompts popped up against a closed dialog.
    {
        let weak = window.as_weak();
        let editor_test = editor_test.clone();
        window.on_session_dialog_stop_test(move || {
            let in_flight = editor_test.borrow_mut().stop().is_some();
            if in_flight {
                if let Some(w) = weak.upgrade() {
                    w.set_dialog_test_status(t("测试已取消", "Test cancelled").into());
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        let editor_test = editor_test.clone();
        window.on_session_dialog_closed(move || {
            editor_test.borrow_mut().stop();
            if let Some(w) = weak.upgrade() {
                w.set_dialog_test_status("".into());
            }
        });
    }
    {
        // Editing the draft invalidates the running test (its values no longer
        // match what is being tested). `stop` aborts the task and finishes the
        // ticket so a late result can never update a newer test (#276).
        let editor_test = editor_test.clone();
        window.on_session_dialog_changed(move || {
            editor_test.borrow_mut().stop();
        });
    }

    // Cancel dialog.
    {
        let weak = window.as_weak();
        window.on_session_dialog_cancel(move || {
            if let Some(w) = weak.upgrade() {
                w.set_dialog_open(false);
            }
        });
    }

    // Private-key file picker: pick the private key and store its path with
    // forward-slash separators (uniform across Windows/Linux; russh accepts them).
    {
        let weak = window.as_weak();
        window.on_session_dialog_pick_key(move || {
            let mut dialog =
                rfd::FileDialog::new().set_title(t("选择私钥文件", "Choose private key file"));
            // OpenSSH's standard key names (id_ed25519, id_rsa, …) usually
            // have no extension. Extension filters hide or disable those files
            // in native pickers, so show every file on every platform (#393).
            // Start in ~/.ssh if it exists.
            if let Some(home) = directories::UserDirs::new().map(|u| u.home_dir().join(".ssh")) {
                if home.is_dir() {
                    dialog = dialog.set_directory(home);
                }
            }
            if let Some(file) = dialog.pick_file() {
                let path = file.to_string_lossy().replace('\\', "/");
                if let Some(w) = weak.upgrade() {
                    w.set_dialog_key_path(path.into());
                }
            }
        });
    }

    // In-app SSH keypair generator. The (potentially slow) RSA/Ed25519
    // generation runs off the UI thread; the pair is written to ~/.ssh with
    // 0600 and the private key is pasted into the inline field so the session
    // can use it immediately.
    {
        let weak = window.as_weak();
        window.on_session_dialog_generate_key(move || {
            let weak = weak.clone();
            // The callback must be 'static; reach for the window through the
            // Weak handle instead of capturing the &AppWindow reference.
            let Some(w) = weak.upgrade() else { return };
            let algo = match w.get_dialog_key_algo().to_string().as_str() {
                "rsa" => crate::ssh::keygen::KeyAlgorithm::Rsa,
                _ => crate::ssh::keygen::KeyAlgorithm::Ed25519,
            };
            std::thread::spawn(move || {
                let outcome = (|| -> Result<(String, String), String> {
                    let key = crate::ssh::keygen::generate_keypair(algo).map_err(|e| e.to_string())?;
                    let fp = key.fingerprint.clone();
                    let pem = key.private_key_pem.clone();
                    let key = crate::ssh::keygen::write_key_files(key, algo).map_err(|e| e.to_string())?;
                    let path = key.private_key_path.display().to_string();
                    Ok((
                        pem,
                        format!("{} · {} · {}", t("已生成", "generated"), path, fp),
                    ))
                })()
                .map_err(|e| format!("{}: {e}", t("生成失败", "generation failed")));
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else { return };
                    // Only a successful generation fills the inline field and
                    // arms the upload button; a failure leaves whatever the
                    // user already typed untouched.
                    match outcome {
                        Ok((pem, status)) => {
                            w.set_dialog_key_inline(pem.into());
                            w.set_dialog_key_inline_mode(true);
                            w.set_dialog_key_generated(true);
                            w.set_dialog_key_status(status.into());
                        }
                        Err(status) => {
                            w.set_dialog_key_status(status.into());
                        }
                    }
                });
            });
        });
    }

    // Push the freshly generated public key to the server's authorized_keys so
    // the matching private key can log in without a password. Connects with the
    // session's current credentials (the same host-key / credential / MFA
    // prompt plumbing as "Test connection"), then runs an idempotent append
    // over an SSH exec channel.
    {
        let weak = window.as_weak();
        let runtime = runtime.clone();
        let store = store.clone();
        let edit_forwards = edit_forwards.clone();
        let edit_triggers = edit_triggers.clone();
        let edit_trigger_secrets = edit_trigger_secrets.clone();
        window.on_session_dialog_upload_key(move |draft: SessionDraft| {
            if draft.kind.to_string() != "ssh" {
                if let Some(w) = weak.upgrade() {
                    w.set_dialog_key_status(
                        t("只有 SSH 会话才能上传公钥", "only SSH sessions can receive a public key").into(),
                    );
                }
                return;
            }
            let weak_done = weak.clone();
            let existing = store.borrow().get(draft.id.as_str()).cloned();
            let forwards = match validated_port_forwards(&edit_forwards.borrow()) {
                Ok(forwards) => forwards,
                Err(message) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_key_status(message.into());
                    }
                    return;
                }
            };
            let triggers = match validated_triggers(&edit_triggers.borrow(), &edit_trigger_secrets.borrow()) {
                Ok(triggers) => triggers,
                Err(message) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_key_status(message.into());
                    }
                    return;
                }
            };
            let session = session_from_draft(&draft, existing.as_ref(), forwards, triggers);
            if session.host.is_empty() {
                if let Some(w) = weak.upgrade() {
                    w.set_dialog_key_status(t("请先填写主机地址", "enter a host address first").into());
                }
                return;
            }
            // The generated private key lives in the draft's inline field; turn it
            // back into a public key line for the server.
            let pem = draft.private_key_inline.as_str().trim().to_string();
            let public_key_line = match (|| -> Result<String> {
                let key = ssh_key::PrivateKey::from_openssh(&pem).context("parse generated private key")?;
                Ok(key.public_key().to_openssh().context("serialise public key")?)
            })() {
                Ok(line) => line,
                Err(e) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_key_status(format!("{}: {e}", t("公钥无效", "invalid public key")).into());
                    }
                    return;
                }
            };
            let jump = match resolve_jump(&store, &session) {
                Ok(jump) => jump,
                Err(error) => {
                    if let Some(w) = weak_done.upgrade() {
                        w.set_dialog_key_status(error.to_string().into());
                    }
                    return;
                }
            };
            let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
            let session_for_task = session.clone();
            let events_tx_for_task = events_tx.clone();
            // Clone for the inner relay task: it is spawned as a 'static task,
            // so it must own its Weak handle. The outer task keeps `weak_done`
            // for the final status line.
            let weak_inner = weak_done.clone();
            // Clone the runtime handle for the inner relay task: the outer
            // async block consumes `runtime`, so the nested spawn would move
            // it twice.
            let runtime_inner = runtime.clone();
            runtime.spawn(async move {
                // Relay host-key / credential / MFA prompts to the same UI the
                // "Test connection" button uses, then push the public key.
                runtime_inner.spawn(async move {
                    while let Some(event) = events_rx.recv().await {
                        if matches!(
                            event,
                            SessionEvent::HostKeyPrompt { .. }
                                | SessionEvent::CredentialPrompt { .. }
                                | SessionEvent::MfaPrompt { .. }
                        ) {
                            let weak_prompt = weak_inner.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                let Some(w) = weak_prompt.upgrade() else { return };
                                match event {
                                    SessionEvent::HostKeyPrompt {
                                        host,
                                        port,
                                        key_type,
                                        fingerprint,
                                        changed,
                                        responder,
                                    } => enqueue_hostkey_prompt(
                                        &w, window_id, host, port, key_type, fingerprint, changed, responder,
                                    ),
                                    SessionEvent::CredentialPrompt {
                                        session_id,
                                        host,
                                        user,
                                        need_user,
                                        need_password,
                                        responder,
                                    } => enqueue_cred_prompt(
                                        &w, window_id, session_id, host, user, need_user, need_password, responder,
                                    ),
                                    SessionEvent::MfaPrompt {
                                        session_id,
                                        host,
                                        prompt,
                                        echo,
                                        responder,
                                    } => enqueue_mfa_prompt(
                                        &w, window_id, session_id, host, prompt, echo, responder,
                                    ),
                                    _ => {}
                                }
                            });
                        }
                    }
                });
                let result = push_authorized_keys(session_for_task, jump, public_key_line, events_tx_for_task).await;
                let message = match result {
                    Ok(()) => t("公钥已上传，现在可用密钥登录", "public key uploaded; key login now works").to_string(),
                    Err(e) => format!("{}: {e:#}", t("上传失败", "upload failed")),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_done.upgrade() {
                        w.set_dialog_key_status(message.into());
                    }
                });
            });
        });
    }

    // Add another editable port-forward row (#56, #277).
    {
        let weak = window.as_weak();
        let ef = edit_forwards.clone();
        window.on_add_forward(move || {
            ef.borrow_mut().push(blank_forward_draft());
            if let Some(w) = weak.upgrade() {
                w.set_dialog_forwards(forward_model(&ef.borrow()));
            }
        });
    }
    // Keep each editable row in the Rust-side working set. Saving validates and
    // converts all non-empty rows together, so no separate "added" state exists.
    {
        let ef = edit_forwards.clone();
        window.on_update_forward(move |index: i32, forward: PortFwd| {
            let i = index as usize;
            let mut forwards = ef.borrow_mut();
            if i < forwards.len() {
                forwards[i] = forward;
            }
        });
    }
    // Delete a port forward by index (#56).
    {
        let weak = window.as_weak();
        let ef = edit_forwards.clone();
        window.on_delete_forward(move |index: i32| {
            let i = index as usize;
            {
                let mut v = ef.borrow_mut();
                if i < v.len() {
                    v.remove(i);
                }
                if v.is_empty() {
                    v.push(blank_forward_draft());
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_dialog_forwards(forward_model(&ef.borrow()));
            }
        });
    }

    // Session expect/send trigger editor (#212).
    {
        let weak = window.as_weak();
        let triggers = edit_triggers.clone();
        let secrets = edit_trigger_secrets.clone();
        window.on_add_trigger(move || {
            triggers.borrow_mut().push(blank_trigger_draft());
            secrets.borrow_mut().push(Secret::default());
            if let Some(w) = weak.upgrade() {
                w.set_dialog_triggers(trigger_model(&triggers.borrow()));
            }
        });
    }
    {
        let triggers = edit_triggers.clone();
        window.on_update_trigger(move |index: i32, trigger: TriggerDraft| {
            let i = index as usize;
            let mut values = triggers.borrow_mut();
            if i < values.len() { values[i] = trigger; }
        });
    }
    {
        let weak = window.as_weak();
        let triggers = edit_triggers.clone();
        let secrets = edit_trigger_secrets.clone();
        window.on_delete_trigger(move |index: i32| {
            let i = index as usize;
            let mut values = triggers.borrow_mut();
            let mut saved = secrets.borrow_mut();
            if i < values.len() { values.remove(i); }
            if i < saved.len() { saved.remove(i); }
            if values.is_empty() {
                values.push(blank_trigger_draft());
                saved.push(Secret::default());
            }
            if let Some(w) = weak.upgrade() {
                w.set_dialog_triggers(trigger_model(&values));
            }
        });
    }

    // Connect session -> open a new terminal tab.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let tabs_model = tabs_model.clone();
        let terminals_model = terminals_model.clone();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let handles = handles.clone();
        let bufs = bufs.clone();
        let render_gates = render_gates.clone();
        let runtime = runtime.clone();
        let last_term_size = last_term_size.clone();
        let sftp_handles = sftp_handles.clone();
        let sftp_last_cwd = sftp_last_cwd.clone();
        let tab_statuses = tab_statuses.clone();
        let local_snap = local_snap.clone();
        let local_net_hist = local_net_hist.clone();
        let sftp_follow_cd = sftp_follow_cd.clone();
        let tab_routes = tab_routes.clone();
        let repeat_guard = RefCell::new(DoubleClickGuard::default());
        window.on_connect_session(move |id: SharedString| {
            let id = id.to_string();
            // A session row connects on every click release, so a double-click
            // arrives here twice: two tabs, or two stacked RDP windows that
            // kick each other off (#511). Only the first click connects.
            if !repeat_guard
                .borrow_mut()
                .allow(&id, std::time::Instant::now())
            {
                return;
            }
            let session = if id.starts_with("system:")
                || id.starts_with(super::agents::AGENT_ID_PREFIX)
            {
                // Built-in system shells (incl. the detected pwsh / Git Bash /
                // Nushell and user-defined local shells, #533/#538) and custom
                // agents share one resolver; a miss must not fail silently.
                match session_models::resolve_builtin_session(&store.borrow(), &id) {
                    Some(s) => s,
                    None => return,
                }
            } else {
                match store.borrow().get(&id).cloned() {
                    Some(s) => s,
                    None => return,
                }
            };
            // ── RDP: hand the saved account to the system client, open no tab ──
            // FinalShell does the same thing: meatshell stores host / port /
            // user / password and starts the operating system's own remote
            // desktop client (mstsc on Windows), so the session lives in a
            // native window rather than in one of our tabs.
            if session.kind == SessionKind::Rdp {
                let message = match crate::rdp::launch(&session) {
                    Ok(started) => format!(
                        "{} {}",
                        t("已用系统远程桌面打开", "Opened with the system remote desktop client"),
                        started
                    ),
                    Err(err) => format!("{}: {err}", t("RDP 启动失败", "Failed to start RDP")),
                };
                tracing::info!("{message}");
                if let Some(w) = weak.upgrade() {
                    w.set_ssh_import_hint(message.into());
                }
                return;
            }
            let tab_id = format!("term-{}", uuid::Uuid::new_v4());
            let tab_title = session.name.clone();

            // Connection label shown in the sidebar / status line, per transport.
            let conn_label = match session.kind {
                SessionKind::Ssh => format!("{}@{}", session.user, session.host),
                SessionKind::Serial => {
                    format!("{} @{}", session.serial_port, session.baud_rate)
                }
                SessionKind::Telnet => format!("telnet {}:{}", session.host, session.port),
                SessionKind::Local => format!("local {}", session.name),
                // RDP opens in the system client instead of a tab (see the
                // early return above); this label only exists for completeness.
                SessionKind::Rdp => format!("rdp {}:{}", session.host, session.port),
            };
            // Compatibility mode also suppresses the SFTP side-channel so
            // bastions that only permit one proxied PTY connection stay alive.
            let has_sftp = should_start_sftp(&session);

            // Seed the per-tab status so the sidebar shows "连接中 host" the
            // moment this tab becomes active (the `changed active-tab-id`
            // handler fires refresh-sidebar right after set_active_tab_id below).
            lock_or_recover(&tab_statuses).insert(
                tab_id.clone(),
                TabStatus {
                    host: conn_label.clone(),
                    user: session.user.clone(),
                    session_id: id.clone(),
                    state: 0,
                    is_local: session.kind == SessionKind::Local,
                    ..Default::default()
                },
            );

            // Register tab + terminal state (SFTP fields start empty/loading).
            tabs_model.push(TabInfo {
                id: tab_id.clone().into(),
                title_len: tab_title_len(&tab_title),
                title: tab_title.into(),
                kind: "terminal".into(),
                connected: false,
                local: session.kind == SessionKind::Local,
            });
            // Each session keeps its own SFTP collapse state + sizes, seeded from
            // the global defaults (the "collapse SFTP by default" pref and the
            // persisted panel sizes) so they no longer bleed across panes (#v0.5).
            let (sftp_collapsed_default, sftp_h_default, sftp_w_default) = weak
                .upgrade()
                .map(|w| {
                    (
                        w.get_collapse_sftp_default(),
                        w.get_sftp_panel_height(),
                        w.get_sftp_panel_width(),
                    )
                })
                .unwrap_or((false, 220.0, 380.0));
            // Local list options (#532 follow-up) seed the per-tab row from the
            // window-wide settings (the same store the panel persists to).
            let (sftp_local_show_hidden, sftp_local_sort_key, sftp_local_sort_dir) = weak
                .upgrade()
                .map(|w| {
                    (
                        w.get_sftp_local_show_hidden(),
                        w.get_sftp_local_sort_key().to_string(),
                        w.get_sftp_local_sort_dir(),
                    )
                })
                .unwrap_or((false, String::new(), 0));
            terminals_model.push(TerminalState {
                id: tab_id.clone().into(),
                status: t("连接中...", "Connecting...").into(),
                spans: ModelRc::from(std::rc::Rc::new(VecModel::<TermSpan>::default())),
                cursor_row: 0,
                cursor_col: 0,
                cursor_visible: true,
                rows_used: 0,
                scroll_max: 0,
                scroll_offset: 0,
                is_alt_screen: false,
                mouse_tracked: false,
                find_matches: ModelRc::from(std::rc::Rc::new(VecModel::<TermMatch>::default())),
                selection: ModelRc::from(std::rc::Rc::new(VecModel::<TermMatch>::default())),
                has_selection: false,
                sftp_path: "/".into(),
                sftp_entries: ModelRc::from(std::rc::Rc::new(VecModel::<SftpEntry>::default())),
                sftp_status: if has_sftp {
                    t("SFTP 连接中...", "SFTP connecting...").into()
                } else {
                    t(
                        "此会话类型不支持 SFTP",
                        "SFTP not available for this session",
                    )
                    .into()
                },
                sftp_status_kind: 0,
                sftp_loading: has_sftp,
                sftp_tree_nodes: ModelRc::from(std::rc::Rc::new(
                    VecModel::<SftpTreeNode>::default(),
                )),
                sftp_selected_count: 0,
                sftp_sort_key: "".into(),
                sftp_sort_dir: 0,
                sftp_bookmarks: {
                    let v: Vec<slint::SharedString> = store
                        .borrow()
                        .sftp_bookmarks(&conn_label)
                        .iter()
                        .map(|p| p.as_str().into())
                        .collect();
                    ModelRc::from(std::rc::Rc::new(VecModel::from(v)))
                },
                sftp_available: has_sftp,
                // Split (dual-pane) view starts off; the local pane remembers
                // the home folder until it is first opened (lazy listing).
                sftp_dual: false,
                sftp_local_path: home_display().into(),
                sftp_local_entries: ModelRc::from(std::rc::Rc::new(
                    VecModel::<SftpEntry>::default(),
                )),
                sftp_local_status: "".into(),
                sftp_local_loading: false,
                sftp_local_selected_count: 0,
                sftp_local_pane_width: 0.0,
                sftp_recent: {
                    let v: Vec<slint::SharedString> = store
                        .borrow()
                        .sftp_places(&id)
                        .recent
                        .iter()
                        .map(|p| p.as_str().into())
                        .collect();
                    ModelRc::from(std::rc::Rc::new(VecModel::from(v)))
                },
                // #500 per-session bookmarks live in their own field: the row's
                // `sftp_bookmarks` belongs to the older per-server menu.
                sftp_places_bookmarks: {
                    let v: Vec<slint::SharedString> = store
                        .borrow()
                        .sftp_places(&id)
                        .bookmarks
                        .iter()
                        .map(|p| p.as_str().into())
                        .collect();
                    ModelRc::from(std::rc::Rc::new(VecModel::from(v)))
                },
                sftp_bookmarked: {
                    // Seed the star flag from the initial path ("/") so the
                    // toolbar matches the places menu on the first draw; the
                    // navigation callbacks own it afterwards.
                    store
                        .borrow()
                        .sftp_places(&id)
                        .bookmarks
                        .iter()
                        .any(|p| p.as_str() == "/")
                },
                font_size: 0,
                tunnels: ModelRc::from(std::rc::Rc::new(VecModel::<TunnelInfo>::default())),
                sftp_collapsed: !has_sftp || sftp_collapsed_default,
                sftp_panel_height: sftp_h_default,
                sftp_panel_width: sftp_w_default,
                sftp_saved_height: sftp_h_default,
                link_matches: ModelRc::from(std::rc::Rc::new(VecModel::<TermMatch>::default())),
                find_active: -1,
                find_total: 0,
                command_running: false,
                backpressure: false,
                find_current: TermMatch::default(),
                sftp_local_quick_places: ModelRc::from(std::rc::Rc::new(
                    VecModel::<SftpLocalPlace>::default(),
                )),
                sftp_local_drive_places: ModelRc::from(std::rc::Rc::new(
                    VecModel::<SftpLocalPlace>::default(),
                )),
                sftp_local_bookmarks: ModelRc::from(std::rc::Rc::new(VecModel::<SharedString>::default())),
                sftp_local_recent: ModelRc::from(std::rc::Rc::new(VecModel::<SharedString>::default())),
                sftp_local_bookmarked: false,
                sftp_local_show_hidden,
                sftp_local_sort_key: sftp_local_sort_key.into(),
                sftp_local_sort_dir,
                sftp_local_filter: "".into(),
                sftp_local_all_entries: ModelRc::from(std::rc::Rc::new(
                    VecModel::<SftpEntry>::default(),
                )),
            });
            // Create vt100 parser for this tab (default 24×80; resized on first
            // terminal-resize callback). 5000-line scrollback is stored for
            // future scroll-navigation support.
            let is_dark_now = weak.upgrade().map(|w| w.get_dark_mode()).unwrap_or(true);
            let (output_highlight, custom_highlight_rules) = {
                let settings = store.borrow();
                (
                    OutputHighlightPreset::from_settings(
                        settings.output_highlight_enabled(),
                        settings.output_highlight_preset(),
                    ),
                    compile_output_rules(settings.output_highlight_rules()),
                )
            };
            lock_or_recover(&bufs).insert(
                tab_id.clone(),
                Arc::new(Mutex::new(TermBuffer {
                    parser: crate::terminal::vt_parser(24, 80, 5000),
                    find_query: String::new(),
                    find_positions: Vec::new(),
                    find_active: -1,
                    find_dirty: false,
                    find_cursor: None,
                    search_history_mode: store.borrow().search_history_mode(),
                    command_running: false,
                    backpressure: false,
                    is_dark: is_dark_now,
                    output_highlight,
                    custom_highlight_rules,
                    json_format_output: store.borrow().json_format_output(),
                    vt100_drawing: session.vt100_drawing,
                    charset: crate::terminal::CharsetTracker::default(),
                    interactive_echo_until: std::time::Instant::now(),
                    sel_anchor: None,
                    sel_focus: None,
                    sel_ranges: Vec::new(),
                    mouse_tracked: false,
                    history: VecDeque::new(),
                    prev: Vec::new(),
                    view_offset: 0,
                    scroll_accum: 0.0,
                    displayed_text: Vec::new(),
                    action_links: Vec::new(),
                    action_link_flags: action_link_flags(&store.borrow()),
                    csi_state: CsiState::Normal,
                    csi_pending: Vec::new(),
                    raw: std::collections::VecDeque::new(),
                    session_log: None,
                    session_log_spec: session_log_spec(&session),
                })),
            );
            // Session logging (#265): open the transcript before the first
            // byte arrives.
            {
                let (enabled, dir) = {
                    let settings = store.borrow();
                    (settings.session_log_enabled(), settings.session_log_dir())
                };
                with_term_buf(&bufs, &tab_id, |b| apply_session_log_to_buffer(b, enabled, &dir));
            }
            lock_or_recover(&render_gates).insert(
                tab_id.clone(),
                Arc::new(TabRenderGate::new(RENDER_MIN_INTERVAL)),
            );
            // No followed-cwd yet: the first OSC 7 always triggers a follow.
            lock_or_recover(&sftp_last_cwd).remove(&tab_id);
            // Add the new tab to the focused pane and re-flatten (this also sets
            // active-tab-id to the new tab via refresh_panes).
            layout.borrow_mut().add_tab(tab_id.clone());
            if let Some(w) = weak.upgrade() {
                let lay = layout.borrow().clone();
                refresh_panes(
                    &w,
                    &lay,
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }

            // Spawn the shell (+ SFTP) workers and their event-pump threads.
            // Shared with in-place reconnect (#79) via start_session_in_tab.
            let ctx = ConnectCtx {
                weak: weak.clone(),
                editor: editor.clone(),
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
                tab_routes: tab_routes.clone(),
            };
            start_session_in_tab(&tab_id, session, &ctx);
        });
    }

    // Duplicate a tab's connection (#v0.5): open a fresh tab to the same saved
    // session, landing in the same pane as the source tab.
    {
        let weak = window.as_weak();
        let tab_statuses = tab_statuses.clone();
        let layout = layout.clone();
        window.on_tab_duplicate(move |tab_id: SharedString| {
            let tab_id = tab_id.to_string();
            let session_id = tab_statuses
                .lock()
                .unwrap()
                .get(&tab_id)
                .map(|s| s.session_id.clone())
                .unwrap_or_default();
            if session_id.is_empty() {
                return;
            }
            // Land the new tab in the same pane as the source. Read the pane id
            // into a local first so the immutable borrow is dropped before the
            // borrow_mut (else RefCell panics on the overlapping borrow).
            let pane = layout.borrow().leaf_of_tab(&tab_id);
            if let Some(pane) = pane {
                layout.borrow_mut().focused = pane;
            }
            if let Some(w) = weak.upgrade() {
                w.invoke_connect_session(session_id.into());
            }
        });
    }

    // Rename session (tab context menu): open the dialog pre-filled with the
    // tab's current title.
    {
        let weak = window.as_weak();
        let tabs_model = tabs_model.clone();
        window.on_tab_rename_request(move |tab_id: SharedString| {
            let tab_id = tab_id.to_string();
            if tab_id.is_empty() || tab_id == "welcome" {
                return;
            }
            use slint::Model as _;
            let title = (0..tabs_model.row_count())
                .find_map(|i| {
                    let row = tabs_model.row_data(i)?;
                    (row.id.as_str() == tab_id).then(|| row.title.to_string())
                })
                .unwrap_or_default();
            if let Some(w) = weak.upgrade() {
                w.set_tab_rename_id(tab_id.into());
                w.set_tab_rename_value(title.into());
                w.set_tab_rename_open(true);
            }
        });
    }

    // Apply the new display name. An empty name clears the override and
    // restores the saved session's name. Display-only: the config is untouched.
    {
        let weak = window.as_weak();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model_rename.clone();
        let tab_statuses = tab_statuses.clone();
        let tab_titles = tab_titles.clone();
        let store = store.clone();
        window.on_rename_tab(move |tab_id: SharedString, name: SharedString| {
            use slint::Model as _;
            if let Some(w) = weak.upgrade() {
                w.set_tab_rename_open(false);
            }
            let tab_id = tab_id.to_string();
            let name = name.trim().to_string();
            let title = if name.is_empty() {
                tab_titles.borrow_mut().remove(&tab_id);
                let session_id = tab_statuses
                    .lock()
                    .unwrap()
                    .get(&tab_id)
                    .map(|s| s.session_id.clone())
                    .unwrap_or_default();
                // Two separate borrows: the builtin fallback must not run while
                // the store lookup's RefCell borrow is still alive.
                let saved = store.borrow().get(&session_id).map(|s| s.name.clone());
                saved
                    .or_else(|| {
                        session_models::system_shell_sessions(&store.borrow())
                            .into_iter()
                            .find(|s| s.id == session_id)
                            .map(|s| s.name)
                    })
                    .or_else(|| super::agents::agent_name(&store.borrow(), &session_id))
            } else {
                tab_titles.borrow_mut().insert(tab_id.clone(), name.clone());
                Some(name)
            };
            let Some(title) = title else {
                return;
            };
            for i in 0..tabs_model.row_count() {
                if let Some(mut row) = tabs_model.row_data(i) {
                    if row.id.as_str() == tab_id {
                        row.title_len = tab_title_len(&title);
                        row.title = title.clone().into();
                        tabs_model.set_row_data(i, row);
                        break;
                    }
                }
            }
            // The tab strips render pane.tabs — per-pane snapshot sub-models
            // built by refresh_panes — not tabs_model itself, so update them
            // in place too.
            for pi in 0..panes_model.row_count() {
                if let Some(pane) = panes_model.row_data(pi) {
                    for ti in 0..pane.tabs.row_count() {
                        if let Some(mut tab) = pane.tabs.row_data(ti) {
                            if tab.id.as_str() == tab_id {
                                tab.title_len = tab_title_len(&title);
                                tab.title = title.clone().into();
                                pane.tabs.set_row_data(ti, tab);
                            }
                        }
                    }
                }
            }
        });
    }
}
