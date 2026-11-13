//! Pane layout: split/tab geometry, dock refresh and drag-target hit
//! testing for the edge-stack model. Split out of app.rs to match the
//! upstream module boundary (panes).

use super::*;


/// Resolve a session's configured SSH jump host to the saved session it points
/// at, ignoring a missing / dangling / self reference (#211).
pub(crate) fn save_layout(
    win: &AppWindow,
    store: &Rc<RefCell<ConfigStore>>,
    dock_stacks: &Rc<RefCell<DockStacks>>,
) {
    let scale = win.window().scale_factor().max(0.01);
    let size = win.window().size();
    let w = size.width as f32 / scale;
    let h = size.height as f32 / scale;
    let mut s = store.borrow_mut();
    s.set_sidebar_width(win.get_sidebar_width());
    s.set_sidebar_height(win.get_sidebar_height());
    s.set_sidebar_dock(win.get_sidebar_dock().to_string());
    s.set_sidebar_collapsed(win.get_sidebar_collapsed());
    s.set_sftp_panel_width(win.get_sftp_panel_width());
    s.set_sftp_panel_height(win.get_sftp_panel_height());
    s.set_sftp_dock(win.get_sftp_dock().to_string());
    s.set_sftp_local_show_hidden(win.get_sftp_local_show_hidden());
    s.set_sftp_local_sort_key(&win.get_sftp_local_sort_key().to_string());
    s.set_sftp_local_sort_dir(win.get_sftp_local_sort_dir());
    s.set_quick_panel_open(win.get_quick_panel_open());
    s.set_quick_panel_collapsed(win.get_quick_panel_collapsed());
    s.set_quick_panel_width(win.get_quick_panel_width());
    s.set_quick_panel_height(win.get_quick_panel_height());
    s.set_quick_panel_dock(win.get_quick_panel_dock().to_string());
    s.set_ai_panel_open(win.get_ai_panel_open());
    s.set_ai_panel_collapsed(win.get_ai_panel_collapsed());
    s.set_ai_panel_width(win.get_ai_panel_width());
    s.set_ai_panel_height(win.get_ai_panel_height());
    s.set_ai_panel_dock(win.get_ai_panel_dock().to_string());
    // Per-edge stacked panels (#dock-stack), ratios included.
    s.set_dock_stacks(dock_stacks.borrow().to_saved());
    s.set_welcome_sidebar_width(win.get_welcome_sidebar_width());
    s.set_welcome_sidebar_dock(win.get_welcome_sidebar_dock().to_string());
    s.set_welcome_collapsed(win.get_welcome_collapsed());
    // A maximized size isn't a useful "preferred" size to restore to, so only
    // remember the windowed size. Ask the native window too, because the Slint
    // property can lag during startup/shutdown on frameless Windows (#234).
    let native_maximized = win
        .window()
        .with_winit_window(|ww| ww.is_maximized())
        .unwrap_or_else(|| win.get_window_maximized());
    // Windows reports (-32000,-32000) for a minimized window; persisting
    // that sentinel would make the next launch open off-screen (#save-position).
    let native_minimized = win
        .window()
        .with_winit_window(|ww| ww.is_minimized().unwrap_or(false))
        .unwrap_or(false);
    if !native_maximized && w > 200.0 && h > 200.0 {
        // Resize events normally keep this cache current. Persist the final
        // valid native geometry as well, because a close can arrive before the
        // last resize callback has reached the UI store.
        s.set_window_size(w, h);
    }
    // Remember the window position too, so it restores on next launch.
    // Only when not maximized (a maximized position isn't useful) and not
    // minimized (its sentinel position isn't either). Wayland compositors
    // don't expose a settable window position, so `position()` reports a
    // constant (0,0); writing that would clobber a real position saved under
    // X11, so skip the save on Wayland (restore then centres / defers to the
    // compositor).
    if !native_maximized
        && !native_minimized
        && !is_wayland_window(win.window())
    {
        let pos = win.window().position();
        s.set_window_position(pos.x as f32 / scale, pos.y as f32 / scale);
        // Remember which monitor's scale these logical coordinates came from so
        // a mixed-DPI restore doesn't re-project them onto a different screen.
        s.set_window_scale(scale);
    }
    let _ = s.save();
}

