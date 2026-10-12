//! Operation-private source descriptors. A staged descriptor keeps the original accepted source
//! identity; restoration never captures or validates a newer file in place of that source.
use super::{Json, JsonSource};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize)]
pub(crate) struct SourceDescriptor {
    source: usize,
    offset: u64,
    len: u64,
}

/// Structural state scales with distinct captured source paths/identities, not record count.
/// Different captured identities of a single path are deliberately separate accepted sources.
#[derive(Default)]
pub(crate) struct SourceRegistry {
    ids: HashMap<JsonSource, usize>,
    sources: Vec<JsonSource>,
}
impl SourceRegistry {
    /// Replace a source value with an explicit transport placeholder without resolving its bytes.
    pub(crate) fn detach(&mut self, value: &mut Json) -> Option<SourceDescriptor> {
        let Json::Source(range) = value else { return None };
        let key = range.source.clone();
        let source = *self.ids.entry(key).or_insert_with(|| {
            let id = self.sources.len();
            self.sources.push(range.source.clone());
            id
        });
        let descriptor = SourceDescriptor {
            source,
            offset: range.offset,
            len: range.len,
        };
        *value = Json::Owned(serde_json::Value::Null);
        Some(descriptor)
    }

    /// Bounds-check against the original accepted source. Demanded reads later validate that
    /// identity normally; excluded records need not read or stat these unused payloads.
    pub(crate) fn restore(&self, descriptor: SourceDescriptor) -> Result<Json> {
        let source = self
            .sources
            .get(descriptor.source)
            .context("unknown staged JSON source")?;
        Ok(Json::Source(std::sync::Arc::new(
            source.range(descriptor.offset, descriptor.len)?,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_sources_share_original_identity_and_never_recapture() {
        let world = cv_testkit::World::in_process("json-source-transport");
        let path = world.base.join("source.json");
        std::fs::write(&path, r#"{"value":"old"}"#).unwrap();
        let mut first = Json::source(&path, 0, 15).unwrap();
        let mut second = Json::source(&path, 0, 15).unwrap();
        let mut registry = SourceRegistry::default();
        let descriptor = registry.detach(&mut first).unwrap();
        registry.detach(&mut second).unwrap();
        assert_eq!(registry.sources.len(), 1);
        std::fs::write(&path, r#"{"value":"new"}"#).unwrap();
        let restored = registry.restore(descriptor).unwrap();
        assert!(restored.resolve().is_err());
        assert!(matches!(first, Json::Owned(serde_json::Value::Null)));
    }
}
