//! Linux backend: grabs keyboards via `evdev`, injects through a `uinput`
//! virtual device, and reads the active window's `WM_CLASS` over X11.

// Submodules

mod enumerator;
mod executor;

// Imports

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ab_glyph::{point, Font, FontVec, PxScale, ScaleFont};
use anyhow::{anyhow, Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, Device, EventSummary, EventType, InputEvent, KeyCode};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt, CreateGCAux,
    CreateWindowAux, EventMask, Gcontext, ImageFormat, ImageOrder, NotifyMode, Pixmap, Window,
    WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::CURRENT_TIME;

use super::actions::pickwindow::{
    advance, key_to_hint_char, make_hints, place_hint, split_app_from_title, HintMatch,
};
use super::{Options, WindowWatcher};
use crate::engine::{
    Corner, CycleDirection, Effect, Engine, OutEvent, Side, WindowAction, Workspace, STEP_DIVISOR,
};
use crate::key::Key;
use super::actions::pickelement::{Element, HintAction, HintSession, HINT_CHARS};

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

/// Pixel size the pick-window overlay renders the hint key at.
const OVERLAY_FONT_PX: f32 = 26.0;

/// Pixel size for the app-name line (the brand stripped from the title).
const OVERLAY_APP_FONT_PX: f32 = 20.0;

/// Pixel size for the smaller window-title line beneath the app name.
const OVERLAY_INFO_FONT_PX: f32 = 16.0;

/// Font size for pick-element hint badges.
const ELEMENT_HINT_FONT_PX: f32 = 16.0;

/// Padding inside an element hint badge: 2 px vertical, 4 px horizontal.
const ELEMENT_HINT_VPAD: i32 = 2;
const ELEMENT_HINT_HPAD: i32 = 4;

/// Vertical gap between the app-name and window-title lines (pixels).
const INFO_LINE_GAP: i32 = 1;

/// Maximum width of the app/title info text before it is truncated (pixels).
const MAX_INFO_WIDTH_PX: i32 = 560;

/// Vertical padding above and below the text inside an overlay chip (pixels).
const OVERLAY_VPAD: i32 = 3;

/// Horizontal padding inside the hint key chip (pixels on each side).
const HINT_CHIP_PAD: i32 = 9;
/// Horizontal padding inside the app-name chip (pixels on each side).
const APP_CHIP_PAD: i32 = 11;

/// Overlay chip colors as `(r, g, b)`: ice-blue badge with deep-navy text and
/// a medium-blue border. The pick-window info
/// chip uses a slightly darker blue field with off-white text.
const HINT_BG: (u8, u8, u8) = (0xca, 0xe0, 0xfa);
const HINT_FG: (u8, u8, u8) = (0x29, 0x72, 0xb6);
const HINT_BORDER: (u8, u8, u8) = (0x3b, 0x82, 0xf6);
const APP_BG: (u8, u8, u8) = (0x1e, 0x40, 0x7a);
const APP_FG: (u8, u8, u8) = (0xe8, 0xee, 0xf6);
/// Window-title text: a light slate that reads as a subtitle under the app name.
const TITLE_FG: (u8, u8, u8) = (0xc4, 0xcf, 0xde);

/// Fonts tried, in order, when `fc-match` cannot resolve the system sans font.
const FALLBACK_FONTS: &[&str] = &[
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
];

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

/// One entry in the pick-window hint overlay: a target window with the
/// override-redirect window and the pixmap holding its pre-rendered chip image.
/// Its hint label lives at the same index in [`PickWindowOverlay::hints`].
struct HintEntry {
    overlay: Window,
    pixmap: Pixmap,
    target: Window,
}

/// Active state of the Vimium-style window-finder overlay.
struct PickWindowOverlay {
    entries: Vec<HintEntry>,
    hints: Vec<String>,
    prefix: String,
}

/// One element hint chip in the find-element overlay.
struct ElementEntry {
    overlay: Window,
    pixmap: Pixmap,
}

/// Active state of the pick-element hint overlay.
struct PickElementOverlay {
    entries: Vec<ElementEntry>,
}

/// A CPU-rendered RGB image, row-major, used to compose an overlay chip before
/// uploading it to the X server.
struct TextImage {
    width: u16,
    height: u16,
    pixels: Vec<(u8, u8, u8)>,
}

/// The server's Z-pixmap layout for the root depth, so a [`TextImage`] can be
/// serialized into the byte order and channel positions `put_image` expects.
struct ImageFmt {
    bytes_per_pixel: usize,
    scanline_pad_bytes: usize,
    lsb_first: bool,
    r_shift: u32,
    g_shift: u32,
    b_shift: u32,
}

/// Rasterizes overlay labels with the system font: the hint key at
/// [`OVERLAY_FONT_PX`], the app-name line at [`OVERLAY_APP_FONT_PX`], and the
/// window-title line at [`OVERLAY_INFO_FONT_PX`].
///
/// `fonts[0]` is the primary sans font; later entries are fallbacks loaded on
/// demand (via fontconfig) to cover characters the primary lacks — CJK, Arabic,
/// and other scripts — so non-Latin titles render instead of showing blanks.
struct OverlayRenderer {
    fonts: Vec<FontVec>,
    /// Loaded fallback font file paths → index into `fonts`, to avoid reloading.
    loaded: HashMap<String, usize>,
    /// Resolved character → `fonts` index, memoized across a render pass.
    cache: HashMap<char, usize>,
    hint_scale: PxScale,
    app_scale: PxScale,
    info_scale: PxScale,
}

// === PickWindowOverlay ===

impl PickWindowOverlay {
    /// Feed a hint character; returns the chosen target window once a single
    /// hint remains.
    fn input(&mut self, ch: char) -> Option<Window> {
        match advance(&self.hints, &mut self.prefix, ch) {
            HintMatch::Done(i) => Some(self.entries[i].target),
            HintMatch::Pending => None,
        }
    }

    fn backspace(&mut self) {
        self.prefix.pop();
    }
}

// === TextImage ===

impl TextImage {
    /// A `width`×`height` image filled with `bg`.
    fn new(width: u16, height: u16, bg: (u8, u8, u8)) -> Self {
        TextImage {
            width,
            height,
            pixels: vec![bg; width as usize * height as usize],
        }
    }

    /// Set one pixel, ignoring out-of-bounds coordinates.
    fn set(&mut self, x: i32, y: i32, color: (u8, u8, u8)) {
        if x >= 0 && y >= 0 && x < self.width as i32 && y < self.height as i32 {
            self.pixels[y as usize * self.width as usize + x as usize] = color;
        }
    }

