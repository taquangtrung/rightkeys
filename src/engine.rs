//! The transform engine: the portable, OS-agnostic core that turns an incoming
//! key event (plus the active application name) into a list of [`OutEvent`]s to
//! emit.
//!
//! # Output model
//!
//! The backend grabs the keyboard and emits *only* what the engine returns, so
//! the engine owns all output. Real modifiers are forwarded as they are pressed,
//! so other devices see them (this is what makes `Shift`/`Ctrl` + mouse-click
//! selection work). When a remapped key fires, the engine briefly syncs the OS
//! modifiers to exactly what the target needs, then restores the held set: that
//! is what lets `C-a -> home` emit a bare `Home` while `Ctrl` is held, and lets
//! unbound `Shift-a` still type `A`. The synthetic `Hyper` modifier has no OS
//! key and is never forwarded.

// Imports

use std::collections::{BTreeSet, HashMap};

use regex::Regex;

use crate::key::{Combo, Key, Modifier};

// Data Structures

/// A single low-level event for the backend to emit. `value` is `1` for press
/// and `0` for release.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutEvent {
    pub key: Key,
    pub value: i32,
}

/// A screen corner, for quarter-screen tiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// A screen edge, for half-screen (smart) tiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

/// Direction to step when cycling through windows of the same application.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CycleDirection {
    Forward,
    Backward,
}

impl CycleDirection {
    /// The index reached by stepping one position from `i` in this direction
    /// within a ring of `len` items; wraps around at both ends. `len` must be
    /// non-zero (the caller only cycles when more than one window exists).
    pub fn step(self, i: usize, len: usize) -> usize {
        match self {
            CycleDirection::Forward => (i + 1) % len,
            CycleDirection::Backward => (i + len - 1) % len,
        }
    }
}

/// A workspace / virtual-desktop target. `Index` is 1-based as written in the
/// config (workspace 1 is the first).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Workspace {
    Index(u32),
    Prev,
    Next,
}

/// A window-management action the backend performs on the foreground window.
/// Geometry is in screen pixels; `Preset` fractions are of the window's monitor
/// work area.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WindowAction {
    /// Add to the window's position (`dx`, `dy`) and size (`dw`, `dh`). One
    /// action expresses every move/resize in the old AHK config: e.g. "shorten
    /// at the top" is `dy = +20, dh = -20`.
    Adjust { dx: i32, dy: i32, dw: i32, dh: i32 },
    /// Size the window to a fraction of its monitor's work area and place it at
    /// `anchor` — a corner, or the centre when `None`.
    Preset { w: f64, h: f64, anchor: Option<Corner> },
    /// Centre the window on its monitor, keeping its current size.
    Center,
    /// Move the window to a corner of its monitor, keeping its current size.
    Snap(Corner),
    /// Tile the window to a quarter of its monitor work area.
    Corner(Corner),
    /// Tile the window to a screen edge at `fraction` of the work area. The
    /// engine fills `fraction` from the smart-tile cycle (1/2 → 1/3 → 2/3 on
    /// consecutive same-edge tiles; a broken chain restarts at a half).
    SmartTile { side: Side, fraction: f64 },
    /// Maximize the window.
    Maximize,
    /// Maximize the window if it is restored, restore it if it is maximized.
    MaximizeToggle,
    /// Minimize (iconify) the window.
    Minimize,
    /// Toggle the window's always-on-top (keep-above) state.
    AlwaysOnTop,
    /// Toggle showing the desktop (minimize/restore all windows).
    ShowDesktop,
    /// Move the window to the next/previous monitor, keeping its relative place.
    MoveToMonitor(CycleDirection),
    /// Activate the next/previous window of the same application.
    CycleSameApp(CycleDirection),
    /// Switch to a workspace, optionally taking the active window along.
    Workspace { target: Workspace, move_window: bool },
}

/// A non-keyboard side effect produced by a binding. The portable engine only
/// records these as *intents*; the active backend performs them — mirroring how
/// the engine emits [`OutEvent`]s for the backend to inject rather than touching
/// the OS itself.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Activate an existing window of the program, or launch it if none is open.
    /// The string is an executable name or path (e.g. `brave` or `Code.exe`).
    Launch(String),
    /// Act on the foreground window.
    Window(WindowAction),
}

/// One action in a binding's action list.
#[derive(Clone, Debug)]
pub enum Step {
    /// Emit a combo verbatim.
    Keys(Combo),
    /// Emit a combo, adding `Shift` when the selection mark is active.
    WithMark(Combo),
    /// Set or clear the selection mark.
    SetMark(bool),
    /// Let the original trigger key through unchanged.
    PassThrough,
    /// Activate-or-launch a program. A side effect, not a keystroke.
    Exec(String),
    /// Perform a window-management action. A side effect, not a keystroke.
    Window(WindowAction),
}

