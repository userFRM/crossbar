// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! SHM publisher and subscriber.

use alloc::format;
use alloc::string::ToString;
use alloc::vec;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::error::IpcError;
use crate::protocol::layout::*;
use crate::protocol::{PubSubConfig, Region};

use super::loan::{PinnedLoan, ShmLoan, TopicHandle, TypedShmLoan};
use super::mmap::RawMmap;
use super::subscription::Subscription;

// ---- File helpers ----

fn shm_path(name: &str) -> PathBuf {
    if cfg!(target_os = "linux") {
        PathBuf::from(format!("/dev/shm/crossbar-{name}"))
    } else if cfg!(windows) {
        std::env::temp_dir().join(format!("crossbar-{name}"))
    } else {
        PathBuf::from(format!("/tmp/crossbar-shm-{name}"))
    }
}

fn lock_path(name: &str) -> PathBuf {
    let mut p = shm_path(name);
    p.set_extension("lock");
    p
}

/// Validate segment name against path traversal (C2).
fn validate_name(name: &str) -> Result<(), IpcError> {
    if !is_valid_segment_name(name) {
        return Err(IpcError::InvalidRegion(format!(
            "invalid segment name '{name}': must be non-empty and contain no '/', '\\\\', '..', or null bytes"
        )));
    }
    Ok(())
}

/// Acquire an exclusive, non-blocking lock on `file`.
#[cfg(unix)]
fn exclusive_lock(file: &std::fs::File, name: &str) -> Result<(), IpcError> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(IpcError::InvalidRegion(format!(
            "pub/sub region '{name}' is already active (lock held)"
        )));
    }
    Ok(())
}

/// Acquire an exclusive, non-blocking lock on `file`.
#[cfg(windows)]
fn exclusive_lock(file: &std::fs::File, name: &str) -> Result<(), IpcError> {
    use std::os::windows::io::AsRawHandle;
    let handle = file.as_raw_handle();
    let mut overlapped: windows_sys::Win32::System::IO::OVERLAPPED = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::LockFileEx(
            handle,
            windows_sys::Win32::Storage::FileSystem::LOCKFILE_EXCLUSIVE_LOCK
                | windows_sys::Win32::Storage::FileSystem::LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if ok == 0 {
        return Err(IpcError::InvalidRegion(format!(
            "pub/sub region '{name}' is already active (lock held)"
        )));
    }
    Ok(())
}

/// Acquire or downgrade to a shared lock on `file`.
/// On Unix, this atomically downgrades an exclusive lock to shared.
/// Multiple shared locks can coexist, but shared locks prevent new exclusive locks.
#[cfg(unix)]
fn shared_lock(file: &std::fs::File, name: &str) -> Result<(), IpcError> {
    use std::os::unix::io::AsRawFd;
    // LOCK_SH without LOCK_NB: blocks until shared lock is available.
    // If this fd already holds LOCK_EX, atomically downgrades to shared.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) };
    if rc != 0 {
        return Err(IpcError::InvalidRegion(format!(
            "pub/sub region '{name}' cannot acquire shared lock"
        )));
    }
    Ok(())
}

/// Acquire or downgrade to a shared lock on `file`.
/// Note: Windows has no atomic downgrade. There is a brief unlock window
/// between releasing the exclusive lock and acquiring the shared lock.
#[cfg(windows)]
fn shared_lock(file: &std::fs::File, name: &str) -> Result<(), IpcError> {
    use std::os::windows::io::AsRawHandle;
    let handle = file.as_raw_handle();
    // Unlock first (no atomic downgrade on Windows), then reacquire as shared.
    // TOCTOU gap: another process could acquire exclusive between these calls.
    let mut overlapped: windows_sys::Win32::System::IO::OVERLAPPED = unsafe { std::mem::zeroed() };
    unsafe {
        windows_sys::Win32::Storage::FileSystem::UnlockFileEx(
            handle,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        );
    }
    let mut overlapped2: windows_sys::Win32::System::IO::OVERLAPPED = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::LockFileEx(
            handle,
            0, // 0 = shared lock, blocking
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped2,
        )
    };
    if ok == 0 {
        return Err(IpcError::InvalidRegion(format!(
            "pub/sub region '{name}' cannot acquire shared lock"
        )));
    }
    Ok(())
}

