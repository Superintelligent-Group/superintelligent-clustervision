//! Complete JSON values with explicit owned and read-only source representations.
//!
//! Generic Serde serialization always emits the actual value, resolving a selected source value
//! when necessary. Consumers that only need projections or compact serialized length can use
//! [`Json::with_view`], [`Json::canonical_len`] and [`Json::write_canonical`] without constructing
//! a descendant value tree. This module does not yet change the IR or enable lazy producers.
//!
//! Native source views borrow mapped bytes. Without `mmap`, positioned reads retain only bounded
//! input windows shared by all projections in an operation. The canonical walker bounds decoded
//! string scratch in both builds;
//! object key/order state grows with object width. Neither limitation is hidden by this API.

use crate::harness::store::document;
#[cfg(not(feature = "mmap"))]
use crate::harness::store::document::range::{Range, RangeSource};
#[cfg(any(feature = "mmap", test))]
use crate::harness::store::document::Raw;
#[cfg(feature = "mmap")]
use crate::harness::store::document::{Record, Source};
use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Number, Value};
use std::borrow::Cow;
use std::fs::Metadata;
#[cfg(feature = "mmap")]
use std::io::Cursor;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// A complete JSON value. Source descriptors clone cheaply; owned values retain Value semantics.
/// Deserialization always produces `Owned`, including objects that resemble source descriptors.
#[derive(Clone, Debug)]
pub enum Json {
    Owned(Value),
    Source(Arc<JsonRange>),
}

/// A validated complete value range, tied to a read-only source identity. Fields are private so
/// callers cannot create an unchecked range. Normal edits, appends and replacement invalidate it.
/// Metadata checks do not provide an immutable snapshot against arbitrary external in-place
/// rewrites or prevent external truncation of a mapped file.
#[derive(Debug)]
pub struct JsonRange {
    source: JsonSource,
    offset: u64,
    len: u64,
}

/// Distinguish invalid content (archives historically appear empty) from unavailable or changed
/// sources, whose failures must abort consumers and cannot publish freshness.
#[derive(Debug)]
pub(crate) struct InvalidJson;
impl std::fmt::Display for InvalidJson {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid JSON document")
    }
}
impl std::error::Error for InvalidJson {}

/// Capture before an adapter validates its source. Only already accepted complete values may use
/// validated_range afterward; this avoids validating each giant input again at producer handoff.
#[derive(Clone, Debug)]
pub(crate) struct JsonSource(Arc<CapturedSource>);
#[derive(Debug)]
struct CapturedSource {
    path: PathBuf,
    identity: Identity,
}
#[derive(Debug, PartialEq, Eq)]
struct Identity {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed: (i64, i64),
}
impl Identity {
    fn read(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path).with_context(|| format!("reading JSON source {}", path.display()))?;
        Self::from_metadata(metadata)
    }
    fn from_metadata(metadata: Metadata) -> Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(metadata.is_file(), "JSON source is not a regular file");
        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}
impl JsonSource {
    pub(crate) fn capture(path: &Path) -> Result<Self> {
        // Preserve symlink spelling so later retargeting changes the observed identity.
        let path = std::path::absolute(path).context("locating JSON source")?;
        let identity = Identity::read(&path)?;
        Ok(Self(Arc::new(CapturedSource { path, identity })))
    }
    pub(crate) fn check(&self) -> Result<()> {
        anyhow::ensure!(
            Identity::read(&self.0.path)? == self.0.identity,
            "JSON source changed: {}",
            self.0.path.display()
        );
        Ok(())
    }
    #[cfg(any(not(feature = "mmap"), test))]
    pub(crate) fn path(&self) -> &Path {
        &self.0.path
    }
    #[cfg(any(not(feature = "mmap"), test))]
    pub(crate) fn len(&self) -> u64 {
        self.0.identity.len
    }
    pub(crate) fn check_metadata(&self, metadata: Metadata) -> Result<()> {
        anyhow::ensure!(
            Identity::from_metadata(metadata)? == self.0.identity,
            "JSON source changed: {}",
            self.0.path.display()
        );
        Ok(())
    }
    fn range(&self, offset: u64, len: u64) -> Result<JsonRange> {
        anyhow::ensure!(len != 0, "JSON range is empty");
        anyhow::ensure!(
            offset.checked_add(len).is_some_and(|end| end <= self.0.identity.len),
            "JSON range is out of bounds"
        );
        Ok(JsonRange {
            source: self.clone(),
            offset,
            len,
        })
    }
    pub(crate) fn read_document<T>(&self, f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>) -> Result<T> {
        self.range(0, self.0.identity.len)?
            .with_validation(Validation::Value, f)
    }
    /// Archive grammar and UTF-8 are whole-source; each child resets Value's depth budget.
    /// Invalid children remain None so a consumer can preserve first-record shape semantics.
    pub(crate) fn read_archive<T>(&self, f: impl for<'a> FnOnce(Option<AcceptedArray<'a>>) -> Result<T>) -> Result<T> {
        self.range(0, self.0.identity.len)?
            .with_validation(Validation::Archive, |view| f(view.array()?.map(AcceptedArray)))
    }
    pub(crate) fn metadata_bytes(&self) -> usize {
        std::mem::size_of::<CapturedSource>() + 2 * std::mem::size_of::<usize>() + self.0.path.capacity()
    }
    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        self.0.path == other.0.path && self.0.identity == other.0.identity
    }
    /// Caller must have syntax AND Value-compatible scalar/depth validation for this complete
    /// range since capture. Quotes/containers are included; surrounding whitespace is excluded.
    /// Never use this for a decoded JSON string that represents a different IR argument value.
    pub(crate) fn validated_range(&self, offset: u64, len: u64) -> Result<Json> {
        self.check()?;
        Ok(Json::Source(Arc::new(self.range(offset, len)?)))
    }
}
impl JsonRange {
    pub fn path(&self) -> &Path {
        &self.source.0.path
    }
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    #[cfg(feature = "mmap")]
    fn with_raw<T>(&self, f: impl for<'a> FnOnce(Raw<'a>) -> Result<T>) -> Result<T> {
        self.source.check()?;
        let file = std::fs::File::open(self.path())?;
        self.source.check_metadata(file.metadata()?)?;
        let source = Source::map_file(&file)?;
        self.source.check()?;
        // Even validated ranges keep the safe UTF-8 check on newly mapped/read external bytes.
        // No unchecked string cast is justified by metadata identity.
        let result = source
            .slice(self.offset, self.len)
            .ok_or_else(|| InvalidJson.into())
            .and_then(f);
        // Check even after callback failure: an intentional staged-output cap must not hide
        // a source mutation. Source failure takes precedence over the callback error.
        self.source.check_metadata(file.metadata()?)?;
        self.source.check()?;
        result
    }
    fn with_view<T>(&self, f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>) -> Result<T> {
        self.with_validation(Validation::Cached, f)
    }
    fn with_validation<T>(
        &self,
        validation: Validation,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        #[cfg(feature = "mmap")]
        {
            self.with_raw(|raw| {
                let base = raw.get().as_ptr() as usize;
                let index = document::Index::default();
                let raw = if matches!(validation, Validation::Cached) {
                    raw
                } else {
                    let syntax: &crate::lazy::RawValue = serde_json::from_str(raw.get()).map_err(|_| InvalidJson)?;
                    let checked = Raw::new(syntax.get(), &index);
                    if matches!(validation, Validation::Value) {
                        anyhow::ensure!(document::accepts_value(checked), "invalid JSON scalar or nesting depth");
                    }
                    checked
                };
                f(JsonView(View::Source(SourceView { raw, range: self, base })))
            })
        }
        #[cfg(not(feature = "mmap"))]
        {
            let source = RangeSource::from_captured(self.source.clone())?;
            let requested = Range {
                offset: self.offset,
                len: self.len,
            };
            let result = (|| {
                let value = match validation {
                    Validation::Cached => requested,
                    Validation::Value | Validation::Archive => {
                        let valid = if matches!(validation, Validation::Value) {
                            source.validate(requested)?
                        } else {
                            source.syntax(requested)?
                        };
                        if !valid {
                            return Err(InvalidJson.into());
                        }
                        source.cursor(requested)?.value()?
                    }
                };
                f(JsonView(View::Portable(PortableView {
                    source: &source,
                    owner: self,
                    value,
                })))
            })();
            source.check_identity()?;
            result
        }
    }
}

