// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod audio_ring;
mod game;
mod inputs;
mod live_client;
mod mkv;
mod mp4;
mod player;
mod recorder;
mod settings;
mod toast;
mod updater;
mod watcher;
mod win;

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, Wry};

use settings::{Settings, SettingsState};

pub struct AppState {
    status: Mutex<String>,
    status_item: MenuItem<Wry>,
    replay_item: MenuItem<Wry>,
    pub last_clip: Mutex<Option<PathBuf>>,
    /// While a game records: its recorder and its clip folder (inside the clips folder), for the save hotkey.
    pub recording: Mutex<Option<(Arc<recorder::Recorder>, game::Current)>>,
    /// The save hotkey, registered only while a game records.
    pub save_key: Mutex<Option<win::Hotkey>>,
}

/// Closes the replay from inside the game. Always registered.
const CLOSE_KEY: &str = "Ctrl+Shift+X";

static LOG_FILE: OnceLock<PathBuf> = OnceLock::new();

/// Writes a timestamped line to stderr and to the log file.
pub fn log(msg: &str) {
    let t = win::local_time(std::time::SystemTime::now());
    let line = format!(
        "[{}-{:02}-{:02} {:02}:{:02}:{:02}] {msg}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    eprintln!("{line}");
    if let Some(path) = LOG_FILE.get() {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }
}

pub fn set_status(app: &AppHandle, status: &str) {
    let state = app.state::<AppState>();
    *state.status.lock().unwrap() = status.to_string();
    let _ = state.status_item.set_text(status);
}

/// Registers Ctrl+Shift+X, which closes the replay from inside the game. Kept for the app's lifetime.
fn arm_close_key(app: &AppHandle) {
    let app_for_key = app.clone();
    let pressed = move || {
        log(&format!("{CLOSE_KEY} pressed - closing the replay"));
        player::close(&app_for_key);
    };
    match win::Hotkey::register(CLOSE_KEY, pressed) {
        Ok(k) => std::mem::forget(k),
        Err(e) => log(&e),
    }
}

fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.unminimize();
        let _ = w.set_focus();
        return;
    }
    let built = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("settings.html".into()))
        .title("LoL Death Replay")
        .inner_size(460.0, 640.0)
        .min_inner_size(400.0, 480.0)
        .center()
        .build();
    match built {
        Ok(window) => {
            if let Ok(hwnd) = window.hwnd() {
                win::exclude_from_capture(hwnd.0); // kept out of clips, like the replay window
            }
        }
        Err(e) => log(&format!("Couldn't open settings: {e}")),
    }
}

/// Opens a folder in Explorer, or a file in its default app.
fn open_path(path: &std::path::Path) {
    if let Err(e) = std::process::Command::new("explorer").arg(path).spawn() {
        log(&format!("Couldn't open {}: {e}", path.display()));
    }
}

/// The clip from this session's last death, or else the newest clip in the folder.
/// Skips a clip that has since been deleted.
fn last_clip(app: &AppHandle) -> Option<PathBuf> {
    let from_session = app.state::<AppState>().last_clip.lock().unwrap().clone();
    if let Some(clip) = from_session.filter(|clip| clip.exists()) {
        return Some(clip);
    }
    watcher::clips(&settings::current(app).clips_dir).into_iter().max().map(|(_, _, path)| path)
}

const NO_CLIPS: &str = "Nothing to replay yet"; // the settings toast fits one line

fn replay_last(app: &AppHandle) -> Result<(), String> {
    let clip = last_clip(app).ok_or(NO_CLIPS)?;
    player::show(app, &clip, "Last death", settings::current(app).after);
    Ok(())
}

/// Greys out "Replay last death" when there's nothing to replay. Called as the pointer reaches the
/// tray icon, just before the menu can open, so it's never stale (clips saved or deleted since).
fn refresh_replay_item(app: &AppHandle) {
    let has_clip = last_clip(app).is_some();
    let item = &app.state::<AppState>().replay_item;
    let _ = item.set_enabled(has_clip);
    let _ = item.set_text(if has_clip { "Replay last death" } else { "Replay last death (none yet)" });
}

// ---- commands used by ui/settings.html ----

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Info {
    version: String,
    status: String,
    start_with_windows: bool,
    /// A newer version to offer, e.g. "0.3.0".
    update: Option<String>,
    /// The settings window's limit for "Seconds before death".
    max_before: f64,
}

#[tauri::command]
fn get_settings(app: AppHandle) -> Settings {
    settings::current(&app)
}

#[tauri::command]
fn get_info(app: AppHandle) -> Info {
    Info {
        version: app.package_info().version.to_string(),
        status: app.state::<AppState>().status.lock().unwrap().clone(),
        start_with_windows: win::autostart_enabled(&app.package_info().name),
        update: updater::available(&app),
        max_before: settings::MAX_BEFORE,
    }
}

#[tauri::command]
fn save_settings(app: AppHandle, mut new: Settings, start_with_windows: bool) -> Result<(), String> {
    // A save hotkey that can't be used keeps the old one; the rest of the settings still save.
    let old_key = settings::current(&app).save_key;
    let key_error = (new.save_key != old_key).then(|| check_save_key(&new.save_key).err()).flatten();
    if key_error.is_some() {
        new.save_key = old_key.clone();
    }
    let key_changed = new.save_key != old_key;
    new.max_folder_gb = new.max_folder_gb.min(settings::MAX_FOLDER_GB);
    settings::update(&app, |s| *s = new);
    if key_changed {
        watcher::arm_save_key(&app); // takes effect now if a game is recording
    }
    win::set_autostart(&app.package_info().name, start_with_windows)
        .map_err(|e| format!("Couldn't change start with Windows: {e}"))?;
    key_error.map_or(Ok(()), Err)
}

