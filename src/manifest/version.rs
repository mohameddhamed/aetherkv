use crate::common::{compare_internal_keys, extract_tag, extract_user_key, SequenceNumber, ValueType};
use crate::sstable::TableReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const NUM_LEVELS: usize = 7;

#[derive(Debug, Clone)]
pub struct FileMetaData {
    pub file_id: u64,
    pub file_size: u64,
    pub smallest_key: Vec<u8>,
    pub largest_key: Vec<u8>,
    pub level: usize,
}

#[derive(Clone)]
pub struct Version {
    levels: [Vec<Arc<FileMetaData>>; NUM_LEVELS],
    db_dir: PathBuf,
}

impl Version {
    pub fn new<P: AsRef<Path>>(db_dir: P) -> Self {
        const EMPTY_VEC: Vec<Arc<FileMetaData>> = Vec::new();
        Self {
            levels: [EMPTY_VEC; NUM_LEVELS],
            db_dir: db_dir.as_ref().to_path_buf(),
        }
    }

    pub fn levels(&self) -> &[Vec<Arc<FileMetaData>>; NUM_LEVELS] {
        &self.levels
    }

    pub fn num_files_at_level(&self, level: usize) -> usize {
        if level < NUM_LEVELS {
            self.levels[level].len()
        } else {
            0
        }
    }

    pub fn total_level_size(&self, level: usize) -> u64 {
        if level < NUM_LEVELS {
            self.levels[level].iter().map(|f| f.file_size).sum()
        } else {
            0
        }
    }

    /// Point lookup across on-disk SSTable hierarchy.
    /// Traverses Level 0 (newest to oldest), then Levels 1..NUM_LEVELS via binary search.
    pub fn get(
        &self,
        user_key: &[u8],
        snapshot_seq: SequenceNumber,
    ) -> Option<(ValueType, Vec<u8>)> {
        // Construct target internal key for version search
        let target = crate::common::InternalKey::new(user_key.to_vec(), snapshot_seq, ValueType::Put).encode();

        // 1. Search Level 0: files can overlap, scan from newest to oldest
        for file in self.levels[0].iter().rev() {
            // Check bounding key range
            if compare_internal_keys(&file.smallest_key, &target).is_le()
                && compare_internal_keys(&file.largest_key, &target).is_ge()
            {
                if let Some(res) = self.search_table(file.file_id, user_key, snapshot_seq) {
                    return Some(res);
                }
            }
        }

        // 2. Search Levels 1..NUM_LEVELS: files at each level have disjoint sorted ranges
        for level in 1..NUM_LEVELS {
            let files = &self.levels[level];
            if files.is_empty() {
                continue;
            }

            // Binary search candidate file
            let idx = files.partition_point(|f| {
                compare_internal_keys(&f.largest_key, &target).is_lt()
            });

            if idx < files.len() {
                let file = &files[idx];
                if compare_internal_keys(&file.smallest_key, &target).is_le() {
                    if let Some(res) = self.search_table(file.file_id, user_key, snapshot_seq) {
                        return Some(res);
                    }
                }
            }
        }

        None
    }

    fn search_table(
        &self,
        file_id: u64,
        user_key: &[u8],
        snapshot_seq: SequenceNumber,
    ) -> Option<(ValueType, Vec<u8>)> {
        let path = self.db_dir.join(format!("{:08}.sst", file_id));
        let reader = TableReader::open(&path).ok()?;

        // Point query inside SSTable
        let target = crate::common::InternalKey::new(user_key.to_vec(), snapshot_seq, ValueType::Put).encode();
        if let Ok(Some(val)) = reader.get(&target) {
            return Some((ValueType::Put, val));
        }

        // Check if there is an exact or older version matching user_key in this table
        for (ikey, val) in reader.iter() {
            if extract_user_key(&ikey) == user_key {
                if let Some((seq, vt)) = extract_tag(&ikey) {
                    if seq <= snapshot_seq {
                        return Some((vt, val));
                    }
                }
            }
        }

        None
    }

    /// Apply delta edits to create a new Version.
    pub fn apply(&self, edit: &VersionEdit) -> Self {
        let mut new_levels = self.levels.clone();

        // Delete deleted files
        for (level, file_id) in &edit.deleted_files {
            if *level < NUM_LEVELS {
                new_levels[*level].retain(|f| f.file_id != *file_id);
            }
        }

        // Add new files
        for (level, file_meta) in &edit.added_files {
            if *level < NUM_LEVELS {
                new_levels[*level].push(Arc::new(file_meta.clone()));
                if *level > 0 {
                    // Sort files in levels 1..N by smallest_key
                    new_levels[*level].sort_by(|a, b| {
                        compare_internal_keys(&a.smallest_key, &b.smallest_key)
                    });
                }
            }
        }

        Self {
            levels: new_levels,
            db_dir: self.db_dir.clone(),
        }
    }
}

/// Delta modification descriptor for atomic manifest updates.
#[derive(Debug, Default, Clone)]
pub struct VersionEdit {
    pub added_files: Vec<(usize, FileMetaData)>,
    pub deleted_files: Vec<(usize, u64)>,
    pub next_file_number: Option<u64>,
    pub last_sequence: Option<u64>,
}

impl VersionEdit {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_file(&mut self, level: usize, meta: FileMetaData) {
        self.added_files.push((level, meta));
    }

    pub fn delete_file(&mut self, level: usize, file_id: u64) {
        self.deleted_files.push((level, file_id));
    }
}
