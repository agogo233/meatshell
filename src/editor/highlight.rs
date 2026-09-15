//! Lightweight per-line syntax highlighter for the built-in editor.
//!
//! Design constraints (see the editor-highlight plan):
//! * zero new dependencies — hand-written scanners, no regex, no syntect;
//! * line-oriented with a small explicit cross-line state per row (`carry`:
//!   open triple-quoted strings for Python/TOML, open shell heredocs);
//! * output is `[HlLine]` models whose `segments` are coloured text runs the
//!   Slint overlay lays out horizontally next to the transparent `TextInput`.
//!
//! Correctness contract for `sync_model`: the tokeniser is a deterministic
//! state machine, so a row whose source text *and* trailing carry state match
//! the running state implies every following row tokenises identically —
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
    /// Control-flow keywords (if/for/return…). VS Code "keyword.control"
    /// colour: #C586C0 dark, #AF00DB light.
    Control,
    Str,
    Comment,
    Number,
    Builtin,
    /// Shell parameter expansions ($HOME, ${X}, $?). VS Code "variable"
    /// colour: #9CDCFE dark, #001080 light.
    Variable,
    /// Python builtin types (int/str/list…). VS Code "entity.name.type"
    /// colour: #4EC9B0 dark, #267F99 light.
    Type,
}

/// Band overlay strength. Applied here (rather than via `.with-alpha()` in
/// the .slint file) so that `Role::Default` can stay fully transparent:
/// `with-alpha` overwrites the alpha of whatever it is given.
const BAND_ALPHA: u8 = 66; // round(0.26 * 255); Color channels are u8 anyway

fn role_color(role: Role, dark: bool) -> Color {
    // Default (plain text/whitespace) segments only exist to advance the
    // x position of the coloured runs after them; tinting them black painted
    // an exposed grey block on whitespace-only "empty" lines (no glyph
    // covers it there) and a grey wash under plain text.
    if role == Role::Default {
        return Color::from_argb_u8(0x00, 0, 0, 0);
    }
    let rgb: (u8, u8, u8) = match (role, dark) {
        (Role::Keyword, true) => (0x56, 0x9c, 0xd6),
        (Role::Control, true) => (0xc5, 0x86, 0xc0),
        (Role::Str, true) => (0xce, 0x91, 0x78),
        (Role::Comment, true) => (0x6a, 0x99, 0x55),
        (Role::Number, true) => (0xb5, 0xce, 0xa8),
        (Role::Builtin, true) => (0xdc, 0xdc, 0xaa),
        (Role::Variable, true) => (0x9c, 0xdc, 0xfe),
        (Role::Type, true) => (0x4e, 0xc9, 0xb0),
        (Role::Keyword, false) => (0x00, 0x00, 0xff),
        (Role::Control, false) => (0xaf, 0x00, 0xdb),
        (Role::Str, false) => (0xa3, 0x15, 0x15),
        (Role::Comment, false) => (0x00, 0x80, 0x00),
        (Role::Number, false) => (0x09, 0x86, 0x58),
        (Role::Builtin, false) => (0x79, 0x5e, 0x26),
        (Role::Variable, false) => (0x00, 0x10, 0x80),
        (Role::Type, false) => (0x26, 0x7f, 0x99),
        (Role::Default, _) => unreachable!("handled above"),
    };
    Color::from_argb_u8(BAND_ALPHA, rgb.0, rgb.1, rgb.2)
}

/// Keyword/builtin word lists for one language, split by the VS Code role
/// each word class maps to. Empty slices simply never match.
struct WordRules {
    /// Control-flow / statement keywords → `Role::Control`.
    control: &'static [&'static str],
    /// Declaration / namespace keywords → `Role::Keyword`.
    keywords: &'static [&'static str],
    /// Builtin type names → `Role::Type`.
    types: &'static [&'static str],
    /// Builtin functions/commands → `Role::Builtin`.
    builtins: &'static [&'static str],
    /// Literals (bools, None) → `Role::Keyword` (VS Code colours
    /// `constant.language` like a keyword).
    literals: &'static [&'static str],
}

// Control flow / reserved words (bash "keyword" class in highlight.js).
const SHELL_CONTROL: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "function", "select", "coproc", "time",
];

// Declaration / special builtin commands (export, local, …): VS Code paints
// these `keyword.other` blue, distinct from control flow.
const SHELL_KEYWORDS: &[&str] = &[
    "return", "export", "readonly", "local", "declare", "typeset", "unset", "set", "shopt",
    "shift", "trap", "eval", "exec", "exit", "source", "alias", "unalias", "let", "getopts",
    "hash", "builtin", "command", "caller", "mapfile", "readarray", "bind", "logout", "suspend",
    "disown", "enable", "fc", "history", "times", "umask", "ulimit",
];

// Everyday builtin commands (yellow).
const SHELL_BUILTINS: &[&str] = &[
    "echo", "printf", "cd", "pwd", "read", "type", "true", "false", "test", "pushd", "popd",
    "jobs", "fg", "bg", "kill", "wait", "help",
];

const SHELL_RULES: WordRules = WordRules {
    control: SHELL_CONTROL,
    keywords: SHELL_KEYWORDS,
    types: &[],
    builtins: SHELL_BUILTINS,
    literals: &[],
};

const PYTHON_CONTROL: &[&str] = &[
    "if", "elif", "else", "for", "while", "break", "continue", "return", "try", "except",
    "finally", "with", "yield", "pass", "raise", "match", "case",
];

const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "class", "def", "del", "from", "global", "import",
    "in", "is", "lambda", "nonlocal", "not", "or",
];

// Literal constants earn the keyword colour (VS Code `constant.language`).
const PYTHON_LITERALS: &[&str] = &["True", "False", "None", "self", "cls"];

const PYTHON_TYPES: &[&str] = &[
    "bool", "bytearray", "bytes", "complex", "dict", "float", "frozenset", "int", "list",
    "memoryview", "object", "set", "str", "tuple",
];

// The remaining Python builtins (functions) keep the yellow builtin colour;
// the type names live in `PYTHON_TYPES` (teal) and are not repeated here.
const PYTHON_BUILTINS: &[&str] = &[
    "__import__", "abs", "aiter", "all", "anext", "any", "ascii", "bin", "breakpoint", "callable",
    "chr", "classmethod", "compile", "delattr", "dir", "divmod", "enumerate", "eval", "exec",
    "filter", "format", "getattr", "globals", "hasattr", "hash", "help", "hex", "id", "input",
    "isinstance", "issubclass", "iter", "len", "locals", "map", "max", "min", "next", "oct",
    "open", "ord", "pow", "print", "property", "range", "repr", "reversed", "round", "setattr",
    "slice", "sorted", "staticmethod", "sum", "super", "type", "vars", "zip",
];

const PYTHON_RULES: WordRules = WordRules {
    control: PYTHON_CONTROL,
    keywords: PYTHON_KEYWORDS,
    types: PYTHON_TYPES,
    builtins: PYTHON_BUILTINS,
    literals: PYTHON_LITERALS,
};

// Dockerfile line-leading instructions (always upper-case in practice).
const DOCKER_INSTRUCTIONS: &[&str] = &[
    "ARG", "ADD", "CMD", "COPY", "ENTRYPOINT", "ENV", "EXPOSE", "FROM", "HEALTHCHECK", "LABEL", "MAINTAINER",
    "ONBUILD", "RUN", "SHELL", "STOPSIGNAL", "USER", "VOLUME", "WORKDIR",
];

const YAML_BUILTINS: &[&str] =
    &["true", "false", "null", "yes", "no", "on", "off", "True", "False", "Null"];


