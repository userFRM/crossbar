#![allow(unsafe_code)]

use crate::error::CrossbarError;
use crate::types::{Method, Request, Response};
use bytes::Bytes;
use memmap2::MmapMut;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

// Magic bytes: "XBAR_SHM"
pub const MAGIC: &[u8; 8] = b"XBAR_SHM";
pub const VERSION: u32 = 1;
pub const GLOBAL_HEADER_SIZE: usize = 128;
pub const SLOT_HEADER_SIZE: usize = 64;

// Slot states
pub const FREE: u32 = 0;
pub const REQUEST_READY: u32 = 1;
pub const PROCESSING: u32 = 2;
pub const RESPONSE_READY: u32 = 3;
/// Intermediate state: slot is acquired by a client, request data is being
/// written. The server must not read the slot until it transitions to
/// [`REQUEST_READY`].
pub const WRITING: u32 = 4;

/// Minimum allowed `slot_data_capacity`.
///
/// Must be large enough to hold the error-response fallback payload
/// (2-byte empty headers + error body). 64 bytes (one cache line)
/// provides comfortable headroom.
pub const MIN_SLOT_DATA_CAPACITY: u32 = 64;

/// Configuration for shared memory transport.
#[derive(Debug, Clone)]
pub struct ShmConfig {
    /// Number of request/response slots (default: 64).
    pub slot_count: u32,
    /// Maximum payload size per slot in bytes (default: 65536).
    pub slot_data_capacity: u32,
    /// Heartbeat interval (default: 100ms).
    pub heartbeat_interval: Duration,
    /// Timeout for considering server/client dead (default: 5s).
    pub stale_timeout: Duration,
}

impl Default for ShmConfig {
    fn default() -> Self {
        Self {
            slot_count: 64,
            slot_data_capacity: 65536,
            heartbeat_interval: Duration::from_millis(100),
            stale_timeout: Duration::from_secs(5),
        }
    }
}

fn align_up(val: usize, align: usize) -> usize {
    (val + align - 1) & !(align - 1)
}

fn slot_stride(slot_data_capacity: u32) -> usize {
    align_up(SLOT_HEADER_SIZE + slot_data_capacity as usize, 64)
}

fn region_size(config: &ShmConfig) -> usize {
    GLOBAL_HEADER_SIZE + config.slot_count as usize * slot_stride(config.slot_data_capacity)
}

/// Derives the file path for a named shared memory region.
pub fn shm_path(name: &str) -> PathBuf {
    // Use /dev/shm on Linux for tmpfs-backed shared memory.
    // On other platforms, fall back to /tmp.
    if cfg!(target_os = "linux") {
        PathBuf::from(format!("/dev/shm/crossbar-{name}"))
    } else {
        PathBuf::from(format!("/tmp/crossbar-shm-{name}"))
    }
}

/// Handle to a memory-mapped shared region.
pub struct ShmRegion {
    mmap: MmapMut,
    pub slot_count: u32,
    pub slot_data_capacity: u32,
    slot_stride: usize,
    #[allow(dead_code)]
    pub path: PathBuf,
    #[allow(dead_code)]
    pub config: ShmConfig,
}

