INSERT INTO messages (session_id, role, content, timestamp) VALUES ('root','user','first prompt',1001.0);
INSERT INTO messages (session_id, role, content, timestamp) VALUES ('root','assistant','first answer',1002.0);
INSERT INTO messages (session_id, role, content, timestamp) VALUES ('root','user','second prompt',1003.0);
-- child replays 'second prompt' then continues.
INSERT INTO messages (session_id, role, content, timestamp) VALUES ('child','user','second prompt',2002.0);
INSERT INTO messages (session_id, role, content, timestamp) VALUES ('child','assistant','second answer',2003.0);
