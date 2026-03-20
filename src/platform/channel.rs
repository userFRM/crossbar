// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! Bidirectional shared-memory channel.

use alloc::format;
use std::time::Duration;

use crate::error::IpcError;
use crate::protocol::PubSubConfig;
use crate::wait::WaitStrategy;

use super::loan::{ShmLoan, TopicHandle};
use super::shm::{ShmPublisher, ShmSubscriber};
use super::subscription::{SampleGuard, Subscription};

/// Bidirectional shared-memory channel.
///
/// Composed of two pub/sub regions -- one per direction. The server creates
/// `"{name}-srv"` and subscribes to `"{name}-cli"`; the client does the
/// reverse. Both endpoints have identical capabilities after construction.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::*;
/// use std::time::Duration;
///
/// // Process A (server -- start first)
/// let mut srv = ShmChannel::listen("rpc", PubSubConfig::default(),
///     Duration::from_secs(30)).unwrap();
///
/// // Process B (client)
/// let mut cli = ShmChannel::connect("rpc", PubSubConfig::default(),
///     Duration::from_secs(5)).unwrap();
///
/// cli.send(b"request").unwrap();
/// let msg = srv.recv().unwrap();
/// assert_eq!(&*msg, b"request");
/// drop(msg);
///
/// srv.send(b"response").unwrap();
/// let reply = cli.recv().unwrap();
/// assert_eq!(&*reply, b"response");
/// ```
pub struct ShmChannel {
    tx_pub: ShmPublisher,
    tx_topic: TopicHandle,
    pub(crate) rx: Subscription, // must drop before _rx_sub to avoid use-after-unmap
    _rx_sub: ShmSubscriber,      // keeps mmap alive
}

impl core::fmt::Debug for ShmChannel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ShmChannel").field("rx", &self.rx).finish()
    }
}

impl ShmChannel {
    /// Creates the server side of a bidirectional channel.
    ///
    /// Creates the `"{name}-srv"` region immediately, then waits up to
    /// `timeout` for a client to create `"{name}-cli"`.
    ///
    /// # Errors
    ///
    /// Returns an error if the server region cannot be created or the
    /// client does not appear before `timeout`.
    pub fn listen(name: &str, config: PubSubConfig, timeout: Duration) -> Result<Self, IpcError> {
        let mut tx_pub = ShmPublisher::create(&format!("{name}-srv"), config)?;
        let tx_topic = tx_pub.register("/ch")?;

        let (rx_sub, rx) = wait_for_peer(&format!("{name}-cli"), timeout)?;

        Ok(Self {
            tx_pub,
            tx_topic,
            _rx_sub: rx_sub,
            rx,
        })
    }

    /// Creates the client side of a bidirectional channel.
    ///
    /// Creates the `"{name}-cli"` region immediately, then connects to
    /// the server's `"{name}-srv"` region. Retries up to `timeout` if
    /// the server has not started yet.
    ///
    /// # Errors
    ///
    /// Returns an error if the client region cannot be created or the
    /// server region does not appear before `timeout`.
    pub fn connect(name: &str, config: PubSubConfig, timeout: Duration) -> Result<Self, IpcError> {
        let mut tx_pub = ShmPublisher::create(&format!("{name}-cli"), config)?;
        let tx_topic = tx_pub.register("/ch")?;

        let (rx_sub, rx) = wait_for_peer(&format!("{name}-srv"), timeout)?;

        Ok(Self {
            tx_pub,
            tx_topic,
            _rx_sub: rx_sub,
            rx,
        })
    }

    /// Copies `data` into a pool block and sends it to the other endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::PoolExhausted`] if all blocks are in use, or
    /// [`IpcError::DataTooLarge`] if `data` exceeds block capacity.
    pub fn send(&mut self, data: &[u8]) -> Result<(), IpcError> {
        let mut loan = self.tx_pub.loan(&self.tx_topic)?;
        loan.set_data(data)?;
        loan.publish();
        Ok(())
    }

    /// Returns a mutable loan for born-in-SHM writes.
    ///
    /// Write directly into the loan, then call [`ShmLoan::publish`].
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::PoolExhausted`] if all blocks are in use.
    pub fn loan(&mut self) -> Result<ShmLoan<'_>, IpcError> {
        self.tx_pub.loan(&self.tx_topic)
    }

    /// Non-blocking receive. Returns `None` if no new message.
    #[inline]
    pub fn try_recv(&self) -> Option<SampleGuard<'_>> {
        self.rx.try_recv()
    }

    /// Blocking receive with the default wait strategy.
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::PublisherDead`] if the other endpoint's
    /// heartbeat goes stale.
    pub fn recv(&self) -> Result<SampleGuard<'_>, IpcError> {
        self.rx.recv()
    }

    /// Blocking receive with a custom [`WaitStrategy`].
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::PublisherDead`] if the other endpoint's
    /// heartbeat goes stale.
    pub fn recv_with(&self, strategy: WaitStrategy) -> Result<SampleGuard<'_>, IpcError> {
        self.rx.recv_with(strategy)
    }

    /// Updates the publisher heartbeat. Call during idle periods when
    /// not sending to prevent the other side from reporting publisher dead.
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::ClockError`] if the system clock is before UNIX epoch.
    pub fn heartbeat(&mut self) -> Result<(), IpcError> {
        self.tx_pub.heartbeat()
    }
}

/// Poll for a peer's region to appear, subscribing to its `/ch` topic.
///
/// The subscription starts from seq 0 (not the latest) so that messages
/// published between topic registration and subscription are not missed.
fn wait_for_peer(
    region_name: &str,
    timeout: Duration,
) -> Result<(ShmSubscriber, Subscription), IpcError> {
    let deadline = if timeout.is_zero() {
        None
    } else {
        Some(std::time::Instant::now() + timeout)
    };

    loop {
        match ShmSubscriber::connect(region_name) {
            Ok(sub) => match sub.subscribe("/ch") {
                Ok(rx) => {
                    // Start from beginning -- channel must not miss early messages.
                    rx.last_seq.set(0);
                    return Ok((sub, rx));
                }
                // Topic not registered yet -- retry
                Err(_) if deadline.is_some_and(|d| std::time::Instant::now() < d) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(e),
            },
            Err(_) if deadline.is_some_and(|d| std::time::Instant::now() < d) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e),
        }
    }
}