/// Returns a stable numeric identity for the file at `path`.
/// Uses inode on Unix, file index on Windows.
/// Returns `None` on error (prevents accidental deletion of wrong file).
#[cfg(unix)]
fn file_identity(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).map(|m| m.ino()).ok()
}

/// Returns a stable numeric identity for the file at `path`.
#[cfg(windows)]
fn file_identity(path: &std::path::Path) -> Option<u64> {
    use std::os::windows::io::AsRawHandle;
    let file = std::fs::File::open(path).ok()?;
    let handle = file.as_raw_handle();
    let mut info: windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION =
        unsafe { std::mem::zeroed() };
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle(handle, &mut info)
    };
    if ok == 0 {
        return None;
    }
    Some((info.nFileIndexHigh as u64) << 32 | info.nFileIndexLow as u64)
}

/// Generate a unique publisher ID using PID + atomic counter + thread ID hash.
fn generate_publisher_id() -> u64 {
    use core::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = u64::from(std::process::id());
    // Combine PID, monotonic counter, and thread ID for uniqueness
    let thread_hash = {
        let id = std::thread::current().id();
        // Thread IDs are opaque, hash via Debug formatting
        let s = alloc::format!("{id:?}");
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in s.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h
    };
    pid ^ seq ^ thread_hash
}

/// Shared logic for opening and validating an existing SHM region.
/// Used by both `ShmPublisher::open()` and `ShmSubscriber::connect()`.
fn open_and_validate_region(
    name: &str,
    needs_write: bool,
) -> Result<(RawMmap, Arc<Region>, PathBuf), IpcError> {
    let path = shm_path(name);

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(needs_write) // needed for atomic CAS on refcounts
        .open(&path)
        .map_err(IpcError::Io)?;

    let mmap = RawMmap::from_file(&file).map_err(IpcError::Io)?;

    if mmap.len() < HEADER_SIZE {
        return Err(IpcError::InvalidRegion(
            "region too small for header".into(),
        ));
    }

    // Validate header
    let ptr = mmap.as_ptr();
    unsafe {
        let mut magic = [0u8; 8];
        core::ptr::copy_nonoverlapping(ptr.add(GH_MAGIC), magic.as_mut_ptr(), 8);
        if &magic != MAGIC {
            return Err(IpcError::InvalidRegion(
                "invalid magic (expected XBAR_ZC)".into(),
            ));
        }
        let ver = (ptr.add(GH_VERSION) as *const u32).read();
        if ver != VERSION {
            return Err(IpcError::InvalidRegion(format!(
                "unsupported version {ver}, expected {VERSION}"
            )));
        }
    }

    // Read config from header
    let config = unsafe {
        PubSubConfig {
            max_topics: (ptr.add(GH_MAX_TOPICS) as *const u32).read(),
            block_count: (ptr.add(GH_BLOCK_COUNT) as *const u32).read(),
            block_size: (ptr.add(GH_BLOCK_SIZE) as *const u32).read(),
            ring_depth: (ptr.add(GH_RING_DEPTH) as *const u32).read(),
            stale_timeout: Duration::from_micros(
                (ptr.add(GH_STALE_TIMEOUT_US) as *const u64).read(),
            ),
            ..PubSubConfig::default()
        }
    };

    // Validate config read from SHM header (H5: defense against malicious header)
    if config.block_size < BLOCK_DATA_OFFSET as u32 + 1 {
        return Err(IpcError::InvalidRegion(format!(
            "header block_size {} too small",
            config.block_size
        )));
    }
    if !config.ring_depth.is_power_of_two() || config.ring_depth == 0 {
        return Err(IpcError::InvalidRegion(format!(
            "header ring_depth {} is not a power of 2",
            config.ring_depth
        )));
    }
    if config.block_count == 0 {
        return Err(IpcError::InvalidRegion("header block_count is 0".into()));
    }
    if config.max_topics == 0 {
        return Err(IpcError::InvalidRegion("header max_topics is 0".into()));
    }
    if config.max_topics > MAX_TOPICS_LIMIT {
        return Err(IpcError::InvalidRegion(format!(
            "header max_topics {} exceeds limit {MAX_TOPICS_LIMIT}",
            config.max_topics
        )));
    }

    // Use checked arithmetic to prevent overflow (M6)
    let expected_size = region_size_checked(&config)
        .ok_or_else(|| IpcError::InvalidRegion("region size computation overflow".into()))?;

    if mmap.len() < expected_size {
        return Err(IpcError::InvalidRegion(format!(
            "region size {} < expected {expected_size}",
            mmap.len()
        )));
    }

    // SAFETY: mmap provides a valid region of the computed size.
    let region = Arc::new(unsafe { Region::from_raw(mmap.as_mut_ptr(), mmap.len(), config) });

    region.check_heartbeat()?;

    Ok((mmap, region, path))
}

