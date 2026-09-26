//! impin daemon: owns the pin windows and the CLI socket
//! (`$XDG_RUNTIME_DIR/impin.sock`). Single writer for the state file.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{
    App, AsyncApp, DisplayId, Pixels, PlatformDisplay, Point, WindowHandle, point, px, size,
};
use gpui_platform::application;
use log::warn;

use crate::cli::socket_path;
use crate::pin::{self, PinEvent};
use crate::state::{PinRecord, Store};

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
    store: Store,
    /// Sender handed to every pin window (persist/lifetime events).
    events: UnboundedSender<PinEvent>,
    fallback: Option<DisplayId>,
    next_id: u64,
}

impl gpui::Global for PinsGlobal {}

pub fn run() -> anyhow::Result<()> {
    let sock = socket_path();
    if sock.exists() {
        match UnixStream::connect(&sock) {
            Ok(_) => anyhow::bail!(
                "another impin daemon is already running ({})",
                sock.display()
            ),
            Err(_) => {
                std::fs::remove_file(&sock).ok(); // stale socket
            }
        }
    }
    let listener = UnixListener::bind(&sock)?;
    let (tx, mut rx) = unbounded::<Ipc>();

    application().run(move |cx: &mut App| {
        std::thread::spawn(move || accept_loop(listener, tx));

        let store = Store::new();
        let records = store.load().unwrap_or_default();
        let fallback = primary_display(cx).map(|d| d.id());
        // Pins -> daemon events (persist/lifetime); created before restore
        // so spawned pins can send immediately.
        let (event_tx, mut event_rx) = unbounded::<PinEvent>();
        let mons = monitors().unwrap_or_default();
        let mut entries = Vec::new();
        for (id, mut record) in records.into_iter().enumerate() {
            let id = id as u64;
            let mon = mons.iter().find(|m| m.name == record.output);
            // Spawn clamped: every pin must be fully on-screen (spec).
            if let Some(mon) = mon {
                clamp_to_mon(&mut record, mon);
            }
            let output_origin = mon
                .map(|m| point(px(m.x), px(m.y)))
                .unwrap_or(point(px(0.), px(0.)));
            // Natural size feeds zoom math; a vanished file still pins at
            // its stored size (missing-file placeholder is M3).
            let natural = image_dims(&record.source)
                .map_or(size(px(record.w as f32), px(record.h as f32)), |(w, h)| {
                    size(px(w as f32), px(h as f32))
                });
            let display_id = display_for_name(cx, &record.output).or(fallback);
            match pin::spawn(
                cx,
                id,
                &record,
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
        log::info!("restored {} pin(s)", entries.len());
        let next_id = entries.len() as u64;
        cx.set_global(PinsGlobal {
            entries,
            store,
            events: event_tx,
            fallback,
            next_id,
        });

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
                            let mons = monitors().unwrap_or_default();
                            let mut updates = Vec::new();
                            {
                                let global = app.global_mut::<PinsGlobal>();
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
                                    if let Some((x, y, w, h)) = geom.map(|r| (r.x, r.y, r.w, r.h)) {
                                        pin.apply_geometry(x, y, w, h, cx);
                                    }
                                });
                            }
                            if dirty {
                                if let Err(err) = persist(app.global_mut::<PinsGlobal>()) {
                                    warn!("saving pins: {err:#}");
                                }
                            }
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
    for (i, record) in records.iter_mut().enumerate() {
        record.z = i as u32;
    }
    global.store.save(&records)
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
                    let _ = handle.update(app, |_, window, _| window.set_visible(true));
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
        }
    }
}

/// New pin from a file: default size = natural size capped to 50% of the
/// cursor output, centered under the cursor (output-local coords).
fn add_pin(app: &mut App, path: PathBuf) -> anyhow::Result<String> {
    if !path.is_file() {
        anyhow::bail!("no such file: {}", path.display());
    }
    let (dw, dh) = image_dims(&path)
        .ok_or_else(|| anyhow::anyhow!("not a decodable image: {}", path.display()))?;
    let (dw, dh) = (f64::from(dw), f64::from(dh));

    let mon = cursor_monitor().ok_or_else(|| anyhow::anyhow!("cannot resolve cursor output"))?;
    // Spawn default size: natural capped to 50% of the output, aspect
    // preserved; a pin whose image is larger than that spawns fit — zoom
    // < 1 exactly encodes that (spec).
    let scale = 1.0_f64
        .min(f64::from(mon.w) * 0.5 / dw)
        .min(f64::from(mon.h) * 0.5 / dh);
    let (w, h) = (dw * scale, dh * scale);
    let cursor = hypr_cursor_global().ok_or_else(|| anyhow::anyhow!("cannot read cursor"))?;
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
    };
    clamp_to_mon(&mut record, &mon);

    let (id, fallback, events) = {
        let global = app.global::<PinsGlobal>();
        (global.next_id, global.fallback, global.events.clone())
    };
    let display_id = display_for_name(app, &record.output).or(fallback);
    let output_origin = point(px(mon.x), px(mon.y));
    let natural = size(px(dw as f32), px(dh as f32));
    let handle = pin::spawn(app, id, &record, natural, output_origin, display_id, events)?;

    let global = app.global_mut::<PinsGlobal>();
    global.next_id += 1;
    global.entries.push(PinEntry {
        id,
        handle,
        record: record.clone(),
        visible: true,
    });
    persist(&global)?;
    Ok(format!(
        "pinned {} ({}x{})",
        record.source.display(),
        record.w as u32,
        record.h as u32
    ))
}

