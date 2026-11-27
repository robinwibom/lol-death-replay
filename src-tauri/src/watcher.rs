//! The background loop: waits for a game, records while it runs, and saves a clip on each death.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use tauri::{AppHandle, Manager};
use windows_sys::Win32::Foundation::SYSTEMTIME;

use crate::live_client::{champion, find_killer, last_fight, find_player, is_dead, my_names, LiveClient};
use crate::recorder::{ffmpeg_path, next_capture, pick_encoder, remove_stale_segments, Recorder};
use crate::{player, set_status, settings, toast, win, AppState};

pub fn run(app: AppHandle) {
    remove_stale_segments(); // safe here: single-instance means no other copy is recording
    move_into_day_folders(&settings::current(&app).clips_dir);
    let client = LiveClient::new();
    let ffmpeg = ffmpeg_path();
    let mut auto_encoder: Option<(bool, String)> = None; // (force_h264, the encoder picked)
    let mut rec: Option<Arc<Recorder>> = None;
    let mut names = HashSet::new();
    let mut game_folder: Option<PathBuf> = None; // where this game's clips go, decided once per game
    let mut was_dead = false;
    let mut shown_all_sound = false; // whether the status currently says the sound is all sound
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
                // Picked once per H.264 setting: testing the encoders takes a moment.
                if auto_encoder.as_ref().is_none_or(|(force_h264, _)| *force_h264 != s.force_h264) {
                    auto_encoder = Some((s.force_h264, pick_encoder(&ffmpeg, s.force_h264)));
                }
                auto_encoder.as_ref().unwrap().1.clone()
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
                    shown_all_sound = r.all_sound();
                    set_status(&app, recording_status(shown_all_sound));
                    rec = Some(r);
                    was_dead = false;
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
        // The sound can switch from all sound to the game's alone once the game's process appears.
        if r.all_sound() != shown_all_sound {
            shown_all_sound = r.all_sound();
            set_status(&app, recording_status(shown_all_sound));
        }
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

/// The status while recording. It starts with "Recording this" (the settings window keys off that),
/// and says when the sound is all sound, since voice chat may be in it.
fn recording_status(all_sound: bool) -> &'static str {
    if all_sound {
        "Recording this game (all sound: voice chat may be included)"
    } else {
        "Recording this game"
    }
}

/// Where the game being played keeps its clips, inside the clips folder: its day's folder, then
/// its own, e.g. "2026-11-21\2130_Jinx_ARAM".
fn current_game_folder(client: &LiveClient, names: &HashSet<String>) -> PathBuf {
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
    game_folder(&started, &champ, &mode)
}

