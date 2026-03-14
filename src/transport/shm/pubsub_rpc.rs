// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(unsafe_code)]
#![allow(dead_code)]

//! Pub/sub-backed RPC: URI routing at pub/sub speed.
//!
//! Uses dual pub/sub channels (request ring + response ring) per client
//! instead of the coordination slot state machine, eliminating the slot
//! overhead per roundtrip. Each client gets a dedicated SPSC ring pair;
//! the block pool is shared across all clients (same Treiber stack as
//! [`ShmPoolPublisher`](super::pool_pubsub::ShmPoolPublisher)).
//!
//! Measured: **~560 ns** for `/health -> "ok"` — **25% faster** than the
//! slot-based 745 ns, and **38% faster** for large (64 KB) payloads.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::mmap::RawMmap;
use super::notify;
use super::ShmHandle;
use crate::error::CrossbarError;
use crate::router::Router;
use crate::types::{Body, Method, Request, Response};

// ─── Layout constants ────────────────────────────────────────────────────

const MAGIC: &[u8; 8] = b"XBAR_RC\0";
const VERSION: u32 = 1;

const HEADER_SIZE: usize = 128;
const CLIENT_ENTRY_SIZE: usize = 64;
const RING_ENTRY_SIZE: usize = 16;
const BLOCK_DATA_OFFSET: usize = 8;
const NO_BLOCK: u32 = u32::MAX;

// Global header offsets
const GH_MAGIC: usize = 0x00;
const GH_VERSION: usize = 0x08;
const GH_MAX_CLIENTS: usize = 0x0C;
const GH_BLOCK_COUNT: usize = 0x10;
const GH_BLOCK_SIZE: usize = 0x14;
const GH_RING_DEPTH: usize = 0x18;
const GH_POOL_HEAD: usize = 0x20;
const GH_HEARTBEAT: usize = 0x28;
const GH_PID: usize = 0x30;
const GH_SERVER_NOTIFY: usize = 0x38;
const GH_SERVER_WAITERS: usize = 0x3C;
const GH_STALE_TIMEOUT_US: usize = 0x40;

// Client entry offsets (relative to entry start)
const CE_ACTIVE: usize = 0x00;
const CE_CLIENT_PID: usize = 0x04;
const CE_CLIENT_ID: usize = 0x08;
const CE_REQ_WRITE_SEQ: usize = 0x10;
const CE_RESP_WRITE_SEQ: usize = 0x18;
const CE_RESP_NOTIFY: usize = 0x20;
const CE_RESP_WAITERS: usize = 0x24;
const CE_HEARTBEAT: usize = 0x28;

// Ring entry offsets (same as pool_pubsub)
const RE_SEQ: usize = 0;
const RE_BLOCK_IDX: usize = 8;
const RE_DATA_LEN: usize = 12;

// Message header size (inline in block data section)
const MSG_HEADER_SIZE: usize = 16;

// Request message offsets (relative to block data start)
const RQ_METHOD: usize = 0;
const RQ_FLAGS: usize = 1;
const RQ_URI_LEN: usize = 2;
const RQ_BODY_LEN: usize = 4;
const RQ_HEADERS_LEN: usize = 8;
const RQ_CORR_ID: usize = 12;

// Response message offsets (relative to block data start)
const RS_STATUS: usize = 0;
const RS_BODY_LEN: usize = 4;
const RS_HEADERS_LEN: usize = 8;
const RS_CORR_ID: usize = 12;

// ─── Config ──────────────────────────────────────────────────────────────

/// Configuration for pub/sub-backed RPC.
#[derive(Debug, Clone)]
pub struct PubSubRpcConfig {
    /// Maximum concurrent clients (default: 16).
    pub max_clients: u32,
    /// Number of blocks in the shared pool (default: 256).
    pub block_count: u32,
    /// Size of each block in bytes (default: 65536 = 64 KiB).
    pub block_size: u32,
    /// Ring depth per client (default: 8).
    pub ring_depth: u32,
    /// Heartbeat write interval (default: 100 ms).
    pub heartbeat_interval: Duration,
    /// Server considered dead after this duration (default: 5 s).
    pub stale_timeout: Duration,
}

impl Default for PubSubRpcConfig {
    fn default() -> Self {
        Self {
            max_clients: 16,
            block_count: 256,
            block_size: 65536,
            ring_depth: 8,
            heartbeat_interval: Duration::from_millis(100),
            stale_timeout: Duration::from_secs(5),
        }
    }
}

// ─── Layout helpers ──────────────────────────────────────────────────────

fn client_table_off() -> usize {
    HEADER_SIZE
}

fn client_entry_off(idx: u32) -> usize {
    client_table_off() + idx as usize * CLIENT_ENTRY_SIZE
}

fn req_ring_base(config: &PubSubRpcConfig) -> usize {
    client_table_off() + config.max_clients as usize * CLIENT_ENTRY_SIZE
}

fn req_ring_entry_off(config: &PubSubRpcConfig, client_idx: u32, slot: u32) -> usize {
    req_ring_base(config)
        + client_idx as usize * config.ring_depth as usize * RING_ENTRY_SIZE
        + slot as usize * RING_ENTRY_SIZE
}

fn resp_ring_base(config: &PubSubRpcConfig) -> usize {
    req_ring_base(config)
        + config.max_clients as usize * config.ring_depth as usize * RING_ENTRY_SIZE
}

fn resp_ring_entry_off(config: &PubSubRpcConfig, client_idx: u32, slot: u32) -> usize {
    resp_ring_base(config)
        + client_idx as usize * config.ring_depth as usize * RING_ENTRY_SIZE
        + slot as usize * RING_ENTRY_SIZE
}