/// Selects which application a rule applies to. With neither field set the rule
/// is global.
#[derive(Debug, Default)]
pub struct AppMatcher {
    pub include: Option<Regex>,
    pub exclude: Option<Regex>,
}

/// A keymap: a set of trigger-combo bindings scoped to matching applications.
#[derive(Debug)]
pub struct KeymapRule {
    pub name: String,
    pub matcher: AppMatcher,
    pub bindings: HashMap<Combo, Vec<Step>>,
}

/// A tap-hold rule: tap the key for `tap`, hold it to act as the `hold`
/// modifier. Scoped to matching applications.
#[derive(Debug)]
pub struct MultipurposeRule {
    pub matcher: AppMatcher,
    pub map: HashMap<Key, (Key, Modifier)>,
}

/// The fully lowered configuration the engine runs on.
#[derive(Debug, Default)]
pub struct Config {
    pub modmap: HashMap<Key, Key>,
    pub multipurpose: Vec<MultipurposeRule>,
    pub keymaps: Vec<KeymapRule>,
}

/// A multipurpose key awaiting its tap-vs-hold decision.
#[derive(Clone, Copy, Debug)]
struct Pending {
    src: Key,
    tap: Key,
    hold: Modifier,
    resolved_hold: bool,
}

/// A single-combo binding currently held down, so it repeats naturally.
#[derive(Clone, Debug)]
struct HeldChord {
    target: Key,
    saved: BTreeSet<Modifier>,
}

/// Smart-tile sizes, cycled in order on consecutive same-edge tiles: a half,
/// then a third, then two-thirds. A broken chain restarts at index 0.
const TILE_FRACTIONS: [f64; 3] = [0.5, 1.0 / 3.0, 2.0 / 3.0];

/// The stateful remapping engine.
#[derive(Debug)]
pub struct Engine {
    config: Config,
    pressed_mods: BTreeSet<Modifier>,
    output_mods: BTreeSet<Modifier>,
    passthrough_down: BTreeSet<Key>,
    held: HashMap<Key, HeldChord>,
    /// Window-move bindings held down, re-fired on each auto-repeat so holding a
    /// move key glides the window instead of nudging it once per physical tap.
    held_window: HashMap<Key, WindowAction>,
    pending: Option<Pending>,
    mark_set: bool,
    /// The active smart-tile chain (edge + index into [`TILE_FRACTIONS`]). Set
    /// while consecutive tiles target the same edge; cleared when any other
    /// action breaks the chain so the next tile restarts at a half.
    tile_chain: Option<(Side, usize)>,
    /// Side effects produced since the last [`Engine::take_effects`] drain.
    effects: Vec<Effect>,
}

// === AppMatcher ===

impl AppMatcher {
    /// Whether this matcher applies to the given application name.
    pub fn matches(&self, app: &str) -> bool {
        if let Some(include) = &self.include {
            if !include.is_match(app) {
                return false;
            }
        }
        if let Some(exclude) = &self.exclude {
            if exclude.is_match(app) {
                return false;
            }
        }
        true
    }
}

// === Engine ===

impl Engine {
    /// Create an engine over a lowered configuration.
    pub fn new(config: Config) -> Self {
        Engine {
            config,
            pressed_mods: BTreeSet::new(),
            output_mods: BTreeSet::new(),
            passthrough_down: BTreeSet::new(),
            held: HashMap::new(),
            held_window: HashMap::new(),
            pending: None,
            mark_set: false,
            tile_chain: None,
            effects: Vec::new(),
        }
    }

    /// Drain the side effects accumulated since the last call. The backend calls
    /// this right after each [`Engine::on_event`] and performs them (launching
    /// programs, moving windows); the portable engine never performs effects.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    /// Replace the active configuration (live reload). Transient state (held
    /// keys, pressed modifiers, the selection mark) is preserved so a reload
    /// never strands a key that is down.
    pub fn set_config(&mut self, config: Config) {
        self.config = config;
    }

