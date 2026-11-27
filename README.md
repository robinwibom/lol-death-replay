# LoL Death Replay

When you die in League of Legends, a small replay window pops up over the game and plays the moments leading up to your death, so you can see what went wrong while you wait to respawn. Turn on **Include the whole fight** and a death that ends a fight gets a clip of the whole fight. Every death is also saved as a clip, named with the game time and who killed you.

It runs quietly in the system tray. It starts recording when a match begins and stops when it ends.

## Install

1. Download the installer (`LoLDeathReplay_x.y.z_x64-setup.exe`) from the [latest release](../../releases/latest) and run it. Everything it needs, ffmpeg included, comes with it.
2. In League, set **Settings → Video → Window Mode** to **Borderless**, so the replay can appear on top of the game.

The installer isn't code-signed. Windows SmartScreen may warn about it (choose **More info → Run anyway**), and PCs with **Smart App Control** turned on will block it.

## Use

Start **LoLDeathReplay** from the Start menu (it starts with Windows by default; turn off **Start with Windows** in its settings to stop that). A gold replay icon appears in the tray; click it for:

- **Replay last death**
- **Open clips folder** (by default `Videos\LoLDeathReplay`)
- **Settings**
- **Quit**

Clips are kept in a folder per day, with a folder per game inside it, named by when the game started: for example `2026-11-24\1849_Caitlyn_SR\12m40s_by-Ahri.mp4`. Game folders from older versions, which sat directly in the clips folder, are moved into their day's folder the next time the app starts.

### The replay window

It stays on top of the game and never takes your keyboard away from it. Move the mouse over it to show the controls:

