// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! SHM publisher and subscriber.

use alloc::format;
use alloc::string::ToString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::error::IpcError;
use crate::protocol::layout::*;
use crate::protocol::{PubSubConfig, Region};

use super::loan::{ShmLoan, TopicHandle, TypedShmLoan};
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
#[cfg(windows)]
fn shared_lock(file: &std::fs::File, name: &str) -> Result<(), IpcError> {
    use std::os::windows::io::AsRawHandle;
    let handle = file.as_raw_handle();
    // Unlock first (no atomic downgrade on Windows), then reacquire as shared.
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
#[cfg(unix)]
fn file_identity(path: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).map(|m| m.ino()).unwrap_or(0)
}

/// Returns a stable numeric identity for the file at `path`.
/// Uses inode on Unix, file index on Windows.
#[cfg(windows)]
fn file_identity(path: &std::path::Path) -> u64 {
    use std::os::windows::io::AsRawHandle;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return 0,
    };
    let handle = file.as_raw_handle();
    let mut info: windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION =
        unsafe { std::mem::zeroed() };
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle(handle, &mut info)
    };
    if ok == 0 {
        return 0;
    }
    (info.nFileIndexHigh as u64) << 32 | info.nFileIndexLow as u64
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
/// let mut loan = pub_.loan(&topic);
/// loan.as_mut_slice()[..8].copy_from_slice(&42u64.to_le_bytes());
/// loan.set_len(8);
/// loan.publish(); // O(1) -- writes 8 bytes to ring
/// ```
pub struct ShmPublisher {
    _mmap: RawMmap,
    region: Arc<Region>,
    path: PathBuf,
    _lock_file: Option<std::fs::File>,
    created_ino: u64,
    is_owner: bool,
    id: u64,
    last_heartbeat: std::time::Instant,
    loan_count: u32,
}

