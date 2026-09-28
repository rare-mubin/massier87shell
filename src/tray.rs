//! System tray ("notification area") icons for the `tray` module.
//!
//! Windows has no API to read tray icons, and on Windows 11 they live inside Explorer's
//! XAML taskbar. So we do what alternative shells (RetroBar, Cairo) do: create our own
//! window of class `Shell_TrayWnd` and keep it above Explorer's. `Shell_NotifyIcon` sends
//! icon updates to the first `Shell_TrayWnd` it finds — ours. We record them, and forward
//! *every* message to Explorer's real tray, so the Windows taskbar, AppBars (including our
//! own bars) and everything else keep working exactly as before.
//!
//! Everything here runs on the UI thread and deliberately never touches the app `State`:
//! the window procedure can be entered synchronously (e.g. from our own SHAppBarMessage
//! calls) while that is borrowed.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW, HBITMAP,
    HGDIOBJ, ReleaseDC,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::NIN_SELECT;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowExW, FindWindowW, GetCursorPos,
    GetIconInfo, GetWindowThreadProcessId, HICON, HWND_BROADCAST, HWND_NOTOPMOST, HWND_TOPMOST, ICONINFO, IsWindow, PostMessageW,
    RegisterClassW, RegisterWindowMessageW, SMTO_ABORTIFHUNG, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SendMessageTimeoutW,
    SendNotifyMessageW, SetWindowPos, WM_CONTEXTMENU, WM_COPYDATA, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_USER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{GUID, w};

use crate::taskbar::{RawIcon, process_path};
use slint::Rgba8Pixel;

const TRAY_CLASS: windows::core::PCWSTR = w!("Shell_TrayWnd");

/// `COPYDATASTRUCT` (declared here to avoid another `windows` crate feature).
#[repr(C)]
struct CopyData {
    kind: usize,
    len: u32,
    data: *const c_void,
}

/// One tray icon, as its app registered it.
pub struct TrayIcon {
    hwnd: HWND,
    uid: u32,
    guid: Option<GUID>,
    callback: u32,
    version: u32,
    hidden: bool,
    pub pid: u32,
    /// Lower-case exe file name; what pins are keyed on (stable across app updates).
    pub exe: String,
    pub tip: String,
    pub icon: Option<RawIcon>,
}

impl TrayIcon {
    fn same(&self, hwnd: HWND, uid: u32, guid: Option<GUID>) -> bool {
        match (guid, self.guid) {
            (Some(a), Some(b)) => a == b,
            _ => self.hwnd == hwnd && self.uid == uid,
        }
    }

    /// Display name: the first line of the tooltip, else the exe name.
    pub fn name(&self) -> String {
        let tip = self.tip.lines().next().unwrap_or("").trim();
        if tip.is_empty() { self.exe.trim_end_matches(".exe").to_string() } else { tip.to_string() }
    }
}

