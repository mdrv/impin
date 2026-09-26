# impin

Floating reference-image pins for creative work — a Wayland layer-shell
overlay that keeps any image above Krita, Blender, Inkscape, whatever.
Drag it, resize it, zoom to 800%, ghost it to 20% opacity for tracing,
round the corners — and toggle everything away with one keystroke.

PNG/JPEG/WebP/GIF/BMP and SVG via the gpui asset pipeline; AVIF and JXL
decoded in-process (first frame). Linux (Wayland) only.

## Install

### Arch Linux (mdrv repo, x86_64 + aarch64)

```ini
[mdrv]
SigLevel = Optional TrustAll
Server = https://mdrv.github.io/alarm/$arch
```

```bash
sudo pacman -Syu impin
```

### Release tarballs

Grab `impin-v<TAG>-<x86_64|aarch64>-unknown-linux-gnu.tar.gz` from
[releases](https://github.com/mdrv/impin/releases) (binary + README +
LICENSE + `impin.desktop`), unpack, and put `impin` on your `$PATH`.

## Usage

A resident daemon owns the pin windows; the `impin` CLI talks to it over
`$XDG_RUNTIME_DIR/impin.sock`.

```bash
impin daemon start --foreground   # start the daemon (exec-once / autostart)
impin add ~/ref/pose.png          # pin a file, centered under the cursor
impin clipboard                   # pin the clipboard image (or a copied file)
impin toggle                      # hide/show all pins
impin show | hide                 # force visible / hidden
impin status                      # is the daemon running?
impin stop                        # exit (state saved)
```

Bare `impin` prints help. Pins persist across restarts in
`~/.local/state/impin/pins.toml`; clipboard images are stored
content-addressed under `~/.local/state/impin/images/`.

## Keys

Click a pin to focus it (pins only take the keyboard while focused).

| Input                    | Action                                     |
| ------------------------ | ------------------------------------------ |
| Left-drag                | move                                       |
| Edge / corner drag       | resize (6 px edges, 12 px corners)         |
| Ctrl-drag or middle-drag | pan (clamped gapless when zoomed in)       |
| Ctrl+wheel               | zoom at cursor (0.1×–8×; 1× = 100% raster) |
| `-` / `=`                | zoom out / in                              |
| `1` / `0` / double-click | 100% / fit / fit                           |
| `[` / `]`                | opacity 0.2–1.0 (trace over your canvas)   |
| `,` / `.`                | corner radius down / up (max = circle)     |
| Click                    | raise above sibling pins                   |
| `Q` / `Delete`           | delete pin                                 |

Every critical op is mouse-reachable; nothing steals your keyboard unless
you click a pin.

## Hyprland

```ini
# autostart
exec-once = impin daemon start --foreground

# binds
bind = $mainMod, I, exec, impin toggle
bind = $mainMod SHIFT, I, exec, impin clipboard

# optional compositor-side blur on the pin surfaces
layerrule = blur, impin
```

## File manager

```bash
cp dist/impin.desktop ~/.local/share/applications/
```

enables "Open With → impin" for images (pins the file; daemon must be
running).

## Building

```bash
cargo build --release
```

Rust 1.93+ (edition 2024). The gpui fork is consumed from
`github.com/mdrv/gpui-ce` (tag `mdrv-gpui-0.0.260925.5`).

## License

MIT
