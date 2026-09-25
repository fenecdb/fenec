#!/usr/bin/env python3
"""What keeping a database costs a page, in a real browser: `make file-bench`.

Serves the repository, opens crates/fenec-bench/browser/file.html in a
browser and prints what its worker measured, which the page posts back
here. Headless Chrome unless BROWSER says otherwise: BROWSER=safari opens a
Safari window (macOS; it is left open), BROWSER=firefox runs Firefox
headless. CHROME or FIREFOX name the binary when it is not where it
usually is.
"""

import http.server
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import threading

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))
PAGE = "crates/fenec-bench/browser/file.html"
RESULT = []
DONE = threading.Event()


class Handler(http.server.SimpleHTTPRequestHandler):
    extensions_map = {
        **http.server.SimpleHTTPRequestHandler.extensions_map,
        ".js": "text/javascript",
        ".wasm": "application/wasm",
    }

    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=ROOT, **kwargs)

    def end_headers(self):
        # A page measured twice must not be handed the first run's module.
        self.send_header("Cache-Control", "no-store")
        # Cross-origin isolated, the page's clock is fine enough for what is
        # measured: without it Chrome rounds performance.now() to 0.1 ms
        # and Safari to 1 ms.
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        super().end_headers()

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        self.send_response(204)
        self.end_headers()
        if self.path == "/progress":
            print("  " + json.loads(body), flush=True)
        elif self.path == "/result":
            RESULT.append(json.loads(body))
            DONE.set()

    def log_message(self, *args):
        pass


def chrome():
    for name in [os.environ.get("CHROME"), "google-chrome", "google-chrome-stable", "chromium", "chromium-browser"]:
        if name and shutil.which(name):
            return shutil.which(name)
    mac = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
    return mac if os.path.exists(mac) else None


def launch(url, profile):
    which = os.environ.get("BROWSER", "chrome").lower()
    if which == "safari":
        if platform.system() != "Darwin":
            sys.exit("BROWSER=safari needs macOS")
        subprocess.run(["open", "-a", "Safari", url], check=True)
        return None
    if which == "firefox":
        exe = os.environ.get("FIREFOX") or shutil.which("firefox")
        if not exe:
            sys.exit("no firefox (FIREFOX=/path/to/firefox)")
        return subprocess.Popen([exe, "-headless", "-no-remote", "-profile", profile, url],
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    exe = chrome()
    if not exe:
        sys.exit("no Chrome (CHROME=/path/to/chrome), or BROWSER=safari|firefox")
    return subprocess.Popen([exe, "--headless=new", f"--user-data-dir={profile}", "--no-first-run",
                             "--no-default-browser-check", url],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def ms(x):
    return f"{x:.2f} ms" if x < 10 else f"{x:.0f} ms"


def spread(s):
    return f"p50 {ms(s['p50'])}, mean {ms(s['mean'])}, at most {ms(s['max'])}"


def main():
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://localhost:{server.server_address[1]}/{PAGE}"
    print(f"serving {url}", flush=True)
    with tempfile.TemporaryDirectory() as profile:
        proc = launch(url, profile)
        try:
            if not DONE.wait(timeout=900):
                sys.exit("no result in 15 minutes")
        finally:
            if proc:
                proc.terminate()
                proc.wait()
    server.shutdown()
    r = RESULT[0]
    if "error" in r:
        sys.exit(f"the page failed: {r['error']}")
    mb = r["image_bytes"] / 1e6
    print()
    print(r["agent"])
    print(f"cross-origin isolated: {r['isolated']}, the clock's step {r['tick'] * 1000:.0f} us")
    print(f"{r['rows']} rows, vector<{r['dim']}>: a {mb:.1f} MB image")
    print(f"  the statement alone          {spread(r['statement'])}")
    print("  IndexedDB (persist)")
    print(f"    the image                  {ms(r['idb_image'])}")
    print(f"    a row after it             {spread(r['idb_row'])}")
    print(f"    restored                   {ms(r['idb_open'])} ({r['idb_rows']} rows)")
    print("  OPFS file (openFile)")
    print(f"    the image                  {ms(r['file_image'])}")
    print(f"    a row, with its statement  {spread(r['file_row'])}")
    print(f"    opened again               {ms(r['file_open'])} ({r['file_rows']} rows, {r['file_bytes'] / 1e6:.1f} MB)")
    print(f"  a small database ({r['small_bytes']} bytes)")
    print(f"    a row appended             {spread(r['small_append'])}")
    print(f"    a new image (compact)      {spread(r['small_image'])}")


if __name__ == "__main__":
    main()
