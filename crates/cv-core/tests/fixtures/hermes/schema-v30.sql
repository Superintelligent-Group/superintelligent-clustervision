-- Schema v30 (2026-09, `hermes_state_common.py:239`): the v14 tables plus the in-place
-- compaction flags, display sidecars, `system_prompts`, cwd/git columns and listing flags.
--
-- A *subset* of the real schema 30 — column ORDER and types match
-- `tests/fixtures/hermes/state-v30.db` (`sqlite3 … '.schema sessions'`), but the gateway,
-- billing, handoff and compression-cooldown columns the adapter never reads are omitted.
-- Every column this adapter SELECTs or a test INSERTs must be here; add it in its real
-- position when one is missing.
CREATE TABLE system_prompts (hash TEXT PRIMARY KEY, prompt TEXT NOT NULL);
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    user_id TEXT,
    display_name TEXT,
    -- Provenance of an imported session (`hermes sessions import` writes
    -- `origin_json.imported_from`); real schema 30 puts it right here, after
    -- `display_name` — verified against tests/fixtures/hermes/state-v30.db.
    origin_json TEXT,
    model TEXT,
    model_config TEXT,
    system_prompt TEXT,
    system_prompt_hash TEXT,
    parent_session_id TEXT,
    started_at REAL NOT NULL,
    ended_at REAL,
    end_reason TEXT,
    message_count INTEGER DEFAULT 0,
    tool_call_count INTEGER DEFAULT 0,
    input_tokens INTEGER DEFAULT 0,
    output_tokens INTEGER DEFAULT 0,
    cache_read_tokens INTEGER DEFAULT 0,
    cache_write_tokens INTEGER DEFAULT 0,
    reasoning_tokens INTEGER DEFAULT 0,
    cwd TEXT,
    git_branch TEXT,
    git_repo_root TEXT,
    title TEXT,
    title_source TEXT,
    last_activity_at REAL,
    profile_name TEXT,
    archived INTEGER NOT NULL DEFAULT 0,
    hidden INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT,
    tool_call_id TEXT,
    tool_calls TEXT,
    tool_name TEXT,
    effect_disposition TEXT,
    timestamp REAL NOT NULL,
    token_count INTEGER,
    finish_reason TEXT,
    reasoning TEXT,
    reasoning_content TEXT,
    reasoning_details TEXT,
    codex_reasoning_items TEXT,
    codex_message_items TEXT,
    platform_message_id TEXT,
    observed INTEGER DEFAULT 0,
    _compressed_summary INTEGER NOT NULL DEFAULT 0,
    active INTEGER NOT NULL DEFAULT 1,
    compacted INTEGER NOT NULL DEFAULT 0,
    api_content TEXT,
    display_kind TEXT,
    display_metadata TEXT,
    display_identity BLOB,
    display_order INTEGER
);
