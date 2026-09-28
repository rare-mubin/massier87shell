//! Power actions and the small bits of system info shown in the power menu.

use std::os::windows::process::CommandExt;
use std::process::Command;

use windows::Win32::System::Power::{IsPwrHibernateAllowed, SetSuspendState};
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::System::SystemInformation::GetTickCount64;

use crate::PowerAction;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn user_name() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "User".into())
}

pub fn initial(name: &str) -> String {
    name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "?".into())
}

/// Time since boot, e.g. "3h 12m" or "2d 4h".
pub fn uptime() -> String {
    let mins = unsafe { GetTickCount64() } / 60_000;
    let (d, h, m) = (mins / 1440, (mins / 60) % 24, mins % 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

pub fn can_hibernate() -> bool {
    unsafe { IsPwrHibernateAllowed() }
}

fn shutdown_exe(args: &[&str]) {
    let _ = Command::new("shutdown").args(args).creation_flags(CREATE_NO_WINDOW).spawn();
}

pub fn run(action: PowerAction) {
    match action {
        PowerAction::Lock => unsafe {
            let _ = LockWorkStation();
        },
        PowerAction::Sleep => unsafe {
            let _ = SetSuspendState(false, false, false);
        },
        PowerAction::SignOut => shutdown_exe(&["/l"]),
        PowerAction::Restart => shutdown_exe(&["/r", "/t", "0"]),
        PowerAction::Shutdown => shutdown_exe(&["/s", "/t", "0"]),
        PowerAction::Hibernate => shutdown_exe(&["/h"]),
    }
}
