//! SFTP subsystem worker.
//!
//! Each terminal tab that spawns an SSH shell also spawns a *separate* SSH
//! connection for SFTP. This keeps the shell PTY completely unblocked: large
//! file transfers cannot stall readline or vim.
//!
//! The public API is a simple command channel (`SftpHandle::commands`) that
//! accepts `SftpCommand` messages. Results and status updates are pushed back
//! via the shared `UnboundedSender<SessionEvent>` that already exists for the
//! terminal tab.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use uuid::Uuid;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt};
use russh::client::{self, Handler};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::{RawSftpSession, SftpSession};
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use ssh_key::{HashAlg, PublicKey};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::config::{AuthMethod, Session};
use crate::i18n::t;
use crate::ssh::{
    format_mtime, format_size, RemoteEntry, RemoteTreeNode, SessionEvent, UploadConflictReply,
    UploadConflictResponder,
};

use super::common::{
    parse_scp_stat_output, scp_c_frame, scp_download_command, scp_frame_name_ok, scp_mkdir_command,
    scp_move_command, scp_parse_frame, scp_remove_command, scp_stat_single_command, scp_test_dir,
    scp_test_exists, scp_tmp_name, scp_upload_command, ScpFrame,
};
use super::elevation::Elevation;
use super::scp::ScpPeer;
use super::transfer::{
    numbered_name, DedupPolicy, DownloadConflict, DragReport, SftpCommand, SftpHandle,
    SftpQueueConfig, UploadDecision,
};

