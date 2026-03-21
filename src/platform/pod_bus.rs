// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! Single-producer, multi-consumer (SPMC) broadcast ring for [`Pod`] types,
//! backed by shared memory.
//!
//! [`PodBus`] publishes values into a fixed-size ring buffer stored in a
//! memory-mapped file under `/dev/shm` (Linux), `/tmp` (macOS), or `%TEMP%`
//! (Windows). Any number of [`BusSubscriber`]s -- in the same or different
//! processes -- can independently read from the ring without blocking the
//! publisher or each other. Publish is O(1) regardless of subscriber count.
//!
//! ## Bounded backpressure (lossless mode)
//!
//! [`PodBus::create_bounded`] creates a ring where the publisher blocks (or
//! returns [`Error::Full`]) when the slowest subscriber is too far behind,
//! preventing data loss. Subscriber cursors are tracked via an atomic slot
//! array in the SHM header, supporting up to [`MAX_SUBSCRIBER_SLOTS`]
//! concurrent subscribers across processes.
//!
//! ## Soundness guarantees
//!
//! - **Single-producer enforcement**: `publish(&mut self)` requires exclusive
//!   access. `PodBus` is `Send` but NOT `Sync`, preventing shared references
//!   across threads (photon-ring pattern).
//!
//! - **Subscriber independence**: Each subscriber owns its own mmap of the
//!   backing file. The publisher can drop without affecting subscribers.
//!
//! - **Lock discipline**: `create()` acquires an exclusive flock, then
//!   downgrades to shared. `connect()` acquires a shared flock. A second
//!   `create()` on the same name will fail with `LockContention`.
//!
//! - **Cache-line aligned slots**: `Slot<T>` uses `#[repr(C, align(64))]`
//!   to prevent false sharing between adjacent slots (photon-ring pattern).
//!
//! - **Alignment enforcement**: Types with `align_of::<T>() > 8` are rejected
//!   at creation/connection time.
//!
//! - **Inode-checked cleanup**: `Drop` verifies the file's inode matches
//!   the one recorded at creation before deleting, preventing removal of
//!   a successor publisher's file.
//!
//! ## SHM region layout (version 2, bounded)
//!
//! ```text
//! +----------------------------------------------------------+
//! | Header (64 bytes)                                        |
//! |   [0..8)   magic: "XPOD_ZC\0"                           |
//! |   [8..12)  version: u32 (1 = unbounded, 2 = bounded)    |
//! |   [12..16) ring_size: u32                                |
//! |   [16..20) value_size: u32 (size_of::<T>())              |
//! |   [20..24) value_align: u32 (align_of::<T>())            |
//! |   [24..32) heartbeat_us: AtomicU64                       |
//! |   [32..40) write_seq: AtomicU64                          |
//! |   [40..48) publisher_pid: u64                            |
//! |   [48..52) watermark: u32 (0 for v1 unbounded)           |
//! |   [52..64) reserved                                      |
//! +----------------------------------------------------------+
//! | Subscriber slots (v2 only): 64 x SubscriberSlot          |
//! |   Each slot (16 bytes, naturally aligned):                |
//! |     [+0..+4)  active: AtomicU32  (0=free, 2=claiming,    |
//! |               1=active)                                  |
//! |     [+4..+8)  pid: AtomicU32     (subscriber PID)        |
//! |     [+8..+16) cursor: AtomicU64  (subscriber read pos)   |
//! +----------------------------------------------------------+
//! | Ring: ring_size x Slot<T>                                |
//! |   Each slot: stamp (AtomicU64, 8 bytes) + T data         |
//! |   Slot is #[repr(C, align(64))] for cache-line isolation |
//! +----------------------------------------------------------+
//! ```
//!
//! The ring uses photon-ring's proven stamp protocol:
//! - `0` -- slot never written
//! - `seq * 2 + 1` -- write in progress for sequence `seq`
//! - `seq * 2 + 2` -- write complete for sequence `seq`

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::path::PathBuf;

use crate::error::Error;
use crate::protocol::layout::is_valid_segment_name;
use crate::Pod;

use super::mmap::RawMmap;
use super::shm::{exclusive_lock, file_identity, shared_lock};

// ---- Constants ----

const POD_BUS_MAGIC: &[u8; 8] = b"XPOD_ZC\0";
/// Version 1: unbounded (lossy) ring, no subscriber tracking.
const POD_BUS_VERSION_V1: u32 = 1;
/// Version 2: bounded (lossless) ring with subscriber cursor slots.
const POD_BUS_VERSION_V2: u32 = 2;

/// Header size for both v1 and v2 (the base header).
const POD_BUS_HEADER_SIZE: usize = 64;

/// Maximum number of concurrent subscribers tracked for backpressure.
pub const MAX_SUBSCRIBER_SLOTS: usize = 64;

/// Size of each subscriber slot in the SHM header (active: u32, pid: u32, cursor: u64).
const SUBSCRIBER_SLOT_SIZE: usize = 16;

/// Total size of the subscriber slots section.
const SUBSCRIBER_SLOTS_SECTION_SIZE: usize = MAX_SUBSCRIBER_SLOTS * SUBSCRIBER_SLOT_SIZE;

// Header field offsets
const PH_MAGIC: usize = 0;
const PH_VERSION: usize = 8;
const PH_RING_SIZE: usize = 12;
const PH_VALUE_SIZE: usize = 16;
const PH_VALUE_ALIGN: usize = 20;
const PH_HEARTBEAT_US: usize = 24;
const PH_WRITE_SEQ: usize = 32;
const PH_PUBLISHER_PID: usize = 40;
const PH_WATERMARK: usize = 48;

/// Stale threshold: if heartbeat older than 5 seconds, publisher is considered dead.
const HEARTBEAT_STALE_US: u64 = 5_000_000;

/// Maximum supported alignment for Pod types in PodBus.
const MAX_POD_ALIGN: usize = 8;

// ---- File helpers ----

fn pod_bus_shm_path(name: &str) -> PathBuf {
    if cfg!(target_os = "linux") {
        PathBuf::from(alloc::format!("/dev/shm/crossbar-pod-{name}"))
    } else if cfg!(windows) {
        std::env::temp_dir().join(alloc::format!("crossbar-pod-{name}"))
    } else {
        PathBuf::from(alloc::format!("/tmp/crossbar-pod-shm-{name}"))
    }
}

