#![allow(unsafe_code)]

use super::mmap::RawMmap;
use crate::error::CrossbarError;
use crate::types::{Body, Method, Request, Response, ShmBodyGuard};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Returns the current monotonic timestamp in milliseconds (for slot timestamps).
///
/// Uses `clock_gettime(CLOCK_MONOTONIC_COARSE)` on Linux for ~6 ns cost
/// instead of `SystemTime::now()` at ~25 ns. Falls back to `Instant` on
/// other platforms.
#[inline]
fn monotonic_ms() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: ts is a valid mutable pointer to a timespec struct.
        unsafe {
            libc::clock_gettime(libc::CLOCK_MONOTONIC_COARSE, &mut ts);
        }
        ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Fallback: use a process-start epoch so values are comparable.
        use std::sync::OnceLock;
        use std::time::Instant;
        static EPOCH: OnceLock<Instant> = OnceLock::new();
        let epoch = EPOCH.get_or_init(Instant::now);
        epoch.elapsed().as_millis() as u64
    }
}

/// Magic bytes identifying a V2 shared memory region: `XBAR_V2\0`.
pub const MAGIC: &[u8; 8] = b"XBAR_V2\0";
/// Wire protocol version for the shared memory region layout.
pub const VERSION: u32 = 2;
/// Size of the global header at the start of the region, in bytes.
pub const GLOBAL_HEADER_SIZE: usize = 128;
/// Size of each coordination slot, in bytes.
pub const SLOT_SIZE: usize = 64;

// Sentinel for "no block assigned"
pub(crate) const NO_BLOCK: u32 = u32::MAX;

/// Slot state: no request is pending; the slot is available for acquisition.
pub const FREE: u32 = 0;
/// Slot state: a client has written a request and the server should process it.
pub const REQUEST_READY: u32 = 1;
/// Slot state: the server is processing the request.
pub const PROCESSING: u32 = 2;
/// Slot state: the server has written a response and the client should read it.
pub const RESPONSE_READY: u32 = 3;
/// Slot state: a client has acquired the slot and is writing request data.
/// The server must not read the slot until it transitions to
/// [`REQUEST_READY`].
pub const WRITING: u32 = 4;

/// Configuration for the shared memory transport.
///
/// All fields have sensible defaults via [`ShmConfig::default`]. Override
/// individual fields to tune concurrency, block sizing, or liveness detection.
///
/// # Examples
///
/// ```rust
/// use crossbar::transport::ShmConfig;
/// use std::time::Duration;
///
/// let config = ShmConfig {
///     slot_count: 128,
///     block_count: 384,
///     block_size: 128 * 1024, // 128 KiB
///     ..ShmConfig::default()
/// };
/// ```
#[derive(Debug, Clone)]
pub struct ShmConfig {
    /// Number of coordination slots (default: 64).
    pub slot_count: u32,
    /// Number of data blocks in the pool (default: 192, i.e. `slot_count * 3`).
    pub block_count: u32,
    /// Size of each data block in bytes (default: 65536 = 64 KiB).
    pub block_size: u32,
    /// Heartbeat interval (default: 100 ms).
    pub heartbeat_interval: Duration,
    /// Timeout for considering the server dead (default: 5 s).
    pub stale_timeout: Duration,
}

impl Default for ShmConfig {
    fn default() -> Self {
        Self {
            slot_count: 64,
            block_count: 192, // 64 * 3
            block_size: 65536,
            heartbeat_interval: Duration::from_millis(100),
            stale_timeout: Duration::from_secs(5),
        }
    }
}

/// Derives the file path for a named shared memory region.
///
/// On Linux this is `/dev/shm/crossbar-{name}`; on other Unix platforms
/// it falls back to `/tmp/crossbar-shm-{name}`.
#[must_use]
pub fn shm_path(name: &str) -> PathBuf {
    if cfg!(target_os = "linux") {
        PathBuf::from(format!("/dev/shm/crossbar-{name}"))
    } else {
        PathBuf::from(format!("/tmp/crossbar-shm-{name}"))
    }
}

// -- Treiber stack helpers --

#[inline]
fn pack(gen: u32, idx: u32) -> u64 {
    u64::from(gen) << 32 | u64::from(idx)
}

#[inline]
#[allow(clippy::cast_possible_truncation)] // intentional: extracting u32 halves from a u64
fn unpack(val: u64) -> (u32, u32) {
    ((val >> 32) as u32, val as u32)
}

fn region_size(config: &ShmConfig) -> Option<usize> {
    let slots = (config.slot_count as usize).checked_mul(SLOT_SIZE)?;
    let blocks = (config.block_count as usize).checked_mul(config.block_size as usize)?;
    GLOBAL_HEADER_SIZE.checked_add(slots)?.checked_add(blocks)
}

/// Handle to a V2 memory-mapped shared region with a lock-free block pool.
///
/// The region layout is: global header (128 bytes) | coordination slots |
/// data blocks. Coordination slots track request/response state machines;
/// data blocks hold serialized payloads. Blocks are managed by a Treiber
/// stack for lock-free allocation and deallocation.
pub struct ShmRegion {
    mmap: RawMmap,
    /// Number of coordination slots in this region.
    pub slot_count: u32,
    /// Number of data blocks in the pool.
    pub block_count: u32,
    /// Size of each data block in bytes.
    pub block_size: u32,
    /// File system path of the shared memory file.
    #[allow(dead_code)]
    pub path: PathBuf,
    /// Configuration used to create this region.
    #[allow(dead_code)]
    pub config: ShmConfig,
}

