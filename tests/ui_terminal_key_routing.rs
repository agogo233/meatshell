#[test]
fn scrollback_navigation_is_consumed_only_when_rust_handles_it() {
    let source = include_str!("../ui/terminal_view.slint");
    let branch_start = source
        .find("Local history navigation")
        .expect("terminal history navigation branch");
    let branch_end = source[branch_start..]
        .find("// ── All other keys")
        .expect("PTY fallback branch")
        + branch_start;
    let branch = &source[branch_start..branch_end];

    assert!(branch.contains("event.text == Key.Home"));
    assert!(branch.contains("event.text == Key.End"));
    assert!(branch.contains("event.text == Key.PageUp"));
    assert!(branch.contains("event.text == Key.PageDown"));
    assert!(branch.contains("&& root.terminal-scrollback-key(event.text)"));
    assert!(source[branch_end..].contains("root.send-key(event.text"));

    let app_source = include_str!("../ui/app.slint");
    assert!(app_source.contains(
        "callback terminal-scrollback-key(string /* tab-id */, string /* key */) -> bool;"
    ));
    assert!(app_source.contains(
        "terminal-scrollback-key(key) => { root.terminal-scrollback-key(term.id, key) }"
    ));
}

#[test]
fn scrollback_transition_reclaims_the_hidden_ime_anchor_on_the_next_ui_turn() {
    let source = include_str!("../ui/terminal_view.slint");
    let transition_start = source
        .find("changed cursor-row => {")
        .expect("scrollback cursor transition handler");
    let transition = &source[transition_start..];

    assert!(transition.contains("if (root.cursor-row < 0) {"));
    assert!(transition.contains("root.focus-pending = true;"));
    assert!(source.contains("focus-defer-timer := Timer"));
    assert!(source.contains("ime-input.focus();"));
}

#[test]
fn ctrl_v_pastes_locally_except_on_the_alternate_screen() {
    let source = include_str!("../ui/terminal_view.slint");
    let paste_start = source.find("// ── Paste:").expect("paste key routing");
    let paste_end = source[paste_start..]
        .find("// ── Paste: Shift+Insert")
        .expect("Shift+Insert routing")
        + paste_start;
    let paste = &source[paste_start..paste_end];

    assert!(paste.contains("event.modifiers.control && event.modifiers.shift"));
    assert!(paste.contains("!root.is-alt-screen"));
    assert!(paste.contains("!event.modifiers.shift"));
    assert!(source[paste_end..].contains("root.send-key(event.text"));
}

#[test]
fn scrollbar_pointer_down_defers_terminal_focus_recovery() {
    let source = include_str!("../ui/terminal_view.slint");
    let scrollbar_start = source
        .find("// Terminal scrollbar")
        .expect("terminal scrollbar declaration");
    let scrollbar_end = source[scrollbar_start..]
        .find("if root.find-active")
        .expect("following terminal overlay")
        + scrollbar_start;
    let scrollbar = &source[scrollbar_start..scrollbar_end];

    assert!(scrollbar.contains("e.kind == PointerEventKind.down"));
    assert!(scrollbar.contains("root.focus-pending = true;"));
    assert!(scrollbar.contains("root.terminal-scroll-to("));
}

#[test]
fn scrollback_ime_anchor_stays_in_the_visible_tree_without_a_mouse_hitbox() {
    let source = include_str!("../ui/terminal_view.slint");
    let input_start = source
        .find("ime-input := TextInput")
        .expect("hidden terminal IME input");
    let input_end = source[input_start..]
        .find("changed has-focus")
        .expect("IME focus handler")
        + input_start;
    let input = &source[input_start..input_end];

    assert!(!input.contains(": -1000px"));
    assert!(input.contains("x: root.cursor-row >= 0"));
    assert!(input.contains("y: root.cursor-row >= 0"));
    assert!(input.contains("width: 0px;"));
    assert!(input.contains("height: 0px;"));
}
