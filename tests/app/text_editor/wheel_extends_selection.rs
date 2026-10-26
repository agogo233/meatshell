use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::WindowEvent;
use slint::{ComponentHandle, Rgb8Pixel};
use std::rc::Rc;

slint::slint! {
    export struct GrownSelection {
        start: int,
        end: int,
    }

    export component EditorWheelSelect inherits Window {
        width: 240px; height: 120px;
        background: #000000;
        in-out property <string> text: "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\nline 11\nline 12\nline 13\nline 14\nline 15\nline 16\nline 17\nline 18\nline 19\nline 20\nline 21\nline 22\nline 23\nline 24";
        out property <int> anchor-offset: edit.anchor-position-byte-offset;
        out property <int> cursor-offset: edit.cursor-position-byte-offset;
        out property <length> scroll-y: fl.content-y;
        out property <int> extend-calls: 0;
        property <length> tracked-scroll-y: fl.content-y;
        property <length> last-scroll-y: 0;
        property <bool> scroll-guard: false;
        property <int> scroll-depth: 0;
        callback scroll-select(string, int, int, length, length) -> GrownSelection;
        changed tracked-scroll-y => {
            if (root.scroll-guard || root.scroll-depth > 0) {
                root.last-scroll-y = root.tracked-scroll-y;
            } else {
                let delta-y = root.tracked-scroll-y - root.last-scroll-y;
                root.last-scroll-y = root.tracked-scroll-y;
                if (delta-y != 0px && edit.anchor-position-byte-offset
                        != edit.cursor-position-byte-offset) {
                    let line-height = edit.font-metrics.ascent
                        - edit.font-metrics.descent;
                    if (line-height > 0px) {
                        root.scroll-depth = root.scroll-depth + 1;
                        root.extend-calls = root.extend-calls + 1;
                        let grown = root.scroll-select(root.text,
                            edit.anchor-position-byte-offset,
                            edit.cursor-position-byte-offset,
                            delta-y, line-height);
                        edit.set-selection-offsets(grown.start, grown.end);
                        root.scroll-depth = root.scroll-depth - 1;
                    }
                }
            }
        }
        public function set-selection(anchor: int, focus: int) {
            edit.set-selection-offsets(anchor, focus);
        }
        fl := Flickable {
            x: 0px; y: 0px;
            width: 240px; height: 120px;
            content-height: 360px;
            mouse-drag-pan-enabled: false;
            // Mirror EditorPane: the wheel is applied to the offset immediately
            // instead of going through the Flickable's smooth-scroll animation,
            // so one wheel step is one whole delta.
            TouchArea {
                scroll-event(event) => {
                    fl.content-y = clamp(
                        fl.content-y + event.delta-y,
                        min(0px, fl.height - fl.content-height),
                        0px);
                    accept
                }
            }
            edit := TextInput {
                x: 4px; y: 4px;
                width: 232px; height: 352px;
                font-size: 14px;
                color: #ffffff;
                single-line: false;
                wrap: char-wrap;
                text <=> root.text;
                cursor-position-changed(position) => {
                    root.scroll-guard = true;
                    let cursor-y = position.y + self.y + fl.content-y;
                    if (cursor-y < 12px) {
                        fl.content-y = min(0px, fl.content-y + 12px - cursor-y);
                    } else if (cursor-y > fl.height - 12px) {
                        fl.content-y = max(
                            min(0px, fl.height - fl.content-height),
                            fl.content-y - (cursor-y - fl.height + 12px));
                    }
                    root.scroll-guard = false;
                }
            }
        }
    }
}

struct Backend(Rc<MinimalSoftwareWindow>);
impl slint::platform::Platform for Backend {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

fn settle(window: &MinimalSoftwareWindow) {
    slint::platform::update_timers_and_animations();
    let mut pixels = vec![Rgb8Pixel::default(); 240 * 120];
    window.request_redraw();
    let _ = window.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 240);
    });
    slint::platform::update_timers_and_animations();
}