fn pod_bus_lock_path(name: &str) -> PathBuf {
    let mut p = pod_bus_shm_path(name);
    p.set_extension("lock");
    p
}

fn validate_name(name: &str) -> Result<(), Error> {
    if !is_valid_segment_name(name) {
        return Err(Error::SegmentNameInvalid(name.into()));
    }
    Ok(())
}

/// Return microseconds since UNIX epoch, or `Err` if the clock is before epoch.
fn now_micros() -> Result<u64, Error> {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| Error::ClockError)?;
    #[allow(clippy::cast_possible_truncation)]
    Ok(d.as_micros() as u64)
}

/// Reject types with alignment > 8.
fn check_alignment<T>() -> Result<(), Error> {
    let align = core::mem::align_of::<T>();
    if align > MAX_POD_ALIGN {
        return Err(Error::AlignmentError {
            align,
            max: MAX_POD_ALIGN,
        });
    }
    Ok(())
}

// ---- Slot ----

/// A cache-line-aligned slot in the broadcast ring.
///
/// Uses `#[repr(C, align(64))]` to prevent false sharing between adjacent
/// slots (photon-ring pattern). The stamp and value co-locate in the same
/// cache line for T <= 56 bytes, eliminating an extra cache miss on reads.
///
/// Stamp encoding (photon-ring protocol):
/// - `stamp = 0` -- never written
/// - `stamp = seq * 2 + 1` -- write in progress for sequence `seq`
/// - `stamp = seq * 2 + 2` -- write complete for sequence `seq`
#[repr(C, align(64))]
pub struct Slot<T: Pod> {
    stamp: AtomicU64,
    data: UnsafeCell<T>,
}

// Compile-time check: Slot is cache-line aligned.
const _: () = assert!(core::mem::align_of::<Slot<u64>>() == 64);

/// Compute the size of a single slot, including alignment padding.
fn slot_size<T: Pod>() -> usize {
    core::mem::size_of::<Slot<T>>()
}

/// Compute total SHM region size for a given ring_size, type T, and version.
fn region_size_for_version<T: Pod>(ring_size: usize, version: u32) -> usize {
    let header_total = if version >= POD_BUS_VERSION_V2 {
        POD_BUS_HEADER_SIZE + SUBSCRIBER_SLOTS_SECTION_SIZE
    } else {
        POD_BUS_HEADER_SIZE
    };
    header_total + ring_size * slot_size::<T>()
}

// ---- Subscriber slot field offsets (relative to slot start) ----
const SS_ACTIVE: usize = 0;
const SS_PID: usize = 4;
const SS_CURSOR: usize = 8;

// ---- PodBus (publisher) ----

/// SPMC broadcast ring for [`Pod`] types, backed by shared memory.
///
/// The publisher creates the SHM region and writes into a power-of-two ring.
/// Each publish is O(1) regardless of how many subscribers exist. In the
/// default (unbounded / lossy) mode, subscribers that fall behind will skip to
/// the newest available data. In bounded mode (created with
/// [`create_bounded`](Self::create_bounded)), the publisher blocks or returns
/// [`Error::Full`] when the slowest subscriber is too far behind.
///
/// `PodBus` is `Send` but **not** `Sync`. The `publish(&mut self)` signature
/// enforces the single-producer guarantee at the type level -- no runtime
/// synchronization is needed.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::{Pod, PodBus};
///
/// let mut bus = PodBus::<u64>::create("example-prices", 256).unwrap();
/// bus.publish(42u64);
/// ```
pub struct PodBus<T: Pod> {
    _mmap: RawMmap,
    ring_ptr: *mut u8,
    mask: u64,
    ring_size: usize,
    heartbeat_ptr: *const AtomicU64,
    write_seq_ptr: *const AtomicU64,
    name: alloc::string::String,
    path: PathBuf,
    lock_path: PathBuf,
    created_ino: Option<u64>,
    _lock_file: std::fs::File,
    /// Pointer to the subscriber-slots section in SHM (v2 only, null for v1).
    sub_slots_base: *mut u8,
    /// Backpressure watermark. `None` for unbounded (v1) buses.
    watermark: Option<u64>,
    /// Cached minimum subscriber cursor to avoid scanning every publish.
    cached_slowest: u64,
    _marker: core::marker::PhantomData<T>,
}

// Safety: T: Pod implies T: Send + Copy, and all shared state uses atomics.
// The raw pointers point into an mmap region that outlives them (owned by _mmap).
// NOT Sync -- publish(&mut self) enforces single-producer at the type level.
unsafe impl<T: Pod> Send for PodBus<T> {}

/// A subscriber that reads from a [`PodBus`] ring via shared memory.
///
/// Each subscriber tracks its own read position independently and owns its
/// own mmap of the backing file. The subscriber is fully self-contained --
/// the publisher can drop without affecting subscribers.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::{Pod, BusSubscriber};
///
/// let mut sub = BusSubscriber::<u64>::connect("example-prices").unwrap();
/// if let Some(value) = sub.try_recv() {
///     println!("got: {value}");
/// }
/// ```
pub struct BusSubscriber<T: Pod> {
    _mmap: RawMmap,
    ring_ptr: *const u8,
    mask: u64,
    ring_size: usize,
    write_seq_ptr: *const AtomicU64,
    cursor: u64,
    total_lagged: u64,
    _lock_file: std::fs::File,
    /// Index into the subscriber-slots array (v2 only, `None` for v1).
    slot_index: Option<usize>,
    /// Pointer to the subscriber-slots section in SHM (v2 only, null for v1).
    sub_slots_base: *mut u8,
    _marker: core::marker::PhantomData<T>,
}

// Safety: same reasoning as PodBus. Subscriber is read-only (aside from its own
// cursor slot, which is exclusively owned by this subscriber instance).
unsafe impl<T: Pod> Send for BusSubscriber<T> {}
unsafe impl<T: Pod> Sync for BusSubscriber<T> {}

