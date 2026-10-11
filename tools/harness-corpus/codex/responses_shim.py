#!/usr/bin/env python3
"""A local OpenAI *Responses API* endpoint backed by an OpenAI-compatible *chat completions* API.

Codex 0.16x speaks only the Responses wire (`wire_api = "responses"`); the cheap models we hold keys
for (Z.ai GLM) speak only chat completions. This translates one to the other so Codex can be driven
by a real model and write its rollouts itself. Only the model backend is adapted; every byte in the
store is Codex's own.

    UPSTREAM_KEY=... responses_shim.py --port 18765 --upstream https://api.z.ai/api/coding/paas/v4 \
        --log /path/requests.jsonl

The key comes from $UPSTREAM_KEY (never from argv, never logged). Each upstream call is counted in
--log (one JSON line per call, request shape only: item and tool TYPES, never content or headers).
Images in the input are replaced by a text placeholder (the model is text-only; Codex still persists
the image item itself).
"""
import argparse
import json
import os
import sys
import threading
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ARGS = None
LOCK = threading.Lock()
CALLS = 0


def text_of(content):
    if isinstance(content, str):
        return content
    out = []
    for p in content or []:
        t = p.get("type")
        if t in ("input_text", "output_text", "text", "summary_text", "reasoning_text"):
            out.append(p.get("text", ""))
        elif t == "input_image":
            out.append("[image attached; this model cannot see images]")
    return "\n".join(out)


def tools_to_chat(tools):
    """Responses tool list -> (chat tools, {flat name: (kind, namespace, name)})."""
    chat, names = [], {}

    def add(t, ns=None):
        kind = t.get("type")
        name = t.get("name", "")
        flat = f"{ns}{name}" if ns else name
        if kind == "function":
            params = t.get("parameters") or {"type": "object", "properties": {}}
        elif kind == "custom":
            params = {"type": "object", "properties": {"input": {"type": "string", "description": "the raw tool input"}},
                      "required": ["input"]}
        else:
            return
        names[flat] = (kind, ns, name)
        chat.append({"type": "function", "function": {"name": flat, "description": (t.get("description") or "")[:2000],
                                                      "parameters": params}})

    for t in tools or []:
        if t.get("type") == "namespace":
            for c in t.get("tools", []):
                add(c, t.get("name", ""))
        else:
            add(t)
    return chat, names


def input_to_messages(instructions, items, names):
    msgs = []
    if instructions:
        msgs.append({"role": "system", "content": instructions})
    rev = {(ns, n): flat for flat, (_, ns, n) in names.items()}

    def assistant():
        if not msgs or msgs[-1]["role"] != "assistant" or msgs[-1].get("_closed"):
            msgs.append({"role": "assistant", "content": ""})
        return msgs[-1]

    for it in items or []:
        t = it.get("type")
        if t == "message":
            role = {"developer": "system"}.get(it.get("role"), it.get("role"))
            txt = text_of(it.get("content"))
            if role == "assistant":
                a = assistant()
                a["content"] = (a["content"] + "\n" + txt).strip() if a["content"] else txt
                a["_closed"] = True
            else:
                msgs.append({"role": role, "content": txt})
        elif t in ("function_call", "custom_tool_call"):
            a = assistant()
            if a.get("_closed") and not a.get("tool_calls"):
                pass
            flat = rev.get((it.get("namespace"), it.get("name")), it.get("name"))
            args = it.get("arguments") if t == "function_call" else json.dumps({"input": it.get("input", "")})
            a.setdefault("tool_calls", []).append(
                {"id": it.get("call_id"), "type": "function", "function": {"name": flat, "arguments": args or "{}"}})
            a.pop("_closed", None)
        elif t in ("function_call_output", "custom_tool_call_output"):
            out = it.get("output")
            msgs.append({"role": "tool", "tool_call_id": it.get("call_id"),
                         "content": out if isinstance(out, str) else text_of(out)})
        # reasoning, compaction summaries etc. are not forwarded (the model keeps no hidden state)
        elif t == "compaction" or t == "context_compaction":
            s = it.get("summary") or it.get("encrypted_content") or ""
            if isinstance(s, str) and s:
                msgs.append({"role": "user", "content": s})
    for m in msgs:
        m.pop("_closed", None)
    return msgs


