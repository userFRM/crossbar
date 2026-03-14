//! URI router that dispatches requests to registered handlers.
//!
//! The [`Router`] is the central piece of crossbar.  It matches incoming
//! [`Request`]s against a list of (method, path-pattern, handler) triples and
//! calls the first one that matches.
//!
//! # Path parameters
//!
//! Route patterns may contain named segments prefixed with `:`.  On a
//! successful match the captured values are stored on the request and can be
//! retrieved with [`Request::path_param`](crate::types::Request::path_param).
//!
//! ```rust
//! use crossbar::prelude::*;
//!
//! async fn get_user(req: Request) -> String {
//!     let id = req.path_param("id").unwrap_or("unknown");
//!     format!("user {id}")
//! }
//!
//! let router = Router::new()
//!     .route("/users/:id", get(get_user));
//! ```

use crate::handler::BoxedHandler;
use crate::types::{percent_decode, Method, Request, Response};
use std::collections::HashMap;
use std::sync::Arc;

// ── Path pattern matching ────────────────────────────────

/// A single segment in a route pattern.
#[derive(Clone)]
enum Segment {
    /// A literal path segment that must match exactly.
    Literal(String),
    /// A named parameter segment (`:name`) that matches any value.
    Param(String),
}

/// A compiled route pattern, ready for matching.
#[derive(Clone)]
struct PathPattern {
    /// The parsed segments.
    segments: Vec<Segment>,
    /// The original pattern string, kept for [`Router::routes_info`].
    raw: String,
    /// True when at least one segment is a [`Segment::Param`].
    /// When false, [`matches_literal`] can be used for zero-alloc matching.
    has_params: bool,
}

impl PathPattern {
    /// Compile a pattern string into a [`PathPattern`].
    fn parse(pattern: &str) -> Self {
        let segments: Vec<Segment> = pattern
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| {
                if let Some(name) = s.strip_prefix(':') {
                    Segment::Param(name.to_string())
                } else {
                    Segment::Literal(s.to_string())
                }
            })
            .collect();
        let has_params = segments.iter().any(|s| matches!(s, Segment::Param(_)));
        PathPattern {
            segments,
            raw: pattern.to_string(),
            has_params,
        }
    }

    /// Fast path for literal-only patterns: no `HashMap`, no allocations.
    ///
    /// Returns `true` if `path` matches this pattern exactly. Only valid
    /// when `has_params` is `false`.
    #[inline]
    fn matches_literal(&self, path: &str) -> bool {
        debug_assert!(!self.has_params);
        let mut path_iter = path.split('/').filter(|s| !s.is_empty());
        for seg in &self.segments {
            match seg {
                Segment::Literal(lit) => match path_iter.next() {
                    Some(s) if s == lit.as_str() => {}
                    _ => return false,
                },
                Segment::Param(_) => unreachable!("matches_literal called with params"),
            }
        }
        path_iter.next().is_none()
    }

    /// Attempt to match `path` against this pattern.
    ///
    /// Returns `Some(params)` with any captured named parameters on success,
    /// or `None` if the path does not match.
    ///
    /// Uses iterator-based matching to avoid collecting path segments into
    /// a `Vec`.
    fn matches(&self, path: &str) -> Option<HashMap<String, String>> {
        let mut path_iter = path.split('/').filter(|s| !s.is_empty());
        let mut params = HashMap::new();

        for seg in &self.segments {
            let path_seg = path_iter.next()?;
            match seg {
                Segment::Literal(lit) => {
                    if lit != path_seg {
                        return None;
                    }
                }
                Segment::Param(name) => {
                    params.insert(name.clone(), percent_decode(path_seg));
                }
            }
        }

        // Reject paths with trailing segments
        if path_iter.next().is_some() {
            return None;
        }

        Some(params)
    }
}

// ── Route ────────────────────────────────────────────────

/// A single registered route: method + pattern + handler.
#[derive(Clone)]
struct Route {
    method: Method,
    pattern: PathPattern,
    handler: BoxedHandler,
}

// ── Router ───────────────────────────────────────────────

/// Immutable, cheaply clonable request router.
///
/// The router stores its routes behind an [`Arc`] so that cloning it — as
/// transport server tasks must do for each accepted connection — is O(1).
///
/// Routes are matched in registration order; the first matching (method,
/// pattern) pair wins.  If no route matches, the router returns
/// [`Response::not_found`](crate::types::Response::not_found).
///
/// # Examples
///
/// ```rust
/// use crossbar::prelude::*;
///
/// async fn health() -> &'static str { "ok" }
/// async fn greet(req: Request) -> String {
///     format!("hello, {}!", req.path_param("name").unwrap_or("stranger"))
/// }
///
/// let router = Router::new()
///     .route("/health", get(health))
///     .route("/hello/:name", get(greet));
/// ```
#[derive(Clone)]
pub struct Router {
    routes: Arc<Vec<Route>>,
}

impl Router {
    /// Creates a new, empty router.
    ///
    /// Equivalent to [`Router::default`].
    #[must_use]
    pub fn new() -> Self {
        Router {
            routes: Arc::new(Vec::new()),
        }
    }

