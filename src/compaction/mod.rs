pub mod picker;
pub mod stall;

pub use picker::{CompactionPicker, CompactionTask, L0_COMPACTION_THRESHOLD};
pub use stall::BackpressureController;

use crate::common::{extract_tag, extract_user_key, Result, ValueType};
use crate::manifest::{FileMetaData, VersionEdit, VersionSet};
use crate::sstable::{TableBuilder, TableReader};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct CompactionExecutor {
    db_dir: PathBuf,
    version_set: Arc<VersionSet>,
    backpressure: Arc<BackpressureController>,
}

impl CompactionExecutor {
    pub fn new<P: AsRef<Path>>(
        db_dir: P,
        version_set: Arc<VersionSet>,
        backpressure: Arc<BackpressureController>,
    ) -> Self {
        Self {
            db_dir: db_dir.as_ref().to_path_buf(),
            version_set,
            backpressure,
        }
    }

    /// Execute a compaction task: merges input files into new SSTable(s) in target level.
    pub fn execute(&self, task: CompactionTask) -> Result<()> {
        let target_level = task.level + 1;
        let mut readers = Vec::new();

        // Open all input files
        for f in task.input_level_files.iter().chain(task.output_level_files.iter()) {
            let path = self.db_dir.join(format!("{:08}.sst", f.file_id));
            if let Ok(reader) = TableReader::open(&path) {
                readers.push(reader);
            }
        }

        // K-way merge using sorted map or priority queue
        // Keys are sorted according to compare_internal_keys
        let mut merged_map: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        for reader in &readers {
            for (ikey, val) in reader.iter() {
                merged_map.insert(ikey, val);
            }
        }

        // Filter superseded keys / tombstones
        let mut deduped_entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut last_user_key: Option<Vec<u8>> = None;

        for (ikey, val) in merged_map {
            let ukey = extract_user_key(&ikey).to_vec();
            if let Some(ref last) = last_user_key {
                if *last == ukey {
                    // Newer version already taken because BTreeMap sorts ascending
                    // (in internal key comparison, newer versions sort first)
                    continue;
                }
            }

            if let Some((_, vt)) = extract_tag(&ikey) {
                if target_level >= 6 && vt == ValueType::Delete {
                    // Tombstone reached bottom level: can be dropped permanently!
                    last_user_key = Some(ukey);
                    continue;
                }
            }

            last_user_key = Some(ukey);
            deduped_entries.push((ikey, val));
        }

        if deduped_entries.is_empty() {
            // All keys expired: apply deletion edit
            let mut edit = VersionEdit::new();
            for f in &task.input_level_files {
                edit.delete_file(task.level, f.file_id);
            }
            for f in &task.output_level_files {
                edit.delete_file(target_level, f.file_id);
            }
            self.version_set.log_and_apply(&edit)?;
            self.cleanup_files(&task)?;
            self.backpressure.notify_compaction_complete();
            return Ok(());
        }

        // Build new SSTable for output level
        let new_file_id = self.version_set.next_file_number();
        let out_path = self.db_dir.join(format!("{:08}.sst", new_file_id));
        let mut builder = TableBuilder::new(&out_path)?;

        let smallest_key = deduped_entries.first().unwrap().0.clone();
        let largest_key = deduped_entries.last().unwrap().0.clone();

        for (k, v) in &deduped_entries {
            builder.add(k, v)?;
        }

        let (file_size, _) = builder.finish()?;

        // Construct VersionEdit
        let mut edit = VersionEdit::new();
        for f in &task.input_level_files {
            edit.delete_file(task.level, f.file_id);
        }
        for f in &task.output_level_files {
            edit.delete_file(target_level, f.file_id);
        }

        edit.add_file(
            target_level,
            FileMetaData {
                file_id: new_file_id,
                file_size,
                smallest_key,
                largest_key,
                level: target_level,
            },
        );

        self.version_set.log_and_apply(&edit)?;
        self.cleanup_files(&task)?;
        self.backpressure.notify_compaction_complete();

        Ok(())
    }

    fn cleanup_files(&self, task: &CompactionTask) -> Result<()> {
        for f in task.input_level_files.iter().chain(task.output_level_files.iter()) {
            let path = self.db_dir.join(format!("{:08}.sst", f.file_id));
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{InternalKey, ValueType};
    use tempfile::tempdir;

    #[test]
    fn test_compaction_execution() {
        let dir = tempdir().unwrap();
        let vset = Arc::new(VersionSet::new(dir.path()));
        let backpressure = Arc::new(BackpressureController::new());

        // Create 4 L0 tables
        let mut edit = VersionEdit::new();
        for i in 0..4 {
            let file_id = vset.next_file_number();
            let path = dir.path().join(format!("{:08}.sst", file_id));
            let mut builder = TableBuilder::new(&path).unwrap();

            let k = format!("key_{:04}", i);
            let v = format!("val_{:04}", i);
            let ikey = InternalKey::new(k.into_bytes(), i, ValueType::Put).encode();
            builder.add(&ikey, v.as_bytes()).unwrap();
            let (size, _) = builder.finish().unwrap();

            edit.add_file(
                0,
                FileMetaData {
                    file_id,
                    file_size: size,
                    smallest_key: ikey.clone(),
                    largest_key: ikey,
                    level: 0,
                },
            );
        }

        let version = vset.log_and_apply(&edit).unwrap();
        assert_eq!(version.num_files_at_level(0), 4);

        let task = CompactionPicker::pick_compaction(&version).unwrap();
        assert_eq!(task.level, 0);

        let executor = CompactionExecutor::new(dir.path(), vset.clone(), backpressure);
        executor.execute(task).unwrap();

        let new_version = vset.current();
        assert_eq!(new_version.num_files_at_level(0), 0);
        assert_eq!(new_version.num_files_at_level(1), 1);
    }
}
