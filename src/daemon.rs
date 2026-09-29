//! impin daemon: owns the pin windows and the CLI transport (Unix socket
//! `$XDG_RUNTIME_DIR/impin.sock`, named pipe `\\.\pipe\impin` on Windows).
//! Single writer for the state file.

#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{App, AsyncApp, DisplayId, Pixels, QuitMode, Size, WindowHandle, point, px, size};
use gpui_platform::application;
use log::{info, warn};

use crate::cli::socket_path;
use crate::content::{self, PinImage};
use crate::notice::{self, Notice};
use crate::pin::{self, PinEvent};
use crate::platform::{self, Mon};
use crate::state::{PinRecord, Store};

#[cfg(windows)]
use windows::Win32::Foundation::HANDLE;

enum Ipc {
    Toggle,
    Show,
    Hide,
    Stop,
    Add {
        path: PathBuf,
        resp: futures::channel::oneshot::Sender<anyhow::Result<String>>,
    },
    Clipboard {
        resp: futures::channel::oneshot::Sender<anyhow::Result<String>>,
    },
}

struct PinEntry {
    /// Stable key matching `Pin::id` (survives record edits).
    id: u64,
    handle: WindowHandle<pin::Pin>,
    record: PinRecord,
    visible: bool,
}

struct PinsGlobal {
    entries: Vec<PinEntry>,
    /// Records whose output was absent at boot (spec: they stay hidden
    /// until it reappears; toggle/show re-checks).
    pending: Vec<PinRecord>,
    store: Store,
    /// Sender handed to every pin window (persist/lifetime events).
    events: UnboundedSender<PinEvent>,
    /// The "No pinned image" pill; visibility tracks emptiness. Keeping a
    /// window alive also means the process never loses its last window.
    notice: Option<WindowHandle<Notice>>,
    /// Show/hide mode (`show`/`hide`/`toggle`). The pill only shows while
    /// pins are empty AND the daemon is in show mode.
    shown: bool,
    fallback: Option<DisplayId>,
    next_id: u64,
}

impl gpui::Global for PinsGlobal {}

