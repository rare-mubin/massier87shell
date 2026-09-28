// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod appbar;
mod apps;
mod audio;
mod autostart;
mod backdrop;
mod config;
mod control;
mod media;
mod network;
mod wallpaper;
mod panel;
mod power;
mod settings;
mod sys;
mod taskbar;
mod temp;
mod tray;
mod winbar;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use config::Config;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::winit_030::{EventResult, WinitWindowAccessor, winit};
use slint::{Color, ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_A, VK_ESCAPE, VK_LCONTROL, VK_LSHIFT, VK_LWIN, VK_N, VK_TAB, VK_X};

slint::include_modules!();

/// The version from Cargo.toml, shown in Settings and by `--version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// One bar window, pinned to one display.
struct BarWindow {
    ui: Bar,
    monitor: HMONITOR,
    hwnd: HWND,
    /// Set once the native window exists and has reserved its screen space.
    appbar: Option<appbar::AppBar>,
}

struct State {
    cfg: Config,
    cfg_path: PathBuf,
    cfg_mtime: Option<SystemTime>,
    bars: Vec<BarWindow>,
    audio: Option<audio::Audio>,
    cpu: sys::Cpu,
    settings: Option<SettingsWindow>,
    save_timer: Timer,
    taskbar: taskbar::Taskbar,
    /// Shared by every bar, so new bars show the apps immediately.
    apps: ModelRc<AppItem>,
    now_playing: media::NowPlaying,
    /// Written by the media thread, picked up by the fast tick.
    media_update: Arc<Mutex<Option<media::NowPlaying>>>,
    media_tx: Sender<media::Command>,
    temp: Arc<temp::Shared>,
    power: Rc<PowerUi>,
    last_stats: Instant,
    /// (exe, index) per tray item id, as last sent to the UI.
    tray_ids: Vec<(String, usize)>,
    tray_ui: Rc<TrayUi>,
    /// Virtual desktop the focus sink was last moved to.
    sink_desktop: Option<windows::core::GUID>,
    app_menu: Rc<apps::AppMenu>,
    preview: Rc<apps::Preview>,
    /// Hover-to-open delay for the taskview module.
    taskview_timer: Timer,
    control: control::Control,
    control_ui: Rc<panel::ControlUi>,
}

impl State {
    fn bar_hwnds(&self) -> Vec<HWND> {
        self.bars.iter().map(|b| b.hwnd).collect()
    }

    fn for_each_sys(&self, f: impl Fn(&Sys<'_>)) {
        for b in &self.bars {
            f(&b.ui.global::<Sys>());
        }
    }

    fn has_module(&self, name: &str) -> bool {
        let m = &self.cfg.modules;
        [&m.left, &m.center, &m.right].iter().any(|list| list.iter().any(|n| n.trim() == name))
    }
}

fn color(rgba: u32) -> Color {
    Color::from_argb_u8((rgba & 0xff) as u8, (rgba >> 24) as u8, (rgba >> 16) as u8, (rgba >> 8) as u8)
}

fn model(v: &[String]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(v.iter().map(|s| SharedString::from(s.trim())).collect::<Vec<_>>()))
}

/// Pushes the theme into a popup's or the settings window's `Theme` global (every window has
/// its own copy).
fn set_theme(t: Theme<'_>, c: &Config, pal: &config::Palette) {
    theme_with_glass(t, c, pal, c.theme.popup_opacity());
}

/// `glass` is the window's background opacity. Below 1, surfaces (pills, tiles, cards) turn
/// translucent too, less so than the background, so the blur shows through everywhere while
/// they still stand out.
fn theme_with_glass(t: Theme<'_>, c: &Config, pal: &config::Palette, glass: f32) {
    let see_through = |rgba: u32, keep: f32| {
        let alpha = if glass < 1.0 { keep + (1.0 - keep) * glass } else { 1.0 };
        color(rgba).with_alpha(alpha)
    };
    t.set_glass(glass);
    t.set_background(color(pal.background));
    t.set_surface(see_through(pal.surface, 0.35));
    t.set_surface_hover(see_through(pal.surface_hover, 0.5));
    t.set_surface_solid(color(pal.surface));
    t.set_foreground(color(pal.foreground));
    t.set_foreground_dim(color(pal.foreground_dim));
    t.set_accent(color(pal.accent));
    t.set_on_accent(color(pal.on_accent));
    t.set_urgent(color(pal.urgent));
    t.set_font_family(c.bar.font.as_str().into());
    t.set_font_size(c.bar.font_size);
    t.set_pill_radius(c.bar.pill_radius);
    t.set_spacing(c.bar.spacing);
    t.set_padding(c.bar.padding);
    t.set_edge_padding(c.bar.edge_padding);
    t.set_pill_background(c.bar.pill_background);
    t.set_window_max_width(c.window.max_width);
    t.set_media_max_width(c.media.max_width);
    t.set_app_icon_size(c.taskbar.icon_size);
    t.set_bar_height(c.bar.height);
    t.set_motion(c.animations.motion());
    t.set_hover_effects(c.animations.hover_effects);
}

/// Theme and layout for one bar (used for new bars and on every config change).
fn configure_bar(ui: &Bar, c: &Config, pal: &config::Palette) {
    theme_with_glass(ui.global::<Theme>(), c, pal, c.theme.bar_opacity());
    ui.set_left_modules(model(&c.modules.left));
    ui.set_center_modules(model(&c.modules.center));
    ui.set_right_modules(model(&c.modules.right));
}

