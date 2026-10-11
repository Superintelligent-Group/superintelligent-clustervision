INSERT INTO messages (session_id, role, content, tool_calls, timestamp) VALUES ('s1','assistant','','[{"id":"c1","type":"function","function":{"name":"web_search","arguments":"{\"q\":\"x\"}"}}]',1001.0);
INSERT INTO messages (session_id, role, content, tool_call_id, tool_name, timestamp) VALUES ('s1','tool','{"ok":true}','c1','web_search',1002.0);
