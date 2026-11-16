use aetherkv::{AtomicArena, ConcurrentSkipList, InternalKey, ValueType};
use std::sync::Arc;

#[test]
fn test_concurrent_skiplist_interleaving_permutations() {
    #[cfg(loom)]
    {
        loom::model(|| {
            let arena = Arc::new(AtomicArena::new(64 * 1024));
            let list = Arc::new(ConcurrentSkipList::new(arena));

            let l1 = Arc::clone(&list);
            let h1 = loom::thread::spawn(move || {
                let k1 = InternalKey::new(b"loom_k1".to_vec(), 1, ValueType::Put).encode();
                l1.insert(&k1, b"v1", 1);
            });

            let l2 = Arc::clone(&list);
            let h2 = loom::thread::spawn(move || {
                let k2 = InternalKey::new(b"loom_k2".to_vec(), 2, ValueType::Put).encode();
                l2.insert(&k2, b"v2", 2);
            });

            h1.join().unwrap();
            h2.join().unwrap();

            let k1 = InternalKey::new(b"loom_k1".to_vec(), 1, ValueType::Put).encode();
            let k2 = InternalKey::new(b"loom_k2".to_vec(), 2, ValueType::Put).encode();
            assert!(list.get(&k1).is_some());
            assert!(list.get(&k2).is_some());
        });
    }

    #[cfg(not(loom))]
    {
        let arena = Arc::new(AtomicArena::new(512 * 1024));
        let list = Arc::new(ConcurrentSkipList::new(arena));
        let num_threads = 4;
        let iters = 100;

        let mut handles = Vec::new();
        for t in 0..num_threads {
            let l = Arc::clone(&list);
            handles.push(std::thread::spawn(move || {
                for i in 0..iters {
                    let k = format!("perm_k_{}_{}", t, i).into_bytes();
                    let v = format!("perm_v_{}_{}", t, i).into_bytes();
                    let ikey = InternalKey::new(k, (t * iters + i) as u64, ValueType::Put).encode();
                    assert!(l.insert(&ikey, &v, (t * iters + i) as u64));
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        for t in 0..num_threads {
            for i in 0..iters {
                let k = format!("perm_k_{}_{}", t, i).into_bytes();
                let ikey = InternalKey::new(k, (t * iters + i) as u64, ValueType::Put).encode();
                assert!(list.get(&ikey).is_some());
            }
        }
    }
}
