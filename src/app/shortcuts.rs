//! Customisable keyboard shortcuts.
//!
//! Every app-level shortcut (tabs, window, terminal) is an *action* with a
//! platform default chord. Users can rebind any action from the Shortcuts
//! dialog; only overrides are persisted (`ConfigFile::keybindings`), so
//! "Reset all" simply clears that map.
//!
//! The Slint key handlers no longer hard-code chords: they call
//! `match-shortcut(text, control, meta, alt, shift, alt-screen)` and dispatch
//! on the returned action id ("" = not a shortcut → the key goes to the PTY).
//!
//! Modifier naming: Slint reports Cmd (⌘) as `control` and the physical Ctrl
//! key as `meta` on macOS (#158). `Chord` stores *physical* meaning instead:
//! `ctrl` is the Ctrl key everywhere and `cmd` is ⌘ on macOS / the Windows
//! key elsewhere.

use super::*;

use std::collections::BTreeMap;

/// Which dialog section an action is listed under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Section {
    Tabs,
    Window,
    Terminal,
}

impl Section {
    fn title(self, en: bool) -> &'static str {
        match (self, en) {
            (Section::Tabs, true) => "Tabs",
            (Section::Tabs, false) => "标签页",
            (Section::Window, true) => "Window",
            (Section::Window, false) => "窗口",
            (Section::Terminal, true) => "Terminal",
            (Section::Terminal, false) => "终端",
        }
    }
}

pub(crate) struct ActionDef {
    pub id: &'static str,
    pub section: Section,
    pub en: &'static str,
    pub zh: &'static str,
    /// Default chord on Windows / Linux.
    pub default_pc: &'static str,
    /// Default chord on macOS.
    pub default_mac: &'static str,
}

impl ActionDef {
    pub(crate) fn default_chord(&self, is_mac: bool) -> Chord {
        let s = if is_mac { self.default_mac } else { self.default_pc };
        // Built-in defaults are known-good; a parse failure is a typo here.
        Chord::parse(s).unwrap_or_else(|| panic!("bad default chord {s:?} for {}", self.id))
    }

    fn desc(&self, en: bool) -> &'static str {
        if en {
            self.en
        } else {
            self.zh
        }
    }
}

/// Every rebindable action, in dialog order.
pub(crate) const ACTIONS: &[ActionDef] = &[
    ActionDef { id: "next-tab", section: Section::Tabs, en: "Next tab", zh: "下一个标签页",
        default_pc: "Ctrl+Tab", default_mac: "Ctrl+Tab" },
    ActionDef { id: "prev-tab", section: Section::Tabs, en: "Previous tab", zh: "上一个标签页",
        default_pc: "Ctrl+Shift+Tab", default_mac: "Ctrl+Shift+Tab" },
    ActionDef { id: "new-window", section: Section::Window, en: "New window", zh: "新建窗口",
        default_pc: "Ctrl+Shift+N", default_mac: "Shift+Cmd+N" },
    ActionDef { id: "zen", section: Section::Window, en: "Toggle zen (focus) mode", zh: "切换专注模式",
        default_pc: "Ctrl+Alt+Z", default_mac: "Alt+Cmd+Z" },
    ActionDef { id: "fullscreen", section: Section::Window, en: "Toggle terminal fullscreen", zh: "切换终端全屏",
        default_pc: "Alt+Enter", default_mac: "Alt+Enter" },
    // Sends ^C exactly like the physical key. Listing it makes Ctrl+C visible
    // and rebindable; an unbound Ctrl+C still reaches the shell as usual.
    ActionDef { id: "interrupt", section: Section::Terminal, en: "Interrupt running command (send ^C)", zh: "中断当前命令（发送 ^C）",
        default_pc: "Ctrl+C", default_mac: "Ctrl+C" },
    ActionDef { id: "copy", section: Section::Terminal, en: "Copy selection", zh: "复制选中内容",
        default_pc: "Ctrl+Shift+C", default_mac: "Cmd+C" },
    ActionDef { id: "paste", section: Section::Terminal, en: "Paste", zh: "粘贴",
        default_pc: "Ctrl+V", default_mac: "Cmd+V" },
    ActionDef { id: "find", section: Section::Terminal, en: "Find in terminal", zh: "查找",
        default_pc: "Ctrl+F", default_mac: "Cmd+F" },
    ActionDef { id: "history-search", section: Section::Terminal, en: "Jump to command history search", zh: "跳到命令历史搜索",
        default_pc: "Ctrl+Shift+R", default_mac: "Shift+Cmd+R" },
    ActionDef { id: "zoom-in", section: Section::Terminal, en: "Enlarge terminal font (this session)", zh: "放大终端字体（当前会话）",
        default_pc: "Ctrl+=", default_mac: "Cmd+=" },
    ActionDef { id: "zoom-out", section: Section::Terminal, en: "Shrink terminal font (this session)", zh: "缩小终端字体（当前会话）",
        default_pc: "Ctrl+-", default_mac: "Cmd+-" },
    ActionDef { id: "zoom-reset", section: Section::Terminal, en: "Reset terminal font size (this session)", zh: "重置终端字体大小（当前会话）",
        default_pc: "Ctrl+0", default_mac: "Cmd+0" },
    ActionDef { id: "zoom-in-all", section: Section::Terminal, en: "Enlarge terminal font (all sessions in window)", zh: "放大终端字体（本窗口所有会话）",
        default_pc: "Ctrl+Alt+=", default_mac: "Alt+Cmd+=" },
    ActionDef { id: "zoom-out-all", section: Section::Terminal, en: "Shrink terminal font (all sessions in window)", zh: "缩小终端字体（本窗口所有会话）",
        default_pc: "Ctrl+Alt+-", default_mac: "Alt+Cmd+-" },
    ActionDef { id: "zoom-reset-all", section: Section::Terminal, en: "Reset terminal font size (all sessions in window)", zh: "重置终端字体大小（本窗口所有会话）",
        default_pc: "Ctrl+Alt+0", default_mac: "Alt+Cmd+0" },
];

