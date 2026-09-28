//! A pinned image window: an overlay surface anchored top-left — layer
//! surface positioned by margins on Wayland (fork `Window::set_margin`),
//! `Window::set_position` on macOS. Left-drag moves, edge/corner-drag
//! resizes (ground-truth poll mechanics proven in upperadd), middle/Ctrl-drag
//! pans, Ctrl+wheel zooms; `[`/`]` opacity, `,`/`.` corner radius,
//! `0`/`1`/`-`/`=` zoom, double-click fits, `Q`/`Delete` deletes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedSender;
use gpui::{
    App, Bounds, ClickEvent, Context, CursorStyle, DisplayId, FocusHandle, InteractiveElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, Pixels,
    Point, Render, ScrollDelta, ScrollWheelEvent, Size, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowOptions, div, hsla, img, point,
    prelude::*, px, size,
};
use log::debug;

use crate::content::PinImage;
use crate::state::PinRecord;

/// Smallest allowed pin size (edge-drag resize clamps here); reference
/// thumbnails want to go small.
const MIN_W: f32 = 40.0;
const MIN_H: f32 = 40.0;
/// Width/height of the invisible edge hit strips.
const EDGE: f32 = 6.0;
/// Corner grab squares (larger than edge strips: corners are the hardest
/// target to hit, and they resize both axes at once).
const CORNER: f32 = 12.0;
/// Cursor poll rate for move/resize gestures (the pin is not eased — every
/// poll jumps straight to the cursor — so this is the upper bound on how
/// far a moving edge trails the pointer).
const MOVE_POLL: Duration = Duration::from_millis(16);
/// A fast fling exits the surface at once; keep following the cursor while
/// it is outside, but end the gesture after this long without re-entry.
#[cfg_attr(target_os = "windows", allow(dead_code))] // Windows ends on button state
const GESTURE_LEAVE_GRACE: Duration = Duration::from_millis(250);
/// While resizing, the cursor rides exactly on the moving edge — count the
/// cursor as "inside" within this band so the leave-grace never fires
/// mid-resize. Move gestures don't need it (the window follows the cursor).
#[cfg_attr(target_os = "windows", allow(dead_code))] // Windows ends on button state
const RESIZE_EDGE_BAND: f32 = 24.0;
/// Lowest pin opacity (tracing over a canvas still needs a ghost visible).
const MIN_OPACITY: f32 = 0.2;
/// Opacity step for `[` / `]`.
const OPACITY_STEP: f32 = 0.05;
/// Ctrl+wheel and `-`/`=` zoom step.
const ZOOM_STEP: f32 = 1.25;
/// Zoom range; 1 = 100% raster resolution.
const ZOOM_MIN: f32 = 0.1;
const ZOOM_MAX: f32 = 8.0;
/// Corner-radius step for `,` / `.`.
const RADIUS_STEP: f32 = 4.0;

/// Window -> daemon events. The pin owns its live view state; the daemon
/// mirrors records for persistence and owns window lifetime.
pub enum PinEvent {
    /// Live state changed (gesture end, key op) — mirror and persist.
    Update(u64, PinRecord),
    /// Click: raise above sibling pins (remap; map order = z order).
    Raise(u64),
    /// `Q`/`Delete`: close the pin and drop its record.
    Delete(u64),
}

/// Which cardinal edge a resize drag started from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    fn cursor(self) -> CursorStyle {
        self.dir().cursor()
    }

    fn dir(self) -> ResizeDir {
        match self {
            Edge::Left => ResizeDir::Left,
            Edge::Right => ResizeDir::Right,
            Edge::Top => ResizeDir::Top,
            Edge::Bottom => ResizeDir::Bottom,
        }
    }
}

/// Which corner a resize drag started from (both axes at once).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    fn dir(self) -> ResizeDir {
        match self {
            Corner::TopLeft => ResizeDir::TopLeft,
            Corner::TopRight => ResizeDir::TopRight,
            Corner::BottomLeft => ResizeDir::BottomLeft,
            Corner::BottomRight => ResizeDir::BottomRight,
        }
    }
}

