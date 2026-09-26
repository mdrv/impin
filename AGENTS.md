# impin v0.1.0 — agent guide

Layer-shell reference-image pins: floating, zoomable, translucent image
windows for creative work (Krita/Blender). Spec:
[specs/00-v0.1.0-spec.md](specs/00-v0.1.0-spec.md). Sister project:
`/x/g/upperadd` (the proven daemon+socket+sticky pattern — read its
`src/sticky.rs` before reimplementing any gesture).

## Status

Scaffold. Single crate `impin` (bin), edition 2024, MSRV 1.85,
Linux/Wayland only in v0.1 (Windows deliberately deferred).

## Toolchain & dependencies

```toml
[dependencies]
gpui = { package = "mdrv-gpui-ce", git = "https://github.com/mdrv/gpui-ce", tag = "mdrv-gpui-0.0.260925.4" }
gpui_platform = { package = "mdrv-gpui-platform", git = "https://github.com/mdrv/gpui-ce", tag = "mdrv-gpui-0.0.260925.4", features = [
	"wayland",
] }

[patch.crates-io]
arrayref = { git = "https://github.com/mdrv/gpui-ce" }
```

- **Pin `image` at 0.25.10**: git-dep consumers resolve their own lockfile,
  and newer semver-compatible `image` releases break the fork
  (`into_raw_bgra` gone). After first resolve:
  `cargo update -p image --precise 0.25.10`. (Fork should switch to an
  `=` req — flagged.)

- Docs BEFORE writing gpui code: `/x/m/v270/gpui-ce/00-overview.md`, then
  `10-practical-api.md` (§6 layer-shell), `20-dragging-panels.md`,
  `30-resizing-panels.md`, `40-animation-freeze.md` (§28: timed show/hide,
  never opacity fades), `/g/gpui-ce/MDRV.md` (fork policy).
- New deps need a one-line justification in the commit body. Expected:
  clap(derive), serde+toml (state), anyhow, log+env_logger, futures,
  blake3 (clipboard-image hashing), image (dimensions/decoding),
  jxl-oxide + avif-decode (JXL/AVIF → RenderImage).

## Fork rules (inherited the hard way — see upperadd docs/01)

1. Consume the fork via git tag; NEVER modify `/g/gpui-ce` without owner
   authorization (patches are small, documented in MDRV.md, new
   `mdrv-gpui-0.0.<ts>` tag per change).
2. `set_margin` STAGES ONLY (tag ≥ .2) — stage geometry in `render()`, the
   present commit carries margin+size+buffer atomically. Never commit
   mid-gesture.
3. Drag/resize = poll-based ground truth (`hyprctl cursorpos` every 16 ms,
   offset = −press_local), never event accumulation (wiggle + fling loss).
4. `Anchor::empty()` misbehaves — use `Anchor::TOP | Anchor::LEFT` +
   margin-as-position.
5. All-four-anchors windows need 0×0 bounds (§16.3); pins are TOP|LEFT with
   explicit Windowed bounds.
6. `AsyncApp::update` re-enters/deadlocks mid-update — surface mutations
   from handlers go through `cx.defer`; resize callbacks must stay spawned.
7. Animations: geometry only; opacity changes are instant (no fades).

## impin-specific rules

- Never hand-edit `~/.local/state/impin/` — writes go through the daemon,
  atomically (tmp+rename).
- Clipboard reads: `cx.read_from_clipboard()` returns
  `ClipboardEntry::Image` on Wayland today (text-only writes — don't try).
- Every pin fully on-screen: clamp at spawn and at toggle-on.
- CLI verbs over `$XDG_RUNTIME_DIR/impin.sock`: `toggle | show | hide |
  clipboard | add <file> | stop | status`; bare `impin` prints help.
- Stale-socket probe on daemon start (upperadd pattern); single writer.

## Verification (green before every commit)

    cargo check && cargo test
    cargo build --release
    # manual round-trip (interactive):
    impin toggle        # pins appear
    impin clipboard     # with an image on the clipboard → new pin
    impin stop          # clean exit; restart restores geometry

## House style

- Conventional commits (`feat:`, `fix:`, `docs:`, `chore:`), small and
  frequent; commit only with checks green.
- `specs/` = numbered design docs; `docs/` = findings/gotchas — write one
  the first time a tool lies to you.
- Minimal diffs; no speculative abstraction.
- The owner tests interactive gestures himself (synthetic input is
  unreliable); build, restart, and hand over.

## Out of scope for v0.1.0 (do not build)

Windows, video/frame-pause, live fs-refresh, per-pin hide, config file,
mouse-only delete, animated AVIF/JXL, crop/annotate, sync, systemd unit.
