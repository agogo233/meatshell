use super::*;
use std::cmp::Ordering;
use std::fmt::Write;
use std::sync::OnceLock;

pub(super) fn language(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" => "Rust",
        "py" => "Python",
        "sh" | "bash" | "zsh" | "fish" | "ksh" | "csh" | "tcsh" | "command" => "Shell",
        "json" => "JSON",
        "yaml" | "yml" => "YAML",
        "toml" => "TOML",
        "js" | "ts" | "jsx" | "tsx" => "JavaScript",
        "c" | "h" | "cpp" | "hpp" => "C/C++",
        _ => "Text",
    }
}

// Dark-theme palette. `themed_spans` maps these to the light palette when the
// window is light; `spans_for` picks the bracket colour directly per theme.
const COMMENT: &str = "ff6a9955";
const STR: &str = "ffce9178";
const KEY: &str = "ff9cdcfe";
const NUMBER: &str = "ffb5cea8";
const BOOL: &str = "ff569cd6";
const KEYWORD: &str = "ffc586c0";
const BRACKET_DARK: &str = "ffffd700";
const BRACKET_LIGHT: &str = "ffb1590b";

/// The lexical class of a base token, used both to colour the text and to
/// decide whether a bracket or a heredoc / block-scalar marker is structural
/// or inside a string / comment.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Comment,
    Str,
    Other,
}

/// Lexical colors only: offsets refer to original UTF-8 text and never change
/// fonts or whitespace. The native renderer owns shaping, selection and IME.
pub(super) fn highlight(text: &str, path: &str) -> String {
    encode(&colorize(text, path))
}

/// Syntax spans for `text`, recoloured for the light theme when needed.
pub(super) fn themed_spans(text: &str, path: &str, dark: bool) -> String {
    let spans = colorize(text, path);
    let spans: Vec<(usize, usize, &str)> = if dark {
        spans
    } else {
        spans
            .into_iter()
            .map(|(a, b, c)| (a, b, lighten(c)))
            .collect()
    };
    encode(&spans)
}

/// Highlight spans plus, when `pair` is `Some((open, close))`, a bracket-pair
/// span (1 byte each, the pair the caret sits on) in a distinct colour. The
/// pair is merged in ascending order and the output is monotonic, which is the
/// contract the vendored renderer's `parse_syntax_spans` enforces.
pub(super) fn spans_for(
    text: &str,
    path: &str,
    dark: bool,
    pair: Option<(usize, usize)>,
) -> String {
    let mut spans = colorize(text, path);
    if let Some((open, close)) = pair {
        let colour = if dark { BRACKET_DARK } else { BRACKET_LIGHT };
        spans.push((open, open + 1, colour));
        spans.push((close, close + 1, colour));
        spans.sort_by_key(|s| s.0);
    }
    let spans: Vec<(usize, usize, &str)> = if dark {
        spans
    } else {
        spans
            .into_iter()
            .map(|(a, b, c)| (a, b, lighten(c)))
            .collect()
    };
    encode(&spans)
}

fn encode(spans: &[(usize, usize, &str)]) -> String {
    let mut result = String::new();
    let mut previous_end = 0;
    for &(start, end, color) in spans {
        // Defensive: never emit overlapping spans; the vendored renderer drops
        // any span whose start is before the previous end.
        if start < previous_end || start >= end {
            continue;
        }
        let _ = write!(result, "{start}:{end}:{color};");
        previous_end = end;
    }
    result
}

fn lighten(color: &str) -> &str {
    match color {
        COMMENT => "ff38761d",
        STR => "ffa31515",
        KEY => "ff0451a5",
        NUMBER => "ff098658",
        BOOL => "ff0000ff",
        KEYWORD => "ff800080",
        BRACKET_DARK => BRACKET_LIGHT,
        other => other,
    }
}

