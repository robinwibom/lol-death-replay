//! User settings, stored as JSON in the app's config folder.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Seconds before the death to keep.
    pub before: f64,
    /// Seconds after the death to keep (so the death itself is in the clip).
    pub after: f64,
    pub fps: u32,
    /// Recording height in pixels; 0 records at the monitor's native size.
    pub height: u32,
    /// Not in the settings window: the main display is recorded unless settings.json says otherwise.
    pub monitor: u32,
    /// "ddagrab" (fast, default) or "gdigrab". Not in the settings window. Fast records just the
    /// game window (gfxcapture) when it can, else the whole monitor (ddagrab); the app moves down
    /// that list, then to gdigrab, by itself for the rest of a game when one fails.
    pub capture: String,
    /// "auto" or a specific ffmpeg encoder such as "h264_nvenc".
    pub encoder: String,
    /// Pop up the replay window after each death.
    pub auto_play: bool,
    /// Keep each death's clip in `clips_dir`. When off, the replay uses a temporary file instead.
    pub save_clips: bool,
    /// Replay window size and position, in Windows' scaled pixels. Updated when you move it.
    pub window_width: f64,
    pub window_x: f64,
    pub window_y: f64,
    /// The clips folder is kept under this size by deleting the oldest clips.
    pub max_folder_gb: f64,
    pub clips_dir: PathBuf,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            before: 15.0,
            after: 2.0,
            fps: 30,
            height: 1080,
            monitor: 0,
            capture: "ddagrab".into(),
            encoder: "auto".into(),
            auto_play: true,
            save_clips: true,
            window_width: 480.0,
            window_x: 20.0,
            window_y: 60.0,
            max_folder_gb: 10.0,
            clips_dir: PathBuf::new(),
        }
    }
}

pub struct SettingsState(pub Mutex<Settings>);

fn file(app: &AppHandle) -> PathBuf {
    app.path().app_config_dir().expect("no config dir").join("settings.json")
}

/// Loads saved settings. Returns true as the second value on first run (nothing saved yet).
pub fn load(app: &AppHandle) -> (Settings, bool) {
    let saved = std::fs::read_to_string(file(app)).ok();
    let mut s: Settings = saved
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_default();
    if s.clips_dir.as_os_str().is_empty() {
        let videos = app.path().video_dir().unwrap_or_else(|_| std::env::temp_dir());
        s.clips_dir = videos.join("LoLDeathReplay");
    }
    (s, saved.is_none())
}

pub fn save(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let path = file(app);
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())
}

pub fn current(app: &AppHandle) -> Settings {
    app.state::<SettingsState>().0.lock().unwrap().clone()
}

/// Changes the in-memory settings and saves them.
pub fn update(app: &AppHandle, change: impl FnOnce(&mut Settings)) {
    let state = app.state::<SettingsState>();
    let mut s = state.0.lock().unwrap();
    change(&mut s);
    if let Err(e) = save(app, &s) {
        crate::log(&format!("Couldn't save settings: {e}"));
    }
}