/// Zen is a transient per-window state. Closing a window while it is on must
/// not leak the persisted flag into the next window, which would open stuck in
/// zen mode (#zen). Called on every confirmed-close path right after
/// `save_layout`.
pub(crate) fn clear_zen_on_close(win: &AppWindow, store: &Rc<RefCell<ConfigStore>>) {
    if !win.get_zen_mode() {
        return;
    }
    let mut s = store.borrow_mut();
    s.set_zen_mode(false);
    let _ = s.save();
}

/// Every quick-command group name (used to start with all groups collapsed, #55):
/// "default" when any ungrouped command exists, plus explicit quick-groups and any
/// group referenced by a command.
pub(crate) fn tabs_eq(a: &ModelRc<TabInfo>, b: &ModelRc<TabInfo>) -> bool {
    if a.row_count() != b.row_count() {
        return false;
    }
    (0..a.row_count()).all(|i| match (a.row_data(i), b.row_data(i)) {
        (Some(x), Some(y)) => x.id == y.id,
        _ => false,
    })
}

/// Find the terminal row with `tab_id`, apply `mutator`, and write it back.
pub(crate) fn update_terminal_row(
    model: &VecModel<TerminalState>,
    tab_id: &str,
    mutator: impl FnOnce(&mut TerminalState),
) {
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i) {
            if row.id.as_str() == tab_id {
                mutator(&mut row);
                model.set_row_data(i, row);
                return;
            }
        }
    }
}

pub(crate) fn update_welcome_tab(layout: &mut crate::layout::Layout, as_sidebar: bool) {
    if as_sidebar {
        layout.remove_tab("welcome");
    } else if layout.leaf_of_tab("welcome").is_none() {
        layout.add_tab("welcome".into());
    }
}



