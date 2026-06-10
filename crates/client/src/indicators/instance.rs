//! Per-instance state: identity, the boxed kind impl, placement, color,
//! pane height, hidden flag. The `kind` Box<dyn> carries dispatch + typed
//! params; the surrounding fields are presentation state.
//!
//! IDs are a session-local incrementing `u64` rather than a UUID — avoids
//! pulling in a uuid dep, and the value is just a serializable tag (the
//! persistence layer stores per-chart lists keyed by chart-id, so cross-
//! session collisions don't matter).

use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{Hsla, hsla};

use super::kind::{IndicatorKind, PaneKind, Placement};

pub type InstanceId = u64;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Mint a fresh per-session instance id.
pub fn new_instance_id() -> InstanceId {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Ensure subsequent calls to `new_instance_id` return values strictly
/// greater than `id`. Called by the persistence restore path so that
/// instance ids loaded from disk don't collide with freshly-minted ones.
pub fn bump_next_id_past(id: InstanceId) {
    let target = id.saturating_add(1);
    let mut cur = NEXT_ID.load(Ordering::Relaxed);
    while cur < target {
        match NEXT_ID.compare_exchange(cur, target, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(observed) => cur = observed,
        }
    }
}

/// One indicator on one chart panel. The `kind` Box owns the impl + its
/// typed params; the surrounding fields are presentation state. `kind_id`
/// mirrors `kind.kind_id()` so we can filter / route without dynamic
/// dispatch on every access.
pub struct IndicatorInstance {
    pub id: InstanceId,
    pub kind_id: &'static str,
    pub kind: Box<dyn IndicatorKind>,
    /// Only meaningful when `kind.pane_kind() == Both`. For OverlayOnly or
    /// PaneOnly kinds this is fixed to the matching variant.
    pub placement: Placement,
    /// Sub-pane height in pixels. `Some(_)` only when `placement == Pane`.
    pub pane_height: Option<f32>,
    /// Per-slot draw colors, indexed parallel to `kind.color_slots()`.
    /// Slot 0 is the primary line; additional slots back kinds with
    /// multiple distinct series (MACD: signal). Empty for kinds that
    /// declare no slots (Volume). Auto-seeded on construction from the
    /// kind's palette and a hue-shift rotation; each entry is overridable
    /// in the settings panel.
    pub colors: Vec<Hsla>,
    /// True when the user toggled visibility off (eye icon or context-menu
    /// `Hide`). Indicator still computes; paint skips it.
    pub hidden: bool,
}

impl IndicatorInstance {
    /// Create a fresh instance with the kind's default placement (overlay for
    /// OverlayOnly / Both kinds, pane for PaneOnly), a kind-specific default
    /// pane height where applicable, and per-slot colors seeded from
    /// `primary_color` + a hue-shift rotation for additional slots.
    pub fn new(kind: Box<dyn IndicatorKind>, primary_color: Hsla) -> Self {
        Self::new_with_id(new_instance_id(), kind, primary_color)
    }

    /// Like [`Self::new`] but adopts a caller-supplied `id`. Used by the
    /// persistence restore path so that drawings anchored to a
    /// `PaneRef::Indicator(InstanceId)` keep their target across reloads.
    /// The caller is responsible for bumping the global `NEXT_ID` past the
    /// id via [`bump_next_id_past`] to avoid future collisions.
    pub fn new_with_id(id: InstanceId, kind: Box<dyn IndicatorKind>, primary_color: Hsla) -> Self {
        let kind_id = kind.kind_id();
        let placement = match kind.pane_kind() {
            PaneKind::OverlayOnly | PaneKind::Both => Placement::Overlay,
            PaneKind::PaneOnly => Placement::Pane,
        };
        let pane_height = match placement {
            Placement::Pane => Some(default_pane_height(kind_id)),
            Placement::Overlay => None,
        };
        // Single primary slot, seeded from the per-kind palette rotation.
        // Multi-line kinds (none in v1; MA Suite when it lands) will store
        // their per-line colors in typed params and ignore this slot.
        let colors = vec![primary_color];
        Self {
            id,
            kind_id,
            kind,
            placement,
            pane_height,
            colors,
            hidden: false,
        }
    }

    /// Read the color for `slot`, falling back to the primary slot when
    /// the requested index is out of bounds. Defensive — paint code
    /// typically reads slot 0 only.
    pub fn color_at(&self, slot: usize) -> Hsla {
        self.colors
            .get(slot)
            .copied()
            .unwrap_or_else(|| self.primary_color())
    }

    /// Shortcut for slot 0 — the most common case. Returns a sensible
    /// palette color when the kind declares no slots (Volume), so callers
    /// that just want "the chip color" don't have to special-case.
    pub fn primary_color(&self) -> Hsla {
        self.colors
            .first()
            .copied()
            .unwrap_or_else(|| palette_color_for(0))
    }

    /// Resize hook retained as a no-op for callers that still invoke it
    /// (`chart.update_indicator` runs it after each typed mutation). With
    /// the per-kind slot count now driven by the settings_form
    /// declaration, the instance just stores a single primary color slot;
    /// no resizing happens here.
    pub fn sync_colors(&mut self) {}
}

// (derive_slot_default removed — only one slot is allocated now.)

/// Per-kind default sub-pane height (px). Histogram panes (volume, trades)
/// get a slim 90px slot; anything else falls back to 140px. Used only when
/// an instance lands in `Placement::Pane`.
pub fn default_pane_height(kind_id: &str) -> f32 {
    match kind_id {
        "volume" => 90.0,
        "trades" => 90.0,
        "volume_delta" => 120.0,
        // Two stacked text rows + a sliver of padding. The pane has no
        // y-axis (paint owns layout), so we don't need height for ticks.
        "bar_stat" => 48.0,
        _ => 140.0,
    }
}

/// Number of distinct colors in the auto-rotation palette. Per-kind rotation
/// indexes into this with `count % COLOR_PALETTE_SIZE`.
pub const COLOR_PALETTE_SIZE: usize = 8;

/// Pick the next palette color for a per-kind instance count (0-based,
/// wraps at 8). Approximate HSL values chosen to be visually distinct on
/// both light and dark themes. User can override per instance in settings.
pub fn palette_color_for(count: usize) -> Hsla {
    match count % COLOR_PALETTE_SIZE {
        0 => hsla(0.00, 0.85, 0.55, 1.0), // red
        1 => hsla(0.61, 0.80, 0.55, 1.0), // blue
        2 => hsla(0.36, 0.55, 0.50, 1.0), // green
        3 => hsla(0.08, 0.85, 0.55, 1.0), // orange
        4 => hsla(0.78, 0.55, 0.55, 1.0), // purple
        5 => hsla(0.50, 0.65, 0.55, 1.0), // cyan
        6 => hsla(0.90, 0.65, 0.55, 1.0), // magenta
        _ => hsla(0.48, 0.55, 0.42, 1.0), // teal
    }
}
