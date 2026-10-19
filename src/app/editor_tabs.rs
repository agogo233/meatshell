//! Editor tabs (#494): with Settings → SFTP "Open files in a tab" on, a text
//! file opened from the SFTP panel becomes a tab of kind "editor" in the
//! window's panes instead of the detached `EditorWindow`.
//!
//! Each open file is one `EditorDoc` row in `AppWindow.editor-docs`, keyed by
//! its tab id. The row is authoritative: the pane reports every edit back, so
//! a tab moved to another pane (or re-shown) reloads the latest text. The
//! layout is not persisted across restarts, so editor tabs never come back on
//! their own; they are also not movable between windows (see tab_transfer).

use super::*;
use std::cell::Cell;

/// Models the editor tabs touch to add, focus and re-flatten tabs.
pub(super) struct EditorTabModels {
    pub(super) docs: Rc<VecModel<EditorDoc>>,
    pub(super) tabs: Rc<VecModel<TabInfo>>,
    pub(super) layout: Rc<RefCell<crate::layout::Layout>>,
    pub(super) content_size: Rc<Cell<(f32, f32)>>,
    pub(super) panes: Rc<VecModel<PaneInfo>>,
    pub(super) splitters: Rc<VecModel<SplitterInfo>>,
}

impl EditorTabModels {
    fn refresh(&self, win: &AppWindow) {
        // Snapshot the layout so no RefCell guard is alive while Slint models
        // change (a binding callback could re-enter `layout.borrow*()`).
        let lay = (*self.layout.borrow()).clone();
        refresh_panes(
            win,
            &lay,
            self.content_size.get(),
            &self.tabs,
            &self.panes,
            &self.splitters,
        );
    }

    fn focus(&self, tab_id: &str) {
        let mut lay = self.layout.borrow_mut();
        if let Some(leaf) = lay.leaf_of_tab(tab_id) {
            lay.focused = leaf;
            if let Some(l) = lay.leaf_mut(leaf) {
                l.active = tab_id.to_string();
            }
        }
    }
}

thread_local! {
    static NEXT_EDITOR_TAB: Cell<u64> = const { Cell::new(1) };
}

fn next_tab_id() -> String {
    NEXT_EDITOR_TAB.with(|n| {
        let id = n.get();
        n.set(id + 1);
        format!("editor-{id}")
    })
}

/// Apply `f` to the doc row `id` in place. Returns false when there is none.
fn update_doc(docs: &VecModel<EditorDoc>, id: &str, f: impl FnOnce(&mut EditorDoc)) -> bool {
    for i in 0..docs.row_count() {
        if let Some(mut row) = docs.row_data(i) {
            if row.id.as_str() == id {
                f(&mut row);
                docs.set_row_data(i, row);
                return true;
            }
        }
    }
    false
}

fn find_doc(docs: &VecModel<EditorDoc>, id: &str) -> Option<EditorDoc> {
    docs.iter().find(|d| d.id.as_str() == id)
}

/// Show an editor tab's unsaved state on its strip dot. Tab strips render
/// per-pane snapshots of `tabs`, so update those too (as session events do).
fn set_tab_saved(win: &AppWindow, tabs: &VecModel<TabInfo>, id: &str, saved: bool) {
    let apply = |model: &VecModel<TabInfo>| {
        for i in 0..model.row_count() {
            if let Some(mut row) = model.row_data(i) {
                if row.id.as_str() == id {
                    if row.connected != saved {
                        row.connected = saved;
                        model.set_row_data(i, row);
                    }
                    return;
                }
            }
        }
    };
    apply(tabs);
    let panes = win.get_panes();
    if let Some(pm) = panes.as_any().downcast_ref::<VecModel<PaneInfo>>() {
        for pi in 0..pm.row_count() {
            if let Some(pane) = pm.row_data(pi) {
                if let Some(tm) = pane.tabs.as_any().downcast_ref::<VecModel<TabInfo>>() {
                    apply(tm);
                }
            }
        }
    }
}