pub fn run() -> anyhow::Result<()> {
    // First line out: crashes during AppKit/XPC init (machine-level flakes,
    // zero output before this existed) are at least attributable to a pid.
    info!("daemon starting (pid {})", std::process::id());
    // Accessory before any window exists: no Dock icon, no menu bar.
    platform::accessory_mode();
    let sock = socket_path();
    #[cfg(unix)]
    let listener = bind_transport(&sock)?;
    #[cfg(windows)]
    bind_transport(&sock)?;
    let (tx, mut rx) = unbounded::<Ipc>();

    // Quit only on `impin stop`: deleting the last pin must leave a live
    // daemon (the notice pill keeps a window alive meanwhile).
    // macOS: accessory policy via gpui's builder — NOT by touching
    // NSApplication early (instantiating the shared app before
    // application().run() makes the fork's GPUIApplication ivars write
    // out of bounds; see platform::accessory_mode docs).
    #[cfg(target_os = "macos")]
    let app = application().with_activation_policy(gpui::MacActivationPolicy::Accessory);
    #[cfg(not(target_os = "macos"))]
    let app = application();
    app.with_quit_mode(QuitMode::Explicit)
        .run(move |cx: &mut App| {
            // Global toggle hotkey (macOS CGEventTap, Windows RegisterHotKey
            // thread): macOS Ctrl+Cmd+I / Windows Win+Ctrl+I by default,
            // override with IMPIN_HOTKEY (e.g. `cmd+i` / `win+alt+k`).
            // Registered here — after app/initialization — and forwarded
            // through the same channel as the CLI's `toggle` verb;
            // debounced because a held combo auto-repeats ~30 taps/s.
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                let hotkey_tx = tx.clone();
                let last_fire = Arc::new(AtomicU64::new(0));
                platform::install_toggle_hotkey(Box::new(move || {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let last = last_fire.load(Ordering::Relaxed);
                    if now.saturating_sub(last) < 200 {
                        return; // key auto-repeat
                    }
                    last_fire.store(now, Ordering::Relaxed);
                    let _ = hotkey_tx.unbounded_send(Ipc::Toggle);
                }));
            }
            #[cfg(unix)]
            std::thread::spawn(move || accept_loop(listener, tx));
            // Windows: the pipe server loop creates its own instances.
            #[cfg(windows)]
            std::thread::spawn(move || pipe_accept_loop(tx));

            let store = Store::new();
            let records = match store.load() {
                Ok(records) => records,
                Err(err) => {
                    // Never clobber a corrupt state file: move it aside so
                    // the next save starts clean and nothing is lost.
                    warn!("state file unreadable: {err:#}");
                    let path = store.path().to_path_buf();
                    std::fs::rename(&path, path.with_extension("toml.corrupt")).ok();
                    Vec::new()
                }
            };
            let fallback = platform::primary_display(cx).map(|d| d.id());
            // The notice pill: created hidden, shown while zero pins exist. It
            // also guarantees a window always exists (Explicit quit mode).
            let notice_window = notice::spawn(cx, fallback)
                .map_err(|err| warn!("notice window: {err:#}"))
                .ok();
            // Pins -> daemon events (persist/lifetime); created before restore
            // so spawned pins can send immediately.
            let (event_tx, mut event_rx) = unbounded::<PinEvent>();
            let mons = platform::monitors(cx).unwrap_or_default();
            let (mut entries, mut pending) = (Vec::new(), Vec::new());
            let mut id_counter = 0u64;
            for mut record in records {
                let id = id_counter;
                id_counter += 1;
                // Missing output at boot: the pin waits (hidden) until its
                // output reappears — re-checked on toggle/show (spec).
                if !mons.iter().any(|m| m.name == record.output) {
                    pending.push(record);
                    continue;
                }
                let (pin_image, natural) = load_content(&record);
                let mon = mons.iter().find(|m| m.name == record.output);
                // Spawn clamped: every pin must be fully on-screen (spec).
                if let Some(mon) = mon {
                    clamp_to_mon(&mut record, mon);
                }
                let output_origin = mon
                    .map(|m| point(px(m.x), px(m.y)))
                    .unwrap_or(point(px(0.), px(0.)));
                let display_id = platform::display_for_name(cx, &record.output).or(fallback);
                match pin::spawn(
                    cx,
                    id,
                    &record,
                    pin_image,
                    natural,
                    output_origin,
                    display_id,
                    event_tx.clone(),
                ) {
                    Ok(handle) => entries.push(PinEntry {
                        id,
                        handle,
                        record,
                        visible: true,
                    }),
                    Err(err) => warn!("spawning pin {}: {err:#}", record.source.display()),
                }
            }
            log::info!(
                "restored {} pin(s), {} waiting for output",
                entries.len(),
                pending.len()
            );
            let next_id = entries.len() as u64;
            cx.set_global(PinsGlobal {
                entries,
                pending,
                store,
                events: event_tx,
                notice: notice_window,
                shown: true,
                fallback,
                next_id,
            });
            sync_notice(cx);

            // Window events -> persistence / lifetime.
            cx.spawn(async move |cx: &mut AsyncApp| {
                while let Some(ev) = event_rx.next().await {
                    cx.update(|app| handle_pin_event(app, ev));
                }
            })
            .detach();

            // Socket thread -> this task (fork §16.1: never block inside spawn).
            cx.spawn(async move |cx: &mut AsyncApp| {
                while let Some(msg) = rx.next().await {
                    match msg {
                        Ipc::Stop => {
                            if let Err(err) = cx.update(save_all) {
                                warn!("saving pins: {err:#}");
                            }
                            cx.update(|app| app.quit());
                            break;
                        }
                        Ipc::Toggle | Ipc::Show | Ipc::Hide => {
                            let toggle = matches!(msg, Ipc::Toggle);
                            let forced = matches!(msg, Ipc::Show);
                            cx.update(|app| {
                                // Outputs may have reappeared: spawn pins
                                // that were waiting for theirs (spec).
                                spawn_pending(app);
                                let mons = platform::monitors(app).unwrap_or_default();
                                let mut updates = Vec::new();
                                {
                                    let global = app.global_mut::<PinsGlobal>();
                                    // Track show mode for the pill: `hide`
                                    // hides it, `toggle`/`show` restore it.
                                    global.shown = if forced {
                                        true
                                    } else if toggle {
                                        !global.shown
                                    } else {
                                        false
                                    };
                                    for entry in &mut global.entries {
                                        let target = if toggle { !entry.visible } else { forced };
                                        if target == entry.visible {
                                            continue;
                                        }
                                        entry.visible = target;
                                        // Toggle-on re-check: every pin must be
                                        // fully on-screen (covers resolution/
                                        // output changes while hidden).
                                        let mut geom = None;
                                        if target {
                                            if let Some(mon) =
                                                mons.iter().find(|m| m.name == entry.record.output)
                                            {
                                                let mut rec = entry.record.clone();
                                                clamp_to_mon(&mut rec, mon);
                                                if rec != entry.record {
                                                    entry.record = rec.clone();
                                                    geom = Some(rec);
                                                }
                                            }
                                        }
                                        updates.push((entry.handle, target, geom));
                                    }
                                }
                                let mut dirty = false;
                                for (handle, target, geom) in updates {
                                    if geom.is_some() {
                                        dirty = true;
                                    }
                                    let _ = handle.update(app, |pin, window, cx| {
                                        window.set_visible(target);
                                        if let Some((x, y, w, h)) =
                                            geom.map(|r| (r.x, r.y, r.w, r.h))
                                        {
                                            pin.apply_geometry(x, y, w, h, cx);
                                        }
                                    });
                                }
                                if dirty {
                                    if let Err(err) = persist(app.global_mut::<PinsGlobal>()) {
                                        warn!("saving pins: {err:#}");
                                    }
                                }
                                sync_notice(app);
                            });
                        }
                        Ipc::Add { path, resp } => {
                            let _ = resp.send(cx.update(|app| add_pin(app, path)));
                        }
                        Ipc::Clipboard { resp } => {
                            let _ = resp.send(cx.update(|app| add_clipboard(app)));
                        }
                    }
                }
            })
            .detach();
        });

    std::fs::remove_file(&sock).ok();
    log::info!("socket removed, daemon stopped");
    Ok(())
}