// ---- ShmPublisher ----

/// O(1) zero-copy publisher over shared memory.
///
/// Uses a shared block pool (Treiber stack) for data storage and a ring of
/// block indices for publication. Transfer cost is O(1) regardless of payload
/// size -- only 8 bytes (block index + data length) are written to the ring.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::*;
///
/// let mut pub_ = ShmPublisher::create("prices", PubSubConfig::default()).unwrap();
/// let topic = pub_.register("/tick/AAPL").unwrap();
///
/// let mut loan = pub_.loan(&topic).unwrap();
/// loan.as_mut_slice()[..8].copy_from_slice(&42u64.to_le_bytes());
/// loan.set_len(8).unwrap();
/// loan.publish(); // O(1) -- writes 8 bytes to ring
/// ```
pub struct ShmPublisher {
    _mmap: RawMmap,
    region: Arc<Region>,
    path: PathBuf,
    _lock_file: Option<std::fs::File>,
    created_ino: Option<u64>,
    is_owner: bool,
    id: u64,
    last_heartbeat: std::time::Instant,
    loan_count: u32,
    block_cache: [u32; 8],
    cache_len: u8,
    pinned_blocks: alloc::vec::Vec<u32>,
}

impl ShmPublisher {
    /// Allocates a block from the local cache, refilling from the global pool on miss.
    /// Checks the recycled (cache-warm) block first for optimal L1/L2 reuse.
    fn alloc_cached(&mut self) -> Option<u32> {
        // First: check for a recycled block (cache-warm from last publish)
        if let Some(idx) = self.region.alloc_recycled() {
            return Some(idx);
        }
        // Then: check the local cache
        if self.cache_len > 0 {
            self.cache_len -= 1;
            return Some(self.block_cache[self.cache_len as usize]);
        }
        // Finally: refill from global Treiber stack
        while (self.cache_len as usize) < self.block_cache.len() {
            match self.region.alloc_block() {
                Some(idx) => {
                    self.block_cache[self.cache_len as usize] = idx;
                    self.cache_len += 1;
                }
                None => break,
            }
        }
        if self.cache_len > 0 {
            self.cache_len -= 1;
            Some(self.block_cache[self.cache_len as usize])
        } else {
            None
        }
    }

    /// Shared preamble for `loan` and `loan_typed`: heartbeat check + block alloc.
    /// Returns (block_idx, topic_idx). Atomic refs are computed by the caller
    /// from `self.region` to avoid borrow conflicts.
    fn loan_preamble(&mut self, handle: &TopicHandle) -> Result<(u32, u32), IpcError> {
        // Runtime check: reject handles from a different publisher (Codex HIGH)
        if handle.publisher_id != self.id {
            return Err(IpcError::InvalidRegion(
                "TopicHandle belongs to a different ShmPublisher".into(),
            ));
        }

        // Counter-based heartbeat: check clock every 1024 loans, not every loan.
        self.loan_count = self.loan_count.wrapping_add(1);
        if self.loan_count & 0x3FF == 0
            && self.last_heartbeat.elapsed() >= self.region.config.heartbeat_interval
        {
            // Only advance last_heartbeat if the update succeeded
            if self.region.update_heartbeat().is_ok() {
                self.last_heartbeat = std::time::Instant::now();
            }
        }

        let block_idx = self.alloc_cached().ok_or(IpcError::PoolExhausted)?;

        Ok((block_idx, handle.topic_idx))
    }

