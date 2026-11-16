use crate::common::{extract_user_key, AetherError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockHandle {
    pub offset: u64,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub largest_key: Vec<u8>,
    pub handle: BlockHandle,
}

/// Two-level sparse block index builder.
pub struct IndexBuilder {
    entries: Vec<IndexEntry>,
}

impl Default for IndexBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl IndexBuilder {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn add_block(&mut self, largest_key: Vec<u8>, offset: u64, size: u64) {
        self.entries.push(IndexEntry {
            largest_key,
            handle: BlockHandle { offset, size },
        });
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Encode the index into a compact byte vector.
    /// Wire format:
    /// [num_entries: u32]
    /// For each entry:
    ///   [key_len: u32]
    ///   [key_bytes]
    ///   [offset: u64]
    ///   [size: u64]
    pub fn finish(self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());

        for entry in self.entries {
            buf.extend_from_slice(&(entry.largest_key.len() as u32).to_le_bytes());
            buf.extend_from_slice(&entry.largest_key);
            buf.extend_from_slice(&entry.handle.offset.to_le_bytes());
            buf.extend_from_slice(&entry.handle.size.to_le_bytes());
        }

        buf
    }
}

/// In-memory two-level index reader with binary search.
pub struct BlockIndex {
    entries: Vec<IndexEntry>,
}

impl BlockIndex {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 4 {
            return Err(AetherError::Corruption("Index bytes too short".into()));
        }

        let num_entries = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let mut offset = 4;
        let mut entries = Vec::with_capacity(num_entries);

        for _ in 0..num_entries {
            if offset + 4 > bytes.len() {
                return Err(AetherError::Corruption(
                    "Truncated index entry header".into(),
                ));
            }

            let key_len =
                u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;

            if offset + key_len + 16 > bytes.len() {
                return Err(AetherError::Corruption("Truncated index entry body".into()));
            }

            let key = bytes[offset..offset + key_len].to_vec();
            offset += key_len;

            let block_offset = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
            offset += 8;

            let block_size = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
            offset += 8;

            entries.push(IndexEntry {
                largest_key: key,
                handle: BlockHandle {
                    offset: block_offset,
                    size: block_size,
                },
            });
        }

        Ok(Self { entries })
    }

    /// Binary search for candidate data block that could contain user_key.
    pub fn find_candidate_block(&self, user_key: &[u8]) -> Option<BlockHandle> {
        if self.entries.is_empty() {
            return None;
        }

        let mut left = 0;
        let mut right = self.entries.len();

        while left < right {
            let mid = left + (right - left) / 2;
            let entry_ukey = extract_user_key(&self.entries[mid].largest_key);

            if entry_ukey < user_key {
                left = mid + 1;
            } else {
                right = mid;
            }
        }

        if left < self.entries.len() {
            Some(self.entries[left].handle.clone())
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_index_encode_decode() {
        let mut builder = IndexBuilder::new();
        builder.add_block(b"alpha".to_vec(), 0, 4096);
        builder.add_block(b"beta".to_vec(), 4096, 4096);
        builder.add_block(b"gamma".to_vec(), 8192, 4096);

        let bytes = builder.finish();
        let index = BlockIndex::decode(&bytes).expect("valid index");

        assert_eq!(index.entries().len(), 3);

        let handle1 = index.find_candidate_block(b"alpha").unwrap();
        assert_eq!(handle1.offset, 0);

        let handle2 = index.find_candidate_block(b"bar").unwrap();
        assert_eq!(handle2.offset, 4096);

        let handle_none = index.find_candidate_block(b"zeta");
        assert!(handle_none.is_none());
    }
}
