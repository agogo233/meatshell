//! Idle-CPU guard (#499).
//!
//! A Slint `animate … { iteration-count: -1; }` never finishes: any later
//! change of the animated property restarts it with the same infinite
//! settings, so once triggered it keeps the animation driver asking for a
//! new frame forever — even while the element is hidden. With the GPU
//! renderer that is a full-window repaint every frame and CPU stays high
//! until the app is closed. Looping effects must use `animation-tick()`
//! behind a condition instead, which stops requesting frames as soon as the
//! condition turns false.

use std::path::Path;

fn live_lines(source: &str) -> impl Iterator<Item = (usize, &str)> {
    source
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.split("//").next().unwrap_or("")))
}

#[test]
fn no_infinite_property_animations_in_ui() {
    let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&ui).expect("ui dir") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("slint") {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        for (line_no, code) in live_lines(&source) {
            let compact: String = code.chars().filter(|c| !c.is_whitespace()).collect();
            if compact.contains("iteration-count:-") {
                offenders.push(format!("{}:{line_no}", path.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "infinite `animate` found (use a conditional animation-tick() instead, #499): {offenders:?}"
    );
}

#[test]
fn transfers_pulse_only_ticks_while_breathing() {
    let source = include_str!("../ui/app.slint");
    let start = source.find("breathe-layer := Rectangle").expect("breathe layer");
    let layer = &source[start..start + source[start..].find("\n    }").unwrap()];
    assert!(layer.contains("opacity: root.breathe"));
    assert!(layer.contains("animation-tick()"));
    assert!(!layer.contains("animate opacity"));
}
