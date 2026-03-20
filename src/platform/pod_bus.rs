// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! Single-producer, multi-consumer (SPMC) broadcast ring for [`Pod`] types,
//! backed by shared memory.
//!
//! [`PodBus`] publishes values into a fixed-size ring buffer stored in a
//! memory-mapped file under `/dev/shm` (Linux), `/tmp` (macOS), or `%TEMP%`
//! (Windows). Any number of [`PodBusSubscriber`]s — in the same or different
//! processes — can independently read from the ring without blocking the
//! publisher or each other. Publish is O(1) regardless of subscriber count.
//!
//! ## SHM region layout
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────┐
//! │ Header (64 bytes)                                        │
//! │   [0..8)   magic: "XPOD_ZC\0"                           │
//! │   [8..12)  version: u32                                  │
//! │   [12..16) ring_size: u32                                │
//! │   [16..20) value_size: u32 (size_of::<T>())              │
//! │   [20..24) value_align: u32 (align_of::<T>())            │
//! │   [24..32) heartbeat_us: AtomicU64                       │
//! │   [32..40) write_seq: AtomicU64                          │
//! │   [40..48) publisher_pid: u64                            │
//! │   [48..64) reserved                                      │
//! ├──────────────────────────────────────────────────────────┤
//! │ Ring: ring_size × Slot<T>                                │
//! │   Each slot: stamp (AtomicU64, 8 bytes) + T data         │
//! └──────────────────────────────────────────────────────────┘
//! ```
//!
//! The ring uses a seqlock-style stamp protocol:
//! - `0` — slot never written
//! - odd  (`seq * 2 + 1`) — write in progress
//! - even (`(seq + 1) * 2`) — committed and readable

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};
use std::path::PathBuf;

use crate::error::IpcError;
use crate::protocol::layout::is_valid_segment_name;
use crate::Pod;

use super::mmap::RawMmap;

// ---- Constants ----

const POD_BUS_MAGIC: &[u8; 8] = b"XPOD_ZC\0";
const POD_BUS_VERSION: u32 = 1;
const POD_BUS_HEADER_SIZE: usize = 64;

// Header field offsets
const PH_MAGIC: usize = 0;
const PH_VERSION: usize = 8;
const PH_RING_SIZE: usize = 12;
const PH_VALUE_SIZE: usize = 16;
const PH_VALUE_ALIGN: usize = 20;
const PH_HEARTBEAT_US: usize = 24;
const PH_WRITE_SEQ: usize = 32;
const PH_PUBLISHER_PID: usize = 40;

/// Stale threshold: if heartbeat older than 5 seconds, publisher is considered dead.
const HEARTBEAT_STALE_US: u64 = 5_000_000;

// ---- File helpers (reuse patterns from shm.rs) ----

fn pod_bus_shm_path(name: &str) -> PathBuf {
    if cfg!(target_os = "linux") {
        PathBuf::from(alloc::format!("/dev/shm/crossbar-pod-{name}"))
    } else if cfg!(windows) {
        std::env::temp_dir().join(alloc::format!("crossbar-pod-{name}"))
    } else {
        PathBuf::from(alloc::format!("/tmp/crossbar-pod-shm-{name}"))
    }
}

fn validate_name(name: &str) -> Result<(), IpcError> {
    if !is_valid_segment_name(name) {
        return Err(IpcError::SegmentNameInvalid(name.into()));
    }
    Ok(())
}

/// Return microseconds since UNIX epoch, or `Err` if the clock is before epoch.
fn now_micros() -> Result<u64, IpcError> {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| IpcError::ClockError)?;
    #[allow(clippy::cast_possible_truncation)]
    Ok(d.as_micros() as u64)
}

// ---- Slot ----

/// A slot in the broadcast ring (same layout in-process and cross-process).
#[repr(C)]
pub struct Slot<T: Pod> {
    stamp: AtomicU64,
    data: UnsafeCell<T>,
}

