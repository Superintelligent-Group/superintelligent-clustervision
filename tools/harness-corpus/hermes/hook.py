#!/usr/bin/env python3
"""Shell hook for the corpus hooks scenario (Hermes JSON wire protocol on stdin/stdout).

pre_tool_call: blocks any `terminal` command containing the word "forbidden".
pre_llm_call:  injects one line of context into the turn.
"""
import json
import sys

ev = json.load(sys.stdin)
name = ev.get("hook_event_name")
if name == "pre_tool_call":
    cmd = str((ev.get("tool_input") or {}).get("command", ""))
    print(json.dumps({"decision": "block", "reason": "corpus hook: 'forbidden' commands are blocked"}
                     if "forbidden" in cmd else {}))
elif name == "pre_llm_call":
    print(json.dumps({"context": "corpus-hook: the build is green."}))
else:
    print("{}")