pub(crate) fn action(id: &str) -> Option<&'static ActionDef> {
    ACTIONS.iter().find(|a| a.id == id)
}

const NAMED_KEYS: &[&str] = &[
    "Tab", "Enter", "Esc", "Backspace", "Delete", "Space", "Up", "Down", "Left", "Right",
    "Insert", "Home", "End", "PageUp", "PageDown", "Menu", "F1", "F2", "F3", "F4", "F5",
    "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14", "F15", "F16", "F17", "F18",
    "F19", "F20", "F21", "F22", "F23", "F24",
];

/// A key combination in physical-modifier terms (see module docs).
/// `key` is normalised: a lowercase letter, a digit / punctuation character,
/// or one of `NAMED_KEYS`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub cmd: bool,
    pub key: String,
}

/// Normalise one Slint key-event text into a key name. Returns the key and
/// whether the text itself implies Shift (Backtab). `None` for bare modifier
/// presses, IME strings and anything we can't name.
fn key_from_text(text: &str) -> Option<(String, bool)> {
    let mut chars = text.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let named = |s: &str| Some((s.to_string(), false));
    match c {
        // Bare modifier / lock keys (Shift … MetaR in Slint's key codes).
        '\u{10}'..='\u{18}' => None,
        '\u{8}' => named("Backspace"),
        '\t' => named("Tab"),
        '\u{19}' => Some(("Tab".to_string(), true)),
        '\n' | '\r' => named("Enter"),
        '\u{1b}' => named("Esc"),
        '\u{7f}' => named("Delete"),
        ' ' => named("Space"),
        // Ctrl+letter delivered as a C0 control char by some backends
        // (^A..^O and ^Z; ^P..^X collide with Slint's modifier key codes).
        '\u{1}'..='\u{f}' | '\u{1a}' => {
            Some((char::from(b'a' + (c as u8) - 1).to_string(), false))
        }
        '\u{1f}' => Some(("-".to_string(), false)), // Ctrl+- as ^_
        '\u{F700}' => named("Up"),
        '\u{F701}' => named("Down"),
        '\u{F702}' => named("Left"),
        '\u{F703}' => named("Right"),
        '\u{F704}'..='\u{F71B}' => Some((format!("F{}", c as u32 - 0xF704 + 1), false)),
        '\u{F727}' => named("Insert"),
        '\u{F729}' => named("Home"),
        '\u{F72B}' => named("End"),
        '\u{F72C}' => named("PageUp"),
        '\u{F72D}' => named("PageDown"),
        '\u{F735}' => named("Menu"),
        // Other private-use special keys (ScrollLock, Pause, …) aren't bindable.
        '\u{F700}'..='\u{F8FF}' => None,
        '+' => Some(("=".to_string(), false)), // Ctrl++ / Ctrl+Shift+= / numpad +
        c if c.is_ascii_alphabetic() => Some((c.to_ascii_lowercase().to_string(), false)),
        c if c.is_control() => None,
        c => Some((c.to_string(), false)),
    }
}

