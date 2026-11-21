#!/usr/bin/env python3
"""
Replay window: a small frameless, always-on-top player for one clip.

The UI lives in player.html and runs in an Edge WebView2 window (pywebview).
The clip is served from a tiny local HTTP server with Range support, so the
timeline can seek anywhere. The window never takes keyboard focus from the
game; everything is controlled with the mouse.

Usage: python player.py CLIP [--title T] [--speed 1] [--width 480] [--x 20] [--y 60] [--death-at 2]
"""
import argparse
import http.server
import re
import threading
import urllib.parse
from pathlib import Path

import webview
from webview.window import FixPoint

HTML = Path(__file__).with_name("player.html")
ASPECT = 16 / 9  # until the video reports its real size


def make_handler(clip):
    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            path = urllib.parse.urlsplit(self.path).path
            if path == "/":
                self.send_file(HTML, "text/html; charset=utf-8")
            elif path == "/clip.mp4":
                self.send_file(clip, "video/mp4")
            else:
                self.send_error(404)

        def send_file(self, file, ctype):
            size = file.stat().st_size
            start, end = 0, size - 1
            m = re.fullmatch(r"bytes=(\d*)-(\d*)", self.headers.get("Range", ""))
            if m and (m[1] or m[2]):
                if m[1]:
                    start = int(m[1])
                    end = min(int(m[2]), size - 1) if m[2] else size - 1
                else:  # suffix range: the last N bytes
                    start = max(size - int(m[2]), 0)
                if start > end:
                    self.send_response(416)
                    self.send_header("Content-Range", f"bytes */{size}")
                    self.end_headers()
                    return
                self.send_response(206)
                self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
            else:
                self.send_response(200)
            self.send_header("Content-Type", ctype)
            self.send_header("Accept-Ranges", "bytes")
            self.send_header("Content-Length", str(end - start + 1))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            try:
                with open(file, "rb") as f:
                    f.seek(start)
                    left = end - start + 1
                    while left > 0:
                        chunk = f.read(min(256 * 1024, left))
                        if not chunk:
                            break
                        self.wfile.write(chunk)
                        left -= len(chunk)
            except (ConnectionError, OSError):
                pass  # the player dropped the request after seeking elsewhere

    return Handler


class Api:
    """Called from player.html via window.pywebview.api."""

    def __init__(self):
        self._window = None

    def close(self):
        self._window.destroy()

    def resize(self, width, height, anchor):
        """Resize, keeping the `anchor` corner (e.g. "SE") in place."""
        fix = ((FixPoint.SOUTH if "S" in anchor else FixPoint.NORTH)
               | (FixPoint.EAST if "E" in anchor else FixPoint.WEST))
        self._window.resize(int(width), int(height), fix)

    def fit(self, aspect):
        """Match the window's height to the video's aspect ratio."""
        self._window.resize(self._window.width, round(self._window.width / aspect))


def main():
    ap = argparse.ArgumentParser(description="Play a death replay in a small floating window.")
    ap.add_argument("clip")
    ap.add_argument("--title", default="Death replay")
    ap.add_argument("--speed", type=float, default=1.0)
    ap.add_argument("--width", type=int, default=480)
    ap.add_argument("--x", type=int, default=20)
    ap.add_argument("--y", type=int, default=60)
    ap.add_argument("--death-at", type=float, default=0,
                    help="seconds before the clip's end where the death happens (marked on the timeline)")
    args = ap.parse_args()

    clip = Path(args.clip).resolve()
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), make_handler(clip))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    query = urllib.parse.urlencode({"title": args.title, "speed": args.speed, "death": args.death_at})

    api = Api()
    api._window = webview.create_window(
        args.title, f"http://127.0.0.1:{server.server_port}/?{query}", js_api=api,
        width=args.width, height=round(args.width / ASPECT), x=args.x, y=args.y,
        min_size=(240, 135), frameless=True, easy_drag=False, on_top=True,
        focus=False, background_color="#000000")
    webview.start()
    server.shutdown()


if __name__ == "__main__":
    main()