fn apply_config(st: &mut State) {
    let pal = st.cfg.theme.palette();
    backdrop::configure(&st.cfg.theme, pal.background);
    for b in &st.bars {
        configure_bar(&b.ui, &st.cfg, &pal);
        if let Some(ab) = &b.appbar {
            ab.place(&st.cfg.bar, pal.surface_hover, b.monitor);
        }
        backdrop::apply(b.hwnd, false);
    }
    if let Some(w) = &st.settings {
        set_theme(w.global::<Theme>(), &st.cfg, &pal);
        if let Some(hwnd) = hwnd_of(w.window()) {
            backdrop::apply_soon(hwnd, true);
        }
    }
    st.taskbar.set_style(st.cfg.taskbar.monochrome, pal.foreground);
    st.taskbar.set_pins(st.cfg.taskbar.pinned.clone());
    st.temp.enabled.store(st.has_module("temp"), Ordering::Relaxed);
    taskbar::DRAG_TO_MOVE.store(st.cfg.bar.drag_windows, Ordering::Relaxed);
    winbar::set_hidden(st.cfg.bar.hide_windows_taskbar);
    let hotkey_error = winbar::set_hotkey(&st.cfg.bar.taskbar_hotkey).err().map(|e| format!("Shortcut: {e}")).unwrap_or_default();
    if let Some(w) = &st.settings {
        w.set_taskbar_hotkey_error(hotkey_error.into());
    }
    // The tray is only taken over while the module is in the layout.
    tray::set_enabled(st.has_module("tray"));
    refresh_tray(st);
    st.temp.interval_ms.store(st.cfg.stats.interval().as_millis() as u64, Ordering::Relaxed);
}

fn format_clock(fmt: &str) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    // An invalid strftime string makes chrono's Display fail instead of panicking here.
    if write!(s, "{}", chrono::Local::now().format(fmt)).is_err() {
        s = chrono::Local::now().format("%H:%M").to_string();
    }
    s
}

fn push_media(st: &State) {
    let now = &st.now_playing;
    st.for_each_sys(|s| {
        s.set_media_active(now.active);
        s.set_media_playing(now.playing);
        s.set_media_title(now.title.as_str().into());
        s.set_media_artist(now.artist.as_str().into());
    });
}

/// Values that change often: focused window, desktops, volume, media, apps.
fn fast_tick(st: &mut State) {
    // The taskbar shortcut / control panel tile changed "hide Windows taskbar": save it.
    if let Some(hidden) = winbar::take_user_change() {
        st.cfg.bar.hide_windows_taskbar = hidden;
        if config::save(&st.cfg_path, &st.cfg).is_ok() {
            st.cfg_mtime = config::modified(&st.cfg_path); // not a hand edit to hot-reload
        }
        if let Some(w) = &st.settings {
            w.set_hide_windows_taskbar(hidden);
        }
    }
    let title = sys::foreground_title(&st.bar_hwnds());
    let (count, current) = sys::virtual_desktops();
    if let Some(desktop) = sys::current_desktop_id().filter(|d| Some(*d) != st.sink_desktop) {
        st.sink_desktop = Some(desktop);
        appbar::move_focus_sink_to(desktop);
    }
    let volume = st.audio.as_ref().and_then(|a| a.get());
    st.for_each_sys(|s| {
        if let Some(t) = &title {
            s.set_window_title(t.as_str().into());
        }
        s.set_desktop_count(count);
        s.set_desktop_current(current);
        s.set_has_audio(volume.is_some());
        if let Some((vol, muted)) = volume {
            s.set_volume(vol.round());
            s.set_muted(muted);
        }
    });

    winbar::keep_hidden();
    panel::refresh(st);
    if tray::take_dirty() {
        refresh_tray(st);
    }

    let update = st.media_update.lock().unwrap().take();
    if let Some(now) = update {
        st.now_playing = now;
        push_media(st);
    }

    // The app list is only built when the module is in the layout.
    if st.last_stats.elapsed() >= st.cfg.stats.interval() {
        stats_tick(st);
    }

    if st.has_module("taskbar") {
        // Icons are fetched at exactly the size they are drawn at (in physical pixels).
        if let Some(bar) = st.bars.first() {
            let logical = st.cfg.taskbar.icon_size.min(st.cfg.bar.height - 4.0);
            st.taskbar.set_icon_px((logical * bar.ui.window().scale_factor()).round() as i32);
        }
        if let Some(items) = st.taskbar.update() {
            st.apps = ModelRc::new(VecModel::from(items));
            let apps = st.apps.clone();
            st.for_each_sys(|s| s.set_apps(apps.clone()));
        }
    }
}

/// System stats: CPU, RAM, battery, temperature. Runs every `[stats] interval`.
fn stats_tick(st: &mut State) {
    st.last_stats = Instant::now();
    let cpu = st.cpu.sample().round();
    let ram = sys::memory_percent().round();
    let battery = sys::battery();
    let reading = *st.temp.latest.lock().unwrap();
    let fahrenheit = st.cfg.temp.fahrenheit();
    st.for_each_sys(|s| {
        s.set_cpu(cpu);
        s.set_ram(ram);
        s.set_has_battery(battery.is_some());
        if let Some((pct, charging)) = battery {
            s.set_battery(pct);
            s.set_charging(charging);
        }
        s.set_has_temp(reading.is_some());
        if let Some(r) = reading {
            s.set_temp(r.celsius);
            s.set_temp_text(temp::format(r, fahrenheit).into());
            s.set_temp_source(r.source.into());
        }
    });
}

fn clock_tick(st: &State) {
    let clock = format_clock(&st.cfg.clock.format);
    st.for_each_sys(|s| s.set_clock(clock.as_str().into()));
}

/// Everything slow at once, for config changes and new bars.
fn slow_tick(st: &mut State) {
    clock_tick(st);
    stats_tick(st);
}

/// Hot reload: picks up edits made to the config file by hand.
fn reload_if_changed(st: &mut State) {
    let mtime = config::modified(&st.cfg_path);
    if mtime == st.cfg_mtime {
        return;
    }
    st.cfg_mtime = mtime;
    match config::load(&st.cfg_path) {
        Ok(cfg) => {
            st.cfg = cfg;
            apply_config(st);
            slow_tick(st);
            if let Some(w) = &st.settings {
                settings::fill(w, &st.cfg, &st.cfg_path.display().to_string());
            }
            eprintln!("config reloaded");
        }
        Err(e) => eprintln!("config error (keeping previous config): {e}"),
    }
}

