//! The floating replay window (ui/player.html).

use std::path::Path;

use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::settings;

const LABEL: &str = "player";

pub fn show(app: &AppHandle, clip: &Path, title: &str, death_from_end: f64) {
    close(app);
    let s = settings::current(app);
    if let Err(e) = app.asset_protocol_scope().allow_file(clip) {
        crate::log(&format!("Couldn't allow the clip for playback: {e}"));
        return;
    }
    let mut url = tauri::Url::parse("http://x/").unwrap();
    url.query_pairs_mut()
        .append_pair("clip", &clip.to_string_lossy())
        .append_pair("title", title)
        .append_pair("speed", &s.speed.to_string())
        .append_pair("death", &death_from_end.to_string());
    let page = format!("player.html?{}", url.query().unwrap_or_default());

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
    if let Err(e) = built {
        crate::log(&format!("Couldn't open the replay window: {e}"));
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

/// Matches the window's height to the video's aspect ratio.
#[tauri::command]
pub fn player_fit(window: WebviewWindow, aspect: f64) -> Result<(), String> {
    let size = window.inner_size().map_err(|e| e.to_string())?;
    let height = (size.width as f64 / aspect).round() as u32;
    window.set_size(PhysicalSize::new(size.width, height)).map_err(|e| e.to_string())
}