- **Drag the video** to move the window, and drag any **edge or corner** to resize it. It opens in the same place next time. Click the video to pause or play.
- **Timeline**: playback starts 5 seconds before the death; drag back to see the rest of the clip. The gold **DEATH** section at the end is the interesting part: from about 3 seconds before you died to the end of the clip.
- **Bottom row**: back 5s, play/pause and forward 5s; on the right, sound (click the speaker to mute, hover it for the volume, both remembered; it's greyed out when the clip has no sound, e.g. with **Record game sound** off) and playback speed (0.25× to 2×). On a clip with keys and clicks, a keyboard button there hides or shows them (remembered).
- **Keys and clicks** (with **Record keys and clicks** on): a strip along the bottom (Q W E R, D F, 1 to 7 and B) lights up each key while it's held, and a ripple marks each click where it was (gold for left, green for right). It follows the video, slow motion and scrubbing included. Keys are shown as on the keyboard, not as abilities. While chat was open the strip says CHAT, since nothing typed there is kept.
- **X** (top right) closes it. **Ctrl+Shift+X** also closes it from inside the game.

It closes by itself a second before you respawn, the next time you die (replaced by the new replay) and when the game ends.

### Saving a moment

Not every moment worth keeping is a death. During a game, press **Ctrl+Shift+S** to save the last 12 seconds (the same length as a death replay) as a clip like `12m40s_saved.mp4` in that game's folder. A small "Saved" note shows briefly at the top of the screen; no replay window opens. These clips are saved even with **Save clips** turned off. The hotkey can be changed or turned off in settings. It only works while a game is being recorded, and it never takes the keys from other apps: the app in front still gets them too.

### Settings

Whether to show the replay and whether to save clips (turn both off and it doesn't record at all), how many seconds to keep before and after a death, **Include the whole fight** (off by default; when you die in a fight, the clip goes back to where the fight started, up to 60 seconds before the death), recording quality (720p to native; 24, 30, 60 or 120 fps: 24 makes clips about 15% smaller but fast moves look choppier, and 120 only helps on a monitor and game running at 120 or more, with clips and memory use growing to match), **Record game sound** (only the game's own sound: voice chat, music and other apps stay out of clips), **Record keys and clicks** (off by default; see How it works), the clips folder and its size limit (up to 20 GB; oldest clips are deleted first). Recording changes (sound included), and turning on **Include the whole fight**, apply from the next game.

Clips are AV1 on graphics cards that encode it (NVIDIA RTX 40 or newer, AMD RX 7000 or newer, Intel Arc or Core Ultra) and H.264 on older ones. AV1 looks the same in smaller clips. It plays in browsers, Discord and the replay window; Windows' Photos and Media Player need the free AV1 Video Extension from the Microsoft Store. If clips go somewhere that can't play AV1, set `"encoder": "h264_nvenc"` (or `h264_amf`, `h264_qsv` on AMD or Intel) in `%APPDATA%\io.github.robinwibom.loldeathreplay\settings.json` while the app is closed: clips get larger but play everywhere.

## Troubleshooting

- **Replays are black**: check the game is in Borderless mode. If they still are, close the app, set `"capture": "gdigrab"` in `%APPDATA%\io.github.robinwibom.loldeathreplay\settings.json` and start it again. (The app already switches to that by itself when fast capture fails outright.)
- **The game stutters**: lower **Quality** to 720p. The app uses your graphics card's video encoder (NVIDIA, AMD or Intel), AV1 when it has one; the log says which one.
- **Replay doesn't appear over the game**: use Borderless mode; true fullscreen hides other windows.
- Anything else: **Settings → Open log**.

## How it works

Deaths are detected with Riot's official [Live Client Data API](https://developer.riotgames.com/docs/lol#game-client-api), which the game serves on `https://127.0.0.1:2999` during a match. The app only reads that API and records your screen with ffmpeg, the same way OBS does. It never touches the game client.

While a match runs, ffmpeg streams the recording into the app, which keeps only the last stretch of it in memory (about 20 MB at 1080p in H.264, less in AV1, or 55 MB with **Include the whole fight** on, which keeps a full minute, plus 4 or 12 MB of sound). Nothing is written to disk until a clip is saved, and then the video is copied into the `.mp4` as it is, never encoded twice. The stream is Matroska, which carries AV1 as well as H.264, one frame per cluster, so a clip can start exactly on a keyframe.

Sound is recorded by the app itself, with Windows' per-process loopback (Windows 10 2004 or newer): only what the game plays, the same audio Windows sends to your speakers. Where that isn't available, it records everything the speakers play instead, and the status says so (voice chat may be included). The app keeps looking for the game's process for up to two minutes, in case it starts after the game is reported. The sound is kept beside the video and only joined to it when a clip is saved: the app notes when each bit of sound was heard and works out when each video frame was captured, so they line up to within about 20 milliseconds. Clips carry it as AAC at 128 kbps, about 1 MB a minute. On a death, the app waits 1 second, writes the last 13 seconds to an `.mp4` and opens it in the replay window. With **Include the whole fight** on, it first looks back through the game's kill events for the fight that ended in your death: earlier kills less than 10 seconds apart that share a player (killer, victim or assister) with the fight count as part of it. The clip then starts 5 seconds before the fight's first kill, up to 60 seconds before the death.

Keys and clicks (with **Record keys and clicks** on) are read with Windows' Raw Input, like the save hotkey: Windows hands the app a copy of each key and mouse button, nothing is hooked or injected, and the game gets its input exactly as before. Only what happens while the game is in front is kept, and only clicks on the game itself. Chat is never kept: from the Enter that opens it to the Enter or Escape that closes it, no key at all is recorded, only that chat was open. Like the video, only the last stretch is kept, in memory. A saved clip gets them in a small file beside it, e.g. `12m40s_by-Ahri.inputs.json`, timed from the clip's first frame the same way the sound is, so they line up with the video to within a few tens of milliseconds. The folder size limit deletes it together with its clip.

## Building from source

Needs [Rust](https://rustup.rs), the Visual Studio C++ Build Tools, and the Tauri CLI (`cargo install tauri-cli --version "^2" --locked`).

1. Put an `ffmpeg.exe` at `src-tauri/binaries/ffmpeg-x86_64-pc-windows-msvc.exe`. Any recent Windows build works for development, e.g. from [BtbN](https://github.com/BtbN/FFmpeg-Builds/releases). Releases ship a much smaller one built by `scripts/build-ffmpeg.sh`, which cross-compiles from Linux or WSL (MinGW-w64 13 or newer, e.g. Ubuntu 26.04) and enables only what the app uses. If you use a new ffmpeg feature, enable it there and add it to `scripts/test-ffmpeg.ps1`, which checks an `ffmpeg.exe` has everything the app needs.
2. Run it: `cd src-tauri` then `cargo run`
3. Build the installer: `cargo tauri build` (output in `src-tauri/target/release/bundle/nsis/`)

Releases are built by GitHub Actions: push a tag like `v0.1.0` and `.github/workflows/release.yml` publishes the installer.

Layout:

- `src-tauri/src/`: the Rust app. `watcher.rs` is the main loop, `recorder.rs` drives ffmpeg (`mkv.rs` reads the Matroska it streams), `audio.rs` records the game's sound, `live_client.rs` talks to the game, `player.rs` opens the replay window, `settings.rs` stores settings, `inputs.rs` keeps keys and clicks, `win.rs` holds small Windows helpers (start with Windows, the hotkeys and the Raw Input listener, local time).
- `ui/`: `player.html` (replay window) and `settings.html`.

## License

GPL-3.0. The installer bundles a GPL build of [ffmpeg](https://ffmpeg.org).

League of Legends is a trademark of Riot Games, Inc. This project isn't endorsed by or affiliated with Riot Games.