// SAFETY: The mmap is process-shared memory protected by atomic operations.
// All access to shared fields uses proper atomic orderings.
unsafe impl Send for ShmRegion {}
unsafe impl Sync for ShmRegion {}

impl ShmRegion {
    /// Creates a new V2 shared memory region (server side).
    ///
    /// # Errors
    ///
    /// Returns [`CrossbarError::ShmInvalidRegion`] if any config field is
    /// zero or the computed layout overflows, or [`CrossbarError::Io`] if the
    /// backing file cannot be created.
    pub fn create(name: &str, config: &ShmConfig) -> Result<Self, CrossbarError> {
        if config.slot_count == 0 {
            return Err(CrossbarError::ShmInvalidRegion(
                "slot_count must be > 0".into(),
            ));
        }
        if config.block_count == 0 {
            return Err(CrossbarError::ShmInvalidRegion(
                "block_count must be > 0".into(),
            ));
        }
        if config.block_size < 64 {
            return Err(CrossbarError::ShmInvalidRegion(format!(
                "block_size must be >= 64 (got {})",
                config.block_size
            )));
        }
        let path = shm_path(name);
        let size = region_size(config)
            .ok_or_else(|| CrossbarError::ShmInvalidRegion("layout size overflows".into()))?;

        // Remove any stale file
        let _ = std::fs::remove_file(&path);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(CrossbarError::Io)?;

        file.set_len(size as u64).map_err(CrossbarError::Io)?;

        let mmap = RawMmap::from_file_with_len(&file, size).map_err(CrossbarError::Io)?;

        // Write global header
        let ptr = mmap.as_mut_ptr();
        unsafe {
            // 0x00: magic (8B)
            std::ptr::copy_nonoverlapping(MAGIC.as_ptr(), ptr, 8);
            // 0x08: version (4B)
            std::ptr::copy_nonoverlapping(VERSION.to_le_bytes().as_ptr(), ptr.add(0x08), 4);
            // 0x0C: slot_count (4B)
            std::ptr::copy_nonoverlapping(
                config.slot_count.to_le_bytes().as_ptr(),
                ptr.add(0x0C),
                4,
            );
            // 0x10: block_count (4B)
            std::ptr::copy_nonoverlapping(
                config.block_count.to_le_bytes().as_ptr(),
                ptr.add(0x10),
                4,
            );
            // 0x14: block_size (4B)
            std::ptr::copy_nonoverlapping(
                config.block_size.to_le_bytes().as_ptr(),
                ptr.add(0x14),
                4,
            );
            // 0x20: server_pid (8B)
            let pid = u64::from(std::process::id());
            std::ptr::copy_nonoverlapping(pid.to_le_bytes().as_ptr(), ptr.add(0x20), 8);
        }

        let region = ShmRegion {
            mmap,
            slot_count: config.slot_count,
            block_count: config.block_count,
            block_size: config.block_size,
            path,
            config: config.clone(),
        };

        // Initialize heartbeat
        region.update_heartbeat();

        // Initialize all slots to FREE
        for i in 0..config.slot_count {
            region.slot_state(i).store(FREE, Ordering::Release);
            // Set block indices to NO_BLOCK
            region.set_request_block_idx(i, NO_BLOCK);
            region.set_response_block_idx(i, NO_BLOCK);
        }

        // Initialize free list: chain all blocks
        region.init_free_list();

        Ok(region)
    }

    /// Opens an existing V2 shared memory region (client side).
    ///
    /// # Errors
    ///
    /// Returns [`CrossbarError::Io`] if the backing file cannot be opened,
    /// or [`CrossbarError::ShmInvalidRegion`] if the header magic, version,
    /// or layout do not match.
    pub fn open(name: &str) -> Result<Self, CrossbarError> {
        let path = shm_path(name);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(CrossbarError::Io)?;

        let mmap = RawMmap::from_file(&file).map_err(CrossbarError::Io)?;

        if mmap.len() < GLOBAL_HEADER_SIZE {
            return Err(CrossbarError::ShmInvalidRegion(
                "region too small for header".into(),
            ));
        }

        let ptr = mmap.as_ptr();
        unsafe {
            let mut magic = [0u8; 8];
            std::ptr::copy_nonoverlapping(ptr, magic.as_mut_ptr(), 8);
            if &magic != MAGIC {
                return Err(CrossbarError::ShmInvalidRegion(
                    "invalid magic bytes (expected XBAR_V2)".into(),
                ));
            }

            let mut buf4 = [0u8; 4];

            std::ptr::copy_nonoverlapping(ptr.add(0x08), buf4.as_mut_ptr(), 4);
            let ver = u32::from_le_bytes(buf4);
            if ver != VERSION {
                return Err(CrossbarError::ShmInvalidRegion(format!(
                    "unsupported version {ver}, expected {VERSION}"
                )));
            }

            std::ptr::copy_nonoverlapping(ptr.add(0x0C), buf4.as_mut_ptr(), 4);
            let slot_count = u32::from_le_bytes(buf4);

            std::ptr::copy_nonoverlapping(ptr.add(0x10), buf4.as_mut_ptr(), 4);
            let block_count = u32::from_le_bytes(buf4);

            std::ptr::copy_nonoverlapping(ptr.add(0x14), buf4.as_mut_ptr(), 4);
            let block_size = u32::from_le_bytes(buf4);

            let expected_size = GLOBAL_HEADER_SIZE
                .checked_add(
                    (slot_count as usize)
                        .checked_mul(SLOT_SIZE)
                        .ok_or_else(|| {
                            CrossbarError::ShmInvalidRegion("layout size overflows (slots)".into())
                        })?,
                )
                .and_then(|s| {
                    s.checked_add((block_count as usize).checked_mul(block_size as usize)?)
                })
                .ok_or_else(|| CrossbarError::ShmInvalidRegion("layout size overflows".into()))?;
            if mmap.len() < expected_size {
                return Err(CrossbarError::ShmInvalidRegion(format!(
                    "region size {} < expected {expected_size}",
                    mmap.len()
                )));
            }
            if block_size < 64 {
                return Err(CrossbarError::ShmInvalidRegion(format!(
                    "block_size must be >= 64 (got {block_size})"
                )));
            }

            Ok(ShmRegion {
                mmap,
                slot_count,
                block_count,
                block_size,
                path,
                config: ShmConfig {
                    slot_count,
                    block_count,
                    block_size,
                    ..ShmConfig::default()
                },
            })
        }
    }

