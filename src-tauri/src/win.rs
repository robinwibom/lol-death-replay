//! Small Windows helpers, each standing in for a dependency that did much more than this app needs.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, FILETIME, SYSTEMTIME, WIN32_ERROR};
use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, VK_CONTROL, VK_F1,
    VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK,
    RID_INPUT, RIM_TYPEKEYBOARD,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowW, GetMessageW, GetWindowThreadProcessId, RegisterClassW,
    SetWindowDisplayAffinity, MSG, RI_KEY_BREAK, RI_KEY_E0, WDA_EXCLUDEFROMCAPTURE, WM_INPUT, WNDCLASSW,
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

/// A global hotkey, active until dropped.
///
/// Keys are read with Raw Input in the background, not with `RegisterHotKey`: League tells Windows
/// to skip registered hotkeys while the game is in front (they work everywhere else), but Raw Input
/// still sees every key. Nothing is hooked or held back, so the key also reaches the app in front.
pub struct Hotkey {
    id: u64,
}

type Pressed = Arc<dyn Fn() + Send + Sync>;

struct Binding {
    id: u64,
    mods: u32,
    key: u32,
    pressed: Pressed,
}

static BINDINGS: Mutex<Vec<Binding>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl Hotkey {
    /// Calls `pressed` whenever `combo` (e.g. "Ctrl+Shift+S", see `parse_hotkey`) is pressed, in any app.
    pub fn register(combo: &str, pressed: impl Fn() + Send + Sync + 'static) -> Result<Hotkey, String> {
        let (mods, key) = parse_hotkey(combo)?;
        start_listener()?;
        let mut bindings = BINDINGS.lock().unwrap();
        if bindings.iter().any(|b| b.mods == mods && b.key == key) {
            return Err(format!("{combo} is already in use"));
        }
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        bindings.push(Binding { id, mods, key, pressed: Arc::new(pressed) });
        Ok(Hotkey { id })
    }
}

impl Drop for Hotkey {
    fn drop(&mut self) {
        BINDINGS.lock().unwrap().retain(|b| b.id != self.id);
    }
}

/// Whether another app has `combo` as its hotkey. Then Windows hands the key to that app and we'd
/// never see it, so it can't be ours either. Tries registering it for a moment.
pub fn hotkey_is_free(combo: &str) -> Result<(), String> {
    let (mods, key) = parse_hotkey(combo)?;
    unsafe {
        if RegisterHotKey(null_mut(), 0xB00, mods | MOD_NOREPEAT, key) == 0 {
            return Err(format!("Couldn't use {combo} (another app may be using it)"));
        }
        UnregisterHotKey(null_mut(), 0xB00);
    }
    Ok(())
}

/// Starts the one thread that reads the keyboard (a process gets one Raw Input target per device
/// kind). Must run after Tauri's event loop exists, so ours is the keyboard target that stays.
fn start_listener() -> Result<(), String> {
    static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
    STARTED
        .get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || listen(tx));
            rx.recv().unwrap_or_else(|_| Err("The keyboard listener stopped".into()))
        })
        .clone()
}

fn listen(ready: std::sync::mpsc::Sender<Result<(), String>>) {
    unsafe {
        let class = wide("LoLDeathReplayKeys");
        let wc = WNDCLASSW { lpfnWndProc: Some(DefWindowProcW), lpszClassName: class.as_ptr(), ..std::mem::zeroed() };
        RegisterClassW(&wc);
        // A hidden window to receive the input; it's never shown.
        let hwnd = CreateWindowExW(0, class.as_ptr(), null(), 0, 0, 0, 0, 0, null_mut(), null_mut(), null_mut(), null());
        let device = RAWINPUTDEVICE {
            usUsagePage: 0x01, // generic desktop
            usUsage: 0x06,     // keyboard
            dwFlags: RIDEV_INPUTSINK, // also while another app is in front
            hwndTarget: hwnd,
        };
        if hwnd.is_null() || RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32) == 0 {
            let _ = ready.send(Err("Couldn't start listening for hotkeys".into()));
            return;
        }
        let _ = ready.send(Ok(()));
        let mut down = [false; 256];
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if msg.message == WM_INPUT {
                if let Some((vk, pressed)) = read_key(msg.lParam) {
                    on_key(&mut down, vk, pressed);
                }
            }
            DispatchMessageW(&msg); // WM_INPUT needs DefWindowProc to clean up
        }
    }
}