/// Recover a poisoned guard instead of aborting the worker: the shared map is
/// still sound after the owner panicked, and losing the connection here would
/// kill every transfer in flight. Mirrors the UI-side helper of the same name.
fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl SftpHandle {
    pub fn list_dir(&self, path: String) {
        let _ = self.commands.send(SftpCommand::ListDir(path));
    }
    pub fn refresh_dir(&self, path: String) {
        let _ = self.commands.send(SftpCommand::RefreshDir(path));
    }
    pub fn download(
        &self,
        remote: String,
        local_dir: String,
        conflict: DownloadConflict,
        policy: DedupPolicy,
    ) {
        let _ = self.commands.send(SftpCommand::Download {
            remote,
            local_dir,
            conflict,
            policy,
        });
    }
    pub fn download_archive(&self, remote_dir: String, names: Vec<String>, local_dir: String) {
        let _ = self.commands.send(SftpCommand::DownloadArchive {
            remote_dir,
            names,
            local_dir,
        });
    }
    /// Dual-pane download: copy `remotes` directly into `local_dir`.
    pub fn download_into(
        &self,
        remotes: Vec<String>,
        local_dir: String,
        conflict: DownloadConflict,
    ) {
        let _ = self.commands.send(SftpCommand::DownloadInto {
            remotes,
            local_dir,
            conflict,
        });
    }
    pub fn cancel_transfer(&self, id: String) {
        let _ = self.commands.send(SftpCommand::CancelTransfer(id));
    }
    pub fn upload(&self, local: PathBuf, remote_dir: String, decision: UploadDecision) {
        let _ = self.commands.send(SftpCommand::Upload {
            local,
            remote_dir,
            cleanup_after: None,
            decision,
        });
    }
    pub fn copy_to(
        &self,
        remotes: Vec<String>,
        target: UnboundedSender<SftpCommand>,
        target_dir: String,
    ) {
        let _ = self.commands.send(SftpCommand::CopyTo {
            remotes,
            target,
            target_dir,
        });
    }
    pub fn toggle_tree_node(&self, path: String) {
        let _ = self.commands.send(SftpCommand::ToggleTreeNode(path));
    }
    pub fn delete(&self, path: String) {
        let _ = self.commands.send(SftpCommand::Delete(path));
    }
    pub fn open_temp(&self, remote: String, edit: bool) {
        let _ = self.commands.send(SftpCommand::OpenTemp { remote, edit });
    }
    pub fn rename(&self, from: String, to: String) {
        let _ = self.commands.send(SftpCommand::Rename { from, to });
    }
    pub fn chmod(&self, path: String, mode: u32) {
        let _ = self.commands.send(SftpCommand::Chmod { path, mode });
    }
    pub fn mkdir(&self, path: String) {
        let _ = self.commands.send(SftpCommand::MkDir(path));
    }
    pub fn touch(&self, path: String) {
        let _ = self.commands.send(SftpCommand::TouchFile(path));
    }
    pub fn read_text(&self, remote: String, edit: bool) {
        let _ = self.commands.send(SftpCommand::ReadText { remote, edit });
    }
    /// Double-click on a file: edit it if it is text, else download (#493).
    pub fn open_or_download(&self, remote: String) {
        let _ = self.commands.send(SftpCommand::OpenOrDownload { remote });
    }
    pub fn write_text(&self, remote: String, content: String) {
        let _ = self
            .commands
            .send(SftpCommand::WriteText { remote, content });
    }
    pub fn close(&self) {
        let _ = self.commands.send(SftpCommand::Close);
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn chmod_attributes(mode: u32) -> FileAttributes {
    FileAttributes {
        permissions: Some(mode),
        ..FileAttributes::empty()
    }
}

/// Turn an SFTP-worker failure into a status-bar message.
///
/// SFTP runs on its own SSH connection, fully separate from the shell PTY, so
/// when it can't connect the terminal keeps working — we just surface why in the
/// SFTP panel. The common bastion/jump-host case is "shell is allowed but the
/// `sftp` subsystem is not", which shows up as a failed subsystem request /
/// channel / handshake (or an explicit "permission denied"). For that family we
/// give a plain-language hint instead of the raw russh error (#190).
fn friendly_sftp_error(err: &anyhow::Error) -> String {
    // SFTP privileges problems (#438) already say what to fix.
    if let Some(elevation) = err
        .chain()
        .find_map(|e| e.downcast_ref::<super::elevation::ElevationError>())
    {
        return elevation.to_string();
    }
    let chain = err
        .chain()
        .map(|e| e.to_string().to_lowercase())
        .collect::<Vec<_>>()
        .join(" | ");
    let permission_like = [
        "subsystem",      // server refused the `sftp` subsystem request
        "sftp channel",   // channel_open_session refused
        "sftp handshake", // subsystem opened but no SFTP server behind it
        "permission",
        "denied",
        "prohibited", // "administratively prohibited"
        "not allowed",
    ]
    .iter()
    .any(|k| chain.contains(k));
    if permission_like {
        // OpenWrt ships Dropbear, which does not implement the `sftp` subsystem at
        // all. A generic "check your permissions" hint sends those users chasing a
        // permission problem they do not have, so call out the actual remedy
        // (install an SFTP server, or use the SCP-capable transfer mode).
        if chain.contains("subsystem") || chain.contains("sftp handshake") {
            t(
                "服务器未提供 SFTP 服务（OpenWrt/Dropbear 常见）。请安装 openssh-sftp-server，或在会话设置中改用支持 SCP 的传输方式",
                "The server does not offer SFTP (common on OpenWrt/Dropbear). Install openssh-sftp-server, or switch the session's transfer mode to one that supports SCP",
            )
            .to_string()
        } else {
            t(
                "SFTP 不可用,请检查是否有访问权限(服务器可能未开放 SFTP)",
                "SFTP unavailable — check whether you have permission (server may not allow SFTP)",
            )
            .to_string()
        }
    } else {
        format!("{}: {err:#}", t("SFTP 错误", "SFTP error"))
    }
}

/// Spawn an SFTP worker on the Tokio runtime.
///
/// The worker opens its own SSH connection to the same server, authenticates,
/// and requests the `sftp` subsystem. Events (directory listings, progress,
/// errors) are sent back via `events`, which is the same sender used by the
/// terminal's shell session.
pub fn spawn_sftp(
    runtime: &tokio::runtime::Handle,
    session: Session,
    jump: Vec<Session>,
    events: UnboundedSender<SessionEvent>,
    queue: SftpQueueConfig,
) -> SftpHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let self_tx = cmd_tx.clone();
    let events_err = events.clone();
    let join = runtime.spawn(crate::ssh::inherit_automation_cancellation(async move {
        if let Err(err) = run_sftp(session, jump, cmd_rx, self_tx, events, queue).await {
            // Surface as SftpError, not SftpStatus: the status variant leaves the
            // panel's spinner running, so a failed connect ("SFTP 连接中...") never
            // resolves and looks like it is stuck mid-handshake. SftpError stops the
            // spinner and shows the reason, leaving any existing listing in place.
            let _ = events_err.send(SessionEvent::SftpError(friendly_sftp_error(&err)));
        }
    }));
    SftpHandle {
        commands: cmd_tx,
        join,
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tree state helpers
// ---------------------------------------------------------------------------

/// Recursively build the flat node list from tree state (DFS pre-order).
fn build_tree_nodes(
    path: &str,
    depth: u32,
    expanded: &std::collections::HashSet<String>,
    tree_dirs: &std::collections::HashMap<String, Vec<(String, String)>>,
    nodes: &mut Vec<RemoteTreeNode>,
) {
    let name = if path == "/" {
        "/".to_string()
    } else {
        path.rsplit('/').next().unwrap_or(path).to_string()
    };
    let children = tree_dirs.get(path);
    let has_children = children.map(|c| !c.is_empty()).unwrap_or(true);
    let is_expanded = expanded.contains(path);
    nodes.push(RemoteTreeNode {
        path: path.to_string(),
        name,
        depth,
        expanded: is_expanded,
        has_children,
    });
    if is_expanded {
        if let Some(ch) = children {
            for (_, child_path) in ch {
                build_tree_nodes(child_path, depth + 1, expanded, tree_dirs, nodes);
            }
        }
    }
}

/// Rebuild the flat tree node list from the current cache and push it to the UI.
fn emit_tree(
    tree_dirs: &std::collections::HashMap<String, Vec<(String, String)>>,
    tree_expanded: &std::collections::HashSet<String>,
    events: &UnboundedSender<SessionEvent>,
) {
    let mut nodes = Vec::new();
    build_tree_nodes("/", 0, tree_expanded, tree_dirs, &mut nodes);
    let _ = events.send(SessionEvent::SftpTreeUpdate(nodes));
}

/// Re-fetch a directory's sub-directories into the tree cache, but only if that
/// directory is already known to the tree (root or previously expanded) — so a
/// mutation under a collapsed/unknown branch doesn't graft unrelated nodes in.
/// This is how create/delete/rename keep the left tree in sync without a
/// reconnect (#189).
async fn sync_tree_dir(
    sftp: &SftpSession,
    dir: &str,
    tree_dirs: &mut std::collections::HashMap<String, Vec<(String, String)>>,
) {
    if tree_dirs.contains_key(dir) {
        let dirs = list_dirs_only_impl(sftp, dir).await.unwrap_or_default();
        tree_dirs.insert(dir.to_string(), dirs);
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

async fn run_sftp(
    session: Session,
    jump: Vec<Session>,
    mut commands: UnboundedReceiver<SftpCommand>,
    self_tx: UnboundedSender<SftpCommand>,
    events: UnboundedSender<SessionEvent>,
    queue: SftpQueueConfig,
) -> Result<()> {
    // Bound concurrent transfer tasks (each Download/Upload spawns its own
    // task; this semaphore caps how many run at once per the setting).
    let permits = queue.concurrency.clamp(1, 4) as usize;
    let transfer_slots = Arc::new(tokio::sync::Semaphore::new(permits));
    let _ = events.send(SessionEvent::SftpStatus(
        t("SFTP 连接中...", "SFTP connecting...").into(),
    ));

    // Open a dedicated SSH connection for SFTP.
    let mut config = client::Config {
        // Match the shell connection's algorithm set so SFTP reaches the same
        // legacy servers (#172) instead of failing with "No common algorithm".
        preferred: russh::Preferred {
            kex: std::borrow::Cow::Borrowed(crate::ssh::COMPAT_KEX),
            cipher: std::borrow::Cow::Borrowed(crate::ssh::COMPAT_CIPHER),
            ..russh::Preferred::DEFAULT
        },
        ..<_>::default()
    };
    // Keep the idle SFTP connection alive (#160). Without a keepalive, an idle
    // SFTP connection (no file ops for a while) gets silently dropped by
    // NAT / firewall / server idle timeouts; afterwards every operation fails
    // ("folder read failed"). Uses the same setting as the terminal (#515).
    crate::ssh::apply_keepalive(&mut config);
    let config = Arc::new(config);

    let addr = format!("{}:{}", session.host, session.port);
    // Isolate external-editor files by connection and keep the server in the
    // visible local filename. Different hosts (or concurrent edit sessions)
    // can therefore open the same remote basename without sharing one temp
    // file or editor document (#318).
    let external_edit_prefix = sanitize_filename(&session.host);
    let external_edit_dir = std::env::temp_dir().join("meatshell").join(format!(
        "{}-{}-{}",
        external_edit_prefix,
        session.port,
        Uuid::new_v4()
    ));
    // Keep the jump-host connection alive for the whole SFTP session — the
    // direct-tcpip tunnel rides on it (#211). Declared here so it lives to the
    // end of the function; `_`-prefixed so it isn't flagged unused.
    let mut _jump_keepalive;
    // Tunnel through an SSH jump host (#211), the same proxy as the shell (#7),
    // or connect directly.
    let mut handle = match jump.as_slice() {
        j if !j.is_empty() => {
            let (h, jh) = crate::ssh::connect_target_via_jump(
                j,
                &session.host,
                session.port,
                config.clone(),
                sftp_handler(&session, &events),
                &events,
            )
            .await
            .with_context(|| format!("sftp connect {} via jump failed", addr))?;
            _jump_keepalive = jh;
            h
        }
        _ => {
            _jump_keepalive = Vec::new();
            crate::ssh::connect_direct_ssh(
                &session,
                config.clone(),
                sftp_handler(&session, &events),
                &events,
            )
            .await?
        }
    };

    // Resolve missing username/password (shares the shell's prompt; the UI
    // de-dupes by session id so SFTP doesn't prompt a second time) (#110).
    let (user, password) = match crate::ssh::resolve_credentials(&session, &events).await {
        Some(c) => c,
        None => return Err(anyhow!(t("已取消登录", "login cancelled"))),
    };

    // --- Authenticate (same method as the shell session) -------------------
    let authed = match session.auth {
        AuthMethod::Password => {
            let mut ok = crate::ssh::network_stage(
                &format!(
                    "SFTP password authentication at {}:{}",
                    session.host, session.port
                ),
                handle.authenticate_password(&user, password.as_str()),
            )
            .await?;
            if !ok {
                // Match the shell session's fallback: russh can hang if a second
                // auth method is attempted on the same failed handle, so reconnect
                // before trying keyboard-interactive (#86, #186).
                crate::ssh::disconnect_ssh(&handle, "authentication fallback").await;
                handle = match jump.as_slice() {
                    j if !j.is_empty() => {
                        let (h, jh) = crate::ssh::connect_target_via_jump(
                            j,
                            &session.host,
                            session.port,
                            config.clone(),
                            sftp_handler(&session, &events),
                            &events,
                        )
                        .await
                        .with_context(|| format!("sftp reconnect {} via jump failed", addr))?;
                        _jump_keepalive = jh;
                        h
                    }
                    _ => {
                        _jump_keepalive = Vec::new();
                        crate::ssh::connect_direct_ssh(
                            &session,
                            config.clone(),
                            sftp_handler(&session, &events),
                            &events,
                        )
                        .await?
                    }
                };
                ok = crate::ssh::keyboard_interactive_auth(
                    &mut handle,
                    &user,
                    password.as_str(),
                    &session.id,
                    &session.host,
                    &events,
                )
                .await
                .context("sftp keyboard-interactive auth failed")?;
            }
            ok
        }
        AuthMethod::KeyboardInteractive => crate::ssh::keyboard_interactive_auth(
            &mut handle,
            &user,
            password.as_str(),
            &session.id,
            &session.host,
            &events,
        )
        .await
        .context("sftp keyboard-interactive auth failed")?,
        AuthMethod::Key => {
            // An encrypted private key needs its passphrase; reuse the session's
            // password field for it (empty = unencrypted), exactly like the shell
            // session does — otherwise a passphrase-protected key authenticates the
            // shell but fails SFTP with "the key is encrypted" (#133).
            let pass = password.as_str();
            let keypair = crate::ssh::load_session_private_key(&session, pass)?;
            // RSA keys need an explicit SHA-2 hash; other key types don't.
            let hash = keypair.algorithm().is_rsa().then_some(HashAlg::Sha256);
            let key_with_hash = PrivateKeyWithHashAlg::new(Arc::new(keypair), hash)
                .context("invalid private key")?;
            crate::ssh::network_stage(
                &format!(
                    "SFTP public-key authentication at {}:{}",
                    session.host, session.port
                ),
                handle.authenticate_publickey(&user, key_with_hash),
            )
            .await?
        }
    };

    if !authed {
        return Err(anyhow!(t("SFTP 认证失败", "SFTP authentication failed")));
    }

    // --- Decide who SFTP works as (#438) ----------------------------------
    // Login user via the `sftp` subsystem by default; root via sudo, or a
    // custom SFTP server command, when the session asks for it.
    let elevation = Elevation::resolve(&handle, &session, password.as_str()).await?;

    // --- Open the SFTP channel ---------------------------------------------
    // OpenWrt ships Dropbear, which has no `sftp` subsystem: the subsystem
    // request itself is refused ("subsystem" in the chain) or the channel opens
    // then dies mid-handshake ("sftp handshake"). When the session is on the
    // plain subsystem and `scp` is present, fall back to driving the server's
    // `scp` for the panel's browse-only operations instead of showing
    // "SFTP unavailable". Custom/sudo modes keep their own error — those are
    // explicit user choices and must not be silently re-routed.
    let fallback_key = session
        .sftp_mode
        .is_empty()
        .then(|| scp_fallback_key(&session.host, session.port, &user));
    let sftp = if let Some(key) = &fallback_key {
        if scp_fallback_known(key) {
            // Verdict already recorded: skip the doomed subsystem attempt and
            // probe + list in one exec. A 127 (scp disappeared) clears the
            // verdict and falls through to the subsystem path below.
            match scp_probe_home_and_list(&handle).await {
                Ok(prefetched) => {
                    tracing::info!(
                        "sftp {}:{} known no-SFTP host; switching to SCP fallback (cached)",
                        session.host,
                        session.port
                    );
                    let handle = std::sync::Arc::new(SftpLink { handle, elevation });
                    let _ = events.send(SessionEvent::SftpStatus(
                        t(
                            "已切换 SCP 模式（服务器未提供 SFTP）...",
                            "Switched to SCP mode (server does not offer SFTP)...",
                        )
                        .into(),
                    ));
                    return run_scp_browse(
                        handle, events, commands, self_tx, queue,
                        Some(prefetched),
                        Some(key.clone()),
                    )
                    .await;
                }
                Err(ScpProbeOutcome::NoScp) => {
                    tracing::warn!(
                        "sftp {}:{} cached SCP verdict but scp is gone; re-probing subsystem",
                        session.host,
                        session.port
                    );
                    scp_fallback_clear(key);
                }
                Err(ScpProbeOutcome::ProbeFailed(e)) => return Err(e),
            }
        }
        match try_open_sftp(&handle, &elevation, &session, SFTP_SUBSYSTEM_PROBE_TIMEOUT).await {
            Ok(s) => {
                // A working subsystem means any stale verdict is wrong.
                scp_fallback_clear(key);
                s
            }
            Err(err) => {
                if scp_unavailable_like(&err) {
                    match scp_probe_home_and_list(&handle).await {
                        Ok(prefetched) => {
                            tracing::info!(
                                "sftp {}:{} subsystem missing; switching to SCP fallback",
                                session.host,
                                session.port
                            );
                            scp_fallback_mark(key);
                            let handle = std::sync::Arc::new(SftpLink { handle, elevation });
                            let _ = events.send(SessionEvent::SftpStatus(
                                t(
                                    "已切换 SCP 模式（服务器未提供 SFTP）...",
                                    "Switched to SCP mode (server does not offer SFTP)...",
                                )
                                .into(),
                            ));
                            return run_scp_browse(
                                handle, events, commands, self_tx, queue,
                                Some(prefetched),
                                Some(key.clone()),
                            )
                            .await;
                        }
                        Err(ScpProbeOutcome::NoScp) => {}
                        Err(ScpProbeOutcome::ProbeFailed(e)) => {
                            // The SCP probe failing must not mask why SFTP was
                            // rejected: keep the original error on top.
                            return Err(err.context(e.to_string()));
                        }
                    }
                }
                // A timed-out handshake is a slow link, not proof of a missing
                // subsystem: point the user at the manual SCP mode instead of
                // a bare timeout.
                let err = if err.chain().any(|e| e.to_string().contains("timed out")) {
                    err.context(t(
                        "连接响应慢：请重试；若服务器确实未提供 SFTP，可手动切换到 SCP 模式。",
                        "The connection is slow to respond: retry; if the server really has no SFTP, switch to SCP mode manually.",
                    ))
                } else {
                    err
                };
                return Err(err);
            }
        }
    } else {
        // Custom/sudo modes never fall back, so they keep the generic budget:
        // their server command (sudo sftp-server, a custom path) may legitimately
        // start slower than a pre-installed subsystem.
        try_open_sftp(
            &handle,
            &elevation,
            &session,
            crate::ssh::SSH_NETWORK_TIMEOUT,
        )
        .await?
    };
    if !elevation.label().is_empty() {
        tracing::info!(
            "sftp {}:{} opened{}",
            session.host,
            session.port,
            elevation.label()
        );
    }
    // Share the session + connection so transfers can run on their own task,
    // leaving the command loop free to list/switch directories meanwhile (#116-2).
    let sftp = std::sync::Arc::new(sftp);
    let handle = std::sync::Arc::new(SftpLink { handle, elevation });

    let home = sftp
        .canonicalize(".")
        .await
        .unwrap_or_else(|_| "/".to_string());
    // A failed home listing is not fatal: the panel stays usable (the user can
    // navigate elsewhere), exactly like before the SCP probe was added.
    let home_entries: Option<Vec<RemoteEntry>> = match list_dir_impl(&sftp, &home).await {
        Ok(entries) => Some(entries),
        Err(e) => {
            let _ = events.send(SessionEvent::SftpError(list_error_msg(&home, &e)));
            None
        }
    };
    // When SFTP's `read_dir` comes back empty but the directory actually has
    // entries (Dropbear's minimal SFTP server), cross-check with `ls -A` and
    // switch to SCP mode. Every probe step degrades to "stay in SFTP mode" on
    // failure so a transient exec-channel problem never kills a healthy
    // session.
    if let Some(entries) = &home_entries {
        if entries.is_empty()
            && session.sftp_mode.is_empty()
            && !handle.elevation.is_custom()
            && probe_scp(&handle.handle).await.ok().flatten().is_some()
        {
            match exec_remote_output(
                &handle.handle,
                &super::common::scp_list_names_command(&home),
                // A `ls -A` name-only probe; the 10-minute bound would only
                // block the startup for a listing that is merely a fallback
                // decision point ("stay in SFTP mode" on failure).
                super::common::SCP_CMD_IO_TIMEOUT,
            )
            .await
            {
                Ok((status, output)) => {
                    let out = output.trim();
                    if status == 0 && !out.is_empty() {
                        tracing::warn!(
                            "sftp {}:{} read_dir returned empty but directory has entries; switching to SCP fallback",
                            session.host,
                            session.port
                        );
                        let _ = events.send(SessionEvent::SftpStatus(
                            t(
                                "SFTP 目录读取不可用，已切换 SCP 模式...",
                                "SFTP directory listing is unavailable; switched to SCP mode...",
                            )
                            .into(),
                        ));
                        return run_scp_browse(
                            handle, events, commands, self_tx, queue, None, None,
                        )
                        .await;
                    }
                    if status != 0 && !out.is_empty() {
                        tracing::warn!(
                            "sftp {}:{} empty-listing cross-check failed: {}",
                            session.host,
                            session.port,
                            out
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "sftp {}:{} empty-listing cross-check unavailable; staying in SFTP mode: {e}",
                        session.host,
                        session.port
                    );
                }
            }
        }
    }

    // Owner/group names live for the whole SFTP connection so every listing
    // after the first can paint names without another `getent` round-trip.
    let owner_cache: SharedOwnerCache = Arc::new(Mutex::new(OwnerNameCache::default()));
    let (names_ready_tx, names_ready_rx) = tokio::sync::watch::channel(false);
    {
        let handle = handle.clone();
        let owner_cache = owner_cache.clone();
        tokio::spawn(async move {
            prefetch_all_owner_names(&handle, &owner_cache).await;
            let _ = names_ready_tx.send(true);
        });
    }

    let cancels: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let _ = events.send(SessionEvent::SftpStatus(format!(
        "{} {}...",
        t("SFTP 加载", "SFTP loading"),
        home
    )));
    if let Some(entries) = home_entries {
        // Prefer painting names with the listing: wait briefly for the
        // connect-time full getent dump that started above. (A failed home
        // listing already sent its `SftpError` above; the loop stays usable.)
        wait_owner_prefetch(&names_ready_rx, Duration::from_secs(3)).await;
        let mut entries = entries;
        let need = lock_or_recover(&owner_cache).apply(&mut entries);
        let _ = events.send(SessionEvent::SftpEntries {
            path: home.clone(),
            entries: entries.clone(),
        });
        // Defer any remaining remote name lookup so directory-tree init can
        // run first; the command loop's EnrichEntries repaints with names.
        if !need.is_complete() {
            let _ = self_tx.send(SftpCommand::EnrichEntries {
                path: home.clone(),
                entries,
            });
        }
        let _ = events.send(SessionEvent::SftpStatus(home.clone()));
    }

    // --- Directory tree initialization -------------------------------------
    // tree_dirs: path -> [(child_name, child_full_path)] for directories only
    // tree_expanded: set of paths currently shown as expanded
    let mut tree_dirs: std::collections::HashMap<String, Vec<(String, String)>> =
        std::collections::HashMap::new();
    let mut tree_expanded: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Fetch root "/" subdirs, then expand path down to home.
    let root_dirs = list_dirs_only_impl(&sftp, "/").await.unwrap_or_default();
    tree_dirs.insert("/".to_string(), root_dirs);
    tree_expanded.insert("/".to_string());

    // Walk each path segment from "/" toward home, expanding as we go.
    if home != "/" {
        let mut current = "/".to_string();
        for segment in home.trim_start_matches('/').split('/') {
            if segment.is_empty() {
                continue;
            }
            let child = format!("{}/{}", current.trim_end_matches('/'), segment);
            // Only expand if this child appeared in the parent listing.
            let found = tree_dirs
                .get(&current)
                .map(|c| c.iter().any(|(_, p)| p == &child))
                .unwrap_or(false);
            if !found {
                break;
            }
            let dirs = list_dirs_only_impl(&sftp, &child).await.unwrap_or_default();
            tree_dirs.insert(child.clone(), dirs);
            tree_expanded.insert(child.clone());
            current = child;
        }
    }
    {
        let mut nodes = Vec::new();
        build_tree_nodes("/", 0, &tree_expanded, &tree_dirs, &mut nodes);
        let _ = events.send(SessionEvent::SftpTreeUpdate(nodes));
    }

    // Own every transfer task. Dropping/aborting the worker cancels transfers
    // too, rather than leaving detached writes alive after an MCP cancellation.
    let mut transfers = tokio::task::JoinSet::new();

    // --- Command loop -------------------------------------------------------
    loop {
        let cmd = tokio::select! {
            command = commands.recv() => match command { Some(command) => command, None => break },
            _ = transfers.join_next(), if !transfers.is_empty() => continue,
        };
        match cmd {
            SftpCommand::Close => break,

            SftpCommand::EnrichEntries { path, entries } => {
                let (entries, _) = enrich_entries_with_owners(&handle, &owner_cache, entries).await;
                // Repaint when any name is available — the deferred home listing
                // already painted with numbers/placeholders.
                if entries
                    .iter()
                    .any(|entry| entry.owner.is_some() || entry.group.is_some())
                {
                    let _ = events.send(SessionEvent::SftpEntries { path, entries });
                }
            }

            SftpCommand::ListDir(path) => {
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("加载", "Loading"),
                    path
                )));
                match list_dir_impl(&sftp, &path).await {
                    Ok(entries) => {
                        publish_dir_entries(
                            &handle,
                            &owner_cache,
                            &names_ready_rx,
                            &events,
                            path.clone(),
                            entries,
                        )
                        .await;
                        let _ = events.send(SessionEvent::SftpStatus(path));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpError(list_error_msg(&path, &e)));
                    }
                }
            }

            SftpCommand::RefreshDir(path) => {
                // File panel — same as ListDir.
                lock_or_recover(&owner_cache).reset_probes();
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("加载", "Loading"),
                    path
                )));
                match list_dir_impl(&sftp, &path).await {
                    Ok(entries) => {
                        publish_dir_entries(
                            &handle,
                            &owner_cache,
                            &names_ready_rx,
                            &events,
                            path.clone(),
                            entries,
                        )
                        .await;
                        let _ = events.send(SessionEvent::SftpStatus(path.clone()));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpError(list_error_msg(&path, &e)));
                    }
                }
                // Tree — re-fetch every currently-expanded directory so deleted /
                // created folders sync without a reconnect (#189). Stale entries
                // whose parent no longer lists them are simply never walked by
                // build_tree_nodes, so they drop out on the rebuild.
                let expanded: Vec<String> = tree_expanded.iter().cloned().collect();
                for dir in expanded {
                    let dirs = list_dirs_only_impl(&sftp, &dir).await.unwrap_or_default();
                    tree_dirs.insert(dir, dirs);
                }
                emit_tree(&tree_dirs, &tree_expanded, &events);
            }

            SftpCommand::ToggleTreeNode(path) => {
                if tree_expanded.contains(&path) {
                    // Collapse this node and all descendants.
                    let prefix = format!("{}/", path.trim_end_matches('/'));
                    tree_expanded.retain(|p| p != &path && !p.starts_with(&prefix));
                } else {
                    // Expand: fetch children if not yet cached.
                    if !tree_dirs.contains_key(&path) {
                        let dirs = list_dirs_only_impl(&sftp, &path).await.unwrap_or_default();
                        tree_dirs.insert(path.clone(), dirs);
                    }
                    tree_expanded.insert(path.clone());
                }
                let mut nodes = Vec::new();
                build_tree_nodes("/", 0, &tree_expanded, &tree_dirs, &mut nodes);
                let _ = events.send(SessionEvent::SftpTreeUpdate(nodes));
            }

            SftpCommand::Download {
                remote,
                local_dir,
                conflict,
                policy,
            } => {
                // Run on its own task so the command loop stays free to list /
                // switch directories during the transfer (#116-2).
                let sftp = sftp.clone();
                let handle = handle.clone();
                let events = events.clone();
                let slots = transfer_slots.clone();
                let rate_kbps = queue.rate_limit_kbps;
                let preserve_mtime = queue.preserve_mtime;
                // Register a cancel flag up-front under the file id, so a
                // CancelTransfer arriving mid-download can flip it (#100).
                let file_id = Uuid::new_v4().to_string();
                let cancel = Arc::new(AtomicBool::new(false));
                lock_or_recover(&cancels).insert(file_id.clone(), cancel.clone());
                let cancels_done = cancels.clone();
                transfers.spawn(async move {
                    // Wait for a free transfer slot so at most `concurrency`
                    // transfers run at once; the permit is held for the whole
                    // transfer and released when the task ends.
                    let _permit = match slots.acquire().await {
                        Ok(p) => p,
                        Err(_) => {
                            lock_or_recover(&cancels_done).remove(&file_id);
                            return;
                        }
                    };
                    // A directory target → recursively mirror the whole tree (#50).
                    let is_dir = sftp
                        .metadata(&remote)
                        .await
                        .ok()
                        .map(|m| (m.permissions.unwrap_or(0) & 0o170_000) == 0o040_000)
                        .unwrap_or(false);
                    if is_dir {
                        let dirname = base_name(&remote);
                        // #100.3: an empty folder downloads nothing — just say so
                        // rather than silently creating an empty local directory.
                        let empty = list_dir_impl(&sftp, &remote)
                            .await
                            .map(|e| e.is_empty())
                            .unwrap_or(false);
                        if empty {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("空文件夹", "Empty folder"),
                                dirname
                            )));
                            return;
                        }
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{} {}/...",
                            t("下载文件夹", "Downloading folder"),
                            dirname
                        )));
                        match download_dir(
                            &sftp,
                            &handle,
                            &remote,
                            &local_dir,
                            &events,
                            rate_kbps,
                            preserve_mtime,
                        )
                        .await
                        {
                            Ok(_) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("下载完成", "Downloaded"),
                                    dirname
                                )));
                            }
                            Err(e) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("下载失败", "Download failed")
                                )));
                            }
                        }
                    } else {
                        // Sanitize the server-supplied name before it touches the local
                        // filesystem (#26): a malicious server could otherwise craft a
                        // name with traversal, shell-special chars or a Windows reserved
                        // device name to write outside the chosen dir or hit a device.
                        let filename = sanitize_filename(&base_name(&remote));
                        let requested = download_target_path(&remote, &local_dir);
                        // The caller's existence check happened earlier; a file
                        // can appear in between (another tab, another program).
                        // `Absent` means "nothing there when we looked", so the
                        // configured policy decides what to do now instead of
                        // silently truncating something the user never saw.
                        let local_path = match conflict {
                            DownloadConflict::Absent if requested.is_file() => match policy {
                                DedupPolicy::Overwrite => requested,
                                DedupPolicy::Skip => {
                                    let _ = events.send(SessionEvent::SftpStatus(format!(
                                        "{}: {}",
                                        t(
                                            "已跳过（目标已存在）",
                                            "Skipped (target already exists)"
                                        ),
                                        filename
                                    )));
                                    lock_or_recover(&cancels_done).remove(&file_id);
                                    return;
                                }
                                // Rename and Ask both keep both copies: the
                                // worker cannot prompt, and silently overwriting
                                // is the one outcome it must not choose.
                                _ => available_download_path(&requested),
                            },
                            DownloadConflict::Absent => requested,
                            DownloadConflict::Replace => requested,
                            DownloadConflict::KeepBoth => available_download_path(&requested),
                            DownloadConflict::Fail => requested,
                        };
                        let local_path_text = local_path.to_string_lossy().to_string();
                        let id = file_id.clone();
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{} {}...",
                            t("下载", "Downloading"),
                            filename
                        )));
                        match download_impl(
                            &handle,
                            &remote,
                            &local_path_text,
                            &filename,
                            &id,
                            &events,
                            &cancel,
                            rate_kbps,
                            preserve_mtime,
                            conflict == DownloadConflict::Fail,
                        )
                        .await
                        {
                            Ok(true) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("下载完成", "Downloaded"),
                                    filename
                                )));
                            }
                            Ok(false) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("已取消", "Cancelled"),
                                    filename
                                )));
                            }
                            Err(e) => {
                                emit_transfer(
                                    &events,
                                    &id,
                                    &filename,
                                    false,
                                    0,
                                    0,
                                    2,
                                    &e.to_string(),
                                );
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("下载失败", "Download failed")
                                )));
                            }
                        }
                    }
                    // Let a dual-pane local browser refresh if it shows this dir.
                    let _ = events.send(SessionEvent::LocalFilesChanged {
                        path: local_dir.clone(),
                    });
                    lock_or_recover(&cancels_done).remove(&file_id);
                });
            }

            SftpCommand::DownloadInto {
                remotes,
                local_dir,
                conflict,
            } => {
                // Dual-pane "← 下载": write every selected entry into the local
                // folder the pane is showing, one after another (no remote tar —
                // the user expects plain files, not an archive).
                let sftp = sftp.clone();
                let handle = handle.clone();
                let events = events.clone();
                let cancels_done = cancels.clone();
                let rate_kbps = queue.rate_limit_kbps;
                let preserve_mtime = queue.preserve_mtime;
                transfers.spawn(async move {
                    let total = remotes.len();
                    let mut ok = 0usize;
                    let mut failed = 0usize;
                    for remote in &remotes {
                        let id = Uuid::new_v4().to_string();
                        let cancel = Arc::new(AtomicBool::new(false));
                        cancels_done
                            .lock()
                            .unwrap()
                            .insert(id.clone(), cancel.clone());
                        let name = base_name(remote);
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{} {}...",
                            t("下载", "Downloading"),
                            name
                        )));
                        match download_one_into(
                            &sftp,
                            &handle,
                            remote,
                            &local_dir,
                            conflict,
                            &events,
                            &cancel,
                            &id,
                            rate_kbps,
                            preserve_mtime,
                        )
                        .await
                        {
                            Ok(true) => {
                                ok += 1;
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("下载完成", "Downloaded"),
                                    name
                                )));
                            }
                            Ok(false) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("已取消", "Cancelled"),
                                    name
                                )));
                            }
                            Err(e) => {
                                failed += 1;
                                emit_transfer(&events, &id, &name, false, 0, 0, 2, &e.to_string());
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("下载失败", "Download failed")
                                )));
                            }
                        }
                        cancels_done.lock().unwrap().remove(&id);
                    }
                    if total > 1 {
                        let _ = events.send(SessionEvent::SftpStatus(if failed == 0 {
                            format!(
                                "{}: {} {}",
                                t("下载完成", "Downloaded"),
                                ok,
                                t("项", "items")
                            )
                        } else {
                            format!(
                                "{}: {}/{} {}",
                                t("下载失败", "Download failed"),
                                failed,
                                total,
                                t("项", "items")
                            )
                        }));
                    }
                    // Let the local pane pick up the new files.
                    let _ = events.send(SessionEvent::LocalFilesChanged {
                        path: local_dir.clone(),
                    });
                });
            }

            SftpCommand::DownloadArchive {
                remote_dir,
                names,
                local_dir,
            } => {
                // #100: multi-select download. Instead of N concurrent transfers
                // (which raced and dropped files), tar everything into ONE archive
                // on the remote, pull that single file, then delete the temp.
                let sftp = sftp.clone();
                let handle = handle.clone();
                let events = events.clone();
                let slots = transfer_slots.clone();
                let rate_kbps = queue.rate_limit_kbps;
                let preserve_mtime = queue.preserve_mtime;
                // Register a cancel flag up-front so CancelTransfer can flip it (#100).
                let id = Uuid::new_v4().to_string();
                let cancel = Arc::new(AtomicBool::new(false));
                lock_or_recover(&cancels).insert(id.clone(), cancel.clone());
                let cancels_done = cancels.clone();
                transfers.spawn(async move {
                    // The archive is one transfer: it holds a slot like any
                    // other, so a big multi-select can't bypass the cap.
                    let _permit = match slots.acquire().await {
                        Ok(p) => p,
                        Err(_) => {
                            lock_or_recover(&cancels_done).remove(&id);
                            return;
                        }
                    };
                    let n = names.len();
                    let tmp = format!("/tmp/meatshell-{}.tar", Uuid::new_v4());
                    // Name the archive after the first item's stem, per the user:
                    // 11.txt → "11等文件.tar". Sanitize since names come from the server.
                    let first = names.first().map(|s| s.as_str()).unwrap_or("download");
                    let stem = first
                        .rsplit_once('.')
                        .map(|(a, _)| a)
                        .filter(|a| !a.is_empty())
                        .unwrap_or(first);
                    let arc_name =
                        sanitize_filename(&format!("{}{}.tar", stem, t("等文件", "-and-more")));
                    let local_path = format!("{}/{}", local_dir.trim_end_matches('/'), arc_name);
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{} {} {}...",
                        t("打包下载", "Archiving"),
                        n,
                        t("项", "items")
                    )));
                    // Show a "preparing" row in the transfer panel right away so a
                    // big selection isn't a silent wait while tar runs (#100). The
                    // download then reuses this same id, so the row turns into the
                    // live progress bar once bytes start flowing.
                    emit_transfer(&events, &id, &arc_name, false, 0, 0, 3, "");
                    // Plain tar (no gzip): the user prefers speed over a smaller file.
                    // Server-supplied names are untrusted → quote every argument.
                    // Preserve symlinks instead of dereferencing them. Following a
                    // server-controlled link could archive files outside the selected
                    // directory tree (for example, a link into /etc).
                    let mut cmd =
                        format!("tar -cf {} -C {}", sh_quote(&tmp), sh_quote(&remote_dir));
                    for nm in &names {
                        cmd.push(' ');
                        cmd.push_str(&sh_quote(nm));
                    }
                    let _ = &sftp; // listing session kept alive; transfer uses `handle`
                    let res: Result<bool> = async {
                        let st = handle
                            .elevation
                            .exec_shell(&handle.handle, &cmd, super::common::SCP_LONG_CMD_IO_TIMEOUT)
                            .await
                            .context("tar on remote")?;
                        if st != 0 {
                            return Err(anyhow!(t("远端 tar 打包失败", "remote tar failed")));
                        }
                        download_impl(
                            &handle,
                            &tmp,
                            &local_path,
                            &arc_name,
                            &id,
                            &events,
                            &cancel,
                            rate_kbps,
                            preserve_mtime,
                            false,
                        )
                        .await
                    }
                    .await;
                    // Best-effort cleanup of the remote temp tar — success, failure
                    // or cancel all reach here, so no junk is left on the server (#100).
                    let _ = handle
                        .elevation
                        .exec_shell(&handle.handle, &format!("rm -f {}", sh_quote(&tmp)), super::common::SCP_CMD_IO_TIMEOUT)
                        .await;
                    match res {
                        Ok(true) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("下载完成", "Downloaded"),
                                arc_name
                            )));
                        }
                        Ok(false) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("已取消", "Cancelled"),
                                arc_name
                            )));
                        }
                        Err(e) => {
                            emit_transfer(&events, &id, &arc_name, false, 0, 0, 2, &e.to_string());
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {e}",
                                t("下载失败", "Download failed")
                            )));
                        }
                    }
                    // The archive landed in `local_dir`; refresh a local pane
                    // showing that folder (dual-pane view).
                    let _ = events.send(SessionEvent::LocalFilesChanged {
                        path: local_dir.clone(),
                    });
                    lock_or_recover(&cancels_done).remove(&id);
                });
            }

            SftpCommand::CancelTransfer(id) => {
                if let Some(flag) = lock_or_recover(&cancels).get(&id) {
                    flag.store(true, Ordering::Relaxed);
                }
            }

            SftpCommand::Upload {
                local,
                remote_dir,
                cleanup_after,
                decision,
            } => {
                // Run on its own task so the command loop stays free to list /
                // switch directories during the transfer (#116-2).
                let sftp = sftp.clone();
                let handle = handle.clone();
                let events = events.clone();
                let slots = transfer_slots.clone();
                let rate_kbps = queue.rate_limit_kbps;
                let preserve_mtime = queue.preserve_mtime;
                let dedup = queue.dedup;
                let owner_cache = owner_cache.clone();
                let names_ready_rx = names_ready_rx.clone();
                // Register a cancel flag up-front under the file id so a
                // CancelTransfer arriving mid-upload can flip it (#100).
                let up_id = Uuid::new_v4().to_string();
                let cancel = Arc::new(AtomicBool::new(false));
                lock_or_recover(&cancels).insert(up_id.clone(), cancel.clone());
                let cancels_done = cancels.clone();
                transfers.spawn(async move {
                    // Cap concurrent transfers via the shared semaphore.
                    let _permit = match slots.acquire().await {
                        Ok(p) => p,
                        Err(_) => {
                            lock_or_recover(&cancels_done).remove(&up_id);
                            return;
                        }
                    };
                    // A directory source → recursively upload the whole tree (#50).
                    let is_dir = tokio::fs::metadata(&local)
                        .await
                        .map(|m| m.is_dir())
                        .unwrap_or(false);
                    if is_dir {
                        // Folder uploads merge into the remote tree; the
                        // duplicate policy applies to single files only
                        // (renaming a whole tree mid-mirror would be surprising).
                        let dirname = match local_file_name_utf8(&local) {
                            Ok(name) => name,
                            Err(e) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("上传失败", "Upload failed")
                                )));
                                if let Some(path) = cleanup_after.as_deref() {
                                    cleanup_import_path(path).await;
                                }
                                lock_or_recover(&cancels_done).remove(&up_id);
                                return;
                            }
                        };
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{} {}/...",
                            t("上传文件夹", "Uploading folder"),
                            dirname
                        )));
                        // Folder uploads keep their name (the duplicate policy
                        // applies to single files only; renaming a whole tree
                        // mid-mirror would be surprising). The "existed" marker
                        // is left off since a merge is not an overwrite.
                        let remote_name = dirname.clone();
                        let existed = false;

                        let res = upload_dir(
                            &handle,
                            &sftp,
                            &local,
                            &remote_dir,
                            &remote_name,
                            &events,
                            rate_kbps,
                            preserve_mtime,
                        )
                        .await;
                        if let Ok(entries) = list_dir_impl(&sftp, &remote_dir).await {
                            publish_dir_entries(
                                &handle,
                                &owner_cache,
                                &names_ready_rx,
                                &events,
                                remote_dir.clone(),
                                entries,
                            )
                            .await;
                        }
                        match res {
                            Ok(_) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}{}",
                                    t("上传完成", "Uploaded"),
                                    remote_name,
                                    if existed {
                                        t("（已覆盖）", " (overwritten)")
                                    } else {
                                        ""
                                    }
                                )));
                            }
                            Err(e) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("上传失败", "Upload failed")
                                )));
                                if let Some(path) = cleanup_after.as_deref() {
                                    cleanup_import_path(path).await;
                                }
                                lock_or_recover(&cancels_done).remove(&up_id);
                                return;
                            }
                        }
                    } else {
                        let filename = match local_file_name_utf8(&local) {
                            Ok(name) => name,
                            Err(e) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("上传失败", "Upload failed")
                                )));
                                if let Some(path) = cleanup_after.as_deref() {
                                    cleanup_import_path(path).await;
                                }
                                lock_or_recover(&cancels_done).remove(&up_id);
                                return;
                            }
                        };
                        let requested =
                            format!("{}/{}", remote_dir.trim_end_matches('/'), filename);
                        // Resolve the duplicate-target policy against the live
                        // server state before anything is truncated.
                        let (remote_path, no_clobber) = match resolve_upload_target(
                            &sftp, requested, decision, dedup, &events, &filename,
                        )
                        .await
                        {
                            Some((path, no_clobber)) => (path, no_clobber),
                            None => {
                                if let Some(path) = cleanup_after.as_deref() {
                                    cleanup_import_path(path).await;
                                }
                                lock_or_recover(&cancels_done).remove(&up_id);
                                return;
                            }
                        };
                        let filename = base_name(&remote_path);
                        let remote_name = filename.clone();
                        let id = up_id.clone();
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{} {}...",
                            t("上传", "Uploading"),
                            remote_name
                        )));
                        match upload_pipelined(
                            &handle,
                            &sftp,
                            &local,
                            &remote_path,
                            &remote_name,
                            &id,
                            &events,
                            &cancel,
                            rate_kbps,
                            preserve_mtime,
                            no_clobber,
                        )
                        .await
                        {
                            Ok(true) => {
                                if let Ok(entries) = list_dir_impl(&sftp, &remote_dir).await {
                                    publish_dir_entries(
                                        &handle,
                                        &owner_cache,
                                        &names_ready_rx,
                                        &events,
                                        remote_dir.clone(),
                                        entries,
                                    )
                                    .await;
                                }
                                // The overwrite marker: resolve_upload_target
                                // returns no_clobber=true only when the name was
                                // kept (renamed / skipped), so a plain replace is
                                // the "overwritten" case.
                                let existed = !no_clobber;
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}{}",
                                    t("上传完成", "Uploaded"),
                                    remote_name,
                                    if existed {
                                        t("（已覆盖）", " (overwritten)")
                                    } else {
                                        ""
                                    }
                                )));
                            }
                            Ok(false) => {
                                // Refresh the listing so the removed partial file disappears.
                                if let Ok(entries) = list_dir_impl(&sftp, &remote_dir).await {
                                    publish_dir_entries(
                                        &handle,
                                        &owner_cache,
                                        &names_ready_rx,
                                        &events,
                                        remote_dir.clone(),
                                        entries,
                                    )
                                    .await;
                                }
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("已取消", "Cancelled"),
                                    filename
                                )));
                            }
                            Err(e) => {
                                emit_transfer(
                                    &events,
                                    &id,
                                    &filename,
                                    true,
                                    0,
                                    0,
                                    2,
                                    &e.to_string(),
                                );
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("上传失败", "Upload failed")
                                )));
                            }
                        }
                    }
                    if let Some(path) = cleanup_after.as_deref() {
                        cleanup_import_path(path).await;
                    }
                    lock_or_recover(&cancels_done).remove(&up_id);
                });
            }

            SftpCommand::UploadEdited { local, remote } => {
                let sftp = sftp.clone();
                let handle = handle.clone();
                let events = events.clone();
                let slots = transfer_slots.clone();
                let rate_kbps = queue.rate_limit_kbps;
                let preserve_mtime = queue.preserve_mtime;
                let owner_cache = owner_cache.clone();
                let names_ready_rx = names_ready_rx.clone();
                transfers.spawn(async move {
                    // Re-uploading an edited file is a transfer too: it waits
                    // for a slot like everything else.
                    let _permit = match slots.acquire().await {
                        Ok(p) => p,
                        Err(_) => return,
                    };
                    let filename = base_name(&remote);
                    let remote_dir = parent_dir(&remote);
                    let id = Uuid::new_v4().to_string();
                    let no_cancel = Arc::new(AtomicBool::new(false));
                    let result = upload_pipelined(
                        &handle,
                        &sftp,
                        &local,
                        &remote,
                        &filename,
                        &id,
                        &events,
                        &no_cancel,
                        rate_kbps,
                        preserve_mtime,
                        false,
                    )
                    .await;
                    match result {
                        Ok(true) => {
                            if let Ok(entries) = list_dir_impl(&sftp, &remote_dir).await {
                                publish_dir_entries(
                                    &handle,
                                    &owner_cache,
                                    &names_ready_rx,
                                    &events,
                                    remote_dir,
                                    entries,
                                )
                                .await;
                            }
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("已上传修改", "Re-uploaded changes"),
                                filename
                            )));
                        }
                        Ok(false) => {}
                        Err(e) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {e}",
                                t("上传修改失败", "Failed to re-upload changes")
                            )));
                        }
                    }
                });
            }

            SftpCommand::CopyTo {
                remotes,
                target,
                target_dir,
            } => {
                let sftp = sftp.clone();
                let handle = handle.clone();
                let events = events.clone();
                transfers.spawn(async move {
                    let label = format!("{} {}", remotes.len(), t("项", "items"));
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{} {}...",
                        t("复制到其他会话", "Copying to another session"),
                        label
                    )));
                    for remote in remotes {
                        match stage_remote_for_copy(&sftp, &handle, &remote, &events).await {
                            Ok((local, cleanup_root)) => {
                                let _ = target.send(SftpCommand::Upload {
                                    local,
                                    remote_dir: target_dir.clone(),
                                    cleanup_after: Some(cleanup_root),
                                    // The target session's worker applies its own
                                    // queue policy to the staged copy.
                                    decision: UploadDecision::Proceed,
                                });
                            }
                            Err(e) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("复制失败", "Copy failed")
                                )));
                            }
                        }
                    }
                });
            }

            SftpCommand::Delete(path) => {
                let filename = base_name(&path);
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("删除", "Deleting"),
                    filename
                )));
                // Directories are removed recursively (a plain remove_dir only
                // works on an empty dir, so an uploaded folder couldn't be
                // deleted); files via remove_file.
                let is_dir = sftp
                    .metadata(&path)
                    .await
                    .ok()
                    .map(|m| (m.permissions.unwrap_or(0) & 0o170_000) == 0o040_000)
                    .unwrap_or(false);
                let res: Result<()> = if is_dir {
                    remove_dir_recursive(&sftp, &path).await
                } else {
                    sftp.remove_file(&path)
                        .await
                        .map(|_| ())
                        .map_err(|e| anyhow::anyhow!("{e}"))
                };
                match res {
                    Ok(_) => {
                        let parent = parent_dir(&path);
                        if let Ok(entries) = list_dir_impl(&sftp, &parent).await {
                            publish_dir_entries(
                                &handle,
                                &owner_cache,
                                &names_ready_rx,
                                &events,
                                parent.clone(),
                                entries,
                            )
                            .await;
                        }
                        // Keep the left directory tree in sync (#189): drop the
                        // deleted folder and any cached descendants, then re-list
                        // the parent's sub-dirs so the deleted node disappears
                        // without needing a reconnect.
                        let prefix = format!("{}/", path.trim_end_matches('/'));
                        tree_dirs.retain(|p, _| p != &path && !p.starts_with(&prefix));
                        tree_expanded.retain(|p| p != &path && !p.starts_with(&prefix));
                        sync_tree_dir(&sftp, &parent, &mut tree_dirs).await;
                        emit_tree(&tree_dirs, &tree_expanded, &events);
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("已删除", "Deleted"),
                            filename
                        )));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("删除失败", "Delete failed")
                        )));
                    }
                }
            }

            SftpCommand::Rename { from, to } => {
                let refresh = parent_dir(&from);
                match sftp.rename(&from, &to).await {
                    Ok(_) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("已重命名", "Renamed"),
                            base_name(&to)
                        )));
                        // Sync the left tree (#189): drop the old name + cached
                        // descendants, then re-list both the source and the
                        // destination parent (rename can also move across dirs).
                        let prefix = format!("{}/", from.trim_end_matches('/'));
                        tree_dirs.retain(|p, _| p != &from && !p.starts_with(&prefix));
                        tree_expanded.retain(|p| p != &from && !p.starts_with(&prefix));
                        sync_tree_dir(&sftp, &refresh, &mut tree_dirs).await;
                        let to_parent = parent_dir(&to);
                        if to_parent != refresh {
                            sync_tree_dir(&sftp, &to_parent, &mut tree_dirs).await;
                        }
                        emit_tree(&tree_dirs, &tree_expanded, &events);
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("重命名失败", "Rename failed")
                        )));
                    }
                }
                if let Ok(entries) = list_dir_impl(&sftp, &refresh).await {
                    publish_dir_entries(
                        &handle,
                        &owner_cache,
                        &names_ready_rx,
                        &events,
                        refresh,
                        entries,
                    )
                    .await;
                }
            }

            SftpCommand::Chmod { path, mode } => {
                let refresh = parent_dir(&path);
                let attrs = chmod_attributes(mode);
                match sftp.set_metadata(&path, attrs).await {
                    Ok(_) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {} → {:o}",
                            t("已修改权限", "Permissions changed"),
                            base_name(&path),
                            mode
                        )));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("修改权限失败", "chmod failed")
                        )));
                    }
                }
                if let Ok(entries) = list_dir_impl(&sftp, &refresh).await {
                    publish_dir_entries(
                        &handle,
                        &owner_cache,
                        &names_ready_rx,
                        &events,
                        refresh,
                        entries,
                    )
                    .await;
                }
            }

            SftpCommand::MkDir(path) => {
                let refresh = parent_dir(&path);
                match sftp.create_dir(&path).await {
                    Ok(_) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("已新建文件夹", "Folder created"),
                            base_name(&path)
                        )));
                        // Show the new folder in the left tree too (#189).
                        sync_tree_dir(&sftp, &refresh, &mut tree_dirs).await;
                        emit_tree(&tree_dirs, &tree_expanded, &events);
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("新建文件夹失败", "Create folder failed")
                        )));
                    }
                }
                if let Ok(entries) = list_dir_impl(&sftp, &refresh).await {
                    publish_dir_entries(
                        &handle,
                        &owner_cache,
                        &names_ready_rx,
                        &events,
                        refresh,
                        entries,
                    )
                    .await;
                }
            }

            SftpCommand::TouchFile(path) => {
                let refresh = parent_dir(&path);
                // create() truncates if the file exists, so refuse to clobber.
                let exists = sftp.metadata(&path).await.is_ok();
                if exists {
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{}: {}",
                        t("文件已存在", "File already exists"),
                        base_name(&path)
                    )));
                } else {
                    match sftp.create(&path).await {
                        Ok(_) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("已新建文件", "File created"),
                                base_name(&path)
                            )));
                        }
                        Err(e) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {e}",
                                t("新建文件失败", "Create file failed")
                            )));
                        }
                    }
                }
                if let Ok(entries) = list_dir_impl(&sftp, &refresh).await {
                    publish_dir_entries(
                        &handle,
                        &owner_cache,
                        &names_ready_rx,
                        &events,
                        refresh,
                        entries,
                    )
                    .await;
                }
            }

            SftpCommand::OpenTemp { remote, edit } => {
                // Runs inline on the command pump rather than as a transfer
                // task: the editor must not open before the bytes are on disk,
                // and the pump is already serialized, so no slot is needed (and
                // taking one here would deadlock the pump against the
                // transfers holding it).
                // Sanitize the remote-controlled name before it becomes a local
                // file path that we later hand to the OS "open" call.
                let filename = sanitize_filename(&base_name(&remote));
                let tmp_dir = external_edit_dir.clone();
                let _ = tokio::fs::create_dir_all(&tmp_dir).await;
                let local = tmp_dir.join(external_edit_local_name(
                    &external_edit_prefix,
                    &filename,
                    &Uuid::new_v4().to_string(),
                ));
                let local_str = local.to_string_lossy().to_string();
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("打开", "Opening"),
                    filename
                )));
                let xid = Uuid::new_v4().to_string();
                let no_cancel = Arc::new(AtomicBool::new(false));
                match download_impl(
                    &handle,
                    &remote,
                    &local_str,
                    &filename,
                    &xid,
                    &events,
                    &no_cancel,
                    queue.rate_limit_kbps,
                    queue.preserve_mtime,
                    false,
                )
                .await
                {
                    Ok(_) => {
                        open_with_os(&local_str);
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            if edit {
                                t("已打开编辑", "Opened for editing")
                            } else {
                                t("已打开", "Opened")
                            },
                            filename
                        )));
                        if edit {
                            spawn_edit_watcher(self_tx.clone(), local_str, remote.clone());
                        }
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("打开失败", "Open failed")
                        )));
                    }
                }
            }
            SftpCommand::ReadText { remote, edit } => {
                let name = base_name(&remote);
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("打开", "Opening"),
                    name
                )));
                let (content, error) = match read_text_guarded(&sftp, &remote).await {
                    Ok(text) => (text, String::new()),
                    Err(msg) => (String::new(), msg),
                };
                let _ = events.send(SessionEvent::SftpFileText {
                    path: remote,
                    name,
                    content,
                    edit,
                    error,
                });
            }
            SftpCommand::OpenOrDownload { remote } => {
                let name = base_name(&remote);
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("打开", "Opening"),
                    name
                )));
                // Same checks as "Edit": anything the built-in editor would
                // refuse (binary, not UTF-8, too large, unreadable) is
                // downloaded instead, without an error message.
                match read_text_guarded(&sftp, &remote).await {
                    Ok(content) => {
                        let _ = events.send(SessionEvent::SftpFileText {
                            path: remote,
                            name,
                            content,
                            edit: true,
                            error: String::new(),
                        });
                    }
                    Err(reason) => {
                        tracing::debug!("double-click {remote}: not opened as text ({reason})");
                        let _ = events.send(SessionEvent::SftpDownloadInstead { path: remote });
                    }
                }
            }
            SftpCommand::WriteText { remote, content } => {
                let name = base_name(&remote);
                match write_text_file(&sftp, &remote, &content).await {
                    Ok(_) => {
                        let _ = events.send(SessionEvent::SftpSaveResult {
                            path: remote,
                            ok: true,
                            message: format!("{}: {}", t("已保存", "Saved"), name),
                        });
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpSaveResult {
                            path: remote,
                            ok: false,
                            message: format!("{}: {e:#}", t("保存失败", "Save failed")),
                        });
                    }
                }
            }
            // Drag-out to the OS file manager (#504). Both run on their own
            // task so the panel keeps listing while Explorer copies.
            SftpCommand::WalkForExport { roots, reply } => {
                let sftp = sftp.clone();
                transfers.spawn(async move {
                    let result = walk_for_export(&sftp, &roots)
                        .await
                        .map_err(|e| format!("{e:#}"));
                    let _ = reply.send(result);
                });
            }
            SftpCommand::StreamFile {
                remote,
                chunks,
                progress,
            } => {
                // One Transfers row per drag; its Cancel button flips this flag.
                cancels
                    .lock()
                    .unwrap()
                    .entry(progress.id.clone())
                    .or_insert_with(|| progress.cancel.clone());
                progress.emit_progress(&events, true);
                let handle = handle.clone();
                let events = events.clone();
                transfers.spawn(async move {
                    if let Err(e) =
                        stream_file_impl(&handle, &remote, &chunks, &progress, &events).await
                    {
                        let message = format!("{e:#}");
                        if !progress.cancel.load(Ordering::Relaxed) {
                            progress.fail(format!("{}: {message}", base_name(&remote)));
                        }
                        let _ = chunks.send(Err(message)).await;
                    }
                });
            }
            SftpCommand::FinishDragTransfer(progress) => {
                lock_or_recover(&cancels).remove(&progress.id);
                progress.emit_final(&events);
            }
        }
    }

    transfers.abort_all();
    while transfers.join_next().await.is_some() {}
    crate::ssh::disconnect_ssh(&handle, "bye").await;
    Ok(())
}

