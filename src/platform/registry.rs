// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! Global topic registry for service discovery.
//!
//! Publishers automatically register their topics. Subscribers can
//! discover topics by URI pattern without knowing region names.
//!
//! The registry is a fixed-size shared memory file at `/dev/shm/crossbar-registry`
//! (Linux), `$TMPDIR/crossbar-registry` (macOS/Windows) with atomic access.
//! The path can be overridden via the `CROSSBAR_REGISTRY` environment variable.
//!
//! ## Layout
//!
//! ```text
//! Header (64 bytes):
//!   magic: [u8; 8] = "XREG_ZC\0"
//!   version: u32
//!   entry_count: AtomicU32
//!   max_entries: u32 = 256
//!   _padding
//!
//! Entry (128 bytes each):
//!   active: AtomicU32       // 0 = free, 1 = active
//!   pid: u32                // publisher PID
//!   timestamp: u64          // last heartbeat (micros since epoch)
//!   region_name: [u8; 48]   // null-terminated
//!   topic_uri: [u8; 64]     // null-terminated
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use super::mmap::RawMmap;

// ---- Registry layout constants ----

const REG_MAGIC: &[u8; 8] = b"XREG_ZC\0";
const REG_VERSION: u32 = 1;

const REG_HEADER_SIZE: usize = 64;
const REG_ENTRY_SIZE: usize = 128;
const REG_MAX_ENTRIES: u32 = 256;
const REG_TOTAL_SIZE: usize = REG_HEADER_SIZE + REG_MAX_ENTRIES as usize * REG_ENTRY_SIZE;

// Header offsets
const RH_MAGIC: usize = 0;
const RH_VERSION: usize = 8;
const RH_ENTRY_COUNT: usize = 12; // AtomicU32
const RH_MAX_ENTRIES: usize = 16;

// Entry offsets (relative to entry start)
const RE_ACTIVE: usize = 0; // AtomicU32
const RE_PID: usize = 4; // u32
const RE_TIMESTAMP: usize = 8; // u64
const RE_REGION_NAME: usize = 16; // [u8; 48]
const RE_REGION_NAME_MAX: usize = 48;
const RE_TOPIC_URI: usize = 64; // [u8; 64]
const RE_TOPIC_URI_MAX: usize = 64;

const RE_STATE_FREE: u32 = 0;
const RE_STATE_ACTIVE: u32 = 1;

/// A discovered topic from the global registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredTopic {
    /// The SHM region name (pass to `Subscriber::connect`).
    pub region: String,
    /// The topic URI.
    pub uri: String,
    /// The publisher's PID.
    pub pid: u32,
    /// Registration/heartbeat timestamp (microseconds since UNIX epoch).
    pub timestamp_us: u64,
}

/// Global topic registry for service discovery.
///
/// Publishers automatically register their topics. Subscribers can
/// discover topics by URI pattern without knowing region names.
pub struct Registry {
    _mmap: RawMmap,
    path: PathBuf,
}

fn registry_path() -> PathBuf {
    // Allow override via environment variable for custom deployments.
    if let Ok(p) = std::env::var("CROSSBAR_REGISTRY") {
        return PathBuf::from(p);
    }
    if cfg!(target_os = "linux") {
        PathBuf::from("/dev/shm/crossbar-registry")
    } else if cfg!(windows) {
        std::env::temp_dir().join("crossbar-registry")
    } else {
        PathBuf::from("/tmp/crossbar-shm-registry")
    }
}

/// Returns microseconds since UNIX epoch, or `None` if the clock is before epoch.
fn now_micros() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_micros() as u64)
}

/// Read a null-terminated string from a raw pointer with a maximum length.
///
/// # Safety
///
/// `ptr` must point to at least `max_len` readable bytes.
unsafe fn read_fixed_str(ptr: *const u8, max_len: usize) -> String {
    let mut len = 0;
    while len < max_len {
        if *ptr.add(len) == 0 {
            break;
        }
        len += 1;
    }
    let slice = core::slice::from_raw_parts(ptr, len);
    String::from_utf8_lossy(slice).into_owned()
}

