// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler trait and type-erased [`BoxedHandler`].
//!
//! Crossbar handlers are async functions (or closures) that accept zero or one
//! [`Request`] argument and return any type implementing [`IntoResponse`].
//! The [`Handler`] trait is implemented automatically for matching function
//! signatures via blanket impls.
//!
//! You never need to implement [`Handler`] manually — just write a plain async
//! function and pass it to [`Router::route`](crate::router::Router::route).
//!
//! Synchronous functions are also supported via [`sync_handler`] and
//! [`sync_handler_with_req`], which wrap a plain `Fn` in an adapter struct
//! that implements [`Handler`].
//!
//! # Supported Signatures
//!
//! ```rust,no_run
//! use crossbar::prelude::*;
//!
//! // Zero-argument async handler (ignores the request)
//! async fn ping() -> &'static str { "pong" }
//!
//! // One-argument async handler receives the full request
//! async fn echo(req: Request) -> String {
//!     format!("body: {}", std::str::from_utf8(&req.body).unwrap_or("<binary>"))
//! }
//!
//! // Synchronous zero-argument handler
//! fn health() -> &'static str { "ok" }
//!
//! // Synchronous one-argument handler
//! fn body_len(req: Request) -> String {
//!     format!("{} bytes", req.body.len())
//! }
//!
//! let router = Router::new()
//!     .route("/ping", get(ping))
//!     .route("/echo", post(echo))
//!     .route("/health", get(sync_handler(health)))
//!     .route("/len", post(sync_handler_with_req(body_len)));
//! ```

use crate::types::{IntoResponse, Request, Response};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

// ── Handler trait ────────────────────────────────────────

/// Trait implemented by async functions that can serve as route handlers.
///
/// The type parameter `T` is a marker for the argument tuple and prevents
/// conflicting blanket implementations.  You never interact with `T` directly.
///
/// Implemented automatically for:
/// - `async fn() -> impl IntoResponse` (zero-argument handlers)
/// - `async fn(Request) -> impl IntoResponse` (single-argument handlers)
pub trait Handler<T: 'static>: Send + Sync + 'static {
    /// Calls the handler with the given request and returns a boxed future
    /// that resolves to a [`Response`].
    fn call(&self, req: Request) -> Pin<Box<dyn Future<Output = Response> + Send>>;

    /// Wraps this handler into a [`BoxedHandler`] for storage in the router.
    ///
    /// The default implementation uses the async path. Sync handler adapters
    /// override this to install a zero-alloc fast path.
    fn boxed(self) -> BoxedHandler
    where
        Self: Sized,
    {
        BoxedHandler::new_async(self)
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
pub struct BoxedHandler {
    f: Arc<DynHandler>,
    /// Optional sync fast path — skips `Box::pin` allocation entirely.
    /// Set for handlers created via [`sync_handler`] / [`sync_handler_with_req`].
    sync_f: Option<Arc<DynSyncHandler>>,
}

type DynHandler = dyn Fn(Request) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync;
type DynSyncHandler = dyn Fn(Request) -> Response + Send + Sync;

impl BoxedHandler {
    /// Wraps a [`Handler`] implementation in a type-erased, heap-allocated
    /// container.
    ///
    /// Prefer using the free functions [`get`](crate::router::get),
    /// [`post`](crate::router::post), etc., which call this internally.
    #[must_use]
    pub fn new<H: Handler<T>, T: 'static>(handler: H) -> Self {
        handler.boxed()
    }

    /// Default async-only boxing (used by `Handler::boxed` default impl).
    #[must_use]
    pub(crate) fn new_async<H: Handler<T>, T: 'static>(handler: H) -> Self {
        let handler = Arc::new(handler);
        BoxedHandler {
            f: Arc::new(move |req| handler.call(req)),
            sync_f: None,
        }
    }

    /// Wraps a sync handler with a direct call path that avoids `Box::pin`.
    #[must_use]
    pub(crate) fn new_sync<F>(sync_fn: F, async_fn: Arc<DynHandler>) -> Self
    where
        F: Fn(Request) -> Response + Send + Sync + 'static,
    {
        BoxedHandler {
            f: async_fn,
            sync_f: Some(Arc::new(sync_fn)),
        }
    }

    /// Tries to call the handler synchronously without `Box::pin`.
    ///
    /// Returns `Ok(response)` if a sync fast path is available, or
    /// `Err(request)` if the handler is async (request returned for reuse).
    #[inline]
    #[allow(clippy::result_large_err)] // Request is returned to caller on miss, not a real error
    pub(crate) fn try_call_sync(&self, req: Request) -> Result<Response, Request> {
        match &self.sync_f {
            Some(f) => Ok((f)(req)),
            None => Err(req),
        }
    }