pub(crate) fn refresh_panes(
    window: &AppWindow,
    layout: &crate::layout::Layout,
    content: (f32, f32),
    tabs_model: &VecModel<TabInfo>,
    panes_model: &VecModel<PaneInfo>,
    splitters_model: &VecModel<SplitterInfo>,
) {
    let (cw, ch) = (content.0.max(1.0), content.1.max(1.0));
    let (panes, splits) = layout.flatten(0.0, 0.0, cw, ch);
    // With the custom title bar the toolbar icons sit in the title bar, so the
    // tab row no longer needs to keep room for them.
    let toolbar_in_tab_row = !window.get_custom_titlebar();

    let pane_infos: Vec<PaneInfo> = panes
        .iter()
        .map(|p| {
            // Map this pane's tab ids to their TabInfo rows (skipping any not yet
            // in the model).
            let tabs: Vec<TabInfo> = p
                .tabs
                .iter()
                .filter_map(|tid| {
                    (0..tabs_model.row_count()).find_map(|i| {
                        let row = tabs_model.row_data(i)?;
                        (row.id.as_str() == tid.as_str()).then_some(row)
                    })
                })
                .collect();
            // Only the pane touching the top-right corner keeps room for the
            // floating toolbar icons (#122). 7 icons at a 30px pitch leave the
            // rightmost one at width-214px, so 210px keeps the 4px gap the
            // 6-icon layout (180px ↔ the pin at width-184px) had before the
            // Updates icon joined the strip.
            let top_right = p.x + p.w >= cw - 0.5 && p.y <= 0.5;
            PaneInfo {
                id: p.id as i32,
                x: p.x,
                y: p.y,
                w: p.w,
                h: p.h,
                active_id: p.active.clone().into(),
                focused: p.focused,
                // Room for the top-right toolbar icons (#122) — only when they
                // float over the tab row (macOS); with the custom title bar the
                // icons sit in the title bar and no tab-row space is reserved.
                // 7 icons at a 30px pitch leave the rightmost at width-214px, so
                // 210px keeps the 4px gap.
                reserve_right: if top_right && toolbar_in_tab_row { 210.0 } else { 0.0 },
                tabs: ModelRc::from(Rc::new(VecModel::from(tabs))),
            }
        })
        .collect();

    // Update the models IN PLACE rather than replacing them, so the `for pane` /
    // `for sp` elements are reused: this keeps terminals from being recreated on
    // every refresh AND preserves the splitter's pointer-grab during a drag (a
    // fresh model would destroy the element mid-drag and drop the grab). When the
    // structure changes (split/close → different row count) a full rebuild is fine
    // since no drag is in flight.
    if panes_model.row_count() == pane_infos.len() {
        for (i, mut r) in pane_infos.into_iter().enumerate() {
            if let Some(old) = panes_model.row_data(i) {
                // Reuse the existing tab sub-model when the tabs are unchanged so a
                // geometry-only refresh doesn't churn the tab strips.
                let same_tabs = old.id == r.id && tabs_eq(&old.tabs, &r.tabs);
                let unchanged = same_tabs
                    && old.x == r.x
                    && old.y == r.y
                    && old.w == r.w
                    && old.h == r.h
                    && old.active_id == r.active_id
                    && old.focused == r.focused
                    && old.reserve_right == r.reserve_right;
                if same_tabs {
                    r.tabs = old.tabs;
                }
                if unchanged {
                    continue;
                }
            }
            panes_model.set_row_data(i, r);
        }
    } else {
        panes_model.set_vec(pane_infos);
    }

    let split_infos: Vec<SplitterInfo> = splits
        .iter()
        .map(|s| SplitterInfo {
            split_id: s.split_id as i32,
            x: s.x,
            y: s.y,
            w: s.w,
            h: s.h,
            vertical: s.vertical,
        })
        .collect();
    if splitters_model.row_count() == split_infos.len() {
        for (i, r) in split_infos.into_iter().enumerate() {
            let unchanged = splitters_model.row_data(i).is_some_and(|old| {
                old.split_id == r.split_id
                    && old.x == r.x
                    && old.y == r.y
                    && old.w == r.w
                    && old.h == r.h
                    && old.vertical == r.vertical
            });
            if !unchanged {
                splitters_model.set_row_data(i, r);
            }
        }
    } else {
        splitters_model.set_vec(split_infos);
    }

    if let Some(fp) = panes.iter().find(|p| p.focused) {
        if window.get_active_tab_id().as_str() != fp.active.as_str() {
            window.set_active_tab_id(fp.active.clone().into());
        }
    }
}

// --- Docked-panel edge stacks (#dock-stack) --------------------------------

