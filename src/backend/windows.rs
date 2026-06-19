//! Windows backend: a low-level keyboard hook (`WH_KEYBOARD_LL`) captures and
//! suppresses keys, `SendInput` injects the engine's output, and the foreground
//! window's process name scopes keymaps.
//!
//! The hook callback is global, so engine state lives in a thread-local owned by
//! the message-pump thread. Injected events carry [`INJECT_MARKER`] in their
//! `dwExtraInfo` so the hook passes its own output straight through.

// Imports

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::mem::size_of;
use std::path::Path;

use anyhow::{Context, Result};
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, BOOL, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, EndPaint, EnumDisplayMonitors, FillRect,
    FrameRect, GetDC, GetMonitorInfoW, GetTextExtentPoint32W, MonitorFromWindow, ReleaseDC,
    SelectObject, SetBkMode, SetTextColor, TextOutW, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, FF_DONTCARE, FW_BOLD, FW_NORMAL, HDC, HFONT, HGDIOBJ, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, OUT_TT_PRECIS, PAINTSTRUCT, TRANSPARENT, VARIABLE_PITCH,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCondition, IUIAutomationElement,
    IUIAutomationElementArray, TreeScope_Descendants, UIA_ButtonControlTypeId,
    UIA_CheckBoxControlTypeId, UIA_ComboBoxControlTypeId, UIA_EditControlTypeId,
    UIA_HeaderItemControlTypeId, UIA_HyperlinkControlTypeId, UIA_ListItemControlTypeId,
    UIA_MenuItemControlTypeId, UIA_RadioButtonControlTypeId, UIA_SpinnerControlTypeId,
    UIA_TabItemControlTypeId, UIA_ToggleButtonControlTypeId, UIA_TreeItemControlTypeId,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId, OpenProcess,
    QueryFullProcessImageNameW, TerminateProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows,
    GetForegroundWindow, GetMessageW, GetSystemMetrics, GetWindow, GetWindowLongPtrW, GetWindowLongW,
    GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, IsZoomed, KillTimer, RegisterClassExW, SetForegroundWindow, SetTimer,
    SetWindowLongPtrW, SetWindowPos, SetWindowsHookExW, ShowWindow, TranslateMessage,
    PostThreadMessageW, UnhookWindowsHookEx, GWLP_USERDATA, GWL_EXSTYLE, GW_OWNER, HC_ACTION,
    HHOOK, HWND_NOTOPMOST, HWND_TOPMOST, KBDLLHOOKSTRUCT, MSG, SM_CXSCREEN, SM_CYSCREEN,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_MAXIMIZE, SW_MINIMIZE,
    SW_RESTORE, SW_SHOWNA, SW_SHOWNORMAL, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_KEYUP,
    WM_NCDESTROY, WM_PAINT, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WNDCLASSEXW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use super::actions::pickwindow::{
    advance, key_to_hint_char, make_hints, place_hint, split_app_from_title, HintMatch,
};
use super::{Options, WindowWatcher};
use crate::engine::{Corner, CycleDirection, Effect, Engine, OutEvent, Side, WindowAction, Workspace, STEP_DIVISOR};
use crate::key::Key;
use super::actions::pickelement::{Element, HintAction, HintSession, HINT_CHARS};

// Constants

/// Tag stamped into `dwExtraInfo` of injected events so the hook ignores them.
const INJECT_MARKER: usize = 0x5249_4748; // "RIGH"

/// Thread message: UIA element enumeration completed.
/// `wParam` holds a `Box<Vec<Element>>` raw pointer.
const WM_PICK_ELEMENT: u32 = WM_APP + 3;

/// Buffer length for process image paths.
const PATH_BUF_LEN: usize = 512;

/// Milliseconds a tap-hold key may be held with no other key before committing
/// to its hold modifier, so the modifier reaches the OS in time for a mouse
/// click (which never reaches the hook), e.g. Shift/Ctrl-click multi-select.
const TAP_HOLD_TIMEOUT_MS: u32 = 200;

/// Window class name for the pick-window hint chips.
const HINT_CLASS: PCWSTR = w!("RightKeysHint");

/// System UI font family used for the hint chips.
const HINT_FACE: PCWSTR = w!("Segoe UI");

/// Pixel heights of the three chip fonts: the hint key, the app name, and the
/// smaller window-title line.
const HINT_FONT_PX: i32 = 24;
const APP_FONT_PX: i32 = 18;
const INFO_FONT_PX: i32 = 15;

/// Chip padding (pixels): inside the hint chip, inside the info chip, the
/// vertical padding around the text, and the gap between the two info lines.
const HINT_CHIP_PAD: i32 = 9;
const APP_CHIP_PAD: i32 = 11;
const OVERLAY_VPAD: i32 = 5;
const INFO_LINE_GAP: i32 = 2;

/// Chip colors. `0x00BBGGRR` packed for `COLORREF` (Win32's byte order).
const HINT_BG: COLORREF = rgb(0x17, 0x25, 0x54);
const HINT_FG: COLORREF = rgb(0x38, 0xbd, 0xf8);
const HINT_BORDER: COLORREF = rgb(0x60, 0xa5, 0xfa);
const APP_BG: COLORREF = rgb(0x23, 0x48, 0x7a);
const APP_FG: COLORREF = rgb(0xe8, 0xee, 0xf6);
const TITLE_FG: COLORREF = rgb(0xc4, 0xcf, 0xde);

/// Pack `(r, g, b)` into a `COLORREF` (`0x00BBGGRR`).
const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}

// Data Structures

/// Engine plus window watcher, owned by the hook thread.
struct State {
    engine: Engine,
    watcher: ForegroundWatcher,
    /// Active standalone hint session (triggered by the `pick-element` action).
    hint_session: Option<HintSession>,
    /// True while UIA enumeration for a `pick-element` action is in flight.
    standalone_hints_pending: bool,
}

/// One element hint chip in the find-element overlay.
struct ElementEntry {
    overlay: HWND,
    element: Element,
}

/// Active state of the element-hint overlay (pick-element mode).
struct PickElement {
    entries: Vec<ElementEntry>,
    hints: Vec<String>,
}

/// Foreground-window watcher with a one-entry cache keyed on the window handle.
#[derive(Default)]
struct ForegroundWatcher {
    cached_hwnd: isize,
    cached_app: String,
}

/// One hint chip bound to a target window. Its hint label lives at the same
/// index in [`PickWindow::hints`].
struct HintEntry {
    overlay: HWND,
    target: HWND,
}

/// Active state of the Vimium-style window-finder overlay.
struct PickWindow {
    entries: Vec<HintEntry>,
    hints: Vec<String>,
    prefix: String,
}

/// The three chip fonts, created once per overlay and reused while it is up.
#[derive(Clone, Copy)]
struct OverlayFonts {
    hint: HFONT,
    app: HFONT,
    info: HFONT,
}

