"""Deterministic SQLite scale inputs using adapter-supported native schemas.

Synthetic payloads and IDs only; never copies or rewrites corpus versions. Each row is
inserted separately, so generation itself need not retain a logical session in memory.
"""
import importlib.util
import json
import sqlite3
from pathlib import Path

FORMATS = ("devin", "hermes", "goose", "opencode", "cursor")
SID = "00000000-0000-4000-8000-000000000001"
REPO = Path(__file__).resolve().parent.parent


def generate_sqlite(harness, root, size, records, escaped, shape):
    allowed = {"devin": ("history", "metadata", "unselected"),
               "hermes": ("history", "prompt-only"), "goose": ("history",),
               "opencode": ("history", "parts"), "cursor": ("history", "inline", "legacy")}
    if shape not in allowed[harness]:
        raise ValueError(f"{harness} supports shapes {allowed[harness]}")
    name = {"devin": "sessions.db", "hermes": "state.db", "goose": "sessions.db",
            "opencode": "opencode.db", "cursor": "globalStorage/state.vscdb"}[harness]
    if harness == "cursor" and shape == "legacy": name = "workspaceStorage/perf/state.vscdb"
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    pattern = 'line\n"quoted"\t\\end' if escaped else "plain payload "
    def payload(i):
        n = size // records + (i < size % records)
        # Distinct prompts exercise retention rather than duplicate-message suppression.
        prefix = f"record {i}: "
        return (prefix + pattern * ((n + len(pattern) - 1) // len(pattern)))[:n]
    def j(value):
        return json.dumps(value, ensure_ascii=False, separators=(",", ":"))
    with sqlite3.connect(path) as db:
        if harness == "devin":
            spec = importlib.util.spec_from_file_location("devin_fixture_schema", REPO / "tools/harness-fixtures/devin/generate.py")
            schema = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(schema)
            db.executescript(schema.SCHEMA)
            db.execute("INSERT INTO sessions(id,working_directory,backend_type,model,agent_mode,created_at,last_activity_at,title,main_chain_id) VALUES(?, '/perf','synthetic','fixture-model','agent',1767225600,1767225601,'scale fixture',?)", (SID, 1 if shape == "unselected" else records))
            for i in range(records):
                body = payload(i)
                chat = {"message_id": f"m{i}", "role": "assistant", "content": body if shape == "history" else "small reply"}
                metadata = {"summarized_from": None, "num_tokens_preceding": i, "is_system_prefix": False}
                if shape != "history":
                    metadata["extensions"] = {"synthetic/scale-payload": body}
                db.execute("INSERT INTO message_nodes(session_id,node_id,parent_node_id,chat_message,created_at,metadata) VALUES(?,?,?,?,?,?)",
                           (SID, i + 1, None if i == 0 or shape == "unselected" else i, j(chat), 1767225600 + i, j(metadata)))
        elif harness == "hermes":
            db.executescript((REPO / "crates/cv-core/tests/fixtures/hermes/schema-v30.sql").read_text())
            db.execute("CREATE INDEX perf_messages_session_id ON messages(session_id,id)")
            db.execute("INSERT INTO sessions(id,source,started_at,title) VALUES(?,'cli',1767225600,'scale fixture')", (SID,))
            for i in range(records):
                db.execute("INSERT INTO messages(session_id,role,content,timestamp,active,compacted) VALUES(?,?,?,?,1,0)",
                           (SID, "user" if shape == "prompt-only" else "assistant", payload(i), 1767225600 + i))
        elif harness == "goose":
            db.executescript("CREATE TABLE sessions(id TEXT PRIMARY KEY,name TEXT,working_dir TEXT,created_at TEXT,updated_at TEXT); CREATE TABLE messages(id INTEGER PRIMARY KEY,session_id TEXT,role TEXT,content_json TEXT,created_timestamp INTEGER); CREATE INDEX messages_session ON messages(session_id,created_timestamp,id);")
            db.execute("INSERT INTO sessions VALUES(?,'scale fixture','/perf','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')", (SID,))
            for i in range(records):
                db.execute("INSERT INTO messages VALUES(?,?, 'assistant',?,?)", (i, SID, j([{"type": "text", "text": payload(i)}]), 1767225600 + i))
        elif harness == "opencode":
            db.executescript("CREATE TABLE session(id TEXT PRIMARY KEY,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,data TEXT); CREATE TABLE part(id TEXT PRIMARY KEY,message_id TEXT,data TEXT); CREATE INDEX message_session ON message(session_id,time_created,id); CREATE INDEX part_message ON part(message_id,id);")
            db.execute("INSERT INTO session VALUES(?,'/perf','scale fixture',1767225600000,1767225600000)", (SID,))
            for i in range(records):
                mid = "m0" if shape == "parts" else f"m{i:09d}"
                if i == 0 or shape != "parts":
                    db.execute("INSERT INTO message VALUES(?,?,?,?)", (mid, SID, 1767225600000 + i, j({"role": "assistant", "time": {"created": 1767225600000 + i}})))
                db.execute("INSERT INTO part VALUES(?,?,?)", (f"p{i:09d}", mid, j({"type": "text", "text": payload(i)})))
        else:
            table = "ItemTable" if shape == "legacy" else "cursorDiskKV"
            db.execute(f"CREATE TABLE {table}(key TEXT PRIMARY KEY,value TEXT)")
            # Stream the inline JSON to a temporary generator file to avoid a whole Python tree.
            composer = {"name": "scale fixture", "createdAt": 1767225600000, "modelConfig": {"modelName": "fixture-model"}}
            if shape in ("inline", "legacy"):
                parts = root / "composer-generation.json"
                with parts.open("w") as out:
                    out.write('{"tabs":[{"tabId":"scale-tab","chatTitle":"scale fixture","bubbles":[' if shape == "legacy" else j(composer)[:-1] + ',"conversation":[')
                    for i in range(records):
                        if i: out.write(',')
                        out.write(j({"bubbleId": f"b{i}", "type": "ai" if shape == "legacy" else 2, "text": payload(i)}))
                    out.write(']}]}' if shape == 'legacy' else ']}')
                # Native stores accept BLOB values too. Allocate the cell without a giant
                # Python/String/SQLite binding copy, then fill it through SQLite's blob API.
                key = "workbench.panel.aichat.view.aichat.chatdata" if shape == "legacy" else f"composerData:{SID}"
                db.execute(f"INSERT INTO {table} VALUES(?,zeroblob(?))", (key, parts.stat().st_size))
                rowid = db.execute(f"SELECT rowid FROM {table} WHERE key=?", (key,)).fetchone()[0]
                with db.blobopen(table, "value", rowid) as blob, parts.open("rb") as source:
                    while chunk := source.read(65536):
                        blob.write(chunk)
                parts.unlink()
            else:
                composer["fullConversationHeadersOnly"] = [{"bubbleId": f"b{i}"} for i in range(records)]
                db.execute("INSERT INTO cursorDiskKV VALUES(?,?)", (f"composerData:{SID}", j(composer)))
                for i in range(records):
                    db.execute("INSERT INTO cursorDiskKV VALUES(?,?)", (f"bubbleId:{SID}:b{i}", j({"bubbleId": f"b{i}", "type": 2, "text": payload(i)})))
    count = 1 if shape in ("unselected", "parts") else records
    return {"harness": harness, "id": "legacy:scale-tab" if harness == "cursor" and shape == "legacy" else SID, "root": str(root), "path": str(path), "shape": shape,
            "requested_payload_bytes": size, "records": records, "expected_messages": count,
            "escaped": escaped, "physical_input_bytes": path.stat().st_size,
            "synthetic": True, "provenance": "adapter-supported native schema; made-up IDs and payloads"}