/// Does this SFTP-handshake failure look like "the server has no `sftp`
/// subsystem" (rather than a network problem we must not re-route)? A missing
/// sftp-server (Dropbear et al) shows up as a refused subsystem request — our
/// own "request sftp subsystem" context — or a channel that opens then dies
/// mid-handshake. Transport resets and timeouts are NOT proof of a missing
/// subsystem: re-probing SCP on a possibly-dead connection would only stall,
/// so anything else must surface as-is.
fn scp_unavailable_like(err: &anyhow::Error) -> bool {
    let chain = err
        .chain()
        .map(|e| e.to_string().to_lowercase())
        .collect::<Vec<_>>()
        .join(" | ");
    ["request sftp subsystem", "sftp handshake"]
        .iter()
        .any(|k| chain.contains(k))
}

// ---------------------------------------------------------------------------
// "Server has no SFTP subsystem" verdict cache
// ---------------------------------------------------------------------------
// OpenWrt/Dropbear refuses the `sftp` subsystem on every connection. Once the
// panel has switched to SCP mode for a host, the next connection skips the
// doomed subsystem attempt entirely and goes straight to the SCP probe+listing,
// so a reopened panel after a reconnect is not held up by a stall the host is
// guaranteed to repeat. The verdict expires after a TTL so a host that later
// gains openssh-sftp-server is re-probed automatically; a manual directory
// refresh also clears it (an explicit fresh look), and the SCP probe returning
// 127 (scp gone) clears it and retries the subsystem path.
//
// The TTL bounds the cost of the aggressive 2 s probe budget in
// [`SFTP_SUBSYSTEM_PROBE_TIMEOUT`]: a slow server misclassified as "no SFTP
// subsystem" recovers on a later reconnect instead of staying on SCP for the
// whole session.

/// Verdict lifetime before a reconnect re-probes the subsystem.
const SCP_FALLBACK_TTL: Duration = Duration::from_secs(30 * 60);

/// One verdict per server, keyed by target host:port:user, stamped with the
/// moment it was recorded so it can expire.
static SCP_FALLBACK_VERDICT: std::sync::OnceLock<Mutex<HashMap<String, Instant>>> =
    std::sync::OnceLock::new();

fn scp_fallback_verdict() -> &'static Mutex<HashMap<String, Instant>> {
    SCP_FALLBACK_VERDICT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn scp_fallback_key(host: &str, port: u16, user: &str) -> String {
    format!("{host}:{port}/{user}")
}

fn scp_fallback_known(key: &str) -> bool {
    let mut verdicts = lock_or_recover(scp_fallback_verdict());
    let Some(recorded) = verdicts.get(key) else {
        return false;
    };
    if recorded.elapsed() >= SCP_FALLBACK_TTL {
        // Expired: treat as no verdict, so the next connect re-probes. Drop it
        // here rather than on every `contains` so the map does not grow stale.
        verdicts.remove(key);
        return false;
    }
    true
}

fn scp_fallback_mark(key: &str) {
    lock_or_recover(scp_fallback_verdict()).insert(key.to_string(), Instant::now());
}

fn scp_fallback_clear(key: &str) {
    lock_or_recover(scp_fallback_verdict()).remove(key);
}

/// Budget for one SFTP subsystem probe stage. A healthy server answers in
/// milliseconds; a stalled request (a Dropbear that never replies) must fail
/// fast so the panel can fall back to SCP instead of parking the user on the
/// generic 15-second stage budget. 2 s is a compromise: a plain-subsystem
/// server that genuinely needs longer (a slow link plus a slow sftp-server
/// spawn) falls back to SCP, which is still a usable browse mode — the
/// fallback verdict cache then lets a manual refresh re-probe the subsystem.
const SFTP_SUBSYSTEM_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Open the `sftp` subsystem on `handle` and complete the SFTP handshake, with
/// a per-stage budget so a silent refusal surfaces quickly enough to fall back.
async fn try_open_sftp(
    handle: &client::Handle<SftpClientHandler>,
    elevation: &Elevation,
    session: &Session,
    budget: Duration,
) -> Result<SftpSession> {
    let stream = crate::ssh::network_stage_bounded(
        &format!("open SFTP channel at {}:{}", session.host, session.port),
        elevation.open_stream(handle),
        budget,
    )
    .await?;
    crate::ssh::network_stage_bounded(
        &format!("SFTP handshake at {}:{}", session.host, session.port),
        SftpSession::new(stream),
        budget,
    )
    .await
    .map_err(|err| {
        if elevation.is_custom() {
            tracing::warn!("sftp handshake over custom server failed: {err:#}");
            elevation.handshake_error()
        } else {
            err
        }
    })
}

/// How the one-shot SCP probe+listing command turned out.
enum ScpProbeOutcome {
    /// `status == 0`: `scp` exists and the home+listing output is usable
    /// (possibly degraded to `/` + empty by a missing sentinel).
    /// `status != 0`: `scp` is absent (`command -v` → 127) or the shell itself
    /// could not finish the command — treated the same so the caller keeps the
    /// original subsystem error rather than guessing at a listing that was
    /// never produced.
    NoScp,
    /// The exec channel itself failed (transport), which is not a verdict.
    ProbeFailed(anyhow::Error),
}

/// Run the combined `command -v scp` probe + home + first listing in one exec,
/// so the fallback decision costs no extra round-trip on top of the listing.
async fn scp_probe_home_and_list(
    handle: &client::Handle<SftpClientHandler>,
) -> std::result::Result<(String, bool, Vec<RemoteEntry>), ScpProbeOutcome> {
    let (status, output) = exec_remote_output(
        handle,
        &super::common::scp_probe_home_and_list_command(),
        super::common::SCP_LONG_CMD_IO_TIMEOUT,
    )
    .await
    .map_err(ScpProbeOutcome::ProbeFailed)?;
    if status == 0 {
        Ok(super::common::parse_home_and_listing(&output))
    } else {
        Err(ScpProbeOutcome::NoScp)
    }
}

/// Is `scp` available on the server? Runs on the already-authenticated
/// connection so it costs one round-trip, only after a handshake failure.
async fn probe_scp(handle: &client::Handle<SftpClientHandler>) -> Result<Option<()>> {
    let (status, _) = exec_remote_output(handle, "command -v scp", super::common::SCP_CMD_IO_TIMEOUT).await?;
    Ok((status == 0).then_some(()))
}

/// SCP fallback loop, entered when the server has no `sftp` subsystem but
/// does have `scp` (the OpenWrt/Dropbear case).
///
/// Mirrors `run_sftp`: initial listing, directory-tree init, the browse
/// commands (`ListDir` / `RefreshDir` / `ToggleTreeNode`), the transfer
/// commands (`Download` / `DownloadInto` / `Upload` / `UploadEdited` /
/// `ReadText` / `WriteText` / `CancelTransfer`) over the wire-protocol
/// engine in `super::scp`, and the file-management commands (`Delete` /
/// `Rename` / `Chmod` / `MkDir` / `TouchFile` / `OpenOrDownload` / `CopyTo`)
/// as remote shell commands through the same privileges as the SFTP channel.
/// Remaining operations (external open, drag-out, archives) are still refused
/// with a "not supported in SCP mode" status.
async fn run_scp_browse(
    handle: Arc<SftpLink>,
    events: UnboundedSender<SessionEvent>,
    mut commands: UnboundedReceiver<SftpCommand>,
    self_tx: UnboundedSender<SftpCommand>,
    queue: SftpQueueConfig,
    prefetched: Option<(String, bool, Vec<RemoteEntry>)>,
    fallback_key: Option<String>,
) -> Result<()> {
    // Owner/group names live for the whole SCP connection, exactly like SFTP.
    let owner_cache: SharedOwnerCache = Arc::new(Mutex::new(OwnerNameCache::default()));
    let (names_ready_tx, names_ready_rx) = tokio::sync::watch::channel(false);
    {
        let handle = handle.clone();
        let owner_cache = owner_cache.clone();
        tokio::spawn(async move {
            prefetch_all_owner_names(&handle.handle, &owner_cache).await;
            let _ = names_ready_tx.send(true);
        });
    }

    // Home + first listing in one exec (`pwd` then `find`/`stat` reusing the
    // captured path — the exec channel starts in the home, so no second
    // round-trip is needed to learn it), with the root-tree probe running
    // concurrently since the two are independent. Startup is two round-trips
    // instead of four. The listing keeps the long bound (a big home on a slow
    // link needs it); the root probe is auxiliary UI and uses the short bound.
    // `prefetched` carries the home+listing already captured by the fallback's
    // merged probe command, so this init is down to the single root probe.
    let init_started = Instant::now();
    let root_probe = scp_list_dirs_only(&handle, "/", super::common::SCP_CMD_IO_TIMEOUT);
    let (list_res, root_res) = match prefetched {
        // Already parsed by the merged probe command: only the root probe runs.
        Some((home, stat_available, entries)) => {
            (Ok::<_, anyhow::Error>((home, stat_available, entries)), root_probe.await)
        }
        None => {
            let list_cmd = super::common::scp_home_and_list_command();
            let (exec_res, root_res) = tokio::join!(
                handle.elevation.exec_shell_output(
                    &handle.handle,
                    &list_cmd,
                    super::common::SCP_LONG_CMD_IO_TIMEOUT,
                ),
                root_probe,
            );
            let parsed = exec_res
                // A failed combined command degrades the same way a failed `pwd`
                // did: stay usable from "/" with an empty listing rather than an
                // error page.
                .map(|(_status, output)| super::common::parse_home_and_listing(&output));
            (parsed, root_res)
        }
    };
    let (home, stat_available, entries) = list_res.unwrap_or_else(|_| ("/".to_string(), false, Vec::new()));
    let _ = events.send(SessionEvent::SftpStatus(format!(
        "{} {}...",
        t("SFTP 加载", "SFTP loading"),
        home
    )));
    // The merged probe+listing command lists through `stat`, which the busybox
    // build may not ship — exactly why `scp_list_dir` falls back to `ls`. The
    // stat availability marker distinguishes a stat-less device (an empty
    // listing is not proof of an empty home, so re-list once via `scp_list_dir`
    // and its `ls` fallback) from a genuinely empty home (no extra round-trip).
    // Note: a permission-denied home (stat available but `find` fails, stderr
    // swallowed by `2>/dev/null`) is now treated as empty rather than re-listed
    // via `ls`; that keeps the empty-home fast path free of an extra round-trip.
    // Failure keeps the degraded empty listing rather than blanking the panel,
    // mirroring the non-prefetched init's error handling.
    let entries = if !stat_available {
        match scp_list_dir(&handle, &home).await {
            Ok(entries) => entries,
            Err(err) => {
                tracing::warn!(
                    sftp_mode = "scp",
                    stage = "first_listing_fallback",
                    error = %err,
                    "scp init: stat-free re-list failed"
                );
                Vec::new()
            }
        }
    } else {
        entries
    };
    {
        wait_owner_prefetch(&names_ready_rx, Duration::from_secs(3)).await;
        tracing::debug!(
            sftp_mode = "scp",
            stage = "first_listing",
            elapsed_ms = init_started.elapsed().as_millis() as u64,
            "scp init: home + first listing"
        );
        let mut entries = entries;
        let need = lock_or_recover(&owner_cache).apply(&mut entries);
        let _ = events.send(SessionEvent::SftpEntries {
            path: home.clone(),
            entries: entries.clone(),
        });
        if !need.is_complete() {
            let _ = self_tx.send(SftpCommand::EnrichEntries {
                path: home.clone(),
                entries,
            });
        }
        let _ = events.send(SessionEvent::SftpStatus(home.clone()));
    }

    // Directory-tree init: root "/" subdirs (already probed above), then
    // expand down to home.
    let mut tree_dirs: std::collections::HashMap<String, Vec<(String, String)>> =
        std::collections::HashMap::new();
    let mut tree_expanded: std::collections::HashSet<String> = std::collections::HashSet::new();
    let root_dirs = root_res.unwrap_or_default();
    tree_dirs.insert("/".to_string(), root_dirs);
    tree_expanded.insert("/".to_string());
    if home != "/" {
        let mut current = "/".to_string();
        for segment in home.trim_start_matches('/').split('/') {
            if segment.is_empty() {
                continue;
            }
            let child = format!("{}/{}", current.trim_end_matches('/'), segment);
            let found = tree_dirs
                .get(&current)
                .map(|c| c.iter().any(|(_, p)| p == &child))
                .unwrap_or(false);
            if !found {
                break;
            }
            let dirs = scp_list_dirs_only(&handle, &child, super::common::SCP_CMD_IO_TIMEOUT)
                .await
                .unwrap_or_default();
            tree_dirs.insert(child.clone(), dirs);
            tree_expanded.insert(child.clone());
            current = child;
        }
    }
    {
        let mut nodes = Vec::new();
        build_tree_nodes("/", 0, &tree_expanded, &tree_dirs, &mut nodes);
        let _ = events.send(SessionEvent::SftpTreeUpdate(nodes));
        tracing::debug!(
            sftp_mode = "scp",
            stage = "tree_init",
            elapsed_ms = init_started.elapsed().as_millis() as u64,
            "scp init: tree"
        );
    }

    // Transfers run on their own tasks so the command loop stays free to
    // list / switch directories meanwhile — mirroring run_sftp (#116-2).
    let permits = queue.concurrency.clamp(1, 4) as usize;
    let transfer_slots = Arc::new(tokio::sync::Semaphore::new(permits));
    // Transfer ids → cancel flags; a CancelTransfer command flips one.
    let cancels: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    // Own every transfer task. Dropping/aborting the worker cancels transfers
    // too, rather than leaving detached writes alive after the connection is
    // gone — same ownership rule as run_sftp.
    let mut transfers = tokio::task::JoinSet::new();

    // --- Command loop (browse-only) ----------------------------------------
    loop {
        let cmd = tokio::select! {
            command = commands.recv() => match command { Some(command) => command, None => break },
            _ = transfers.join_next(), if !transfers.is_empty() => continue,
        };
        match cmd {
            SftpCommand::Close => break,

            SftpCommand::EnrichEntries { path, entries } => {
                let (entries, _) =
                    enrich_entries_with_owners(&handle.handle, &owner_cache, entries).await;
                if entries
                    .iter()
                    .any(|e| e.owner.is_some() || e.group.is_some())
                {
                    let _ = events.send(SessionEvent::SftpEntries { path, entries });
                }
            }

            SftpCommand::ListDir(path) => {
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("加载", "Loading"),
                    path
                )));
                // TODO(panel-block): the panel listing runs inline in the
                // command loop, so one stalled remote directory holds up
                // navigation/refresh/cancel for up to its 10-minute bound.
                // Same shape as the SFTP subsystem's list_dir_impl arm; move
                // the listing onto its own task with a stale-result guard.
                match scp_list_dir(&handle, &path).await {
                    Ok(entries) => {
                        publish_dir_entries(
                            &handle.handle,
                            &owner_cache,
                            &names_ready_rx,
                            &events,
                            path.clone(),
                            entries,
                        )
                        .await;
                        let _ = events.send(SessionEvent::SftpStatus(path));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpError(list_error_msg(&path, &e)));
                    }
                }
            }

            SftpCommand::RefreshDir(path) => {
                // A manual refresh is the user asking for a fresh look: clear
                // the no-SFTP verdict so the next connect re-probes the
                // subsystem (a host that gained sftp-server should be found).
                if let Some(key) = &fallback_key {
                    scp_fallback_clear(key);
                }
                lock_or_recover(&owner_cache).reset_probes();
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("加载", "Loading"),
                    path
                )));
                // TODO(panel-block): same inline-listing concern as ListDir —
                // a stalled directory blocks the whole command loop until its
                // 10-minute bound. See the ListDir arm.
                match scp_list_dir(&handle, &path).await {
                    Ok(entries) => {
                        publish_dir_entries(
                            &handle.handle,
                            &owner_cache,
                            &names_ready_rx,
                            &events,
                            path.clone(),
                            entries,
                        )
                        .await;
                        let _ = events.send(SessionEvent::SftpStatus(path.clone()));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpError(list_error_msg(&path, &e)));
                    }
                }
                // Re-fetch every expanded node concurrently, capped at 4 so the
                // SSH session's MaxSessions (default 10) leaves room for the
                // panel listing and transfers, each with the short tree bound.
                // A stalled node degrades to empty rather than serialising the
                // whole refresh behind it; results are independent so order is
                // irrelevant.
                let expanded: Vec<String> = tree_expanded.iter().cloned().collect();
                let mut tree_probes = futures::stream::iter(expanded)
                    .map(|dir| {
                        let handle = handle.clone();
                        async move {
                            let dirs = scp_list_dirs_only(
                                &handle,
                                &dir,
                                super::common::SCP_CMD_IO_TIMEOUT,
                            )
                            .await
                            .unwrap_or_default();
                            (dir, dirs)
                        }
                    })
                    .buffer_unordered(4);
                while let Some((dir, dirs)) = tree_probes.next().await {
                    tree_dirs.insert(dir, dirs);
                }
                emit_tree(&tree_dirs, &tree_expanded, &events);
            }

            SftpCommand::ToggleTreeNode(path) => {
                if tree_expanded.contains(&path) {
                    let prefix = format!("{}/", path.trim_end_matches('/'));
                    tree_expanded.retain(|p| p != &path && !p.starts_with(&prefix));
                } else {
                    if !tree_dirs.contains_key(&path) {
                        let dirs = scp_list_dirs_only(
                            &handle,
                            &path,
                            super::common::SCP_CMD_IO_TIMEOUT,
                        )
                        .await
                        .unwrap_or_default();
                        tree_dirs.insert(path.clone(), dirs);
                    }
                    tree_expanded.insert(path.clone());
                }
                let mut nodes = Vec::new();
                build_tree_nodes("/", 0, &tree_expanded, &tree_dirs, &mut nodes);
                let _ = events.send(SessionEvent::SftpTreeUpdate(nodes));
            }

            SftpCommand::CancelTransfer(id) => {
                if let Some(flag) = lock_or_recover(&cancels).get(&id) {
                    flag.store(true, Ordering::Relaxed);
                }
            }

            SftpCommand::Download {
                remote,
                local_dir,
                conflict,
                policy,
            } => {
                // The SCP twin of the SFTP Download arm: same conflict chain,
                // same rows, same cancel registration (#100, #116-2).
                let handle = handle.clone();
                let events = events.clone();
                let slots = transfer_slots.clone();
                let rate_kbps = queue.rate_limit_kbps;
                let preserve_mtime = queue.preserve_mtime;
                let file_id = Uuid::new_v4().to_string();
                let cancel = Arc::new(AtomicBool::new(false));
                lock_or_recover(&cancels).insert(file_id.clone(), cancel.clone());
                let cancels_done = cancels.clone();
                transfers.spawn(async move {
                    let _permit = match slots.acquire().await {
                        Ok(p) => p,
                        Err(_) => {
                            lock_or_recover(&cancels_done).remove(&file_id);
                            return;
                        }
                    };
                    let is_dir = scp_remote_is_dir(&handle, &remote).await;
                    if is_dir {
                        let dirname = base_name(&remote);
                        // #100.3: an empty folder downloads nothing — just say
                        // so rather than silently creating an empty local dir.
                        let empty = scp_list_dir(&handle, &remote)
                            .await
                            .map(|e| e.is_empty())
                            .unwrap_or(false);
                        if empty {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("空文件夹", "Empty folder"),
                                dirname
                            )));
                            lock_or_recover(&cancels_done).remove(&file_id);
                            return;
                        }
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{} {}/...",
                            t("下载文件夹", "Downloading folder"),
                            dirname
                        )));
                        match scp_download_dir(
                            &handle,
                            &remote,
                            &local_dir,
                            &events,
                            rate_kbps,
                            preserve_mtime,
                            &cancel,
                        )
                        .await
                        {
                            Ok(true) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("下载完成", "Downloaded"),
                                    dirname
                                )));
                            }
                            Ok(false) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("已取消", "Cancelled"),
                                    dirname
                                )));
                            }
                            Err(e) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("下载失败", "Download failed")
                                )));
                            }
                        }
                    } else {
                        // Sanitize the server-supplied name before it touches
                        // the local filesystem (#26) — same rule as SFTP.
                        let filename = sanitize_filename(&base_name(&remote));
                        let requested = download_target_path(&remote, &local_dir);
                        let local_path = match conflict {
                            DownloadConflict::Absent if requested.is_file() => match policy {
                                DedupPolicy::Overwrite => requested,
                                DedupPolicy::Skip => {
                                    let _ = events.send(SessionEvent::SftpStatus(format!(
                                        "{}: {}",
                                        t(
                                            "已跳过（目标已存在）",
                                            "Skipped (target already exists)"
                                        ),
                                        filename
                                    )));
                                    lock_or_recover(&cancels_done).remove(&file_id);
                                    return;
                                }
                                // Rename and Ask both keep both copies: the
                                // worker cannot prompt, and silently
                                // overwriting is the one outcome it must not
                                // choose.
                                _ => available_download_path(&requested),
                            },
                            DownloadConflict::Absent => requested,
                            DownloadConflict::Replace => requested,
                            DownloadConflict::KeepBoth => available_download_path(&requested),
                            DownloadConflict::Fail => requested,
                        };
                        let local_path_text = local_path.to_string_lossy().to_string();
                        let id = file_id.clone();
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{} {}...",
                            t("下载", "Downloading"),
                            filename
                        )));
                        match scp_download_single(
                            &handle,
                            &remote,
                            &local_path_text,
                            &filename,
                            &id,
                            &events,
                            &cancel,
                            rate_kbps,
                            preserve_mtime,
                            conflict == DownloadConflict::Fail,
                        )
                        .await
                        {
                            Ok(true) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("下载完成", "Downloaded"),
                                    filename
                                )));
                            }
                            Ok(false) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("已取消", "Cancelled"),
                                    filename
                                )));
                            }
                            Err(e) => {
                                emit_transfer(
                                    &events,
                                    &id,
                                    &filename,
                                    false,
                                    0,
                                    0,
                                    2,
                                    &e.to_string(),
                                );
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("下载失败", "Download failed")
                                )));
                            }
                        }
                    }
                    // Let a dual-pane local browser refresh if it shows this dir.
                    let _ = events.send(SessionEvent::LocalFilesChanged {
                        path: local_dir.clone(),
                    });
                    lock_or_recover(&cancels_done).remove(&file_id);
                });
            }

            SftpCommand::DownloadInto {
                remotes,
                local_dir,
                conflict,
            } => {
                // Dual-pane "← 下载": one task, entries one after another —
                // the SCP twin of the SFTP DownloadInto arm.
                let handle = handle.clone();
                let events = events.clone();
                let cancels_done = cancels.clone();
                let rate_kbps = queue.rate_limit_kbps;
                let preserve_mtime = queue.preserve_mtime;
                transfers.spawn(async move {
                    let total = remotes.len();
                    let mut ok = 0usize;
                    let mut failed = 0usize;
                    for remote in &remotes {
                        let id = Uuid::new_v4().to_string();
                        let cancel = Arc::new(AtomicBool::new(false));
                        lock_or_recover(&cancels_done).insert(id.clone(), cancel.clone());
                        let name = base_name(remote);
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{} {}...",
                            t("下载", "Downloading"),
                            name
                        )));
                        match scp_download_one_into(
                            &handle,
                            remote,
                            &local_dir,
                            conflict,
                            &events,
                            &cancel,
                            &id,
                            rate_kbps,
                            preserve_mtime,
                        )
                        .await
                        {
                            Ok(true) => {
                                ok += 1;
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("下载完成", "Downloaded"),
                                    name
                                )));
                            }
                            Ok(false) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("已取消", "Cancelled"),
                                    name
                                )));
                            }
                            Err(e) => {
                                failed += 1;
                                emit_transfer(&events, &id, &name, false, 0, 0, 2, &e.to_string());
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("下载失败", "Download failed")
                                )));
                            }
                        }
                        lock_or_recover(&cancels_done).remove(&id);
                    }
                    if total > 1 {
                        let _ = events.send(SessionEvent::SftpStatus(if failed == 0 {
                            format!(
                                "{}: {} {}",
                                t("下载完成", "Downloaded"),
                                ok,
                                t("项", "items")
                            )
                        } else {
                            format!(
                                "{}: {}/{} {}",
                                t("下载失败", "Download failed"),
                                failed,
                                total,
                                t("项", "items")
                            )
                        }));
                    }
                    // Let the local pane pick up the new files.
                    let _ = events.send(SessionEvent::LocalFilesChanged {
                        path: local_dir.clone(),
                    });
                });
            }

            SftpCommand::Upload {
                local,
                remote_dir,
                cleanup_after,
                decision,
            } => {
                // The SCP twin of the SFTP Upload arm. Folder uploads mirror
                // with remote `mkdir -p` plus one temp+`mv` push per file, so
                // a failed or cancelled upload never damages an existing
                // remote file (#100, #116-2).
                let handle = handle.clone();
                let events = events.clone();
                let slots = transfer_slots.clone();
                let rate_kbps = queue.rate_limit_kbps;
                let dedup = queue.dedup;
                let owner_cache = owner_cache.clone();
                let names_ready_rx = names_ready_rx.clone();
                let up_id = Uuid::new_v4().to_string();
                let cancel = Arc::new(AtomicBool::new(false));
                lock_or_recover(&cancels).insert(up_id.clone(), cancel.clone());
                let cancels_done = cancels.clone();
                transfers.spawn(async move {
                    let _permit = match slots.acquire().await {
                        Ok(p) => p,
                        Err(_) => {
                            lock_or_recover(&cancels_done).remove(&up_id);
                            return;
                        }
                    };
                    let is_dir = tokio::fs::metadata(&local)
                        .await
                        .map(|m| m.is_dir())
                        .unwrap_or(false);
                    if is_dir {
                        // Folder uploads merge into the remote tree; the
                        // duplicate policy applies to single files only.
                        let dirname = match local_file_name_utf8(&local) {
                            Ok(name) => name,
                            Err(e) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("上传失败", "Upload failed")
                                )));
                                if let Some(path) = cleanup_after.as_deref() {
                                    cleanup_import_path(path).await;
                                }
                                lock_or_recover(&cancels_done).remove(&up_id);
                                return;
                            }
                        };
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{} {}/...",
                            t("上传文件夹", "Uploading folder"),
                            dirname
                        )));
                        let res = scp_upload_dir(
                            &handle,
                            &local,
                            &remote_dir,
                            &events,
                            rate_kbps,
                            &cancel,
                        )
                        .await;
                        // Refresh the listing whatever happened (mirrors the
                        // SFTP arm: cancelled uploads clean up behind them).
                        if let Ok(entries) = scp_list_dir(&handle, &remote_dir).await {
                            publish_dir_entries(
                                &handle,
                                &owner_cache,
                                &names_ready_rx,
                                &events,
                                remote_dir.clone(),
                                entries,
                            )
                            .await;
                        }
                        match res {
                            Ok((_, failed, false)) if failed == 0 => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("上传完成", "Uploaded"),
                                    dirname
                                )));
                            }
                            Ok((ok, failed, false)) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}/{} {}",
                                    t("上传失败", "Upload failed"),
                                    failed,
                                    ok + failed,
                                    t("项", "items")
                                )));
                            }
                            Ok((_, _, true)) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("已取消", "Cancelled"),
                                    dirname
                                )));
                            }
                            Err(e) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("上传失败", "Upload failed")
                                )));
                            }
                        }
                    } else {
                        let filename = match local_file_name_utf8(&local) {
                            Ok(name) => name,
                            Err(e) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("上传失败", "Upload failed")
                                )));
                                if let Some(path) = cleanup_after.as_deref() {
                                    cleanup_import_path(path).await;
                                }
                                lock_or_recover(&cancels_done).remove(&up_id);
                                return;
                            }
                        };
                        let requested =
                            format!("{}/{}", remote_dir.trim_end_matches('/'), filename);
                        // Resolve the duplicate-target policy against the live
                        // server state before anything is sent.
                        let (remote_path, no_clobber) = match scp_resolve_upload_target(
                            &handle, requested, decision, dedup, &events, &filename,
                        )
                        .await
                        {
                            Some((path, no_clobber)) => (path, no_clobber),
                            None => {
                                if let Some(path) = cleanup_after.as_deref() {
                                    cleanup_import_path(path).await;
                                }
                                lock_or_recover(&cancels_done).remove(&up_id);
                                return;
                            }
                        };
                        let filename = base_name(&remote_path);
                        let id = up_id.clone();
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{} {}...",
                            t("上传", "Uploading"),
                            filename
                        )));
                        match scp_push_file(
                            &handle,
                            &local,
                            &remote_path,
                            &filename,
                            &id,
                            &events,
                            &cancel,
                            rate_kbps,
                            no_clobber,
                        )
                        .await
                        {
                            Ok(true) => {
                                if let Ok(entries) = scp_list_dir(&handle, &remote_dir).await {
                                    publish_dir_entries(
                                        &handle,
                                        &owner_cache,
                                        &names_ready_rx,
                                        &events,
                                        remote_dir.clone(),
                                        entries,
                                    )
                                    .await;
                                }
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("上传完成", "Uploaded"),
                                    filename
                                )));
                            }
                            Ok(false) => {
                                // Refresh so the cleaned-up temp disappears.
                                if let Ok(entries) = scp_list_dir(&handle, &remote_dir).await {
                                    publish_dir_entries(
                                        &handle,
                                        &owner_cache,
                                        &names_ready_rx,
                                        &events,
                                        remote_dir.clone(),
                                        entries,
                                    )
                                    .await;
                                }
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("已取消", "Cancelled"),
                                    filename
                                )));
                            }
                            Err(e) => {
                                emit_transfer(
                                    &events,
                                    &id,
                                    &filename,
                                    true,
                                    0,
                                    0,
                                    2,
                                    &e.to_string(),
                                );
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("上传失败", "Upload failed")
                                )));
                            }
                        }
                    }
                    if let Some(path) = cleanup_after.as_deref() {
                        cleanup_import_path(path).await;
                    }
                    lock_or_recover(&cancels_done).remove(&up_id);
                });
            }

            SftpCommand::UploadEdited { local, remote } => {
                // Re-upload an externally edited file to its exact original
                // path (#318): no dedup, a plain overwrite.
                let handle = handle.clone();
                let events = events.clone();
                let slots = transfer_slots.clone();
                let rate_kbps = queue.rate_limit_kbps;
                let owner_cache = owner_cache.clone();
                let names_ready_rx = names_ready_rx.clone();
                transfers.spawn(async move {
                    let _permit = match slots.acquire().await {
                        Ok(p) => p,
                        Err(_) => return,
                    };
                    let filename = base_name(&remote);
                    let remote_dir = parent_dir(&remote);
                    let id = Uuid::new_v4().to_string();
                    // Folder uploads aren't cancellable from the UI; neither
                    // is this (same throwaway flag as the SFTP arm).
                    let no_cancel = Arc::new(AtomicBool::new(false));
                    match scp_push_file(
                        &handle, &local, &remote, &filename, &id, &events, &no_cancel, rate_kbps,
                        false,
                    )
                    .await
                    {
                        Ok(true) => {
                            if let Ok(entries) = scp_list_dir(&handle, &remote_dir).await {
                                publish_dir_entries(
                                    &handle,
                                    &owner_cache,
                                    &names_ready_rx,
                                    &events,
                                    remote_dir,
                                    entries,
                                )
                                .await;
                            }
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("已上传修改", "Re-uploaded changes"),
                                filename
                            )));
                        }
                        Ok(false) => {}
                        Err(e) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {e}",
                                t("上传修改失败", "Failed to re-upload changes")
                            )));
                        }
                    }
                });
            }

            SftpCommand::ReadText { remote, edit } => {
                let name = base_name(&remote);
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("打开", "Opening"),
                    name
                )));
                let (content, error) = match scp_read_text_guarded(&handle, &remote).await {
                    Ok(text) => (text, String::new()),
                    Err(msg) => (String::new(), msg),
                };
                let _ = events.send(SessionEvent::SftpFileText {
                    path: remote,
                    name,
                    content,
                    edit,
                    error,
                });
            }

            SftpCommand::WriteText { remote, content } => {
                let name = base_name(&remote);
                match scp_write_text(&handle, &remote, &content).await {
                    Ok(_) => {
                        let _ = events.send(SessionEvent::SftpSaveResult {
                            path: remote,
                            ok: true,
                            message: format!("{}: {}", t("已保存", "Saved"), name),
                        });
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpSaveResult {
                            path: remote,
                            ok: false,
                            message: format!("{}: {e:#}", t("保存失败", "Save failed")),
                        });
                    }
                }
            }

            SftpCommand::Delete(path) => {
                // Never issue `rm -rf` for the filesystem root or an empty path:
                // under sudo elevation that would be catastrophic.
                if path.is_empty() || path == "/" {
                    let _ = events.send(SessionEvent::SftpStatus(
                        t(
                            "无法删除文件系统根目录",
                            "cannot delete the filesystem root",
                        )
                        .into(),
                    ));
                    continue;
                }
                let filename = base_name(&path);
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("删除", "Deleting"),
                    filename
                )));
                let cmd = super::common::scp_remove_recursive_command(&path);
                match scp_mgmt_command(&handle, &cmd).await {
                    Ok(()) => {
                        let parent = parent_dir(&path);
                        if let Ok(entries) = scp_list_dir(&handle, &parent).await {
                            publish_dir_entries(
                                &handle,
                                &owner_cache,
                                &names_ready_rx,
                                &events,
                                parent.clone(),
                                entries,
                            )
                            .await;
                        }
                        // Drop the deleted node and its cached descendants, then
                        // re-list the parent so it disappears without a reconnect (#189).
                        let prefix = format!("{}/", path.trim_end_matches('/'));
                        tree_dirs.retain(|p, _| p != &path && !p.starts_with(&prefix));
                        tree_expanded.retain(|p| p != &path && !p.starts_with(&prefix));
                        if tree_dirs.contains_key(&parent) {
                            tree_dirs.insert(
                                parent.clone(),
                                scp_list_dirs_only(
                                    &handle,
                                    &parent,
                                    super::common::SCP_CMD_IO_TIMEOUT,
                                )
                                .await
                                .unwrap_or_default(),
                            );
                        }
                        emit_tree(&tree_dirs, &tree_expanded, &events);
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("已删除", "Deleted"),
                            filename
                        )));
                    }
                    Err(msg) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("删除失败", "Delete failed"),
                            msg
                        )));
                    }
                }
            }

            SftpCommand::Rename { from, to } => {
                let refresh = parent_dir(&from);
                // `mv -f` would silently overwrite an existing target; like the
                // SFTP backend, refuse to clobber (#69).
                let target_taken = scp_remote_exists(&handle.handle, &to)
                    .await
                    .unwrap_or(false);
                if target_taken {
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{}: {}",
                        t(
                            "重命名失败: 目标已存在",
                            "Rename failed: target already exists"
                        ),
                        base_name(&to)
                    )));
                } else {
                    let cmd = super::common::scp_move_command(&from, &to);
                    match scp_mgmt_command(&handle, &cmd).await {
                        Ok(()) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("已重命名", "Renamed"),
                                base_name(&to)
                            )));
                            // Sync the tree: drop the old name and its cached
                            // descendants, then re-list both parents (a rename
                            // can cross directories).
                            let prefix = format!("{}/", from.trim_end_matches('/'));
                            tree_dirs.retain(|p, _| p != &from && !p.starts_with(&prefix));
                            tree_expanded.retain(|p| p != &from && !p.starts_with(&prefix));
                            let to_parent = parent_dir(&to);
                            let mut parents = vec![refresh.clone()];
                            if to_parent != refresh {
                                parents.push(to_parent);
                            }
                            for p in parents {
                                if tree_dirs.contains_key(&p) {
                                    tree_dirs.insert(
                                        p.clone(),
                                        scp_list_dirs_only(
                                            &handle,
                                            &p,
                                            super::common::SCP_CMD_IO_TIMEOUT,
                                        )
                                        .await
                                        .unwrap_or_default(),
                                    );
                                }
                            }
                            emit_tree(&tree_dirs, &tree_expanded, &events);
                        }
                        Err(msg) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("重命名失败", "Rename failed"),
                                msg
                            )));
                        }
                    }
                }
                if let Ok(entries) = scp_list_dir(&handle, &refresh).await {
                    publish_dir_entries(
                        &handle,
                        &owner_cache,
                        &names_ready_rx,
                        &events,
                        refresh,
                        entries,
                    )
                    .await;
                }
            }

            SftpCommand::Chmod { path, mode } => {
                let refresh = parent_dir(&path);
                let cmd = super::common::scp_chmod_command(&path, mode);
                match scp_mgmt_command(&handle, &cmd).await {
                    Ok(()) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {} → {:o}",
                            t("已修改权限", "Permissions changed"),
                            base_name(&path),
                            mode
                        )));
                    }
                    Err(msg) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("修改权限失败", "chmod failed"),
                            msg
                        )));
                    }
                }
                if let Ok(entries) = scp_list_dir(&handle, &refresh).await {
                    publish_dir_entries(
                        &handle,
                        &owner_cache,
                        &names_ready_rx,
                        &events,
                        refresh,
                        entries,
                    )
                    .await;
                }
            }

            SftpCommand::MkDir(path) => {
                let refresh = parent_dir(&path);
                let cmd = super::common::scp_mkdir_command(&path);
                match scp_mgmt_command(&handle, &cmd).await {
                    Ok(()) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("已新建文件夹", "Folder created"),
                            base_name(&path)
                        )));
                        if tree_dirs.contains_key(&refresh) {
                            tree_dirs.insert(
                                refresh.clone(),
                                scp_list_dirs_only(
                                    &handle,
                                    &refresh,
                                    super::common::SCP_CMD_IO_TIMEOUT,
                                )
                                .await
                                .unwrap_or_default(),
                            );
                        }
                        emit_tree(&tree_dirs, &tree_expanded, &events);
                    }
                    Err(msg) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("新建文件夹失败", "Create folder failed"),
                            msg
                        )));
                    }
                }
                if let Ok(entries) = scp_list_dir(&handle, &refresh).await {
                    publish_dir_entries(
                        &handle,
                        &owner_cache,
                        &names_ready_rx,
                        &events,
                        refresh,
                        entries,
                    )
                    .await;
                }
            }

            SftpCommand::TouchFile(path) => {
                let refresh = parent_dir(&path);
                // `touch` never truncates an existing file (safe, unlike the SFTP
                // backend's create()); probe first so the success message is
                // accurate, mirroring the SFTP twin.
                let exists = scp_remote_exists(&handle.handle, &path)
                    .await
                    .unwrap_or(false);
                if exists {
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{}: {}",
                        t("文件已存在", "File already exists"),
                        base_name(&path)
                    )));
                } else {
                    let cmd = super::common::scp_touch_command(&path);
                    match scp_mgmt_command(&handle, &cmd).await {
                        Ok(()) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("已新建文件", "File created"),
                                base_name(&path)
                            )));
                        }
                        Err(msg) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("新建文件失败", "Create file failed"),
                                msg
                            )));
                        }
                    }
                }
                if let Ok(entries) = scp_list_dir(&handle, &refresh).await {
                    publish_dir_entries(
                        &handle,
                        &owner_cache,
                        &names_ready_rx,
                        &events,
                        refresh,
                        entries,
                    )
                    .await;
                }
            }

            SftpCommand::OpenOrDownload { remote } => {
                let name = base_name(&remote);
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("打开", "Opening"),
                    name
                )));
                // Anything the built-in editor would refuse (binary, not UTF-8,
                // too large, unreadable) is downloaded instead — the SCP twin of
                // the SFTP double-click path.
                match scp_read_text_guarded(&handle.handle, &remote).await {
                    Ok(content) => {
                        let _ = events.send(SessionEvent::SftpFileText {
                            path: remote,
                            name,
                            content,
                            edit: true,
                            error: String::new(),
                        });
                    }
                    Err(reason) => {
                        tracing::debug!("scp double-click {remote}: not text ({reason})");
                        let _ = events.send(SessionEvent::SftpDownloadInstead { path: remote });
                    }
                }
            }

            SftpCommand::CopyTo {
                remotes,
                target,
                target_dir,
            } => {
                let handle = handle.clone();
                let events = events.clone();
                transfers.spawn(async move {
                    let label = format!("{} {}", remotes.len(), t("项", "items"));
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{} {}...",
                        t("复制到其他会话", "Copying to another session"),
                        label
                    )));
                    for remote in &remotes {
                        // Staging is an internal hop (remote → local temp → remote):
                        // run it full-speed with no mtime preservation, matching
                        // the SFTP twin. The queue's rate/mtime settings apply to
                        // the upload leg the target session performs.
                        match stage_remote_for_scp_copy(&handle, remote, &events, 0, false).await {
                            Ok((local, cleanup_root)) => {
                                let _ = target.send(SftpCommand::Upload {
                                    local,
                                    remote_dir: target_dir.clone(),
                                    cleanup_after: Some(cleanup_root),
                                    // The target session's worker applies its own
                                    // queue policy to the staged copy.
                                    decision: UploadDecision::Proceed,
                                });
                            }
                            Err(e) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("复制失败", "Copy failed")
                                )));
                            }
                        }
                    }
                });
            }

            other => {
                // Unblock the drag-out flow if it ever reaches SCP mode: a
                // reply channel with no answer would leave the UI waiting.
                if let SftpCommand::WalkForExport { reply, .. } = &other {
                    let _ = reply.send(Err(t(
                        "SCP 模式暂不支持拖拽导出",
                        "drag-out is not supported in SCP mode yet",
                    )
                    .into()));
                }
                // Finish a drag row instead of leaving it "running" forever.
                if let SftpCommand::FinishDragTransfer(progress) = &other {
                    lock_or_recover(&cancels).remove(&progress.id);
                    progress.emit_final(&events);
                }
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{}",
                    t(
                        "SCP 模式：该操作暂未支持",
                        "SCP mode: this operation is not supported yet"
                    )
                )));
                tracing::debug!("scp fallback: refusing {other:?}");
            }
        }
    }

    transfers.abort_all();
    while transfers.join_next().await.is_some() {}
    crate::ssh::disconnect_ssh(&handle, "bye").await;
    Ok(())
}

