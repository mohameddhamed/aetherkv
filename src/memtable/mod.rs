pub mod arena;
pub mod skiplist;

pub use arena::{AtomicArena, DEFAULT_ARENA_CAPACITY};
pub use skiplist::{ConcurrentSkipList, SkipListIterator, SkipListNode, MAX_HEIGHT};

use crate::common::{
    compare_internal_keys, extract_tag, extract_user_key, InternalKey, SequenceNumber, ValueType,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub enum MemTableEntry {
    Value(Vec<u8>),
    Deletion,
}

pub struct MemTable {
    arena: Arc<AtomicArena>,
    skiplist: ConcurrentSkipList,
    id: usize,
    num_entries: AtomicUsize,
}

impl MemTable {
    pub fn new(id: usize, arena_capacity: usize) -> Self {
        let arena = Arc::new(AtomicArena::new(arena_capacity));
        let skiplist = ConcurrentSkipList::new(Arc::clone(&arena));
        Self {
            arena,
            skiplist,
            id,
            num_entries: AtomicUsize::new(0),
        }
    }

    #[inline(always)]
    pub fn id(&self) -> usize {
        self.id
    }

    #[inline(always)]
    pub fn arena(&self) -> &Arc<AtomicArena> {
        &self.arena
    }

    #[inline(always)]
    pub fn approximate_size(&self) -> usize {
        self.arena.allocated_bytes()
    }

    #[inline(always)]
    pub fn is_full(&self) -> bool {
        self.arena.is_full()
    }

    #[inline(always)]
    pub fn entry_count(&self) -> usize {
        self.num_entries.load(Ordering::Relaxed)
    }

    /// Insert a record with sequence number and value type into the MemTable.
    pub fn insert(
        &self,
        user_key: &[u8],
        value: &[u8],
        seq: SequenceNumber,
        value_type: ValueType,
    ) -> bool {
        let internal_key = InternalKey::new(user_key.to_vec(), seq, value_type).encode();
        if self.skiplist.insert(&internal_key, value, seq) {
            self.num_entries.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Query the MemTable for a user key visible at the given snapshot sequence number.
    pub fn get(&self, user_key: &[u8], snapshot_seq: SequenceNumber) -> Option<MemTableEntry> {
        // Construct search target with snapshot_seq
        // In internal key ordering, for identical user_keys, higher seq numbers sort first.
        let target = InternalKey::new(user_key.to_vec(), snapshot_seq, ValueType::Put).encode();

        let mut it = self.skiplist.iter();
        while let Some((_, ikey, val)) = it.next() {
            let ukey = extract_user_key(&ikey);
            if ukey == user_key {
                if let Some((seq, vt)) = extract_tag(&ikey) {
                    if seq <= snapshot_seq {
                        return match vt {
                            ValueType::Put => Some(MemTableEntry::Value(val)),
                            ValueType::Delete => Some(MemTableEntry::Deletion),
                        };
                    }
                }
            } else if compare_internal_keys(&ikey, &target).is_gt() {
                // Past the target user key
                break;
            }
        }
        None
    }

    /// Return an iterator over all internal keys in the MemTable.
    #[inline(always)]
    pub fn iter(&self) -> SkipListIterator {
        self.skiplist.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memtable_versioning() {
        let mem = MemTable::new(1, 1024 * 1024);

        // Insert version 1 (Put)
        mem.insert(b"foo", b"v1", 10, ValueType::Put);
        // Insert version 2 (Put)
        mem.insert(b"foo", b"v2", 20, ValueType::Put);
        // Insert version 3 (Delete)
        mem.insert(b"foo", b"", 30, ValueType::Delete);

        // Read at seq 15 -> should see "v1"
        match mem.get(b"foo", 15) {
            Some(MemTableEntry::Value(v)) => assert_eq!(v, b"v1"),
            _ => panic!("Expected v1 at seq 15"),
        }

        // Read at seq 25 -> should see "v2"
        match mem.get(b"foo", 25) {
            Some(MemTableEntry::Value(v)) => assert_eq!(v, b"v2"),
            _ => panic!("Expected v2 at seq 25"),
        }

        // Read at seq 35 -> should see Deletion
        match mem.get(b"foo", 35) {
            Some(MemTableEntry::Deletion) => (),
            _ => panic!("Expected Deletion at seq 35"),
        }
    }
}
