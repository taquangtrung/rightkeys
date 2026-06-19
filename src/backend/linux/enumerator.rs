//! AT-SPI accessibility tree enumerator (Linux).
//!
//! Walks the AT-SPI tree of the currently focused window and collects all
//! actionable elements with their screen coordinates.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;
use atspi::connection::AccessibilityConnection;
use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::component::ComponentProxy;
use atspi::{Accessible, CoordType, Role, State, StateSet};
use log::{debug, error, warn};
use zbus::CacheProperties;

use crate::backend::actions::pickelement::Element;

// Constants

const MAX_WALK_DEPTH: u32 = 20;
const REGISTRY_DEST: &str = "org.a11y.atspi.Registry";
const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";

/// AT-SPI roles that produce hint targets.
const ACTIONABLE: &[Role] = &[
    Role::CheckBox,
    Role::CheckMenuItem,
    Role::ColumnHeader,
    Role::ComboBox,
    Role::Entry,
    Role::Icon,
    Role::Link,
    Role::ListItem,
    Role::Menu,
    Role::MenuItem,
    Role::PageTab,
    Role::PushButton,
    Role::RadioButton,
    Role::RadioMenuItem,
    Role::TableCell,
    Role::TableColumnHeader,
    Role::ToggleButton,
    Role::TreeItem,
];

// Functions

/// Enumerate actionable elements in the focused window.
///
/// `focused_pid` is the PID of the active X11 window's process; `active_bounds`
/// is its `(x, y, w, h)` screen rectangle, used to pick the right AT-SPI window
/// for multi-window applications.
///
/// Tries multiple identification strategies in reliability order.
pub async fn enumerate(
    focused_pid: Option<u32>,
    active_bounds: Option<(i32, i32, i32, i32)>,
) -> Result<Vec<Element>> {
    let conn = AccessibilityConnection::open()
        .await
        .map_err(|e| anyhow::anyhow!("AT-SPI connection failed: {e}"))?;
    let bus = conn.connection().clone();

    let apps = atspi_children(&bus, REGISTRY_DEST, ROOT_PATH)
        .await
        .map_err(|e| anyhow::anyhow!("AT-SPI desktop children: {e}"))?;

    debug!("AT-SPI applications visible: {}", apps.len());

    let mut all_windows: Vec<Accessible> = Vec::new();
    let mut pid_windows: Vec<Accessible> = Vec::new();

    for app_ref in &apps {
        if app_ref.name == REGISTRY_DEST {
            continue;
        }
        let Ok(wins) = atspi_children(&bus, &app_ref.name, app_ref.path.as_str()).await else {
            continue;
        };
        if let Some(target) = focused_pid {
            if dbus_pid(&bus, &app_ref.name).await == Some(target) {
                debug!("pass-0: PID match for {}", app_ref.name);
                pid_windows.extend(wins.clone());
            }
        }
        all_windows.extend(wins);
    }

    if !pid_windows.is_empty() {
        // Pass 0a: X11 active-window bounds match.
        if let Some(bounds) = active_bounds {
            if let Some(win) = window_at_bounds(&bus, &pid_windows, bounds).await {
                debug!("pass-0a: PID + X11 bounds match for {}", win.name);
                let elems = walk_clipped(&bus, win).await;
                if !elems.is_empty() {
                    return Ok(clipped_to_active(elems, active_bounds));
                }
            }
        }
        // Pass 0b: PID + AT-SPI Active state.
        for win in &pid_windows {
            let states = atspi_states(&bus, &win.name, win.path.as_str()).await;
            if states.contains(State::Active) {
                debug!("pass-0b: PID+Active for {}", win.name);
                let elems = walk_clipped(&bus, win.clone()).await;
                if !elems.is_empty() {
                    return Ok(clipped_to_active(elems, active_bounds));
                }
            }
        }
        // Pass 0c: PID + AT-SPI Focused state.
        for win in &pid_windows {
            let states = atspi_states(&bus, &win.name, win.path.as_str()).await;
            if states.contains(State::Focused) {
                debug!("pass-0c: PID+Focused for {}", win.name);
                let elems = walk_clipped(&bus, win.clone()).await;
                if !elems.is_empty() {
                    return Ok(clipped_to_active(elems, active_bounds));
                }
            }
        }
        // Pass 0d: all PID windows, each clipped to its own bounds.
        debug!("pass-0d: walking all PID windows clipped");
        let mut elems = Vec::new();
        for win in pid_windows {
            elems.extend(walk_clipped(&bus, win).await);
        }
        if !elems.is_empty() {
            return Ok(clipped_to_active(elems, active_bounds));
        }
    }

    // Pass 1: State::Active in any app.
    for win in &all_windows {
        let states = atspi_states(&bus, &win.name, win.path.as_str()).await;
        if states.contains(State::Active) {
            debug!("pass-1: Active window in {}", win.name);
            let elems = walk_clipped(&bus, win.clone()).await;
            if !elems.is_empty() {
                return Ok(clipped_to_active(elems, active_bounds));
            }
        }
    }
    // Pass 2: State::Focused in any app.
    for win in &all_windows {
        let states = atspi_states(&bus, &win.name, win.path.as_str()).await;
        if states.contains(State::Focused) {
            debug!("pass-2: Focused window in {}", win.name);
            let elems = walk_clipped(&bus, win.clone()).await;
            if !elems.is_empty() {
                return Ok(clipped_to_active(elems, active_bounds));
            }
        }
    }

    warn!("no focused AT-SPI window found; hint overlay will be empty");
    Ok(Vec::new())
}