/// Break `text` into coloured spans for `path`. `Text` files and very large
/// files are left uncoloured, matching the upstream behaviour.
fn colorize(text: &str, path: &str) -> Vec<(usize, usize, &'static str)> {
    let language = language(path);
    if language == "Text" || text.len() > 512 * 1024 {
        return Vec::new();
    }
    let tokens = base_tokens(text, language);
    let bodies = cross_line_bodies(text, language, &tokens);
    let keywords = keyword_set(language);
    let mut out: Vec<(usize, usize, &'static str)> = Vec::new();
    for &(start, end, kind) in &tokens {
        // A token fully inside a heredoc / block-scalar body is part of that
        // string and is emitted as the body span instead.
        if bodies.iter().any(|&(bs, be)| start >= bs && end <= be) {
            continue;
        }
        let value = &text[start..end];
        let colour = match kind {
            Kind::Comment => COMMENT,
            Kind::Str => STR,
            Kind::Other => {
                let is_other = if value.as_bytes()[0].is_ascii_digit() {
                    NUMBER
                } else if matches!(
                    value,
                    "true" | "false" | "null" | "None" | "True" | "False" | "nil"
                ) {
                    BOOL
                } else if keywords
                    .split_ascii_whitespace()
                    .any(|keyword| keyword == value)
                {
                    KEYWORD
                } else if matches!(language, "JSON" | "YAML" | "TOML")
                    && is_line_start_key(text, language, start, end)
                {
                    KEY
                } else {
                    continue;
                };
                is_other
            }
        };
        out.push((start, end, colour));
    }
    for (start, end) in bodies {
        // The body is one whole string: no token inside it is coloured.
        out.push((start, end, STR));
    }
    out.sort_by_key(|s| s.0);
    out
}

/// True when the token at `start..end` is a mapping key in JSON / YAML / TOML.
/// Narrowed to line starts and flow-collection prefixes so a bare URL value
/// (`url: http://…`) is not mistaken for a key — the scheme sits after a
/// colon, not after a line start / `-` / `{` / `[` / `,`.
fn is_line_start_key(text: &str, language: &str, start: usize, end: usize) -> bool {
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let prefix = text[line_start..start].trim();
    let prefix_ok = prefix.is_empty()
        || matches!(
            prefix.chars().last(),
            Some('-') | Some('{') | Some('[') | Some(',')
        );
    if !prefix_ok {
        return false;
    }
    let line_end = text[end..].find('\n').map_or(text.len(), |i| end + i);
    let rest = text[end..line_end].trim_start_matches([' ', '\t']);
    rest.starts_with(if language == "TOML" { '=' } else { ':' })
}

fn keyword_set(language: &str) -> &'static str {
    match language {
        "Rust" => "fn let mut pub struct enum impl trait use mod const static match if else for while loop in return break continue async await move ref self Self where as unsafe dyn type crate super extern union",
        "Python" => "def class import from try except finally with lambda yield pass raise and or not if elif else for while in return break continue async await as is global nonlocal del assert",
        "Shell" => "if then elif else fi for while until in do done case esac function local export readonly return break continue select time coproc getopts shift unset source alias unalias test echo printf cd pwd read mapfile declare typeset let eval exec exit trap wait kill",
        "JavaScript" => "let const var function class extends if else for while do in of return break continue async await new switch case default throw try catch finally export import from interface type typeof instanceof void delete this super implements public private protected",
        "C/C++" => "struct enum class namespace typedef using template typename const static extern volatile if else for while do return break continue switch case default try catch throw new delete public private protected virtual override auto void int char float double bool unsigned signed long short sizeof nullptr",
        _ => "",
    }
}

