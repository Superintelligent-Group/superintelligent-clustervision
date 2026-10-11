//! **OpenSession** — cv's own interchange format (`docs/OPENSESSION.md`), written by
//! `cv export --format json` and read back in here.
//!
//! OpenSession 0.3 *is* the IR: same keys, same snake_case, blocks tagged `type`, messages
//! carrying `kind`/`origin`. So the writer ([`document`]) is the IR's own serialization plus one
//! version marker, and the reader below is its inverse — which is the only way the spec's central
//! claim ("clustervision's in-memory IR is the reference implementation") can be checked rather
//! than asserted.
//!
//! **Discovery.** A document has no fixed home, so — exactly like the account-export adapters —
//! it is found through the sources the user registers with `cv config --add-export <path>`
//! (unioned with `$CV_EXPORTS`). Under each source (depth ≤ 2) a file counts as a document when
//! it is named `*.opensession.json`, or when it is a `*.json` whose head carries an
//! `open_session` / `openSession` key. **Opt-in**: with nothing registered, discovery is a no-op.
//!
//! **One version.** The reader is the writer run backwards: check and strip the `open_session`
//! marker, hand the rest to the IR's own `Deserialize`. A 0.3 document round-trips byte-identical
//! through [`document`] → [`from_str`] → [`document`]. A 0.2 document (`openSession`, camelCase,
//! `kind`-tagged blocks — written by cv ≤ 0.10) is refused with a pointer to the renames in
//! `docs/OPENSESSION.md`; so is any other version. The ~450-line canonicalizer that used to accept
//! 0.2 spellings beside 0.3 ones was a second, hand-written copy of the IR's vocabulary, and the
//! clean break (`docs/INTERFACE-V2.md` §3: no aliases for old names) applies to documents too.
//!
//! The one rule beyond deserialization: a document that names no harness, or one this build has
//! never heard of, is an `opensession` session (what [`Harness::OpenSession`] exists for) rather
//! than a parse failure — the document is still a session, it just came from somewhere cv cannot
//! name.

use super::{ts_from_value, Adapter};
use crate::ir::*;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The OpenSession version cv writes — the one place it is spelled, so `docs/OPENSESSION.md`,
/// the export and the reader cannot drift apart.
///
/// It lives at the **document boundary** ([`document`]), not as a field on
/// [`Session`](crate::ir::Session): a `Session` is cv's in-memory IR and is serialized raw by
/// `cv show --json`, the MCP payloads and cvd, whose key sets are pinned by tests. A version
/// marker is a fact about a *document* someone wrote to disk or to a wire, not about a value in
/// memory — so it is added exactly where a document is produced, and stripped (ignored) where one
/// is read.
pub const VERSION: &str = "0.3";

/// The document's version key, and the 0.2 spelling — recognized only so a 0.2 document is
/// discovered and refused with a pointer, instead of silently not being a document.
const VERSION_KEY: &str = "open_session";
const VERSION_KEY_V02: &str = "openSession";

/// How much of a plain `*.json` file's head is read to decide whether it is an OpenSession
/// document. cv writes the version marker as the FIRST key, so this is generous; a hand-written
/// document that buries it past 64 KiB is out of scope (name it `*.opensession.json` instead).
const SNIFF_BYTES: usize = 64 * 1024;

// ── Writing: the document boundary ────────────────────────────────────────────

/// A session as an OpenSession document: the IR's own serialization, with `open_session` first.
/// Produced by [`document`]; see [`VERSION`] for why the marker lives here and not on `Session`.
#[derive(Serialize)]
pub struct Document<'a> {
    open_session: &'static str,
    #[serde(flatten)]
    session: &'a Session,
}

/// Wrap `session` as an OpenSession [`Document`] for serialization. The session should be
/// materialized (`Session::materialize`, which `Adapter::parse` already does) so its content
/// serializes as text rather than as lazy spans into a file the reader will not have.
pub fn document(session: &Session) -> Document<'_> {
    Document {
        open_session: VERSION,
        session,
    }
}

// ── The adapter ───────────────────────────────────────────────────────────────

pub struct OpenSession;

impl OpenSession {
    pub fn new() -> Self {
        OpenSession
    }
}

