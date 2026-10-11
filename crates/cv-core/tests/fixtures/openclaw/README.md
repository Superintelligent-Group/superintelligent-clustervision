# OpenClaw fixtures

`openclaw-agent.sqlite` was written by OpenClaw's own store code, not by cv. Its generator moved to

    tools/harness-fixtures/openclaw/generate-openclaw-agent-sqlite.mts

with prerequisites, the one command and what it produces in the README next to it. Every
real-writer fixture generator lives under `tools/harness-fixtures/<harness>/` now.

The `.jsonl` files here are legacy v3 transcripts kept as-is (hand-checked samples, not generated).

The rest are hand-written test inputs the inline tests in `src/harness/openclaw.rs` read:
`branchy-v4.jsonl`, `legacy-1.jsonl`, `reset-and-side-entries.jsonl`, `assistant-api-failure.jsonl`,
`non-utf8-line.jsonl` (line 3 is deliberately invalid UTF-8), and `transcript-tables.sql` (the
three transcript tables of OpenClaw's `src/state/openclaw-agent-schema.sql`, foreign keys to
`conversations` dropped).
