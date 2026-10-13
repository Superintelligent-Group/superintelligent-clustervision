//! Evidence-only live stored-document stream and representative query oracle.
//! Pipe JSONL into a SHA256 reducer; this example never collects document bodies.
use anyhow::{Context, Result};
use std::io::{BufWriter, Write};
use tantivy::collector::DocSetCollector;
use tantivy::query::QueryParser;
use tantivy::schema::{document::Document, Value};
use tantivy::{Index, TantivyDocument};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().context("usage: fleet_snapshot INDEX [QUERY ...]")?;
    let index = Index::open_in_dir(path).context("opening fleet index")?;
    anyhow::ensure!(
        index.validate_checksum()?.is_empty(),
        "index contains corrupt managed files"
    );
    let schema = index.schema();
    let id = schema.get_field("id")?;
    let fields = ["title", "body", "cwd"]
        .into_iter()
        .map(|name| schema.get_field(name))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let searcher = index.reader()?.searcher();
    let mut out = BufWriter::new(std::io::stdout().lock());
    let mut count = 0u64;
    for segment in searcher.segment_readers() {
        let store = segment.get_store_reader(0)?;
        for doc_id in segment.doc_ids_alive() {
            let doc: TantivyDocument = store.get(doc_id).context("reading live stored document")?;
            let value: serde_json::Value = serde_json::from_str(&doc.to_json(&schema))?;
            serde_json::to_writer(&mut out, &serde_json::json!({"document": value}))?;
            out.write_all(b"\n")?;
            count += 1;
        }
    }
    let mut parser = QueryParser::for_index(&index, fields);
    parser.set_conjunction_by_default();
    for text in args {
        let query = parser.parse_query(&text).with_context(|| format!("query {text:?}"))?;
        let addresses = searcher.search(&query, &DocSetCollector)?;
        let mut ids = Vec::with_capacity(addresses.len());
        for address in addresses {
            let doc: TantivyDocument = searcher.doc(address)?;
            ids.push(
                doc.get_first(id)
                    .and_then(|v| v.as_str())
                    .context("matching document lacks id")?
                    .to_owned(),
            );
        }
        ids.sort();
        serde_json::to_writer(&mut out, &serde_json::json!({"query": text, "ids": ids}))?;
        out.write_all(b"\n")?;
    }
    serde_json::to_writer(
        &mut out,
        &serde_json::json!({"live_documents": count, "managed_file_checksums_valid": true}),
    )?;
    out.write_all(b"\n")?;
    out.flush().context("flushing snapshot")
}
