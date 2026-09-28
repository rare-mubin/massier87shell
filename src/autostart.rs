//! "Start with Windows".
//!
//! Uses a Task Scheduler task with an at-logon trigger: it starts right as you sign in.
//! Entries in the Run key are held back by Windows for several seconds after sign-in and
//! then start in a queue with every other startup app (about 17 s here). A per-user task
//! doesn't need admin rights. The Run key stays as a fallback if the task can't be created,
//! and an old Run-key entry is upgraded to the task (`refresh`).
//!
//! What Windows has registered is the source of truth, so the setting can never disagree
//! with what will actually happen.

use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW};
use windows::core::{HSTRING, w};

const RUN_KEY: windows::core::PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const VALUE: windows::core::PCWSTR = w!("massier87shell");
const TASK: &str = "massier87shell";
/// Names used before the app was renamed (massier87bar); replaced on the next `refresh`.
const OLD_VALUE: windows::core::PCWSTR = w!("massier87bar");
const OLD_TASK: &str = "massier87bar";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn schtasks(args: &[&str]) -> bool {
    Command::new("schtasks")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .is_ok_and(|s| s.success())
}

fn task_exists(name: &str) -> bool {
    schtasks(&["/Query", "/TN", name])
}

fn run_key_exists(value: windows::core::PCWSTR) -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, value, RRF_RT_REG_SZ, None, None, None) == ERROR_SUCCESS }
}

pub fn is_enabled() -> bool {
    task_exists(TASK) || run_key_exists(VALUE) || task_exists(OLD_TASK) || run_key_exists(OLD_VALUE)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Registers the at-logon task for this executable (replacing an existing one).
fn create_task(exe: &str) -> bool {
    let user = std::env::var("USERDOMAIN")
        .ok()
        .zip(std::env::var("USERNAME").ok())
        .map(|(d, u)| format!("{d}\\{u}"))
        .unwrap_or_default();
    // No time limit (the default kills tasks after 3 days), runs on battery, and normal
    // process priority (tasks default to below normal).
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Starts massier87shell when you sign in.</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>{}</Command></Exec></Actions>
</Task>"#,
        xml_escape(exe),
        user = xml_escape(&user),
    );
    // schtasks wants the XML as a file, in UTF-16 as declared.
    let path = std::env::temp_dir().join("massier87shell-task.xml");
    let utf16: Vec<u8> = std::iter::once(0xFEFFu16).chain(xml.encode_utf16()).flat_map(u16::to_le_bytes).collect();
    if std::fs::write(&path, utf16).is_err() {
        return false;
    }
    let ok = schtasks(&["/Create", "/TN", TASK, "/XML", &path.to_string_lossy(), "/F"]);
    let _ = std::fs::remove_file(&path);
    ok
}

fn set_run_key(exe: &str) -> Result<(), String> {
    let command = HSTRING::from(format!("\"{exe}\""));
    // Includes the terminating NUL, as REG_SZ requires.
    let bytes = (command.len() + 1) * 2;
    let status = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE, REG_SZ.0, Some(command.as_ptr().cast()), bytes as u32) };
    if status == ERROR_SUCCESS { Ok(()) } else { Err(format!("registry error {}", status.0)) }
}

fn remove_run_key() {
    unsafe {
        let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE);
    }
}

/// Removes the entries of the old name (massier87bar).
fn remove_old() {
    if task_exists(OLD_TASK) {
        schtasks(&["/Delete", "/TN", OLD_TASK, "/F"]);
    }
    unsafe {
        let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, OLD_VALUE);
    }
}

/// Registers (or removes) this executable to start at sign-in.
pub fn set(enabled: bool) -> Result<(), String> {
    remove_old();
    if !enabled {
        if task_exists(TASK) {
            schtasks(&["/Delete", "/TN", TASK, "/F"]);
        }
        remove_run_key();
        return if is_enabled() { Err("couldn't remove the startup entry".into()) } else { Ok(()) };
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?.display().to_string();
    if create_task(&exe) {
        remove_run_key(); // one way to start is enough
        Ok(())
    } else {
        set_run_key(&exe)
    }
}

/// When autostart is on, re-registers it: moves an old Run-key entry to the faster task, and
/// keeps the task pointing at this executable if it moved. Runs schtasks, so call it off the
/// UI thread.
pub fn refresh() {
    if is_enabled() {
        let _ = set(true);
    }
}
