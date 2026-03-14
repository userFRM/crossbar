//! Core request/response types and the [`IntoResponse`] trait.
//!
//! This module defines the data model that flows through every crossbar
//! transport:
//!
//! - [`Method`] — HTTP-style verb encoded as a single byte on the wire.
//! - [`Uri`] — Parsed request URI with path and query components.
//! - [`Request`] — Inbound message carrying method, URI, headers, and body.
//! - [`Response`] — Outbound message with status code, headers, and body.
//! - [`Body`] — Zero-copy request/response body.
//! - [`Json`] — Thin wrapper that serializes `T` to a JSON response body.
//! - [`IntoResponse`] — Trait implemented by everything that can become a
//!   [`Response`]; enables ergonomic handler return types.

use std::collections::HashMap;

// ── Body ───────────────────────────────────────────────

#[cfg(feature = "shm")]
use crate::transport::shm::region::ShmRegion;
#[cfg(feature = "shm")]
use std::sync::Arc;

/// Guard that owns a block in the SHM pool. Frees the block back to the pool
/// on drop. Used as the `Mmap` variant of [`Body`] for zero-copy reads.
#[cfg(feature = "shm")]
pub struct ShmBodyGuard {
    pub(crate) region: Arc<ShmRegion>,
    pub(crate) block_idx: u32,
    pub(crate) offset: usize,
    pub(crate) len: usize,
}

// SAFETY: ShmBodyGuard holds an Arc<ShmRegion> which keeps the mmap alive.
// The block is exclusively owned (not in the free list) until this guard drops.
// The underlying data is immutable while the guard exists.
#[cfg(feature = "shm")]
#[allow(unsafe_code)]
unsafe impl Send for ShmBodyGuard {}
#[cfg(feature = "shm")]
#[allow(unsafe_code)]
unsafe impl Sync for ShmBodyGuard {}

#[cfg(feature = "shm")]
#[allow(unsafe_code)]
impl ShmBodyGuard {
    fn as_slice(&self) -> &[u8] {
        let ptr = self.region.block_ptr(self.block_idx);
        unsafe { std::slice::from_raw_parts(ptr.add(self.offset), self.len) }
    }
}

#[cfg(feature = "shm")]
impl Drop for ShmBodyGuard {
    fn drop(&mut self) {
        self.region.free_block(self.block_idx);
    }
}

#[cfg(feature = "shm")]
impl std::fmt::Debug for ShmBodyGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShmBodyGuard")
            .field("block_idx", &self.block_idx)
            .field("offset", &self.offset)
            .field("len", &self.len)
            .finish()
    }
}

/// Zero-copy request/response body.
///
/// Avoids heap allocation on the SHM path by storing the mmap block guard
/// inline. For owned data, wraps a `Vec<u8>` with no Arc overhead.
pub enum Body {
    /// No body.
    Empty,
    /// Heap-owned bytes (from user code, JSON serialization, etc.).
    Owned(Vec<u8>),
    /// Zero-copy view into an mmap block. The block is freed when this
    /// variant is dropped. Only constructed inside `region.rs`.
    #[cfg(feature = "shm")]
    Mmap(ShmBodyGuard),
}

impl Body {
    /// Creates an empty body.
    #[inline]
    #[must_use]
    pub fn new() -> Body {
        Body::Empty
    }

    /// Returns the length of the body in bytes.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Body::Empty => 0,
            Body::Owned(v) => v.len(),
            #[cfg(feature = "shm")]
            Body::Mmap(guard) => guard.len,
        }
    }

    /// Returns `true` if the body has zero length.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns a raw pointer to the start of the body data.
    ///
    /// Returns a dangling pointer if the body is empty.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *const u8 {
        match self {
            Body::Empty => [].as_ptr(),
            Body::Owned(v) => v.as_ptr(),
            #[cfg(feature = "shm")]
            Body::Mmap(guard) => guard.as_slice().as_ptr(),
        }
    }
}

impl Default for Body {
    #[inline]
    fn default() -> Self {
        Body::Empty
    }
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Body::Empty => f.write_str("Body::Empty"),
            Body::Owned(v) => write!(f, "Body::Owned(len={})", v.len()),
            #[cfg(feature = "shm")]
            Body::Mmap(guard) => write!(f, "Body::Mmap(len={})", guard.len),
        }
    }
}

