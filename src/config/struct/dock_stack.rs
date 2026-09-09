//! Docked-panel edge stacks (#dock-stack).
//!
//! Window-level panels (sidebar / welcome / quick / ai) can share the same
//! window edge at the same time, stacked along that edge. A `DockEdgeSer`
//! records, per edge, the ordered list of simultaneously-expanded panels and
//! how that edge's secondary axis is divided between them. Legacy configs have
//! an empty list → the old single-panel-per-edge layout keeps working.

use serde::{Deserialize, Serialize};

/// One panel in an edge stack. `kind` is one of
/// `"sidebar" | "welcome" | "quick" | "ai"`; `ratio` divides the edge's
/// secondary axis among the stacked panels (0..1 — the stack is renormalised
/// when loaded, so stale ratios can never sum away from 1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DockSlotSer {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub ratio: f32,
}

/// The ordered stack of simultaneously-expanded panels on one window edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DockEdgeSer {
    /// `"left" | "right" | "top" | "bottom"` — the edge this stack owns.
    #[serde(default)]
    pub edge: String,
    #[serde(default)]
    pub slots: Vec<DockSlotSer>,
}

pub(crate) fn valid_edge(edge: &str) -> bool {
    matches!(edge, "left" | "right" | "top" | "bottom")
}

pub(crate) fn valid_kind(kind: &str) -> bool {
    matches!(kind, "sidebar" | "welcome" | "quick" | "ai")
}

/// Normalise one edge stack so it is safe to apply:
/// - drops unknown/empty kinds and out-of-range ratios;
/// - `sidebar` and `welcome` are the same session list in two forms, so at
///   most one of each survives (first one wins);
/// - the surviving ratios are clamped to a sane minimum and renormalised to
///   sum to 1, so a corrupt config can never gap or overflow the edge.
pub(crate) fn sanitize_edge(mut edge: DockEdgeSer) -> Option<DockEdgeSer> {
    if !valid_edge(&edge.edge) {
        return None;
    }
    edge.slots.retain(|s| valid_kind(&s.kind) && s.ratio.is_finite() && s.ratio > 0.0);
    if edge.slots.len() < 2 {
        // A single-panel stack is just the legacy layout; do not store it.
        return None;
    }
    let mut seen = std::collections::HashSet::new();
    let mut dedup = Vec::with_capacity(edge.slots.len());
    for s in edge.slots {
        if seen.insert(s.kind.clone()) {
            dedup.push(s);
        }
    }
    if dedup.len() < 2 {
        return None;
    }
    let total: f32 = dedup.iter().map(|s| s.ratio).sum();
    for s in &mut dedup {
        s.ratio = (s.ratio / total).clamp(0.05, 0.95);
    }
    // Renormalise again after clamping so the sum is exactly 1.
    let total: f32 = dedup.iter().map(|s| s.ratio).sum();
    if total <= 0.0 {
        return None;
    }
    for s in &mut dedup {
        s.ratio = (s.ratio / total).clamp(0.0, 1.0);
    }
    edge.slots = dedup;
    Some(edge)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(kind: &str, ratio: f32) -> DockSlotSer {
        DockSlotSer {
            kind: kind.to_string(),
            ratio,
        }
    }

    fn edge(edge: &str, slots: Vec<DockSlotSer>) -> DockEdgeSer {
        DockEdgeSer {
            edge: edge.to_string(),
            slots,
        }
    }

    #[test]
    fn serde_roundtrip_preserves_stacks() {
        let cfg = crate::config::ConfigFile {
            dock_stacks: vec![edge(
                "left",
                vec![slot("sidebar", 0.7), slot("quick", 0.3)],
            )],
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: crate::config::ConfigFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.dock_stacks, cfg.dock_stacks);
    }

    #[test]
    fn missing_field_defaults_to_empty_stacks() {
        let back: crate::config::ConfigFile = serde_json::from_str("{}").unwrap();
        assert!(back.dock_stacks.is_empty());
    }

    #[test]
    fn sanitize_keeps_valid_edge_and_renormalises() {
        let out = sanitize_edge(edge("right", vec![slot("ai", 1.0), slot("quick", 1.0)]))
            .expect("valid two-panel stack survives");
        assert_eq!(out.edge, "right");
        assert_eq!(out.slots.len(), 2);
        assert!((out.slots.iter().map(|s| s.ratio).sum::<f32>() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn sanitize_drops_unknown_kinds() {
        let out = sanitize_edge(edge(
            "bottom",
            vec![slot("sidebar", 0.5), slot("bogus", 0.5), slot("ai", 0.5)],
        ))
        .expect("survives after dropping the unknown kind");
        // ["bogus"] filtered → sidebar + ai remain, renormalised.
        assert_eq!(out.slots.len(), 2);
        assert!(out.slots.iter().all(|s| matches!(s.kind.as_str(), "sidebar" | "ai")));
    }

    #[test]
    fn sanitize_deduplicates_sidebar_and_welcome() {
        let out = sanitize_edge(edge(
            "left",
            vec![slot("welcome", 0.5), slot("sidebar", 0.2), slot("quick", 0.3)],
        ))
        .expect("deduplicated stack survives");
        // First welcome survives, the later sidebar is dropped.
        assert_eq!(out.slots.len(), 2);
        assert_eq!(out.slots[0].kind, "welcome");
        assert_eq!(out.slots[1].kind, "quick");
    }

    #[test]
    fn sanitize_rejects_single_panel_or_bad_edge() {
        assert!(sanitize_edge(edge("left", vec![slot("sidebar", 1.0)])).is_none());
        assert!(sanitize_edge(edge("middle", vec![slot("sidebar", 0.5), slot("ai", 0.5)])).is_none());
        assert!(sanitize_edge(edge("left", Vec::new())).is_none());
        // Zero/negative ratios leave fewer than 2 valid panels → rejected.
        assert!(sanitize_edge(edge("left", vec![slot("sidebar", 0.0), slot("ai", 0.0)])).is_none());
    }

    #[test]
    fn sanitize_clamps_ratios_within_range() {
        let out = sanitize_edge(edge(
            "top",
            vec![slot("sidebar", 0.0001), slot("ai", 999.0)],
        ))
        .expect("stack survives");
        assert!(out.slots[0].ratio >= 0.05);
        assert!(out.slots[1].ratio <= 0.95);
        let sum: f32 = out.slots.iter().map(|s| s.ratio).sum();
        assert!((sum - 1.0).abs() < 1e-6);
    }
}