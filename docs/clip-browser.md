# Clip browser and post-game review: design note

Covers #7 (clip browser) and #17 (end-of-game death review). Decided in a grill session on 2026-11-28,
with throwaway sketches in `prototypes/clip-browser/` (not committed; see "Sketches" at the end).

## In short

- **Clip browser:** a normal app window. On the left are all games that match the filters; on the
  right is the game in focus: a large player, and that game's clips in a strip below it.
- **Post-game review:** a separate small window, offered by a toast when a game ends. It's for
  triage: watch each death, keep it, bin it or leave it, then bin the chosen ones in one step.
- **A clip has three states:** starred, left as is, deleted. A star means "this one matters": you
  can find it again with the ★ Starred filter, across all games. As a side effect, pruning deletes
  starred clips only once nothing else is left.
- **Data:** one `game.json` per game folder, and one thumbnail per clip, both written at save time.

## Data

### `game.json`, one per game folder

Written when the game's first clip is saved and updated with each clip after that. Updated again at
"Game over" (result), and whenever the user stars, deletes or reviews a clip. It's
deleted together with its folder, so pruning never leaves an orphan behind.

```json
{
  "version": 1,
  "champion": "Caitlyn",
  "mode": "SR",
  "started": "2026-11-27T21:00:00+01:00",
  "result": "Win",
  "reviewed": false,
  "clips": {
    "08m40s_by-Zed.mp4": {
      "kind": "death",
      "gameTime": 520,
      "killer": "Zed",
      "assisters": ["Ahri"],
      "starred": false
    },
    "15m22s_saved.mp4": { "kind": "saved", "gameTime": 922, "starred": true }
  }
}
```

- `killer` is a readable name. Raw names are cleaned up at save time: `sru_dragon_water-6-1`
  becomes Dragon, and likewise Baron, Herald, Turret, Minion and the camps.
- `result` comes from the live client API at "Game over" if it still answers then. It is `null`
  when it doesn't, and the UI then shows nothing for it. No K/D/A: the number of deaths is what
  matters here, and the clips already give it.
- Writes go to a temporary file first and are then renamed, so a crash never leaves a half-written
  file.
- **No backfill:** it's greenfield. A game folder without `game.json` isn't shown in the browser,
  and today's clips can go.

### Thumbnails

One frame about a second before the death (two seconds before the clip's end, since clips keep 1 s
after the death), 320 px wide, saved beside the clip as `<clip>.jpg`. Made by the bundled ffmpeg
right after the clip is saved.

- **The ffmpeg build** gains the `mjpeg` encoder and `image2` muxer, plus ffmpeg's own `av1` decoder
  with `av1_d3d11va`: that decoder only works on the graphics card, and a card that records AV1 also
  decodes it. A thumbnail is tried in software first (H.264), then on the graphics card (AV1).
- A clip whose thumbnail fails still plays; the browser shows a plain tile for it.
- Saved clips (Ctrl+Shift+S) get a frame two seconds before their end as well.

## Stars

A star marks a clip as worth coming back to. Its job is to make that clip easy to find later:

- **★ Starred** in the browser's top bar shows only games with starred clips, and in each game's
  strip only its starred clips. With the other filters (champion, killer, Deaths or Saved) it
  answers questions like "my starred deaths to Zed".
- A starred clip carries a gold ★ on its thumbnail everywhere it's shown, and a game row shows how
  many starred clips it holds.
- You star with S or the star on the video, in the browser and in the post-game review (where it's
  "keep").

## Pruning with stars