/// Punctuation already encodes Shift in its character ("?" vs "/"), so the
/// Shift flag is dropped for it — otherwise Ctrl+Shift+= ("+") and numpad
/// Ctrl++ would be two different chords.
fn shift_is_implied_by(key: &str) -> bool {
    let mut it = key.chars();
    matches!((it.next(), it.next()), (Some(c), None) if !c.is_ascii_alphanumeric())
}

impl Chord {
    /// Build from a Slint key event (raw `control` / `meta` as Slint reports them).
    pub(crate) fn from_event(
        is_mac: bool,
        text: &str,
        control: bool,
        meta: bool,
        alt: bool,
        shift: bool,
    ) -> Option<Chord> {
        let (key, implied_shift) = key_from_text(text)?;
        let (ctrl, cmd) = if is_mac { (meta, control) } else { (control, meta) };
        let shift = (shift || implied_shift) && !shift_is_implied_by(&key);
        Some(Chord { ctrl, alt, shift, cmd, key })
    }

    /// Parse a stored chord such as "Ctrl+Shift+C" (case-insensitive; also
    /// accepts Control / Option / Cmd / Win / Super / Meta aliases).
    pub(crate) fn parse(s: &str) -> Option<Chord> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        // "Ctrl++" — the key itself is '+'.
        let (mods, key_tok) = if let Some(rest) = s.strip_suffix("++") {
            (rest, "+")
        } else if s == "+" {
            ("", "+")
        } else {
            match s.rfind('+') {
                Some(i) => (&s[..i], &s[i + 1..]),
                None => ("", s),
            }
        };
        let mut c = Chord::default();
        for m in mods.split('+').filter(|m| !m.is_empty()) {
            match m.trim().to_ascii_lowercase().as_str() {
                "ctrl" | "control" => c.ctrl = true,
                "alt" | "option" | "opt" => c.alt = true,
                "shift" => c.shift = true,
                "cmd" | "command" | "win" | "super" | "meta" => c.cmd = true,
                _ => return None,
            }
        }
        let key_tok = key_tok.trim();
        c.key = if key_tok.chars().count() == 1 {
            key_from_text(key_tok)?.0
        } else {
            let lower = key_tok.to_ascii_lowercase();
            let alias = match lower.as_str() {
                "return" => "Enter",
                "escape" => "Esc",
                "del" => "Delete",
                "ins" => "Insert",
                "pgup" => "PageUp",
                "pgdn" | "pgdown" => "PageDown",
                _ => "",
            };
            if !alias.is_empty() {
                alias.to_string()
            } else {
                NAMED_KEYS
                    .iter()
                    .find(|k| k.eq_ignore_ascii_case(key_tok))?
                    .to_string()
            }
        };
        if shift_is_implied_by(&c.key) {
            c.shift = false;
        }
        Some(c)
    }

    fn key_label(&self) -> String {
        let mut it = self.key.chars();
        match (it.next(), it.next()) {
            (Some(ch), None) => ch.to_ascii_uppercase().to_string(),
            _ => self.key.clone(),
        }
    }

    /// Canonical storage form, e.g. "Ctrl+Alt+Shift+Cmd+K".
    pub(crate) fn to_storage(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".into());
        }
        if self.alt {
            parts.push("Alt".into());
        }
        if self.shift {
            parts.push("Shift".into());
        }
        if self.cmd {
            parts.push("Cmd".into());
        }
        parts.push(self.key_label());
        parts.join("+")
    }

    /// What the dialog shows: "Ctrl+Shift+C" on Windows/Linux, "⌃⌥⇧⌘C" on macOS.
    pub(crate) fn display(&self, is_mac: bool) -> String {
        if !is_mac {
            let mut s = self.to_storage();
            if self.cmd {
                s = s.replace("Cmd", "Win");
            }
            return s;
        }
        let mut s = String::new();
        if self.ctrl {
            s.push('⌃');
        }
        if self.alt {
            s.push('⌥');
        }
        if self.shift {
            s.push('⇧');
        }
        if self.cmd {
            s.push('⌘');
        }
        let key = match self.key.as_str() {
            "Enter" => "↩".to_string(),
            "Backspace" => "⌫".to_string(),
            "Delete" => "⌦".to_string(),
            "Up" => "↑".to_string(),
            "Down" => "↓".to_string(),
            "Left" => "←".to_string(),
            "Right" => "→".to_string(),
            _ => self.key_label(),
        };
        s.push_str(&key);
        s
    }

    fn is_letter(&self) -> bool {
        self.key.len() == 1 && self.key.as_bytes()[0].is_ascii_lowercase()
    }
}