    /// Creates a new pool-backed pub/sub region.
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::Io`] if the backing file cannot be created,
    /// or [`IpcError::InvalidRegion`] if another publisher is active.
    pub fn create(name: &str, config: PubSubConfig) -> Result<Self, IpcError> {
        // Validate segment name against path traversal (C2)
        validate_name(name)?;

        // Validate config to prevent panics from invalid values
        if config.block_size < BLOCK_DATA_OFFSET as u32 + 1 {
            return Err(IpcError::InvalidRegion(format!(
                "block_size must be at least {} (got {})",
                BLOCK_DATA_OFFSET + 1,
                config.block_size
            )));
        }
        if !config.ring_depth.is_power_of_two() {
            return Err(IpcError::InvalidRegion(format!(
                "ring_depth must be a power of 2 (got {})",
                config.ring_depth
            )));
        }
        if config.block_count == 0 {
            return Err(IpcError::InvalidRegion(
                "block_count must be at least 1".to_string(),
            ));
        }
        if config.max_topics == 0 {
            return Err(IpcError::InvalidRegion(
                "max_topics must be at least 1".to_string(),
            ));
        }

        // Check for overflow in region size computation (M6)
        let size = region_size_checked(&config)
            .ok_or_else(|| IpcError::InvalidRegion("region size computation overflow".into()))?;

        let path = shm_path(name);
        let lpath = lock_path(name);

        let lock_file = {
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600); // H8: restrictive permissions
            }
            opts.open(&lpath).map_err(IpcError::Io)?
        };

        // Exclusive lock -- only one publisher per region
        exclusive_lock(&lock_file, name)?;

        // Remove stale file
        let _ = std::fs::remove_file(&path);

        let file = {
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true).write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600); // H8: restrictive permissions
            }
            opts.open(&path).map_err(IpcError::Io)?
        };
        file.set_len(size as u64).map_err(IpcError::Io)?;

        let mmap = RawMmap::from_file_with_len(&file, size).map_err(IpcError::Io)?;

        // Write global header
        let ptr = mmap.as_mut_ptr();
        unsafe {
            core::ptr::copy_nonoverlapping(MAGIC.as_ptr(), ptr.add(GH_MAGIC), 8);
            (ptr.add(GH_VERSION) as *mut u32).write(VERSION);
            (ptr.add(GH_MAX_TOPICS) as *mut u32).write(config.max_topics);
            (ptr.add(GH_BLOCK_COUNT) as *mut u32).write(config.block_count);
            (ptr.add(GH_BLOCK_SIZE) as *mut u32).write(config.block_size);
            (ptr.add(GH_RING_DEPTH) as *mut u32).write(config.ring_depth);
            let pid = u64::from(std::process::id());
            (ptr.add(GH_PID) as *mut u64).write(pid);
            #[allow(clippy::cast_possible_truncation)]
            (ptr.add(GH_STALE_TIMEOUT_US) as *mut u64)
                .write(config.stale_timeout.as_micros() as u64);
        }

        // SAFETY: mmap provides a valid region of the computed size.
        let region = Arc::new(unsafe { Region::from_raw(mmap.as_mut_ptr(), mmap.len(), config) });

        // Initialize pool free list
        region.init_free_list();
        region.update_heartbeat()?;

        // Initialize all ring entries to NO_BLOCK
        for t in 0..config.max_topics {
            for s in 0..config.ring_depth {
                let off = ring_entry_off(&config, t, s);
                unsafe {
                    let base = mmap.as_mut_ptr().add(off);
                    (base.add(RE_SEQ) as *mut u64).write(0);
                    (base.add(RE_BLOCK_IDX) as *mut u32).write(NO_BLOCK);
                    (base.add(RE_DATA_LEN) as *mut u32).write(0);
                }
            }
        }

        // Downgrade exclusive lock to shared. This allows secondary publishers
        // (open()) to acquire shared locks, while all shared locks together
        // prevent a new create() from acquiring exclusive.
        shared_lock(&lock_file, name)?;

        let created_ino = file_identity(&path);
        let id = generate_publisher_id();

        Ok(ShmPublisher {
            _mmap: mmap,
            region,
            path,
            _lock_file: Some(lock_file),
            created_ino,
            is_owner: true,
            id,
            last_heartbeat: std::time::Instant::now(),
            loan_count: 0,
            block_cache: [0; 8],
            cache_len: 0,
            pinned_blocks: vec![NO_BLOCK; config.max_topics as usize],
        })
    }

    /// Opens an existing pub/sub region as a secondary publisher.
    ///
    /// Unlike [`create`](Self::create), this does not create the SHM file or
    /// hold an exclusive lock. The region must already exist (created by
    /// another `ShmPublisher::create` call). Config is read from the header.
    ///
    /// # Errors
    ///
    /// Returns an error if the region file doesn't exist, has invalid
    /// magic/version, or the publisher's heartbeat is stale.
    pub fn open(name: &str) -> Result<Self, IpcError> {
        validate_name(name)?;

        let lpath = lock_path(name);

        // Acquire a shared lock on the lock file. This prevents a new
        // create() from truncating the region while we're using it.
        let lock_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lpath)
            .map_err(IpcError::Io)?;
        shared_lock(&lock_file, name)?;

        let (mmap, region, path) = open_and_validate_region(name, true)?;
        let id = generate_publisher_id();
        let max_topics = region.config.max_topics;

        Ok(ShmPublisher {
            _mmap: mmap,
            region,
            path,
            _lock_file: Some(lock_file),
            created_ino: None,
            is_owner: false,
            id,
            last_heartbeat: std::time::Instant::now(),
            loan_count: 0,
            block_cache: [0; 8],
            cache_len: 0,
            pinned_blocks: vec![NO_BLOCK; max_topics as usize],
        })
    }

    /// Registers a topic URI (untyped). Returns a handle for use with [`loan`](Self::loan).
    ///
    /// # Errors
    ///
    /// Returns an error if the maximum number of topics has been reached
    /// or the URI exceeds 64 bytes.
    pub fn register(&mut self, uri: &str) -> Result<TopicHandle, IpcError> {
        self.register_inner(uri, 0)
    }

    /// Registers a typed topic URI. Returns a handle for use with
    /// [`loan_typed`](Self::loan_typed).
    ///
    /// The type size is stored in the topic entry so that subscribers can
    /// verify the expected type size at receive time.
    ///
    /// # Errors
    ///
    /// Returns an error if `T`'s alignment exceeds 8, the maximum number
    /// of topics has been reached, or the URI exceeds 64 bytes.
    pub fn register_typed<T: crate::Pod>(&mut self, uri: &str) -> Result<TopicHandle, IpcError> {
        const { assert!(core::mem::align_of::<u128>() <= 16) }; // sanity
        if core::mem::align_of::<T>() > 8 {
            return Err(IpcError::InvalidRegion(format!(
                "Pod type alignment ({}) exceeds block data offset (8)",
                core::mem::align_of::<T>()
            )));
        }
        self.register_inner(uri, core::mem::size_of::<T>() as u32)
    }

    fn register_inner(&mut self, uri: &str, type_size: u32) -> Result<TopicHandle, IpcError> {
        if uri.len() > TE_URI_MAX {
            return Err(IpcError::InvalidRegion(format!(
                "topic URI too long ({} > {TE_URI_MAX})",
                uri.len()
            )));
        }
        let hash = uri_hash(uri);

        let base_ptr = self.region.base_ptr();

        // Phase 1: Scan for existing topic (check active==ACTIVE slots for URI match)
        for i in 0..self.region.config.max_topics {
            let off = topic_entry_off(i);
            let active = unsafe { &*(base_ptr.add(off + TE_ACTIVE) as *const AtomicU32) };

            if active.load(Ordering::Acquire) == TE_STATE_ACTIVE {
                // Check if same URI already registered (hash + byte comparison)
                let existing_hash =
                    unsafe { (base_ptr.add(off + TE_URI_HASH) as *const u64).read() };
                if existing_hash == hash {
                    let existing_len =
                        unsafe { (base_ptr.add(off + TE_URI_LEN) as *const u32).read() } as usize;
                    let existing_bytes = unsafe {
                        core::slice::from_raw_parts(base_ptr.add(off + TE_URI), existing_len)
                    };
                    if existing_len == uri.len() && existing_bytes == uri.as_bytes() {
                        return Ok(TopicHandle {
                            topic_idx: i,
                            publisher_id: self.id,
                        });
                    }
                    // Hash collision -- different URI, keep searching
                }
            }
        }

        // Phase 2: Claim a free slot via CAS (0=free -> 2=initializing)
        for i in 0..self.region.config.max_topics {
            let off = topic_entry_off(i);
            let active = unsafe { &*(base_ptr.add(off + TE_ACTIVE) as *const AtomicU32) };

            // Skip slots that are not free (already active or being initialized)
            if active
                .compare_exchange(
                    TE_STATE_FREE,
                    TE_STATE_INIT,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                continue;
            }

            // We own this slot (state == INIT). Write topic data.
            unsafe {
                let base = base_ptr.add(off);
                (base.add(TE_URI_HASH) as *mut u64).write(hash);
                (base.add(TE_URI_LEN) as *mut u32).write(uri.len() as u32);
                core::ptr::copy_nonoverlapping(
                    uri.as_bytes().as_ptr(),
                    base.add(TE_URI),
                    uri.len(),
                );
                // Initialize write_seq to 0
                (base.add(TE_WRITE_SEQ) as *mut u64).write(0);
                // Write type_size (0 for untyped)
                (base.add(TE_TYPE_SIZE) as *mut u32).write(type_size);
            }
            // Transition INIT -> ACTIVE (now visible to subscribers)
            active.store(TE_STATE_ACTIVE, Ordering::Release);

            return Ok(TopicHandle {
                topic_idx: i,
                publisher_id: self.id,
            });
        }

        Err(IpcError::InvalidRegion("maximum topics reached".into()))
    }

    /// Updates the publisher heartbeat. Call this periodically during idle
    /// periods (when not calling [`loan`](Self::loan)) to prevent subscribers
    /// from treating the publisher as dead.
    ///
    /// `loan()` updates the heartbeat automatically every 1024 calls, so you
    /// only need this if the publisher may be idle longer than `stale_timeout`.
    /// Updates the publisher heartbeat.
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::ClockError`] if the system clock is before UNIX epoch.
    pub fn heartbeat(&mut self) -> Result<(), IpcError> {
        self.region.update_heartbeat()?;
        self.last_heartbeat = std::time::Instant::now();
        Ok(())
    }

    /// Loans a block from the pool for writing. Write your data, then call
    /// [`publish`](ShmLoan::publish) to make it visible to subscribers.
    ///
    /// If the loan is dropped without publishing, the block is returned to
    /// the pool automatically.
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::PoolExhausted`] if all blocks are in use.
    #[inline]
    pub fn loan(&mut self, handle: &TopicHandle) -> Result<ShmLoan<'_>, IpcError> {
        let (block_idx, topic_idx) = self.loan_preamble(handle)?;
        let off = topic_entry_off(topic_idx);
        let base_ptr = self.region.base_ptr();
        let write_seq_atom = unsafe { &*(base_ptr.add(off + TE_WRITE_SEQ) as *const AtomicU64) };
        let waiters_atom = unsafe { &*(base_ptr.add(off + TE_WAITERS) as *const AtomicU32) };
        let data_ptr = unsafe { self.region.block_ptr(block_idx).add(BLOCK_DATA_OFFSET) };

        Ok(ShmLoan {
            region: &self.region,
            data_ptr,
            capacity: self.region.data_capacity(),
            len: 0,
            block_idx,
            topic_idx,
            write_seq_atom,
            waiters_atom,
            single_publisher: self.is_owner,
        })
    }

    /// Loans a typed block from the pool for writing a `T: Pod` value.
    ///
    /// Use [`TypedShmLoan::send`] to write and publish in one step,
    /// or [`TypedShmLoan::as_mut`] + [`TypedShmLoan::publish`] to
    /// fill fields individually (born-in-SHM pattern).
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::PoolExhausted`] if all blocks are in use.
    #[inline]
    pub fn loan_typed<T: crate::Pod>(
        &mut self,
        handle: &TopicHandle,
    ) -> Result<TypedShmLoan<'_, T>, IpcError> {
        let (block_idx, topic_idx) = self.loan_preamble(handle)?;
        let off = topic_entry_off(topic_idx);
        let base_ptr = self.region.base_ptr();
        let write_seq_atom = unsafe { &*(base_ptr.add(off + TE_WRITE_SEQ) as *const AtomicU64) };
        let waiters_atom = unsafe { &*(base_ptr.add(off + TE_WAITERS) as *const AtomicU32) };

        Ok(TypedShmLoan {
            region: &self.region,
            block_idx,
            topic_idx,
            write_seq_atom,
            waiters_atom,
            single_publisher: self.is_owner,
            _marker: core::marker::PhantomData,
        })
    }

    /// Loans the pinned block for a topic. On the first call, allocates a
    /// dedicated block. Subsequent calls return the same block -- no alloc.
    ///
    /// The publisher uses a CAS-based writer sentinel to prevent data races:
    /// if any subscriber holds a [`PinnedGuard`] for this topic, this method
    /// returns an error.
    ///
    /// # Errors
    ///
    /// - [`IpcError::PinnedReadersActive`] if a subscriber holds a `PinnedGuard`.
    /// - [`IpcError::PoolExhausted`] if the pool is exhausted (first call only).
    pub fn loan_pinned(&mut self, handle: &TopicHandle) -> Result<PinnedLoan<'_>, IpcError> {
        if handle.publisher_id != self.id {
            return Err(IpcError::InvalidRegion(
                "TopicHandle belongs to a different ShmPublisher".into(),
            ));
        }
        let topic_idx = handle.topic_idx;

        // Bounds check (H2: pinned_blocks is now Vec sized to max_topics)
        if topic_idx as usize >= self.pinned_blocks.len() {
            return Err(IpcError::InvalidRegion(format!(
                "topic_idx {} >= max_topics {}",
                topic_idx,
                self.pinned_blocks.len()
            )));
        }

        // Get or allocate the pinned block for this topic
        let block_idx = if self.pinned_blocks[topic_idx as usize] != NO_BLOCK {
            self.pinned_blocks[topic_idx as usize]
        } else {
            let idx = self.alloc_cached().ok_or(IpcError::PoolExhausted)?;
            self.pinned_blocks[topic_idx as usize] = idx;
            // Store in SHM topic entry so subscribers can find it
            self.region.set_pinned_block(topic_idx, idx);
            idx
        };

        // CAS-based writer sentinel: atomically swap readers from 0 to
        // PINNED_WRITER_ACTIVE. If CAS fails, a subscriber is active.
        let readers = self.region.pinned_readers(topic_idx);
        match readers.compare_exchange(0, PINNED_WRITER_ACTIVE, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {} // Successfully claimed — no readers, sentinel is set
            Err(current) => {
                let count = if current == PINNED_WRITER_ACTIVE {
                    0 // Another writer — shouldn't happen with &mut self, but handle it
                } else {
                    current
                };
                return Err(IpcError::PinnedReadersActive { count, topic_idx });
            }
        }

        let pinned_off = topic_entry_off(topic_idx);
        let bp = self.region.base_ptr();
        let write_seq_atom = unsafe { &*(bp.add(pinned_off + TE_WRITE_SEQ) as *const AtomicU64) };
        let waiters_atom = unsafe { &*(bp.add(pinned_off + TE_WAITERS) as *const AtomicU32) };
        let data_ptr = unsafe { self.region.block_ptr(block_idx).add(BLOCK_DATA_OFFSET) };

        Ok(PinnedLoan {
            region: &self.region,
            data_ptr,
            capacity: self.region.data_capacity(),
            len: 0,
            topic_idx,
            write_seq_atom,
            waiters_atom,
            readers,
        })
    }
}

