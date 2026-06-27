//! Field declarations. A `Field` is one row in the form: a label + optional
//! tooltip description + a typed control. Closures are stored as `Rc<dyn
//! Fn(...)>` so the form is `Clone`-friendly and field declarations can
//! survive across renders.
//!
//! Each closure pair reads from / writes to App-global state. Per-kind code
//! typically builds these via [`IndicatorTarget::typed`] etc., which captures
//! a `WeakEntity<ContentPanel>` + an instance id and routes the read/write
//! through `chart.update_indicator` + a downcast on `kind.as_any_mut()`.

use std::rc::Rc;

use gpui::{App, Hsla, SharedString};

/// A single row in the form. Label sits in the left column; `description`
/// becomes a tooltip on a small ⓘ icon next to the label. `visible_if`
/// gates the entire row at render time.
pub struct Field {
    pub(crate) label: SharedString,
    pub(crate) description: Option<SharedString>,
    pub(crate) visible_if: Option<Rc<dyn Fn(&App) -> bool>>,
    pub(crate) kind: FieldKind,
}

impl Field {
    pub fn description(mut self, text: impl Into<SharedString>) -> Self {
        self.description = Some(text.into());
        self
    }

    /// Hide the row when the predicate returns false. Re-evaluated on every
    /// render against the current App state; useful for "POC color" rows
    /// that should only show when `show_poc` is on.
    pub fn visible_if<F>(mut self, predicate: F) -> Self
    where
        F: Fn(&App) -> bool + 'static,
    {
        self.visible_if = Some(Rc::new(predicate));
        self
    }
}

/// One field-kind variant per shape of control. Each carries its own typed
/// read / write closure pair (or a list of sub-items for multi-checkbox).
#[allow(clippy::type_complexity)]
pub enum FieldKind {
    Dropdown {
        options: Vec<DropdownOption>,
        get: Rc<dyn Fn(&App) -> SharedString>,
        set: Rc<dyn Fn(SharedString, &mut App)>,
    },
    Number {
        opts: NumberOpts,
        get: Rc<dyn Fn(&App) -> f64>,
        set: Rc<dyn Fn(f64, &mut App)>,
    },
    /// A horizontal slider over `opts.min..=opts.max` with a live value readout
    /// (formatted via the same `format`/`suffix` as a number field). Suits
    /// bounded ratios like percentages where dragging beats typing.
    Slider {
        opts: NumberOpts,
        get: Rc<dyn Fn(&App) -> f64>,
        set: Rc<dyn Fn(f64, &mut App)>,
    },
    Switch {
        get: Rc<dyn Fn(&App) -> bool>,
        set: Rc<dyn Fn(bool, &mut App)>,
    },
    Checkbox {
        get: Rc<dyn Fn(&App) -> bool>,
        set: Rc<dyn Fn(bool, &mut App)>,
    },
    MultiCheckbox {
        items: Vec<MultiCheckItem>,
    },
    Color {
        get: Rc<dyn Fn(&App) -> Hsla>,
        set: Rc<dyn Fn(Hsla, &mut App)>,
    },
    Text {
        get: Rc<dyn Fn(&App) -> SharedString>,
        set: Rc<dyn Fn(SharedString, &mut App)>,
    },
    Action {
        button_label: SharedString,
        on_click: Rc<dyn Fn(&mut App)>,
    },
}

/// One option in a dropdown menu. `value` is the canonical encoding the
/// getter / setter exchange (typically the rust enum variant's `as_str()`
/// representation); `label` is the human-readable display.
#[derive(Clone)]
pub struct DropdownOption {
    pub value: SharedString,
    pub label: SharedString,
}

impl DropdownOption {
    pub fn new(value: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }
}

/// Bounds + step for the number input. `format` controls only the *numeric*
/// rendering of the editable value (e.g. fixed decimals) — it must NOT embed a
/// unit, since it becomes the text inside the input box. The unit belongs in
/// [`NumberOpts::suffix`], which renders as a muted label *beside* the input so
/// the editable field stays a bare number.
#[derive(Clone)]
pub struct NumberOpts {
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub format: Option<Rc<dyn Fn(f64) -> SharedString>>,
    /// Unit shown next to (not inside) the input — e.g. `%`, `ticks`, `h`.
    pub suffix: Option<SharedString>,
}

impl Default for NumberOpts {
    fn default() -> Self {
        Self {
            min: f64::MIN,
            max: f64::MAX,
            step: 1.0,
            format: None,
            suffix: None,
        }
    }
}

impl NumberOpts {
    /// Integer-typed input: integral step, integral formatting.
    pub fn int(min: i64, max: i64) -> Self {
        Self {
            min: min as f64,
            max: max as f64,
            step: 1.0,
            format: None,
            suffix: None,
        }
    }

    pub fn float(min: f64, max: f64, step: f64) -> Self {
        Self {
            min,
            max,
            step,
            format: None,
            suffix: None,
        }
    }

    pub fn with_step(mut self, step: f64) -> Self {
        self.step = step;
        self
    }