    /// Process one incoming key event for the active `app` and return the events
    /// the backend should emit. `value` is `1` press, `0` release, `2` repeat.
    pub fn on_event(&mut self, raw: Key, value: i32, app: &str) -> Vec<OutEvent> {
        let mut out = Vec::new();
        let is_press = value == 1;
        let is_release = value == 0;

        // A multipurpose key's release resolves the pending tap-vs-hold.
        if is_release && self.pending.is_some_and(|p| p.src == raw) {
            let pending = self.pending.take().expect("just checked");
            if pending.resolved_hold {
                self.pressed_mods.remove(&pending.hold);
                let desired = real_only(&self.pressed_mods);
                self.sync_mods(&desired, &mut out);
            } else {
                let combo = Combo {
                    modifiers: self.pressed_mods.clone(),
                    key: pending.tap,
                };
                self.emit_combo(&combo, false, &mut out);
            }
            return out;
        }

        // A multipurpose key's first press starts a pending decision.
        if is_press {
            if let Some((tap, hold)) = self.multipurpose_lookup(app, raw) {
                self.pending = Some(Pending {
                    src: raw,
                    tap,
                    hold,
                    resolved_hold: false,
                });
                return out;
            }
        }

        let key = self.config.modmap.get(&raw).copied().unwrap_or(raw);

        // Modifier keys are tracked, and real ones are forwarded as they happen.
        if let Some(modifier) = key.as_modifier() {
            if is_release {
                self.pressed_mods.remove(&modifier);
                let desired = real_only(&self.pressed_mods);
                self.sync_mods(&desired, &mut out);
            } else {
                self.resolve_pending_hold();
                self.pressed_mods.insert(modifier);
                // Forward it now so other devices (the mouse, for Shift/Ctrl-click
                // selection) see it; `real_only` keeps synthetic `Hyper` suppressed.
                let desired = real_only(&self.pressed_mods);
                self.sync_mods(&desired, &mut out);
            }
            return out;
        }

        if is_release {
            if let Some(chord) = self.held.remove(&key) {
                out.push(OutEvent {
                    key: chord.target,
                    value: 0,
                });
                self.sync_mods(&chord.saved, &mut out);
            } else if self.passthrough_down.remove(&key) {
                out.push(OutEvent { key, value: 0 });
            } else {
                // A held window-move binding emits no key event; just stop repeating.
                self.held_window.remove(&key);
            }
            return out;
        }

        self.resolve_pending_hold();

        // Auto-repeat: forward as a repeat of whatever the key drives, so a held
        // key repeats naturally instead of being re-tapped on every kernel repeat.
        if value == 2 {
            if let Some(chord) = self.held.get(&key) {
                out.push(OutEvent {
                    key: chord.target,
                    value: 2,
                });
            } else if let Some(action) = self.held_window.get(&key) {
                self.effects.push(Effect::Window(*action));
            } else if self.passthrough_down.contains(&key) {
                out.push(OutEvent { key, value: 2 });
            }
            return out;
        }

        // Initial press of an ordinary key.
        let combo = Combo {
            modifiers: self.pressed_mods.clone(),
            key,
        };
        match self.lookup(app, &combo).cloned() {
            Some(steps) => self.press_binding(key, &steps, &combo, &mut out),
            None => {
                self.tile_chain = None; // an unbound key press breaks the chain
                let desired = real_only(&self.pressed_mods);
                self.sync_mods(&desired, &mut out);
                out.push(OutEvent { key, value: 1 });
                self.passthrough_down.insert(key);
            }
        }
        out
    }

    /// Handle the initial press of a bound key. A single combo is held down so it
    /// repeats naturally; multi-step sequences, mark toggles, and pass-through are
    /// one-shots. The exception is a single combo whose target carries `Super`: it
    /// drives a window-manager shortcut, so it is emitted one-shot to avoid
    /// leaving modifiers down while the shortcut runs.
    fn press_binding(
        &mut self,
        phys: Key,
        steps: &[Step],
        trigger: &Combo,
        out: &mut Vec<OutEvent>,
    ) {
        // A lone relative window move repeats while held, like a single combo, so
        // holding the key glides the window. Absolute actions (snap, center,
        // maximize) are one-shots: repeating them is a no-op or flickers.
        if let [Step::Window(action @ WindowAction::Adjust { .. })] = steps {
            self.tile_chain = None; // a move breaks the smart-tile chain
            self.effects.push(Effect::Window(*action));
            self.held_window.insert(phys, *action);
            return;
        }
        let single = match steps {
            [Step::Keys(target)] => Some((target, false)),
            [Step::WithMark(target)] => Some((target, true)),
            _ => None,
        };
        if let Some((target, mark_aware)) = single {
            self.tile_chain = None; // a key remap breaks the smart-tile chain
            let add_shift = mark_aware && self.mark_set;
            if target.modifiers.contains(&Modifier::Super) {
                self.emit_combo(target, add_shift, out);
                return;
            }
            let saved = self.output_mods.clone();
            let mut desired = real_only(&target.modifiers);
            if add_shift {
                desired.insert(Modifier::Shift);
            }
            self.sync_mods(&desired, out);
            out.push(OutEvent {
                key: target.key,
                value: 1,
            });
            self.held.insert(
                phys,
                HeldChord {
                    target: target.key,
                    saved,
                },
            );
        } else {
            self.run_steps(steps, trigger, out);
        }
    }

    /// Emit the events that make the OS modifier state match the held physical
    /// modifiers. The backend calls this before forwarding a key absent from the
    /// key table, so held modifiers are not lost.
    pub fn sync_modifiers(&mut self) -> Vec<OutEvent> {
        let desired = real_only(&self.pressed_mods);
        let mut out = Vec::new();
        self.sync_mods(&desired, &mut out);
        out
    }

    /// Whether a tap-hold key is currently held with its tap-vs-hold decision
    /// still undecided. The backend uses this to arm a timeout so a long hold
    /// commits to its modifier even when no other key follows.
    pub fn has_pending_hold(&self) -> bool {
        self.pending.is_some_and(|p| !p.resolved_hold)
    }

