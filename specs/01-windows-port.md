# impin v0.2.0 — Windows port (spec)

**impin** gains Windows. Decided in grilling with the owner, 2026-09-28.
This is a **delta spec**: `00-v0.1.0-spec.md` remains the product truth; only
Windows deltas appear here. v0.2.0 = the **cross-platform release**
(Linux + macOS + Windows), one `main`, minor bump.

## Branch & fork lineage

- Working branch **`cross-platform`**, cut off `macos-port` (aebc112) —
  inherits `src/platform.rs` (free functions, paired cfg arms) and the fork
  `.7` lineage (`Window::set_position`, true display origins, chrome-less
  panels). Whole chain merges to `main` together as v0.2.0.
- **Fork tag `.8` (owner-authorized)**: implement `Window::set_position` on
  `gpui_windows` — one `SetWindowPos`, no staging semantics (unlike Wayland
  margin staging). Re-assert `HWND_TOPMOST` in the same call → raise-on-click
  for free. MDRV.md-documented, upstream-first.
- The audit's feared topmost patch is **not needed**: `WindowKind::PopUp` on
  Windows already gives `WS_EX_TOPMOST | WS_EX_TOOLWINDOW`, borderless, with
  `SW_SHOWNOACTIVATE` — exactly an impin pin.

## Windows mapping (platform.rs cfg arms)

- **Pin/notice windows**: `WindowKind::PopUp` (macOS arm already models this).
- **Positioning**: `window.set_position(origin + pos)` — macOS-arm pattern.
- **Cursor ground truth**: `GetCursorPos` (16 ms poll mechanics unchanged).
- **Displays**: `cx.displays()`; persistence key = `display_id` if stable
  (spike-verify), else origin-based like Linux.
- **Clipboard image**: `cx.read_from_clipboard()` (`ClipboardEntry::Image`;
  Windows backend reads PNG/GIF/JPG + `CF_DIB`) → same blake3 store. No
  subprocess.
- **IPC**: named pipe `\\.\pipe\impin`, same verb protocol. Single instance
  via named mutex (`Local\`); `CreateNamedPipeW` ACCESS_DENIED = already
  running (replaces the stale-socket probe). Client = `CreateFileW`.
- **State**: `%LOCALAPPDATA%\impin\` (`pins.toml` + `images/`), tmp+rename
  atomic writes, never hand-edited.
- **Hotkey**: **Win+Ctrl+I** default (Super+I occupied), `IMPIN_HOTKEY` env
  override, same parse as macOS arm. Impin-side thread: hidden message-only
  window, `RegisterHotKey`, `WM_HOTKEY` → toggle channel. Registration
  failure → log + continue (hotkey optional). No fork work.
- **Raise-on-click**: `set_position` topmost re-assert (vs Linux re-map).
- Stay-Linux: `hyprctl` helpers, layer-shell arms, compositor binds.

## Documented divergences (accepted by owner)

- **Per-virtual-desktop pins** — no public all-desktops API; the COM route is
  undocumented/fragile. A pin lives on the desktop where it spawned.
- **No file-manager "Open With"** — a portable zip can't register file
  associations; `impin add <file>` is the path in.
- **No drag-and-drop onto pins** — never implemented on Linux either.
- No tray icon, no taskbar presence (toolwindow), no autostart code
  (`shell:startup` shortcut = the user's `exec-once`).

## Scope & target

- **Full v0.1 feature parity** minus the divergences above: all verbs,
  gestures (move/resize/pan/zoom/opacity/radius, keys), decode pipeline
  (pure-Rust AVIF/JXL/SVG arms carry over unchanged), persistence,
  clamp-at-spawn + toggle-on, missing-file placeholders.
- **Addition on all platforms (owner-approved during W2 testing): `B`
  toggles the backdrop/border layer** (persisted per pin, `bg` in
  PinRecord). Motivated by Windows testing: the near-opaque backdrop sits
  behind the image, so group fades read as "backdrop still opaque" until
  deep in the fade — and a bare-image mode is the reference-tool use case.
- **Windows 10 1809+ x64 only** (`x86_64-pc-windows-msvc`), per-monitor
  DPI v2 (fork provides). No aarch64 (no hardware). No acrylic/mica/DWM
  effects — translucency exactly as Linux. 00's out-of-scope list carries
  verbatim.
- New dep `windows` (narrow features) in impin: pipes, hotkey, cursor —
  one-line justification in the commit.
- Windows findings folded back into the shared code (all platforms benefit):
  `is_resizable: false` on pins (the Windows hit-test installs a native
  top-resize band on titlebar-less resizable windows that hijacks our edge
  gestures); per-element opacity instead of group `.opacity()`; fork `.10`
  (DWMWA_COLOR_NONE) + `.11` (DWMNCRP_DISABLED) PopUp DWM-frame opt-out.

## Release & packaging

- Release matrix: Linux tarball jobs unchanged + `windows-latest` job →
  `impin-v0.2.0-x86_64-pc-windows-msvc.zip`. No installer, no signing.
- PKGBUILD / `.desktop` stay Linux-only.

## Milestones

- **W1 — spike**: fork `.8` (set_position on Windows, MDRV.md entry); impin
  deps build on the windows target; one PopUp pin shows an image, positions,
  click-to-focus. Gate: image pin visible + movable on the owner's box.
- **W2 — platform arms**: GetCursorPos, displays, clipboard, named-pipe IPC +
  mutex, state dir, hotkey thread. Gate: `toggle|show|hide|status` round-trip.
- **W3 — gestures + persistence**: drag/resize/zoom/pan/opacity/radius;
  restore+clamp; raise-on-click; missing-file placeholder. Gate: owner
  verifies every interaction in Krita on this box.
- **W4 — release**: windows release job; un-stale AGENTS.md (scaffold status,
  MSRV 1.93, fork tag, clipboard method); README Windows notes; v0.2.0 tag at
  chain merge. Gate: release published.

## Spike risks (verify in W1, no decisions attached)

`display_id` stability across reboots/dock cycles; PopUp click-to-focus
keyboard behavior; per-frame `set_position` drag feel (immediate `SetWindowPos`
vs staged Wayland semantics).
