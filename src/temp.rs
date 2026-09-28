//! Temperature for the "temp" module. Without admin rights Windows exposes no CPU
//! temperature, so this reads the NVIDIA GPU through NVML (the library nvidia-smi uses),
//! loaded at runtime so the bar still starts on machines without an NVIDIA driver.
//!
//! Polling runs on its own thread (a sleeping laptop GPU can take a while to answer) and
//! only while the module is shown: querying a laptop's dGPU keeps it from powering down.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{s, w};

/// Never poll the GPU faster than this, even if the stats interval is shorter.
const MIN_POLL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
pub struct Reading {
    pub celsius: f32,
    pub source: &'static str,
}

#[derive(Default)]
pub struct Shared {
    /// Set by the UI when the module is in the layout.
    pub enabled: AtomicBool,
    pub latest: Mutex<Option<Reading>>,
    /// The stats interval chosen in settings.
    pub interval_ms: AtomicU64,
}

type Init = unsafe extern "C" fn() -> u32;
type HandleByIndex = unsafe extern "C" fn(u32, *mut *mut c_void) -> u32;
type GetTemperature = unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> u32;

struct Nvml {
    device: *mut c_void,
    get_temperature: GetTemperature,
}

impl Nvml {
    fn load() -> Option<Self> {
        unsafe {
            let lib = LoadLibraryW(w!("nvml.dll")).ok()?;
            let init: Init = std::mem::transmute(GetProcAddress(lib, s!("nvmlInit_v2"))?);
            let by_index: HandleByIndex = std::mem::transmute(GetProcAddress(lib, s!("nvmlDeviceGetHandleByIndex_v2"))?);
            let get_temperature: GetTemperature = std::mem::transmute(GetProcAddress(lib, s!("nvmlDeviceGetTemperature"))?);
            if init() != 0 {
                return None;
            }
            let mut device = std::ptr::null_mut();
            if by_index(0, &mut device) != 0 {
                return None;
            }
            Some(Self { device, get_temperature })
        }
    }

    fn gpu_celsius(&self) -> Option<f32> {
        const NVML_TEMPERATURE_GPU: u32 = 0;
        let mut t = 0u32;
        let ok = unsafe { (self.get_temperature)(self.device, NVML_TEMPERATURE_GPU, &mut t) } == 0;
        ok.then_some(t as f32)
    }
}

pub fn spawn() -> Arc<Shared> {
    let shared = Arc::new(Shared::default());
    let s = shared.clone();
    std::thread::spawn(move || {
        let mut nvml: Option<Nvml> = None;
        let mut tried = false;
        loop {
            if s.enabled.load(Ordering::Relaxed) {
                if !tried {
                    nvml = Nvml::load();
                    tried = true;
                }
                let reading = nvml.as_ref().and_then(Nvml::gpu_celsius).map(|celsius| Reading { celsius, source: "GPU" });
                *s.latest.lock().unwrap() = reading;
                let interval = Duration::from_millis(s.interval_ms.load(Ordering::Relaxed));
                std::thread::sleep(interval.max(MIN_POLL));
            } else {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
    shared
}

pub fn format(r: Reading, fahrenheit: bool) -> String {
    if fahrenheit {
        format!("{:.0}°F", r.celsius * 9.0 / 5.0 + 32.0)
    } else {
        format!("{:.0}°C", r.celsius)
    }
}
