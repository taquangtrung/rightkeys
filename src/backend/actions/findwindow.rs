//! Portable find-window overlay logic shared by the platform backends: hint
//! label generation, key→hint mapping, the prefix navigator, and extracting the
//! application-name component from a window title. Window enumeration and the
//! on-screen rendering of the hint chips are backend-specific.

// Imports

use crate::key::Key;

// Constants

/// Hint key sequence for the find-window overlay (home-row keys first).
const HINT_KEYS: &[char] = &[
    'a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l', 'q', 'w', 'e', 'r', 't',
    'y', 'u', 'i', 'o', 'p', 'z', 'x', 'c', 'v', 'b', 'n', 'm',
];

/// Separators apps put between a window title's components, widest dashes first.
const TITLE_SEPARATORS: [&str; 5] = [" — ", " – ", " - ", " | ", " · "];

/// Gap left between hint chips when nudging one off another to avoid overlap.
const OVERLAY_GAP: i32 = 4;

// Data Structures

/// Result of feeding one hint character to the navigator.
pub enum HintMatch {
    /// More input is needed (the prefix narrowed but several hints still match,
    /// or the character matched nothing and was ignored).
    Pending,
    /// Exactly one hint matched; its index into the hint list.
    Done(usize),
}

// Functions

/// Generate `count` short hint strings drawn from [`HINT_KEYS`] (home-row first,
/// two-character hints when more than 26 windows are present).
pub fn make_hints(count: usize) -> Vec<String> {
    if count == 0 {
        return Vec::new();
    }
    let n = HINT_KEYS.len();
    let mut length = 1usize;
    while n.pow(length as u32) < count {
        length += 1;
    }
    let mut hints = Vec::with_capacity(count);
    let mut indices = vec![0usize; length];
    for _ in 0..count {
        hints.push(indices.iter().map(|&i| HINT_KEYS[i]).collect());
        for i in (0..length).rev() {
            indices[i] += 1;
            if indices[i] < n {
                break;
            }
            indices[i] = 0;
        }
    }
    hints
}

/// Map a [`Key`] to the corresponding [`HINT_KEYS`] character, if any.
pub fn key_to_hint_char(key: Key) -> Option<char> {
    match key {
        Key::A => Some('a'),
        Key::B => Some('b'),
        Key::C => Some('c'),
        Key::D => Some('d'),
        Key::E => Some('e'),
        Key::F => Some('f'),
        Key::G => Some('g'),
        Key::H => Some('h'),
        Key::I => Some('i'),
        Key::J => Some('j'),
        Key::K => Some('k'),
        Key::L => Some('l'),
        Key::M => Some('m'),
        Key::N => Some('n'),
        Key::O => Some('o'),
        Key::P => Some('p'),
        Key::Q => Some('q'),
        Key::R => Some('r'),
        Key::S => Some('s'),
        Key::T => Some('t'),
        Key::U => Some('u'),
        Key::V => Some('v'),
        Key::W => Some('w'),
        Key::X => Some('x'),
        Key::Y => Some('y'),
        Key::Z => Some('z'),
        _ => None,
    }
}

/// Feed hint character `ch` to the navigator. Appends it to `prefix` when at
/// least one hint still matches (otherwise the character is ignored and `prefix`
/// is unchanged). Returns [`HintMatch::Done`] with the unique index once a
/// single hint remains.
pub fn advance(hints: &[String], prefix: &mut String, ch: char) -> HintMatch {
    let candidate = format!("{prefix}{ch}");
    let matched: Vec<usize> = (0..hints.len())
        .filter(|&i| hints[i].starts_with(&candidate))
        .collect();
    if matched.is_empty() {
        return HintMatch::Pending;
    }
    *prefix = candidate;
    match matched.as_slice() {
        [only] => HintMatch::Done(*only),
        _ => HintMatch::Pending,
    }
}

