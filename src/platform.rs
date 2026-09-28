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

// --- global hotkey (macOS: Carbon RegisterEventHotKey, no Accessibility
// permission needed; Linux: compositor binds, e.g. Hyprland `bind = $mainMod,
// I, exec, impin toggle`) ---

#[cfg(target_os = "macos")]
pub(crate) fn install_toggle_hotkey(handler: Box<dyn Fn() + Send + Sync>) -> bool {
    use std::sync::OnceLock;

    static HANDLER: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

    // Carbon's RegisterEventHotKey + application-event-target handler is
    // silently never delivered on some systems (observed on this Hackintosh
    // setup), so use the modern route: an active CGEventTap. It requires
    // Input Monitoring / Accessibility for the responsible process (here:
    // OpenCode, which is already granted). Matching combos are swallowed so
    // the frontmost app never sees them; everything else passes through.
    const K_CG_SESSION_EVENT_TAP: u32 = 1;
    const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
    const K_CG_EVENT_TAP_OPTION_DEFAULT: u32 = 0; // active: may swallow events
    const K_CG_EVENT_KEY_DOWN: u64 = 1 << 10;
    const K_CG_EVENT_FLAGS_CHANGED: u64 = 1 << 12;
    // kCGKeyboardEventKeycode
    const K_CG_KEYBOARD_EVENT_KEYCODE: i64 = 9;

    unsafe extern "C" {
        fn CGEventTapCreate(
            tap: u32,
            place: u32,
            options: u32,
            events_of_interest: u64,
            callback: unsafe extern "C" fn(
                *mut std::ffi::c_void,
                u32,
                *mut std::ffi::c_void,
                *mut std::ffi::c_void,
            ) -> *mut std::ffi::c_void,
            user_info: *mut std::ffi::c_void,
        ) -> *mut std::ffi::c_void;
        fn CGEventTapEnable(tap: *mut std::ffi::c_void, enable: bool);
        fn CGEventGetIntegerValueField(event: *mut std::ffi::c_void, field: i64) -> i64;
        fn CGEventGetFlags(event: *mut std::ffi::c_void) -> u64;
        fn CFMachPortCreateRunLoopSource(
            allocator: *mut std::ffi::c_void,
            port: *mut std::ffi::c_void,
            order: i64,
        ) -> *mut std::ffi::c_void;
        fn CFRunLoopGetMain() -> *mut std::ffi::c_void;
        fn CFRunLoopAddSource(
            run_loop: *mut std::ffi::c_void,
            source: *mut std::ffi::c_void,
            mode: *mut std::ffi::c_void,
        );
    }

    static KEY_FLAGS: OnceLock<(u32, u64)> = OnceLock::new(); // (keycode, cg-flags)

    unsafe extern "C" fn on_tap(
        _proxy: *mut std::ffi::c_void,
        event_type: u32,
        event: *mut std::ffi::c_void,
        _user: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void {
        // Runs on the main run loop. Active tap: swallow exactly our combo
        // (return NULL) so the frontmost app never sees it, and pass
        // everything else through untouched.
        if event_type == 10
            && let Some(&(key_code, flags)) = KEY_FLAGS.get()
            && unsafe { CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_KEYCODE) }
                == key_code as i64
            && unsafe { CGEventGetFlags(event) } & flags == flags
        {
            log::debug!("hotkey fired");
            if let Some(handler) = HANDLER.get() {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler()));
                if result.is_err() {
                    log::warn!("toggle hotkey handler panicked (suppressed)");
                }
            }
            return std::ptr::null_mut();
        }
        event
    }

    /// `IMPIN_HOTKEY` spec: `+`-separated modifiers + one key, e.g.
    /// `ctrl+cmd+i`, `cmd+i`, `alt+cmd+k`. `win` is an alias for `cmd`.
    /// Keys: single letters and digits (virtual keycodes; layout-dependent
    /// beyond ANSI).
    fn parse_combo(spec: &str) -> Option<(u32, u32, String)> {
        const CMD: u32 = 1 << 8;
        const SHIFT: u32 = 1 << 9;
        const ALT: u32 = 1 << 11;
        const CTRL: u32 = 1 << 12;
        let mut mods = 0u32;
        let mut key: Option<u32> = None;
        for token in spec.to_ascii_lowercase().split('+') {
            match token {
                "cmd" | "win" => mods |= CMD,
                "ctrl" | "control" => mods |= CTRL,
                "alt" | "opt" | "option" => mods |= ALT,
                "shift" => mods |= SHIFT,
                letter if letter.len() == 1 && letter.as_bytes()[0].is_ascii_alphabetic() => {
                    const CODES: &[(u8, u32)] = &[
                        (b'a', 0x00), (b's', 0x01), (b'd', 0x02), (b'f', 0x03),
                        (b'h', 0x04), (b'g', 0x05), (b'z', 0x06), (b'x', 0x07),
                        (b'c', 0x08), (b'v', 0x09), (b'b', 0x0b), (b'q', 0x0c),
                        (b'w', 0x0d), (b'e', 0x0e), (b'r', 0x0f), (b'y', 0x10),
                        (b't', 0x11), (b'o', 0x1f), (b'u', 0x20), (b'i', 0x22),
                        (b'p', 0x23), (b'l', 0x25), (b'j', 0x26), (b'k', 0x28),
                        (b'n', 0x2d), (b'm', 0x2e),
                    ];
                    key = CODES
                        .iter()
                        .find(|(ch, _)| *ch == letter.as_bytes()[0])
                        .map(|(_, code)| *code);
                }
                digit if digit.len() == 1 && digit.as_bytes()[0].is_ascii_digit() => {
                    // kVK_ANSI_0..9 = 29,18,19,20,21,23,22,26,28,25.
                    const CODES: &[u32] = &[29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
                    key = digit
                        .bytes()
                        .next()
                        .map(|b| CODES[(b - b'0') as usize]);
                }
                _ => return None,
            }
        }
        let key = key?;
        Some((key, mods, spec.to_string()))
    }

    let _ = HANDLER.set(handler);
    let (key_code, mods, label) = match std::env::var("IMPIN_HOTKEY") {
        Ok(spec) if !spec.trim().is_empty() => match parse_combo(spec.trim()) {
            Some(combo) => combo,
            None => {
                log::warn!("IMPIN_HOTKEY={spec:?} not understood (want e.g. cmd+i); using ctrl+cmd+i");
                (0x22, (1 << 12) | (1 << 8), "ctrl+cmd+i".to_string())
            }
        },
        _ => (0x22, (1 << 12) | (1 << 8), "ctrl+cmd+i".to_string()),
    };

    // Carbon modifier bits -> CGEventFlags bits.
    // cmd 1<<8 -> 1<<20, shift 1<<9 -> 1<<17, alt 1<<11 -> 1<<19,
    // ctrl 1<<12 -> 1<<18.
    let cg_flags = ((mods & (1 << 8)) != 0) as u64 * (1 << 20)
        | ((mods & (1 << 9)) != 0) as u64 * (1 << 17)
        | ((mods & (1 << 11)) != 0) as u64 * (1 << 19)
        | ((mods & (1 << 12)) != 0) as u64 * (1 << 18);
    let _ = KEY_FLAGS.set((key_code, cg_flags));

    unsafe {
        let tap = CGEventTapCreate(
            K_CG_SESSION_EVENT_TAP,
            K_CG_HEAD_INSERT_EVENT_TAP,
            K_CG_EVENT_TAP_OPTION_DEFAULT,
            K_CG_EVENT_KEY_DOWN | K_CG_EVENT_FLAGS_CHANGED,
            on_tap,
            std::ptr::null_mut(),
        );
        if tap.is_null() {
            log::warn!(
                "CGEventTapCreate returned null; grant Input Monitoring/Accessibility to the \
                 host process to enable the global toggle hotkey"
            );
            return false;
        }
        CGEventTapEnable(tap, true);
        let source = CFMachPortCreateRunLoopSource(std::ptr::null_mut(), tap, 0);
        if source.is_null() {
            log::warn!("CFMachPortCreateRunLoopSource failed; global toggle unavailable");
            return false;
        }
        CFRunLoopAddSource(CFRunLoopGetMain(), source, k_cf_run_loop_common_modes());
        log::info!("global toggle hotkey registered: {label} (CGEventTap)");
        true
    }
}

/// `kCFRunLoopCommonModes` as a CFStringRef global.
#[cfg(target_os = "macos")]
unsafe fn k_cf_run_loop_common_modes() -> *mut std::ffi::c_void {
    unsafe extern "C" {
        static kCFRunLoopCommonModes: *mut std::ffi::c_void;
    }
    unsafe { kCFRunLoopCommonModes }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn install_toggle_hotkey(_handler: Box<dyn Fn() + Send + Sync>) -> bool {
    false // compositor-side binding (see README)
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