// Internal helpers

async fn dbus_pid(bus: &zbus::Connection, name: &str) -> Option<u32> {
    let msg = bus
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetConnectionUnixProcessID",
            &(name,),
        )
        .await
        .ok()?;
    msg.body::<u32>().ok()
}

async fn atspi_children(
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
) -> zbus::Result<Vec<Accessible>> {
    let proxy = AccessibleProxy::builder(conn)
        .destination(dest)?
        .path(path)?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    proxy.get_children().await
}

async fn atspi_states(conn: &zbus::Connection, dest: &str, path: &str) -> StateSet {
    let Ok(proxy) = AccessibleProxy::builder(conn)
        .destination(dest)
        .and_then(|b| b.path(path))
        .map(|b| b.cache_properties(CacheProperties::No).build())
    else {
        return StateSet::empty();
    };
    match proxy.await {
        Ok(p) => p.get_state().await.unwrap_or_else(|_| StateSet::empty()),
        Err(_) => StateSet::empty(),
    }
}

async fn atspi_node_info(
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
) -> Option<(Role, String, Vec<Accessible>)> {
    let proxy = AccessibleProxy::builder(conn)
        .destination(dest)
        .and_then(|b| b.path(path))
        .map(|b| b.cache_properties(CacheProperties::No).build())
        .ok()?
        .await
        .ok()?;
    let role = proxy.get_role().await.unwrap_or(Role::Invalid);
    let label = proxy.name().await.unwrap_or_default();
    let children = proxy.get_children().await.unwrap_or_default();
    Some((role, label, children))
}

async fn atspi_extents(
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
) -> Option<(i32, i32, i32, i32)> {
    let comp = ComponentProxy::builder(conn)
        .destination(dest)
        .and_then(|b| b.path(path))
        .map(|b| b.cache_properties(CacheProperties::No).build())
        .ok()?
        .await
        .ok()?;
    comp.get_extents(CoordType::Screen).await.ok()
}

/// Walk `win` and discard elements whose origin lies outside the window's own
/// screen extents. AT-SPI still reports `Showing` for partially-overlapping and
/// scrolled-out nodes; placing a chip at such an origin would render it outside
/// the focused window (typically clamped to the screen edge, where many pile
/// up). The origin-in-rect test keeps chips on the element they label.
///
/// When the window reports zero-size extents (a known LibreOffice VCL quirk),
/// the AT-SPI clip is skipped; `clipped_to_active` will use the X11 window
/// bounds as the fallback filter.
async fn walk_clipped(conn: &zbus::Connection, win: Accessible) -> Vec<Element> {
    match atspi_extents(conn, &win.name, win.path.as_str()).await {
        None => {
            warn!("AT-SPI window '{}' has no screen extents; skipping", win.name);
            Vec::new()
        }
        Some((_, _, ww, wh)) if ww <= 0 || wh <= 0 => {
            debug!(
                "walk_clipped: window '{}' has zero-size extents; skipping AT-SPI clip",
                win.name
            );
            walk(0, conn.clone(), win).await
        }
        Some((wx, wy, ww, wh)) => {
            debug!("walk_clipped: window '{}' screen=({wx},{wy},{ww},{wh})", win.name);
            let elements = walk(0, conn.clone(), win).await;
            let before = elements.len();
            let result: Vec<Element> = elements
                .into_iter()
                .filter(|e| origin_within(e.x, e.y, wx, wy, ww, wh))
                .collect();
            debug!("walk_clipped: {before} elements → {} after clip", result.len());
            result
        }
    }
}

async fn window_at_bounds(
    bus: &zbus::Connection,
    windows: &[Accessible],
    target: (i32, i32, i32, i32),
) -> Option<Accessible> {
    let mut best: Option<(Accessible, i64)> = None;
    for win in windows {
        let Some(extents) = atspi_extents(bus, &win.name, win.path.as_str()).await else {
            continue;
        };
        let overlap = rect_overlap(extents, target);
        if overlap > 0 && best.as_ref().is_none_or(|(_, prev)| overlap > *prev) {
            best = Some((win.clone(), overlap));
        }
    }
    best.map(|(win, _)| win)
}

