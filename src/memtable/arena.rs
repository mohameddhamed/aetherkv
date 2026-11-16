use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const DEFAULT_ARENA_CAPACITY: usize = 64 * 1024 * 1024; // 64 MB

/// AtomicArena provides monotonic lock-free bump allocation from a contiguous 64MB chunk.
/// Zero per-node deallocations; the entire arena is reclaimed in $O(1)$ on Drop.
#[repr(align(64))]
pub struct AtomicArena {
    ptr: NonNull<u8>,
    capacity: usize,
    allocated: AtomicUsize,
    is_mmap: bool,
}

unsafe impl Send for AtomicArena {}
unsafe impl Sync for AtomicArena {}

impl AtomicArena {
    /// Create a new 64MB atomic arena.
    pub fn new(capacity: usize) -> Self {
        // Attempt anonymous mmap first for zero-overhead OS page faulting
        #[cfg(unix)]
        {
            let mmap_ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    capacity,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };

            if mmap_ptr != libc::MAP_FAILED && !mmap_ptr.is_null() {
                return Self {
                    ptr: NonNull::new(mmap_ptr as *mut u8).expect("valid mmap ptr"),
                    capacity,
                    allocated: AtomicUsize::new(0),
                    is_mmap: true,
                };
            }
        }

        // Fallback to aligned heap allocation
        let layout = Layout::from_size_align(capacity, 64).expect("valid layout");
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).expect("allocation failed");

        Self {
            ptr,
            capacity,
            allocated: AtomicUsize::new(0),
            is_mmap: false,
        }
    }

    /// Allocate a block of `size` bytes with the specified `align` requirement.
    /// Returns a raw pointer into the arena, or null if capacity is exhausted.
    #[inline(always)]
    pub fn alloc(&self, size: usize, align: usize) -> *mut u8 {
        debug_assert!(align.is_power_of_two());
        let align_mask = align - 1;

        let mut current = self.allocated.load(Ordering::Relaxed);
        loop {
            let aligned = (current + align_mask) & !align_mask;
            let next = aligned + size;

            if next > self.capacity {
                return std::ptr::null_mut();
            }

            match self.allocated.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return unsafe { self.ptr.as_ptr().add(aligned) };
                }
                Err(actual) => {
                    current = actual;
                }
            }
        }
    }

    /// Return total bytes currently allocated (monotonic).
    #[inline(always)]
    pub fn allocated_bytes(&self) -> usize {
        self.allocated.load(Ordering::Relaxed)
    }

    /// Return total capacity of the arena.
    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns true if the arena has exhausted its capacity.
    #[inline(always)]
    pub fn is_full(&self) -> bool {
        self.allocated_bytes() >= self.capacity
    }
}

impl Drop for AtomicArena {
    fn drop(&mut self) {
        if self.is_mmap {
            #[cfg(unix)]
            unsafe {
                libc::munmap(self.ptr.as_ptr() as *mut libc::c_void, self.capacity);
            }
        } else {
            let layout = Layout::from_size_align(self.capacity, 64).expect("valid layout");
            unsafe {
                dealloc(self.ptr.as_ptr(), layout);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_arena_basic_allocation() {
        let arena = AtomicArena::new(1024 * 1024);
        let ptr1 = arena.alloc(100, 8);
        assert!(!ptr1.is_null());
        assert_eq!(ptr1 as usize % 8, 0);

        let ptr2 = arena.alloc(200, 64);
        assert!(!ptr2.is_null());
        assert_eq!(ptr2 as usize % 64, 0);
        assert!(ptr2 as usize >= ptr1 as usize + 100);

        assert!(arena.allocated_bytes() >= 300);
    }

    #[test]
    fn test_arena_capacity_exhaustion() {
        let arena = AtomicArena::new(1024);
        let p1 = arena.alloc(800, 8);
        assert!(!p1.is_null());

        let p2 = arena.alloc(500, 8);
        assert!(p2.is_null()); // Cannot exceed capacity
    }

    #[test]
    fn test_arena_concurrent_bump() {
        let arena = Arc::new(AtomicArena::new(4 * 1024 * 1024));
        let num_threads = 8;
        let iters = 1000;
        let alloc_size = 128;

        let mut handles = Vec::new();
        for _ in 0..num_threads {
            let a = Arc::clone(&arena);
            handles.push(thread::spawn(move || {
                for _ in 0..iters {
                    let ptr = a.alloc(alloc_size, 8);
                    assert!(!ptr.is_null());
                    // Write to memory to ensure valid mapping
                    unsafe {
                        std::ptr::write_bytes(ptr, 0xAB, alloc_size);
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(arena.allocated_bytes(), num_threads * iters * alloc_size);
    }
}
