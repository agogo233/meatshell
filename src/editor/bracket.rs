//! Bracket-pair matching for the built-in editor overlay.
//!
//! Given the caret byte offset, find the partner of the bracket directly
//! left of the caret (the typical "just typed it" case) or under it. The
//! scan reuses the highlighter's per-line state machine so brackets inside
//! strings, comments and heredoc bodies never participate in matching.

use super::highlight::{line_roles, Role};
use super::lang::Lang;

const OPEN: &[u8] = b"({[";
const CLOSE: &[u8] = b")]}" ;

fn is_bracket(b: u8) -> bool {
    OPEN.contains(&b) || CLOSE.contains(&b)
}

fn matching(open: u8, close: u8) -> bool {
    matches!((open, close), (b'(', b')') | (b'{', b'}') | (b'[', b']'))
}

/// Byte offsets `(open, close)` of the bracket pair adjacent to `offset`, or
/// `None` when the caret is not next to a bracket or has no partner.
pub fn find_pair(content: &str, offset: usize, lang: Lang) -> Option<(usize, usize)> {
    if lang == Lang::Plain {
        return None;
    }
    let t = target(content, offset)?;
    let mut stack: Vec<(u8, usize)> = Vec::new();
    let mut carry = String::new();
    let mut base = 0usize;
    for line in content.split('\n') {
        let (segments, next_carry) = line_roles(lang, line, &carry);
        carry = next_carry;
        let mut pos = base;
        for (text, role) in &segments {
            // String/comment/heredoc content is never bracket structure.
            if matches!(role, Role::Str | Role::Comment) {
                pos += text.len();
                continue;
            }
            for ch in text.bytes() {
                if OPEN.contains(&ch) {
                    stack.push((ch, pos));
                } else if CLOSE.contains(&ch) {
                    // A closer at the target with nothing open can never pair:
                    // stop instead of scanning the rest of the document.
                    if pos == t && stack.is_empty() {
                        return None;
                    }
                    if let Some((ob, opos)) = stack.pop() {
                        if matching(ob, ch) && (opos == t || pos == t) {
                            return Some((opos.min(pos), opos.max(pos)));
                        }
                        // The target just lost its only partner (a popped
                        // opener is never pushed again): no later scan can
                        // pair it, so stop early.
                        if !matching(ob, ch) && (opos == t || pos == t) {
                            return None;
                        }
                    }
                }
                pos += 1;
            }
        }
        base = pos + 1; // step over the '\n'
    }
    None
}

/// The bracket position the caret acts on: the character directly left of the
/// caret wins (mirrors the moment right after typing a bracket), otherwise the
/// character under it.
fn target(content: &str, offset: usize) -> Option<usize> {
    let left = offset.checked_sub(1).filter(|&i| {
        content.is_char_boundary(i) && is_bracket(content.as_bytes()[i])
    });
    left.or_else(|| {
        (offset < content.len() && content.is_char_boundary(offset)
            && is_bracket(content.as_bytes()[offset]))
        .then_some(offset)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(text: &str, offset: usize) -> Option<(usize, usize)> {
        find_pair(text, offset, Lang::Shell)
    }

    #[test]
    fn adjacent_brackets_match_both_sides() {
        assert_eq!(pair("echo $(date)", 6), Some((6, 11))); // caret on `(`
        assert_eq!(pair("echo $(date)", 11), Some((6, 11))); // caret before `)`
    }

    #[test]
    fn nested_prefers_the_tightest_partner() {
        let text = "f((g(x)))";
        assert_eq!(pair(text, 2), Some((1, 8))); // left char `(` at 1
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
        // "中文" is 6 bytes; the parens follow it.
        let text = "echo 中文(x)";
        assert_eq!(pair(text, 11), Some((11, 13))); // caret before `(`
        assert_eq!(pair(text, 12), Some((11, 13))); // caret after `(`
    }

    #[test]
    fn brackets_in_strings_and_comments_are_skipped() {
        // The `}` inside the quoted text must not steal the block's closer.
        let text = "x = \"{\"\ny = \"}\"";
        assert_eq!(find_pair(text, 4, Lang::Python), None);
        assert_eq!(find_pair("a(b)", 1, Lang::Shell), Some((1, 3)));
    }

    #[test]
    fn heredoc_body_is_skipped() {
        // A stray `)` inside the heredoc body must not close an earlier
        // paren: the pair spans the whole heredoc.
        let src = "(a\ncat <<EOF\n)\nEOF\nb)";
        assert_eq!(find_pair(src, 0, Lang::Shell), Some((0, 20)));
    }

    #[test]
    fn shell_arithmetic_pairs_normally() {
        assert_eq!(pair("x=$((1 + 2))", 4), Some((3, 11)));
    }

    #[test]
    fn plain_language_never_matches() {
        assert_eq!(find_pair("a(b)", 1, Lang::Plain), None);
    }

    #[test]
    fn unmatched_brackets_take_the_pruned_paths() {
        // Closer at the target with an empty stack stops the scan in place
        // (the later opener `(` cannot pair it — same result as the old
        // full-document scan, reached faster).
        assert_eq!(pair("a) b(", 1), None);
        // Opener popped by a non-matching closer: no partner possible.
        assert_eq!(find_pair("([)]", 0, Lang::Shell), None);
        // Caret right of the `)` that kills the opener (left char wins).
        assert_eq!(find_pair("([)]", 3, Lang::Shell), None);
    }
}
