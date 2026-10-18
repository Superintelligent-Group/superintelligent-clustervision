//! Lossless native-record derivation: selectors choose checked source ranges and replacement
//! nodes; one writer applies identity/standalone changes and preserves every untouched byte.
use crate::harness::store::jsonl::ranges::{self, Range};
use crate::json::{Json, JsonSource};
use anyhow::Result;
use serde_json::Value;
use std::io::Write;

pub(crate) struct Select {
    pub range: Range,
    pub newline: bool,
    pub record: Option<Json>,
    pub ids: Vec<(u64, u64)>,
    pub replacements: Vec<(Json, Value)>,
    pub root: bool,
}
impl Select {
    pub fn read(source: &JsonSource, range: Range, newline: bool, syntax: bool) -> Result<Self> {
        let read = |view: crate::json::JsonView<'_>| Ok((view.to_json()?, crate::restamp::source_id_spans(view)?));
        let projected = if syntax {
            source.read_range_syntax(range.offset, range.len, read)
        } else {
            source.read_range_value(range.offset, range.len, read)
        };
        let (record, ids) = match projected {
            Ok((record, ids)) => (Some(record), ids),
            Err(error) if error.is::<crate::json::InvalidJson>() => (None, Vec::new()),
            Err(error) => return Err(error),
        };
        Ok(Self {
            range,
            newline,
            record,
            ids,
            replacements: Vec::new(),
            root: false,
        })
    }
}

pub(crate) struct Rewrite<'a> {
    pub session_id: &'a str,
    pub standalone: bool,
}
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub lines: usize,
    pub stamped: usize,
    pub bytes: usize,
}
struct Counted<'a, W> {
    output: &'a mut W,
    bytes: usize,
}
impl<W: Write> Write for Counted<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let count = self.output.write(bytes)?;
        self.bytes = self
            .bytes
            .checked_add(count)
            .ok_or_else(|| std::io::Error::other("derived size overflow"))?;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

/// Derive selected records. An empty selection is a no-op; selectors may stream one record at
/// a time when they fold emitted-byte evidence or decide the next range from prior results.
pub(crate) fn derive(
    source: &JsonSource,
    file: &std::fs::File,
    selected: impl IntoIterator<Item = Result<Select>>,
    rewrite: &Rewrite<'_>,
    output: &mut impl Write,
) -> Result<Counts> {
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    let mut output = Counted { output, bytes: 0 };
    let mut counts = Counts::default();
    for selection in selected {
        let mut selection = selection?;
        counts.lines += usize::from(ranges::trim(file, selection.range)?.len != 0);
        let payload_rewrite = !selection.replacements.is_empty();
        let mut normalize = false;
        if let Some(record) = selection
            .record
            .as_ref()
            .filter(|_| rewrite.standalone || selection.root)
        {
            record.with_view(|view| {
                normalize = rewrite.standalone
                    && (view.get("agentId")?.is_some()
                        || view.get("isSidechain")?.map(|v| v.as_bool()).transpose()?.flatten() == Some(true));
                if selection.root {
                    if let Some(parent) = view.get("parentUuid")? {
                        if !parent.is_null()? {
                            selection.replacements.push((parent.to_json()?, Value::Null));
                        }
                    }
                }
                Ok(())
            })?;
        }
        if normalize || !selection.replacements.is_empty() {
            let record = selection
                .record
                .as_ref()
                .expect("replacements require a checked record");
            let stamped = record.with_view(|view| {
                if normalize || payload_rewrite {
                    write_modified(
                        view,
                        &selection.replacements,
                        rewrite.session_id,
                        rewrite.standalone,
                        &mut output,
                    )
                } else {
                    // A pure reroot keeps the producer's accepted id roster (duplicate known fields reject restamp).
                    for &(start, end) in &selection.ids {
                        selection.replacements.push((
                            source.validated_range(start, end - start)?,
                            Value::String(rewrite.session_id.into()),
                        ));
                    }
                    view.write_canonical_replacing(&selection.replacements, &mut output)?;
                    Ok(!selection.ids.is_empty())
                }
            })?;
            counts.stamped += usize::from(stamped);
        } else {
            counts.stamped += usize::from(crate::restamp::write_restamped_spans(
                source,
                file,
                selection.range.offset,
                selection.range.len,
                rewrite.session_id,
                &mut selection.ids,
                &mut output,
            )?);
        }
        if selection.newline {
            output.write_all(b"\n")?;
        }
    }
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    counts.bytes = output.bytes;
    Ok(counts)
}

pub(crate) fn write_modified(
    view: crate::json::JsonView<'_>,
    replacements: &[(Json, Value)],
    new_id: &str,
    standalone: bool,
    writer: &mut impl Write,
) -> Result<bool> {
    if !view.is_object()? {
        view.write_canonical_replacing(replacements, writer)?;
        return Ok(false);
    }
    writer.write_all(b"{")?;
    let mut first = true;
    let mut stamped = false;
    view.for_each_entry(|key, child| {
        if standalone && key == "agentId" {
            return Ok(());
        }
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        serde_json::to_writer(&mut *writer, key)?;
        writer.write_all(b":")?;
        if standalone && key == "isSidechain" && child.as_bool()? == Some(true) {
            writer.write_all(b"false")?;
        } else if matches!(key, "sessionId" | "session_id") && child.is_string()? {
            serde_json::to_writer(&mut *writer, new_id)?;
            stamped = true;
        } else {
            child.write_canonical_replacing(replacements, writer)?;
        }
        Ok(())
    })?;
    writer.write_all(b"}")?;
    Ok(stamped)
}

/// Adoption selects the complete transcript; metadata publication remains the caller's job.
pub(crate) fn transcript(
    source: &JsonSource,
    file: &std::fs::File,
    new_id: &str,
    output: &mut impl Write,
) -> Result<Counts> {
    let selected = ranges::records(
        file,
        Range {
            offset: 0,
            len: source.len(),
        },
    )?
    .map(|row| {
        let (range, newline) = row?;
        if !ranges::valid_utf8(&mut ranges::reader(file, range)?)? {
            return Err(
                std::io::Error::new(std::io::ErrorKind::InvalidData, "stream did not contain valid UTF-8").into(),
            );
        }
        Select::read(source, range, newline, true)
    });
    derive(
        source,
        file,
        selected,
        &Rewrite {
            session_id: new_id,
            standalone: false,
        },
        output,
    )
}
