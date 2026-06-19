//! Portable key model: the OS-agnostic [`Key`] / [`Modifier`] / [`Combo`] types
//! that the config and engine speak in.
//!
//! A [`Key`] is a physical key identity (e.g. `A`, `Left`, `VolumeUp`). Each key
//! carries the two per-OS scancodes it lowers to: a Linux `evdev` code and a
//! Windows virtual-key code. The backends translate to/from these; nothing else
//! in the crate touches raw codes.

// Imports

use std::collections::BTreeSet;

use anyhow::{bail, Result};

// Data Structures

/// A modifier that can qualify a [`Combo`]. `Hyper` is synthetic: it is tracked
/// internally (typically `CapsLock` remapped via the modmap) and is never
/// emitted to the OS.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Modifier {
    Alt,
    Control,
    Hyper,
    Shift,
    Super,
}

/// A key chord: a set of modifiers plus a single triggering key.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Combo {
    pub modifiers: BTreeSet<Modifier>,
    pub key: Key,
}

// === Modifier ===

impl Modifier {
    /// Parse a combo-prefix token. The modifier prefixes are the Emacs-style
    /// letters `C` (Control), `M` (Alt), `S` (Shift), `s` (Super), plus `Hyper`.
    /// All are case-sensitive (`S` and `s` differ).
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "C" => Some(Modifier::Control),
            "M" => Some(Modifier::Alt),
            "S" => Some(Modifier::Shift),
            "s" => Some(Modifier::Super),
            "Hyper" => Some(Modifier::Hyper),
            _ => None,
        }
    }

    /// The concrete (left-hand) key the OS should see for this modifier, or
    /// `None` for `Hyper`, which is internal-only and never emitted.
    pub fn emit_key(self) -> Option<Key> {
        match self {
            Modifier::Alt => Some(Key::LeftAlt),
            Modifier::Control => Some(Key::LeftCtrl),
            Modifier::Hyper => None,
            Modifier::Shift => Some(Key::LeftShift),
            Modifier::Super => Some(Key::LeftMeta),
        }
    }
}

// === Combo ===

impl Combo {
    /// Parse a combo string such as `C+M+s+S+a` or `Hyper+left` or a bare key
    /// like `home`. Tokens are `+`-separated; every token but the last is a
    /// modifier, the last is the key.
    pub fn parse(spec: &str) -> Result<Self> {
        let mut modifiers = BTreeSet::new();
        let parts: Vec<&str> = spec.split('+').collect();
        let (key_token, mod_tokens) = parts
            .split_last()
            .filter(|(last, _)| !last.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "invalid key combo {spec:?}: the key is missing. \
                     '+' joins modifiers to the key, e.g. `C+a` or `C+S+-`, \
                     or a bare key like `=`"
                )
            })?;
        for token in mod_tokens {
            match Modifier::parse(token) {
                Some(m) => {
                    modifiers.insert(m);
                }
                None => bail!(
                    "invalid key combo {spec:?}: unknown modifier {token:?} \
                     (expected C=Ctrl, M=Alt, S=Shift, s=Super, Hyper)"
                ),
            }
        }
        let key = Key::parse(key_token).ok_or_else(|| {
            anyhow::anyhow!(
                "invalid key combo {spec:?}: unknown key {key_token:?}. \
                 Keys use their symbol or evdev name, e.g. `a`, `-`, `=`, `/`, \
                 `enter`, `space`, `f5`"
            )
        })?;
        Ok(Combo { modifiers, key })
    }
}

// Key definitions
//
// `define_keys!` generates the `Key` enum together with its name table and the
// per-OS code tables, keeping the single source of truth in one place. Codes are
// the stable Linux `input-event-codes.h` constants and Windows `VK_*` constants.

