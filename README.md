# RightKeys

*Using the right keys, everywhere you want.*

A cross-platform key remapper for Linux (X11) and Windows. Remap keys globally
or per application, manage windows, and launch apps, all from one config file:

- **Modmaps** for global single-key remaps.
- **Multi-step actions** that emit several combos in sequence.
- **A selection** for shift-select style bindings.
- **Tap-hold** keys: tap for one key, hold for a modifier.
- **Window management**, native on each OS: move, resize, smart-tile (cycling
  ½ / ⅓ / ⅔), presets, corners, center, maximize, minimize, always-on-top,
  move to the next monitor, cycle same-app windows, and virtual-desktop
  switching.
- **App launching** that raises a running window of the app, or starts it.
- **One config for both OSes**: any step (`keys`, `exec`, ...) can carry per-OS
  targets (`windows=` / `linux=` / `macos=`), so a single binding works on each.

## Install

From crates.io (install the dependencies below first):

```sh
cargo install rightkeys
```

Dependencies (Linux only; Windows needs none):

- **Build:** GTK 3 headers, `libgtk-3-dev` (Debian/Ubuntu) or `gtk3-devel` (Fedora).
- **Run:** an Ayatana AppIndicator host (`libayatana-appindicator3-1`), for the tray icon. Most desktops provide it; GNOME needs the AppIndicator extension.

Build only:

```sh
cargo build --release      # binary at target/release/rightkeys
```