fn save_all(app: &mut App) -> anyhow::Result<()> {
    persist(app.global_mut::<PinsGlobal>())
}

/// Mirror entries -> records (z = sibling order) and write atomically.
fn persist(global: &PinsGlobal) -> anyhow::Result<()> {
    let mut records: Vec<PinRecord> = global.entries.iter().map(|e| e.record.clone()).collect();
    records.extend(global.pending.iter().cloned());
    for (i, record) in records.iter_mut().enumerate() {
        record.z = i as u32;
    }
    global.store.save(&records)
}

/// Decode (or measure) a record's content; a vanished file renders the dim
/// missing placeholder at its stored size (spec).
fn load_content(record: &PinRecord) -> (PinImage, Size<Pixels>) {
    match content::load(&record.source) {
        Ok(loaded) => (
            loaded.render,
            size(px(loaded.natural.0 as f32), px(loaded.natural.1 as f32)),
        ),
        Err(err) => {
            warn!("pin source {}: {err:#}", record.source.display());
            (
                PinImage::Missing,
                size(px(record.w as f32), px(record.h as f32)),
            )
        }
    }
}

/// Spawn a window for `record` (clamped to its output when known), register
/// the entry, persist. `visible: false` adopts the current hide mode.
fn spawn_entry(
    app: &mut App,
    record: &mut PinRecord,
    pin_image: PinImage,
    natural: Size<Pixels>,
    visible: bool,
) -> anyhow::Result<()> {
    let mon = platform::monitors(app)
        .unwrap_or_default()
        .into_iter()
        .find(|m| m.name == record.output);
    // Every pin must be fully on-screen (spec).
    if let Some(mon) = &mon {
        clamp_to_mon(record, mon);
    }
    let (id, fallback, events) = {
        let global = app.global::<PinsGlobal>();
        (global.next_id, global.fallback, global.events.clone())
    };
    let display_id = platform::display_for_name(app, &record.output).or(fallback);
    let output_origin = mon
        .map(|m| point(px(m.x), px(m.y)))
        .unwrap_or(point(px(0.), px(0.)));
    let handle = pin::spawn(
        app,
        id,
        record,
        pin_image,
        natural,
        output_origin,
        display_id,
        events,
    )?;
    if !visible {
        let _ = handle.update(app, |_, window, _| window.set_visible(false));
    }
    let global = app.global_mut::<PinsGlobal>();
    global.next_id += 1;
    global.entries.push(PinEntry {
        id,
        handle,
        record: record.clone(),
        visible,
    });
    persist(&global)?;
    Ok(())
}

