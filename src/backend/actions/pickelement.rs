//! Portable pick-element overlay logic shared by the platform backends: the
//! target element type, hint label generation, key→hint mapping, the prefix
//! navigator session, and position deduplication. Element enumeration and
//! activation, plus on-screen rendering of the hint chips, stay per-backend.

use crate::key::Key;

// Constants

/// Character pool for generating hint labels (home-row keys first).
pub const HINT_CHARS: &str = "asdfghjklqwertyuiopzxcvbnm";

/// Two elements collapse into one hint when their overlap covers at least
/// `1/DEDUP_OVERLAP_DENOM` of the candidate's bounding-box area.
const DEDUP_OVERLAP_DENOM: i64 = 2;

// Data Structures

/// A single interactive UI element collected by the platform enumerator.
#[derive(Clone, Debug)]
pub struct Element {
    pub height: i32,
    pub label: String,
    pub width: i32,
    pub x: i32,
    pub y: i32,
    /// D-Bus bus name of the AT-SPI application (Linux only).
    #[cfg(target_os = "linux")]
    pub bus_name: String,
    /// AT-SPI object path of this element (Linux only).
    #[cfg(target_os = "linux")]
    pub node_path: String,
    /// AT-SPI role of this element (Linux only).
    #[cfg(target_os = "linux")]
    pub role: atspi::Role,
}

/// Result returned by [`HintSession::process_key`].
pub enum HintAction {
    /// A prefix character was added or removed; callers should update overlay visibility.
    Updated,
    /// A unique hint was matched; the caller should activate this element and close the overlay.
    Activate(Element),
    /// Esc was pressed; the caller should close the overlay.
    Dismiss,
    /// Key had no effect (non-hint key while waiting); caller should suppress it.
    Suppressed,
}

/// One element paired with its generated hint label.
struct Hint {
    element: Element,
    label: String,
    matched: bool,
}

/// Active hint session: holds the current element set and tracks the typed prefix.
///
/// Created when enumeration completes (via [`HintSession::new`]). The caller
/// routes each subsequent key press through [`HintSession::process_key`] until
/// the session returns [`HintAction::Activate`] or [`HintAction::Dismiss`].
pub struct HintSession {
    hints: Vec<Hint>,
    hint_prefix: String,
}

// === HintSession ===

impl HintSession {
    /// Build a new session from raw enumerated elements.
    ///
    /// Deduplicates overlapping elements, generates labels, and returns the
    /// session together with the `(element, label)` pairs the backend should render.
    pub fn new(elements: Vec<Element>, hint_chars: &str) -> (Self, Vec<(Element, String)>) {
        let elements = deduplicate(elements);
        let labels = generate_labels(elements.len(), hint_chars);
        let pairs: Vec<(Element, String)> = elements.into_iter().zip(labels).collect();
        let hints = pairs
            .iter()
            .map(|(e, l)| Hint {
                element: e.clone(),
                label: l.clone(),
                matched: true,
            })
            .collect();
        (HintSession { hints, hint_prefix: String::new() }, pairs)
    }

    /// Process a single key press. Returns the action the caller should perform.
    pub fn process_key(&mut self, key: Key) -> HintAction {
        if key == Key::Esc {
            return HintAction::Dismiss;
        }
        if key == Key::Backspace {
            self.hint_prefix.pop();
            self.refilter();
            return HintAction::Updated;
        }
        if let Some(c) = key_to_char(key) {
            self.hint_prefix.push(c);
            self.refilter();
            if let Some(idx) = self.unique_match() {
                let element = self.hints[idx].element.clone();
                return HintAction::Activate(element);
            }
            return HintAction::Updated;
        }
        HintAction::Suppressed
    }

    /// Iterate over all hints with their current visibility state.
    ///
    /// Used by the backend to show/hide overlay chips after each keystroke.
    pub fn matched_hints(&self) -> impl Iterator<Item = (&Element, &str, bool)> {
        self.hints.iter().map(|h| (&h.element, h.label.as_str(), h.matched))
    }

    fn refilter(&mut self) {
        let prefix = self.hint_prefix.clone();
        for h in &mut self.hints {
            h.matched = h.label.starts_with(prefix.as_str());
        }
    }

    fn unique_match(&self) -> Option<usize> {
        let mut found: Option<usize> = None;
        for (i, h) in self.hints.iter().enumerate() {
            if h.matched {
                if found.is_some() {
                    return None;
                }
                found = Some(i);
            }
        }
        found
    }
}

// Functions

/// Map a letter `Key` to its lowercase `char`, for hint prefix matching.
fn key_to_char(key: Key) -> Option<char> {
    let c = match key {
        Key::A => 'a', Key::B => 'b', Key::C => 'c', Key::D => 'd',
        Key::E => 'e', Key::F => 'f', Key::G => 'g', Key::H => 'h',
        Key::I => 'i', Key::J => 'j', Key::K => 'k', Key::L => 'l',
        Key::M => 'm', Key::N => 'n', Key::O => 'o', Key::P => 'p',
        Key::Q => 'q', Key::R => 'r', Key::S => 's', Key::T => 't',
        Key::U => 'u', Key::V => 'v', Key::W => 'w', Key::X => 'x',
        Key::Y => 'y', Key::Z => 'z',
        _ => return None,
    };
    Some(c)
}