/// Per-chip data stored behind a window's `GWLP_USERDATA`, read by `WM_PAINT`.
/// Strings are UTF-16 without a trailing NUL (as `TextOutW` wants them).
struct ChipData {
    hint: Vec<u16>,
    app: Vec<u16>,
    title: Vec<u16>,
    hint_chip_w: i32,
    width: i32,
    height: i32,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static HOOK: Cell<HHOOK> = const { Cell::new(HHOOK(std::ptr::null_mut())) };
    /// Identifier of the active tap-hold timeout timer, or `0` when none is set.
    static TAP_HOLD_TIMER: Cell<usize> = const { Cell::new(0) };
    /// The live pick-window overlay, or `None` when it is not showing.
    static PICK_WINDOW: RefCell<Option<PickWindow>> = const { RefCell::new(None) };
    /// The live find-element overlay (pick-element), or `None` when not showing.
    static PICK_ELEMENT: RefCell<Option<PickElement>> = const { RefCell::new(None) };
    /// Fonts owned by the live overlay, freed when it tears down.
    static OVERLAY_FONTS: Cell<Option<OverlayFonts>> = const { Cell::new(None) };
    /// Whether the hint window class has been registered (once per process).
    static HINT_CLASS_REGISTERED: Cell<bool> = const { Cell::new(false) };
}

// === ForegroundWatcher ===

impl WindowWatcher for ForegroundWatcher {
    fn active_app(&mut self) -> String {
        let hwnd = unsafe { GetForegroundWindow() };
        let handle = hwnd.0 as isize;
        if handle != self.cached_hwnd {
            self.cached_hwnd = handle;
            self.cached_app = process_name(hwnd).unwrap_or_default();
        }
        self.cached_app.clone()
    }
}

// Functions

/// Install the keyboard hook and pump messages until interrupted.
pub fn run(engine: Engine, options: Options) -> Result<()> {
    replace_or_reject(options.force)?;
    unsafe {
        // Opt into per-monitor DPI awareness so UIA element coordinates match
        // what the OS actually places on screen at high DPI.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    STATE.with(|state| {
        *state.borrow_mut() = Some(State {
            engine,
            watcher: ForegroundWatcher::default(),
            hint_session: None,
            standalone_hints_pending: false,
        });
    });

    unsafe {
        let module = GetModuleHandleW(None).context("GetModuleHandleW")?;
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), HINSTANCE(module.0), 0)
            .context("installing WH_KEYBOARD_LL hook")?;
        HOOK.with(|cell| cell.set(hook));

        log::info!("RightKeys running; press Ctrl-C to stop");
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // A tap-hold key held past the timeout: commit it to its hold
            // modifier so a mouse click (which never reaches the hook) sees it.
            if msg.message == WM_TIMER {
                if crate::tray::is_enabled() {
                    let out = STATE.with(|state| {
                        state
                            .borrow_mut()
                            .as_mut()
                            .map(|state| state.engine.flush_pending_hold())
                            .unwrap_or_default()
                    });
                    send_inputs(&out);
                }
                clear_tap_hold_timer();
            }
            // UIA element enumeration finished: build the element overlay.
            if msg.message == WM_PICK_ELEMENT {
                let ptr = msg.wParam.0 as *mut Vec<Element>;
                if !ptr.is_null() {
                    let elements = unsafe { *Box::from_raw(ptr) };
                    show_pick_element(elements);
                }
            }
            // Dispatch so the hint-overlay windows receive WM_PAINT.
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnhookWindowsHookEx(hook);
    }
    Ok(())
}

/// The low-level keyboard hook callback.
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code != HC_ACTION as i32 {
        return call_next(code, wparam, lparam);
    }
    let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    if kb.dwExtraInfo == INJECT_MARKER {
        return call_next(code, wparam, lparam);
    }
    let value = match wparam.0 as u32 {
        WM_KEYDOWN | WM_SYSKEYDOWN => 1,
        WM_KEYUP | WM_SYSKEYUP => 0,
        _ => return call_next(code, wparam, lparam),
    };

    // Apply a live-reloaded config, if one is ready (borrow released before notify).
    let reloaded = STATE.with(
        |state| match (crate::reload::take(), state.borrow_mut().as_mut()) {
            (Some(config), Some(state)) => {
                state.engine.set_config(config);
                true
            }
            _ => false,
        },
    );
    if reloaded {
        crate::notify::info("RightKeys reloaded!");
    }

    // When paused from the tray, let the key reach the OS unchanged.
    if !crate::tray::is_enabled() {
        return call_next(code, wparam, lparam);
    }

    let vk = kb.vkCode as u16;

    // While the pick-window overlay is up, route keys to its navigator and
    // swallow them so nothing reaches the apps underneath.
    if PICK_WINDOW.with(|f| f.borrow().is_some()) {
        handle_pick_window_key(vk, value);
        return LRESULT(1);
    }

    let key = match Key::from_win_vk(vk) {
        Some(key) => key,
        None => {
            // Unknown key: preserve held modifiers, then inject it raw.
            let mods = STATE.with(|state| {
                state
                    .borrow_mut()
                    .as_mut()
                    .expect("state initialized in run()")
                    .engine
                    .sync_modifiers()
            });
            send_inputs(&mods);
            send_raw_vk(vk, value);
            return LRESULT(1);
        }
    };

    // A VM/remote-viewer window owns the keyboard (e.g. a nested remapper inside
    // the guest): let the key reach the OS unchanged, bypassing the engine so
    // the guest receives it raw.
    let pass_through = STATE.with(|state| {
        let mut state = state.borrow_mut();
        let state = state.as_mut().expect("state initialized in run()");
        let app = state.watcher.active_app();
        state.engine.is_pass_through(&app)
    });
    if pass_through {
        return call_next(code, wparam, lparam);
    }

    // Standalone hint session: intercept when pick-element is active.
    let hint_action = STATE.with(|state| {
        let mut state = state.borrow_mut();
        let s = state.as_mut().expect("state initialized in run()");
        if s.standalone_hints_pending {
            return Some(HintAction::Suppressed);
        }
        let hs = s.hint_session.as_mut()?;
        if value == 0 {
            return Some(HintAction::Suppressed); // suppress releases
        }
        Some(hs.process_key(key))
    });
    if let Some(action) = hint_action {
        handle_hint_action(action);
        return LRESULT(1);
    }

    // Compute output with the borrow released before SendInput re-enters the hook
    // and before effects (which call into the window manager) run.
    let (out, pending, effects) = STATE.with(|state| {
        let mut state = state.borrow_mut();
        let state = state.as_mut().expect("state initialized in run()");
        let app = state.watcher.active_app();
        let out = state.engine.on_event(key, value, &app);
        (
            out,
            state.engine.has_pending_hold(),
            state.engine.take_effects(),
        )
    });
    send_inputs(&out);
    arm_tap_hold_timer(pending);
    for effect in &effects {
        perform_effect(effect);
    }
    LRESULT(1)
}

