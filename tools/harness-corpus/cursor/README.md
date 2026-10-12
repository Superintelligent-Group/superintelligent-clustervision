# Cursor agent CLI: no corpus yet (blocked on a credential)

`install.sh <build>` works: it fetches the tarball the official installer names,
`https://downloads.cursor.com/lab/<build>/<os>/<arch>/agent-cli-package.tar.gz`, into
`~/tools/harness-versions/cursor/<build>/`. Installed 2026-10-11: `2025.09.12-4852336` (ember's
build, identical to `~/.local/share/cursor-agent/versions/`) and `2026.10.01-e373342` (the build
https://cursor.com/install named that day).

There is no `generate.sh`, because nothing can run a model turn without ember's account:

- **The login is in the macOS Keychain, not a file.** On darwin both builds keep the OAuth tokens
  as generic passwords (`cursor-access-token`, `cursor-refresh-token`, `cursor-api-key`, account
  `cursor-user`) via `/usr/bin/security`; 2026.10 adds a keychain probe and only falls back to
  `~/.cursor/auth.json` when the keychain is unusable. ember has no `~/.cursor/auth.json` (or
  `~/.config/cursor/auth.json`), so the lane rule "copy only the auth file into the throwaway home"
  has nothing to copy. `security` resolves the user's login keychain no matter what `$HOME` says,
  so a throwaway-home run would read, and on refresh rewrite, ember's real credential.
- **No BYOK.** Every turn goes to Cursor's backend (`api2.cursor.sh`, `--endpoint` /
  `CURSOR_API_ENDPOINT` only moves that); there is no custom-model or base-URL setting to point at
  Z.ai, so no key we hold can drive it.

**What would unblock it:** a Cursor API key (Cursor dashboard → Integrations → API keys) in a key
file such as `~/.cursor-key`. Both builds take `CURSOR_API_KEY` / `--api-key` and then read no
keychain entry. With no base URL to point at `key_proxy.py`, Cursor would be the one harness that
holds its own key: `generate.sh` would export it as `CURSOR_API_KEY`, add the file to
`CORPUS_KEY_FILES` so the scrub checks for it, run `cursor-agent -p
--output-format json` in a throwaway `HOME` (and `CURSOR_CONFIG_DIR`/`CURSOR_DATA_DIR`), and copy
the store below. 2026.10 also accepts `CURSOR_AUTH_TOKEN`.

**What a corpus would hold, and what cv cannot read yet:** cursor-agent writes
`~/.cursor/chats/<md5(cwd)>/<chatId>/store.db`, a sqlite blob store (`blobs(id TEXT PRIMARY KEY,
data BLOB)`, `meta(key TEXT PRIMARY KEY, value TEXT)`). cv's `cursor` adapter reads the Cursor
IDE's `state.vscdb` (`formats/cursor.toml`), not this store, so cursor-agent sessions are invisible
to cv today, corpus or not. ember's machine has no `~/.cursor/chats/`, so there is no local data to
reverse-engineer from either.


## Read-only installed-format assessment, 2026-10-12

The installed IDE is **3.18.25**, product commit
`280eca2911f1774689696e5f1efa5a4f97a87af0`, product build date 2026-08-31.
The active `cursor-agent` launcher still targets `2025.09.12-4852336`; the separately
installed `2026.10.01-e373342` CLI is a different executable and store format.
No Cursor corpus has been generated, and no CLI `chats/` or `acp-sessions/` store
was present during this assessment. IDE login alone does not prove an isolated CLI
run will avoid refreshing the real login keychain.

An immutable, read-only inspection of the IDE SQLite store confirmed both inline
`conversation` and external `fullConversationHeadersOnly` / `bubbleId` records,
plus a `composerHeaders` table. Existing records may have been written by older
IDE releases; this is schema evidence, **not a version-attributed session capture**.
Bubble `unifiedMode` values are integers, while composer values are strings.
The installed application bundle, `Contents/Resources/app/out/main.js`, defines
`aiserver.v1.StreamUnifiedChatRequest.UnifiedMode` explicitly as:
`0=UNSPECIFIED, 1=CHAT, 2=AGENT, 3=EDIT, 4=CUSTOM, 5=PLAN, 6=DEBUG`.
This enum was read from the installed bundle, without invoking Cursor or reading
credential values. cv maps known numbers to lowercase mode names and retains the
original number as `extra.cursor.unified_mode_raw`, including unknown numbers.
The adapter regressions exercising these modes are synthetic; they are not a
real harness-written corpus.

A useful future capture would use the newer CLI in an isolated home/config/data
root with an explicitly supplied, non-refreshing auth token, or an IDE scratch
project/session, then copy only the harness-written store and scrub it. The CLI's
`blobs`/`meta` store still needs an adapter path before it can join corpus golden
tests. A fresh model session is unnecessary to demonstrate the numeric-mode gap.
