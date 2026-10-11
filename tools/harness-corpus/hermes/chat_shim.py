#!/usr/bin/env python3
"""A local OpenAI chat-completions endpoint that forwards to a real one, so the harness never holds
the key. Hermes speaks chat completions natively; this only swaps the dummy key for the real one,
pins the model (one --also model may pass through, for a model-switch scenario), turns GLM thinking on, replaces image parts with a text placeholder (the model is
text-only; Hermes still persists the image), and counts calls.

    UPSTREAM_KEY=... chat_shim.py --port 18811 --upstream https://api.z.ai/api/coding/paas/v4 \
        --model glm-5.3-flash --log calls.jsonl --max-calls 40

The key comes from $UPSTREAM_KEY (never argv, never logged). --log gets one JSON line per call with
the request SHAPE only (message roles, tool names, stream flag), never content or headers.
GET /v1/models answers locally with the one model, so context-length probes cost no call.
A request whose last user message contains FAIL_MARKER gets a 500 without reaching upstream (the
provider-error scenario); those are logged with "injected": true and cost no model call.
"""
import argparse
import json
import os
import sys
import threading
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

FAIL_MARKER = "CORPUS-PROVIDER-ERROR"
ARGS = None
LOCK = threading.Lock()
CALLS = 0


def strip_images(msgs):
    for m in msgs:
        c = m.get("content")
        if isinstance(c, list):
            out = []
            for p in c:
                if isinstance(p, dict) and p.get("type") in ("image_url", "input_image", "image"):
                    out.append({"type": "text", "text": "[image attached; this model cannot see images]"})
                else:
                    out.append(p)
            m["content"] = out


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def reply(self, code, body=b"", ctype="application/json"):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            self.reply(200, json.dumps({"object": "list", "data": [
                {"id": m, "object": "model", "owned_by": "corpus", "context_length": ARGS.context}
                for m in [ARGS.model, *ARGS.also]]}).encode())
        else:
            self.reply(404)

    def do_POST(self):
        global CALLS
        n = int(self.headers.get("Content-Length") or 0)
        req = json.loads(self.rfile.read(n) or b"{}")
        if not self.path.rstrip("/").endswith("/chat/completions"):
            self.reply(404)
            return
        users = [m for m in req.get("messages") or [] if m.get("role") == "user"]
        if users and FAIL_MARKER in json.dumps(users[-1].get("content")):
            with open(ARGS.log, "a") as f:
                f.write(json.dumps({"injected": True, "stream": bool(req.get("stream"))}) + "\n")
            self.reply(500, b'{"error":{"message":"corpus shim: injected provider error","type":"server_error"}}')
            return
        with LOCK:
            CALLS += 1
            over = ARGS.max_calls and CALLS > ARGS.max_calls
            with open(ARGS.log, "a") as f:
                f.write(json.dumps({"call": CALLS, "model": req.get("model") if req.get("model") in ARGS.also else ARGS.model, "refused": bool(over), "stream": bool(req.get("stream")),
                                    "roles": [m.get("role") for m in req.get("messages") or []],
                                    "tools": [(t.get("function") or {}).get("name") for t in req.get("tools") or []],
                                    "keys": sorted(req)}) + "\n")
        if over:
            self.reply(429, b'{"error":{"message":"corpus shim: call budget exhausted"}}')
            return
        if req.get("model") not in ARGS.also:
            req["model"] = ARGS.model
        req.setdefault("thinking", {"type": "enabled"})
        strip_images(req.get("messages") or [])
        up = urllib.request.Request(ARGS.upstream.rstrip("/") + "/chat/completions", data=json.dumps(req).encode(),
                                    headers={"Content-Type": "application/json",
                                             "Authorization": "Bearer " + os.environ["UPSTREAM_KEY"]})
        try:
            r = urllib.request.urlopen(up, timeout=300)
        except urllib.error.HTTPError as e:
            self.reply(e.code, e.read()[:2000])
            return
        except Exception as e:  # never echo request headers
            self.reply(502, json.dumps({"error": {"message": "corpus shim upstream: " + str(e)[:200]}}).encode())
            return
        with r:
            self.send_response(r.status)
            self.send_header("Content-Type", r.headers.get("Content-Type", "application/json"))
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            while True:
                chunk = r.read1(65536) if hasattr(r, "read1") else r.read(65536)
                if not chunk:
                    break
                self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
                self.wfile.flush()
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()


class Server(ThreadingHTTPServer):
    daemon_threads = True

    def handle_error(self, request, client_address):
        # A client closing its keep-alive socket is routine; anything else is worth one line.
        e = sys.exc_info()[1]
        if not isinstance(e, (ConnectionResetError, BrokenPipeError)):
            sys.stderr.write(f"chat_shim: {type(e).__name__}: {str(e)[:200]}\n")


def main():
    global ARGS
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, required=True)
    p.add_argument("--upstream", required=True)
    p.add_argument("--model", required=True)
    p.add_argument("--log", required=True)
    p.add_argument("--max-calls", type=int, default=40)
    p.add_argument("--context", type=int, default=131072)
    p.add_argument("--also", action="append", default=[])
    ARGS = p.parse_args()
    if not os.environ.get("UPSTREAM_KEY"):
        sys.exit("UPSTREAM_KEY is not set")
    Server(("127.0.0.1", ARGS.port), H).serve_forever()


if __name__ == "__main__":
    main()