macro_rules! define_keys {
    (
        $(
            $variant:ident {
                names: [$($name:literal),+ $(,)?],
                evdev: $evdev:expr,
                vk: $vk:expr
                $(, modifier: $modifier:expr)?
            }
        ),+ $(,)?
    ) => {
        /// An OS-agnostic physical key identity.
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub enum Key {
            $($variant),+
        }

        impl Key {
            /// Every known key, used for reverse code lookups.
            const ALL: &'static [Key] = &[$(Key::$variant),+];

            /// Look up a key by any of its accepted names (case-insensitive).
            pub fn parse(name: &str) -> Option<Self> {
                let lower = name.to_ascii_lowercase();
                match lower.as_str() {
                    $($($name => Some(Key::$variant),)+)+
                    _ => None,
                }
            }

            /// The Linux `evdev` key code for this key.
            #[allow(dead_code)] // used by the Linux backend only
            pub fn evdev_code(self) -> u16 {
                match self {
                    $(Key::$variant => $evdev),+
                }
            }

            /// The Windows virtual-key code for this key.
            #[allow(dead_code)] // used by the Windows backend only
            pub fn win_vk(self) -> u16 {
                match self {
                    $(Key::$variant => $vk),+
                }
            }

            /// The [`Modifier`] this key acts as, if it is a modifier key.
            pub fn as_modifier(self) -> Option<Modifier> {
                match self {
                    $($(Key::$variant => Some($modifier),)?)+
                    _ => None,
                }
            }
        }
    };
}

