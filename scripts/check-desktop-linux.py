#!/usr/bin/env python3
"""Check the whole workspace, desktop app included, on a native Linux desktop userspace."""

import argparse
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
TARGETS = {"x86_64-unknown-linux-gnu": "x86_64", "aarch64-unknown-linux-gnu": "aarch64"}
SHORTCUT_TESTS = ["xvfb-run", "-a", "-s", "-screen 0 1280x1024x24 -noreset", "cargo", "test", "--locked", "--workspace", "--all-features", "--lib", "quick_terminal::platform::linux", "--", "--ignored"]
PAGE_TESTS = ["cargo", "test", "--locked", "--workspace", "--all-features", "--lib", "webview::linux", "--", "--ignored"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=sorted(TARGETS), required=True)
    args = parser.parse_args()
    compiler = subprocess.check_output(["rustc", "-vV"], cwd=ROOT, text=True)
    if (platform.system() != "Linux" or platform.machine() != TARGETS[args.target]
            or f"host: {args.target}\n" not in compiler):
        raise ValueError("The Linux desktop check requires the requested native Linux host and Rust toolchain")
    if os.environ.get("CARGO_BUILD_TARGET", args.target) != args.target:
        raise ValueError("Remove CARGO_BUILD_TARGET or set it to the native Linux host")
    wrapper = ROOT / "scripts/zig"
    zig = os.environ.get("ATUR_TERM_ZIG") or shutil.which("zig")
    if not zig or Path(zig).resolve() == (wrapper / "zig").resolve():
        raise ValueError("Install Zig 0.15.2 or set ATUR_TERM_ZIG to it before running the Linux desktop check")
    env = {
        **os.environ,
        "ATUR_TERM_ZIG": zig,
        "PATH": str(wrapper) + os.pathsep + os.environ.get("PATH", ""),
        "RUSTDOCFLAGS": (os.environ.get("RUSTDOCFLAGS", "") + " -D warnings").strip(),
    }
    display = bool(os.environ.get("DISPLAY") or os.environ.get("WAYLAND_DISPLAY"))
    commands = [
        [sys.executable, "-m", "unittest", "discover", "-s", "scripts/tests"],
        ["cargo", "fmt", "--all", "--check"],
        ["cargo", "clippy", "--locked", "--workspace", "--all-targets", "--all-features", "--", "-D", "warnings"],
        ["cargo", "test", "--locked", "--workspace", "--all-features"],
        SHORTCUT_TESTS,
        *([PAGE_TESTS] if display else []),
        ["cargo", "doc", "--locked", "--workspace", "--no-deps"],
        ["cargo", "build", "--locked", "-p", "muxy-app", "-p", "muxy-cli", "-p", "muxy-server"],
    ]
    for command in commands:
        print("+ " + " ".join(command), flush=True)
        subprocess.run(command, cwd=ROOT, env=env, check=True)
    if not display:
        print("Extension page tests were skipped: there is no display to render in.", flush=True)
    print(f"Linux desktop checks passed on {args.target}; they are headless and replace no human check.", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