/// `clipboard` verb: pin whatever image the clipboard holds.
fn add_clipboard(app: &mut App) -> anyhow::Result<String> {
    let path = clipboard_image_path()?;
    add_pin(app, path)
}

/// Clipboard image -> `images/<blake3>.<ext>` (spec: content-addressed
/// store; wl-clipboard supplies the bytes).
fn clipboard_image_path() -> anyhow::Result<PathBuf> {
    for ty in ["image/png", "image/jpeg"] {
        let Some(bytes) = run_capture_bytes("wl-paste", &["-t", ty]) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        let ext = ty.split_once('/').unwrap().1;
        let hash = blake3::hash(&bytes).to_hex();
        let dir = crate::state::images_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{hash}.{ext}"));
        std::fs::write(&path, &bytes)?;
        return Ok(path);
    }
    anyhow::bail!("clipboard holds no image (copy one, or install wl-clipboard for wl-paste)")
}

fn run_capture_bytes(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new(program).args(args).output().ok()?;
    out.status.success().then(|| out.stdout)
}

fn image_dims(path: &Path) -> Option<(u32, u32)> {
    image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Every pin must be fully on-screen (spec): shrink oversized pins, clamp
/// the origin into the monitor.
fn clamp_to_mon(record: &mut PinRecord, mon: &Mon) {
    record.w = record.w.min(f64::from(mon.w));
    record.h = record.h.min(f64::from(mon.h));
    record.x = record.x.clamp(0.0, f64::from(mon.w) - record.w);
    record.y = record.y.clamp(0.0, f64::from(mon.h) - record.h);
}

fn accept_loop(listener: UnixListener, tx: UnboundedSender<Ipc>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { break };
        let mut text = String::new();
        if stream.read_to_string(&mut text).is_err() {
            continue;
        }
        let reply: anyhow::Result<String> = match text.trim() {
            "toggle" => forward(&tx, Ipc::Toggle),
            "show" => forward(&tx, Ipc::Show),
            "hide" => forward(&tx, Ipc::Hide),
            "stop" => forward(&tx, Ipc::Stop),
            "status" => Ok("running".into()),
            "clipboard" => ask(&tx, |resp| Ipc::Clipboard { resp }),
            other => match other.strip_prefix("add ") {
                Some(path) => {
                    let path = PathBuf::from(path.trim());
                    ask(&tx, |resp| Ipc::Add { path, resp })
                }
                None => Err(anyhow::anyhow!("unknown verb")),
            },
        };
        let line = match reply {
            Ok(payload) => format!("ok {payload}\n"),
            Err(err) => format!("err {err:#}\n"),
        };
        let _ = stream.write_all(line.as_bytes());
    }
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

// --- output/cursor resolution (Hyprland), adapted from upperadd ---

struct Mon {
    name: String,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

pub(crate) fn hypr_cursor_global() -> Option<Point<Pixels>> {
    let pos = run_capture("hyprctl", &["cursorpos"])?;
    let (x, y) = pos.trim().split_once(',')?;
    Some(point(
        px(x.trim().parse::<f32>().ok()?),
        px(y.trim().parse::<f32>().ok()?),
    ))
}

fn monitors() -> Option<Vec<Mon>> {
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

fn cursor_monitor() -> Option<Mon> {
    let cursor = hypr_cursor_global()?;
    let (cx_, cy_) = (f32::from(cursor.x), f32::from(cursor.y));
    monitors()?
        .into_iter()
        .find(|m| cx_ >= m.x && cx_ < m.x + m.w && cy_ >= m.y && cy_ < m.y + m.h)
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

fn display_for_name(cx: &App, name: &str) -> Option<DisplayId> {
    let mon = monitors()?.into_iter().find(|m| m.name == name)?;
    display_for_origin(cx, mon.x, mon.y)
}

fn primary_display(cx: &App) -> Option<Rc<dyn PlatformDisplay>> {
    cx.displays().into_iter().next()
}

fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn num(value: &serde_json::Value, key: &str) -> Option<f32> {
    value.get(key)?.as_f64().map(|v| v as f32)
}
