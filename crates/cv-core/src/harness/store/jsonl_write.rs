//! Bounded JSONL staging: serialize successfully before opening the destination.
use super::bytes::ByteStage;
use anyhow::{Context, Result};
use serde::Serialize;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

pub(crate) struct JsonlStage {
    output: BufWriter<ByteStage>,
    records: usize,
    failed: bool,
}
impl JsonlStage {
    pub(crate) fn new() -> Self {
        Self {
            output: BufWriter::with_capacity(64 * 1024, ByteStage::new()),
            records: 0,
            failed: false,
        }
    }
    pub(crate) fn len(&self) -> usize {
        self.records
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.records == 0
    }
    pub(crate) fn push(&mut self, value: impl Serialize) -> Result<()> {
        self.push_with(|writer| serde_json::to_writer(writer, &value).map_err(Into::into))
    }
    pub(crate) fn push_with(&mut self, produce: impl FnOnce(&mut dyn Write) -> Result<()>) -> Result<()> {
        anyhow::ensure!(!self.failed, "JSONL stage already failed");
        self.failed = true;
        produce(&mut self.output)?;
        self.output.write_all(b"\n")?;
        self.records += 1;
        self.failed = false;
        Ok(())
    }
    pub(crate) fn publish(self, path: &Path) -> Result<()> {
        anyhow::ensure!(!self.failed, "cannot publish a failed JSONL stage");
        let stage = self.output.into_inner().map_err(|e| e.into_error())?;
        publish_stage(stage, path)
    }
}

/// Publish fully produced bytes with fs::write-compatible permissions/link/I/O behavior.
/// Checked serialization must complete before calling; I/O errors may leave partial output.
pub(crate) fn publish_stage(stage: ByteStage, path: &Path) -> Result<()> {
    let source = stage.finish()?;
    let mut reader = BufReader::with_capacity(256 * 1024, source.reader()?);
    // Like fs::write, preserve existing inode permissions, links and symlink following.
    // Serialization errors occur before open; write errors retain fs::write's semantics.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    std::io::copy(&mut reader, &mut file).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_spilled_jsonl_and_failed_publication() -> Result<()> {
        let dir = cv_testkit::World::new("jsonl-stage-spill");
        let path = dir.base.join("records.jsonl");
        let value = serde_json::json!({"escaped": "a\"\\\nλ".repeat(300_000), "n": 1.25});
        let mut stage = JsonlStage::new();
        stage.push(&value)?;
        stage.push(serde_json::Value::Null)?;
        stage.publish(&path)?;
        assert_eq!(
            std::fs::read(&path)?,
            format!("{}\nnull\n", serde_json::to_string(&value)?).as_bytes()
        );
        struct Failure;
        impl Serialize for Failure {
            fn serialize<S: serde::Serializer>(&self, _: S) -> std::result::Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("synthetic failure"))
            }
        }
        let before = std::fs::read(&path)?;
        let mut stage = JsonlStage::new();
        stage.push(serde_json::json!({"valid": true}))?;
        assert!(stage.push(Failure).is_err());
        assert!(stage.publish(&path).is_err());
        assert_eq!(std::fs::read(&path)?, before);
        let mut stage = JsonlStage::new();
        assert!(stage
            .push_with(|writer| {
                writer.write_all(b"{\"partial\":")?;
                anyhow::bail!("checked payload source failed")
            })
            .is_err());
        assert!(stage.publish(&path).is_err());
        assert_eq!(std::fs::read(&path)?, before);
        Ok(())
    }
    #[cfg(unix)]
    #[test]
    fn preserves_destination_links_and_permissions() -> Result<()> {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = cv_testkit::World::new("jsonl-stage-links");
        let path = dir.base.join("original");
        let hard = dir.base.join("hard");
        let sym = dir.base.join("sym");
        std::fs::write(&path, "before")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))?;
        std::fs::hard_link(&path, &hard)?;
        symlink(&path, &sym)?;
        let mut stage = JsonlStage::new();
        stage.push(42)?;
        stage.publish(&sym)?;
        assert_eq!(std::fs::read(&hard)?, b"42\n");
        assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o640);
        assert!(std::fs::symlink_metadata(&sym)?.file_type().is_symlink());
        Ok(())
    }
}
