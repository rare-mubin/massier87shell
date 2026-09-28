//! The "taskbar" module: pinned apps (from the Windows taskbar's pin folder) merged with
//! the apps that currently have windows open on this virtual desktop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT, SIZE, WPARAM};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW, HGDIOBJ,
    ReleaseDC,
};
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile, STGM_READ};
use windows::Win32::System::Threading::{GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, IShellLinkW, SHCreateItemFromParsingName, SIIGBF_ICONONLY,
    ShellExecuteW, ShellLink,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetCursorPos, GetWindowRect, IsZoomed, SW_MAXIMIZE, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    SetWindowPos, GWL_EXSTYLE, GetForegroundWindow, GetWindow, GetWindowLongPtrW, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, SW_RESTORE, SW_SHOWMINNOACTIVE, SW_SHOWNORMAL,
    ShowWindow, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, GetWindowTextW, PostMessageW, SwitchToThisWindow,
    WM_SYSCOMMAND, HWND_TOP, SWP_NOMOVE,
};
use windows::core::{BOOL, HSTRING, Interface, PWSTR, w};

use crate::AppItem;

/// Icon size requested from the shell before the bar tells us the real one.
const DEFAULT_ICON_PX: i32 = 20;

struct Pinned {
    /// What the pin opens: a .lnk, an .exe, or `shell:AppsFolder<AUMID>` for Store apps.
    source: String,
    /// Lower-cased target exe path, used to match running windows. Empty for shortcuts
    /// without a file target (e.g. some Store apps).
    target: String,
}

/// One entry shown in the module.
struct Entry {
    key: String,
    icon_source: String,
    launch: Option<PathBuf>,
    windows: Vec<HWND>,
    /// The pin's source, if pinned.
    pin: Option<String>,
}

#[derive(Default)]
pub struct Taskbar {
    pinned: Vec<Pinned>,
    pinned_stamp: Option<SystemTime>,
    /// Our own pin list (`[taskbar] pinned`); None = follow the Windows taskbar's pins.
    pins_cfg: Option<Vec<String>>,
    pins_changed: bool,
    /// Raw pixels per app (None = the shell had no icon), and the rendered images for
    /// the current style. Changing style only re-renders; it never re-asks the shell.
    raw: HashMap<String, Option<RawIcon>>,
    images: HashMap<String, Image>,
    /// Monochrome tint (0xRRGGBBAA), or None for full color.
    tint: Option<u32>,
    /// Physical pixel size the icons are displayed at.
    icon_px: i32,
    /// Unpinned running apps, in the order they were first seen (so icons don't jump around).
    order: Vec<String>,
    entries: Vec<Entry>,
    signature: String,
}

fn pin_dir() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar"))
}

fn shortcut_target(lnk: &Path) -> String {
    unsafe {
        let Ok(link) = CoCreateInstance::<_, IShellLinkW>(&ShellLink, None, CLSCTX_INPROC_SERVER) else {
            return String::new();
        };
        let Ok(file) = link.cast::<IPersistFile>() else { return String::new() };
        if file.Load(&HSTRING::from(lnk.as_os_str()), STGM_READ).is_err() {
            return String::new();
        }
        let mut buf = [0u16; 520];
        if link.GetPath(&mut buf, std::ptr::null_mut(), 0).is_err() {
            return String::new();
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..n]).to_lowercase()
    }
}

/// A pin from its source, with the key used to match running windows.
fn pin_from_source(source: &str) -> Pinned {
    let lower = source.to_lowercase();
    let target = if lower.ends_with(".lnk") {
        let mut target = shortcut_target(Path::new(source));
        // The File Explorer pin targets a shell folder, not explorer.exe, so it would
        // never match open Explorer windows.
        let stem = Path::new(source).file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
        if target.is_empty() && stem == "file explorer" {
            let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
            target = format!(r"{windir}\explorer.exe").to_lowercase();
        }
        target
    } else if let Some(id) = lower.strip_prefix(r"shell:appsfolder\") {
        id.to_string() // Store app: matched by AUMID
    } else {
        lower
    };
    Pinned { source: source.to_string(), target }
}

