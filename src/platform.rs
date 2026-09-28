//! Per-platform windowing shims, so the rest of impin stays single-source.
//!
//! Two arms: Wayland + Hyprland (the original) and macOS AppKit. Both speak
//! the same vocabulary:
//!
//! - A display (`Mon`) lives in *global top-left-origin screen coordinates*
//!   (Hyprland's global layout; `CGDisplayBounds` on macOS), with a stable
//!   `name` doubling as the persistence key (output name / display UUID).
//! - The cursor (`cursor_global`) is reported in that same space.
//! - Pin/notice windows are overlay surfaces that never steal focus
//!   (layer-shell `OnDemand` / `NSPanel` `NonactivatingPanel`) and float
//!   above fullscreen apps on every workspace/desktop.

use std::path::PathBuf;

#[cfg(target_os = "macos")]
use anyhow::Context as _;
use gpui::{App, DisplayId, Pixels, Point, WindowKind, point, px};

use crate::state;

/// One display/output in global top-left-origin screen coordinates.
pub(crate) struct Mon {
    /// Persistence key: Hyprland output name / macOS display UUID.
    pub name: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

// --- overlay window kinds ---

/// Pin window: an anchored top+left layer surface on Wayland (margins are
/// its position); a non-activating popup panel on macOS (positioned via the
/// `window_bounds` origin + `Window::set_position`).
#[cfg(not(target_os = "macos"))]
pub(crate) fn pin_kind(pos: Point<Pixels>) -> WindowKind {
    use gpui::layer_shell::*;

    WindowKind::LayerShell(LayerShellOptions {
        namespace: "impin".into(),
        layer: Layer::Top,
        anchor: Anchor::TOP | Anchor::LEFT,
        exclusive_zone: Some(px(-1.)),
        margin: Some((pos.y, px(0.), px(0.), pos.x)),
        keyboard_interactivity: KeyboardInteractivity::OnDemand,
        ..Default::default()
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn pin_kind(_pos: Point<Pixels>) -> WindowKind {
    // NSPanel: NonactivatingPanel (never steals the canvas app's focus, but
    // takes keys once clicked), popup level (above normal + fullscreen
    // windows), CanJoinAllSpaces | FullScreenAuxiliary.
    WindowKind::PopUp
}

/// Notice pill: centered by the compositor on Wayland (all-edge anchors);
/// opened centered on macOS (fixed size, never moves afterwards).
#[cfg(not(target_os = "macos"))]
pub(crate) fn notice_kind() -> WindowKind {
    use gpui::layer_shell::*;

    WindowKind::LayerShell(LayerShellOptions {
        namespace: "impin-notice".into(),
        layer: Layer::Top,
        anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
        exclusive_zone: Some(px(-1.)),
        margin: Some((px(0.), px(0.), px(0.), px(0.))),
        keyboard_interactivity: KeyboardInteractivity::None,
        ..Default::default()
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn notice_kind() -> WindowKind {
    WindowKind::PopUp
}

// --- daemon mode ---

/// Keep the daemon out of the Dock and the cmd-tab switcher (the moral
/// equivalent of a compositor session without a toplevel).
#[cfg(target_os = "macos")]
pub(crate) fn accessory_mode() {
    let mtm = objc2::MainThreadMarker::new().expect("impin daemon runs on the main thread");
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Accessory);
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn accessory_mode() {}

// --- displays ---

#[cfg(not(target_os = "macos"))]
pub(crate) fn monitors(_cx: &App) -> Option<Vec<Mon>> {
    let json = run_capture("hyprctl", &["monitors", "-j"])?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    Some(
        value
            .as_array()?
            .iter()
            .filter_map(|m| {
                Some(Mon {
                    name: m.get("name")?.as_str()?.to_string(),
                    x: num(m, "x")?,
                    y: num(m, "y")?,
                    w: num(m, "width")?,
                    h: num(m, "height")?,
                })
            })
            .collect(),
    )
}

#[cfg(target_os = "macos")]
pub(crate) fn monitors(cx: &App) -> Option<Vec<Mon>> {
    Some(
        cx.displays()
            .into_iter()
            .map(|d| {
                let bounds = d.bounds();
                let name = d
                    .uuid()
                    .map(|uuid| uuid.to_string())
                    .unwrap_or_else(|_| format!("display-{:?}", d.id()));
                Mon {
                    name,
                    x: f32::from(bounds.origin.x),
                    y: f32::from(bounds.origin.y),
                    w: f32::from(bounds.size.width),
                    h: f32::from(bounds.size.height),
                }
            })
            .collect(),
    )
}

/// The display the cursor is on.
pub(crate) fn cursor_monitor(cx: &App) -> Option<Mon> {
    let cursor = cursor_global()?;
    let (cx_, cy_) = (f32::from(cursor.x), f32::from(cursor.y));
    monitors(cx)?
        .into_iter()
        .find(|m| cx_ >= m.x && cx_ < m.x + m.w && cy_ >= m.y && cy_ < m.y + m.h)
}

// --- cursor ground truth (the gesture poll loop's global truth) ---

#[cfg(not(target_os = "macos"))]
pub(crate) fn cursor_global() -> Option<Point<Pixels>> {
    let pos = run_capture("hyprctl", &["cursorpos"])?;
    let (x, y) = pos.trim().split_once(',')?;
    Some(point(
        px(x.trim().parse::<f32>().ok()?),
        px(y.trim().parse::<f32>().ok()?),
    ))
}

#[cfg(target_os = "macos")]
pub(crate) fn cursor_global() -> Option<Point<Pixels>> {
    use objc2_app_kit::{NSEvent, NSScreen};

    // `NSEvent::mouseLocation` is Cocoa global space: bottom-left of the
    // primary screen, y up. impin wants top-left-origin, y down.
    let mtm = objc2::MainThreadMarker::new().expect("impin runs on the main thread");
    let location = NSEvent::mouseLocation();
    let screens = NSScreen::screens(mtm);
    if screens.len() == 0 {
        return None;
    }
    let primary = screens.objectAtIndex(0).frame();
    Some(point(
        px(location.x as f32),
        px((primary.origin.y + primary.size.height - location.y) as f32),
    ))
}

// --- clipboard images ---

/// Clipboard image (or a copied image file) -> content-addressed path under
/// the images store; the `clipboard` verb pins it.
#[cfg(not(target_os = "macos"))]
pub(crate) fn clipboard_image_path() -> anyhow::Result<PathBuf> {
    for ty in ["image/png", "image/jpeg"] {
        let Some(bytes) = run_capture_bytes("wl-paste", &["-t", ty]) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        let ext = ty.split_once('/').unwrap().1;
        return store_clipboard_bytes(&bytes, ext);
    }
    // No image bytes on the clipboard — a copied *file* whose path points at
    // a decodable image pins without duplicating bytes (file-manager paste).
    if let Some(path) = clipboard_file_path() {
        return Ok(path);
    }
    anyhow::bail!("clipboard holds no image (copy image data or an image file)")
}

#[cfg(not(target_os = "macos"))]
fn clipboard_file_path() -> Option<PathBuf> {
    for ty in ["text/uri-list", "text/plain;charset=utf-8", "text/plain"] {
        let Some(bytes) = run_capture_bytes("wl-paste", &["-t", ty]) else {
            continue;
        };
        for line in String::from_utf8_lossy(&bytes).lines() {
            let trimmed = line.trim();
            let raw = trimmed.strip_prefix("file://").unwrap_or(trimmed);
            if raw.is_empty() {
                continue;
            }
            let path = percent_decode(raw);
            if path.is_file() && crate::content::dims(&path).is_ok() {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
pub(crate) fn clipboard_image_path() -> anyhow::Result<PathBuf> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeTIFF};

    unsafe {
        let board = NSPasteboard::generalPasteboard();
        // PNG stays raw; screenshots and most copy-image flows hand us TIFF,
        // which is normalized to PNG so the store stays uniform.
        for (paste_type, ext) in [
            (NSPasteboardTypePNG, "png"),
            (NSPasteboardTypeTIFF, "tiff"),
        ] {
            let Some(data) = board.dataForType(paste_type) else {
                continue;
            };
            let bytes = data.to_vec();
            if bytes.is_empty() {
                continue;
            }
            if ext == "tiff" {
                let image = image::load_from_memory(&bytes).context("clipboard TIFF decode")?;
                let mut png = std::io::Cursor::new(Vec::new());
                image
                    .write_to(&mut png, image::ImageFormat::Png)
                    .context("clipboard PNG encode")?;
                return store_clipboard_bytes(&png.into_inner(), "png");
            }
            return store_clipboard_bytes(&bytes, ext);
        }
    }
    anyhow::bail!("clipboard holds no image (copy image data or an image file)")
}

/// `images/<blake3>.<ext>` (spec: content-addressed clipboard image store).
fn store_clipboard_bytes(bytes: &[u8], ext: &str) -> anyhow::Result<PathBuf> {
    let hash = blake3::hash(bytes).to_hex();
    let dir = state::images_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{hash}.{ext}"));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

// --- Hyprland helpers (Linux arm only) ---

#[cfg(not(target_os = "macos"))]
fn run_capture_bytes(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let out = std::process::Command::new(program).args(args).output().ok()?;
    out.status.success().then(|| out.stdout)
}

#[cfg(not(target_os = "macos"))]
fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn num(value: &serde_json::Value, key: &str) -> Option<f32> {
    value.get(key)?.as_f64().map(|v| v as f32)
}

/// Minimal %XX decoding for file:// URIs (spaces are the common case).
#[cfg(not(target_os = "macos"))]
fn percent_decode(raw: &str) -> PathBuf {
    if !raw.contains('%') {
        return PathBuf::from(raw);
    }
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(v) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    PathBuf::from(String::from_utf8_lossy(&out).into_owned())
}

/// Map a persisted output name to a gpui display (by global origin).
pub(crate) fn display_for_name(cx: &App, name: &str) -> Option<DisplayId> {
    let mon = monitors(cx)?.into_iter().find(|m| m.name == name)?;
    display_for_origin(cx, mon.x, mon.y)
}

fn display_for_origin(cx: &App, x: f32, y: f32) -> Option<DisplayId> {
    cx.displays()
        .into_iter()
        .find(|d| {
            let o = d.bounds().origin;
            (f32::from(o.x) - x).abs() <= 1.0 && (f32::from(o.y) - y).abs() <= 1.0
        })
        .map(|d| d.id())
}

pub(crate) fn primary_display(cx: &App) -> Option<std::rc::Rc<dyn gpui::PlatformDisplay>> {
    cx.displays().into_iter().next()
}
