//! A pinned image window: an anchored top+left layer surface whose margins
//! position it within its output (fork `Window::set_margin`, stage-only
//! since tag .2). M1 shows the image; gestures land in M2.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, DisplayId, ObjectFit, Render, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowKind, WindowOptions, div, hsla, img, layer_shell::*, point,
    prelude::*, px, size,
};

use crate::state::PinRecord;

pub struct Pin {
    source: PathBuf,
}

/// Open a pin window anchored top+left: the margins position it within the
/// output, so `record.x/y` stay output-local pixels.
pub fn spawn(
    cx: &mut App,
    record: &PinRecord,
    display_id: Option<DisplayId>,
) -> anyhow::Result<WindowHandle<Pin>> {
    let pos = point(px(record.x as f32), px(record.y as f32));
    let win_size = size(px(record.w as f32), px(record.h as f32));
    let options = WindowOptions {
        titlebar: None,
        focus: false,
        show: true,
        app_id: Some("impin".into()),
        window_background: WindowBackgroundAppearance::Transparent,
        display_id,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.), px(0.)),
            size: win_size,
        })),
        kind: WindowKind::LayerShell(LayerShellOptions {
            namespace: "impin".into(),
            layer: Layer::Top,
            anchor: Anchor::TOP | Anchor::LEFT,
            exclusive_zone: Some(px(-1.)),
            margin: Some((pos.y, px(0.), px(0.), pos.x)),
            keyboard_interactivity: KeyboardInteractivity::OnDemand,
            ..Default::default()
        }),
        ..Default::default()
    };
    cx.open_window(options, |_, cx| {
        cx.new(|_| Pin {
            source: record.source.clone(),
        })
    })
}

impl Render for Pin {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(hsla(220.0, 0.2, 0.10, 0.92))
            .border_1()
            .border_color(hsla(0.0, 0.0, 1.0, 0.18))
            .overflow_hidden()
            .child(
                img(Arc::<Path>::from(self.source.as_path()))
                    .size_full()
                    .object_fit(ObjectFit::Contain),
            )
    }
}
