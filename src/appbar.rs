//! Registers the window as a Windows AppBar so maximized windows leave room for it,
//! and applies the window styles a shell bar needs.

use crate::config::BarConfig;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::core::{GUID, w};
use windows::core::BOOL;
use windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUND,
    DWMWCP_ROUNDSMALL, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen,
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromPoint, MonitorFromWindow,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{DragDetect, GetCapture, ReleaseCapture, SetCapture};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Shell::{ABE_BOTTOM, ABE_TOP, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, APPBARDATA, SHAppBarMessage};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetCursorPos, GetWindowLongPtrW, GetWindowRect, HWND_TOPMOST, IsWindow, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetWindowLongPtrW, SetWindowPos,
    WM_USER, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    CreateWindowExW, DefWindowProcW, DestroyWindow, IDC_ARROW, LWA_ALPHA, LoadCursorW, MA_NOACTIVATE, RegisterClassW,
    HWND_BOTTOM, SW_HIDE, SWP_NOMOVE, SWP_NOSIZE, WM_ACTIVATE, SetLayeredWindowAttributes, ShowWindow, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WNDCLASSW,
    WS_EX_LAYERED,
    WS_EX_TOPMOST, WS_POPUP,
};

pub struct AppBar {
    hwnd: HWND,
    /// Invisible window covering the whole reserved strip (see `create_shield`).
    shield: Option<HWND>,
}