fn block_pool_offset(config: &PubSubRpcConfig) -> usize {
    resp_ring_base(config)
        + config.max_clients as usize * config.ring_depth as usize * RING_ENTRY_SIZE
}

fn region_size(config: &PubSubRpcConfig) -> usize {
    block_pool_offset(config) + config.block_count as usize * config.block_size as usize
}

fn shm_path(name: &str) -> PathBuf {
    if cfg!(target_os = "linux") {
        PathBuf::from(format!("/dev/shm/crossbar-rpc-{name}"))
    } else {
        PathBuf::from(format!("/tmp/crossbar-rpc-shm-{name}"))
    }
}

fn lock_path(name: &str) -> PathBuf {
    let mut p = shm_path(name);
    p.set_extension("lock");
    p
}

// ─── Treiber stack helpers ───────────────────────────────────────────────

#[inline]
fn pack(gen: u32, idx: u32) -> u64 {
    u64::from(gen) << 32 | u64::from(idx)
}

#[inline]
#[allow(clippy::cast_possible_truncation)]
fn unpack(val: u64) -> (u32, u32) {
    ((val >> 32) as u32, val as u32)
}

// ─── RpcRegion ───────────────────────────────────────────────────────────

struct RpcRegion {
    mmap: RawMmap,
    config: PubSubRpcConfig,
    pool_offset: usize,
}

impl RpcRegion {
    // ── Pool allocator (Treiber stack, same as pool_pubsub) ──

    #[inline]
    fn pool_head(&self) -> &AtomicU64 {
        unsafe { &*(self.mmap.as_ptr().add(GH_POOL_HEAD) as *const AtomicU64) }
    }

    #[inline]
    fn block_ptr(&self, idx: u32) -> *mut u8 {
        debug_assert!((idx as usize) < self.config.block_count as usize);
        unsafe {
            self.mmap
                .as_ptr()
                .add(self.pool_offset + idx as usize * self.config.block_size as usize)
                .cast_mut()
        }
    }

    #[inline]
    fn alloc_block(&self) -> Option<u32> {
        loop {
            let head = self.pool_head().load(Ordering::Acquire);
            let (gen, idx) = unpack(head);
            if idx == NO_BLOCK {
                return None;
            }
            let next = unsafe {
                let mut buf = [0u8; 4];
                std::ptr::copy_nonoverlapping(self.block_ptr(idx), buf.as_mut_ptr(), 4);
                u32::from_le_bytes(buf)
            };
            let new_head = pack(gen.wrapping_add(1), next);
            if self
                .pool_head()
                .compare_exchange(head, new_head, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(idx);
            }
        }
    }