def upstream(body):
    req = urllib.request.Request(ARGS.upstream.rstrip("/") + "/chat/completions", data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json",
                                          "Authorization": "Bearer " + os.environ["UPSTREAM_KEY"]})
    with urllib.request.urlopen(req, timeout=300) as r:
        return json.load(r)


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        self.send_response(404)
        self.end_headers()

    def do_POST(self):
        global CALLS
        n = int(self.headers.get("Content-Length") or 0)
        req = json.loads(self.rfile.read(n) or b"{}")
        if not self.path.rstrip("/").endswith("/responses"):
            self.send_response(404)
            self.end_headers()
            return
        # Responses-lite models get (some) tool definitions as `additional_tools` input items.
        all_tools = list(req.get("tools") or [])
        for it in req.get("input") or []:
            if it.get("type") == "additional_tools":
                all_tools += it.get("tools") or []
        tools, names = tools_to_chat(all_tools)
        body = {"model": ARGS.model, "messages": input_to_messages(req.get("instructions"), req.get("input"), names),
                "thinking": {"type": "enabled"}}
        if tools:
            body["tools"] = tools
        with LOCK:
            CALLS += 1
            if ARGS.max_calls and CALLS > ARGS.max_calls:
                self.send_response(429)
                self.end_headers()
                return
            with open(ARGS.log, "a") as f:
                f.write(json.dumps({"call": CALLS, "path": self.path,
                                    "input_types": [i.get("type") for i in req.get("input") or []],
                                    "tools": [(t.get("type"), t.get("name")) for t in req.get("tools") or []],
                                    "keys": sorted(req)}) + "\n")
        try:
            resp = upstream(body)
        except Exception as e:  # report as a failed response, never the request headers
            self.sse([{"type": "response.failed", "response": {"id": "resp_err", "error": {"code": "upstream", "message": str(e)[:200]}}}])
            return
        msg = (resp.get("choices") or [{}])[0].get("message") or {}
        rid = "resp_" + uuid.uuid4().hex[:24]
        evs = [{"type": "response.created", "response": {"id": rid}}]
        if msg.get("reasoning_content"):
            evs.append({"type": "response.output_item.done", "item": {
                "type": "reasoning", "id": "rs_" + uuid.uuid4().hex[:24],
                "summary": [{"type": "summary_text", "text": msg["reasoning_content"]}]}})
        if msg.get("content"):
            mid = "msg_" + uuid.uuid4().hex[:24]
            evs.append({"type": "response.output_item.added", "item": {
                "type": "message", "role": "assistant", "id": mid, "content": []}})
            evs.append({"type": "response.output_text.delta", "item_id": mid, "delta": msg["content"]})
            evs.append({"type": "response.output_item.done", "item": {
                "type": "message", "role": "assistant", "id": mid,
                "content": [{"type": "output_text", "text": msg["content"]}]}})
        for tc in msg.get("tool_calls") or []:
            fn = tc.get("function") or {}
            kind, ns, name = names.get(fn.get("name"), ("function", None, fn.get("name")))
            call_id = tc.get("id") or ("call_" + uuid.uuid4().hex[:16])
            if kind == "custom":
                try:
                    inp = json.loads(fn.get("arguments") or "{}").get("input", "")
                except ValueError:
                    inp = fn.get("arguments") or ""
                item = {"type": "custom_tool_call", "call_id": call_id, "name": name, "input": inp}
            else:
                item = {"type": "function_call", "call_id": call_id, "name": name, "arguments": fn.get("arguments") or "{}"}
            if ns:
                item["namespace"] = ns
            evs.append({"type": "response.output_item.done", "item": item})
        u = resp.get("usage") or {}
        evs.append({"type": "response.completed", "response": {"id": rid, "usage": {
            "input_tokens": u.get("prompt_tokens", 0),
            "input_tokens_details": {"cached_tokens": (u.get("prompt_tokens_details") or {}).get("cached_tokens", 0)},
            "output_tokens": u.get("completion_tokens", 0),
            "output_tokens_details": {"reasoning_tokens": (u.get("completion_tokens_details") or {}).get("reasoning_tokens", 0)},
            "total_tokens": u.get("total_tokens", 0)}}})
        self.sse(evs)

    def sse(self, evs):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for e in evs:
            self.wfile.write(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n".encode())
        self.wfile.flush()


def main():
    global ARGS
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, required=True)
    p.add_argument("--upstream", required=True)
    p.add_argument("--model", required=True)
    p.add_argument("--log", required=True)
    p.add_argument("--max-calls", type=int, default=40)
    ARGS = p.parse_args()
    if not os.environ.get("UPSTREAM_KEY"):
        sys.exit("UPSTREAM_KEY is not set")
    ThreadingHTTPServer(("127.0.0.1", ARGS.port), H).serve_forever()


if __name__ == "__main__":
    main()