/// List `path`'s direct children over the SCP backend's `find` + `stat` chain,
/// falling back to `ls -lAn` when BusyBox `stat` is unavailable. Bounded by
/// the long timeout (10 minutes): a genuinely large directory on a slow link
/// needs the slack, and callers in the transfer tasks do not block the panel.
async fn scp_list_dir(handle: &SftpLink, path: &str) -> Result<Vec<RemoteEntry>> {
    scp_list_dir_bounded(handle, path, super::common::SCP_LONG_CMD_IO_TIMEOUT).await
}

/// [`scp_list_dir`] with an explicit command timeout. The tree probes use a
/// short bound (30 s): the tree is auxiliary UI, so a stalled directory is
/// better dropped than left blocking a refresh that walks every expanded node.
async fn scp_list_dir_bounded(
    handle: &SftpLink,
    path: &str,
    timeout: std::time::Duration,
) -> Result<Vec<RemoteEntry>> {
    let cmd = super::common::scp_stat_command(path);
    let (_status, output) = handle
        .elevation
        .exec_shell_output(&handle.handle, &cmd, timeout)
        .await
        .with_context(|| format!("scp list {path}"))?;
    let mut entries = super::common::parse_scp_stat_output(&output);
    if entries.is_empty() {
        // `stat` may be absent (BusyBox/OpenWrt) or the directory may be
        // genuinely empty. An unparseable error also lands here; the `ls`
        // fallback separates those cases below.
        let (ls_status, ls_output) = handle
            .elevation
            .exec_shell_output(&handle.handle, &super::common::scp_long_command(path), timeout)
            .await
            .with_context(|| format!("scp list {path}"))?;
        if ls_status != 0 && ls_output.trim().is_empty() {
            return Ok(Vec::new());
        }
        let ls_entries = super::common::parse_scp_long_output(&ls_output, path);
        if ls_entries.is_empty() {
            // An empty directory legitimately prints only the `total` line
            // (or nothing). Report an error only when there are entry lines we
            // failed to parse.
            let unparsed = ls_output.lines().any(|l| {
                let line = l.trim();
                !line.is_empty() && !line.starts_with("total")
            });
            if unparsed {
                anyhow::bail!("scp list {path}: {ls_output}");
            }
        }
        entries = ls_entries;
    }
    // Same order as the SFTP backend: directories first, then files, both
    // alphabetical, so the panel and tree paint identically in both modes.
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    Ok(entries)
}

/// Subdirectories of `path` only (for the tree), mirroring `list_dirs_only_impl`.
async fn scp_list_dirs_only(
    handle: &SftpLink,
    path: &str,
    timeout: std::time::Duration,
) -> Result<Vec<(String, String)>> {
    let entries = scp_list_dir_bounded(handle, path, timeout).await?;
    Ok(entries
        .into_iter()
        .filter(|e| e.is_dir)
        .map(|e| (e.name, e.full_path))
        .collect())
}

// ---------------------------------------------------------------------------
// SCP transfers
// ---------------------------------------------------------------------------

/// Monotonic counter making each upload's temp name unique process-wide.
static SCP_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Does `path` exist on the remote? A probe status other than 0/1 is an
/// error: between "free" and "taken" we must not guess.
async fn scp_remote_exists(handle: &client::Handle<SftpClientHandler>, path: &str) -> Result<bool> {
    let (status, output) = exec_remote_output(handle, &scp_test_exists(path), super::common::SCP_CMD_IO_TIMEOUT).await?;
    match status {
        0 => Ok(true),
        1 => Ok(false),
        other => Err(anyhow!(
            "{} (test -e: {other}): {}",
            t(
                "无法确认远端文件是否存在",
                "cannot determine whether the remote path exists"
            ),
            output.trim()
        )),
    }
}

/// Is `path` a remote directory? An unusable probe falls through to "file",
/// mirroring the SFTP side (metadata error → not a dir); and `scp -f` on a
/// directory without -r refuses with a clean error anyway.
async fn scp_remote_is_dir(handle: &client::Handle<SftpClientHandler>, path: &str) -> bool {
    match exec_remote_output(handle, &scp_test_dir(path), super::common::SCP_CMD_IO_TIMEOUT).await {
        Ok((status, _)) => status == 0,
        Err(_) => false,
    }
}

/// Create one remote directory level for a folder upload (`mkdir -p`).
async fn scp_mkdir_remote(handle: &client::Handle<SftpClientHandler>, path: &str) -> Result<()> {
    let (status, output) = exec_remote_output(handle, &scp_mkdir_command(path), super::common::SCP_CMD_IO_TIMEOUT).await?;
    if status != 0 {
        Err(anyhow!(
            "{} {path}: {}",
            t("创建远端目录失败", "failed to create the remote directory"),
            output.trim()
        ))
    } else {
        Ok(())
    }
}

/// Promote an uploaded temp file to its final name (`mv -f` in the same
/// directory, so it is a rename, not a copy).
async fn scp_move_remote(
    handle: &client::Handle<SftpClientHandler>,
    from: &str,
    to: &str,
) -> Result<()> {
    let (status, output) = exec_remote_output(handle, &scp_move_command(from, to), super::common::SCP_LONG_CMD_IO_TIMEOUT).await?;
    if status != 0 {
        Err(anyhow!(
            "{} {to}: {}",
            t("移动远端文件失败", "failed to move the remote file"),
            output.trim()
        ))
    } else {
        Ok(())
    }
}

/// Best-effort `rm -f` of an upload temp file after a failure or cancel.
async fn scp_remove_remote(handle: &client::Handle<SftpClientHandler>, path: &str) {
    if let Err(e) = exec_remote_output(handle, &scp_remove_command(path), super::common::SCP_CMD_IO_TIMEOUT).await {
        tracing::debug!("failed to clean scp temp {path}: {e}");
    }
}

/// Run one file-management shell command (`rm` / `mv` / `chmod` / `mkdir` /
/// `touch`) through the same privileges as the SFTP channel — `sudo`-wrapped
/// when the session is elevated to root, plain otherwise (#438) — and return
/// the remote failure detail when it exits non-zero.
async fn scp_mgmt_command(handle: &SftpLink, cmd: &str) -> std::result::Result<(), String> {
    match handle
        .elevation
        .exec_shell_output(&handle.handle, cmd, super::common::SCP_LONG_CMD_IO_TIMEOUT)
        .await
    {
        Ok((0, _)) => Ok(()),
        Ok((status, output)) if !output.trim().is_empty() => {
            Err(format!("[exit {status}] {}", output.trim()))
        }
        Ok((status, _)) => Err(format!("exit {status}")),
        Err(e) => Err(e.to_string()),
    }
}

/// First free remote name: `a.txt` → `a (1).txt`, … — the exec-probe twin of
/// [`available_remote_path`], bounded at 100 probes. A failed probe counts
/// as "free", exactly like the SFTP original treats a failed stat.
async fn scp_available_remote_path(
    handle: &client::Handle<SftpClientHandler>,
    requested: &str,
) -> String {
    let (prefix, name) = match requested.trim_end_matches('/').rsplit_once('/') {
        Some((dir, base)) => (format!("{dir}/"), base),
        None => (String::new(), requested.trim_end_matches('/')),
    };
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    };
    let candidate = |suffix: u32| match extension {
        Some(ext) if !ext.is_empty() => format!("{prefix}{stem} ({suffix}).{ext}"),
        _ => format!("{prefix}{stem} ({suffix})"),
    };
    for suffix in 1..=100u32 {
        let free = candidate(suffix);
        if !matches!(scp_remote_exists(handle, &free).await, Ok(true)) {
            return free;
        }
    }
    candidate(100)
}

/// Ask the UI what to do with an upload that hit a remote file which appeared
/// after the listing was taken. The transfer stays blocked until the UI
/// answers.
///
/// Fallback when no answer reaches us (window gone, responder dropped, or no
/// reply within the generous wait): `KeepBoth` — the user never saw the
/// question, so the file is uploaded under a fresh name instead of being
/// dropped. This differs from the dialog's own dismiss reply (`Skip`), which
/// only happens when the user was asked and declined; see
/// [`SessionEvent::SftpAskUploadConflict`] for the split.
async fn ask_upload_conflict(
    events: &UnboundedSender<SessionEvent>,
    name: &str,
) -> UploadConflictReply {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let sent = events.send(SessionEvent::SftpAskUploadConflict {
        name: name.to_string(),
        responder: UploadConflictResponder::new(tx),
    });
    if sent.is_err() {
        return UploadConflictReply::KeepBoth;
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5 * 60), rx).await {
        Ok(Ok(reply)) => reply,
        _ => UploadConflictReply::KeepBoth,
    }
}

/// The exec-probe twin of [`resolve_upload_target`]: resolve an upload's
/// target against the live server state. `None` = do not start the transfer.
/// Returns the final path plus whether it must land without clobbering.
async fn scp_resolve_upload_target(
    handle: &client::Handle<SftpClientHandler>,
    requested: String,
    decision: UploadDecision,
    policy: DedupPolicy,
    events: &UnboundedSender<SessionEvent>,
    name: &str,
) -> Option<(String, bool)> {
    let effective = match decision {
        UploadDecision::Proceed => {
            if !scp_remote_exists(handle, &requested).await.unwrap_or(false) {
                return Some((requested, true));
            }
            policy
        }
        UploadDecision::Overwrite => return Some((requested, false)),
        UploadDecision::KeepBoth => DedupPolicy::Rename,
        UploadDecision::Skip => {
            skipped_upload_status(events, name);
            return None;
        }
    };
    match effective {
        DedupPolicy::Skip => {
            skipped_upload_status(events, name);
            None
        }
        DedupPolicy::Overwrite => Some((requested, false)),
        DedupPolicy::Rename => {
            let path = scp_available_remote_path(handle, &requested).await;
            let _ = events.send(SessionEvent::SftpStatus(format!(
                "{}: {} → {}",
                t("已保留两份", "Kept both"),
                name,
                base_name(&path)
            )));
            Some((path, true))
        }
        // Ask here means the UI saw no conflict but the target exists now:
        // ask the user instead of silently replacing a file they never agreed
        // to overwrite.
        DedupPolicy::Ask => match ask_upload_conflict(events, name).await {
            UploadConflictReply::Overwrite => Some((requested, false)),
            UploadConflictReply::Skip => {
                skipped_upload_status(events, name);
                None
            }
            UploadConflictReply::KeepBoth => {
                let path = scp_available_remote_path(handle, &requested).await;
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{}: {} → {}",
                    t("已保留两份", "Kept both"),
                    name,
                    base_name(&path)
                )));
                Some((path, true))
            }
        },
    }
}

/// Permission bits for a C frame: the file's real bits on Unix, a plain 0644
/// elsewhere (the Windows side has no mode to carry).
#[cfg(unix)]
fn scp_local_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o644)
}

/// Permission bits for a C frame where the local filesystem has none.
#[cfg(not(unix))]
fn scp_local_mode(_path: &Path) -> u32 {
    0o644
}

