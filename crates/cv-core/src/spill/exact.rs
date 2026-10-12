//! Exact equality with a bounded in-memory fast path and a portable disk hash table.
use super::Temporary;
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};

const MEMORY_BYTES: usize = 1024 * 1024;
const SLOT_BYTES: u64 = 24;
const COMPARE_BYTES: usize = 8192;

pub(crate) struct ExactStrings {
    memory: HashSet<String>,
    bytes: usize,
    spill: Option<Disk>,
    hashes: std::collections::hash_map::RandomState,
    limit: usize,
}
impl Default for ExactStrings {
    fn default() -> Self {
        Self {
            memory: HashSet::new(),
            bytes: 0,
            spill: None,
            hashes: std::collections::hash_map::RandomState::new(),
            limit: MEMORY_BYTES,
        }
    }
}
impl ExactStrings {
    #[cfg(any(feature = "sqlite", test))]
    pub(crate) fn contains(&mut self, text: &str) -> Result<bool> {
        let hash = self.hash(text);
        match &mut self.spill {
            Some(disk) => Ok(disk.find(text.as_bytes(), hash)?.1),
            None => Ok(self.memory.contains(text)),
        }
    }

    /// True only on first insertion. Empty strings, embedded NUL and hash collisions are exact.
    pub(crate) fn insert(&mut self, text: String) -> Result<bool> {
        if self.spill.is_none() && self.memory.contains(&text) {
            return Ok(false);
        }
        // Account table slots as well as payloads. This conservative upper bound exceeds the
        // HashSet's bucket/control allocation even when the next insert grows an empty table.
        let slots = self
            .memory
            .len()
            .saturating_add(1)
            .checked_next_power_of_two()
            .unwrap_or(usize::MAX)
            .saturating_mul(2);
        let estimated = self
            .bytes
            .saturating_add(text.capacity())
            .saturating_add(slots.saturating_mul(std::mem::size_of::<String>() + 16));
        if self.spill.is_none() && estimated > self.limit {
            let mut disk = Disk::new(self.memory.len())?;
            for old in &self.memory {
                disk.insert(old.as_bytes(), self.hash(old))?;
            }
            self.spill = Some(disk);
            self.memory = HashSet::new();
            self.bytes = 0;
        }
        let hash = self.hash(&text);
        match &mut self.spill {
            Some(disk) => disk.insert(text.as_bytes(), hash),
            None => {
                self.bytes += text.capacity();
                Ok(self.memory.insert(text))
            }
        }
    }

    #[cfg(all(test, feature = "sqlite"))]
    pub(crate) fn is_spilled(&self) -> bool {
        self.spill.is_some()
    }

    #[cfg(any(feature = "sqlite", test))]
    pub(crate) fn clear(&mut self) {
        self.spill.take();
        self.memory = HashSet::new();
        self.bytes = 0;
    }

    fn hash(&self, text: &str) -> u64 {
        let mut hasher = self.hashes.build_hasher();
        hasher.write(text.as_bytes());
        hasher.finish()
    }
}