/// Base lexical tokens: strings, comments and the other runs the regex can
/// see. `Kind::Str` / `Kind::Comment` mark the regions that never count as
/// bracket structure and that heredoc / block-scalar markers cannot live in.
fn base_tokens(text: &str, language: &str) -> Vec<(usize, usize, Kind)> {
    static TOKENS: OnceLock<[regex::Regex; 3]> = OnceLock::new();
    let tokens = TOKENS.get_or_init(|| {
        let common = r#"(?s:""".*?(?:"""|$)|'''.*?(?:'''|$))|"(?:\\.|[^"\\\r\n])*"|'(?:\\.|[^'\\\r\n])*'|`(?:\\.|[^`\\\r\n])*`|\b(?:0[xX][0-9a-fA-F]+|[0-9]+(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?)\b|\b[A-Za-z_][A-Za-z_0-9]*\b"#;
        [common.to_string(), format!(r"\#[^\r\n]*|{common}"),
            format!(r"(?s:/\*.*?(?:\*/|$))|//[^\r\n]*|{common}")]
            .map(|pattern| regex::Regex::new(&pattern).expect("editor token regex"))
    });
    let hash_comments = matches!(language, "Python" | "Shell" | "YAML" | "TOML");
    let slash_comments = matches!(language, "Rust" | "JavaScript" | "C/C++");
    let tokens = &tokens[if hash_comments {
        1
    } else if slash_comments {
        2
    } else {
        0
    }];
    tokens
        .find_iter(text)
        .map(|token| {
            let value = token.as_str();
            let kind = if (hash_comments && value.starts_with('#'))
                || (slash_comments && (value.starts_with("//") || value.starts_with("/*")))
            {
                Kind::Comment
            } else if value.starts_with('"') || value.starts_with('\'') || value.starts_with('`') {
                Kind::Str
            } else {
                Kind::Other
            };
            (token.start(), token.end(), kind)
        })
        .collect()
}

/// Ranges that are whole string bodies spanning lines: Shell heredocs and
/// YAML block scalars. Their markers must sit outside strings / comments.
fn cross_line_bodies(
    text: &str,
    language: &str,
    tokens: &[(usize, usize, Kind)],
) -> Vec<(usize, usize)> {
    let cover = covered_ranges(tokens);
    let mut bodies = Vec::new();
    if language == "Shell" {
        for marker in heredoc_markers(text) {
            if !in_cover(&cover, marker.pos) {
                bodies.push(heredoc_body(text, marker));
            }
        }
    } else if language == "YAML" {
        for marker in block_scalar_markers(text) {
            if !in_cover(&cover, marker.pos) {
                if let Some(body) = block_scalar_body(text, marker.line_start) {
                    bodies.push(body);
                }
            }
        }
    }
    bodies
}

/// Union of the string / comment token ranges, used to test whether a marker
/// or a bracket sits inside a string or comment.
fn covered_ranges(tokens: &[(usize, usize, Kind)]) -> Vec<(usize, usize)> {
    let mut cover: Vec<(usize, usize)> = tokens
        .iter()
        .filter(|t| t.2 != Kind::Other)
        .map(|t| (t.0, t.1))
        .collect();
    cover.sort_by_key(|r| r.0);
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (a, b) in cover {
        if let Some(last) = merged.last_mut() {
            if a <= last.1 {
                last.1 = last.1.max(b);
                continue;
            }
        }
        merged.push((a, b));
    }
    merged
}

fn in_cover(cover: &[(usize, usize)], pos: usize) -> bool {
    match cover.binary_search_by(|r| {
        if r.1 <= pos {
            Ordering::Less
        } else if r.0 > pos {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    }) {
        Ok(_) => true,
        Err(_) => false,
    }
}

struct HeredocMarker {
    pos: usize,
    delimiter: String,
    hyphen: bool,
}

fn heredoc_markers(text: &str) -> Vec<HeredocMarker> {
    // Three alternative delimiter groups instead of a backreference: the
    // regex crate does not support backreferences.
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r#"<<-?[ \t]*(?:'([A-Za-z_][A-Za-z0-9_]*)'|"([A-Za-z_][A-Za-z0-9_]*)"|([A-Za-z_][A-Za-z0-9_]*))"#,
        )
        .expect("heredoc marker regex")
    });
    re.find_iter(text)
        .map(|m| {
            let caps = re.captures(m.as_str()).expect("heredoc captures");
            let raw = caps
                .get(1)
                .or_else(|| caps.get(2))
                .or_else(|| caps.get(3))
                .expect("heredoc delimiter")
                .as_str();
            let hyphen = m.as_str().starts_with("<<-");
            HeredocMarker {
                pos: m.start(),
                delimiter: raw.to_string(),
                hyphen,
            }
        })
        .collect()
}

