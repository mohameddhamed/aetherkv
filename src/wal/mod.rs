pub mod ring_buffer;
pub mod segment;
pub mod uio;

pub use ring_buffer::{
    decode_wal_record, encode_wal_record, SectorAlignedBuffer, WalFrameHeader, WalRecordType,
    FRAME_HEADER_SIZE, SECTOR_SIZE,
};
pub use segment::{WalSegment, WAL_SEGMENT_SIZE};
pub use uio::UioDriver;

use crate::common::{crc32c, AetherError, Result, SequenceNumber, ValueType};
use parking_lot::Mutex;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct WalWriter {
    segment: Arc<WalSegment>,
    buffer: Mutex<SectorAlignedBuffer>,
    uio: Mutex<UioDriver>,
    _dir: PathBuf,
    current_segment_id: AtomicU64,
}

impl WalWriter {
    pub fn new<P: AsRef<Path>>(dir: P, segment_id: u64) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;

        let segment = Arc::new(WalSegment::create(&dir, segment_id, WAL_SEGMENT_SIZE)?);
        let buffer = Mutex::new(SectorAlignedBuffer::new(64 * 1024)); // 64KB write combining buffer
        let uio = Mutex::new(UioDriver::new(64)?);

        Ok(Self {
            segment,
            buffer,
            uio,
            _dir: dir,
            current_segment_id: AtomicU64::new(segment_id),
        })
    }

    #[inline(always)]
    pub fn current_segment_id(&self) -> u64 {
        self.current_segment_id.load(Ordering::Relaxed)
    }

    #[inline(always)]
    pub fn is_near_capacity(&self) -> bool {
        self.segment.remaining_bytes() < 64 * 1024
    }

    /// Append a single operation to the WAL.
    pub fn append(
        &self,
        seq_num: SequenceNumber,
        record_type: WalRecordType,
        payload: &[u8],
    ) -> Result<()> {
        let mut buf = self.buffer.lock();

        let frame_size = FRAME_HEADER_SIZE + payload.len();
        if frame_size > buf.remaining() {
            self.flush_locked(&mut buf)?;
        }

        if frame_size > buf.remaining() {
            return Err(AetherError::WalExhausted);
        }

        buf.append_record(seq_num, record_type, payload)?;
        Ok(())
    }

    /// Append a Put operation.
    pub fn append_put(&self, seq_num: SequenceNumber, key: &[u8], value: &[u8]) -> Result<()> {
        let payload = encode_wal_record(key, value, ValueType::Put);
        self.append(seq_num, WalRecordType::Put, &payload)
    }

    /// Append a Delete tombstone operation.
    pub fn append_delete(&self, seq_num: SequenceNumber, key: &[u8]) -> Result<()> {
        let payload = encode_wal_record(key, &[], ValueType::Delete);
        self.append(seq_num, WalRecordType::Delete, &payload)
    }

    /// Flush the write-combining buffer to disk using sector-aligned Direct I/O.
    pub fn flush(&self) -> Result<()> {
        let mut buf = self.buffer.lock();
        self.flush_locked(&mut buf)
    }

    fn flush_locked(&self, buf: &mut SectorAlignedBuffer) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }

        let bytes_to_write = buf.len();
        // Align up to next 4096-byte sector boundary for O_DIRECT
        let aligned_write_len = (bytes_to_write + SECTOR_SIZE - 1) & !(SECTOR_SIZE - 1);

        // Reserve space in pre-allocated segment
        let offset = self.segment.reserve_space(aligned_write_len)?;

        // Ensure trailing bytes in sector are zeroed
        if aligned_write_len > bytes_to_write {
            let slice = buf.as_raw_mut_slice();
            slice[bytes_to_write..aligned_write_len].fill(0);
        }

        let mut uio = self.uio.lock();
        uio.write_at(
            self.segment.raw_fd(),
            offset,
            &buf.as_raw_slice()[..aligned_write_len],
        )?;

        buf.clear();
        Ok(())
    }

    /// Synchronize WAL data to persistent media.
    pub fn sync(&self) -> Result<()> {
        self.flush()?;
        let uio = self.uio.lock();
        uio.sync_data(self.segment.raw_fd())?;
        Ok(())
    }
}

/// WAL Reader for crash recovery.
pub struct WalReader {
    file: File,
}

impl WalReader {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = File::open(path)?;
        Ok(Self { file })
    }

    /// Recover all valid frames from the WAL file.
    /// Traversal halts cleanly when all-zero preallocated sectors are encountered.
    pub fn recover_all(&mut self) -> Result<Vec<(WalFrameHeader, Vec<u8>)>> {
        let mut records = Vec::new();
        let mut header_buf = [0u8; FRAME_HEADER_SIZE];

        loop {
            match self.file.read_exact(&mut header_buf) {
                Ok(_) => {
                    // Check for preallocated zero-filled space (marks end of valid logs)
                    if header_buf.iter().all(|&b| b == 0) {
                        break;
                    }

                    let header = WalFrameHeader::from_bytes(&header_buf);
                    if header.length == 0 {
                        break;
                    }

                    let mut payload = vec![0u8; header.length as usize];
                    self.file.read_exact(&mut payload)?;

                    // Verify hardware CRC32C
                    let actual_crc = crc32c(&payload);
                    if actual_crc != header.crc32c {
                        return Err(AetherError::FrameCrcMismatch {
                            expected: header.crc32c,
                            actual: actual_crc,
                        });
                    }

                    records.push((header, payload));
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    break;
                }
                Err(e) => return Err(AetherError::Io(e)),
            }
        }

        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_wal_write_and_recovery() {
        let dir = tempdir().unwrap();
        let wal = WalWriter::new(dir.path(), 1).unwrap();

        wal.append_put(1, b"key1", b"value1").unwrap();
        wal.append_put(2, b"key2", b"value2").unwrap();
        wal.append_delete(3, b"key1").unwrap();
        wal.sync().unwrap();

        let wal_path = dir.path().join("00000001.wal");
        let mut reader = WalReader::open(&wal_path).unwrap();
        let records = reader.recover_all().unwrap();

        assert_eq!(records.len(), 3);

        // Verify Record 1
        assert_eq!(records[0].0.seq_num, 1);
        let (k1, v1, vt1) = decode_wal_record(&records[0].1).unwrap();
        assert_eq!(k1, b"key1");
        assert_eq!(v1, b"value1");
        assert_eq!(vt1, ValueType::Put);

        // Verify Record 2
        assert_eq!(records[1].0.seq_num, 2);
        let (k2, v2, vt2) = decode_wal_record(&records[1].1).unwrap();
        assert_eq!(k2, b"key2");
        assert_eq!(v2, b"value2");
        assert_eq!(vt2, ValueType::Put);

        // Verify Record 3
        assert_eq!(records[2].0.seq_num, 3);
        let (k3, _v3, vt3) = decode_wal_record(&records[2].1).unwrap();
        assert_eq!(k3, b"key1");
        assert_eq!(vt3, ValueType::Delete);
    }
}