    #[inline]
    fn free_block(&self, idx: u32) {
        debug_assert!((idx as usize) < self.config.block_count as usize);
        loop {
            let head = self.pool_head().load(Ordering::Acquire);
            let (gen, old_head_idx) = unpack(head);
            unsafe {
                std::ptr::copy_nonoverlapping(
                    old_head_idx.to_le_bytes().as_ptr(),
                    self.block_ptr(idx),
                    4,
                );
            }
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

    fn init_free_list(&self) {
        for i in 0..self.config.block_count {
            let next = if i + 1 < self.config.block_count {
                i + 1
            } else {
                NO_BLOCK
            };
            unsafe {
                std::ptr::copy_nonoverlapping(next.to_le_bytes().as_ptr(), self.block_ptr(i), 4);
            }
        }
        self.pool_head().store(pack(0, 0), Ordering::Release);
    }

    fn data_capacity(&self) -> usize {
        self.config.block_size as usize - BLOCK_DATA_OFFSET
    }

    // ── Heartbeat ──

    fn heartbeat_atom(&self) -> &AtomicU64 {
        unsafe { &*(self.mmap.as_ptr().add(GH_HEARTBEAT) as *const AtomicU64) }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn update_heartbeat(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        self.heartbeat_atom().store(now, Ordering::Release);
    }

    #[allow(clippy::cast_possible_truncation)]
    fn check_heartbeat(&self) -> Result<(), CrossbarError> {
        let hb = self.heartbeat_atom().load(Ordering::Acquire);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        if now.saturating_sub(hb) > self.config.stale_timeout.as_micros() as u64 {
            return Err(CrossbarError::ShmServerDead);
        }
        Ok(())
    }

    // ── Server notification ──

    fn server_notify(&self) -> &AtomicU32 {
        unsafe { &*(self.mmap.as_ptr().add(GH_SERVER_NOTIFY) as *const AtomicU32) }
    }

    fn server_waiters(&self) -> &AtomicU32 {
        unsafe { &*(self.mmap.as_ptr().add(GH_SERVER_WAITERS) as *const AtomicU32) }
    }

    /// Notify the server if it's sleeping (futex). When the server is
    /// spinning, it polls write_seqs directly — no notify needed.
    fn notify_server(&self) {
        if self.server_waiters().load(Ordering::Acquire) > 0 {
            self.server_notify().fetch_add(1, Ordering::Release);
            notify::wake_all(self.server_notify());
        }
    }

    fn wait_for_work(&self) {
        let cur = self.server_notify().load(Ordering::Acquire);
        self.server_waiters().fetch_add(1, Ordering::AcqRel);
        // Re-check to avoid missed wake
        if self.server_notify().load(Ordering::Acquire) != cur {
            self.server_waiters().fetch_sub(1, Ordering::Release);
            return;
        }
        let _ = notify::wait_until_not(self.server_notify(), cur, Duration::from_millis(1));
        self.server_waiters().fetch_sub(1, Ordering::Release);
    }

    // ── Client table accessors ──

    fn client_active(&self, idx: u32) -> &AtomicU32 {
        let off = client_entry_off(idx);
        unsafe { &*(self.mmap.as_ptr().add(off + CE_ACTIVE) as *const AtomicU32) }
    }

    fn client_req_write_seq(&self, idx: u32) -> &AtomicU64 {
        let off = client_entry_off(idx);
        unsafe { &*(self.mmap.as_ptr().add(off + CE_REQ_WRITE_SEQ) as *const AtomicU64) }
    }

    fn client_resp_write_seq(&self, idx: u32) -> &AtomicU64 {
        let off = client_entry_off(idx);
        unsafe { &*(self.mmap.as_ptr().add(off + CE_RESP_WRITE_SEQ) as *const AtomicU64) }
    }

    fn client_resp_notify(&self, idx: u32) -> &AtomicU32 {
        let off = client_entry_off(idx);
        unsafe { &*(self.mmap.as_ptr().add(off + CE_RESP_NOTIFY) as *const AtomicU32) }
    }

    fn client_resp_waiters(&self, idx: u32) -> &AtomicU32 {
        let off = client_entry_off(idx);
        unsafe { &*(self.mmap.as_ptr().add(off + CE_RESP_WAITERS) as *const AtomicU32) }
    }

    // ── Ring operations ──

    /// Publish a block descriptor to a ring slot.
    fn publish_to_ring(
        &self,
        entry_off: usize,
        block_idx: u32,
        data_len: u32,
        seq: u64,
        write_seq_atom: &AtomicU64,
    ) {
        let entry_ptr = unsafe { self.mmap.as_mut_ptr().add(entry_off) };

        // Seqlock open — invalidate
        let entry_seq = unsafe { &*(entry_ptr.add(RE_SEQ) as *const AtomicU64) };
        entry_seq.store(0, Ordering::Release);

        // Write block_idx and data_len
        unsafe {
            (entry_ptr.add(RE_BLOCK_IDX) as *mut u32).write(block_idx);
            (entry_ptr.add(RE_DATA_LEN) as *mut u32).write(data_len);
        }

        // Seqlock close — data visible
        entry_seq.store(seq, Ordering::Release);

        // Bump write_seq
        write_seq_atom.store(seq, Ordering::Release);
    }

    /// Try to read a block descriptor from a ring slot.
    /// Returns `(block_idx, data_len)` if the expected seq is available.
    fn try_recv_from_ring(&self, entry_off: usize, expected_seq: u64) -> Option<(u32, u32)> {
        let entry_ptr = unsafe { self.mmap.as_ptr().add(entry_off) };

        // Seqlock check 1
        let entry_seq = unsafe { &*(entry_ptr.add(RE_SEQ) as *const AtomicU64) };
        if entry_seq.load(Ordering::Acquire) != expected_seq {
            return None;
        }

        let block_idx = unsafe { (entry_ptr.add(RE_BLOCK_IDX) as *const u32).read() };
        let data_len = unsafe { (entry_ptr.add(RE_DATA_LEN) as *const u32).read() };

        // Seqlock check 2
        if entry_seq.load(Ordering::Acquire) != expected_seq {
            return None;
        }

        if block_idx == NO_BLOCK {
            return None;
        }

        Some((block_idx, data_len))
    }

    // ── Request ring helpers (client publishes, server reads) ──

    fn publish_request(&self, client_idx: u32, block_idx: u32, data_len: u32, seq: u64) {
        let slot = (seq % self.config.ring_depth as u64) as u32;
        let entry_off = req_ring_entry_off(&self.config, client_idx, slot);
        let write_seq = self.client_req_write_seq(client_idx);
        self.publish_to_ring(entry_off, block_idx, data_len, seq, write_seq);
    }

    fn try_recv_request(&self, client_idx: u32, last_seq: u64) -> Option<(u32, u32, u64)> {
        let write_seq = self
            .client_req_write_seq(client_idx)
            .load(Ordering::Acquire);
        if write_seq <= last_seq {
            return None;
        }
        let target = last_seq + 1;
        let slot = (target % self.config.ring_depth as u64) as u32;
        let entry_off = req_ring_entry_off(&self.config, client_idx, slot);
        self.try_recv_from_ring(entry_off, target)
            .map(|(block_idx, data_len)| (block_idx, data_len, target))
    }

    // ── Response ring helpers (server publishes, client reads) ──

    fn publish_response(&self, client_idx: u32, block_idx: u32, data_len: u32, seq: u64) {
        let slot = (seq % self.config.ring_depth as u64) as u32;
        let entry_off = resp_ring_entry_off(&self.config, client_idx, slot);
        let write_seq = self.client_resp_write_seq(client_idx);
        self.publish_to_ring(entry_off, block_idx, data_len, seq, write_seq);
    }

    fn try_recv_response(&self, client_idx: u32, last_seq: u64) -> Option<(u32, u32, u64)> {
        let write_seq = self
            .client_resp_write_seq(client_idx)
            .load(Ordering::Acquire);
        if write_seq <= last_seq {
            return None;
        }
        let target = last_seq + 1;
        let slot = (target % self.config.ring_depth as u64) as u32;
        let entry_off = resp_ring_entry_off(&self.config, client_idx, slot);
        self.try_recv_from_ring(entry_off, target)
            .map(|(block_idx, data_len)| (block_idx, data_len, target))
    }

    // ── Request/Response encoding ──

    /// Encode a request into a block. Returns total data length (header + body + uri + headers_data).
    fn encode_request(
        &self,
        block_idx: u32,
        req: &Request,
        correlation_id: u32,
    ) -> Result<u32, CrossbarError> {
        let data_ptr = unsafe { self.block_ptr(block_idx).add(BLOCK_DATA_OFFSET) };
        let capacity = self.data_capacity();

        let uri_bytes = req.uri.raw().as_bytes();
        let body_bytes: &[u8] = req.body.as_ref();

        // Serialize headers to a temp buffer to measure length
        let headers_buf_offset = MSG_HEADER_SIZE + body_bytes.len() + uri_bytes.len();
        let headers_buf_capacity = capacity.saturating_sub(headers_buf_offset);

        // Check capacity
        let mut headers_data_len = 0usize;
        if !req.headers.is_empty() {
            let target = unsafe {
                std::slice::from_raw_parts_mut(
                    data_ptr.add(headers_buf_offset),
                    headers_buf_capacity,
                )
            };
            headers_data_len = crate::transport::serialize_headers_into(&req.headers, target)?;
        }

        let total = MSG_HEADER_SIZE + body_bytes.len() + uri_bytes.len() + headers_data_len;
        if total > capacity {
            return Err(CrossbarError::ShmMessageTooLarge {
                size: total,
                max: capacity,
            });
        }

        // Write message header
        unsafe {
            *data_ptr.add(RQ_METHOD) = u8::from(req.method);
            *data_ptr.add(RQ_FLAGS) = 0;
            (data_ptr.add(RQ_URI_LEN) as *mut u16).write((uri_bytes.len() as u16).to_le());
            (data_ptr.add(RQ_BODY_LEN) as *mut u32).write((body_bytes.len() as u32).to_le());
            (data_ptr.add(RQ_HEADERS_LEN) as *mut u32).write((headers_data_len as u32).to_le());
            (data_ptr.add(RQ_CORR_ID) as *mut u32).write(correlation_id.to_le());
        }

        // Write body
        if !body_bytes.is_empty() {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    body_bytes.as_ptr(),
                    data_ptr.add(MSG_HEADER_SIZE),
                    body_bytes.len(),
                );
            }
        }

        // Write URI
        unsafe {
            std::ptr::copy_nonoverlapping(
                uri_bytes.as_ptr(),
                data_ptr.add(MSG_HEADER_SIZE + body_bytes.len()),
                uri_bytes.len(),
            );
        }

        // Headers already written in-place above

        #[allow(clippy::cast_possible_truncation)]
        Ok(total as u32)
    }

    /// Decode a request from a block. Returns (Request, correlation_id).
    /// The caller is responsible for freeing the block.
    fn decode_request(
        &self,
        block_idx: u32,
        data_len: u32,
    ) -> Result<(Request, u32), CrossbarError> {
        let data_ptr = unsafe { self.block_ptr(block_idx).add(BLOCK_DATA_OFFSET) };
        let data_len = data_len as usize;

        if data_len < MSG_HEADER_SIZE {
            return Err(CrossbarError::ShmInvalidRegion(
                "request data too short for header".into(),
            ));
        }

        // Read header
        let method_byte = unsafe { *data_ptr.add(RQ_METHOD) };
        let method = Method::try_from(method_byte).map_err(CrossbarError::InvalidMethod)?;
        let uri_len =
            u16::from_le(unsafe { (data_ptr.add(RQ_URI_LEN) as *const u16).read() }) as usize;
        let body_len =
            u32::from_le(unsafe { (data_ptr.add(RQ_BODY_LEN) as *const u32).read() }) as usize;
        let headers_data_len =
            u32::from_le(unsafe { (data_ptr.add(RQ_HEADERS_LEN) as *const u32).read() }) as usize;
        let correlation_id =
            u32::from_le(unsafe { (data_ptr.add(RQ_CORR_ID) as *const u32).read() });

        // Validate lengths
        if MSG_HEADER_SIZE + body_len + uri_len + headers_data_len > data_len {
            return Err(CrossbarError::ShmInvalidRegion(
                "request data lengths exceed block".into(),
            ));
        }

        // Read body (copy to owned Vec — block will be freed by caller)
        let body = if body_len > 0 {
            let mut buf = vec![0u8; body_len];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    data_ptr.add(MSG_HEADER_SIZE),
                    buf.as_mut_ptr(),
                    body_len,
                );
            }
            Body::Owned(buf)
        } else {
            Body::Empty
        };