/// Write a null-terminated string to a raw pointer with a maximum length.
///
/// # Safety
///
/// `ptr` must point to at least `max_len` writable bytes.
unsafe fn write_fixed_str(ptr: *mut u8, s: &str, max_len: usize) {
    let bytes = s.as_bytes();
    let copy_len = bytes.len().min(max_len - 1);
    core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, copy_len);
    // Null-terminate
    *ptr.add(copy_len) = 0;
    // Zero remaining bytes to avoid stale data
    if copy_len + 1 < max_len {
        core::ptr::write_bytes(ptr.add(copy_len + 1), 0, max_len - copy_len - 1);
    }
}

impl Registry {
    /// Open or create the global registry.
    ///
    /// The registry path is resolved in this order:
    /// 1. `CROSSBAR_REGISTRY` environment variable (if set)
    /// 2. Platform default (`/dev/shm/crossbar-registry` on Linux)
    ///
    /// If the registry file does not exist, it is created and initialized.
    /// If it exists but has invalid magic/version, it is re-created.
    pub fn open() -> Result<Self, crate::error::Error> {
        let path = registry_path();

        // Try to open existing first
        if let Ok(existing) = Self::try_open_existing(&path) {
            return Ok(existing);
        }

        // Create new registry
        Self::create_new(&path)
    }

    /// Open or create a registry at a custom path.
    ///
    /// Useful for testing or when multiple isolated registries are needed.
    pub fn open_at(path: PathBuf) -> Result<Self, crate::error::Error> {
        if let Ok(existing) = Self::try_open_existing(&path) {
            return Ok(existing);
        }
        Self::create_new(&path)
    }

    fn try_open_existing(path: &std::path::Path) -> Result<Self, crate::error::Error> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(crate::error::Error::Io)?;

        let meta = file.metadata().map_err(crate::error::Error::Io)?;
        if meta.len() < REG_TOTAL_SIZE as u64 {
            return Err(crate::error::Error::InvalidRegion(
                "registry file too small".into(),
            ));
        }

        let mmap =
            RawMmap::from_file_with_len(&file, REG_TOTAL_SIZE).map_err(crate::error::Error::Io)?;

        // Validate magic and version
        let ptr = mmap.as_ptr();
        unsafe {
            let mut magic = [0u8; 8];
            core::ptr::copy_nonoverlapping(ptr.add(RH_MAGIC), magic.as_mut_ptr(), 8);
            if &magic != REG_MAGIC {
                return Err(crate::error::Error::InvalidRegion(
                    "registry: invalid magic".into(),
                ));
            }
            let ver = (ptr.add(RH_VERSION) as *const u32).read();
            if ver != REG_VERSION {
                return Err(crate::error::Error::InvalidRegion(
                    "registry: unsupported version".into(),
                ));
            }
        }

