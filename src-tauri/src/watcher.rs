//! The background loop: waits for a game, records while it runs, and saves a clip on each death.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use tauri::{AppHandle, Manager};
use windows_sys::Win32::Foundation::SYSTEMTIME;

use crate::live_client::{champion, find_killer, find_player, is_dead, my_names, LiveClient};
use crate::recorder::{ffmpeg_path, next_capture, pick_encoder, remove_stale_segments, Recorder};
use crate::{player, set_status, settings, toast, win, AppState};

pub fn run(app: AppHandle) {
    remove_stale_segments(); // safe here: single-instance means no other copy is recording
    let client = LiveClient::new();
    let ffmpeg = ffmpeg_path();
    let mut auto_encoder: Option<String> = None;
    let mut rec: Option<Arc<Recorder>> = None;
    let mut names = HashSet::new();
    let mut game_folder: Option<String> = None; // where this game's clips go, decided once per game
    let mut was_dead = false;
    let mut failed = false; // don't retry recording until the next game
    let mut idle = false; // in a game, but replay and clips are both turned off
    let mut fallback: Option<&str> = None; // a capture method failed this game, so record with this one instead

    set_status(&app, "Waiting for game");
    loop {
        let Some(active) = client.get("activeplayer") else {
            if rec.take().is_some() || failed || idle {
                set_recording(&app, None);
                player::close(&app);
                crate::log("Game over. Waiting for the next one...");
                set_status(&app, "Waiting for game");
            }
            (failed, idle, fallback) = (false, false, None);
            game_folder = None;
            thread::sleep(Duration::from_secs(2));
            continue;
        };
        if failed {
            thread::sleep(Duration::from_secs(2));
            continue;
        }

        if rec.is_none() {
            let mut s = settings::current(&app);
            // The fast setting tries the game window first; gdigrab is used as is.
            s.capture = fallback.unwrap_or(if s.capture == "gdigrab" { "gdigrab" } else { "gfxcapture" }).into();
            // Nothing to record for. Checked every loop, so turning one back on starts recording.
            if !s.auto_play && !s.save_clips {
                if !idle {
                    set_status(&app, "Replay and clips are off");
                    idle = true;
                }
                thread::sleep(Duration::from_secs(2));
                continue;
            }
            idle = false;
            let encoder = if s.encoder == "auto" {
                auto_encoder.get_or_insert_with(|| pick_encoder(&ffmpeg)).clone()
            } else {
                s.encoder.clone()
            };
            names = my_names(&active);
            crate::log(&format!("Game found - playing as {}", names.iter().min().map_or("?", |n| n)));
            if game_folder.is_none() {
                game_folder = Some(current_game_folder(&client, &names));
            }
            match Recorder::start(&ffmpeg, &s, &encoder) {
                Ok(r) => {
                    let r = Arc::new(r);
                    set_recording(&app, Some((r.clone(), game_folder.clone().unwrap_or_default())));
                    rec = Some(r);
                    was_dead = false;
                    set_status(&app, "Recording this game");
                }
                Err(e) => {
                    crate::log(&format!("Couldn't start recording: {e}"));
                    set_status(&app, "Recording failed - see log");
                    failed = true;
                    continue;
                }
            }
        }

        let r = rec.as_ref().unwrap();
        if !r.alive() {
            crate::log(&format!("ffmpeg stopped unexpectedly:\n{}", r.error_tail()));
            set_recording(&app, None);
            // Window capture needs a recent ffmpeg and Windows; Desktop Duplication doesn't work on
            // every setup (some dual-GPU laptops, remote desktop).
            if let Some(next) = next_capture(r.capture) {
                crate::log(&format!("{} capture failed - switching to {next} for this game", r.capture));
                fallback = Some(next);
                rec = None;
                continue;
            }
            set_status(&app, "Recording failed - see log");
            rec = None;
            failed = true;
            continue;
        }

        let dead = client.get("playerlist").is_some_and(|p| is_dead(&p, &names));
        if dead && !was_dead {
            crate::log("You died - saving replay...");
            let (app, r, names) = (app.clone(), r.clone(), names.clone());
            let folder = game_folder.clone().unwrap_or_default();
            thread::spawn(move || handle_death(app, r, names, folder));
        }
        was_dead = dead;
        thread::sleep(Duration::from_millis(250));
    }
}

