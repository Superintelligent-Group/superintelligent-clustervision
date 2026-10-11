INSERT INTO system_prompts (hash, prompt) VALUES ('h1', 'You are Hermes');
INSERT INTO sessions (id, source, started_at, system_prompt, system_prompt_hash) VALUES ('s', 'cli', 1000.0, NULL, 'h1');