/// Arm a one-shot tap-hold timeout when a decision is `pending`, or cancel any
/// running timer once it resolves. Re-arming is skipped while a timer is live so
/// the original deadline stands.
fn arm_tap_hold_timer(pending: bool) {
    TAP_HOLD_TIMER.with(|cell| {
        let current = cell.get();
        if pending {
            if current == 0 {
                let id = unsafe { SetTimer(None, 0, TAP_HOLD_TIMEOUT_MS, None) };
                cell.set(id);
            }
        } else if current != 0 {
            unsafe {
                let _ = KillTimer(None, current);
            }
            cell.set(0);
        }
    });
}

/// Cancel the tap-hold timeout timer, if one is running.
fn clear_tap_hold_timer() {
    TAP_HOLD_TIMER.with(|cell| {
        let id = cell.get();
        if id != 0 {
            unsafe {
                let _ = KillTimer(None, id);
            }
            cell.set(0);
        }
    });
}

fn call_next(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let hook = HOOK.with(|cell| cell.get());
    unsafe { CallNextHookEx(hook, code, wparam, lparam) }
}

fn send_inputs(events: &[OutEvent]) {
    if events.is_empty() {
        return;
    }
    let inputs: Vec<INPUT> = events
        .iter()
        .map(|event| {
            let mut flags = KEYBD_EVENT_FLAGS(0);
            if event.value == 0 {
                flags |= KEYEVENTF_KEYUP;
            }
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(event.key.win_vk()),
                        wScan: 0,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: INJECT_MARKER,
                    },
                },
            }
        })
        .collect();
    unsafe {
        SendInput(&inputs, size_of::<INPUT>() as i32);
    }
}

/// Inject a single raw virtual-key event (used for keys absent from the table).
fn send_raw_vk(vk: u16, value: i32) {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if value == 0 {
        flags |= KEYEVENTF_KEYUP;
    }
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_MARKER,
            },
        },
    };
    unsafe {
        SendInput(&[input], size_of::<INPUT>() as i32);
    }
}

/// Resolve a window's owning process to its executable stem (e.g. `firefox`).
fn process_name(hwnd: HWND) -> Option<String> {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return None;
    }
    let path = image_path(pid)?;
    Path::new(&path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
}

/// The full executable path of a process, via `QueryFullProcessImageNameW`.
fn image_path(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; PATH_BUF_LEN];
        let mut size = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        result.ok()?;
        Some(String::from_utf16_lossy(&buf[..size as usize]))
    }
}

/// Handle an already-running instance. With `force`, terminate every other copy;
/// otherwise reject the launch so two instances don't fight over the keyboard.
fn replace_or_reject(force: bool) -> Result<()> {
    let others = other_instances();
    if others.is_empty() {
        return Ok(());
    }
    if !force {
        crate::notify::info("RightKeys is already running");
        anyhow::bail!(
            "another RightKeys instance is already running.\nRun with --force to replace it"
        );
    }
    // Replace any already-running instance so a relaunch just restarts cleanly.
    for pid in others {
        terminate(pid);
    }
    log::info!("replaced a running RightKeys instance");
    crate::notify::info("RightKeys replaced a running instance");
    Ok(())
}

/// PIDs of other processes running this program.
///
/// Processes are matched on their executable's file name (case-insensitively)
/// so an instance launched from a different path (e.g. an installed copy vs a
/// freshly built `target\release\rightkeys.exe`) still counts.
fn other_instances() -> Vec<u32> {
    let self_pid = unsafe { GetCurrentProcessId() };
    let Some(self_exe) = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
    else {
        return Vec::new();
    };
    let mut pids = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return Vec::new();
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let pid = entry.th32ProcessID;
                if pid != self_pid && pid != 0 {
                    if let Some(path) = image_path(pid) {
                        if same_program(&path, &self_exe) {
                            pids.push(pid);
                        }
                    }
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    pids
}

fn terminate(pid: u32) {
    unsafe {
        if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, pid) {
            let _ = TerminateProcess(handle, 1);
            let _ = CloseHandle(handle);
        }
    }
}

/// Whether two executable image paths refer to the same program, compared on
/// the file name (case-insensitively) so an instance installed elsewhere still
/// counts.
fn same_program(a: &str, b: &str) -> bool {
    match (file_name(a), file_name(b)) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(&b),
        _ => false,
    }
}

