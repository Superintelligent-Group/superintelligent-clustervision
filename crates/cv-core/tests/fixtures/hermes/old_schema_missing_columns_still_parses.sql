INSERT INTO sessions (id, source, model, started_at, title) VALUES ('s1','cli','m',1000.0,'T');
INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s1','user','hello old',1001.0);
INSERT INTO messages (session_id, role, content, tool_calls, timestamp) VALUES ('s1','assistant','','[{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]',1002.0);
