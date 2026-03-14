#![allow(unsafe_code)]

mod notify;
mod pubsub;
mod region;

pub use pubsub::{
    PubSubConfig, ShmLoan, ShmPublisher, ShmSample, ShmSampleRef, ShmSubscriber, ShmSubscription,
    TopicHandle,
};
pub use region::ShmConfig;

use crate::error::CrossbarError;
use crate::router::Router;
use crate::types::{Method, Request, Response};
use bytes::Bytes;
use region::{ShmRegion, FREE, PROCESSING, REQUEST_READY, RESPONSE_READY};
use std::io;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// A handle that stops a running [`ShmServer`] when dropped.
///
/// Returned by [`ShmServer::spawn`] and [`ShmServer::spawn_with_config`].
/// Dropping this handle signals the server's polling loop to exit.
pub struct ShmHandle {
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl ShmHandle {
    /// Signals the server to stop.
    pub fn stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
    }
}

impl Drop for ShmHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Shared-memory IPC server (V2 — block pool, zero-copy reads).
///
/// Creates a memory-mapped region and serves requests from any [`ShmClient`]
/// that attaches to the same name. Achieves low-microsecond latency by
/// transferring only block indices in coordination slots and using
/// `Bytes::from_owner` for O(1) zero-copy reads.
///
/// Only available on Unix targets with the `shm` feature enabled.
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
/// ShmServer::bind("myapp", router).await.unwrap();
/// # }
/// ```
pub struct ShmServer;

impl ShmServer {
    /// Creates the shared memory region and serves requests forever.
    pub async fn bind(name: &str, router: Router) -> io::Result<()> {
        Self::bind_with_config(name, router, ShmConfig::default()).await
    }

    /// Like `bind` but with custom configuration.
    pub async fn bind_with_config(name: &str, router: Router, config: ShmConfig) -> io::Result<()> {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Self::serve(name, router, config, stop).await
    }

    /// Spawns the server in a background task and returns an [`ShmHandle`]
    /// that stops the server when dropped.
    pub async fn spawn(name: &str, router: Router) -> io::Result<ShmHandle> {
        Self::spawn_with_config(name, router, ShmConfig::default()).await
    }

    /// Like `spawn` but with custom configuration.
    pub async fn spawn_with_config(
        name: &str,
        router: Router,
        config: ShmConfig,
    ) -> io::Result<ShmHandle> {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = ShmHandle {
            stop: Arc::clone(&stop),
        };

        let region = Arc::new(
            ShmRegion::create(name, &config).map_err(|e| io::Error::other(e.to_string()))?,
        );
        region.update_heartbeat();

        Self::start_background_tasks(&region, &config, Arc::clone(&stop));
        Self::start_server_loop(region, router, stop);

        Ok(handle)
    }

    async fn serve(
        name: &str,
        router: Router,
        config: ShmConfig,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) -> io::Result<()> {
        let region = Arc::new(
            ShmRegion::create(name, &config).map_err(|e| io::Error::other(e.to_string()))?,
        );
        region.update_heartbeat();

        Self::start_background_tasks(&region, &config, Arc::clone(&stop));

        let rt_handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            Self::poll_loop(&region, &router, &rt_handle, &stop);
        })
        .await
        .map_err(|e| io::Error::other(e.to_string()))?;

        Ok(())
    }

    fn start_background_tasks(
        region: &Arc<ShmRegion>,
        config: &ShmConfig,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) {
        let hb_region = Arc::clone(region);
        let hb_interval = config.heartbeat_interval;
        let hb_stop = Arc::clone(&stop);
        tokio::spawn(async move {
            while !hb_stop.load(Ordering::Acquire) {
                hb_region.update_heartbeat();
                tokio::time::sleep(hb_interval).await;
            }
        });

        let recovery_region = Arc::clone(region);
        let stale_timeout = config.stale_timeout;
        let rec_stop = stop;
        tokio::spawn(async move {
            while !rec_stop.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_secs(1)).await;
                recovery_region.recover_stale_slots(stale_timeout);
            }
        });
    }

    fn start_server_loop(
        region: Arc<ShmRegion>,
        router: Router,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) {
        let rt_handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            Self::poll_loop(&region, &router, &rt_handle, &stop);
        });
    }

    fn poll_loop(
        region: &Arc<ShmRegion>,
        router: &Router,
        rt_handle: &tokio::runtime::Handle,
        stop: &std::sync::atomic::AtomicBool,
    ) {
        while !stop.load(Ordering::Acquire) {
            let mut found_work = false;
            for slot_idx in 0..region.slot_count {
                let state = region.slot_state(slot_idx);

                if state
                    .compare_exchange(
                        REQUEST_READY,
                        PROCESSING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    found_work = true;
                    region.touch_slot(slot_idx);

                    // Read request from block — O(1) zero-copy body
                    let req = match region.read_request_from_block(slot_idx) {
                        Ok(r) => r,
                        Err(_) => {
                            // Malformed request — allocate a response block for the error
                            let resp = Response::bad_request("malformed shm request");
                            match region.alloc_block() {
                                Some(resp_block_idx) => {
                                    region.write_response_to_block(slot_idx, resp_block_idx, &resp);
                                }
                                None => {
                                    // Pool exhausted even for error response.
                                    // Set a minimal response in the slot metadata.
                                    region.set_response_status(slot_idx, 503);
                                    region.set_response_body_len(slot_idx, 0);
                                    region.set_response_headers_data_len(slot_idx, 0);
                                    region.set_response_block_idx(slot_idx, u32::MAX);
                                }
                            }
                            state.store(RESPONSE_READY, Ordering::Release);
                            notify::wake_one(state);
                            continue;
                        }
                    };

                    // Dispatch through router
                    // (request body Bytes holds ShmBlockGuard — block freed when req dropped)
                    let resp = rt_handle.block_on(router.dispatch(req));
                    // req dropped here → request block freed back to pool

                    // Allocate response block and write response
                    match region.alloc_block() {
                        Some(resp_block_idx) => {
                            region.write_response_to_block(slot_idx, resp_block_idx, &resp);
                        }
                        None => {
                            // Pool exhausted for response
                            region.set_response_status(slot_idx, 503);
                            region.set_response_body_len(slot_idx, 0);
                            region.set_response_headers_data_len(slot_idx, 0);
                            region.set_response_block_idx(slot_idx, u32::MAX);
                        }
                    }
                    state.store(RESPONSE_READY, Ordering::Release);
                    notify::wake_one(state);
                }
            }

            if !found_work {
                std::thread::sleep(Duration::from_micros(1));
            }
        }
    }
}