/// The file-name component of an executable image path.
fn file_name(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

// === Effects ===

/// Perform one engine [`Effect`]: launch/activate a program, or act on the
/// foreground window. The engine emits these as intents; performing them is the
/// backend's job, just like injecting [`OutEvent`]s.
fn perform_effect(effect: &Effect) {
    match effect {
        Effect::Launch(program) => activate_or_launch(program),
        Effect::Window(action) => {
            let hwnd = unsafe { GetForegroundWindow() };
            if !hwnd.0.is_null() {
                perform_window(hwnd, *action);
            }
        }
    }
}

/// Apply a [`WindowAction`] to `hwnd`.
fn perform_window(hwnd: HWND, action: WindowAction) {
    unsafe {
        match action {
            WindowAction::Adjust { dx, dy, dw, dh } => {
                if let Some(r) = window_rect(hwnd) {
                    let x = r.left + dx;
                    let y = r.top + dy;
                    let w = (r.right - r.left + dw).max(1);
                    let h = (r.bottom - r.top + dh).max(1);
                    let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
                }
            }
            WindowAction::Preset { w, h, anchor } => {
                if let Some(area) = monitor_work_area(hwnd) {
                    let nw = ((area.right - area.left) as f64 * w) as i32;
                    let nh = ((area.bottom - area.top) as f64 * h) as i32;
                    let (x, y) = anchor_pos(area, nw, nh, anchor);
                    // Restore first so a maximized window can be sized (mirrors AHK's WinRestore).
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                    let _ = SetWindowPos(hwnd, None, x, y, nw, nh, SWP_NOZORDER | SWP_NOACTIVATE);
                }
            }
            WindowAction::Center => {
                if let (Some(r), Some(area)) = (window_rect(hwnd), monitor_work_area(hwnd)) {
                    let (w, h) = (r.right - r.left, r.bottom - r.top);
                    let (x, y) = anchor_pos(area, w, h, None);
                    let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
                }
            }
            WindowAction::Snap(corner) => {
                if let (Some(r), Some(area)) = (window_rect(hwnd), monitor_work_area(hwnd)) {
                    let (w, h) = (r.right - r.left, r.bottom - r.top);
                    let (x, y) = anchor_pos(area, w, h, Some(corner));
                    let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
                }
            }
            WindowAction::StepToward(corner) => {
                if let (Some(r), Some(area)) = (window_rect(hwnd), monitor_work_area(hwnd)) {
                    let w = r.right - r.left;
                    let h = r.bottom - r.top;
                    let (tx, ty) = anchor_pos(area, w, h, Some(corner));
                    let mw = (area.right - area.left) as f64;
                    let mh = (area.bottom - area.top) as f64;
                    let mag = mw.hypot(mh) / STEP_DIVISOR as f64;
                    let cx = r.left;
                    let cy = r.top;
                    let rdx = (tx - cx) as f64;
                    let rdy = (ty - cy) as f64;
                    let dist = rdx.hypot(rdy);
                    let (nx, ny) = if dist <= mag {
                        (tx, ty)
                    } else {
                        (cx + (rdx * mag / dist).round() as i32, cy + (rdy * mag / dist).round() as i32)
                    };
                    let _ = SetWindowPos(hwnd, None, nx, ny, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
                }
            }
            WindowAction::Corner(corner) => {
                if let Some(area) = monitor_work_area(hwnd) {
                    let mw = area.right - area.left;
                    let mh = area.bottom - area.top;
                    let w = mw / 2;
                    let h = mh / 2;
                    let x = match corner {
                        Corner::TopLeft | Corner::BottomLeft => area.left,
                        Corner::TopRight | Corner::BottomRight => area.left + mw - w,
                    };
                    let y = match corner {
                        Corner::TopLeft | Corner::TopRight => area.top,
                        Corner::BottomLeft | Corner::BottomRight => area.top + mh - h,
                    };
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                    let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
                }
            }
            WindowAction::SmartTile { side, fraction } => {
                if let Some(area) = monitor_work_area(hwnd) {
                    let mw = area.right - area.left;
                    let mh = area.bottom - area.top;
                    let tw = (mw as f64 * fraction) as i32;
                    let th = (mh as f64 * fraction) as i32;
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                    let (x, y, w, h) = match side {
                        Side::Left => (area.left, area.top, tw, mh),
                        Side::Right => (area.left + mw - tw, area.top, tw, mh),
                        Side::Top => (area.left, area.top, mw, th),
                        Side::Bottom => (area.left, area.top + mh - th, mw, th),
                    };
                    let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
                }
            }
            WindowAction::Maximize => {
                let _ = ShowWindow(hwnd, SW_MAXIMIZE);
            }
            WindowAction::MaximizeToggle => {
                let cmd = if IsZoomed(hwnd).as_bool() {
                    SW_RESTORE
                } else {
                    SW_MAXIMIZE
                };
                let _ = ShowWindow(hwnd, cmd);
            }
            WindowAction::Minimize => {
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
            }
            WindowAction::ShowDesktop => {
                // No direct API; synthesize Win+D, which toggles Show Desktop.
                // The injected events carry INJECT_MARKER, so the hook ignores them.
                send_inputs(&[
                    OutEvent { key: Key::LeftMeta, value: 1 },
                    OutEvent { key: Key::D, value: 1 },
                    OutEvent { key: Key::D, value: 0 },
                    OutEvent { key: Key::LeftMeta, value: 0 },
                ]);
            }
            WindowAction::AlwaysOnTop => {
                // Toggle the topmost (always-on-top) Z-order band.
                let topmost = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0;
                let after = if topmost { HWND_NOTOPMOST } else { HWND_TOPMOST };
                let _ = SetWindowPos(hwnd, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
            WindowAction::MoveToMonitor(direction) => move_to_monitor(hwnd, direction),
            WindowAction::Workspace {
                target,
                move_window,
            } => perform_workspace(target, move_window),
            WindowAction::CycleSameApp(direction) => {
                if let Some(stem) = process_name(hwnd) {
                    // Includes the foreground window, so we can step from its
                    // index in either direction and wrap around.
                    let same = windows_for_stem(&stem);
                    if same.len() > 1 {
                        let next = match same.iter().position(|h| h.0 == hwnd.0) {
                            Some(i) => same[direction.step(i, same.len())],
                            None => same[same.len() - 1],
                        };
                        activate(next);
                    }
                }
            }
            WindowAction::PickWindow => start_pick_window(),
            WindowAction::PickElement => {
                let tid = unsafe { GetCurrentThreadId() };
                STATE.with(|s| {
                    if let Some(s) = s.borrow_mut().as_mut() {
                        s.standalone_hints_pending = true;
                    }
                });
                spawn_uia_enum(tid);
            }
        }
    }
}

/// Activate an existing top-level window of `program`, or launch it if none is
/// open. `program` is matched on its file stem (`brave.exe` matches a `brave`
/// process), mirroring the old AHK "activate or launch" helper.
fn activate_or_launch(program: &str) {
    // Split shell-style so an argument can contain spaces when quoted, e.g.
    //   exec windows="raise-or-run.bat -w \"Google - Chrome\" -c run.bat"
    let args = match shell_words::split(program) {
        Ok(args) => args,
        Err(err) => {
            crate::notify::warn(&format!("bad exec command {program:?}: {err}"));
            return;
        }
    };
    let Some((bin, rest)) = args.split_first() else {
        return;
    };
    let stem = Path::new(bin)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| bin.clone());
    if let Some(&hwnd) = windows_for_stem(&stem).first() {
        activate(hwnd);
    } else {
        launch(bin, rest);
    }
}

/// Quote launch arguments for `ShellExecuteW`'s parameter string: wrap any
/// argument containing whitespace in double quotes so `CommandLineToArgvW`
/// re-splits it as a single argument.
fn quote_params(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg.contains(char::is_whitespace) {
                format!("\"{arg}\"")
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Launch a program by name or path. `ShellExecuteW` resolves bare names through
/// the App Paths registry and `PATH`, just as AHK's `Run` did.
///
/// NOTE: the launched process inherits this process's integrity level. If
/// RightKeys is run elevated (to capture keys from elevated windows), launched
/// apps are elevated too; launch RightKeys un-elevated, or add a shell-based
/// de-elevation step, if that matters.
fn launch(bin: &str, args: &[String]) {
    let file = to_wide(bin);
    let params = quote_params(args);
    let params_wide = to_wide(&params);
    let params_ptr = if args.is_empty() {
        PCWSTR::null()
    } else {
        PCWSTR(params_wide.as_ptr())
    };
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(file.as_ptr()),
            params_ptr,
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        crate::notify::warn(&format!("could not launch {bin:?}"));
    }
}

/// Bring `hwnd` to the foreground, working around the foreground-lock by briefly
/// attaching to the current foreground thread's input queue.
fn activate(hwnd: HWND) {
    unsafe {
        if hwnd.0.is_null() {
            return;
        }
        let foreground = GetForegroundWindow();
        let fg_thread = GetWindowThreadProcessId(foreground, None);
        let our_thread = GetCurrentThreadId();
        let attach = fg_thread != 0 && fg_thread != our_thread;
        if attach {
            let _ = AttachThreadInput(our_thread, fg_thread, true);
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
        if attach {
            let _ = AttachThreadInput(our_thread, fg_thread, false);
        }
    }
}

/// State threaded through the [`EnumWindows`] callback.
struct EnumState {
    stem: String,
    hwnds: Vec<HWND>,
}

/// Visible top-level windows whose owning process has the given executable stem.
fn windows_for_stem(stem: &str) -> Vec<HWND> {
    let mut state = EnumState {
        stem: stem.to_string(),
        hwnds: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(collect_window),
            LPARAM(&mut state as *mut EnumState as isize),
        );
    }
    state.hwnds
}

/// [`EnumWindows`] callback: collect visible windows matching the target stem.
unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = unsafe { &mut *(lparam.0 as *mut EnumState) };
    if unsafe { IsWindowVisible(hwnd) }.as_bool() {
        if let Some(name) = process_name(hwnd) {
            if name.eq_ignore_ascii_case(&state.stem) {
                state.hwnds.push(hwnd);
            }
        }
    }
    BOOL(1) // keep enumerating
}

/// The foreground window's frame rectangle in screen coordinates.
fn window_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect).ok()? };
    Some(rect)
}