fn hwnd_of(window: &slint::Window) -> Option<HWND> {
    let handle = window.window_handle();
    match handle.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut _)),
        _ => None,
    }
}

/// Makes the set of bars match the configured displays: creates bars for new
/// displays and removes bars whose display is gone (or no longer wanted).
fn sync_bars(state: &Rc<RefCell<State>>) {
    let wanted = appbar::monitors(state.borrow().cfg.bar.all_monitors());

    // Windows are created and destroyed outside the borrow: that can dispatch window
    // messages, and handlers for those may need `State` themselves.
    let removed: Vec<BarWindow> = {
        let mut st = state.borrow_mut();
        let (keep, gone) = std::mem::take(&mut st.bars).into_iter().partition(|b| wanted.contains(&b.monitor));
        st.bars = keep;
        gone
    };
    for mut b in removed {
        b.appbar.take(); // releases the reserved screen space
        let _ = b.ui.hide();
    }

    let missing: Vec<HMONITOR> =
        wanted.into_iter().filter(|m| !state.borrow().bars.iter().any(|b| b.monitor == *m)).collect();
    if missing.is_empty() {
        return;
    }
    for monitor in missing {
        NEXT_WINDOW.store(WINDOW_BAR, Ordering::Relaxed);
        let Ok(ui) = Bar::new() else { continue };
        connect_callbacks(&ui, state);
        {
            let st = state.borrow();
            configure_bar(&ui, &st.cfg, &st.cfg.theme.palette());
            ui.global::<Sys>().set_apps(st.apps.clone());
        }
        let _ = ui.show();
        state.borrow_mut().bars.push(BarWindow { ui, monitor, hwnd: HWND::default(), appbar: None });
    }
    {
        let mut st = state.borrow_mut();
        push_media(&st);
        fast_tick(&mut st);
        slow_tick(&mut st);
        refresh_tray(&mut st);
    }
    attach_bars(state);
}

/// Turns newly created bar windows into AppBars once their native windows exist
/// (winit creates them lazily, so this retries briefly).
fn attach_bars(state: &Rc<RefCell<State>>) {
    let mut pending = false;
    {
        let mut st = state.borrow_mut();
        let State { bars, cfg, .. } = &mut *st;
        let border = cfg.theme.palette().surface_hover;
        for b in bars.iter_mut().filter(|b| b.appbar.is_none()) {
            match hwnd_of(b.ui.window()) {
                Some(hwnd) => {
                    b.hwnd = hwnd;
                    let ab = appbar::AppBar::new(hwnd);
                    ab.place(&cfg.bar, border, b.monitor);
                    backdrop::apply_soon(hwnd, false);
                    b.appbar = Some(ab);
                }
                None => pending = true,
            }
        }
    }
    if pending {
        let st = state.clone();
        Timer::single_shot(Duration::from_millis(30), move || attach_bars(&st));
    }
}

/// Saves the config a moment after the last change, so dragging a slider writes once.
fn schedule_save(state: &Rc<RefCell<State>>) {
    let st = state.clone();
    state.borrow().save_timer.start(TimerMode::SingleShot, Duration::from_millis(400), move || {
        let mut st = st.borrow_mut();
        let status = match config::save(&st.cfg_path, &st.cfg) {
            Ok(()) => format!("Saved · {}", chrono::Local::now().format("%H:%M:%S")),
            Err(e) => format!("Save failed: {e}"),
        };
        // Our own write must not trigger a hot reload.
        st.cfg_mtime = config::modified(&st.cfg_path);
        if let Some(w) = &st.settings {
            w.set_status(status.into());
        }
    });
}

/// Applies the edited config to every bar and queues a save.
fn commit(state: &Rc<RefCell<State>>) {
    {
        let mut st = state.borrow_mut();
        apply_config(&mut st);
        fast_tick(&mut st);
        slow_tick(&mut st);
    }
    sync_bars(state);
    schedule_save(state);
}

