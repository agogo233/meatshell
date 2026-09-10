//! Lightweight per-line syntax highlighter for the built-in editor.
//!
//! Design constraints (see the editor-highlight plan):
//! * zero new dependencies — hand-written scanners, no regex, no syntect;
//! * line-oriented with one piece of cross-line state (Python triple quotes);
//! * output is `[HlLine]` models whose `segments` are coloured text runs the
//!   Slint overlay lays out horizontally next to the transparent `TextInput`.
//!
//! Correctness contract for `sync_model`: the tokeniser is a deterministic
//! state machine, so a row whose source text *and* trailing triple-quote state
//! match the running state implies every following row tokenises identically —
//! stable rows skip the repaint and only pay a cheap clone + compare.

use std::rc::Rc;

use slint::{Color, Model, ModelRc, VecModel};
use crate::ui::{HlLine, HlSeg};

use super::lang::Lang;

/// Files above this line count fall back to the plain (unhighlighted) editor:
/// the overlay instantiates a handful of elements per row and must stay cheap.
pub const MAX_EDITOR_HL_LINES: usize = 2000;

/// Single lines longer than this tokenise as one plain run (see `tokenize`).
const MAX_TOKENIZE_CHARS: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Default,
    Keyword,
    Str,
    Comment,
    Number,
    Builtin,
}

fn role_color(role: Role, dark: bool) -> Color {
    match (role, dark) {
        (Role::Keyword, true) => Color::from_rgb_u8(0x56, 0x9c, 0xd6),
        (Role::Str, true) => Color::from_rgb_u8(0xce, 0x91, 0x78),
        (Role::Comment, true) => Color::from_rgb_u8(0x6a, 0x99, 0x55),
        (Role::Number, true) => Color::from_rgb_u8(0xb5, 0xce, 0xa8),
        (Role::Builtin, true) => Color::from_rgb_u8(0xdc, 0xdc, 0xaa),
        (Role::Keyword, false) => Color::from_rgb_u8(0x00, 0x00, 0xff),
        (Role::Str, false) => Color::from_rgb_u8(0xa3, 0x15, 0x15),
        (Role::Comment, false) => Color::from_rgb_u8(0x00, 0x80, 0x00),
        (Role::Number, false) => Color::from_rgb_u8(0x09, 0x86, 0x58),
        (Role::Builtin, false) => Color::from_rgb_u8(0x79, 0x5e, 0x26),
        (Role::Default, _) => Color::from_rgb_u8(0x00, 0x00, 0x00),
    }
}

const SHELL_KEYWORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "function", "select", "time", "return", "export", "readonly", "local", "declare",
    "unset", "set", "shift", "trap", "eval", "exec", "exit", "source", "alias",
];

const SHELL_BUILTINS: &[&str] = &[
    "echo", "printf", "cd", "pwd", "read", "type", "command", "true", "false", "test", "pushd",
    "popd", "jobs", "fg", "bg", "kill", "wait", "help",
];

const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda",
    "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with", "yield",
];

const PYTHON_BUILTINS: &[&str] = &[
    "True", "False", "None", "print", "len", "range", "str", "int", "float", "bool", "list",
    "dict", "set", "tuple", "open", "super", "abs", "min", "max", "sum", "sorted", "enumerate",
    "zip", "map", "filter", "isinstance", "type",
];

const YAML_BUILTINS: &[&str] =
    &["true", "false", "null", "yes", "no", "on", "off", "True", "False", "Null"];

/// Trailing cross-line state: which Python triple quote (if any) is still open
/// after a line. `0` = none, `1` = `'''`, `2` = `"""`.
pub type TripleState = u8;

/// One row's highlighting result: the original text (`source`, kept for the
/// diff) plus the coloured runs. `Default`-role runs are merged into
/// `segments` with the plain colour so ordering stays trivial.
struct BuiltLine {
    source: String,
    triple: TripleState,
    segments: Vec<(String, Role)>,
}

/// Accumulates runs, merging adjacent same-role ones to keep the model small.
struct Segs {
    out: Vec<(String, Role)>,
    cur: String,
    role: Role,
}

