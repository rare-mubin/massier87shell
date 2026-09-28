//! Blur or acrylic behind translucent windows (`[theme] blur`).
//!
//! Uses SetWindowCompositionAttribute's accent policy: undocumented, but what Windows' own
//! shell surfaces and tools like TranslucentTB rely on, and unlike the documented system
//! backdrops it works on frameless popup windows. The windows themselves draw their
//! background with alpha (`Theme.glass`); DWM fills in the effect.

use std::cell::Cell;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::core::{BOOL, s, w};

use crate::config::ThemeConfig;

const ACCENT_DISABLED: u32 = 0;
const ACCENT_ENABLE_BLURBEHIND: u32 = 3;
const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;
const WCA_ACCENT_POLICY: u32 = 19;

#[repr(C)]
struct AccentPolicy {
    state: u32,
    flags: u32,
    /// 0xAABBGGRR tint (used by acrylic).
    gradient: u32,
    animation: u32,
}

#[repr(C)]
struct CompositionData {
    attribute: u32,
    data: *mut std::ffi::c_void,
    size: usize,
}

type SetWindowCompositionAttribute = unsafe extern "system" fn(HWND, *mut CompositionData) -> BOOL;

#[derive(Clone, Copy, Default)]
struct Settings {
    accent: u32,
    bar_opacity: f32,
    popup_opacity: f32,
    /// Background color, 0xRRGGBBAA.
    tint: u32,
}

thread_local! {
    static SETTINGS: Cell<Settings> = Cell::default();
}

/// Takes the current theme settings (call before `apply`).
pub fn configure(theme: &ThemeConfig, background: u32) {
    let accent = match theme.blur.to_ascii_lowercase().as_str() {
        "blur" => ACCENT_ENABLE_BLURBEHIND,
        "acrylic" => ACCENT_ENABLE_ACRYLICBLURBEHIND,
        _ => ACCENT_DISABLED,
    };
    SETTINGS.set(Settings {
        accent,
        bar_opacity: theme.bar_opacity(),
        popup_opacity: theme.popup_opacity(),
        tint: background,
    });
}

/// `apply` now and again once the window has settled: while a window is still being set up
/// (its first show), winit's own transparency setup resets the effect.
pub fn apply_soon(hwnd: HWND, popup: bool) {
    apply(hwnd, popup);
    let raw = hwnd.0 as isize;
    for ms in [50, 250] {
        slint::Timer::single_shot(std::time::Duration::from_millis(ms), move || {
            apply(HWND(raw as *mut _), popup);
        });
    }
}

/// Sets the effect on a bar (`popup` false) or popup window. Opaque windows get none.
pub fn apply(hwnd: HWND, popup: bool) {
    let s = SETTINGS.get();
    let opacity = if popup { s.popup_opacity } else { s.bar_opacity };
    let state = if opacity < 0.995 { s.accent } else { ACCENT_DISABLED };
    let (r, g, b) = ((s.tint >> 24) & 0xff, (s.tint >> 16) & 0xff, (s.tint >> 8) & 0xff);
    let mut policy = AccentPolicy {
        state,
        flags: 0,
        // Nearly clear: the window paints its own tint on top.
        gradient: (0x01 << 24) | (b << 16) | (g << 8) | r,
        animation: 0,
    };
    let mut data = CompositionData {
        attribute: WCA_ACCENT_POLICY,
        data: (&mut policy as *mut AccentPolicy).cast(),
        size: size_of::<AccentPolicy>(),
    };
    unsafe {
        let Ok(user32) = GetModuleHandleW(w!("user32.dll")) else { return };
        let Some(f) = GetProcAddress(user32, s!("SetWindowCompositionAttribute")) else { return };
        let f: SetWindowCompositionAttribute = std::mem::transmute(f);
        let _ = f(hwnd, &mut data);
    }
}
