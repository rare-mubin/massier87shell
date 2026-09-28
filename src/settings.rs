//! Moves values between `Config` and the settings window.

use crate::config::{Config, parse_color};
use crate::{ColorRow, ControlOption, ModRow, SettingsWindow, Swatch, color};
use slint::{ModelRc, SharedString, VecModel};

pub const PRESETS: &[&str] = &["catppuccin-mocha", "tokyo-night", "gruvbox", "nord", "rose-pine", "material", "system"];
pub const MODULES: &[&str] = &[
    "launcher", "workspaces", "taskview", "taskbar", "window", "clock", "media", "cpu", "ram", "temp", "tray", "control",
    "volume", "battery", "settings", "power",
];
const RENDERERS: &[&str] = &["software", "skia", "femtovg"];
const CORNERS: &[&str] = &["round", "small", "square"];
const MONITORS: &[&str] = &["primary", "all"];
const TEMP_UNITS: &[&str] = &["C", "F"];
const BLURS: &[&str] = &["none", "blur", "acrylic"];
/// Choices for the stats refresh interval, in seconds (matches the dropdown in the UI).
const STATS_INTERVALS: [f32; 6] = [0.5, 1.0, 2.0, 3.0, 5.0, 10.0];
const COLOR_LABELS: [&str; 8] =
    ["Background", "Pill surface", "Pill hover", "Text", "Dim text", "Accent", "Text on accent", "Urgent / warning"];
/// Quick accent picks (Catppuccin + Material tones).
const SWATCHES: &[&str] = &[
    "#cba6f7", "#89b4fa", "#74c7ec", "#94e2d5", "#a6e3a1", "#f9e2af", "#fab387", "#f38ba8", "#f5c2e7", "#d0bcff",
];

pub const ACCENT_INDEX: usize = 5;

pub fn hex(rgba: u32) -> String {
    format!("#{:06x}", rgba >> 8)
}

fn index_of(list: &[&str], value: &str) -> i32 {
    list.iter().position(|v| v.eq_ignore_ascii_case(value)).unwrap_or(0) as i32
}

pub fn clock_preview(fmt: &str) -> String {
    crate::format_clock(fmt)
}

/// Rebuilds the color rows. Rebuilding (rather than updating rows) also resets the
/// hex text fields, whose bindings break once the user types into them.
pub fn fill_colors(w: &SettingsWindow, cfg: &Config) {
    let pal = cfg.theme.palette().as_array();
    let rows: Vec<ColorRow> = cfg
        .theme
        .overrides()
        .iter()
        .enumerate()
        .map(|(i, (_, over))| ColorRow {
            label: COLOR_LABELS[i].into(),
            hex: (*over).clone().unwrap_or_else(|| hex(pal[i])).into(),
            preview: color(pal[i]),
            overridden: over.is_some(),
        })
        .collect();
    w.set_colors(ModelRc::new(VecModel::from(rows)));
    w.set_accent(color(pal[ACCENT_INDEX]));
}

/// Display name and icon for a module in the editor.
fn module_info(name: &str) -> (&'static str, &'static str) {
    match name {
        "launcher" => ("Start button", "\u{E700}"),
        "workspaces" => ("Workspaces", "\u{E7C4}"),
        "taskbar" => ("Taskbar apps", "\u{F0E2}"),
        "window" => ("Window title", "\u{E8A7}"),
        "clock" => ("Clock", "\u{E823}"),
        "media" => ("Media", "\u{E8D6}"),
        "cpu" => ("CPU usage", "\u{E950}"),
        "ram" => ("Memory usage", "\u{E964}"),
        "temp" => ("Temperature", "\u{E9CA}"),
        "tray" => ("Background apps (tray)", "\u{E70E}"),
        "control" => ("Control panel", "\u{E9E9}"),
        "taskview" => ("Task view", "\u{E7C4}"),
        "volume" => ("Volume", "\u{E767}"),
        "battery" => ("Battery", "\u{E83F}"),
        "settings" => ("Settings", "\u{E713}"),
        "power" => ("Power menu", "\u{E7E8}"),
        _ => ("Unknown module", "\u{E897}"),
    }
}

/// Modules with an options page in the editor (the gear button).
fn configurable(name: &str) -> bool {
    matches!(name, "clock" | "taskbar" | "temp" | "cpu" | "ram" | "battery" | "taskview" | "window" | "media" | "volume" | "control")
}

const SECTIONS: [&str; 3] = ["LEFT", "CENTER", "RIGHT"];

/// The module layout flattened into editor rows: `None` is a section header.
fn flatten(cfg: &Config) -> Vec<Option<String>> {
    let m = &cfg.modules;
    [&m.left, &m.center, &m.right]
        .iter()
        .flat_map(|list| std::iter::once(None).chain(list.iter().cloned().map(Some)))
        .collect()
}