impl ShmRegion {
    /// Creates a new shared memory region (server side).
    ///
    /// Returns [`CrossbarError::ShmInvalidRegion`] if `slot_data_capacity` is
    /// below [`MIN_SLOT_DATA_CAPACITY`] (64 bytes).
    pub fn create(name: &str, config: &ShmConfig) -> Result<Self, CrossbarError> {
        if config.slot_data_capacity < MIN_SLOT_DATA_CAPACITY {
            return Err(CrossbarError::ShmInvalidRegion(format!(
                "slot_data_capacity {} is below minimum {}",
                config.slot_data_capacity, MIN_SLOT_DATA_CAPACITY
            )));
        }

        let path = shm_path(name);
        let size = region_size(config);
        let stride = slot_stride(config.slot_data_capacity);

        // Remove any stale file
        let _ = std::fs::remove_file(&path);

        // Create and size the file
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(CrossbarError::Io)?;

        file.set_len(size as u64).map_err(CrossbarError::Io)?;

        // Memory-map it
        let mut mmap = unsafe { MmapMut::map_mut(&file) }.map_err(CrossbarError::Io)?;

        // Write global header
        let ptr = mmap.as_mut_ptr();
        unsafe {
            // Magic
            std::ptr::copy_nonoverlapping(MAGIC.as_ptr(), ptr, 8);
            // Version
            std::ptr::copy_nonoverlapping(VERSION.to_le_bytes().as_ptr(), ptr.add(0x08), 4);
            // slot_count
            std::ptr::copy_nonoverlapping(
                config.slot_count.to_le_bytes().as_ptr(),
                ptr.add(0x0C),
                4,
            );
            // slot_data_capacity
            std::ptr::copy_nonoverlapping(
                config.slot_data_capacity.to_le_bytes().as_ptr(),
                ptr.add(0x10),
                4,
            );
            // slot_stride
            std::ptr::copy_nonoverlapping((stride as u32).to_le_bytes().as_ptr(), ptr.add(0x14), 4);
            // server_pid
            let pid = std::process::id() as u64;
            std::ptr::copy_nonoverlapping(pid.to_le_bytes().as_ptr(), ptr.add(0x20), 8);
            // region_size
            std::ptr::copy_nonoverlapping((size as u32).to_le_bytes().as_ptr(), ptr.add(0x28), 4);
        }

        // Initialize heartbeat
        let region = ShmRegion {
            mmap,
            slot_count: config.slot_count,
            slot_data_capacity: config.slot_data_capacity,
            slot_stride: stride,
            path,
            config: config.clone(),
        };
        region.update_heartbeat();

        // Initialize all slots to FREE
        for i in 0..config.slot_count {
            let state = region.slot_state(i);
            state.store(FREE, Ordering::Release);
        }

        Ok(region)
    }

    /// Opens an existing shared memory region (client side).
    pub fn open(name: &str) -> Result<Self, CrossbarError> {
        let path = shm_path(name);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(CrossbarError::Io)?;

        let mmap = unsafe { MmapMut::map_mut(&file) }.map_err(CrossbarError::Io)?;

        // Validate header
        if mmap.len() < GLOBAL_HEADER_SIZE {
            return Err(CrossbarError::ShmInvalidRegion(
                "region too small for header".into(),
            ));
        }

        let ptr = mmap.as_ptr();
        unsafe {
            // Check magic
            let mut magic = [0u8; 8];
            std::ptr::copy_nonoverlapping(ptr, magic.as_mut_ptr(), 8);
            if &magic != MAGIC {
                return Err(CrossbarError::ShmInvalidRegion(
                    "invalid magic bytes".into(),
                ));
            }

            // Check version
            let mut ver_bytes = [0u8; 4];
            std::ptr::copy_nonoverlapping(ptr.add(0x08), ver_bytes.as_mut_ptr(), 4);
            let ver = u32::from_le_bytes(ver_bytes);
            if ver != VERSION {
                return Err(CrossbarError::ShmInvalidRegion(format!(
                    "unsupported version {ver}, expected {VERSION}"
                )));
            }

            let mut buf4 = [0u8; 4];

            std::ptr::copy_nonoverlapping(ptr.add(0x0C), buf4.as_mut_ptr(), 4);
            let slot_count = u32::from_le_bytes(buf4);

            std::ptr::copy_nonoverlapping(ptr.add(0x10), buf4.as_mut_ptr(), 4);
            let slot_data_capacity = u32::from_le_bytes(buf4);

            std::ptr::copy_nonoverlapping(ptr.add(0x14), buf4.as_mut_ptr(), 4);
            let stride = u32::from_le_bytes(buf4) as usize;

            let expected_size = GLOBAL_HEADER_SIZE + slot_count as usize * stride;
            if mmap.len() < expected_size {
                return Err(CrossbarError::ShmInvalidRegion(format!(
                    "region size {} < expected {expected_size}",
                    mmap.len()
                )));
            }

            Ok(ShmRegion {
                mmap,
                slot_count,
                slot_data_capacity,
                slot_stride: stride,
                path,
                config: ShmConfig {
                    slot_count,
                    slot_data_capacity,
                    ..ShmConfig::default()
                },
            })
        }
    }