#[derive(Clone, Copy)]
enum Validation {
    Cached,
    Value,
    Archive,
}

pub(crate) struct AcceptedArray<'a>(JsonArray<'a>);
impl<'a> Iterator for AcceptedArray<'a> {
    type Item = Result<Option<JsonView<'a>>>;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|value| {
            let value = value?;
            Ok(value.accepts_value()?.then_some(value))
        })
    }
}

impl From<Value> for Json {
    fn from(value: Value) -> Self {
        Self::Owned(value)
    }
}
impl Default for Json {
    fn default() -> Self {
        Self::Owned(Value::Null)
    }
}
impl Serialize for Json {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.resolve().map_err(serde::ser::Error::custom)?.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Value::deserialize(deserializer).map(Self::Owned)
    }
}
impl Json {
    /// Validate a complete JSON range before constructing a source descriptor. Missing files,
    /// overflow, truncated ranges and values rejected by serde_json Value return errors.
    pub fn source(path: impl AsRef<Path>, offset: u64, len: u64) -> Result<Self> {
        let source = JsonSource::capture(path.as_ref())?;
        let mut range = source.range(offset, len)?;
        #[cfg(feature = "mmap")]
        let (leading, normalized_len) = range.with_raw(|raw| {
            // RawValue validates syntax; the shared strict preflight supplies Value's finite
            // numbers, UTF-16 and recursion budget without decoding descendant strings.
            let syntax: &crate::lazy::RawValue = serde_json::from_str(raw.get()).context("invalid JSON range")?;
            let index = document::Index::default();
            let checked = Raw::new(syntax.get(), &index);
            anyhow::ensure!(
                document::accepts_value(checked),
                "JSON range is not a valid serde_json Value"
            );
            Ok((
                (syntax.get().as_ptr() as usize - raw.get().as_ptr() as usize) as u64,
                syntax.get().len() as u64,
            ))
        })?;
        #[cfg(not(feature = "mmap"))]
        let (leading, normalized_len) = {
            let context = RangeSource::from_captured(source.clone())?;
            let requested = Range { offset, len };
            anyhow::ensure!(
                context.validate(requested)?,
                "JSON range is not a valid serde_json Value"
            );
            let normalized = context.cursor(requested)?.value()?;
            context.check_identity()?;
            (normalized.offset - offset, normalized.len)
        };
        range.offset += leading;
        range.len = normalized_len;
        range.source.validated_range(range.offset, range.len)
    }
    /// Select a child value without materializing its descendants. Source children retain their
    /// validated range and source identity; missing keys/nonobjects remain distinct from errors.
    pub fn select(&self, key: &str) -> Result<Option<Self>> {
        self.with_view(|view| view.get(key)?.map(JsonView::to_json).transpose())
    }

    /// Borrow owned data or materialize the selected complete source value. This is deliberately
    /// explicit: there is no implicit Deref that builds a giant tree or substitutes missing data.
    pub fn resolve(&self) -> Result<Cow<'_, Value>> {
        match self {
            Self::Owned(value) => Ok(Cow::Borrowed(value)),
            Self::Source(range) => range.with_view(|view| Ok(Cow::Owned(view.resolve()?.into_owned()))),
        }
    }
    /// Resolve before mutation. A failed read leaves the source descriptor intact.
    pub fn make_owned(&mut self) -> Result<&mut Value> {
        if let Self::Source(_) = self {
            *self = Self::Owned(self.resolve()?.into_owned());
        }
        match self {
            Self::Owned(value) => Ok(value),
            Self::Source(_) => unreachable!(),
        }
    }
    /// A scoped view cannot outlive its backing mapping/buffer. All source read/validation errors
    /// are distinct from a missing field; a changed source also rejects the completed operation.
    pub fn with_view<T>(&self, f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>) -> Result<T> {
        match self {
            Self::Owned(value) => f(JsonView(View::Owned(value))),
            Self::Source(range) => range.with_view(f),
        }
    }
    /// Byte length of Value's compact serialization, preserving insertion order and duplicates'
    /// final values at their first key position. Does not materialize descendant strings.
    pub fn canonical_len(&self) -> Result<u64> {
        let mut counter = CountWriter(0);
        self.write_canonical(&mut counter)?;
        Ok(counter.0)
    }
    /// A compact JSON byte prefix ending on a Unicode scalar boundary, plus whether the complete
    /// value was truncated. Source validation and the final identity check still apply. No more
    /// than max_bytes of output are retained; the prefix need not itself be complete JSON.
    pub fn canonical_prefix(&self, max_bytes: usize) -> Result<(String, bool)> {
        self.with_view(|view| view.canonical_prefix(max_bytes))
    }
    /// Emit the exact compact JSON that serde_json serializes from the complete Value.
    ///
    /// An I/O or source-change error may occur after a prefix has been written. Callers must
    /// discard/reset staged output on Err and must not publish consumer freshness for that pass.
    /// Generic Serialize instead resolves the selected value, as serializers cannot stream pieces
    /// of one string. This explicit writer keeps decoded leaf scratch bounded.
    pub fn write_canonical(&self, writer: &mut impl Write) -> Result<()> {
        self.with_view(|view| view.write_canonical(writer))
    }
}

