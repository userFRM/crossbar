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
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// async fn ping() -> &'static str { "pong" }
///
/// # #[tokio::main] async fn main() {
/// let router = Router::new().route("/ping", get(ping));
/// let client = InProcessClient::new(router);
/// let resp = client.get("/ping").await;
/// assert_eq!(resp.status, 200);
/// # }
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
    pub async fn request(&self, req: Request) -> Response {
        self.router.dispatch(req).await
    }

    /// Convenience method for `GET` requests.
    pub async fn get(&self, uri: &str) -> Response {
        self.request(Request::new(Method::Get, uri)).await
    }

    /// Convenience method for `POST` requests with a body.
    pub async fn post(&self, uri: &str, body: impl Into<Body>) -> Response {
        self.request(Request::new(Method::Post, uri).with_body(body))
            .await
    }
}