/// The axis-resolved resize direction: which edges move and which stay put.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResizeDir {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl ResizeDir {
    fn cursor(self) -> CursorStyle {
        match self {
            ResizeDir::Left | ResizeDir::Right => CursorStyle::ResizeLeftRight,
            ResizeDir::Top | ResizeDir::Bottom => CursorStyle::ResizeUpDown,
            ResizeDir::TopLeft | ResizeDir::BottomRight => CursorStyle::ResizeUpLeftDownRight,
            ResizeDir::TopRight | ResizeDir::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
        }
    }

    fn moves_x(self) -> bool {
        matches!(
            self,
            ResizeDir::Left | ResizeDir::TopLeft | ResizeDir::BottomLeft
        )
    }

    fn moves_y(self) -> bool {
        matches!(
            self,
            ResizeDir::Top | ResizeDir::TopLeft | ResizeDir::TopRight
        )
    }

    fn active_x(self) -> bool {
        !matches!(self, ResizeDir::Top | ResizeDir::Bottom)
    }

    fn active_y(self) -> bool {
        !matches!(self, ResizeDir::Left | ResizeDir::Right)
    }
}

/// Pointer gestures. Move/resize use the ground-truth poll loop (the pin
/// follows the global cursor, so surface-local origin shifts cannot feed
/// back and fast flings that exit the surface keep working). Pan never
/// needs ground truth outside the surface, so plain move events drive it.
enum Gesture {
    #[cfg_attr(target_os = "windows", allow(dead_code))] // Windows ends on button state
    Move {
        /// `pos = global cursor + offset`, constant for the whole drag.
        offset: Point<Pixels>,
        left_at: Option<Instant>,
    },
    #[cfg_attr(target_os = "windows", allow(dead_code))] // Windows ends on button state
    Resize {
        dir: ResizeDir,
        /// Position/size at press; edges that must stay put derive their
        /// target from the snapshot, never from accumulated deltas.
        start_pos: Point<Pixels>,
        start_size: Size<Pixels>,
        left_at: Option<Instant>,
    },
    /// `pan = cursor - grab`, updated from surface-local move events.
    Pan { grab: Point<Pixels> },
}

impl Gesture {
    #[cfg_attr(target_os = "windows", allow(dead_code))] // Windows ends on button state
    fn left_at_mut(&mut self) -> Option<&mut Option<Instant>> {
        match self {
            Gesture::Move { left_at, .. } | Gesture::Resize { left_at, .. } => Some(left_at),
            Gesture::Pan { .. } => None,
        }
    }

    fn cursor(&self) -> CursorStyle {
        match self {
            Gesture::Move { .. } | Gesture::Pan { .. } => CursorStyle::ClosedHand,
            Gesture::Resize { dir, .. } => dir.cursor(),
        }
    }

    /// The button that must stay pressed for this gesture.
    fn button(&self) -> MouseButton {
        match self {
            Gesture::Pan { .. } => MouseButton::Middle,
            Gesture::Move { .. } | Gesture::Resize { .. } => MouseButton::Left,
        }
    }
}

pub struct Pin {
    /// Daemon-side entry key (stable across record edits).
    id: u64,
    events: UnboundedSender<PinEvent>,
    /// Keyboard focus node — keys work after a click (OnDemand surface).
    focus: FocusHandle,
    source: PathBuf,
    /// How the image is rendered (asset pipeline vs pre-decoded).
    image: PinImage,
    output: String,
    /// Surface offset from the output's top-left == (top, left) margins.
    pos: Point<Pixels>,
    /// Surface size (requested via `Window::resize`).
    size: Size<Pixels>,
    /// Position/size actually sent to the compositor (advanced in `render`).
    sent_pos: Point<Pixels>,
    sent_size: Size<Pixels>,
    /// Layout origin of the output this surface lives on (cursorpos is
    /// global, margins are output-local).
    output_origin: Point<Pixels>,
    /// Decoded natural image size (px); zoom 1 = one raster px per logical px.
    natural: Size<Pixels>,
    zoom: f32,
    /// Image top-left within the pin (clamped: no gaps when zoomed in).
    pan: Point<Pixels>,
    opacity: f32,
    radius: f32,
    gesture: Option<Gesture>,
}