/// Full bounding rectangles of all monitors, ordered left-to-right then
/// top-to-bottom so "next" monitor is predictable.
fn monitor_rects() -> Vec<RECT> {
    let mut rects: Vec<RECT> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&mut rects as *mut Vec<RECT> as isize),
        );
    }
    rects.sort_by_key(|r| (r.left, r.top));
    rects
}

/// [`EnumDisplayMonitors`] callback: collect each monitor's bounding rectangle.
unsafe extern "system" fn collect_monitor(
    _monitor: HMONITOR,
    _dc: HDC,
    rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let rects = unsafe { &mut *(lparam.0 as *mut Vec<RECT>) };
    rects.push(unsafe { *rect });
    BOOL(1) // keep enumerating
}

/// Move `hwnd` to the next/previous monitor, preserving its position relative to
/// that monitor's top-left and clamping its size to fit. A no-op with one monitor.
fn move_to_monitor(hwnd: HWND, direction: CycleDirection) {
    let Some(r) = window_rect(hwnd) else {
        return;
    };
    let rects = monitor_rects();
    if rects.len() < 2 {
        return;
    }
    let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
    let current = rects
        .iter()
        .position(|m| cx >= m.left && cx < m.right && cy >= m.top && cy < m.bottom)
        .unwrap_or(0);
    let cur = rects[current];
    let next = rects[direction.step(current, rects.len())];

    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let new_w = w.min(next.right - next.left);
    let new_h = h.min(next.bottom - next.top);
    let new_x = next.left + (r.left - cur.left).clamp(0, (next.right - next.left - new_w).max(0));
    let new_y = next.top + (r.top - cur.top).clamp(0, (next.bottom - next.top - new_h).max(0));
    unsafe {
        // Restore a maximized window so it can be moved, then re-maximize it on
        // the destination monitor.
        let maximized = IsZoomed(hwnd).as_bool();
        if maximized {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetWindowPos(hwnd, None, new_x, new_y, new_w, new_h, SWP_NOZORDER | SWP_NOACTIVATE);
        if maximized {
            let _ = ShowWindow(hwnd, SW_MAXIMIZE);
        }
    }
}

/// The work area (screen minus taskbar) of the monitor `hwnd` sits on.
fn monitor_work_area(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(monitor, &mut info)
            .as_bool()
            .then_some(info.rcWork)
    }
}

/// A NUL-terminated UTF-16 buffer for a Win32 wide-string argument.
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Top-left position for a window of size `nw`×`nh` placed at `anchor` within
/// the work area `area` (`None` = centred).
fn anchor_pos(area: RECT, nw: i32, nh: i32, anchor: Option<Corner>) -> (i32, i32) {
    let mw = area.right - area.left;
    let mh = area.bottom - area.top;
    match anchor {
        None => (area.left + (mw - nw) / 2, area.top + (mh - nh) / 2),
        Some(Corner::TopLeft) => (area.left, area.top),
        Some(Corner::TopRight) => (area.left + mw - nw, area.top),
        Some(Corner::BottomLeft) => (area.left, area.top + mh - nh),
        Some(Corner::BottomRight) => (area.left + mw - nw, area.top + mh - nh),
    }
}

/// Switch to (or move the active window to) a virtual desktop, via the Windows
/// virtual-desktop COM API (the `winvd` crate). Requires Windows 11 ≥ 24H2.
fn perform_workspace(target: Workspace, move_window: bool) {
    let Some(index) = resolve_desktop_index(target) else {
        return;
    };
    if move_window {
        let hwnd = unsafe { GetForegroundWindow() };
        if !hwnd.0.is_null() {
            if let Err(err) = winvd::move_window_to_desktop(index, &hwnd) {
                log::warn!("move window to desktop {index} failed: {err:?}");
                return;
            }
        }
    }
    if let Err(err) = winvd::switch_desktop(index) {
        log::warn!("switch to desktop {index} failed: {err:?}");
    }
}

/// Resolve a [`Workspace`] target to a 0-based desktop index, clamped to the
/// existing desktops.
fn resolve_desktop_index(target: Workspace) -> Option<u32> {
    match target {
        Workspace::Index(n) => Some(n.saturating_sub(1)), // config is 1-based
        Workspace::Prev => {
            let current = winvd::get_current_desktop().ok()?.get_index().ok()?;
            Some(current.saturating_sub(1))
        }
        Workspace::Next => {
            let current = winvd::get_current_desktop().ok()?.get_index().ok()?;
            let count = winvd::get_desktop_count().unwrap_or(current + 1);
            Some((current + 1).min(count.saturating_sub(1)))
        }
    }
}

// === Find-window overlay ===

/// This process's module handle, as an `HINSTANCE` for window/class creation.
fn instance() -> HINSTANCE {
    unsafe {
        GetModuleHandleW(None)
            .map(|m| HINSTANCE(m.0))
            .unwrap_or(HINSTANCE(std::ptr::null_mut()))
    }
}

/// Build and show the Vimium-style hint overlay over every alt-tab window, and
/// store it as the live [`PICK_WINDOW`] so the hook routes keys to it.
fn start_pick_window() {
    if PICK_WINDOW.with(|f| f.borrow().is_some()) {
        return;
    }
    let hinst = instance();
    register_hint_class(hinst);

    let mut wins: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(collect_alt_tab),
            LPARAM(&mut wins as *mut Vec<HWND> as isize),
        );
    }
    if wins.is_empty() {
        return;
    }

    let hints = make_hints(wins.len());
    let fonts = unsafe { create_fonts() };
    OVERLAY_FONTS.with(|c| c.set(Some(fonts)));

    let screen = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let dc = unsafe { GetDC(None) };
    let mut placed: Vec<(i32, i32, i32, i32)> = Vec::new();
    let mut entries = Vec::new();
    let mut kept_hints = Vec::new();
    for (hwnd, hint) in wins.iter().zip(hints.iter()) {
        let title = window_title(*hwnd);
        let app = process_name(*hwnd).unwrap_or_default();
        // Line 1 shows the app's brand as it appears in the title, falling back
        // to the process name; line 2 is the remaining document/page part.
        let (brand, rest) = split_app_from_title(&title, &app);
        let label = if brand.is_empty() { app } else { brand };
        let chip = unsafe { layout_chip(dc, fonts, &hint.to_uppercase(), &label, &rest) };
        let desired = window_rect(*hwnd).map(|r| (r.left, r.top)).unwrap_or((0, 0));
        let (px, py) = place_hint(desired, (chip.width, chip.height), &placed, screen);
        placed.push((px, py, chip.width, chip.height));
        let overlay = unsafe { create_overlay_window(hinst, px, py, chip) };
        if overlay.0.is_null() {
            continue;
        }
        entries.push(HintEntry { overlay, target: *hwnd });
        kept_hints.push(hint.clone());
    }
    unsafe {
        ReleaseDC(None, dc);
    }
    if entries.is_empty() {
        close_fonts();
        return;
    }
    PICK_WINDOW.with(|f| {
        *f.borrow_mut() = Some(PickWindow {
            entries,
            hints: kept_hints,
            prefix: String::new(),
        });
    });
}

