#!/usr/bin/env python3
"""Drive an interactive TUI through a pty with expect/send steps, for affordances a harness only
offers interactively (slash commands like /undo and /clear, interrupts).

    TUI_SCRIPT=script.json tui_driver.py <transcript-log> <cmd> [args...]

The script is a JSON list of steps, run in order:
    {"expect": "<regex>", "timeout": 10, "optional": false}   wait until the screen text matches
    {"send": "<text>"}                                        type text ("\\r" is Enter, "\\u0003" ^C)
    {"sleep": 1.5}                                            let the screen settle
A required expect that times out stops the script (exit 3); an optional one is skipped, and with
"then" it sends that text only when it matched. Everything the TUI printed goes to the log (ANSI
stripped) so the generator can show what happened. The child is sent SIGTERM at the end.
"""
import json
import os
import pty
import re
import select
import signal
import sys
import time

ANSI = re.compile(rb"\x1b\[[0-9;?<>=]*[a-zA-Z~]|\x1b\][^\x07]*\x07|\x1b[=>()][0-9A-Za-z]?")


def main():
    steps = json.loads(open(os.environ.pop("TUI_SCRIPT")).read())
    log = open(sys.argv[1], "ab")
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(sys.argv[2], sys.argv[2:])
    screen = bytearray()  # text since the last matched expect

    def pump(seconds, pattern=None):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            r, _, _ = select.select([fd], [], [], 0.1)
            if r:
                try:
                    d = os.read(fd, 65536)
                except OSError:
                    return None
                if not d:
                    return None
                clean = ANSI.sub(b"", d)
                log.write(clean)
                screen.extend(clean)
            if pattern is not None and pattern.search(screen.decode("utf-8", "replace")):
                del screen[:]
                return True
        return False

    code = 0
    try:
        for index, st in enumerate(steps):
            if "sleep" in st:
                if pump(st["sleep"]) is None:
                    if any("expect" in later and not later.get("optional") for later in steps[index + 1:]):
                        code = 3
                        log.write(b"\n[tui_driver: child exited before remaining required expectations]\n")
                    break
            elif "expect" in st:
                hit = pump(st.get("timeout", 10), re.compile(st["expect"]))
                if hit is None:
                    if not st.get("optional"):
                        log.write(f"\n[tui_driver: child exited before {st['expect']!r}]\n".encode())
                        code = 3
                        break
                    continue
                if hit and "then" in st:
                    os.write(fd, st["then"].encode())
                if not hit and not st.get("optional"):
                    log.write(f"\n[tui_driver: timed out waiting for {st['expect']!r}]\n".encode())
                    code = 3
                    break
            elif "send" in st:
                os.write(fd, st["send"].encode())
        pump(2)
    finally:
        # Reap the owned child, escalating if it ignores SIGTERM. Blocking waitpid after
        # SIGTERM alone can strand the generator indefinitely.
        done, status = os.waitpid(pid, os.WNOHANG)
        if done:
            result = os.waitstatus_to_exitcode(status)
            if result:
                code = code or (result if result > 0 else 3)
        else:
            os.kill(pid, signal.SIGTERM)
            deadline = time.monotonic() + 1
            while time.monotonic() < deadline:
                done, _ = os.waitpid(pid, os.WNOHANG)
                if done:
                    break
                time.sleep(0.02)
            if not done:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
        os.close(fd)
        log.close()
    sys.exit(code)


if __name__ == "__main__":
    main()