/// A readable app name from a pin/launch source.
fn app_name(source: &str) -> String {
    if let Some(id) = source.strip_prefix(r"shell:AppsFolder\") {
        // "Publisher.App_hash!AppName" -> "AppName"
        return id.rsplit('!').next().unwrap_or(id).to_string();
    }
    Path::new(source).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| source.to_string())
}

pub(crate) fn process_path(hwnd: HWND) -> Option<String> {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(process);
        ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// The AppUserModelID of a window, set by Store apps and some desktop apps.
fn window_aumid(hwnd: HWND) -> Option<String> {
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(hwnd).ok()?;
        let value = store.GetValue(&PKEY_AppUserModel_ID).ok()?;
        let s = value.to_string();
        (!s.is_empty()).then_some(s)
    }
}

/// Same rules Explorer uses to decide whether a window gets a taskbar button.
fn is_taskbar_window(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || GetWindowTextLengthW(hwnd) == 0 {
            return false;
        }
        // Our own panels (power menu, tray panel, previews) are not apps; the settings
        // window is, like any other window.
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == GetCurrentProcessId() {
            let mut buf = [0u16; 64];
            let n = GetWindowTextW(hwnd, &mut buf) as usize;
            if !String::from_utf16_lossy(&buf[..n]).ends_with("Settings") {
                return false;
            }
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let owned = GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid());
        if ex & WS_EX_APPWINDOW.0 == 0 && (owned || ex & (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) != 0) {
            return false;
        }
        // Cloaked = on another virtual desktop, or a suspended Store app frame.
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&mut cloaked as *mut u32).cast(), 4);
        cloaked == 0
    }
}

unsafe extern "system" fn collect(hwnd: HWND, list: LPARAM) -> BOOL {
    let list = unsafe { &mut *(list.0 as *mut Vec<HWND>) };
    if is_taskbar_window(hwnd) {
        list.push(hwnd);
    }
    true.into()
}

