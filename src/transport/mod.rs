//! Transport adapters: memory, channel, Unix Domain Socket, TCP, and shared memory.
//!
//! Every transport exposes the same conceptual API: a **server** that binds to
//! some address and dispatches incoming requests through a
//! [`Router`](crate::router::Router), and a **client** that sends
//! [`Request`]s and receives [`Response`]s.
//!
//! | Transport | Typical latency | Notes |
//! |-----------|-----------------|-------|
//! | [`MemoryClient`] | sub-µs | In-process, bypasses framing entirely |
//! | [`ChannelClient`] | 1–5 µs | tokio `mpsc` channel, cross-task |
//! | `ShmClient` | 2–5 µs | Shared memory via `/dev/shm` (`shm` feature) |
//! | [`UdsClient`] | 10–50 µs | Unix Domain Socket, same host (Unix only) |
//! | [`TcpClient`] | 50–100 µs | Raw TCP, `TCP_NODELAY` enabled |
//!
//! ## Wire Protocol (UDS and TCP)
//!
//! ```text
//! Request (13-byte fixed header):
//!   [1B method][4B uri_len LE][4B body_len LE][4B headers_data_len LE]
//!   [uri bytes][headers data][body bytes]
//!
//! Response (10-byte fixed header):
//!   [2B status LE][4B body_len LE][4B headers_data_len LE]
//!   [headers data][body bytes]
//!
//! Headers data format:
//!   [2B num_headers LE]
//!   for each header:
//!     [2B key_len LE][key bytes][2B val_len LE][val bytes]
//! ```
//!
//! Both sides use zero-copy [`bytes::BytesMut`] reads: the URI, headers data,
//! and body are read into a single allocation, then split into [`bytes::Bytes`]
//! slices without copying.
//!
//! ## Unix Domain Socket availability
//!
//! [`UdsServer`] and [`UdsClient`] are only available on Unix targets
//! (gated with `#[cfg(unix)]`).  On other platforms use [`TcpClient`] /
//! [`TcpServer`] or the in-process transports.

mod channel;
mod memory;
#[cfg(all(unix, feature = "shm"))]
mod shm;
mod tcp;
#[cfg(unix)]
mod uds;

pub use channel::{ChannelClient, ChannelServer};
pub use memory::MemoryClient;
#[cfg(all(unix, feature = "shm"))]
pub use shm::{
    PubSubConfig, ShmClient, ShmConfig, ShmHandle, ShmLoan, ShmPublisher, ShmSample, ShmSampleRef,
    ShmServer, ShmSubscriber, ShmSubscription, TopicHandle,
};
pub use tcp::{TcpClient, TcpServer};
#[cfg(unix)]
pub use uds::{UdsClient, UdsServer};

use crate::error::CrossbarError;
use crate::types::{Method, Request, Response};
use bytes::{Bytes, BytesMut};
use std::collections::HashMap;
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ── Frame size limit ─────────────────────────────────

/// Maximum total frame size (URI + headers + body) accepted by the wire protocol.
///
/// Defaults to 64 MiB. Frames exceeding this limit are rejected with
/// [`CrossbarError::FrameTooLarge`].
pub const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

// ── Headers serialization helpers ────────────────────

/// Serializes a header map into the wire format.
///
/// ```text
/// [2B num_headers LE]
/// for each header:
///   [2B key_len LE][key bytes][2B val_len LE][val bytes]
/// ```
pub(crate) fn serialize_headers(
    headers: &HashMap<String, String>,
) -> Result<Vec<u8>, CrossbarError> {
    if headers.len() > u16::MAX as usize {
        return Err(CrossbarError::HeaderOverflow(format!(
            "header count {} exceeds u16::MAX",
            headers.len()
        )));
    }
    let num = headers.len() as u16;
    let mut out = Vec::new();
    out.extend_from_slice(&num.to_le_bytes());
    for (k, v) in headers {
        let kb = k.as_bytes();
        let vb = v.as_bytes();
        if kb.len() > u16::MAX as usize {
            return Err(CrossbarError::HeaderOverflow(format!(
                "header key length {} exceeds u16::MAX",
                kb.len()
            )));
        }
        if vb.len() > u16::MAX as usize {
            return Err(CrossbarError::HeaderOverflow(format!(
                "header value length {} exceeds u16::MAX",
                vb.len()
            )));
        }
        out.extend_from_slice(&(kb.len() as u16).to_le_bytes());
        out.extend_from_slice(kb);
        out.extend_from_slice(&(vb.len() as u16).to_le_bytes());
        out.extend_from_slice(vb);
    }
    Ok(out)
}

/// Deserializes a header map from the wire format.
///
/// Returns an error if the data is malformed.
pub(crate) fn deserialize_headers(data: &[u8]) -> Result<HashMap<String, String>, CrossbarError> {
    if data.len() < 2 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "headers data too short").into());
    }
    let num = u16::from_le_bytes([data[0], data[1]]) as usize;
    let mut pos = 2;
    let mut headers = HashMap::with_capacity(num);

    for _ in 0..num {
        if pos + 2 > data.len() {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "truncated header key length").into(),
            );
        }
        let key_len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;

        if pos + key_len > data.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated header key").into());
        }
        let key = std::str::from_utf8(&data[pos..pos + key_len])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "header key not UTF-8"))?
            .to_string();
        pos += key_len;

        if pos + 2 > data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated header value length",
            )
            .into());
        }
        let val_len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;

        if pos + val_len > data.len() {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "truncated header value").into(),
            );
        }
        let val = std::str::from_utf8(&data[pos..pos + val_len])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "header value not UTF-8"))?
            .to_string();
        pos += val_len;

        headers.insert(key, val);
    }

    Ok(headers)
}

