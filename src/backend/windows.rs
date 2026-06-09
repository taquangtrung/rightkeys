//! Windows backend: a low-level keyboard hook (`WH_KEYBOARD_LL`) captures and
//! suppresses keys, `SendInput` injects the engine's output, and the foreground
//! window's process name scopes keymaps.
//!
//! The hook callback is global, so engine state lives in a thread-local owned by
//! the message-pump thread. Injected events carry [`INJECT_MARKER`] in their
//! `dwExtraInfo` so the hook passes its own output straight through.

// Imports

use std::cell::{Cell, RefCell};
use std::mem::size_of;
use std::path::Path;

use anyhow::{Context, Result};
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, BOOL, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromWindow, HDC, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
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
    CallNextHookEx, EnumWindows, GetForegroundWindow, GetMessageW, GetWindowLongW, GetWindowRect,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, IsZoomed, KillTimer, SetForegroundWindow,
    SetTimer, SetWindowPos, SetWindowsHookExW, ShowWindow, UnhookWindowsHookEx, GWL_EXSTYLE,
    HC_ACTION, HHOOK, HWND_NOTOPMOST, HWND_TOPMOST, KBDLLHOOKSTRUCT, MSG, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SW_SHOWNORMAL,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WS_EX_TOPMOST,
};

use super::{Options, WindowWatcher};
use crate::engine::{Corner, CycleDirection, Effect, Engine, OutEvent, Side, WindowAction, Workspace};
use crate::key::Key;

// Constants

/// Tag stamped into `dwExtraInfo` of injected events so the hook ignores them.
const INJECT_MARKER: usize = 0x5249_4748; // "RIGH"

/// Buffer length for process image paths.
const PATH_BUF_LEN: usize = 512;

/// Milliseconds a tap-hold key may be held with no other key before committing
/// to its hold modifier, so the modifier reaches the OS in time for a mouse
/// click (which never reaches the hook), e.g. Shift/Ctrl-click multi-select.
const TAP_HOLD_TIMEOUT_MS: u32 = 200;

// Data Structures

/// Engine plus window watcher, owned by the hook thread.
struct State {
    engine: Engine,
    watcher: ForegroundWatcher,
}

/// Foreground-window watcher with a one-entry cache keyed on the window handle.
#[derive(Default)]
struct ForegroundWatcher {
    cached_hwnd: isize,
    cached_app: String,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static HOOK: Cell<HHOOK> = const { Cell::new(HHOOK(std::ptr::null_mut())) };
    /// Identifier of the active tap-hold timeout timer, or `0` when none is set.
    static TAP_HOLD_TIMER: Cell<usize> = const { Cell::new(0) };
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

    STATE.with(|state| {
        *state.borrow_mut() = Some(State {
            engine,
            watcher: ForegroundWatcher::default(),
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
        }
    }
}

/// Activate an existing top-level window of `program`, or launch it if none is
/// open. `program` is matched on its file stem (`brave.exe` matches a `brave`
/// process), mirroring the old AHK "activate or launch" helper.
fn activate_or_launch(program: &str) {
    let stem = Path::new(program)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string());
    if let Some(&hwnd) = windows_for_stem(&stem).first() {
        activate(hwnd);
    } else {
        launch(program);
    }
}

/// Launch a program by name or path. `ShellExecuteW` resolves bare names through
/// the App Paths registry and `PATH`, just as AHK's `Run` did.
///
/// NOTE: the launched process inherits this process's integrity level. If
/// RightKeys is run elevated (to capture keys from elevated windows), launched
/// apps are elevated too; launch RightKeys un-elevated, or add a shell-based
/// de-elevation step, if that matters.
fn launch(program: &str) {
    let file = to_wide(program);
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
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