        Ok(Registry {
            _mmap: mmap,
            path: path.to_path_buf(),
        })
    }

    fn create_new(path: &std::path::Path) -> Result<Self, crate::error::Error> {
        let file = {
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true).write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600); // Owner-only; cross-user discovery is opt-in
            }
            opts.open(path).map_err(crate::error::Error::Io)?
        };

        file.set_len(REG_TOTAL_SIZE as u64)
            .map_err(crate::error::Error::Io)?;

        let mmap =
            RawMmap::from_file_with_len(&file, REG_TOTAL_SIZE).map_err(crate::error::Error::Io)?;

        // Write header
        let ptr = mmap.as_mut_ptr();
        unsafe {
            core::ptr::copy_nonoverlapping(REG_MAGIC.as_ptr(), ptr.add(RH_MAGIC), 8);
            (ptr.add(RH_VERSION) as *mut u32).write(REG_VERSION);
            // entry_count starts at 0 (file is zero-initialized)
            (ptr.add(RH_MAX_ENTRIES) as *mut u32).write(REG_MAX_ENTRIES);
        }

        Ok(Registry {
            _mmap: mmap,
            path: path.to_path_buf(),
        })
    }

    /// Base pointer to the mmap.
    fn base(&self) -> *mut u8 {
        self._mmap.as_mut_ptr()
    }

    /// Pointer to the start of entry `i`.
    fn entry_ptr(&self, i: u32) -> *mut u8 {
        unsafe {
            self.base()
                .add(REG_HEADER_SIZE + i as usize * REG_ENTRY_SIZE)
        }
    }

    /// The atomic entry count in the header.
    fn entry_count_atom(&self) -> &AtomicU32 {
        unsafe { &*(self.base().add(RH_ENTRY_COUNT) as *const AtomicU32) }
    }

    /// Register a topic in the global registry.
    ///
    /// Called automatically by `Publisher::register()`. If the registry is
    /// full or the strings are too long, the call is silently ignored (best-effort).
    pub fn register(&self, region: &str, uri: &str, pid: u32) -> Result<(), crate::error::Error> {
        if region.len() >= RE_REGION_NAME_MAX || uri.len() >= RE_TOPIC_URI_MAX {
            // Silently skip — strings too long for fixed-size entry
            return Ok(());
        }

        let ts = now_micros().unwrap_or(0);

        // Check if this (region, uri, pid) is already registered — update timestamp
        for i in 0..REG_MAX_ENTRIES {
            let entry = self.entry_ptr(i);
            let active = unsafe { &*(entry.add(RE_ACTIVE) as *const AtomicU32) };

            if active.load(Ordering::Acquire) != RE_STATE_ACTIVE {
                continue;
            }

            let existing_pid = unsafe { (entry.add(RE_PID) as *const u32).read() };
            if existing_pid != pid {
                continue;
            }

            let existing_region =
                unsafe { read_fixed_str(entry.add(RE_REGION_NAME), RE_REGION_NAME_MAX) };
            let existing_uri = unsafe { read_fixed_str(entry.add(RE_TOPIC_URI), RE_TOPIC_URI_MAX) };

            if existing_region == region && existing_uri == uri {
                // Already registered — update heartbeat timestamp
                unsafe {
                    (entry.add(RE_TIMESTAMP) as *mut u64).write(ts);
                }
                return Ok(());
            }
        }

        // Find a free slot via CAS
        for i in 0..REG_MAX_ENTRIES {
            let entry = self.entry_ptr(i);
            let active = unsafe { &*(entry.add(RE_ACTIVE) as *const AtomicU32) };

            if active
                .compare_exchange(
                    RE_STATE_FREE,
                    RE_STATE_ACTIVE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                continue;
            }

            // We own this slot — write entry data
            unsafe {
                (entry.add(RE_PID) as *mut u32).write(pid);
                (entry.add(RE_TIMESTAMP) as *mut u64).write(ts);
                write_fixed_str(entry.add(RE_REGION_NAME), region, RE_REGION_NAME_MAX);
                write_fixed_str(entry.add(RE_TOPIC_URI), uri, RE_TOPIC_URI_MAX);
            }

            // Increment entry count
            self.entry_count_atom().fetch_add(1, Ordering::Relaxed);

            return Ok(());
        }

        // Registry full — silently ignore (best-effort)
        Ok(())
    }

    /// Unregister all topics for a given region and PID.
    ///
    /// Called automatically by `Publisher::Drop`.
    ///
    /// Uses `compare_exchange` to atomically transition the slot from ACTIVE
    /// to FREE, preventing a double-decrement race with [`Self::prune_stale`].
    pub fn unregister(&self, region: &str, pid: u32) {
        for i in 0..REG_MAX_ENTRIES {
            let entry = self.entry_ptr(i);
            let active = unsafe { &*(entry.add(RE_ACTIVE) as *const AtomicU32) };

            if active.load(Ordering::Acquire) != RE_STATE_ACTIVE {
                continue;
            }

            let existing_pid = unsafe { (entry.add(RE_PID) as *const u32).read() };
            if existing_pid != pid {
                continue;
            }

            let existing_region =
                unsafe { read_fixed_str(entry.add(RE_REGION_NAME), RE_REGION_NAME_MAX) };
            if existing_region != region {
                continue;
            }

            // Atomically clear the slot -- only one thread wins the CAS and decrements.
            if active
                .compare_exchange(
                    RE_STATE_ACTIVE,
                    RE_STATE_FREE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                let prev = self.entry_count_atom().fetch_sub(1, Ordering::Relaxed);
                if prev == 0 {
                    // Counter wrapped due to double-decrement bug — clamp to zero.
                    self.entry_count_atom().store(0, Ordering::Relaxed);
                }
            }
        }
    }

    /// Discover topics matching a URI pattern.
    ///
    /// Pattern supports trailing `*` wildcard: `"/tick/*"` matches `"/tick/AAPL"`.
    /// An exact string (no wildcard) requires an exact match.
    pub fn discover(&self, pattern: &str) -> Vec<DiscoveredTopic> {
        let mut results = Vec::new();
        let (prefix, is_wildcard) = if let Some(prefix) = pattern.strip_suffix('*') {
            (prefix, true)
        } else {
            (pattern, false)
        };

        for i in 0..REG_MAX_ENTRIES {
            let entry = self.entry_ptr(i);
            let active = unsafe { &*(entry.add(RE_ACTIVE) as *const AtomicU32) };

            if active.load(Ordering::Acquire) != RE_STATE_ACTIVE {
                continue;
            }

            let uri = unsafe { read_fixed_str(entry.add(RE_TOPIC_URI), RE_TOPIC_URI_MAX) };

            let matches = if is_wildcard {
                uri.starts_with(prefix)
            } else {
                uri == pattern
            };

            if matches {
                let region =
                    unsafe { read_fixed_str(entry.add(RE_REGION_NAME), RE_REGION_NAME_MAX) };
                let pid = unsafe { (entry.add(RE_PID) as *const u32).read() };
                let timestamp_us = unsafe { (entry.add(RE_TIMESTAMP) as *const u64).read() };
                results.push(DiscoveredTopic {
                    region,
                    uri,
                    pid,
                    timestamp_us,
                });
            }
        }

        results
    }

    /// Discover topics matching a URI pattern that were registered after `since_us`.
    ///
    /// Returns topics whose URI matches `pattern` **and** whose heartbeat
    /// timestamp is strictly greater than `since_us` (microseconds since UNIX
    /// epoch). This enables reactive, polling-based discovery: callers store
    /// the latest `timestamp_us` from the previous batch and pass it as
    /// `since_us` on the next call.
    ///
    /// Pattern supports trailing `*` wildcard (same as [`Self::discover`]).
    pub fn discover_since(&self, pattern: &str, since_us: u64) -> Vec<DiscoveredTopic> {
        let mut results = Vec::new();
        let (prefix, is_wildcard) = if let Some(prefix) = pattern.strip_suffix('*') {
            (prefix, true)
        } else {
            (pattern, false)
        };

        for i in 0..REG_MAX_ENTRIES {
            let entry = self.entry_ptr(i);
            let active = unsafe { &*(entry.add(RE_ACTIVE) as *const AtomicU32) };

            if active.load(Ordering::Acquire) != RE_STATE_ACTIVE {
                continue;
            }

            let timestamp_us = unsafe { (entry.add(RE_TIMESTAMP) as *const u64).read() };
            if timestamp_us <= since_us {
                continue;
            }

            let uri = unsafe { read_fixed_str(entry.add(RE_TOPIC_URI), RE_TOPIC_URI_MAX) };

            let matches = if is_wildcard {
                uri.starts_with(prefix)
            } else {
                uri == pattern
            };

            if matches {
                let region =
                    unsafe { read_fixed_str(entry.add(RE_REGION_NAME), RE_REGION_NAME_MAX) };
                let pid = unsafe { (entry.add(RE_PID) as *const u32).read() };
                results.push(DiscoveredTopic {
                    region,
                    uri,
                    pid,
                    timestamp_us,
                });
            }
        }

        results
    }

    /// Prune entries from dead publishers (stale heartbeat).
    ///
    /// Removes entries whose timestamp is older than `stale_timeout` from now.
    ///
    /// Uses `compare_exchange` to atomically transition the slot from ACTIVE
    /// to FREE, preventing a double-decrement race with [`Self::unregister`].
    pub fn prune_stale(&self, stale_timeout: Duration) {
        let now = match now_micros() {
            Some(t) => t,
            None => return,
        };
        let cutoff = now.saturating_sub(stale_timeout.as_micros() as u64);

        for i in 0..REG_MAX_ENTRIES {
            let entry = self.entry_ptr(i);
            let active = unsafe { &*(entry.add(RE_ACTIVE) as *const AtomicU32) };

            if active.load(Ordering::Acquire) != RE_STATE_ACTIVE {
                continue;
            }

            let ts = unsafe { (entry.add(RE_TIMESTAMP) as *const u64).read() };
            if ts < cutoff {
                // Atomically clear the slot -- only one thread wins the CAS and decrements.
                if active
                    .compare_exchange(
                        RE_STATE_ACTIVE,
                        RE_STATE_FREE,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    let prev = self.entry_count_atom().fetch_sub(1, Ordering::Relaxed);
                    if prev == 0 {
                        // Counter wrapped due to double-decrement bug — clamp to zero.
                        self.entry_count_atom().store(0, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    /// Returns the path to the registry file.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}
