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
keychain entry; `generate.sh` would load it with `corpus_load_key`, run `cursor-agent -p
--output-format json` in a throwaway `HOME` (and `CURSOR_CONFIG_DIR`/`CURSOR_DATA_DIR`), and copy
the store below. 2026.10 also accepts `CURSOR_AUTH_TOKEN`.

**What a corpus would hold, and what cv cannot read yet:** cursor-agent writes
`~/.cursor/chats/<md5(cwd)>/<chatId>/store.db`, a sqlite blob store (`blobs(id TEXT PRIMARY KEY,
data BLOB)`, `meta(key TEXT PRIMARY KEY, value TEXT)`). cv's `cursor` adapter reads the Cursor
IDE's `state.vscdb` (`formats/cursor.toml`), not this store, so cursor-agent sessions are invisible
to cv today, corpus or not. ember's machine has no `~/.cursor/chats/`, so there is no local data to
reverse-engineer from either.
