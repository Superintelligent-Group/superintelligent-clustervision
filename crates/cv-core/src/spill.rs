//! Operation-owned, bounded temporary storage for ordered records and exact string membership.
mod exact;
mod ordered;
pub(crate) use exact::ExactStrings;
pub(crate) use ordered::{Ordered, Record, Rows};

use anyhow::{Context, Result};
use std::fs::{self, File};
use std::path::PathBuf;

/// Exclusively-created temporary file. On Unix it is unlinked immediately; other platforms
/// remove the owned path after the caller closes its file handles.
struct Temporary(PathBuf);
impl Temporary {
    fn create(label: &str) -> Result<(File, Self)> {
        let path = std::env::temp_dir().join(format!("cv-{label}-{}", uuid::Uuid::new_v4()));
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).context("creating private spill file")?;
        let owner = Self(path);
        #[cfg(unix)]
        fs::remove_file(&owner.0).context("unlinking private spill file")?;
        Ok((file, owner))
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        // Only the exclusive path this operation created, never a source or another pass's file.
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_file_permissions_and_owned_cleanup() {
        let (file, owner) = Temporary::create("ownership-test").unwrap();
        let path = owner.0.clone();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
            assert!(!path.exists());
        }
        drop(file);
        drop(owner);
        assert!(!path.exists());
    }
}
