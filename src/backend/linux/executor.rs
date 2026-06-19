//! Element activation for Linux (AT-SPI + xdotool).
//!
//! Activates a selected element using the most reliable available mechanism,
//! falling back through AT-SPI actions, xdotool click, and Return key.

use std::process::Command;

use anyhow::{Context, Result};
use atspi::connection::AccessibilityConnection;
use atspi::proxy::action::ActionProxy;
use atspi::proxy::component::ComponentProxy;
use atspi::Role;
use log::{debug, warn};
use zbus::CacheProperties;

use crate::backend::actions::pickelement::Element;

// Constants

const FALLBACK_KEY: &str = "Return";
const MOUSE_BUTTON_LEFT: &str = "1";

// Functions

/// Activate `element` using the best available mechanism.
///
/// Strategy order:
/// 0. Menu items: AT-SPI `doAction(0)` (reliable under the toolkit's menu grab).
/// 1. `xdotool` real X11 button click at the element's centre.
/// 2. AT-SPI `doAction(0)` for non-menu items when xdotool is absent.
/// 3. AT-SPI `grab_focus` followed by `xdotool key Return`.
pub async fn execute(element: &Element) -> Result<()> {
    debug!(
        "executing element: label={} role={:?} x={} y={}",
        element.label, element.role, element.x, element.y
    );

    if is_menu_item(element.role) && atspi_do_action(element).await {
        return Ok(());
    }

    let cx = element.x + element.width / 2;
    let cy = element.y + element.height / 2;
    if cx >= 0 && cy >= 0 {
        if xdotool_click(cx, cy).is_ok() {
            return Ok(());
        }
    }

    if atspi_do_action(element).await {
        return Ok(());
    }

    grab_focus(element).await;
    xdotool_key(FALLBACK_KEY)
}

// Internal helpers

fn is_menu_item(role: Role) -> bool {
    matches!(role, Role::MenuItem | Role::CheckMenuItem | Role::RadioMenuItem)
}

async fn atspi_do_action(element: &Element) -> bool {
    let conn = match AccessibilityConnection::open().await {
        Ok(c) => c,
        Err(e) => {
            warn!("AT-SPI connection failed: {e}");
            return false;
        }
    };
    let bus = conn.connection().clone();
    let proxy = ActionProxy::builder(&bus)
        .destination(element.bus_name.as_str())
        .and_then(|b| b.path(element.node_path.as_str()))
        .map(|b| b.cache_properties(CacheProperties::No).build());
    let action = match proxy {
        Ok(fut) => match fut.await {
            Ok(a) => a,
            Err(e) => {
                warn!("AT-SPI Action proxy build failed: {e}");
                return false;
            }
        },
        Err(e) => {
            warn!("AT-SPI Action proxy address invalid: {e}");
            return false;
        }
    };
    match action.do_action(0).await {
        Ok(true) => {
            debug!("AT-SPI doAction(0) succeeded");
            true
        }
        Ok(false) => {
            warn!("AT-SPI doAction(0) returned false");
            false
        }
        Err(e) => {
            warn!("AT-SPI doAction(0) failed: {e}");
            false
        }
    }
}

async fn grab_focus(element: &Element) {
    let conn = match AccessibilityConnection::open().await {
        Ok(c) => c,
        Err(e) => {
            warn!("AT-SPI connection failed: {e}");
            return;
        }
    };
    let bus = conn.connection().clone();
    let proxy = ComponentProxy::builder(&bus)
        .destination(element.bus_name.as_str())
        .and_then(|b| b.path(element.node_path.as_str()))
        .map(|b| b.cache_properties(CacheProperties::No).build());
    let Ok(fut) = proxy else {
        warn!("AT-SPI Component proxy address invalid");
        return;
    };
    match fut.await {
        Ok(comp) => match comp.grab_focus().await {
            Ok(true) => debug!("AT-SPI grab_focus succeeded"),
            Ok(false) => warn!("AT-SPI grab_focus returned false"),
            Err(e) => warn!("AT-SPI grab_focus failed: {e}"),
        },
        Err(e) => warn!("AT-SPI Component proxy build failed: {e}"),
    }
}

fn xdotool_click(x: i32, y: i32) -> Result<()> {
    let status = Command::new("xdotool")
        .args(["mousemove", "--sync", &x.to_string(), &y.to_string()])
        .args(["click", MOUSE_BUTTON_LEFT])
        .args(["mousemove_relative", "--sync", "--", "1", "1"])
        .status()
        .context("xdotool not found")?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("xdotool click at ({x},{y}) failed: {status}")
    }
}

fn xdotool_key(key: &str) -> Result<()> {
    let status = Command::new("xdotool")
        .args(["key", "--clearmodifiers", key])
        .status()
        .context("xdotool not found")?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("xdotool key {key} failed: {status}")
    }
}
