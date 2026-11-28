"""Serves a folder for the browser pane, telling the browser never to cache, so a reload always
shows the file as it is now. Used by .claude/launch.json.

    python scripts/dev-server.py PORT [FOLDER]   (FOLDER defaults to the repo root)
"""
import functools
import http.server
import pathlib
import sys


class NoCache(http.server.SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header("Cache-Control", "no-store")
        super().end_headers()


port = int(sys.argv[1])
folder = sys.argv[2] if len(sys.argv) > 2 else str(pathlib.Path(__file__).resolve().parent.parent)
handler = functools.partial(NoCache, directory=folder)
http.server.ThreadingHTTPServer(("127.0.0.1", port), handler).serve_forever()
