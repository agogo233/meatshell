//! Connects the shared update state to each window's existing update controls.

use super::*;
use crate::update::inplace::{self, Method};
use crate::update::Release;

#[derive(Default)]
pub struct State {
    release: Option<Release>,
    busy: bool,
    status: String,
    error: bool,
    /// How this build updates; detected once (it probes folder permissions).
    method: Option<Method>,
    package: Option<Package>,
}

/// A verified update waiting for "Install and restart".
enum Package {
    #[cfg(windows)]
    Msi(crate::update::windows::Download),
    InPlace(inplace::Prepared),
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
        Method::InPlace(_) => true,
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
    {
        let mut state = core.update.borrow_mut();
        state.busy = true;
        state.error = false;
        state.status = t("正在检查更新…", "Checking for updates…").into();
    }
    publish(core);
    std::thread::spawn(move || {
        let result = crate::update::check();
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
                        state.release = release;
                    }
                    Err(error) => {
                        state.error = manual;
                        state.status = if manual {
                            format!(
                                "{}: {error:#}",
                                t("检查失败，请重试", "Update check failed; please retry")
                            )
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
    {
        let mut state = core.update.borrow_mut();
        state.busy = true;
        state.error = false;
        state.status = t("正在下载更新…", "Downloading update…").into();
    }
    publish(core);
    std::thread::spawn(move || {
        let mut last_percent = 0;
        let progress = move |received: u64, size: u64| {
            let percent = (received.saturating_mul(100) / size.max(1)) as i32;
            if percent != last_percent {
                last_percent = percent;
                on_ui(move |core| {
                    core.update.borrow_mut().status =
                        format!("{} {percent}%", t("正在下载", "Downloading"));
                    publish(core);
                });
            }
        };
        let result = match method {
            #[cfg(windows)]
            Method::Msi(_) => release.windows_installer().and_then(|asset| {
                crate::update::windows::Download::fetch(asset, progress).map(Package::Msi)
            }),
            Method::InPlace(target) => {
                inplace::prepare(&release, &target, progress).map(Package::InPlace)
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
    let description = if msi {
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
        state.status = t("正在准备安装…", "Preparing installation…").into();
        state.error = false;
    }
    publish(core);
    match package {
        #[cfg(windows)]
        Package::Msi(package) => install_msi(package),
        Package::InPlace(prepared) => install_in_place(prepared),
    }
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

pub(super) fn wire(core: &Rc<AppCore>, window: &AppWindow) {
    window.set_update_install_supported(one_click(&method(core)));
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
