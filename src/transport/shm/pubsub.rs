#![allow(unsafe_code)]

//! Zero-copy pub/sub over shared memory.
//!
//! Data is never serialized or deserialized by the transport — the publisher
//! writes raw bytes directly into mmap, the subscriber reads them in-place.
//! What those bytes mean (JSON, rkyv, protobuf, a raw struct) is entirely
//! the user's business.
//!
//! Per-publish overhead: one `memcpy` (data into mmap) + two atomic stores.
//! No URI, no headers, no method byte, no syscall — just bytes.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::notify;
use crate::error::CrossbarError;

// ─── Layout constants ────────────────────────────────────────────────────

const MAGIC: &[u8; 8] = b"XBAR_PS\0";
const VERSION: u32 = 1;

const GLOBAL_HEADER_SIZE: usize = 128;
const TOPIC_ENTRY_SIZE: usize = 128;
const SAMPLE_HEADER_SIZE: usize = 16;

// Global header offsets
const GH_MAGIC: usize = 0;
const GH_VERSION: usize = 8;
const GH_MAX_TOPICS: usize = 12;
const GH_SAMPLE_CAPACITY: usize = 16;
const GH_RING_DEPTH: usize = 20;
const GH_HEARTBEAT: usize = 24; // AtomicU64
const GH_PID: usize = 32;
const GH_STALE_TIMEOUT_US: usize = 40; // u64: stale timeout in microseconds

// Topic entry offsets (relative to entry start)
const TE_ACTIVE: usize = 0; // AtomicU32: 0=free, 1=active
const TE_NOTIFY: usize = 4; // AtomicU32: bumped on publish, futex target
const TE_WRITE_SEQ: usize = 8; // AtomicU64: monotonic publish counter
const TE_URI_HASH: usize = 16; // u64
const TE_URI_LEN: usize = 24; // u8
const TE_URI_STR: usize = 25; // [u8; 103]
const MAX_URI_LEN: usize = 103;

// Sample slot offsets (relative to slot start)
const SS_SEQ: usize = 0; // u64 (written atomically by publisher)
const SS_DATA_LEN: usize = 8; // u32
                              // offset 12: padding
                              // offset 16: data[sample_capacity]

// ─── Configuration ───────────────────────────────────────────────────────

/// Configuration for a pub/sub shared memory region.
#[derive(Debug, Clone)]
pub struct PubSubConfig {
    /// Maximum number of topics (default: 16).
    pub max_topics: u32,
    /// Maximum bytes per sample (default: 65536 = 64 KiB).
    pub sample_capacity: u32,
    /// Number of samples in each topic's ring buffer (default: 8).
    pub ring_depth: u32,
    /// How often the publisher updates its heartbeat (default: 100 ms).
    pub heartbeat_interval: Duration,
    /// How long before a publisher is considered dead (default: 5 s).
    pub stale_timeout: Duration,
}

impl Default for PubSubConfig {
    fn default() -> Self {
        Self {
            max_topics: 16,
            sample_capacity: 65536,
            ring_depth: 8,
            heartbeat_interval: Duration::from_millis(100),
            stale_timeout: Duration::from_secs(5),
        }
    }
}

// ─── FNV-1a hash ─────────────────────────────────────────────────────────

fn uri_hash(uri: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in uri.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ─── Shared offset helpers ───────────────────────────────────────────────

fn topic_entry_off(idx: u32) -> usize {
    GLOBAL_HEADER_SIZE + idx as usize * TOPIC_ENTRY_SIZE
}

fn ring_base(cfg: &PubSubConfig) -> usize {
    GLOBAL_HEADER_SIZE + cfg.max_topics as usize * TOPIC_ENTRY_SIZE
}

fn sample_stride(cfg: &PubSubConfig) -> usize {
    // Round up to 8-byte alignment so AtomicU64 in SS_SEQ is always aligned.
    let raw = SAMPLE_HEADER_SIZE + cfg.sample_capacity as usize;
    (raw + 7) & !7
}

fn ring_offset_for(cfg: &PubSubConfig, topic_idx: u32) -> usize {
    ring_base(cfg) + topic_idx as usize * cfg.ring_depth as usize * sample_stride(cfg)
}

fn sample_offset(cfg: &PubSubConfig, topic_idx: u32, slot: u32) -> usize {
    ring_offset_for(cfg, topic_idx) + slot as usize * sample_stride(cfg)
}

fn total_size(cfg: &PubSubConfig) -> usize {
    checked_total_size(cfg).expect("layout overflow in total_size (must validate beforehand)")
}

/// Returns `None` if the layout overflows `usize`.
fn checked_total_size(cfg: &PubSubConfig) -> Option<usize> {
    // Checked ring_base: GLOBAL_HEADER_SIZE + max_topics * TOPIC_ENTRY_SIZE
    let base = (cfg.max_topics as usize)
        .checked_mul(TOPIC_ENTRY_SIZE)?
        .checked_add(GLOBAL_HEADER_SIZE)?;
    // Checked sample_stride: align_up(SAMPLE_HEADER_SIZE + sample_capacity, 8)
    let raw_stride = SAMPLE_HEADER_SIZE.checked_add(cfg.sample_capacity as usize)?;
    let stride = raw_stride.checked_add(7)? & !7;
    // Total ring area
    let ring_bytes = (cfg.max_topics as usize)
        .checked_mul(cfg.ring_depth as usize)?
        .checked_mul(stride)?;
    base.checked_add(ring_bytes)
}

fn shm_path(name: &str) -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        PathBuf::from(format!("/dev/shm/crossbar-ps-{name}"))
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        PathBuf::from(format!("/tmp/crossbar-ps-{name}"))
    }
}

// ─── ShmPublisher ────────────────────────────────────────────────────────

/// A cached handle to a registered topic. Eliminates hash lookup and
/// heartbeat overhead on the hot path — use with [`ShmPublisher::loan_to`].
///
/// Returned by [`ShmPublisher::register`]. Bound to the publisher that
/// created it — using a handle from a different publisher will panic.
#[derive(Clone, Copy)]
pub struct TopicHandle {
    topic_idx: u32,
    /// Unique ID of the publisher that created this handle.
    publisher_id: u64,
}