/// A game that runs past midnight stays in the day it started.
fn game_folder(t: &SYSTEMTIME, champion: &str, mode: &str) -> PathBuf {
    let mode = match mode {
        "CLASSIC" => "SR",
        "CHERRY" => "Arena",
        "PRACTICETOOL" => "Practice",
        "" => "Unknown",
        other => other,
    };
    let day = format!("{}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay);
    Path::new(&day).join(safe(&format!("{:02}{:02}_{champion}_{mode}", t.wHour, t.wMinute)))
}

fn handle_death(app: AppHandle, rec: Arc<Recorder>, names: HashSet<String>, game_folder: PathBuf) {
    let s = settings::current(&app);
    thread::sleep(Duration::from_secs_f64(s.after)); // capture the death itself
    if !rec.alive() {
        return;
    }
    let client = LiveClient::new();
    let events = client.get("eventdata");
    let killer = find_killer(events.as_ref(), client.get("playerlist").as_ref(), &names);
    let now = game_seconds(&client);
    // How long ago the death happened, on the game clock: it's noticed up to a poll late, and the
    // calls above take time too, so it's a little more than `after`. A latest death further back
    // than that is an earlier one, because this one isn't in the events yet.
    let this_death = last_fight(events.as_ref(), &names)
        .zip(now)
        .map(|(fight, now)| (now - fight.death, fight.length))
        .filter(|(ago, _)| (0.0..=s.after + DEATH_LATE).contains(ago));
    let (since_death, fight_length) = this_death.unwrap_or((s.after, 0.0));
    let before = s.before_death(fight_length);
    if before > s.before {
        crate::log(&format!("Died in a fight - keeping {before:.0}s before the death"));
    }
    let game_time = now.unwrap_or(0.0) as u64;
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
    match rec.save_last(before + since_death, &out) {
        Ok(()) => {
            crate::log(&format!("Saved {} (killed by {killer})", out.display()));
            *app.state::<AppState>().last_clip.lock().unwrap() = Some(out.clone());
            if s.auto_play {
                player::show(&app, &out, &format!("Death at {min}:{sec:02} - killed by {killer}"), since_death);
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

/// How much later than `after` a death's event can still be this death's: a poll (0.25s), the API
/// calls in between, and room to spare.
const DEATH_LATE: f64 = 5.0;

/// Seconds into the game, if the game says.
fn game_seconds(client: &LiveClient) -> Option<f64> {
    client.get("gamestats").and_then(|g| g.get("gameTime").and_then(Value::as_f64))
}

/// Whole seconds into the game, or 0 if the game doesn't say.
fn game_time(client: &LiveClient) -> u64 {
    game_seconds(client).unwrap_or(0.0) as u64
}

/// How clip names start: the game time, e.g. "12m40s".
fn clip_time(game_time: u64) -> String {
    format!("{:02}m{:02}s", game_time / 60, game_time % 60)
}

// ---- The save hotkey ----

/// Sets what the save hotkey saves from (this game's recorder and clip folder), or `None` when not
/// recording, then registers or releases the hotkey to match.
fn set_recording(app: &AppHandle, now: Option<(Arc<Recorder>, PathBuf)>) {
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
    if state.recording.lock().unwrap().is_none() {
        return;
    }
    if combo.is_empty() {
        crate::log("Save hotkey is off (Settings > Clips)");
        return;
    }
    let app_for_key = app.clone();
    match win::Hotkey::register(&combo, move || save_now(&app_for_key)) {
        Ok(k) => {
            crate::log(&format!("Save hotkey {combo} ready"));
            *key = Some(k);
        }
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
fn save_moment(app: AppHandle, rec: Arc<Recorder>, game_folder: PathBuf) {
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

/// Moves game folders from older versions' layout ("2026-11-21_2130_Jinx_SR", right in the clips
/// folder) into day folders ("2026-11-210_Jinx_SR"). Renames on the same drive: nothing is
/// copied. One that can't move now (a clip in it is open, say) is tried again on the next start.
fn move_into_day_folders(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut moved = 0;
    for path in entries.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()) {
        let Some((day, game)) = path.file_name().and_then(|n| n.to_str()).and_then(old_game_folder) else { continue };
        // The clips folder may be one the user keeps other things in: only clips folders move.
        let only_clips = std::fs::read_dir(&path).is_ok_and(|mut files| {
            files.all(|f| f.is_ok_and(|f| f.path().is_file() && f.path().extension().is_some_and(|x| x == "mp4")))
        });
        let to = dir.join(day).join(game);
        if !only_clips || to.exists() {
            continue;
        }
        match std::fs::create_dir_all(dir.join(day)).and_then(|()| std::fs::rename(&path, &to)) {
            Ok(()) => moved += 1,
            Err(e) => crate::log(&format!("Couldn't move {} into its day's folder: {e}", path.display())),
        }
    }
    if moved > 0 {
        crate::log(&format!("Moved {moved} games into day folders"));
    }
}

/// "2026-11-21_2130_Jinx_SR" -> ("2026-11-21", "2130_Jinx_SR"); None for any other name.
fn old_game_folder(name: &str) -> Option<(&str, &str)> {
    let b = name.as_bytes();
    let digits = |from: usize, to: usize| b.get(from..to).is_some_and(|d| d.iter().all(u8::is_ascii_digit));
    let shape = digits(0, 4) && b.get(4) == Some(&b'-') && digits(5, 7) && b.get(7) == Some(&b'-')
        && digits(8, 10) && b.get(10) == Some(&b'_') && digits(11, 15) && b.get(15) == Some(&b'_') && b.len() > 16;
    shape.then(|| (&name[..10], &name[11..]))
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

/// Every clip in `dir`, as (modified, size, path): in its day folders' game folders (two levels
/// down), and in game folders right inside it (one level down: older versions' layout, and any a
/// move into day folders couldn't do).
pub fn clips(dir: &Path) -> Vec<(SystemTime, u64, PathBuf)> {
    let mut found = vec![];
    find_clips(dir, 2, &mut found);
    found
}

fn find_clips(dir: &Path, levels_down: u32, found: &mut Vec<(SystemTime, u64, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for path in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
        let Ok(meta) = path.metadata() else { continue };
        if meta.is_dir() {
            if levels_down > 0 {
                find_clips(&path, levels_down - 1, found);
            }
        } else if path.extension().is_some_and(|x| x == "mp4") {
            if let Ok(modified) = meta.modified() {
                found.push((modified, meta.len(), path));
            }
        }
    }
}

/// Deletes whole games, oldest first, until the folder's clips total at most `max_bytes`.
/// A game is a game folder, or a single clip directly in `dir` (how the unsaved-replays folder holds them).
/// A day folder goes with its last game.
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
            if let Some(day) = path.parent().filter(|day| *day != dir) {
                let _ = std::fs::remove_dir(day); // only removes an empty folder
            }
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
        (t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond) = (2026, 11, 1, 9, 5, 5);
        let day = Path::new("2026-11-01");
        assert_eq!(game_folder(&t, "Jinx", "CLASSIC"), day.join("0905_Jinx_SR"));
        assert_eq!(game_folder(&t, "MissFortune", "ARAM"), day.join("0905_MissFortune_ARAM"));
        assert_eq!(game_folder(&t, "Jinx", ""), day.join("0905_Jinx_Unknown"));
        // Only the game's part is cleaned: the day's separator stays a separator.
        assert_eq!(game_folder(&t, "Jinx", "Odd/Mode"), day.join("0905_Jinx_Odd-Mode"));
    }

    #[test]
    fn recognizes_old_game_folders() {
        assert_eq!(old_game_folder("2026-11-21_2130_Jinx_SR"), Some(("2026-11-21", "2130_Jinx_SR")));
        assert_eq!(old_game_folder("2026-11-21"), None); // a day folder
        assert_eq!(old_game_folder("2026-11-21_holiday"), None);
        assert_eq!(old_game_folder("2026-11-21_2130_"), None);
        assert_eq!(old_game_folder("26-11-21_2130_Jinx_SR"), None);
        assert_eq!(old_game_folder("2026-11-2ä_2130_Jinx"), None);
    }

    #[test]
    fn moves_old_game_folders_into_days() {
        let dir = std::env::temp_dir().join(format!("day_folders_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let put = |rel: &str| {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"x").unwrap();
        };
        put("2026-11-21_2130_Jinx_SR/12m40s_by-Ahri.mp4");
        put("2026-11-21_2210_Lux_ARAM/01m00s_saved.mp4");
        put("2026-11-22_0900_Jinx_SR/03m00s_by-Vi.mp4");
        put("2026-11-22_1200_photos_trip/beach.jpg"); // not ours: has something besides clips
        put("2026-11-23_1000_Ezreal_SR/05m00s_by-Vi.mp4"); // its new place is already taken
        put("2026-11-23/1000_Ezreal_SR/01m00s_by-Lux.mp4");

        move_into_day_folders(&dir);
        for moved in ["2026-11-21/2130_Jinx_SR/12m40s_by-Ahri.mp4", "2026-11-21/2210_Lux_ARAM/01m00s_saved.mp4",
            "2026-11-22/0900_Jinx_SR/03m00s_by-Vi.mp4"] {
            assert!(dir.join(moved).exists(), "{moved}");
        }
        assert!(!dir.join("2026-11-21_2130_Jinx_SR").exists());
        assert!(dir.join("2026-11-22_1200_photos_trip/beach.jpg").exists());
        assert!(dir.join("2026-11-23_1000_Ezreal_SR/05m00s_by-Vi.mp4").exists());
        assert!(dir.join("2026-11-23/1000_Ezreal_SR/01m00s_by-Lux.mp4").exists());
        // Clips in both layouts are found (the leftover old folder too); the photo isn't a clip.
        assert_eq!(clips(&dir).len(), 5);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prune_deletes_a_day_with_its_last_game() {
        let dir = std::env::temp_dir().join(format!("prune_days_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let now = SystemTime::now();
        let make = |rel: &str, age: u64| {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let f = std::fs::File::create(&path).unwrap();
            f.set_len(100).unwrap();
            f.set_modified(now - Duration::from_secs(age)).unwrap();
            path
        };
        make("2026-11-21/2130_Jinx_SR/a.mp4", 50);
        make("2026-11-22/0900_Lux_SR/a.mp4", 40);
        make("2026-11-22/1000_Lux_SR/a.mp4", 30);
        let newest = make("2026-11-23/1000_Vi_SR/a.mp4", 10);

        // 400 bytes, limit 250: the 21st's only game goes, and the day with it; then the 22nd's
        // first game, leaving that day its second.
        prune(&dir, 250, &newest);
        assert!(!dir.join("2026-11-21").exists());
        assert!(!dir.join("2026-11-22/0900_Lux_SR").exists());
        assert!(dir.join("2026-11-22/1000_Lux_SR/a.mp4").exists() && newest.exists());
        std::fs::remove_dir_all(dir).unwrap();
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