/// The folder name for the game being played, e.g. "2026-11-21_2130_Jinx_ARAM".
fn current_game_folder(client: &LiveClient, names: &HashSet<String>) -> String {
    let stats = client.get("gamestats");
    let stat = |key: &str| stats.as_ref().and_then(|g| g.get(key).cloned());
    let game_time = stat("gameTime").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let mode = stat("gameMode").and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    let champ = client
        .get("playerlist")
        .and_then(|p| find_player(&p, names).and_then(champion))
        .map_or_else(|| "Unknown".to_string(), |c| c.file);
    // When the game started, not when we noticed it, so starting the app mid-game names it the same.
    let started = win::local_time(SystemTime::now() - Duration::from_secs_f64(game_time.max(0.0)));
    safe(&game_folder_name(&started, &champ, &mode))
}

fn game_folder_name(t: &SYSTEMTIME, champion: &str, mode: &str) -> String {
    let mode = match mode {
        "CLASSIC" => "SR",
        "CHERRY" => "Arena",
        "PRACTICETOOL" => "Practice",
        "" => "Unknown",
        other => other,
    };
    format!("{}-{:02}-{:02}_{:02}{:02}_{champion}_{mode}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute)
}

fn handle_death(app: AppHandle, rec: Arc<Recorder>, names: HashSet<String>, game_folder: String) {
    let s = settings::current(&app);
    thread::sleep(Duration::from_secs_f64(s.after)); // capture the death itself
    if !rec.alive() {
        return;
    }
    let client = LiveClient::new();
    let killer = find_killer(client.get("eventdata").as_ref(), client.get("playerlist").as_ref(), &names);
    let game_time = game_time(&client);
    let (min, sec) = (game_time / 60, game_time % 60);
    let name = format!("{}_by-{}.mp4", clip_time(game_time), safe(&killer.file));
    let killer = killer.display;
    // Unsaved replays still need a file to play; they go to a temp folder that only holds the latest.
    let dir = if s.save_clips { s.clips_dir.join(&game_folder) } else { unsaved_dir() };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        crate::log(&format!("Couldn't create {}: {e}", dir.display()));
        return;
    }
    let out = dir.join(name);
    match rec.save_last(s.before + s.after, &out) {
        Ok(()) => {
            crate::log(&format!("Saved {} (killed by {killer})", out.display()));
            *app.state::<AppState>().last_clip.lock().unwrap() = Some(out.clone());
            if s.auto_play {
                player::show(&app, &out, &format!("Death at {min}:{sec:02} - killed by {killer}"), s.after);
            }
            if s.save_clips {
                prune(&s.clips_dir, (s.max_folder_gb * 1e9) as u64, &out);
            } else {
                prune(&dir, 0, &out);
            }
        }
        Err(e) => crate::log(&format!("Clip save failed: {e}")),
    }
}

/// Seconds into the game, or 0 if the game doesn't say.
fn game_time(client: &LiveClient) -> u64 {
    client.get("gamestats").and_then(|g| g.get("gameTime").and_then(Value::as_f64)).unwrap_or(0.0) as u64
}

/// How clip names start: the game time, e.g. "12m40s".
fn clip_time(game_time: u64) -> String {
    format!("{:02}m{:02}s", game_time / 60, game_time % 60)
}

// ---- The save hotkey ----

/// Sets what the save hotkey saves from (this game's recorder and clip folder), or `None` when not
/// recording, then registers or releases the hotkey to match.
fn set_recording(app: &AppHandle, now: Option<(Arc<Recorder>, String)>) {
    *app.state::<AppState>().recording.lock().unwrap() = now;
    arm_save_key(app);
}

