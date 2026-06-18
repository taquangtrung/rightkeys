//! KDL configuration: parse a config file with the `kdl` crate and lower it into
//! the engine's runtime [`Config`].
//!
//! The on-disk schema uses no boolean literals (string values such as
//! `selection "clear"` stand in) so it parses identically under either KDL dialect.

// Imports

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use kdl::{KdlDocument, KdlNode};
use regex::Regex;

use crate::engine::{
    AppMatcher, Config, Corner, CycleDirection, KeymapRule, MultipurposeRule, Side, Step,
    WindowAction, Workspace,
};
use crate::key::{Combo, Key};

// Constants

const NO_NODES: &[KdlNode] = &[];

// Functions

/// Read and lower a KDL config file into a runtime [`Config`].
pub fn load(path: &Path) -> Result<Config> {
    let text =
        fs::read_to_string(path).with_context(|| format!("reading config {}", path.display()))?;
    let doc: KdlDocument = text
        .parse()
        .map_err(|e: kdl::KdlError| anyhow!("parsing {}:\n{e}", path.display()))?;
    lower(&doc)
}

fn lower(doc: &KdlDocument) -> Result<Config> {
    let mut config = Config::default();
    for node in doc.nodes() {
        match node.name().value() {
            "settings" => {
                // `timeout` is reserved for future tap-hold timing; the engine
                // currently resolves tap-vs-hold structurally, so it is unused.
            }
            "modmap" => lower_modmap(node, &mut config)?,
            "multipurpose-modmap" => config.multipurpose.push(lower_multipurpose(node)?),
            "keymap" => config.keymaps.push(lower_keymap(node)?),
            other => bail!("unknown top-level node {other:?}"),
        }
    }
    Ok(config)
}

fn lower_modmap(node: &KdlNode, config: &mut Config) -> Result<()> {
    for child in children(node) {
        expect_name(child, "map")?;
        let from = parse_key(prop_required(child, "from")?)?;
        let to = parse_key(prop_required(child, "to")?)?;
        config.modmap.insert(from, to);
    }
    Ok(())
}

fn lower_multipurpose(node: &KdlNode) -> Result<MultipurposeRule> {
    let matcher = build_matcher(node)?;
    let mut map = HashMap::new();
    for child in children(node) {
        expect_name(child, "map")?;
        let key = parse_key(prop_required(child, "key")?)?;
        let tap = parse_key(prop_required(child, "tap")?)?;
        let hold_key = parse_key(prop_required(child, "hold")?)?;
        let hold = hold_key
            .as_modifier()
            .ok_or_else(|| anyhow!("multipurpose hold {hold_key:?} is not a modifier key"))?;
        map.insert(key, (tap, hold));
    }
    Ok(MultipurposeRule { matcher, map })
}

fn lower_keymap(node: &KdlNode) -> Result<KeymapRule> {
    let matcher = build_matcher(node)?;
    let name = prop_str(node, "name").unwrap_or_default().to_string();
    let mut bindings = HashMap::new();
    for binding in children(node) {
        expect_name(binding, "bind")?;
        let from = first_arg(binding)
            .ok_or_else(|| anyhow!("bind is missing its trigger combo argument"))?;
        let trigger = Combo::parse(from)?;
        if prop_str(binding, "to").is_some() || prop_str(binding, "mark-to").is_some() {
            bail!(
                "bind {from:?}: the `to=`/`mark-to=` shorthand was removed; use a child \
                 node instead, e.g. {{ keys \"...\"; }} (add extend=\"selection\" for mark)"
            );
        }
        let mut steps = Vec::new();
        let mut had_steps = false;
        for step in children(binding) {
            had_steps = true;
            // A step that resolves to `None` lists only other OSes; omit it. The
            // binding still applies here as long as some other step remains, so a
            // single bind can pair e.g. a Windows `keys` with a Linux `exec`.
            if let Some(s) = lower_step(step)? {
                steps.push(s);
            }
        }
        // Every step was for another OS: this binding doesn't apply here.
        if had_steps && steps.is_empty() {
            continue;
        }
        if steps.is_empty() {
            bail!("bind {from:?} has no action");
        }
        bindings.insert(trigger, steps);
    }
    Ok(KeymapRule {
        name,
        matcher,
        bindings,
    })
}

