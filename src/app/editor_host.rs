//! Detached built-in file viewer/editor window (#70), plus the literal
//! find helpers it shares with the editor tabs (#494, see `editor_tabs`).

use super::*;

/// Load a remote text file into the detached editor window and show it.
pub(super) fn open_document(
    editor: &EditorWindow,
    tab_id: &str,
    path: String,
    name: String,
    content: String,
    edit: bool,
) {
    editor.set_editor_tab_id(tab_id.into());
    editor.set_editor_path(path.into());
    editor.set_editor_name(name.into());
    editor_syntax::refresh(editor, &content);
    editor.set_editor_content(content.into());
    editor.set_editor_readonly(!edit);
    editor.set_editor_dirty(false);
    editor.set_editor_open(true);
    let _ = editor.show();
    editor.window().with_winit_window(|ww| ww.focus_window());
}

/// Save, close, syntax refresh and find/replace for the detached window.
pub(super) fn wire_editor_window(editor: &EditorWindow, sftp_handles: SftpHandles) {
    // Refresh syntax after edits. The debounced callback keeps typing
    // responsive while the native TextInput owns selection and IME handling.
    {
        let weak = editor.as_weak();
        editor.on_editor_highlight(move |text: SharedString| {
            if let Some(e) = weak.upgrade() {
                editor_syntax::refresh(&e, text.as_str());
            }
        });
    }

    // Bracket-pair highlight: the pane reports the caret offset whenever it
    // moves or the text is re-highlighted; the pair is merged into the spans
    // only when the caret actually sits next to a bracket, so plain caret
    // moves stay cheap (the O(1) fast path inside `bracket_pair`).
    {
        let weak = editor.as_weak();
        let mut last_pair: Option<(usize, usize)> = None;
        editor.on_editor_bracket_check(move |caret: i32| {
            let Some(e) = weak.upgrade() else { return };
            let text = e.get_editor_content();
            let path = e.get_editor_path();
            let pair = editor_syntax::bracket_pair(text.as_str(), caret, path.as_str());
            if pair.is_none() && last_pair.is_none() {
                return;
            }
            last_pair = pair;
            let spans =
                editor_syntax::spans_for(text.as_str(), path.as_str(), e.get_dark_mode(), pair);
            e.set_editor_syntax_spans(spans.into());
        });
    }

    // Save (Ctrl+S / button) writes the text back to the remote file (#70).
    // Read-only (view) sessions never save.
    {
        let weak = editor.as_weak();
        editor.on_save_file(move || {
            let Some(e) = weak.upgrade() else { return };
            if e.get_editor_readonly() {
                return;
            }
            let path = e.get_editor_path().to_string();
            let content = e.get_editor_content().to_string();
            let tab_id = e.get_editor_tab_id().to_string();
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(sftp) = handles.get(&tab_id) {
                    sftp.write_text(path, content);
                }
            }
            e.set_editor_dirty(false);
        });
    }

    // Closing discards unsaved edits. Saving is an explicit action; silently
    // uploading on X or Esc can overwrite a remote file the user only meant to
    // inspect (#287). The window asks before discarding unsaved changes.
    {
        let weak = editor.as_weak();
        editor.on_close_editor(move || {
            let Some(e) = weak.upgrade() else { return };
            e.set_editor_open(false);
            let _ = e.hide();
            e.set_editor_dirty(false);
            e.set_editor_find_query("".into());
            e.set_editor_replace_text("".into());
            e.set_editor_match_count(0);
            e.set_editor_find_position(-1);
        });
    }

    editor.on_editor_count_matches(|content: SharedString, query: SharedString| {
        count_matches(content.as_str(), query.as_str())
    });
    editor.on_editor_find_step(
        |content: SharedString, query: SharedString, current: i32, reverse: bool| {
            find_step(content.as_str(), query.as_str(), current, reverse)
        },
    );
    // The UI reports one incremental scroll delta per change tick; desktop
    // wheel steps arrive split across many small animation frames, so the
    // sub-line remainder is kept per editor and folded into the next call.
    {
        let pending = std::rc::Rc::new(std::cell::RefCell::new(0.0f32));
        editor.on_editor_scroll_select(
            move |content: SharedString, anchor: i32, focus: i32, delta_y: f32, line_height: f32| {
                let (lines, next) = scroll_accumulate(*pending.borrow(), delta_y, line_height);
                *pending.borrow_mut() = next;
                scroll_selection(content.as_str(), anchor, focus, lines)
            },
        );
    }
    {
        let weak = editor.as_weak();
        editor.on_editor_replace_all(move |query: SharedString, replacement: SharedString| {
            let Some(e) = weak.upgrade() else { return };
            if e.get_editor_readonly() || query.is_empty() {
                return;
            }
            let replaced = e
                .get_editor_content()
                .replace(query.as_str(), replacement.as_str());
            editor_syntax::refresh(&e, &replaced);
            e.set_editor_content(replaced.into());
            e.set_editor_dirty(true);
            e.set_editor_match_count(0);
        });
    }
}