/// The resolved binding of every action.
#[derive(Clone, Debug)]
pub(crate) struct Keymap {
    pub is_mac: bool,
    /// Parallel to `ACTIONS`. `None` = unbound: the action's default chord
    /// is taken by a user override of another action.
    pub chords: Vec<Option<Chord>>,
}

impl Keymap {
    /// Defaults with the user's overrides applied. An unparsable override
    /// (hand-edited config) falls back to the default. A user's explicit
    /// choice always wins over a default: a default that collides with an
    /// override is left unbound (e.g. Copy rebound to Ctrl+C before the
    /// Interrupt action existed keeps copying).
    pub(crate) fn resolve(overrides: &BTreeMap<String, String>, is_mac: bool) -> Keymap {
        let custom: Vec<Option<Chord>> = ACTIONS
            .iter()
            .map(|a| overrides.get(a.id).and_then(|s| Chord::parse(s)))
            .collect();
        let chords = ACTIONS
            .iter()
            .zip(&custom)
            .map(|(a, c)| match c {
                Some(c) => Some(c.clone()),
                None => {
                    let d = a.default_chord(is_mac);
                    (!custom.iter().flatten().any(|o| *o == d)).then_some(d)
                }
            })
            .collect();
        Keymap { is_mac, chords }
    }

    #[cfg(test)]
    pub(crate) fn chord(&self, id: &str) -> Option<&Chord> {
        ACTIONS
            .iter()
            .position(|a| a.id == id)
            .and_then(|i| self.chords[i].as_ref())
    }

    fn lookup(&self, chord: &Chord) -> Option<&'static str> {
        self.chords
            .iter()
            .position(|c| c.as_ref() == Some(chord))
            .map(|i| ACTIONS[i].id)
    }

    /// Map one key event to an action id. `alt_screen` = a full-screen TUI
    /// (vim, less, …) owns the terminal; `extra_paste` = the Interface
    /// setting for the additional paste/copy chords (#300).
    pub(crate) fn match_event(
        &self,
        chord: &Chord,
        alt_screen: bool,
        extra_paste: bool,
    ) -> Option<&'static str> {
        // A plain Ctrl+<letter> paste binding (the Windows/Linux default
        // Ctrl+V) yields to full-screen programs, where ^V is a real key.
        let guard = |id: &'static str, c: &Chord| -> Option<&'static str> {
            if id == "paste"
                && !self.is_mac
                && alt_screen
                && c.ctrl
                && !c.shift
                && !c.cmd
            {
                None
            } else {
                Some(id)
            }
        };
        if let Some(id) = self.lookup(chord) {
            return guard(id, chord);
        }
        // Extra chords (#300): copy/paste also fire with Alt added
        // (Ctrl+Alt+V, Ctrl+Alt+Shift+C — AltGr layouts report Ctrl+Alt).
        if extra_paste && chord.alt {
            let mut no_alt = chord.clone();
            no_alt.alt = false;
            if let Some(id) = self.lookup(&no_alt) {
                if id == "copy" || id == "paste" {
                    return guard(id, chord);
                }
            }
        }
        // Fixed paste fallbacks that predate customisation; any action bound
        // to these chords wins above.
        let primary = if self.is_mac {
            chord.cmd && !chord.ctrl
        } else {
            chord.ctrl && !chord.cmd
        };
        if primary && chord.shift && !chord.alt && chord.key == "v" {
            return Some("paste");
        }
        if extra_paste
            && chord.shift
            && !chord.ctrl
            && !chord.alt
            && !chord.cmd
            && chord.key == "Insert"
        {
            return Some("paste");
        }
        None
    }
}

