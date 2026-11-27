//! Keys and clicks, for the replay window's inputs overlay. Off by default ("Record keys and clicks").
//!
//! They're read passively with Raw Input by the background listener that also serves the hotkeys
//! (win.rs): Windows hands the app a copy of each key and mouse button, nothing is hooked, injected
//! or held back, so the game gets its input exactly as before. They're kept for as long as the
//! video, in memory, and saved beside a clip as `<clip>.inputs.json` (see `Sidecar`).
//!
//! Privacy: only input while the game window is in front is kept, and only clicks on the game
//! itself (not on the replay window above it). Chat is never kept: Enter opens it and the next
//! Enter (or Escape) closes it, and no key in between is stored, only that chat was open.
//!
//! Sync: inputs are stamped with `Instant::now()` (QueryPerformanceCounter) the moment the
//! listener gets them, a few microseconds after Windows does. The recorder works out on the same
//! clock when each video frame was captured (`recorder::Ring::capture_origin`: of all frames kept,
//! arrival time minus timestamp, smallest, so within the fastest frame's few milliseconds of
//! encoding and piping), and a clip's .mp4 timeline starts at its first frame (ffmpeg shifts it to
//! 0 when copying). So an input's time in the clip is its instant minus that first frame's capture
//! instant, the same way the clip's sound is lined up. What's left is under 50 ms: the frames'
//! few ms of latency (which show inputs that much early), and that a frame shows the screen as the
//! game last drew it, up to a frame before it was captured.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{ClientToScreen, GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClientRect, GetCursorPos, GetForegroundWindow, GetSystemMetrics, WindowFromPoint, GA_ROOT,
    RI_MOUSE_BUTTON_4_DOWN, RI_MOUSE_BUTTON_5_DOWN, RI_MOUSE_LEFT_BUTTON_DOWN, RI_MOUSE_MIDDLE_BUTTON_DOWN,
    RI_MOUSE_RIGHT_BUTTON_DOWN, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

const VK_RETURN: u16 = 0x0D;
const VK_ESCAPE: u16 = 0x1B;

/// The sidecar file a clip's inputs are saved in: "03m12s_by-Zed.mp4" -> "03m12s_by-Zed.inputs.json".
pub fn sidecar(clip: &Path) -> PathBuf {
    clip.with_extension("inputs.json")
}