/// Top-level windows that would appear on the taskbar, front to back.
fn taskbar_windows() -> Vec<HWND> {
    let mut list: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

/// Renders a file, shortcut or `shell:AppsFolder\<AUMID>` icon through the shell.
/// Asks for the icon at exactly `px` physical pixels: the shell then picks the best frame
/// from the icon file and scales it with proper filtering, so nothing gets resampled later.
pub(crate) fn load_icon(source: &str, px: i32) -> Option<RawIcon> {
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(source), None).ok()?;
        let bitmap = factory.GetImage(SIZE { cx: px, cy: px }, SIIGBF_ICONONLY).ok()?;
        let object = HGDIOBJ(bitmap.0);

        let mut bm = BITMAP::default();
        GetObjectW(object, size_of::<BITMAP>() as i32, Some((&mut bm as *mut BITMAP).cast()));
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        if w <= 0 || h <= 0 {
            let _ = DeleteObject(object);
            return None;
        }

        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        let dc = GetDC(None);
        let rows = GetDIBits(dc, bitmap, 0, h as u32, Some(bgra.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        let _ = DeleteObject(object);
        if rows == 0 {
            return None;
        }

        // The shell usually returns premultiplied BGRA, but legacy icons come back with
        // straight alpha or no alpha at all. Feeding those to Slint as premultiplied
        // makes channels exceed alpha, which overflows the software renderer's blending.
        // We normalize everything to straight (non-premultiplied) RGBA.
        let px = bgra.chunks_exact(4);
        let no_alpha = px.clone().all(|p| p[3] == 0);
        let straight = !no_alpha && px.clone().any(|p| p[0] > p[3] || p[1] > p[3] || p[2] > p[3]);
        let pixels = px
            .map(|p| {
                let a = if no_alpha { 255 } else { p[3] };
                let un = |c: u8| {
                    if straight || no_alpha || a == 0 { c } else { ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8 }
                };
                Rgba8Pixel { r: un(p[2]), g: un(p[1]), b: un(p[0]), a }
            })
            .collect();
        Some(RawIcon { width: w as u32, height: h as u32, pixels })
    }
}

/// An icon's pixels as straight-alpha RGBA, kept so we can re-render it in another style.
pub(crate) struct RawIcon {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<Rgba8Pixel>,
}

impl RawIcon {
    /// `tint` = Some(0xRRGGBBAA) renders a monochrome version in that color, keeping the
    /// icon's shading (luminance) so detailed icons don't collapse into flat silhouettes.
    pub fn render(&self, tint: Option<u32>) -> Image {
        let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(self.width, self.height);
        match tint {
            None => buf.make_mut_slice().copy_from_slice(&self.pixels),
            Some(t) => {
                let (tr, tg, tb) = ((t >> 24) as f32, ((t >> 16) & 0xff) as f32, ((t >> 8) & 0xff) as f32);
                for (dst, p) in buf.make_mut_slice().iter_mut().zip(&self.pixels) {
                    let luma = (0.299 * p.r as f32 + 0.587 * p.g as f32 + 0.114 * p.b as f32) / 255.0;
                    let k = 0.45 + 0.55 * luma;
                    *dst = Rgba8Pixel { r: (tr * k) as u8, g: (tg * k) as u8, b: (tb * k) as u8, a: p.a };
                }
            }
        }
        Image::from_rgba8(buf)
    }
}

fn focus(hwnd: HWND) {
    const SC_RESTORE: usize = 0xF120;
    unsafe {
        if IsIconic(hwnd).as_bool() {
            // The same switch Alt+Tab performs. Games (e.g. Valorant) often ignore a plain
            // ShowWindow(SW_RESTORE) but come back for this; SC_RESTORE is the fallback.
            SwitchToThisWindow(hwnd, true);
            // The switch finishes asynchronously, so the window still reads as minimized
            // right now. Only fall back if it really didn't come back: SC_RESTORE sent to a
            // window that did come back maximized knocks it down to its restored size.
            let raw = hwnd.0 as isize;
            slint::Timer::single_shot(std::time::Duration::from_millis(300), move || {
                let hwnd = HWND(raw as *mut _);
                if IsIconic(hwnd).as_bool() {
                    let _ = PostMessageW(Some(hwnd), WM_SYSCOMMAND, WPARAM(SC_RESTORE), LPARAM(0));
                    crate::sys::bring_to_front(hwnd);
                }
            });
        }
        // Allowed because our process just received the click that triggered this.
        crate::sys::bring_to_front(hwnd);
    }
}

/// Minimizes `hwnd` like clicking its taskbar button, but without letting Windows pick
/// who gets focus: left to Windows, minimizing the last visible window hands focus to the
/// Windows taskbar, which makes an auto-hidden taskbar slide up. The next app window gets
/// focus if there is one; otherwise focus simply stays put.
fn minimize(hwnd: HWND) {
    let next = taskbar_windows().into_iter().find(|&h| h != hwnd && !unsafe { IsIconic(h) }.as_bool());
    if let Some(next) = next {
        crate::sys::bring_to_front(next);
    }
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWMINNOACTIVE);
    }
}

fn launch(path: &Path) {
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(path.as_os_str()), None, None, SW_SHOWNORMAL);
    }
}

impl Taskbar {
    fn refresh_pins(&mut self) {
        if let Some(list) = &self.pins_cfg {
            if std::mem::take(&mut self.pins_changed) {
                self.pinned = list.iter().map(|s| pin_from_source(s)).collect();
            }
            return;
        }
        let Some(dir) = pin_dir() else { return };
        let stamp = std::fs::metadata(&dir).and_then(|m| m.modified()).ok();
        if stamp == self.pinned_stamp && !self.pinned.is_empty() {
            return;
        }
        self.pinned_stamp = stamp;
        let mut links: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")))
            .map(|p| {
                let created = std::fs::metadata(&p).and_then(|m| m.created()).unwrap_or(SystemTime::UNIX_EPOCH);
                (created, p)
            })
            .collect();
        // Pin order isn't stored in the folder; pin time is the closest approximation.
        links.sort();
        self.pinned = links.into_iter().map(|(_, lnk)| pin_from_source(&lnk.to_string_lossy())).collect();
    }