#[test]
fn editor_scroll_selection_keeps_a_plain_caret() {
    let grown = super::super::editor_host::scroll_selection(
        "a\nbb\nccc\ndddd\nffff", 3, 3, 20.0, 10.0,
    );
    assert_eq!(grown.start, 3);
    assert_eq!(grown.end, 3);
}

#[test]
fn editor_scroll_selection_upward_extends_the_top_end() {
    // anchor 10 / focus 6 is a backwards selection; scrolling up moves the lower
    // (top-of-document) end back by two lines and keeps the roles in place.
    let grown = super::super::editor_host::scroll_selection(
        "a\nbb\nccc\ndddd\nffff", 10, 6, 20.0, 10.0,
    );
    assert_eq!(grown.start, 10);
    assert_eq!(grown.end, 2);
}

#[test]
fn editor_scroll_selection_downward_extends_the_bottom_end() {
    let grown = super::super::editor_host::scroll_selection(
        "a\nbb\nccc\ndddd\nffff", 0, 5, -20.0, 10.0,
    );
    assert_eq!(grown.start, 0);
    assert_eq!(grown.end, 14);
}

#[test]
fn editor_scroll_selection_clamps_to_the_document() {
    let text = "a\nbb\nccc\ndddd\nffff";
    let upward = super::super::editor_host::scroll_selection(text, 8, 4, 900.0, 10.0);
    assert_eq!(upward.start, 8);
    assert_eq!(upward.end, 0);
    let downward = super::super::editor_host::scroll_selection(text, 0, 4, -900.0, 10.0);
    assert_eq!(downward.start, 0);
    assert_eq!(downward.end as usize, text.len());
}

#[test]
fn editor_scroll_selection_ignores_fractional_lines() {
    let grown = super::super::editor_host::scroll_selection(
        "a\nbb\nccc\ndddd\nffff", 10, 6, 4.0, 10.0,
    );
    assert_eq!(grown.start, 10);
    assert_eq!(grown.end, 6);
}

#[test]
fn editor_wheel_grows_the_selection() {
    // Slint contexts are thread-local; avoid interacting with any app window.
    std::thread::spawn(|| {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Backend(window.clone()))).unwrap();
        let ui = EditorWheelSelect::new().unwrap();
        ui.on_scroll_select(|content: slint::SharedString, anchor: i32, focus: i32,
                            delta_y: f32, line_height: f32| {
            let grown = super::super::editor_host::scroll_selection(
                content.as_str(), anchor, focus, delta_y, line_height,
            );
            GrownSelection { start: grown.start, end: grown.end }
        });
        ui.show().unwrap();
        window.set_size(slint::PhysicalSize::new(240, 120));
        settle(&window);

        ui.invoke_set_selection(60, 100);
        settle(&window);
        let extend_calls = ui.get_extend_calls();
        assert_eq!(ui.get_anchor_offset(), 60);
        assert_eq!(ui.get_cursor_offset(), 100);

        let position = slint::LogicalPosition::new(120., 60.);
        // Scroll down a few notches: the viewport moves down, so the revealed
        // text is below the selection and its bottom end must grow.
        for _ in 0..3 {
            window.dispatch_event(WindowEvent::PointerScrolled {
                position,
                delta_x: 0.,
                delta_y: -24.,
            });
            settle(&window);
        }
        assert!(ui.get_scroll_y() < -60.0, "the wheel must scroll the viewport");
        assert!(
            ui.get_extend_calls() > extend_calls,
            "scrolling must grow the selection"
        );
        assert!(
            ui.get_cursor_offset() > 100,
            "scrolling down must extend the bottom end"
        );

        // Reset the selection and scroll back up: the top end must grow.
        ui.invoke_set_selection(60, 100);
        settle(&window);
        window.dispatch_event(WindowEvent::PointerScrolled {
            position,
            delta_x: 0.,
            delta_y: 48.,
        });
        settle(&window);
        assert!(
            ui.get_anchor_offset() < 60,
            "scrolling up must extend the top end"
        );
    })
    .join()
    .unwrap();
}