/// Route one key to the navigator while the overlay is up: Esc cancels,
/// Backspace un-types, a hint character narrows or selects. On selection (or
/// cancel) the overlay is torn down; on selection the target is activated and
/// any modifier held to open the overlay is released.
fn handle_pick_window_key(vk: u16, value: i32) {
    if value != 1 {
        return; // suppress key-ups silently
    }
    enum Act {
        Ignore,
        Update,
        Close(Option<HWND>),
    }
    let act = PICK_WINDOW.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let Some(fw) = borrow.as_mut() else {
            return Act::Ignore;
        };
        match Key::from_win_vk(vk) {
            Some(Key::Esc) => Act::Close(None),
            Some(Key::Backspace) => {
                fw.prefix.pop();
                Act::Update
            }
            Some(key) => match key_to_hint_char(key) {
                Some(ch) => match advance(&fw.hints, &mut fw.prefix, ch) {
                    HintMatch::Done(i) => Act::Close(Some(fw.entries[i].target)),
                    HintMatch::Pending => Act::Update,
                },
                None => Act::Ignore,
            },
            None => Act::Ignore,
        }
    });
    match act {
        Act::Ignore => {}
        Act::Update => update_pick_window_visibility(),
        Act::Close(target) => {
            close_pick_window();
            if let Some(hwnd) = target {
                activate(hwnd);
            }
            // The modifier that opened the overlay never saw its release routed
            // through the engine, so drop it now (mirrors the X11 backend).
            let releases = STATE.with(|state| {
                state
                    .borrow_mut()
                    .as_mut()
                    .map(|state| state.engine.clear_modifiers())
                    .unwrap_or_default()
            });
            send_inputs(&releases);
        }
    }
}

/// Show chips whose hint still matches the current prefix; hide the rest.
fn update_pick_window_visibility() {
    PICK_WINDOW.with(|cell| {
        if let Some(fw) = cell.borrow().as_ref() {
            for (entry, hint) in fw.entries.iter().zip(fw.hints.iter()) {
                let cmd = if hint.starts_with(&fw.prefix) {
                    SW_SHOWNA
                } else {
                    SW_HIDE
                };
                unsafe {
                    let _ = ShowWindow(entry.overlay, cmd);
                }
            }
        }
    });
}

/// Tear down the overlay: destroy its windows and free its fonts.
fn close_pick_window() {
    if let Some(fw) = PICK_WINDOW.with(|cell| cell.borrow_mut().take()) {
        for entry in &fw.entries {
            unsafe {
                let _ = DestroyWindow(entry.overlay);
            }
        }
    }
    close_fonts();
}

/// Free the overlay fonts, if any are live.
fn close_fonts() {
    if let Some(fonts) = OVERLAY_FONTS.with(|c| c.take()) {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(fonts.hint.0));
            let _ = DeleteObject(HGDIOBJ(fonts.app.0));
            let _ = DeleteObject(HGDIOBJ(fonts.info.0));
        }
    }
}

// === pick-element overlay ===

/// Dispatch a [`HintAction`] from the standalone hint session.
fn handle_hint_action(action: HintAction) {
    match action {
        HintAction::Suppressed => {}
        HintAction::Updated => update_pick_element_visibility(),
        HintAction::Activate(element) => {
            STATE.with(|s| {
                if let Some(s) = s.borrow_mut().as_mut() {
                    s.hint_session = None;
                }
            });
            close_pick_element();
            activate_element(element);
        }
        HintAction::Dismiss => {
            STATE.with(|s| {
                if let Some(s) = s.borrow_mut().as_mut() {
                    s.hint_session = None;
                }
            });
            close_pick_element();
        }
    }
}

/// Spawn a worker thread that runs UIA element enumeration and posts the results
/// back to the message loop via [`WM_PICK_ELEMENT`].
fn spawn_uia_enum(main_thread_id: u32) {
    std::thread::spawn(move || {
        let elements = uia_enumerate();
        let ptr = Box::into_raw(Box::new(elements));
        unsafe {
            let _ = PostThreadMessageW(main_thread_id, WM_PICK_ELEMENT, WPARAM(ptr as usize), LPARAM(0));
        }
    });
}

/// Build and show the element-hint overlay from a completed enumeration result.
fn show_pick_element(elements: Vec<Element>) {
    let pairs = STATE.with(|state| {
        let mut state = state.borrow_mut();
        let s = state.as_mut()?;
        if !s.standalone_hints_pending {
            return None;
        }
        s.standalone_hints_pending = false;
        let (hs, pairs) = HintSession::new(elements, HINT_CHARS);
        // Only enter the session when there is something to pick, so empty
        // results don't trap input.
        if !pairs.is_empty() {
            s.hint_session = Some(hs);
        }
        Some(pairs)
    }).unwrap_or_default();
    if pairs.is_empty() {
        crate::notify::info("No window elements detected");
        return;
    }
    let hinst = instance();
    register_hint_class(hinst);

    let fonts = unsafe { create_fonts() };
    OVERLAY_FONTS.with(|c| c.set(Some(fonts)));

    let screen = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let dc = unsafe { GetDC(None) };
    let mut placed: Vec<(i32, i32, i32, i32)> = Vec::new();
    let mut entries: Vec<ElementEntry> = Vec::new();
    let mut kept_hints: Vec<String> = Vec::new();

    for (element, hint) in &pairs {
        let hint_upper = hint.to_uppercase();
        let chip = unsafe { layout_chip(dc, fonts, &hint_upper, "", "") };
        let (px, py) = place_hint((element.x, element.y), (chip.width, chip.height), &placed, screen);
        placed.push((px, py, chip.width, chip.height));
        let overlay = unsafe { create_overlay_window(hinst, px, py, chip) };
        if overlay.0.is_null() {
            continue;
        }
        entries.push(ElementEntry { overlay, element: element.clone() });
        kept_hints.push(hint.clone());
    }
    unsafe { ReleaseDC(None, dc) };

    if entries.is_empty() {
        close_fonts();
        return;
    }
    PICK_ELEMENT.with(|f| {
        *f.borrow_mut() = Some(PickElement { entries, hints: kept_hints });
    });
}

/// Show only chips whose hint matches the current prefix; hide the rest.
///
/// Reads from the standalone hint session.
fn update_pick_element_visibility() {
    let matched: Vec<bool> = STATE.with(|state| {
        state.borrow().as_ref().map(|s| {
            s.hint_session.as_ref()
                .map(|hs| hs.matched_hints().map(|(_, _, m)| m).collect())
                .unwrap_or_default()
        }).unwrap_or_default()
    });
    PICK_ELEMENT.with(|cell| {
        if let Some(fe) = cell.borrow().as_ref() {
            for (entry, &visible) in fe.entries.iter().zip(matched.iter()) {
                let cmd = if visible { SW_SHOWNA } else { SW_HIDE };
                unsafe {
                    let _ = ShowWindow(entry.overlay, cmd);
                }
            }
        }
    });
}