fn recolour(doc: &mut EditorDoc, dark: bool) {
    doc.language = editor_syntax::language(doc.path.as_str()).into();
    doc.syntax_spans =
        editor_syntax::themed_spans(doc.content.as_str(), doc.path.as_str(), dark).into();
    doc.syntax_source = doc.content.clone();
}

/// Tab-close hook: an editor tab with unsaved edits asks first. Returns true
/// when the close was deferred to the confirmation dialog.
pub(super) fn close_needs_confirm(win: &AppWindow, tab_id: &str) -> bool {
    let docs_rc = win.get_editor_docs();
    let Some(docs) = docs_rc.as_any().downcast_ref::<VecModel<EditorDoc>>() else {
        return false;
    };
    match find_doc(docs, tab_id) {
        Some(doc) if doc.dirty && !doc.readonly => {
            win.set_editor_discard_id(tab_id.into());
            win.set_editor_discard_confirm_open(true);
            true
        }
        _ => false,
    }
}

/// Tab-close hook, after the tab left the models: drop a closed editor tab's
/// document, and mark documents opened through a closed terminal as orphaned
/// (their SFTP channel is gone, so Save is disabled).
pub(super) fn forget_tab(win: &AppWindow, tab_id: &str) {
    let docs_rc = win.get_editor_docs();
    let Some(docs) = docs_rc.as_any().downcast_ref::<VecModel<EditorDoc>>() else {
        return;
    };
    if let Some(i) =
        (0..docs.row_count()).find(|&i| docs.row_data(i).is_some_and(|d| d.id.as_str() == tab_id))
    {
        docs.remove(i);
        return;
    }
    for i in 0..docs.row_count() {
        if let Some(mut row) = docs.row_data(i) {
            if row.owner.as_str() == tab_id && !row.orphaned {
                row.orphaned = true;
                docs.set_row_data(i, row);
            }
        }
    }
}

