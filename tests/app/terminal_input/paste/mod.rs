use super::super::*;
use crate::terminal::build_paste_preview;

#[test]
fn paste_normalizes_newlines_to_cr() {
    assert_eq!(
        normalize_pasted_newlines("sudo apt install \\\r\n  docker-ce"),
        "sudo apt install \\\r  docker-ce"
    );
    assert_eq!(normalize_pasted_newlines("a\nb\nc"), "a\rb\rc");
    assert_eq!(normalize_pasted_newlines("a\rb"), "a\rb");
    assert_eq!(normalize_pasted_newlines("echo hi"), "echo hi");
}

#[test]
fn command_bar_preserves_multiline_heredoc() {
    let command = "cat <<'EOF'\nHEREDOC-1\n中文-HEREDOC-2\nEOF\n";
    let (history, bytes) = encode_command_bar_input(command);
    assert_eq!(history.as_deref(), Some(command.trim_end()));
    assert_eq!(bytes, command.as_bytes());
    assert!(!history
        .unwrap()
        .lines()
        .any(|line| line.starts_with(' ')));
}

#[test]
fn empty_command_bar_submission_sends_enter_without_history() {
    for input in ["", "   ", "\t"] {
        let (history, bytes) = encode_command_bar_input(input);
        assert_eq!(history, None);
        assert_eq!(bytes, b"\n");
    }
}

#[test]
fn paste_uses_remote_bracketed_paste_mode() {
    assert_eq!(
        encode_pasted_text("first\r\n  second", true),
        b"\x1b[200~first\r  second\x1b[201~"
    );
    assert_eq!(
        encode_pasted_text("safe\x1b[201~\x03text", true),
        b"\x1b[200~safe[201~text\x1b[201~"
    );
    assert_eq!(
        encode_pasted_text("first\r\nsecond", false),
        b"first\rsecond"
    );
}

#[test]
fn long_pastes_switch_to_large_review() {
    assert!(!paste_requires_large_review("short prompt\nsecond line"));
    assert!(!paste_requires_large_review(&"a".repeat(600)));
    assert!(paste_requires_large_review(&"a".repeat(601)));
    assert!(!paste_requires_large_review(&vec!["line"; 12].join("\r\n")));
    assert!(paste_requires_large_review(&vec!["line"; 13].join("\r\n")));
}

#[test]
fn paste_preview_passes_short_text_through() {
    assert_eq!(build_paste_preview(""), "");
    assert_eq!(build_paste_preview("hello"), "hello");
    assert_eq!(build_paste_preview("a\nb\nc"), "a\nb\nc");
}

/// Slint's software renderer uses i16 coordinates (~±32767). A Text that
/// lays out thousands of lines (or one multi-kilobyte line) overflows and
/// panics (slint#12985 / #434). The preview must stay well inside that range.
#[test]
fn paste_preview_stays_within_i16_safe_layout_bounds() {
    // ~3000 lines of typical code — the #434 repro shape.
    let text = (0..3000)
        .map(|i| format!("fn handler_{i}() {{ let x = {i}; }}"))
        .collect::<Vec<_>>()
        .join("\n");
    let preview = build_paste_preview(&text);

    // 48 content lines + 1 notice ≪ 32767 / ~20px line height.
    assert!(
        preview.lines().count() <= 50,
        "preview lines {} exceed i16-safe budget",
        preview.lines().count()
    );
    // Longest display line is capped at 240 + ellipsis.
    let max_line = preview.lines().map(|l| l.chars().count()).max().unwrap_or(0);
    assert!(
        max_line <= 250,
        "preview max line {max_line} exceeds i16-safe width"
    );
    assert!(preview.chars().count() < 8 * 1024);
    assert!(preview.starts_with("fn handler_0"));
    assert!(preview.contains("3000") || preview.contains("48/"));
}

#[test]
fn paste_preview_caps_long_single_line() {
    let text = "x".repeat(50_000);
    let preview = build_paste_preview(&text);
    assert!(preview.chars().count() < 8 * 1024);
    assert!(preview.contains('…') || preview.contains("..."));
    // One display line plus a notice line.
    assert!(preview.lines().count() <= 3);
}

/// End-to-end contract for #434 at the logic layer:
/// 1. UI preview is truncated (so software-renderer layout stays in i16).
/// 2. Confirm still yields the FULL original payload (so paste is not lossy).
/// 3. Cancel drops the payload.
/// 4. Confirm for another tab does not steal the pending paste.
#[test]
fn pending_paste_keeps_full_payload_while_ui_only_sees_preview() {
    let pending: PendingPaste = std::sync::Mutex::new(None);
    let full: String = (0..3000)
        .map(|i| format!("line {i} content for vim insert-mode paste"))
        .collect::<Vec<_>>()
        .join("\n");

    let preview = store_pending_paste(&pending, "tab-a".into(), full.clone());

    // (1) Preview must not be the full payload and must be bounded.
    assert_ne!(preview, full);
    assert!(preview.len() < full.len() / 10);
    assert!(preview.lines().count() <= 50);

    // (2) Confirm path recovers the exact full text (not the preview).
    let taken = take_pending_paste(&pending, "tab-a").expect("full payload for matching tab");
    assert_eq!(taken, full);
    assert_ne!(taken, preview);

    // Empty after take.
    assert!(take_pending_paste(&pending, "tab-a").is_none());

    // (3) Cancel clears.
    store_pending_paste(&pending, "tab-a".into(), full.clone());
    clear_pending_paste(&pending);
    assert!(take_pending_paste(&pending, "tab-a").is_none());

    // (4) Wrong tab cannot steal.
    store_pending_paste(&pending, "tab-a".into(), full.clone());
    assert!(take_pending_paste(&pending, "tab-b").is_none());
    assert_eq!(take_pending_paste(&pending, "tab-a").unwrap(), full);
}

/// Confirm must encode the full payload into the PTY bytes, not the preview.
#[test]
fn confirmed_paste_bytes_come_from_full_payload_not_preview() {
    let pending: PendingPaste = std::sync::Mutex::new(None);
    let full = vec!["echo step"; 3000].join("\n");
    let preview = store_pending_paste(&pending, "tab".into(), full.clone());
    let taken = take_pending_paste(&pending, "tab").unwrap();

    let from_full = encode_pasted_text(&taken, false);
    let from_preview = encode_pasted_text(&preview, false);
    let expected = normalize_pasted_newlines(&full).into_bytes();

    assert_eq!(from_full, expected, "confirm must paste the full clipboard");
    assert_ne!(
        from_full, from_preview,
        "preview must not be what reaches the PTY"
    );
}

#[test]
fn paste_preview_never_matches_full_multithousand_line_payload() {
    // Regression for #434: the UI must never receive the full payload.
    let text = vec!["fn main() { println!(\"hi\"); }"; 3000].join("\n");
    let preview = build_paste_preview(&text);
    assert!(preview.len() < text.len() / 10);
    assert_ne!(preview, text);
}
