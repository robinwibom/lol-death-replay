//! Small Windows helpers, each standing in for a dependency that did much more than this app needs.

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, FILETIME, SYSTEMTIME, WIN32_ERROR};
use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, VK_F1,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetMessageW, PeekMessageW, PostThreadMessageW, SetWindowDisplayAffinity, MSG, PM_NOREMOVE,
    WDA_EXCLUDEFROMCAPTURE, WM_HOTKEY, WM_QUIT, WM_USER,
};

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

// ---- Global hotkeys ----

/// A global hotkey, registered until dropped.
pub struct Hotkey {
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Hotkey {
    /// Calls `pressed` whenever `combo` (e.g. "Ctrl+Shift+S", see `parse_hotkey`) is pressed, from any app.
    /// The hotkey belongs to the thread that registers it, so it gets a thread of its own to wait on.
    pub fn register(combo: &str, pressed: impl Fn() + Send + 'static) -> Result<Hotkey, String> {
        let (mods, key) = parse_hotkey(combo)?;
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || unsafe {
            if RegisterHotKey(null_mut(), 1, mods | MOD_NOREPEAT, key) == 0 {
                let _ = tx.send(None);
                return;
            }
            // Makes sure the thread has a message queue before anyone posts WM_QUIT to it.
            let mut msg: MSG = std::mem::zeroed();
            PeekMessageW(&mut msg, null_mut(), WM_USER, WM_USER, PM_NOREMOVE);
            let _ = tx.send(Some(GetCurrentThreadId()));
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                if msg.message == WM_HOTKEY {
                    pressed();
                }
            }
            UnregisterHotKey(null_mut(), 1);
        });
        match rx.recv() {
            Ok(Some(thread_id)) => Ok(Hotkey { thread_id, thread: Some(thread) }),
            _ => Err(format!("Couldn't use {combo} (another app may be using it)")),
        }
    }
}

impl Drop for Hotkey {
    /// Unregisters it, and waits so the same combo can be registered again straight away.
    fn drop(&mut self) {
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Reads a combo like "Ctrl+Shift+S" into RegisterHotKey's modifiers and key code. Takes Ctrl, Alt,
/// Shift and Win (at least one, so a bare key doesn't stop working in the game), then one key:
/// a letter, a digit or F1 to F24. Case and spaces don't matter.
pub fn parse_hotkey(combo: &str) -> Result<(u32, u32), String> {
    let invalid = || format!("\"{combo}\" isn't a hotkey");
    let mut parts: Vec<String> = combo.split('+').map(|p| p.trim().to_ascii_uppercase()).collect();
    let key = parts.pop().ok_or_else(invalid)?;
    let mut mods = 0;
    for part in parts {
        let m = match part.as_str() {
            "CTRL" | "CONTROL" => MOD_CONTROL,
            "ALT" => MOD_ALT,
            "SHIFT" => MOD_SHIFT,
            "WIN" => MOD_WIN,
            _ => return Err(invalid()),
        };
        if mods & m != 0 {
            return Err(invalid());
        }
        mods |= m;
    }
    if mods == 0 {
        return Err(format!("A hotkey needs Ctrl, Alt, Shift or Win, not just {key}"));
    }
    let code = match key.as_bytes() {
        [c] if c.is_ascii_alphanumeric() => *c as u32, // A-Z and 0-9 are their own key codes
        _ => match key.strip_prefix('F').map(|n| (n, n.parse::<u32>())) {
            Some((n, Ok(f @ 1..=24))) if n == f.to_string() => VK_F1 as u32 + f - 1,
            _ => return Err(invalid()),
        },
    };
    Ok((mods, code))
}

// ---- Screen capture ----

/// Leaves a window out of every screen capture (this app's recordings, but also screenshots and
/// streams), so a replay window still open when you die again doesn't end up in the next clip.
/// Needs Windows 10 2004 or newer; older versions refuse it and the window stays capturable.
pub fn exclude_from_capture(hwnd: *mut c_void) {
    if unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) } == 0 {
        crate::log("Couldn't hide a window from screen capture; it may show up in clips");
    }
}

/// The game window (not the lobby client), as the handle ffmpeg's gfxcapture takes.
pub fn league_window() -> Option<usize> {
    let hwnd = unsafe { FindWindowW(null(), wide("League of Legends (TM) Client").as_ptr()) };
    (!hwnd.is_null()).then_some(hwnd as usize)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hotkeys() {
        assert_eq!(parse_hotkey("Ctrl+Shift+S"), Ok((MOD_CONTROL | MOD_SHIFT, 'S' as u32)));
        assert_eq!(parse_hotkey(" alt + 5 "), Ok((MOD_ALT, '5' as u32)));
        assert_eq!(parse_hotkey("Win+Ctrl+F12"), Ok((MOD_WIN | MOD_CONTROL, VK_F1 as u32 + 11)));
        let bad = ["", "S", "F5", "Ctrl+", "Ctrl+Ctrl+S", "Ctrl+Meta+S", "Ctrl+F0", "Ctrl+F25", "Ctrl+F05", "Ctrl+Space"];
        for bad in bad {
            assert!(parse_hotkey(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn hotkeys_are_released_when_dropped() {
        let combo = "Ctrl+Alt+Shift+Win+F24"; // unlikely to belong to anything else
        let key = Hotkey::register(combo, || {}).unwrap();
        assert!(Hotkey::register(combo, || {}).is_err(), "already ours");
        drop(key);
        assert!(Hotkey::register(combo, || {}).is_ok(), "free again straight away");
    }
}