impl Drop for ShmPublisher {
    fn drop(&mut self) {
        // Free pinned blocks
        for &blk in &self.pinned_blocks {
            if blk != NO_BLOCK {
                self.region.free_block(blk);
            }
        }
        // Return recycled block to pool
        if let Some(idx) = self.region.alloc_recycled() {
            self.region.free_block(idx);
        }
        // Return cached blocks to the global pool
        for i in 0..self.cache_len as usize {
            self.region.free_block(self.block_cache[i]);
        }
        self.cache_len = 0;

        // Only delete the SHM file if we're the owner and the file identity matches.
        // Uses Option to prevent deletion on filesystem errors (M15).
        if self.is_owner {
            if let (Some(created), Some(current)) = (self.created_ino, file_identity(&self.path)) {
                if created == current {
                    let _ = std::fs::remove_file(&self.path);
                }
            }
        }
    }
}

// ---- ShmSubscriber ----

/// O(1) zero-copy subscriber for pool-backed pub/sub.
///
/// Connects to an existing [`ShmPublisher`] region. Returns safe
/// [`super::subscription::SampleGuard`] references -- no `unsafe` needed to read data.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::*;
///
/// let sub = ShmSubscriber::connect("prices").unwrap();
/// let stream = sub.subscribe("/tick/AAPL").unwrap();
///
/// if let Some(sample) = stream.try_recv() {
///     println!("data: {:?}", &*sample); // safe Deref<Target=[u8]>
/// };
/// ```
pub struct ShmSubscriber {
    _mmap: RawMmap,
    region: Arc<Region>,
}