/// Tear down the element-hint overlay windows.
fn close_pick_element() {
    if let Some(fe) = PICK_ELEMENT.with(|cell| cell.borrow_mut().take()) {
        for entry in &fe.entries {
            unsafe {
                let _ = DestroyWindow(entry.overlay);
            }
        }
    }
    close_fonts();
}

/// Activate a selected element: move cursor to its centre and click.
fn activate_element(element: Element) {
    let cx = element.x + element.width / 2;
    let cy = element.y + element.height / 2;
    if cx < 0 || cy < 0 {
        return;
    }
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEINPUT, INPUT_MOUSE,
        };
        let _ = SetCursorPos(cx, cy);
        let click = [
            INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0, dy: 0,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_LEFTDOWN,
                        time: 0,
                        dwExtraInfo: INJECT_MARKER,
                    },
                },
            },
            INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0, dy: 0,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_LEFTUP,
                        time: 0,
                        dwExtraInfo: INJECT_MARKER,
                    },
                },
            },
        ];
        SendInput(&click, size_of::<INPUT>() as i32);
    }
}

// === UIA element enumeration ===

/// UIA control type IDs considered actionable for hint generation.
const UIA_ACTIONABLE_TYPES: &[i32] = &[
    UIA_ButtonControlTypeId,
    UIA_CheckBoxControlTypeId,
    UIA_ComboBoxControlTypeId,
    UIA_EditControlTypeId,
    UIA_HeaderItemControlTypeId,
    UIA_HyperlinkControlTypeId,
    UIA_ListItemControlTypeId,
    UIA_MenuItemControlTypeId,
    UIA_RadioButtonControlTypeId,
    UIA_SpinnerControlTypeId,
    UIA_TabItemControlTypeId,
    UIA_ToggleButtonControlTypeId,
    UIA_TreeItemControlTypeId,
];

/// Enumerate interactive elements in the current foreground window via UIA.
///
/// Initialises COM (apartment-threaded) for this thread, enumerates, and
/// uninitialises COM before returning.
fn uia_enumerate() -> Vec<Element> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let did_init = hr.is_ok();
        let result = uia_enumerate_inner().unwrap_or_else(|e| {
            log::warn!("UIA enumeration failed: {e}");
            Vec::new()
        });
        if did_init {
            CoUninitialize();
        }
        result
    }
}

unsafe fn uia_enumerate_inner() -> anyhow::Result<Vec<Element>> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        log::debug!("no foreground window; element overlay will be empty");
        return Ok(Vec::new());
    }
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)? };
    let root = unsafe { automation.ElementFromHandle(hwnd)? };
    let window_rect = unsafe { get_window_rect_opt(hwnd) };
    let true_cond: IUIAutomationCondition = unsafe { automation.CreateTrueCondition()? };
    let all: IUIAutomationElementArray =
        unsafe { root.FindAll(TreeScope_Descendants, &true_cond)? };
    let count = unsafe { all.Length()? };
    log::debug!("UIA raw element count: {count}");

    let mut elements = Vec::new();
    for i in 0..count {
        let elem: IUIAutomationElement = unsafe { all.GetElement(i)? };
        if let Some(e) = unsafe { uia_element_to_element(&elem, window_rect) } {
            elements.push(e);
        }
    }
    log::debug!("UIA actionable elements found: {}", elements.len());
    Ok(elements)
}

unsafe fn uia_element_to_element(
    elem: &IUIAutomationElement,
    window_rect: Option<RECT>,
) -> Option<Element> {
    use windows::Win32::Foundation::BOOL;
    let ctrl_type = unsafe { elem.get_CurrentControlType().ok()? };
    if !UIA_ACTIONABLE_TYPES.contains(&ctrl_type) {
        return None;
    }
    let enabled: BOOL = unsafe { elem.get_CurrentIsEnabled().ok()? };
    if !enabled.as_bool() {
        return None;
    }
    let offscreen: BOOL = unsafe { elem.get_CurrentIsOffscreen().ok()? };
    if offscreen.as_bool() {
        return None;
    }
    let rect: RECT = unsafe { elem.get_CurrentBoundingRectangle().ok()? };
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w <= 0 || h <= 0 || rect.left < 0 || rect.top < 0 {
        return None;
    }
    if let Some(wr) = window_rect {
        if !rects_intersect_elem(rect, wr) {
            return None;
        }
    }
    let name = unsafe {
        elem.get_CurrentName()
            .map(|s| s.to_string())
            .unwrap_or_default()
    };
    Some(Element { height: h, label: name, width: w, x: rect.left, y: rect.top })
}

unsafe fn get_window_rect_opt(hwnd: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut r).ok()? };
    Some(r)
}

fn rects_intersect_elem(a: RECT, b: RECT) -> bool {
    a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top
}

/// [`EnumWindows`] callback: collect alt-tab-able top-level windows.
unsafe extern "system" fn collect_alt_tab(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let wins = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    if unsafe { is_alt_tab_window(hwnd) } {
        wins.push(hwnd);
    }
    BOOL(1) // keep enumerating
}

/// Whether `hwnd` is a normal, user-switchable window: visible, titled, not a
/// tool window, un-owned, and not cloaked (a hidden virtual-desktop/UWP shell).
unsafe fn is_alt_tab_window(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || GetWindowTextLengthW(hwnd) == 0 {
            return false;
        }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        if !GetWindow(hwnd, GW_OWNER).unwrap_or_default().0.is_null() {
            return false;
        }
        !is_cloaked(hwnd)
    }
}

/// Whether the desktop window manager reports `hwnd` as cloaked (hidden).
unsafe fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    unsafe {
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            size_of::<u32>() as u32,
        );
    }
    cloaked != 0
}

/// A window's title text.
fn window_title(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

/// Register the hint window class once per process.
fn register_hint_class(hinst: HINSTANCE) {
    if HINT_CLASS_REGISTERED.with(|c| c.get()) {
        return;
    }
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(overlay_wndproc),
        hInstance: hinst,
        lpszClassName: HINT_CLASS,
        ..Default::default()
    };
    unsafe {
        RegisterClassExW(&wc);
    }
    HINT_CLASS_REGISTERED.with(|c| c.set(true));
}

/// Create the three chip fonts in the system UI face.
unsafe fn create_fonts() -> OverlayFonts {
    unsafe {
        OverlayFonts {
            hint: make_font(HINT_FONT_PX, true),
            app: make_font(APP_FONT_PX, false),
            info: make_font(INFO_FONT_PX, false),
        }
    }
}

/// Create one font of pixel height `px`, bold or regular, in [`HINT_FACE`].
unsafe fn make_font(px: i32, bold: bool) -> HFONT {
    let weight = if bold { FW_BOLD } else { FW_NORMAL };
    unsafe {
        CreateFontW(
            -px,
            0,
            0,
            0,
            weight.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (VARIABLE_PITCH.0 | FF_DONTCARE.0) as u32,
            HINT_FACE,
        )
    }
}

