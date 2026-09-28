//! Cheap Win32 system probes, all polled from the UI thread.

use std::cell::Cell;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{FILETIME, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmGetColorizationColor;
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_BINARY, RegGetValueW};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId, GetSystemTimes};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY, VK_LCONTROL, VK_LEFT, VK_LWIN, VK_RIGHT,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::Shell::{APPBARDATA, SHAppBarMessage};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZESTART, GetClassNameW, GetCursorPos,
    GetForegroundWindow, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, OBJID_WINDOW, SetForegroundWindow,
    WINEVENT_OUTOFCONTEXT, CallNextHookEx, HHOOK, MSLLHOOKSTRUCT, SetWindowsHookExW, UnhookWindowsHookEx, WH_MOUSE_LL,
    WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_RBUTTONDOWN,
};
use windows::core::{GUID, HSTRING, PCWSTR};

#[derive(Default)]
pub struct Cpu {
    prev_idle: u64,
    prev_total: u64,
}

fn ft(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

impl Cpu {
    /// Total CPU usage in percent since the previous call.
    pub fn sample(&mut self) -> f32 {
        let (mut idle, mut kernel, mut user) = Default::default();
        if unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.is_err() {
            return 0.0;
        }
        // Kernel time already includes idle time.
        let (idle, total) = (ft(idle), ft(kernel) + ft(user));
        let (di, dt) = (idle - self.prev_idle, total - self.prev_total);
        self.prev_idle = idle;
        self.prev_total = total;
        if dt == 0 { 0.0 } else { (1.0 - di as f32 / dt as f32) * 100.0 }
    }
}

pub fn memory_percent() -> f32 {
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    match unsafe { GlobalMemoryStatusEx(&mut m) } {
        Ok(()) => (1.0 - m.ullAvailPhys as f64 / m.ullTotalPhys as f64) as f32 * 100.0,
        Err(_) => 0.0,
    }
}

/// (percent, plugged in), or None on machines without a battery.
pub fn battery() -> Option<(f32, bool)> {
    let mut s = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut s) }.ok()?;
    if s.BatteryFlag & 128 != 0 || s.BatteryLifePercent > 100 {
        return None;
    }
    Some((s.BatteryLifePercent as f32, s.ACLineStatus == 1))
}

/// Title of the focused window; empty for the desktop, None (keep the old one) for our bars.
pub fn foreground_title(ours: &[HWND]) -> Option<String> {
    let hwnd = unsafe { GetForegroundWindow() };
    if ours.contains(&hwnd) {
        return None; // keep showing the previous title
    }
    if hwnd.is_invalid() {
        return Some(String::new());
    }
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
    let title = String::from_utf16_lossy(&buf[..n]);
    // Our own popups (control panel, tray, menus) aren't "the window you're in" either.
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == std::process::id() && !title.ends_with("Settings") {
        return None;
    }
    let mut class = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut class) } as usize;
    let class = String::from_utf16_lossy(&class[..n]);
    if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
        return Some(String::new());
    }
    Some(title)
}

fn reg_binary(subkey: &str, value: &str) -> Option<Vec<u8>> {
    let (subkey, value) = (HSTRING::from(subkey), HSTRING::from(value));
    let mut size = 0u32;
    unsafe {
        let key = PCWSTR(subkey.as_ptr());
        let val = PCWSTR(value.as_ptr());
        RegGetValueW(HKEY_CURRENT_USER, key, val, RRF_RT_REG_BINARY, None, None, Some(&mut size)).ok().ok()?;
        let mut buf = vec![0u8; size as usize];
        RegGetValueW(HKEY_CURRENT_USER, key, val, RRF_RT_REG_BINARY, None, Some(buf.as_mut_ptr().cast()), Some(&mut size))
            .ok()
            .ok()?;
        buf.truncate(size as usize);
        Some(buf)
    }
}