/// Whether `combo` would work as the save hotkey: a valid combo that no other app (or our own close
/// key) has. Tries registering it for a moment, since outside a game it isn't registered.
fn check_save_key(combo: &str) -> Result<(), String> {
    if combo.is_empty() {
        return Ok(()); // turned off
    }
    if win::parse_hotkey(combo)? == win::parse_hotkey(CLOSE_KEY)? {
        return Err(format!("{CLOSE_KEY} already closes the replay"));
    }
    win::hotkey_is_free(combo)
}

/// The "Change..." folder picker. Blocks while open, so it runs off the async runtime's threads.
#[tauri::command]
async fn pick_folder(window: tauri::WebviewWindow, start: PathBuf) -> Option<PathBuf> {
    let pick = move || rfd::FileDialog::new().set_directory(start).set_parent(&window).pick_folder();
    tauri::async_runtime::spawn_blocking(pick).await.ok().flatten()
}

#[tauri::command]
fn open_clips_folder(app: AppHandle) {
    let dir = settings::current(&app).clips_dir;
    let _ = std::fs::create_dir_all(&dir);
    open_path(&dir);
}

#[tauri::command]
fn open_log() {
    if let Some(path) = LOG_FILE.get() {
        open_path(path);
    }
}

/// Lets the windows write to the log (playback errors and the like).
#[tauri::command]
fn ui_log(msg: String) {
    log(&msg);
}

/// The settings window shows the error as a toast. Async because it opens a window: a sync command
/// runs inside WebView2's message handler on Windows, and creating a webview from there deadlocks.
#[tauri::command]
async fn preview_player(app: AppHandle) -> Result<(), String> {
    replay_last(&app)
}

/// Only returns if the update failed; the settings window shows why.
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    updater::install(app).await
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let handle = app.handle().clone();
    let log_dir = app.path().app_log_dir()?;
    std::fs::create_dir_all(&log_dir)?;
    let _ = LOG_FILE.set(log_dir.join("lol-death-replay.log"));

    let (s, first_run) = settings::load(&handle);
    app.manage(SettingsState(Mutex::new(s)));

    // Tray icon and menu
    let status_item = MenuItem::with_id(app, "status", "Starting...", false, None::<&str>)?;
    let replay_item = MenuItem::with_id(app, "replay", "Replay last death", true, None::<&str>)?;
    // Added to the menu only once there's an update.
    let update_item = MenuItem::with_id(app, "update", "Update", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &status_item,
            &PredefinedMenuItem::separator(app)?,
            &replay_item,
            &MenuItem::with_id(app, "clips", "Open clips folder", true, None::<&str>)?,
            &MenuItem::with_id(app, "settings", "Settings...", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?,
        ],
    )?;
    app.manage(AppState {
        status: Mutex::new(String::new()),
        status_item,
        replay_item,
        last_clip: Mutex::new(None),
        recording: Mutex::new(None),
        save_key: Mutex::new(None),
    });
    app.manage(updater::UpdateState::new(update_item, menu.clone()));
    refresh_replay_item(app.handle());
    TrayIconBuilder::with_id("tray")
        .icon(app.default_window_icon().cloned().ok_or("no app icon")?)
        .tooltip("LoL Death Replay")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_tray_icon_event(|tray, event| {
            if matches!(event, TrayIconEvent::Enter { .. } | TrayIconEvent::Click { .. }) {
                refresh_replay_item(tray.app_handle());
            }
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            // Normally greyed out with no clips; this catches a clip deleted while the menu was open.
            "replay" => replay_last(app).unwrap_or_else(|e| log(&e)),
            "update" => drop(tauri::async_runtime::spawn(updater::install(app.clone()))),
            "clips" => open_clips_folder(app.clone()),
            "settings" => open_settings(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    arm_close_key(app.handle());

    log(&format!("LoL Death Replay {} started", app.package_info().version));
    let update_handle = handle.clone();
    std::thread::spawn(move || watcher::run(handle));
    std::thread::spawn(move || updater::run(update_handle));
    if first_run {
        // Start with Windows is on by default. Debug builds leave it alone, so a dev run doesn't
        // put target\debug's exe in the real Run key.
        if !cfg!(debug_assertions) {
            let name = &app.package_info().name;
            win::set_autostart(name, true).unwrap_or_else(|e| log(&format!("Couldn't turn on start with Windows: {e}")));
        }
        open_settings(app.handle());
    }
    Ok(())
}

fn main() {
    tauri::Builder::default()
        // A second launch just opens settings in the running app.
        .plugin(tauri_plugin_single_instance::init(|app, _, _| open_settings(app)))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            get_settings,
            get_info,
            save_settings,
            open_clips_folder,
            open_log,
            pick_folder,
            preview_player,
            install_update,
            ui_log,
            player::player_close,
            player::player_resize,
            player::player_fit,
            player::player_inputs,
        ])
        .setup(setup)
        .build(tauri::generate_context!())
        .expect("error while building the app")
        .run(|_, event| {
            // Keep running in the tray when the last window closes; only Quit exits.
            if let RunEvent::ExitRequested { api, code: None, .. } = event {
                api.prevent_exit();
            }
        });
}
