# Native OpenCode export acceptance

The emitter bootstraps fresh stores with the complete **native 2.0.25 schema and matching
migration frontier**, extracted from the immutable native-written
`crates/cv-core/tests/corpus/opencode/2.0.25/.local/share/opencode/opencode.db`.
This reproduces `packages/core/src/database/migration.ts`'s native empty-store bootstrap:
install the current generated schema and journal every migration included in that schema.
Export-time timestamps replace the corpus journal timestamps. No project/session/message/kv
rows are copied. In particular, no `migration.v1-v2` completion marker is copied or reset.
Schema, journal and exported rows commit in one transaction. Existing stores keep their
native schema and journal; v2 tables win in migrated stores, and existing v1 stores keep v1 output.

The SQL and migration ID files are a versioned unit inside the published core crate. To update
them, capture a new native corpus version, run `extract_bootstrap.py CORPUS_DB OUTPUT_DIRECTORY VERSION`,
update the two `include_str!` paths together, and rerun native fresh/existing/migrated acceptance.
Do not infer migration IDs from a newer checkout or mix schema and journal versions.

Generate synthetic export fixtures through the scoped Rust test:

```sh
CV_NATIVE_OPENCODE_OUT=/absolute/new/export-fixtures \
  CLUSTERVISION_HOME=/absolute/scratch/catalog \
  CARGO_TARGET_DIR=/absolute/exclusive/target \
  cargo test -p clustervision-core --lib native_v2_export_fixture_and_atomic_duplicate -j2
```

This writes `opencode.db`, `fresh-v2/opencode.db` and `migrated/opencode.db` under the output directory.
For each, run:

```sh
python3 tools/harness-fixtures/opencode/check_export.py \
  /absolute/opencode-binary /absolute/exported/opencode.db /absolute/new/native-scratch
```

The helper copies the database with SQLite backup into an isolated HOME/XDG tree, runs the native
server, and requires get/list/messages/context plus a real native agent-loop continuation.
It reuses the deterministic Goose OpenAI stub: no upstream provider, credentials or external
messages. It verifies that the provider request contains the imported user prompt, assistant
answer and tool output; native continuation must persist the stub answer. Only known child PIDs
are stopped. Native logs and synthetic evidence remain in the requested scratch directory.