impl Clone for Body {
    fn clone(&self) -> Self {
        match self {
            Body::Empty => Body::Empty,
            Body::Owned(v) => Body::Owned(v.clone()),
            #[cfg(feature = "shm")]
            Body::Mmap(guard) => Body::Owned(guard.as_slice().to_vec()),
        }
    }
}

impl AsRef<[u8]> for Body {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        match self {
            Body::Empty => &[],
            Body::Owned(v) => v.as_ref(),
            #[cfg(feature = "shm")]
            Body::Mmap(guard) => guard.as_slice(),
        }
    }
}

impl std::ops::Deref for Body {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}

impl From<Vec<u8>> for Body {
    #[inline]
    fn from(v: Vec<u8>) -> Self {
        if v.is_empty() {
            Body::Empty
        } else {
            Body::Owned(v)
        }
    }
}

impl From<&[u8]> for Body {
    #[inline]
    fn from(s: &[u8]) -> Self {
        if s.is_empty() {
            Body::Empty
        } else {
            Body::Owned(s.to_vec())
        }
    }
}

impl From<&str> for Body {
    #[inline]
    fn from(s: &str) -> Self {
        if s.is_empty() {
            Body::Empty
        } else {
            Body::Owned(s.as_bytes().to_vec())
        }
    }
}

impl From<String> for Body {
    #[inline]
    fn from(s: String) -> Self {
        if s.is_empty() {
            Body::Empty
        } else {
            Body::Owned(s.into_bytes())
        }
    }
}

// ── Method ──────────────────────────────────────────────

/// An HTTP-style request method.
///
/// Encoded as a single byte on the crossbar wire protocol (see
/// [`From<Method> for u8`] and [`TryFrom<u8> for Method`]).
///
/// # Examples
///
/// ```rust
/// use crossbar::types::Method;
///
/// assert_eq!(Method::Get.to_string(), "GET");
/// assert_eq!(u8::from(Method::Post), 1u8);
/// assert_eq!(Method::try_from(2u8).unwrap(), Method::Put);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `GET` — retrieve a resource.
    Get,
    /// `POST` — submit data to a resource.
    Post,
    /// `PUT` — replace a resource.
    Put,
    /// `DELETE` — remove a resource.
    Delete,
    /// `PATCH` — partially update a resource.
    Patch,
}

impl Method {
    /// Returns the canonical uppercase ASCII string for this method.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::types::Method;
    /// assert_eq!(Method::Delete.as_str(), "DELETE");
    /// ```
    #[inline]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Patch => "PATCH",
        }
    }
}

impl std::fmt::Display for Method {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Converts a [`Method`] to its single-byte wire representation.
///
/// The mapping is stable and forms part of the crossbar wire protocol:
/// `Get=0`, `Post=1`, `Put=2`, `Delete=3`, `Patch=4`.
impl From<Method> for u8 {
    #[inline]
    fn from(m: Method) -> u8 {
        match m {
            Method::Get => 0,
            Method::Post => 1,
            Method::Put => 2,
            Method::Delete => 3,
            Method::Patch => 4,
        }
    }
}

/// Attempts to parse a [`Method`] from its single-byte wire representation.
///
/// Returns `Err(byte)` for any byte that does not correspond to a known
/// variant.
///
/// # Examples
///
/// ```rust
/// use crossbar::types::Method;
///
/// assert_eq!(Method::try_from(0u8).unwrap(), Method::Get);
/// assert!(Method::try_from(99u8).is_err());
/// ```
impl TryFrom<u8> for Method {
    type Error = u8;

    #[inline]
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(Method::Get),
            1 => Ok(Method::Post),
            2 => Ok(Method::Put),
            3 => Ok(Method::Delete),
            4 => Ok(Method::Patch),
            other => Err(other),
        }
    }
}

// ── Uri ─────────────────────────────────────────────────

