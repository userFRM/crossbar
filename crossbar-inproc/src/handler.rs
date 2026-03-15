// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler trait and type-erased [`BoxedHandler`].
//!
//! Crossbar handlers are functions (or closures) that accept zero or one
//! [`Request`] argument and return any type implementing [`IntoResponse`].
//! The [`Handler`] trait is implemented automatically for matching function
//! signatures via blanket impls.
//!
//! You never need to implement [`Handler`] manually — just write a plain
//! function and pass it to [`Router::route`](crate::router::Router::route).
//!
//! # Supported Signatures
//!
//! ```rust,no_run
//! use crossbar_inproc::prelude::*;
//!
//! // Zero-argument handler (ignores the request)
//! fn ping() -> &'static str { "pong" }
//!
//! // One-argument handler receives the full request
//! fn echo(req: Request) -> String {
//!     format!("body: {}", std::str::from_utf8(&req.body).unwrap_or("<binary>"))
//! }
//!
//! let router = Router::new()
//!     .route("/ping", get(ping))
//!     .route("/echo", post(echo));
//! ```

use crate::types::{IntoResponse, Request, Response};
use std::sync::Arc;

// ── Handler trait ────────────────────────────────────────

/// Trait implemented by functions that can serve as route handlers.
///
/// The type parameter `T` is a marker for the argument tuple and prevents
/// conflicting blanket implementations.  You never interact with `T` directly.
///
/// Implemented automatically for:
/// - `fn() -> impl IntoResponse` (zero-argument handlers)
/// - `fn(Request) -> impl IntoResponse` (single-argument handlers)
pub trait Handler<T: 'static>: Send + Sync + 'static {
    /// Calls the handler with the given request and returns a [`Response`].
    fn call(&self, req: Request) -> Response;

    /// Wraps this handler into a [`BoxedHandler`] for storage in the router.
    fn boxed(self) -> BoxedHandler
    where
        Self: Sized,
    {
        BoxedHandler::new(self)
    }
}

// ── BoxedHandler (type-erased) ───────────────────────────

/// A type-erased, cheaply clonable wrapper around a [`Handler`].
///
/// [`BoxedHandler`] is what the router stores internally.  You create one via
/// [`BoxedHandler::new`] (or, more typically, via [`get`](crate::router::get),
/// [`post`](crate::router::post), etc.).
///
/// Cloning a [`BoxedHandler`] increments an `Arc` reference count — no heap
/// allocation occurs.
#[derive(Clone)]
pub struct BoxedHandler {
    f: Arc<dyn Fn(Request) -> Response + Send + Sync>,
}

impl BoxedHandler {
    /// Wraps a [`Handler`] implementation in a type-erased, heap-allocated
    /// container.
    ///
    /// Prefer using the free functions [`get`](crate::router::get),
    /// [`post`](crate::router::post), etc., which call this internally.
    #[must_use]
    pub fn new<H: Handler<T>, T: 'static>(handler: H) -> Self {
        let handler = Arc::new(handler);
        BoxedHandler {
            f: Arc::new(move |req| handler.call(req)),
        }
    }

    /// Invokes the underlying handler with the given request.
    #[inline]
    pub(crate) fn call(&self, req: Request) -> Response {
        (self.f)(req)
    }
}

// ── Blanket impls ────────────────────────────────────────

// 0 args: fn() -> impl IntoResponse
impl<F, R> Handler<()> for F
where
    F: Fn() -> R + Send + Sync + 'static,
    R: IntoResponse + 'static,
{
    fn call(&self, _req: Request) -> Response {
        (self)().into_response()
    }
}

// 1 arg: fn(Request) -> impl IntoResponse
impl<F, R> Handler<(Request,)> for F
where
    F: Fn(Request) -> R + Send + Sync + 'static,
    R: IntoResponse + 'static,
{
    fn call(&self, req: Request) -> Response {
        (self)(req).into_response()
    }
}