// ── Wire framing helpers ─────────────────────────────

/// Serializes `req` onto the writer using the crossbar binary framing.
///
/// ```text
/// [1B method][4B uri_len LE][4B body_len LE][4B headers_data_len LE]
/// [uri bytes][headers data][body bytes]
/// ```
pub(super) async fn write_request<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    req: &Request,
) -> io::Result<()> {
    let uri_bytes = req.uri.raw().as_bytes();
    let headers_data =
        serialize_headers(&req.headers).map_err(|e| io::Error::other(e.to_string()))?;
    let mut header = [0u8; 13];
    header[0] = u8::from(req.method);
    header[1..5].copy_from_slice(&(uri_bytes.len() as u32).to_le_bytes());
    header[5..9].copy_from_slice(&(req.body.len() as u32).to_le_bytes());
    header[9..13].copy_from_slice(&(headers_data.len() as u32).to_le_bytes());
    w.write_all(&header).await?;
    w.write_all(uri_bytes).await?;
    w.write_all(&headers_data).await?;
    w.write_all(&req.body).await?;
    w.flush().await
}

/// Deserializes a [`Request`] from the reader using the crossbar binary framing.
///
/// Reads the URI, headers data, and body into a single [`BytesMut`] allocation,
/// then freezes and splits so that all slices share the allocation without copying.
pub(super) async fn read_request<R: AsyncReadExt + Unpin>(
    r: &mut R,
) -> Result<Request, CrossbarError> {
    let mut header = [0u8; 13];
    r.read_exact(&mut header).await?;

    let method = Method::try_from(header[0]).map_err(CrossbarError::InvalidMethod)?;
    let uri_len = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
    let body_len = u32::from_le_bytes(header[5..9].try_into().unwrap()) as usize;
    let headers_data_len = u32::from_le_bytes(header[9..13].try_into().unwrap()) as usize;

    // Enforce frame size limit before allocating.
    let total = uri_len + headers_data_len + body_len;
    if total > MAX_FRAME_SIZE {
        return Err(CrossbarError::FrameTooLarge {
            size: total,
            max: MAX_FRAME_SIZE,
        });
    }

    // Single allocation for URI, headers data, and body bytes.
    let mut buf = BytesMut::with_capacity(total);
    buf.resize(total, 0);
    r.read_exact(&mut buf).await?;

    // Freeze the allocation and split into zero-copy slices.
    let mut frozen = buf.freeze();
    let uri_bytes = frozen.split_to(uri_len);
    let headers_bytes = frozen.split_to(headers_data_len);
    let body: Bytes = frozen; // remaining bytes are the body

    let uri_str = std::str::from_utf8(&uri_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "URI is not valid UTF-8"))?;

    let headers = deserialize_headers(&headers_bytes)?;

    let mut req = Request::new(method, uri_str).with_body(body);
    req.headers = headers;
    Ok(req)
}

/// Serializes `resp` onto the writer using the crossbar binary framing.
///
/// ```text
/// [2B status LE][4B body_len LE][4B headers_data_len LE]
/// [headers data][body bytes]
/// ```
pub(super) async fn write_response<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    resp: &Response,
) -> io::Result<()> {
    let headers_data =
        serialize_headers(&resp.headers).map_err(|e| io::Error::other(e.to_string()))?;
    let mut header = [0u8; 10];
    header[0..2].copy_from_slice(&resp.status.to_le_bytes());
    header[2..6].copy_from_slice(&(resp.body.len() as u32).to_le_bytes());
    header[6..10].copy_from_slice(&(headers_data.len() as u32).to_le_bytes());
    w.write_all(&header).await?;
    w.write_all(&headers_data).await?;
    w.write_all(&resp.body).await?;
    w.flush().await
}

/// Deserializes a [`Response`] from the reader using the crossbar binary framing.
///
/// The headers data and body are read into a single [`BytesMut`] that is frozen
/// to [`Bytes`] without copying.
pub(super) async fn read_response<R: AsyncReadExt + Unpin>(
    r: &mut R,
) -> Result<Response, CrossbarError> {
    let mut header = [0u8; 10];
    r.read_exact(&mut header).await?;
    let status = u16::from_le_bytes(header[0..2].try_into().unwrap());
    let body_len = u32::from_le_bytes(header[2..6].try_into().unwrap()) as usize;
    let headers_data_len = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;

    // Enforce frame size limit before allocating.
    let total = headers_data_len + body_len;
    if total > MAX_FRAME_SIZE {
        return Err(CrossbarError::FrameTooLarge {
            size: total,
            max: MAX_FRAME_SIZE,
        });
    }

    // Single allocation for headers data and body bytes.
    let mut buf = BytesMut::with_capacity(total);
    buf.resize(total, 0);
    r.read_exact(&mut buf).await?;

    // Freeze and split.
    let mut frozen = buf.freeze();
    let headers_bytes = frozen.split_to(headers_data_len);
    let body: Bytes = frozen;

    let headers = deserialize_headers(&headers_bytes)?;

    Ok(Response {
        status,
        headers,
        body,
    })
}
