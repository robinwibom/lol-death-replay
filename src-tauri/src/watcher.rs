//! The background loop: waits for a game, records while it runs, and saves a clip on each death.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

use regex::Regex;
use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::live_client::{find_killer, is_dead, my_names, LiveClient};
use crate::recorder::{ffmpeg_path, pick_encoder, Recorder};
use crate::{player, set_status, settings, AppState};

pub fn run(app: AppHandle) {
    let client = LiveClient::new();
    let ffmpeg = ffmpeg_path();
    let mut auto_encoder: Option<String> = None;
    let mut rec: Option<Arc<Recorder>> = None;
    let mut names = HashSet::new();
    let mut was_dead = false;
    let mut failed = false; // don't retry recording until the next game
    let mut idle = false; // in a game, but replay and clips are both turned off
    let mut compatible = false; // fast capture failed this game, so record with gdigrab instead

    set_status(&app, "Waiting for game");
    loop {
        let Some(active) = client.get("activeplayer") else {
            if rec.take().is_some() || failed || idle {
                player::close(&app);
                crate::log("Game over. Waiting for the next one...");
                set_status(&app, "Waiting for game");
            }
            (failed, idle, compatible) = (false, false, false);
            thread::sleep(Duration::from_secs(2));
            continue;
        };
        if failed {
            thread::sleep(Duration::from_secs(2));
            continue;
        }

        if rec.is_none() {
            let mut s = settings::current(&app);
            if compatible {
                s.capture = "gdigrab".into();
            }
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
                auto_encoder.get_or_insert_with(|| pick_encoder(&ffmpeg, "auto")).clone()
            } else {
                s.encoder.clone()
            };
            names = my_names(&active);
            crate::log(&format!("Game found - playing as {}", names.iter().min().map_or("?", |n| n)));
            match Recorder::start(&ffmpeg, &s, &encoder) {
                Ok(r) => {
                    rec = Some(Arc::new(r));
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
            if !compatible && settings::current(&app).capture != "gdigrab" {
                // Desktop Duplication doesn't work on every setup (some dual-GPU laptops, remote desktop).
                crate::log("Fast capture failed - switching to compatible capture for this game");
                compatible = true;
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
            thread::spawn(move || handle_death(app, r, names));
        }
        was_dead = dead;
        thread::sleep(Duration::from_millis(250));
    }
}

fn handle_death(app: AppHandle, rec: Arc<Recorder>, names: HashSet<String>) {
    let s = settings::current(&app);
    thread::sleep(Duration::from_secs_f64(s.after)); // capture the death itself
    if !rec.alive() {
        return;
    }
    let client = LiveClient::new();
    let killer = find_killer(client.get("eventdata").as_ref(), &names);
    let game_time = client
        .get("gamestats")
        .and_then(|g| g.get("gameTime").and_then(Value::as_f64))
        .unwrap_or(0.0) as u64;
    let (min, sec) = (game_time / 60, game_time % 60);
    let name = format!(
        "{}_{min:02}m{sec:02}s_by-{}.mp4",
        chrono::Local::now().format("%Y-%m-%d_%H%M%S"),
        safe(&killer)
    );
    // Unsaved replays still need a file to play; they go to a temp folder that only holds the latest.
    let dir = if s.save_clips { s.clips_dir.clone() } else { unsaved_dir() };
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
                prune(&dir, (s.max_folder_gb * 1e9) as u64, &out);
            } else {
                prune(&dir, 0, &out);
            }
        }
        Err(e) => crate::log(&format!("Clip save failed: {e}")),
    }
}

fn unsaved_dir() -> PathBuf {
    std::env::temp_dir().join("LoL Death Replay (unsaved)")
}

fn safe(s: &str) -> String {
    let cleaned = Regex::new(r"[^A-Za-z0-9_-]+").unwrap().replace_all(s, "-");
    let cleaned: String = cleaned.trim_matches('-').chars().take(40).collect();
    if cleaned.is_empty() { "unknown".into() } else { cleaned }
}

/// Deletes the oldest clips until the folder's clips total at most `max_bytes`.
/// `newest` (the clip just saved) is never deleted, even if it alone is over the limit.
fn prune(dir: &Path, max_bytes: u64, newest: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut clips: Vec<(SystemTime, u64, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some((meta.modified().ok()?, meta.len(), e.path()))
        })
        .collect();
    clips.sort(); // oldest first
    let mut total: u64 = clips.iter().map(|(_, size, _)| size).sum();
    for (_, size, path) in clips {
        if total <= max_bytes {
            break;
        }
        if path != newest && std::fs::remove_file(&path).is_ok() {
            total -= size;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_removes_oldest_until_under_limit() {
        let dir = std::env::temp_dir().join(format!("prune_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now();
        let mut paths = vec![];
        for i in 0..4u64 {
            let path = dir.join(format!("clip{i}.mp4"));
            let f = std::fs::File::create(&path).unwrap();
            f.set_len(100).unwrap();
            f.set_modified(now - Duration::from_secs(100 - i)).unwrap(); // clip0 is the oldest
            paths.push(path);
        }
        prune(&dir, 250, &paths[3]);
        let left: Vec<bool> = paths.iter().map(|p| p.exists()).collect();
        assert_eq!(left, [false, false, true, true]);

        prune(&dir, 0, &paths[3]); // limit 0: only the newest survives
        assert!(!paths[2].exists() && paths[3].exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