    /// Uses our own pin list (None = follow the Windows taskbar's pins).
    pub fn set_pins(&mut self, pins: Option<Vec<String>>) {
        if pins != self.pins_cfg {
            self.pins_cfg = pins;
            self.pins_changed = true;
            self.pinned_stamp = None; // back to Windows' pins: re-read the folder
            self.signature.clear();
        }
    }

    fn pin_sources(&self) -> Vec<String> {
        self.pinned.iter().map(|p| p.source.clone()).collect()
    }

    fn entry_source(e: &Entry) -> String {
        e.pin.clone().unwrap_or_else(|| e.launch.as_ref().map_or(e.icon_source.clone(), |p| p.to_string_lossy().into_owned()))
    }

    /// Pins or unpins app `index`. Returns the new pin list to save.
    pub fn toggle_pin(&mut self, index: usize) -> Option<Vec<String>> {
        let e = self.entries.get(index)?;
        let mut pins = self.pin_sources();
        match &e.pin {
            Some(source) => pins.retain(|p| p != source),
            None => pins.push(Self::entry_source(e)),
        }
        self.set_pins(Some(pins.clone()));
        Some(pins)
    }

    /// Moves app `from` to position `to`. Pinned apps stay in front: dropping an unpinned app
    /// among the pinned ones pins it. Returns the new pin list when pins changed.
    pub fn move_app(&mut self, from: usize, to: usize) -> Option<Vec<String>> {
        if from >= self.entries.len() || from == to {
            return None;
        }
        let pinned_count = self.entries.iter().filter(|e| e.pin.is_some()).count();
        let moved_pinned = self.entries[from].pin.is_some();
        let mut order: Vec<usize> = (0..self.entries.len()).collect();
        let item = order.remove(from);
        let to = if moved_pinned { to.min(pinned_count - 1) } else { to };
        order.insert(to.min(order.len()), item);
        let block = if !moved_pinned && to < pinned_count { pinned_count + 1 } else { pinned_count };

        let new_pins: Vec<String> = order[..block].iter().map(|&i| Self::entry_source(&self.entries[i])).collect();
        self.order = order[block..].iter().map(|&i| self.entries[i].key.clone()).collect();
        self.signature.clear();
        (new_pins != self.pin_sources()).then(|| {
            self.set_pins(Some(new_pins.clone()));
            new_pins
        })
    }

