//! The control panel ("quick settings") window: showing it, keeping it up to date while it's
//! open, and what its tiles do. The data comes from `control` (radios, networks, brightness,
//! dark mode) and the bar's audio/media state.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::winit_030::WinitWindowAccessor;
use slint::{ComponentHandle, Model, ModelRc, Timer, VecModel};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_LCONTROL, VK_LSHIFT, VK_LWIN, VK_P, VK_S};

use crate::control::{self, Command, Status};
use crate::{
    AppAudio, AudioDevice, Bar, ControlPanel, ControlTile, EthItem, State, Theme, WifiItem, hwnd_of, media, set_theme, sys,
    taskbar, winbar,
};

pub struct ControlUi {
    window: RefCell<Option<ControlPanel>>,
    hidden_at: Cell<Option<Instant>>,
    /// When the user last moved a slider: live updates wait a moment so it doesn't jump back.
    touched: Cell<Option<Instant>>,
    // Models are kept and updated row by row: replacing them every refresh rebuilt the tiles
    // under the pointer, which lost clicks.
    tiles: Rc<VecModel<ControlTile>>,
    networks: Rc<VecModel<WifiItem>>,
    adapters: Rc<VecModel<EthItem>>,
    outputs: Rc<VecModel<AudioDevice>>,
    inputs: Rc<VecModel<AudioDevice>>,
    apps: Rc<VecModel<AppAudio>>,
    /// App display names and icons, by executable path.
    app_names: RefCell<HashMap<String, String>>,
    app_icons: RefCell<HashMap<String, Option<slint::Image>>>,
    /// The control worker's "panel is open" flag.
    active: RefCell<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>>,
}

impl Default for ControlUi {
    fn default() -> Self {
        Self {
            window: RefCell::default(),
            hidden_at: Cell::default(),
            touched: Cell::default(),
            tiles: Rc::new(VecModel::default()),
            networks: Rc::new(VecModel::default()),
            adapters: Rc::new(VecModel::default()),
            outputs: Rc::new(VecModel::default()),
            inputs: Rc::new(VecModel::default()),
            apps: Rc::new(VecModel::default()),
            app_names: RefCell::default(),
            app_icons: RefCell::default(),
            active: RefCell::default(),
        }
    }
}

/// Makes `model` equal to `rows`, touching only the rows that changed.
fn sync<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.row_count() != rows.len() {
        model.set_vec(rows);
        return;
    }
    for (i, row) in rows.into_iter().enumerate() {
        if model.row_data(i).as_ref() != Some(&row) {
            model.set_row_data(i, row);
        }
    }
}

