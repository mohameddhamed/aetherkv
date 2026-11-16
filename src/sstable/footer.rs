use crate::common::{AetherError, Result};

pub const TABLE_MAGIC_NUMBER: u64 = 0x4145_5448_4552_4B56; // "AETHERKV" in ASCII
pub const FOOTER_ENCODED_SIZE: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableFooter {
    pub index_offset: u64,
    pub index_size: u64,
    pub bloom_offset: u64,
    pub bloom_size: u64,
    pub num_entries: u64,
    pub magic: u64,
}

impl TableFooter {
    pub fn new(
        index_offset: u64,
        index_size: u64,
        bloom_offset: u64,
        bloom_size: u64,
        num_entries: u64,
    ) -> Self {
        Self {
            index_offset,
            index_size,
            bloom_offset,
            bloom_size,
            num_entries,
            magic: TABLE_MAGIC_NUMBER,
        }
    }

    pub fn encode(&self) -> [u8; FOOTER_ENCODED_SIZE] {
        let mut buf = [0u8; FOOTER_ENCODED_SIZE];
        buf[0..8].copy_from_slice(&self.index_offset.to_le_bytes());
        buf[8..16].copy_from_slice(&self.index_size.to_le_bytes());
        buf[16..24].copy_from_slice(&self.bloom_offset.to_le_bytes());
        buf[24..32].copy_from_slice(&self.bloom_size.to_le_bytes());
        buf[32..40].copy_from_slice(&self.num_entries.to_le_bytes());
        buf[40..48].copy_from_slice(&self.magic.to_le_bytes());
        buf
    }

    pub fn decode(buf: &[u8; FOOTER_ENCODED_SIZE]) -> Result<Self> {
        let index_offset = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let index_size = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let bloom_offset = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        let bloom_size = u64::from_le_bytes(buf[24..32].try_into().unwrap());
        let num_entries = u64::from_le_bytes(buf[32..40].try_into().unwrap());
        let magic = u64::from_le_bytes(buf[40..48].try_into().unwrap());

        if magic != TABLE_MAGIC_NUMBER {
            return Err(AetherError::InvalidMagicNumber);
        }

        Ok(Self {
            index_offset,
            index_size,
            bloom_offset,
            bloom_size,
            num_entries,
            magic,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_footer_encode_decode() {
        let footer = TableFooter::new(100, 200, 300, 400, 500);
        let encoded = footer.encode();
        assert_eq!(encoded.len(), FOOTER_ENCODED_SIZE);

        let decoded = TableFooter::decode(&encoded).expect("valid footer");
        assert_eq!(decoded, footer);
    }
}
