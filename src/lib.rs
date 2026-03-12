//! # Crossbar
//!
//! **Transport-polymorphic URI router** — define your handlers once and serve
//! them over in-process memory, a tokio channel, shared memory, a Unix Domain
//! Socket, or plain TCP, all with the same API.
//!
//! Crossbar is designed for low-latency, high-throughput Rust applications
//! (trading systems, game servers, inter-process bridges) that need to swap
//! transport layers without touching business logic.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use crossbar::prelude::*;
//! use serde::Serialize;
//!
//! #[derive(Serialize)]
//! struct Health { status: &'static str }
//!
//! async fn health() -> Json<Health> {
//!     Json(Health { status: "ok" })
//! }
//!
//! async fn echo(req: Request) -> String {
//!     format!("you sent: {}", std::str::from_utf8(&req.body).unwrap_or("<binary>"))
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Build the router once — same for every transport.
//!     let router = Router::new()
//!         .route("/health", get(health))
//!         .route("/echo", post(echo));
//!
//!     // ── In-process (zero overhead) ───────────────────
//!     let mem = MemoryClient::new(router.clone());
//!     let resp = mem.get("/health").await;
//!     assert_eq!(resp.status, 200);
//!
//!     // ── tokio channel (~1-5 µs) ──────────────────────
//!     let chan = ChannelServer::spawn(router.clone());
//!     let resp = chan.get("/health").await?;
//!     assert_eq!(resp.status, 200);
//!
//!     // ── shared memory (shm feature, ~2-5 µs) ────────
//!     // Enable with: cargo add crossbar --features shm
//!     // ShmServer::bind("myapp", router.clone()).await?;
//!     // let shm = ShmClient::connect("myapp").await?;
//!     // let resp = shm.get("/health").await?;
//!
//!     // ── TCP ──────────────────────────────────────────
//!     let addr = "127.0.0.1:18800";
//!     {
//!         let r = router;
//!         let a = addr.to_string();
//!         tokio::spawn(async move { TcpServer::bind(&a, r).await.unwrap() });
//!     }
//!     tokio::time::sleep(std::time::Duration::from_millis(10)).await;
//!     let tcp = TcpClient::connect(addr).await?;
//!     let resp = tcp.get("/health").await?;
//!     assert_eq!(resp.status, 200);
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Wire Protocol
//!
//! The binary framing used by the UDS and TCP transports is intentionally
//! minimal:
//!
//! ```text
//! Request (13-byte fixed header):
//!   [1B method][4B uri_len LE][4B body_len LE][4B headers_data_len LE]
//!   [uri bytes][headers data][body bytes]
//!
//! Response (10-byte fixed header):
//!   [2B status LE][4B body_len LE][4B headers_data_len LE]
//!   [headers data][body bytes]
//! ```
//!
//! Both sides share the internal `read_request` / `write_response` helpers,
//! so adding a new transport is just a matter of providing an
//! `AsyncRead + AsyncWrite` pair.
//!
//! ## Shared Memory
//!
//! [`ShmServer`](transport::ShmServer) and [`ShmClient`](transport::ShmClient)
//! are gated behind the `shm` Cargo feature. Enable with
//! `cargo add crossbar --features shm`. On Linux, the shared memory region
//! lives at `/dev/shm/crossbar-{name}`.
//!
//! ## Unix Domain Socket
//!
//! [`UdsServer`](transport::UdsServer) and [`UdsClient`](transport::UdsClient)
//! are gated with `#[cfg(unix)]` and are only available on Unix platforms.
//! On other platforms use [`TcpClient`](transport::TcpClient) /
//! [`TcpServer`](transport::TcpServer) or the in-process transports.

#![warn(missing_docs)]
#![deny(unsafe_code)]

pub mod error;
pub mod handler;
pub mod router;
pub mod transport;
pub mod types;

/// Re-export procedural macros from `crossbar-macros`.
pub use crossbar_macros::{handler, IntoResponse};

/// Convenient re-exports of everything you need for typical usage.
///
/// ```rust
/// use crossbar::prelude::*;
/// ```
pub mod prelude {
    pub use crate::error::CrossbarError;
    pub use crate::handler::*;
    pub use crate::router::*;
    pub use crate::transport::*;
    pub use crate::types::*;
    pub use crate::{handler, IntoResponse};
}