/// Compute the size of a single slot, including alignment padding.
/// Each slot must be aligned to `max(8, align_of::<T>())` for the AtomicU64.
fn slot_size<T: Pod>() -> usize {
    core::mem::size_of::<Slot<T>>()
}

/// Compute total SHM region size for a given ring_size and type T.
fn region_size<T: Pod>(ring_size: usize) -> usize {
    POD_BUS_HEADER_SIZE + ring_size * slot_size::<T>()
}

// ---- PodBus (publisher) ----

/// SPMC broadcast ring for [`Pod`] types, backed by shared memory.
///
/// The publisher creates the SHM region and writes into a power-of-two ring.
/// Each publish is O(1) regardless of how many subscribers exist. Subscribers
/// that fall behind will skip to the newest available data (lossy).
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::{Pod, PodBus};
///
/// let bus = PodBus::<u64>::create("example-prices", 256).unwrap();
/// bus.publish(42u64);
/// ```
pub struct PodBus<T: Pod> {
    _mmap: RawMmap,
    ring_ptr: *mut u8,
    mask: u64,
    ring_size: usize,
    heartbeat_ptr: *const AtomicU64,
    write_seq_ptr: *const AtomicU64,
    path: PathBuf,
    _marker: core::marker::PhantomData<T>,
}

// Safety: T: Pod implies T: Send + Copy, and all shared state uses atomics.
// The raw pointers point into an mmap region that outlives them (owned by _mmap).
unsafe impl<T: Pod> Send for PodBus<T> {}
unsafe impl<T: Pod> Sync for PodBus<T> {}

/// A subscriber that reads from a [`PodBus`] ring via shared memory.
///
/// Each subscriber tracks its own read position independently. Can live in a
/// different process from the publisher.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::{Pod, PodBusSubscriber};
///
/// let mut sub = PodBusSubscriber::<u64>::connect("example-prices").unwrap();
/// if let Some(value) = sub.try_recv() {
///     println!("got: {value}");
/// }
/// ```
pub struct PodBusSubscriber<T: Pod> {
    _mmap: RawMmap,
    ring_ptr: *const u8,
    mask: u64,
    ring_size: usize,
    write_seq_ptr: *const AtomicU64,
    cursor: u64,
    _marker: core::marker::PhantomData<T>,
}

// Safety: same reasoning as PodBus.
unsafe impl<T: Pod> Send for PodBusSubscriber<T> {}
unsafe impl<T: Pod> Sync for PodBusSubscriber<T> {}