/// Rebuilds the three sections from editor rows (every header starts the next section).
fn unflatten(cfg: &mut Config, rows: Vec<Option<String>>) {
    let mut sections: [Vec<String>; 3] = Default::default();
    let mut current = 0;
    let mut headers = 0;
    for row in rows {
        match row {
            None => {
                current = headers.min(2);
                headers += 1;
            }
            Some(name) => sections[current].push(name),
        }
    }
    let [left, center, right] = sections;
    cfg.modules.left = left;
    cfg.modules.center = center;
    cfg.modules.right = right;
}

/// Moves editor row `from` to position `to` among the remaining rows.
pub fn drop_row(cfg: &mut Config, from: usize, to: usize) {
    let mut rows = flatten(cfg);
    if from >= rows.len() || rows[from].is_none() {
        return;
    }
    let item = rows.remove(from);
    // Index 0 is the LEFT header; nothing goes above it.
    rows.insert(to.clamp(1, rows.len()), item);
    unflatten(cfg, rows);
}

pub fn remove_row(cfg: &mut Config, index: usize) {
    let mut rows = flatten(cfg);
    if rows.get(index).is_some_and(Option::is_some) {
        rows.remove(index);
        unflatten(cfg, rows);
    }
}

pub fn fill_modules(w: &SettingsWindow, cfg: &Config) {
    let mut section = 0;
    let rows: Vec<ModRow> = flatten(cfg)
        .into_iter()
        .map(|row| match row {
            None => {
                let label = SECTIONS[section.min(2)];
                section += 1;
                ModRow { header: true, name: "".into(), label: label.into(), icon: "".into(), configurable: false }
            }
            Some(name) => {
                let (label, icon) = module_info(name.trim());
                ModRow {
                    header: false,
                    name: name.as_str().into(),
                    label: label.into(),
                    icon: icon.into(),
                    configurable: configurable(name.trim()),
                }
            }
        })
        .collect();
    w.set_module_rows(ModelRc::new(VecModel::from(rows)));
}

pub fn fill(w: &SettingsWindow, cfg: &Config, config_path: &str) {
    w.set_config_path(config_path.into());
    w.set_version(crate::VERSION.into());
    w.set_presets(ModelRc::new(VecModel::from(PRESETS.iter().map(|&p| p.into()).collect::<Vec<SharedString>>())));
    w.set_available_modules(ModelRc::new(VecModel::from(
        MODULES.iter().map(|&p| p.into()).collect::<Vec<SharedString>>(),
    )));
    w.set_accent_swatches(ModelRc::new(VecModel::from(
        SWATCHES
            .iter()
            .map(|&h| Swatch { hex: h.into(), preview: color(parse_color(h).unwrap_or(0xffffffff)) })
            .collect::<Vec<_>>(),
    )));

    let b = &cfg.bar;
    w.set_preset_index(index_of(PRESETS, &cfg.theme.preset));
    w.set_position_index(if b.is_top() { 0 } else { 1 });
    w.set_floating(b.floating);
    w.set_bar_height(b.height);
    w.set_margin(b.margin);
    w.set_corners_index(index_of(CORNERS, &b.corners));
    w.set_border(b.border);
    w.set_pill_radius(b.pill_radius);
    w.set_pill_background(b.pill_background);
    w.set_pill_spacing(b.spacing);
    w.set_v_padding(b.padding);
    w.set_edge_padding(b.edge_padding);
    w.set_font(b.font.as_str().into());
    w.set_font_size(b.font_size);
    w.set_renderer_index(index_of(RENDERERS, &b.renderer));
    w.set_monitors_index(index_of(MONITORS, &b.monitors));
    w.set_autostart(crate::autostart::is_enabled());
    w.set_drag_windows(b.drag_windows);
    w.set_hide_windows_taskbar(b.hide_windows_taskbar);
    w.set_taskbar_hotkey(b.taskbar_hotkey.as_str().into());

    w.set_clock_format(cfg.clock.format.as_str().into());
    w.set_clock_preview(clock_preview(&cfg.clock.format).into());
    w.set_window_max_width(cfg.window.max_width);
    w.set_media_max_width(cfg.media.max_width);
    w.set_volume_step(cfg.volume.step);
    w.set_icon_size(cfg.taskbar.icon_size);
    w.set_monochrome(cfg.taskbar.monochrome);
    w.set_temp_unit_index(index_of(TEMP_UNITS, &cfg.temp.unit));
    let nearest = STATS_INTERVALS
        .iter()
        .enumerate()
        .min_by(|a, b| (a.1 - cfg.stats.interval).abs().total_cmp(&(b.1 - cfg.stats.interval).abs()))
        .map_or(1, |(i, _)| i);
    w.set_stats_interval_index(nearest as i32);
    w.set_taskview_trigger_index(if cfg.taskview.trigger.eq_ignore_ascii_case("hover") { 1 } else { 0 });
    w.set_taskview_delay(cfg.taskview.hover_delay_ms as f32);
    fill_control(w, cfg);
    w.set_anim_enabled(cfg.animations.enabled);
    w.set_anim_speed((cfg.animations.speed * 100.0).round());
    w.set_anim_hover(cfg.animations.hover_effects);
    w.set_bar_opacity((cfg.theme.bar_opacity() * 100.0).round());
    w.set_panel_opacity((cfg.theme.popup_opacity() * 100.0).round());
    w.set_blur_index(index_of(BLURS, &cfg.theme.blur));
    w.set_accent_wallpaper(cfg.theme.accent_from_wallpaper);

    fill_colors(w, cfg);
    fill_modules(w, cfg);
}