/// The heredoc body: from just after the marker's line through the delimiter
/// line (inclusive), or to the end of the text when the delimiter never comes.
fn heredoc_body(text: &str, marker: HeredocMarker) -> (usize, usize) {
    let after = text[marker.pos..]
        .find('\n')
        .map_or(text.len(), |i| marker.pos + i + 1);
    let mut end = text.len();
    for (start, line) in lines(text).filter(|&(start, _)| start >= after) {
        let trimmed = if marker.hyphen {
            line.trim_start_matches('\t')
        } else {
            line
        };
        if trimmed.trim_end() == marker.delimiter {
            end = start + line.len();
            break;
        }
    }
    (after, end)
}

struct BlockScalarMarker {
    pos: usize,
    line_start: usize,
}

fn block_scalar_markers(text: &str) -> Vec<BlockScalarMarker> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?m)^.*:\s*([|>])[-+]?[ \t]*(?:#.*)?$").expect("block scalar regex")
    });
    re.find_iter(text)
        .filter_map(|m| {
            let caps = re.captures(m.as_str())?;
            let marker = caps.get(1)?;
            Some(BlockScalarMarker {
                pos: m.start() + marker.start(),
                line_start: m.start(),
            })
        })
        .collect()
}

/// The block scalar body: from the first non-empty content line through the
/// last line indented at least as much, per the YAML indentation rule.
fn block_scalar_body(text: &str, marker_line_start: usize) -> Option<(usize, usize)> {
    let mut base_indent: Option<usize> = None;
    let mut start: Option<usize> = None;
    for (line_start, line) in lines(text).filter(|&(s, _)| s > marker_line_start) {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
        match base_indent {
            None => {
                base_indent = Some(indent);
                start = Some(line_start);
            }
            Some(base) if indent < base => {
                return start.map(|s| (s, line_start));
            }
            Some(_) => {}
        }
    }
    start.map(|s| (s, text.len()))
}

/// Iterate lines with their start offset; each `line` has no trailing newline.
fn lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.split_inclusive('\n').scan(0usize, |base, chunk| {
        let start = *base;
        let line = chunk.strip_suffix('\n').unwrap_or(chunk);
        *base += chunk.len();
        Some((start, line))
    })
}

/// The bracket position the caret acts on: the character directly left of the
/// caret wins (mirrors the moment right after typing a bracket), otherwise the
/// character under it.
fn target(content: &str, caret: usize) -> Option<usize> {
    let left = caret
        .checked_sub(1)
        .filter(|&i| content.is_char_boundary(i) && is_bracket(content.as_bytes()[i]));
    left.or_else(|| {
        (caret < content.len()
            && content.is_char_boundary(caret)
            && is_bracket(content.as_bytes()[caret]))
        .then_some(caret)
    })
}

const OPEN: &[u8] = b"({[";
const CLOSE: &[u8] = b")]}";

fn is_bracket(b: u8) -> bool {
    OPEN.contains(&b) || CLOSE.contains(&b)
}

fn matching(open: u8, close: u8) -> bool {
    matches!((open, close), (b'(', b')') | (b'{', b'}') | (b'[', b']'))
}

