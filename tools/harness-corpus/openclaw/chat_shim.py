#!/usr/bin/env python3
"""A local OpenAI chat-completions forwarder: the harness talks to 127.0.0.1 with a dummy key, the
shim adds the real key and forwards to an OpenAI-compatible upstream (Z.ai GLM), streaming the reply
back byte for byte. It exists so the key never reaches the harness's config, env, or store.

    UPSTREAM_KEY=... chat_shim.py --port 18821 --upstream https://api.z.ai/api/coding/paas/v4 \
        --model glm-5.3-flash --log calls.jsonl --max-calls 60

Every request is answered by --model (whatever model id the harness sends). The `developer` role becomes `system` and image parts are
replaced by a text placeholder (the model is text-only; the harness still persists the image).
Each call is logged as one JSON line of request SHAPE only (message roles, tool names), never
content or headers. Calls past --max-calls get HTTP 429.
"""
import argparse
import json
import os
import sys
import threading
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ARGS = None
LOCK = threading.Lock()
CALLS = 0


def adapt_messages(msgs):
    for m in msgs:
        if m.get("role") == "developer":  # Z.ai: 400 "Incorrect role information"
            m["role"] = "system"
        c = m.get("content")
        if isinstance(c, list):
            m["content"] = [p if p.get("type") not in ("image_url", "input_image", "image")
                            else {"type": "text", "text": "[image attached; this model cannot see images]"} for p in c]


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            body = json.dumps({"object": "list", "data": [{"id": ARGS.model, "object": "model"}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(404)
        self.end_headers()

    def do_POST(self):
        global CALLS
        n = int(self.headers.get("Content-Length") or 0)
        req = json.loads(self.rfile.read(n) or b"{}")
        if not self.path.rstrip("/").endswith("/chat/completions"):
            self.send_response(404)
            self.end_headers()
            return
        with LOCK:
            CALLS += 1
            if ARGS.max_calls and CALLS > ARGS.max_calls:
                self.send_response(429)
                self.end_headers()
                return
            with open(ARGS.log, "a") as f:
                f.write(json.dumps({"call": CALLS, "model_sent": req.get("model"), "stream": bool(req.get("stream")),
                                    "roles": [m.get("role") for m in req.get("messages") or []],
                                    "tools": [(t.get("function") or {}).get("name") for t in req.get("tools") or []],
                                    "keys": sorted(req)}) + "\n")
        req["model"] = ARGS.model
        adapt_messages(req.get("messages") or [])
        req.setdefault("thinking", {"type": "enabled"})
        # OpenAI-only knobs Z.ai rejects
        for k in ("store", "prompt_cache_key", "reasoning_effort", "parallel_tool_calls", "metadata", "service_tier"):
            req.pop(k, None)
        up = urllib.request.Request(ARGS.upstream.rstrip("/") + "/chat/completions", data=json.dumps(req).encode(),
                                    headers={"Content-Type": "application/json",
                                             "Authorization": "Bearer " + os.environ["UPSTREAM_KEY"]})
        try:
            r = urllib.request.urlopen(up, timeout=300)
        except urllib.error.HTTPError as e:
            body = e.read()[:2000]
            self.send_response(e.code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        except Exception as e:  # never the request headers
            body = json.dumps({"error": {"message": "shim upstream: " + str(e)[:200]}}).encode()
            self.send_response(502)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        with r:
            self.send_response(r.status)
            self.send_header("Content-Type", r.headers.get("Content-Type", "application/json"))
            self.end_headers()
            while True:
                chunk = r.read1(65536) if hasattr(r, "read1") else r.read(65536)
                if not chunk:
                    break
                self.wfile.write(chunk)
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
