//! `SettingsForm` builder + renderer. Per-surface code (indicator settings,
//! footprint settings, drawing settings) builds one of these via
//! `SettingsForm::new(...)` and calls `.render(...)` inside its hosted view.
//!
//! The form does NOT own any view-level state — input/color picker entities
//! live as keyed states on the window (`window.use_keyed_state(...)`) so
//! re-rendering the form doesn't drop them. The form is therefore safe to
//! rebuild on every `Render::render` call.

use gpui::{
    AnyElement, App, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
    prelude::FluentBuilder as _,
};
use gpui_component::{ActiveTheme as _, StyledExt as _, v_flex};

use super::field::{Field, FieldKind};
use super::widgets;

/// One group of fields. Renders as a section under its title; in multi-
/// group forms it becomes a sidebar entry that swaps the content pane.
pub struct SettingsGroup {
    pub(crate) title: SharedString,
    pub(crate) fields: Vec<Field>,
}

impl SettingsGroup {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            fields: Vec::new(),
        }
    }

    pub fn item(mut self, field: Field) -> Self {
        self.fields.push(field);
        self
    }

    pub fn items(mut self, fields: impl IntoIterator<Item = Field>) -> Self {
        self.fields.extend(fields);
        self
    }
}

/// Declarative settings form. Top-level container; one per subject (active
/// indicator, footprint render kind, drawing shape).
pub struct SettingsForm {
    /// Stable identifier (e.g., "indicator-42") used to key per-field
    /// input/color state across re-renders. Different forms get different
    /// keys so the keyed state survives retarget without leaking.
    pub(crate) form_id: SharedString,
    pub(crate) groups: Vec<SettingsGroup>,
}

impl SettingsForm {
    pub fn new(form_id: impl Into<SharedString>) -> Self {
        Self {
            form_id: form_id.into(),
            groups: Vec::new(),
        }
    }

    pub fn group(mut self, group: SettingsGroup) -> Self {
        self.groups.push(group);
        self
    }

    /// Render the form. Picks single-group vs sidebar layout based on
    /// group count. Returns an `AnyElement` ready to drop into a
    /// `FloatingWindow` body.
    pub fn render(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        if self.groups.is_empty() {
            return widgets::muted_message("No settings", cx).into_any_element();
        }
        if self.groups.len() == 1 {
            self.render_single_group(window, cx)
        } else {
            self.render_with_sidebar(window, cx)
        }
    }

    fn render_single_group(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        let group = &self.groups[0];
        let rows = self.render_group_rows(0, group, window, cx);
        v_flex()
            .id(SharedString::from(format!("{}-body", self.form_id)))
            .size_full()
            .child(
                div()
                    .id(SharedString::from(format!("{}-scroll", self.form_id)))
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(v_flex().w_full().p_4().gap_3().child(rows)),
            )
            .into_any_element()
    }

    fn render_with_sidebar(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        let active_key = SharedString::from(format!("{}-active-group", self.form_id));
        let active_state = window.use_keyed_state(active_key, cx, |_, _| 0usize);
        let active_ix = *active_state.read(cx);
        let active_ix = active_ix.min(self.groups.len() - 1);

        let theme_border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let fg = cx.theme().foreground;
        let primary = cx.theme().primary;

        let mut sidebar = v_flex()
            .w(px(140.))
            .h_full()
            .p_2()
            .gap_1()
            .border_r_1()
            .border_color(theme_border);
        for (ix, group) in self.groups.iter().enumerate() {
            let is_active = ix == active_ix;
            let label_color = if is_active { primary } else { muted };
            let label = group.title.clone();
            let state = active_state.clone();
            let item_id = SharedString::from(format!("{}-side-{}", self.form_id, ix));
            sidebar = sidebar.child(
                div()
                    .id(item_id)
                    .px_2()
                    .py_1()
                    .text_sm()
                    .text_color(label_color)
                    .when(is_active, |this| this.font_semibold())
                    .cursor_pointer()
                    .hover(|this| this.text_color(fg))
                    .child(label)
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        move |_, _w, cx| {
                            cx.stop_propagation();
                            let _ = state.update(cx, |v, _| *v = ix);
                        },
                    ),
            );
        }

        let content_rows = self.render_group_rows(active_ix, &self.groups[active_ix], window, cx);
        let content = div()
            .id(SharedString::from(format!("{}-scroll-{}", self.form_id, active_ix)))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(v_flex().w_full().p_4().gap_3().child(content_rows));

        gpui_component::h_flex()
            .size_full()
            .child(sidebar)
            .child(content)
            .into_any_element()
    }

    fn render_group_rows(
        &self,
        group_ix: usize,
        group: &SettingsGroup,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let mut rows = v_flex().w_full().gap_2();
        for (field_ix, field) in group.fields.iter().enumerate() {
            if let Some(predicate) = &field.visible_if {
                if !predicate(cx) {
                    continue;
                }
            }
            let row_key = SharedString::from(format!(
                "{}-g{}-f{}",
                self.form_id, group_ix, field_ix
            ));
            rows = rows.child(self.render_field(field, row_key, window, cx));
        }
        rows.into_any_element()
    }

    fn render_field(
        &self,
        field: &Field,
        row_key: SharedString,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let label = widgets::label_with_tooltip(
            field.label.clone(),
            field.description.clone(),
            row_key.clone(),
            cx,
        );
        let control = match &field.kind {
            FieldKind::Dropdown { options, get, set } => widgets::render_dropdown(
                row_key.clone(),
                options.clone(),
                get.clone(),
                set.clone(),
                cx,
            ),
            FieldKind::Number { opts, get, set } => widgets::render_number(
                row_key.clone(),
                opts.clone(),
                get.clone(),
                set.clone(),
                window,
                cx,
            ),
            FieldKind::Slider { opts, get, set } => widgets::render_slider(
                row_key.clone(),
                opts.clone(),
                get.clone(),
                set.clone(),
                window,
                cx,
            ),
            FieldKind::Switch { get, set } => {
                widgets::render_switch(row_key.clone(), get.clone(), set.clone(), cx)
            }
            FieldKind::Checkbox { get, set } => {
                widgets::render_checkbox(row_key.clone(), get.clone(), set.clone(), cx)
            }
            FieldKind::MultiCheckbox { items } => {
                widgets::render_multi_checkbox(row_key.clone(), items, cx)
            }
            FieldKind::Color { get, set } => widgets::render_color(
                row_key.clone(),
                get.clone(),
                set.clone(),
                window,
                cx,
            ),
            FieldKind::Text { get, set } => widgets::render_text(
                row_key.clone(),
                get.clone(),
                set.clone(),
                window,
                cx,
            ),
            FieldKind::Action {
                button_label,
                on_click,
            } => widgets::render_action(
                row_key.clone(),
                button_label.clone(),
                on_click.clone(),
                cx,
            ),
        };
        widgets::row(label, control, cx).into_any_element()
    }
}
