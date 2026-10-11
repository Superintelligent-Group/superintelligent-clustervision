#!/usr/bin/env python3
"""A local OpenAI chat-completions pass-through that holds the upstream key, so the harness never does.

OpenCode speaks chat completions natively (`@ai-sdk/openai-compatible`), so nothing is translated:
each POST to `/v1/chat/completions` is forwarded to `--upstream` with the key from $UPSTREAM_KEY
(never argv, never logged) and the response bytes are streamed back as they arrive. Three edits to
the request body: `model` is forced to `--model`, GLM `thinking` is switched on, and image parts
become a text placeholder (the model is text-only; the harness still persists the attachment).
A request for model `corpus-error` is answered here with a 400 and never sent upstream or counted
(the provider-error scenario).

    UPSTREAM_KEY=... chat_proxy.py --port 18766 --upstream https://api.z.ai/api/coding/paas/v4 \
        --model glm-5.3-flash --log calls.jsonl --max-calls 60

Every call is one line in --log with the request shape only (message roles, tool names), never
content or headers. Calls past --max-calls get a 429.
"""
import argparse
import json
import os
import threading
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ARGS = None
LOCK = threading.Lock()
CALLS = 0


def strip_images(messages):
    for m in messages:
        c = m.get("content")
        if isinstance(c, list):
            m["content"] = [
                {"type": "text", "text": "[image attached; this model cannot see images]"}
                if p.get("type") in ("image_url", "input_image", "file") else p
                for p in c
            ]


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        body = json.dumps({"object": "list", "data": [{"id": ARGS.model, "object": "model"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        global CALLS
        n = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(n) or b"{}")
        if not self.path.rstrip("/").endswith("/chat/completions"):
            self.send_error(404)
            return
        if body.get("model") == "corpus-error":
            # The provider-error scenario: answered here, never upstream, never counted.
            err = json.dumps({"error": {"message": "corpus: this model always fails", "type": "invalid_request_error",
                                        "code": "corpus_error"}}).encode()
            self.send_response(400)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(err)))
            self.end_headers()
            self.wfile.write(err)
            return
        with LOCK:
            CALLS += 1
            call = CALLS
            with open(ARGS.log, "a") as f:
                f.write(json.dumps({"call": call, "roles": [m.get("role") for m in body.get("messages", [])],
                                    "tools": [(t.get("function") or {}).get("name") for t in body.get("tools") or []], "stream": bool(body.get("stream"))}) + "\n")
        if ARGS.max_calls and call > ARGS.max_calls:
            self.send_error(429, "corpus budget exhausted")
            return
        body["model"] = ARGS.model
        body.setdefault("thinking", {"type": "enabled"})
        strip_images(body.get("messages", []))
        req = urllib.request.Request(ARGS.upstream.rstrip("/") + "/chat/completions", data=json.dumps(body).encode(),
                                     headers={"Content-Type": "application/json",
                                              "Authorization": "Bearer " + os.environ["UPSTREAM_KEY"]})
        try:
            r = urllib.request.urlopen(req, timeout=300)
        except urllib.error.HTTPError as e:
            r = e
        with r:
            self.send_response(r.status if hasattr(r, "status") else r.code)
            self.send_header("Content-Type", r.headers.get("Content-Type", "application/json"))
            self.send_header("Connection", "close")
            self.end_headers()
            while True:
                chunk = r.read1(65536) if hasattr(r, "read1") else r.read(65536)
                if not chunk:
                    break
                self.wfile.write(chunk)
                self.wfile.flush()
        self.close_connection = True


def main():
    global ARGS
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, required=True)
    p.add_argument("--upstream", required=True)
    p.add_argument("--model", required=True)
    p.add_argument("--log", required=True)
    p.add_argument("--max-calls", type=int, default=60)
    ARGS = p.parse_args()
    ThreadingHTTPServer(("127.0.0.1", ARGS.port), H).serve_forever()


if __name__ == "__main__":
    main()
