use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, Rgb8Pixel};
use std::rc::Rc;

slint::slint! {
    export component EditorDragScroll inherits Window {
        width: 240px; height: 120px;
        background: #000000;
        out property <bool> scrolled: fl.content-y != 0px;
        out property <int> cursor-moves: 0;
        fl := Flickable {
            x: 0px; y: 0px;
            width: 240px; height: 120px;
            content-height: 360px;
            mouse-drag-pan-enabled: false;
            edit := TextInput {
                x: 4px; y: 4px;
                width: 232px; height: 352px;
                font-size: 14px;
                color: #ffffff;
                single-line: false;
                wrap: char-wrap;
                text: "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\nline 11\nline 12\nline 13\nline 14\nline 15\nline 16\nline 17\nline 18\nline 19\nline 20\nline 21\nline 22\nline 23\nline 24";
                cursor-position-changed(position) => {
                    root.cursor-moves = root.cursor-moves + 1;
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
fn editor_wheel_scrolls_while_dragging_a_selection() {
    // Slint contexts are thread-local; avoid interacting with any app window.
    std::thread::spawn(|| {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Backend(window.clone()))).unwrap();
        let ui = EditorDragScroll::new().unwrap();
        ui.show().unwrap();
        window.set_size(slint::PhysicalSize::new(240, 120));
        settle(&window);

        let start = slint::LogicalPosition::new(120., 60.);
        window.dispatch_event(WindowEvent::PointerPressed {
            position: start,
            button: PointerEventButton::Left,
        });
        // Not strictly required (mouse-drag-pan-enabled is off, so the Flickable rejects the
        // press up front and nothing is delayed), but keeping the press flushed before the
        // first move.
        settle(&window);
        window.dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(120., 90.),
        });
        settle(&window);
        let cursor_moves = ui.get_cursor_moves();
        assert!(cursor_moves > 0, "the drag must extend the selection");
        assert!(!ui.get_scrolled(), "nothing may scroll before the wheel");

        window.dispatch_event(WindowEvent::PointerScrolled {
            position: start,
            delta_x: 0.,
            delta_y: -12.,
        });
        settle(&window);
        assert!(ui.get_scrolled(), "the wheel must scroll the editor while a selection is dragged");

        window.dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(120., 110.),
        });
        settle(&window);
        assert!(
            ui.get_cursor_moves() > cursor_moves,
            "the drag must keep extending after the wheel"
        );
    })
    .join()
    .unwrap();
}
