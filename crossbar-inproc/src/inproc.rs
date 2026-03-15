// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! In-process transport for zero-overhead router dispatch.

use crate::router::Router;
use crate::types::{Body, Method, Request, Response};

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// InProcessClient — in-process, zero-overhead
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// In-process client that dispatches directly to the router with no framing.
///
/// This is the fastest transport — there is no serialization, no I/O, and no
/// inter-thread communication.  Use it for in-process service-to-service calls
/// or for testing handlers without standing up a server.
///
/// # Examples
///
/// ```rust
/// use crossbar_inproc::prelude::*;
///
/// fn ping() -> &'static str { "pong" }
///
/// let router = Router::new().route("/ping", get(ping));
/// let client = InProcessClient::new(router);
/// let resp = client.get("/ping");
/// assert_eq!(resp.status, 200);
/// ```
#[derive(Clone)]
pub struct InProcessClient {
    router: Router,
}

impl InProcessClient {
    /// Wraps a [`Router`] in an [`InProcessClient`].
    #[must_use]
    pub fn new(router: Router) -> Self {
        InProcessClient { router }
    }

    /// Dispatches `req` directly through the router and returns the response.
    pub fn request(&self, req: Request) -> Response {
        self.router.dispatch(req)
    }

    /// Convenience method for `GET` requests.
    pub fn get(&self, uri: &str) -> Response {
        self.request(Request::new(Method::Get, uri))
    }

    /// Convenience method for `POST` requests with a body.
    pub fn post(&self, uri: &str, body: impl Into<Body>) -> Response {
        self.request(Request::new(Method::Post, uri).with_body(body))
    }
}