/// (number of virtual desktops, index of the current one), read from Explorer's registry state.
pub fn virtual_desktops() -> (i32, i32) {
    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\VirtualDesktops";
    let Some(ids) = reg_binary(KEY, "VirtualDesktopIDs") else { return (1, 0) };
    let count = (ids.len() / 16).max(1) as i32;
    let current = reg_binary(KEY, "CurrentVirtualDesktop").or_else(|| {
        let mut session = 0u32;
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }.ok()?;
        let key = format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\SessionInfo\{session}\VirtualDesktops");
        reg_binary(&key, "CurrentVirtualDesktop")
    });
    let index = current
        .and_then(|cur| ids.chunks_exact(16).position(|id| id == cur.as_slice()))
        .unwrap_or(0) as i32;
    (count, index)
}

fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    if matches!(vk, VK_LEFT | VK_RIGHT) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, dwFlags: flags, ..Default::default() } },
    }
}

/// Presses `keys` in order, then releases them in reverse, as one shortcut.
pub fn send_shortcut(keys: &[VIRTUAL_KEY]) {
    let mut inputs: Vec<INPUT> = keys.iter().map(|&k| key_input(k, false)).collect();
    inputs.extend(keys.iter().rev().map(|&k| key_input(k, true)));
    unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
}

pub fn switch_desktop(from: i32, to: i32) {
    let arrow = if to > from { VK_RIGHT } else { VK_LEFT };
    for _ in 0..(to - from).abs() {
        send_shortcut(&[VK_LCONTROL, VK_LWIN, arrow]);
    }
}

/// The Windows accent color as 0xRRGGBBAA.
pub fn system_accent() -> Option<u32> {
    let (mut argb, mut opaque) = (0u32, Default::default());
    unsafe { DwmGetColorizationColor(&mut argb, &mut opaque) }.ok()?;
    Some((argb << 8) | 0xff)
}

/// Brings `hwnd` to the front with focus, reliably.
///
/// The bar never takes focus itself (so clicking it doesn't disturb the app you're in),
/// which means Windows' focus-stealing protection may refuse a plain SetForegroundWindow
/// from us. When it refuses, it flashes the window's taskbar button instead, and a flashing
/// button makes an auto-hidden Windows taskbar pop up. Temporarily sharing input state with
/// the current foreground thread makes the switch count as coming from it.
pub fn bring_to_front(hwnd: HWND) {
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground == hwnd {
            return;
        }
        let ours = GetCurrentThreadId();
        let theirs = GetWindowThreadProcessId(foreground, None);
        let attached = theirs != 0 && theirs != ours && AttachThreadInput(ours, theirs, true).as_bool();
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(ours, theirs, false);
        }
    }
}

thread_local! {
    static LAST_MINIMIZE: Cell<Option<Instant>> = const { Cell::new(None) };
}

fn is_windows_taskbar(hwnd: HWND) -> bool {
    let mut class = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut class) } as usize;
    let class = String::from_utf16_lossy(&class[..n]);
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    matches!(class.as_str(), "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") && pid != unsafe { GetCurrentProcessId() }
}

fn taskbar_autohides() -> bool {
    const ABM_GETSTATE: u32 = 4;
    const ABS_AUTOHIDE: usize = 1;
    let mut abd = APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, ..Default::default() };
    crate::tray::bypass(|| unsafe { SHAppBarMessage(ABM_GETSTATE, &mut abd) }) & ABS_AUTOHIDE != 0
}

fn cursor_over(hwnd: HWND) -> bool {
    unsafe {
        let (mut pt, mut rect) = (POINT::default(), RECT::default());
        let _ = GetCursorPos(&mut pt);
        let _ = GetWindowRect(hwnd, &mut rect);
        pt.x >= rect.left && pt.x < rect.right && pt.y >= rect.top && pt.y < rect.bottom
    }
}

unsafe extern "system" fn on_win_event(_: HWINEVENTHOOK, event: u32, hwnd: HWND, object: i32, _: i32, _: u32, _: u32) {
    if object != OBJID_WINDOW.0 {
        return;
    }
    match event {
        EVENT_SYSTEM_MINIMIZESTART => LAST_MINIMIZE.set(Some(Instant::now())),
        EVENT_SYSTEM_FOREGROUND => {
            let just_minimized = LAST_MINIMIZE.get().is_some_and(|t| t.elapsed() < Duration::from_millis(1500));
            if just_minimized && is_windows_taskbar(hwnd) && !cursor_over(hwnd) && taskbar_autohides() {
                LAST_MINIMIZE.set(None);
                // Backup for when Windows still picked the taskbar: move focus to our invisible
                // focus sink (the desktop itself would make the taskbar peek).
                if let Some(sink) = crate::appbar::focus_sink() {
                    bring_to_front(sink);
                }
            }
        }
        _ => {}
    }
}

