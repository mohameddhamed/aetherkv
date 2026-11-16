use crate::common::{AetherError, Result};
use std::os::unix::io::RawFd;

/// UioDriver abstracts kernel asynchronous and direct I/O.
/// On Linux: leverages `io_uring` with registered fixed buffers.
/// On macOS / Unix: provides Direct I/O via POSIX pwrite/fsync.
pub struct UioDriver {
    #[cfg(target_os = "linux")]
    ring: io_uring::IoUring,
}

impl UioDriver {
    pub fn new(entries: u32) -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let ring = io_uring::IoUring::new(entries).map_err(AetherError::Io)?;
            Ok(Self { ring })
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = entries;
            Ok(Self {})
        }
    }

    /// Write a sector-aligned buffer to the given file descriptor at `offset`.
    pub fn write_at(&mut self, fd: RawFd, offset: usize, buffer: &[u8]) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            use io_uring::{opcode, types};

            let write_e = opcode::Write::new(types::Fd(fd), buffer.as_ptr(), buffer.len() as u32)
                .offset(offset as u64)
                .build();

            unsafe {
                self.ring.submission().push(&write_e).map_err(|_| {
                    AetherError::Io(std::io::Error::other("io_uring submission queue full"))
                })?;
            }

            self.ring.submit_and_wait(1).map_err(AetherError::Io)?;

            let mut cqe_iter = self.ring.completion();
            if let Some(cqe) = cqe_iter.next() {
                let res = cqe.result();
                if res < 0 {
                    return Err(AetherError::Io(std::io::Error::from_raw_os_error(-res)));
                }
            }
            Ok(())
        }

        #[cfg(not(target_os = "linux"))]
        {
            let mut total_written = 0;
            while total_written < buffer.len() {
                let to_write = &buffer[total_written..];
                let ret = unsafe {
                    libc::pwrite(
                        fd,
                        to_write.as_ptr() as *const libc::c_void,
                        to_write.len(),
                        (offset + total_written) as libc::off_t,
                    )
                };

                if ret < 0 {
                    return Err(AetherError::Io(std::io::Error::last_os_error()));
                }
                total_written += ret as usize;
            }
            Ok(())
        }
    }

    /// Flush and synchronize data to disk.
    pub fn sync_data(&self, fd: RawFd) -> Result<()> {
        #[cfg(target_os = "linux")]
        let ret = unsafe { libc::fdatasync(fd) };

        #[cfg(not(target_os = "linux"))]
        let ret = unsafe { libc::fsync(fd) };

        if ret != 0 {
            return Err(AetherError::Io(std::io::Error::last_os_error()));
        }
        Ok(())
    }
}