impl ShmSubscriber {
    /// Connects to an existing pool pub/sub region.
    ///
    /// # Errors
    ///
    /// Returns an error if the region file doesn't exist, has invalid
    /// magic/version, or the publisher's heartbeat is stale.
    pub fn connect(name: &str) -> Result<Self, IpcError> {
        validate_name(name)?;

        let (mmap, region, _path) = open_and_validate_region(name, true)?;

        Ok(ShmSubscriber {
            _mmap: mmap,
            region,
        })
    }

    /// Subscribes to a topic by URI.
    ///
    /// # Errors
    ///
    /// Returns an error if the topic is not registered by the publisher.
    pub fn subscribe(&self, uri: &str) -> Result<Subscription, IpcError> {
        let hash = uri_hash(uri);

        let base_ptr = self.region.base_ptr();

        for i in 0..self.region.config.max_topics {
            let off = topic_entry_off(i);
            let active = unsafe { &*(base_ptr.add(off + TE_ACTIVE) as *const AtomicU32) };
            if active.load(Ordering::Acquire) != TE_STATE_ACTIVE {
                continue;
            }
            let stored_hash = unsafe { (base_ptr.add(off + TE_URI_HASH) as *const u64).read() };
            if stored_hash != hash {
                continue;
            }
            // Verify URI bytes match (not just hash) to prevent collision aliasing
            let stored_len =
                unsafe { (base_ptr.add(off + TE_URI_LEN) as *const u32).read() } as usize;
            if stored_len != uri.len() {
                continue;
            }
            let stored_bytes =
                unsafe { core::slice::from_raw_parts(base_ptr.add(off + TE_URI), stored_len) };
            if stored_bytes != uri.as_bytes() {
                continue;
            }

            let write_seq = unsafe { &*(base_ptr.add(off + TE_WRITE_SEQ) as *const AtomicU64) };
            // Start from latest -- don't replay history
            let current = write_seq.load(Ordering::Acquire);

            let type_size = unsafe { (base_ptr.add(off + TE_TYPE_SIZE) as *const u32).read() };

            return Ok(Subscription {
                region: Arc::clone(&self.region),
                topic_idx: i,
                last_seq: std::cell::Cell::new(current),
                type_size,
            });
        }

        Err(IpcError::InvalidRegion(format!("topic '{uri}' not found")))
    }
}
