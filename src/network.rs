//! Wi-Fi networks (native WLAN API) and wired adapters (IP helper) for the control panel.
//! Everything here is blocking and runs on the control worker thread.

use std::ptr::null_mut;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses, GetIfEntry2,
    IP_ADAPTER_ADDRESSES_LH, MIB_IF_ROW2,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::NetworkManagement::WiFi::{
    DOT11_AUTH_ALGO_80211_OPEN, DOT11_AUTH_ALGO_RSNA_PSK, DOT11_AUTH_ALGO_WPA_PSK, DOT11_AUTH_ALGO_WPA3_SAE,
    DOT11_BSS_TYPE, DOT11_CIPHER_ALGO_NONE, DOT11_CIPHER_ALGO_TKIP, WLAN_AVAILABLE_NETWORK_CONNECTED,
    WLAN_AVAILABLE_NETWORK_LIST, WLAN_CONNECTION_PARAMETERS, WLAN_INTERFACE_INFO_LIST, WlanCloseHandle, WlanConnect,
    WlanDeleteProfile, WlanDisconnect, WlanEnumInterfaces, WlanFreeMemory, WlanGetAvailableNetworkList, WlanOpenHandle,
    WlanScan, WlanSetProfile, wlan_connection_mode_profile,
};
use windows::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR_IN};
use windows::core::{GUID, HSTRING, PCWSTR};

/// Opportunistic Wireless Encryption: encrypted, but no password.
const AUTH_OWE: i32 = 10;
const IF_TYPE_ETHERNET: u32 = 6;

#[derive(Clone, PartialEq)]
pub struct Network {
    pub ssid: String,
    /// 0..100
    pub signal: u8,
    pub secured: bool,
    pub connected: bool,
    /// Windows already has a profile (a saved password) for it.
    pub saved: bool,
    /// 802.1X (work/school) networks need Windows' own sign-in UI.
    pub enterprise: bool,
    profile: String,
    auth: i32,
    cipher: i32,
    bss_type: i32,
}

impl Network {
    /// Whether connecting needs a password typed in.
    pub fn needs_password(&self) -> bool {
        self.secured && !self.saved && !self.enterprise && self.auth != AUTH_OWE
    }
}

fn wide(s: &[u16]) -> String {
    let n = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..n])
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

pub struct Wlan {
    handle: HANDLE,
}

impl Drop for Wlan {
    fn drop(&mut self) {
        unsafe {
            WlanCloseHandle(self.handle, None);
        }
    }
}

impl Wlan {
    pub fn open() -> Option<Self> {
        let (mut version, mut handle) = (0, HANDLE::default());
        (unsafe { WlanOpenHandle(2, None, &mut version, &mut handle) } == 0).then_some(Self { handle })
    }

    /// The first Wi-Fi adapter (looked up each time: USB adapters come and go).
    fn interface(&self) -> Option<GUID> {
        unsafe {
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = null_mut();
            if WlanEnumInterfaces(self.handle, None, &mut list) != 0 || list.is_null() {
                return None;
            }
            let guid = ((*list).dwNumberOfItems > 0).then(|| (*list).InterfaceInfo[0].InterfaceGuid);
            WlanFreeMemory(list as *const _);
            guid
        }
    }

    /// Asks the adapter to look for networks; results show up in `networks()` a few seconds later.
    pub fn scan(&self) {
        if let Some(guid) = self.interface() {
            unsafe {
                WlanScan(self.handle, &guid, None, None, None);
            }
        }
    }