impl ShmPublisher {
    /// Creates a new pool-backed pub/sub region.
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::Io`] if the backing file cannot be created,
    /// or [`IpcError::InvalidRegion`] if another publisher is active.
    pub fn create(name: &str, config: PubSubConfig) -> Result<Self, IpcError> {
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

        let path = shm_path(name);
        let lpath = lock_path(name);

        let lock_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lpath)
            .map_err(IpcError::Io)?;

        // Exclusive lock -- only one publisher per region
        exclusive_lock(&lock_file, name)?;

        // Remove stale file
        let _ = std::fs::remove_file(&path);

        let size = region_size(&config);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(IpcError::Io)?;
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
            (ptr.add(GH_STALE_TIMEOUT_US) as *mut u64)
                .write(config.stale_timeout.as_micros() as u64);
        }

        // SAFETY: mmap provides a valid region of the computed size.
        let region = Arc::new(unsafe { Region::from_raw(mmap.as_mut_ptr(), mmap.len(), config) });

        // Initialize pool free list
        region.init_free_list();
        region.update_heartbeat();

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

        #[allow(clippy::cast_possible_truncation)]
        let id = u64::from(std::process::id())
            ^ (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64);

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
        let path = shm_path(name);
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

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true) // needed for atomic CAS on refcounts
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

        let expected_size = region_size(&config);
        if mmap.len() < expected_size {
            return Err(IpcError::InvalidRegion(format!(
                "region size {} < expected {expected_size}",
                mmap.len()
            )));
        }

        // SAFETY: mmap provides a valid region of the computed size.
        let region = Arc::new(unsafe { Region::from_raw(mmap.as_mut_ptr(), mmap.len(), config) });

        region.check_heartbeat()?;

        #[allow(clippy::cast_possible_truncation)]
        let id = u64::from(std::process::id())
            ^ (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64);

        Ok(ShmPublisher {
            _mmap: mmap,
            region,
            path,
            _lock_file: Some(lock_file),
            created_ino: 0,
            is_owner: false,
            id,
            last_heartbeat: std::time::Instant::now(),
            loan_count: 0,
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
    /// # Panics
    ///
    /// Panics if `T`'s alignment exceeds 8 (the block data offset).
    ///
    /// # Errors
    ///
    /// Returns an error if the maximum number of topics has been reached
    /// or the URI exceeds 64 bytes.
    pub fn register_typed<T: crate::Pod>(&mut self, uri: &str) -> Result<TopicHandle, IpcError> {
        assert!(
            core::mem::align_of::<T>() <= 8,
            "Pod type alignment ({}) exceeds block data offset (8)",
            core::mem::align_of::<T>()
        );
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

        let base_ptr = self.region.base;

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
    pub fn heartbeat(&mut self) {
        self.region.update_heartbeat();
        self.last_heartbeat = std::time::Instant::now();
    }

    /// Loans a block from the pool for writing. Write your data, then call
    /// [`publish`](ShmLoan::publish) to make it visible to subscribers.
    ///
    /// If the loan is dropped without publishing, the block is returned to
    /// the pool automatically.
    ///
    /// # Panics
    ///
    /// Panics if the pool is exhausted (all blocks in use) or if the handle
    /// belongs to a different publisher.
    #[inline]
    pub fn loan(&mut self, handle: &TopicHandle) -> ShmLoan<'_> {
        assert_eq!(
            handle.publisher_id, self.id,
            "TopicHandle belongs to a different ShmPublisher"
        );

        // Counter-based heartbeat: check clock every 1024 loans, not every loan.
        // Saves ~20ns per loan by avoiding Instant::now() on the hot path.
        // All publishers update the heartbeat — it signals "region is alive",
        // not "primary is alive". fetch_max in update_heartbeat() handles races.
        self.loan_count = self.loan_count.wrapping_add(1);
        if self.loan_count & 0x3FF == 0
            && self.last_heartbeat.elapsed() >= self.region.config.heartbeat_interval
        {
            self.region.update_heartbeat();
            self.last_heartbeat = std::time::Instant::now();
        }

        let block_idx = self
            .region
            .alloc_block()
            .expect("pool exhausted -- increase block_count in PubSubConfig");

        let topic_idx = handle.topic_idx;
        let off = topic_entry_off(topic_idx);

        let base_ptr = self.region.base;

        let write_seq_atom = unsafe { &*(base_ptr.add(off + TE_WRITE_SEQ) as *const AtomicU64) };

        let notify_atom = unsafe { &*(base_ptr.add(off + TE_NOTIFY) as *const AtomicU32) };

        let waiters_atom = unsafe { &*(base_ptr.add(off + TE_WAITERS) as *const AtomicU32) };

        let data_ptr = unsafe { self.region.block_ptr(block_idx).add(BLOCK_DATA_OFFSET) };

        ShmLoan {
            region: &self.region,
            data_ptr,
            capacity: self.region.data_capacity(),
            len: 0,
            block_idx,
            topic_idx,
            write_seq_atom,
            notify_atom,
            waiters_atom,
        }
    }

    /// Loans a typed block from the pool for writing a `T: Pod` value.
    ///
    /// Use [`TypedShmLoan::send`] to write and publish in one step,
    /// or [`TypedShmLoan::as_mut`] + [`TypedShmLoan::publish`] to
    /// fill fields individually (born-in-SHM pattern).
    ///
    /// # Panics
    ///
    /// Panics if the pool is exhausted or the handle belongs to a different
    /// publisher.
    #[inline]
    pub fn loan_typed<T: crate::Pod>(&mut self, handle: &TopicHandle) -> TypedShmLoan<'_, T> {
        assert_eq!(
            handle.publisher_id, self.id,
            "TopicHandle belongs to a different ShmPublisher"
        );

        // Counter-based heartbeat: check clock every 1024 loans, not every loan.
        self.loan_count = self.loan_count.wrapping_add(1);
        if self.loan_count & 0x3FF == 0
            && self.last_heartbeat.elapsed() >= self.region.config.heartbeat_interval
        {
            self.region.update_heartbeat();
            self.last_heartbeat = std::time::Instant::now();
        }

        let block_idx = self
            .region
            .alloc_block()
            .expect("pool exhausted -- increase block_count in PubSubConfig");

        let topic_idx = handle.topic_idx;
        let off = topic_entry_off(topic_idx);

        let base_ptr = self.region.base;

        let write_seq_atom = unsafe { &*(base_ptr.add(off + TE_WRITE_SEQ) as *const AtomicU64) };

        let notify_atom = unsafe { &*(base_ptr.add(off + TE_NOTIFY) as *const AtomicU32) };

        let waiters_atom = unsafe { &*(base_ptr.add(off + TE_WAITERS) as *const AtomicU32) };

        TypedShmLoan {
            region: &self.region,
            block_idx,
            topic_idx,
            write_seq_atom,
            notify_atom,
            waiters_atom,
            _marker: core::marker::PhantomData,
        }
    }
}

impl Drop for ShmPublisher {
    fn drop(&mut self) {
        if self.is_owner && file_identity(&self.path) == self.created_ino {
            let _ = std::fs::remove_file(&self.path);
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
        let path = shm_path(name);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true) // needed for atomic CAS on refcounts
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

        let expected_size = region_size(&config);
        if mmap.len() < expected_size {
            return Err(IpcError::InvalidRegion(format!(
                "region size {} < expected {expected_size}",
                mmap.len()
            )));
        }

        // SAFETY: mmap provides a valid region of the computed size.
        let region = Arc::new(unsafe { Region::from_raw(mmap.as_mut_ptr(), mmap.len(), config) });

        region.check_heartbeat()?;

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

        let base_ptr = self.region.base;

        for i in 0..self.region.config.max_topics {
            let off = topic_entry_off(i);
            let active = unsafe { &*(base_ptr.add(off + TE_ACTIVE) as *const AtomicU32) };
            if active.load(Ordering::Acquire) != 1 {
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