fn hide(c: &ControlUi) {
    if let Some(w) = c.window.borrow().as_ref() {
        if w.window().is_visible() {
            if let Some(h) = hwnd_of(w.window()) {
                sys::stop_outside_watch(h);
            }
            w.set_open(false);
            let _ = w.hide();
            c.hidden_at.set(Some(Instant::now()));
            if let Some(active) = c.active.borrow().as_ref() {
                active.store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

/// Pushes the current state into the panel (called on the bar's fast timer while it's open).
pub fn refresh(st: &State) {
    let c = st.control_ui.clone();
    let window = c.window.borrow();
    if let Some(w) = window.as_ref().filter(|w| w.window().is_visible()) {
        fill(st, &c, w);
    }
}

fn signal_icon(signal: u8) -> &'static str {
    match signal {
        0..=25 => "\u{E872}",
        26..=50 => "\u{E873}",
        51..=75 => "\u{E874}",
        _ => "\u{E701}",
    }
}

fn fill_tiles(st: &State, c: &ControlUi, status: &Status, volume: Option<(f32, bool)>) {
    let mic_muted = st.audio.as_ref().and_then(|a| a.mic_muted());
    let tiles: Vec<ControlTile> = st
        .cfg
        .control
        .tiles
        .iter()
        .filter_map(|id| control::TILES.iter().find(|t| t.0 == id.as_str()))
        .filter_map(|&(id, label, icon, toggle)| {
            let active = match id {
                "wifi" => status.wifi?,
                "bluetooth" => status.bluetooth?,
                "airplane" => status.airplane(),
                "dark_mode" => control::dark_mode(),
                "taskbar" => winbar::is_hidden(),
                "ethernet" => status.ethernet_connected(),
                "mute" => volume?.1,
                "mic" => mic_muted?,
                _ => false,
            };
            let label = match id {
                "wifi" => status.wifi_network().map_or(label, |n| n.ssid.as_str()),
                _ => label,
            };
            let detail = matches!(id, "wifi" | "bluetooth" | "ethernet");
            Some(ControlTile { id: id.into(), icon: icon.into(), label: label.into(), toggle, active, detail })
        })
        .collect();
    sync(&c.tiles, tiles);
}

fn fill_wifi(c: &ControlUi, w: &ControlPanel, status: &Status) {
    w.set_wifi_available(status.wifi.is_some());
    w.set_wifi_on(status.wifi == Some(true));
    w.set_scanning(status.scanning);
    w.set_wifi_blocked(status.wifi_blocked);
    w.set_wifi_message(status.wifi_error.clone().unwrap_or_default().into());
    let item = |n: &crate::network::Network| WifiItem {
        ssid: n.ssid.as_str().into(),
        icon: signal_icon(n.signal).into(),
        secured: n.secured,
        connected: n.connected,
        saved: n.saved,
        needs_password: n.needs_password(),
        enterprise: n.enterprise,
        connecting: status.connecting.as_deref() == Some(n.ssid.as_str()),
    };
    // Keep rows where they are (signal strengths wobble, and a row may be open for typing a
    // password): update the ones still there, drop vanished ones, add new ones at the end.
    let expanded = w.get_expanded();
    let mut rows: Vec<WifiItem> = c
        .networks
        .iter()
        .filter_map(|old| match status.networks.iter().find(|n| n.ssid == old.ssid.as_str()) {
            Some(n) => Some(item(n)),
            None => (old.ssid == expanded && status.wifi == Some(true)).then_some(old),
        })
        .collect();
    for n in &status.networks {
        if !rows.iter().any(|r| r.ssid.as_str() == n.ssid) {
            rows.push(item(n));
        }
    }
    sync(&c.networks, rows);
}

/// Sorted fresh (connected first, strongest next) when the page opens.
fn reset_wifi_list(c: &ControlUi, w: &ControlPanel, status: &Status) {
    c.networks.set_vec(Vec::new());
    w.set_expanded("".into());
    fill_wifi(c, w, status);
}

/// Devices, per-app volumes and the microphone, for the Sound page.
fn fill_audio(st: &State, c: &ControlUi, w: &ControlPanel) {
    let Some(audio) = &st.audio else { return };
    let device = |d: crate::audio::Device| AudioDevice { id: d.id.into(), name: d.name.into(), selected: d.default };
    sync(&c.outputs, audio.devices(false).into_iter().map(device).collect());
    sync(&c.inputs, audio.devices(true).into_iter().map(device).collect());

    let px = (20.0 * w.window().scale_factor()).round() as i32;
    let mut icons = c.app_icons.borrow_mut();
    let apps: Vec<AppAudio> = audio
        .apps(&mut c.app_names.borrow_mut())
        .into_iter()
        .map(|a| {
            let icon = icons
                .entry(a.key.clone())
                .or_insert_with(|| (!a.key.is_empty()).then(|| taskbar::load_icon(&a.key, px)).flatten().map(|i| i.render(None)))
                .clone();
            AppAudio {
                key: a.key.into(),
                name: a.name.into(),
                has_icon: icon.is_some(),
                icon: icon.unwrap_or_default(),
                volume: a.volume.round(),
                muted: a.muted,
            }
        })
        .collect();
    sync(&c.apps, apps);

    let mic = audio.mic_volume();
    w.set_has_mic(mic.is_some());
    w.set_mic_muted(audio.mic_muted().unwrap_or(false));
    if let (Some(v), true) = (mic, c.touched.get().is_none_or(|t| t.elapsed() > Duration::from_millis(1500))) {
        w.set_mic_volume(v.round());
    }
}

fn fill(st: &State, c: &ControlUi, w: &ControlPanel) {
    let status = st.control.status();
    let volume = st.audio.as_ref().and_then(|a| a.get());

    fill_tiles(st, c, &status, volume);
    match w.get_page().as_str() {
        "wifi" => fill_wifi(c, w, &status),
        "audio" => fill_audio(st, c, w),
        _ => {}
    }
    let adapters: Vec<EthItem> = status
        .ethernet
        .iter()
        .map(|a| EthItem {
            name: a.name.as_str().into(),
            description: a.description.as_str().into(),
            connected: a.connected,
            ipv4: a.ipv4.as_str().into(),
            speed: a.speed.as_str().into(),
        })
        .collect();
    sync(&c.adapters, adapters);

    let sections = &st.cfg.control.sections;
    let has = |s: &str| sections.iter().any(|x| x == s);
    w.set_show_volume(has("volume") && volume.is_some());
    w.set_show_brightness(has("brightness") && status.brightness.is_some());
    w.set_show_media(has("media"));
    w.set_show_power(has("power"));
    let settled = c.touched.get().is_none_or(|t| t.elapsed() > Duration::from_millis(1500));
    if let Some((vol, muted)) = volume {
        w.set_muted(muted);
        if settled {
            w.set_volume(vol.round());
        }
    }
    if let (Some(b), true) = (status.brightness, settled) {
        w.set_brightness(b);
    }
    let now = &st.now_playing;
    w.set_media_active(now.active);
    w.set_media_playing(now.playing);
    w.set_media_title(now.title.as_str().into());
    w.set_media_artist(now.artist.as_str().into());
}

fn open_uri(uri: &str) {
    let _ = std::process::Command::new("explorer.exe").arg(uri).spawn();
}

/// Refreshes now and again shortly (radios and networks report back from their thread).
fn refresh_soon(state: &Rc<RefCell<State>>) {
    refresh(&state.borrow());
    let st = state.clone();
    Timer::single_shot(Duration::from_millis(700), move || refresh(&st.borrow()));
}

fn tile_clicked(state: &Rc<RefCell<State>>, id: &str) {
    let st = state.borrow();
    let status = st.control.status();
    // Actions close the panel first (they open something else).
    let action = |f: &dyn Fn()| {
        hide(&st.control_ui);
        f();
    };
    match id {
        "wifi" => st.control.send(Command::Wifi(status.wifi != Some(true))),
        "bluetooth" => st.control.send(Command::Bluetooth(status.bluetooth != Some(true))),
        "airplane" => st.control.send(Command::Airplane(!status.airplane())),
        "ethernet" => {
            drop(st);
            tile_details(state, id);
            return;
        }
        // Broadcasting the theme change can take a moment; keep the UI responsive.
        "dark_mode" => {
            let dark = !control::dark_mode();
            std::thread::spawn(move || control::set_dark_mode(dark));
        }
        "taskbar" => winbar::toggle(),
        "mute" => {
            if let Some(a) = &st.audio {
                a.toggle_mute();
            }
        }
        "mic" => {
            if let Some(a) = &st.audio {
                a.toggle_mic();
            }
        }
        "screenshot" => action(&|| sys::send_shortcut(&[VK_LWIN, VK_LSHIFT, VK_S])),
        "settings" => action(&|| open_uri("ms-settings:")),
        "lock" => action(&|| crate::power::run(crate::PowerAction::Lock)),
        "taskmgr" => action(&|| sys::send_shortcut(&[VK_LCONTROL, VK_LSHIFT, VK_ESCAPE])),
        "project" => action(&|| sys::send_shortcut(&[VK_LWIN, VK_P])),
        _ => {}
    }
    drop(st);
    refresh_soon(state);
}

/// The ">" on a tile: Wi-Fi and Ethernet open a page, Bluetooth opens Windows' settings.
fn tile_details(state: &Rc<RefCell<State>>, id: &str) {
    let st = state.borrow();
    let c = st.control_ui.clone();
    let window = c.window.borrow();
    let Some(w) = window.as_ref() else { return };
    match id {
        "wifi" => {
            w.set_page("wifi".into());
            reset_wifi_list(&c, w, &st.control.status());
            st.control.send(Command::Scan);
        }
        "ethernet" => w.set_page("ethernet".into()),
        "audio" => {
            w.set_page("audio".into());
            fill_audio(&st, &c, w);
        }
        "bluetooth" => {
            drop(window);
            hide(&st.control_ui);
            open_uri("ms-settings:bluetooth");
        }
        _ => {}
    }
}

fn create(state: &Rc<RefCell<State>>, c: &Rc<ControlUi>) -> Option<ControlPanel> {
    crate::NEXT_WINDOW.store(crate::WINDOW_POPUP, std::sync::atomic::Ordering::Relaxed);
    let w = ControlPanel::new().ok()?;
    w.set_tiles(ModelRc::from(c.tiles.clone()));
    w.set_networks(ModelRc::from(c.networks.clone()));
    w.set_adapters(ModelRc::from(c.adapters.clone()));
    w.set_outputs(ModelRc::from(c.outputs.clone()));
    w.set_inputs(ModelRc::from(c.inputs.clone()));
    w.set_apps(ModelRc::from(c.apps.clone()));
    w.set_can_hibernate(crate::power::can_hibernate());
    *c.active.borrow_mut() = Some(state.borrow().control.active_flag());

    let st = state.clone();
    w.on_tile_clicked(move |id| tile_clicked(&st, &id));
    let st = state.clone();
    w.on_tile_details(move |id| tile_details(&st, &id));
    let (st, cw) = (state.clone(), Rc::downgrade(c));
    w.on_volume_changed(move |v| {
        if let Some(c) = cw.upgrade() {
            c.touched.set(Some(Instant::now()));
        }
        if let Some(a) = &st.borrow().audio {
            a.set_volume(v);
        }
    });
    let (st, cw) = (state.clone(), Rc::downgrade(c));
    w.on_brightness_changed(move |v| {
        if let Some(c) = cw.upgrade() {
            c.touched.set(Some(Instant::now()));
        }
        st.borrow().control.send(Command::Brightness(v));
    });
    let st = state.clone();
    w.on_mute_clicked(move || {
        if let Some(a) = &st.borrow().audio {
            a.toggle_mute();
        }
        refresh(&st.borrow());
    });
    let st = state.clone();
    w.on_media_prev(move || {
        let _ = st.borrow().media_tx.send(media::Command::Prev);
    });
    let st = state.clone();
    w.on_media_toggle(move || {
        let _ = st.borrow().media_tx.send(media::Command::Toggle);
    });
    let st = state.clone();
    w.on_media_next(move || {
        let _ = st.borrow().media_tx.send(media::Command::Next);
    });
    let st = state.clone();
    w.on_power(move |action| {
        hide(&st.borrow().control_ui);
        crate::power::run(action);
    });

    // Wi-Fi page
    let st = state.clone();
    w.on_wifi_toggled(move |on| {
        st.borrow().control.send(Command::Wifi(on));
        if on {
            st.borrow().control.send(Command::Scan);
        }
        refresh_soon(&st);
    });
    let st = state.clone();
    w.on_wifi_scan(move || {
        st.borrow().control.send(Command::Scan);
        refresh(&st.borrow());
    });
    let st = state.clone();
    w.on_wifi_connect(move |ssid, password| {
        let password = (!password.is_empty()).then(|| password.to_string());
        st.borrow().control.send(Command::Connect { ssid: ssid.into(), password });
        refresh(&st.borrow());
    });
    let st = state.clone();
    w.on_wifi_disconnect(move || {
        st.borrow().control.send(Command::Disconnect);
        refresh_soon(&st);
    });
    for (setter, uri) in [
        (ControlPanel::on_wifi_settings as fn(&ControlPanel, _), "ms-settings:network-wifi"),
        (ControlPanel::on_wifi_windows, "ms-availablenetworks:"),
        (ControlPanel::on_ethernet_settings, "ms-settings:network-ethernet"),
        (ControlPanel::on_location_settings, "ms-settings:privacy-location"),
    ] {
        let st = state.clone();
        setter(&w, move || {
            hide(&st.borrow().control_ui);
            open_uri(uri);
        });
    }
    let st = state.clone();
    w.on_adapter_options(move || {
        hide(&st.borrow().control_ui);
        let _ = std::process::Command::new("control.exe").arg("ncpa.cpl").spawn();
    });

    // Sound page
    let with_audio = |state: &Rc<RefCell<State>>, f: Box<dyn Fn(&crate::audio::Audio)>| {
        if let Some(a) = &state.borrow().audio {
            f(a);
        }
        refresh(&state.borrow());
    };
    let st = state.clone();
    w.on_output_selected(move |id| with_audio(&st, Box::new(move |a| a.set_default_device(&id))));
    let st = state.clone();
    w.on_input_selected(move |id| with_audio(&st, Box::new(move |a| a.set_default_device(&id))));
    let st = state.clone();
    w.on_app_volume_changed(move |key, v| with_audio(&st, Box::new(move |a| a.set_app_volume(&key, v))));
    let st = state.clone();
    w.on_app_mute_clicked(move |key, mute| with_audio(&st, Box::new(move |a| a.set_app_mute(&key, mute))));
    let (st, cw) = (state.clone(), Rc::downgrade(c));
    w.on_mic_volume_changed(move |v| {
        if let Some(c) = cw.upgrade() {
            c.touched.set(Some(Instant::now()));
        }
        if let Some(a) = &st.borrow().audio {
            a.set_mic_volume(v);
        }
    });
    let st = state.clone();
    w.on_mic_mute_clicked(move || with_audio(&st, Box::new(|a| a.toggle_mic())));
    for (setter, uri) in [
        (ControlPanel::on_sound_settings as fn(&ControlPanel, _), "ms-settings:sound"),
        (ControlPanel::on_volume_mixer, "ms-settings:apps-volume"),
    ] {
        let st = state.clone();
        setter(&w, move || {
            hide(&st.borrow().control_ui);
            open_uri(uri);
        });
    }

    let st = state.clone();
    w.on_open_settings(move || {
        hide(&st.borrow().control_ui);
        crate::open_settings(&st);
    });
    let cw = Rc::downgrade(c);
    w.on_dismiss(move || {
        if let Some(c) = cw.upgrade() {
            hide(&c);
        }
    });
    let cw = Rc::downgrade(c);
    w.window().on_winit_window_event(move |_, event| {
        if let slint::winit_030::winit::event::WindowEvent::Focused(false) = event {
            if let Some(c) = cw.upgrade() {
                hide(&c);
            }
        }
        slint::winit_030::EventResult::Propagate
    });
    Some(w)
}

/// Opens (or closes) the panel under the control module's button.
pub fn toggle(state: &Rc<RefCell<State>>, bar: &slint::Weak<Bar>) {
    open(state, bar, "");
}

/// Opens (or closes) just the Sound page, under the volume module.
pub fn toggle_audio(state: &Rc<RefCell<State>>, bar: &slint::Weak<Bar>) {
    open(state, bar, "audio");
}

fn open(state: &Rc<RefCell<State>>, bar: &slint::Weak<Bar>, page: &str) {
    let c = state.borrow().control_ui.clone();
    if c.window.borrow().as_ref().is_some_and(|w| w.window().is_visible()) {
        hide(&c);
        return;
    }
    if c.hidden_at.get().is_some_and(|t| t.elapsed() < Duration::from_millis(300)) {
        return; // this click is what just closed it
    }
    let Some(bar_hwnd) = bar.upgrade().and_then(|b| hwnd_of(b.window())) else { return };
    if c.window.borrow().is_none() {
        let Some(w) = create(state, &c) else { return };
        *c.window.borrow_mut() = Some(w);
    }
    let (top, border) = {
        let st = state.borrow();
        st.control.set_active(true);
        let pal = st.cfg.theme.palette();
        if let Some(w) = c.window.borrow().as_ref() {
            set_theme(w.global::<Theme>(), &st.cfg, &pal);
        }
        (st.cfg.bar.is_top(), pal.surface_hover)
    };
    let window = c.window.borrow();
    let Some(w) = window.as_ref() else { return };
    w.set_open(false);
    w.set_page(page.into());
    w.set_standalone(!page.is_empty());
    w.set_armed(-1);
    fill(&state.borrow(), &c, w); // before showing, so it opens at its final size
    let cw = Rc::downgrade(&c);
    crate::show_popup(
        w,
        bar_hwnd,
        top,
        border,
        Rc::new(move || {
            if let Some(c) = cw.upgrade() {
                hide(&c);
            }
        }),
    );
    let ww = w.as_weak();
    Timer::single_shot(Duration::from_millis(16), move || {
        if let Some(w) = ww.upgrade() {
            w.set_open(true);
        }
    });
}
