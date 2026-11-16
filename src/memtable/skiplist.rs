use crate::common::compare_internal_keys;
use crate::memtable::arena::AtomicArena;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::Arc;

pub const MAX_HEIGHT: usize = 20;
pub const BRANCHING_FACTOR: u32 = 4; // p = 1/4

#[repr(C, align(8))]
pub struct SkipListNode {
    pub key_len: u32,
    pub val_len: u32,
    pub height: u16,
    pub flags: u16,
    pub seq_num: u64,
}

impl SkipListNode {
    /// Returns a reference to the atomic forward pointer at `level`.
    ///
    /// # Safety
    /// The caller must ensure that `level < self.height` and that `self` points
    /// to valid node memory inside the arena.
    #[inline(always)]
    pub unsafe fn next_ptr(&self, level: usize) -> &AtomicPtr<SkipListNode> {
        let base = (self as *const Self as *const u8).add(std::mem::size_of::<SkipListNode>());
        let ptr = base as *const AtomicPtr<SkipListNode>;
        &*ptr.add(level)
    }

    /// Returns a byte slice view of the key payload.
    ///
    /// # Safety
    /// The caller must ensure that `self` has been fully initialized with valid key bytes.
    #[inline(always)]
    pub unsafe fn key(&self) -> &[u8] {
        let base = (self as *const Self as *const u8).add(
            std::mem::size_of::<SkipListNode>()
                + self.height as usize * std::mem::size_of::<AtomicPtr<SkipListNode>>(),
        );
        std::slice::from_raw_parts(base, self.key_len as usize)
    }

    /// Returns a byte slice view of the value payload.
    ///
    /// # Safety
    /// The caller must ensure that `self` has been fully initialized with valid value bytes.
    #[inline(always)]
    pub unsafe fn value(&self) -> &[u8] {
        let base = (self as *const Self as *const u8).add(
            std::mem::size_of::<SkipListNode>()
                + self.height as usize * std::mem::size_of::<AtomicPtr<SkipListNode>>()
                + self.key_len as usize,
        );
        std::slice::from_raw_parts(base, self.val_len as usize)
    }
}

pub struct ConcurrentSkipList {
    head: *mut SkipListNode,
    max_height: AtomicUsize,
    arena: Arc<AtomicArena>,
}

unsafe impl Send for ConcurrentSkipList {}
unsafe impl Sync for ConcurrentSkipList {}

impl ConcurrentSkipList {
    pub fn new(arena: Arc<AtomicArena>) -> Self {
        // Allocate head node with MAX_HEIGHT
        let node_size = std::mem::size_of::<SkipListNode>()
            + MAX_HEIGHT * std::mem::size_of::<AtomicPtr<SkipListNode>>();
        let head_ptr = arena.alloc(node_size, 8) as *mut SkipListNode;
        assert!(!head_ptr.is_null(), "Arena out of memory for SkipList head");

        unsafe {
            (*head_ptr).key_len = 0;
            (*head_ptr).val_len = 0;
            (*head_ptr).height = MAX_HEIGHT as u16;
            (*head_ptr).flags = 0;
            (*head_ptr).seq_num = 0;

            let next_base = (head_ptr as *mut u8).add(std::mem::size_of::<SkipListNode>())
                as *mut AtomicPtr<SkipListNode>;
            for i in 0..MAX_HEIGHT {
                std::ptr::write(next_base.add(i), AtomicPtr::new(null_mut()));
            }
        }

        Self {
            head: head_ptr,
            max_height: AtomicUsize::new(1),
            arena,
        }
    }

    #[inline(always)]
    pub fn arena(&self) -> &Arc<AtomicArena> {
        &self.arena
    }

    #[inline(always)]
    pub fn max_height(&self) -> usize {
        self.max_height.load(Ordering::Relaxed)
    }

    /// Random height generation using fast xorshift32 PRNG (geometric p=1/4).
    fn random_height(&self) -> usize {
        let mut height = 1;
        let mut seed = (self.arena.allocated_bytes() as u32) ^ 0xA5A5A5A5;
        if seed == 0 {
            seed = 0x12345678;
        }

        while height < MAX_HEIGHT {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            if !seed.is_multiple_of(BRANCHING_FACTOR) {
                break;
            }
            height += 1;
        }
        height
    }