/// Open a pin window anchored top+left: the margins position it within the
/// output, so `record.x/y` stay output-local pixels.
pub fn spawn(
    cx: &mut App,
    id: u64,
    record: &PinRecord,
    image: PinImage,
    natural: Size<Pixels>,
    output_origin: Point<Pixels>,
    display_id: Option<DisplayId>,
    events: UnboundedSender<PinEvent>,
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
            // Wayland: the margin positions the surface, origin is ignored.
            // Windows/macOS: bounds origin is global logical px — spawn on
            // the pin's output directly (no flash at the primary's 0,0).
            #[cfg(target_os = "linux")]
            origin: point(px(0.), px(0.)),
            #[cfg(not(target_os = "linux"))]
            origin: output_origin + pos,
            size: win_size,
        })),
        kind: crate::platform::pin_kind(pos),
        ..Default::default()
    };
    let output = record.output.clone();
    let image = image.clone();
    cx.open_window(options, |_, cx| {
        cx.new(|cx| {
            Pin::new(
                id,
                record,
                image,
                natural,
                output_origin,
                output,
                events,
                cx,
            )
        })
    })
}

impl Pin {
    fn new(
        id: u64,
        record: &PinRecord,
        image: PinImage,
        natural: Size<Pixels>,
        output_origin: Point<Pixels>,
        output: String,
        events: UnboundedSender<PinEvent>,
        cx: &mut Context<Self>,
    ) -> Self {
        let pos = point(px(record.x as f32), px(record.y as f32));
        let win_size = size(px(record.w as f32), px(record.h as f32));
        let mut pin = Self {
            id,
            events,
            focus: cx.focus_handle(),
            source: record.source.clone(),
            image,
            output,
            pos,
            size: win_size,
            sent_pos: pos,
            sent_size: win_size,
            output_origin,
            natural,
            zoom: record.zoom,
            pan: point(px(record.pan_x as f32), px(record.pan_y as f32)),
            opacity: record.opacity.clamp(MIN_OPACITY, 1.0),
            radius: record.radius as f32,
            gesture: None,
        };
        pin.clamp_pan();
        pin
    }

    /// Daemon-side re-clamp (toggle-on): move/resize without touching view
    /// state beyond pan.
    pub fn apply_geometry(&mut self, x: f64, y: f64, w: f64, h: f64, cx: &mut Context<Self>) {
        self.pos = point(px(x as f32), px(y as f32));
        self.size = size(px(w as f32), px(h as f32));
        self.clamp_pan();
        cx.notify();
    }

    /// Whole image visible in the pin, never upscaled past raster 1:1.
    fn fit_zoom(&self) -> f32 {
        (f32::from(self.size.width) / f32::from(self.natural.width))
            .min(f32::from(self.size.height) / f32::from(self.natural.height))
            .clamp(ZOOM_MIN, 1.0)
    }

    /// Gapless, per axis: an image axis larger than the pin is clamped to
    /// its overhang (no background shows); a smaller axis centers. (A shared
    /// branch would clamp the non-overflowing axis against min > max.)
    fn clamp_pan(&mut self) {
        let w = f32::from(self.size.width);
        let h = f32::from(self.size.height);
        let dw = f32::from(self.natural.width) * self.zoom;
        let dh = f32::from(self.natural.height) * self.zoom;
        let x = if dw > w {
            f32::from(self.pan.x).clamp(w - dw, 0.0)
        } else {
            (w - dw) / 2.0
        };
        let y = if dh > h {
            f32::from(self.pan.y).clamp(h - dh, 0.0)
        } else {
            (h - dh) / 2.0
        };
        self.pan = point(px(x), px(y));
    }

