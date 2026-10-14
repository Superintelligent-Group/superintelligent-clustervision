#!/usr/bin/env python3
"""Drive an interactive CLI through a pseudo-terminal: type each line, wait until the program has
been quiet for --idle seconds (and at least --min seconds have passed), then type the next.

    drive_tty.py --log out.log [--idle 4] [--min 2] [--timeout 300] -l '/compress' -l '/exit' -- cmd args...

Hermes only dispatches slash commands (/compress, /branch, /model, /undo, /title, ...) in its
interactive CLI; -q/-Q runs submit text literally. Output goes to --log only.
"""
import argparse
import os
import pty
import select
import signal
import sys
import time


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--log", required=True)
    p.add_argument("--idle", type=float, default=4.0)
    p.add_argument("--min", type=float, default=2.0)
    p.add_argument("--startup", type=float, default=8.0)
    p.add_argument("--timeout", type=float, default=300.0)
    p.add_argument("-l", "--line", action="append", default=[])
    p.add_argument("--paste", action="store_true", help="send each line as one bracketed paste")
    p.add_argument("--watch", action="append", default=[],
                   help="a path that must still exist after every line; the drive stops (exit 3) when one vanishes")
    p.add_argument("cmd", nargs=argparse.REMAINDER)
    a = p.parse_args()
    cmd = a.cmd[1:] if a.cmd and a.cmd[0] == "--" else a.cmd
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.environ.setdefault("COLUMNS", "160")
        os.environ.setdefault("LINES", "50")
        os.execvp(cmd[0], cmd)
    log = open(a.log, "ab")
    deadline = time.monotonic() + a.timeout

    def pump(quiet_for, at_least):
        start = time.monotonic()
        last = time.monotonic()
        while time.monotonic() < deadline:
            r, _, _ = select.select([fd], [], [], 0.25)
            if r:
                try:
                    b = os.read(fd, 65536)
                except OSError:
                    return False
                if not b:
                    return False
                log.write(b)
                log.flush()
                last = time.monotonic()
            elif time.monotonic() - last >= quiet_for and time.monotonic() - start >= at_least:
                return True
        return False

    code = 0
    alive = pump(a.idle, a.startup)
    for line in a.line:
        if not alive:
            code = 3
            break
        log.write(f"\n<<< {line}\n".encode())
        if a.paste:
            # One bracketed paste: the composer inserts the line whole, so per-keystroke slash
            # completion (Hermes 0.21 swallows spaces into its completion menu) never runs.
            os.write(fd, b"\x1b[200~" + line.encode() + b"\x1b[201~")
        else:
            for ch in line:
                os.write(fd, ch.encode())
                time.sleep(0.02)
        time.sleep(0.5)
        os.write(fd, b"\r")
        alive = pump(a.idle, a.min)
        gone = [w for w in a.watch if not os.path.exists(w)]
        if gone:
            sys.stderr.write(f"drive_tty: {gone[0]} vanished after typing {line!r}; stopping\n")
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
            sys.exit(3)
    if time.monotonic() >= deadline:
        code = 3
    done, status = 0, 0
    terminated_by_driver = False
    end = time.monotonic() + 20
    while time.monotonic() < end:
        done, status = os.waitpid(pid, os.WNOHANG)
        if done:
            break
        pump(0.5, 0)
    else:
        terminated_by_driver = True
        os.kill(pid, signal.SIGTERM)
        time.sleep(2)
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    if not done:
        _, status = os.waitpid(pid, 0)
    if not terminated_by_driver:
        result = os.waitstatus_to_exitcode(status)
        if result:
            code = code or (result if result > 0 else 3)
    os.close(fd)
    log.close()
    sys.exit(code)


if __name__ == "__main__":
    main()
