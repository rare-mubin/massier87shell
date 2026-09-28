//! Backend for the control panel: radios (Wi-Fi, Bluetooth, "airplane" = all off), Wi-Fi
//! networks, wired adapters, display brightness, dark mode and microphone mute.
//!
//! Radios (WinRT), networks (WLAN API) and brightness (WMI) can be slow, so they live on a
//! background thread that polls (often while the panel is open, rarely otherwise) and takes
//! commands; the UI reads the latest `Status`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::network::{self, Adapter, Network, Wlan};

use windows::Devices::Radios::{Radio, RadioKind, RadioState};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, EOAC_NONE,
    RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_DWORD, RRF_RT_REG_DWORD, RegGetValueW, RegSetKeyValueW};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Wmi::{
    IWbemClassObject, IWbemLocator, IWbemServices, WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_INFINITE,
    WbemLocator,
};
use windows::Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE};
use windows::core::{BSTR, w};

/// Latest known state; `None` = not available on this PC.
#[derive(Clone, Default, PartialEq)]
pub struct Status {
    pub wifi: Option<bool>,
    pub bluetooth: Option<bool>,
    pub brightness: Option<f32>,
    /// Visible Wi-Fi networks (empty while Wi-Fi is off).
    pub networks: Vec<Network>,
    pub ethernet: Vec<Adapter>,
    /// The network being connected to.
    pub connecting: Option<String>,
    /// Why the last Wi-Fi action failed.
    pub wifi_error: Option<String>,
    pub scanning: bool,
    /// Windows hides the network list: Location is off for desktop apps.
    pub wifi_blocked: bool,
}

impl Status {
    /// "Airplane mode": every radio off.
    pub fn airplane(&self) -> bool {
        let radios = [self.wifi, self.bluetooth];
        radios.iter().any(Option::is_some) && radios.iter().all(|r| *r != Some(true))
    }

    pub fn wifi_network(&self) -> Option<&Network> {
        self.networks.iter().find(|n| n.connected)
    }

    pub fn ethernet_connected(&self) -> bool {
        self.ethernet.iter().any(|a| a.connected)
    }
}

pub enum Command {
    /// Poll right away (the panel just opened).
    Refresh,
    Wifi(bool),
    Bluetooth(bool),
    /// Airplane on = all radios off; off = all radios back on.
    Airplane(bool),
    Brightness(f32),
    Scan,
    Connect { ssid: String, password: Option<String> },
    Disconnect,
}

pub struct Control {
    status: Arc<Mutex<Status>>,
    tx: Sender<Command>,
    /// The panel is open: poll often.
    active: Arc<AtomicBool>,
    /// Bumped by every radio command, so a poll that started before it can't undo the
    /// optimistic update.
    generation: Arc<AtomicU64>,
}

impl Control {
    pub fn spawn() -> Self {
        let status = Arc::new(Mutex::new(Status::default()));
        let (tx, rx) = channel();
        let active = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        let (s, a, g) = (status.clone(), active.clone(), generation.clone());
        std::thread::spawn(move || worker(rx, s, a, g));
        Self { status, tx, active, generation }
    }