impl AppBar {
    pub fn new(hwnd: HWND) -> Self {
        unsafe {
            // Tool window: no taskbar button, not in Alt+Tab. No-activate: clicks don't steal focus.
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let ex = (ex | WS_EX_TOOLWINDOW.0 as isize | WS_EX_NOACTIVATE.0 as isize) & !(WS_EX_APPWINDOW.0 as isize);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);

        }
        let bar = Self { hwnd, shield: create_shield() };
        bar.register();
        bar
    }

    /// Registers with Explorer as an AppBar (harmless if already registered).
    fn register(&self) {
        remember(self.hwnd);
        let mut abd = Self::data(self.hwnd);
        abd.uCallbackMessage = WM_USER + 0x87;
        crate::tray::bypass(|| unsafe { SHAppBarMessage(ABM_NEW, &mut abd) });
    }

    /// Whether Windows still keeps the bar's strip out of the display's work area. It can
    /// get lost, e.g. when Explorer restarts (every AppBar must then register again).
    pub fn is_reserved(&self, top: bool) -> bool {
        unsafe {
            let mut rect = RECT::default();
            let _ = GetWindowRect(self.hwnd, &mut rect);
            let monitor = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            if !GetMonitorInfoW(monitor, &mut info).as_bool() {
                return true;
            }
            if top { info.rcWork.top >= rect.bottom } else { info.rcWork.bottom <= rect.top }
        }
    }

    /// Registers again and re-reserves the strip if Windows lost track of it.
    pub fn ensure_reserved(&self, cfg: &BarConfig, border_color: u32, monitor: HMONITOR) {
        if !self.is_reserved(cfg.is_top()) {
            self.register();
            self.place(cfg, border_color, monitor);
        }
    }

    fn data(hwnd: HWND) -> APPBARDATA {
        APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, hWnd: hwnd, ..Default::default() }
    }

    /// Covers the reserved strip with the shield and slots it just below the bar. Only
    /// needed when floating: docked, the bar itself fills the strip.
    fn place_shield(&self, strip: RECT, floating: bool) {
        let Some(shield) = self.shield else { return };
        unsafe {
            if !floating {
                let _ = ShowWindow(shield, SW_HIDE);
                return;
            }
            let (w, h) = (strip.right - strip.left, strip.bottom - strip.top);
            let _ = SetWindowPos(shield, Some(HWND_TOPMOST), strip.left, strip.top, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            // Directly under the bar, so the bar still gets its own clicks.
            let _ = SetWindowPos(shield, Some(self.hwnd), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
        }
    }

    /// Reserves screen space for the bar and moves the window into it.
    pub fn place(&self, cfg: &BarConfig, border_color: u32, monitor: HMONITOR) {
        unsafe {
            let scale = monitor_scale(monitor);
            let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            if !GetMonitorInfoW(monitor, &mut info).as_bool() {
                return;
            }

            let height = (cfg.height * scale).round() as i32;
            let margin = if cfg.floating { (cfg.margin * scale).round() as i32 } else { 0 };
            let thickness = height + 2 * margin;
            let top = cfg.is_top();

            let mut abd = Self::data(self.hwnd);
            abd.uEdge = if top { ABE_TOP } else { ABE_BOTTOM };
            abd.rc = info.rcMonitor;
            let fit = |rc: &mut RECT| {
                if top {
                    rc.bottom = rc.top + thickness;
                } else {
                    rc.top = rc.bottom - thickness;
                }
            };
            fit(&mut abd.rc);
            crate::tray::bypass(|| SHAppBarMessage(ABM_QUERYPOS, &mut abd));
            fit(&mut abd.rc);
            crate::tray::bypass(|| SHAppBarMessage(ABM_SETPOS, &mut abd));

            let rc = abd.rc;
            self.place_shield(rc, cfg.floating);
            // Twice: moving onto a monitor with another DPI makes winit resize the window to
            // Windows' suggested rect; the second call pins the exact rect we want.
            for _ in 0..2 {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    rc.left + margin,
                    rc.top + margin,
                    rc.right - rc.left - 2 * margin,
                    height,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }

            let corners = match cfg.corners.to_ascii_lowercase().as_str() {
                "square" => DWMWCP_DONOTROUND,
                "small" => DWMWCP_ROUNDSMALL,
                _ if !cfg.floating => DWMWCP_DONOTROUND,
                _ => DWMWCP_ROUND,
            };
            set_frame(self.hwnd, corners, cfg.border.then_some(border_color));
        }
    }
}

/// Sets the DWM corner style and 1px border (`None` hides it). `border` is 0xRRGGBBAA.
fn set_frame(hwnd: HWND, corners: DWM_WINDOW_CORNER_PREFERENCE, border: Option<u32>) {
    // DWM wants a COLORREF (0x00BBGGRR).
    let colorref = match border {
        Some(c) => (c >> 24) | (((c >> 16) & 0xff) << 8) | (((c >> 8) & 0xff) << 16),
        None => DWMWA_COLOR_NONE,
    };
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&corners as *const DWM_WINDOW_CORNER_PREFERENCE).cast(),
            size_of_val(&corners) as u32,
        );
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, (&colorref as *const u32).cast(), 4);
    }
}

/// Rounded corners and a themed border for our own frameless windows (the settings window).
pub fn style_floating_window(hwnd: HWND, border: u32) {
    set_frame(hwnd, DWMWCP_ROUND, Some(border));
}

impl Drop for AppBar {
    fn drop(&mut self) {
        forget(self.hwnd);
        if let Some(shield) = self.shield {
            unsafe {
                let _ = DestroyWindow(shield);
            }
        }
        let mut abd = Self::data(self.hwnd);
        crate::tray::bypass(|| unsafe { SHAppBarMessage(ABM_REMOVE, &mut abd) });
    }
}

/// How far (in logical px) a maximized window extends past its monitor's work area.
/// Windows keeps an invisible resize border outside the screen when maximizing; a
/// frameless window has to inset its content by this much or it gets clipped.
pub fn maximized_overhang(hwnd: HWND) -> f32 {
    unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return 0.0;
        }
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return 0.0;
        }
        let px = (info.rcWork.left - rect.left).max(info.rcWork.top - rect.top).max(0);
        px as f32 / (GetDpiForWindow(hwnd) as f32 / 96.0)
    }
}