/// Registers the save hotkey from settings while a game is recording, and releases it otherwise, so
/// the combo (Ctrl+Shift+S is "Save as" in many apps) only belongs to us during a game.
pub fn arm_save_key(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut key = state.save_key.lock().unwrap();
    *key = None; // released first, so registering the same combo again works
    let combo = settings::current(app).save_key;
    if combo.is_empty() || state.recording.lock().unwrap().is_none() {
        return;
    }
    let app_for_key = app.clone();
    match win::Hotkey::register(&combo, move || save_now(&app_for_key)) {
        Ok(k) => *key = Some(k),
        Err(e) => crate::log(&e),
    }
}

fn save_now(app: &AppHandle) {
    let Some((rec, game_folder)) = app.state::<AppState>().recording.lock().unwrap().clone() else { return };
    crate::log("Save hotkey pressed - saving...");
    let app = app.clone();
    thread::spawn(move || save_moment(app, rec, game_folder));
}

/// Keeps the last `before` seconds as e.g. "12m40s_saved.mp4": no wait for an "after", no replay
/// window, just a toast. Goes to the clips folder even with "Save clips" off, since it was asked for.
fn save_moment(app: AppHandle, rec: Arc<Recorder>, game_folder: String) {
    let s = settings::current(&app);
    let name = format!("{}_saved.mp4", clip_time(game_time(&LiveClient::new())));
    let dir = s.clips_dir.join(&game_folder);
    let out = dir.join(name);
    let saved = std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Couldn't create {}: {e}", dir.display()))
        .and_then(|()| rec.save_last(s.before, &out));
    match saved {
        Ok(()) => {
            crate::log(&format!("Saved {}", out.display()));
            toast::show(&app, "Saved");
            prune(&s.clips_dir, (s.max_folder_gb * 1e9) as u64, &out);
        }
        Err(e) => {
            crate::log(&format!("Clip save failed: {e}"));
            toast::show(&app, "Couldn't save - see log");
        }
    }
}

fn unsaved_dir() -> PathBuf {
    std::env::temp_dir().join("LoLDeathReplay_unsaved")
}

/// Keeps letters, digits, `_` and `-` (ASCII only); each run of anything else becomes one `-`.
fn safe(s: &str) -> String {
    let mut cleaned = String::with_capacity(s.len());
    let mut in_run = false;
    for c in s.chars() {
        let keep = c.is_ascii_alphanumeric() || c == '_' || c == '-';
        if keep {
            cleaned.push(c);
        } else if !in_run {
            cleaned.push('-');
        }
        in_run = !keep;
    }
    let cleaned: String = cleaned.trim_matches('-').chars().take(60).collect();
    if cleaned.is_empty() { "unknown".into() } else { cleaned }
}

/// Every clip in `dir` and in its game folders (one level down), as (modified, size, path).
pub fn clips(dir: &Path) -> Vec<(SystemTime, u64, PathBuf)> {
    let mut found = vec![];
    let Ok(entries) = std::fs::read_dir(dir) else { return found };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let files: Vec<PathBuf> = if path.is_dir() {
            std::fs::read_dir(&path).into_iter().flatten().filter_map(|e| e.ok()).map(|e| e.path()).collect()
        } else {
            vec![path]
        };
        for file in files.into_iter().filter(|p| p.extension().is_some_and(|x| x == "mp4")) {
            if let Some(meta) = file.metadata().ok().filter(|m| m.is_file()) {
                if let Ok(modified) = meta.modified() {
                    found.push((modified, meta.len(), file));
                }
            }
        }
    }
    found
}

