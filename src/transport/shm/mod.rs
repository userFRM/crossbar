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

/// Shared-memory IPC server.
///
/// Creates a memory-mapped region and serves requests from any [`ShmClient`]
/// that attaches to the same name. Achieves low-microsecond latency for small
/// payloads by avoiding kernel data copies.
///
/// The server uses a single blocking thread that polls all slots in a loop.
/// Handlers are invoked sequentially via `block_on`, so a slow handler will
/// delay processing of other slots. For workloads with consistently fast
/// handlers (the typical shm use case), this is optimal; for slow handlers,
/// consider a socket-based transport instead.
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
    ///
    /// `name` is used to derive the file path (`/dev/shm/crossbar-{name}` on
    /// Linux, `/tmp/crossbar-shm-{name}` on other platforms).
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
    ///
    /// Unlike [`bind`](Self::bind), this method returns immediately.
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

        // Create the region synchronously so errors propagate to the caller.
        let region = Arc::new(
            ShmRegion::create(name, &config).map_err(|e| io::Error::other(e.to_string()))?,
        );
        region.update_heartbeat();

        Self::start_background_tasks(&region, &config, Arc::clone(&stop));
        Self::start_server_loop(region, router, stop);

        Ok(handle)
    }

    /// Internal: runs the server loop (blocks the current task forever or
    /// until `stop` is set).
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

        // Server polling loop — runs on a blocking thread to avoid
        // starving the tokio runtime with spin-waiting.
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
        // Spawn heartbeat updater
        let hb_region = Arc::clone(region);
        let hb_interval = config.heartbeat_interval;
        let hb_stop = Arc::clone(&stop);
        tokio::spawn(async move {
            while !hb_stop.load(Ordering::Acquire) {
                hb_region.update_heartbeat();
                tokio::time::sleep(hb_interval).await;
            }
        });

        // Spawn stale slot recovery
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
        region: &ShmRegion,
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

                    // Refresh timestamp so stale recovery doesn't
                    // reclaim this slot while the handler runs.
                    region.touch_slot(slot_idx);

                    // Read request from slot
                    let req = match region.read_request_from_slot(slot_idx) {
                        Ok(r) => r,
                        Err(_) => {
                            // Malformed request — write a 400 response
                            let resp = Response::bad_request("malformed shm request");
                            region.write_response_to_slot(slot_idx, &resp);
                            state.store(RESPONSE_READY, Ordering::Release);
                            notify::wake_one(state);
                            continue;
                        }
                    };

                    // Dispatch through router
                    let resp = rt_handle.block_on(router.dispatch(req));

                    // Write response to slot
                    region.write_response_to_slot(slot_idx, &resp);
                    state.store(RESPONSE_READY, Ordering::Release);
                    notify::wake_one(state);
                }
            }

            if !found_work {
                // Brief pause when idle to avoid 100% CPU.
                // yield_now() alone is a near-no-op on Linux — it issues
                // sched_yield() which re-runs immediately if nothing else
                // is scheduled, burning 100% CPU.  A short sleep keeps
                // idle overhead negligible while adding < 1 µs to latency
                // when a request arrives mid-sleep.
                std::thread::sleep(Duration::from_micros(1));
            }
        }
    }
}