thread_local! {
    static HOST: Cell<Option<HWND>> = const { Cell::new(None) };
    static EXPLORER: Cell<Option<HWND>> = const { Cell::new(None) };
    static ICONS: RefCell<Vec<TrayIcon>> = const { RefCell::new(Vec::new()) };
    static DIRTY: Cell<bool> = const { Cell::new(false) };
    static LAST_BROADCAST: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Explorer's own tray window. Checked by process, not just "not ours": right after a
/// restart the previous bar instance's tray window can still be around for a moment.
fn explorer_tray() -> Option<HWND> {
    if let Some(h) = EXPLORER.get().filter(|&h| unsafe { IsWindow(Some(h)).as_bool() }) {
        return Some(h);
    }
    let mut after = None;
    loop {
        let h = unsafe { FindWindowExW(None, after, TRAY_CLASS, None) }.ok()?;
        let is_explorer = process_path(h).is_some_and(|p| p.to_lowercase().ends_with("\\explorer.exe"));
        if is_explorer {
            EXPLORER.set(Some(h));
            return Some(h);
        }
        after = Some(h);
    }
}

fn forward(msg: u32, wparam: WPARAM, lparam: LPARAM, fallback: usize) -> LRESULT {
    let Some(explorer) = explorer_tray() else { return LRESULT(fallback as isize) };
    let mut result = 0usize;
    let ok = unsafe { SendMessageTimeoutW(explorer, msg, wparam, lparam, SMTO_ABORTIFHUNG, 5000, Some(&mut result)) };
    LRESULT(if ok.0 == 0 { fallback } else { result } as isize)
}

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_COPYDATA && lparam.0 != 0 {
        let cds = unsafe { &*(lparam.0 as *const CopyData) };
        // 1 = SH_TRAY_DATA (Shell_NotifyIcon). 0 = AppBar messages, 3 = icon rect queries.
        if cds.kind == 1 && !cds.data.is_null() {
            let bytes = unsafe { std::slice::from_raw_parts(cds.data as *const u8, cds.len as usize) };
            record(bytes);
        }
        return forward(msg, wparam, lparam, (cds.kind == 1) as usize);
    }
    // Private messages carry plain values, so they are safe to pass on. Registered
    // messages (0xC000+) are broadcasts like "TaskbarCreated" that must not bounce back.
    if (WM_USER..0xC000).contains(&msg) {
        return forward(msg, wparam, lparam, 0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    b.get(off..off + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn wstr_at(b: &[u8], off: usize, chars: usize) -> String {
    let Some(s) = b.get(off..off + chars * 2) else { return String::new() };
    let units: Vec<u16> = s.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
    String::from_utf16_lossy(&units)
}

/// Parses a SHELLTRAYDATA message. Layout: signature, message, then the 32-bit
/// NOTIFYICONDATAW (handles are 32 bits wide even for 64-bit apps).
fn record(b: &[u8]) {
    const NIF_MESSAGE: u32 = 0x1;
    const NIF_ICON: u32 = 0x2;
    const NIF_TIP: u32 = 0x4;
    const NIF_STATE: u32 = 0x8;
    const NIF_GUID: u32 = 0x20;
    const NIS_HIDDEN: u32 = 0x1;
    if b.len() < 8 + 24 {
        return;
    }
    let message = u32_at(b, 4);
    let nid = &b[8..];
    let hwnd = HWND(u32_at(nid, 4) as usize as *mut c_void);
    let uid = u32_at(nid, 8);
    let flags = u32_at(nid, 12);
    let guid = (flags & NIF_GUID != 0 && nid.len() >= 952).then(|| {
        let g = &nid[936..952];
        GUID::from_u128(u128::from_be_bytes([
            g[3], g[2], g[1], g[0], g[5], g[4], g[7], g[6], g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15],
        ]))
    });

    ICONS.with_borrow_mut(|icons| {
        let pos = icons.iter().position(|i| i.same(hwnd, uid, guid));
        match message {
            // NIM_ADD / NIM_MODIFY
            0 | 1 => {
                let index = match pos {
                    Some(i) => i,
                    None => {
                        let exe = process_path(hwnd)
                            .and_then(|p| p.rsplit('\\').next().map(str::to_lowercase))
                            .unwrap_or_default();
                        let mut pid = 0u32;
                        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
                        icons.push(TrayIcon {
                            hwnd,
                            uid,
                            guid,
                            callback: 0,
                            version: 0,
                            hidden: false,
                            pid,
                            exe,
                            tip: String::new(),
                            icon: None,
                        });
                        icons.len() - 1
                    }
                };
                let icon = &mut icons[index];
                icon.hwnd = hwnd;
                if flags & NIF_MESSAGE != 0 {
                    icon.callback = u32_at(nid, 16);
                }
                if flags & NIF_ICON != 0 {
                    let handle = u32_at(nid, 20);
                    // Converted right away: the app may destroy the icon handle later.
                    icon.icon = (handle != 0).then(|| icon_pixels(HICON(handle as usize as *mut c_void))).flatten();
                }
                if flags & NIF_TIP != 0 {
                    icon.tip = wstr_at(nid, 24, 128);
                }
                if flags & NIF_STATE != 0 {
                    let (state, mask) = (u32_at(nid, 280), u32_at(nid, 284));
                    if mask & NIS_HIDDEN != 0 {
                        icon.hidden = state & NIS_HIDDEN != 0;
                    }
                }
            }
            // NIM_DELETE
            2 => {
                if let Some(i) = pos {
                    icons.remove(i);
                }
            }
            // NIM_SETVERSION
            4 => {
                if let Some(i) = pos {
                    icons[i].version = u32_at(nid, 800);
                }
            }
            _ => return,
        }
        DIRTY.set(true);
    });
}

/// Reads an HICON (a session-wide handle, usable from any process) into RGBA pixels.
fn icon_pixels(icon: HICON) -> Option<RawIcon> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(icon, &mut info).ok()?;
        let read = |bitmap: HBITMAP| -> Option<(i32, i32, Vec<u8>)> {
            if bitmap.is_invalid() {
                return None;
            }
            let mut bm = BITMAP::default();
            GetObjectW(HGDIOBJ(bitmap.0), size_of::<BITMAP>() as i32, Some((&mut bm as *mut BITMAP).cast()));
            let (w, h) = (bm.bmWidth, bm.bmHeight);
            let mut bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut buf = vec![0u8; (w * h * 4).max(0) as usize];
            let dc = GetDC(None);
            let rows = GetDIBits(dc, bitmap, 0, h as u32, Some(buf.as_mut_ptr().cast()), &mut bmi, DIB_RGB_COLORS);
            ReleaseDC(None, dc);
            (rows > 0).then_some((w, h, buf))
        };
        let color = read(info.hbmColor);
        let mask = read(info.hbmMask);
        let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
        let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));

        let (w, h, bgra) = color?; // monochrome-only icons are practically extinct
        let has_alpha = bgra.chunks_exact(4).any(|p| p[3] != 0);
        let pixels = bgra
            .chunks_exact(4)
            .enumerate()
            .map(|(i, p)| {
                // Old icons without alpha: the AND mask says which pixels are transparent.
                let a = if has_alpha {
                    p[3]
                } else {
                    mask.as_ref().and_then(|(_, _, m)| m.get(i * 4)).map_or(255, |&m| if m == 0 { 255 } else { 0 })
                };
                Rgba8Pixel { r: p[2], g: p[1], b: p[0], a }
            })
            .collect();
        Some(RawIcon { width: w as u32, height: h as u32, pixels })
    }
}