/// Cross-line scanner state encoded as a compact string stored on every row
/// (`HlLine.carry`) so `sync_model` can diff it cheaply:
/// * `""` — nothing open;
/// * `py3dq`/`py3sq` — inside an open Python `"""`/`'''` string;
/// * `tl3dq`/`tl3sq` — inside an open TOML multi-line string;
/// * `hd:WORD` / `hd-:WORD` — inside a shell heredoc (`<<-`, tab-stripped).
fn carry_open_quote(carry: &str) -> Option<char> {
    match carry {
        "py3dq" | "tl3dq" => Some('"'),
        "py3sq" | "tl3sq" => Some('\''),
        _ => None,
    }
}

fn triple_tag(quote: char, lang: Lang) -> &'static str {
    let py = matches!(lang, Lang::Python);
    match (py, quote) {
        (true, '"') => "py3dq",
        (true, _) => "py3sq",
        (false, '"') => "tl3dq",
        (false, _) => "tl3sq",
    }
}

/// One row's highlighting result: the original text (`source`, kept for the
/// diff) plus the coloured runs. `Default`-role runs are merged into
/// `segments` with the plain colour so ordering stays trivial.
struct BuiltLine {
    source: String,
    carry: String,
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

/// Whether the number token at `[i, i + n)` stands on its own, or is buried
/// in a longer Yaml/Shell scalar (`nemotron-3.5`, `30b` in model ids, dates
/// like `2026-08-24`): buried ones stay plain, only standalone numbers earn
/// the band. A `-` on the left counts as joins unless it reads as a sign —
/// itself preceded by start-of-line, whitespace, or `[`/`{`/`,`/`:`/`=` — so
/// `timeout: -1` and Shell `KEY=-1` still colour while `nemotron-3.5` stays
/// plain.
/// `chars[i..i + 10]` is a bare TOML date (`YYYY-MM-DD`) followed by
/// whitespace, `T` (datetime) or end of line.
fn is_iso_date(chars: &[char], i: usize) -> bool {
    if chars.len() < i + 10 {
        return false;
    }
    let digits = |a: usize, b: usize| chars[a..b].iter().all(char::is_ascii_digit);
    digits(i, i + 4)
        && chars[i + 4] == '-'
        && digits(i + 5, i + 7)
        && chars[i + 7] == '-'
        && digits(i + 8, i + 10)
        && matches!(chars.get(i + 10), None | Some(' ') | Some('T'))
}

fn number_is_standalone(chars: &[char], i: usize, n: usize) -> bool {
    let joins = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '/' | '.');
    let prev_ok = if i == 0 {
        true
    } else if chars[i - 1] == '-' {
        i - 1 == 0
            || chars[i - 2].is_whitespace()
            || matches!(chars[i - 2], '[' | '{' | ',' | ':' | '=')
    } else {
        !joins(chars[i - 1])
    };
    let next_ok = i + n >= chars.len() || !joins(chars[i + n]);
    prev_ok && next_ok
}

fn classify_word(word: &str, rules: &WordRules) -> Role {
    if rules.control.contains(&word) {
        Role::Control
    } else if rules.keywords.contains(&word) || rules.literals.contains(&word) {
        // Literals (True/None) share the keyword colour, like VS Code.
        Role::Keyword
    } else if rules.types.contains(&word) {
        Role::Type
    } else if rules.builtins.contains(&word) {
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
        Lang::Shell | Lang::Yaml | Lang::Python | Lang::Toml | Lang::Dockerfile | Lang::Nginx => {
            line[i] == '#'
        }
        Lang::Makefile => line[i] == '#',
        _ => false,
    }
}

/// Yaml treats `#` as a comment start only when it begins the line or follows
/// whitespace (and we're not inside quotes, which the caller guarantees).
fn yaml_comment(line: &[char], i: usize) -> bool {
    line[i] == '#' && (i == 0 || line[i - 1].is_whitespace())
}

