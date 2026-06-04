//! Workspace-wide source of truth for user drawings.
//!
//! Drawings are stored keyed by symbol (each drawing is anchored in wall-clock
//! ms, so a single drawing is naturally visible on every chart of that symbol
//! at every timeframe — see [`Drawing::tf_filter`] for the optional per-TF
//! restriction). One service instance lives on `App` as a [`Global`] and is
//! mutated through [`Entity`] update calls, mirroring the existing
//! [`crate::services::watchlist`] / [`crate::services::recents`] pattern.
//!
//! Persistence: every mutation calls [`DrawingService::persist`], which writes
//! a versioned JSON blob (`terminal_demo.drawings.v1`). Save is synchronous
//! per-call — drawings change at human pace, not 60 Hz, so a debounce isn't
//! worth its complexity yet. Edit-drag broadcasts (Q8) re-enter this code at
//! 60 Hz; if persistence becomes hot we'll add a 500 ms debounce identical to
//! the layout saver.

use std::collections::{BTreeMap, BTreeSet};

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString};

use crate::persistence;
use crate::services::market_data::Timeframe;

pub use super::shapes::{Drawing, DrawingOrigin, DrawingShape, LineRectShape, PositionShape, TextShape};

pub type DrawingId = u64;

#[derive(Clone, Debug)]
pub enum DrawingEvent {
    /// Drawings on `symbol` changed (any add/remove/update/visibility/tf-filter
    /// mutation). Subscribers should filter by symbol before re-rendering.
    Changed { symbol: SharedString },
    /// Mass change — every symbol's drawings were affected. Subscribers should
    /// always re-render. Emitted only by [`DrawingService::clear_all`].
    Wiped,
    /// Selection moved. Subscribers should re-render iff the change involves
    /// their symbol (either the old or new selection symbol).
    SelectionChanged,
}

pub struct DrawingService {
    by_symbol: BTreeMap<SharedString, Vec<Drawing>>,
    selected: Option<(SharedString, DrawingId)>,
    next_id: u64,
}

impl EventEmitter<DrawingEvent> for DrawingService {}