/// Set a local file's mtime — mirrors the download-side handling in
/// `download_impl` (a blocking syscall moved off the async worker).
async fn scp_set_local_mtime(path: &str, secs: u64) {
    let when = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
    let path = path.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path) {
            let _ = f.set_modified(when);
        }
    })
    .await;
}

/// Read the message line that follows an error frame byte (1 or 2); both
/// carry one line of error text from the remote scp.
async fn scp_error_line(peer: &mut ScpPeer) -> String {
    match peer.read_line().await {
        Ok(Some(line)) => {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                t("远端报告了一个错误", "the remote reported an error").into()
            } else {
                trimmed.to_string()
            }
        }
        _ => t("远端报告了一个错误", "the remote reported an error").into(),
    }
}

/// Header exchange of a single-file `scp -f` download: our initial ack, any
/// T frames (only sent with -p), then the C frame announcing size and name.
/// Returns the announced size and the last mtime seen.
async fn scp_read_file_header(peer: &mut ScpPeer) -> Result<(u64, Option<u64>)> {
    // The source reads its first ack before sending anything.
    peer.ack().await?;
    let mut mtime = None;
    loop {
        let Some(kind) = peer.read_byte().await? else {
            anyhow::bail!(
                "{}",
                t(
                    "远端提前关闭了连接",
                    "the remote closed the connection early"
                )
            );
        };
        match kind {
            b'T' => {
                let line = peer.read_line().await?.unwrap_or_default();
                if let Some(ScpFrame::Times { mtime: secs }) = scp_parse_frame(b'T', &line) {
                    mtime = Some(secs);
                }
                peer.ack().await?;
            }
            b'C' => {
                let line = peer.read_line().await?.unwrap_or_default();
                match scp_parse_frame(b'C', &line) {
                    Some(ScpFrame::File { size, .. }) => {
                        peer.ack().await?;
                        return Ok((size, mtime));
                    }
                    _ => anyhow::bail!(
                        "{}: {line:?}",
                        t("无法解析 scp 文件头", "unparseable scp header")
                    ),
                }
            }
            1 | 2 => {
                let msg = scp_error_line(peer).await;
                anyhow::bail!("{msg}");
            }
            other => anyhow::bail!("{} {other:#04x}", t("意外 scp 帧", "unexpected scp frame")),
        }
    }
}

/// Stream one file announced by a C frame into `target`: the row states and
/// 150 ms cadence of `download_impl` over an open [`ScpPeer`] — the data
/// bytes, the terminating `\x00`, and our final ack. Called with the C frame
/// already parsed and acked. `Ok(false)` = cancelled (partial removed).
#[allow(clippy::too_many_arguments)]
async fn scp_receive_file(
    peer: &mut ScpPeer,
    target: &Path,
    name: &str,
    id: &str,
    total: u64,
    events: &UnboundedSender<SessionEvent>,
    rate_kbps: u32,
    cancel: &Arc<AtomicBool>,
    create_new: bool,
) -> Result<bool> {
    use tokio::io::AsyncWriteExt;

    const CHUNK: usize = 32 * 1024;

    let mut local_file = if create_new {
        // Atomic create-if-absent: no exists()-then-write race window, and no
        // file left behind when the target already exists.
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)
            .await
            .with_context(|| {
                format!(
                    "{}: {}",
                    t("下载目标文件已存在", "download target already exists"),
                    target.display()
                )
            })?
    } else {
        tokio::fs::File::create(target)
            .await
            .with_context(|| format!("create local {}", target.display()))?
    };

    emit_transfer(events, id, name, false, 0, total, 0, "");

    let streamed = async {
        let mut pacer = RatePacer::new(rate_kbps);
        let mut done: u64 = 0;
        let mut last = Instant::now();
        let mut chunk: Vec<u8> = Vec::with_capacity(CHUNK);
        while done < total {
            if cancel.load(Ordering::Relaxed) {
                emit_transfer(
                    events,
                    id,
                    name,
                    false,
                    done,
                    total,
                    4,
                    t("已取消", "Cancelled"),
                );
                return Ok::<bool, anyhow::Error>(false);
            }
            chunk.clear();
            let want = usize::try_from(total - done).unwrap_or(CHUNK).min(CHUNK);
            let n = peer.read_chunk(&mut chunk, want).await?;
            if n == 0 {
                anyhow::bail!(
                    "{}",
                    t(
                        "远端提前关闭了连接",
                        "the remote closed the connection early"
                    )
                );
            }
            local_file.write_all(&chunk).await?;
            done += n as u64;
            pacer.pace(n as u64, cancel).await;
            if last.elapsed() >= Duration::from_millis(150) {
                last = Instant::now();
                emit_transfer(events, id, name, false, done, total, 0, "");
            }
        }
        Ok(true)
    }
    .await;

    match streamed {
        Ok(true) => {}
        Ok(false) => {
            drop(local_file);
            let _ = tokio::fs::remove_file(target).await;
            return Ok(false);
        }
        Err(e) => {
            drop(local_file);
            let _ = tokio::fs::remove_file(target).await;
            return Err(e);
        }
    }

    // The source's terminating \x00, then our final ack.
    match peer.read_byte().await? {
        Some(0) => {}
        Some(1) | Some(2) => {
            drop(local_file);
            let _ = tokio::fs::remove_file(target).await;
            let msg = scp_error_line(peer).await;
            return Err(anyhow!("{msg}"));
        }
        Some(other) => {
            drop(local_file);
            let _ = tokio::fs::remove_file(target).await;
            anyhow::bail!(
                "{} {other:#04x}",
                t("意外 scp 终止字节", "unexpected scp terminator")
            );
        }
        None => {
            drop(local_file);
            let _ = tokio::fs::remove_file(target).await;
            anyhow::bail!(
                "{}",
                t(
                    "远端提前关闭了连接",
                    "the remote closed the connection early"
                )
            );
        }
    }
    peer.ack().await?;
    local_file.flush().await.context("flush local file")?;
    emit_transfer(events, id, name, false, total, total, 1, "");
    Ok(true)
}

/// Download one remote file over `scp -q [-p] -f` into `local` (the resolved
/// final path) — the twin of `download_impl`, minus the pipelining the scp
/// protocol cannot do. `Ok(false)` = cancelled.
#[allow(clippy::too_many_arguments)]
async fn scp_download_single(
    handle: &client::Handle<SftpClientHandler>,
    remote: &str,
    local: &str,
    name: &str,
    id: &str,
    events: &UnboundedSender<SessionEvent>,
    cancel: &Arc<AtomicBool>,
    rate_kbps: u32,
    preserve_mtime: bool,
    fail_if_exists: bool,
) -> Result<bool> {
    let mut peer = ScpPeer::open(handle, &scp_download_command(remote, false, preserve_mtime))
        .await
        .context("start remote scp")?;
    let (total, mtime) = scp_read_file_header(&mut peer).await?;
    let received = scp_receive_file(
        &mut peer,
        Path::new(local),
        name,
        id,
        total,
        events,
        rate_kbps,
        cancel,
        fail_if_exists,
    )
    .await?;
    if !received {
        peer.close().await;
        return Ok(false);
    }
    if preserve_mtime {
        if let Some(secs) = mtime {
            scp_set_local_mtime(local, secs).await;
        }
    }
    let status = peer.finish().await?;
    if !matches!(status, Some(0)) {
        // Every byte is in and acked, so keep the file; surface the odd exit
        // instead of pretending nothing happened.
        return Err(peer.with_stderr(anyhow!(
            "{}",
            t(
                "远端 scp 异常退出（文件可能不完整）",
                "the remote scp exited abnormally (the file may be incomplete)"
            )
        )));
    }
    Ok(true)
}

/// Recursively download a remote directory tree over one `scp -q [-p] -r -f`
/// connection — the twin of `download_dir`. The remote D frames (the root's
/// included) create the local mirror, and every frame name is sanitized
/// before it touches the filesystem (#26), so a hostile server cannot escape
/// the chosen folder. Remote error frames are recorded while the walk
/// continues (the scp source skips unreadable entries itself). `Ok(false)`
/// = cancelled (the partial file is cleaned up, mirrored dirs stay).
async fn scp_download_dir(
    handle: &client::Handle<SftpClientHandler>,
    remote_root: &str,
    local_parent: &str,
    events: &UnboundedSender<SessionEvent>,
    rate_kbps: u32,
    preserve_mtime: bool,
    cancel: &Arc<AtomicBool>,
) -> Result<bool> {
    let mut peer = ScpPeer::open(
        handle,
        &scp_download_command(remote_root, true, preserve_mtime),
    )
    .await
    .context("start remote scp")?;
    // The source reads its first ack before sending anything.
    peer.ack().await?;

    // `local_parent` is where the root's own D frame lands; deeper D frames
    // nest on top of it (same layout as download_dir's local_parent/root).
    let mut stack: Vec<PathBuf> = vec![PathBuf::from(local_parent)];
    let mut errors: Vec<String> = Vec::new();
    let mut pending_mtime: Option<u64> = None;
    let mut cancelled = false;

    loop {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let Some(kind) = peer.read_byte().await? else {
            break;
        };
        match kind {
            b'T' => {
                let line = peer.read_line().await?.unwrap_or_default();
                if let Some(ScpFrame::Times { mtime }) = scp_parse_frame(b'T', &line) {
                    pending_mtime = Some(mtime);
                }
                peer.ack().await?;
            }
            b'D' => {
                let line = peer.read_line().await?.unwrap_or_default();
                // Parse before acking: an unparsable D would corrupt the E
                // nesting, and a remote left waiting for its ack dies with
                // the channel instead of streaming children at us.
                let frame = scp_parse_frame(b'D', &line).ok_or_else(|| {
                    anyhow!(
                        "{}: {line:?}",
                        t("无法解析 scp 目录帧", "unparseable scp directory frame")
                    )
                })?;
                let ScpFrame::Dir { name, .. } = frame else {
                    anyhow::bail!(
                        "{}: {line:?}",
                        t("无法解析 scp 目录帧", "unparseable scp directory frame")
                    );
                };
                peer.ack().await?;
                let local_name = sanitize_filename(&name);
                let dir = stack.last().cloned().unwrap_or_default().join(&local_name);
                if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                    errors.push(format!(
                        "{}: {e}",
                        t("创建本地目录失败", "failed to create the local directory")
                    ));
                }
                // Push even on failure: the pending E frame must still match.
                stack.push(dir);
            }
            b'C' => {
                let line = peer.read_line().await?.unwrap_or_default();
                let frame = scp_parse_frame(b'C', &line).ok_or_else(|| {
                    anyhow!(
                        "{}: {line:?}",
                        t("无法解析 scp 文件头", "unparseable scp header")
                    )
                })?;
                let ScpFrame::File { size, name, .. } = frame else {
                    anyhow::bail!(
                        "{}: {line:?}",
                        t("无法解析 scp 文件头", "unparseable scp header")
                    );
                };
                peer.ack().await?;
                let file_name = sanitize_filename(&name);
                let target = stack.last().cloned().unwrap_or_default().join(&file_name);
                let id = Uuid::new_v4().to_string();
                match scp_receive_file(
                    &mut peer, &target, &file_name, &id, size, events, rate_kbps, cancel, false,
                )
                .await
                {
                    Ok(true) => {
                        if preserve_mtime {
                            if let Some(secs) = pending_mtime.take() {
                                scp_set_local_mtime(&target.to_string_lossy(), secs).await;
                            }
                        }
                    }
                    Ok(false) => {
                        cancelled = true;
                        break;
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        emit_transfer(events, &id, &file_name, false, 0, size, 2, &msg);
                        errors.push(format!("{file_name}: {msg}"));
                    }
                }
                pending_mtime = None;
            }
            b'E' => {
                let _ = peer.read_line().await?; // "E" carries no payload
                peer.ack().await?;
                if stack.len() > 1 {
                    stack.pop();
                }
            }
            1 | 2 => {
                // That entry failed on the remote; the source continues with
                // the next one — record and carry on (download-side only).
                let msg = scp_error_line(&mut peer).await;
                errors.push(msg);
            }
            other => {
                return Err(anyhow!(
                    "{} {other:#04x}",
                    t("意外 scp 帧", "unexpected scp frame")
                ));
            }
        }
    }

    if cancelled {
        peer.close().await;
        return Ok(false);
    }
    let status = peer.finish().await?;
    if !errors.is_empty() {
        let n = errors.len();
        let detail = errors.join("; ");
        return Err(peer.with_stderr(anyhow!(
            "{}: {n} — {detail}",
            t("部分文件下载失败", "some files failed to download")
        )));
    }
    if !matches!(status, Some(0)) {
        return Err(peer.with_stderr(anyhow!(
            "{}",
            t("远端 scp 异常退出", "the remote scp exited abnormally")
        )));
    }
    Ok(true)
}

/// Download one remote entry (a file or a whole folder) into an existing
/// local folder — the SCP twin of `download_one_into`. An empty folder IS
/// created here (the pane shows what is on the server).
#[allow(clippy::too_many_arguments)]
async fn scp_download_one_into(
    handle: &client::Handle<SftpClientHandler>,
    remote: &str,
    local_dir: &str,
    conflict: DownloadConflict,
    events: &UnboundedSender<SessionEvent>,
    cancel: &Arc<AtomicBool>,
    id: &str,
    rate_kbps: u32,
    preserve_mtime: bool,
) -> Result<bool> {
    if scp_remote_is_dir(handle, remote).await {
        let dirname = base_name(remote);
        let _ = events.send(SessionEvent::SftpStatus(format!(
            "{} {}/...",
            t("下载文件夹", "Downloading folder"),
            dirname
        )));
        return scp_download_dir(
            handle,
            remote,
            local_dir,
            events,
            rate_kbps,
            preserve_mtime,
            cancel,
        )
        .await;
    }
    // The server controls the name, so sanitize it before it becomes a local
    // path (#26) — same rule as the regular download path.
    let filename = sanitize_filename(&base_name(remote));
    let requested = download_target_path(remote, local_dir);
    let local_path = match conflict {
        DownloadConflict::Absent => requested,
        DownloadConflict::Replace => requested,
        DownloadConflict::KeepBoth => available_download_path(&requested),
        // Refuse-if-exists is enforced by scp_receive_file's create_new.
        DownloadConflict::Fail => requested,
    };
    scp_download_single(
        handle,
        remote,
        &local_path.to_string_lossy(),
        &filename,
        id,
        events,
        cancel,
        rate_kbps,
        preserve_mtime,
        conflict == DownloadConflict::Fail,
    )
    .await
}

/// Upload one local file to `remote_final` over `scp -q -t`: stream it to a
/// temp name in the target directory, then `mv -f` it into place, so a
/// failed or cancelled upload never damages an existing file. Mirrors
/// `upload_pipelined`'s row states and cadence. `Ok(false)` = cancelled.
#[allow(clippy::too_many_arguments)]
async fn scp_push_file(
    handle: &client::Handle<SftpClientHandler>,
    local: &Path,
    remote_final: &str,
    name: &str,
    id: &str,
    events: &UnboundedSender<SessionEvent>,
    cancel: &Arc<AtomicBool>,
    rate_kbps: u32,
    no_clobber: bool,
) -> Result<bool> {
    use tokio::io::AsyncReadExt;

    const CHUNK: usize = 32 * 1024;

    // A frame cannot carry a newline, and separators would corrupt the path.
    if !scp_frame_name_ok(name) {
        anyhow::bail!(
            "{}",
            t(
                "文件名无法通过 scp 传输",
                "the file name cannot travel over scp"
            )
        );
    }
    let total = tokio::fs::metadata(local)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let mode = scp_local_mode(local);
    let mut reader = tokio::fs::File::open(local)
        .await
        .with_context(|| format!("open local {}", local.display()))?;

    let seq = SCP_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let remote_parent = parent_dir(remote_final);
    let tmp = format!(
        "{}/{}",
        remote_parent.trim_end_matches('/'),
        scp_tmp_name(name, seq)
    );

    let mut peer = ScpPeer::open(handle, &scp_upload_command(&tmp))
        .await
        .context("start remote scp")?;

    let pushed = async {
        // The sink writes its first ack byte before reading anything.
        peer.expect_ack().await?;
        // The C frame; a non-zero ack means the sink could not open the
        // target and NO data may follow — the push is abandoned.
        peer.send(scp_c_frame(mode, total, name).as_bytes()).await?;
        peer.expect_ack().await?;

        emit_transfer(events, id, name, true, 0, total, 0, "");
        let mut pacer = RatePacer::new(rate_kbps);
        let mut done: u64 = 0;
        let mut last = Instant::now();
        let mut buf = vec![0u8; CHUNK];
        // The frame announced `total`, so exactly that many bytes may
        // follow: a local file that grows mid-upload keeps its remote copy
        // at the announced size (the sink would misparse any extra byte as
        // the next frame), and one that shrinks fails cleanly below.
        while done < total {
            if cancel.load(Ordering::Relaxed) {
                emit_transfer(
                    events,
                    id,
                    name,
                    true,
                    done,
                    total,
                    4,
                    t("已取消", "Cancelled"),
                );
                return Ok::<bool, anyhow::Error>(false);
            }
            let want = usize::try_from(total - done).unwrap_or(CHUNK).min(CHUNK);
            let n = reader.read(&mut buf[..want]).await?;
            if n == 0 {
                anyhow::bail!(
                    "{}",
                    t(
                        "本地文件在读取中变短",
                        "the local file shrank while reading"
                    )
                );
            }
            peer.send(&buf[..n]).await?;
            done += n as u64;
            pacer.pace(n as u64, cancel).await;
            if last.elapsed() >= Duration::from_millis(150) {
                last = Instant::now();
                emit_transfer(events, id, name, true, done, total, 0, "");
            }
        }
        // Terminating \x00, then the sink's final ack. EOF follows so the
        // remote `scp -t` sink exits instead of waiting for more input:
        // without it `finish()` times out even though the file was acked.
        peer.send(&b"\x00"[..]).await?;
        peer.expect_ack().await?;
        peer.eof().await;
        let status = peer.finish().await?;
        if !matches!(status, Some(0)) {
            anyhow::bail!(
                "{}",
                t("远端 scp 异常退出", "the remote scp exited abnormally")
            );
        }
        Ok(true)
    }
    .await;

    match pushed {
        Ok(true) => {}
        Ok(false) => {
            peer.close().await;
            scp_remove_remote(handle, &tmp).await;
            return Ok(false);
        }
        Err(e) => {
            peer.close().await;
            scp_remove_remote(handle, &tmp).await;
            return Err(peer.with_stderr(e));
        }
    }
    // Promote the temp. With no_clobber the earlier probe said "free", but
    // another client may have created the target since — re-probe and
    // renumber instead of replacing (the exec twin of open_upload_target's
    // exclusive create).
    let mut final_path = remote_final.to_string();
    if no_clobber && scp_remote_exists(handle, &final_path).await.unwrap_or(true) {
        final_path = scp_available_remote_path(handle, &final_path).await;
    }
    if let Err(e) = scp_move_remote(handle, &tmp, &final_path).await {
        scp_remove_remote(handle, &tmp).await;
        return Err(e);
    }
    emit_transfer(events, id, name, true, total, total, 1, "");
    Ok(true)
}

/// Recursively upload a local directory tree into `remote_parent` over scp
/// (#50) — the twin of `upload_dir` with per-file isolation: each remote
/// level is created with `mkdir -p`, each file goes through the temp + `mv`
/// push with its own row, and a failed file is recorded while the walk
/// continues (that isolation is exactly why uploads go through temp names).
/// Symlinks and special files are skipped, like `upload_dir`. Returns
/// (ok, failed, cancelled).
async fn scp_upload_dir(
    handle: &client::Handle<SftpClientHandler>,
    local_root: &Path,
    remote_parent: &str,
    events: &UnboundedSender<SessionEvent>,
    rate_kbps: u32,
    cancel: &Arc<AtomicBool>,
) -> Result<(usize, usize, bool)> {
    let root_name = local_file_name_utf8(local_root)?;
    if !scp_frame_name_ok(&root_name) {
        anyhow::bail!(
            "{}",
            t(
                "文件夹名无法通过 scp 传输",
                "the folder name cannot travel over scp"
            )
        );
    }
    let remote_root = format!("{}/{}", remote_parent.trim_end_matches('/'), root_name);
    scp_mkdir_remote(handle, &remote_root).await?;
    let mut stack = vec![(local_root.to_path_buf(), remote_root)];
    let mut ok = 0usize;
    let mut failed = 0usize;
    while let Some((ldir, rdir)) = stack.pop() {
        let mut rd = tokio::fs::read_dir(&ldir)
            .await
            .with_context(|| format!("read local dir {}", ldir.display()))?;
        while let Some(entry) = rd.next_entry().await.context("read dir entry")? {
            if cancel.load(Ordering::Relaxed) {
                return Ok((ok, failed, true));
            }
            let lpath = entry.path();
            let name = local_file_name_utf8(&lpath)?;
            let rchild = format!("{}/{}", rdir, name);
            let ft = entry.file_type().await.context("file type")?;
            if ft.is_dir() {
                if !scp_frame_name_ok(&name) {
                    failed += 1;
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{}: {}",
                        t(
                            "已跳过（名称无法通过 scp 传输）",
                            "Skipped (name cannot travel over scp)"
                        ),
                        name
                    )));
                    continue;
                }
                // A dir we cannot create (permissions, a file in the way)
                // fails its whole subtree — skip it rather than abort.
                if let Err(e) = scp_mkdir_remote(handle, &rchild).await {
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{}: {e}",
                        t("创建远端目录失败", "failed to create the remote directory")
                    )));
                    continue;
                }
                stack.push((lpath, rchild));
            } else if ft.is_file() {
                let id = Uuid::new_v4().to_string();
                match scp_push_file(
                    handle, &lpath, &rchild, &name, &id, events, cancel, rate_kbps, false,
                )
                .await
                {
                    Ok(true) => ok += 1,
                    Ok(false) => return Ok((ok, failed, true)),
                    Err(e) => {
                        failed += 1;
                        let msg = format!("{e:#}");
                        emit_transfer(events, &id, &name, true, 0, 0, 2, &msg);
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {msg}",
                            t("上传失败", "Upload failed")
                        )));
                    }
                }
            }
        }
    }
    Ok((ok, failed, false))
}

/// Download one remote file's bytes into memory, capped at `cap` — bails if
/// the C frame announces more (or the stream delivers more), so an untrusted
/// size can never balloon the allocation.
async fn scp_fetch_bytes(
    handle: &client::Handle<SftpClientHandler>,
    remote: &str,
    cap: u64,
) -> Result<Vec<u8>> {
    let mut peer = ScpPeer::open(handle, &scp_download_command(remote, false, false))
        .await
        .context("start remote scp")?;
    let (total, _) = scp_read_file_header(&mut peer).await?;
    if total > cap {
        anyhow::bail!(
            "{}",
            t("文件超出读取上限", "the file exceeds the read limit")
        );
    }
    let mut out = Vec::with_capacity(total.min(cap) as usize);
    let mut chunk: Vec<u8> = Vec::new();
    while (out.len() as u64) < total {
        chunk.clear();
        let want = ((total - out.len() as u64) as usize).min(32 * 1024);
        let n = peer.read_chunk(&mut chunk, want).await?;
        if n == 0 {
            anyhow::bail!(
                "{}",
                t(
                    "远端提前关闭了连接",
                    "the remote closed the connection early"
                )
            );
        }
        out.extend_from_slice(&chunk);
    }
    match peer.read_byte().await? {
        Some(0) => {}
        Some(1) | Some(2) => {
            let msg = scp_error_line(&mut peer).await;
            anyhow::bail!("{msg}");
        }
        Some(other) => {
            anyhow::bail!(
                "{} {other:#04x}",
                t("意外 scp 终止字节", "unexpected scp terminator")
            );
        }
        None => anyhow::bail!(
            "{}",
            t(
                "远端提前关闭了连接",
                "the remote closed the connection early"
            )
        ),
    }
    peer.ack().await?;
    let status = peer.finish().await?;
    if !matches!(status, Some(0)) {
        anyhow::bail!(
            "{}",
            t("远端 scp 异常退出", "the remote scp exited abnormally")
        );
    }
    Ok(out)
}

/// Read a remote file as UTF-8 text for the built-in editor over scp — the
/// twin of `read_text_guarded`, with the same guards (512 KB cap, binary
/// rejection, line caps) applied to the fetched bytes.
async fn scp_read_text_guarded(
    handle: &client::Handle<SftpClientHandler>,
    remote: &str,
) -> std::result::Result<String, String> {
    // Stat first so a huge file is rejected before a byte moves.
    let (status, output) = exec_remote_output(handle, &scp_stat_single_command(remote), super::common::SCP_CMD_IO_TIMEOUT)
        .await
        .map_err(|e| format!("{}: {e}", t("打开失败", "Open failed")))?;
    if status != 0 || output.trim().is_empty() {
        return Err(format!(
            "{}: {}",
            t("打开失败", "Open failed"),
            output.trim()
        ));
    }
    let size = parse_scp_stat_output(&output)
        .first()
        .map(|e| e.size)
        .unwrap_or(0);
    if size > MAX_BUILTIN_EDITOR_BYTES as u64 {
        return Err(editor_rejection_message(EditorTextRejection::TooLarge));
    }
    // Read at most one byte past the limit so an untrusted file that grew
    // between the stat and the read is caught exactly like the SFTP path.
    let bytes = scp_fetch_bytes(handle, remote, MAX_BUILTIN_EDITOR_BYTES as u64 + 1)
        .await
        .map_err(|e| format!("{}: {e}", t("读取失败", "Read failed")))?;
    validate_editor_text(bytes).map_err(editor_rejection_message)
}

