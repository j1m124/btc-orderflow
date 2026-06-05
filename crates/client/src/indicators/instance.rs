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
        let kind_id = kind.kind_id();
        let placement = match kind.pane_kind() {
            PaneKind::OverlayOnly | PaneKind::Both => Placement::Overlay,
            PaneKind::PaneOnly => Placement::Pane,
        };
        let pane_height = match placement {
            Placement::Pane => Some(default_pane_height(kind_id)),
            Placement::Overlay => None,
        };
        // Seed one Hsla per declared color slot. Slot 0 = primary palette
        // pick; subsequent slots fan out across the hue wheel so multi-line
        // kinds (MACD today, anything richer tomorrow) read distinctly out
        // of the box. Users can override any slot in the settings panel.
        let slot_count = kind.color_slots().len();
        let colors = (0..slot_count)
            .map(|i| derive_slot_default(primary_color, i))
            .collect();
        Self {
            id: new_instance_id(),
            kind_id,
            kind,
            placement,
            pane_height,
            colors,
            hidden: false,
        }
    }

    /// Read the color for `slot`, falling back to a hue-shifted default
    /// when the slot is out of bounds (defensive — shouldn't happen since
    /// `colors.len() == kind.color_slots().len()`).
    pub fn color_at(&self, slot: usize) -> Hsla {
        self.colors
            .get(slot)
            .copied()
            .unwrap_or_else(|| derive_slot_default(self.primary_color(), slot))
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

    /// Resize `colors` to match the kind's current `color_slots()` count.
    /// Slots beyond the new count are dropped; new slots are seeded via
    /// `derive_slot_default` from the primary color. Used by mutators
    /// that can change the slot count (MA Suite's add/remove entry).
    /// Existing slot colors are preserved at their indices — only the
    /// shape changes.
    pub fn sync_colors(&mut self) {
        let target = self.kind.color_slots().len();
        if target == self.colors.len() {
            return;
        }
        let primary = self.primary_color();
        if target < self.colors.len() {
            self.colors.truncate(target);
        } else {
            for slot in self.colors.len()..target {
                self.colors.push(derive_slot_default(primary, slot));
            }
        }
    }
}

/// Deterministic per-slot default color: slot 0 returns the primary
/// untouched, slot N adds N × half-rotations on the hue wheel with a
/// gentle alpha decay so secondary lines read as "supporting".
fn derive_slot_default(primary: Hsla, slot: usize) -> Hsla {
    if slot == 0 {
        return primary;
    }
    Hsla {
        h: (primary.h + 0.5 * slot as f32) % 1.0,
        a: primary.a * (0.85_f32).powi(slot as i32),
        ..primary
    }
}

/// Per-kind default sub-pane height (px). Histogram panes (volume, trades)
/// get a slim 90px slot; anything else falls back to 140px. Used only when
/// an instance lands in `Placement::Pane`.
pub fn default_pane_height(kind_id: &str) -> f32 {
    match kind_id {
        "volume" => 90.0,
        "trades" => 90.0,
        "volume_delta" => 120.0,
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
