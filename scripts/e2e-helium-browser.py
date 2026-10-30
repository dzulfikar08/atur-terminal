#!/usr/bin/env python3
"""Drive a headless Chromium-based browser through `atur-term browser --engine ...`.

The browser stands in for Helium: it is started by the CLI itself with remote
debugging on loopback, on a throwaway profile folder, and a local test page is
driven with open, fill, click, wait, read, screenshot and the rest of the verbs.
Run it with the path of a built `atur-term` and, optionally, the engine name
(default `chrome`) and `ATUR_TERM_BROWSER_PATH` for a specific executable.
"""

import base64
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse
from urllib.request import Request, urlopen

INDEX = """<!doctype html><title>Agent test</title>
<input id="name" placeholder="Your name"><button id="go"
 onclick="document.getElementById('out').textContent = 'Hello, ' + document.getElementById('name').value">Greet</button>
<div id="out"></div>
<input type="checkbox" id="agree"><select id="size"><option value="s">S</option><option value="l">L</option></select>
<form action="/done" method="get"><input name="q" id="q"><button type="submit">Send</button></form>
<a id="next" href="/next">Next</a>
<script>setTimeout(() => { const p = document.createElement('p'); p.id = 'late'; p.textContent = 'Loaded later'; document.body.appendChild(p); }, 800)</script>
"""


class Page(BaseHTTPRequestHandler):
    def do_GET(self):
        url = urlparse(self.path)
        if url.path == "/done":
            query = parse_qs(url.query).get("q", [""])[0]
            body = f"<title>Done</title><h1 id='h'>Done: {query}</h1>"
        elif url.path == "/next":
            body = "<title>Next</title><h1>Next page</h1>"
        else:
            body = INDEX
        data = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        if url.path == "/":
            self.send_header("Set-Cookie", "seen=1; Path=/")
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


class Run:
    def __init__(self, binary, engine):
        self.binary = binary
        self.engine = engine
        self.temporary = tempfile.mkdtemp(prefix="atur-helium-e2e-")
        self.profile = os.path.join(self.temporary, "profile")
        self.state = os.path.join(self.temporary, "state")
        os.makedirs(self.state)
        self.env = {
            **os.environ,
            "ATUR_TERM_DIR": self.state,
            "ATUR_TERM_BROWSER_ENGINE": engine,
            "ATUR_TERM_BROWSER_USER_DATA_DIR": self.profile,
            "ATUR_TERM_BROWSER_ARGS": "--headless=new --no-sandbox --disable-gpu --no-first-run "
            "--disable-dev-shm-usage --no-default-browser-check",
        }
        self.results = []

    def cli(self, *args, env=None, fail=False, timeout=90):
        done = subprocess.run(
            [str(self.binary), "browser", *args],
            env=env or self.env,
            text=True,
            capture_output=True,
            timeout=timeout,
        )
        if fail:
            assert done.returncode != 0, f"{args} should fail, printed {done.stdout!r}"
            return done.stderr
        assert done.returncode == 0, f"{args} failed: {done.stderr}"
        return done.stdout.strip()

    def expect(self, name, got, want):
        self.check(name, got == want, f"got {got!r}, wanted {want!r}")

    def check(self, name, condition, detail=""):
        assert condition, f"{name}: {detail}"
        self.results.append(name)
        print(f"PASS {name}", flush=True)

    def stop_browser(self):
        subprocess.run(["pkill", "-f", f"--user-data-dir={self.profile}"], check=False)


def debug_port(run):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        out = subprocess.run(["ps", "-axo", "args="], text=True, capture_output=True).stdout
        for line in out.splitlines():
            if f"--user-data-dir={run.profile}" in line and "--type=" not in line:
                for word in line.split():
                    if word.startswith("--remote-debugging-port="):
                        return int(word.split("=", 1)[1])
        time.sleep(0.2)
    raise AssertionError("no browser with a debugging port")


