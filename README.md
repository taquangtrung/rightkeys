# RightKeys

*Using the right keys, everywhere you want.*

A cross-platform key remapper for Linux (X11) and Windows. Configure keymaps,
window management, and app launching in one KDL file that works on both OSes.

**Features:** modmaps, tap-hold keys, multi-step bindings, selection-aware
remaps, window management (move, resize, tile, snap, maximize, workspaces,
monitors), app launching, and a hint-based element/window picker.

## Install

```sh
cargo install rightkeys
```

Linux build dependencies: `libgtk-3-dev` (Debian/Ubuntu) or `gtk3-devel`
(Fedora). Runtime: an Ayatana AppIndicator host for the tray icon.

Or build from source:

```sh
make build
sudo make install        # binary + icons + .desktop entry
make install-config      # copy example config to ~/.config/rightkeys/settings.kdl
```

## Run

### Linux

```sh
sudo rightkeys                    # uses ~/.config/rightkeys/settings.kdl
sudo rightkeys --config my.kdl -d # custom config, debug output
```

For passwordless use, grant a group access to input devices:

```sh
sudo groupadd -f keymapper && sudo gpasswd -a $USER keymapper
cat <<EOF | sudo tee /etc/udev/rules.d/70-keymapper.rules
KERNEL=="uinput", GROUP="keymapper", MODE="0660", OPTIONS+="static_node=uinput"
KERNEL=="event[0-9]*", GROUP="keymapper", MODE="0660"
EOF
# reboot, then run without sudo
```

### Windows

```powershell
rightkeys.exe    # uses %APPDATA%\rightkeys\settings.kdl
```

Virtual-desktop actions require Windows 11 ≥ 24H2.

### Flags

| Flag | Meaning |
|---|---|
| `--config <file>` | Config file to load |
| `--device <path>` | Grab a specific keyboard (repeatable; auto-detects if unset) |
| `--list-devices` | List keyboards and exit (Linux) |
| `-f`, `--force` | Replace an already-running instance |
| `-d`, `--debug` | Print each key and its translation |

The tray icon (Linux and Windows) lets you toggle remapping, reload config, or quit.

## Configuration

See [`config.example.kdl`](config.example.kdl) for a full example. Config edits
reload automatically; an invalid file is rejected and the previous config kept.

### Top-level nodes

| Node | Purpose |
|---|---|
| `modmap` | Global single-key remaps (`map from="capslock" to="left_hyper"`) |
| `multipurpose-modmap` | Tap-hold keys (`map key="left_alt" tap="esc" hold="left_alt"`) |
| `keymap` | A set of bindings, optionally scoped to apps via `application=` / `application-not=` (regex) |
| `settings` | Global settings (`timeout` reserved for future tap-hold timing) |

### Bindings

Each `bind` in a `keymap` takes a trigger combo and a block of step nodes:

```kdl
bind "M+c"  { keys "C+c"; }
bind "C+a"  { keys "home" extend="selection"; }   // adds Shift while a selection is active
bind "C+k"  { keys "S+end"; keys "C+x"; selection "clear"; }
bind "C+q"  { pass-through; }
```

Step nodes:

| Step | Meaning |
|---|---|
| `keys "<combo>"` | Emit a combo |
| `keys "<combo>" extend="selection"` | Emit a combo, adding Shift while a selection is active |
| `selection "start"` / `"clear"` | Start or clear a selection anchor |
| `pass-through` | Emit the original key unchanged |
| `exec "<program>"` | Raise an existing window of the app, or launch it |
| `wm action="<...>"` | Window-manager action (see below) |

`keys` and `exec` accept per-OS targets; a positional string is the default:

```kdl
bind "s+M+b" { exec windows="brave.exe" linux="brave-browser"; }
bind "s+x"   { keys windows="C+esc"; exec linux="rofi.sh"; }
```

`exec` is split into a command and arguments shell-style, so an argument that
contains spaces must be quoted. KDL has no single-quoted strings (wrapping the
whole value in `'...'` silently mis-parses), so the value itself is always a
KDL double-quoted or raw string. Quote the spaced argument *inside* it, either
with single quotes in a normal string, or double quotes in a raw `#"..."#`
string:

```kdl
bind "s+g" { exec linux="raise-or-run.sh -w 'Brave-browser:Google Scholar' -c run.sh"; }
bind "s+g" { exec linux=#"raise-or-run.sh -w "Brave-browser:Google Scholar" -c run.sh"#; }
```

### Window management (`wm`)

| `action=` | Extra properties | Effect |
|---|---|---|
| `adjust` | `dx` `dy` `dw` `dh` (pixels) | Move/resize by delta |
| `preset` | `w` `h` (0–1 fractions); `anchor=` | Size to fraction of work area, optionally anchored to a corner |
| `center` | | Centre on monitor, keeping size |
| `snap` | `to=` corner | Move to corner, keeping size |
| `corner` | `to=` corner | Tile to a quarter of the work area |
| `smart-tile` | `to=` `left`/`right`/`top`/`bottom` | Tile to half, cycling ½→⅓→⅔ on repeats |
| `maximize` / `maximize-toggle` / `minimize` | | |
| `always-on-top` / `show-desktop` | | |
| `move-to-monitor` | `to=` `next`/`prev` | Move window to adjacent monitor |
| `cycle-same-app` | `direction=` `forward`/`backward` | Cycle windows of the same app |
| `workspace` | `to=` `prev`/`next`/`<n>` | Switch workspace |
| `move-to-workspace` | `to=` `prev`/`next`/`<n>` | Move window to workspace |

Corners: `top-left` `top-right` `bottom-left` `bottom-right`.

### Modifiers and key names

Combo syntax: `Mod+Mod+key`, e.g. `C+M+s+a`. Prefixes are case-sensitive:

| Prefix | Modifier |
|---|---|
| `C` | Control |
| `M` | Alt |
| `S` | Shift |
| `s` | Super / Win |
| `Hyper` | Synthetic (tracked internally, never sent to the OS) |

Key names: letters `a`-`z`, digits `0`-`9`, punctuation by unshifted glyph
(`-` `=` `[` `]` `\` `;` `'` `,` `.` `/` `` ` ``; write backslash as `"\\"`),
editing (`backspace` `delete` `enter` `esc` `space` `tab`), navigation
(`up` `down` `left` `right` `home` `end` `page_up` `page_down`),
function keys `f1`-`f12`, media (`volumeup` `volumedown` `mute` `playpause`
`nextsong` `previoussong`), and modifier keys (`left_ctrl` `right_ctrl`
`left_shift` `right_shift` `left_alt` `right_alt` `left_meta` `right_meta`
`left_hyper` `right_hyper`).

## Notes

- Linux: X11 only (no Wayland).
- Windows: raw-input games bypass the remapper; remapping inside elevated
  windows requires running elevated.

## Acknowledgments

Inspired by [xkeysnail](https://github.com/mooz/xkeysnail) and [Vimium](https://github.com/philc/vimium).

## License

MIT