/// A parsed request URI.
///
/// Handles both bare paths (`/foo/bar?q=1`) and full URIs with a scheme and
/// authority (`app://localhost/foo/bar?q=1`).  The scheme and authority are
/// stripped during parsing; only the path and query string are retained.
///
/// # Examples
///
/// ```rust
/// use crossbar::types::Uri;
///
/// let u = Uri::parse("/v1/users?page=2");
/// assert_eq!(u.path(), "/v1/users");
/// assert_eq!(u.query(), Some("page=2"));
/// assert_eq!(u.to_string(), "/v1/users?page=2");
/// ```
#[derive(Debug, Clone)]
pub struct Uri {
    raw: String,
    path: String,
    query: Option<String>,
}

impl Uri {
    /// Parse a URI string, stripping any scheme and authority components.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::types::Uri;
    ///
    /// let u = Uri::parse("tauri://localhost/api/health");
    /// assert_eq!(u.path(), "/api/health");
    /// assert_eq!(u.query(), None);
    /// ```
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        // Strip scheme + authority if present (e.g. "app://localhost/path" -> "/path")
        let path_start = if let Some(scheme_end) = raw.find("://") {
            let after_scheme = &raw[scheme_end + 3..];
            match after_scheme.find('/') {
                Some(p) => scheme_end + 3 + p,
                None => raw.len(),
            }
        } else {
            0
        };

        let remainder = if path_start < raw.len() {
            &raw[path_start..]
        } else {
            "/"
        };

        if let Some(q) = remainder.find('?') {
            Uri {
                raw: raw.to_string(),
                path: remainder[..q].to_string(),
                query: Some(remainder[q + 1..].to_string()),
            }
        } else {
            Uri {
                raw: raw.to_string(),
                path: remainder.to_string(),
                query: None,
            }
        }
    }

    /// Returns the path component (never empty; defaults to `/`).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::types::Uri;
    /// assert_eq!(Uri::parse("/api/v1").path(), "/api/v1");
    /// ```
    #[inline]
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the raw query string, if present (without the leading `?`).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::types::Uri;
    /// assert_eq!(Uri::parse("/x?a=1&b=2").query(), Some("a=1&b=2"));
    /// assert_eq!(Uri::parse("/x").query(), None);
    /// ```
    #[inline]
    #[must_use]
    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }

    /// Returns the original raw URI string exactly as passed to [`Uri::parse`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::types::Uri;
    /// assert_eq!(Uri::parse("/x?y=1").raw(), "/x?y=1");
    /// ```
    #[inline]
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }
}

impl std::fmt::Display for Uri {
    /// Displays the raw URI string.
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

impl From<&str> for Uri {
    /// Parses a URI from a string slice via [`Uri::parse`].
    #[inline]
    fn from(s: &str) -> Self {
        Uri::parse(s)
    }
}

// ── Request ─────────────────────────────────────────────

/// An inbound request dispatched through the router.
///
/// Constructed by transport layers when a client sends a request, and passed
/// directly to the matching handler.  Path parameters extracted by the router
/// are injected before the handler is called.
///
/// # Examples
///
/// ```rust
/// use crossbar::types::{Method, Request};
///
/// let req = Request::new(Method::Get, "/v1/users/42")
///     .with_body("hello");
/// assert_eq!(req.method, Method::Get);
/// assert_eq!(req.uri.path(), "/v1/users/42");
/// assert!(req.is_get());
/// ```
#[derive(Debug)]
pub struct Request {
    /// The HTTP-style method of the request.
    pub method: Method,
    /// The parsed URI.
    pub uri: Uri,
    /// Request headers as a flat key-value map (lowercase keys by convention).
    pub headers: HashMap<String, String>,
    /// The raw request body.
    pub body: Body,
    /// Path parameters extracted by the router (e.g. `{id}` from `/users/:id`).
    pub(crate) path_params: HashMap<String, String>,
}

impl Request {
    /// Creates a new request with an empty body and no headers.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::types::{Method, Request};
    /// let req = Request::new(Method::Post, "/submit");
    /// assert_eq!(req.uri.path(), "/submit");
    /// ```
    #[must_use]
    pub fn new(method: Method, uri: &str) -> Self {
        Request {
            method,
            uri: Uri::parse(uri),
            headers: HashMap::new(),
            body: Body::Empty,
            path_params: HashMap::new(),
        }
    }

