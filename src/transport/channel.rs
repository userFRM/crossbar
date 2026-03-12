use crate::error::CrossbarError;
use crate::router::Router;
use crate::types::{Method, Request, Response};
use bytes::Bytes;

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Channel — tokio::mpsc, cross-task dispatch
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Spawns a background task that processes requests sent via a [`ChannelClient`].
///
/// This transport is ideal for cross-task communication within the same process
/// where you need to serialize access to the router from multiple concurrent
/// callers.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// async fn health() -> &'static str { "ok" }
///
/// # #[tokio::main] async fn main() -> Result<(), CrossbarError> {
/// let router = Router::new().route("/health", get(health));
/// let client = ChannelServer::spawn(router);
/// let resp = client.get("/health").await?;
/// assert_eq!(resp.status, 200);
/// # Ok(())
/// # }
/// ```
pub struct ChannelServer;

impl ChannelServer {
    /// Spawns a background task and returns a [`ChannelClient`] connected to it.
    ///
    /// The background task runs until the last [`ChannelClient`] clone is
    /// dropped, at which point the channel closes and the task exits cleanly.
    pub fn spawn(router: Router) -> ChannelClient {
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<(Request, tokio::sync::oneshot::Sender<Response>)>(256);

        tokio::spawn(async move {
            while let Some((req, reply_tx)) = rx.recv().await {
                let resp = router.dispatch(req).await;
                let _ = reply_tx.send(resp);
            }
        });

        ChannelClient { tx }
    }
}

/// Client that sends requests to a [`ChannelServer`] background task.
///
/// Cheaply clonable — each clone shares the same underlying sender.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// async fn health() -> &'static str { "ok" }
///
/// # #[tokio::main] async fn main() -> Result<(), CrossbarError> {
/// let router = Router::new().route("/health", get(health));
/// let client = ChannelServer::spawn(router);
/// let resp = client.get("/health").await?;
/// println!("{}", resp.status);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct ChannelClient {
    tx: tokio::sync::mpsc::Sender<(Request, tokio::sync::oneshot::Sender<Response>)>,
}

impl ChannelClient {
    /// Sends `req` to the background task and awaits the response.
    ///
    /// # Errors
    ///
    /// Returns [`CrossbarError::ServerDropped`] if the background task has
    /// exited (i.e. the receiving end of the channel was dropped).
    pub async fn request(&self, req: Request) -> Result<Response, CrossbarError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send((req, reply_tx))
            .await
            .map_err(|_| CrossbarError::ServerDropped)?;
        reply_rx.await.map_err(|_| CrossbarError::ServerDropped)
    }

    /// Convenience method for `GET` requests.
    ///
    /// # Errors
    ///
    /// Returns [`CrossbarError::ServerDropped`] if the background task has exited.
    pub async fn get(&self, uri: &str) -> Result<Response, CrossbarError> {
        self.request(Request::new(Method::Get, uri)).await
    }

    /// Convenience method for `POST` requests with a body.
    ///
    /// # Errors
    ///
    /// Returns [`CrossbarError::ServerDropped`] if the background task has exited.
    pub async fn post(&self, uri: &str, body: impl Into<Bytes>) -> Result<Response, CrossbarError> {
        self.request(Request::new(Method::Post, uri).with_body(body))
            .await
    }
}
