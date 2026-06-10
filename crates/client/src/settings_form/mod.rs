//! Standardized settings-form framework. The plan: every indicator,
//! footprint render, and drawing-shape settings window shares one declarative
//! layout — a table of rows shaped `[label + ⓘ tooltip] | [control]`, with
//! an optional sidebar when a form has more than one group.
//!
//! Per-surface (`indicator_settings.rs`, `panels/chart/footprint_settings.rs`,
//! `drawings/settings_view.rs`) keeps the floating-window wrapper +
//! retarget/dismiss plumbing; the body is built by calling
//! `SettingsForm::render` with the form declaration each surface fetches
//! from the active subject (`IndicatorKind::settings_form`, etc.).
//!
//! Field types are the eight locked during the grilling session: dropdown,
//! multi-checkbox, number, text, color, action, switch, checkbox.
//! `visible_if` predicates gate render-time visibility (re-evaluated each
//! frame against the current params snapshot).

pub mod field;
pub mod form;
pub mod target;
mod widgets;

pub use field::{DropdownOption, Field, FieldKind, MultiCheckItem, NumberOpts};
pub use form::{SettingsForm, SettingsGroup};
pub use target::{AfterChange, IndicatorTarget, inst_color_field, placement_field};
