//! Connects the shared update state to each window's update controls: the
//! toolbar Updates button (red dot when a newer release exists) and the
//! Settings → Updates page.

use super::*;
use crate::update::inplace::{self, Method};
use crate::update::{mirror, Release};

#[derive(Default)]
pub struct State {
    release: Option<Release>,
    busy: bool,
    status: String,
    error: bool,
    /// How this build updates; detected once (it probes folder permissions).
    method: Option<Method>,
    package: Option<Package>,
    /// Settings speed test: latest result per source prefix (`""` = GitHub).
    pings: std::collections::HashMap<String, Ping>,
    /// Bumped per speed test so a stale result never overwrites a newer one.
    ping_generation: u64,
}

/// A verified update waiting for "Install and restart".
enum Package {
    #[cfg(windows)]
    Msi(crate::update::windows::Download),
    InPlace(inplace::Prepared),
    /// The AUR helper ran in a terminal tab; only a restart is left.
    Restart,
}

fn method(core: &Rc<AppCore>) -> Method {
    core.update
        .borrow_mut()
        .method
        .get_or_insert_with(inplace::detect)
        .clone()
}

fn one_click(method: &Method) -> bool {
    match method {
        #[cfg(windows)]
        Method::Msi(_) => true,
        Method::InPlace(_) | Method::Aur => true,
        _ => false,
    }
}

fn publish(core: &Rc<AppCore>) {
    let state = core.update.borrow();
    core.registry.for_each(|weak| {
        if let Some(window) = weak.upgrade() {
            window.set_update_busy(state.busy);
            window.set_update_status(state.status.clone().into());
            window.set_update_error(state.error);
            window.set_update_version(
                state
                    .release
                    .as_ref()
                    .map(|r| r.tag_name.as_str())
                    .unwrap_or("")
                    .into(),
            );
            window.set_update_available(state.release.is_some());
            window.set_update_ready(state.package.is_some());
        }
    });
}

fn on_ui(f: impl FnOnce(&Rc<AppCore>) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        NEW_WINDOW_CORE.with(|slot| {
            if let Some(core) = slot.borrow().as_ref() {
                f(core);
            }
        });
    });
}

fn check(core: &Rc<AppCore>, manual: bool) {
    if core.update.borrow().busy {
        return;
    }
    let package_manager = method(core) == Method::PackageManager;
    let aur = method(core) == Method::Aur;
    {
        let mut state = core.update.borrow_mut();
        state.busy = true;
        state.error = false;
        state.status = t("正在检查更新…", "Checking for updates…").into();
    }
    publish(core);
    std::thread::spawn(move || {
        let mut result = crate::update::check();
        // The AUR package follows the GitHub release by a little while; offer
        // the update once the AUR has it, or paru would find nothing to do.
        let mut aur_behind = None;
        if let (true, Ok(Some(release))) = (aur, &result) {
            match crate::update::linux::aur_version() {
                Ok(version) if !crate::update::caught_up(&version, &release.tag_name) => {
                    aur_behind = Some(release.tag_name.clone());
                    result = Ok(None);
                }
                Ok(_) => {}
                Err(error) => tracing::warn!("update: AUR check failed: {error:#}"),
            }
        }
        on_ui(move |core| {
            {
                let mut state = core.update.borrow_mut();
                state.busy = false;
                match result {
                    Ok(release) => {
                        if state.release.as_ref().map(|r| &r.tag_name)
                            != release.as_ref().map(|r| &r.tag_name)
                        {
                            state.package = None;
                        }
                        state.status = match (&release, package_manager) {
                            (Some(_), true) => t(
                                "有新版本，请通过系统包管理器更新",
                                "An update is available — update it with your package manager",
                            ),
                            (Some(_), false) => t("有新版本可用", "An update is available"),
                            (None, _) => t("当前已是最新版本", "You are up to date"),
                        }
                        .into();
                        if let Some(tag) = &aur_behind {
                            state.status = format!(
                                "{tag} {}",
                                t(
                                    "已发布，AUR 软件包更新后即可升级",
                                    "is out; it can be installed once the AUR package is updated",
                                )
                            );
                        }
                        state.release = release;
                    }
                    Err(error) => {
                        state.error = manual;
                        state.status = if manual {
                            if let Some(limit) = error.downcast_ref::<crate::update::RateLimited>()
                            {
                                let time: chrono::DateTime<chrono::Local> = limit.retry_at.into();
                                format!(
                                    "{} {}{}",
                                    t(
                                        "GitHub 版本查询达到限额，请在",
                                        "GitHub API rate limit reached; retry after"
                                    ),
                                    time.format("%H:%M:%S"),
                                    t(
                                        " 后重试，或打开发布页面下载",
                                        "; or download from the release page"
                                    ),
                                )
                            } else {
                                format!(
                                    "{}: {error:#}",
                                    t("检查失败，请重试", "Update check failed; please retry")
                                )
                            }
                        } else {
                            String::new()
                        };
                    }
                }
            }
            publish(core);
        });
    });
}