/// Byte offsets `(open, close)` of the bracket pair the caret sits on, or
/// `None` when the caret is not next to a bracket, has no partner, or the
/// file is not a highlightable language. Brackets inside strings, comments,
/// heredoc bodies and YAML block scalars never participate.
pub(super) fn bracket_pair(content: &str, caret: i32, path: &str) -> Option<(usize, usize)> {
    let caret = caret.max(0) as usize;
    if caret > content.len() {
        return None;
    }
    // O(1) fast path: the caret is usually not next to a bracket.
    let t = target(content, caret)?;
    let language = language(path);
    if language == "Text" || content.len() > 512 * 1024 {
        return None;
    }
    let tokens = base_tokens(content, language);
    let bodies = cross_line_bodies(content, language, &tokens);
    let mut cover = covered_ranges(&tokens);
    cover.extend(bodies);
    cover.sort_by_key(|r| r.0);
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (a, b) in cover {
        if let Some(last) = merged.last_mut() {
            if a <= last.1 {
                last.1 = last.1.max(b);
                continue;
            }
        }
        merged.push((a, b));
    }
    let mut stack: Vec<(u8, usize)> = Vec::new();
    let mut ci = 0;
    for (pos, ch) in content.bytes().enumerate() {
        while ci < merged.len() && merged[ci].1 <= pos {
            ci += 1;
        }
        if ci < merged.len() && merged[ci].0 <= pos && pos < merged[ci].1 {
            continue;
        }
        if OPEN.contains(&ch) {
            stack.push((ch, pos));
        } else if CLOSE.contains(&ch) {
            if pos == t && stack.is_empty() {
                return None;
            }
            if let Some((ob, opos)) = stack.pop() {
                if matching(ob, ch) && (opos == t || pos == t) {
                    return Some((opos.min(pos), opos.max(pos)));
                }
                if !matching(ob, ch) && (opos == t || pos == t) {
                    return None;
                }
            }
        }
    }
    None
}

