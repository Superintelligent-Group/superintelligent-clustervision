# LM Studio: no generated corpus, a schema snapshot instead

```sh
tools/harness-corpus/lmstudio/snapshot.sh            # record the installed version's store shape
tools/harness-corpus/lmstudio/snapshot.sh --check    # exit 1 if the shape has changed since
```

## What blocks a generated corpus

`~/.lmstudio/conversations/*.conversation.json` is written by the desktop app's main process
(`LM Studio.app/Contents/Resources/app/.webpack/main` is where the format is named). No headless path
is known to write it, and none could be tested safely:

- `lms chat` / `lms chat -p` talks to the running app's local server. The `lms` binary has no
  reference to `.conversation.json`. Whether the app saves those chats was not tested: the test
  would run against ember's real LM Studio server and write into her real store, because `lms` has
  no throwaway-home mode.
- `lms server` serves an OpenAI-compatible API, which keeps no chat history.
- Every path runs a local model through ember's install. There is no remote provider to point at Z.ai.

**What would unblock it:** an LM Studio CLI or SDK call that writes a conversation file (none exists
as of 0.4.21), or a GUI-automation run of the app with a throwaway `~/.lmstudio`, which would mean
running a second app instance and downloading a small model.

## What is recorded instead

`crates/cv-core/tests/corpus/lmstudio/<app version>/conversations.shape.txt` is the merged JSON
shape (key paths, value types, and tag-like values) of every conversation file on this machine,
with `SNAPSHOT.toml` beside it. `schema_snapshot.py` reads no content into the output. On 2026-10-11
the app was 0.4.21+2 and the newest file was written on 2026-10-02.
