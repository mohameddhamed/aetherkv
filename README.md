# AetherKV — High-Throughput Asynchronous LSM Storage Engine

AetherKV is an embedded, ultra-high-throughput Log-Structured Merge (LSM) key-value engine written in Rust, engineered from the ground up for modern multi-core architectures, PCIe Gen4/Gen5 NVMe storage, and low-latency Linux systems.

---

## Key Architectural Highlights

### 1. Lock-Free MemTable with Generational Bump Arena Allocation
- **Concurrency:** Multi-level concurrent SkipList with atomic Compare-And-Swap (CAS) tower pointer linking and relaxed/acquire memory orderings.
- **Zero Epoch Reclamation / Zero Hazard Pointers:** LSM keys are never deleted in place; mutations append new versions or tombstones. Nodes are bump-allocated from a contiguous 64MB `AtomicArena` tracked with a monotonic `AtomicUsize`.
- **$O(1)$ Generational Reclamation:** When the MemTable hits 64MB, it is atomically frozen. Once the asynchronous SSTable flush completes and reader reference counts reach zero, the entire 64MB Arena is released in a single $O(1)$ operation (`munmap`).

### 2. Linux Kernel Direct I/O & Pre-allocated WAL
- **Bypassing the Inode `i_rwsem` Trap:** In ext4 and XFS, unaligned or file-extending `O_DIRECT` writes take the inode's `i_rwsem` in exclusive mode, serializing concurrent writes. AetherKV eliminates this bottleneck by pre-allocating contiguous 64MB WAL segments via `libc::fallocate(fd, FALLOC_FL_ZERO_RANGE, 0, 64MB)`.
- **Aligned Write-Combining Buffer:** Log frames are aggregated in a user-space 4096-byte sector-aligned buffer (`SectorAlignedBuffer`) and submitted via `io_uring` with registered fixed buffers (`io_uring_register_buffers`) and `O_DIRECT`.
- **Hardware-Accelerated Checksumming:** Hardware SSE4.2 instructions (`_mm_crc32_u64`) on x86_64 and ARMv8 CRC32 intrinsics on AArch64, with an 8-way sliced Castagnoli table fallback.

### 3. SSTable Block Format & Vectorized Bloom Filters
- **4KB Delta Blocks:** Data blocks utilize prefix delta compression with periodic restart points (every 16 keys), enabling binary-search interval seeks.
- **Memory-Mapped Index:** Zero-deserialization sparse block index accessed via `mmap(MAP_PRIVATE, PROT_READ)`.
- **AVX2 256-Bit Block-Split Bloom Filters:** Evaluates 8 probe bits in parallel across a single 256-bit AVX2 register (`_mm256_testz_si256`) at 10 bits/key (<1% false positive rate), eliminating cold disk lookups on non-existent keys.

### 4. Dynamic Anti-Stall Backpressure & Leveled Compaction
- **Dynamic Write-Amplification Budgeting:** Leveled Compaction engine targeting Write Amplification $< 10\times$ ($L_1 = 64\text{MB}$, level multiplier $= 10$).
- **Progressive L0 Backpressure:** Eliminates multi-second write stalls during sustained load by injecting exponential micro-delays before hard stalls:
  $$\text{Delay}(\mu s) = \begin{cases} 
  0, & \text{if } N_{L0} \le 4 \\
  2^{(\frac{N_{L0} - 4}{2})}, & \text{if } 4 < N_{L0} < 12 \\
  \infty \text{ (Block thread)}, & \text{if } N_{L0} \ge 12 
  \end{cases}$$

---

## Directory Structure

```
aetherkv/
├── Cargo.toml
├── README.md
├── benches/
│   ├── open_loop_bench.rs         # Poisson open-loop benchmark (HdrHistogram)
│   └── throughput_criterion.rs    # Criterion microbenchmarks
├── tests/
│   ├── crash_consistency.rs       # SIGKILL fault-injection durability tests
│   ├── loom_skiplist.rs           # Loom atomic permutation model tests
│   └── miri_arena_test.rs         # Pointer provenance & alignment verification
└── src/
    ├── lib.rs                     # Crate root & public exports
    ├── common/
    │   ├── mod.rs
    │   ├── crc32c.rs              # SSE4.2 / ARMv8 / Slice-by-8 CRC32C
    │   ├── error.rs               # Engine error types
    │   └── types.rs               # InternalKey, SequenceNumber, ValueType
    ├── memtable/
    │   ├── mod.rs                 # MemTable facade
    │   ├── arena.rs               # 64MB Atomic bump-allocator
    │   └── skiplist.rs            # Lock-free CAS concurrent SkipList
    ├── wal/
    │   ├── mod.rs                 # WAL writer & crash recovery reader
    │   ├── ring_buffer.rs         # 4KB sector-aligned write-combining buffer
    │   ├── segment.rs             # 64MB fallocate pre-allocation
    │   └── uio.rs                 # Linux io_uring & POSIX Direct I/O
    ├── sstable/
    │   ├── mod.rs                 # SSTable builder, reader, and iterator
    │   ├── block.rs               # 4KB data blocks with delta prefix compression
    │   ├── bloom.rs               # 256-bit AVX2 Block-Split Bloom filter
    │   ├── footer.rs              # 48-byte fixed trailer with magic number
    │   └── index.rs               # Two-level sparse block index
    ├── manifest/
    │   ├── mod.rs                 # VersionSet and state persistence
    │   └── version.rs             # Multi-level LSM hierarchy and reference-counted views
    ├── compaction/
    │   ├── mod.rs                 # Multi-way merge compaction executor
    │   ├── picker.rs              # Score-based level picker (WA < 10x)
    │   └── stall.rs               # Dynamic exponential backpressure controller
    └── engine/
        ├── mod.rs                 # Engine core orchestrating all subsystems
        ├── batch.rs               # Atomic WriteBatch
        └── recovery.rs            # WAL replay and crash recovery
```

---

## Empirical Benchmarks & Verification

### Running the Open-Loop Poisson Benchmark
Evaluates true response latency under a realistic open-loop Poisson arrival distribution, preventing Gil Tene's Coordinated Omission:
```bash
cargo bench --bench open_loop_bench
```
Sample benchmark output:
```
==========================================================================
 AetherKV Open-Loop Poisson Latency Benchmark (Coordinated Omission Free) 
==========================================================================

Benchmark Results (Open-Loop Poisson Arrival):
  Total Writes Completed: 1,251,855
  Elapsed Time:           3.00s
  Sustained Throughput:   416,725.99 IOPS
--------------------------------------------------------------------------
  Latency Percentiles (True Wait + Service Time):
    P50   (Median):       6 µs
    P90:               1,912 µs
    P99:             162,815 µs
    P99.9:           180,479 µs
    Max:             182,399 µs
==========================================================================
```

### Running the Full Test Suite
```bash
cargo test --all-targets
```

### Running Crash Consistency Durability Tests (SIGKILL)
Spawns an active writer thread, kills it abruptly with `SIGKILL`, and asserts 100% data integrity upon WAL recovery:
```bash
cargo test --test crash_consistency -- --nocapture
```

### Running Memory Safety (Miri) & Model Checking (Loom)
```bash
cargo test --test miri_arena_test
RUSTFLAGS="--cfg loom" cargo test --test loom_skiplist
```
