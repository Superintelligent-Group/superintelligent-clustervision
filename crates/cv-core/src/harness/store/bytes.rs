//! Immutable operation-owned bytes, bounded in memory and spilled privately when necessary.
pub(crate) const MEMORY_LIMIT: usize = 1024 * 1024;
#[derive(Debug)]
pub(crate) struct Spill {
    file: Option<std::fs::File>,
    path: std::path::PathBuf,
}
impl Spill {
    fn create(unlink: bool) -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!("cv-byte-source-{}", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let spill = Self {
            file: Some(options.open(&path)?),
            path,
        };
        #[cfg(unix)]
        if unlink {
            std::fs::remove_file(&spill.path)?;
        }
        #[cfg(not(unix))]
        let _ = unlink;
        Ok(spill)
    }
}
impl Drop for Spill {
    fn drop(&mut self) {
        self.file.take();
        let _ = std::fs::remove_file(&self.path);
    }
}
#[derive(Debug)]
pub struct ByteSource {
    bytes: Bytes,
}
#[derive(Debug)]
enum Bytes {
    Memory(Vec<u8>),
    File(Spill),
    Closed {
        spill: Spill,
        captured: crate::json::JsonSource,
    },
}

/// Writable until finish, then immutable. Partial/failed operations own and remove their spill.
#[derive(Default)]
pub struct ByteStage {
    memory: Vec<u8>,
    spill: Option<Spill>,
}
impl std::io::Write for ByteStage {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.spill.is_none() && self.memory.len().saturating_add(bytes.len()) > MEMORY_LIMIT {
            self.spill = Some(Spill::create(true)?);
            self.spill
                .as_mut()
                .unwrap()
                .file
                .as_mut()
                .unwrap()
                .write_all(&self.memory)?;
            self.memory = Vec::new();
        }
        if let Some(spill) = &mut self.spill {
            spill.file.as_mut().unwrap().write_all(bytes)?;
        } else {
            // Vec's geometric growth must not overshoot the memory budget for irregular writes.
            let required = self.memory.len() + bytes.len();
            if required > self.memory.capacity() {
                let capacity = self
                    .memory
                    .capacity()
                    .max(64 * 1024)
                    .saturating_mul(2)
                    .min(MEMORY_LIMIT)
                    .max(required);
                self.memory.reserve_exact(capacity - self.memory.len());
            }
            self.memory.extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(spill) = &mut self.spill {
            spill.file.as_mut().unwrap().flush()?;
        }
        Ok(())
    }
}
impl ByteStage {
    pub fn new() -> Self {
        Self::default()
    }
    pub(crate) fn disk() -> std::io::Result<Self> {
        Ok(Self {
            memory: Vec::new(),
            spill: Some(Spill::create(false)?),
        })
    }
    /// Retain private backing by path so many decoded documents do not retain one FD each.
    pub(crate) fn finish_closed(mut self) -> std::io::Result<ByteSource> {
        use std::io::Write;
        self.flush()?;
        let mut spill = self.spill.take().expect("disk stage");
        let captured = crate::json::JsonSource::capture(&spill.path).map_err(std::io::Error::other)?;
        spill.file.take();
        Ok(ByteSource {
            bytes: Bytes::Closed { spill, captured },
        })
    }
    pub fn finish(mut self) -> std::io::Result<ByteSource> {
        use std::io::Write;
        self.flush()?;
        Ok(ByteSource {
            bytes: match self.spill.take() {
                Some(spill) => Bytes::File(spill),
                None => Bytes::Memory(std::mem::take(&mut self.memory)),
            },
        })
    }
}
impl ByteSource {
    pub(crate) fn len(&self) -> std::io::Result<u64> {
        match &self.bytes {
            Bytes::Memory(bytes) => Ok(bytes.len() as u64),
            Bytes::File(spill) => Ok(spill.file.as_ref().unwrap().metadata()?.len()),
            Bytes::Closed { captured, .. } => Ok(captured.len()),
        }
    }

    pub(crate) fn check(&self) -> anyhow::Result<()> {
        if let Bytes::Closed { captured, .. } = &self.bytes {
            captured.check()?;
        }
        Ok(())
    }

    pub(crate) fn file(&self) -> std::io::Result<std::fs::File> {
        match &self.bytes {
            Bytes::File(spill) => spill.file.as_ref().unwrap().try_clone(),
            Bytes::Closed { spill, captured } => {
                captured.check().map_err(std::io::Error::other)?;
                let file = std::fs::File::open(&spill.path)?;
                captured
                    .check_metadata(file.metadata()?)
                    .map_err(std::io::Error::other)?;
                Ok(file)
            }
            Bytes::Memory(_) => Err(std::io::Error::other("expected disk-backed bytes")),
        }
    }