/// Mirror the ACTIVE tab's SFTP state onto the window, where the docked
/// SftpPanel reads it (#dock-stack). Models are copied BY REFERENCE, so the
/// docked panel shares the same VecModel as the tab's TerminalView and picks
/// up listing updates for free. Called whenever the active tab or its SFTP
/// state changes; overwrites unconditionally so no stale seed (e.g. the
/// startup `collapse-sftp-default`) can leak into the dock decision.
pub(crate) fn sync_active_sftp_to_root(win: &AppWindow, terminals: &VecModel<TerminalState>) {
    use slint::Model as _;
    let active = win.get_active_tab_id().to_string();
    let Some(row) = (0..terminals.row_count()).find_map(|i| {
        let r = terminals.row_data(i)?;
        (r.id.as_str() == active).then_some(r)
    }) else {
        // Welcome / unknown tab: no session → the SFTP panel leaves the dock.
        win.set_sftp_available(false);
        win.set_sftp_collapsed(false);
        win.set_sftp_path("/".into());
        win.set_sftp_status(Default::default());
        win.set_sftp_status_kind(0);
        win.set_sftp_loading(false);
        win.set_sftp_selected_count(0);
        win.set_sftp_sort_key(Default::default());
        win.set_sftp_sort_dir(0);
        win.set_sftp_entries(ModelRc::from(std::rc::Rc::new(
            VecModel::<SftpEntry>::default(),
        )));
        win.set_sftp_tree_nodes(ModelRc::from(std::rc::Rc::new(
            VecModel::<SftpTreeNode>::default(),
        )));
        win.set_sftp_bookmarks(ModelRc::from(std::rc::Rc::new(
            VecModel::<slint::SharedString>::default(),
        )));
        win.set_sftp_places_bookmarks(ModelRc::from(std::rc::Rc::new(
            VecModel::<slint::SharedString>::default(),
        )));
        win.set_sftp_recent(ModelRc::from(std::rc::Rc::new(
            VecModel::<slint::SharedString>::default(),
        )));
        win.set_sftp_bookmarked(false);
        win.set_sftp_tunnels(ModelRc::from(std::rc::Rc::new(
            VecModel::<TunnelInfo>::default(),
        )));
        win.set_sftp_current_tab_id("".into());
        win.set_sftp_dual(false);
        win.set_sftp_local_path(Default::default());
        win.set_sftp_local_status(Default::default());
        win.set_sftp_local_loading(false);
        win.set_sftp_local_selected_count(0);
        win.set_sftp_local_pane_width(0.0);
        win.set_sftp_local_entries(ModelRc::from(std::rc::Rc::new(
            VecModel::<SftpEntry>::default(),
        )));
        win.set_sftp_local_quick_places(ModelRc::from(std::rc::Rc::new(
            VecModel::<SftpLocalPlace>::default(),
        )));
        win.set_sftp_local_drive_places(ModelRc::from(std::rc::Rc::new(
            VecModel::<SftpLocalPlace>::default(),
        )));
        win.set_sftp_local_bookmarks(ModelRc::from(std::rc::Rc::new(
            VecModel::<slint::SharedString>::default(),
        )));
        win.set_sftp_local_recent(ModelRc::from(std::rc::Rc::new(
            VecModel::<slint::SharedString>::default(),
        )));
        win.set_sftp_local_bookmarked(false);
        return;
    };
    win.set_sftp_available(row.sftp_available);
    win.set_sftp_collapsed(row.sftp_collapsed);
    win.set_sftp_path(row.sftp_path.clone());
    win.set_sftp_status(row.sftp_status.clone());
    win.set_sftp_status_kind(row.sftp_status_kind);
    win.set_sftp_loading(row.sftp_loading);
    win.set_sftp_selected_count(row.sftp_selected_count);
    win.set_sftp_sort_key(row.sftp_sort_key.clone());
    win.set_sftp_sort_dir(row.sftp_sort_dir);
    win.set_sftp_entries(row.sftp_entries.clone());
    win.set_sftp_tree_nodes(row.sftp_tree_nodes.clone());
    win.set_sftp_bookmarks(row.sftp_bookmarks.clone());
    win.set_sftp_places_bookmarks(row.sftp_places_bookmarks.clone());
    win.set_sftp_recent(row.sftp_recent.clone());
    win.set_sftp_bookmarked(row.sftp_bookmarked);
    win.set_sftp_tunnels(row.tunnels.clone());
    win.set_sftp_current_tab_id(row.id.clone());
    win.set_sftp_dual(row.sftp_dual);
    win.set_sftp_local_path(row.sftp_local_path.clone());
    win.set_sftp_local_status(row.sftp_local_status.clone());
    win.set_sftp_local_loading(row.sftp_local_loading);
    win.set_sftp_local_selected_count(row.sftp_local_selected_count);
    win.set_sftp_local_pane_width(row.sftp_local_pane_width);
    win.set_sftp_local_entries(row.sftp_local_entries.clone());
    // Local-pane places (#532) are machine-wide but mirrored into every tab;
    // surface the active tab's copy so the docked panel stays in sync.
    win.set_sftp_local_quick_places(row.sftp_local_quick_places.clone());
    win.set_sftp_local_drive_places(row.sftp_local_drive_places.clone());
    win.set_sftp_local_bookmarks(row.sftp_local_bookmarks.clone());
    win.set_sftp_local_recent(row.sftp_local_recent.clone());
    win.set_sftp_local_bookmarked(row.sftp_local_bookmarked);
    // Panel extent follows the active tab's remembered size (#v0.5 keeps these
    // per-tab), so a wide panel on tab A isn't crushed to tab B's narrow one.
    win.set_sftp_panel_width(row.sftp_panel_width);
    win.set_sftp_panel_height(row.sftp_panel_height);
}

