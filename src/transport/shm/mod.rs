#![allow(unsafe_code)]

mod mmap;
mod notify;
mod pool_pubsub;
mod pubsub;
pub(crate) mod region;

#[allow(unused_imports)] // used by consumers, not internally
pub use pool_pubsub::{
    PoolPubSubConfig, PoolTopicHandle, ShmPoolPublisher, ShmPoolSampleGuard, ShmPoolSubscriber,
    ShmPoolSubscription,
};
pub use pubsub::{
    PubSubConfig, ShmLoan, ShmPublisher, ShmSample, ShmSampleRef, ShmSubscriber, ShmSubscription,
    TopicHandle,
};
pub use region::ShmConfig;

use crate::error::CrossbarError;
use crate::router::Router;
use crate::types::{Body, Method, Request, Response};
use region::{ShmRegion, FREE, NO_BLOCK, PROCESSING, REQUEST_READY, RESPONSE_READY};
use std::future::Future;
use std::io;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// A handle that stops a running [`ShmServer`] when dropped.
///
/// Returned by [`ShmServer::spawn`] and [`ShmServer::spawn_with_config`].
/// Dropping this handle signals the server's polling loop to exit.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// async fn health() -> &'static str { "ok" }
/// let router = Router::new().route("/health", get(health));
///
/// let handle = ShmServer::spawn("myapp", router).await?;
/// // Server runs in the background...
/// handle.stop(); // explicitly stop, or let `handle` drop
/// # Ok(())
/// # }
/// ```
pub struct ShmHandle {
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl ShmHandle {
    /// Signals the server to stop.
    ///
    /// This is idempotent — calling it multiple times is safe. The server
    /// exits at the end of its current poll iteration.
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
/// `Body::Mmap` for O(1) zero-copy reads.
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
    /// This is a blocking call that returns only when the server is shut down.
    /// For non-blocking usage, see [`ShmServer::spawn`].
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the shared memory region cannot be created
    /// (e.g., permission denied, file system full).
    pub async fn bind(name: &str, router: Router) -> io::Result<()> {
        Self::bind_with_config(name, router, ShmConfig::default()).await
    }

    /// Like [`ShmServer::bind`] but with custom [`ShmConfig`].
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the shared memory region cannot be created.
    pub async fn bind_with_config(name: &str, router: Router, config: ShmConfig) -> io::Result<()> {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Self::serve(name, router, config, stop).await
    }

    /// Spawns the server in a background task and returns an [`ShmHandle`]
    /// that stops the server when dropped.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the shared memory region cannot be created.
    pub async fn spawn(name: &str, router: Router) -> io::Result<ShmHandle> {
        Self::spawn_with_config(name, router, ShmConfig::default()).await
    }

    /// Like [`ShmServer::spawn`] but with custom [`ShmConfig`].
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the shared memory region cannot be created.
    #[allow(clippy::unused_async)] // async for API consistency with other SHM methods
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

    /// Dispatches a request, bypassing tokio when the handler resolves immediately.
    ///
    /// Most handlers resolve on first poll (no yield points). For these,
    /// a single `poll()` with a no-op waker is sufficient — no tokio runtime
    /// context needed. Only truly async handlers (that return `Pending`) fall
    /// back to `block_on`.
    #[inline]
    fn dispatch_fast(
        router: &Router,
        req: Request,
        rt_handle: &tokio::runtime::Handle,
    ) -> Response {
        let mut fut = std::pin::pin!(router.dispatch(req));
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        match fut.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(resp) => resp,
            std::task::Poll::Pending => rt_handle.block_on(fut),
        }
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

                    let Ok(req) = region.read_request_from_block(slot_idx) else {
                        region.set_request_block_idx(slot_idx, NO_BLOCK);
                        let resp = Response::bad_request("malformed shm request");
                        if let Some(resp_block_idx) = region.alloc_block() {
                            region.write_response_to_block(slot_idx, resp_block_idx, &resp);
                        } else {
                            region.set_response_status(slot_idx, 503);
                            region.set_response_body_len(slot_idx, 0);
                            region.set_response_headers_data_len(slot_idx, 0);
                            region.set_response_block_idx(slot_idx, u32::MAX);
                        }
                        state.store(RESPONSE_READY, Ordering::Release);
                        continue;
                    };

                    // Try-poll: skip tokio for sync-like handlers
                    let resp = Self::dispatch_fast(router, req, rt_handle);

                    if let Some(resp_block_idx) = region.alloc_block() {
                        region.write_response_to_block(slot_idx, resp_block_idx, &resp);
                    } else {
                        region.set_response_status(slot_idx, 503);
                        region.set_response_body_len(slot_idx, 0);
                        region.set_response_headers_data_len(slot_idx, 0);
                        region.set_response_block_idx(slot_idx, u32::MAX);
                    }
                    state.store(RESPONSE_READY, Ordering::Release);
                }
            }

            if !found_work {
                region.wait_for_work();
            }
        }
    }
}