    /// Replaces the request body and returns `self` for chaining.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::types::{Method, Request};
    /// let req = Request::new(Method::Post, "/data").with_body("payload");
    /// assert_eq!(&req.body[..], b"payload");
    /// ```
    #[must_use]
    pub fn with_body(mut self, body: impl Into<Body>) -> Self {
        self.body = body.into();
        self
    }

    /// Returns `true` if this is a `GET` request.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::types::{Method, Request};
    /// assert!(Request::new(Method::Get, "/").is_get());
    /// assert!(!Request::new(Method::Post, "/").is_get());
    /// ```
    #[inline]
    #[must_use]
    pub fn is_get(&self) -> bool {
        self.method == Method::Get
    }

    /// Returns `true` if this is a `POST` request.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::types::{Method, Request};
    /// assert!(Request::new(Method::Post, "/").is_post());
    /// assert!(!Request::new(Method::Get, "/").is_post());
    /// ```
    #[inline]
    #[must_use]
    pub fn is_post(&self) -> bool {
        self.method == Method::Post
    }

    /// Returns the value of a path parameter by name, if present.
    ///
    /// Path parameters are defined in route patterns with a leading colon, e.g.
    /// `/users/:id`.  After the router matches a request, `:id` is available
    /// via `req.path_param("id")`.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use crossbar::prelude::*;
    ///
    /// async fn greet(req: Request) -> String {
    ///     let name = req.path_param("name").unwrap_or("stranger");
    ///     format!("hello {name}")
    /// }
    ///
    /// let router = Router::new().route("/greet/:name", get(greet));
    /// ```
    #[must_use]
    pub fn path_param(&self, name: &str) -> Option<&str> {
        self.path_params.get(name).map(String::as_str)
    }

    /// Parses the query string into a key-value map.
    ///
    /// Both keys and values are percent-decoded.  If there is no query string,
    /// an empty map is returned.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::types::{Method, Request};
    /// let req = Request::new(Method::Get, "/search?q=hello+world&page=2");
    /// let params = req.query_params();
    /// assert_eq!(params.get("q").map(String::as_str), Some("hello world"));
    /// ```
    #[must_use]
    pub fn query_params(&self) -> HashMap<String, String> {
        parse_query(self.uri.query().unwrap_or(""))
    }

    /// Returns a single query parameter by name, or `None` if absent.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::types::{Method, Request};
    /// let req = Request::new(Method::Get, "/items?sort=desc");
    /// assert_eq!(req.query_param("sort").as_deref(), Some("desc"));
    /// assert_eq!(req.query_param("page"), None);
    /// ```
    #[must_use]
    pub fn query_param(&self, name: &str) -> Option<String> {
        self.query_params().remove(name)
    }

    /// Deserializes the request body as JSON into type `T`.
    ///
    /// # Errors
    ///
    /// Returns a [`serde_json::Error`] if the body is not valid JSON or does
    /// not match the shape of `T`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use serde::Deserialize;
    /// use crossbar::types::{Method, Request};
    ///
    /// #[derive(Deserialize, PartialEq, Debug)]
    /// struct Payload { value: u32 }
    ///
    /// let req = Request::new(Method::Post, "/data")
    ///     .with_body(r#"{"value":7}"#);
    /// let p: Payload = req.json_body().unwrap();
    /// assert_eq!(p.value, 7);
    /// ```
    pub fn json_body<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }
}

// ── Response ────────────────────────────────────────────

/// An outbound response returned from a handler.
///
/// Built using the constructor methods ([`Response::ok`],
/// [`Response::with_status`]) and the builder methods ([`Response::with_body`],
/// [`Response::with_header`]).
///
/// # Examples
///
/// ```rust
/// use crossbar::types::Response;
///
/// let resp = Response::ok()
///     .with_header("x-request-id", "abc123")
///     .with_body("hello");
/// assert_eq!(resp.status, 200);
/// assert_eq!(resp.body_str(), "hello");
/// ```
#[derive(Debug)]
pub struct Response {
    /// HTTP-style status code (e.g. `200`, `404`).
    pub status: u16,
    /// Response headers as a flat key-value map (lowercase keys by convention).
    pub headers: HashMap<String, String>,
    /// The raw response body.
    pub body: Body,
}