/// Zero-copy pub/sub publisher over shared memory.
///
/// Creates a ring buffer per topic. Data is written directly into mmap via
/// [`ShmLoan`] — no serialization, no headers, no URI per message.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// let mut pub_ = ShmPublisher::create("prices", PubSubConfig::default()).unwrap();
/// let handle = pub_.register("/tick/AAPL").unwrap();
///
/// let data = b"raw bytes - serialize however you want";
/// let mut loan = pub_.loan_to(&handle);
/// loan.set_data(data);
/// loan.publish();
/// ```
pub struct ShmPublisher {
    mmap: memmap2::MmapMut,
    config: PubSubConfig,
    path: PathBuf,
    /// (uri_hash, topic_idx) cache for O(1) lookup after registration.
    topics: Vec<(u64, u32)>,
    /// Tracks when heartbeat was last refreshed, for amortizing the update
    /// on the fast `loan_to` path.
    last_heartbeat: std::time::Instant,
    /// Background heartbeat thread — keeps the region connectable even when
    /// the publisher is idle (no loan/publish calls).
    _heartbeat_thread: Option<std::thread::JoinHandle<()>>,
    heartbeat_stop: Arc<std::sync::atomic::AtomicBool>,
    /// Unique ID for binding TopicHandles to this publisher instance.
    id: u64,
    /// Inode of the backing file at creation time. Used in Drop to avoid
    /// deleting a replacement publisher's file.
    created_ino: u64,
}