        // Read URI
        let uri_bytes = unsafe {
            std::slice::from_raw_parts(data_ptr.add(MSG_HEADER_SIZE + body_len), uri_len)
        };
        let uri_str = std::str::from_utf8(uri_bytes)
            .map_err(|_| CrossbarError::ShmInvalidRegion("request URI not valid UTF-8".into()))?;

        let mut req = Request::new(method, uri_str);
        req.body = body;

        // Read headers
        if headers_data_len > 0 {
            let headers_data = unsafe {
                std::slice::from_raw_parts(
                    data_ptr.add(MSG_HEADER_SIZE + body_len + uri_len),
                    headers_data_len,
                )
            };
            req.headers = crate::transport::deserialize_headers(headers_data)?;
        }

        Ok((req, correlation_id))
    }

    /// Encode a response into a block. Returns total data length.
    fn encode_response(
        &self,
        block_idx: u32,
        resp: &Response,
        correlation_id: u32,
    ) -> Result<u32, CrossbarError> {
        let data_ptr = unsafe { self.block_ptr(block_idx).add(BLOCK_DATA_OFFSET) };
        let capacity = self.data_capacity();

        let body_bytes: &[u8] = resp.body.as_ref();

        // Serialize headers in-place
        let headers_buf_offset = MSG_HEADER_SIZE + body_bytes.len();
        let headers_buf_capacity = capacity.saturating_sub(headers_buf_offset);
        let mut headers_data_len = 0usize;
        if !resp.headers.is_empty() {
            let target = unsafe {
                std::slice::from_raw_parts_mut(
                    data_ptr.add(headers_buf_offset),
                    headers_buf_capacity,
                )
            };
            headers_data_len = crate::transport::serialize_headers_into(&resp.headers, target)?;
        }

        let total = MSG_HEADER_SIZE + body_bytes.len() + headers_data_len;
        if total > capacity {
            return Err(CrossbarError::ShmMessageTooLarge {
                size: total,
                max: capacity,
            });
        }

        // Write header
        unsafe {
            (data_ptr.add(RS_STATUS) as *mut u16).write((resp.status).to_le());
            // 2 bytes reserved
            (data_ptr.add(RS_BODY_LEN) as *mut u32).write((body_bytes.len() as u32).to_le());
            (data_ptr.add(RS_HEADERS_LEN) as *mut u32).write((headers_data_len as u32).to_le());
            (data_ptr.add(RS_CORR_ID) as *mut u32).write(correlation_id.to_le());
        }

        // Write body
        if !body_bytes.is_empty() {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    body_bytes.as_ptr(),
                    data_ptr.add(MSG_HEADER_SIZE),
                    body_bytes.len(),
                );
            }
        }

        // Headers already written in-place

        #[allow(clippy::cast_possible_truncation)]
        Ok(total as u32)
    }

    /// Decode a response from a block. Returns (Response, correlation_id).
    /// The caller is responsible for freeing the block.
    fn decode_response(
        &self,
        block_idx: u32,
        data_len: u32,
    ) -> Result<(Response, u32), CrossbarError> {
        let data_ptr = unsafe { self.block_ptr(block_idx).add(BLOCK_DATA_OFFSET) };
        let data_len = data_len as usize;

        if data_len < MSG_HEADER_SIZE {
            return Err(CrossbarError::ShmInvalidRegion(
                "response data too short for header".into(),
            ));
        }

        let status = u16::from_le(unsafe { (data_ptr.add(RS_STATUS) as *const u16).read() });
        let body_len =
            u32::from_le(unsafe { (data_ptr.add(RS_BODY_LEN) as *const u32).read() }) as usize;
        let headers_data_len =
            u32::from_le(unsafe { (data_ptr.add(RS_HEADERS_LEN) as *const u32).read() }) as usize;
        let correlation_id =
            u32::from_le(unsafe { (data_ptr.add(RS_CORR_ID) as *const u32).read() });

        if MSG_HEADER_SIZE + body_len + headers_data_len > data_len {
            return Err(CrossbarError::ShmInvalidRegion(
                "response data lengths exceed block".into(),
            ));
        }

        let body = if body_len > 0 {
            let mut buf = vec![0u8; body_len];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    data_ptr.add(MSG_HEADER_SIZE),
                    buf.as_mut_ptr(),
                    body_len,
                );
            }
            Body::Owned(buf)
        } else {
            Body::Empty
        };

        let mut resp = Response::with_status(status);
        resp.body = body;

        if headers_data_len > 0 {
            let headers_data = unsafe {
                std::slice::from_raw_parts(
                    data_ptr.add(MSG_HEADER_SIZE + body_len),
                    headers_data_len,
                )
            };
            resp.headers = crate::transport::deserialize_headers(headers_data)?;
        }

        Ok((resp, correlation_id))
    }
}

