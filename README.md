# LoL Death Replay

When you die in League of Legends, a small replay window pops up over the game and plays the moments leading up to your death, so you can see what went wrong while you wait to respawn. If you died in a fight, the clip covers the whole fight. Every death is also saved as a clip, named with the game time and who killed you.

It runs quietly in the system tray. It starts recording when a match begins and stops when it ends.

## Install

1. Download the installer (`LoLDeathReplay_x.y.z_x64-setup.exe`) from the [latest release](../../releases/latest) and run it. Everything it needs, ffmpeg included, comes with it.
2. In League, set **Settings → Video → Window Mode** to **Borderless**, so the replay can appear on top of the game.

The installer isn't code-signed. Windows SmartScreen may warn about it (choose **More info → Run anyway**), and PCs with **Smart App Control** turned on will block it.

## Use

Start **LoLDeathReplay** from the Start menu (or turn on **Start with Windows** in its settings). A gold replay icon appears in the tray; click it for:

- **Replay last death**
- **Open clips folder** (by default `Videos\LoLDeathReplay`)
- **Settings**
- **Quit**

### The replay window

It stays on top of the game and never takes your keyboard away from it. Move the mouse over it to show the controls:

- **Drag the video** to move the window, and drag any **edge or corner** to resize it. It opens in the same place next time. Click the video to pause or play.
- **Timeline**: playback starts 5 seconds before the death; drag back to see the rest of the clip. The gold **DEATH** section at the end is the interesting part: from about 3 seconds before you died to the end of the clip.
- **Bottom row**: back 5s, play/pause and forward 5s; on the right, sound (click the speaker to mute, hover it for the volume, both remembered) and playback speed (0.25× to 2×).
- **X** (top right) closes it. **Ctrl+Shift+X** also closes it from inside the game.

It closes by itself the next time you die (replaced by the new replay) and when the game ends.

### Saving a moment

Not every moment worth keeping is a death. During a game, press **Ctrl+Shift+S** to save the last 15 seconds (the same length as a death replay) as a clip like `12m40s_saved.mp4` in that game's folder. A small "Saved" note shows briefly at the top of the screen; no replay window opens. These clips are saved even with **Save clips** turned off. The hotkey can be changed or turned off in settings. It only works while a game is being recorded, and it never takes the keys from other apps: the app in front still gets them too.

### Settings

Whether to show the replay and whether to save clips (turn both off and it doesn't record at all), how many seconds to keep before and after a death, **Include the whole fight** (when you die in a fight, the clip goes back to where the fight started, up to 60 seconds before the death), recording quality (720p to native, 30 or 60 fps), **Record game sound** (only the game's own sound: voice chat, music and other apps stay out of clips), the clips folder and its size limit (oldest clips are deleted first). Recording changes (sound included), and turning on **Include the whole fight**, apply from the next game.

## Troubleshooting

- **Replays are black**: check the game is in Borderless mode. If they still are, close the app, set `"capture": "gdigrab"` in `%APPDATA%\io.github.robinwibom.loldeathreplay\settings.json` and start it again. (The app already switches to that by itself when fast capture fails outright.)
- **The game stutters**: lower **Quality** to 720p. The app uses your graphics card's video encoder (NVIDIA, AMD or Intel); the log says which one.
- **Replay doesn't appear over the game**: use Borderless mode; true fullscreen hides other windows.
- Anything else: **Settings → Open log**.

## How it works

Deaths are detected with Riot's official [Live Client Data API](https://developer.riotgames.com/docs/lol#game-client-api), which the game serves on `https://127.0.0.1:2999` during a match. The app only reads that API and records your screen with ffmpeg, the same way OBS does. It never touches the game client.

While a match runs, ffmpeg streams the recording into the app, which keeps only the last stretch of it in memory (about 30 MB at 1080p, or 90 MB with **Include the whole fight** on, which keeps a full minute, plus 4 or 12 MB of sound). Nothing is written to disk until a clip is saved.

Sound is recorded by the app itself, with Windows' per-process loopback (Windows 10 2004 or newer): only what the game plays, the same audio Windows sends to your speakers. Where that isn't available, it records everything the speakers play instead, and the log says so. The sound is kept beside the video and only joined to it when a clip is saved: the app notes when each bit of sound was heard and works out when each video frame was captured, so they line up to within a few milliseconds. Clips carry it as AAC at 128 kbps, about 1 MB a minute. On a death, the app waits 2 seconds, writes the last 17 seconds to an `.mp4` and opens it in the replay window. With **Include the whole fight** on, it first looks back through the game's kill events for the fight that ended in your death: earlier kills less than 10 seconds apart that share a player (killer, victim or assister) with the fight count as part of it. The clip then starts 5 seconds before the fight's first kill, up to 60 seconds before the death.

## Building from source

Needs [Rust](https://rustup.rs), the Visual Studio C++ Build Tools, and the Tauri CLI (`cargo install tauri-cli --version "^2" --locked`).

1. Put an `ffmpeg.exe` at `src-tauri/binaries/ffmpeg-x86_64-pc-windows-msvc.exe`. Any recent Windows build works for development, e.g. from [BtbN](https://github.com/BtbN/FFmpeg-Builds/releases). Releases ship a much smaller one built by `scripts/build-ffmpeg.sh`, which cross-compiles from Linux or WSL (MinGW-w64 13 or newer, e.g. Ubuntu 26.04) and enables only what the app uses. If you use a new ffmpeg feature, enable it there and add it to `scripts/test-ffmpeg.ps1`, which checks an `ffmpeg.exe` has everything the app needs.
2. Run it: `cd src-tauri` then `cargo run`
3. Build the installer: `cargo tauri build` (output in `src-tauri/target/release/bundle/nsis/`)

Releases are built by GitHub Actions: push a tag like `v0.1.0` and `.github/workflows/release.yml` publishes the installer.

Layout:

- `src-tauri/src/`: the Rust app. `watcher.rs` is the main loop, `recorder.rs` drives ffmpeg, `audio.rs` records the game's sound, `live_client.rs` talks to the game, `player.rs` opens the replay window, `settings.rs` stores settings, `win.rs` holds small Windows helpers (start with Windows, the hotkey, local time).
- `ui/`: `player.html` (replay window) and `settings.html`.

## License

GPL-3.0. The installer bundles a GPL build of [ffmpeg](https://ffmpeg.org).

League of Legends is a trademark of Riot Games, Inc. This project isn't endorsed by or affiliated with Riot Games.