impl ShmPublisher {
    /// Creates a new pub/sub shared memory region.
    pub fn create(name: &str, config: PubSubConfig) -> Result<Self, CrossbarError> {
        if config.ring_depth == 0 {
            return Err(CrossbarError::ShmInvalidRegion(
                "ring_depth must be > 0".into(),
            ));
        }
        if config.sample_capacity == 0 {
            return Err(CrossbarError::ShmInvalidRegion(
                "sample_capacity must be > 0".into(),
            ));
        }
        if config.max_topics == 0 {
            return Err(CrossbarError::ShmInvalidRegion(
                "max_topics must be > 0".into(),
            ));
        }
        if config.heartbeat_interval.is_zero() {
            return Err(CrossbarError::ShmInvalidRegion(
                "heartbeat_interval must be > 0".into(),
            ));
        }
        if config.heartbeat_interval >= config.stale_timeout {
            return Err(CrossbarError::ShmInvalidRegion(
                "heartbeat_interval must be less than stale_timeout".into(),
            ));
        }
        if checked_total_size(&config).is_none() {
            return Err(CrossbarError::ShmInvalidRegion(
                "layout size overflows (max_topics * ring_depth * sample_capacity too large)"
                    .into(),
            ));
        }
        let path = shm_path(name);
        let size = total_size(&config);

        // Serialize region creation with flock. Without this, two concurrent
        // create() calls could both pass the freshness check, both unlink,
        // and produce split publishers serving different inodes.
        // Append ".lock" rather than using with_extension(), which replaces
        // the extension and would alias "foo.lock" with its own data file.
        let lock_path = PathBuf::from(format!("{}.lock", path.display()));
        let lock_file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| CrossbarError::ShmInvalidRegion(format!("lock file: {e}")))?;
        let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(CrossbarError::ShmInvalidRegion(format!(
                "flock: {}",
                io::Error::last_os_error()
            )));
        }

        // Check if an active publisher already owns this name. If so, refuse
        // to create — otherwise we'd split subscribers across two regions.
        if path.exists() {
            if let Ok(file) = std::fs::OpenOptions::new().read(true).open(&path) {
                if let Ok(mmap) = unsafe { memmap2::Mmap::map(&file) } {
                    if mmap.len() >= GLOBAL_HEADER_SIZE
                        && &mmap[0..8] == MAGIC
                        && u32::from_le_bytes(mmap[GH_VERSION..GH_VERSION + 4].try_into().unwrap())
                            == VERSION
                    {
                        let hb_atom =
                            unsafe { &*(mmap.as_ptr().add(GH_HEARTBEAT) as *const AtomicU64) };
                        let hb = hb_atom.load(Ordering::Acquire);
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_micros() as u64;

                        // Use the EXISTING region's stale timeout, not our config's.
                        // A live publisher with a longer heartbeat_interval must not
                        // be mistaken for stale by a new publisher with a shorter timeout.
                        let existing_stale_us = u64::from_le_bytes(
                            mmap[GH_STALE_TIMEOUT_US..GH_STALE_TIMEOUT_US + 8]
                                .try_into()
                                .unwrap_or([0; 8]),
                        );
                        // Heartbeat and now are both in microseconds.
                        // Fall back to our config timeout if the existing region
                        // has no stored timeout (legacy or zeroed).
                        let stale_us = if existing_stale_us > 0 {
                            existing_stale_us
                        } else {
                            config.stale_timeout.as_micros() as u64
                        };

                        if now.saturating_sub(hb) <= stale_us {
                            // Release lock before returning error
                            drop(lock_file);
                            return Err(CrossbarError::ShmInvalidRegion(format!(
                                "pub/sub region '{name}' is already active (heartbeat is fresh)"
                            )));
                        }
                    }
                }
            }
        }

        // Create the file under a temp name so subscribers never see a
        // partially-initialized header. After the header is fully written,
        // we rename it to the real path atomically.
        let tmp_path = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));

        // Unlink any stale file so existing subscribers mapping the old
        // inode aren't corrupted by us truncating/reinitializing their memory.
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&tmp_path);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(|e| CrossbarError::ShmInvalidRegion(format!("create: {e}")))?;

        file.set_len(size as u64)
            .map_err(|e| CrossbarError::ShmInvalidRegion(format!("set_len: {e}")))?;

        // Pre-set inode; updated after rename below.
        let created_ino = file.metadata().map(|m| m.ino()).unwrap_or(0);

        let mmap = unsafe {
            memmap2::MmapOptions::new()
                .len(size)
                .map_mut(&file)
                .map_err(|e| CrossbarError::ShmInvalidRegion(format!("mmap: {e}")))?
        };

        let heartbeat_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Unique publisher ID: monotonic clock XOR mmap address. Uses an
        // atomic counter as primary source — even if two publishers map at
        // the same virtual address in the same nanosecond, the counter
        // ensures distinct IDs.
        static ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let pub_id = ID_COUNTER.fetch_add(1, Ordering::Relaxed)
            ^ (mmap.as_ptr() as u64)
            ^ (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64);

        let mut pub_ = Self {
            mmap,
            config,
            path,
            topics: Vec::new(),
            last_heartbeat: std::time::Instant::now(),
            _heartbeat_thread: None,
            heartbeat_stop: Arc::clone(&heartbeat_stop),
            id: pub_id,
            created_ino,
        };

        // Write global header
        pub_.mmap[GH_MAGIC..GH_MAGIC + 8].copy_from_slice(MAGIC);
        pub_.mmap[GH_VERSION..GH_VERSION + 4].copy_from_slice(&VERSION.to_le_bytes());
        pub_.mmap[GH_MAX_TOPICS..GH_MAX_TOPICS + 4]
            .copy_from_slice(&pub_.config.max_topics.to_le_bytes());
        pub_.mmap[GH_SAMPLE_CAPACITY..GH_SAMPLE_CAPACITY + 4]
            .copy_from_slice(&pub_.config.sample_capacity.to_le_bytes());
        pub_.mmap[GH_RING_DEPTH..GH_RING_DEPTH + 4]
            .copy_from_slice(&pub_.config.ring_depth.to_le_bytes());

        let pid = std::process::id() as u64;
        pub_.mmap[GH_PID..GH_PID + 8].copy_from_slice(&pid.to_le_bytes());

        let stale_us = pub_.config.stale_timeout.as_micros() as u64;
        pub_.mmap[GH_STALE_TIMEOUT_US..GH_STALE_TIMEOUT_US + 8]
            .copy_from_slice(&stale_us.to_le_bytes());

        pub_.update_heartbeat();

        // Atomically expose the fully-initialized region to subscribers.
        std::fs::rename(&tmp_path, &pub_.path)
            .map_err(|e| CrossbarError::ShmInvalidRegion(format!("rename: {e}")))?;

        // Update inode to match the renamed file.
        pub_.created_ino = std::fs::metadata(&pub_.path).map(|m| m.ino()).unwrap_or(0);

        // Lock released — the freshness check + unlink + create + init is
        // fully serialized. Future create() calls will see our fresh
        // heartbeat and back off.
        drop(lock_file);

        // Spawn background heartbeat thread so the region stays connectable
        // even when the publisher is idle (not calling loan/loan_to).
        let hb_path = pub_.path.clone();
        let hb_interval = pub_.config.heartbeat_interval;
        let hb_stop = Arc::clone(&heartbeat_stop);
        let hb_thread = std::thread::spawn(move || {
            // Open a read-only mmap for heartbeat updates only.
            let file = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&hb_path)
            {
                Ok(f) => f,
                Err(_) => return,
            };
            let mmap = match unsafe { memmap2::MmapMut::map_mut(&file) } {
                Ok(m) => m,
                Err(_) => return,
            };
            // Sleep until the next heartbeat deadline, in short increments
            // so Drop doesn't block for the full heartbeat_interval.
            let stop_check = Duration::from_millis(50).min(hb_interval);
            let atom = unsafe { &*(mmap.as_ptr().add(GH_HEARTBEAT) as *const AtomicU64) };
            let mut last_write = std::time::Instant::now();
            while !hb_stop.load(std::sync::atomic::Ordering::Relaxed) {
                let remaining = hb_interval.saturating_sub(last_write.elapsed());
                if remaining.is_zero() {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_micros() as u64;
                    atom.store(now, Ordering::Release);
                    last_write = std::time::Instant::now();
                } else {
                    std::thread::sleep(remaining.min(stop_check));
                }
            }
        });
        pub_._heartbeat_thread = Some(hb_thread);

        Ok(pub_)
    }

    /// Updates the heartbeat timestamp.
    pub fn update_heartbeat(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        let atom = unsafe { &*(self.mmap.as_ptr().add(GH_HEARTBEAT) as *const AtomicU64) };
        atom.store(now, Ordering::Release);
    }

    /// Registers a topic. Call once per URI before publishing.
    /// Returns a [`TopicHandle`] for fast-path publishing via [`loan_to`](Self::loan_to).
    pub fn register(&mut self, uri: &str) -> Result<TopicHandle, CrossbarError> {
        if uri.len() > MAX_URI_LEN {
            return Err(CrossbarError::ShmInvalidRegion(format!(
                "URI too long ({} > {MAX_URI_LEN})",
                uri.len()
            )));
        }

        let hash = uri_hash(uri);

        // Check if already registered — compare hash AND full URI string
        // to guard against FNV-1a collisions.
        for &(h, idx) in &self.topics {
            if h == hash {
                let off = topic_entry_off(idx);
                let stored_len = self.mmap[off + TE_URI_LEN] as usize;
                let stored_uri = &self.mmap[off + TE_URI_STR..off + TE_URI_STR + stored_len];
                if stored_uri == uri.as_bytes() {
                    return Ok(TopicHandle {
                        topic_idx: idx,
                        publisher_id: self.id,
                    });
                }
            }
        }

        for idx in 0..self.config.max_topics {
            let off = topic_entry_off(idx);
            let active = unsafe { &*(self.mmap.as_ptr().add(off + TE_ACTIVE) as *const AtomicU32) };

            if active.load(Ordering::Acquire) == 0 {
                // Write topic metadata
                self.mmap[off + TE_URI_HASH..off + TE_URI_HASH + 8]
                    .copy_from_slice(&hash.to_le_bytes());
                self.mmap[off + TE_URI_LEN] = uri.len() as u8;
                self.mmap[off + TE_URI_STR..off + TE_URI_STR + uri.len()]
                    .copy_from_slice(uri.as_bytes());

                // Zero write_seq
                let ws =
                    unsafe { &*(self.mmap.as_ptr().add(off + TE_WRITE_SEQ) as *const AtomicU64) };
                ws.store(0, Ordering::Release);

                // Mark active (release: all writes above visible to subscribers)
                active.store(1, Ordering::Release);

                self.topics.push((hash, idx));
                return Ok(TopicHandle {
                    topic_idx: idx,
                    publisher_id: self.id,
                });
            }
        }

        Err(CrossbarError::ShmSlotsFull)
    }

    /// Returns a zero-copy loan by URI string (convenience method).
    ///
    /// For the hot path, prefer [`loan_to`](Self::loan_to) with a cached
    /// [`TopicHandle`] — it skips the hash lookup and heartbeat update.
    pub fn loan(&mut self, uri: &str) -> Result<ShmLoan<'_>, CrossbarError> {
        self.update_heartbeat();

        // Match hash AND full URI to prevent FNV-1a collision aliasing,
        // consistent with register() and subscribe().
        let hash = uri_hash(uri);
        let topic_idx = self
            .topics
            .iter()
            .filter(|(h, _)| *h == hash)
            .find(|(_, idx)| {
                let off = topic_entry_off(*idx);
                let stored_len = self.mmap[off + TE_URI_LEN] as usize;
                let stored_uri = &self.mmap[off + TE_URI_STR..off + TE_URI_STR + stored_len];
                stored_uri == uri.as_bytes()
            })
            .map(|(_, i)| *i)
            .ok_or_else(|| {
                CrossbarError::ShmInvalidRegion(format!("topic not registered: {uri}"))
            })?;

        self.loan_inner(topic_idx)
    }

    /// Fast-path loan: no hash lookup, no syscall on the hot path.
    ///
    /// Use the [`TopicHandle`] returned by [`register`](Self::register).
    /// This is the iceoryx-equivalent hot path — heartbeat is refreshed
    /// at most once per `heartbeat_interval` (default 100 ms), so most
    /// calls do zero syscalls.
    #[inline]
    pub fn loan_to(&mut self, handle: &TopicHandle) -> ShmLoan<'_> {
        assert_eq!(
            handle.publisher_id, self.id,
            "TopicHandle belongs to a different ShmPublisher"
        );
        // Amortized heartbeat: one Instant::now() comparison (~20 ns) per call,
        // but the actual SystemTime write only fires once per interval.
        if self.last_heartbeat.elapsed() >= self.config.heartbeat_interval {
            self.update_heartbeat();
            self.last_heartbeat = std::time::Instant::now();
        }
        // SAFETY: TopicHandle is only created by register(), which validates the index.
        self.loan_inner(handle.topic_idx)
            .expect("TopicHandle refers to invalid topic")
    }

    fn loan_inner(&mut self, topic_idx: u32) -> Result<ShmLoan<'_>, CrossbarError> {
        let off = topic_entry_off(topic_idx);

        let write_seq_atom =
            unsafe { &*(self.mmap.as_ptr().add(off + TE_WRITE_SEQ) as *const AtomicU64) };
        let next_seq = write_seq_atom.load(Ordering::Acquire) + 1;
        let slot = (next_seq % self.config.ring_depth as u64) as u32;

        let s_off = sample_offset(&self.config, topic_idx, slot);

        let data_ptr = unsafe { self.mmap.as_mut_ptr().add(s_off + SAMPLE_HEADER_SIZE) };

        let notify_atom =
            unsafe { &*(self.mmap.as_ptr().add(off + TE_NOTIFY) as *const AtomicU32) };

        Ok(ShmLoan {
            data_ptr,
            capacity: self.config.sample_capacity as usize,
            len: 0,
            seq: next_seq,
            sample_base: unsafe { self.mmap.as_mut_ptr().add(s_off) },
            write_seq_atom,
            notify_atom,
        })
    }
}

