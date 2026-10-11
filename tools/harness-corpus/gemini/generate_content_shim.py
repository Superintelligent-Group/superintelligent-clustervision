#!/usr/bin/env python3
"""A local Gemini API (`generateContent` / `streamGenerateContent`) endpoint backed by an
OpenAI-compatible chat completions API.

Gemini CLI and Qwen Code's Gemini provider speak the Gemini API. Google's free OAuth tier refuses
third-party clients since 2026 ("UNSUPPORTED_CLIENT"), and the keys we hold (Z.ai GLM) speak only chat
completions. Gemini CLI's `gateway` auth (`GOOGLE_GEMINI_BASE_URL`) points the CLI here, and this
translates each request to Z.ai. Only the model backend is adapted; every byte in the store is the
harness's own.

    UPSTREAM_KEY=... generate_content_shim.py --port 18766 --upstream https://api.z.ai/api/coding/paas/v4 \
        --model glm-5.3-flash --log calls.jsonl

The key comes from $UPSTREAM_KEY (never argv, never logged). Each upstream call is one line in --log
(request shape only: part kinds, tool names, the path; never content or headers). `countTokens` is
answered locally (chars / 4) and is not a model call. Images become a text placeholder (the model is
text-only; the harness still persists the image part itself).
"""
import argparse
import json
import os
import re
import sys
import threading
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ARGS = None
LOCK = threading.Lock()
CALLS = 0


def lower_types(schema):
    """Gemini `parameters` schemas use OpenAPI upper-case type names (`OBJECT`); JSON Schema wants lower."""
    if isinstance(schema, dict):
        out = {}
        for k, v in schema.items():
            if k == "type" and isinstance(v, str):
                out[k] = v.lower()
            elif k in ("propertyOrdering", "nullable"):
                continue
            else:
                out[k] = lower_types(v)
        return out
    if isinstance(schema, list):
        return [lower_types(v) for v in schema]
    return schema


def tools_to_chat(tools):
    chat = []
    for t in tools or []:
        for d in t.get("functionDeclarations") or []:
            params = d.get("parametersJsonSchema") or d.get("parameters") or {"type": "object", "properties": {}}
            chat.append({"type": "function", "function": {
                "name": d.get("name", ""), "description": (d.get("description") or "")[:2000],
                "parameters": lower_types(params)}})
    return chat


def part_text(p):
    if "text" in p and not p.get("thought"):
        return p["text"]
    if "inlineData" in p or "fileData" in p:
        return "[an image or file is attached; this model cannot see it]"
    return None


def text_of(content):
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    parts = content.get("parts", []) if isinstance(content, dict) else content
    return "\n".join(t for t in (part_text(p) for p in parts if isinstance(p, dict)) if t)


def contents_to_messages(system, contents):
    msgs = []
    sys_text = text_of(system)
    if sys_text:
        msgs.append({"role": "system", "content": sys_text})
    pending = []  # (name, id) of the last assistant turn's calls not yet answered
    for c in contents or []:
        role = c.get("role", "user")
        parts = c.get("parts") or []
        if role == "model":
            text = "\n".join(t for t in (part_text(p) for p in parts) if t)
            calls = []
            for p in parts:
                fc = p.get("functionCall")
                if fc:
                    cid = fc.get("id") or ("call_" + uuid.uuid4().hex[:16])
                    calls.append({"id": cid, "type": "function", "function": {
                        "name": fc.get("name", ""), "arguments": json.dumps(fc.get("args") or {})}})
            m = {"role": "assistant", "content": text}
            if calls:
                m["tool_calls"] = calls
                pending = [(x["function"]["name"], x["id"]) for x in calls]
            msgs.append(m)
            continue
        texts = []
        for p in parts:
            fr = p.get("functionResponse")
            if fr:
                cid = fr.get("id")
                ids = [i for _, i in pending]
                if cid not in ids:
                    match = [x for x in pending if x[0] == fr.get("name")] or pending
                    cid = match[0][1] if match else ("call_" + uuid.uuid4().hex[:16])
                pending = [x for x in pending if x[1] != cid]
                resp = fr.get("response")
                msgs.append({"role": "tool", "tool_call_id": cid,
                             "content": resp if isinstance(resp, str) else json.dumps(resp)})
            else:
                t = part_text(p)
                if t:
                    texts.append(t)
        if texts:
            msgs.append({"role": "user", "content": "\n".join(texts)})
    return msgs


def upstream(body):
    req = urllib.request.Request(ARGS.upstream.rstrip("/") + "/chat/completions", data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json",
                                          "Authorization": "Bearer " + os.environ["UPSTREAM_KEY"]})
    with urllib.request.urlopen(req, timeout=300) as r:
        return json.load(r)