    pub fn format<F>(mut self, f: F) -> Self
    where
        F: Fn(f64) -> SharedString + 'static,
    {
        self.format = Some(Rc::new(f));
        self
    }

    /// Set the unit label rendered beside the input (`%`, `ticks`, `h`, …).
    /// Keeps the editable field a bare number while still showing the unit.
    pub fn suffix(mut self, unit: impl Into<SharedString>) -> Self {
        self.suffix = Some(unit.into());
        self
    }
}

/// One checkbox within a `MultiCheckbox` field. Each carries its own get /
/// set pair so the items can target unrelated bool fields on the same
/// params struct (e.g., `show_volume`, `show_delta`, …).
#[allow(clippy::type_complexity)]
pub struct MultiCheckItem {
    pub(crate) label: SharedString,
    pub(crate) description: Option<SharedString>,
    pub(crate) get: Rc<dyn Fn(&App) -> bool>,
    pub(crate) set: Rc<dyn Fn(bool, &mut App)>,
}

impl MultiCheckItem {
    pub fn new<G, S>(label: impl Into<SharedString>, get: G, set: S) -> Self
    where
        G: Fn(&App) -> bool + 'static,
        S: Fn(bool, &mut App) + 'static,
    {
        Self {
            label: label.into(),
            description: None,
            get: Rc::new(get),
            set: Rc::new(set),
        }
    }

    pub fn description(mut self, text: impl Into<SharedString>) -> Self {
        self.description = Some(text.into());
        self
    }
}

// ─────────────────────────── builders ───────────────────────────

impl Field {
    pub fn dropdown<G, S>(label: impl Into<SharedString>, options: Vec<DropdownOption>, get: G, set: S) -> Self
    where
        G: Fn(&App) -> SharedString + 'static,
        S: Fn(SharedString, &mut App) + 'static,
    {
        Self::with_kind(label, FieldKind::Dropdown {
            options,
            get: Rc::new(get),
            set: Rc::new(set),
        })
    }

    pub fn number<G, S>(label: impl Into<SharedString>, opts: NumberOpts, get: G, set: S) -> Self
    where
        G: Fn(&App) -> f64 + 'static,
        S: Fn(f64, &mut App) + 'static,
    {
        Self::with_kind(label, FieldKind::Number {
            opts,
            get: Rc::new(get),
            set: Rc::new(set),
        })
    }

    /// Bounded numeric value as a drag slider with a live readout. Same
    /// `NumberOpts` as [`Field::number`] (bounds / step / `format` / `suffix`).
    pub fn slider<G, S>(label: impl Into<SharedString>, opts: NumberOpts, get: G, set: S) -> Self
    where
        G: Fn(&App) -> f64 + 'static,
        S: Fn(f64, &mut App) + 'static,
    {
        Self::with_kind(label, FieldKind::Slider {
            opts,
            get: Rc::new(get),
            set: Rc::new(set),
        })
    }

    pub fn switch<G, S>(label: impl Into<SharedString>, get: G, set: S) -> Self
    where
        G: Fn(&App) -> bool + 'static,
        S: Fn(bool, &mut App) + 'static,
    {
        Self::with_kind(label, FieldKind::Switch {
            get: Rc::new(get),
            set: Rc::new(set),
        })
    }

    pub fn checkbox<G, S>(label: impl Into<SharedString>, get: G, set: S) -> Self
    where
        G: Fn(&App) -> bool + 'static,
        S: Fn(bool, &mut App) + 'static,
    {
        Self::with_kind(label, FieldKind::Checkbox {
            get: Rc::new(get),
            set: Rc::new(set),
        })
    }

    pub fn multi_checkbox(label: impl Into<SharedString>, items: Vec<MultiCheckItem>) -> Self {
        Self::with_kind(label, FieldKind::MultiCheckbox { items })
    }

    pub fn color<G, S>(label: impl Into<SharedString>, get: G, set: S) -> Self
    where
        G: Fn(&App) -> Hsla + 'static,
        S: Fn(Hsla, &mut App) + 'static,
    {
        Self::with_kind(label, FieldKind::Color {
            get: Rc::new(get),
            set: Rc::new(set),
        })
    }

    pub fn text<G, S>(label: impl Into<SharedString>, get: G, set: S) -> Self
    where
        G: Fn(&App) -> SharedString + 'static,
        S: Fn(SharedString, &mut App) + 'static,
    {
        Self::with_kind(label, FieldKind::Text {
            get: Rc::new(get),
            set: Rc::new(set),
        })
    }

    pub fn action<F>(
        label: impl Into<SharedString>,
        button_label: impl Into<SharedString>,
        on_click: F,
    ) -> Self
    where
        F: Fn(&mut App) + 'static,
    {
        Self::with_kind(label, FieldKind::Action {
            button_label: button_label.into(),
            on_click: Rc::new(on_click),
        })
    }

    fn with_kind(label: impl Into<SharedString>, kind: FieldKind) -> Self {
        Self {
            label: label.into(),
            description: None,
            visible_if: None,
            kind,
        }
    }
}
