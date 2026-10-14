# Cursor native CLI and IDE corpora

`2026.10.01-e373342` now has a genuine native scratch capture: one user task,
Read/StrReplace/Shell calls and their results, generated on 2026-10-12.
The capture used isolated Python subprocess orchestration; `generate.py` codifies
that procedure afterwards and was not rerun for this committed version. Its help,
syntax and offline codec tests were checked. The IDE shape inspection and synthetic
mode regressions below remain distinct.

## Read-only installed-format assessment, 2026-10-12

The installed IDE is **3.18.25**, product commit
`280eca2911f1774689696e5f1efa5a4f97a87af0`, product build date 2026-08-31.
The active `cursor-agent` launcher still targets `2025.09.12-4852336`; the separately
installed `2026.10.01-e373342` CLI is a different executable and store format.
No CLI `chats/` or `acp-sessions/` store was present during the initial assessment. IDE login alone does not prove an isolated CLI
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

## Generation and isolation

`install.sh <build>` installs an exact build without changing the active launcher.
`generate.py --bin /path/to/cursor-agent --version 2026.10.01-e373342
--out NEW_VERSION_DIR --ide-auth-db /explicit/authorized/state.vscdb` performs one
bounded task in a disposable project, HOME, XDG config/data/cache and TMPDIR.
Use an account token only when its use for this capture is authorized. The supplied
IDE DB is immutable/read-only; only `cursorAuth/accessToken` is read into memory.
The generator does not write an auth file, print credentials or keep CLI output.
It refuses an existing version directory. It runs the common secret scan after
protocol-aware scrubbing and SQLite backup; regenerate only the new summary.

The installed bundle's `createCredentialManager` explicitly supports
`AGENT_CLI_CREDENTIAL_STORE=memory`; with this mode `CURSOR_AUTH_TOKEN` calls an
in-memory `setAuthentication`. **Token alone is insufficient isolation:** the
default macOS credential manager can write the login keychain even with a scratch
HOME. Do not use API keys, token exchange, refresh or default credentials here.
The memory behavior is verified for this exact build, not inferred for future builds.

## Native storage and reader coverage

The CLI writes `chats/<md5(cwd)>/<agentId>/store.db` under `CURSOR_CONFIG_DIR`
(default `~/.cursor`, or XDG config's `cursor` directory). ACP uses
`acp-sessions/<agentId>/store.db`, with a sibling `meta.json` cwd/title fallback.
`meta["0"]` contains hex-encoded JSON; `latestRootBlobId` selects a protobuf root.
`blobs` contains protobuf structures, JSON model messages and plain-text file blobs.
Root field 1 references live JSON messages, field 13 references ordered summary
archives (legacy field 11 supported), and archive field 1 references older messages.
Only reachable messages are replayed; obsolete root snapshots do not create twins.

Schema evidence comes from the installed `index.js` classes
`ConversationStateStructure`, `ConversationSummaryArchive` and `AgentMode`, not
private transcript content. CLI AgentMode differs from the IDE UnifiedMode enum:
0 unspecified, 1 agent, 2 ask, 3 plan, 4 debug, 5 triage, 6 project, 7 multitask,
8 custom. Unknown numeric modes retain their raw values.
Discovery uses the same message reducer as streaming with a strict count/title
projection. SQLite snapshot and incrementally staged blobs bound retained history;
full streaming still has a per-message JSON allocation floor. Discovery borrows
plain strings from contiguous bytes; escaped strings and portable reader scalars
still have serde scratch allocation floors. This is not a universal record-size bound. Wire lengths are
validated and unknown protobuf fields skipped without allocating their payloads.
Archive order, subagent lineage and ACP sidecar freshness have synthetic tests; the first native
capture covers ordinary chat and tool use only. No claim of genuine capture is
made for those ungenerated scenarios. `codec.py` rebuilds protobuf length prefixes
when paths change and rebuilds content hashes/references; the common byte scrubber alone would corrupt these stores.

The CLI has no native logical update timestamp in the captured root: `updated_at`
uses the freshest main DB/WAL modification time, an approximation. Optional ACP
metadata is consulted once and recorded before opening, including absence.

## Native IDE capture, 2026-10-13

`3.24.12-ide` is a genuine freshly authored IDE composer, distinct from the CLI
corpus. Cursor was not running before capture. It was launched with disposable
`--user-data-dir`, `--extensions-dir`, `--disable-extensions`, `--sync off`, and
`--remote-debugging-port` plus a synthetic Python scratch project. CDP attached
only to that newly launched instance. The native application recognized its
existing account without credential copying or reading token values.

Four user turns exercised read/search/edit/shell, thinking and ordinary text,
a Unicode follow-up, a model-authored syntax failure followed by its repair,
a stopped response, graceful application quit/reopen, and continued conversation.
There are 44 native bubbles and 17 tool calls (8 reads, 4 edits, 4 commands, 1 glob).
The native failed command has `toolFormerData.status=completed`,
`additionalData.status=error`, and `result.exitCode=1`; cv must mark that result
as an error even though the tool invocation completed.

Capture used SQLite backup after the application was closed. The committed
copy retains only the selected composer's native rows, all native content blobs,
its composerHeaders row, and the workspace composer mapping. Global ItemTable
rows (including authentication and settings), unrelated drafts, caches and logs
are excluded. Paths were scrubbed with the shared scanner. Base64 toolCallBinary
protobufs were decoded and scrubbed using `codec.wire`, rebuilding lengths before
re-encoding. Encryption keys were removed and opaque encrypted conversationState
snapshots replaced with the native empty marker `~`; those encrypted alternate
snapshots cannot be content-scrubbed and cv reads the ordinary composer/bubble
records. No message, tool call or result was manufactured.

The manifest distinguishes observed interruption/reopen from a persisted lifecycle
event: this store does not record a dedicated stop event. Native model repair is
covered; the regenerate-button action, image attachment, mode switch, compaction
and subagents remain uncaptured. The store's zero tokenCount fields are native
placeholders, not measured usage. Read_file_v2 results contain line counts rather
than the file contents displayed to the model.