fn download(core: &Rc<AppCore>) {
    if core.update.borrow().busy {
        return;
    }
    let Some(release) = core.update.borrow().release.clone() else {
        return;
    };
    let method = method(core);
    if !one_click(&method) {
        return;
    }
    // Chinese UI in a mainland China time zone: the picked source first, or
    // race the mirrors against GitHub when the choice is automatic.
    let sources = if mirror::applies(crate::i18n::is_en()) {
        let store = core.store.borrow();
        mirror::Sources::with_mirrors(&store.update_mirrors())
            .prefer(store.update_mirror_choice().as_deref())
    } else {
        mirror::Sources::github()
    };
    {
        let mut state = core.update.borrow_mut();
        state.busy = true;
        state.error = false;
        state.status = if sources.will_probe() {
            t("正在测速下载源…", "Testing download mirrors…")
        } else {
            t("正在下载更新…", "Downloading update…")
        }
        .into();
    }
    publish(core);
    std::thread::spawn(move || {
        let mut last = (String::new(), -1);
        let progress = move |source: &str, received: u64, size: u64| {
            let percent = (received.saturating_mul(100) / size.max(1)) as i32;
            if percent != last.1 || source != last.0 {
                last = (source.to_string(), percent);
                let source = source.to_string();
                on_ui(move |core| {
                    core.update.borrow_mut().status = if crate::i18n::is_en() {
                        format!("Downloading from {source} {percent}%")
                    } else {
                        format!("正在从 {source} 下载 {percent}%")
                    };
                    publish(core);
                });
            }
        };
        let result = match method {
            #[cfg(windows)]
            Method::Msi(_) => release.windows_installer().and_then(|asset| {
                crate::update::windows::Download::fetch(asset, &sources, progress)
                    .map(Package::Msi)
            }),
            Method::InPlace(target) => {
                inplace::prepare(&release, &target, &sources, progress).map(Package::InPlace)
            }
            _ => unreachable!("download is offered only for one-click updates"),
        };
        on_ui(move |core| {
            {
                let mut state = core.update.borrow_mut();
                state.busy = false;
                match result {
                    Ok(package) => {
                        state.package = Some(package);
                        state.status = t(
                            "下载已验证，可以安装",
                            "Download verified; ready to install",
                        )
                        .into();
                    }
                    Err(error) => {
                        state.error = true;
                        state.status = format!(
                            "{}: {error:#}",
                            t("下载失败，请重试", "Download failed; please retry")
                        );
                    }
                }
            }
            publish(core);
        });
    });
}

/// Close every window through the same path as Quit (saves layouts, stops workers).
fn shut_down(core: &Rc<AppCore>) {
    let windows: Vec<_> = core
        .window_states
        .borrow()
        .values()
        .map(|state| state.main_win.clone())
        .collect();
    for window in windows {
        window.invoke_confirm_close_yes();
    }
}