    // -- Server notification (global header offsets 0x30, 0x38) --

    /// Returns a reference to the server notification flag (offset 0x30).
    #[allow(clippy::cast_ptr_alignment)] // offset 0x30 is 4-byte aligned within the header
    fn server_notify_atomic(&self) -> &AtomicU32 {
        unsafe { &*self.mmap.as_ptr().add(0x30).cast::<AtomicU32>() }
    }

    /// Returns a reference to the server waiters counter (offset 0x38).
    ///
    /// Tracks how many threads are blocked in `wait_for_work()`. Used by
    /// `notify_server()` to skip the `futex_wake` syscall (~170 ns) when
    /// the server is already busy processing requests (smart wake).
    #[allow(clippy::cast_ptr_alignment)] // offset 0x38 is 4-byte aligned within the header
    fn server_waiters_atomic(&self) -> &AtomicU32 {
        unsafe { &*self.mmap.as_ptr().add(0x38).cast::<AtomicU32>() }
    }

    /// Signals the server that at least one slot has transitioned to
    /// `REQUEST_READY`.
    ///
    /// Smart wake: only issues the `futex_wake` syscall if the server is
    /// actually blocked in `wait_for_work()`. When the server is busy
    /// processing, it will see the flag on its next poll loop iteration
    /// without a syscall. Saves ~170 ns per request under load.
    #[inline]
    pub fn notify_server(&self) {
        let flag = self.server_notify_atomic();
        flag.store(1, Ordering::Release);
        // Only issue the syscall if the server is actually sleeping
        if self.server_waiters_atomic().load(Ordering::Acquire) > 0 {
            super::notify::wake_one(flag);
        }
    }

    /// Blocks the server thread until the notification flag becomes non-zero,
    /// then clears it and returns.
    ///
    /// Increments the waiters counter so `notify_server()` knows to issue
    /// a `futex_wake`. The timeout is capped at 1 ms so the server still
    /// checks the stop flag promptly.
    pub fn wait_for_work(&self) {
        let flag = self.server_notify_atomic();
        // Fast path: already signalled (no syscall needed).
        if flag.load(Ordering::Acquire) != 0 {
            flag.store(0, Ordering::Release);
            return;
        }
        // Slow path: register as waiter, then futex_wait.
        self.server_waiters_atomic().fetch_add(1, Ordering::Release);
        super::notify::wait_until_not(flag, 0, std::time::Duration::from_millis(1)).ok();
        self.server_waiters_atomic().fetch_sub(1, Ordering::Release);
        flag.store(0, Ordering::Release);
    }

    // -- Pool allocator (Treiber stack) --

    #[allow(clippy::cast_ptr_alignment)] // offset 0x28 is 8-byte aligned within the header
    fn pool_head(&self) -> &AtomicU64 {
        unsafe { &*self.mmap.as_ptr().add(0x28).cast::<AtomicU64>() }
    }

    /// Initialize the free list: chain block 0 -> 1 -> 2 -> ... -> `NO_BLOCK`.
    fn init_free_list(&self) {
        for i in 0..self.block_count {
            let next = if i + 1 < self.block_count {
                i + 1
            } else {
                NO_BLOCK
            };
            self.write_block_next_free(i, next);
        }
        // Head points to block 0 with generation 0
        self.pool_head().store(pack(0, 0), Ordering::Release);
    }