/// When the last visible window is minimized (by any means: its own button, a shortcut),
/// Windows gives focus to the Windows taskbar, and an auto-hidden taskbar then pops up.
/// Normally the focus sink (see appbar) prevents that; this backup takes focus back if
/// Windows picked the taskbar anyway. It only acts right after a minimize,
/// with auto-hide on and the mouse away from the taskbar, so using the taskbar still works.
pub fn install_minimize_focus_guard() {
    unsafe {
        for event in [EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZESTART] {
            SetWinEventHook(event, event, None, Some(on_win_event), 0, 0, WINEVENT_OUTOFCONTEXT);
        }
    }
}

/// The id of the current virtual desktop.
pub fn current_desktop_id() -> Option<GUID> {
    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\VirtualDesktops";
    let b = reg_binary(KEY, "CurrentVirtualDesktop").filter(|b| b.len() == 16)?;
    Some(GUID::from_values(
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u16::from_le_bytes([b[4], b[5]]),
        u16::from_le_bytes([b[6], b[7]]),
        [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
    ))
}

// Closing panels (power menu, tray panel, context menus) on a click anywhere else. Losing
// focus isn't enough: a panel that never got focus would otherwise stay open forever.

type CloseFn = std::rc::Rc<dyn Fn()>;

thread_local! {
    static OUTSIDE: std::cell::RefCell<Vec<(HWND, CloseFn)>> = const { std::cell::RefCell::new(Vec::new()) };
    static MOUSE_HOOK: Cell<Option<HHOOK>> = const { Cell::new(None) };
}

fn contains(hwnd: HWND, pt: POINT) -> bool {
    let mut rect = RECT::default();
    let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
    pt.x >= rect.left && pt.x < rect.right && pt.y >= rect.top && pt.y < rect.bottom
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && matches!(wparam.0 as u32, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN) {
        let pt = unsafe { (*(lparam.0 as *const MSLLHOOKSTRUCT)).pt };
        let hit: Vec<CloseFn> =
            OUTSIDE.with_borrow(|list| list.iter().filter(|(h, _)| !contains(*h, pt)).map(|(_, c)| c.clone()).collect());
        // Close after the hook returns: low-level hooks must be quick.
        for close in hit {
            slint::Timer::single_shot(Duration::ZERO, move || close());
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Calls `close` when the user clicks anywhere outside `popup`, until `stop_outside_watch`.
pub fn close_on_outside_click(popup: HWND, close: CloseFn) {
    OUTSIDE.with_borrow_mut(|list| {
        list.retain(|(h, _)| *h != popup);
        list.push((popup, close));
    });
    if MOUSE_HOOK.get().is_none() {
        if let Ok(hook) = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0) } {
            MOUSE_HOOK.set(Some(hook));
        }
    }
}

pub fn stop_outside_watch(popup: HWND) {
    let empty = OUTSIDE.with_borrow_mut(|list| {
        list.retain(|(h, _)| *h != popup);
        list.is_empty()
    });
    if empty {
        if let Some(hook) = MOUSE_HOOK.take() {
            let _ = unsafe { UnhookWindowsHookEx(hook) };
        }
    }
}

/// Waits (at most `max`) until Explorer's taskbar exists. Started right at sign-in, the bar
/// can beat it, and everything that talks to the shell needs it: reserving screen space,
/// hiding the Windows taskbar, the tray host. Returns at once when Explorer is already up.
pub fn wait_for_explorer_taskbar(max: Duration) {
    let start = Instant::now();
    while start.elapsed() < max {
        let found = unsafe { windows::Win32::UI::WindowsAndMessaging::FindWindowW(windows::core::w!("Shell_TrayWnd"), PCWSTR::null()) };
        if found.is_ok_and(|h| !h.is_invalid()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