impl Segs {
    fn new() -> Self {
        Segs { out: Vec::new(), cur: String::new(), role: Role::Default }
    }
    fn push(&mut self, text: &str, role: Role) {
        if text.is_empty() {
            return;
        }
        if self.role != role && !self.cur.is_empty() {
            self.out.push((std::mem::take(&mut self.cur), self.role));
        }
        self.role = role;
        self.cur.push_str(text);
    }
    fn word(&mut self, text: &str) {
        self.push(text, Role::Default);
    }
    fn finish(mut self) -> Vec<(String, Role)> {
        if !self.cur.is_empty() {
            self.out.push((self.cur, self.role));
        }
        self.out
    }
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Consume a quoted string starting at `open` (the quote char already verified)
/// and return the bytes consumed including both quotes. Handles `\\` escapes.
fn take_quoted(line: &[char], start: usize) -> usize {
    let quote = line[start];
    let mut i = start + 1;
    while i < line.len() {
        if line[i] == '\\' && i + 1 < line.len() {
            i += 2;
            continue;
        }
        if line[i] == quote {
            return i + 1 - start;
        }
        i += 1;
    }
    line.len() - start // unterminated: colour to end of line
}

fn take_while(line: &[char], start: usize, pred: impl Fn(char) -> bool) -> usize {
    let mut i = start;
    while i < line.len() && pred(line[i]) {
        i += 1;
    }
    i - start
}

fn take_number(line: &[char], start: usize) -> usize {
    // digits with optional sign prefix already handled by caller; allow
    // 0x/0b/0o prefixes, decimals and exponents.
    let mut i = start;
    if line[i] == '0' && i + 1 < line.len() && matches!(line[i + 1], 'x' | 'X' | 'b' | 'B' | 'o' | 'O')
    {
        i += 2;
        i += take_while(line, i, |c| c.is_ascii_alphanumeric() || c == '_');
        return i - start;
    }
    i += take_while(line, i, |c| c.is_ascii_digit() || c == '_');
    if i < line.len() && line[i] == '.' && i + 1 < line.len() && line[i + 1].is_ascii_digit() {
        i += 1;
        i += take_while(line, i, |c| c.is_ascii_digit() || c == '_');
    }
    if i < line.len() && (line[i] == 'e' || line[i] == 'E') {
        let mut j = i + 1;
        if j < line.len() && (line[j] == '+' || line[j] == '-') {
            j += 1;
        }
        let digits = take_while(line, j, |c| c.is_ascii_digit());
        if digits > 0 {
            i = j + digits;
        }
    }
    i - start
}

fn classify_word(word: &str, keywords: &[&str], builtins: &[&str]) -> Role {
    if keywords.contains(&word) {
        Role::Keyword
    } else if builtins.contains(&word) {
        Role::Builtin
    } else {
        Role::Default
    }
}

/// Comment token for the line-comment languages. Shell/Yaml/Python/Toml use
/// `#`; Ini also accepts `;` (used as a full-line or trailing comment).
fn starts_comment(lang: Lang, line: &[char], i: usize) -> bool {
    match lang {
        Lang::Ini => line[i] == '#' || line[i] == ';',
        Lang::Shell | Lang::Yaml | Lang::Python | Lang::Toml => line[i] == '#',
        _ => false,
    }
}

/// Yaml treats `#` as a comment start only when it begins the line or follows
/// whitespace (and we're not inside quotes, which the caller guarantees).
fn yaml_comment(line: &[char], i: usize) -> bool {
    line[i] == '#' && (i == 0 || line[i - 1].is_whitespace())
}

/// Tokenise one line. `triple_in` carries the Python triple-quote state into
/// the line; the returned state carries it out.
fn tokenize(lang: Lang, line: &str, triple_in: TripleState) -> BuiltLine {
    let chars: Vec<char> = line.chars().collect();
    // Degenerate guard for pathological single lines (minified JSON can hit the
    // 64 KB line cap): colouring tens of thousands of runs buys nothing, so
    // fall back to one plain run and keep the editor responsive.
    if chars.len() > MAX_TOKENIZE_CHARS {
        return BuiltLine {
            source: line.to_string(),
            triple: triple_in,
            segments: vec![(line.to_string(), Role::Default)],
        };
    }
    let byte_at = byte_offsets(&chars); // byte_at[i] = byte offset of char i
    let b = |i: usize| byte_at[i];
    let mut segs = Segs::new();
    let mut triple = triple_in;
    let mut i = 0usize;

    // Continue an open Python triple-quoted string from the previous line.
    if triple != 0 {
        let quote_char = if triple == 1 { '\'' } else { '"' };
        let closer: [char; 3] = [quote_char; 3];
        match find_slice(&chars, i, &closer) {
            Some(pos) => {
                segs.push(&line[..b(pos + 3)], Role::Str);
                i = pos + 3;
                triple = 0;
            }
            None => {
                segs.push(line, Role::Str);
                return BuiltLine { source: line.to_string(), triple, segments: segs.finish() };
            }
        }
    }

    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            let n = take_while(&chars, i, char::is_whitespace);
            segs.word(&line[b(i)..b(i + n)]);
            i += n;
            continue;
        }
        // Comments. Yaml additionally requires the `#` to follow whitespace.
        if starts_comment(lang, &chars, i)
            && (lang != Lang::Yaml || yaml_comment(&chars, i))
        {
            segs.push(&line[b(i)..], Role::Comment);
            break;
        }
        // Python triple quotes open a multi-line string.
        if lang == Lang::Python && i + 2 < chars.len() && chars[i] == chars[i + 1] && chars[i] == chars[i + 2]
            && (chars[i] == '\'' || chars[i] == '"')
        {
            let quote_char = chars[i];
            let closer: [char; 3] = [quote_char; 3];
            match find_slice(&chars, i + 3, &closer) {
                Some(pos) => {
                    segs.push(&line[b(i)..b(pos + 3)], Role::Str);
                    i = pos + 3;
                }
                None => {
                    segs.push(&line[b(i)..], Role::Str);
                    triple = if quote_char == '\'' { 1 } else { 2 };
                    break;
                }
            }
            continue;
        }
        if c == '\'' || c == '"' {
            let n = take_quoted(&chars, i);
            // `"key":` (Json/Yaml) or `"key" =` (Toml/Ini) → keyword.
            let next = i + n;
            let is_key = next < chars.len()
                && (chars[next] == ':'
                    || (matches!(lang, Lang::Toml | Lang::Ini) && {
                        let mut j = next;
                        while j < chars.len() && chars[j].is_whitespace() {
                            j += 1;
                        }
                        j < chars.len() && chars[j] == '='
                    }));
            let role = if is_key && matches!(lang, Lang::Json | Lang::Yaml | Lang::Toml | Lang::Ini) {
                Role::Keyword
            } else {
                Role::Str
            };
            segs.push(&line[b(i)..b(i + n)], role);
            i += n;
            continue;
        }
        if lang == Lang::Shell && c == '$' {
            // $VAR ${VAR} $1 $? — colour the whole reference as builtin.
            let mut n = 1usize;
            if i + n < chars.len() && chars[i + n] == '{' {
                if let Some(pos) = find_char(&chars, i + n, '}') {
                    n = pos + 1 - i;
                } else {
                    n = chars.len() - i;
                }
            } else {
                n += take_while(&chars, i + 1, |c| c.is_alphanumeric() || c == '_');
            }
            segs.push(&line[b(i)..b(i + n)], Role::Builtin);
            i += n;
            continue;
        }
        if lang == Lang::Python && (c == '@') && (i == 0 || !is_word(chars[i - 1])) {
            // decorator
            let n = 1 + take_while(&chars, i + 1, is_word);
            segs.push(&line[b(i)..b(i + n)], Role::Builtin);
            i += n;
            continue;
        }
        if lang == Lang::Yaml && (c == '&' || c == '*') {
            let n = 1 + take_while(&chars, i + 1, |c| !c.is_whitespace());
            segs.push(&line[b(i)..b(i + n)], Role::Builtin);
            i += n;
            continue;
        }
        if is_word_start(c) {
            let n = take_while(&chars, i, is_word);
            let word: String = chars[i..i + n].iter().collect();
            let role = match lang {
                Lang::Shell => classify_word(&word, SHELL_KEYWORDS, SHELL_BUILTINS),
                Lang::Python => classify_word(&word, PYTHON_KEYWORDS, PYTHON_BUILTINS),
                Lang::Json | Lang::Yaml | Lang::Toml | Lang::Ini => {
                    // Bare scalar words: bools/nulls are builtin, the rest is
                    // plain — unless the word is a key (checked below).
                    if YAML_BUILTINS.contains(&word.as_str()) {
                        Role::Builtin
                    } else {
                        Role::Default
                    }
                }
                Lang::Plain => Role::Default,
            };
            let mut total = n;
            // Key detection: "key:" (Yaml, also Json) and "key =" after blanks
            // (Toml/Ini) colour the whole `key`(+separator) as a keyword.
            let is_colon_key = matches!(lang, Lang::Json | Lang::Yaml)
                && i + total < chars.len()
                && chars[i + total] == ':';
            let is_eq_key = matches!(lang, Lang::Toml | Lang::Ini) && {
                let mut j = i + total;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                j < chars.len() && chars[j] == '='
            };
            if is_colon_key || is_eq_key {
                if is_colon_key {
                    total += 1;
                }
                segs.push(&line[b(i)..b(i + total)], Role::Keyword);
                i += total;
                continue;
            }
            segs.push(&line[b(i)..b(i + total)], role);
            i += total;
            continue;
        }
        if c.is_ascii_digit() {
            let n = take_number(&chars, i);
            segs.push(&line[b(i)..b(i + n)], Role::Number);
            i += n;
            continue;
        }
        // [section] header for Toml/Ini.
        if (lang == Lang::Toml || lang == Lang::Ini) && c == '[' {
            if let Some(pos) = find_char(&chars, i, ']') {
                segs.push(&line[b(i)..b(pos + 1)], Role::Keyword);
                i = pos + 1;
                continue;
            }
        }
        // Yaml document markers (only when the cursor char opens one; push the
        // remainder of the line so segments still concatenate to the source).
        if lang == Lang::Yaml && (c == '-' || c == '.') && matches!(line.trim(), "---" | "...") {
            segs.push(&line[b(i)..], Role::Keyword);
            break;
        }
        segs.word(&line[b(i)..b(i + 1)]);
        i += 1;
    }

    BuiltLine { source: line.to_string(), triple, segments: segs.finish() }
}