impl Drop for ShmPublisher {
    fn drop(&mut self) {
        // Signal heartbeat thread to stop and wait for it.
        self.heartbeat_stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self._heartbeat_thread.take() {
            let _ = t.join();
        }
        // Only remove the backing file if it's still the one we created.
        // A replacement publisher may have unlinked our file and created a new one
        // at the same path — in that case, deleting would destroy their live region.
        if let Ok(meta) = std::fs::metadata(&self.path) {
            if meta.ino() == self.created_ino {
                let _ = std::fs::remove_file(&self.path);
                // Note: we intentionally do NOT remove the .lock sidecar.
                // Removing it while another process holds flock() on the same
                // inode allows a third creator to recreate the file and acquire
                // a lock on a different inode, breaking serialization.
            }
        }
    }
}

// ─── ShmLoan ─────────────────────────────────────────────────────────────

/// A mutable view into a ring buffer slot in shared memory.
///
/// Write your raw bytes (any format — the transport doesn't care), then
/// call [`publish`](ShmLoan::publish) to make the sample visible to
/// subscribers. This is the iceoryx-style "loan" pattern: data is born
/// in shared memory, never on the heap.
pub struct ShmLoan<'a> {
    data_ptr: *mut u8,
    capacity: usize,
    len: usize,
    seq: u64,
    sample_base: *mut u8,
    write_seq_atom: &'a AtomicU64,
    notify_atom: &'a AtomicU32,
}

impl<'a> ShmLoan<'a> {
    /// Returns the writable region as a mutable slice. You can write
    /// directly into this slice for structs, `rkyv`, `memcpy`, etc.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.data_ptr, self.capacity) }
    }

    /// Copies `data` into the loan starting at offset 0, replacing any
    /// previous content. Panics if `data` exceeds capacity.
    ///
    /// For incremental/appending writes, use the [`io::Write`] trait instead.
    pub fn set_data(&mut self, data: &[u8]) {
        assert!(
            data.len() <= self.capacity,
            "data ({}) exceeds sample capacity ({})",
            data.len(),
            self.capacity
        );
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), self.data_ptr, data.len());
        }
        self.len = data.len();
    }

    /// Sets the valid data length (use after writing via `as_mut_slice`).
    pub fn set_len(&mut self, len: usize) {
        assert!(len <= self.capacity);
        self.len = len;
    }

    /// Maximum bytes this loan can hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Publishes the sample and wakes any blocked subscribers.
    ///
    /// Both polling ([`try_recv`](ShmSubscription::try_recv)) and blocking
    /// ([`recv`](ShmSubscription::recv)) subscribers will see the new data
    /// immediately. The futex wake adds ~50-100 ns.
    ///
    /// For the absolute lowest latency on polling-only workloads, use
    /// [`publish_silent`](Self::publish_silent) to skip the wake syscall.
    #[inline]
    pub fn publish(self) {
        self.commit(true);
    }

    /// Publishes without waking blocked subscribers. Saves ~50-100 ns
    /// by skipping the futex syscall, but subscribers using
    /// [`recv`](ShmSubscription::recv) will only see the data on their
    /// next 50 ms poll timeout. Only use this when all subscribers poll
    /// via [`try_recv`](ShmSubscription::try_recv).
    #[inline]
    pub fn publish_silent(self) {
        self.commit(false);
    }

    fn commit(self, wake: bool) {
        // Invalidate the slot's seq BEFORE writing data_len. This prevents
        // a lagging subscriber from reading partially-overwritten bytes:
        // its seqlock check will see 0 (no valid seq starts at 0) and
        // return None instead of corrupted data.
        let sample_seq = unsafe { &*(self.sample_base.add(SS_SEQ) as *const AtomicU64) };
        sample_seq.store(0, Ordering::Release);

        // Write data_len into sample header
        unsafe {
            (self.sample_base.add(SS_DATA_LEN) as *mut u32).write(self.len as u32);
        }

        // Write seq into sample header (Release: data visible before seq)
        sample_seq.store(self.seq, Ordering::Release);

        // Bump topic write_seq (Release: sample fully committed)
        self.write_seq_atom.store(self.seq, Ordering::Release);

        if wake {
            self.notify_atom.fetch_add(1, Ordering::Release);
            notify::wake_all(self.notify_atom);
        }
    }
}

impl<'a> io::Write for ShmLoan<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let remaining = self.capacity - self.len;
        if remaining == 0 && !buf.is_empty() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "sample full"));
        }
        let n = buf.len().min(remaining);
        unsafe {
            std::ptr::copy_nonoverlapping(buf.as_ptr(), self.data_ptr.add(self.len), n);
        }
        self.len += n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ─── ShmSubscriber ───────────────────────────────────────────────────────

