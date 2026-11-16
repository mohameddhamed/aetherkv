use crate::common::{
    crc32c, extract_tag, extract_user_key, AetherError, Result, SequenceNumber, ValueType,
};

pub const BLOCK_TARGET_SIZE: usize = 4096;
pub const RESTART_INTERVAL: usize = 16;

/// Builder for constructing 4KB data blocks with delta prefix compression.
pub struct BlockBuilder {
    buffer: Vec<u8>,
    restarts: Vec<u32>,
    counter: usize,
    last_key: Vec<u8>,
    restart_interval: usize,
}

impl BlockBuilder {
    pub fn new(restart_interval: usize) -> Self {
        let mut restarts = Vec::with_capacity(16);
        restarts.push(0); // Initial restart point at offset 0
        Self {
            buffer: Vec::with_capacity(BLOCK_TARGET_SIZE),
            restarts,
            counter: 0,
            last_key: Vec::new(),
            restart_interval,
        }
    }

    #[inline(always)]
    pub fn current_size(&self) -> usize {
        self.buffer.len() + self.restarts.len() * 4 + 8 // buffer + restarts + num_restarts (4) + crc (4)
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Add a key-value pair to the block with delta encoding.
    /// Keys MUST be added in monotonically increasing order.
    pub fn add(&mut self, key: &[u8], value: &[u8]) {
        let mut shared = 0;
        if self.counter < self.restart_interval {
            // Calculate common prefix with last_key
            let min_len = self.last_key.len().min(key.len());
            while shared < min_len && self.last_key[shared] == key[shared] {
                shared += 1;
            }
        } else {
            // New restart interval
            self.restarts.push(self.buffer.len() as u32);
            self.counter = 0;
        }

        let unshared = key.len() - shared;

        // Encode: shared_len (u32), unshared_len (u32), val_len (u32)
        self.buffer
            .extend_from_slice(&(shared as u32).to_le_bytes());
        self.buffer
            .extend_from_slice(&(unshared as u32).to_le_bytes());
        self.buffer
            .extend_from_slice(&(value.len() as u32).to_le_bytes());

        // Delta key bytes and value bytes
        self.buffer.extend_from_slice(&key[shared..]);
        self.buffer.extend_from_slice(value);

        // Update tracking state
        self.last_key.truncate(shared);
        self.last_key.extend_from_slice(&key[shared..]);
        self.counter += 1;
    }

    /// Finalize and seal the block: appends restart offsets, num_restarts, and CRC32C.
    pub fn finish(mut self) -> Vec<u8> {
        let num_restarts = self.restarts.len() as u32;
        for &offset in &self.restarts {
            self.buffer.extend_from_slice(&offset.to_le_bytes());
        }
        self.buffer.extend_from_slice(&num_restarts.to_le_bytes());

        let checksum = crc32c(&self.buffer);
        self.buffer.extend_from_slice(&checksum.to_le_bytes());

        self.buffer
    }
}

/// Reader for decoding 4KB data blocks using binary search on restart points.
pub struct BlockReader<'a> {
    data: &'a [u8],
    data_len: usize,
    restarts_offset: usize,
    num_restarts: usize,
}

