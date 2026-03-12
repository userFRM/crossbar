use super::{read_request, write_response};
use crate::error::CrossbarError;
use crate::router::Router;
use crate::types::{Method, Request, Response};
use bytes::Bytes;
use std::io;

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// UDS — Unix Domain Socket (Unix only)
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
pub struct UdsServer;

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
pub struct UdsClient {
    inner: tokio::sync::Mutex<UdsConn>,
}

struct UdsConn {
    rd: tokio::io::ReadHalf<tokio::net::UnixStream>,
    wr: tokio::io::WriteHalf<tokio::net::UnixStream>,
}

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