define_keys! {
    // Letters
    A { names: ["a"], evdev: 30, vk: 0x41 },
    B { names: ["b"], evdev: 48, vk: 0x42 },
    C { names: ["c"], evdev: 46, vk: 0x43 },
    D { names: ["d"], evdev: 32, vk: 0x44 },
    E { names: ["e"], evdev: 18, vk: 0x45 },
    F { names: ["f"], evdev: 33, vk: 0x46 },
    G { names: ["g"], evdev: 34, vk: 0x47 },
    H { names: ["h"], evdev: 35, vk: 0x48 },
    I { names: ["i"], evdev: 23, vk: 0x49 },
    J { names: ["j"], evdev: 36, vk: 0x4A },
    K { names: ["k"], evdev: 37, vk: 0x4B },
    L { names: ["l"], evdev: 38, vk: 0x4C },
    M { names: ["m"], evdev: 50, vk: 0x4D },
    N { names: ["n"], evdev: 49, vk: 0x4E },
    O { names: ["o"], evdev: 24, vk: 0x4F },
    P { names: ["p"], evdev: 25, vk: 0x50 },
    Q { names: ["q"], evdev: 16, vk: 0x51 },
    R { names: ["r"], evdev: 19, vk: 0x52 },
    S { names: ["s"], evdev: 31, vk: 0x53 },
    T { names: ["t"], evdev: 20, vk: 0x54 },
    U { names: ["u"], evdev: 22, vk: 0x55 },
    V { names: ["v"], evdev: 47, vk: 0x56 },
    W { names: ["w"], evdev: 17, vk: 0x57 },
    X { names: ["x"], evdev: 45, vk: 0x58 },
    Y { names: ["y"], evdev: 21, vk: 0x59 },
    Z { names: ["z"], evdev: 44, vk: 0x5A },

    // Digits
    Num1 { names: ["1"], evdev: 2, vk: 0x31 },
    Num2 { names: ["2"], evdev: 3, vk: 0x32 },
    Num3 { names: ["3"], evdev: 4, vk: 0x33 },
    Num4 { names: ["4"], evdev: 5, vk: 0x34 },
    Num5 { names: ["5"], evdev: 6, vk: 0x35 },
    Num6 { names: ["6"], evdev: 7, vk: 0x36 },
    Num7 { names: ["7"], evdev: 8, vk: 0x37 },
    Num8 { names: ["8"], evdev: 9, vk: 0x38 },
    Num9 { names: ["9"], evdev: 10, vk: 0x39 },
    Num0 { names: ["0"], evdev: 11, vk: 0x30 },

    // Whitespace / editing
    Backspace { names: ["backspace"], evdev: 14, vk: 0x08 },
    Delete { names: ["delete"], evdev: 111, vk: 0x2E },
    Enter { names: ["enter"], evdev: 28, vk: 0x0D },
    Esc { names: ["esc"], evdev: 1, vk: 0x1B },
    Space { names: ["space"], evdev: 57, vk: 0x20 },
    Tab { names: ["tab"], evdev: 15, vk: 0x09 },

    // Navigation
    Down { names: ["down"], evdev: 108, vk: 0x28 },
    End { names: ["end"], evdev: 107, vk: 0x23 },
    Home { names: ["home"], evdev: 102, vk: 0x24 },
    Left { names: ["left"], evdev: 105, vk: 0x25 },
    PageDown { names: ["page_down"], evdev: 109, vk: 0x22 },
    PageUp { names: ["page_up"], evdev: 104, vk: 0x21 },
    Right { names: ["right"], evdev: 106, vk: 0x27 },
    Up { names: ["up"], evdev: 103, vk: 0x26 },

    // Punctuation (named by the literal unshifted glyph the key produces)
    Apostrophe { names: ["'"], evdev: 40, vk: 0xDE },
    Backslash { names: ["\\"], evdev: 43, vk: 0xDC },
    Comma { names: [","], evdev: 51, vk: 0xBC },
    Dot { names: ["."], evdev: 52, vk: 0xBE },
    Equal { names: ["="], evdev: 13, vk: 0xBB },
    Backtick { names: ["`"], evdev: 41, vk: 0xC0 },
    LeftBrace { names: ["["], evdev: 26, vk: 0xDB },
    Minus { names: ["-"], evdev: 12, vk: 0xBD },
    RightBrace { names: ["]"], evdev: 27, vk: 0xDD },
    Semicolon { names: [";"], evdev: 39, vk: 0xBA },
    Slash { names: ["/"], evdev: 53, vk: 0xBF },

    // Function keys
    F1 { names: ["f1"], evdev: 59, vk: 0x70 },
    F2 { names: ["f2"], evdev: 60, vk: 0x71 },
    F3 { names: ["f3"], evdev: 61, vk: 0x72 },
    F4 { names: ["f4"], evdev: 62, vk: 0x73 },
    F5 { names: ["f5"], evdev: 63, vk: 0x74 },
    F6 { names: ["f6"], evdev: 64, vk: 0x75 },
    F7 { names: ["f7"], evdev: 65, vk: 0x76 },
    F8 { names: ["f8"], evdev: 66, vk: 0x77 },
    F9 { names: ["f9"], evdev: 67, vk: 0x78 },
    F10 { names: ["f10"], evdev: 68, vk: 0x79 },
    F11 { names: ["f11"], evdev: 87, vk: 0x7A },
    F12 { names: ["f12"], evdev: 88, vk: 0x7B },

    // System / media
    CapsLock { names: ["capslock"], evdev: 58, vk: 0x14 },
    Mute { names: ["mute"], evdev: 113, vk: 0xAD },
    NextSong { names: ["nextsong"], evdev: 163, vk: 0xB0 },
    Pause { names: ["pause"], evdev: 119, vk: 0x13 },
    PlayPause { names: ["playpause"], evdev: 164, vk: 0xB3 },
    PreviousSong { names: ["previoussong"], evdev: 165, vk: 0xB1 },
    ScrollLock { names: ["scrolllock"], evdev: 70, vk: 0x91 },
    PrintScreen { names: ["printscreen"], evdev: 99, vk: 0x2C },
    VolumeDown { names: ["volumedown"], evdev: 114, vk: 0xAE },
    VolumeUp { names: ["volumeup"], evdev: 115, vk: 0xAF },

    // Modifier keys
    LeftAlt { names: ["left_alt"], evdev: 56, vk: 0xA4, modifier: Modifier::Alt },
    LeftCtrl { names: ["left_ctrl"], evdev: 29, vk: 0xA2, modifier: Modifier::Control },
    LeftHyper { names: ["left_hyper"], evdev: 0, vk: 0, modifier: Modifier::Hyper },
    LeftMeta { names: ["left_meta"], evdev: 125, vk: 0x5B, modifier: Modifier::Super },
    LeftShift { names: ["left_shift"], evdev: 42, vk: 0xA0, modifier: Modifier::Shift },
    RightAlt { names: ["right_alt"], evdev: 100, vk: 0xA5, modifier: Modifier::Alt },
    RightCtrl { names: ["right_ctrl"], evdev: 97, vk: 0xA3, modifier: Modifier::Control },
    RightHyper { names: ["right_hyper"], evdev: 0, vk: 0, modifier: Modifier::Hyper },
    RightMeta { names: ["right_meta"], evdev: 126, vk: 0x5C, modifier: Modifier::Super },
    RightShift { names: ["right_shift"], evdev: 54, vk: 0xA1, modifier: Modifier::Shift },
}

impl Key {
    /// Whether this key is one of the function keys F1–F12. A real modifier
    /// paired with a function key (e.g. `M+f4` to close a window) is a
    /// window-manager shortcut rather than a repeatable key, so the engine
    /// emits such chords one-shot — holding them would leave the modifier
    /// reported as down while the shortcut's dialog runs.
    pub fn is_function_key(self) -> bool {
        matches!(
            self,
            Key::F1
                | Key::F2
                | Key::F3
                | Key::F4
                | Key::F5
                | Key::F6
                | Key::F7
                | Key::F8
                | Key::F9
                | Key::F10
                | Key::F11
                | Key::F12
        )
    }