/// An in-flight request submitted to the poller thread.
struct InflightRequest {
    slot_idx: u32,
    tx: Option<tokio::sync::oneshot::Sender<Result<Response, CrossbarError>>>,
    deadline: std::time::Instant,
}

// -- Poller wake mechanism (eventfd on Linux, pipe on other Unix) --

/// Zero-latency wake for the poller thread.
///
/// Uses `eventfd` on Linux (single fd, 8-byte counter) or a self-pipe on
/// other Unix. Either way, `wake()` is a single write syscall and
/// `try_drain()` is a non-blocking read that resets the wake signal.
struct PollerWake {
    #[cfg(target_os = "linux")]
    fd: std::os::fd::OwnedFd,
    #[cfg(all(unix, not(target_os = "linux")))]
    read_fd: std::os::fd::OwnedFd,
    #[cfg(all(unix, not(target_os = "linux")))]
    write_fd: std::os::fd::OwnedFd,
}

#[cfg(target_os = "linux")]
impl PollerWake {
    fn new() -> io::Result<Self> {
        use std::os::fd::FromRawFd;
        // SAFETY: eventfd returns a valid fd or -1 on error.
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is valid (checked above).
        Ok(Self {
            fd: unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) },
        })
    }

    /// Signal the poller thread to wake up (non-blocking, idempotent).
    fn wake(&self) {
        use std::os::fd::AsRawFd;
        let val: u64 = 1;
        // SAFETY: writing 8 bytes to an eventfd is well-defined.
        unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                std::ptr::from_ref(&val).cast(),
                std::mem::size_of::<u64>(),
            );
        }
    }

    /// Drain the eventfd counter (non-blocking). Returns true if a wake
    /// was pending.
    fn try_drain(&self) -> bool {
        use std::os::fd::AsRawFd;
        let mut val: u64 = 0;
        // SAFETY: reading 8 bytes from an eventfd is well-defined.
        let n = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                std::ptr::from_mut(&mut val).cast(),
                std::mem::size_of::<u64>(),
            )
        };
        n > 0
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
impl PollerWake {
    fn new() -> io::Result<Self> {
        use std::os::fd::FromRawFd;
        let mut fds = [0i32; 2];
        // SAFETY: pipe2 writes two valid fds or returns -1.
        let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fds are valid (checked above).
        Ok(Self {
            read_fd: unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[0]) },
            write_fd: unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[1]) },
        })
    }

    fn wake(&self) {
        use std::os::fd::AsRawFd;
        let buf: [u8; 1] = [1];
        // SAFETY: writing 1 byte to a pipe is well-defined.
        unsafe {
            libc::write(self.write_fd.as_raw_fd(), buf.as_ptr().cast(), 1);
        }
    }

    fn try_drain(&self) -> bool {
        use std::os::fd::AsRawFd;
        let mut buf = [0u8; 64];
        // SAFETY: reading from a pipe is well-defined.
        let n = unsafe { libc::read(self.read_fd.as_raw_fd(), buf.as_mut_ptr().cast(), 64) };
        n > 0
    }
}

