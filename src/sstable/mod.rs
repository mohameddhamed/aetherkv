pub mod block;
pub mod bloom;
pub mod footer;
pub mod index;

pub use block::{BlockBuilder, BlockIterator, BlockReader, BLOCK_TARGET_SIZE, RESTART_INTERVAL};
pub use bloom::{hash_key, BlockSplitBloomFilter};
pub use footer::{TableFooter, FOOTER_ENCODED_SIZE, TABLE_MAGIC_NUMBER};
pub use index::{BlockHandle, BlockIndex, IndexBuilder, IndexEntry};

use crate::common::{AetherError, Result};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct TableBuilder {
    file: File,
    path: PathBuf,
    block_builder: BlockBuilder,
    index_builder: IndexBuilder,
    key_hashes: Vec<u64>,
    current_offset: u64,
    last_key: Vec<u8>,
    num_entries: u64,
}

impl TableBuilder {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::create(&path)?;

        Ok(Self {
            file,
            path,
            block_builder: BlockBuilder::new(RESTART_INTERVAL),
            index_builder: IndexBuilder::new(),
            key_hashes: Vec::new(),
            current_offset: 0,
            last_key: Vec::new(),
            num_entries: 0,
        })
    }

    /// Add a key-value pair to the SSTable.
    /// Invariant: keys must be passed in monotonically increasing order.
    pub fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.key_hashes.push(hash_key(key));
        self.last_key = key.to_vec();
        self.num_entries += 1;

        self.block_builder.add(key, value);

        if self.block_builder.current_size() >= BLOCK_TARGET_SIZE {
            self.flush_data_block()?;
        }

        Ok(())
    }

    fn flush_data_block(&mut self) -> Result<()> {
        if self.block_builder.is_empty() {
            return Ok(());
        }

        let old_builder = std::mem::replace(
            &mut self.block_builder,
            BlockBuilder::new(RESTART_INTERVAL),
        );
        let block_bytes = old_builder.finish();
        let block_len = block_bytes.len() as u64;

        self.file.write_all(&block_bytes)?;

        self.index_builder.add_block(
            self.last_key.clone(),
            self.current_offset,
            block_len,
        );

        self.current_offset += block_len;
        Ok(())
    }

    /// Seal and finalize the SSTable file:
    /// 1. Flushes final data block.
    /// 2. Writes AVX2 Block-Split Bloom filter.
    /// 3. Writes sparse block index.
    /// 4. Writes 48-byte fixed footer.
    pub fn finish(mut self) -> Result<(u64, PathBuf)> {
        self.flush_data_block()?;

        // Build and write Bloom filter (10 bits/key, <1% false positive rate)
        let bloom = BlockSplitBloomFilter::build_from_hashes(&self.key_hashes, 10);
        let bloom_bytes = bloom.as_bytes();
        let bloom_offset = self.current_offset;
        let bloom_size = bloom_bytes.len() as u64;
        self.file.write_all(bloom_bytes)?;
        self.current_offset += bloom_size;

        // Build and write sparse block index
        let index_bytes = self.index_builder.finish();
        let index_offset = self.current_offset;
        let index_size = index_bytes.len() as u64;
        self.file.write_all(&index_bytes)?;
        self.current_offset += index_size;

        // Write 48-byte fixed footer
        let footer = TableFooter::new(
            index_offset,
            index_size,
            bloom_offset,
            bloom_size,
            self.num_entries,
        );
        let footer_bytes = footer.encode();
        self.file.write_all(&footer_bytes)?;
        self.current_offset += FOOTER_ENCODED_SIZE as u64;

        self.file.sync_all()?;
        Ok((self.current_offset, self.path))
    }
}

/// Zero-deserialization memory-mapped SSTable reader.
pub struct TableReader {
    path: PathBuf,
    mmap_ptr: *const u8,
    file_size: usize,
    index: BlockIndex,
    bloom: BlockSplitBloomFilter,
    footer: TableFooter,
    _file: File,
}

unsafe impl Send for TableReader {}
unsafe impl Sync for TableReader {}

impl TableReader {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let file_size = file.metadata()?.len() as usize;

        if file_size < FOOTER_ENCODED_SIZE {
            return Err(AetherError::Corruption("SSTable file smaller than footer".into()));
        }