impl<T: Pod> PodBus<T> {
    /// Create a new unbounded (lossy) SPMC broadcast ring backed by shared memory.
    ///
    /// Creates a file at `/dev/shm/crossbar-pod-{name}` (Linux) containing the
    /// header and `ring_size` slots. Acquires an exclusive lock on the lock
    /// file, then downgrades to shared after initialization.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SegmentNameInvalid`] if the name contains path
    /// separators or null bytes.
    /// Returns [`Error::AlignmentError`] if `align_of::<T>() > 8`.
    /// Returns [`Error::LockContention`] if another publisher is active.
    /// Returns [`Error::Io`] if the backing file cannot be created.
    ///
    /// # Panics
    ///
    /// Panics if `ring_size` is zero or not a power of two.
    pub fn create(name: &str, ring_size: usize) -> Result<Self, Error> {
        Self::create_inner(name, ring_size, None)
    }

    /// Create a new bounded (lossless) SPMC broadcast ring backed by shared memory.
    ///
    /// When backpressure is enabled, [`publish`](Self::publish) spin-waits until
    /// the slowest subscriber has advanced enough, and [`try_publish`](Self::try_publish)
    /// returns [`Error::Full`] instead of blocking.
    ///
    /// The `watermark` parameter controls how many slots of headroom to leave
    /// between the publisher and the slowest subscriber. Backpressure triggers
    /// when `write_seq - slowest_cursor >= ring_size - watermark`.
    ///
    /// # Errors
    ///
    /// Same as [`create`](Self::create).
    ///
    /// # Panics
    ///
    /// Panics if `ring_size` is zero or not a power of two, or if
    /// `watermark >= ring_size`.
    pub fn create_bounded(name: &str, ring_size: usize, watermark: usize) -> Result<Self, Error> {
        assert!(
            watermark < ring_size,
            "watermark ({watermark}) must be less than ring_size ({ring_size})"
        );
        Self::create_inner(name, ring_size, Some(watermark as u64))
    }

    /// Internal constructor shared by `create` and `create_bounded`.
    fn create_inner(name: &str, ring_size: usize, watermark: Option<u64>) -> Result<Self, Error> {
        assert!(
            ring_size > 0 && ring_size.is_power_of_two(),
            "ring_size must be a power of two"
        );

        validate_name(name)?;
        check_alignment::<T>()?;

        let bounded = watermark.is_some();
        let version = if bounded {
            POD_BUS_VERSION_V2
        } else {
            POD_BUS_VERSION_V1
        };

        let path = pod_bus_shm_path(name);
        let lpath = pod_bus_lock_path(name);
        let size = region_size_for_version::<T>(ring_size, version);

        // Create/open lock file and acquire exclusive lock
        let lock_file = {
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            opts.open(&lpath).map_err(Error::Io)?
        };
        exclusive_lock(&lock_file, name)?;

        // Remove stale SHM file
        let _ = std::fs::remove_file(&path);

        let file = {
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true).write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            opts.open(&path).map_err(Error::Io)?
        };
        file.set_len(size as u64).map_err(Error::Io)?;

        let mmap = RawMmap::from_file_with_len(&file, size).map_err(Error::Io)?;

        // Write header
        let base = mmap.as_mut_ptr();
        unsafe {
            core::ptr::copy_nonoverlapping(POD_BUS_MAGIC.as_ptr(), base.add(PH_MAGIC), 8);
            (base.add(PH_VERSION) as *mut u32).write(version);
            (base.add(PH_RING_SIZE) as *mut u32).write(ring_size as u32);
            (base.add(PH_VALUE_SIZE) as *mut u32).write(core::mem::size_of::<T>() as u32);
            (base.add(PH_VALUE_ALIGN) as *mut u32).write(core::mem::align_of::<T>() as u32);
            // write_seq starts at 0 (already zero from truncate, but be explicit)
            (base.add(PH_WRITE_SEQ) as *mut u64).write(0);
            (base.add(PH_PUBLISHER_PID) as *mut u64).write(u64::from(std::process::id()));
            (base.add(PH_WATERMARK) as *mut u32).write(watermark.unwrap_or(0) as u32);
        }

        // Initialize subscriber slots (v2 only) -- already zeroed from truncate,
        // but be explicit about the active flags.
        let sub_slots_base = if bounded {
            let slots_base = unsafe { base.add(POD_BUS_HEADER_SIZE) };
            for i in 0..MAX_SUBSCRIBER_SLOTS {
                unsafe {
                    let slot_ptr = slots_base.add(i * SUBSCRIBER_SLOT_SIZE);
                    (slot_ptr.add(SS_ACTIVE) as *mut u32).write(0);
                    (slot_ptr.add(SS_PID) as *mut u32).write(0);
                    (slot_ptr.add(SS_CURSOR) as *mut u64).write(0);
                }
            }
            slots_base
        } else {
            core::ptr::null_mut()
        };

        // Compute ring base: after header + subscriber slots (v2) or just header (v1)
        let ring_offset = if bounded {
            POD_BUS_HEADER_SIZE + SUBSCRIBER_SLOTS_SECTION_SIZE
        } else {
            POD_BUS_HEADER_SIZE
        };
        let ring_base = unsafe { base.add(ring_offset) };

        // Initialize all slot stamps to 0
        let slot_sz = slot_size::<T>();
        for i in 0..ring_size {
            unsafe {
                let slot_ptr = ring_base.add(i * slot_sz);
                (slot_ptr as *mut u64).write(0); // stamp = 0
            }
        }

        // Write initial heartbeat
        let heartbeat_ptr = unsafe { &*(base.add(PH_HEARTBEAT_US) as *const AtomicU64) };
        let now = now_micros()?;
        heartbeat_ptr.store(now, Ordering::Release);

        let write_seq_ptr = unsafe { &*(base.add(PH_WRITE_SEQ) as *const AtomicU64) };

        // Record inode for Drop guard
        let created_ino = file_identity(&path);

        // Downgrade exclusive lock to shared
        shared_lock(&lock_file, name)?;