/// Refresh the detached editor window's language and syntax colours.
pub(super) fn refresh(window: &EditorWindow, text: &str) {
    let path = window.get_editor_path();
    window.set_editor_language(language(path.as_str()).into());
    window
        .set_editor_syntax_spans(themed_spans(text, path.as_str(), window.get_dark_mode()).into());
    window.set_editor_syntax_source(text.into());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(text: &str, path: &str) -> Vec<(usize, usize, String)> {
        highlight(text, path)
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|entry| {
                let mut f = entry.split(':');
                let a = f.next().unwrap().parse().unwrap();
                let b = f.next().unwrap().parse().unwrap();
                (a, b, f.next().unwrap().to_string())
            })
            .collect()
    }

    fn covering(text: &str, path: &str, needle: &str) -> Option<(usize, usize, String)> {
        let pos = text.find(needle)?;
        spans(text, path)
            .into_iter()
            .find(|(a, b, _)| *a <= pos && pos < *b)
    }

    #[test]
    fn spans_preserve_unicode_and_do_not_color_keywords_in_strings() {
        let text = "let 名称 = \"if 中文\"; // return\n";
        let result = highlight(text, "test.rs");
        let entries: Vec<_> = result.split(';').filter(|s| !s.is_empty()).collect();
        assert_eq!(entries.len(), 3);
        for entry in entries {
            let parts: Vec<_> = entry.split(':').collect();
            let start: usize = parts[0].parse().unwrap();
            let end: usize = parts[1].parse().unwrap();
            assert!(text.get(start..end).is_some());
        }
    }

    #[test]
    fn plain_text_has_no_highlighting() {
        assert!(highlight("let x = 123", "notes.txt").is_empty());
        assert!(highlight("# comment\nvalue = 123", "config.toml").contains("ff6a9955"));
    }

    #[test]
    fn shell_files_highlight_keywords_strings_comments_and_numbers() {
        let result = highlight(
            "#!/bin/sh\nif [ \"$1\" = \"ok\" ]; then\n  echo 42\nfi\n",
            "deploy.sh",
        );
        assert!(result.contains("ff6a9955"));
        assert!(result.contains("ffce9178"));
        assert!(result.contains("ffc586c0"));
        assert!(result.contains("ffb5cea8"));
    }

    #[test]
    fn unterminated_shell_quote_does_not_color_following_lines() {
        let text = "sudo bash -c '\necho plain\necho \"quoted\"\n";
        for entry in highlight(text, "deploy.sh")
            .split(';')
            .filter(|entry| !entry.is_empty())
        {
            let mut fields = entry.split(':');
            let start: usize = fields.next().unwrap().parse().unwrap();
            let end: usize = fields.next().unwrap().parse().unwrap();
            let value = &text[start..end];
            assert!(
                !value.contains('\n'),
                "span crossed a line boundary: {value:?}"
            );
        }
    }

    #[test]
    fn strings_containing_r_or_n_are_lexed_as_strings() {
        for text in [
            r#""error""#,
            r#""name""#,
            r#""print""#,
            r#"'error'"#,
            r#"`error`"#,
        ] {
            let tokens = base_tokens(text, "Rust");
            assert_eq!(tokens.len(), 1, "expected one token in {text}");
            assert!(tokens[0].2 == Kind::Str, "{text} should be a string");
            assert_eq!(tokens[0].0, 0);
            assert_eq!(tokens[0].1, text.len());
        }
    }

    #[test]
    fn unterminated_quote_does_not_span_lines() {
        let text = "echo \"oops\ncall(arg)\n";
        for entry in highlight(text, "deploy.sh")
            .split(';')
            .filter(|entry| !entry.is_empty())
        {
            let mut fields = entry.split(':');
            let start: usize = fields.next().unwrap().parse().unwrap();
            let end: usize = fields.next().unwrap().parse().unwrap();
            let value = &text[start..end];
            assert!(
                !value.contains('\n'),
                "span crossed a line boundary: {value:?}"
            );
        }
    }

    #[test]
    fn string_content_is_not_keyword_colored() {
        let text = "let msg = \"return\";\n";
        let hit = covering(text, "x.rs", "return").expect("the string should be coloured");
        assert!(
            hit.2 == STR,
            "the string content should not be keyword-coloured"
        );
    }

    #[test]
    fn heredoc_body_is_not_keyword_colored() {
        let text = "cat <<EOF\nif true; then\nEOF\n";
        let result = highlight(text, "deploy.sh");
        assert!(
            !result.contains("ffc586c0"),
            "keyword inside heredoc: {result}"
        );
        assert!(
            result.contains("ffce9178"),
            "heredoc body is a string: {result}"
        );
    }

    #[test]
    fn heredoc_marker_inside_a_string_does_not_suppress() {
        let text = "echo \"<<EOF\"\nif true; then\n";
        assert!(highlight(text, "deploy.sh").contains("ffc586c0"));
    }

    #[test]
    fn yaml_block_scalar_content_is_not_key_colored() {
        let text = "key: |\n  nested: val\nnext: 1\n";
        let hit = covering(text, "config.yaml", "nested");
        let (_, _, colour) = hit.expect("nested should be covered");
        assert_eq!(colour, STR);
        assert!(covering(text, "config.yaml", "key").is_some());
        assert!(covering(text, "config.yaml", "next").is_some());
    }

    #[test]
    fn bare_url_scheme_is_not_a_key() {
        let text = "url: http://example.com\n";
        assert!(covering(text, "config.yaml", "http").is_none());
        assert!(covering(text, "config.yaml", "url").is_some());
    }

    #[test]
    fn line_start_keys_are_still_colored() {
        let text = "a: 1\n- b: 2\n";
        assert!(covering(text, "config.yaml", "a").is_some());
        assert!(covering(text, "config.yaml", "b").is_some());
    }

    fn pair(text: &str, caret: i32) -> Option<(usize, usize)> {
        bracket_pair(text, caret, "x.sh")
    }

    #[test]
    fn adjacent_brackets_match_both_sides() {
        assert_eq!(pair("echo $(date)", 6), Some((6, 11)));
        assert_eq!(pair("echo $(date)", 11), Some((6, 11)));
    }

    #[test]
    fn nested_prefers_the_tightest_partner() {
        let text = "f((g(x)))";
        assert_eq!(pair(text, 2), Some((1, 8)));
        assert_eq!(pair(text, 3), Some((2, 7)));
        assert_eq!(pair(text, 5), Some((4, 6)));
    }

    #[test]
    fn unmatched_bracket_has_no_pair() {
        assert_eq!(pair("a ( b", 2), None);
        assert_eq!(pair("a ) b", 4), None);
    }

    #[test]
    fn cjk_offsets_are_respected() {
        let text = "echo 中文(x)";
        assert_eq!(pair(text, 11), Some((11, 13)));
        assert_eq!(pair(text, 12), Some((11, 13)));
    }

    #[test]
    fn brackets_in_strings_and_comments_are_skipped() {
        let text = "x = \"{\"\ny = \"}\"";
        assert_eq!(bracket_pair(text, 4, "x.py"), None);
        assert_eq!(pair("a(b)", 1), Some((1, 3)));
    }

    #[test]
    fn brackets_in_strings_with_r_or_n_are_skipped() {
        let text = "call(\"r)\");\n";
        let open = text.find('(').expect("the call should be there");
        let close = text.rfind(')').expect("the call should be there");
        assert_eq!(
            bracket_pair(text, close as i32, "x.rs"),
            Some((open, close))
        );
    }

    #[test]
    fn brackets_after_an_unterminated_quote_still_pair() {
        let text = "echo \"oops\ncall(arg)\n";
        let open = text.find("(arg)").expect("the call should be there");
        assert_eq!(
            bracket_pair(text, open as i32, "x.sh"),
            Some((open, open + 4))
        );
    }

    #[test]
    fn heredoc_body_brackets_are_skipped() {
        let src = "(a\ncat <<EOF\n)\nEOF\nb)";
        assert_eq!(pair(src, 0), Some((0, 20)));
    }

    #[test]
    fn shell_arithmetic_pairs_normally() {
        assert_eq!(pair("x=$((1 + 2))", 4), Some((3, 11)));
    }

    #[test]
    fn plain_language_never_matches() {
        assert_eq!(bracket_pair("a(b)", 1, "x.txt"), None);
    }

    #[test]
    fn unmatched_brackets_take_the_pruned_paths() {
        assert_eq!(pair("a) b(", 1), None);
        assert_eq!(pair("([)]", 0), None);
        assert_eq!(pair("([)]", 3), None);
    }

    #[test]
    fn caret_edges_are_safe() {
        assert_eq!(pair("", 0), None);
        assert_eq!(pair("abc", 0), None);
        assert_eq!(pair("abc", 3), None);
        assert_eq!(pair("abc", 99), None);
        assert_eq!(pair("abc", -1), None);
    }

    #[test]
    fn spans_for_merges_bracket_monotonically() {
        let text = "a(b)";
        let result = spans_for(text, "x.sh", true, Some((1, 3)));
        let mut previous_end = 0;
        for entry in result.split(';').filter(|s| !s.is_empty()) {
            let mut f = entry.split(':');
            let start: usize = f.next().unwrap().parse().unwrap();
            let end: usize = f.next().unwrap().parse().unwrap();
            let colour = f.next().unwrap();
            assert!(start >= previous_end, "overlap in {result}");
            previous_end = end;
            if start == 1 || start == 3 {
                assert_eq!(colour, BRACKET_DARK);
            }
        }
    }

    #[test]
    fn spans_for_light_theme_uses_bracket_light() {
        let result = spans_for("a(b)", "x.sh", false, Some((1, 3)));
        assert!(result.contains(BRACKET_LIGHT));
    }

    #[test]
    fn spans_for_none_matches_themed_spans() {
        let text = "if x; then echo 1; fi";
        assert_eq!(
            spans_for(text, "x.sh", true, None),
            themed_spans(text, "x.sh", true)
        );
    }
}
