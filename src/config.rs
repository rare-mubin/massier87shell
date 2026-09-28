use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG: &str = include_str!("../config.default.toml");

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    pub bar: BarConfig,
    pub theme: ThemeConfig,
    pub modules: ModulesConfig,
    pub clock: ClockConfig,
    pub window: WidthConfig,
    pub media: WidthConfig,
    pub volume: VolumeConfig,
    pub taskbar: TaskbarConfig,
    pub temp: TempConfig,
    pub stats: StatsConfig,
    pub tray: TrayConfig,
    pub animations: AnimationsConfig,
    pub taskview: TaskViewConfig,
    pub control: ControlConfig,
}

// These fallbacks (for keys missing from the file) match config.default.toml.
impl Default for Config {
    fn default() -> Self {
        Self {
            bar: BarConfig::default(),
            theme: ThemeConfig::default(),
            modules: ModulesConfig::default(),
            clock: ClockConfig::default(),
            window: WidthConfig { max_width: 143.0 },
            media: WidthConfig { max_width: 128.0 },
            volume: VolumeConfig::default(),
            taskbar: TaskbarConfig::default(),
            temp: TempConfig::default(),
            stats: StatsConfig::default(),
            tray: TrayConfig::default(),
            animations: AnimationsConfig::default(),
            taskview: TaskViewConfig::default(),
            control: ControlConfig::default(),
        }
    }
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct BarConfig {
    pub position: String,
    pub height: f32,
    pub floating: bool,
    pub margin: f32,
    pub corners: String,
    pub border: bool,
    pub font: String,
    pub font_size: f32,
    pub spacing: f32,
    pub padding: f32,
    pub edge_padding: f32,
    pub pill_radius: f32,
    pub pill_background: bool,
    pub renderer: String,
    /// "primary" or "all" displays.
    pub monitors: String,
    /// Dragging empty bar space moves the maximized window below, like its title bar.
    pub drag_windows: bool,
    /// Hide the Windows taskbar completely (like "Taskbar Eliminator").
    pub hide_windows_taskbar: bool,
    /// Global shortcut that shows/hides the Windows taskbar, e.g. "Ctrl+Win+B". Empty = none.
    pub taskbar_hotkey: String,
}

impl Default for BarConfig {
    fn default() -> Self {
        Self {
            position: "top".into(),
            height: 26.0,
            floating: false,
            margin: 0.0,
            corners: "square".into(),
            border: false,
            font: "Segoe UI Variable Text".into(),
            font_size: 11.0,
            spacing: 4.0,
            padding: 0.0,
            edge_padding: 3.0,
            pill_radius: 5.0,
            pill_background: false,
            renderer: "software".into(),
            monitors: "all".into(),
            drag_windows: true,
            hide_windows_taskbar: true,
            taskbar_hotkey: "Ctrl+Win+B".into(),
        }
    }
}

impl BarConfig {
    pub fn is_top(&self) -> bool {
        !self.position.eq_ignore_ascii_case("bottom")
    }