    fn read_block_next_free(&self, idx: u32) -> u32 {
        let ptr = self.block_ptr(idx);
        unsafe {
            let mut buf = [0u8; 4];
            std::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), 4);
            u32::from_le_bytes(buf)
        }
    }

    fn write_block_next_free(&self, idx: u32, next: u32) {
        let ptr = self.block_ptr(idx);
        unsafe {
            std::ptr::copy_nonoverlapping(next.to_le_bytes().as_ptr(), ptr, 4);
        }
    }

    /// Allocates a block from the pool.
    ///
    /// Returns the block index, or `None` if all blocks are in use.
    #[inline]
    pub fn alloc_block(&self) -> Option<u32> {
        loop {
            let head = self.pool_head().load(Ordering::Acquire);
            let (gen, idx) = unpack(head);
            if idx == NO_BLOCK {
                return None;
            }
            let next_idx = self.read_block_next_free(idx);
            let new_head = pack(gen.wrapping_add(1), next_idx);
            if self
                .pool_head()
                .compare_exchange(head, new_head, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(idx);
            }
        }
    }

    /// Returns a block to the pool.
    ///
    /// # Panics
    ///
    /// Panics if `idx` is out of bounds (>= `block_count`).
    #[inline]
    pub fn free_block(&self, idx: u32) {
        assert!(
            (idx as usize) < self.block_count as usize,
            "free_block: index {idx} out of bounds"
        );
        loop {
            let head = self.pool_head().load(Ordering::Acquire);
            let (gen, old_head_idx) = unpack(head);
            self.write_block_next_free(idx, old_head_idx);
            let new_head = pack(gen.wrapping_add(1), idx);
            if self
                .pool_head()
                .compare_exchange(head, new_head, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Returns a raw pointer to the start of block `idx`.
    ///
    /// # Panics
    ///
    /// Panics if `idx` is out of bounds (>= `block_count`).
    #[inline]
    pub fn block_ptr(&self, idx: u32) -> *mut u8 {
        assert!(
            (idx as usize) < self.block_count as usize,
            "block index {idx} out of bounds (max {})",
            self.block_count
        );
        let offset = GLOBAL_HEADER_SIZE
            + self.slot_count as usize * SLOT_SIZE
            + idx as usize * self.block_size as usize;
        unsafe { self.mmap.as_ptr().add(offset).cast_mut() }
    }

    /// Returns a slice of the block's data region.
    fn block_slice(&self, idx: u32, offset: usize, len: usize) -> &[u8] {
        let ptr = self.block_ptr(idx);
        unsafe { std::slice::from_raw_parts(ptr.add(offset), len) }
    }

    // -- Slot access --

    fn slot_base(&self, idx: u32) -> *mut u8 {
        debug_assert!((idx as usize) < self.slot_count as usize);
        unsafe {
            self.mmap
                .as_ptr()
                .add(GLOBAL_HEADER_SIZE + idx as usize * SLOT_SIZE)
                .cast_mut()
        }
    }

    /// Returns a reference to the slot's atomic state field (offset 0x00).
    #[inline]
    #[allow(clippy::cast_ptr_alignment)] // slot_base is 64-byte aligned
    pub fn slot_state(&self, idx: u32) -> &AtomicU32 {
        unsafe { &*self.slot_base(idx).cast::<AtomicU32>() }
    }

    /// Returns a reference to the slot's sequence counter (offset 0x04).
    #[allow(clippy::cast_ptr_alignment)] // offset 0x04 within a 64-byte aligned slot
    pub fn slot_sequence(&self, idx: u32) -> &AtomicU32 {
        unsafe { &*self.slot_base(idx).add(4).cast::<AtomicU32>() }
    }

    /// Writes the `client_id` into a slot (offset 0x08).
    pub fn set_slot_client_id(&self, idx: u32, client_id: u64) {
        unsafe {
            let ptr = self.slot_base(idx).add(0x08);
            std::ptr::copy_nonoverlapping(client_id.to_le_bytes().as_ptr(), ptr, 8);
        }
    }

    /// Reads the `client_id` from a slot.
    #[allow(dead_code)]
    pub fn slot_client_id(&self, idx: u32) -> u64 {
        unsafe {
            let ptr = self.slot_base(idx).add(0x08);
            let mut buf = [0u8; 8];
            std::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), 8);
            u64::from_le_bytes(buf)
        }
    }

    /// Updates the slot timestamp to now (offset 0x10).
    ///
    /// Uses `monotonic_ms()` (~6 ns on Linux) instead of `SystemTime::now()`
    /// (~25 ns) to avoid vDSO overhead on the hot path.
    pub fn touch_slot(&self, idx: u32) {
        let now = monotonic_ms();
        unsafe {
            let ptr = self.slot_base(idx).add(0x10);
            std::ptr::copy_nonoverlapping(now.to_le_bytes().as_ptr(), ptr, 8);
        }
    }

    /// Reads the slot timestamp.
    pub fn slot_timestamp(&self, idx: u32) -> u64 {
        unsafe {
            let ptr = self.slot_base(idx).add(0x10);
            let mut buf = [0u8; 8];
            std::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), 8);
            u64::from_le_bytes(buf)
        }
    }

    // -- Coordination slot metadata accessors --

    /// Sets the method byte in the coordination slot (offset 0x18).
    fn set_slot_method(&self, idx: u32, method: u8) {
        unsafe {
            *self.slot_base(idx).add(0x18) = method;
        }
    }

    /// Reads the method byte from the coordination slot.
    fn slot_method(&self, idx: u32) -> u8 {
        unsafe { *self.slot_base(idx).add(0x18) }
    }

    fn read_slot_u32(&self, idx: u32, offset: usize) -> u32 {
        unsafe {
            let ptr = self.slot_base(idx).add(offset);
            let mut buf = [0u8; 4];
            std::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), 4);
            u32::from_le_bytes(buf)
        }
    }

    fn write_slot_u32(&self, idx: u32, offset: usize, val: u32) {
        unsafe {
            let ptr = self.slot_base(idx).add(offset);
            std::ptr::copy_nonoverlapping(val.to_le_bytes().as_ptr(), ptr, 4);
        }
    }

    fn read_slot_u16(&self, idx: u32, offset: usize) -> u16 {
        unsafe {
            let ptr = self.slot_base(idx).add(offset);
            let mut buf = [0u8; 2];
            std::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), 2);
            u16::from_le_bytes(buf)
        }
    }

    fn write_slot_u16(&self, idx: u32, offset: usize, val: u16) {
        unsafe {
            let ptr = self.slot_base(idx).add(offset);
            std::ptr::copy_nonoverlapping(val.to_le_bytes().as_ptr(), ptr, 2);
        }
    }

    // Slot field accessors per V2 layout:
    // 0x1C: uri_len (u32)
    // 0x20: body_len (u32)
    // 0x24: headers_data_len (u32)
    // 0x28: request_block_idx (u32)
    // 0x2C: response_block_idx (u32)
    // 0x30: response_status (u16)
    // 0x34: response_body_len (u32)
    // 0x38: response_headers_data_len (u32)

    /// Sets the URI length in the coordination slot at `idx`.
    pub fn set_uri_len(&self, idx: u32, val: u32) {
        self.write_slot_u32(idx, 0x1C, val);
    }
    /// Reads the URI length from the coordination slot at `idx`.
    pub fn uri_len(&self, idx: u32) -> u32 {
        self.read_slot_u32(idx, 0x1C)
    }

    /// Sets the request body length in the coordination slot at `idx`.
    pub fn set_body_len(&self, idx: u32, val: u32) {
        self.write_slot_u32(idx, 0x20, val);
    }
    /// Reads the request body length from the coordination slot at `idx`.
    pub fn body_len(&self, idx: u32) -> u32 {
        self.read_slot_u32(idx, 0x20)
    }

    /// Sets the headers data length in the coordination slot at `idx`.
    pub fn set_headers_data_len(&self, idx: u32, val: u32) {
        self.write_slot_u32(idx, 0x24, val);
    }
    /// Reads the headers data length from the coordination slot at `idx`.
    pub fn headers_data_len(&self, idx: u32) -> u32 {
        self.read_slot_u32(idx, 0x24)
    }

    /// Sets the request block index in the coordination slot at `idx`.
    pub fn set_request_block_idx(&self, idx: u32, val: u32) {
        self.write_slot_u32(idx, 0x28, val);
    }
    /// Reads the request block index from the coordination slot at `idx`.
    pub fn request_block_idx(&self, idx: u32) -> u32 {
        self.read_slot_u32(idx, 0x28)
    }

    /// Sets the response block index in the coordination slot at `idx`.
    pub fn set_response_block_idx(&self, idx: u32, val: u32) {
        self.write_slot_u32(idx, 0x2C, val);
    }
    /// Reads the response block index from the coordination slot at `idx`.
    pub fn response_block_idx(&self, idx: u32) -> u32 {
        self.read_slot_u32(idx, 0x2C)
    }

    /// Sets the response status code in the coordination slot at `idx`.
    pub fn set_response_status(&self, idx: u32, val: u16) {
        self.write_slot_u16(idx, 0x30, val);
    }
    /// Reads the response status code from the coordination slot at `idx`.
    pub fn response_status(&self, idx: u32) -> u16 {
        self.read_slot_u16(idx, 0x30)
    }

    /// Sets the response body length in the coordination slot at `idx`.
    pub fn set_response_body_len(&self, idx: u32, val: u32) {
        self.write_slot_u32(idx, 0x34, val);
    }
    /// Reads the response body length from the coordination slot at `idx`.
    pub fn response_body_len(&self, idx: u32) -> u32 {
        self.read_slot_u32(idx, 0x34)
    }

    /// Sets the response headers data length in the coordination slot at `idx`.
    pub fn set_response_headers_data_len(&self, idx: u32, val: u32) {
        self.write_slot_u32(idx, 0x38, val);
    }
    /// Reads the response headers data length from the coordination slot at `idx`.
    pub fn response_headers_data_len(&self, idx: u32) -> u32 {
        self.read_slot_u32(idx, 0x38)
    }

    // -- Request/Response block I/O --

    /// Writes a request directly into a block.
    ///
    /// Block layout: `[uri bytes][headers_data][body bytes]`.
    /// Coordination slot metadata fields are also written.
    ///
    /// # Errors
    ///
    /// Returns [`CrossbarError::ShmMessageTooLarge`] if the request payload
    /// exceeds the block capacity, or [`CrossbarError::HeaderOverflow`] if
    /// header serialization fails.
    #[inline]
    #[allow(clippy::cast_possible_truncation)] // lengths bounded by block_size (u32)
    pub fn write_request_to_block(
        &self,
        slot_idx: u32,
        block_idx: u32,
        req: &Request,
    ) -> Result<(), CrossbarError> {
        let uri_bytes = req.uri.raw().as_bytes();
        let body_len = req.body.len();
        let block_cap = self.block_size as usize;
        let block = self.block_ptr(block_idx);

        // Block layout: [body][uri][headers] — body first for born-in-SHM compat
        let mut off = 0usize;

        // Write body into block
        if body_len > block_cap {
            return Err(CrossbarError::ShmMessageTooLarge {
                size: body_len,
                max: block_cap,
            });
        }
        unsafe {
            std::ptr::copy_nonoverlapping(req.body.as_ptr(), block.add(off), body_len);
        }
        off += body_len;

        // Write URI after body
        if off + uri_bytes.len() > block_cap {
            return Err(CrossbarError::ShmMessageTooLarge {
                size: off + uri_bytes.len(),
                max: block_cap,
            });
        }
        unsafe {
            std::ptr::copy_nonoverlapping(uri_bytes.as_ptr(), block.add(off), uri_bytes.len());
        }
        off += uri_bytes.len();

        // Write headers after URI
        let remaining = unsafe { std::slice::from_raw_parts_mut(block.add(off), block_cap - off) };
        let headers_len = crate::transport::serialize_headers_into(&req.headers, remaining)?;

        // Write metadata to coordination slot
        self.set_slot_method(slot_idx, u8::from(req.method));
        self.set_uri_len(slot_idx, uri_bytes.len() as u32);
        self.set_body_len(slot_idx, body_len as u32);
        self.set_headers_data_len(slot_idx, headers_len as u32);
        self.set_request_block_idx(slot_idx, block_idx);

        Ok(())
    }

    /// Finalizes a born-in-SHM request: the body is already in the block
    /// at offset 0, so only URI and headers are appended after the body.
    ///
    /// Block layout: `[body bytes (already written)][uri][headers_data]`.
    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    pub fn write_request_direct(
        &self,
        slot_idx: u32,
        block_idx: u32,
        body_len: usize,
        method: Method,
        uri: &str,
        headers: &std::collections::HashMap<String, String>,
    ) -> Result<(), CrossbarError> {
        let uri_bytes = uri.as_bytes();
        let block_cap = self.block_size as usize;
        let block = self.block_ptr(block_idx);

        let mut off = body_len;

        // Write URI after body
        if off + uri_bytes.len() > block_cap {
            return Err(CrossbarError::ShmMessageTooLarge {
                size: off + uri_bytes.len(),
                max: block_cap,
            });
        }
        unsafe {
            std::ptr::copy_nonoverlapping(uri_bytes.as_ptr(), block.add(off), uri_bytes.len());
        }
        off += uri_bytes.len();

        // Write headers after URI
        let remaining = unsafe { std::slice::from_raw_parts_mut(block.add(off), block_cap - off) };
        let headers_len = crate::transport::serialize_headers_into(headers, remaining)?;

        // Write metadata to coordination slot
        self.set_slot_method(slot_idx, u8::from(method));
        self.set_uri_len(slot_idx, uri_bytes.len() as u32);
        self.set_body_len(slot_idx, body_len as u32);
        self.set_headers_data_len(slot_idx, headers_len as u32);
        self.set_request_block_idx(slot_idx, block_idx);

        Ok(())
    }

    /// Reads a request from a block.
    ///
    /// The body is zero-copy via `Body::Mmap`.
    /// Block layout: `[body bytes][uri bytes][headers_data]`.
    ///
    /// # Errors
    ///
    /// Returns an error if the block contents are malformed (invalid method
    /// byte, non-UTF-8 URI, truncated headers, or payload exceeding block
    /// size). The block is freed before returning on error.
    #[inline]
    pub fn read_request_from_block(
        self: &Arc<Self>,
        slot_idx: u32,
    ) -> Result<Request, CrossbarError> {
        let block_idx = self.request_block_idx(slot_idx);
        let block_cap = self.block_size as usize;

        let method_byte = self.slot_method(slot_idx);
        let method = match Method::try_from(method_byte) {
            Ok(m) => m,
            Err(e) => {
                self.free_block(block_idx);
                return Err(CrossbarError::InvalidMethod(e));
            }
        };
        let uri_len = self.uri_len(slot_idx) as usize;
        let body_len = self.body_len(slot_idx) as usize;
        let headers_data_len = self.headers_data_len(slot_idx) as usize;

        // Bounds check
        let total = uri_len
            .checked_add(headers_data_len)
            .and_then(|v| v.checked_add(body_len));
        match total {
            Some(t) if t <= block_cap => {}
            _ => {
                self.free_block(block_idx);
                return Err(CrossbarError::ShmInvalidRegion(format!(
                    "request payload ({uri_len}+{headers_data_len}+{body_len}) exceeds block size ({block_cap})"
                )));
            }
        }

        // Parse URI and headers BEFORE creating Body::Mmap to avoid
        // double-free: if we created Body::Mmap first, then free_block on
        // error, the Mmap guard would also free on drop.
        let uri_slice = self.block_slice(block_idx, body_len, uri_len);
        let Ok(uri_str) = std::str::from_utf8(uri_slice) else {
            self.free_block(block_idx);
            return Err(CrossbarError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "URI not valid UTF-8",
            )));
        };

        let headers_off = body_len + uri_len;
        let headers = if headers_data_len == 0 {
            std::collections::HashMap::new()
        } else {
            let headers_slice = self.block_slice(block_idx, headers_off, headers_data_len);
            match crate::transport::deserialize_headers(headers_slice) {
                Ok(h) => h,
                Err(e) => {
                    self.free_block(block_idx);
                    return Err(e);
                }
            }
        };

        // Body at offset 0 — zero-copy via Body::Mmap
        // Created after URI/headers parsing so block ownership is clear.
        let body = if body_len == 0 {
            Body::Empty
        } else {
            Body::Mmap(ShmBodyGuard {
                region: Arc::clone(self),
                block_idx,
                offset: 0,
                len: body_len,
            })
        };

        let mut req = Request::new(method, uri_str).with_body(body);
        req.headers = headers;

        // If body is empty, free the block immediately since we won't hold it
        if body_len == 0 {
            self.free_block(block_idx);
        }

        Ok(req)
    }

    /// Writes a response directly into a block.
    ///
    /// Block layout: `[body bytes][headers_data]` (body first for born-in-SHM
    /// compatibility — see [`write_response_direct`]).
    ///
    /// If the response exceeds block capacity or headers fail to serialize,
    /// a 500 fallback is written instead (this method never fails).
    #[inline]
    #[allow(clippy::cast_possible_truncation)] // lengths bounded by block_size (u32)
    pub fn write_response_to_block(&self, slot_idx: u32, block_idx: u32, resp: &Response) {
        let block_cap = self.block_size as usize;
        let block = self.block_ptr(block_idx);

        // Try to write the actual response
        let result = (|| -> Result<(u16, usize, usize), CrossbarError> {
            let mut off = 0usize;

            // Write body first
            if resp.body.len() > block_cap {
                return Err(CrossbarError::ShmMessageTooLarge {
                    size: resp.body.len(),
                    max: block_cap,
                });
            }
            unsafe {
                std::ptr::copy_nonoverlapping(resp.body.as_ptr(), block.add(off), resp.body.len());
            }
            off += resp.body.len();

            // Write headers after body
            let remaining =
                unsafe { std::slice::from_raw_parts_mut(block.add(off), block_cap - off) };
            let headers_len = crate::transport::serialize_headers_into(&resp.headers, remaining)?;

            Ok((resp.status, headers_len, resp.body.len()))
        })();

        let (status, headers_data_len, body_len) = result.unwrap_or_else(|_| {
            // Fallback: write a 500 error into the block, but respect block_cap.
            let fallback_body = b"response too large for shm block";
            let fallback_headers = [0u8, 0u8]; // 0 headers
            let total = fallback_body.len() + fallback_headers.len();
            if total <= block_cap {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        fallback_body.as_ptr(),
                        block,
                        fallback_body.len(),
                    );
                    std::ptr::copy_nonoverlapping(
                        fallback_headers.as_ptr(),
                        block.add(fallback_body.len()),
                        fallback_headers.len(),
                    );
                }
                (500u16, fallback_headers.len(), fallback_body.len())
            } else if block_cap >= 2 {
                // Block too small for fallback body — write just the header count.
                unsafe {
                    std::ptr::copy_nonoverlapping(fallback_headers.as_ptr(), block, 2);
                }
                (500u16, 2, 0)
            } else {
                // Block cannot even hold the header count field.
                (500u16, 0, 0)
            }
        });

        // Write metadata to coordination slot
        self.set_response_block_idx(slot_idx, block_idx);
        self.set_response_status(slot_idx, status);
        self.set_response_headers_data_len(slot_idx, headers_data_len as u32);
        self.set_response_body_len(slot_idx, body_len as u32);
    }

    /// Finalizes a born-in-SHM response: the body is already in the block
    /// at offset 0, so only headers are appended after the body.
    ///
    /// Block layout: `[body bytes (already written)][headers_data]`.
    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    pub fn write_response_direct(
        &self,
        slot_idx: u32,
        block_idx: u32,
        body_len: usize,
        headers: &std::collections::HashMap<String, String>,
        status: u16,
    ) {
        let block_cap = self.block_size as usize;
        let block = self.block_ptr(block_idx);

        // Write headers after the body
        let headers_data_len = if body_len < block_cap {
            let remaining = unsafe {
                std::slice::from_raw_parts_mut(block.add(body_len), block_cap - body_len)
            };
            crate::transport::serialize_headers_into(headers, remaining).unwrap_or(0)
        } else {
            0
        };

        // Write metadata to coordination slot
        self.set_response_block_idx(slot_idx, block_idx);
        self.set_response_status(slot_idx, status);
        self.set_response_headers_data_len(slot_idx, headers_data_len as u32);
        self.set_response_body_len(slot_idx, body_len as u32);
    }

    /// Reads a response from a block.
    ///
    /// The body is zero-copy via `Body::Mmap`.
    /// Block layout: `[body bytes][headers_data]`.
    ///
    /// # Errors
    ///
    /// Returns an error if the block contents are malformed (truncated
    /// headers or payload exceeding block size). The block is freed before
    /// returning on error.
    #[inline]
    pub fn read_response_from_block(
        self: &Arc<Self>,
        slot_idx: u32,
    ) -> Result<Response, CrossbarError> {
        let block_idx = self.response_block_idx(slot_idx);
        let block_cap = self.block_size as usize;

        let status = self.response_status(slot_idx);
        let body_len = self.response_body_len(slot_idx) as usize;
        let headers_data_len = self.response_headers_data_len(slot_idx) as usize;

        // Bounds check
        let total = body_len.checked_add(headers_data_len);
        match total {
            Some(t) if t <= block_cap => {}
            _ => {
                // Free the block before returning — caller won't see it again.
                self.free_block(block_idx);
                return Err(CrossbarError::ShmInvalidRegion(format!(
                    "response payload ({body_len}+{headers_data_len}) exceeds block size ({block_cap})"
                )));
            }
        }

        // Body at offset 0 — zero-copy via Body::Mmap
        let body = if body_len == 0 {
            Body::Empty
        } else {
            Body::Mmap(ShmBodyGuard {
                region: Arc::clone(self),
                block_idx,
                offset: 0,
                len: body_len,
            })
        };

        // Headers after body
        let headers = if headers_data_len == 0 {
            std::collections::HashMap::new()
        } else {
            let headers_slice = self.block_slice(block_idx, body_len, headers_data_len);
            match crate::transport::deserialize_headers(headers_slice) {
                Ok(h) => h,
                Err(e) => {
                    // Drop the body guard first (if any) to avoid double-free
                    drop(body);
                    if body_len == 0 {
                        self.free_block(block_idx);
                    }
                    return Err(e);
                }
            }
        };

        // If body is empty, free the block immediately
        if body_len == 0 {
            self.free_block(block_idx);
        }

        Ok(Response {
            status,
            headers,
            body,
        })
    }

    // -- Heartbeat --

    /// Updates the server heartbeat timestamp.
    #[allow(clippy::cast_possible_truncation)] // millis since epoch fits in u64 for ~585 million years
    pub fn update_heartbeat(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let hb = self.heartbeat_atomic();
        hb.store(now, Ordering::Release);
    }

    /// Checks if the server heartbeat is recent enough.
    ///
    /// # Errors
    ///
    /// Returns [`CrossbarError::ShmServerDead`] if the heartbeat is older
    /// than `stale_timeout`.
    #[allow(clippy::cast_possible_truncation)] // millis since epoch fits in u64
    pub fn check_heartbeat(&self, stale_timeout: Duration) -> Result<(), CrossbarError> {
        let hb = self.heartbeat_atomic();
        let last = hb.load(Ordering::Acquire);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if now.saturating_sub(last) > stale_timeout.as_millis() as u64 {
            return Err(CrossbarError::ShmServerDead);
        }
        Ok(())
    }

    #[allow(clippy::cast_ptr_alignment)] // offset 0x18 is 8-byte aligned within the header
    fn heartbeat_atomic(&self) -> &AtomicU64 {
        unsafe { &*self.mmap.as_ptr().add(0x18).cast::<AtomicU64>() }
    }

    // -- Slot acquisition --

    /// Returns a reference to the client slot hint (global header offset 0x34).
    #[allow(clippy::cast_ptr_alignment)]
    #[inline]
    fn slot_hint(&self) -> &AtomicU32 {
        unsafe { &*self.mmap.as_ptr().add(0x34).cast::<AtomicU32>() }
    }

    /// Returns a reference to the server slot hint (global header offset 0x3C).
    ///
    /// Tracks where the server last found a REQUEST_READY slot, so the poll
    /// loop starts scanning there instead of slot 0. Gives O(1) amortised
    /// scanning for sequential workloads.
    #[allow(clippy::cast_ptr_alignment)]
    #[inline]
    pub fn server_slot_hint(&self) -> &AtomicU32 {
        unsafe { &*self.mmap.as_ptr().add(0x3C).cast::<AtomicU32>() }
    }

    /// Tries to acquire a FREE slot via CAS. Returns the slot index or None.
    ///
    /// Scanning starts at the hint index (last-acquired slot + 1) and wraps
    /// around, giving O(1) amortised acquisition under low contention.
    #[inline]
    pub fn try_acquire_slot(&self, client_id: u64) -> Option<u32> {
        let n = self.slot_count;
        let start = self.slot_hint().load(Ordering::Relaxed) % n;
        for offset in 0..n {
            let i = (start + offset) % n;
            let state = self.slot_state(i);
            if state
                .compare_exchange(FREE, WRITING, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.set_slot_client_id(i, client_id);
                self.slot_sequence(i).fetch_add(1, Ordering::Relaxed);
                self.touch_slot(i);
                // Advance the hint past this slot for the next caller.
                self.slot_hint().store(i.wrapping_add(1), Ordering::Relaxed);
                return Some(i);
            }
        }
        None
    }

    /// Scans for stale slots and resets them to [`FREE`].
    pub fn recover_stale_slots(&self, stale_timeout: Duration) {
        let now = monotonic_ms();

        for i in 0..self.slot_count {
            let state = self.slot_state(i);
            let current = state.load(Ordering::Acquire);

            if current == FREE || current == PROCESSING {
                continue;
            }

            let ts = self.slot_timestamp(i);
            if now.saturating_sub(ts) > stale_timeout.as_millis() as u64 {
                // CAS to PROCESSING first so no client can reacquire the slot
                // while we are freeing its blocks.
                if state
                    .compare_exchange(current, PROCESSING, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    // Safe: slot is PROCESSING, no client can acquire it.
                    // Free request block if assigned
                    let req_block = self.request_block_idx(i);
                    if req_block != NO_BLOCK {
                        self.free_block(req_block);
                        self.set_request_block_idx(i, NO_BLOCK);
                    }
                    // Free response block if assigned
                    let resp_block = self.response_block_idx(i);
                    if resp_block != NO_BLOCK {
                        self.free_block(resp_block);
                        self.set_response_block_idx(i, NO_BLOCK);
                    }
                    // NOW release the slot to FREE
                    state.store(FREE, Ordering::Release);
                }
            }
        }
    }
}