/// Shared-memory IPC client (V2 — block pool, zero-copy reads).
///
/// Attaches to an existing shared memory region created by [`ShmServer`].
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// # #[tokio::main] async fn main() -> Result<(), CrossbarError> {
/// let client = ShmClient::connect("myapp").await?;
/// let resp = client.get("/health").await?;
/// println!("{}", resp.status);
/// # Ok(())
/// # }
/// ```
pub struct ShmClient {
    region: Arc<ShmRegion>,
    client_id: u64,
    stale_timeout: Duration,
}

impl ShmClient {
    /// Connects to an existing shared memory region.
    pub async fn connect(name: &str) -> Result<Self, CrossbarError> {
        Self::connect_with_timeout(name, Duration::from_secs(5)).await
    }

    /// Connects with a custom stale timeout.
    pub async fn connect_with_timeout(
        name: &str,
        stale_timeout: Duration,
    ) -> Result<Self, CrossbarError> {
        let region = ShmRegion::open(name)?;
        region.check_heartbeat(stale_timeout)?;

        let client_id = std::process::id() as u64
            ^ (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64);

        Ok(ShmClient {
            region: Arc::new(region),
            client_id,
            stale_timeout,
        })
    }

    /// Sends a request via shared memory and waits for the response.
    pub async fn request(&self, req: Request) -> Result<Response, CrossbarError> {
        // Check server is alive
        self.region.check_heartbeat(self.stale_timeout)?;

        // Allocate a request block from the pool
        let req_block_idx = self
            .region
            .alloc_block()
            .ok_or(CrossbarError::ShmPoolExhausted)?;

        // Acquire a slot (with brief retry)
        let slot_idx = {
            let mut attempts = 0;
            loop {
                if let Some(idx) = self.region.try_acquire_slot(self.client_id) {
                    break idx;
                }
                attempts += 1;
                if attempts > 1000 {
                    // Return the block before failing
                    self.region.free_block(req_block_idx);
                    return Err(CrossbarError::ShmSlotsFull);
                }
                tokio::task::yield_now().await;
            }
        };

        // Write request into block (state is WRITING from try_acquire_slot)
        if let Err(e) = self
            .region
            .write_request_to_block(slot_idx, req_block_idx, &req)
        {
            // Free the block and release the slot on failure
            self.region.free_block(req_block_idx);
            self.region
                .slot_state(slot_idx)
                .store(FREE, Ordering::Release);
            return Err(e);
        }

        // Transition to REQUEST_READY so the server picks it up
        let state_atom = self.region.slot_state(slot_idx);
        state_atom.store(REQUEST_READY, Ordering::Release);
        notify::wake_one(state_atom);

        // Wait for response — spawn_blocking to avoid blocking tokio
        let region = Arc::clone(&self.region);
        let stale_timeout = self.stale_timeout;
        let resp = tokio::task::spawn_blocking(move || {
            let state = region.slot_state(slot_idx);
            let deadline = std::time::Instant::now() + stale_timeout;

            loop {
                let current = state.load(Ordering::Acquire);
                if current == RESPONSE_READY {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = state.compare_exchange(
                        REQUEST_READY,
                        FREE,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    return Err(CrossbarError::ShmServerDead);
                }
                if current == PROCESSING || current == REQUEST_READY {
                    notify::wait_until_not(state, current, Duration::from_millis(50)).ok();
                } else {
                    return Err(CrossbarError::ShmServerDead);
                }
            }

            // Read response — O(1) zero-copy body via Bytes::from_owner
            let resp_block_idx = region.response_block_idx(slot_idx);
            let resp = if resp_block_idx == u32::MAX {
                // Server couldn't allocate a response block (503 fallback)
                let status = region.response_status(slot_idx);
                Ok(Response::with_status(status))
            } else {
                region.read_response_from_block(slot_idx)
            };

            // Release slot
            state.store(FREE, Ordering::Release);
            notify::wake_one(state);

            resp
        })
        .await
        .map_err(|_| CrossbarError::ShmServerDead)??;

        Ok(resp)
    }

    /// Convenience method for `GET` requests.
    pub async fn get(&self, uri: &str) -> Result<Response, CrossbarError> {
        self.request(Request::new(Method::Get, uri)).await
    }

    /// Convenience method for `POST` requests with a body.
    pub async fn post(&self, uri: &str, body: impl Into<Bytes>) -> Result<Response, CrossbarError> {
        self.request(Request::new(Method::Post, uri).with_body(body))
            .await
    }
}
