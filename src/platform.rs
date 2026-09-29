//! Per-platform windowing shims, so the rest of impin stays single-source.
//!
//! Three arms: Wayland + Hyprland (the original), macOS AppKit, and Windows
//! (Win32; overlay PopUp windows). All speak the same vocabulary:
//!
//! - A display (`Mon`) lives in *global top-left-origin screen coordinates*
//!   (Hyprland's global layout; `CGDisplayBounds` on macOS), with a stable
//!   `name` doubling as the persistence key (output name / display UUID).
//! - The cursor (`cursor_global`) is reported in that same space.
//! - Pin/notice windows are overlay surfaces that never steal focus
//!   (layer-shell `OnDemand` / `NSPanel` `NonactivatingPanel`) and float
//!   above fullscreen apps on every workspace/desktop.

use std::path::PathBuf;

#[cfg(any(target_os = "macos", target_os = "windows"))]
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
#[cfg(target_os = "linux")]
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

#[cfg(target_os = "windows")]
pub(crate) fn pin_kind(_pos: Point<Pixels>) -> WindowKind {
    // Topmost toolwindow (WS_EX_TOPMOST|WS_EX_TOOLWINDOW, borderless,
    // SW_SHOWNOACTIVATE): the fork's PopUp on Windows is the never-steals-
    // focus overlay. set_position re-asserts the topmost band, so raising
    // by click falls out for free.
    WindowKind::PopUp
}

/// Notice pill: centered by the compositor on Wayland (all-edge anchors);
/// opened centered on macOS/Windows (fixed size, never moves afterwards).
#[cfg(target_os = "linux")]
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

#[cfg(target_os = "windows")]
pub(crate) fn notice_kind() -> WindowKind {
    WindowKind::PopUp
}

// --- daemon mode ---

/// Keep the daemon out of the Dock and the cmd-tab switcher (the moral
/// equivalent of a compositor session without a toplevel).
///
/// macOS: deliberately EMPTY — the Accessory policy goes through
/// `application().with_activation_policy` in daemon.rs. Calling
/// `NSApplication::sharedApplication` here (as this once did) creates
/// the shared app as plain `NSApplication` before gpui runs, and the
/// fork's `GPUIApplication` ivars write then corrupts the heap
/// (guard-malloc-verified 2026-09-29).
#[cfg(target_os = "macos")]
pub(crate) fn accessory_mode() {}

#[cfg(not(target_os = "macos"))]
pub(crate) fn accessory_mode() {}

// --- displays ---

#[cfg(target_os = "linux")]
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

/// EnumDisplayMonitors + per-monitor DPI: same logical space as the fork's
/// `WindowsDisplay::bounds` (each origin/size divided by its own factor).
/// Persistence key = the device name (`\\.\DISPLAY1`), stable across
/// reboots (Q15); HMONITOR values are not.
#[cfg(target_os = "windows")]
pub(crate) fn monitors(_cx: &App) -> Option<Vec<Mon>> {
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    };
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    unsafe extern "system" fn collect(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> windows::core::BOOL {
        let mons = data.0 as *mut Vec<Mon>;
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(hmon, &mut info as *mut _ as *mut MONITORINFO) }.as_bool() {
            // windows-rs 0.62: raw out-params (fork's 0.61 returns Result).
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
            let _ = dy;
            let dpi = dx as f32;
            let s = (dpi / 96.0).max(0.5);
            let r = info.monitorInfo.rcMonitor;
            let name = String::from_utf16_lossy(&info.szDevice)
                .trim_end_matches('\0')
                .to_string();
            unsafe {
                (*mons).push(Mon {
                    name,
                    x: r.left as f32 / s,
                    y: r.top as f32 / s,
                    w: (r.right - r.left) as f32 / s,
                    h: (r.bottom - r.top) as f32 / s,
                });
            }
        }
        windows::core::BOOL(1)
    }

    let mut mons: Vec<Mon> = Vec::new();
    unsafe {
        EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut mons as *mut _ as _))
            .ok()
            .ok()?;
    }
    (!mons.is_empty()).then_some(mons)
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

