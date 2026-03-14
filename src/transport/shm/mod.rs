#![allow(unsafe_code)]

mod bidi;
mod mmap;
mod notify;
mod pool_pubsub;
mod pubsub;
pub(crate) mod region;

pub use bidi::{BidiClient, BidiConfig, BidiServer};
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

    /// Starts heartbeat and stale-recovery threads.
    ///
    /// Uses bare OS threads instead of tokio tasks — the server does not
    /// require a tokio runtime for its background work.
    fn start_background_tasks(
        region: &Arc<ShmRegion>,
        config: &ShmConfig,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) {
        let hb_region = Arc::clone(region);
        let hb_interval = config.heartbeat_interval;
        let hb_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("crossbar-heartbeat".into())
            .spawn(move || {
                while !hb_stop.load(Ordering::Acquire) {
                    hb_region.update_heartbeat();
                    std::thread::sleep(hb_interval);
                }
            })
            .expect("failed to spawn heartbeat thread");

        let recovery_region = Arc::clone(region);
        let stale_timeout = config.stale_timeout;
        let rec_stop = stop;
        std::thread::Builder::new()
            .name("crossbar-recovery".into())
            .spawn(move || {
                while !rec_stop.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_secs(1));
                    recovery_region.recover_stale_slots(stale_timeout);
                }
            })
            .expect("failed to spawn recovery thread");
    }

    fn start_server_loop(
        region: Arc<ShmRegion>,
        router: Router,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) {
        let rt_handle = tokio::runtime::Handle::current();
        std::thread::Builder::new()
            .name("crossbar-server".into())
            .spawn(move || {
                Self::poll_loop(&region, &router, &rt_handle, &stop);
            })
            .expect("failed to spawn server thread");
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
        // Enter the runtime context so handlers that use tokio timers,
        // IO, etc. can find the reactor during the initial poll.
        let _guard = rt_handle.enter();
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
        let n = region.slot_count;
        while !stop.load(Ordering::Acquire) {
            let mut found_work = false;
            let start = region.server_slot_hint().load(Ordering::Relaxed) % n;
            for offset in 0..n {
                let slot_idx = (start + offset) % n;
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
                    // Advance hint past this slot for next scan
                    region
                        .server_slot_hint()
                        .store(slot_idx.wrapping_add(1) % n, Ordering::Relaxed);
                    region.touch_slot(slot_idx);

                    let Ok(mut req) = region.read_request_from_block(slot_idx) else {
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

                    // Inject SHM region so handlers can allocate born-in-SHM responses
                    req.shm_region = Some(Arc::clone(region));

                    // Try-poll: skip tokio for sync-like handlers
                    let mut resp = Self::dispatch_fast(router, req, rt_handle);

                    // Check if the handler used born-in-SHM (Body::ShmDirect)
                    if let Body::ShmDirect(ref mut guard) = resp.body {
                        // Body is already in a pool block — just append headers
                        let block_idx = guard.take_block_idx();
                        region.write_response_direct(
                            slot_idx,
                            block_idx,
                            guard.body_len,
                            &resp.headers,
                            resp.status,
                        );
                    } else if let Some(resp_block_idx) = region.alloc_block() {
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

    /// Block until a wake signal arrives or `timeout_ms` elapses.
    fn wait(&self, timeout_ms: i32) {
        use std::os::fd::AsRawFd;
        let mut pfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll with a single fd is well-defined.
        unsafe {
            libc::poll(&mut pfd, 1, timeout_ms);
        }
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

    /// Block until a wake signal arrives or `timeout_ms` elapses.
    fn wait(&self, timeout_ms: i32) {
        use std::os::fd::AsRawFd;
        let mut pfd = libc::pollfd {
            fd: self.read_fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll with a single fd is well-defined.
        unsafe {
            libc::poll(&mut pfd, 1, timeout_ms);
        }
    }
}

// Send + Sync are safe: the fd(s) are only accessed via atomic-like
// read/write syscalls (no shared mutable state).
unsafe impl Send for PollerWake {}
unsafe impl Sync for PollerWake {}

/// Shared-memory IPC client (V2 — inline spin + poller fallback).
///
/// Attaches to an existing shared memory region created by [`ShmServer`].
/// Uses a two-phase response strategy:
/// 1. **Inline spin** (~2 µs) catches fast handlers without any channel overhead
/// 2. **Poller thread fallback** handles slow handlers via eventfd + oneshot
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

    /// Number of inline spin iterations in `request()` before falling back
    /// to the poller thread. At ~1 ns per iteration, 2048 ≈ 2 µs of
    /// spinning. Catches fast handlers (e.g. /health) without touching
    /// channels, oneshot, or eventfd.
    const INLINE_SPIN_ITERS: u32 = 2048;

    /// Adaptive spin constants for the poller loop (slow-path fallback).
    const SPIN_ITERS: u32 = 64;
    const YIELD_ITERS: u32 = 8;
    const PARK_MIN_US: u64 = 50;
    const PARK_MAX_US: u64 = 5000;

    /// Reads the response from a completed slot and releases it back to FREE.
    ///
    /// Shared by both the inline fast-spin path and the poller thread.
    #[inline]
    fn complete_response(
        region: &Arc<ShmRegion>,
        slot_idx: u32,
    ) -> Result<Response, CrossbarError> {
        // CAS to claim the slot — prevents race with stale recovery.
        // PROCESSING is skipped by recover_stale_slots and not targeted
        // by try_acquire_slot, so this is a safe intermediate state.
        if region
            .slot_state(slot_idx)
            .compare_exchange(
                RESPONSE_READY,
                PROCESSING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            // Stale recovery already reclaimed this slot
            return Err(CrossbarError::ShmServerDead);
        }

        let resp_block_idx = region.response_block_idx(slot_idx);
        let resp = if resp_block_idx == u32::MAX {
            let status = region.response_status(slot_idx);
            Ok(Response::with_status(status))
        } else {
            region.read_response_from_block(slot_idx)
        };

        // Clear block indices before releasing slot
        region.set_request_block_idx(slot_idx, NO_BLOCK);
        region.set_response_block_idx(slot_idx, NO_BLOCK);
        region.slot_state(slot_idx).store(FREE, Ordering::Release);

        resp
    }

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
                // Nothing to poll — block on the wake fd until a new request
                // arrives or 10ms elapses (so the stop flag is checked).
                wake.wait(10);
                continue;
            }

            // Poll all in-flight slots
            let mut any_completed = false;
            inflight.retain_mut(|req| {
                let state = region.slot_state(req.slot_idx);
                let current = state.load(Ordering::Acquire);

                if current == RESPONSE_READY {
                    any_completed = true;
                    let resp = Self::complete_response(&region, req.slot_idx);
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

    /// Allocates a pool block for writing request body directly into SHM.
    ///
    /// Returns `None` if the pool is exhausted. Use the returned loan to write
    /// data directly into the SHM block, then convert it to a `Body` and attach
    /// to a `Request`. This eliminates the heap→SHM copy on the request path.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use crossbar::prelude::*;
    /// # async fn example(client: &ShmClient) -> Result<(), CrossbarError> {
    /// if let Some(mut loan) = client.alloc_request_block() {
    ///     loan.as_mut_slice()[..4].copy_from_slice(b"data");
    ///     loan.set_len(4);
    ///     let resp = client.post("/path", loan).await?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn alloc_request_block(&self) -> Option<crate::types::ShmResponseLoan> {
        crate::types::ShmResponseLoan::new(&self.region)
    }

    /// Sends a request via shared memory and waits for the response.
    ///
    /// If the request body is [`Body::ShmDirect`], the body is already in
    /// the pool block and no copy is performed — O(1) transfer.
    pub async fn request(&self, mut req: Request) -> Result<Response, CrossbarError> {
        // Counter-based heartbeat: only check every 1024 requests (~20 ns saved)
        let count = self
            .request_count
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        if count & 0x3FF == 0 {
            self.region.check_heartbeat(self.stale_timeout)?;
        }

        // Acquire a slot FIRST — before any block allocation.
        // This ensures no block is held across an await point (yield_now),
        // preventing block leaks on async cancellation.
        let slot_idx = if let Some(idx) = self.region.try_acquire_slot(self.client_id) {
            idx
        } else {
            // Contention: retry with deadline
            let deadline = std::time::Instant::now() + Duration::from_millis(100);
            loop {
                if let Some(idx) = self.region.try_acquire_slot(self.client_id) {
                    break idx;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(CrossbarError::ShmSlotsFull);
                }
                tokio::task::yield_now().await;
            }
        };

        // Slot acquired — no more await points until response wait.
        // Now safe to extract/allocate the block.
        let (req_block_idx, direct_body_len) = if let Body::ShmDirect(ref mut guard) = req.body {
            // Verify the block belongs to this client's region (provenance check)
            if !Arc::ptr_eq(&guard.region, &self.region) {
                self.region
                    .slot_state(slot_idx)
                    .store(FREE, Ordering::Release);
                return Err(CrossbarError::ShmInvalidRegion(
                    "ShmDirect body belongs to a different SHM region".into(),
                ));
            }
            (guard.take_block_idx(), Some(guard.body_len))
        } else {
            match self.region.alloc_block() {
                Some(idx) => (idx, None),
                None => {
                    self.region
                        .slot_state(slot_idx)
                        .store(FREE, Ordering::Release);
                    return Err(CrossbarError::ShmPoolExhausted);
                }
            }
        };

        // Write request into block (state is WRITING from try_acquire_slot)
        let write_result = if let Some(body_len) = direct_body_len {
            // Born-in-SHM: body already at offset 0, just append URI + headers
            self.region.write_request_direct(
                slot_idx,
                req_block_idx,
                body_len,
                req.method,
                req.uri.raw(),
                &req.headers,
            )
        } else {
            self.region
                .write_request_to_block(slot_idx, req_block_idx, &req)
        };
        if let Err(e) = write_result {
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

        // ── Fast path: inline spin ──
        // Catches fast handlers (~2 µs window) without touching channels,
        // oneshot, or eventfd. Zero overhead for the common case.
        for _ in 0..Self::INLINE_SPIN_ITERS {
            if state_atom.load(Ordering::Acquire) == RESPONSE_READY {
                return Self::complete_response(&self.region, slot_idx);
            }
            core::hint::spin_loop();
        }

        // ── Slow path: poller thread fallback ──
        // Handler didn't respond within the spin window. Submit to the
        // dedicated poller thread which uses adaptive backoff.
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.poller_tx
            .send(InflightRequest {
                slot_idx,
                tx: Some(tx),
                deadline: std::time::Instant::now() + self.stale_timeout,
            })
            .map_err(|_| CrossbarError::ShmServerDead)?;

        // Wake the poller thread via eventfd/pipe
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
