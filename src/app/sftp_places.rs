//! SFTP path bookmarks + recent folders (#500).
//!
//! Places are stored per saved session (`ConfigStore::sftp_places`) and
//! mirrored into every open tab of that session (`TerminalState::
//! sftp_bookmarks` / `sftp_recent` / `sftp_bookmarked`), so two tabs on the
//! same host share one list. A folder becomes "recent" when its listing
//! actually arrives (`SessionEvent::SftpEntries`), so failed navigations
//! and typos never pollute the history.

use super::*;

use crate::config::SftpPlaces;
use crate::resource::TabStatuses;

pub(super) fn string_model(items: &[String]) -> ModelRc<SharedString> {
    ModelRc::from(Rc::new(VecModel::from(
        items.iter().map(SharedString::from).collect::<Vec<_>>(),
    )))
}

fn session_of(statuses: &TabStatuses, tab_id: &str) -> Option<String> {
    lock_or_recover(statuses)
        .get(tab_id)
        .map(|s| s.session_id.clone())
        .filter(|id| !id.is_empty())
}

/// Push `places` into every open tab (in this window) of `session_id`.
///
/// `sftp_bookmarks` on the tab row belongs to the older per-server menu; the
/// #500 per-session list lives in its own `sftp_places_bookmarks` field so the
/// two systems never fight over one model.
fn apply_to_tabs(
    window: &AppWindow,
    statuses: &TabStatuses,
    session_id: &str,
    places: &SftpPlaces,
) {
    let tabs: Vec<String> = lock_or_recover(statuses)
        .iter()
        .filter(|(_, st)| st.session_id == session_id)
        .map(|(tab, _)| tab.clone())
        .collect();
    let terminals = window.get_terminals();
    let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
        return;
    };
    for i in 0..tm.row_count() {
        let Some(mut row) = tm.row_data(i) else { continue };
        if !tabs.iter().any(|t| t == row.id.as_str()) {
            continue;
        }
        row.sftp_places_bookmarks = string_model(&places.bookmarks);
        row.sftp_recent = string_model(&places.recent);
        row.sftp_bookmarked = places.is_bookmarked(row.sftp_path.as_str());
        tm.set_row_data(i, row);
    }
    mirror_places_to_panel(window, tm);
}

/// Re-mirror the active tab's places onto AppWindow's properties: these
/// callbacks fire without an intervening session event, so without this the
/// docked SftpPanel would keep showing stale stars / lists until the next tab
/// switch or directory navigation.
fn mirror_places_to_panel(window: &AppWindow, terminals: &VecModel<TerminalState>) {
    super::sync_active_sftp_to_root(window, terminals);
}

/// Change one session's places, persist, and refresh its open tabs.
fn edit_places(
    window: &AppWindow,
    store: &RefCell<ConfigStore>,
    statuses: &TabStatuses,
    tab_id: &str,
    f: impl FnOnce(&mut SftpPlaces) -> bool,
) {
    let Some(sid) = session_of(statuses, tab_id) else {
        return;
    };
    let (changed, places) = {
        let mut s = store.borrow_mut();
        let changed = s.update_sftp_places(&sid, f);
        if changed {
            if let Err(e) = s.save() {
                tracing::warn!("saving SFTP places failed: {e:#}");
            }
        }
        (changed, s.sftp_places(&sid))
    };
    if changed {
        apply_to_tabs(window, statuses, &sid, &places);
    } else {
        // Nothing to persist, but the star still has to follow the path.
        set_bookmarked_flag(window, tab_id, &places);
        let terminals = window.get_terminals();
        if let Some(tm) = terminals
            .as_any()
            .downcast_ref::<VecModel<TerminalState>>()
        {
            mirror_places_to_panel(window, tm);
        }
    }
}

fn set_bookmarked_flag(window: &AppWindow, tab_id: &str, places: &SftpPlaces) {
    let terminals = window.get_terminals();
    let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
        return;
    };
    for i in 0..tm.row_count() {
        let Some(mut row) = tm.row_data(i) else { continue };
        if row.id.as_str() != tab_id {
            continue;
        }
        let flag = places.is_bookmarked(row.sftp_path.as_str());
        if row.sftp_bookmarked != flag {
            row.sftp_bookmarked = flag;
            tm.set_row_data(i, row);
        }
        break;
    }
}

pub(super) fn wire_sftp_places(
    window: &AppWindow,
    store: Rc<RefCell<ConfigStore>>,
    statuses: TabStatuses,
) {
    macro_rules! wire {
        ($on:ident, |$w:ident, $tab:ident $(, $arg:ident)*| $body:expr) => {{
            let store = store.clone();
            let statuses = statuses.clone();
            let weak = window.as_weak();
            window.$on(move |$tab: SharedString $(, $arg: SharedString)*| {
                let Some($w) = weak.upgrade() else { return };
                edit_places(&$w, &store, &statuses, $tab.as_str(), $body);
            });
        }};
    }

    // A listing arrived: remember the folder (no-op if already on top).
    wire!(on_sftp_record_visit, |w, tab, path| move |p: &mut SftpPlaces| {
        p.record_visit(path.as_str())
    });
    wire!(on_sftp_toggle_bookmark, |w, tab, path| move |p: &mut SftpPlaces| {
        p.toggle_bookmark(path.as_str());
        true
    });
    wire!(on_sftp_remove_bookmark, |w, tab, path| move |p: &mut SftpPlaces| {
        let before = p.bookmarks.len();
        p.remove_bookmark(path.as_str());
        p.bookmarks.len() != before
    });
    wire!(on_sftp_clear_recent, |w, tab| |p: &mut SftpPlaces| {
        let had = !p.recent.is_empty();
        p.clear_recent();
        had
    });

    // Opening the places menu re-syncs from the store, picking up changes
    // made from a tab of the same session in another window.
    {
        let store = store.clone();
        let statuses = statuses.clone();
        let weak = window.as_weak();
        window.on_sftp_places_refresh(move |tab: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            if let Some(sid) = session_of(&statuses, tab.as_str()) {
                let places = store.borrow().sftp_places(&sid);
                apply_to_tabs(&w, &statuses, &sid, &places);
            }
        });
    }
}

#[cfg(test)]
#[path = "../../tests/app/sftp_places/mod.rs"]
mod tests;
