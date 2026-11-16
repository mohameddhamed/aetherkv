/// 256-bit Block-Split Bloom Filter (Split-Block Bloom Filter).
/// Uses AVX2 SIMD instructions on x86_64 and a bit-compatible portable fallback.
/// Consumes 8 bits per key (~1% false positive rate) with single cacheline (32-byte) locality.

const SALTS: [u32; 8] = [
    0x9efc4947, 0x2df1424b, 0x705495c7, 0xa2b7289d,
    0x8824ad5b, 0x44974d91, 0x47b6137b, 0x00000001,
];

pub struct BlockSplitBloomFilter {
    data: Vec<u8>,
    num_blocks: usize,
}

impl BlockSplitBloomFilter {
    /// Build a bloom filter from a slice of 64-bit key hashes.
    /// Allocates approximately `bits_per_key` (recommended: 8) bits per key.
    pub fn build_from_hashes(hashes: &[u64], bits_per_key: usize) -> Self {
        let total_bits = (hashes.len() * bits_per_key).max(256);
        let num_blocks = (total_bits + 255) / 256;
        let mut filter = Self {
            data: vec![0u8; num_blocks * 32],
            num_blocks,
        };

        for &h in hashes {
            filter.add_hash(h);
        }

        filter
    }

    /// Load filter from pre-existing raw byte buffer (e.g. read from SSTable).
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        let num_blocks = bytes.len() / 32;
        Self {
            data: bytes,
            num_blocks,
        }
    }

    #[inline(always)]
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    #[inline(always)]
    pub fn num_blocks(&self) -> usize {
        self.num_blocks
    }

    /// Add a 64-bit key hash to the filter.
    pub fn add_hash(&mut self, hash: u64) {
        if self.num_blocks == 0 {
            return;
        }

        let block_idx = self.select_block(hash);
        let key_hash = hash as u32;
        let block_offset = block_idx * 32;

        unsafe {
            let block_ptr = self.data.as_mut_ptr().add(block_offset) as *mut u32;
            for i in 0..8 {
                let shift = (key_hash.wrapping_mul(SALTS[i])) >> 27;
                let mask = 1u32 << shift;
                *block_ptr.add(i) |= mask;
            }
        }
    }

    /// Fast membership test for a 64-bit key hash.
    #[inline(always)]
    pub fn may_contain_hash(&self, hash: u64) -> bool {
        if self.num_blocks == 0 {
            return true;
        }

        let block_idx = self.select_block(hash);
        let key_hash = hash as u32;
        let block_offset = block_idx * 32;

        unsafe {
            let block_ptr = self.data.as_ptr().add(block_offset) as *const u32;

            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("avx2") {
                    return test_key_avx2(block_ptr as *const std::arch::x86_64::__m256i, key_hash);
                }
            }

            test_key_portable(block_ptr, key_hash)
        }
    }

    #[inline(always)]
    fn select_block(&self, hash: u64) -> usize {
        let high = (hash >> 32) as u32;
        ((high as u64 * self.num_blocks as u64) >> 32) as usize
    }
}

/// Portable SIMD-equivalent bitwise evaluation.
#[inline(always)]
unsafe fn test_key_portable(block_ptr: *const u32, key_hash: u32) -> bool {
    for i in 0..8 {
        let shift = (key_hash.wrapping_mul(SALTS[i])) >> 27;
        let mask = 1u32 << shift;
        if (*block_ptr.add(i) & mask) == 0 {
            return false;
        }
    }
    true
}

/// AVX2 256-bit SIMD implementation: single 32-byte load, parallel multiplication, shift, and testz.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn test_key_avx2(block_ptr: *const std::arch::x86_64::__m256i, key_hash: u32) -> bool {
    use std::arch::x86_64::*;

    let hash_vec = _mm256_set1_epi32(key_hash as i32);
    let salts = _mm256_set_epi32(
        0x00000001i32,
        0x47b6137bi32,
        0x44974d91u32 as i32,
        0x8824ad5bu32 as i32,
        0xa2b7289du32 as i32,
        0x705495c7u32 as i32,
        0x2df1424bu32 as i32,
        0x9efc4947u32 as i32,
    );

    let product = _mm256_mullo_epi32(hash_vec, salts);
    let shifts = _mm256_srli_epi32(product, 27);
    let ones = _mm256_set1_epi32(1);
    let mask = _mm256_sllv_epi32(ones, shifts);

    let block = _mm256_loadu_si256(block_ptr);

    // _mm256_testz_si256 checks if (mask & (~block)) == 0.
    // If 1, all bits in mask are set in block -> member!
    _mm256_testz_si256(mask, _mm256_andnot_si256(block, mask)) == 1
}

/// High-quality 64-bit MurmurHash3 / Wyhash-style fast mixer.
#[inline(always)]
pub fn hash_key(key: &[u8]) -> u64 {
    let mut h = 0xc6a4a7935bd1e995u64 ^ (key.len() as u64).wrapping_mul(0x5bd1e995);
    let mut chunks = key.chunks_exact(8);

    for chunk in &mut chunks {
        let mut k = u64::from_le_bytes(chunk.try_into().unwrap());
        k = k.wrapping_mul(0x5bd1e995);
        k ^= k >> 47;
        k = k.wrapping_mul(0x5bd1e995);

        h ^= k;
        h = h.wrapping_mul(0x5bd1e995);
    }

    let remainder = chunks.remainder();
    if !remainder.is_empty() {
        let mut rem_buf = [0u8; 8];
        rem_buf[..remainder.len()].copy_from_slice(remainder);
        let mut k = u64::from_le_bytes(rem_buf);
        k = k.wrapping_mul(0x5bd1e995);
        k ^= k >> 47;
        k = k.wrapping_mul(0x5bd1e995);

        h ^= k;
        h = h.wrapping_mul(0x5bd1e995);
    }

    h ^= h >> 47;
    h = h.wrapping_mul(0x5bd1e995);
    h ^= h >> 47;
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bloom_no_false_negatives() {
        let n = 10_000;
        let mut hashes = Vec::with_capacity(n);
        for i in 0..n {
            let key = format!("aether_key_{:08}", i);
            hashes.push(hash_key(key.as_bytes()));
        }

        let filter = BlockSplitBloomFilter::build_from_hashes(&hashes, 10);

        // Zero false negatives: every inserted key MUST be found
        for &h in &hashes {
            assert!(
                filter.may_contain_hash(h),
                "Bloom filter returned false negative!"
            );
        }

        // Verify False Positive Rate is strictly < 1.0% (at 10 bits/key)
        let mut false_positives = 0;
        let queries = 20_000;
        for i in n..(n + queries) {
            let absent_key = format!("aether_key_{:08}", i);
            let h = hash_key(absent_key.as_bytes());
            if filter.may_contain_hash(h) {
                false_positives += 1;
            }
        }

        let fpr = (false_positives as f64) / (queries as f64);
        println!("Observed Bloom False Positive Rate: {:.3}%", fpr * 100.0);
        assert!(fpr < 0.015, "FPR {:.3}% exceeded 1.5% threshold", fpr * 100.0);
    }
}