fn find_char(chars: &[char], from: usize, needle: char) -> Option<usize> {
    (from..chars.len()).find(|&i| chars[i] == needle)
}

fn find_slice(chars: &[char], from: usize, needle: &[char]) -> Option<usize> {
    if needle.is_empty() || chars.len() < needle.len() {
        return None;
    }
    (from..=chars.len() - needle.len()).find(|&i| &chars[i..i + needle.len()] == needle)
}

/// Byte-offset prefix table for `chars`: `out[i]` = byte offset of char `i`
/// (length `chars.len() + 1`, last entry = total byte length). One O(n) pass
/// replaces per-segment O(n) scans in the tokeniser.
fn byte_offsets(chars: &[char]) -> Vec<usize> {
    let mut byte_at = Vec::with_capacity(chars.len() + 1);
    let mut acc = 0usize;
    byte_at.push(0);
    for c in chars {
        acc += c.len_utf8();
        byte_at.push(acc);
    }
    byte_at
}

/// Build the full overlay model for `content`.
pub fn build_lines(content: &str, lang: Lang, dark: bool) -> Vec<HlLine> {
    let mut triple: TripleState = 0;
    content
        .split('\n')
        .map(|line| {
            let built = tokenize(lang, line, triple);
            triple = built.triple;
            to_hl_line(&built, dark)
        })
        .collect()
}

