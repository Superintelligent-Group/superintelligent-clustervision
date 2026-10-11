# Zed: no generated corpus, a schema snapshot instead

```sh
tools/harness-corpus/zed/snapshot.sh            # record the installed version's store shape
tools/harness-corpus/zed/snapshot.sh --check    # exit 1 if the store's shape has changed since
```

## What blocks a generated corpus

Zed's agent threads (`threads/threads.db`) are written only by the agent panel of the desktop app.
There is no headless or CLI path to it:

- The `zed` CLI (1.16.1) only opens paths in the app. It has `--user-data-dir`, `--foreground`,
  `--diff` and similar flags, but nothing that sends a prompt to the agent.
- The agent panel takes input only through the GUI. Driving it means macOS UI scripting of a
  visible app window on ember's desktop. It would also need a throwaway data dir and an
  OpenAI-compatible provider in the settings.

**What would unblock it:** either a Zed test harness built from source, where the
`agent`/`agent2` crates' thread-store tests run `ThreadStore::save_thread` against a temp dir, or
an accepted GUI-automation run (Zed with `--user-data-dir <tmp>` and a `language_models.openai_compatible`
provider pointed at `key_proxy.py`, driven by `osascript`). Either way the data comes from Zed's own code.

## What is recorded instead

`crates/cv-core/tests/corpus/zed/<app version>/` holds:

- `threads.schema.txt`: the DDL of `threads.db`, plus the merged JSON shape (key paths, value types,
  and the values of tag-like fields) of every zstd thread blob on this machine.
- `workspace-agent-tables.schema.txt`: the DDL of the agent tables in `db/0-stable/db.sqlite`
  (`sidebar_threads`, `sidebar_terminal_threads`, `thread_archived_worktrees`, `archived_git_worktrees`).
- `SNAPSHOT.toml`: the app version and when the store was last written.

`schema_snapshot.py` opens the databases read-only and immutable. It writes no content: an object
whose keys are not plain identifiers is collapsed to `<key>`. On 2026-10-11 the installed app was
1.16.1, but the newest thread was written on 2026-06-10, so the blob shape reflects older Zed builds
(versions 0.2.0 and 0.3.0). The DDL is what 1.16.1 migrated to.
