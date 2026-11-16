use aetherkv::{AtomicArena, ConcurrentSkipList, InternalKey, ValueType};
use std::sync::Arc;

#[test]
fn test_miri_arena_alignment_and_provenance() {
    let arena = AtomicArena::new(64 * 1024);

    // Verify 64-byte alignment
    let p1 = arena.alloc(128, 64);
    assert!(!p1.is_null());
    assert_eq!(p1 as usize % 64, 0);

    // Write bytes across entire allocated region
    unsafe {
        for i in 0..128 {
            *p1.add(i) = (i % 256) as u8;
        }
        for i in 0..128 {
            assert_eq!(*p1.add(i), (i % 256) as u8);
        }
    }

    // Verify 8-byte alignment
    let p2 = arena.alloc(32, 8);
    assert!(!p2.is_null());
    assert_eq!(p2 as usize % 8, 0);

    unsafe {
        std::ptr::write_bytes(p2, 0xFE, 32);
        for i in 0..32 {
            assert_eq!(*p2.add(i), 0xFE);
        }
    }
}

#[test]
fn test_miri_skiplist_node_provenance() {
    let arena = Arc::new(AtomicArena::new(128 * 1024));
    let list = ConcurrentSkipList::new(arena);

    for i in 0..20 {
        let key = format!("miri_key_{:04}", i);
        let val = format!("miri_val_{:04}", i);
        let ikey = InternalKey::new(key.into_bytes(), i as u64, ValueType::Put).encode();
        assert!(list.insert(&ikey, val.as_bytes(), i as u64));
    }

    for i in 0..20 {
        let key = format!("miri_key_{:04}", i);
        let expected_val = format!("miri_val_{:04}", i);
        let ikey = InternalKey::new(key.into_bytes(), i as u64, ValueType::Put).encode();
        let res = list.get(&ikey).expect("key must exist");
        assert_eq!(res.1, expected_val.as_bytes());
    }
}
