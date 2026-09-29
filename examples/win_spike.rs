//! W1 spike v5 (specs/01): one real pin — `WindowKind::PopUp` with fork
//! tag `.9` `Window::set_position`/`resize` — and the W3 gesture
//! skeleton: poll-based ground truth (16 ms `GetCursorPos`) driving
//! `SetWindowPos` directly in the tick (Windows: independent of the
//! present path, unlike Wayland's staged margins).
//!
//! Owner checklist (Windows):
//! 1. one AVIF appears top-left, rounded corners (desktop visible through
//!    the corner cutouts = per-pixel alpha works).
//! 2. drag its body — 1:1 tracking, no wiggle, survives fast flings.
//! 3. hover the right/bottom edge (cursor flips to ↔/↕) or the corner
//!    (⌟) and drag to resize — anchored at the top-left. Releasing the
//!    button outside the window (or losing focus mid-drag) must end the
//!    gesture.
//! 4. ctrl+wheel steps opacity 0.2–1.0.
//! 5. q / escape closes the app (click first — it starts unfocused).
//!
//! Run: `cargo run --release --example win_spike [DIR]` (Windows only;
//! defaults to `C:\x\b`, first `*.avif` sorted).

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("win_spike is Windows-only; run it on Windows");
}

#[cfg(target_os = "windows")]
mod spike_app {
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        time::{Duration, Instant},
    };

    use gpui::{
        actions, div, img, point, px, size, App, Bounds, Context, CursorStyle, FocusHandle,
        Focusable, ImageSource, IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent,
        MouseUpEvent, ParentElement, Pixels, Point, Render, RenderImage, ScrollWheelEvent, Size,
        Styled, Window, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions,
        prelude::*,
    };
    use gpui_platform::application;
    use image::Frame as ImageFrame;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    use zenpixels::PixelFormat;

    actions!(spike, [Quit]);

    const MAX_SIDE: f32 = 560.;
    const EDGE: f32 = 8.;
    const MIN_W: f32 = 60.;
    const MIN_H: f32 = 40.;

    /// Global cursor in logical gpui space (top-left origin, y down — same
    /// space as `PlatformDisplay::bounds`).
    fn cursor_logical(scale: f32) -> Option<Point<Pixels>> {
        let mut p = POINT::default();
        unsafe { GetCursorPos(&mut p) }.ok()?;
        Some(point(px(p.x as f32 / scale), px(p.y as f32 / scale)))
    }

    /// AVIF → RGBA8 + natural size (the common 8-bit slice of src/content.rs's
    /// format matrix; exotic formats are skipped with a log).
    fn decode_avif(path: &Path) -> anyhow::Result<(image::RgbaImage, (u32, u32))> {
        let bytes = std::fs::read(path)?;
        let buffer = zenavif::decode(&bytes)?;
        let (w, h) = (buffer.width(), buffer.height());
        let raw = buffer.copy_to_contiguous_bytes();
        let format = buffer.descriptor().format;
        let to_rgba = |stride: usize, px: &dyn Fn(&[u8]) -> [u8; 4]| -> anyhow::Result<image::RgbaImage> {
            let mut buf = Vec::with_capacity(w as usize * h as usize * 4);
            for p in raw.chunks_exact(stride) {
                buf.extend_from_slice(&px(p));
            }
            image::RgbaImage::from_raw(w, h, buf).ok_or_else(|| anyhow::anyhow!("size mismatch"))
        };
        let img = match format {
            PixelFormat::Rgba8 => to_rgba(4, &|p| [p[0], p[1], p[2], p[3]])?,
            PixelFormat::Rgb8 => to_rgba(3, &|p| [p[0], p[1], p[2], 255])?,
            PixelFormat::Bgra8 => to_rgba(4, &|p| [p[2], p[1], p[0], p[3]])?,
            PixelFormat::Rgbx8 => to_rgba(4, &|p| [p[0], p[1], p[2], 255])?,
            PixelFormat::Bgrx8 => to_rgba(4, &|p| [p[2], p[1], p[0], 255])?,
            PixelFormat::Gray8 => to_rgba(1, &|p| [p[0], p[0], p[0], 255])?,
            other => anyhow::bail!("unsupported pixel format {other:?}"),
        };
        Ok((img, (w, h)))
    }

    /// Gesture at press; drives the poll tick.
    #[derive(Clone, Copy)]
    enum Gesture {
        /// Origin follows cursor; offset = origin − global cursor, fixed at
        /// press (the doc's `offset = −press_local` in window-origin terms).
        Move { offset: Point<Pixels> },
        /// Right/bottom edges; the top-left anchor stays fixed.
        Resize { right: bool, bottom: bool },
    }

    struct Spike {
        image: Arc<RenderImage>,
        scale: f32,
        pos: Point<Pixels>,
        sent: Point<Pixels>,
        size: Size<Pixels>,
        mode: Option<Gesture>,
        /// Edge-proximity for the per-frame cursor style (applied in render —
        /// styles set from listeners are reset by the next frame).
        hover: (bool, bool),
        opacity: f32,
        focus_handle: FocusHandle,
    }

    impl Spike {
        /// Windows differs from the Wayland doc here: `SetWindowPos` is
        /// independent of the present path, so moves apply directly in the
        /// poll tick with a fresh cursor sample — no frame gating, no
        /// re-render (content is unchanged; DWM slides the surface).
        fn gesture_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
            let Some(mode) = self.mode else {
                return false; // gesture over → poll loop exits
            };
            // Ground truth: if the button was released while capture was lost
            // (up event swallowed), stop instead of chasing the cursor.
            if unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } >= 0 {
                self.mode = None;
                return false;
            }
            let Some(cursor) = cursor_logical(self.scale) else {
                return true;
            };
            match mode {
                Gesture::Move { offset } => {
                    // Windows differs from the Wayland doc here: `SetWindowPos`
                    // is independent of the present path, so moves apply
                    // directly in the poll tick — no frame gating, no
                    // re-render (content unchanged; DWM slides the surface).
                    let target = point(cursor.x + offset.x, cursor.y + offset.y);
                    if target != self.pos {
                        self.pos = target;
                        self.sent = target;
                        window.set_position(target);
                    }
                }
                Gesture::Resize { right, bottom } => {
                    let new = size(
                        if right {
                            (cursor.x - self.pos.x).max(px(MIN_W))
                        } else {
                            self.size.width
                        },
                        if bottom {
                            (cursor.y - self.pos.y).max(px(MIN_H))
                        } else {
                            self.size.height
                        },
                    );
                    if new != self.size {
                        self.size = new;
                        window.resize(new);
                        cx.notify(); // re-fit content at 1:1 texels
                    }
                }
            }
            true
        }
    }

    impl Render for Spike {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            // Frame-gate: at most one set_position per frame, only on change.
            if self.pos != self.sent {
                self.sent = self.pos;
                window.set_position(self.pos);
            }
            window.set_window_cursor_style(match self.hover {
                (true, true) => CursorStyle::ResizeUpRightDownLeft,
                (true, _) => CursorStyle::ResizeLeftRight,
                (_, true) => CursorStyle::ResizeUpDown,
                _ => CursorStyle::Arrow,
            });
            div()
                .id("board")
                .size_full()
                .overflow_hidden()
                .rounded(px(24.))
                .key_context("spike")
                .track_focus(&self.focus_handle)
                .child(
                    img(ImageSource::Render(self.image.clone()))
                        .size_full()
                        .opacity(self.opacity),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                        let Some(cursor) = cursor_logical(this.scale) else {
                            return;
                        };
                        let local = ev.position;
                        let on_right = local.x >= this.size.width - px(EDGE);
                        let on_bottom = local.y >= this.size.height - px(EDGE);
                        this.mode = Some(if on_right || on_bottom {
                            Gesture::Resize {
                                right: on_right,
                                bottom: on_bottom,
                            }
                        } else {
                            Gesture::Move {
                                offset: point(this.pos.x - cursor.x, this.pos.y - cursor.y),
                            }
                        });
                        window.focus(&this.focus_handle, cx);
                        // The tick leases through the window handle only: the
                        // root view of this window IS Spike, so nesting a
                        // handle lease inside WeakEntity::update would
                        // double-lease and abort (seen in the field).
                        let handle = window.window_handle().downcast::<Spike>().unwrap();
                        cx.spawn(async move |_, cx| {
                            loop {
                                cx.background_executor()
                                    .timer(Duration::from_millis(16))
                                    .await;
                                if !handle
                                    .update(cx, |spike, window, cx| spike.gesture_tick(window, cx))
                                    .unwrap_or(false)
                                {
                                    break;
                                }
                            }
                        })
                        .detach();
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _ev: &MouseUpEvent, _window, _cx| {
                        this.mode = None;
                    }),
                )
                .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _window, cx| {
                    if this.mode.is_some() {
                        return; // a gesture owns the cursor; the tick drives it
                    }
                    let local = ev.position;
                    let hover = (
                        local.x >= this.size.width - px(EDGE),
                        local.y >= this.size.height - px(EDGE),
                    );
                    if hover != this.hover {
                        this.hover = hover;
                        cx.notify(); // style is applied per-frame in render
                    }
                }))
                .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _window, cx| {
                    if !ev.modifiers.control {
                        return;
                    }
                    let dy = ev.delta.pixel_delta(px(20.)).y;
                    let step = if dy > px(0.) { 0.1 } else { -0.1 };
                    let next = (this.opacity + step).clamp(0.2, 1.0);
                    if next != this.opacity {
                        this.opacity = next;
                        cx.notify();
                    }
                }))
                .on_action(cx.listener(|_this, _: &Quit, _window, cx| {
                    cx.quit();
                }))
        }
    }

    impl Focusable for Spike {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    pub fn main() {
        application().run(|cx: &mut App| {
            cx.bind_keys([
                KeyBinding::new("q", Quit, None),
                KeyBinding::new("escape", Quit, None),
            ]);

            let dir = std::env::args().nth(1).unwrap_or_else(|| r"C:\x\b".into());
            let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| {
                            p.extension()
                                .is_some_and(|e| e.eq_ignore_ascii_case("avif"))
                        })
                        .collect()
                })
                .unwrap_or_default();
            files.sort();
            files.truncate(1); // one pin: the resize test

            let mut n = 0usize;
            for path in files.into_iter() {
                let t = Instant::now();
                let (img, (w, h)) = match decode_avif(&path) {
                    Ok(ok) => ok,
                    Err(e) => {
                        eprintln!("skip {}: {e}", path.display());
                        continue;
                    }
                };
                let fit = (MAX_SIDE / w.max(h) as f32).min(1.0);
                let (vw, vh) = ((w as f32 * fit).round().max(1.), (h as f32 * fit).round().max(1.));
                // CPU Lanczos downsample to display size — the fork's GPU path
                // samples without mips (blocky at 0.2×); interactive zoom needs
                // GPU mipmaps (fork work item), CPU resample pins the spike.
                let img = if (vw as u32, vh as u32) != (w, h) {
                    image::imageops::resize(
                        &img,
                        vw as u32,
                        vh as u32,
                        image::imageops::FilterType::Lanczos3,
                    )
                } else {
                    img
                };
                // gpui's atlas is Bgra8 — swap into place (field manual §8).
                let mut img = img;
                for p in img.pixels_mut() {
                    p.0.swap(0, 2);
                }
                let image = Arc::new(RenderImage::new(vec![ImageFrame::new(img)]));
                eprintln!(
                    "{} ({}x{} → {}x{}) ready in {:?}",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    w,
                    h,
                    vw,
                    vh,
                    t.elapsed()
                );
                let start = point(px(80.), px(80.));
                cx.open_window(
                    WindowOptions {
                        titlebar: None,
                        focus: false,
                        show: true,
                        window_background: WindowBackgroundAppearance::Transparent,
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: start,
                            size: size(px(vw), px(vh)),
                        })),
                        kind: WindowKind::PopUp,
                        ..Default::default()
                    },
                    |window, cx| {
                        let scale = window.scale_factor();
                        cx.new(|cx| Spike {
                            image,
                            scale,
                            pos: start,
                            sent: start,
                            size: size(px(vw), px(vh)),
                            mode: None,
                            hover: (false, false),
                            opacity: 1.0,
                            focus_handle: cx.focus_handle(),
                        })
                    },
                )
                .unwrap();
                n += 1;
            }
            eprintln!("{n} pin(s) on screen; click one, drag, escape to quit");
        });
    }
}

#[cfg(target_os = "windows")]
fn main() {
    spike_app::main()
}
