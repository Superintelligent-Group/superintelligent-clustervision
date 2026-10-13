"""Synthetic native store shapes based on corpus schemas; no committed corpus is rewritten."""

import json

FORMATS = ("cline-sdk", "opensession", "chatgpt-export", "claude-export", "kimi", "kimi-code", "opencode-files")
SID = "00000000-0000-4000-8000-000000000001"
STAMP = "2026-01-01T00:00:00Z"
EPOCH = 1767225600


def dump(out, value):
    json.dump(value, out, ensure_ascii=False, separators=(",", ":"))


def line(out, value):
    dump(out, value)
    out.write("\n")


def array(out, values):
    out.write("[")
    for index, value in enumerate(values):
        if index:
            out.write(",")
        dump(out, value)
    out.write("]")


def generate(harness, root, size, records, escaped, shape):
    if harness in ("kimi-code", "opencode-files") and shape != "selected":
        raise ValueError(f"{harness} requires --shape selected")
    if shape == "unrelated" and harness not in ("chatgpt-export", "claude-export"):
        raise ValueError("unrelated is an account archive shape")
    if shape.startswith("sidecar-") and harness != "kimi":
        raise ValueError("sidecar shapes require kimi")
    pattern = 'line\n"quoted"\t\\end' if escaped else "plain payload "

    def body(index):
        length = size // records + (index < size % records)
        return (pattern * ((length + len(pattern) - 1) // len(pattern)))[:length]

    def messages(native, count, payload):
        for index in range(count):
            for role, text in (("user", f"prompt {index}"), ("assistant", payload(index))):
                mid = f"m{index}-{role}"
                if native == "cline-sdk":
                    yield {"id": mid, "role": role, "content": [{"type": "text", "text": text}], "ts": EPOCH * 1000}
                elif native == "claude-export":
                    yield {"uuid": mid, "sender": "human" if role == "user" else "assistant",
                           "created_at": STAMP, "content": [{"type": "text", "text": text}]}
                else:
                    yield {"id": mid, "role": role, "kind": "prompt" if role == "user" else "reply",
                           "origin": "human" if role == "user" else "model", "timestamp": STAMP,
                           "content": [{"type": "text", "text": text}]}

    sid = SID
    selected_bytes = size
    expected = records * 2
    discovered = 1
    if harness == "cline-sdk":
        sid = f"{EPOCH * 1000}_perf"
        path = root / ".cline" / "data" / "sessions" / sid / f"{sid}.messages.json"
        path.parent.mkdir(parents=True)
        path.with_name(f"{sid}.json").write_text(json.dumps({"cwd": "/perf", "started_at": STAMP,
                                                          "metadata": {"title": "perf fixture"}}) + "\n")
        with path.open("w", encoding="utf-8") as out:
            out.write('{"version":1,"agent":"lead","updated_at":' + json.dumps(STAMP) + ',"sessionId":' + json.dumps(sid) + ',"messages":')
            array(out, messages(harness, records, body))
            out.write(',"system_prompt":"fixture system"}\n')
    elif harness == "opensession":
        path = root / "fixture.opensession.json"
        with path.open("w", encoding="utf-8") as out:
            out.write('{"open_session":"0.3","harness":"claude","id":' + json.dumps(sid) + ',"title":"perf fixture","created_at":' + json.dumps(STAMP) + ',"updated_at":' + json.dumps(STAMP) + ',"messages":')
            array(out, messages(harness, records, body))
            out.write("}\n")
    elif harness in ("chatgpt-export", "claude-export"):
        path = root / "conversations.json"

        def conversation(out, cid, count, payload):
            if harness == "claude-export":
                out.write('{"uuid":' + json.dumps(cid) + ',"name":"perf fixture","created_at":' + json.dumps(STAMP) + ',"updated_at":' + json.dumps(STAMP) + ',"chat_messages":')
                array(out, messages(harness, count, payload))
                out.write("}")
            else:
                out.write('{"conversation_id":' + json.dumps(cid) + ',"title":"perf fixture","create_time":' + str(EPOCH) + ',"update_time":' + str(EPOCH) + ',"current_node":' + json.dumps(f"m{count-1}-assistant") + ',"mapping":{')
                parent = None
                for index, message in enumerate(messages(harness, count, payload)):
                    if index:
                        out.write(",")
                    mid = message["id"]
                    out.write(json.dumps(mid) + ":")
                    dump(out, {"id": mid, "parent": parent, "children": [], "message": {
                        "id": mid, "author": {"role": message["role"]}, "recipient": "all",
                        "create_time": EPOCH, "content": {"content_type": "text", "parts": [message["content"][0]["text"]]}}})
                    parent = mid
                out.write("}}")

        with path.open("w", encoding="utf-8") as out:
            out.write("[")
            if shape == "unrelated":
                conversation(out, "unrelated", records, body)
                out.write(",")
                conversation(out, sid, 1, lambda _: "selected reply")
                selected_bytes, expected, discovered = len("selected reply"), 2, 2
            else:
                conversation(out, sid, records, body)
            out.write("]\n")
    elif harness == "kimi-code":
        path = root / "sessions" / "wd_perf" / f"session_{sid}"
        wire = path / "agents" / "main" / "wire.jsonl"
        wire.parent.mkdir(parents=True)
        (path / "state.json").write_text(json.dumps({"id": f"session_{sid}", "cwd": "/perf"}))
        with wire.open("w", encoding="utf-8") as out:
            line(out, {"type": "profile.bind", "modelAlias": "test", "systemPrompt": "fixture"})
            def event(value):
                value["stepUuid"] = current_step
                line(out, {"type": "context.append_loop_event", "event": value})
            for index in range(records):
                current_step = f"step-{index}"
                text = body(index)
                quarter = len(text) // 4
                event({"type": "step.begin", "uuid": f"step-{index}"})
                event({"type": "content.part", "part": {"type": "think", "think": text[:quarter]}})
                event({"type": "tool.call", "toolCallId": "call", "name": "Read",
                       "args": {"payload": text[quarter:2 * quarter]}})
                event({"type": "tool.result", "toolCallId": "call", "result": {"output": [
                    {"type": "text", "text": text[2 * quarter:3 * quarter]},
                    {"text": text[3 * quarter:]}]}})
                event({"type": "step.end", "uuid": f"step-{index}", "usage": {"inputOther": 10, "output": 2}})
            current_step = "final"
            event({"type": "step.begin", "uuid": "final"})
            event({"type": "tool.call", "toolCallId": "final", "name": "gate-final", "args": {}})
            event({"type": "step.end", "uuid": "final"})
        expected = records * 2 + 2
    elif harness == "opencode-files":
        storage = root / "storage"
        path = storage / "session" / "perf" / f"{sid}.json"
        path.parent.mkdir(parents=True)
        path.write_text(json.dumps({"id": sid, "directory": "/perf", "title": "selected fixture",
                                   "time": {"created": 1, "updated": 2}}))
        for index in range(records + 1):
            mid = f"m{index:09d}"
            message_path = storage / "message" / sid / f"{mid}.json"
            message_path.parent.mkdir(parents=True, exist_ok=True)
            message_path.write_text(json.dumps({"id": mid, "role": "assistant", "time": {"created": index + 1}}))
            parts = storage / "part" / mid
            parts.mkdir(parents=True)
            if index == records:
                values = [{"type": "tool", "callID": "final", "tool": "gate-final",
                           "state": {"status": "running", "input": {}}}]
            else:
                text = body(index)
                quarter = len(text) // 4
                values = [{"type": "text", "text": text[:quarter]},
                          {"type": "reasoning", "text": text[quarter:2 * quarter]},
                          {"type": "tool", "callID": f"call-{index}", "tool": "read",
                           "state": {"status": "completed", "input": {"payload": text[2 * quarter:3 * quarter]},
                                     "output": {"payload": text[3 * quarter:]}}}]
            for part, value in enumerate(values):
                (parts / f"p{part:03d}.json").write_text(json.dumps(value, ensure_ascii=False, separators=(",", ":")))
        expected = records * 2 + 1
    else:
        path = root / "sessions" / "perf" / sid
        path.mkdir(parents=True)
        if shape == "history":
            with (path / "context.jsonl").open("w", encoding="utf-8") as out:
                for index in range(records):
                    line(out, {"role": "user", "content": f"prompt {index}"})
                    line(out, {"role": "assistant", "content": body(index)})
        else:
            selected_bytes, expected = len("selected winner"), 3
            with (path / "context.jsonl").open("w", encoding="utf-8") as out:
                line(out, {"role": "user", "content": "prompt"})
                line(out, {"role": "assistant", "content": "reply", "tool_calls": [
                    {"id": "selected", "type": "function", "function": {"name": "Shell", "arguments": "{}"}}]})
                line(out, {"role": "tool", "tool_call_id": "selected", "content": "context fallback"})
            with (path / "wire.jsonl").open("w", encoding="utf-8") as out:
                line(out, {"message": {"type": "StatusUpdate", "payload": {"message_id": "reply-id",
                          "token_usage": {"input_other": 12, "output": 3}}}})
                for index in range(records):
                    text = body(index)
                    result = {"is_error": False, "output": text}
                    if shape == "sidecar-unmatched":
                        split = len(text) // 2
                        result.update(output=text[:split], extras={"metadata": text[split:]})
                    line(out, {"timestamp": EPOCH, "message": {"type": "ToolResult", "payload": {
                        "tool_call_id": "selected" if shape == "sidecar-selected" else f"unmatched-{index}",
                        "return_value": result}}})
                if shape == "sidecar-unmatched":
                    for text in ("selected superseded", "selected winner"):
                        line(out, {"timestamp": EPOCH, "message": {"type": "ToolResult", "payload": {
                            "tool_call_id": "selected", "return_value": {"is_error": False, "output": text}}}})
                else:
                    selected_bytes = len(body(records - 1))
    physical = {str(p.relative_to(root)): p.stat().st_size for p in root.rglob("*") if p.is_file()}
    return {"harness": "cline" if harness == "cline-sdk" else "opencode" if harness == "opencode-files" else harness, "format": harness, "shape": shape,
            "id": sid, "path": str(path), "root": str(root), "decoded_payload_bytes": size,
            "selected_payload_bytes": selected_bytes, "physical_input_bytes": sum(physical.values()),
            "files": physical, "records": records, "escaped": escaped, "expected_messages": expected,
            "expected_discovered_sessions": discovered}
