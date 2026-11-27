//! User settings, stored as JSON in the app's config folder.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// The most seconds before a death a clip can hold; also the settings window's limit for `before`.
pub const MAX_BEFORE: f64 = 60.0;
/// Seen before a fight's first kill, so the clip shows how the fight started.
const FIGHT_LEAD: f64 = 5.0;
/// The largest folder size limit, in GB; also the settings window's limit for `max_folder_gb`.
pub const MAX_FOLDER_GB: f64 = 20.0;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Seconds before the death to keep.
    pub before: f64,
    /// Seconds after the death to keep (so the death itself is in the clip).
    pub after: f64,
    /// When the death ends a fight, keep the whole fight: more than `before`, up to `MAX_BEFORE`.
    pub dynamic_clip: bool,
    pub fps: u32,
    /// Record the game's sound into clips.
    pub record_sound: bool,
    /// Record keys and clicks while the game is in front, for the replay's inputs overlay (inputs.rs).
    pub record_inputs: bool,
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
    /// During a game, saves the last `before` seconds as a clip. A combo like "Ctrl+Shift+S"
    /// (see `win::parse_hotkey`); empty turns it off.
    pub save_key: String,
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
            before: 12.0,
            after: 1.0,
            dynamic_clip: false,
            fps: 30,
            record_sound: true,
            record_inputs: false,
            height: 1080,
            monitor: 0,
            capture: "ddagrab".into(),
            encoder: "auto".into(),
            auto_play: true,
            save_clips: true,
            save_key: "Ctrl+Shift+S".into(),
            window_width: 480.0,
            window_x: 20.0,
            window_y: 60.0,
            max_folder_gb: 10.0,
            clips_dir: PathBuf::new(),
        }
    }
}

impl Settings {
    /// Seconds before a death to keep, when the death ended a fight that lasted `fight` seconds (0
    /// for none). With "Include the whole fight" on: the fight plus a lead-in, never less than
    /// `before` and never more than `MAX_BEFORE`. Off: always `before`.
    pub fn before_death(&self, fight: f64) -> f64 {
        if !self.dynamic_clip || fight <= 0.0 {
            return self.before;
        }
        (fight + FIGHT_LEAD).min(MAX_BEFORE).max(self.before)
    }

    /// The most `before_death` can ask for, so the recording knows how much to keep.
    pub fn max_before_death(&self) -> f64 {
        self.before_death(f64::INFINITY)
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
    s.max_folder_gb = s.max_folder_gb.min(MAX_FOLDER_GB); // older versions allowed up to 1000
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_fight_clips_cover_the_fight() {
        let s = Settings { before: 15.0, dynamic_clip: true, ..Default::default() };
        assert_eq!(s.before_death(0.0), 15.0); // no fight: the setting
        assert_eq!(s.before_death(5.0), 15.0); // a short fight fits in the usual clip
        assert_eq!(s.before_death(25.0), 30.0); // the fight plus the lead-in
        assert_eq!(s.before_death(90.0), MAX_BEFORE); // capped
        assert_eq!(s.max_before_death(), MAX_BEFORE);
        let long = Settings { before: 50.0, ..s.clone() };
        assert_eq!(long.before_death(25.0), 50.0); // never shorter than the setting
    }

    #[test]
    fn without_the_whole_fight_clips_keep_the_setting() {
        let s = Settings { before: 15.0, dynamic_clip: false, ..Default::default() };
        assert_eq!(s.before_death(25.0), 15.0);
        assert_eq!(s.max_before_death(), 15.0);
    }
}
