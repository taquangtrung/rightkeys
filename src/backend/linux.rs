//! Linux backend: grabs keyboards via `evdev`, injects through a `uinput`
//! virtual device, and reads the active window's `WM_CLASS` over X11.

// Imports

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, Device, EventSummary, EventType, InputEvent, KeyCode};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt, EventMask, Window,
};
use x11rb::rust_connection::RustConnection;
use x11rb::CURRENT_TIME;

use super::{Options, WindowWatcher};
use crate::engine::{
    Corner, CycleDirection, Effect, Engine, OutEvent, Side, WindowAction, Workspace,
};
use crate::key::Key;

// Constants

/// Name of the injected virtual keyboard.
const VIRTUAL_DEVICE_NAME: &str = "RightKeys virtual keyboard";

/// Highest evdev key code the virtual device advertises, so unmapped keys can
/// still be forwarded verbatim.
const MAX_KEY_CODE: u16 = 255;

/// Maximum parent-walk depth when resolving a window's `WM_CLASS`.
const WM_CLASS_MAX_DEPTH: u8 = 8;

/// How long a tap-hold key may be held with no other key before it commits to
/// its hold modifier. This lets the modifier reach the OS in time for a mouse
/// click (which never reaches the engine), e.g. Shift/Ctrl-click multi-select.
const TAP_HOLD_TIMEOUT: Duration = Duration::from_millis(200);

/// How long the last geometry we commanded for a window stays authoritative for
/// follow-up nudges. The window manager applies `_NET_MOVERESIZE_WINDOW`
/// asynchronously, so a server read taken between rapid repeats returns the
/// pre-move position; accumulating on our own target over this window keeps a
/// held move/resize key gliding smoothly instead of stuttering. Comfortably
/// covers a key's auto-repeat interval while self-healing between gestures.
const MOVE_CACHE_TTL: Duration = Duration::from_millis(250);

/// A window transition whose largest dimension change is at least this many
/// pixels is animated; smaller ones (a held nudge) apply instantly so rapid
/// auto-repeats stay responsive instead of queuing behind animation frames.
const ANIM_MIN_DELTA: i32 = 120;

/// Number of frames in an animated window transition.
const ANIM_STEPS: u32 = 16;

/// Pause between animation frames; `ANIM_STEPS` times this is the total
/// transition duration (~128 ms).
const ANIM_FRAME_PAUSE: Duration = Duration::from_millis(8);

// Data Structures

/// A key event read from a grabbed device.
struct InEvent {
    code: u16,
    value: i32,
}

/// The geometry last commanded for a window and when, so rapid follow-up nudges
/// accumulate on the intended target instead of a stale server read.
struct MoveCache {
    at: Instant,
    geom: (i32, i32, i32, i32),
    win: Window,
}

/// X11-based active-window watcher with a one-entry focus cache. Also performs
/// window-management [`Effect`]s via EWMH messages to the root window.
struct X11Watcher {
    conn: RustConnection,
    root: Window,
    cached_focus: Window,
    cached_class: String,
    last_move: Option<MoveCache>,
    /// Geometry to restore when a maximize-toggle un-fills the window we last
    /// filled; `None` once restored, or for a window we never filled.
    restore_geom: Option<(Window, (i32, i32, i32, i32))>,
}

// === X11Watcher ===

impl X11Watcher {
    fn connect() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("connecting to X11 display")?;
        let root = conn.setup().roots[screen].root;
        Ok(X11Watcher {
            conn,
            root,
            cached_focus: 0,
            cached_class: String::new(),
            last_move: None,
            restore_geom: None,
        })
    }

    /// Read a window's `WM_CLASS` (the class half of the pair), walking up to
    /// parents when the focused child lacks the property.
    fn wm_class(&self, mut window: Window) -> Option<String> {
        for _ in 0..WM_CLASS_MAX_DEPTH {
            if window == 0 {
                return None;
            }
            let reply = self
                .conn
                .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 1024)
                .ok()?
                .reply()
                .ok()?;
            if !reply.value.is_empty() {
                return Some(parse_wm_class(&reply.value));
            }
            window = self.conn.query_tree(window).ok()?.reply().ok()?.parent;
        }
        None
    }
}

impl WindowWatcher for X11Watcher {
    fn active_app(&mut self) -> String {
        let focus = match self.conn.get_input_focus().map(|cookie| cookie.reply()) {
            Ok(Ok(reply)) => reply.focus,
            _ => return self.cached_class.clone(),
        };
        if focus != self.cached_focus {
            self.cached_focus = focus;
            self.cached_class = self.wm_class(focus).unwrap_or_default();
        }
        self.cached_class.clone()
    }
}

// === X11Watcher: effects ===
//
// Window management is done the EWMH way: client messages to the root window
// (`_NET_*`) that the window manager acts on. Geometry uses `_NET_WORKAREA`
// (the panel-excluded area of the current desktop) and `_NET_MOVERESIZE_WINDOW`.