    /// Zoom keeping `anchor` (surface-local) fixed on the same image point.
    fn zoom_at(&mut self, anchor: Point<Pixels>, factor: f32) {
        let next = (self.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        if next == self.zoom {
            return;
        }
        let k = next / self.zoom;
        self.pan = point(
            px(f32::from(anchor.x) - (f32::from(anchor.x) - f32::from(self.pan.x)) * k),
            px(f32::from(anchor.y) - (f32::from(anchor.y) - f32::from(self.pan.y)) * k),
        );
        self.zoom = next;
        self.clamp_pan();
    }

    fn record(&self) -> PinRecord {
        PinRecord {
            source: self.source.clone(),
            output: self.output.clone(),
            x: f64::from(f32::from(self.pos.x)),
            y: f64::from(f32::from(self.pos.y)),
            w: f64::from(f32::from(self.size.width)),
            h: f64::from(f32::from(self.size.height)),
            z: 0, // daemon assigns sibling order on save
            zoom: self.zoom,
            pan_x: f64::from(f32::from(self.pan.x)),
            pan_y: f64::from(f32::from(self.pan.y)),
            opacity: self.opacity,
            radius: f64::from(self.radius),
        }
    }

    /// View state changed: mirror to the daemon (persists) + repaint.
    fn changed(&mut self, cx: &mut Context<Self>) {
        let _ = self
            .events
            .unbounded_send(PinEvent::Update(self.id, self.record()));
        cx.notify();
    }

    /// Begin a pointer gesture; move/resize also start the ground-truth
    /// poll loop (pan is event-driven).
    fn start_gesture(&mut self, gesture: Gesture, window: &mut Window, cx: &mut Context<Self>) {
        self.gesture = Some(gesture);
        cx.notify(); // pick up the gesture cursor this frame
        if matches!(self.gesture, Some(Gesture::Pan { .. })) {
            return; // pan rides mouse-move deltas; no poll needed
        }
        // The poll leases through the window handle: root view + window in
        // ONE lease. (Nesting a handle lease inside WeakEntity::update would
        // double-lease the same entity — gpui aborts.)
        let handle = window.window_handle().downcast::<Pin>().unwrap();
        cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor().timer(MOVE_POLL).await;
                if !handle
                    .update(cx, |pin, window, cx| pin.gesture_tick(window, cx))
                    .unwrap_or(false)
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn on_root_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        let _ = self.events.unbounded_send(PinEvent::Raise(self.id));
        // At press the surface origin == self.pos, so cursor_global =
        // self.pos + ev.position; with pos = cursor_global + offset that
        // makes offset = −press_local, constant for the whole drag.
        let offset = point(-ev.position.x, -ev.position.y);
        self.start_gesture(
            Gesture::Move {
                offset,
                left_at: None,
            },
            window,
            cx,
        );
    }

    fn on_middle_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        let _ = self.events.unbounded_send(PinEvent::Raise(self.id));
        self.start_gesture(
            Gesture::Pan {
                grab: ev.position - self.pan,
            },
            window,
            cx,
        );
    }

    fn on_edge_down(
        &mut self,
        dir: ResizeDir,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        // Edge/corner strips overlap the whole-surface move handler; without
        // this the move handler fires after ours and overwrites the gesture.
        cx.stop_propagation();
        debug!("pin resize start {:?} at {:?}", dir, ev.position);
        self.start_gesture(
            Gesture::Resize {
                dir,
                start_pos: self.pos,
                start_size: self.size,
                left_at: None,
            },
            window,
            cx,
        );
    }

    fn on_move(&mut self, ev: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(gesture) = &mut self.gesture else {
            return;
        };
        if ev.pressed_button != Some(gesture.button()) {
            debug!("pin gesture cancelled (button up missed)");
            self.gesture = None;
            cx.notify();
            return;
        }
        if let Gesture::Pan { grab } = gesture {
            let target = ev.position - *grab;
            if target != self.pan {
                self.pan = target;
                self.clamp_pan();
                cx.notify();
            }
        }
    }

    fn on_up(&mut self, ev: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ends = matches!(&self.gesture, Some(g) if g.button() == ev.button);
        if ends {
            self.gesture = None;
            self.changed(cx); // gesture end persists (release the cursor too)
        }
    }