// ─── PubSubRpcServer ─────────────────────────────────────────────────────

/// Pub/sub-backed RPC server.
///
/// Uses dual pub/sub channels per client instead of coordination slots,
/// achieving ~25% lower latency than [`ShmServer`](super::ShmServer) for
/// small payloads and ~38% lower for large ones.
/// Same handler API — uses the same [`Router`] and handler traits.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// async fn health() -> &'static str { "ok" }
///
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let router = Router::new().route("/health", get(health));
/// let handle = PubSubRpcServer::spawn("fast", router).await?;
/// // Server runs in the background...
/// # Ok(())
/// # }
/// ```
pub struct PubSubRpcServer;

/// Per-client tracking state used by the server poll loop.
struct ClientState {
    known_active: bool,
    last_req_seq: u64,
    resp_seq: u64,
}

impl PubSubRpcServer {
    /// Creates the shared memory region and serves requests forever.
    pub async fn bind(name: &str, router: Router) -> io::Result<()> {
        Self::bind_with_config(name, router, PubSubRpcConfig::default()).await
    }

    /// Like [`bind`](Self::bind) but with custom config.
    pub async fn bind_with_config(
        name: &str,
        router: Router,
        config: PubSubRpcConfig,
    ) -> io::Result<()> {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Self::serve(name, router, config, stop).await
    }

    /// Spawns the server in a background thread and returns an [`ShmHandle`].
    pub async fn spawn(name: &str, router: Router) -> io::Result<ShmHandle> {
        Self::spawn_with_config(name, router, PubSubRpcConfig::default()).await
    }

    /// Like [`spawn`](Self::spawn) but with custom config.
    #[allow(clippy::unused_async)]
    pub async fn spawn_with_config(
        name: &str,
        router: Router,
        config: PubSubRpcConfig,
    ) -> io::Result<ShmHandle> {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = ShmHandle {
            stop: Arc::clone(&stop),
        };

        let region = Arc::new(Self::create_region(name, &config)?);
        region.update_heartbeat();

        Self::start_heartbeat(&region, &config, Arc::clone(&stop));
        Self::start_server_loop(region, router, stop);

        Ok(handle)
    }