impl Default for OpenSession {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for OpenSession {
    fn harness(&self) -> Harness {
        Harness::OpenSession
    }

    fn storage_root(&self) -> Option<PathBuf> {
        super::export::export_dirs().into_iter().next()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let mut out: Vec<SessionRef> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for path in document_files() {
            if let Some(r) = doc_ref(&path) {
                if seen.insert(r.id.clone()) {
                    out.push(r);
                }
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        read_document(&r.path)
    }
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Every OpenSession document under the registered export sources.
fn document_files() -> Vec<PathBuf> {
    super::export::files_under(super::export::export_dirs(), is_document_file)
}

/// Is this file an OpenSession document? By name (`*.opensession.json`), or by a head sniff of a
/// plain `*.json` for the version key.
fn is_document_file(p: &Path) -> bool {
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.ends_with(".opensession.json") {
        return true;
    }
    if !name.ends_with(".json") {
        return false;
    }
    crate::lazy::read_head(p, SNIFF_BYTES)
        .is_some_and(|head| head.contains(VERSION_KEY) || head.contains(VERSION_KEY_V02))
}

/// A lightweight [`SessionRef`] for one document: metadata only, counting conversational turns
/// (user + assistant, the cross-harness contract) straight off the raw records. A document with
/// no `id` is not a session cv can name, so it is not discovered.
fn doc_ref(path: &Path) -> Option<SessionRef> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let o = v.as_object()?;
    let string = |k: &str| o.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    let message_count = o
        .get("messages")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|m| matches!(m.get("role").and_then(Value::as_str), Some("user" | "assistant")))
                .count()
        })
        .unwrap_or(0);
    Some(SessionRef {
        id: string("id")?.to_string(),
        // The REF is tagged `opensession` because that is the adapter that reads this file; the
        // parsed session keeps the harness the document names (`Session::harness`), which is the
        // fact the document carries and the round trip must preserve.
        harness: Harness::OpenSession,
        path: path.to_path_buf(),
        cwd: string("cwd").map(PathBuf::from),
        title: string("title").map(str::to_string),
        created_at: o.get("created_at").and_then(ts_from_value),
        updated_at: o.get("updated_at").and_then(ts_from_value),
        message_count,
    })
}

// ── Reading a document ────────────────────────────────────────────────────────

/// Read one OpenSession document into the IR. The inverse of [`document`].
pub fn read_document(path: &Path) -> Result<Session> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading OpenSession document {}", path.display()))?;
    let mut s = from_str(&text).with_context(|| format!("parsing OpenSession document {}", path.display()))?;
    // `source_path` is where cv read this session from — this document, not whatever store the
    // document's own `source_path` named on the machine that wrote it.
    s.source_path = Some(path.to_path_buf());
    Ok(s)
}

/// Read an OpenSession document from text: check and strip the version marker, then the IR's own
/// `Deserialize`. A document with no marker is read as a bare session (what `cv show --json`
/// prints); a marker other than [`VERSION`] is an error that names the version and where the
/// renames are documented.
pub fn from_str(text: &str) -> Result<Session> {
    let v: Value = serde_json::from_str(text).context("not JSON")?;
    let Value::Object(mut o) = v else {
        bail!("an OpenSession document is a JSON object");
    };
    match o.remove(VERSION_KEY) {
        None if !o.contains_key(VERSION_KEY_V02) => {}
        Some(Value::String(v)) if v == VERSION => {}
        other => {
            let found = match other {
                Some(v) => v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()),
                None => o[VERSION_KEY_V02]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| "0.2".to_string()),
            };
            bail!(
                "OpenSession {found} document; cv reads {VERSION} only — re-export the session with \
                 `cv export --format json`, or apply the renames in docs/OPENSESSION.md \"0.2 → 0.3\""
            );
        }
    }
    // No harness, or one this build cannot name ⇒ an `opensession` session, not a failure.
    let named = o.get("harness").and_then(Value::as_str).and_then(Harness::parse);
    if named.is_none() {
        o.insert("harness".into(), Value::String(Harness::OpenSession.as_str().into()));
    }
    serde_json::from_value::<Session>(Value::Object(o)).context("not an OpenSession 0.3 document")
}

#[cfg(test)]
mod tests;
