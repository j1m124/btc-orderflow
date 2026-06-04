use gpui::{
    Action, Context, IntoElement, ParentElement as _, Render, SharedString, Styled as _, Window,
    div, px,
};
use gpui_component::{
    ActiveTheme as _, IconName, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use serde::Deserialize;

use crate::persistence::Mode;

/// Dispatched when the user clicks a mode button in the sidebar. The id is
/// `Mode::id()` (e.g. "charting"); the workspace converts back via
/// `Mode::from_id` and applies the switch.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = terminal_demo, no_json)]
pub struct SwitchMode(pub SharedString);

pub struct Sidebar {
    current: Mode,
}

impl Sidebar {
    pub fn new(current: Mode, _window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { current }
    }

    pub fn set_current(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.current == mode {
            return;
        }
        self.current = mode;
        cx.notify();
    }
}

fn icon_for(mode: Mode) -> IconName {
    match mode {
        Mode::Charting => IconName::GalleryVerticalEnd,
        Mode::Signal => IconName::Asterisk,
        Mode::Research => IconName::BookOpen,
        Mode::Portfolio => IconName::ChartPie,
        Mode::FreeLayout => IconName::LayoutDashboard,
    }
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let current = self.current;
        let mut col = v_flex()
            .h_full()
            .w(px(48.))
            .py_2()
            .gap_1()
            .items_center()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.sidebar);

        for mode in Mode::ALL {
            let mode = *mode;
            let is_active = mode == current;
            let id = SharedString::from(mode.id());
            let mut btn = Button::new(SharedString::from(format!("sidebar-{}", mode.id())))
                .icon(icon_for(mode))
                .tooltip(mode.display())
                // Rounded-rectangle selection fill. 10px reads as a soft
                // rectangle on a 36px button (well short of a pill).
                .rounded(px(10.))
                .on_click(move |_, window, cx| {
                    window.dispatch_action(Box::new(SwitchMode(id.clone())), cx);
                });
            // Active button gets a filled variant; inactive stays ghost.
            btn = if is_active {
                btn.primary()
            } else {
                btn.ghost()
            };
            col = col.child(
                h_flex()
                    .w(px(36.))
                    .h(px(36.))
                    .items_center()
                    .justify_center()
                    .child(btn),
            );
        }

        // Flexible spacer pushes the bottom-rail buttons (notifications,
        // settings) to the foot of the sidebar regardless of how many
        // mode buttons are above them.
        col = col.child(div().flex_1());

        // Notifications: placeholder bell for now — clicking shows a
        // "coming soon" toast via gpui-component's notification system.
        // Hook this up to a real notifications surface when one exists.
        let notifications_btn = Button::new("sidebar-notifications")
            .icon(IconName::Bell)
            .ghost()
            .rounded(px(10.))
            .tooltip("Notifications")
            .on_click(|_, window, cx| {
                window.push_notification(
                    gpui_component::notification::Notification::info("Notifications coming soon"),
                    cx,
                );
            });
        col = col.child(
            h_flex()
                .w(px(36.))
                .h(px(36.))
                .items_center()
                .justify_center()
                .child(notifications_btn),
        );

        // Profile: opens a dialog with email, current plan, view-profile +
        // bug-report shortcuts, and a sign-out button. Sits between
        // notifications and settings in the bottom rail.
        let profile_btn = Button::new("sidebar-profile")
            .icon(IconName::CircleUser)
            .ghost()
            .rounded(px(10.))
            .tooltip("Account")
            .on_click(|_, window, cx| {
                crate::profile::open_profile_dialog(window, cx);
            });
        col = col.child(
            h_flex()
                .w(px(36.))
                .h(px(36.))
                .items_center()
                .justify_center()
                .child(profile_btn),
        );

        let settings_btn = Button::new("sidebar-settings")
            .icon(IconName::Settings)
            .ghost()
            .rounded(px(10.))
            .tooltip("Settings")
            .on_click(|_, window, cx| {
                crate::top_bar::open_settings_dialog(window, cx);
            });
        col.child(
            h_flex()
                .w(px(36.))
                .h(px(36.))
                .items_center()
                .justify_center()
                .child(settings_btn),
        )
    }
}
