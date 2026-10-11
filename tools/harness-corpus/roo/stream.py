#!/usr/bin/env python3
"""Drive one Roo CLI process through its NDJSON stdin protocol (`--stdin-prompt-stream`): a
`start`, then one `message` per later turn (each answers the completion ask, so the same task
continues), then `shutdown`. Each turn waits for the stream's `result` event.

    stream.py --log LOG [--start-json JSON] -- roo ARGS...   < turns (one JSON object per line:
                                                               {"prompt": ..., "images": [...]})

Prints one line per turn. Never prints prompts' answers; the raw stream goes to --log.
"""
import argparse
import json
import subprocess
import sys
import threading
import queue
import uuid


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--log", required=True)
    p.add_argument("--timeout", type=int, default=300)
    p.add_argument("cmd", nargs=argparse.REMAINDER)
    a = p.parse_args()
    cmd = a.cmd[1:] if a.cmd and a.cmd[0] == "--" else a.cmd
    turns = [json.loads(l) for l in sys.stdin if l.strip()]
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    events = queue.Queue()
    log = open(a.log, "a")

    def reader():
        for line in proc.stdout:
            log.write(line)
            log.flush()
            try:
                events.put(json.loads(line))
            except ValueError:
                pass
        events.put(None)

    threading.Thread(target=reader, daemon=True).start()

    def send(obj):
        proc.stdin.write(json.dumps(obj) + "\n")
        proc.stdin.flush()

    ok = True
    for i, t in enumerate(turns):
        req = str(uuid.uuid4())
        cmd_obj = {"command": "start" if i == 0 else "message", "requestId": req, "prompt": t["prompt"]}
        for k in ("images", "taskId", "configuration"):
            if k in t:
                cmd_obj[k] = t[k]
        send(cmd_obj)
        while True:
            try:
                ev = events.get(timeout=a.timeout)
            except queue.Empty:
                print(f"turn {i}: timeout")
                ok = False
                break
            if ev is None:
                print(f"turn {i}: roo exited")
                ok = False
                break
            if ev.get("type") == "result":
                print(f"turn {i}: result success={ev.get('success')}")
                break
            if ev.get("type") == "assistant" and ev.get("subtype") == "followup" and ev.get("done"):
                print(f"turn {i}: follow-up question")  # the next message answers it
                break
            if ev.get("type") == "control" and ev.get("subtype") == "error":
                print(f"turn {i}: control error {ev.get('code')}")
                ok = False
                break
        if not ok:
            break
    try:
        send({"command": "shutdown", "requestId": str(uuid.uuid4())})
        proc.stdin.close()
    except BrokenPipeError:
        pass
    try:
        proc.wait(timeout=60)
    except subprocess.TimeoutExpired:
        proc.terminate()
        proc.wait(timeout=10)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
