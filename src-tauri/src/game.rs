//! A game's own record: `game.json` in its folder, beside its clips. The clip browser and the
//! post-game review read it (docs/clip-browser.md), and pruning reads its stars.
//!
//! It's written when the game's first clip is saved, updated with each clip after that, and once more
//! when the game ends (its result). Writes go to a temporary file that then replaces the old one, so
//! a crash leaves the previous version rather than half a file. Clips are saved from their own
//! threads, so every change reads, edits and writes under one lock.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

const FILE: &str = "game.json";

/// What `game.json` holds. Field names are camelCase in the file, for the pages that read it.
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Game {
    pub version: u32,
    /// As the game shows it: "Miss Fortune".
    pub champion: String,
    /// As in the folder's name: "SR", "ARAM", "Arena", "Practice".
    pub mode: String,
    /// When the game started, local time: "2026-11-27T21:00:00".
    pub started: String,
    /// "Win" or "Lose", when the game said so before it closed.
    #[serde(default)]
    pub result: Option<String>,
    /// Set by the post-game review once every death has been seen.
    #[serde(default)]
    pub reviewed: bool,
    /// By file name ("08m40s_by-Zed.mp4"), so they're in game-time order.
    #[serde(default)]
    pub clips: BTreeMap<String, Clip>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Clip {
    pub kind: Kind,
    /// Seconds into the game.
    pub game_time: u64,
    /// What killed you, as the game shows it ("Zed", "Ocean Drake", "Turret"). Deaths only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub killer: Option<String>,
    /// The champions that helped, if any. Deaths only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assisters: Vec<String>,
    #[serde(default)]
    pub starred: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Death,
    /// Kept with the save hotkey.
    Saved,
}

/// What's known about a game when its first clip is saved.
#[derive(Debug, Clone, Default)]
pub struct Info {
    pub champion: String,
    pub mode: String,
    pub started: String,
}

/// The game being recorded: its folder (inside the clips folder) and what its game.json starts with.
pub type Current = (PathBuf, Info);

static LOCK: Mutex<()> = Mutex::new(());

fn path(folder: &Path) -> PathBuf {
    folder.join(FILE)
}

/// A game folder's record, if it has a readable one.
pub fn read(folder: &Path) -> Option<Game> {
    let text = std::fs::read_to_string(path(folder)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write(folder: &Path, game: &Game) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(game).map_err(std::io::Error::other)?;
    let tmp = folder.join(format!("{FILE}.tmp"));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path(folder)) // replaces the old file
}

/// Reads, changes and writes a game's record under the lock. `start` is used when there's none yet
/// (or it can't be read); with no `start`, a missing record stays missing.
fn change(folder: &Path, start: Option<&Info>, edit: impl FnOnce(&mut Game)) {
    let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut game = match (read(folder), start) {
        (Some(game), _) => game,
        (None, Some(info)) => Game {
            version: 1,
            champion: info.champion.clone(),
            mode: info.mode.clone(),
            started: info.started.clone(),
            ..Game::default()
        },
        (None, None) => return,
    };
    edit(&mut game);
    if let Err(e) = write(folder, &game) {
        crate::log(&format!("Couldn't write {}: {e}", path(folder).display()));
    }
}

/// Records a clip just saved into `folder`, creating the game's record with its first clip.
pub fn add_clip(folder: &Path, info: &Info, file_name: &str, clip: Clip) {
    change(folder, Some(info), |game| {
        game.clips.insert(file_name.to_string(), clip);
    });
}

/// Records how the game ended. Does nothing for a game that saved no clips.
pub fn set_result(folder: &Path, result: &str) {
    change(folder, None, |game| game.result = Some(result.to_string()));
}

/// Forgets a clip that was deleted.
pub fn remove_clip(folder: &Path, file_name: &str) {
    change(folder, None, |game| {
        game.clips.remove(file_name);
    });
}

/// The file names of a game folder's starred clips.
pub fn starred(folder: &Path) -> HashSet<String> {
    read(folder)
        .map(|g| g.clips.into_iter().filter(|(_, c)| c.starred).map(|(name, _)| name).collect())
        .unwrap_or_default()
}

/// A clip's thumbnail, saved beside it: "08m40s_by-Zed.mp4" -> "08m40s_by-Zed.jpg".
pub fn thumbnail(clip: &Path) -> PathBuf {
    clip.with_extension("jpg")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("game_json_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn info() -> Info {
        Info { champion: "Miss Fortune".into(), mode: "SR".into(), started: "2026-11-27T21:00:00".into() }
    }

    fn death(t: u64, killer: &str) -> Clip {
        Clip { kind: Kind::Death, game_time: t, killer: Some(killer.into()), assisters: vec!["Ahri".into()], starred: false }
    }

    #[test]
    fn first_clip_creates_the_record_and_later_ones_join_it() {
        let dir = temp("create");
        add_clip(&dir, &info(), "08m40s_by-Zed.mp4", death(520, "Zed"));
        add_clip(&dir, &Info::default(), "15m22s_saved.mp4", Clip { kind: Kind::Saved, game_time: 922, killer: None, assisters: vec![], starred: false });
        let game = read(&dir).unwrap();
        // The game's details come from the first clip; a later clip's don't overwrite them.
        assert_eq!((game.version, game.champion.as_str(), game.mode.as_str()), (1, "Miss Fortune", "SR"));
        assert_eq!(game.clips.len(), 2);
        assert_eq!(game.clips["08m40s_by-Zed.mp4"], death(520, "Zed"));
        assert!(!dir.join("game.json.tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_file_is_camel_case_and_leaves_out_what_a_saved_clip_lacks() {
        let dir = temp("shape");
        add_clip(&dir, &info(), "15m22s_saved.mp4", Clip { kind: Kind::Saved, game_time: 922, killer: None, assisters: vec![], starred: true });
        let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("game.json")).unwrap()).unwrap();
        let clip = &json["clips"]["15m22s_saved.mp4"];
        assert_eq!(clip, &serde_json::json!({ "kind": "saved", "gameTime": 922, "starred": true }));
        assert_eq!(json["result"], serde_json::Value::Null);
        assert_eq!(json["reviewed"], false);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn result_and_removal_need_a_record() {
        let dir = temp("result");
        set_result(&dir, "Win"); // no clips saved: nothing written
        assert!(read(&dir).is_none());
        add_clip(&dir, &info(), "a.mp4", death(60, "Vi"));
        add_clip(&dir, &info(), "b.mp4", death(120, "Vi"));
        set_result(&dir, "Win");
        remove_clip(&dir, "a.mp4");
        let game = read(&dir).unwrap();
        assert_eq!(game.result.as_deref(), Some("Win"));
        assert_eq!(game.clips.keys().collect::<Vec<_>>(), ["b.mp4"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reads_stars() {
        let dir = temp("stars");
        assert!(starred(&dir).is_empty()); // no record: nothing starred
        add_clip(&dir, &info(), "a.mp4", Clip { starred: true, ..death(60, "Vi") });
        add_clip(&dir, &info(), "b.mp4", death(120, "Vi"));
        assert_eq!(starred(&dir), HashSet::from(["a.mp4".to_string()]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unreadable_record_starts_over() {
        let dir = temp("broken");
        std::fs::write(dir.join("game.json"), "{ half a fi").unwrap();
        assert!(read(&dir).is_none());
        add_clip(&dir, &info(), "a.mp4", death(60, "Vi"));
        assert_eq!(read(&dir).unwrap().clips.len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
