//! Line-ending normalisation for the built-in editor.
//!
//! A Windows clipboard (and any CRLF file) delivers `\r\n`, and parley treats
//! CR and LF as two independent line breaks — each one commits its own line —
//! so a pasted line would render with a stray empty row, which also stretched
//! the highlight overlay into the next row. Pasting is normalised inside the
//! vendored i-slint-core `insert()` (see `vendor/i-slint-core`); this helper
//! covers the other two write paths: file open and replace-all.

/// Convert every line ending in `text` to `\n`: `\r\n` collapses to `\n` and a
/// lone `\r` (old-Mac file) becomes `\n`.
pub fn strip_cr(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_collapses() {
        assert_eq!(strip_cr("a\r\nb\r\nc"), "a\nb\nc");
    }

    #[test]
    fn lone_cr_is_length_preserving() {
        assert_eq!(strip_cr("a\rb\rc"), "a\nb\nc");
    }

    #[test]
    fn mixed_endings() {
        assert_eq!(strip_cr("a\r\nb\rc\r"), "a\nb\nc\n");
        assert_eq!(strip_cr("one\nonly"), "one\nonly");
    }

    #[test]
    fn empty_string() {
        assert_eq!(strip_cr(""), "");
    }
}