    // -- Slot access --

    fn slot_base(&self, idx: u32) -> *mut u8 {
        debug_assert!((idx as usize) < self.slot_count as usize);
        unsafe {
            self.mmap
                .as_ptr()
                .add(GLOBAL_HEADER_SIZE + idx as usize * self.slot_stride) as *mut u8
        }
    }

    /// Returns a reference to the slot's atomic state field.
    pub fn slot_state(&self, idx: u32) -> &AtomicU32 {
        unsafe { &*(self.slot_base(idx) as *const AtomicU32) }
    }

    /// Returns a reference to the slot's sequence counter.
    pub fn slot_sequence(&self, idx: u32) -> &AtomicU32 {
        unsafe { &*(self.slot_base(idx).add(4) as *const AtomicU32) }
    }

    /// Writes the client_id into a slot.
    pub fn set_slot_client_id(&self, idx: u32, client_id: u64) {
        unsafe {
            let ptr = self.slot_base(idx).add(0x08);
            std::ptr::copy_nonoverlapping(client_id.to_le_bytes().as_ptr(), ptr, 8);
        }
    }

    /// Reads the client_id from a slot.
    #[allow(dead_code)]
    pub fn slot_client_id(&self, idx: u32) -> u64 {
        unsafe {
            let ptr = self.slot_base(idx).add(0x08);
            let mut buf = [0u8; 8];
            std::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), 8);
            u64::from_le_bytes(buf)
        }
    }

    /// Updates the slot timestamp to now.
    pub fn touch_slot(&self, idx: u32) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
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

    // -- Request/Response I/O --

    /// Writes a request into a slot's data region.
    ///
    /// Slot metadata fields (method, uri_len, body_len, headers_data_len) are
    /// also written. The data region is packed as: [uri][headers_data][body].
    ///
    /// Returns [`CrossbarError::HeaderOverflow`] if headers exceed the wire
    /// protocol's u16 length limits.
    pub fn write_request_to_slot(&self, idx: u32, req: &Request) -> Result<(), CrossbarError> {
        let base = self.slot_base(idx);
        let uri_bytes = req.uri.raw().as_bytes();
        let headers_data = crate::transport::serialize_headers(&req.headers)?;

        unsafe {
            // method (1 byte at offset 0x18)
            *base.add(0x18) = u8::from(req.method);
            // uri_len
            std::ptr::copy_nonoverlapping(
                (uri_bytes.len() as u32).to_le_bytes().as_ptr(),
                base.add(0x1C),
                4,
            );
            // body_len
            std::ptr::copy_nonoverlapping(
                (req.body.len() as u32).to_le_bytes().as_ptr(),
                base.add(0x20),
                4,
            );
            // headers_data_len
            std::ptr::copy_nonoverlapping(
                (headers_data.len() as u32).to_le_bytes().as_ptr(),
                base.add(0x24),
                4,
            );

            // Data region starts at SLOT_HEADER_SIZE (0x40)
            let data = base.add(SLOT_HEADER_SIZE);
            let mut off = 0;
            std::ptr::copy_nonoverlapping(uri_bytes.as_ptr(), data.add(off), uri_bytes.len());
            off += uri_bytes.len();
            std::ptr::copy_nonoverlapping(headers_data.as_ptr(), data.add(off), headers_data.len());
            off += headers_data.len();
            std::ptr::copy_nonoverlapping(req.body.as_ptr(), data.add(off), req.body.len());
        }
        Ok(())
    }

    /// Reads a request from a slot's data region.
    ///
    /// Validates that all length fields fit within [`slot_data_capacity`] before
    /// creating any slices, preventing out-of-bounds reads from corrupted or
    /// malicious shared memory data.
    pub fn read_request_from_slot(&self, idx: u32) -> Result<Request, CrossbarError> {
        let base = self.slot_base(idx);
        let capacity = self.slot_data_capacity as usize;

        unsafe {
            let method_byte = *base.add(0x18);
            let method = Method::try_from(method_byte).map_err(CrossbarError::InvalidMethod)?;

            let mut buf4 = [0u8; 4];
            std::ptr::copy_nonoverlapping(base.add(0x1C), buf4.as_mut_ptr(), 4);
            let uri_len = u32::from_le_bytes(buf4) as usize;

            std::ptr::copy_nonoverlapping(base.add(0x20), buf4.as_mut_ptr(), 4);
            let body_len = u32::from_le_bytes(buf4) as usize;

            std::ptr::copy_nonoverlapping(base.add(0x24), buf4.as_mut_ptr(), 4);
            let headers_data_len = u32::from_le_bytes(buf4) as usize;

            // Bounds check: total payload must fit within the slot data region.
            let total = uri_len
                .checked_add(headers_data_len)
                .and_then(|v| v.checked_add(body_len));
            match total {
                Some(t) if t <= capacity => {}
                _ => {
                    return Err(CrossbarError::ShmInvalidRegion(format!(
                        "request payload lengths ({uri_len}+{headers_data_len}+{body_len}) exceed slot capacity ({capacity})"
                    )));
                }
            }

            let data = base.add(SLOT_HEADER_SIZE);
            let mut off = 0;

            // URI
            let uri_slice = std::slice::from_raw_parts(data.add(off), uri_len);
            let uri_str = std::str::from_utf8(uri_slice).map_err(|_| {
                CrossbarError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "URI not valid UTF-8",
                ))
            })?;
            off += uri_len;

            // Headers
            let headers_slice = std::slice::from_raw_parts(data.add(off), headers_data_len);
            let headers = crate::transport::deserialize_headers(headers_slice)?;
            off += headers_data_len;

            // Body
            let body_slice = std::slice::from_raw_parts(data.add(off), body_len);
            let body = Bytes::copy_from_slice(body_slice);

            let mut req = Request::new(method, uri_str).with_body(body);
            req.headers = headers;
            Ok(req)
        }
    }

    /// Writes a response into a slot's data region.
    ///
    /// If the response exceeds [`slot_data_capacity`] or its headers cannot be
    /// serialized, a 500 error with an empty-header block is written instead.
    /// The minimum capacity enforced by [`ShmRegion::create`] guarantees that
    /// the fallback payload always fits.
    pub fn write_response_to_slot(&self, idx: u32, resp: &Response) {
        let base = self.slot_base(idx);
        let capacity = self.slot_data_capacity as usize;

        // Hardcoded fallback: 2-byte empty headers ([0x00, 0x00] = 0 headers)
        // + short error body. Always fits within MIN_SLOT_DATA_CAPACITY.
        let fallback = || -> (u16, Vec<u8>, &'static [u8]) {
            (500u16, vec![0u8, 0u8], b"response too large for shm slot")
        };

        let (status, headers_data, body) = match crate::transport::serialize_headers(&resp.headers)
        {
            Ok(hd) => {
                let total = hd.len() + resp.body.len();
                if total > capacity {
                    fallback()
                } else {
                    (resp.status, hd, resp.body.as_ref())
                }
            }
            Err(_) => fallback(),
        };

        unsafe {
            // response_status (2 bytes at 0x28)
            std::ptr::copy_nonoverlapping(status.to_le_bytes().as_ptr(), base.add(0x28), 2);
            // response_body_len
            std::ptr::copy_nonoverlapping(
                (body.len() as u32).to_le_bytes().as_ptr(),
                base.add(0x2C),
                4,
            );
            // response_headers_data_len
            std::ptr::copy_nonoverlapping(
                (headers_data.len() as u32).to_le_bytes().as_ptr(),
                base.add(0x30),
                4,
            );

            // Response data goes into the data region (overwrites request data)
            let data = base.add(SLOT_HEADER_SIZE);
            let mut off = 0;
            std::ptr::copy_nonoverlapping(headers_data.as_ptr(), data.add(off), headers_data.len());
            off += headers_data.len();
            std::ptr::copy_nonoverlapping(body.as_ptr(), data.add(off), body.len());
        }
    }

    /// Reads a response from a slot's data region.
    ///
    /// Validates that all length fields fit within [`slot_data_capacity`] before
    /// creating any slices.
    pub fn read_response_from_slot(&self, idx: u32) -> Result<Response, CrossbarError> {
        let base = self.slot_base(idx);
        let capacity = self.slot_data_capacity as usize;

        unsafe {
            let mut buf2 = [0u8; 2];
            std::ptr::copy_nonoverlapping(base.add(0x28), buf2.as_mut_ptr(), 2);
            let status = u16::from_le_bytes(buf2);

            let mut buf4 = [0u8; 4];
            std::ptr::copy_nonoverlapping(base.add(0x2C), buf4.as_mut_ptr(), 4);
            let body_len = u32::from_le_bytes(buf4) as usize;

            std::ptr::copy_nonoverlapping(base.add(0x30), buf4.as_mut_ptr(), 4);
            let headers_data_len = u32::from_le_bytes(buf4) as usize;

            // Bounds check: total payload must fit within the slot data region.
            let total = headers_data_len.checked_add(body_len);
            match total {
                Some(t) if t <= capacity => {}
                _ => {
                    return Err(CrossbarError::ShmInvalidRegion(format!(
                        "response payload lengths ({headers_data_len}+{body_len}) exceed slot capacity ({capacity})"
                    )));
                }
            }

            let data = base.add(SLOT_HEADER_SIZE);
            let mut off = 0;

            let headers_slice = std::slice::from_raw_parts(data.add(off), headers_data_len);
            let headers = crate::transport::deserialize_headers(headers_slice)?;
            off += headers_data_len;

            let body_slice = std::slice::from_raw_parts(data.add(off), body_len);
            let body = Bytes::copy_from_slice(body_slice);

            Ok(Response {
                status,
                headers,
                body,
            })
        }
    }

    // -- Heartbeat --

    /// Updates the server heartbeat timestamp.
    pub fn update_heartbeat(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let hb = self.heartbeat_atomic();
        hb.store(now, Ordering::Release);
    }

    /// Checks if the server heartbeat is recent enough.
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

    fn heartbeat_atomic(&self) -> &AtomicU64 {
        unsafe { &*(self.mmap.as_ptr().add(0x18) as *const AtomicU64) }
    }

    // -- Slot acquisition --

    /// Tries to acquire a FREE slot via CAS. Returns the slot index or None.
    ///
    /// The slot enters [`WRITING`] state — the caller must write the request
    /// data and then transition the slot to [`REQUEST_READY`] so the server
    /// knows the data is complete.
    pub fn try_acquire_slot(&self, client_id: u64) -> Option<u32> {
        for i in 0..self.slot_count {
            let state = self.slot_state(i);
            if state
                .compare_exchange(FREE, WRITING, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.set_slot_client_id(i, client_id);
                self.slot_sequence(i).fetch_add(1, Ordering::Relaxed);
                self.touch_slot(i);
                return Some(i);
            }
        }
        None
    }

    /// Scans for stale slots and resets them to FREE.
    ///
    /// Only recovers client-owned states (WRITING, RESPONSE_READY) and
    /// REQUEST_READY. PROCESSING slots are skipped because the server owns
    /// them — if the server is alive (heartbeat is fresh) then a PROCESSING
    /// slot is a legitimately running handler, regardless of how long it
    /// takes. If the server dies, the heartbeat goes stale and clients will
    /// get [`CrossbarError::ShmServerDead`] on their next request.
    ///
    /// Uses CAS to avoid racing with legitimate state transitions.
    pub fn recover_stale_slots(&self, stale_timeout: Duration) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        for i in 0..self.slot_count {
            let state = self.slot_state(i);
            let current = state.load(Ordering::Acquire);

            // Skip FREE (nothing to do) and PROCESSING (server owns it).
            if current == FREE || current == PROCESSING {
                continue;
            }

            let ts = self.slot_timestamp(i);
            if now.saturating_sub(ts) > stale_timeout.as_millis() as u64 {
                // CAS instead of store: if the state changed since we
                // loaded it, a legitimate transition happened and we
                // must not clobber it.
                let _ = state.compare_exchange(current, FREE, Ordering::AcqRel, Ordering::Acquire);
            }
        }
    }
}
