//! The floating replay window (ui/player.html).

use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::{inputs, mp4, settings, win};

const LABEL: &str = "player";

/// A yes or no as the page's query string carries it: "1" or "0" (ui/player.html reads them).
fn yes_no(on: bool) -> &'static str {
    if on {
        "1"
    } else {
        "0"
    }
}

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
        .append_pair("sound", yes_no(mp4::has_sound(clip)))
        .append_pair("recordSound", yes_no(s.record_sound));
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

/// The keys and clicks saved beside `clip` (its .inputs.json, see inputs.rs), if there are any.
#[tauri::command]
pub fn player_inputs(clip: PathBuf) -> Option<String> {
    std::fs::read_to_string(inputs::sidecar(&clip)).ok()
}

/// Matches the window's height to the video's aspect ratio.
#[tauri::command]
pub fn player_fit(window: WebviewWindow, aspect: f64) -> Result<(), String> {
    let size = window.inner_size().map_err(|e| e.to_string())?;
    let height = (size.width as f64 / aspect).round() as u32;
    window.set_size(PhysicalSize::new(size.width, height)).map_err(|e| e.to_string())
}