/// The edge (left|right|top|bottom) a window panel is currently expanded on,
/// `None` when folded, closed, or off (welcome not in sidebar mode).
pub(crate) fn panel_edge(window: &AppWindow, kind: &str) -> Option<&'static str> {
    let norm = |edge: &str| -> &'static str {
        match edge {
            "right" => "right",
            "top" => "top",
            "bottom" => "bottom",
            _ => "left",
        }
    };
    match kind {
        "sidebar" => {
            (!window.get_sidebar_collapsed()).then(|| norm(window.get_sidebar_dock().as_str()))
        }
        "welcome" => {
            (window.get_welcome_as_sidebar() && !window.get_welcome_collapsed())
                .then(|| norm(window.get_welcome_sidebar_dock().as_str()))
        }
        "quick" => {
            (window.get_quick_panel_open() && !window.get_quick_panel_collapsed())
                .then(|| norm(window.get_quick_panel_dock().as_str()))
        }
        "ai" => {
            (window.get_ai_panel_open() && !window.get_ai_panel_collapsed())
                .then(|| norm(window.get_ai_panel_dock().as_str()))
        }
        // SFTP is a normal window panel now (#dock-stack): expanded when the
        // active tab offers SFTP and hasn't collapsed it. Mirrored state lives
        // on the window (`sync_active_sftp_to_root`).
        "sftp" => {
            (window.get_sftp_available() && !window.get_sftp_collapsed())
                .then(|| norm(window.get_sftp_dock().as_str()))
        }
        _ => None,
    }
}

/// A panel's preferred thickness along its edge's normal: width on a
/// left/right edge, height on a top/bottom one (logical px).
pub(crate) fn panel_extent(window: &AppWindow, kind: &str) -> f32 {
    let horizontal_edge = matches!(panel_edge(window, kind), Some("left" | "right"));
    match kind {
        "sidebar" if horizontal_edge => window.get_sidebar_width(),
        "sidebar" => window.get_sidebar_height(),
        "welcome" => window.get_welcome_sidebar_width(),
        "quick" if horizontal_edge => window.get_quick_panel_width(),
        "quick" => window.get_quick_panel_height(),
        "ai" if horizontal_edge => window.get_ai_panel_width(),
        "ai" => window.get_ai_panel_height(),
        "sftp" if horizontal_edge => window.get_sftp_panel_width(),
        "sftp" => window.get_sftp_panel_height(),
        _ => 220.0,
    }
}