    /// Asks every window of app `index` to close (like "Close all windows").
    pub fn close_windows(&self, index: usize) {
        const WM_CLOSE: u32 = 0x0010;
        for &hwnd in self.entries.get(index).map_or(&[][..], |e| &e.windows[..]) {
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }

    pub fn windows_of(&self, index: usize) -> Vec<HWND> {
        self.entries.get(index).map(|e| e.windows.clone()).unwrap_or_default()
    }

    /// (name, icon, pinned, can launch) for the menu and hover tooltip.
    pub fn info(&mut self, index: usize) -> Option<(String, Image, bool, bool)> {
        let e = self.entries.get(index)?;
        let source = Self::entry_source(e);
        let name = app_name(&source);
        let (key, icon_source, pinned, launchable) = (e.key.clone(), e.icon_source.clone(), e.pin.is_some(), e.launch.is_some());
        Some((name, self.icon(&key, &icon_source), pinned, launchable))
    }

    /// Switches between full-color and monochrome (in `tint`) icons.
    /// Sets the on-screen icon size in physical pixels; icons are re-fetched at that size.
    pub fn set_icon_px(&mut self, px: i32) {
        let px = px.clamp(8, 256);
        if px != self.icon_px {
            self.icon_px = px;
            self.raw.clear();
            self.images.clear();
            self.signature.clear();
        }
    }

    pub fn set_style(&mut self, monochrome: bool, tint: u32) {
        let tint = monochrome.then_some(tint);
        if tint != self.tint {
            self.tint = tint;
            self.images.clear();
            self.signature.clear(); // force the next update to resend items
        }
    }

    fn icon(&mut self, key: &str, source: &str) -> Image {
        if let Some(image) = self.images.get(key) {
            return image.clone();
        }
        let px = if self.icon_px > 0 { self.icon_px } else { DEFAULT_ICON_PX };
        let raw = self.raw.entry(key.to_string()).or_insert_with(|| load_icon(source, px));
        let image = raw.as_ref().map(|r| r.render(self.tint)).unwrap_or_default();
        self.images.insert(key.to_string(), image.clone());
        image
    }

    /// Re-scans windows and pins. Returns the items to show when something changed.
    pub fn update(&mut self) -> Option<Vec<AppItem>> {
        self.refresh_pins();

        // Group windows by app: exe path, or the AUMID for Store apps hosted in ApplicationFrameHost.
        let mut running: Vec<(String, String, HWND)> = Vec::new(); // (key, icon source, hwnd)
        for hwnd in taskbar_windows() {
            let Some(path) = process_path(hwnd) else { continue };
            let lower = path.to_lowercase();
            let (key, source) = if lower.ends_with(r"\applicationframehost.exe") {
                match window_aumid(hwnd) {
                    Some(id) => (id.to_lowercase(), format!(r"shell:AppsFolder\{id}")),
                    None => continue,
                }
            } else {
                (lower, path)
            };
            running.push((key, source, hwnd));
        }

        let mut entries: Vec<Entry> = self
            .pinned
            .iter()
            .map(|p| Entry {
                key: if p.target.is_empty() { p.source.to_lowercase() } else { p.target.clone() },
                icon_source: p.source.clone(),
                launch: Some(PathBuf::from(&p.source)),
                windows: Vec::new(),
                pin: Some(p.source.clone()),
            })
            .collect();

        self.order.retain(|k| running.iter().any(|(rk, _, _)| rk == k));
        for (key, _, _) in &running {
            if !entries.iter().any(|e| &e.key == key) && !self.order.contains(key) {
                self.order.push(key.clone());
            }
        }
        for key in &self.order {
            if !entries.iter().any(|e| &e.key == key) {
                let (_, source, _) = running.iter().find(|(k, _, _)| k == key).unwrap();
                // ShellExecute opens `shell:AppsFolder\<AUMID>` too, so Store apps can relaunch.
                let launch = Some(PathBuf::from(source));
                entries.push(Entry { key: key.clone(), icon_source: source.clone(), launch, windows: Vec::new(), pin: None });
            }
        }
        for (key, _, hwnd) in &running {
            if let Some(e) = entries.iter_mut().find(|e| &e.key == key) {
                e.windows.push(*hwnd);
            }
        }

        let foreground = unsafe { GetForegroundWindow() };
        let signature: String = entries
            .iter()
            .map(|e| format!("{}:{}:{}|", e.key, e.windows.len(), e.windows.contains(&foreground)))
            .collect();
        self.entries = entries;
        if signature == self.signature {
            return None;
        }
        self.signature = signature;

        let rows: Vec<_> = self
            .entries
            .iter()
            .map(|e| (e.key.clone(), e.icon_source.clone(), e.windows.len() as i32, e.windows.contains(&foreground)))
            .collect();
        let items = rows
            .into_iter()
            .map(|(key, source, windows, focused)| AppItem {
                icon: self.icon(&key, &source),
                running: windows > 0,
                focused,
                windows,
            })
            .collect();
        Some(items)
    }

    /// Left click: launch if closed, focus if in the background, minimize if focused,
    /// and cycle through windows when the app has several.
    pub fn click(&self, index: usize) {
        let Some(e) = self.entries.get(index) else { return };
        if e.windows.is_empty() {
            if let Some(path) = &e.launch {
                launch(path);
            }
            return;
        }
        let foreground = unsafe { GetForegroundWindow() };
        match e.windows.iter().position(|&h| h == foreground) {
            Some(_) if e.windows.len() == 1 => minimize(foreground),
            // Windows are front-to-back, so the last one is the least recently used.
            Some(_) => focus(*e.windows.last().unwrap()),
            None => focus(e.windows[0]),
        }
    }

    /// Middle click: start another instance.
    pub fn new_instance(&self, index: usize) {
        if let Some(path) = self.entries.get(index).and_then(|e| e.launch.as_ref()) {
            launch(path);
        }
    }
}

/// "Drag to move windows": when on, dragging empty bar space moves the maximized window
/// below the bar, like grabbing its title bar. Read by the bar and by the shield window.
pub static DRAG_TO_MOVE: AtomicBool = AtomicBool::new(true);

/// The window the bar should act as a title bar for: the focused window if it is
/// maximized on `monitor`, otherwise the frontmost maximized window there (so a small
/// window floating on top doesn't get in the way). Our own windows never count.
fn maximized_window_on(monitor: HMONITOR) -> Option<HWND> {
    let own = unsafe { GetCurrentProcessId() };
    let fits = |h: HWND| unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        pid != own && IsZoomed(h).as_bool() && MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST) == monitor
    };
    let foreground = unsafe { GetForegroundWindow() };
    if is_taskbar_window(foreground) && fits(foreground) {
        return Some(foreground);
    }
    taskbar_windows().into_iter().find(|&h| fits(h))
}