/// Starts or stops hosting the tray (only while the `tray` module is in the layout).
pub fn set_enabled(enabled: bool) {
    match (enabled, HOST.get()) {
        (true, None) => start(),
        (false, Some(host)) => {
            unsafe {
                let _ = DestroyWindow(host);
            }
            HOST.set(None);
            ICONS.with_borrow_mut(Vec::clear);
            DIRTY.set(true);
        }
        _ => {}
    }
}

fn start() {
    unsafe {
        let Ok(instance) = GetModuleHandleW(None) else { return };
        let wc = WNDCLASSW { lpfnWndProc: Some(host_proc), hInstance: instance.into(), lpszClassName: TRAY_CLASS, ..Default::default() };
        RegisterClassW(&wc);
        explorer_tray(); // remember Explorer's tray before ours exists
        let Ok(host) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            TRAY_CLASS,
            w!(""),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            return;
        };
        HOST.set(Some(host));
    }
    keep_first();
}

/// Makes sure `Shell_NotifyIcon` still finds our window first. Explorer's taskbar moves
/// above it whenever it is raised; if that happened, move back on top and ask every app to
/// re-register its icons (the same "TaskbarCreated" broadcast Explorer sends on restart).
pub fn keep_first() {
    let Some(host) = HOST.get() else { return };
    unsafe {
        if FindWindowW(TRAY_CLASS, None).ok() == Some(host) && LAST_BROADCAST.get().is_some() {
            return;
        }
        let _ = SetWindowPos(host, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
        // Rate-limited: apps redo some work when asked to re-register.
        if LAST_BROADCAST.get().is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
            LAST_BROADCAST.set(Some(Instant::now()));
            let created = RegisterWindowMessageW(w!("TaskbarCreated"));
            let _ = SendNotifyMessageW(HWND_BROADCAST, created, WPARAM(0), LPARAM(0));
        }
    }
}

