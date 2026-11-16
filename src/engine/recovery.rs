use crate::common::Result;
use crate::memtable::MemTable;
use crate::wal::{decode_wal_record, WalReader, WalRecordType};
use std::path::Path;

pub struct RecoveryManager;

impl RecoveryManager {
    /// Recover state from all WAL segments in the database directory.
    /// Replays all validated mutations into the active MemTable.
    /// Returns the highest recovered Log Sequence Number (LSN).
    pub fn recover_wal<P: AsRef<Path>>(db_dir: P, active_mem: &MemTable) -> Result<u64> {
        let db_dir = db_dir.as_ref();
        let mut wal_files = Vec::new();

        if let Ok(entries) = std::fs::read_dir(db_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) == Some("wal") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        if let Ok(id) = stem.parse::<u64>() {
                            wal_files.push((id, path));
                        }
                    }
                }
            }
        }

        // Sort WAL files in ascending segment ID order
        wal_files.sort_by_key(|&(id, _)| id);

        let mut max_seq = 0;

        for (_, path) in wal_files {
            let mut reader = WalReader::open(&path)?;
            let frames = reader.recover_all()?;

            for (header, payload) in frames {
                max_seq = max_seq.max(header.seq_num);

                match WalRecordType::from_u8(header.record_type) {
                    Some(WalRecordType::Put) | Some(WalRecordType::Delete) => {
                        let (k, v, vt) = decode_wal_record(&payload)?;
                        active_mem.insert(&k, &v, header.seq_num, vt);
                    }
                    Some(WalRecordType::Batch) => {
                        // Batches can be decoded as multiple records
                        let mut offset = 0;
                        while offset < payload.len() {
                            if offset + 4 > payload.len() {
                                break;
                            }
                            let rec_len =
                                u32::from_le_bytes(payload[offset..offset + 4].try_into().unwrap())
                                    as usize;
                            offset += 4;
                            if offset + rec_len > payload.len() {
                                break;
                            }
                            let rec_payload = &payload[offset..offset + rec_len];
                            offset += rec_len;

                            if let Ok((k, v, vt)) = decode_wal_record(rec_payload) {
                                active_mem.insert(&k, &v, header.seq_num, vt);
                            }
                        }
                    }
                    None => {}
                }
            }
        }

        Ok(max_seq)
    }
}
