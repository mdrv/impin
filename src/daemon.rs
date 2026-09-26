//! impin daemon: owns the pin windows and the CLI socket
//! (`$XDG_RUNTIME_DIR/impin.sock`). Single writer for the state file.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{App, AsyncApp, DisplayId, Pixels, PlatformDisplay, Point, WindowHandle, point, px};
use gpui_platform::application;
use log::warn;

use crate::cli::socket_path;
use crate::pin;
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
}

struct PinEntry {
    handle: WindowHandle<pin::Pin>,
    record: PinRecord,
    visible: bool,
}

struct PinsGlobal {
    entries: Vec<PinEntry>,
    store: Store,
    fallback: Option<DisplayId>,
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
        let mut entries = Vec::new();
        for record in &records {
            let display_id = display_for_name(cx, &record.output).or(fallback);
            match pin::spawn(cx, record, display_id) {
                Ok(handle) => entries.push(PinEntry {
                    handle,
                    record: record.clone(),
                    visible: true,
                }),
                Err(err) => warn!("spawning pin {}: {err:#}", record.source.display()),
            }
        }
        log::info!("restored {} pin(s)", entries.len());
        cx.set_global(PinsGlobal {
            entries,
            store,
            fallback,
        });

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
                            let mut updates = Vec::new();
                            {
                                let global = app.global_mut::<PinsGlobal>();
                                for entry in &mut global.entries {
                                    let target = if toggle { !entry.visible } else { forced };
                                    if target != entry.visible {
                                        entry.visible = target;
                                        updates.push((entry.handle, target));
                                    }
                                }
                            }
                            for (handle, target) in updates {
                                let _ =
                                    handle.update(app, |_, window, _| window.set_visible(target));
                            }
                        });
                    }
                    Ipc::Add { path, resp } => {
                        let _ = resp.send(cx.update(|app| add_pin(app, path)));
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
    let pins = app.global_mut::<PinsGlobal>();
    let records: Vec<PinRecord> = pins.entries.iter().map(|e| e.record.clone()).collect();
    pins.store.save(&records)
}

/// New pin from a file: default size = natural size capped to 50% of the
/// cursor output, centered under the cursor (output-local coords).
fn add_pin(app: &mut App, path: PathBuf) -> anyhow::Result<String> {
    if !path.is_file() {
        anyhow::bail!("no such file: {}", path.display());
    }
    let (dw, dh) = image::ImageReader::open(&path)?
        .with_guessed_format()?
        .into_dimensions()
        .map_err(|e| anyhow::anyhow!("reading image dims: {e}"))?;
    let (dw, dh) = (f64::from(dw), f64::from(dh));

    let mon = cursor_monitor().ok_or_else(|| anyhow::anyhow!("cannot resolve cursor output"))?;
    let scale = 1.0_f64
        .min(f64::from(mon.w) * 0.5 / dw)
        .min(f64::from(mon.h) * 0.5 / dh);
    let (w, h) = (dw * scale, dh * scale);
    let cursor = hypr_cursor_global().ok_or_else(|| anyhow::anyhow!("cannot read cursor"))?;
    let x = f64::from((f32::from(cursor.x) - mon.x).max(0.0)) - w / 2.0;
    let y = f64::from((f32::from(cursor.y) - mon.y).max(0.0)) - h / 2.0;

    let z = app.global::<PinsGlobal>().entries.len() as u32;
    let record = PinRecord {
        source: path.clone(),
        output: mon.name.clone(),
        x,
        y,
        w,
        h,
        z,
    };
    let fallback = app.global::<PinsGlobal>().fallback;
    let display_id = display_for_name(app, &mon.name).or(fallback);
    let handle = pin::spawn(app, &record, display_id)?;

    let global = app.global_mut::<PinsGlobal>();
    global.entries.push(PinEntry {
        handle,
        record: record.clone(),
        visible: true,
    });
    let records: Vec<PinRecord> = global.entries.iter().map(|e| e.record.clone()).collect();
    global.store.save(&records)?;
    Ok(format!(
        "pinned {} ({}x{})",
        path.display(),
        w as u32,
        h as u32
    ))
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
            "clipboard" => Err(anyhow::anyhow!("clipboard pinning lands in M2")),
            other => match other.strip_prefix("add ") {
                Some(path) => {
                    let (resp_tx, resp_rx) = futures::channel::oneshot::channel();
                    match tx.unbounded_send(Ipc::Add {
                        path: PathBuf::from(path.trim()),
                        resp: resp_tx,
                    }) {
                        Ok(()) => match futures::executor::block_on(resp_rx) {
                            Ok(result) => result,
                            Err(_) => Err(anyhow::anyhow!("daemon shutting down")),
                        },
                        Err(_) => Err(anyhow::anyhow!("daemon shutting down")),
                    }
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

// --- output/cursor resolution (Hyprland), adapted from upperadd ---

struct Mon {
    name: String,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

fn hypr_cursor_global() -> Option<Point<Pixels>> {
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
