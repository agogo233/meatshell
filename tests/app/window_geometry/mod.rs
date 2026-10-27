use super::{maximized_geometry_needs_repair, window_rect_visible_on_any_monitor};

#[test]
fn repairs_large_maximized_geometry_mismatch() {
    assert!(maximized_geometry_needs_repair(604, 1384, 1080, 1501));
    assert!(maximized_geometry_needs_repair(1920, 1000, 3840, 2160));
}

#[test]
fn accepts_taskbar_sized_maximized_work_area() {
    assert!(!maximized_geometry_needs_repair(1920, 1040, 1920, 1080));
    assert!(!maximized_geometry_needs_repair(2560, 1400, 2560, 1440));
}

#[test]
fn on_screen_position_is_visible() {
    let monitors = [(0.0, 0.0, 1920.0, 1080.0)];
    assert!(window_rect_visible_on_any_monitor((100.0, 100.0, 1280.0, 720.0), &monitors));
}

#[test]
fn windows_minimized_sentinel_is_invisible() {
    // Windows reports (-32000,-32000) for a minimized HWND.
    let monitors = [(0.0, 0.0, 1920.0, 1080.0)];
    assert!(!window_rect_visible_on_any_monitor((-32000.0, -32000.0, 1280.0, 720.0), &monitors));
}

#[test]
fn off_screen_position_is_invisible() {
    let monitors = [(0.0, 0.0, 1920.0, 1080.0)];
    assert!(!window_rect_visible_on_any_monitor((5000.0, 5000.0, 1280.0, 720.0), &monitors));
}

#[test]
fn secondary_left_monitor_is_visible() {
    // Negative monitor origins are legitimate dual-monitor layouts.
    let monitors = [
        (-1920.0, 0.0, 1920.0, 1080.0),
        (0.0, 0.0, 1920.0, 1080.0),
    ];
    assert!(window_rect_visible_on_any_monitor((-1900.0, 100.0, 1280.0, 720.0), &monitors));
}

#[test]
fn sliver_smaller_than_threshold_is_invisible() {
    // Only 30 physical px of the window's left edge straddle the screen —
    // not enough to grab, so treat it as off-screen.
    let monitors = [(0.0, 0.0, 1920.0, 1080.0)];
    assert!(!window_rect_visible_on_any_monitor((-1250.0, 100.0, 1280.0, 720.0), &monitors));
}

#[test]
fn no_monitors_is_invisible() {
    // Callers treat an empty monitor list as "cannot verify" and keep the
    // saved position; the pure predicate itself reports invisibility.
    assert!(!window_rect_visible_on_any_monitor((0.0, 0.0, 1280.0, 720.0), &[]));
}