    fn on_click(&mut self, ev: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let ClickEvent::Mouse(click) = ev {
            if click.up.click_count >= 2 {
                let center = point(
                    px(f32::from(self.size.width) / 2.0),
                    px(f32::from(self.size.height) / 2.0),
                );
                self.zoom_at(center, self.fit_zoom() / self.zoom);
                self.changed(cx);
            }
        }
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !ev.modifiers.control {
            return;
        }
        let up = match ev.delta {
            ScrollDelta::Pixels(p) => p.y > Pixels::ZERO,
            ScrollDelta::Lines(l) => l.y > 0.0,
        };
        let factor = if up { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
        self.zoom_at(ev.position, factor);
        self.changed(cx);
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let center = point(
            px(f32::from(self.size.width) / 2.0),
            px(f32::from(self.size.height) / 2.0),
        );
        let max_radius = f32::from(self.size.width).min(f32::from(self.size.height)) / 2.0;
        match ev.keystroke.key.as_str() {
            "[" => {
                self.opacity = (self.opacity - OPACITY_STEP).max(MIN_OPACITY);
                self.changed(cx);
            }
            "]" => {
                self.opacity = (self.opacity + OPACITY_STEP).min(1.0);
                self.changed(cx);
            }
            "," => {
                self.radius = (self.radius - RADIUS_STEP).max(0.0);
                self.changed(cx);
            }
            "." => {
                self.radius = (self.radius + RADIUS_STEP).min(max_radius);
                self.changed(cx);
            }
            "0" => {
                self.zoom_at(center, self.fit_zoom() / self.zoom);
                self.changed(cx);
            }
            "1" => {
                self.zoom_at(center, 1.0 / self.zoom);
                self.changed(cx);
            }
            "-" => {
                self.zoom_at(center, 1.0 / ZOOM_STEP);
                self.changed(cx);
            }
            "=" | "+" => {
                self.zoom_at(center, ZOOM_STEP);
                self.changed(cx);
            }
            "q" | "delete" => {
                let _ = self.events.unbounded_send(PinEvent::Delete(self.id));
            }
            _ => {}
        }
    }

    /// GetAsyncKeyState ground truth for the poll tick (spike finding: the
    /// up event can be swallowed when the release lands outside the window).
    #[cfg(target_os = "windows")]
    fn windows_button_down(button: MouseButton) -> bool {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

        let vk: i32 = match button {
            MouseButton::Left => 0x01,   // VK_LBUTTON
            MouseButton::Middle => 0x04, // VK_MBUTTON
            _ => return false,
        };
        // Parenthesized: `unsafe {}` at statement start would parse as a
        // block statement, eating the `< 0`.
        (unsafe { GetAsyncKeyState(vk) }) < 0 // negative (high bit) = down
    }

    /// One cursor poll while a move/resize gesture is active. Returns false
    /// when over.
    #[allow(unused_variables)] // `window` only drives direct applies (Windows)
    fn gesture_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(gesture) = &mut self.gesture else {
            return false;
        };
        // Windows ground truth: capture keeps events flowing, but a release
        // over another window can swallow the up event — the poll would
        // chase the cursor forever. The button, not the pointer's
        // whereabouts, ends the gesture (so no leave-grace either).
        #[cfg(target_os = "windows")]
        if !Self::windows_button_down(gesture.button()) {
            debug!("pin gesture ended (button up, event missed)");
            self.gesture = None;
            self.changed(cx);
            return false;
        }
        let Some(cursor) = crate::platform::cursor_global() else {
            return true; // Hyprland IPC hiccup; keep the gesture alive
        };
        let local = point(
            cursor.x - self.output_origin.x,
            cursor.y - self.output_origin.y,
        );
        // Pointer-leave detection from the same ground truth as the motion
        // (surface events stop when the cursor exits): grace-period the end
        // of the gesture so fast flings that exit the surface still follow.
        #[cfg(not(target_os = "windows"))]
        {
            let band = match *gesture {
                Gesture::Resize { .. } => px(RESIZE_EDGE_BAND),
                _ => px(0.0),
            };
            let inside = local.x >= self.pos.x - band
                && local.x < self.pos.x + self.size.width + band
                && local.y >= self.pos.y - band
                && local.y < self.pos.y + self.size.height + band;
            if inside {
                if let Some(left_at) = gesture.left_at_mut() {
                    *left_at = None;
                }
            } else if let Some(left_at) = gesture.left_at_mut() {
                let left_at = left_at.get_or_insert(Instant::now());
                if left_at.elapsed() > GESTURE_LEAVE_GRACE {
                    debug!("pin gesture ended (cursor left the surface)");
                    self.gesture = None;
                    self.changed(cx);
                    return false;
                }
            }
        }

        // Snap to whole pixels: the wayland layer path truncates to i32.
        let round = |v: Pixels| px(f32::from(v).round());
        match &gesture {
            Gesture::Move { offset, .. } => {
                let target = point(local.x + offset.x, local.y + offset.y);
                if target != self.pos {
                    self.pos = target;
                    // Windows: SetWindowPos is independent of the present
                    // path — apply in the tick (frame-gated application
                    // trails a frame at 60 Hz and reads as drag lag; the
                    // spike proved the direct pipeline).
                    #[cfg(target_os = "windows")]
                    {
                        window.set_position(self.output_origin + target);
                        self.sent_pos = target;
                    }
                    #[cfg(not(target_os = "windows"))]
                    cx.notify(); // render applies the margin (frame-gated)
                }
            }
            Gesture::Resize {
                dir,
                start_pos,
                start_size,
                ..
            } => {
                let (mut pos, mut size) = (*start_pos, *start_size);
                // Snapshot edges never move; moving edges chase the cursor
                // with a min-size clamp. Right/bottom come from the live
                // self.pos (fixed for the whole gesture) — deriving them
                // from the snapshot mixes frames and jumps the size.
                let right = round(start_pos.x + start_size.width);
                let bottom = round(start_pos.y + start_size.height);
                if dir.moves_x() {
                    pos.x = round(local.x).min(right - px(MIN_W));
                    size.width = round(right - pos.x);
                } else if dir.active_x() {
                    size.width = round((local.x - self.pos.x).max(px(MIN_W)));
                }
                if dir.moves_y() {
                    pos.y = round(local.y).min(bottom - px(MIN_H));
                    size.height = round(bottom - pos.y);
                } else if dir.active_y() {
                    size.height = round((local.y - self.pos.y).max(px(MIN_H)));
                }
                if size.width != self.size.width || size.height != self.size.height {
                    self.size = size;
                    self.pos = pos;
                    // Keep the image gapless as the pin shrinks under it.
                    self.clamp_pan();
                    #[cfg(target_os = "windows")]
                    {
                        // Direct apply (spike pipeline); sent_* advance so
                        // the render gates skip. notify repaints the fit.
                        window.resize(size);
                        window.set_position(self.output_origin + pos);
                        self.sent_size = size;
                        self.sent_pos = pos;
                        cx.notify();
                    }
                    #[cfg(not(target_os = "windows"))]
                    cx.notify(); // render applies resize (+ margin for L/T)
                } else if pos != self.pos {
                    self.pos = pos; // min-clamp shift without a size change
                    #[cfg(target_os = "windows")]
                    {
                        window.set_position(self.output_origin + pos);
                        self.sent_pos = pos;
                    }
                    #[cfg(not(target_os = "windows"))]
                    cx.notify();
                }
            }
            Gesture::Pan { .. } => {}
        }
        true
    }

