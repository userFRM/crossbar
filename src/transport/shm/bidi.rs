//! Bidirectional SHM transport: RPC + server-push events.
//!
//! Combines [`ShmServer`] / [`ShmClient`] (request-response) with
//! [`ShmPoolPublisher`] / [`ShmPoolSubscriber`] (server→client push) into a
//! single unified transport.
//!
//! - **Client → Server:** RPC via [`BidiClient::get`] / [`BidiClient::post`] / [`BidiClient::request`]
//! - **Server → Client:** Push via [`BidiServer::loan`] + [`ShmPoolLoan::publish`]
//! - **Client receives pushes:** [`BidiClient::subscribe`] → [`ShmPoolSubscription::try_recv`]

use crate::error::CrossbarError;
use crate::router::Router;
use crate::types::{Body, Request, Response};

use super::pool_pubsub::{PoolPubSubConfig, PoolTopicHandle, ShmPoolLoan, ShmPoolSubscription};
use super::region::ShmConfig;
use super::{ShmClient, ShmHandle, ShmPoolPublisher, ShmPoolSubscriber, ShmServer};

use std::io;
use std::time::Duration;

/// Combined configuration for the bidirectional transport.
///
/// Controls both the RPC region (coordination slots + block pool) and the
/// events region (pool pub/sub for server→client pushes).
#[derive(Debug, Clone, Default)]
pub struct BidiConfig {
    /// Configuration for the RPC transport.
    pub rpc: ShmConfig,
    /// Configuration for the event push transport.
    pub events: PoolPubSubConfig,
}

/// Bidirectional SHM server: handles RPC requests and pushes events.
///
/// Wraps an [`ShmServer`] for request-response handling and an
/// [`ShmPoolPublisher`] for pushing events to connected clients.
///
/// # SHM regions
///
/// Creates two shared-memory regions:
/// - `crossbar-{name}` — RPC coordination slots + block pool
/// - `crossbar-pool-{name}-events` — event pub/sub pool
///
/// # Example
///
/// ```no_run
/// # use crossbar::prelude::*;
/// # async fn example() -> std::io::Result<()> {
/// let router = Router::new().route("/health", get(|| async { "ok" }));
/// let mut server = BidiServer::spawn("myapp", router).await?;
///
/// // Register and push events
/// let price_topic = server.register("/prices/AAPL").unwrap();
/// let mut loan = server.loan(&price_topic);
/// loan.set_data(b"150.25");
/// loan.publish();
/// # Ok(())
/// # }
/// ```
pub struct BidiServer {
    rpc_handle: ShmHandle,
    publisher: ShmPoolPublisher,
}

impl BidiServer {
    /// Spawns the bidirectional server with default configuration.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if either SHM region cannot be created.
    pub async fn spawn(name: &str, router: Router) -> io::Result<Self> {
        Self::spawn_with_config(name, router, BidiConfig::default()).await
    }

    /// Spawns with custom configuration for both RPC and events.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if either SHM region cannot be created.
    pub async fn spawn_with_config(
        name: &str,
        router: Router,
        config: BidiConfig,
    ) -> io::Result<Self> {
        let events_name = format!("{name}-events");
        let publisher = ShmPoolPublisher::create(&events_name, config.events)
            .map_err(|e| io::Error::other(e.to_string()))?;
        let rpc_handle = ShmServer::spawn_with_config(name, router, config.rpc).await?;

        Ok(Self {
            rpc_handle,
            publisher,
        })
    }

    /// Registers an event topic. Returns a handle for use with [`loan`](Self::loan).
    ///
    /// # Errors
    ///
    /// Returns an error if the maximum number of topics is reached or the
    /// URI exceeds 64 bytes.
    pub fn register(&mut self, topic: &str) -> Result<PoolTopicHandle, CrossbarError> {
        self.publisher.register(topic)
    }

