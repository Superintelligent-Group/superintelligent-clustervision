//! Semantic search via model2vec static embeddings + brute-force cosine.
//!
//! model2vec embeds text with a *static* distilled embedding table (no transformer forward pass,
//! no ONNX): tokenize, look up per-token vectors, mean-pool. It's tiny and CPU-fast, which makes
//! "embed every session up front, then cosine-scan in memory" a perfectly tasteful design for the
//! few-thousand-session scale here. No ANN crate required.
//!
//! On first use the model (`MODEL_REPO`) is fetched from the HuggingFace Hub and cached by
//! `hf-hub` under `~/.cache/huggingface`. To pin a local copy (offline), set
//! `CV_SEARCH_MODEL=/path/to/model-dir`.
//!
//! # The store format
//!
//! Vectors are persisted in a small hand-rolled binary layout (fixed-width little-endian f32s)
//! instead of JSON: a query loads ~N·dim·4 bytes and reinterprets them, rather than JSON-parsing
//! hundreds of thousands of ASCII floats. Layout, all integers little-endian:
//!
//! ```text
//! [8]  magic  b"CVEMBED\x02"
//! u32  dim          (vector dimensionality)
//! u32  count        (number of records)
//! u64  meta_len
//! […]  meta JSON    ({ model, records: [{id, harness, cwd, title, preview, dates}…] })
//! […]  vectors      (count × dim × f32 LE, record i's vector at offset i·dim)
//! ```
//!
//! The metadata stays JSON (it's small, schema-evolvable via serde defaults); only the bulk —
//! the vectors — is fixed-width. A store written by an older build (pure JSON, starts with `{`)
//! still loads, and [`semantic_search`] migrates the legacy default `embeddings.json` to the
//! binary default path on first use.

use crate::Hit;
use anyhow::{bail, Context, Result};
use cv_core::source::{Probe, ReadSet, Snapshot};
use model2vec_rs::model::StaticModel;
use std::io::Write;
use std::path::{Path, PathBuf};

const PROJECTION_VERSION: u32 = 1;

/// A small, strong static-embedding model (~30MB, 256-dim). Distilled from a sentence encoder.
const MODEL_REPO: &str = "minishlab/potion-base-8M";

/// Store header magic: `CVEMBED` + format version 2 (version 1 was the implicit all-JSON store).
const MAGIC: &[u8; 8] = b"CVEMBED\x02";

/// Exact discovery hints visible to the reader, kept separately from its derived metadata.
#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct ReferenceHints {
    cwd: Option<PathBuf>,
    title: Option<String>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    message_count: usize,
}
impl From<&cv_core::SessionRef> for ReferenceHints {
    fn from(r: &cv_core::SessionRef) -> Self {
        Self {
            cwd: r.cwd.clone(),
            title: r.title.clone(),
            created_at: r.created_at,
            updated_at: r.updated_at,
            message_count: r.message_count,
        }
    }
}

/// Per-session metadata, persisted as the store's JSON block (everything but the vector).
#[derive(serde::Serialize, serde::Deserialize)]
struct RecordMeta {
    id: String,
    harness: String,
    cwd: Option<String>,
    title: Option<String>,
    /// A short preview of the body, for display in hits.
    preview: String,
    /// Unix-seconds session dates, surfaced on hits. `serde(default)` keeps stores written
    /// before these fields loading cleanly (their hits are just dateless until the next
    /// `embed_all`).
    #[serde(default)]
    created_at: Option<i64>,
    #[serde(default)]
    updated_at: Option<i64>,
    #[serde(default)]
    source_path: Option<PathBuf>,
    #[serde(default)]
    source: Option<Snapshot>,
    #[serde(default)]
    hints: Option<ReferenceHints>,
}

/// The JSON block inside the binary store (deserialize side; serialization uses a borrowed twin
/// in [`save_store`] to avoid cloning the records).
#[derive(serde::Deserialize)]
struct MetaBlock {
    model: String,
    #[serde(default)]
    projection: u32,
    #[serde(default)]
    model_reads: Option<ReadSet>,
    records: Vec<RecordMeta>,
}

/// The in-memory store: metadata plus one flat `count × dim` vector block (record `i`'s vector is
/// `vectors[i*dim..(i+1)*dim]`).
struct Store {
    model: String,
    projection: u32,
    model_reads: Option<ReadSet>,
    dim: usize,
    meta: Vec<RecordMeta>,
    vectors: Vec<f32>,
}

impl Store {
    fn vector(&self, i: usize) -> &[f32] {
        &self.vectors[i * self.dim..(i + 1) * self.dim]
    }
}

// ---- legacy (pre-binary) store --------------------------------------------------------------

/// The old all-JSON store layout, kept so existing `embeddings.json` files load + migrate.
#[derive(serde::Deserialize)]
struct LegacyStore {
    model: String,
    records: Vec<LegacyRecord>,
}

#[derive(serde::Deserialize)]
struct LegacyRecord {
    id: String,
    harness: String,
    cwd: Option<String>,
    title: Option<String>,
    preview: String,
    #[serde(default)]
    created_at: Option<i64>,
    #[serde(default)]
    updated_at: Option<i64>,
    vector: Vec<f32>,
}

impl From<LegacyStore> for Store {
    fn from(old: LegacyStore) -> Store {
        let dim = old.records.first().map(|r| r.vector.len()).unwrap_or(0);
        let mut meta = Vec::with_capacity(old.records.len());
        let mut vectors = Vec::with_capacity(old.records.len() * dim);
        for r in old.records {
            // A malformed row (wrong dim) would shear the flat block; skip it instead.
            if r.vector.len() != dim {
                continue;
            }
            vectors.extend_from_slice(&r.vector);
            meta.push(RecordMeta {
                id: r.id,
                harness: r.harness,
                cwd: r.cwd,
                title: r.title,
                preview: r.preview,
                created_at: r.created_at,
                updated_at: r.updated_at,
                source_path: None,
                source: None,
                hints: None,
            });
        }
        Store {
            model: old.model,
            projection: 0,
            model_reads: None,
            dim,
            meta,
            vectors,
        }
    }
}

