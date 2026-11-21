#!/usr/bin/env python3
"""
LoL Death Replay
----------------
Keeps a rolling recording of your screen while you're in a League of Legends
game. When you die, it saves the last ~15 seconds before the death and pops it
up in a looping replay window so you can see what went wrong.

Death detection uses Riot's official Live Client Data API
(https://127.0.0.1:2999/liveclientdata/...), which the game itself serves
locally during a match. Nothing touches the game's memory or files.

Requirements: Python 3.9+, ffmpeg on PATH, and pywebview for the replay window
(python -m pip install pywebview).
"""
import argparse
import ctypes
import datetime as dt
import importlib.util
import json
import os
import re
import shutil
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from ctypes import wintypes
from pathlib import Path

IS_WINDOWS = os.name == "nt"
NO_WINDOW = 0x08000000 if IS_WINDOWS else 0  # CREATE_NO_WINDOW

# The game uses a self-signed certificate on localhost.
_SSL = ssl.create_default_context()
_SSL.check_hostname = False
_SSL.verify_mode = ssl.CERT_NONE


def log(msg):
    print(f"[{dt.datetime.now():%H:%M:%S}] {msg}", flush=True)


# --------------------------------------------------------------------------
# Live Client Data API
# --------------------------------------------------------------------------
class LiveClient:
    def __init__(self, base):
        self.base = base.rstrip("/")

    def get(self, path, timeout=1.0):
        try:
            with urllib.request.urlopen(f"{self.base}/{path}", context=_SSL, timeout=timeout) as r:
                data = json.loads(r.read().decode("utf-8"))
        except Exception:
            return None
        # During loading screen the API answers with an error object.
        if isinstance(data, dict) and "errorCode" in data:
            return None
        return data


def my_names(active):
    """All the names the API might use for the local player."""
    names = set()
    for key in ("riotId", "riotIdGameName", "summonerName"):
        v = active.get(key)
        if v:
            names.add(v)
            names.add(v.split("#")[0])
    return names


def find_me(players, names):
    for p in players or []:
        for key in ("riotId", "riotIdGameName", "summonerName"):
            v = p.get(key)
            if v and (v in names or v.split("#")[0] in names):
                return p
    return None


def find_killer(events, names):
    for ev in reversed((events or {}).get("Events", [])):
        if ev.get("EventName") == "ChampionKill":
            victim = ev.get("VictimName", "")
            if victim in names or victim.split("#")[0] in names:
                return pretty_killer(ev.get("KillerName") or "unknown")
    return "unknown"


# Internal names the API uses for non-champion killers, e.g. "Turret_TChaos_L1_P2_2237424422".
KILLER_NAMES = [("Turret", "Turret"), ("Minion", "Minion"), ("SRU_Baron", "Baron"),
                ("SRU_RiftHerald", "Rift Herald"), ("SRU_Horde", "Voidgrubs"),
                ("SRU_Dragon_Elder", "Elder Dragon"), ("SRU_Dragon", "Dragon")]


def pretty_killer(name):
    for key, pretty in KILLER_NAMES:
        if key in name:
            return pretty
    if name.startswith("SRU_"):  # other jungle monsters, e.g. "SRU_Gromp13.1.1" -> "Gromp"
        return re.sub(r"[\d.]+$", "", name[4:]).replace("_", " ")
    return name.split("#")[0]  # champions: drop the Riot ID tag


# --------------------------------------------------------------------------
# ffmpeg helpers
# --------------------------------------------------------------------------
HW_ENCODERS = ["h264_nvenc", "h264_amf", "h264_qsv"]

ENCODER_ARGS = {
    "h264_nvenc": ["-preset", "p4", "-rc", "vbr", "-cq", "23"],
    "h264_amf": ["-quality", "speed", "-rc", "cqp", "-qp_i", "23", "-qp_p", "23"],
    "h264_qsv": ["-preset", "veryfast", "-global_quality", "23"],
    "libx264": ["-preset", "ultrafast", "-crf", "23", "-tune", "zerolatency"],
}