    fn create_region(name: &str, config: &PubSubRpcConfig) -> io::Result<RpcRegion> {
        let path = shm_path(name);
        let lpath = lock_path(name);

        let lock_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lpath)?;

        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("pub/sub RPC region '{name}' is already active"),
            ));
        }

        // Remove stale file
        let _ = std::fs::remove_file(&path);

        let size = region_size(config);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        file.set_len(size as u64)?;

        let mmap = RawMmap::from_file_with_len(&file, size)?;

        // Write global header
        let ptr = mmap.as_mut_ptr();
        unsafe {
            std::ptr::copy_nonoverlapping(MAGIC.as_ptr(), ptr.add(GH_MAGIC), 8);
            (ptr.add(GH_VERSION) as *mut u32).write(VERSION);
            (ptr.add(GH_MAX_CLIENTS) as *mut u32).write(config.max_clients);
            (ptr.add(GH_BLOCK_COUNT) as *mut u32).write(config.block_count);
            (ptr.add(GH_BLOCK_SIZE) as *mut u32).write(config.block_size);
            (ptr.add(GH_RING_DEPTH) as *mut u32).write(config.ring_depth);
            (ptr.add(GH_PID) as *mut u64).write(u64::from(std::process::id()));
            #[allow(clippy::cast_possible_truncation)]
            (ptr.add(GH_STALE_TIMEOUT_US) as *mut u64)
                .write(config.stale_timeout.as_micros() as u64);
        }

        let pool_off = block_pool_offset(config);
        let region = RpcRegion {
            mmap,
            config: config.clone(),
            pool_offset: pool_off,
        };

        region.init_free_list();
        region.update_heartbeat();

        // Initialize all ring entries to NO_BLOCK
        for c in 0..config.max_clients {
            for s in 0..config.ring_depth {
                // Request ring
                let off = req_ring_entry_off(config, c, s);
                unsafe {
                    let base = region.mmap.as_mut_ptr().add(off);
                    (base.add(RE_SEQ) as *mut u64).write(0);
                    (base.add(RE_BLOCK_IDX) as *mut u32).write(NO_BLOCK);
                    (base.add(RE_DATA_LEN) as *mut u32).write(0);
                }
                // Response ring
                let off = resp_ring_entry_off(config, c, s);
                unsafe {
                    let base = region.mmap.as_mut_ptr().add(off);
                    (base.add(RE_SEQ) as *mut u64).write(0);
                    (base.add(RE_BLOCK_IDX) as *mut u32).write(NO_BLOCK);
                    (base.add(RE_DATA_LEN) as *mut u32).write(0);
                }
            }
        }

        // Keep lock file alive via leak (dropped when process exits)
        std::mem::forget(lock_file);

        Ok(region)
    }

    fn start_heartbeat(
        region: &Arc<RpcRegion>,
        config: &PubSubRpcConfig,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) {
        let hb_region = Arc::clone(region);
        let hb_interval = config.heartbeat_interval;
        std::thread::Builder::new()
            .name("crossbar-rpc-heartbeat".into())
            .spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    hb_region.update_heartbeat();
                    std::thread::sleep(hb_interval);
                }
            })
            .expect("failed to spawn heartbeat thread");
    }

    fn start_server_loop(
        region: Arc<RpcRegion>,
        router: Router,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) {
        let rt_handle = tokio::runtime::Handle::current();
        std::thread::Builder::new()
            .name("crossbar-rpc-server".into())
            .spawn(move || {
                Self::poll_loop(&region, &router, &rt_handle, &stop);
            })
            .expect("failed to spawn server thread");
    }

    async fn serve(
        name: &str,
        router: Router,
        config: PubSubRpcConfig,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) -> io::Result<()> {
        let region = Arc::new(Self::create_region(name, &config)?);
        region.update_heartbeat();

        Self::start_heartbeat(&region, &config, Arc::clone(&stop));

        let rt_handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            Self::poll_loop(&region, &router, &rt_handle, &stop);
        })
        .await
        .map_err(|e| io::Error::other(e.to_string()))?;

        Ok(())
    }

    /// Server-side spin iterations before falling back to futex wait.
    const SERVER_SPIN_ITERS: u32 = 2048;

    /// Try-poll dispatch: bypasses tokio for sync handlers.
    #[inline]
    fn dispatch_fast(
        router: &Router,
        req: Request,
        rt_handle: &tokio::runtime::Handle,
    ) -> Response {
        let mut fut = std::pin::pin!(router.dispatch(req));
        let _guard = rt_handle.enter();
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        match fut.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(resp) => resp,
            std::task::Poll::Pending => rt_handle.block_on(fut),
        }
    }

    fn poll_loop(
        region: &Arc<RpcRegion>,
        router: &Router,
        rt_handle: &tokio::runtime::Handle,
        stop: &std::sync::atomic::AtomicBool,
    ) {
        let max_clients = region.config.max_clients;
        let mut clients: Vec<ClientState> = (0..max_clients)
            .map(|_| ClientState {
                known_active: false,
                last_req_seq: 0,
                resp_seq: 0,
            })
            .collect();

        while !stop.load(Ordering::Acquire) {
            let mut found_work = false;

            for idx in 0..max_clients {
                let active = region.client_active(idx).load(Ordering::Acquire);
                let cs = &mut clients[idx as usize];

                if active == 0 {
                    if cs.known_active {
                        // Client disconnected
                        cs.known_active = false;
                        cs.last_req_seq = 0;
                        cs.resp_seq = 0;
                    }
                    continue;
                }

                if !cs.known_active {
                    // New client — start from seq 0 so we catch the first
                    // request (seq=1). Reading write_seq from SHM would race
                    // with the client's initialization.
                    cs.known_active = true;
                    cs.last_req_seq = 0;
                    cs.resp_seq = 0;
                }

                // Try to read next request from this client's ring
                if let Some((block_idx, data_len, seq)) =
                    region.try_recv_request(idx, cs.last_req_seq)
                {
                    found_work = true;
                    cs.last_req_seq = seq;

                    // Decode request — data is copied out, block can be reused
                    let (req, correlation_id) = match region.decode_request(block_idx, data_len) {
                        Ok(r) => r,
                        Err(_) => {
                            // Reuse request block for error response
                            let resp = Response::bad_request("malformed request");
                            let resp_len = region.encode_response(block_idx, &resp, 0).unwrap_or(0);
                            cs.resp_seq += 1;
                            region.publish_response(idx, block_idx, resp_len, cs.resp_seq);
                            Self::notify_client(region, idx);
                            continue;
                        }
                    };

                    // Dispatch
                    let resp = Self::dispatch_fast(router, req, rt_handle);

                    // Reuse request block for response (data already copied out)
                    let resp_len = match region.encode_response(block_idx, &resp, correlation_id) {
                        Ok(len) => len,
                        Err(_) => {
                            // Encode failed — send minimal error
                            let err_resp = Response::with_status(500);
                            region
                                .encode_response(block_idx, &err_resp, correlation_id)
                                .unwrap_or(0)
                        }
                    };
                    cs.resp_seq += 1;
                    region.publish_response(idx, block_idx, resp_len, cs.resp_seq);
                    Self::notify_client(region, idx);
                }
            }

            if !found_work {
                // Server-side spin: poll write_seqs directly (avoids
                // the client needing to bump server_notify on every request).
                let mut woke_via_spin = false;
                'spin: for _ in 0..Self::SERVER_SPIN_ITERS {
                    for idx in 0..max_clients {
                        let cs = &clients[idx as usize];
                        if !cs.known_active {
                            continue;
                        }
                        if region.client_req_write_seq(idx).load(Ordering::Acquire)
                            > cs.last_req_seq
                        {
                            woke_via_spin = true;
                            break 'spin;
                        }
                    }
                    core::hint::spin_loop();
                }
                if !woke_via_spin {
                    region.wait_for_work();
                }
            }
        }
    }

    /// Wake a client waiting for a response (smart wake).
    #[inline]
    fn notify_client(region: &RpcRegion, client_idx: u32) {
        let notify = region.client_resp_notify(client_idx);
        notify.fetch_add(1, Ordering::Release);
        if region
            .client_resp_waiters(client_idx)
            .load(Ordering::Acquire)
            > 0
        {
            notify::wake_all(notify);
        }
    }
}

