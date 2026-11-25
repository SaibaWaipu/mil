#!/usr/bin/env python3
"""End-to-end tests for `mil` driven through a real PTY.

A minimal ANSI screen emulator is used so assertions are made against what a
human would actually see, not against raw escape sequences. The test pages are
served by a local HTTP server, so the suite needs no network access.

Usage: python3 scripts/e2e.py [path/to/mil]
"""

from __future__ import annotations

import codecs
import fcntl
import http.server
import os
import pty
import re
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import unicodedata
from functools import partial
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PAGES = ROOT / "tests" / "e2e" / "pages"
ROWS, COLS = 30, 100


class Screen:
    """Small ANSI/UTF-8 screen model, sufficient for ratatui output."""

    def __init__(self, rows: int, cols: int) -> None:
        self.rows, self.cols = rows, cols
        self.grid = [[" "] * cols for _ in range(rows)]
        self.row = self.col = 0
        self.saved = (0, 0)
        self.decoder = codecs.getincrementaldecoder("utf-8")()

    def feed(self, data: bytes) -> None:
        text = self.decoder.decode(data)
        i = 0
        while i < len(text):
            ch = text[i]
            if ch == "\x1b":
                i = self._escape(text, i)
            elif ch == "\r":
                self.col = 0
                i += 1
            elif ch == "\n":
                self.row = min(self.rows - 1, self.row + 1)
                i += 1
            elif ch == "\b":
                self.col = max(0, self.col - 1)
                i += 1
            elif ch == "\t":
                self.col = min(self.cols - 1, (self.col // 8 + 1) * 8)
                i += 1
            else:
                self._put(ch)
                i += 1

    def _escape(self, text: str, i: int) -> int:
        if i + 1 >= len(text):
            return i + 1
        marker = text[i + 1]
        if marker == "[":
            end = i + 2
            while end < len(text) and not ("@" <= text[end] <= "~"):
                end += 1
            if end >= len(text):
                return i
            self._csi(text[i + 2 : end], text[end])
            return end + 1
        if marker == "]":
            end = i + 2
            while end < len(text):
                if text[end] == "\x07":
                    return end + 1
                if text[end] == "\x1b" and end + 1 < len(text) and text[end + 1] == "\\":
                    return end + 2
                end += 1
            return end
        if marker in "P_X^":
            end = i + 2
            while end + 1 < len(text):
                if text[end] == "\x1b" and text[end + 1] == "\\":
                    return end + 2
                end += 1
            return end
        if marker == "7":
            self.saved = (self.row, self.col)
            return i + 2
        if marker == "8":
            self.row, self.col = self.saved
            return i + 2
        return i + 2

    def _csi(self, params: str, cmd: str) -> None:
        nums = [int(n) for n in re.findall(r"\d+", params)]
        if cmd in "Hf":
            self.row = (nums[0] - 1) if nums else 0
            self.col = (nums[1] - 1) if len(nums) > 1 else 0
        elif cmd == "A":
            self.row = max(0, self.row - (nums[0] if nums else 1))
        elif cmd == "B":
            self.row = min(self.rows - 1, self.row + (nums[0] if nums else 1))
        elif cmd == "C":
            self.col = min(self.cols - 1, self.col + (nums[0] if nums else 1))
        elif cmd == "D":
            self.col = max(0, self.col - (nums[0] if nums else 1))
        elif cmd == "J":
            mode = nums[0] if nums else 0
            if mode == 2:
                self.grid = [[" "] * self.cols for _ in range(self.rows)]
            elif mode == 0:
                for c in range(self.col, self.cols):
                    self.grid[self.row][c] = " "
                for r in range(self.row + 1, self.rows):
                    self.grid[r] = [" "] * self.cols
        elif cmd == "K":
            start = self.col if not nums or nums[0] == 0 else 0
            for c in range(start, self.cols):
                self.grid[self.row][c] = " "

    def _put(self, ch: str) -> None:
        if not (0 <= self.row < self.rows and 0 <= self.col < self.cols):
            return
        self.grid[self.row][self.col] = ch
        width = 2 if unicodedata.east_asian_width(ch) in "WF" else 1
        if width == 2 and self.col + 1 < self.cols:
            self.grid[self.row][self.col + 1] = ""
        self.col += width
        if self.col >= self.cols:
            self.col = self.cols - 1

    def text(self) -> str:
        return "\n".join("".join(row).rstrip() for row in self.grid)


class Browser:
    def __init__(self, binary: str, args: list[str], config: Path, state_dir: Path) -> None:
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        env = os.environ.copy()
        env.update(
            TERM="xterm-256color",
            MIL_CONFIG=str(config),
            MIL_IMAGE_PROTOCOL="none",
            XDG_STATE_HOME=str(state_dir),
        )
        err_path = state_dir / "stderr.log"
        self.stderr = err_path.open("w")
        self.proc = subprocess.Popen(
            [binary, *args],
            stdin=slave,
            stdout=slave,
            stderr=self.stderr,
            env=env,
            close_fds=True,
        )
        os.close(slave)
        self.master = master
        self.screen = Screen(ROWS, COLS)
        self._stop = threading.Event()

        def reader() -> None:
            while not self._stop.is_set():
                try:
                    data = os.read(master, 4096)
                except OSError:
                    return
                if not data:
                    return
                self.screen.feed(data)

        self.thread = threading.Thread(target=reader, daemon=True)
        self.thread.start()

    def send(self, keys: str) -> None:
        os.write(self.master, keys.encode())

    def type_text(self, text: str, delay: float = 0.03) -> None:
        for ch in text:
            self.send(ch)
            time.sleep(delay)

    def expect(self, needle: str, timeout: float = 5.0) -> str:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            text = self.screen.text()
            if needle in text:
                return text
            time.sleep(0.05)
        raise AssertionError(f"expected {needle!r} in screen:\n{self.screen.text()}")

    def absent(self, needle: str, timeout: float = 1.0) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if needle not in self.screen.text():
                return
            time.sleep(0.05)
        raise AssertionError(f"unexpected {needle!r} still on screen")

    def close(self) -> None:
        if self.proc.poll() is None:
            self.send("q")
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self._stop.set()
        try:
            os.close(self.master)
        except OSError:
            pass
        self.stderr.close()

    def stderr_text(self) -> str:
        self.stderr.flush()
        return self.stderr.name and Path(self.stderr.name).read_text(errors="replace")


class TestServer(http.server.ThreadingHTTPServer):
    daemon_threads = True


def run(binary: str) -> None:
    server = TestServer(("127.0.0.1", 0), partial(http.server.SimpleHTTPRequestHandler, directory=str(PAGES)))
    port = server.server_port
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{port}"

    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        config = tmp / "config.toml"
        config.write_text(
            'homepage = "' + base + '/second.html"\n'
            'search_engine = "https://example.invalid/search?q={}"\n'
            "[javascript]\nenabled = true\n"
            "[keybindings]\nnext_tab = \"n\"\n"
        )
        state = tmp / "state"
        state.mkdir()

        def launch(*args: str) -> Browser:
            return Browser(binary, list(args), config, state)

        passed: list[str] = []
        failed: list[str] = []

        def check(name: str, fn) -> None:
            try:
                fn()
            except Exception as exc:
                failed.append(name)
                print(f"FAIL {name}: {exc}")
            else:
                passed.append(name)
                print(f"PASS {name}")

        # 1. basic rendering
        def basic() -> None:
            b = launch("--no-session", base + "/index.html")
            try:
                b.expect("HOME HEADING")
                b.expect("GO SECOND")
                b.expect("E2E Home")
            finally:
                b.close()

        check("basic page rendering and title", basic)

        # 2. link activation with Tab + Enter
        def follow_link() -> None:
            b = launch("--no-session", base + "/index.html")
            try:
                b.expect("GO SECOND")
                b.send("\t")
                time.sleep(0.2)
                b.send("\r")
                b.expect("SECOND PAGE")
            finally:
                b.close()

        check("link focus and activation", follow_link)

        # 3. back navigation
        def back() -> None:
            b = launch("--no-session", base + "/index.html")
            try:
                b.expect("HOME HEADING")
                b.send("\t")
                time.sleep(0.2)
                b.send("\r")
                b.expect("SECOND PAGE")
                b.send("\x1b[1;3D")  # Alt+Left
                b.expect("HOME HEADING")
            finally:
                b.close()

        check("back navigation", back)

        # 4. new tab / switch / close
        def tabs() -> None:
            b = launch("--no-session", base + "/index.html")
            try:
                b.expect("HOME HEADING")
                b.send("\x14")  # Ctrl+T
                b.expect("HOME HEADING")
                b.send("n")  # next tab
                time.sleep(0.2)
                b.send("\x17")  # Ctrl+W
                b.expect("HOME HEADING")
            finally:
                b.close()

        check("tab create/switch/close", tabs)

        # 5. omnibox navigation
        def omnibox() -> None:
            b = launch("--no-session", base + "/index.html")
            try:
                b.expect("HOME HEADING")
                b.send("\x0c")  # Ctrl+L
                time.sleep(0.2)
                b.type_text(base + "/second.html")
                time.sleep(0.2)
                b.send("\r")
                b.expect("SECOND PAGE")
            finally:
                b.close()

        check("omnibox navigation", omnibox)

        # 6. JavaScript mutation
        def js() -> None:
            b = launch("--no-session", base + "/js.html")
            try:
                b.expect("JS CHANGED")
                b.expect("JS AFTER")
            finally:
                b.close()

        check("javascript DOM and title mutation", js)

        # 7. find and scroll
        def find() -> None:
            b = launch("--no-session", base + "/long.html")
            try:
                b.expect("LONG DOCUMENT")
                b.send("/")
                time.sleep(0.1)
                b.type_text("BOTTOM MARKER")
                time.sleep(0.2)
                b.send("\r")
                b.expect("BOTTOM MARKER")
            finally:
                b.close()

        check("find scrolls to match", find)

        # 8. reload and error resilience
        def reload() -> None:
            b = launch("--no-session", base + "/index.html")
            try:
                b.expect("HOME HEADING")
                b.send("r")
                b.expect("HOME HEADING")
                b.send("\x0c")
                time.sleep(0.2)
                b.type_text(base + "/missing.html")
                time.sleep(0.2)
                b.send("\r")
                time.sleep(0.6)
                if b.proc.poll() is not None:
                    raise AssertionError("browser exited on failed navigation")
            finally:
                b.close()

        check("reload and failed navigation resilience", reload)

        # 9. session restore
        def session() -> None:
            b = launch(base + "/index.html")
            b.expect("HOME HEADING")
            b.send("q")
            try:
                b.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                b.proc.kill()
                raise AssertionError("q did not quit the browser gracefully")
            finally:
                b.close()
            if not (state / "mil" / "session.json").exists():
                raise AssertionError("session.json was not written")
            b2 = launch()  # no URL: must come from the saved session
            try:
                b2.expect("HOME HEADING")
                b2.absent("SECOND PAGE")
            finally:
                b2.close()

        check("session restore", session)

        # 10. scrolling keys
        def scrolling() -> None:
            b = launch("--no-session", base + "/long.html")
            try:
                b.expect("LONG DOCUMENT")
                for _ in range(6):
                    b.send("j")
                    time.sleep(0.04)
                b.expect("%")
                text = b.screen.text()
                if "0%  ? help" in text:
                    raise AssertionError("j did not scroll")
                b.send("g")
                b.expect("0%  ? help")
                time.sleep(0.2)
                b.send("G")
                b.expect("100%")
            finally:
                b.close()

        check("scroll with j / g / G", scrolling)

        # 11. forward navigation
        def forward() -> None:
            b = launch("--no-session", base + "/index.html")
            try:
                b.expect("GO SECOND")
                b.send("\t")
                time.sleep(0.2)
                b.send("\r")
                b.expect("SECOND PAGE")
                b.send("\x1b[1;3D")
                b.expect("GO SECOND")
                b.send("\x1b[1;3C")  # Alt+Right
                b.expect("SECOND PAGE")
            finally:
                b.close()

        check("forward navigation", forward)

        # 12. mouse click on a link
        def mouse_link() -> None:
            b = launch("--no-session", base + "/index.html")
            try:
                b.expect("GO SECOND")
                row = next(i for i, line in enumerate(b.screen.text().splitlines()) if "GO SECOND" in line)
                col = b.screen.text().splitlines()[row].index("GO SECOND")
                b.send(f"\x1b[<0;{col + 2};{row + 1}M")
                b.expect("SECOND PAGE")
            finally:
                b.close()

        check("mouse click follows link", mouse_link)

        # 13. bookmark
        def bookmark() -> None:
            b = launch("--no-session", base + "/index.html")
            b.expect("HOME HEADING")
            b.send("b")
            time.sleep(0.3)
            b.close()
            path = state / "mil" / "bookmarks.json"
            if not path.exists() or base + "/index.html" not in path.read_text():
                raise AssertionError("bookmark was not persisted")

        check("bookmark persistence", bookmark)

        # 14. two-tab session restore
        def multi_tab_session() -> None:
            b = launch(base + "/index.html")
            b.expect("HOME HEADING")
            b.send("\x14")  # Ctrl+T
            b.expect("HOME HEADING")
            b.send("\x0c")
            time.sleep(0.2)
            b.type_text(base + "/second.html")
            b.send("\r")
            b.expect("SECOND PAGE")
            b.send("q")
            b.proc.wait(timeout=3)
            b.close()
            b2 = launch()
            try:
                b2.expect("SECOND PAGE")
                b2.send("n")  # next tab (custom binding avoids crossterm Ctrl+Tab parsing)
                time.sleep(0.3)
                b2.expect("HOME HEADING")
            finally:
                b2.close()

        check("multi-tab session restore", multi_tab_session)

    server.shutdown()
    print(f"\n{len(passed)} passed, {len(failed)} failed")
    if failed:
        print("failed:", ", ".join(failed))
        sys.exit(1)


if __name__ == "__main__":
    binary = sys.argv[1] if len(sys.argv) > 1 else str(ROOT / "target" / "debug" / "mil")
    run(str(Path(binary).resolve()))
