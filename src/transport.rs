//! Transport adapters: memory, channel, Unix Domain Socket, and TCP.
//!
//! Every transport exposes the same conceptual API: a **server** that binds to
//! some address and dispatches incoming requests through a [`Router`], and a
//! **client** that sends [`Request`]s and receives [`Response`]s.
//!
//! | Transport | Typical latency | Notes |
//! |-----------|-----------------|-------|
//! | [`MemoryClient`] | sub-µs | In-process, bypasses framing entirely |
//! | [`ChannelClient`] | 1–5 µs | tokio `mpsc` channel, cross-task |
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

use crate::error::CrossbarError;
use crate::router::Router;
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
fn serialize_headers(headers: &HashMap<String, String>) -> Vec<u8> {
    let num = headers.len() as u16;
    let mut out = Vec::new();
    out.extend_from_slice(&num.to_le_bytes());
    for (k, v) in headers {
        let kb = k.as_bytes();
        let vb = v.as_bytes();
        out.extend_from_slice(&(kb.len() as u16).to_le_bytes());
        out.extend_from_slice(kb);
        out.extend_from_slice(&(vb.len() as u16).to_le_bytes());
        out.extend_from_slice(vb);
    }
    out
}

/// Deserializes a header map from the wire format.
///
/// Returns an error if the data is malformed.
fn deserialize_headers(data: &[u8]) -> Result<HashMap<String, String>, CrossbarError> {
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
async fn write_request<W: AsyncWriteExt + Unpin>(w: &mut W, req: &Request) -> io::Result<()> {
    let uri_bytes = req.uri.raw().as_bytes();
    let headers_data = serialize_headers(&req.headers);
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
async fn read_request<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<Request, CrossbarError> {
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
async fn write_response<W: AsyncWriteExt + Unpin>(w: &mut W, resp: &Response) -> io::Result<()> {
    let headers_data = serialize_headers(&resp.headers);
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
async fn read_response<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<Response, CrossbarError> {
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

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 1. MemoryClient — in-process, zero-overhead
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// In-process client that dispatches directly to the router with no framing.
///
/// This is the fastest transport — there is no serialization, no I/O, and no
/// inter-thread communication.  Use it for in-process service-to-service calls
/// or for testing handlers without standing up a server.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// async fn ping() -> &'static str { "pong" }
///
/// # #[tokio::main] async fn main() {
/// let router = Router::new().route("/ping", get(ping));
/// let client = MemoryClient::new(router);
/// let resp = client.get("/ping").await;
/// assert_eq!(resp.status, 200);
/// # }
/// ```
#[derive(Clone)]
pub struct MemoryClient {
    router: Router,
}

impl MemoryClient {
    /// Wraps a [`Router`] in a [`MemoryClient`].
    #[must_use]
    pub fn new(router: Router) -> Self {
        MemoryClient { router }
    }

    /// Dispatches `req` directly through the router and returns the response.
    pub async fn request(&self, req: Request) -> Response {
        self.router.dispatch(req).await
    }

    /// Convenience method for `GET` requests.
    pub async fn get(&self, uri: &str) -> Response {
        self.request(Request::new(Method::Get, uri)).await
    }

    /// Convenience method for `POST` requests with a body.
    pub async fn post(&self, uri: &str, body: impl Into<Bytes>) -> Response {
        self.request(Request::new(Method::Post, uri).with_body(body))
            .await
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 2. UDS — Unix Domain Socket (Unix only)
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Unix Domain Socket server.
///
/// Binds to a socket file and serves requests indefinitely.  Each accepted
/// connection is handled in its own tokio task.  Multiple requests can be
/// pipelined over a single persistent connection (keep-alive).
///
/// Only available on Unix targets.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// async fn health() -> &'static str { "ok" }
///
/// # #[tokio::main] async fn main() {
/// let router = Router::new().route("/health", get(health));
/// // Runs forever; typically spawned in a background task.
/// UdsServer::bind("/tmp/app.sock", router).await.unwrap();
/// # }
/// ```
#[cfg(unix)]
pub struct UdsServer;

#[cfg(unix)]
impl UdsServer {
    /// Binds to `path` and serves requests forever.
    ///
    /// Any pre-existing file at `path` is removed before binding.  Each
    /// accepted connection spawns a tokio task that handles requests until the
    /// client closes the connection.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the socket cannot be created or if
    /// `accept()` fails fatally.
    pub async fn bind(path: &str, router: Router) -> io::Result<()> {
        let _ = std::fs::remove_file(path);
        let listener = tokio::net::UnixListener::bind(path)?;

        loop {
            let (stream, _) = listener.accept().await?;
            let router = router.clone();
            tokio::spawn(async move {
                let (mut rd, mut wr) = tokio::io::split(stream);
                while let Ok(req) = read_request(&mut rd).await {
                    let resp = router.dispatch(req).await;
                    if write_response(&mut wr, &resp).await.is_err() {
                        break;
                    }
                }
            });
        }
    }
}

/// Persistent Unix Domain Socket client.
///
/// Connects once and reuses the connection for all subsequent requests.
/// Access is serialized through a [`tokio::sync::Mutex`], so the client can
/// be shared across tasks by wrapping it in an [`Arc`](std::sync::Arc).
///
/// Only available on Unix targets.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// # #[tokio::main] async fn main() -> Result<(), CrossbarError> {
/// let client = UdsClient::connect("/tmp/app.sock").await?;
/// let resp = client.get("/health").await?;
/// println!("{}", resp.status);
/// # Ok(())
/// # }
/// ```
#[cfg(unix)]
pub struct UdsClient {
    inner: tokio::sync::Mutex<UdsConn>,
}

#[cfg(unix)]
struct UdsConn {
    rd: tokio::io::ReadHalf<tokio::net::UnixStream>,
    wr: tokio::io::WriteHalf<tokio::net::UnixStream>,
}

#[cfg(unix)]
impl UdsClient {
    /// Connects to the Unix Domain Socket at `path`.
    ///
    /// # Errors
    ///
    /// Returns a [`CrossbarError::Io`] if the connection cannot be established.
    pub async fn connect(path: &str) -> Result<Self, CrossbarError> {
        let stream = tokio::net::UnixStream::connect(path).await?;
        let (rd, wr) = tokio::io::split(stream);
        Ok(UdsClient {
            inner: tokio::sync::Mutex::new(UdsConn { rd, wr }),
        })
    }

    /// Sends `req` over the socket and returns the response.
    ///
    /// # Errors
    ///
    /// Returns a [`CrossbarError`] on I/O failure or framing errors.
    pub async fn request(&self, req: Request) -> Result<Response, CrossbarError> {
        let mut conn = self.inner.lock().await;
        write_request(&mut conn.wr, &req).await?;
        read_response(&mut conn.rd).await
    }

    /// Convenience method for `GET` requests.
    ///
    /// # Errors
    ///
    /// Returns a [`CrossbarError`] on I/O failure.
    pub async fn get(&self, uri: &str) -> Result<Response, CrossbarError> {
        self.request(Request::new(Method::Get, uri)).await
    }

    /// Convenience method for `POST` requests with a body.
    ///
    /// # Errors
    ///
    /// Returns a [`CrossbarError`] on I/O failure.
    pub async fn post(&self, uri: &str, body: impl Into<Bytes>) -> Result<Response, CrossbarError> {
        self.request(Request::new(Method::Post, uri).with_body(body))
            .await
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 3. TCP — raw binary over TCP
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// TCP server with `TCP_NODELAY` enabled for low-latency operation.
///
/// Binds to the given address and serves requests indefinitely.  Each accepted
/// connection is handled in its own tokio task with keep-alive semantics.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// async fn health() -> &'static str { "ok" }
///
/// # #[tokio::main] async fn main() {
/// let router = Router::new().route("/health", get(health));
/// TcpServer::bind("127.0.0.1:8080", router).await.unwrap();
/// # }
/// ```
pub struct TcpServer;

impl TcpServer {
    /// Binds to `addr` and serves requests forever.
    ///
    /// Sets `TCP_NODELAY` on every accepted connection to disable Nagle's
    /// algorithm and reduce per-request latency.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the listener cannot be bound or if
    /// `accept()` fails fatally.
    pub async fn bind(addr: &str, router: Router) -> io::Result<()> {
        let listener = tokio::net::TcpListener::bind(addr).await?;

        loop {
            let (stream, _) = listener.accept().await?;
            stream.set_nodelay(true)?;
            let router = router.clone();
            tokio::spawn(async move {
                let (mut rd, mut wr) = tokio::io::split(stream);
                while let Ok(req) = read_request(&mut rd).await {
                    let resp = router.dispatch(req).await;
                    if write_response(&mut wr, &resp).await.is_err() {
                        break;
                    }
                }
            });
        }
    }
}

/// Persistent TCP client with `TCP_NODELAY` enabled.
///
/// Connects once and reuses the connection for all subsequent requests.
/// Access is serialized through a [`tokio::sync::Mutex`].
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// # #[tokio::main] async fn main() -> Result<(), CrossbarError> {
/// let client = TcpClient::connect("127.0.0.1:8080").await?;
/// let resp = client.get("/health").await?;
/// println!("{}", resp.status);
/// # Ok(())
/// # }
/// ```
pub struct TcpClient {
    inner: tokio::sync::Mutex<TcpConn>,
}

struct TcpConn {
    rd: tokio::io::ReadHalf<tokio::net::TcpStream>,
    wr: tokio::io::WriteHalf<tokio::net::TcpStream>,
}

impl TcpClient {
    /// Connects to the TCP server at `addr` with `TCP_NODELAY` enabled.
    ///
    /// # Errors
    ///
    /// Returns a [`CrossbarError::Io`] if the connection cannot be established.
    pub async fn connect(addr: &str) -> Result<Self, CrossbarError> {
        let stream = tokio::net::TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let (rd, wr) = tokio::io::split(stream);
        Ok(TcpClient {
            inner: tokio::sync::Mutex::new(TcpConn { rd, wr }),
        })
    }

    /// Sends `req` over the TCP connection and returns the response.
    ///
    /// # Errors
    ///
    /// Returns a [`CrossbarError`] on I/O failure or framing errors.
    pub async fn request(&self, req: Request) -> Result<Response, CrossbarError> {
        let mut conn = self.inner.lock().await;
        write_request(&mut conn.wr, &req).await?;
        read_response(&mut conn.rd).await
    }

    /// Convenience method for `GET` requests.
    ///
    /// # Errors
    ///
    /// Returns a [`CrossbarError`] on I/O failure.
    pub async fn get(&self, uri: &str) -> Result<Response, CrossbarError> {
        self.request(Request::new(Method::Get, uri)).await
    }

    /// Convenience method for `POST` requests with a body.
    ///
    /// # Errors
    ///
    /// Returns a [`CrossbarError`] on I/O failure.
    pub async fn post(&self, uri: &str, body: impl Into<Bytes>) -> Result<Response, CrossbarError> {
        self.request(Request::new(Method::Post, uri).with_body(body))
            .await
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 4. Channel — tokio::mpsc, cross-task dispatch
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Spawns a background task that processes requests sent via a [`ChannelClient`].
///
/// This transport is ideal for cross-task communication within the same process
/// where you need to serialize access to the router from multiple concurrent
/// callers.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// async fn health() -> &'static str { "ok" }
///
/// # #[tokio::main] async fn main() -> Result<(), CrossbarError> {
/// let router = Router::new().route("/health", get(health));
/// let client = ChannelServer::spawn(router);
/// let resp = client.get("/health").await?;
/// assert_eq!(resp.status, 200);
/// # Ok(())
/// # }
/// ```
pub struct ChannelServer;

impl ChannelServer {
    /// Spawns a background task and returns a [`ChannelClient`] connected to it.
    ///
    /// The background task runs until the last [`ChannelClient`] clone is
    /// dropped, at which point the channel closes and the task exits cleanly.
    pub fn spawn(router: Router) -> ChannelClient {
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<(Request, tokio::sync::oneshot::Sender<Response>)>(256);

        tokio::spawn(async move {
            while let Some((req, reply_tx)) = rx.recv().await {
                let resp = router.dispatch(req).await;
                let _ = reply_tx.send(resp);
            }
        });

        ChannelClient { tx }
    }
}

/// Client that sends requests to a [`ChannelServer`] background task.
///
/// Cheaply clonable — each clone shares the same underlying sender.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// async fn health() -> &'static str { "ok" }
///
/// # #[tokio::main] async fn main() -> Result<(), CrossbarError> {
/// let router = Router::new().route("/health", get(health));
/// let client = ChannelServer::spawn(router);
/// let resp = client.get("/health").await?;
/// println!("{}", resp.status);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct ChannelClient {
    tx: tokio::sync::mpsc::Sender<(Request, tokio::sync::oneshot::Sender<Response>)>,
}

impl ChannelClient {
    /// Sends `req` to the background task and awaits the response.
    ///
    /// # Errors
    ///
    /// Returns [`CrossbarError::ServerDropped`] if the background task has
    /// exited (i.e. the receiving end of the channel was dropped).
    pub async fn request(&self, req: Request) -> Result<Response, CrossbarError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send((req, reply_tx))
            .await
            .map_err(|_| CrossbarError::ServerDropped)?;
        reply_rx.await.map_err(|_| CrossbarError::ServerDropped)
    }

    /// Convenience method for `GET` requests.
    ///
    /// # Errors
    ///
    /// Returns [`CrossbarError::ServerDropped`] if the background task has exited.
    pub async fn get(&self, uri: &str) -> Result<Response, CrossbarError> {
        self.request(Request::new(Method::Get, uri)).await
    }

    /// Convenience method for `POST` requests with a body.
    ///
    /// # Errors
    ///
    /// Returns [`CrossbarError::ServerDropped`] if the background task has exited.
    pub async fn post(&self, uri: &str, body: impl Into<Bytes>) -> Result<Response, CrossbarError> {
        self.request(Request::new(Method::Post, uri).with_body(body))
            .await
    }
}
