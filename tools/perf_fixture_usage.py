"""Tiny native usage records: record-count scale without demanded giant output."""
import json


def generate_usage(harness, root, calls):
    if harness not in ("claude", "codex"):
        raise ValueError("usage fixtures support Claude and Codex")
    sid = "perf"
    stamp = "2026-10-12T00:00:00Z"
    path = root / "usage.jsonl"
    with path.open("w", encoding="utf-8") as out:
        def row(value):
            out.write(json.dumps(value, separators=(",", ":")) + "\n")
        if harness == "codex":
            row({"type": "session_meta", "timestamp": stamp,
                 "payload": {"id": sid, "cwd": "/perf", "model": "gpt-perf"}})
            row({"type": "turn_context", "timestamp": stamp,
                 "payload": {"cwd": "/perf", "model": "gpt-perf"}})
        for i in range(calls):
            if harness == "claude":
                for block in range(2):
                    row({"type": "assistant", "uuid": f"{i}-{block}", "sessionId": sid,
                         "timestamp": stamp, "message": {
                             "id": f"msg-{i}", "role": "assistant", "model": "perf-model",
                             "content": [{"type": "text", "text": "small"}],
                             "usage": {"input_tokens": 3, "cache_read_input_tokens": 10,
                                       "cache_creation_input_tokens": 2, "output_tokens": 7}}})
            else:
                row({"type": "event_msg", "timestamp": stamp,
                     "payload": {"type": "agent_message", "message": "small"}})
                row({"type": "event_msg", "timestamp": stamp, "payload": {
                    "type": "token_count", "info": {
                        "last_token_usage": {"input_tokens": 5, "cached_input_tokens": 2,
                                             "output_tokens": 7},
                        "total_token_usage": {"input_tokens": 5 * (i + 1),
                                              "output_tokens": 7 * (i + 1)}}}})
    totals = {"calls": calls, "input": 3 * calls, "output": 7 * calls,
              "cache_read": (10 if harness == "claude" else 2) * calls,
              "cache_write": 2 * calls if harness == "claude" else 0,
              "reasoning": 0, "uncached": (12 if harness == "claude" else 10) * calls,
              "total": (22 if harness == "claude" else 12) * calls, "cost_usd": None}
    return {"harness": harness, "id": sid, "path": str(path), "root": str(root),
            "shape": "usage", "calls": calls, "records": 2 * calls,
            "physical_input_bytes": path.stat().st_size, "expected_totals": totals,
            "expected_duplicates": calls if harness == "claude" else 0,
            "synthetic": True, "provenance": "native usage fields; synthetic IDs/content"}
