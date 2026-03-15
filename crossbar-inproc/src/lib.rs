// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! # crossbar-inproc
//!
//! **In-process URI router with handler dispatch.**
//!
//! Register handlers by URI pattern, dispatch requests directly with no
//! serialization or I/O. ~143 ns for a `/health -> "ok"` roundtrip.
//!
//! ## Quick Start
//!
//! ```rust
//! use crossbar_inproc::prelude::*;
//!
//! fn health() -> &'static str { "ok" }
//!
//! let router = Router::new().route("/health", get(health));
//! let client = InProcessClient::new(router);
//! let resp = client.get("/health");
//! assert_eq!(resp.status, 200);
//! ```

#![warn(missing_docs)]
#![deny(unsafe_code)]

pub mod handler;
mod inproc;
pub mod router;
pub mod types;

/// Re-export procedural macros from `crossbar-macros`.
pub use crossbar_macros::handler;

/// Convenient re-exports of everything you need for typical usage.
pub mod prelude {
    pub use crate::handler::*;
    pub use crate::inproc::InProcessClient;
    pub use crate::router::*;
    pub use crate::types::*;
}

pub use inproc::InProcessClient;