/// Where to put a `width` x `height` (logical px) popup: under the mouse horizontally,
/// just below a top bar (or above a bottom bar), kept inside the monitor.
pub fn popup_position(bar: HWND, bar_on_top: bool, width: f32, height: f32) -> (i32, i32) {
    unsafe {
        let scale = GetDpiForWindow(bar) as f32 / 96.0;
        let (w, h) = ((width * scale) as i32, (height * scale) as i32);
        let gap = (8.0 * scale) as i32;
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let mut bar_rect = RECT::default();
        let _ = GetWindowRect(bar, &mut bar_rect);
        let monitor = MonitorFromWindow(bar, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(monitor, &mut info);
        let area = info.rcMonitor;
        let x = (cursor.x - w / 2).clamp(area.left + gap, (area.right - w - gap).max(area.left + gap));
        let y = if bar_on_top { bar_rect.bottom + gap } else { bar_rect.top - h - gap };
        (x, y)
    }
}

/// Brings one of our own windows to the front with keyboard focus.
pub fn focus(hwnd: HWND) {
    crate::sys::bring_to_front(hwnd);
}

fn monitor_scale(monitor: HMONITOR) -> f32 {
    let (mut x, mut y) = (96u32, 96u32);
    let _ = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) };
    x as f32 / 96.0
}

unsafe extern "system" fn collect_monitor(monitor: HMONITOR, _: HDC, _: *mut RECT, list: LPARAM) -> BOOL {
    unsafe { (*(list.0 as *mut Vec<HMONITOR>)).push(monitor) };
    true.into()
}

/// The displays that should get a bar: the primary one first, then (if `all`) the rest.
pub fn monitors(all: bool) -> Vec<HMONITOR> {
    let primary = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    let mut list = vec![primary];
    if all {
        let mut every: Vec<HMONITOR> = Vec::new();
        unsafe {
            let _ = EnumDisplayMonitors(None, None, Some(collect_monitor), LPARAM(&mut every as *mut _ as isize));
        }
        list.extend(every.into_iter().filter(|m| *m != primary));
    }
    list
}

unsafe extern "system" fn shield_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize); // clicks must not steal focus
    }
    // Same as empty bar space: a drag here moves the maximized window below.
    match msg {
        WM_LBUTTONDOWN => unsafe {
            let mut pt = POINT { x: (lparam.0 & 0xffff) as i16 as i32, y: ((lparam.0 >> 16) & 0xffff) as i16 as i32 };
            let _ = ClientToScreen(hwnd, &mut pt);
            if DragDetect(hwnd, pt).as_bool()
                && crate::taskbar::drag_begin(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST))
            {
                SetCapture(hwnd); // keep receiving moves after leaving the strip
            }
            return LRESULT(0);
        },
        WM_MOUSEMOVE if unsafe { GetCapture() } == hwnd => {
            crate::taskbar::drag_move();
            return LRESULT(0);
        }
        WM_LBUTTONUP if unsafe { GetCapture() } == hwnd => unsafe {
            let _ = ReleaseCapture();
            crate::taskbar::drag_end(bottom_edge(hwnd));
            return LRESULT(0);
        },
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// A floating bar leaves margin gaps inside its reserved strip. Without a window there,
/// those gaps belong to whatever is underneath (the desktop, or a maximized window's
/// invisible top edge), so they behaved differently depending on the app below. The
/// shield is a practically transparent window that owns those gaps and treats them like
/// empty bar space.
fn create_shield() -> Option<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        let class = w!("massier87shell-shield");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(shield_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW).ok()?,
            ..Default::default()
        };
        RegisterClassW(&wc); // fails harmlessly if already registered by another bar
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            class,
            w!("massier87shell-shield"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .ok()?;
        // Alpha 1/255: invisible, but unlike alpha 0 it still receives mouse input.
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA);
        Some(hwnd)
    }
}

/// Screen y of a window's bottom edge (where a dragged window snaps back to maximized).
pub fn bottom_edge(hwnd: HWND) -> i32 {
    let mut rect = RECT::default();
    let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
    rect.bottom
}

// Windows never releases the strip of an AppBar whose process was killed (crash, Task
// Manager "End task"): it stays reserved until sign-out, and the next bar stacks below it.
// So we note every bar window we register in a small file, and on startup release the
// strips of any noted window that no longer exists.