        Ok(Self {
            _mmap: mmap,
            ring_ptr: ring_base,
            mask: (ring_size as u64) - 1,
            ring_size,
            heartbeat_ptr,
            write_seq_ptr,
            name: name.into(),
            path,
            lock_path: lpath,
            created_ino,
            _lock_file: lock_file,
            sub_slots_base,
            watermark,
            cached_slowest: 0,
            _marker: core::marker::PhantomData,
        })
    }

    /// Publish a value into the ring. O(1) regardless of subscriber count.
    ///
    /// On an unbounded bus, this always succeeds immediately.
    ///
    /// On a bounded bus, this spin-waits until the slowest subscriber has
    /// advanced enough to make room, ensuring lossless delivery.
    ///
    /// Takes `&mut self` to enforce the single-producer guarantee at the type
    /// level. `PodBus` is `Send` but not `Sync`, so this cannot be called from
    /// multiple threads simultaneously.
    pub fn publish(&mut self, value: T) {
        if self.watermark.is_some() {
            // Bounded: spin-wait until there is room.
            let mut spins = 0u32;
            loop {
                match self.try_publish(value) {
                    Ok(()) => return,
                    Err(_) => {
                        spins += 1;
                        if spins & 0x3FF == 0 {
                            // Refresh heartbeat every 1024 spins to prevent
                            // subscribers from declaring us dead during backpressure.
                            if let Ok(now) = now_micros() {
                                self.heartbeat_atomic().store(now, Ordering::Release);
                            }
                        }
                        core::hint::spin_loop();
                    }
                }
            }
        }
        self.publish_unchecked(value);
    }

    /// Try to publish a value into the ring with backpressure awareness.
    ///
    /// On an unbounded bus, this always succeeds and returns `Ok(())`.
    ///
    /// On a bounded bus, this checks whether the slowest subscriber has fallen
    /// too far behind. If `write_seq - slowest_cursor >= ring_size - watermark`,
    /// returns `Err(Error::Full)` without writing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Full`] if the ring is full (bounded mode only).
    pub fn try_publish(&mut self, value: T) -> Result<(), Error> {
        if let Some(watermark) = self.watermark {
            let write_seq = self.write_seq().load(Ordering::Relaxed);
            let effective = self.ring_size as u64 - watermark;

            // Fast path: use cached slowest cursor.
            if write_seq >= self.cached_slowest + effective {
                // Slow path: rescan all subscriber slots.
                match self.slowest_subscriber_cursor() {
                    Some(slowest) => {
                        self.cached_slowest = slowest;
                        if write_seq >= slowest + effective {
                            return Err(Error::Full);
                        }
                    }
                    None => {
                        // No active subscribers -- ring is unbounded.
                    }
                }
            }
        }
        self.publish_unchecked(value);
        Ok(())
    }

    /// Write a value into the ring without any backpressure check.
    fn publish_unchecked(&mut self, value: T) {
        let write_seq = self.write_seq().load(Ordering::Relaxed);

        // Update heartbeat every 1024 publishes
        if write_seq & 0x3FF == 0 {
            if let Ok(now) = now_micros() {
                self.heartbeat_atomic().store(now, Ordering::Release);
            }
        }

        let idx = (write_seq & self.mask) as usize;
        let slot = self.slot_mut(idx);

        // Photon-ring stamp protocol: mark write in progress.
        // stamp = seq * 2 + 1
        slot.stamp.store(write_seq * 2 + 1, Ordering::Release);

        // Write data.
        // Safety: we are the sole writer and the odd stamp signals readers to retry.
        unsafe {
            core::ptr::write_volatile(slot.data.get(), value);
        }

        // Mark slot as committed.
        // stamp = seq * 2 + 2
        slot.stamp.store(write_seq * 2 + 2, Ordering::Release);

        // Advance sequence.
        self.write_seq().store(write_seq + 1, Ordering::Release);
    }

    /// Scan all subscriber slots in SHM and return the minimum (slowest) cursor.
    ///
    /// Returns `None` if no active subscribers exist. Prunes stale subscribers
    /// whose PID no longer exists.
    fn slowest_subscriber_cursor(&self) -> Option<u64> {
        if self.sub_slots_base.is_null() {
            return None;
        }

        let mut min_cursor = u64::MAX;
        let mut has_active = false;

        for i in 0..MAX_SUBSCRIBER_SLOTS {
            let slot_base = unsafe { self.sub_slots_base.add(i * SUBSCRIBER_SLOT_SIZE) };

            let active_atomic = unsafe { &*(slot_base.add(SS_ACTIVE) as *const AtomicU32) };
            let state = active_atomic.load(Ordering::Acquire);

            // Only consider fully-initialized (ACTIVE) slots for backpressure.
            // CLAIMING slots are still being initialized and are not yet visible.
            if state == SS_STATE_CLAIMING {
                // A CLAIMING slot with a dead PID means the subscriber crashed
                // during initialization -- prune it back to FREE.
                let pid_atomic = unsafe { &*(slot_base.add(SS_PID) as *const AtomicU32) };
                let pid = pid_atomic.load(Ordering::Relaxed);
                if pid != 0 && !is_pid_alive(pid) {
                    active_atomic.store(SS_STATE_FREE, Ordering::Release);
                }
                continue;
            }
            if state != SS_STATE_ACTIVE {
                continue;
            }

            // Check if subscriber PID is still alive.
            let pid_atomic = unsafe { &*(slot_base.add(SS_PID) as *const AtomicU32) };
            let pid = pid_atomic.load(Ordering::Relaxed);
            if pid != 0 && !is_pid_alive(pid) {
                // Subscriber crashed -- prune the slot.
                active_atomic.store(SS_STATE_FREE, Ordering::Release);
                continue;
            }

            let cursor_atomic = unsafe { &*(slot_base.add(SS_CURSOR) as *const AtomicU64) };
            let cursor = cursor_atomic.load(Ordering::Acquire);

            has_active = true;
            if cursor < min_cursor {
                min_cursor = cursor;
            }
        }

        if has_active {
            Some(min_cursor)
        } else {
            None
        }
    }

    /// Explicitly update the heartbeat timestamp.
    ///
    /// Call this during idle periods (when no values are being published) to
    /// prevent subscribers from considering the publisher dead. The publisher
    /// automatically updates the heartbeat every 1024 publishes, but if the
    /// publishing rate is low, manual heartbeats prevent false `PublisherDead`
    /// errors.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ClockError`] if the system clock is before the UNIX
    /// epoch.
    pub fn heartbeat(&mut self) -> Result<(), Error> {
        let now = now_micros()?;
        self.heartbeat_atomic().store(now, Ordering::Release);
        Ok(())
    }

    /// Create a new subscriber for this bus (same process).
    ///
    /// The subscriber starts from the current write position so it only
    /// sees values published after this call. The subscriber opens its own
    /// mmap of the backing file and acquires a shared lock, making it fully
    /// independent of the publisher's lifetime.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the SHM file cannot be re-opened.
    /// Returns [`Error::LockContention`] if the lock file cannot be opened.
    pub fn subscriber(&self) -> Result<BusSubscriber<T>, Error> {
        BusSubscriber::connect(&self.name)
    }

    /// Return the total number of values published so far.
    pub fn published_count(&self) -> u64 {
        self.write_seq().load(Ordering::Acquire)
    }

    /// Return the ring capacity.
    pub fn ring_size(&self) -> usize {
        self.ring_size
    }

    /// Returns `true` if this bus was created with bounded backpressure.
    pub fn is_bounded(&self) -> bool {
        self.watermark.is_some()
    }

    /// Get a reference to the heartbeat atomic in SHM (for internal auto-update).
    #[inline]
    fn heartbeat_atomic(&self) -> &AtomicU64 {
        unsafe { &*self.heartbeat_ptr }
    }

    /// Get a reference to the write_seq atomic.
    #[inline]
    fn write_seq(&self) -> &AtomicU64 {
        unsafe { &*self.write_seq_ptr }
    }

    /// Get a reference to a slot by index.
    #[inline]
    fn slot_mut(&self, idx: usize) -> &Slot<T> {
        debug_assert!(idx < self.ring_size);
        let slot_sz = slot_size::<T>();
        unsafe { &*(self.ring_ptr.add(idx * slot_sz) as *const Slot<T>) }
    }
}

