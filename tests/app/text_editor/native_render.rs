use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::{ComponentHandle, Rgb8Pixel};
use std::rc::Rc;

slint::slint! {
    export component EditorRenderTest inherits Window {
        width: 360px; height: 240px;
        background: #000000;
        in-out property <string> content;
        in property <string> source;
        in property <string> spans;
        in property <length> text-width: 340px;
        in property <length> gutter-width: 0px;
        in property <length> scroll-y: 0px;
        out property <length> measured-height: input.preferred-height;
        out property <length> cursor-x;
        out property <length> cursor-y;
        public function select(start: int, end: int) {
            input.set-selection-offsets(start, end);
        }
        public function focus-editor() { input.focus(); }
        input := TextInput {
            x: 8px; y: 8px + root.scroll-y;
            width: root.text-width; height: 224px;
            font-family: "Consolas"; font-size: 18px;
            color: #ffffff;
            selection-foreground-color: #ffffff;
            selection-background-color: #224488;
            single-line: false;
            wrap: char-wrap;
            text <=> root.content;
            syntax-source: root.source;
            syntax-spans: root.spans;
            line-number-width: root.gutter-width;
            cursor-position-changed(position) => {
                root.cursor-x = position.x;
                root.cursor-y = position.y;
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

#[test]
fn editor_native_render_preserves_geometry_selection_and_editing() {
    // Slint contexts are thread-local; avoid interacting with any app window.
    std::thread::spawn(|| {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Backend(window.clone()))).unwrap();
        let ui = EditorRenderTest::new().unwrap();
        ui.show().unwrap();
        window.set_size(slint::PhysicalSize::new(360, 240));
        let render = || {
            let mut pixels = vec![Rgb8Pixel::default(); 360 * 240];
            window.request_redraw();
            assert!(window.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, 360);
            }));
            pixels
        };
        let text = "let 名称 = \"if 中文\"; // return\n\nfn main() {\n    let value = 12345;\n}\n";
        ui.set_content(text.into());
        ui.set_source(text.into());
        for width in [340.0, 140.0] {
            ui.set_text_width(width);
            ui.set_spans("".into());
            ui.invoke_select(0, 0);
            let plain = render();
            let height = ui.get_measured_height();
            ui.set_spans(super::super::editor_syntax::highlight(text, "test.rs").into());
            let colored = render();
            assert_eq!(height, ui.get_measured_height());
            assert!(
                colored.iter().any(|p| p.r != p.g || p.g != p.b),
                "syntax colors missing"
            );
            let shape = |pixels: &[Rgb8Pixel]| {
                pixels
                    .iter()
                    .map(|p| p.r > 0 || p.g > 0 || p.b > 0)
                    .collect::<Vec<_>>()
            };
            let plain_shape = shape(&plain);
            let color_shape = shape(&colored);
            let different = plain_shape
                .iter()
                .zip(&color_shape)
                .filter(|(a, b)| a != b)
                .count();
            let occupied = plain_shape.iter().filter(|&&v| v).count();
            eprintln!("width={width}: mask differences={different}, occupied={occupied}");
            for (name, pixels) in [("plain", &plain), ("color", &colored)] {
                let bytes: Vec<u8> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
                let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(format!("target/editor-{name}-{width}.png"));
                image::save_buffer(path, &bytes, 360, 240, image::ColorType::Rgb8).unwrap();
            }
            // Quantizing alpha against different foreground colors can lose
            // a few faint antialiasing pixels; it must not move glyphs.
            assert!(
                different <= occupied / 50 + 4,
                "highlight changed glyph geometry"
            );

            ui.invoke_focus_editor();
            ui.invoke_select(0, text.len() as i32);
            let selected_color = render();
            let cursor = (ui.get_cursor_x(), ui.get_cursor_y());
            ui.set_spans("".into());
            let selected_plain = render();
            assert!(
                selected_color == selected_plain,
                "syntax overrode selection colors"
            );
            assert_eq!(cursor, (ui.get_cursor_x(), ui.get_cursor_y()));
            ui.invoke_select(0, 0);
        }
        ui.set_text_width(340.0);
        ui.set_spans(super::super::editor_syntax::highlight(text, "test.rs").into());
        let screenshot = render();
        let bytes: Vec<u8> = screenshot.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/editor-syntax-qa.png");
        image::save_buffer(path, &bytes, 360, 240, image::ColorType::Rgb8).unwrap();

        // Large-file layouts have two native items in the app rather than
        // thousands of hidden TextInputs. Measure initial paint vs cached paint.
        let large = "let value = 123; // comment\n".repeat(9_999);
        ui.set_content(large.clone().into());
        ui.set_source(large.clone().into());
        ui.set_spans(super::super::editor_syntax::highlight(&large, "large.rs").into());
        let first_at = std::time::Instant::now();
        render();
        let first_time = first_at.elapsed();
        let cached_at = std::time::Instant::now();
        render();
        let cached_time = cached_at.elapsed();
        eprintln!("editor 10,000 lines: first paint {first_time:?}, cached paint {cached_time:?}");
        ui.set_gutter_width(42.0);
        ui.set_spans("".into());
        ui.set_scroll_y(-36.0);
        let gutter = render();
        assert!(gutter.iter().any(|p| p.r > 0), "gutter missing");
        assert!(
            gutter
                .chunks(360)
                .all(|row| row[52..].iter().all(|p| p.r == 0)),
            "gutter painted document text instead of numbers"
        );
        ui.set_gutter_width(0.0);
        ui.set_scroll_y(0.0);
        ui.set_content(text.into());
        ui.set_source(text.into());
        ui.set_spans(super::super::editor_syntax::highlight(text, "test.rs").into());

        // Stale syntax must not apply to newly typed text.
        ui.invoke_select(0, text.len() as i32);
        window.dispatch_event(slint::platform::WindowEvent::KeyPressed { text: "x".into() });
        window.dispatch_event(slint::platform::WindowEvent::KeyReleased { text: "x".into() });
        assert_eq!(ui.get_content().as_str(), "x");
        let stale = render();
        ui.set_spans("".into());
        assert!(stale == render());
    })
    .join()
    .unwrap();
}