impl X11Watcher {
    /// Perform one engine [`Effect`].
    fn perform_effect(&mut self, effect: &Effect) -> Result<()> {
        match effect {
            Effect::Launch(program) => self.activate_or_launch(program),
            Effect::Window(action) => self.perform_window(*action),
        }
    }

    fn perform_window(&mut self, action: WindowAction) -> Result<()> {
        // Workspace switching needs no active window.
        if let WindowAction::Workspace {
            target,
            move_window,
        } = action
        {
            let index = self.resolve_workspace(target)?;
            if move_window {
                if let Some(win) = self.active_window()? {
                    self.move_window_to_desktop(win, index)?;
                    self.switch_desktop(index)?;
                    self.activate_window(win)?;
                }
            } else {
                self.switch_desktop(index)?;
            }
            return Ok(());
        }

        // Show-desktop is window-independent (and the "restore" toggle fires when
        // nothing is focused), so handle it before requiring an active window.
        if let WindowAction::ShowDesktop = action {
            self.toggle_show_desktop()?;
            return Ok(());
        }

        let Some(win) = self.active_window()? else {
            return Ok(());
        };
        match action {
            WindowAction::Adjust { dx, dy, dw, dh } => {
                let (x, y, w, h) = self.current_geometry(win)?;
                self.move_resize(win, x + dx, y + dy, w + dw, h + dh)?;
            }
            WindowAction::Preset { w, h, anchor } => {
                let (ax, ay, aw, ah) = self.work_area()?;
                let nw = (aw as f64 * w) as i32;
                let nh = (ah as f64 * h) as i32;
                let (x, y) = anchor_pos(ax, ay, aw, ah, nw, nh, anchor);
                self.unmaximize(win)?; // un-maximize first
                self.move_resize(win, x, y, nw, nh)?;
            }
            WindowAction::Center => {
                let (ax, ay, aw, ah) = self.work_area()?;
                let (_, _, w, h) = self.current_geometry(win)?;
                let (x, y) = anchor_pos(ax, ay, aw, ah, w, h, None);
                self.move_resize(win, x, y, w, h)?;
            }
            WindowAction::Snap(corner) => {
                let (ax, ay, aw, ah) = self.work_area()?;
                let (_, _, w, h) = self.current_geometry(win)?;
                let (x, y) = anchor_pos(ax, ay, aw, ah, w, h, Some(corner));
                self.unmaximize(win)?;
                self.move_resize(win, x, y, w, h)?;
            }
            WindowAction::Corner(corner) => {
                let (ax, ay, aw, ah) = self.work_area()?;
                let (w, h) = (aw / 2, ah / 2);
                let x = match corner {
                    Corner::TopLeft | Corner::BottomLeft => ax,
                    Corner::TopRight | Corner::BottomRight => ax + aw - w,
                };
                let y = match corner {
                    Corner::TopLeft | Corner::TopRight => ay,
                    Corner::BottomLeft | Corner::BottomRight => ay + ah - h,
                };
                self.unmaximize(win)?;
                self.move_resize(win, x, y, w, h)?;
            }
            WindowAction::SmartTile { side, fraction } => {
                let (ax, ay, aw, ah) = self.work_area()?;
                self.unmaximize(win)?;
                let tw = (aw as f64 * fraction) as i32;
                let th = (ah as f64 * fraction) as i32;
                let (x, y, w, h) = match side {
                    Side::Left => (ax, ay, tw, ah),
                    Side::Right => (ax + aw - tw, ay, tw, ah),
                    Side::Top => (ax, ay, aw, th),
                    Side::Bottom => (ax, ay + ah - th, aw, th),
                };
                self.move_resize(win, x, y, w, h)?;
            }
            WindowAction::Maximize => {
                let (ax, ay, aw, ah) = self.work_area()?;
                self.unmaximize(win)?; // drop any WM-maximized state so the move sticks
                self.move_resize(win, ax, ay, aw, ah)?; // large delta: glides to fill
            }
            WindowAction::MaximizeToggle => {
                self.unmaximize(win)?;
                match self.restore_geom.take() {
                    // Filled by us before: glide back to the saved geometry.
                    Some((w, (rx, ry, rw, rh))) if w == win => {
                        self.move_resize(win, rx, ry, rw, rh)?;
                    }
                    // Otherwise remember the current geometry and glide to fill.
                    _ => {
                        let (ax, ay, aw, ah) = self.work_area()?;
                        self.restore_geom = Some((win, self.current_geometry(win)?));
                        self.move_resize(win, ax, ay, aw, ah)?;
                    }
                }
            }
            WindowAction::Minimize => self.minimize(win)?,
            WindowAction::AlwaysOnTop => self.toggle_above(win)?,
            WindowAction::MoveToMonitor(direction) => self.move_to_monitor(win, direction)?,
            WindowAction::CycleSameApp(direction) => self.cycle_same_app(win, direction)?,
            WindowAction::Workspace { .. } | WindowAction::ShowDesktop => {
                unreachable!("handled above")
            }
        }
        Ok(())
    }

