// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod live_client;
mod player;
mod recorder;
mod settings;
mod watcher;

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, Wry};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as _};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

use settings::{Settings, SettingsState};

pub struct AppState {
    status: Mutex<String>,
    status_item: MenuItem<Wry>,
    pub last_clip: Mutex<Option<PathBuf>>,
}

static LOG_FILE: OnceLock<PathBuf> = OnceLock::new();

/// Writes a timestamped line to stderr and to the log file.
pub fn log(msg: &str) {
    let line = format!("[{}] {msg}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
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
    if let Err(e) = built {
        log(&format!("Couldn't open settings: {e}"));
    }
}

fn open_path(path: &std::path::Path) {
    if let Err(e) = tauri_plugin_opener::open_path(path, None::<&str>) {
        log(&format!("Couldn't open {}: {e}", path.display()));
    }
}

/// The clip from this session's last death, or else the newest clip in the folder.
fn last_clip(app: &AppHandle) -> Option<PathBuf> {
    if let Some(clip) = app.state::<AppState>().last_clip.lock().unwrap().clone() {
        return Some(clip);
    }
    std::fs::read_dir(settings::current(app).clips_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max()
        .map(|(_, path)| path)
}

fn replay_last(app: &AppHandle) {
    match last_clip(app) {
        Some(clip) => player::show(app, &clip, "Last death", settings::current(app).after),
        None => log("No clips yet"),
    }
}

// ---- commands used by ui/settings.html ----

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Info {
    version: String,
    status: String,
    start_with_windows: bool,
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
        start_with_windows: app.autolaunch().is_enabled().unwrap_or(false),
    }
}

#[tauri::command]
fn save_settings(app: AppHandle, new: Settings, start_with_windows: bool) -> Result<(), String> {
    settings::update(&app, |s| *s = new);
    let autostart = app.autolaunch();
    let result = if start_with_windows { autostart.enable() } else { autostart.disable() };
    result.map_err(|e| format!("Couldn't change start with Windows: {e}"))
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

#[tauri::command]
fn preview_player(app: AppHandle) {
    replay_last(&app);
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
    let menu = Menu::with_items(
        app,
        &[
            &status_item,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "replay", "Replay last death", true, None::<&str>)?,
            &MenuItem::with_id(app, "clips", "Open clips folder", true, None::<&str>)?,
            &MenuItem::with_id(app, "settings", "Settings...", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?,
        ],
    )?;
    app.manage(AppState { status: Mutex::new(String::new()), status_item, last_clip: Mutex::new(None) });
    TrayIconBuilder::with_id("tray")
        .icon(app.default_window_icon().cloned().ok_or("no app icon")?)
        .tooltip("LoL Death Replay")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "replay" => replay_last(app),
            "clips" => open_clips_folder(app.clone()),
            "settings" => open_settings(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    // Ctrl+Shift+X closes the replay from inside the game.
    let shortcut = app.global_shortcut().on_shortcut("ctrl+shift+x", |app, _, event| {
        if event.state == ShortcutState::Pressed {
            player::close(app);
        }
    });
    if let Err(e) = shortcut {
        log(&format!("Couldn't register Ctrl+Shift+X (another app may be using it): {e}"));
    }

    log(&format!("LoL Death Replay {} started", app.package_info().version));
    std::thread::spawn(move || watcher::run(handle));
    if first_run {
        open_settings(app.handle());
    }
    Ok(())
}

fn main() {
    tauri::Builder::default()
        // A second launch just opens settings in the running app.
        .plugin(tauri_plugin_single_instance::init(|app, _, _| open_settings(app)))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            get_settings,
            get_info,
            save_settings,
            open_clips_folder,
            open_log,
            preview_player,
            ui_log,
            player::player_close,
            player::player_resize,
            player::player_fit,
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