    /// Invokes the underlying handler with the given request.
    #[inline]
    pub(crate) async fn call(&self, req: Request) -> Response {
        (self.f)(req).await
    }
}

impl Clone for BoxedHandler {
    fn clone(&self) -> Self {
        BoxedHandler {
            f: Arc::clone(&self.f),
            sync_f: self.sync_f.as_ref().map(Arc::clone),
        }
    }
}

// ── Blanket impls ────────────────────────────────────────

// 0 args: async fn() -> impl IntoResponse
impl<F, Fut, R> Handler<()> for F
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = R> + Send + 'static,
    R: IntoResponse + 'static,
{
    fn call(&self, _req: Request) -> Pin<Box<dyn Future<Output = Response> + Send>> {
        let fut = (self)();
        Box::pin(async move { fut.await.into_response() })
    }
}

// 1 arg: async fn(Request) -> impl IntoResponse
impl<F, Fut, R> Handler<(Request,)> for F
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = R> + Send + 'static,
    R: IntoResponse + 'static,
{
    fn call(&self, req: Request) -> Pin<Box<dyn Future<Output = Response> + Send>> {
        let fut = (self)(req);
        Box::pin(async move { fut.await.into_response() })
    }
}

// ── Synchronous handler support ────────────────────────

/// Adapter that turns a synchronous `Fn() -> R` into a [`Handler`].
///
/// Created via [`sync_handler`].  You should not need to name this type
/// directly — use `sync_handler(f)` and let the compiler infer the rest.
pub struct SyncFn0<F>(F);

/// Adapter that turns a synchronous `Fn(Request) -> R` into a [`Handler`].
///
/// Created via [`sync_handler_with_req`].  You should not need to name this
/// type directly — use `sync_handler_with_req(f)` and let the compiler infer
/// the rest.
pub struct SyncFn1<F>(F);

/// Wraps a synchronous zero-argument function for use as a route handler.
///
/// Use this when your handler does not need to be `async` — the function is
/// called directly and its return value is wrapped in a ready future.
///
/// # Examples
///
/// ```rust
/// use crossbar::prelude::*;
///
/// fn health() -> &'static str { "ok" }
///
/// let router = Router::new()
///     .route("/health", get(sync_handler(health)));
/// ```
#[must_use]
pub fn sync_handler<F, R>(f: F) -> SyncFn0<F>
where
    F: Fn() -> R + Send + Sync + 'static,
    R: IntoResponse + Send + 'static,
{
    SyncFn0(f)
}

/// Wraps a synchronous one-argument function for use as a route handler.
///
/// The function receives the [`Request`] directly.
///
/// # Examples
///
/// ```rust
/// use crossbar::prelude::*;
///
/// fn echo(req: Request) -> String {
///     format!("got {} bytes", req.body.len())
/// }
///
/// let router = Router::new()
///     .route("/echo", post(sync_handler_with_req(echo)));
/// ```
#[must_use]
pub fn sync_handler_with_req<F, R>(f: F) -> SyncFn1<F>
where
    F: Fn(Request) -> R + Send + Sync + 'static,
    R: IntoResponse + Send + 'static,
{
    SyncFn1(f)
}

impl<F, R> Handler<()> for SyncFn0<F>
where
    F: Fn() -> R + Send + Sync + 'static,
    R: IntoResponse + Send + 'static,
{
    fn call(&self, _req: Request) -> Pin<Box<dyn Future<Output = Response> + Send>> {
        let resp = (self.0)().into_response();
        Box::pin(async move { resp })
    }

    fn boxed(self) -> BoxedHandler
    where
        Self: Sized,
    {
        let handler = Arc::new(self);
        let async_handler = Arc::clone(&handler);
        BoxedHandler::new_sync(
            move |_req| (handler.0)().into_response(),
            Arc::new(move |req| async_handler.call(req)),
        )
    }
}

impl<F, R> Handler<(Request,)> for SyncFn1<F>
where
    F: Fn(Request) -> R + Send + Sync + 'static,
    R: IntoResponse + Send + 'static,
{
    fn call(&self, req: Request) -> Pin<Box<dyn Future<Output = Response> + Send>> {
        let resp = (self.0)(req).into_response();
        Box::pin(async move { resp })
    }

    fn boxed(self) -> BoxedHandler
    where
        Self: Sized,
    {
        let handler = Arc::new(self);
        let async_handler = Arc::clone(&handler);
        BoxedHandler::new_sync(
            move |req| (handler.0)(req).into_response(),
            Arc::new(move |req| async_handler.call(req)),
        )
    }
}
