//! Core Audio: master and microphone volume, output/input devices (and switching the
//! default one), and per-app volume.

use std::collections::HashMap;

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, S_OK};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    AudioSessionStateExpired, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, eCapture, eCommunications, eConsole, eMultimedia, eRender,
};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{GUID, HRESULT, HSTRING, IUnknown, Interface, PCWSTR, PWSTR, w};

pub struct Audio {
    enumerator: IMMDeviceEnumerator,
}

impl Audio {
    pub fn new() -> Option<Self> {
        unsafe {
            // winit already initialized COM on this thread; this is a harmless no-op then.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok().map(|enumerator| Self { enumerator })
        }
    }

    /// Looked up on every call so switching output devices is picked up.
    fn endpoint(&self) -> Option<IAudioEndpointVolume> {
        unsafe {
            let device = self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            device.Activate(CLSCTX_ALL, None).ok()
        }
    }

    /// (volume percent, muted)
    pub fn get(&self) -> Option<(f32, bool)> {
        let ep = self.endpoint()?;
        unsafe {
            let level = ep.GetMasterVolumeLevelScalar().ok()?;
            let muted = ep.GetMute().ok()?.as_bool();
            Some((level * 100.0, muted))
        }
    }

    pub fn adjust(&self, delta_percent: f32) {
        if let (Some(ep), Some((vol, _))) = (self.endpoint(), self.get()) {
            let v = ((vol + delta_percent) / 100.0).clamp(0.0, 1.0);
            unsafe {
                let _ = ep.SetMasterVolumeLevelScalar(v, std::ptr::null());
                if delta_percent > 0.0 {
                    let _ = ep.SetMute(false, std::ptr::null());
                }
            }
        }
    }

    pub fn toggle_mute(&self) {
        if let (Some(ep), Some((_, muted))) = (self.endpoint(), self.get()) {
            unsafe {
                let _ = ep.SetMute(!muted, std::ptr::null());
            }
        }
    }
}

impl Audio {
    /// The default microphone's volume control.
    fn capture_endpoint(&self) -> Option<IAudioEndpointVolume> {
        unsafe {
            let device = self.enumerator.GetDefaultAudioEndpoint(eCapture, eCommunications).ok()?;
            device.Activate(CLSCTX_ALL, None).ok()
        }
    }

    /// Whether the microphone is muted; None without a microphone.
    pub fn mic_muted(&self) -> Option<bool> {
        let ep = self.capture_endpoint()?;
        unsafe { ep.GetMute() }.ok().map(|m| m.as_bool())
    }

    pub fn toggle_mic(&self) {
        if let (Some(ep), Some(muted)) = (self.capture_endpoint(), self.mic_muted()) {
            unsafe {
                let _ = ep.SetMute(!muted, std::ptr::null());
            }
        }
    }

    /// Sets the speaker volume to `percent`.
    pub fn set_volume(&self, percent: f32) {
        if let Some(ep) = self.endpoint() {
            unsafe {
                let _ = ep.SetMasterVolumeLevelScalar((percent / 100.0).clamp(0.0, 1.0), std::ptr::null());
            }
        }
    }
}

/// An output or input device.
#[derive(Clone, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub default: bool,
}

/// Volume of one app (all of its audio sessions together).
#[derive(Clone, PartialEq)]
pub struct AppVolume {
    /// The executable path ("" for Windows' system sounds); identifies the app.
    pub key: String,
    pub name: String,
    pub volume: f32,
    pub muted: bool,
}

fn take_pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn device_id(device: &IMMDevice) -> String {
    take_pwstr(unsafe { device.GetId() }.unwrap_or(PWSTR::null()))
}

fn device_name(device: &IMMDevice) -> String {
    unsafe { device.OpenPropertyStore(STGM_READ).and_then(|store| store.GetValue(&PKEY_Device_FriendlyName)) }
        .map(|v| v.to_string())
        .unwrap_or_default()
}

/// An app's display name from its executable's version info ("Google Chrome"), or the file name.
fn exe_description(path: &str) -> String {
    let fallback =
        || std::path::Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    unsafe {
        let file = HSTRING::from(path);
        let size = GetFileVersionInfoSizeW(&file, None);
        if size == 0 {
            return fallback();
        }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(&file, None, size, data.as_mut_ptr().cast()).is_err() {
            return fallback();
        }
        let (mut ptr, mut len) = (std::ptr::null_mut(), 0u32);
        if !VerQueryValueW(data.as_ptr().cast(), w!(r"\VarFileInfo\Translation"), &mut ptr, &mut len).as_bool() || len < 4 {
            return fallback();
        }
        let lang = *(ptr as *const u16);
        let codepage = *(ptr as *const u16).add(1);
        let key = HSTRING::from(format!(r"\StringFileInfo\{lang:04x}{codepage:04x}\FileDescription"));
        if !VerQueryValueW(data.as_ptr().cast(), &key, &mut ptr, &mut len).as_bool() || len == 0 {
            return fallback();
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr as *const u16, len as usize));
        let text = text.trim_end_matches('\0').trim();
        if text.is_empty() { fallback() } else { text.to_string() }
    }
}

fn process_path(pid: u32) -> Option<String> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(process);
        ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

// Changing the default device has no public API. Windows' own sound settings use the
// IPolicyConfig COM object, which has been stable since Windows 7.
const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
/// IPolicyConfig::SetDefaultEndpoint: the vtable slot after IUnknown's 3 and 10 other methods.
const SET_DEFAULT_ENDPOINT_SLOT: usize = 13;