// Find/replace (#287). Matching is literal and case-sensitive, which is
// predictable for configuration/source files.

pub(super) fn count_matches(content: &str, query: &str) -> i32 {
    if query.is_empty() {
        0
    } else {
        content.matches(query).count().min(i32::MAX as usize) as i32
    }
}

pub(super) fn find_step(
    content: &str,
    query: &str,
    current: i32,
    reverse: bool,
) -> EditorFindResult {
    if query.is_empty() {
        return EditorFindResult {
            start: 0,
            end: 0,
            count: 0,
        };
    }
    let positions: Vec<usize> = content
        .match_indices(query)
        .map(|(index, _)| index)
        .collect();
    let selected = if reverse {
        positions
            .iter()
            .rev()
            .copied()
            .find(|position| current < 0 || *position < current as usize)
            .or_else(|| positions.last().copied())
    } else {
        positions
            .iter()
            .copied()
            .find(|position| current < 0 || *position > current as usize)
            .or_else(|| positions.first().copied())
    };
    let start = selected.unwrap_or(0).min(i32::MAX as usize) as i32;
    EditorFindResult {
        start,
        end: start.saturating_add(query.len().min(i32::MAX as usize) as i32),
        count: positions.len().min(i32::MAX as usize) as i32,
    }
}

/// Fold a viewport scroll displacement into whole visual lines, carrying the
/// sub-line remainder into the next call. A desktop wheel notch is animated
/// over ~180 ms, so one notch reports many small per-frame deltas: rounding
/// each frame separately extended nothing. Accumulating instead, and
/// truncating toward zero, makes "grow by the scrolled distance" hold for
/// mouse wheels, touchpads and one-shot headless deltas alike.
pub(super) fn scroll_accumulate(prev_px: f32, delta_px: f32, line_height_px: f32) -> (i32, f32) {
    if line_height_px <= 0.0
        || !line_height_px.is_finite()
        || !prev_px.is_finite()
        || !delta_px.is_finite()
    {
        return (0, 0.0);
    }
    let total = prev_px + delta_px;
    let lines = (total / line_height_px) as i32;
    (lines, total - lines as f32 * line_height_px)
}

/// Extend a selection by `lines` whole visual lines revealed by a viewport
/// scroll (positive = the viewport moved up, so text above is revealed).
///
/// The end on the revealed side moves: scrolling up extends the top end, scrolling down the
/// bottom end. `anchor` keeps whichever side it sat on, so a backwards selection keeps growing
/// backwards. Offsets are byte offsets, the unit `set-selection-offsets` takes, and are clamped
/// to the document. Wrapping lines are measured against logical newlines, which is exact for
/// short lines and only approximate on very long wrapped lines.
pub(super) fn scroll_selection(content: &str, anchor: i32, focus: i32, lines: i32) -> EditorFindResult {
    let len = content.len().min(i32::MAX as usize) as i32;
    let anchor = anchor.clamp(0, len) as usize;
    let focus = focus.clamp(0, len) as usize;
    if anchor == focus || lines == 0 {
        return EditorFindResult {
            start: anchor as i32,
            end: focus as i32,
            count: 0,
        };
    }
    let (lo, hi) = if anchor <= focus { (anchor, focus) } else { (focus, anchor) };
    let (lo, hi) = if lines > 0 {
        (move_selection_back(content, lo, lines), hi)
    } else {
        (lo, move_selection_forward(content, hi, -lines))
    };
    let (start, end) = if anchor <= focus { (lo, hi) } else { (hi, lo) };
    EditorFindResult {
        start: start as i32,
        end: end as i32,
        count: 0,
    }
}

/// Move a selection end back `lines` logical lines, toward the start of the document. Each step
/// lands on a line boundary: an end inside a line snaps to that line's start, and an end already
/// on a line start steps back to the previous line's start.
fn move_selection_back(content: &str, pos: usize, lines: i32) -> usize {
    let mut p = pos.min(content.len());
    let mut left = lines;
    while left > 0 && p > 0 {
        let line_start = content[..p].rfind('\n').map_or(0, |i| i + 1);
        p = if line_start < p {
            line_start
        } else {
            content[..p - 1].rfind('\n').map_or(0, |i| i + 1)
        };
        left -= 1;
    }
    p
}

/// Move a selection end forward `lines` logical lines, toward the end of the document.
fn move_selection_forward(content: &str, pos: usize, lines: i32) -> usize {
    let mut p = pos.min(content.len());
    let mut left = lines;
    while left > 0 && p < content.len() {
        let line_end = content[p..].find('\n').map_or(content.len(), |i| p + i);
        p = (line_end + 1).min(content.len());
        left -= 1;
    }
    p
}