    /// Activate an existing window whose `WM_CLASS` matches `program`, else launch it.
    fn activate_or_launch(&self, program: &str) -> Result<()> {
        let stem = program.rsplit('/').next().unwrap_or(program).to_lowercase();
        if let Ok(list) = self.client_list() {
            for win in list {
                if let Some(class) = self.wm_class(win) {
                    if class.to_lowercase().contains(&stem) {
                        return self.activate_window(win);
                    }
                }
            }
        }
        // No window found: launch the program (first whitespace-separated token
        // is the binary, the rest are arguments).
        let mut parts = program.split_whitespace();
        let Some(bin) = parts.next() else {
            return Ok(());
        };
        std::process::Command::new(bin)
            .args(parts)
            .spawn()
            .with_context(|| format!("launching {program:?}"))?;
        Ok(())
    }

    fn cycle_same_app(&self, win: Window, direction: CycleDirection) -> Result<()> {
        let class = self.wm_class(win).unwrap_or_default();
        let same: Vec<Window> = self
            .client_list()?
            .into_iter()
            .filter(|&w| self.wm_class(w).as_deref() == Some(class.as_str()))
            .collect();
        if same.len() > 1 {
            let next = match same.iter().position(|&w| w == win) {
                Some(i) => same[direction.step(i, same.len())],
                None => same[same.len() - 1],
            };
            self.activate_window(next)?;
        }
        Ok(())
    }

    // ── EWMH primitives ──

    fn atom(&self, name: &[u8]) -> Result<u32> {
        Ok(self.conn.intern_atom(false, name)?.reply()?.atom)
    }

    /// Read the 32-bit values of a root-window property.
    fn root_cardinals(&self, name: &[u8], type_: AtomEnum, len: u32) -> Result<Vec<u32>> {
        let atom = self.atom(name)?;
        let reply = self
            .conn
            .get_property(false, self.root, atom, type_, 0, len)?
            .reply()?;
        Ok(reply.value32().map(|it| it.collect()).unwrap_or_default())
    }

    fn active_window(&self) -> Result<Option<Window>> {
        Ok(self
            .root_cardinals(b"_NET_ACTIVE_WINDOW", AtomEnum::WINDOW, 1)?
            .into_iter()
            .next()
            .filter(|&w| w != 0))
    }

    fn client_list(&self) -> Result<Vec<Window>> {
        self.root_cardinals(b"_NET_CLIENT_LIST", AtomEnum::WINDOW, 4096)
    }

    /// The active desktop's work area (`x, y, w, h`), excluding panels/struts.
    fn work_area(&self) -> Result<(i32, i32, i32, i32)> {
        let desktop = self.current_desktop().unwrap_or(0) as usize;
        let vals = self.root_cardinals(b"_NET_WORKAREA", AtomEnum::CARDINAL, 4 * 64)?;
        let base = desktop * 4;
        let slice = if vals.len() >= base + 4 {
            &vals[base..base + 4]
        } else if vals.len() >= 4 {
            &vals[0..4]
        } else {
            return Err(anyhow!("_NET_WORKAREA is unavailable"));
        };
        Ok((
            slice[0] as i32,
            slice[1] as i32,
            slice[2] as i32,
            slice[3] as i32,
        ))
    }

    /// A window's client geometry (`x, y, w, h`): the inner rectangle the X
    /// server reports, before the window manager's title bar and borders are
    /// added. Use [`Self::frame_geometry`] wherever the visible outer rectangle
    /// matters (snapping, animating, repeating against the work area).
    fn client_geometry(&self, win: Window) -> Result<(i32, i32, i32, i32)> {
        let geo = self.conn.get_geometry(win)?.reply()?;
        let abs = self
            .conn
            .translate_coordinates(win, self.root, 0, 0)?
            .reply()?;
        Ok((
            abs.dst_x as i32,
            abs.dst_y as i32,
            geo.width as i32,
            geo.height as i32,
        ))
    }

    /// A window's outer (frame) geometry (`x, y, w, h`): client geometry grown
    /// by the window manager's `_NET_FRAME_EXTENTS`. WMs that do not set that
    /// property — and undecorated windows — read back all-zero extents, so the
    /// frame equals the client and the same code path handles both cases.
    fn frame_geometry(&self, win: Window) -> Result<(i32, i32, i32, i32)> {
        let client = self.client_geometry(win)?;
        let extents = self.frame_extents(win)?;
        Ok(frame_geom(client, extents))
    }

