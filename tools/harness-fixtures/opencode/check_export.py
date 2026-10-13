#!/usr/bin/env python3
"""Validate an exported store through native OpenCode 2.x, including a local stub continuation.
Usage: check_export.py OPENCODE_BINARY EXPORTED_DB SCRATCH_DIR [SESSION_ID]
The input database is read-only; all native writes use a copy under a new scratch HOME.
"""
import base64
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import subprocess
import sys
import threading
import time
from http.server import HTTPServer
from urllib.request import Request, urlopen


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def main():
    binary, source, scratch = map(Path, sys.argv[1:4])
    session = sys.argv[4] if len(sys.argv) > 4 else "ses_cv_native_v2"
    scratch.mkdir(parents=True, exist_ok=False)
    home = scratch / "home"
    data = home / ".local/share"
    config = home / ".config"
    for path in (data / "opencode", config / "opencode", scratch / "tmp"):
        path.mkdir(parents=True)
    db = data / "opencode/opencode.db"
    # SQLite backup also snapshots WAL-bearing inputs without copying uncheckpointed bytes.
    with sqlite3.connect(f"file:{source}?mode=ro", uri=True) as src, sqlite3.connect(db) as dst:
        src.backup(dst)
    with sqlite3.connect(db) as conn:
        table = "session_v2" if conn.execute("SELECT 1 FROM sqlite_master WHERE name='session_v2'").fetchone() else "session"
        directory = Path(conn.execute(f"SELECT directory FROM {table} WHERE id=?", (session,)).fetchone()[0])
        original_rows = conn.execute("SELECT id,type,seq,data FROM session_message WHERE session_id=? ORDER BY seq", (session,)).fetchall() if table == "session_v2" else []
    # Only create a project under our own artifact/scratch directory.
    if not directory.is_relative_to(source.parent) and not directory.is_relative_to(scratch):
        raise RuntimeError(f"export directory outside fixture scratch: {directory}")
    directory.mkdir(parents=True, exist_ok=True)
    stub_path = Path(__file__).resolve().parents[1] / "goose/stub_openai.py"
    spec = importlib.util.spec_from_file_location("cv_openai_fixture", stub_path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    evidence = {"schema_before": table, "session": session, "requests": []}

    class Stub(module.H):
        def do_POST(self):
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            request = json.loads(raw)
            encoded = json.dumps(request)
            evidence["requests"].append({"path": self.path, "model": request.get("model"), "prior_prompt": "fixture prompt" in encoded, "prior_answer": "fixture answer" in encoded, "prior_tool_result": "fixture output" in encoded})
            import io
            self.rfile = io.BytesIO(raw)
            super().do_POST()

    proxy = HTTPServer(("127.0.0.1", 0), Stub)
    threading.Thread(target=proxy.serve_forever, daemon=True).start()
    port = free_port()
    settings = {"autoupdate": False, "share": "disabled", "model": "clustervision/fixture-model", "small_model": "clustervision/fixture-model", "provider": {"clustervision": {"npm": "@ai-sdk/openai-compatible", "name": "local fixture", "options": {"baseURL": f"http://127.0.0.1:{proxy.server_port}/v1", "apiKey": "local-fixture"}, "models": {"fixture-model": {"name": "fixture", "limit": {"context": 128000, "output": 1024}}}}}}
    (config / "opencode/opencode.json").write_text(json.dumps(settings))
    env = dict(os.environ)
    for key in list(env):
        if key.startswith("OPENCODE_") or key in ("OPENAI_API_KEY", "ANTHROPIC_API_KEY"):
            env.pop(key)
    env.update(HOME=str(home), TMPDIR=str(scratch / "tmp"), XDG_DATA_HOME=str(data), XDG_CONFIG_HOME=str(config), XDG_STATE_HOME=str(home / ".local/state"), XDG_CACHE_HOME=str(home / ".cache"), OPENCODE_PASSWORD="cv-local-fixture", OPENCODE_DISABLE_AUTOUPDATE="1", OPENCODE_DISABLE_SHARE="1")
    evidence["native_version"] = subprocess.check_output([str(binary), "--version"], text=True, env=env).strip()
    auth = "Basic " + base64.b64encode(b"opencode:cv-local-fixture").decode()

    def api(path, payload=None):
        body = None if payload is None else json.dumps(payload).encode()
        request = Request(f"http://127.0.0.1:{port}/api{path}", data=body, headers={"Authorization": auth, "Content-Type": "application/json"})
        with urlopen(request, timeout=20) as response:
            return json.load(response)

    with (scratch / "native.log").open("wb") as log:
        process = subprocess.Popen([str(binary), "serve", "--port", str(port)], cwd=directory, env=env, stdout=log, stderr=log)
        try:
            for _ in range(150):
                try:
                    listed = api("/session")
                    break
                except Exception:
                    if process.poll() is not None:
                        raise RuntimeError("native server exited; see native.log")
                    time.sleep(.2)
            else:
                raise RuntimeError("native server never became ready")
            info = api(f"/session/{session}")
            messages = api(f"/session/{session}/message")
            context = api(f"/session/{session}/context")
            evidence.update(get=info, list_has_export=session in json.dumps(listed), messages=messages, context=context)
            assert evidence["list_has_export"]
            assert "fixture prompt" in json.dumps(context)
            assert "fixture output" in json.dumps(context)
            api(f"/session/{session}/prompt", {"text": "Continue this fixture; answer pong."})
            for _ in range(150):
                after = api(f"/session/{session}/message")
                if module.REPLY in json.dumps(after, ensure_ascii=False):
                    evidence["continuation"] = True
                    evidence["messages_after"] = after
                    break
                time.sleep(.2)
            else:
                raise RuntimeError("native continuation did not finish")
            with sqlite3.connect(db) as conn:
                after_rows=conn.execute("SELECT id,type,seq,data FROM session_message WHERE session_id=? ORDER BY seq", (session,)).fetchall()
            original_ids={row[0] for row in original_rows}
            preserved=[row for row in after_rows if row[0] in original_ids]
            assert preserved == original_rows, "native continuation changed or reordered imported history"
            continued=[row for row in after_rows if row[0] not in original_ids]
            assert continued and continued[0][2] > original_rows[-1][2], "native continuation reused imported sequence range"
            assert len({row[2] for row in after_rows}) == len(after_rows)
            evidence["sequence"]={"imported_rows":len(original_rows),"imported_max":original_rows[-1][2],"first_continued":continued[0][2],"original_history_preserved":True}
            assert any(r["prior_prompt"] and r["prior_answer"] and r["prior_tool_result"] for r in evidence["requests"]), evidence["requests"]
            (scratch / "evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
            print(json.dumps({"native_version": evidence["native_version"], "schema_before": table, "get": True, "list": True, "messages": True, "context": True, "continuation": True, "prior_history_in_model_request": True}))
        finally:
            process.terminate()
            process.wait(timeout=15)
            proxy.shutdown()


if __name__ == "__main__":
    main()