    /// Loans a pool block for writing event data.
    ///
    /// Write your data into the loan, then call [`publish`](ShmPoolLoan::publish)
    /// to push it to all subscribers. The transfer is O(1).
    ///
    /// # Panics
    ///
    /// Panics if the pool is exhausted or the handle belongs to a different server.
    #[inline]
    pub fn loan(&mut self, handle: &PoolTopicHandle) -> ShmPoolLoan<'_> {
        self.publisher.loan(handle)
    }

    /// Updates the publisher heartbeat manually.
    ///
    /// Call this during idle periods when not publishing. The [`loan`](Self::loan)
    /// method updates the heartbeat automatically every 1024 calls.
    pub fn heartbeat(&mut self) {
        self.publisher.heartbeat();
    }

    /// Signals the RPC server to stop. The event publisher is dropped when
    /// this `BidiServer` is dropped.
    pub fn stop(&self) {
        self.rpc_handle.stop();
    }
}

/// Bidirectional SHM client: makes RPC calls and receives pushed events.
///
/// Wraps an [`ShmClient`] for request-response and an [`ShmPoolSubscriber`]
/// for receiving server-pushed events.
///
/// # Example
///
/// ```no_run
/// # use crossbar::prelude::*;
/// # async fn example() -> Result<(), CrossbarError> {
/// let client = BidiClient::connect("myapp").await?;
///
/// // RPC
/// let resp = client.get("/health").await?;
///
/// // Subscribe to server pushes
/// let mut sub = client.subscribe("/prices/AAPL")?;
/// if let Some(sample) = sub.try_recv() {
///     println!("price: {}", std::str::from_utf8(&*sample).unwrap());
/// }
/// # Ok(())
/// # }
/// ```
pub struct BidiClient {
    rpc: ShmClient,
    subscriber: ShmPoolSubscriber,
}

impl BidiClient {
    /// Connects to a bidirectional server.
    ///
    /// # Errors
    ///
    /// Returns [`CrossbarError`] if either the RPC or events region cannot
    /// be opened, or if the server heartbeat is stale.
    pub async fn connect(name: &str) -> Result<Self, CrossbarError> {
        Self::connect_with_timeout(name, Duration::from_secs(5)).await
    }

    /// Connects with a custom stale timeout.
    ///
    /// # Errors
    ///
    /// Returns [`CrossbarError`] if either region cannot be opened or if
    /// the server heartbeat is stale.
    pub async fn connect_with_timeout(
        name: &str,
        stale_timeout: Duration,
    ) -> Result<Self, CrossbarError> {
        let events_name = format!("{name}-events");
        let rpc = ShmClient::connect_with_timeout(name, stale_timeout).await?;
        let subscriber = ShmPoolSubscriber::connect(&events_name)?;

        Ok(Self { rpc, subscriber })
    }

    /// Sends a GET request via RPC.
    pub async fn get(&self, uri: &str) -> Result<Response, CrossbarError> {
        self.rpc.get(uri).await
    }

    /// Sends a POST request via RPC.
    pub async fn post(&self, uri: &str, body: impl Into<Body>) -> Result<Response, CrossbarError> {
        self.rpc.post(uri, body).await
    }

    /// Sends an arbitrary request via RPC.
    pub async fn request(&self, req: Request) -> Result<Response, CrossbarError> {
        self.rpc.request(req).await
    }

    /// Subscribes to a server-pushed event topic.
    ///
    /// Returns a [`ShmPoolSubscription`] that can be polled with
    /// [`try_recv`](ShmPoolSubscription::try_recv) (non-blocking) or
    /// [`recv`](ShmPoolSubscription::recv) (blocking with futex wait).
    ///
    /// # Errors
    ///
    /// Returns an error if the topic is not registered on the server.
    pub fn subscribe(&self, topic: &str) -> Result<ShmPoolSubscription, CrossbarError> {
        self.subscriber.subscribe(topic)
    }
}