    /// Visible networks, one entry per name, connected first, then strongest.
    /// `Err(Blocked)` when Windows' location privacy setting hides them from apps.
    pub fn networks(&self) -> Result<Vec<Network>, Blocked> {
        let Some(guid) = self.interface() else { return Ok(Vec::new()) };
        let mut out: Vec<Network> = Vec::new();
        unsafe {
            let mut list: *mut WLAN_AVAILABLE_NETWORK_LIST = null_mut();
            match WlanGetAvailableNetworkList(self.handle, &guid, 0, None, &mut list) {
                0 if !list.is_null() => {}
                ERROR_ACCESS_DENIED => return Err(Blocked),
                _ => return Ok(out),
            }
            let items = std::slice::from_raw_parts((*list).Network.as_ptr(), (*list).dwNumberOfItems as usize);
            for it in items {
                let len = (it.dot11Ssid.uSSIDLength as usize).min(32);
                let ssid = String::from_utf8_lossy(&it.dot11Ssid.ucSSID[..len]).into_owned();
                if ssid.is_empty() {
                    continue; // hidden network
                }
                let auth = it.dot11DefaultAuthAlgorithm.0;
                let net = Network {
                    ssid,
                    signal: it.wlanSignalQuality.min(100) as u8,
                    secured: it.bSecurityEnabled.as_bool(),
                    connected: it.dwFlags & WLAN_AVAILABLE_NETWORK_CONNECTED != 0,
                    saved: false,
                    enterprise: matches!(auth, 3 | 6 | 8 | 11),
                    profile: wide(&it.strProfileName),
                    auth,
                    cipher: it.dot11DefaultCipherAlgorithm.0,
                    bss_type: it.dot11BssType.0,
                };
                // Windows lists a network once per saved profile plus once without: merge them.
                match out.iter_mut().find(|n| n.ssid == net.ssid) {
                    Some(n) => {
                        n.signal = n.signal.max(net.signal);
                        n.connected |= net.connected;
                        if n.profile.is_empty() || net.connected {
                            n.profile = net.profile;
                        }
                    }
                    None => out.push(net),
                }
            }
            WlanFreeMemory(list as *const _);
        }
        for n in &mut out {
            n.saved = !n.profile.is_empty();
        }
        out.sort_by(|a, b| b.connected.cmp(&a.connected).then(b.signal.cmp(&a.signal)));
        Ok(out)
    }

    /// Starts connecting. With a password, a (new or replaced) profile is saved first and its
    /// name returned, so a failed attempt can be cleaned up.
    pub fn connect(&self, net: &Network, password: Option<&str>) -> Result<Option<String>, String> {
        let guid = self.interface().ok_or("No Wi-Fi adapter")?;
        let mut created = None;
        let profile = if net.saved && password.is_none() {
            net.profile.clone()
        } else {
            let xml = profile_xml(net, password.unwrap_or(""))?;
            let mut reason = 0;
            let r = unsafe { WlanSetProfile(self.handle, &guid, 0, &HSTRING::from(xml), PCWSTR::null(), true, None, &mut reason) };
            if r != 0 {
                return Err("Windows rejected the network settings".into());
            }
            created = Some(net.ssid.clone());
            net.ssid.clone()
        };
        let name = HSTRING::from(profile);
        let params = WLAN_CONNECTION_PARAMETERS {
            wlanConnectionMode: wlan_connection_mode_profile,
            strProfile: PCWSTR(name.as_ptr()),
            pDot11Ssid: null_mut(),
            pDesiredBssidList: null_mut(),
            dot11BssType: DOT11_BSS_TYPE(net.bss_type),
            dwFlags: 0,
        };
        match unsafe { WlanConnect(self.handle, &guid, &params, None) } {
            0 => Ok(created),
            _ => Err(format!("Couldn't connect to {}", net.ssid)),
        }
    }

    pub fn disconnect(&self) {
        if let Some(guid) = self.interface() {
            unsafe {
                WlanDisconnect(self.handle, &guid, None);
            }
        }
    }

    /// Forgets a profile we saved (after a wrong password).
    pub fn delete_profile(&self, name: &str) {
        if let Some(guid) = self.interface() {
            unsafe {
                WlanDeleteProfile(self.handle, &guid, &HSTRING::from(name), None);
            }
        }
    }
}

/// Listing networks needs Location access for desktop apps (Windows 11 24H2 and later).
#[derive(Debug)]
pub struct Blocked;

const ERROR_ACCESS_DENIED: u32 = 5;