#[cfg(target_os = "linux")]
pub(crate) fn cursor_global() -> Option<Point<Pixels>> {
    let pos = run_capture("hyprctl", &["cursorpos"])?;
    let (x, y) = pos.trim().split_once(',')?;
    Some(point(
        px(x.trim().parse::<f32>().ok()?),
        px(y.trim().parse::<f32>().ok()?),
    ))
}

#[cfg(target_os = "windows")]
pub(crate) fn cursor_global() -> Option<Point<Pixels>> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    // Physical px, top-left origin, y down. Dividing by the cursor
    // monitor's DPI factor lands in the same logical space as `Mon`
    // (each monitor's origin is divided by its own factor; within one
    // monitor that transform is affine, so p/s is exact).
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p) }.ok()?;
    let scale = cursor_scale(p);
    Some(point(px(p.x as f32 / scale), px(p.y as f32 / scale)))
}

/// DPI scale factor of the monitor at `p` (Windows; 1.0 if undetectable).
#[cfg(target_os = "windows")]
fn cursor_scale(p: windows::Win32::Foundation::POINT) -> f32 {
    use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST};
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    let mon = unsafe { MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST) };
    if mon.is_invalid() {
        return 1.0;
    }
    // windows-rs 0.62: raw out-params (the fork's 0.61 returns a Result).
    let (mut dx, mut dy) = (96u32, 96u32);
    let _ = unsafe { GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
    let _ = dy;
    (dx as f32 / 96.0).max(0.5)
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
#[cfg(target_os = "linux")]
pub(crate) fn clipboard_image_path(_cx: &App) -> anyhow::Result<PathBuf> {
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

#[cfg(target_os = "linux")]
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
pub(crate) fn clipboard_image_path(_cx: &App) -> anyhow::Result<PathBuf> {
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

/// `cx.read_from_clipboard()` (fork: CF_DIB arrives BMP-wrapped, PNG/JPEG
/// raw). Everything normalizes to PNG for the store (like macOS TIFF); a
/// copied image *file* (Explorer copy) pins without duplicating bytes.
#[cfg(target_os = "windows")]
pub(crate) fn clipboard_image_path(cx: &App) -> anyhow::Result<PathBuf> {
    use gpui::{ClipboardEntry, ImageFormat};

    let Some(item) = cx.read_from_clipboard() else {
        anyhow::bail!("clipboard holds no image (copy image data or an image file)");
    };
    for entry in item.entries() {
        match entry {
            ClipboardEntry::Image(image) => {
                let png = match image.format {
                    ImageFormat::Png => image.bytes().to_vec(),
                    other => {
                        let fmt = match other {
                            ImageFormat::Jpeg => image::ImageFormat::Jpeg,
                            ImageFormat::Gif => image::ImageFormat::Gif,
                            ImageFormat::Bmp => image::ImageFormat::Bmp,
                            ImageFormat::Tiff => image::ImageFormat::Tiff,
                            ImageFormat::Webp => image::ImageFormat::WebP,
                            _ => anyhow::bail!("unsupported clipboard image format {other:?}"),
                        };
                        let decoded = image::load_from_memory_with_format(image.bytes(), fmt)
                            .context("clipboard image decode")?;
                        let mut png = Vec::new();
                        decoded
                            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                            .context("clipboard PNG encode")?;
                        png
                    }
                };
                return store_clipboard_bytes(&png, "png");
            }
            ClipboardEntry::ExternalPaths(paths) => {
                // Any copied file path qualifies (Explorer "copy"): the
                // extension decides at add time. dims() here would reject
                // every format the sniffing reader can't see (AVIF/JXL).
                if let Some(path) = paths.paths().iter().find(|p| p.is_file()) {
                    return Ok(path.clone());
                }
            }
            ClipboardEntry::String(_) => {}
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

/// Windows: `RegisterHotKey` on a dedicated message-loop thread (no
/// accessibility permissions needed). Default win+ctrl+i; `IMPIN_HOTKEY`
/// override (e.g. `win+alt+k`, `ctrl+shift+p`); `win` = the Windows key.
#[cfg(target_os = "windows")]
pub(crate) fn install_toggle_hotkey(handler: Box<dyn Fn() + Send + Sync>) -> bool {
    use std::sync::OnceLock;

    use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, HOT_KEY_MODIFIERS};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, TranslateMessage, MSG, WM_HOTKEY,
    };

    static HANDLER: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

    /// `IMPIN_HOTKEY` spec: `+`-separated modifiers + one key, e.g.
    /// `win+ctrl+i`. Keys: single letters and digits (layout-independent
    /// virtual keys).
    fn parse_combo(spec: &str) -> Option<(i32, HOT_KEY_MODIFIERS, String)> {
        const MOD_ALT: u32 = 1;
        const MOD_CONTROL: u32 = 2;
        const MOD_SHIFT: u32 = 4;
        const MOD_WIN: u32 = 8;
        let mut mods = 0u32;
        let mut key: Option<i32> = None;
        for token in spec.to_ascii_lowercase().split('+') {
            match token {
                "win" | "super" | "meta" => mods |= MOD_WIN,
                "ctrl" | "control" => mods |= MOD_CONTROL,
                "alt" => mods |= MOD_ALT,
                "shift" => mods |= MOD_SHIFT,
                letter if letter.len() == 1 && letter.as_bytes()[0].is_ascii_alphabetic() => {
                    key = Some(letter.as_bytes()[0].to_ascii_uppercase() as i32);
                }
                digit if digit.len() == 1 && digit.as_bytes()[0].is_ascii_digit() => {
                    key = Some(digit.as_bytes()[0] as i32);
                }
                _ => return None,
            }
        }
        Some((key?, HOT_KEY_MODIFIERS(mods), spec.to_string()))
    }

    let _ = HANDLER.set(handler);
    let (vk, mods, label) = match std::env::var("IMPIN_HOTKEY") {
        Ok(spec) if !spec.trim().is_empty() => match parse_combo(spec.trim()) {
            Some(combo) => combo,
            None => {
                log::warn!(
                    "IMPIN_HOTKEY={spec:?} not understood (want e.g. win+ctrl+i); using win+ctrl+i"
                );
                (0x49, HOT_KEY_MODIFIERS(2 | 8), "win+ctrl+i".to_string())
            }
        },
        _ => (0x49, HOT_KEY_MODIFIERS(2 | 8), "win+ctrl+i".to_string()),
    };

    // The thread dies only with the process; GetMessageW parks it for free.
    std::thread::Builder::new()
        .name("impin-hotkey".into())
        .spawn(move || unsafe {
            if RegisterHotKey(None, 1, mods, vk as u32).is_err() {
                log::warn!("RegisterHotKey failed; global toggle unavailable");
                return;
            }
            log::info!("global toggle hotkey registered: {label} (RegisterHotKey)");
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                if msg.message == WM_HOTKEY
                    && let Some(handler) = HANDLER.get()
                {
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler()));
                    if result.is_err() {
                        log::warn!("toggle hotkey handler panicked (suppressed)");
                    }
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        })
        .is_ok()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn install_toggle_hotkey(_handler: Box<dyn Fn() + Send + Sync>) -> bool {
    false // compositor-side binding (see README)
}

// --- Hyprland helpers (Linux arm only) ---

#[cfg(target_os = "linux")]
fn run_capture_bytes(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let out = std::process::Command::new(program).args(args).output().ok()?;
    out.status.success().then(|| out.stdout)
}

#[cfg(target_os = "linux")]
fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(target_os = "linux")]
fn num(value: &serde_json::Value, key: &str) -> Option<f32> {
    value.get(key)?.as_f64().map(|v| v as f32)
}

/// Minimal %XX decoding for file:// URIs (spaces are the common case).
#[cfg(target_os = "linux")]
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
