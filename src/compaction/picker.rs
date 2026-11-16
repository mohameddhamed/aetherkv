use crate::common::compare_internal_keys;
use crate::manifest::{FileMetaData, Version, NUM_LEVELS};
use std::sync::Arc;

pub const BASE_LEVEL_SIZE: u64 = 64 * 1024 * 1024; // 64 MB
pub const LEVEL_MULTIPLIER: u64 = 10;
pub const L0_COMPACTION_THRESHOLD: usize = 4;

#[derive(Debug, Clone)]
pub struct CompactionTask {
    pub level: usize,
    pub input_level_files: Vec<Arc<FileMetaData>>,
    pub output_level_files: Vec<Arc<FileMetaData>>,
}

pub struct CompactionPicker;

impl CompactionPicker {
    /// Calculate target byte size for a given level.
    pub fn target_level_size(level: usize) -> u64 {
        if level <= 1 {
            BASE_LEVEL_SIZE
        } else {
            BASE_LEVEL_SIZE * LEVEL_MULTIPLIER.pow((level - 1) as u32)
        }
    }

    /// Pick the optimal compaction task based on level amplification scores.
    pub fn pick_compaction(version: &Version) -> Option<CompactionTask> {
        let mut best_score = 0.9999;
        let mut best_level = None;

        // Check L0 score based on file count
        let l0_count = version.num_files_at_level(0);
        let l0_score = l0_count as f64 / L0_COMPACTION_THRESHOLD as f64;
        if l0_score > best_score {
            best_score = l0_score;
            best_level = Some(0);
        }

        // Check levels 1..NUM_LEVELS - 1 based on byte size
        for level in 1..NUM_LEVELS - 1 {
            let size = version.total_level_size(level);
            let target = Self::target_level_size(level);
            let score = size as f64 / target as f64;

            if score > best_score {
                best_score = score;
                best_level = Some(level);
            }
        }

        let level = best_level?;

        if level == 0 {
            // For L0 -> L1 compaction: take all L0 files
            let l0_files = version.levels()[0].clone();
            if l0_files.is_empty() {
                return None;
            }

            // Find key range of all L0 files
            let mut smallest = l0_files[0].smallest_key.clone();
            let mut largest = l0_files[0].largest_key.clone();

            for f in &l0_files[1..] {
                if compare_internal_keys(&f.smallest_key, &smallest).is_lt() {
                    smallest = f.smallest_key.clone();
                }
                if compare_internal_keys(&f.largest_key, &largest).is_gt() {
                    largest = f.largest_key.clone();
                }
            }

            // Find overlapping files in Level 1
            let mut l1_overlaps = Vec::new();
            for f in &version.levels()[1] {
                if compare_internal_keys(&f.largest_key, &smallest).is_ge()
                    && compare_internal_keys(&f.smallest_key, &largest).is_le()
                {
                    l1_overlaps.push(f.clone());
                }
            }

            Some(CompactionTask {
                level: 0,
                input_level_files: l0_files,
                output_level_files: l1_overlaps,
            })
        } else {
            // For Ln -> Ln+1 compaction: pick earliest file
            let input_files = version.levels()[level].clone();
            if input_files.is_empty() {
                return None;
            }

            let candidate = input_files[0].clone();
            let mut output_overlaps = Vec::new();

            for f in &version.levels()[level + 1] {
                if compare_internal_keys(&f.largest_key, &candidate.smallest_key).is_ge()
                    && compare_internal_keys(&f.smallest_key, &candidate.largest_key).is_le()
                {
                    output_overlaps.push(f.clone());
                }
            }

            Some(CompactionTask {
                level,
                input_level_files: vec![candidate],
                output_level_files: output_overlaps,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_compaction_picker_l0_trigger() {
        let dir = tempdir().unwrap();
        let mut version = Version::new(dir.path());

        let mut edit = crate::manifest::VersionEdit::new();
        for i in 0..5 {
            edit.add_file(
                0,
                FileMetaData {
                    file_id: i,
                    file_size: 1024,
                    smallest_key: format!("k_{:04}", i).into_bytes(),
                    largest_key: format!("k_{:04}_end", i).into_bytes(),
                    level: 0,
                },
            );
        }

        version = version.apply(&edit);
        let task = CompactionPicker::pick_compaction(&version);
        assert!(task.is_some());
        let t = task.unwrap();
        assert_eq!(t.level, 0);
        assert_eq!(t.input_level_files.len(), 5);
    }
}
