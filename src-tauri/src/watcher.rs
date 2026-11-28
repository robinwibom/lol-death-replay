//! The background loop: waits for a game, records while it runs, and saves a clip on each death.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use tauri::{AppHandle, Manager};
use windows_sys::Win32::Foundation::SYSTEMTIME;

use crate::live_client::{
    champion, find_assisters, find_killer, find_player, game_result, is_dead, last_fight, my_names, respawn_in, LiveClient,
};
use crate::recorder::{ffmpeg_path, next_capture, pick_encoder, remove_stale_segments, save_thumbnail, Recorder};
use crate::{game, inputs, player, set_status, settings, toast, win, AppState};

pub fn run(app: AppHandle) {
    remove_stale_segments(); // safe here: single-instance means no other copy is recording
    move_into_day_folders(&settings::current(&app).clips_dir);
    let client = LiveClient::new();
    let ffmpeg = ffmpeg_path();
    let mut auto_encoder: Option<String> = None;
    let mut rec: Option<Arc<Recorder>> = None;
    let mut names = HashSet::new();
    // Where this game's clips go and what's known about it, decided once per game.
    let mut game: Option<game::Current> = None;
    let mut result_saved = false; // the game's result is in its game.json
    let mut polls: u32 = 0;
    let mut was_dead = false;
    let mut close_on_respawn = false; // dead, and the replay closes before respawning
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
            (game, result_saved) = (None, false);
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
                // Picked once: testing the encoders takes a moment.
                auto_encoder.get_or_insert_with(|| pick_encoder(&ffmpeg)).clone()
            } else {
                s.encoder.clone()
            };
            names = my_names(&active);
            crate::log(&format!("Game found - playing as {}", names.iter().min().map_or("?", |n| n)));
            if game.is_none() {
                game = Some(current_game(&client, &names));
            }
            match Recorder::start(&ffmpeg, &s, &encoder) {
                Ok(r) => {
                    let r = Arc::new(r);
                    set_recording(&app, Some((r.clone(), game.clone().unwrap_or_default())));
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

        // A missed answer says nothing: taking it as alive would close the replay early, and the
        // next answer would look like a new death.
        let Some(players) = client.get("playerlist") else {
            thread::sleep(Duration::from_millis(250));
            continue;
        };
        let dead = is_dead(&players, &names);
        if dead && !was_dead {
            crate::log("You died - saving replay...");
            let (app, r, names) = (app.clone(), r.clone(), names.clone());
            let game = game.clone().unwrap_or_default();
            thread::spawn(move || handle_death(app, r, names, game));
            close_on_respawn = true;
        }
        // Out of the way a moment before you can move again. Respawning is the backstop, for a
        // timer that skips past the mark (a revive, say).
        if close_on_respawn && respawning(&players, &names) {
            player::close(&app);
            close_on_respawn = false;
        }
        was_dead = dead;
        // The result only shows in the few seconds between the nexus falling and the game closing,
        // so it's looked for every 2 seconds (the events are a bigger answer than the player list).
        polls = polls.wrapping_add(1);
        if !result_saved && polls.is_multiple_of(8) {
            if let (Some(result), Some((folder, _))) = (game_result(client.get("eventdata").as_ref()), &game) {
                crate::log(&format!("Game ended: {result}"));
                game::set_result(&settings::current(&app).clips_dir.join(folder), &result);
                result_saved = true;
            }
        }
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
/// its own, e.g. "2026-11-21\2130_Jinx_ARAM". With what its game.json starts with.
fn current_game(client: &LiveClient, names: &HashSet<String>) -> game::Current {
    let stats = client.get("gamestats");
    let stat = |key: &str| stats.as_ref().and_then(|g| g.get(key).cloned());
    let game_time = stat("gameTime").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let mode = short_mode(&stat("gameMode").and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default());
    let champ = client.get("playerlist").and_then(|p| find_player(&p, names).and_then(champion));
    let (display, file) = champ.map_or_else(|| ("Unknown".to_string(), "Unknown".to_string()), |c| (c.display, c.file));
    // When the game started, not when we noticed it, so starting the app mid-game names it the same.
    let started = win::local_time(SystemTime::now() - Duration::from_secs_f64(game_time.max(0.0)));
    let info = game::Info { champion: display, mode: mode.clone(), started: iso_time(&started) };
    (game_folder(&started, &file, &mode), info)
}

/// The game modes' short names, as folders and game.json use them.
fn short_mode(mode: &str) -> String {
    match mode {
        "CLASSIC" => "SR",
        "CHERRY" => "Arena",
        "PRACTICETOOL" => "Practice",
        "" => "Unknown",
        other => other,
    }
    .to_string()
}

/// "2026-11-21T21:30:05", local time.
fn iso_time(t: &SYSTEMTIME) -> String {
    format!("{}-{:02}-{:02}T{:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

/// A game that runs past midnight stays in the day it started.
fn game_folder(t: &SYSTEMTIME, champion: &str, mode: &str) -> PathBuf {
    let day = format!("{}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay);
    Path::new(&day).join(safe(&format!("{:02}{:02}_{champion}_{mode}", t.wHour, t.wMinute)))
}

fn handle_death(app: AppHandle, rec: Arc<Recorder>, names: HashSet<String>, (game_folder, info): game::Current) {
    let s = settings::current(&app);
    thread::sleep(Duration::from_secs_f64(s.after)); // capture the death itself
    if !rec.alive() {
        return;
    }
    let client = LiveClient::new();
    let events = client.get("eventdata");
    let players = client.get("playerlist");
    let killer = find_killer(events.as_ref(), players.as_ref(), &names);
    let assisters = find_assisters(events.as_ref(), players.as_ref(), &names);
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
    let out = dir.join(&name);
    match rec.save_last(before + since_death, &out) {
        Ok(()) => {
            crate::log(&format!("Saved {} (killed by {killer})", out.display()));
            *app.state::<AppState>().last_clip.lock().unwrap() = Some(out.clone());
            // A short death timer can run out while the clip saves: it's kept, but not shown.
            let too_late = client.get("playerlist").is_some_and(|p| respawning(&p, &names));
            if too_late && s.auto_play {
                crate::log("Respawned before the replay was ready - not showing it");
            } else if s.auto_play {
                player::show(&app, &out, &format!("Death at {min}:{sec:02} - killed by {killer}"), since_death);
            }
            if s.save_clips {
                let clip = game::Clip { kind: game::Kind::Death, game_time, killer: Some(killer), assisters, starred: false };
                game::add_clip(&dir, &info, &name, clip);
                thumbnail(&out, since_death + THUMBNAIL_BEFORE_DEATH);
                prune_clips(&app, &s, &out);
            } else {
                prune(&dir, 0, &out);
            }
        }
        Err(e) => crate::log(&format!("Clip save failed: {e}")),
    }
}

/// How long before the death a clip's thumbnail is taken: the moment that tells what happened.
const THUMBNAIL_BEFORE_DEATH: f64 = 1.0;

/// Saves a small picture of the clip beside it, `from_end` seconds before its end, for the clip
/// browser. A clip without one still plays; the browser shows a plain tile instead.
fn thumbnail(clip: &Path, from_end: f64) {
    if let Err(e) = save_thumbnail(&ffmpeg_path(), clip, &game::thumbnail(clip), from_end) {
        crate::log(&format!("Couldn't save a thumbnail for {}: {e}", clip.display()));
    }
}

/// Prunes the clips folder down to its size limit, and says so when starred clips had to go too.
fn prune_clips(app: &AppHandle, s: &settings::Settings, newest: &Path) {
    let starred = prune(&s.clips_dir, (s.max_folder_gb * 1e9) as u64, newest);
    if starred > 0 {
        let what = if starred == 1 { "a starred clip".to_string() } else { format!("{starred} starred clips") };
        crate::log(&format!("Deleted {what}: the clips folder was over its limit with only starred clips left"));
        toast::show(app, &format!("Deleted {what} - the clips folder is full"));
    }
}

/// How long before respawning the replay closes, in seconds.
const CLOSE_BEFORE_RESPAWN: f64 = 1.0;

/// Whether the local player is about to respawn or already has: time for the replay to go.
fn respawning(players: &Value, names: &HashSet<String>) -> bool {
    !is_dead(players, names) || respawn_in(players, names).is_some_and(|t| t <= CLOSE_BEFORE_RESPAWN)
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
fn set_recording(app: &AppHandle, now: Option<(Arc<Recorder>, game::Current)>) {
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
    let Some((rec, game)) = app.state::<AppState>().recording.lock().unwrap().clone() else { return };
    crate::log("Save hotkey pressed - saving...");
    let app = app.clone();
    thread::spawn(move || save_moment(app, rec, game));
}

/// Keeps the last `before` seconds as e.g. "12m40s_saved.mp4": no wait for an "after", no replay
/// window, just a toast. Goes to the clips folder even with "Save clips" off, since it was asked for.
fn save_moment(app: AppHandle, rec: Arc<Recorder>, (game_folder, info): game::Current) {
    let s = settings::current(&app);
    let game_time = game_time(&LiveClient::new());
    let name = format!("{}_saved.mp4", clip_time(game_time));
    let dir = s.clips_dir.join(&game_folder);
    let out = dir.join(&name);
    let saved = std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Couldn't create {}: {e}", dir.display()))
        .and_then(|()| rec.save_last(s.before, &out));
    match saved {
        Ok(()) => {
            crate::log(&format!("Saved {}", out.display()));
            toast::show(&app, "Saved");
            let clip = game::Clip { kind: game::Kind::Saved, game_time, killer: None, assisters: vec![], starred: false };
            game::add_clip(&dir, &info, &name, clip);
            thumbnail(&out, SAVED_THUMBNAIL_FROM_END);
            prune_clips(&app, &s, &out);
        }
        Err(e) => {
            crate::log(&format!("Clip save failed: {e}"));
            toast::show(&app, "Couldn't save - see log");
        }
    }
}

/// A saved clip's thumbnail: just before the key was pressed.
const SAVED_THUMBNAIL_FROM_END: f64 = 2.0;

/// Moves game folders from older versions' layout ("2026-11-21_2130_Jinx_SR", right in the clips
/// folder) into day folders ("2026-11-210_Jinx_SR"). Renames on the same drive: nothing is
/// copied. One that can't move now (a clip in it is open, say) is tried again on the next start.
fn move_into_day_folders(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut moved = 0;
    for path in entries.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()) {
        let Some((day, game)) = path.file_name().and_then(|n| n.to_str()).and_then(old_game_folder) else { continue };
        // The clips folder may be one the user keeps other things in: only clips folders move
        // (clips, and the keys and clicks saved beside them).
        let only_clips = std::fs::read_dir(&path).is_ok_and(|mut files| {
            files.all(|f| f.is_ok_and(|f| f.path().is_file() && is_clip_file(&f.path())))
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

/// A clip, or the inputs saved beside one.
fn is_clip_file(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    name.ends_with(".mp4") || name.ends_with(".inputs.json")
}

/// "2026-11-21_2130_Jinx_SR" ->("2026-11-21", "2130_Jinx_SR"); None for any other name.
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

/// Deletes clips until the folder holds at most `max_bytes`, and returns how many starred ones had
/// to go. Unstarred clips go first, oldest first, whatever their game; starred ones only once no
/// unstarred clip is left to delete. A clip counts and goes with its inputs and thumbnail, and
/// leaves its game's game.json. A game folder goes with its last clip, a day folder with its last game.
/// Clips directly in `dir` stand alone (how the unsaved-replays folder holds them).
/// Nothing in the game holding `newest` (the clip just saved) is deleted, even if it alone is over the limit.
fn prune(dir: &Path, max_bytes: u64, newest: &Path) -> usize {
    let game_of = |clip: &Path| clip.parent().filter(|parent| *parent != dir).map(Path::to_path_buf);
    let name_of = |clip: &Path| clip.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let beside = |clip: &Path| [inputs::sidecar(clip), game::thumbnail(clip)];
    let current = game_of(newest);
    let mut stars: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    let mut found: Vec<(bool, SystemTime, u64, PathBuf)> = clips(dir)
        .into_iter()
        .map(|(modified, size, path)| {
            let extra: u64 = beside(&path).iter().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum();
            let starred = game_of(&path)
                .is_some_and(|game| stars.entry(game.clone()).or_insert_with(|| game::starred(&game)).contains(&name_of(&path)));
            (starred, modified, size + extra, path)
        })
        .collect();
    found.sort(); // unstarred before starred, then oldest first
    let mut total: u64 = found.iter().map(|(_, _, size, _)| size).sum();
    let mut starred_deleted = 0;
    for (starred, _, size, path) in found {
        if total <= max_bytes {
            break;
        }
        let game = game_of(&path);
        if path == newest || (game.is_some() && game == current) {
            continue;
        }
        if std::fs::remove_file(&path).is_err() {
            continue; // open somewhere, say: tried again next time
        }
        for file in beside(&path) {
            let _ = std::fs::remove_file(file);
        }
        total -= size;
        starred_deleted += usize::from(starred);
        let Some(game) = game else { continue };
        let clips_left = std::fs::read_dir(&game)
            .is_ok_and(|mut files| files.any(|f| f.is_ok_and(|f| f.path().extension().is_some_and(|x| x == "mp4"))));
        if clips_left {
            game::remove_clip(&game, &name_of(&path));
        } else if std::fs::remove_dir_all(&game).is_ok() {
            if let Some(day) = game.parent().filter(|day| *day != dir) {
                let _ = std::fs::remove_dir(day); // only removes an empty folder
            }
        }
    }
    starred_deleted
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
        let folder = |champion: &str, mode: &str| game_folder(&t, champion, &short_mode(mode));
        assert_eq!(folder("Jinx", "CLASSIC"), day.join("0905_Jinx_SR"));
        assert_eq!(folder("MissFortune", "ARAM"), day.join("0905_MissFortune_ARAM"));
        assert_eq!(folder("Jinx", ""), day.join("0905_Jinx_Unknown"));
        // Only the game's part is cleaned: the day's separator stays a separator.
        assert_eq!(folder("Jinx", "Odd/Mode"), day.join("0905_Jinx_Odd-Mode"));
        // game.json keeps the start to the second.
        assert_eq!(iso_time(&t), "2026-11-01T09:05:05");
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
        put("2026-11-21_2210_Lux_ARAM/01m00s_saved.inputs.json"); // a clip's inputs move with it
        put("2026-11-22_0900_Jinx_SR/03m00s_by-Vi.mp4");
        put("2026-11-22_1200_photos_trip/beach.jpg"); // not ours: has something besides clips
        put("2026-11-23_1000_Ezreal_SR/05m00s_by-Vi.mp4"); // its new place is already taken
        put("2026-11-23/1000_Ezreal_SR/01m00s_by-Lux.mp4");

        move_into_day_folders(&dir);
        for moved in ["2026-11-21/2130_Jinx_SR/12m40s_by-Ahri.mp4", "2026-11-21/2210_Lux_ARAM/01m00s_saved.mp4",
            "2026-11-21/2210_Lux_ARAM/01m00s_saved.inputs.json", "2026-11-22/0900_Jinx_SR/03m00s_by-Vi.mp4"] {
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

    /// A file of `size` bytes at `dir/rel`, last changed `age` seconds ago.
    fn file(dir: &Path, rel: &str, size: u64, age: u64) -> PathBuf {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(size).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(age)).unwrap();
        path
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("prune_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Records `clip` in its folder's game.json, starred or not.
    fn record(clip: &Path, starred: bool) {
        let entry = game::Clip { kind: game::Kind::Death, game_time: 60, killer: None, assisters: vec![], starred };
        game::add_clip(clip.parent().unwrap(), &game::Info::default(), &clip.file_name().unwrap().to_string_lossy(), entry);
    }

    #[test]
    fn prune_deletes_oldest_clips_first_and_empty_folders_after() {
        let dir = temp_dir("oldest");
        let jinx = file(&dir, "2026-11-21/2130_Jinx_SR/a.mp4", 100, 60);
        let lux_a = file(&dir, "2026-11-22/0900_Lux_SR/a.mp4", 100, 50);
        let lux_b = file(&dir, "2026-11-22/0900_Lux_SR/b.mp4", 100, 40);
        let newest = file(&dir, "2026-11-23/1000_Vi_SR/a.mp4", 100, 10);
        for clip in [&jinx, &lux_a, &lux_b, &newest] {
            record(clip, false);
        }
        // A clip's inputs and thumbnail count toward the limit and go with it.
        file(&dir, "2026-11-22/0900_Lux_SR/a.inputs.json", 50, 50);
        file(&dir, "2026-11-22/0900_Lux_SR/a.jpg", 50, 50);

        // 500 bytes, limit 250: the 21st's only clip goes, its game and day with it; then Lux's
        // older clip (200 with its files), leaving that game its newer one and a game.json without a.
        assert_eq!(prune(&dir, 250, &newest), 0);
        assert!(!dir.join("2026-11-21").exists());
        let lux = dir.join("2026-11-22/0900_Lux_SR");
        assert!(!lux_a.exists() && !lux.join("a.inputs.json").exists() && !lux.join("a.jpg").exists());
        assert!(lux_b.exists() && newest.exists());
        assert_eq!(game::read(&lux).unwrap().clips.keys().collect::<Vec<_>>(), ["b.mp4"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prune_keeps_starred_clips_until_nothing_else_is_left() {
        let dir = temp_dir("stars");
        let old_star = file(&dir, "2026-11-20/1000_Jinx_SR/a.mp4", 100, 90);
        let old_plain = file(&dir, "2026-11-20/1000_Jinx_SR/b.mp4", 100, 80);
        let new_star = file(&dir, "2026-11-21/1000_Lux_SR/a.mp4", 100, 30);
        let new_plain = file(&dir, "2026-11-21/1000_Lux_SR/b.mp4", 100, 20);
        let current = file(&dir, "2026-11-22/1000_Vi_SR/a.mp4", 100, 15);
        let newest = file(&dir, "2026-11-22/1000_Vi_SR/b.mp4", 100, 10);
        for (clip, starred) in [(&old_star, true), (&old_plain, false), (&new_star, true), (&new_plain, false), (&current, false), (&newest, false)] {
            record(clip, starred);
        }

        // 600 bytes, limit 400: both unstarred clips go, the newer one before any starred clip,
        // however old that is.
        assert_eq!(prune(&dir, 400, &newest), 0);
        assert!(!old_plain.exists() && !new_plain.exists());
        assert!(old_star.exists() && new_star.exists());

        // Limit 300: only starred clips (and the current game's) are left, so the oldest starred one goes.
        assert_eq!(prune(&dir, 300, &newest), 1);
        assert!(!old_star.exists() && !dir.join("2026-11-20").exists() && new_star.exists());

        // The current game is kept whole, even when it alone is over the limit.
        assert_eq!(prune(&dir, 0, &newest), 1);
        assert!(!new_star.exists() && current.exists() && newest.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prune_treats_loose_clips_one_by_one() {
        // The unsaved-replays folder: clips right in it, each with its own inputs, no game.json.
        let dir = temp_dir("loose");
        let old = file(&dir, "old.mp4", 100, 30);
        let inputs = file(&dir, "old.inputs.json", 10, 30);
        let other = file(&dir, "other.inputs.json", 10, 30); // another clip's: stays
        let newest = file(&dir, "new.mp4", 100, 10);
        assert_eq!(prune(&dir, 0, &newest), 0);
        assert!(!old.exists() && !inputs.exists() && other.exists() && newest.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
