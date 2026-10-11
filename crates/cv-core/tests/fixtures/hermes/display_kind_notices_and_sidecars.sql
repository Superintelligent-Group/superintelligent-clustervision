INSERT INTO messages (session_id, role, content, timestamp, display_kind) VALUES ('s', 'user', 'Model switched to x', 1001.0, 'model_switch');
INSERT INTO messages (session_id, role, content, timestamp, display_kind) VALUES ('s', 'user', 'keep going but faster', 1002.0, 'steer');
INSERT INTO messages (session_id, role, content, timestamp, api_content, effect_disposition, display_metadata) VALUES ('s', 'user', 'what the user typed', 1003.0, '[context] what the user typed', 'observed', '{"k":1}');