/// Result of trying to bind a recorded chord.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RecordOutcome {
    /// Saved. `override_value` is `None` when the chord equals the default
    /// (the override is dropped). `warning` explains a side effect.
    Bound { override_value: Option<String>, warning: Option<String> },
    Rejected(String),
}

/// Validate binding `chord` to action `id` against the current keymap.
pub(crate) fn record(keymap: &Keymap, id: &str, chord: &Chord, en: bool) -> RecordOutcome {
    let Some(def) = action(id) else {
        return RecordOutcome::Rejected("unknown action".into());
    };
    let disp = chord.display(keymap.is_mac);
    let is_fkey = chord.key.len() > 1
        && chord.key.starts_with('F')
        && chord.key[1..].chars().all(|c| c.is_ascii_digit());
    let is_nav = matches!(
        chord.key.as_str(),
        "Insert" | "Delete" | "Home" | "End" | "PageUp" | "PageDown"
    );
    if !(chord.ctrl || chord.alt || chord.cmd || is_fkey || (chord.shift && is_nav)) {
        return RecordOutcome::Rejected(if en {
            format!("{disp} can't be a shortcut — it would stop that key from typing. Add Ctrl, Alt or {}.",
                if keymap.is_mac { "⌘" } else { "Win" })
        } else {
            format!("{disp} 不能作为快捷键（会导致该键无法输入），请加上 Ctrl、Alt 或 {}。",
                if keymap.is_mac { "⌘" } else { "Win" })
        });
    }
    if let Some(other) = keymap
        .chords
        .iter()
        .zip(ACTIONS)
        .find(|(c, a)| c.as_ref() == Some(chord) && a.id != id)
        .map(|(_, a)| a)
    {
        return RecordOutcome::Rejected(if en {
            format!("{disp} is already used by “{}”. Change that one first.", other.en)
        } else {
            format!("{disp} 已被“{}”占用，请先修改它。", other.zh)
        });
    }
    // Ctrl+<letter> is a terminal control key (^C interrupt, ^V literal-next…);
    // binding it means the shell never sees it.
    // (Paste on Windows/Linux is exempt: it yields to full-screen programs.
    // Interrupt is exempt: it sends ^C itself.)
    let warning = (chord.ctrl && !chord.alt && !chord.cmd && !chord.shift && chord.is_letter()
        && def.id != "interrupt"
        && !(def.id == "paste" && !keymap.is_mac))
        .then(|| {
            let caret = format!("^{}", chord.key.to_ascii_uppercase());
            if en {
                format!("{disp} will no longer reach the shell ({caret}).")
            } else {
                format!("{disp} 将不再发送到终端（{caret}）。")
            }
        });
    let override_value = if *chord == def.default_chord(keymap.is_mac) {
        None
    } else {
        Some(chord.to_storage())
    };
    RecordOutcome::Bound { override_value, warning }
}

// ---------------------------------------------------------------------------
// Slint wiring
// ---------------------------------------------------------------------------

thread_local! {
    /// Resolved keymap, rebuilt lazily after a change. UI-thread only, shared
    /// by every window (all windows read the same `ConfigStore`).
    static KEYMAP: RefCell<Option<Keymap>> = const { RefCell::new(None) };
}

fn with_keymap<R>(store: &RefCell<ConfigStore>, f: impl FnOnce(&Keymap) -> R) -> R {
    KEYMAP.with(|cell| {
        let mut slot = cell.borrow_mut();
        let km = slot.get_or_insert_with(|| {
            Keymap::resolve(store.borrow().keybindings(), cfg!(target_os = "macos"))
        });
        f(km)
    })
}

fn invalidate_keymap() {
    KEYMAP.with(|cell| *cell.borrow_mut() = None);
}

