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
pub trait Handler<T>: Send + Sync + 'static {
    /// Calls the handler with the given request and returns a boxed future
    /// that resolves to a [`Response`].
    fn call(&self, req: Request) -> Pin<Box<dyn Future<Output = Response> + Send>>;
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
}

type DynHandler = dyn Fn(Request) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync;

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
    pub(crate) async fn call(&self, req: Request) -> Response {
        (self.f)(req).await
    }
}

impl Clone for BoxedHandler {
    fn clone(&self) -> Self {
        BoxedHandler {
            f: Arc::clone(&self.f),
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
}