fn install(core: &Rc<AppCore>, weak: &slint::Weak<AppWindow>) {
    if core.update.borrow().busy || core.update.borrow().package.is_none() {
        return;
    }
    #[cfg(windows)]
    let msi = matches!(core.update.borrow().package, Some(Package::Msi(_)));
    #[cfg(not(windows))]
    let msi = false;
    let (restart_only, admin) = match &core.update.borrow().package {
        Some(Package::Restart) => (true, false),
        Some(Package::InPlace(prepared)) => (false, prepared.needs_admin()),
        _ => (false, false),
    };
    let description = if restart_only {
        t(
            "将关闭所有 MeatShell 窗口和连接并重新打开程序。请确认终端标签页中的更新已经完成，并先保存编辑中的文件。",
            "All MeatShell windows and connections will close and the app will reopen. Make sure the update in the terminal tab has finished, and save any files you are editing first.",
        )
    } else if admin {
        t(
            "将安装新版本，系统会要求输入管理员密码。安装完成后关闭所有 MeatShell 窗口和连接并重新打开程序。请先保存编辑中的文件。",
            "The new version will be installed; your system will ask for an administrator password. All MeatShell windows and connections then close and the app reopens. Save any files you are editing first.",
        )
    } else if msi {
        t(
            "将关闭所有 MeatShell 窗口和连接，然后安装更新并重新打开程序。请先保存编辑中的文件。Windows 可能要求管理员授权。",
            "All MeatShell windows and connections will close. The update will be installed and the app reopened. Save any files you are editing first. Windows may request administrator permission.",
        )
    } else {
        t(
            "将关闭所有 MeatShell 窗口和连接，然后替换为新版本并重新打开程序。请先保存编辑中的文件。",
            "All MeatShell windows and connections will close, MeatShell will be replaced with the new version and reopened. Save any files you are editing first.",
        )
    };
    let result = DialogOwner::of_weak(weak)
        .message()
        .set_title(t("安装更新", "Install update"))
        .set_description(description)
        .set_buttons(rfd::MessageButtons::OkCancel)
        .show();
    if result != rfd::MessageDialogResult::Ok {
        return;
    }
    let Some(package) = core.update.borrow_mut().package.take() else {
        return;
    };
    {
        let mut state = core.update.borrow_mut();
        state.busy = true;
        state.status = if admin {
            t("正在安装，请在系统对话框中授权…", "Installing — approve the system password prompt…")
        } else {
            t("正在准备安装…", "Preparing installation…")
        }
        .into();
        state.error = false;
    }
    publish(core);
    match package {
        #[cfg(windows)]
        Package::Msi(package) => install_msi(package),
        Package::InPlace(prepared) => install_in_place(prepared),
        Package::Restart => restart_app(core),
    }
}

/// After the AUR helper replaced `/usr/bin/meatshell`: start it and close.
fn restart_app(core: &Rc<AppCore>) {
    // Once the package replaced the binary, Linux reports this process's
    // executable as "<path> (deleted)"; the new file is at <path>.
    let result = std::env::current_exe()
        .map(|exe| {
            let replaced = exe
                .to_str()
                .and_then(|p| p.strip_suffix(" (deleted)"))
                .map(std::path::PathBuf::from);
            replaced.unwrap_or(exe)
        })
        .map_err(anyhow::Error::from)
        .and_then(|exe| inplace::relaunch(&inplace::Relaunch::Path(exe)));
    match result {
        Ok(()) => shut_down(core),
        Err(error) => {
            core.update.borrow_mut().package = Some(Package::Restart);
            failed(core, t("无法重新启动", "Could not restart"), error);
        }
    }
}

/// AUR: run the user's AUR helper in a new terminal tab, then offer a restart.
fn update_in_terminal(core: &Rc<AppCore>, window: &AppWindow) {
    window.invoke_connect_session(crate::update::linux::AUR_SESSION_ID.into());
    {
        let mut state = core.update.borrow_mut();
        state.error = false;
        state.package = Some(Package::Restart);
        state.status = t(
            "请在终端标签页中完成更新，然后点击“重启”",
            "Finish the update in the terminal tab, then click Restart",
        )
        .into();
    }
    publish(core);
}

#[cfg(windows)]
fn install_msi(mut package: crate::update::windows::Download) {
    let Some(executable) = crate::update::windows::installed_executable() else {
        return;
    };
    let data_dir = crate::config::data_dir();
    let english = crate::i18n::is_en();
    std::thread::spawn(move || {
        let result = package.launch(&executable, &data_dir, english);
        on_ui(move |core| match result {
            Ok(()) => shut_down(core),
            Err(error) => {
                drop(package);
                failed(
                    core,
                    t("无法开始安装，请重新下载", "Could not start installation; download again"),
                    error,
                );
            }
        });
    });
}

fn install_in_place(mut prepared: inplace::Prepared) {
    std::thread::spawn(move || {
        let result = prepared.install();
        drop(prepared);
        let outcome = result.map(|path| inplace::relaunch(&path));
        on_ui(move |core| match outcome {
            Ok(Ok(())) => shut_down(core),
            Ok(Err(error)) => {
                // The new files are in place; only the automatic restart failed.
                tracing::warn!("update: relaunch failed: {error:#}");
                {
                    let mut state = core.update.borrow_mut();
                    state.busy = false;
                    state.error = true;
                    state.release = None;
                    state.status = t(
                        "更新已安装，请重新启动 MeatShell 以使用新版本",
                        "Update installed; restart MeatShell to use it",
                    )
                    .into();
                }
                publish(core);
            }
            Err(error) => failed(
                core,
                t("安装失败，请重新下载", "Installation failed; download again"),
                error,
            ),
        });
    });
}

fn failed(core: &Rc<AppCore>, message: &str, error: anyhow::Error) {
    {
        let mut state = core.update.borrow_mut();
        state.busy = false;
        state.error = true;
        state.status = format!("{message}: {error:#}");
    }
    publish(core);
}