// Send + Sync are safe: the fd(s) are only accessed via atomic-like
// read/write syscalls (no shared mutable state).
unsafe impl Send for PollerWake {}
unsafe impl Sync for PollerWake {}

/// Shared-memory IPC client (V2 — dedicated poller, zero-copy reads).
///
/// Attaches to an existing shared memory region created by [`ShmServer`].
/// Uses a dedicated background thread to poll for responses instead of
/// `spawn_blocking`, eliminating ~15-20 µs of tokio threadpool overhead
/// per request.
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
    poller_tx: std::sync::mpsc::Sender<InflightRequest>,
    poller_stop: Arc<std::sync::atomic::AtomicBool>,
    poller_wake: Arc<PollerWake>,
    poller_thread: Option<std::thread::JoinHandle<()>>,
    /// Counter-based heartbeat: only check every 1024 requests (like pub/sub).
    request_count: std::sync::atomic::AtomicU32,
}

impl Drop for ShmClient {
    fn drop(&mut self) {
        self.poller_stop
            .store(true, std::sync::atomic::Ordering::Release);
        self.poller_wake.wake();
        if let Some(handle) = self.poller_thread.take() {
            let _ = handle.join();
        }
    }
}

impl ShmClient {
    /// Connects to an existing shared memory region.
    ///
    /// Spawns a dedicated poller thread for low-latency response polling.
    /// Uses the default stale timeout of 5 seconds. For a custom timeout, use
    /// [`ShmClient::connect_with_timeout`].
    ///
    /// # Errors
    ///
    /// Returns [`CrossbarError::Io`] if the shared memory file cannot be
    /// opened, [`CrossbarError::ShmInvalidRegion`] if the region header is
    /// invalid, or [`CrossbarError::ShmServerDead`] if the server heartbeat
    /// is stale.
    pub async fn connect(name: &str) -> Result<Self, CrossbarError> {
        Self::connect_with_timeout(name, Duration::from_secs(5)).await
    }

    /// Connects with a custom stale timeout.
    ///
    /// # Errors
    ///
    /// Returns [`CrossbarError::Io`] if the shared memory file cannot be
    /// opened, [`CrossbarError::ShmInvalidRegion`] if the region header is
    /// invalid, or [`CrossbarError::ShmServerDead`] if the server heartbeat
    /// is stale.
    #[allow(clippy::unused_async)] // async for API consistency with other SHM methods
    pub async fn connect_with_timeout(
        name: &str,
        stale_timeout: Duration,
    ) -> Result<Self, CrossbarError> {
        let region = ShmRegion::open(name)?;
        region.check_heartbeat(stale_timeout)?;

        // Truncating nanos to u64 is intentional — only used for uniqueness.
        #[allow(clippy::cast_possible_truncation)]
        let client_id = u64::from(std::process::id())
            ^ (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64);

        let region = Arc::new(region);
        let poller_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let poller_wake = Arc::new(PollerWake::new().map_err(CrossbarError::Io)?);
        let (poller_tx, poller_rx) = std::sync::mpsc::channel::<InflightRequest>();

        // Spawn dedicated poller thread
        let poller_region = Arc::clone(&region);
        let poller_stop_clone = Arc::clone(&poller_stop);
        let poller_wake_clone = Arc::clone(&poller_wake);
        let poller_thread = std::thread::Builder::new()
            .name("crossbar-poller".into())
            .spawn(move || {
                Self::poller_loop(
                    poller_region,
                    poller_rx,
                    poller_stop_clone,
                    poller_wake_clone,
                );
            })
            .map_err(CrossbarError::Io)?;

        Ok(ShmClient {
            region,
            client_id,
            stale_timeout,
            poller_tx,
            poller_stop,
            poller_wake,
            poller_thread: Some(poller_thread),
            request_count: std::sync::atomic::AtomicU32::new(0),
        })
    }

