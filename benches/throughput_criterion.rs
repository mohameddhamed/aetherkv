use aetherkv::{
    crc32c, hash_key, AtomicArena, BlockBuilder, BlockReader, BlockSplitBloomFilter,
    ConcurrentSkipList, InternalKey, ValueType,
};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::sync::Arc;

fn bench_skiplist_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("skiplist");
    group.throughput(Throughput::Elements(1));

    group.bench_function("insert_monotonic", |b| {
        let arena = Arc::new(AtomicArena::new(32 * 1024 * 1024));
        let list = ConcurrentSkipList::new(arena);
        let mut i = 0u64;

        b.iter(|| {
            let key = format!("k_{:08}", i).into_bytes();
            let ikey = InternalKey::new(key, i, ValueType::Put).encode();
            black_box(list.insert(&ikey, b"val", i));
            i += 1;
        });
    });

    group.finish();
}

fn bench_crc32c_hardware(c: &mut Criterion) {
    let mut group = c.benchmark_group("crc32c");
    let payload = vec![0x5Au8; 4096]; // 4KB buffer
    group.throughput(Throughput::Bytes(payload.len() as u64));

    group.bench_with_input(BenchmarkId::new("throughput", "4096_bytes"), &payload, |b, data| {
        b.iter(|| {
            black_box(crc32c(data));
        });
    });

    group.finish();
}

fn bench_bloom_simd(c: &mut Criterion) {
    let mut group = c.benchmark_group("bloom_filter");
    group.throughput(Throughput::Elements(1));

    let n = 10_000;
    let mut hashes = Vec::with_capacity(n);
    for i in 0..n {
        let k = format!("bloom_{:08}", i);
        hashes.push(hash_key(k.as_bytes()));
    }

    let filter = BlockSplitBloomFilter::build_from_hashes(&hashes, 10);
    let probe_hash = hash_key(b"bloom_00000500");

    group.bench_function("probe_present_key", |b| {
        b.iter(|| {
            black_box(filter.may_contain_hash(probe_hash));
        });
    });

    let absent_hash = hash_key(b"absent_bloom_key");
    group.bench_function("probe_absent_key", |b| {
        b.iter(|| {
            black_box(filter.may_contain_hash(absent_hash));
        });
    });

    group.finish();
}

fn bench_block_delta_decoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("sstable_block");
    group.throughput(Throughput::Elements(1));

    let mut builder = BlockBuilder::new(16);
    for i in 0..100 {
        let k = format!("usr_{:06}", i);
        let ikey = InternalKey::new(k.into_bytes(), 1, ValueType::Put).encode();
        builder.add(&ikey, b"val_data");
    }
    let block_bytes = builder.finish();
    let reader = BlockReader::new(&block_bytes).unwrap();
    let target = InternalKey::new(b"usr_000050".to_vec(), 1, ValueType::Put).encode();

    group.bench_function("binary_search_point_lookup", |b| {
        b.iter(|| {
            black_box(reader.get(&target));
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_skiplist_insert,
    bench_crc32c_hardware,
    bench_bloom_simd,
    bench_block_delta_decoding
);
criterion_main!(benches);
