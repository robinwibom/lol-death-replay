//! The floating replay window (ui/player.html).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::{settings, win};

const LABEL: &str = "player";

pub fn show(app: &AppHandle, clip: &Path, title: &str, death_from_end: f64) {
    if let Err(e) = app.asset_protocol_scope().allow_file(clip) {
        crate::log(&format!("Couldn't allow the clip for playback: {e}"));
        return;
    }
    // Without sound, the page greys out its speaker: the clip has none (the setting was off when it
    // was recorded, or nothing was heard), and its tooltip says if the setting is off now.
    let s = settings::current(app);
    let mut url = tauri::Url::parse("http://x/").unwrap();
    url.query_pairs_mut()
        .append_pair("clip", &clip.to_string_lossy())
        .append_pair("title", title)
        .append_pair("death", &death_from_end.to_string())
        .append_pair("sound", if has_sound(clip) { "1" } else { "0" })
        .append_pair("recordSound", if s.record_sound { "1" } else { "0" });
    let query = url.query().unwrap_or_default().to_string();

    // Already open (died again while watching): load the new clip into it. Destroying it and
    // building a new one fails, since the old one's label is only freed a moment later.
    if let Some(window) = app.get_webview_window(LABEL) {
        let reused = window.url().and_then(|mut url| {
            url.set_path("/player.html");
            url.set_query(Some(&query));
            window.navigate(url)
        });
        match reused {
            Ok(()) => {
                let _ = window.set_title(title);
                return;
            }
            Err(e) => {
                crate::log(&format!("Couldn't load the clip into the open replay window: {e}"));
                close(app);
            }
        }
    }

    let page = format!("player.html?{query}");

    let built = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App(page.into()))
        .title(title)
        .inner_size(s.window_width, (s.window_width * 9.0 / 16.0).round())
        .min_inner_size(240.0, 135.0)
        .position(s.window_x, s.window_y)
        .decorations(false)
        .shadow(true)
        .resizable(false) // ui/player.html resizes it, keeping the video's shape
        .always_on_top(true)
        .skip_taskbar(true)
        // Never take keyboard focus from the game, even when clicked.
        .focused(false)
        .focusable(false)
        .build();
    match built {
        Ok(window) => {
            if let Ok(hwnd) = window.hwnd() {
                win::exclude_from_capture(hwnd.0); // so dying again with it open doesn't record it
            }
        }
        Err(e) => crate::log(&format!("Couldn't open the replay window: {e}")),
    }
}

/// Whether an .mp4 has a sound track: a handler box of type "soun" in its movie box. Walks the
/// top-level boxes (a clip's are ftyp, moov and mdat) and reads only the movie box.
pub fn has_sound(clip: &Path) -> bool {
    let Ok(mut f) = File::open(clip) else { return false };
    let mut head = [0u8; 8];
    while f.read_exact(&mut head).is_ok() {
        let (size, header) = match u32::from_be_bytes(head[..4].try_into().unwrap()) {
            0 => return false, // runs to the end of the file: the last box, and not moov
            1 => {
                let mut big = [0u8; 8];
                if f.read_exact(&mut big).is_err() {
                    return false;
                }
                (u64::from_be_bytes(big), 16)
            }
            size => (size as u64, 8),
        };
        let Some(body) = size.checked_sub(header) else { return false };
        if &head[4..] == b"moov" {
            if body > 64 << 20 {
                return false; // not a clip of ours
            }
            let mut moov = vec![0; body as usize];
            // hdlr's body: version and flags, a reserved word, then the handler type.
            return f.read_exact(&mut moov).is_ok()
                && moov.windows(16).any(|w| &w[..4] == b"hdlr" && &w[12..] == b"soun");
        }
        if f.seek(SeekFrom::Current(body as i64)).is_err() {
            return false;
        }
    }
    false
}

/// Closes the replay window, remembering where it was for next time.
pub fn close(app: &AppHandle) {
    let Some(window) = app.get_webview_window(LABEL) else { return };
    if let (Ok(pos), Ok(size), Ok(scale)) = (window.outer_position(), window.inner_size(), window.scale_factor()) {
        settings::update(app, |s| {
            s.window_x = (pos.x as f64 / scale).round();
            s.window_y = (pos.y as f64 / scale).round();
            s.window_width = (size.width as f64 / scale).round();
        });
    }
    let _ = window.destroy();
}

#[tauri::command]
pub fn player_close(app: AppHandle) {
    close(&app);
}

/// Resizes to `width` x `height` (scaled pixels), keeping the `anchor` corner (e.g. "SE") in place.
#[tauri::command]
pub fn player_resize(window: WebviewWindow, width: f64, height: f64, anchor: String) -> Result<(), String> {
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let old = window.inner_size().map_err(|e| e.to_string())?;
    let pos = window.outer_position().map_err(|e| e.to_string())?;
    let new = PhysicalSize::new((width * scale).round() as u32, (height * scale).round() as u32);
    let mut moved = pos;
    if anchor.contains('E') {
        moved.x += old.width as i32 - new.width as i32;
    }
    if anchor.contains('S') {
        moved.y += old.height as i32 - new.height as i32;
    }
    window.set_size(new).map_err(|e| e.to_string())?;
    if moved != pos {
        window.set_position(PhysicalPosition::new(moved.x, moved.y)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Matches the window's height to the video's aspect ratio.
#[tauri::command]
pub fn player_fit(window: WebviewWindow, aspect: f64) -> Result<(), String> {
    let size = window.inner_size().map_err(|e| e.to_string())?;
    let height = (size.width as f64 / aspect).round() as u32;
    window.set_size(PhysicalSize::new(size.width, height)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        [&(8 + body.len() as u32).to_be_bytes()[..], kind, body].concat()
    }

    fn handler(kind: &[u8; 4]) -> Vec<u8> {
        mp4_box(b"hdlr", &[&[0; 8][..], kind, &[0; 13]].concat())
    }

    #[test]
    fn finds_a_sound_track() {
        let dir = std::env::temp_dir();
        let clip = |name: &str, moov: Vec<u8>| {
            let path = dir.join(format!("has_sound_{}_{name}.mp4", std::process::id()));
            let file = [mp4_box(b"ftyp", b"isom"), mp4_box(b"moov", &moov), mp4_box(b"mdat", &[0; 100])].concat();
            std::fs::write(&path, file).unwrap();
            let found = has_sound(&path);
            std::fs::remove_file(&path).unwrap();
            found
        };
        let track = |kind| mp4_box(b"trak", &mp4_box(b"mdia", &handler(kind)));
        assert!(!clip("video", track(b"vide")));
        assert!(clip("both", [track(b"vide"), track(b"soun")].concat()));
        // "soun" elsewhere in the movie box isn't a sound track.
        assert!(!clip("name", [track(b"vide"), mp4_box(b"udta", b"hdlr....soun")].concat()));
        assert!(!has_sound(Path::new("no such clip.mp4")));
    }
}