/// Shared-memory IPC client.
///
/// Attaches to an existing shared memory region created by [`ShmServer`].
/// Each request acquires its own slot — no internal Mutex needed, so
/// multiple callers can submit requests concurrently without client-side
/// contention. Note that the server processes slots sequentially, so
/// server-side throughput is bounded by handler latency.
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
    ///
    /// # Errors
    ///
    /// Returns [`CrossbarError::ShmInvalidRegion`] if the region doesn't exist
    /// or has invalid metadata. Returns [`CrossbarError::ShmServerDead`] if the
    /// server heartbeat is stale.
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
    ///
    /// # Errors
    ///
    /// - [`CrossbarError::ShmServerDead`] if the server heartbeat is stale.
    /// - [`CrossbarError::ShmSlotsFull`] if no slots are available.
    /// - [`CrossbarError::ShmMessageTooLarge`] if the request exceeds slot capacity.
    /// - [`CrossbarError::HeaderOverflow`] if a header key/value exceeds u16::MAX bytes.
    pub async fn request(&self, req: Request) -> Result<Response, CrossbarError> {
        // Check server is alive
        self.region.check_heartbeat(self.stale_timeout)?;

        // Check message fits
        let uri_bytes = req.uri.raw().as_bytes();
        let headers_data = super::serialize_headers(&req.headers)?;
        let total = uri_bytes.len() + headers_data.len() + req.body.len();
        if total > self.region.slot_data_capacity as usize {
            return Err(CrossbarError::ShmMessageTooLarge {
                size: total,
                max: self.region.slot_data_capacity as usize,
            });
        }

        // Acquire a slot (with brief retry)
        let slot_idx = {
            let mut attempts = 0;
            loop {
                if let Some(idx) = self.region.try_acquire_slot(self.client_id) {
                    break idx;
                }
                attempts += 1;
                if attempts > 1000 {
                    return Err(CrossbarError::ShmSlotsFull);
                }
                tokio::task::yield_now().await;
            }
        };

        // Write request into slot (state is WRITING from try_acquire_slot)
        self.region.write_request_to_slot(slot_idx, &req)?;

        // Data is written — transition to REQUEST_READY so the server picks it up
        let state_atom = self.region.slot_state(slot_idx);
        state_atom.store(REQUEST_READY, Ordering::Release);
        notify::wake_one(state_atom);

        // Wait for response — spawn_blocking to avoid blocking tokio
        let region = Arc::clone(&self.region);
        let stale_timeout = self.stale_timeout;
        let resp = tokio::task::spawn_blocking(move || {
            let state = region.slot_state(slot_idx);
            let deadline = std::time::Instant::now() + stale_timeout;

            // Wait until the slot reaches RESPONSE_READY.
            // It may still be in REQUEST_READY (server hasn't picked it up)
            // or PROCESSING (server is working on it).
            loop {
                let current = state.load(Ordering::Acquire);
                if current == RESPONSE_READY {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    // Only reclaim if the slot is still client-owned
                    // (REQUEST_READY = server hasn't picked it up yet).
                    // If the server already CAS'd to PROCESSING, it owns
                    // the slot — we must NOT free it or the server could
                    // later write its response into a slot that another
                    // request has reused. Stale recovery will eventually
                    // clean up the abandoned RESPONSE_READY slot after
                    // the handler finishes.
                    let _ = state.compare_exchange(
                        REQUEST_READY,
                        FREE,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    return Err(CrossbarError::ShmServerDead);
                }
                // Use futex/spin wait based on current state
                if current == PROCESSING || current == REQUEST_READY {
                    // ignore timeout, we check deadline above
                    notify::wait_until_not(state, current, Duration::from_millis(50)).ok();
                } else {
                    // Unexpected state — don't force-free, just bail.
                    // Stale recovery handles cleanup.
                    return Err(CrossbarError::ShmServerDead);
                }
            }

            let resp = region.read_response_from_slot(slot_idx)?;

            // Release slot
            state.store(FREE, Ordering::Release);
            notify::wake_one(state);

            Ok(resp)
        })
        .await
        .map_err(|_| CrossbarError::ShmServerDead)??;

        Ok(resp)
    }

    /// Convenience method for `GET` requests.
    ///
    /// # Errors
    ///
    /// Returns a [`CrossbarError`] on failure.
    pub async fn get(&self, uri: &str) -> Result<Response, CrossbarError> {
        self.request(Request::new(Method::Get, uri)).await
    }

    /// Convenience method for `POST` requests with a body.
    ///
    /// # Errors
    ///
    /// Returns a [`CrossbarError`] on failure.
    pub async fn post(&self, uri: &str, body: impl Into<Bytes>) -> Result<Response, CrossbarError> {
        self.request(Request::new(Method::Post, uri).with_body(body))
            .await
    }
}
