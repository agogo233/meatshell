use super::*;

fn pc() -> Keymap {
    Keymap::resolve(&BTreeMap::new(), false)
}

fn mac() -> Keymap {
    Keymap::resolve(&BTreeMap::new(), true)
}

fn ev(is_mac: bool, text: &str, control: bool, meta: bool, alt: bool, shift: bool) -> Chord {
    Chord::from_event(is_mac, text, control, meta, alt, shift).expect("bindable key")
}

#[test]
fn every_default_parses_and_defaults_are_conflict_free() {
    for is_mac in [false, true] {
        let km = Keymap::resolve(&BTreeMap::new(), is_mac);
        for (i, a) in km.chords.iter().enumerate() {
            for b in &km.chords[i + 1..] {
                assert_ne!(a, b, "duplicate default chord (mac={is_mac})");
            }
        }
    }
}

#[test]
fn storage_round_trips() {
    for s in ["Ctrl+Shift+C", "Alt+Enter", "Ctrl+Alt+=", "Ctrl+-", "Shift+Insert", "F5", "Alt+Cmd+Z"] {
        let c = Chord::parse(s).unwrap();
        assert_eq!(Chord::parse(&c.to_storage()).unwrap(), c, "{s}");
    }
    assert_eq!(Chord::parse("ctrl+shift+c"), Chord::parse("Ctrl+Shift+C"));
    assert_eq!(Chord::parse("Ctrl++"), Chord::parse("Ctrl+="));
    assert_eq!(Chord::parse("Win+K"), Chord::parse("Cmd+K"));
    assert!(Chord::parse("Hyper+K").is_none());
    assert!(Chord::parse("Ctrl+NotAKey").is_none());
}

#[test]
fn default_terminal_behaviour_is_preserved_on_windows() {
    let km = pc();
    // Ctrl+C interrupts (sends ^C, also inside vim); Ctrl+Shift+C copies.
    let ctrl_c = ev(false, "c", true, false, false, false);
    assert_eq!(km.match_event(&ctrl_c, false, true), Some("interrupt"));
    assert_eq!(km.match_event(&ctrl_c, true, true), Some("interrupt"));
    assert_eq!(km.match_event(&ev(false, "C", true, false, false, true), false, true), Some("copy"));
    // Ctrl+V pastes on the normal screen but reaches vim on the alt screen.
    let ctrl_v = ev(false, "v", true, false, false, false);
    assert_eq!(km.match_event(&ctrl_v, false, true), Some("paste"));
    assert_eq!(km.match_event(&ctrl_v, true, true), None);
    // Fixed paste fallbacks.
    assert_eq!(km.match_event(&ev(false, "V", true, false, false, true), true, true), Some("paste"));
    let shift_ins = ev(false, "\u{F727}", false, false, false, true);
    assert_eq!(km.match_event(&shift_ins, false, true), Some("paste"));
    assert_eq!(km.match_event(&shift_ins, false, false), None);
    // Tabs, including Backtab delivery for Ctrl+Shift+Tab.
    assert_eq!(km.match_event(&ev(false, "\t", true, false, false, false), false, true), Some("next-tab"));
    assert_eq!(km.match_event(&ev(false, "\u{19}", true, false, false, true), false, true), Some("prev-tab"));
    assert_eq!(km.match_event(&ev(false, "\u{19}", true, false, false, false), false, true), Some("prev-tab"));
    // Zoom: "+" with or without Shift, and ^_ for Ctrl+-.
    assert_eq!(km.match_event(&ev(false, "+", true, false, false, true), false, true), Some("zoom-in"));
    assert_eq!(km.match_event(&ev(false, "=", true, false, false, false), false, true), Some("zoom-in"));
    assert_eq!(km.match_event(&ev(false, "\u{1f}", true, false, false, false), false, true), Some("zoom-out"));
    assert_eq!(km.match_event(&ev(false, "0", true, false, true, false), false, true), Some("zoom-reset-all"));
    assert_eq!(km.match_event(&ev(false, "\n", false, false, true, false), false, true), Some("fullscreen"));
    // Control-char delivery of Ctrl+F.
    assert_eq!(km.match_event(&ev(false, "\u{6}", true, false, false, false), false, true), Some("find"));
}

#[test]
fn mac_uses_cmd_for_app_shortcuts_and_physical_ctrl_for_tabs() {
    let km = mac();
    // Slint: control = ⌘, meta = physical Ctrl.
    assert_eq!(km.match_event(&ev(true, "c", true, false, false, false), false, true), Some("copy"));
    // Physical Ctrl+C interrupts; ⌘C copies.
    assert_eq!(km.match_event(&ev(true, "c", false, true, false, false), false, true), Some("interrupt"));
    assert_eq!(km.match_event(&ev(true, "\t", false, true, false, false), false, true), Some("next-tab"));
    assert_eq!(km.match_event(&ev(true, "v", true, false, false, false), true, true), Some("paste"));
    assert_eq!(Chord::parse("Alt+Shift+Cmd+K").unwrap().display(true), "⌥⇧⌘K");
}