    /// Fill a rectangle, clipped to the image bounds.
    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: (u8, u8, u8)) {
        for py in y..y + h {
            for px in x..x + w {
                self.set(px, py, color);
            }
        }
    }

    /// Draw a 1-pixel rectangle outline, clipped to the image bounds.
    fn draw_outline(&mut self, x: i32, y: i32, w: i32, h: i32, color: (u8, u8, u8)) {
        for px in x..x + w {
            self.set(px, y, color);
            self.set(px, y + h - 1, color);
        }
        for py in y..y + h {
            self.set(x, py, color);
            self.set(x + w - 1, py, color);
        }
    }

    /// Alpha-blend `color` over the pixel at `(x, y)` with coverage `cov` ∈ [0, 1].
    fn blend(&mut self, x: i32, y: i32, color: (u8, u8, u8), cov: f32) {
        if cov <= 0.0 || x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let idx = y as usize * self.width as usize + x as usize;
        let (br, bg, bb) = self.pixels[idx];
        let mix = |base: u8, fg: u8| {
            (base as f32 * (1.0 - cov) + fg as f32 * cov).round().clamp(0.0, 255.0) as u8
        };
        self.pixels[idx] = (mix(br, color.0), mix(bg, color.1), mix(bb, color.2));
    }
}

// === OverlayRenderer ===

impl OverlayRenderer {
    /// Load the system sans font as the primary; fallbacks load on demand.
    fn load() -> Result<Self> {
        let bytes = system_font_bytes()?;
        let font = FontVec::try_from_vec(bytes)
            .map_err(|err| anyhow!("parsing the overlay font: {err}"))?;
        Ok(OverlayRenderer {
            fonts: vec![font],
            loaded: HashMap::new(),
            cache: HashMap::new(),
            hint_scale: PxScale::from(OVERLAY_FONT_PX),
            app_scale: PxScale::from(OVERLAY_APP_FONT_PX),
            info_scale: PxScale::from(OVERLAY_INFO_FONT_PX),
        })
    }

    /// Index into `fonts` of a font that can render `c`: the primary if it has
    /// the glyph, else an already-loaded fallback, else one resolved through
    /// fontconfig and loaded now. Falls back to the primary (index 0, a blank
    /// `.notdef`) when nothing covers `c`. Results are memoized.
    fn resolve_font(&mut self, c: char) -> usize {
        if let Some(&idx) = self.cache.get(&c) {
            return idx;
        }
        let idx = self.resolve_uncached(c);
        self.cache.insert(c, idx);
        idx
    }

    fn resolve_uncached(&mut self, c: char) -> usize {
        if c.is_whitespace() || self.fonts[0].glyph_id(c).0 != 0 {
            return 0;
        }
        for i in 1..self.fonts.len() {
            if self.fonts[i].glyph_id(c).0 != 0 {
                return i;
            }
        }
        let Some(path) = fc_match_char(c) else {
            return 0;
        };
        if let Some(&i) = self.loaded.get(&path) {
            return i;
        }
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(font) = FontVec::try_from_vec(bytes) {
                if font.glyph_id(c).0 != 0 {
                    self.fonts.push(font);
                    let i = self.fonts.len() - 1;
                    self.loaded.insert(path, i);
                    return i;
                }
            }
        }
        0
    }

    /// Pixel width that `text` will occupy at `scale` (sum of glyph advances,
    /// each from the font that covers the character).
    fn text_width(&mut self, text: &str, scale: PxScale) -> i32 {
        let mut width = 0.0;
        for c in text.chars() {
            let idx = self.resolve_font(c);
            let font = &self.fonts[idx];
            width += font.as_scaled(scale).h_advance(font.glyph_id(c));
        }
        width.ceil() as i32
    }

    /// Full line height (ascent plus descent) at `scale`, from the primary font
    /// so all lines share a baseline rhythm regardless of fallbacks.
    fn line_height(&self, scale: PxScale) -> i32 {
        let scaled = self.fonts[0].as_scaled(scale);
        (scaled.ascent() - scaled.descent()).ceil() as i32
    }

    /// Ascent of the primary font at `scale`, used to place baselines.
    fn ascent(&self, scale: PxScale) -> f32 {
        self.fonts[0].as_scaled(scale).ascent()
    }

    /// `text` shortened with a trailing ellipsis so it fits within `max_px` at
    /// `scale`; returned unchanged when it already fits.
    fn truncate(&mut self, text: &str, scale: PxScale, max_px: i32) -> String {
        if text.is_empty() || self.text_width(text, scale) <= max_px {
            return text.to_string();
        }
        let ellipsis = '…';
        let ellipsis_w = {
            let idx = self.resolve_font(ellipsis);
            let font = &self.fonts[idx];
            font.as_scaled(scale).h_advance(font.glyph_id(ellipsis))
        };
        let mut out = String::new();
        let mut width = 0.0;
        for c in text.chars() {
            let idx = self.resolve_font(c);
            let font = &self.fonts[idx];
            let advance = font.as_scaled(scale).h_advance(font.glyph_id(c));
            if width + advance + ellipsis_w > max_px as f32 {
                break;
            }
            out.push(c);
            width += advance;
        }
        out.push(ellipsis);
        out
    }

    /// Draw `text` left-aligned at `x0`, on `baseline`, at `scale`, in `color`,
    /// rasterizing each character from the font that covers it.
    fn draw_text(
        &mut self,
        img: &mut TextImage,
        text: &str,
        x0: i32,
        baseline: f32,
        scale: PxScale,
        color: (u8, u8, u8),
    ) {
        let mut pen = x0 as f32;
        for c in text.chars() {
            let idx = self.resolve_font(c);
            let font = &self.fonts[idx];
            let id = font.glyph_id(c);
            let glyph = id.with_scale_and_position(scale, point(pen, baseline));
            if let Some(outline) = font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|gx, gy, cov| {
                    img.blend(
                        bounds.min.x as i32 + gx as i32,
                        bounds.min.y as i32 + gy as i32,
                        color,
                        cov,
                    );
                });
            }
            pen += font.as_scaled(scale).h_advance(id);
        }
    }

    /// Compose one hint's label: a bordered hint-key chip on the left, and an
    /// info chip on the right carrying the app name (larger) above the window
    /// title (smaller). The info chip is omitted when both are empty; a missing
    /// app or title collapses to the single remaining line.
    fn render_label(&mut self, hint: &str, app: &str, title: &str) -> TextImage {
        let app = self.truncate(app, self.app_scale, MAX_INFO_WIDTH_PX);
        let title = self.truncate(title, self.info_scale, MAX_INFO_WIDTH_PX);
        let has_app = !app.is_empty();
        let has_title = !title.is_empty();
        let has_info = has_app || has_title;

        let hint_line = self.line_height(self.hint_scale);
        let app_line = self.line_height(self.app_scale);
        let title_line = self.line_height(self.info_scale);
        let info_block = match (has_app, has_title) {
            (true, true) => app_line + INFO_LINE_GAP + title_line,
            (true, false) => app_line,
            (false, true) => title_line,
            (false, false) => 0,
        };
        let height = hint_line.max(info_block) + OVERLAY_VPAD * 2;

        let hint_w = self.text_width(hint, self.hint_scale) + HINT_CHIP_PAD * 2;
        let info_w = if has_info {
            self.text_width(&app, self.app_scale)
                .max(self.text_width(&title, self.info_scale))
                + APP_CHIP_PAD * 2
        } else {
            0
        };

        let mut img = TextImage::new((hint_w + info_w) as u16, height as u16, HINT_BG);
        if info_w > 0 {
            img.fill_rect(hint_w, 0, info_w, height, APP_BG);
        }
        img.draw_outline(0, 0, hint_w, height, HINT_BORDER);

        // Center the hint key vertically against the (possibly taller) info block.
        let hint_baseline = (height - hint_line) as f32 / 2.0 + self.ascent(self.hint_scale);
        self.draw_text(&mut img, hint, HINT_CHIP_PAD, hint_baseline, self.hint_scale, HINT_FG);

        if has_info {
            let x = hint_w + APP_CHIP_PAD;
            let mut top = (height - info_block) as f32 / 2.0;
            if has_app {
                let baseline = top + self.ascent(self.app_scale);
                self.draw_text(&mut img, &app, x, baseline, self.app_scale, APP_FG);
                top += (app_line + INFO_LINE_GAP) as f32;
            }
            if has_title {
                let baseline = top + self.ascent(self.info_scale);
                self.draw_text(&mut img, &title, x, baseline, self.info_scale, TITLE_FG);
            }
        }
        img
    }

    /// Render a small pick-element hint badge: just the key label, no info chip.
    fn render_element_hint(&mut self, hint: &str) -> TextImage {
        let scale = PxScale::from(ELEMENT_HINT_FONT_PX);
        let line = self.line_height(scale);
        let height = line + ELEMENT_HINT_VPAD * 2;
        let width = self.text_width(hint, scale) + ELEMENT_HINT_HPAD * 2;
        let mut img = TextImage::new(width as u16, height as u16, HINT_BG);
        img.draw_outline(0, 0, width, height, HINT_BORDER);
        let baseline = ELEMENT_HINT_VPAD as f32 + self.ascent(scale);
        self.draw_text(&mut img, hint, ELEMENT_HINT_HPAD, baseline, scale, HINT_FG);
        img
    }
}

