# LoL Death Replay

When you die in League, this pops up a small looping replay player in the top-left corner, showing the 15 seconds leading up to your death, so you can review what went wrong while you wait to respawn. Every death is also saved to the `clips` folder (named with game time and who killed you) for reviewing later.

## Setup (once)

1. **Python**: install from python.org (tick "Add Python to PATH"), or `winget install Python.Python.3.12`
2. **ffmpeg** (does the recording + replay window): open a terminal and run
   `winget install Gyan.FFmpeg`
   then close and reopen the terminal.
3. **pywebview** (draws the replay window): run
   `python -m pip install pywebview`
4. In League settings → Video, set **Window Mode: Borderless**, so the replay window can appear on top of the game.

## Use

Double-click the **LoL Death Replay** shortcut before (or during) a game. (`start.bat` does the same, but Windows' Smart App Control blocks it if the file came from the internet; the shortcut gets through because it starts Python directly.) It waits for a match to start, records in the background, and stops on its own when the game ends. Leave it running between games.

The replay window stays on top of the game and never takes your keyboard away from it. Move the mouse over it to show the controls:

- **Drag the video** to move the window, and drag any **edge or corner** to resize it. Click the video to pause or play it.
- **Timeline**: drag it to scrub through the clip. The gold **DEATH** section at the end is the interesting part: from about 3 seconds before you died to the end of the clip.
- **Bottom row**: back 5s, play/pause and forward 5s in the middle; playback speed (0.25× to 2×) on the right. Clips have no sound.
- **X** (top right) closes it. **Ctrl+Shift+X** also closes it from inside the game.

The window closes by itself the next time you die (replaced by the new replay).

## Options

Right-click the shortcut → **Properties** and add any of these to the end of **Target** (or to the line in `start.bat`):

| Option | What it does |
|---|---|
| `--before 20` | Show 20 seconds before death instead of 15 |
| `--speed 0.5` | Start replays in slow motion (you can change it in the window too) |
| `--no-play` | Don't pop up the replay, just save clips |
| `--window-width 640` | Make the replay window bigger (default 480, follows Windows display scaling) |
| `--window-x 1400 --window-y 100` | Move the replay window (default 20, 60 from the top-left corner) |
| `--height 720` | Record at 720p (default 1080p; 720p is lighter on your PC) |
| `--fps 60` | Smoother replays (more load) |
| `--monitor 1` | Record your second monitor |
| `--keep 0` | Never delete old clips (default keeps the newest 100) |

## Troubleshooting

- **Replay is black**: add `--capture gdigrab` to the shortcut's Target, and check that the game is in Borderless mode.
- **Game stutters**: the tool picks your GPU's video encoder automatically (NVIDIA/AMD/Intel). If it falls back to the CPU (`libx264` in the startup line), try `--height 720`, or `--fps 24 --height 600` if it still stutters.
- **Replay doesn't appear over the game**: use Borderless mode; true fullscreen hides other windows.
- **Clips open in your normal video player instead of the floating window**: pywebview isn't installed (see Setup step 3).

Deaths are detected with Riot's official Live Client Data API, which the game serves locally. The tool only reads that API and records your screen, the same way OBS does. It doesn't touch the game client.