/// Spawn waiting pins whose output has appeared (spec: missing-output pins
/// stay hidden until it reappears; toggle/show re-checks).
fn spawn_pending(app: &mut App) {
    let (waiting, shown) = {
        let mons = platform::monitors(app).unwrap_or_default();
        let global = app.global_mut::<PinsGlobal>();
        let (mut waiting, mut remaining) = (Vec::new(), Vec::new());
        for record in global.pending.drain(..) {
            if mons.iter().any(|m| m.name == record.output) {
                waiting.push(record);
            } else {
                remaining.push(record);
            }
        }
        global.pending = remaining;
        (waiting, global.shown)
    };
    for mut record in waiting {
        let (pin_image, natural) = load_content(&record);
        if let Err(err) = spawn_entry(app, &mut record, pin_image, natural, shown) {
            warn!("spawning pin {}: {err:#}", record.source.display());
        }
    }
    sync_notice(app);
}

/// Notice visibility tracks emptiness: shown iff no pins exist.
fn sync_notice(app: &mut App) {
    let (notice, show) = {
        let global = app.global_mut::<PinsGlobal>();
        (
            global.notice,
            global.shown && global.entries.is_empty() && global.pending.is_empty(),
        )
    };
    if let Some(handle) = notice {
        let _ = handle.update(app, |_, window, _| window.set_visible(show));
    }
}

/// Window events: persist view state, raise, delete.
fn handle_pin_event(app: &mut App, ev: PinEvent) {
    match ev {
        PinEvent::Update(id, record) => {
            let global = app.global_mut::<PinsGlobal>();
            if let Some(entry) = global.entries.iter_mut().find(|e| e.id == id) {
                entry.record = record;
            }
            if let Err(err) = persist(global) {
                warn!("saving pins: {err:#}");
            }
        }
        PinEvent::Raise(id) => {
            // Layer-shell has no sibling-reorder request, so raising = move
            // to the end of the entries (z order) + re-map the surface.
            let handle = {
                let global = app.global_mut::<PinsGlobal>();
                let Some(i) = global.entries.iter().position(|e| e.id == id) else {
                    return;
                };
                if i == global.entries.len() - 1 {
                    return; // already top
                }
                let entry = global.entries.remove(i);
                global.entries.push(entry);
                global.entries.last().unwrap().handle
            };
            let _ = handle.update(app, |_, window, _| window.set_visible(false));
            // Map back a tick later so the compositor sees a real
            // unmap+map (a same-tick pair could coalesce into a no-op).
            app.spawn(async move |cx: &mut AsyncApp| {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                cx.update(|app| {
                    // The pin may have been deleted within the tick (click
                    // sends Raise, a fast `q` deletes); don't touch a closed
                    // window — gpui would log "window not found".
                    if app.global::<PinsGlobal>().entries.iter().any(|e| e.id == id) {
                        let _ = handle.update(app, |_, window, _| window.set_visible(true));
                    }
                });
            })
            .detach();
            if let Err(err) = persist(app.global_mut::<PinsGlobal>()) {
                warn!("saving pins: {err:#}");
            }
        }
        PinEvent::Delete(id) => {
            let handle = {
                let global = app.global_mut::<PinsGlobal>();
                let Some(i) = global.entries.iter().position(|e| e.id == id) else {
                    return;
                };
                global.entries.remove(i).handle
            };
            let _ = handle.update(app, |_, window, _| window.remove_window());
            if let Err(err) = persist(app.global_mut::<PinsGlobal>()) {
                warn!("saving pins: {err:#}");
            }
            sync_notice(app); // empty? show the pill, keep the daemon visible
        }
    }
}

