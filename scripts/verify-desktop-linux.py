#!/usr/bin/env python3
"""Exercise a real Linux desktop package without launching a GUI or touching a user's profile."""

import argparse
import hashlib
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent


def verify(archive, version):
    with tempfile.TemporaryDirectory(prefix="atur-terminal-desktop-check-", dir="/tmp") as temporary:
        work = Path(temporary)
        home = work / "home"
        home.mkdir()
        profile = work / "profile"
        env = {key: value for key, value in os.environ.items() if not key.startswith("ATUR_TERM_")}
        env.update(HOME=str(home), XDG_DATA_HOME=str(home / ".local/share"), ATUR_TERM_DIR=str(profile),
                   PATH="/usr/bin:/bin", SHELL="/bin/sh", PYTHONDONTWRITEBYTECODE="1")
        env.pop("DISPLAY", None)
        env.pop("WAYLAND_DISPLAY", None)
        beta = "-beta." in version
        commands = home / ".local/bin"
        commands.mkdir(parents=True)
        unrelated = commands / ("atur-term-beta" if beta else "atur-term")
        unrelated.write_text("existing user command\n")
        checksum = hashlib.sha256()
        with archive.open("rb") as source:
            for block in iter(lambda: source.read(1024 * 1024), b""):
                checksum.update(block)
        install = [sys.executable, str(ROOT / "scripts/install-desktop-linux.py")]
        subprocess.run([*install, "install", "--version", version, "--archive", str(archive),
                        "--sha256", checksum.hexdigest()], check=True, env=env)
        directory = home / ".local/share" / ("atur-terminal-beta" if beta else "atur-terminal")
        binaries = directory / "current"
        assert not profile.exists(), "Installing or checking build metadata started the app or a server"
        assert unrelated.read_text() == "existing user command\n"
        subprocess.run([*install, "verify", "--root", str(directory), "--version", version], check=True, env=env)
        subprocess.run([sys.executable, str(ROOT / "scripts/smoke-headless.py"), str(binaries / "atur-term")], check=True, env=env)
        server = subprocess.Popen([str(binaries / "atur-term-server")], env=env)
        try:
            deadline = time.monotonic() + 10
            socket = profile / "server.sock"
            while not socket.exists():
                if server.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("Packaged server did not start")
                time.sleep(0.025)
            before = subprocess.check_output([str(binaries / "atur-term"), "project", "list"], env=env)
            inode = socket.stat().st_ino
            subprocess.run([*install, "stage", "--root", str(directory), "--version", version,
                            "--archive", str(archive), "--sha256", checksum.hexdigest()], check=True, env=env)
            blocked = subprocess.run([*install, "uninstall", "--root", str(directory)], env=env, capture_output=True)
            assert blocked.returncode != 0, "Uninstall removed a running server's helpers"
            assert server.poll() is None and socket.stat().st_ino == inode
            assert subprocess.check_output([str(binaries / "atur-term"), "project", "list"], env=env) == before
        finally:
            if server.poll() is None:
                server.send_signal(signal.SIGTERM)
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()
                raise
        retained = {path.relative_to(profile): path.read_bytes() for path in profile.rglob("*")
                    if path.is_file() and not path.is_symlink()}
        subprocess.run([*install, "uninstall", "--root", str(directory)], check=True, env=env)
        assert not directory.exists()
        assert unrelated.read_text() == "existing user command\n"
        assert all((profile / path).read_bytes() == contents for path, contents in retained.items())
        print(f"Verified native desktop package {archive.name}: install, matching helpers, live-server lease, removal and retained data.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("--version", required=True)
    args = parser.parse_args()
    verify(args.archive.resolve(), args.version)