/// Split a window `title` into `(brand, rest)`: the application-name component
/// and the remaining document/page part. The app's branding in the title may
/// itself span several separator-joined segments and need not match `app`
/// verbatim (WM_CLASS "code-insiders" vs the title's "Visual Studio Code -
/// Insiders"), so this works on tokens: it isolates the smallest run of trailing
/// (then leading) segments whose combined tokens cover every token of `app`.
/// e.g. "… - ZKDual - Visual Studio Code - Insiders" with app "code-insiders"
/// yields ("Visual Studio Code - Insiders", "… - ZKDual"). `brand` is empty when
/// no such run exists short of the whole title.
pub fn split_app_from_title(title: &str, app: &str) -> (String, String) {
    let title = title.trim();
    let app_tokens = tokenize(app);
    if app_tokens.is_empty() {
        return (String::new(), title.to_string());
    }
    let ranges = segment_ranges(title);
    let covers = |run: &[(usize, usize)]| {
        let tokens: Vec<String> = run.iter().flat_map(|&(a, b)| tokenize(&title[a..b])).collect();
        app_tokens.iter().all(|t| tokens.contains(t))
    };
    // Trailing run: smallest non-empty suffix (short of the whole) that is the app.
    for k in 1..ranges.len() {
        if covers(&ranges[ranges.len() - k..]) {
            let brand = title[ranges[ranges.len() - k].0..].trim().to_string();
            let rest = title[..ranges[ranges.len() - k - 1].1].trim().to_string();
            return (brand, rest);
        }
    }
    // Leading run: apps that put their name first.
    for k in 1..ranges.len() {
        if covers(&ranges[..k]) {
            let brand = title[..ranges[k - 1].1].trim().to_string();
            let rest = title[ranges[k].0..].trim().to_string();
            return (brand, rest);
        }
    }
    (String::new(), title.to_string())
}

/// The `(start, end)` byte ranges of a title's components, split on any
/// [`TITLE_SEPARATORS`]. Always returns at least one range (the whole string).
fn segment_ranges(title: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < title.len() {
        let rest = &title[i..];
        if let Some(sep) = TITLE_SEPARATORS.iter().find(|s| rest.starts_with(**s)) {
            ranges.push((start, i));
            i += sep.len();
            start = i;
        } else {
            i += rest.chars().next().map_or(1, char::len_utf8);
        }
    }
    ranges.push((start, title.len()));
    ranges
}

/// Lowercased alphanumeric tokens of `s`, dropping single-character noise.
fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 2)
        .map(str::to_lowercase)
        .collect()
}

/// Whether two `(x, y, w, h)` rectangles overlap.
pub fn rects_overlap(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
    let (ax, ay, aw, ah) = a;
    let (bx, by, bw, bh) = b;
    ax < bx + bw && bx < ax + aw && ay < by + bh && by < ay + ah
}