fn open_settings(state: &Rc<RefCell<State>>) {
    if let Some(w) = &state.borrow().settings {
        let st = state.borrow();
        settings::fill(w, &st.cfg, &st.cfg_path.display().to_string());
        if w.window().is_minimized() {
            w.window().set_minimized(false);
        }
        let _ = w.show();
        if let Some(hwnd) = hwnd_of(w.window()) {
            sys::bring_to_front(hwnd);
        }
        return;
    }
    let Ok(w) = SettingsWindow::new() else { return };
    {
        let st = state.borrow();
        set_theme(w.global::<Theme>(), &st.cfg, &st.cfg.theme.palette());
    }

    // Custom title bar: the window has no native frame, so move/resize go through winit.
    let ww = w.as_weak();
    w.on_start_drag(move || {
        if let Some(w) = ww.upgrade() {
            w.window().with_winit_window(|win| {
                let _ = win.drag_window();
            });
        }
    });
    let ww = w.as_weak();
    w.on_start_resize(move |dir| {
        use winit::window::ResizeDirection as R;
        let dir = match dir {
            0 => R::North,
            1 => R::South,
            2 => R::East,
            3 => R::West,
            6 => R::SouthEast,
            7 => R::SouthWest,
            _ => return,
        };
        if let Some(w) = ww.upgrade() {
            w.window().with_winit_window(|win| {
                let _ = win.drag_resize_window(dir);
            });
        }
    });
    let ww = w.as_weak();
    w.on_minimize(move || {
        if let Some(w) = ww.upgrade() {
            w.window().set_minimized(true);
        }
    });
    let ww = w.as_weak();
    w.on_toggle_maximize(move || {
        if let Some(w) = ww.upgrade() {
            let max = !w.window().is_maximized();
            w.window().set_maximized(max);
            w.set_is_max(max);
            // Measure once the resize has landed.
            let ww = w.as_weak();
            Timer::single_shot(Duration::from_millis(80), move || {
                if let Some(w) = ww.upgrade() {
                    let inset = hwnd_of(w.window()).map_or(0.0, appbar::maximized_overhang);
                    w.set_max_inset(inset);
                }
            });
        }
    });
    let ww = w.as_weak();
    w.on_close_window(move || {
        if let Some(w) = ww.upgrade() {
            let _ = w.hide();
        }
    });

    let (st, ww) = (state.clone(), w.as_weak());
    w.on_changed(move || {
        let Some(w) = ww.upgrade() else { return };
        {
            let mut s = st.borrow_mut();
            let preset_before = s.cfg.theme.preset.clone();
            settings::read(&w, &mut s.cfg);
            w.set_clock_preview(settings::clock_preview(&s.cfg.clock.format).into());
            if s.cfg.theme.preset != preset_before {
                settings::fill_colors(&w, &s.cfg);
            }
        }
        commit(&st);
    });

    let (st, ww) = (state.clone(), w.as_weak());
    w.on_control_option_toggled(move |id, on| {
        let Some(w) = ww.upgrade() else { return };
        {
            let mut s = st.borrow_mut();
            settings::toggle_control_option(&mut s.cfg, &id, on);
            settings::fill_control(&w, &s.cfg);
        }
        commit(&st);
    });

    let ww = w.as_weak();
    w.on_autostart_toggled(move |enabled| {
        let Some(w) = ww.upgrade() else { return };
        match autostart::set(enabled) {
            Ok(()) => w.set_status(if enabled { "Starts with Windows" } else { "Autostart off" }.into()),
            Err(e) => w.set_status(format!("Autostart failed: {e}").into()),
        }
        // Show what Windows will actually do.
        w.set_autostart(autostart::is_enabled());
    });

    let (st, ww) = (state.clone(), w.as_weak());
    w.on_color_edited(move |index, text| {
        let Some(w) = ww.upgrade() else { return };
        let Some(rgba) = config::parse_color(&text) else { return }; // still typing
        {
            let mut s = st.borrow_mut();
            let Some(slot) = s.cfg.theme.override_mut(index as usize) else { return };
            *slot = Some(format!("#{}", text.trim().trim_start_matches('#').to_ascii_lowercase()));
            // Only refresh this row's preview; rebuilding would steal focus from the text box.
            if let Some(mut row) = w.get_colors().row_data(index as usize) {
                row.hex = text;
                row.preview = color(rgba);
                row.overridden = true;
                w.get_colors().set_row_data(index as usize, row);
            }
            if index as usize == settings::ACCENT_INDEX {
                w.set_accent(color(rgba));
            }
        }
        commit(&st);
    });

    let (st, ww) = (state.clone(), w.as_weak());
    w.on_swatch_picked(move |hex| {
        let Some(w) = ww.upgrade() else { return };
        {
            let mut s = st.borrow_mut();
            if let Some(slot) = s.cfg.theme.override_mut(settings::ACCENT_INDEX) {
                *slot = Some(hex.to_string());
            }
            settings::fill_colors(&w, &s.cfg);
        }
        commit(&st);
    });

    let (st, ww) = (state.clone(), w.as_weak());
    w.on_color_reset(move |index| {
        let Some(w) = ww.upgrade() else { return };
        {
            let mut s = st.borrow_mut();
            if let Some(slot) = s.cfg.theme.override_mut(index as usize) {
                *slot = None;
            }
            settings::fill_colors(&w, &s.cfg);
        }
        commit(&st);
    });

    // Every module edit: change the config, refresh the editor rows, apply + save.
    let module_op = |op: Box<dyn Fn(&mut Config, i32, i32, &str)>| {
        let (st, ww) = (state.clone(), w.as_weak());
        move |a: i32, b: i32, name: &str| {
            let Some(w) = ww.upgrade() else { return };
            {
                let mut s = st.borrow_mut();
                op(&mut s.cfg, a, b, name);
                settings::fill_modules(&w, &s.cfg);
            }
            commit(&st);
        }
    };
    let add = module_op(Box::new(|cfg, section, _, name| {
        if let Some(list) = settings::section(cfg, section).filter(|_| !name.is_empty()) {
            list.push(name.to_string());
        }
    }));
    w.on_module_add(move |section, name| add(section, 0, &name));
    let drop = module_op(Box::new(|cfg, from, to, _| settings::drop_row(cfg, from as usize, to as usize)));
    w.on_module_drop(move |from, to| drop(from, to, ""));
    let remove = module_op(Box::new(|cfg, index, _, _| settings::remove_row(cfg, index as usize)));
    w.on_module_remove_row(move |index| remove(index, 0, ""));

    let st = state.clone();
    w.on_open_config_file(move || {
        let _ = std::process::Command::new("notepad.exe").arg(&st.borrow().cfg_path).spawn();
    });

    let (st, ww) = (state.clone(), w.as_weak());
    w.on_reset_defaults(move || {
        let Some(w) = ww.upgrade() else { return };
        {
            let mut s = st.borrow_mut();
            let _ = std::fs::write(&s.cfg_path, config::DEFAULT_CONFIG);
            s.cfg = config::load(&s.cfg_path).unwrap_or_default();
            s.cfg_mtime = config::modified(&s.cfg_path);
            settings::fill(&w, &s.cfg, &s.cfg_path.display().to_string());
            w.set_status("Defaults restored".into());
            apply_config(&mut s);
        }
        sync_bars(&st);
    });

    {
        let st = state.borrow();
        settings::fill(&w, &st.cfg, &st.cfg_path.display().to_string());
    }
    let _ = w.show();
    // Windows 11 rounded corners + themed 1px border for the frameless window.
    let (ww, st) = (w.as_weak(), state.clone());
    Timer::single_shot(Duration::from_millis(50), move || {
        if let Some(hwnd) = ww.upgrade().and_then(|w| hwnd_of(w.window())) {
            appbar::style_floating_window(hwnd, st.borrow().cfg.theme.palette().surface_hover);
            backdrop::apply_soon(hwnd, true);
            // Opened from the bar, which never takes focus: without this it can open behind.
            sys::bring_to_front(hwnd);
        }
    });
    state.borrow_mut().settings = Some(w);
}