// ---- store I/O -------------------------------------------------------------------------------

/// Serialize + write the store atomically (temp + rename).
fn save_store(path: &Path, store: &Store) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let meta_json = {
        // Serialize the metadata without cloning the records: a borrowed twin of `MetaBlock`.
        #[derive(serde::Serialize)]
        struct MetaBlockRef<'a> {
            model: &'a str,
            projection: u32,
            model_reads: &'a Option<ReadSet>,
            records: &'a [RecordMeta],
        }
        serde_json::to_vec(&MetaBlockRef {
            model: &store.model,
            projection: store.projection,
            model_reads: &store.model_reads,
            records: &store.meta,
        })
        .context("serializing embedding store metadata")?
    };

    let tmp = path.with_extension(format!("tmp.{}", uuid::Uuid::new_v4()));
    let mut created = false;
    let result = (|| -> Result<()> {
        let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        created = true;
        let mut out = std::io::BufWriter::new(file);
        out.write_all(MAGIC)?;
        out.write_all(&u32::try_from(store.dim)?.to_le_bytes())?;
        out.write_all(&u32::try_from(store.meta.len())?.to_le_bytes())?;
        out.write_all(&(meta_json.len() as u64).to_le_bytes())?;
        out.write_all(&meta_json)?;
        for vector in &store.vectors {
            out.write_all(&vector.to_le_bytes())?;
        }
        out.flush()?;
        out.get_ref().sync_all()?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("moving embedding store into place at {}", path.display()))?;
        Ok(())
    })();
    if created && result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    Ok(())
}

/// Load a store: the binary format, or (transparently) a legacy all-JSON store.
fn load_store(path: &Path) -> Result<Store> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading embedding store {} — run `embed_all` first", path.display()))?;
    if bytes.first() == Some(&b'{') {
        let legacy: LegacyStore = serde_json::from_slice(&bytes).context("parsing legacy JSON embedding store")?;
        return Ok(legacy.into());
    }
    if bytes.len() < 24 || &bytes[..8] != MAGIC {
        bail!(
            "unrecognized embedding store {} (bad magic) — re-run `cv index --semantic`",
            path.display()
        );
    }
    let dim = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let count = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let meta_len = u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as usize;
    let meta_end = 24usize
        .checked_add(meta_len)
        .filter(|&e| e <= bytes.len())
        .with_context(|| format!("truncated embedding store {}", path.display()))?;
    let meta: MetaBlock = serde_json::from_slice(&bytes[24..meta_end]).context("parsing embedding store metadata")?;
    let vec_bytes = &bytes[meta_end..];
    let expected_bytes = count
        .checked_mul(dim)
        .and_then(|n| n.checked_mul(4))
        .context("embedding store dimensions overflow")?;
    if meta.records.len() != count || vec_bytes.len() != expected_bytes {
        bail!(
            "corrupt embedding store {} ({} records, {} vector bytes, dim {dim}) — re-run `cv index --semantic`",
            path.display(),
            meta.records.len(),
            vec_bytes.len()
        );
    }
    let vectors: Vec<f32> = vec_bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect();
    Ok(Store {
        model: meta.model,
        projection: meta.projection,
        model_reads: meta.model_reads,
        dim,
        meta: meta.records,
        vectors,
    })
}

// ---- embedding -------------------------------------------------------------------------------

/// The id of the model [`load_model`] would load right now: the `CV_SEARCH_MODEL` override (a
/// local model dir) when set, else [`MODEL_REPO`]. Recorded in the store by [`embed_all`] and
/// compared at query time — query and store vectors must come from the **same** model, or the
/// cosine ranking is cross-space noise.
fn model_id() -> String {
    std::env::var_os("CV_SEARCH_MODEL")
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_else(|| MODEL_REPO.to_string())
}

/// Resolve cached artifact paths first, then load from that pinned local snapshot. An alias
/// moving between batches cannot silently mix embedding spaces. Merely resolving is offline.
struct ModelSpec {
    key: String,
    path: PathBuf,
    reads: ReadSet,
}
fn resolve_model(download: bool) -> Result<Option<ModelSpec>> {
    let id = model_id();
    let root = if std::env::var_os("CV_SEARCH_MODEL").is_some_and(|v| !v.is_empty()) {
        PathBuf::from(&id)
    } else {
        let repo = hf_hub::Cache::from_env().model(id.clone());
        let cached = ["config.json", "tokenizer.json", "model.safetensors"].map(|name| repo.get(name));
        let paths = if let [Some(config), Some(tokenizer), Some(weights)] = cached {
            [config, tokenizer, weights]
        } else if download {
            let api = hf_hub::api::sync::Api::new().context("initializing embedding model download")?;
            let repo = api.model(id.clone());
            [
                repo.get("config.json")?,
                repo.get("tokenizer.json")?,
                repo.get("model.safetensors")?,
            ]
        } else {
            return Ok(None);
        };
        artifact_root(&paths)?
    };
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving model directory {}", root.display()))?;
    let reads = model_reads(&root)?;
    Ok(Some(ModelSpec {
        key: format!("{id}@{}", root.display()),
        path: root,
        reads,
    }))
}
fn artifact_root(paths: &[PathBuf; 3]) -> Result<PathBuf> {
    let parent = paths[0].parent().context("model config has no directory")?;
    anyhow::ensure!(
        paths.iter().all(|p| p.parent() == Some(parent)),
        "model artifacts resolved to different revisions; retry semantic indexing"
    );
    Ok(parent.to_path_buf())
}

fn load_model(spec: &ModelSpec) -> Result<StaticModel> {
    anyhow::ensure!(
        spec.reads.fresh(&mut Probe::default()),
        "embedding model artifacts changed before loading"
    );
    let model = StaticModel::from_pretrained(&spec.path, None, None, None).context("loading pinned embedding model")?;
    anyhow::ensure!(
        spec.reads.fresh(&mut Probe::default()),
        "embedding model artifacts changed while loading"
    );
    Ok(model)
}

