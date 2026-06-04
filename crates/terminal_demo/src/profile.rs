//! User profile popover for the sidebar. Shows email + current plan, with
//! sign-out and bug-report shortcuts. The email and plan are read from the
//! JWT's payload (no signature verification — the server already validates
//! it; we just want the claims for display).

use gpui::{ParentElement as _, SharedString, Styled as _, Window, div, px};
use gpui_component::{
    ActiveTheme as _, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::{DialogButtonProps, DialogHeader, DialogTitle},
    h_flex,
    separator::Separator,
    v_flex,
};

use crate::net::CentoflowConfig;

/// Decode a JWT payload (middle segment, base64url-encoded JSON) and return it
/// as a `serde_json::Value`. Returns `None` on any parse failure — callers
/// treat missing claims as unknown.
fn decode_jwt_payload(token: &str) -> Option<serde_json::Value> {
    let payload_b64 = token.split('.').nth(1)?;
    let bytes = base64url_decode(payload_b64)?;
    serde_json::from_slice(&bytes).ok()
}

/// Minimal base64url decoder (RFC 4648 §5, padding optional). Inlined so this
/// module doesn't pull in a new crate dep just for one JWT segment.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for c in input.chars() {
        let v: u32 = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '-' => 62,
            '_' => 63,
            '=' => break,
            _ => return None,
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

/// Best-effort email extraction. Supabase/Clerk/Auth0 all put it under `email`
/// in the top-level claims; fall back to common alternatives just in case.
fn email_from_claims(claims: &serde_json::Value) -> Option<String> {
    for key in ["email", "user_email", "preferred_username"] {
        if let Some(s) = claims.get(key).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

/// Best-effort plan extraction. No canonical claim across providers, so we
/// probe a few likely keys and fall back to "Free" so the row is never blank.
fn plan_from_claims(claims: &serde_json::Value) -> String {
    for key in ["plan", "tier", "subscription"] {
        if let Some(s) = claims.get(key).and_then(|v| v.as_str()) {
            return s.to_string();
        }
    }
    "Free".to_string()
}

struct ProfileInfo {
    email: SharedString,
    plan: SharedString,
}

fn current_profile(cx: &gpui::App) -> ProfileInfo {
    let token = cx.global::<CentoflowConfig>().token.clone();
    let claims = token.as_deref().and_then(decode_jwt_payload);
    let email = claims
        .as_ref()
        .and_then(email_from_claims)
        .map(SharedString::from)
        .unwrap_or_else(|| SharedString::from("Not signed in"));
    let plan = claims
        .as_ref()
        .map(plan_from_claims)
        .map(SharedString::from)
        .unwrap_or_else(|| SharedString::from("—"));
    ProfileInfo { email, plan }
}

/// Open the profile popover. Mirrors the settings dialog pattern (compact
/// dialog with a "Done" footer button); rows are stacked plain `div`s with
/// muted labels on top and primary values below.
pub fn open_profile_dialog(window: &mut Window, cx: &mut gpui::App) {
    let info = current_profile(cx);
    window.open_dialog(cx, move |dialog, _, cx| {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let email = info.email.clone();
        let plan = info.plan.clone();

        let row = |label: &'static str, value: SharedString| {
            v_flex()
                .px_4()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(SharedString::from(label)),
                )
                .child(div().text_sm().child(value))
        };

        let profile_btn = Button::new("profile-view")
            .label("View profile")
            .icon(IconName::CircleUser)
            .ghost()
            .small()
            .on_click(|_, window, cx| {
                window.push_notification(
                    gpui_component::notification::Notification::info(
                        "Profile page coming soon",
                    ),
                    cx,
                );
            });

        let reset_password_btn = Button::new("profile-reset-password")
            .label("Reset password")
            .icon(IconName::Settings)
            .ghost()
            .small()
            .on_click(|_, window, cx| {
                window.push_notification(
                    gpui_component::notification::Notification::info(
                        "Password reset coming soon",
                    ),
                    cx,
                );
            });

        let bug_btn = Button::new("profile-bug-report")
            .label("Report a bug")
            .icon(IconName::TriangleAlert)
            .ghost()
            .small()
            .on_click(|_, window, cx| {
                window.push_notification(
                    gpui_component::notification::Notification::info(
                        "Bug report coming soon",
                    ),
                    cx,
                );
            });

        let signout_btn = Button::new("profile-signout")
            .label("Sign out")
            .danger()
            .small()
            .on_click(|_, window, cx| {
                crate::auth::logout(cx);
                window.close_dialog(cx);
                window.push_notification(
                    gpui_component::notification::Notification::success("Signed out"),
                    cx,
                );
            });

        dialog
            .max_w(px(360.))
            .button_props(DialogButtonProps::default().ok_text("Done"))
            .child(
                v_flex()
                    .gap_3()
                    .child(
                        DialogHeader::new()
                            .px_4()
                            .pt_4()
                            .child(DialogTitle::new().child("Account")),
                    )
                    .child(row("Email", email))
                    .child(row("Current plan", plan))
                    .child(div().px_4().child(Separator::horizontal()))
                    .child(
                        v_flex()
                            .px_4()
                            .gap_2()
                            .child(profile_btn)
                            .child(reset_password_btn)
                            .child(bug_btn),
                    )
                    .child(div().px_4().child(Separator::horizontal()))
                    .child(
                        h_flex()
                            .px_4()
                            .pb_4()
                            .w_full()
                            .justify_end()
                            .child(signout_btn),
                    )
                    .child(div().h(px(0.)).border_t_1().border_color(border)),
            )
    });
}