impl Response {
    /// Creates a `200 OK` response with an empty body.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::types::Response;
    /// let r = Response::ok();
    /// assert_eq!(r.status, 200);
    /// ```
    #[inline]
    #[must_use]
    pub fn ok() -> Self {
        Response {
            status: 200,
            headers: HashMap::new(),
            body: Body::Empty,
        }
    }

    /// Creates a response with the given status code and an empty body.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::types::Response;
    /// let r = Response::with_status(204);
    /// assert_eq!(r.status, 204);
    /// ```
    #[inline]
    #[must_use]
    pub fn with_status(status: u16) -> Self {
        Response {
            status,
            headers: HashMap::new(),
            body: Body::Empty,
        }
    }

    /// Replaces the body and returns `self` for chaining.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::types::Response;
    /// let r = Response::ok().with_body("pong");
    /// assert_eq!(r.body_str(), "pong");
    /// ```
    #[must_use]
    pub fn with_body(mut self, body: impl Into<Body>) -> Self {
        self.body = body.into();
        self
    }

    /// Inserts a header and returns `self` for chaining.
    ///
    /// Keys are stored as provided; by convention use lowercase ASCII.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::types::Response;
    /// let r = Response::ok().with_header("cache-control", "no-store");
    /// assert_eq!(r.headers.get("cache-control").map(String::as_str), Some("no-store"));
    /// ```
    #[must_use]
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(key.into(), value.into());
        self
    }

    /// Serializes `data` as JSON and returns a `200 OK` response with the
    /// appropriate `content-type` header.
    ///
    /// If serialization fails, returns a `500` response with a plain-text body.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use serde::Serialize;
    /// use crossbar::types::Response;
    ///
    /// #[derive(Serialize)]
    /// struct Point { x: i32, y: i32 }
    ///
    /// let r = Response::json(&Point { x: 1, y: 2 });
    /// assert_eq!(r.status, 200);
    /// assert_eq!(r.headers.get("content-type").map(String::as_str), Some("application/json"));
    /// ```
    #[must_use]
    pub fn json<T: serde::Serialize>(data: &T) -> Self {
        match serde_json::to_vec(data) {
            Ok(bytes) => Response::ok()
                .with_body(Body::Owned(bytes))
                .with_header("content-type", "application/json"),
            Err(_) => Response::with_status(500).with_body("serialization error"),
        }
    }

    /// Creates a `404 Not Found` response with a plain-text body.
    #[inline]
    #[must_use]
    pub fn not_found() -> Self {
        Response::with_status(404).with_body("not found")
    }

    /// Creates a `400 Bad Request` response with the supplied message as body.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::types::Response;
    /// let r = Response::bad_request("missing field");
    /// assert_eq!(r.status, 400);
    /// ```
    #[must_use]
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Response::with_status(400).with_body(msg.into())
    }

    /// Returns the body as a UTF-8 string slice.
    ///
    /// If the body contains non-UTF-8 bytes, returns the literal string
    /// `"<binary>"`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use crossbar::types::Response;
    /// let r = Response::ok().with_body("hello");
    /// assert_eq!(r.body_str(), "hello");
    /// ```
    #[must_use]
    pub fn body_str(&self) -> &str {
        std::str::from_utf8(&self.body).unwrap_or("<binary>")
    }
}

// ── Json wrapper ────────────────────────────────────────

/// A typed JSON response wrapper.
///
/// Return `Json(value)` from a handler to automatically serialize `value` as
/// JSON with a `content-type: application/json` header.
///
/// # Examples
///
/// ```rust
/// use serde::Serialize;
/// use crossbar::types::{Json, IntoResponse};
///
/// #[derive(Serialize)]
/// struct Msg { text: &'static str }
///
/// let resp = Json(Msg { text: "hi" }).into_response();
/// assert_eq!(resp.status, 200);
/// ```
pub struct Json<T>(
    /// The value to be serialized as JSON.
    pub T,
);

// ── IntoResponse ────────────────────────────────────────