impl<T: Pod> PodBus<T> {
    /// Create a new SPMC broadcast ring backed by shared memory.
    ///
    /// Creates a file at `/dev/shm/crossbar-pod-{name}` (Linux) containing the
    /// header and `ring_size` slots. Any previous file with the same name is
    /// removed first.
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::InvalidRegion`] if the name contains path
    /// separators or null bytes. Returns [`IpcError::Io`] if the backing file
    /// cannot be created.
    ///
    /// # Panics
    ///
    /// Panics if `ring_size` is zero or not a power of two.
    pub fn create(name: &str, ring_size: usize) -> Result<Self, IpcError> {
        assert!(
            ring_size > 0 && ring_size.is_power_of_two(),
            "ring_size must be a power of two"
        );

        validate_name(name)?;

        let path = pod_bus_shm_path(name);
        let size = region_size::<T>(ring_size);

        // Remove stale file
        let _ = std::fs::remove_file(&path);

        let file = {
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true).write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            opts.open(&path).map_err(IpcError::Io)?
        };
        file.set_len(size as u64).map_err(IpcError::Io)?;

        let mmap = RawMmap::from_file_with_len(&file, size).map_err(IpcError::Io)?;

        // Write header
        let base = mmap.as_mut_ptr();
        unsafe {
            core::ptr::copy_nonoverlapping(POD_BUS_MAGIC.as_ptr(), base.add(PH_MAGIC), 8);
            (base.add(PH_VERSION) as *mut u32).write(POD_BUS_VERSION);
            (base.add(PH_RING_SIZE) as *mut u32).write(ring_size as u32);
            (base.add(PH_VALUE_SIZE) as *mut u32).write(core::mem::size_of::<T>() as u32);
            (base.add(PH_VALUE_ALIGN) as *mut u32).write(core::mem::align_of::<T>() as u32);
            // write_seq starts at 0 (already zero from truncate, but be explicit)
            (base.add(PH_WRITE_SEQ) as *mut u64).write(0);
            (base.add(PH_PUBLISHER_PID) as *mut u64).write(u64::from(std::process::id()));
        }

        // Initialize all slot stamps to 0
        let ring_base = unsafe { base.add(POD_BUS_HEADER_SIZE) };
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

        Ok(Self {
            _mmap: mmap,
            ring_ptr: ring_base,
            mask: (ring_size as u64) - 1,
            ring_size,
            heartbeat_ptr,
            write_seq_ptr,
            path,
            _marker: core::marker::PhantomData,
        })
    }

    /// Publish a value into the ring. O(1) regardless of subscriber count.
    pub fn publish(&self, value: T) {
        // Counter-based heartbeat: update every 1024 publishes.
        // We need interior mutability for the counter, but since PodBus is
        // the sole publisher, we use a separate path. The counter is on the
        // stack of the caller conceptually, but we approximate by using the
        // atomic write_seq as an implicit counter.
        let write_seq = self.write_seq().load(Ordering::Relaxed);

        // Update heartbeat every 1024 publishes
        if write_seq & 0x3FF == 0 {
            if let Ok(now) = now_micros() {
                self.heartbeat().store(now, Ordering::Release);
            }
        }

        let idx = (write_seq & self.mask) as usize;
        let slot = self.slot_mut(idx);

        // Mark slot as being written (odd stamp).
        slot.stamp.store(write_seq * 2 + 1, Ordering::Release);

        // Write data.
        // Safety: we are the sole writer and the odd stamp signals readers to retry.
        unsafe {
            core::ptr::write_volatile(slot.data.get(), value);
        }

        // Mark slot as committed (even stamp >= 2).
        slot.stamp.store((write_seq + 1) * 2, Ordering::Release);

        // Advance sequence.
        self.write_seq().store(write_seq + 1, Ordering::Release);
    }

    /// Create a new subscriber for this bus (same process).
    ///
    /// The subscriber starts from the current write position so it only
    /// sees values published after this call.
    pub fn subscriber(&self) -> PodBusSubscriber<T> {
        let cursor = self.write_seq().load(Ordering::Acquire);
        PodBusSubscriber {
            // Safety: we create a second mmap of the same file. The subscriber
            // only reads; all writes go through atomics.
            _mmap: self.reopen_mmap(),
            ring_ptr: self.ring_ptr,
            mask: self.mask,
            ring_size: self.ring_size,
            write_seq_ptr: self.write_seq_ptr,
            cursor,
            _marker: core::marker::PhantomData,
        }
    }

    /// Return the total number of values published so far.
    pub fn published_count(&self) -> u64 {
        self.write_seq().load(Ordering::Acquire)
    }

    /// Return the ring capacity.
    pub fn ring_size(&self) -> usize {
        self.ring_size
    }

    /// Get a reference to the heartbeat atomic.
    #[inline]
    fn heartbeat(&self) -> &AtomicU64 {
        unsafe { &*self.heartbeat_ptr }
    }

    /// Get a reference to the write_seq atomic.
    #[inline]
    fn write_seq(&self) -> &AtomicU64 {
        unsafe { &*self.write_seq_ptr }
    }

    /// Get a mutable reference to a slot by index.
    #[inline]
    fn slot_mut(&self, idx: usize) -> &Slot<T> {
        debug_assert!(idx < self.ring_size);
        let slot_sz = slot_size::<T>();
        unsafe { &*(self.ring_ptr.add(idx * slot_sz) as *const Slot<T>) }
    }

    /// Re-open the mmap for the same file (used by subscriber()).
    /// For same-process subscribers we share the pointers, but we need
    /// a no-op mmap holder so the subscriber is self-contained when
    /// the publisher is dropped. Actually, the subscriber created via
    /// subscriber() shares the publisher's mmap lifetime. We create
    /// a separate mmap from the same file.
    fn reopen_mmap(&self) -> RawMmap {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .expect("PodBus SHM file disappeared");
        RawMmap::from_file(&file).expect("failed to re-mmap PodBus SHM file")
    }
}

impl<T: Pod> Drop for PodBus<T> {
    fn drop(&mut self) {
        // Clean up the SHM file. The publisher owns the file.
        let _ = std::fs::remove_file(&self.path);
    }
}

impl<T: Pod> PodBusSubscriber<T> {
    /// Connect to an existing [`PodBus`] by name.
    ///
    /// Opens the SHM file and validates the header (magic, version, type size
    /// and alignment). The subscriber starts reading from the current write
    /// position.
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::Io`] if the file doesn't exist.
    /// Returns [`IpcError::InvalidRegion`] if the header is invalid or the
    /// type parameters don't match.
    /// Returns [`IpcError::PublisherDead`] if the heartbeat is stale.
    pub fn connect(name: &str) -> Result<Self, IpcError> {
        validate_name(name)?;

        let path = pod_bus_shm_path(name);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true) // needed for atomic operations on the mmap
            .open(&path)
            .map_err(IpcError::Io)?;

        let mmap = RawMmap::from_file(&file).map_err(IpcError::Io)?;

        if mmap.len() < POD_BUS_HEADER_SIZE {
            return Err(IpcError::InvalidRegion(
                "PodBus region too small for header".into(),
            ));
        }

        let base = mmap.as_ptr();

        // Validate magic
        unsafe {
            let mut magic = [0u8; 8];
            core::ptr::copy_nonoverlapping(base.add(PH_MAGIC), magic.as_mut_ptr(), 8);
            if &magic != POD_BUS_MAGIC {
                return Err(IpcError::InvalidRegion(
                    "invalid PodBus magic (expected XPOD_ZC)".into(),
                ));
            }
        }

        // Validate version
        let version = unsafe { (base.add(PH_VERSION) as *const u32).read() };
        if version != POD_BUS_VERSION {
            return Err(IpcError::InvalidRegion(alloc::format!(
                "unsupported PodBus version {version}, expected {POD_BUS_VERSION}"
            )));
        }

        // Read ring parameters
        let ring_size = unsafe { (base.add(PH_RING_SIZE) as *const u32).read() } as usize;
        let value_size = unsafe { (base.add(PH_VALUE_SIZE) as *const u32).read() } as usize;
        let value_align = unsafe { (base.add(PH_VALUE_ALIGN) as *const u32).read() } as usize;

        // Validate ring_size
        if ring_size == 0 || !ring_size.is_power_of_two() {
            return Err(IpcError::InvalidRegion(alloc::format!(
                "PodBus ring_size {ring_size} is not a power of two"
            )));
        }

        // Validate type compatibility
        if value_size != core::mem::size_of::<T>() {
            return Err(IpcError::InvalidRegion(alloc::format!(
                "PodBus value_size mismatch: file has {value_size}, type needs {}",
                core::mem::size_of::<T>()
            )));
        }
        if value_align != core::mem::align_of::<T>() {
            return Err(IpcError::InvalidRegion(alloc::format!(
                "PodBus value_align mismatch: file has {value_align}, type needs {}",
                core::mem::align_of::<T>()
            )));
        }

        // Validate total region size
        let expected_size = region_size::<T>(ring_size);
        if mmap.len() < expected_size {
            return Err(IpcError::InvalidRegion(alloc::format!(
                "PodBus region size {} < expected {expected_size}",
                mmap.len()
            )));
        }

        // Check heartbeat liveness
        let hb =
            unsafe { &*(base.add(PH_HEARTBEAT_US) as *const AtomicU64) }.load(Ordering::Acquire);
        let now = now_micros()?;
        if now.saturating_sub(hb) > HEARTBEAT_STALE_US {
            return Err(IpcError::PublisherDead);
        }

        let write_seq_ptr = unsafe { &*(base.add(PH_WRITE_SEQ) as *const AtomicU64) };
        let cursor = write_seq_ptr.load(Ordering::Acquire);

        let ring_ptr = unsafe { base.add(POD_BUS_HEADER_SIZE) };

        Ok(Self {
            _mmap: mmap,
            ring_ptr,
            mask: (ring_size as u64) - 1,
            ring_size,
            write_seq_ptr,
            cursor,
            _marker: core::marker::PhantomData,
        })
    }

    /// Try to receive the next value from the ring.
    ///
    /// Returns `None` if no new data is available. If the subscriber has
    /// fallen behind (the slot it wants has been overwritten), it advances
    /// its cursor to the oldest still-available data.
    pub fn try_recv(&mut self) -> Option<T> {
        let head = self.write_seq().load(Ordering::Acquire);

        if self.cursor >= head {
            return None; // caught up
        }

        // If subscriber is more than ring_size behind, skip ahead.
        let ring_size = self.ring_size as u64;
        if head - self.cursor > ring_size {
            self.cursor = head - ring_size;
        }

        let idx = (self.cursor & self.mask) as usize;
        let slot = self.slot(idx);

        // Seqlock read: snapshot stamp, read data, verify stamp unchanged.
        let stamp_before = slot.stamp.load(Ordering::Acquire);

        // Odd stamp means write in progress — skip this attempt.
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
            // Stamp changed during read — data was torn, retry next call.
            return None;
        }

        // Verify this stamp corresponds to the sequence we expected.
        // Expected committed stamp for `self.cursor` is `(self.cursor + 1) * 2`.
        let expected_stamp = (self.cursor + 1) * 2;
        if stamp_before != expected_stamp {
            // Slot has been overwritten with a newer sequence — advance cursor.
            // Derive the sequence from the stamp: stamp = (seq + 1) * 2 => seq = stamp / 2 - 1
            let written_seq = stamp_before / 2 - 1;
            self.cursor = written_seq + 1;
            return None;
        }

        self.cursor += 1;
        Some(value)
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
        let bus = PodBus::<u64>::create(&name, 4).unwrap();
        let mut sub = bus.subscriber();

        assert!(sub.try_recv().is_none());

        bus.publish(42u64);
        assert_eq!(sub.try_recv(), Some(42));
        assert!(sub.try_recv().is_none());
    }

    #[test]
    fn multiple_subscribers() {
        let name = test_name("multi");
        let bus = PodBus::<u32>::create(&name, 8).unwrap();
        let mut s1 = bus.subscriber();
        let mut s2 = bus.subscriber();
        let mut s3 = bus.subscriber();

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
        let bus = PodBus::<u64>::create(&name, 4).unwrap();
        let mut sub = bus.subscriber();

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
        let bus = PodBus::<u64>::create(&name, 8).unwrap();
        bus.publish(100);
        bus.publish(200);

        // Connect as a separate subscriber (simulates cross-process).
        let mut sub = PodBusSubscriber::<u64>::connect(&name).unwrap();
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
        let result = PodBusSubscriber::<u32>::connect(&name);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_name_rejected() {
        let result = PodBus::<u64>::create("../evil", 8);
        assert!(result.is_err());

        let result = PodBus::<u64>::create("foo/bar", 8);
        assert!(result.is_err());
    }
}
