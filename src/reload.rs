//! Live config reload: a background thread polls the config file and, when it
//! changes, re-parses it. A new config is published only if it parses and
//! lowers cleanly; an invalid edit is logged and the running config is kept, so
//! a typo can never strand the keyboard.
//!
//! The reloaded config is handed off through a global mailbox that each backend
//! drains on its own thread (the event loop on Linux, the hook thread on
//! Windows), so the swap always happens where the engine lives.

// Imports

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, SystemTime};

use crate::config;
use crate::engine::Config;

// Constants

const POLL_INTERVAL: Duration = Duration::from_secs(1);

// State

/// A validated config waiting to be applied by a backend.
static PENDING: Mutex<Option<Config>> = Mutex::new(None);

/// The watched config path, so an on-demand reload can re-read it.
static CONFIG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

// Functions

/// Spawn a background thread that watches `path` and publishes a validated
/// config whenever the file changes.
pub fn watch(path: PathBuf) {
    if let Ok(mut slot) = CONFIG_PATH.lock() {
        *slot = Some(path.clone());
    }
    thread::spawn(move || {
        let mut last = modified_time(&path);
        loop {
            thread::sleep(POLL_INTERVAL);
            let current = modified_time(&path);
            match current {
                Some(time) if Some(time) != last => {
                    last = Some(time);
                    publish(&path);
                }
                _ => {}
            }
        }
    });
}

/// Take the pending reloaded config, if one is ready.
pub fn take() -> Option<Config> {
    PENDING.lock().ok().and_then(|mut pending| pending.take())
}

/// Re-read the config from disk on demand (e.g. the tray's "Reload config"),
/// publishing it if it still parses. The running config is kept on error.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
pub fn reload_now() {
    let path = CONFIG_PATH.lock().ok().and_then(|slot| slot.clone());
    if let Some(path) = path {
        publish(&path);
    }
}

fn publish(path: &Path) {
    match config::load(path) {
        Ok(config) => {
            log::info!("config reloaded from {}", path.display());
            if let Ok(mut pending) = PENDING.lock() {
                *pending = Some(config);
            }
        }
        Err(error) => {
            log::warn!("config reload failed, keeping current config: {error:#}");
            crate::notify::warn("Invalid config, keeping current settings");
        }
    }
}

fn modified_time(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|meta| meta.modified()).ok()
}