    /// The window manager's frame extents for `win` as
    /// `(left, right, top, bottom)`, or `(0, 0, 0, 0)` when the property is
    /// missing — the case for undecorated windows and WMs that do not implement
    /// `_NET_FRAME_EXTENTS`.
    fn frame_extents(&self, win: Window) -> Result<(i32, i32, i32, i32)> {
        let atom = self.atom(b"_NET_FRAME_EXTENTS")?;
        let reply = self
            .conn
            .get_property(false, win, atom, AtomEnum::CARDINAL, 0, 4)?
            .reply()?;
        let vals: Vec<u32> = reply.value32().map(|it| it.collect()).unwrap_or_default();
        Ok(match vals.as_slice() {
            [left, right, top, bottom] => {
                (*left as i32, *right as i32, *top as i32, *bottom as i32)
            }
            _ => (0, 0, 0, 0),
        })
    }

    /// A window's outer (frame) geometry, preferring the value we last
    /// commanded while it is still fresh. The window manager applies moves
    /// asynchronously, so a server read taken between rapid nudges lags behind;
    /// accumulating on our own target keeps held-key movement smooth.
    fn current_geometry(&mut self, win: Window) -> Result<(i32, i32, i32, i32)> {
        if let Some(cache) = &self.last_move {
            if cache.win == win && cache.at.elapsed() < MOVE_CACHE_TTL {
                return Ok(cache.geom);
            }
        }
        self.frame_geometry(win)
    }

    /// Move/resize a window so its outer (frame) rectangle lands at
    /// `(x, y, w, h)`. A large jump (snap, preset, center, ...) glides there
    /// along an ease-out curve; a small one (a held move/resize nudge) lands
    /// instantly so rapid repeats stay responsive.
    ///
    /// The target is in frame geometry so it lines up with the work area and
    /// with [`Self::current_geometry`]; `_NET_MOVERESIZE_WINDOW` is then fed the
    /// frame position alongside the client size it expects.
    fn move_resize(&mut self, win: Window, x: i32, y: i32, w: i32, h: i32) -> Result<()> {
        let target = (x, y, w.max(1), h.max(1));
        let start = self.current_geometry(win)?;
        let extents = self.frame_extents(win)?;
        // Intern the atom once and reuse it across every frame of the animation.
        let atom = self.atom(b"_NET_MOVERESIZE_WINDOW")?;
        if max_delta(start, target) >= ANIM_MIN_DELTA {
            for frame in anim_frames(start, target, ANIM_STEPS) {
                self.send_move_resize(win, atom, frame, extents)?;
                thread::sleep(ANIM_FRAME_PAUSE);
            }
        } else {
            self.send_move_resize(win, atom, target, extents)?;
        }
        self.last_move = Some(MoveCache {
            at: Instant::now(),
            geom: target,
            win,
        });
        Ok(())
    }

    /// Send a single `_NET_MOVERESIZE_WINDOW` request for an already-interned
    /// atom; no animation, no cache update. `geom` is a target frame rectangle,
    /// translated to the message's frame-position + client-size layout via
    /// `extents`.
    fn send_move_resize(
        &self,
        win: Window,
        atom: u32,
        geom: (i32, i32, i32, i32),
        extents: (i32, i32, i32, i32),
    ) -> Result<()> {
        let (x, y, w, h) = moveresize_message(geom, extents);
        // Gravity NorthWest (1) + flags for x/y/w/h present + source = pager.
        let flags = 1u32 | (1 << 8) | (1 << 9) | (1 << 10) | (1 << 11) | (1 << 13);
        self.send_root_message(
            win,
            atom,
            [flags, x as u32, y as u32, w.max(1) as u32, h.max(1) as u32],
        )
    }

    /// Remove any window-manager maximized state so a manual move/resize is
    /// honored. Clears the move cache because the restore changes geometry
    /// outside `move_resize`.
    fn unmaximize(&mut self, win: Window) -> Result<()> {
        self.last_move = None;
        let state = self.atom(b"_NET_WM_STATE")?;
        let vert = self.atom(b"_NET_WM_STATE_MAXIMIZED_VERT")?;
        let horz = self.atom(b"_NET_WM_STATE_MAXIMIZED_HORZ")?;
        // 0 = remove the maximized state.
        self.send_root_message(win, state, [0, vert, horz, 1, 0])
    }

    /// Iconify (minimize) the window via the ICCCM `WM_CHANGE_STATE` message.
    fn minimize(&self, win: Window) -> Result<()> {
        let atom = self.atom(b"WM_CHANGE_STATE")?;
        // IconicState = 3.
        self.send_root_message(win, atom, [3, 0, 0, 0, 0])
    }

    /// Toggle showing the desktop (`_NET_SHOWING_DESKTOP`): read the current
    /// root state and request the opposite.
    fn toggle_show_desktop(&self) -> Result<()> {
        let atom = self.atom(b"_NET_SHOWING_DESKTOP")?;
        let showing = self
            .root_cardinals(b"_NET_SHOWING_DESKTOP", AtomEnum::CARDINAL, 1)?
            .into_iter()
            .next()
            .unwrap_or(0);
        self.send_root_message(self.root, atom, [u32::from(showing == 0), 0, 0, 0, 0])
    }