/// Generate `n` unique, ordered hint labels from `chars`.
///
/// Returns single-character labels while `n` fits within the pool; grows to
/// two-character labels beyond that.
fn generate_labels(n: usize, chars: &str) -> Vec<String> {
    let pool: Vec<char> = chars.chars().collect();
    if pool.is_empty() {
        return vec![String::new(); n];
    }
    if n <= pool.len() {
        return pool[..n].iter().map(|c| c.to_string()).collect();
    }
    let mut labels = Vec::with_capacity(n);
    'outer: for &a in &pool {
        for &b in &pool {
            labels.push(format!("{a}{b}"));
            if labels.len() == n {
                break 'outer;
            }
        }
    }
    labels
}

// Position deduplication
//
// AT-SPI exposes a single visible widget as a stack of nested accessible
// objects (a list cell plus its icon plus its text label), all occupying
// overlapping screen space. `deduplicate` collapses each such cluster into a
// single element so the overlay shows one hint per visible target.
//
// Elements are inspected in the order yielded by the AT-SPI walk (parents
// before children). A candidate is dropped when its bounding box overlaps an
// already-accepted element by at least `1/DEDUP_OVERLAP_DENOM` of its own area.

/// Collapse overlapping elements into one hint each.
fn deduplicate(elements: Vec<Element>) -> Vec<Element> {
    let mut kept: Vec<Element> = Vec::with_capacity(elements.len());
    for candidate in elements {
        let candidate_area = area(&candidate);
        let mut duplicate = false;
        for k in &kept {
            if candidate_area > 0
                && overlap_area(&candidate, k) * DEDUP_OVERLAP_DENOM >= candidate_area
            {
                duplicate = true;
                break;
            }
        }
        if !duplicate {
            kept.push(candidate);
        }
    }
    kept
}

/// Bounding-box area of an element in square pixels.
fn area(e: &Element) -> i64 {
    e.width as i64 * e.height as i64
}

/// Overlapping area of two elements' bounding boxes in square pixels.
fn overlap_area(a: &Element, b: &Element) -> i64 {
    let x_overlap = (a.x + a.width).min(b.x + b.width) - a.x.max(b.x);
    let y_overlap = (a.y + a.height).min(b.y + b.height) - a.y.max(b.y);
    if x_overlap > 0 && y_overlap > 0 {
        x_overlap as i64 * y_overlap as i64
    } else {
        0
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn make_element(label: &str, x: i32, y: i32, width: i32, height: i32) -> Element {
        Element {
            height,
            label: label.to_owned(),
            width,
            x,
            y,
            #[cfg(target_os = "linux")]
            bus_name: String::new(),
            #[cfg(target_os = "linux")]
            node_path: "/stub".to_owned(),
            #[cfg(target_os = "linux")]
            role: atspi::Role::PushButton,
        }
    }

    #[test]
    fn element_clone_preserves_label() {
        let e = make_element("button", 10, 10, 80, 20);
        assert_eq!(e.clone().label, "button");
    }

    #[test]
    fn generate_labels_single_char_when_few() {
        let labels = generate_labels(3, "asdf");
        assert_eq!(labels, &["a", "s", "d"]);
    }

    #[test]
    fn generate_labels_two_char_beyond_pool() {
        let labels = generate_labels(4, "ab");
        assert_eq!(labels.len(), 4);
        assert!(labels.iter().all(|l| l.len() == 2));
    }

    #[test]
    fn generate_labels_empty_pool_returns_empty_strings() {
        let labels = generate_labels(3, "");
        assert_eq!(labels, &["", "", ""]);
    }

    #[test]
    fn nested_child_inside_parent_is_dropped() {
        let parent = make_element("cell", 0, 0, 100, 100);
        let child = make_element("icon", 40, 40, 20, 20);
        let result = deduplicate(vec![parent, child]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].label, "cell");
    }

    #[test]
    fn disjoint_elements_are_kept() {
        let a = make_element("a", 0, 0, 50, 50);
        let b = make_element("b", 200, 200, 50, 50);
        let result = deduplicate(vec![a, b]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn low_overlap_neighbours_are_kept() {
        // 50x50 boxes overlapping only 10x10 — far below half of either.
        let a = make_element("a", 0, 0, 50, 50);
        let b = make_element("b", 40, 40, 50, 50);
        let result = deduplicate(vec![a, b]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn mostly_overlapping_neighbour_is_dropped() {
        // Second box covers more than half of the first — collapse to one.
        let a = make_element("a", 0, 0, 50, 50);
        let b = make_element("b", 10, 10, 50, 50);
        let result = deduplicate(vec![a, b]);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(deduplicate(vec![]).is_empty());
    }

    #[test]
    fn overlap_area_and_area_geometry() {
        let a = make_element("a", 0, 0, 100, 100);
        let b = make_element("b", 50, 50, 100, 100);
        assert_eq!(area(&a), 100 * 100);
        assert_eq!(overlap_area(&a, &b), 50 * 50);
        assert_eq!(overlap_area(&a, &a), 100 * 100);
    }
}