/// Overwrite a remote file with the given text over scp — the twin of
/// `write_text_file`: upload to a temp name, then `mv -f` into place, so a
/// failed save never leaves a truncated original behind.
async fn scp_write_text(
    handle: &client::Handle<SftpClientHandler>,
    remote: &str,
    content: &str,
) -> Result<()> {
    let name = base_name(remote);
    if !scp_frame_name_ok(&name) {
        anyhow::bail!(
            "{}",
            t(
                "远端文件名无法通过 scp 传输",
                "the remote file name cannot travel over scp"
            )
        );
    }
    let bytes = content.as_bytes();
    let seq = SCP_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let remote_parent = parent_dir(remote);
    let tmp = format!(
        "{}/{}",
        remote_parent.trim_end_matches('/'),
        scp_tmp_name(&name, seq)
    );

    let mut peer = ScpPeer::open(handle, &scp_upload_command(&tmp))
        .await
        .context("start remote scp")?;

    let pushed = async {
        peer.expect_ack().await?;
        peer.send(scp_c_frame(0o644, bytes.len() as u64, &name).as_bytes())
            .await?;
        peer.expect_ack().await?;
        if !bytes.is_empty() {
            peer.send(bytes).await?;
        }
        peer.send(&b"\x00"[..]).await?;
        peer.expect_ack().await?;
        peer.eof().await;
        let status = peer.finish().await?;
        if !matches!(status, Some(0)) {
            anyhow::bail!(
                "{}",
                t("远端 scp 异常退出", "the remote scp exited abnormally")
            );
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if let Err(e) = pushed {
        peer.close().await;
        scp_remove_remote(handle, &tmp).await;
        return Err(peer.with_stderr(e));
    }
    if let Err(e) = scp_move_remote(handle, &tmp, remote).await {
        scp_remove_remote(handle, &tmp).await;
        return Err(e);
    }
    Ok(())
}

/// Upper bounds for one drag-out (#504), so dropping `/` by accident cannot
/// make the walk run (and the UI wait) forever.
const MAX_EXPORT_ENTRIES: usize = 50_000;
const MAX_EXPORT_DEPTH: usize = 64;

/// Stat the dragged `roots` and walk folders depth-first, returning every
/// folder before its contents. Relative paths start at each root's own name.
async fn walk_for_export(
    sftp: &SftpSession,
    roots: &[String],
) -> Result<Vec<super::transfer::ExportEntry>> {
    use super::transfer::ExportEntry;
    let is_dir = |perms: Option<u32>| (perms.unwrap_or(0) & 0o170_000) == 0o040_000;
    let mut out = Vec::new();
    for root in roots {
        let meta = sftp
            .metadata(root)
            .await
            .with_context(|| format!("stat {root}"))?;
        let name = base_name(root);
        let root_is_dir = is_dir(meta.permissions);
        out.push(ExportEntry {
            remote: root.clone(),
            relative: name.clone(),
            is_dir: root_is_dir,
            size: if root_is_dir {
                0
            } else {
                meta.size.unwrap_or(0)
            },
            mtime: meta.mtime.unwrap_or(0) as u64,
        });
        if !root_is_dir {
            continue;
        }
        let mut stack = vec![(root.clone(), name, 1usize)];
        while let Some((dir, rel, depth)) = stack.pop() {
            if depth > MAX_EXPORT_DEPTH {
                anyhow::bail!("{dir}: {}", t("目录层级过深", "folder nesting is too deep"));
            }
            let mut children = list_dir_impl(sftp, &dir)
                .await
                .with_context(|| format!("list {dir}"))?;
            // Stable, readable order in Explorer's progress dialog.
            children.sort_by(|a, b| a.name.cmp(&b.name));
            for child in children {
                if out.len() >= MAX_EXPORT_ENTRIES {
                    anyhow::bail!(
                        "{} ({MAX_EXPORT_ENTRIES}+)",
                        t("文件数量过多", "too many files")
                    );
                }
                let child_rel = format!("{rel}/{}", child.name);
                out.push(ExportEntry {
                    remote: child.full_path.clone(),
                    relative: child_rel.clone(),
                    is_dir: child.is_dir,
                    size: if child.is_dir { 0 } else { child.size },
                    mtime: child.modified as u64,
                });
                if child.is_dir {
                    stack.push((child.full_path, child_rel, depth + 1));
                }
            }
        }
    }
    Ok(out)
}

/// Transfer state codes understood by the Transfers panel.
const TRANSFER_ACTIVE: u8 = 0;
const TRANSFER_DONE: u8 = 1;
const TRANSFER_FAILED: u8 = 2;
const TRANSFER_CANCELLED: u8 = 4;

impl super::transfer::DragTransfer {
    /// Bytes to show as an in-progress row, or `None` when nothing should be
    /// emitted (finished already, or throttled to one update per 150 ms).
    #[cfg(test)]
    pub(crate) fn progress_update(&self, force: bool) -> Option<u64> {
        let mut report = lock_or_recover(&self.report);
        self.progress_update_locked(&mut report, force)
    }

    fn progress_update_locked(&self, report: &mut DragReport, force: bool) -> Option<u64> {
        if report.finished {
            return None;
        }
        let now = Instant::now();
        if !force
            && report
                .last_emit
                .is_some_and(|t| now.duration_since(t) < Duration::from_millis(150))
        {
            return None;
        }
        report.last_emit = Some(now);
        report.started = true;
        Some(self.done.load(Ordering::Relaxed))
    }

    /// The row's final `(state, bytes, message)`, once. `None` if the drag
    /// never transferred anything (no row was created) or already finished.
    #[cfg(test)]
    pub(crate) fn final_update(&self) -> Option<(u8, u64, String)> {
        let mut report = lock_or_recover(&self.report);
        self.final_update_locked(&mut report)
    }

    fn final_update_locked(&self, report: &mut DragReport) -> Option<(u8, u64, String)> {
        if report.finished {
            return None;
        }
        report.finished = true;
        if !report.started {
            return None;
        }
        let done = self.done.load(Ordering::Relaxed);
        Some(if let Some(error) = &report.error {
            (TRANSFER_FAILED, done, error.clone())
        } else if self.cancel.load(Ordering::Relaxed) || self.interrupted.load(Ordering::Relaxed) {
            (TRANSFER_CANCELLED, done, String::new())
        } else {
            (TRANSFER_DONE, self.total.max(done), String::new())
        })
    }

    /// Remember the first failure; it decides the final row.
    pub(crate) fn fail(&self, message: String) {
        let mut report = lock_or_recover(&self.report);
        if report.error.is_none() {
            report.error = Some(message);
        }
    }

    fn emit_progress(&self, events: &UnboundedSender<SessionEvent>, force: bool) {
        // Keep snapshots and synchronous sends ordered across concurrent files.
        let mut report = lock_or_recover(&self.report);
        if let Some(done) = self.progress_update_locked(&mut report, force) {
            emit_transfer(events, &self.id, &self.name, false, done, self.total, TRANSFER_ACTIVE, "");
        }
    }

    fn emit_final(&self, events: &UnboundedSender<SessionEvent>) {
        // Keep snapshots and synchronous sends ordered across concurrent files.
        let mut report = lock_or_recover(&self.report);
        if let Some((state, done, message)) = self.final_update_locked(&mut report) {
            emit_transfer(events, &self.id, &self.name, false, done, self.total, state, &message);
        }
    }
}

/// Stream `remote` into `chunks` in file order over a dedicated SFTP
/// channel, keeping up to ~1 MB of READ requests in flight like
/// `download_impl`. Delivered bytes count toward the drag's Transfers row.
/// Stops when the receiver goes away (Explorer cancelled) or the row's
/// Cancel button is pressed.
async fn stream_file_impl(
    handle: &SftpLink,
    remote: &str,
    chunks: &tokio::sync::mpsc::Sender<std::result::Result<Vec<u8>, String>>,
    progress: &super::transfer::DragTransfer,
    events: &UnboundedSender<SessionEvent>,
) -> Result<()> {
    use futures::stream::FuturesOrdered;

    const CHUNK: usize = 32 * 1024;
    const MAX_INFLIGHT: usize = 32;

    let stream = handle
        .elevation
        .open_stream(&handle.handle)
        .await
        .context("open sftp channel")?;
    let raw = Arc::new(RawSftpSession::new(stream));
    raw.init().await.context("sftp handshake")?;
    let total = raw
        .stat(remote)
        .await
        .ok()
        .and_then(|a| a.attrs.size)
        .unwrap_or(0);
    let fhandle = raw
        .open(remote, OpenFlags::READ, FileAttributes::default())
        .await
        .with_context(|| format!("open remote {remote}"))?
        .handle;

    // Fixed-size reads in order. When the size is unknown (or the file grew),
    // keep reading past `total` until a short/empty read signals EOF.
    let mut next_off = 0u64;
    let mut eof = false;
    let mut inflight = FuturesOrdered::new();
    loop {
        while !eof && inflight.len() < MAX_INFLIGHT && (total == 0 || next_off < total) {
            let off = next_off;
            let want = if total == 0 {
                CHUNK
            } else {
                ((total - off) as usize).min(CHUNK)
            };
            next_off += want as u64;
            let raw = raw.clone();
            let h = fhandle.clone();
            inflight.push_back(async move {
                let mut data = Vec::with_capacity(want);
                let mut o = off;
                let end = off + want as u64;
                while o < end {
                    match raw.read(h.clone(), o, (end - o) as u32).await {
                        Ok(d) if d.data.is_empty() => break,
                        Ok(d) => {
                            o += d.data.len() as u64;
                            data.extend_from_slice(&d.data);
                        }
                        Err(SftpError::Status(s)) if s.status_code == StatusCode::Eof => break,
                        Err(e) => return Err(anyhow!("read remote: {e}")),
                    }
                }
                Ok::<(Vec<u8>, bool), anyhow::Error>((data, (o - off) < want as u64))
            });
            // Unknown size: one request at a time is enough to find EOF.
            if total == 0 {
                break;
            }
        }
        if progress.cancel.load(Ordering::Relaxed) {
            let _ = raw.close(fhandle).await;
            anyhow::bail!("{}", t("已取消", "Cancelled"));
        }
        let Some(result) = inflight.next().await else {
            break;
        };
        let (data, short) = result?;
        let n = data.len() as u64;
        if !data.is_empty() {
            if chunks.send(Ok(data)).await.is_err() {
                // Receiver dropped: Explorer cancelled or skipped this file.
                progress.interrupted.store(true, Ordering::Relaxed);
                break;
            }
            progress.done.fetch_add(n, Ordering::Relaxed);
            progress.emit_progress(events, false);
        }
        if short {
            eof = true;
            if inflight.is_empty() {
                break;
            }
        }
    }
    let _ = raw.close(fhandle).await;
    Ok(())
}

const MAX_BUILTIN_EDITOR_BYTES: usize = 512 * 1024;
const MAX_BUILTIN_EDITOR_LINES: usize = 10_000;
const MAX_BUILTIN_EDITOR_LINE_BYTES: usize = 64 * 1024;

#[derive(Debug, PartialEq, Eq)]
enum EditorTextRejection {
    TooLarge,
    TooManyLines,
    LineTooLong,
    Binary,
    InvalidUtf8,
}

/// Control characters other than tab / LF / CR (and DEL): the file is binary.
fn has_control_bytes(bytes: &[u8]) -> bool {
    bytes.iter().any(|&byte| {
        (byte < 0x20 && byte != b'\t' && byte != b'\n' && byte != b'\r') || byte == 0x7f
    })
}

fn validate_editor_text(bytes: Vec<u8>) -> std::result::Result<String, EditorTextRejection> {
    if bytes.len() > MAX_BUILTIN_EDITOR_BYTES {
        return Err(EditorTextRejection::TooLarge);
    }
    if has_control_bytes(&bytes) {
        return Err(EditorTextRejection::Binary);
    }

    let text = String::from_utf8(bytes).map_err(|_| EditorTextRejection::InvalidUtf8)?;
    let mut line_count = 1usize;
    let mut line_bytes = 0usize;
    for byte in text.bytes() {
        if byte == b'\n' {
            line_count += 1;
            line_bytes = 0;
            if line_count > MAX_BUILTIN_EDITOR_LINES {
                return Err(EditorTextRejection::TooManyLines);
            }
        } else {
            line_bytes += 1;
            if line_bytes > MAX_BUILTIN_EDITOR_LINE_BYTES {
                return Err(EditorTextRejection::LineTooLong);
            }
        }
    }
    Ok(text)
}

fn editor_rejection_message(rejection: EditorTextRejection) -> String {
    match rejection {
        EditorTextRejection::TooLarge => t(
            "文件过大,无法在内置编辑器中打开(上限 512 KB),请使用外部打开/编辑或下载",
            "Too large for the built-in editor (512 KB limit); open/edit externally or download it instead",
        ),
        EditorTextRejection::TooManyLines => t(
            "文本过大(超过 10,000 行),无法在内置编辑器中打开,请使用外部打开/编辑或下载",
            "Text too large (over 10,000 lines) for the built-in editor; open/edit externally or download it instead",
        ),
        EditorTextRejection::LineTooLong => t(
            "文件包含过长的单行,无法在内置编辑器中安全打开,请使用外部打开/编辑或下载",
            "Contains a line too long for the built-in editor; open/edit externally or download it instead",
        ),
        EditorTextRejection::Binary => t(
            "包含控制字符(疑似二进制),无法以文本打开,请下载查看",
            "Contains control characters (likely binary); download it instead",
        ),
        EditorTextRejection::InvalidUtf8 => {
            t("非 UTF-8 文本,无法打开", "Not UTF-8 text; cannot open")
        }
    }
    .into()
}

/// Read a remote file as UTF-8 text for the built-in editor, rejecting content
/// that would make Slint eagerly lay out an unsafe amount of text (#70, #331).
/// Returns the text on success or a human-readable error message on failure.
async fn read_text_guarded(
    sftp: &SftpSession,
    remote: &str,
) -> std::result::Result<String, String> {
    use tokio::io::AsyncReadExt;
    let size = sftp
        .metadata(remote)
        .await
        .ok()
        .and_then(|m| m.size)
        .unwrap_or(0);
    if size > MAX_BUILTIN_EDITOR_BYTES as u64 {
        return Err(editor_rejection_message(EditorTextRejection::TooLarge));
    }
    let f = sftp
        .open(remote)
        .await
        .map_err(|e| format!("{}: {e}", t("打开失败", "Open failed")))?;
    // Metadata may be missing or stale. Read at most one byte past the limit so
    // an untrusted remote file can never make this path allocate without bound.
    let mut bytes = Vec::with_capacity((size as usize).min(MAX_BUILTIN_EDITOR_BYTES));
    let mut reader = f.take(MAX_BUILTIN_EDITOR_BYTES as u64 + 1);
    let mut chunk = [0u8; 8192];
    let mut lines = 1usize;
    loop {
        let count = reader
            .read(&mut chunk)
            .await
            .map_err(|e| format!("{}: {e}", t("读取失败", "Read failed")))?;
        if count == 0 {
            break;
        }
        // Binary files usually show it in the first chunk: stop reading
        // instead of pulling up to 512 KB first (matters for #493, where every
        // double-clicked file is tried as text before it is downloaded).
        if has_control_bytes(&chunk[..count]) {
            return Err(editor_rejection_message(EditorTextRejection::Binary));
        }
        lines += chunk[..count].iter().filter(|&&byte| byte == b'\n').count();
        if lines > MAX_BUILTIN_EDITOR_LINES {
            return Err(editor_rejection_message(EditorTextRejection::TooManyLines));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    validate_editor_text(bytes).map_err(editor_rejection_message)
}

/// Overwrite a remote file with the given text (CREATE | WRITE | TRUNCATE).
async fn write_text_file(sftp: &SftpSession, remote: &str, content: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut f = sftp
        .create(remote)
        .await
        .with_context(|| format!("create remote {remote}"))?;
    f.write_all(content.as_bytes())
        .await
        .context("write remote file")?;
    f.flush().await.context("flush remote file")?;
    let _ = f.shutdown().await;
    Ok(())
}

/// File name component of a path.  Handles both remote (`/`) and local Windows
/// (`\`) separators, so uploading `C:\…\frp.tar.gz` yields `frp.tar.gz` rather
/// than the whole path (which previously became the remote file name).
fn base_name(path: &str) -> String {
    let sep = |c: char| c == '/' || c == '\\';
    path.trim_end_matches(sep)
        .rsplit(sep)
        .next()
        .unwrap_or(path)
        .to_string()
}

fn local_file_name_utf8(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("local file name is not valid UTF-8: {}", path.display()))
}

/// Single-quote a string for safe interpolation into a remote `/bin/sh`
/// command. Remote names come from the *server's* listing and are therefore
/// untrusted — without quoting, a crafted name like `; rm -rf ~` would run.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

async fn exec_remote_output(
    handle: &client::Handle<SftpClientHandler>,
    cmd: &str,
    timeout: std::time::Duration,
) -> Result<(u32, String)> {
    let mut ch = handle
        .channel_open_session()
        .await
        .context("open owner lookup channel")?;
    ch.exec(true, cmd.as_bytes())
        .await
        .context("execute owner lookup")?;
    let _ = ch.eof().await;
    let mut output = Vec::new();
    let mut status = 0u32;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let msg = match tokio::time::timeout_at(deadline, ch.wait()).await {
            Ok(Some(msg)) => msg,
            Ok(None) => break,
            Err(_) => {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), ch.close()).await;
                return Err(anyhow::anyhow!(t("命令执行超时", "command timed out")));
            }
        };
        match msg {
            russh::ChannelMsg::Data { data } | russh::ChannelMsg::ExtendedData { data, .. } => {
                output.extend_from_slice(&data)
            }
            russh::ChannelMsg::ExitStatus { exit_status } => status = exit_status,
            russh::ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok((status, String::from_utf8_lossy(&output).into_owned()))
}

/// Parent directory of a remote path ("/a/b" → "/a", "/a" → "/").
fn parent_dir(path: &str) -> String {
    let p = path.trim_end_matches('/');
    match p.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => p[..i].to_string(),
    }
}

/// Open a local file with the OS default application.
///
/// Security: we must NOT route the path through a shell.  The previous
/// `cmd /C start "" <path>` let cmd.exe re-parse the path, so a remote file name
/// containing shell metacharacters (`&` `|` `>` `<` `^` …) — e.g. `foo&calc.exe`
/// — could inject and run arbitrary commands when the user opened it.  We call
/// `ShellExecuteW` directly instead: it treats the path as one opaque string, so
/// no shell parsing happens.  (`xdg-open` on Unix already takes a single argv
/// argument and never invokes a shell.)
#[cfg(windows)]
pub(crate) fn open_with_os(path: &str) {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: isize,
            lp_operation: *const u16,
            lp_file: *const u16,
            lp_parameters: *const u16,
            lp_directory: *const u16,
            n_show_cmd: i32,
        ) -> isize;
    }
    let to_wide = |s: &str| -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let op = to_wide("open");
    let file = to_wide(path);
    unsafe {
        ShellExecuteW(
            0,
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        );
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn open_with_os(path: &str) {
    let _ = std::process::Command::new("open").arg(path).spawn();
}

#[cfg(all(not(windows), not(target_os = "macos")))]
pub(crate) fn open_with_os(path: &str) {
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

/// Make a remote-supplied file name safe to use as a *local* file name (for
/// both downloads and temp files): drops path separators (defence-in-depth
/// against traversal), replaces characters invalid on Windows or special to
/// shells with `_`, trims surrounding whitespace and Windows' trailing dots,
/// and neutralises reserved device names (CON, NUL, COM1…).  Normal names
/// (letters, digits, `.`, `-`, `_`, Unicode) pass through; Unix dotfiles keep
/// their leading dot.  Falls back to `file` when nothing usable remains.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*' | '&' | '^' | '%' | '!' | '`'
            | '$' | '\'' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    // Drop leading whitespace and trailing dots/spaces (Windows strips the
    // latter silently). A leading dot is preserved so `.bashrc` survives.
    let trimmed = cleaned.trim_start_matches(' ').trim_end_matches([' ', '.']);
    if trimmed.is_empty() {
        return "file".to_string();
    }
    // Windows reserved device names are reserved case-insensitively and even
    // with an extension ("CON.txt" still opens the console). A download named
    // after one could read/write a device instead of a file, so prefix `_`.
    let stem = trimmed.split('.').next().unwrap_or(trimmed);
    let reserved = matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    );
    if reserved {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    }
}

fn external_edit_local_name(host: &str, filename: &str, unique: &str) -> String {
    format!(
        "{}_{}_{}",
        sanitize_filename(host),
        sanitize_filename(unique),
        sanitize_filename(filename)
    )
}

pub(crate) fn download_target_path(remote: &str, local_dir: &str) -> PathBuf {
    Path::new(local_dir).join(sanitize_filename(&base_name(remote)))
}

pub(crate) fn available_download_path(requested: &Path) -> PathBuf {
    if !requested.exists() {
        return requested.to_path_buf();
    }
    let parent = requested.parent().unwrap_or_else(|| Path::new(""));
    let stem = requested
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    let extension = requested.extension().and_then(|value| value.to_str());
    for suffix in 1usize.. {
        let name = match extension {
            Some(extension) if !extension.is_empty() => format!("{stem} ({suffix}).{extension}"),
            _ => format!("{stem} ({suffix})"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("an unused download filename suffix must exist")
}

/// First free remote name: `a.txt` → `a (1).txt`, `a (2).txt`, … mirroring the
/// local [`available_download_path`] numbering. Bounded at 100 probes so a
/// pathological server can't spin us; the last candidate is returned as-is.
async fn available_remote_path(sftp: &SftpSession, requested: &str) -> String {
    // Number only the file name: `requested` carries a directory prefix, and
    // the extension belongs to the name alone (not to a directory like
    // `backup.dir`). The split mirrors std's `file_stem`/`extension` on the
    // local side of [`available_download_path`]: a dot leading the name
    // (`.bashrc`) is not an extension separator.
    let (prefix, name) = match requested.trim_end_matches('/').rsplit_once('/') {
        Some((dir, base)) => (format!("{dir}/"), base),
        None => (String::new(), requested.trim_end_matches('/')),
    };
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    };
    let candidate = |suffix: u32| match extension {
        Some(ext) if !ext.is_empty() => format!("{prefix}{stem} ({suffix}).{ext}"),
        _ => format!("{prefix}{stem} ({suffix})"),
    };
    for suffix in 1..=100u32 {
        let free = candidate(suffix);
        if sftp.metadata(&free).await.is_err() {
            return free;
        }
    }
    candidate(100)
}

fn skipped_upload_status(events: &UnboundedSender<SessionEvent>, name: &str) {
    let _ = events.send(SessionEvent::SftpStatus(format!(
        "{}: {}",
        t("已跳过（远端已存在）", "Skipped (target already exists)"),
        name
    )));
}

/// Resolve an upload's remote target against the live server state; `None`
/// means the transfer must not start. A decision from the UI (which could
/// prompt) wins; when the UI saw no conflict the worker still applies the
/// configured policy, because the displayed listing can be stale and the file
/// picker round trip gives the remote time to change.
///
/// Returns the resolved path plus whether the create must be atomic
/// (`no_clobber`): every target we are returning because it looked free must
/// not be silently replaced if it appears while the transfer is being queued.
async fn resolve_upload_target(
    sftp: &SftpSession,
    requested: String,
    decision: UploadDecision,
    policy: DedupPolicy,
    events: &UnboundedSender<SessionEvent>,
    name: &str,
) -> Option<(String, bool)> {
    let effective = match decision {
        UploadDecision::Proceed => {
            if sftp.metadata(&requested).await.is_err() {
                return Some((requested, true));
            }
            policy
        }
        UploadDecision::Overwrite => return Some((requested, false)),
        UploadDecision::KeepBoth => DedupPolicy::Rename,
        UploadDecision::Skip => {
            skipped_upload_status(events, name);
            return None;
        }
    };
    match effective {
        DedupPolicy::Skip => {
            skipped_upload_status(events, name);
            None
        }
        DedupPolicy::Overwrite => Some((requested, false)),
        DedupPolicy::Rename => {
            let path = available_remote_path(sftp, &requested).await;
            let _ = events.send(SessionEvent::SftpStatus(format!(
                "{}: {} → {}",
                t("已保留两份", "Kept both"),
                name,
                base_name(&path)
            )));
            Some((path, true))
        }
        // Ask here means the UI showed no conflict but the target exists now:
        // ask the user instead of silently replacing a file they never agreed
        // to overwrite.
        DedupPolicy::Ask => match ask_upload_conflict(events, name).await {
            UploadConflictReply::Overwrite => Some((requested, false)),
            UploadConflictReply::Skip => {
                skipped_upload_status(events, name);
                None
            }
            UploadConflictReply::KeepBoth => {
                let path = available_remote_path(sftp, &requested).await;
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{}: {} → {}",
                    t("已保留两份", "Kept both"),
                    name,
                    base_name(&path)
                )));
                Some((path, true))
            }
        },
    }
}

/// Watch a downloaded temp file and re-upload it to the remote whenever it
/// changes on disk (the "edit" flow).  Re-upload is routed back through the
/// worker's own command channel.  Stops when the channel closes or after a
/// generous idle window.
fn spawn_edit_watcher(self_tx: UnboundedSender<SftpCommand>, local: String, remote: String) {
    tokio::spawn(async move {
        let mtime = |p: &str| std::fs::metadata(p).ok().and_then(|m| m.modified().ok());
        let mut last = mtime(&local);
        // ~40 min of 2s polls; also exits early once the worker is gone.
        for _ in 0..1200 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if self_tx.is_closed() {
                break;
            }
            let cur = mtime(&local);
            if cur.is_some() && cur != last {
                last = cur;
                let _ = self_tx.send(SftpCommand::UploadEdited {
                    local: PathBuf::from(&local),
                    remote: remote.clone(),
                });
            }
        }
    });
}

// ---------------------------------------------------------------------------
// SFTP helpers
// ---------------------------------------------------------------------------

async fn cleanup_import_path(path: &Path) {
    let res = match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_dir() => tokio::fs::remove_dir_all(path).await,
        Ok(_) => tokio::fs::remove_file(path).await,
        Err(_) => Ok(()),
    };
    if let Err(e) = res {
        tracing::debug!("failed to clean temporary SFTP copy {:?}: {e}", path);
    }
}