    pub(crate) fn read(mut input: impl std::io::Read) -> std::io::Result<Self> {
        use std::io::Write;
        let mut stage = ByteStage::new();
        let mut chunk = [0; 64 * 1024];
        loop {
            let n = input.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            stage.write_all(&chunk[..n])?;
        }
        stage.finish()
    }
    /// Prefer native contiguous parsing. Mapping never outlives this exclusively owned source.
    pub(crate) fn with_bytes<T>(&self, f: impl FnOnce(&[u8]) -> T) -> std::io::Result<Option<T>> {
        match &self.bytes {
            Bytes::Memory(bytes) => Ok(Some(f(bytes))),
            Bytes::Closed { .. } => Ok(None),
            Bytes::File(spill) => {
                #[cfg(feature = "mmap")]
                {
                    let file = spill.file.as_ref().unwrap();
                    if file.metadata()?.len() == 0 {
                        return Ok(Some(f(&[])));
                    }
                    // Only this operation owns the backing descriptor; it is never mutated.
                    if let Ok(map) = unsafe { memmap2::Mmap::map(file) } {
                        return Ok(Some(f(&map)));
                    }
                }
                #[cfg(not(feature = "mmap"))]
                let _ = (spill, f);
                Ok(None)
            }
        }
    }
    /// Each bounded reader has an independent position, including on unlinked private spills.
    pub fn reader(&self) -> std::io::Result<Box<dyn std::io::Read + '_>> {
        match &self.bytes {
            Bytes::Memory(bytes) => Ok(Box::new(std::io::Cursor::new(bytes))),
            Bytes::Closed { .. } => Ok(Box::new(std::io::BufReader::new(self.file()?))),
            Bytes::File(spill) => Ok(Box::new(std::io::BufReader::new(FileReader {
                file: spill.file.as_ref().unwrap(),
                position: 0,
            }))),
        }
    }
    /// Publish only after the owning operation has validated its successful completion.
    #[allow(dead_code)] // Publication consumers land separately from SQLite byte-source extraction.
    pub fn copy_to(&self, out: &mut (impl std::io::Write + ?Sized)) -> std::io::Result<u64> {
        match &self.bytes {
            Bytes::Memory(bytes) => {
                out.write_all(bytes)?;
                Ok(bytes.len() as u64)
            }
            Bytes::File(_) | Bytes::Closed { .. } => std::io::copy(&mut self.reader()?, out),
        }
    }
}
struct FileReader<'a> {
    file: &'a std::fs::File,
    position: u64,
}
impl std::io::Read for FileReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(unix)]
        let n = {
            use std::os::unix::fs::FileExt;
            self.file.read_at(bytes, self.position)?
        };
        #[cfg(windows)]
        let n = {
            use std::os::windows::fs::FileExt;
            self.file.seek_read(bytes, self.position)?
        };
        #[cfg(not(any(unix, windows)))]
        let n = {
            use std::io::{Read, Seek, SeekFrom};
            let mut file = self.file.try_clone()?;
            file.seek(SeekFrom::Start(self.position))?;
            file.read(bytes)?
        };
        self.position += n as u64;
        Ok(n)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closed_backing_keeps_private_bytes_without_retaining_a_descriptor() {
        use std::io::{Read, Write};
        let mut stage = ByteStage::disk().unwrap();
        stage.write_all(b"owned bytes").unwrap();
        let source = stage.finish_closed().unwrap();
        let Bytes::Closed { spill, .. } = &source.bytes else {
            panic!("expected closed backing")
        };
        assert!(spill.file.is_none());
        let path = spill.path.clone();
        let mut output = String::new();
        source.reader().unwrap().read_to_string(&mut output).unwrap();
        assert_eq!(output, "owned bytes");
        std::fs::write(&path, b"changed").unwrap();
        assert!(source.reader().is_err());
        drop(source);
        assert!(!path.exists());
    }

    #[test]
    fn closed_backing_rejects_replacement_and_partial_stages_remove_their_files() {
        use std::io::Write;
        let mut stage = ByteStage::disk().unwrap();
        stage.write_all(b"owned bytes").unwrap();
        let source = stage.finish_closed().unwrap();
        let Bytes::Closed { spill, .. } = &source.bytes else {
            unreachable!()
        };
        let path = spill.path.clone();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let replacement = path.with_extension("replacement");
        std::fs::write(&replacement, b"other bytes").unwrap();
        std::fs::File::open(&replacement)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        #[cfg(unix)]
        assert!(source.file().is_err());
        drop(source);
        assert!(!path.exists());
        let mut stage = ByteStage::disk().unwrap();
        let path = stage.spill.as_ref().unwrap().path.clone();
        stage.write_all(b"partial bytes").unwrap();
        drop(stage); // Producer or writer errors drop incomplete output through this same owner.
        assert!(!path.exists());
    }

    #[test]
    fn writer_capacity_and_independent_readers_preserve_bytes() {
        use std::io::{Read, Write};
        let mut stage = ByteStage::new();
        stage.write_all(&vec![b'a'; 900000]).unwrap();
        stage.write_all(&vec![b'b'; 100000]).unwrap();
        assert!(stage.memory.capacity() <= MEMORY_LIMIT);
        stage.write_all(&vec![b'c'; 100000]).unwrap();
        let source = stage.finish().unwrap();
        let mut first = source.reader().unwrap();
        let mut second = source.reader().unwrap();
        let mut prefix = [0; 4];
        first.read_exact(&mut prefix).unwrap();
        second.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"aaaa");
        let mut output = Vec::new();
        assert_eq!(source.copy_to(&mut output).unwrap(), 1100000);
        assert_eq!(&output[899999..900001], b"ab");
        assert_eq!(&output[999999..1000001], b"bc");
        let mut tail = Vec::new();
        first.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, output[4..]);
        tail.clear();
        second.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, output[4..]);
    }

    #[test]
    fn owned_spill_is_unlinked_and_reader_stays_bounded() {
        use std::io::Read;
        let input = vec![b' '; MEMORY_LIMIT + 17];
        let source = ByteSource::read(input.as_slice()).unwrap();
        let Bytes::File(spill) = &source.bytes else {
            panic!("expected spill")
        };
        assert_eq!(
            spill.file.as_ref().unwrap().metadata().unwrap().len(),
            input.len() as u64
        );
        #[cfg(unix)]
        assert!(!spill.path.exists());
        let path = spill.path.clone();
        let mut prefix = [0; 17];
        source.reader().unwrap().read_exact(&mut prefix).unwrap();
        assert_eq!(prefix, [b' '; 17]);
        #[cfg(feature = "mmap")]
        assert_eq!(source.with_bytes(|bytes| bytes.len()).unwrap(), Some(input.len()));
        drop(source);
        assert!(!path.exists());
    }
}
