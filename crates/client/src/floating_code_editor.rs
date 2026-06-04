//! Free-Layout-only floating code editor — a placeholder surface for the
//! future scripting feature. Wraps a multi-line `InputState` with monospace
//! styling and pre-fills a hello-world comment so the window has visible
//! content out of the box.
//!
//! The action [`ToggleFloatingCodeEditor`] is dispatched by the "+ Panel"
//! menu entry; the workspace owns the singleton and the `FloatingWindow`
//! that hosts this view.

use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, IntoElement,
    ParentElement as _, Render, Styled as _, Window, actions, div,
};
use gpui_component::{
    ActiveTheme as _,
    input::{Input, InputState},
};

actions!(client, [ToggleFloatingCodeEditor]);

const PLACEHOLDER_SCRIPT: &str = "\
// Future scripting surface — under construction.
// Try typing here to feel out the floating window.

fn on_bar_close(bar) {
    if bar.close > bar.open {
        notify(\"green bar\");
    }
}
";

pub struct FloatingCodeEditor {
    input_state: Entity<InputState>,
}

impl FloatingCodeEditor {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input_state = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder("// scripting goes here")
                .default_value(PLACEHOLDER_SCRIPT)
        });
        Self { input_state }
    }
}

impl Focusable for FloatingCodeEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input_state.read(cx).focus_handle(cx)
    }
}

impl Render for FloatingCodeEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        // `h_full()` is required for multi-line `Input` to fill the parent
        // height — without it the input keeps its natural-row height and the
        // floating window shows empty space below.
        div()
            .size_full()
            .font_family(mono)
            .text_sm()
            .child(Input::new(&self.input_state).h_full())
    }
}
