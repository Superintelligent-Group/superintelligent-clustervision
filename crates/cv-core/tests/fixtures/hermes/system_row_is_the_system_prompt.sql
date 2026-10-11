INSERT INTO sessions (id, source, started_at, system_prompt) VALUES ('s', 'cli', 1000.0, 'You are Hermes');
INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s','system','You are Hermes',1000.5);
INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s','user','hi',1001.0);