/// The power menu keeps its own state instead of living inside `State`: Windows can
/// deliver its focus-lost event synchronously while `State` is borrowed.
#[derive(Default)]
struct PowerUi {
    menu: RefCell<Option<PowerMenu>>,
    /// When the menu last closed, so the click that closed it (by stealing focus)
    /// doesn't immediately reopen it.
    hidden_at: Cell<Option<Instant>>,
    disarm: Timer,
}

fn hide_power(p: &PowerUi) {
    if let Some(m) = p.menu.borrow().as_ref() {
        if m.window().is_visible() {
            if let Some(h) = hwnd_of(m.window()) {
                sys::stop_outside_watch(h);
            }
            m.set_open(false);
            m.set_armed(-1);
            let _ = m.hide();
            p.hidden_at.set(Some(Instant::now()));
        }
    }
}

fn create_power(p: &Rc<PowerUi>, state: &Rc<RefCell<State>>) -> Option<PowerMenu> {
    NEXT_WINDOW.store(WINDOW_POPUP, Ordering::Relaxed);
    let m = PowerMenu::new().ok()?;
    let name = power::user_name();
    m.set_initial(power::initial(&name).into());
    m.set_user_name(name.into());
    m.set_can_hibernate(power::can_hibernate());

    let pw = Rc::downgrade(p);
    m.window().on_winit_window_event(move |_, event| {
        if let winit::event::WindowEvent::Focused(false) = event {
            if let Some(p) = pw.upgrade() {
                hide_power(&p);
            }
        }
        EventResult::Propagate
    });

    let pw = Rc::downgrade(p);
    m.on_run(move |action| {
        if let Some(p) = pw.upgrade() {
            hide_power(&p);
        }
        power::run(action);
    });
    let (pw, mw) = (Rc::downgrade(p), m.as_weak());
    m.on_arm(move |index| {
        let (Some(p), Some(m)) = (pw.upgrade(), mw.upgrade()) else { return };
        m.set_armed(index);
        let mw = mw.clone();
        p.disarm.start(TimerMode::SingleShot, Duration::from_millis(3500), move || {
            if let Some(m) = mw.upgrade() {
                m.set_armed(-1);
            }
        });
    });
    let (pw, st) = (Rc::downgrade(p), state.clone());
    m.on_open_settings(move || {
        if let Some(p) = pw.upgrade() {
            hide_power(&p);
        }
        open_settings(&st);
    });
    m.on_quit_bar(|| {
        let _ = slint::quit_event_loop();
    });
    let pw = Rc::downgrade(p);
    m.on_dismiss(move || {
        if let Some(p) = pw.upgrade() {
            hide_power(&p);
        }
    });
    Some(m)
}

/// Opens (or closes) the power menu under the power button of the bar that was clicked.
fn toggle_power(state: &Rc<RefCell<State>>, bar: &slint::Weak<Bar>) {
    let p = state.borrow().power.clone();
    if p.menu.borrow().as_ref().is_some_and(|m| m.window().is_visible()) {
        hide_power(&p);
        return;
    }
    if p.hidden_at.get().is_some_and(|t| t.elapsed() < Duration::from_millis(300)) {
        return; // this click is what just closed it
    }
    let Some(bar_hwnd) = bar.upgrade().and_then(|b| hwnd_of(b.window())) else { return };
    if p.menu.borrow().is_none() {
        let Some(m) = create_power(&p, state) else { return };
        *p.menu.borrow_mut() = Some(m);
    }
    let menu = p.menu.borrow();
    let Some(m) = menu.as_ref() else { return };

    let (top, border) = {
        let st = state.borrow();
        let pal = st.cfg.theme.palette();
        set_theme(m.global::<Theme>(), &st.cfg, &pal);
        (st.cfg.bar.is_top(), pal.surface_hover)
    };
    m.set_uptime(power::uptime().into());
    m.set_armed(-1);
    m.set_open(false);

    let pw = Rc::downgrade(&p);
    show_popup(m, bar_hwnd, top, border, Rc::new(move || {
        if let Some(p) = pw.upgrade() {
            hide_power(&p);
        }
    }));
    // Next frame: play the slide/fade-in.
    let mw = m.as_weak();
    Timer::single_shot(Duration::from_millis(16), move || {
        if let Some(m) = mw.upgrade() {
            m.set_open(true);
        }
    });
}

/// Shows a drop-down panel under the mouse, just below (or above) the bar, focused.
/// `on_outside` runs when the user clicks anywhere else.
fn show_popup<C: ComponentHandle + 'static>(
    popup: &C,
    bar_hwnd: HWND,
    bar_on_top: bool,
    border: u32,
    on_outside: Rc<dyn Fn()>,
) {
    let window = popup.window();
    let _ = window.show();
    // Measure after showing: before its first show the window has no size yet.
    let size = window.size().to_logical(window.scale_factor());
    let (x, y) = appbar::popup_position(bar_hwnd, bar_on_top, size.width, size.height);
    window.set_position(slint::PhysicalPosition::new(x, y));
    finish_popup(popup.as_weak(), border, on_outside, 0);
}