    /// Finds predecessors and successors across all levels.
    /// `prev[i]` is the rightmost node at level `i` with `key < search_key`.
    /// `succ[i]` is `prev[i].next[i]` (the leftmost node at level `i` with `key >= search_key`).
    fn find_splice_points(
        &self,
        key: &[u8],
        prev: &mut [*mut SkipListNode; MAX_HEIGHT],
        succ: &mut [*mut SkipListNode; MAX_HEIGHT],
    ) {
        let mut curr = self.head;
        let mut level = self.max_height();

        loop {
            let next = unsafe { (*curr).next_ptr(level - 1).load(Ordering::Acquire) };

            let advance = if !next.is_null() {
                let next_key = unsafe { (*next).key() };
                compare_internal_keys(next_key, key).is_lt()
            } else {
                false
            };

            if advance {
                curr = next;
            } else {
                prev[level - 1] = curr;
                succ[level - 1] = next;
                if level == 1 {
                    break;
                }
                level -= 1;
            }
        }
    }

    /// Finds the earliest node where node.key >= key at level 0.
    fn find_greater_or_equal(&self, key: &[u8]) -> *mut SkipListNode {
        let mut curr = self.head;
        let mut level = self.max_height();

        loop {
            let next = unsafe { (*curr).next_ptr(level - 1).load(Ordering::Acquire) };

            let advance = if !next.is_null() {
                let next_key = unsafe { (*next).key() };
                compare_internal_keys(next_key, key).is_lt()
            } else {
                false
            };

            if advance {
                curr = next;
            } else {
                if level == 1 {
                    return next;
                }
                level -= 1;
            }
        }
    }

    /// Insert internal key and value into the SkipList.
    /// Returns true on success, false if the Arena is full.
    pub fn insert(&self, internal_key: &[u8], value: &[u8], seq_num: u64) -> bool {
        let height = self.random_height();

        // Update max_height if necessary
        let mut current_max = self.max_height.load(Ordering::Relaxed);
        while height > current_max {
            match self.max_height.compare_exchange_weak(
                current_max,
                height,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current_max = actual,
            }
        }

        let total_size = std::mem::size_of::<SkipListNode>()
            + height * std::mem::size_of::<AtomicPtr<SkipListNode>>()
            + internal_key.len()
            + value.len();

        let node_mem = self.arena.alloc(total_size, 8);
        if node_mem.is_null() {
            return false; // Arena full
        }

        let new_node = node_mem as *mut SkipListNode;
        unsafe {
            (*new_node).key_len = internal_key.len() as u32;
            (*new_node).val_len = value.len() as u32;
            (*new_node).height = height as u16;
            (*new_node).flags = 0;
            (*new_node).seq_num = seq_num;

            // Initialize atomic pointers to null
            let next_base =
                node_mem.add(std::mem::size_of::<SkipListNode>()) as *mut AtomicPtr<SkipListNode>;
            for i in 0..height {
                std::ptr::write(next_base.add(i), AtomicPtr::new(null_mut()));
            }

            // Copy key bytes
            let key_dest = node_mem.add(
                std::mem::size_of::<SkipListNode>()
                    + height * std::mem::size_of::<AtomicPtr<SkipListNode>>(),
            );
            std::ptr::copy_nonoverlapping(internal_key.as_ptr(), key_dest, internal_key.len());

            // Copy value bytes
            let val_dest = key_dest.add(internal_key.len());
            std::ptr::copy_nonoverlapping(value.as_ptr(), val_dest, value.len());
        }

        let mut prev = [self.head; MAX_HEIGHT];
        let mut succ = [null_mut(); MAX_HEIGHT];

        // Link level by level from 0 up to height - 1.
        for level in 0..height {
            loop {
                self.find_splice_points(internal_key, &mut prev, &mut succ);

                unsafe {
                    (*new_node)
                        .next_ptr(level)
                        .store(succ[level], Ordering::Relaxed);
                }

                // CAS linking with Release ordering on success to publish initialized node
                match unsafe {
                    (*prev[level]).next_ptr(level).compare_exchange(
                        succ[level],
                        new_node,
                        Ordering::Release,
                        Ordering::Relaxed,
                    )
                } {
                    Ok(_) => break, // Successfully linked at this level
                    Err(_) => {
                        // Concurrent modification: re-run find_splice_points and retry
                    }
                }
            }
        }

        true
    }

