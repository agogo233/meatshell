use super::*;
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

/// Lexical colors only: offsets refer to original UTF-8 text and never change
/// fonts or whitespace. The native renderer owns shaping, selection and IME.
pub(super) fn highlight(text: &str, path: &str) -> String {
    let language = language(path);
    if language == "Text" || text.len() > 512 * 1024 {
        return String::new();
    }
    static TOKENS: OnceLock<[regex::Regex; 3]> = OnceLock::new();
    let tokens = TOKENS.get_or_init(|| {
        let common = r#"(?s:""".*?(?:"""|$)|'''.*?(?:'''|$))|"(?:\\.|[^"\\r\\n])*"|'(?:\\.|[^'\\r\\n])*'|`(?:\\.|[^`\\r\\n])*`|\b(?:0[xX][0-9a-fA-F]+|[0-9]+(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?)\b|\b[A-Za-z_][A-Za-z_0-9]*\b"#;
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
    let keywords = match language {
        "Rust" => "fn let mut pub struct enum impl trait use mod const static match if else for while loop in return break continue async await move ref self Self where as unsafe dyn type crate super extern union",
        "Python" => "def class import from try except finally with lambda yield pass raise and or not if elif else for while in return break continue async await as is global nonlocal del assert",
        "Shell" => "if then elif else fi for while until in do done case esac function local export readonly return break continue select time coproc getopts shift unset source alias unalias test echo printf cd pwd read mapfile declare typeset let eval exec exit trap wait kill",
        "JavaScript" => "let const var function class extends if else for while do in of return break continue async await new switch case default throw try catch finally export import from interface type typeof instanceof void delete this super implements public private protected",
        "C/C++" => "struct enum class namespace typedef using template typename const static extern volatile if else for while do return break continue switch case default try catch throw new delete public private protected virtual override auto void int char float double bool unsigned signed long short sizeof nullptr",
        _ => "",
    };
    let mut result = String::new();
    for token in tokens.find_iter(text) {
        let value = token.as_str();
        let color = if (hash_comments && value.starts_with('#'))
            || (slash_comments && (value.starts_with("//") || value.starts_with("/*")))
        {
            "ff6a9955"
        } else if matches!(language, "JSON" | "YAML" | "TOML")
            && text[token.end()..]
                .trim_start_matches([' ', '\t'])
                .starts_with(if language == "TOML" { '=' } else { ':' })
        {
            "ff9cdcfe"
        } else if value.starts_with('"') || value.starts_with('\'') || value.starts_with('`') {
            "ffce9178"
        } else if value.as_bytes()[0].is_ascii_digit() {
            "ffb5cea8"
        } else if matches!(
            value,
            "true" | "false" | "null" | "None" | "True" | "False" | "nil"
        ) {
            "ff569cd6"
        } else if keywords
            .split_ascii_whitespace()
            .any(|keyword| keyword == value)
        {
            "ffc586c0"
        } else {
            continue;
        };
        let _ = write!(result, "{}:{}:{};", token.start(), token.end(), color);
    }
    result
}

/// Refresh the detached editor window's language and syntax colours.
pub(super) fn refresh(window: &EditorWindow, text: &str) {
    let path = window.get_editor_path();
    window.set_editor_language(language(path.as_str()).into());
    window.set_editor_syntax_spans(themed_spans(text, path.as_str(), window.get_dark_mode()).into());
    window.set_editor_syntax_source(text.into());
}

/// Syntax spans for `text`, recoloured for the light theme when needed.
pub(super) fn themed_spans(text: &str, path: &str, dark: bool) -> String {
    let mut spans = highlight(text, path);
    if !dark {
        for (dark, light) in [
            ("ff6a9955", "ff38761d"),
            ("ffce9178", "ffa31515"),
            ("ffb5cea8", "ff098658"),
            ("ff569cd6", "ff0000ff"),
            ("ffc586c0", "ff800080"),
            ("ff9cdcfe", "ff0451a5"),
        ] {
            spans = spans.replace(dark, light);
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let result = highlight("#!/bin/sh\nif [ \"$1\" = \"ok\" ]; then\n  echo 42\nfi\n", "deploy.sh");
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
            assert!(!value.contains('\n'), "span crossed a line boundary: {value:?}");
        }
    }
}
