//! CLI glue for the query calculus: parse `-q` strings and resolve the one external predicate
//! cv-core cannot (`text:`, which needs the tantivy index) into the [`TextSets`] the core's
//! [`SessionFacts`] resolver consults. Everything else — the events/forest/compaction predicates
//! and the two-phase ref filter — lives in `cv_core::query::facts`, so `cv -q` and `cv serve`'s
//! `/api/stats?q=` evaluate the same calculus. The reference itself is `cv schema`
//! (`cmd/schema.rs`).

use crate::util::usage;
use anyhow::Result;
use cv_core::query::{FieldId, SessionQuery};
use std::collections::HashSet;

pub(crate) use cv_core::query::{filter_refs, TextSets};

/// Parse an optional `-q` string into a query. A malformed query is a caller mistake (exit 2,
/// like a bad window or a removed flag). The core's messages point at the reference by its 0.10
/// name; the CLI seam re-points them at `cv schema`.
pub(crate) fn build(query: Option<String>) -> Result<Option<SessionQuery>> {
    match query {
        Some(q) => match SessionQuery::parse(&q) {
            Ok(parsed) => Ok(Some(parsed)),
            Err(e) => usage(e.replace("`cv query`", "`cv schema`")),
        },
        None => Ok(None),
    }
}

/// Resolve every indexed matching ID per distinct `text:` needle, without reading transcripts.
/// If there's no index, warn
/// once and treat every `text:` needle as matching nothing.
pub(crate) fn text_sets(query: &SessionQuery) -> TextSets {
    let needles = query.needles(FieldId::Text);
    if needles.is_empty() {
        return TextSets::empty();
    }
    if !cv_search::default_tantivy_dir().exists() {
        eprintln!("cv query: text: needs a full-text index — run `cv index` (treating text: as no match)");
        return TextSets::resolve(query, |_| HashSet::new());
    }
    let borrowed: Vec<_> = needles.iter().map(String::as_str).collect();
    let matches = cv_search::text_search_ids_batch(None, &borrowed).unwrap_or_default();
    let mut sets: std::collections::HashMap<_, _> = needles
        .into_iter()
        .zip(matches.into_iter().map(Result::unwrap_or_default))
        .collect();
    TextSets::resolve(query, |needle| sets.remove(needle).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_sets_batch_distinct_needles_without_losing_membership() {
        let world = cv_testkit::World::in_process("query_text_sets_batch");
        let refs: Vec<_> = [("first", "alpha"), ("second", "beta")]
            .into_iter()
            .map(|(id, content)| {
                let path = world.write_session(
                    id,
                    &[serde_json::json!({
                        "type":"user", "message":{"role":"user", "content":content}
                    })],
                );
                cv_core::SessionRef::new(id, cv_core::Harness::Claude, path)
            })
            .collect();
        cv_search::index_sessions(None, refs, true, false).unwrap();
        let query = SessionQuery::parse("text:alpha OR text:beta OR text:alpha").unwrap();
        let sets = text_sets(&query);
        assert!(sets.contains("alpha", "first"));
        assert!(!sets.contains("alpha", "second"));
        assert!(sets.contains("beta", "second"));
        assert!(!sets.contains("beta", "first"));
    }
}
