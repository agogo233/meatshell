use super::term_fullscreen_allowed;

#[test]
fn allows_an_active_terminal_tab() {
    assert!(term_fullscreen_allowed("t1", ["t0", "t1"].into_iter()));
}

#[test]
fn refuses_welcome_editor_and_empty_tabs() {
    assert!(!term_fullscreen_allowed("welcome", ["t1"].into_iter()));
    assert!(!term_fullscreen_allowed("editor-3", ["t1"].into_iter()));
    assert!(!term_fullscreen_allowed("", [""].into_iter()));
    assert!(!term_fullscreen_allowed("t1", std::iter::empty()));
}