#[test]
fn bare_modifiers_are_not_chords() {
    for code in ['\u{10}', '\u{11}', '\u{12}', '\u{16}', '\u{17}'] {
        assert!(Chord::from_event(false, &code.to_string(), true, false, false, false).is_none());
    }
    assert!(Chord::from_event(false, "", true, false, false, false).is_none());
    assert!(Chord::from_event(false, "你好", false, false, false, false).is_none());
}

#[test]
fn overrides_rebind_and_free_the_old_chord() {
    let mut o = BTreeMap::new();
    o.insert("copy".to_string(), "Ctrl+C".to_string());
    o.insert("find".to_string(), "garbage".to_string()); // ignored → default
    let km = Keymap::resolve(&o, false);
    // The user's custom Ctrl+C wins; Interrupt's default is left unbound.
    assert_eq!(km.match_event(&ev(false, "c", true, false, false, false), false, true), Some("copy"));
    assert_eq!(km.match_event(&ev(false, "C", true, false, false, true), false, true), None);
    assert_eq!(km.chord("find").unwrap().to_storage(), "Ctrl+F");
    assert!(km.chord("interrupt").is_none());
    let rows = binding_rows(&km, true);
    let interrupt = rows.iter().find(|r| r.id == "interrupt").unwrap();
    assert_eq!(interrupt.keys.as_str(), "—");

    // Interrupt moved elsewhere: that chord sends ^C, and plain Ctrl+C is
    // no longer a shortcut (it still reaches the shell as a raw key).
    let mut o = BTreeMap::new();
    o.insert("interrupt".to_string(), "Ctrl+Shift+X".to_string());
    let km = Keymap::resolve(&o, false);
    assert_eq!(km.match_event(&ev(false, "X", true, false, false, true), false, true), Some("interrupt"));
    assert_eq!(km.match_event(&ev(false, "c", true, false, false, false), false, true), None);
}

#[test]
fn record_validates_conflicts_modifiers_and_warns_about_ctrl_letters() {
    let km = pc();
    // Conflict.
    let ctrl_f = Chord::parse("Ctrl+F").unwrap();
    assert!(matches!(record(&km, "copy", &ctrl_f, true), RecordOutcome::Rejected(m) if m.contains("Find in terminal")));
    // No modifier.
    let plain = Chord::parse("K").unwrap();
    assert!(matches!(record(&km, "copy", &plain, true), RecordOutcome::Rejected(_)));
    // F-keys alone are fine.
    assert!(matches!(record(&km, "find", &Chord::parse("F3").unwrap(), true), RecordOutcome::Bound { warning: None, .. }));
    // Ctrl+C is Interrupt's by default, so Copy can't take it directly...
    let ctrl_c = Chord::parse("Ctrl+C").unwrap();
    assert!(matches!(record(&km, "copy", &ctrl_c, true), RecordOutcome::Rejected(m) if m.contains("Interrupt")));
    // ...and Interrupt keeping Ctrl+C is not warned about.
    assert_eq!(record(&km, "interrupt", &ctrl_c, true), RecordOutcome::Bound { override_value: None, warning: None });
    // Once Interrupt moves away, Copy may take Ctrl+C, with a warning that
    // ^C won't reach the shell.
    let mut o = BTreeMap::new();
    o.insert("interrupt".to_string(), "Ctrl+Shift+X".to_string());
    let km = Keymap::resolve(&o, false);
    match record(&km, "copy", &ctrl_c, true) {
        RecordOutcome::Bound { override_value, warning } => {
            assert_eq!(override_value.as_deref(), Some("Ctrl+C"));
            assert!(warning.unwrap().contains("^C"));
        }
        other => panic!("{other:?}"),
    }
    // Re-recording the default drops the override.
    let default = Chord::parse("Ctrl+Shift+C").unwrap();
    assert_eq!(record(&km, "copy", &default, true), RecordOutcome::Bound { override_value: None, warning: None });
}

#[test]
fn dialog_rows_follow_sections_and_flag_customised_entries() {
    let mut o = BTreeMap::new();
    o.insert("zen".to_string(), "F11".to_string());
    let rows = binding_rows(&Keymap::resolve(&o, false), true);
    assert_eq!(rows.len(), ACTIONS.len());
    assert_eq!(rows[0].header.as_str(), "Tabs");
    assert_eq!(rows[1].header.as_str(), "");
    let zen = rows.iter().find(|r| r.id == "zen").unwrap();
    assert_eq!(zen.keys.as_str(), "F11");
    assert!(!zen.is_default);
    assert!(rows.iter().filter(|r| r.id != "zen").all(|r| r.is_default));
}