/// New pin from a file: default size = natural size capped to 50% of the
/// cursor output, centered under the cursor (output-local coords).
fn add_pin(app: &mut App, path: PathBuf) -> anyhow::Result<String> {
    if !path.is_file() {
        anyhow::bail!("no such file: {}", path.display());
    }
    let loaded = content::load(&path)?;
    let (dw, dh) = (f64::from(loaded.natural.0), f64::from(loaded.natural.1));

    let mon =
        platform::cursor_monitor(app).ok_or_else(|| anyhow::anyhow!("cannot resolve cursor output"))?;
    // Spawn default size: natural capped to 50% of the output, aspect
    // preserved; a pin whose image is larger than that spawns fit — zoom
    // < 1 exactly encodes that (spec).
    let scale = 1.0_f64
        .min(f64::from(mon.w) * 0.5 / dw)
        .min(f64::from(mon.h) * 0.5 / dh);
    let (w, h) = (dw * scale, dh * scale);
    let cursor = platform::cursor_global().ok_or_else(|| anyhow::anyhow!("cannot read cursor"))?;
    let x = f64::from((f32::from(cursor.x) - mon.x).max(0.0)) - w / 2.0;
    let y = f64::from((f32::from(cursor.y) - mon.y).max(0.0)) - h / 2.0;

    let mut record = PinRecord {
        source: path,
        output: mon.name.clone(),
        x,
        y,
        w,
        h,
        z: 0, // persist() assigns sibling order
        zoom: scale as f32,
        pan_x: 0.0,
        pan_y: 0.0,
        opacity: 1.0,
        radius: 0.0,
        bg: true,
    };
    clamp_to_mon(&mut record, &mon);

    let natural = size(px(dw as f32), px(dh as f32));
    spawn_entry(app, &mut record, loaded.render, natural, true)?;
    sync_notice(app); // pins exist again -> hide the pill
    Ok(format!(
        "pinned {} ({}x{})",
        record.source.display(),
        record.w as u32,
        record.h as u32
    ))
}

/// `clipboard` verb: pin whatever image the clipboard holds.
fn add_clipboard(app: &mut App) -> anyhow::Result<String> {
    let path = platform::clipboard_image_path(app)?;
    add_pin(app, path)
}

/// Every pin must be fully on-screen (spec): shrink oversized pins, clamp
/// the origin into the monitor.
fn clamp_to_mon(record: &mut PinRecord, mon: &Mon) {
    record.w = record.w.min(f64::from(mon.w));
    record.h = record.h.min(f64::from(mon.h));
    record.x = record.x.clamp(0.0, f64::from(mon.w) - record.w);
    record.y = record.y.clamp(0.0, f64::from(mon.h) - record.h);
}

#[cfg(unix)]
fn accept_loop(listener: UnixListener, tx: UnboundedSender<Ipc>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { break };
        let mut text = String::new();
        if stream.read_to_string(&mut text).is_err() {
            continue;
        }
        let line = dispatch_verb(&text, &tx);
        let _ = stream.write_all(line.as_bytes());
    }
}

/// Verb table shared by both transports; returns the wire reply line.
fn dispatch_verb(text: &str, tx: &UnboundedSender<Ipc>) -> String {
    let reply: anyhow::Result<String> = match text.trim() {
        "toggle" => forward(tx, Ipc::Toggle),
        "show" => forward(tx, Ipc::Show),
        "hide" => forward(tx, Ipc::Hide),
        "stop" => forward(tx, Ipc::Stop),
        "status" => Ok("running".into()),
        "clipboard" => ask(tx, |resp| Ipc::Clipboard { resp }),
        other => match other.strip_prefix("add ") {
            Some(path) => {
                let path = PathBuf::from(path.trim());
                ask(tx, |resp| Ipc::Add { path, resp })
            }
            None => Err(anyhow::anyhow!("unknown verb")),
        },
    };
    match reply {
        Ok(payload) => format!("ok {payload}\n"),
        Err(err) => format!("err {err:#}\n"),
    }
}

/// Single-instance probe + listener bind.
///
/// Unix: stale-socket probe — a connect that fails means no live daemon is
/// listening, so the socket file is stale and removed before binding.
#[cfg(unix)]
fn bind_transport(sock: &std::path::Path) -> anyhow::Result<UnixListener> {
    if sock.exists() {
        match UnixStream::connect(sock) {
            Ok(_) => anyhow::bail!(
                "another impin daemon is already running ({})",
                sock.display()
            ),
            Err(_) => {
                std::fs::remove_file(sock).ok(); // stale socket
            }
        }
    }
    Ok(UnixListener::bind(sock)?)
}

/// Windows: a named mutex is the single-instance marker (the kernel
/// releases it when the process dies — no stale files to clean). The pipe
/// itself needs no bind; client connect failures mean "not running".
#[cfg(windows)]
fn bind_transport(_sock: &std::path::Path) -> anyhow::Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;

    let name: Vec<u16> = "Local\\impin-daemon\0".encode_utf16().collect();
    // Held for the process lifetime: windows-rs HANDLE has no Drop, and we
    // never CloseHandle, so the kernel object lives until exit.
    let _mutex = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
        .map_err(|e| anyhow::anyhow!("CreateMutexW: {e}"))?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        anyhow::bail!("another impin daemon is already running");
    }
    Ok(())
}

