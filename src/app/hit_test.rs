//! Hit-testing and geometry for the terminal pane, docked SFTP panel and
//! file-drop targets. Split out of app.rs to match the upstream module
//! boundary (hit_test).

use crate::terminal::ScreenDims as _;
use super::*;

/// The active terminal tab's current SFTP directory ("" if unknown).
pub(crate) fn active_sftp_path(win: &AppWindow, tab_id: &str) -> String {
    let model = win.get_terminals();
    if let Some(m) = model.as_any().downcast_ref::<VecModel<TerminalState>>() {
        for i in 0..m.row_count() {
            if let Some(row) = m.row_data(i) {
                if row.id.as_str() == tab_id {
                    return row.sftp_path.to_string();
                }
            }
        }
    }
    String::new()
}

// The raw macOS wheel fallback runs before the usual Slint hit testing. Keep
// modal-state routing explicit so it cannot target a terminal behind a dialog.
pub(crate) fn macos_terminal_wheel_can_target_terminal(interface_open: bool) -> bool {
    !interface_open
}

pub(crate) fn terminal_wheel_hit(
    win: &AppWindow,
    bufs: &TermBuffers,
    x: f32,
    y: f32,
) -> Option<TerminalWheelHit> {
    let (active, term, _) = active_terminal_panel_rects(win)?;
    let mut term_x = term.x;
    let mut term_y = term.y;
    let mut term_w = term.w;
    let mut term_h = term.h;

    // Zen mode removes only the 24px status strip; docks and the command bar
    // stay on screen.
    if !win.get_zen_mode() {
        term_y += 24.0;
        term_h = (term_h - 24.0).max(0.0);
    }

    // The SFTP panel is now a window-level docked panel (#dock-stack), no
    // longer carved out of this pane's rect, so there is nothing to shrink
    // here — the central pane already excludes every docked edge stack.

    // Leave the command bar to TextInput/history handling; wheel fallback is
    // for terminal output only. The bar is present in every mode unless the
    // toolbar toggle hid it.
    if !win.get_cmd_bar_hidden() {
        term_h = (term_h - 34.0).max(0.0);
    }
    if !contains_logical(
        LogicalRect {
            x: term_x,
            y: term_y,
            w: term_w,
            h: term_h,
        },
        x,
        y,
    ) {
        return None;
    }

    let h = term_buf(bufs, &active)?;
    let guard = h.lock().ok()?;
    let screen = guard.parser.screen();
    let (rows, cols) = screen.dims();
    let cell_w = (term_w / cols.max(1) as f32).max(1.0);
    let cell_h = (term_h / rows.max(1) as f32).max(1.0);
    Some(TerminalWheelHit {
        tab_id: active,
        is_alt: screen.alternate_screen(),
        col: ((x - term_x) / cell_w).floor() as i32,
        row: ((y - term_y) / cell_h).floor() as i32,
    })
}

pub(crate) fn shrink_edge(x: &mut f32, y: &mut f32, w: &mut f32, h: &mut f32, dock: &str, amount: f32) {
    let amount = amount.max(0.0);
    match dock {
        "left" => {
            *x += amount;
            *w = (*w - amount).max(0.0);
        }
        "right" => *w = (*w - amount).max(0.0),
        "top" => {
            *y += amount;
            *h = (*h - amount).max(0.0);
        }
        "bottom" => *h = (*h - amount).max(0.0),
        _ => {}
    }
}

pub(crate) fn contains_logical(rect: LogicalRect, x: f32, y: f32) -> bool {
    x >= rect.x && x <= rect.x + rect.w && y >= rect.y && y <= rect.y + rect.h
}