The size limit counts each clip together with its `.inputs.json` and thumbnail (a `game.json` is
a few kilobytes and isn't counted).

1. Delete **unstarred clips one at a time, oldest first**, across all games, with each clip's
   `.inputs.json` and thumbnail, and its entry in `game.json`.
2. A game folder goes once it has no clips left, and a day folder goes once it has no games left.
3. Only if the folder is still over the limit, delete **starred clips**, oldest first, and say so in
   a toast and in the log. Starred clips are never deleted silently.
4. The game being played right now is never touched, as today.

This replaces today's "remove whole game folders, oldest first" in `watcher.rs::prune`. Tests:
starred clips outlive unstarred ones regardless of age; folders are removed once they're empty;
starred clips go only when nothing else is left; the current game is skipped.

## Clip browser (#7)

**Opening it:** **Browse clips** in the tray menu, and in the settings window, where it replaces
"Open clips folder" (Explorer stays available as a small link). It's a normal window that shows in
the taskbar, takes focus and remembers its size. Opening it again brings the existing window to the
front.

**Layout (Split):**

- **Top bar:** All / Deaths / Saved, champion, mode, killer, ★ Starred, and a count ("32 games ·
  159 clips").
- **Left (≈330 px):** one row per game that matches the filters, under day headers. A row shows the
  first death's thumbnail, the champion initial in a circle (first letter only), champion, mode and
  start time, then the **number of deaths** as the row's main figure ("8 deaths"), followed by saved
  and ★ counts, and ✓ once reviewed. A red or green edge marks a loss or a win when the result is
  known.
- **Right, the game in focus:** a header (champion, mode, date and time, counts), then the player,
  then the clip's title ("Death at 8:40 — Zed"), length, size and "keys recorded". Below that is a
  strip of the game's clips that match the filters. Picking a game plays its first clip.
- **On the video, top right:** star, show in folder, then a small divider and trash. These are icon
  buttons with tooltips that name the action and its key.

**Player:** the existing `ui/player.html`, embedded in the browser window. Inside the browser it
hides its own window chrome (title, close, resize edges). There is one player, and only its clip
changes: starring or deleting a clip must not reload it. While a clip loads, its thumbnail stands
in, and the video fades in once it has jumped to just before the death.

**Keys:** ↑/↓ change game, ←/→ (or J/K) change clip, S stars, Del deletes, Ctrl+Z undoes a
delete. The player takes no keys itself, so key presses made while it has focus are passed up to
the browser.

**Delete:** moves the file to the Recycle Bin straight away and shows a toast with **Undo**. There
is no confirmation dialog.

**Left out for now:** renaming clips and a date-range filter.

## Post-game review (#17)

**When it appears:** at "Game over", if the game saved at least one death, a toast with a **Review
deaths** button (`toast.html`). A setting can make the window open by itself instead, and another
turns review off. The toast or window closes itself if a new game starts. With "Save clips" off
there's nothing to review, so nothing appears.

**The window:** about 920×640, titled "Game over · Caitlyn · Summoner's Rift · 8 deaths".

- **Deaths only, in game order.** Saved clips are left out, since they were saved on purpose. Each
  death plays from just before the death.
- **Icons on the video, top right:** star (keep, S), show in folder, and trash (bin, D). Keep and
  bin both move straight on to the next death. A kept death shows a gold-filled star, and a binned
  one a solid red trash button.
- **Under the video:** the death ("7:38 — Sylas"), "2 of 8", ←, and Next (→ or Space). Skipping
  leaves the clip as it is.
- **One tick per death** along the bottom: gold for kept, red for binned. Click a tick to jump to
  that death.
- **Header links:** **Open in Clips** opens the browser on this game. **Later** closes the window
  and leaves the game unreviewed.
- **End screen:** a grid of every death showing your decisions, for example "2 clips to bin ·
  1 starred · 5 left as they are · frees 33 MB". Click a clip to change your mind, then **Move 2
  clips to Recycle Bin** (Enter). Nothing is deleted before this step. When the end screen is
  reached, the game is marked `reviewed`.

## Look and motion

These follow Emil Kowalski's design-engineering rules, as applied in the sketches.

- **Anything done by key is instant:** changing game or clip, starring, deleting by key, keep or bin
  in review. Those happen dozens of times per game.
- **Only occasional things animate:** the toast (enters 300 ms, leaves 150 ms, pauses while
  hovered), a mouse delete (the card shrinks and fades over 150 ms), the player fading in on a new
  clip (180 ms), the review end screen (260 ms, tiles 30 ms apart), and the first list load (rows
  30 ms apart).
- **Easing:** a strong ease-out `cubic-bezier(.23,1,.32,1)` for things appearing, ease-in-out for
  things moving across the screen (the All/Deaths/Saved highlight), plain `ease` for hover colours.
  Everything stays under 300 ms.
- **Pressing** a button, row or card scales it to 0.97. Hover effects only on devices with a mouse
  (`hover: hover`). Tooltips wait 450 ms, then are instant across the group.
- The "reduced motion" system setting turns movement into plain fades.
- Colours, fonts and light/dark themes come from the tokens in `ui/settings.html`, shared between the
  windows along with the icons and the icon-button style.

## Build order

Feature branches cut from `main` once `test-tooling` is merged, one PR each:

1. **Foundation (no UI):** `game.json` (written at save time and at "Game over", with readable
   killer names, assisters and W/L), thumbnails (the ffmpeg build change plus writing the
   frame), and pruning with stars. Tested with `cargo test`.
2. **Clip browser (#7):** the window, tray and settings entries, the embedded player, and commands
   to star, delete (Recycle Bin) and show in folder.
3. **Post-game review (#17):** the toast, the review window, the batch move to the Recycle Bin, the
   `reviewed` flag, and the settings.

## Sketches

`prototypes/clip-browser/` (throwaway, not committed): `build-data.ps1` reads the real clip folder
into `data/` (a catalogue and thumbnails); `index.html` is the browser; `review.html` is the post-game
window; and `player-embed.html` embeds the real player with Tauri stood in for. Serve the repo with
the `player-harness` preview and open `/prototypes/clip-browser/`. Delete them once this design has
been built.