    /// Toggle the window's always-on-top (`_NET_WM_STATE_ABOVE`) state.
    fn toggle_above(&self, win: Window) -> Result<()> {
        let state = self.atom(b"_NET_WM_STATE")?;
        let above = self.atom(b"_NET_WM_STATE_ABOVE")?;
        // 2 = toggle; source indication 1 (application).
        self.send_root_message(win, state, [2, above, 0, 1, 0])
    }

    /// Move the window to the next/previous monitor, preserving its position
    /// relative to that monitor's top-left and clamping its size to fit. A no-op
    /// when fewer than two monitors are connected.
    fn move_to_monitor(&mut self, win: Window, direction: CycleDirection) -> Result<()> {
        let mut rects: Vec<(i32, i32, i32, i32)> = self
            .conn
            .randr_get_monitors(self.root, true)?
            .reply()?
            .monitors
            .iter()
            .map(|m| (m.x as i32, m.y as i32, m.width as i32, m.height as i32))
            .collect();
        if rects.len() < 2 {
            return Ok(());
        }
        // Order left-to-right (then top-to-bottom) so "next" is predictable.
        rects.sort_by_key(|&(x, y, _, _)| (x, y));

        let (x, y, w, h) = self.current_geometry(win)?;
        let (cx, cy) = (x + w / 2, y + h / 2);
        let current = rects
            .iter()
            .position(|&(mx, my, mw, mh)| cx >= mx && cx < mx + mw && cy >= my && cy < my + mh)
            .unwrap_or(0);
        let (curx, cury, _, _) = rects[current];
        let (nx, ny, nw, nh) = rects[direction.step(current, rects.len())];

        let new_w = w.min(nw);
        let new_h = h.min(nh);
        let new_x = nx + (x - curx).clamp(0, (nw - new_w).max(0));
        let new_y = ny + (y - cury).clamp(0, (nh - new_h).max(0));
        self.unmaximize(win)?; // so the move is honored if the window was maximized
        self.move_resize(win, new_x, new_y, new_w, new_h)
    }

    fn activate_window(&self, win: Window) -> Result<()> {
        let atom = self.atom(b"_NET_ACTIVE_WINDOW")?;
        self.send_root_message(win, atom, [2, CURRENT_TIME, 0, 0, 0])
    }

    fn current_desktop(&self) -> Result<u32> {
        self.root_cardinals(b"_NET_CURRENT_DESKTOP", AtomEnum::CARDINAL, 1)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("_NET_CURRENT_DESKTOP is unavailable"))
    }

    fn switch_desktop(&self, index: u32) -> Result<()> {
        let atom = self.atom(b"_NET_CURRENT_DESKTOP")?;
        self.send_root_message(self.root, atom, [index, CURRENT_TIME, 0, 0, 0])
    }

    fn move_window_to_desktop(&self, win: Window, index: u32) -> Result<()> {
        let atom = self.atom(b"_NET_WM_DESKTOP")?;
        self.send_root_message(win, atom, [index, 2, 0, 0, 0])
    }

    /// Resolve a [`Workspace`] target to a 0-based desktop index.
    fn resolve_workspace(&self, target: Workspace) -> Result<u32> {
        Ok(match target {
            Workspace::Index(n) => n.saturating_sub(1), // config is 1-based
            Workspace::Prev => self.current_desktop()?.saturating_sub(1),
            Workspace::Next => {
                let count = self
                    .root_cardinals(b"_NET_NUMBER_OF_DESKTOPS", AtomEnum::CARDINAL, 1)?
                    .into_iter()
                    .next()
                    .unwrap_or(0);
                let next = self.current_desktop()? + 1;
                if count > 0 {
                    next.min(count - 1)
                } else {
                    next
                }
            }
        })
    }

    fn send_root_message(&self, win: Window, type_: u32, data: [u32; 5]) -> Result<()> {
        let event = ClientMessageEvent::new(32, win, type_, data);
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
            event,
        )?;
        self.conn.flush()?;
        Ok(())
    }
}

// Functions

/// Top-left position for a window of size `nw`×`nh` placed at `anchor` within
/// the work area (`ax, ay, aw, ah`); `None` = centred.
fn anchor_pos(
    ax: i32,
    ay: i32,
    aw: i32,
    ah: i32,
    nw: i32,
    nh: i32,
    anchor: Option<Corner>,
) -> (i32, i32) {
    match anchor {
        None => (ax + (aw - nw) / 2, ay + (ah - nh) / 2),
        Some(Corner::TopLeft) => (ax, ay),
        Some(Corner::TopRight) => (ax + aw - nw, ay),
        Some(Corner::BottomLeft) => (ax, ay + ah - nh),
        Some(Corner::BottomRight) => (ax + aw - nw, ay + ah - nh),
    }
}