def encoder_works(ffmpeg, enc):
    try:
        r = subprocess.run(
            [ffmpeg, "-hide_banner", "-loglevel", "error", "-f", "lavfi",
             "-i", "color=s=256x256:d=0.2", "-vf", "format=nv12", "-c:v", enc, "-f", "null", "-"],
            capture_output=True, timeout=15, creationflags=NO_WINDOW)
        return r.returncode == 0
    except Exception:
        return False


def pick_encoder(ffmpeg, wanted):
    if wanted != "auto":
        return wanted
    for enc in HW_ENCODERS:
        if encoder_works(ffmpeg, enc):
            return enc
    return "libx264"


def capture_input(mode, fps, monitor):
    if mode == "ddagrab":  # Desktop Duplication API: fast, handles most fullscreen modes
        return ["-f", "lavfi", "-i",
                f"ddagrab=output_idx={monitor}:framerate={fps}:draw_mouse=1,hwdownload,format=bgra"]
    if mode == "gdigrab":  # older/slower fallback
        return ["-f", "gdigrab", "-framerate", str(fps), "-draw_mouse", "1", "-i", "desktop"]
    if mode == "test":     # synthetic video, for testing without a game
        return ["-re", "-f", "lavfi", "-i", f"testsrc2=size=1280x720:rate={fps}"]
    raise ValueError(f"unknown capture mode {mode}")


