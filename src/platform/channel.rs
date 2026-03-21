// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! Bidirectional shared-memory channel.

use alloc::format;
use std::time::Duration;

use crate::error::Error;
use crate::protocol::Config;
use crate::wait::WaitStrategy;

use super::loan::{Loan, Topic};
use super::shm::{Publisher, Subscriber};
use super::subscription::{Sample, Stream};

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
/// let mut srv = Channel::listen("rpc", Config::default(),
///     Duration::from_secs(30)).unwrap();
///
/// // Process B (client)
/// let mut cli = Channel::connect("rpc", Config::default(),
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
pub struct Channel {
    tx_pub: Publisher,
    tx_topic: Topic,
    pub(crate) rx: Stream, // must drop before _rx_sub to avoid use-after-unmap
    _rx_sub: Subscriber,   // keeps mmap alive
}

impl core::fmt::Debug for Channel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Channel").field("rx", &self.rx).finish()
    }
}

impl Channel {
    /// Creates the server side of a bidirectional channel.
    ///
    /// Creates the `"{name}-srv"` region immediately, then waits up to
    /// `timeout` for a client to create `"{name}-cli"`.
    ///
    /// # Errors
    ///
    /// Returns an error if the server region cannot be created or the
    /// client does not appear before `timeout`.
    pub fn listen(name: &str, config: Config, timeout: Duration) -> Result<Self, Error> {
        let mut tx_pub = Publisher::create(&format!("{name}-srv"), config)?;
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
    pub fn connect(name: &str, config: Config, timeout: Duration) -> Result<Self, Error> {
        let mut tx_pub = Publisher::create(&format!("{name}-cli"), config)?;
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
    /// Returns [`Error::PoolExhausted`] if all blocks are in use, or
    /// [`Error::DataTooLarge`] if `data` exceeds block capacity.
    pub fn send(&mut self, data: &[u8]) -> Result<(), Error> {
        let mut loan = self.tx_pub.loan(&self.tx_topic)?;
        loan.set_data(data)?;
        loan.publish();
        Ok(())
    }

    /// Returns a mutable loan for born-in-SHM writes.
    ///
    /// Write directly into the loan, then call [`Loan::publish`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::PoolExhausted`] if all blocks are in use.
    pub fn loan(&mut self) -> Result<Loan<'_>, Error> {
        self.tx_pub.loan(&self.tx_topic)
    }

    /// Non-blocking receive. Returns `None` if no new message.
    #[inline]
    pub fn try_recv(&self) -> Option<Sample<'_>> {
        self.rx.try_recv()
    }

    /// Blocking receive with the default wait strategy.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PublisherDead`] if the other endpoint's
    /// heartbeat goes stale.
    pub fn recv(&self) -> Result<Sample<'_>, Error> {
        self.rx.recv()
    }

    /// Blocking receive with a custom [`WaitStrategy`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::PublisherDead`] if the other endpoint's
    /// heartbeat goes stale.
    pub fn recv_with(&self, strategy: WaitStrategy) -> Result<Sample<'_>, Error> {
        self.rx.recv_with(strategy)
    }

    /// Updates the publisher heartbeat. Call during idle periods when
    /// not sending to prevent the other side from reporting publisher dead.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ClockError`] if the system clock is before UNIX epoch.
    pub fn heartbeat(&mut self) -> Result<(), Error> {
        self.tx_pub.heartbeat()
    }
}

/// Poll for a peer's region to appear, subscribing to its `/ch` topic.
///
/// The subscription starts from seq 0 (not the latest) so that messages
/// published between topic registration and subscription are not missed.
fn wait_for_peer(region_name: &str, timeout: Duration) -> Result<(Subscriber, Stream), Error> {
    let deadline = if timeout.is_zero() {
        None
    } else {
        Some(std::time::Instant::now() + timeout)
    };

    loop {
        match Subscriber::connect(region_name) {
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