async fn stage_remote_for_copy(
    sftp: &SftpSession,
    handle: &SftpLink,
    remote: &str,
    events: &UnboundedSender<SessionEvent>,
) -> Result<(PathBuf, PathBuf)> {
    let cleanup_root =
        std::env::temp_dir().join(format!("meatshell-remote-copy-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&cleanup_root)
        .await
        .with_context(|| format!("failed to create temp dir {}", cleanup_root.display()))?;

    let name = sanitize_filename(&base_name(remote));
    let local_path = cleanup_root.join(&name);
    let local_parent = cleanup_root.to_string_lossy().to_string();
    let is_dir = sftp
        .metadata(remote)
        .await
        .ok()
        .map(|m| (m.permissions.unwrap_or(0) & 0o170_000) == 0o040_000)
        .unwrap_or(false);
    let no_cancel = Arc::new(AtomicBool::new(false));
    let id = Uuid::new_v4().to_string();

    // Staging runs at full speed with no mtime preservation on purpose: it is
    // an internal hop (remote → local temp → remote), never the user-visible
    // transfer, and the queue's rate limit / mtime settings apply to the
    // upload leg that follows.
    if is_dir {
        tokio::fs::create_dir_all(&local_path)
            .await
            .with_context(|| format!("failed to create temp dir {}", local_path.display()))?;
        let empty = list_dir_impl(sftp, remote)
            .await
            .map(|entries| entries.is_empty())
            .unwrap_or(false);
        if !empty {
            download_dir(sftp, handle, remote, &local_parent, events, 0, false).await?;
        }
    } else {
        let local = local_path.to_string_lossy().to_string();
        download_impl(
            handle, remote, &local, &name, &id, events, &no_cancel, 0, false, false,
        )
        .await?;
    }

    Ok((local_path, cleanup_root))
}

/// Stage a remote entry into a local temp dir over the SCP wire-protocol
/// engine (remote → local hop), ready to be forwarded to another session's
/// `Upload` arm — the SCP twin of `stage_remote_for_copy`.
async fn stage_remote_for_scp_copy(
    handle: &SftpLink,
    remote: &str,
    events: &UnboundedSender<SessionEvent>,
    rate_kbps: u32,
    preserve_mtime: bool,
) -> Result<(PathBuf, PathBuf)> {
    let cleanup_root =
        std::env::temp_dir().join(format!("meatshell-remote-copy-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&cleanup_root)
        .await
        .with_context(|| format!("failed to create temp dir {}", cleanup_root.display()))?;

    let name = sanitize_filename(&base_name(remote));
    let local_path = cleanup_root.join(&name);
    let local_parent = cleanup_root.to_string_lossy().to_string();
    let no_cancel = Arc::new(AtomicBool::new(false));
    let id = Uuid::new_v4().to_string();
    if scp_remote_is_dir(&handle.handle, remote).await {
        tokio::fs::create_dir_all(&local_path)
            .await
            .with_context(|| format!("failed to create temp dir {}", local_path.display()))?;
        let empty = scp_list_dir(handle, remote)
            .await
            .map(|entries| entries.is_empty())
            .unwrap_or(false);
        if !empty {
            scp_download_dir(
                &handle.handle,
                remote,
                &local_parent,
                events,
                rate_kbps,
                preserve_mtime,
                &no_cancel,
            )
            .await?;
        }
    } else {
        let local = local_path.to_string_lossy().to_string();
        scp_download_single(
            &handle.handle,
            remote,
            &local,
            &name,
            &id,
            events,
            &no_cancel,
            rate_kbps,
            preserve_mtime,
            false,
        )
        .await?;
    }

    Ok((local_path, cleanup_root))
}

/// A friendlier message for a failed directory listing, calling out the common
/// permission-denied case explicitly rather than dumping the raw error (#112).
fn list_error_msg(path: &str, e: &impl std::fmt::Display) -> String {
    let raw = e.to_string();
    let low = raw.to_lowercase();
    if low.contains("permission") || low.contains("denied") {
        format!("{}: {}", t("权限不足,无法访问", "Permission denied"), path)
    } else {
        format!("{} {}: {}", t("无法访问", "Cannot open"), path, raw)
    }
}

async fn list_dir_impl(sftp: &SftpSession, path: &str) -> Result<Vec<RemoteEntry>> {
    let raw = sftp
        .read_dir(path)
        .await
        .with_context(|| format!("read_dir {path} failed"))?;

    let mut entries: Vec<RemoteEntry> = raw
        .into_iter()
        .filter(|e| {
            let n = e.file_name();
            n != "." && n != ".."
        })
        .map(|e| {
            let name = e.file_name().to_string();
            let full_path = format!("{}/{}", path.trim_end_matches('/'), name);
            let meta = e.metadata();
            // Determine if entry is a directory via Unix permission bits.
            let permissions = meta.permissions.unwrap_or(0);
            let is_dir = (permissions & 0o170_000) == 0o040_000;
            let size = meta.size.unwrap_or(0);
            let modified = meta.mtime.unwrap_or(0);
            RemoteEntry {
                name,
                full_path,
                is_dir,
                size,
                modified,
                mode: permissions & 0o7777,
                permissions_mode: permissions,
                uid: meta.uid,
                gid: meta.gid,
                owner: None,
                group: None,
                file_type: crate::ssh::file_type_from_mode(permissions).to_string(),
            }
        })
        .collect();

    // Sort: directories first, then files; both groups alphabetically.
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    Ok(entries)
}

/// Per-SFTP-connection cache of `uid`/`gid` → account name.
///
/// SFTP listings only carry numeric IDs. Resolving them costs a remote `getent`
/// round-trip, so the first paint of a directory used to flash raw numbers and
/// only later flip to names. A full passwd/group dump is prefetched at connect;
/// this cache is filled from that dump (and later targeted lookups) and
/// consulted on every listing in the same connection, so warm IDs paint as
/// names immediately. Unknown IDs stay blank in the UI rather than showing
/// raw numbers.
type SharedOwnerCache = Arc<Mutex<OwnerNameCache>>;

#[derive(Debug, Default)]
struct OwnerNameCache {
    owners: HashMap<u32, String>,
    groups: HashMap<u32, String>,
    /// IDs `getent` answered for with "no such account" (e.g. NFS zombie UIDs).
    /// Later listings skip the remote lookup for these instead of retrying.
    owner_miss: HashSet<u32>,
    group_miss: HashSet<u32>,
    /// A full `getent passwd` / `getent group` dump has been absorbed. After
    /// that, any ID not in the maps is a real miss and never needs `getent`.
    full_lookup_done: bool,
    /// `getent` is absent on this host (`command -v getent` → 127). Every
    /// unknown ID is then a real miss: later listings skip the probe entirely
    /// instead of re-running a command that can only fail (e.g. BusyBox/OpenWrt).
    getent_missing: bool,
}

/// IDs still needing a remote lookup after the cache was consulted.
#[derive(Debug, Default)]
struct OwnerLookupNeed {
    uids: Vec<u32>,
    gids: Vec<u32>,
}

impl OwnerLookupNeed {
    fn is_complete(&self) -> bool {
        self.uids.is_empty() && self.gids.is_empty()
    }
}

impl OwnerNameCache {
    /// Fill `entry.owner` / `entry.group` from the cache. Unknown IDs are
    /// collected into the returned need-list; misses (and any ID after a full
    /// passwd dump) count as already resolved — the UI leaves those cells blank
    /// and does not trigger `getent`.
    fn apply(&self, entries: &mut [RemoteEntry]) -> OwnerLookupNeed {
        let mut need_uids = std::collections::BTreeSet::new();
        let mut need_gids = std::collections::BTreeSet::new();
        for entry in entries.iter_mut() {
            if let Some(uid) = entry.uid {
                if let Some(name) = self.owners.get(&uid) {
                    entry.owner = Some(name.clone());
                } else if !self.owner_miss.contains(&uid)
                    && !self.full_lookup_done
                    && !self.getent_missing
                {
                    need_uids.insert(uid);
                }
            }
            if let Some(gid) = entry.gid {
                if let Some(name) = self.groups.get(&gid) {
                    entry.group = Some(name.clone());
                } else if !self.group_miss.contains(&gid)
                    && !self.full_lookup_done
                    && !self.getent_missing
                {
                    need_gids.insert(gid);
                }
            }
        }
        OwnerLookupNeed {
            uids: need_uids.into_iter().collect(),
            gids: need_gids.into_iter().collect(),
        }
    }

    /// Store a successful targeted `getent` parse. Requested IDs with no name
    /// become misses so later listings skip the remote lookup for them. Callers
    /// must **not** absorb a failed/timeout lookup — that would permanently pin
    /// blanks for IDs that simply had not been resolved yet.
    fn absorb_resolved(
        &mut self,
        need: &OwnerLookupNeed,
        owners: HashMap<u32, String>,
        groups: HashMap<u32, String>,
    ) {
        for (id, name) in owners {
            self.owner_miss.remove(&id);
            self.owners.insert(id, name);
        }
        for (id, name) in groups {
            self.group_miss.remove(&id);
            self.groups.insert(id, name);
        }
        for &uid in &need.uids {
            if !self.owners.contains_key(&uid) {
                self.owner_miss.insert(uid);
            }
        }
        for &gid in &need.gids {
            if !self.groups.contains_key(&gid) {
                self.group_miss.insert(gid);
            }
        }
    }

    /// Store a full passwd/group dump from the connect-time prefetch. After
    /// this, IDs absent from the maps are treated as misses (no more `getent`).
    fn absorb_full_lookup(&mut self, owners: HashMap<u32, String>, groups: HashMap<u32, String>) {
        for (id, name) in owners {
            self.owner_miss.remove(&id);
            self.owners.insert(id, name);
        }
        for (id, name) in groups {
            self.group_miss.remove(&id);
            self.groups.insert(id, name);
        }
        self.full_lookup_done = true;
    }

    /// `getent` is absent (exit 127). Unknown IDs are real misses from here
    /// on, so `apply` stops collecting them and the panel stops probing a
    /// command that can only fail.
    fn mark_getent_missing(&mut self) {
        self.getent_missing = true;
    }

    /// Forget the negative conclusions (absent `getent`, unknown IDs, absorbed
    /// full dump) so a later listing retries the remote probes. Keeps the
    /// resolved name maps — a warm directory still paints names instantly.
    /// Called on a manual directory refresh, so a host that gained `getent` or
    /// a new account mid-session stops showing blank owners until reconnect.
    fn reset_probes(&mut self) {
        self.owner_miss.clear();
        self.group_miss.clear();
        self.getent_missing = false;
        self.full_lookup_done = false;
    }
}

/// Split `getent passwd` / `getent group` output (see `resolve_owner_names`)
/// into id → name maps.
fn parse_owner_lookup_output(output: &str) -> (HashMap<u32, String>, HashMap<u32, String>) {
    let mut owners = HashMap::new();
    let mut groups = HashMap::new();
    let mut in_groups = false;
    for line in output.lines() {
        if line == "--MEATSHELL-GROUPS--" {
            in_groups = true;
            continue;
        }
        let fields: Vec<&str> = line.split(':').collect();
        if in_groups {
            if let Some((id, name)) = fields
                .get(2)
                .and_then(|id| id.parse::<u32>().ok())
                .zip(fields.first().copied())
            {
                groups.insert(id, name.to_string());
            }
        } else if let Some((id, name)) = fields
            .get(2)
            .and_then(|id| id.parse::<u32>().ok())
            .zip(fields.first().copied())
        {
            owners.insert(id, name.to_string());
        }
    }
    (owners, groups)
}

/// Fill owner/group names on `entries`: cache first, remote `getent` only for
/// IDs this connection has never seen. Returns the entries and whether the
/// remote lookup produced any new names (i.e. the caller should repaint).
async fn enrich_entries_with_owners(
    handle: &client::Handle<SftpClientHandler>,
    cache: &SharedOwnerCache,
    mut entries: Vec<RemoteEntry>,
) -> (Vec<RemoteEntry>, bool) {
    let need = lock_or_recover(&cache).apply(&mut entries);
    if need.is_complete() {
        // Warm cache (or nothing to resolve): names are already in place.
        return (entries, false);
    }
    match resolve_owner_names(handle, &need.uids, &need.gids).await {
        OwnerLookup::Failed => (entries, false),
        OwnerLookup::Missing => {
            lock_or_recover(&cache).mark_getent_missing();
            (entries, false)
        }
        OwnerLookup::Parsed { owners, groups } => {
            let gained = !owners.is_empty() || !groups.is_empty();
            {
                let mut cache = lock_or_recover(&cache);
                cache.absorb_resolved(&need, owners, groups);
                cache.apply(&mut entries);
            }
            (entries, gained)
        }
    }
}

/// Wait briefly for the connect-time full passwd/group dump so the first
/// listing can paint names instead of blanks. No-op once the dump has landed
/// (or after `max` elapsed — the listing then paints blanks and fills in later).
async fn wait_owner_prefetch(ready: &tokio::sync::watch::Receiver<bool>, max: Duration) {
    if *ready.borrow() {
        return;
    }
    let mut ready = ready.clone();
    let _ = tokio::time::timeout(max, ready.wait_for(|done| *done)).await;
}

/// Dump the entire remote passwd + group tables into `cache` (best-effort).
/// Runs at SFTP connect, overlapping the first `read_dir`, so names are usually
/// ready before the first paint. On timeout / missing `getent` the cache is
/// left untouched and later listings fall back to targeted per-ID lookups.
async fn prefetch_all_owner_names(
    handle: &client::Handle<SftpClientHandler>,
    cache: &SharedOwnerCache,
) {
    // No id arguments → getent prints the whole database. `command -v getent
    // || exit 127` keeps a host without getent from marking the dump done
    // with an empty map (which would blank every owner until reconnect).
    let command = "command -v getent >/dev/null 2>&1 || exit 127; \
getent passwd 2>/dev/null; printf '\\n--MEATSHELL-GROUPS--\\n'; getent group 2>/dev/null";
    // A full database dump can be slow on a big LDAP host, so it gets the long
    // bound; a wedged getent still surfaces as an error instead of hanging.
    let Ok((status, output)) = exec_remote_output(
        handle,
        command,
        super::common::SCP_LONG_CMD_IO_TIMEOUT,
    )
    .await
    else {
        return;
    };
    if status == 127 {
        lock_or_recover(&cache).mark_getent_missing();
        return;
    }
    let (owners, groups) = parse_owner_lookup_output(&output);
    lock_or_recover(&cache).absorb_full_lookup(owners, groups);
}

/// Publish a directory listing to the UI with owner names filled in.
///
/// Prefetch hit / warm cache → a single `SftpEntries` paint already carrying
/// names. Otherwise the first paint leaves owner/group blank (never raw uid/gid)
/// and a second paint lands once `getent` returns.
async fn publish_dir_entries(
    handle: &client::Handle<SftpClientHandler>,
    cache: &SharedOwnerCache,
    names_ready: &tokio::sync::watch::Receiver<bool>,
    events: &UnboundedSender<SessionEvent>,
    path: String,
    entries: Vec<RemoteEntry>,
) {
    wait_owner_prefetch(names_ready, Duration::from_millis(1500)).await;
    let mut entries = entries;
    let need = lock_or_recover(&cache).apply(&mut entries);
    if need.is_complete() {
        let _ = events.send(SessionEvent::SftpEntries { path, entries });
        return;
    }
    let _ = events.send(SessionEvent::SftpEntries {
        path: path.clone(),
        entries: entries.clone(),
    });
    let (entries, gained) = enrich_entries_with_owners(handle, cache, entries).await;
    if gained {
        let _ = events.send(SessionEvent::SftpEntries { path, entries });
    }
}

enum OwnerLookup {
    /// `getent` produced parseable output. IDs missing from the maps are real
    /// "no such account" answers and may be negative-cached.
    Parsed {
        owners: HashMap<u32, String>,
        groups: HashMap<u32, String>,
    },
    /// Timeout or transport failure — must not write misses.
    Failed,
    /// `getent` is not installed on the host (`command -v` → 127). The caller
    /// records this so later listings stop probing.
    Missing,
}

async fn resolve_owner_names(
    handle: &client::Handle<SftpClientHandler>,
    uids: &[u32],
    gids: &[u32],
) -> OwnerLookup {
    let mut uid_args: Vec<String> = uids.iter().map(u32::to_string).collect();
    uid_args.sort();
    uid_args.dedup();
    let mut gid_args: Vec<String> = gids.iter().map(u32::to_string).collect();
    gid_args.sort();
    gid_args.dedup();
    if uid_args.is_empty() && gid_args.is_empty() {
        return OwnerLookup::Parsed {
            owners: HashMap::new(),
            groups: HashMap::new(),
        };
    }

    // IDs are parsed as u32 above, so interpolating them cannot introduce shell
    // syntax. A single getent invocation handles all files in the directory.
    // `command -v getent || exit 127` keeps a host without getent from looking
    // like "every uid is a zombie" (which would negative-cache the whole map).
    let passwd = if uid_args.is_empty() {
        "true".to_string()
    } else {
        format!("getent passwd {} 2>/dev/null", uid_args.join(" "))
    };
    let group = if gid_args.is_empty() {
        "true".to_string()
    } else {
        format!("getent group {} 2>/dev/null", gid_args.join(" "))
    };
    let command = format!(
        "command -v getent >/dev/null 2>&1 || exit 127; {passwd}; printf '\\n--MEATSHELL-GROUPS--\\n'; {group}"
    );
    // A targeted lookup must be quick; 2s bounds a wedged getent (the original
    // busy-loop regression) without a separate outer timeout.
    let Ok((status, output)) = exec_remote_output(
        handle,
        &command,
        std::time::Duration::from_secs(2),
    )
    .await
    else {
        return OwnerLookup::Failed;
    };
    if status == 127 {
        return OwnerLookup::Missing;
    }
    let (owners, groups) = parse_owner_lookup_output(&output);
    OwnerLookup::Parsed { owners, groups }
}

/// List only the subdirectories of `path` (no files). Used to build the tree.
async fn list_dirs_only_impl(sftp: &SftpSession, path: &str) -> Result<Vec<(String, String)>> {
    let entries = list_dir_impl(sftp, path).await?;
    Ok(entries
        .into_iter()
        .filter(|e| e.is_dir)
        .map(|e| (e.name, e.full_path))
        .collect())
}

/// Emit a transfer-progress event.
fn emit_transfer(
    events: &UnboundedSender<SessionEvent>,
    id: &str,
    name: &str,
    is_upload: bool,
    transferred: u64,
    total: u64,
    state: u8,
    msg: &str,
) {
    let _ = events.send(SessionEvent::SftpTransfer {
        id: id.to_string(),
        name: name.to_string(),
        is_upload,
        transferred,
        total,
        sampled_at: Some(Instant::now()),
        state,
        msg: msg.to_string(),
    });
}

/// Paces a transfer to `rate_kbps` KiB/s in 100 ms windows; `0` is unlimited.
/// An overrun makes the caller sleep for exactly as long as the surplus takes
/// to be allowed — deliberately uncapped, so a small configured rate really
/// throttles instead of running at whatever the pipeline can deliver.
struct RatePacer {
    rate_bytes_per_sec: u64,
    window: Instant,
    window_bytes: u64,
}

impl RatePacer {
    fn new(rate_kbps: u32) -> Self {
        Self {
            rate_bytes_per_sec: (rate_kbps as u64).saturating_mul(1024),
            window: Instant::now(),
            window_bytes: 0,
        }
    }

    async fn pace(&mut self, bytes: u64, cancel: &AtomicBool) {
        if self.rate_bytes_per_sec == 0 {
            return;
        }
        self.window_bytes = self.window_bytes.saturating_add(bytes);
        if self.window.elapsed() < Duration::from_millis(100) {
            return;
        }
        let allowed = self.rate_bytes_per_sec / 10; // share of one 100 ms window
        if self.window_bytes > allowed {
            let over = self.window_bytes - allowed;
            let ms = (over as f64 / self.rate_bytes_per_sec as f64 * 1000.0) as u64;
            // Sleep in ≤1 s slices: at a low configured rate a single surplus
            // can otherwise park the task for tens of seconds without ever
            // re-reading the cancel flag.
            let mut remaining = Duration::from_millis(ms);
            while !remaining.is_zero() && !cancel.load(Ordering::Relaxed) {
                let slice = remaining.min(Duration::from_secs(1));
                tokio::time::sleep(slice).await;
                remaining -= slice;
            }
        }
        self.window = Instant::now();
        self.window_bytes = 0;
    }
}

/// Download a remote file over a dedicated, *pipelined* raw SFTP channel.
///
/// The high-level reader issues one READ and waits for the reply before the
/// next, so throughput is capped by the round-trip time (slow on any latent
/// link). Here we keep many READ requests in flight at once, each tagged with
/// its absolute offset so out-of-order completion is fine — mirroring
/// `upload_pipelined`.
///
/// Returns `Ok(true)` when the whole file was written, or `Ok(false)` if the
/// transfer was cancelled. In both the cancel and error cases the partial
/// local file is removed so no half-downloaded junk is left behind.

async fn download_impl(
    handle: &SftpLink,
    remote: &str,
    local: &str,
    name: &str,
    id: &str,
    events: &UnboundedSender<SessionEvent>,
    cancel: &Arc<AtomicBool>,
    rate_kbps: u32,
    preserve_mtime: bool,
    fail_if_exists: bool,
) -> Result<bool> {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    const CHUNK: usize = 32 * 1024;
    const MAX_INFLIGHT: usize = 32; // ~1 MB outstanding hides the RTT

    let stream = handle
        .elevation
        .open_stream(&handle.handle)
        .await
        .context("open sftp download channel")?;
    let raw = Arc::new(RawSftpSession::new(stream));
    raw.init().await.context("sftp download handshake")?;

    let remote_attrs = raw.stat(remote).await.ok();
    let total = remote_attrs
        .as_ref()
        .and_then(|a| a.attrs.size)
        .unwrap_or(0);
    let remote_mtime = remote_attrs.as_ref().and_then(|a| a.attrs.mtime);
    let fhandle = raw
        .open(remote, OpenFlags::READ, FileAttributes::default())
        .await
        .with_context(|| format!("open remote {remote}"))?
        .handle;
    let mut local_file = if fail_if_exists {
        // Atomic create-if-absent: no exists()-then-write race window, and no
        // file is left behind when the target already exists.
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(local)
            .await
            .with_context(|| {
                format!(
                    "{}: {}",
                    t("下载目标文件已存在", "download target already exists"),
                    local
                )
            })?
    } else {
        tokio::fs::File::create(local)
            .await
            .with_context(|| format!("create local {local}"))?
    };

    emit_transfer(events, id, name, false, 0, total, 0, "");

    // Token-bucket pacer: `rate_kbps` KiB/s, refilled per 100 ms window.
    let mut pacer = RatePacer::new(rate_kbps);

    let mut done: u64 = 0;
    let mut last = Instant::now();
    let mut err: Option<anyhow::Error> = None;
    let mut cancelled = false;

    if total > 0 {
        let mut next_off = 0u64;
        let mut inflight = FuturesUnordered::new();
        loop {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
            }
            // Top up the pipeline with fresh READ requests.
            while !cancelled && err.is_none() && next_off < total && inflight.len() < MAX_INFLIGHT {
                let off = next_off;
                let want = ((total - off) as usize).min(CHUNK);
                next_off += want as u64;
                let raw2 = raw.clone();
                let h = fhandle.clone();
                inflight.push(async move {
                    // Fill the whole chunk, coping with short reads.
                    let mut data = Vec::with_capacity(want);
                    let mut o = off;
                    let end = off + want as u64;
                    while o < end {
                        match raw2.read(h.clone(), o, (end - o) as u32).await {
                            Ok(d) => {
                                if d.data.is_empty() {
                                    break;
                                }
                                o += d.data.len() as u64;
                                data.extend_from_slice(&d.data);
                            }
                            Err(SftpError::Status(s)) if s.status_code == StatusCode::Eof => break,
                            Err(e) => return Err(anyhow!("read remote: {e}")),
                        }
                    }
                    Ok::<(u64, Vec<u8>), anyhow::Error>((off, data))
                });
            }
            if inflight.is_empty() {
                break;
            }
            match inflight.next().await {
                Some(Ok((off, data))) => {
                    if !data.is_empty() {
                        if let Err(e) = local_file.seek(std::io::SeekFrom::Start(off)).await {
                            err = Some(anyhow!("seek local: {e}"));
                        } else if let Err(e) = local_file.write_all(&data).await {
                            err = Some(anyhow!("write local: {e}"));
                        } else {
                            done += data.len() as u64;
                        }
                    }
                    pacer.pace(data.len() as u64, cancel).await;
                    if last.elapsed() >= Duration::from_millis(150) {
                        last = Instant::now();
                        emit_transfer(events, id, name, false, done, total, 0, "");
                    }
                }
                Some(Err(e)) => err = Some(e),
                None => {}
            }
            if (cancelled || err.is_some()) && inflight.is_empty() {
                break;
            }
        }
    } else {
        // Unknown / zero size: serial drain until EOF (rare; keeps correctness).
        let mut off = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            match raw.read(fhandle.clone(), off, CHUNK as u32).await {
                Ok(d) => {
                    if d.data.is_empty() {
                        break;
                    }
                    local_file
                        .write_all(&d.data)
                        .await
                        .context("write local file")?;
                    off += d.data.len() as u64;
                    done += d.data.len() as u64;
                    pacer.pace(d.data.len() as u64, cancel).await;
                    if last.elapsed() >= Duration::from_millis(150) {
                        last = Instant::now();
                        emit_transfer(events, id, name, false, done, done, 0, "");
                    }
                }
                Err(SftpError::Status(s)) if s.status_code == StatusCode::Eof => break,
                Err(e) => {
                    err = Some(anyhow!("read remote: {e}"));
                    break;
                }
            }
        }
    }

    let _ = raw.close(fhandle).await;

    if let Some(e) = err {
        drop(local_file);
        let _ = tokio::fs::remove_file(local).await;
        return Err(e);
    }
    if cancelled {
        drop(local_file);
        let _ = tokio::fs::remove_file(local).await;
        emit_transfer(
            events,
            id,
            name,
            false,
            done,
            total,
            4,
            t("已取消", "Cancelled"),
        );
        return Ok(false);
    }
    local_file.flush().await.context("flush local file")?;
    if preserve_mtime {
        if let Some(secs) = remote_mtime {
            drop(local_file);
            let when =
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64);
            // `set_modified` is a blocking syscall; run it off the async
            // worker so a slow filesystem can't stall other transfers.
            let path = local.to_string();
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path) {
                    let _ = f.set_modified(when);
                }
            })
            .await;
        }
    }
    emit_transfer(events, id, name, false, done, total.max(done), 1, "");
    Ok(true)
}

/// Download one remote entry (a file or a whole folder) into an existing local
/// folder — the dual-pane view's "download to the folder on the left".
///
/// Unlike the single-entry `Download` command above, an empty folder is still
/// created locally (the pane shows what is on the server, and "the folder moved
/// over" is the expected result there), and the conflict policy applies to
/// every entry of a multi-selection in the same way.
#[allow(clippy::too_many_arguments)]
async fn download_one_into(
    sftp: &SftpSession,
    handle: &SftpLink,
    remote: &str,
    local_dir: &str,
    conflict: DownloadConflict,
    events: &UnboundedSender<SessionEvent>,
    cancel: &Arc<AtomicBool>,
    id: &str,
    rate_kbps: u32,
    preserve_mtime: bool,
) -> Result<bool> {
    let is_dir = sftp
        .metadata(remote)
        .await
        .ok()
        .map(|m| (m.permissions.unwrap_or(0) & 0o170_000) == 0o040_000)
        .unwrap_or(false);
    if is_dir {
        let dirname = base_name(remote);
        let _ = events.send(SessionEvent::SftpStatus(format!(
            "{} {}/...",
            t("下载文件夹", "Downloading folder"),
            dirname
        )));
        download_dir(
            sftp,
            handle,
            remote,
            local_dir,
            events,
            rate_kbps,
            preserve_mtime,
        )
        .await?;
        return Ok(true);
    }
    // The server controls the name, so sanitize it before it becomes a local
    // path (#26) — same rule as the regular download path.
    let filename = sanitize_filename(&base_name(remote));
    let requested = download_target_path(remote, local_dir);
    let local_path = match conflict {
        DownloadConflict::Absent => requested,
        DownloadConflict::Replace => requested,
        DownloadConflict::KeepBoth => available_download_path(&requested),
        // Refuse-if-exists is enforced by download_impl's fail_if_exists flag.
        DownloadConflict::Fail => requested,
    };
    download_impl(
        handle,
        remote,
        &local_path.to_string_lossy(),
        &filename,
        id,
        events,
        cancel,
        rate_kbps,
        preserve_mtime,
        conflict == DownloadConflict::Fail,
    )
    .await
}