/// Sessions whose bodies are batched into one `model.encode` call. Bounds how many session body
/// heads are resident at once during embedding.
const EMBED_BATCH: usize = 128;

/// Refresh only sessions whose selected semantic sources have changed.
pub fn embed_all(path: &Path) -> Result<usize> {
    embed_sessions(path, &cv_core::discover_all())
}

pub fn embed_sessions(path: &Path, refs: &[cv_core::SessionRef]) -> Result<usize> {
    if refs.is_empty() {
        return refresh(
            path,
            refs,
            &model_id(),
            PROJECTION_VERSION,
            ReadSet::default(),
            crate::read_corpus_doc,
            |_| anyhow::bail!("empty semantic inventory unexpectedly demanded a model"),
            save_store,
        );
    }
    // No model acquisition is needed to remove the last session or publish an empty inventory.
    let spec = resolve_model(!refs.is_empty())?;
    let key = spec.as_ref().map_or_else(model_id, |s| s.key.clone());
    let reads = spec.as_ref().map_or_else(ReadSet::default, |s| s.reads.clone());
    let mut model = None;
    refresh(
        path,
        refs,
        &key,
        PROJECTION_VERSION,
        reads,
        crate::read_corpus_doc,
        |bodies| {
            if model.is_none() {
                model = Some(load_model(spec.as_ref().context("embedding model is not cached")?)?);
            }
            Ok(model.as_ref().unwrap().encode(bodies))
        },
        save_store,
    )
}

fn model_reads(root: &Path) -> Result<ReadSet> {
    fn visit(path: &Path, depth: usize, reads: &mut ReadSet) -> Result<()> {
        anyhow::ensure!(depth < 32, "local model directory nesting exceeds 32");
        reads.record(path);
        if path.is_dir() {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                reads.record(&entry.path());
                if entry.file_type()?.is_dir() {
                    visit(&entry.path(), depth + 1, reads)?;
                }
            }
        }
        Ok(())
    }
    let mut reads = ReadSet::default();
    visit(root, 0, &mut reads)?;
    // Directory symlinks are not recursively followed; stamp the model's concrete supported
    // layout files too, so a symlinked 0_StaticEmbedding cannot hide artifact replacement.
    for base in [root.to_path_buf(), root.join("0_StaticEmbedding")] {
        for name in [
            "config.json",
            "config_sentence_transformers.json",
            "tokenizer.json",
            "model.safetensors",
        ] {
            reads.record(&base.join(name));
        }
    }
    if let Some(parent) = root.parent() {
        reads.record(&parent.join("config_sentence_transformers.json"));
    }
    Ok(reads)
}

// The three injected operations keep refresh/publication tests independent of model downloads.
#[allow(clippy::too_many_arguments)]
fn refresh(
    path: &Path,
    refs: &[cv_core::SessionRef],
    model: &str,
    projection: u32,
    model_reads: ReadSet,
    mut read: impl FnMut(&cv_core::SessionRef) -> Result<(crate::Doc, ReadSet)>,
    mut encode: impl FnMut(&[String]) -> Result<Vec<Vec<f32>>>,
    mut save: impl FnMut(&Path, &Store) -> Result<()>,
) -> Result<usize> {
    let mut old = if path.exists() {
        load_store(path)?
    } else {
        Store {
            model: model.into(),
            projection,
            model_reads: None,
            dim: 0,
            meta: vec![],
            vectors: vec![],
        }
    };
    let compatible = old.model == model
        && old.projection == projection
        && old.model_reads.as_ref().is_some_and(|r| r.fresh(&mut Probe::default()));
    let lookup: std::collections::HashMap<_, _> = old
        .meta
        .iter()
        .enumerate()
        .map(|(i, r)| ((r.harness.as_str(), r.id.as_str()), i))
        .collect();
    let mut keep = vec![false; old.meta.len()];
    let mut changed = Vec::new();
    let mut before = Probe::default();
    for r in refs {
        let snapshot = before.snapshot(r);
        let reused = compatible
            && lookup.get(&(r.harness.as_str(), r.id.as_str())).is_some_and(|&i| {
                let m = &old.meta[i];
                let fresh = m.source_path.as_ref() == Some(&r.path)
                    && m.hints.as_ref() == Some(&ReferenceHints::from(r))
                    && m.source.as_ref().is_some_and(|old| old.fresh(&snapshot, &mut before));
                keep[i] |= fresh;
                fresh
            });
        if !reused {
            changed.push((r, snapshot));
        }
    }
    drop(lookup);
    if changed.is_empty() && keep.iter().all(|&v| v) && compatible {
        return Ok(old.meta.len());
    }
    // Compact retained vectors within their existing allocation, preserving old row order.
    let mut retained = Vec::with_capacity(refs.len());
    for (i, meta) in std::mem::take(&mut old.meta).into_iter().enumerate() {
        if keep[i] {
            let dest = retained.len() * old.dim;
            old.vectors.copy_within(i * old.dim..(i + 1) * old.dim, dest);
            retained.push(meta);
        }
    }
    old.vectors.truncate(retained.len() * old.dim);
    old.meta = retained;
    if !compatible {
        old.dim = 0;
    }
    old.model = model.into();
    old.projection = projection;
    old.model_reads = Some(model_reads);
    let mut pending = Vec::new();
    let mut bodies = Vec::new();
    for (r, snapshot) in changed {
        let (doc, reads) = read(r)?;
        pending.push(RecordMeta {
            id: doc.id,
            harness: doc.harness,
            cwd: doc.cwd,
            title: doc.title,
            preview: preview(&doc.body, 200),
            created_at: doc.created_at,
            updated_at: doc.updated_at,
            source_path: Some(r.path.clone()),
            source: Some(snapshot.with_reads(&reads)),
            hints: Some(ReferenceHints::from(r)),
        });
        bodies.push(doc.body);
        if bodies.len() >= EMBED_BATCH {
            encode_batch(&mut encode, &mut pending, &mut bodies, &mut old)?;
        }
    }
    encode_batch(&mut encode, &mut pending, &mut bodies, &mut old)?;
    // A new operation-local probe must re-stat: the pre-parse probe deliberately memoizes.
    let mut after = Probe::default();
    let rows: std::collections::HashMap<_, _> = old
        .meta
        .iter()
        .map(|m| ((m.harness.as_str(), m.id.as_str()), m))
        .collect();
    for r in refs {
        let row = rows
            .get(&(r.harness.as_str(), r.id.as_str()))
            .context("semantic session missing after refresh")?;
        anyhow::ensure!(
            row.source
                .as_ref()
                .is_some_and(|s| s.fresh(&after.snapshot(r), &mut after)),
            "semantic source changed during refresh: {}",
            r.id
        );
    }
    anyhow::ensure!(
        old.model_reads.as_ref().unwrap().fresh(&mut Probe::default()),
        "model changed during semantic refresh"
    );
    save(path, &old)?;
    Ok(old.meta.len())
}