/// Whether the point `(x, y)` lies inside rectangle `(rx, ry, rw, rh)`:
/// inclusive at the top-left corner, exclusive at the bottom-right edge, so
/// abutting elements do not count as inside each other.
fn origin_within(x: i32, y: i32, rx: i32, ry: i32, rw: i32, rh: i32) -> bool {
    x >= rx && x < rx + rw && y >= ry && y < ry + rh
}

/// Drop elements whose origin falls outside the X11 active window's bounds.
/// Fallback passes in [`enumerate`] can walk AT-SPI windows other than the
/// focused X11 one (other windows of a multi-window app, off-screen popups);
/// without this filter their chips would render at off-window screen coords.
fn clipped_to_active(
    mut elems: Vec<Element>,
    active_bounds: Option<(i32, i32, i32, i32)>,
) -> Vec<Element> {
    if let Some((bx, by, bw, bh)) = active_bounds {
        let before = elems.len();
        elems.retain(|e| origin_within(e.x, e.y, bx, by, bw, bh));
        debug!(
            "clipped_to_active: active=({bx},{by},{bw},{bh}), {before} → {} after filter",
            elems.len()
        );
    }
    for e in &elems {
        debug!(
            "  hint target: role={:?} label={:?} at ({},{},{},{})",
            e.role, e.label, e.x, e.y, e.width, e.height
        );
    }
    elems
}

fn rect_overlap(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> i64 {
    let x_overlap = (a.0 + a.2).min(b.0 + b.2) - a.0.max(b.0);
    let y_overlap = (a.1 + a.3).min(b.1 + b.3) - a.1.max(b.1);
    if x_overlap > 0 && y_overlap > 0 {
        x_overlap as i64 * y_overlap as i64
    } else {
        0
    }
}

pub(crate) fn walk(
    depth: u32,
    conn: zbus::Connection,
    node: Accessible,
) -> Pin<Box<dyn Future<Output = Vec<Element>> + Send>> {
    Box::pin(async move {
        if depth > MAX_WALK_DEPTH {
            error!("AT-SPI walk exceeded max depth; pruning branch");
            return Vec::new();
        }

        let dest = node.name.clone();
        let path = node.path.to_string();

        let Some((role, label, children)) = atspi_node_info(&conn, &dest, &path).await else {
            debug!("skipping inaccessible node {dest} {path}");
            return Vec::new();
        };

        let mut result = Vec::new();

        if ACTIONABLE.contains(&role) {
            // Accept Showing (standard) OR Visible (LibreOffice VCL sets Visible
            // but not Showing on toolbar buttons and other interactive elements).
            let state = atspi_states(&conn, &dest, &path).await;
            if state.contains(State::Showing) || state.contains(State::Visible) {
                if let Some((x, y, width, height)) = atspi_extents(&conn, &dest, &path).await {
                    if width > 0 && height > 0 && x >= 0 && y >= 0 {
                        result.push(Element {
                            bus_name: dest.clone(),
                            height,
                            label,
                            node_path: path.clone(),
                            role,
                            width,
                            x,
                            y,
                        });
                    }
                }
            }
        }

        for child in children {
            result.extend(walk(depth + 1, conn.clone(), child).await);
        }
        result
    })
}

// Tests

#[cfg(test)]
mod tests {
    use super::{origin_within, rect_overlap};

    #[test]
    fn rect_overlap_disjoint_is_zero() {
        assert_eq!(rect_overlap((0, 0, 10, 10), (100, 100, 10, 10)), 0);
    }

    #[test]
    fn rect_overlap_partial_is_intersection() {
        assert_eq!(rect_overlap((0, 0, 10, 10), (5, 5, 10, 10)), 25);
    }

    #[test]
    fn rect_overlap_edge_touching_is_zero() {
        assert_eq!(rect_overlap((0, 0, 10, 10), (10, 0, 10, 10)), 0);
    }

    #[test]
    fn origin_within_top_left_corner_is_inside() {
        assert!(origin_within(0, 0, 0, 0, 100, 100));
    }

    #[test]
    fn origin_within_bottom_right_edge_is_outside() {
        // Exclusive at the far edge: abutting elements don't count as inside.
        assert!(!origin_within(100, 100, 0, 0, 100, 100));
        assert!(origin_within(99, 99, 0, 0, 100, 100));
    }

    #[test]
    fn origin_within_negative_origin_is_outside() {
        // The pile-at-left-edge bug: elements reported by AT-SPI with origins
        // at negative x (clamped to 0 by the renderer) must be filtered out.
        assert!(!origin_within(-1, 50, 0, 0, 100, 100));
        assert!(!origin_within(50, -1, 0, 0, 100, 100));
    }
}