#[derive(Clone, Copy, Default)]
struct Slot {
    hash: u64,
    offset: u64,
    len: u64,
}
impl Slot {
    fn read(file: &mut Index, index: u64) -> Result<Self> {
        let bytes = file.read(index)?;
        Ok(Self {
            hash: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            offset: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            len: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
        })
    }
    fn write(self, file: &mut Index, index: u64) -> Result<()> {
        let mut bytes = [0; SLOT_BYTES as usize];
        bytes[0..8].copy_from_slice(&self.hash.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.offset.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.len.to_le_bytes());
        file.write(index, &bytes)
    }
}
// A fixed direct-mapped page cache avoids a syscall per small bucket lookup. Its bound is
// independent of the index/key count; evictions flush dirty pages before exact reads continue.
const SLOTS_PER_PAGE: u64 = 128;
const PAGE_BYTES: usize = SLOTS_PER_PAGE as usize * SLOT_BYTES as usize;
const CACHE_PAGES: usize = 64;
struct Page {
    key: Option<u64>,
    dirty: bool,
    bytes: [u8; PAGE_BYTES],
}
struct Index {
    file: File,
    _owner: Temporary,
    pages: Vec<Page>,
}
impl Index {
    fn new(capacity: u64) -> Result<Self> {
        let (file, owner) = Temporary::create("exact-index")?;
        let count = capacity.div_ceil(SLOTS_PER_PAGE);
        file.set_len(count.checked_mul(PAGE_BYTES as u64).context("spill index overflow")?)?;
        let mut pages = Vec::with_capacity(CACHE_PAGES);
        for _ in 0..CACHE_PAGES {
            pages.push(Page {
                key: None,
                dirty: false,
                bytes: [0; PAGE_BYTES],
            });
        }
        Ok(Self {
            file,
            _owner: owner,
            pages,
        })
    }
    fn page(&mut self, number: u64) -> Result<&mut Page> {
        let page = &mut self.pages[(number % CACHE_PAGES as u64) as usize];
        if page.key != Some(number) {
            if let Some(old) = page.key.filter(|_| page.dirty) {
                self.file.seek(SeekFrom::Start(
                    old.checked_mul(PAGE_BYTES as u64).context("spill index overflow")?,
                ))?;
                self.file
                    .write_all(&page.bytes)
                    .context("writing exact set index page")?;
            }
            self.file.seek(SeekFrom::Start(
                number.checked_mul(PAGE_BYTES as u64).context("spill index overflow")?,
            ))?;
            self.file
                .read_exact(&mut page.bytes)
                .context("reading exact set index page")?;
            page.key = Some(number);
            page.dirty = false;
        }
        Ok(page)
    }
    fn read(&mut self, slot: u64) -> Result<[u8; SLOT_BYTES as usize]> {
        let offset = (slot % SLOTS_PER_PAGE) as usize * SLOT_BYTES as usize;
        let page = self.page(slot / SLOTS_PER_PAGE)?;
        Ok(page.bytes[offset..offset + SLOT_BYTES as usize].try_into().unwrap())
    }
    fn write(&mut self, slot: u64, bytes: &[u8; SLOT_BYTES as usize]) -> Result<()> {
        let offset = (slot % SLOTS_PER_PAGE) as usize * SLOT_BYTES as usize;
        let page = self.page(slot / SLOTS_PER_PAGE)?;
        page.bytes[offset..offset + SLOT_BYTES as usize].copy_from_slice(bytes);
        page.dirty = true;
        Ok(())
    }
}
struct Disk {
    data: File,
    index: Index,
    _data_owner: Temporary,
    capacity: u64,
    len: u64,
    end: u64,
}
impl Disk {
    fn new(len: usize) -> Result<Self> {
        let (data, data_owner) = Temporary::create("exact-data")?;
        let capacity = (len as u64)
            .checked_mul(2)
            .context("spill capacity overflow")?
            .max(16)
            .checked_next_power_of_two()
            .context("spill capacity overflow")?;
        let index = Index::new(capacity)?;
        Ok(Self {
            data,
            index,
            _data_owner: data_owner,
            capacity,
            len: 0,
            end: 0,
        })
    }
    fn find(&mut self, text: &[u8], hash: u64) -> Result<(u64, bool)> {
        let mut bucket = hash & (self.capacity - 1);
        for _ in 0..self.capacity {
            let slot = Slot::read(&mut self.index, bucket)?;
            if slot.offset == 0 {
                return Ok((bucket, false));
            }
            if slot.hash == hash && slot.len == text.len() as u64 && self.equal(slot, text)? {
                return Ok((bucket, true));
            }
            bucket = (bucket + 1) & (self.capacity - 1);
        }
        bail!("exact set index is full")
    }
    fn equal(&mut self, slot: Slot, text: &[u8]) -> Result<bool> {
        self.data.seek(SeekFrom::Start(slot.offset - 1))?;
        let mut scratch = [0; COMPARE_BYTES];
        for chunk in text.chunks(COMPARE_BYTES) {
            self.data
                .read_exact(&mut scratch[..chunk.len()])
                .context("reading exact set key")?;
            if chunk != &scratch[..chunk.len()] {
                return Ok(false);
            }
        }
        Ok(true)
    }
    fn insert(&mut self, text: &[u8], hash: u64) -> Result<bool> {
        let (mut bucket, present) = self.find(text, hash)?;
        if present {
            return Ok(false);
        }
        if self.len >= self.capacity / 2 {
            self.grow()?;
            bucket = self.find(text, hash)?.0;
        }
        self.data.seek(SeekFrom::Start(self.end))?;
        self.data.write_all(text).context("writing exact set key")?;
        let offset = self.end.checked_add(1).context("spill offset overflow")?;
        self.end = self.end.checked_add(text.len() as u64).context("spill size overflow")?;
        Slot {
            hash,
            offset,
            len: text.len() as u64,
        }
        .write(&mut self.index, bucket)?;
        self.len += 1;
        Ok(true)
    }
    fn grow(&mut self) -> Result<()> {
        let capacity = self.capacity.checked_mul(2).context("spill capacity overflow")?;
        let mut index = Index::new(capacity)?;
        // Rehash one slot at a time. Neither the keys nor the index are materialized in RAM.
        for old in 0..self.capacity {
            let slot = Slot::read(&mut self.index, old)?;
            if slot.offset == 0 {
                continue;
            }
            let mut bucket = slot.hash & (capacity - 1);
            while Slot::read(&mut index, bucket)?.offset != 0 {
                bucket = (bucket + 1) & (capacity - 1);
            }
            slot.write(&mut index, bucket)?;
        }
        self.index = index;
        self.capacity = capacity;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn collisions_growth_empty_and_binary_keys_match_hash_set() {
        let mut disk = Disk::new(0).unwrap();
        let mut oracle = HashSet::new();
        for i in 0..300 {
            let key = format!("{i}\0λ");
            // Deliberately identical hashes require full key comparison, across index growth.
            assert_eq!(disk.insert(key.as_bytes(), 7).unwrap(), oracle.insert(key.clone()));
            assert!(!disk.insert(key.as_bytes(), 7).unwrap());
        }
        assert!(disk.insert(b"", 7).unwrap());
        assert!(!disk.insert(b"", 7).unwrap());
        for key in oracle {
            assert!(disk.find(key.as_bytes(), 7).unwrap().1);
        }
        assert!(!disk.find(b"absent", 7).unwrap().1);
    }
    #[test]
    fn tiny_keys_charge_slots_spill_and_clear() {
        let mut set = ExactStrings {
            limit: 1024,
            ..ExactStrings::default()
        };
        assert!(set.insert(String::new()).unwrap());
        assert!(!set.insert(String::new()).unwrap());
        for i in 0..400 {
            assert!(set.insert(i.to_string()).unwrap());
        }
        assert!(set.spill.is_some());
        assert_eq!(set.memory.capacity(), 0);
        for i in 0..400 {
            assert!(set.contains(&i.to_string()).unwrap());
        }
        set.clear();
        assert!(set.spill.is_none());
        assert!(!set.contains("").unwrap());
    }
    #[test]
    fn cache_evictions_and_index_growth_keep_every_key() {
        let mut disk = Disk::new(0).unwrap();
        for i in 0..20000_u64 {
            assert!(disk
                .insert(&i.to_le_bytes(), i.wrapping_mul(0x9e3779b97f4a7c15))
                .unwrap());
        }
        assert!(disk.capacity > SLOTS_PER_PAGE * CACHE_PAGES as u64);
        for i in (0..20000_u64).rev() {
            assert!(
                disk.find(&i.to_le_bytes(), i.wrapping_mul(0x9e3779b97f4a7c15))
                    .unwrap()
                    .1
            );
        }
    }
    #[test]
    fn disk_io_error_is_not_a_duplicate() {
        let mut disk = Disk::new(0).unwrap();
        disk.insert(b"key", 1).unwrap();
        disk.index.file.set_len(0).unwrap();
        for page in &mut disk.index.pages {
            page.key = None;
        }
        assert!(disk.find(b"key", 1).is_err());
    }
}