/// Zero-copy pub/sub subscriber over shared memory.
///
/// Connects to an existing [`ShmPublisher`] region. Samples are read
/// directly from mmap — no copies, no allocations, no deserialization.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// let sub = ShmSubscriber::connect("prices").unwrap();
/// let mut stream = sub.subscribe("/tick/AAPL").unwrap();
///
/// loop {
///     let sample = stream.recv().unwrap();
///     // sample is &[u8] pointing directly into shared memory
///     println!("got {} bytes", sample.len());
/// }
/// ```
pub struct ShmSubscriber {
    mmap: Arc<memmap2::Mmap>,
    config: PubSubConfig,
}

impl ShmSubscriber {
    /// Connects to an existing pub/sub region created by [`ShmPublisher`].
    ///
    /// Uses the publisher's stored stale timeout (from the header) so
    /// that non-default publisher configurations are honored automatically.
    /// Falls back to 5s if the header has no stored timeout.
    pub fn connect(name: &str) -> Result<Self, CrossbarError> {
        Self::connect_inner(name, None)
    }

    /// Connects with a caller-specified stale timeout. This timeout is
    /// used as-is for both initial validation and subsequent heartbeat
    /// checks, enabling fast failover when the caller needs it.
    ///
    /// **Important:** the timeout should be >= the publisher's
    /// `heartbeat_interval` (default 100ms). A shorter value will cause
    /// spurious `ShmServerDead` errors because the publisher cannot write
    /// heartbeats faster than its configured interval.
    pub fn connect_with_timeout(
        name: &str,
        stale_timeout: Duration,
    ) -> Result<Self, CrossbarError> {
        Self::connect_inner(name, Some(stale_timeout))
    }

    fn connect_inner(name: &str, caller_timeout: Option<Duration>) -> Result<Self, CrossbarError> {
        let path = shm_path(name);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .map_err(|e| CrossbarError::ShmInvalidRegion(format!("open: {e}")))?;

        let mmap = unsafe {
            memmap2::Mmap::map(&file)
                .map_err(|e| CrossbarError::ShmInvalidRegion(format!("mmap: {e}")))?
        };

        // Validate header
        if mmap.len() < GLOBAL_HEADER_SIZE || &mmap[0..8] != MAGIC {
            return Err(CrossbarError::ShmInvalidRegion(
                "magic mismatch (not a crossbar pub/sub region)".into(),
            ));
        }
        let version = u32::from_le_bytes(mmap[GH_VERSION..GH_VERSION + 4].try_into().unwrap());
        if version != VERSION {
            return Err(CrossbarError::ShmInvalidRegion(format!(
                "version mismatch: expected {VERSION}, got {version}"
            )));
        }

        let max_topics =
            u32::from_le_bytes(mmap[GH_MAX_TOPICS..GH_MAX_TOPICS + 4].try_into().unwrap());
        let sample_capacity = u32::from_le_bytes(
            mmap[GH_SAMPLE_CAPACITY..GH_SAMPLE_CAPACITY + 4]
                .try_into()
                .unwrap(),
        );
        let ring_depth =
            u32::from_le_bytes(mmap[GH_RING_DEPTH..GH_RING_DEPTH + 4].try_into().unwrap());
        if ring_depth == 0 {
            return Err(CrossbarError::ShmInvalidRegion(
                "on-disk ring_depth is 0 (corrupted metadata)".into(),
            ));
        }

        // Determine the effective stale timeout:
        // - If the caller specified one (connect_with_timeout), use it as-is.
        // - Otherwise, read the publisher's stored timeout from the header,
        //   falling back to 5s if the header has no stored value.
        let effective_stale = if let Some(t) = caller_timeout {
            t
        } else {
            let stored_stale_us = u64::from_le_bytes(
                mmap[GH_STALE_TIMEOUT_US..GH_STALE_TIMEOUT_US + 8]
                    .try_into()
                    .unwrap_or([0; 8]),
            );
            if stored_stale_us > 0 {
                Duration::from_micros(stored_stale_us)
            } else {
                Duration::from_secs(5)
            }
        };

        let config = PubSubConfig {
            max_topics,
            sample_capacity,
            ring_depth,
            heartbeat_interval: Duration::from_millis(100),
            stale_timeout: effective_stale,
        };

        // Validate that the on-disk metadata doesn't overflow the layout calculation.
        let expected_size = match checked_total_size(&config) {
            Some(s) => s,
            None => {
                return Err(CrossbarError::ShmInvalidRegion(
                    "on-disk metadata overflows layout size (corrupted region)".into(),
                ));
            }
        };
        if mmap.len() < expected_size {
            return Err(CrossbarError::ShmInvalidRegion(format!(
                "region too small: {} bytes, expected at least {expected_size}",
                mmap.len()
            )));
        }

        let sub = Self {
            mmap: Arc::new(mmap),
            config,
        };
        sub.check_heartbeat(effective_stale)?;
        Ok(sub)
    }

    fn check_heartbeat(&self, stale_timeout: Duration) -> Result<(), CrossbarError> {
        let atom = unsafe { &*(self.mmap.as_ptr().add(GH_HEARTBEAT) as *const AtomicU64) };
        let hb = atom.load(Ordering::Acquire);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;

        if now.saturating_sub(hb) > stale_timeout.as_micros() as u64 {
            return Err(CrossbarError::ShmServerDead);
        }
        Ok(())
    }

    /// Subscribes to a topic. Returns a [`ShmSubscription`] that yields
    /// zero-copy samples.
    pub fn subscribe(&self, uri: &str) -> Result<ShmSubscription, CrossbarError> {
        let hash = uri_hash(uri);

        for idx in 0..self.config.max_topics {
            let off = topic_entry_off(idx);
            let active = unsafe { &*(self.mmap.as_ptr().add(off + TE_ACTIVE) as *const AtomicU32) };

            if active.load(Ordering::Acquire) == 0 {
                continue;
            }

            let stored = u64::from_le_bytes(
                self.mmap[off + TE_URI_HASH..off + TE_URI_HASH + 8]
                    .try_into()
                    .unwrap(),
            );

            if stored == hash {
                // Full URI comparison to guard against hash collisions.
                let stored_len = self.mmap[off + TE_URI_LEN] as usize;
                if stored_len > MAX_URI_LEN {
                    return Err(CrossbarError::ShmInvalidRegion(format!(
                        "topic {idx} has invalid URI length {stored_len} (max {MAX_URI_LEN})"
                    )));
                }
                let stored_uri = &self.mmap[off + TE_URI_STR..off + TE_URI_STR + stored_len];
                if stored_uri != uri.as_bytes() {
                    continue; // hash collision, keep searching
                }

                let ws =
                    unsafe { &*(self.mmap.as_ptr().add(off + TE_WRITE_SEQ) as *const AtomicU64) };
                let current = ws.load(Ordering::Acquire);

                return Ok(ShmSubscription {
                    mmap: Arc::clone(&self.mmap),
                    config: self.config.clone(),
                    topic_idx: idx,
                    last_seq: current, // start from now (no replay)
                    inflight_seq: None,
                });
            }
        }

        Err(CrossbarError::ShmInvalidRegion(format!(
            "topic not found: {uri}"
        )))
    }
}

