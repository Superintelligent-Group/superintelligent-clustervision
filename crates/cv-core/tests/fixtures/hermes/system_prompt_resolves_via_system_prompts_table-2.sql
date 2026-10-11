INSERT INTO sessions (id, source, started_at, system_prompt) VALUES ('s', 'cli', 1000.0, 'legacy prompt');
INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s','user','hi',1001.0);
