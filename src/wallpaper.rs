//! Accent color from the desktop wallpaper (`[theme] accent_from_wallpaper`).
//!
//! The slow timer calls `check()`: when the wallpaper file (or its modification time) changed,
//! a background thread decodes a small version of it, finds the dominant vivid color and
//! stores it. `take_changed()` then tells the UI thread to re-apply the theme; the palette
//! reads the color through `dominant()` and adjusts it to suit a dark or light theme.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Gdi::{COLOR_BACKGROUND, GetSysColor};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, IWICImagingFactory, IWICPalette, WICBitmapDitherTypeNone,
    WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::UI::WindowsAndMessaging::{SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW};
use windows::core::HSTRING;

/// The picked color as 0xRRGGBBff, before toning.
static DOMINANT: Mutex<Option<u32>> = Mutex::new(None);
/// What the color was computed from: (wallpaper path, its modification time).
static SOURCE: Mutex<Option<(String, Option<SystemTime>)>> = Mutex::new(None);
static CHANGED: AtomicBool = AtomicBool::new(false);
static BUSY: AtomicBool = AtomicBool::new(false);

pub fn dominant() -> Option<u32> {
    *DOMINANT.lock().unwrap()
}

/// True once after a new color was picked.
pub fn take_changed() -> bool {
    CHANGED.swap(false, Ordering::Relaxed)
}

fn wallpaper_path() -> String {
    let mut buf = [0u16; 520];
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            buf.len() as u32,
            Some(buf.as_mut_ptr().cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// Starts picking a new color if the wallpaper changed since last time. Cheap to call often.
pub fn check() {
    let path = wallpaper_path();
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let source = Some((path.clone(), mtime));
    if *SOURCE.lock().unwrap() == source || BUSY.swap(true, Ordering::Relaxed) {
        return;
    }
    *SOURCE.lock().unwrap() = source;
    std::thread::spawn(move || {
        let color = if path.is_empty() {
            // A solid color background.
            let bgr = unsafe { GetSysColor(COLOR_BACKGROUND) };
            let (r, g, b) = (bgr & 0xff, (bgr >> 8) & 0xff, (bgr >> 16) & 0xff);
            Some((r << 24) | (g << 16) | (b << 8) | 0xff)
        } else {
            pixels(&path).and_then(|px| pick(&px))
        };
        let mut d = DOMINANT.lock().unwrap();
        if *d != color {
            *d = color;
            CHANGED.store(true, Ordering::Relaxed);
        }
        BUSY.store(false, Ordering::Relaxed);
    });
}

const SIZE: u32 = 64;

/// The wallpaper shrunk to SIZE x SIZE, as BGRA.
fn pixels(path: &str) -> Option<Vec<u8>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let decoder = factory
            .CreateDecoderFromFilename(&HSTRING::from(path), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)
            .ok()?;
        let frame = decoder.GetFrame(0).ok()?;
        let scaler = factory.CreateBitmapScaler().ok()?;
        scaler.Initialize(&frame, SIZE, SIZE, WICBitmapInterpolationModeFant).ok()?;
        let converter = factory.CreateFormatConverter().ok()?;
        converter
            .Initialize(
                &scaler,
                &GUID_WICPixelFormat32bppBGRA,
                WICBitmapDitherTypeNone,
                None::<&IWICPalette>,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .ok()?;
        let mut buf = vec![0u8; (SIZE * SIZE * 4) as usize];
        converter.CopyPixels(std::ptr::null(), SIZE * 4, &mut buf).ok()?;
        Some(buf)
    }
}

/// (hue 0..360, saturation 0..1, lightness 0..1)
fn to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    let d = max - min;
    if d < 1e-6 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h, s.min(1.0), l)
}

fn from_hsl(h: f32, s: f32, l: f32) -> u32 {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let to = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u32;
    (to(r) << 24) | (to(g) << 16) | (to(b) << 8) | 0xff
}

/// The most prominent vivid color: pixels are binned by hue, weighted towards colorful
/// ones, and the heaviest neighborhood of bins wins. None for a gray-ish image.
fn pick(bgra: &[u8]) -> Option<u32> {
    const BINS: usize = 24;
    // (weight, r, g, b) sums
    let mut bins = [(0f32, 0f32, 0f32, 0f32); BINS];
    for p in bgra.chunks_exact(4) {
        let (r, g, b) = (p[2] as f32 / 255.0, p[1] as f32 / 255.0, p[0] as f32 / 255.0);
        let (max, min) = (r.max(g).max(b), r.min(g).min(b));
        let chroma = max - min;
        if chroma < 0.12 || max < 0.12 {
            continue; // gray, or too dark to tell
        }
        let (h, _, _) = to_hsl(r, g, b);
        let w = chroma * chroma;
        let bin = &mut bins[((h / 360.0 * BINS as f32) as usize).min(BINS - 1)];
        bin.0 += w;
        bin.1 += r * w;
        bin.2 += g * w;
        bin.3 += b * w;
    }
    let total: f32 = bins.iter().map(|b| b.0).sum();
    // Almost no color at all (a black-and-white photo): keep the preset's accent.
    if total < (SIZE * SIZE) as f32 * 0.004 {
        return None;
    }
    let around = |i: usize| (0..3).map(|k| bins[(i + BINS + k - 1) % BINS]).fold((0.0, 0.0, 0.0, 0.0), |a, b| {
        (a.0 + b.0, a.1 + b.1, a.2 + b.2, a.3 + b.3)
    });
    let best = (0..BINS).max_by(|&a, &b| around(a).0.total_cmp(&around(b).0))?;
    let (w, r, g, b) = around(best);
    let to = |v: f32| ((v / w).clamp(0.0, 1.0) * 255.0).round() as u32;
    Some((to(r) << 24) | (to(g) << 16) | (to(b) << 8) | 0xff)
}

/// Adjusts a picked color to work as an accent: light and soft on dark themes, deeper on
/// light ones, and never washed out.
pub fn tone(color: u32, dark_theme: bool) -> u32 {
    let (r, g, b) = ((color >> 24) as f32 / 255.0, ((color >> 16) & 0xff) as f32 / 255.0, ((color >> 8) & 0xff) as f32 / 255.0);
    let (h, s, l) = to_hsl(r, g, b);
    if dark_theme {
        from_hsl(h, s.clamp(0.45, 0.85), l.clamp(0.70, 0.80))
    } else {
        from_hsl(h, s.clamp(0.50, 0.90), l.clamp(0.36, 0.46))
    }
}