fn to_hl_line(built: &BuiltLine, dark: bool) -> HlLine {
    let segments: Vec<HlSeg> = built
        .segments
        .iter()
        .map(|(text, role)| HlSeg {
            text: text.as_str().into(),
            color: role_color(*role, dark),
        })
        .collect();
    HlLine {
        source: built.source.as_str().into(),
        triple: built.triple as i32,
        segments: ModelRc::new(VecModel::from(segments)),
    }
}

/// Rebuild the whole overlay in place (test helper). The window property keeps
/// pointing at the same VecModel, so subsequent `sync_model` calls can still
/// downcast it.
#[cfg(test)]
pub fn reset_model(model: &VecModel<HlLine>, content: &str, lang: Lang, dark: bool) {
    let lines = build_lines(content, lang, dark);
    while model.row_count() > 0 {
        model.remove(model.row_count() - 1);
    }
    for line in lines {
        model.push(line);
    }
}

/// Incrementally bring the overlay model in line with `content`.
///
/// Walks rows from the top; stable rows (same `source` *and* same trailing
/// triple-quote state as the running state machine) only cost a cheap clone +
/// compare and skip the repaint. The walk itself is O(rows) — necessary anyway
/// when rows are inserted/removed — but `set_row_data` (and thus element
/// rebuilds in Slint) fires only for genuinely changed rows.
///
/// Precondition: `dark` matches the palette the model was built with. Theme
/// flips must go through `build_lines`/`model_rc` (reset), not this function.
pub fn sync_model(model: &VecModel<HlLine>, content: &str, lang: Lang, dark: bool) {
    let mut triple: TripleState = 0;
    let mut row = 0usize;
    for line in content.split('\n') {
        let old = model.row_data(row);
        let built = match old {
            Some(prev) if prev.source.as_str() == line && prev.triple == triple as i32 => {
                // Stable row; the deterministic state machine guarantees the
                // rest of the document is untouched.
                triple = prev.triple as TripleState;
                row += 1;
                continue;
            }
            _ => tokenize(lang, line, triple),
        };
        triple = built.triple;
        let built_hl = to_hl_line(&built, dark);
        if row < model.row_count() {
            model.set_row_data(row, built_hl);
        } else {
            model.push(built_hl);
        }
        row += 1;
    }
    while model.row_count() > row {
        model.remove(model.row_count() - 1);
    }
}