    /// Build a key from a raw Linux `evdev` code, or `None` if unknown.
    /// Code `0` (the synthetic `Hyper` placeholder) never matches.
    #[cfg(target_os = "linux")]
    pub fn from_evdev_code(code: u16) -> Option<Self> {
        if code == 0 {
            return None;
        }
        Key::ALL.iter().copied().find(|k| k.evdev_code() == code)
    }

    /// Build a key from a raw Windows virtual-key code, or `None` if unknown.
    #[cfg(windows)]
    pub fn from_win_vk(vk: u16) -> Option<Self> {
        if vk == 0 {
            return None;
        }
        Key::ALL.iter().copied().find(|k| k.win_vk() == vk)
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_key() {
        let combo = Combo::parse("home").unwrap();
        assert!(combo.modifiers.is_empty());
        assert_eq!(combo.key, Key::Home);
    }

    #[test]
    fn parses_multi_modifier_combo() {
        let combo = Combo::parse("C+M+s+S+a").unwrap();
        assert_eq!(combo.key, Key::A);
        assert!(combo.modifiers.contains(&Modifier::Control));
        assert!(combo.modifiers.contains(&Modifier::Alt));
        assert!(combo.modifiers.contains(&Modifier::Super));
        assert!(combo.modifiers.contains(&Modifier::Shift));
    }

    #[test]
    fn single_letter_super_and_shift_are_case_sensitive() {
        // S = Shift, s = Super (Emacs-style modifier letters).
        let shift = Combo::parse("C+S+a").unwrap();
        assert!(shift.modifiers.contains(&Modifier::Control));
        assert!(shift.modifiers.contains(&Modifier::Shift));
        assert!(!shift.modifiers.contains(&Modifier::Super));

        let super_ = Combo::parse("C+s+a").unwrap();
        assert!(super_.modifiers.contains(&Modifier::Super));
        assert!(!super_.modifiers.contains(&Modifier::Shift));

        // The 's' key is still a key when it is the final token.
        assert_eq!(Combo::parse("C+s").unwrap().key, Key::S);
    }

    #[test]
    fn parses_literal_symbol_keys() {
        // Punctuation keys are named by their glyph and need no escaping in the
        // combo grammar now that `+` (not `-`) is the separator.
        assert_eq!(Combo::parse("C+-").unwrap().key, Key::Minus);
        assert_eq!(Combo::parse("C+S+=").unwrap().key, Key::Equal);
        assert_eq!(Combo::parse("s+/").unwrap().key, Key::Slash);
        assert_eq!(Combo::parse("[").unwrap().key, Key::LeftBrace);
        assert_eq!(Combo::parse("-").unwrap().key, Key::Minus);
    }

    #[test]
    fn parses_hyper_and_digit() {
        let combo = Combo::parse("Hyper+1").unwrap();
        assert_eq!(combo.key, Key::Num1);
        assert_eq!(
            combo.modifiers.iter().copied().collect::<Vec<_>>(),
            vec![Modifier::Hyper]
        );
    }

    #[test]
    fn rejects_unknown_tokens() {
        assert!(Combo::parse("Nope+a").is_err());
        assert!(Combo::parse("C+boguskey").is_err());
        assert!(Combo::parse("").is_err());
    }

    #[test]
    fn error_messages_are_actionable() {
        // A trailing separator leaves the key missing and explains the grammar.
        let empty = Combo::parse("C+").unwrap_err().to_string();
        assert!(empty.contains("invalid key combo"), "{empty}");
        assert!(empty.contains("the key is missing"), "{empty}");

        // Unknown modifier lists the valid prefixes.
        let modifier = Combo::parse("Ctrl+a").unwrap_err().to_string();
        assert!(modifier.contains("unknown modifier"), "{modifier}");
        assert!(modifier.contains("Super"), "{modifier}");

        // An unrecognized symbol points at key names.
        let key = Combo::parse("S+s+@").unwrap_err().to_string();
        assert!(key.contains("unknown key"), "{key}");
    }

    #[test]
    fn modifier_keys_report_themselves() {
        assert_eq!(Key::CapsLock.as_modifier(), None);
        assert_eq!(Key::LeftHyper.as_modifier(), Some(Modifier::Hyper));
        assert_eq!(Key::RightAlt.as_modifier(), Some(Modifier::Alt));
    }
}
