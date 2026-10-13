//! Store helpers shared by every adapter: the on-disk plumbing that is the same whichever harness
//! wrote the files. An adapter imports these and keeps only what is genuinely its own — where its
//! files are and what its records mean.

use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub(crate) mod bytes;
pub(crate) mod document;
pub(crate) mod json;
pub(crate) mod jsonl;
mod jsonl_write;
pub(crate) use jsonl_write::JsonlStage;
pub(crate) mod text;

#[cfg(feature = "sqlite")]
pub mod sqlite;

/// Validate discarded JSON with the same scalar, UTF-16 and depth checks as `Value`, without
/// retaining its tree. `IgnoredAny` skips some of those checks. Escaped strings still use serde's
/// scratch buffer; ordinary strings borrow their source. Shared by metadata-only store scans.
pub(crate) struct DiscardValue;

impl<'de> serde::Deserialize<'de> for DiscardValue {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> std::result::Result<Self, D::Error> {
        struct Discard;
        impl<'de> serde::de::Visitor<'de> for Discard {
            type Value = DiscardValue;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(DiscardValue)
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(DiscardValue)
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(DiscardValue)
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(DiscardValue)
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(DiscardValue)
            }
            fn visit_str<E: serde::de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(DiscardValue)
            }
            fn visit_seq<S: serde::de::SeqAccess<'de>>(self, mut seq: S) -> std::result::Result<Self::Value, S::Error> {
                while seq.next_element::<DiscardValue>()?.is_some() {}
                Ok(DiscardValue)
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> std::result::Result<Self::Value, M::Error> {
                while map.next_key::<DiscardValue>()?.is_some() {
                    map.next_value::<DiscardValue>()?;
                }
                Ok(DiscardValue)
            }
        }
        de.deserialize_any(Discard)
    }
}

/// `$XDG_DATA_HOME` when set to an absolute path (the spec says a relative value is ignored), else
/// `~/.local/share`. The base of every XDG-shaped store (opencode, devin, goose, zed); `None` only
/// when there is no home directory at all.
pub(crate) fn xdg_data_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
}

/// Write `lines` as JSONL (one compact JSON value per line) to `path`, replacing the file.
pub(crate) fn write_jsonl(path: &Path, lines: &[Value]) -> Result<()> {
    let mut stage = JsonlStage::new();
    for value in lines {
        stage.push(value)?;
    }
    stage.publish(path)
}