/// Styles a popup once its native window exists: the winit backend creates it a moment
/// after `show()`, so right after showing there is no HWND yet to style.
fn finish_popup<C: ComponentHandle + 'static>(popup: slint::Weak<C>, border: u32, on_outside: Rc<dyn Fn()>, tries: u32) {
    let Some(p) = popup.upgrade() else { return };
    if !p.window().is_visible() {
        return;
    }
    match hwnd_of(p.window()) {
        Some(hwnd) => {
            appbar::style_floating_window(hwnd, border);
            backdrop::apply_soon(hwnd, true);
            appbar::focus(hwnd);
            sys::close_on_outside_click(hwnd, on_outside);
        }
        None if tries < 50 => {
            Timer::single_shot(Duration::from_millis(10), move || finish_popup(popup, border, on_outside, tries + 1));
        }
        None => {}
    }
}

/// The background apps panel. Like `PowerUi`, kept outside `State` for the same reason.
#[derive(Default)]
struct TrayUi {
    menu: RefCell<Option<TrayMenu>>,
    hidden_at: Cell<Option<Instant>>,
}

fn hide_tray_menu(t: &TrayUi) {
    if let Some(m) = t.menu.borrow().as_ref() {
        if m.window().is_visible() {
            if let Some(h) = hwnd_of(m.window()) {
                sys::stop_outside_watch(h);
            }
            m.set_open(false);
            let _ = m.hide();
            t.hidden_at.set(Some(Instant::now()));
        }
    }
}

/// Rebuilds the tray models: every icon for the panel, pinned ones for the bars.
fn refresh_tray(st: &mut State) {
    let tint = st.cfg.taskbar.monochrome.then(|| st.cfg.theme.palette().foreground);
    let pinned = &st.cfg.tray.pinned;
    let (ids, items): (Vec<(String, usize)>, Vec<TrayItem>) = tray::with_icons(|icons| {
        icons
            .iter()
            .enumerate()
            .map(|(i, icon)| {
                let item = TrayItem {
                    id: i as i32,
                    icon: icon.icon.as_ref().map(|r| r.render(tint)).unwrap_or_default(),
                    name: icon.name().into(),
                    pinned: pinned.contains(&icon.exe),
                };
                ((icon.exe.clone(), i), item)
            })
            .unzip()
    });
    st.tray_ids = ids;
    let bar_items: Vec<TrayItem> = items.iter().filter(|i| i.pinned).cloned().collect();
    let bar_model = ModelRc::new(VecModel::from(bar_items));
    let count = items.len() as i32;
    st.for_each_sys(|s| {
        s.set_tray_pinned(bar_model.clone());
        s.set_tray_count(count);
    });
    if let Some(m) = st.tray_ui.menu.borrow().as_ref() {
        m.set_items(ModelRc::new(VecModel::from(items)));
    }
}

fn tray_click(state: &Rc<RefCell<State>>, id: i32, right: bool) {
    // Cloned out first: posting to the app must not happen while `State` is borrowed.
    let target = state.borrow().tray_ids.get(id as usize).cloned();
    if let Some(target) = target {
        tray::click(target, right);
    }
}

fn toggle_tray_menu(state: &Rc<RefCell<State>>, bar: &slint::Weak<Bar>) {
    let t = state.borrow().tray_ui.clone();
    if t.menu.borrow().as_ref().is_some_and(|m| m.window().is_visible()) {
        hide_tray_menu(&t);
        return;
    }
    if t.hidden_at.get().is_some_and(|at| at.elapsed() < Duration::from_millis(300)) {
        return; // this click is what just closed it
    }
    let Some(bar_hwnd) = bar.upgrade().and_then(|b| hwnd_of(b.window())) else { return };
    if t.menu.borrow().is_none() {
        NEXT_WINDOW.store(WINDOW_POPUP, Ordering::Relaxed);
        let Ok(m) = TrayMenu::new() else { return };
        let tw = Rc::downgrade(&t);
        m.window().on_winit_window_event(move |_, event| {
            if let winit::event::WindowEvent::Focused(false) = event {
                if let Some(t) = tw.upgrade() {
                    hide_tray_menu(&t);
                }
            }
            EventResult::Propagate
        });
        let st = state.clone();
        m.on_activate(move |id, right| tray_click(&st, id, right));
        let st = state.clone();
        m.on_toggle_pin(move |id| {
            {
                let mut s = st.borrow_mut();
                let Some((exe, _)) = s.tray_ids.get(id as usize).cloned() else { return };
                let pinned = &mut s.cfg.tray.pinned;
                match pinned.iter().position(|p| *p == exe) {
                    Some(i) => {
                        pinned.remove(i);
                    }
                    None => pinned.push(exe),
                }
                refresh_tray(&mut s);
            }
            schedule_save(&st);
        });
        let tw = Rc::downgrade(&t);
        m.on_dismiss(move || {
            if let Some(t) = tw.upgrade() {
                hide_tray_menu(&t);
            }
        });
        *t.menu.borrow_mut() = Some(m);
    }
    let (top, border) = {
        let mut st = state.borrow_mut();
        refresh_tray(&mut st); // fills the panel's list
        let pal = st.cfg.theme.palette();
        if let Some(m) = t.menu.borrow().as_ref() {
            set_theme(m.global::<Theme>(), &st.cfg, &pal);
        }
        (st.cfg.bar.is_top(), pal.surface_hover)
    };
    let menu = t.menu.borrow();
    let Some(m) = menu.as_ref() else { return };
    m.set_open(false);
    let tw = Rc::downgrade(&t);
    show_popup(m, bar_hwnd, top, border, Rc::new(move || {
        if let Some(t) = tw.upgrade() {
            hide_tray_menu(&t);
        }
    }));
    let mw = m.as_weak();
    Timer::single_shot(Duration::from_millis(16), move || {
        if let Some(m) = mw.upgrade() {
            m.set_open(true);
        }
    });
}