fn registry_file() -> std::path::PathBuf {
    crate::config::default_path().with_file_name("appbars.txt")
}

fn read_registry() -> Vec<isize> {
    std::fs::read_to_string(registry_file())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

fn write_registry(hwnds: &[isize]) {
    let text: String = hwnds.iter().map(|h| format!("{h}\n")).collect();
    let _ = std::fs::write(registry_file(), text);
}

fn remember(hwnd: HWND) {
    let mut list = read_registry();
    if !list.contains(&(hwnd.0 as isize)) {
        list.push(hwnd.0 as isize);
        write_registry(&list);
    }
}

fn forget(hwnd: HWND) {
    let list: Vec<isize> = read_registry().into_iter().filter(|&h| h != hwnd.0 as isize).collect();
    write_registry(&list);
}

/// Releases the strips left behind by bar windows of an earlier run that ended abnormally.
pub fn release_stale() {
    for value in read_registry() {
        let hwnd = HWND(value as *mut std::ffi::c_void);
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            let mut abd = AppBar::data(hwnd);
            crate::tray::bypass(|| unsafe { SHAppBarMessage(ABM_REMOVE, &mut abd) });
        }
    }
    write_registry(&[]);
}

// Focus sink. When the last visible window is minimized, Windows activates the next window
// in line and, when there is none, falls back to the Windows taskbar, which makes an
// auto-hidden taskbar slide up. This invisible 1x1 window (under the bar, always at the
// bottom of the window stack) is that "next window", so the taskbar is never picked.

thread_local! {
    static FOCUS_SINK: std::cell::Cell<Option<HWND>> = const { std::cell::Cell::new(None) };
}

unsafe extern "system" fn sink_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Activated: stay at the bottom, so it only ever gets picked when nothing else is left.
    if msg == WM_ACTIVATE && (wparam.0 & 0xffff) != 0 {
        unsafe {
            let _ = SetWindowPos(hwnd, Some(HWND_BOTTOM), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

pub fn create_focus_sink() {
    unsafe {
        let Ok(instance) = GetModuleHandleW(None) else { return };
        let class = w!("massier87shell-focus-sink");
        let wc = WNDCLASSW { lpfnWndProc: Some(sink_proc), hInstance: instance.into(), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        // Windows skips tool windows when picking the next window to activate, so the sink
        // is a normal window. Having an owner (a hidden window) keeps it out of the taskbar and
        // Alt+Tab instead. The owner must not be the bar: owned windows share their owner's
        // always-on-top band, and the sink has to stay at the bottom.
        let Ok(owner) = CreateWindowExW(Default::default(), class, w!(""), WS_POPUP, 0, 0, 0, 0, None, None, Some(instance.into()), None)
        else {
            return;
        };
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_LAYERED,
            class,
            w!(""),
            WS_POPUP,
            0,
            0,
            1,
            1,
            Some(owner),
            None,
            Some(instance.into()),
            None,
        ) else {
            return;
        };
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA);
        let _ = SetWindowPos(hwnd, Some(HWND_BOTTOM), 0, 0, 1, 1, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        FOCUS_SINK.set(Some(hwnd));
    }
}

pub fn focus_sink() -> Option<HWND> {
    FOCUS_SINK.get()
}

/// Keeps the focus sink on the current virtual desktop: on any other desktop it would be
/// hidden, and Windows only activates windows that are on the current one.
pub fn move_focus_sink_to(desktop: GUID) {
    let Some(sink) = FOCUS_SINK.get() else { return };
    unsafe {
        use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
        use windows::Win32::UI::Shell::{IVirtualDesktopManager, VirtualDesktopManager};
        if let Ok(manager) = CoCreateInstance::<_, IVirtualDesktopManager>(&VirtualDesktopManager, None, CLSCTX_ALL) {
            let _ = manager.MoveWindowToDesktop(sink, &desktop);
        }
    }
}
