#!/usr/bin/env python3
"""A local OpenAI chat-completions endpoint that forwards to an upstream one (Z.ai GLM), holding the key.

Goose speaks chat completions natively, so nothing is translated: the request body goes upstream as
Goose sent it (with GLM thinking switched on), and the response bytes,
streamed or not, come back unchanged. The model is the one Goose asked for (GOOSE_MODEL or `--model`),
so a model switch in a session is a real one; --model only answers GET /models. The shim exists so
the key never reaches Goose's config, its env, or its store: Goose gets a dummy key for a provider
at 127.0.0.1:$PORT.

    UPSTREAM_KEY=... chat_shim.py --port 18831 --upstream https://api.z.ai/api/coding/paas/v4 \
        --model glm-5.3-flash --log calls.jsonl --max-calls 40

Every upstream call is one line in --log (message roles and tool names only, never content or
headers). Calls past --max-calls get a 429.
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

    def reply(self, code, body=b"", ctype="application/json"):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            body = json.dumps({"object": "list", "data": [{"id": ARGS.model, "object": "model"}]}).encode()
            return self.reply(200, body)
        self.reply(404)

    def do_POST(self):
        global CALLS
        n = int(self.headers.get("Content-Length") or 0)
        req = json.loads(self.rfile.read(n) or b"{}")
        if not self.path.rstrip("/").endswith("/chat/completions"):
            return self.reply(404)
        with LOCK:
            CALLS += 1
            if ARGS.max_calls and CALLS > ARGS.max_calls:
                return self.reply(429, b'{"error":{"message":"corpus call budget spent"}}')
            with open(ARGS.log, "a") as f:
                f.write(json.dumps({"call": CALLS, "stream": bool(req.get("stream")),
                                    "roles": [m.get("role") for m in req.get("messages") or []],
                                    "tools": [(t.get("function") or {}).get("name") for t in req.get("tools") or []],
                                    "keys": sorted(req)}) + "\n")
        req.setdefault("model", ARGS.model)
        req.setdefault("thinking", {"type": "enabled"})
        up = urllib.request.Request(ARGS.upstream.rstrip("/") + "/chat/completions", data=json.dumps(req).encode(),
                                    headers={"Content-Type": "application/json",
                                             "Authorization": "Bearer " + os.environ["UPSTREAM_KEY"]})
        try:
            r = urllib.request.urlopen(up, timeout=300)
        except urllib.error.HTTPError as e:
            return self.reply(e.code, e.read()[:2000])
        except Exception as e:  # never the request headers
            return self.reply(502, json.dumps({"error": {"message": str(e)[:200]}}).encode())
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
    p.add_argument("--max-calls", type=int, default=40)
    ARGS = p.parse_args()
    if not os.environ.get("UPSTREAM_KEY"):
        sys.exit("UPSTREAM_KEY is not set")
    ThreadingHTTPServer(("127.0.0.1", ARGS.port), H).serve_forever()


if __name__ == "__main__":
    main()