impl<T: Pod> Drop for PodBus<T> {
    fn drop(&mut self) {
        // Only delete if inode matches (prevents removing a successor's file).
        if let (Some(created), Some(current)) = (self.created_ino, file_identity(&self.path)) {
            if created == current {
                let _ = std::fs::remove_file(&self.path);
            }
        }
        // Clean up the lock file.
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

impl<T: Pod> BusSubscriber<T> {
    /// Connect to an existing [`PodBus`] by name.
    ///
    /// Opens its OWN mmap of the SHM file and validates the header (magic,
    /// version, type size and alignment). The subscriber starts reading from
    /// the current write position. Acquires a shared lock on the lock file
    /// to verify the publisher is still alive.
    ///
    /// On a bounded (v2) bus, the subscriber claims a slot in the SHM header
    /// for cursor tracking. The slot is released on [`Drop`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file doesn't exist.
    /// Returns [`Error::AlignmentError`] if `align_of::<T>() > 8`.
    /// Returns [`Error::InvalidRegion`] if the header is invalid or the
    /// type parameters don't match.
    /// Returns [`Error::PublisherDead`] if the heartbeat is stale.
    /// Returns [`Error::LockContention`] if the lock cannot be acquired.
    /// Returns [`Error::PoolExhausted`] if all subscriber slots are taken (v2 only).
    pub fn connect(name: &str) -> Result<Self, Error> {
        validate_name(name)?;
        check_alignment::<T>()?;

        let path = pod_bus_shm_path(name);
        let lpath = pod_bus_lock_path(name);

        // Acquire shared lock (verifies publisher is still alive via flock).
        let lock_file = {
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true).write(true).create(false).truncate(false);
            opts.open(&lpath).map_err(Error::Io)?
        };
        shared_lock(&lock_file, name)?;

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true) // needed for atomic operations on the mmap
            .open(&path)
            .map_err(Error::Io)?;

        let mmap = RawMmap::from_file(&file).map_err(Error::Io)?;

        if mmap.len() < POD_BUS_HEADER_SIZE {
            return Err(Error::InvalidRegion(
                "PodBus region too small for header".into(),
            ));
        }

        let base = mmap.as_ptr();

        // Validate magic
        unsafe {
            let mut magic = [0u8; 8];
            core::ptr::copy_nonoverlapping(base.add(PH_MAGIC), magic.as_mut_ptr(), 8);
            if &magic != POD_BUS_MAGIC {
                return Err(Error::InvalidRegion(
                    "invalid PodBus magic (expected XPOD_ZC)".into(),
                ));
            }
        }

        // Validate version (accept both v1 and v2)
        let version = unsafe { (base.add(PH_VERSION) as *const u32).read() };
        if version != POD_BUS_VERSION_V1 && version != POD_BUS_VERSION_V2 {
            return Err(Error::InvalidRegion(alloc::format!(
                "unsupported PodBus version {version}, expected {POD_BUS_VERSION_V1} or {POD_BUS_VERSION_V2}"
            )));
        }

        let bounded = version == POD_BUS_VERSION_V2;

        // Read ring parameters
        let ring_size = unsafe { (base.add(PH_RING_SIZE) as *const u32).read() } as usize;
        let value_size = unsafe { (base.add(PH_VALUE_SIZE) as *const u32).read() } as usize;
        let value_align = unsafe { (base.add(PH_VALUE_ALIGN) as *const u32).read() } as usize;

        // Validate ring_size
        if ring_size == 0 || !ring_size.is_power_of_two() {
            return Err(Error::InvalidRegion(alloc::format!(
                "PodBus ring_size {ring_size} is not a power of two"
            )));
        }

        // Validate type compatibility
        if value_size != core::mem::size_of::<T>() {
            return Err(Error::InvalidRegion(alloc::format!(
                "PodBus value_size mismatch: file has {value_size}, type needs {}",
                core::mem::size_of::<T>()
            )));
        }
        if value_align != core::mem::align_of::<T>() {
            return Err(Error::InvalidRegion(alloc::format!(
                "PodBus value_align mismatch: file has {value_align}, type needs {}",
                core::mem::align_of::<T>()
            )));
        }

        // Validate total region size
        let expected_size = region_size_for_version::<T>(ring_size, version);
        if mmap.len() < expected_size {
            return Err(Error::InvalidRegion(alloc::format!(
                "PodBus region size {} < expected {expected_size}",
                mmap.len()
            )));
        }

        // Check heartbeat liveness
        let hb =
            unsafe { &*(base.add(PH_HEARTBEAT_US) as *const AtomicU64) }.load(Ordering::Acquire);
        let now = now_micros()?;
        if now.saturating_sub(hb) > HEARTBEAT_STALE_US {
            return Err(Error::PublisherDead);
        }

        // Derive all pointers from THIS subscriber's own mmap.
        let write_seq_ptr: *const AtomicU64 =
            unsafe { &*(base.add(PH_WRITE_SEQ) as *const AtomicU64) };
        let cursor = unsafe { &*write_seq_ptr }.load(Ordering::Acquire);

        let ring_offset = if bounded {
            POD_BUS_HEADER_SIZE + SUBSCRIBER_SLOTS_SECTION_SIZE
        } else {
            POD_BUS_HEADER_SIZE
        };
        let ring_ptr = unsafe { base.add(ring_offset) };

        // For v2 bounded buses, claim a subscriber slot via CAS.
        let (slot_index, sub_slots_base) = if bounded {
            let slots_base = unsafe { mmap.as_mut_ptr().add(POD_BUS_HEADER_SIZE) };
            let my_pid = std::process::id();
            let idx = claim_subscriber_slot(slots_base, cursor, my_pid)?;
            (Some(idx), slots_base)
        } else {
            (None, core::ptr::null_mut())
        };

        Ok(Self {
            _mmap: mmap,
            ring_ptr,
            mask: (ring_size as u64) - 1,
            ring_size,
            write_seq_ptr,
            cursor,
            total_lagged: 0,
            _lock_file: lock_file,
            slot_index,
            sub_slots_base,
            _marker: core::marker::PhantomData,
        })
    }