/// Speed-test result for one download source in Settings → Updates.
#[derive(Clone, Copy)]
pub enum Ping {
    Testing,
    /// Milliseconds to fetch the probe, or `None` if the source failed.
    Done(Option<u128>),
}

/// GitHub (`""`) and the user's valid, distinct mirror prefixes.
fn mirror_prefixes(core: &Rc<AppCore>) -> Vec<String> {
    let mut prefixes = vec![String::new()];
    for mirror in core.store.borrow().update_mirrors() {
        if let Ok(prefix) = mirror::normalize(&mirror) {
            if !prefixes.contains(&prefix) {
                prefixes.push(prefix);
            }
        }
    }
    prefixes
}

/// The user's picked source if it is still in the list (`None` = automatic).
fn valid_choice(core: &Rc<AppCore>) -> Option<String> {
    let choice = core.store.borrow().update_mirror_choice()?;
    let choice = if choice.is_empty() { choice } else { mirror::normalize(&choice).ok()? };
    mirror_prefixes(core).contains(&choice).then_some(choice)
}

fn save_choice(core: &Rc<AppCore>, choice: Option<String>) {
    let mut store = core.store.borrow_mut();
    store.set_update_mirror_choice(choice);
    let _ = store.save();
}

/// Push the source rows (GitHub first, then mirrors, each with its latest
/// speed-test result and which one is selected) and the last add error to
/// every window. Automatic choice selects the fastest tested source.
fn publish_mirrors(core: &Rc<AppCore>, error: &str) {
    let mirrors = core.store.borrow().update_mirrors();
    let choice = valid_choice(core);
    let auto = choice.is_none();
    let state = core.update.borrow();
    let fastest_key = state
        .pings
        .iter()
        .filter_map(|(key, ping)| match ping {
            Ping::Done(Some(ms)) => Some((*ms, key.clone())),
            _ => None,
        })
        .min()
        .map(|(_, key)| key);
    let fastest = fastest_key.as_ref().and_then(|key| match state.pings.get(key) {
        Some(Ping::Done(Some(ms))) => Some(*ms),
        _ => None,
    });
    let selected = choice.or_else(|| fastest_key.clone());
    let testing = state.pings.values().any(|ping| matches!(ping, Ping::Testing));
    let row = |url: &str, key: &str, removable: bool| {
        let (ping, level, is_fastest) = match state.pings.get(key) {
            None => (String::new(), 0, false),
            Some(Ping::Testing) => (t("测速中…", "Testing…").to_string(), 0, false),
            Some(Ping::Done(None)) => (t("失败", "Failed").to_string(), 4, false),
            Some(Ping::Done(Some(ms))) => (
                format!("{ms} ms"),
                match ms {
                    0..=999 => 1,
                    1000..=2999 => 2,
                    _ => 3,
                },
                Some(*ms) == fastest,
            ),
        };
        UpdateMirrorRow {
            url: url.into(),
            key: key.into(),
            ping: ping.into(),
            level,
            fastest: is_fastest,
            selected: selected.as_deref() == Some(key),
            removable,
        }
    };
    let mut rows = vec![row("https://github.com/", "", false)];
    for mirror in &mirrors {
        let key = mirror::normalize(mirror).unwrap_or_else(|_| mirror.clone());
        rows.push(row(mirror, &key, true));
    }
    drop(state);
    core.registry.for_each(|weak| {
        if let Some(window) = weak.upgrade() {
            window.set_update_mirrors(slint::ModelRc::new(slint::VecModel::from(rows.clone())));
            window.set_update_mirror_error(error.into());
            window.set_update_mirrors_testing(testing);
            window.set_update_mirror_auto(auto);
        }
    });
}

/// Speed-test GitHub and every mirror with the same probe a one-click update
/// uses (the first 64 KiB of a real release file), in the background.
fn test_mirrors(core: &Rc<AppCore>) {
    let prefixes = mirror_prefixes(core);
    // The newest known release, else this build's own (which is published).
    let tag = core
        .update
        .borrow()
        .release
        .as_ref()
        .map(|release| release.tag_name.clone())
        .unwrap_or_else(|| {
            let version = env!("CARGO_PKG_VERSION");
            format!("v{}", version.split('-').next().unwrap_or(version))
        });
    let generation = {
        let mut state = core.update.borrow_mut();
        state.ping_generation += 1;
        state.pings = prefixes.iter().map(|p| (p.clone(), Ping::Testing)).collect();
        state.ping_generation
    };
    publish_mirrors(core, "");
    std::thread::spawn(move || {
        let times = mirror::probe_all(&prefixes, &mirror::speed_test_url(&tag), u64::MAX);
        on_ui(move |core| {
            {
                let mut state = core.update.borrow_mut();
                // A newer test (or a list edit) superseded this one.
                if state.ping_generation != generation {
                    return;
                }
                for (prefix, time) in prefixes.into_iter().zip(times) {
                    state
                        .pings
                        .insert(prefix, Ping::Done(time.map(|t| t.as_millis())));
                }
            }
            publish_mirrors(core, "");
        });
    });
}

