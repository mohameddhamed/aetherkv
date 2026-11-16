use crate::common::{AetherError, Result};
use std::fs::{File, OpenOptions};
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

pub const WAL_SEGMENT_SIZE: usize = 64 * 1024 * 1024; // 64 MB
pub const SECTOR_SIZE: usize = 4096;

pub struct WalSegment {
    id: u64,
    path: PathBuf,
    file: File,
    written_bytes: AtomicUsize,
    capacity: usize,
}

impl WalSegment {
    /// Create a new pre-allocated WAL segment file of `capacity` bytes.
    pub fn create<P: AsRef<Path>>(dir: P, id: u64, capacity: usize) -> Result<Self> {
        let path = dir.as_ref().join(format!("{:08}.wal", id));
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);

        // On Linux, open with O_DIRECT to bypass page cache
        #[cfg(target_os = "linux")]
        {
            opts.custom_flags(libc::O_DIRECT);
        }

        let file = opts.open(&path)?;
        let fd = file.as_raw_fd();

        // Pre-allocate the segment file to avoid i_rwsem serialization trap on file extension
        preallocate_file(fd, capacity)?;

        // On macOS, enable F_NOCACHE to disable VFS page caching
        #[cfg(target_os = "macos")]
        unsafe {
            libc::fcntl(fd, libc::F_NOCACHE, 1);
        }

        Ok(Self {
            id,
            path,
            file,
            written_bytes: AtomicUsize::new(0),
            capacity,
        })
    }

    /// Open an existing WAL segment file for recovery reading.
    pub fn open_for_read<P: AsRef<Path>>(path: P, id: u64) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().read(true).open(&path)?;
        let metadata = file.metadata()?;
        let len = metadata.len() as usize;

        Ok(Self {
            id,
            path,
            file,
            written_bytes: AtomicUsize::new(len),
            capacity: len,
        })
    }

    #[inline(always)]
    pub fn id(&self) -> u64 {
        self.id
    }

    #[inline(always)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[inline(always)]
    pub fn raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    #[inline(always)]
    pub fn file(&self) -> &File {
        &self.file
    }

    #[inline(always)]
    pub fn written_bytes(&self) -> usize {
        self.written_bytes.load(Ordering::Acquire)
    }

    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    #[inline(always)]
    pub fn remaining_bytes(&self) -> usize {
        let written = self.written_bytes();
        self.capacity.saturating_sub(written)
    }

    /// Atomically reserve an offset range in the pre-allocated segment.
    /// Returns Ok(offset) if sufficient space remains, or Err(WalExhausted).
    pub fn reserve_space(&self, bytes: usize) -> Result<usize> {
        let mut curr = self.written_bytes.load(Ordering::Relaxed);
        loop {
            if curr + bytes > self.capacity {
                return Err(AetherError::WalExhausted);
            }

            match self.written_bytes.compare_exchange_weak(
                curr,
                curr + bytes,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(curr),
                Err(actual) => curr = actual,
            }
        }
    }

    /// Sync written ranges to persistent storage.
    pub fn sync(&self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }
}

/// Pre-allocate physical blocks on disk.
/// On Linux: libc::fallocate with FALLOC_FL_ZERO_RANGE sets file size and zero-fills extents without user-space zero writes.
/// On macOS: fcntl with F_PREALLOCATE followed by ftruncate.
fn preallocate_file(fd: RawFd, size: usize) -> Result<()> {
    #[cfg(target_os = "linux")]
    unsafe {
        let ret = libc::fallocate(fd, libc::FALLOC_FL_ZERO_RANGE, 0, size as libc::off_t);
        if ret != 0 {
            // Fallback to posix_fallocate if filesystem doesn't support FALLOC_FL_ZERO_RANGE
            let p_ret = libc::posix_fallocate(fd, 0, size as libc::off_t);
            if p_ret != 0 {
                return Err(AetherError::Io(std::io::Error::from_raw_os_error(p_ret)));
            }
        }
    }

    #[cfg(target_os = "macos")]
    unsafe {
        let mut fstore = libc::fstore_t {
            fst_flags: libc::F_ALLOCATECONTIG,
            fst_posmode: libc::F_PEOFPOSMODE,
            fst_offset: 0,
            fst_length: size as libc::off_t,
            fst_bytesalloc: 0,
        };

        // Try contiguous allocation first, fall back to non-contiguous
        if libc::fcntl(fd, libc::F_PREALLOCATE, &fstore) != 0 {
            fstore.fst_flags = libc::F_ALLOCATEALL;
            libc::fcntl(fd, libc::F_PREALLOCATE, &fstore);
        }

        if libc::ftruncate(fd, size as libc::off_t) != 0 {
            return Err(AetherError::Io(std::io::Error::last_os_error()));
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    unsafe {
        if libc::ftruncate(fd, size as libc::off_t) != 0 {
            return Err(AetherError::Io(std::io::Error::last_os_error()));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_wal_segment_preallocation() {
        let dir = tempdir().unwrap();
        let segment = WalSegment::create(dir.path(), 1, 1024 * 1024).unwrap();
        assert_eq!(segment.capacity(), 1024 * 1024);
        assert_eq!(segment.written_bytes(), 0);

        let off1 = segment.reserve_space(4096).unwrap();
        assert_eq!(off1, 0);
        assert_eq!(segment.written_bytes(), 4096);

        let off2 = segment.reserve_space(4096).unwrap();
        assert_eq!(off2, 4096);
        assert_eq!(segment.written_bytes(), 8192);
    }
}
