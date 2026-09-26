//! The "No pinned image" pill: a centered, keyboard-less top-layer surface
//! shown while zero pins exist. It gives the daemon a visible heartbeat and,
//! with `QuitMode::Explicit`, a reason to survive its last window closing.

use gpui::{
    App, Bounds, Context, DisplayId, Render, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle, WindowKind, WindowOptions, div, hsla, layer_shell::*, point, prelude::*, px,
    size,
};

/// The window is the pill: anchored to all four edges, a smaller surface is
/// centered by the compositor on both axes.
const W: f32 = 230.0;
const H: f32 = 44.0;

pub struct Notice;

/// One instance lives for the whole daemon (hidden while pins exist);
/// visibility is driven by `daemon::sync_notice`.
pub fn spawn(cx: &mut App, display_id: Option<DisplayId>) -> anyhow::Result<WindowHandle<Notice>> {
    let options = WindowOptions {
        titlebar: None,
        focus: false,
        show: false,
        app_id: Some("impin-notice".into()),
        window_background: WindowBackgroundAppearance::Transparent,
        display_id,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(W), px(H)),
        })),
        kind: WindowKind::LayerShell(LayerShellOptions {
            namespace: "impin-notice".into(),
            layer: Layer::Top,
            anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            exclusive_zone: Some(px(-1.)),
            margin: Some((px(0.), px(0.), px(0.), px(0.))),
            keyboard_interactivity: KeyboardInteractivity::None,
            ..Default::default()
        }),
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
