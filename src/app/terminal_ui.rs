use super::*;

pub(super) fn history_model(store: &ConfigStore) -> ModelRc<SharedString> {
    let rows: Vec<SharedString> = store
        .command_history()
        .iter()
        .map(|s| s.clone().into())
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

pub(super) fn output_highlight_rule_model(store: &ConfigStore) -> ModelRc<OutputRuleItem> {
    let rows: Vec<OutputRuleItem> = store
        .output_highlight_rules()
        .iter()
        .map(|rule| OutputRuleItem {
            pattern: rule.pattern.clone().into(),
            regex: rule.regex,
            case_sensitive: rule.case_sensitive,
            whole_line: rule.whole_line,
            color: match rule.color.as_str() {
                "yellow" | "green" | "cyan" | "magenta" | "gray" => rule.color.clone(),
                _ => "red".to_string(),
            }
            .into(),
            enabled: rule.enabled,
        })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

pub(super) fn parse_hex_color(value: &str) -> Option<slint::Color> {
    let digits = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let red = u8::from_str_radix(&digits[0..2], 16).ok()?;
    let green = u8::from_str_radix(&digits[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&digits[4..6], 16).ok()?;
    Some(slint::Color::from_rgb_u8(red, green, blue))
}

pub(super) fn validate_output_highlight_rule(
    pattern: &str,
    is_regex: bool,
    case_sensitive: bool,
) -> std::result::Result<(), String> {
    if pattern.is_empty() {
        return Err(t(
            "请输入关键词或正则表达式",
            "Enter a keyword or regular expression",
        )
        .into());
    }
    if pattern.chars().count() > 512 {
        return Err(t(
            "规则不能超过 512 个字符",
            "Rules cannot exceed 512 characters",
        )
        .into());
    }
    if is_regex {
        regex::RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|error| {
                format!(
                    "{}: {error}",
                    t("无效的正则表达式", "Invalid regular expression")
                )
            })?;
    }
    Ok(())
}

/// Build the filtered history-view rows for the dropdown. Empty query returns
/// every command oldest-first (newest last, #55/#101). A non-empty query ranks
/// commands by a fuzzy score — substring matches beat subsequence matches, and
/// start-of-word / consecutive-character runs score higher — sorted ascending so
/// the best match lands at the bottom, where the dropdown highlights by default.
pub(super) fn history_view_rows(history: &[String], query: &str) -> Vec<SharedString> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return history.iter().map(|command| command.clone().into()).collect();
    }
    let mut scored: Vec<(i64, usize, String)> = history
        .iter()
        .enumerate()
        .filter_map(|(index, command)| {
            fuzzy_score(&command.to_lowercase(), &q).map(|score| (score, index, command.clone()))
        })
        .collect();
    // Ascending score; ties keep the original (oldest→newest) order.
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, command)| command.into()).collect()
}

/// Fuzzy score of `q` (lowercased) against `text` (lowercased). `None` when the
/// query is neither a substring nor a subsequence. Higher is a better match.
fn fuzzy_score(text: &str, q: &str) -> Option<i64> {
    if q.is_empty() {
        return Some(0);
    }
    let chars: Vec<char> = text.chars().collect();
    let qchars: Vec<char> = q.chars().collect();

    // Substring match: strong signal, boosted by an early / word-start hit.
    if let Some(pos) = text.find(q) {
        let mut score = 1000i64;
        if pos == 0 {
            score += 400;
        } else if chars
            .get(text[..pos].chars().count() - 1)
            .is_some_and(|c| *c == ' ' || *c == '\t')
        {
            score += 200;
        }
        // Earlier occurrences rank slightly higher.
        score -= pos.min(150) as i64;
        return Some(score);
    }

    // Subsequence match: weaker, rewarded for consecutive runs and word starts.
    let mut score = 100i64;
    let mut qi = 0usize;
    let mut prev_match: Option<usize> = None;
    for (ti, &c) in chars.iter().enumerate() {
        if qi < qchars.len() && c == qchars[qi] {
            if let Some(p) = prev_match {
                if ti == p + 1 {
                    score += 15; // consecutive run
                }
            }
            if ti == 0 || matches!(chars.get(ti - 1), Some(' ') | Some('\t') | Some('_') | Some('-') | Some('/')) {
                score += 40; // word-start hit
            }
            prev_match = Some(ti);
            qi += 1;
        }
    }
    if qi == qchars.len() {
        Some(score)
    } else {
        None
    }
}

