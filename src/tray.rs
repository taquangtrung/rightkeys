//! Optional system-tray icon for Linux and Windows. It runs on its own thread
//! with its own event loop (a GTK loop on Linux, a Win32 message pump on
//! Windows), so the backend hot paths stay untouched. The tray is best-effort:
//! if it cannot be created (no display, no indicator host) the remapper keeps
//! running without it.
//!
//! The menu exposes an "Enabled" toggle (a shared flag the backends consult on
//! every key event), an on-demand "Reload config", and "Quit".

#[cfg(any(target_os = "linux", windows))]
pub use imp::{is_enabled, spawn};

/// Spawn the tray icon. No-op on platforms without tray support.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn spawn() {}

#[cfg(any(target_os = "linux", windows))]
mod imp {
    // Imports

    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, Ordering};

    use anyhow::Result;
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

    // Constants

    /// Embedded 32x32 RGBA icon shown in the tray.
    const ICON_PNG: &[u8] = include_bytes!("../assets/icons/rightkeys-32.png");

    /// Hover text for the tray icon, by remapping state.
    const TOOLTIP_ON: &str = "RightKeys: remapping on";
    const TOOLTIP_OFF: &str = "RightKeys: remapping paused";

    /// Stable menu-item ids, matched in the event handler.
    const ID_ENABLE: &str = "enable";
    const ID_RELOAD: &str = "reload";
    const ID_QUIT: &str = "quit";

    // State

    /// Whether remapping is active. Backends forward keys untouched when `false`.
    static ENABLED: AtomicBool = AtomicBool::new(true);

    thread_local! {
        /// The live tray icon, kept on the tray thread so its tooltip can be
        /// refreshed when remapping is toggled.
        static TRAY: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
    }

    // Functions

    /// Whether remapping is currently enabled.
    pub fn is_enabled() -> bool {
        ENABLED.load(Ordering::Relaxed)
    }

    /// Spawn the tray on a dedicated thread that owns the platform event loop.
    pub fn spawn() {
        std::thread::spawn(|| {
            if !init_event_loop() {
                log::warn!("tray: could not start the platform event loop; running without a tray");
                return;
            }
            match build_tray() {
                Ok(tray) => TRAY.with(|cell| *cell.borrow_mut() = Some(tray)),
                Err(error) => {
                    log::warn!("tray: could not create the tray icon: {error:#}");
                    return;
                }
            }
            MenuEvent::set_event_handler(Some(on_menu_event));
            run_event_loop();
        });
    }

    fn on_menu_event(event: MenuEvent) {
        if event.id == ID_ENABLE {
            let enabled = !is_enabled();
            ENABLED.store(enabled, Ordering::Relaxed);
            TRAY.with(|cell| {
                if let Some(tray) = cell.borrow().as_ref() {
                    let status = tooltip(enabled);
                    // Windows shows the tooltip; Linux (AppIndicator) shows the title.
                    let _ = tray.set_tooltip(Some(status));
                    tray.set_title(Some(status));
                }
            });
            crate::notify::info(if enabled {
                "RightKeys enabled"
            } else {
                "RightKeys disabled"
            });
        } else if event.id == ID_RELOAD {
            crate::reload::reload_now();
        } else if event.id == ID_QUIT {
            std::process::exit(0);
        }
    }

    /// Tooltip text for the given remapping state.
    fn tooltip(enabled: bool) -> &'static str {
        if enabled {
            TOOLTIP_ON
        } else {
            TOOLTIP_OFF
        }
    }

    fn build_tray() -> Result<TrayIcon> {
        let enabled = CheckMenuItem::with_id(ID_ENABLE, "Enabled", true, is_enabled(), None);
        let reload = MenuItem::with_id(ID_RELOAD, "Reload config", true, None);
        let quit = MenuItem::with_id(ID_QUIT, "Quit", true, None);
        let menu = Menu::new();
        menu.append(&enabled)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&reload)?;
        menu.append(&quit)?;

        let status = tooltip(is_enabled());
        let mut builder = TrayIconBuilder::new()
            .with_tooltip(status)
            .with_title(status)
            .with_menu(Box::new(menu));
        if let Some(icon) = load_icon() {
            builder = builder.with_icon(icon);
        }
        Ok(builder.build()?)
    }

    fn load_icon() -> Option<Icon> {
        let (rgba, width, height) = decode_rgba(ICON_PNG)?;
        Icon::from_rgba(rgba, width, height).ok()
    }

    /// Decode an 8-bit RGBA PNG into raw `(pixels, width, height)`.
    fn decode_rgba(png: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = decoder.read_info().ok()?;
        let mut pixels = vec![0; reader.output_buffer_size()?];
        let frame = reader.next_frame(&mut pixels).ok()?;
        if frame.color_type != png::ColorType::Rgba || frame.bit_depth != png::BitDepth::Eight {
            return None;
        }
        pixels.truncate(frame.buffer_size());
        Some((pixels, frame.width, frame.height))
    }

    #[cfg(target_os = "linux")]
    fn init_event_loop() -> bool {
        gtk::init().is_ok()
    }

    #[cfg(target_os = "linux")]
    fn run_event_loop() {
        gtk::main();
    }

    #[cfg(windows)]
    fn init_event_loop() -> bool {
        true
    }

    #[cfg(windows)]
    fn run_event_loop() {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, GetMessageW, TranslateMessage, MSG,
        };
        unsafe {
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    // Tests

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn decodes_embedded_icon() {
            let (rgba, width, height) = decode_rgba(ICON_PNG).expect("icon decodes");
            assert_eq!((width, height), (32, 32));
            assert_eq!(rgba.len(), (width * height * 4) as usize);
        }
    }
}
