pub mod batch;
pub mod recovery;

pub use batch::{BatchOp, WriteBatch};
pub use recovery::RecoveryManager;

use crate::common::{AetherError, Result, SequenceNumber, ValueType};
use crate::compaction::{BackpressureController, CompactionExecutor, CompactionPicker};
use crate::manifest::{FileMetaData, VersionEdit, VersionSet};
use crate::memtable::{MemTable, MemTableEntry, DEFAULT_ARENA_CAPACITY};
use crate::sstable::TableBuilder;
use crate::wal::{WalWriter, WAL_SEGMENT_SIZE};
use parking_lot::{Mutex, RwLock};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Options {
    pub db_dir: PathBuf,
    pub arena_capacity: usize,
    pub wal_segment_size: usize,
    pub sync_wal_on_write: bool,
}

impl Options {
    pub fn new<P: AsRef<Path>>(db_dir: P) -> Self {
        Self {
            db_dir: db_dir.as_ref().to_path_buf(),
            arena_capacity: DEFAULT_ARENA_CAPACITY,
            wal_segment_size: WAL_SEGMENT_SIZE,
            sync_wal_on_write: false,
        }
    }
}

pub struct Engine {
    options: Options,
    active_mem: RwLock<Arc<MemTable>>,
    imm_mems: Mutex<VecDeque<Arc<MemTable>>>,
    wal: Mutex<WalWriter>,
    version_set: Arc<VersionSet>,
    backpressure: Arc<BackpressureController>,
    compaction_executor: Arc<CompactionExecutor>,
    next_seq: AtomicU64,
    _shutting_down: AtomicBool,
}

