//! The update check: asks the newest GitHub release at startup and then at most once a day whether
//! there's a newer version. Never during a game, and never installs by itself: a newer version is
//! offered in the tray menu and the settings window, and installed when the user picks it.

use std::path::PathBuf;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem};
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_updater::UpdaterExt;

use crate::live_client::LiveClient;

const DAY: u64 = 24 * 60 * 60;
const RETRY_AFTER_ERROR: u64 = 60 * 60; // offline, or GitHub didn't answer
const TRAY_POSITION: usize = 1; // under the status line

pub struct UpdateState {
    /// The newer version on offer, if any.
    available: Mutex<Option<String>>,
    item: MenuItem<Wry>,
    menu: Menu<Wry>,
}

impl UpdateState {
    /// `item` is the tray menu's update entry; it's only added to `menu` once there's an update.
    pub fn new(item: MenuItem<Wry>, menu: Menu<Wry>) -> Self {
        Self { available: Mutex::new(None), item, menu }
    }
}

/// What's remembered between runs, so restarting the app doesn't mean checking again.
#[derive(Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LastCheck {
    /// Seconds since 1970.
    time: u64,
    /// The newer version that check found.
    available: Option<String>,
}

fn file(app: &AppHandle) -> PathBuf {
    app.path().app_config_dir().expect("no config dir").join("update-check.json")
}

fn load(app: &AppHandle) -> Option<LastCheck> {
    serde_json::from_str(&std::fs::read_to_string(file(app)).ok()?).ok()
}

fn save(app: &AppHandle, last: &LastCheck) {
    let path = file(app);
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    if let Err(e) = std::fs::write(&path, serde_json::to_string(last).unwrap_or_default()) {
        crate::log(&format!("Couldn't save {}: {e}", path.display()));
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A day since the last check, or never checked. A check "in the future" means the clock was
/// turned back, so that counts as due too rather than waiting until the clock catches up.
fn is_due(last: Option<u64>, now: u64) -> bool {
    last.is_none_or(|last| last > now || now - last >= DAY)
}

pub fn run(app: AppHandle) {
    let game = LiveClient::new();
    let last = load(&app);
    // Checked less than a day ago: offer what that check found, unless it's since been installed.
    let current = app.package_info().version.to_string();
    if let Some(version) = last.as_ref().and_then(|l| l.available.clone()).filter(|v| *v != current) {
        offer(&app, &version);
    }
    let mut last_time = last.map(|l| l.time);
    let mut retry_at = 0;
    loop {
        let now = now();
        // The game answering means one is in progress (loading screen aside).
        if is_due(last_time, now) && now >= retry_at && game.get("activeplayer").is_none() {
            match check(&app) {
                Ok(available) => {
                    match &available {
                        Some(version) => {
                            crate::log(&format!("Update available: {version}"));
                            offer(&app, version);
                        }
                        None => withdraw(&app),
                    }
                    last_time = Some(now);
                    save(&app, &LastCheck { time: now, available });
                }
                Err(e) => {
                    crate::log(&format!("Update check failed: {e}"));
                    retry_at = now + RETRY_AFTER_ERROR;
                }
            }
        }
        thread::sleep(Duration::from_secs(10 * 60));
    }
}

/// The newer version, if there is one.
fn check(app: &AppHandle) -> Result<Option<String>, String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = tauri::async_runtime::block_on(updater.check()).map_err(|e| e.to_string())?;
    Ok(update.map(|u| u.version))
}

/// Shows "Update to vX.Y.Z" in the tray menu; the settings window reads it from `available()`.
/// The lock is let go before touching the menu: that waits for the main thread, which may itself be
/// waiting for the lock (in `get_info`).
fn offer(app: &AppHandle, version: &str) {
    let state = app.state::<UpdateState>();
    let first = state.available.lock().unwrap().replace(version.to_string()).is_none();
    let _ = state.item.set_text(format!("Update to v{version}"));
    let _ = state.item.set_enabled(true);
    if first {
        let _ = state.menu.insert(&state.item, TRAY_POSITION);
    }
}

fn withdraw(app: &AppHandle) {
    let state = app.state::<UpdateState>();
    let was_offered = state.available.lock().unwrap().take().is_some();
    if was_offered {
        let _ = state.menu.remove(&state.item);
    }
}

pub fn available(app: &AppHandle) -> Option<String> {
    app.state::<UpdateState>().available.lock().unwrap().clone()
}

/// Downloads and runs the installer, which closes the app and starts the new version. Only returns
/// if that fails. Asks GitHub again first: the release on offer may be from an earlier check.
pub async fn install(app: AppHandle) -> Result<(), String> {
    let item = &app.state::<UpdateState>().item;
    let _ = item.set_enabled(false);
    let _ = item.set_text("Downloading update...");
    let result = download_and_install(&app).await;
    if let Err(e) = &result {
        crate::log(&format!("Update failed: {e}"));
        let _ = item.set_enabled(true);
        if let Some(version) = available(&app) {
            let _ = item.set_text(format!("Update to v{version}"));
        }
    }
    result
}

async fn download_and_install(app: &AppHandle) -> Result<(), String> {
    let update = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())?;
    let Some(update) = update else {
        withdraw(app);
        return Err("Already up to date".into());
    };
    crate::log(&format!("Installing update {}", update.version));
    update.download_and_install(|_, _| {}, || {}).await.map_err(|e| e.to_string())?;
    // On Windows the installer has already closed the app by now.
    app.restart();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_at_most_once_a_day() {
        let now = 1_800_000_000;
        assert!(is_due(None, now)); // never checked
        assert!(!is_due(Some(now - 60), now));
        assert!(!is_due(Some(now - DAY + 1), now));
        assert!(is_due(Some(now - DAY), now));
        assert!(is_due(Some(now + 60), now)); // the clock went back
    }
}
