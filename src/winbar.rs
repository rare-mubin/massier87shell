//! Hiding the Windows taskbar (like "Taskbar Eliminator"), plus a global shortcut that
//! toggles it.
//!
//! Hiding a window alone isn't enough: a visible (non-auto-hide) taskbar keeps its screen
//! space reserved. So we first switch it to auto-hide (which releases the space), then hide
//! its windows on every display, and keep them hidden: Explorer shows them again on some
//! occasions (e.g. when the Start menu opens). The previous auto-hide setting is restored
//! when hiding is turned off or the bar exits.

use std::cell::Cell;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::Shell::{APPBARDATA, SHAppBarMessage};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, EnumWindows, GetClassNameW, GetWindowThreadProcessId, IsWindowVisible,
    RegisterClassW, SW_HIDE, SW_SHOWNA, ShowWindow, WM_HOTKEY, WNDCLASSW, WS_POPUP,
};
use windows::core::{BOOL, w};

const HOTKEY_ID: i32 = 0x87;
const ABM_GETSTATE: u32 = 4;
const ABM_SETSTATE: u32 = 10;
const ABS_AUTOHIDE: u32 = 1;

thread_local! {
    /// The "hide Windows taskbar" setting.
    static SETTING: Cell<bool> = const { Cell::new(false) };
    /// The shortcut or the control panel tile changed the setting; the bar saves it to the config.
    static USER_CHANGED: Cell<bool> = const { Cell::new(false) };
    /// Whether we currently keep it hidden, and the auto-hide state to restore afterwards.
    static HIDING: Cell<bool> = const { Cell::new(false) };
    static SAVED_STATE: Cell<Option<u32>> = const { Cell::new(None) };
    static MSG_WINDOW: Cell<Option<HWND>> = const { Cell::new(None) };
    static HOTKEY: Cell<Option<(HOT_KEY_MODIFIERS, u32)>> = const { Cell::new(None) };
}

unsafe extern "system" fn collect(hwnd: HWND, list: LPARAM) -> BOOL {
    let list = unsafe { &mut *(list.0 as *mut Vec<HWND>) };
    let mut class = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut class) } as usize;
    let class = String::from_utf16_lossy(&class[..n]);
    if matches!(class.as_str(), "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
        // Skip our own tray window (see tray.rs), which has the same class name.
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid != std::process::id() {
            list.push(hwnd);
        }
    }
    true.into()
}

/// Explorer's taskbar windows on every display.
fn taskbars() -> Vec<HWND> {
    let mut list: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

fn appbar_state(set: Option<u32>) -> u32 {
    let mut abd = APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, ..Default::default() };
    // Straight to Explorer, not through our own tray window.
    crate::tray::bypass(|| unsafe {
        match set {
            Some(state) => {
                abd.lParam = LPARAM(state as isize);
                SHAppBarMessage(ABM_SETSTATE, &mut abd) as u32
            }
            None => SHAppBarMessage(ABM_GETSTATE, &mut abd) as u32,
        }
    })
}

fn apply() {
    let hide = SETTING.get();
    if hide == HIDING.get() {
        return;
    }
    HIDING.set(hide);
    if hide {
        if SAVED_STATE.get().is_none() {
            SAVED_STATE.set(Some(appbar_state(None)));
        }
        appbar_state(Some(ABS_AUTOHIDE)); // releases the screen space it reserves
        keep_hidden();
    } else {
        for tb in taskbars() {
            unsafe {
                let _ = ShowWindow(tb, SW_SHOWNA);
            }
        }
        if let Some(state) = SAVED_STATE.take() {
            appbar_state(Some(state));
        }
    }
}

/// Re-hides the taskbar if Explorer showed it again. Called on the bar's fast timer.
pub fn keep_hidden() {
    if !HIDING.get() {
        return;
    }
    for tb in taskbars() {
        unsafe {
            if IsWindowVisible(tb).as_bool() {
                let _ = ShowWindow(tb, SW_HIDE);
            }
        }
    }
}

/// Applies the setting from the config.
pub fn set_hidden(hidden: bool) {
    SETTING.set(hidden);
    apply();
}

/// Whether the Windows taskbar is currently kept hidden.
pub fn is_hidden() -> bool {
    HIDING.get()
}

/// Flips the setting (the shortcut and the control panel tile). It's a real setting change,
/// saved by the bar through `take_user_change`, so it survives a restart.
pub fn toggle() {
    SETTING.set(!SETTING.get());
    USER_CHANGED.set(true);
    apply();
}

/// The new setting, once, after `toggle` changed it.
pub fn take_user_change() -> Option<bool> {
    USER_CHANGED.replace(false).then(|| SETTING.get())
}

/// Shows the taskbar again and restores its auto-hide setting (on exit).
pub fn restore() {
    SETTING.set(false);
    apply();
}

unsafe extern "system" fn msg_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_HOTKEY && wparam.0 == HOTKEY_ID as usize {
        toggle();
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn msg_window() -> Option<HWND> {
    if let Some(h) = MSG_WINDOW.get() {
        return Some(h);
    }
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        let class = w!("massier87shell-hotkeys");
        let wc = WNDCLASSW { lpfnWndProc: Some(msg_proc), hInstance: instance.into(), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(Default::default(), class, w!(""), WS_POPUP, 0, 0, 0, 0, None, None, Some(instance.into()), None).ok()?;
        MSG_WINDOW.set(Some(hwnd));
        Some(hwnd)
    }
}

/// Parses shortcuts like "Ctrl+Win+B", "Alt+Shift+F9".
fn parse_hotkey(text: &str) -> Option<(HOT_KEY_MODIFIERS, u32)> {
    let mut mods = MOD_NOREPEAT;
    let mut key = None;
    for part in text.split('+').map(|p| p.trim().to_ascii_uppercase()) {
        match part.as_str() {
            "CTRL" | "CONTROL" => mods |= MOD_CONTROL,
            "ALT" => mods |= MOD_ALT,
            "SHIFT" => mods |= MOD_SHIFT,
            "WIN" | "WINDOWS" | "SUPER" => mods |= MOD_WIN,
            p if p.len() == 1 && p.chars().all(|c| c.is_ascii_alphanumeric()) => key = Some(p.as_bytes()[0] as u32),
            p if p.starts_with('F') && p[1..].parse::<u32>().is_ok_and(|n| (1..=24).contains(&n)) => {
                key = Some(0x70 + p[1..].parse::<u32>().unwrap() - 1) // VK_F1..
            }
            _ => return None,
        }
    }
    // A bare key would steal normal typing.
    (mods != MOD_NOREPEAT).then_some(())?;
    Some((mods, key?))
}

/// Registers the toggle shortcut. Returns an error text if it's invalid or already taken.
pub fn set_hotkey(text: &str) -> Result<(), String> {
    let parsed = if text.trim().is_empty() { None } else { Some(parse_hotkey(text).ok_or("not a valid shortcut")?) };
    if parsed == HOTKEY.get() {
        return Ok(());
    }
    let hwnd = msg_window().ok_or("no message window")?;
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
    }
    HOTKEY.set(None);
    let Some((mods, vk)) = parsed else { return Ok(()) };
    unsafe { RegisterHotKey(Some(hwnd), HOTKEY_ID, mods, vk) }.map_err(|_| "already used by another app".to_string())?;
    HOTKEY.set(Some((mods, vk)));
    Ok(())
}