    /// Adaptive spin constants for the poller loop.
    const SPIN_ITERS: u32 = 64;
    const YIELD_ITERS: u32 = 8;
    const PARK_MIN_US: u64 = 50;
    const PARK_MAX_US: u64 = 5000;

    /// Dedicated response poller — runs on its own OS thread.
    ///
    /// Uses a three-phase adaptive spin strategy:
    /// 1. **Tight spin** (`spin_loop()`) for `SPIN_ITERS` iterations — fastest
    ///    response detection, keeps the cache line hot.
    /// 2. **Yield** (`yield_now()`) for `YIELD_ITERS` iterations — lets other
    ///    threads on the same core make progress.
    /// 3. **Park with exponential backoff** — starts at `PARK_MIN_US`, doubles
    ///    up to `PARK_MAX_US`. Resets to phase 1 on any completed response or
    ///    new request arrival (via eventfd/pipe wake).
    fn poller_loop(
        region: Arc<ShmRegion>,
        rx: std::sync::mpsc::Receiver<InflightRequest>,
        stop: Arc<std::sync::atomic::AtomicBool>,
        wake: Arc<PollerWake>,
    ) {
        let mut inflight: Vec<InflightRequest> = Vec::with_capacity(16);
        let mut miss_count: u32 = 0;

        while !stop.load(Ordering::Relaxed) {
            // Check for new-request wake signal
            let woke = wake.try_drain();

            // Drain new requests (non-blocking)
            while let Ok(req) = rx.try_recv() {
                inflight.push(req);
                miss_count = 0; // reset backoff on new work
            }

            if inflight.is_empty() {
                // Nothing to poll — park until a new request arrives or stop.
                // Use park_timeout so the stop flag is checked periodically.
                std::thread::park_timeout(Duration::from_millis(10));
                continue;
            }

            // Poll all in-flight slots
            let mut any_completed = false;
            inflight.retain_mut(|req| {
                let state = region.slot_state(req.slot_idx);
                let current = state.load(Ordering::Acquire);

                if current == RESPONSE_READY {
                    any_completed = true;

                    // Read response — O(1) zero-copy body via Body::Mmap
                    let resp_block_idx = region.response_block_idx(req.slot_idx);
                    let resp = if resp_block_idx == u32::MAX {
                        let status = region.response_status(req.slot_idx);
                        Ok(Response::with_status(status))
                    } else {
                        region.read_response_from_block(req.slot_idx)
                    };

                    // Clear block indices before releasing slot
                    region.set_request_block_idx(req.slot_idx, NO_BLOCK);
                    region.set_response_block_idx(req.slot_idx, NO_BLOCK);
                    state.store(FREE, Ordering::Release);

                    if let Some(tx) = req.tx.take() {
                        let _ = tx.send(resp);
                    }
                    return false; // remove from inflight
                }

                // Check timeout
                if std::time::Instant::now() >= req.deadline {
                    // Try to reclaim the slot
                    if state
                        .compare_exchange(REQUEST_READY, FREE, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        let req_block = region.request_block_idx(req.slot_idx);
                        if req_block != NO_BLOCK {
                            region.free_block(req_block);
                        }
                        region.set_request_block_idx(req.slot_idx, NO_BLOCK);
                        region.set_response_block_idx(req.slot_idx, NO_BLOCK);
                    }
                    // If CAS failed, server owns slot — stale recovery cleans up
                    if let Some(tx) = req.tx.take() {
                        let _ = tx.send(Err(CrossbarError::ShmServerDead));
                    }
                    return false;
                }

                // Unexpected state
                if current != PROCESSING && current != REQUEST_READY {
                    if let Some(tx) = req.tx.take() {
                        let _ = tx.send(Err(CrossbarError::ShmServerDead));
                    }
                    return false;
                }

                true // keep polling
            });

            if any_completed || woke {
                // Response found or new work arrived — reset adaptive backoff
                miss_count = 0;
            } else if !inflight.is_empty() {
                // Adaptive three-phase backoff
                miss_count = miss_count.saturating_add(1);

                if miss_count <= Self::SPIN_ITERS {
                    // Phase 1: tight spin — keeps cache line hot
                    core::hint::spin_loop();
                } else if miss_count <= Self::SPIN_ITERS + Self::YIELD_ITERS {
                    // Phase 2: yield — let sibling hyperthreads run
                    std::thread::yield_now();
                } else {
                    // Phase 3: park with exponential backoff
                    let backoff_step = miss_count - Self::SPIN_ITERS - Self::YIELD_ITERS;
                    let park_us = Self::PARK_MIN_US
                        .saturating_mul(1u64.wrapping_shl(backoff_step.min(16)))
                        .min(Self::PARK_MAX_US);
                    std::thread::park_timeout(Duration::from_micros(park_us));
                }
            }
        }
    }

