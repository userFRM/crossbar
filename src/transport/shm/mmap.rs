#![allow(unsafe_code)]

//! Raw mmap wrappers with `MADV_HUGEPAGE`.
//!
//! Replaces `memmap2` with direct `libc` calls for maximum control over
//! mmap flags and to eliminate a dependency.
//!
//! On Linux, applies `MADV_HUGEPAGE` after mapping to hint the kernel to
//! back the region with transparent 2 MiB huge pages, reducing TLB misses.
//! Our 12.6 MiB default SHM region spans ~3,072 regular 4 KiB pages but
//! only ~7 huge pages — fewer TLB entries, fewer misses (~100 ns each).
//!
//! Pages are faulted lazily on first access (`MAP_SHARED` without
//! `MAP_POPULATE`), so overprovisioned configurations don't consume
//! physical memory until pages are actually touched. This keeps the
//! wrapper safe for constrained environments like Docker containers
//! with small `/dev/shm`.

use std::io;
use std::os::unix::io::AsRawFd;

// ─── Read-write mapping ─────────────────────────────────────────────────

/// Read-write memory-mapped region with optimized kernel flags.
///
/// `Deref<Target=[u8]>` is intentionally **not** implemented. This memory is
/// shared with other processes via `MAP_SHARED`, so creating `&[u8]` or
/// `&mut [u8]` references would violate Rust's aliasing model — another
/// process can write to the region at any time, making `&[u8]` unsound,
/// and `&mut [u8]` is never exclusive. Use [`as_ptr`](Self::as_ptr),
/// [`as_mut_ptr`](Self::as_mut_ptr), and [`len`](Self::len) to access
/// the mapping through raw pointers.
///
/// Calls `munmap` on drop.
pub struct RawMmap {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: The mmap region is process-shared memory. All cross-process access
// is mediated by atomic operations in the caller (region.rs, pubsub.rs).
unsafe impl Send for RawMmap {}
unsafe impl Sync for RawMmap {}

impl RawMmap {
    /// Maps `file` read-write using the file's current size.
    ///
    /// Applies `MAP_SHARED | MAP_POPULATE` and `MADV_HUGEPAGE` on Linux.
    pub fn from_file(file: &std::fs::File) -> io::Result<Self> {
        let len = file.metadata()?.len() as usize;
        Self::from_file_with_len(file, len)
    }

    /// Maps `file` read-write with an explicit length.
    ///
    /// Use this when you've just called `file.set_len()` and want to map
    /// exactly that many bytes (avoids an extra `fstat` call).
    pub fn from_file_with_len(file: &std::fs::File, len: usize) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mmap length must be > 0",
            ));
        }

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };

        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        #[cfg(target_os = "linux")]
        unsafe {
            // Best-effort: fails silently if THP is disabled system-wide
            libc::madvise(ptr, len, libc::MADV_HUGEPAGE);
        }

        Ok(Self {
            ptr: ptr as *mut u8,
            len,
        })
    }

    /// Raw const pointer to the start of the mapping.
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr as *const u8
    }

    /// Raw mutable pointer to the start of the mapping.
    #[inline]
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// Length of the mapping in bytes.
    #[inline]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.len
    }
}

impl Drop for RawMmap {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}

// ─── Read-only mapping ──────────────────────────────────────────────────

/// Read-only memory-mapped region with optimized kernel flags.
///
/// `Deref<Target=[u8]>` is intentionally **not** implemented. Although
/// this mapping is `PROT_READ`, the backing file is `MAP_SHARED` and
/// a publisher process holds a writable mapping to the same pages.
/// Creating `&[u8]` would assert exclusive read access that the hardware
/// does not guarantee, making it unsound under Rust's aliasing model.
/// Use [`as_ptr`](Self::as_ptr) and [`len`](Self::len) instead.
///
/// Calls `munmap` on drop.
pub struct RawMmapReadOnly {
    ptr: *const u8,
    len: usize,
}

unsafe impl Send for RawMmapReadOnly {}
unsafe impl Sync for RawMmapReadOnly {}

impl RawMmapReadOnly {
    /// Maps `file` read-only using the file's current size.
    pub fn from_file(file: &std::fs::File) -> io::Result<Self> {
        let len = file.metadata()?.len() as usize;
        Self::from_file_with_len(file, len)
    }

    /// Maps `file` read-only with an explicit length.
    ///
    /// Unlike [`RawMmap`], read-only mappings do **not** use `MAP_POPULATE`.
    /// Subscribers typically touch only a subset of pages (the topics they
    /// subscribe to), so pre-faulting the entire region would waste physical
    /// memory — especially in constrained environments like Docker containers
    /// with small `/dev/shm` (default 64 MiB). Pages are faulted lazily on
    /// first read. `MADV_HUGEPAGE` is still applied for TLB efficiency.
    pub fn from_file_with_len(file: &std::fs::File, len: usize) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mmap length must be > 0",
            ));
        }

        let flags = libc::MAP_SHARED;

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                flags,
                file.as_raw_fd(),
                0,
            )
        };

        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        #[cfg(target_os = "linux")]
        unsafe {
            libc::madvise(ptr, len, libc::MADV_HUGEPAGE);
        }

        Ok(Self {
            ptr: ptr as *const u8,
            len,
        })
    }

    /// Raw const pointer to the start of the mapping.
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    /// Length of the mapping in bytes.
    #[inline]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.len
    }
}

impl Drop for RawMmapReadOnly {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}