    /// Try to receive the next value from the ring.
    ///
    /// Returns `None` if no new data is available. If the subscriber has
    /// fallen behind (the slot it wants has been overwritten), it advances
    /// its cursor to the oldest still-available data and increments the
    /// internal lag counter (queryable via [`total_lagged()`](Self::total_lagged)).
    pub fn try_recv(&mut self) -> Option<T> {
        let head = self.write_seq().load(Ordering::Acquire);

        if self.cursor >= head {
            return None; // caught up
        }

        // If subscriber is more than ring_size behind, skip ahead.
        let ring_size = self.ring_size as u64;
        if head - self.cursor > ring_size {
            let skipped = head - ring_size - self.cursor;
            self.total_lagged += skipped;
            self.cursor = head - ring_size;
        }

        let idx = (self.cursor & self.mask) as usize;
        let slot = self.slot(idx);

        // Seqlock read: snapshot stamp, read data, verify stamp unchanged.
        let stamp_before = slot.stamp.load(Ordering::Acquire);

        // Odd stamp means write in progress -- skip this attempt.
        if stamp_before & 1 != 0 {
            return None;
        }

        // Stamp 0 means never written.
        if stamp_before == 0 {
            return None;
        }

        // Read the data.
        // Safety: T: Pod (Copy), volatile read through UnsafeCell.
        let value = unsafe { core::ptr::read_volatile(slot.data.get()) };

        let stamp_after = slot.stamp.load(Ordering::Acquire);

        if stamp_after != stamp_before {
            // Stamp changed during read -- data was torn, retry next call.
            return None;
        }

        // Verify this stamp corresponds to the sequence we expected.
        // Expected committed stamp: seq * 2 + 2 (photon-ring protocol).
        let expected_stamp = self.cursor * 2 + 2;
        if stamp_before != expected_stamp {
            // Slot has been overwritten with a newer sequence -- advance cursor.
            // Derive the sequence from the stamp: stamp = seq * 2 + 2 => seq = stamp / 2 - 1
            let written_seq = stamp_before / 2 - 1;
            let skipped = written_seq + 1 - self.cursor;
            self.total_lagged += skipped;
            self.cursor = written_seq + 1;
            return None;
        }

        self.cursor += 1;

        // Update cursor in SHM for backpressure tracking (v2 only).
        self.update_shm_cursor(self.cursor);

        Some(value)
    }

    /// Total messages skipped due to lag (subscriber fell behind the ring).
    ///
    /// This counter accumulates across the lifetime of the subscriber.
    #[inline]
    pub fn total_lagged(&self) -> u64 {
        self.total_lagged
    }

    /// Return the total number of values published so far (from the publisher's
    /// write sequence counter in SHM).
    pub fn published_count(&self) -> u64 {
        self.write_seq().load(Ordering::Acquire)
    }

    /// Return the ring capacity.
    pub fn ring_size(&self) -> usize {
        self.ring_size
    }

    /// Update this subscriber's cursor in SHM (v2 only).
    #[inline]
    fn update_shm_cursor(&self, new_cursor: u64) {
        if let Some(idx) = self.slot_index {
            let slot_base = unsafe { self.sub_slots_base.add(idx * SUBSCRIBER_SLOT_SIZE) };
            let cursor_atomic = unsafe { &*(slot_base.add(SS_CURSOR) as *const AtomicU64) };
            cursor_atomic.store(new_cursor, Ordering::Release);
        }
    }

    /// Get a reference to the write_seq atomic in SHM.
    #[inline]
    fn write_seq(&self) -> &AtomicU64 {
        unsafe { &*self.write_seq_ptr }
    }

    /// Get a reference to a slot by index.
    #[inline]
    fn slot(&self, idx: usize) -> &Slot<T> {
        debug_assert!(idx < self.ring_size);
        let slot_sz = slot_size::<T>();
        unsafe { &*(self.ring_ptr.add(idx * slot_sz) as *const Slot<T>) }
    }
}

impl<T: Pod> Drop for BusSubscriber<T> {
    fn drop(&mut self) {
        // Release subscriber slot in SHM (v2 only).
        if let Some(idx) = self.slot_index {
            if !self.sub_slots_base.is_null() {
                let slot_base = unsafe { self.sub_slots_base.add(idx * SUBSCRIBER_SLOT_SIZE) };
                let active_atomic = unsafe { &*(slot_base.add(SS_ACTIVE) as *const AtomicU32) };
                active_atomic.store(SS_STATE_FREE, Ordering::Release);
            }
        }
    }
}

// ---- Subscriber slot helpers ----