fn connect_callbacks(ui: &Bar, state: &Rc<RefCell<State>>) {
    let s = ui.global::<Sys>();

    let st = state.clone();
    s.on_settings_clicked(move || open_settings(&st));
    let st = state.clone();
    s.on_tray_activate(move |id, right| tray_click(&st, id, right));
    let (st, weak) = (state.clone(), ui.as_weak());
    s.on_tray_open(move || toggle_tray_menu(&st, &weak));
    let st = state.clone();
    s.on_app_clicked(move |i| {
        apps::hide_preview(&st);
        st.borrow().taskbar.click(i as usize);
    });
    let st = state.clone();
    s.on_app_moved(move |from, to| {
        apps::hide_preview(&st);
        let pins = st.borrow_mut().taskbar.move_app(from as usize, to as usize);
        if let Some(pins) = pins {
            st.borrow_mut().cfg.taskbar.pinned = Some(pins);
            schedule_save(&st);
        }
        fast_tick(&mut st.borrow_mut()); // show the new order right away
    });
    let (st, weak) = (state.clone(), ui.as_weak());
    s.on_app_context(move |i| apps::open_menu(&st, &weak, i as usize));
    let (st, weak) = (state.clone(), ui.as_weak());
    s.on_app_hover(move |i, hovered, x| apps::app_hover(&st, &weak, i, hovered, x));
    let st = state.clone();
    s.on_app_middle_clicked(move |i| st.borrow().taskbar.new_instance(i as usize));

    // Empty bar space as the maximized window's title bar.
    let weak = ui.as_weak();
    s.on_bar_drag_begin(move || {
        let Some(hwnd) = weak.upgrade().and_then(|ui| hwnd_of(ui.window())) else { return false };
        taskbar::drag_begin(unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) })
    });
    s.on_bar_drag_move(taskbar::drag_move);
    let weak = ui.as_weak();
    s.on_bar_drag_end(move || {
        let bottom = weak.upgrade().and_then(|ui| hwnd_of(ui.window())).map_or(0, appbar::bottom_edge);
        taskbar::drag_end(bottom);
    });
    s.on_launcher_clicked(|| sys::send_shortcut(&[VK_LWIN]));
    s.on_taskview_clicked(|| sys::send_shortcut(&[VK_LWIN, VK_TAB]));
    let (st, weak) = (state.clone(), ui.as_weak());
    s.on_control_clicked(move || panel::toggle(&st, &weak));
    let st = state.clone();
    s.on_taskview_hover(move |hovered| {
        let s = st.borrow();
        if !s.cfg.taskview.trigger.eq_ignore_ascii_case("hover") {
            return;
        }
        if hovered {
            let delay = Duration::from_millis(s.cfg.taskview.hover_delay_ms.clamp(100, 3000) as u64);
            s.taskview_timer.start(TimerMode::SingleShot, delay, || sys::send_shortcut(&[VK_LWIN, VK_TAB]));
        } else {
            s.taskview_timer.stop();
        }
    });
    s.on_clock_clicked(|| sys::send_shortcut(&[VK_LWIN, VK_N]));
    s.on_stats_clicked(|| sys::send_shortcut(&[VK_LCONTROL, VK_LSHIFT, VK_ESCAPE]));
    s.on_volume_right_clicked(|| sys::send_shortcut(&[VK_LWIN, VK_A]));
    let (st, weak) = (state.clone(), ui.as_weak());
    s.on_power_clicked(move || toggle_power(&st, &weak));
    s.on_power_right_clicked(|| sys::send_shortcut(&[VK_LWIN, VK_X]));

    let weak = ui.as_weak();
    s.on_desktop_clicked(move |target| {
        if let Some(ui) = weak.upgrade() {
            sys::switch_desktop(ui.global::<Sys>().get_desktop_current(), target);
        }
    });
    let weak = ui.as_weak();
    s.on_desktop_scrolled(move |dir| {
        if let Some(ui) = weak.upgrade() {
            let s = ui.global::<Sys>();
            let (cur, count) = (s.get_desktop_current(), s.get_desktop_count());
            let target = (cur + dir).clamp(0, count - 1);
            sys::switch_desktop(cur, target);
        }
    });

    // Devices and per-app volume (the speaker icon in it mutes).
    let (st, weak) = (state.clone(), ui.as_weak());
    s.on_volume_clicked(move || panel::toggle_audio(&st, &weak));
    let st = state.clone();
    s.on_volume_scrolled(move |dir| {
        let mut st = st.borrow_mut();
        if let Some(a) = &st.audio {
            a.adjust(dir as f32 * st.cfg.volume.step);
        }
        fast_tick(&mut st);
    });

    let st = state.clone();
    s.on_media_prev(move || {
        let _ = st.borrow().media_tx.send(media::Command::Prev);
    });
    let st = state.clone();
    s.on_media_toggle(move || {
        // Flip the icon right away; the media thread confirms it a moment later.
        let mut st = st.borrow_mut();
        st.now_playing.playing = !st.now_playing.playing;
        push_media(&st);
        let _ = st.media_tx.send(media::Command::Toggle);
    });
    let st = state.clone();
    s.on_media_next(move || {
        let _ = st.borrow().media_tx.send(media::Command::Next);
    });
}

// winit's window-attributes hook is global, so whoever creates a window says what kind
// it is here first. Consumed (reset to normal) by the hook.
const WINDOW_NORMAL: u8 = 0;
const WINDOW_BAR: u8 = 1;
const WINDOW_POPUP: u8 = 2;
/// Hover previews: never activated, no taskbar button.
const WINDOW_TOOLTIP: u8 = 3;
static NEXT_WINDOW: AtomicU8 = AtomicU8::new(WINDOW_NORMAL);

