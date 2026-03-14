//! # Crossbar
//!
//! **Transport-polymorphic URI router** -- define your handlers once and serve
//! them over in-process or shared memory, all with the same API.
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
//!     // Build the router once -- same for every transport.
//!     let router = Router::new()
//!         .route("/health", get(health))
//!         .route("/echo", post(echo));
//!
//!     // -- In-process (zero overhead) ---
//!     let mem = InProcessClient::new(router.clone());
//!     let resp = mem.get("/health").await;
//!     assert_eq!(resp.status, 200);
//!
//!     // -- shared memory (shm feature, ~2-5 us) ---
//!     // Enable with: cargo add crossbar --features shm
//!     // ShmServer::bind("myapp", router.clone()).await?;
//!     // let shm = ShmClient::connect("myapp").await?;
//!     // let resp = shm.get("/health").await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Shared Memory
//!
//! `ShmServer` and `ShmClient` are gated behind the `shm` Cargo feature and
//! are only available on Unix targets. Enable with
//! `cargo add crossbar --features shm`. On Linux, the shared memory region
//! lives at `/dev/shm/crossbar-{name}`; on other Unix platforms it falls back
//! to `/tmp/crossbar-shm-{name}`.

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