/// Conversion trait for handler return types.
///
/// Any type that implements `IntoResponse` can be returned from a crossbar
/// handler.  The built-in implementations cover the most common cases; add
/// your own for custom response types.
///
/// # Examples
///
/// ```rust
/// use crossbar::types::{IntoResponse, Response};
///
/// struct NotFound;
/// impl IntoResponse for NotFound {
///     fn into_response(self) -> Response {
///         Response::not_found()
///     }
/// }
/// ```
pub trait IntoResponse {
    /// Consumes `self` and produces a [`Response`].
    fn into_response(self) -> Response;
}

impl IntoResponse for Response {
    #[inline]
    fn into_response(self) -> Response {
        self
    }
}

impl IntoResponse for &'static str {
    /// Wraps the string in a `200 OK` response.
    #[inline]
    fn into_response(self) -> Response {
        Response::ok().with_body(self)
    }
}

impl IntoResponse for String {
    /// Wraps the string in a `200 OK` response.
    #[inline]
    fn into_response(self) -> Response {
        Response::ok().with_body(self)
    }
}

impl IntoResponse for Body {
    /// Wraps the body in a `200 OK` response.
    #[inline]
    fn into_response(self) -> Response {
        Response::ok().with_body(self)
    }
}

impl IntoResponse for Vec<u8> {
    /// Wraps the bytes in a `200 OK` response.
    #[inline]
    fn into_response(self) -> Response {
        Response::ok().with_body(self)
    }
}

impl<T: serde::Serialize> IntoResponse for Json<T> {
    /// Serializes the inner value as JSON.
    fn into_response(self) -> Response {
        Response::json(&self.0)
    }
}

impl IntoResponse for (u16, &'static str) {
    /// Uses the first element as the status code and the second as the body.
    #[inline]
    fn into_response(self) -> Response {
        Response::with_status(self.0).with_body(self.1)
    }
}

impl IntoResponse for (u16, String) {
    /// Uses the first element as the status code and the second as the body.
    #[inline]
    fn into_response(self) -> Response {
        Response::with_status(self.0).with_body(self.1)
    }
}

impl<R: IntoResponse, E: IntoResponse> IntoResponse for Result<R, E> {
    /// Calls `into_response` on the `Ok` or `Err` value.
    fn into_response(self) -> Response {
        match self {
            Ok(r) => r.into_response(),
            Err(e) => e.into_response(),
        }
    }
}

// ── Helpers ─────────────────────────────────────────────

/// Percent-decodes a URI-encoded string.
///
/// `+` is decoded as a space (form-encoding convention).  Invalid percent
/// sequences are passed through unchanged.  Multi-byte UTF-8 sequences encoded
/// as consecutive percent-escaped bytes (e.g. `%C3%A9` for `é`) are decoded
/// correctly.
///
/// # Examples
///
/// ```rust
/// use crossbar::types::percent_decode;
/// assert_eq!(percent_decode("hello%20world"), "hello world");
/// assert_eq!(percent_decode("a+b"), "a b");
/// assert_eq!(percent_decode("caf%C3%A9"), "café");
/// ```
#[must_use]
pub fn percent_decode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut iter = s.bytes();
    while let Some(b) = iter.next() {
        if b == b'%' {
            if let (Some(h), Some(l)) = (iter.next(), iter.next()) {
                if let (Some(hv), Some(lv)) = (hex_val(h), hex_val(l)) {
                    bytes.push(hv << 4 | lv);
                    continue;
                }
            }
            bytes.push(b'%');
        } else if b == b'+' {
            bytes.push(b' ');
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Decodes a single hex digit to its numeric value.
#[inline]
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Parses a query string into a key-value map.
///
/// Splits on `&`, then on `=`, and percent-decodes both key and value.
/// Pairs with no `=` sign are recorded with an empty string value.
///
/// # Examples
///
/// ```rust
/// use crossbar::types::parse_query;
/// let params = parse_query("a=1&b=hello%20world");
/// assert_eq!(params.get("b").map(String::as_str), Some("hello world"));
/// ```
#[must_use]
pub fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next()?;
            let value = parts.next().unwrap_or("");
            Some((percent_decode(key), percent_decode(value)))
        })
        .collect()
}