impl Audio {
    /// Active output (`capture` false) or input devices, the default one marked.
    pub fn devices(&self, capture: bool) -> Vec<Device> {
        let flow = if capture { eCapture } else { eRender };
        unsafe {
            let default = self.enumerator.GetDefaultAudioEndpoint(flow, eConsole).ok().map(|d| device_id(&d));
            let Ok(list) = self.enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) else { return Vec::new() };
            (0..list.GetCount().unwrap_or(0))
                .filter_map(|i| list.Item(i).ok())
                .map(|d| {
                    let id = device_id(&d);
                    Device { default: default.as_deref() == Some(id.as_str()), name: device_name(&d), id }
                })
                .collect()
        }
    }

    /// Makes a device the default for everything (media, communications, system sounds).
    pub fn set_default_device(&self, id: &str) {
        unsafe {
            let Ok(policy) = CoCreateInstance::<_, IUnknown>(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL) else { return };
            let mut raw = std::ptr::null_mut();
            if policy.query(&IID_POLICY_CONFIG, &mut raw).is_err() || raw.is_null() {
                return;
            }
            let config = IUnknown::from_raw(raw); // released when dropped
            let vtable = *(raw as *const *const usize);
            let set_default: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, i32) -> HRESULT =
                std::mem::transmute(*vtable.add(SET_DEFAULT_ENDPOINT_SLOT));
            let id = HSTRING::from(id);
            for role in [eConsole, eMultimedia, eCommunications] {
                let _ = set_default(raw, PCWSTR(id.as_ptr()), role.0);
            }
            drop(config);
        }
    }

    /// Every live audio session of the default output: (app key, its volume, its control).
    fn sessions(&self) -> Vec<(String, ISimpleAudioVolume, IAudioSessionControl2)> {
        let mut out = Vec::new();
        unsafe {
            let Ok(device) = self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole) else { return out };
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { return out };
            let Ok(list) = manager.GetSessionEnumerator() else { return out };
            for i in 0..list.GetCount().unwrap_or(0) {
                let Ok(control) = list.GetSession(i) else { continue };
                if control.GetState().is_ok_and(|s| s == AudioSessionStateExpired) {
                    continue;
                }
                let (Ok(c2), Ok(volume)) = (control.cast::<IAudioSessionControl2>(), control.cast::<ISimpleAudioVolume>())
                else {
                    continue;
                };
                let key = if c2.IsSystemSoundsSession() == S_OK {
                    String::new()
                } else {
                    match c2.GetProcessId().ok().and_then(process_path) {
                        Some(path) => path,
                        None => continue, // its process is gone
                    }
                };
                out.push((key, volume, c2));
            }
        }
        out
    }

    /// Apps with audio sessions on the default output. `names` caches display names by key.
    pub fn apps(&self, names: &mut HashMap<String, String>) -> Vec<AppVolume> {
        let mut apps: Vec<AppVolume> = Vec::new();
        for (key, volume, control) in self.sessions() {
            let (level, muted) =
                unsafe { (volume.GetMasterVolume().unwrap_or(1.0), volume.GetMute().is_ok_and(|m| m.as_bool())) };
            if let Some(app) = apps.iter_mut().find(|a| a.key == key) {
                app.volume = app.volume.max(level * 100.0);
                app.muted &= muted;
                continue;
            }
            let name = names
                .entry(key.clone())
                .or_insert_with(|| {
                    if key.is_empty() {
                        return "System sounds".into();
                    }
                    // Some apps name their session; resource references ("@%SystemRoot%...") are unreadable.
                    let own = take_pwstr(unsafe { control.GetDisplayName() }.unwrap_or(PWSTR::null()));
                    if !own.is_empty() && !own.starts_with('@') { own } else { exe_description(&key) }
                })
                .clone();
            apps.push(AppVolume { key, name, volume: level * 100.0, muted });
        }
        // System sounds last, the rest by name.
        apps.sort_by(|a, b| {
            a.key.is_empty().cmp(&b.key.is_empty()).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        apps
    }

    pub fn set_app_volume(&self, key: &str, percent: f32) {
        for (k, volume, _) in self.sessions() {
            if k == key {
                unsafe {
                    let _ = volume.SetMasterVolume((percent / 100.0).clamp(0.0, 1.0), std::ptr::null());
                    if percent > 0.0 {
                        let _ = volume.SetMute(false, std::ptr::null());
                    }
                }
            }
        }
    }

    pub fn set_app_mute(&self, key: &str, mute: bool) {
        for (k, volume, _) in self.sessions() {
            if k == key {
                unsafe {
                    let _ = volume.SetMute(mute, std::ptr::null());
                }
            }
        }
    }

    /// The default microphone's level, 0..100.
    pub fn mic_volume(&self) -> Option<f32> {
        let ep = self.capture_endpoint()?;
        unsafe { ep.GetMasterVolumeLevelScalar() }.ok().map(|v| v * 100.0)
    }

    pub fn set_mic_volume(&self, percent: f32) {
        if let Some(ep) = self.capture_endpoint() {
            unsafe {
                let _ = ep.SetMasterVolumeLevelScalar((percent / 100.0).clamp(0.0, 1.0), std::ptr::null());
            }
        }
    }
}
