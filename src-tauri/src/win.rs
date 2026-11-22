//! Small Windows helpers, each standing in for a dependency that did much more than this app needs.

use std::ptr::{null, null_mut};
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, FILETIME, SYSTEMTIME, WIN32_ERROR};
use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn check(status: WIN32_ERROR) -> Result<(), String> {
    match status {
        ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()), // deleting what isn't there is fine
        e => Err(std::io::Error::from_raw_os_error(e as i32).to_string()),
    }
}

// ---- Start with Windows: a value named after the app under HKCU\...\Run ----
// (The NSIS uninstaller deletes that same value.)

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Where Task Manager's Startup tab records an entry it has disabled.
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

pub fn autostart_enabled(name: &str) -> bool {
    let name = wide(name);
    let get = |key: &str, kind, buf: *mut u8, len: &mut u32| unsafe {
        RegGetValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), name.as_ptr(), kind, null_mut(), buf.cast(), len)
    };
    if get(RUN, RRF_RT_REG_SZ, null_mut(), &mut 0) != ERROR_SUCCESS {
        return false;
    }
    // The first byte is even when enabled (2) and odd when disabled in Task Manager (3).
    let mut flags = [0u8; 12];
    let found = get(APPROVED, RRF_RT_REG_BINARY, flags.as_mut_ptr(), &mut (flags.len() as u32)) == ERROR_SUCCESS;
    !found || flags[0] & 1 == 0
}

pub fn set_autostart(name: &str, on: bool) -> Result<(), String> {
    let name = wide(name);
    let delete = |key: &str| check(unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), name.as_ptr()) });
    delete(APPROVED)?; // so turning it on here also undoes a "Disabled" from Task Manager
    if !on {
        return delete(RUN);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let command = wide(&format!("\"{}\"", exe.display()));
    check(unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            wide(RUN).as_ptr(),
            name.as_ptr(),
            REG_SZ,
            command.as_ptr().cast(),
            (command.len() * 2) as u32,
        )
    })
}

// ---- Global hotkey ----

/// Calls `pressed` whenever Ctrl+Shift+`key` is pressed, from any app. `key` is an uppercase letter.
/// The hotkey belongs to the thread that registers it, so it gets a thread of its own to wait on.
pub fn on_ctrl_shift(key: char, pressed: impl Fn() + Send + 'static) {
    std::thread::spawn(move || unsafe {
        if RegisterHotKey(null_mut(), 1, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, key as u32) == 0 {
            crate::log(&format!("Couldn't register Ctrl+Shift+{key} (another app may be using it)"));
            return;
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if msg.message == WM_HOTKEY {
                pressed();
            }
        }
    });
}

// ---- Local time ----

/// `t` in the PC's time zone (fields wYear, wMonth, wDay, wHour, wMinute, wSecond).
pub fn local_time(t: SystemTime) -> SYSTEMTIME {
    // FILETIME counts 100 ns steps from 1601; the Unix epoch is 11644473600 s after that.
    let since_1970 = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let ticks = since_1970.as_nanos() as u64 / 100 + 11_644_473_600 * 10_000_000;
    let file = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    unsafe {
        let (mut utc, mut local) = (std::mem::zeroed(), std::mem::zeroed());
        FileTimeToSystemTime(&file, &mut utc);
        SystemTimeToTzSpecificLocalTime(null(), &utc, &mut local);
        local
    }
}
