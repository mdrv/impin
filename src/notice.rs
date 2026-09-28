//! The "No pinned image" pill: a centered, keyboard-less overlay surface
//! shown while zero pins exist. It gives the daemon a visible heartbeat and,
//! with `QuitMode::Explicit`, a reason to survive its last window closing.

use gpui::{
    App, Bounds, Context, DisplayId, Render, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle, WindowOptions, div, hsla, point, prelude::*, px, size,
};

/// The pill is a fixed-size surface centered on its display: Wayland does it
/// with all-edge anchors (origin ignored); macOS centers display-locally at
/// open and never moves afterwards.
const W: f32 = 230.0;
const H: f32 = 44.0;

pub struct Notice;

/// One instance lives for the whole daemon (hidden while pins exist);
/// visibility is driven by `daemon::sync_notice`.
pub fn spawn(cx: &mut App, display_id: Option<DisplayId>) -> anyhow::Result<WindowHandle<Notice>> {
    let display_size = display_id.and_then(|id| {
        cx.displays()
            .into_iter()
            .find(|d| d.id() == id)
            .map(|d| d.bounds().size)
    });
    let origin = display_size.map_or(point(px(0.), px(0.)), |s| {
        point((s.width - px(W)) / 2.0, (s.height - px(H)) / 2.0)
    });
    let options = WindowOptions {
        titlebar: None,
        focus: false,
        show: false,
        app_id: Some("impin-notice".into()),
        window_background: WindowBackgroundAppearance::Transparent,
        display_id,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin,
            size: size(px(W), px(H)),
        })),
        kind: crate::platform::notice_kind(),
        ..Default::default()
    };
    cx.open_window(options, |_, cx| cx.new(|_| Notice))
}

impl Render for Notice {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .px(px(14.0))
                    .py(px(8.0))
                    .rounded(px(10.0))
                    .bg(hsla(220.0, 0.2, 0.10, 0.88))
                    .border_1()
                    .border_color(hsla(0.0, 0.0, 1.0, 0.18))
                    .text_size(px(13.0))
                    .text_color(hsla(0.0, 0.0, 1.0, 0.7))
                    .child("No pinned image"),
            )
    }
}