/// Grow a client window's geometry outward by `_NET_FRAME_EXTENTS`
/// `(left, right, top, bottom)` to obtain the outer frame rectangle the window
/// manager draws around it. The size-wise inverse of [`moveresize_message`].
fn frame_geom(
    (x, y, w, h): (i32, i32, i32, i32),
    (left, right, top, bottom): (i32, i32, i32, i32),
) -> (i32, i32, i32, i32) {
    (x - left, y - top, w + left + right, h + top + bottom)
}

/// Translate a target frame rectangle into the `(x, y, w, h)` payload for
/// `_NET_MOVERESIZE_WINDOW`, which expects the frame position (kept as-is) but
/// the client size (extents stripped from width and height).
fn moveresize_message(
    (x, y, w, h): (i32, i32, i32, i32),
    (left, right, top, bottom): (i32, i32, i32, i32),
) -> (i32, i32, i32, i32) {
    (x, y, w - left - right, h - top - bottom)
}

/// The largest absolute change across the four geometry dimensions of `a` → `b`.
fn max_delta(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> i32 {
    [b.0 - a.0, b.1 - a.1, b.2 - a.2, b.3 - a.3]
        .into_iter()
        .map(i32::abs)
        .max()
        .unwrap_or(0)
}

/// Linear interpolation between two integer endpoints at fraction `t` ∈ [0, 1].
fn lerp(from: i32, to: i32, t: f64) -> i32 {
    from + ((to - from) as f64 * t).round() as i32
}

/// The intermediate geometries for an ease-out cubic transition from `start` to
/// `target` over `steps` frames (fast start, gentle settle). The final frame
/// lands exactly on `target`, so no separate correcting move is needed.
fn anim_frames(
    start: (i32, i32, i32, i32),
    target: (i32, i32, i32, i32),
    steps: u32,
) -> Vec<(i32, i32, i32, i32)> {
    (1..=steps)
        .map(|i| {
            let p = i as f64 / steps as f64;
            let eased = 1.0 - (1.0 - p).powi(3);
            (
                lerp(start.0, target.0, eased),
                lerp(start.1, target.1, eased),
                lerp(start.2, target.2, eased),
                lerp(start.3, target.3, eased),
            )
        })
        .collect()
}

/// Print candidate keyboard devices and their paths.
pub fn list_devices() -> Result<()> {
    println!("Candidate input devices (* = detected as a keyboard):\n");
    for (path, device) in evdev::enumerate() {
        let marker = if is_keyboard(&device) { "*" } else { " " };
        println!(
            "{marker} {:<18} {}",
            path.display(),
            device.name().unwrap_or("<unnamed>")
        );
    }
    Ok(())
}

/// Run the Linux event loop until interrupted.
pub fn run(mut engine: Engine, options: Options) -> Result<()> {
    replace_or_reject(options.force)?;

    let devices = open_devices(&options.devices)?;
    if devices.is_empty() {
        anyhow::bail!("no keyboard devices found (try --list-devices, or run with privileges)");
    }
    let mut virtual_device = build_virtual_device()?;
    let mut watcher = X11Watcher::connect()?;

    let (tx, rx): (Sender<InEvent>, Receiver<InEvent>) = mpsc::channel();
    for mut device in devices {
        device
            .grab()
            .with_context(|| format!("grabbing {:?}", device.name()))?;
        let tx = tx.clone();
        thread::spawn(move || read_device(device, tx));
    }
    drop(tx);

    log::info!("RightKeys running; press Ctrl-C to stop");
    let mut app = String::new();
    // When a tap-hold key is held undecided, wait only until its timeout so the
    // hold modifier can be committed even if no other key follows.
    let mut hold_deadline: Option<Instant> = None;
    loop {
        let event = match hold_deadline {
            Some(deadline) => {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(event) => event,
                    Err(RecvTimeoutError::Timeout) => {
                        if crate::tray::is_enabled() {
                            emit(&mut virtual_device, &engine.flush_pending_hold())?;
                        }
                        hold_deadline = None;
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match rx.recv() {
                Ok(event) => event,
                Err(_) => break,
            },
        };

        // Apply a live-reloaded config, if one is ready.
        if let Some(config) = crate::reload::take() {
            engine.set_config(config);
            crate::notify::info("RightKeys reloaded!");
        }
        // When paused from the tray, forward every key untouched.
        if !crate::tray::is_enabled() {
            emit_raw(&mut virtual_device, event.code, event.value)?;
            continue;
        }
        // The active window only matters when deciding a fresh press; refresh it
        // there to keep X11 round-trips off the repeat/release hot path.
        if event.value == 1 {
            app = watcher.active_app();
        }
        match Key::from_evdev_code(event.code) {
            Some(key) => {
                let out = engine.on_event(key, event.value, &app);
                log::debug!(
                    "[{app}] {:<7} {key:?} -> {}",
                    action(event.value),
                    format_out(&out)
                );
                emit(&mut virtual_device, &out)?;
                // Perform any side effects the binding produced (launch/window).
                for effect in engine.take_effects() {
                    if let Err(err) = watcher.perform_effect(&effect) {
                        log::warn!("effect {effect:?} failed: {err:#}");
                    }
                }
            }
            None => {
                log::debug!(
                    "[{app}] {:<7} code {} -> raw",
                    action(event.value),
                    event.code
                );
                // Unknown key: preserve any held modifiers, then forward it raw.
                emit(&mut virtual_device, &engine.sync_modifiers())?;
                emit_raw(&mut virtual_device, event.code, event.value)?;
            }
        }

        // Arm the timeout while a tap-hold decision is pending; clear it once the
        // key resolves (another key, release, or an earlier timeout flush).
        hold_deadline = engine
            .has_pending_hold()
            .then(|| hold_deadline.unwrap_or_else(|| Instant::now() + TAP_HOLD_TIMEOUT));
    }
    Ok(())
}

fn read_device(mut device: Device, tx: Sender<InEvent>) {
    loop {
        let events = match device.fetch_events() {
            Ok(events) => events,
            Err(err) => {
                log::warn!("device read error: {err}");
                return;
            }
        };
        for event in events {
            if let EventSummary::Key(_, code, value) = event.destructure() {
                if tx
                    .send(InEvent {
                        code: code.code(),
                        value,
                    })
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

fn open_devices(selectors: &[String]) -> Result<Vec<Device>> {
    if selectors.is_empty() {
        return Ok(evdev::enumerate()
            .filter(|(_, device)| is_keyboard(device))
            .map(|(_, device)| device)
            .collect());
    }
    let mut devices = Vec::new();
    for selector in selectors {
        let device =
            open_selector(selector).with_context(|| format!("opening device {selector:?}"))?;
        devices.push(device);
    }
    Ok(devices)
}

fn open_selector(selector: &str) -> Result<Device> {
    let path = PathBuf::from(selector);
    if path.exists() {
        return Device::open(&path).map_err(Into::into);
    }
    evdev::enumerate()
        .find(|(_, device)| device.name() == Some(selector))
        .map(|(_, device)| device)
        .ok_or_else(|| anyhow!("no device matching {selector:?}"))
}

fn is_keyboard(device: &Device) -> bool {
    device.supported_keys().is_some_and(|keys| {
        keys.contains(KeyCode::KEY_A)
            && keys.contains(KeyCode::KEY_Z)
            && keys.contains(KeyCode::KEY_SPACE)
            && !keys.contains(KeyCode::BTN_LEFT)
    })
}

fn build_virtual_device() -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for code in 1..=MAX_KEY_CODE {
        keys.insert(KeyCode::new(code));
    }
    VirtualDevice::builder()
        .context("creating uinput device (is /dev/uinput accessible?)")?
        .name(VIRTUAL_DEVICE_NAME)
        .with_keys(&keys)?
        .build()
        .map_err(Into::into)
}

fn emit(device: &mut VirtualDevice, events: &[OutEvent]) -> Result<()> {
    // Flush each event in its own SYN_REPORT frame. A single
    // batched frame makes the modifier-downs and the key press atomic, which
    // stops window-manager shortcut grabs from recognising the chord.
    for event in events {
        device.emit(&[InputEvent::new(
            EventType::KEY.0,
            event.key.evdev_code(),
            event.value,
        )])?;
    }
    Ok(())
}

fn emit_raw(device: &mut VirtualDevice, code: u16, value: i32) -> Result<()> {
    device.emit(&[InputEvent::new(EventType::KEY.0, code, value)])?;
    Ok(())
}

/// Handle an already-running instance. With `force`, terminate every other copy;
/// otherwise reject the launch so two instances don't fight over the keyboard.
fn replace_or_reject(force: bool) -> Result<()> {
    let others = other_instances();
    if others.is_empty() {
        return Ok(());
    }
    if !force {
        crate::notify::info("RightKeys is already running");
        anyhow::bail!(
            "another RightKeys instance is already running.\nRun with --force to replace it"
        );
    }
    // Replace any already-running instance so a relaunch just restarts cleanly
    // (the old process still holds the keyboard grab until it exits).
    for pid in others {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();
    }
    log::info!("replaced a running RightKeys instance");
    crate::notify::info("RightKeys replaced a running instance");
    thread::sleep(std::time::Duration::from_millis(400));
    Ok(())
}

/// PIDs of other processes running this program.
///
/// Processes are matched on their `/proc/<pid>/exe` link rather than their
/// `comm` name (which is truncated and trivially spoofable), comparing the
/// executable's file name so an instance launched from a different path (e.g.
/// the installed `/usr/local/bin/rightkeys` vs a freshly built
/// `target/release/rightkeys`) still counts. (Signalling a copy run by a
/// different user later fails the `kill` with `EPERM`, which is ignored.)
fn other_instances() -> Vec<u32> {
    let self_pid = std::process::id();
    let Ok(self_exe) = std::fs::read_link("/proc/self/exe") else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pids = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) {
            if same_program(&exe, &self_exe) {
                pids.push(pid);
            }
        }
    }
    pids
}

/// Whether two `/proc/.../exe` targets are the same program, compared on the
/// executable's file name. The ` (deleted)` suffix the kernel appends once the
/// binary's file has been replaced (e.g. by `make install` while an old
/// instance is still running) is stripped first.
fn same_program(a: &Path, b: &Path) -> bool {
    let (a, b) = (strip_deleted(a).file_name(), strip_deleted(b).file_name());
    a.is_some() && a == b
}

fn strip_deleted(path: &Path) -> &Path {
    match path.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(stripped) => Path::new(stripped),
        None => path,
    }
}

/// Short label for an evdev key value (`1` press, `0` release, `2` repeat).
fn action(value: i32) -> &'static str {
    match value {
        0 => "release",
        1 => "press",
        2 => "repeat",
        _ => "?",
    }
}

/// Render the engine's output events compactly: `+Key` for press, `-Key` for
/// release (e.g. `+LeftMeta +Left`).
fn format_out(events: &[OutEvent]) -> String {
    if events.is_empty() {
        return "(none)".to_string();
    }
    events
        .iter()
        .map(|e| format!("{}{:?}", if e.value == 0 { '-' } else { '+' }, e.key))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extract the class name from a raw `WM_CLASS` value of the form
/// `instance\0class\0`.
fn parse_wm_class(value: &[u8]) -> String {
    let text = String::from_utf8_lossy(value);
    let mut parts = text.split('\0').filter(|s| !s.is_empty());
    let instance = parts.next();
    parts.next().or(instance).unwrap_or("").to_string()
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wm_class_pair() {
        assert_eq!(parse_wm_class(b"navigator\0Firefox\0"), "Firefox");
    }

    #[test]
    fn parses_wm_class_single() {
        assert_eq!(parse_wm_class(b"Code\0"), "Code");
    }

    #[test]
    fn parses_wm_class_empty() {
        assert_eq!(parse_wm_class(b""), "");
    }

    #[test]
    fn same_program_matches_across_paths() {
        assert!(same_program(
            Path::new("/usr/local/bin/rightkeys"),
            Path::new("/home/u/rightkeys/target/release/rightkeys"),
        ));
    }

    #[test]
    fn same_program_ignores_deleted_suffix() {
        assert!(same_program(
            Path::new("/usr/local/bin/rightkeys (deleted)"),
            Path::new("/usr/local/bin/rightkeys"),
        ));
    }

    #[test]
    fn same_program_rejects_other_binaries() {
        assert!(!same_program(
            Path::new("/usr/bin/other"),
            Path::new("/usr/local/bin/rightkeys"),
        ));
    }

    #[test]
    fn max_delta_picks_largest_dimension() {
        assert_eq!(max_delta((0, 0, 100, 100), (5, -30, 140, 100)), 40);
        assert_eq!(max_delta((10, 10, 10, 10), (10, 10, 10, 10)), 0);
    }

    #[test]
    fn frame_geom_grows_client_outward_by_extents() {
        // Client at (10, 20) of size 300×200; a 3/5/30/4 frame (typical
        // title-bar-heavy Linux WM) yields an outer rectangle shifted up-left
        // and grown by the sum of opposing sides.
        let client = (10, 20, 300, 200);
        let extents = (3, 5, 30, 4);
        assert_eq!(frame_geom(client, extents), (7, -10, 308, 234));
    }

    #[test]
    fn moveresize_message_keeps_position_strips_extents_from_size() {
        // Inverse of `frame_geom` on size: the frame rectangle lands where the
        // caller asked, and `_NET_MOVERESIZE_WINDOW` is told the smaller client
        // size so the WM-painted frame reaches the target edges.
        let frame = (7, -10, 308, 234);
        let extents = (3, 5, 30, 4);
        assert_eq!(moveresize_message(frame, extents), (7, -10, 300, 200));
    }

    #[test]
    fn anim_frames_land_exactly_on_target() {
        let frames = anim_frames((0, 0, 0, 0), (1000, 500, 800, 600), ANIM_STEPS);
        assert_eq!(frames.len(), ANIM_STEPS as usize);
        assert_eq!(*frames.last().unwrap(), (1000, 500, 800, 600));
    }

    #[test]
    fn anim_frames_ease_out_so_the_first_step_outruns_linear() {
        // Ease-out cubic moves fast up front: the first frame covers more ground
        // than a constant-speed (linear) step would.
        let frames = anim_frames((0, 0, 0, 0), (1600, 0, 0, 0), ANIM_STEPS);
        let linear_first = 1600 / ANIM_STEPS as i32;
        assert!(frames[0].0 > linear_first, "{} !> {linear_first}", frames[0].0);
    }
}