/// Lower a step node. Returns `None` only for an `exec` whose per-OS target list
/// has no entry for the current OS — the caller then skips the whole binding.
fn lower_step(node: &KdlNode) -> Result<Option<Step>> {
    Ok(Some(match node.name().value() {
        "keys" => match os_target(node)? {
            // `extend="selection"` makes the emitted combo add Shift while a
            // selection is active; plain `keys` does not.
            Some(combo) => {
                let combo = Combo::parse(&combo)?;
                match prop_str(node, "extend") {
                    None => Step::Keys(combo),
                    Some("selection") => Step::WithMark(combo),
                    Some(other) => {
                        bail!("keys extend={other:?} is invalid (only extend=\"selection\")")
                    }
                }
            }
            None => return Ok(None),
        },
        "selection" => match first_arg(node) {
            Some("start") => Step::SetMark(true),
            Some("clear") => Step::SetMark(false),
            Some(other) => {
                bail!("selection {other:?} is invalid (expected \"start\" or \"clear\")")
            }
            None => bail!("selection needs an argument: \"start\" or \"clear\""),
        },
        "pass-through" => Step::PassThrough,
        "exec" => match os_target(node)? {
            Some(program) => Step::Exec(program),
            None => return Ok(None),
        },
        "wm" => Step::Window(lower_window_action(node)?),
        other => bail!("unknown step node {other:?}"),
    }))
}

/// Resolve a step node's target string for the current OS, shared by `keys` and
/// `exec`. A per-OS property (`windows=`/`linux=`/`macos=`) wins;
/// otherwise the positional argument is the cross-OS default. Returns `None` when
/// the node lists only other OSes (so the binding is skipped here); errors when it
/// gives no target at all.
fn os_target(node: &KdlNode) -> Result<Option<String>> {
    if let Some(target) = prop_str(node, std::env::consts::OS) {
        return Ok(Some(target.to_string()));
    }
    if let Some(default) = first_arg(node) {
        return Ok(Some(default.to_string()));
    }
    let names_an_os = ["windows", "linux", "macos"]
        .iter()
        .any(|os| prop_str(node, os).is_some());
    if names_an_os {
        Ok(None)
    } else {
        let name = node.name().value();
        Err(anyhow!(
            "{name} needs a target: a positional string, or an os property like windows=/linux="
        ))
    }
}

/// Lower a `wm action="..."` step into a [`WindowAction`]. Position/size
/// deltas (`dx`/`dy`/`dw`/`dh`) default to `0`; `preset` requires `w` and `h`.
fn lower_window_action(node: &KdlNode) -> Result<WindowAction> {
    let action = prop_required(node, "action")?;
    Ok(match action {
        "adjust" => WindowAction::Adjust {
            dx: prop_i32(node, "dx")?,
            dy: prop_i32(node, "dy")?,
            dw: prop_i32(node, "dw")?,
            dh: prop_i32(node, "dh")?,
        },
        "preset" => WindowAction::Preset {
            w: prop_f64(node, "w")?,
            h: prop_f64(node, "h")?,
            anchor: parse_anchor(prop_str(node, "anchor"))?,
        },
        "center" => WindowAction::Center,
        "snap" => WindowAction::Snap(parse_corner(prop_required(node, "to")?)?),
        "corner" => WindowAction::Corner(parse_corner(prop_required(node, "to")?)?),
        "smart-tile" => WindowAction::SmartTile {
            side: parse_side(prop_required(node, "to")?)?,
            // Placeholder; the engine fills the real fraction from the tile cycle.
            fraction: 0.5,
        },
        "find-window" => WindowAction::FindWindow,
        "maximize" => WindowAction::Maximize,
        "maximize-toggle" => WindowAction::MaximizeToggle,
        "minimize" => WindowAction::Minimize,
        "always-on-top" => WindowAction::AlwaysOnTop,
        "show-desktop" => WindowAction::ShowDesktop,
        "move-to-monitor" => {
            WindowAction::MoveToMonitor(parse_cycle_direction(Some(prop_required(node, "to")?))?)
        }
        "cycle-same-app" => WindowAction::CycleSameApp(parse_cycle_direction(prop_str(node, "direction"))?),
        "workspace" => WindowAction::Workspace {
            target: parse_workspace(prop_required(node, "to")?)?,
            move_window: false,
        },
        "move-to-workspace" => WindowAction::Workspace {
            target: parse_workspace(prop_required(node, "to")?)?,
            move_window: true,
        },
        other => bail!("unknown window action {other:?}"),
    })
}

