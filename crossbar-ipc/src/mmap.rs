// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Raw memory-mapped file wrappers.
//!
//! Provides a thin, platform-specific abstraction over OS memory-mapping
//! APIs (`mmap` on Unix, `CreateFileMappingW`/`MapViewOfFile` on Windows),
//! replacing `memmap2` for maximum control over mapping flags and to
//! eliminate a dependency.
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

#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

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
/// Calls `munmap` (Unix) or `UnmapViewOfFile` (Windows) on drop.
pub struct RawMmap {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: The mmap region is process-shared memory backed by a named file in
// /dev/shm. All cross-process access is mediated by atomic operations in the
// caller (pubsub.rs). The raw pointer is never dereferenced
// without explicit unsafe blocks in those callers.
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
    ///
    /// # Safety considerations
    ///
    /// The returned mapping uses `MAP_SHARED` (Unix) or read-write file
    /// mapping (Windows), so the memory is shared with other processes.
    /// Callers must use atomic operations or other synchronization to
    /// coordinate access.
    #[cfg(unix)]
    pub fn from_file_with_len(file: &std::fs::File, len: usize) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mmap length must be > 0",
            ));
        }

        // SAFETY: We pass a valid fd, non-zero length, and standard flags.
        // The returned pointer is checked against MAP_FAILED before use.
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
        // SAFETY: ptr is a valid mmap region of `len` bytes. MADV_HUGEPAGE is
        // advisory — failure is silently ignored if THP is disabled system-wide.
        unsafe {
            libc::madvise(ptr, len, libc::MADV_HUGEPAGE);
        }

        Ok(Self {
            ptr: ptr as *mut u8,
            len,
        })
    }

    /// Maps `file` read-write with an explicit length (Windows).
    ///
    /// Uses `CreateFileMappingW` + `MapViewOfFile` to create a shared,
    /// read-write view of the file. The file-mapping handle is closed
    /// immediately after mapping — the view keeps the mapping alive.
    #[cfg(windows)]
    pub fn from_file_with_len(file: &std::fs::File, len: usize) -> io::Result<Self> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
        };

        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mmap length must be > 0",
            ));
        }

        let handle = file.as_raw_handle();

        // Split `len` into high/low 32-bit parts for CreateFileMappingW.
        let len_high = (len as u64 >> 32) as u32;
        let len_low = len as u32;

        // SAFETY: handle is a valid file handle from an open std::fs::File.
        // PAGE_READWRITE creates a read-write section object.
        let mapping = unsafe {
            CreateFileMappingW(
                handle,
                std::ptr::null(),
                PAGE_READWRITE,
                len_high,
                len_low,
                std::ptr::null(),
            )
        };

        if mapping.is_null() {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: mapping is a valid file-mapping handle.
        // FILE_MAP_ALL_ACCESS gives read-write access to the view.
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, len) };

        // Close the mapping handle — the view keeps the mapping alive.
        // SAFETY: mapping is a valid handle returned by CreateFileMappingW.
        unsafe {
            CloseHandle(mapping);
        }

        let ptr = view.Value;
        if ptr.is_null() {
            return Err(io::Error::last_os_error());
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

#[cfg(unix)]
impl Drop for RawMmap {
    fn drop(&mut self) {
        // SAFETY: ptr and len were produced by a successful mmap call in
        // from_file_with_len and have not been modified since.
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}

#[cfg(windows)]
impl Drop for RawMmap {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::{UnmapViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS};
        // SAFETY: ptr was produced by a successful MapViewOfFile call in
        // from_file_with_len and has not been modified since.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.ptr as *mut std::ffi::c_void,
            });
        }
    }
}
