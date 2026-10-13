#!/usr/bin/env python3
"""Create a fresh synthetic Codex source for the verified Claude-port ownership control."""
import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("output", type=Path)
parser.add_argument("--characters", type=int, default=64 * 1024 * 1024)
args = parser.parse_args()
if args.characters < 0:
    parser.error("characters must be nonnegative")
pattern = "escaped\nλ thought "
text = (pattern * ((args.characters + len(pattern) - 1) // len(pattern)))[:args.characters]
with args.output.open("x", encoding="utf-8") as stream:
    def row(kind, payload):
        stream.write(json.dumps(dict(timestamp="2026-10-12T12:00:00Z", type=kind,
                                     payload=payload), separators=(",", ":")) + "\n")
    row("session_meta", dict(id="synthetic-source", cwd="/synthetic/claude-export",
                             originator="controlled-probe"))
    row("turn_context", dict(model="controlled-model", cwd="/synthetic/claude-export"))
    row("event_msg", dict(type="user_message", message="initial controlled user"))
    row("response_item", dict(type="reasoning", summary=[dict(type="summary_text", text=text)]))
    row("response_item", dict(type="function_call", call_id="call1", name="Read",
                              arguments='{"path":"/synthetic/input"}'))
    row("response_item", dict(type="function_call_output", call_id="call1",
                              output="controlled tool result"))
    row("response_item", dict(type="message", role="assistant", content=[
        dict(type="output_text", text="final controlled assistant")]))