    /// Registers a route by pattern and a `(Method, BoxedHandler)` tuple.
    ///
    /// Use the free functions [`get`], [`post`], [`put`], [`delete`], and
    /// [`patch`] to construct the tuple:
    ///
    /// ```rust
    /// use crossbar::prelude::*;
    ///
    /// async fn handler() -> &'static str { "ok" }
    ///
    /// let router = Router::new()
    ///     .route("/ping", get(handler))
    ///     .route("/data", post(handler));
    /// ```
    #[must_use]
    pub fn route(mut self, pattern: &str, (method, handler): (Method, BoxedHandler)) -> Self {
        Arc::make_mut(&mut self.routes).push(Route {
            method,
            pattern: PathPattern::parse(pattern),
            handler,
        });
        self
    }

    /// Dispatches a request to the first matching handler.
    ///
    /// Returns [`Response::not_found`](crate::types::Response::not_found) when
    /// no route matches.
    ///
    /// This method is called internally by every transport server task.  It is
    /// also available publicly so callers can dispatch requests directly
    /// without going through a transport layer.
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
    /// let req = Request::new(Method::Get, "/health");
    /// let resp = router.dispatch(req).await;
    /// assert_eq!(resp.status, 200);
    /// # }
    /// ```
    pub async fn dispatch(&self, mut req: Request) -> Response {
        for route in self.routes.iter() {
            if route.method == req.method {
                if route.pattern.has_params {
                    if let Some(params) = route.pattern.matches(req.uri.path()) {
                        req.path_params = params;
                        // Try sync first to avoid Box::pin allocation
                        match route.handler.try_call_sync(req) {
                            Ok(resp) => return resp,
                            Err(req) => return route.handler.call(req).await,
                        }
                    }
                } else if route.pattern.matches_literal(req.uri.path()) {
                    // Zero-alloc fast path: no params, try sync dispatch
                    match route.handler.try_call_sync(req) {
                        Ok(resp) => return resp,
                        Err(req) => return route.handler.call(req).await,
                    }
                }
            }
        }
        Response::not_found()
    }

    /// Returns a list of `(method, pattern)` pairs for all registered routes.
    ///
    /// Useful for logging or diagnostics.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::prelude::*;
    ///
    /// async fn handler() -> &'static str { "ok" }
    ///
    /// let router = Router::new()
    ///     .route("/a", get(handler))
    ///     .route("/b", post(handler));
    /// let info = router.routes_info();
    /// assert_eq!(info.len(), 2);
    /// ```
    #[must_use]
    pub fn routes_info(&self) -> Vec<(Method, String)> {
        self.routes
            .iter()
            .map(|r| (r.method, r.pattern.raw.clone()))
            .collect()
    }
}

impl Default for Router {
    /// Creates an empty router.  Equivalent to [`Router::new`].
    fn default() -> Self {
        Self::new()
    }
}

// ── Method helpers ───────────────────────────────────────

/// Creates a `(Method::Get, BoxedHandler)` pair for use with [`Router::route`].
///
/// # Examples
///
/// ```rust
/// use crossbar::prelude::*;
/// async fn handler() -> &'static str { "ok" }
/// let _entry = get(handler);
/// ```
pub fn get<H: crate::handler::Handler<T>, T: 'static>(handler: H) -> (Method, BoxedHandler) {
    (Method::Get, BoxedHandler::new(handler))
}

/// Creates a `(Method::Post, BoxedHandler)` pair for use with [`Router::route`].
///
/// # Examples
///
/// ```rust
/// use crossbar::prelude::*;
/// async fn handler() -> &'static str { "ok" }
/// let _entry = post(handler);
/// ```
pub fn post<H: crate::handler::Handler<T>, T: 'static>(handler: H) -> (Method, BoxedHandler) {
    (Method::Post, BoxedHandler::new(handler))
}

/// Creates a `(Method::Put, BoxedHandler)` pair for use with [`Router::route`].
///
/// # Examples
///
/// ```rust
/// use crossbar::prelude::*;
/// async fn handler() -> &'static str { "ok" }
/// let _entry = put(handler);
/// ```
pub fn put<H: crate::handler::Handler<T>, T: 'static>(handler: H) -> (Method, BoxedHandler) {
    (Method::Put, BoxedHandler::new(handler))
}

/// Creates a `(Method::Delete, BoxedHandler)` pair for use with [`Router::route`].
///
/// # Examples
///
/// ```rust
/// use crossbar::prelude::*;
/// async fn handler() -> &'static str { "ok" }
/// let _entry = delete(handler);
/// ```
pub fn delete<H: crate::handler::Handler<T>, T: 'static>(handler: H) -> (Method, BoxedHandler) {
    (Method::Delete, BoxedHandler::new(handler))
}

/// Creates a `(Method::Patch, BoxedHandler)` pair for use with [`Router::route`].
///
/// # Examples
///
/// ```rust
/// use crossbar::prelude::*;
/// async fn handler() -> &'static str { "ok" }
/// let _entry = patch(handler);
/// ```
pub fn patch<H: crate::handler::Handler<T>, T: 'static>(handler: H) -> (Method, BoxedHandler) {
    (Method::Patch, BoxedHandler::new(handler))
}