#[test]
fn editor_app_window_renders_highlighted_document() {
    std::thread::spawn(|| {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Backend(window.clone()))).unwrap();
        let ui = super::super::EditorWindow::new().unwrap();
        let text = "// Unicode and wrapping\nfn main() {\n    let message = \"Hello, 中文\";\n    println!(\"{}\", message);\n}\n";
        ui.set_editor_path("example.rs".into());
        ui.set_editor_name("example.rs".into());
        ui.set_editor_content(text.into());
        super::super::editor_syntax::refresh(&ui, text);
        ui.set_editor_open(true);
        ui.show().unwrap();
        window.set_size(slint::PhysicalSize::new(1000, 700));
        let mut pixels = vec![Rgb8Pixel::default(); 1000 * 700];
        window.request_redraw();
        assert!(window.draw_if_needed(|renderer| { renderer.render(&mut pixels, 1000); }));
        assert_eq!(ui.get_editor_language().as_str(), "Rust");
        assert!(!ui.get_editor_syntax_spans().is_empty());
        let bytes: Vec<u8> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/editor-window-qa.png");
        image::save_buffer(path, &bytes, 1000, 700, image::ColorType::Rgb8).unwrap();
    }).join().unwrap();
}

fn editor_doc(id: &str, owner: &str, path: &str, content: &str, dirty: bool) -> super::super::EditorDoc {
    super::super::EditorDoc {
        id: id.into(),
        owner: owner.into(),
        path: path.into(),
        name: path.rsplit('/').next().unwrap_or(path).into(),
        content: content.into(),
        revision: 0,
        readonly: false,
        dirty,
        orphaned: false,
        language: super::super::editor_syntax::language(path).into(),
        syntax_source: content.into(),
        syntax_spans: super::super::editor_syntax::themed_spans(content, path, true).into(),
    }
}