    pub fn all_monitors(&self) -> bool {
        self.monitors.eq_ignore_ascii_case("all")
    }
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct ThemeConfig {
    pub preset: String,
    pub background: Option<String>,
    pub surface: Option<String>,
    pub surface_hover: Option<String>,
    pub foreground: Option<String>,
    pub foreground_dim: Option<String>,
    pub accent: Option<String>,
    pub on_accent: Option<String>,
    pub urgent: Option<String>,
    /// Bar background opacity, 0..1 (lower shows the desktop through it).
    pub opacity: f32,
    /// Opacity of every other window (settings, control panel, tray, menus...), 0..1.
    pub panel_opacity: f32,
    /// What shows through translucent backgrounds: "none", "blur" or "acrylic".
    pub blur: String,
    /// Picks the accent color from the desktop wallpaper (and follows it when it changes).
    pub accent_from_wallpaper: bool,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            preset: "tokyo-night".into(),
            background: None,
            surface: None,
            surface_hover: None,
            foreground: None,
            foreground_dim: None,
            accent: Some("#cba6f7".into()),
            on_accent: None,
            urgent: None,
            opacity: 0.5,
            panel_opacity: 0.5,
            blur: "blur".into(),
            accent_from_wallpaper: true,
        }
    }
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct ModulesConfig {
    pub left: Vec<String>,
    pub center: Vec<String>,
    pub right: Vec<String>,
}

impl Default for ModulesConfig {
    fn default() -> Self {
        let v = |s: &[&str]| s.iter().map(|s| s.to_string()).collect();
        Self {
            left: v(&["taskview", "workspaces", "media", "window"]),
            center: v(&["taskbar"]),
            right: v(&["tray", "cpu", "volume", "clock", "control"]),
        }
    }
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct ClockConfig {
    pub format: String,
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self { format: "%a %d %b   %l:%M %p".into() }
    }
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct WidthConfig {
    pub max_width: f32,
}

impl Default for WidthConfig {
    fn default() -> Self {
        Self { max_width: 300.0 }
    }
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct VolumeConfig {
    pub step: f32,
}

impl Default for VolumeConfig {
    fn default() -> Self {
        Self { step: 2.0 }
    }
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct TaskbarConfig {
    pub icon_size: f32,
    /// Our own pinned apps (.lnk, .exe or `shell:AppsFolder<AUMID>`), in order. Absent =
    /// use the Windows taskbar's pins; created the first time you pin, unpin or reorder.
    pub pinned: Option<Vec<String>>,
    /// Render app icons in the theme's text color instead of their own colors.
    pub monochrome: bool,
}

impl Default for TaskbarConfig {
    fn default() -> Self {
        Self { icon_size: 14.0, pinned: None, monochrome: false }
    }
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct ControlConfig {
    /// Tiles shown in the control panel, in order (ids from control::TILES).
    pub tiles: Vec<String>,
    /// Sections below the tiles: "volume", "brightness", "media".
    pub sections: Vec<String>,
}

impl Default for ControlConfig {
    fn default() -> Self {
        let v = |s: &[&str]| s.iter().map(|s| s.to_string()).collect();
        Self {
            tiles: v(&[
                "wifi", "ethernet", "bluetooth", "dark_mode", "taskbar", "mute", "mic", "screenshot", "settings", "lock", "taskmgr",
                "project",
            ]),
            sections: v(&["volume", "brightness", "media", "power"]),
        }
    }
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct TaskViewConfig {
    /// "click" or "hover".
    pub trigger: String,
    /// How long to hover before Task View opens (hover mode).
    pub hover_delay_ms: u32,
}

impl Default for TaskViewConfig {
    fn default() -> Self {
        Self { trigger: "click".into(), hover_delay_ms: 100 }
    }
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct AnimationsConfig {
    pub enabled: bool,
    /// 1 = normal, 2 = twice as fast, 0.5 = half speed.
    pub speed: f32,
    pub hover_effects: bool,
}

impl Default for AnimationsConfig {
    fn default() -> Self {
        Self { enabled: true, speed: 0.5, hover_effects: false }
    }
}

impl AnimationsConfig {
    /// Multiplier for every animation duration (0 = off).
    pub fn motion(&self) -> f32 {
        if self.enabled { 1.0 / self.speed.clamp(0.25, 4.0) } else { 0.0 }
    }
}

#[derive(Deserialize, Clone, Default)]
#[serde(default)]
pub struct TrayConfig {
    /// Exe names (lower case) of tray apps whose icons are shown on the bar.
    pub pinned: Vec<String>,
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct StatsConfig {
    /// Seconds between CPU / RAM / battery / temperature updates.
    pub interval: f32,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self { interval: 3.0 }
    }
}

impl StatsConfig {
    pub fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f32(self.interval.clamp(0.25, 60.0))
    }
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct TempConfig {
    /// "C" or "F".
    pub unit: String,
}

impl Default for TempConfig {
    fn default() -> Self {
        Self { unit: "C".into() }
    }
}

impl TempConfig {
    pub fn fahrenheit(&self) -> bool {
        self.unit.eq_ignore_ascii_case("F")
    }
}

/// Resolved colors as 0xRRGGBBAA.
pub struct Palette {
    pub background: u32,
    pub surface: u32,
    pub surface_hover: u32,
    pub foreground: u32,
    pub foreground_dim: u32,
    pub accent: u32,
    pub on_accent: u32,
    pub urgent: u32,
}

impl Palette {
    pub fn as_array(&self) -> [u32; 8] {
        [
            self.background,
            self.surface,
            self.surface_hover,
            self.foreground,
            self.foreground_dim,
            self.accent,
            self.on_accent,
            self.urgent,
        ]
    }
}

impl ThemeConfig {
    /// The per-color overrides, in the same order as `Palette::as_array`.
    pub fn overrides(&self) -> [(&'static str, &Option<String>); 8] {
        [
            ("background", &self.background),
            ("surface", &self.surface),
            ("surface_hover", &self.surface_hover),
            ("foreground", &self.foreground),
            ("foreground_dim", &self.foreground_dim),
            ("accent", &self.accent),
            ("on_accent", &self.on_accent),
            ("urgent", &self.urgent),
        ]
    }

    pub fn override_mut(&mut self, index: usize) -> Option<&mut Option<String>> {
        Some(match index {
            0 => &mut self.background,
            1 => &mut self.surface,
            2 => &mut self.surface_hover,
            3 => &mut self.foreground,
            4 => &mut self.foreground_dim,
            5 => &mut self.accent,
            6 => &mut self.on_accent,
            7 => &mut self.urgent,
            _ => return None,
        })
    }

    pub fn palette(&self) -> Palette {
        let p = |bg, s, sh, fg, fd, ac, oa, ur| Palette {
            background: bg,
            surface: s,
            surface_hover: sh,
            foreground: fg,
            foreground_dim: fd,
            accent: ac,
            on_accent: oa,
            urgent: ur,
        };
        #[rustfmt::skip]
        let mut pal = match self.preset.to_ascii_lowercase().as_str() {
            "tokyo-night" => p(0x1a1b26ff, 0x24283bff, 0x414868ff, 0xc0caf5ff, 0xa9b1d6ff, 0x7aa2f7ff, 0x1a1b26ff, 0xf7768eff),
            "gruvbox"     => p(0x282828ff, 0x3c3836ff, 0x504945ff, 0xebdbb2ff, 0xa89984ff, 0xfabd2fff, 0x282828ff, 0xfb4934ff),
            "nord"        => p(0x2e3440ff, 0x3b4252ff, 0x434c5eff, 0xeceff4ff, 0xd8dee9ff, 0x88c0d0ff, 0x2e3440ff, 0xbf616aff),
            "rose-pine"   => p(0x191724ff, 0x1f1d2eff, 0x26233aff, 0xe0def4ff, 0x908caaff, 0xebbcbaff, 0x191724ff, 0xeb6f92ff),
            "material" | "system"
                          => p(0x141218ff, 0x211f26ff, 0x2b2930ff, 0xe6e0e9ff, 0xcac4d0ff, 0xd0bcffff, 0x381e72ff, 0xf2b8b5ff),
            _             => p(0x1e1e2eff, 0x313244ff, 0x45475aff, 0xcdd6f4ff, 0xa6adc8ff, 0xcba6f7ff, 0x1e1e2eff, 0xf38ba8ff),
        };
        if self.preset.eq_ignore_ascii_case("system") {
            if let Some(accent) = crate::sys::system_accent() {
                pal.accent = accent;
            }
        }
        let over = |slot: &mut u32, v: &Option<String>| {
            if let Some(c) = v.as_deref().and_then(parse_color) {
                *slot = c;
            }
        };
        over(&mut pal.background, &self.background);
        over(&mut pal.surface, &self.surface);
        over(&mut pal.surface_hover, &self.surface_hover);
        over(&mut pal.foreground, &self.foreground);
        over(&mut pal.foreground_dim, &self.foreground_dim);
        over(&mut pal.accent, &self.accent);
        over(&mut pal.on_accent, &self.on_accent);
        over(&mut pal.urgent, &self.urgent);
        if self.accent_from_wallpaper {
            if let Some(raw) = crate::wallpaper::dominant() {
                let dark = luma(pal.background) < 0.5;
                pal.accent = crate::wallpaper::tone(raw, dark);
                pal.on_accent = match (luma(pal.accent) > 0.55, dark) {
                    (true, true) => pal.background,
                    (true, false) => 0x1a1a1aff,
                    (false, _) => 0xffffffff,
                };
            }
        }
        pal
    }
}

/// Relative brightness of a 0xRRGGBBAA color, 0..1.
pub fn luma(c: u32) -> f32 {
    let (r, g, b) = ((c >> 24) as f32, ((c >> 16) & 0xff) as f32, ((c >> 8) & 0xff) as f32);
    (0.299 * r + 0.587 * g + 0.114 * b) / 255.0
}

impl ThemeConfig {
    pub fn bar_opacity(&self) -> f32 {
        self.opacity.clamp(0.0, 1.0)
    }