fn encode_batch(
    encode: &mut impl FnMut(&[String]) -> Result<Vec<Vec<f32>>>,
    pending: &mut Vec<RecordMeta>,
    bodies: &mut Vec<String>,
    store: &mut Store,
) -> Result<()> {
    if bodies.is_empty() {
        return Ok(());
    }
    let vectors = encode(bodies)?;
    anyhow::ensure!(
        vectors.len() == pending.len(),
        "embedding model returned wrong record count"
    );
    for (rec, vector) in pending.drain(..).zip(vectors) {
        if store.dim == 0 {
            store.dim = vector.len();
        }
        anyhow::ensure!(
            vector.len() == store.dim && store.dim != 0,
            "embedding model returned inconsistent dimensions"
        );
        store.vectors.extend_from_slice(&vector);
        store.meta.push(rec);
    }
    bodies.clear();
    Ok(())
}

// ---- query -----------------------------------------------------------------------------------

/// Embed `query` and return the top-`k` sessions by cosine similarity.
///
/// Legacy stores still load and migrate to the binary format. Stores without recorded model
/// artifact identities must refresh once through `cv index --semantic` before querying.
///
/// The store's recorded model must match the model the query would embed under (including a
/// `CV_SEARCH_MODEL` override), and the query vector's dimensionality must match the store's —
/// cosine across different embedding spaces is noise, so a mismatch is an error with a re-embed
/// hint rather than silently garbage rankings.
pub fn semantic_search(path: &Path, query: &str, k: usize) -> Result<Vec<Hit>> {
    semantic_search_filtered(path, query, k, crate::SearchFilter::default())
}

/// Filter stored session metadata before selecting the top semantic results.
pub fn semantic_search_filtered(
    path: &Path,
    query: &str,
    k: usize,
    filter: crate::SearchFilter<'_>,
) -> Result<Vec<Hit>> {
    migrate_legacy_sibling(path);
    let store = load_store(path)?;
    if store.meta.is_empty() {
        return Ok(Vec::new());
    }
    anyhow::ensure!(
        store.projection == PROJECTION_VERSION,
        "semantic projection version changed — run cv index --semantic to rebuild vectors"
    );
    let spec = resolve_model(false)?.context("embedding model is not cached — run cv index --semantic")?;
    let current = &spec.key;
    if store.model != *current
        || store.model_reads.is_none()
        || store
            .model_reads
            .as_ref()
            .is_some_and(|r| !r.fresh(&mut Probe::default()))
    {
        bail!(
            "embedding store {} was built with model {:?}, but the current model is {current:?} \
             — run `cv index --semantic` to re-embed (or set CV_SEARCH_MODEL to match)",
            path.display(),
            store.model
        );
    }
    let model = load_model(&spec)?;
    let q = model.encode_single(query);
    if q.len() != store.dim {
        bail!(
            "query embedding has dim {}, but store {} has dim {} — run `cv index --semantic` to re-embed",
            q.len(),
            path.display(),
            store.dim
        );
    }
    Ok(rank_filtered(&store, &q, k, filter))
}

/// One-time upgrade: when `path` is absent but the old default `embeddings.json` exists alongside
/// it, convert that JSON store into the binary format at `path`. Best-effort (a failure just means
/// `load_store` reports the missing file); the legacy file is left untouched.
fn migrate_legacy_sibling(path: &Path) {
    if path.exists() || path.file_name().is_none() {
        return;
    }
    let legacy = path.with_file_name("embeddings.json");
    if legacy == path || !legacy.exists() {
        return;
    }
    if let Ok(store) = load_store(&legacy) {
        if save_store(path, &store).is_ok() {
            eprintln!(
                "(migrated legacy embedding store {} → {})",
                legacy.display(),
                path.display()
            );
        }
    }
}

/// Brute-force cosine ranking of the store against an already-embedded query, mapping the top-`k`
/// records to [`Hit`]s. Split from [`semantic_search`] so the record→hit projection (dates,
/// preview-as-snippet) is testable without loading the embedding model.
#[cfg(test)]
fn rank(store: &Store, q: &[f32], k: usize) -> Vec<Hit> {
    rank_filtered(store, q, k, crate::SearchFilter::default())
}

fn rank_filtered(store: &Store, q: &[f32], k: usize, filter: crate::SearchFilter<'_>) -> Vec<Hit> {
    let mut scored: Vec<(f32, usize)> = (0..store.meta.len())
        .filter(|&i| {
            let record = &store.meta[i];
            filter.harness.is_none_or(|h| record.harness == h)
                && filter
                    .cwd
                    .is_none_or(|needle| record.cwd.as_ref().is_some_and(|cwd| cwd.contains(needle)))
        })
        .map(|i| (cosine(q, store.vector(i)), i))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.truncate(k);

    scored
        .into_iter()
        .map(|(score, i)| {
            let r = &store.meta[i];
            Hit {
                id: r.id.clone(),
                harness: r.harness.clone(),
                cwd: r.cwd.clone(),
                title: r.title.clone(),
                score,
                snippet: r.preview.clone(),
                created_at: r.created_at,
                updated_at: r.updated_at,
                // Semantic store doesn't fold the sub-agent forest (yet); no provenance to carry.
                parent_id: None,
                agent_id: None,
                workflow: None,
            }
        })
        .collect()
}