def kinds(contents):
    out = []
    for c in contents or []:
        for p in c.get("parts") or []:
            out.append(next((k for k in ("functionCall", "functionResponse", "inlineData", "fileData") if k in p),
                            "thought" if p.get("thought") else "text"))
    return out


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        self.send_response(404)
        self.end_headers()

    def reply_json(self, obj, code=200):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_POST(self):
        global CALLS
        n = int(self.headers.get("Content-Length") or 0)
        req = json.loads(self.rfile.read(n) or b"{}")
        path = self.path.split("?")[0]
        m = re.search(r"/models/([^/:]+):(\w+)$", path)
        if not m:
            self.reply_json({"error": {"code": 404, "message": "not found", "status": "NOT_FOUND"}}, 404)
            return
        asked, method = m.group(1), m.group(2)
        if method == "countTokens":
            chars = len(json.dumps(req.get("contents") or req.get("generateContentRequest") or {}))
            self.reply_json({"totalTokens": max(1, chars // 4)})
            return
        if method not in ("generateContent", "streamGenerateContent"):
            self.reply_json({"error": {"code": 404, "message": f"{method} unsupported", "status": "NOT_FOUND"}}, 404)
            return
        cfg = req.get("generationConfig") or {}
        msgs = contents_to_messages(req.get("systemInstruction"), req.get("contents"))
        body = {"model": ARGS.model, "messages": msgs}
        thinking = (cfg.get("thinkingConfig") or {}).get("includeThoughts")
        body["thinking"] = {"type": "enabled" if thinking else "disabled"}
        schema = cfg.get("responseJsonSchema") or cfg.get("responseSchema")
        if cfg.get("responseMimeType") == "application/json" or schema:
            body["response_format"] = {"type": "json_object"}
            note = "Reply with one JSON object only."
            if schema:
                note += " It must match this JSON schema: " + json.dumps(lower_types(schema))
            msgs.append({"role": "user", "content": note})
        tools = tools_to_chat(req.get("tools"))
        if tools:
            body["tools"] = tools
        with LOCK:
            CALLS += 1
            over = ARGS.max_calls and CALLS > ARGS.max_calls
            with open(ARGS.log, "a") as f:
                f.write(json.dumps({"call": CALLS, "method": method, "model_asked": asked,
                                    "part_kinds": kinds(req.get("contents")), "json_mode": bool(schema),
                                    "thinking": bool(thinking), "tools": [t["function"]["name"] for t in tools],
                                    "refused": bool(over)}) + "\n")
        if over:
            self.reply_json({"error": {"code": 429, "message": "corpus budget exhausted",
                                       "status": "RESOURCE_EXHAUSTED"}}, 429)
            return
        try:
            resp = upstream(body)
        except Exception as e:  # never echo request headers
            self.reply_json({"error": {"code": 500, "message": "upstream: " + str(e)[:200], "status": "INTERNAL"}}, 500)
            return
        msg = (resp.get("choices") or [{}])[0].get("message") or {}
        parts = []
        if msg.get("reasoning_content") and thinking:
            parts.append({"text": msg["reasoning_content"], "thought": True})
        if msg.get("content"):
            parts.append({"text": msg["content"]})
        for tc in msg.get("tool_calls") or []:
            fn = tc.get("function") or {}
            try:
                args = json.loads(fn.get("arguments") or "{}")
            except ValueError:
                args = {}
            parts.append({"functionCall": {"name": fn.get("name", ""), "args": args,
                                           "id": tc.get("id") or ("call_" + uuid.uuid4().hex[:16])}})
        if not parts:
            parts.append({"text": ""})
        u = resp.get("usage") or {}
        out = {
            "candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": "STOP", "index": 0}],
            "usageMetadata": {
                "promptTokenCount": u.get("prompt_tokens", 0),
                "candidatesTokenCount": u.get("completion_tokens", 0),
                "cachedContentTokenCount": (u.get("prompt_tokens_details") or {}).get("cached_tokens", 0),
                "thoughtsTokenCount": (u.get("completion_tokens_details") or {}).get("reasoning_tokens", 0),
                "totalTokenCount": u.get("total_tokens", 0)},
            "modelVersion": asked,
            "responseId": uuid.uuid4().hex[:24],
        }
        if method == "generateContent":
            self.reply_json(out)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        self.wfile.write(b"data: " + json.dumps(out).encode() + b"\r\n\r\n")
        self.wfile.flush()


def main():
    global ARGS
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, required=True)
    p.add_argument("--upstream", required=True)
    p.add_argument("--model", required=True)
    p.add_argument("--log", required=True)
    p.add_argument("--max-calls", type=int, default=60)
    ARGS = p.parse_args()
    if not os.environ.get("UPSTREAM_KEY"):
        sys.exit("UPSTREAM_KEY is not set")
    ThreadingHTTPServer(("127.0.0.1", ARGS.port), H).serve_forever()


if __name__ == "__main__":
    main()