#[derive(Clone, Copy)]
enum View<'a> {
    Owned(&'a Value),
    #[cfg(feature = "mmap")]
    Source(SourceView<'a>),
    #[cfg(not(feature = "mmap"))]
    Portable(PortableView<'a>),
}
#[cfg(feature = "mmap")]
#[derive(Clone, Copy)]
struct SourceView<'a> {
    raw: Raw<'a>,
    range: &'a JsonRange,
    base: usize,
}
#[cfg(not(feature = "mmap"))]
#[derive(Clone, Copy)]
struct PortableView<'a> {
    source: &'a RangeSource,
    owner: &'a JsonRange,
    value: Range,
}
/// A typed projection of an already validated complete JSON value, scoped by Json::with_view.
#[derive(Clone, Copy)]
pub struct JsonView<'a>(View<'a>);
impl<'a> From<&'a Value> for JsonView<'a> {
    fn from(value: &'a Value) -> Self {
        Self(View::Owned(value))
    }
}
impl<'a> JsonView<'a> {
    pub(crate) fn accepts_value(self) -> Result<bool> {
        match self.0 {
            View::Owned(_) => Ok(true),
            #[cfg(feature = "mmap")]
            View::Source(source) => Ok(document::accepts_value(source.raw)),
            #[cfg(not(feature = "mmap"))]
            View::Portable(source) => source.source.accepts_value(source.value),
        }
    }
    pub(crate) fn select_fields(self, keys: &[&str]) -> Result<Option<Vec<Option<Self>>>> {
        Ok(match self.0 {
            View::Owned(Value::Object(object)) => Some(
                keys.iter()
                    .map(|key| object.get(*key).map(|value| Self(View::Owned(value))))
                    .collect(),
            ),
            View::Owned(_) => None,
            #[cfg(feature = "mmap")]
            View::Source(source) => document::select_fields(source.raw, keys).map(|fields| {
                fields
                    .into_iter()
                    .map(|raw| raw.map(|raw| Self(View::Source(SourceView { raw, ..source }))))
                    .collect()
            }),
            #[cfg(not(feature = "mmap"))]
            View::Portable(source) => source.source.select_fields(source.value, keys)?.map(|fields| {
                fields
                    .into_iter()
                    .map(|value| value.map(|value| Self(View::Portable(PortableView { value, ..source }))))
                    .collect()
            }),
        })
    }
    pub(crate) fn source_range(self) -> Result<Option<(u64, u64)>> {
        Ok(match self.0 {
            View::Owned(_) => None,
            #[cfg(feature = "mmap")]
            View::Source(source) => {
                let delta = (source.raw.get().as_ptr() as usize)
                    .checked_sub(source.base)
                    .context("JSON child outside source")? as u64;
                Some((
                    source.range.offset.checked_add(delta).context("JSON offset overflow")?,
                    source.raw.get().len() as u64,
                ))
            }
            #[cfg(not(feature = "mmap"))]
            View::Portable(source) => Some((source.value.offset, source.value.len)),
        })
    }
    /// Native transcript text uses the session's primary source, just like existing Text spans.
    pub(crate) fn text(self, spans: bool) -> Result<Option<crate::lazy::Text>> {
        match self.0 {
            View::Owned(value) => Ok(value.as_str().map(|s| crate::lazy::Text::Inline(s.to_owned()))),
            #[cfg(feature = "mmap")]
            View::Source(source) => {
                let raw = source.raw.get().as_bytes();
                if raw.first() != Some(&b'"') {
                    return Ok(None);
                }
                if spans && raw.len() - 2 > crate::lazy::INLINE_MAX {
                    let (offset, len) = self.source_range()?.context("JSON text source")?;
                    Ok(Some(crate::lazy::Text::Span(crate::lazy::Span {
                        source: None,
                        offset: offset + 1,
                        len: len - 2,
                        escaped: memchr::memchr(b'\\', &raw[1..raw.len() - 1]).is_some(),
                    })))
                } else {
                    Ok(Some(crate::lazy::Text::Inline(serde_json::from_slice(raw)?)))
                }
            }
            #[cfg(not(feature = "mmap"))]
            View::Portable(source) => source.source.text(source.value, spans),
        }
    }
    pub(crate) fn argument(self, spans: bool) -> Result<Json> {
        if spans
            && self
                .source_range()?
                .is_some_and(|(_, len)| len > crate::lazy::INLINE_MAX as u64)
        {
            self.to_json()
        } else {
            Ok(Json::Owned(self.resolve()?.into_owned()))
        }
    }
    /// A nonobject or missing field is None; backing-source failures occur before this view exists.
    pub fn get(self, key: &str) -> Result<Option<Self>> {
        Ok(match self.0 {
            View::Owned(value) => value.get(key).map(|value| Self(View::Owned(value))),
            #[cfg(not(feature = "mmap"))]
            View::Portable(source) => source
                .source
                .get(source.value, key)?
                .map(|value| Self(View::Portable(PortableView { value, ..source }))),
            #[cfg(feature = "mmap")]
            View::Source(source) => Record::parse(source.raw)
                .and_then(|record| record.get(key))
                .map(|raw| Self(View::Source(SourceView { raw, ..source }))),
        })
    }
    pub fn is_string(self) -> Result<bool> {
        Ok(match self.0 {
            View::Owned(value) => value.is_string(),
            #[cfg(not(feature = "mmap"))]
            View::Portable(source) => source.source.kind(source.value)? == Some(b'"'),
            #[cfg(feature = "mmap")]
            View::Source(source) => source.raw.get().starts_with('"'),
        })
    }
    pub fn is_object(self) -> Result<bool> {
        Ok(match self.0 {
            View::Owned(value) => value.is_object(),
            #[cfg(not(feature = "mmap"))]
            View::Portable(source) => source.source.kind(source.value)? == Some(b'{'),
            #[cfg(feature = "mmap")]
            View::Source(source) => source.raw.get().starts_with('{'),
        })
    }
    pub fn is_array(self) -> Result<bool> {
        Ok(match self.0 {
            View::Owned(value) => value.is_array(),
            #[cfg(not(feature = "mmap"))]
            View::Portable(source) => source.source.kind(source.value)? == Some(b'['),
            #[cfg(feature = "mmap")]
            View::Source(source) => source.raw.get().starts_with('['),
        })
    }
    pub fn is_null(self) -> Result<bool> {
        Ok(match self.0 {
            View::Owned(value) => value.is_null(),
            #[cfg(not(feature = "mmap"))]
            View::Portable(source) => source.source.kind(source.value)? == Some(b'n'),
            #[cfg(feature = "mmap")]
            View::Source(source) => source.raw.get() == "null",
        })
    }
    pub fn as_bool(self) -> Result<Option<bool>> {
        Ok(match self.0 {
            View::Owned(value) => value.as_bool(),
            #[cfg(not(feature = "mmap"))]
            View::Portable(source) => match source.source.kind(source.value)? {
                Some(b't') => Some(true),
                Some(b'f') => Some(false),
                _ => None,
            },
            #[cfg(feature = "mmap")]
            View::Source(source) => match source.raw.get() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
        })
    }
    pub fn as_number(self) -> Result<Option<Number>> {
        match self.0 {
            View::Owned(value) => Ok(value.as_number().cloned()),
            #[cfg(feature = "mmap")]
            View::Source(source) if matches!(source.raw.get().as_bytes().first(), Some(b'-' | b'0'..=b'9')) => {
                Ok(Some(serde_json::from_str(source.raw.get())?))
            }
            #[cfg(feature = "mmap")]
            View::Source(_) => Ok(None),
            #[cfg(not(feature = "mmap"))]
            View::Portable(source) => {
                if matches!(source.source.kind(source.value)?, Some(b'-' | b'0'..=b'9')) {
                    Ok(Some(source.source.materialize(source.value)?))
                } else {
                    Ok(None)
                }
            }
        }
    }
    /// Chronological array traversal holds one descriptor, not a vector of all children.
    pub fn array(self) -> Result<Option<JsonArray<'a>>> {
        Ok(match self.0 {
            View::Owned(value) => value.as_array().map(|array| JsonArray(Array::Owned(array.iter()))),
            #[cfg(not(feature = "mmap"))]
            View::Portable(source) => source
                .source
                .array(source.value)?
                .map(|array| JsonArray(Array::Portable { array, source })),
            #[cfg(feature = "mmap")]
            View::Source(source) => {
                document::array_iter(source.raw).map(|array| JsonArray(Array::Source { array, source }))
            }
        })
    }
    /// Visit normalized object entries in Value's insertion order. Structural key state grows
    /// with object width; descendant payloads are borrowed, not decoded into a tree.
    pub fn for_each_entry(self, mut f: impl FnMut(&str, Self) -> Result<()>) -> Result<bool> {
        match self.0 {
            View::Owned(Value::Object(object)) => {
                for (key, value) in object {
                    f(key, Self(View::Owned(value)))?;
                }
            }
            #[cfg(feature = "mmap")]
            View::Source(source) => {
                let Some(record) = Record::parse(source.raw) else {
                    return Ok(false);
                };
                for (key, value) in record.entries_in_order() {
                    f(&key, Self(View::Source(SourceView { raw: value, ..source })))?;
                }
            }
            #[cfg(not(feature = "mmap"))]
            View::Portable(source) => {
                let Some(entries) = source.source.object(source.value)? else {
                    return Ok(false);
                };
                let mut fields = std::collections::BTreeMap::<String, (usize, Range)>::new();
                for (i, entry) in entries.enumerate() {
                    let (key, value) = entry?;
                    let key: String = source.source.materialize(key)?;
                    fields
                        .entry(key)
                        .and_modify(|(_, old)| *old = value)
                        .or_insert((i, value));
                }
                let mut ordered = fields.iter().collect::<Vec<_>>();
                ordered.sort_by_key(|(_, (position, _))| *position);
                for (key, (_, value)) in ordered {
                    f(
                        key,
                        Self(View::Portable(PortableView {
                            value: *value,
                            ..source
                        })),
                    )?;
                }
            }
            View::Owned(_) => return Ok(false),
        }
        Ok(true)
    }
    /// Visit decoded string fragments with bounded source scratch. Returns false for a nonstring.
    /// Errors are explicit; callback effects must be discarded if this operation returns Err.
    pub fn for_each_str_chunk(self, mut f: impl FnMut(&str) -> Result<()>) -> Result<bool> {
        match self.0 {
            View::Owned(value) => match value.as_str() {
                Some(text) => {
                    f(text)?;
                    Ok(true)
                }
                None => Ok(false),
            },
            #[cfg(feature = "mmap")]
            View::Source(source) if source.raw.get().starts_with('"') => {
                decode_string(source.raw, &mut f)?;
                Ok(true)
            }
            #[cfg(feature = "mmap")]
            View::Source(_) => Ok(false),
            #[cfg(not(feature = "mmap"))]
            View::Portable(source) => {
                if source.source.kind(source.value)? != Some(b'"') {
                    return Ok(false);
                };
                source.source.for_each_string_chunk_until(source.value, &mut f)?;
                Ok(true)
            }
        }
    }
    /// Own a descriptor for this complete child. Source values copy only range metadata, while
    /// already owned values explicitly clone their selected data.
    pub fn to_json(self) -> Result<Json> {
        match self.0 {
            View::Owned(value) => Ok(Json::Owned(value.clone())),
            #[cfg(not(feature = "mmap"))]
            View::Portable(source) => Ok(Json::Source(Arc::new(
                source.owner.source.range(source.value.offset, source.value.len)?,
            ))),
            #[cfg(feature = "mmap")]
            View::Source(source) => {
                let delta = (source.raw.get().as_ptr() as usize)
                    .checked_sub(source.base)
                    .context("JSON child outside source")? as u64;
                let offset = source
                    .range
                    .offset
                    .checked_add(delta)
                    .context("JSON child offset overflow")?;
                // The containing scoped operation performs the final identity check; child
                // descriptors do not need another stat for each field within that operation.
                Ok(Json::Source(Arc::new(
                    source.range.source.range(offset, source.raw.get().len() as u64)?,
                )))
            }
        }
    }
    /// Materialize only this selected view. Source strings/containers may allocate their full data.
    pub fn resolve(self) -> Result<Cow<'a, Value>> {
        match self.0 {
            View::Owned(value) => Ok(Cow::Borrowed(value)),
            #[cfg(feature = "mmap")]
            View::Source(source) => Ok(Cow::Owned(serde_json::from_str(source.raw.get())?)),
            #[cfg(not(feature = "mmap"))]
            View::Portable(source) => Ok(Cow::Owned(source.source.materialize(source.value)?)),
        }
    }
    pub fn canonical_len(self) -> Result<u64> {
        let mut counter = CountWriter(0);
        self.write_canonical(&mut counter)?;
        Ok(counter.0)
    }
    /// Same bounded output and truncation contract as Json::canonical_prefix.
    pub fn canonical_prefix(self, max_bytes: usize) -> Result<(String, bool)> {
        let mut writer = PrefixWriter {
            bytes: Vec::new(),
            limit: max_bytes,
            stopped: false,
        };
        let result = self.write_canonical(&mut writer);
        if !writer.stopped {
            result?;
        }
        Ok((String::from_utf8(writer.bytes)?, writer.stopped))
    }
    /// Same partial-output-on-error contract as Json::write_canonical.
    pub fn write_canonical(self, writer: &mut impl Write) -> Result<()> {
        match self.0 {
            View::Owned(value) => serde_json::to_writer(writer, value).map_err(Into::into),
            #[cfg(feature = "mmap")]
            View::Source(source) => write_raw(source.raw, writer),
            #[cfg(not(feature = "mmap"))]
            View::Portable(_) => write_portable(self, writer),
        }
    }
}