/// Choose a top-left for a `w`×`h` hint chip near `desired`, clamped on screen
/// and not overlapping any already-`placed` chip. The desired spot is tried
/// first, then slots stepping downward, then upward (windows stacked at the same
/// corner thus get their chips fanned vertically). Falls back to the desired
/// spot when no free slot fits.
pub fn place_hint(
    desired: (i32, i32),
    size: (i32, i32),
    placed: &[(i32, i32, i32, i32)],
    screen: (i32, i32),
) -> (i32, i32) {
    let (w, h) = size;
    let (sw, sh) = screen;
    let x = desired.0.clamp(0, (sw - w).max(0));
    let y0 = desired.1.clamp(0, (sh - h).max(0));

    let step = h + OVERLAY_GAP;
    let mut candidates = vec![y0];
    let mut down = y0;
    while down + step <= sh - h {
        down += step;
        candidates.push(down);
    }
    let mut up = y0;
    while up - step >= 0 {
        up -= step;
        candidates.push(up);
    }
    for cy in candidates {
        if !placed.iter().any(|&p| rects_overlap(p, (x, cy, w, h))) {
            return (x, cy);
        }
    }
    (x, y0)
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_hints_single_char_for_small_counts() {
        let hints = make_hints(3);
        assert_eq!(hints, vec!["a", "s", "d"]);
    }

    #[test]
    fn make_hints_two_chars_when_more_than_26() {
        let hints = make_hints(27);
        assert_eq!(hints.len(), 27);
        assert_eq!(hints[0], "aa");
        assert_eq!(hints[1], "as");
        assert_eq!(hints[26], "sa");
    }

    #[test]
    fn make_hints_empty() {
        assert!(make_hints(0).is_empty());
    }

    #[test]
    fn advance_narrows_then_resolves() {
        let hints = vec!["aa".to_string(), "as".to_string(), "sa".to_string()];
        let mut prefix = String::new();
        // 'a' still matches "aa" and "as": pending, prefix advanced.
        assert!(matches!(advance(&hints, &mut prefix, 'a'), HintMatch::Pending));
        assert_eq!(prefix, "a");
        // A non-matching char is ignored, prefix unchanged.
        assert!(matches!(advance(&hints, &mut prefix, 'z'), HintMatch::Pending));
        assert_eq!(prefix, "a");
        // 's' leaves only "as": done at its index.
        assert!(matches!(advance(&hints, &mut prefix, 's'), HintMatch::Done(1)));
    }

    #[test]
    fn place_hint_keeps_desired_spot_when_free() {
        assert_eq!(place_hint((100, 50), (80, 30), &[], (1920, 1080)), (100, 50));
    }

    #[test]
    fn place_hint_nudges_down_off_a_collision() {
        let placed = [(100, 50, 80, 30)];
        let pos = place_hint((100, 50), (80, 30), &placed, (1920, 1080));
        assert_eq!(pos, (100, 50 + 30 + OVERLAY_GAP));
        assert!(!rects_overlap((pos.0, pos.1, 80, 30), placed[0]));
    }

    #[test]
    fn place_hint_clamps_within_screen() {
        assert_eq!(
            place_hint((1900, 1070), (80, 30), &[], (1920, 1080)),
            (1920 - 80, 1080 - 30)
        );
    }

    #[test]
    fn splits_trailing_and_leading_app_from_title() {
        assert_eq!(
            split_app_from_title("README.md - rightkeys - VSCodium", "VSCodium"),
            ("VSCodium".to_string(), "README.md - rightkeys".to_string())
        );
        assert_eq!(
            split_app_from_title("GitHub — Mozilla Firefox", "firefox"),
            ("Mozilla Firefox".to_string(), "GitHub".to_string())
        );
        assert_eq!(
            split_app_from_title("Visual Studio Code - notes.md", "Visual Studio Code"),
            ("Visual Studio Code".to_string(), "notes.md".to_string())
        );
    }

    #[test]
    fn splits_multi_segment_app_brand_via_tokens() {
        assert_eq!(
            split_app_from_title(
                "[Extension Development Host] all_users [dir] - ZKDual - Visual Studio Code - Insiders",
                "code-insiders",
            ),
            (
                "Visual Studio Code - Insiders".to_string(),
                "[Extension Development Host] all_users [dir] - ZKDual".to_string()
            )
        );
    }

    #[test]
    fn split_app_yields_empty_brand_when_nothing_matches_or_would_empty() {
        assert_eq!(
            split_app_from_title("trung@pc: ~/ws", "Xfce4-terminal"),
            (String::new(), "trung@pc: ~/ws".to_string())
        );
        assert_eq!(
            split_app_from_title("VSCodium", "VSCodium"),
            (String::new(), "VSCodium".to_string())
        );
        assert_eq!(
            split_app_from_title("Some Doc - Thing", ""),
            (String::new(), "Some Doc - Thing".to_string())
        );
    }
}