pub(crate) fn app_content_area(win: &AppWindow) -> LogicalRect {
    let size = win.window().size();
    let scale = win.window().scale_factor().max(0.01) as f32;
    let mut area = LogicalRect {
        x: 0.0,
        y: if win.get_custom_titlebar() {
            38.0
        } else if win.get_is_mac() {
            28.0
        } else {
            0.0
        },
        w: size.width as f32 / scale,
        h: 0.0,
    };
    area.h = size.height as f32 / scale - area.y;

    if win.get_zen_mode() {
        return area;
    }

    if win.get_welcome_as_sidebar() {
        let dock = win.get_welcome_sidebar_dock().to_string();
        let sidebar_strip_outside = !win.get_welcome_collapsed()
            && win.get_sidebar_collapsed()
            && win.get_sidebar_dock().as_str() == dock.as_str();
        let welcome_taken = (if win.get_welcome_collapsed() {
            36.0
        } else {
            win.get_welcome_sidebar_width()
        }) + if sidebar_strip_outside { 36.0 } else { 0.0 };
        shrink_edge(
            &mut area.x,
            &mut area.y,
            &mut area.w,
            &mut area.h,
            &dock,
            welcome_taken,
        );
    }

    let side_dock = win.get_sidebar_dock().to_string();
    let side_take = if win.get_sidebar_collapsed() {
        36.0
    } else if side_dock == "left" || side_dock == "right" {
        win.get_sidebar_width() + 4.0
    } else {
        win.get_sidebar_height() + 4.0
    };
    shrink_edge(
        &mut area.x,
        &mut area.y,
        &mut area.w,
        &mut area.h,
        &side_dock,
        side_take,
    );
    if win.get_quick_panel_open() {
        let quick_dock = win.get_quick_panel_dock().to_string();
        let quick_merged = win.get_quick_panel_collapsed()
            && ((win.get_welcome_as_sidebar()
                && win.get_welcome_collapsed()
                && win.get_welcome_sidebar_dock().as_str() == quick_dock.as_str())
                || (win.get_sidebar_collapsed() && side_dock.as_str() == quick_dock.as_str()));
        if !quick_merged {
            let quick_take = if win.get_quick_panel_collapsed() {
                36.0
            } else if quick_dock == "left" || quick_dock == "right" {
                win.get_quick_panel_width() + 4.0
            } else {
                win.get_quick_panel_height() + 4.0
            };
            shrink_edge(
                &mut area.x,
                &mut area.y,
                &mut area.w,
                &mut area.h,
                &quick_dock,
                quick_take,
            );
        }
    }
    if win.get_ai_panel_open() {
        let ai_dock = win.get_ai_panel_dock().to_string();
        // A collapsed AI panel joined a collapsed strip on the same edge
        // (welcome > sidebar > quick priority), so it reserves no extra space.
        let ai_merged = win.get_ai_panel_collapsed()
            && ((win.get_welcome_as_sidebar()
                && win.get_welcome_collapsed()
                && win.get_welcome_sidebar_dock().as_str() == ai_dock.as_str())
                || (win.get_sidebar_collapsed() && side_dock.as_str() == ai_dock.as_str())
                || (win.get_quick_panel_open()
                    && win.get_quick_panel_collapsed()
                    && win.get_quick_panel_dock().as_str() == ai_dock.as_str()));
        if !ai_merged {
            let ai_take = if win.get_ai_panel_collapsed() {
                36.0
            } else if ai_dock == "left" || ai_dock == "right" {
                win.get_ai_panel_width() + 4.0
            } else {
                win.get_ai_panel_height() + 4.0
            };
            shrink_edge(
                &mut area.x,
                &mut area.y,
                &mut area.w,
                &mut area.h,
                &ai_dock,
                ai_take,
            );
        }
    }
    area
}

pub(crate) fn active_terminal_panel_rects(win: &AppWindow) -> Option<(String, LogicalRect, TerminalState)> {
    let active = win.get_active_tab_id().to_string();
    if active.is_empty() || active == "welcome" {
        return None;
    }

    let area = app_content_area(win);
    let panes = win.get_panes();
    let pane = (0..panes.row_count())
        .filter_map(|i| panes.row_data(i))
        .find(|p| p.active_id.as_str() == active.as_str())?;

    let terms = win.get_terminals();
    let term_state = (0..terms.row_count())
        .filter_map(|i| terms.row_data(i))
        .find(|t| t.id.as_str() == active.as_str())?;

    Some((
        active,
        LogicalRect {
            x: area.x + pane.x,
            y: area.y + pane.y + 40.0,
            w: pane.w,
            h: (pane.h - 40.0).max(0.0),
        },
        term_state,
    ))
}

/// The dock area's frame in logical client coordinates (below the title bar).
/// Sibling of `app_content_area`'s origin: panels are laid out inside it.
pub(crate) fn dock_area_frame(win: &AppWindow) -> LogicalRect {
    let size = win.window().size();
    let scale = win.window().scale_factor().max(0.01) as f32;
    let y = if win.get_custom_titlebar() {
        38.0
    } else if win.get_is_mac() {
        28.0
    } else {
        0.0
    };
    LogicalRect {
        x: 0.0,
        y,
        w: size.width as f32 / scale,
        h: size.height as f32 / scale - y,
    }
}

/// The docked SFTP panel's rect in logical client coordinates, when it is
/// expanded for the active tab; `None` in zen mode (panels hidden there).
pub(crate) fn sftp_panel_client_rect(
    win: &AppWindow,
    dock_stacks: &Rc<RefCell<DockStacks>>,
) -> Option<LogicalRect> {
    let frame = dock_area_frame(win);
    if win.get_zen_mode() {
        return None;
    }
    let geom = dock_stacks.borrow().compute_geom(
        &|k| panel_extent(win, k),
        &|edge| strip_on_edge(win, edge),
        frame.w,
        frame.h,
    );
    geom.panels
        .into_iter()
        .find(|p| p.kind == "sftp")
        .map(|p| LogicalRect {
            x: frame.x + p.rect.x,
            y: frame.y + p.rect.y,
            w: p.rect.w,
            h: p.rect.h,
        })
}