// ─── ShmSubscription ─────────────────────────────────────────────────────

/// A subscription to one pub/sub topic. Yields zero-copy [`ShmSample`]s.
pub struct ShmSubscription {
    mmap: Arc<memmap2::Mmap>,
    config: PubSubConfig,
    topic_idx: u32,
    last_seq: u64,
    /// If a `recv_async` was cancelled, the detached blocking worker may
    /// still update this `AtomicU64`. We sync from it on the next call.
    inflight_seq: Option<Arc<AtomicU64>>,
}

impl ShmSubscription {
    /// Sync progress from a previously cancelled `recv_async`.
    fn sync_inflight(&mut self) {
        if let Some(seq) = self.inflight_seq.take() {
            let updated = seq.load(Ordering::Acquire);
            if updated > self.last_seq {
                self.last_seq = updated;
            }
        }
    }

    fn write_seq_atom(&self) -> &AtomicU64 {
        let off = topic_entry_off(self.topic_idx);
        unsafe { &*(self.mmap.as_ptr().add(off + TE_WRITE_SEQ) as *const AtomicU64) }
    }

    fn notify_atom(&self) -> &AtomicU32 {
        let off = topic_entry_off(self.topic_idx);
        unsafe { &*(self.mmap.as_ptr().add(off + TE_NOTIFY) as *const AtomicU32) }
    }

    /// Non-blocking: returns the next sample or `None`.
    ///
    /// Copies the data within the seqlock-checked window so the returned
    /// `ShmSample` is guaranteed to be consistent even if the publisher
    /// wraps the ring immediately after this call.
    pub fn try_recv(&mut self) -> Option<ShmSample> {
        self.sync_inflight();
        let (data, _seq) = self.next_sample_copy(None)?;
        Some(ShmSample { data: data.into() })
    }