/// Parse a `preset` anchor: a corner, or the centre (`center`/absent → `None`).
fn parse_anchor(value: Option<&str>) -> Result<Option<Corner>> {
    match value {
        None | Some("center") | Some("centre") => Ok(None),
        Some(corner) => Ok(Some(parse_corner(corner)?)),
    }
}

fn parse_corner(value: &str) -> Result<Corner> {
    Ok(match value {
        "top-left" => Corner::TopLeft,
        "top-right" => Corner::TopRight,
        "bottom-left" => Corner::BottomLeft,
        "bottom-right" => Corner::BottomRight,
        other => bail!("unknown corner {other:?} (expected top-left/top-right/bottom-left/bottom-right)"),
    })
}

fn parse_side(value: &str) -> Result<Side> {
    Ok(match value {
        "left" => Side::Left,
        "right" => Side::Right,
        "top" => Side::Top,
        "bottom" => Side::Bottom,
        other => bail!("unknown side {other:?} (expected left/right/top/bottom)"),
    })
}

/// Parse a `cycle-same-app` direction; absent defaults to forward.
fn parse_cycle_direction(value: Option<&str>) -> Result<CycleDirection> {
    Ok(match value {
        None | Some("forward") | Some("next") => CycleDirection::Forward,
        Some("backward") | Some("prev") | Some("previous") => CycleDirection::Backward,
        Some(other) => bail!("unknown cycle direction {other:?} (expected forward/backward)"),
    })
}

fn parse_workspace(value: &str) -> Result<Workspace> {
    Ok(match value {
        "prev" | "previous" | "left" => Workspace::Prev,
        "next" | "right" => Workspace::Next,
        number => Workspace::Index(
            number
                .parse::<u32>()
                .with_context(|| format!("workspace {number:?} must be prev/next or a number"))?,
        ),
    })
}

fn build_matcher(node: &KdlNode) -> Result<AppMatcher> {
    Ok(AppMatcher {
        include: compile(prop_str(node, "application"))?,
        exclude: compile(prop_str(node, "application-not"))?,
    })
}

fn compile(pattern: Option<&str>) -> Result<Option<Regex>> {
    pattern
        .map(|p| Regex::new(p).with_context(|| format!("invalid regex {p:?}")))
        .transpose()
}

fn parse_key(name: &str) -> Result<Key> {
    Key::parse(name).ok_or_else(|| anyhow!("unknown key {name:?}"))
}

// KDL accessors

/// The child nodes of `node`, or an empty slice when it has no block.
fn children(node: &KdlNode) -> &[KdlNode] {
    node.children().map(KdlDocument::nodes).unwrap_or(NO_NODES)
}

/// The first positional argument of `node` as a string, if any.
fn first_arg(node: &KdlNode) -> Option<&str> {
    node.entries()
        .iter()
        .find(|entry| entry.name().is_none())
        .and_then(|entry| entry.value().as_string())
}