// ─── PubSubRpcClient ─────────────────────────────────────────────────────

/// Pub/sub-backed RPC client.
///
/// Connects to an existing [`PubSubRpcServer`] region. Achieves ~25% lower
/// latency than [`ShmClient`](super::ShmClient) by using pub/sub rings
/// instead of coordination slots.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// # #[tokio::main] async fn main() -> Result<(), CrossbarError> {
/// let client = PubSubRpcClient::connect("fast").await?;
/// let resp = client.get("/health").await?;
/// println!("{}", resp.status);
/// # Ok(())
/// # }
/// ```
pub struct PubSubRpcClient {
    region: Arc<RpcRegion>,
    client_idx: u32,
    stale_timeout: Duration,
    next_req_seq: AtomicU64,
    last_resp_seq: AtomicU64,
    next_correlation: AtomicU32,
    request_count: AtomicU32,
}

impl PubSubRpcClient {
    /// Connects to an existing pub/sub RPC region.
    pub async fn connect(name: &str) -> Result<Self, CrossbarError> {
        Self::connect_with_timeout(name, Duration::from_secs(5)).await
    }

    /// Connects with a custom stale timeout.
    #[allow(clippy::unused_async)]
    pub async fn connect_with_timeout(
        name: &str,
        stale_timeout: Duration,
    ) -> Result<Self, CrossbarError> {
        let path = shm_path(name);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(CrossbarError::Io)?;

        let mmap = RawMmap::from_file(&file).map_err(CrossbarError::Io)?;

        if mmap.len() < HEADER_SIZE {
            return Err(CrossbarError::ShmInvalidRegion(
                "region too small for header".into(),
            ));
        }

        // Validate header
        let ptr = mmap.as_ptr();
        unsafe {
            let mut magic = [0u8; 8];
            std::ptr::copy_nonoverlapping(ptr.add(GH_MAGIC), magic.as_mut_ptr(), 8);
            if &magic != MAGIC {
                return Err(CrossbarError::ShmInvalidRegion(
                    "invalid magic (expected XBAR_RC)".into(),
                ));
            }
            let ver = (ptr.add(GH_VERSION) as *const u32).read();
            if ver != VERSION {
                return Err(CrossbarError::ShmInvalidRegion(format!(
                    "unsupported version {ver}, expected {VERSION}"
                )));
            }
        }

        // Read config from header
        let config = unsafe {
            PubSubRpcConfig {
                max_clients: (ptr.add(GH_MAX_CLIENTS) as *const u32).read(),
                block_count: (ptr.add(GH_BLOCK_COUNT) as *const u32).read(),
                block_size: (ptr.add(GH_BLOCK_SIZE) as *const u32).read(),
                ring_depth: (ptr.add(GH_RING_DEPTH) as *const u32).read(),
                stale_timeout: Duration::from_micros(
                    (ptr.add(GH_STALE_TIMEOUT_US) as *const u64).read(),
                ),
                ..PubSubRpcConfig::default()
            }
        };

        let expected_size = region_size(&config);
        if mmap.len() < expected_size {
            return Err(CrossbarError::ShmInvalidRegion(format!(
                "region size {} < expected {expected_size}",
                mmap.len()
            )));
        }

        let pool_off = block_pool_offset(&config);
        let region = Arc::new(RpcRegion {
            mmap,
            config,
            pool_offset: pool_off,
        });

        region.check_heartbeat()?;

        // Register in client table (CAS active from 0 to 1)
        let client_idx = Self::register_client(&region)?;

        // Generate client ID
        #[allow(clippy::cast_possible_truncation)]
        let client_id = u64::from(std::process::id())
            ^ (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64);

        // Write client metadata
        let entry_off = client_entry_off(client_idx);
        unsafe {
            let base = region.mmap.as_mut_ptr().add(entry_off);
            (base.add(CE_CLIENT_PID) as *mut u32).write(std::process::id());
            (base.add(CE_CLIENT_ID) as *mut u64).write(client_id);
        }

        Ok(PubSubRpcClient {
            region,
            client_idx,
            stale_timeout,
            next_req_seq: AtomicU64::new(0),
            last_resp_seq: AtomicU64::new(0),
            next_correlation: AtomicU32::new(0),
            request_count: AtomicU32::new(0),
        })
    }