class Recorder:
    """Rolling buffer: ffmpeg writes short .ts segments and overwrites old ones."""

    def __init__(self, ffmpeg, mode, fps, monitor, encoder, height, seg_len, buffer_secs):
        self.ffmpeg, self.mode, self.fps, self.monitor = ffmpeg, mode, fps, monitor
        self.encoder, self.height = encoder, height
        self.seg_len = seg_len
        self.wrap = int(buffer_secs // seg_len) + 3
        self.proc = None
        self.dir = None
        self.logf = None

    def start(self):
        self.dir = Path(tempfile.mkdtemp(prefix="lol_replay_"))
        pix = "yuv420p" if self.encoder == "libx264" else "nv12"
        vf = f"scale=-2:{self.height}:flags=fast_bilinear," if self.height else ""
        vf += f"format={pix}"
        cmd = [self.ffmpeg, "-hide_banner", "-loglevel", "warning", "-y",
               *capture_input(self.mode, self.fps, self.monitor),
               "-vf", vf, "-c:v", self.encoder, *ENCODER_ARGS.get(self.encoder, []),
               "-g", str(self.fps), "-an",
               "-f", "segment", "-segment_time", str(self.seg_len),
               "-segment_wrap", str(self.wrap), "-segment_format", "mpegts",
               "seg%03d.ts"]
        self.logf = open(self.dir / "ffmpeg.log", "w")
        self.proc = subprocess.Popen(cmd, cwd=self.dir, stdin=subprocess.PIPE,
                                     stdout=self.logf, stderr=self.logf,
                                     creationflags=NO_WINDOW)
        log(f"Recording started ({self.mode}, {self.encoder}, {self.fps} fps)")

    def alive(self):
        return self.proc is not None and self.proc.poll() is None

    def stop(self):
        if self.proc and self.proc.poll() is None:
            try:
                self.proc.stdin.write(b"q")
                self.proc.stdin.flush()
                self.proc.wait(timeout=5)
            except Exception:
                self.proc.kill()
        self.proc = None
        if self.logf:
            self.logf.close()
            self.logf = None
        if self.dir:
            shutil.rmtree(self.dir, ignore_errors=True)
            self.dir = None
        log("Recording stopped")

    def error_tail(self):
        try:
            return (self.dir / "ffmpeg.log").read_text(errors="replace")[-1500:]
        except Exception:
            return ""

    def save_last(self, seconds, out_path):
        """Stitch the newest segments together and keep the last `seconds`."""
        segs = [p for p in self.dir.glob("seg*.ts") if p.stat().st_size > 0]
        segs.sort(key=lambda p: p.stat().st_mtime)
        need = int(seconds // self.seg_len) + 2
        segs = segs[-need:]
        if not segs:
            return False
        joined = "concat:" + "|".join(p.name for p in segs)
        r = subprocess.run(
            [self.ffmpeg, "-hide_banner", "-loglevel", "error", "-y",
             "-sseof", f"-{seconds}", "-i", joined,
             "-c", "copy", "-movflags", "+faststart", str(Path(out_path).resolve())],
            cwd=self.dir, capture_output=True, creationflags=NO_WINDOW)
        if r.returncode != 0:
            log("Clip save failed: " + r.stderr.decode(errors="replace")[-500:])
        return r.returncode == 0 and Path(out_path).exists()


# --------------------------------------------------------------------------
# Replay window
# --------------------------------------------------------------------------
PLAYER_SCRIPT = Path(__file__).with_name("player.py")


def has_pywebview():
    return importlib.util.find_spec("webview") is not None


class Player:
    """Opens each clip in player.py's floating window (sizes are in Windows' scaled pixels)."""

    def __init__(self, use_window, speed, width, x, y, death_at):
        self.use_window, self.speed, self.width = use_window, speed, width
        self.x, self.y, self.death_at = x, y, death_at
        self.proc = None

    def show(self, clip, title):
        self.close()
        if self.use_window:
            cmd = [sys.executable, str(PLAYER_SCRIPT), str(clip), "--title", title,
                   "--speed", str(self.speed), "--width", str(self.width),
                   "--x", str(self.x), "--y", str(self.y), "--death-at", str(self.death_at)]
            self.proc = subprocess.Popen(cmd, creationflags=NO_WINDOW)
        elif IS_WINDOWS:
            os.startfile(clip)  # default video player
        else:
            log(f"Clip ready: {clip}")

    def close(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
        self.proc = None


def close_hotkey_loop(player):
    """Ctrl+Shift+X closes the replay, even while the game has focus."""
    MOD_CONTROL, MOD_SHIFT, MOD_NOREPEAT, WM_HOTKEY = 0x2, 0x4, 0x4000, 0x312
    user32 = ctypes.windll.user32
    if not user32.RegisterHotKey(None, 1, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, ord("X")):
        log("Couldn't register Ctrl+Shift+X (another app is using it).")
        return
    msg = wintypes.MSG()
    while user32.GetMessageW(ctypes.byref(msg), None, 0, 0) > 0:
        if msg.message == WM_HOTKEY:
            player.close()


# --------------------------------------------------------------------------
# Main loop
# --------------------------------------------------------------------------
def safe(s):
    return re.sub(r"[^A-Za-z0-9_-]+", "-", s).strip("-")[:40] or "unknown"


def prune(clips_dir, keep):
    if keep <= 0:
        return
    clips = sorted(clips_dir.glob("*.mp4"), key=lambda p: p.stat().st_mtime)
    for old in clips[:-keep]:
        try:
            old.unlink()
        except OSError:
            pass


def handle_death(args, rec, player, client, names, clips_dir):
    time.sleep(args.after)  # capture the death itself + let ffmpeg flush
    if not rec.alive():
        return
    killer = find_killer(client.get("eventdata"), names)
    stats = client.get("gamestats") or {}
    gt = int(stats.get("gameTime", 0))
    name = f"{dt.datetime.now():%Y-%m-%d_%H%M%S}_{gt // 60:02d}m{gt % 60:02d}s_by-{safe(killer)}.mp4"
    out = clips_dir / name
    if rec.save_last(args.before + args.after, out):
        log(f"Saved {out.name}  (killed by {killer})")
        if not args.no_play:
            player.show(out, f"Death at {gt // 60}:{gt % 60:02d} - killed by {killer}")
        prune(clips_dir, args.keep)


def main():
    ap = argparse.ArgumentParser(description="Auto-replay your League of Legends deaths.")
    ap.add_argument("--before", type=float, default=15, help="seconds before death to show (15)")
    ap.add_argument("--after", type=float, default=2, help="seconds after death to include (2)")
    ap.add_argument("--fps", type=int, default=30)
    ap.add_argument("--height", type=int, default=1080, help="recording height, 0 = native (1080)")
    ap.add_argument("--capture", default="ddagrab", choices=["ddagrab", "gdigrab", "test"])
    ap.add_argument("--monitor", type=int, default=0, help="monitor index for ddagrab (0)")
    ap.add_argument("--encoder", default="auto",
                    help="auto, h264_nvenc, h264_amf, h264_qsv or libx264")
    ap.add_argument("--speed", type=float, default=1.0, help="replay speed, e.g. 0.5 for slow-mo")
    ap.add_argument("--window-width", type=int, default=480, help="replay window width (480)")
    ap.add_argument("--window-x", type=int, default=20, help="replay window left edge (20)")
    ap.add_argument("--window-y", type=int, default=60, help="replay window top edge (60)")
    ap.add_argument("--clips-dir", default="clips")
    ap.add_argument("--keep", type=int, default=100, help="max clips to keep, 0 = all (100)")
    ap.add_argument("--no-play", action="store_true", help="save clips but don't pop them up")
    ap.add_argument("--api", default="https://127.0.0.1:2999/liveclientdata")
    args = ap.parse_args()

    ffmpeg = shutil.which("ffmpeg")
    if not ffmpeg:
        sys.exit("ffmpeg not found. Install it (Windows: winget install Gyan.FFmpeg) and reopen the terminal.")
    use_window = has_pywebview()
    if not use_window and not args.no_play:
        log("pywebview not found - clips will open in your default video player instead. "
            "For the floating replay window run: python -m pip install pywebview")

    encoder = pick_encoder(ffmpeg, args.encoder)
    clips_dir = Path(args.clips_dir).resolve()
    clips_dir.mkdir(parents=True, exist_ok=True)
    client = LiveClient(args.api)
    player = Player(use_window, args.speed, args.window_width,
                    args.window_x, args.window_y, args.after)
    if IS_WINDOWS and use_window:
        threading.Thread(target=close_hotkey_loop, args=(player,), daemon=True).start()
    seg_len = 1
    rec = None
    names, was_dead = set(), False

    log(f"Waiting for a game... (encoder: {encoder}, clips -> {clips_dir})")
    try:
        while True:
            active = client.get("activeplayer")
            if not active:
                if rec:
                    rec.stop()
                    rec = None
                    player.close()
                    log("Game over. Waiting for the next one...")
                time.sleep(2)
                continue

            if rec is None:
                names = my_names(active)
                log(f"Game found - playing as {sorted(names)[0] if names else '?'}")
                rec = Recorder(ffmpeg, args.capture, args.fps, args.monitor, encoder,
                               args.height, seg_len, args.before + args.after)
                rec.start()
                was_dead = False

            if not rec.alive():
                log("ffmpeg stopped unexpectedly:\n" + rec.error_tail())
                sys.exit("Try --capture gdigrab or --encoder libx264.")

            me = find_me(client.get("playerlist"), names)
            dead = bool(me and me.get("isDead"))
            if dead and not was_dead:
                log("You died - saving replay...")
                threading.Thread(target=handle_death, daemon=True,
                                 args=(args, rec, player, client, names, clips_dir)).start()
            was_dead = dead
            time.sleep(0.25)
    except KeyboardInterrupt:
        pass
    finally:
        if rec:
            rec.stop()
        player.close()


if __name__ == "__main__":
    main()