/// Build the filtered history-view model for the dropdown: case-insensitive
/// substring matches of `query`, ordered from oldest to newest (#101).
pub(super) fn history_view_model(store: &ConfigStore, query: &str) -> ModelRc<SharedString> {
    let rows = history_view_rows(store.command_history(), query);
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

/// One-line previews parallel to [`history_view_model`] (same order and
/// length). A history entry pasted with line breaks (e.g. a block of `top`
/// output sent via the command bar) used to be drawn with its real newlines
/// inside a 28px row and overlapped the rows around it (#419). The raw
/// strings stay in `history-view` for run / copy / delete.
pub(super) fn history_preview_model(store: &ConfigStore, query: &str) -> ModelRc<SharedString> {
    let rows: Vec<SharedString> = history_view_rows(store.command_history(), query)
        .iter()
        .map(|command| command_preview(command).into())
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

/// Refresh the history dropdown's raw rows and their display previews together.
pub(super) fn set_history_view(window: &AppWindow, store: &ConfigStore, query: &str) {
    window.set_history_view(history_view_model(store, query));
    window.set_history_preview(history_preview_model(store, query));
}

#[cfg(test)]
#[path = "../../tests/app/command_history/mod.rs"]
mod history_view_tests;

/// Find every (case-insensitive) occurrence of `query` across the currently
/// displayed rows and return highlight rectangles in GRID-COLUMN space (wide
/// CJK glyphs count as two columns, so highlights line up over the text #132).
pub(super) fn compute_find_matches(rows: &[String], query: &str) -> Vec<TermMatch> {
    let mut out: Vec<TermMatch> = Vec::new();
    if query.is_empty() {
        return out;
    }
    // Unicode folding, to match the history-mode index: a plain
    // `to_ascii_lowercase` counted "É" as a hit without painting a highlight
    // for it. Folding must stay one char in, one char out — `İ` and `ß` fold
    // to two or more chars, and the grid-column offsets below index into a
    // prefix built from the unfolded characters, so a multi-char fold would
    // run past the end and panic. Those few glyphs keep their original shape.
    fn fold_char(c: char) -> char {
        let mut chars = c.to_lowercase();
        match (chars.next(), chars.next()) {
            (Some(f), None) => f,
            _ => c,
        }
    }
    let q: Vec<char> = query.chars().map(fold_char).collect();
    if q.is_empty() {
        return out;
    }
    for (r, line) in rows.iter().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        let lower: Vec<char> = chars.iter().map(|c| fold_char(*c)).collect();
        let prefix = cell_prefix(&chars);
        let mut i = 0usize;
        while i + q.len() <= lower.len() {
            if lower[i..i + q.len()] == q[..] {
                let col = prefix[i] as i32;
                let len = (prefix[i + q.len()] - prefix[i]) as i32;
                out.push(TermMatch {
                    row: r as i32,
                    col,
                    len,
                });
                i += q.len();
            } else {
                i += 1;
            }
        }
    }
    out
}

/// Apply a settled terminal size to the PTY + vt100 grid. Factored out of the
/// resize callback so that callback can debounce — a layout reflow can briefly
/// report a near-zero width, collapsing term-cols to its 10-col floor; applying
/// that to the remote PTY reflows vt100 and garbles running output like a
/// `git clone` progress meter (#163). Debouncing means only the settled size
/// ever reaches the server.
pub(super) fn apply_terminal_resize(
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    bufs: &TermBuffers,
    last_term_size: &Arc<Mutex<(u32, u32)>>,
    tab_id: &str,
    cols: u32,
    rows: u32,
) {
    *lock_or_recover(&last_term_size) = (cols, rows);
    if let Some(handle) = handles.borrow().get(tab_id) {
        handle.resize(cols, rows);
    }
    if let Some(h) = term_buf(bufs, tab_id) {
        let mut buf = lock_or_recover(&h);
        let (old_rows, old_cols) = buf.parser.screen().size();
        let (new_rows, new_cols) = (rows as u16, cols as u16);
        if (new_rows, new_cols) != (old_rows, old_cols) {
            if buf.parser.screen().alternate_screen() {
                // Alt-screen (tmux/vim/btop): the remote redraws the whole screen
                // on SIGWINCH, so just resize the grid and let that redraw fill it.
                buf.parser.screen_mut().set_size(new_rows, new_cols);
            } else {
                // Reflow already-printed output to the new width by replaying the
                // byte stream — vt100's set_size only truncates/pads (#169).
                buf.reflow(new_rows, new_cols);
            }
            // The pre/post-resize screens differ; drop the scroll-detection
            // snapshot so the next output isn't mis-read as a scroll.
            buf.prev.clear();
        }
    }
}

/// Recompute spans + cursor + find/selection highlights for one tab from its
/// current vt100 screen (respecting scrollback) and push them to the model.
/// Used by the scroll + selection callbacks and by every output flush (see
/// `do_tab_render_flush`), so it is the single place the caret cell and its
/// DECTCEM visibility reach the model.
pub(super) fn rebuild_tab_display(win: &AppWindow, bufs: &TermBuffers, tab_id: &str) {
    let data = with_term_buf(bufs, tab_id, |buf| {
        let cols = buf.parser.screen().size().1;
        let b = buf.render(); // also refreshes buf.displayed_text + buf.action_links
        let matches = compute_find_matches(&buf.displayed_text, &buf.find_query);
        let links: Vec<TermMatch> = buf
            .action_links
            .iter()
            .map(|h| TermMatch {
                row: h.row,
                col: h.col,
                len: h.len,
            })
            .collect();
        let sel = buf.selection_rects_visible(cols);
        // Extent-based (not visible-rect-based): a selection scrolled out of
        // view still extracts text, so the ctx-menu item must stay available.
        let has_sel = buf.selection_has_extent();
        let (fa, ft) = (buf.find_active, buf.find_positions.len() as i32);
        let bp = buf.backpressure;
        // The buffer tracks "a command is running" from OSC 133 on the pump
        // thread; re-push it so a full row rebuild (which starts at `false`)
        // doesn't briefly un-suppress history suggestions mid-command. Named
        // to dodge the `cr` cursor-row binding below.
        let cmd_running = buf.command_running;
        (b, matches, links, sel, has_sel, fa, ft, bp, cmd_running)
    });
    let Some((b, matches, links, sel, has_sel, fa, ft, bp, cmd_running)) = data else {
        return;
    };
    let spans = std::cell::Cell::new(Some(b.spans));
    let fm = ModelRc::from(Rc::new(VecModel::from(matches)));
    let lm = ModelRc::from(Rc::new(VecModel::from(links)));
    let sm = ModelRc::from(Rc::new(VecModel::from(sel)));
    let (cr, cc, ru, alt) = (b.cursor_row, b.cursor_col, b.rows_used, b.is_alt);
    let caret_visible = b.cursor_visible;
    let (smax, soff) = (b.scroll_max, b.scroll_offset);
    set_terminal_row(win, tab_id, move |row| {
        if let Some(spans) = spans.take() {
            // Update the tab's existing model in place. Assigning a new
            // ModelRc makes Slint's repeater drop and re-create every span
            // element (and re-shape all their text) on every frame, which is
            // what made fast log output stutter.
            match row.spans.as_any().downcast_ref::<VecModel<TermSpan>>() {
                Some(model) => sync_span_model(model, spans),
                None => row.spans = ModelRc::from(Rc::new(VecModel::from(spans))),
            }
        }
        row.cursor_row = cr;
        row.cursor_col = cc;
        row.cursor_visible = caret_visible;
        row.rows_used = ru;
        row.is_alt_screen = alt;
        row.mouse_tracked = b.mouse_tracked;
        if !model_matches(&row.find_matches, &fm) {
            row.find_matches = fm.clone();
        }
        if !model_matches(&row.selection, &sm) {
            row.selection = sm.clone();
        }
        row.link_matches = lm.clone();
        row.has_selection = has_sel;
        row.scroll_max = smax;
        row.scroll_offset = soff;
        row.find_active = fa;
        row.find_total = ft;
        row.backpressure = bp;
        row.command_running = cmd_running;
    });
    win.window().request_redraw();
}

/// True when two small overlay models (find matches, selection rects) hold the
/// same rows, so the existing one can stay and its repeater keeps its items.
fn model_matches(old: &ModelRc<TermMatch>, new: &ModelRc<TermMatch>) -> bool {
    use slint::Model as _;
    old.row_count() == new.row_count() && old.iter().zip(new.iter()).all(|(a, b)| a == b)
}

/// Identity of a span ignoring its row, so a line that only moved up the
/// screen (scrolling output) can keep its Slint element.
#[derive(PartialEq, Eq, Hash)]
struct SpanKey {
    text: SharedString,
    font: SharedString,
    col: i32,
    cells: i32,
    fg: u32,
    bg: u32,
    flags: u8,
}

fn span_key(span: &TermSpan) -> SpanKey {
    SpanKey {
        text: span.text.clone(),
        font: span.font.clone(),
        col: span.col,
        cells: span.cells,
        fg: span.fg.as_argb_encoded(),
        bg: span.bg.as_argb_encoded(),
        flags: u8::from(span.bold)
            | u8::from(span.underline) << 1
            | u8::from(span.cjk) << 2
            | u8::from(span.emoji) << 3,
    }
}

/// Make `model` hold `spans`, touching as few rows as possible.
///
/// Each model row is one Slint element whose Text must be re-shaped when its
/// text changes. When output scrolls, nearly every line moves up one or more
/// rows without changing, so each new span first claims an old slot with the
/// same content: only its `row` (a position) changes. Spans that are really
/// new reuse the slots of lines that scrolled off. The model only grows or
/// shrinks at the end, so no other element is shifted.
pub(super) fn sync_span_model(model: &VecModel<TermSpan>, spans: Vec<TermSpan>) {
    use slint::Model as _;

    let old: Vec<TermSpan> = model.iter().collect();
    if old == spans {
        return;
    }

    let slots = plan_span_slots(&old, spans);
    for (index, span) in slots.iter().enumerate() {
        if index >= old.len() {
            model.push(span.clone());
        } else if old[index] != *span {
            model.set_row_data(index, span.clone());
        }
    }
    // Shrink from the end only, so no surviving element is shifted.
    while model.row_count() > slots.len() {
        model.remove(model.row_count() - 1);
    }
}

/// Decide which model row each new span goes to (see [`sync_span_model`]).
/// Returns the complete new row list, without holes.
fn plan_span_slots(old: &[TermSpan], spans: Vec<TermSpan>) -> Vec<TermSpan> {
    let mut by_key: std::collections::HashMap<SpanKey, Vec<usize>> =
        std::collections::HashMap::with_capacity(old.len());
    for (index, span) in old.iter().enumerate() {
        by_key.entry(span_key(span)).or_default().push(index);
    }

    let mut slots: Vec<Option<TermSpan>> = vec![None; old.len()];
    let mut unmatched = Vec::new();
    for span in spans {
        let reused = by_key.get_mut(&span_key(&span)).and_then(|candidates| {
            // Emoji spans carry their glyph in the image, not the text.
            let position = candidates
                .iter()
                .position(|&index| !span.emoji || old[index].emoji_image == span.emoji_image)?;
            Some(candidates.swap_remove(position))
        });
        match reused {
            Some(index) => slots[index] = Some(span),
            None => unmatched.push(span),
        }
    }

    // Slots of lines that scrolled away take the genuinely new spans; any
    // left over are appended.
    let mut unmatched = unmatched.into_iter();
    for slot in slots.iter_mut().filter(|slot| slot.is_none()) {
        match unmatched.next() {
            Some(span) => *slot = Some(span),
            None => break,
        }
    }
    slots.extend(unmatched.map(Some));

    // Fewer spans than before: move spans from the end into interior holes
    // so the list only shrinks at its tail.
    let mut front = 0;
    loop {
        while slots.last().is_some_and(Option::is_none) {
            slots.pop();
        }
        while front < slots.len() && slots[front].is_some() {
            front += 1;
        }
        if front >= slots.len() {
            break;
        }
        slots[front] = slots.pop().flatten();
    }
    slots
        .into_iter()
        .map(|slot| slot.expect("holes were filled"))
        .collect()
}

/// Refresh only the lightweight selection overlay. Dragging used to call
/// `rebuild_tab_display` for every mouse-move event, reparsing and rebuilding
/// all terminal spans even though the underlying screen had not changed.
pub(super) fn refresh_terminal_selection(win: &AppWindow, bufs: &TermBuffers, tab_id: &str) {
    let selection = with_term_buf(bufs, tab_id, |buf| {
        let cols = buf.parser.screen().size().1;
        (buf.selection_rects_visible(cols), buf.selection_has_extent())
    });
    let Some((selection, has_sel)) = selection else {
        return;
    };
    let model = ModelRc::from(Rc::new(VecModel::from(selection)));
    set_terminal_row(win, tab_id, move |row| {
        row.selection = model.clone();
        row.has_selection = has_sel;
    });
    win.window().request_redraw();
}

/// Resolve the user's saved theme preference to a dark/light bool (mirrors the
/// startup logic): "light"/"dark" win; otherwise ask the OS, defaulting to dark.
pub(super) fn theme_pref_is_dark(store: &ConfigStore) -> bool {
    match store.theme_pref() {
        "light" => false,
        "dark" => true,
        _ => match dark_light::detect() {
            dark_light::Mode::Light => false,
            dark_light::Mode::Dark => true,
            dark_light::Mode::Default => true, // undetectable → dark
        },
    }
}

/// Flip the whole app between light and dark. Setting `Theme.dark` alone only
/// recolours the Slint chrome — each terminal bakes its ANSI/default colours
/// from a per-buffer `is_dark` flag at render time, so we must also update every
/// buffer and re-render it. Both the theme toggle and wallpaper switching route
/// through here (the proc-window mirror stays with the toggle).
pub(super) fn apply_dark_mode(window: &AppWindow, bufs: &TermBuffers, dark: bool) {
    window.set_dark_mode(dark);
    {
        let handles: Vec<_> = lock_or_recover(&bufs).values().cloned().collect();
        for h in handles {
            lock_or_recover(&h).is_dark = dark;
        }
    }
    let tab_ids: Vec<String> = lock_or_recover(&bufs).keys().cloned().collect();
    for tid in tab_ids {
        rebuild_tab_display(window, bufs, &tid);
    }
}

pub(super) fn apply_output_highlight(
    window: &AppWindow,
    bufs: &TermBuffers,
    enabled: bool,
    preset: &str,
) {
    let mode = OutputHighlightPreset::from_settings(enabled, preset);
    {
        let handles: Vec<_> = lock_or_recover(&bufs).values().cloned().collect();
        for handle in handles {
            lock_or_recover(&handle).output_highlight = mode;
        }
    }
    let tab_ids: Vec<String> = lock_or_recover(&bufs).keys().cloned().collect();
    for tab_id in tab_ids {
        rebuild_tab_display(window, bufs, &tab_id);
    }
}

pub(super) fn apply_custom_output_rules(
    window: &AppWindow,
    bufs: &TermBuffers,
    rules: &[OutputHighlightRule],
) {
    let compiled = compile_output_rules(rules);
    {
        let handles: Vec<_> = lock_or_recover(&bufs).values().cloned().collect();
        for handle in handles {
            lock_or_recover(&handle).custom_highlight_rules = compiled.clone();
        }
    }
    let tab_ids: Vec<String> = lock_or_recover(&bufs).keys().cloned().collect();
    for tab_id in tab_ids {
        rebuild_tab_display(window, bufs, &tab_id);
    }
}

/// Push the current action-link flags to every terminal buffer and refresh the
/// window-level master switch, then re-render so underlines appear/disappear
/// immediately. `flags` is all-false when the feature is off or the platform is
/// excluded, which also clears existing hits on the next render.
pub(super) fn apply_action_links(
    window: &AppWindow,
    bufs: &TermBuffers,
    flags: crate::terminal::ActionLinkFlags,
    enabled: bool,
) {
    {
        let handles: Vec<_> = lock_or_recover(&bufs).values().cloned().collect();
        for handle in handles {
            lock_or_recover(&handle).action_link_flags = flags;
        }
    }
    window.set_action_links_enabled(enabled);
    let tab_ids: Vec<String> = lock_or_recover(&bufs).keys().cloned().collect();
    for tab_id in tab_ids {
        rebuild_tab_display(window, bufs, &tab_id);
    }
}

/// Apply a wallpaper id to the window: load the image + derived palette, push the
/// immersive Theme overrides (accent / tint / image) and set `dark` from the
/// image luminance. An empty or undecodable id turns immersive mode off and
/// restores the user's saved light/dark theme.
pub(super) fn apply_wallpaper(
    window: &AppWindow,
    store: &ConfigStore,
    bufs: &TermBuffers,
    id: &str,
    apply_builtin_theme: bool,
) {
    match crate::wallpaper::load(id) {
        Some(wp) => {
            let (ar, ag, ab) = wp.palette.accent;
            let (tr, tg, tb) = wp.palette.tint;
            window.set_wallpaper_img(wp.image);
            window.set_wp_accent(slint::Color::from_rgb_u8(ar, ag, ab));
            window.set_wp_tint(slint::Color::from_rgb_u8(tr, tg, tb));
            // Only the built-ins (designed as a light/dark pair) auto-set the
            // theme. A custom photo keeps the user's light/dark choice so the
            // theme toggle still governs text contrast — a light/white wallpaper
            // reads best in light mode (crisp dark text) rather than being forced
            // dark and greying the text out (#wallpaper).
            // With immersive colours off the wallpaper is only a backdrop and
            // leaves light/dark alone.
            if apply_builtin_theme && crate::wallpaper::is_builtin(id) && store.immersive_colors()
            {
                apply_dark_mode(window, bufs, wp.palette.is_dark);
            }
            window.set_wallpaper_active(true);
            window.set_current_wallpaper(id.into());
            let name = if crate::wallpaper::is_builtin(id) {
                String::new()
            } else {
                std::path::Path::new(id)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            window.set_custom_wallpaper_name(name.into());
        }
        None => {
            window.set_wallpaper_active(false);
            window.set_current_wallpaper("".into());
            window.set_custom_wallpaper_name("".into());
            apply_dark_mode(window, bufs, theme_pref_is_dark(store));
        }
    }
}

/// Resolve which interface drives the top sparkline: the user's selection if it
/// still exists, otherwise the busiest (the list is sorted busiest-first).
/// Returns (name, rx_bps, tx_bps).
pub(super) fn selected_iface(st: &TabStatus) -> (String, u64, u64) {
    if !st.selected_iface.is_empty() {
        if let Some(e) = st.net.iter().find(|e| e.0 == st.selected_iface) {
            return e.clone();
        }
    }
    st.net.first().cloned().unwrap_or_default()
}

/// Recompute the whole sidebar (status dot + CPU/mem/swap + dual network panel)
/// for whichever tab is active.  Welcome tab → local machine; a session tab →
/// that server.  The bottom network graph is always the local machine.
/// Must run on the Slint event loop thread.
/// The copyable IP/host from a `user@host` connection label (#192): the part
/// after the last `@`, trimmed. Falls back to the whole string when there's no
/// `@` (already a bare host/IP).
pub(super) fn conn_ip(host: &str) -> String {
    host.rsplit('@').next().unwrap_or(host).trim().to_string()
}

#[cfg(test)]
mod span_model_tests {
    use super::{plan_span_slots, sync_span_model};
    use crate::ui::TermSpan;
    use slint::{Model as _, VecModel};

    fn span(text: &str, row: i32) -> TermSpan {
        TermSpan {
            text: text.into(),
            row,
            col: 0,
            cells: text.chars().count() as i32,
            ..Default::default()
        }
    }

    fn texts(spans: &[TermSpan]) -> Vec<(String, i32)> {
        spans.iter().map(|s| (s.text.to_string(), s.row)).collect()
    }

    #[test]
    fn scrolled_lines_keep_their_slot_and_new_lines_fill_freed_ones() {
        let old = vec![span("a", 0), span("b", 1), span("c", 2)];
        // One line scrolled off the top, "d" arrived at the bottom.
        let new = vec![span("b", 0), span("c", 1), span("d", 2)];
        let slots = plan_span_slots(&old, new);
        // "b" and "c" stay in slots 1 and 2 (only their row changes); "d"
        // reuses slot 0, the line that scrolled away.
        assert_eq!(
            texts(&slots),
            vec![("d".into(), 2), ("b".into(), 0), ("c".into(), 1)]
        );
    }

    #[test]
    fn shrinking_moves_tail_spans_into_holes() {
        let old = vec![span("a", 0), span("b", 1), span("c", 2), span("d", 3)];
        let new = vec![span("d", 0)];
        let slots = plan_span_slots(&old, new);
        assert_eq!(texts(&slots), vec![("d".into(), 0)]);
    }

    #[test]
    fn growing_appends_after_existing_slots() {
        let old = vec![span("a", 0)];
        let new = vec![span("a", 0), span("b", 1), span("c", 2)];
        let slots = plan_span_slots(&old, new);
        assert_eq!(
            texts(&slots),
            vec![("a".into(), 0), ("b".into(), 1), ("c".into(), 2)]
        );
    }

    #[test]
    fn model_ends_up_with_exactly_the_new_spans() {
        let model = VecModel::from(vec![span("a", 0), span("b", 1), span("c", 2), span("x", 3)]);
        let new = vec![span("b", 0), span("c", 1), span("d", 2)];
        sync_span_model(&model, new.clone());
        let mut got: Vec<_> = texts(&model.iter().collect::<Vec<_>>());
        let mut want = texts(&new);
        got.sort();
        want.sort();
        assert_eq!(got, want);
        assert_eq!(model.row_count(), 3);
    }

    #[test]
    fn duplicate_lines_are_matched_one_to_one() {
        let old = vec![span("same", 0), span("same", 1)];
        let new = vec![span("same", 0), span("same", 1), span("same", 2)];
        let slots = plan_span_slots(&old, new);
        assert_eq!(slots.len(), 3);
        let mut rows: Vec<i32> = slots.iter().map(|s| s.row).collect();
        rows.sort();
        assert_eq!(rows, vec![0, 1, 2]);
    }
}
