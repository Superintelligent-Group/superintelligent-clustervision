#!/usr/bin/env python3
"""A local pass-through for an OpenAI-compatible chat completions API that adds the key itself.

Qwen Code speaks chat completions natively, so nothing is translated: the proxy exists so the key
never enters the harness's environment, argv or config (the harness gets a dummy key for
127.0.0.1), and so every model call is counted against the corpus budget.

    UPSTREAM_KEY=... chat_proxy.py --port 18767 --upstream https://api.z.ai/api/coding/paas/v4 \
        --model glm-5.3-flash --log calls.jsonl --max-calls 60

The `model` field is rewritten to --model (the harness may ask for any name). Each call is one line
in --log (path, the asked model, message roles, tool count, stream flag; never content or headers).
The response (SSE or JSON) is streamed back byte for byte.
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


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def fail(self, code, msg):
        b = json.dumps({"error": {"message": msg, "type": "corpus_proxy", "code": code}}).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            b = json.dumps({"object": "list", "data": [{"id": ARGS.model, "object": "model"}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(b)))
            self.end_headers()
            self.wfile.write(b)
            return
        self.fail(404, "not found")

    def do_POST(self):
        global CALLS
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n) or b"{}"
        if not self.path.split("?")[0].rstrip("/").endswith("/chat/completions"):
            self.fail(404, "only /chat/completions is proxied")
            return
        req = json.loads(raw)
        asked = req.get("model")
        req["model"] = ARGS.model
        with LOCK:
            CALLS += 1
            over = ARGS.max_calls and CALLS > ARGS.max_calls
            with open(ARGS.log, "a") as f:
                f.write(json.dumps({"call": CALLS, "model_asked": asked, "stream": bool(req.get("stream")),
                                    "roles": [m.get("role") for m in req.get("messages") or []],
                                    "tools": len(req.get("tools") or []), "refused": bool(over)}) + "\n")
        if over:
            self.fail(429, "corpus budget exhausted")
            return
        up = urllib.request.Request(ARGS.upstream.rstrip("/") + "/chat/completions", data=json.dumps(req).encode(),
                                    headers={"Content-Type": "application/json",
                                             "Accept": self.headers.get("Accept", "*/*"),
                                             "Authorization": "Bearer " + os.environ["UPSTREAM_KEY"]})
        try:
            r = urllib.request.urlopen(up, timeout=300)
        except urllib.error.HTTPError as e:
            self.fail(e.code, "upstream: " + e.read()[:300].decode("utf-8", "replace"))
            return
        except Exception as e:  # never echo request headers
            self.fail(502, "upstream: " + str(e)[:200])
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
