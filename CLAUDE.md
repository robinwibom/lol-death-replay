# Notes for agents

- **Checking `ui/player.html`**: run `prototypes/player-harness/get-clip.ps1`, start the `player-harness` preview, and open `/prototypes/player-harness/`. It loads the real player with Tauri stood in for. Check a moment with `window.__drawAt(seconds)`, which returns what the keys shutter shows. Playback is no proof in a hidden pane, where the browser barely runs animation frames.
- **Saved clips**: `%USERPROFILE%\Videos\LoLDeathReplay\<date>\<HHMM_Champion_Mode>\<time>_by-<killer>.mp4`, with recorded keys and clicks in a `.inputs.json` beside each one (format in `src-tauri/src/inputs.rs`).
- **Tests that inject input** (`win.rs`) print `SKIPPED` instead of failing when Windows drops synthetic input (desktop locked, or a game or other elevated window has focus). A skip there says nothing about the code; `cargo test` only shows it with `-- --nocapture`. On CI (`CI` set) they fail instead of skipping.