/// Whether the edge shows a 36px collapsed-panel ToolStrip band: any panel
/// whose folded form docks there (welcome counts only in sidebar mode, where
/// its strip actually renders). Multiple folded panels on one edge merge
/// into a single band, hence the boolean.
pub(crate) fn strip_on_edge(window: &AppWindow, edge: &str) -> bool {
    let docks = |s: slint::SharedString| s.as_str() == edge;
    (window.get_sidebar_collapsed() && docks(window.get_sidebar_dock()))
        || (window.get_welcome_as_sidebar()
            && window.get_welcome_collapsed()
            && docks(window.get_welcome_sidebar_dock()))
        || (window.get_quick_panel_open()
            && window.get_quick_panel_collapsed()
            && docks(window.get_quick_panel_dock()))
        || (window.get_ai_panel_open()
            && window.get_ai_panel_collapsed()
            && docks(window.get_ai_panel_dock()))
        // A collapsed SFTP panel keeps its 36px band on its docked edge, exactly
        // like the other window panels (#dock-stack).
        || (window.get_sftp_available()
            && window.get_sftp_collapsed()
            && docks(window.get_sftp_dock()))
}

/// Rebuild the edge stacks from the current window panel state, recompute the
/// dock geometry and push the result into the UI (models updated in place so
/// the rendered panel components keep their state). Call whenever a panel
/// docks, resizes or collapses — the close path then only has to persist the
/// already-synced stacks.
/// `area` is the dock-area frame in logical px (Slint reports it through
/// `dock-area-resized`) — panels lay out in that frame, not the window's.
///
/// While a panel drag is live the push is deferred: `set_vec` mid-drag can
/// re-key `for` rows and destroy the very component holding the pointer grab,
/// which strands the drag (its end handler never runs). The deferred run
/// retries until the flag drops, so a release commit still lands ~100ms
/// later.
pub(crate) fn refresh_dock(
    window: &AppWindow,
    dock_stacks: &Rc<RefCell<DockStacks>>,
    panels_model: &Rc<VecModel<PanelGeomInfo>>,
    dividers_model: &Rc<VecModel<DividerGeomInfo>>,
    area: (f32, f32),
) {
    refresh_dock_inner(
        window.as_weak(),
        dock_stacks.clone(),
        panels_model.clone(),
        dividers_model.clone(),
        area,
    );
}

pub(crate) fn refresh_dock_inner(
    window: slint::Weak<AppWindow>,
    dock_stacks: Rc<RefCell<DockStacks>>,
    panels_model: Rc<VecModel<PanelGeomInfo>>,
    dividers_model: Rc<VecModel<DividerGeomInfo>>,
    area: (f32, f32),
) {
    let Some(w) = window.upgrade() else {
        return;
    };
    let (cw, ch) = area;
    // The deferral itself: any refresh while a panel drag holds the pointer
    // is postponed (and re-postponed until the drag ends), so a mid-drag
    // relayout can never rebuild the dragged panel out from under the grab.
    if w.get_panel_dragging() {
        slint::Timer::single_shot(std::time::Duration::from_millis(100), move || {
            refresh_dock_inner(window, dock_stacks, panels_model, dividers_model, area);
        });
        return;
    }
    // Zen mode hides every docked panel and the terminal fills the window.
    if w.get_zen_mode() {
        panels_model.set_vec(Vec::new());
        dividers_model.set_vec(Vec::new());
        w.set_dock_central_x(0.0);
        w.set_dock_central_y(0.0);
        w.set_dock_central_w(cw);
        w.set_dock_central_h(ch);
        return;
    }
    let saved = dock_stacks.borrow().clone();
    let geom = {
        let mut cur = dock_stacks.borrow_mut();
        cur.rebuild_from(&saved, &|k| panel_edge(&w, k));
        cur.compute_geom(
            &|k| panel_extent(&w, k),
            &|edge| strip_on_edge(&w, edge),
            cw,
            ch,
        )
    };
    let panels: Vec<PanelGeomInfo> = geom
        .panels
        .iter()
        .map(|p| PanelGeomInfo {
            kind: p.kind.into(),
            edge: p.edge.into(),
            x: p.rect.x,
            y: p.rect.y,
            w: p.rect.w,
            h: p.rect.h,
        })
        .collect();
    // Same row-level path as the dividers below: VecModel::set_vec resets the
    // model, and a Slint repeater reacts by destroying and re-creating every
    // panel component — visible as the edge panels popping in a frame or two
    // late whenever a restore-only 1px geometry tweak churns the whole model.
    // Only a structural change (row count) may take the set_vec path.
    if panels_model.row_count() == panels.len() {
        for (i, p) in panels.into_iter().enumerate() {
            let unchanged = panels_model.row_data(i).is_some_and(|old| old == p);
            if !unchanged {
                panels_model.set_row_data(i, p);
            }
        }
    } else {
        panels_model.set_vec(panels);
    }
    let dividers: Vec<DividerGeomInfo> = geom
        .dividers
        .iter()
        .map(|d| DividerGeomInfo {
            edge: d.edge.into(),
            index: d.index as i32,
            x: d.rect.x,
            y: d.rect.y,
            w: d.rect.w,
            h: d.rect.h,
            vertical: d.vertical,
            axis_start: d.axis_start,
            axis_len: d.axis_len,
        })
        .collect();
    if dividers_model.row_count() == dividers.len() {
        for (i, d) in dividers.into_iter().enumerate() {
            let unchanged = dividers_model.row_data(i).is_some_and(|old| {
                old.edge == d.edge
                    && old.index == d.index
                    && old.x == d.x
                    && old.y == d.y
                    && old.w == d.w
                    && old.h == d.h
                    && old.vertical == d.vertical
                    && old.axis_start == d.axis_start
                    && old.axis_len == d.axis_len
            });
            if !unchanged {
                dividers_model.set_row_data(i, d);
            }
        }
    } else {
        dividers_model.set_vec(dividers);
    }
    w.set_dock_central_x(geom.central.x);
    w.set_dock_central_y(geom.central.y);
    w.set_dock_central_w(geom.central.w);
    w.set_dock_central_h(geom.central.h);
}

