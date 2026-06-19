//! rightkeys: a cross-platform, KDL-configured key remapper.

// On Windows, build as a GUI-subsystem binary so launching it (tray icon,
// autostart, Start menu) shows no console window; it just runs in the
// background. A parent console is reattached at startup when one exists, so
// CLI use (`--help`, `--debug`) still prints normally from a terminal.
#![cfg_attr(windows, windows_subsystem = "windows")]

// Modules

mod backend;
mod config;
mod engine;
mod key;
mod notify;
mod reload;
mod tray;

// Imports

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Error, Result};
use clap::Parser;
use log::Level;

use crate::engine::Engine;

// Constants

/// ANSI escapes used to emphasise a fatal error on a terminal.
const BOLD_RED: &str = "\x1b[1;31m";
const RESET: &str = "\x1b[0m";

// Data Structures

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(name = "rightkeys", about = "Cross-platform key remapper (KDL config)")]
struct Cli {
    /// Path to the KDL config file. Defaults to the per-user config location.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Device path or name to grab (Linux, repeatable). Auto-detects if unset.
    #[arg(long = "device")]
    devices: Vec<String>,

    /// List candidate keyboard devices and exit (Linux only).
    #[arg(long)]
    list_devices: bool,

    /// Replace any already-running rightkeys instance instead of refusing to start.
    #[arg(short, long)]
    force: bool,

    /// Print each pressed key and its translation (enables debug logging).
    #[arg(short, long)]
    debug: bool,
}

// Functions

fn main() -> ExitCode {
    #[cfg(windows)]
    attach_parent_console();
    if let Err(error) = run() {
        report_error(&error);
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Reattach to the launching process's console, if any, so `stdout`/`stderr`
/// reach the terminal for CLI use. The `windows` subsystem otherwise gives the
/// process no console; this is a best-effort no-op when launched without one
/// (tray, autostart), leaving such failures to the desktop notification path.
#[cfg(windows)]
fn attach_parent_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // SAFETY: a plain FFI call; the error (no parent console) is ignored.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Report a fatal error: print it to stderr (bold red on a terminal, plain when
/// redirected) and raise a desktop notification, so a failure is visible even
/// when RightKeys was launched without a console (autostart, a hotkey, the tray).
fn report_error(error: &Error) {
    let message = format!("{error:#}");
    if std::io::stderr().is_terminal() {
        eprintln!("\n{BOLD_RED}Error:{RESET} {message}");
    } else {
        eprintln!("Error: {message}");
    }
    notify::warn(&format!("RightKeys error: {message}"));
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let default_filter = if cli.debug { "rightkeys=debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
        .format(|buf, record| match record.level() {
            Level::Info | Level::Debug => writeln!(buf, "{}", record.args()),
            level => writeln!(buf, "{level}: {}", record.args()),
        })
        .init();

    if cli.list_devices {
        #[cfg(target_os = "linux")]
        {
            return backend::list_devices();
        }
        #[cfg(not(target_os = "linux"))]
        {
            anyhow::bail!("--list-devices is only supported on Linux");
        }
    }

    let config_path = cli.config.map(Ok).unwrap_or_else(default_config_path)?;
    let config = config::load(&config_path)?;
    log::info!("Configuration loaded: {}", display_path(&config_path));

    let engine = Engine::new(config);
    reload::watch(config_path);
    tray::spawn();
    backend::run(
        engine,
        backend::Options {
            devices: cli.devices,
            force: cli.force,
        },
    )
}

/// The default per-user config path (`$XDG_CONFIG_HOME`/`%APPDATA%` aware).
fn default_config_path() -> Result<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .context("APPDATA is not set")?;
    #[cfg(not(windows))]
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME").context("HOME is not set")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("rightkeys").join("settings.kdl"))
}

/// Render a path for display, abbreviating the home directory as `~`.
fn display_path(path: &Path) -> String {
    match std::env::var_os("HOME") {
        Some(home) => abbreviate_home(path, Path::new(&home)),
        None => path.display().to_string(),
    }
}

/// Replace a leading `home` component of `path` with `~`, leaving other paths
/// untouched.
fn abbreviate_home(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbreviates_home_prefix() {
        assert_eq!(
            abbreviate_home(
                Path::new("/home/u/.config/rightkeys/settings.kdl"),
                Path::new("/home/u"),
            ),
            "~/.config/rightkeys/settings.kdl"
        );
    }

    #[test]
    fn leaves_non_home_paths_untouched() {
        assert_eq!(
            abbreviate_home(Path::new("/etc/rightkeys.kdl"), Path::new("/home/u")),
            "/etc/rightkeys.kdl"
        );
    }
}