/// Copies every simple control value from the window into `cfg`.
pub fn read(w: &SettingsWindow, cfg: &mut Config) {
    let pick = |list: &[&str], i: i32| list.get(i as usize).copied().unwrap_or(list[0]).to_string();
    let b = &mut cfg.bar;
    b.position = if w.get_position_index() == 1 { "bottom" } else { "top" }.into();
    b.floating = w.get_floating();
    b.height = w.get_bar_height();
    b.margin = w.get_margin();
    b.corners = pick(CORNERS, w.get_corners_index());
    b.border = w.get_border();
    b.pill_radius = w.get_pill_radius();
    b.pill_background = w.get_pill_background();
    b.spacing = w.get_pill_spacing();
    b.padding = w.get_v_padding();
    b.edge_padding = w.get_edge_padding();
    b.font = w.get_font().to_string();
    b.font_size = w.get_font_size();
    b.renderer = pick(RENDERERS, w.get_renderer_index());
    b.monitors = pick(MONITORS, w.get_monitors_index());
    b.drag_windows = w.get_drag_windows();
    b.hide_windows_taskbar = w.get_hide_windows_taskbar();
    b.taskbar_hotkey = w.get_taskbar_hotkey().trim().to_string();

    cfg.theme.preset = pick(PRESETS, w.get_preset_index());
    cfg.clock.format = w.get_clock_format().to_string();
    cfg.window.max_width = w.get_window_max_width();
    cfg.media.max_width = w.get_media_max_width();
    cfg.volume.step = w.get_volume_step();
    cfg.taskbar.icon_size = w.get_icon_size();
    cfg.taskbar.monochrome = w.get_monochrome();
    cfg.temp.unit = pick(TEMP_UNITS, w.get_temp_unit_index());
    cfg.stats.interval = STATS_INTERVALS.get(w.get_stats_interval_index() as usize).copied().unwrap_or(1.0);
    cfg.taskview.trigger = if w.get_taskview_trigger_index() == 1 { "hover" } else { "click" }.into();
    cfg.taskview.hover_delay_ms = w.get_taskview_delay().round() as u32;
    cfg.animations.enabled = w.get_anim_enabled();
    cfg.animations.speed = w.get_anim_speed() / 100.0;
    cfg.animations.hover_effects = w.get_anim_hover();
    cfg.theme.opacity = w.get_bar_opacity() / 100.0;
    cfg.theme.panel_opacity = w.get_panel_opacity() / 100.0;
    cfg.theme.blur = pick(BLURS, w.get_blur_index());
    cfg.theme.accent_from_wallpaper = w.get_accent_wallpaper();
}

pub fn section(cfg: &mut Config, index: i32) -> Option<&mut Vec<String>> {
    match index {
        0 => Some(&mut cfg.modules.left),
        1 => Some(&mut cfg.modules.center),
        2 => Some(&mut cfg.modules.right),
        _ => None,
    }
}

/// The control panel's tiles and sections, each with an on/off switch.
pub fn fill_control(w: &SettingsWindow, cfg: &Config) {
    let option = |id: &str, label: &str, icon: &str, list: &[String]| ControlOption {
        id: id.into(),
        label: label.into(),
        icon: icon.into(),
        enabled: list.iter().any(|x| x == id),
    };
    let tiles: Vec<ControlOption> =
        crate::control::TILES.iter().map(|&(id, label, icon, _)| option(id, label, icon, &cfg.control.tiles)).collect();
    let sections: Vec<ControlOption> =
        crate::control::SECTIONS.iter().map(|&(id, label)| option(id, label, "", &cfg.control.sections)).collect();
    w.set_control_options(ModelRc::new(VecModel::from(tiles)));
    w.set_control_sections(ModelRc::new(VecModel::from(sections)));
}

/// Turns a control panel tile or section on or off (kept in catalog order).
pub fn toggle_control_option(cfg: &mut Config, id: &str, on: bool) {
    let is_tile = crate::control::TILES.iter().any(|t| t.0 == id);
    let catalog: Vec<&str> = if is_tile {
        crate::control::TILES.iter().map(|t| t.0).collect()
    } else {
        crate::control::SECTIONS.iter().map(|s| s.0).collect()
    };
    let list = if is_tile { &mut cfg.control.tiles } else { &mut cfg.control.sections };
    let enabled = |x: &str| if x == id { on } else { list.iter().any(|l| l == x) };
    *list = catalog.into_iter().filter(|x| enabled(x)).map(String::from).collect();
}