    /// Sends a request via shared memory and waits for the response.
    ///
    /// # Errors
    ///
    /// Returns [`CrossbarError::ShmServerDead`] if the server heartbeat is
    /// stale or the response times out, [`CrossbarError::ShmPoolExhausted`]
    /// if no data blocks are available, [`CrossbarError::ShmSlotsFull`] if
    /// all coordination slots are occupied, or
    /// [`CrossbarError::ShmMessageTooLarge`] if the request exceeds block
    /// capacity.
    pub async fn request(&self, req: Request) -> Result<Response, CrossbarError> {
        // Counter-based heartbeat: only check every 1024 requests (~20 ns saved)
        let count = self
            .request_count
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        if count & 0x3FF == 0 {
            self.region.check_heartbeat(self.stale_timeout)?;
        }

        // Allocate a request block from the pool
        let req_block_idx = self
            .region
            .alloc_block()
            .ok_or(CrossbarError::ShmPoolExhausted)?;

        // Acquire a slot (with bounded retry)
        let slot_idx = {
            let deadline = std::time::Instant::now() + Duration::from_millis(100);
            loop {
                if let Some(idx) = self.region.try_acquire_slot(self.client_id) {
                    break idx;
                }
                if std::time::Instant::now() >= deadline {
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
            self.region.free_block(req_block_idx);
            self.region
                .slot_state(slot_idx)
                .store(FREE, Ordering::Release);
            return Err(e);
        }

        // Transition to REQUEST_READY and notify server
        let state_atom = self.region.slot_state(slot_idx);
        state_atom.store(REQUEST_READY, Ordering::Release);
        self.region.notify_server();

        // Submit to dedicated poller thread (not spawn_blocking)
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.poller_tx
            .send(InflightRequest {
                slot_idx,
                tx: Some(tx),
                deadline: std::time::Instant::now() + self.stale_timeout,
            })
            .map_err(|_| CrossbarError::ShmServerDead)?;

        // Wake the poller thread via eventfd/pipe (zero-latency)
        self.poller_wake.wake();

        // Await response — non-blocking in tokio
        rx.await.map_err(|_| CrossbarError::ShmServerDead)?
    }

    /// Convenience method for `GET` requests.
    ///
    /// # Errors
    ///
    /// See [`ShmClient::request`] for the full list of error conditions.
    pub async fn get(&self, uri: &str) -> Result<Response, CrossbarError> {
        self.request(Request::new(Method::Get, uri)).await
    }

    /// Convenience method for `POST` requests with a body.
    ///
    /// # Errors
    ///
    /// See [`ShmClient::request`] for the full list of error conditions.
    pub async fn post(&self, uri: &str, body: impl Into<Body>) -> Result<Response, CrossbarError> {
        self.request(Request::new(Method::Post, uri).with_body(body))
            .await
    }
}