    fn render_edge(&self, edge: Edge, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let listener = cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
            this.on_edge_down(edge.dir(), ev, window, cx);
        });
        let base = div()
            .absolute()
            .cursor(edge.cursor())
            .on_mouse_down(MouseButton::Left, listener);
        match edge {
            Edge::Left => base.top_0().bottom_0().left_0().w(px(EDGE)),
            Edge::Right => base.top_0().bottom_0().right_0().w(px(EDGE)),
            Edge::Top => base.top_0().left_0().right_0().h(px(EDGE)),
            Edge::Bottom => base.bottom_0().left_0().right_0().h(px(EDGE)),
        }
    }

    /// Corner grab squares, painted above the edge strips; resize both axes
    /// at once like a true window.
    fn render_corner(&self, corner: Corner, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let dir = corner.dir();
        let listener = cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
            this.on_edge_down(dir, ev, window, cx);
        });
        let base = div()
            .absolute()
            .size(px(CORNER))
            .cursor(dir.cursor())
            .on_mouse_down(MouseButton::Left, listener);
        match corner {
            Corner::TopLeft => base.top_0().left_0(),
            Corner::TopRight => base.top_0().right_0(),
            Corner::BottomLeft => base.bottom_0().left_0(),
            Corner::BottomRight => base.bottom_0().right_0(),
        }
    }
}

