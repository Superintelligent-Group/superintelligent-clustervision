UPDATE sessions SET title = NULL WHERE id = 'tip';
INSERT INTO sessions (id, source, started_at, archived) VALUES ('arch', 'cli', 1700.0, 1);
INSERT INTO sessions (id, source, started_at, hidden) VALUES ('hid', 'cli', 1800.0, 1);
INSERT INTO sessions (id, source, started_at, parent_session_id, model_config) VALUES ('del', 'cli', 1900.0, 'tip', '{"_delegate_from":"tip"}');
INSERT INTO sessions (id, source, started_at, cwd, last_activity_at, ended_at) VALUES ('plain', 'cli', 2000.0, '/work/proj', 5000.0, 4000.0);