/// A named property of `node` as a string, if present and string-valued.
fn prop_str<'a>(node: &'a KdlNode, name: &str) -> Option<&'a str> {
    node.entries()
        .iter()
        .find(|entry| entry.name().map(|n| n.value()) == Some(name))
        .and_then(|entry| entry.value().as_string())
}

fn prop_required<'a>(node: &'a KdlNode, name: &str) -> Result<&'a str> {
    prop_str(node, name)
        .ok_or_else(|| anyhow!("node {:?} is missing property {name:?}", node.name().value()))
}

/// The named integer property of `node`, or `0` when it is absent.
fn prop_i32(node: &KdlNode, name: &str) -> Result<i32> {
    match entry(node, name) {
        None => Ok(0),
        Some(value) => {
            let n = value
                .as_integer()
                .ok_or_else(|| anyhow!("property {name:?} must be an integer"))?;
            i32::try_from(n).with_context(|| format!("property {name:?} out of range"))
        }
    }
}

/// The named numeric property of `node` as `f64`, accepting an integer or float
/// literal (so both `w=1` and `w=0.6` parse). Required.
fn prop_f64(node: &KdlNode, name: &str) -> Result<f64> {
    let value = entry(node, name)
        .ok_or_else(|| anyhow!("node {:?} is missing property {name:?}", node.name().value()))?;
    if let Some(f) = value.as_float() {
        Ok(f)
    } else if let Some(i) = value.as_integer() {
        Ok(i as f64)
    } else {
        bail!("property {name:?} must be a number")
    }
}

/// The value of `node`'s named property, if present.
fn entry<'a>(node: &'a KdlNode, name: &str) -> Option<&'a kdl::KdlValue> {
    node.entries()
        .iter()
        .find(|entry| entry.name().map(|n| n.value()) == Some(name))
        .map(|entry| entry.value())
}