/// Tokenise one line. `carry_in` carries the cross-line scanner state into
/// the line; the returned state carries it out.
fn tokenize(lang: Lang, line: &str, carry_in: &str) -> BuiltLine {
    let chars: Vec<char> = line.chars().collect();
    // Degenerate guard for pathological single lines (minified JSON can hit the
    // 64 KB line cap): colouring tens of thousands of runs buys nothing, so
    // fall back to one plain run and keep the editor responsive.
    if chars.len() > MAX_TOKENIZE_CHARS {
        return BuiltLine {
            source: line.to_string(),
            carry: carry_in.to_string(),
            segments: vec![(line.to_string(), Role::Default)],
        };
    }
    let byte_at = byte_offsets(&chars); // byte_at[i] = byte offset of char i
    let b = |i: usize| byte_at[i];
    let mut segs = Segs::new();
    let mut carry_out = String::new();
    let mut i = 0usize;
    // Last classified bare word (for Python `def`/`class` name colouring).
    let mut prev_word: Option<&str> = None;
    // Heredoc opened by this line (`<<WORD` / `<<-WORD`): (delimiter, indented).
    let mut pending_heredoc: Option<(String, bool)> = None;
    // Paren depth so `$((1 << 8))` shift ops never read as heredocs.
    let mut paren_depth = 0i32;

    // A heredoc body line is verbatim text up to (and including) the closing
    // delimiter at column 0 (`<<-` also allows leading tabs). Comparisons are
    // exact and case-sensitive; a never-closed heredoc simply runs to EOF.
    if lang == Lang::Shell {
        if let Some(spec) = carry_in.strip_prefix("hd:") {
            let closed = line.trim_end_matches('\r') == spec;
            segs.push(line, Role::Str);
            return BuiltLine {
                source: line.to_string(),
                carry: if closed { String::new() } else { carry_in.to_string() },
                segments: segs.finish(),
            };
        } else if let Some(spec) = carry_in.strip_prefix("hd-:") {
            let closed = line.trim_start_matches('\t').trim_end_matches('\r') == spec;
            segs.push(line, Role::Str);
            return BuiltLine {
                source: line.to_string(),
                carry: if closed { String::new() } else { carry_in.to_string() },
                segments: segs.finish(),
            };
        }
    }

    // Continue an open multi-line string from the previous line.
    if let Some(quote_char) = carry_open_quote(carry_in) {
        let closer: [char; 3] = [quote_char; 3];
        match find_slice(&chars, 0, &closer) {
            Some(pos) => {
                segs.push(&line[..b(pos + 3)], Role::Str);
                i = pos + 3;
            }
            None => {
                segs.push(line, Role::Str);
                return BuiltLine { source: line.to_string(), carry: carry_in.to_string(), segments: segs.finish() };
            }
        }
    }

    while i < chars.len() {
        let c = chars[i];
        // Yaml mapping key: the whole bare run (`a-b_c/d`) up to the `:`
        // that opens a block-style mapping (`:` followed by whitespace or
        // end of line) is one keyword, not just the last word — model ids
        // and version fragments inside keys keep their shape.
        if lang == Lang::Yaml && (is_word_start(c) || c.is_ascii_digit()) {
            let mut j = i;
            while j < chars.len() && !chars[j].is_whitespace() && chars[j] != ':' {
                j += 1;
            }
            if j < chars.len()
                && chars[j] == ':'
                && (j + 1 == chars.len() || chars[j + 1].is_whitespace())
            {
                segs.push(&line[b(i)..b(j + 1)], Role::Keyword);
                i = j + 1;
                continue;
            }
        }
        if c.is_whitespace() {
            let n = take_while(&chars, i, char::is_whitespace);
            segs.word(&line[b(i)..b(i + n)]);
            i += n;
            continue;
        }
        // Comments. Yaml additionally requires the `#` to follow whitespace;
        // Shell requires it at a word start (`a#b` is one literal word in
        // bash, not a comment).
        if starts_comment(lang, &chars, i)
            && match lang {
                Lang::Yaml => yaml_comment(&chars, i),
                Lang::Shell => i == 0 || !is_word(chars[i - 1]),
                _ => true,
            }
        {
            segs.push(&line[b(i)..], Role::Comment);
            break;
        }
        // Python / TOML triple quotes open a multi-line string.
        if matches!(lang, Lang::Python | Lang::Toml)
            && i + 2 < chars.len()
            && chars[i] == chars[i + 1]
            && chars[i] == chars[i + 2]
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
                    carry_out = triple_tag(quote_char, lang).to_string();
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
        if lang == Lang::Shell && c == '<' && chars.get(i + 1) == Some(&'<') && paren_depth == 0 {
            if chars.get(i + 2) == Some(&'<') {
                // `<<<` here-string: a single redirection operator, no body.
                segs.push(&line[b(i)..b(i + 3)], Role::Default);
                i += 3;
                continue;
            }
            // Heredoc opener `<<[-] WORD` (quotes tolerated). Colour the marker and remember the
            // delimiter: the verbatim body starts on the next line.
            let mut j = i + 2;
            let indented = chars.get(j) == Some(&'-');
            if indented {
                j += 1;
            }
            while chars.get(j).is_some_and(|ch| ch.is_whitespace()) {
                j += 1;
            }
            let quote = match chars.get(j) {
                Some(q @ ('\'' | '"')) => {
                    j += 1;
                    Some(*q)
                }
                _ => None,
            };
            let m = take_while(&chars, j, is_word);
            if m > 0 && pending_heredoc.is_none() {
                let mut end = j + m;
                if quote.is_some() && chars.get(end) == quote.as_ref() {
                    end += 1;
                }
                pending_heredoc = Some((chars[j..j + m].iter().collect(), indented));
                segs.push(&line[b(i)..b(end)], Role::Str);
                i = end;
                continue;
            }
        }
        if lang == Lang::Makefile && c == '$' {
            // $(VAR) / ${VAR} / $@ $< $^ — all makefile variables.
            let mut n = 1;
            if matches!(chars.get(i + n), Some('(') | Some('{')) {
                let closer = if chars[i + n] == '(' { ')' } else { '}' };
                match find_char(&chars, i + n, closer) {
                    Some(pos) => n = pos + 1 - i,
                    None => n = chars.len() - i,
                }
            } else {
                n += take_while(&chars, i + n, is_word);
                if n == 1
                    && i + 1 < chars.len()
                    && matches!(chars[i + 1], '@' | '%' | '<' | '?' | '+' | '^' | '|')
                {
                    n = 2;
                }
            }
            segs.push(&line[b(i)..b(i + n)], Role::Variable);
            i += n;
            continue;
        }
        if matches!(lang, Lang::Shell | Lang::Dockerfile | Lang::Nginx) && c == '$' {
            // $VAR ${VAR} $1 $? — colour the whole reference as a variable
            // (VS Code `variable` role). `$(...)` / `$(( ... ))` / `$[...]`
            // command & arithmetic substitutions leave the `$` plain:
            // colouring it alone looked like a stray variable marker before
            // the parens.
            let mut n = 1usize;
            if i + n < chars.len() && chars[i + n] == '{' {
                if let Some(pos) = find_char(&chars, i + n, '}') {
                    n = pos + 1 - i;
                } else {
                    n = chars.len() - i;
                }
            } else if i + n < chars.len() && (chars[i + n] == '(' || chars[i + n] == '[') {
                segs.push(&line[b(i)..b(i + n)], Role::Default);
                i += n;
                continue;
            } else {
                n += take_while(&chars, i + 1, |c| c.is_alphanumeric() || c == '_');
                // $# $? $$ $! $* $@ $- — one-char special parameters (the
                // comment promised them; digits were already covered above).
                if n == 1
                    && i + 1 < chars.len()
                    && matches!(chars[i + 1], '#' | '?' | '$' | '!' | '*' | '@' | '-')
                {
                    n = 2;
                }
            }
            segs.push(&line[b(i)..b(i + n)], Role::Variable);
            i += n;
            continue;
        }
        if lang == Lang::Python && (c == '@') && (i == 0 || !is_word(chars[i - 1])) {
            // decorator, including dotted paths (`@app.route`).
            let mut n = 1 + take_while(&chars, i + 1, is_word);
            while i + n < chars.len()
                && chars[i + n] == '.'
                && i + n + 1 < chars.len()
                && is_word_start(chars[i + n + 1])
            {
                n += 1 + take_while(&chars, i + n + 1, is_word);
            }
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
        if lang == Lang::Makefile && i == 0 {
            // Rule target: the token run in column 0 up to `:` (`build: all`,
            // `%.o: %.c`, `.PHONY:`). `:=` assignments are skipped by the `=`
            // scan stop; recipes start with a tab and never match.
            let mut j = 0;
            while j < chars.len()
                && !chars[j].is_whitespace()
                && chars[j] != ':'
                && chars[j] != '='
                && chars[j] != '#'
            {
                j += 1;
            }
            if j > 0
                && j < chars.len()
                && chars[j] == ':'
                && chars.get(j + 1) != Some(&'=')
            {
                segs.push(&line[..b(j)], Role::Builtin);
                i = j;
                continue;
            }
        }
        // Nginx: the first word of a line is a directive or block name.
        if lang == Lang::Nginx && is_word_start(c) && line[..b(i)].trim().is_empty() {
            let n = take_while(&chars, i, is_word);
            segs.push(&line[b(i)..b(i + n)], Role::Keyword);
            prev_word = Some(&line[b(i)..b(i + n)]);
            i += n;
            continue;
        }
        // Dockerfile: the line-leading instruction is a keyword.
        if lang == Lang::Dockerfile && is_word_start(c) && line[..b(i)].trim().is_empty() {
            let n = take_while(&chars, i, is_word);
            let word: String = chars[i..i + n].iter().collect();
            if DOCKER_INSTRUCTIONS.contains(&word.as_str()) {
                segs.push(&line[b(i)..b(i + n)], Role::Keyword);
                prev_word = Some(&line[b(i)..b(i + n)]);
                i += n;
                continue;
            }
        }
        if is_word_start(c) {
            let n = take_while(&chars, i, is_word);
            let word: String = chars[i..i + n].iter().collect();
            // A bare URL `word://…` (unquoted is legal only in Yaml) would
            // otherwise scatter number/keyword bands across the host, port
            // and path; keep the whole address plain up to the next space
            // so a trailing `# comment` is still recognised.
            let scheme_end = i + n;
            if lang == Lang::Yaml
                && scheme_end + 2 < chars.len()
                && chars[scheme_end] == ':'
                && chars[scheme_end + 1] == '/'
                && chars[scheme_end + 2] == '/'
            {
                let m = take_while(&chars, i, |ch| !ch.is_whitespace());
                segs.push(&line[b(i)..b(i + m)], Role::Default);
                i += m;
                continue;
            }
            let mut role = match lang {
                Lang::Shell => classify_word(&word, &SHELL_RULES),
                Lang::Python => classify_word(&word, &PYTHON_RULES),
                Lang::Json | Lang::Yaml | Lang::Toml | Lang::Ini => {
                    // Bare scalar words: bools/nulls are builtin, the rest is
                    // plain — unless the word is a key (checked below).
                    if YAML_BUILTINS.contains(&word.as_str()) {
                        Role::Builtin
                    } else {
                        Role::Default
                    }
                }
                Lang::Plain | Lang::Dockerfile | Lang::Makefile | Lang::Nginx => Role::Default,
            };
            // Python: the name bound by `def`/`class` takes VS Code's
            // function/type colouring instead of staying plain.
            if lang == Lang::Python
                && role == Role::Default
                && matches!(prev_word, Some("def") | Some("class"))
            {
                role = if prev_word == Some("def") { Role::Builtin } else { Role::Type };
            }
            let mut total = n;
            // TOML: a bare key may span dotted segments (`server.host`);
            // claim them so the whole key reads as one keyword run.
            if lang == Lang::Toml {
                let mut j = i + total;
                loop {
                    let mut k = j;
                    while k < chars.len() && chars[k].is_whitespace() {
                        k += 1;
                    }
                    if k >= chars.len() || chars[k] != '.' {
                        break;
                    }
                    let m = take_while(&chars, k + 1, is_word);
                    if m == 0 {
                        break;
                    }
                    j = k + 1 + m;
                    total = j - i;
                }
            }
            // Shell: a plain command word immediately followed by `()` is a
            // function definition — colour its name like a builtin.
            if lang == Lang::Shell && role == Role::Default {
                let mut j = i + total;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                if j < chars.len() && chars[j] == '(' {
                    let mut k = j + 1;
                    while k < chars.len() && chars[k].is_whitespace() {
                        k += 1;
                    }
                    if k < chars.len() && chars[k] == ')' {
                        role = Role::Builtin;
                    }
                }
            }
            // Key detection: "key:" (Json) and "key =" after blanks (Toml/Ini)
            // colour the whole `key`(+separator) as a keyword. Yaml keys are
            // claimed by the bare-key scan above, which requires the colon to
            // be followed by whitespace so `http://` fragments do not match.
            let is_colon_key = matches!(lang, Lang::Json)
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
                prev_word = Some(&line[b(i - total)..b(i)]);
                continue;
            }
            segs.push(&line[b(i)..b(i + total)], role);
            prev_word = Some(&line[b(i)..b(i + total)]);
            i += total;
            continue;
        }
        if c.is_ascii_digit() {
            // TOML bare date (YYYY-MM-DD): one number band, not three.
            if lang == Lang::Toml && is_iso_date(&chars, i) {
                segs.push(&line[b(i)..b(i + 10)], Role::Number);
                i += 10;
                continue;
            }
            let n = take_number(&chars, i);
            // Numbers buried in a longer Yaml/Shell scalar (model ids like
            // `nemotron-3.5`, dates like `2026-08-24`) stay plain; only a
            // standalone number earns the band.
            let role = if matches!(
                lang,
                Lang::Yaml | Lang::Shell | Lang::Dockerfile | Lang::Makefile | Lang::Nginx
            ) && !number_is_standalone(&chars, i, n)
            {
                Role::Default
            } else {
                Role::Number
            };
            segs.push(&line[b(i)..b(i + n)], role);
            i += n;
            continue;
        }
        // [section] / [[array of tables]] header for Toml/Ini.
        if (lang == Lang::Toml || lang == Lang::Ini) && c == '[' {
            if let Some(pos) = find_char(&chars, i, ']') {
                let mut end = pos + 1;
                if lang == Lang::Toml
                    && i + 1 < chars.len()
                    && chars[i + 1] == '['
                    && end < chars.len()
                    && chars[end] == ']'
                {
                    end += 1;
                }
                segs.push(&line[b(i)..b(end)], Role::Keyword);
                i = end;
                continue;
            }
        }
        // Yaml block-scalar indicator (`|`, `>`, `|-`, `>+2`, …): marks the
        // verbatim text below; colour it like a string opener.
        if lang == Lang::Yaml
            && (c == '|' || c == '>')
            && (i == 0 || chars[i - 1].is_whitespace())
        {
            let mut n = 1;
            if i + n < chars.len() && matches!(chars[i + n], '-' | '+') {
                n += 1;
            }
            while i + n < chars.len() && chars[i + n].is_ascii_digit() {
                n += 1;
            }
            if i + n >= chars.len() || chars[i + n].is_whitespace() {
                segs.push(&line[b(i)..b(i + n)], Role::Str);
                i += n;
                continue;
            }
        }
        // Yaml document markers (only when the cursor char opens one; push the
        // remainder of the line so segments still concatenate to the source).
        if lang == Lang::Yaml && (c == '-' || c == '.') && matches!(line.trim(), "---" | "...") {
            segs.push(&line[b(i)..], Role::Keyword);
            break;
        }
        if c == '(' {
            paren_depth += 1;
        } else if c == ')' {
            paren_depth -= 1;
        }
        segs.word(&line[b(i)..b(i + 1)]);
        i += 1;
    }

    // A heredoc opened on this line starts its body on the NEXT line.
    if carry_out.is_empty() {
        if let Some((delim, indented)) = pending_heredoc {
            carry_out = format!("hd{}:{}", if indented { "-" } else { "" }, delim);
        }
    }
    BuiltLine { source: line.to_string(), carry: carry_out, segments: segs.finish() }
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

/// Tokenise a single line for the bracket matcher: the coloured runs plus the
/// trailing carry state. Same state machine as the overlay, so "inside a
/// string / comment / heredoc" decisions can never diverge from the colours.
pub(crate) fn line_roles(lang: Lang, line: &str, carry_in: &str) -> (Vec<(String, Role)>, String) {
    let built = tokenize(lang, line, carry_in);
    (built.segments, built.carry)
}

/// Build the full overlay model for `content`.
pub fn build_lines(content: &str, lang: Lang, dark: bool) -> Vec<HlLine> {
    let mut carry = String::new();
    content
        .split('\n')
        .map(|line| {
            let built = tokenize(lang, line, &carry);
            let hl = to_hl_line(&built, dark, &[]);
            carry = built.carry;
            hl
        })
        .collect()
}

/// Turn one tokeniser row into the overlay model entry. `box_positions` are
/// byte offsets INSIDE the row whose characters get the bracket-match box
/// (typically 0, 1 or 2 of them).
fn to_hl_line(built: &BuiltLine, dark: bool, box_positions: &[usize]) -> HlLine {
    let mut segments: Vec<HlSeg> = Vec::new();
    let mut pos = 0usize;
    for (text, role) in &built.segments {
        let len = text.len();
        let in_seg: Vec<usize> =
            box_positions.iter().copied().filter(|p| *p >= pos && *p < pos + len).collect();
        if in_seg.is_empty() {
            segments.push(HlSeg { text: text.as_str().into(), color: role_color(*role, dark), boxed: false });
        } else {
            // Split the run so each boxed character gets its own segment.
            let mut cur = 0usize;
            for p in in_seg {
                let rel = p - pos;
                if rel > cur {
                    segments.push(HlSeg {
                        text: (&text[cur..rel]).into(),
                        color: role_color(*role, dark),
                        boxed: false,
                    });
                }
                segments.push(HlSeg {
                    text: (&text[rel..rel + 1]).into(),
                    color: role_color(*role, dark),
                    boxed: true,
                });
                cur = rel + 1;
            }
            if cur < len {
                segments.push(HlSeg {
                    text: (&text[cur..]).into(),
                    color: role_color(*role, dark),
                    boxed: false,
                });
            }
        }
        pos += len;
    }
    HlLine {
        source: built.source.as_str().into(),
        carry: built.carry.as_str().into(),
        segments: ModelRc::new(VecModel::from(segments)),
    }
}

/// Byte position of row `row` inside `content` (rows joined by '\n').
fn row_start(content: &str, row: usize) -> usize {
    let mut start = 0usize;
    for (i, line) in content.split('\n').enumerate() {
        if i == row {
            return start;
        }
        start += line.len() + 1;
    }
    start
}

/// Re-tokenise ONLY the rows touched by the old/new bracket pair, re-building
/// them with the match box baked into the segment runs. Called after every
/// bracket-pair change and after every content re-sync (whose rebuild drops
/// boxes).
pub fn apply_bracket(
    model: &VecModel<HlLine>,
    content: &str,
    lang: Lang,
    dark: bool,
    old: Option<(usize, usize)>,
    new: Option<(usize, usize)>,
) {
    if old == new {
        return;
    }
    let mut dirty = std::collections::BTreeSet::new();
    let rows: Vec<&str> = content.split('\n').collect();
    let mut positions: Vec<usize> = Vec::new();
    if let Some((o, c)) = old {
        positions.extend([o, c]);
    }
    if let Some((o, c)) = new {
        positions.extend([o, c]);
    }
    let mut start = 0usize;
    for (i, line) in rows.iter().enumerate() {
        if positions.iter().any(|p| *p >= start && *p < start + line.len()) {
            dirty.insert(i);
        }
        start += line.len() + 1;
    }
    for row in dirty {
        if row >= model.row_count() {
            continue;
        }
        let carry_in = if row == 0 {
            String::new()
        } else {
            model.row_data(row - 1).map(|p| p.carry.to_string()).unwrap_or_default()
        };
        let built = tokenize(lang, rows[row], &carry_in);
        let mut local: Vec<usize> = Vec::new();
        let rs = row_start(content, row);
        if let Some((o, c)) = new {
            for p in [o, c] {
                if p >= rs && p < rs + rows[row].len() {
                    local.push(p - rs);
                }
            }
        }
        model.set_row_data(row, to_hl_line(&built, dark, &local));
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
    let mut carry = String::new();
    let mut row = 0usize;
    for line in content.split('\n') {
        let old = model.row_data(row);
        let built = match old {
            Some(prev) if prev.source.as_str() == line && prev.carry.as_str() == carry => {
                // Stable row; the deterministic state machine guarantees the
                // rest of the document is untouched.
                row += 1;
                continue;
            }
            _ => tokenize(lang, line, &carry),
        };
        carry = built.carry.clone();
        let built_hl = to_hl_line(&built, dark, &[]);
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

    #[test]
    fn shell_segments() {
        let built = tokenize(Lang::Shell, "if [ -n \"$HOME\" ]; then", "");
        let roles: Vec<Role> = built.segments.iter().map(|(_, r)| *r).collect();
        assert!(roles.contains(&Role::Control)); // if / then
        assert!(roles.contains(&Role::Str)); // "$HOME"
        // $HOME is inside the quoted string → only one string segment.
        assert_eq!(built.segments.iter().filter(|(_, r)| *r == Role::Str).count(), 1);
    }

    #[test]
    fn shell_comment_to_eol() {
        let built = tokenize(Lang::Shell, "export PATH=/bin # set path", "");
        let texts = built.segments.iter().map(|(t, r)| (t.as_str(), *r)).collect::<Vec<_>>();
        let last = texts.last().unwrap();
        assert_eq!(last.1, Role::Comment);
        assert!(last.0.contains("set path"));
    }

    #[test]
    fn shell_variable_outside_string() {
        let built = tokenize(Lang::Shell, "echo $HOME", "");
        assert!(built.segments.iter().any(|(t, r)| t == "$HOME" && *r == Role::Variable));
        let built2 = tokenize(Lang::Shell, "echo ${HOME}/x", "");
        assert!(built2.segments.iter().any(|(t, r)| t == "${HOME}" && *r == Role::Variable));
    }

    #[test]
    fn shell_special_parameters() {
        let built = tokenize(
            Lang::Shell,
            "if [ $# -eq 0 ] || [ $? -ne 1 ] || [ $$ = $! ] || [ $* $@ $- ]; then",
            "",
        );
        for p in ["$#", "$?", "$$", "$!", "$*", "$@", "$-"] {
            assert!(
                built.segments.iter().any(|(t, r)| t == p && *r == Role::Variable),
                "missing {p}"
            );
        }
        // No standalone `$` leaks out as a variable band.
        assert!(!built.segments.iter().any(|(t, r)| t == "$" && *r == Role::Variable));
    }

    #[test]
    fn shell_command_substitution_plain() {
        let src = "monday=$(date -d \"@$monday_epoch\" +%Y-%m-%d)";
        let built = tokenize(Lang::Shell, src, "");
        assert_eq!(
            built.segments.iter().map(|(t, _)| t.as_str()).collect::<String>(),
            src
        );
        // `$(` must not render as a lone builtin dollar.
        assert!(!built.segments.iter().any(|(t, r)| t == "$" && *r == Role::Variable));
        // Coreutils commands are deliberately not coloured (noise).
        assert!(built.segments.iter().any(|(t, r)| t.contains("date") && *r == Role::Default));
        assert!(built
            .segments
            .iter()
            .any(|(t, r)| t == "\"@$monday_epoch\"" && *r == Role::Str));
    }

    #[test]
    fn shell_arithmetic_plain_but_numbers_kept() {
        let built = tokenize(Lang::Shell, "i=$((i + 1))\necho $(( days / 7 + 1 ))", "");
        assert!(!built.segments.iter().any(|(t, r)| t == "$" && *r == Role::Variable));
        assert!(built.segments.iter().any(|(t, r)| t == "1" && *r == Role::Number));
        assert!(built.segments.iter().any(|(t, r)| t == "7" && *r == Role::Number));
        assert!(built.segments.iter().any(|(t, r)| t == "echo" && *r == Role::Builtin));
    }

    #[test]
    fn shell_date_value_stays_plain() {
        let built = tokenize(Lang::Shell, "START_DATE=2026-08-24", "");
        assert!(!built.segments.iter().any(|(_, r)| *r == Role::Number));
    }

    #[test]
    fn shell_assignment_negative_kept_number() {
        // A `-` after `=` reads as a sign, not a hyphen: `OFFSET=-7` keeps
        // its number band while the date above stays plain.
        let built = tokenize(Lang::Shell, "OFFSET=-7", "");
        assert!(built.segments.iter().any(|(t, r)| t == "7" && *r == Role::Number));
    }

    #[test]
    fn shell_substitution_line_roundtrip() {
        let src = "monday_epoch=$(( $(date -d \"$START_DATE\" +%s) + (w - 1) * 7 * 86400 ))";
        let built = tokenize(Lang::Shell, src, "");
        assert_eq!(
            built.segments.iter().map(|(t, _)| t.as_str()).collect::<String>(),
            src
        );
        assert!(!built.segments.iter().any(|(t, r)| t == "$" && *r == Role::Variable));
        // Coreutils commands are deliberately not coloured (noise).
        assert!(built.segments.iter().any(|(t, r)| t.contains("date") && *r == Role::Default));
        assert!(built.segments.iter().any(|(t, r)| t == "\"$START_DATE\"" && *r == Role::Str));
        assert!(built.segments.iter().any(|(t, r)| t == "86400" && *r == Role::Number));
        assert!(built.segments.iter().any(|(t, r)| t == "7" && *r == Role::Number));
    }

    #[test]
    fn json_keys_strings_numbers() {
        let built = tokenize(Lang::Json, r#"{"name": "meatshell", "n": 42, "ok": true}"#, "");
        let joined: String = built.segments.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(joined, r#"{"name": "meatshell", "n": 42, "ok": true}"#);
        // "name" and "ok" are keys (keyword colour); "meatshell" is a value.
        let key_count = built.segments.iter().filter(|(_, r)| *r == Role::Keyword).count();
        let value_count = built.segments.iter().filter(|(_, r)| *r == Role::Str).count();
        assert_eq!(key_count, 3);
        assert_eq!(value_count, 1);
        assert!(built.segments.iter().any(|(t, r)| t == "42" && *r == Role::Number));
        assert!(built.segments.iter().any(|(t, r)| t == "true" && *r == Role::Builtin));
    }

    #[test]
    fn yaml_keys_and_comments() {
        let built = tokenize(Lang::Yaml, "port: 8080 # the port", "");
        assert!(built.segments.iter().any(|(t, r)| t == "port:" && *r == Role::Keyword));
        assert!(built.segments.iter().any(|(t, r)| t == "8080" && *r == Role::Number));
        let last = built.segments.last().unwrap();
        assert_eq!(last.1, Role::Comment);
    }

    #[test]
    fn yaml_hash_inside_word_is_not_comment() {
        // A '#' directly attached to a word (no preceding blank) must not start
        // a comment — e.g. an anchor-less URL fragment.
        let built = tokenize(Lang::Yaml, "url: http://x#a", "");
        assert!(!built.segments.iter().any(|(_, r)| *r == Role::Comment));
    }

    #[test]
    fn yaml_url_value_stays_plain_and_comment_kept() {
        // The scheme must not read as a key and the IP/port must not scatter
        // number bands; a comment after the URL still wins.
        let built = tokenize(Lang::Yaml, "base_url: http://192.168.31.25:13000/v1", "");
        let segs: Vec<(&str, Role)> = built.segments.iter().map(|(t, r)| (t.as_str(), *r)).collect();
        assert!(segs.iter().any(|(t, r)| *t == "base_url:" && *r == Role::Keyword));
        assert!(!segs.iter().any(|(_, r)| *r == Role::Number));
        assert!(!segs.iter().any(|(t, r)| *r == Role::Keyword && t.starts_with("http")));
        assert_eq!(
            built.segments.iter().map(|(t, _)| t.as_str()).collect::<String>(),
            "base_url: http://192.168.31.25:13000/v1"
        );
        let with_comment = tokenize(Lang::Yaml, "url: https://example.com # note", "");
        let last = with_comment.segments.last().unwrap();
        assert_eq!(last.1, Role::Comment);
        assert!(!with_comment
            .segments
            .iter()
            .any(|(t, r)| *r == Role::Number && t.contains("168")));
    }

    #[test]
    fn yaml_hyphenated_key_is_one_keyword() {
        // Bare keys keep hyphens/slashes/version fragments: the whole run up
        // to `:` (whitespace or EOL) is one keyword, not just the last word.
        let built = tokenize(Lang::Yaml, "DeepSeek-V4-Flash-0731-Event: {}", "");
        let segs: Vec<(&str, Role)> = built.segments.iter().map(|(t, r)| (t.as_str(), *r)).collect();
        assert!(segs
            .iter()
            .any(|(t, r)| *t == "DeepSeek-V4-Flash-0731-Event:" && *r == Role::Keyword));
        assert!(!segs.iter().any(|(_, r)| *r == Role::Number));
        assert_eq!(
            built.segments.iter().map(|(t, _)| t.as_str()).collect::<String>(),
            "DeepSeek-V4-Flash-0731-Event: {}"
        );
    }

    #[test]
    fn yaml_model_value_has_no_number_fragments() {
        let built = tokenize(Lang::Yaml, "model: nvidia/nemotron-3.5-lightning-30b-a3b", "");
        let segs: Vec<(&str, Role)> = built.segments.iter().map(|(t, r)| (t.as_str(), *r)).collect();
        assert!(segs.iter().any(|(t, r)| *t == "model:" && *r == Role::Keyword));
        assert!(!segs.iter().any(|(_, r)| *r == Role::Number));
    }

    #[test]
    fn yaml_standalone_numbers_still_coloured() {
        let built = tokenize(Lang::Yaml, "port: 8080\nlist: [1, 2]\npi: 3.14", "");
        let segs: Vec<(&str, Role)> = built.segments.iter().map(|(t, r)| (t.as_str(), *r)).collect();
        assert!(segs.iter().any(|(t, r)| *t == "8080" && *r == Role::Number));
        assert!(segs.iter().any(|(t, r)| *t == "1" && *r == Role::Number));
        assert!(segs.iter().any(|(t, r)| *t == "2" && *r == Role::Number));
        assert!(segs.iter().any(|(t, r)| *t == "3.14" && *r == Role::Number));
        // Multi-dot version-like scalars stay plain (no fragments).
        let v = tokenize(Lang::Yaml, "version: 1.2.3", "");
        assert!(!v.segments.iter().any(|(_, r)| *r == Role::Number));
        // A minus sign reads as a sign, not a hyphen: `-1` still colours.
        let neg = tokenize(Lang::Yaml, "timeout: -1", "");
        assert!(neg.segments.iter().any(|(t, r)| t == "1" && *r == Role::Number));
    }

    #[test]
    fn yaml_compact_flow_key_not_coloured() {
        // Accepted trade: a colon without a following space is no longer a key,
        // so `{a:1}` stays plain while `{a: 1}` and `b: 2` still colour.
        let spaced = tokenize(Lang::Yaml, "labels: {a: 1}", "");
        let segs: Vec<(&str, Role)> =
            spaced.segments.iter().map(|(t, r)| (t.as_str(), *r)).collect();
        assert!(segs.iter().any(|(t, r)| *t == "labels:" && *r == Role::Keyword));
        assert!(segs.iter().any(|(t, r)| *t == "a:" && *r == Role::Keyword));
        let compact = tokenize(Lang::Yaml, "labels: {a:1}", "");
        assert!(!compact
            .segments
            .iter()
            .any(|(t, r)| *r == Role::Keyword && t.starts_with('{')));
    }

    #[test]
    fn ini_sections_and_keys() {
        let sec = tokenize(Lang::Ini, "[Unit]", "");
        assert!(sec.segments.iter().any(|(t, r)| t == "[Unit]" && *r == Role::Keyword));
        let cmt = tokenize(Lang::Ini, "; comment", "");
        assert!(cmt.segments.iter().all(|(_, r)| *r == Role::Comment));
        let key = tokenize(Lang::Ini, "After=network.target", "");
        assert!(key.segments.iter().any(|(t, r)| t.contains("After") && *r == Role::Keyword));
    }

    #[test]
    fn toml_strings_numbers_bools() {
        let built = tokenize(Lang::Toml, "name = \"srv\" \nworkers = 4\nverbose = true", "");
        assert!(built.segments.iter().any(|(t, r)| t == "\"srv\"" && *r == Role::Str));
        assert!(built.segments.iter().any(|(t, r)| t == "4" && *r == Role::Number));
        assert!(built.segments.iter().any(|(t, r)| t == "true" && *r == Role::Builtin));
    }

    #[test]
    fn python_keywords_and_decorators() {
        let built = tokenize(Lang::Python, "@app.route\ndef main():\n    return True", "");
        assert!(built.segments.iter().any(|(t, r)| t.starts_with("@app") && *r == Role::Builtin));
        assert!(built.segments.iter().any(|(t, r)| t == "def" && *r == Role::Keyword));
        assert!(built.segments.iter().any(|(t, r)| t == "return" && *r == Role::Control));
    }

    #[test]
    fn python_triple_quote_spans_lines() {
        let l1 = tokenize(Lang::Python, "x = 1", "");
        assert_eq!(l1.carry, "");
        let l2 = tokenize(Lang::Python, "s = \"\"\"start", "");
        assert_eq!(l2.carry, "py3dq");
        assert!(l2.segments.iter().any(|(t, r)| t.contains("start") && *r == Role::Str));
        let l3 = tokenize(Lang::Python, "still inside \"\" more", "py3dq");
        assert!(l3.segments.iter().all(|(_, r)| *r == Role::Str));
        assert_eq!(l3.carry, "py3dq");
        let l4 = tokenize(Lang::Python, "end\"\"\" x = 2", "py3dq");
        assert_eq!(l4.carry, "");
        assert!(l4.segments.iter().any(|(t, r)| t.contains("x") && *r == Role::Default));
    }

    #[test]
    fn shell_roles_split_control_decl_builtin() {
        let built = tokenize(Lang::Shell, "if export echo; then", "");
        let segs: Vec<(&str, Role)> = built.segments.iter().map(|(t, r)| (t.as_str(), *r)).collect();
        assert!(segs.iter().any(|(t, r)| *t == "if" && *r == Role::Control));
        assert!(segs.iter().any(|(t, r)| *t == "export" && *r == Role::Keyword));
        assert!(segs.iter().any(|(t, r)| *t == "echo" && *r == Role::Builtin));
    }

    #[test]
    fn python_roles_split_control_type_literal() {
        let built = tokenize(Lang::Python, "def f(x):\n    return isinstance(x, str) and True", "");
        let segs: Vec<(&str, Role)> = built.segments.iter().map(|(t, r)| (t.as_str(), *r)).collect();
        assert!(segs.iter().any(|(t, r)| *t == "def" && *r == Role::Keyword));
        assert!(segs.iter().any(|(t, r)| *t == "return" && *r == Role::Control));
        assert!(segs.iter().any(|(t, r)| *t == "isinstance" && *r == Role::Builtin));
        assert!(segs.iter().any(|(t, r)| *t == "str" && *r == Role::Type));
        assert!(segs.iter().any(|(t, r)| *t == "and" && *r == Role::Keyword));
        assert!(segs.iter().any(|(t, r)| *t == "True" && *r == Role::Keyword));
    }

    #[test]
    fn python_self_cls_are_keywords() {
        let built = tokenize(Lang::Python, "def m(self, cls):\n    pass", "");
        assert!(built.segments.iter().any(|(t, r)| t == "self" && *r == Role::Keyword));
        assert!(built.segments.iter().any(|(t, r)| t == "cls" && *r == Role::Keyword));
        assert!(built.segments.iter().any(|(t, r)| t == "pass" && *r == Role::Control));
    }

    #[test]
    fn shell_hash_inside_word_is_literal() {
        let built = tokenize(Lang::Shell, "echo a#b # real", "");
        let segs: Vec<(&str, Role)> = built.segments.iter().map(|(t, r)| (t.as_str(), *r)).collect();
        assert!(segs.iter().any(|&(t, r)| t.contains("a#b") && r == Role::Default));
        assert!(segs.iter().any(|&(t, r)| t == "# real" && r == Role::Comment));
    }

    #[test]
    fn shell_function_definition_name() {
        let built = tokenize(Lang::Shell, "run_it() { echo hi; }", "");
        assert!(built.segments.iter().any(|(t, r)| t == "run_it" && *r == Role::Builtin));
        let plain = tokenize(Lang::Shell, "run_it arg", "");
        assert!(plain.segments.iter().any(|(t, r)| t.contains("run_it") && *r == Role::Default));
    }

    #[test]
    fn python_dotted_decorator_is_one_run() {
        let built = tokenize(Lang::Python, "@app.route(\"/x\")", "");
        assert!(built.segments.iter().any(|(t, r)| t == "@app.route" && *r == Role::Builtin));
    }

    #[test]
    fn python_def_and_class_names() {
        let built = tokenize(Lang::Python, "def make_thing(arg):", "");
        assert!(built.segments.iter().any(|(t, r)| t == "make_thing" && *r == Role::Builtin));
        let built2 = tokenize(Lang::Python, "class ThingBase:", "");
        assert!(built2.segments.iter().any(|(t, r)| t == "ThingBase" && *r == Role::Type));
    }

    #[test]
    fn toml_dotted_key_is_one_keyword() {
        let built = tokenize(Lang::Toml, "server.host = \"1\"  # addr", "");
        let segs: Vec<(&str, Role)> = built.segments.iter().map(|(t, r)| (t.as_str(), *r)).collect();
        assert!(segs.iter().any(|&(t, r)| t == "server.host" && r == Role::Keyword));
    }

    #[test]
    fn toml_array_of_tables_brackets() {
        let built = tokenize(Lang::Toml, "[[bin.hosts]]", "");
        assert!(built.segments.iter().any(|(t, r)| t == "[[bin.hosts]]" && *r == Role::Keyword));
    }

    #[test]
    fn toml_iso_date_is_one_number() {
        let built = tokenize(Lang::Toml, "day = 2026-09-15", "");
        let segs: Vec<(&str, Role)> = built.segments.iter().map(|(t, r)| (t.as_str(), *r)).collect();
        assert!(segs.iter().any(|&(t, r)| t == "2026-09-15" && r == Role::Number));
    }

    #[test]
    fn yaml_block_scalar_indicator() {
        let built = tokenize(Lang::Yaml, "script: |", "");
        assert!(built.segments.iter().any(|(t, r)| t == "|" && *r == Role::Str));
        let built2 = tokenize(Lang::Yaml, "text: >-", "");
        assert!(built2.segments.iter().any(|(t, r)| t == ">-" && *r == Role::Str));
        // Mid-value pipes (regex-ish strings) stay plain.
        let built3 = tokenize(Lang::Yaml, "pattern: a|b", "");
        assert!(!built3.segments.iter().any(|(_, r)| *r == Role::Str));
    }

    #[test]
    fn shell_heredoc_spans_until_delimiter() {
        let opener = tokenize(Lang::Shell, "cat <<EOF", "");
        assert_eq!(opener.carry, "hd:EOF");
        assert!(opener.segments.iter().any(|(t, r)| t == "<<EOF" && *r == Role::Str));
        let body = tokenize(Lang::Shell, "hello $USER", "hd:EOF");
        assert!(body.segments.iter().all(|(_, r)| *r == Role::Str));
        assert_eq!(body.carry, "hd:EOF");
        let closer = tokenize(Lang::Shell, "EOF", "hd:EOF");
        assert_eq!(closer.carry, "");
        // Keyword inside the body must not colour: $USER is literal text.
        assert!(!body.segments.iter().any(|(_, r)| *r == Role::Variable));
    }

    #[test]
    fn shell_herestring_not_heredoc() {
        // `<<<` is a here-string: no cross-line state opens.
        let built = tokenize(Lang::Shell, "grep x <<< 'one two'", "");
        assert_eq!(built.carry, "");
    }

    #[test]
    fn shell_shift_operator_is_not_a_heredoc() {
        let built = tokenize(Lang::Shell, "m=$((1 << 8))", "");
        assert_eq!(built.carry, "");
        let built2 = tokenize(Lang::Shell, "echo $(expr 5 '2' << 3)", "");
        assert_eq!(built2.carry, "");
        // A real heredoc still opens at depth 0.
        assert_eq!(tokenize(Lang::Shell, "cat <<EOF", "").carry, "hd:EOF");
    }

    #[test]
    fn shell_heredoc_quoted_delimiter_consumes_both_quotes() {
        let built = tokenize(Lang::Shell, "cat <<\'CFG\' > /tmp/x", "");
        assert_eq!(built.carry, "hd:CFG");
        let joined: String = built.segments.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(joined, "cat <<'CFG' > /tmp/x");
        assert!(built.segments.iter().any(|(t, r)| t == "<<'CFG'" && *r == Role::Str));
        // The redirect tail stays plain, not swallowed as an open string.
        let tail: Vec<&String> = built
            .segments
            .iter()
            .filter(|(_, r)| *r == Role::Str)
            .map(|(t, _)| t)
            .collect();
        assert_eq!(tail.len(), 1);
    }

    #[test]
    fn shell_heredoc_dash_and_quoted_delimiter() {
        let opener = tokenize(Lang::Shell, "cat <<-'CFG' <<REST", "");
        assert_eq!(opener.carry, "hd-:CFG"); // first opener wins
        assert_eq!(tokenize(Lang::Shell, "\t\tCFG", "hd-:CFG").carry, "");
        assert_eq!(tokenize(Lang::Shell, "CFG", "hd:CFG").carry, "");
        assert_eq!(tokenize(Lang::Shell, "\tCFG", "hd:CFG").carry, "hd:CFG"); // not indented form
        // Delimiters are case-sensitive.
        assert_eq!(tokenize(Lang::Shell, "eof", "hd:EOF").carry, "hd:EOF");
    }

    #[test]
    fn toml_multiline_string_spans_lines() {
        let opener = tokenize(Lang::Toml, "note = \"\"\"first", "");
        assert_eq!(opener.carry, "tl3dq");
        let body = tokenize(Lang::Toml, "still note = 1", "tl3dq");
        assert!(body.segments.iter().all(|(_, r)| *r == Role::Str));
        let closer = tokenize(Lang::Toml, "done\"\"\"", "tl3dq");
        assert_eq!(closer.carry, "");
    }

    #[test]
    fn build_lines_chains_carry() {
        let doc = "cat <<EOF\nkey: value\nEOF\nafter: 1";
        let lines = build_lines(doc, Lang::Shell, true);
        assert_eq!(lines[1].carry, "hd:EOF");
        // Inside the heredoc a Yaml-looking line stays a string band.
        assert!(lines[1].segments.iter().all(|seg| seg.color.to_argb_u8().alpha > 0));
        assert_eq!(lines[3].carry, "");
    }

    #[test]
    fn dockerfile_instructions_and_args() {
        let built = tokenize(Lang::Dockerfile, "FROM ubuntu:24.04 AS base", "");
        let segs: Vec<(&str, Role)> = built.segments.iter().map(|t| (t.0.as_str(), t.1)).collect();
        assert!(segs.iter().any(|&(t, r)| t == "FROM" && r == Role::Keyword));
        let run = tokenize(Lang::Dockerfile, "RUN apt-get install $PKG \'x\'", "");
        assert!(run.segments.iter().any(|(t, r)| t == "RUN" && *r == Role::Keyword));
        assert!(run.segments.iter().any(|(t, r)| t == "$PKG" && *r == Role::Variable));
        // Lower-case word at start is not an instruction.
        let note = tokenize(Lang::Dockerfile, "# build stage two", "");
        assert!(note.segments.iter().all(|(_, r)| *r == Role::Comment));
    }

    #[test]
    fn makefile_targets_vars_and_recipes() {
        let built = tokenize(Lang::Makefile, "build: $(SRC) main.o", "");
        let segs: Vec<(&str, Role)> = built.segments.iter().map(|t| (t.0.as_str(), t.1)).collect();
        assert!(segs.iter().any(|&(t, r)| t == "build" && r == Role::Builtin));
        assert!(segs.iter().any(|&(t, r)| t == "$(SRC)" && r == Role::Variable));
        // Recipe lines (tab-indented) are not targets.
        let recipe = tokenize(Lang::Makefile, "\tgcc -o app main.c", "");
        assert!(!recipe.segments.iter().any(|(_, r)| *r == Role::Builtin));
        // `:=` assignment colons must not trigger target mode.
        let asg = tokenize(Lang::Makefile, "CC := gcc", "");
        assert!(!asg.segments.iter().any(|(_, r)| *r == Role::Builtin));
        let phony = tokenize(Lang::Makefile, ".PHONY: build", "");
        assert!(phony.segments.iter().any(|(t, r)| t == ".PHONY" && *r == Role::Builtin));
    }

    #[test]
    fn nginx_directives_and_comments() {
        let built = tokenize(Lang::Nginx, "    listen 80;", "");
        let segs: Vec<(&str, Role)> = built.segments.iter().map(|t| (t.0.as_str(), t.1)).collect();
        assert!(segs.iter().any(|&(t, r)| t == "listen" && r == Role::Keyword));
        assert!(segs.iter().any(|&(t, r)| t == "80" && r == Role::Number));
        let server = tokenize(Lang::Nginx, "server {", "");
        assert!(server.segments.iter().any(|(t, r)| t == "server" && *r == Role::Keyword));
        let cmt = tokenize(Lang::Nginx, "# upstream note", "");
        assert!(cmt.segments.iter().all(|(_, r)| *r == Role::Comment));
    }

    #[test]
    fn apply_bracket_boxes_only_touched_rows() {
        let content = "echo (hi)\nx = 1";
        let model = VecModel::from(build_lines(content, Lang::Shell, true));
        apply_bracket(&model, content, Lang::Shell, true, None, Some((5, 8)));
        let row0: Vec<HlSeg> = model.row_data(0).unwrap().segments.iter().collect();
        assert!(row0.iter().any(|sg| sg.text.as_str() == "(" && sg.boxed));
        assert!(row0.iter().any(|sg| sg.text.as_str() == ")" && sg.boxed));
        let joined: String = row0.iter().map(|sg| sg.text.as_str()).collect();
        assert_eq!(joined, "echo (hi)");
        let row1: Vec<HlSeg> = model.row_data(1).unwrap().segments.iter().collect();
        assert!(!row1.iter().any(|sg| sg.boxed));
        // Moving the pair away clears the old boxes.
        apply_bracket(&model, content, Lang::Shell, true, Some((5, 8)), None);
        let row0: Vec<HlSeg> = model.row_data(0).unwrap().segments.iter().collect();
        assert!(!row0.iter().any(|sg| sg.boxed));
    }

    #[test]
    fn apply_bracket_cjk_row_offsets() {
        let content = "echo 中文(hi)";
        let model = VecModel::from(build_lines(content, Lang::Shell, true));
        apply_bracket(&model, content, Lang::Shell, true, None, Some((11, 14)));
        let row: Vec<HlSeg> = model.row_data(0).unwrap().segments.iter().collect();
        assert!(row.iter().any(|sg| sg.text.as_str() == "(" && sg.boxed));
        assert!(row.iter().any(|sg| sg.text.as_str() == ")" && sg.boxed));
        let joined: String = row.iter().map(|sg| sg.text.as_str()).collect();
        assert_eq!(joined, content);
    }

    #[test]
    fn plain_language_yields_default_only() {
        let built = tokenize(Lang::Plain, "anything # no highlighting", "");
        assert!(built.segments.iter().all(|(_, r)| *r == Role::Default));
    }

    #[test]
    fn adjacent_same_role_runs_merge() {
        let built = tokenize(Lang::Shell, "aaa bbb", "");
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
    fn default_segments_are_bandless_and_roles_are_tinted() {
        // A whitespace-only row still yields Default segments (they carry the
        // x advance for later runs) but must not paint a band: that was the
        // "empty line renders coloured" bug.
        let lines = build_lines("   \n# c\n", Lang::Shell, true);
        let ws = &lines[0];
        assert_eq!(ws.segments.row_count(), 1);
        assert_eq!(ws.segments.row_data(0).unwrap().color.to_argb_u8().alpha, 0);
        let comment = &lines[1];
        assert_eq!(comment.segments.row_count(), 1);
        let band = comment.segments.row_data(0).unwrap().color.to_argb_u8();
        assert_eq!(band.alpha, 66); // the former .with-alpha(0.26) in the UI
        assert_eq!((band.red, band.green, band.blue), (0x6a, 0x99, 0x55));
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
        // simulate by checking row identity through `carry`/`source` stability
        // — sync_model walks but skips set_row_data for stable rows.
        let content = "head\n".to_string() + &"body line\n".repeat(500);
        let model = VecModel::from(build_lines(&content, Lang::Json, true));
        let edited = "HEAD\n".to_owned() + &"body line\n".repeat(500);
        sync_model(&model, &edited, Lang::Json, true);
        assert_eq!(model.row_count(), 502);
        assert_eq!(model.row_data(0).unwrap().source, "HEAD");
        // Row 501 stays the (empty) trailing line.
        assert_eq!(model.row_data(501).unwrap().source, "");
    }

    #[test]
    fn model_rc_roundtrip() {
        let rc = model_rc(build_lines("x", Lang::Shell, true));
        assert!(rc.as_any().downcast_ref::<VecModel<HlLine>>().is_some());
    }

    #[test]
    fn cjk_lines_keep_byte_offsets_aligned() {
        // Wide characters must not corrupt slicing (byte vs char indices).
        let built = tokenize(Lang::Python, "s = \"中文注释\"  # 说明", "");
        let joined = built.segments.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>().join("");
        assert_eq!(joined, "s = \"中文注释\"  # 说明");
        let comment = built.segments.last().unwrap();
        assert_eq!(comment.1, Role::Comment);
    }

    #[test]
    fn overly_long_line_degrades_to_single_plain_run() {
        let line = "x".repeat(MAX_TOKENIZE_CHARS + 1);
        let built = tokenize(Lang::Json, &line, "");
        assert_eq!(built.segments.len(), 1);
        assert_eq!(built.segments[0].1, Role::Default);
        assert_eq!(built.segments[0].0, line);
        assert_eq!(built.carry, "");
    }

    #[test]
    fn lang_detect_via_public_helper() {
        assert_eq!(lang::detect("x.py", ""), Lang::Python);
    }
}