/// Subscriber slot states (3-state machine):
/// - `SS_STATE_FREE (0)`: slot is available for claiming
/// - `SS_STATE_CLAIMING (2)`: slot is being initialized (pid/cursor being written)
/// - `SS_STATE_ACTIVE (1)`: slot is fully initialized and in use
const SS_STATE_FREE: u32 = 0;
const SS_STATE_ACTIVE: u32 = 1;
const SS_STATE_CLAIMING: u32 = 2;

/// Claim a free subscriber slot via CAS. Returns the slot index on success.
///
/// Uses a 3-state machine to prevent partial initialization from being visible
/// to the publisher's subscriber scan:
/// 1. CAS `FREE(0) -> CLAIMING(2)` to reserve the slot
/// 2. Write pid and cursor
/// 3. Store `ACTIVE(1)` with Release to make the slot visible
///
/// The publisher only considers `active == ACTIVE(1)`, never `CLAIMING(2)`.
/// If a subscriber dies during CLAIMING, the publisher can prune it (any
/// CLAIMING slot older than a few seconds is dead).
fn claim_subscriber_slot(
    slots_base: *mut u8,
    initial_cursor: u64,
    pid: u32,
) -> Result<usize, Error> {
    for i in 0..MAX_SUBSCRIBER_SLOTS {
        let slot_base = unsafe { slots_base.add(i * SUBSCRIBER_SLOT_SIZE) };
        let active_atomic = unsafe { &*(slot_base.add(SS_ACTIVE) as *const AtomicU32) };

        // Try to claim this slot: CAS FREE(0) -> CLAIMING(2)
        if active_atomic
            .compare_exchange(
                SS_STATE_FREE,
                SS_STATE_CLAIMING,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            // Write PID and initial cursor while in CLAIMING state.
            let pid_atomic = unsafe { &*(slot_base.add(SS_PID) as *const AtomicU32) };
            pid_atomic.store(pid, Ordering::Release);

            let cursor_atomic = unsafe { &*(slot_base.add(SS_CURSOR) as *const AtomicU64) };
            cursor_atomic.store(initial_cursor, Ordering::Release);

            // Transition CLAIMING(2) -> ACTIVE(1) to make slot visible to publisher.
            active_atomic.store(SS_STATE_ACTIVE, Ordering::Release);

            return Ok(i);
        }
    }
    Err(Error::PoolExhausted)
}