/// The key code of a WM_INPUT keyboard event, and whether it went down.
unsafe fn read_key(lparam: isize) -> Option<(usize, bool)> {
    let mut input: RAWINPUT = std::mem::zeroed();
    let mut size = size_of::<RAWINPUT>() as u32;
    let header = size_of::<RAWINPUTHEADER>() as u32;
    if GetRawInputData(lparam as HRAWINPUT, RID_INPUT, (&raw mut input).cast(), &mut size, header) == u32::MAX
        || input.header.dwType != RIM_TYPEKEYBOARD
    {
        return None;
    }
    let kb = input.data.keyboard;
    // 0xFF: part of an escape sequence. E0 + 2A/36: a "fake shift" Windows adds around some keys.
    let fake_shift = kb.Flags as u32 & RI_KEY_E0 != 0 && (kb.MakeCode == 0x2A || kb.MakeCode == 0x36);
    if kb.VKey == 0xFF || fake_shift {
        return None;
    }
    Some((kb.VKey as usize, kb.Flags as u32 & RI_KEY_BREAK == 0))
}

/// Tracks which keys are down, and runs a binding when its key goes down with exactly its modifiers.
fn on_key(down: &mut [bool; 256], vk: usize, pressed: bool) {
    let repeat = pressed && down[vk];
    down[vk] = pressed;
    if !pressed || repeat {
        return;
    }
    let held = |vks: &[u16]| vks.iter().any(|&k| down[k as usize]);
    let mut mods = 0;
    for (vks, m) in [
        (&[VK_CONTROL, VK_LCONTROL, VK_RCONTROL][..], MOD_CONTROL),
        (&[VK_MENU, VK_LMENU, VK_RMENU][..], MOD_ALT),
        (&[VK_SHIFT, VK_LSHIFT, VK_RSHIFT][..], MOD_SHIFT),
        (&[VK_LWIN, VK_RWIN][..], MOD_WIN),
    ] {
        if held(vks) {
            mods |= m;
        }
    }
    // Run outside the lock, so a binding may add or drop hotkeys.
    let matching: Vec<Pressed> = BINDINGS
        .lock()
        .unwrap()
        .iter()
        .filter(|b| b.key as usize == vk && b.mods == mods)
        .map(|b| b.pressed.clone())
        .collect();
    for pressed in matching {
        pressed();
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

/// The game's process id, found by its window.
pub fn league_process() -> Option<u32> {
    let hwnd = league_window()?;
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd as _, &mut pid) };
    (pid != 0).then_some(pid)
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

    /// Presses Ctrl+Alt+Shift+Win+`vk` with synthetic input, which global hotkeys react to.
    fn press(vk: u16) {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_CONTROL, VK_LWIN, VK_MENU,
            VK_SHIFT,
        };
        let key = |vk: u16, up: bool| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: if up { KEYEVENTF_KEYUP } else { 0 }, time: 0, dwExtraInfo: 0 },
            },
        };
        let mods = [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN];
        let mut inputs: Vec<INPUT> = mods.iter().map(|&m| key(m, false)).collect();
        inputs.push(key(vk, false));
        inputs.push(key(vk, true));
        inputs.extend(mods.iter().rev().map(|&m| key(m, true)));
        unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
        std::thread::sleep(std::time::Duration::from_millis(300));
    }

    #[test]
    fn hotkeys_fire_and_dropping_one_leaves_the_other() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        let (a_hits, b_hits) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let (a2, b2) = (a_hits.clone(), b_hits.clone());
        let a = Hotkey::register("Ctrl+Alt+Shift+Win+F23", move || { a2.fetch_add(1, Ordering::SeqCst); }).unwrap();
        let b = Hotkey::register("Ctrl+Alt+Shift+Win+F22", move || { b2.fetch_add(1, Ordering::SeqCst); }).unwrap();
        press(VK_F1 + 22); // F23
        press(VK_F1 + 21); // F22
        assert_eq!((a_hits.load(Ordering::SeqCst), b_hits.load(Ordering::SeqCst)), (1, 1), "both fire");
        drop(a);
        press(VK_F1 + 21);
        assert_eq!(b_hits.load(Ordering::SeqCst), 2, "the other still fires after one is dropped");
        drop(b);
    }
}
