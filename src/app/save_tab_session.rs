//! "Add to session list" for local terminal tabs (#501).
//!
//! Users often open a local shell and type `ssh user@host` instead of saving a
//! session first. The tab menu turns that ad-hoc connection into a saved SSH
//! session in one click: we find the `ssh` process running under the tab's
//! shell, read its command line (destination, `-p`, `-l`, `-i`, `-J`, `-o`,
//! `~/.ssh/config` aliases), save the session ungrouped (top level of the list)
//! and open it in the session editor so name, group and credentials can be
//! adjusted. Credentials typed into ssh cannot be recovered; with no saved
//! password the first connect prompts and offers to remember it.

use std::path::Path;

use super::*;
use crate::ssh::ssh_cli::{self, Endpoint};
use crate::ssh::ImportedHost;

/// What "Add to session list" should do for one `ssh` command line.
#[derive(Debug)]
pub(super) enum SavePlan {
    /// A new session to save, plus notes for the user (e.g. an unmatched jump).
    Add {
        session: Session,
        notes: Vec<String>,
    },
    /// An equivalent session is already saved; open that one instead.
    Existing { id: String, name: String },
}

fn same_user(a: &str, b: &str) -> bool {
    a == b
}

/// Saved SSH session for one `-J` hop: the hop's host (after `~/.ssh/config`)
/// or a saved session's name, with the same port, and the same user when the
/// hop names one.
fn find_jump_session<'a>(
    saved: &'a [Session],
    hop: &Endpoint,
    config: &[ImportedHost],
) -> Option<&'a Session> {
    let block = ssh_cli::find_config_host(config, &hop.host);
    let host = block.map_or(hop.host.as_str(), |b| b.hostname.as_str());
    let port = hop.port.or_else(|| block.map(|b| b.port)).unwrap_or(22);
    let user = hop
        .user
        .clone()
        .or_else(|| block.map(|b| b.user.clone()).filter(|u| !u.is_empty()));
    saved.iter().find(|s| {
        s.kind == SessionKind::Ssh
            && (s.host.eq_ignore_ascii_case(host) || s.name == hop.host)
            && s.port == port
            && user.as_deref().map_or(true, |u| same_user(&s.user, u))
    })
}

fn display_endpoint(hop: &Endpoint) -> String {
    let mut text = match &hop.user {
        Some(user) => format!("{user}@{}", hop.host),
        None => hop.host.clone(),
    };
    if let Some(port) = hop.port {
        text.push_str(&format!(":{port}"));
    }
    text
}

/// Decide what to save for `argv`, an `ssh` command line. `load_config` reads
/// the ssh config to apply (`Some(path)` for `ssh -F path`). `local_user` is
/// who ssh logs in as when nothing names a user.
pub(super) fn plan_saved_session(
    argv: &[String],
    saved: &[Session],
    load_config: impl FnOnce(Option<&str>) -> Vec<ImportedHost>,
    home: &Path,
    local_user: &str,
) -> Result<SavePlan, String> {
    let invocation = ssh_cli::parse_command_line(argv)?;
    let config = load_config(invocation.config_file.as_deref());
    let target = ssh_cli::resolve(&invocation, &config, home)?;
    let user = if target.user.is_empty() {
        local_user.to_string()
    } else {
        target.user.clone()
    };

    if let Some(existing) = saved.iter().find(|s| {
        s.kind == SessionKind::Ssh
            && s.host.eq_ignore_ascii_case(&target.host)
            && s.port == target.port
            && same_user(&s.user, &user)
    }) {
        return Ok(SavePlan::Existing {
            id: existing.id.clone(),
            name: existing.name.clone(),
        });
    }

    let mut notes = Vec::new();
    let mut jump_ids = Vec::new();
    for hop in &target.jump {
        match find_jump_session(saved, hop, &config) {
            Some(session) => jump_ids.push(session.id.clone()),
            None => {
                notes.push(format!(
                    "{} {}",
                    t(
                        "跳板机不在会话列表中，请在“跳板机”里手动选择：",
                        "Jump host is not a saved session; pick it under Jump hosts:"
                    ),
                    display_endpoint(hop)
                ));
            }
        }
    }
    // A partial route would connect the wrong way; keep all hops or none.
    if jump_ids.len() != target.jump.len() {
        jump_ids.clear();
    }

    let name = target.alias.clone().unwrap_or_else(|| {
        let mut name = if user.is_empty() {
            target.host.clone()
        } else {
            format!("{user}@{}", target.host)
        };
        if target.port != 22 {
            name.push_str(&format!(":{}", target.port));
        }
        name
    });
    let auth = if target.identity_file.is_empty() {
        AuthMethod::Password
    } else {
        AuthMethod::Key
    };
    let session = Session {
        name,
        host: target.host,
        port: target.port,
        user,
        auth,
        private_key_path: target.identity_file,
        jump_session_ids: jump_ids,
        // Ungrouped sessions sit at the top level of the list (#501).
        group: String::new(),
        kind: SessionKind::Ssh,
        ..Session::new_empty()
    };
    Ok(SavePlan::Add { session, notes })
}