fn save_mirrors(core: &Rc<AppCore>, mirrors: Option<Vec<String>>) {
    {
        let mut store = core.store.borrow_mut();
        store.set_update_mirrors(mirrors);
        let _ = store.save();
    }
    test_mirrors(core);
}

fn wire_mirrors(core: &Rc<AppCore>, window: &AppWindow) {
    window.set_update_china_tz(mirror::china_time_zone());
    {
        let core = core.clone();
        window.on_add_update_mirror(move |text| match mirror::normalize(&text) {
            Ok(prefix) => {
                let mut list = core.store.borrow().update_mirrors();
                let known = list
                    .iter()
                    .any(|m| mirror::normalize(m).ok().as_deref() == Some(prefix.as_str()));
                if !known {
                    list.push(prefix);
                }
                save_mirrors(&core, Some(list));
            }
            Err(_) => publish_mirrors(
                &core,
                t(
                    "地址无效：须以 https:// 开头，且不含 ?、# 或账号密码",
                    "Invalid address: use an https:// prefix without ?, # or a user name",
                ),
            ),
        });
    }
    {
        // Removing keeps the other results; no new speed test needed.
        let core = core.clone();
        window.on_remove_update_mirror(move |url| {
            let mut list = core.store.borrow().update_mirrors();
            list.retain(|m| m.as_str() != url.as_str());
            {
                let mut store = core.store.borrow_mut();
                store.set_update_mirrors(Some(list));
                let _ = store.save();
            }
            let key = mirror::normalize(&url).unwrap_or_else(|_| url.to_string());
            core.update.borrow_mut().pings.remove(&key);
            // Removing the picked mirror goes back to automatic choice.
            if valid_choice(&core).is_none() {
                save_choice(&core, None);
            }
            publish_mirrors(&core, "");
        });
    }
    {
        let core = core.clone();
        window.on_reset_update_mirrors(move || {
            save_choice(&core, None);
            save_mirrors(&core, None);
        });
    }
    {
        // Clicking a row makes it the fixed choice (automatic turns off).
        let core = core.clone();
        window.on_select_update_mirror(move |key| {
            save_choice(&core, Some(key.to_string()));
            publish_mirrors(&core, "");
        });
    }
    {
        // On: follow the fastest source. Off: keep the currently selected one.
        let core = core.clone();
        let weak = window.as_weak();
        window.on_set_update_mirror_auto(move |auto| {
            use slint::Model;
            let choice = if auto {
                None
            } else {
                // Nothing tested yet: fall back to GitHub ("").
                let selected = weak.upgrade().and_then(|w| {
                    w.get_update_mirrors().iter().find(|r| r.selected).map(|r| r.key.to_string())
                });
                Some(selected.unwrap_or_default())
            };
            save_choice(&core, choice);
            publish_mirrors(&core, "");
        });
    }
    {
        let core = core.clone();
        window.on_test_update_mirrors(move || test_mirrors(&core));
    }
}

pub(super) fn wire(core: &Rc<AppCore>, window: &AppWindow) {
    window.set_update_install_supported(one_click(&method(core)));
    window.set_update_via_terminal(method(core) == Method::Aur);
    wire_mirrors(core, window);
    publish_mirrors(core, "");
    {
        let core = core.clone();
        window.on_check_for_updates(move || check(&core, true));
    }
    {
        let core = core.clone();
        let weak = window.as_weak();
        window.on_update_action(move || {
            let Some(window) = weak.upgrade() else { return };
            if window.get_update_install_supported() {
                if core.update.borrow().package.is_some() {
                    install(&core, &weak);
                } else if method(&core) == Method::Aur {
                    update_in_terminal(&core, &window);
                } else {
                    download(&core);
                }
                return;
            }
            window.invoke_open_update_url();
        });
    }
    publish(core);
    let first_window = !core.first_window_done.replace(true);
    if first_window && core.store.borrow().update_check_enabled() {
        check(core, false);
    }
}

/// The application uses process::exit after bounded shutdown, so drop pending
/// downloads explicitly rather than relying on destructors at process exit.
pub(super) fn cleanup(core: &Rc<AppCore>) {
    core.update.borrow_mut().package = None;
}