impl<'a> BlockReader<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self> {
        if data.len() < 8 {
            return Err(AetherError::Corruption("Block data too small".into()));
        }

        // Verify CRC32C trailer
        let payload_len = data.len() - 4;
        let expected_crc = u32::from_le_bytes(data[payload_len..].try_into().unwrap());
        let actual_crc = crc32c(&data[..payload_len]);
        if expected_crc != actual_crc {
            return Err(AetherError::BlockCrcMismatch {
                expected: expected_crc,
                actual: actual_crc,
            });
        }

        let num_restarts =
            u32::from_le_bytes(data[payload_len - 4..payload_len].try_into().unwrap()) as usize;

        let restarts_size = num_restarts * 4;
        if payload_len < 4 + restarts_size {
            return Err(AetherError::Corruption(
                "Invalid block restart structure".into(),
            ));
        }

        let restarts_offset = payload_len - 4 - restarts_size;

        Ok(Self {
            data: &data[..payload_len],
            data_len: restarts_offset,
            restarts_offset,
            num_restarts,
        })
    }

    #[inline(always)]
    fn get_restart_offset(&self, idx: usize) -> usize {
        let base = self.restarts_offset + idx * 4;
        u32::from_le_bytes(self.data[base..base + 4].try_into().unwrap()) as usize
    }

    /// Search for user key using binary search across restart points followed by delta scanning.
    pub fn get(
        &self,
        user_key: &[u8],
        snapshot_seq: SequenceNumber,
    ) -> Option<(ValueType, Vec<u8>)> {
        if self.num_restarts == 0 {
            return None;
        }

        // 1. Binary search on restart points to locate candidate interval
        let mut left = 0;
        let mut right = self.num_restarts - 1;

        while left < right {
            let mid = (left + right).div_ceil(2);
            let offset = self.get_restart_offset(mid);
            let key = self.decode_restart_key(offset)?;

            if extract_user_key(&key) <= user_key {
                left = mid;
            } else {
                right = mid - 1;
            }
        }

        // 2. Linear delta scan from restart point `left`
        let start_offset = self.get_restart_offset(left);
        let end_offset = if left + 1 < self.num_restarts {
            self.get_restart_offset(left + 1)
        } else {
            self.data_len
        };

        let mut offset = start_offset;
        let mut current_key = Vec::new();

        while offset < end_offset {
            if offset + 12 > self.data_len {
                break;
            }

            let shared =
                u32::from_le_bytes(self.data[offset..offset + 4].try_into().unwrap()) as usize;
            let unshared =
                u32::from_le_bytes(self.data[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let val_len =
                u32::from_le_bytes(self.data[offset + 8..offset + 12].try_into().unwrap()) as usize;
            offset += 12;

            if offset + unshared + val_len > self.data_len {
                break;
            }

            current_key.truncate(shared);
            current_key.extend_from_slice(&self.data[offset..offset + unshared]);
            offset += unshared;

            let val = &self.data[offset..offset + val_len];
            offset += val_len;

            let ukey = extract_user_key(&current_key);
            if ukey == user_key {
                if let Some((seq, vt)) = extract_tag(&current_key) {
                    if seq <= snapshot_seq {
                        return Some((vt, val.to_vec()));
                    }
                }
            } else if ukey > user_key {
                return None;
            }
        }

        None
    }

    fn decode_restart_key(&self, offset: usize) -> Option<Vec<u8>> {
        if offset + 12 > self.data_len {
            return None;
        }
        let shared = u32::from_le_bytes(self.data[offset..offset + 4].try_into().unwrap()) as usize;
        let unshared =
            u32::from_le_bytes(self.data[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let val_len =
            u32::from_le_bytes(self.data[offset + 8..offset + 12].try_into().unwrap()) as usize;

        if shared != 0 {
            return None; // Restart points must have shared == 0
        }

        let key_start = offset + 12;
        let key_end = key_start + unshared;
        if key_end + val_len > self.data_len {
            return None;
        }

        Some(self.data[key_start..key_end].to_vec())
    }

    /// Iterate over all entries in this block.
    pub fn iter(&self) -> BlockIterator<'a> {
        BlockIterator {
            data: &self.data[..self.data_len],
            offset: 0,
            current_key: Vec::new(),
        }
    }
}

pub struct BlockIterator<'a> {
    data: &'a [u8],
    offset: usize,
    current_key: Vec<u8>,
}

impl<'a> Iterator for BlockIterator<'a> {
    type Item = (Vec<u8>, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset + 12 > self.data.len() {
            return None;
        }

        let shared = u32::from_le_bytes(self.data[self.offset..self.offset + 4].try_into().unwrap())
            as usize;
        let unshared = u32::from_le_bytes(
            self.data[self.offset + 4..self.offset + 8]
                .try_into()
                .unwrap(),
        ) as usize;
        let val_len = u32::from_le_bytes(
            self.data[self.offset + 8..self.offset + 12]
                .try_into()
                .unwrap(),
        ) as usize;
        self.offset += 12;

        if self.offset + unshared + val_len > self.data.len() {
            return None;
        }

        self.current_key.truncate(shared);
        self.current_key
            .extend_from_slice(&self.data[self.offset..self.offset + unshared]);
        self.offset += unshared;

        let val = self.data[self.offset..self.offset + val_len].to_vec();
        self.offset += val_len;

        Some((self.current_key.clone(), val))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{compare_internal_keys, InternalKey, ValueType};

    #[test]
    fn test_block_builder_and_reader() {
        let mut builder = BlockBuilder::new(4);
        let mut entries = Vec::new();

        for i in 0..50 {
            let key = format!("user_{:06}", i);
            let val = format!("value_data_{:06}", i);
            let ikey = InternalKey::new(key.into_bytes(), 100, ValueType::Put).encode();
            entries.push((ikey.clone(), val.into_bytes()));
        }

        // Sort entries by internal key order
        entries.sort_by(|a, b| compare_internal_keys(&a.0, &b.0));

        for (k, v) in &entries {
            builder.add(k, v);
        }

        let block_bytes = builder.finish();
        let reader = BlockReader::new(&block_bytes).unwrap();

        // Test point lookups
        for (k, v) in &entries {
            let ukey = extract_user_key(k);
            let res = reader.get(ukey, 100);
            assert!(res.is_some(), "Key not found in block");
            assert_eq!(res.unwrap().1, *v);
        }

        // Test absent key
        assert!(reader.get(b"nonexistent", 100).is_none());

        // Test iterator
        let decoded_entries: Vec<_> = reader.iter().collect();
        assert_eq!(decoded_entries.len(), entries.len());
        for i in 0..entries.len() {
            assert_eq!(decoded_entries[i].0, entries[i].0);
            assert_eq!(decoded_entries[i].1, entries[i].1);
        }
    }
}