/// Recursively download a remote directory tree under `local_parent` (#50).
/// Iterative (work-stack) rather than a boxed async recursion: each remote dir
/// is mirrored to a sanitized local name, then its files are downloaded with the
/// same per-file pipeline used for single downloads. Names are sanitized (#26)
/// so a hostile server can't escape the chosen folder.
async fn download_dir(
    sftp: &SftpSession,
    handle: &SftpLink,
    remote_root: &str,
    local_parent: &str,
    events: &UnboundedSender<SessionEvent>,
    rate_kbps: u32,
    preserve_mtime: bool,
) -> Result<()> {
    // Folder transfers aren't individually cancellable from the UI; a throwaway
    // never-set flag satisfies download_impl's signature.
    let no_cancel = Arc::new(AtomicBool::new(false));
    let root_name = sanitize_filename(&base_name(remote_root));
    let root_local = format!("{}/{}", local_parent.trim_end_matches('/'), root_name);
    // (remote_dir, local_dir) pairs still to mirror.
    let mut stack = vec![(remote_root.trim_end_matches('/').to_string(), root_local)];
    while let Some((rdir, ldir)) = stack.pop() {
        tokio::fs::create_dir_all(&ldir)
            .await
            .with_context(|| format!("create local dir {ldir}"))?;
        for entry in list_dir_impl(sftp, &rdir).await? {
            if entry.is_dir {
                let child_local = format!("{}/{}", ldir, sanitize_filename(&entry.name));
                stack.push((entry.full_path, child_local));
            } else {
                let fname = sanitize_filename(&entry.name);
                let lpath = format!("{}/{}", ldir, fname);
                let id = Uuid::new_v4().to_string();
                download_impl(
                    handle,
                    &entry.full_path,
                    &lpath,
                    &fname,
                    &id,
                    events,
                    &no_cancel,
                    rate_kbps,
                    preserve_mtime,
                    false,
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// Recursively remove a remote directory tree (#50 follow-up).
///
/// A plain `remove_dir` only deletes an *empty* directory, so deleting an
/// uploaded folder failed. We BFS to discover every sub-directory (deleting
/// files as we go), then rmdir them deepest-first.
async fn remove_dir_recursive(sftp: &SftpSession, root: &str) -> Result<()> {
    let mut all_dirs = vec![root.trim_end_matches('/').to_string()];
    let mut i = 0;
    while i < all_dirs.len() {
        let d = all_dirs[i].clone();
        i += 1;
        for entry in list_dir_impl(sftp, &d).await? {
            if entry.is_dir {
                all_dirs.push(entry.full_path);
            } else {
                sftp.remove_file(&entry.full_path)
                    .await
                    .map_err(|e| anyhow::anyhow!("remove file {}: {e}", entry.full_path))?;
            }
        }
    }
    // BFS discovered parents before children, so reversing gives deepest-first.
    for d in all_dirs.iter().rev() {
        sftp.remove_dir(d)
            .await
            .map_err(|e| anyhow::anyhow!("remove dir {d}: {e}"))?;
    }
    Ok(())
}

/// Recursively upload a local directory tree into `remote_parent` (#50).
///
/// Iterative work-stack: mirror each local dir to the remote (create_dir, whose
/// "already exists" error is ignored), then upload its files with the pipelined
/// path. Symlinks and other special files are skipped.
/// First remote name that does not exist yet in `dir`: the name itself when it
/// is free, otherwise `name (1)`, `name (2)`, … (the "keep both" upload policy).
async fn available_remote_name(sftp: &SftpSession, dir: &str, name: &str) -> String {
    let dir = dir.trim_end_matches('/');
    let mut candidate = name.to_string();
    for n in 1..=99u32 {
        if sftp.metadata(&format!("{dir}/{candidate}")).await.is_err() {
            return candidate;
        }
        candidate = numbered_name(name, n);
    }
    candidate
}

async fn upload_dir(
    handle: &SftpLink,
    sftp: &SftpSession,
    local_root: &Path,
    remote_parent: &str,
    remote_name: &str,
    events: &UnboundedSender<SessionEvent>,
    rate_kbps: u32,
    preserve_mtime: bool,
) -> Result<()> {
    // Folder uploads aren't individually cancellable from the UI; a throwaway
    // never-set flag satisfies upload_pipelined's signature.
    let no_cancel = Arc::new(AtomicBool::new(false));
    let remote_root = format!("{}/{}", remote_parent.trim_end_matches('/'), remote_name);
    let mut stack = vec![(local_root.to_path_buf(), remote_root)];
    while let Some((ldir, rdir)) = stack.pop() {
        // Best-effort mkdir; an error usually just means the dir already exists.
        let _ = sftp.create_dir(&rdir).await;
        let mut rd = tokio::fs::read_dir(&ldir)
            .await
            .with_context(|| format!("read local dir {}", ldir.display()))?;
        while let Some(entry) = rd.next_entry().await.context("read dir entry")? {
            let lpath = entry.path();
            let name = local_file_name_utf8(&lpath)?;
            let rchild = format!("{}/{}", rdir, name);
            let ft = entry.file_type().await.context("file type")?;
            if ft.is_dir() {
                stack.push((lpath, rchild));
            } else if ft.is_file() {
                let id = Uuid::new_v4().to_string();
                upload_pipelined(
                    handle,
                    sftp,
                    &lpath,
                    &rchild,
                    &name,
                    &id,
                    events,
                    &no_cancel,
                    rate_kbps,
                    preserve_mtime,
                    false,
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// Create the remote end of an upload and return its handle and the final
/// path it was written to.
///
/// With `no_clobber` the create carries `EXCLUDE` (SSH_FXF_EXCL) so the server
/// decides "target is free" atomically: the duplicate probe ran earlier in the
/// worker, so another client could still have created the file in the meantime
/// and a plain `CREATE|WRITE|TRUNCATE` would have silently overwritten it.
/// Servers that do not honour `EXCLUDE` fail in the same way, so after an
/// error a stat on the target separates "someone else created it" (renumber
/// and retry) from "not supported" (one truncating create) instead of trusting
/// a status code. Only an explicit no-such-file stat proves the target is
/// still absent: any other stat outcome (permission denied, timeout, I/O)
/// could hide a live target, so it aborts rather than risk a truncating
/// clobber.
async fn open_upload_target(
    raw: &RawSftpSession,
    sftp: &SftpSession,
    remote: &str,
    no_clobber: bool,
) -> anyhow::Result<(String, String)> {
    const MAX_RENUMBER: u32 = 10;

    let mut target = remote.to_string();
    let mut exclusive = no_clobber;
    let mut renumbered = 0u32;
    loop {
        let flags = if exclusive {
            OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::EXCLUDE
        } else {
            OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE
        };
        match raw.open(&target, flags, FileAttributes::default()).await {
            Ok(h) => return Ok((h.handle, target.clone())),
            Err(open_error) if exclusive => match raw.stat(&target).await {
                Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => {
                    // Still absent: the server refused the flag itself. One
                    // plain create is then as safe as the probe said.
                    tracing::warn!(
                        "sftp server rejected SSH_FXF_EXCL for {target}; \
                         falling back to a truncating create"
                    );
                    exclusive = false;
                    continue;
                }
                Ok(_) => {
                    renumbered += 1;
                    if renumbered > MAX_RENUMBER {
                        return Err(anyhow::anyhow!(
                            "keep-both target {target} was re-created {MAX_RENUMBER} times: {open_error}"
                        ));
                    }
                    target = available_remote_path(sftp, &target).await;
                }
                Err(stat_error) => {
                    return Err(anyhow::anyhow!(
                        "keep-both create of {target} failed: {open_error}; \
                         target state undetermined: {stat_error}"
                    ));
                }
            },
            Err(e) => return Err(anyhow::anyhow!("{e}")),
        }
    }
}

/// Pipelined SFTP upload (#16).
///
/// The high-level `SftpSession`/`File` writes one chunk and waits for the
/// server's ack before sending the next, so throughput is capped by the
/// round-trip time (~15x slower than scp on a latent link).  Here we open a
/// dedicated raw SFTP channel and keep many WRITE requests in flight at once
/// (each tagged with its absolute offset, so out-of-order completion is fine),
/// which hides the latency and brings us within a single order of magnitude of
/// native scp.
async fn upload_pipelined(
    handle: &SftpLink,
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    name: &str,
    id: &str,
    events: &UnboundedSender<SessionEvent>,
    cancel: &Arc<AtomicBool>,
    rate_kbps: u32,
    preserve_mtime: bool,
    no_clobber: bool,
) -> Result<bool> {
    use tokio::io::AsyncReadExt;

    const CHUNK: usize = 32 * 1024; // safe SFTP write size
    const MAX_INFLIGHT: usize = 32; // ~1 MB of outstanding writes hides the RTT

    // Upload-side mtime would need a remote SETSTAT; the raw SFTP API for it is
    // not exercised elsewhere in this crate, so we keep the flag for symmetry but
    // only honour download-side mtime for now.
    let _ = preserve_mtime;

    let total = tokio::fs::metadata(local)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let mut local_file = tokio::fs::File::open(local)
        .await
        .with_context(|| format!("open local {}", local.display()))?;

    // Dedicated raw SFTP channel for the transfer (keeps the browse session
    // responsive and lets us issue concurrent WRITE requests).
    let stream = handle
        .elevation
        .open_stream(&handle.handle)
        .await
        .context("open sftp upload channel")?;
    let raw = Arc::new(RawSftpSession::new(stream));
    raw.init().await.context("sftp upload handshake")?;

    // With keep-both a renumber can land the upload on a different name than
    // `remote`, so the cleanup and the terminal status lines below track the
    // actually-created path (in-progress lines still show the source name).
    let (fhandle, final_path) = open_upload_target(raw.as_ref(), sftp, remote, no_clobber)
        .await
        .with_context(|| format!("create remote {remote}"))?;
    let final_name = base_name(&final_path);

    emit_transfer(events, id, name, true, 0, total, 0, "");

    // Token-bucket pacer (see download_impl): rate_kbps KiB/s per 100 ms window.
    let mut pacer = RatePacer::new(rate_kbps);

    let mut offset: u64 = 0;
    let mut done: u64 = 0;
    let mut last = Instant::now();
    let mut eof = false;
    let mut err: Option<anyhow::Error> = None;
    let mut cancelled = false;
    let mut inflight = FuturesUnordered::new();

    while !eof || !inflight.is_empty() {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            eof = true; // stop reading more; drain what's in flight
        }
        // Top up the pipeline with fresh WRITE requests.
        while !eof && inflight.len() < MAX_INFLIGHT {
            let mut buf = vec![0u8; CHUNK];
            match local_file.read(&mut buf).await {
                Ok(0) => eof = true,
                Ok(n) => {
                    buf.truncate(n);
                    let off = offset;
                    offset += n as u64;
                    let raw2 = raw.clone();
                    let h = fhandle.clone();
                    inflight.push(async move { raw2.write(h, off, buf).await.map(|_| n as u64) });
                }
                Err(e) => {
                    err = Some(anyhow!("read local file: {e}"));
                    eof = true;
                }
            }
        }
        match inflight.next().await {
            Some(Ok(n)) => {
                done += n;
                pacer.pace(n, cancel).await;
                if last.elapsed() >= Duration::from_millis(150) {
                    last = Instant::now();
                    emit_transfer(events, id, name, true, done, total, 0, "");
                }
            }
            Some(Err(e)) => {
                err = Some(anyhow!("write remote file: {e}"));
                eof = true; // stop reading more
            }
            None => {}
        }
        if err.is_some() {
            break;
        }
    }

    let _ = raw.close(fhandle).await;
    if let Some(e) = err {
        // Drop the partial remote file so a failed upload leaves no junk.
        // A keep-both renumber wrote to `final_path`, not `remote` — removing
        // the original would delete a file another client created.
        let _ = raw.remove(&final_path).await;
        return Err(e);
    }
    if cancelled {
        // Remove the half-written remote file on cancel (#100).
        let _ = raw.remove(&final_path).await;
        emit_transfer(
            events,
            id,
            final_name.as_str(),
            true,
            done,
            total,
            4,
            t("已取消", "Cancelled"),
        );
        return Ok(false);
    }
    emit_transfer(
        events,
        id,
        final_name.as_str(),
        true,
        done,
        total.max(done),
        1,
        "",
    );
    Ok(true)
}

// ---------------------------------------------------------------------------
// russh client handler — verifies the host key against known_hosts, reusing the
// shell session's prompt path (#109-5). The UI de-duplicates by host:port, so a
// fresh host confirmed for the shell won't prompt again for SFTP.
// ---------------------------------------------------------------------------

struct SftpClientHandler {
    host: String,
    port: u16,
    events: UnboundedSender<SessionEvent>,
    host_key_wait: crate::ssh::HostKeyWait,
}

/// The SFTP connection plus how its SFTP channels are opened (#438). Derefs to
/// the SSH handle, so helpers that only run plain commands take it as is.
struct SftpLink {
    handle: client::Handle<SftpClientHandler>,
    elevation: Elevation,
}

impl std::ops::Deref for SftpLink {
    type Target = client::Handle<SftpClientHandler>;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl crate::ssh::HandshakeHandler for SftpClientHandler {
    fn host_key_wait(&self) -> &crate::ssh::HostKeyWait {
        &self.host_key_wait
    }
}

fn sftp_handler(session: &Session, events: &UnboundedSender<SessionEvent>) -> SftpClientHandler {
    SftpClientHandler {
        host: session.host.clone(),
        port: session.port,
        events: events.clone(),
        host_key_wait: crate::ssh::HostKeyWait::default(),
    }
}

#[async_trait]
impl Handler for SftpClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(crate::ssh::verify_host_key(
            &self.host,
            self.port,
            server_public_key,
            &self.events,
            &self.host_key_wait,
        )
        .await)
    }

    async fn data(
        &mut self,
        _channel: russh::ChannelId,
        _data: &[u8],
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

// Keep format helpers and RemoteTreeNode imports live.
const _: fn() = || {
    let _ = format_size(0);
    let _ = format_mtime(0);
    let _: RemoteTreeNode;
};

#[cfg(test)]
mod transfer_event_tests {
    use super::*;
    use crate::sftp::DragTransfer;

    #[test]
    fn concurrent_drag_events_stay_ordered_through_completion() {
        let progress = DragTransfer::new("drag".into(), "files".into(), 66);
        let (events, mut received) = mpsc::unbounded_channel();
        let ready = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let (progress, events, ready) = (&progress, &events, &ready);
                scope.spawn(move || {
                    progress.done.fetch_add(1, Ordering::Relaxed);
                    progress.emit_progress(events, true);
                    ready.wait();
                    for _ in 0..32 {
                        progress.done.fetch_add(1, Ordering::Relaxed);
                        progress.emit_progress(events, true);
                    }
                });
            }
            ready.wait();
            progress.emit_final(&events);
        });
        progress.emit_final(&events);
        progress.emit_progress(&events, true);

        let samples: Vec<_> = std::iter::from_fn(|| received.try_recv().ok())
            .map(|event| match event {
                SessionEvent::SftpTransfer { state, transferred, sampled_at, .. } =>
                    (state, transferred, sampled_at.expect("SFTP source time")),
                _ => panic!("unexpected event"),
            })
            .collect();
        assert!(samples.len() >= 3);
        assert_eq!(samples.last().unwrap().0, TRANSFER_DONE);
        assert!(samples[..samples.len() - 1].iter().all(|s| s.0 == TRANSFER_ACTIVE));
        assert!(samples.windows(2).all(|s| s[0].1 <= s[1].1 && s[0].2 <= s[1].2));
    }
}

#[cfg(test)]
mod fallback_verdict_tests {
    use super::*;

    // The verdict map is a process-global static, so each test uses its own
    // key: Rust runs tests in parallel, and a shared key would let one test's
    // clear()/backdate() race another's assertions.
    const KEY_FRESH: &str = "fresh.example.com:22/a";
    const KEY_EXPIRED: &str = "expired.example.com:22/b";
    const KEY_CLEAR: &str = "clear.example.com:22/c";

    #[test]
    fn fresh_verdict_is_known() {
        scp_fallback_clear(KEY_FRESH);
        scp_fallback_mark(KEY_FRESH);
        assert!(scp_fallback_known(KEY_FRESH));
        scp_fallback_clear(KEY_FRESH);
    }

    #[test]
    fn expired_verdict_is_not_known_and_is_dropped() {
        scp_fallback_clear(KEY_EXPIRED);
        scp_fallback_mark(KEY_EXPIRED);
        // Backdate the verdict past the TTL, as if it were recorded long ago.
        lock_or_recover(scp_fallback_verdict()).insert(
            KEY_EXPIRED.to_string(),
            Instant::now() - SCP_FALLBACK_TTL - Duration::from_secs(1),
        );
        assert!(!scp_fallback_known(KEY_EXPIRED));
        // The expired entry is pruned on read, so the map does not grow stale.
        assert!(!lock_or_recover(scp_fallback_verdict()).contains_key(KEY_EXPIRED));
    }

    #[test]
    fn clear_removes_a_known_verdict() {
        scp_fallback_clear(KEY_CLEAR);
        scp_fallback_mark(KEY_CLEAR);
        scp_fallback_clear(KEY_CLEAR);
        assert!(!scp_fallback_known(KEY_CLEAR));
    }
}

#[cfg(test)]
mod sanitize_tests {
    use super::{
        available_download_path, chmod_attributes, download_target_path, external_edit_local_name,
        sanitize_filename, validate_editor_text, EditorTextRejection, MAX_BUILTIN_EDITOR_BYTES,
        MAX_BUILTIN_EDITOR_LINES, MAX_BUILTIN_EDITOR_LINE_BYTES,
    };

    #[test]
    fn plain_names_pass_through() {
        assert_eq!(sanitize_filename("report.txt"), "report.txt");
        assert_eq!(sanitize_filename("my-file_v2.tar.gz"), "my-file_v2.tar.gz");
        assert_eq!(sanitize_filename("数据.csv"), "数据.csv");
        // Unix dotfiles keep their leading dot.
        assert_eq!(sanitize_filename(".bashrc"), ".bashrc");
    }

    #[test]
    fn strips_path_separators_and_traversal() {
        // base_name already strips dirs, but sanitize is defence-in-depth: the
        // result must never keep a separator that could escape the target dir.
        assert_eq!(sanitize_filename("a/b\\c"), "a_b_c");
        let traversal = sanitize_filename("../../etc/passwd");
        assert!(!traversal.contains('/') && !traversal.contains('\\'));
        let win = sanitize_filename("..\\..\\Windows\\System32");
        assert!(!win.contains('/') && !win.contains('\\'));
    }

    #[test]
    fn replaces_shell_and_windows_special_chars() {
        assert_eq!(sanitize_filename("foo&calc.exe"), "foo_calc.exe");
        assert_eq!(sanitize_filename("a|b>c<d:e?f*g"), "a_b_c_d_e_f_g");
        assert_eq!(sanitize_filename("$(whoami)"), "_(whoami)");
        assert_eq!(sanitize_filename("a`b'c"), "a_b_c");
    }

    #[test]
    fn trims_whitespace_and_trailing_dots() {
        assert_eq!(sanitize_filename("   spaced.txt  "), "spaced.txt");
        assert_eq!(sanitize_filename("name..."), "name");
        // control chars become underscores, not trimmed
        assert_eq!(sanitize_filename("a\tb"), "a_b");
    }

    #[test]
    fn neutralises_windows_reserved_device_names() {
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("nul"), "_nul");
        assert_eq!(sanitize_filename("COM1"), "_COM1");
        assert_eq!(sanitize_filename("LPT9.txt"), "_LPT9.txt"); // reserved even with ext
        assert_eq!(sanitize_filename("Aux.log"), "_Aux.log");
        // Not reserved: a name that merely starts with the same letters.
        assert_eq!(sanitize_filename("console.txt"), "console.txt");
        assert_eq!(sanitize_filename("COM10"), "COM10");
    }

    #[test]
    fn empty_or_all_bad_falls_back() {
        assert_eq!(sanitize_filename(""), "file");
        assert_eq!(sanitize_filename("   "), "file");
        assert_eq!(sanitize_filename("..."), "file");
    }

    #[test]
    fn external_edits_include_the_host_in_the_local_name() {
        assert_eq!(
            external_edit_local_name("192.168.1.10", "nginx.conf", "edit-1"),
            "192.168.1.10_edit-1_nginx.conf"
        );
        assert_ne!(
            external_edit_local_name("server-a", "nginx.conf", "edit-1"),
            external_edit_local_name("server-b", "nginx.conf", "edit-1")
        );
        assert_ne!(
            external_edit_local_name("server-a", "nginx.conf", "edit-1"),
            external_edit_local_name("server-a", "nginx.conf", "edit-2")
        );
    }

    #[test]
    fn keep_both_uses_the_first_available_numbered_name() {
        let dir =
            std::env::temp_dir().join(format!("meatshell-download-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let requested = download_target_path("/remote/report.txt", dir.to_str().unwrap());
        std::fs::write(&requested, b"old").unwrap();
        std::fs::write(dir.join("report (1).txt"), b"older").unwrap();

        assert_eq!(
            available_download_path(&requested),
            dir.join("report (2).txt")
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn chmod_attributes_do_not_reset_file_size_or_timestamps() {
        let attrs = chmod_attributes(0o640);
        assert_eq!(attrs.permissions, Some(0o640));
        assert_eq!(attrs.size, None);
        assert_eq!(attrs.atime, None);
        assert_eq!(attrs.mtime, None);
    }

    #[test]
    fn double_click_text_detection_keeps_text_and_rejects_binaries() {
        // #493: the same check decides between "open in editor" and "download".
        for text in [
            "plain\n",
            "tabs\tand\r\nCRLF",
            "中文 UTF-8 ✓\n",
            "#!/bin/sh\necho hi\n",
            "",
        ] {
            assert!(!super::has_control_bytes(text.as_bytes()), "{text:?}");
            assert_eq!(
                validate_editor_text(text.as_bytes().to_vec()),
                Ok(text.to_string())
            );
        }
        for binary in [
            &b"\x89PNG\r\n\x1a\n"[..],
            &b"\x7fELF\x02\x01"[..],
            &b"PK\x03\x04"[..],
            &b"text\0with nul"[..],
        ] {
            assert!(super::has_control_bytes(binary), "{binary:?}");
            assert_eq!(
                validate_editor_text(binary.to_vec()),
                Err(EditorTextRejection::Binary)
            );
        }
        // GBK text has no control bytes but is not UTF-8: downloaded, not garbled.
        let gbk = vec![0xc4, 0xe3, 0xba, 0xc3];
        assert!(!super::has_control_bytes(&gbk));
        assert_eq!(
            validate_editor_text(gbk),
            Err(EditorTextRejection::InvalidUtf8)
        );
    }

    #[test]
    fn editor_rejects_content_over_the_safe_byte_limit() {
        let bytes = vec![b'a'; MAX_BUILTIN_EDITOR_BYTES + 1];
        assert_eq!(
            validate_editor_text(bytes),
            Err(EditorTextRejection::TooLarge)
        );
    }

    #[test]
    fn editor_rejects_pathological_text_layouts() {
        let lines = vec![b'\n'; MAX_BUILTIN_EDITOR_LINES];
        assert_eq!(
            validate_editor_text(lines),
            Err(EditorTextRejection::TooManyLines)
        );
        let line = vec![b'a'; MAX_BUILTIN_EDITOR_LINE_BYTES + 1];
        assert_eq!(
            validate_editor_text(line),
            Err(EditorTextRejection::LineTooLong)
        );
    }

    #[test]
    fn editor_line_limit_is_ten_thousand_including_trailing_empty_line() {
        for newline in ["\n", "\r\n"] {
            let exact = newline.repeat(9_999);
            assert_eq!(
                validate_editor_text(exact.as_bytes().to_vec()),
                Ok(exact.clone())
            );
            let over = format!("{exact}{newline}");
            assert_eq!(
                validate_editor_text(over.into_bytes()),
                Err(EditorTextRejection::TooManyLines)
            );
        }
    }

    #[test]
    fn editor_accepts_regular_utf8_text_within_limits() {
        assert_eq!(
            validate_editor_text("第一行\nsecond line\n".as_bytes().to_vec()),
            Ok("第一行\nsecond line\n".into())
        );
    }
}

#[cfg(test)]
mod owner_cache_tests {
    use super::*;

    fn entry(uid: Option<u32>, gid: Option<u32>) -> RemoteEntry {
        RemoteEntry {
            name: "f".into(),
            full_path: "/f".into(),
            is_dir: false,
            size: 0,
            modified: 0,
            mode: 0o644,
            permissions_mode: 0o100644,
            uid,
            gid,
            owner: None,
            group: None,
            file_type: "file".into(),
        }
    }

    #[test]
    fn apply_fills_names_from_cache_and_reports_complete() {
        let mut cache = OwnerNameCache::default();
        cache.owners.insert(1000, "www-data".into());
        cache.groups.insert(1000, "www-data".into());

        let mut entries = vec![entry(Some(1000), Some(1000))];
        let need = cache.apply(&mut entries);

        assert!(need.is_complete());
        assert_eq!(entries[0].owner.as_deref(), Some("www-data"));
        assert_eq!(entries[0].group.as_deref(), Some("www-data"));
    }

    #[test]
    fn apply_requests_only_unknown_ids() {
        let mut cache = OwnerNameCache::default();
        cache.owners.insert(0, "root".into());

        let mut entries = vec![entry(Some(0), Some(33)), entry(Some(1000), Some(33))];
        let need = cache.apply(&mut entries);

        assert!(!need.is_complete());
        assert_eq!(need.uids, vec![1000]);
        assert_eq!(need.gids, vec![33]);
        assert_eq!(entries[0].owner.as_deref(), Some("root"));
        assert_eq!(entries[0].group.as_deref(), None);
        assert_eq!(entries[1].owner.as_deref(), None);
    }

    #[test]
    fn miss_counts_as_resolved_so_getent_is_skipped() {
        let mut cache = OwnerNameCache::default();
        cache.owner_miss.insert(4242);
        cache.group_miss.insert(4242);

        let mut entries = vec![entry(Some(4242), Some(4242))];
        let need = cache.apply(&mut entries);

        assert!(need.is_complete());
        assert_eq!(entries[0].owner, None);
        assert_eq!(entries[0].group, None);
    }

    #[test]
    fn full_lookup_treats_unknown_ids_as_blanks_without_remote() {
        let mut cache = OwnerNameCache::default();
        cache.absorb_full_lookup(
            HashMap::from([(0, "root".to_string())]),
            HashMap::from([(0, "root".to_string())]),
        );

        let mut entries = vec![entry(Some(0), Some(0)), entry(Some(9999), Some(9999))];
        let need = cache.apply(&mut entries);

        // 0 resolves from the dump; 9999 is a real miss after a full dump and
        // must NOT queue another getent (UI stays blank).
        assert!(need.is_complete());
        assert_eq!(entries[0].owner.as_deref(), Some("root"));
        assert_eq!(entries[1].owner, None);
        assert_eq!(entries[1].group, None);
    }

    #[test]
    fn getent_missing_treats_all_ids_as_blanks_without_remote() {
        let mut cache = OwnerNameCache::default();
        cache.mark_getent_missing();

        let mut entries = vec![entry(Some(0), Some(0)), entry(Some(9999), Some(9999))];
        let need = cache.apply(&mut entries);

        // With getent absent, no ID queues another probe: cells stay blank.
        assert!(need.is_complete());
        assert_eq!(entries[0].owner, None);
        assert_eq!(entries[1].owner, None);
        assert_eq!(entries[1].group, None);
    }

    #[test]
    fn absorb_resolved_stores_names_and_negative_caches_missing_ids() {
        let mut cache = OwnerNameCache::default();
        let need = OwnerLookupNeed {
            uids: vec![0, 9999],
            gids: vec![0, 9999],
        };
        let mut owners = HashMap::new();
        owners.insert(0, "root".to_string());
        let mut groups = HashMap::new();
        groups.insert(0, "root".to_string());
        cache.absorb_resolved(&need, owners, groups);

        assert_eq!(cache.owners.get(&0).map(String::as_str), Some("root"));
        assert!(cache.owner_miss.contains(&9999));
        assert_eq!(cache.groups.get(&0).map(String::as_str), Some("root"));
        assert!(cache.group_miss.contains(&9999));

        let mut entries = vec![entry(Some(0), Some(0)), entry(Some(9999), Some(9999))];
        let need = cache.apply(&mut entries);
        assert!(need.is_complete());
    }

    #[test]
    fn absorb_replaces_miss_when_name_appears_later() {
        let mut cache = OwnerNameCache::default();
        cache.owner_miss.insert(7);
        cache.absorb_resolved(
            &OwnerLookupNeed {
                uids: vec![7],
                gids: vec![],
            },
            HashMap::from([(7, "alice".to_string())]),
            HashMap::new(),
        );
        assert!(!cache.owner_miss.contains(&7));
        assert_eq!(cache.owners.get(&7).map(String::as_str), Some("alice"));
    }

    #[test]
    fn reset_probes_forgets_negative_conclusions_but_keeps_names() {
        let mut cache = OwnerNameCache::default();
        cache.owner_miss.insert(9999);
        cache.group_miss.insert(9999);
        cache.getent_missing = true;
        cache.full_lookup_done = true;
        cache.owners.insert(0, "root".to_string());
        cache.groups.insert(0, "root".to_string());
        cache.reset_probes();
        assert!(cache.owner_miss.is_empty());
        assert!(cache.group_miss.is_empty());
        assert!(!cache.getent_missing);
        assert!(!cache.full_lookup_done);
        assert_eq!(cache.owners.get(&0).map(String::as_str), Some("root"));
        assert_eq!(cache.groups.get(&0).map(String::as_str), Some("root"));
    }

    #[test]
    fn failed_lookup_shape_does_not_mark_miss_via_empty_absorb() {
        // Documented contract: callers must not absorb a failed lookup. A
        // successful parse with zero names for a requested id *does* mark miss.
        let mut cache = OwnerNameCache::default();
        cache.absorb_resolved(
            &OwnerLookupNeed {
                uids: vec![5],
                gids: vec![],
            },
            HashMap::new(),
            HashMap::new(),
        );
        assert!(cache.owner_miss.contains(&5));
    }

    #[test]
    fn parse_owner_lookup_output_splits_passwd_and_group() {
        let output = "\
root:x:0:0:root:/root:/bin/bash
www-data:x:33:33:www-data:/var/www:/usr/sbin/nologin
--MEATSHELL-GROUPS--
root:x:0:
www-data:x:33:
";
        let (owners, groups) = parse_owner_lookup_output(output);
        assert_eq!(owners.get(&0).map(String::as_str), Some("root"));
        assert_eq!(owners.get(&33).map(String::as_str), Some("www-data"));
        assert_eq!(groups.get(&0).map(String::as_str), Some("root"));
        assert_eq!(groups.get(&33).map(String::as_str), Some("www-data"));
    }

    #[test]
    fn parse_owner_lookup_output_ignores_malformed_lines() {
        let output = "\
not-a-passwd-line
ghost:x:notanumber:0:a:b:c
--MEATSHELL-GROUPS--
also-bad
";
        let (owners, groups) = parse_owner_lookup_output(output);
        assert!(owners.is_empty());
        assert!(groups.is_empty());
    }

    #[test]
    fn second_apply_after_absorb_needs_no_remote_lookup() {
        // Simulates the warm path: first listing discovers 1000, later listings
        // must return `is_complete` so publish paints names in one shot.
        let mut cache = OwnerNameCache::default();
        let mut first = vec![entry(Some(1000), Some(1000))];
        let need = cache.apply(&mut first);
        assert!(!need.is_complete());
        cache.absorb_resolved(
            &need,
            HashMap::from([(1000, "alice".to_string())]),
            HashMap::from([(1000, "staff".to_string())]),
        );
        cache.apply(&mut first);
        assert_eq!(first[0].owner.as_deref(), Some("alice"));
        assert_eq!(first[0].group.as_deref(), Some("staff"));

        let mut second = vec![entry(Some(1000), Some(1000)), entry(Some(1000), Some(1000))];
        let need = cache.apply(&mut second);
        assert!(need.is_complete());
        assert!(second.iter().all(|e| e.owner.as_deref() == Some("alice")));
    }
}

#[cfg(test)]
mod upload_conflict_ask_tests {
    use super::ask_upload_conflict;
    use crate::ssh::{SessionEvent, UploadConflictReply};

    #[tokio::test]
    async fn answer_is_forwarded_back_to_the_worker() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
        let worker = tokio::spawn(async move { ask_upload_conflict(&tx, "a.txt").await });
        let event = rx.recv().await.expect("ask event").clone();
        let SessionEvent::SftpAskUploadConflict { name, responder } = event else {
            panic!("expected SftpAskUploadConflict, got {event:?}");
        };
        assert_eq!(name, "a.txt");
        responder.respond(UploadConflictReply::Overwrite);
        assert_eq!(worker.await.unwrap(), UploadConflictReply::Overwrite);
    }

    #[tokio::test]
    async fn dropped_responder_falls_back_to_keep_both() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
        let worker = tokio::spawn(async move { ask_upload_conflict(&tx, "a.txt").await });
        let event = rx.recv().await.expect("ask event").clone();
        let SessionEvent::SftpAskUploadConflict { responder, .. } = event else {
            panic!("expected SftpAskUploadConflict, got {event:?}");
        };
        drop(responder); // window closed before answering
        assert_eq!(worker.await.unwrap(), UploadConflictReply::KeepBoth);
    }

    #[tokio::test]
    async fn no_ui_falls_back_to_keep_both() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
        drop(rx);
        assert_eq!(
            ask_upload_conflict(&tx, "a.txt").await,
            UploadConflictReply::KeepBoth
        );
    }
}