/// Measure and lay out one chip (no drawing), producing the [`ChipData`] stored
/// behind the overlay window for painting.
unsafe fn layout_chip(dc: HDC, fonts: OverlayFonts, hint: &str, app: &str, title: &str) -> ChipData {
    let hint16 = to_wide_no_nul(hint);
    let app16 = to_wide_no_nul(app);
    let title16 = to_wide_no_nul(title);
    let (hint_w, hint_h) = unsafe { text_extent(dc, fonts.hint, &hint16) };
    let (app_w, app_h) = if app.is_empty() {
        (0, 0)
    } else {
        unsafe { text_extent(dc, fonts.app, &app16) }
    };
    let (title_w, title_h) = if title.is_empty() {
        (0, 0)
    } else {
        unsafe { text_extent(dc, fonts.info, &title16) }
    };
    let has_app = app_w > 0;
    let has_title = title_w > 0;
    let hint_chip_w = hint_w + HINT_CHIP_PAD * 2;
    let info_w = if has_app || has_title {
        app_w.max(title_w) + APP_CHIP_PAD * 2
    } else {
        0
    };
    let info_block = match (has_app, has_title) {
        (true, true) => app_h + INFO_LINE_GAP + title_h,
        (true, false) => app_h,
        (false, true) => title_h,
        (false, false) => 0,
    };
    let height = hint_h.max(info_block) + OVERLAY_VPAD * 2;
    ChipData {
        hint: hint16,
        app: app16,
        title: title16,
        hint_chip_w,
        width: hint_chip_w + info_w,
        height,
    }
}

/// The pixel `(width, height)` of `text` rendered with `font` on `dc`.
unsafe fn text_extent(dc: HDC, font: HFONT, text: &[u16]) -> (i32, i32) {
    unsafe {
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let mut size = SIZE::default();
        let _ = GetTextExtentPoint32W(dc, text, &mut size);
        SelectObject(dc, old);
        (size.cx, size.cy)
    }
}

/// Create one overlay window for a laid-out chip and show it without stealing
/// focus. The [`ChipData`] is boxed behind `GWLP_USERDATA` for `WM_PAINT`.
unsafe fn create_overlay_window(hinst: HINSTANCE, x: i32, y: i32, chip: ChipData) -> HWND {
    let (w, h) = (chip.width, chip.height);
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            HINT_CLASS,
            PCWSTR::null(),
            WS_POPUP,
            x,
            y,
            w,
            h,
            None,
            None,
            hinst,
            None,
        )
    }
    .unwrap_or(HWND(std::ptr::null_mut()));
    if !hwnd.0.is_null() {
        let ptr = Box::into_raw(Box::new(chip));
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
            let _ = ShowWindow(hwnd, SW_SHOWNA);
        }
    }
    hwnd
}

/// Window procedure for the hint chips: paint on demand, free the boxed
/// [`ChipData`] on destroy, default otherwise.
unsafe extern "system" fn overlay_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            unsafe { paint_overlay(hwnd) };
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut ChipData;
            if !ptr.is_null() {
                drop(unsafe { Box::from_raw(ptr) });
                unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Paint a chip from its stored [`ChipData`] using the live overlay fonts.
unsafe fn paint_overlay(hwnd: HWND) {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const ChipData;
    let mut ps = PAINTSTRUCT::default();
    let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
    if !ptr.is_null() {
        if let Some(fonts) = OVERLAY_FONTS.with(|c| c.get()) {
            unsafe { draw_chip(hdc, &*ptr, fonts) };
        }
    }
    unsafe {
        let _ = EndPaint(hwnd, &ps);
    }
}

/// Draw the two-chip label: a bordered hint chip on the left, then the app name
/// above the window title on the right.
unsafe fn draw_chip(hdc: HDC, data: &ChipData, fonts: OverlayFonts) {
    let full = RECT {
        left: 0,
        top: 0,
        right: data.width,
        bottom: data.height,
    };
    let hint_rect = RECT {
        left: 0,
        top: 0,
        right: data.hint_chip_w,
        bottom: data.height,
    };
    unsafe {
        fill(hdc, &full, APP_BG);
        fill(hdc, &hint_rect, HINT_BG);
        let border = CreateSolidBrush(HINT_BORDER);
        let _ = FrameRect(hdc, &hint_rect, border);
        let _ = DeleteObject(HGDIOBJ(border.0));
        SetBkMode(hdc, TRANSPARENT);

        let (_, hint_h) = text_extent(hdc, fonts.hint, &data.hint);
        let hint_y = (data.height - hint_h) / 2;
        draw_text(hdc, fonts.hint, HINT_CHIP_PAD, hint_y, HINT_FG, &data.hint);

        let has_app = !data.app.is_empty();
        let has_title = !data.title.is_empty();
        let (_, app_h) = if has_app {
            text_extent(hdc, fonts.app, &data.app)
        } else {
            (0, 0)
        };
        let (_, title_h) = if has_title {
            text_extent(hdc, fonts.info, &data.title)
        } else {
            (0, 0)
        };
        let block = match (has_app, has_title) {
            (true, true) => app_h + INFO_LINE_GAP + title_h,
            (true, false) => app_h,
            (false, true) => title_h,
            (false, false) => 0,
        };
        let mut top = (data.height - block) / 2;
        let x = data.hint_chip_w + APP_CHIP_PAD;
        if has_app {
            draw_text(hdc, fonts.app, x, top, APP_FG, &data.app);
            top += app_h + INFO_LINE_GAP;
        }
        if has_title {
            draw_text(hdc, fonts.info, x, top, TITLE_FG, &data.title);
        }
    }
}

/// Fill `rect` with a solid `color`.
unsafe fn fill(hdc: HDC, rect: &RECT, color: COLORREF) {
    unsafe {
        let brush = CreateSolidBrush(color);
        let _ = FillRect(hdc, rect, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }
}

/// Draw `text` at `(x, y)` in `color` with `font`.
unsafe fn draw_text(hdc: HDC, font: HFONT, x: i32, y: i32, color: COLORREF, text: &[u16]) {
    unsafe {
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        SetTextColor(hdc, color);
        let _ = TextOutW(hdc, x, y, text);
        SelectObject(hdc, old);
    }
}

/// UTF-16 encode `s` without a trailing NUL (as `TextOutW`/extent calls want).
fn to_wide_no_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_program_matches_across_paths() {
        assert!(same_program(
            r"C:\Program Files\rightkeys\rightkeys.exe",
            r"D:\src\rightkeys\target\release\rightkeys.exe",
        ));
    }

    #[test]
    fn same_program_is_case_insensitive() {
        assert!(same_program(
            r"C:\bin\RightKeys.exe",
            r"C:\other\rightkeys.exe"
        ));
    }

    #[test]
    fn same_program_rejects_other_binaries() {
        assert!(!same_program(r"C:\bin\other.exe", r"C:\bin\rightkeys.exe"));
    }
}
