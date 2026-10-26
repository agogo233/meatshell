# Native editor patch (Slint 1.18.1)

`i-slint-core` and `i-slint-compiler` are the published 1.18.1 crates, with
upstream license files retained. Both are selected by `[patch.crates-io]`.
They must be upgraded together; do not remove only one override. Every change
is marked with a `MeatShell editor patch` comment.

Changes from upstream:

- `i-slint-compiler/builtin_elements.rs` (the `item! { TextInput { … } }`
  block) and `i-slint-core/items/text.rs` expose three opt-in TextInput
  properties: `syntax-source`, `syntax-spans`, and `line-number-width`.
  Existing inputs use empty/zero defaults.
- `i-slint-core/item_rendering.rs` adds `RenderString::syntax_colors()`
  (default `None`). `TextInput` returns its spans only when `syntax-source`
  equals the displayed text, no IME preedit is active, and it is not a
  password field, so debounced results never colour the wrong text.
- `i-slint-core/textlayout/sharedparley/shaping.rs` applies those colour-only
  ranges while shaping plain text (`create_text_paragraphs_with_syntax`). The
  paragraph split, byte ranges and metrics are exactly those of plain text, so
  native shaping, wrap, cursor, clipboard, selection and IME are unchanged.
  Because shaping reads the properties, Slint's own `TextLayoutCache` reshapes
  when they change and reuses the shaped text otherwise (typing reshapes once;
  scrolling, selecting and cursor blinking do not).
- Syntax spans use sorted, non-overlapping UTF-8 byte ranges encoded as
  `start:end:AARRGGBB;`. Invalid ranges are ignored.
- A positive `line-number-width` (`sharedparley.rs`, `draw_line_numbers`)
  draws logical line numbers only, right-aligned and on the first visual
  baseline of each paragraph. The gutter uses the same source, font, width,
  height and wrapping as the editable input and is clipped by its narrow
  parent. Only numbers inside the clip are shaped and drawn.
- `i-slint-core/input.rs` (`handle_mouse_grab`): a wheel held by the mouse grab is first
  offered to the grabber and, when the grabber ignores it, to the items below it in the grab
  stack, so the scrollable area behind a text input being dragged can scroll (#388). The grab
  is kept for the whole wheel, so the selection keeps following the pointer; only wheel events
  are forwarded, the grabber is never invoked twice, and when nothing accepts the wheel the
  upstream behaviour (end the grab, return a move event) is preserved.

Dropped when rebasing from 1.16.1, because Slint 1.18 does it upstream: the
per-item editor layout cache (now `TextLayoutCache`), drawing the selection
after syntax colours (selection is now recoloured by clipping), painting the
selection background inside the input's clip (#440), and skipping paragraphs
outside the clip.

Qt's separate native renderer does not implement these properties, so the
application explicitly enables Winit instead of `backend-default`. Slint and
slint-build are pinned to the patched version.

The application owns language detection and lexical tokenization in
`src/app/editor_syntax.rs`; the renderer does not parse programming languages.

`ui/editor_pane.slint` reads three TextInput properties upstream marks "internal,
undocumented, only exposed for tests": `anchor-position-byte-offset`,
`cursor-position-byte-offset`, and `font-metrics`. Nothing writes them. The first
two hand the pane the selection's byte offsets so it can grow the selection while
the viewport scrolls; `font-metrics` gives the line height as
`ascent - descent` (descent is negative), which turns a pixel delta into whole
lines. Keep them available when rebasing.

## i-slint-backend-winit

Also the published 1.18.1 crate, with two fixes marked `MeatShell patch`:

- `accesskit.rs`: never drop the AccessKit adapter on macOS, which avoids a
  Touch Bar crash when a window closes (#488).
- `lib.rs`: don't materialize windows the app never showed; on Wayland they
  would be mapped immediately and appear in the taskbar.

Dropped as upstream in 1.18: the macOS DisplayLink fallback (1.18 probes the
`displayLinkWithTarget:selector:` selector itself) and the focus-change
`RefCell` re-entrancy guard.

Validation: `cargo test --locked editor_ -- --nocapture`. The headless native
render test compares plain/highlighted geometry at two wrapping widths, checks
selection precedence, verifies stale spans cannot affect edits, exercises the
gutter, and prints 10,000-line first/cached paint timings. Its screenshots go to
`target/editor-*.png`. Run this test when rebasing the patch, in addition to
checking IME behavior on the target desktop platforms. The same filter also runs
the drag-scroll test, which presses, drags and wheels inside a grabbed editor and
requires both the scroll offset to move and the selection drag to survive the
wheel: that is what keeps the `handle_mouse_grab` change honest. The filter also
runs the wheel-grow tests, which select in the same editor, wheel both ways and
require each scroll to extend the end on the revealed side: that is what keeps the
`scroll_selection` helper honest.