/// X11-based active-window watcher with a one-entry focus cache. Also performs
/// window-management [`Effect`]s via EWMH messages to the root window.
struct X11Watcher {
    conn: RustConnection,
    root: Window,
    screen_num: usize,
    cached_focus: Window,
    cached_class: String,
    last_move: Option<MoveCache>,
    /// Geometry to restore when a maximize-toggle un-fills the window we last
    /// filled; `None` once restored, or for a window we never filled.
    restore_geom: Option<(Window, (i32, i32, i32, i32))>,
    /// True while another X11 client holds a keyboard grab (e.g. Rofi). When
    /// set, `active_app` returns an empty string so no application-scoped
    /// keymap matches and keys are forwarded raw.
    keyboard_grabbed: bool,
}

// === X11Watcher ===

impl X11Watcher {
    fn connect() -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None).context("connecting to X11 display")?;
        let root = conn.setup().roots[screen_num].root;
        Ok(X11Watcher {
            conn,
            root,
            screen_num,
            cached_focus: 0,
            cached_class: String::new(),
            last_move: None,
            restore_geom: None,
            keyboard_grabbed: false,
        })
    }

    /// Subscribe to `FocusChangeMask` on `window` so we receive `FocusOut` with
    /// `mode = Grab` when another client grabs the keyboard while `window` is focused.
    fn subscribe_focus(&self, window: Window) {
        if window > 1 {
            let _ = self.conn.change_window_attributes(
                window,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::FOCUS_CHANGE),
            );
            let _ = self.conn.flush();
        }
    }

    /// Drain pending X11 events and update keyboard-grab state.
    ///
    /// `FocusOut(Grab)` on the tracked window means another client grabbed the
    /// keyboard (e.g. Rofi); `FocusIn(Ungrab)` means the grab was released.
    pub fn poll_focus_events(&mut self) {
        while let Ok(Some(event)) = self.conn.poll_for_event() {
            match event {
                Event::FocusOut(ev)
                    if ev.mode == NotifyMode::GRAB && ev.event == self.cached_focus =>
                {
                    self.keyboard_grabbed = true;
                    self.cached_class = String::new();
                }
                // Any ungrab clears the flag. The matching grab was on our tracked
                // window, but the releasing FocusIn can land on a different window
                // when focus moved while grabbed (e.g. the grabbed window closed),
                // so matching the window here would miss the release.
                Event::FocusIn(ev) if ev.mode == NotifyMode::UNGRAB => {
                    self.keyboard_grabbed = false;
                    self.cached_focus = 0; // force refresh on next active_app() call
                }
                _ => {}
            }
        }
        // Self-heal a stuck grab: if the window we are awaiting the ungrab on was
        // destroyed (closed by the very shortcut whose grab we saw, e.g. Alt+Q
        // closing the focused window), that FocusIn(Ungrab) never arrives. Drop
        // the stale flag so remapping resumes instead of forwarding every key raw
        // until a restart.
        if self.keyboard_grabbed && !self.window_exists(self.cached_focus) {
            self.keyboard_grabbed = false;
            self.cached_focus = 0;
        }
    }

    /// Whether `window` still exists on the server. A destroyed window yields a
    /// `BadWindow` error on any request, which surfaces when the cookie's reply
    /// is awaited.
    fn window_exists(&self, window: Window) -> bool {
        window != 0 && self.conn.get_geometry(window).is_ok_and(|c| c.reply().is_ok())
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
        if self.keyboard_grabbed {
            return String::new();
        }
        let focus = match self.conn.get_input_focus().map(|cookie| cookie.reply()) {
            Ok(Ok(reply)) => reply.focus,
            _ => return self.cached_class.clone(),
        };
        if focus != self.cached_focus {
            self.cached_focus = focus;
            self.cached_class = self.wm_class(focus).unwrap_or_default();
            self.subscribe_focus(focus);
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
            WindowAction::StepToward(corner) => {
                if self.wm_class(win).as_deref() == Some("xfce4-panel") {
                    return Ok(());
                }
                let (ax, ay, aw, ah) = self.work_area()?;
                let (x, y, w, h) = self.current_geometry(win)?;
                let (tx, ty) = anchor_pos(ax, ay, aw, ah, w, h, Some(corner));
                let rdx = (tx - x) as f64;
                let rdy = (ty - y) as f64;
                let dist = rdx.hypot(rdy);
                let mag = (aw as f64).hypot(ah as f64) / STEP_DIVISOR as f64;
                let (nx, ny) = if dist <= mag {
                    (tx, ty)
                } else {
                    (x + (rdx * mag / dist).round() as i32, y + (rdy * mag / dist).round() as i32)
                };
                self.move_resize(win, nx, ny, w, h)?;
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
            WindowAction::PickWindow => {} // intercepted in run() before perform_effect
            WindowAction::PickElement => {} // intercepted in run() before perform_effect
            WindowAction::Workspace { .. } | WindowAction::ShowDesktop => {
                unreachable!("handled above")
            }
        }
        Ok(())
    }

    /// Activate an existing window whose `WM_CLASS` matches `program`, else launch it.
    fn activate_or_launch(&self, program: &str) -> Result<()> {
        // Split shell-style so an argument can contain spaces when quoted, e.g.
        //   exec linux="raise-or-run.sh -w 'Brave-browser:Google Scholar' -c run.sh"
        let args = shell_words::split(program)
            .with_context(|| format!("parsing exec command {program:?}"))?;
        let Some((bin, rest)) = args.split_first() else {
            return Ok(());
        };
        let stem = bin.rsplit('/').next().unwrap_or(bin).to_lowercase();
        if let Ok(list) = self.client_list() {
            for win in list {
                if let Some(class) = self.wm_class(win) {
                    if class.to_lowercase().contains(&stem) {
                        return self.activate_window(win);
                    }
                }
            }
        }
        // No window found: launch the binary with its parsed arguments.
        std::process::Command::new(bin)
            .args(rest)
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

    // ── pick-window overlay ──

    /// Build and display the Vimium-style hint overlay over every window on the
    /// current desktop; returns the overlay state for key-event routing.
    fn start_pick_window(&mut self) -> Result<PickWindowOverlay> {
        // Pipeline all atoms before waiting for any reply.
        let desktop_atom_c = self.conn.intern_atom(false, b"_NET_CURRENT_DESKTOP")?;
        let client_list_atom_c = self.conn.intern_atom(false, b"_NET_CLIENT_LIST")?;
        let wm_desktop_atom_c = self.conn.intern_atom(false, b"_NET_WM_DESKTOP")?;
        let frame_ext_atom_c = self.conn.intern_atom(false, b"_NET_FRAME_EXTENTS")?;
        let net_name_atom_c = self.conn.intern_atom(false, b"_NET_WM_NAME")?;
        let utf8_atom_c = self.conn.intern_atom(false, b"UTF8_STRING")?;
        let desktop_atom = desktop_atom_c.reply()?.atom;
        let client_list_atom = client_list_atom_c.reply()?.atom;
        let wm_desktop_atom = wm_desktop_atom_c.reply()?.atom;
        let frame_ext_atom = frame_ext_atom_c.reply()?.atom;
        let net_name_atom = net_name_atom_c.reply()?.atom;
        let utf8_atom = utf8_atom_c.reply()?.atom;

        let cur_desk_c =
            self.conn.get_property(false, self.root, desktop_atom, AtomEnum::CARDINAL, 0, 1)?;
        let client_list_c =
            self.conn.get_property(false, self.root, client_list_atom, AtomEnum::WINDOW, 0, 4096)?;
        let desktop = cur_desk_c
            .reply()?
            .value32()
            .and_then(|mut it| it.next())
            .ok_or_else(|| anyhow!("_NET_CURRENT_DESKTOP unavailable"))?;
        let wins: Vec<Window> =
            client_list_c.reply()?.value32().map(|it| it.collect()).unwrap_or_default();

        if wins.is_empty() {
            anyhow::bail!("no windows on the current desktop");
        }

        // Pipeline 6×N per-window reads; the first .reply() flushes and waits,
        // subsequent ones drain from the already-buffered replies.
        let desk_cs: Vec<_> = wins
            .iter()
            .map(|&w| {
                self.conn.get_property(false, w, wm_desktop_atom, AtomEnum::CARDINAL, 0, 1)
            })
            .collect::<std::result::Result<_, _>>()?;
        let coord_cs: Vec<_> = wins
            .iter()
            .map(|&w| self.conn.translate_coordinates(w, self.root, 0, 0))
            .collect::<std::result::Result<_, _>>()?;
        let fext_cs: Vec<_> = wins
            .iter()
            .map(|&w| {
                self.conn.get_property(false, w, frame_ext_atom, AtomEnum::CARDINAL, 0, 4)
            })
            .collect::<std::result::Result<_, _>>()?;
        let class_cs: Vec<_> = wins
            .iter()
            .map(|&w| {
                self.conn.get_property(false, w, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
            })
            .collect::<std::result::Result<_, _>>()?;
        // Title: prefer the UTF-8 `_NET_WM_NAME`, fall back to legacy `WM_NAME`.
        let name_cs: Vec<_> = wins
            .iter()
            .map(|&w| self.conn.get_property(false, w, net_name_atom, utf8_atom, 0, 256))
            .collect::<std::result::Result<_, _>>()?;
        let wmname_cs: Vec<_> = wins
            .iter()
            .map(|&w| {
                self.conn.get_property(false, w, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 256)
            })
            .collect::<std::result::Result<_, _>>()?;
        let geo_cs: Vec<_> = wins
            .iter()
            .map(|&w| self.conn.get_geometry(w))
            .collect::<std::result::Result<_, _>>()?;

        let windows: Vec<(Window, (i32, i32), String, String)> = desk_cs
            .into_iter()
            .zip(coord_cs)
            .zip(fext_cs)
            .zip(geo_cs)
            .zip(class_cs)
            .zip(name_cs)
            .zip(wmname_cs)
            .zip(wins)
            .filter_map(|(((((((d, c), f), g), k), n), wn), win)| {
                let win_desktop = d.reply().ok()?.value32()?.next()?;
                if win_desktop != desktop {
                    return None;
                }
                let coords = c.reply().ok()?;
                // fext cookie consumed; frame extents are not needed for centering.
                let _ = f.reply();
                let client_geo = g.reply().ok()?;
                // Center of the client area in root coordinates.
                let cx = coords.dst_x as i32 + client_geo.width as i32 / 2;
                let cy = coords.dst_y as i32 + client_geo.height as i32 / 2;
                let app =
                    k.reply().ok().map(|r| parse_wm_class(&r.value)).unwrap_or_default();
                let title = n
                    .reply()
                    .ok()
                    .map(|r| parse_window_name(&r.value))
                    .filter(|s| !s.is_empty())
                    .or_else(|| wn.reply().ok().map(|r| parse_window_name(&r.value)))
                    .unwrap_or_default();
                // Line 1 shows the app's brand as it appears in the title (e.g.
                // "Visual Studio Code - Insiders"), falling back to the WM_CLASS
                // name when the title carries no recognizable brand; line 2 is
                // the remaining document/page part.
                let (brand, rest) = split_app_from_title(&title, &app);
                let label = if brand.is_empty() { app } else { brand };
                Some((win, (cx, cy), label, rest))
            })
            .collect();

        if windows.is_empty() {
            anyhow::bail!("no windows on the current desktop");
        }

        let hints = make_hints(windows.len());
        let mut renderer =
            OverlayRenderer::load().context("loading the pick-window overlay font")?;
        let fmt = self.image_fmt()?;
        // One scratch GC drives every `put_image`; freed once the chips are up.
        let upload_gc: Gcontext = self.conn.generate_id()?;
        self.conn.create_gc(upload_gc, self.root, &CreateGCAux::new())?;

        let screen = &self.conn.setup().roots[self.screen_num];
        let screen_w = screen.width_in_pixels;
        let screen_h = screen.height_in_pixels;
        let depth = screen.root_depth;

        let screen = (screen_w as i32, screen_h as i32);
        let mut placed: Vec<(i32, i32, i32, i32)> = Vec::new();
        let mut entries = Vec::new();
        for ((win, (cx, cy), app, title), hint) in windows.iter().zip(hints.iter()) {
            let img = renderer.render_label(&hint.to_uppercase(), app, title);
            let pixmap = self.upload_image(&fmt, upload_gc, depth, &img)?;
            let size = (img.width as i32, img.height as i32);
            let desired = (cx - size.0 / 2, cy - size.1 / 2);
            let (px, py) = place_hint(desired, size, &placed, screen);
            placed.push((px, py, size.0, size.1));

            let overlay: Window = self.conn.generate_id()?;
            self.conn.create_window(
                0u8,
                overlay,
                self.root,
                px as i16,
                py as i16,
                img.width,
                img.height,
                0u16,
                WindowClass::INPUT_OUTPUT,
                0u32,
                &CreateWindowAux::new()
                    .background_pixmap(pixmap)
                    .override_redirect(1u32),
            )?;
            self.conn.map_window(overlay)?;

            entries.push(HintEntry {
                overlay,
                pixmap,
                target: *win,
            });
        }
        self.conn.free_gc(upload_gc)?;
        self.conn.flush()?;

        Ok(PickWindowOverlay {
            entries,
            hints,
            prefix: String::new(),
        })
    }

    /// The server's Z-pixmap layout for the root depth, used to serialize a
    /// [`TextImage`] for `put_image`.
    fn image_fmt(&self) -> Result<ImageFmt> {
        let setup = self.conn.setup();
        let screen = &setup.roots[self.screen_num];
        let depth = screen.root_depth;
        let format = setup
            .pixmap_formats
            .iter()
            .find(|f| f.depth == depth)
            .ok_or_else(|| anyhow!("no pixmap format for depth {depth}"))?;
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == screen.root_visual)
            .ok_or_else(|| anyhow!("root visual {} not found", screen.root_visual))?;
        Ok(ImageFmt {
            bytes_per_pixel: format.bits_per_pixel as usize / 8,
            scanline_pad_bytes: format.scanline_pad as usize / 8,
            lsb_first: setup.image_byte_order == ImageOrder::LSB_FIRST,
            r_shift: visual.red_mask.trailing_zeros(),
            g_shift: visual.green_mask.trailing_zeros(),
            b_shift: visual.blue_mask.trailing_zeros(),
        })
    }

    /// Create a pixmap holding `img`, serialized to the server's pixel layout.
    fn upload_image(
        &self,
        fmt: &ImageFmt,
        gc: Gcontext,
        depth: u8,
        img: &TextImage,
    ) -> Result<Pixmap> {
        let pixmap: Pixmap = self.conn.generate_id()?;
        self.conn
            .create_pixmap(depth, pixmap, self.root, img.width, img.height)?;
        let data = encode_image(fmt, img);
        self.conn.put_image(
            ImageFormat::Z_PIXMAP,
            pixmap,
            gc,
            img.width,
            img.height,
            0,
            0,
            0,
            depth,
            &data,
        )?;
        Ok(pixmap)
    }

    /// Show overlays whose hint still matches `fw.prefix`; hide the rest. The
    /// chip image is the window's background pixmap, so a freshly mapped overlay
    /// only needs a `clear_area` to repaint it.
    fn update_pick_window_visibility(&self, fw: &PickWindowOverlay) -> Result<()> {
        for (entry, hint) in fw.entries.iter().zip(fw.hints.iter()) {
            if hint.starts_with(&fw.prefix) {
                self.conn.map_window(entry.overlay)?;
                self.conn.clear_area(false, entry.overlay, 0, 0, 0, 0)?;
            } else {
                self.conn.unmap_window(entry.overlay)?;
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Destroy all overlay windows and free their pixmaps.
    fn destroy_pick_window(&self, fw: PickWindowOverlay) {
        for entry in &fw.entries {
            let _ = self.conn.destroy_window(entry.overlay);
            let _ = self.conn.free_pixmap(entry.pixmap);
        }
        let _ = self.conn.flush();
    }

    /// The PID of the focused X11 window's process, for AT-SPI matching.
    fn focused_pid(&self) -> Option<u32> {
        let atom = self.atom(b"_NET_WM_PID").ok()?;
        // Try the X input focus first (catches apps whose focused child window
        // carries the PID, e.g. browsers with embedded render widgets), then
        // fall back to the EWMH active window.
        let focus = self.conn.get_input_focus().ok()?.reply().ok()?.focus;
        if let Some(pid) = self.window_pid(focus, atom) {
            return Some(pid);
        }
        let active = self.active_window().ok()??;
        self.window_pid(active, atom)
    }

    /// Climb the X window tree from `win` toward the root returning the first
    /// `_NET_WM_PID` found, or `None` if none is set before reaching root.
    fn window_pid(&self, mut win: Window, atom: u32) -> Option<u32> {
        if win < 2 {
            return None;
        }
        loop {
            let reply = self
                .conn
                .get_property(false, win, atom, AtomEnum::CARDINAL, 0, 1)
                .ok()?
                .reply()
                .ok()?;
            if reply.value_len > 0 {
                return reply.value32()?.next();
            }
            let tree = self.conn.query_tree(win).ok()?.reply().ok()?;
            if tree.parent == 0 || tree.parent == self.root {
                return None;
            }
            win = tree.parent;
        }
    }

    /// The client geometry of the active window, for AT-SPI window matching.
    ///
    /// Uses the raw client bounds (no frame-extents expansion) because AT-SPI
    /// reports window extents as the client area; matching against client bounds
    /// gives the highest overlap score for the correct window.
    fn active_bounds(&mut self) -> Option<(i32, i32, i32, i32)> {
        let win = self.active_window().ok()??;
        self.client_geometry(win).ok()
    }

    /// Build and show the element-hint overlay from `pairs` returned by a
    /// completed hint enumeration.
    fn start_pick_element(
        &mut self,
        pairs: Vec<(Element, String)>,
    ) -> Result<PickElementOverlay> {
        if pairs.is_empty() {
            anyhow::bail!("no elements to show");
        }
        let mut renderer =
            OverlayRenderer::load().context("loading element overlay font")?;
        let fmt = self.image_fmt()?;
        let upload_gc: Gcontext = self.conn.generate_id()?;
        self.conn.create_gc(upload_gc, self.root, &CreateGCAux::new())?;

        let screen = &self.conn.setup().roots[self.screen_num];
        let screen_size = (screen.width_in_pixels as i32, screen.height_in_pixels as i32);
        let depth = screen.root_depth;

        let mut placed: Vec<(i32, i32, i32, i32)> = Vec::new();
        let mut entries: Vec<ElementEntry> = Vec::new();

        for (element, hint) in &pairs {
            let img = renderer.render_element_hint(&hint.to_uppercase());
            let pixmap = self.upload_image(&fmt, upload_gc, depth, &img)?;
            let size = (img.width as i32, img.height as i32);
            let (px, py) = place_hint((element.x, element.y), size, &placed, screen_size);
            placed.push((px, py, size.0, size.1));

            let overlay: Window = self.conn.generate_id()?;
            self.conn.create_window(
                0u8,
                overlay,
                self.root,
                px as i16,
                py as i16,
                img.width,
                img.height,
                0u16,
                WindowClass::INPUT_OUTPUT,
                0u32,
                &CreateWindowAux::new()
                    .background_pixmap(pixmap)
                    .override_redirect(1u32),
            )?;
            self.conn.map_window(overlay)?;
            entries.push(ElementEntry { overlay, pixmap });
        }
        self.conn.free_gc(upload_gc)?;
        self.conn.flush()?;
        Ok(PickElementOverlay { entries })
    }

    /// Show/hide element chips according to `matched[i]` (parallel to `entries`).
    fn update_pick_element_visibility(
        &self,
        fe: &PickElementOverlay,
        matched: &[bool],
    ) -> Result<()> {
        for (entry, &visible) in fe.entries.iter().zip(matched.iter()) {
            if visible {
                self.conn.map_window(entry.overlay)?;
                self.conn.clear_area(false, entry.overlay, 0, 0, 0, 0)?;
            } else {
                self.conn.unmap_window(entry.overlay)?;
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Destroy all element-hint overlay windows and free their pixmaps.
    fn destroy_pick_element(&self, fe: PickElementOverlay) {
        for entry in &fe.entries {
            let _ = self.conn.destroy_window(entry.overlay);
            let _ = self.conn.free_pixmap(entry.pixmap);
        }
        let _ = self.conn.flush();
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
    let mut pick_window: Option<PickWindowOverlay> = None;
    let mut pick_element: Option<PickElementOverlay> = None;
    // Channel populated while AT-SPI hint enumeration is in flight.
    let mut hints_rx: Option<Receiver<Vec<Element>>> = None;
    // Standalone hint session active when pick-element was triggered.
    let mut hint_session: Option<HintSession> = None;
    // When a tap-hold key is held undecided, wait only until its timeout so the
    // hold modifier can be committed even if no other key follows.
    let mut hold_deadline: Option<Instant> = None;
    loop {
        // Deadlines: hold commit, hints poll (50 ms), and an always-on
        // config-reload poll so editing the config applies live even while idle
        // (no key press needed to wake up).
        let hints_deadline: Option<Instant> = hints_rx
            .as_ref()
            .map(|_| Instant::now() + Duration::from_millis(50));
        let reload_deadline = Instant::now() + Duration::from_secs(1);
        let combined_deadline = [hold_deadline, hints_deadline]
            .into_iter()
            .flatten()
            .chain(std::iter::once(reload_deadline))
            .min();
        let event = match combined_deadline {
            Some(deadline) => {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(event) => event,
                    Err(RecvTimeoutError::Timeout) => {
                        // Apply a live-reloaded config even while idle so a
                        // config edit takes effect without a key press.
                        apply_pending_reload(&mut engine);
                        // Check if the hold deadline actually expired.
                        if hold_deadline.map(|d| Instant::now() >= d).unwrap_or(false) {
                            if crate::tray::is_enabled() {
                                emit(&mut virtual_device, &engine.flush_pending_hold())?;
                            }
                            hold_deadline = None;
                        }
                        // Check if hints results have arrived.
                        if let Some(ref hr) = hints_rx {
                            if let Ok(elements) = hr.try_recv() {
                                hints_rx = None;
                                let (hs, pairs) = HintSession::new(elements, HINT_CHARS);
                                // Only enter the session when there is something
                                // to pick, so empty results don't trap input.
                                if !pairs.is_empty() {
                                    hint_session = Some(hs);
                                }
                                if pairs.is_empty() {
                                    crate::notify::info("No window elements detected");
                                    // Modifier releases were swallowed while awaiting hints;
                                    // drop any still-held modifiers so none stays stuck down.
                                    emit(&mut virtual_device, &engine.clear_modifiers())?;
                                } else {
                                    match watcher.start_pick_element(pairs) {
                                        Ok(overlay) => pick_element = Some(overlay),
                                        Err(err) => log::warn!("find-element failed: {err:#}"),
                                    }
                                }
                            }
                        }
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
        apply_pending_reload(&mut engine);
        // When paused from the tray, forward every key untouched.
        if !crate::tray::is_enabled() {
            emit_raw(&mut virtual_device, event.code, event.value)?;
            continue;
        }

        // While the pick-window overlay is active, route presses to the hint
        // navigator instead of the remapping engine. Releases are passed through
        // the engine so modifier state (e.g. Hyper) stays consistent; repeats
        // are dropped.
        if pick_window.is_some() {
            if event.value == 2 {
                continue;
            }
            if event.value == 0 {
                if let Some(key) = Key::from_evdev_code(event.code) {
                    emit(&mut virtual_device, &engine.on_event(key, 0, &app))?;
                }
                continue;
            }
            let (keep, activate_target) = {
                let fw = pick_window.as_mut().unwrap();
                match Key::from_evdev_code(event.code) {
                    Some(Key::Esc) => (false, None),
                    Some(Key::Backspace) => {
                        fw.backspace();
                        (true, None)
                    }
                    Some(key) => match key_to_hint_char(key) {
                        Some(ch) => match fw.input(ch) {
                            Some(target) => (false, Some(target)),
                            None => (true, None),
                        },
                        None => (true, None),
                    },
                    None => (true, None),
                }
            };
            if keep {
                if let Some(ref fw) = pick_window {
                    if let Err(err) = watcher.update_pick_window_visibility(fw) {
                        log::warn!("pick-window update failed: {err:#}");
                    }
                }
            } else {
                let fw = pick_window.take().unwrap();
                watcher.destroy_pick_window(fw);
                // Releases here (selection or Esc) routed past the engine, so a
                // modifier held to open the overlay never got its release. Drop
                // any still-held modifiers so it is not left stuck down.
                emit(&mut virtual_device, &engine.clear_modifiers())?;
            }
            if let Some(target) = activate_target {
                if let Err(err) = watcher.activate_window(target) {
                    log::warn!("pick-window activate failed: {err:#}");
                }
            }
            continue;
        }

        // Refresh the active app name on presses; releases and repeats use the
        // cached value to keep X11 round-trips off the hot path.
        if event.value == 1 {
            watcher.poll_focus_events();
            app = watcher.active_app();
        }

        // Another X11 client holds a keyboard grab (e.g. a KVM/VirtualBox VM):
        // bypass all remapping and forward keys raw so the VM receives them
        // unmodified.
        if watcher.keyboard_grabbed || engine.is_pass_through(&app) {
            // Forward raw, but keep the engine's modifier tracking in sync so a
            // modifier released while the grab/pass-through was active is not
            // left "stuck on" in the pressed set once normal remapping resumes.
            if let Some(key) = Key::from_evdev_code(event.code) {
                engine.track_passthrough(key, event.value);
            }
            emit_raw(&mut virtual_device, event.code, event.value)?;
            continue;
        }

        // Standalone hint session: intercept key events when pick-element is
        // active (overlay up, or enumeration still in flight). Both presses and
        // releases are swallowed. `hints_rx.is_some()` covers the brief window
        // between triggering pick-element and the AT-SPI results arriving, so no
        // key slips through to the app before the overlay appears.
        if hint_session.is_some() || hints_rx.is_some() {
            if let Some(key) = Key::from_evdev_code(event.code) {
                if event.value != 0 {
                    // Press or repeat: route to the hint session if it exists.
                    if let Some(hs) = &mut hint_session {
                        match hs.process_key(key) {
                            HintAction::Updated => {
                                if let Some(fe) = pick_element.as_ref() {
                                    let matched: Vec<bool> =
                                        hs.matched_hints().map(|(_, _, m)| m).collect();
                                    if let Err(err) =
                                        watcher.update_pick_element_visibility(fe, &matched)
                                    {
                                        log::warn!("find-element update failed: {err:#}");
                                    }
                                }
                            }
                            HintAction::Activate(elem) => {
                                if let Some(fe) = pick_element.take() {
                                    watcher.destroy_pick_element(fe);
                                }
                                hint_session = None;
                                // Key releases routed past the engine while the
                                // hints were up, so a modifier held to trigger
                                // pick-element never got its release. Drop any
                                // still-held modifiers so none is left stuck down.
                                emit(&mut virtual_device, &engine.clear_modifiers())?;
                                thread::spawn(move || {
                                    let rt = tokio::runtime::Builder::new_current_thread()
                                        .enable_all()
                                        .build()
                                        .expect("tokio runtime for element activation");
                                    if let Err(err) =
                                        rt.block_on(crate::backend::linux::executor::execute(&elem))
                                    {
                                        log::warn!("element activation failed: {err:#}");
                                    }
                                });
                            }
                            HintAction::Dismiss => {
                                if let Some(fe) = pick_element.take() {
                                    watcher.destroy_pick_element(fe);
                                }
                                hint_session = None;
                                // Esc's release (and the trigger modifier's) were
                                // routed past the engine while the hints were up;
                                // drop any still-held modifiers so none stays stuck.
                                emit(&mut virtual_device, &engine.clear_modifiers())?;
                            }
                            HintAction::Suppressed => {}
                        }
                    }
                }
            }
            continue;
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
                    if matches!(&effect, Effect::Window(WindowAction::PickWindow)) {
                        match watcher.start_pick_window() {
                            Ok(overlay) => pick_window = Some(overlay),
                            Err(err) => log::warn!("pick-window failed: {err:#}"),
                        }
                    } else if matches!(&effect, Effect::Window(WindowAction::PickElement)) {
                        let pid = watcher.focused_pid();
                        let bounds = watcher.active_bounds();
                        let (hints_tx, hints_rx_new) = mpsc::channel();
                        hints_rx = Some(hints_rx_new);
                        thread::spawn(move || {
                            let rt = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .expect("tokio runtime for AT-SPI enumeration");
                            let elements = rt
                                .block_on(crate::backend::linux::enumerator::enumerate(pid, bounds))
                                .unwrap_or_else(|e| {
                                    log::warn!("AT-SPI enumeration failed: {e:#}");
                                    Vec::new()
                                });
                            let _ = hints_tx.send(elements);
                        });
                    } else if let Err(err) = watcher.perform_effect(&effect) {
                        log::warn!("effect {effect:?} failed: {err:#}");
                        crate::notify::warn(&format!("{err}"));
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

/// Apply a pending live-reloaded config, if one is ready. Returns `true` when a
/// config was applied.
fn apply_pending_reload(engine: &mut Engine) -> bool {
    let Some(config) = crate::reload::take() else {
        return false;
    };
    engine.set_config(config);
    crate::notify::info("RightKeys reloaded!");
    true
}

fn read_device(mut device: Device, tx: Sender<InEvent>) {
    loop {
        let events = match device.fetch_events() {
            Ok(events) => events,
            // A blocking `read()` returns EINTR whenever a signal (e.g. SIGCHLD
            // from a spawned `notify-send`/`xdotool`, or a tokio/zbus signal)
            // lands on this thread; EAGAIN can surface likewise. Both are
            // transient: retry rather than letting the reader die, which would
            // silently stop every remap until a restart.
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
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

/// Decode a window title from a `_NET_WM_NAME` (UTF-8) or `WM_NAME` (Latin-1,
/// read lossily) property value, trimming whitespace and any trailing NUL.
fn parse_window_name(value: &[u8]) -> String {
    String::from_utf8_lossy(value)
        .trim_end_matches('\0')
        .trim()
        .to_string()
}

/// Serialize a [`TextImage`] into the server's Z-pixmap byte layout: one pixel
/// per `(r, g, b)`, channels placed by `fmt`'s shifts, bytes ordered per the
/// server's endianness, and each row padded to the scanline boundary.
fn encode_image(fmt: &ImageFmt, img: &TextImage) -> Vec<u8> {
    let row_bytes = img.width as usize * fmt.bytes_per_pixel;
    let stride = row_bytes.div_ceil(fmt.scanline_pad_bytes) * fmt.scanline_pad_bytes;
    let mut data = vec![0u8; stride * img.height as usize];
    for y in 0..img.height as usize {
        for x in 0..img.width as usize {
            let (r, g, b) = img.pixels[y * img.width as usize + x];
            let value = ((r as u32) << fmt.r_shift)
                | ((g as u32) << fmt.g_shift)
                | ((b as u32) << fmt.b_shift);
            let off = y * stride + x * fmt.bytes_per_pixel;
            for k in 0..fmt.bytes_per_pixel {
                let shift = if fmt.lsb_first {
                    8 * k
                } else {
                    8 * (fmt.bytes_per_pixel - 1 - k)
                };
                data[off + k] = (value >> shift) as u8;
            }
        }
    }
    data
}

/// Read the bytes of the system sans font, resolving it via `fc-match` and
/// falling back to a few well-known paths.
fn system_font_bytes() -> Result<Vec<u8>> {
    if let Some(path) = fc_match_font() {
        if let Ok(bytes) = std::fs::read(&path) {
            return Ok(bytes);
        }
    }
    for path in FALLBACK_FONTS {
        if let Ok(bytes) = std::fs::read(path) {
            return Ok(bytes);
        }
    }
    Err(anyhow!(
        "no system font found (install fontconfig, or a Noto/DejaVu sans font)"
    ))
}

/// Ask `fc-match` for the file backing the default sans font.
fn fc_match_font() -> Option<PathBuf> {
    let output = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", "sans"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Ask `fc-match` for the file of a font that contains the character `c`, used
/// to render scripts the primary font lacks.
fn fc_match_char(c: char) -> Option<String> {
    let pattern = format!(":charset={:x}", c as u32);
    let output = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", &pattern])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!path.is_empty()).then_some(path)
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_image_fills_and_outlines() {
        let mut img = TextImage::new(4, 3, (1, 2, 3));
        // Filled with the background color.
        assert_eq!(img.pixels[0], (1, 2, 3));
        img.draw_outline(0, 0, 4, 3, (9, 9, 9));
        // Border pixel (corner) set, interior pixel untouched.
        assert_eq!(img.pixels[0], (9, 9, 9));
        assert_eq!(img.pixels[4 + 1], (1, 2, 3));
        // Out-of-bounds writes are ignored, not panics.
        img.set(99, 99, (0, 0, 0));
    }

    #[test]
    fn text_image_blend_interpolates_by_coverage() {
        let mut img = TextImage::new(1, 1, (0, 0, 0));
        img.blend(0, 0, (100, 200, 50), 0.5);
        assert_eq!(img.pixels[0], (50, 100, 25));
        // Zero coverage is a no-op.
        img.blend(0, 0, (255, 255, 255), 0.0);
        assert_eq!(img.pixels[0], (50, 100, 25));
    }

    #[test]
    fn encode_image_packs_bgrx_little_endian() {
        // The common case: 32 bpp, LSB-first, masks R=0xff0000 G=0xff00 B=0xff.
        let fmt = ImageFmt {
            bytes_per_pixel: 4,
            scanline_pad_bytes: 4,
            lsb_first: true,
            r_shift: 16,
            g_shift: 8,
            b_shift: 0,
        };
        let img = TextImage::new(1, 1, (0x12, 0x34, 0x56));
        // Pixel 0x00123456 stored low-byte first: B, G, R, 0.
        assert_eq!(encode_image(&fmt, &img), vec![0x56, 0x34, 0x12, 0x00]);
    }

    #[test]
    fn encode_image_pads_rows_to_scanline() {
        // 3 px wide at 4 bytes each = 12 bytes; padded up to a 8-byte scanline = 16.
        let fmt = ImageFmt {
            bytes_per_pixel: 4,
            scanline_pad_bytes: 8,
            lsb_first: true,
            r_shift: 16,
            g_shift: 8,
            b_shift: 0,
        };
        let img = TextImage::new(3, 2, (0, 0, 0));
        assert_eq!(encode_image(&fmt, &img).len(), 16 * 2);
    }

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
    fn parses_window_name_trims_and_drops_nul() {
        assert_eq!(parse_window_name(b"README.md - Editor\0"), "README.md - Editor");
        assert_eq!(parse_window_name(b"  spaced  "), "spaced");
        assert_eq!(parse_window_name(b""), "");
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