/// Rows for the Shortcuts dialog.
pub(crate) fn binding_rows(keymap: &Keymap, en: bool) -> Vec<ShortcutBinding> {
    let mut last: Option<Section> = None;
    ACTIONS
        .iter()
        .zip(&keymap.chords)
        .map(|(a, c)| {
            let header = if last != Some(a.section) {
                last = Some(a.section);
                a.section.title(en)
            } else {
                ""
            };
            ShortcutBinding {
                id: a.id.into(),
                header: header.into(),
                desc: a.desc(en).into(),
                keys: match c {
                    Some(c) => c.display(keymap.is_mac),
                    None => "—".to_string(), // default taken by a custom binding
                }
                .into(),
                is_default: c.as_ref().is_none_or(|c| *c == a.default_chord(keymap.is_mac)),
            }
        })
        .collect()
}

fn refresh_rows(window: &AppWindow, store: &RefCell<ConfigStore>) {
    let rows = with_keymap(store, |km| binding_rows(km, crate::i18n::is_en()));
    window.set_shortcut_bindings(ModelRc::from(Rc::new(VecModel::from(rows))));
}

pub(super) fn wire_shortcuts(ctx: &WinCtx) {
    let WinCtx { store, window, .. } = ctx;
    refresh_rows(window, store);

    {
        let store = store.clone();
        let weak = window.as_weak();
        window.on_match_shortcut(move |text, control, meta, alt, shift, alt_screen| {
            let Some(chord) = Chord::from_event(
                cfg!(target_os = "macos"),
                text.as_str(),
                control,
                meta,
                alt,
                shift,
            ) else {
                return SharedString::new();
            };
            let extra = weak
                .upgrade()
                .map(|w| w.get_extra_paste_shortcuts_enabled())
                .unwrap_or(true);
            with_keymap(&store, |km| km.match_event(&chord, alt_screen, extra))
                .map(SharedString::from)
                .unwrap_or_default()
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        window.on_shortcuts_refresh(move || {
            if let Some(w) = weak.upgrade() {
                refresh_rows(&w, &store);
            }
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        window.on_shortcut_record(move |id, text, control, meta, alt, shift| {
            let Some(w) = weak.upgrade() else { return };
            let en = crate::i18n::is_en();
            let Some(chord) = Chord::from_event(
                cfg!(target_os = "macos"),
                text.as_str(),
                control,
                meta,
                alt,
                shift,
            ) else {
                return; // bare modifier: keep listening
            };
            // Recording ends on any real key, accepted or not.
            w.set_shortcut_recording(SharedString::new());
            match with_keymap(&store, |km| record(km, id.as_str(), &chord, en)) {
                RecordOutcome::Rejected(msg) => {
                    w.set_shortcut_status(msg.into());
                    w.set_shortcut_status_error(true);
                }
                RecordOutcome::Bound { override_value, warning } => {
                    {
                        let mut s = store.borrow_mut();
                        s.set_keybinding(id.as_str(), override_value);
                        if let Err(e) = s.save() {
                            tracing::warn!("saving keybindings failed: {e:#}");
                        }
                    }
                    invalidate_keymap();
                    w.set_shortcut_status(warning.unwrap_or_default().into());
                    w.set_shortcut_status_error(false);
                    refresh_rows(&w, &store);
                }
            }
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        window.on_shortcut_reset(move |id| {
            let Some(w) = weak.upgrade() else { return };
            {
                let mut s = store.borrow_mut();
                s.set_keybinding(id.as_str(), None);
                if let Err(e) = s.save() {
                    tracing::warn!("saving keybindings failed: {e:#}");
                }
            }
            invalidate_keymap();
            w.set_shortcut_status(SharedString::new());
            refresh_rows(&w, &store);
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        window.on_shortcut_reset_all(move || {
            let Some(w) = weak.upgrade() else { return };
            {
                let mut s = store.borrow_mut();
                s.reset_keybindings();
                if let Err(e) = s.save() {
                    tracing::warn!("saving keybindings failed: {e:#}");
                }
            }
            invalidate_keymap();
            w.set_shortcut_status(SharedString::new());
            refresh_rows(&w, &store);
        });
    }
}

#[cfg(test)]
#[path = "../../tests/app/shortcuts/mod.rs"]
mod tests;