thread_local! {
    /// The window being dragged, and where the cursor grabbed it (offset from its top-left).
    static DRAG: std::cell::Cell<Option<(HWND, POINT)>> = const { std::cell::Cell::new(None) };
}

/// Starts dragging the maximized window on `monitor` as if its title bar had been grabbed:
/// it leaves maximized and lands under the cursor at the same relative spot, like Windows
/// does. Follow up with `drag_move` on every mouse move and `drag_end` on release.
///
/// We move the window ourselves rather than starting Windows' move loop: that loop runs on
/// the other app's thread, which never saw the button go down (the press came to the bar),
/// so it would end immediately.
pub fn drag_begin(monitor: HMONITOR) -> bool {
    if !DRAG_TO_MOVE.load(Ordering::Relaxed) {
        return false;
    }
    let Some(target) = maximized_window_on(monitor) else { return false };
    unsafe {
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let mut maxed = RECT::default();
        let _ = GetWindowRect(target, &mut maxed);
        // Keep the grab point at the same fraction of the window's width.
        let fraction = ((cursor.x - maxed.left) as f32 / (maxed.right - maxed.left).max(1) as f32).clamp(0.0, 1.0);

        let _ = ShowWindow(target, SW_RESTORE);
        let mut normal = RECT::default();
        let _ = GetWindowRect(target, &mut normal);
        let grab = POINT {
            x: ((normal.right - normal.left) as f32 * fraction) as i32,
            y: (16.0 * GetDpiForWindow(target) as f32 / 96.0) as i32, // inside its title bar
        };
        DRAG.set(Some((target, grab)));
        // On top while it moves, but not activated yet: bring_to_front shares input state
        // with the current foreground thread, and undoing that drops the bar's mouse
        // capture, so the drag would stop right here for a window that wasn't focused.
        let _ = SetWindowPos(target, Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        drag_move();
    }
    true
}

pub fn drag_move() {
    let Some((target, grab)) = DRAG.get() else { return };
    unsafe {
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let _ = SetWindowPos(target, None, cursor.x - grab.x, cursor.y - grab.y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// Ends a drag. Dropping the window back onto the bar (the top edge of the display)
/// maximizes it again, like dragging a window to the top of the screen in Windows.
pub fn drag_end(bar_bottom: i32) {
    let Some((target, _)) = DRAG.take() else { return };
    unsafe {
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        if cursor.y <= bar_bottom {
            let _ = ShowWindow(target, SW_MAXIMIZE);
        }
        // The button is up, so taking focus can't break the drag any more.
        crate::sys::bring_to_front(target);
    }
}

/// Brings a window to the front, restoring it if minimized (for the hover preview).
pub fn focus_window(hwnd: HWND) {
    focus(hwnd);
}

/// Asks one window to close (the preview's close button).
pub fn close_window(hwnd: HWND) {
    const WM_CLOSE: u32 = 0x0010;
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}
