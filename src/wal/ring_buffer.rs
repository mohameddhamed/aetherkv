use crate::common::{crc32c, AetherError, Result, SequenceNumber, ValueType};
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ptr::NonNull;

pub const SECTOR_SIZE: usize = 4096;
pub const FRAME_HEADER_SIZE: usize = 32;

#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum WalRecordType {
    Put = 1,
    Delete = 2,
    Batch = 3,
}

impl WalRecordType {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            1 => Some(Self::Put),
            2 => Some(Self::Delete),
            3 => Some(Self::Batch),
            _ => None,
        }
    }
}

/// 32-byte header strictly aligned to eliminate torn reads and optimize SSE4.2 CRC32C.
#[repr(C, align(32))]
#[derive(Debug, Copy, Clone)]
pub struct WalFrameHeader {
    pub crc32c: u32,
    pub length: u32,
    pub seq_num: u64,
    pub record_type: u8,
    pub _reserved: [u8; 15],
}

impl WalFrameHeader {
    pub fn new(crc32c: u32, length: u32, seq_num: u64, record_type: WalRecordType) -> Self {
        Self {
            crc32c,
            length,
            seq_num,
            record_type: record_type as u8,
            _reserved: [0u8; 15],
        }
    }

    pub fn to_bytes(&self) -> [u8; FRAME_HEADER_SIZE] {
        let mut buf = [0u8; FRAME_HEADER_SIZE];
        buf[0..4].copy_from_slice(&self.crc32c.to_le_bytes());
        buf[4..8].copy_from_slice(&self.length.to_le_bytes());
        buf[8..16].copy_from_slice(&self.seq_num.to_le_bytes());
        buf[16] = self.record_type;
        buf
    }

    pub fn from_bytes(buf: &[u8; FRAME_HEADER_SIZE]) -> Self {
        let crc32c = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let length = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        let seq_num = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let record_type = buf[16];
        let mut _reserved = [0u8; 15];
        _reserved.copy_from_slice(&buf[17..32]);

        Self {
            crc32c,
            length,
            seq_num,
            record_type,
            _reserved,
        }
    }
}

/// Sector-aligned write combining buffer (4096-byte aligned for Direct I/O).
pub struct SectorAlignedBuffer {
    ptr: NonNull<u8>,
    capacity: usize,
    len: usize,
}

unsafe impl Send for SectorAlignedBuffer {}
unsafe impl Sync for SectorAlignedBuffer {}

impl SectorAlignedBuffer {
    pub fn new(capacity: usize) -> Self {
        let aligned_cap = (capacity + SECTOR_SIZE - 1) & !(SECTOR_SIZE - 1);
        let layout = Layout::from_size_align(aligned_cap, SECTOR_SIZE).expect("valid layout");
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).expect("allocation failed");

        Self {
            ptr,
            capacity: aligned_cap,
            len: 0,
        }
    }

    #[inline(always)]
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    #[inline(always)]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline(always)]
    pub fn remaining(&self) -> usize {
        self.capacity - self.len
    }

    #[inline(always)]
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    #[inline(always)]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    #[inline(always)]
    pub fn as_raw_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.capacity) }
    }

    #[inline(always)]
    pub fn as_raw_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.capacity) }
    }

    /// Append raw bytes to the aligned buffer.
    pub fn append(&mut self, data: &[u8]) -> usize {
        let to_copy = data.len().min(self.remaining());
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.ptr.as_ptr().add(self.len),
                to_copy,
            );
        }
        self.len += to_copy;
        to_copy
    }

    /// Append a framed record (32-byte header + payload).
    pub fn append_record(
        &mut self,
        seq_num: SequenceNumber,
        record_type: WalRecordType,
        payload: &[u8],
    ) -> Result<()> {
        let frame_len = FRAME_HEADER_SIZE + payload.len();
        if frame_len > self.remaining() {
            return Err(AetherError::WalExhausted);
        }

        let checksum = crc32c(payload);
        let header = WalFrameHeader::new(checksum, payload.len() as u32, seq_num, record_type);
        let header_bytes = header.to_bytes();

        self.append(&header_bytes);
        self.append(payload);
        Ok(())
    }

    /// Clear buffer contents and reset length.
    pub fn clear(&mut self) {
        unsafe {
            std::ptr::write_bytes(self.ptr.as_ptr(), 0, self.len);
        }
        self.len = 0;
    }
}

impl Drop for SectorAlignedBuffer {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.capacity, SECTOR_SIZE).expect("valid layout");
        unsafe {
            dealloc(self.ptr.as_ptr(), layout);
        }
    }
}

/// Helper to serialize a key-value record into a WAL payload.
pub fn encode_wal_record(key: &[u8], value: &[u8], value_type: ValueType) -> Vec<u8> {
    let mut payload = Vec::with_capacity(9 + key.len() + value.len());
    payload.push(value_type as u8);
    payload.extend_from_slice(&(key.len() as u32).to_le_bytes());
    payload.extend_from_slice(key);
    payload.extend_from_slice(&(value.len() as u32).to_le_bytes());
    payload.extend_from_slice(value);
    payload
}

/// Helper to decode a WAL payload into key, value, and value type.
pub fn decode_wal_record(payload: &[u8]) -> Result<(Vec<u8>, Vec<u8>, ValueType)> {
    if payload.len() < 9 {
        return Err(AetherError::Corruption("WAL record payload too short".into()));
    }

    let vt = ValueType::from_u8(payload[0])
        .ok_or_else(|| AetherError::InvalidRecordType(payload[0]))?;

    let key_len = u32::from_le_bytes(payload[1..5].try_into().unwrap()) as usize;
    if payload.len() < 5 + key_len + 4 {
        return Err(AetherError::Corruption("Corrupted WAL record key length".into()));
    }

    let key = payload[5..5 + key_len].to_vec();
    let val_offset = 5 + key_len;
    let val_len = u32::from_le_bytes(payload[val_offset..val_offset + 4].try_into().unwrap()) as usize;

    if payload.len() < val_offset + 4 + val_len {
        return Err(AetherError::Corruption("Corrupted WAL record value length".into()));
    }

    let val = payload[val_offset + 4..val_offset + 4 + val_len].to_vec();
    Ok((key, val, vt))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sector_aligned_buffer_alignment() {
        let buf = SectorAlignedBuffer::new(8192);
        assert_eq!(buf.as_ptr() as usize % SECTOR_SIZE, 0);
        assert_eq!(buf.capacity(), 8192);
    }

    #[test]
    fn test_wal_frame_header_serialization() {
        let header = WalFrameHeader::new(0x12345678, 128, 42, WalRecordType::Put);
        let bytes = header.to_bytes();
        let decoded = WalFrameHeader::from_bytes(&bytes);
        assert_eq!(decoded.crc32c, 0x12345678);
        assert_eq!(decoded.length, 128);
        assert_eq!(decoded.seq_num, 42);
        assert_eq!(decoded.record_type, WalRecordType::Put as u8);
    }

    #[test]
    fn test_wal_record_encode_decode() {
        let payload = encode_wal_record(b"hello", b"world", ValueType::Put);
        let (k, v, vt) = decode_wal_record(&payload).unwrap();
        assert_eq!(k, b"hello");
        assert_eq!(v, b"world");
        assert_eq!(vt, ValueType::Put);
    }
}