/// Windows named-pipe server: `\\.\pipe\impin`, byte mode, one client at a
/// time (CLI verbs are short; a second concurrent CLI gets "not running" —
/// acceptable for v0.2). Wire protocol: u32-LE length + verb (a byte pipe
/// can't be half-closed like a Unix socket's shutdown(Write), so the length
/// ends the verb); reply + FlushFileBuffers + Disconnect gives the client
/// its EOF.
#[cfg(windows)]
fn pipe_accept_loop(tx: UnboundedSender<Ipc>) {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, CloseHandle, GetLastError};
    use windows::Win32::Storage::FileSystem::{FlushFileBuffers, PIPE_ACCESS_DUPLEX, WriteFile};
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    };

    const PIPE_NAME: &str = "\\\\.\\pipe\\impin";
    // NUL-terminated for PCWSTR — a missing terminator here once made the
    // pipe register as `\\.\pipe\impinTT??u` (heap garbage suffix).
    let name: Vec<u16> = format!("{PIPE_NAME}\0").encode_utf16().collect();
    loop {
        // Returns the bare handle; INVALID_HANDLE_VALUE on failure.
        let pipe: HANDLE = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                None,
            )
        };
        if pipe.is_invalid() {
            log::error!(
                "CreateNamedPipeW failed ({}); IPC disabled",
                unsafe { GetLastError() }.0
            );
            std::process::exit(1);
        }
        // Blocks until a client connects (CreateFile on the CLI side); a
        // client that raced in first surfaces as ERROR_PIPE_CONNECTED.
        if let Err(err) = unsafe { ConnectNamedPipe(pipe, None) }
            && err.code() != windows::core::HRESULT(ERROR_PIPE_CONNECTED.0 as i32)
        {
            let _ = unsafe { DisconnectNamedPipe(pipe) };
            let _ = unsafe { CloseHandle(pipe) };
            continue;
        }
        let verb = read_verb(pipe);
        let line = dispatch_verb(&verb, &tx);
        let mut written: u32 = 0;
        let _ = unsafe { WriteFile(pipe, Some(line.as_bytes()), Some(&mut written), None) };
        let _ = unsafe { FlushFileBuffers(pipe) };
        let _ = unsafe { DisconnectNamedPipe(pipe) };
        let _ = unsafe { CloseHandle(pipe) };
    }
}

/// u32-LE length + verb bytes; a short/garbled read just yields an empty
/// (unknown) verb.
#[cfg(windows)]
fn read_verb(pipe: HANDLE) -> String {
    use windows::Win32::Storage::FileSystem::ReadFile;

    let read_exact = |buf: &mut [u8]| -> bool {
        let mut filled = 0;
        while filled < buf.len() {
            let mut n: u32 = 0;
            let ok = unsafe { ReadFile(pipe, Some(&mut buf[filled..]), Some(&mut n), None) }.is_ok();
            if !ok || n == 0 {
                return false; // client went away mid-verb
            }
            filled += n as usize;
        }
        true
    };
    let mut len_buf = [0u8; 4];
    if !read_exact(&mut len_buf) {
        return String::new();
    }
    let len = u32::from_le_bytes(len_buf).min(64 * 1024) as usize;
    let mut verb = vec![0u8; len];
    if !read_exact(&mut verb) {
        return String::new();
    }
    String::from_utf8_lossy(&verb).into_owned()
}

fn forward(tx: &UnboundedSender<Ipc>, msg: Ipc) -> anyhow::Result<String> {
    tx.unbounded_send(msg)
        .map(|_| "ok".into())
        .map_err(|_| anyhow::anyhow!("daemon shutting down"))
}

/// Forward a verb to the UI task and wait for its reply (blocking the
/// socket thread is fine — the reply comes from the main thread's update).
fn ask(
    tx: &UnboundedSender<Ipc>,
    make: impl FnOnce(futures::channel::oneshot::Sender<anyhow::Result<String>>) -> Ipc,
) -> anyhow::Result<String> {
    let (resp_tx, resp_rx) = futures::channel::oneshot::channel();
    if tx.unbounded_send(make(resp_tx)).is_err() {
        anyhow::bail!("daemon shutting down");
    }
    match futures::executor::block_on(resp_rx) {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("daemon shutting down")),
    }
}