    pub fn send(&self, cmd: Command) {
        // Show the change right away; the worker confirms it on its next poll.
        {
            let mut s = self.status.lock().unwrap();
            let set = |r: &mut Option<bool>, on: bool| {
                if r.is_some() {
                    *r = Some(on);
                }
            };
            match &cmd {
                Command::Wifi(on) => set(&mut s.wifi, *on),
                Command::Bluetooth(on) => set(&mut s.bluetooth, *on),
                Command::Airplane(on) => {
                    set(&mut s.wifi, !on);
                    set(&mut s.bluetooth, !on);
                }
                Command::Connect { ssid, .. } => {
                    s.connecting = Some(ssid.clone());
                    s.wifi_error = None;
                }
                Command::Scan => {
                    s.scanning = true;
                    s.wifi_error = None;
                }
                _ => {}
            }
            if matches!(cmd, Command::Wifi(_) | Command::Bluetooth(_) | Command::Airplane(_)) {
                self.generation.fetch_add(1, Ordering::SeqCst);
                if s.wifi != Some(true) {
                    s.networks.clear();
                }
            }
        }
        let _ = self.tx.send(cmd);
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    /// The "panel is open" flag, for clearing it without going through `State`.
    pub fn active_flag(&self) -> Arc<AtomicBool> {
        self.active.clone()
    }

    /// Polls often while the panel is open (and right away when it opens).
    pub fn set_active(&self, active: bool) {
        if !self.active.swap(active, Ordering::Relaxed) && active {
            self.send(Command::Refresh);
        }
    }
}

struct Pending {
    ssid: String,
    /// A profile saved for this attempt, deleted again if it fails (likely a wrong password).
    created: Option<String>,
    deadline: Instant,
}

fn worker(rx: Receiver<Command>, status: Arc<Mutex<Status>>, active: Arc<AtomicBool>, generation: Arc<AtomicU64>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let _ = Radio::RequestAccessAsync().and_then(|op| op.join());
    let wmi = Wmi::connect();
    let wlan = Wlan::open();
    let mut pending: Option<Pending> = None;
    let mut error: Option<String> = None;
    let mut scanning_until: Option<Instant> = None;
    loop {
        let generation_before = generation.load(Ordering::SeqCst);
        let (wifi, bluetooth) = radio_states();
        let brightness = wmi.as_ref().and_then(Wmi::brightness);
        let (networks, blocked) = match (&wlan, wifi) {
            (Some(wlan), Some(true)) => match wlan.networks() {
                Ok(list) => (list, false),
                Err(network::Blocked) => (Vec::new(), true),
            },
            _ => (Vec::new(), false),
        };
        let ethernet = network::ethernet();
        if let Some(p) = &pending {
            if networks.iter().any(|n| n.ssid == p.ssid && n.connected) {
                pending = None;
            } else if Instant::now() > p.deadline {
                if let (Some(name), Some(wlan)) = (&p.created, &wlan) {
                    wlan.delete_profile(name);
                }
                error = Some(match p.created {
                    Some(_) => format!("Couldn't connect to {}. Check the password.", p.ssid),
                    None => format!("Couldn't connect to {}", p.ssid),
                });
                pending = None;
            }
        }
        let scanning = scanning_until.is_some_and(|t| Instant::now() < t);
        {
            let mut s = status.lock().unwrap();
            if generation.load(Ordering::SeqCst) == generation_before {
                s.wifi = wifi;
                s.bluetooth = bluetooth;
                s.networks = networks;
                s.wifi_blocked = blocked;
            }
            s.brightness = brightness;
            s.ethernet = ethernet;
            s.connecting = pending.as_ref().map(|p| p.ssid.clone());
            s.wifi_error = error.clone();
            s.scanning = scanning;
        }
        let timeout = if pending.is_some() || scanning {
            Duration::from_millis(800)
        } else if active.load(Ordering::Relaxed) {
            Duration::from_secs(2)
        } else {
            Duration::from_secs(15)
        };
        match rx.recv_timeout(timeout) {
            Ok(first) => {
                // Dragging the brightness slider sends many values: only the latest matters.
                let mut brightness = None;
                for cmd in std::iter::once(first).chain(rx.try_iter()) {
                    match cmd {
                        Command::Refresh => {}
                        Command::Wifi(on) => set_radios(&[RadioKind::WiFi], on),
                        Command::Bluetooth(on) => set_radios(&[RadioKind::Bluetooth], on),
                        Command::Airplane(on) => set_radios(&[RadioKind::WiFi, RadioKind::Bluetooth, RadioKind::MobileBroadband], !on),
                        Command::Brightness(pct) => brightness = Some(pct),
                        Command::Scan => {
                            error = None;
                            if let Some(wlan) = &wlan {
                                wlan.scan();
                            }
                            scanning_until = Some(Instant::now() + Duration::from_secs(4));
                        }
                        Command::Connect { ssid, password } => {
                            error = None;
                            let Some(wlan) = &wlan else { continue };
                            let Some(net) = wlan.networks().unwrap_or_default().into_iter().find(|n| n.ssid == ssid) else {
                                error = Some(format!("{ssid} is out of range"));
                                continue;
                            };
                            match wlan.connect(&net, password.as_deref()) {
                                Ok(created) => {
                                    pending = Some(Pending { ssid, created, deadline: Instant::now() + Duration::from_secs(20) });
                                }
                                Err(e) => error = Some(e),
                            }
                        }
                        Command::Disconnect => {
                            if let Some(wlan) = &wlan {
                                wlan.disconnect();
                            }
                        }
                    }
                }
                if let (Some(pct), Some(wmi)) = (brightness, &wmi) {
                    wmi.set_brightness(pct);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn radios() -> Vec<Radio> {
    Radio::GetRadiosAsync().and_then(|op| op.join()).map(|list| list.into_iter().collect()).unwrap_or_default()
}

fn radio_states() -> (Option<bool>, Option<bool>) {
    let (mut wifi, mut bt) = (None, None);
    for r in radios() {
        let on = r.State().is_ok_and(|s| s == RadioState::On);
        match r.Kind() {
            Ok(RadioKind::WiFi) => wifi = Some(wifi.unwrap_or(false) || on),
            Ok(RadioKind::Bluetooth) => bt = Some(bt.unwrap_or(false) || on),
            _ => {}
        }
    }
    (wifi, bt)
}

fn set_radios(kinds: &[RadioKind], on: bool) {
    let state = if on { RadioState::On } else { RadioState::Off };
    for r in radios() {
        if r.Kind().is_ok_and(|k| kinds.contains(&k)) {
            let _ = r.SetStateAsync(state).and_then(|op| op.join());
        }
    }
}

/// Laptop display brightness through WMI (root\WMI). External monitors don't support it.
struct Wmi {
    services: IWbemServices,
}

impl Wmi {
    fn connect() -> Option<Self> {
        unsafe {
            let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER).ok()?;
            let services =
                locator.ConnectServer(&BSTR::from(r"root\WMI"), &BSTR::new(), &BSTR::new(), &BSTR::new(), 0, &BSTR::new(), None).ok()?;
            CoSetProxyBlanket(
                &services,
                RPC_C_AUTHN_WINNT,
                RPC_C_AUTHZ_NONE,
                None,
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
            )
            .ok()?;
            Some(Self { services })
        }
    }

    fn first(&self, query: &str) -> Option<IWbemClassObject> {
        unsafe {
            let items = self
                .services
                .ExecQuery(&BSTR::from("WQL"), &BSTR::from(query), WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY, None)
                .ok()?;
            let mut row = [None];
            let mut returned = 0;
            let _ = items.Next(WBEM_INFINITE, &mut row, &mut returned);
            row[0].take()
        }
    }

    fn brightness(&self) -> Option<f32> {
        let obj = self.first("SELECT CurrentBrightness FROM WmiMonitorBrightness")?;
        let mut value = VARIANT::default();
        unsafe { obj.Get(w!("CurrentBrightness"), 0, &mut value, None, None) }.ok()?;
        u32::try_from(&value).ok().map(|v| v as f32)
    }

    fn set_brightness(&self, percent: f32) {
        let _ = (|| -> Option<()> {
            let target = self.first("SELECT * FROM WmiMonitorBrightnessMethods")?;
            let mut path = VARIANT::default();
            unsafe { target.Get(w!("__PATH"), 0, &mut path, None, None) }.ok()?;
            let path = BSTR::try_from(&path).ok()?;
            unsafe {
                let mut class = None;
                self.services.GetObject(&BSTR::from("WmiMonitorBrightnessMethods"), Default::default(), None, Some(&mut class), None).ok()?;
                let mut signature = None;
                class?.GetMethod(w!("WmiSetBrightness"), 0, &mut signature, std::ptr::null_mut()).ok()?;
                let params = signature?.SpawnInstance(0).ok()?;
                params.Put(w!("Timeout"), 0, &VARIANT::from(0i32), 0).ok()?;
                params.Put(w!("Brightness"), 0, &VARIANT::from(percent.round().clamp(0.0, 100.0) as u8), 0).ok()?;
                self.services
                    .ExecMethod(&path, &BSTR::from("WmiSetBrightness"), Default::default(), None, &params, None, None)
                    .ok()?;
            }
            Some(())
        })();
    }
}

const PERSONALIZE: windows::core::PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");

pub fn dark_mode() -> bool {
    let mut value = 1u32;
    let mut size = 4u32;
    unsafe {
        let _ = RegGetValueW(
            HKEY_CURRENT_USER,
            PERSONALIZE,
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        );
    }
    value == 0
}

/// Switches Windows (apps and system) between light and dark.
pub fn set_dark_mode(dark: bool) {
    let light: u32 = (!dark).into();
    unsafe {
        for name in [w!("AppsUseLightTheme"), w!("SystemUsesLightTheme")] {
            let _ = RegSetKeyValueW(HKEY_CURRENT_USER, PERSONALIZE, name, REG_DWORD.0, Some((&light as *const u32).cast()), 4);
        }
        // Tell running apps and Explorer to repaint.
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(w!("ImmersiveColorSet").as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            200,
            None,
        );
    }
}

/// Every tile the control panel can show: (id, label, icon, is a toggle).
pub const TILES: &[(&str, &str, &str, bool)] = &[
    ("wifi", "Wi-Fi", "\u{E701}", true),
    ("ethernet", "Ethernet", "\u{E839}", false),
    ("bluetooth", "Bluetooth", "\u{E702}", true),
    ("airplane", "Airplane", "\u{E709}", true),
    ("dark_mode", "Dark mode", "\u{E708}", true),
    ("taskbar", "Hide taskbar", "\u{E740}", true),
    ("mute", "Mute", "\u{E74F}", true),
    ("mic", "Mic off", "\u{E720}", true),
    ("screenshot", "Screenshot", "\u{E722}", false),
    ("settings", "Settings", "\u{E713}", false),
    ("lock", "Lock", "\u{E72E}", false),
    ("taskmgr", "Task Manager", "\u{E9D9}", false),
    ("project", "Project", "\u{E7F4}", false),
];

/// Optional sections below the tiles: (id, label).
pub const SECTIONS: &[(&str, &str)] =
    &[("volume", "Volume slider"), ("brightness", "Brightness slider"), ("media", "Media card"), ("power", "Power buttons")];
