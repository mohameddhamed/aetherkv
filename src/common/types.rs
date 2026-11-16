use std::cmp::Ordering;

pub type SequenceNumber = u64;

#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ValueType {
    Put = 1,
    Delete = 2,
}

impl ValueType {
    #[inline(always)]
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            1 => Some(Self::Put),
            2 => Some(Self::Delete),
            _ => None,
        }
    }
}

/// An InternalKey bundles user key, sequence number, and value type.
/// Wire format: [UserKey bytes][8 bytes: (seq_num << 8) | (value_type as u64)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalKey {
    user_key: Vec<u8>,
    seq_num: SequenceNumber,
    value_type: ValueType,
}

impl InternalKey {
    #[inline]
    pub fn new(user_key: Vec<u8>, seq_num: SequenceNumber, value_type: ValueType) -> Self {
        Self {
            user_key,
            seq_num,
            value_type,
        }
    }

    #[inline(always)]
    pub fn user_key(&self) -> &[u8] {
        &self.user_key
    }

    #[inline(always)]
    pub fn seq_num(&self) -> SequenceNumber {
        self.seq_num
    }

    #[inline(always)]
    pub fn value_type(&self) -> ValueType {
        self.value_type
    }

    /// Encode internal key to raw bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.user_key.len() + 8);
        buf.extend_from_slice(&self.user_key);
        let tag = (self.seq_num << 8) | (self.value_type as u64);
        buf.extend_from_slice(&tag.to_be_bytes());
        buf
    }

    /// Decode internal key from raw bytes.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 8 {
            return None;
        }
        let user_key_len = bytes.len() - 8;
        let user_key = bytes[..user_key_len].to_vec();
        let mut tag_bytes = [0u8; 8];
        tag_bytes.copy_from_slice(&bytes[user_key_len..]);
        let tag = u64::from_be_bytes(tag_bytes);
        let seq_num = tag >> 8;
        let value_type = ValueType::from_u8((tag & 0xFF) as u8)?;
        Some(Self {
            user_key,
            seq_num,
            value_type,
        })
    }
}

/// Comparator for internal keys:
/// 1. User key ascending
/// 2. Sequence number descending (newest versions first)
/// 3. Value type descending
#[inline]
pub fn compare_internal_keys(a: &[u8], b: &[u8]) -> Ordering {
    if a.len() < 8 || b.len() < 8 {
        return a.cmp(b);
    }
    let ukey_a = &a[..a.len() - 8];
    let ukey_b = &b[..b.len() - 8];
    match ukey_a.cmp(ukey_b) {
        Ordering::Equal => {
            let tag_a = u64::from_be_bytes(a[a.len() - 8..].try_into().unwrap());
            let tag_b = u64::from_be_bytes(b[b.len() - 8..].try_into().unwrap());
            // Higher sequence number comes first (descending)
            tag_b.cmp(&tag_a)
        }
        ord => ord,
    }
}

/// Extract user key slice from internal key slice.
#[inline(always)]
pub fn extract_user_key(internal_key: &[u8]) -> &[u8] {
    if internal_key.len() >= 8 {
        &internal_key[..internal_key.len() - 8]
    } else {
        internal_key
    }
}

/// Extract sequence number and value type from internal key slice.
#[inline(always)]
pub fn extract_tag(internal_key: &[u8]) -> Option<(SequenceNumber, ValueType)> {
    if internal_key.len() < 8 {
        return None;
    }
    let tag = u64::from_be_bytes(internal_key[internal_key.len() - 8..].try_into().ok()?);
    let seq = tag >> 8;
    let vt = ValueType::from_u8((tag & 0xFF) as u8)?;
    Some((seq, vt))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_internal_key_encoding_decoding() {
        let key = InternalKey::new(b"test_key".to_vec(), 42, ValueType::Put);
        let encoded = key.encode();
        let decoded = InternalKey::decode(&encoded).expect("decode failed");
        assert_eq!(decoded, key);
        assert_eq!(extract_user_key(&encoded), b"test_key");
        assert_eq!(extract_tag(&encoded), Some((42, ValueType::Put)));
    }

    #[test]
    fn test_internal_key_ordering() {
        let k1_old = InternalKey::new(b"key".to_vec(), 1, ValueType::Put).encode();
        let k1_new = InternalKey::new(b"key".to_vec(), 2, ValueType::Put).encode();
        let k2 = InternalKey::new(b"key2".to_vec(), 1, ValueType::Put).encode();

        // Newer version of the same user key comes BEFORE older version
        assert_eq!(compare_internal_keys(&k1_new, &k1_old), Ordering::Less);
        // "key" comes before "key2"
        assert_eq!(compare_internal_keys(&k1_old, &k2), Ordering::Less);
        assert_eq!(compare_internal_keys(&k1_new, &k2), Ordering::Less);
    }
}