    /// Search for an exact internal key or closest match.
    pub fn get(&self, internal_key: &[u8]) -> Option<(*const SkipListNode, Vec<u8>)> {
        let node = self.find_greater_or_equal(internal_key);
        if node.is_null() {
            return None;
        }

        let found_key = unsafe { (*node).key() };
        if found_key == internal_key {
            let val = unsafe { (*node).value().to_vec() };
            Some((node as *const SkipListNode, val))
        } else {
            None
        }
    }

    /// Iterator over the SkipList in ascending internal key order.
    pub fn iter(&self) -> SkipListIterator {
        let first = unsafe { (*self.head).next_ptr(0).load(Ordering::Acquire) };
        SkipListIterator { curr: first }
    }
}

pub struct SkipListIterator {
    curr: *mut SkipListNode,
}

impl Iterator for SkipListIterator {
    type Item = (*const SkipListNode, Vec<u8>, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.curr.is_null() {
            return None;
        }

        let node = self.curr;
        unsafe {
            let key = (*node).key().to_vec();
            let val = (*node).value().to_vec();
            self.curr = (*node).next_ptr(0).load(Ordering::Acquire);
            Some((node as *const SkipListNode, key, val))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{InternalKey, ValueType};
    use std::thread;

    #[test]
    fn test_skiplist_single_thread() {
        let arena = Arc::new(AtomicArena::new(10 * 1024 * 1024));
        let list = ConcurrentSkipList::new(arena);

        let k1 = InternalKey::new(b"alpha".to_vec(), 1, ValueType::Put).encode();
        let k2 = InternalKey::new(b"beta".to_vec(), 2, ValueType::Put).encode();
        let k3 = InternalKey::new(b"gamma".to_vec(), 3, ValueType::Put).encode();

        assert!(list.insert(&k1, b"val_alpha", 1));
        assert!(list.insert(&k2, b"val_beta", 2));
        assert!(list.insert(&k3, b"val_gamma", 3));

        let res1 = list.get(&k1);
        assert!(res1.is_some());
        assert_eq!(res1.unwrap().1, b"val_alpha");

        let res2 = list.get(&k2);
        assert!(res2.is_some());
        assert_eq!(res2.unwrap().1, b"val_beta");

        let res3 = list.get(&k3);
        assert!(res3.is_some());
        assert_eq!(res3.unwrap().1, b"val_gamma");

        let k_none = InternalKey::new(b"delta".to_vec(), 4, ValueType::Put).encode();
        assert!(list.get(&k_none).is_none());
    }

    #[test]
    fn test_skiplist_concurrent_insert_and_reads() {
        let arena = Arc::new(AtomicArena::new(32 * 1024 * 1024));
        let list = Arc::new(ConcurrentSkipList::new(arena));
        let num_threads = 8;
        let items_per_thread = 500;

        let mut handles = Vec::new();
        for t in 0..num_threads {
            let l = Arc::clone(&list);
            handles.push(thread::spawn(move || {
                for i in 0..items_per_thread {
                    let key_str = format!("user_{:04}_{:06}", t, i);
                    let val_str = format!("val_{:04}_{:06}", t, i);
                    let ikey =
                        InternalKey::new(key_str.into_bytes(), i as u64, ValueType::Put).encode();
                    assert!(l.insert(&ikey, val_str.as_bytes(), i as u64));
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Verify all items are present
        for t in 0..num_threads {
            for i in 0..items_per_thread {
                let key_str = format!("user_{:04}_{:06}", t, i);
                let expected_val = format!("val_{:04}_{:06}", t, i);
                let ikey =
                    InternalKey::new(key_str.into_bytes(), i as u64, ValueType::Put).encode();
                let res = list.get(&ikey).expect("key must be present");
                assert_eq!(res.1, expected_val.as_bytes());
            }
        }
    }
}