enum Array<'a> {
    #[cfg(not(feature = "mmap"))]
    Portable {
        array: document::range::ArrayRanges<'a>,
        source: PortableView<'a>,
    },
    Owned(std::slice::Iter<'a, Value>),
    #[cfg(feature = "mmap")]
    Source {
        array: document::ArrayIter<'a>,
        source: SourceView<'a>,
    },
}
/// A scoped chronological array iterator. No source payloads or child roster are owned.
/// Items are fallible so file-backed cursor implementations can propagate read errors.
pub struct JsonArray<'a>(Array<'a>);
impl<'a> Iterator for JsonArray<'a> {
    type Item = Result<JsonView<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            Array::Owned(array) => array.next().map(|value| Ok(JsonView(View::Owned(value)))),
            #[cfg(not(feature = "mmap"))]
            Array::Portable { array, source } => array.next().map(|value| {
                value
                    .map(|value| JsonView(View::Portable(PortableView { value, ..*source })))
                    .map_err(Into::into)
            }),
            #[cfg(feature = "mmap")]
            Array::Source { array, source } => array
                .next()
                .map(|raw| Ok(JsonView(View::Source(SourceView { raw, ..*source })))),
        }
    }
}

struct PrefixWriter {
    bytes: Vec<u8>,
    limit: usize,
    stopped: bool,
}
impl Write for PrefixWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let text = std::str::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let remaining = self.limit - self.bytes.len();
        let mut end = bytes.len().min(remaining);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let needed = self.bytes.len() + end;
        if needed > self.bytes.capacity() {
            let capacity = self
                .bytes
                .capacity()
                .max(32)
                .saturating_mul(2)
                .min(self.limit)
                .max(needed);
            self.bytes.reserve_exact(capacity - self.bytes.len());
        }
        self.bytes.extend_from_slice(&bytes[..end]);
        if end != bytes.len() {
            self.stopped = true;
            return Err(std::io::Error::other("canonical JSON prefix complete"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct CountWriter(u64);
impl Write for CountWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("JSON length overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[cfg(feature = "mmap")]
fn decode_string(raw: Raw<'_>, f: &mut impl FnMut(&str) -> Result<()>) -> Result<()> {
    let body = &raw.get()[1..raw.get().len() - 1];
    // Validated unescaped text is borrowed directly. A callback can choose its own bounded scan.
    if !body.as_bytes().contains(&b'\\') {
        return f(body);
    }
    let mut error = None;
    let decoded = crate::lazy::decode_validated_json_body_until(&mut Cursor::new(body.as_bytes()), &mut |text| {
        error = f(text).err();
        error.is_none()
    });
    if let Some(error) = error {
        return Err(error);
    }
    anyhow::ensure!(decoded.is_ok(), "validated JSON string failed decoding");
    Ok(())
}
#[cfg(feature = "mmap")]
fn write_raw(raw: Raw<'_>, writer: &mut impl Write) -> Result<()> {
    match raw.get().as_bytes().first() {
        Some(b'{') => {
            let record = Record::parse(raw).context("invalid validated JSON object")?;
            writer.write_all(b"{")?;
            for (i, (key, value)) in record.entries_in_order().enumerate() {
                if i != 0 {
                    writer.write_all(b",")?;
                }
                serde_json::to_writer(&mut *writer, &key)?;
                writer.write_all(b":")?;
                write_raw(value, writer)?;
            }
            writer.write_all(b"}")?;
        }
        Some(b'[') => {
            writer.write_all(b"[")?;
            for (i, value) in document::array_iter(raw)
                .context("invalid validated JSON array")?
                .enumerate()
            {
                if i != 0 {
                    writer.write_all(b",")?;
                }
                write_raw(value, writer)?;
            }
            writer.write_all(b"]")?;
        }
        Some(b'"') => {
            if !raw.get().as_bytes().contains(&b'\\') {
                writer.write_all(raw.get().as_bytes())?;
            } else {
                writer.write_all(b"\"")?;
                let mut scratch = Vec::new();
                decode_string(raw, &mut |text| {
                    scratch.clear();
                    serde_json::to_writer(&mut scratch, text)?;
                    writer.write_all(&scratch[1..scratch.len() - 1])?;
                    Ok(())
                })?;
                writer.write_all(b"\"")?;
            }
        }
        Some(b'-' | b'0'..=b'9') => {
            let number: Number = serde_json::from_str(raw.get())?;
            serde_json::to_writer(writer, &number)?;
        }
        _ => writer.write_all(raw.get().as_bytes())?, // Validated null/bool.
    }
    Ok(())
}

#[cfg(not(feature = "mmap"))]
fn write_portable(value: JsonView<'_>, writer: &mut impl Write) -> Result<()> {
    let View::Portable(source) = value.0 else {
        unreachable!()
    };
    match source.source.kind(source.value)? {
        Some(b'{') => {
            writer.write_all(b"{")?;
            let mut first = true;
            value.for_each_entry(|key, child| {
                if !first {
                    writer.write_all(b",")?;
                }
                first = false;
                serde_json::to_writer(&mut *writer, key)?;
                writer.write_all(b":")?;
                child.write_canonical(writer)
            })?;
            writer.write_all(b"}")?;
        }
        Some(b'[') => {
            writer.write_all(b"[")?;
            for (i, child) in value.array()?.context("invalid validated JSON array")?.enumerate() {
                if i != 0 {
                    writer.write_all(b",")?;
                }
                child?.write_canonical(writer)?;
            }
            writer.write_all(b"]")?;
        }
        Some(b'"') => {
            writer.write_all(b"\"")?;
            let mut scratch = Vec::new();
            source.source.for_each_string_chunk_until(source.value, &mut |text| {
                scratch.clear();
                serde_json::to_writer(&mut scratch, text)?;
                writer.write_all(&scratch[1..scratch.len() - 1])?;
                Ok(())
            })?;
            writer.write_all(b"\"")?;
        }
        Some(b'-' | b'0'..=b'9') => {
            let number: Number = source.source.materialize(source.value)?;
            serde_json::to_writer(writer, &number)?;
        }
        Some(b't') => writer.write_all(b"true")?,
        Some(b'f') => writer.write_all(b"false")?,
        Some(b'n') => writer.write_all(b"null")?,
        _ => anyhow::bail!("invalid validated JSON scalar"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture {
        dir: PathBuf,
        path: PathBuf,
    }
    impl Fixture {
        fn new(bytes: &[u8]) -> Self {
            let dir = std::env::temp_dir().join(format!("cv-json-foundation-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            let path = dir.join("source.json");
            std::fs::write(&path, bytes).unwrap();
            Self { dir, path }
        }
        fn value(&self) -> Json {
            Json::source(&self.path, 0, std::fs::metadata(&self.path).unwrap().len()).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).unwrap();
        }
    }
    fn canonical(value: &Json) -> Vec<u8> {
        let mut output = Vec::new();
        value.write_canonical(&mut output).unwrap();
        assert_eq!(value.canonical_len().unwrap(), output.len() as u64);
        output
    }

    #[test]
    fn document_projection_preserves_escaped_keys_final_values_and_complete_arguments() {
        let body = "雪\"\\".repeat(4000);
        let bytes = format!(
            " \n{{\"ignored{}\":null,\"id\":\"old\",\"\\u0069d\":\"last\",\"body\":{},\"input\":{{\"payload\":{},\"n\":2}}}} \n",
            "x".repeat(20000), serde_json::to_string(&body).unwrap(), serde_json::to_string(&body).unwrap()
        );
        let fixture = Fixture::new(bytes.as_bytes());
        let source = JsonSource::capture(&fixture.path).unwrap();
        source
            .read_document(|view| {
                let fields = document::Fields::parse(view, &["id", "id", "body", "input"])?.unwrap();
                assert_eq!(fields.string("id")?.as_deref(), Some("last"));
                let text = fields.get("body").unwrap().text(true)?.unwrap();
                let crate::lazy::Text::Span(span) = text else {
                    panic!("large text must remain a span")
                };
                assert!(span.escaped);
                assert_eq!(
                    crate::lazy::Resolver::new(Some(fixture.path.clone())).resolve(&span),
                    body
                );
                let argument = fields.get("input").unwrap().argument(true)?;
                assert!(matches!(argument, Json::Source(_)));
                assert_eq!(
                    argument.resolve()?.get("payload").and_then(Value::as_str),
                    Some(body.as_str())
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn archive_whole_grammar_and_independent_value_budgets_match_raw_children() {
        let nested = format!("{}0{}", "[".repeat(126), "]".repeat(126));
        let fixture = Fixture::new(
            format!("[{{\"bad\":1e400}},{{\"bad\":\"\\ud800\"}},{{\"ok\":{nested}}},{{\"ok\":1}}]").as_bytes(),
        );
        let source = JsonSource::capture(&fixture.path).unwrap();
        assert!(source.read_document(|_| Ok(())).is_err());
        let accepted = source
            .read_archive(|array| {
                array
                    .unwrap()
                    .map(|value| value.map(|v| v.is_some()))
                    .collect::<Result<Vec<_>>>()
            })
            .unwrap();
        assert_eq!(accepted, [false, false, true, true]);
        for bytes in [b"[{\"ok\":1},]".as_slice(), b"[{\"ok\":1},\"\xff\"]"] {
            let fixture = Fixture::new(bytes);
            let source = JsonSource::capture(&fixture.path).unwrap();
            let mut published = false;
            let error = source
                .read_archive(|_| {
                    published = true;
                    Ok(())
                })
                .unwrap_err();
            assert!(error.is::<InvalidJson>());
            assert!(!published);
        }
    }

    #[test]
    fn owned_wire_and_mutation_are_value_transparent() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Json>();
        let values = [
            Value::Null,
            json!(true),
            json!(-7),
            json!(1.25),
            json!(["x", null, {"a": 1}]),
            json!({"Source": {"path": "missing", "offset": 0, "len": 100}, "Owned": null}),
        ];
        for value in values {
            let expected = serde_json::to_vec(&value).unwrap();
            let mut wrapped = Json::from(value.clone());
            assert!(matches!(wrapped.resolve().unwrap(), Cow::Borrowed(_)));
            assert_eq!(serde_json::to_vec(&wrapped).unwrap(), expected);
            assert_eq!(canonical(&wrapped), expected);
            let decoded: Json = serde_json::from_slice(&expected).unwrap();
            assert!(matches!(decoded, Json::Owned(_)));
            assert_eq!(*decoded.resolve().unwrap(), value);
            *wrapped.make_owned().unwrap() = json!("mutated");
            assert_eq!(canonical(&wrapped), br#""mutated""#);
        }
    }

    #[test]
    fn source_writer_and_selected_serde_match_independent_value_oracle() {
        let mut cases: Vec<String> = [
            "null",
            "true",
            "false",
            "-0",
            "1.234e+10",
            "1e-400",
            "18446744073709551616",
            "-99999999999999999999999",
            r#""literal UTF8 é\n\r\t\b\f\/\\\"\u0000\u001f\ud83c\udf19""#,
            r#"{"z":0,"a":1,"z":{"b":2,"a":3,"b":4},"a":[true,null,"\u0041"],"":5}"#,
            r#"{"\u0061":1,"b":2,"a":3,"\u0062":4}"#,
            "[[],{},[1,2],\"\",{\"x\": [false, -0, 1e2]}]",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        for split in 0..=12 {
            cases.push(format!(
                "{{\"prefix\":\"{}\\ud83c\\udf19é\\n{}\",\"tail\":1}}",
                "x".repeat(65_536 - split),
                "\\\\".repeat(32_769)
            ));
        }
        // Large escaped controls exercise the serializer's worst-case per-window expansion.
        cases.push(format!("\"{}\"", "\\u0000\\t\\n".repeat(20_000)));
        for input in cases {
            let oracle: Value = serde_json::from_str(&input).unwrap();
            let expected = serde_json::to_vec(&oracle).unwrap();
            let fixture = Fixture::new(input.as_bytes());
            let value = fixture.value();
            assert_eq!(canonical(&value), expected);
            assert_eq!(serde_json::to_vec(&value).unwrap(), expected);
            assert_eq!(*value.resolve().unwrap(), oracle);
        }
    }

    #[test]
    fn source_acceptance_matches_value_including_overwritten_invalid_values() {
        let mut cases: Vec<Vec<u8>> = [
            "",
            " ",
            "[] trailing",
            "[1,]",
            "{",
            "00",
            "1e400",
            "-1e400",
            r#""\ud800""#,
            r#""\udc00""#,
            r#"{"a":"\ud800","a":0}"#,
            r#"{"a":1e400,"a":0}"#,
            r#""\ud800\u0041""#,
            r#""\q""#,
            "\"raw\ncontrol\"",
            " \n true \r ",
            r#""\\ud800""#,
        ]
        .into_iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
        cases.push(b"{\"unused\":\"\xff\"}".to_vec());
        cases.push(format!("\"{}\\ud800\"", "x".repeat(131_072)).into_bytes());
        cases.push("9".repeat(2000).into_bytes());
        for depth in 125..=130 {
            cases.push(format!("{}0{}", "[".repeat(depth), "]".repeat(depth)).into_bytes());
            cases.push(format!("{}0{}", "{\"a\":".repeat(depth), "}".repeat(depth)).into_bytes());
        }
        for input in cases {
            let expected = serde_json::from_slice::<Value>(&input).is_ok();
            let fixture = Fixture::new(&input);
            assert_eq!(
                Json::source(&fixture.path, 0, input.len() as u64).is_ok(),
                expected,
                "{input:?}"
            );
        }
    }

    #[test]
    fn scoped_projections_match_owned_and_preserve_duplicate_order() {
        let input = r#"{"z":{"n":1.5,"b":false},"a":[true,null,"\ud83c\udf19"],"z":{"n":2,"b":true},"s":"head\n尾"}"#;
        let fixture = Fixture::new(input.as_bytes());
        let owned = Json::from(serde_json::from_str::<Value>(input).unwrap());
        for value in [owned, fixture.value()] {
            let (keys, strings) = value
                .with_view(|view| {
                    assert!(view.get("missing")?.is_none());
                    let z = view.get("z")?.unwrap();
                    assert_eq!(z.get("n")?.unwrap().as_number()?, Some(Number::from(2)));
                    assert_eq!(z.get("b")?.unwrap().as_bool()?, Some(true));
                    assert!(z.get("n")?.unwrap().get("anything")?.is_none());
                    let array = view.get("a")?.unwrap().array()?.unwrap().collect::<Result<Vec<_>>>()?;
                    assert_eq!(array.len(), 3);
                    assert_eq!(array[0].as_bool()?, Some(true));
                    assert!(array[1].is_null()?);
                    assert!(view.is_object()?);
                    assert!(view.get("a")?.unwrap().is_array()?);
                    assert!(array[2].is_string()?);
                    assert!(!array[1].is_string()?);
                    assert!(!array[1].for_each_str_chunk(|_| panic!("nonstring callback"))?);
                    let mut keys = Vec::new();
                    view.for_each_entry(|key, _| {
                        keys.push(key.to_string());
                        Ok(())
                    })?;
                    let mut strings = String::new();
                    array[2].for_each_str_chunk(|part| {
                        strings.push_str(part);
                        Ok(())
                    })?;
                    view.get("s")?.unwrap().for_each_str_chunk(|part| {
                        strings.push_str(part);
                        Ok(())
                    })?;
                    Ok((keys, strings))
                })
                .unwrap();
            assert_eq!(keys, ["z", "a", "s"]);
            assert_eq!(strings, "🌙head\n尾");
        }
    }

    #[test]
    fn exact_range_bounds_and_read_errors_never_become_null_or_missing() {
        let body = b" {\"value\":\"range\"} \n";
        // Keep non-JSON prefix/suffix outside the selected range.
        let fixture = Fixture::new(&[b"junk".as_slice(), body.as_slice(), b"trailing".as_slice()].concat());
        let mut value = Json::source(&fixture.path, 4, body.len() as u64).unwrap();
        assert_eq!(canonical(&value), br#"{"value":"range"}"#);
        assert!(Json::source(&fixture.path, 0, 4).is_err());
        assert!(Json::source(&fixture.path, u64::MAX, 2).is_err());
        assert!(Json::source(&fixture.path, 0, u64::MAX).is_err());
        assert!(Json::source(&fixture.path, 1000, 1).is_err());
        assert!(Json::source(fixture.dir.join("absent"), 0, 1).is_err());
        std::fs::remove_file(&fixture.path).unwrap();
        assert!(value.resolve().is_err());
        assert!(value.with_view(|view| Ok(view.get("absent")?.is_none())).is_err());
        assert!(value.canonical_len().is_err());
        assert!(serde_json::to_vec(&value).is_err());
        assert!(value.make_owned().is_err());
        assert!(matches!(value, Json::Source(_)));
    }

    #[test]
    fn source_identity_rejects_append_replacement_and_mutation_during_projection() {
        let fixture = Fixture::new(br#"{"a":1}"#);
        let value = fixture.value();
        std::fs::write(&fixture.path, br#"{"a":1} "#).unwrap();
        assert!(value.canonical_len().is_err());
        let value = fixture.value();
        value
            .with_view(|view| {
                assert_eq!(view.get("a")?.unwrap().as_number()?, Some(Number::from(1)));
                // Preserve length so mapped bytes remain valid; a deliberate mtime makes the check
                // deterministic on filesystems with coarse timestamp granularity.
                std::fs::write(&fixture.path, br#"{"a":2} "#)?;
                std::fs::File::open(&fixture.path)?.set_modified(SystemTime::UNIX_EPOCH)?;
                Ok(())
            })
            .expect_err("post-operation source change must reject its output");
        let value = fixture.value();
        let replacement = fixture.dir.join("replacement");
        std::fs::write(&replacement, br#"{"a":3} "#).unwrap();
        std::fs::File::open(&replacement)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        std::fs::rename(replacement, &fixture.path).unwrap();
        #[cfg(unix)]
        assert!(value.canonical_len().is_err()); // Same size/mtime; inode distinguishes replacement.
        drop(value);
        let mut value = fixture.value();
        let expected = value.resolve().unwrap().into_owned();
        assert_eq!(*value.make_owned().unwrap(), expected);
        std::fs::remove_file(&fixture.path).unwrap();
        assert_eq!(*value.resolve().unwrap(), expected); // Explicit ownership survives source loss.
    }

    #[test]
    fn selected_children_share_validated_identity_and_borrowed_owned_views() {
        let input = br#" {"ignored":"large","wrapper":{"n":7,"list":[{"a":"first"},{"a":"last"}]}} "#;
        let fixture = Fixture::new(input);
        let snapshot = JsonSource::capture(&fixture.path).unwrap();
        let index = document::Index::default();
        let raw: &crate::lazy::RawValue = serde_json::from_slice(input).unwrap();
        assert!(document::accepts_value(Raw::new(raw.get(), &index)));
        let value = snapshot.validated_range(1, input.len() as u64 - 2).unwrap();
        let wrapper = value.select("wrapper").unwrap().unwrap();
        let child = wrapper
            .with_view(|view| view.get("list")?.unwrap().array()?.unwrap().nth(1).unwrap()?.to_json())
            .unwrap();
        assert_eq!(canonical(&child), br#"{"a":"last"}"#);
        assert_eq!(canonical(&wrapper.select("n").unwrap().unwrap()), b"7");
        assert!(wrapper.select("missing").unwrap().is_none());
        assert!(matches!(&child, Json::Source(range) if range.path() == fixture.path && range.offset() > 1));
        let public_value = fixture.value();
        assert_eq!(
            canonical(&public_value.select("wrapper").unwrap().unwrap()),
            canonical(&wrapper)
        );
        let owned: Value = serde_json::from_slice(input).unwrap();
        assert!(matches!(JsonView::from(&owned).resolve().unwrap(), Cow::Borrowed(_)));
        assert_eq!(
            canonical(
                &JsonView::from(&owned)
                    .get("wrapper")
                    .unwrap()
                    .unwrap()
                    .to_json()
                    .unwrap()
            ),
            canonical(&wrapper)
        );
        std::fs::File::open(&fixture.path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        assert!(child.resolve().is_err());
        assert!(snapshot.validated_range(1, input.len() as u64 - 2).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn identity_rejects_restored_mtime_and_symlink_retargeting() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new(br#"{"a":1}"#);
        let original_time = std::fs::metadata(&fixture.path).unwrap().modified().unwrap();
        let value = fixture.value();
        std::fs::write(&fixture.path, br#"{"a":2}"#).unwrap();
        std::fs::File::open(&fixture.path)
            .unwrap()
            .set_modified(original_time)
            .unwrap();
        assert!(value.resolve().is_err()); // Unix change time captures the restored mtime edit.
        let other = fixture.dir.join("other");
        std::fs::write(&other, br#"{"a":3}"#).unwrap();
        let link = fixture.dir.join("link");
        symlink(&fixture.path, &link).unwrap();
        let value = Json::source(&link, 0, 7).unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink(&other, &link).unwrap();
        assert!(value.resolve().is_err());
    }

    #[test]
    fn canonical_prefix_is_bounded_unicode_complete_and_reports_truncation() {
        let input = r#"{"z":"é🌙尾\n","a":[true,1.25],"z":"é🌙尾\n","last":"tail"}"#;
        let fixture = Fixture::new(input.as_bytes());
        let owned: Value = serde_json::from_str(input).unwrap();
        let expected = serde_json::to_string(&owned).unwrap();
        for value in [Json::from(owned), fixture.value()] {
            for cap in 0..=expected.len() + 4 {
                let (prefix, truncated) = value.canonical_prefix(cap).unwrap();
                let mut end = cap.min(expected.len());
                while !expected.is_char_boundary(end) {
                    end -= 1;
                }
                assert_eq!(prefix, expected[..end]);
                assert_eq!(truncated, cap < expected.len());
                assert!(prefix.capacity() <= cap);
            }
            let (prefix, truncated) = value.canonical_prefix(usize::MAX).unwrap();
            assert_eq!(prefix, expected);
            assert!(!truncated);
        }
        let value = fixture.value();
        std::fs::remove_file(&fixture.path).unwrap();
        assert!(value.canonical_prefix(0).is_err());
    }

    #[test]
    fn prefix_stop_cannot_mask_a_source_change() {
        let fixture = Fixture::new(br#"{"a":1}"#);
        let value = fixture.value();
        let error = value
            .with_view(|view| {
                let result = view.canonical_prefix(0)?;
                std::fs::File::open(&fixture.path)?.set_modified(SystemTime::UNIX_EPOCH)?;
                Ok(result)
            })
            .unwrap_err();
        assert!(error.to_string().contains("JSON source changed"));
        let value = fixture.value();
        let error = value
            .with_view::<()>(|_| {
                std::fs::File::open(&fixture.path)?
                    .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1))?;
                anyhow::bail!("intentional callback stop")
            })
            .unwrap_err();
        assert!(error.to_string().contains("JSON source changed"));
    }

    struct FailingWriter {
        remaining: usize,
        written: usize,
    }
    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Err(std::io::Error::other("injected writer failure"));
            }
            let n = bytes.len().min(self.remaining);
            self.remaining -= n;
            self.written += n;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn writer_and_string_reducer_errors_are_propagated() {
        let input = format!("{{\"a\":\"{}\"}}", "\\n".repeat(100_000));
        let fixture = Fixture::new(input.as_bytes());
        for value in [
            Json::from(serde_json::from_str::<Value>(&input).unwrap()),
            fixture.value(),
        ] {
            let mut writer = FailingWriter {
                remaining: 100,
                written: 0,
            };
            let error = value.write_canonical(&mut writer).unwrap_err();
            assert!(error.to_string().contains("injected writer failure"));
            assert_eq!(writer.written, 100);
            let mut calls = 0;
            let error = value
                .with_view(|view| {
                    view.get("a")?.unwrap().for_each_str_chunk(|_| {
                        calls += 1;
                        anyhow::bail!("injected reducer failure")
                    })
                })
                .unwrap_err();
            assert_eq!(calls, 1);
            assert!(error.to_string().contains("injected reducer failure"));
        }
    }
}