    /// Popups stay readable: never below 40%.
    pub fn popup_opacity(&self) -> f32 {
        self.panel_opacity.clamp(0.4, 1.0)
    }
}

/// Parses "#rrggbb" or "#rrggbbaa" into 0xRRGGBBAA.
pub fn parse_color(s: &str) -> Option<u32> {
    let hex = s.trim().trim_start_matches('#');
    let v = u32::from_str_radix(hex, 16).ok()?;
    match hex.len() {
        6 => Some((v << 8) | 0xff),
        8 => Some(v),
        _ => None,
    }
}

pub fn default_path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| ".".into());
    base.join("massier87shell").join("config.toml")
}

/// The app was called massier87bar before 1.0: carries its settings folder over once.
pub fn migrate_old_folder() {
    let Some(base) = std::env::var_os("APPDATA").map(PathBuf::from) else { return };
    let (old, new) = (base.join("massier87bar"), base.join("massier87shell"));
    if new.exists() || !old.join("config.toml").exists() || std::fs::create_dir_all(&new).is_err() {
        return;
    }
    for name in ["config.toml", "appbars.txt"] {
        let _ = std::fs::copy(old.join(name), new.join(name));
    }
}

/// Reads the config, writing the default file first if none exists.
pub fn load(path: &Path) -> Result<Config, String> {
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(path, DEFAULT_CONFIG).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn modified(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Writes `cfg` into the config file, editing values in place so comments and layout survive.
pub fn save(path: &Path, cfg: &Config) -> Result<(), String> {
    use toml_edit::{Array, DocumentMut, Item, Value};

    let text = std::fs::read_to_string(path).unwrap_or_else(|_| DEFAULT_CONFIG.to_string());
    let mut doc: DocumentMut = text.parse().or_else(|_| DEFAULT_CONFIG.parse()).map_err(|e| format!("{e}"))?;

    // Indexing a missing table makes toml_edit create an inline `name = { ... }` at the top
    // of the file; make every section a real `[name]` table instead (and repair files that
    // an earlier version wrote that way).
    for name in ["bar", "theme", "modules", "clock", "window", "media", "volume", "taskbar", "temp", "stats", "tray", "animations", "taskview", "control"] {
        if doc.get(name).is_some_and(Item::is_table) {
            continue;
        }
        // Re-inserting (rather than converting in place) puts the section at the end of the
        // file with a clean `[name]` header.
        let mut table = match doc.remove(name) {
            Some(Item::Value(Value::InlineTable(inline))) => inline.into_table(),
            _ => toml_edit::Table::new(),
        };
        table.set_position(None);
        doc.insert(name, Item::Table(table));
    }

    fn set(doc: &mut DocumentMut, table: &str, key: &str, new: Value) {
        let item = &mut doc[table][key];
        match item.as_value_mut() {
            // Keep the existing value's decor (e.g. a trailing `# comment`).
            Some(old) => {
                let decor = old.decor().clone();
                *old = new;
                *old.decor_mut() = decor;
            }
            None => *item = Item::Value(new),
        }
    }
    let num = |v: f32| -> Value {
        // Round in f64: widening an f32 like 0.6 gives 0.6000000238418579.
        if v.fract() == 0.0 { Value::from(v as i64) } else { Value::from((v as f64 * 1e4).round() / 1e4) }
    };
    let list = |v: &[String]| -> Value { Value::Array(v.iter().map(String::as_str).collect::<Array>()) };

    let b = &cfg.bar;
    set(&mut doc, "bar", "position", b.position.as_str().into());
    set(&mut doc, "bar", "height", num(b.height));
    set(&mut doc, "bar", "floating", b.floating.into());
    set(&mut doc, "bar", "margin", num(b.margin));
    set(&mut doc, "bar", "corners", b.corners.as_str().into());
    set(&mut doc, "bar", "border", b.border.into());
    set(&mut doc, "bar", "font", b.font.as_str().into());
    set(&mut doc, "bar", "font_size", num(b.font_size));
    set(&mut doc, "bar", "spacing", num(b.spacing));
    set(&mut doc, "bar", "padding", num(b.padding));
    set(&mut doc, "bar", "edge_padding", num(b.edge_padding));
    set(&mut doc, "bar", "pill_radius", num(b.pill_radius));
    set(&mut doc, "bar", "pill_background", b.pill_background.into());
    set(&mut doc, "bar", "renderer", b.renderer.as_str().into());
    set(&mut doc, "bar", "monitors", b.monitors.as_str().into());
    set(&mut doc, "bar", "drag_windows", b.drag_windows.into());
    set(&mut doc, "bar", "hide_windows_taskbar", b.hide_windows_taskbar.into());
    set(&mut doc, "bar", "taskbar_hotkey", b.taskbar_hotkey.as_str().into());

    let t = &cfg.theme;
    set(&mut doc, "theme", "preset", t.preset.as_str().into());
    set(&mut doc, "theme", "opacity", num(t.opacity));
    set(&mut doc, "theme", "panel_opacity", num(t.panel_opacity));
    set(&mut doc, "theme", "blur", t.blur.as_str().into());
    set(&mut doc, "theme", "accent_from_wallpaper", t.accent_from_wallpaper.into());
    for (key, v) in t.overrides() {
        match v {
            Some(hex) => set(&mut doc, "theme", key, hex.as_str().into()),
            None => {
                if let Some(table) = doc["theme"].as_table_like_mut() {
                    table.remove(key);
                }
            }
        }
    }

    set(&mut doc, "modules", "left", list(&cfg.modules.left));
    set(&mut doc, "modules", "center", list(&cfg.modules.center));
    set(&mut doc, "modules", "right", list(&cfg.modules.right));
    set(&mut doc, "clock", "format", cfg.clock.format.as_str().into());
    set(&mut doc, "window", "max_width", num(cfg.window.max_width));
    set(&mut doc, "media", "max_width", num(cfg.media.max_width));
    set(&mut doc, "volume", "step", num(cfg.volume.step));
    set(&mut doc, "taskbar", "icon_size", num(cfg.taskbar.icon_size));
    set(&mut doc, "taskbar", "monochrome", cfg.taskbar.monochrome.into());
    if let Some(pins) = &cfg.taskbar.pinned {
        set(&mut doc, "taskbar", "pinned", list(pins));
    }
    set(&mut doc, "temp", "unit", cfg.temp.unit.as_str().into());
    set(&mut doc, "stats", "interval", Value::from(cfg.stats.interval as f64));
    set(&mut doc, "tray", "pinned", list(&cfg.tray.pinned));
    set(&mut doc, "animations", "enabled", cfg.animations.enabled.into());
    set(&mut doc, "animations", "speed", Value::from(cfg.animations.speed as f64));
    set(&mut doc, "animations", "hover_effects", cfg.animations.hover_effects.into());
    set(&mut doc, "taskview", "trigger", cfg.taskview.trigger.as_str().into());
    set(&mut doc, "control", "tiles", list(&cfg.control.tiles));
    set(&mut doc, "control", "sections", list(&cfg.control.sections));
    set(&mut doc, "taskview", "hover_delay_ms", Value::from(cfg.taskview.hover_delay_ms as i64));

    std::fs::write(path, doc.to_string()).map_err(|e| format!("cannot write {}: {e}", path.display()))
}