/// `renderer` is "software" (partial redraws, default), "skia" (Direct3D) or "femtovg" (OpenGL).
fn select_backend(renderer: &str) {
    let hook = |attrs: winit::window::WindowAttributes| {
        use winit::platform::windows::WindowAttributesExtWindows;
        use winit::window::WindowLevel;
        match NEXT_WINDOW.swap(WINDOW_NORMAL, Ordering::Relaxed) {
            WINDOW_BAR => attrs
                .with_skip_taskbar(true)
                .with_decorations(false)
                .with_resizable(false)
                .with_active(false)
                .with_window_level(WindowLevel::AlwaysOnTop),
            WINDOW_POPUP => attrs
                .with_skip_taskbar(true)
                .with_decorations(false)
                .with_resizable(false)
                .with_undecorated_shadow(true)
                .with_window_level(WindowLevel::AlwaysOnTop),
            WINDOW_TOOLTIP => attrs
                .with_skip_taskbar(true)
                .with_decorations(false)
                .with_resizable(false)
                .with_active(false)
                .with_undecorated_shadow(true)
                .with_window_level(WindowLevel::AlwaysOnTop),
            _ => attrs.with_undecorated_shadow(true),
        }
    };
    let base = || slint::BackendSelector::new().backend_name("winit".into()).with_winit_window_attributes_hook(hook);
    let chosen = match renderer.to_ascii_lowercase().as_str() {
        "femtovg" | "opengl" => base().renderer_name("femtovg".into()).select(),
        "software" => base().renderer_name("software".into()).select(),
        _ => base().require_d3d().select(),
    };
    if let Err(e) = chosen {
        eprintln!("renderer '{renderer}' unavailable ({e}); falling back");
        let _ = base().select();
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        // Release builds have no console of their own: print to the one we were started from.
        unsafe {
            use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
        use std::io::Write;
        let _ = writeln!(std::io::stdout(), "massier87shell {VERSION}"); // no panic if nobody reads it
        return Ok(());
    }
    // One bar at a time: a second copy (e.g. autostart plus a manual launch) would reserve
    // a second strip and stack below the first. The mutex lives until the process exits.
    let _single_instance = unsafe {
        use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
        let mutex = windows::Win32::System::Threading::CreateMutexW(None, true, windows::core::w!("massier87shell.single-instance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            eprintln!("massier87shell is already running");
            return Ok(());
        }
        mutex
    };
    config::migrate_old_folder();
    let cfg_path = std::env::args()
        .skip_while(|a| a != "--config")
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(config::default_path);
    let cfg = config::load(&cfg_path).unwrap_or_else(|e| {
        eprintln!("config error, using defaults: {e}");
        Config::default()
    });
    eprintln!("config: {}", cfg_path.display());

    // Autostart runs us right at sign-in, possibly before Explorer's taskbar exists.
    sys::wait_for_explorer_taskbar(Duration::from_secs(20));
    // Move an old Run-key autostart to the faster at-logon task (off the startup path).
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(20));
        autostart::refresh();
    });

    select_backend(&cfg.bar.renderer);

    let media_update = Arc::new(Mutex::new(None));
    let sink = media_update.clone();
    let media_tx = media::spawn(move |now| *sink.lock().unwrap() = Some(now));

    let state = Rc::new(RefCell::new(State {
        cfg,
        cfg_mtime: config::modified(&cfg_path),
        cfg_path,
        bars: Vec::new(),
        audio: audio::Audio::new(),
        cpu: sys::Cpu::default(),
        settings: None,
        save_timer: Timer::default(),
        taskbar: taskbar::Taskbar::default(),
        apps: ModelRc::default(),
        now_playing: media::NowPlaying::default(),
        media_update,
        media_tx,
        temp: temp::spawn(),
        power: Rc::new(PowerUi::default()),
        last_stats: Instant::now(),
        tray_ids: Vec::new(),
        tray_ui: Rc::new(TrayUi::default()),
        sink_desktop: None,
        app_menu: Rc::new(apps::AppMenu::default()),
        preview: Rc::new(apps::Preview::default()),
        taskview_timer: Timer::default(),
        control: control::Control::spawn(),
        control_ui: Rc::new(panel::ControlUi::default()),
    }));
    apply_config(&mut state.borrow_mut());
    appbar::release_stale();
    appbar::create_focus_sink();
    sys::install_minimize_focus_guard();
    sync_bars(&state);

    let fast = Timer::default();
    let st = state.clone();
    fast.start(TimerMode::Repeated, Duration::from_millis(250), move || fast_tick(&mut st.borrow_mut()));

    let slow = Timer::default();
    let st = state.clone();
    slow.start(TimerMode::Repeated, Duration::from_secs(1), move || {
        {
            let mut s = st.borrow_mut();
            clock_tick(&s);
            reload_if_changed(&mut s);
            if s.cfg.theme.accent_from_wallpaper {
                wallpaper::check(); // decodes on a thread only when the wallpaper changed
                if wallpaper::take_changed() {
                    apply_config(&mut s);
                    if let Some(w) = &s.settings {
                        settings::fill_colors(w, &s.cfg);
                    }
                }
            }
            tray::keep_first();
            let border = s.cfg.theme.palette().surface_hover;
            for b in &s.bars {
                if let Some(ab) = &b.appbar {
                    ab.ensure_reserved(&s.cfg.bar, border, b.monitor);
                }
            }
        }
        // Displays plugged in/out, or the "monitors" setting changed by hand.
        sync_bars(&st);
    });

    if std::env::args().any(|a| a == "--control") {
        let st = state.clone();
        Timer::single_shot(Duration::from_millis(800), move || {
            let bar = st.borrow().bars.first().map(|b| b.ui.as_weak());
            if let Some(bar) = bar {
                panel::toggle(&st, &bar);
            }
        });
    }
    if std::env::args().any(|a| a == "--settings") {
        let st = state.clone();
        Timer::single_shot(Duration::from_millis(200), move || open_settings(&st));
    }

    slint::run_event_loop_until_quit()?;

    // Never leave the Windows taskbar hidden behind.
    winbar::restore();
    // Releases the reserved screen space.
    for mut b in state.borrow_mut().bars.drain(..) {
        b.appbar.take();
    }
    Ok(())
}