impl Engine {
    /// Open or create an AetherKV database at the specified directory.
    pub fn open(options: Options) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&options.db_dir)?;

        let version_set = Arc::new(VersionSet::recover(&options.db_dir)?);
        let backpressure = Arc::new(BackpressureController::new());
        let compaction_executor = Arc::new(CompactionExecutor::new(
            &options.db_dir,
            Arc::clone(&version_set),
            Arc::clone(&backpressure),
        ));

        // Create initial active MemTable
        let active_mem = Arc::new(MemTable::new(1, options.arena_capacity));

        // Perform crash recovery if existing WAL files are present
        let wal_max_seq = RecoveryManager::recover_wal(&options.db_dir, &active_mem)?;
        let max_seq = wal_max_seq.max(version_set.max_sequence.load(Ordering::SeqCst));

        // Initialize WAL writer for current segment
        let next_wal_id = version_set.next_file_number();
        let wal = WalWriter::new(&options.db_dir, next_wal_id)?;

        let engine = Arc::new(Self {
            options,
            active_mem: RwLock::new(active_mem),
            imm_mems: Mutex::new(VecDeque::new()),
            wal: Mutex::new(wal),
            version_set,
            backpressure,
            compaction_executor,
            next_seq: AtomicU64::new(max_seq + 1),
            _shutting_down: AtomicBool::new(false),
        });

        Ok(engine)
    }

    #[inline(always)]
    pub fn next_sequence(&self) -> SequenceNumber {
        self.next_seq.fetch_add(1, Ordering::SeqCst)
    }

    /// Insert or update a single key-value pair.
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let mut batch = WriteBatch::new();
        batch.put(key, value);
        self.write_batch(&batch)
    }

    /// Delete a single key (appends an LSM tombstone).
    pub fn delete(&self, key: &[u8]) -> Result<()> {
        let mut batch = WriteBatch::new();
        batch.delete(key);
        self.write_batch(&batch)
    }

    /// Atomically write a batch of operations.
    pub fn write_batch(&self, batch: &WriteBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        // 1. Dynamic L0 Backpressure: throttle if L0 files are accumulating
        let l0_count = self.version_set.current().num_files_at_level(0);
        self.backpressure.throttle(l0_count);

        // 2. Check if active MemTable capacity is exhausted or WAL is near capacity
        {
            let need_freeze = {
                let active = self.active_mem.read();
                active.approximate_size() + batch.size_in_bytes() + 4096 > active.arena().capacity()
                    || self.wal.lock().is_near_capacity()
            };
            if need_freeze {
                self.freeze_active_memtable()?;
            }
        }

        // 3. Append to WAL
        loop {
            let wal = self.wal.lock();
            let mut wal_exhausted = false;

            for op in batch.ops() {
                let seq = self.next_sequence();
                let res = match op {
                    BatchOp::Put { key, value } => wal.append_put(seq, key, value),
                    BatchOp::Delete { key } => wal.append_delete(seq, key),
                };

                if let Err(AetherError::WalExhausted) = res {
                    wal_exhausted = true;
                    break;
                } else {
                    res?;
                }
            }

            if wal_exhausted {
                drop(wal);
                self.freeze_active_memtable()?;
                continue;
            }

            if self.options.sync_wal_on_write {
                wal.sync()?;
            }
            break;
        }

        // 4. Insert into active MemTable (Lock-Free SkipList)
        {
            let mut active = self.active_mem.read();
            for op in batch.ops() {
                let seq = self.next_seq.load(Ordering::Relaxed) - 1;
                let inserted = match op {
                    BatchOp::Put { key, value } => active.insert(key, value, seq, ValueType::Put),
                    BatchOp::Delete { key } => active.insert(key, &[], seq, ValueType::Delete),
                };

                if !inserted {
                    drop(active);
                    self.freeze_active_memtable()?;
                    active = self.active_mem.read();
                    match op {
                        BatchOp::Put { key, value } => {
                            active.insert(key, value, seq, ValueType::Put);
                        }
                        BatchOp::Delete { key } => {
                            active.insert(key, &[], seq, ValueType::Delete);
                        }
                    }
                }
            }
        }

        // 5. Trigger background compaction check if needed
        self.maybe_schedule_compaction();

        Ok(())
    }

    /// Point lookup for a user key at the latest snapshot.
    pub fn get(&self, user_key: &[u8]) -> Result<Option<Vec<u8>>> {
        let snapshot_seq = self.next_seq.load(Ordering::Acquire);

        // 1. Query Active MemTable
        {
            let active = self.active_mem.read();
            if let Some(entry) = active.get(user_key, snapshot_seq) {
                return match entry {
                    MemTableEntry::Value(v) => Ok(Some(v)),
                    MemTableEntry::Deletion => Ok(None),
                };
            }
        }

        // 2. Query Immutable MemTables (newest to oldest)
        {
            let imms = self.imm_mems.lock();
            for imm in imms.iter() {
                if let Some(entry) = imm.get(user_key, snapshot_seq) {
                    return match entry {
                        MemTableEntry::Value(v) => Ok(Some(v)),
                        MemTableEntry::Deletion => Ok(None),
                    };
                }
            }
        }

        // 3. Query On-Disk SSTable Hierarchy
        let version = self.version_set.current();
        if let Some((vt, val)) = version.get(user_key, snapshot_seq) {
            match vt {
                ValueType::Put => Ok(Some(val)),
                ValueType::Delete => Ok(None),
            }
        } else {
            Ok(None)
        }
    }

    /// Atomically freeze active MemTable and swap with a new 64MB arena.
    fn freeze_active_memtable(&self) -> Result<()> {
        let mut active_guard = self.active_mem.write();

        let old_mem = active_guard.clone();
        let new_id = old_mem.id() + 1;
        let new_mem = Arc::new(MemTable::new(new_id, self.options.arena_capacity));
        *active_guard = new_mem;

        // Push old MemTable to immutable queue
        {
            let mut imms = self.imm_mems.lock();
            imms.push_back(old_mem.clone());
        }

        // Rotate WAL segment
        {
            let mut wal_guard = self.wal.lock();
            let _ = wal_guard.flush();
            let new_wal_id = self.version_set.next_file_number();
            *wal_guard = WalWriter::new(&self.options.db_dir, new_wal_id)?;
        }

        // Trigger asynchronous flush of the frozen MemTable
        self.flush_memtable(old_mem)?;

        Ok(())
    }

    /// Flush a frozen MemTable to a new L0 SSTable file, then execute $O(1)$ drop of the 64MB Arena.
    pub fn flush_memtable(&self, mem: Arc<MemTable>) -> Result<()> {
        let new_file_id = self.version_set.next_file_number();
        let sst_path = self.options.db_dir.join(format!("{:08}.sst", new_file_id));
        let mut builder = TableBuilder::new(&sst_path)?;

        let mut smallest_key = None;
        let mut largest_key = None;

        for (_, ikey, val) in mem.iter() {
            if smallest_key.is_none() {
                smallest_key = Some(ikey.clone());
            }
            largest_key = Some(ikey.clone());
            builder.add(&ikey, &val)?;
        }

        if let (Some(smallest), Some(largest)) = (smallest_key, largest_key) {
            let (file_size, _) = builder.finish()?;

            let mut edit = VersionEdit::new();
            edit.add_file(
                0,
                FileMetaData {
                    file_id: new_file_id,
                    file_size,
                    smallest_key: smallest,
                    largest_key: largest,
                    level: 0,
                },
            );

            self.version_set.log_and_apply(&edit)?;
        }

        // Remove from immutable list
        {
            let mut imms = self.imm_mems.lock();
            imms.retain(|m| m.id() != mem.id());
        }

        // At this point, once any concurrent readers drop references to `mem`,
        // Arc reference count drops to 0 and the 64MB AtomicArena is freed in $O(1)$!
        Ok(())
    }

    /// Flush all in-memory data (active + immutable) to disk.
    pub fn flush(&self) -> Result<()> {
        self.wal.lock().flush()?;

        let active = {
            let mut active_guard = self.active_mem.write();
            let old = active_guard.clone();
            let new_id = old.id() + 1;
            *active_guard = Arc::new(MemTable::new(new_id, self.options.arena_capacity));
            old
        };

        self.flush_memtable(active)?;

        loop {
            let next_imm = {
                let mut imms = self.imm_mems.lock();
                imms.pop_front()
            };

            match next_imm {
                Some(imm) => self.flush_memtable(imm)?,
                None => break,
            }
        }

        Ok(())
    }

    /// Check and trigger compaction if any level meets its score threshold.
    pub fn maybe_schedule_compaction(&self) {
        let version = self.version_set.current();
        if let Some(task) = CompactionPicker::pick_compaction(&version) {
            let _ = self.compaction_executor.execute(task);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_engine_put_get_delete() {
        let dir = tempdir().unwrap();
        let opts = Options::new(dir.path());
        let engine = Engine::open(opts).unwrap();

        // Put keys
        engine.put(b"user1", b"alice").unwrap();
        engine.put(b"user2", b"bob").unwrap();

        // Get keys
        assert_eq!(engine.get(b"user1").unwrap(), Some(b"alice".to_vec()));
        assert_eq!(engine.get(b"user2").unwrap(), Some(b"bob".to_vec()));
        assert_eq!(engine.get(b"user3").unwrap(), None);

        // Delete key
        engine.delete(b"user1").unwrap();
        assert_eq!(engine.get(b"user1").unwrap(), None);
        assert_eq!(engine.get(b"user2").unwrap(), Some(b"bob".to_vec()));
    }

    #[test]
    fn test_engine_flush_and_persistence() {
        let dir = tempdir().unwrap();
        let opts = Options::new(dir.path());

        {
            let engine = Engine::open(opts.clone()).unwrap();
            for i in 0..100 {
                let k = format!("key_{:04}", i);
                let v = format!("val_{:04}", i);
                engine.put(k.as_bytes(), v.as_bytes()).unwrap();
            }
            // Force flush to SSTable
            engine.flush().unwrap();
        }

        // Re-open engine: verify data persisted in SSTables
        {
            let engine = Engine::open(opts).unwrap();
            for i in 0..100 {
                let k = format!("key_{:04}", i);
                let expected_v = format!("val_{:04}", i);
                let val = engine.get(k.as_bytes()).unwrap();
                assert_eq!(val, Some(expected_v.into_bytes()));
            }
        }
    }
}