/// True (once) when the icons changed since the last call.
pub fn take_dirty() -> bool {
    // Icons of apps that exited without removing them.
    ICONS.with_borrow_mut(|icons| {
        let before = icons.len();
        icons.retain(|i| unsafe { IsWindow(Some(i.hwnd)).as_bool() });
        if icons.len() != before {
            DIRTY.set(true);
        }
    });
    DIRTY.replace(false)
}

/// Calls `f` with every visible icon (in registration order).
pub fn with_icons<R>(f: impl FnOnce(&[&TrayIcon]) -> R) -> R {
    ICONS.with_borrow(|icons| {
        let visible: Vec<&TrayIcon> = icons.iter().filter(|i| !i.hidden && i.icon.is_some()).collect();
        f(&visible)
    })
}

/// Sends the icon's app the same messages Explorer would for a click on it.
pub fn click(exe_and_index: (String, usize), right: bool) {
    ICONS.with_borrow(|icons| {
        let visible: Vec<&TrayIcon> = icons.iter().filter(|i| !i.hidden && i.icon.is_some()).collect();
        let Some(icon) = visible.get(exe_and_index.1).filter(|i| i.exe == exe_and_index.0) else { return };
        unsafe {
            // The app's menu or window must be allowed to come to the front. Handing focus to
            // its tray window first (as Explorer effectively does) keeps Windows from refusing
            // it and flashing the taskbar instead, which pops up an auto-hidden taskbar.
            let _ = AllowSetForegroundWindow(icon.pid);
            crate::sys::bring_to_front(icon.hwnd);
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let post = |wparam: usize, lparam: usize| {
                let _ = PostMessageW(Some(icon.hwnd), icon.callback, WPARAM(wparam), LPARAM(lparam as isize));
            };
            let mouse: &[u32] = if right {
                &[WM_RBUTTONDOWN, WM_RBUTTONUP, WM_CONTEXTMENU]
            } else {
                &[WM_LBUTTONDOWN, WM_LBUTTONUP, NIN_SELECT]
            };
            if icon.version >= 4 {
                // NOTIFYICON_VERSION_4: wParam = anchor point, lParam = (event, icon id).
                let anchor = ((pt.y as u16 as usize) << 16) | pt.x as u16 as usize;
                for &m in mouse {
                    post(anchor, ((icon.uid as usize) << 16) | m as usize);
                }
            } else {
                // Older versions: wParam = icon id, lParam = mouse message (no menu/select events).
                for &m in &mouse[..2] {
                    post(icon.uid as usize, m as usize);
                }
            }
        }
    });
}

/// Runs `f` (our own AppBar calls) with our tray window out of first place, so the calls
/// go straight to Explorer. Routed through our window they reach Explorer fine for other
/// apps, but not for our own: the call comes from the same thread that runs our window,
/// and Explorer then fails to apply the reservation (verified: other apps' AppBars work).
pub fn bypass<R>(f: impl FnOnce() -> R) -> R {
    let host = HOST.get();
    if let Some(host) = host {
        unsafe {
            let _ = SetWindowPos(host, Some(HWND_NOTOPMOST), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
        }
    }
    let result = f();
    if let Some(host) = host {
        unsafe {
            let _ = SetWindowPos(host, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
        }
    }
    result
}