    /// Zero-allocation non-blocking recv. Returns a borrowed reference
    /// directly into mmap — no `Arc::clone`, no heap, pure pointer.
    /// This is the iceoryx-equivalent read path.
    ///
    /// **Caveat:** because this returns a raw mmap pointer, a fast publisher
    /// can overwrite the underlying ring slot. Process data immediately
    /// or use [`try_recv`](Self::try_recv) for guaranteed consistency.
    #[inline]
    pub fn try_recv_ref(&mut self) -> Option<ShmSampleRef<'_>> {
        self.sync_inflight();
        let (s_off, data_len) = self.next_sample_inner()?;
        Some(ShmSampleRef {
            mmap: &self.mmap,
            offset: s_off + SAMPLE_HEADER_SIZE,
            len: data_len,
        })
    }

    fn next_sample_inner(&mut self) -> Option<(usize, usize)> {
        let current_seq = self.write_seq_atom().load(Ordering::Acquire);
        if current_seq <= self.last_seq {
            return None;
        }

        let target = if current_seq - self.last_seq > self.config.ring_depth as u64 {
            current_seq // fell behind — skip to latest
        } else {
            self.last_seq + 1
        };

        if let Some(result) = self.read_sample_inner(target) {
            return Some(result);
        }

        // Slot was overwritten — retry with the latest seq.
        let latest = self.write_seq_atom().load(Ordering::Acquire);
        if latest > self.last_seq && latest != target {
            self.read_sample_inner(latest)
        } else {
            None
        }
    }

    /// Like next_sample_inner, but copies data within the seqlock window.
    /// If the target slot was overwritten (torn read), retries once with
    /// the latest write_seq to avoid returning None when data is available.
    fn next_sample_copy(&mut self, progress: Option<&AtomicU64>) -> Option<(Vec<u8>, u64)> {
        let current_seq = self.write_seq_atom().load(Ordering::Acquire);
        if current_seq <= self.last_seq {
            return None;
        }

        let target = if current_seq - self.last_seq > self.config.ring_depth as u64 {
            current_seq
        } else {
            self.last_seq + 1
        };

        if let Some(result) = self.read_sample_copy(target, progress) {
            return Some(result);
        }

        // Slot was overwritten during read — retry with the latest seq.
        let latest = self.write_seq_atom().load(Ordering::Acquire);
        if latest > self.last_seq && latest != target {
            self.read_sample_copy(latest, progress)
        } else {
            None
        }
    }

    /// Blocking: waits for the next sample. Uses spin → yield → futex.
    ///
    /// Returns `Err(ShmServerDead)` if the publisher's heartbeat goes stale
    /// while waiting.
    pub fn recv(&mut self) -> Result<ShmSample, CrossbarError> {
        // Scale poll interval to stale_timeout so short timeouts are honored.
        // Minimum 1ms (below that, spin_loop in notify is more appropriate).
        let poll_ms = (self.config.stale_timeout.as_millis() as u64 / 3).clamp(1, 50);

        loop {
            if let Some(s) = self.try_recv() {
                return Ok(s);
            }

            // Snapshot the notify counter AFTER checking for data.
            let cur = self.notify_atom().load(Ordering::Acquire);

            // Re-check after loading counter to close the race window:
            // if a publish happened between try_recv and the load above,
            // the data is ready but the counter may match.
            if let Some(s) = self.try_recv() {
                return Ok(s);
            }

            notify::wait_until_not(self.notify_atom(), cur, Duration::from_millis(poll_ms)).ok();

            // Check heartbeat every iteration — the cost (~50 ns) is
            // negligible vs the poll wait, and it ensures short stale
            // timeouts are honored promptly.
            self.check_publisher_heartbeat()?;
        }
    }

    /// Checks whether the publisher is still alive by reading GH_HEARTBEAT.
    fn check_publisher_heartbeat(&self) -> Result<(), CrossbarError> {
        let atom = unsafe { &*(self.mmap.as_ptr().add(GH_HEARTBEAT) as *const AtomicU64) };
        let hb = atom.load(Ordering::Acquire);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        if now.saturating_sub(hb) > self.config.stale_timeout.as_micros() as u64 {
            return Err(CrossbarError::ShmServerDead);
        }
        Ok(())
    }

    /// Blocking recv with cancellation flag. Returns `Err(ShmServerDead)`
    /// when `cancel` is set to `true` or the publisher heartbeat goes stale.
    ///
    /// If `progress` is provided, `self.last_seq` is published to it
    /// immediately when a sample is consumed, before returning. This
    /// eliminates the race window in `recv_async` cancellation.
    fn recv_cancellable(
        &mut self,
        cancel: &std::sync::atomic::AtomicBool,
        progress: Option<&AtomicU64>,
    ) -> Result<ShmSample, CrossbarError> {
        let poll_ms = (self.config.stale_timeout.as_millis() as u64 / 3).clamp(1, 50);

        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(CrossbarError::ShmServerDead);
            }
            if let Some((data, _seq)) = self.next_sample_copy(progress) {
                return Ok(ShmSample { data: data.into() });
            }
            let cur = self.notify_atom().load(Ordering::Acquire);
            if let Some((data, _seq)) = self.next_sample_copy(progress) {
                return Ok(ShmSample { data: data.into() });
            }
            notify::wait_until_not(self.notify_atom(), cur, Duration::from_millis(poll_ms)).ok();
            self.check_publisher_heartbeat()?;
        }
    }

    /// Async-friendly recv. Runs the blocking wait on a Tokio blocking
    /// thread. Works on both multi-thread and current-thread runtimes.
    ///
    /// When the returned future is cancelled (e.g. via `select!` or
    /// `timeout`), a cancellation flag is set on drop, causing the
    /// blocking task to terminate within one poll interval (~50 ms max).
    /// Progress is recovered on the next call via `inflight_seq`.
    pub async fn recv_async(&mut self) -> Result<ShmSample, CrossbarError> {
        // Recover progress from a previously cancelled recv_async.
        self.sync_inflight();

        let mmap = Arc::clone(&self.mmap);
        let config = self.config.clone();
        let topic_idx = self.topic_idx;
        let shared_seq = Arc::new(AtomicU64::new(self.last_seq));
        let shared_seq_clone = Arc::clone(&shared_seq);
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_clone = Arc::clone(&cancel);

        // Store shared_seq so a future cancellation can recover progress
        // on the next call, instead of racing with the worker in Drop.
        self.inflight_seq = Some(Arc::clone(&shared_seq));

        // Signal cancellation on drop (future cancelled).
        struct CancelSignal {
            cancel: Arc<std::sync::atomic::AtomicBool>,
        }
        impl Drop for CancelSignal {
            fn drop(&mut self) {
                self.cancel.store(true, Ordering::Release);
            }
        }
        let _signal = CancelSignal { cancel };

        let sample = tokio::task::spawn_blocking(move || {
            let mut sub = ShmSubscription {
                mmap,
                config,
                topic_idx,
                last_seq: shared_seq_clone.load(Ordering::Relaxed),
                inflight_seq: None,
            };
            let s = sub.recv_cancellable(&cancel_clone, Some(&shared_seq_clone))?;
            Ok::<_, CrossbarError>(s)
        })
        .await
        .map_err(|_| CrossbarError::ShmServerDead)??;

        // Normal completion: sync progress and clear inflight.
        self.sync_inflight();
        Ok(sample)
    }

    /// Returns the most recent sample, skipping anything missed.
    pub fn latest(&mut self) -> Option<ShmSample> {
        self.sync_inflight();
        let seq = self.write_seq_atom().load(Ordering::Acquire);
        if seq == 0 || seq <= self.last_seq {
            return None;
        }
        if let Some(s) = self.read_sample(seq) {
            return Some(s);
        }
        // Slot was overwritten during read — retry with refreshed seq.
        let latest = self.write_seq_atom().load(Ordering::Acquire);
        if latest > self.last_seq && latest != seq {
            self.read_sample(latest)
        } else {
            None
        }
    }

    fn read_sample(&mut self, seq: u64) -> Option<ShmSample> {
        let (data, _) = self.read_sample_copy(seq, None)?;
        Some(ShmSample { data: data.into() })
    }

    /// Copies data within the seqlock-checked window. Returns owned bytes
    /// that are guaranteed consistent (not torn by concurrent publisher writes).
    ///
    /// If `progress` is provided, `seq` is published to it atomically with
    /// the `self.last_seq` update. This is critical for `recv_async`
    /// cancellation safety — see `recv_cancellable`.
    fn read_sample_copy(
        &mut self,
        seq: u64,
        progress: Option<&AtomicU64>,
    ) -> Option<(Vec<u8>, u64)> {
        let slot = (seq % self.config.ring_depth as u64) as u32;
        let s_off = sample_offset(&self.config, self.topic_idx, slot);

        let sample_seq = unsafe { &*(self.mmap.as_ptr().add(s_off + SS_SEQ) as *const AtomicU64) };

        // First check: is this the seq we expect?
        if sample_seq.load(Ordering::Acquire) != seq {
            return None;
        }

        let data_len =
            unsafe { (self.mmap.as_ptr().add(s_off + SS_DATA_LEN) as *const u32).read() } as usize;

        if data_len > self.config.sample_capacity as usize {
            return None;
        }

        // Copy data while we're inside the seqlock window
        let data_start = s_off + SAMPLE_HEADER_SIZE;
        let data = self.mmap[data_start..data_start + data_len].to_vec();

        // Final seqlock check: if the slot was overwritten during our copy,
        // the data is inconsistent — discard it.
        if sample_seq.load(Ordering::Acquire) != seq {
            return None;
        }

        self.last_seq = seq;
        if let Some(p) = progress {
            p.store(seq, Ordering::Release);
        }
        Some((data, seq))
    }

    /// Returns (sample_byte_offset, data_len) or None if overwritten.
    ///
    /// Used by `try_recv_ref` which returns a raw mmap pointer. The caller
    /// accepts the risk that the slot may be overwritten after this returns.
    fn read_sample_inner(&mut self, seq: u64) -> Option<(usize, usize)> {
        let slot = (seq % self.config.ring_depth as u64) as u32;
        let s_off = sample_offset(&self.config, self.topic_idx, slot);

        let sample_seq = unsafe { &*(self.mmap.as_ptr().add(s_off + SS_SEQ) as *const AtomicU64) };

        if sample_seq.load(Ordering::Acquire) != seq {
            return None;
        }

        let data_len =
            unsafe { (self.mmap.as_ptr().add(s_off + SS_DATA_LEN) as *const u32).read() } as usize;

        if data_len > self.config.sample_capacity as usize {
            return None;
        }

        if sample_seq.load(Ordering::Acquire) != seq {
            return None;
        }

        self.last_seq = seq;
        Some((s_off, data_len))
    }
}

// ─── ShmSample ───────────────────────────────────────────────────────────

/// An owned snapshot of a published sample. Data is copied from shared
/// memory within the seqlock-checked window, so the bytes are guaranteed
/// consistent — no risk of torn reads from a concurrent publisher.
///
/// Parse however you like: `serde_json`, `sonic_rs`, `rkyv`, `bytemuck`,
/// or just read the raw bytes. The transport doesn't care.
pub struct ShmSample {
    data: Box<[u8]>,
}

impl ShmSample {
    /// Returns the raw byte length of the sample data.
    pub fn data_len(&self) -> usize {
        self.data.len()
    }