fn expect_name(node: &KdlNode, name: &str) -> Result<()> {
    if node.name().value() == name {
        Ok(())
    } else {
        bail!("expected {name:?} node, found {:?}", node.name().value())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn lower_str(text: &str) -> Result<Config> {
        let doc: KdlDocument = text.parse().map_err(|e: kdl::KdlError| anyhow!("{e}"))?;
        lower(&doc)
    }

    #[test]
    fn bundled_example_config_parses() {
        // Guards the shipped example against grammar drift (e.g. the `+`
        // separator and literal symbol key names).
        let example = include_str!("../config.example.kdl");
        lower_str(example).expect("config.example.kdl should parse");
    }

    #[test]
    fn lowers_modmap_and_keymap() {
        let config = lower_str(
            r#"
            modmap {
                map from="capslock" to="left_hyper"
            }
            keymap name="global" {
                bind "Hyper+left" { keys "s+left"; }
                bind "C+a" { keys "home" extend="selection"; }
                bind "C+k" {
                    keys "S+end"
                    keys "C+x"
                    selection "clear"
                }
            }
            "#,
        )
        .unwrap();

        assert_eq!(config.modmap.get(&Key::CapsLock), Some(&Key::LeftHyper));
        let keymap = &config.keymaps[0];
        assert_eq!(keymap.name, "global");
        assert_eq!(keymap.bindings.len(), 3);
        let ck = keymap.bindings.get(&Combo::parse("C+k").unwrap()).unwrap();
        assert_eq!(ck.len(), 3);
    }

    #[test]
    fn rejects_non_modifier_hold() {
        let err = lower_str(
            r#"
            multipurpose-modmap {
                map key="left_alt" tap="esc" hold="a"
            }
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a modifier"));
    }

    #[test]
    fn application_matcher_compiles() {
        let config = lower_str(
            r#"
            keymap application="Firefox|Brave" {
                bind "M+l" { keys "C+l"; }
            }
            "#,
        )
        .unwrap();
        let matcher = &config.keymaps[0].matcher;
        assert!(matcher.matches("Firefox"));
        assert!(!matcher.matches("Code"));
    }

    #[test]
    fn rejects_unknown_top_level_node() {
        assert!(lower_str("bogus-node\n").is_err());
    }

    #[test]
    fn lowers_exec_and_window_steps() {
        let config = lower_str(
            r#"
            keymap name="global" {
                bind "Hyper+b" { exec "brave"; }
                bind "Hyper+0" { wm action="maximize"; }
                bind "Hyper+y" { wm action="adjust" dx=-30; }
                bind "Hyper+6" { wm action="preset" w=0.6 h=0.75; }
            }
            "#,
        )
        .unwrap();
        let bindings = &config.keymaps[0].bindings;
        assert_eq!(bindings.len(), 4);

        let exec = bindings.get(&Combo::parse("Hyper+b").unwrap()).unwrap();
        assert!(matches!(exec.as_slice(), [Step::Exec(p)] if p == "brave"));

        let adjust = bindings.get(&Combo::parse("Hyper+y").unwrap()).unwrap();
        assert!(matches!(
            adjust.as_slice(),
            [Step::Window(WindowAction::Adjust { dx: -30, dy: 0, dw: 0, dh: 0 })]
        ));

        let preset = bindings.get(&Combo::parse("Hyper+6").unwrap()).unwrap();
        assert!(matches!(
            preset.as_slice(),
            [Step::Window(WindowAction::Preset { w, h, anchor: None })] if *w == 0.6 && *h == 0.75
        ));
    }

    #[test]
    fn parses_find_window_action() {
        let config = lower_str(r#"keymap { bind "Hyper+f" { wm action="find-window"; } }"#).unwrap();
        let b = &config.keymaps[0].bindings;
        assert!(matches!(
            b.get(&Combo::parse("Hyper+f").unwrap()).unwrap().as_slice(),
            [Step::Window(WindowAction::FindWindow)]
        ));
    }

    #[test]
    fn rejects_unknown_window_action() {
        assert!(lower_str(r#"keymap { bind "Hyper+x" { wm action="fly"; } }"#).is_err());
    }

    #[test]
    fn lowers_center_corner_smarttile_and_workspaces() {
        let config = lower_str(
            r#"
            keymap {
                bind "Hyper+c" { wm action="center"; }
                bind "Hyper+q" { wm action="corner" to="top-left"; }
                bind "Hyper+a" { wm action="smart-tile" to="left"; }
                bind "C+s+3" { wm action="workspace" to="3"; }
                bind "C+M+s+3" { wm action="move-to-workspace" to="3"; }
                bind "C+s+l" { wm action="workspace" to="next"; }
            }
            "#,
        )
        .unwrap();
        let b = &config.keymaps[0].bindings;

        assert!(matches!(
            b.get(&Combo::parse("Hyper+c").unwrap()).unwrap().as_slice(),
            [Step::Window(WindowAction::Center)]
        ));
        assert!(matches!(
            b.get(&Combo::parse("Hyper+q").unwrap()).unwrap().as_slice(),
            [Step::Window(WindowAction::Corner(Corner::TopLeft))]
        ));
        assert!(matches!(
            b.get(&Combo::parse("Hyper+a").unwrap()).unwrap().as_slice(),
            [Step::Window(WindowAction::SmartTile { side: Side::Left, .. })]
        ));
        assert!(matches!(
            b.get(&Combo::parse("C+s+3").unwrap()).unwrap().as_slice(),
            [Step::Window(WindowAction::Workspace { target: Workspace::Index(3), move_window: false })]
        ));
        assert!(matches!(
            b.get(&Combo::parse("C+M+s+3").unwrap()).unwrap().as_slice(),
            [Step::Window(WindowAction::Workspace { target: Workspace::Index(3), move_window: true })]
        ));
        assert!(matches!(
            b.get(&Combo::parse("C+s+l").unwrap()).unwrap().as_slice(),
            [Step::Window(WindowAction::Workspace { target: Workspace::Next, move_window: false })]
        ));
    }

    #[test]
    fn parses_minimize_always_on_top_and_move_to_monitor() {
        let config = lower_str(
            r#"
            keymap {
                bind "a" { wm action="minimize"; }
                bind "b" { wm action="always-on-top"; }
                bind "c" { wm action="move-to-monitor" to="next"; }
                bind "d" { wm action="move-to-monitor" to="prev"; }
            }
            "#,
        )
        .unwrap();
        let b = &config.keymaps[0].bindings;
        assert!(matches!(
            b.get(&Combo::parse("a").unwrap()).unwrap().as_slice(),
            [Step::Window(WindowAction::Minimize)]
        ));
        assert!(matches!(
            b.get(&Combo::parse("b").unwrap()).unwrap().as_slice(),
            [Step::Window(WindowAction::AlwaysOnTop)]
        ));
        assert!(matches!(
            b.get(&Combo::parse("c").unwrap()).unwrap().as_slice(),
            [Step::Window(WindowAction::MoveToMonitor(CycleDirection::Forward))]
        ));
        assert!(matches!(
            b.get(&Combo::parse("d").unwrap()).unwrap().as_slice(),
            [Step::Window(WindowAction::MoveToMonitor(CycleDirection::Backward))]
        ));
        // move-to-monitor requires a direction.
        assert!(lower_str(r#"keymap { bind "e" { wm action="move-to-monitor"; } }"#).is_err());
    }

    #[test]
    fn cycle_same_app_direction_defaults_to_forward_and_parses_backward() {
        let config = lower_str(
            r#"
            keymap {
                bind "a" { wm action="cycle-same-app"; }
                bind "b" { wm action="cycle-same-app" direction="backward"; }
            }
            "#,
        )
        .unwrap();
        let b = &config.keymaps[0].bindings;
        assert!(matches!(
            b.get(&Combo::parse("a").unwrap()).unwrap().as_slice(),
            [Step::Window(WindowAction::CycleSameApp(CycleDirection::Forward))]
        ));
        assert!(matches!(
            b.get(&Combo::parse("b").unwrap()).unwrap().as_slice(),
            [Step::Window(WindowAction::CycleSameApp(CycleDirection::Backward))]
        ));
        assert!(lower_str(r#"keymap { bind "c" { wm action="cycle-same-app" direction="sideways"; } }"#).is_err());
    }

    #[test]
    fn preset_anchor_defaults_to_center_and_parses_corners() {
        let config = lower_str(
            r#"
            keymap {
                bind "a" { wm action="preset" w=0.87 h=0.81 anchor="top-left"; }
                bind "b" { wm action="preset" w=0.5 h=0.6; }
                bind "c" { wm action="preset" w=0.5 h=0.6 anchor="center"; }
            }
            "#,
        )
        .unwrap();
        let b = &config.keymaps[0].bindings;
        assert!(matches!(
            b.get(&Combo::parse("a").unwrap()).unwrap().as_slice(),
            [Step::Window(WindowAction::Preset { anchor: Some(Corner::TopLeft), .. })]
        ));
        assert!(matches!(
            b.get(&Combo::parse("b").unwrap()).unwrap().as_slice(),
            [Step::Window(WindowAction::Preset { anchor: None, .. })]
        ));
        assert!(matches!(
            b.get(&Combo::parse("c").unwrap()).unwrap().as_slice(),
            [Step::Window(WindowAction::Preset { anchor: None, .. })]
        ));
    }

    #[test]
    fn rejects_unknown_corner_and_workspace() {
        assert!(lower_str(r#"keymap { bind "a" { wm action="corner" to="middle"; } }"#).is_err());
        assert!(lower_str(r#"keymap { bind "a" { wm action="workspace" to="abc"; } }"#).is_err());
    }

    #[test]
    fn exec_resolves_per_os_target_and_skips_other_os_only() {
        let this = std::env::consts::OS;
        let other = if this == "linux" { "windows" } else { "linux" };
        let config = lower_str(&format!(
            r#"
            keymap {{
                bind "a" {{ exec windows="w.exe" linux="l" macos="m"; }}
                bind "b" {{ exec "shared"; }}
                bind "c" {{ exec {other}="only-other"; }}
            }}
            "#
        ))
        .unwrap();
        let b = &config.keymaps[0].bindings;

        // Positional default applies on every OS.
        assert!(matches!(
            b.get(&Combo::parse("b").unwrap()).unwrap().as_slice(),
            [Step::Exec(p)] if p == "shared"
        ));
        // A target listing only the *other* OS is skipped here.
        assert!(!b.contains_key(&Combo::parse("c").unwrap()));
        // The per-OS target resolves to this host's value (when listed).
        if let "windows" | "linux" | "macos" = this {
            let expected = match this {
                "windows" => "w.exe",
                "linux" => "l",
                _ => "m",
            };
            assert!(matches!(
                b.get(&Combo::parse("a").unwrap()).unwrap().as_slice(),
                [Step::Exec(p)] if p == expected
            ));
        }
    }

    #[test]
    fn keys_resolves_per_os_and_skips_other_os_only() {
        let this = std::env::consts::OS;
        let other = if this == "linux" { "windows" } else { "linux" };
        let config = lower_str(&format!(
            r#"
            keymap {{
                bind "a" {{ keys windows="C+a" linux="C+q" macos="s+q"; }}
                bind "b" {{ keys "C+b"; }}
                bind "c" {{ keys {other}="C+c"; }}
            }}
            "#
        ))
        .unwrap();
        let b = &config.keymaps[0].bindings;

        // Positional default applies on every OS.
        let expected_b = Combo::parse("C+b").unwrap();
        assert!(matches!(
            b.get(&Combo::parse("b").unwrap()).unwrap().as_slice(),
            [Step::Keys(c)] if *c == expected_b
        ));
        // A remap listing only the *other* OS is skipped here.
        assert!(!b.contains_key(&Combo::parse("c").unwrap()));
        // The per-OS remap resolves to this host's value.
        let expected_a = match this {
            "windows" => "C+a",
            "macos" => "s+q",
            _ => "C+q",
        };
        let expected_a = Combo::parse(expected_a).unwrap();
        assert!(matches!(
            b.get(&Combo::parse("a").unwrap()).unwrap().as_slice(),
            [Step::Keys(c)] if *c == expected_a
        ));
    }

    #[test]
    fn selection_node_and_keys_extend() {
        let config = lower_str(
            r#"
            keymap {
                bind "a" { selection "start"; }
                bind "b" { selection "clear"; }
                bind "c" { keys "home" extend="selection"; }
                bind "d" { keys "home"; }
            }
            "#,
        )
        .unwrap();
        let bindings = &config.keymaps[0].bindings;
        let get = |c: &str| bindings.get(&Combo::parse(c).unwrap()).unwrap().as_slice();
        let home = Combo::parse("home").unwrap();
        assert!(matches!(get("a"), [Step::SetMark(true)]));
        assert!(matches!(get("b"), [Step::SetMark(false)]));
        assert!(matches!(get("c"), [Step::WithMark(x)] if *x == home));
        assert!(matches!(get("d"), [Step::Keys(x)] if *x == home));
    }

    #[test]
    fn bind_pairs_per_os_keys_and_exec() {
        // A bind may pair a Windows-only `keys` with a Linux-only `exec`; only the
        // step for this OS survives, and the binding still applies.
        let config = lower_str(
            r#"keymap { bind "s+x" { keys windows="C+esc"; exec linux="menu.sh"; } }"#,
        )
        .unwrap();
        let steps = config
            .keymaps[0]
            .bindings
            .get(&Combo::parse("s+x").unwrap())
            .unwrap()
            .as_slice();
        match std::env::consts::OS {
            "linux" => assert!(matches!(steps, [Step::Exec(p)] if p == "menu.sh")),
            "windows" => {
                let esc = Combo::parse("C+esc").unwrap();
                assert!(matches!(steps, [Step::Keys(c)] if *c == esc));
            }
            _ => {}
        }
    }
}