    fn register_client(region: &RpcRegion) -> Result<u32, CrossbarError> {
        for i in 0..region.config.max_clients {
            let active = region.client_active(i);
            if active
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // Initialize write seqs to 0
                region.client_req_write_seq(i).store(0, Ordering::Release);
                region.client_resp_write_seq(i).store(0, Ordering::Release);
                return Ok(i);
            }
        }
        Err(CrossbarError::ShmSlotsFull)
    }

    /// Number of inline spin iterations before falling back to futex wait.
    const SPIN_ITERS: u32 = 2048;
    const YIELD_ITERS: u32 = 16;

    /// Sends a request and waits for the response.
    pub async fn request(&self, req: Request) -> Result<Response, CrossbarError> {
        // Counter-based heartbeat: check every 1024 requests
        let count = self
            .request_count
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        if count & 0x3FF == 0 {
            self.region.check_heartbeat()?;
        }

        // Allocate block
        let block_idx = self
            .region
            .alloc_block()
            .ok_or(CrossbarError::ShmPoolExhausted)?;

        // Encode request
        let correlation_id = self.next_correlation.fetch_add(1, Ordering::Relaxed);
        let data_len = match self.region.encode_request(block_idx, &req, correlation_id) {
            Ok(len) => len,
            Err(e) => {
                self.region.free_block(block_idx);
                return Err(e);
            }
        };

        // Publish to request ring
        let seq = self.next_req_seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.region
            .publish_request(self.client_idx, block_idx, data_len, seq);

        // Notify server
        self.region.notify_server();

        // ── Fast path: tight spin on write_seq (one atomic load per iter) ──
        let resp_write_seq = self.region.client_resp_write_seq(self.client_idx);
        let expected_seq = self.last_resp_seq.load(Ordering::Relaxed) + 1;
        for _ in 0..Self::SPIN_ITERS {
            if resp_write_seq.load(Ordering::Acquire) >= expected_seq {
                // Response available — decode and return
                if let Some(resp) = self.try_recv_response_inner()? {
                    return Ok(resp);
                }
            }
            core::hint::spin_loop();
        }

        // ── Yield phase ──
        for _ in 0..Self::YIELD_ITERS {
            if resp_write_seq.load(Ordering::Acquire) >= expected_seq {
                if let Some(resp) = self.try_recv_response_inner()? {
                    return Ok(resp);
                }
            }
            std::thread::yield_now();
        }

        // ── Slow path: futex wait ──
        let deadline = std::time::Instant::now() + self.stale_timeout;
        loop {
            if let Some(resp) = self.try_recv_response_inner()? {
                return Ok(resp);
            }

            if std::time::Instant::now() >= deadline {
                return Err(CrossbarError::ShmServerDead);
            }

            let notify = self.region.client_resp_notify(self.client_idx);
            let cur = notify.load(Ordering::Acquire);

            // Re-check before sleeping
            if let Some(resp) = self.try_recv_response_inner()? {
                return Ok(resp);
            }

            let waiters = self.region.client_resp_waiters(self.client_idx);
            waiters.fetch_add(1, Ordering::AcqRel);

            // Re-check after incrementing waiters (avoid missed wake)
            if let Some(resp) = self.try_recv_response_inner()? {
                waiters.fetch_sub(1, Ordering::Release);
                return Ok(resp);
            }

            let _ = notify::wait_until_not(notify, cur, Duration::from_millis(1));
            waiters.fetch_sub(1, Ordering::Release);
        }
    }

    #[inline]
    fn try_recv_response_inner(&self) -> Result<Option<Response>, CrossbarError> {
        let last_seq = self.last_resp_seq.load(Ordering::Relaxed);
        if let Some((block_idx, data_len, seq)) =
            self.region.try_recv_response(self.client_idx, last_seq)
        {
            self.last_resp_seq.store(seq, Ordering::Relaxed);
            let (resp, _corr_id) = self.region.decode_response(block_idx, data_len)?;
            self.region.free_block(block_idx);
            return Ok(Some(resp));
        }
        Ok(None)
    }

    /// Convenience method for `GET` requests.
    pub async fn get(&self, uri: &str) -> Result<Response, CrossbarError> {
        self.request(Request::new(Method::Get, uri)).await
    }

    /// Convenience method for `POST` requests with a body.
    pub async fn post(&self, uri: &str, body: impl Into<Body>) -> Result<Response, CrossbarError> {
        self.request(Request::new(Method::Post, uri).with_body(body))
            .await
    }
}

impl Drop for PubSubRpcClient {
    fn drop(&mut self) {
        // Deregister from client table
        self.region
            .client_active(self.client_idx)
            .store(0, Ordering::Release);
    }
}