/// Cosine similarity. model2vec L2-normalizes by default, so this is ~a dot product, but we
/// normalize defensively so unnormalized vectors still rank sanely.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

fn preview(s: &str, max_chars: usize) -> String {
    let t: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let t: String = t.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        format!("{t}…")
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_ref(path: &Path, id: &str) -> cv_core::SessionRef {
        cv_core::SessionRef {
            id: id.into(),
            harness: cv_core::Harness::Claude,
            path: path.into(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 1,
        }
    }
    fn fake_doc(r: &cv_core::SessionRef, dependency: Option<&Path>) -> Result<(crate::Doc, ReadSet)> {
        let mut reads = ReadSet::default();
        let mut body = std::fs::read_to_string(&r.path)?;
        if let Some(path) = dependency {
            reads.record(path);
            if path.exists() {
                body.push_str(&std::fs::read_to_string(path)?);
            }
        }
        Ok((
            crate::Doc {
                id: r.id.clone(),
                harness: r.harness.as_str().into(),
                cwd: None,
                title: None,
                body,
                created_at: None,
                updated_at: None,
            },
            reads,
        ))
    }
    fn fake_encode(bodies: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(bodies
            .iter()
            .map(|b| vec![b.len() as f32, b.bytes().map(u32::from).sum::<u32>() as f32])
            .collect())
    }

    #[test]
    fn cached_model_revisions_and_mutated_artifacts_are_distinct_without_network() {
        let dir = std::env::temp_dir().join(format!("cv-model-identity-{}", uuid::Uuid::new_v4()));
        let cache = hf_hub::Cache::new(dir.clone());
        let repo = cache.model("test/model".into());
        let names = ["config.json", "tokenizer.json", "model.safetensors"];
        for revision in ["first", "second"] {
            let root = dir.join("models--test--model/snapshots").join(revision);
            std::fs::create_dir_all(&root).unwrap();
            for name in names {
                std::fs::write(root.join(name), revision).unwrap();
            }
        }
        repo.create_ref("first").unwrap();
        let first_paths = names.map(|name| repo.get(name).unwrap());
        let first = artifact_root(&first_paths).unwrap();
        let reads = model_reads(&first).unwrap();
        repo.create_ref("second").unwrap();
        let second_paths = names.map(|name| repo.get(name).unwrap());
        let second = artifact_root(&second_paths).unwrap();
        assert_ne!(first, second);
        // Loading the first snapshot remains pinned even if main points elsewhere.
        assert!(reads.fresh(&mut Probe::default()));
        let mixed = [first_paths[0].clone(), second_paths[1].clone(), first_paths[2].clone()];
        assert!(artifact_root(&mixed).is_err());
        let input = dir.join("input.jsonl");
        let store_path = dir.join("vectors.bin");
        std::fs::write(&input, "body").unwrap();
        let refs = [fixture_ref(&input, "a")];
        refresh(
            &store_path,
            &refs,
            "same-revision",
            1,
            reads.clone(),
            |r| fake_doc(r, None),
            fake_encode,
            save_store,
        )
        .unwrap();
        refresh(
            &store_path,
            &refs,
            "same-revision",
            1,
            reads.clone(),
            |_| panic!("unchanged model must reuse"),
            |_| panic!(),
            save_store,
        )
        .unwrap();
        std::fs::write(first.join("model.safetensors"), "replacement weights").unwrap();
        assert!(!reads.fresh(&mut Probe::default()));
        let count = std::cell::Cell::new(0);
        refresh(
            &store_path,
            &refs,
            "same-revision",
            1,
            model_reads(&first).unwrap(),
            |r| {
                count.set(count.get() + 1);
                fake_doc(r, None)
            },
            fake_encode,
            save_store,
        )
        .unwrap();
        assert_eq!(
            count.get(),
            1,
            "artifact replacement invalidates vectors even with the same alias/revision path"
        );
        #[cfg(unix)]
        {
            let target = dir.join("linked-static");
            std::fs::create_dir(&target).unwrap();
            std::fs::write(target.join("model.safetensors"), "linked weights").unwrap();
            std::os::unix::fs::symlink(&target, first.join("0_StaticEmbedding")).unwrap();
            let linked_reads = model_reads(&first).unwrap();
            std::fs::write(target.join("model.safetensors"), "changed linked weights").unwrap();
            assert!(!linked_reads.fresh(&mut Probe::default()));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_refresh_composes_real_claude_projection_with_fake_embedder() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-compose-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let input = dir.join("session.jsonl");
        let path = dir.join("vectors.bin");
        let user = serde_json::json!({"type":"user", "uuid":"u", "sessionId":"a", "message":{
            "role":"user", "content":[{"type":"text", "text":"Inspect the file"}]}});
        let assistant = serde_json::json!({"type":"assistant", "uuid":"v", "parentUuid":"u", "sessionId":"a", "message":{
            "role":"assistant", "content":[{"type":"tool_use", "id":"call", "name":"Edit", "input":{"arg":"x".repeat(200000)}}]}});
        std::fs::write(&input, format!("{user}\n{assistant}\n")).unwrap();
        let refs = [fixture_ref(&input, "a")];
        let count = std::cell::Cell::new(0);
        refresh(
            &path,
            &refs,
            "fake",
            1,
            ReadSet::default(),
            crate::read_corpus_doc,
            |bodies| {
                count.set(count.get() + bodies.len());
                assert_eq!(bodies[0].len(), 64 * 1024);
                assert!(bodies[0].starts_with("Inspect the file\nEdit {\"arg\":\""));
                fake_encode(bodies)
            },
            save_store,
        )
        .unwrap();
        assert_eq!(count.get(), 1);
        let saved = load_store(&path).unwrap();
        assert_eq!(saved.meta[0].title.as_deref(), Some("Inspect the file"));
        refresh(
            &path,
            &refs,
            "fake",
            1,
            ReadSet::default(),
            |_| panic!("fresh native source should not parse"),
            |_| panic!("fresh native source should not load model"),
            save_store,
        )
        .unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Isolated requested-Rust-heap oracle; includes native parsing and atomic fake-vector save.
    #[cfg(feature = "probe-heap")]
    #[test]
    #[ignore = "explicit giant fixture and isolated allocator process required"]
    fn production_semantic_heap_probe() {
        use std::io::Write;
        let input = std::path::PathBuf::from(std::env::var_os("CV_SWEEP_INPUT_PROBE").expect("CV_SWEEP_INPUT_PROBE"));
        let bytes: usize = std::env::var("CV_SWEEP_INPUT_BYTES")
            .expect("CV_SWEEP_INPUT_BYTES")
            .parse()
            .unwrap();
        assert!(bytes >= 64 * 1024);
        let dir = std::env::temp_dir().join(format!("cv-semantic-heap-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let mut reports = Vec::new();
        for kind in ["arguments", "plain-text", "escaped-text"] {
            let source = if kind == "arguments" {
                input.clone()
            } else {
                dir.join(format!("{kind}.jsonl"))
            };
            if kind != "arguments" {
                let mut out = std::io::BufWriter::new(std::fs::File::create(&source).unwrap());
                out.write_all(br#"{"type":"user","uuid":"u","sessionId":"perf","message":{"role":"user","content":[{"type":"text","text":""#).unwrap();
                let scalar = if kind == "escaped-text" { "\\u0078" } else { "x" };
                let block = scalar.repeat(8192);
                for _ in 0..bytes / 8192 {
                    out.write_all(block.as_bytes()).unwrap();
                }
                out.write_all(scalar.repeat(bytes % 8192).as_bytes()).unwrap();
                out.write_all(b"\"}]}}\n").unwrap();
                out.flush().unwrap();
            }
            let path = dir.join(format!("{kind}.bin"));
            let mut r = fixture_ref(&source, "perf");
            r.message_count = if kind == "arguments" { 2 } else { 1 };
            let refs = [r];
            let mut calls = 0;
            let baseline = cv_testkit::heap::begin();
            refresh(
                &path,
                &refs,
                "fake",
                PROJECTION_VERSION,
                ReadSet::default(),
                crate::read_corpus_doc,
                |bodies| {
                    calls += 1;
                    assert_eq!(bodies.len(), 1);
                    if kind == "arguments" {
                        let prefix = "Inspect the file\nEdit {\"file_path\":\"/perf/a.rs\",\"content\":\"";
                        assert_eq!(bodies[0].len(), 64 * 1024);
                        assert!(bodies[0].starts_with(prefix));
                        assert!(bodies[0][prefix.len()..].bytes().all(|b| b == b'x'));
                    } else if kind == "plain-text" {
                        assert_eq!(bodies[0], "x".repeat(64 * 1024));
                    } else {
                        assert_eq!(bodies[0], format!("{}\n", "x".repeat(64 * 1024 / 6)));
                    }
                    fake_encode(bodies)
                },
                save_store,
            )
            .unwrap();
            let heap = cv_testkit::heap::end(baseline);
            assert_eq!(calls, 1);
            assert!(
                heap.peak_above_baseline_bytes < 8 * 1024 * 1024,
                "{kind}: {}",
                heap.peak_above_baseline_bytes
            );
            refresh(
                &path,
                &refs,
                "fake",
                PROJECTION_VERSION,
                ReadSet::default(),
                |_| panic!("unchanged producer must not parse"),
                |_| panic!("unchanged producer must not embed"),
                |_, _| panic!("unchanged vectors must not save"),
            )
            .unwrap();
            reports.push(serde_json::json!({"kind":kind,"decoded_payload_bytes":bytes,"source_bytes":std::fs::metadata(&source).unwrap().len(),"heap":heap}));
        }
        println!(
            "{}",
            serde_json::json!({"semantic_projection_heap":reports,"ceiling_bytes":8 * 1024 * 1024,"model":"fake; no model allocations or downloads"})
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_freshness_compares_exact_reference_hints_not_derived_metadata() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-hints-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let input = dir.join("session.jsonl");
        let path = dir.join("vectors.bin");
        std::fs::write(&input, "body").unwrap();
        let mut r = fixture_ref(&input, "a");
        r.cwd = Some("/discovery/cwd".into());
        r.title = Some("discovery title".into());
        r.created_at = chrono::DateTime::from_timestamp(42, 1);
        let count = std::cell::Cell::new(0);
        let run = |r: &cv_core::SessionRef| {
            refresh(
                &path,
                std::slice::from_ref(r),
                "fake",
                1,
                ReadSet::default(),
                |r| {
                    count.set(count.get() + 1);
                    let (mut d, reads) = fake_doc(r, None)?;
                    d.cwd = Some("/derived/cwd".into());
                    d.title = Some("derived title".into());
                    Ok((d, reads))
                },
                fake_encode,
                save_store,
            )
            .unwrap();
        };
        run(&r);
        assert_eq!(count.get(), 1);
        run(&r);
        assert_eq!(count.get(), 1);
        r.title = Some("new hint".into());
        run(&r);
        assert_eq!(count.get(), 2);
        r.message_count += 1;
        run(&r);
        assert_eq!(count.get(), 3);
        r.cwd = Some("/other/hint".into());
        run(&r);
        assert_eq!(count.get(), 4);
        r.created_at = chrono::DateTime::from_timestamp(42, 2);
        run(&r);
        assert_eq!(count.get(), 5);
        r.updated_at = chrono::DateTime::from_timestamp(43, 0);
        run(&r);
        assert_eq!(count.get(), 6);
        run(&r);
        assert_eq!(count.get(), 6);
        let mut old = load_store(&path).unwrap();
        old.projection = 0;
        save_store(&path, &old).unwrap();
        assert!(semantic_search(&path, "query", 1)
            .unwrap_err()
            .to_string()
            .contains("projection version changed"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_refresh_reuses_only_fresh_rows_without_loading_model() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-reuse-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let a = dir.join("a.jsonl");
        let b = dir.join("b.jsonl");
        let dep = dir.join("dep");
        let path = dir.join("store.bin");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "bb").unwrap();
        let refs = [fixture_ref(&a, "a"), fixture_ref(&b, "b")];
        let run = |model: &str, version, count: &std::cell::Cell<usize>| {
            refresh(
                &path,
                &refs,
                model,
                version,
                ReadSet::default(),
                |r| {
                    count.set(count.get() + 1);
                    fake_doc(r, Some(&dep))
                },
                fake_encode,
                save_store,
            )
            .unwrap()
        };
        let count = std::cell::Cell::new(0);
        assert_eq!(run("fake", 1, &count), 2);
        assert_eq!(count.get(), 2);
        let initial = std::fs::read(&path).unwrap();
        refresh(
            &path,
            &refs,
            "fake",
            1,
            ReadSet::default(),
            |_| panic!("unchanged rows must not parse"),
            |_| panic!("must not load model"),
            |_, _| panic!("unchanged store must not save"),
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), initial);
        std::fs::write(&a, "changed primary").unwrap();
        count.set(0);
        run("fake", 1, &count);
        assert_eq!(count.get(), 1);
        // Absence was recorded before the initial projection, so an adopted sidecar invalidates.
        std::fs::write(&dep, "output").unwrap();
        count.set(0);
        run("fake", 1, &count);
        assert_eq!(count.get(), 2);
        count.set(0);
        run("other", 1, &count);
        assert_eq!(count.get(), 2);
        count.set(0);
        run("other", 2, &count);
        assert_eq!(count.get(), 2);
        refresh(
            &path,
            &refs[..1],
            "other",
            2,
            ReadSet::default(),
            |_| panic!("removal needs no parsing"),
            |_| panic!("removal needs no model"),
            save_store,
        )
        .unwrap();
        assert_eq!(load_store(&path).unwrap().meta.len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_refresh_failures_leave_old_vectors_and_baselines_atomic() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-failure-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let input = dir.join("input.jsonl");
        let path = dir.join("store");
        std::fs::write(&input, "before").unwrap();
        let refs = [fixture_ref(&input, "a")];
        refresh(
            &path,
            &refs,
            "fake",
            1,
            ReadSet::default(),
            |r| fake_doc(r, None),
            fake_encode,
            save_store,
        )
        .unwrap();
        let initial = std::fs::read(&path).unwrap();
        let moved = dir.join("saved-input.jsonl");
        std::fs::rename(&input, &moved).unwrap();
        assert!(refresh(
            &path,
            &refs,
            "fake",
            1,
            ReadSet::default(),
            |r| fake_doc(r, None),
            |_| panic!("missing selected source must not encode"),
            |_, _| panic!("missing selected source must not save")
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), initial);
        std::fs::rename(&moved, &input).unwrap();
        std::fs::write(&input, "changed").unwrap();
        assert!(refresh(
            &path,
            &refs,
            "fake",
            1,
            ReadSet::default(),
            |r| fake_doc(r, None),
            fake_encode,
            |_, _| anyhow::bail!("injected save failure")
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), initial);
        assert!(refresh(
            &path,
            &refs,
            "fake",
            1,
            ReadSet::default(),
            |r| fake_doc(r, None),
            |_| anyhow::bail!("injected model failure"),
            save_store
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), initial);
        assert!(refresh(
            &path,
            &refs,
            "fake",
            1,
            ReadSet::default(),
            |r| {
                let doc = fake_doc(r, None)?;
                std::fs::write(&input, "changed during projection")?;
                Ok(doc)
            },
            fake_encode,
            save_store
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), initial);
        let count = std::cell::Cell::new(0);
        refresh(
            &path,
            &refs,
            "fake",
            1,
            ReadSet::default(),
            |r| {
                count.set(count.get() + 1);
                fake_doc(r, None)
            },
            fake_encode,
            save_store,
        )
        .unwrap();
        assert_eq!(count.get(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_legacy_metadata_rebuilds_once_and_save_failure_cleans_temp() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-legacy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let input = dir.join("input.jsonl");
        let path = dir.join("store");
        std::fs::write(&input, "new").unwrap();
        let refs = [fixture_ref(&input, "a")];
        let mut old = store(vec![(meta("a", None), vec![1., 2.])]);
        old.projection = 0;
        save_store(&path, &old).unwrap();
        let count = std::cell::Cell::new(0);
        refresh(
            &path,
            &refs,
            "test",
            1,
            ReadSet::default(),
            |r| {
                count.set(count.get() + 1);
                fake_doc(r, None)
            },
            fake_encode,
            save_store,
        )
        .unwrap();
        assert_eq!(count.get(), 1);
        refresh(
            &path,
            &refs,
            "test",
            1,
            ReadSet::default(),
            |_| panic!(),
            |_| panic!(),
            save_store,
        )
        .unwrap();
        let blocked = dir.join("directory");
        std::fs::create_dir(&blocked).unwrap();
        assert!(save_store(&blocked, &old).is_err());
        assert!(std::fs::read_dir(&dir).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("tmp.")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn meta(id: &str, dates: Option<(i64, i64)>) -> RecordMeta {
        RecordMeta {
            id: id.into(),
            harness: "claude".into(),
            cwd: None,
            title: Some(format!("title-{id}")),
            preview: format!("preview-{id}"),
            created_at: dates.map(|(c, _)| c),
            updated_at: dates.map(|(_, u)| u),
            source_path: None,
            source: None,
            hints: None,
        }
    }

    fn store(records: Vec<(RecordMeta, Vec<f32>)>) -> Store {
        let dim = records.first().map(|(_, v)| v.len()).unwrap_or(0);
        let mut s = Store {
            model: "test".into(),
            projection: PROJECTION_VERSION,
            model_reads: Some(ReadSet::default()),
            dim,
            meta: Vec::new(),
            vectors: Vec::new(),
        };
        for (m, v) in records {
            assert_eq!(v.len(), dim);
            s.vectors.extend_from_slice(&v);
            s.meta.push(m);
        }
        s
    }

    #[test]
    fn rank_filtered_selects_rare_metadata_before_limit() {
        let mut records: Vec<_> = (0..100)
            .map(|i| (meta(&format!("other-{i}"), None), vec![1.0, 0.0]))
            .collect();
        let mut wanted = meta("wanted", None);
        wanted.harness = "codex".into();
        wanted.cwd = Some("/repo/mixed.Path/project".into());
        records.push((wanted, vec![0.5, 0.5]));
        let store = store(records);
        for filter in [
            crate::SearchFilter {
                harness: Some("codex"),
                cwd: None,
            },
            crate::SearchFilter {
                harness: None,
                cwd: Some("mixed.Path/proj"),
            },
            crate::SearchFilter {
                harness: Some("codex"),
                cwd: Some("mixed.Path/proj"),
            },
        ] {
            let hits = rank_filtered(&store, &[1.0, 0.0], 1, filter);
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].id, "wanted");
            assert_eq!(hits[0].snippet, "preview-wanted");
        }
        assert!(rank_filtered(
            &store,
            &[1.0, 0.0],
            1,
            crate::SearchFilter {
                harness: None,
                cwd: Some("MIXED")
            }
        )
        .is_empty());
        assert!(rank_filtered(&store, &[1.0, 0.0], 0, crate::SearchFilter::default()).is_empty());
    }

    /// Dates stored on a record surface on its hit; records embedded without dates stay `None`.
    #[test]
    fn rank_carries_dates_onto_hits() {
        let store = store(vec![
            (meta("dated", Some((100, 200))), vec![1.0, 0.0]),
            (meta("dateless", None), vec![0.0, 1.0]),
        ]);
        let hits = rank(&store, &[1.0, 0.0], 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, "dated"); // cosine 1.0 ranks first
        assert_eq!(hits[0].created_at, Some(100));
        assert_eq!(hits[0].updated_at, Some(200));
        assert_eq!(hits[0].snippet, "preview-dated");
        assert_eq!(hits[1].id, "dateless");
        assert_eq!(hits[1].created_at, None);
        assert_eq!(hits[1].updated_at, None);
    }

    /// The binary store round-trips exactly: metadata, dim, and every vector bit.
    #[test]
    fn binary_store_roundtrips() {
        let dir = std::env::temp_dir().join(format!("cv-emb-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("embeddings.bin");

        let s = store(vec![
            (meta("a", Some((1, 2))), vec![0.25, -1.5, 3.0]),
            (meta("b", None), vec![f32::MIN_POSITIVE, 0.0, -0.0]),
        ]);
        save_store(&path, &s).unwrap();
        let back = load_store(&path).unwrap();
        assert_eq!(back.model, "test");
        assert_eq!(back.dim, 3);
        assert_eq!(back.meta.len(), 2);
        assert_eq!(back.meta[0].id, "a");
        assert_eq!(back.meta[0].created_at, Some(1));
        assert_eq!(back.meta[1].title.as_deref(), Some("title-b"));
        assert_eq!(back.vectors, s.vectors); // bit-exact through to_le_bytes/from_le_bytes
                                             // And the loaded store ranks like the in-memory one.
        assert_eq!(rank(&back, &[0.25, -1.5, 3.0], 1)[0].id, "a");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// An embedding store written by an older build (pure JSON, with or without the date fields)
    /// must still load — and `migrate_legacy_sibling` upgrades it to the binary default path.
    #[test]
    fn legacy_json_store_loads_and_migrates() {
        let json = r#"{
            "model": "minishlab/potion-base-8M",
            "records": [{
                "id": "old", "harness": "claude", "cwd": null, "title": "t",
                "preview": "p", "vector": [0.5, 0.5]
            }]
        }"#;
        let dir = std::env::temp_dir().join(format!("cv-emb-legacy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let legacy_path = dir.join("embeddings.json");
        std::fs::write(&legacy_path, json).unwrap();

        // Loads directly (dateless fields default to None), ranks, and maps to a hit.
        let store = load_store(&legacy_path).unwrap();
        assert_eq!(store.dim, 2);
        assert_eq!(store.meta.len(), 1);
        assert_eq!(store.meta[0].created_at, None);
        let hits = rank(&store, &[0.5, 0.5], 1);
        assert_eq!(hits[0].id, "old");
        assert_eq!(hits[0].created_at, None);

        // Sibling migration: a missing binary store next to the legacy JSON gets created.
        let bin_path = dir.join("embeddings.bin");
        migrate_legacy_sibling(&bin_path);
        assert!(bin_path.exists(), "legacy store should migrate to the binary path");
        let migrated = load_store(&bin_path).unwrap();
        assert_eq!(migrated.meta.len(), 1);
        assert_eq!(migrated.vectors, vec![0.5, 0.5]);
        assert!(legacy_path.exists(), "legacy file is left in place");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Garbage on disk fails loudly with a re-index hint instead of mis-parsing.
    #[test]
    fn bad_magic_is_an_error() {
        let dir = std::env::temp_dir().join(format!("cv-emb-bad-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("embeddings.bin");
        std::fs::write(&path, b"not a store at all").unwrap();
        let err = load_store(&path).err().expect("bad magic must fail").to_string();
        assert!(err.contains("bad magic"), "got: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