/// A WLAN profile for a personal (password or open) network.
fn profile_xml(net: &Network, password: &str) -> Result<String, String> {
    let (auth, encryption, key) = match net.auth {
        _ if net.enterprise => return Err("This network needs Windows' sign-in".into()),
        a if a == DOT11_AUTH_ALGO_80211_OPEN.0 && net.cipher == DOT11_CIPHER_ALGO_NONE.0 => ("open", "none", false),
        AUTH_OWE => ("OWE", "AES", false),
        a if a == DOT11_AUTH_ALGO_RSNA_PSK.0 => ("WPA2PSK", if net.cipher == DOT11_CIPHER_ALGO_TKIP.0 { "TKIP" } else { "AES" }, true),
        a if a == DOT11_AUTH_ALGO_WPA_PSK.0 => ("WPAPSK", if net.cipher == DOT11_CIPHER_ALGO_TKIP.0 { "TKIP" } else { "AES" }, true),
        a if a == DOT11_AUTH_ALGO_WPA3_SAE.0 => ("WPA3SAE", "AES", true),
        _ => return Err("This network's security type isn't supported here".into()),
    };
    if key && password.len() < 8 {
        return Err("The password must be at least 8 characters".into());
    }
    let hex: String = net.ssid.bytes().map(|b| format!("{b:02X}")).collect();
    let name = xml_escape(&net.ssid);
    let shared_key = if key {
        format!(
            "<sharedKey><keyType>passPhrase</keyType><protected>false</protected><keyMaterial>{}</keyMaterial></sharedKey>",
            xml_escape(password)
        )
    } else {
        String::new()
    };
    Ok(format!(
        r#"<?xml version="1.0"?><WLANProfile xmlns="http://www.microsoft.com/networking/WLAN/profile/v1"><name>{name}</name><SSIDConfig><SSID><hex>{hex}</hex><name>{name}</name></SSID></SSIDConfig><connectionType>ESS</connectionType><connectionMode>auto</connectionMode><MSM><security><authEncryption><authentication>{auth}</authentication><encryption>{encryption}</encryption><useOneX>false</useOneX></authEncryption>{shared_key}</security></MSM></WLANProfile>"#
    ))
}

#[derive(Clone, PartialEq)]
pub struct Adapter {
    pub name: String,
    pub description: String,
    pub connected: bool,
    pub ipv4: String,
    /// e.g. "1 Gbps"
    pub speed: String,
}

fn speed_text(bits: u64) -> String {
    match bits {
        0 | u64::MAX => String::new(),
        b if b >= 1_000_000_000 => format!("{} Gbps", (b as f64 / 1e9 * 10.0).round() / 10.0),
        b => format!("{} Mbps", b / 1_000_000),
    }
}

/// Physical wired adapters (virtual switches, VPNs and the like are left out).
pub fn ethernet() -> Vec<Adapter> {
    let mut size = 16 * 1024u32;
    let mut buf: Vec<u64>;
    loop {
        buf = vec![0u64; size as usize / 8 + 1];
        let r = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC.0 as u32,
                GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                None,
                Some(buf.as_mut_ptr().cast()),
                &mut size,
            )
        };
        match r {
            0 => break,
            111 => continue, // ERROR_BUFFER_OVERFLOW: `size` now holds what's needed
            _ => return Vec::new(),
        }
    }
    let mut out = Vec::new();
    let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while let Some(a) = unsafe { p.as_ref() } {
        p = a.Next;
        if a.IfType != IF_TYPE_ETHERNET {
            continue;
        }
        let mut row = MIB_IF_ROW2 { InterfaceLuid: a.Luid, ..Default::default() };
        let hardware = unsafe { GetIfEntry2(&mut row) }.is_ok() && row.InterfaceAndOperStatusFlags._bitfield & 1 != 0;
        if !hardware {
            continue;
        }
        let mut ipv4 = String::new();
        let mut u = a.FirstUnicastAddress;
        while let Some(addr) = unsafe { u.as_ref() } {
            u = addr.Next;
            let sa = addr.Address.lpSockaddr;
            if !sa.is_null() && unsafe { (*sa).sa_family } == AF_INET {
                let b = unsafe { (*(sa as *const SOCKADDR_IN)).sin_addr.S_un.S_addr }.to_ne_bytes();
                ipv4 = format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]);
                break;
            }
        }
        let connected = a.OperStatus == IfOperStatusUp;
        out.push(Adapter {
            name: unsafe { a.FriendlyName.to_string() }.unwrap_or_default(),
            description: unsafe { a.Description.to_string() }.unwrap_or_default(),
            connected,
            ipv4,
            speed: if connected { speed_text(a.ReceiveLinkSpeed) } else { String::new() },
        });
    }
    out
}
