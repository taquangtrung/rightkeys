//! Platform backends: the per-OS keyboard capture/injection layer plus the
//! active-window watcher. The portable [`Engine`](crate::engine::Engine) sits
//! above this; each backend owns its own event loop and feeds the engine.

// Imports

use anyhow::Result;

use crate::engine::Engine;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

// Data Structures

/// Runtime options passed from the CLI to the active backend.
#[derive(Debug, Default)]
pub struct Options {
    /// Explicit device paths/names to grab (Linux). Empty means auto-detect.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub devices: Vec<String>,

    /// Replace an already-running instance instead of refusing to start.
    #[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
    pub force: bool,
}

// Traits

/// Reports the active application's identifier (X11 `WM_CLASS`, Windows process
/// name, ...) used to scope keymaps.
pub trait WindowWatcher {
    fn active_app(&mut self) -> String;
}

// Functions

/// Run the remapper using the platform backend. Blocks until interrupted.
pub fn run(engine: Engine, options: Options) -> Result<()> {
    #[cfg(target_os = "linux")]
    let result = linux::run(engine, options);
    #[cfg(windows)]
    let result = windows::run(engine, options);
    #[cfg(not(any(target_os = "linux", windows)))]
    let result = {
        let _ = (engine, options);
        Err(anyhow::anyhow!(
            "RightKeys supports only Linux (X11) and Windows so far"
        ))
    };
    result
}

/// List candidate keyboard devices (Linux only).
#[cfg(target_os = "linux")]
pub fn list_devices() -> Result<()> {
    linux::list_devices()
}
