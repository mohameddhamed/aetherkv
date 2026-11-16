pub mod version;

pub use version::{FileMetaData, Version, VersionEdit, NUM_LEVELS};

use crate::common::Result;
use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct VersionSet {
    _db_dir: PathBuf,
    current: RwLock<Arc<Version>>,
    next_file_number: AtomicU64,
    pub max_sequence: AtomicU64,
}

impl VersionSet {
    pub fn new<P: AsRef<Path>>(db_dir: P) -> Self {
        let db_dir = db_dir.as_ref().to_path_buf();
        let initial_version = Arc::new(Version::new(&db_dir));

        Self {
            _db_dir: db_dir,
            current: RwLock::new(initial_version),
            next_file_number: AtomicU64::new(1),
            max_sequence: AtomicU64::new(0),
        }
    }

    /// Recover version state by scanning existing SSTables in the database directory.
    pub fn recover<P: AsRef<Path>>(db_dir: P) -> Result<Self> {
        let db_dir = db_dir.as_ref().to_path_buf();
        let vset = Self::new(&db_dir);

        let mut max_file_id = 0;
        let mut max_seq = 0;
        let mut sst_files = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&db_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("sst") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        if let Ok(id) = stem.parse::<u64>() {
                            max_file_id = max_file_id.max(id);
                            sst_files.push((id, path));
                        }
                    }
                }
            }
        }

        // Sort SSTable files by ID
        sst_files.sort_by_key(|&(id, _)| id);

        let mut edit = VersionEdit::new();
        for (id, path) in sst_files {
            if let Ok(reader) = crate::sstable::TableReader::open(&path) {
                let mut it = reader.iter();
                if let Some((first_key, _)) = it.next() {
                    if let Some((seq, _)) = crate::common::extract_tag(&first_key) {
                        max_seq = max_seq.max(seq);
                    }
                    let mut last_key = first_key.clone();
                    for (k, _) in it {
                        if let Some((seq, _)) = crate::common::extract_tag(&k) {
                            max_seq = max_seq.max(seq);
                        }
                        last_key = k;
                    }

                    edit.add_file(
                        0,
                        FileMetaData {
                            file_id: id,
                            file_size: reader.file_size() as u64,
                            smallest_key: first_key,
                            largest_key: last_key,
                            level: 0,
                        },
                    );
                }
            }
        }

        vset.log_and_apply(&edit)?;
        vset.next_file_number.store(max_file_id + 1, Ordering::SeqCst);
        vset.max_sequence.store(max_seq, Ordering::SeqCst);
        Ok(vset)
    }

    /// Read-only snapshot of current version.
    #[inline(always)]
    pub fn current(&self) -> Arc<Version> {
        self.current.read().clone()
    }

    /// Atomically allocate next monotonic file number.
    #[inline(always)]
    pub fn next_file_number(&self) -> u64 {
        self.next_file_number.fetch_add(1, Ordering::SeqCst)
    }

    /// Apply delta modification to current version and atomically swap to new version.
    pub fn log_and_apply(&self, edit: &VersionEdit) -> Result<Arc<Version>> {
        let mut guard = self.current.write();
        let new_version = Arc::new(guard.apply(edit));
        *guard = new_version.clone();
        Ok(new_version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_version_set_delta_application() {
        let dir = tempdir().unwrap();
        let vset = VersionSet::new(dir.path());

        let mut edit = VersionEdit::new();
        edit.add_file(
            0,
            FileMetaData {
                file_id: 1,
                file_size: 4096,
                smallest_key: b"a".to_vec(),
                largest_key: b"z".to_vec(),
                level: 0,
            },
        );

        let v1 = vset.log_and_apply(&edit).unwrap();
        assert_eq!(v1.num_files_at_level(0), 1);
        assert_eq!(v1.total_level_size(0), 4096);

        // Delete file in next version
        let mut edit2 = VersionEdit::new();
        edit2.delete_file(0, 1);
        let v2 = vset.log_and_apply(&edit2).unwrap();
        assert_eq!(v2.num_files_at_level(0), 0);
    }
}
