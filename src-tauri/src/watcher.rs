//! The background loop: waits for a game, records while it runs, and saves a clip on each death.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

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

    set_status(&app, "Waiting for a game");
    loop {
        let Some(active) = client.get("activeplayer") else {
            if rec.take().is_some() || failed {
                player::close(&app);
                crate::log("Game over. Waiting for the next one...");
                set_status(&app, "Waiting for a game");
            }
            failed = false;
            thread::sleep(Duration::from_secs(2));
            continue;
        };
        if failed {
            thread::sleep(Duration::from_secs(2));
            continue;
        }

        if rec.is_none() {
            let s = settings::current(&app);
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
    if let Err(e) = std::fs::create_dir_all(&s.clips_dir) {
        crate::log(&format!("Couldn't create {}: {e}", s.clips_dir.display()));
        return;
    }
    let out = s.clips_dir.join(name);
    match rec.save_last(s.before + s.after, &out) {
        Ok(()) => {
            crate::log(&format!("Saved {} (killed by {killer})", out.display()));
            *app.state::<AppState>().last_clip.lock().unwrap() = Some(out.clone());
            if s.auto_play {
                player::show(&app, &out, &format!("Death at {min}:{sec:02} - killed by {killer}"), s.after);
            }
            prune(&s.clips_dir, s.keep);
        }
        Err(e) => crate::log(&format!("Clip save failed: {e}")),
    }
}

fn safe(s: &str) -> String {
    let cleaned = Regex::new(r"[^A-Za-z0-9_-]+").unwrap().replace_all(s, "-");
    let cleaned: String = cleaned.trim_matches('-').chars().take(40).collect();
    if cleaned.is_empty() { "unknown".into() } else { cleaned }
}

/// Deletes the oldest clips so at most `keep` remain (0 keeps everything).
fn prune(dir: &Path, keep: usize) {
    if keep == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut clips: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    clips.sort();
    let excess = clips.len().saturating_sub(keep);
    for (_, path) in clips.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}