Or install system-wide with the application icon and a menu launcher. Build as
your user first (cargo is not on root's `PATH`), then install as root:

```sh
make build
sudo make install          # binary, icons, and a Start-menu (.desktop) entry
make install-config        # copy the example config to ~/.config/rightkeys/config.kdl
```

Install for the current user only (no root; `~/.local/bin` must be on `PATH`):

```sh
make build
make install PREFIX=$HOME/.local
```

Other Make targets:

- `make icons` regenerates the icon set from `assets/icons/rightkeys.svg`.
- `sudo make uninstall` removes everything.

## Run

### Linux

Linux needs access to your input devices. The quickest way is `sudo`:

```sh
rightkeys --list-devices                        # see your keyboards
sudo rightkeys --config ./config.example.kdl    # run
sudo rightkeys --config ./config.example.kdl -d # run with debug output
```

**Without `sudo`:** grant a dedicated group access to the input devices, then
run as your normal user:

```sh
# 1. Create a group and add yourself to it
sudo groupadd -f keymapper
sudo gpasswd -a $USER keymapper

# 2. Let that group read keyboards and write the virtual device
cat <<EOF | sudo tee /etc/udev/rules.d/70-keymapper.rules
KERNEL=="uinput", GROUP="keymapper", MODE="0660", OPTIONS+="static_node=uinput"
KERNEL=="event[0-9]*", GROUP="keymapper", MODE="0660"
EOF

# 3. Reboot, then run without sudo
rightkeys --config ./config.example.kdl
```

> **Default config** (when `--config` is omitted): `~/.config/rightkeys/config.kdl`
> (or `$XDG_CONFIG_HOME/rightkeys/config.kdl`).

### Windows

> **Note:** virtual-desktop actions (`workspace` / `move-to-workspace`) use the
> Windows 11 virtual-desktop COM API and require **Windows 11 ≥ 24H2**.

Run from an elevated prompt to remap inside elevated windows:

```powershell
rightkeys.exe --config %APPDATA%\rightkeys\config.kdl
```

> **Default config** (when `--config` is omitted): `%APPDATA%\rightkeys\config.kdl`.

### Flags

| Flag | Meaning |
|---|---|
| `--config <file>` | Config file to load. |
| `--device <path or name>` | Grab a specific keyboard (repeatable). Auto-detects if unset. |
| `--list-devices` | List keyboards and exit (Linux). |
| `-f`, `--force` | Replace an already-running instance (otherwise RightKeys refuses to start a second one). |
| `-d`, `--debug` | Print each key and its translation. |

### System tray

RightKeys shows a tray icon (Linux and Windows) with a menu to toggle remapping,
reload the config, or quit. If the tray can't be created, it keeps running
without it.

## Configuration

See [`config.example.kdl`](config.example.kdl) for a worked example.

### Nodes

| Node | Properties | Purpose |
|---|---|---|
| `settings` | `timeout` | Global settings (`timeout` is reserved; currently unused). |
| `modmap` | (none) | Container for global single-key remaps. |
| `map` (in `modmap`) | `from`, `to` | Remap a physical key globally (e.g. `capslock` to `left_hyper`). |
| `multipurpose-modmap` | `application`, `application-not` | Container for tap-hold keys. |
| `map` (in `multipurpose-modmap`) | `key`, `tap`, `hold` | Tap `key` to send `tap`; hold it to act as the `hold` modifier. |
| `keymap` | `name`, `application`, `application-not` | A set of `bind` bindings scoped to matching apps. |
| `bind` (in `keymap`) | argument = trigger combo; a block of step nodes | One binding (see below). |

`application` / `application-not` are regular expressions matched against the
active window's identifier.

One config file serves every platform: window management is performed natively on
each OS, and steps like `keys` and `exec` can take per-OS targets (see below).

### Bind forms

Inside a `keymap`, each `bind` takes the trigger combo as its argument and a
block of one or more step nodes (there is no `to=`/`mark-to=` shorthand, one
form only):

```kdl
bind "M-c" { keys "C-c"; }                  // emit a combo
bind "C-a" { keys "home" extend="selection"; }  // adds Shift while a selection is active
bind "C-k" { keys "S-end"; keys "C-x"; selection "clear"; }  // multiple steps, run in order
bind "C-q" { pass-through; }                // let the original key through
```

Step nodes (inside a `bind { ... }` block):

| Step | Meaning |
|---|---|
| `keys "<combo>"` | Emit a combo. Accepts per-OS targets, see below. |
| `keys "<combo>" extend="selection"` | Emit a combo, adding `Shift` while a selection is active. Accepts per-OS targets. |
| `selection "start"` | Start a selection (anchor here; subsequent `extend` keys grow it). |
| `selection "clear"` | Clear the selection. |
| `pass-through` | Emit the original trigger key unchanged. |
| `exec "<name\|path>"` | Activate an existing window of the program, or launch it. Accepts per-OS targets, see below. |
| `wm action="<...>" ...` | Act on the foreground window, native on both OSes (see below). |

The `wm` step performs a window-manager action on the foreground window:

| `action=` | Extra properties | Effect |
|---|---|---|
| `adjust` | `dx` `dy` `dw` `dh` (pixels, default `0`) | Move by `(dx, dy)` and resize by `(dw, dh)`. |
| `preset` | `w` `h` (fractions `0` to `1`); optional `anchor=` | Size to a fraction of the work area, placed at `anchor` (`top-left`/`top-right`/`bottom-left`/`bottom-right`, default centred). |
| `center` | (none) | Centre on the monitor, keeping the current size. |
| `snap` | `to=` `top-left`/`top-right`/`bottom-left`/`bottom-right` | Move to a corner, keeping the current size. |
| `corner` | `to=` `top-left`/`top-right`/`bottom-left`/`bottom-right` | Tile to a quarter of the work area. |
| `smart-tile` | `to=` `left`/`right`/`top`/`bottom` | Tile that edge, cycling ½ → ⅓ → ⅔ on consecutive tiles of the same edge (any other action resets it to a half). |
| `maximize` | (none) | Maximize. |
| `maximize-toggle` | (none) | Maximize if restored, restore if maximized. |
| `minimize` | (none) | Minimize (iconify) the window. |
| `always-on-top` | (none) | Toggle the window's always-on-top (keep-above) state. |
| `show-desktop` | (none) | Toggle showing the desktop (minimize/restore all windows). |
| `move-to-monitor` | `to=` `next`/`prev` | Move the window to the next/previous monitor, keeping its relative place. |
| `cycle-same-app` | `direction=` `forward`/`backward` (default `forward`) | Activate the next/previous window of the same application. |
| `workspace` | `to=` `prev`/`next`/`<n>` | Switch to a workspace (`<n>` is 1-based). |
| `move-to-workspace` | `to=` `prev`/`next`/`<n>` | Move the active window to a workspace and follow it. |

Virtual desktops work on both platforms, Linux via EWMH, Windows via the
virtual-desktop COM API. **On Windows this requires Windows 11 ≥ 24H2** (build
26100.2605); on older builds the `workspace`/`move-to-workspace` actions log a
warning and do nothing.

```kdl
bind "s-M-b" { exec "brave.exe"; }                   // launch or focus Brave
bind "Hyper-y" { wm action="adjust" dx=-30; }        // nudge window left
bind "Hyper-6" { wm action="preset" w=0.6 h=0.75; }  // centred 60% × 75%
bind "M-f10" { wm action="maximize-toggle"; }
```

`keys` and `exec` can each give **per-OS targets** so one binding
serves every platform, the combo is written once (keeping keymaps consistent),
and the target is chosen by the running OS. A positional string is the default;
`windows=`/`linux=`/`macos=` override it. A step that names only other OSes is
dropped, and the binding still applies as long as some step remains, so one
bind can pair a Windows-only step with a Linux-only one.

```kdl
bind "s-M-b" { exec windows="brave.exe" linux="brave-browser"; }  // per-OS launch
bind "s-M-x" { exec linux="code-insiders"; }                      // Linux only
bind "C-s-a" { keys windows="C-a" linux="C-q"; }                  // per-OS remap
bind "s-x"   { keys windows="C-esc"; exec linux="rofi.sh"; }      // Win keystroke / Linux exec
```

> **Note:** `exec` launches at this process's integrity level. If RightKeys runs
> elevated, launched apps are elevated too, run it un-elevated if that matters.

### Combos and modifiers

A combo is `Mod-Mod-...-key`, e.g. `C-M-s-S-a` (Ctrl+Alt+Super+Shift+A).
Modifier prefixes are the Emacs-style letters and are **case-sensitive** (note
`S` = Shift vs `s` = Super):

| Prefix | Modifier |
|---|---|
| `C` | Control |
| `M` | Alt |
| `S` | Shift |
| `s` | Super / Windows key |
| `Hyper` | Synthetic, tracked internally, never sent to the OS (map it to real combos) |

### Key names

Key names are descriptive and platform-neutral (mostly matching Linux evdev
names). Each key has exactly one name.

- **Letters:** `a` `b` `c` `d` `e` `f` `g` `h` `i` `j` `k` `l` `m` `n` `o` `p` `q` `r` `s` `t` `u` `v` `w` `x` `y` `z`
- **Digits:** `0`-`9`
- **Editing:** `backspace` `delete` `enter` `esc` `space` `tab`
- **Navigation:** `up` `down` `left` `right` `home` `end` `page_up` `page_down`
- **Punctuation:** `minus` `equal` `left_brace` `right_brace` `backslash` `semicolon` `apostrophe` `comma` `dot` `slash` `backtick`
- **Function:** `f1`-`f12`
- **Media / system:** `volumeup` `volumedown` `mute` `playpause` `nextsong` `previoussong` `capslock` `pause` `scrolllock` `printscreen`
- **Modifier keys:** `left_ctrl` `right_ctrl` `left_shift` `right_shift` `left_alt` `right_alt` `left_meta` `right_meta` `left_hyper` `right_hyper`

### Application matching

`application` / `application-not` are regular expressions matched against the
active window: the X11 `WM_CLASS` on Linux (find it with `xprop WM_CLASS`), or
the process name on Windows (e.g. `firefox`).

### Live reload

Config edits apply automatically, no restart needed:

- A successful reload shows a "RightKeys reloaded!" notification.
- An invalid edit is rejected and the running config kept, so a typo can't
  strand your keyboard.

## Notes

- Linux support is X11 only (no Wayland yet).
- On Windows, some games that read raw input bypass the remapper, and remapping
  inside elevated windows requires running elevated.

## Acknowledgments

This project is inspired by the awesome [xkeysnail](https://github.com/mooz/xkeysnail).

## License

MIT