    /// Commit a still-pending tap-hold key to its hold modifier and forward it to
    /// the OS. The backend calls this when the key has been held past the timeout,
    /// so the modifier is live for a mouse click that never reaches the engine.
    /// Returns the events to emit, empty when nothing is pending.
    pub fn flush_pending_hold(&mut self) -> Vec<OutEvent> {
        let mut out = Vec::new();
        if self.has_pending_hold() {
            self.resolve_pending_hold();
            let desired = real_only(&self.pressed_mods);
            self.sync_mods(&desired, &mut out);
        }
        out
    }

    /// Promote a still-undecided pending multipurpose key to its hold meaning,
    /// because another key arrived while it was held.
    fn resolve_pending_hold(&mut self) {
        if let Some(pending) = self.pending.as_mut() {
            if !pending.resolved_hold {
                pending.resolved_hold = true;
                let hold = pending.hold;
                self.pressed_mods.insert(hold);
            }
        }
    }

    fn multipurpose_lookup(&self, app: &str, raw: Key) -> Option<(Key, Modifier)> {
        for rule in &self.config.multipurpose {
            if rule.matcher.matches(app) {
                if let Some(target) = rule.map.get(&raw) {
                    return Some(*target);
                }
            }
        }
        None
    }

    fn lookup(&self, app: &str, combo: &Combo) -> Option<&Vec<Step>> {
        for keymap in &self.config.keymaps {
            if keymap.matcher.matches(app) {
                if let Some(steps) = keymap.bindings.get(combo) {
                    log::trace!("matched {combo:?} in keymap {:?}", keymap.name);
                    return Some(steps);
                }
            }
        }
        None
    }

    fn run_steps(&mut self, steps: &[Step], trigger: &Combo, out: &mut Vec<OutEvent>) {
        // Any binding except another same-edge smart-tile breaks the tile chain;
        // take it now and let a smart-tile step below restore/advance it.
        let prev_tile = self.tile_chain.take();
        for step in steps {
            match step {
                Step::Keys(combo) => self.emit_combo(combo, false, out),
                Step::WithMark(combo) => {
                    let mark = self.mark_set;
                    self.emit_combo(combo, mark, out);
                }
                Step::SetMark(value) => self.mark_set = *value,
                Step::PassThrough => {
                    let combo = Combo {
                        modifiers: self.pressed_mods.clone(),
                        key: trigger.key,
                    };
                    self.emit_combo(&combo, false, out);
                }
                // Side effects emit no key events; the backend performs them
                // after draining `take_effects`.
                Step::Exec(program) => self.effects.push(Effect::Launch(program.clone())),
                // Smart-tile advances the cycle when the previous action tiled
                // the same edge, else restarts at a half; the engine fills in the
                // fraction so the backend stays a stateless geometry applier.
                Step::Window(WindowAction::SmartTile { side, .. }) => {
                    let index = match prev_tile {
                        Some((edge, i)) if edge == *side => (i + 1) % TILE_FRACTIONS.len(),
                        _ => 0,
                    };
                    self.tile_chain = Some((*side, index));
                    self.effects.push(Effect::Window(WindowAction::SmartTile {
                        side: *side,
                        fraction: TILE_FRACTIONS[index],
                    }));
                }
                Step::Window(action) => self.effects.push(Effect::Window(*action)),
            }
        }
    }

    fn emit_combo(&mut self, combo: &Combo, add_shift: bool, out: &mut Vec<OutEvent>) {
        // Emit a self-contained chord: set exactly the modifiers this combo needs,
        // tap the key, then restore the prior modifier state. Keeping the whole
        // sequence tight is what lets window-manager global-shortcut grabs
        // recognise the chord.
        let saved = self.output_mods.clone();
        let mut desired = real_only(&combo.modifiers);
        if add_shift {
            desired.insert(Modifier::Shift);
        }
        self.sync_mods(&desired, out);
        out.push(OutEvent {
            key: combo.key,
            value: 1,
        });
        out.push(OutEvent {
            key: combo.key,
            value: 0,
        });
        self.sync_mods(&saved, out);
    }

    /// Emit the press/release events that move the OS modifier state from its
    /// current value to `desired` (which must contain only emittable modifiers).
    fn sync_mods(&mut self, desired: &BTreeSet<Modifier>, out: &mut Vec<OutEvent>) {
        let to_release: Vec<Modifier> = self.output_mods.difference(desired).copied().collect();
        for modifier in to_release {
            if let Some(key) = modifier.emit_key() {
                out.push(OutEvent { key, value: 0 });
            }
            self.output_mods.remove(&modifier);
        }
        let to_press: Vec<Modifier> = desired.difference(&self.output_mods).copied().collect();
        for modifier in to_press {
            if let Some(key) = modifier.emit_key() {
                out.push(OutEvent { key, value: 1 });
            }
            self.output_mods.insert(modifier);
        }
    }
}