fn local_user() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default()
}

fn load_ssh_config(path: Option<&str>) -> Vec<ImportedHost> {
    match path {
        Some(path) => {
            let path = match crate::ssh::ssh_config::home_dir() {
                Some(home) => crate::ssh::ssh_config::expand_tilde(path, &home),
                None => path.to_string(),
            };
            crate::ssh::ssh_config::parse_path(Path::new(&path))
        }
        None => crate::ssh::ssh_config::parse_default(),
    }
}

fn show_message(weak: &slint::Weak<AppWindow>, description: String) {
    DialogOwner::of_weak(weak)
        .message()
        .set_title(t("加入会话列表", "Add to session list"))
        .set_description(description)
        .set_level(rfd::MessageLevel::Info)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

/// Wire the tab menu's "Add to session list" (#501).
pub(super) fn wire(
    window: &AppWindow,
    store: Rc<RefCell<ConfigStore>>,
    registry: Rc<WindowRegistry<slint::Weak<AppWindow>>>,
    sessions_model: Rc<VecModel<SessionInfo>>,
) {
    let weak = window.as_weak();
    window.on_tab_save_session(move |tab_id: SharedString| {
        let Some(argv) = crate::terminal::local::ssh_command_in_tab(tab_id.as_str()) else {
            show_message(
                &weak,
                t(
                    "这个标签页里没有正在运行的 ssh 连接。请先在本地终端里用 ssh 连上服务器，再加入会话列表。（WSL 内运行的 ssh 无法识别。）",
                    "No running ssh connection was found in this tab. Connect with ssh in the local terminal first, then add it to the session list. (An ssh running inside WSL can't be detected.)",
                )
                .to_string(),
            );
            return;
        };
        let home = crate::ssh::ssh_config::home_dir().unwrap_or_default();
        let plan = plan_saved_session(
            &argv,
            store.borrow().sessions(),
            load_ssh_config,
            &home,
            &local_user(),
        );
        let (id, status) = match plan {
            Err(err) => {
                show_message(
                    &weak,
                    format!(
                        "{}: {err}\n\n{}",
                        t("无法识别这条 ssh 命令", "Couldn't read this ssh command"),
                        argv.join(" ")
                    ),
                );
                return;
            }
            Ok(SavePlan::Existing { id, name }) => (
                id,
                format!("{} {name}", t("已在会话列表中：", "Already in the session list:")),
            ),
            Ok(SavePlan::Add { session, notes }) => {
                let id = session.id.clone();
                let saved = {
                    let mut s = store.borrow_mut();
                    s.upsert(session);
                    s.save()
                };
                if let Err(err) = saved {
                    tracing::warn!("failed to save config: {err:#}");
                    show_message(
                        &weak,
                        format!("{}: {err:#}", t("保存会话失败", "Failed to save the session")),
                    );
                    return;
                }
                sync_sessions_for_window(
                    &weak,
                    &store.borrow(),
                    &sessions_model,
                );
                registry.broadcast_config_changed();
                (id, notes.join("\n"))
            }
        };
        // Open the editor so the name, group and credentials can be adjusted.
        if let Some(w) = weak.upgrade() {
            w.invoke_edit_session(id.into());
            if !status.is_empty() {
                w.set_dialog_test_status(status.into());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    fn saved(name: &str, host: &str, port: u16, user: &str) -> Session {
        Session {
            name: name.into(),
            host: host.into(),
            port,
            user: user.into(),
            kind: SessionKind::Ssh,
            ..Session::new_empty()
        }
    }

    fn plan(line: &str, existing: &[Session], config: Vec<ImportedHost>) -> SavePlan {
        plan_saved_session(
            &argv(line),
            existing,
            |_| config,
            Path::new("/home/me"),
            "me",
        )
        .unwrap()
    }

    fn added(plan: SavePlan) -> (Session, Vec<String>) {
        match plan {
            SavePlan::Add { session, notes } => (session, notes),
            other => panic!("expected a new session, got {other:?}"),
        }
    }

    #[test]
    fn adds_an_ungrouped_password_session() {
        let (session, notes) = added(plan("ssh root@192.168.100.2", &[], vec![]));
        assert_eq!(session.name, "root@192.168.100.2");
        assert_eq!(session.host, "192.168.100.2");
        assert_eq!(session.port, 22);
        assert_eq!(session.user, "root");
        assert_eq!(session.auth, AuthMethod::Password);
        assert!(session.password.is_empty());
        assert!(session.group.is_empty());
        assert_eq!(session.kind, SessionKind::Ssh);
        assert!(notes.is_empty());
    }

    #[test]
    fn defaults_to_the_local_user_and_names_non_default_ports() {
        let (session, _) = added(plan("ssh -p 2222 -i ~/.ssh/k nas.lan", &[], vec![]));
        assert_eq!(session.user, "me");
        assert_eq!(session.name, "me@nas.lan:2222");
        assert_eq!(session.auth, AuthMethod::Key);
        assert_eq!(session.private_key_path, "/home/me/.ssh/k");
    }

    #[test]
    fn uses_the_ssh_config_alias_as_the_name() {
        let config = vec![ImportedHost {
            alias: "pve".into(),
            hostname: "192.168.100.2".into(),
            user: "root".into(),
            port: 22,
            identity_file: String::new(),
        }];
        let (session, _) = added(plan("ssh pve", &[], config));
        assert_eq!(
            (
                session.name.as_str(),
                session.host.as_str(),
                session.user.as_str()
            ),
            ("pve", "192.168.100.2", "root")
        );
    }

    #[test]
    fn reuses_an_existing_session() {
        let existing = saved("pve", "192.168.100.2", 22, "root");
        match plan("ssh root@192.168.100.2", &[existing.clone()], vec![]) {
            SavePlan::Existing { id, name } => {
                assert_eq!(id, existing.id);
                assert_eq!(name, "pve");
            }
            other => panic!("{other:?}"),
        }
        // A different user is a different session.
        added(plan("ssh jeff@192.168.100.2", &[existing], vec![]));
    }

    #[test]
    fn maps_jump_hosts_to_saved_sessions() {
        let bastion = saved("bastion", "8.163.89.229", 22, "root");
        let hop2 = saved("hop2", "10.0.0.2", 2200, "ops");
        let existing = [bastion.clone(), hop2.clone()];
        let (session, notes) = added(plan(
            "ssh -J root@8.163.89.229,ops@10.0.0.2:2200 target",
            &existing,
            vec![],
        ));
        assert_eq!(session.jump_session_ids, [bastion.id.clone(), hop2.id]);
        assert!(notes.is_empty());
        // A saved session's name works as a hop, like an ssh_config alias.
        let (session, _) = added(plan("ssh -J bastion target", &existing, vec![]));
        assert_eq!(session.jump_session_ids, [bastion.id]);
    }

    #[test]
    fn unmatched_jump_hosts_leave_the_route_empty_with_a_note() {
        let bastion = saved("bastion", "8.163.89.229", 22, "root");
        let (session, notes) = added(plan(
            "ssh -J bastion,unknown:2222 target",
            &[bastion],
            vec![],
        ));
        assert!(session.jump_session_ids.is_empty());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("unknown:2222"), "{notes:?}");
    }

    #[test]
    fn passes_dash_f_to_the_config_loader() {
        let mut seen = None;
        let result = plan_saved_session(
            &argv("ssh -F ~/alt_config h"),
            &[],
            |path| {
                seen = path.map(String::from);
                Vec::new()
            },
            Path::new("/home/me"),
            "me",
        );
        assert!(result.is_ok());
        assert_eq!(seen.as_deref(), Some("~/alt_config"));
    }

    #[test]
    fn rejects_unreadable_command_lines() {
        let err = plan_saved_session(&argv("ssh -v"), &[], |_| Vec::new(), Path::new("/"), "me")
            .unwrap_err();
        assert!(err.contains("destination"), "{err}");
    }
}
