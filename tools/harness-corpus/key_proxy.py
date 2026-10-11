#!/usr/bin/env python3
"""A local pass-through to an OpenAI- or Anthropic-compatible model API that adds the real key.

Harnesses that already speak the provider's wire (chat completions, Anthropic messages) still must
not hold the key: it would land in their config, their logs, or argv. Point the harness at
http://127.0.0.1:PORT with any dummy key; this forwards each request to --upstream with the key from
$UPSTREAM_KEY (never argv, never logged) and streams the answer back byte for byte.

    UPSTREAM_KEY=... key_proxy.py --port 18766 --upstream https://api.z.ai/api/coding/paas/v4 \
        --log calls.jsonl --max-calls 40 [--model glm-5.3-flash] [--auth bearer|x-api-key]

The request path after the proxy root is appended to --upstream, minus a leading /v1 when the
upstream already ends in a version segment, so a harness may be given either `.../v1` or the bare
root. --model rewrites the request's "model" (harnesses insist on their own catalog names).
Every model call (a POST whose body has "messages") is counted in --log as one JSON line holding
the path, the message count, the offered tool NAMES and the stream flag; never content or headers. Calls
past --max-calls get a 429.
"""
import argparse
import json
import os
import re
import threading
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ARGS = None
KEY = None
LOCK = threading.Lock()
CALLS = 0
DROP = {"host", "authorization", "x-api-key", "content-length", "connection", "accept-encoding"}


def tool_names(req):
    out = []
    for t in req.get("tools") or []:
        if isinstance(t, dict):
            out.append((t.get("function") or {}).get("name") or t.get("name") or t.get("type") or "?")
    return out


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def target(self):
        path = self.path
        if re.search(r"/v\d+$", ARGS.upstream.rstrip("/")) and re.match(r"^/v\d+(/|$)", path):
            path = re.sub(r"^/v\d+", "", path, count=1)
        return ARGS.upstream.rstrip("/") + path

    def forward(self, method):
        global CALLS
        n = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(n) if n else None
        if body and method == "POST":
            try:
                req = json.loads(body)
            except ValueError:
                req = None
            if isinstance(req, dict) and "messages" in req:
                with LOCK:
                    CALLS += 1
                    over = CALLS > ARGS.max_calls
                    with open(ARGS.log, "a") as f:
                        f.write(json.dumps({"path": self.path.split("?")[0], "messages": len(req["messages"]),
                                            "tools": tool_names(req), "stream": bool(req.get("stream")),
                                            "refused": over}) + "\n")
                if over:
                    return self.reply(429, b'{"error":{"message":"corpus budget exhausted (key_proxy --max-calls)"}}')
                if ARGS.model:
                    req["model"] = ARGS.model
                    body = json.dumps(req).encode()
        headers = {k: v for k, v in self.headers.items() if k.lower() not in DROP}
        if ARGS.auth == "bearer":
            headers["Authorization"] = "Bearer " + KEY
        else:
            headers["x-api-key"] = KEY
        r = urllib.request.Request(self.target(), data=body, method=method, headers=headers)
        try:
            resp = urllib.request.urlopen(r, timeout=600)
        except urllib.error.HTTPError as e:
            resp = e
        except Exception as e:  # network: report as a gateway error, without the request
            return self.reply(502, json.dumps({"error": {"message": f"key_proxy upstream: {type(e).__name__}"}}).encode())
        self.send_response(resp.status if hasattr(resp, "status") else resp.code)
        for k, v in resp.headers.items():
            if k.lower() not in ("transfer-encoding", "connection", "content-length", "content-encoding"):
                self.send_header(k, v)
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        while True:
            chunk = resp.read1(65536) if hasattr(resp, "read1") else resp.read(65536)
            if not chunk:
                break
            self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
            self.wfile.flush()
        self.wfile.write(b"0\r\n\r\n")

    def reply(self, code, body):
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        self.forward("POST")

    def do_GET(self):
        self.forward("GET")


def main():
    global ARGS, KEY
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, required=True)
    p.add_argument("--upstream", required=True)
    p.add_argument("--log", required=True)
    p.add_argument("--max-calls", type=int, default=40)
    p.add_argument("--model")
    p.add_argument("--auth", choices=["bearer", "x-api-key"], default="bearer")
    ARGS = p.parse_args()
    KEY = os.environ.pop("UPSTREAM_KEY", "")
    if not KEY:
        raise SystemExit("key_proxy: $UPSTREAM_KEY is empty")
    open(ARGS.log, "a").close()
    ThreadingHTTPServer(("127.0.0.1", ARGS.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