def e2e(binary, engine):
    server = ThreadingHTTPServer(("127.0.0.1", 0), Page)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{server.server_address[1]}"
    run = Run(binary, engine)
    try:
        started = time.monotonic()
        tab = run.cli("open", base + "/")
        run.check("open starts the browser and prints a tab id", len(tab) >= 8 and " " not in tab, tab)
        print(f"  (browser started and tab opened in {time.monotonic() - started:.1f}s)")
        port = debug_port(run)
        banner = subprocess.run(["ps", "-axo", "args="], text=True, capture_output=True).stdout
        run.check(
            "debugging is bound to loopback only",
            "--remote-debugging-address=127.0.0.1" in banner,
        )
        version = json.load(urlopen(f"http://127.0.0.1:{port}/json/version", timeout=5))
        run.check("the browser answers on 127.0.0.1", "webSocketDebuggerUrl" in version)

        run.cli("wait", tab, "--selector", "#go")
        listed = run.cli("list").splitlines()
        run.check(
            "list shows the tab with title and url",
            any(line.split("\t")[0] == tab and "Agent test" in line and base in line for line in listed),
            "\n".join(listed),
        )
        snapshot = json.loads(run.cli("snapshot", tab))
        selectors = [item["selector"] for item in snapshot]
        run.check("snapshot lists interactive elements", "#go" in selectors and "#name" in selectors, str(selectors))

        run.cli("fill", tab, "#name", "Ada")
        run.check("fill sets the value", json.loads(run.cli("get-value", tab, "#name")) == "Ada")
        run.cli("click", tab, "#go")
        run.check(
            "wait --text sees the click's effect",
            json.loads(run.cli("wait", tab, "--text", "Hello, Ada", "--timeout-ms", "5000")) is True,
        )
        run.check("read returns title, url and text", "Hello, Ada" in run.cli("read", tab) and "Agent test" in run.cli("read", tab))
        run.check("wait-for sees later content", json.loads(run.cli("wait-for", tab, "#late", "5000")) is True)
        run.check("wait times out with false", json.loads(run.cli("wait", tab, "--selector", "#never", "--timeout-ms", "300")) is False)
        run.expect("eval returns JSON", run.cli("eval", tab, "1 + 1"), "2")
        run.expect("eval runs statements", run.cli("eval", tab, "const a = 20; return a + 1;"), "21")
        run.check("get-text", json.loads(run.cli("get-text", tab, "#out")) == "Hello, Ada")
        run.expect("get-count", run.cli("get-count", tab, "input"), "3")
        run.check("get-attribute", json.loads(run.cli("get-attribute", tab, "#next", "href")) == "/next")
        run.expect("is visible", run.cli("is", tab, "visible", "#go"), "true")
        run.cli("check", tab, "#agree")
        run.expect("check sets the box", run.cli("is", tab, "checked", "#agree"), "true")
        run.cli("select", tab, "#size", "l")
        run.check("select sets the value", json.loads(run.cli("get-value", tab, "#size")) == "l")
        found = json.loads(run.cli("find", tab, "text", "Greet"))
        run.check("find returns selectors", found and found[0]["selector"] == "#go", str(found))
        run.check("get-html", "<h1" not in run.cli("get-html", tab, "#out") and "Hello" in run.cli("get-html", tab, "#out"))

        run.cli("storage", "set", tab, "token", "abc")
        run.check("storage round trip", json.loads(run.cli("storage", "get", tab, "token")) == "abc")
        run.cli("storage", "clear", tab)
        run.expect("storage clear", run.cli("storage", "get", tab, "token"), "null")

        cookies = json.loads(run.cli("cookies", "get", tab))
        run.check("cookies get lists the page's cookie", any(c["name"] == "seen" and c["value"] == "1" for c in cookies), str(cookies))
        run.cli("cookies", "set", tab, "agent", "7", "127.0.0.1")
        names = [c["name"] for c in json.loads(run.cli("cookies", "get", tab))]
        run.check("cookies set", "agent" in names, str(names))
        run.cli("cookies", "delete", tab, "agent")
        names = [c["name"] for c in json.loads(run.cli("cookies", "get", tab))]
        run.check("cookies delete", "agent" not in names and "seen" in names, str(names))
        run.cli("cookies", "clear", tab)
        run.check("cookies clear empties the page's cookies", json.loads(run.cli("cookies", "get", tab)) == [])

        shot = base64.b64decode(run.cli("screenshot", tab))
        run.check("screenshot is a PNG", shot[:8] == b"\x89PNG\r\n\x1a\n" and len(shot) > 500, str(len(shot)))

        run.cli("type", tab, "#q", "rust", "--submit")
        landed = run.cli("wait-for-navigation", tab, "10000")
        run.check("type --submit then wait-for-navigation", "/done?q=rust" in landed, landed)
        run.check("the new page reads", "Done: rust" in run.cli("read", tab))
        run.cli("back", tab)
        run.cli("wait", tab, "--selector", "#go", "--timeout-ms", "5000")
        run.check("back returns to the first page", base + "/" in json.loads(run.cli("eval", tab, "location.href")))
        run.cli("forward", tab)
        run.check("forward", json.loads(run.cli("wait", tab, "--url-contains", "/done", "--timeout-ms", "5000")) is True)
        run.cli("navigate", tab, base + "/next")
        run.check("navigate", json.loads(run.cli("wait", tab, "--text", "Next page", "--timeout-ms", "5000")) is True)
        run.cli("reload", tab)
        run.check("reload", json.loads(run.cli("wait", tab, "--text", "Next page", "--timeout-ms", "5000")) is True)

        second = run.cli("open", base + "/next")
        run.check("a second open is a new tab", second != tab)
        listed = [line.split("\t")[0] for line in run.cli("list").splitlines()]
        run.check("both agent tabs are listed", tab in listed and second in listed, str(listed))

        # A tab the user opened themselves is off limits unless --any-tab.
        request = Request(f"http://127.0.0.1:{port}/json/new?{base}/next", method="PUT")
        user_tab = json.load(urlopen(request, timeout=5))["id"]
        refusal = run.cli("read", user_tab, fail=True)
        run.check("agents may not touch tabs they did not open", "not found" in refusal and "--any-tab" in refusal, refusal)
        run.check("list hides the user's tabs", user_tab not in run.cli("list"))
        run.check("--any-tab lists them", user_tab in run.cli("list", "--any-tab"))
        run.check("--any-tab allows driving them", "Next page" in run.cli("read", user_tab, "--any-tab"))

        state = json.load(open(os.path.join(run.state, "helium-agent.json")))
        run.check(
            "the activity note lists the agent's tabs",
            state["engine"] == engine and tab in state["tabs"] and second in state["tabs"] and user_tab not in state["tabs"]
            and state["endpoint"] == f"127.0.0.1:{port}" and state["active_at_ms"] > 0,
            str(state),
        )

        run.cli("close", second)
        run.check("close removes the tab", second not in run.cli("list"))
        run.check("a closed tab is gone", "not found" in run.cli("read", second, fail=True))

        refused = run.cli("list", env={**run.env, "ATUR_TERM_BROWSER_ENDPOINT": "0.0.0.0:9222"}, fail=True)
        run.check("a non-loopback endpoint is refused", "not on this computer" in refused, refused)
        refused = run.cli("list", env={**run.env, "ATUR_TERM_BROWSER_ENDPOINT": "192.168.1.5:9222"}, fail=True)
        run.check("a LAN endpoint is refused", "not on this computer" in refused, refused)

        run.cli("close", tab)
    finally:
        run.stop_browser()
        server.shutdown()

    # A browser that runs without a debugging port is never restarted silently.
    profile = os.path.join(run.temporary, "profile-plain")
    env = {**run.env, "ATUR_TERM_BROWSER_USER_DATA_DIR": profile}
    plain = None
    try:
        plain = subprocess.Popen(
            [
                os.environ.get("ATUR_TERM_BROWSER_PATH") or ("google-chrome" if engine == "chrome" else engine),
                "--headless=new", "--no-sandbox", "--disable-gpu", "--no-first-run",
                f"--user-data-dir={profile}", "about:blank",
            ],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True,
        )
        for _ in range(100):
            if subprocess.run(["pgrep", "-f", f"--user-data-dir={profile}"], capture_output=True).returncode == 0:
                break
            time.sleep(0.2)
        time.sleep(3)
        run.check("the plain browser is running", plain.poll() is None, f"exited with {plain.poll()}")
        refusal = run.cli("list", env=env, fail=True)
        run.check(
            "a running browser without debugging gets an explanation, not a restart",
            "without remote debugging" in refusal and "--restart-browser" in refusal and "never restarts" in refusal,
            refusal,
        )
        time.sleep(1)
        run.check("and is left running", plain.poll() is None)
    finally:
        subprocess.run(["pkill", "-f", f"--user-data-dir={profile}"], check=False)
        run.stop_browser()
    print(f"\n{len(run.results)} checks passed with engine {engine}")


if __name__ == "__main__":
    e2e(Path(sys.argv[1]).resolve(), sys.argv[2] if len(sys.argv) > 2 else "chrome")