        // Memory-map the SSTable with read-only permissions
        let mmap_ptr = unsafe {
            use std::os::unix::io::AsRawFd;
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                file_size,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            );
            if ptr == libc::MAP_FAILED {
                return Err(AetherError::Io(std::io::Error::last_os_error()));
            }
            ptr as *const u8
        };

        let footer_offset = file_size - FOOTER_ENCODED_SIZE;
        let footer_slice = unsafe {
            std::slice::from_raw_parts(mmap_ptr.add(footer_offset), FOOTER_ENCODED_SIZE)
        };
        let footer = TableFooter::decode(footer_slice.try_into().unwrap())?;

        // Read Bloom Filter
        let bloom_slice = unsafe {
            std::slice::from_raw_parts(
                mmap_ptr.add(footer.bloom_offset as usize),
                footer.bloom_size as usize,
            )
        };
        let bloom = BlockSplitBloomFilter::from_bytes(bloom_slice.to_vec());

        // Read Index
        let index_slice = unsafe {
            std::slice::from_raw_parts(
                mmap_ptr.add(footer.index_offset as usize),
                footer.index_size as usize,
            )
        };
        let index = BlockIndex::decode(index_slice)?;

        Ok(Self {
            path,
            mmap_ptr,
            file_size,
            index,
            bloom,
            footer,
            _file: file,
        })
    }

    #[inline(always)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[inline(always)]
    pub fn file_size(&self) -> usize {
        self.file_size
    }

    #[inline(always)]
    pub fn num_entries(&self) -> u64 {
        self.footer.num_entries
    }

    /// Point lookup with AVX2 Bloom filter fast exit.
    pub fn get(&self, target_key: &[u8]) -> Result<Option<Vec<u8>>> {
        let hash = hash_key(target_key);

        // Sub-nanosecond Bloom check: eliminate cold disk lookups on non-existent keys
        if !self.bloom.may_contain_hash(hash) {
            return Ok(None);
        }

        // Binary search sparse block index
        let handle = match self.index.find_candidate_block(target_key) {
            Some(h) => h,
            None => return Ok(None),
        };

        if (handle.offset + handle.size) as usize > self.file_size {
            return Err(AetherError::Corruption("Block handle offset out of bounds".into()));
        }

        let block_slice = unsafe {
            std::slice::from_raw_parts(
                self.mmap_ptr.add(handle.offset as usize),
                handle.size as usize,
            )
        };

        let block_reader = BlockReader::new(block_slice)?;
        Ok(block_reader.get(target_key))
    }

    /// Full iterator over all entries in the SSTable.
    pub fn iter(&self) -> TableIterator<'_> {
        TableIterator {
            reader: self,
            block_idx: 0,
            current_block_iter: None,
        }
    }
}

impl Drop for TableReader {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.mmap_ptr as *mut libc::c_void, self.file_size);
        }
    }
}

pub struct TableIterator<'a> {
    reader: &'a TableReader,
    block_idx: usize,
    current_block_iter: Option<BlockIterator<'a>>,
}

impl<'a> Iterator for TableIterator<'a> {
    type Item = (Vec<u8>, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(ref mut it) = self.current_block_iter {
                if let Some(entry) = it.next() {
                    return Some(entry);
                }
            }

            // Move to next block
            let entries = self.reader.index.entries();
            if self.block_idx >= entries.len() {
                return None;
            }

            let handle = &entries[self.block_idx].handle;
            self.block_idx += 1;

            let block_slice = unsafe {
                std::slice::from_raw_parts(
                    self.reader.mmap_ptr.add(handle.offset as usize),
                    handle.size as usize,
                )
            };

            if let Ok(block_reader) = BlockReader::new(block_slice) {
                // We create an iterator with lifetime matching the mmap data slice
                let it: BlockIterator<'a> = unsafe {
                    std::mem::transmute(block_reader.iter())
                };
                self.current_block_iter = Some(it);
            } else {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{compare_internal_keys, InternalKey, ValueType};
    use tempfile::tempdir;

    #[test]
    fn test_sstable_build_and_read() {
        let dir = tempdir().unwrap();
        let sst_path = dir.path().join("000001.sst");

        let mut builder = TableBuilder::new(&sst_path).unwrap();
        let mut dataset = Vec::new();

        for i in 0..1000 {
            let k = format!("key_{:06}", i);
            let v = format!("val_{:06}", i);
            let ikey = InternalKey::new(k.into_bytes(), 1, ValueType::Put).encode();
            dataset.push((ikey, v.into_bytes()));
        }

        dataset.sort_by(|a, b| compare_internal_keys(&a.0, &b.0));

        for (k, v) in &dataset {
            builder.add(k, v).unwrap();
        }

        let (size, path) = builder.finish().unwrap();
        assert!(size > 0);

        let reader = TableReader::open(&path).unwrap();
        assert_eq!(reader.num_entries(), 1000);

        // Test lookups for all keys
        for (k, v) in &dataset {
            let res = reader.get(k).unwrap();
            assert!(res.is_some(), "Key not found");
            assert_eq!(res.unwrap(), *v);
        }

        // Test absent keys (fast Bloom exit)
        for i in 1000..1100 {
            let k = format!("absent_key_{:06}", i);
            let ikey = InternalKey::new(k.into_bytes(), 1, ValueType::Put).encode();
            let res = reader.get(&ikey).unwrap();
            assert!(res.is_none());
        }

        // Test iterator scan
        let scanned: Vec<_> = reader.iter().collect();
        assert_eq!(scanned.len(), 1000);
        for i in 0..1000 {
            assert_eq!(scanned[i].0, dataset[i].0);
            assert_eq!(scanned[i].1, dataset[i].1);
        }
    }
}