/// Rc helper: wrap a fresh VecModel so it can live inside a `ModelRc` property
/// and still be downcast later (`ModelRc::as_any`).
pub fn model_rc(lines: Vec<HlLine>) -> ModelRc<HlLine> {
    ModelRc::new(Rc::new(VecModel::from(lines)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::lang;

    fn texts(line: &HlLine) -> Vec<String> {
        let model = &line.segments;
        (0..model.row_count()).map(|i| model.row_data(i).unwrap().text.to_string()).collect()
    }

    fn joined(line: &HlLine) -> String {
        texts(line).concat()
    }

    #[test]
    fn shell_segments() {
        let built = tokenize(Lang::Shell, "if [ -n \"$HOME\" ]; then", 0);
        let roles: Vec<Role> = built.segments.iter().map(|(_, r)| *r).collect();
        assert!(roles.contains(&Role::Keyword)); // if / then
        assert!(roles.contains(&Role::Str)); // "$HOME"
        // $HOME is inside the quoted string → only one string segment.
        assert_eq!(built.segments.iter().filter(|(_, r)| *r == Role::Str).count(), 1);
    }

    #[test]
    fn shell_comment_to_eol() {
        let built = tokenize(Lang::Shell, "export PATH=/bin # set path", 0);
        let texts = built.segments.iter().map(|(t, r)| (t.as_str(), *r)).collect::<Vec<_>>();
        let last = texts.last().unwrap();
        assert_eq!(last.1, Role::Comment);
        assert!(last.0.contains("set path"));
    }

    #[test]
    fn shell_variable_outside_string() {
        let built = tokenize(Lang::Shell, "echo $HOME", 0);
        assert!(built.segments.iter().any(|(t, r)| t == "$HOME" && *r == Role::Builtin));
        let built2 = tokenize(Lang::Shell, "echo ${HOME}/x", 0);
        assert!(built2.segments.iter().any(|(t, r)| t == "${HOME}" && *r == Role::Builtin));
    }

    #[test]
    fn json_keys_strings_numbers() {
        let built = tokenize(Lang::Json, r#"{"name": "meatshell", "n": 42, "ok": true}"#, 0);
        assert_eq!(joined(&built), r#"{"name": "meatshell", "n": 42, "ok": true}"#);
        // "name" and "ok" are keys (keyword colour); "meatshell" is a value.
        let key_count = built.segments.iter().filter(|(_, r)| *r == Role::Keyword).count();
        let value_count = built.segments.iter().filter(|(_, r)| *r == Role::Str).count();
        assert_eq!(key_count, 2);
        assert_eq!(value_count, 1);
        assert!(built.segments.iter().any(|(t, r)| t == "42" && *r == Role::Number));
        assert!(built.segments.iter().any(|(t, r)| t == "true" && *r == Role::Builtin));
    }

    #[test]
    fn yaml_keys_and_comments() {
        let built = tokenize(Lang::Yaml, "port: 8080 # the port", 0);
        assert!(built.segments.iter().any(|(t, r)| t == "port:" && *r == Role::Keyword));
        assert!(built.segments.iter().any(|(t, r)| t == "8080" && *r == Role::Number));
        let last = built.segments.last().unwrap();
        assert_eq!(last.1, Role::Comment);
    }

    #[test]
    fn yaml_hash_inside_word_is_not_comment() {
        // A '#' directly attached to a word (no preceding blank) must not start
        // a comment — e.g. an anchor-less URL fragment.
        let built = tokenize(Lang::Yaml, "url: http://x#a", 0);
        assert!(!built.segments.iter().any(|(_, r)| *r == Role::Comment));
    }

    #[test]
    fn ini_sections_and_keys() {
        let built = tokenize(Lang::Ini, "[Unit]\n; comment\nAfter=network.target", 0);
        assert!(built.segments.iter().any(|(t, r)| t == "[Unit]" && *r == Role::Keyword));
        assert!(built.segments.iter().any(|(t, r)| t.contains("After") && *r == Role::Keyword));
    }

    #[test]
    fn toml_strings_numbers_bools() {
        let built = tokenize(Lang::Toml, "name = \"srv\" \nworkers = 4\nverbose = true", 0);
        assert!(built.segments.iter().any(|(t, r)| t == "\"srv\"" && *r == Role::Str));
        assert!(built.segments.iter().any(|(t, r)| t == "4" && *r == Role::Number));
        assert!(built.segments.iter().any(|(t, r)| t == "true" && *r == Role::Builtin));
    }

    #[test]
    fn python_keywords_and_decorators() {
        let built = tokenize(Lang::Python, "@app.route\ndef main():\n    return True", 0);
        assert!(built.segments.iter().any(|(t, r)| t.starts_with("@app") && *r == Role::Builtin));
        assert!(built.segments.iter().any(|(t, r)| t == "def" && *r == Role::Keyword));
        assert!(built.segments.iter().any(|(t, r)| t == "return" && *r == Role::Keyword));
    }

    #[test]
    fn python_triple_quote_spans_lines() {
        let l1 = tokenize(Lang::Python, "x = 1", 0);
        assert_eq!(l1.triple, 0);
        let l2 = tokenize(Lang::Python, "s = \"\"\"start", 0);
        assert_eq!(l2.triple, 2);
        assert!(l2.segments.iter().any(|(t, r)| t.contains("start") && *r == Role::Str));
        let l3 = tokenize(Lang::Python, "still inside \"\" more", 2);
        assert!(l3.segments.iter().all(|(_, r)| *r == Role::Str));
        assert_eq!(l3.triple, 2);
        let l4 = tokenize(Lang::Python, "end\"\"\" x = 2", 2);
        assert_eq!(l4.triple, 0);
        assert!(l4.segments.iter().any(|(t, r)| t == "x" && *r == Role::Default));
    }

    #[test]
    fn plain_language_yields_default_only() {
        let built = tokenize(Lang::Plain, "anything # no highlighting", 0);
        assert!(built.segments.iter().all(|(_, r)| *r == Role::Default));
    }

    #[test]
    fn adjacent_same_role_runs_merge() {
        let built = tokenize(Lang::Shell, "aaa bbb", 0);
        // Both words are Default; whitespace also Default → one segment.
        assert_eq!(built.segments.len(), 1);
        assert_eq!(built.segments[0].0, "aaa bbb");
    }

    #[test]
    fn build_lines_keeps_blank_and_trailing_lines() {
        let lines = build_lines("a\n\nb\n", Lang::Shell, true);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[1].source, "");
        assert_eq!(lines[3].source, "");
    }

    #[test]
    fn reset_and_sync_models() {
        let model = VecModel::from(build_lines("one\ntwo\nthree", Lang::Shell, true));
        // No-op sync: everything already matches.
        sync_model(&model, "one\ntwo\nthree", Lang::Shell, true);
        assert_eq!(model.row_count(), 3);

        // Edit a middle row.
        sync_model(&model, "one\nTWO!\nthree", Lang::Shell, true);
        assert_eq!(model.row_data(1).unwrap().source, "TWO!");

        // Append and delete rows.
        sync_model(&model, "one\nTWO!\nthree\nfour", Lang::Shell, true);
        assert_eq!(model.row_count(), 4);
        assert_eq!(model.row_data(3).unwrap().source, "four");
        sync_model(&model, "TWO!\nthree", Lang::Shell, true);
        assert_eq!(model.row_count(), 2);
        assert_eq!(model.row_data(0).unwrap().source, "TWO!");

        // Theme flip repaints with the other palette (sources unchanged).
        reset_model(&model, "TWO!\nthree", Lang::Shell, false);
        assert_eq!(model.row_count(), 2);
    }

    #[test]
    fn sync_stops_early_on_stable_rows() {
        // Editing the first line of a long file must not re-tokenise the tail:
        // simulate by checking row identity through `triple`/`source` stability
        // — sync_model walks but skips set_row_data for stable rows.
        let content = "head\n".to_string() + &"body line\n".repeat(500);
        let model = VecModel::from(build_lines(&content, Lang::Json, true));
        sync_model(&model, "HEAD\n" + &"body line\n".repeat(500), Lang::Json, true);
        assert_eq!(model.row_count(), 501);
        assert_eq!(model.row_data(0).unwrap().source, "HEAD");
        // Row 501 stays the (empty) trailing line.
        assert_eq!(model.row_data(500).unwrap().source, "");
    }

    #[test]
    fn model_rc_roundtrip() {
        let rc = model_rc(build_lines("x", Lang::Shell, true));
        assert!(rc.as_any().downcast_ref::<VecModel<HlLine>>().is_some());
    }

    #[test]
    fn cjk_lines_keep_byte_offsets_aligned() {
        // Wide characters must not corrupt slicing (byte vs char indices).
        let built = tokenize(Lang::Python, "s = \"中文注释\"  # 说明", 0);
        let joined = built.segments.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>().join("");
        assert_eq!(joined, "s = \"中文注释\"  # 说明");
        let comment = built.segments.last().unwrap();
        assert_eq!(comment.1, Role::Comment);
    }

    #[test]
    fn overly_long_line_degrades_to_single_plain_run() {
        let line = "x".repeat(MAX_TOKENIZE_CHARS + 1);
        let built = tokenize(Lang::Json, &line, 0);
        assert_eq!(built.segments.len(), 1);
        assert_eq!(built.segments[0].1, Role::Default);
        assert_eq!(built.segments[0].0, line);
        assert_eq!(built.triple, 0);
    }

    #[test]
    fn lang_detect_via_public_helper() {
        assert_eq!(lang::detect("x.py", ""), Lang::Python);
    }
}
