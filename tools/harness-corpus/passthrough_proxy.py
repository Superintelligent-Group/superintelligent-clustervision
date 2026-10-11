#!/usr/bin/env python3
"""A local pass-through to a model API that holds the key and counts the calls.

For a harness whose provider already speaks the right wire (Kimi Code and kimi-cli against Kimi's
OpenAI-compatible coding endpoint), this is the whole model path: the harness is pointed at
http://127.0.0.1:PORT/<prefix> with a dummy key, and each request is forwarded byte for byte to
--upstream with the real key put in its place. The harness never holds the key, so no tool call,
log or environment dump inside the session can leak it.

    UPSTREAM_KEY=... passthrough_proxy.py --port-file port --upstream https://api.kimi.com/coding/v1 \
        --log calls.jsonl --max-calls 40

Every POST is one model call: it is logged (path, status, model, stream flag, message count, tool
NAMES; never content or headers) and refused with 400 past --max-calls. Other methods (a model list) pass through uncounted.
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
KEY = None
LOCK = threading.Lock()
CALLS = 0
HOP = {"host", "authorization", "x-api-key", "content-length", "connection", "accept-encoding",
       "transfer-encoding", "keep-alive", "proxy-connection"}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _forward(self, method):
        global CALLS
        n = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(n) if n else None
        counted = method == "POST"
        if counted:
            with LOCK:
                CALLS += 1
                call = CALLS
            if call > ARGS.max_calls:
                msg = json.dumps({"error": {"message": f"corpus budget of {ARGS.max_calls} model calls spent",
                                            "type": "invalid_request_error"}}).encode()
                self.send_response(400)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(msg)))
                self.end_headers()
                self.wfile.write(msg)
                self._log(call, 400, body)
                return
        path = self.path
        if ARGS.strip_prefix and path.startswith(ARGS.strip_prefix):
            path = path[len(ARGS.strip_prefix):]
        req = urllib.request.Request(ARGS.upstream.rstrip("/") + path, data=body, method=method)
        for k, v in self.headers.items():
            if k.lower() not in HOP:
                req.add_header(k, v)
        req.add_header("Authorization", f"Bearer {KEY}")
        try:
            resp = urllib.request.urlopen(req, timeout=600)
        except urllib.error.HTTPError as e:
            resp = e
        status = resp.status if hasattr(resp, "status") else resp.code
        self.send_response(status)
        for k, v in resp.headers.items():
            if k.lower() not in HOP and k.lower() != "content-encoding":
                self.send_header(k, v)
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()
        while True:
            chunk = resp.read1(65536) if hasattr(resp, "read1") else resp.read(65536)
            if not chunk:
                break
            self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
            self.wfile.flush()
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()
        if counted:
            self._log(call, status, body)

    def _log(self, call, status, body):
        shape = {}
        try:
            j = json.loads(body or b"{}")
            shape = {"model": j.get("model"), "stream": j.get("stream"), "messages": len(j.get("messages") or []),
                     "tools": sorted((t.get("function") or t).get("name", "?") for t in j.get("tools") or [])}
        except ValueError:
            pass
        with LOCK, open(ARGS.log, "a") as f:
            f.write(json.dumps({"n": call, "path": self.path, "status": status, **shape}) + "\n")

    def do_POST(self):
        self._forward("POST")

    def do_GET(self):
        self._forward("GET")


def main():
    global ARGS, KEY
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, default=0, help="0 picks a free port")
    p.add_argument("--port-file", help="write the bound port here once listening")
    p.add_argument("--upstream", required=True)
    p.add_argument("--strip-prefix", default="", help="drop this path prefix before forwarding")
    p.add_argument("--log", required=True)
    p.add_argument("--max-calls", type=int, default=40)
    ARGS = p.parse_args()
    KEY = os.environ.pop("UPSTREAM_KEY", "")
    if not KEY:
        sys.exit("passthrough_proxy: $UPSTREAM_KEY is empty")
    open(ARGS.log, "a").close()
    srv = ThreadingHTTPServer(("127.0.0.1", ARGS.port), Handler)
    if ARGS.port_file:
        with open(ARGS.port_file + ".tmp", "w") as f:
            f.write(str(srv.server_address[1]))
        os.replace(ARGS.port_file + ".tmp", ARGS.port_file)
    srv.serve_forever()


if __name__ == "__main__":
    main()