/// Hit-test a drag point (pane-area coords) to a target pane + drop zone, plus
/// the highlight rect the dropped tab would affect. Zone is one of
/// "tabstrip"/"left"/"right"/"up"/"down"/"center"; `None` when the point is
/// outside every pane. The 30% edge bands trigger a split; the tab strip and
/// middle drop into the pane's tab group.
pub(crate) fn drag_target(
    layout: &crate::layout::Layout,
    content: (f32, f32),
    x: f32,
    y: f32,
) -> Option<(u64, &'static str, (f32, f32, f32, f32))> {
    const STRIP: f32 = 36.0;
    const EDGE: f32 = 0.30;
    let (cw, ch) = (content.0.max(1.0), content.1.max(1.0));
    let (panes, _) = layout.flatten(0.0, 0.0, cw, ch);
    let p = panes
        .iter()
        .find(|p| x >= p.x && x < p.x + p.w && y >= p.y && y < p.y + p.h)?;
    let body_top = p.y + STRIP;
    if y < body_top {
        let ix = x.clamp(p.x + 3.0, p.x + p.w - 3.0) - 3.0;
        return Some((p.id, "tabstrip", (ix, p.y + 4.0, 6.0, STRIP - 8.0)));
    }
    let bw = p.w.max(1.0);
    let bh = (p.h - STRIP).max(1.0);
    let rx = (x - p.x) / bw;
    let ry = (y - body_top) / bh;
    let (dl, dr, dt, db) = (rx, 1.0 - rx, ry, 1.0 - ry);
    let m = dl.min(dr).min(dt).min(db);
    let (zone, rect) = if m > EDGE {
        ("center", (p.x, p.y, p.w, p.h))
    } else if m == dl {
        ("left", (p.x, p.y, p.w * 0.5, p.h))
    } else if m == dr {
        ("right", (p.x + p.w * 0.5, p.y, p.w * 0.5, p.h))
    } else if m == dt {
        ("up", (p.x, p.y, p.w, p.h * 0.5))
    } else {
        ("down", (p.x, p.y + p.h * 0.5, p.w, p.h * 0.5))
    };
    Some((p.id, zone, rect))
}