    /// Returns the sample data as an owned `Vec<u8>`.
    pub fn to_vec(&self) -> Vec<u8> {
        self.data.to_vec()
    }
}

impl AsRef<[u8]> for ShmSample {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl std::ops::Deref for ShmSample {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.data
    }
}

impl std::fmt::Debug for ShmSample {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShmSample")
            .field("len", &self.data.len())
            .finish()
    }
}

/// Borrowed zero-copy view into a sample. No `Arc::clone`, no heap —
/// just a pointer into mmap. Returned by [`ShmSubscription::try_recv_ref`].
///
/// The reference is valid as long as the [`ShmSubscription`] is alive
/// and no new `try_recv_ref` call has been made. Like [`ShmSample`],
/// the underlying ring slot can be overwritten by a fast publisher —
/// process data immediately for maximum correctness.
pub struct ShmSampleRef<'a> {
    mmap: &'a memmap2::Mmap,
    offset: usize,
    len: usize,
}

impl ShmSampleRef<'_> {
    /// Raw byte length of the sample data.
    pub fn data_len(&self) -> usize {
        self.len
    }
}

impl AsRef<[u8]> for ShmSampleRef<'_> {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.mmap[self.offset..self.offset + self.len]
    }
}

impl std::ops::Deref for ShmSampleRef<'_> {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}

impl std::fmt::Debug for ShmSampleRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShmSampleRef")
            .field("len", &self.len)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pubsub_roundtrip() {
        let name = &format!("test-ps-{}", std::process::id());
        let mut pub_ = ShmPublisher::create(name, PubSubConfig::default()).unwrap();
        let _handle = pub_.register("/tick/AAPL").unwrap();

        let sub = ShmSubscriber::connect(name).unwrap();
        let mut stream = sub.subscribe("/tick/AAPL").unwrap();

        // Nothing published yet
        assert!(stream.try_recv().is_none());

        // Publish a sample
        let mut loan = pub_.loan("/tick/AAPL").unwrap();
        loan.set_data(b"hello from shm");
        loan.publish();

        // Should receive it
        let sample = stream.try_recv().unwrap();
        assert_eq!(&*sample, b"hello from shm");

        // No more
        assert!(stream.try_recv().is_none());
    }

    #[test]
    fn pubsub_multiple_samples() {
        let name = &format!("test-ps-multi-{}", std::process::id());
        let mut pub_ = ShmPublisher::create(name, PubSubConfig::default()).unwrap();
        let _handle = pub_.register("/data").unwrap();

        let sub = ShmSubscriber::connect(name).unwrap();
        let mut stream = sub.subscribe("/data").unwrap();

        for i in 0u32..5 {
            let mut loan = pub_.loan("/data").unwrap();
            loan.set_data(&i.to_le_bytes());
            loan.publish();
        }

        // Read them back in order
        for i in 0u32..5 {
            let sample = stream.try_recv().unwrap();
            let val = u32::from_le_bytes(sample[..4].try_into().unwrap());
            assert_eq!(val, i);
        }
    }

    #[test]
    fn pubsub_io_write() {
        let name = &format!("test-ps-write-{}", std::process::id());
        let mut pub_ = ShmPublisher::create(name, PubSubConfig::default()).unwrap();
        let _handle = pub_.register("/json").unwrap();

        let sub = ShmSubscriber::connect(name).unwrap();
        let mut stream = sub.subscribe("/json").unwrap();

        // Use io::Write trait (like serde_json::to_writer would)
        let mut loan = pub_.loan("/json").unwrap();
        io::Write::write_all(&mut loan, b"{\"price\":150.25}").unwrap();
        loan.publish();

        let sample = stream.try_recv().unwrap();
        assert_eq!(std::str::from_utf8(&sample).unwrap(), "{\"price\":150.25}");
    }

    #[test]
    fn pubsub_multiple_topics() {
        let name = &format!("test-ps-topics-{}", std::process::id());
        let mut pub_ = ShmPublisher::create(name, PubSubConfig::default()).unwrap();
        let _h1 = pub_.register("/tick/AAPL").unwrap();
        let _h2 = pub_.register("/tick/MSFT").unwrap();

        let sub = ShmSubscriber::connect(name).unwrap();
        let mut aapl = sub.subscribe("/tick/AAPL").unwrap();
        let mut msft = sub.subscribe("/tick/MSFT").unwrap();

        let mut loan = pub_.loan("/tick/AAPL").unwrap();
        loan.set_data(b"AAPL");
        loan.publish();

        let mut loan = pub_.loan("/tick/MSFT").unwrap();
        loan.set_data(b"MSFT");
        loan.publish();

        assert_eq!(&*aapl.try_recv().unwrap(), b"AAPL");
        assert_eq!(&*msft.try_recv().unwrap(), b"MSFT");

        // Cross-topic isolation
        assert!(aapl.try_recv().is_none());
        assert!(msft.try_recv().is_none());
    }

    #[test]
    fn pubsub_ring_overwrite() {
        let cfg = PubSubConfig {
            ring_depth: 4,
            ..PubSubConfig::default()
        };
        let name = &format!("test-ps-ring-{}", std::process::id());
        let mut pub_ = ShmPublisher::create(name, cfg).unwrap();
        let _handle = pub_.register("/data").unwrap();

        let sub = ShmSubscriber::connect(name).unwrap();
        let mut stream = sub.subscribe("/data").unwrap();

        // Publish 10 samples into a ring of depth 4 — subscriber should skip ahead
        for i in 0u32..10 {
            let mut loan = pub_.loan("/data").unwrap();
            loan.set_data(&i.to_le_bytes());
            loan.publish();
        }

        // Subscriber fell behind: first recv skips to latest available
        let sample = stream.try_recv().unwrap();
        let val = u32::from_le_bytes(sample[..4].try_into().unwrap());
        // Should be one of the recent values (ring wraps, so ≥ 6)
        assert!(val >= 6, "expected recent value, got {val}");
    }

    #[test]
    fn pubsub_latest() {
        let name = &format!("test-ps-latest-{}", std::process::id());
        let mut pub_ = ShmPublisher::create(name, PubSubConfig::default()).unwrap();
        let _handle = pub_.register("/data").unwrap();

        let sub = ShmSubscriber::connect(name).unwrap();
        let mut stream = sub.subscribe("/data").unwrap();

        for i in 0u32..5 {
            let mut loan = pub_.loan("/data").unwrap();
            loan.set_data(&i.to_le_bytes());
            loan.publish();
        }

        // latest() should return the most recent, skipping intermediate
        let sample = stream.latest().unwrap();
        let val = u32::from_le_bytes(sample[..4].try_into().unwrap());
        assert_eq!(val, 4);
    }
}