impl DrawingService {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        let doc = persistence::load_drawings();
        let mut by_symbol: BTreeMap<SharedString, Vec<Drawing>> = BTreeMap::new();
        for (sym, drawings) in doc.by_symbol {
            by_symbol.insert(SharedString::from(sym), drawings);
        }
        // `next_id` must be > every persisted id, otherwise a freshly-created
        // drawing could collide with one loaded from disk.
        let max_persisted = by_symbol
            .values()
            .flat_map(|v| v.iter().map(|d| d.id))
            .max()
            .unwrap_or(0);
        let next_id = doc.next_id.max(max_persisted + 1).max(1);
        Self {
            by_symbol,
            selected: None,
            next_id,
        }
    }

    pub fn for_symbol(&self, symbol: &str) -> &[Drawing] {
        match self.by_symbol.get(symbol) {
            Some(v) => v.as_slice(),
            None => &[],
        }
    }

    /// Visit drawings across every symbol — used by the global-cleanup path
    /// (`ClearAllDrawings`) and any future cross-symbol UI.
    pub fn by_symbol(&self) -> &BTreeMap<SharedString, Vec<Drawing>> {
        &self.by_symbol
    }

    pub fn selected(&self) -> Option<&(SharedString, DrawingId)> {
        self.selected.as_ref()
    }

    /// Resolve the currently-selected drawing if it still exists in the
    /// service. Returns `None` if selection points to a deleted id (which can
    /// happen after a concurrent remove).
    pub fn selected_drawing(&self) -> Option<(&SharedString, &Drawing)> {
        let (sym, id) = self.selected.as_ref()?;
        let d = self.by_symbol.get(sym)?.iter().find(|d| d.id == *id)?;
        Some((sym, d))
    }

    pub fn set_selected(
        &mut self,
        sel: Option<(SharedString, DrawingId)>,
        cx: &mut Context<Self>,
    ) {
        if self.selected == sel {
            return;
        }
        self.selected = sel;
        cx.emit(DrawingEvent::SelectionChanged);
        cx.notify();
    }

    /// Append a fresh drawing built from `shape`. Returns the assigned id so
    /// the caller can keep their newly-created object focused or selected.
    /// Origin defaults to `User`; the AI tool dispatcher uses `add_with_origin`
    /// to tag its drawings.
    pub fn add(
        &mut self,
        symbol: SharedString,
        shape: DrawingShape,
        cx: &mut Context<Self>,
    ) -> DrawingId {
        self.add_with_origin(symbol, shape, DrawingOrigin::User, cx)
    }

    /// Like [`Self::add`] but stamps a specific provenance on the new
    /// drawing. Used by the AI tool dispatcher.
    pub fn add_with_origin(
        &mut self,
        symbol: SharedString,
        shape: DrawingShape,
        created_by: DrawingOrigin,
        cx: &mut Context<Self>,
    ) -> DrawingId {
        let id = self.next_id;
        self.next_id += 1;
        let drawing = Drawing {
            id,
            hidden: false,
            tf_filter: None,
            created_by,
            shape,
        };
        self.by_symbol.entry(symbol.clone()).or_default().push(drawing);
        self.persist();
        cx.emit(DrawingEvent::Changed { symbol });
        cx.notify();
        id
    }

    /// Replace the shape of an existing drawing and persist immediately. Use
    /// this for commit-points (mouse-up after edit, programmatic edits). For
    /// the in-flight intermediate updates emitted at 60 Hz during an active
    /// drag, prefer [`Self::preview_shape`] to avoid hammering localStorage.
    pub fn update_shape(
        &mut self,
        symbol: &str,
        id: DrawingId,
        shape: DrawingShape,
        cx: &mut Context<Self>,
    ) {
        if !self.write_shape(symbol, id, shape) {
            return;
        }
        self.persist();
        cx.emit(DrawingEvent::Changed {
            symbol: SharedString::from(symbol.to_string()),
        });
        cx.notify();
    }

    /// Live edit-drag broadcast: rewrite the shape in memory + notify
    /// subscribers, but skip persistence (the eventual mouse-up calls
    /// [`Self::update_shape`] to flush). Used so two same-symbol charts can
    /// see the drag in real time without writing every mouse-move to disk.
    pub fn preview_shape(
        &mut self,
        symbol: &str,
        id: DrawingId,
        shape: DrawingShape,
        cx: &mut Context<Self>,
    ) {
        if !self.write_shape(symbol, id, shape) {
            return;
        }
        cx.emit(DrawingEvent::Changed {
            symbol: SharedString::from(symbol.to_string()),
        });
        cx.notify();
    }

    fn write_shape(&mut self, symbol: &str, id: DrawingId, shape: DrawingShape) -> bool {
        let Some(list) = self.by_symbol.get_mut(symbol) else {
            return false;
        };
        let Some(d) = list.iter_mut().find(|d| d.id == id) else {
            return false;
        };
        d.shape = shape;
        true
    }

    /// Remove a single drawing. Returns true if it was present. Clears the
    /// global selection if it matched.
    pub fn delete(&mut self, symbol: &str, id: DrawingId, cx: &mut Context<Self>) -> bool {
        let Some(list) = self.by_symbol.get_mut(symbol) else {
            return false;
        };
        let before = list.len();
        list.retain(|d| d.id != id);
        let removed = list.len() != before;
        if list.is_empty() {
            self.by_symbol.remove(symbol);
        }
        if !removed {
            return false;
        }
        if matches!(&self.selected, Some((s, sel_id)) if s.as_ref() == symbol && *sel_id == id) {
            self.selected = None;
            cx.emit(DrawingEvent::SelectionChanged);
        }
        self.persist();
        cx.emit(DrawingEvent::Changed {
            symbol: SharedString::from(symbol.to_string()),
        });
        cx.notify();
        true
    }

    pub fn delete_selected(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((symbol, id)) = self.selected.clone() else {
            return false;
        };
        self.delete(symbol.as_ref(), id, cx)
    }

    pub fn toggle_hidden(&mut self, symbol: &str, id: DrawingId, cx: &mut Context<Self>) {
        let Some(list) = self.by_symbol.get_mut(symbol) else {
            return;
        };
        let Some(d) = list.iter_mut().find(|d| d.id == id) else {
            return;
        };
        d.hidden = !d.hidden;
        self.persist();
        cx.emit(DrawingEvent::Changed {
            symbol: SharedString::from(symbol.to_string()),
        });
        cx.notify();
    }

    /// Flip a single TF in a drawing's filter. `None` (all-visible) flips to
    /// `Some({all-except-tf})`; toggling the last remaining TF off re-collapses
    /// to `None` (matches user intent: empty filter ≡ "show everywhere").
    pub fn toggle_tf_filter(
        &mut self,
        symbol: &str,
        id: DrawingId,
        tf: Timeframe,
        cx: &mut Context<Self>,
    ) {
        let Some(list) = self.by_symbol.get_mut(symbol) else {
            return;
        };
        let Some(d) = list.iter_mut().find(|d| d.id == id) else {
            return;
        };
        let tf_str = tf.as_str().to_string();
        let mut set = d.tf_filter.clone().unwrap_or_else(|| {
            Timeframe::ALL
                .iter()
                .map(|t| t.as_str().to_string())
                .collect::<BTreeSet<_>>()
        });
        if set.contains(&tf_str) {
            set.remove(&tf_str);
        } else {
            set.insert(tf_str);
        }
        // Re-collapse "all 5 selected" to None so the JSON stays compact +
        // the default-all semantic remains the canonical representation.
        let all_count = Timeframe::ALL.len();
        d.tf_filter = if set.len() == all_count { None } else { Some(set) };
        self.persist();
        cx.emit(DrawingEvent::Changed {
            symbol: SharedString::from(symbol.to_string()),
        });
        cx.notify();
    }

    pub fn reset_tf_filter(
        &mut self,
        symbol: &str,
        id: DrawingId,
        cx: &mut Context<Self>,
    ) {
        let Some(list) = self.by_symbol.get_mut(symbol) else {
            return;
        };
        let Some(d) = list.iter_mut().find(|d| d.id == id) else {
            return;
        };
        if d.tf_filter.is_none() {
            return;
        }
        d.tf_filter = None;
        self.persist();
        cx.emit(DrawingEvent::Changed {
            symbol: SharedString::from(symbol.to_string()),
        });
        cx.notify();
    }

    pub fn clear_symbol(&mut self, symbol: &str, cx: &mut Context<Self>) -> bool {
        let removed = self.by_symbol.remove(symbol).is_some();
        if !removed {
            return false;
        }
        if matches!(&self.selected, Some((s, _)) if s.as_ref() == symbol) {
            self.selected = None;
            cx.emit(DrawingEvent::SelectionChanged);
        }
        self.persist();
        cx.emit(DrawingEvent::Changed {
            symbol: SharedString::from(symbol.to_string()),
        });
        cx.notify();
        true
    }

    pub fn clear_all(&mut self, cx: &mut Context<Self>) -> bool {
        if self.by_symbol.is_empty() {
            return false;
        }
        self.by_symbol.clear();
        self.selected = None;
        // Full wipe → restart the #N numbering. Per-symbol clear keeps the
        // counter so newly-created drawings on other symbols don't reuse
        // ids that may still appear in an external backup of this file.
        self.next_id = 1;
        self.persist();
        cx.emit(DrawingEvent::Wiped);
        cx.emit(DrawingEvent::SelectionChanged);
        cx.notify();
        true
    }

    /// Force-write the current in-memory state to persistence. Used by the
    /// chart's mouse-up after an edit-drag that streamed through
    /// [`Self::preview_shape`] (which deliberately skips disk I/O).
    pub fn flush_persist(&self) {
        self.persist();
    }

    /// Update the optional label on a horizontal-ray drawing. No-op if the
    /// drawing isn't a horizontal ray (caller is responsible for routing).
    pub fn update_ray_text(
        &mut self,
        symbol: &str,
        id: DrawingId,
        text: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(list) = self.by_symbol.get_mut(symbol) else {
            return;
        };
        let Some(d) = list.iter_mut().find(|d| d.id == id) else {
            return;
        };
        if let DrawingShape::HorizontalRay(r) = &mut d.shape {
            r.text = text;
            self.persist();
            cx.emit(DrawingEvent::Changed {
                symbol: SharedString::from(symbol.to_string()),
            });
            cx.notify();
        }
    }

    /// Build a persistence document mirror and write it. Called from every
    /// mutating method.
    fn persist(&self) {
        let doc = persistence::PersistedDrawingsDoc {
            next_id: self.next_id,
            by_symbol: self
                .by_symbol
                .iter()
                .map(|(sym, drawings)| (sym.to_string(), drawings.clone()))
                .collect(),
        };
        if let Err(err) = persistence::save_drawings(&doc) {
            log::warn!("save drawings failed: {err:?}");
        }
    }
}

#[derive(Clone)]
pub struct DrawingServiceHandle(pub Entity<DrawingService>);
impl Global for DrawingServiceHandle {}

pub fn init(cx: &mut App) {
    let entity = cx.new(DrawingService::new);
    cx.set_global(DrawingServiceHandle(entity));
}