fn tab(id: &str, title: &str, kind: &str, connected: bool) -> super::super::TabInfo {
    super::super::TabInfo {
        id: id.into(),
        title: title.into(),
        title_len: title.chars().count() as i32,
        kind: kind.into(),
        connected,
        local: false,
    }
}

#[test]
fn editor_tab_renders_in_a_split_pane() {
    // #494: an editor tab shows only the editor, in whichever pane holds it.
    std::thread::spawn(|| {
        use slint::{ModelRc, VecModel};
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Backend(window.clone()))).unwrap();
        let ui = super::super::AppWindow::new().unwrap();
        let text = "server {\n    listen 80;\n}\nfn main() {\n    println!(\"tab\");\n}\n";
        ui.set_editor_docs(ModelRc::from(std::rc::Rc::new(VecModel::from(vec![
            editor_doc("editor-1", "t1", "/etc/example.rs", text, true),
        ]))));
        let pane = |id: i32, x: f32, w: f32, tabs: Vec<super::super::TabInfo>, active: &str| {
            super::super::PaneInfo {
                id,
                x,
                y: 0.0,
                w,
                h: 700.0,
                active_id: active.into(),
                focused: id == 1,
                reserve_right: 0.0,
                tabs: ModelRc::from(std::rc::Rc::new(VecModel::from(tabs))),
            }
        };
        ui.set_panes(ModelRc::from(std::rc::Rc::new(VecModel::from(vec![
            pane(0, 0.0, 500.0, vec![tab("welcome", "New tab", "welcome", false)], "welcome"),
            // Unsaved edits: the tab dot uses the accent colour.
            pane(1, 504.0, 600.0, vec![tab("editor-1", "example.rs", "editor", false)], "editor-1"),
        ]))));
        ui.show().unwrap();
        window.set_size(slint::PhysicalSize::new(1400, 800));
        let mut pixels = vec![Rgb8Pixel::default(); 1400 * 800];
        window.request_redraw();
        assert!(window.draw_if_needed(|renderer| { renderer.render(&mut pixels, 1400); }));
        let bytes: Vec<u8> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/editor-tab-qa.png");
        image::save_buffer(path, &bytes, 1400, 800, image::ColorType::Rgb8).unwrap();
    }).join().unwrap();
}

#[test]
fn editor_tab_close_asks_when_dirty_and_orphans_on_terminal_close() {
    std::thread::spawn(|| {
        use slint::{Model, ModelRc, VecModel};
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Backend(window.clone()))).unwrap();
        let ui = super::super::AppWindow::new().unwrap();
        let docs = std::rc::Rc::new(VecModel::from(vec![
            editor_doc("editor-1", "t1", "/a.conf", "a", true),
            editor_doc("editor-2", "t1", "/b.conf", "b", false),
            editor_doc("editor-3", "t2", "/c.conf", "c", false),
        ]));
        ui.set_editor_docs(ModelRc::from(docs.clone()));

        // Unsaved edits defer the close to the confirmation dialog.
        assert!(super::super::editor_tabs::close_needs_confirm(&ui, "editor-1"));
        assert!(ui.get_editor_discard_confirm_open());
        assert_eq!(ui.get_editor_discard_id().as_str(), "editor-1");
        // A clean document and a terminal tab close straight away.
        assert!(!super::super::editor_tabs::close_needs_confirm(&ui, "editor-2"));
        assert!(!super::super::editor_tabs::close_needs_confirm(&ui, "t1"));

        // Closing an editor tab drops its document only.
        super::super::editor_tabs::forget_tab(&ui, "editor-2");
        assert_eq!(docs.row_count(), 2);
        // Closing terminal t1 orphans the documents it saved through.
        super::super::editor_tabs::forget_tab(&ui, "t1");
        let rows: Vec<_> = docs.iter().collect();
        assert!(rows.iter().find(|d| d.id == "editor-1").unwrap().orphaned);
        assert!(!rows.iter().find(|d| d.id == "editor-3").unwrap().orphaned);
    }).join().unwrap();
}
