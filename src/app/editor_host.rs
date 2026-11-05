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
    editor.set_editor_save_ack(0);
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
            e.set_editor_save_ack(0);
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
    // sub-line remainder and any snapped crossing are kept per editor.
    {
        let state = std::rc::Rc::new(WheelSelectState::new());
        editor.on_editor_scroll_select(
            move |content: SharedString, anchor: i32, focus: i32, delta_y: f32, line_height: f32| {
                state.select(content.as_str(), anchor, focus, delta_y, line_height)
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

/// Stretch or pull back a selection as the viewport scrolls by `lines` whole
/// visual lines (positive = the viewport moved up, negative = down).
///
/// `anchor` is the fixed rubber-band end (the side the user pressed); `focus`
/// is the live end. Rolling only moves `focus`: rolling away from `anchor`
/// grows the band, rolling back pulls it in, and when one step would land
/// past `anchor` the live end snaps to `anchor` and the unconsumed lines come
/// back in `count` so the caller can keep them and let the band grow on the
/// other side on the next call. The result always keeps the input order
/// (`start` = anchor side, `end` = live side): writing it back through
/// `set-selection-offsets` therefore never rewrites the input's anchor field,
/// so the pressed end cannot drift. Offsets are byte offsets, clamped to the
/// document; wrapped lines are measured against logical newlines, which is
/// exact near the ends for short lines and approximate on very long wrapped
/// lines.
pub(super) fn scroll_selection(content: &str, anchor: i32, focus: i32, lines: i32) -> EditorFindResult {
    let len = content.len().min(i32::MAX as usize) as i32;
    let anchor = anchor.clamp(0, len) as usize;
    let focus = focus.clamp(0, len) as usize;
    if lines == 0 {
        return EditorFindResult {
            start: anchor as i32,
            end: focus as i32,
            count: 0,
        };
    }
    let up = lines > 0;
    // Rolling towards the anchor can cross it; rolling away (and growing an
    // empty band, which the caller only does after a snap) cannot.
    let towards = if up { focus > anchor } else { focus < anchor };
    if !towards {
        let moved = if up {
            move_selection_back(content, focus, lines)
        } else {
            move_selection_forward(content, focus, -lines)
        };
        return EditorFindResult {
            start: anchor as i32,
            end: moved as i32,
            count: 0,
        };
    }
    // Walk towards the anchor and stop at the step that reaches it, so a fast
    // scroll shrinks the band to nothing before the next call grows the other
    // side, instead of jumping across the anchor in one frame.
    let mut p = focus;
    let mut consumed = 0;
    let crossed = loop {
        p = if up { step_back_line(content, p) } else { step_forward_line(content, p) };
        consumed += 1;
        if (up && p <= anchor) || (!up && p >= anchor) {
            break true;
        }
        if consumed as i32 == lines.abs() {
            break false;
        }
    };
    if crossed {
        EditorFindResult {
            start: anchor as i32,
            end: anchor as i32,
            count: lines.abs() - consumed,
        }
    } else {
        EditorFindResult {
            start: anchor as i32,
            end: p as i32,
            count: 0,
        }
    }
}

/// One logical-line step towards the start of the document: an end inside a
/// line snaps to that line's start, an end already on a line start moves to
/// the previous line's start.
fn step_back_line(content: &str, p: usize) -> usize {
    let p = p.min(content.len());
    if p == 0 {
        return 0;
    }
    let line_start = content[..p].rfind('\n').map_or(0, |i| i + 1);
    if line_start < p {
        line_start
    } else {
        content[..p - 1].rfind('\n').map_or(0, |i| i + 1)
    }
}

/// One logical-line step towards the end of the document: the character after
/// the next newline, or the end of the document.
fn step_forward_line(content: &str, p: usize) -> usize {
    let p = p.min(content.len());
    if p >= content.len() {
        return p;
    }
    let line_end = content[p..].find('\n').map_or(content.len(), |i| p + i);
    (line_end + 1).min(content.len())
}

/// Move a selection end back `lines` logical lines, toward the start of the
/// document.
fn move_selection_back(content: &str, pos: usize, lines: i32) -> usize {
    let mut p = pos.min(content.len());
    for _ in 0..lines.max(0) {
        p = step_back_line(content, p);
    }
    p
}

/// Move a selection end forward `lines` logical lines, toward the end of the
/// document.
fn move_selection_forward(content: &str, pos: usize, lines: i32) -> usize {
    let mut p = pos.min(content.len());
    for _ in 0..lines.max(0) {
        p = step_forward_line(content, p);
    }
    p
}

/// Per-editor scroll-extension state for the `editor-scroll-select` callback.
/// A desktop wheel arrives split across many small animation frames, so each
/// call only reports its own incremental delta and the sub-line remainder is
/// folded in here; a scroll that snapped the live end onto the anchor owes
/// its unused lines, kept as `carry_lines` until the next call. Session
/// state resets when the anchor byte changes (a new press elsewhere) or when
/// the live end moves back on its own; a re-press at the same byte keeps the
/// previous session's flags, so wheeling from that press can grow a band out
/// of the press point rather than staying a plain caret.
pub(super) struct WheelSelectState {
    pending_px: std::cell::RefCell<f32>,
    carry_lines: std::cell::Cell<i32>,
    last_anchor: std::cell::Cell<i32>,
    had_selection: std::cell::Cell<bool>,
}

impl WheelSelectState {
    pub(super) fn new() -> Self {
        Self {
            pending_px: std::cell::RefCell::new(0.0),
            carry_lines: std::cell::Cell::new(0),
            last_anchor: std::cell::Cell::new(i32::MIN),
            had_selection: std::cell::Cell::new(false),
        }
    }

    /// Fold this call's scroll into the selection: `focus` starts from the
    /// input's live end, `anchor` is the press point the band is pinned to.
    pub(super) fn select(
        &self,
        content: &str,
        anchor: i32,
        focus: i32,
        delta_y: f32,
        line_height: f32,
    ) -> EditorFindResult {
        if self.last_anchor.get() != anchor {
            self.last_anchor.set(anchor);
            self.carry_lines.set(0);
            self.had_selection.set(false);
        }
        if focus != anchor {
            self.carry_lines.set(0);
        }
        let (lines, next) = scroll_accumulate(*self.pending_px.borrow(), delta_y, line_height);
        *self.pending_px.borrow_mut() = next;
        let total = lines + self.carry_lines.get();
        let grown = if focus == anchor && !self.had_selection.get() {
            // No selection yet in this drag: wheeling must keep the caret
            // alone, not grow one out of the press point.
            EditorFindResult {
                start: anchor,
                end: focus,
                count: 0,
            }
        } else {
            self.had_selection.set(true);
            scroll_selection(content, anchor, focus, total)
        };
        self.carry_lines.set(grown.count);
        grown
    }
}