/// Deletes whole games, oldest first, until the folder's clips total at most `max_bytes`.
/// A game is a game folder, or a single clip directly in `dir` (how the unsaved-replays folder holds them).
/// The game holding `newest` (the clip just saved) is never deleted, even if it alone is over the limit.
fn prune(dir: &Path, max_bytes: u64, newest: &Path) {
    let group_of = |clip: &Path| -> PathBuf {
        match clip.parent() {
            Some(parent) if parent != dir => parent.to_path_buf(),
            _ => clip.to_path_buf(),
        }
    };
    // game -> (last clip's time, total size)
    let mut games: HashMap<PathBuf, (SystemTime, u64)> = HashMap::new();
    for (modified, size, path) in clips(dir) {
        let game = games.entry(group_of(&path)).or_insert((modified, 0));
        game.0 = game.0.max(modified);
        game.1 += size;
    }
    let current = group_of(newest);
    let mut games: Vec<(SystemTime, u64, PathBuf)> =
        games.into_iter().map(|(path, (modified, size))| (modified, size, path)).collect();
    games.sort(); // oldest first
    let mut total: u64 = games.iter().map(|(_, size, _)| size).sum();
    for (_, size, path) in games {
        if total <= max_bytes {
            break;
        }
        if path == current {
            continue;
        }
        let removed = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        if removed.is_ok() {
            total -= size;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names_for_files() {
        assert_eq!(safe("Miss Fortune"), "Miss-Fortune");
        assert_eq!(safe("Kai'Sa  (ARAM)"), "Kai-Sa-ARAM"); // a run of several becomes one "-"
        assert_eq!(safe("a-!b"), "a--b"); // a kept "-" isn't merged with a replaced one
        assert_eq!(safe("Ñunu & Willump"), "unu-Willump"); // non-ASCII is replaced, edges trimmed
        assert_eq!(safe("???"), "unknown");
        assert_eq!(safe(&"x".repeat(80)).len(), 60);
    }

    #[test]
    fn names_clips_by_game_time() {
        assert_eq!(clip_time(760), "12m40s");
        assert_eq!(clip_time(0), "00m00s");
        assert_eq!(clip_time(3725), "62m05s");
    }

    #[test]
    fn names_game_folders() {
        let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
        (t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond) = (2026, 11, 21, 21, 30, 5);
        assert_eq!(game_folder_name(&t, "Jinx", "CLASSIC"), "2026-11-21_2130_Jinx_SR");
        assert_eq!(game_folder_name(&t, "MissFortune", "ARAM"), "2026-11-21_2130_MissFortune_ARAM");
        assert_eq!(game_folder_name(&t, "Jinx", ""), "2026-11-21_2130_Jinx_Unknown");
    }

    #[test]
    fn prune_deletes_whole_games() {
        let dir = std::env::temp_dir().join(format!("prune_games_test_{}", std::process::id()));
        let games = [dir.join("game1"), dir.join("game2"), dir.join("game3")];
        let now = SystemTime::now();
        let mut age = 100;
        // A loose clip, then two clips each in game1 and game2, one in game3.
        let mut make = |folder: &Path, name: &str| {
            std::fs::create_dir_all(folder).unwrap();
            let path = folder.join(name);
            let f = std::fs::File::create(&path).unwrap();
            f.set_len(100).unwrap();
            f.set_modified(now - Duration::from_secs(age)).unwrap();
            age -= 1;
            path
        };
        let loose = make(&dir, "old.mp4");
        make(&games[0], "a.mp4");
        make(&games[0], "b.mp4");
        make(&games[1], "a.mp4");
        make(&games[1], "b.mp4");
        let newest = make(&games[2], "a.mp4");
        assert_eq!(clips(&dir).len(), 6);

        // 600 bytes, limit 250: the loose clip and game1 go, which leaves 300, so game2 goes as a
        // whole too rather than losing just one of its clips.
        prune(&dir, 250, &newest);
        assert!(!loose.exists() && !games[0].exists() && !games[1].exists());
        assert!(newest.exists());

        // The current game is kept even when it alone is over the limit.
        prune(&dir, 0, &newest);
        assert!(newest.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