// ---- What's kept ----

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl Button {
    fn name(self) -> &'static str {
        match self {
            Button::Left => "left",
            Button::Right => "right",
            Button::Middle => "middle",
            Button::Back => "mouse4",
            Button::Forward => "mouse5",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Input {
    /// A key going down or up, by its Windows key code.
    Key { vk: u16, down: bool },
    /// A mouse button going down, where it was in the recorded area (0 to 1 across and down).
    Click { button: Button, x: f64, y: f64 },
    /// Chat opened (true) or closed (false). Nothing typed in between is kept.
    Chat(bool),
}

/// Keeps chat out of what's recorded. League opens chat with Enter and sends (closes) it with the
/// next Enter; Escape closes it without sending. While it's open no key is kept at all, not just
/// letters: digits, spaces and punctuation are part of a message too.
#[derive(Default)]
struct ChatFilter {
    chat: bool,
    /// Keys physically down, as far as this has seen: a key that's down again is the key repeating.
    physical: Vec<u16>,
    /// Keys reported down and not yet up, so each down is matched by an up.
    shown: Vec<u16>,
}

impl ChatFilter {
    /// What a key going down or up in the game adds to the recording.
    fn key(&mut self, vk: u16, down: bool, out: &mut Vec<Input>) {
        let was_down = self.physical.contains(&vk);
        if down && was_down {
            return; // held: Windows repeats it
        }
        if down {
            self.physical.push(vk);
        } else {
            self.physical.retain(|&k| k != vk);
        }
        if down && vk == VK_RETURN {
            self.chat = !self.chat;
            if self.chat {
                self.release(out); // nothing stays lit while typing
            }
            out.push(Input::Chat(self.chat));
        } else if down && vk == VK_ESCAPE && self.chat {
            self.chat = false;
            out.push(Input::Chat(false));
        } else if self.chat {
            // Typing: nothing kept.
        } else if down {
            self.shown.push(vk);
            out.push(Input::Key { vk, down: true });
        } else if self.shown.contains(&vk) {
            self.shown.retain(|&k| k != vk);
            out.push(Input::Key { vk, down: false });
        }
    }

    /// The game went to the background: keys let go there are never seen, so they're let go now.
    /// Chat is taken as closed, the state the game is in nearly always when you come back to it.
    fn focus_lost(&mut self, out: &mut Vec<Input>) {
        self.release(out);
        self.physical.clear();
        if self.chat {
            self.chat = false;
            out.push(Input::Chat(false));
        }
    }

    fn release(&mut self, out: &mut Vec<Input>) {
        out.extend(self.shown.drain(..).map(|vk| Input::Key { vk, down: false }));
    }
}

/// The last `keep` of inputs, in the order they happened.
struct Buffer {
    events: VecDeque<(Instant, Input)>,
    keep: Duration,
}

impl Buffer {
    fn new(keep: Duration) -> Self {
        Self { events: VecDeque::new(), keep }
    }

    fn push(&mut self, at: Instant, input: Input) {
        self.events.push_back((at, input));
        while self.events.front().is_some_and(|(t, _)| at.saturating_duration_since(*t) > self.keep) {
            self.events.pop_front();
        }
    }

    /// The inputs of a clip whose first frame was captured at `start`, up to `end`, timed from
    /// that frame. Keys already down at the start (and chat already open) are put at 0.
    fn clip(&self, start: Instant, end: Instant) -> Vec<ClipEvent> {
        let mut held: Vec<u16> = Vec::new();
        let mut chat = false;
        let mut out = Vec::new();
        let mut started = false;
        for (at, input) in &self.events {
            if *at > end {
                break;
            }
            if *at < start {
                match input {
                    Input::Key { vk, down: true } => held.push(*vk),
                    Input::Key { vk, down: false } => held.retain(|k| k != vk),
                    Input::Chat(open) => chat = *open,
                    Input::Click { .. } => {}
                }
                continue;
            }
            if !started {
                out.extend(at_start(&held, chat));
                started = true;
            }
            out.push(ClipEvent::new(at.duration_since(start).as_secs_f64(), input));
        }
        if !started {
            out.extend(at_start(&held, chat));
        }
        out
    }
}

fn at_start(held: &[u16], chat: bool) -> Vec<ClipEvent> {
    let mut out: Vec<ClipEvent> = held.iter().map(|&vk| ClipEvent::new(0.0, &Input::Key { vk, down: true })).collect();
    if chat {
        out.push(ClipEvent::new(0.0, &Input::Chat(true)));
    }
    out
}

// ---- The sidecar file ----

/// `<clip>.inputs.json`: `{"version": 1, "events": [...]}`, where each event is one of
///
/// - `{"t": 1.234, "key": "Q", "down": true}`: a key going down (or up, `"down": false`). Keys are
///   named as on the keyboard ("Q", "4", "Space", "Shift", "F1", "Num1"), not as abilities.
/// - `{"t": 2.5, "button": "right", "x": 0.4123, "y": 0.6634}`: a mouse button going down
///   ("left", "right", "middle", "mouse4", "mouse5"), at x and y from 0 to 1 across the video.
/// - `{"t": 3.0, "chat": true}`: chat opened (`false`: closed). No key in between is kept.
///
/// `t` is seconds from the clip's first frame, which is where the video's own timeline starts.
/// Events are in time order; keys held and chat open when the clip starts are at 0.
#[derive(Serialize)]
struct Sidecar {
    version: u32,
    events: Vec<ClipEvent>,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(untagged)]
enum ClipEvent {
    Key { t: f64, key: String, down: bool },
    Click { t: f64, button: &'static str, x: f64, y: f64 },
    Chat { t: f64, chat: bool },
}

/// Rounded to `places` decimals, so the file doesn't spell out float noise.
fn round(v: f64, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    (v * scale).round() / scale
}

impl ClipEvent {
    fn new(t: f64, input: &Input) -> Self {
        let t = round(t, 3);
        match input {
            Input::Key { vk, down } => ClipEvent::Key { t, key: key_name(*vk), down: *down },
            Input::Click { button, x, y } => ClipEvent::Click { t, button: button.name(), x: round(*x, 4), y: round(*y, 4) },
            Input::Chat(open) => ClipEvent::Chat { t, chat: *open },
        }
    }
}

/// A key's name as printed on it (US layout for punctuation), by its Windows key code.
fn key_name(vk: u16) -> String {
    let named = match vk {
        0x30..=0x39 | 0x41..=0x5A => return char::from(vk as u8).to_string(),
        0x60..=0x69 => return format!("Num{}", vk - 0x60),
        0x70..=0x87 => return format!("F{}", vk - 0x6F),
        0x08 => "Backspace",
        0x09 => "Tab",
        0x0D => "Enter",
        0x10 | 0xA0 | 0xA1 => "Shift",
        0x11 | 0xA2 | 0xA3 => "Ctrl",
        0x12 | 0xA4 | 0xA5 => "Alt",
        0x13 => "Pause",
        0x14 => "CapsLock",
        0x1B => "Esc",
        0x20 => "Space",
        0x21 => "PageUp",
        0x22 => "PageDown",
        0x23 => "End",
        0x24 => "Home",
        0x25 => "Left",
        0x26 => "Up",
        0x27 => "Right",
        0x28 => "Down",
        0x2D => "Insert",
        0x2E => "Delete",
        0x5B | 0x5C => "Win",
        0x6A => "Num*",
        0x6B => "Num+",
        0x6D => "Num-",
        0x6E => "Num.",
        0x6F => "Num/",
        0xBA => ";",
        0xBB => "=",
        0xBC => ",",
        0xBD => "-",
        0xBE => ".",
        0xBF => "/",
        0xC0 => "`",
        0xDB => "[",
        0xDC => "\\",
        0xDD => "]",
        0xDE => "'",
        _ => return format!("VK{vk:02X}"),
    };
    named.to_string()
}

// ---- Recording ----

/// The part of the screen the video shows, which clicks are placed in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Area {
    /// The game window's inside (gfxcapture records it without its border).
    GameWindow,
    /// The monitor the game is on (ddagrab).
    GameMonitor,
    /// Every monitor (gdigrab records the whole desktop).
    Desktop,
}

impl Area {
    /// The area a capture method (see recorder.rs) records.
    pub fn of_capture(capture: &str) -> Area {
        match capture {
            "gfxcapture" => Area::GameWindow,
            "gdigrab" => Area::Desktop,
            _ => Area::GameMonitor,
        }
    }

    /// Where it is on the screen, in physical pixels (the app is DPI aware, like the cursor's position).
    fn rect(self, game: HWND) -> Option<RECT> {
        unsafe {
            match self {
                Area::GameWindow => {
                    let mut r: RECT = std::mem::zeroed();
                    let mut corner = POINT { x: 0, y: 0 };
                    (GetClientRect(game, &mut r) != 0 && ClientToScreen(game, &mut corner) != 0).then(|| RECT {
                        left: corner.x,
                        top: corner.y,
                        right: corner.x + r.right,
                        bottom: corner.y + r.bottom,
                    })
                }
                Area::GameMonitor => {
                    let mut info: MONITORINFO = std::mem::zeroed();
                    info.cbSize = size_of::<MONITORINFO>() as u32;
                    let monitor = MonitorFromWindow(game, MONITOR_DEFAULTTONEAREST);
                    (GetMonitorInfoW(monitor, &mut info) != 0).then_some(info.rcMonitor)
                }
                Area::Desktop => {
                    let (x, y) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
                    let (w, h) = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
                    Some(RECT { left: x, top: y, right: x + w, bottom: y + h })
                }
            }
        }
    }
}

/// `point` as 0 to 1 across and down `area`; None outside it.
fn place(point: POINT, area: RECT) -> Option<(f64, f64)> {
    let (w, h) = (area.right - area.left, area.bottom - area.top);
    if w <= 0 || h <= 0 {
        return None;
    }
    let x = f64::from(point.x - area.left) / f64::from(w);
    let y = f64::from(point.y - area.top) / f64::from(h);
    ((0.0..1.0).contains(&x) && (0.0..1.0).contains(&y)).then_some((x, y))
}

/// How often mouse movement checks whether the game is still in front (movement itself isn't kept).
const FOCUS_CHECK: Duration = Duration::from_millis(100);
/// How often the game window is looked up again while something else is in front.
const GAME_LOOKUP: Duration = Duration::from_secs(1);

/// One recording's inputs, and what decides which are kept.
struct Tracker {
    buffer: Buffer,
    filter: ChatFilter,
    area: Area,
    game: Option<usize>,
    looked_up: Option<Instant>,
    focused: bool,
    checked: Option<Instant>,
}

impl Tracker {
    fn new(keep: Duration, area: Area) -> Self {
        Self {
            buffer: Buffer::new(keep),
            filter: ChatFilter::default(),
            area,
            game: None,
            looked_up: None,
            focused: false,
            checked: None,
        }
    }

    /// Notes whether the game is in front; going to the background lets go of keys and chat.
    fn set_focused(&mut self, focused: bool, now: Instant) {
        if self.focused && !focused {
            let mut out = Vec::new();
            self.filter.focus_lost(&mut out);
            for input in out {
                self.buffer.push(now, input);
            }
        }
        self.focused = focused;
        self.checked = Some(now);
    }

    fn key(&mut self, vk: u16, down: bool, focused: bool, now: Instant) {
        self.set_focused(focused, now);
        if !focused {
            return;
        }
        let mut out = Vec::new();
        self.filter.key(vk, down, &mut out);
        for input in out {
            self.buffer.push(now, input);
        }
    }

    fn click(&mut self, button: Button, at: Option<(f64, f64)>, focused: bool, now: Instant) {
        self.set_focused(focused, now);
        if let (true, Some((x, y))) = (focused, at) {
            self.buffer.push(now, Input::Click { button, x, y });
        }
    }

    /// Whether the game window is the one in front.
    fn game_in_front(&mut self, now: Instant) -> bool {
        let front = unsafe { GetForegroundWindow() } as usize;
        if self.game != Some(front) && self.looked_up.is_none_or(|t| now.duration_since(t) >= GAME_LOOKUP) {
            self.game = crate::win::league_window();
            self.looked_up = Some(now);
        }
        front != 0 && self.game == Some(front)
    }

    /// Where the cursor is in the recorded area, if it's over the game itself: not over a window
    /// on top of it, like the replay window (which never takes focus, so the game stays in front).
    fn cursor(&self) -> Option<(f64, f64)> {
        let game = self.game? as HWND;
        unsafe {
            let mut p = POINT { x: 0, y: 0 };
            if GetCursorPos(&mut p) == 0 || GetAncestor(WindowFromPoint(p), GA_ROOT) != game {
                return None;
            }
            place(p, self.area.rect(game)?)
        }
    }
}

/// The recording's tracker, which the listener thread feeds. One at a time.
static ACTIVE: Mutex<Option<Arc<Mutex<Tracker>>>> = Mutex::new(None);

fn active() -> Option<Arc<Mutex<Tracker>>> {
    ACTIVE.lock().unwrap().clone()
}

/// A key from the listener (win.rs), at `now`, when it got it.
pub fn raw_key(vk: u16, down: bool, now: Instant) {
    let Some(tracker) = active() else { return };
    let mut t = tracker.lock().unwrap();
    let focused = t.game_in_front(now);
    t.key(vk, down, focused, now);
}

/// A mouse event from the listener (win.rs): RAWMOUSE's button flags. Movement alone isn't kept.
pub fn raw_mouse(buttons: u32, now: Instant) {
    let Some(tracker) = active() else { return };
    let mut t = tracker.lock().unwrap();
    const BUTTONS: [(u32, Button); 5] = [
        (RI_MOUSE_LEFT_BUTTON_DOWN, Button::Left),
        (RI_MOUSE_RIGHT_BUTTON_DOWN, Button::Right),
        (RI_MOUSE_MIDDLE_BUTTON_DOWN, Button::Middle),
        (RI_MOUSE_BUTTON_4_DOWN, Button::Back),
        (RI_MOUSE_BUTTON_5_DOWN, Button::Forward),
    ];
    let pressed: Vec<Button> = BUTTONS.iter().filter(|(flag, _)| buttons & flag != 0).map(|(_, b)| *b).collect();
    if pressed.is_empty() {
        // Movement comes hundreds of times a second: only now and then see if the game's still in front.
        if t.checked.is_none_or(|c| now.duration_since(c) >= FOCUS_CHECK) {
            let focused = t.game_in_front(now);
            t.set_focused(focused, now);
        }
        return;
    }
    let focused = t.game_in_front(now);
    let at = if focused { t.cursor() } else { None };
    for button in pressed {
        t.click(button, at, focused, now);
    }
}

/// Inputs being recorded, from `start` until dropped.
pub struct Capture {
    tracker: Arc<Mutex<Tracker>>,
}

impl Capture {
    /// Keeps the last `keep` of inputs, with clicks placed in `area`.
    pub fn start(keep: Duration, area: Area) -> Self {
        let tracker = Arc::new(Mutex::new(Tracker::new(keep, area)));
        *ACTIVE.lock().unwrap() = Some(tracker.clone());
        crate::win::watch_mouse(true);
        Self { tracker }
    }

    /// Writes `clip`'s sidecar: the inputs from its first frame, captured at `start`, to `end`.
    fn save(&self, start: Instant, end: Instant, clip: &Path) -> Result<(), String> {
        let events = self.tracker.lock().unwrap().buffer.clip(start, end);
        let text = serde_json::to_string(&Sidecar { version: 1, events }).map_err(|e| e.to_string())?;
        std::fs::write(sidecar(clip), text).map_err(|e| e.to_string())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let mut active = ACTIVE.lock().unwrap();
        // A newer recording may have started already (an older one's last clip can still be saving).
        if active.as_ref().is_some_and(|a| Arc::ptr_eq(a, &self.tracker)) {
            *active = None;
            crate::win::watch_mouse(false);
        }
    }
}

/// Saves the inputs beside a clip just saved, whose first frame was captured at `first_frame` and
/// which ends at `end`. With no inputs (turned off) any sidecar left from an older clip of the same
/// name goes, so it isn't shown over this one. A failure is logged: the clip itself is fine.
pub fn save_beside(capture: Option<&Capture>, first_frame: Option<Instant>, end: Instant, clip: &Path) {
    let saved = match (capture, first_frame) {
        (Some(capture), Some(start)) => capture.save(start, end, clip),
        (Some(_), None) => Err("couldn't tell when the clip starts".into()),
        (None, _) => Ok(()),
    };
    if capture.is_none() || saved.is_err() {
        let _ = std::fs::remove_file(sidecar(clip));
    }
    if let Err(e) = saved {
        crate::log(&format!("Saved the clip without its keys and clicks: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q: u16 = b'Q' as u16;
    const A: u16 = b'A' as u16;

    fn keys(events: &[(u16, bool)]) -> (ChatFilter, Vec<Input>) {
        let mut f = ChatFilter::default();
        let mut out = Vec::new();
        for &(vk, down) in events {
            f.key(vk, down, &mut out);
        }
        (f, out)
    }
    fn key(vk: u16, down: bool) -> Input {
        Input::Key { vk, down }
    }

    #[test]
    fn keeps_keys_outside_chat_and_drops_repeats() {
        let (_, out) = keys(&[(Q, true), (Q, true), (Q, true), (Q, false), (A, true), (A, false)]);
        assert_eq!(out, [key(Q, true), key(Q, false), key(A, true), key(A, false)]);
    }

    #[test]
    fn nothing_typed_in_chat_is_kept() {
        let (f, out) = keys(&[
            (Q, true),
            (VK_RETURN, true), // opens chat while Q is held: Q is let go
            (VK_RETURN, true), // Enter repeating while held doesn't close it again
            (VK_RETURN, false),
            (Q, false),
            (b'H' as u16, true), (b'H' as u16, false), (b'1' as u16, true), (0x20, true), // "h1 "
            (VK_RETURN, true), // sent: chat closes
            (VK_RETURN, false),
            (A, true),
        ]);
        assert_eq!(out, [key(Q, true), key(Q, false), Input::Chat(true), Input::Chat(false), key(A, true)]);
        assert!(!f.chat);
    }

    #[test]
    fn escape_closes_chat_and_is_a_key_otherwise() {
        let (_, out) = keys(&[(VK_RETURN, true), (A, true), (VK_ESCAPE, true), (VK_ESCAPE, false), (A, false),
            (VK_ESCAPE, true), (VK_ESCAPE, false)]);
        // A went down in chat, so its up isn't kept either.
        assert_eq!(out, [Input::Chat(true), Input::Chat(false), key(VK_ESCAPE, true), key(VK_ESCAPE, false)]);
    }

    #[test]
    fn losing_focus_lets_go_and_closes_chat() {
        let (mut f, mut out) = keys(&[(Q, true), (A, true)]);
        f.focus_lost(&mut out);
        assert_eq!(out[2..], [key(Q, false), key(A, false)]);
        // Back in front: Q held all along is a fresh press, not a repeat.
        out.clear();
        f.key(Q, true, &mut out);
        f.key(VK_RETURN, true, &mut out);
        f.focus_lost(&mut out);
        assert_eq!(out, [key(Q, true), key(Q, false), Input::Chat(true), Input::Chat(false)]);
        // Chat taken as closed when back: keys count again.
        out.clear();
        f.key(A, true, &mut out);
        assert_eq!(out, [key(A, true)]);
    }

    #[test]
    fn buffer_forgets_what_is_older_than_the_video() {
        let t0 = Instant::now();
        let at = |s: f64| t0 + Duration::from_secs_f64(s);
        let mut b = Buffer::new(Duration::from_secs(5));
        for i in 0..10 {
            b.push(at(f64::from(i)), key(Q, i % 2 == 0));
        }
        // Newest at 9 s: 4 s and later are kept.
        assert_eq!(b.events.len(), 6);
        assert_eq!(b.events.front().unwrap().0, at(4.0));
    }

    #[test]
    fn clip_events_are_timed_from_the_first_frame() {
        let t0 = Instant::now();
        let at = |s: f64| t0 + Duration::from_secs_f64(s);
        let mut b = Buffer::new(Duration::from_secs(60));
        b.push(at(1.0), key(Q, true)); // held across the start
        b.push(at(1.5), key(A, true));
        b.push(at(1.6), key(A, false)); // over before it
        b.push(at(1.8), Input::Chat(true));
        b.push(at(1.9), Input::Chat(false));
        b.push(at(2.25), Input::Click { button: Button::Right, x: 0.123_456, y: 0.5 });
        b.push(at(2.5), key(Q, false));
        b.push(at(4.0), key(A, true)); // after the clip's end
        let events = b.clip(at(2.0), at(3.0));
        assert_eq!(events, [
            ClipEvent::Key { t: 0.0, key: "Q".into(), down: true },
            ClipEvent::Click { t: 0.25, button: "right", x: 0.1235, y: 0.5 },
            ClipEvent::Key { t: 0.5, key: "Q".into(), down: false },
        ]);
        let json = serde_json::to_string(&Sidecar { version: 1, events }).unwrap();
        assert_eq!(json, concat!(
            r#"{"version":1,"events":[{"t":0.0,"key":"Q","down":true},"#,
            r#"{"t":0.25,"button":"right","x":0.1235,"y":0.5},{"t":0.5,"key":"Q","down":false}]}"#
        ));
        // Chat open at the start shows from 0; nothing in the clip still gives what's held.
        b.push(at(5.0), Input::Chat(true));
        assert_eq!(b.clip(at(5.5), at(6.0)), [
            ClipEvent::Key { t: 0.0, key: "A".into(), down: true },
            ClipEvent::Chat { t: 0.0, chat: true },
        ]);
    }

    #[test]
    fn tracker_keeps_only_what_happens_in_the_game() {
        let t0 = Instant::now();
        let at = |s: f64| t0 + Duration::from_secs_f64(s);
        let mut t = Tracker::new(Duration::from_secs(60), Area::GameMonitor);
        t.key(Q, true, true, at(0.0));
        t.click(Button::Left, Some((0.5, 0.5)), true, at(0.1));
        t.click(Button::Left, None, true, at(0.2)); // over the replay window: not kept
        t.key(A, true, false, at(0.3)); // another app in front: Q let go there, A not kept
        t.click(Button::Right, Some((0.1, 0.1)), false, at(0.4));
        let kept: Vec<_> = t.buffer.events.iter().map(|(_, i)| i.clone()).collect();
        assert_eq!(kept, [key(Q, true), Input::Click { button: Button::Left, x: 0.5, y: 0.5 }, key(Q, false)]);
        assert_eq!(t.buffer.events[2].0, at(0.3));
    }

    #[test]
    fn names_keys_as_printed() {
        assert_eq!(key_name(Q), "Q");
        assert_eq!(key_name(b'4' as u16), "4");
        assert_eq!(key_name(0x20), "Space");
        assert_eq!(key_name(0x70), "F1");
        assert_eq!(key_name(0x87), "F24");
        assert_eq!(key_name(0x61), "Num1");
        assert_eq!(key_name(0xA0), "Shift");
        assert_eq!(key_name(0xFE), "VKFE");
    }

    #[test]
    fn places_clicks_in_the_recorded_area() {
        let area = RECT { left: 1920, top: 0, right: 3840, bottom: 1080 }; // a second monitor
        let (x, y) = place(POINT { x: 1920 + 960, y: 540 }, area).unwrap();
        assert!((x - 0.5).abs() < 1e-9 && (y - 0.5).abs() < 1e-9);
        assert!(place(POINT { x: 100, y: 100 }, area).is_none()); // on the first monitor
        assert!(place(POINT { x: 3840, y: 10 }, area).is_none()); // just past the edge
        assert!(place(POINT { x: 0, y: 0 }, RECT { left: 0, top: 0, right: 0, bottom: 0 }).is_none());
    }

    #[test]
    fn saves_inputs_beside_the_clip_and_never_stale_ones() {
        let _switch = crate::win::tests::MOUSE_SWITCH.lock().unwrap_or_else(|e| e.into_inner());
        let clip = std::env::temp_dir().join(format!("inputs_test_{}.mp4", std::process::id()));
        let t0 = Instant::now();
        let capture = Capture::start(Duration::from_secs(60), Area::GameMonitor);
        capture.tracker.lock().unwrap().buffer.push(t0 + Duration::from_millis(1500), key(Q, true));
        save_beside(Some(&capture), Some(t0 + Duration::from_secs(1)), t0 + Duration::from_secs(2), &clip);
        let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(sidecar(&clip)).unwrap()).unwrap();
        assert_eq!(saved, serde_json::json!({"version": 1, "events": [{"t": 0.5, "key": "Q", "down": true}]}));
        // No start time: nothing to line them up with, so no file rather than a wrong one.
        save_beside(Some(&capture), None, t0, &clip);
        assert!(!sidecar(&clip).exists());
        // Recording turned off: a clip saved over an older one of the same name loses its inputs.
        std::fs::write(sidecar(&clip), "{}").unwrap();
        drop(capture);
        assert!(ACTIVE.lock().unwrap().is_none());
        save_beside(None, Some(t0), t0, &clip);
        assert!(!sidecar(&clip).exists());
    }

    #[test]
    fn sidecar_sits_beside_the_clip() {
        let clip = Path::new(r"C:\clips\2026-11-24\1849_Caitlyn_SR\03m12s_by-Zed.mp4");
        assert_eq!(sidecar(clip), Path::new(r"C:\clips\2026-11-24\1849_Caitlyn_SR\03m12s_by-Zed.inputs.json"));
    }
}