// Functions

/// Keep only modifiers that the OS can actually be sent (drops synthetic
/// `Hyper`).
fn real_only(mods: &BTreeSet<Modifier>) -> BTreeSet<Modifier> {
    mods.iter()
        .copied()
        .filter(|m| m.emit_key().is_some())
        .collect()
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn keymap(name: &str, matcher: AppMatcher, binds: Vec<(&str, Vec<Step>)>) -> KeymapRule {
        let bindings = binds
            .into_iter()
            .map(|(combo, steps)| (Combo::parse(combo).unwrap(), steps))
            .collect();
        KeymapRule {
            name: name.to_string(),
            matcher,
            bindings,
        }
    }

    fn press(key: Key) -> OutEvent {
        OutEvent { key, value: 1 }
    }

    fn release(key: Key) -> OutEvent {
        OutEvent { key, value: 0 }
    }

    #[test]
    fn smart_tile_cycles_per_edge_and_resets_on_break() {
        let tile = |side| Step::Window(WindowAction::SmartTile { side, fraction: 0.5 });
        let config = Config {
            keymaps: vec![keymap(
                "g",
                AppMatcher::default(),
                vec![
                    ("h", vec![tile(Side::Left)]),
                    ("l", vec![tile(Side::Right)]),
                    ("0", vec![Step::Window(WindowAction::Maximize)]),
                ],
            )],
            ..Config::default()
        };
        let mut engine = Engine::new(config);

        // Tap a key and return the fraction of the single SmartTile effect it produced.
        fn tap_tile(engine: &mut Engine, key: Key) -> f64 {
            engine.on_event(key, 1, "");
            engine.on_event(key, 0, "");
            match engine.take_effects().as_slice() {
                [Effect::Window(WindowAction::SmartTile { fraction, .. })] => *fraction,
                other => panic!("expected one SmartTile effect, got {other:?}"),
            }
        }

        // Consecutive same-edge tiles cycle 1/2 → 1/3 → 2/3 → 1/2.
        assert_eq!(tap_tile(&mut engine, Key::H), 0.5);
        assert_eq!(tap_tile(&mut engine, Key::H), 1.0 / 3.0);
        assert_eq!(tap_tile(&mut engine, Key::H), 2.0 / 3.0);
        assert_eq!(tap_tile(&mut engine, Key::H), 0.5);

        // Switching edge restarts at a half.
        assert_eq!(tap_tile(&mut engine, Key::H), 1.0 / 3.0);
        assert_eq!(tap_tile(&mut engine, Key::L), 0.5);

        // Another action between tiles breaks the chain: the next tile is a half.
        assert_eq!(tap_tile(&mut engine, Key::L), 1.0 / 3.0);
        engine.on_event(Key::Num0, 1, "");
        engine.on_event(Key::Num0, 0, "");
        engine.take_effects();
        assert_eq!(tap_tile(&mut engine, Key::L), 0.5);
    }

    #[test]
    fn passes_through_unbound_keys() {
        let mut engine = Engine::new(Config::default());
        assert_eq!(engine.on_event(Key::A, 1, ""), vec![press(Key::A)]);
        assert_eq!(engine.on_event(Key::A, 0, ""), vec![release(Key::A)]);
    }

    #[test]
    fn remaps_bound_combo_releasing_held_modifier() {
        let config = Config {
            keymaps: vec![keymap(
                "global",
                AppMatcher::default(),
                vec![("C+a", vec![Step::Keys(Combo::parse("home").unwrap())])],
            )],
            ..Config::default()
        };
        let mut engine = Engine::new(config);
        // Ctrl is forwarded eagerly so the OS sees it while held.
        assert_eq!(
            engine.on_event(Key::LeftCtrl, 1, ""),
            vec![press(Key::LeftCtrl)]
        );
        // C-a is bound to Home, which needs no modifiers: Ctrl is released around
        // the Home press, and restored when the held key is lifted.
        assert_eq!(
            engine.on_event(Key::A, 1, ""),
            vec![release(Key::LeftCtrl), press(Key::Home)]
        );
        assert_eq!(
            engine.on_event(Key::A, 0, ""),
            vec![release(Key::Home), press(Key::LeftCtrl)]
        );
    }

    #[test]
    fn single_combo_binding_holds_and_repeats() {
        let config = Config {
            keymaps: vec![keymap(
                "browser",
                AppMatcher::default(),
                vec![("M+S+j", vec![Step::Keys(Combo::parse("down").unwrap())])],
            )],
            ..Config::default()
        };
        let mut engine = Engine::new(config);
        // Both modifiers are forwarded eagerly.
        assert_eq!(
            engine.on_event(Key::LeftAlt, 1, ""),
            vec![press(Key::LeftAlt)]
        );
        assert_eq!(
            engine.on_event(Key::LeftShift, 1, ""),
            vec![press(Key::LeftShift)]
        );
        // M-S-j is bound to bare Down: the held modifiers are released around the
        // Down press, held for repeats, and restored when the key is lifted.
        assert_eq!(
            engine.on_event(Key::J, 1, ""),
            vec![
                release(Key::LeftAlt),
                release(Key::LeftShift),
                press(Key::Down)
            ]
        );
        assert_eq!(
            engine.on_event(Key::J, 2, ""),
            vec![OutEvent {
                key: Key::Down,
                value: 2
            }]
        );
        assert_eq!(
            engine.on_event(Key::J, 0, ""),
            vec![
                release(Key::Down),
                press(Key::LeftAlt),
                press(Key::LeftShift)
            ]
        );
    }

    #[test]
    fn shift_passthrough_syncs_modifier() {
        let mut engine = Engine::new(Config::default());
        // Shift is forwarded on its own press, so an unbound Shift-a rides the
        // already-live modifier state and only adds the `a`.
        assert_eq!(
            engine.on_event(Key::LeftShift, 1, ""),
            vec![press(Key::LeftShift)]
        );
        let out = engine.on_event(Key::A, 1, "");
        assert_eq!(out, vec![press(Key::A)]);
    }

    #[test]
    fn mark_to_binding_holds_and_repeats() {
        let config = Config {
            keymaps: vec![keymap(
                "g",
                AppMatcher::default(),
                vec![("C+f", vec![Step::WithMark(Combo::parse("right").unwrap())])],
            )],
            ..Config::default()
        };
        let mut engine = Engine::new(config);
        assert_eq!(
            engine.on_event(Key::LeftCtrl, 1, ""),
            vec![press(Key::LeftCtrl)]
        );
        // Mark off: plain Right, held down so it auto-repeats. Ctrl is released
        // around the press and restored on release.
        assert_eq!(
            engine.on_event(Key::F, 1, ""),
            vec![release(Key::LeftCtrl), press(Key::Right)]
        );
        assert_eq!(
            engine.on_event(Key::F, 2, ""),
            vec![OutEvent {
                key: Key::Right,
                value: 2
            }]
        );
        assert_eq!(
            engine.on_event(Key::F, 0, ""),
            vec![release(Key::Right), press(Key::LeftCtrl)]
        );
    }

    #[test]
    fn mark_to_extends_selection_when_marked() {
        let config = Config {
            keymaps: vec![keymap(
                "g",
                AppMatcher::default(),
                vec![
                    ("C+space", vec![Step::SetMark(true)]),
                    ("C+f", vec![Step::WithMark(Combo::parse("right").unwrap())]),
                ],
            )],
            ..Config::default()
        };
        let mut engine = Engine::new(config);
        // Turn the mark on, then C-f holds Shift+Right (extends selection). Ctrl
        // is forwarded eagerly, then swapped for Shift around the Right press.
        assert_eq!(
            engine.on_event(Key::LeftCtrl, 1, ""),
            vec![press(Key::LeftCtrl)]
        );
        engine.on_event(Key::Space, 1, "");
        engine.on_event(Key::Space, 0, "");
        assert_eq!(
            engine.on_event(Key::F, 1, ""),
            vec![
                release(Key::LeftCtrl),
                press(Key::LeftShift),
                press(Key::Right)
            ]
        );
    }

    #[test]
    fn sync_modifiers_is_noop_once_modifier_is_live() {
        // The backend still asks the engine to reassert modifiers before
        // forwarding a key absent from the table. With eager forwarding the
        // modifier is already down, so this is a no-op rather than a re-emit.
        let mut engine = Engine::new(Config::default());
        assert_eq!(
            engine.on_event(Key::LeftShift, 1, ""),
            vec![press(Key::LeftShift)]
        );
        assert!(engine.sync_modifiers().is_empty());
    }

    #[test]
    fn held_modifier_reaches_os_for_mouse_combos() {
        // Regression: a bare held modifier must be forwarded to the OS so a
        // different device (the mouse) sees it, e.g. Shift/Ctrl-click
        // multi-select. The engine never sees the click; what matters is that
        // pressing the modifier emits it and keeps it down, then releasing lifts
        // exactly what was added.
        let mut engine = Engine::new(Config::default());
        assert_eq!(
            engine.on_event(Key::LeftCtrl, 1, ""),
            vec![press(Key::LeftCtrl)]
        );
        assert_eq!(
            engine.on_event(Key::LeftShift, 1, ""),
            vec![press(Key::LeftShift)]
        );
        assert_eq!(
            engine.on_event(Key::LeftShift, 0, ""),
            vec![release(Key::LeftShift)]
        );
        assert_eq!(
            engine.on_event(Key::LeftCtrl, 0, ""),
            vec![release(Key::LeftCtrl)]
        );
    }

    #[test]
    fn synthetic_hyper_is_never_forwarded() {
        // Eager forwarding must not leak the internal-only Hyper modifier: it has
        // no OS key, so its press and release emit nothing.
        let config = Config {
            modmap: HashMap::from([(Key::CapsLock, Key::LeftHyper)]),
            ..Config::default()
        };
        let mut engine = Engine::new(config);
        assert!(engine.on_event(Key::CapsLock, 1, "").is_empty());
        assert!(engine.on_event(Key::CapsLock, 0, "").is_empty());
    }

    #[test]
    fn modmap_capslock_to_hyper_is_internal_only() {
        let config = Config {
            modmap: HashMap::from([(Key::CapsLock, Key::LeftHyper)]),
            keymaps: vec![keymap(
                "global",
                AppMatcher::default(),
                vec![(
                    "Hyper+left",
                    vec![Step::Keys(Combo::parse("s+left").unwrap())],
                )],
            )],
            ..Config::default()
        };
        let mut engine = Engine::new(config);
        // CapsLock press maps to Hyper: tracked, nothing emitted.
        assert!(engine.on_event(Key::CapsLock, 1, "").is_empty());
        // The target carries Super, so the chord is emitted one-shot: Super is
        // pressed, Left tapped, and Super released, all on the press. This keeps
        // the modifier state clean for window-manager shortcuts that synthesise
        // further keys. The physical release then emits nothing.
        assert_eq!(
            engine.on_event(Key::Left, 1, ""),
            vec![
                press(Key::LeftMeta),
                press(Key::Left),
                release(Key::Left),
                release(Key::LeftMeta)
            ]
        );
        assert!(engine.on_event(Key::Left, 0, "").is_empty());
    }

    #[test]
    fn super_chord_emits_one_shot_not_held() {
        // A WM-shortcut chord (Hyper+h -> C+M+s+S+h) must release its modifiers
        // immediately, so a shortcut the window manager fires off it sees a clean
        // modifier state. The chord is pressed and released on the press event,
        // and the physical release emits nothing.
        let config = Config {
            modmap: HashMap::from([(Key::CapsLock, Key::LeftHyper)]),
            keymaps: vec![keymap(
                "global",
                AppMatcher::default(),
                vec![(
                    "Hyper+h",
                    vec![Step::Keys(Combo::parse("C+M+s+S+h").unwrap())],
                )],
            )],
            ..Config::default()
        };
        let mut engine = Engine::new(config);
        assert!(engine.on_event(Key::CapsLock, 1, "").is_empty());
        assert_eq!(
            engine.on_event(Key::H, 1, ""),
            vec![
                press(Key::LeftAlt),
                press(Key::LeftCtrl),
                press(Key::LeftShift),
                press(Key::LeftMeta),
                press(Key::H),
                release(Key::H),
                release(Key::LeftAlt),
                release(Key::LeftCtrl),
                release(Key::LeftShift),
                release(Key::LeftMeta),
            ]
        );
        assert!(engine.on_event(Key::H, 0, "").is_empty());
    }

    #[test]
    fn non_super_chord_still_holds_for_repeat() {
        // A modifier-bearing target without Super (e.g. mark-to C-left) keeps the
        // held-chord behaviour so holding the key auto-repeats word-left.
        let config = Config {
            keymaps: vec![keymap(
                "global",
                AppMatcher::default(),
                vec![("M+b", vec![Step::Keys(Combo::parse("C+left").unwrap())])],
            )],
            ..Config::default()
        };
        let mut engine = Engine::new(config);
        assert_eq!(
            engine.on_event(Key::LeftAlt, 1, ""),
            vec![press(Key::LeftAlt)]
        );
        // M-b is bound to C-left: Alt is swapped for Ctrl around the held Left,
        // and Alt is restored when the key is released.
        assert_eq!(
            engine.on_event(Key::B, 1, ""),
            vec![
                release(Key::LeftAlt),
                press(Key::LeftCtrl),
                press(Key::Left)
            ]
        );
        assert_eq!(
            engine.on_event(Key::B, 2, ""),
            vec![OutEvent {
                key: Key::Left,
                value: 2
            }]
        );
        assert_eq!(
            engine.on_event(Key::B, 0, ""),
            vec![
                release(Key::Left),
                release(Key::LeftCtrl),
                press(Key::LeftAlt)
            ]
        );
    }

    #[test]
    fn tap_hold_taps_when_released_alone() {
        let config = Config {
            multipurpose: vec![MultipurposeRule {
                matcher: AppMatcher::default(),
                map: HashMap::from([(Key::LeftAlt, (Key::Esc, Modifier::Alt))]),
            }],
            ..Config::default()
        };
        let mut engine = Engine::new(config);
        assert!(engine.on_event(Key::LeftAlt, 1, "").is_empty());
        let out = engine.on_event(Key::LeftAlt, 0, "");
        assert_eq!(out, vec![press(Key::Esc), release(Key::Esc)]);
    }

    #[test]
    fn tap_hold_holds_when_another_key_pressed() {
        let config = Config {
            multipurpose: vec![MultipurposeRule {
                matcher: AppMatcher::default(),
                map: HashMap::from([(Key::LeftAlt, (Key::Esc, Modifier::Alt))]),
            }],
            ..Config::default()
        };
        let mut engine = Engine::new(config);
        assert!(engine.on_event(Key::LeftAlt, 1, "").is_empty());
        // Pressing X while Alt is held resolves Alt to its hold meaning.
        let out = engine.on_event(Key::X, 1, "");
        assert_eq!(out, vec![press(Key::LeftAlt), press(Key::X)]);
    }

    #[test]
    fn tap_hold_timeout_commits_to_modifier() {
        // A tap-hold key held past the timeout with no other key must commit to
        // its hold modifier, so a mouse click (which never reaches the engine)
        // still sees it.
        let config = Config {
            multipurpose: vec![MultipurposeRule {
                matcher: AppMatcher::default(),
                map: HashMap::from([(Key::LeftAlt, (Key::Esc, Modifier::Alt))]),
            }],
            ..Config::default()
        };
        let mut engine = Engine::new(config);
        assert!(engine.on_event(Key::LeftAlt, 1, "").is_empty());
        assert!(engine.has_pending_hold());
        // The timeout commits the hold and forwards Alt to the OS.
        assert_eq!(engine.flush_pending_hold(), vec![press(Key::LeftAlt)]);
        assert!(!engine.has_pending_hold());
        // A second flush is a no-op; releasing the key lifts the modifier.
        assert!(engine.flush_pending_hold().is_empty());
        assert_eq!(
            engine.on_event(Key::LeftAlt, 0, ""),
            vec![release(Key::LeftAlt)]
        );
    }

    #[test]
    fn flush_pending_hold_is_noop_without_pending() {
        let mut engine = Engine::new(Config::default());
        assert!(!engine.has_pending_hold());
        assert!(engine.flush_pending_hold().is_empty());
    }

    #[test]
    fn exec_step_records_launch_effect_and_emits_no_keys() {
        let config = Config {
            keymaps: vec![keymap(
                "g",
                AppMatcher::default(),
                vec![("b", vec![Step::Exec("brave".to_string())])],
            )],
            ..Config::default()
        };
        let mut engine = Engine::new(config);
        // The trigger key is swallowed (no keystroke), and a Launch effect is queued.
        assert!(engine.on_event(Key::B, 1, "").is_empty());
        assert_eq!(engine.take_effects(), vec![Effect::Launch("brave".to_string())]);
        // Effects are drained: a second take is empty, and the release emits nothing.
        assert!(engine.take_effects().is_empty());
        assert!(engine.on_event(Key::B, 0, "").is_empty());
    }

    #[test]
    fn window_step_records_window_effect() {
        let config = Config {
            keymaps: vec![keymap(
                "g",
                AppMatcher::default(),
                vec![("0", vec![Step::Window(WindowAction::Maximize)])],
            )],
            ..Config::default()
        };
        let mut engine = Engine::new(config);
        assert!(engine.on_event(Key::Num0, 1, "").is_empty());
        assert_eq!(
            engine.take_effects(),
            vec![Effect::Window(WindowAction::Maximize)]
        );
    }

    #[test]
    fn window_adjust_repeats_while_held() {
        let adjust = WindowAction::Adjust {
            dx: 20,
            dy: 0,
            dw: 0,
            dh: 0,
        };
        let config = Config {
            keymaps: vec![keymap(
                "g",
                AppMatcher::default(),
                vec![("l", vec![Step::Window(adjust)])],
            )],
            ..Config::default()
        };
        let mut engine = Engine::new(config);
        // Press and each auto-repeat queue another move; the key emits no keystroke.
        assert!(engine.on_event(Key::L, 1, "").is_empty());
        assert_eq!(engine.take_effects(), vec![Effect::Window(adjust)]);
        assert!(engine.on_event(Key::L, 2, "").is_empty());
        assert!(engine.on_event(Key::L, 2, "").is_empty());
        assert_eq!(
            engine.take_effects(),
            vec![Effect::Window(adjust), Effect::Window(adjust)]
        );
        // Release stops the repeat: a further (stray) repeat queues nothing.
        assert!(engine.on_event(Key::L, 0, "").is_empty());
        assert!(engine.on_event(Key::L, 2, "").is_empty());
        assert!(engine.take_effects().is_empty());
    }

    #[test]
    fn cycle_direction_steps_and_wraps() {
        // len 3: forward advances, wrapping past the end; backward retreats,
        // wrapping past the start.
        assert_eq!(CycleDirection::Forward.step(0, 3), 1);
        assert_eq!(CycleDirection::Forward.step(2, 3), 0);
        assert_eq!(CycleDirection::Backward.step(2, 3), 1);
        assert_eq!(CycleDirection::Backward.step(0, 3), 2);
    }
}
