use super::{read_request, write_response};
use crate::error::CrossbarError;
use crate::router::Router;
use crate::types::{Method, Request, Response};
use bytes::Bytes;
use std::io;

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// TCP — raw binary over TCP
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
        super::write_request(&mut conn.wr, &req).await?;
        super::read_response(&mut conn.rd).await
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
