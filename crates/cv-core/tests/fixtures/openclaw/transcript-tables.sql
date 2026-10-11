CREATE TABLE session_nodes (
  session_key TEXT NOT NULL PRIMARY KEY, current_session_id TEXT NOT NULL,
  entry_json TEXT NOT NULL, legacy_acp_migration_json TEXT,
  entry_valid INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL, status TEXT,
  created_at INTEGER, parent_session_key TEXT, spawned_by TEXT, fork_source_session_key TEXT,
  fork_source_session_id TEXT, fork_source_entry_id TEXT, label TEXT, display_name TEXT,
  category TEXT, icon TEXT, pinned_at INTEGER, archived_at INTEGER, last_read_at INTEGER,
  last_interaction_at INTEGER, last_activity_at INTEGER) STRICT;
CREATE TABLE session_windows (
  session_id TEXT NOT NULL PRIMARY KEY, session_key TEXT NOT NULL, previous_session_id TEXT,
  reason TEXT, session_scope TEXT NOT NULL DEFAULT 'conversation', created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL, transcript_updated_at INTEGER DEFAULT NULL,
  transcript_observed_at INTEGER DEFAULT NULL, started_at INTEGER, ended_at INTEGER, status TEXT,
  chat_type TEXT, channel TEXT, account_id TEXT, primary_conversation_id TEXT,
  model_provider TEXT, model TEXT, agent_harness_id TEXT, parent_session_key TEXT,
  spawned_by TEXT, display_name TEXT,
  FOREIGN KEY (session_key) REFERENCES session_nodes(session_key) ON DELETE CASCADE) STRICT;
CREATE TABLE transcript_events (
  session_id TEXT NOT NULL, seq INTEGER NOT NULL, event_json TEXT NOT NULL,
  created_at INTEGER NOT NULL, PRIMARY KEY (session_id, seq),
  FOREIGN KEY (session_id) REFERENCES session_windows(session_id) ON DELETE CASCADE) STRICT;
