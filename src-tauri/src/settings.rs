//! User settings, stored as JSON in the app's config folder.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Seconds before the death to keep.
    pub before: f64,
    /// Seconds after the death to keep (so the death itself is in the clip).
    pub after: f64,
    pub fps: u32,
    /// Recording height in pixels; 0 records at the monitor's native size.
    pub height: u32,
    pub monitor: u32,
    /// "ddagrab" (fast, default) or "gdigrab" (fallback when replays are black).
    pub capture: String,
    /// "auto" or a specific ffmpeg encoder such as "h264_nvenc".
    pub encoder: String,
    /// Pop up the replay window after each death.
    pub auto_play: bool,
    /// Playback speed the replay window starts at.
    pub speed: f64,
    /// Replay window size and position, in Windows' scaled pixels. Updated when you move it.
    pub window_width: f64,
    pub window_x: f64,
    pub window_y: f64,
    /// How many clips to keep; 0 keeps them all.
    pub keep: usize,
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
            speed: 1.0,
            window_width: 480.0,
            window_x: 20.0,
            window_y: 60.0,
            keep: 100,
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
        s.clips_dir = videos.join("LoL Death Replay");
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