pub(super) fn wire_editor_tabs(
    window: &AppWindow,
    models: EditorTabModels,
    sftp_handles: SftpHandles,
) {
    let models = Rc::new(models);

    // Open (or re-open) a file from the SFTP panel as an editor tab.
    {
        let weak = window.as_weak();
        let models = models.clone();
        window.on_open_editor_tab(
            move |owner: SharedString,
                  path: SharedString,
                  name: SharedString,
                  content: SharedString,
                  readonly: bool| {
                let Some(w) = weak.upgrade() else { return };
                let dark = w.get_dark_mode();
                let docs = &models.docs;
                // The same file from the same terminal is already open: switch
                // to it, and reload it unless it has unsaved edits.
                let existing = docs
                    .iter()
                    .find(|d| d.owner == owner && d.path == path)
                    .map(|d| d.id.to_string());
                let id = match existing {
                    Some(id) => {
                        update_doc(docs, &id, |d| {
                            if !d.dirty {
                                d.content = content.clone();
                                d.revision += 1;
                                d.readonly = readonly;
                                recolour(d, dark);
                            }
                        });
                        id
                    }
                    None => {
                        let id = next_tab_id();
                        let mut doc = EditorDoc {
                            id: id.clone().into(),
                            owner,
                            path,
                            name: name.clone(),
                            content,
                            revision: 0,
                            readonly,
                            dirty: false,
                            orphaned: false,
                            language: SharedString::default(),
                            syntax_source: SharedString::default(),
                            syntax_spans: SharedString::default(),
                        };
                        recolour(&mut doc, dark);
                        docs.push(doc);
                        models.tabs.push(TabInfo {
                            id: id.clone().into(),
                            title_len: tab_title_len(name.as_str()),
                            title: name,
                            kind: "editor".into(),
                            connected: true,
                        });
                        // Joins the focused pane, like a new terminal tab.
                        models.layout.borrow_mut().add_tab(id.clone());
                        id
                    }
                };
                models.focus(&id);
                models.refresh(&w);
            },
        );
    }

    // Every edit lands in the row so the text survives re-shows and moves.
    {
        let weak = window.as_weak();
        let models = models.clone();
        window.on_editor_doc_edited(move |id: SharedString, content: SharedString| {
            let mut became_dirty = false;
            update_doc(&models.docs, id.as_str(), |d| {
                d.content = content;
                became_dirty = !d.dirty;
                d.dirty = true;
            });
            if became_dirty {
                if let Some(w) = weak.upgrade() {
                    set_tab_saved(&w, &models.tabs, id.as_str(), false);
                }
            }
        });
    }

    // Debounced syntax refresh from the pane.
    {
        let weak = window.as_weak();
        let models = models.clone();
        window.on_editor_doc_highlight(move |id: SharedString, text: SharedString| {
            let dark = weak.upgrade().map(|w| w.get_dark_mode()).unwrap_or(true);
            update_doc(&models.docs, id.as_str(), |d| {
                d.syntax_spans =
                    editor_syntax::themed_spans(text.as_str(), d.path.as_str(), dark).into();
                d.syntax_source = text;
            });
        });
    }

    // Save through the SFTP channel of the terminal the file came from.
    {
        let weak = window.as_weak();
        let models = models.clone();
        window.on_editor_doc_save(move |id: SharedString| {
            let Some(doc) = find_doc(&models.docs, id.as_str()) else {
                return;
            };
            if doc.readonly || doc.orphaned {
                return;
            }
            let sent = sftp_handles
                .lock()
                .ok()
                .and_then(|handles| {
                    handles.get(doc.owner.as_str()).map(|sftp| {
                        sftp.write_text(doc.path.to_string(), doc.content.to_string());
                    })
                })
                .is_some();
            if !sent {
                return;
            }
            update_doc(&models.docs, id.as_str(), |d| d.dirty = false);
            if let Some(w) = weak.upgrade() {
                set_tab_saved(&w, &models.tabs, id.as_str(), true);
            }
        });
    }

    {
        let weak = window.as_weak();
        let models = models.clone();
        window.on_editor_doc_replace_all(
            move |id: SharedString, query: SharedString, replacement: SharedString| {
                if query.is_empty() {
                    return;
                }
                let dark = weak.upgrade().map(|w| w.get_dark_mode()).unwrap_or(true);
                let mut changed = false;
                update_doc(&models.docs, id.as_str(), |d| {
                    if d.readonly || !d.content.contains(query.as_str()) {
                        return;
                    }
                    d.content = d
                        .content
                        .replace(query.as_str(), replacement.as_str())
                        .into();
                    d.revision += 1;
                    d.dirty = true;
                    recolour(d, dark);
                    changed = true;
                });
                if changed {
                    if let Some(w) = weak.upgrade() {
                        set_tab_saved(&w, &models.tabs, id.as_str(), false);
                    }
                }
            },
        );
    }

    // Theme switch: recolour every open document.
    {
        let weak = window.as_weak();
        let models = models.clone();
        window.on_refresh_editor_syntax(move || {
            let Some(w) = weak.upgrade() else { return };
            let dark = w.get_dark_mode();
            for i in 0..models.docs.row_count() {
                if let Some(mut d) = models.docs.row_data(i) {
                    recolour(&mut d, dark);
                    models.docs.set_row_data(i, d);
                }
            }
        });
    }

    // "Discard and close" in the unsaved-changes dialog: clear the flag and
    // run the normal tab close, which no longer needs to ask.
    {
        let weak = window.as_weak();
        let models = models.clone();
        window.on_editor_discard_confirmed(move |id: SharedString| {
            update_doc(&models.docs, id.as_str(), |d| d.dirty = false);
            if let Some(w) = weak.upgrade() {
                w.invoke_pane_tab_closed(0, id);
            }
        });
    }

    window.on_editor_count_matches(|content: SharedString, query: SharedString| {
        count_matches(content.as_str(), query.as_str())
    });
    window.on_editor_find_step(
        |content: SharedString, query: SharedString, current: i32, reverse: bool| {
            find_step(content.as_str(), query.as_str(), current, reverse)
        },
    );
}