impl Render for Pin {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Stage only — no cx.notify() needed: this very frame presents right
        // after render, and its commit carries the staged margin and resize
        // together with the new buffer (one atomic configure).
        if self.size != self.sent_size {
            let s = self.size;
            self.sent_size = s;
            debug!("pin resize {:?}", s);
            window.resize(s);
        }
        if self.pos != self.sent_pos {
            let p = self.pos;
            self.sent_pos = p;
            debug!("pin set_position {:?}", p);
            // macOS: top-left-origin global. (setFrameTopLeftPoint lands on
            // the runloop rather than staging into this frame's commit like
            // a layer-surface margin — a one-frame trailing edge at most.)
            #[cfg(target_os = "linux")]
            window.set_margin((p.y, px(0.), px(0.), p.x));
            #[cfg(not(target_os = "linux"))]
            window.set_position(self.output_origin + p);
        }
        // A window-wide cursor request wins over hover styles for the frame,
        // so the gesture keeps its cursor even when the pointer outruns the
        // hit strips. Cleared next frame it stops.
        if let Some(gesture) = &self.gesture {
            window.set_window_cursor_style(gesture.cursor());
        }

        let (img_w, img_h) = (
            f32::from(self.natural.width) * self.zoom,
            f32::from(self.natural.height) * self.zoom,
        );
        let styled = |el: gpui::Img| {
            el.absolute()
                .left(self.pan.x)
                .top(self.pan.y)
                .w(px(img_w))
                .h(px(img_h))
                // The parent's overflow_hidden does not clip children to the
                // rounded corners; round the image itself to match.
                .rounded(px(self.radius))
                .object_fit(ObjectFit::Contain)
        };
        // The parent's overflow_hidden does not clip children to the rounded
        // corners; the image is rounded itself above.
        let image = match &self.image {
            PinImage::Asset => {
                styled(img(Arc::<Path>::from(self.source.as_path()))).into_any_element()
            }
            PinImage::Decoded(render) => styled(img(render.clone())).into_any_element(),
            PinImage::Missing => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(hsla(220.0, 0.2, 0.10, 0.5))
                .text_size(px(12.0))
                .text_color(hsla(0.0, 0.0, 1.0, 0.55))
                .child("missing")
                .into_any_element(),
        };
        div()
            .id(("pin", self.id))
            .size_full()
            .relative()
            .bg(hsla(220.0, 0.2, 0.10, 0.92))
            .border_1()
            .border_color(hsla(0.0, 0.0, 1.0, 0.18))
            .rounded(px(self.radius))
            .opacity(self.opacity)
            .overflow_hidden()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_click(cx.listener(Self::on_click))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_root_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_middle_down))
            .on_mouse_move(cx.listener(Self::on_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(image)
            .child(self.render_edge(Edge::Left, cx))
            .child(self.render_edge(Edge::Right, cx))
            .child(self.render_edge(Edge::Top, cx))
            .child(self.render_edge(Edge::Bottom, cx))
            .child(self.render_corner(Corner::TopLeft, cx))
            .child(self.render_corner(Corner::TopRight, cx))
            .child(self.render_corner(Corner::BottomLeft, cx))
            .child(self.render_corner(Corner::BottomRight, cx))
    }
}
