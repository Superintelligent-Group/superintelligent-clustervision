-- An older (pre-v14-ish) schema: only the columns Hermes had before the reasoning/codex/token
-- additions. Exercises the column-probing degradation path.
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    model TEXT,
    started_at REAL NOT NULL,
    ended_at REAL,
    message_count INTEGER DEFAULT 0,
    title TEXT
);
CREATE TABLE messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT,
    tool_call_id TEXT,
    tool_calls TEXT,
    tool_name TEXT,
    timestamp REAL NOT NULL
);