/// Check if a process with the given PID is still alive.
///
/// `kill(pid, 0)` returns 0 if the process exists and we can signal it.
/// On error, `EPERM` means the process exists but we lack permission to
/// signal it (still alive). `ESRCH` means the process does not exist.
#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    let ret = unsafe { libc::kill(pid as i32, 0) };
    if ret == 0 {
        return true; // Process exists and we can signal it
    }
    // errno == EPERM means process exists but we lack permission
    // errno == ESRCH means process does not exist
    let err = std::io::Error::last_os_error();
    err.raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn is_pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    unsafe {
        CloseHandle(handle);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a unique test name to avoid SHM file collisions between tests.
    fn test_name(base: &str) -> alloc::string::String {
        use core::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        alloc::format!("test-{base}-{}-{id}", std::process::id())
    }

    #[test]
    fn basic_publish_subscribe() {
        let name = test_name("basic");
        let mut bus = PodBus::<u64>::create(&name, 4).unwrap();
        let mut sub = bus.subscriber().unwrap();

        assert!(sub.try_recv().is_none());

        bus.publish(42u64);
        assert_eq!(sub.try_recv(), Some(42));
        assert!(sub.try_recv().is_none());
    }

    #[test]
    fn multiple_subscribers() {
        let name = test_name("multi");
        let mut bus = PodBus::<u32>::create(&name, 8).unwrap();
        let mut s1 = bus.subscriber().unwrap();
        let mut s2 = bus.subscriber().unwrap();
        let mut s3 = bus.subscriber().unwrap();

        bus.publish(10);
        bus.publish(20);

        assert_eq!(s1.try_recv(), Some(10));
        assert_eq!(s1.try_recv(), Some(20));

        assert_eq!(s2.try_recv(), Some(10));
        assert_eq!(s2.try_recv(), Some(20));

        assert_eq!(s3.try_recv(), Some(10));
        assert_eq!(s3.try_recv(), Some(20));
    }

    #[test]
    fn ring_overwrite() {
        let name = test_name("overwrite");
        let mut bus = PodBus::<u64>::create(&name, 4).unwrap();
        let mut sub = bus.subscriber().unwrap();

        // Publish more than ring_size to force overwrite.
        for i in 0..10u64 {
            bus.publish(i);
        }

        // Subscriber should skip ahead and still be able to read.
        let mut received = alloc::vec::Vec::new();
        while let Some(v) = sub.try_recv() {
            received.push(v);
        }
        // Should get the last ring_size values (6, 7, 8, 9).
        assert!(!received.is_empty());
        assert!(received.len() <= 4);
        assert_eq!(*received.last().unwrap(), 9);
    }

    #[test]
    fn connect_by_name() {
        let name = test_name("connect");
        let mut bus = PodBus::<u64>::create(&name, 8).unwrap();
        bus.publish(100);
        bus.publish(200);

        // Connect as a separate subscriber (simulates cross-process).
        let mut sub = BusSubscriber::<u64>::connect(&name).unwrap();
        // Subscriber connects at current write position, so previous values
        // are not visible unless we rewind. Publish new values.
        bus.publish(300);
        assert_eq!(sub.try_recv(), Some(300));
    }

    #[test]
    #[should_panic]
    fn rejects_non_power_of_two() {
        let name = test_name("pot");
        let _ = PodBus::<u64>::create(&name, 3);
    }

    #[test]
    #[should_panic]
    fn rejects_zero() {
        let name = test_name("zero");
        let _ = PodBus::<u64>::create(&name, 0);
    }

    #[test]
    fn type_mismatch_rejected() {
        let name = test_name("mismatch");
        let _bus = PodBus::<u64>::create(&name, 8).unwrap();

        // Try connecting with wrong type size
        let result = BusSubscriber::<u32>::connect(&name);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_name_rejected() {
        let result = PodBus::<u64>::create("../evil", 8);
        assert!(result.is_err());

        let result = PodBus::<u64>::create("foo/bar", 8);
        assert!(result.is_err());
    }

    #[test]
    fn subscriber_survives_publisher_drop() {
        let name = test_name("surv");
        let mut sub;
        {
            let mut bus = PodBus::<u64>::create(&name, 8).unwrap();
            bus.publish(42);
            bus.publish(43);
            sub = bus.subscriber().unwrap();
            bus.publish(44);
            // bus drops here -- subscriber has its own mmap
        }
        // subscriber can still read what was published before drop
        let mut received = alloc::vec::Vec::new();
        while let Some(v) = sub.try_recv() {
            received.push(v);
        }
        assert_eq!(received, alloc::vec![44]);
    }

    #[test]
    fn second_create_fails_while_first_alive() {
        let name = test_name("lock");
        let _bus = PodBus::<u64>::create(&name, 4).unwrap();

        // A second create on the same name should fail (exclusive lock held as shared,
        // but create needs exclusive).
        let result = PodBus::<u64>::create(&name, 4);
        assert!(result.is_err());
    }

    #[test]
    fn lag_detection() {
        let name = test_name("lag");
        let mut bus = PodBus::<u64>::create(&name, 4).unwrap();
        let mut sub = bus.subscriber().unwrap();

        // Publish way more than ring_size
        for i in 0..100u64 {
            bus.publish(i);
        }

        // Read what we can
        while sub.try_recv().is_some() {}

        // total_lagged should be > 0
        assert!(sub.total_lagged() > 0);
    }

    #[test]
    fn heartbeat_method() {
        let name = test_name("hb");
        let mut bus = PodBus::<u64>::create(&name, 4).unwrap();
        // Should succeed without publishing anything.
        bus.heartbeat().unwrap();
    }

    // ---- Bounded backpressure tests ----

    #[test]
    fn bounded_backpressure_blocks() {
        let name = test_name("bp-blocks");
        // Ring of 4, watermark 1 => effective capacity = 3
        let mut bus = PodBus::<u64>::create_bounded(&name, 4, 1).unwrap();
        assert!(bus.is_bounded());

        let mut sub = bus.subscriber().unwrap();

        // Fill up to effective capacity (3 values).
        assert!(bus.try_publish(1).is_ok());
        assert!(bus.try_publish(2).is_ok());
        assert!(bus.try_publish(3).is_ok());

        // 4th should fail -- ring is full.
        let result = bus.try_publish(4);
        assert!(
            matches!(result, Err(Error::Full)),
            "expected Error::Full, got {result:?}"
        );

        // Subscriber reads one value, freeing a slot.
        assert_eq!(sub.try_recv(), Some(1));

        // Now we can publish again.
        assert!(bus.try_publish(4).is_ok());
    }

    #[test]
    fn bounded_subscriber_advances() {
        let name = test_name("bp-adv");
        // Ring of 8, watermark 0 => effective capacity = 8
        let mut bus = PodBus::<u64>::create_bounded(&name, 8, 0).unwrap();
        let mut sub = bus.subscriber().unwrap();

        // Fill ring completely.
        for i in 0..8u64 {
            assert!(bus.try_publish(i).is_ok(), "failed to publish {i}");
        }

        // Ring is full.
        assert!(matches!(bus.try_publish(99), Err(Error::Full)));

        // Subscriber reads all values.
        let mut received = alloc::vec::Vec::new();
        while let Some(v) = sub.try_recv() {
            received.push(v);
        }
        assert_eq!(received, alloc::vec![0, 1, 2, 3, 4, 5, 6, 7]);

        // Now we can publish again (subscriber advanced cursor).
        for i in 100..108u64 {
            assert!(bus.try_publish(i).is_ok(), "failed to publish {i}");
        }
        assert!(matches!(bus.try_publish(999), Err(Error::Full)));
    }

    #[test]
    fn bounded_no_subscribers_unbounded() {
        let name = test_name("bp-nosub");
        // With no subscribers, a bounded bus should publish freely.
        let mut bus = PodBus::<u64>::create_bounded(&name, 4, 1).unwrap();

        for i in 0..100u64 {
            assert!(bus.try_publish(i).is_ok());
        }
    }

    #[test]
    fn bounded_crashed_subscriber_pruned() {
        let name = test_name("bp-crash");
        // Ring of 4, watermark 0 => effective capacity = 4
        let mut bus = PodBus::<u64>::create_bounded(&name, 4, 0).unwrap();

        // Create subscriber and fill the ring.
        let sub = bus.subscriber().unwrap();
        for i in 0..4u64 {
            assert!(bus.try_publish(i).is_ok());
        }

        // Ring is full.
        assert!(matches!(bus.try_publish(99), Err(Error::Full)));

        // Drop the subscriber (simulates crash -- slot released on drop).
        drop(sub);

        // With no active subscribers, publisher can proceed.
        assert!(bus.try_publish(99).is_ok());
    }

    #[test]
    fn bounded_publish_spin_waits() {
        let name = test_name("bp-spin");
        let mut bus = PodBus::<u64>::create_bounded(&name, 4, 1).unwrap();
        let mut sub = bus.subscriber().unwrap();

        // Spawn a thread that reads after a short delay.
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            let mut count = 0;
            while count < 5 {
                if sub.try_recv().is_some() {
                    count += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
            count
        });

        // Publish 5 values using blocking publish (ring only holds 3).
        for i in 0..5u64 {
            bus.publish(i);
        }

        let count = handle.join().unwrap();
        assert_eq!(count, 5);
    }

    #[test]
    #[should_panic(expected = "watermark")]
    fn bounded_rejects_watermark_ge_ring_size() {
        let name = test_name("bp-wm");
        let _ = PodBus::<u64>::create_bounded(&name, 4, 4);
    }

    #[test]
    fn unbounded_try_publish_always_succeeds() {
        let name = test_name("unbounded-try");
        let mut bus = PodBus::<u64>::create(&name, 4).unwrap();
        let _sub = bus.subscriber().unwrap();

        // Even without reading, try_publish should always succeed on an unbounded bus.
        for i in 0..100u64 {
            assert!(bus.try_publish(i).is_ok());
        }
    }
}
